//! type_text tool — matches the Swift reference TypeTextTool.swift.
//!
//! Inserts text via `AXSelectedText` attribute write — an atomic single-call
//! insertion at the current cursor position. This is the preferred path for
//! standard Cocoa text views (NSTextField, NSTextView) and is significantly
//! faster than per-keystroke CGEvent synthesis.
//!
//! Web page fields (Safari, Chrome and other WebKit or Chromium pages, not
//! Electron) get character-by-character CGEvent keystrokes instead: a page
//! ignores the AX write. So do Electron inputs that don't implement
//! `kAXSelectedText`, so the caller doesn't need to detect the app type.
//!
//! When the target pid belongs to a terminal emulator (Ghostty,
//! Terminal.app, iTerm2, Alacritty, kitty, WezTerm, Hyper, Warp — see
//! [`crate::terminal::TERMINAL_BUNDLE_IDS`]), the AX path is skipped
//! entirely: terminals expose `AXTextArea` for their grid but the
//! `AXSelectedText` write never reaches the pty, so the tool would
//! report success while the shell sees nothing. We go straight to
//! CGEvent key-event synthesis (`path: "key_events"`).
//!
//! Use `type_text_chars` when you explicitly need per-character pacing
//! (e.g., to trigger live-search debounce handlers).

use async_trait::async_trait;
use cua_driver_contract::TypeTextInput;
use cua_driver_core::{
    protocol::ToolResult,
    text_insertion::{classify_insertion, TextInsertionProgress as TypedProgress, TextSelectionRange},
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_string_attr, focused_element_of_pid, kAXErrorSuccess, set_string_attr, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFType, CFTypeRef, TCFType};
use cua_driver_core::background_input::BackgroundRefusal;

use super::ToolState;

pub struct TypeTextTool {
    pub state: Arc<ToolState>,
}

impl TypeTextTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "type_text".into(),
        description:
            "Insert text at an element (element_token) or the \
             focused element. A web page field works in the background by element_token or \
             x,y: the field is focused and gets key events, Electron's included. A line \
             break into a multi-line page field is sent as Shift+Return (most editors take it \
             as a line break; a page can bind it to send). No other special keys (use \
             press_key). If unverifiable, re-read before retrying to avoid duplicate text. \
             Details: skill://cua-driver/MACOS.md"
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid":  { "type": "integer", "description": "Target process ID." },
                "text": { "type": "string",  "description": "Text to insert at the cursor." },
                "window_id": {
                    "type": "integer",
                    "description": "Target window ID; required with x,y, carried by element_token."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "x": { "type": "number", "description": "X in get_window_state screenshot pixels of the field to click, then type into. Not with element_token." },
                "y": { "type": "number", "description": "Y in the same screenshot pixels." },
                "delay_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 200,
                    "default": 30,
                    "description": "Delay between synthesized characters; unused when accessibility insertion succeeds."
                },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Legacy frame; prefer target. \"desktop\" with no pid/window_id types into the frontmost app." },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "\"background\" (default) types without stealing focus; \"foreground\" briefly fronts the window, types, and restores the prior app, for surfaces where background typing does not land."
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

fn screen_sharing_delivery_error(
    is_screen_sharing: bool,
    foreground: bool,
    window_id: Option<u32>,
) -> Option<ToolResult> {
    if !is_screen_sharing || (foreground && window_id.is_some()) {
        return None;
    }
    Some(
        ToolResult::error(
            "Screen Sharing text input requires delivery_mode:\"foreground\" and window_id \
             so Cua Driver can deliver physical HID key transitions safely.",
        )
        .with_structured(serde_json::json!({
            "code": "SCREEN_SHARING_REQUIRES_FOREGROUND_HID",
            "effect": "refused",
            "escalation": {
                "recommended": "foreground",
                "reason": "Screen Sharing forwards physical keycodes; background PID-routed \
                           Unicode events can corrupt guest text.",
                "requires": ["window_id"]
            }
        })),
    )
}

