//! click tool — matches the Swift reference ClickTool.swift.
//!
//! Two addressing modes:
//!
//! * **AX path** (`element_token`): performs AXAction on the cached
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
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_bool_attr, copy_children, copy_element_attr, copy_string_attr,
    element_at_screen_position, element_screen_rect, kAXErrorSuccess, AXUIElementPerformAction,
    AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};

use super::pixel_route::PixelClickRoute;
use super::ToolState;

pub struct ClickTool {
    state: Arc<ToolState>,
}

impl ClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
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

fn selection_pixel_target(
    screen_center: (f64, f64),
    window_origin: (f64, f64),
) -> SelectionPixelTarget {
    SelectionPixelTarget {
        screen_x: screen_center.0,
        screen_y: screen_center.1,
        window_x: screen_center.0 - window_origin.0,
        window_y: screen_center.1 - window_origin.1,
    }
}

/// Resolve the selectable row/item rather than an actionable child such as a
/// selectable NSTextField. A modified click on the child can be consumed as a
/// text interaction without ever reaching the collection's selection model.
fn nearest_selectable_container_center(element_ptr: usize) -> Option<(f64, f64)> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..8 {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if matches!(role.as_str(), "AXRow" | "AXCell" | "AXListItem" | "AXImage")
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            let center = unsafe { element_screen_rect(current) }
                .map(|rect| (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0));
            if owns_current {
                unsafe { CFRelease(current as CFTypeRef) };
            }
            return center;
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        current = parent?;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
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

/// AXPress on a radio button or checkbox that advertises it: a press whose
/// effect the control's own AXValue shows.
fn is_toggle_press(ax_action: &str, role: &str, advertised: &[String]) -> bool {
    ax_action == "AXPress"
        && matches!(role, "AXRadioButton" | "AXCheckBox")
        && advertised.iter().any(|action| action == "AXPress")
}

/// AX errors that AppKit apps return from a press they performed (#3835).
/// Errors that mean the request never reached the app (illegal argument,
/// invalid element, cannot complete, API disabled) are not among them.
fn press_error_may_have_acted(err: crate::ax::bindings::AXError) -> bool {
    use crate::ax::bindings::{
        kAXErrorActionUnsupported, kAXErrorAttributeUnsupported, kAXErrorFailure,
    };
    matches!(
        err,
        kAXErrorFailure | kAXErrorAttributeUnsupported | kAXErrorActionUnsupported
    )
}

/// Whether a toggle's value moving from `before` to `now` is what pressing it
/// does: a checkbox changes state; a radio button becomes selected (a radio
/// turning off was another radio's press).
fn toggle_press_shows(role: &str, before: &str, now: &str) -> bool {
    now != before && (role != "AXRadioButton" || now == "1")
}

/// The value `read` returns when, before `timeout`, it satisfies `shows` and
/// a second read `stability` later agrees on it. Some apps (Finder's toolbar
/// view switcher) apply a press and still return an AX error.
fn settled_value(
    shows: impl Fn(&str) -> bool,
    mut read: impl FnMut() -> Option<String>,
    timeout: std::time::Duration,
    poll: std::time::Duration,
    stability: std::time::Duration,
) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(now) = read().filter(|now| shows(now)) {
            std::thread::sleep(stability);
            if read().as_deref() == Some(now.as_str()) {
                return Some(now);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}

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

/// After a raw background left click (AllowTargetWithoutRaise): once the
/// click has been queued, restore the prior app if the target is still
/// reported frontmost. The no-raise record can make NSWorkspace report the
/// target active although its window never moved in z-order. Based on
/// observed state, not `focus_without_raise`: the private recipe can report
/// failure after partially activating the target, and the raw click can
/// self-activate even when that recipe is unavailable. A different app is
/// never overwritten here; the wildcard suppression lease handles genuine
/// side effects. Blocks about 50 ms.
fn restore_after_background_pixel_click(
    pid: i32,
    window_id: Option<u32>,
    prior_front: Option<i32>,
    focus_without_raise: bool,
) {
    if prior_front == Some(pid) {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    // WindowServer's foreground, not NSWorkspace's cached view.
    let observed_front = if crate::input::skylight::front_pid_matches(pid) == Some(true) {
        Some(pid)
    } else {
        apps::frontmost_pid()
    };
    if let Some(previous_pid) = background_pixel_restore_pid(
        PixelActivationPolicy::AllowTargetWithoutRaise,
        prior_front,
        pid,
        observed_front,
    ) {
        let _ = apps::restore_prior_app(previous_pid);
    } else if let (Some(previous_pid), Some(wid)) = (prior_front, window_id) {
        // The prior app is still frontmost, but the no-raise recipe posted it
        // a defocus record: hand its key window focus back so the user's
        // typing keeps landing there.
        if focus_without_raise && apps::frontmost_pid() == Some(previous_pid) {
            crate::input::skylight::restore_focus_after_without_raise(pid, wid);
        }
    }
}

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "click".into(),
        description:
            "Click an element or point. Prefer element_token from get_window_state; it works \
             on background or hidden windows without moving the pointer. Use x,y in \
             get_window_state screenshot pixels only for surfaces missing from the tree. To \
             choose a pop-up option use set_value. Background by default. \
             Details: skill://cua-driver/WORKFLOW.md"
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
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid":           { "type": "integer", "description": "Target process ID. Optional with element_token." },
                "window_id":     { "type": "integer", "description": "Target window ID; required with x,y, carried by element_token." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "capture_id": { "type": "string", "description": "Capture ID from get_window_state or get_desktop_state that x,y were read from; stale captures are refused." },
                "x":             { "type": "number",  "description": "X in screenshot pixels: get_window_state for a window, get_desktop_state for the desktop." },
                "y":             { "type": "number",  "description": "Y in the same screenshot pixels." },
                "action":        { "type": "string",  "description": "AX action: press (default), show_menu, pick, confirm, cancel, open." },
                "button":        {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "default": "left",
                    "description": "Mouse button. On an element, right opens its menu and middle clicks its center."
                },
                "count":         { "type": "integer", "default": 1, "description": "Click count, pixel path only." },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl. Needs delivery_mode \"foreground\"."
                },
                "from_zoom": {
                    "type": "boolean",
                    "description": "x,y are pixels of the last zoom image for this pid."
                },
                "debug_image_out": {
                    "type": "string",
                    "description": "PNG path; draws a crosshair at x,y on a fresh screenshot. Needs window_id."
                },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "\"background\" (default) acts without raising the window; \"foreground\" briefly fronts it, acts, and restores the prior app. Needs window_id."
                },
                "scope": cua_driver_core::tool_schema::scope_schema()
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
        let capture_id = args.opt_str("capture_id");
        if capture_id.is_some() && !has_xy {
            return ToolResult::error("click.capture_id requires pixel coordinates x and y.")
                .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
        }
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
            let (sx, sy) = if let Some(ref capture_id) = capture_id {
                match self
                    .state
                    .capture_bindings
                    .admit_desktop_click(capture_id, &args, sx_shot, sy_shot)
                {
                    Ok(point) => point,
                    Err(refusal) => return refusal,
                }
            } else {
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
                (sx_shot / desktop_ratio, sy_shot / desktop_ratio)
            };
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
            // Glide the session's agent cursor to the screen point for visibility.
            let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), sx, sy, None).await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, sx, sy);
            // Press edge for the agent-cursor overlay (renders a click pulse on
            // viewers via the cursor hook).
            self.state.cursor_registry.note_press(&cursor_key, sx, sy);

            let btn = button.clone();
            let desktop_modifiers: Vec<String> = args.str_array("modifier");
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                // Desktop scope is explicitly foreground and vision-driven: post
                // at the global HID tap so WindowServer delivers to the window
                // actually visible at this point. PID-posting here would silently
                // turn the foreground contract back into background delivery.
                let modifier_refs: Vec<&str> =
                    desktop_modifiers.iter().map(String::as_str).collect();
                crate::input::mouse::click_at_xy_desktop_with_modifiers(
                    sx,
                    sy,
                    count,
                    &btn,
                    &modifier_refs,
                )
            })
            .await;
            let button_label = match button.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent screen-absolute {button_label} at desktop-pixel \
                     ({sx_shot:.0},{sy_shot:.0}) → screen-point ({sx:.0},{sy:.0}) \
                     (desktop scope; not driver-verified)."
                ))
                .with_structured(serde_json::json!({ "path": "cgevent_hid", "verified": false, "effect": "unverifiable" })),
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            };
        }

        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Resolve this action's cursor key so its click-pulse / glide land on
        // the calling session's cursor, not the shared "default" one.
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        if capture_id.is_some() && element_guard.is_some() {
            return ToolResult::error(
                "click.capture_id is valid only for the pixel x,y path, not element actions.",
            )
            .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
        }
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
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

        // A Chrome page the extension reaches takes the click gestures a
        // browser tool sends exactly (see browser_route::click_next); every
        // other gesture, and the internal focus click of the keyboard tools,
        // keeps native delivery.
        if let Some(next) =
            super::browser_route::click_next(&button_str, count, !modifiers.is_empty(), &action)
        {
            let redirect = match (element_guard.as_ref(), x, y) {
                (Some(guard), _, _) => {
                    super::browser_route::page_input_redirect(
                        "click",
                        next,
                        super::browser_route::Control::Any,
                        pid,
                        window_id,
                        Some(guard.as_ptr() as usize),
                    )
                    .await
                }
                (None, Some(x), Some(y)) if !from_zoom && capture_id.is_none() => {
                    super::browser_route::page_input_redirect_at_pixel(
                        "click",
                        next,
                        super::browser_route::Control::Any,
                        pid,
                        window_id,
                        x,
                        y,
                    )
                    .await
                }
                _ => None,
            };
            if let Some(redirect) = redirect {
                return redirect;
            }
        }

        if let (Some(idx), Some(wid), Some(element_guard)) =
            (element_index, window_id, element_guard)
        {
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
            // The row this token named in the snapshot. A Catalyst list can
            // hand the same accessibility element to another row between the
            // snapshot and the click (a Stocks row token pressed Home Depot
            // instead of Apple); a click must not land on that other row.
            let snapshot_row = self
                .state
                .snapshots
                .with_latest_payload(pid, u64::from(wid), |payload| {
                    payload.rows.indexed.get(&idx).map(|(_, signature)| signature.clone())
                })
                .flatten();
            // Match the requested action against what the element advertises
            // now, before any cursor or input work: an unknown name refuses.
            let action_guard = element_guard.clone();
            let requested = effective_action.clone();
            let press_row = snapshot_row.clone();
            let resolved_action = tokio::task::spawn_blocking(move || unsafe {
                let element = action_guard.as_ptr() as AXUIElementRef;
                let now_reads = ["AXDescription", "AXTitle"]
                    .into_iter()
                    .filter_map(|attribute| copy_string_attr(element, attribute))
                    .find(|text| !still_names_row(snapshot_row.as_deref(), text));
                (now_reads, resolve_element_action(&requested, &copy_action_names(element)))
            })
            .await;
            let ax_action = match resolved_action {
                Ok((Some(now_reads), _)) => return element_changed_refusal(idx, &now_reads),
                Ok((None, Ok(ax_action))) => ax_action,
                Ok((None, Err(advertised))) => {
                    return unknown_action_refusal(&effective_action, &advertised)
                }
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };

            // Animate cursor to element center BEFORE firing AX action,
            // mirroring Swift's `performElementClick` → `animateAndWait(to:)`.
            let center_guard = element_guard.clone();
            // The element's screen rect rides along on the glide so motion
            // styles can time by target size and highlight the target.
            let (center, target_rect) = tokio::task::spawn_blocking(move || unsafe {
                let el = center_guard.as_ptr() as AXUIElementRef;
                (
                    crate::ax::bindings::element_screen_center(el),
                    element_screen_rect(el),
                )
            })
            .await
            .unwrap_or((None, None));

            // Surface 5: button=middle on the AX path has no AX equivalent.
            // Fall back to a pixel middle-click at the element's screen-space center
            // so the request still produces a real middle-button event (browser tab
            // close, autoscroll, etc.). If we can't resolve a center, error rather
            // than silently degrade to AXPress.
            if button_str == "middle" {
                let (cx, cy) = match center {
                    Some(c) => c,
                    None => {
                        return ToolResult::error(
                            "click(button=middle) on element_token: could not resolve element \
                         center for the pixel-middle-click fallback. Pass x, y directly.",
                        )
                    }
                };
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to_target(
                    cursor_key.clone(),
                    cx,
                    cy,
                    Some(wid as u64),
                    target_rect,
                )
                .await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);
                self.state.cursor_registry.note_press(&cursor_key, cx, cy);

                let mods_owned = modifiers.clone();
                let foreground = delivery_mode.is_foreground();
                let result = tokio::task::spawn_blocking(move || {
                    let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                    if foreground && !m.is_empty() {
                        crate::input::skylight::with_foreground_pointer_activation(
                            pid as libc::pid_t,
                            wid,
                            (cx, cy),
                            || {
                                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    cx, cy, 1, "middle", &m,
                                )
                            },
                        )
                    } else {
                        crate::input::mouse::middle_click_at_xy(pid, cx, cy, &m)
                    }
                })
                .await;
                return match result {
                    Ok(Ok(())) => ToolResult::text(format!(
                        "✅ Posted middle-click to pid {pid} at element [{idx}] center \
                         (background CGEvent; not driver-verified — confirm via screenshot)."
                    ))
                    .with_structured(serde_json::json!({ "path": "cgevent", "verified": false, "effect": "unverifiable" })),
                    Ok(Err(e)) if super::pixel_route::is_pointer_refusal(&e) => {
                        super::pixel_route::foreground_unavailable("Middle-click", wid, &e)
                    }
                    Ok(Err(e)) => ToolResult::error(format!("Middle-click failed: {e}")),
                    Err(e)     => ToolResult::error(format!("Task error: {e}")),
                };
            }


            // Finder icon/list items can expose a readable AXSelected state
            // while refusing both AXSelected writes and AXPress. Resolve a
            // verified coordinate frame for those collection-like elements so
            // perform_ax_click can cross that one failed semantic rung
            // internally and confirm the result by AX read-back. A text-entry
            // control gets the same fallback at its own center: it has no
            // AXPress, and an AXFocused write can be rejected or clobbered.
            let selection_center = if ax_action == "AXPress" {
                let selection_guard = element_guard.clone();
                tokio::task::spawn_blocking(move || {
                    let ptr = selection_guard.as_ptr();
                    crate::input::ax_actions::RowSelection::capture(ptr)
                        .and_then(|row| row.center())
                        .or_else(|| {
                            crate::input::ax_actions::nearest_container_selection_state(ptr)
                                .and_then(|_| nearest_selectable_container_center(ptr))
                        })
                        .or_else(|| {
                            let role = unsafe {
                                crate::ax::bindings::copy_string_attr(
                                    ptr as crate::ax::bindings::AXUIElementRef,
                                    "AXRole",
                                )
                            }
                            .unwrap_or_default();
                            is_text_entry_role(&role)
                                .then(|| unsafe {
                                    crate::ax::bindings::element_screen_center(
                                        ptr as crate::ax::bindings::AXUIElementRef,
                                    )
                                })
                                .flatten()
                        })
                })
                .await
                .unwrap_or(None)
            } else {
                None
            };
            let mut selection_pixel = if let Some(screen_center) = selection_center {
                super::px_frame::resolve_or_refuse(wid)
                    .await
                    .ok()
                    .map(|frame| {
                        selection_pixel_target(screen_center, (frame.bounds.x, frame.bounds.y))
                    })
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

            // Pin the overlay and glide the cursor to the element before the
            // AX action (Swift's animateAndWait), keeping the registry truthful
            // for AX-dispatched clicks. When the selection target's pointer
            // frame was refused, a refused click must not move the public
            // cursor: move it only after the action succeeds. Adapted from
            // 1d958bf45 (#3631).
            let position_cursor = || async {
                if let Some((cx, cy)) = center {
                    crate::cursor::overlay::send_command(
                        cursor_key.clone(),
                        cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                    );
                    crate::cursor::overlay::animate_cursor_to_target(
                        cursor_key.clone(),
                        cx,
                        cy,
                        Some(wid as u64),
                        target_rect,
                    )
                    .await;
                    self.state
                        .cursor_registry
                        .update_position(&cursor_key, cx, cy);
                    self.state.cursor_registry.note_press(&cursor_key, cx, cy);
                }
            };
            let defer_cursor_position = selection_center.is_some() && selection_pixel.is_none();
            if !defer_cursor_position {
                position_cursor().await;
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
            let action_clone = ax_action.clone();
            // Thread the resolved session cursor key into the blocking AX path
            // so its ShowFocusRect + ClickPulse land on THIS session's cursor,
            // not the shared "default" one (which would light the wrong cursor
            // and stomp default for a non-default session).
            let ck = cursor_key.clone();
            let selection_modifiers = modifiers.clone();
            let result = focus_guard::with_focus_suppressed(
                if foreground { None } else { Some(pid) },
                prior_front,
                "click.AXPress",
                || async move {
                    tokio::task::spawn_blocking(move || {
                        let element_ptr = element_guard.as_ptr();
                        // Menus on screen before the press, so a menu it opens
                        // is tied to this window while the action holds the
                        // input lock (a Catalyst pop-up's menu names no window).
                        let menus_before = crate::outcome::menu_window_ids(pid);
                        let note_menu = |result: anyhow::Result<_>| {
                            if result.is_ok() {
                                // SAFETY: the element guard keeps it retained.
                                unsafe { crate::outcome::note_menu_opened(pid, wid, element_ptr, menus_before.clone()) };
                            }
                            result
                        };
                        note_menu((|| {
                        if foreground {
                            let mut outcome = None;
                            let has_modifiers = !selection_modifiers.is_empty();
                            let action = || {
                                outcome = Some(perform_ax_click(
                                    (element_ptr, idx),
                                    (pid, wid),
                                    &action_clone,
                                    &ck,
                                    selection_pixel,
                                    &selection_modifiers,
                                    foreground,
                                    press_row.as_deref(),
                                    prior_front,
                                )?);
                                std::thread::sleep(std::time::Duration::from_millis(150));
                                Ok(())
                            };
                            let fronted = if has_modifiers {
                                match selection_pixel {
                                    // The modified click is a HID pointer
                                    // event at the item: it must reach the
                                    // target, not a window covering it.
                                    Some(point) => {
                                        crate::input::skylight::with_foreground_pointer_activation(
                                            pid as libc::pid_t,
                                            wid,
                                            (point.screen_x, point.screen_y),
                                            action,
                                        )?
                                    }
                                    None => crate::input::skylight::with_foreground_hid_activation(
                                        pid as libc::pid_t,
                                        wid,
                                        action,
                                    )?,
                                }
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
                                (element_ptr, idx),
                                (pid, wid),
                                &action_clone,
                                &ck,
                                selection_pixel,
                                &selection_modifiers,
                                false,
                                press_row.as_deref(),
                                prior_front,
                            )
                            .map(|outcome| (outcome, false))
                        }
                        })())
                    })
                    .await
                },
            )
            .await;

            // Drop the wildcard lease + detect window/foreground side-effects.
            let changes = super::finish_window_observation(snapshot).await;

            match result {
                Ok(Ok((
                    (
                        mut msg,
                        needs_webkit_delay,
                        suspected_noop,
                        selection_verified,
                        selection_via_pixel,
                    ),
                    fronted,
                ))) => {
                    // For text inputs, wait 800ms for WebKit DOM focus to settle
                    // before returning — matches the Swift reference behaviour.
                    if defer_cursor_position {
                        position_cursor().await;
                    }
                    if needs_webkit_delay {
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                    msg.push_str(&changes.result_suffix());
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
                Ok(Err(e)) if super::pixel_route::is_pointer_refusal(&e) => {
                    super::pixel_route::foreground_unavailable("click", wid, &e)
                }
                Ok(Err(e)) if e.is::<ElementChanged>() => {
                    let changed = e.downcast_ref::<ElementChanged>().expect("checked above");
                    element_changed_refusal(changed.idx, &changed.now_reads)
                }
                Ok(Err(e)) => ToolResult::error(format!("AX action failed: {e}")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            }
        } else if let (Some(mut cx), Some(mut cy)) = (x, y) {
            // ── Pixel path ─────────────────────────────────────────────────

            if capture_id.is_some() && (from_zoom || debug_image_out.is_some()) {
                return ToolResult::error(
                    "click.capture_id is incompatible with from_zoom and debug_image_out; \
                     pass coordinates from the exact source capture directly.",
                )
                .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
            }

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

            if let Some(ref capture_id) = capture_id {
                let wid = match window_id {
                    Some(window_id) => window_id,
                    None => {
                        return ToolResult::error(
                            "window capture_id requires the matching pid and window_id.",
                        )
                        .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
                    }
                };
                match self
                    .state
                    .capture_bindings
                    .admit_window_click(capture_id, &args, pid, wid, cx, cy)
                {
                    Ok((action_x, action_y)) => {
                        cx = action_x;
                        cy = action_y;
                    }
                    Err(refusal) => return refusal,
                }
            } else if from_zoom {
                match super::zoom_context(&self.state, &args, pid, window_id) {
                    Ok(ctx) => {
                        let (wx, wy) = ctx.zoom_to_window(cx, cy);
                        cx = wx;
                        cy = wy;
                    }
                    Err(refusal) => return refusal,
                }
            } else if args
                .get("_window_native_pixels")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                // Driver-derived window pixels (an AX element's center) are
                // already native: no screenshot scale applies, and the caller
                // never had to take one. Public callers cannot set this flag;
                // ingress strips underscore arguments.
            } else {
                let ratio = match super::screenshot_scale(&self.state, &args, pid, window_id) {
                    Ok(ratio) => ratio,
                    Err(refusal) => return refusal,
                };
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
            // win_local_x/y: window-local logical-pixel coords used when the
            // Chromium recipe falls back to public PID posting. Its SkyLight
            // route uses the translated screen coordinates instead.
            let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
                match super::px_frame::resolve_or_refuse(wid).await {
                    Ok(frame) => {
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

            // Pin the overlay above the target window BEFORE animating so
            // the cursor is already sandwiched correctly while it glides in.
            // Both PX deliveries below (AX hit-test and routed events) share
            // this glide, so the cursor shows whichever one lands the click.
            if let Some(wid) = window_id {
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
            }
            // Animate the visual cursor to the click point and wait for it to
            // arrive — mirrors Swift's `AgentCursor.shared.animateAndWait(to:)`.
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y, window_id.map(|wid| wid as u64)).await;
            // Keep the registry in sync with the overlay (see AX path above).
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);
            self.state
                .cursor_registry
                .note_press(&cursor_key, screen_x, screen_y);

            // A background PX action can still use an accessibility delivery
            // backend after resolving the requested screen point. This keeps
            // targeting (PX) orthogonal to delivery (AX) and avoids making a
            // Chromium/AppKit window key merely to satisfy first-mouse rules.
            if let Some(hit_test_wid) = window_id.filter(|_| {
                !delivery_mode.is_foreground()
                    && button_str == "left"
                    && count == 1
                    && modifiers.is_empty()
            }) {
                let focus_only = action == "focus";
                // An AX press needs no activation, so guard it like the AX
                // element path: this route used to return with no focus
                // protection at all (measured: every delayed self-activation
                // of the pressed app kept focus).
                let prior_front = apps::frontmost_pid();
                let snapshot = WindowChangeDetector::snapshot(prior_front);
                let pixel = SelectionPixelTarget {
                    screen_x,
                    screen_y,
                    window_x: win_local_x,
                    window_y: win_local_y,
                };
                // A click on a Catalyst list row is a row selection: the
                // hit-test press is the first rung of the verified row ladder
                // (and a pointer click at this point the next), so the result
                // says selected only when the app reads the row selected.
                let ax_result = focus_guard::with_focus_suppressed(
                    Some(pid),
                    prior_front,
                    "click.pixel_ax",
                    || async move {
                        tokio::task::spawn_blocking(move || unsafe {
                            let Some(element) = element_at_screen_position(pid, screen_x, screen_y) else {
                                return Ok::<PixelHit, anyhow::Error>(PixelHit::Missed);
                            };
                            // The pid-scoped hit-test can resolve an element
                            // from a same-process sibling overlapping the
                            // requested point. Require proven ancestry in the
                            // requested window before acting; otherwise fall
                            // through to the routed pixel path (already gated
                            // for this exact window).
                            if crate::ax::exact_target::element_window_id(element) != Some(hit_test_wid) {
                                CFRelease(element as _);
                                return Ok(PixelHit::Missed);
                            }
                            let row = (!focus_only)
                                .then(|| crate::input::ax_actions::RowSelection::capture_for_hit(element as usize))
                                .flatten()
                                .filter(|row| row.kind == crate::input::ax_actions::RowKind::Catalyst);
                            let outcome = if let Some(row) = row {
                                let presses =
                                    copy_action_names(element).iter().any(|action| action == "AXPress");
                                PixelHit::Row(select_row(
                                    &row,
                                    element,
                                    presses,
                                    RowTarget {
                                        idx: 0,
                                        pid,
                                        window_id: hit_test_wid,
                                        label: format!(
                                            "the {} row \"{}\" at the clicked point",
                                            row.role,
                                            row.name()
                                        ),
                                        snapshot_row: None,
                                        prior_front,
                                    },
                                    Some(pixel),
                                    false,
                                ))
                            } else if focus_only {
                                if crate::input::ax_actions::focus_element(element as usize).is_ok() {
                                    PixelHit::Delivered
                                } else {
                                    PixelHit::Missed
                                }
                            } else {
                                let press = core_foundation::string::CFString::new("AXPress");
                                if AXUIElementPerformAction(element, press.as_concrete_TypeRef())
                                    == kAXErrorSuccess
                                {
                                    PixelHit::Delivered
                                } else {
                                    PixelHit::Missed
                                }
                            };
                            CFRelease(element as _);
                            Ok(outcome)
                        })
                        .await
                    },
                )
                .await;
                match ax_result {
                    Ok(Ok(PixelHit::Delivered)) => {
                        let changes = super::finish_window_observation(snapshot).await;
                        crate::cursor::overlay::send_command(
                            cursor_key.clone(),
                            cursor_overlay::OverlayCommand::ClickPulse {
                                x: screen_x,
                                y: screen_y,
                            },
                        );
                        let label = if focus_only { "focused" } else { "pressed" };
                        return ToolResult::text(format!(
                            "✅ PX hit-test {label} the background element via AX.{}",
                            changes.result_suffix()
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "verified": false,
                            "effect": "unverifiable"
                        }));
                    }
                    Ok(Ok(PixelHit::Row(outcome))) => {
                        let changes = super::finish_window_observation(snapshot).await;
                        crate::cursor::overlay::send_command(
                            cursor_key.clone(),
                            cursor_overlay::OverlayCommand::ClickPulse {
                                x: screen_x,
                                y: screen_y,
                            },
                        );
                        return match outcome {
                            Ok((mut msg, _, _, verified, via_pixel)) => {
                                msg.push_str(&changes.result_suffix());
                                let mut structured = serde_json::json!({
                                    "path": if via_pixel { "cgevent" } else { "ax" },
                                    "verified": verified,
                                    "effect": if verified { "confirmed" } else { "unverifiable" },
                                });
                                if verified {
                                    structured["evidence"] =
                                        serde_json::json!([{ "kind": "accessibility_readback" }]);
                                }
                                ToolResult::text(msg).with_structured(structured)
                            }
                            Err(e) if super::pixel_route::is_pointer_refusal(&e) => {
                                super::pixel_route::foreground_unavailable("click", hit_test_wid, &e)
                            }
                            Err(e) => ToolResult::error(format!("click: {e}")),
                        };
                    }
                    Ok(Ok(PixelHit::Missed)) if focus_only => {
                        return ToolResult::error(
                            "Background PX focus is unavailable at the requested point.".to_owned(),
                        )
                        .with_structured(serde_json::json!({
                            "code": "background_unavailable"
                        }));
                    }
                    Ok(Err(error)) if focus_only => {
                        return ToolResult::error(format!("Background PX focus failed: {error}"))
                            .with_structured(serde_json::json!({
                                "code": "background_unavailable"
                            }));
                    }
                    _ => {}
                }
            }

            // Resolve the effective delivery posture before observation. A
            // requested foreground click without a window id still degrades to
            // background, matching the existing contract and result label.
            let fg = delivery_mode.is_foreground() && window_id.is_some();
            // Background delivery cannot satisfy a toolkit that reads the
            // hardware pointer; refuse before any activation or dispatch.
            let route = match super::pixel_route::resolve(pid, fg, window_id, "mouse_click").await {
                Ok(route) => route,
                Err(refusal) => return refusal,
            };
            let activation_policy = pixel_activation_policy(&button_str, fg, window_id.is_some());

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
                            crate::cursor::overlay::send_command(
                                cursor_key.clone(),
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

            // Pulse only after the activation settle so it visually coincides
            // with the real target click rather than the private focus prelude.
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::ClickPulse {
                    x: screen_x,
                    y: screen_y,
                },
            );

            let mods_owned = modifiers.clone();
            // Surface 5: route to the right/middle CGEvent primitives when
            // button != left. Left-button path stays on the existing Chromium-
            // routed `click_at_xy_with_window_local` for back-compat.
            let button_kind = button_str.clone();
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
                        let do_click = move || -> anyhow::Result<()> {
                            let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                            if route == PixelClickRoute::ForegroundHid {
                                // Warp the hardware pointer to the mapped global
                                // point and post at the HID tap. The pointer stays
                                // at the target, as on Windows and X11, so apps that
                                // read the pointer when handling the event see it.
                                return crate::input::mouse::click_at_xy_desktop_with_modifiers(
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
                                    // When we know the window_id, use the targeted primitive so
                                    // foreground delivery can stamp window-local coordinates and
                                    // background delivery can retain the translated screen point
                                    // while adding Chromium routing fields (f40, f51, f58, f91, f92).
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
                        // Foreground rung: front the exact window → HID click →
                        // restore the prior front process. The HID tap has no
                        // pid addressing, so activation must be proven (the
                        // window is AX-focused) or no input is sent.
                        match (route, window_id) {
                            (PixelClickRoute::ForegroundHid, Some(wid)) => {
                                crate::input::skylight::with_foreground_pointer_activation(
                                    pid as libc::pid_t,
                                    wid,
                                    (screen_x, screen_y),
                                    do_click,
                                )
                            }
                            _ => do_click(),
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
            if activation_policy == PixelActivationPolicy::AllowTargetWithoutRaise {
                let _ = tokio::task::spawn_blocking(move || {
                    restore_after_background_pixel_click(pid, window_id, prior_front, focus_without_raise)
                })
                .await;
            }

            let changes = super::finish_window_observation(snapshot).await;

            let button_label = match button_str.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            match result {
                Ok(Ok(())) => {
                    let target = if route == PixelClickRoute::ForegroundHid {
                        format!("at screen-point ({screen_x:.0},{screen_y:.0}) for pid {pid}")
                    } else {
                        format!("to pid {pid}")
                    };
                    ToolResult::text(format!(
                        "✅ Posted {button_label} {target} ({}).{}",
                        super::pixel_route::delivery_note(route),
                        changes.result_suffix()
                    ))
                    .with_structured(serde_json::json!({
                        "path": super::pixel_route::path_label(route),
                        "verified": false,
                        "effect": "unverifiable",
                        "focus_without_raise": focus_without_raise
                    }))
                }
                Ok(Err(e)) if route == PixelClickRoute::ForegroundHid => {
                    super::pixel_route::foreground_unavailable(
                        button_label,
                        window_id.unwrap_or_default(),
                        &e,
                    )
                }
                Ok(Err(e)) => ToolResult::error(format!("{button_label} failed: {e}")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            }
        } else {
            ToolResult::error("Provide either element_token or (x + y). pid is required for x,y clicks; an element_token carries its own pid.")
        }
    }
}

// ── AX click implementation (blocking) ───────────────────────────────────────

/// Roles whose click gesture is "put the caret here" rather than "activate".
/// None of them advertise `AXPress`.
fn is_text_entry_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField"
            | "AXTextArea"
            | "AXSecureTextField"
            | "AXSearchField"
            | "AXComboBox"
            | "AXDateField"
            | "AXTimeField"
    )
}

/// An `AXFocused` write can be accepted and then clobbered when AppKit
/// installs the window's remembered first responder, so the write is read back
/// rather than trusted.
const FOCUS_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(80);

/// Focus a text control, proving it through the application's own
/// `AXFocusedUIElement`, and escalate to a pointer click at the control's
/// centre when the `AXFocused` write does not stick.
fn focus_text_entry(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    role: &str,
    title: &str,
    pixel: Option<SelectionPixelTarget>,
    foreground: bool,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    let ax_accepted = crate::input::ax_actions::focus_element(element_ptr).is_ok();
    if ax_accepted {
        std::thread::sleep(FOCUS_READBACK_SETTLE);
        if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
            return Ok((
                format!(
                    "✅ Focused [{idx}] {role} \"{title}\": a text control has no AXPress \
                     action, so the click set keyboard focus, confirmed through the \
                     application's own AXFocusedUIElement."
                ),
                false,
                false,
                true,
                false,
            ));
        }
    }

    let Some(target) = pixel else {
        anyhow::bail!(
            "{role} has no AXPress action, the AXFocused write {}, and no resolvable \
             on-window frame was available for a pointer click; take a fresh snapshot and \
             click by pixel",
            if ax_accepted {
                "did not stick"
            } else {
                "was rejected"
            }
        );
    };
    crate::input::mouse::click_at_xy_with_window_local(
        pid,
        target.screen_x,
        target.screen_y,
        target.window_x,
        target.window_y,
        window_id,
        1,
        &[],
        crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
    )?;
    std::thread::sleep(FOCUS_READBACK_SETTLE);
    if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
        return Ok((
            format!(
                "✅ Focused [{idx}] {role} \"{title}\" at its centre ({:.0}, {:.0}): the \
                 AXFocused write did not stick, so a pointer click was delivered and \
                 confirmed through the application's own AXFocusedUIElement.",
                target.screen_x, target.screen_y
            ),
            false,
            false,
            true,
            true,
        ));
    }
    Ok((
        format!(
            "📨 Clicked [{idx}] {role} \"{title}\" at its centre ({:.0}, {:.0}): a text \
             control has no AXPress action, so focus was written and a pointer click \
             delivered, but the application still reports another element focused.",
            target.screen_x, target.screen_y
        ),
        false,
        false,
        false,
        true,
    ))
}

/// Returns `(summary_text, needs_webkit_delay, suspected_noop,
/// selection_verified, selection_via_pixel)`.
///
/// `suspected_noop` is true when the element did not advertise the action we
/// dispatched — AXUIElementPerformAction returns success regardless, so this is
/// the driver's only signal that the press likely did nothing. The caller turns
/// it into `effect: "suspected_noop"` + an escalation hint so the agent crosses
/// to the vision/pixel path instead of trusting a hollow success.
///
/// `element` is the cached AX element pointer and its snapshot index; `window`
/// is the target (pid, window_id).
fn perform_ax_click(
    element: (usize, usize),
    window: (i32, u32),
    ax_action: &str,
    cursor_key: &str,
    selection_pixel: Option<SelectionPixelTarget>,
    modifiers: &[String],
    foreground: bool,
    snapshot_row: Option<&str>,
    prior_front: Option<i32>,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    let (element_ptr, idx) = element;
    let (pid, window_id) = window;
    let element = element_ptr as AXUIElementRef;
    let action_label = crate::ax::tree::display_action_name(ax_action.to_owned());

    // Check the live value immediately before dispatch. Foreground assist can
    // enable menu items that were disabled in the cached snapshot, while a
    // background transition can disable them after that snapshot. macOS may
    // otherwise return success for a disabled action that did nothing.
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, ax_action)?;

    // Capture advertised actions BEFORE dispatching so we can detect silent no-ops
    // (AX returns success even when the element doesn't advertise the action).
    let advertised = unsafe { copy_action_names(element) };

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    // The name the element answers with now (Catalyst rows have only a
    // description), so the result says which row was acted on.
    let title = ["AXTitle", "AXDescription"]
        .into_iter()
        .filter_map(|attribute| unsafe { copy_string_attr(element, attribute) })
        .find(|name| !name.trim().is_empty())
        .unwrap_or_default();
    ensure_names_row(element, idx, snapshot_row)?;

    // A plain click on a list row means "make this the selected row". Prove
    // that from the app's own selection instead of trusting a press: AppKit
    // takes an AX selection write, Catalyst selects on press or pointer (and
    // some Catalyst lists toggle rows into a multi-selection on press).
    if ax_action == "AXPress" && modifiers.is_empty() {
        if let Some(row) = crate::input::ax_actions::RowSelection::capture(element_ptr) {
            return select_row(
                &row,
                element,
                advertised.iter().any(|action| action == "AXPress"),
                RowTarget {
                    idx,
                    pid,
                    window_id,
                    label: format!("[{idx}] {role} \"{title}\""),
                    snapshot_row,
                    prior_front,
                },
                selection_pixel,
                foreground,
            );
        }
    }

    // A click on an AppKit collection item is frequently represented by a
    // label child that does not advertise AXPress. A modified click on it
    // is a pointer click at the item, confirmed by read-back.
    if ax_action == "AXPress" && !advertised.iter().any(|action| action == ax_action) {
        if let (false, Some(target), Some(selection)) = (
            modifiers.is_empty(),
            selection_pixel,
            crate::input::ax_actions::capture_nearest_container_selection(element_ptr),
        ) {
            let selected_role = selection.role().to_owned();
            let Some((before, _)) = selection.observe() else {
                anyhow::bail!("selection target stopped exposing AXSelected before delivery");
            };
            let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
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
            anyhow::bail!(
                "foreground modified coordinate click did not produce a stable AXSelected \
                 transition while preserving the prior selection; \
                 before_selected={before}, last_readback={last_observation:?}; \
                 take a fresh snapshot before retrying"
            );
        }

        // Nothing selectable around this element. A text-entry role has no
        // AXPress to dispatch and never had: the click means "put the caret
        // here", so write AXFocused and prove it through the application's
        // own AXFocusedUIElement instead of pressing an action the control
        // does not have. Ordering matters - the collection-selection
        // fallbacks above keep precedence, so clicking a Finder filename
        // label still selects its row rather than entering rename mode.
        if modifiers.is_empty() && is_text_entry_role(&role) {
            return focus_text_entry(
                element_ptr,
                idx,
                pid,
                window_id,
                &role,
                &title,
                selection_pixel,
                foreground,
            );
        }
    }

    let alive_before = unsafe { crate::ax::bindings::element_is_alive(element) };
    // A toggle's value before the press: some apps (Finder's toolbar view
    // switcher) apply the press and still return an AX error.
    let read_value = || unsafe {
        crate::ax::bindings::copy_stringish_attr(element, "AXValue").map(|value| value.state_value)
    };
    let toggle_before = is_toggle_press(ax_action, &role, &advertised)
        .then(read_value)
        .flatten();
    let err = unsafe { crate::ax::bindings::perform_action(element, ax_action) };
    if crate::ax::bindings::action_replaced_element(err, alive_before, || unsafe {
        crate::ax::bindings::element_gone_after_action(element)
    }) {
        // The action replaced its own element (Finder's AXOpen navigates the
        // window), so the call reported an error although it ran. Report it as
        // performed but unverified; a retry would act twice.
        return Ok((
            format!(
                "✅ Performed {action_label} on [{idx}] {role} \"{title}\"; the element no longer \
                 exists afterwards (the action replaced it; AX returned {err}). Take a fresh \
                 snapshot before acting again: do not retry this action."
            ),
            false,
            false,
            false,
            false,
        ));
    }
    if err != crate::ax::bindings::kAXErrorSuccess {
        if let Some(before) = toggle_before.filter(|_| press_error_may_have_acted(err)) {
            if let Some(now) = settled_value(
                |now| toggle_press_shows(&role, &before, now),
                read_value,
                SELECTION_READBACK_TIMEOUT,
                SELECTION_READBACK_POLL,
                SELECTION_READBACK_STABILITY,
            ) {
                return Ok((
                    format!(
                        "✅ Performed {action_label} on [{idx}] {role} \"{title}\": its value is now \
                         {now} (was {before}), read back twice, although the app returned AX error {err}."
                    ),
                    false,
                    false,
                    true,
                    false,
                ));
            }
        }
        anyhow::bail!("AXUIElementPerformAction({action_label}) returned {err}");
    }

    let mut summary = format!("✅ Performed {action_label} on [{idx}] {role} \"{title}\".");

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
                     again — instead, use:\n  set_value(pid, element_token, value)\n\
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
            "\n⚠️ Element does not advertise {action_label} (actions: {adv_list}). \
             Action may have been a no-op."
        ));
    }

    // WebKit DOM focus settle: 800 ms for text inputs (returned to async caller).
    let needs_webkit_delay =
        ax_action == "AXPress" && (role == "AXTextField" || role == "AXTextArea");

    // Show focus-rect highlight around the element (matches Swift showFocusRect).
    // Also move the cursor to the element center so the glide animation plays.
    if let Some(rect) = unsafe { element_screen_rect(element) } {
        // Drive THIS session's cursor (threaded in via `cursor_key`), matching
        // the keyed glide already played in the invoke body above. The keyed
        // glide already played on the session's cursor in the invoke body.
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ShowFocusRect(Some(rect)),
        );
        // Animate cursor to element center.
        let cx = rect[0] + rect[2] / 2.0;
        let cy = rect[1] + rect[3] / 2.0;
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: cx, y: cy },
        );
    }
    let _ = pid;
    let _ = window_id; // used by caller context

    Ok((summary, needs_webkit_delay, suspected_noop, false, false))
}

