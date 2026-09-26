//! `browser_tabs`: list and organize the tabs of the user's own Chrome through
//! the Cua Driver extension (see [`super::extension_bridge`]). Tab and group
//! management is browser-level state that CDP does not cover; the extension
//! reaches it through `chrome.tabs` and `chrome.tabGroups`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use super::extension_bridge::ExtensionBridge;
use crate::protocol::ToolResult;
use crate::tool::{Tool, ToolDef};

const GROUP_COLORS: [&str; 9] = [
    "grey", "blue", "red", "yellow", "green", "pink", "purple", "cyan", "orange",
];

pub struct BrowserTabsTool {
    def: ToolDef,
    bridge: Arc<ExtensionBridge>,
}

impl BrowserTabsTool {
    pub fn new(bridge: Arc<ExtensionBridge>) -> Self {
        Self {
            def: ToolDef {
                name: "browser_tabs".into(),
                description: "List and organize the tabs of the user's own Chrome (their \
                    logged-in profile) through the Cua Driver Chrome extension. Actions: list \
                    (windows, tabs with title and URL, tab groups), open (a URL in a new tab, in \
                    the background unless active:true, grouped under \"Cua\"), activate (select a tab in its window \
                    without raising the window), close, move, group (put tabs in a new or \
                    existing group, optionally naming and coloring it), ungroup, and \
                    update_group. Tab, window, and group ids come from list, \
                    with the pid of the Chrome they belong to. Requires the Cua \
                    Driver extension in Chrome; the error says so when it is not connected."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "open", "activate", "close", "move", "group", "ungroup", "update_group"]
                        },
                        "tab_id": { "type": "integer", "description": "One tab, for activate." },
                        "tab_ids": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Tabs for close, move, group, and ungroup."
                        },
                        "url": { "type": "string", "description": "URL for open." },
                        "window_id": { "type": "integer", "description": "Window for list (filter), open, move, or a new group." },
                        "index": { "type": "integer", "description": "Position for open or move; -1 or omitted means the end." },
                        "active": { "type": "boolean", "description": "open: select the new tab. Default false." },
                        "group": { "type": "boolean", "description": "open: add the tab to the window's \"Cua\" tab group, so the agent's tabs stay together. Default true." },
                        "group_id": { "type": "integer", "description": "Existing group for group or update_group." },
                        "title": { "type": "string", "description": "Group name for group or update_group." },
                        "color": { "type": "string", "enum": GROUP_COLORS },
                        "collapsed": { "type": "boolean" },
                        "pid": { "type": "integer", "description": "The Chrome process, from list. Required for every change (not for list)." },
                        "session": { "type": "string" }
                    },
                    "required": ["action"],
                    "additionalProperties": false
                }),
                read_only: false,
                destructive: true,
                idempotent: false,
                open_world: true,
            },
            bridge,
        }
    }

    /// The Chrome a change acts on, named by `pid` from list. Required even
    /// with one Chrome connected: a Chrome restarted between list and change
    /// would otherwise receive the change. Several profiles of one Chrome share
    /// its pid; the profile that owns every tab, window, and group the request
    /// names is chosen, and a request that names none of them is refused.
    async fn resolve_change(&self, args: &Value) -> Result<(u64, i64), String> {
        let pid = args
            .get("pid")
            .and_then(Value::as_i64)
            .ok_or("changes need pid: the Chrome process, from list")?;
        let candidates: Vec<u64> = self
            .bridge
            .links()
            .into_iter()
            .rev()
            .filter(|link| link.chrome_pid == Some(pid))
            .map(|link| link.link)
            .collect();
        let ids = |key: &str| -> Vec<i64> {
            args.get(key)
                .into_iter()
                .flat_map(|value| match value {
                    Value::Array(values) => values.clone(),
                    other => vec![other.clone()],
                })
                .filter_map(|value| value.as_i64())
                .collect()
        };
        let (mut tabs, windows, groups) = (ids("tab_id"), ids("window_id"), ids("group_id"));
        tabs.extend(ids("tab_ids"));
        match candidates.len() {
            0 => Err(format!("no connected Chrome has pid {pid}; call list for the current ones")),
            1 => Ok((candidates[0], pid)),
            _ if tabs.is_empty() && windows.is_empty() && groups.is_empty() => Err(format!(
                "several profiles of Chrome {pid} are connected; name a window_id, tab, or group from list"
            )),
            _ => {
                for link in &candidates {
                    let owned = |list: Value, key: &str| -> Vec<i64> {
                        list.as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|item| item.get(key).and_then(Value::as_i64))
                            .collect()
                    };
                    let owned_tabs = owned(self.request(*link, "tabs.list", json!({})).await?, "tabId");
                    let owned_windows =
                        owned(self.request(*link, "windows.list", json!({})).await?, "windowId");
                    let owned_groups =
                        owned(self.request(*link, "tabGroups.list", json!({})).await?, "groupId");
                    if tabs.iter().all(|id| owned_tabs.contains(id))
                        && windows.iter().all(|id| owned_windows.contains(id))
                        && groups.iter().all(|id| owned_groups.contains(id))
                    {
                        return Ok((*link, pid));
                    }
                }
                Err(format!("no single profile of Chrome {pid} owns every tab, window, and group named"))
            }
        }
    }

    /// The Chrome named by `pid`, or the only connected one (reads only).
    fn resolve(&self, args: &Value) -> Result<(u64, i64), String> {
        let links: Vec<_> = self
            .bridge
            .links()
            .into_iter()
            .filter_map(|link| link.chrome_pid.map(|pid| (link.link, pid)))
            .collect();
        if links.is_empty() {
            return Err(
                "the Cua Driver Chrome extension is not connected: install it in Chrome and keep Chrome open"
                    .to_owned(),
            );
        }
        match args.get("pid").and_then(Value::as_i64) {
            Some(pid) => links
                .iter()
                .rev()
                .find(|(_, candidate)| *candidate == pid)
                .copied()
                .ok_or_else(|| format!("no connected Chrome has pid {pid}; call list for the current ones")),
            None if links.len() == 1 => Ok(links[0]),
            None => Err(format!(
                "several Chrome instances are connected (pids {}); pass pid from list",
                links.iter().map(|(_, pid)| pid.to_string()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    async fn run(&self, args: &Value) -> Result<Value, String> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or_default();
        let int = |key: &str| args.get(key).and_then(Value::as_i64);
        let ids = || -> Result<Value, String> {
            match args.get("tab_ids").and_then(Value::as_array) {
                Some(ids) if !ids.is_empty() && ids.iter().all(Value::is_i64) => {
                    Ok(Value::Array(ids.clone()))
                }
                _ => Err(format!("{action} needs tab_ids: a non-empty list of tab ids from list")),
            }
        };
        let mut params = Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                params.insert(key.to_owned(), value);
            }
        };
        let (method, params) = match action {
            "list" => {
                let filter = strip_nulls(json!({ "windowId": int("window_id") }));
                let links: Vec<_> = match args.get("pid") {
                    Some(_) => vec![self.resolve(args)?],
                    None => self
                        .bridge
                        .links()
                        .into_iter()
                        .filter_map(|link| link.chrome_pid.map(|pid| (link.link, pid)))
                        .collect(),
                };
                if links.is_empty() {
                    self.resolve(args)?;
                }
                let (mut windows, mut tabs, mut groups) = (Vec::new(), Vec::new(), Vec::new());
                for (link, pid) in links {
                    let (w, t, g) = tokio::try_join!(
                        self.request(link, "windows.list", json!({})),
                        self.request(link, "tabs.list", filter.clone()),
                        self.request(link, "tabGroups.list", filter.clone()),
                    )?;
                    for (list, out) in [(w, &mut windows), (t, &mut tabs), (g, &mut groups)] {
                        for mut item in list.as_array().cloned().unwrap_or_default() {
                            item["pid"] = json!(pid);
                            out.push(item);
                        }
                    }
                }
                return Ok(json!({ "windows": windows, "tabs": tabs, "groups": groups }));
            }
            "open" => {
                let url = args
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
                    .ok_or("open needs url")?;
                put("url", Some(json!(url)));
                put("windowId", int("window_id").map(Value::from));
                put("index", int("index").map(Value::from));
                put("active", Some(json!(args.get("active").and_then(Value::as_bool).unwrap_or(false))));
                put("group", Some(json!(args.get("group").and_then(Value::as_bool).unwrap_or(true))));
                ("tabs.create", params)
            }
            "activate" => {
                let tab = int("tab_id").ok_or("activate needs tab_id")?;
                put("tabId", Some(json!(tab)));
                put("active", Some(json!(true)));
                ("tabs.update", params)
            }
            "close" => {
                put("tabIds", Some(ids()?));
                ("tabs.remove", params)
            }
            "move" => {
                put("tabIds", Some(ids()?));
                put("windowId", int("window_id").map(Value::from));
                put("index", Some(json!(int("index").unwrap_or(-1))));
                ("tabs.move", params)
            }
            "group" => {
                put("tabIds", Some(ids()?));
                put("groupId", int("group_id").map(Value::from));
                put("windowId", int("window_id").map(Value::from));
                put_group_fields(args, &mut put)?;
                ("tabs.group", params)
            }
            "ungroup" => {
                put("tabIds", Some(ids()?));
                ("tabs.ungroup", params)
            }
            "update_group" => {
                let group = int("group_id").ok_or("update_group needs group_id")?;
                put("groupId", Some(json!(group)));
                put_group_fields(args, &mut put)?;
                ("tabGroups.update", params)
            }
            other => return Err(format!("unknown action {other:?}")),
        };
        let (link, _) = self.resolve_change(args).await?;
        self.request(link, method, Value::Object(params)).await
    }

    async fn request(&self, link: u64, method: &str, params: Value) -> Result<Value, String> {
        self.bridge
            .request_on(link, method, params)
            .await
            .map_err(|error| error.to_string())
    }
}

/// What an approval prompt says for a change to the user's tabs.
pub(crate) fn consent_summary(args: &Value) -> String {
    let count = |key: &str| args.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    let tabs = |n: usize| if n == 1 { "1 tab".to_owned() } else { format!("{n} tabs") };
    match args.get("action").and_then(Value::as_str).unwrap_or_default() {
        "open" => {
            let origin = args
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| url::Url::parse(url).ok())
                .map(|url| url.origin().ascii_serialization())
                .unwrap_or_else(|| "a page".to_owned());
            format!("Allow Cua to open {origin} in a new tab in your Chrome")
        }
        "activate" => "Allow Cua to switch to one of your Chrome tabs".to_owned(),
        "close" => format!("Allow Cua to close {} in your Chrome", tabs(count("tab_ids"))),
        "move" => format!("Allow Cua to move {} in your Chrome", tabs(count("tab_ids"))),
        "group" => match args.get("title").and_then(Value::as_str) {
            Some(title) => format!("Allow Cua to group {} as \"{title}\" in your Chrome", tabs(count("tab_ids"))),
            None => format!("Allow Cua to group {} in your Chrome", tabs(count("tab_ids"))),
        },
        "ungroup" => format!("Allow Cua to ungroup {} in your Chrome", tabs(count("tab_ids"))),
        "update_group" => "Allow Cua to rename or recolor a tab group in your Chrome".to_owned(),
        other => format!("Allow Cua to change your Chrome tabs ({other})"),
    }
}

