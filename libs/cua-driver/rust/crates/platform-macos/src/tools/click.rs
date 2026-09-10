//! click tool — matches the Swift reference ClickTool.swift.
//!
//! Two addressing modes:
//!
//! * **AX path** (`element_index` + `window_id`): performs AXAction on the cached
//!   element. Fires via AX RPC — the target app never needs to be frontmost.
//!   Extra behaviors vs. the naive dispatch:
//!   - AXTextField / AXTextArea: 800 ms post-click delay for WebKit DOM focus settle.
//!   - AXPopUpButton: appends the list of available options and redirects to set_value.
//!   - Advertised-action warning if the element didn't list the requested action.
//!
//! * **Pixel path** (`x`, `y`): synthesises CGEvent mouse clicks and posts them to
//!   the target pid.  `from_zoom=true` translates zoom-crop pixel coordinates back
//!   to full-window space using the most recent `zoom` context stored per-pid.

use async_trait::async_trait;
use cua_driver_contract::ClickButton;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_legacy_click_input,
};
use serde_json::Value;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_children, copy_string_attr, element_at_screen_position,
    element_screen_rect, kAXErrorSuccess, AXUIElementPerformAction, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFRelease, TCFType};

use super::ToolState;
use crate::cursor::visual::{
    emit_pointer_target, DeliveryReceipt, OverlayVisualSink, PointerVisualSink,
    ResolvedPointerTarget,
};

pub struct ClickTool {
    state: Arc<ToolState>,
    visual_sink: Arc<dyn PointerVisualSink>,
}

impl ClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self {
            state,
            visual_sink: Arc::new(OverlayVisualSink),
        }
    }

    pub(crate) fn with_visual_sink(mut self, sink: Arc<dyn PointerVisualSink>) -> Self {
        self.visual_sink = sink;
        self
    }

    // Both futures are lazy: a completed semantic route never polls native input.
    // Selection can deliver input and then fail its postcondition readback. Its
    // receipt keeps that delivery fact independent of the tool result.
    pub(crate) async fn dispatch_resolved(
        &self,
        cursor_key: &str,
        target: Option<ResolvedPointerTarget>,
        delivery_receipt: &DeliveryReceipt,
        semantic: impl std::future::Future<Output = Option<ToolResult>>,
        native: impl std::future::Future<Output = ToolResult>,
    ) -> ToolResult {
        let visual = emit_pointer_target(
            &self.state.cursor_registry,
            self.visual_sink.as_ref(),
            cursor_key,
            target,
        );
        delivery_receipt.attach(self.visual_sink.clone(), visual);
        let _approach = match delivery_receipt.prepare_click().await {
            Ok(guard) => guard,
            Err(error) => return crate::cursor::visual::approach_refusal(error),
        };
        if let Err(error) = delivery_receipt.ensure_current() {
            return crate::cursor::visual::approach_refusal(error);
        }
        let result = match semantic.await {
            Some(result) => result,
            None => {
                if let Err(error) = delivery_receipt.ensure_current() {
                    return crate::cursor::visual::approach_refusal(error);
                }
                native.await
            }
        };
        result
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Focus posture for the raw pixel transport after AX hit-testing has failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelActivationPolicy {
    /// Standard background delivery: suppress activation of the target.
    SuppressTarget,
    /// Left-click with a concrete window: intentionally make the target
    /// AppKit-active without raising it, while suppressing every other app.
    AllowTargetWithoutRaise,
    /// Explicit foreground rung owns its brief activation and restoration.
    ForegroundAssist,
}

#[derive(Clone, Copy, Debug)]
struct SelectionPixelTarget {
    screen_x: f64,
    screen_y: f64,
    window_x: f64,
    window_y: f64,
}

fn selection_readback_confirms(
    before: bool,
    after: bool,
    has_modifiers: bool,
    prior_selected_peers_preserved: bool,
) -> bool {
    if has_modifiers {
        after != before && prior_selected_peers_preserved
    } else {
        after
    }
}

const SELECTION_READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const SELECTION_READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(25);
const SELECTION_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);
const SELECTION_READBACK_STABILITY: std::time::Duration = std::time::Duration::from_millis(250);

fn pixel_activation_policy(
    button: &str,
    effective_foreground: bool,
    has_window: bool,
) -> PixelActivationPolicy {
    if effective_foreground {
        PixelActivationPolicy::ForegroundAssist
    } else if button == "left" && has_window {
        PixelActivationPolicy::AllowTargetWithoutRaise
    } else {
        PixelActivationPolicy::SuppressTarget
    }
}

