use async_trait::async_trait;
use cua_driver_contract::MoveCursorInput;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::{future::Future, sync::Arc};

use super::{px_frame, ToolState};

pub struct MoveCursorTool {
    state: Arc<ToolState>,
}

impl MoveCursorTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
    async fn invoke_overlay<F, R, P, V>(
        &self,
        args: Value,
        resolve_frame: F,
        publish: P,
    ) -> ToolResult
    where
        F: FnOnce(i32, u32) -> R,
        R: Future<Output = Result<px_frame::WindowPxFrame, ToolResult>>,
        P: FnOnce(String, Option<u32>, f64, f64) -> V,
        V: Future<Output = ()>,
    {
        if !self.state.cursor_overlay_available {
            return super::cursor_overlay_unavailable();
        }
        let target = match normalized_window_target(&args) {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
        // The same downscale ratio a pixel click uses for this session's last
        // delivered screenshot of the window (refuses when none was delivered
        // at a known scale, rather than guessing).
        let ratio = match target {
            Some((pid, wid)) => match super::screenshot_scale(&self.state, &args, pid, Some(wid)) {
                Ok(ratio) => Some(ratio),
                Err(refusal) => return refusal,
            },
            None => None,
        };
        let (x, y, window_id) = match resolve_overlay_target(&args, ratio, resolve_frame).await {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
        let cursor_id = super::cursor_tools::resolve_cursor_key(&args);
        self.state.cursor_registry.update_position(&cursor_id, x, y);
        publish(cursor_id.clone(), window_id, x, y).await;
        ToolResult::text(format!(
            "Agent cursor '{cursor_id}' moved to ({x:.1}, {y:.1})."
        ))
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "move_cursor".into(),
        description: "Move a cursor to x,y: window targets use get_window_state pixels and move the \
            agent cursor; desktop targets use get_desktop_state pixels and move the real \
            pointer. Untargeted moves use screen points.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["x", "y"],
            "properties": {
                "session": cua_driver_core::tool_schema::session_schema(),
                "x": { "type": "number", "description": "Destination X: with pid and window_id, that window's screenshot pixels; otherwise screen points (window scope) or get_desktop_state pixels (desktop scope)." },
                "y": { "type": "number", "description": "Destination Y, in the same space as x." },
                "pid": { "type": "integer", "description": "With window_id, x,y are that window's screenshot pixels." },
                "window_id": { "type": "integer", "description": "Window whose screenshot x,y refer to." },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "\"window\" moves only the agent cursor; \"desktop\" moves the real pointer." },
                "cursor_id": { "type": "string", "default": "default", "description": "Cursor instance to move." }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for MoveCursorTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop") {
            let input = match parse_typed_projection::<MoveCursorInput>("move_cursor", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (x, y) = (input.x, input.y);
            let (x, y) = super::desktop_screenshot_point(x, y).await;
            let result =
                tokio::task::spawn_blocking(move || crate::input::mouse::move_cursor_desktop(x, y))
                    .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Moved the real desktop pointer to ({x:.1}, {y:.1})."
                ))
                .with_structured(serde_json::json!({
                    "scope": "desktop",
                    "x": x,
                    "y": y,
                    "effect": "unverifiable"
                })),
                Ok(Err(error)) => {
                    ToolResult::error(format!("desktop pointer move failed: {error}"))
                }
                Err(error) => ToolResult::error(format!("desktop pointer task failed: {error}")),
            };
        }
        self.invoke_overlay(
            args,
            |pid, wid| async move {
                let owner = tokio::task::spawn_blocking(move || {
                    crate::windows::resolve_window_owner(pid, wid)
                })
                .await
                .map_err(|error| {
                    ToolResult::error(format!("window ownership lookup failed: {error}"))
                })?;
                require_window_owner(pid, wid, owner)?;
                px_frame::resolve_or_refuse(wid).await
            },
            |cursor_id, window_id, x, y| async move {
                if let Some(wid) = window_id {
                    crate::cursor::overlay::send_command(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::PinAbove(u64::from(wid)),
                    );
                }
                // Preserve the first-move seeding and animation used by legacy moves.
                crate::cursor::overlay::animate_cursor_to(cursor_id, x, y, window_id.map(u64::from)).await;
            },
        )
        .await
    }
}