#[cfg(test)]
mod action_name_tests {
    use super::resolve_element_action;

    fn stocks_row() -> Vec<String> {
        [
            "AXPress",
            "AXCancel",
            "AXShowMenu",
            "Name:Toggle price change display\nTarget:0x0\nSelector:(null)",
            "Name:Move Down\nTarget:0x0\nSelector:(null)",
        ]
        .map(String::from)
        .to_vec()
    }

    /// Stocks rows advertise UIKit custom actions; the driver pressed the
    /// row for any unknown name and reported success.
    #[test]
    fn custom_actions_match_by_display_name_in_any_case_and_send_the_raw_name() {
        let raw = "Name:Toggle price change display\nTarget:0x0\nSelector:(null)";
        for requested in ["toggle price change display", "Toggle Price Change Display", raw] {
            assert_eq!(resolve_element_action(requested, &stocks_row()).as_deref(), Ok(raw));
        }
        assert_eq!(resolve_element_action("cancel", &stocks_row()).as_deref(), Ok("AXCancel"));
        assert_eq!(resolve_element_action("AXCancel", &stocks_row()).as_deref(), Ok("AXCancel"));
        assert_eq!(resolve_element_action("press", &stocks_row()).as_deref(), Ok("AXPress"));
        // Raw names match exactly only.
        assert!(resolve_element_action(&raw.to_lowercase(), &stocks_row()).is_err());
    }

