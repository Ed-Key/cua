//! The transport abstraction every test targets.

use crate::response::ToolResponse;
use serde_json::Value;

/// A way to invoke cua-driver tools. Implemented by [`crate::McpDriver`]
/// (long-lived proxy) and [`crate::CliDriver`] (one shell process per call).
///
/// Write scenarios against `Driver` to run them over either transport — the one
/// behavior that only surfaces across both is config persistence (`set_config`
/// is session-scoped over MCP but persists to disk over the CLI).
pub trait Driver {
    /// Invoke `tool` with `args`, returning the normalized response.
    fn call(&mut self, tool: &str, args: Value) -> ToolResponse;
}

/// Explicit lifecycle capability for canonical per-cell behavioral clips.
pub trait BehaviorRecording {
    fn start_behavior_recording(&mut self);
}

/// Harness scenarios read both `elements` and `tree_markdown` from
/// `get_window_state`. The tool's model-facing default is now one compact
/// markdown tree, so request the full response unless the scenario chose a
/// shape itself (`tree_format`, `full_output`, `since`, `element_fields`, or
/// `diff`).
pub fn with_full_window_state(tool: &str, mut args: Value) -> Value {
    if tool == "get_window_state" {
        if let Some(map) = args.as_object_mut() {
            let chose_shape = [
                "tree_format",
                "full_output",
                "since",
                "element_fields",
                "diff",
            ]
            .iter()
            .any(|key| map.contains_key(*key));
            if !chose_shape {
                map.insert("full_output".into(), Value::Bool(true));
            }
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::with_full_window_state;
    use serde_json::json;

    #[test]
    fn full_output_is_injected_only_when_no_shape_was_chosen() {
        let plain = with_full_window_state("get_window_state", json!({"pid": 1, "window_id": 2}));
        assert_eq!(plain["full_output"], json!(true));
        for (key, value) in [
            ("tree_format", json!("markdown")),
            ("full_output", json!(false)),
            ("since", json!("s00000001")),
            ("element_fields", json!("compact")),
            ("diff", json!(true)),
        ] {
            let mut args = json!({"pid": 1, "window_id": 2});
            args[key] = value.clone();
            let out = with_full_window_state("get_window_state", args);
            assert_eq!(out[key], value, "{key} kept");
            if key != "full_output" {
                assert!(out.get("full_output").is_none(), "{key} chose the shape");
            }
        }
        let other = with_full_window_state("click", json!({"pid": 1}));
        assert!(other.get("full_output").is_none());
    }
}
