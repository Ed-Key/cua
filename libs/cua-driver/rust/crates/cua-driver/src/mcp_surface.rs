//! What one `cua-driver mcp` stdio connection shows its client.
//!
//! The SDK, the daemon registry, the CLI and every program consumer keep the
//! full tool set and full results. Only this client boundary narrows them:
//!
//! - `--tools core` advertises `mcp_wire::CORE_TOOLS` and refuses calls to any other
//!   tool with the option that restores it. `--tools full` (the default)
//!   advertises everything.
//! - The registry itself marks the core tools `anthropic/alwaysLoad` for
//!   Claude Code (`ToolDef::to_list_entry`), so both profiles carry it.
//! - For Codex (`clientInfo.name` "codex-mcp-client"), whose code mode prints
//!   the whole result object, a successful result's text blocks become one
//!   line pointing at `structuredContent`, so the model reads one copy.
//!   Claude Code already shows its model only `structuredContent`. Images and
//!   error results are left as they are. Codex also gets the compact
//!   `codex_instructions`, because it repeats the instructions in every
//!   ALL_TOOLS entry.

use cua_driver_core::mcp_result::{conforming_tool_result, tool_error_result};
use cua_driver_core::mcp_wire::{finish_response, is_core_tool, ProtocolEra};
use cua_driver_core::protocol::{codex_instructions, Request, Response, ResponseBody};
use serde_json::{json, Value};

/// The text a Codex client gets in place of a result's text blocks.
pub const CODEX_TEXT_POINTER: &str = "Full result in structuredContent.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolProfile {
    Core,
    #[default]
    Full,
}

impl ToolProfile {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "core" => Ok(Self::Core),
            "full" => Ok(Self::Full),
            other => Err(format!("--tools takes core or full, not {other:?}")),
        }
    }

    pub fn includes(self, tool: &str) -> bool {
        self == Self::Full || is_core_tool(tool)
    }
}

/// Per-connection state: the profile and whether the client is Codex.
#[derive(Debug, Default)]
pub struct Surface {
    profile: ToolProfile,
    codex: bool,
}

impl Surface {
    pub fn new(profile: ToolProfile) -> Self {
        Self {
            profile,
            codex: false,
        }
    }

    /// Note the client from its `initialize` request.
    pub fn observe(&mut self, request: &Request) {
        if let Some(metadata) = request.initialize_metadata() {
            self.codex = metadata
                .client_name
                .is_some_and(|name| name.to_ascii_lowercase().starts_with("codex"));
        }
    }

    /// The refusal for a call to a tool outside the profile. The call is never
    /// dispatched.
    pub fn refusal(&self, request: &Request, era: ProtocolEra) -> Option<Response> {
        if request.method != "tools/call" {
            return None;
        }
        let call = request.tool_call().ok()?;
        if self.profile.includes(&call.name) {
            return None;
        }
        let message = format!(
            "`{}` is not in this MCP server's core tool profile (`cua-driver mcp --tools core`). \
             Start the server with `--tools full`, the default, to use it.",
            call.name
        );
        let result = conforming_tool_result(
            &call.name,
            tool_error_result(
                message.clone(),
                json!({"code": "tool_not_in_profile", "profile": "core", "message": message}),
            ),
        );
        Some(finish_response(
            era,
            "tools/call",
            Response::ok(request.id.clone().unwrap_or(Value::Null), result),
        ))
    }

    /// Apply the profile to a `tools/list` result and, for Codex, project a
    /// `tools/call` result to one copy.
    pub fn render(&self, method: &str, mut response: Response) -> Response {
        if let ResponseBody::Result { result } = &mut response.body {
            match method {
                "tools/list" => self.render_tools(result),
                "initialize" if self.codex => {
                    if let Some(result) = result.as_object_mut() {
                        result.insert("instructions".into(), json!(codex_instructions()));
                    }
                }
                "tools/call" if self.codex => project_for_codex(result),
                _ => {}
            }
        }
        response
    }

    fn render_tools(&self, list: &mut Value) {
        let Some(tools) = list.get_mut("tools").and_then(Value::as_array_mut) else {
            return;
        };
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| self.profile.includes(name))
        });
    }
}