    /// Names are shown lowercased with Unicode rules; the resolver compared
    /// ASCII case only, so a displayed "öffnen" was refused.
    #[test]
    fn a_displayed_non_ascii_name_resolves_to_its_raw_action() {
        let advertised: Vec<String> = [
            "Name:Öffnen\nTarget:0x0\nSelector:(null)",
            "Name:ÉDITER\nTarget:0x0\nSelector:(null)",
            "AXPress",
        ]
        .map(String::from)
        .to_vec();
        for raw in &advertised {
            let shown = crate::ax::tree::action_key(&crate::ax::tree::display_action_name(raw.clone()));
            assert_eq!(resolve_element_action(&shown, &advertised).as_ref(), Ok(raw), "{shown}");
            assert_eq!(resolve_element_action(raw, &advertised).as_ref(), Ok(raw), "raw {raw:?}");
        }
        assert_eq!(resolve_element_action("öffnen", &advertised).as_deref(), Ok(advertised[0].as_str()));
        assert_eq!(resolve_element_action("ÖFFNEN", &advertised).as_deref(), Ok(advertised[0].as_str()));
        assert_eq!(resolve_element_action("éditer", &advertised).as_deref(), Ok(advertised[1].as_str()));
        assert!(resolve_element_action("offnen", &advertised).is_err());
        let axis = vec!["Name:AXAxis\nTarget:0x0\nSelector:(null)".to_owned()];
        for requested in ["AXIS", "axis", "AXAxis"] {
            assert_eq!(resolve_element_action(requested, &axis).as_ref(), Ok(&axis[0]), "{requested}");
        }
        // An exact raw name is never shadowed by a custom action shown the same way.
        let shadow = vec![
            "Name:AXAXIncrement\nTarget:0x0\nSelector:(null)".to_owned(),
            "AXIncrement".to_owned(),
        ];
        assert_eq!(resolve_element_action("AXIncrement", &shadow).as_deref(), Ok("AXIncrement"));
    }