fn put_group_fields(
    args: &Value,
    put: &mut impl FnMut(&str, Option<Value>),
) -> Result<(), String> {
    if let Some(color) = args.get("color").and_then(Value::as_str) {
        if !GROUP_COLORS.contains(&color) {
            return Err(format!("color must be one of {}", GROUP_COLORS.join(", ")));
        }
        put("color", Some(json!(color)));
    }
    put("title", args.get("title").cloned().filter(Value::is_string));
    put("collapsed", args.get("collapsed").cloned().filter(Value::is_boolean));
    Ok(())
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

/// A short human-readable summary; the structured result carries everything.
fn summary(action: &str, result: &Value) -> String {
    match action {
        "list" => {
            let tabs = result["tabs"].as_array().map_or(0, Vec::len);
            let groups = result["groups"].as_array().map_or(0, Vec::len);
            let windows = result["windows"].as_array().map_or(0, Vec::len);
            let mut lines = vec![format!("{tabs} tabs in {windows} windows, {groups} groups")];
            for tab in result["tabs"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "- tab {} (window {}{}{}): {} <{}>",
                    tab["tabId"],
                    tab["windowId"],
                    if tab["active"] == true { ", active" } else { "" },
                    match tab["groupId"].as_i64() {
                        Some(group) if group >= 0 => format!(", group {group}"),
                        _ => String::new(),
                    },
                    tab["title"].as_str().unwrap_or_default(),
                    tab["url"].as_str().unwrap_or_default(),
                ));
            }
            lines.join("\n")
        }
        _ => format!("{action} done"),
    }
}