/// Return the prior foreground pid that should be restored after a raw
/// background pixel click.
///
/// This decision deliberately depends on observed application state rather
/// than the private focus recipe's return value. The recipe can be unavailable
/// or partially fail while the raw click still makes the target AppKit-active;
/// in that case the allow-target suppression lease will not restore it for us.
fn background_pixel_restore_pid(
    activation_policy: PixelActivationPolicy,
    prior_front: Option<i32>,
    target_pid: i32,
    observed_front: Option<i32>,
) -> Option<i32> {
    if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
        && prior_front != Some(target_pid)
        && observed_front == Some(target_pid)
    {
        prior_front
    } else {
        None
    }
}

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "click".into(),
        description:
            "Click against a target pid. **Prefer `element_token` over pixel \
             coordinates** — the token works on backgrounded / minimized / hidden / \
             off-Space windows, identifies one exact snapshot element, and tells \
             you what you're clicking via the cached element's role + label. Reach for \
             `x, y` only when the target is a canvas / video / WebGL / custom-drawn surface \
             that doesn't appear in the AX tree.\n\n\
             Two addressing modes:\n\n\
             - element_token, or element_index + snapshot_id (from get_window_state): AX action path. \
               Works on backgrounded/hidden windows. No cursor move, no focus steal. \
               The snapshot cache is scoped per (pid, window_id) and is replaced by the \
               next snapshot of the same window — re-snapshot every turn before clicking.\n\n\
             - x, y (window-local screenshot pixels, top-left origin of the PNG returned \
               by get_window_state): CGEvent path. Synthesizes mouse events and posts to \
               pid. Use modifier for cmd/shift/option/ctrl. Needs a visible on-screen \
               window to anchor the conversion.\n\n\
             button: \"left\" (default), \"right\", or \"middle\". Defaults to left so the \
             field is fully back-compat — omit it and you get the legacy left-click behaviour. \
             Pixel path: routes through the CGEvent left/right/middle mouse-button primitives. \
             AX path: \"right\" maps to AXShowMenu (same surface as the dedicated `right_click` \
             tool); \"middle\" has no AX equivalent and falls back to a pixel middle-click at the \
             element's center.\n\
             action: press (default), show_menu, pick, confirm, cancel, open.\n\
             from_zoom: set true after a zoom call to auto-translate zoom-image pixel \
             coordinates to full-window space."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            // `pid` is conditionally required — needed for window/element clicks
            // but omitted for windowless `scope:"desktop"` clicks — so it is NOT
            // in `required`; the code validates it with a clear error when needed.
            // (Keeps the contract consistent across platforms; see
            // cua_driver_core::tool_schema.)
            "required": [],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid":           { "type": "integer", "description": "Target process ID." },
                "window_id":     { "type": "integer", "description": "Target window ID. Required for element_index. Optional when element_token is supplied (the token carries it)." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x":             { "type": "number",  "description": "X in screenshot pixels. A window target uses the get_window_state PNG; a desktop target uses the native get_desktop_state PNG. The driver reverses Retina backing scale and any window-image downscale." },
                "y":             { "type": "number",  "description": "Y in screenshot pixels from the image selected by target." },
                "action":        { "type": "string",  "description": "AX action: press, show_menu, pick, confirm, cancel, open." },
                "button":        {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button. Default: \"left\" — omit for legacy left-click behaviour. Pixel path uses the matching CGEvent primitive; AX path maps \"right\" to AXShowMenu and falls back to a pixel middle-click at the element's center for \"middle\"."
                },
                "count":         { "type": "integer", "description": "Click count (pixel path only). Default 1." },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl."
                },
                "from_zoom": {
                    "type": "boolean",
                    "description": "When true, x and y are in the last zoom image for this pid; driver translates back to full-window coordinates."
                },
                "debug_image_out": {
                    "type": "string",
                    "description": "Optional file path. When set on a pixel-addressed click, captures a fresh screenshot, draws a red crosshair at (x, y), and writes the PNG. Use to verify coordinate spaces. Requires window_id; incompatible with from_zoom."
                },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Best-effort-background ladder rung (default \"background\"). \"background\": perform the AX action or post the CGEvent without fronting. \"foreground\": briefly front the window, act, let transient UI settle, then restore the prior frontmost app. Requires window_id. Modified clicks require \"foreground\" so macOS observes physical modifier-key state. A generic click has no independent postcondition read-back, except selection of list-like AX rows whose AXSelected state can be confirmed; otherwise confirm the effect from a fresh state snapshot. Use the agent loop: background AX (element_index) → snapshot → background pixel (x/y) → snapshot → delivery_mode:\"foreground\"."
                },
                "scope": {
                    "type": "string",
                    "enum": ["window", "desktop"],
                    "description": "Coordinate frame for a windowless screen-absolute click (default \"window\"). Pass \"desktop\" when sending x,y with NO pid/window_id — the coordinates are then true screen pixels (read from get_desktop_state with scope=\"desktop\"). Per-call; not a setting."
                }
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
impl Tool for ClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;

        // ── Window-less screen-absolute branch (scope="desktop") ──────
        // x,y given with NO pid and NO window_id → the coordinates are TRUE
        // SCREEN pixels. This is the foreground, vision-driven desktop-scope
        // path, the macOS peer of the Windows WindowFromPoint click. Gate on the
        // effective scope: under "window" return a structured
        // `desktop_scope_disabled` error (same contract as Windows) rather than
        // silently treating window-local pixels as screen pixels.
        let has_pid = args.get("pid").map(|v| !v.is_null()).unwrap_or(false);
        let has_window_id = args.get("window_id").map(|v| !v.is_null()).unwrap_or(false);
        let has_xy = args.get("x").map(|v| v.is_number()).unwrap_or(false)
            && args.get("y").map(|v| v.is_number()).unwrap_or(false);
        if has_xy && !has_pid && !has_window_id {
            // `scope` is a per-call param now (default "window"); pass
            // scope="desktop" to enable screen-absolute clicks.
            let scope = args.str_or("scope", "window");
            if scope != "desktop" {
                return ToolResult::error(
                    "click: x,y given with no pid/window_id, but scope is \"window\". \
                     Screen-absolute clicks require desktop scope. Pass scope=\"desktop\" \
                     (and use get_desktop_state with scope=\"desktop\" to read true \
                     screen pixels) first."
                        .to_string(),
                )
                .with_structured(serde_json::json!({
                    "code": "desktop_scope_disabled",
                    "scope": scope,
                    "suggestion": "pass scope=\"desktop\"",
                }));
            }
            let input = match parse_legacy_click_input(&args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let sx_shot = input.x;
            let sy_shot = input.y;
            // ── Desktop-screenshot pixels → logical screen points ──────────────
            // The vision invariant: the pixel an agent reads off the screenshot it
            // was handed is the pixel that gets clicked. `get_desktop_state`
            // returns the display at NATIVE pixels (e.g. 3024×1964 on a 2× Retina
            // display whose logical size is 1512×982), but everything below — the
            // window-under-point hit test (logical CGWindow bounds), the cursor
            // warp, and the CGEvent post — operates in LOGICAL screen points. So
            // x,y arrive in desktop-SCREENSHOT space (what the agent reads off the
            // PNG) and must be divided by the screenshot↔logical ratio, or a
            // center-pixel pick warps to the corner (off by the backing scale).
            //
            // Derive the ratio the same way `get_desktop_state` reports it: native
            // screenshot width / logical screen width. This is robust even when
            // CGDisplayPixelsWide under-reports the backing scale (it returns the
            // scaled-mode point width on some Retina configs → a bogus 1.0).
            let desktop_ratio = tokio::task::spawn_blocking(|| {
                let logical_w =
                    super::get_screen_size::main_screen_size().map(|(w, _, _)| w as f64);
                let shot_w = crate::capture::screenshot_display_bytes()
                    .ok()
                    .and_then(|png| crate::capture::png_dimensions(&png).ok())
                    .map(|(w, _)| w as f64);
                match (shot_w, logical_w) {
                    (Some(sw), Some(lw)) if lw > 0.0 && sw > lw => sw / lw,
                    _ => 1.0,
                }
            })
            .await
            .unwrap_or(1.0);
            let sx = sx_shot / desktop_ratio;
            let sy = sy_shot / desktop_ratio;
            let button = match input.button.unwrap_or(ClickButton::Left) {
                ClickButton::Left => "left",
                ClickButton::Right => "right",
                ClickButton::Middle => "middle",
            }
            .to_owned();
            let count = input.count.unwrap_or(1) as usize;
            if count == 0 {
                return ToolResult::error("click.count must be at least 1.")
                    .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
            }
            let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
            let delivery_receipt = Arc::new(DeliveryReceipt::default());
            let dispatch = async {
                let btn = button.clone();
                let desktop_modifiers: Vec<String> = args.str_array("modifier");
                let delivered = delivery_receipt.clone();
                let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    // Desktop scope is explicitly foreground and vision-driven: post
                    // at the global HID tap so WindowServer delivers to the window
                    // actually visible at this point. PID-posting here would silently
                    // turn the foreground contract back into background delivery.
                    let modifier_refs: Vec<&str> =
                        desktop_modifiers.iter().map(String::as_str).collect();
                    delivered.dispatch_checked(|| {
                        crate::input::mouse::click_at_xy_desktop_with_modifiers(
                            sx,
                            sy,
                            count,
                            &btn,
                            &modifier_refs,
                        )
                    })
                })
                .await;
                let button_label = match button.as_str() {
                    "right" => "right-click",
                    "middle" => "middle-click",
                    _ => "click",
                };
                match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent screen-absolute {button_label} at desktop-pixel \
                     ({sx_shot:.0},{sy_shot:.0}) → screen-point ({sx:.0},{sy:.0}) \
                     (desktop scope; not driver-verified)."
                ))
                .with_structured(serde_json::json!({ "path": "cgevent_hid", "verified": false, "effect": "unverifiable" })),
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            }
            };
            return self
                .dispatch_resolved(
                    &cursor_key,
                    Some(ResolvedPointerTarget {
                        x: sx,
                        y: sy,
                        window_id: None,
                        element_bounds: None,
                    }),
                    &delivery_receipt,
                    async { None },
                    dispatch,
                )
                .await;
        }

        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Resolve this action's cursor key so its click-pulse / glide land on
        // the calling session's cursor, not the shared "default" one.
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        // Surface 6: resolve element_token / element_index precedence
        // BEFORE the pixel-path fallback. Token wins on disagreement; a
        // stale token returns an explicit error instead of silently
        // falling back to the integer (Surface 6 hard constraint).
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id").map(|v| v as u32);
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "click",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, _via_token) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => (None, window_id_arg, false),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: wid,
                element_index: idx,
                via_token,
            } => (Some(idx), wid, via_token),
        };
        let x = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|i| i as f64));
        let y = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|i| i as f64));
        let action = args.str_or("action", "press");
        // Surface 5: optional `button` arg, default "left" preserves legacy behaviour.
        // Pixel path: routes to left/right/middle CGEvent primitives.
        // AX path: "right" delegates to AXShowMenu (same surface as right_click);
        // "middle" has no AX equivalent and falls back to a pixel middle-click
        // at the element's screen-space center.
        let button_str = args.str_or("button", "left").to_lowercase();
        // delivery_mode: per-call ladder rung. Foreground briefly activates the
        // target for both AX and pixel paths, then restores the prior app.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        // Reject unknown buttons explicitly so silent left-click fall-through can't
        // mask a typo. Keep "" → default left for old clients that never sent the field.
        if !matches!(button_str.as_str(), "" | "left" | "right" | "middle") {
            return ToolResult::error(format!(
                "click: unknown button \"{button_str}\" — expected one of left, right, middle."
            ));
        }
        let button_str = if button_str.is_empty() {
            "left".to_string()
        } else {
            button_str
        };
        let count = args.u64_or("count", 1) as usize;
        let from_zoom = args.bool_or("from_zoom", false);
        let debug_image_out = args.opt_str("debug_image_out");
        let modifiers: Vec<String> = args.str_array("modifier");

        // PID-routed key transitions can look correct for one AX poll and then
        // collapse to a plain click once AppKit resolves the gesture. Refuse
        // that false-success path. The explicit foreground rung uses physical
        // HID modifier transitions under an exact-window activation guard.
        if !modifiers.is_empty() && (!delivery_mode.is_foreground() || window_id.is_none()) {
            return ToolResult::error(
                "click modifiers require delivery_mode:\"foreground\" and window_id on macOS; \
                 background PID-routed events cannot preserve live modifier-key state",
            )
            .with_structured(serde_json::json!({
                "code": "background_unavailable",
                "effect": "refused",
                "escalation": {
                    "recommended": "foreground",
                    "reason": "macOS modifier clicks require exact-window HID delivery so the target observes live modifier state"
                }
            }));
        }

        if let (Some(idx), Some(wid)) = (element_index, window_id) {
            // ── AX element path ────────────────────────────────────────────
            // Retain the element out of the cache so it can't be freed by a
            // concurrent get_window_state on the same (pid, window_id) while
            // this click is mid-flight (use-after-free → daemon crash). The
            // guard lives to the end of this method, past the AX action below.
            let element_guard = match self.state.element_cache.get_element_retained(pid, wid, idx) {
                Some(e) => e,
                None => {
                    return ToolResult::error(format!(
                        "Element index {idx} not found in cache for pid={pid} window_id={wid}. \
                     Call get_window_state first."
                    ))
                }
            };
            let element_guard = Arc::new(element_guard);
            let element_ptr = element_guard.as_ptr();

            // ── Exact-target background gate (macOS background input v1) ──
            // The element branch is semantic AX delivery, except button=middle
            // which falls back to a routed pixel click at the element's center
            // and is therefore held to the stricter WindowPointer rung. Gate
            // BEFORE any cursor/dispatch work so a stale or sibling-owned
            // target refuses instead of acting on the wrong window.
            let _mutation_lease = if !delivery_mode.is_foreground() {
                let gate_action = if button_str == "middle" {
                    cua_driver_core::background_input::BackgroundAction::WindowPointer
                } else {
                    cua_driver_core::background_input::BackgroundAction::AxSemantic
                };
                match super::gate_background_window_action(pid, wid, Some(element_ptr), gate_action)
                    .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            };

            // Surface 5: button=right on the AX path → AXShowMenu (the same surface
            // the dedicated `right_click` tool dispatches). Threads through the
            // identical perform_ax_click code path with the action remapped.
            let effective_action = if button_str == "right" && action == "press" {
                "show_menu".to_string()
            } else {
                action.clone()
            };

            let bounds_guard = element_guard.clone();
            let bounds = tokio::task::spawn_blocking(move || unsafe {
                element_screen_rect(bounds_guard.as_ptr() as AXUIElementRef)
            })
            .await
            .ok()
            .flatten();
            let target = bounds.and_then(|rect| ResolvedPointerTarget::from_bounds(wid, rect));
            let center = target.map(|target| (target.x, target.y));
            if button_str == "middle" && center.is_none() {
                return ToolResult::error(
                    "click(button=middle) on element_index: could not resolve element \
                     center for the pixel-middle-click fallback. Pass x, y directly.",
                );
            }

            let delivery_receipt = Arc::new(DeliveryReceipt::default());
            delivery_receipt.validate_ax_target(pid, wid, element_guard.clone(), target);
            let dispatch = async {
                // Surface 5: button=middle on the AX path has no AX equivalent.
                // Fall back to a pixel middle-click at the element's screen-space center
                // so the request still produces a real middle-button event (browser tab
                // close, autoscroll, etc.). If we can't resolve a center, error rather
                // than silently degrade to AXPress.
                if button_str == "middle" {
                    let (cx, cy) = center.expect("middle-click bounds checked before intent");

                    let mods_owned = modifiers.clone();
                    let foreground = delivery_mode.is_foreground();
                    let receipt = delivery_receipt.clone();
                    let result = tokio::task::spawn_blocking(move || {
                    let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                    if foreground && !m.is_empty() {
                        crate::input::skylight::with_foreground_hid_activation(
                            pid as libc::pid_t,
                            wid,
                            || {
                                receipt.dispatch_checked(|| crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    cx, cy, 1, "middle", &m,
                                ))
                            },
                        )
                    } else {
                        receipt.dispatch_checked(|| crate::input::mouse::middle_click_at_xy(pid, cx, cy, &m))
                    }
                })
                .await;
                    return match result {
                    Ok(Ok(())) => ToolResult::text(format!(
                        "✅ Posted middle-click to pid {pid} at element [{idx}] center \
                         (background CGEvent; not driver-verified — confirm via screenshot)."
                    ))
                    .with_structured(serde_json::json!({ "path": "cgevent", "verified": false, "effect": "unverifiable" })),
                    Ok(Err(e)) => ToolResult::error(format!("Middle-click failed: {e}")),
                    Err(e)     => ToolResult::error(format!("Task error: {e}")),
                };
                }

                // Finder icon/list items can expose a readable AXSelected state
                // while refusing both AXSelected writes and AXPress. Resolve a
                // verified coordinate frame only for those collection-like
                // elements so perform_ax_click can cross that one failed semantic
                // rung internally and confirm the result by AX read-back.
                let selection_candidate = if effective_action == "press" {
                    let selection_guard = element_guard.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::input::ax_actions::nearest_container_selection_state(
                            selection_guard.as_ptr(),
                        )
                        .is_some()
                    })
                    .await
                    .unwrap_or(false)
                } else {
                    false
                };
                let mut selection_pixel = if selection_candidate {
                    if let Some((cx, cy)) = center {
                        super::px_frame::resolve_or_refuse(wid)
                            .await
                            .ok()
                            .map(|frame| SelectionPixelTarget {
                                screen_x: cx,
                                screen_y: cy,
                                window_x: cx - frame.bounds.x,
                                window_y: cy - frame.bounds.y,
                            })
                    } else {
                        None
                    }
                } else {
                    None
                };
                // The selection fallback delivers a routed window-local pixel
                // click — a stricter (WindowPointer) rung than the semantic gate
                // above. In background, drop the fallback rather than silently
                // escalate when the pointer rung would refuse (e.g. a
                // minimized/hidden target); the semantic path still runs.
                if selection_pixel.is_some()
                    && !delivery_mode.is_foreground()
                    && _mutation_lease
                        .as_ref()
                        .expect("background element actions hold the per-pid lease")
                        .gate_again(
                            wid,
                            Some(element_ptr),
                            cua_driver_core::background_input::BackgroundAction::WindowPointer,
                        )
                        .await
                        .is_err()
                {
                    selection_pixel = None;
                }

                // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
                // Capture prior frontmost, arm the wildcard suppressor in the
                // snapshot, then arm a targeted suppressor across the AX action
                // itself via FocusGuard. After the action returns, detect any
                // new-window / foreground side-effects and append a one-liner
                // suffix matching Swift's wording.
                let prior_front = apps::frontmost_pid();
                let foreground = delivery_mode.is_foreground();
                let snapshot = if foreground {
                    WindowChangeDetector::snapshot_without_suppression(prior_front)
                } else {
                    WindowChangeDetector::snapshot(prior_front)
                };

                // Run AX work on a blocking thread (can't block async executor).
                // Use `effective_action` so button=right rewrites press → show_menu.
                let action_clone = effective_action.clone();
                let selection_modifiers = modifiers.clone();
                let delivered = delivery_receipt.clone();
                let result = focus_guard::with_focus_suppressed(
                    if foreground { None } else { Some(pid) },
                    prior_front,
                    "click.AXPress",
                    || async move {
                        tokio::task::spawn_blocking(move || {
                            if foreground {
                                let mut outcome = None;
                                let has_modifiers = !selection_modifiers.is_empty();
                                let action = || {
                                    outcome = Some(perform_ax_click(
                                        element_ptr,
                                        idx,
                                        pid,
                                        wid,
                                        &action_clone,
                                        &delivered,
                                        selection_pixel,
                                        &selection_modifiers,
                                        foreground,
                                    )?);
                                    std::thread::sleep(std::time::Duration::from_millis(150));
                                    Ok(())
                                };
                                let fronted = if has_modifiers {
                                    crate::input::skylight::with_foreground_hid_activation(
                                        pid as libc::pid_t,
                                        wid,
                                        action,
                                    )?;
                                    true
                                } else {
                                    crate::input::skylight::with_foreground_assist(
                                        pid as libc::pid_t,
                                        wid,
                                        action,
                                    )?
                                };
                                let outcome = outcome.ok_or_else(|| {
                                    anyhow::anyhow!("foreground AX click did not execute")
                                })?;
                                Ok((outcome, fronted))
                            } else {
                                perform_ax_click(
                                    element_ptr,
                                    idx,
                                    pid,
                                    wid,
                                    &action_clone,
                                    &delivered,
                                    selection_pixel,
                                    &selection_modifiers,
                                    false,
                                )
                                .map(|outcome| (outcome, false))
                            }
                        })
                        .await
                    },
                )
                .await;

                // Drop the wildcard lease + detect window/foreground side-effects.
                let changes = super::finish_window_observation(snapshot, &args).await;

                finish_ax_dispatch(result, &changes.result_suffix()).await
            };
            return self
                .dispatch_resolved(
                    &cursor_key,
                    target,
                    &delivery_receipt,
                    async { None },
                    dispatch,
                )
                .await;
        } else if let (Some(mut cx), Some(mut cy)) = (x, y) {
            // ── Pixel path ─────────────────────────────────────────────────

            // debug_image_out: capture fresh screenshot, overlay crosshair BEFORE
            // any coordinate translation (so it shows received coords in the same
            // space the caller was reasoning in).
            if let Some(ref dbg_path) = debug_image_out {
                if from_zoom {
                    return ToolResult::error(
                        "debug_image_out is incompatible with from_zoom — \
                         received (x, y) would be in zoom-crop space, not window-local.",
                    );
                }
                match window_id {
                    None => return ToolResult::error("debug_image_out requires window_id."),
                    Some(wid) => {
                        // Session-effective max dimension so debug_image_out
                        // matches the resize the calling session sees in
                        // get_window_state (precedence: session override > global).
                        let max_dim = self.state.session_config.effective_max_image_dimension(
                            args.opt_str("_session_id").as_deref(),
                            &self.state.config.read().unwrap(),
                        );
                        let dbg_path_c = dbg_path.clone();
                        let dbg_result = tokio::task::spawn_blocking(move || {
                            let png = crate::capture::screenshot_window_bytes(wid)?;
                            let png = crate::capture::resize_png_if_needed(&png, max_dim)?;
                            crate::capture::write_crosshair_png(&png, cx, cy, &dbg_path_c)
                        })
                        .await;
                        match dbg_result {
                            Err(e) => {
                                return ToolResult::error(format!(
                                    "debug_image_out task failed: {e}. Not dispatching click."
                                ))
                            }
                            Ok(Err(e)) => {
                                return ToolResult::error(format!(
                                    "debug_image_out write failed: {e}. Not dispatching click."
                                ))
                            }
                            Ok(Ok(())) => {}
                        }
                    }
                }
            }

            if from_zoom {
                match self.state.zoom_registry.get(pid) {
                    Some(ctx) => {
                        let (wx, wy) = ctx.zoom_to_window(cx, cy);
                        cx = wx;
                        cy = wy;
                    }
                    None => {
                        return ToolResult::error(format!(
                            "from_zoom=true but no zoom context for pid {pid}. Call zoom first."
                        ))
                    }
                }
            } else if let Some(ratio) = self.state.resize_registry.ratio(pid, window_id) {
                // Coordinates are in the downscaled image space; scale back to native pixels.
                cx *= ratio;
                cy *= ratio;
            }

            // ── Window-local → screen coordinate translation ──────────────────
            // `click_at_xy` accepts screen-space coordinates (top-left origin).
            // Callers supply window-local screenshot pixels; `px_frame` adds the
            // window's screen-origin (and divides out the Retina backing scale)
            // to produce the final screen position, or refuses when the window
            // has no live frame — see px_frame's module docs for why there is
            // no screen-absolute fallback.
            //
            // win_local_x/y: window-local logical-pixel coords needed for
            // CGEventSetWindowLocation in the Chromium recipe.
            let mut approach_frame = None;
            let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
                match super::px_frame::resolve_or_refuse(wid).await {
                    Ok(frame) => {
                        approach_frame = Some(frame.bounds.clone());
                        let (sx, sy, lx, ly) = frame.to_screen(cx, cy);
                        // A window-local point outside the live frame would
                        // dispatch onto whatever occupies that screen point —
                        // the same wrong-surface misclick class as #2237.
                        // Refuse in background, where the caller cannot see
                        // what is actually under the translated point.
                        if !delivery_mode.is_foreground()
                            && (lx < 0.0
                                || ly < 0.0
                                || lx > frame.bounds.width
                                || ly > frame.bounds.height)
                        {
                            return ToolResult::error(format!(
                                "click: window-local point ({lx:.1}, {ly:.1}) pt lies outside \
                                 window {wid}'s {:.0}×{:.0} pt frame; background delivery \
                                 refused. Re-read coordinates from a fresh get_window_state \
                                 screenshot.",
                                frame.bounds.width, frame.bounds.height
                            ));
                        }
                        (sx, sy, lx, ly)
                    }
                    Err(refusal) => return refusal,
                }
            } else {
                // No window_id → treat x,y as screen coordinates (legacy behaviour).
                (cx, cy, cx, cy)
            };

            // ── Exact-target background gate (macOS background input v1) ──
            // A window-addressed background pixel action targets coordinates,
            // which only mean something while the exact window is current and
            // not minimized/hidden: a stale target would let the pid-scoped
            // hit-test or routed events land on a same-process sibling. Gate
            // BEFORE the AX hit-test backend and any cursor/dispatch work.
            // delivery_mode:"foreground" stays the explicit last resort.
            let mutation_lease_held = crate::background_mutation::held_by_current_task(pid);
            let _mutation_lease = if !delivery_mode.is_foreground() && !mutation_lease_held {
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

            // Future construction does not dispatch. The boundary emits intent
            // before polling the hit test, then polls native input only on fallback.
            let delivery_receipt = Arc::new(DeliveryReceipt::default());
            delivery_receipt.validate_pixel_frame(pid, window_id, approach_frame);
            let semantic = async {
                // A background PX action can still use an accessibility delivery
                // backend after resolving the requested screen point. This keeps
                // targeting (PX) orthogonal to delivery (AX) and avoids making a
                // Chromium/AppKit window key merely to satisfy first-mouse rules.
                if !delivery_mode.is_foreground()
                    && window_id.is_some()
                    && button_str == "left"
                    && count == 1
                    && modifiers.is_empty()
                {
                    let focus_only = action == "focus";
                    let hit_test_wid = window_id.expect("guarded by window_id.is_some() above");
                    let receipt = delivery_receipt.clone();
                    let ax_result = tokio::task::spawn_blocking(move || unsafe {
                        receipt.ensure_current()?;
                        let Some(element) = element_at_screen_position(pid, screen_x, screen_y)
                        else {
                            return Ok::<bool, anyhow::Error>(false);
                        };
                        // The pid-scoped hit-test can resolve an element from a
                        // same-process sibling overlapping the requested point.
                        // Require proven ancestry in the requested window before
                        // acting; otherwise fall through to the routed pixel path
                        // (already gated for this exact window).
                        if crate::ax::exact_target::element_window_id(element) != Some(hit_test_wid)
                        {
                            CFRelease(element as _);
                            return Ok(false);
                        }
                        if let Err(error) = receipt.ensure_current() {
                            CFRelease(element as _);
                            return Err(error);
                        }
                        let delivered = if focus_only {
                            crate::input::ax_actions::focus_element(element as usize).is_ok()
                        } else {
                            let press = core_foundation::string::CFString::new("AXPress");
                            AXUIElementPerformAction(element, press.as_concrete_TypeRef())
                                == kAXErrorSuccess
                        };
                        if delivered {
                            receipt.accepted();
                        }
                        CFRelease(element as _);
                        Ok(delivered)
                    })
                    .await;
                    return pixel_ax_dispatch_result(focus_only, ax_result);
                }

                None
            };
            let native = async {
                // Resolve the effective delivery posture before observation. A
                // requested foreground click without a window id still degrades to
                // background, matching the existing contract and result label.
                let fg = delivery_mode.is_foreground() && window_id.is_some();
                let activation_policy =
                    pixel_activation_policy(&button_str, fg, window_id.is_some());

                // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
                // A pixel click can land on a "Sign In" button that opens a sheet
                // or a Safari link that activates a new tab — same side-effect
                // shape as the AX path, so we wrap identically.
                let prior_front = apps::frontmost_pid();
                let snapshot = match activation_policy {
                    PixelActivationPolicy::SuppressTarget => {
                        WindowChangeDetector::snapshot(prior_front)
                    }
                    PixelActivationPolicy::AllowTargetWithoutRaise => {
                        WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
                    }
                    PixelActivationPolicy::ForegroundAssist => {
                        WindowChangeDetector::snapshot_without_suppression(prior_front)
                    }
                };

                // Restore the Swift background-click prologue that was left
                // disconnected in the original Rust port. It makes an opaque
                // target AppKit-active without raising/restacking its window, which
                // is required by Chromium gates and remote-HID proxies such as
                // iPhone Mirroring. Re-pin after the focus record because changing
                // AppKit active state can disturb overlay ordering.
                let focus_without_raise =
                    if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise {
                        let wid = window_id.expect("activation policy requires window_id");
                        match tokio::task::spawn_blocking(move || {
                            crate::input::mouse::prepare_background_pixel_click(pid, wid)
                        })
                        .await
                        {
                            Ok(activated) => {
                                self.visual_sink.send(
                                    &cursor_key,
                                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                                );
                                activated
                            }
                            Err(error) => {
                                return ToolResult::error(format!(
                                    "Background click activation task failed: {error}"
                                ));
                            }
                        }
                    } else {
                        false
                    };

                let mods_owned = modifiers.clone();
                // Surface 5: route to the right/middle CGEvent primitives when
                // button != left. Left-button path stays on the existing Chromium-
                // routed `click_at_xy_with_window_local` for back-compat.
                let button_kind = button_str.clone();
                let receipt = delivery_receipt.clone();
                let result = focus_guard::with_focus_suppressed(
                if activation_policy == PixelActivationPolicy::SuppressTarget {
                    Some(pid)
                } else {
                    None
                },
                prior_front,
                "click.pixel",
                || async move {
                    tokio::task::spawn_blocking(move || {
                        let has_modifiers = !mods_owned.is_empty();
                        let do_click = move || -> anyhow::Result<()> {
                            let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                            if fg && !m.is_empty() {
                                return crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    screen_x,
                                    screen_y,
                                    count,
                                    &button_kind,
                                    &m,
                                );
                            }
                            match button_kind.as_str() {
                                "right" => {
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::right_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, wid, &m,
                                        );
                                    }
                                    crate::input::mouse::right_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                "middle" => {
                                    if let Some(_wid) = window_id {
                                        return crate::input::mouse::middle_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, &m,
                                        );
                                    }
                                    crate::input::mouse::middle_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                // "left" (default) or anything else — preserve legacy left-click path.
                                _ => {
                                    // When we know the window_id, pass the window-local coordinates so
                                    // `click_at_xy_with_window_local` can stamp `CGEventSetWindowLocation`
                                    // and Chromium-specific fields (f40, f51, f58, f91, f92) onto events
                                    // for better backgrounded-target delivery.
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::click_at_xy_with_window_local(
                                            pid, screen_x, screen_y,
                                            win_local_x, win_local_y,
                                            wid, count, &m,
                                            crate::input::mouse::WindowClickDelivery::from_foreground(fg),
                                        );
                                    }
                                    crate::input::mouse::click_at_xy(pid, screen_x, screen_y, count, &m)
                                }
                            }
                        };
                        let do_click = || receipt.dispatch_checked(do_click);
                        // Foreground rung: brief front → click → restore.
                        // Returns whether the window was ACTUALLY fronted, so the
                        // reported `path` honestly reflects the rung that ran.
                        match (fg, window_id, has_modifiers) {
                            (true, Some(wid), true) => {
                                crate::input::skylight::with_foreground_hid_activation(
                                    pid as libc::pid_t,
                                    wid,
                                    do_click,
                                )
                                .map(|_| true)
                            }
                            (true, Some(wid), false) => {
                                crate::input::skylight::with_foreground_assist(
                                    pid as libc::pid_t,
                                    wid,
                                    do_click,
                                )
                            }
                            _ => do_click().map(|_| false),
                        }
                    })
                    .await
                },
            )
            .await;

                // The no-raise record can make NSWorkspace report the target as
                // active even though its window never moved in z-order. Once the
                // click has been queued, restore the prior app if the target is
                // still reported frontmost. Base this on observed state, not
                // `focus_without_raise`: the private recipe can report failure
                // after partially activating the target, and the raw click can
                // self-activate even when that recipe is unavailable. Do not
                // overwrite a different app here; the wildcard suppression lease
                // handles genuine side effects.
                if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise
                    && prior_front != Some(pid)
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if let Some(previous_pid) = background_pixel_restore_pid(
                        activation_policy,
                        prior_front,
                        pid,
                        apps::frontmost_pid(),
                    ) {
                        let _ = apps::activate_pid(previous_pid);
                    }
                }

                let changes = super::finish_window_observation(snapshot, &args).await;

                let button_label = match button_str.as_str() {
                    "right" => "right-click",
                    "middle" => "middle-click",
                    _ => "click",
                };
                match result {
                    Ok(Ok(fronted)) => {
                        // `with_foreground_assist` returns `false` when the fronting SPIs
                        // were unavailable and it clicked WITHOUT activation — report the
                        // background path in that case so `path` reflects the rung that ran.
                        let (path, mode_label) = if fg && fronted {
                            ("cgevent_fg", "foreground CGEvent")
                        } else {
                            ("cgevent", "background CGEvent")
                        };
                        ToolResult::text(format!(
                            "✅ Posted {button_label} to pid {pid} ({mode_label}; \
                         not driver-verified — confirm via screenshot).{}",
                            changes.result_suffix()
                        ))
                        .with_structured(serde_json::json!({
                            "path": path,
                            "verified": false,
                            "effect": "unverifiable",
                            "focus_without_raise": focus_without_raise
                        }))
                    }
                    Ok(Err(e)) => ToolResult::error(format!("{button_label} failed: {e}")),
                    Err(e) => ToolResult::error(format!("Task error: {e}")),
                }
            };
            self.dispatch_resolved(
                &cursor_key,
                Some(ResolvedPointerTarget {
                    x: screen_x,
                    y: screen_y,
                    window_id,
                    element_bounds: None,
                }),
                &delivery_receipt,
                semantic,
                native,
            )
            .await
        } else {
            ToolResult::error(
                "Provide either (element_index + window_id) or (x + y). pid is always required.",
            )
        }
    }
}