    #[test]
    fn a_reused_element_no_longer_names_its_snapshot_row() {
        use super::still_names_row;
        let row = r#"- [14] AXGenericElement = "329.40, change" (Apple Inc., AAPL) [actions=[press]]"#;
        assert!(still_names_row(Some(row), "Apple Inc., AAPL"));
        assert!(!still_names_row(Some(row), "The Home Depot, Inc., HD"));
        assert!(still_names_row(Some(row), ""), "no name to compare");
        assert!(still_names_row(None, "anything"), "no snapshot row to compare");
    }

    #[test]
    fn unknown_names_refuse_with_the_advertised_list_and_aliases_keep_working() {
        let advertised = resolve_element_action("frobnicate", &stocks_row()).unwrap_err();
        assert_eq!(
            advertised,
            ["press", "cancel", "show_menu", "Toggle price change display", "Move Down"]
                .map(|name| name.replace("show_menu", "showmenu"))
        );
        // A documented alias is still sent when the element does not list it.
        assert_eq!(resolve_element_action("open", &stocks_row()).as_deref(), Ok("AXOpen"));
        assert_eq!(resolve_element_action("press", &[]).as_deref(), Ok("AXPress"));
    }
}

#[cfg(test)]
mod selection_fallback_tests {
    use super::{selection_pixel_target, selection_readback_confirms};

