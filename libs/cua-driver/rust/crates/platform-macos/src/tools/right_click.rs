use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_action_names, copy_string_attr, kAXErrorSuccess, perform_action, AXUIElementRef,
};

use super::ToolState;

pub struct RightClickTool {
    state: Arc<ToolState>,
    visual_sink: Arc<dyn crate::cursor::visual::PointerVisualSink>,
}

impl RightClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self {
            state,
            visual_sink: Arc::new(crate::cursor::visual::OverlayVisualSink),
        }
    }
    #[cfg(test)]
    pub(crate) fn with_visual_sink(
        mut self,
        sink: Arc<dyn crate::cursor::visual::PointerVisualSink>,
    ) -> Self {
        self.visual_sink = sink;
        self
    }
    pub(crate) async fn dispatch_resolved(
        &self,
        key: &str,
        target: Option<crate::cursor::visual::ResolvedPointerTarget>,
        receipt: &crate::cursor::visual::DeliveryReceipt,
        native: impl std::future::Future<Output = ToolResult>,
    ) -> ToolResult {
        super::click::ClickTool::new(self.state.clone())
            .with_visual_sink(self.visual_sink.clone())
            .dispatch_resolved(key, target, receipt, async { None }, native)
            .await
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "right_click".into(),
        description:
            "Right-click against a target pid. Two addressing modes:\n\n\
             - `element_index` + `window_id` (from the last `get_window_state` snapshot) — \
               performs `AXShowMenu` on the cached element. Pure AX RPC, works on backgrounded / \
               hidden windows, no cursor move or focus steal. Requires a prior \
               `get_window_state(pid, window_id)` in this turn.\n\n\
             - `x`, `y` — synthesizes `rightMouseDown` / `rightMouseUp` CGEvent pair posted \
               to the pid. Driver converts image-pixel → screen-point internally. \
               `modifier` forces the CGEvent path (AX actions don't propagate modifier keys).\n\n\
             Exactly one of `element_index` or (`x` AND `y`) must be provided. `pid` always \
             required. `window_id` required when `element_index` is used."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer", "description": "Target process ID." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID. Required when element_index is used. Optional when element_token is supplied (the token carries it)."
                },
                "x": {
                    "type": "number",
                    "description": "X in window-local screenshot pixels. Must be provided together with y."
                },
                "y": {
                    "type": "number",
                    "description": "Y in window-local screenshot pixels. Must be provided together with x."
                },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys held during the right-click: cmd/shift/option/ctrl. Pixel path only."
                },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  false,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for RightClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // delivery_mode: foreground briefly fronts the window before the pixel
        // right-click (the explicit last resort for surfaces that drop
        // background CGEvents), via the same skylight assist click uses. The AX
        // (AXShowMenu) path is background-by-design and untouched.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        // Surface 6: element_token / element_index precedence resolution.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id").map(|v| v as u32);
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "right_click",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => (None, window_id_arg),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: wid,
                element_index: idx,
                via_token: _,
            } => (Some(idx), wid),
        };
        let x = args.opt_f64("x");
        let y = args.opt_f64("y");
        let has_xy = x.is_some() && y.is_some();
        let partial_xy = x.is_some() != y.is_some();
        let modifiers: Vec<String> = args.str_array("modifier");

        if partial_xy {
            return ToolResult::error("Provide both x and y together, not just one.");
        }
        if element_index.is_some() && has_xy {
            return ToolResult::error("Provide either element_index or (x, y), not both.");
        }
        if element_index.is_none() && !has_xy {
            return ToolResult::error(
                "Provide element_index or (x, y) to address the right-click target.",
            );
        }
        if element_index.is_some() && window_id.is_none() {
            return ToolResult::error("window_id is required when element_index is used.");
        }

        // ── AX element path ──────────────────────────────────────────────────
        if let (Some(idx), Some(wid)) = (element_index, window_id) {
            // Retain out of the cache so a concurrent get_window_state can't
            // free the element mid-action (use-after-free → daemon crash).
            let element_guard = match self.state.element_cache.get_element_retained(pid, wid, idx) {
                Some(e) => e,
                None => {
                    return ToolResult::error(format!(
                        "Element index {idx} not found. Call get_window_state first."
                    ))
                }
            };
            let element_guard = Arc::new(element_guard);
            let element_ptr = element_guard.as_ptr();

            let _mutation_lease = match super::gate_background_window_action(
                pid,
                wid,
                Some(element_ptr),
                cua_driver_core::background_input::BackgroundAction::AxSemantic,
            )
            .await
            {
                Ok(lease) => lease,
                Err(refusal_result) => return refusal_result,
            };

            let bounds_guard = element_guard.clone();
            let target = tokio::task::spawn_blocking(move || unsafe {
                crate::ax::bindings::element_screen_rect(bounds_guard.as_ptr() as AXUIElementRef)
            })
            .await
            .ok()
            .flatten()
            .and_then(|rect| crate::cursor::visual::ResolvedPointerTarget::from_bounds(wid, rect));
            let visual = Arc::new(crate::cursor::visual::DeliveryReceipt::default());
            visual.validate_ax_target(pid, wid, element_guard, target);
            let registry = self.state.cursor_registry.clone();
            let native = async {
                let visual = visual.clone();
                let result = tokio::task::spawn_blocking(move || {
                    ax_show_menu(element_ptr, idx, pid, wid, &visual, &registry)
                })
                .await;
                match result {
                    Ok(Ok(msg)) => ToolResult::text(msg),
                    Ok(Err(e)) => ToolResult::error(format!("Click failed: {e}")),
                    Err(e) => ToolResult::error(format!("Task error: {e}")),
                }
            };
            return self
                .dispatch_resolved(&cursor_key, target, &visual, native)
                .await;
        }

        // ── Pixel path ───────────────────────────────────────────────────────
        let (mut cx, mut cy) = (x.unwrap(), y.unwrap());
        // Scale back from downscaled-image space to native pixels when needed.
        if let Some(ratio) = self.state.resize_registry.ratio(pid, window_id) {
            cx *= ratio;
            cy *= ratio;
        }

        // Window-local → screen coordinate translation + win-local logical coords
        // for CGEventSetWindowLocation (shared with click.rs via px_frame, which
        // refuses a window with no live frame).
        let mut approach_frame = None;
        let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
            match super::px_frame::resolve_or_refuse(wid).await {
                Ok(frame) => {
                    approach_frame = Some(frame.bounds.clone());
                    let translated = frame.to_screen(cx, cy);
                    if !delivery_mode.is_foreground()
                        && (translated.2 < 0.0
                            || translated.3 < 0.0
                            || translated.2 > frame.bounds.width
                            || translated.3 > frame.bounds.height)
                    {
                        return ToolResult::error(format!(
                            "right_click: window-local point ({:.1}, {:.1}) pt lies outside \
                             window {wid}'s {:.0}×{:.0} pt frame; background delivery refused",
                            translated.2, translated.3, frame.bounds.width, frame.bounds.height
                        ));
                    }
                    translated
                }
                Err(refusal) => return refusal,
            }
        } else {
            (cx, cy, cx, cy)
        };

        let _mutation_lease = if !delivery_mode.is_foreground() {
            if let Some(wid) = window_id {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    None,
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
                )
                .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            }
        } else {
            None
        };

        let visual = Arc::new(crate::cursor::visual::DeliveryReceipt::default());
        visual.validate_pixel_frame(pid, window_id, approach_frame);
        let target = crate::cursor::visual::point(screen_x, screen_y, window_id);

        let mod_suffix = if modifiers.is_empty() {
            String::new()
        } else {
            format!(" with {}", modifiers.join("+"))
        };

        let fg = delivery_mode.is_foreground() && window_id.is_some();
        let native = async {
            let visual = visual.clone();
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let do_it =
                    move |observed: &mut dyn FnMut(std::time::Instant)| -> anyhow::Result<()> {
                        let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                        if let Some(wid) = window_id {
                            crate::input::mouse::right_click_at_xy_with_window_local_observed(
                                pid,
                                screen_x,
                                screen_y,
                                win_local_x,
                                win_local_y,
                                wid,
                                &m,
                                observed,
                            )
                        } else {
                            crate::input::mouse::right_click_at_xy_observed(
                                pid, screen_x, screen_y, &m, observed,
                            )
                        }
                    };
                let do_it = || visual.dispatch_mouse(do_it);
                // Foreground rung: brief front → right-click → restore prior frontmost.
                match (fg, window_id) {
                    (true, Some(wid)) => {
                        crate::input::skylight::with_foreground_assist_checked(
                            pid as libc::pid_t,
                            wid,
                            &|| visual.ensure_current(),
                            do_it,
                        )?;
                        Ok(())
                    }
                    _ => do_it(),
                }
            })
            .await;
            let mode_label = if fg {
                " (delivery_mode:foreground)"
            } else {
                ""
            };
            match result {
            Ok(Ok(())) => ToolResult::text(format!("Right-clicked{mod_suffix} at ({screen_x:.1}, {screen_y:.1}){mode_label}."))
                .with_structured(serde_json::json!({
                    "path": if fg { "cgevent_fg" } else { "cgevent" }, "verified": false, "effect": "unverifiable"
                })),
            Ok(Err(e)) => ToolResult::error(format!("Right-click failed: {e}")),
            Err(e)     => ToolResult::error(format!("Task error: {e}")),
        }
        };
        self.dispatch_resolved(&cursor_key, target, &visual, native)
            .await
    }
}