#[async_trait]
impl Tool for TypeTextTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let deadline = posting_deadline();
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<TypeTextInput>("type_text", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let text =
                cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&input.text)
                    .into_owned();
            let delay_ms = args.u64_or("delay_ms", 30).min(200);
            let char_count = text.chars().count();
            if let Some(refusal) = preflight_before(
                TextDeliveryRoute::DesktopUnicode,
                char_count,
                delay_ms,
                deadline,
            ) {
                return synthesis_refusal_result("hid", &refusal, AxAttempt::NotAttempted);
            }
            let result = tokio::task::spawn_blocking(move || {
                let posted =
                    crate::input::keyboard::type_text_global(&text, delay_ms, Some(deadline))?;
                if posted < char_count {
                    // Let what was posted land before answering.
                    std::thread::sleep(DELIVERY_DRAIN_TIMEOUT);
                }
                Ok::<_, anyhow::Error>(posted)
            })
            .await;
            return match result {
                Ok(Ok(posted)) if posted < char_count => {
                    stopped_at_deadline_result(Some("hid"), posted, char_count)
                }
                Ok(Ok(_)) => {
                    ToolResult::text(format!(
                        "{UNCONFIRMED_PREFIX} the keys were sent to the frontmost desktop \
                         application, which gives no read-back here. {OBSERVE_BEFORE_RETYPING}"
                    ))
                        .with_structured(serde_json::json!({
                            "scope": "desktop",
                            "path": "hid",
                            "effect": "unverifiable"
                        }))
                }
                Ok(Err(error)) => ToolResult::error(format!("desktop type_text failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop type_text task failed: {error}")),
            };
        }
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Strip trailing agent-protocol closing tags — see
        // cua_driver_core::text_sanitize docs for rationale.
        let text = cua_driver_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw)
            .into_owned();
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        let delay_ms = args.u64_or("delay_ms", 30);
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        if let Some(error) = screen_sharing_delivery_error(
            crate::input::keyboard::is_screen_sharing_pid(pid),
            delivery_mode.is_foreground(),
            window_id,
        ) {
            return error;
        }

        // Argument-shape errors are reported before any gating or retained
        // lookups: a malformed call must fail the same way regardless of
        // background-target state.
        let px = args.get("x").and_then(|v| v.as_f64());
        let py = args.get("y").and_then(|v| v.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to type_text, not both.",
            );
        }

        let element_guard = element_guard.zip(element_index);

        // A Chrome page with cua's extension connected takes page input
        // through browser_type; refuse before any input, including the px
        // focus click.
        let redirect = match (element_guard.as_ref(), px, py) {
            (Some((guard, _)), _, _) => {
                super::browser_route::page_input_redirect(
                    "type_text",
                    super::browser_route::TYPE_NEXT,
                    super::browser_route::Control::Text,
                    pid,
                    window_id,
                    Some(guard.as_ptr() as usize),
                )
                .await
            }
            (None, Some(x), Some(y)) if !args.bool_or("from_zoom", false) => {
                super::browser_route::page_input_redirect_at_pixel(
                    "type_text",
                    super::browser_route::TYPE_NEXT,
                    super::browser_route::Control::Text,
                    pid,
                    window_id,
                    x,
                    y,
                )
                .await
            }
            (None, _, _) => {
                super::browser_route::page_input_redirect(
                    "type_text",
                    super::browser_route::TYPE_NEXT,
                    super::browser_route::Control::Text,
                    pid,
                    window_id,
                    None,
                )
                .await
            }
        };
        if let Some(redirect) = redirect {
            return redirect;
        }

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background insert must prove exact delivery
        // before any input — including the px focus click — is sent. The pure
        // core decides once from fresh facts: full keyboard ladder, semantic
        // AX write only (exact element, no CGEvent fallback), or a structured
        // refusal. delivery_mode:"foreground" stays the caller's explicit
        // last resort and is not gated here.
        let (_mutation_lease, keyboard_policy) = match window_id {
            Some(wid) if !delivery_mode.is_foreground() => {
                let gate_element_ptr = element_guard.as_ref().map(|(g, _)| g.as_ptr());
                match background_keyboard_policy(pid, wid, gate_element_ptr).await {
                    Ok((lease, policy)) => (Some(lease), policy),
                    Err(refusal_result) => return refusal_result,
                }
            }
            None if !delivery_mode.is_foreground() => (
                Some(super::acquire_background_mutation(pid).await),
                BackgroundKeyboardPolicy::Allowed,
            ),
            _ => (None, BackgroundKeyboardPolicy::Allowed),
        };

        let background_focus = if delivery_mode.is_foreground() {
            None
        } else {
            match self.state.background_focus(&args, pid) {
                Ok(controller) => Some(controller),
                Err(error) => return error,
            }
        };
        // Preparing an unfocused native field (an AXFocused write on the exact
        // addressed element) is an exact-window mutation like set_value's, not
        // a keyboard rung. The WindowPointer gate proves a visible exact target
        // without the same-pid keyboard ambiguity check, so a competing sibling
        // window that makes typing semantic-only still gets its field editor
        // installed before the AX write; keyboard fallback stays refused.
        let prepare_native_text = match (_mutation_lease.as_ref(), element_guard.as_ref(), window_id) {
            (Some(lease), Some((guard, _)), Some(wid)) => lease
                .gate_again(
                    wid,
                    Some(guard.as_ptr()),
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
                )
                .await
                .is_ok(),
            _ => matches!(keyboard_policy, BackgroundKeyboardPolicy::Allowed),
        };

        // A call that waited past its posting deadline (queued behind other
        // input) sends nothing, not even the focus click.
        if std::time::Instant::now() >= deadline {
            return stopped_at_deadline_result(None, 0, text.chars().count());
        }

        // ── Electron page field by element_token, from behind ──
        // Focus it the way the px form does (a background focus click at its
        // centre), then type with keys into the addressed field, whose focus
        // the key rung proves: Chromium ignores the AX text write (item 42).
        let electron_field = match (element_guard.as_ref(), px, window_id, delivery_mode.is_foreground()) {
            (Some((guard, idx)), None, Some(wid), false)
                if crate::browser::electron_js::ElectronJs::is_electron(pid) =>
            {
                let (guard, idx) = (guard.clone(), *idx);
                tokio::task::spawn_blocking(move || {
                    electron_web_field_center(pid, wid, guard.as_ptr(), idx)
                })
                .await
                .ok()
                .flatten()
            }
            _ => None,
        };
        if let (Some((cx, cy)), Some(wid)) = (electron_field, window_id) {
            if let BackgroundKeyboardPolicy::SemanticOnly(ref refusal) = keyboard_policy {
                return super::background_refusal_result(pid, wid, refusal);
            }
            if let Err(e) = super::focus_by_pixel(
                &self.state,
                pid,
                window_id,
                cx,
                cy,
                false,
                args.opt_str("session"),
                args.opt_str("_session_id"),
                false,
                true,
                _mutation_lease.as_ref(),
                background_focus.as_ref(),
            )
            .await
            {
                return e;
            }
            // The page takes the field's focus once its window is made key
            // for the keys; the key rung proves it then, and again right
            // before the first key (a covered page reports the page itself
            // focused until that moment).
        }
        // The field keeps its identity through delivery: the key rung proves
        // it still holds focus right before the first key, and reads it back.

        // ── px form: focus by pixel-click, then type into the focused element ──
        // Pass x,y (no element_token) for an *element px action*: pixel-click the
        // field to give the Chromium/Electron renderer the real keyboard focus the
        // AX path can't, then fall through to the focused-element type path (which
        // escalates AX → CGEvent and lands once focused). Reuses ClickTool's exact
        // coordinate translation + delivery_mode, so it lands on the same pixel a
        // px-click would.
        let mut page_field: Option<crate::ax::snapshot::RetainedElement> = None;
        if let (Some(cx), Some(cy)) = (px, py) {
            // The px form has no exact element for a semantic-only write; when
            // the keyboard rung is refused, refuse before the focus click too.
            if let BackgroundKeyboardPolicy::SemanticOnly(ref refusal) = keyboard_policy {
                let wid = window_id.expect("gate ran only with window_id");
                return super::background_refusal_result(pid, wid, refusal);
            }
            let from_zoom = args
                .get("from_zoom")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // From behind, a web page text field at the point is addressed
            // like its element_token: the key rung focuses it, proves the
            // focus and types (a pixel focus alone does not reach a
            // background Safari page).
            if let (false, false, Some(wid)) = (delivery_mode.is_foreground(), from_zoom, window_id) {
                let ratio = match super::screenshot_scale(&self.state, &args, pid, window_id) {
                    Ok(ratio) => ratio,
                    Err(refusal) => return refusal,
                };
                page_field = tokio::task::spawn_blocking(move || {
                    web_text_field_at_window_pixel(pid, wid, cx * ratio, cy * ratio)
                })
                .await
                .ok()
                .flatten();
            }
        }
        if let (Some(cx), Some(cy), None) = (px, py, page_field.as_ref()) {
            let from_zoom = args.bool_or("from_zoom", false);
            if let Err(e) = super::focus_by_pixel(
                &self.state,
                pid,
                window_id,
                cx,
                cy,
                delivery_mode.is_foreground(),
                args.opt_str("session"),
                args.opt_str("_session_id"),
                from_zoom,
                false,
                _mutation_lease.as_ref(),
                background_focus.as_ref(),
            )
            .await
            {
                return e;
            }
            // element_index stays None → the type path below writes to the now-
            // focused element via the CGEvent (key_events) rung.
        }
        // An untargeted background type_text into a page: the field the page
        // has focused is addressed the same way, and a page with no field
        // focused gets no keys (a key on the page itself can be a shortcut).
        if let (None, None, None, false, Some(wid)) = (
            element_guard.as_ref(),
            px,
            page_field.as_ref(),
            delivery_mode.is_foreground(),
            window_id,
        ) {
            match tokio::task::spawn_blocking(move || focused_web_field(pid, wid)).await {
                Ok(FocusedWebField::Field(field)) => page_field = Some(field),
                Ok(FocusedWebField::PageOnly) => return web_page_has_no_focused_field_result(),
                _ => {}
            }
        }
        let used_pixel_focus =
            (px.is_some() && py.is_some() && page_field.is_none()) || electron_field.is_some();
        let element_ptr = element_guard
            .as_ref()
            .map(|(g, idx)| (g.as_ptr(), Some(*idx)))
            .or_else(|| page_field.as_ref().map(|g| (g.as_ptr(), None)));

        let electron_background_ax_unsafe = element_ptr.is_some()
            && crate::browser::electron_js::ElectronJs::is_electron(pid)
            && electron_background_ax_ancestry_is_unsafe(classify_target_web_area(
                pid,
                element_ptr,
                window_id,
            ));
        if let Some(refusal) = electron_background_ax_refusal(
            delivery_mode,
            element_ptr.is_some(),
            used_pixel_focus,
            electron_background_ax_unsafe,
        ) {
            let wid = window_id.expect("AX element targets require window_id");
            return super::background_refusal_result(pid, wid, &refusal);
        }

        if let (Some((element, _)), Some(wid)) = (element_guard.as_ref(), window_id) {
            let center_guard = element.clone();
            if let Ok((Some((screen_x, screen_y)), target_rect)) =
                tokio::task::spawn_blocking(move || unsafe {
                    let el = center_guard.as_ptr() as AXUIElementRef;
                    (
                        crate::ax::bindings::element_screen_center(el),
                        crate::ax::bindings::element_screen_rect(el),
                    )
                })
                .await
            {
                let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_cursor_to_target(
                    cursor_key.clone(),
                    screen_x,
                    screen_y,
                    Some(wid as u64),
                    target_rect,
                )
                .await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, screen_x, screen_y);
            }
        }
        // Untargeted text still gives a named session visible feedback: its
        // remembered position, or the window centre on the first action.
        if element_guard.is_none() && !used_pixel_focus {
            super::cursor_tools::position_keyboard_cursor(&self.state, &args, window_id, None)
                .await;
        }
        let text_clone = text.clone();
        let char_count = text.chars().count();

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Typing into a field can trigger autocomplete popovers or
        // Chrome/Safari's "Save Password?" prompt, both of which open
        // helper windows. Wrap so callers see them in the result suffix
        // and the wildcard suppressor catches reflex activations.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        // Terminal-emulator short-circuit: when the target pid belongs
        // to a known terminal (Ghostty / Terminal.app / iTerm2 / …), the
        // AX value-set is silently dropped — see crate::terminal docs.
        // Skip the AX path entirely so the caller never sees the
        // "success but nothing typed" symptom.
        let is_terminal_target = crate::terminal::is_terminal_pid(pid);

        let blocking_policy = keyboard_policy.clone();
        let native_guard = (element_guard.clone(), page_field.clone());
        let delivery_lease = _mutation_lease.clone();
        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "type_text.AXSelectedText",
            || async move {
                tokio::task::spawn_blocking(move || {
                    // The route consumes its handle before returning. Keep one
                    // owner until the delivery lease has been released.
                    let _focus_owner = background_focus.clone();
                    let _delivery_lease = delivery_lease;
                    let _native_guard = native_guard;
                    type_text_blocking(
                        pid,
                        &text_clone,
                        element_ptr,
                        delay_ms,
                        is_terminal_target,
                        KeyboardRoute {
                            background_focus,
                            delivery_mode,
                            window_id,
                            keyboard_policy: blocking_policy,
                            prepare_native_text,
                            deadline,
                            proven_web_field: electron_field.is_some(),
                        },
                    )
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot).await;

        // Unwrap the delivery envelope: a structured refusal means no
        // actuator ran and the caller gets the exact reason.
        let result = match result {
            Ok(Ok(TypeTextDelivery::Refused(refusal))) => {
                let wid = window_id.expect("background refusals require a window target");
                return super::background_refusal_result(pid, wid, &refusal);
            }
            Ok(Ok(TypeTextDelivery::CatalystNeedsFocus)) => {
                return catalyst_text_needs_focus_result(pid, window_id);
            }
            Ok(Err(error)) if error.downcast_ref::<WebFieldNotFocused>().is_some() => {
                let target = retry_target(&args, pid, px, py, window_id);
                return web_field_not_focused_result(
                    target,
                    &text,
                    args.opt_str("session").as_deref(),
                    delivery_mode.is_foreground(),
                );
            }
            Ok(Ok(TypeTextDelivery::SynthesisRefused {
                path,
                refusal,
                ax_attempt,
            })) => return synthesis_refusal_result(path, &refusal, ax_attempt),
            Ok(Ok(TypeTextDelivery::StoppedAtDeadline { path, posted_chars })) => {
                return stopped_at_deadline_result(path, posted_chars, char_count);
            }
            Ok(Ok(TypeTextDelivery::Typed(outcome))) => Ok(Ok(outcome)),
            Ok(Err(error)) => Ok(Err(error)),
            Err(error) => Err(error),
        };

        match result {
            Ok(Ok(outcome))
                if outcome.delivered_chars == Some(0)
                    && outcome.unconfirmed == Some(Unconfirmed::Unchanged)
                    && char_count > 0
                    && !delivery_mode.is_foreground()
                    && catalyst_typing_target(pid, element_ptr.map(|(ptr, _)| ptr), window_id) =>
            {
                catalyst_background_keys_not_landed(pid, window_id, char_count, outcome.path)
            }
            Ok(Ok(outcome)) if outcome.delivered_chars.is_some_and(|n| n < char_count) => {
                let delivered_chars = outcome.delivered_chars.unwrap_or_default();
                ToolResult::error(format!(
                    "type_text incomplete: delivered {delivered_chars} of {char_count} character(s){}; retry only the remaining suffix",
                    outcome.detail
                ))
                .with_structured(serde_json::json!({
                    "code": "type_text_incomplete",
                    "path": outcome.path,
                    "effect": "partial",
                    "requested_chars": char_count,
                    "delivered_chars": delivered_chars,
                    "retryable": true,
                    "retry_from_character": delivered_chars,
                }))
            }
            Ok(Ok(outcome)) => {
                let target_is_web_content = outcome.verified
                    && path_has_untrusted_web_readback(outcome.path)
                    && target_in_web_area(pid, element_ptr, window_id);
                let surface = if !target_is_web_content {
                    WebSurface::Other
                } else if crate::browser::electron_js::ElectronJs::is_electron(pid) {
                    WebSurface::Electron
                } else if is_chromium_browser_pid(pid) {
                    WebSurface::ChromiumBrowser
                } else {
                    WebSurface::Other
                };
                let result = completed_typing_result(
                    outcome,
                    char_count,
                    target_is_web_content,
                    surface,
                    used_pixel_focus,
                    &changes.result_suffix(),
                );
                // A field its app saves only when editing ends (Finder's
                // Get Info, its rename field): the text shows, nothing is
                // saved yet.
                match unsafe { typed_field_pending(pid, element_ptr) } {
                    Some(field) => super::edit_commit::mark_typed_not_committed(
                        result,
                        field,
                        &crate::apps::get_app_name_for_pid(pid).unwrap_or_else(|| "The app".into()),
                    ),
                    None => result,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("type_text failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// The pending-commit kind of the field the text went to: the addressed
/// element, else the app's focused element.
///
/// # Safety
///
/// An addressed element must be retained for the duration of the call.
unsafe fn typed_field_pending(
    pid: i32,
    element: Option<(usize, Option<usize>)>,
) -> Option<super::edit_commit::Field> {
    if !super::edit_commit::app_saves_on_end_editing(pid) {
        return None;
    }
    if let Some((element, _)) = element {
        return super::edit_commit::pending_field_of(pid, element as AXUIElementRef);
    }
    let app = crate::ax::bindings::AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let focused = crate::ax::bindings::copy_element_attr(app, "AXFocusedUIElement");
    core_foundation::base::CFRelease(app as core_foundation::base::CFTypeRef);
    let focused = focused?;
    let field = super::edit_commit::pending_field_of(pid, focused);
    core_foundation::base::CFRelease(focused as core_foundation::base::CFTypeRef);
    field
}

// Response formatting is separate from dispatch so recovery guidance can be
// tested without performing another edit.
fn completed_typing_result(
    outcome: TypeTextOutcome,
    char_count: usize,
    target_is_web_content: bool,
    surface: WebSurface,
    used_pixel_focus: bool,
    suffix: &str,
) -> ToolResult {
    let TypeTextOutcome {
        detail,
        path,
        verified,
        delivered_chars,
        unconfirmed,
    } = outcome;
    // SURFACE-AWARE VERIFICATION. On any web-content surface (Chromium,
    // WebKit, Electron) AXValue is not independent renderer evidence: it can
    // report a changed value after an AX write or synthesized keystrokes while
    // the renderer/DOM observed no edit. A browser's own native chrome stays
    // trusted; only an `AXWebArea` ancestor demotes the read-back.
    let verification = surface_verification(path, verified, target_is_web_content);
    let verified = verification.verified;
    let untrusted_web_readback = verification.untrusted_web_readback;
    let electron_web_content = untrusted_web_readback && surface == WebSurface::Electron;
    let next_rung = untrusted_web_readback
        .then(|| web_readback_next_rung(surface, used_pixel_focus))
        .flatten();

    // The summary's first words state the evidence: "✅ Inserted" only when a
    // trusted read-back shows the text; every other completed outcome starts
    // "⚠️ Not confirmed:" with the missing evidence and the next step. Unknown
    // is not failure: the text may already be there, so the guidance is to
    // observe before typing again, never to retype blind. (Partial delivery
    // and refusals have their own results before this point.)
    let summary = if verified {
        format!("✅ Inserted {char_count} char(s){detail}.")
    } else {
        let (reason, web_next_step) = if untrusted_web_readback {
            let next_step = if electron_web_content && used_pixel_focus {
                " The pixel-focus rung already ran, so do not repeat it."
            } else if next_rung.is_none() && path == PATH_AX {
                " The field reads the text; check the page's own response (search \
                 results, a draft) instead of typing again."
            } else if next_rung.is_none() {
                // Safari and other WebKit pages: the keys went in as typing
                // does, and no other background route reaches the page.
                " The keys went in as typing does and the field reads the text; \
                 check the page's own response (search results, a draft) instead of \
                 typing again."
            } else {
                " In Chrome, bind the tab with get_browser_state and type with \
                 browser_type, which the page observes and reads back; for an embedded \
                 web view, use the px form (x,y)."
            };
            (
                "on web content (Chromium / WebKit / Electron) the accessibility \
                 read-back is not proof that the page received the input"
                    .to_string(),
                next_step,
            )
        } else {
            let sent = if path == PATH_AX {
                "the app accepted the accessibility write, but "
            } else {
                "the keys were sent, but "
            };
            let why = unconfirmed.unwrap_or(Unconfirmed::Unreadable).describe();
            (format!("{sent}{why}"), "")
        };
        format!(
            "{UNCONFIRMED_PREFIX} {reason} ({char_count} char(s){detail}).{web_next_step} \
             {OBSERVE_BEFORE_RETYPING}"
        )
    };
    ToolResult::text(format!("{summary}{suffix}")).with_structured({
        // `effect` mirrors `verified`'s read-back tri-state: a TRUSTED positive
        // read-back is "confirmed"; anything else is "unverifiable".
        let mut s = serde_json::json!({
            "path": path,
            "characters": char_count,
            "requested_chars": char_count,
            "verified": verified,
            "effect": if verified { "confirmed" } else { "unverifiable" },
        });
        if let Some(delivered_chars) = delivered_chars {
            s["delivered_chars"] = serde_json::json!(delivered_chars);
        }
        if untrusted_web_readback {
            // Web-content AXValue read-back. A real browser TAB → the typed
            // browser tools (get_browser_state, browser_type) are the reliable
            // rung; an embedded web view (Electron, no CDP) → the element px
            // action. It's a
            // renderer/DOM-focus problem, never a foreground one.
            let escalation = match next_rung {
                Some("px") => Some((
                    "px",
                    "Electron web view — AXValue read-back cannot prove that the renderer \
                     observed the input. Confirm via the screenshot; if it didn't land, \
                     re-type with the element px action (x,y to pixel-focus the field, \
                     then type).",
                )),
                Some("page") => Some((
                    "page",
                    "Browser web content: AXValue read-back cannot prove that the page \
                     observed the input. In Chrome, call get_browser_state (pid, window_id) \
                     and type with browser_type, which reads the field back (its set_value \
                     mode suits controlled React inputs). Or confirm via the screenshot.",
                )),
                _ => None,
            };
            if let Some((recommended, reason)) = escalation {
                s["escalation"] = serde_json::json!({
                    "recommended": recommended,
                    "reason": reason,
                });
            }
        }
        // An unknown native edit supplies no evidence that a different input
        // route is needed, so no escalation is attached: observe first.
        s
    })
}

// ── Blocking implementation ───────────────────────────────────────────────────

/// Which delivery path was taken. Surfaced as `structuredContent.path`
/// on success.
const PATH_AX: &str = "ax";
const PATH_KEY_EVENTS: &str = "key_events";
const PATH_KEY_EVENTS_FG: &str = "key_events_fg";

const UNCONFIRMED_PREFIX: &str = "⚠️ Not confirmed:";
const OBSERVE_BEFORE_RETYPING: &str = "Read the field or take a screenshot before typing again; \
     retyping can duplicate the edit.";

// The daemon transport has a 120-second request deadline. Character synthesis
// is synchronous, so a type_text call that synthesizes characters must answer
// within 100 seconds of reaching the registry (before any queueing), reserving
// 20 seconds for routing, focus assistance, observation, response
// serialization, and transport. Posting stops at the posting deadline (the
// budget less the read-back drain) at a character boundary; the preflight
// refuses up front what its estimate says cannot finish before it. The
// deadline is the guarantee when the estimate is wrong: a daemon whose sleeps
// run long (launchd throttling) or that is stalled.
// Atomic AX writes are deliberately excluded: their cost does not scale per
// character and a single write remains the preferred route for large text.
const SYNTHESIS_BUDGET_MS: u64 = 100_000;

/// When this call must stop posting characters.
fn posting_deadline() -> std::time::Instant {
    cua_driver_core::tool::current_dispatch_received_at().unwrap_or_else(std::time::Instant::now)
        + std::time::Duration::from_millis(SYNTHESIS_BUDGET_MS)
        - DELIVERY_DRAIN_TIMEOUT
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextDeliveryRoute {
    AtomicAx,
    /// PID-routed Unicode keys (`type_text_with_delay`).
    PidUnicode,
    /// Desktop-scope Unicode keys through the HID queue (`type_text_global`).
    DesktopUnicode,
    /// Physical US-keyboard transitions (`type_text_physical_global`).
    PhysicalSynthesis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SynthesisRefusal {
    requested_chars: usize,
    estimated_duration_ms: u64,
    per_character_ms: u64,
    max_chunk_chars: usize,
    budget_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxAttempt {
    NotAttempted,
    Rejected,
    Unchanged,
    Unverifiable,
}

impl AxAttempt {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotAttempted => "not_attempted",
            Self::Rejected => "rejected",
            Self::Unchanged => "unchanged",
            Self::Unverifiable => "unverifiable",
        }
    }
}

/// The sleeps each synthesis route schedules per character (keyboard.rs) at
/// key gap `gap_ms`.
fn per_character_ms(route: TextDeliveryRoute, delay_ms: u64, gap_ms: u64) -> u64 {
    match route {
        TextDeliveryRoute::AtomicAx => 0,
        // The gap after the key-down, then the delay (the gap when it is 0).
        TextDeliveryRoute::PidUnicode => {
            gap_ms.saturating_add(if delay_ms > 0 { delay_ms } else { gap_ms })
        }
        // The gap after the key-down, then max(delay, gap).
        TextDeliveryRoute::DesktopUnicode => gap_ms.saturating_add(delay_ms.max(gap_ms)),
        // The gap after each of up to four events (Shift down, key down, key
        // up, Shift up: a payload-independent worst case), then whatever the
        // delay adds beyond one gap.
        TextDeliveryRoute::PhysicalSynthesis => gap_ms
            .saturating_mul(4)
            .saturating_add(delay_ms.saturating_sub(gap_ms)),
    }
}

/// Refuse synthesis whose estimate (scheduled sleeps plus the read-back
/// drain) exceeds `budget_ms`, the time left to answer.
fn synthesis_preflight(
    route: TextDeliveryRoute,
    requested_chars: usize,
    delay_ms: u64,
    gap_ms: u64,
    budget_ms: u64,
) -> Option<SynthesisRefusal> {
    if route == TextDeliveryRoute::AtomicAx {
        return None;
    }
    let per_character_ms = per_character_ms(route, delay_ms, gap_ms);
    let drain_ms = DELIVERY_DRAIN_TIMEOUT.as_millis() as u64;
    let estimated_duration_ms = (requested_chars as u64)
        .saturating_mul(per_character_ms)
        .saturating_add(drain_ms);
    if estimated_duration_ms <= budget_ms {
        return None;
    }
    let max_chunk_chars = budget_ms
        .saturating_sub(drain_ms)
        .checked_div(per_character_ms)
        .unwrap_or_default() as usize;
    Some(SynthesisRefusal {
        requested_chars,
        estimated_duration_ms,
        per_character_ms,
        max_chunk_chars,
        budget_ms,
    })
}

/// [`synthesis_preflight`] at the daemon's key gap, with the time left before
/// `deadline` (plus the drain that follows it) as the budget.
fn preflight_before(
    route: TextDeliveryRoute,
    requested_chars: usize,
    delay_ms: u64,
    deadline: std::time::Instant,
) -> Option<SynthesisRefusal> {
    let left =
        deadline.saturating_duration_since(std::time::Instant::now()) + DELIVERY_DRAIN_TIMEOUT;
    synthesis_preflight(
        route,
        requested_chars,
        delay_ms,
        crate::input::keyboard::key_gap().as_millis() as u64,
        left.as_millis() as u64,
    )
}

/// Posting stopped at the posting deadline, at a character boundary, so the
/// call could still answer. Posted is not landed, and resending the whole
/// text would duplicate what did land: the next step is to read the field.
fn stopped_at_deadline_result(
    path: Option<&'static str>,
    posted_chars: usize,
    requested_chars: usize,
) -> ToolResult {
    let mut structured = serde_json::json!({
        "code": "type_text_stopped_at_deadline",
        "effect": if posted_chars == 0 { "none" } else { "partial" },
        "requested_chars": requested_chars,
        "posted_chars": posted_chars,
        "retryable": false,
        "escalation": {
            "recommended": "verify_state",
            "reason": "Posted keys are not proof that they landed. Read the field, then type only what is missing.",
        },
    });
    if let Some(path) = path {
        structured["path"] = serde_json::json!(path);
    }
    ToolResult::error(format!(
        "type_text stopped at its time limit after posting {posted_chars} of {requested_chars} \
         character(s), so that it could still answer. Read the field, then type only what is \
         missing; do not resend the whole text."
    ))
    .with_structured(structured)
}

fn synthesis_refusal_result(
    path: &'static str,
    refusal: &SynthesisRefusal,
    ax_attempt: AxAttempt,
) -> ToolResult {
    let effect = if ax_attempt == AxAttempt::Unverifiable {
        "indeterminate"
    } else {
        "refused"
    };
    let retryable = ax_attempt != AxAttempt::Unverifiable;
    let escalation = if retryable {
        serde_json::json!({
            "recommended": "chunk",
            "reason": format!(
                "Character synthesis would exceed the bounded transport-safe budget. Retry in chunks of at most {} characters at this delay.",
                refusal.max_chunk_chars
            )
        })
    } else {
        serde_json::json!({
            "recommended": "verify_state",
            "reason": "The atomic AX attempt could not be observed. Re-read the target before deciding whether any suffix remains; do not retry blindly."
        })
    };
    let mut structured = serde_json::json!({
        "code": "type_text_synthesis_budget_exceeded",
        "path": path,
        "effect": effect,
        "requested_chars": refusal.requested_chars,
        "estimated_duration_ms": refusal.estimated_duration_ms,
        "synthesis_budget_ms": refusal.budget_ms,
        "per_character_ms": refusal.per_character_ms,
        "max_chunk_chars": refusal.max_chunk_chars,
        "synthesized_chars": 0,
        "atomic_ax_effect": ax_attempt.as_str(),
        "retryable": retryable,
        "escalation": escalation,
    });
    if ax_attempt != AxAttempt::Unverifiable {
        structured["delivered_chars"] = serde_json::json!(0);
        structured["retry_from_character"] = serde_json::json!(0);
    }
    let message = if retryable {
        format!(
            "type_text refused character synthesis before emitting character events: {} characters require an estimated {}ms at {}ms per character, exceeding the {}ms budget; retry in chunks of at most {} characters",
            refusal.requested_chars,
            refusal.estimated_duration_ms,
            refusal.per_character_ms,
            refusal.budget_ms,
            refusal.max_chunk_chars,
        )
    } else {
        format!(
            "type_text did not synthesize character events because {} characters require an estimated {}ms, exceeding the {}ms budget; the preceding atomic AX attempt was unverifiable, so re-read the target before retrying",
            refusal.requested_chars, refusal.estimated_duration_ms, refusal.budget_ms,
        )
    };
    ToolResult::error(message).with_structured(structured)
}

fn path_has_untrusted_web_readback(path: &str) -> bool {
    path == PATH_AX || path == PATH_KEY_EVENTS || path == PATH_KEY_EVENTS_FG
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceVerification {
    verified: bool,
    untrusted_web_readback: bool,
}

fn surface_verification(
    path: &str,
    verified: bool,
    target_is_web_content: bool,
) -> SurfaceVerification {
    let untrusted_web_readback =
        verified && target_is_web_content && path_has_untrusted_web_readback(path);
    SurfaceVerification {
        verified: verified && !untrusted_web_readback,
        untrusted_web_readback,
    }
}

/// Which kind of web content took the text, for the next-step advice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebSurface {
    Electron,
    /// A Chromium-family browser page, which the browser tools can drive.
    ChromiumBrowser,
    /// Anything else: Safari and other WebKit pages, or native content.
    Other,
}

fn web_readback_next_rung(surface: WebSurface, used_pixel_focus: bool) -> Option<&'static str> {
    match (surface, used_pixel_focus) {
        (WebSurface::Electron, true) => None,
        (WebSurface::Electron, false) => Some("px"),
        (WebSurface::ChromiumBrowser, _) => Some("page"),
        // A WebKit page has no browser-tool route, and its pixel route ends in
        // the same key events this call already sent.
        (WebSurface::Other, _) => None,
    }
}

fn electron_background_ax_refusal(
    delivery_mode: super::DeliveryMode,
    has_ax_target: bool,
    used_pixel_focus: bool,
    electron_background_ax_unsafe: bool,
) -> Option<BackgroundRefusal> {
    if delivery_mode.is_foreground()
        || !has_ax_target
        || used_pixel_focus
        || !electron_background_ax_unsafe
    {
        return None;
    }
    Some(BackgroundRefusal {
        code: "background_unavailable",
        reason: "The Electron AX target cannot establish a safe exact background text route on macOS because its ancestry is web content or could not be proven native; its centre could not be resolved on the window. Use the pixel-targeted type_text form (x,y) on the field, which also stays in the background."
            .to_owned(),
        advice: Some("px"),
    })
}

/// A type_text addressed to a Mac Catalyst text view without keyboard focus,
/// refused before any input.
const CATALYST_TEXT_NEEDS_FOCUS: &str = "catalyst_text_needs_focus";

/// Whether a `(role, subrole)` chain (the target first, then its ancestors)
/// is a Mac Catalyst text view: a text control inside the `AXGroup` with
/// subrole `iOSContentGroup` that hosts a Catalyst window's UIKit content.
/// UIKit text views there (Messages compose, WhatsApp, the Stocks search
/// field) accept an `AXSelectedText` write and then ignore it, and ignore
/// `AXFocused` writes too. A chain that stops early proves nothing.
fn is_catalyst_text_view(chain: &[(String, String)]) -> bool {
    catalyst_text_control(chain) == CatalystText::Yes
}

/// What a `(role, subrole)` chain says about a Mac Catalyst text control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalystText {
    /// A text control under an `iOSContentGroup`.
    Yes,
    /// Not a text control, or its ancestry reached the window (or the
    /// application) without an `iOSContentGroup`.
    No,
    /// A text control whose ancestry could not be read to the window: an
    /// unreadable role, a missing parent, or the 40-step cap. Not proof of a
    /// native field.
    Unknown,
}

pub(crate) fn catalyst_text_control(chain: &[(String, String)]) -> CatalystText {
    let Some(((role, _), ancestors)) = chain.split_first() else {
        return CatalystText::Unknown;
    };
    // The target's role could not be read (the caller may have read a text
    // role a moment earlier): unknown, not proof of a non-text control.
    if role.is_empty() {
        return CatalystText::Unknown;
    }
    if !matches!(
        role.as_str(),
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
    ) {
        return CatalystText::No;
    }
    if ancestors
        .iter()
        .any(|(role, subrole)| role == "AXGroup" && subrole == "iOSContentGroup")
    {
        return CatalystText::Yes;
    }
    match ancestors.last().map(|(role, _)| role.as_str()) {
        Some("AXWindow" | "AXApplication") => CatalystText::No,
        _ => CatalystText::Unknown,
    }
}

/// [`catalyst_text_control`] for a live element.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub(crate) unsafe fn catalyst_text_control_of(element: AXUIElementRef) -> CatalystText {
    catalyst_text_control(&ax_role_chain(element))
}

/// `(AXRole, AXSubrole)` of `element` and its ancestors up to the window.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn ax_role_chain(element: AXUIElementRef) -> Vec<(String, String)> {
    let mut chain = Vec::new();
    let mut current = element;
    let mut owned = false;
    for _ in 0..40 {
        let role = copy_string_attr(current, "AXRole").unwrap_or_default();
        let subrole = copy_string_attr(current, "AXSubrole").unwrap_or_default();
        let top = matches!(role.as_str(), "" | "AXWindow" | "AXApplication");
        chain.push((role, subrole));
        let parent = if top {
            None
        } else {
            crate::ax::bindings::copy_element_attr(current, "AXParent")
        };
        if owned {
            CFRelease(current as CFTypeRef);
        }
        let Some(parent) = parent else {
            return chain;
        };
        current = parent;
        owned = true;
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    chain
}

/// The refusal for an addressed Catalyst text view without keyboard focus,
/// on the background and the foreground route alike.
fn catalyst_text_needs_focus_result(pid: i32, window_id: Option<u32>) -> ToolResult {
    let reason = "This Mac Catalyst text view does not have keyboard focus. Catalyst ignores \
                  accessibility text writes and focus requests, so only typed keys reach it, \
                  and keys go to the field that has focus. Nothing was sent. Next: click the \
                  field (its element_token; a background click is enough), then call \
                  type_text again.";
    ToolResult::error(format!(
        "type_text refused ({CATALYST_TEXT_NEEDS_FOCUS}): {reason}"
    ))
    .with_structured(serde_json::json!({
        "code": CATALYST_TEXT_NEEDS_FOCUS,
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "reason": reason,
    }))
}

/// An addressed web page field that did not take focus, refused before any key.
const WEB_FIELD_NOT_FOCUSED: &str = "web_field_not_focused";

/// The caller's own target for the refusal's retry: its element_token, or its
/// x,y on the window, with its delay_ms (a long text at delay 0 fits the
/// synthesis budget that the 30 ms default would exceed). Null without either.
fn retry_target(args: &Value, pid: i32, px: Option<f64>, py: Option<f64>, window_id: Option<u32>) -> Value {
    let mut target = match args.get("element_token").and_then(Value::as_str) {
        Some(token) => serde_json::json!({ "element_token": token }),
        None => match (px, py, window_id) {
            (Some(x), Some(y), Some(wid)) => serde_json::json!({ "pid": pid, "window_id": wid, "x": x, "y": y }),
            _ => Value::Null,
        },
    };
    if let (Some(object), Some(delay)) = (target.as_object_mut(), args.get("delay_ms")) {
        object.insert("delay_ms".into(), delay.clone());
    }
    target
}

fn web_field_not_focused_result(
    target: Value,
    text: &str,
    session: Option<&str>,
    foreground: bool,
) -> ToolResult {
    let reason = if foreground {
        "The page did not move keyboard focus to this field, even in front (after a focus \
         request and a press of the field), and typed keys go to whatever the page has \
         focused, so no key was sent. Next: get_window_state with include_screenshot:true, \
         then type_text with x,y on the field and delivery_mode:\"foreground\"."
    } else {
        "The page did not move keyboard focus to this field (after a focus request and a \
         press of the field), and typed keys go to whatever the page has focused, so no key \
         was sent. Next: the same type_text with delivery_mode:\"foreground\", which brings \
         the window to the front for the typing and then gives the front back (after \
         act_and_read, whose read replaces the token, use the field's element_token from \
         that read)."
    };
    let mut structured = serde_json::json!({
        "code": WEB_FIELD_NOT_FOCUSED,
        "effect": "refused",
        "retryable": true,
        "delivered_chars": 0,
        "reason": reason,
    });
    if let (Value::Object(target), false) = (target, foreground) {
        let mut arguments = Value::Object(target);
        arguments["text"] = serde_json::json!(text);
        arguments["delivery_mode"] = serde_json::json!("foreground");
        if let Some(session) = session {
            arguments["session"] = serde_json::json!(session);
        }
        structured["next_call"] = serde_json::json!({ "tool": "type_text", "arguments": arguments });
    }
    ToolResult::error(format!("type_text refused ({WEB_FIELD_NOT_FOCUSED}): {reason}"))
        .with_structured(structured)
}

/// Raised inside the key rung when an addressed web field does not hold
/// keyboard focus right before the first key: nothing was posted.
#[derive(Debug)]
struct WebFieldNotFocused;

impl std::fmt::Display for WebFieldNotFocused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the page did not move keyboard focus to the addressed field")
    }
}

impl std::error::Error for WebFieldNotFocused {}

const CATALYST_BACKGROUND_KEYS_NOT_LANDED: &str = "catalyst_background_keys_not_landed";

/// Whether typing went to a Mac Catalyst text view: the addressed element,
/// else the focused element of the window.
fn catalyst_typing_target(pid: i32, element: Option<usize>, window_id: Option<u32>) -> bool {
    unsafe {
        match element {
            Some(ptr) => catalyst_text_control_of(ptr as AXUIElementRef) == CatalystText::Yes,
            None => {
                let Some(focused) = window_id
                    .and_then(|wid| crate::ax::exact_target::focused_element_in_window(pid, wid))
                else {
                    return false;
                };
                let yes = catalyst_text_control_of(focused) == CatalystText::Yes;
                CFRelease(focused as CFTypeRef);
                yes
            }
        }
    }
}

/// Background keys reached no character of a Catalyst field (its text read
/// back unchanged). Some Catalyst fields (Messages' search) take only keys
/// typed while their window is in front; "retry the remaining suffix" would
/// send the same keys the same way again.
fn catalyst_background_keys_not_landed(pid: i32, window_id: Option<u32>, requested: usize, path: &str) -> ToolResult {
    let reason = format!(
        "0 of {requested} character(s) landed: background keys did not reach this Mac Catalyst \
         field (its text read back unchanged), so nothing needs undoing. Next: type_text again \
         with delivery_mode \"foreground\" (it fronts the window briefly, then restores the \
         previous app). If that also lands nothing, the field does not take typed text."
    );
    ToolResult::error(format!("type_text failed ({CATALYST_BACKGROUND_KEYS_NOT_LANDED}): {reason}"))
        .with_structured(serde_json::json!({
            "code": CATALYST_BACKGROUND_KEYS_NOT_LANDED,
            "effect": "failed",
            "path": path,
            "pid": pid,
            "window_id": window_id,
            "requested_chars": requested,
            "delivered_chars": 0,
            "retryable": true,
            "escalation": { "recommended": "foreground", "reason": reason },
        }))
}

/// Whether `element` is an addressed Mac Catalyst text view that does not
/// have keyboard focus: keys sent now would go to whatever has it.
fn is_unfocused_catalyst_text_view(pid: i32, element: AXUIElementRef, window_id: Option<u32>) -> bool {
    is_catalyst_text_view(&unsafe { ax_role_chain(element) })
        && !addressed_element_has_focus(pid, element, window_id)
}

const DELIVERY_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const DELIVERY_DRAIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Pause before the single `AXFocused` re-apply in the foreground rung. Covers
/// an app that installs its own first responder just after activation is
/// observable, which would otherwise clobber the first write.
const FOCUS_REAPPLY_DELAY: std::time::Duration = std::time::Duration::from_millis(30);

/// Keyboard-rung policy for one window-addressed background insert, decided
/// once by the pure exact-target core before any input is posted.
#[derive(Clone, Debug)]
enum BackgroundKeyboardPolicy {
    /// The full ladder may run: foreground requests, pid-only targets, and
    /// window targets whose exact keyboard delivery is proven (singleton
    /// same-pid destination).
    Allowed,
    /// Only the semantic AX write on the proven exact element may run. The
    /// process-scoped CGEvent rung is refused with this refusal — carried so
    /// the exact reason is returned if the AX write does not land.
    SemanticOnly(BackgroundRefusal),
}

/// Delivery envelope for `type_text_blocking`: either an actuator ran
/// (`Typed`), or the exact-target decision refused before any event was
/// posted (`Refused`) and the caller must return the structured refusal.
enum TypeTextDelivery {
    Typed(TypeTextOutcome),
    Refused(BackgroundRefusal),
    /// An addressed Catalyst text view without keyboard focus; nothing sent.
    CatalystNeedsFocus,
    SynthesisRefused {
        path: &'static str,
        refusal: SynthesisRefusal,
        ax_attempt: AxAttempt,
    },
    /// Key posting reached the posting deadline after `posted_chars`; no
    /// path when the deadline had passed before any route ran.
    StoppedAtDeadline {
        path: Option<&'static str>,
        posted_chars: usize,
    },
}

/// What the keystroke rung saw: the read-back after every character was
/// posted (verified, delivered_chars, unconfirmed), or a stop at the posting
/// deadline after this many characters.
enum Keyed {
    ReadBack(bool, Option<usize>, Option<Unconfirmed>),
    Stopped(usize),
}

/// Decide the keyboard policy for a window-addressed background `type_text`.
///
/// Gathers fresh exact-target facts once and asks the pure core:
/// - `InsertText` executes → the full ladder is `Allowed`;
/// - `InsertText` refused but the caller addressed an exact element whose
///   ancestry is proven and semantic AX executes → `SemanticOnly`;
/// - otherwise the structured refusal result is returned and no input of any
///   kind (including a px focus click) may be sent.
async fn background_keyboard_policy(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
) -> Result<(super::BackgroundMutationLease, BackgroundKeyboardPolicy), ToolResult> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let lease = super::acquire_background_mutation(pid).await;
    let element_guard =
        element_ptr.map(|ptr| unsafe { crate::ax::snapshot::RetainedElement::retain(ptr) });
    let facts = match tokio::task::spawn_blocking(move || {
        let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
        crate::ax::exact_target::gather_background_facts(pid, window_id, element_ptr)
    })
    .await
    {
        Ok(facts) => facts,
        Err(error) => {
            return Err(ToolResult::error(format!(
                "Could not gather exact-target facts for pid {pid} window {window_id}: {error}"
            )));
        }
    };
    let target = ExactWindowTarget { pid, window_id };
    match decide_background_input(target, &facts, BackgroundAction::InsertText) {
        BackgroundInputDecision::Execute { .. } => Ok((lease, BackgroundKeyboardPolicy::Allowed)),
        BackgroundInputDecision::Refuse(refusal) => {
            let semantic_available = element_ptr.is_some()
                && decide_background_input(target, &facts, BackgroundAction::AxSemantic)
                    .is_execute();
            if semantic_available {
                Ok((lease, BackgroundKeyboardPolicy::SemanticOnly(refusal)))
            } else {
                Err(super::background_refusal_result(pid, window_id, &refusal))
            }
        }
    }
}

struct TypeTextOutcome {
    detail: String,
    path: &'static str,
    verified: bool,
    /// Exact when AX exposed the target value. `None` means delivery could not
    /// be observed, so the existing unverifiable contract remains in force.
    delivered_chars: Option<usize>,
    /// The missing evidence behind `verified: false`, stated in the summary.
    unconfirmed: Option<Unconfirmed>,
}

/// Why a completed, non-partial delivery is not confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unconfirmed {
    /// Screen Sharing: the field is on the remote Mac.
    NoReadback,
    /// The field's text cannot be read (Catalyst text views).
    Unreadable,
    /// Readable after typing but not before, so a change cannot be attributed.
    UnreadableBefore,
    /// Readable and unchanged, without a caret to prove that nothing landed
    /// (or web content, whose accessibility value can lag the page).
    Unchanged,
    /// Readable, but the change is not exactly the typed text.
    Mismatch,
    /// Web content showing only part of the typed text when the read-back
    /// ended, every key having been posted.
    Lagging,
}