    #[test]
    fn selection_pixel_uses_container_center_in_both_coordinate_spaces() {
        let target = selection_pixel_target((360.0, 240.0), (100.0, 80.0));
        assert_eq!(target.screen_x, 360.0);
        assert_eq!(target.screen_y, 240.0);
        assert_eq!(target.window_x, 260.0);
        assert_eq!(target.window_y, 160.0);
    }

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

/// The standard AX action a documented `action` alias names.
fn alias_action(action: &str) -> Option<&'static str> {
    Some(match action.to_lowercase().as_str() {
        "press" | "click" => "AXPress",
        "show_menu" | "right_click" | "rightclick" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        _ => return None,
    })
}

/// Resolve a click's `action` against the element's advertised action names
/// (raw, as `AXUIElementCopyActionNames` returns them). An advertised action
/// matches by its raw name exactly, or by its display name in any case, with
/// or without the `AX` prefix (`Toggle price change display`, `increment`).
/// A documented alias whose AX action is not advertised is still sent, as
/// before, and reported as a suspected no-op. Anything else is refused with
/// the advertised display names: the old fallback pressed the element
/// instead and reported success.
fn resolve_element_action(requested: &str, advertised: &[String]) -> Result<String, Vec<String>> {
    let alias = alias_action(requested);
    if let Some(alias) = alias.filter(|alias| advertised.iter().any(|raw| raw == alias)) {
        return Ok(alias.to_owned());
    }
    // The outline shows each name as its action_key, so that exact text
    // (and any other case of it) must resolve.
    // An exact raw name wins over any normalized match.
    let requested_key = crate::ax::tree::action_key(requested);
    let exact = advertised.iter().find(|raw| raw.as_str() == requested);
    let matched = exact.or_else(|| advertised.iter().find(|raw| {
        let display = crate::ax::tree::display_action_name((*raw).clone());
        let display_key = crate::ax::tree::action_key(&display);
        requested.to_lowercase() == display.to_lowercase()
            || requested_key == display_key
            // A bare name that itself starts with "AX" ("AXIS" for "AXAxis").
            || requested.to_lowercase() == display_key
    }));
    match (matched, alias) {
        (Some(raw), _) => Ok(raw.clone()),
        (None, Some(alias)) => Ok(alias.to_owned()),
        (None, None) => Err(advertised
            .iter()
            .map(|raw| {
                let display = crate::ax::tree::display_action_name(raw.clone());
                display.strip_prefix("AX").map_or(display.clone(), str::to_lowercase)
            })
            .collect()),
    }
}

/// Whether an element's live name still matches the row its token named:
/// the snapshot's rendered row contains it. Unknown snapshots and empty
/// names pass; values are not compared (they change on their own).
fn still_names_row(snapshot_row: Option<&str>, live_name: &str) -> bool {
    let name = live_name.trim();
    name.is_empty() || snapshot_row.is_none_or(|row| row.contains(name))
}

/// The element answers with a name its snapshot row did not show.
#[derive(Debug)]
struct ElementChanged {
    idx: usize,
    now_reads: String,
}

impl std::fmt::Display for ElementChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "element [{}] now reads \"{}\"", self.idx, self.now_reads)
    }
}

impl std::error::Error for ElementChanged {}

/// Refuse to act on an element whose live title or description is not in
/// the row its token named (the app reused it for other content).
fn ensure_names_row(element: AXUIElementRef, idx: usize, snapshot_row: Option<&str>) -> anyhow::Result<()> {
    let changed = ["AXDescription", "AXTitle"]
        .into_iter()
        .filter_map(|attribute| unsafe { copy_string_attr(element, attribute) })
        .find(|name| !still_names_row(snapshot_row, name));
    match changed {
        Some(now_reads) => Err(ElementChanged { idx, now_reads }.into()),
        None => Ok(()),
    }
}

fn element_changed_refusal(idx: usize, now_reads: &str) -> ToolResult {
    ToolResult::error(format!(
        "click: element [{idx}] now reads \"{now_reads}\", not what the snapshot showed; \
         the app reused it for other content. Nothing was sent. Take a fresh snapshot."
    ))
    .with_structured(serde_json::json!({
        "code": "element_changed",
        "effect": "refused",
    }))
}

fn unknown_action_refusal(requested: &str, advertised: &[String]) -> ToolResult {
    ToolResult::error(format!(
        "click: the element does not advertise the action \"{requested}\". Advertised: {}.",
        if advertised.is_empty() { "none".to_owned() } else { advertised.join(", ") }
    ))
    .with_structured(serde_json::json!({
        "code": "unknown_action",
        "effect": "refused",
        "advertised_actions": advertised,
    }))
}