async fn resolve_overlay_target<F, R>(
    args: &Value,
    ratio: Option<f64>,
    resolve_frame: F,
) -> Result<(f64, f64, Option<u32>), ToolResult>
where
    F: FnOnce(i32, u32) -> R,
    R: Future<Output = Result<px_frame::WindowPxFrame, ToolResult>>,
{
    use cua_driver_core::tool_args::ArgsExt;
    let (x, y) = (args.require_f64("x")?, args.require_f64("y")?);
    if !x.is_finite() || !y.is_finite() {
        return Err(ToolResult::error("cursor coordinates must be finite"));
    }
    let Some((pid, wid)) = normalized_window_target(args)? else {
        return Ok((x, y, None));
    };
    let ratio = ratio.unwrap_or(1.0);
    if !ratio.is_finite() || ratio <= 0.0 {
        return Err(ToolResult::error(
            "window screenshot resize ratio must be finite and positive",
        ));
    }
    let frame = resolve_frame(pid, wid).await?;
    if !frame.scale.is_finite() || frame.scale <= 0.0 {
        return Err(ToolResult::error(
            "window capture scale must be finite and positive",
        ));
    }
    let (sx, sy, _, _) = frame.to_screen(x * ratio, y * ratio);
    if !sx.is_finite() || !sy.is_finite() {
        return Err(ToolResult::error(
            "window screenshot coordinates do not resolve to a finite screen point",
        ));
    }
    Ok((sx, sy, Some(wid)))
}

// Typed targets have already been flattened by the registry. Retain the full
// numeric values until checked conversion into macOS PID and CGWindowID types.
fn normalized_window_target(args: &Value) -> Result<Option<(i32, u32)>, ToolResult> {
    if args.get("pid").is_none() && args.get("window_id").is_none() {
        return Ok(None);
    }
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    let wid = args
        .get("window_id")
        .and_then(Value::as_u64)
        .and_then(|wid| u32::try_from(wid).ok())
        .filter(|wid| *wid > 0);
    match (pid, wid) {
        (Some(pid), Some(wid)) => Ok(Some((pid, wid))),
        _ => Err(ToolResult::error(
            "window target requires a positive macOS PID and a positive 32-bit window_id",
        )
        .with_structured(serde_json::json!({"code":"invalid_action_target"}))),
    }
}