impl Unconfirmed {
    fn from_readback(before: Option<&str>, after: Option<&str>) -> Self {
        match (before, after) {
            (_, None) => Self::Unreadable,
            (None, Some(_)) => Self::UnreadableBefore,
            (Some(before), Some(after)) if before == after => Self::Unchanged,
            _ => Self::Mismatch,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::NoReadback => "the field is on the remote Mac and cannot be read from here",
            Self::Unreadable => "the field's text cannot be read through accessibility",
            Self::UnreadableBefore => {
                "the field's text was unreadable before typing, so its current text \
                 cannot be attributed to this call"
            }
            Self::Unchanged => {
                "the field's text reads unchanged, which does not prove that nothing landed"
            }
            Self::Mismatch => {
                "the field's text changed, but not by exactly the typed text \
                 (autocorrect, autocapitalization, or another edit)"
            }
            Self::Lagging => {
                "the page showed only part of the text when the read-back ended and \
                 may still be taking the rest"
            }
        }
    }
}

fn foreground_settle_ms(pid: i32, frontmost_pid: Option<i32>) -> u64 {
    if frontmost_pid == Some(pid) {
        20
    } else {
        200
    }
}

/// Read-back verification for a keystroke rung: did the typed text actually land?
///
/// `before`/`after` are `AXValue` read from the target field before and after
/// the keystrokes. Only `Complete` positively confirms the text landed:
/// - unreadable `after` (`None`) → `Unverifiable` (Catalyst case; the agent
///   must confirm via screenshot).
/// - `after` contains the complete text → `Complete`.
/// - empty input text → trivially `Complete`.
///
#[cfg(test)]
fn typed_progress(before: Option<&str>, after: Option<&str>, text: &str) -> TypedProgress {
    classify_insertion(before, None, after, None, text)
}

/// Own the focused element only when it belongs to the requested window.
/// Without a window, preserve the legacy process-scoped focus lookup.
fn readback_focus(pid: i32, window_id: Option<u32>) -> Option<CFType> {
    unsafe {
        let element = match window_id {
            Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
            None => focused_element_of_pid(pid),
        }?;
        Some(CFType::wrap_under_create_rule(element as CFTypeRef))
    }
}

/// Per-invocation evidence used by the existing insertion classifier.
/// Implicit typing must not compare values from two different focused fields.
/// Retaining the original element also prevents its pointer from being recycled.
struct TypingReadback {
    pid: i32,
    element: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
    focused: Option<CFType>,
    before: Option<String>,
    before_range: Option<TextSelectionRange>,
    /// The focused element at capture was a proven web page field (not the
    /// page itself): untargeted keys go to it or nowhere.
    began_on_page_field: bool,
}

impl TypingReadback {
    fn capture(pid: i32, element: Option<(usize, Option<usize>)>, window_id: Option<u32>) -> Self {
        // The focused editor is retained here: read it from a current tree.
        if let Some(wid) = window_id {
            crate::ax::enablement::refresh_page(pid, wid);
        }
        let mut readback = Self {
            pid,
            element,
            window_id,
            focused: if element.is_none() {
                readback_focus(pid, window_id)
            } else {
                None
            },
            before: None,
            before_range: None,
            began_on_page_field: false,
        };
        readback.began_on_page_field = readback.focused.as_ref().is_some_and(|focused| {
            let focused = focused.as_CFTypeRef() as AXUIElementRef;
            unsafe { copy_string_attr(focused, "AXRole") }.as_deref() != Some("AXWebArea")
                && classify_target_web_area(pid, Some((focused as usize, None)), window_id)
                    == WebAreaClassification::WebContent
        });
        (readback.before, readback.before_range) = readback.read_sample();
        readback
    }

    /// The AX role of the field this read-back watches.
    fn role(&self) -> String {
        let ptr = self
            .element
            .map(|(ptr, _)| ptr as AXUIElementRef)
            .or_else(|| self.focused.as_ref().map(|el| el.as_CFTypeRef() as AXUIElementRef));
        ptr.and_then(|ptr| unsafe { copy_string_attr(ptr, "AXRole") }).unwrap_or_default()
    }

    fn selection(&self) -> Option<TextSelectionRange> {
        let ptr = self
            .element
            .map(|(ptr, _)| ptr as AXUIElementRef)
            .or_else(|| {
                self.focused
                    .as_ref()
                    .map(|el| el.as_CFTypeRef() as AXUIElementRef)
            })?;
        unsafe { crate::ax::text_state::focused_range(self.pid, ptr) }
    }

    fn sample(
        &self,
    ) -> (
        Option<String>,
        Option<TextSelectionRange>,
    ) {
        // A covered Chromium page reads its state from before the keys until
        // it is asked to publish (item 42).
        if let Some(wid) = self.window_id {
            crate::ax::enablement::refresh_page(self.pid, wid);
        }
        self.read_sample()
    }

    /// [`Self::sample`] without the page refresh (right after one).
    fn read_sample(
        &self,
    ) -> (
        Option<String>,
        Option<TextSelectionRange>,
    ) {
        let range = self.selection();
        let value = self.read();
        let after = self.selection();
        (value, (range == after).then_some(range).flatten())
    }

    fn read(&self) -> Option<String> {
        if let Some((ptr, _)) = self.element {
            // The caller's retained cache guard owns the addressed element.
            return unsafe { copy_string_attr(ptr as AXUIElementRef, "AXValue") };
        }
        let original = self.focused.as_ref()?;
        let current = readback_focus(self.pid, self.window_id)?;
        unsafe {
            if CFEqual(original.as_CFTypeRef(), current.as_CFTypeRef()) == 0 {
                return None;
            }
            let value = copy_string_attr(current.as_CFTypeRef() as AXUIElementRef, "AXValue");
            // Focus can change during an AX read. Discard that sample too.
            // A replacement editor needs fresh observation, not a guess that
            // the newly focused field is the same logical target.
            let after = readback_focus(self.pid, self.window_id)?;
            (CFEqual(original.as_CFTypeRef(), after.as_CFTypeRef()) != 0)
                .then_some(value)
                .flatten()
        }
    }
}