/// What the background pixel click's AX hit-test delivery did.
enum PixelHit {
    /// No element of the exact window at the point, or the press/focus
    /// failed: fall through to routed pixel events.
    Missed,
    /// Pressed (or focused) the element at the point; not verified.
    Delivered,
    /// The point is on a Catalyst list row: the verified row ladder ran.
    Row(anyhow::Result<(String, bool, bool, bool, bool)>),
}

/// The clicked element, for row-selection messages and pointer delivery.
struct RowTarget<'a> {
    idx: usize,
    pid: i32,
    window_id: u32,
    /// How results name the click: `[3] AXGroup "Message 312 from Lena"`.
    label: String,
    snapshot_row: Option<&'a str>,
    /// The app in front when the call began, restored after a background
    /// pointer rung (as the pixel dispatcher does).
    prior_front: Option<i32>,
}

const ROW_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
const ROW_READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
const ROW_READBACK_STABILITY: std::time::Duration = std::time::Duration::from_millis(200);

/// Read-backs a row click always gets, however slowly the app answers
/// (Catalyst AX reads can take tens of milliseconds each).
const ROW_READBACK_MIN_POLLS: u32 = 3;

/// What a row read-back after an input shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowReadOutcome {
    /// The row is the only selected row, and stayed so.
    Selected,
    /// A complete read-back shows the row not (exclusively) selected.
    Missing,
    /// The selection could not be read completely, or did not hold still.
    Unknown,
}

/// Wait for `row` to become the only selected row and stay so.
fn row_selection_outcome(row: &crate::input::ax_actions::RowSelection) -> RowReadOutcome {
    std::thread::sleep(ROW_READBACK_SETTLE);
    let deadline = std::time::Instant::now() + ROW_READBACK_TIMEOUT;
    let mut polls = 0;
    loop {
        polls += 1;
        let seen = row.observe();
        if seen.is_some_and(|seen| seen.exclusive()) {
            std::thread::sleep(ROW_READBACK_STABILITY);
            // Selected, then not: something else is moving the selection, so
            // the outcome is unknown rather than proven missing.
            return if row.observe().is_some_and(|seen| seen.exclusive()) {
                RowReadOutcome::Selected
            } else {
                RowReadOutcome::Unknown
            };
        }
        if polls >= ROW_READBACK_MIN_POLLS && std::time::Instant::now() >= deadline {
            return if seen.is_some() { RowReadOutcome::Missing } else { RowReadOutcome::Unknown };
        }
        std::thread::sleep(SELECTION_READBACK_POLL);
    }
}

/// The rungs `select_row` climbs, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowRung {
    /// AX selection write (AppKit and SwiftUI rows).
    AxSelect,
    /// The element's own AXPress.
    Press,
    /// AXPress on the content a click at the row's centre reaches (a
    /// Catalyst row has no AXPress; its content does, and a press there
    /// selects the row as a tap would).
    PressAtCentre,
    /// A pointer click at the row.
    Pointer,
}

/// What one rung's delivery did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RungSend {
    /// Nothing reached the app (the app rejected the write before acting).
    NotSent,
    /// Input may have reached the app.
    Sent,
    /// Input reached the app and replaced the element or its row.
    Replaced,
}

/// How a climb ended.
#[derive(Debug, PartialEq, Eq)]
enum RowLadderEnd {
    Confirmed(RowRung),
    /// Input was sent and its outcome is unknown; nothing further was sent.
    Unverifiable { after: RowRung, replaced: bool },
    /// The clicked element no longer reads as its snapshot row (`now_reads`
    /// is None when its name could not be read). `after` is the last rung
    /// that sent input, if any did.
    Changed { now_reads: Option<String>, after: Option<RowRung> },
    /// Every rung that applied ran, and a complete read-back after the last
    /// one shows the row not selected (or no rung applied).
    NotSelected,
}

/// The app side of a row-selection climb.
trait RowIo {
    /// Whether this rung can run at all (sends nothing).
    fn applies(&mut self, rung: RowRung) -> bool;
    /// A fresh read of the clicked element's name: `Err(Some(now_reads))`
    /// when it no longer names its snapshot row, `Err(None)` when the name
    /// could not be read.
    fn identity(&mut self) -> Result<(), Option<String>>;
    fn send(&mut self, rung: RowRung) -> anyhow::Result<RungSend>;
    fn read_back(&mut self) -> RowReadOutcome;
}

/// Climb the row-selection rungs under one rule: after any input has been
/// delivered, send more input only if a complete read-back proves the
/// intended effect is missing; an unknown outcome returns unverifiable with
/// no further input; every input is preceded by a fresh identity check of
/// the target; and a read-back showing the row selected confirms only if a
/// fresh identity check still matches after it (a Catalyst press can rebuild
/// the list and recycle the retained handles for another item, which then
/// reports the selection).
fn climb_row_ladder(io: &mut dyn RowIo) -> anyhow::Result<RowLadderEnd> {
    let mut last_sent = None;
    for rung in [RowRung::AxSelect, RowRung::Press, RowRung::PressAtCentre, RowRung::Pointer] {
        if !io.applies(rung) {
            continue;
        }
        if let Err(now_reads) = io.identity() {
            return Ok(RowLadderEnd::Changed { now_reads, after: last_sent });
        }
        match io.send(rung)? {
            RungSend::NotSent => continue,
            RungSend::Replaced => {
                return Ok(RowLadderEnd::Unverifiable { after: rung, replaced: true })
            }
            RungSend::Sent => {
                last_sent = Some(rung);
                match io.read_back() {
                    RowReadOutcome::Selected => {
                        return Ok(match io.identity() {
                            Ok(()) => RowLadderEnd::Confirmed(rung),
                            Err(now_reads) => RowLadderEnd::Changed { now_reads, after: Some(rung) },
                        })
                    }
                    RowReadOutcome::Unknown => {
                        return Ok(RowLadderEnd::Unverifiable { after: rung, replaced: false })
                    }
                    RowReadOutcome::Missing => {}
                }
            }
        }
    }
    Ok(RowLadderEnd::NotSelected)
}

/// The live app behind `climb_row_ladder` for one row click.
struct LiveRowIo<'a> {
    row: &'a crate::input::ax_actions::RowSelection,
    element: AXUIElementRef,
    element_presses: bool,
    pid: i32,
    window_id: u32,
    snapshot_row: Option<&'a str>,
    pixel: Option<SelectionPixelTarget>,
    foreground: bool,
    /// The pointer rung had a target where the row still is.
    pointer_possible: bool,
    /// The content the centre-press rung presses (retained).
    centre_target: Option<AXUIElementRef>,
    /// Rungs that sent input, in order.
    sent: Vec<RowRung>,
    prior_front: Option<i32>,
    /// The row's name when the climb began: a row that later reads another
    /// name was recycled for other content (a Catalyst list rebuilt under
    /// the click), whatever the clicked element reads.
    row_name: String,
}

impl Drop for LiveRowIo<'_> {
    fn drop(&mut self) {
        if let Some(target) = self.centre_target.take() {
            unsafe { CFRelease(target as CFTypeRef) };
        }
    }
}

/// The pointer rung is a raw left click into an exact window, so in the
/// background it takes the pixel dispatcher's activate-without-raise recipe.
fn pointer_rung_activates_without_raise(foreground: bool) -> bool {
    pixel_activation_policy("left", foreground, true) == PixelActivationPolicy::AllowTargetWithoutRaise
}

/// Whether the row still reads the name it had when the climb began:
/// `Err(Some(now))` when it reads another (recycled for other content),
/// `Err(None)` when its name cannot be read (no proof it is the same row).
fn same_row_name(at_start: &str, now: Option<String>) -> Result<(), Option<String>> {
    match now {
        Some(now) if now.trim() == at_start.trim() => Ok(()),
        Some(now) => Err(Some(now)),
        None => Err(None),
    }
}

/// AXPress on `element` as one rung's delivery: a stale handle fails before
/// anything happens; a press that rebuilt the list or replaced the element
/// (or the row) is reported as such, since the row's place may now hold
/// something else.
fn press_rung(
    row: &crate::input::ax_actions::RowSelection,
    element: AXUIElementRef,
) -> anyhow::Result<RungSend> {
    use crate::ax::bindings::{kAXErrorInvalidUIElement, kAXErrorSuccess};
    let alive_before = unsafe { crate::ax::bindings::element_is_alive(element) };
    let err = unsafe { crate::ax::bindings::perform_action(element, "AXPress") };
    if err == kAXErrorInvalidUIElement {
        anyhow::bail!("AXUIElementPerformAction(AXPress) returned {err}; take a fresh snapshot");
    }
    let replaced = if err == kAXErrorSuccess {
        !row.readable() || !unsafe { crate::ax::bindings::element_is_alive(element) }
    } else {
        crate::ax::bindings::action_replaced_element(err, alive_before, || unsafe {
            crate::ax::bindings::element_gone_after_action(element)
        })
    };
    // A press that returned an error may still have acted; the read-back
    // decides.
    Ok(if replaced { RungSend::Replaced } else { RungSend::Sent })
}

impl RowIo for LiveRowIo<'_> {
    fn applies(&mut self, rung: RowRung) -> bool {
        match rung {
            RowRung::AxSelect => self.row.kind == crate::input::ax_actions::RowKind::Native,
            RowRung::Press => self.element_presses,
            RowRung::PressAtCentre => {
                if let Some(old) = self.centre_target.take() {
                    unsafe { CFRelease(old as CFTypeRef) };
                }
                self.centre_target = self.row.press_target_at_centre(self.pid).and_then(|target| {
                    // The clicked element itself already had its rung.
                    if unsafe { core_foundation::base::CFEqual(target as CFTypeRef, self.element as CFTypeRef) } != 0 {
                        unsafe { CFRelease(target as CFTypeRef) };
                        None
                    } else {
                        Some(target)
                    }
                });
                self.centre_target.is_some()
            }
            // The point must still lie on the row.
            RowRung::Pointer => {
                self.pointer_possible = self
                    .pixel
                    .is_some_and(|point| self.row.contains_point(point.screen_x, point.screen_y));
                self.pointer_possible
            }
        }
    }

    fn identity(&mut self) -> Result<(), Option<String>> {
        // Unlike ensure_names_row, a name that cannot be read (not merely
        // absent) is no proof of identity.
        for attribute in ["AXDescription", "AXTitle"] {
            match unsafe { crate::ax::bindings::copy_string_attr_checked(self.element, attribute) } {
                Ok(name) if !still_names_row(self.snapshot_row, &name) => return Err(Some(name)),
                Ok(_) => {}
                Err(err)
                    if err == crate::ax::bindings::kAXErrorAttributeUnsupported
                        || err == crate::ax::bindings::kAXErrorNoValue => {}
                Err(_) => return Err(None),
            }
        }
        same_row_name(&self.row_name, self.row.read_name())
    }

    fn send(&mut self, rung: RowRung) -> anyhow::Result<RungSend> {
        let sent = self.send_rung(rung)?;
        if sent != RungSend::NotSent {
            self.sent.push(rung);
        }
        Ok(sent)
    }

    fn read_back(&mut self) -> RowReadOutcome {
        row_selection_outcome(self.row)
    }
}