// ── Blocking AX path ─────────────────────────────────────────────────────────

fn ax_show_menu(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    wid: u32,
    visual: &crate::cursor::visual::DeliveryReceipt,
    registry: &crate::cursor::CursorRegistry,
) -> anyhow::Result<String> {
    let element = element_ptr as AXUIElementRef;
    visual.ensure_current()?;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();

    let advertised = unsafe { copy_action_names(element) };

    // Only attempt the pure-AX AXShowMenu when the element actually advertises
    // it. Plain controls (NSButton, custom NSView click targets, most web
    // nodes) DON'T — calling AXShowMenu on them returns kAXErrorActionUnsupported
    // (-25206), which used to surface as a hard "AXShowMenu failed" error and
    // forced the agent onto raw pixels. Instead, resolve the element's on-screen
    // center and synthesize a REAL pixel right-click there — the same actuation
    // a user performs, delivered to backgrounded windows via the window-local
    // primitive. This makes "right-click element N" land on any element, not
    // just ones with a native context-menu AX action.
    if advertised.iter().any(|a| a == "AXShowMenu") {
        visual.ensure_current()?;
        let err = unsafe { perform_action(element, "AXShowMenu") };
        if err == kAXErrorSuccess {
            visual.accepted();
            return Ok(format!(
                "Shown menu for [{idx}] {role} \"{title}\" (AXShowMenu)."
            ));
        }
        // Advertised but the action failed — fall through to the pixel path
        // rather than erroring out.
        tracing::debug!("AXShowMenu returned {err} for [{idx}]; falling back to pixel right-click");
    }

    // Pixel right-click at the element's screen-space center.
    let (cx, cy) =
        unsafe { crate::ax::bindings::element_screen_center(element) }.ok_or_else(|| {
            anyhow::anyhow!(
                "[{idx}] {role} \"{title}\" advertises no AXShowMenu and has no resolvable \
             on-screen center for a pixel right-click. Pass x, y directly."
            )
        })?;
    let (wx, wy) = crate::windows::window_bounds_by_id(wid)
        .map(|b| (cx - b.x, cy - b.y))
        .ok_or_else(|| anyhow::anyhow!("right-click window frame disappeared before fallback"))?;
    visual.dispatch_mouse_at(registry, cx, cy, wid, |cx, cy, observed| {
        crate::input::mouse::right_click_at_xy_with_window_local_observed(
            pid,
            cx,
            cy,
            wx,
            wy,
            wid,
            &[],
            observed,
        )
    })?;
    Ok(format!(
        "Right-clicked [{idx}] {role} \"{title}\" at element center ({cx:.0}, {cy:.0}) \
         (pixel right-click; element advertises no AXShowMenu)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn quick_approach_route_never_polls_input_without_target_frame() {
        let tool = RightClickTool::new(Arc::new(ToolState::default()));
        let receipt = crate::cursor::visual::DeliveryReceipt::default();
        let call = tool.dispatch_resolved(
            "quick-right_click",
            crate::cursor::visual::point(20.0, 30.0, Some(42)),
            &receipt,
            async { panic!("input before target frame") },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        assert!(!receipt.was_accepted());
    }

    #[test]
    fn slice_a_fix_native_fallback_rebinds_actual_center_and_drops_stale_bounds() {
        use crate::cursor::visual::{
            begin_pointer_action, test_support::RecordingSink, ResolvedPointerTarget,
        };
        for initial in [
            ResolvedPointerTarget::from_bounds(42, [10.0, 20.0, 20.0, 20.0]),
            None,
        ] {
            for (accepted, failed) in [(false, true), (true, false), (true, true)] {
                let registry = crate::cursor::CursorRegistry::new();
                let sink = Arc::new(RecordingSink::default());
                let receipt = begin_pointer_action(
                    &registry,
                    sink.clone(),
                    "right_click-fallback",
                    initial,
                    cursor_overlay::CursorAction::Click,
                );
                // AX bounds were at A (or unreadable). Its failed semantic attempt
                // leaves the receipt unaccepted. The existing native read found B.
                let result = receipt.dispatch_at(&registry, 90.0, 80.0, 42, |x, y| {
                    assert_eq!(
                        (x, y),
                        (90.0, 80.0),
                        "native coordinates must stay unchanged"
                    );
                    let pos = registry
                        .get("right_click-fallback")
                        .unwrap()
                        .position
                        .unwrap();
                    assert_eq!(
                        (pos.x, pos.y),
                        (90.0, 80.0),
                        "registry must follow the actual fallback"
                    );
                    if accepted && failed {
                        receipt.accepted();
                    }
                    if failed {
                        Err(anyhow::anyhow!("native outcome error"))
                    } else {
                        Ok("delivered")
                    }
                });
                assert_eq!(
                    result.map_err(|error| error.to_string()),
                    if failed {
                        Err("native outcome error".to_owned())
                    } else {
                        Ok("delivered")
                    }
                );
                assert_eq!(receipt.was_accepted(), accepted);
                let events = sink.1.lock().unwrap();
                let contacts: Vec<_> = events
                    .iter()
                    .filter(|event| event.phase == cursor_overlay::VisualPhase::Contact)
                    .collect();
                assert_eq!(contacts.len(), usize::from(accepted));
                let last = events.last().unwrap();
                assert_eq!(last.target, Some((90.0, 80.0)));
                assert_eq!(last.bounds, None);
                assert_eq!(last.window, Some(42));
                assert_eq!(last.id, events[0].id, "fallback retains action ownership");
                let mut core =
                    cursor_overlay::RenderStateCore::new(cursor_overlay::CursorConfig::default());
                let display = cursor_overlay::DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                };
                // Include a drain of the first rectangle, so omitting later bounds
                // cannot leave an already rendered stale rectangle behind.
                for event in events.iter() {
                    core.apply_visual_event(event.clone(), Some(display), event.timestamp);
                }
                assert!(
                    core.focus_rect.is_none(),
                    "fallback must clear the earlier rendered bounds"
                );
                if accepted {
                    // The user amendment keeps fallback travel visible. Native
                    // acceptance is immediate; the pulse follows scheduled arrival.
                    assert!(core.path.is_some());
                    assert!(core.contact.is_none());
                    let intent = events
                        .iter()
                        .rev()
                        .find(|event| event.phase == cursor_overlay::VisualPhase::Intent)
                        .unwrap();
                    core.advance_visual_presentation(
                        intent.timestamp + std::time::Duration::from_millis(220),
                    );
                    let pulse = core.contact.unwrap();
                    assert_eq!(pulse.target, (90.0, 80.0));
                    assert_eq!(pulse.timestamp, contacts[0].timestamp);
                    assert!(pulse.presentation_timestamp >= pulse.timestamp);
                    assert!((core.pos.0 - core.heading.cos() * 16.0 - 90.0).abs() < 0.001);
                    assert!((core.pos.1 - core.heading.sin() * 16.0 - 80.0).abs() < 0.001);
                    assert!(core.path.is_none());
                    core.advance_visual_presentation(
                        pulse.presentation_timestamp + std::time::Duration::from_millis(150),
                    );
                    assert!(core.contact.is_none());
                }
            }
        }
    }

    #[test]
    fn slice_a_pointer_route_dispatch_feedback_preserves_error_and_fallback() {
        use crate::cursor::visual::{begin_pointer_action, point, test_support::RecordingSink};
        let registry = crate::cursor::CursorRegistry::new();
        let sink = Arc::new(RecordingSink::default());
        let receipt = begin_pointer_action(
            &registry,
            sink.clone(),
            "right_click",
            point(90.0, 80.0, Some(42)),
            cursor_overlay::CursorAction::Click,
        );
        let error = receipt.dispatch(|| Err::<(), _>("native refusal"));
        assert_eq!(error, Err("native refusal"));
        assert_eq!(sink.1.lock().unwrap().len(), 1);
        receipt
            .dispatch(|| {
                let pos = registry.get("right_click").unwrap().position.unwrap();
                assert_eq!((pos.x, pos.y), (90.0, 80.0));
                Ok::<_, ()>(())
            })
            .unwrap();
        receipt.accepted();
        let events = sink.1.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].phase, cursor_overlay::VisualPhase::Contact);
        assert_eq!(events[1].target, Some((90.0, 80.0)));
    }
}
