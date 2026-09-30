//! set_value tool — matches the Swift reference in SetValueTool.swift.
//!
//! Two modes, determined by the element's AXRole:
//!
//! * **AXPopUpButton**: Find the child option whose AXTitle or AXValue matches
//!   `value` (case-insensitive) and AXPress it directly.  The native macOS popup
//!   menu is never opened, so focus is never stolen.  Falls back to Safari
//!   `osascript do JavaScript` for WebKit `<select>` elements that expose no AX
//!   children when the popup is closed.
//!
//! * **Everything else**: Write `AXValue` directly (sliders, steppers, native
//!   text fields that expose a settable AXValue).

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_children, copy_number_attr, copy_string_attr, kAXErrorSuccess, perform_action,
    set_number_attr, set_string_attr, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::CFRelease;

use super::ToolState;

pub struct SetValueTool {
    state: Arc<ToolState>,
}

impl SetValueTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "set_value".into(),
        description:
            "Set an element's value (element_token, or element_index + snapshot_id). A popup or \
             select gets the matching option pressed without opening its menu; other elements get \
             AXValue written (sliders, steppers, date pickers, settable text fields). Web pages \
             ignore value writes: in Chrome use get_browser_state then browser_type, elsewhere \
             type_text."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "value"],
            "properties": {
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid": { "type": "integer", "description": "Target process ID." },
                "window_id": {
                    "type": "integer",
                    "description": "Target window ID; required with element_index, carried by element_token."
                },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "value": {
                    "type": "string",
                    "description": "New value, coerced to the element type; for a popup, the option title or value (case-insensitive)."
                }
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  true,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for SetValueTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let value = match args.require_str("value") {
            Ok(v) => v,
            Err(e) => return e,
        };

        // Surface 6: element_token / element_index precedence. Neither
        // is now schema-required so the resolver can centralize the
        // "missing addressing" error message.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id");
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match self.state.element_cache.resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "set_value",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => {
                return ToolResult::error(
                    "set_value requires element_index (+ window_id) or element_token to \
                     address the target element.",
                )
            }
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: Some(wid),
                element_index: idx,
                element,
                ..
            } => match u32::try_from(wid) {
                Ok(wid) => (idx, wid, element),
                Err(_) => return ToolResult::error("window_id is out of range for macOS."),
            },
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: None, ..
            } => {
                return ToolResult::error(
                    "set_value requires window_id when element_index is used \
                 (omit only when supplying element_token, which carries it).",
                )
            }
        };

        let element_ptr = element_guard.as_ptr();

        if let Some(redirect) = super::browser_route::page_input_redirect(
            "set_value",
            super::browser_route::SET_VALUE_NEXT,
            super::browser_route::Control::Text,
            pid,
            Some(window_id),
            Some(element_ptr as usize),
        )
        .await
        {
            return redirect;
        }

        // set_value is an always-background semantic AX mutation. Re-prove
        // that the retained element still belongs to the requested exact
        // window immediately before any cursor or AX work; a cache hit alone
        // is not delivery proof after a window lifecycle or Space change.
        let _mutation_lease = match super::gate_background_window_action(
            pid,
            window_id,
            Some(element_ptr),
            cua_driver_core::background_input::BackgroundAction::AxSemantic,
        )
        .await
        {
            Ok(lease) => lease,
            Err(refusal_result) => return refusal_result,
        };

        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        let center_guard = element_guard.clone();
        if let Ok(Some((screen_x, screen_y))) = tokio::task::spawn_blocking(move || unsafe {
            crate::ax::bindings::element_screen_center(center_guard.as_ptr() as AXUIElementRef)
        })
        .await
        {
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::PinAbove(window_id as u64),
            );
            crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y, Some(window_id as u64)).await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);
        }
        // An AXValue read-back is not ground truth for web content. Chromium,
        // WebKit, and Electron can echo the write through accessibility while
        // the renderer never observes it. Reuse type_text's bounded ancestor
        // check so native browser chrome stays trusted but rendered content is
        // always reported as unverified.
        let ax_echo_surface = super::type_text::target_in_web_area(
            pid,
            Some((element_ptr, Some(element_index))),
            Some(window_id),
        );

        // A native field can accept AXValue without notifying its delegate
        // until AppKit has installed its field editor. Reuse the existing
        // exact-window visibility gate under the same lease before preparing
        // that editor. The WindowPointer gate proves a visible exact target
        // without requiring a singleton keyboard destination. No pointer or
        // keyboard event is sent: AXFocused addresses the retained element,
        // just as an accessibility click on a native text field already does.
        // Hidden/minimized targets, web content and non-text controls retain
        // their existing behavior.
        let prepare_native_text = !ax_echo_surface
            && matches!(
                unsafe { copy_string_attr(element_ptr as AXUIElementRef, "AXRole") }.as_deref(),
                Some("AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
            )
            && _mutation_lease
                .gate_again(
                    window_id,
                    Some(element_ptr),
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
                )
                .await
                .is_ok();

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // AXValue writes on popups / sliders can cause reflex activations
        // in Chromium-based apps; the AXPopUpButton path also AXPresses a
        // child option which can trigger app activation in some setups.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "set_value.AXValue",
            || async move {
                tokio::task::spawn_blocking(move || {
                    let element_ptr = element_guard.as_ptr();
                    let element = element_ptr as crate::ax::bindings::AXUIElementRef;
                    let role = unsafe { crate::ax::bindings::copy_string_attr(element, "AXRole") }
                        .unwrap_or_default();
                    write_text_control(
                        &role,
                        || unsafe { crate::ax::bindings::attribute_settable(element, "AXValue") },
                        || unsafe { super::type_text::catalyst_text_control_of(element) },
                        || {
                            if prepare_native_text
                                && !crate::input::ax_actions::is_element_focused(pid, element_ptr)
                            {
                                crate::input::ax_actions::focus_element(element_ptr)?;
                            }
                            Ok(())
                        },
                        // Preparation is best effort, not evidence of delivery.
                        // Keep target-bound readback.
                        || set_value_blocking(element_ptr, element_index, pid, &value),
                    )
                })
                .await
            },
        )
        .await;

        let changes = snapshot.detect_async().await;

        match result {
            Ok(Ok(SetValueAttempt::Refused)) => nonsettable_text_refusal(),
            Ok(Ok(SetValueAttempt::CatalystNeedsTyping)) => catalyst_text_needs_typing(pid, window_id),
            Ok(Ok(SetValueAttempt::Applied(mut outcome, catalyst))) => {
                apply_surface_trust(&mut outcome, ax_echo_surface);
                // The caveat rides in the summary, which the public action
                // result keeps.
                apply_catalyst_uncertainty(&mut outcome, catalyst, ax_echo_surface);
                apply_verification_label(&mut outcome);
                let mut msg = outcome.detail;
                msg.push_str(&changes.result_suffix());
                let verified = outcome.verified.unwrap_or(false);
                let mut structured = serde_json::json!({
                    "path": "ax",
                    "verified": verified,
                    "effect": if verified { "confirmed" } else { "unverifiable" },
                });
                if ax_echo_surface {
                    structured["escalation"] = serde_json::json!({
                        "recommended": "px",
                        "reason": "AXValue read-back is not trusted for web content. Verify \
                                   through the renderer; use browser page tools for a tab or \
                                   manipulate the control through its pixel action."
                    });
                }
                ToolResult::text(msg).with_structured(structured)
            }
            Ok(Err(e)) => ToolResult::error(format!("set_value failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

enum SetValueAttempt {
    Refused,
    /// A Mac Catalyst text control: nothing was focused or written.
    CatalystNeedsTyping,
    Applied(SetValueOutcome, super::type_text::CatalystText),
}

fn is_text_control_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
}

/// The ordered route for one set_value on the retained element. Refusals come
/// before any side effect: a read-only text control first (it keeps
/// precedence), then a Mac Catalyst text control. Only then is the field
/// prepared (`prepare_focus`) and the value written.
fn write_text_control(
    role: &str,
    read_settable: impl FnOnce() -> Option<bool>,
    catalyst: impl FnOnce() -> super::type_text::CatalystText,
    prepare_focus: impl FnOnce() -> anyhow::Result<()>,
    write: impl FnOnce() -> anyhow::Result<SetValueOutcome>,
) -> anyhow::Result<SetValueAttempt> {
    use super::type_text::CatalystText;
    if text_value_not_settable(role, read_settable) {
        return Ok(SetValueAttempt::Refused);
    }
    // Only text controls pay for the ancestry read.
    let catalyst = if is_text_control_role(role) { catalyst() } else { CatalystText::No };
    if catalyst == CatalystText::Yes {
        return Ok(SetValueAttempt::CatalystNeedsTyping);
    }
    prepare_focus()?;
    write().map(|outcome| SetValueAttempt::Applied(outcome, catalyst))
}

const CATALYST_TEXT_NEEDS_TYPING: &str = "catalyst_text_needs_typing";

/// The refusal for set_value on a Mac Catalyst text control.
fn catalyst_text_needs_typing(pid: i32, window_id: u32) -> ToolResult {
    let reason = "This is a Mac Catalyst text field. Catalyst apps can take an accessibility \
                  value write without reacting to it (Messages search does not search), and \
                  the value read-back cannot tell the difference, so nothing was written. \
                  Next: click the field and confirm it is focused (type_text refuses with \
                  catalyst_text_needs_focus when it is not), select all (hotkey cmd+a) if \
                  replacing, then type_text on it. Then check the app's own result (for a \
                  search field, that its results changed).";
    ToolResult::error(format!("set_value refused ({CATALYST_TEXT_NEEDS_TYPING}): {reason}"))
        .with_structured(serde_json::json!({
            "code": CATALYST_TEXT_NEEDS_TYPING,
            "effect": "refused",
            "path": "ax",
            "pid": pid,
            "window_id": window_id,
            "reason": reason,
        }))
}

/// A text control whose Catalyst ancestry could not be read keeps today's
/// write, but a matching read-back then proves only the accessibility value,
/// not that the app reacted. Returns whether that caveat applies.
fn apply_catalyst_uncertainty(
    outcome: &mut SetValueOutcome,
    catalyst: super::type_text::CatalystText,
    ax_echo_surface: bool,
) -> bool {
    if catalyst != super::type_text::CatalystText::Unknown || ax_echo_surface {
        return false;
    }
    // Claim the read-back only when it matched; otherwise both the value and
    // the app's reaction stay uncertain.
    let value = if outcome.verified == Some(true) {
        "the read-back confirms the accessibility value, and whether the app itself reacted \
         is unverified"
    } else {
        "neither the accessibility value nor whether the app itself reacted is confirmed"
    };
    outcome.detail.push_str(&format!(
        " The field's ancestry could not be read, so it may be a Mac Catalyst field: {value}. \
         Check the app's own result."
    ));
    true
}

/// A text control that says its AXValue is read-only is not written. This
/// is about AXValue writability, not whether the keyboard could edit it.
fn text_value_not_settable(role: &str, read_settable: impl FnOnce() -> Option<bool>) -> bool {
    matches!(role, "AXTextField" | "AXTextArea") && read_settable() == Some(false)
}

fn nonsettable_text_refusal() -> ToolResult {
    ToolResult::error(
        "Cannot set AXValue: the text control currently reports that its value is not settable. \
         No value write was attempted. This describes AXValue writability, not keyboard editability.",
    )
    .with_structured(serde_json::json!({
        "code": "AX_VALUE_NOT_SETTABLE",
        "effect": "refused",
        "path": "ax",
    }))
}

// ── Blocking implementation (runs on spawn_blocking thread) ─────────────────

/// Outcome of a `set_value` write.
///
/// `verified` is `None` for paths that do not perform a value read-back (the
/// AXPopUpButton path drives menu items rather than writing AXValue), and
/// `Some(false)` when a read-back ran but could not confirm the write. A
/// successful `AXUIElementSetAttributeValue` return code is not by itself
/// evidence that the value landed: web content behind an AXWebArea accepts the
/// write and echoes it back through AXValue while the renderer never observes
/// it — the same trap `type_text` already documents.
struct SetValueOutcome {
    detail: String,
    verified: Option<bool>,
    /// `Some(false)` when the element already held the requested value, so the
    /// write was a no-op. Lets callers distinguish "idempotent" from "applied".
    changed: Option<bool>,
}

fn apply_surface_trust(outcome: &mut SetValueOutcome, ax_echo_surface: bool) {
    if ax_echo_surface && outcome.verified == Some(true) {
        outcome.verified = Some(false);
        outcome.changed = None;
        outcome.detail.push_str(
            " AXValue read-back is not trusted for web content; verify the \
             page via screenshot, or in Chrome use get_browser_state then browser_type.",
        );
    }
}

fn apply_verification_label(outcome: &mut SetValueOutcome) {
    if outcome.verified != Some(true) {
        if let Some(rest) = outcome.detail.strip_prefix("✅ Set") {
            outcome.detail = format!("📨 Sent (unverified){rest}");
        }
    }
}

fn set_value_blocking(
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    value: &str,
) -> anyhow::Result<SetValueOutcome> {
    let element = element_ptr as AXUIElementRef;

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();

    if role == "AXPopUpButton" {
        let element_title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();
        // Menu-item selection, not an AXValue write — no read-back to report.
        select_popup_option(element, element_index, pid, value, &element_title).map(|detail| {
            SetValueOutcome {
                detail,
                verified: None,
                changed: None,
            }
        })
    } else {
        // Default path: write AXValue directly. Numeric controls (AXSlider /
        // AXStepper) reject a CFString with -25201 and need a CFNumber; text
        // fields take a CFString. Try numeric first when the value parses as a
        // number, then fall back to a string write.
        // Numeric target carried through so we can step toward it if the
        // direct writes are rejected (SwiftUI AXSlider rejects every AXValue
        // write with -25200 yet exposes a readable AXValue + increment/decrement
        // actions).
        let numeric_target = value.trim().parse::<f64>().ok();
        // Read the value before writing so an unchanged field can be reported as
        // idempotent rather than silently indistinguishable from a fresh write.
        let before = unsafe { copy_string_attr(element, "AXValue") };
        let err = match numeric_target {
            Some(n) => {
                let e = unsafe { set_number_attr(element, "AXValue", n) };
                if e == kAXErrorSuccess {
                    e
                } else {
                    unsafe { set_string_attr(element, "AXValue", value) }
                }
            }
            None => unsafe { set_string_attr(element, "AXValue", value) },
        };
        if err == kAXErrorSuccess {
            let after = unsafe { copy_string_attr(element, "AXValue") };
            let (verified, changed) = classify_write(
                before.as_deref(),
                after.as_deref(),
                value,
                numeric_target.is_some(),
            );
            let suffix = match (verified, changed) {
                (Some(true), Some(false)) => " Value already matched; write was idempotent.",
                (Some(true), _) => "",
                (Some(false), _) => " Read-back did not confirm the value; verify via screenshot.",
                (None, _) => " Value is not readable through AX; could not confirm.",
            };
            Ok(SetValueOutcome {
                detail: format!("✅ Set AXValue on [{element_index}] {role}.{suffix}"),
                verified,
                changed,
            })
        } else if let Some(target) = numeric_target {
            // Both direct writes failed for a numeric target — fall back to
            // stepping the control via AXIncrement / AXDecrement actions.
            if step_to_value(element, target) {
                let after = unsafe { copy_string_attr(element, "AXValue") };
                let (verified, changed) =
                    classify_write(before.as_deref(), after.as_deref(), value, true);
                Ok(SetValueOutcome {
                    detail: format!(
                        "✅ Set AXValue on [{element_index}] {role} via AXIncrement/AXDecrement stepping."
                    ),
                    verified,
                    changed,
                })
            } else {
                anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
            }
        } else {
            anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
        }
    }
}

/// Decide what a post-write AXValue read proves.
///
/// Returns `(verified, changed)`:
/// - `verified = None` when AXValue is not readable at all, so the write can be
///   neither confirmed nor denied.
/// - `verified = Some(true)` when the read-back equals the requested value.
///   Numeric controls are compared numerically so `"25"` matches a slider that
///   reports `"25.0"`.
/// - `changed = Some(false)` when the read-back equals what was there before,
///   i.e. the element's value did not move. Combined with `verified` this
///   separates "already had the requested value" (verified + unchanged) from
///   "the write did not take" (unverified + unchanged).
fn classify_write(
    before: Option<&str>,
    after: Option<&str>,
    requested: &str,
    numeric: bool,
) -> (Option<bool>, Option<bool>) {
    let Some(after) = after else {
        return (None, None);
    };
    let matches = |observed: &str, expected: &str| -> bool {
        if observed == expected {
            return true;
        }
        if !numeric {
            return false;
        }
        match (
            observed.trim().parse::<f64>(),
            expected.trim().parse::<f64>(),
        ) {
            (Ok(a), Ok(b)) => {
                let scale = a.abs().max(b.abs()).max(1.0);
                (a - b).abs() <= 1e-9 * scale
            }
            _ => false,
        }
    };
    let verified = matches(after, requested);
    let changed = before.map(|before| !matches(after, before));
    (Some(verified), changed)
}

// ── AXIncrement / AXDecrement stepping fallback ──────────────────────────────

/// Step a numeric control toward `target` using its `AXIncrement` /
/// `AXDecrement` actions. Used only when direct `AXValue` writes are rejected
/// (notably SwiftUI's `AXSlider`, which exposes a readable-but-unsettable
/// `AXValue` plus increment/decrement actions).
///
/// Returns `true` once the control's value lands within half of the last
/// observed step of `target`, `false` if it can't be read or can't be moved.
fn step_to_value(element: AXUIElementRef, target: f64) -> bool {
    // Can't target precisely without feedback — bail if AXValue is unreadable.
    let mut current = match unsafe { copy_number_attr(element, "AXValue") } {
        Some(v) => v,
        None => return false,
    };

    // Half of the last observed step. Start near-zero so we never declare the
    // target "reached" before performing (and observing) a real
    // AXIncrement/AXDecrement — otherwise a slider at 0.0 targeting 0.5 would
    // report success without ever moving. The radius widens only after we learn
    // the control's actual step size from an observed value change.
    let mut step_radius = f64::EPSILON;

    // Hard cap to prevent runaway on a control that never quite converges.
    for _ in 0..500 {
        if (current - target).abs() <= step_radius {
            return true;
        }

        let action = if current < target {
            "AXIncrement"
        } else {
            "AXDecrement"
        };
        let _ = unsafe { perform_action(element, action) };

        let next = match unsafe { copy_number_attr(element, "AXValue") } {
            Some(v) => v,
            None => return false,
        };

        // The action didn't move the value — the control can't be stepped (or
        // has hit a min/max bound short of target). Stop to avoid looping.
        if next == current {
            return false;
        }

        // Refine the stop threshold to half of the actual step the control took.
        let step = (next - current).abs();
        if step > 0.0 {
            step_radius = step / 2.0;
        }
        current = next;
    }

    // Exhausted the iteration cap without converging.
    (current - target).abs() <= step_radius
}

// ── AXPopUpButton path ───────────────────────────────────────────────────────

fn select_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    value: &str,
    element_title: &str,
) -> anyhow::Result<String> {
    let children = unsafe { copy_children(element) };

    if !children.is_empty() {
        // Strategy 1: AX children (native AppKit NSPopUpButton).
        let value_lower = value.to_lowercase();
        let mut matched_idx: Option<usize> = None;
        let mut available: Vec<String> = Vec::with_capacity(children.len());

        for (i, &child) in children.iter().enumerate() {
            let child_title = unsafe { copy_string_attr(child, "AXTitle") }.unwrap_or_default();
            let child_value = unsafe { copy_string_attr(child, "AXValue") }.unwrap_or_default();
            available.push(child_title.clone());
            if child_title.to_lowercase() == value_lower
                || child_value.to_lowercase() == value_lower
            {
                matched_idx = Some(i);
                break;
            }
        }

        let result = if let Some(i) = matched_idx {
            let child = children[i];
            let opt_title =
                unsafe { copy_string_attr(child, "AXTitle") }.unwrap_or_else(|| value.to_string());
            let err = unsafe { perform_action(child, "AXPress") };
            if err == kAXErrorSuccess {
                Ok(format!(
                    "✅ Selected '{opt_title}' in AXPopUpButton [{element_index}] \
                     \"{element_title}\" via AX child AXPress."
                ))
            } else {
                anyhow::bail!("AXPress on child option failed with error {err}")
            }
        } else {
            let avail = available
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "No AX child matching '{value}' in AXPopUpButton [{element_index}] \
                 \"{element_title}\". Available: [{avail}]"
            )
        };

        // Release children (copy_children retains each one).
        for &child in &children {
            unsafe {
                CFRelease(child as _);
            }
        }

        return result;
    }

    // Strategy 2: Safari/WebKit — no AX children when popup is closed.
    // Use osascript do JavaScript to set the <select> element's DOM value.
    let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();

    if app_name != "Safari" {
        anyhow::bail!(
            "AXPopUpButton [{element_index}] '{element_title}' has no AX children and \
             target is '{app_name}' (not Safari) — no fallback available."
        )
    }

    set_select_via_js(element_index, element_title, value)
}