impl LiveRowIo<'_> {
    fn send_rung(&mut self, rung: RowRung) -> anyhow::Result<RungSend> {
        Ok(match rung {
            RowRung::AxSelect => {
                if crate::input::ax_actions::ax_write_rejected(self.row.select_via_ax()) {
                    RungSend::NotSent
                } else {
                    RungSend::Sent
                }
            }
            RowRung::Press => press_rung(self.row, self.element)?,
            RowRung::PressAtCentre => press_rung(
                self.row,
                self.centre_target.expect("the centre rung applies only with a target"),
            )?,
            RowRung::Pointer => {
                let point = self.pixel.expect("the pointer rung applies only with a target");
                let click = || {
                    crate::input::mouse::click_at_xy_with_window_local(
                        self.pid,
                        point.screen_x,
                        point.screen_y,
                        point.window_x,
                        point.window_y,
                        self.window_id,
                        1,
                        &[],
                        crate::input::mouse::WindowClickDelivery::from_foreground(self.foreground),
                    )
                };
                // A raw background left click, as the pixel dispatcher sends
                // it: the target made AppKit-active without raising (first
                // mouse, inactive windows), then the prior app restored.
                if pointer_rung_activates_without_raise(self.foreground) {
                    let focus_without_raise =
                        crate::input::mouse::prepare_background_pixel_click(self.pid, self.window_id);
                    let sent = click();
                    restore_after_background_pixel_click(
                        self.pid,
                        Some(self.window_id),
                        self.prior_front,
                        focus_without_raise,
                    );
                    sent?;
                } else {
                    click()?;
                }
                RungSend::Sent
            }
        })
    }
}

fn rung_sent_text(rung: RowRung) -> &'static str {
    match rung {
        RowRung::AxSelect => "Requested the AX selection",
        RowRung::Press => "Sent AXPress",
        RowRung::PressAtCentre => "Sent AXPress to the row's content at its centre",
        RowRung::Pointer => "Posted a pointer click at the row",
    }
}

