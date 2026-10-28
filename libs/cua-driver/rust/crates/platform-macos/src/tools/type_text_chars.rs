use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use super::ToolState;

pub struct TypeTextCharsTool {
    state: Arc<ToolState>,
}

impl TypeTextCharsTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "type_text_chars".into(),
        description: "Type text character-by-character with a configurable inter-character delay. \
            Useful for apps that miss keystrokes when characters arrive too quickly. \
            Otherwise identical to type_text (CGEvent, no focus steal).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "text"],
            "properties": {
                "pid":           { "type": "integer", "description": "Target process ID." },
                "text":          { "type": "string",  "description": "Text to type." },
                "delay_ms":      { "type": "integer", "description": "Milliseconds between characters (default 30)." },
                "window_id":     { "type": "integer", "description": "Window ID for element focus. Optional when element_token is supplied." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "type_chars_only": { "type": "boolean", "description": "Skip AX focus, type directly. Default false." }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for TypeTextCharsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Same trailing-protocol-tag scrub as TypeTextTool — see
        // cua_driver_core::text_sanitize for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        let delay_ms = args.u64_or("delay_ms", 30);
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (_, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let type_chars_only = args.bool_or("type_chars_only", false);

        let element_ptr: Option<usize> = element_guard.as_ref().map(|g| g.as_ptr());

        // ── Exact-target background gate (macOS background input v1) ──
        // This tool is always background CGEvent keystrokes — process-scoped
        // transport with no semantic AX rung. A window-addressed request must
        // prove exact delivery before any event is posted; there is no
        // semantic fallback here, so a refusal is final.
        let _mutation_lease = if let Some(wid) = window_id {
            match super::gate_background_window_action(
                pid,
                wid,
                element_ptr,
                cua_driver_core::background_input::BackgroundAction::InsertText,
            )
            .await
            {
                Ok(lease) => Some(lease),
                Err(refusal_result) => return refusal_result,
            }
        } else {
            Some(super::acquire_background_mutation(pid).await)
        };

        let background_focus = match self.state.background_focus(&args, pid) {
            Ok(controller) => controller,
            Err(error) => return error,
        };

        let text_len = text.chars().count();
        let delivery_lease = _mutation_lease.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _delivery_lease = delivery_lease;
            let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
            let prepare_element = || {
                if let Some(ptr) = element_ptr.filter(|_| !type_chars_only) {
                    crate::input::ax_actions::focus_element(ptr)?;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(())
            };
            super::with_background_key_recipient(
                Some(&background_focus),
                pid,
                window_id,
                element_ptr,
                element_ptr
                    .filter(|_| !type_chars_only)
                    .map(|_| &prepare_element as &dyn Fn() -> anyhow::Result<()>),
                |recipient| {
                    crate::input::keyboard::type_text_with_delay(recipient, &text, delay_ms, None, crate::input::keyboard::LineBreaks::Characters)
                        .map(drop)
                },
            )
        })
        .await;

        match result {
            Ok(Ok(())) => ToolResult::text(format!(
                "Typed {text_len} character(s) with {delay_ms}ms delay."
            )),
            Ok(Err(e)) => ToolResult::error(format!("Type text chars failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}
