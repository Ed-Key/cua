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
                    update_group. Tab, window, and group ids come from list. Requires the Cua \
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
                let filter = json!({ "windowId": int("window_id") });
                let filter = strip_nulls(filter);
                let (windows, tabs, groups) = tokio::try_join!(
                    self.request("windows.list", json!({})),
                    self.request("tabs.list", filter.clone()),
                    self.request("tabGroups.list", filter),
                )?;
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
        self.request(method, Value::Object(params)).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.bridge
            .request(method, params)
            .await
            .map_err(|error| error.to_string())
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