/// True when the addressed (or focused) AX element sits inside a web-content
/// subtree — an `AXWebArea` ancestor. That covers every Chromium / WebKit /
/// Electron rendered surface (Chrome, Safari, Slack, VS Code, X's compose box…),
/// where an AX write is echoed back through `AXValue` while the renderer/DOM
/// never observes it — so an AX read-back "confirm" there is a shim echo. A
/// browser's OWN native chrome (address bar, toolbar) has no `AXWebArea`
/// ancestor, so it stays trusted. Walks a bounded ancestor chain; each
/// `AXParent` copy is released, and it stops at the window/app boundary.
pub(super) fn target_in_web_area(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> bool {
    web_readback_is_untrusted(classify_target_web_area(
        pid,
        element_ptr_and_idx,
        window_id,
    ))
}

/// Whether the window's own focused element is proven to sit outside web
/// content (an unreadable or unavailable focus is not proof).
pub(super) fn window_focus_is_proven_native(pid: i32, window_id: u32) -> bool {
    classify_target_web_area(pid, None, Some(window_id)) == WebAreaClassification::NonWebContent
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebAreaClassification {
    WebContent,
    NonWebContent,
    Incomplete,
    WindowFocusUnavailable,
}

fn web_readback_is_untrusted(classification: WebAreaClassification) -> bool {
    matches!(
        classification,
        WebAreaClassification::WebContent | WebAreaClassification::WindowFocusUnavailable
    )
}

/// The window pixel (native capture pixels) at the centre of an Electron
/// page field, when `element` is one: proven web content with an on-window
/// frame.
fn electron_web_field_center(pid: i32, window_id: u32, element: usize, idx: usize) -> Option<(f64, f64)> {
    // Proven web content only: delivery's focus checks and key route key off
    // the same classification, so an unproven ancestry keeps the refusal.
    if classify_target_web_area(pid, Some((element, Some(idx))), Some(window_id)) != WebAreaClassification::WebContent {
        return None;
    }
    // Only a field takes text: a button or the page itself would take the
    // keys as shortcuts, so those keep the refusal.
    let role = unsafe { copy_string_attr(element as AXUIElementRef, "AXRole") }.unwrap_or_default();
    let editable = unsafe { crate::ax::bindings::copy_element_attr(element as AXUIElementRef, "AXEditableAncestor") }
        .map(|ancestor| unsafe { CFRelease(ancestor as CFTypeRef) })
        .is_some();
    if !takes_text(&role, editable) {
        return None;
    }
    let (screen_x, screen_y) = unsafe { crate::ax::bindings::element_screen_center(element as AXUIElementRef) }?;
    let frame = super::px_frame::resolve_window_px_frame(window_id).ok()?;
    Some(((screen_x - frame.bounds.x) * frame.scale, (screen_y - frame.bounds.y) * frame.scale))
}

enum UntargetedWeb {
    NotWeb,
    Refuse,
    Keys(crate::input::keyboard::LineBreaks),
}

/// See the untargeted check in [`cgevent_type_verified`].
fn untargeted_web_target(pid: i32, readback: &TypingReadback, background: bool) -> UntargetedWeb {
    if let Some(wid) = readback.window_id {
        crate::ax::enablement::refresh_page(pid, wid);
    }
    unsafe {
        let current = match readback.window_id {
            Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
            None => focused_element_of_pid(pid),
        };
        // A page field the call began with: the keys go to it or nowhere, so
        // focus that is now unreadable, unproven or elsewhere is refused.
        let strict = background && readback.began_on_page_field;
        let Some(current) = current else {
            return if strict { UntargetedWeb::Refuse } else { UntargetedWeb::NotWeb };
        };
        let class = classify_target_web_area(pid, Some((current as usize, None)), readback.window_id);
        let role = copy_string_attr(current, "AXRole").unwrap_or_default();
        let editable = crate::ax::bindings::copy_element_attr(current, "AXEditableAncestor")
            .map(|e| CFRelease(e as CFTypeRef))
            .is_some();
        let began_on = readback.began_on_page_field.then(|| {
            readback
                .focused
                .as_ref()
                .is_some_and(|original| CFEqual(original.as_CFTypeRef(), current as CFTypeRef) != 0)
        });
        CFRelease(current as CFTypeRef);
        if class != WebAreaClassification::WebContent {
            return if strict { UntargetedWeb::Refuse } else { UntargetedWeb::NotWeb };
        }
        if background && !untargeted_web_keys_allowed(&role, editable, began_on) {
            return UntargetedWeb::Refuse;
        }
        UntargetedWeb::Keys(line_breaks_for(true, &role))
    }
}

/// Whether untargeted background keys may go to the web element focused now
/// (`role`, `editable`): `began_on` is `Some(still the same)` when the call
/// began with a specific page field focused, `None` when it began with the
/// page itself or no readable page field.
fn untargeted_web_keys_allowed(role: &str, editable: bool, began_on: Option<bool>) -> bool {
    match began_on {
        Some(same) => same && takes_text(role, editable),
        None => role == "AXWebArea" || takes_text(role, editable),
    }
}

/// Whether an element of `role` (with or without an editable ancestor) is a
/// field that takes typed text.
fn takes_text(role: &str, editable_ancestor: bool) -> bool {
    // Chromium exposes a select-only menu button as AXComboBox too: only an
    // editable one takes text.
    matches!(role, "AXTextField" | "AXTextArea" | "AXSearchField")
        || (editable_ancestor && role != "AXWebArea")
}

/// How a line break is typed: Shift+Return into a multi-line web field (a
/// textarea or a contenteditable editor, both AXTextArea), where a "\n"
/// character event inserts nothing (item 42); a character elsewhere. A
/// single-line page field keeps the character: Shift+Return there submits
/// its form.
fn line_breaks_for(web: bool, role: &str) -> crate::input::keyboard::LineBreaks {
    if web && role == "AXTextArea" {
        crate::input::keyboard::LineBreaks::ShiftReturn
    } else {
        crate::input::keyboard::LineBreaks::Characters
    }
}

fn electron_background_ax_ancestry_is_unsafe(classification: WebAreaClassification) -> bool {
    !matches!(classification, WebAreaClassification::NonWebContent)
}

fn classify_target_web_area(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    window_id: Option<u32>,
) -> WebAreaClassification {
    use core_foundation::base::CFTypeRef;
    unsafe {
        // Start element: the addressed one (borrowed — do NOT release) or the
        // focused element (owned — must release when done). A window-addressed
        // request may only classify from the window's OWN focused element; a
        // pid-global focused element can belong to a same-process sibling and
        // sibling state must never vouch for the target. When window-bound
        // reacquisition fails, fail closed: report web content (untrusted
        // read-back) rather than trusting an unproven surface.
        let (start, start_owned) = match element_ptr_and_idx {
            Some((ptr, _)) => (ptr as AXUIElementRef, false),
            None => match window_id {
                Some(wid) => match crate::ax::exact_target::focused_element_in_window(pid, wid) {
                    Some(el) => (el, true),
                    None => return WebAreaClassification::WindowFocusUnavailable,
                },
                None => match focused_element_of_pid(pid) {
                    Some(el) => (el, true),
                    None => return WebAreaClassification::NonWebContent,
                },
            },
        };
        let mut cur = start;
        let mut cur_owned = start_owned;
        let mut classification = WebAreaClassification::Incomplete;
        for _ in 0..40 {
            match copy_string_attr(cur, "AXRole").as_deref() {
                Some("AXWebArea") => {
                    classification = WebAreaClassification::WebContent;
                    break;
                }
                // No web area lives above the window/app root — stop.
                Some("AXWindow") | Some("AXApplication") => {
                    classification = WebAreaClassification::NonWebContent;
                    break;
                }
                None => break,
                _ => {}
            }
            let parent = crate::ax::bindings::copy_element_attr(cur, "AXParent");
            if cur_owned {
                CFRelease(cur as CFTypeRef);
            }
            let Some(parent) = parent else {
                cur = std::ptr::null_mut();
                cur_owned = false;
                break;
            };
            cur = parent;
            cur_owned = true;
        }
        if cur_owned && !cur.is_null() {
            CFRelease(cur as CFTypeRef);
        }
        classification
    }
}

fn focus_typing_element(
    pid: i32,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    reapply: bool,
) -> anyhow::Result<()> {
    // Focus the target element so the keystrokes land in IT. Critical in
    // foreground mode: a freshly-fronted window's keyboard focus may be on the
    // search box or nowhere, so without this the text goes into the void (or the
    // wrong field). AXFocused is best-effort — harmless when unsupported.
    //
    // Ordering matters as much as the write itself. `with_foreground_assist` has
    // already waited for the activation to land, so AppKit has installed the
    // window's remembered first responder by now and this write lands *after*
    // it rather than being clobbered by it. Re-applying once on a failed
    // read-back covers apps that install their responder slightly late.
    if let Some((ptr, _)) = element_ptr_and_idx {
        let _ = crate::input::ax_actions::focus_element(ptr);
        if reapply && !crate::input::ax_actions::is_element_focused(pid, ptr) {
            std::thread::sleep(FOCUS_REAPPLY_DELAY);
            let _ = crate::input::ax_actions::focus_element(ptr);
        }
    }
    Ok(())
}

/// Type via CGEvent keystrokes at the current insertion point, then verify by
/// read-back. `type_text` is deliberately non-idempotent: it must never clear
/// an existing value merely because AX cannot read that value back.
fn cgevent_type_verified(
    pid: i32,
    recipient_pid: i32,
    text: &str,
    delay_ms: u64,
    readback: &TypingReadback,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    settle_ms: u64,
    deadline: std::time::Instant,
    web_field: bool,
    line_breaks: crate::input::keyboard::LineBreaks,
) -> anyhow::Result<Keyed> {
    // Background field focus ran inside protected preparation before recipient resolution.
    if settle_ms > 0 {
        focus_typing_element(pid, element_ptr_and_idx, true)?;
    }
    // First-keystroke settle (foreground rung only — caller passes `settle_ms > 0`).
    // Even once the window is front and the element focused, the surface isn't
    // ready to accept input for a few tens of ms, so the FIRST synthesized
    // character gets eaten: typing "i love u" rendered "love u" (the leading
    // "i " was dropped). A short sleep here covers that. Background/terminal call
    // sites pass 0 — they have no front transition and must not pay this latency.
    if settle_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(settle_ms));
    }
    // Keys go to whatever a page has focused, so an addressed web field must
    // hold keyboard focus when the first key is posted. In front, focus it
    // here as the background preparation does; from behind that preparation
    // already ran, and focus is checked again because it may have moved.
    if let (true, Some((ptr, _))) = (web_field, element_ptr_and_idx) {
        let element = ptr as AXUIElementRef;
        let focused = if settle_ms > 0 {
            focus_web_field(pid, element, readback.window_id)
        } else {
            addressed_element_has_focus(pid, element, readback.window_id)
        };
        if !focused {
            return Err(WebFieldNotFocused.into());
        }
    }
    // Focus preparation can change the caret or selection. Sample immediately
    // before posting input, while retaining the original target identity.
    let (before, selection) = readback.sample();
    if settle_ms == 0 {
        crate::ax::keyboard_target::validate_recipient(
            pid,
            readback.window_id,
            readback.element.map(|(ptr, _)| ptr).or_else(|| {
                readback
                    .focused
                    .as_ref()
                    .map(|element| element.as_CFTypeRef() as usize)
            }),
            recipient_pid,
        )?;
    }
    // The sample and the recipient check are AX calls a page's own focus
    // change can race: prove the addressed web field again, last.
    if let (true, Some((ptr, _))) = (web_field, element_ptr_and_idx) {
        if !addressed_element_has_focus(pid, ptr as AXUIElementRef, readback.window_id) {
            return Err(WebFieldNotFocused.into());
        }
    }
    // Untargeted keys into web content: judged from the element that holds
    // focus right now (refreshed), which is where the keys will go (item 42).
    // From behind they need a field that takes text, the same one the call
    // began with (or, when it began with the page itself, the field the page
    // reports now); the page itself keeps today's keys. The line-break mode
    // follows that element.
    let mut line_breaks = line_breaks;
    if element_ptr_and_idx.is_none() {
        match untargeted_web_target(pid, readback, settle_ms == 0) {
            UntargetedWeb::Refuse => return Err(WebFieldNotFocused.into()),
            UntargetedWeb::Keys(mode) => line_breaks = mode,
            UntargetedWeb::NotWeb => {}
        }
    }
    let posted = crate::input::keyboard::type_text_with_delay(
        recipient_pid,
        text,
        delay_ms,
        Some(deadline),
        line_breaks,
    )?;
    if posted < text.chars().count() {
        // Let what was posted land before answering, so the read that
        // follows sees all of it. A fixed wait, not an AX read-back: a
        // target that stops answering AX must not stretch the answer past
        // the transport deadline.
        std::thread::sleep(DELIVERY_DRAIN_TIMEOUT);
        return Ok(Keyed::Stopped(posted));
    }

    // CGEvent posting is asynchronous with respect to the renderer. In
    // particular, Chromium can acknowledge the posting process while a long
    // tail remains queued. Poll the AX value until the complete payload is
    // visible instead of treating any growth as success. If the deadline
    // expires after observable growth, surface the exact partial count.
    let deadline = std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT;
    let (verified, delivered_chars) = delivery_from_progress(
        submitted_shortfall_is_unverifiable(
            await_typed_progress_with_selection(before.as_deref(), selection, text, deadline, || {
                readback.sample()
            }),
            text,
        ),
        text,
    );
    // "0 delivered" covers both an untouched field and one whose selection
    // was deleted with nothing typed in its place: say which, so only the
    // untouched case is reported as nothing to undo.
    let unconfirmed = match delivered_chars {
        None => Some(Unconfirmed::from_readback(before.as_deref(), readback.read().as_deref())),
        Some(0) if !text.is_empty() => Some(Unconfirmed::from_readback(before.as_deref(), readback.read().as_deref())),
        Some(_) => None,
    };
    Ok(Keyed::ReadBack(verified, delivered_chars, unconfirmed))
}

#[cfg(test)]
fn await_typed_delivery(
    before: Option<&str>,
    text: &str,
    deadline: std::time::Instant,
    read_value: impl FnMut() -> Option<String>,
) -> (bool, Option<usize>) {
    delivery_from_progress(
        await_typed_progress(before, text, deadline, read_value),
        text,
    )
}

/// Typed Enter, Tab and Return are not characters a field keeps: they submit,
/// move focus, or reset the field (a chat box clears, a spreadsheet Name Box
/// jumps). A keystroke read-back short of the complete text then says nothing
/// about how many characters landed, so it must not be reported as partial or
/// zero delivery that the caller retries (which would send the text twice).
/// Only the typed-keys read-back uses this; the AX rung keeps its fallback.
fn submitted_shortfall_is_unverifiable(progress: TypedProgress, text: &str) -> TypedProgress {
    if progress != TypedProgress::Complete && text.contains(['\n', '\r', '\t']) {
        TypedProgress::Unverifiable
    } else {
        progress
    }
}

fn delivery_from_progress(progress: TypedProgress, text: &str) -> (bool, Option<usize>) {
    match progress {
        TypedProgress::Complete => (true, Some(text.chars().count())),
        TypedProgress::Partial(delivered) => (false, Some(delivered)),
        TypedProgress::Unchanged => (false, Some(0)),
        TypedProgress::Unverifiable => (false, None),
    }
}

/// Poll the target's read-back until it proves complete delivery or the
/// deadline passes, and report the strongest reading observed.
///
#[cfg(test)]
fn await_typed_progress(
    before: Option<&str>,
    text: &str,
    deadline: std::time::Instant,
    mut read_value: impl FnMut() -> Option<String>,
) -> TypedProgress {
    await_typed_progress_with_selection(before, None, text, deadline, || (read_value(), None))
}

/// Settle the existing AX or keyboard readback without treating an old payload
/// as proof of delivery. Unchanged ambiguous values may still be pending; poll
/// them within the existing deadline and retain uncertainty if nothing resolves.
fn await_typed_progress_with_selection(
    before: Option<&str>,
    selection: Option<TextSelectionRange>,
    text: &str,
    deadline: std::time::Instant,
    mut read_value: impl FnMut() -> (
        Option<String>,
        Option<TextSelectionRange>,
    ),
) -> TypedProgress {
    let mut best_partial = None;
    // An unchanged value read once with a moved caret: an edit whose value
    // lags. A later reading without the caret must not undo that evidence
    // (item 25: "unchanged" lets type_text type the text a second time).
    let mut moved_caret_seen = false;
    loop {
        let (after, after_selection) = read_value();
        let progress =
            classify_insertion(before, selection, after.as_deref(), after_selection, text);
        match progress {
            TypedProgress::Complete => return TypedProgress::Complete,
            TypedProgress::Partial(delivered) => {
                best_partial =
                    Some(best_partial.map_or(delivered, |best: usize| best.max(delivered)));
            }
            TypedProgress::Unverifiable if before.is_some() && after.as_deref() == before => {
                moved_caret_seen = true;
            }
            TypedProgress::Unverifiable => return TypedProgress::Unverifiable,
            TypedProgress::Unchanged => {}
        }
        if std::time::Instant::now() >= deadline {
            return if progress == TypedProgress::Unverifiable
                || (progress == TypedProgress::Unchanged && moved_caret_seen)
            {
                TypedProgress::Unverifiable
            } else {
                best_partial.map(TypedProgress::Partial).unwrap_or(progress)
            };
        }
        std::thread::sleep(DELIVERY_DRAIN_POLL_INTERVAL);
    }
}

/// How `type_text_blocking` may deliver: the requested mode, the addressed
/// window, and the background keyboard policy decided for it.
struct KeyboardRoute {
    background_focus: Option<Arc<crate::background_focus::HostController>>,
    delivery_mode: super::DeliveryMode,
    window_id: Option<u32>,
    keyboard_policy: BackgroundKeyboardPolicy,
    /// Whether an unfocused addressed native field may be focused (an
    /// exact-window AXFocused write) before the AX insertion.
    prepare_native_text: bool,
    /// When key posting must stop (see [`posting_deadline`]).
    deadline: std::time::Instant,
    /// The addressed element was proven an Electron page field before its
    /// focus click: delivery keeps the web-field rules (keys only, focus
    /// proofs) even if a later ancestry read fails.
    proven_web_field: bool,
}