fn pixel_ax_dispatch_result(
    focus_only: bool,
    result: Result<anyhow::Result<bool>, tokio::task::JoinError>,
) -> Option<ToolResult> {
    match result {
        Ok(Ok(true)) => {
            let label = if focus_only { "focused" } else { "pressed" };
            Some(
                ToolResult::text(format!(
                    "✅ PX hit-test {label} the background element via AX."
                ))
                .with_structured(serde_json::json!({
                    "path": "ax",
                    "verified": false,
                    "effect": "unverifiable"
                })),
            )
        }
        Ok(Ok(false)) if focus_only => Some(
            ToolResult::error(
                "Background PX focus is unavailable at the requested point.".to_owned(),
            )
            .with_structured(serde_json::json!({
                "code": "background_unavailable"
            })),
        ),
        Ok(Err(error)) if focus_only => Some(
            ToolResult::error(format!("Background PX focus failed: {error}")).with_structured(
                serde_json::json!({
                    "code": "background_unavailable"
                }),
            ),
        ),
        _ => None,
    }
}

type AxClickOutcome = (String, bool, bool, bool, bool);

// All AX successes, including selection early returns, use this result path.
async fn finish_ax_dispatch(
    result: Result<anyhow::Result<(AxClickOutcome, bool)>, tokio::task::JoinError>,
    suffix: &str,
) -> ToolResult {
    match result {
        Ok(Ok((
            (mut msg, needs_webkit_delay, suspected_noop, selection_verified, selection_via_pixel),
            fronted,
        ))) => {
            // For text inputs, wait 800ms for WebKit DOM focus to settle
            // before returning — matches the Swift reference behaviour.
            if needs_webkit_delay {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            msg.push_str(suffix);
            // AX dispatch went through, but AXPerformAction returning
            // success does not confirm the on-screen effect (many elements
            // no-op silently). A click is never driver-verifiable (no
            // read-back) → verified:false stays for back-compat. The
            // tri-state `effect` is the richer signal:
            //   * suspected_noop — the element didn't advertise the action,
            //     so the press likely did nothing → cross to vision/pixel.
            //   * unverifiable — dispatched fine, driver just can't confirm;
            //     the caller verifies via screenshot.
            let mut structured = serde_json::json!({
                "path": if selection_via_pixel {
                    if fronted { "cgevent_fg" } else { "cgevent" }
                } else if fronted {
                    "ax_fg"
                } else {
                    "ax"
                },
                "verified": selection_verified,
                "effect": if selection_verified {
                    "confirmed"
                } else if suspected_noop {
                    "suspected_noop"
                } else {
                    "unverifiable"
                },
            });
            if selection_verified {
                structured["evidence"] = serde_json::json!([
                    { "kind": "accessibility_readback" }
                ]);
            }
            if suspected_noop {
                structured["escalation"] = serde_json::json!({
                    "recommended": "px",
                    "reason": "element does not advertise this action — the \
                               AX press likely no-op'd. Do an element px \
                               action: click by pixel (x,y) off the \
                               screenshot from get_window_state."
                });
            }
            ToolResult::text(msg).with_structured(structured)
        }
        Ok(Err(e)) => ToolResult::error(format!("AX action failed: {e}")),
        Err(e) => ToolResult::error(format!("Task error: {e}")),
    }
}

// ── AX click implementation (blocking) ───────────────────────────────────────

/// Returns `(summary_text, needs_webkit_delay, suspected_noop,
/// selection_verified, selection_via_pixel)`.
///
/// `suspected_noop` is true when the element did not advertise the action we
/// dispatched — AXUIElementPerformAction returns success regardless, so this is
/// the driver's only signal that the press likely did nothing. The caller turns
/// it into `effect: "suspected_noop"` + an escalation hint so the agent crosses
/// to the vision/pixel path instead of trusting a hollow success.
fn perform_ax_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    action_str: &str,
    delivered: &DeliveryReceipt,
    selection_pixel: Option<SelectionPixelTarget>,
    modifiers: &[String],
    foreground: bool,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    delivered.ensure_current()?;
    let ax_action = map_action(action_str);
    let element = element_ptr as AXUIElementRef;

    // Check the live value immediately before dispatch. Foreground assist can
    // enable menu items that were disabled in the cached snapshot, while a
    // background transition can disable them after that snapshot. macOS may
    // otherwise return success for a disabled action that did nothing.
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, ax_action)?;

    // Capture advertised actions BEFORE dispatching so we can detect silent no-ops
    // (AX returns success even when the element doesn't advertise the action).
    let advertised = unsafe { copy_action_names(element) };

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();

    // A click on an AppKit collection item is frequently represented by a
    // label child or row that does not advertise AXPress. Prefer a bounded,
    // read-back-verified AXSelected write over dispatching a known hollow press
    // or forcing the caller onto a less stable pixel coordinate.
    if ax_action == "AXPress" && !advertised.iter().any(|action| action == ax_action) {
        if modifiers.is_empty() {
            delivered.ensure_current()?;
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr, delivered)
            {
                return Ok((
                    format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\"; \
                         confirmed AXSelected=true."
                    ),
                    false,
                    false,
                    true,
                    false,
                ));
            }
        }

        if let (Some(target), Some(selection)) = (
            selection_pixel,
            crate::input::ax_actions::capture_nearest_container_selection(element_ptr),
        ) {
            let selected_role = selection.role().to_owned();
            let Some((before, _)) = selection.observe() else {
                anyhow::bail!("selection target stopped exposing AXSelected before delivery");
            };
            let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
            delivered.ensure_current()?;
            if foreground && !modifier_refs.is_empty() {
                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                    target.screen_x,
                    target.screen_y,
                    1,
                    "left",
                    &modifier_refs,
                )?;
            } else {
                crate::input::mouse::click_at_xy_with_window_local(
                    pid,
                    target.screen_x,
                    target.screen_y,
                    target.window_x,
                    target.window_y,
                    window_id,
                    1,
                    &modifier_refs,
                    crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
                )?;
            }
            delivered.accepted();
            // AppKit may publish a transient AXSelected transition while the
            // event queue is still resolving the gesture. Let it settle before
            // accepting a candidate, then require the same state to survive a
            // second observation. A modified selection additionally preserves
            // every peer that was selected before delivery.
            std::thread::sleep(SELECTION_READBACK_SETTLE);
            let deadline = std::time::Instant::now() + SELECTION_READBACK_TIMEOUT;
            let mut last_observation = None;
            loop {
                if let Some((after, peers_preserved)) = selection.observe() {
                    last_observation = Some((after, peers_preserved));
                    let verified = selection_readback_confirms(
                        before,
                        after,
                        !modifiers.is_empty(),
                        peers_preserved,
                    );
                    if verified {
                        std::thread::sleep(SELECTION_READBACK_STABILITY);
                        if let Some((stable_after, stable_peers_preserved)) = selection.observe() {
                            last_observation = Some((stable_after, stable_peers_preserved));
                            if stable_after == after
                                && selection_readback_confirms(
                                    before,
                                    stable_after,
                                    !modifiers.is_empty(),
                                    stable_peers_preserved,
                                )
                            {
                                return Ok((
                                    format!(
                                        "✅ Selected nearest {selected_role} for [{idx}] {role} \
                                         \"{title}\"; AX selection write was unavailable, so a \
                                         coordinate click was delivered and confirmed by stable \
                                         AXSelected read-back."
                                    ),
                                    false,
                                    false,
                                    true,
                                    true,
                                ));
                            }
                        }
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(SELECTION_READBACK_POLL);
            }
            if modifiers.is_empty() {
                anyhow::bail!(
                    "coordinate click did not produce a stable AXSelected transition; \
                     last_readback={last_observation:?}; retry after a fresh snapshot"
                );
            }
            anyhow::bail!(
                "foreground modified coordinate click did not produce a stable AXSelected \
                 transition while preserving the prior selection; \
                 before_selected={before}, last_readback={last_observation:?}; \
                 take a fresh snapshot before retrying"
            );
        }
    }

    delivered.ensure_current()?;
    let err = unsafe { crate::ax::bindings::perform_action(element, ax_action) };
    if err != crate::ax::bindings::kAXErrorSuccess {
        // Some collection rows claim a click-like action but Finder returns
        // kAXErrorCannotComplete. Use the same verified selection fallback
        // before surfacing the dispatch error.
        if ax_action == "AXPress" && modifiers.is_empty() {
            delivered.ensure_current()?;
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr, delivered)
            {
                return Ok((
                    format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\" \
                         after AXPress returned {err}; confirmed AXSelected=true."
                    ),
                    false,
                    false,
                    true,
                    false,
                ));
            }
        }
        anyhow::bail!("AXUIElementPerformAction({ax_action}) returned {err}");
    }

    delivered.accepted();
    let mut summary = format!("✅ Performed {ax_action} on [{idx}] {role} \"{title}\".");

    // AXPopUpButton: list available options, redirect to set_value.
    if role == "AXPopUpButton" {
        let children = unsafe { copy_children(element) };
        if !children.is_empty() {
            let options: Vec<String> = children
                .iter()
                .filter_map(|&child| {
                    let t = unsafe { copy_string_attr(child, "AXTitle") }.unwrap_or_default();
                    let v = unsafe { copy_string_attr(child, "AXValue") }.unwrap_or_default();
                    if t.is_empty() && v.is_empty() {
                        return None;
                    }
                    Some(if v.is_empty() || v == t {
                        format!("\"{t}\"")
                    } else {
                        format!("\"{t}\" (value: {v})")
                    })
                })
                .collect();
            for &child in &children {
                unsafe {
                    CFRelease(child as _);
                }
            }

            if !options.is_empty() {
                let opt_list = options.join(", ");
                summary.push_str(
                    "\n\n⚠️ This is a popup/select button. The native macOS menu closes \
                     immediately when the window is in the background. Do NOT use click \
                     again — instead, use:\n  set_value(pid, window_id, element_index, value)\n\
                     Available options: [",
                );
                summary.push_str(&opt_list);
                summary.push(']');
            }
        }
    }

    // Advertised-action warning: non-fatal but surfaces likely no-ops. Also the
    // machine-readable `suspected_noop` signal returned to the caller.
    let suspected_noop = !advertised.contains(&ax_action.to_string());
    if suspected_noop {
        let adv_list = if advertised.is_empty() {
            "none".into()
        } else {
            advertised.join(", ")
        };
        summary.push_str(&format!(
            "\n⚠️ Element does not advertise {ax_action} (actions: {adv_list}). \
             Action may have been a no-op."
        ));
    }

    // WebKit DOM focus settle: 800 ms for text inputs (returned to async caller).
    let needs_webkit_delay =
        ax_action == "AXPress" && (role == "AXTextField" || role == "AXTextArea");

    Ok((summary, needs_webkit_delay, suspected_noop, false, false))
}