/// Make `row` the exclusive selection and prove it: an AX selection write
/// (AppKit rows), the element's own press, a press on the row's content at
/// its centre (Catalyst), then a pointer click at the row. Success only when
/// the app reports the row selected and no other; the rules for sending more
/// input live on `climb_row_ladder`.
fn select_row(
    row: &crate::input::ax_actions::RowSelection,
    element: AXUIElementRef,
    element_presses: bool,
    target: RowTarget<'_>,
    pixel: Option<SelectionPixelTarget>,
    foreground: bool,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    let RowTarget { idx, pid, window_id, label, snapshot_row, prior_front } = target;
    let row_role = &row.role;
    let mut io = LiveRowIo {
        row,
        element,
        element_presses,
        pid,
        window_id,
        snapshot_row,
        pixel,
        foreground,
        pointer_possible: false,
        centre_target: None,
        sent: Vec::new(),
        prior_front,
        row_name: row.name(),
    };
    let unverifiable = |text: String, rung: RowRung| {
        Ok((text, false, false, false, rung == RowRung::Pointer))
    };
    let end = climb_row_ladder(&mut io)?;
    match end {
        RowLadderEnd::Confirmed(rung) => Ok((
            format!(
                "✅ Selected {row_role} for {label} {}; read back as the only selected row.",
                match rung {
                    RowRung::AxSelect => "through AX selection",
                    RowRung::Press => "with AXPress",
                    RowRung::PressAtCentre => "with AXPress on its content at the row's centre",
                    RowRung::Pointer => "with a pointer click at the row",
                }
            ),
            false,
            false,
            true,
            rung == RowRung::Pointer,
        )),
        RowLadderEnd::Unverifiable { after, replaced: true } => unverifiable(
            format!(
                "{} on {label}; the element or its row was replaced, so the selection cannot \
                 be read back (effect unverifiable). Take a fresh snapshot before acting again: \
                 do not retry this click.",
                rung_sent_text(after)
            ),
            after,
        ),
        RowLadderEnd::Unverifiable { after, replaced: false } => unverifiable(
            format!(
                "{} on {label}; the selection could not be read back completely (effect \
                 unverifiable), so nothing further was sent. Take a fresh snapshot before \
                 acting again: do not retry this click.",
                rung_sent_text(after)
            ),
            after,
        ),
        RowLadderEnd::Changed { now_reads: Some(now_reads), after: None } => {
            Err(ElementChanged { idx, now_reads }.into())
        }
        RowLadderEnd::Changed { now_reads: None, after: None } => anyhow::bail!(
            "the name of {label} could not be read right before input, so it is not \
             confirmed as the row the snapshot named. Nothing was sent; take a fresh snapshot."
        ),
        RowLadderEnd::Changed { now_reads, after: Some(after) } => unverifiable(
            format!(
                "{} on {label}; {}, so nothing further was sent (effect unverifiable). Take a \
                 fresh snapshot before acting again: do not retry this click.",
                rung_sent_text(after),
                match now_reads {
                    Some(now_reads) => format!(
                        "it then read \"{now_reads}\" (element_changed: the app reused it for \
                         other content)"
                    ),
                    None => "its name then could not be read".to_owned(),
                }
            ),
            after,
        ),
        RowLadderEnd::NotSelected => {
            let seen = row.observe();
            anyhow::bail!(
                "row not selected: {label}. {} The app reports {} (want the row selected and \
                 no other). Nothing claimed. {}",
                if io.sent.is_empty() {
                    "No route applied, so nothing was sent.".to_owned()
                } else {
                    format!(
                        "Tried, each read back as not selected: {}.",
                        io.sent.iter().map(|rung| rung_sent_text(*rung)).collect::<Vec<_>>().join("; ")
                    )
                },
                match seen {
                    Some(seen) => format!(
                        "the row {}selected with {} other row(s) selected",
                        if seen.target { "" } else { "not " },
                        seen.others
                    ),
                    None => "no readable selection".to_owned(),
                },
                if foreground {
                    "Take a fresh snapshot: the list may have changed under the click."
                } else {
                    "Next: click the row by x,y with delivery_mode:\"foreground\" (a real pointer \
                     click), then read the selection back."
                }
            )
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted app for `climb_row_ladder`: which rungs apply, what each
    /// identity check reads, what each send does and what each read-back
    /// shows. Every call is logged in order.
    struct ScriptedRow {
        applies: Vec<RowRung>,
        identity: Vec<Result<(), Option<String>>>,
        sends: Vec<RungSend>,
        reads: Vec<RowReadOutcome>,
        log: Vec<String>,
    }

    impl RowIo for ScriptedRow {
        fn applies(&mut self, rung: RowRung) -> bool {
            self.applies.contains(&rung)
        }
        fn identity(&mut self) -> Result<(), Option<String>> {
            self.log.push("identity".into());
            self.identity.remove(0)
        }
        fn send(&mut self, rung: RowRung) -> anyhow::Result<RungSend> {
            self.log.push(format!("send {rung:?}"));
            Ok(self.sends.remove(0))
        }
        fn read_back(&mut self) -> RowReadOutcome {
            let read = self.reads.remove(0);
            self.log.push(format!("read {read:?}"));
            read
        }
    }

    /// The row-selection rule, case by case: more input only after a
    /// complete read-back proves the effect missing, an unknown outcome
    /// stops unverifiable, and every send follows a fresh identity check.
    #[test]
    fn row_ladder_sends_more_input_only_on_proof_of_a_miss() {
        use RowReadOutcome::{Missing, Selected, Unknown};
        use RowRung::{AxSelect, Pointer, Press, PressAtCentre};
        use RungSend::{NotSent, Replaced, Sent};
        let ok = || Ok(());
        let changed = || Err(Some("The Home Depot".to_owned()));
        let unreadable = || Err(None);
        let all = vec![AxSelect, Press, Pointer];
        #[allow(clippy::type_complexity)]
        let cases: Vec<(&str, Vec<RowRung>, Vec<Result<(), Option<String>>>, Vec<RungSend>, Vec<RowReadOutcome>, RowLadderEnd, &[&str])> = vec![
            ("AX selection proven", all.clone(), vec![ok(), ok()], vec![Sent], vec![Selected],
             RowLadderEnd::Confirmed(AxSelect),
             &["identity", "send AxSelect", "read Selected", "identity"]),
            ("AX selection unknown (long list, slow scan): no press, no pointer", all.clone(),
             vec![ok()], vec![Sent], vec![Unknown],
             RowLadderEnd::Unverifiable { after: AxSelect, replaced: false },
             &["identity", "send AxSelect", "read Unknown"]),
            ("AX write rejected, press proven", all.clone(), vec![ok(), ok(), ok()],
             vec![NotSent, Sent], vec![Selected], RowLadderEnd::Confirmed(Press),
             &["identity", "send AxSelect", "identity", "send Press", "read Selected", "identity"]),
            ("press reads selected but the element was recycled for another row", all.clone(),
             vec![ok(), ok(), changed()], vec![NotSent, Sent], vec![Selected],
             RowLadderEnd::Changed { now_reads: Some("The Home Depot".into()), after: Some(Press) },
             &["identity", "send AxSelect", "identity", "send Press", "read Selected", "identity"]),
            ("press missing, pointer proven", vec![Press, Pointer], vec![ok(), ok(), ok()],
             vec![Sent, Sent], vec![Missing, Selected], RowLadderEnd::Confirmed(Pointer),
             &["identity", "send Press", "read Missing", "identity", "send Pointer", "read Selected",
               "identity"]),
            ("press unknown: no pointer", vec![Press, Pointer], vec![ok()], vec![Sent], vec![Unknown],
             RowLadderEnd::Unverifiable { after: Press, replaced: false },
             &["identity", "send Press", "read Unknown"]),
            ("press replaced the row: no pointer", vec![Press, Pointer], vec![ok()], vec![Replaced],
             vec![], RowLadderEnd::Unverifiable { after: Press, replaced: true },
             &["identity", "send Press"]),
            ("press missing, element reused before the pointer: no pointer", vec![Press, Pointer],
             vec![ok(), changed()], vec![Sent], vec![Missing],
             RowLadderEnd::Changed { now_reads: Some("The Home Depot".into()), after: Some(Press) },
             &["identity", "send Press", "read Missing", "identity"]),
            ("element reused before any input", all.clone(), vec![changed()], vec![], vec![],
             RowLadderEnd::Changed { now_reads: Some("The Home Depot".into()), after: None },
             &["identity"]),
            ("press missing, name unreadable before the pointer: no pointer", vec![Press, Pointer],
             vec![ok(), unreadable()], vec![Sent], vec![Missing],
             RowLadderEnd::Changed { now_reads: None, after: Some(Press) },
             &["identity", "send Press", "read Missing", "identity"]),
            ("every rung missing", all.clone(), vec![ok(), ok(), ok()], vec![Sent, Sent, Sent],
             vec![Missing, Missing, Missing], RowLadderEnd::NotSelected,
             &["identity", "send AxSelect", "read Missing", "identity", "send Press",
               "read Missing", "identity", "send Pointer", "read Missing"]),
            // Catalyst row (CatalystSearch, Messages): no AX selection write,
            // no AXPress of its own; its content at the centre takes AXPress.
            ("Catalyst row: centre press proven, no pointer", vec![PressAtCentre, Pointer],
             vec![ok(), ok()], vec![Sent], vec![Selected], RowLadderEnd::Confirmed(PressAtCentre),
             &["identity", "send PressAtCentre", "read Selected", "identity"]),
            ("Catalyst row: centre press missing, pointer proven", vec![PressAtCentre, Pointer],
             vec![ok(), ok(), ok()], vec![Sent, Sent], vec![Missing, Selected],
             RowLadderEnd::Confirmed(Pointer),
             &["identity", "send PressAtCentre", "read Missing", "identity", "send Pointer",
               "read Selected", "identity"]),
            ("Catalyst row: centre press unknown, no pointer", vec![PressAtCentre, Pointer],
             vec![ok()], vec![Sent], vec![Unknown],
             RowLadderEnd::Unverifiable { after: PressAtCentre, replaced: false },
             &["identity", "send PressAtCentre", "read Unknown"]),
            ("own press missing, then the centre press proven", vec![Press, PressAtCentre, Pointer],
             vec![ok(), ok(), ok()], vec![Sent, Sent], vec![Missing, Selected],
             RowLadderEnd::Confirmed(PressAtCentre),
             &["identity", "send Press", "read Missing", "identity", "send PressAtCentre",
               "read Selected", "identity"]),
            ("Catalyst row: both missing, nothing claimed", vec![PressAtCentre, Pointer],
             vec![ok(), ok()], vec![Sent, Sent], vec![Missing, Missing], RowLadderEnd::NotSelected,
             &["identity", "send PressAtCentre", "read Missing", "identity", "send Pointer",
               "read Missing"]),
            ("no rung applies", vec![], vec![], vec![], vec![], RowLadderEnd::NotSelected, &[]),
        ];
        for (name, applies, identity, sends, reads, want, want_log) in cases {
            let mut io = ScriptedRow { applies, identity, sends, reads, log: vec![] };
            let end = climb_row_ladder(&mut io).expect(name);
            assert_eq!(end, want, "{name}");
            assert_eq!(io.log, want_log, "{name}");
            // The rule itself, over the log: each send directly follows an
            // identity check, and a send after a read needs that read Missing.
            for (at, entry) in io.log.iter().enumerate() {
                if entry == "read Selected" {
                    assert_eq!(io.log.get(at + 1).map(String::as_str), Some("identity"),
                               "{name}: confirmed without a fresh identity check");
                }
                if entry.starts_with("send") {
                    assert_eq!(io.log[at - 1], "identity", "{name}: send without identity check");
                    if let Some(read) = io.log[..at].iter().rev().find(|e| e.starts_with("read")) {
                        assert_eq!(read, "read Missing", "{name}: input after {read}");
                    }
                }
            }
        }
    }

    /// A row recycled for other content during the climb (a Catalyst list
    /// rebuilt under a pixel click, whose clicked element has no name of its
    /// own to compare) stops the ladder; an unreadable name proves nothing.
    #[test]
    fn a_recycled_row_is_not_the_same_row() {
        assert_eq!(same_row_name("Message 312 from Lena", Some("Message 312 from Lena".into())), Ok(()));
        assert_eq!(
            same_row_name("Message 312 from Lena", Some("Message 4 from Alex".into())),
            Err(Some("Message 4 from Alex".into()))
        );
        assert_eq!(same_row_name("Message 312 from Lena", None), Err(None));
        // Through the ladder: the centre press reads selected, but the row
        // now names another message, so nothing is confirmed and no pointer
        // click follows.
        let mut io = ScriptedRow {
            applies: vec![RowRung::PressAtCentre, RowRung::Pointer],
            identity: vec![Ok(()), same_row_name("Message 312 from Lena", Some("Message 4 from Alex".into()))],
            sends: vec![RungSend::Sent],
            reads: vec![RowReadOutcome::Selected],
            log: vec![],
        };
        assert_eq!(
            climb_row_ladder(&mut io).unwrap(),
            RowLadderEnd::Changed { now_reads: Some("Message 4 from Alex".into()), after: Some(RowRung::PressAtCentre) }
        );
        // Recycled after a miss: no pointer click on the replacement.
        let mut io = ScriptedRow {
            applies: vec![RowRung::PressAtCentre, RowRung::Pointer],
            identity: vec![Ok(()), same_row_name("Message 312 from Lena", Some("Message 4 from Alex".into()))],
            sends: vec![RungSend::Sent],
            reads: vec![RowReadOutcome::Missing],
            log: vec![],
        };
        assert!(matches!(climb_row_ladder(&mut io).unwrap(), RowLadderEnd::Changed { .. }));
        assert!(!io.log.iter().any(|entry| entry == "send Pointer"));
    }

    /// The background pointer rung activates the target without raising it
    /// (first-mouse and inactive windows ignore a bare routed click), as the
    /// pixel dispatcher does; a foreground rung does not take that recipe.
    #[test]
    fn background_pointer_rung_uses_the_pixel_activation_recipe() {
        assert!(pointer_rung_activates_without_raise(false));
        assert!(!pointer_rung_activates_without_raise(true));
    }

    /// #3835: Finder's toolbar view switcher applies an AXPress and returns
    /// -25205. Only a radio button or checkbox qualifies for the read-back.
    #[test]
    fn toggle_press_is_an_advertised_press_on_a_radio_or_checkbox() {
        let press = vec!["AXPress".to_owned()];
        assert!(is_toggle_press("AXPress", "AXRadioButton", &press));
        assert!(is_toggle_press("AXPress", "AXCheckBox", &press));
        assert!(
            !is_toggle_press("AXPress", "AXButton", &press),
            "a button has no value that shows the press"
        );
        assert!(!is_toggle_press("AXPick", "AXRadioButton", &press));
        assert!(
            !is_toggle_press("AXPress", "AXRadioButton", &[]),
            "AXPress not advertised"
        );
    }

    /// Only errors an app returns after acting qualify; errors that mean the
    /// request never reached the app keep failing (#3835).
    #[test]
    fn only_errors_an_app_returns_after_acting_qualify() {
        assert!(press_error_may_have_acted(-25200));
        assert!(press_error_may_have_acted(-25205));
        assert!(press_error_may_have_acted(-25206));
        for refused in [-25201, -25202, -25204, -25211] {
            assert!(!press_error_may_have_acted(refused), "{refused}");
        }
    }

    /// A checkbox press shows as any state change; a radio press only as the
    /// radio becoming selected.
    #[test]
    fn toggle_press_shows_as_the_value_the_press_produces() {
        assert!(toggle_press_shows("AXCheckBox", "0", "1"));
        assert!(toggle_press_shows("AXCheckBox", "1", "0"));
        assert!(toggle_press_shows("AXCheckBox", "2", "1"), "mixed state");
        assert!(!toggle_press_shows("AXCheckBox", "1", "1"));
        assert!(toggle_press_shows("AXRadioButton", "0", "1"));
        assert!(
            !toggle_press_shows("AXRadioButton", "1", "0"),
            "a radio turning off was another radio's press"
        );
        assert!(!toggle_press_shows("AXRadioButton", "1", "1"));
    }

    /// An erroring toggle press counts only when the value shows the press
    /// and a second read agrees on it.
    #[test]
    fn erroring_toggle_press_counts_only_when_its_value_settles() {
        let zero = std::time::Duration::ZERO;
        let settled = |reads: Vec<Option<&str>>| {
            let mut reads = reads.into_iter().map(|read| read.map(str::to_owned));
            let shows = |now: &str| toggle_press_shows("AXRadioButton", "0", now);
            settled_value(shows, || reads.next().flatten(), zero, zero, zero)
        };
        assert_eq!(settled(vec![Some("1"), Some("1")]), Some("1".to_owned()));
        assert_eq!(settled(vec![Some("1"), Some("0")]), None, "flickered back");
        assert_eq!(settled(vec![Some("1"), None]), None, "second read failed");
        assert_eq!(settled(vec![Some("0")]), None, "never moved");
        assert_eq!(settled(vec![None]), None, "unreadable");
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
        assert_eq!(props["capture_id"]["type"], "string");
    }

    /// Surface 5 hard constraint: MCP introspection must carry the "left"
    /// default so an omitted `button` keeps the legacy left-click behaviour.
    /// The default lives in the schema, where clients read it.
    #[test]
    fn schema_declares_left_button_default() {
        let d = def();
        let button = &d.input_schema["properties"]["button"];
        assert_eq!(button["default"], "left");
        assert!(button["description"]
            .as_str()
            .unwrap()
            .contains("middle"));
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