/// Best-effort-background ladder for `type_text`.
///
/// - `delivery_mode == Background` (default): AX insert then read-back. A
///   rejected write or readable unchanged native value can fall back to keys.
///   An accepted write with uncertain read-back stops for observation. Never fronts.
/// - `delivery_mode == Foreground`: the agent's explicit last resort — briefly
///   front `window_id`, insert at the current cursor, restore, then read-back.
///
/// Returns `(detail, path, verified)`. `verified` is `true` only when a
/// read-back positively confirmed the text; `false` means the agent must
/// confirm via screenshot (and, for background, can escalate to foreground).
fn type_text_blocking(
    pid: i32,
    text: &str,
    element_ptr_and_idx: Option<(usize, Option<usize>)>,
    delay_ms: u64,
    is_terminal_target: bool,
    route: KeyboardRoute,
) -> anyhow::Result<TypeTextDelivery> {
    let KeyboardRoute {
        background_focus,
        delivery_mode,
        window_id,
        keyboard_policy,
        prepare_native_text,
        deadline,
        proven_web_field,
    } = route;
    // The worker can start after the deadline (the async side checked it
    // before its last await): then nothing is sent, on any route.
    if std::time::Instant::now() >= deadline {
        return Ok(TypeTextDelivery::StoppedAtDeadline {
            path: None,
            posted_chars: 0,
        });
    }
    // A background terminal insert has no semantic AX rung: when the
    // exact-target decision restricted this request to semantic-only, there is
    // nothing safe to run, and an over-budget synthesis cannot start. Both
    // refusals need no read of the target, so they come before any AX call.
    if is_terminal_target && !delivery_mode.is_foreground() {
        if let BackgroundKeyboardPolicy::SemanticOnly(refusal) = &keyboard_policy {
            return Ok(TypeTextDelivery::Refused(refusal.clone()));
        }
        if let Some(refusal) = preflight_before(
            TextDeliveryRoute::PidUnicode,
            text.chars().count(),
            delay_ms,
            deadline,
        ) {
            return Ok(TypeTextDelivery::SynthesisRefused {
                path: PATH_KEY_EVENTS,
                refusal,
                ax_attempt: AxAttempt::NotAttempted,
            });
        }
    }
    // Original field value before any rung drives read-back verification only.
    // An unreadable value is not evidence that the field is empty — and for a
    // window-addressed request it must come from the exact target window,
    // never a same-process sibling.
    if let (None, Some(wid), false) = (element_ptr_and_idx, window_id, delivery_mode.is_foreground()) {
        settle_web_area_focus(pid, wid);
    }
    // An addressed field in web content: its keys are guarded by a focus
    // proof on either route. A token-addressed Electron field reaches here
    // only after its pixel focus was proven (item 42).
    let mut readback = TypingReadback::capture(pid, element_ptr_and_idx, window_id);
    // Web content, decided once for the whole call: an earlier proof (the
    // token's, before its focus click, or the read-back's at capture) holds
    // even if a later ancestry read fails, so a page field never falls back
    // to the AX write, character line breaks or a retryable partial count.
    // Proven web content picks the line-break mode; the untrusted reading
    // (an unavailable window focus counts as web) keeps every other rule.
    let web_proven = proven_web_field
        || (element_ptr_and_idx.is_none() && readback.began_on_page_field)
        || classify_target_web_area(pid, element_ptr_and_idx, window_id) == WebAreaClassification::WebContent;
    let web_target = web_proven || target_in_web_area(pid, element_ptr_and_idx, window_id);
    let addressed_web_field = element_ptr_and_idx.is_some() && web_target;
    let line_breaks = line_breaks_for(web_proven, &readback.role());

    // --- Foreground rung: explicit agent request (skip AX/background ladder). ---
    if delivery_mode.is_foreground() {
        // Catalyst ignores the AXFocused write this rung relies on: keys for an
        // addressed text view without focus would go to whatever has it, and a
        // readable unchanged target would then report "delivered 0" wrongly.
        if element_ptr_and_idx
            .is_some_and(|(ptr, _)| is_unfocused_catalyst_text_view(pid, ptr as AXUIElementRef, window_id))
        {
            return Ok(TypeTextDelivery::CatalystNeedsFocus);
        }
        let screen_sharing_target = crate::input::keyboard::is_screen_sharing_pid(pid);
        if let Some(refusal) = preflight_before(
            if screen_sharing_target {
                TextDeliveryRoute::PhysicalSynthesis
            } else {
                TextDeliveryRoute::PidUnicode
            },
            text.chars().count(),
            delay_ms,
            deadline,
        ) {
            return Ok(TypeTextDelivery::SynthesisRefused {
                path: if window_id.is_some() {
                    PATH_KEY_EVENTS_FG
                } else {
                    PATH_KEY_EVENTS
                },
                refusal,
                ax_attempt: AxAttempt::NotAttempted,
            });
        }
        // Settle between front+focus and the first keystroke — see the
        // "i love u" -> "love u" first-char-drop note in cgevent_type_verified.
        // A target that was already frontmost pays only 20ms for element-focus
        // settling. Focus-proxy clients that were just activated need longer:
        // an RDP client (Microsoft Windows App) re-arms its keyboard grab with
        // the remote host over hundreds of ms, so at 60ms every keystroke was
        // dropped. 200ms covers that re-grab without penalizing an already
        // armed interactive stream on every text chunk.
        let foreground_settle_ms = foreground_settle_ms(pid, apps::frontmost_pid());
        let do_type = || {
            cgevent_type_verified(
                pid,
                pid,
                text,
                delay_ms,
                &readback,
                element_ptr_and_idx,
                foreground_settle_ms,
                deadline,
                addressed_web_field,
                line_breaks,
            )
        };
        let (keyed, fronted) = match window_id {
            Some(wid) if screen_sharing_target => {
                // Screen Sharing forwards physical HID transitions to the
                // guest. PID-routed Unicode events all carry keycode 0 (the A
                // key), so a guest sees "aaaa"; modifier flags alone likewise
                // turn Cmd+V into plain "v". The explicit foreground rung may
                // safely use the global HID queue while the exact target is
                // guarded and restored.
                let mut posted = 0;
                crate::input::skylight::with_foreground_hid_activation(
                    pid as libc::pid_t,
                    wid,
                    || {
                        if foreground_settle_ms > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(
                                foreground_settle_ms,
                            ));
                        }
                        posted = crate::input::keyboard::type_text_physical_global(
                            text,
                            delay_ms,
                            Some(deadline),
                        )?;
                        if posted < text.chars().count() {
                            // Let what was posted land while the target is
                            // still in front.
                            std::thread::sleep(DELIVERY_DRAIN_TIMEOUT);
                        }
                        Ok(())
                    },
                )?;
                let keyed = if posted < text.chars().count() {
                    Keyed::Stopped(posted)
                } else {
                    Keyed::ReadBack(false, None, Some(Unconfirmed::NoReadback))
                };
                (keyed, true)
            }
            Some(wid) => {
                // Front → type → restore. The closure returns the read-back
                // result; with_foreground_assist returns whether it actually
                // fronted (Ok(false) when the fronting SPIs are unavailable —
                // the keystrokes still ran, just as background input).
                let mut typed_delivery = Keyed::ReadBack(false, None, None);
                let fronted = crate::input::skylight::with_foreground_assist(
                    pid as libc::pid_t,
                    wid,
                    || {
                        typed_delivery = do_type()?;
                        Ok(())
                    },
                )?;
                (typed_delivery, fronted)
            }
            // No window to front — best-effort background keystrokes instead.
            None => (do_type()?, false),
        };
        let (verified, delivered_chars, unconfirmed) = match keyed {
            Keyed::ReadBack(verified, delivered_chars, unconfirmed) => {
                (verified, delivered_chars, unconfirmed)
            }
            Keyed::Stopped(posted_chars) => {
                return Ok(TypeTextDelivery::StoppedAtDeadline {
                    path: Some(if fronted {
                        PATH_KEY_EVENTS_FG
                    } else {
                        PATH_KEY_EVENTS
                    }),
                    posted_chars,
                });
            }
        };
        let (delivered_chars, unconfirmed) = web_shortfall_is_unknown(
            delivered_chars,
            unconfirmed,
            text.chars().count(),
            web_target,
        );
        // Only claim the `_fg` path when a front actually happened; when no
        // foregrounding occurred (no window, or SPIs unavailable) these were
        // background keystrokes and `path` must say so honestly.
        return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
            detail: format!(" via foreground keystrokes ({delay_ms}ms delay)"),
            path: if fronted {
                PATH_KEY_EVENTS_FG
            } else {
                PATH_KEY_EVENTS
            },
            delivered_chars,
            verified,
            unconfirmed,
        }));
    }

    // --- Background rung 0: terminal emulator → CGEvent only (AX is dropped). ---
    if is_terminal_target {
        // The semantic-only and synthesis-budget refusals already ran before
        // the read-back above.
        tracing::debug!(
            "type_text: pid {pid} is a terminal emulator; skipping AX value-set, \
             using CGEvent key-event synthesis"
        );
        let prepare_element = || focus_typing_element(pid, element_ptr_and_idx, false);
        let keyed = super::with_background_key_recipient(
            background_focus.as_ref(),
            pid,
            window_id,
            element_ptr_and_idx.map(|(ptr, _)| ptr),
            element_ptr_and_idx
                .map(|_| &prepare_element as &dyn Fn() -> anyhow::Result<()>),
            |recipient_pid| {
                cgevent_type_verified(
                    pid,
                    recipient_pid,
                    text,
                    delay_ms,
                    &readback,
                    element_ptr_and_idx,
                    /*settle_ms=*/ 0,
                    deadline,
                    false,
                    crate::input::keyboard::LineBreaks::Characters,
                )
            },
        )?;
        let (verified, delivered_chars, unconfirmed) = match keyed {
            Keyed::ReadBack(verified, delivered_chars, unconfirmed) => {
                (verified, delivered_chars, unconfirmed)
            }
            Keyed::Stopped(posted_chars) => {
                return Ok(TypeTextDelivery::StoppedAtDeadline {
                    path: Some(PATH_KEY_EVENTS),
                    posted_chars,
                });
            }
        };
        return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
            detail: format!(" via CGEvent (terminal emulator, {delay_ms}ms delay)"),
            path: PATH_KEY_EVENTS,
            verified,
            delivered_chars,
            unconfirmed,
        }));
    }

    // --- Background rung 1: AX SelectedText write (element or focused). ---
    // Without an explicit element, a window-addressed request may only write
    // to the focused element when it provably belongs to the exact target
    // window — a sibling window's focused field is not the requested target.
    let ax_target: Option<(AXUIElementRef, bool, Option<usize>)> = match element_ptr_and_idx {
        Some((ptr, idx)) => Some((ptr as AXUIElementRef, /*owns=*/ false, idx)),
        // The readback already retained the editor that was focused when this
        // request started, resolved against the exact window. Write into that
        // same editor instead of resolving focus a second time: a focus change
        // in between would let the write land in one field while the readback
        // watches another, and an unchanged readback could then admit a
        // keyboard fallback into the first.
        None => readback.focused.as_ref().map(|el| {
            let ptr = el.as_CFTypeRef() as AXUIElementRef;
            unsafe { CFRetain(ptr as CFTypeRef) };
            (ptr, /*owns=*/ true, None)
        }),
    };
    // A web page ignores an AX text write: Chromium's renderer state (React's,
    // for one) never changes while AXValue echoes the text, and a background
    // Safari page accepts the write and leaves the field as it was (pilot: no
    // Safari write of 32 landed). A focused element that is not a text control
    // (a web area, a button) cannot take text at all. Both go straight to key
    // events, which reach whatever holds keyboard focus the way a user's
    // typing does. Electron's page is web content too: Chromium accepts the
    // write into its contenteditable or textarea and inserts nothing (item 42).
    let ax_target = ax_target.filter(|&(element, owns, idx_opt)| {
        let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
        let web_page = web_target || target_in_web_area(pid, Some((element as usize, idx_opt)), window_id);
        let keep = ax_text_write_first(&role, owns, web_page);
        if !keep && owns {
            unsafe { CFRelease(element as CFTypeRef) };
        }
        keep
    });
    // A Mac Catalyst text view accepts an AX text write and then ignores it,
    // and ignores AXFocused writes too: never send that write. Keys reach the
    // field when it has keyboard focus (the focused element, or an addressed
    // one that is focused in its window); an addressed field without focus is
    // refused, since keys would go to whatever has it (the foreground rung
    // above applies the same rule).
    let ax_target = match ax_target {
        Some((element, owns, _))
            if is_catalyst_text_view(&unsafe { ax_role_chain(element) }) =>
        {
            let focused = owns || addressed_element_has_focus(pid, element, window_id);
            if owns {
                unsafe { CFRelease(element as CFTypeRef) };
            }
            if !focused {
                return Ok(TypeTextDelivery::CatalystNeedsFocus);
            }
            tracing::debug!("type_text: pid {pid} target is a focused Catalyst text view; keys only");
            None
        }
        other => other,
    };
    let mut ax_attempt = AxAttempt::NotAttempted;
    if let Some((element, owns, idx_opt)) = ax_target {
        let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
        let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
        // An unfocused native field may accept AXSelectedText without editing:
        // its field editor is not installed yet. Reuse the keyboard rung's
        // exact-element preparation before the first write, avoiding a full
        // no-op delivery drain. Do not broaden semantic-only requests or touch
        // implicit/web targets. Already-focused selections must stay intact.
        if element_ptr_and_idx.is_some()
            && window_id.is_some()
            && prepare_native_text
            && matches!(
                role.as_str(),
                "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
            )
            && !web_target
            && !crate::input::ax_actions::is_element_focused(pid, element as usize)
        {
            let prepare_element = || crate::input::ax_actions::focus_element(element as usize);
            if matches!(keyboard_policy, BackgroundKeyboardPolicy::Allowed) {
                background_focus
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("background keyboard focus has no runtime owner"))?
                    .prepare_with(window_id, || {
                        super::validate_prepared_element_window(window_id, Some(element as usize))?;
                        prepare_element()
                    })?;
            } else {
                // Semantic-only requests retain their exact-field AX authority.
                prepare_element()?;
            }
            // A focus write is not proof of focus or insertion. Sample the
            // retained target and its current selection, then verify normally.
            (readback.before, readback.before_range) = readback.sample();
        }
        let err = unsafe { set_string_attr(element, "AXSelectedText", text) };
        // Classify the write before considering synthesis. Complete AX
        // delivery returns immediately. Partial delivery is surfaced as such
        // instead of appending the full payload again. When synthesis would
        // exceed its transport-safe budget, rejected/unchanged AX writes fail
        // safely and unreadable AX state is reported as indeterminate.
        //
        // The write is atomic, its effect on the field is not: the same drain
        // the keystroke rung uses settles the read-back here, so a value the
        // app is still rebuilding is never reported as a partial insertion.
        let ax_progress = if read_back_ax_write(err) {
            let deadline = std::time::Instant::now() + DELIVERY_DRAIN_TIMEOUT;
            Some(await_typed_progress_with_selection(
                readback.before.as_deref(),
                readback.before_range,
                text,
                deadline,
                || readback.sample(),
            ))
        } else {
            None
        };
        // AXValue is not renderer evidence in web content. An unchanged echo
        // there cannot prove that zero characters landed, so blind retry is
        // unsafe even though no synthesis has run yet.
        let unchanged_web_readback = ax_progress == Some(TypedProgress::Unchanged)
            && target_in_web_area(pid, Some((element as usize, idx_opt)), window_id);
        if owns {
            unsafe {
                CFRelease(element as _);
            }
        }
        if ax_progress == Some(TypedProgress::Complete) {
            let idx_str = idx_opt.map(|i| format!(" [{i}]")).unwrap_or_default();
            return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                detail: format!(" into{idx_str} {role} \"{title}\""),
                path: PATH_AX,
                verified: true,
                delivered_chars: Some(text.chars().count()),
                unconfirmed: None,
            }));
        }
        if let Some(TypedProgress::Partial(delivered_chars)) = ax_progress {
            let idx_str = idx_opt.map(|i| format!(" [{i}]")).unwrap_or_default();
            return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                detail: format!(" via partial AX write into{idx_str} {role} \"{title}\""),
                path: PATH_AX,
                verified: false,
                delivered_chars: Some(delivered_chars),
                unconfirmed: None,
            }));
        }
        ax_attempt = match ax_progress {
            Some(TypedProgress::Unchanged) if unchanged_web_readback => AxAttempt::Unverifiable,
            Some(TypedProgress::Unchanged) => AxAttempt::Unchanged,
            Some(TypedProgress::Unverifiable) => AxAttempt::Unverifiable,
            None => AxAttempt::Rejected,
            Some(TypedProgress::Complete | TypedProgress::Partial(_)) => unreachable!(),
        };
        if ax_attempt == AxAttempt::Unverifiable {
            // The write was accepted. Unreadable or untrusted unchanged state
            // cannot establish that nothing landed, so another input route
            // could insert the same text twice within this single request.
            let unconfirmed = if unchanged_web_readback {
                Unconfirmed::Unchanged
            } else {
                Unconfirmed::from_readback(readback.before.as_deref(), readback.read().as_deref())
            };
            return Ok(TypeTextDelivery::Typed(TypeTextOutcome {
                detail: format!(" via accepted AX write into {role} \"{title}\""),
                path: PATH_AX,
                verified: false,
                delivered_chars: None,
                unconfirmed: Some(unconfirmed),
            }));
        }
        tracing::debug!(
            "AX write did not land for {role} \"{title}\" (err={err}); \
             falling back to CGEvent keystrokes"
        );
    } else {
        tracing::debug!("No AX text target for pid {pid}; using CGEvent keystrokes");
    }

    // The semantic AX rung did not land and this request is restricted to it:
    // the process-scoped CGEvent rung could reach a sibling window, so return
    // the structured refusal instead of escalating.
    if let BackgroundKeyboardPolicy::SemanticOnly(refusal) = keyboard_policy {
        return Ok(TypeTextDelivery::Refused(refusal));
    }

    if let Some(refusal) = preflight_before(
        TextDeliveryRoute::PidUnicode,
        text.chars().count(),
        delay_ms,
        deadline,
    ) {
        return Ok(TypeTextDelivery::SynthesisRefused {
            path: PATH_KEY_EVENTS,
            refusal,
            ax_attempt,
        });
    }

    // --- Background rung 2: CGEvent keystrokes with read-back. ---
    // Never clear here: a partial AX write is rare, and clearing would violate
    // insert-at-cursor semantics.
    // Web content: select the exact window as its process's key window for
    // the keystrokes and their readback, without raising it.
    //
    // Keys go to whatever the page has focused, so an addressed web field
    // must hold keyboard focus before the first key: a focus write the page
    // ignored would otherwise type into another field. No key is sent then.
    let prepare_element = || {
        focus_typing_element(pid, element_ptr_and_idx, false)?;
        if let (true, Some((ptr, _))) = (addressed_web_field, element_ptr_and_idx) {
            if !focus_web_field(pid, ptr as AXUIElementRef, window_id) {
                return Err(WebFieldNotFocused.into());
            }
        }
        Ok(())
    };
    let keyed = super::with_background_key_recipient(
        background_focus.as_ref(),
        pid,
        window_id,
        element_ptr_and_idx.map(|(ptr, _)| ptr),
        element_ptr_and_idx
            .map(|_| &prepare_element as &dyn Fn() -> anyhow::Result<()>),
        |recipient_pid| {
            cgevent_type_verified(
                pid,
                recipient_pid,
                text,
                delay_ms,
                &readback,
                element_ptr_and_idx,
                /*settle_ms=*/ 0,
                deadline,
                addressed_web_field,
                line_breaks,
            )
        },
    )?;
    let (verified, delivered_chars, unconfirmed) = match keyed {
        Keyed::ReadBack(verified, delivered_chars, unconfirmed) => {
            (verified, delivered_chars, unconfirmed)
        }
        Keyed::Stopped(posted_chars) => {
            return Ok(TypeTextDelivery::StoppedAtDeadline {
                path: Some(PATH_KEY_EVENTS),
                posted_chars,
            });
        }
    };
    let (delivered_chars, unconfirmed) = web_shortfall_is_unknown(
        delivered_chars,
        unconfirmed,
        text.chars().count(),
        web_target,
    );
    Ok(TypeTextDelivery::Typed(TypeTextOutcome {
        detail: format!(" via CGEvent ({delay_ms}ms delay)"),
        path: PATH_KEY_EVENTS,
        verified,
        delivered_chars,
        unconfirmed,
    }))
}

/// Whether an AX text write's result is read back before anything else is
/// sent. Only an error that proves the app refused the write before acting
/// skips the read-back; a timeout or a generic failure may hide a write that
/// took effect, and typing the text after it would insert it twice (item 25).
fn read_back_ax_write(err: crate::ax::bindings::AXError) -> bool {
    err == kAXErrorSuccess || !crate::input::ax_actions::ax_write_rejected(err)
}

/// Give an addressed web page field keyboard focus after the AXFocused write
/// (already sent). A background Safari page can ignore that write; its press
/// action focuses the field the way `click {element_token}` does. The press
/// is sent only when the write left focus elsewhere, and focus is proven
/// either way before any key.
pub(super) fn focus_web_field(pid: i32, element: AXUIElementRef, window_id: Option<u32>) -> bool {
    if await_addressed_focus(pid, element, window_id) {
        return true;
    }
    // A press of anything but a text field could submit, navigate or delete.
    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let presses = is_text_entry_role(&role)
        && unsafe { crate::ax::bindings::copy_action_names(element) }
            .iter()
            .any(|action| action == "AXPress");
    presses
        && unsafe { crate::ax::bindings::perform_action(element, "AXPress") } == kAXErrorSuccess
        && await_addressed_focus(pid, element, window_id)
}