#[cfg(test)]
mod selection_fallback_tests {
    use super::selection_readback_confirms;

    #[test]
    fn plain_click_requires_selected_readback() {
        assert!(selection_readback_confirms(false, true, false, true));
        assert!(selection_readback_confirms(true, true, false, true));
        assert!(!selection_readback_confirms(false, false, false, true));
    }

    #[test]
    fn modified_click_requires_a_transition_and_preserves_prior_selection() {
        assert!(selection_readback_confirms(false, true, true, true));
        assert!(selection_readback_confirms(true, false, true, true));
        assert!(!selection_readback_confirms(true, true, true, true));
        assert!(!selection_readback_confirms(false, true, true, false));
    }
}

fn map_action(action: &str) -> &'static str {
    match action.to_lowercase().as_str() {
        "press" | "click" => "AXPress",
        "show_menu" | "right_click" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        _ => "AXPress",
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cursor::visual::test_support::{Event, RecordingSink};

    #[tokio::test]
    async fn quick_approach_stalled_renderer_does_not_poll_input() {
        let tool = ClickTool::new(Arc::new(ToolState::default()));
        let receipt = DeliveryReceipt::default();
        let dispatched = std::sync::atomic::AtomicBool::new(false);
        let call = tool.dispatch_resolved(
            "quick-approach-stalled-renderer",
            slice_a_target(),
            &receipt,
            async {
                dispatched.store(true, Ordering::SeqCst);
                Some(ToolResult::text("unexpected input"))
            },
            async { panic!("native must remain unpolled") },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        assert!(!dispatched.load(Ordering::SeqCst));
        assert!(!receipt.was_accepted());
    }

    fn slice_a_tool() -> (ClickTool, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let mut tool = ClickTool::new(Arc::new(ToolState::default()));
        tool.visual_sink = sink.clone();
        tool.state
            .cursor_registry
            .update_position("slice-a-first", 5.0, 5.0);
        (tool, sink)
    }

    fn slice_a_target() -> Option<ResolvedPointerTarget> {
        Some(ResolvedPointerTarget {
            x: 320.0,
            y: 240.0,
            window_id: Some(42),
            element_bounds: None,
        })
    }

    fn slice_a_delivered(path: &str, receipt: &DeliveryReceipt) -> ToolResult {
        receipt.accepted();
        ToolResult::text("delivered attempt").with_structured(serde_json::json!({
            "path": path, "verified": false, "effect": "unverifiable"
        }))
    }

    fn slice_a_assert(tool: &ClickTool, sink: &RecordingSink, key: &str, contact: bool) {
        let mut expected = vec![
            Event::Pin(key.into(), 42),
            Event::Target(key.into(), 320.0, 240.0),
        ];
        if contact {
            expected.push(Event::Contact(key.into(), 320.0, 240.0));
        }
        let events = sink.0.lock().unwrap();
        println!("key={key} events={events:?} expected={expected:?}");
        assert_eq!(*events, expected);
        let pos = tool
            .state
            .cursor_registry
            .get(key)
            .unwrap()
            .position
            .unwrap();
        println!("registry key={key} position=({}, {})", pos.x, pos.y);
        assert_eq!((pos.x, pos.y), (320.0, 240.0));
        assert!(tool
            .state
            .cursor_registry
            .get("default")
            .unwrap()
            .position
            .is_none());
    }

    #[tokio::test]
    async fn slice_a_unaccepted_success_result_is_not_input_delivery() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async { None },
                async { ToolResult::text("no input was needed") },
            )
            .await;
        assert_ne!(result.is_error, Some(true));
        slice_a_assert(&tool, &sink, "slice-a-first", false);
    }

    #[tokio::test]
    async fn slice_a_contact_precedes_delayed_failed_readback() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async { None },
                async {
                    receipt.accepted();
                    // Verification has not returned, but accepted input already has feedback.
                    slice_a_assert(&tool, &sink, "slice-a-first", true);
                    let events = sink.1.lock().unwrap();
                    let contact = events
                        .iter()
                        .find(|event| event.phase == cursor_overlay::VisualPhase::Contact)
                        .unwrap()
                        .clone();
                    let mut core = cursor_overlay::RenderStateCore::new(
                        cursor_overlay::CursorConfig::default(),
                    );
                    let display = cursor_overlay::DisplayBounds {
                        x: 0.0,
                        y: 0.0,
                        width: 1000.0,
                        height: 1000.0,
                    };
                    core.apply_visual_event(
                        contact.clone(),
                        Some(display),
                        contact.timestamp + std::time::Duration::from_millis(75),
                    );
                    assert!((core.contact.unwrap().progress - 0.5).abs() < 1e-9);
                    core.advance_visual_presentation(
                        contact.timestamp + std::time::Duration::from_secs(1),
                    );
                    assert!(
                        core.contact.is_none(),
                        "delayed verification cannot create a fresh click"
                    );
                    ToolResult::error("later verification failed")
                },
            )
            .await;
        assert_eq!(result.is_error, Some(true));
        slice_a_assert(&tool, &sink, "slice-a-first", true);
    }

    #[tokio::test]
    async fn slice_a_pixel_ax_success_skips_native_and_emits_once() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async {
                    slice_a_assert(&tool, &sink, "slice-a-first", false);
                    receipt.accepted();
                    pixel_ax_dispatch_result(false, Ok(Ok(true)))
                },
                async { panic!("native input must not run after AX delivery") },
            )
            .await;
        assert_eq!(result.structured_content.unwrap()["path"], "ax");
        slice_a_assert(&tool, &sink, "slice-a-first", true);
    }

    #[tokio::test]
    async fn slice_a_ax_miss_then_native_does_not_duplicate_intent() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async { None },
                async { slice_a_delivered("cgevent", &receipt) },
            )
            .await;
        assert_eq!(result.structured_content.unwrap()["path"], "cgevent");
        slice_a_assert(&tool, &sink, "slice-a-first", true);
    }

    #[tokio::test]
    async fn slice_a_native_only_pixel_middle_and_desktop_routes_emit_once() {
        for (route, window_id) in [
            ("pixel", Some(42)),
            ("element-middle", Some(42)),
            ("desktop", None),
        ] {
            let (tool, sink) = slice_a_tool();
            let receipt = DeliveryReceipt::default();
            let deliveries = std::sync::atomic::AtomicUsize::new(0);
            let mut target = slice_a_target().unwrap();
            target.window_id = window_id;
            let result = tool
                .dispatch_resolved(
                    "slice-a-first",
                    Some(target),
                    &receipt,
                    async { None },
                    async {
                        let events = sink.0.lock().unwrap();
                        assert_eq!(
                            events
                                .iter()
                                .filter(|event| matches!(event, Event::Target(..)))
                                .count(),
                            1
                        );
                        assert!(!events
                            .iter()
                            .any(|event| matches!(event, Event::Contact(..))));
                        drop(events);
                        deliveries.fetch_add(1, Ordering::Relaxed);
                        slice_a_delivered(
                            if route == "desktop" {
                                "cgevent_hid"
                            } else {
                                "cgevent"
                            },
                            &receipt,
                        )
                    },
                )
                .await;
            assert_ne!(result.is_error, Some(true));
            assert_eq!(deliveries.load(Ordering::Relaxed), 1);
            let mut expected = vec![];
            if let Some(wid) = window_id {
                expected.push(Event::Pin("slice-a-first".into(), wid as u64));
            }
            expected.extend([
                Event::Target("slice-a-first".into(), 320.0, 240.0),
                Event::Contact("slice-a-first".into(), 320.0, 240.0),
            ]);
            let events = sink.0.lock().unwrap();
            println!("native route={route} deliveries=1 events={events:?}");
            assert_eq!(*events, expected);
            let position = tool
                .state
                .cursor_registry
                .get("slice-a-first")
                .unwrap()
                .position
                .unwrap();
            assert_eq!((position.x, position.y), (320.0, 240.0));
            assert!(tool
                .state
                .cursor_registry
                .get("default")
                .unwrap()
                .position
                .is_none());
        }
    }

    #[tokio::test]
    async fn slice_a_ax_press_and_selected_early_success_share_feedback() {
        for (selected, pixel) in [(false, false), (true, false), (true, true)] {
            let (tool, sink) = slice_a_tool();
            let receipt = DeliveryReceipt::default();
            let result = tool
                .dispatch_resolved(
                    "slice-a-first",
                    slice_a_target(),
                    &receipt,
                    async { None },
                    async {
                        receipt.accepted();
                        finish_ax_dispatch(
                            Ok(Ok((
                                ("AX outcome".into(), false, false, selected, pixel),
                                false,
                            ))),
                            "",
                        )
                        .await
                    },
                )
                .await;
            let result = result.structured_content.unwrap();
            assert_eq!(result["verified"], selected);
            assert_eq!(result["path"], if pixel { "cgevent" } else { "ax" });
            slice_a_assert(&tool, &sink, "slice-a-first", true);
        }
    }

    #[tokio::test]
    async fn slice_a_failed_delivery_and_focus_refusal_have_no_contact() {
        for semantic_refusal in [false, true] {
            let (tool, sink) = slice_a_tool();
            let receipt = DeliveryReceipt::default();
            let result = tool
                .dispatch_resolved(
                    "slice-a-first",
                    slice_a_target(),
                    &receipt,
                    async {
                        if semantic_refusal {
                            pixel_ax_dispatch_result(true, Ok(Ok(false)))
                        } else {
                            None
                        }
                    },
                    async {
                        assert!(
                            !semantic_refusal,
                            "focus refusal must not dispatch native input"
                        );
                        ToolResult::error("native delivery failed")
                    },
                )
                .await;
            assert_eq!(result.is_error, Some(true));
            slice_a_assert(&tool, &sink, "slice-a-first", false);
        }
    }

    #[tokio::test]
    async fn slice_a_delivered_selection_with_failed_readback_still_has_contact() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async { None },
                async {
                    receipt.accepted();
                    finish_ax_dispatch(
                        Ok(Err(anyhow::anyhow!("selection readback did not stabilize"))),
                        "",
                    )
                    .await
                },
            )
            .await;
        assert_eq!(
            result.is_error,
            Some(true),
            "feedback must not change the tool result"
        );
        slice_a_assert(&tool, &sink, "slice-a-first", true);
    }

    async fn slice_a_selected_write_receipt(advertised_press: bool, readback: Option<bool>) {
        use crate::ax::bindings::test_support::SelectionScope;

        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let fixture = SelectionScope::install(advertised_press, readback, true);
        let observed_sink = sink.clone();
        fixture.before_readback(move || {
            let events = observed_sink.1.lock().unwrap();
            assert_eq!(
                events.last().unwrap().phase,
                cursor_overlay::VisualPhase::Contact,
                "real accepted AX write must publish before entering readback"
            );
        });
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async { None },
                async {
                    // Exercise both real click call sites and the real selection
                    // helper. No test code sets the delivery receipt.
                    let outcome = perform_ax_click(
                        fixture.element_ptr(),
                        0,
                        1,
                        42,
                        "click",
                        &receipt,
                        None,
                        &[],
                        false,
                    );
                    assert_eq!(
                        outcome.as_ref().unwrap_err().to_string(),
                        "AXUIElementPerformAction(AXPress) returned -25200"
                    );
                    finish_ax_dispatch(Ok(outcome.map(|outcome| (outcome, false))), "").await
                },
            )
            .await;
        let expected = if advertised_press {
            vec![
                "failed press",
                "read selected",
                "write selected",
                "read selected",
            ]
        } else {
            vec![
                "read selected",
                "write selected",
                "read selected",
                "read selected",
                "failed press",
                "read selected",
            ]
        };
        println!(
            "advertised_press={advertised_press} readback={readback:?} calls={:?}",
            fixture.calls()
        );
        assert_eq!(fixture.calls(), expected, "no later successful delivery");
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.structured_content.is_none(),
            "no confirmed effect or verification claim"
        );
        slice_a_assert(&tool, &sink, "slice-a-first", true);
        assert!(receipt.was_accepted());
    }

    #[tokio::test]
    async fn slice_a_selected_write_before_press_false_readback_has_contact() {
        slice_a_selected_write_receipt(false, Some(false)).await;
    }

    #[tokio::test]
    async fn slice_a_selected_write_before_press_failed_readback_has_contact() {
        slice_a_selected_write_receipt(false, None).await;
    }

    #[tokio::test]
    async fn slice_a_selected_write_after_press_false_readback_has_contact() {
        slice_a_selected_write_receipt(true, Some(false)).await;
    }

    #[tokio::test]
    async fn slice_a_selected_write_after_press_failed_readback_has_contact() {
        slice_a_selected_write_receipt(true, None).await;
    }

    #[tokio::test]
    async fn slice_a_selected_write_rejected_has_no_contact() {
        use crate::ax::bindings::test_support::SelectionScope;

        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        let fixture = SelectionScope::install(true, None, false);
        let result = tool
            .dispatch_resolved(
                "slice-a-first",
                slice_a_target(),
                &receipt,
                async { None },
                async {
                    let outcome = perform_ax_click(
                        fixture.element_ptr(),
                        0,
                        1,
                        42,
                        "click",
                        &receipt,
                        None,
                        &[],
                        false,
                    );
                    finish_ax_dispatch(Ok(outcome.map(|outcome| (outcome, false))), "").await
                },
            )
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
        assert!(!receipt.was_accepted());
        slice_a_assert(&tool, &sink, "slice-a-first", false);
    }

    #[tokio::test]
    async fn slice_a_pre_dispatch_refusal_does_not_emit_or_move() {
        let (tool, sink) = slice_a_tool();
        let result = tool
            .invoke(serde_json::json!({"x": 1, "y": 2, "session": "slice-a-first"}))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(sink.0.lock().unwrap().is_empty());
        let pos = tool
            .state
            .cursor_registry
            .get("slice-a-first")
            .unwrap()
            .position
            .unwrap();
        assert_eq!((pos.x, pos.y), (5.0, 5.0));
    }

    #[tokio::test]
    async fn slice_a_sessions_keep_target_and_contact_coordinates_separate() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        tool.dispatch_resolved(
            "slice-a-first",
            slice_a_target(),
            &receipt,
            async {
                receipt.accepted();
                pixel_ax_dispatch_result(false, Ok(Ok(true)))
            },
            async { unreachable!() },
        )
        .await;
        let receipt = DeliveryReceipt::default();
        tool.dispatch_resolved(
            "slice-a-second",
            Some(ResolvedPointerTarget {
                x: -80.0,
                y: 700.0,
                window_id: None,
                element_bounds: None,
            }),
            &receipt,
            async { None },
            async { slice_a_delivered("cgevent_hid", &receipt) },
        )
        .await;
        let events = sink.0.lock().unwrap();
        println!("two-session events={events:?}");
        assert_eq!(
            *events,
            vec![
                Event::Pin("slice-a-first".into(), 42),
                Event::Target("slice-a-first".into(), 320.0, 240.0),
                Event::Contact("slice-a-first".into(), 320.0, 240.0),
                Event::Target("slice-a-second".into(), -80.0, 700.0),
                Event::Contact("slice-a-second".into(), -80.0, 700.0),
            ]
        );
        for (key, expected) in [
            ("slice-a-first", (320.0, 240.0)),
            ("slice-a-second", (-80.0, 700.0)),
        ] {
            let pos = tool
                .state
                .cursor_registry
                .get(key)
                .unwrap()
                .position
                .unwrap();
            assert_eq!((pos.x, pos.y), expected);
        }
        assert!(tool
            .state
            .cursor_registry
            .get("default")
            .unwrap()
            .position
            .is_none());
    }

    #[tokio::test]
    async fn slice_a_semantic_without_bounds_does_not_invent_position() {
        let (tool, sink) = slice_a_tool();
        let receipt = DeliveryReceipt::default();
        tool.dispatch_resolved("slice-a-first", None, &receipt, async { None }, async {
            receipt.accepted();
            finish_ax_dispatch(
                Ok(Ok((
                    ("AX selected".into(), false, false, true, false),
                    false,
                ))),
                "",
            )
            .await
        })
        .await;
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![Event::Semantic("slice-a-first".into())]
        );
        let pos = tool
            .state
            .cursor_registry
            .get("slice-a-first")
            .unwrap()
            .position
            .unwrap();
        assert_eq!((pos.x, pos.y), (5.0, 5.0));
    }

    /// Surface 5: schema must advertise the new `button` field with the three
    /// canonical values and default to "left". Hermes / Codex / Claude Code
    /// consumers branch on this enum being present.
    #[test]
    fn schema_advertises_button_enum() {
        let d = def();
        let props = d.input_schema.get("properties").expect("properties");
        let button = props.get("button").expect("button field present");
        let kind = button.get("type").and_then(|v| v.as_str());
        assert_eq!(kind, Some("string"));
        let enum_vals: Vec<&str> = button
            .get("enum")
            .and_then(|v| v.as_array())
            .expect("button.enum present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(enum_vals.contains(&"left"));
        assert!(enum_vals.contains(&"right"));
        assert!(enum_vals.contains(&"middle"));
    }

    /// Surface 5 hard constraint: the tool description must mention the
    /// `button` argument and the "left" default so MCP introspection (which
    /// pipes description into LLM prompts) carries the back-compat note.
    #[test]
    fn description_mentions_button_default() {
        let d = def();
        let desc = d.description.to_ascii_lowercase();
        assert!(
            desc.contains("button"),
            "description should mention button arg"
        );
        assert!(
            desc.contains("left"),
            "description should mention left default"
        );
        assert!(
            desc.contains("middle"),
            "description should mention middle button"
        );
    }

    /// Existing default behaviour preserved: no `button` field on the call →
    /// resolves to "left" inside invoke. We can't drive the AX path without a
    /// live macOS Window Server, but we CAN check the same arg-parsing logic
    /// the invoke uses produces "left" for empty / absent input.
    #[test]
    fn button_defaults_to_left_when_absent() {
        use cua_driver_core::tool_args::ArgsExt;
        let args = serde_json::json!({ "pid": 1234 });
        let button_str_raw = args.str_or("button", "left").to_lowercase();
        let resolved = if button_str_raw.is_empty() {
            "left".to_string()
        } else {
            button_str_raw
        };
        assert_eq!(resolved, "left");
    }

    /// Round-trip the three canonical values through the same parse the invoke
    /// uses, so any future refactor that changes str_or semantics breaks here
    /// before it breaks consumers.
    #[test]
    fn button_round_trips_right_and_middle() {
        use cua_driver_core::tool_args::ArgsExt;
        for v in ["left", "right", "middle"] {
            let args = serde_json::json!({ "pid": 1234, "button": v });
            let s = args.str_or("button", "left").to_lowercase();
            assert_eq!(s, v);
        }
    }

    /// Regression for the Swift→Rust port gap: only a raw background left
    /// click with an exact window may intentionally activate the target
    /// without raising it. Other background buttons retain strict suppression,
    /// and the explicit foreground rung owns its separate activation.
    #[test]
    fn raw_background_left_click_restores_focus_without_raise_policy() {
        assert_eq!(
            pixel_activation_policy("left", false, true),
            PixelActivationPolicy::AllowTargetWithoutRaise
        );
        assert_eq!(
            pixel_activation_policy("left", false, false),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("right", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("middle", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("left", true, true),
            PixelActivationPolicy::ForegroundAssist
        );
    }

    /// The no-foreground contract must not depend on the private activation
    /// recipe reporting full success. If that recipe is unavailable or only
    /// partially succeeds but the target is nevertheless observed frontmost,
    /// restore the user's prior app.
    #[test]
    fn failed_private_activation_still_restores_observed_target_focus() {
        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::AllowTargetWithoutRaise,
                Some(7),
                42,
                Some(42),
            ),
            Some(7)
        );

        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::AllowTargetWithoutRaise,
                Some(7),
                42,
                Some(99),
            ),
            None,
            "do not overwrite an unrelated app that became frontmost"
        );
        assert_eq!(
            background_pixel_restore_pid(
                PixelActivationPolicy::SuppressTarget,
                Some(7),
                42,
                Some(42),
            ),
            None,
            "strict-suppression paths retain their existing ownership"
        );
    }
}