fn require_window_owner(
    pid: i32,
    wid: u32,
    owner: crate::windows::WindowOwner,
) -> Result<(), ToolResult> {
    match owner {
        crate::windows::WindowOwner::SamePid => Ok(()),
        crate::windows::WindowOwner::Unknown => Err(px_frame::refusal(&px_frame::PxFrameError::WindowNotFound { window_id: wid })),
        crate::windows::WindowOwner::ForeignPid { owner_pid, .. } => Err(ToolResult::error(
            format!("window_id {wid} belongs to pid {owner_pid}, not requested pid {pid}; refusing cursor move")
        ).with_structured(serde_json::json!({"code":"invalid_action_target","pid":pid,"window_id":wid,"owner_pid":owner_pid}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowBounds;
    use cua_driver_core::action_target::normalize_action_target;
    use serde_json::json;
    use std::cell::RefCell;

    fn frame(scale: f64, x: f64, y: f64) -> px_frame::WindowPxFrame {
        px_frame::WindowPxFrame {
            bounds: WindowBounds {
                x,
                y,
                width: 500.0,
                height: 500.0,
            },
            scale,
        }
    }

    fn tool() -> MoveCursorTool {
        let state = Arc::new(ToolState::new(true, true, None));
        state
            .cursor_registry
            .update_position("slice-a-move", 7.0, 9.0);
        MoveCursorTool::new(state)
    }

    fn position(tool: &MoveCursorTool) -> (f64, f64) {
        let position = tool
            .state
            .cursor_registry
            .get("slice-a-move")
            .unwrap()
            .position
            .unwrap();
        (position.x, position.y)
    }

    #[tokio::test]
    async fn exact_window_moves_convert_before_registry_and_visual_publication() {
        for (x, y, scale, ratio, origin, expected) in [
            (30.0, 40.0, 1.0, None, (100.0, 580.0), (130.0, 620.0)),
            (60.0, 80.0, 2.0, None, (100.0, 580.0), (130.0, 620.0)),
            (60.0, 80.0, 2.0, None, (500.0, 100.0), (530.0, 140.0)),
            (60.0, 80.0, 2.0, None, (-1440.0, -900.0), (-1410.0, -860.0)),
        ] {
            // Native window pixels (the trusted in-process flag): the shared
            // lookup (the one pixel clicks use) keeps them; without it a
            // window-pixel move needs this session's screenshot. The downscale ratio math is
            // covered by geometry::screenshot_to_screen.
            assert_eq!(ratio, None::<f64>);
            let tool = tool();
            let commands = RefCell::new(Vec::new());
            let result = tool.invoke_overlay(
                json!({"x": x, "y": y, "scope": "window", "pid": 800, "window_id": 11, "session": "slice-a-move", "_native_window_pixels": true}),
                |pid, wid| async move {
                    assert_eq!((pid, wid), (800, 11));
                    Ok(frame(scale, origin.0, origin.1))
                },
                |key, wid, x, y| {
                    commands.borrow_mut().push((key, wid, x, y));
                    std::future::ready(())
                },
            ).await;
            assert_ne!(result.is_error, Some(true));
            assert_eq!(position(&tool), expected);
            assert_eq!(
                *commands.borrow(),
                vec![("slice-a-move".to_owned(), Some(11), expected.0, expected.1)]
            );
        }
    }

    #[tokio::test]
    async fn typed_window_normalization_reaches_the_same_move_operation() {
        let tool = tool();
        let mut args = json!({"x":60,"y":80,"session":"slice-a-move", "_native_window_pixels": true, "target":{"kind":"window","pid":800,"window_id":11}});
        normalize_action_target("move_cursor", &mut args).unwrap();
        let result = tool
            .invoke_overlay(
                args,
                |pid, wid| async move {
                    assert_eq!((pid, wid), (800, 11));
                    Ok(frame(2.0, 100.0, 580.0))
                },
                |_, wid, x, y| {
                    assert_eq!((wid, x, y), (Some(11), 130.0, 620.0));
                    std::future::ready(())
                },
            )
            .await;
        assert_ne!(result.is_error, Some(true));
        assert_eq!(position(&tool), (130.0, 620.0));
    }

    #[tokio::test]
    async fn legacy_screen_points_do_not_resolve_or_pin_a_window() {
        for scope in [None, Some("window")] {
            let tool = tool();
            let mut args = json!({"x":60,"y":80,"session":"slice-a-move"});
            if let Some(scope) = scope {
                args["scope"] = json!(scope);
            }
            let result = tool
                .invoke_overlay(
                    args,
                    |_, _| async { panic!("legacy move must not capture") },
                    |key, wid, x, y| {
                        assert_eq!(key, "slice-a-move");
                        assert_eq!((wid, x, y), (None, 60.0, 80.0));
                        std::future::ready(())
                    },
                )
                .await;
            assert_ne!(result.is_error, Some(true));
            assert_eq!(position(&tool), (60.0, 80.0));
        }
    }

    #[tokio::test]
    async fn malformed_targets_refuse_before_capture_or_any_mutation() {
        for target in [
            json!({"pid":800}),
            json!({"window_id":11}),
            json!({"pid":0,"window_id":11}),
            json!({"pid":-1,"window_id":11}),
            json!({"pid":2147483648_u64,"window_id":11}),
            json!({"pid":4294968096_u64,"window_id":11}),
            json!({"pid":800,"window_id":4294967307_u64}),
            json!({"pid":800,"window_id":0}),
            json!({"pid":800,"window_id":-1}),
            json!({"pid":800.5,"window_id":11}),
            json!({"pid":800,"window_id":11.5}),
            json!({"pid":"800","window_id":11}),
            json!({"pid":800,"window_id":null}),
        ] {
            let tool = tool();
            let mut args = json!({"x":60,"y":80,"session":"slice-a-move"});
            args.as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            let commands = RefCell::new(Vec::new());
            let result = tool
                .invoke_overlay(
                    args,
                    |_, _| async { panic!("malformed target must not capture") },
                    |key, wid, x, y| {
                        commands.borrow_mut().push((key, wid, x, y));
                        std::future::ready(())
                    },
                )
                .await;
            assert_eq!(result.is_error, Some(true), "{target}");
            assert_eq!(position(&tool), (7.0, 9.0));
            assert!(commands.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn every_frame_refusal_preserves_registry_and_visual_commands() {
        for error in [
            px_frame::PxFrameError::WindowNotFound { window_id: 11 },
            px_frame::PxFrameError::CaptureUnavailable {
                window_id: 11,
                reason: "capture denied".into(),
            },
            px_frame::PxFrameError::FrameMismatch {
                window_id: 11,
                bounds_width: 500.0,
                bounds_height: 500.0,
                capture_width: 1200,
                capture_height: 1000,
                scale_x: 2.4,
                scale_y: 2.0,
            },
        ] {
            let tool = tool();
            let commands = RefCell::new(Vec::new());
            let expected = px_frame::refusal(&error);
            let result = tool
                .invoke_overlay(
                    json!({"x":60,"y":80,"pid":800,"window_id":11,"session":"slice-a-move", "_native_window_pixels": true}),
                    |_, _| std::future::ready(Err(px_frame::refusal(&error))),
                    |key, wid, x, y| {
                        commands.borrow_mut().push((key, wid, x, y));
                        std::future::ready(())
                    },
                )
                .await;
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert_eq!(position(&tool), (7.0, 9.0));
            assert!(commands.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn invalid_scale_or_output_refuses_before_publication() {
        for (scale, ratio, origin, x) in [
            (0.0, 1.0, 100.0, 60.0),
            (-2.0, 1.0, 100.0, 60.0),
            (f64::NAN, 1.0, 100.0, 60.0),
            (f64::INFINITY, 1.0, 100.0, 60.0),
            (2.0, 1.0, f64::INFINITY, 60.0),
        ] {
            assert_eq!(ratio, 1.0, "ratios come from the shared screenshot lookup");
            let tool = tool();
            let commands = RefCell::new(Vec::new());
            let result = tool
                .invoke_overlay(
                    json!({"x":x,"y":80,"pid":800,"window_id":11,"session":"slice-a-move"}),
                    |_, _| std::future::ready(Ok(frame(scale, origin, 580.0))),
                    |key, wid, x, y| {
                        commands.borrow_mut().push((key, wid, x, y));
                        std::future::ready(())
                    },
                )
                .await;
            assert_eq!(result.is_error, Some(true));
            assert_eq!(position(&tool), (7.0, 9.0));
            assert!(commands.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn foreign_or_missing_owners_refuse_before_capture_and_mutation() {
        for owner in [
            crate::windows::WindowOwner::ForeignPid {
                owner_pid: 900,
                owner_app_name: "Other app".into(),
            },
            crate::windows::WindowOwner::Unknown,
        ] {
            let tool = tool();
            let captured = std::cell::Cell::new(false);
            let commands = RefCell::new(Vec::new());
            let result = tool
                .invoke_overlay(
                    json!({"x":60,"y":80,"pid":800,"window_id":11,"session":"slice-a-move", "_native_window_pixels": true}),
                    |pid, wid| {
                        std::future::ready(require_window_owner(pid, wid, owner).map(|()| {
                            captured.set(true);
                            frame(2.0, 100.0, 580.0)
                        }))
                    },
                    |key, wid, x, y| {
                        commands.borrow_mut().push((key, wid, x, y));
                        std::future::ready(())
                    },
                )
                .await;
            assert_eq!(result.is_error, Some(true));
            assert!(!captured.get());
            assert_eq!(position(&tool), (7.0, 9.0));
            assert!(commands.borrow().is_empty());
        }
    }
}