/// The web page text field at a pixel hit: the hit element or one of its
/// three nearest ancestors with a text-entry role (a hit usually lands on the
/// field's placeholder text), in web content outside Electron. Retained; the
/// caller releases it.
pub(super) fn web_text_field_at(
    pid: i32,
    hit: AXUIElementRef,
    window_id: u32,
) -> Option<AXUIElementRef> {
    if crate::browser::electron_js::ElectronJs::is_electron(pid) {
        return None;
    }
    unsafe {
        CFRetain(hit as CFTypeRef);
        let mut current = hit;
        for _ in 0..4 {
            let role = copy_string_attr(current, "AXRole").unwrap_or_default();
            if is_text_entry_role(&role) {
                if target_in_web_area(pid, Some((current as usize, None)), Some(window_id)) {
                    return Some(current);
                }
                break;
            }
            let parent = crate::ax::bindings::copy_element_attr(current, "AXParent");
            CFRelease(current as CFTypeRef);
            current = parent?;
        }
        CFRelease(current as CFTypeRef);
        None
    }
}

enum FocusedWebField {
    /// A web page text field (retained) holds the window's focus.
    Field(crate::ax::snapshot::RetainedElement),
    /// No text field in the page holds focus: the page itself, or a
    /// control such as a link or button.
    PageOnly,
    /// Not web content, or no readable focus.
    Other,
}

/// The window's focused element, once a just-clicked page has reported it
/// (see [`settle_web_area_focus`]), classified for untargeted typing.
fn focused_web_field(pid: i32, window_id: u32) -> FocusedWebField {
    settle_web_area_focus(pid, window_id);
    unsafe {
        let Some(focused) = crate::ax::exact_target::focused_element_in_window(pid, window_id) else {
            return FocusedWebField::Other;
        };
        let role = copy_string_attr(focused, "AXRole").unwrap_or_default();
        let electron = crate::browser::electron_js::ElectronJs::is_electron(pid);
        let outcome = if role == "AXWebArea" && !electron {
            FocusedWebField::PageOnly
        } else if electron
            && role != "AXWebArea"
            && target_in_web_area(pid, Some((focused as usize, None)), Some(window_id))
            && !takes_text(&role, {
                let editable = crate::ax::bindings::copy_element_attr(focused, "AXEditableAncestor");
                editable.map(|e| CFRelease(e as CFTypeRef)).is_some()
            })
        {
            // A focused Electron page control (a select-only combo box, a
            // button) takes keys as its own commands: Chromium ignored the AX
            // text write these used to get, keys would act (item 42). The
            // page itself keeps today's keys, which reach its focused field.
            FocusedWebField::PageOnly
        } else if !is_text_entry_role(&role)
            && !electron
            && target_in_web_area(pid, Some((focused as usize, None)), Some(window_id))
        {
            // A focused link or button takes keys as shortcuts too; an element
            // inside an editable region (a rich editor's own node) is text.
            match crate::ax::bindings::copy_element_attr(focused, "AXEditableAncestor") {
                Some(editable) => {
                    CFRelease(editable as CFTypeRef);
                    FocusedWebField::Other
                }
                None => FocusedWebField::PageOnly,
            }
        } else if is_text_entry_role(&role) {
            match web_text_field_at(pid, focused, window_id) {
                Some(field) => {
                    let retained = crate::ax::snapshot::RetainedElement::retain(field as usize);
                    CFRelease(field as CFTypeRef);
                    FocusedWebField::Field(retained)
                }
                None => FocusedWebField::Other,
            }
        } else {
            FocusedWebField::Other
        };
        CFRelease(focused as CFTypeRef);
        outcome
    }
}

const WEB_PAGE_HAS_NO_FOCUSED_FIELD: &str = "web_page_has_no_focused_field";

fn web_page_has_no_focused_field_result() -> ToolResult {
    let reason = "No text field in this web page has keyboard focus (the page itself or a \
                  control such as a link has it), so typed keys would act as the page's \
                  shortcuts. No key was sent. Next: type_text \
                  with the field's element_token (or x,y on the field).";
    ToolResult::error(format!("type_text refused ({WEB_PAGE_HAS_NO_FOCUSED_FIELD}): {reason}"))
        .with_structured(serde_json::json!({
            "code": WEB_PAGE_HAS_NO_FOCUSED_FIELD,
            "effect": "refused",
            "retryable": true,
            "delivered_chars": 0,
            "reason": reason,
        }))
}

/// The web page text field under window pixel `(x, y)` (native capture
/// pixels) of `window_id`, retained, when the hit belongs to that window.
fn web_text_field_at_window_pixel(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
) -> Option<crate::ax::snapshot::RetainedElement> {
    let frame = super::px_frame::resolve_window_px_frame(window_id).ok()?;
    let (screen_x, screen_y, _, _) = frame.to_screen(x, y);
    unsafe {
        let hit = crate::ax::bindings::element_at_screen_position(pid, screen_x, screen_y)?;
        let field = (crate::ax::exact_target::element_window_id(hit) == Some(window_id))
            .then(|| web_text_field_at(pid, hit, window_id))
            .flatten();
        CFRelease(hit as CFTypeRef);
        let field = field?;
        let retained = crate::ax::snapshot::RetainedElement::retain(field as usize);
        CFRelease(field as CFTypeRef);
        Some(retained)
    }
}

/// A page that was just clicked (`click {element_token}` before an untargeted
/// type_text) reports the field's focus a moment after the press returns;
/// until then the window's focused element is the web area itself. Wait
/// briefly for it to move, so the read-back watches the field the keys reach
/// rather than reporting it unreadable.
fn settle_web_area_focus(pid: i32, window_id: u32) {
    crate::ax::enablement::refresh_page(pid, window_id);
    let deadline = std::time::Instant::now() + WEB_FOCUS_TIMEOUT;
    loop {
        let role = unsafe {
            crate::ax::exact_target::focused_element_in_window(pid, window_id).map(|element| {
                let role = copy_string_attr(element, "AXRole");
                CFRelease(element as CFTypeRef);
                role
            })
        };
        if role.flatten().as_deref() != Some("AXWebArea") || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(DELIVERY_DRAIN_POLL_INTERVAL);
    }
}

/// Wait briefly for `element` to become its window's focused element after a
/// focus write: a page moves focus from its own event loop, not within the
/// write.
fn await_addressed_focus(pid: i32, element: AXUIElementRef, window_id: Option<u32>) -> bool {
    let deadline = std::time::Instant::now() + WEB_FOCUS_TIMEOUT;
    loop {
        if addressed_element_has_focus(pid, element, window_id) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(DELIVERY_DRAIN_POLL_INTERVAL);
    }
}

// Short: the background wait runs inside the focus preparation's 2 s bound.
const WEB_FOCUS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether `element` is the focused element of its window (`window_id`), or
/// of the process when no window is given.
fn addressed_element_has_focus(pid: i32, element: AXUIElementRef, window_id: Option<u32>) -> bool {
    let Some(wid) = window_id else {
        return crate::input::ax_actions::is_element_focused(pid, element as usize);
    };
    unsafe {
        let Some(focused) = crate::ax::exact_target::focused_element_in_window(pid, wid) else {
            return false;
        };
        let same = CFEqual(focused as CFTypeRef, element as CFTypeRef) != 0;
        CFRelease(focused as CFTypeRef);
        same
    }
}

/// Key events into web content (every key posted) whose AXValue shows none
/// or only part of the text: the page may have taken every key (a combobox
/// moves accessibility focus to a suggestion, AX lags the renderer: a
/// background Safari page under load showed 4 of 24 characters at the end of
/// the drain and all 24 a moment later), so "0 delivered" or "delivered 4,
/// retry the rest" would be a false count, and the retry would type the
/// suffix twice. Report it as unknown instead.
fn web_shortfall_is_unknown(
    delivered_chars: Option<usize>,
    unconfirmed: Option<Unconfirmed>,
    requested_chars: usize,
    web_content: bool,
) -> (Option<usize>, Option<Unconfirmed>) {
    match delivered_chars {
        Some(0) if web_content => (None, Some(Unconfirmed::Unchanged)),
        Some(n) if web_content && n < requested_chars => (None, Some(Unconfirmed::Lagging)),
        _ => (delivered_chars, unconfirmed),
    }
}

fn is_text_entry_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
}

/// Whether an AX text write is the first rung for a target with AX `role`.
/// `implicit`: the window's focused element rather than an addressed one.
/// `web_page`: the target is in web content (a browser page or an Electron
/// app's page).
fn ax_text_write_first(role: &str, implicit: bool, web_page: bool) -> bool {
    !web_page && (is_text_entry_role(role) || !implicit)
}

/// A Chromium-family browser, not an Electron app (Electron keeps its own
/// AX rules above).
fn is_chromium_browser_pid(pid: i32) -> bool {
    crate::browser::platform::is_chromium(
        &apps::get_app_name_for_pid(pid).unwrap_or_default(),
        &apps::bundle_id_for_pid(pid).unwrap_or_default(),
    ) && !crate::browser::electron_js::ElectronJs::is_electron(pid)
}

#[cfg(test)]
mod tests {
    /// Item 25 (review): a moved caret seen once in the drain is kept, even
    /// when the last reading has no caret.
    #[test]
    fn a_moved_caret_seen_during_the_drain_is_not_forgotten() {
        use super::{await_typed_progress_with_selection, TextSelectionRange, TypedProgress};
        let caret = |location| Some(TextSelectionRange { location, length: 0 });
        let mut readings = vec![
            (Some("ab".to_owned()), caret(5)),
            (Some("ab".to_owned()), None),
        ]
        .into_iter();
        let mut last = (Some("ab".to_owned()), None);
        let progress = await_typed_progress_with_selection(
            Some("ab"),
            caret(2),
            "xyz",
            std::time::Instant::now() + std::time::Duration::from_millis(60),
            || {
                if let Some(next) = readings.next() {
                    last = next;
                }
                last.clone()
            },
        );
        assert_eq!(progress, TypedProgress::Unverifiable);
    }

    /// Item 25: an AX text write that errored without proving a refusal is
    /// read back like an accepted one; only a proven refusal goes straight
    /// to keys.
    #[test]
    fn an_indeterminate_ax_write_error_is_read_back_before_any_keys() {
        use super::read_back_ax_write;
        const CANNOT_COMPLETE: i32 = -25204;
        const FAILURE: i32 = -25200;
        const ATTRIBUTE_UNSUPPORTED: i32 = -25205;
        const ILLEGAL_ARGUMENT: i32 = -25201;
        assert!(read_back_ax_write(0));
        assert!(read_back_ax_write(CANNOT_COMPLETE), "a timed-out write may have landed");
        assert!(read_back_ax_write(FAILURE), "a generic failure may have landed");
        assert!(!read_back_ax_write(ATTRIBUTE_UNSUPPORTED));
        assert!(!read_back_ax_write(ILLEGAL_ARGUMENT));
    }

    #[test]
    fn an_unchanged_web_value_is_unknown_not_zero() {
        use super::web_shortfall_is_unknown;
        use super::Unconfirmed::{Lagging, Unchanged, Unreadable};
        assert_eq!(web_shortfall_is_unknown(Some(0), None, 5, true), (None, Some(Unchanged)));
        assert_eq!(web_shortfall_is_unknown(Some(0), None, 5, false), (Some(0), None));
        // A web prefix is not a count to retry from: the page may still be taking keys.
        assert_eq!(web_shortfall_is_unknown(Some(3), None, 5, true), (None, Some(Lagging)));
        assert_eq!(web_shortfall_is_unknown(Some(3), None, 5, false), (Some(3), None));
        assert_eq!(web_shortfall_is_unknown(Some(5), None, 5, true), (Some(5), None));
        assert_eq!(
            web_shortfall_is_unknown(None, Some(Unreadable), 5, true),
            (None, Some(Unreadable))
        );
    }

    /// Item 42: an Electron page element addressed by token is focused and
    /// typed into only when it is a field; a button or the page is refused.
    #[test]
    fn only_fields_take_token_typing_on_an_electron_page() {
        assert!(takes_text("AXTextArea", false));
        assert!(takes_text("AXTextField", true));
        assert!(takes_text("AXStaticText", true), "a rich editor's own text node");
        assert!(!takes_text("AXButton", false));
        assert!(!takes_text("AXWebArea", true));
        assert!(!takes_text("AXGroup", false));
        assert!(!takes_text("AXComboBox", false), "a select-only menu button");
        assert!(takes_text("AXComboBox", true));
    }

    /// Item 42: untargeted keys from behind go to the field the call began
    /// with, or, when it began with the page focused, to the page or a field;
    /// never to a control, a different field, or a page that took the focus
    /// away from the field.
    #[test]
    fn untargeted_web_keys_follow_the_field_the_call_began_with() {
        assert!(untargeted_web_keys_allowed("AXTextArea", false, Some(true)));
        assert!(!untargeted_web_keys_allowed("AXTextArea", false, Some(false)), "another field");
        assert!(!untargeted_web_keys_allowed("AXWebArea", false, Some(false)), "the field lost focus to the page");
        assert!(!untargeted_web_keys_allowed("AXComboBox", false, Some(true)), "a select-only control");
        assert!(untargeted_web_keys_allowed("AXWebArea", false, None));
        assert!(untargeted_web_keys_allowed("AXTextArea", false, None));
        assert!(!untargeted_web_keys_allowed("AXButton", false, None));
    }

    /// Item 42: a line break into a multi-line web field is Shift+Return;
    /// a single-line field, or anywhere else, keeps the character.
    #[test]
    fn web_content_line_breaks_are_shift_return() {
        use crate::input::keyboard::LineBreaks;
        assert_eq!(line_breaks_for(true, "AXTextArea"), LineBreaks::ShiftReturn);
        for role in ["AXTextField", "AXSearchField", "AXComboBox", "AXWebArea", ""] {
            assert_eq!(line_breaks_for(true, role), LineBreaks::Characters, "{role}");
        }
        assert_eq!(line_breaks_for(false, "AXTextArea"), LineBreaks::Characters);
    }

    #[test]
    fn chromium_pages_and_non_text_focus_skip_the_ax_write() {
        use super::ax_text_write_first;
        // Native text controls, addressed or focused, keep the AX rung.
        assert!(ax_text_write_first("AXTextField", false, false));
        assert!(ax_text_write_first("AXTextArea", true, false));
        // A Chromium page field goes to key events either way.
        assert!(!ax_text_write_first("AXTextField", false, true));
        assert!(!ax_text_write_first("AXComboBox", true, true));
        // Focus on a web area or a button is not a text target.
        assert!(!ax_text_write_first("AXWebArea", true, false));
        assert!(!ax_text_write_first("AXButton", true, false));
        // An addressed element keeps the caller's choice.
        assert!(ax_text_write_first("AXGroup", false, false));
    }