#[async_trait]
impl Tool for BrowserTabsTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    /// A change to the user's tabs names exactly what it touches: the Chrome
    /// (its OS-proven process), the action, the tabs, and for open the
    /// destination origin. The default revalidation re-derives this right
    /// before dispatch and refuses if anything moved.
    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id != "browser_consequential_action" {
            return Ok(None);
        }
        let (_, pid) = self.resolve_change(args).await?;
        let origin = match args.get("url").and_then(Value::as_str) {
            Some(url) => Some(
                url::Url::parse(url)
                    .map_err(|_| "the URL to open is invalid".to_owned())?
                    .origin()
                    .ascii_serialization(),
            ),
            None => None,
        };
        Ok(Some(json!({
            "kind": "chrome_tabs",
            "pid": pid,
            "action": args.get("action"),
            "tab_id": args.get("tab_id"),
            "tab_ids": args.get("tab_ids"),
            "window_id": args.get("window_id"),
            "group_id": args.get("group_id"),
            "title": args.get("title"),
            "color": args.get("color"),
            "origin": origin,
        })))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match self.run(&args).await {
            Ok(result) => ToolResult::text(summary(&action, &result))
                .with_structured(json!({ "action": action, "result": result })),
            Err(error) => ToolResult::error(format!("browser_tabs {action}: {error}")),
        }
    }
}
