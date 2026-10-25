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
//! - For Codex (`clientInfo.name` "codex-mcp-client"), whose code mode shows
//!   MCP instructions only inside its ALL_TOOLS catalog, prefixed to every
//!   tool entry, the instructions are a short text on showing results and
//!   starting a task (`codex_instructions`), and the listing has no
//!   `outputSchema`: code mode renders each into a TypeScript return type,
//!   about two-thirds of the catalog. Results reach Codex as they reach every
//!   client: code mode hands the script the whole result and the script
//!   prints what the model sees, so every block must keep its content (item
//!   33: a pointer to `structuredContent` in place of the text made the model
//!   read every window twice). The one addition: an error envelope that
//!   carries only a code gets the message from its text, because the
//!   instructions print `structuredContent`. Results still carry
//!   `structuredContent`, which MCP allows without an advertised schema;
//!   servers still validate against it.

use cua_driver_core::mcp_result::{conforming_tool_result, tool_error_result};
use cua_driver_core::mcp_wire::{finish_response, is_core_tool, ProtocolEra};
use cua_driver_core::protocol::{codex_instructions, Request, Response, ResponseBody};
use serde_json::{json, Value};

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
    /// The client was identified: later requests do not change it (the proxy
    /// announces the first identity only).
    identified: bool,
}

impl Surface {
    pub fn new(profile: ToolProfile) -> Self {
        Self {
            profile,
            codex: false,
            identified: false,
        }
    }