// ── Safari JavaScript fallback ───────────────────────────────────────────────

/// Set an HTML `<select>` value in Safari via `osascript do JavaScript`.
/// Searches all `<select>` elements for an `<option>` whose text or value matches
/// `value` (case-insensitive), then sets it and dispatches a `change` event.
fn set_select_via_js(
    element_index: usize,
    element_title: &str,
    value: &str,
) -> anyhow::Result<String> {
    // Percent-encode the lowercased value using only unreserved URL characters
    // as the allowed set, matching the Swift reference's percent-encoding approach.
    // This makes the string safe to embed in both a JS single-quoted string
    // (via decodeURIComponent) and an AppleScript double-quoted string.
    let v_low = value.to_lowercase();
    let v_encoded = percent_encode_unreserved(&v_low);

    // JavaScript that matches the Swift reference verbatim.
    let js = format!(
        "(function(){{\
         var v=decodeURIComponent('{v_encoded}');\
         var ss=document.querySelectorAll('select'),opts=[];\
         for(var i=0;i<ss.length;i++){{\
         for(var j=0;j<ss[i].options.length;j++){{\
         var t=ss[i].options[j].text.toLowerCase(),\
         u=ss[i].options[j].value.toLowerCase();\
         opts.push(t+'|'+u);\
         if(t===v||u===v){{\
         ss[i].value=ss[i].options[j].value;\
         ss[i].dispatchEvent(new Event('change',{{bubbles:true}}));\
         return 'SET:'+ss[i].value;}}}}\
         }}return 'NOTFOUND:'+opts.join(',');\
         }})()"
    );

    let apple_script =
        format!("tell application \"Safari\" to do JavaScript \"{js}\" in front document");

    // Spawn osascript with a 10-second deadline. A stuck Safari permission
    // prompt or unresponsive renderer can cause wait() to block indefinitely,
    // which would stall the MCP tool handler permanently.
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&apple_script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("osascript launch failed: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    anyhow::bail!("osascript timed out after 10 seconds");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => anyhow::bail!("osascript wait error: {e}"),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("osascript output error: {e}"))?;

    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();

    if let Some(dom_val) = raw.strip_prefix("SET:") {
        Ok(format!(
            "✅ Set select [{element_index}] '{element_title}' to '{value}' via \
             Safari JavaScript (DOM value: \"{dom_val}\")."
        ))
    } else if let Some(available) = raw.strip_prefix("NOTFOUND:") {
        anyhow::bail!(
            "No <option> matching '{value}' found in any <select>. \
             Available (text|value): {available}"
        )
    } else if raw.is_empty() && !out.status.success() {
        let err_text = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("osascript failed: {}", err_text.trim())
    } else {
        anyhow::bail!(
            "JavaScript returned unexpected output: {}",
            &raw[..raw.len().min(200)]
        )
    }
}

// ── Percent-encoding helper ──────────────────────────────────────────────────

/// Percent-encode a string, leaving only unreserved URL characters (`-._~` +
/// alphanumerics) unencoded.  Matches the Swift reference's approach.
fn percent_encode_unreserved(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0xF));
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => '0',
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_text_control_reported_read_only_is_refused() {
        assert!(super::text_value_not_settable("AXTextField", || Some(false)));
        assert!(super::text_value_not_settable("AXTextArea", || Some(false)));
        assert!(!super::text_value_not_settable("AXTextField", || None), "unknown still writes");
        assert!(!super::text_value_not_settable("AXTextField", || Some(true)));
        assert!(!super::text_value_not_settable("AXSlider", || Some(false)), "other roles keep their paths");
    }

    use super::{apply_surface_trust, apply_verification_label, classify_write, SetValueOutcome};
    use super::{write_text_control, SetValueAttempt};
    use crate::tools::type_text::CatalystText;
    use std::cell::Cell;

    fn written() -> anyhow::Result<SetValueOutcome> {
        Ok(SetValueOutcome { detail: "✅ Set AXValue on [1] AXTextField.".into(), verified: Some(true), changed: Some(true) })
    }

    /// Runs the route with counters on every side effect.
    fn route(role: &str, settable: Option<bool>, catalyst: CatalystText) -> (SetValueAttempt, usize, usize, usize) {
        let (ancestry, focus, write) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let attempt = write_text_control(
            role,
            || settable,
            || { ancestry.set(ancestry.get() + 1); catalyst },
            || { focus.set(focus.get() + 1); Ok(()) },
            || { write.set(write.get() + 1); written() },
        )
        .unwrap();
        (attempt, ancestry.get(), focus.get(), write.get())
    }

    /// R1: a Catalyst text control is refused before any focus preparation
    /// or write; a read-only text control keeps its own refusal first.
    #[test]
    fn catalyst_text_is_refused_before_focus_or_write() {
        for role in ["AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"] {
            let (attempt, _, focus, write) = route(role, Some(true), CatalystText::Yes);
            assert!(matches!(attempt, SetValueAttempt::CatalystNeedsTyping), "{role}");
            assert_eq!((focus, write), (0, 0), "{role}: nothing prepared or written");
        }
        // Precedence: the read-only refusal wins and no ancestry is read.
        let (attempt, ancestry, focus, write) = route("AXTextField", Some(false), CatalystText::Yes);
        assert!(matches!(attempt, SetValueAttempt::Refused));
        assert_eq!((ancestry, focus, write), (0, 0, 0));
        // Unknown ancestry keeps today's write and carries the caveat.
        let (attempt, _, focus, write) = route("AXTextField", None, CatalystText::Unknown);
        assert!(matches!(attempt, SetValueAttempt::Applied(_, CatalystText::Unknown)));
        assert_eq!((focus, write), (1, 1));
        // A proven native field and a non-text control are written; a slider
        // never pays for the ancestry read.
        let (attempt, _, _, write) = route("AXTextField", Some(true), CatalystText::No);
        assert!(matches!(attempt, SetValueAttempt::Applied(_, CatalystText::No)));
        assert_eq!(write, 1);
        let (attempt, ancestry, _, write) = route("AXSlider", Some(true), CatalystText::Yes);
        assert!(matches!(attempt, SetValueAttempt::Applied(_, CatalystText::No)));
        assert_eq!((ancestry, write), (0, 1));
    }

    #[test]
    fn catalyst_refusal_names_the_typing_route() {
        let result = super::catalyst_text_needs_typing(7, 42);
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "catalyst_text_needs_typing");
        assert_eq!(data["effect"], "refused");
        assert_eq!((data["pid"].as_i64(), data["window_id"].as_u64()), (Some(7), Some(42)));
        let reason = data["reason"].as_str().unwrap();
        for needed in ["nothing was written", "click the field", "focused", "select all", "type_text", "results changed"] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
    }

    /// Unknown ancestry: "confirmed" never stands without the caveat that the
    /// app's reaction is unverified. Web content already distrusts the echo.
    #[test]
    fn unknown_catalyst_ancestry_says_the_app_reaction_is_unverified() {
        let mut outcome = written().unwrap();
        assert!(super::apply_catalyst_uncertainty(&mut outcome, CatalystText::Unknown, false));
        assert!(outcome.detail.contains("read-back confirms the accessibility value"), "{}", outcome.detail);
        assert!(outcome.detail.contains("whether the app itself reacted is unverified"), "{}", outcome.detail);
        assert_eq!(outcome.verified, Some(true), "the AX read-back itself still matched");
        // A read-back that did not match, or could not run, confirms nothing.
        for verified in [Some(false), None] {
            let mut outcome = SetValueOutcome { verified, ..written().unwrap() };
            assert!(super::apply_catalyst_uncertainty(&mut outcome, CatalystText::Unknown, false));
            assert!(!outcome.detail.contains("confirms"), "{verified:?}: {}", outcome.detail);
            assert!(outcome.detail.contains("neither the accessibility value nor"), "{}", outcome.detail);
        }
        for (catalyst, web) in [(CatalystText::No, false), (CatalystText::Unknown, true)] {
            let mut outcome = written().unwrap();
            assert!(!super::apply_catalyst_uncertainty(&mut outcome, catalyst, web));
            assert_eq!(outcome.detail, "✅ Set AXValue on [1] AXTextField.");
        }
    }

    #[test]
    fn unreadable_value_reports_neither_verified_nor_changed() {
        // AXValue is not exposed: the write can be neither confirmed nor denied,
        // so the tool must not claim success on the return code alone.
        assert_eq!(
            classify_write(Some("old"), None, "new", false),
            (None, None)
        );
    }

    #[test]
    fn matching_read_back_verifies_the_write() {
        assert_eq!(
            classify_write(Some("old"), Some("new"), "new", false),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn echoed_but_wrong_value_fails_verification() {
        // Web content behind an AXWebArea accepts the write and echoes a value
        // the renderer never took. A success return code must not be reported
        // as a verified write.
        assert_eq!(
            classify_write(Some("old"), Some("old"), "new", false),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn idempotent_write_is_verified_but_unchanged() {
        assert_eq!(
            classify_write(Some("same"), Some("same"), "same", false),
            (Some(true), Some(false))
        );
    }

    #[test]
    fn numeric_controls_compare_numerically() {
        // AXSlider reports "25.0" for a requested "25".
        assert_eq!(
            classify_write(Some("10"), Some("25.000000001"), "25", true),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn numeric_text_is_not_normalised_on_a_text_target() {
        assert_eq!(
            classify_write(Some("old"), Some("7"), "007", false),
            (Some(false), Some(true))
        );
    }

    #[test]
    fn missing_before_still_verifies_numeric_after() {
        assert_eq!(
            classify_write(None, Some("25.0"), "25", true),
            (Some(true), None)
        );
    }

    #[test]
    fn web_content_ax_echo_is_never_reported_as_verified() {
        let mut outcome = SetValueOutcome {
            detail: "Set value.".to_owned(),
            verified: Some(true),
            changed: Some(true),
        };
        apply_surface_trust(&mut outcome, true);
        assert_eq!(outcome.verified, Some(false));
        assert_eq!(outcome.changed, None);
        assert!(outcome.detail.contains("not trusted for web content"));
    }

    #[test]
    fn native_read_back_remains_trusted() {
        let mut outcome = SetValueOutcome {
            detail: "Set value.".to_owned(),
            verified: Some(true),
            changed: Some(true),
        };
        apply_surface_trust(&mut outcome, false);
        assert_eq!(outcome.verified, Some(true));
        assert_eq!(outcome.changed, Some(true));
        assert_eq!(outcome.detail, "Set value.");
    }

    #[test]
    fn unverified_result_does_not_keep_a_success_checkmark() {
        let mut outcome = SetValueOutcome {
            detail: "✅ Set AXValue on [4] AXTextField.".to_owned(),
            verified: Some(false),
            changed: Some(false),
        };
        apply_verification_label(&mut outcome);
        assert_eq!(
            outcome.detail,
            "📨 Sent (unverified) AXValue on [4] AXTextField."
        );
    }
}
