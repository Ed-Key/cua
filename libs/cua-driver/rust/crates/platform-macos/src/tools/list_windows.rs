use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

pub struct ListWindowsTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_windows".into(),
        description: "List top-level windows (window_id, pid, app_name, title, bounds, z_index). \
            Without pid, only the current Space's windows are listed; on_screen_only:false adds \
            the rest. The frontmost has the highest integer z_index; null is unknown, so never \
            infer order from the array.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "Only this pid's windows, all of them."
                },
                "on_screen_only": {
                    "type": "boolean",
                    "description": "Drop windows not on the current Space. Default true without pid, false with pid."
                }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid_filter: Option<i32> = args.opt_i64("pid").map(|v| v as i32);
        // Without an app filter the full list is mostly off-screen windows (a lab
        // median of 45 records, about 10k characters, for about 5 on screen) and
        // every agent turn after it re-reads that text. Default to the current
        // Space and say how many were left out.
        let explicit = args.get("on_screen_only").and_then(Value::as_bool);
        let on_screen_only = explicit.unwrap_or(pid_filter.is_none());

        let enumeration = if on_screen_only {
            crate::windows::visible_windows_with_space_snapshot()
        } else {
            crate::windows::all_windows_with_space_snapshot()
        };
        let current_space_id = enumeration.current_space_id;
        let mut windows = enumeration.windows;

        if let Some(pid) = pid_filter {
            windows.retain(|w| w.pid == pid);
        }
        crate::windows::retain_ax_reachable(&mut windows, current_space_id);

        let windows_json: Vec<Value> = windows.iter().map(window_record_json).collect();
        let omitted = (explicit.is_none() && on_screen_only).then(|| {
            let mut all = crate::windows::all_windows_with_space_snapshot().windows;
            if let Some(pid) = pid_filter {
                all.retain(|w| w.pid == pid);
            }
            crate::windows::retain_ax_reachable(&mut all, current_space_id);
            all.len().saturating_sub(windows_json.len())
        });

        let text = match omitted {
            Some(n) if n > 0 => format!(
                "Found {} window(s) on the current Space; {n} more are off-screen or on another Space (pass on_screen_only:false to list them).",
                windows_json.len()
            ),
            _ => format!("Found {} window(s).", windows_json.len()),
        };
        let mut structured = serde_json::json!({
            "windows": windows_json,
            "current_space_id": current_space_id
        });
        if let Some(n) = omitted {
            structured["off_screen_omitted"] = serde_json::json!(n);
        }
        ToolResult::text(text).with_structured(structured)
    }
}

pub(super) fn window_record_json(w: &crate::windows::WindowInfo) -> Value {
    serde_json::json!({
        "window_id": w.window_id,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        "bounds": {
            "x": w.bounds.x,
            "y": w.bounds.y,
            "width": w.bounds.width,
            "height": w.bounds.height
        },
        "layer": w.layer,
        "z_index": w.z_index,
        "is_on_screen": w.is_on_screen,
        "current_space_id": w.current_space_id,
        "on_current_space": w.on_current_space,
        "space_ids": w.space_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_record_includes_observed_z_index() {
        let window = crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Example".into(),
            title: "Document".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        };

        assert_eq!(window_record_json(&window)["z_index"], serde_json::json!(7));
        assert_eq!(
            window_record_json(&window)["current_space_id"],
            serde_json::json!(1)
        );
        assert_eq!(
            window_record_json(&window)["on_current_space"],
            serde_json::json!(true)
        );
    }
}