/// Replace a successful result's text blocks with one pointer line when its
/// `structuredContent` carries the result. Images keep their place.
fn project_for_codex(result: &mut Value) {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let structured = result
        .get("structuredContent")
        .and_then(Value::as_object)
        .is_some_and(|object| !object.is_empty());
    if !structured {
        return;
    }
    let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let mut pointed = false;
    content.retain_mut(|part| {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return true;
        }
        if pointed {
            return false;
        }
        pointed = true;
        *part = json!({"type": "text", "text": CODEX_TEXT_POINTER});
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> Request {
        serde_json::from_value(value).unwrap()
    }

    fn initialize(client: &str) -> Request {
        request(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": client, "title": "Codex", "version": "0.159.2"}}}))
    }

    fn call(tool: &str) -> Request {
        request(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": tool, "arguments": {}}}))
    }

    fn result(response: &Response) -> Value {
        serde_json::to_value(response).unwrap()["result"].clone()
    }

    fn listing() -> Value {
        json!({"tools": [
            {"name": "get_window_state", "inputSchema": {"type": "object"}},
            {"name": "click", "_meta": {"cua/hint": "kept"}},
            {"name": "zoom"},
            {"name": "start_recording"},
            {"name": "set_agent_cursor_theme"},
            {"name": "pip_turn"},
        ], "schema_version": "x"})
    }

    fn names(list: &Value) -> Vec<&str> {
        list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect()
    }

    fn sample_result() -> Value {
        json!({
            "content": [
                {"type": "text", "text": "window_id=1 pid=2 elements=3\n- [0] AXWindow"},
                {"type": "image", "data": "iVBO", "mimeType": "image/png"},
                {"type": "text", "text": "second block"},
            ],
            "structuredContent": {"pid": 2, "tree_markdown": "- [0] AXWindow"},
        })
    }

    #[test]
    fn profile_parsing_names_both_values() {
        assert_eq!(ToolProfile::parse("core"), Ok(ToolProfile::Core));
        assert_eq!(ToolProfile::parse("full"), Ok(ToolProfile::Full));
        assert!(ToolProfile::parse("lean")
            .unwrap_err()
            .contains("core or full"));
        assert_eq!(ToolProfile::default(), ToolProfile::Full);
    }

    #[test]
    fn core_profile_lists_only_core_tools_unchanged() {
        let surface = Surface::new(ToolProfile::Core);
        let list = result(&surface.render("tools/list", Response::ok(json!(1), listing())));
        assert_eq!(
            names(&list),
            ["get_window_state", "click", "zoom", "pip_turn"]
        );
        assert_eq!(list["schema_version"], "x");
        assert_eq!(list["tools"][0], listing()["tools"][0]);
        assert_eq!(list["tools"][1], listing()["tools"][1]);
    }

    #[test]
    fn full_profile_lists_every_tool_unchanged() {
        let surface = Surface::new(ToolProfile::Full);
        let list = result(&surface.render("tools/list", Response::ok(json!(1), listing())));
        assert_eq!(list, listing());
    }

    #[test]
    fn core_profile_refuses_other_tools_with_the_way_back() {
        let surface = Surface::new(ToolProfile::Core);
        let refusal = surface
            .refusal(&call("start_recording"), ProtocolEra::Legacy)
            .expect("refused");
        let refused = result(&refusal);
        assert_eq!(refused["isError"], true);
        let text = refused["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("`start_recording`"), "{text}");
        assert!(text.contains("--tools full"), "{text}");
        assert_eq!(refused["structuredContent"]["code"], "tool_not_in_profile");
        assert!(refused["structuredContent"]["message"]
            .as_str()
            .unwrap()
            .contains("--tools full"));
        for tool in cua_driver_core::mcp_wire::CORE_TOOLS
            .iter()
            .copied()
            .chain(["type_text_chars"])
        {
            assert!(
                surface.refusal(&call(tool), ProtocolEra::Legacy).is_none(),
                "{tool}"
            );
        }
        assert!(surface
            .refusal(&initialize("codex-mcp-client"), ProtocolEra::Legacy)
            .is_none());
    }

    #[test]
    fn full_profile_refuses_nothing() {
        let surface = Surface::new(ToolProfile::Full);
        for tool in [
            "start_recording",
            "set_agent_cursor_theme",
            "replay_trajectory",
        ] {
            assert!(surface.refusal(&call(tool), ProtocolEra::Legacy).is_none());
        }
    }

    #[test]
    fn codex_gets_one_copy_with_images_and_structured_content_intact() {
        let mut surface = Surface::new(ToolProfile::Full);
        surface.observe(&initialize("codex-mcp-client"));
        let projected =
            result(&surface.render("tools/call", Response::ok(json!(1), sample_result())));
        assert_eq!(
            projected["content"],
            json!([
                {"type": "text", "text": CODEX_TEXT_POINTER},
                {"type": "image", "data": "iVBO", "mimeType": "image/png"},
            ])
        );
        assert_eq!(
            projected["structuredContent"],
            sample_result()["structuredContent"]
        );
    }

    #[test]
    fn codex_gets_the_compact_instructions_and_others_the_full_ones() {
        let full = cua_driver_core::protocol::initialize_result();
        for (client, expected) in [
            ("codex-mcp-client", json!(codex_instructions())),
            ("claude-code", full["instructions"].clone()),
        ] {
            let mut surface = Surface::new(ToolProfile::Full);
            surface.observe(&initialize(client));
            let rendered =
                result(&surface.render("initialize", Response::ok(json!(0), full.clone())));
            assert_eq!(rendered["instructions"], expected, "{client}");
            assert_eq!(rendered["serverInfo"], full["serverInfo"], "{client}");
        }
    }

    #[test]
    fn other_clients_and_codex_errors_keep_the_result_unchanged() {
        for client in ["claude-code", "cua-lab", "Cursor"] {
            let mut surface = Surface::new(ToolProfile::Core);
            surface.observe(&initialize(client));
            let unchanged =
                result(&surface.render("tools/call", Response::ok(json!(1), sample_result())));
            assert_eq!(unchanged, sample_result(), "{client}");
        }
        let mut codex = Surface::new(ToolProfile::Core);
        codex.observe(&initialize("codex-mcp-client"));
        let error = json!({"content": [{"type": "text", "text": "refused: stale token"}],
                           "isError": true, "structuredContent": {"code": "stale"}});
        assert_eq!(
            result(&codex.render("tools/call", Response::ok(json!(1), error.clone()))),
            error
        );
        let text_only = json!({"content": [{"type": "text", "text": "only text"}]});
        assert_eq!(
            result(&codex.render("tools/call", Response::ok(json!(1), text_only.clone()))),
            text_only
        );
        let empty = json!({"content": [{"type": "text", "text": "t"}], "structuredContent": {}});
        assert_eq!(
            result(&codex.render("tools/call", Response::ok(json!(1), empty.clone()))),
            empty
        );
    }
}