    /// Note the client from its `initialize` request, or from a modern
    /// request's per-request client metadata.
    pub fn observe(&mut self, request: &Request) {
        if self.identified {
            return;
        }
        if let Some(metadata) = request.client_metadata() {
            self.codex = metadata
                .client_name
                .is_some_and(|name| name.to_ascii_lowercase().starts_with("codex"));
            self.identified = true;
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

    /// Apply the profile to a `tools/list` result and give Codex its
    /// instructions. `tools/call` results pass unchanged, except that a
    /// Codex error's envelope gets its message.
    pub fn render(&self, method: &str, mut response: Response) -> Response {
        if let ResponseBody::Result { result } = &mut response.body {
            match method {
                "tools/list" => self.render_tools(result),
                "initialize" if self.codex => {
                    if let Some(result) = result.as_object_mut() {
                        result.insert("instructions".into(), json!(codex_instructions()));
                    }
                }
                "tools/call" if self.codex => carry_error_message(result),
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
        if self.codex {
            for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
                tool.remove("outputSchema");
            }
        }
    }
}

/// Codex prints a result's `structuredContent` (`codex_instructions`), as
/// Claude Code shows its model. An error envelope (a refusal marker: code,
/// status or refusal) can lack the message its text holds
/// (`internal_error_result`, normalized tool errors, clipboard failures), so
/// copy the text in. Envelopes with a message, and results that are not
/// envelopes (act_and_read's output), are left alone.
fn carry_error_message(result: &mut Value) {
    let envelope = result.get("isError").and_then(Value::as_bool) == Some(true)
        && result.get("structuredContent").is_some_and(|structured| {
            cua_driver_contract::is_refusal_envelope(structured)
                && structured.get("message").is_none()
        });
    if !envelope {
        return;
    }
    let message = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if !message.is_empty() {
        result["structuredContent"]["message"] = json!(message);
    }
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
    fn codex_listing_drops_output_schemas_and_others_keep_them() {
        let listed = json!({"tools": [
            {"name": "click", "inputSchema": {"type": "object"}, "outputSchema": {"type": "object"}},
            {"name": "zoom", "outputSchema": {"type": "object"}},
        ]});
        for (client, kept) in [
            ("codex-mcp-client", false),
            ("claude-code", true),
            ("cua-lab", true),
        ] {
            let mut surface = Surface::new(ToolProfile::Core);
            surface.observe(&initialize(client));
            let list =
                result(&surface.render("tools/list", Response::ok(json!(1), listed.clone())));
            for tool in list["tools"].as_array().unwrap() {
                assert_eq!(tool.get("outputSchema").is_some(), kept, "{client} {tool}");
                assert_eq!(tool["name"].is_string(), true);
            }
            assert_eq!(
                list["tools"][0]["inputSchema"],
                json!({"type": "object"}),
                "{client}"
            );
        }
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

    /// A get_window_state result as the daemon returns it (image first, then
    /// the outcome text with the token prefix and the rows), item 33.
    fn window_read() -> Value {
        json!({
            "content": [
                {"type": "image", "data": "iVBORw0KGgo", "mimeType": "image/png"},
                {"type": "text", "text": "window_id=38 pid=771 size=1568x985 elements=3\n\nelement_token = s00000002:<index>\n- [0] AXWindow \"Inbox\"\n  - [24] AXTextField \"Search mail\" [actions=[press]]\n  - [31] AXButton \"Archive\" [actions=[press]]"},
            ],
            "structuredContent": {"pid": 771, "window_id": 38, "snapshot_id": "s00000002",
                                  "tree_markdown": "- [0] AXWindow \"Inbox\""},
        })
    }

    #[test]
    fn codex_gets_the_outcome_text_images_and_structured_content_in_the_first_result() {
        let mut surface = Surface::new(ToolProfile::Full);
        surface.observe(&initialize("codex-mcp-client"));
        for daemon in [window_read(), sample_result()] {
            let rendered =
                result(&surface.render("tools/call", Response::ok(json!(1), daemon.clone())));
            // Code mode forwards `content` block by block: every text block
            // (usable element tokens included) and every image arrive as they are.
            assert_eq!(rendered["content"], daemon["content"]);
            assert_eq!(rendered["structuredContent"], daemon["structuredContent"]);
        }
    }

    #[test]
    fn a_codex_error_envelope_gets_the_message_its_text_carries() {
        let code_only = json!({
            "content": [{"type": "text", "text": "query_context requires a nonblank query"},
                        {"type": "text", "text": "Verify state before retrying."}],
            "isError": true, "structuredContent": {"code": "tool_invocation_failed"}});
        let mut codex = Surface::new(ToolProfile::Full);
        codex.observe(&initialize("codex-mcp-client"));
        let rendered =
            result(&codex.render("tools/call", Response::ok(json!(1), code_only.clone())));
        assert_eq!(rendered["content"], code_only["content"]);
        assert_eq!(
            rendered["structuredContent"],
            json!({"code": "tool_invocation_failed",
                   "message": "query_context requires a nonblank query\nVerify state before retrying."})
        );
        // A clipboard failure marks itself with status, not code.
        let clipboard = json!({
            "content": [{"type": "text", "text": "Clipboard write is unavailable: no such file"}],
            "isError": true,
            "structuredContent": {"status": "unavailable", "error_code": "clipboard_unavailable"}});
        let rendered = result(&codex.render("tools/call", Response::ok(json!(1), clipboard)));
        assert_eq!(
            rendered["structuredContent"]["message"],
            "Clipboard write is unavailable: no such file"
        );
        // Not an envelope (act_and_read's output), or a success: unchanged.
        let output = json!({"content": [{"type": "text", "text": "STEP 1 RESULT"}], "isError": true,
                            "structuredContent": {"action": {"code": "stale"}, "observation": {}}});
        let success = window_read();
        for unchanged in [output, success] {
            assert_eq!(
                result(&codex.render("tools/call", Response::ok(json!(1), unchanged.clone()))),
                unchanged
            );
        }
        // Other clients get the envelope as it is.
        let mut claude = Surface::new(ToolProfile::Full);
        claude.observe(&initialize("claude-code"));
        assert_eq!(
            result(&claude.render("tools/call", Response::ok(json!(1), code_only.clone()))),
            code_only
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
                           "isError": true, "structuredContent": {"code": "stale", "message": "stale"}});
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