    #[test]
    fn typing_insertion_samples_selection_after_existing_focus_preparation() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        scope.value(0, Some("hello"));
        scope.range(0, 5, 0);
        scope.focus(Some(1), 42);
        let readback =
            TypingReadback::capture(-9880, Some((scope.element_ptr(0), Some(7))), Some(42));
        assert_eq!(
            readback.before_range, None,
            "an unfocused range is not an active caret"
        );
        scope.focus(Some(0), 42);
        let (before, selection) = readback.sample();
        assert_eq!(
            selection,
            Some(TextSelectionRange {
                location: 5,
                length: 0
            })
        );
        scope.value(0, Some("helloh"));
        scope.range(0, 6, 0);
        assert_eq!(
            await_typed_progress_with_selection(
                before.as_deref(),
                selection,
                "hello",
                std::time::Instant::now(),
                || readback.sample()
            ),
            TypedProgress::Partial(1)
        );
    }

    #[test]
    fn typing_insertion_waits_for_an_ambiguous_old_value_to_resolve() {
        let mut values = [Some("hello".to_owned()), Some("hellohello".to_owned())].into_iter();
        let mut reads = 0;
        assert_eq!(
            await_typed_progress(
                Some("hello"),
                "hello",
                std::time::Instant::now() + std::time::Duration::from_secs(1),
                || {
                    reads += 1;
                    values.next().flatten()
                }
            ),
            TypedProgress::Complete
        );
        assert_eq!(reads, 2);
    }

    #[test]
    fn typing_insertion_drain_preserves_an_observed_selected_deletion() {
        assert_eq!(
            await_typed_progress_with_selection(
                Some("old"),
                Some(TextSelectionRange {
                    location: 0,
                    length: 3
                }),
                "new",
                std::time::Instant::now(),
                || (
                    Some(String::new()),
                    Some(TextSelectionRange {
                        location: 0,
                        length: 0
                    })
                )
            ),
            TypedProgress::Partial(0),
            "an applied edit must not enter another input route as unchanged"
        );
    }

    #[test]
    fn typing_insertion_preexisting_value_does_not_prove_new_input() {
        assert_eq!(
            typed_progress(Some("hello"), Some("hello"), "hello"),
            TypedProgress::Unverifiable,
            "unchanged text cannot distinguish dropped input from identical replacement"
        );
    }

    #[test]
    fn typing_insertion_existing_payload_does_not_hide_a_prefix() {
        assert_ne!(
            typed_progress(Some("hello"), Some("helloh"), "hello"),
            TypedProgress::Complete,
            "the old payload is still present after only the first new character"
        );
    }

    #[test]
    fn typing_insertion_old_suffix_cannot_complete_a_new_prefix() {
        assert_ne!(
            typed_progress(Some("llo"), Some("hello"), "hello"),
            TypedProgress::Complete,
            "inserting only he before the old llo produces the full string too"
        );
    }

    #[test]
    fn typing_insertion_unrelated_growth_does_not_supply_a_retry_offset() {
        assert_eq!(
            typed_progress(Some("old"), Some("oldXYZ"), "hello"),
            TypedProgress::Unverifiable,
            "three additional characters are not necessarily three delivered characters"
        );
    }

    #[test]
    fn typing_insertion_unknown_before_is_not_observed_delivery() {
        assert_eq!(
            typed_progress(None, Some("hello"), "hello"),
            TypedProgress::Unverifiable
        );
    }

    #[test]
    fn typing_insertion_full_duplicate_append_is_observable() {
        assert_eq!(
            typed_progress(Some("hello"), Some("hellohello"), "hello"),
            TypedProgress::Complete
        );
    }

    fn focused_delivery(readback: &TypingReadback) -> (bool, Option<usize>) {
        await_typed_delivery(
            readback.before.as_deref(),
            "hello",
            std::time::Instant::now(),
            || readback.read(),
        )
    }

    #[test]
    fn typing_identity_does_not_confirm_a_different_focused_field() {
        for window in [Some(42), None] {
            let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
            let readback = TypingReadback::capture(-9880, None, window);
            assert_eq!(readback.before.as_deref(), Some(""));
            scope.focus(Some(1), 42);
            assert_eq!(focused_delivery(&readback), (false, None));
        }
    }

    #[test]
    fn typing_identity_does_not_count_another_fields_length_as_delivery() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        let readback = TypingReadback::capture(-9880, None, Some(42));
        scope.value(1, Some("unrelated pre-existing contents"));
        scope.focus(Some(1), 42);
        assert_eq!(focused_delivery(&readback), (false, None));
    }

    #[test]
    fn typing_identity_replacement_requires_fresh_observation() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        let readback = TypingReadback::capture(-9880, None, Some(42));
        scope.value(0, None);
        scope.focus(Some(1), 42);
        assert_eq!(focused_delivery(&readback), (false, None));
    }

    #[test]
    fn typing_identity_preserves_stable_complete_partial_and_zero_readback() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        let readback = TypingReadback::capture(-9880, None, Some(42));
        assert_eq!(focused_delivery(&readback), (false, Some(0)));
        scope.value(0, Some("hel"));
        assert_eq!(focused_delivery(&readback), (false, Some(3)));
        scope.value(0, Some("hello"));
        assert_eq!(focused_delivery(&readback), (true, Some(5)));
    }

    #[test]
    fn typing_identity_rejects_missing_or_sibling_focus() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        let readback = TypingReadback::capture(-9880, None, Some(42));
        scope.focus(None, 42);
        assert_eq!(focused_delivery(&readback), (false, None));
        scope.focus(Some(1), 99);
        assert_eq!(focused_delivery(&readback), (false, None));
    }

    #[test]
    fn typing_identity_cannot_confirm_focus_that_was_missing_at_start() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        scope.focus(None, 42);
        let readback = TypingReadback::capture(-9880, None, Some(42));
        scope.focus(Some(1), 42);
        assert_eq!(focused_delivery(&readback), (false, None));
    }

    #[test]
    fn typing_identity_discards_a_value_sample_if_focus_changes_during_read() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        let readback = TypingReadback::capture(-9880, None, Some(42));
        scope.value(0, Some("hello"));
        scope.switch_on_next_read();
        assert_eq!(focused_delivery(&readback), (false, None));
    }

    #[test]
    fn typing_identity_keeps_addressed_readback_bound_to_its_retained_element() {
        let scope = crate::ax::bindings::test_support::TypingFocusScope::install();
        let readback =
            TypingReadback::capture(-9880, Some((scope.element_ptr(0), Some(7))), Some(42));
        scope.focus(Some(1), 42);
        assert_eq!(focused_delivery(&readback), (false, Some(0)));
        scope.value(0, Some("hello"));
        assert_eq!(focused_delivery(&readback), (true, Some(5)));
    }


    fn summary_of(result: &ToolResult) -> &str {
        let cua_driver_core::protocol::Content::Text { text, .. } = &result.content[0] else {
            panic!("typing result must include a summary")
        };
        text
    }

    /// Every completed outcome kind maps to one summary prefix and effect: the
    /// summary's first words state the evidence, and nothing says "Sent".
    #[test]
    fn typing_summaries_state_the_evidence() {
        use Unconfirmed::*;
        let outcome = |path, verified, delivered_chars, unconfirmed| TypeTextOutcome {
            detail: " into [1] AXTextArea \"\"".into(),
            path,
            verified,
            delivered_chars,
            unconfirmed,
        };
        // (outcome, web target) -> summary prefix, effect, words of the reason
        let cases = [
            (
                (outcome(PATH_AX, true, Some(5), None), false),
                "✅ Inserted 5 char(s) into [1] AXTextArea",
                "confirmed",
                "",
            ),
            (
                (outcome(PATH_AX, false, None, Some(Unreadable)), false),
                UNCONFIRMED_PREFIX,
                "unverifiable",
                "accepted the accessibility write, but the field's text cannot be read",
            ),
            (
                (outcome(PATH_AX, false, None, Some(Unchanged)), false),
                UNCONFIRMED_PREFIX,
                "unverifiable",
                "accepted the accessibility write, but the field's text reads unchanged",
            ),
            (
                (outcome(PATH_KEY_EVENTS, false, None, Some(UnreadableBefore)), false),
                UNCONFIRMED_PREFIX,
                "unverifiable",
                "the keys were sent, but the field's text was unreadable before typing",
            ),
            (
                (outcome(PATH_KEY_EVENTS_FG, false, None, Some(Mismatch)), false),
                UNCONFIRMED_PREFIX,
                "unverifiable",
                "the keys were sent, but the field's text changed, but not by exactly",
            ),
            (
                (outcome(PATH_KEY_EVENTS_FG, false, None, Some(NoReadback)), false),
                UNCONFIRMED_PREFIX,
                "unverifiable",
                "remote Mac",
            ),
            // A positive AX read-back in web content is not renderer evidence.
            (
                (outcome(PATH_AX, true, Some(5), None), true),
                UNCONFIRMED_PREFIX,
                "unverifiable",
                "not proof that the page received the input",
            ),
        ];
        for ((outcome, web), prefix, effect, reason) in cases {
            let result =
                completed_typing_result(outcome, 5, web, WebSurface::ChromiumBrowser, false, " (window unchanged)");
            let text = summary_of(&result);
            let data = result.structured_content.as_ref().unwrap();
            assert!(text.starts_with(prefix), "{text}");
            assert!(text.contains(reason), "{text}");
            assert!(text.ends_with("(window unchanged)"), "{text}");
            assert!(!text.contains("Sent"), "{text}");
            assert_eq!(data["effect"], effect, "{text}");
            assert_eq!(data["verified"], effect == "confirmed", "{text}");
            if effect != "confirmed" {
                assert!(text.contains("before typing again"), "{text}");
                assert!(text.contains("duplicate"), "{text}");
            }
            if !web {
                // An unknown native edit gives no evidence that another route is needed.
                assert!(data.get("escalation").is_none(), "{data}");
            }
        }
    }

    /// Background keys that missed a Catalyst field: nothing landed, and the
    /// next route is named exactly (not "retry the remaining suffix").
    #[test]
    fn catalyst_keys_that_missed_name_the_foreground_route() {
        let result = catalyst_background_keys_not_landed(7, Some(42), 7, PATH_KEY_EVENTS);
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "catalyst_background_keys_not_landed");
        assert_eq!((data["delivered_chars"].as_u64(), data["escalation"]["recommended"].as_str()), (Some(0), Some("foreground")));
        let reason = data["escalation"]["reason"].as_str().unwrap();
        for needed in ["0 of 7", "nothing needs undoing", "delivery_mode \"foreground\""] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
        assert!(!reason.contains("suffix"), "{reason}");
    }

    #[test]
    fn catalyst_text_views_are_detected_by_their_ios_content_group() {
        let chain = |links: &[(&str, &str)]| -> Vec<(String, String)> {
            links.iter().map(|(r, s)| (r.to_string(), s.to_string())).collect()
        };
        let window = ("AXWindow", "AXStandardWindow");
        let content = ("AXGroup", "iOSContentGroup");
        // Chains read on macOS 26 (checks/axq ancestors): the Catalyst probe's
        // UITextView and UITextField, and the Stocks search field.
        assert!(is_catalyst_text_view(&chain(&[("AXTextArea", ""), ("AXGroup", ""), content, window])));
        assert!(is_catalyst_text_view(&chain(&[("AXTextField", ""), ("AXGroup", ""), content, window])));
        assert!(is_catalyst_text_view(&chain(&[
            ("AXTextField", "AXSearchField"),
            ("AXGroup", ""),
            ("AXGroup", ""),
            ("AXGroup", ""),
            content,
            window,
        ])));
        // TextEdit (AppKit): no content group.
        assert!(!is_catalyst_text_view(&chain(&[("AXTextArea", ""), ("AXScrollArea", ""), window])));
        // A Catalyst button is not a text view.
        assert!(!is_catalyst_text_view(&chain(&[("AXButton", ""), content, window])));
        // The target itself being the content group, or an unproven chain.
        assert!(!is_catalyst_text_view(&chain(&[content, window])));
        assert!(!is_catalyst_text_view(&chain(&[("AXTextArea", "")])));
        assert!(!is_catalyst_text_view(&[]));

        // Known versus unknown: only a chain that reached the window (or the
        // application) without a content group proves a native field.
        use CatalystText::*;
        assert_eq!(catalyst_text_control(&chain(&[("AXTextField", ""), ("AXGroup", ""), content, window])), Yes);
        assert_eq!(catalyst_text_control(&chain(&[("AXTextArea", ""), ("AXScrollArea", ""), window])), No);
        assert_eq!(catalyst_text_control(&chain(&[("AXTextField", ""), ("AXApplication", "")])), No);
        assert_eq!(catalyst_text_control(&chain(&[("AXButton", ""), ("AXGroup", "")])), No, "not a text control");
        // A parent that could not be read, an unreadable role, a missing chain.
        assert_eq!(catalyst_text_control(&chain(&[("AXTextField", "")])), Unknown);
        assert_eq!(catalyst_text_control(&chain(&[("AXTextField", ""), ("AXGroup", "")])), Unknown);
        assert_eq!(catalyst_text_control(&chain(&[("AXTextField", ""), ("AXGroup", ""), ("", "")])), Unknown);
        assert_eq!(catalyst_text_control(&[]), Unknown);
        // The target's own role unreadable (the live chain reads [("", "")]).
        assert_eq!(catalyst_text_control(&chain(&[("", "")])), Unknown);
        assert!(!is_catalyst_text_view(&chain(&[("", "")])));
        // A content group proves Catalyst even when the chain stops above it.
        assert_eq!(catalyst_text_control(&chain(&[("AXTextField", ""), content])), Yes);
    }

    #[test]
    fn unconfirmed_reason_names_the_missing_readback() {
        use Unconfirmed::*;
        assert_eq!(Unconfirmed::from_readback(Some("a"), None), Unreadable);
        assert_eq!(Unconfirmed::from_readback(None, None), Unreadable);
        assert_eq!(Unconfirmed::from_readback(None, Some("a")), UnreadableBefore);
        assert_eq!(Unconfirmed::from_readback(Some("a"), Some("a")), Unchanged);
        assert_eq!(Unconfirmed::from_readback(Some(""), Some("Cua probe")), Mismatch);
    }

    #[test]
    fn unknown_web_typing_keeps_the_renderer_route_hint() {
        let result = completed_typing_result(
            TypeTextOutcome {
                detail: String::new(),
                path: PATH_AX,
                verified: true,
                delivered_chars: None,
                unconfirmed: None,
            },
            3,
            true,
            WebSurface::ChromiumBrowser,
            false,
            "",
        );
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["escalation"]["recommended"], "page");
    }

    #[test]
    fn completed_native_typing_preserves_confirmation_and_count() {
        let result = completed_typing_result(
            TypeTextOutcome {
                detail: String::new(),
                path: PATH_AX,
                verified: true,
                delivered_chars: Some(16),
                unconfirmed: None,
            },
            16,
            false,
            WebSurface::Other,
            false,
            " (window unchanged)",
        );
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["effect"], "confirmed");
        assert_eq!(data["delivered_chars"], 16);
        assert!(data.get("escalation").is_none());
        let cua_driver_core::protocol::Content::Text { text, .. } = &result.content[0] else {
            panic!("typing result must include text")
        };
        assert!(text.ends_with("(window unchanged)"), "{text}");
    }

    use super::*;

    /// A semantic-only policy must refuse the terminal short-circuit before
    /// any CGEvent is posted: terminals have no semantic AX rung, so nothing
    /// safe remains and the carried refusal comes back unchanged.
    #[test]
    fn semantic_only_policy_refuses_terminal_cgevent_rung() {
        let refusal = BackgroundRefusal {
            code: cua_driver_core::background_input::refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY,
            reason: "test".into(),
            advice: None,
        };
        let r = type_text_blocking(
            -1,
            "x",
            None,
            0,
            /*is_terminal_target=*/ true,
            KeyboardRoute {
                background_focus: None,
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: Some(7),
                keyboard_policy: BackgroundKeyboardPolicy::SemanticOnly(refusal.clone()),
                prepare_native_text: false,
                deadline: posting_deadline(),
                proven_web_field: false,
            },
        );
        match r {
            Ok(TypeTextDelivery::Refused(returned)) => assert_eq!(returned, refusal),
            other => panic!("expected a structured refusal, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn accepted_unreadable_ax_write_returns_before_keyboard_fallback() {
        let fixture =
            crate::ax::bindings::test_support::EditorScope::install("AXTextField", None, || {});
        fixture.hide_value_after_write();
        // A large payload guarantees the old fallback stops at its synthesis
        // budget without posting real key events from this unit test.
        let text = "x".repeat(OVERSIZED_CHARS);
        let result = type_text_blocking(
            -9876,
            &text,
            None,
            0,
            false,
            KeyboardRoute {
                background_focus: None,
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: Some(42),
                keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                prepare_native_text: true,
                deadline: posting_deadline(),
                proven_web_field: false,
            },
        )
        .expect("accepted AX write must retain an uncertain outcome");
        let TypeTextDelivery::Typed(outcome) = result else {
            panic!("an accepted unreadable AX write must not enter keyboard fallback");
        };
        assert_eq!(outcome.path, PATH_AX);
        assert!(!outcome.verified);
        assert_eq!(outcome.delivered_chars, None);
    }

    /// A Catalyst text view never gets the AX text write it would ignore. A
    /// focused view (the window's focused element, or an addressed one that
    /// has focus) goes to keys; an addressed view without focus is refused
    /// before any input, on the background and the foreground route alike.
    #[test]
    fn catalyst_text_views_get_keys_when_focused_and_a_refusal_otherwise() {
        use super::super::DeliveryMode::{Background, Foreground};
        // The oversized payload stops the key rung at its synthesis budget, so
        // reaching it posts no real key events from this unit test.
        let text = "x".repeat(OVERSIZED_CHARS);
        for (mode, addressed, focused) in [
            (Background, false, true),
            (Background, true, true),
            (Background, true, false),
            (Foreground, false, true),
            (Foreground, true, true),
            (Foreground, true, false),
        ] {
            let fixture =
                crate::ax::bindings::test_support::EditorScope::install("AXTextArea", None, || {});
            fixture.catalyst();
            if !focused {
                fixture.unfocus();
            }
            let result = type_text_blocking(
                -9876,
                &text,
                addressed.then(|| (fixture.element_ptr(), Some(3))),
                0,
                false,
                KeyboardRoute {
                    background_focus: None,
                    delivery_mode: mode,
                    window_id: Some(42),
                    keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                    prepare_native_text: true,
                    deadline: posting_deadline(),
                    proven_web_field: false,
                },
            )
            .expect("a Catalyst target is decided before any input");
            let case = format!("{mode:?} addressed {addressed} focused {focused}");
            assert_eq!(fixture.value(), "", "no AX text write ({case})");
            match (result, focused) {
                (
                    TypeTextDelivery::SynthesisRefused {
                        ax_attempt: AxAttempt::NotAttempted,
                        ..
                    },
                    true,
                ) => {}
                (TypeTextDelivery::CatalystNeedsFocus, false) => {}
                _ => panic!("{case}: wrong route"),
            }
        }
    }

    #[test]
    fn the_catalyst_focus_refusal_names_the_next_step() {
        let result = catalyst_text_needs_focus_result(7, Some(42));
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "catalyst_text_needs_focus");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["window_id"], 42);
        let text = summary_of(&result);
        assert!(text.starts_with("type_text refused (catalyst_text_needs_focus)"), "{text}");
        assert!(text.contains("click the field"), "{text}");
        assert!(text.contains("Nothing was sent"), "{text}");
    }

    /// Web batch 1: a web page field (an AXWebArea ancestor, outside Electron)
    /// never gets the AX text write a background Safari page accepts and
    /// ignores; the request goes to the key rung, addressed or focused, with
    /// nothing written first (so nothing can land twice).
    #[test]
    fn web_page_fields_get_keys_not_the_ax_write() {
        // The oversized payload stops the key rung at its synthesis budget, so
        // reaching it posts no real key events from this unit test.
        let text = "x".repeat(OVERSIZED_CHARS);
        for addressed in [false, true] {
            let fixture =
                crate::ax::bindings::test_support::EditorScope::install("AXTextField", None, || {});
            fixture.web();
            let element = addressed.then(|| (fixture.element_ptr(), Some(3)));
            let result = type_text_blocking(
                -9876,
                &text,
                element,
                0,
                false,
                KeyboardRoute {
                    background_focus: None,
                    delivery_mode: super::super::DeliveryMode::Background,
                    window_id: Some(42),
                    keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                    prepare_native_text: true,
                    deadline: posting_deadline(),
                    proven_web_field: false,
                },
            )
            .expect("the key rung's budget refusal");
            assert_eq!(fixture.value(), "", "no AX text write (addressed {addressed})");
            assert!(
                matches!(
                    result,
                    TypeTextDelivery::SynthesisRefused {
                        path: PATH_KEY_EVENTS,
                        ax_attempt: AxAttempt::NotAttempted,
                        ..
                    }
                ),
                "addressed {addressed}: the key rung, with no AX attempt"
            );
        }
    }

    /// A focus write the page ignores is followed by one press of the field;
    /// the press is skipped when focus already moved, and a field that takes
    /// neither is reported unfocused (so no key is sent).
    #[test]
    fn an_addressed_web_field_is_pressed_only_when_the_focus_write_did_not_take() {
        use crate::ax::bindings::test_support::EditorScope;
        // (focused at start, press offered, press focuses) -> (focused, presses)
        for (focused, offered, focuses, want, presses) in [
            (true, true, true, true, 0),
            (false, true, true, true, 1),
            (false, true, false, false, 1),
            (false, false, false, false, 0),
        ] {
            let fixture = EditorScope::install("AXTextField", None, || {});
            fixture.web();
            if !focused {
                fixture.unfocus();
            }
            if offered {
                fixture.press(focuses);
            }
            let got = focus_web_field(-9876, fixture.element_ptr() as AXUIElementRef, Some(42));
            let case = format!("focused {focused} offered {offered} focuses {focuses}");
            assert_eq!(got, want, "{case}");
            assert_eq!(fixture.presses(), presses, "{case}");
        }
    }

    #[test]
    fn the_web_focus_refusal_names_the_foreground_call() {
        let result = web_field_not_focused_result(
            serde_json::json!({ "element_token": "s00000003:9", "delay_ms": 0 }),
            "dana@example.com",
            Some("w1"),
            false,
        );
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "web_field_not_focused");
        assert_eq!(data["effect"], "refused");
        assert_eq!(data["delivered_chars"], 0);
        assert_eq!(
            data["next_call"],
            serde_json::json!({
                "tool": "type_text",
                "arguments": {
                    "element_token": "s00000003:9",
                    "delay_ms": 0,
                    "text": "dana@example.com",
                    "delivery_mode": "foreground",
                    "session": "w1",
                },
            })
        );
        let text = summary_of(&result);
        assert!(text.contains("no key was sent"), "{text}");
        // Refused in front too: no foreground call to offer, the pixel route instead.
        let result = web_field_not_focused_result(
            serde_json::json!({ "element_token": "s00000003:9" }),
            "x",
            None,
            true,
        );
        let data = result.structured_content.as_ref().unwrap();
        assert!(data.get("next_call").is_none(), "{data}");
        assert!(summary_of(&result).contains("include_screenshot:true"));
    }

    #[test]
    fn the_retry_target_is_the_callers_own_target_and_pacing() {
        use serde_json::json;
        assert_eq!(
            retry_target(&json!({"element_token": "s1:2", "delay_ms": 0}), 7, None, None, Some(9)),
            json!({"element_token": "s1:2", "delay_ms": 0})
        );
        assert_eq!(retry_target(&json!({"element_token": "s1:2"}), 7, None, None, Some(9)), json!({"element_token": "s1:2"}));
        assert_eq!(
            retry_target(&json!({"delay_ms": 5}), 7, Some(10.0), Some(20.0), Some(9)),
            json!({"pid": 7, "window_id": 9, "x": 10.0, "y": 20.0, "delay_ms": 5})
        );
        assert_eq!(retry_target(&json!({"delay_ms": 5}), 7, None, None, Some(9)), Value::Null);
    }

    /// The key rung itself refuses an addressed web field without focus,
    /// before any key: from behind (focus moved after the preparation) and in
    /// front (where the rung focuses the field itself). A non-text control is
    /// never pressed.
    #[test]
    fn the_key_rung_sends_nothing_to_an_unfocused_web_field() {
        use crate::ax::bindings::test_support::EditorScope;
        for (role, settle_ms, press_offered, presses) in [
            ("AXTextField", 0, true, 0),
            ("AXTextField", 20, true, 1),
            ("AXButton", 20, true, 0),
        ] {
            let fixture = EditorScope::install(role, None, || {});
            fixture.web();
            fixture.unfocus();
            if press_offered {
                fixture.press(false);
            }
            let element = Some((fixture.element_ptr(), Some(3)));
            let readback = TypingReadback {
                pid: -9876,
                element,
                window_id: Some(42),
                focused: None,
                before: Some(String::new()),
                before_range: None,
                began_on_page_field: false,
            };
            let result = cgevent_type_verified(
                -9876,
                -9876,
                "hello",
                0,
                &readback,
                element,
                settle_ms,
                std::time::Instant::now(),
                true,
                crate::input::keyboard::LineBreaks::ShiftReturn,
            );
            let case = format!("{role} settle {settle_ms}");
            let error = result.err().unwrap_or_else(|| panic!("{case}: must refuse"));
            assert!(error.downcast_ref::<WebFieldNotFocused>().is_some(), "{case}: {error:#}");
            assert_eq!(fixture.presses(), presses, "{case}");
            assert_eq!(fixture.value(), "", "{case}");
        }
    }

    /// Untargeted typing into a page: a focused page field is addressed (and
    /// so gets the focus proof), a page with no field focused gets no keys, and
    /// a native field keeps its own route.
    #[test]
    fn untargeted_typing_addresses_the_focused_page_field() {
        use crate::ax::bindings::test_support::EditorScope;
        let fixture = EditorScope::install("AXTextField", None, || {});
        fixture.web();
        match focused_web_field(-9876, 42) {
            FocusedWebField::Field(field) => assert_eq!(field.as_ptr(), fixture.element_ptr()),
            _ => panic!("a focused page field is addressed"),
        }
        fixture.focus_page();
        assert!(matches!(focused_web_field(-9876, 42), FocusedWebField::PageOnly));
        drop(fixture);
        // A focused link or button in a page: its keys would be shortcuts.
        let button = EditorScope::install("AXButton", None, || {});
        button.web();
        assert!(matches!(focused_web_field(-9876, 42), FocusedWebField::PageOnly));
        drop(button);
        let native = EditorScope::install("AXTextField", None, || {});
        assert!(matches!(focused_web_field(-9876, 42), FocusedWebField::Other));
        let hit = native.element_ptr() as AXUIElementRef;
        assert!(web_text_field_at(-9876, hit, 42).is_none(), "a native field is not a page field");
        let result = web_page_has_no_focused_field_result();
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "web_page_has_no_focused_field");
        assert_eq!(data["delivered_chars"], 0);
    }

    /// Safari keys that read back: no px or browser-tool advice (neither
    /// route reaches a WebKit page differently), and the next step is to check
    /// the page instead of typing again.
    #[test]
    fn webkit_typing_points_at_the_page_not_another_route() {
        let result = completed_typing_result(
            TypeTextOutcome {
                detail: " via CGEvent (30ms delay)".into(),
                path: PATH_KEY_EVENTS,
                verified: true,
                delivered_chars: Some(16),
                unconfirmed: None,
            },
            16,
            true,
            WebSurface::Other,
            false,
            "",
        );
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["effect"], "unverifiable");
        assert_eq!(data["delivered_chars"], 16);
        assert!(data.get("escalation").is_none(), "{data}");
        let text = summary_of(&result);
        assert!(text.contains("check the page's own response"), "{text}");
        assert!(!text.contains("px form") && !text.contains("browser_type"), "{text}");
    }

    /// Over the synthesis budget at any supported key gap (2 ms is the
    /// smallest: 4 ms per character at delay 0), so reaching the key rung
    /// refuses instead of posting real key events from a unit test.
    const OVERSIZED_CHARS: usize = 30_000;

    #[test]
    fn oversized_synthesis_is_refused_before_the_terminal_event_path() {
        let text = "x".repeat(OVERSIZED_CHARS);
        let result = type_text_blocking(
            -1,
            &text,
            None,
            0,
            /*is_terminal_target=*/ true,
            KeyboardRoute {
                background_focus: None,
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: None,
                keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                prepare_native_text: true,
                deadline: posting_deadline(),
                proven_web_field: false,
            },
        )
        .expect("preflight refusal must not attempt the invalid pid");
        let TypeTextDelivery::SynthesisRefused {
            path,
            refusal,
            ax_attempt,
        } = result
        else {
            panic!("oversized terminal synthesis must fail before mutation");
        };
        assert_eq!(path, PATH_KEY_EVENTS);
        assert_eq!(ax_attempt, AxAttempt::NotAttempted);
        assert_eq!(refusal.requested_chars, OVERSIZED_CHARS);
        assert_eq!(
            refusal.estimated_duration_ms,
            OVERSIZED_CHARS as u64 * refusal.per_character_ms + 2_000
        );
        // The budget is what is left of the call's deadline when it runs.
        assert!(refusal.budget_ms <= SYNTHESIS_BUDGET_MS, "{refusal:?}");
        assert_eq!(
            refusal.max_chunk_chars as u64,
            (refusal.budget_ms - 2_000) / refusal.per_character_ms
        );
    }

    #[test]
    fn synthesis_preflight_accepts_the_exact_transport_safe_boundary() {
        let budget = SYNTHESIS_BUDGET_MS;
        assert!(synthesis_preflight(TextDeliveryRoute::PidUnicode, 6_125, 0, 8, budget).is_none());
        assert!(synthesis_preflight(TextDeliveryRoute::PidUnicode, 6_126, 0, 8, budget).is_some());
    }

    /// Incident 3: the estimate used a hardcoded 8 ms gap, so with
    /// CUA_DRIVER_KEY_GAP_MS=100 it accepted 1344 characters as 53 s and the
    /// loop ran 188 s, past the client's deadline.
    #[test]
    fn the_preflight_paces_each_route_at_the_configured_gap() {
        use TextDeliveryRoute::*;
        assert_eq!(per_character_ms(PidUnicode, 30, 8), 38);
        assert_eq!(per_character_ms(PidUnicode, 0, 8), 16);
        assert_eq!(per_character_ms(PidUnicode, 30, 100), 130);
        assert_eq!(per_character_ms(DesktopUnicode, 30, 100), 200);
        assert_eq!(per_character_ms(DesktopUnicode, 0, 8), 16);
        assert_eq!(per_character_ms(PhysicalSynthesis, 30, 8), 54);
        assert_eq!(per_character_ms(PhysicalSynthesis, 0, 100), 400);

        assert!(synthesis_preflight(PidUnicode, 1344, 30, 8, SYNTHESIS_BUDGET_MS).is_none());
        let refusal = synthesis_preflight(PidUnicode, 1344, 30, 100, SYNTHESIS_BUDGET_MS)
            .expect("1344 characters at a 100 ms gap cannot finish in the budget");
        assert_eq!(refusal.per_character_ms, 130);
        assert_eq!(refusal.estimated_duration_ms, 1344 * 130 + 2_000);
        assert_eq!(refusal.max_chunk_chars, 98_000 / 130);
        // Time spent queued shrinks the budget: with only the drain left,
        // nothing fits.
        let late = synthesis_preflight(PidUnicode, 2, 30, 8, 2_000).expect("no time to post");
        assert_eq!(late.max_chunk_chars, 0);
    }

    /// Row 4: time spent queued counts against the deadline. A call whose
    /// posting deadline has passed sends nothing.
    #[test]
    fn a_spent_deadline_sends_nothing() {
        let result = type_text_blocking(
            -1,
            "hi",
            None,
            30,
            /*is_terminal_target=*/ true,
            KeyboardRoute {
                background_focus: None,
                delivery_mode: super::super::DeliveryMode::Background,
                window_id: None,
                keyboard_policy: BackgroundKeyboardPolicy::Allowed,
                prepare_native_text: true,
                deadline: std::time::Instant::now(),
                proven_web_field: false,
            },
        )
        .expect("the stop must not touch the invalid pid");
        let TypeTextDelivery::StoppedAtDeadline { path, posted_chars } = result else {
            panic!("a spent deadline must stop before any route runs");
        };
        assert_eq!((path, posted_chars), (None, 0));
    }

    #[test]
    fn a_stop_at_the_deadline_says_posted_and_asks_for_a_read_first() {
        let result = stopped_at_deadline_result(Some(PATH_KEY_EVENTS), 612, 1344);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "type_text_stopped_at_deadline");
        assert_eq!(data["effect"], "partial");
        assert_eq!(data["posted_chars"], 612);
        assert_eq!(data["requested_chars"], 1344);
        assert_eq!(data["retryable"], false);
        assert_eq!(data["path"], PATH_KEY_EVENTS);
        assert!(data.get("retry_from_character").is_none());
        let text = summary_of(&result);
        assert!(text.contains("posting 612 of 1344"), "{text}");
        assert!(text.contains("Read the field"), "{text}");

        let none = stopped_at_deadline_result(None, 0, 5);
        let data = none.structured_content.as_ref().unwrap();
        assert_eq!(data["effect"], "none");
        assert!(data.get("path").is_none());
    }

    #[test]
    fn large_atomic_ax_payloads_are_not_subject_to_the_synthesis_budget() {
        let budget = SYNTHESIS_BUDGET_MS;
        assert!(
            synthesis_preflight(TextDeliveryRoute::AtomicAx, 100_000, 200, 8, budget).is_none()
        );
        assert_eq!(
            typed_progress(Some(""), Some(&"x".repeat(11_500)), &"x".repeat(11_500)),
            TypedProgress::Complete,
            "a successful one-call AX insertion remains eligible regardless of size"
        );
    }

    #[test]
    fn refusal_diagnostics_distinguish_safe_chunking_from_indeterminate_ax() {
        let budget = SYNTHESIS_BUDGET_MS;
        let refusal = synthesis_preflight(TextDeliveryRoute::PidUnicode, 6_500, 0, 8, budget)
            .expect("payload must exceed the synthesis budget");
        let safe = synthesis_refusal_result(PATH_KEY_EVENTS, &refusal, AxAttempt::Rejected);
        let safe = safe.structured_content.expect("structured refusal");
        assert_eq!(safe["code"], "type_text_synthesis_budget_exceeded");
        assert_eq!(safe["effect"], "refused");
        assert_eq!(safe["delivered_chars"], 0);
        assert_eq!(safe["synthesized_chars"], 0);
        assert_eq!(safe["retryable"], true);
        assert_eq!(safe["escalation"]["recommended"], "chunk");

        let indeterminate =
            synthesis_refusal_result(PATH_KEY_EVENTS, &refusal, AxAttempt::Unverifiable);
        let indeterminate = indeterminate
            .structured_content
            .expect("structured indeterminate result");
        assert_eq!(indeterminate["effect"], "indeterminate");
        assert!(indeterminate.get("delivered_chars").is_none());
        assert_eq!(indeterminate["synthesized_chars"], 0);
        assert_eq!(indeterminate["retryable"], false);
        assert_eq!(indeterminate["escalation"]["recommended"], "verify_state");
    }

    #[test]
    fn a_submitted_keystroke_shortfall_offers_no_retry() {
        use TypedProgress::*;
        // A chat field cleared by the typed Return reads as nothing landed.
        let cleared = typed_progress(Some(""), Some(""), "hi\n");
        assert_eq!(
            submitted_shortfall_is_unverifiable(cleared, "hi\n"),
            Unverifiable
        );
        assert_eq!(delivery_from_progress(Unverifiable, "hi\n"), (false, None));
        // A one-line field that kept the text but not the Return.
        let kept = typed_progress(Some(""), Some("hi"), "hi\n");
        assert_eq!(
            submitted_shortfall_is_unverifiable(kept, "hi\n"),
            Unverifiable
        );
        // Complete stays complete; text without submit keys keeps its count.
        assert_eq!(
            submitted_shortfall_is_unverifiable(Complete, "a\tb"),
            Complete
        );
        assert_eq!(
            submitted_shortfall_is_unverifiable(Partial(2), "hello"),
            Partial(2)
        );
        assert_eq!(
            submitted_shortfall_is_unverifiable(Unchanged, "hello"),
            Unchanged
        );
    }

    #[test]
    fn typed_progress_classifies_readback() {
        use TypedProgress::*;
        for (before, after, text, expected) in [
            // Catalyst: can't read AXValue back, so delivery cannot be confirmed.
            (None, None, "hi", Unverifiable),
            (Some(""), None, "hi", Unverifiable),
            (None, Some("h"), "hi", Unverifiable),
            (Some(""), Some("hi"), "hi", Complete),
            (Some("ab"), Some("abhi"), "hi", Complete),
            // A space the caller never typed appeared: not the requested insertion.
            (Some("ab"), Some("ab hi"), "hi", Unverifiable),
            // An observable prefix is partial delivery, never completion.
            (
                Some(""),
                Some("BEGINpayload"),
                "BEGINpayloadEND",
                Partial(12),
            ),
            // Without a caret, an unchanged old value cannot distinguish dropped
            // input from an identical replacement.
            (Some("ab"), Some("ab"), "hi", Unverifiable),
            (None, None, "", Complete),
            // A submitted field rewrites itself; the shortfall proves nothing.
            (Some("A1"), Some("A1xx"), "$Controls.A1\n", Unverifiable),
            (Some(""), Some("one\ntwo"), "one\ntwo", Complete),
        ] {
            assert_eq!(
                typed_progress(before, after, text),
                expected,
                "before={before:?} after={after:?} text={text:?}"
            );
        }
    }

    #[test]
    fn delivery_waits_through_a_partial_readback_until_complete() {
        let text = "BEGIN-payload-END";
        let mut values = std::collections::VecDeque::from([
            Some("BEGIN-payload".to_owned()),
            Some(text.to_owned()),
        ]);
        let mut reads = 0;
        let delivery = await_typed_delivery(
            Some(""),
            text,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            || {
                reads += 1;
                values.pop_front().flatten()
            },
        );
        assert_eq!(delivery, (true, Some(text.chars().count())));
        assert_eq!(reads, 2, "completion must wait past the prefix readback");
    }

    #[test]
    fn drained_prefix_reports_the_delivered_character_count() {
        let delivery = await_typed_delivery(
            Some(""),
            "BEGIN-payload-END",
            std::time::Instant::now(),
            || Some("BEGIN".to_owned()),
        );
        assert_eq!(delivery, (false, Some(5)));
    }

    /// The drain summarises a whole polling window, and the AX rung reads that
    /// summary to decide between reporting a partial insertion and falling
    /// through to keystrokes. A window in which the field never moved must
    /// stay `Unchanged`: `Partial(0)` there would publish "delivered 0 of n"
    /// and swallow the keystroke rung that still had to run.
    #[test]
    fn a_write_the_field_never_took_stays_unchanged() {
        assert_eq!(
            await_typed_progress_with_selection(
                Some("old"),
                Some(TextSelectionRange {
                    location: 3,
                    length: 0
                }),
                "new value",
                std::time::Instant::now(),
                || (
                    Some("old".to_owned()),
                    Some(TextSelectionRange {
                        location: 3,
                        length: 0
                    })
                )
            ),
            TypedProgress::Unchanged
        );
    }

    #[test]
    fn path_constants_are_stable_tokens() {
        // These string constants are part of the structured-response
        // contract; freezing them here makes the contract a unit test.
        assert_eq!(PATH_AX, "ax");
        assert_eq!(PATH_KEY_EVENTS, "key_events");
        assert_eq!(PATH_KEY_EVENTS_FG, "key_events_fg");
    }

    #[test]
    fn ax_backed_web_readbacks_are_downgraded() {
        for path in [PATH_AX, PATH_KEY_EVENTS, PATH_KEY_EVENTS_FG] {
            assert_eq!(
                surface_verification(path, true, true),
                SurfaceVerification {
                    verified: false,
                    untrusted_web_readback: true,
                },
                "path={path}"
            );
        }
    }

    #[test]
    fn web_readback_distrust_preserves_native_and_unverified_outcomes() {
        assert_eq!(
            surface_verification(PATH_KEY_EVENTS_FG, true, false),
            SurfaceVerification {
                verified: true,
                untrusted_web_readback: false,
            },
            "native browser chrome remains eligible for trusted read-back"
        );
        assert_eq!(
            surface_verification(PATH_KEY_EVENTS_FG, false, true),
            SurfaceVerification {
                verified: false,
                untrusted_web_readback: false,
            },
            "an already-unverified delivery is not reclassified as a web echo"
        );
        assert_eq!(
            surface_verification("independent_renderer_oracle", true, true),
            SurfaceVerification {
                verified: true,
                untrusted_web_readback: false,
            },
            "a future independently verified path must not inherit AXValue distrust"
        );
    }

    #[test]
    fn web_readback_escalation_never_recommends_the_completed_pixel_rung() {
        assert_eq!(web_readback_next_rung(WebSurface::Electron, false), Some("px"));
        assert_eq!(web_readback_next_rung(WebSurface::Electron, true), None);
        assert_eq!(web_readback_next_rung(WebSurface::ChromiumBrowser, false), Some("page"));
        assert_eq!(web_readback_next_rung(WebSurface::ChromiumBrowser, true), Some("page"));
    }

    #[test]
    fn electron_web_ax_background_refuses_before_synthesis() {
        let refusal = electron_background_ax_refusal(
            crate::tools::DeliveryMode::Background,
            true,
            false,
            true,
        )
        .expect("Electron AX background typing must refuse");
        assert_eq!(refusal.code, "background_unavailable");
        assert_eq!(refusal.advice, Some("px"));

        assert!(electron_background_ax_refusal(
            crate::tools::DeliveryMode::Foreground,
            true,
            false,
            true,
        )
        .is_none());
        assert!(electron_background_ax_refusal(
            crate::tools::DeliveryMode::Background,
            false,
            true,
            true,
        )
        .is_none());
        assert!(electron_background_ax_refusal(
            crate::tools::DeliveryMode::Background,
            true,
            false,
            false,
        )
        .is_none());
    }

    #[test]
    fn incomplete_web_ancestry_only_fails_closed_for_electron_background_ax() {
        assert!(electron_background_ax_ancestry_is_unsafe(
            WebAreaClassification::Incomplete
        ));
        assert!(electron_background_ax_ancestry_is_unsafe(
            WebAreaClassification::WindowFocusUnavailable
        ));
        assert!(!electron_background_ax_ancestry_is_unsafe(
            WebAreaClassification::NonWebContent
        ));

        assert!(!web_readback_is_untrusted(
            WebAreaClassification::Incomplete
        ));
        assert!(web_readback_is_untrusted(
            WebAreaClassification::WindowFocusUnavailable
        ));
    }

    #[test]
    fn foreground_typing_skips_long_rearm_when_target_is_already_frontmost() {
        assert_eq!(foreground_settle_ms(42, Some(42)), 20);
        assert_eq!(foreground_settle_ms(42, Some(7)), 200);
        assert_eq!(foreground_settle_ms(42, None), 200);
    }

    #[test]
    fn screen_sharing_text_fails_closed_without_foreground_window() {
        for (foreground, window_id) in [(false, None), (false, Some(7)), (true, None)] {
            let result = screen_sharing_delivery_error(true, foreground, window_id)
                .expect("unsafe Screen Sharing route must be refused");
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["code"], "SCREEN_SHARING_REQUIRES_FOREGROUND_HID");
            assert_eq!(structured["effect"], "refused");
            assert_eq!(structured["escalation"]["recommended"], "foreground");
            assert_eq!(structured["escalation"]["requires"][0], "window_id");
        }
        assert!(screen_sharing_delivery_error(true, true, Some(7)).is_none());
        assert!(screen_sharing_delivery_error(false, false, None).is_none());
    }
}
