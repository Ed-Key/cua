//! set_value tool — matches the Swift reference in SetValueTool.swift.
//!
//! Two modes, determined by the element's AXRole:
//!
//! * **AXPopUpButton**: Find the option whose AXTitle or AXValue matches
//!   `value` (case-insensitive) and AXPress it: directly when the popup lists
//!   its options, else after opening its menu (closed again on any outcome),
//!   then read the popup's shown value back.  Safari `<select>` elements that
//!   expose no AX children use `osascript do JavaScript` instead.
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
             select gets the matching option picked (its menu is opened and closed again when \
             the options are not listed) and read back; other elements get AXValue written \
             (sliders, steppers, date pickers, settable text fields). Web pages ignore value \
             writes: in Chrome use get_browser_state then browser_type, elsewhere type_text."
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
                        || unsafe { file_name_cell(element) },
                        || unsafe { super::type_text::catalyst_text_control_of(element) },
                        || unsafe { search_like(element, &role) },
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
                        |catalyst| {
                            let outcome = set_value_blocking(element_ptr, element_index, pid, &value)?;
                            if catalyst == super::type_text::CatalystText::Yes {
                                return Ok(catalyst_read_back(outcome, &value, || unsafe {
                                    copy_string_attr(element, "AXValue")
                                }));
                            }
                            Ok(Written::Outcome(unsafe {
                                settle_end_of_editing(outcome, element, element_index, pid, &value)
                            }))
                        },
                    )
                })
                .await
            },
        )
        .await;

        let changes = snapshot.detect_async().await;

        match result {
            Ok(Ok(SetValueAttempt::Refused)) => nonsettable_text_refusal(super::edit_commit::read_only_note(pid)),
            Ok(Ok(SetValueAttempt::FileNameNeedsRename)) => file_name_needs_rename(pid, window_id),
            Ok(Ok(SetValueAttempt::CatalystNeedsTyping)) => catalyst_text_needs_typing(pid, window_id),
            Ok(Ok(SetValueAttempt::CatalystDidNotTake(now))) => catalyst_text_did_not_take(pid, window_id, now),
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
    /// A file's name shown in a list: nothing was focused or written.
    FileNameNeedsRename,
    /// A Mac Catalyst search-like text control: nothing was focused or written.
    CatalystNeedsTyping,
    /// A Mac Catalyst text control that did not hold the value written; what
    /// it holds now.
    CatalystDidNotTake(Option<String>),
    Applied(SetValueOutcome, super::type_text::CatalystText),
}

/// What a write produced: an outcome to report, or a Catalyst field that
/// did not keep the value.
enum Written {
    Outcome(SetValueOutcome),
    DidNotTake(Option<String>),
}

fn is_text_control_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
}

/// The ordered route for one set_value on the retained element. Refusals come
/// before any side effect: a read-only text control first (it keeps
/// precedence), then a file's name shown in a list, then a Mac Catalyst
/// search-like field. Only then is the field prepared (`prepare_focus`, not
/// for Catalyst fields) and the value written.
#[allow(clippy::too_many_arguments)]
fn write_text_control(
    role: &str,
    read_settable: impl FnOnce() -> Option<bool>,
    file_name: impl FnOnce() -> bool,
    catalyst: impl FnOnce() -> super::type_text::CatalystText,
    search: impl FnOnce() -> bool,
    prepare_focus: impl FnOnce() -> anyhow::Result<()>,
    write: impl FnOnce(super::type_text::CatalystText) -> anyhow::Result<Written>,
) -> anyhow::Result<SetValueAttempt> {
    use super::type_text::CatalystText;
    if text_value_not_settable(role, read_settable) {
        return Ok(SetValueAttempt::Refused);
    }
    if role == "AXTextField" && file_name() {
        return Ok(SetValueAttempt::FileNameNeedsRename);
    }
    // Only text controls pay for the ancestry read.
    let catalyst = if is_text_control_role(role) { catalyst() } else { CatalystText::No };
    if catalyst == CatalystText::Yes {
        // A field that acts on each keystroke ignores a value write (the
        // Messages search does not search): it needs typed keys.
        if search() {
            return Ok(SetValueAttempt::CatalystNeedsTyping);
        }
    } else {
        prepare_focus()?;
    }
    Ok(match write(catalyst)? {
        Written::Outcome(outcome) => SetValueAttempt::Applied(outcome, catalyst),
        Written::DidNotTake(now) => SetValueAttempt::CatalystDidNotTake(now),
    })
}

/// A file's name as a list shows it (Finder's list and icon views): a text
/// field naming a file (AXFilename, a file:// AXURL) that is not being
/// edited. Writing its AXValue changes what the list shows, never the file.
/// Finder's rename editor is a separate focused field and stays writable.
unsafe fn file_name_cell(element: AXUIElementRef) -> bool {
    copy_string_attr(element, "AXFilename").is_some_and(|name| !name.is_empty())
        && crate::ax::bindings::copy_url_attr(element).is_some_and(|url| url.starts_with("file://"))
        && crate::ax::bindings::copy_bool_attr(element, "AXFocused") != Some(true)
}

/// A search-like text field: the role or subrole says so, or its
/// placeholder, description, identifier or title names a search.
unsafe fn search_like(element: AXUIElementRef, role: &str) -> bool {
    let subrole = copy_string_attr(element, "AXSubrole");
    let texts = ["AXPlaceholderValue", "AXDescription", "AXIdentifier", "AXTitle"]
        .map(|attribute| copy_string_attr(element, attribute));
    is_search_like(role, subrole.as_deref(), &texts)
}

fn is_search_like(role: &str, subrole: Option<&str>, texts: &[Option<String>]) -> bool {
    role == "AXSearchField"
        || subrole == Some("AXSearchField")
        || texts
            .iter()
            .flatten()
            .any(|text| text.to_lowercase().contains("search"))
}

/// A Catalyst field's value read again after the app had time to process
/// the write: it must still hold the value, or the write did not take.
fn catalyst_read_back(
    mut outcome: SetValueOutcome,
    value: &str,
    read: impl Fn() -> Option<String>,
) -> Written {
    std::thread::sleep(std::time::Duration::from_millis(300));
    let now = read();
    let Some(now) = now else {
        // An unreadable value proves neither that it took nor that it did not.
        outcome.verified = None;
        outcome.detail.push_str(
            " This is a Mac Catalyst field and its value could not be read back, so the write \
             is unverified; whether the app reacted is unverified too.",
        );
        return Written::Outcome(outcome);
    };
    if now != value {
        return Written::DidNotTake(Some(now));
    }
    outcome.verified = Some(true);
    outcome.detail.push_str(
        " This is a Mac Catalyst field: it holds the value (read back after 300 ms), but the \
         app was not sent typed keys, so whether it reacted (validation, a live search, \
         autocomplete) is unverified. For a field that should act on each keystroke, click it \
         and type_text instead.",
    );
    Written::Outcome(outcome)
}

const FILE_NAME_NEEDS_RENAME: &str = "file_name_needs_rename";

/// The refusal for set_value on a file's name shown in a list.
fn file_name_needs_rename(pid: i32, window_id: u32) -> ToolResult {
    let reason = "This is a file's name as the list shows it. A value write changes only what \
                  the list shows, never the file, so nothing was written. Rename: click the \
                  item, press return (or invoke_menu File > Rename), select all with cmd+a \
                  (Finder selects the name without its extension), type_text the full new name, \
                  press return, then check the list shows it.";
    ToolResult::error(format!("set_value refused ({FILE_NAME_NEEDS_RENAME}): {reason}"))
        .with_structured(serde_json::json!({
            "code": FILE_NAME_NEEDS_RENAME,
            "effect": "refused",
            "path": "ax",
            "pid": pid,
            "window_id": window_id,
            "reason": reason,
        }))
}

const CATALYST_TEXT_DID_NOT_TAKE: &str = "catalyst_text_did_not_take";

fn catalyst_text_did_not_take(pid: i32, window_id: u32, now: Option<String>) -> ToolResult {
    let holds = match &now {
        Some(text) => format!("it holds {}", serde_json::json!(text)),
        None => "its value could not be read".to_owned(),
    };
    let reason = format!(
        "This Mac Catalyst field did not keep the value written ({holds}). Click the field, \
         select all (hotkey cmd+a) if replacing, then type_text; if type_text reports that 0 \
         characters landed, type again with delivery_mode \"foreground\"."
    );
    ToolResult::error(format!("set_value failed ({CATALYST_TEXT_DID_NOT_TAKE}): {reason}"))
        .with_structured(serde_json::json!({
            "code": CATALYST_TEXT_DID_NOT_TAKE,
            "effect": "failed",
            "path": "ax",
            "pid": pid,
            "window_id": window_id,
            "reason": reason,
        }))
}

const CATALYST_TEXT_NEEDS_TYPING: &str = "catalyst_text_needs_typing";

/// The refusal for set_value on a Mac Catalyst text control.
fn catalyst_text_needs_typing(pid: i32, window_id: u32) -> ToolResult {
    let reason = "This is a Mac Catalyst search field. Catalyst apps take an accessibility \
                  value write without reacting to it (a search does not run), and the value \
                  read-back cannot tell the difference, so nothing was written. \
                  Next: click the field (a background click is enough), select all (hotkey \
                  cmd+a) if replacing, then type_text on it. If type_text reports that 0 \
                  characters landed, type again with delivery_mode \"foreground\". Then check \
                  the app's own result (for a search field, that its results changed).";
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

fn nonsettable_text_refusal(note: &str) -> ToolResult {
    ToolResult::error(format!(
        "Cannot set AXValue: the text control currently reports that its value is not settable. \
         No value write was attempted. This describes AXValue writability, not keyboard editability.{note}",
    ))
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

/// A text field whose app saves it only when its editing ends (Finder's Get
/// Info, its rename field): a matching read-back proves only what the field
/// shows. A multi-line field is ended here the way Tab ends it (Return would
/// be a new line); a one-line field is left for the agent's Return. Either
/// way the result is never "confirmed": what the app saved is not readable.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
unsafe fn settle_end_of_editing(
    outcome: SetValueOutcome,
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    value: &str,
) -> SetValueOutcome {
    use super::edit_commit::{end_edit, not_committed, pending_field_of, Field};
    // A read-back that did not match keeps its own words.
    if outcome.verified != Some(true) {
        return outcome;
    }
    let Some(field) = pending_field_of(pid, element) else {
        return outcome;
    };
    let role = copy_string_attr(element, "AXRole").unwrap_or_default();
    let app = crate::apps::get_app_name_for_pid(pid).unwrap_or_else(|| "The app".into());
    let shown = serde_json::json!(value);
    let focused = crate::ax::bindings::copy_bool_attr(element, "AXFocused") == Some(true);
    if field == Field::MultiLine && focused {
        if let Some(ended) = end_edit(element) {
            let now = copy_string_attr(element, "AXValue");
            let (detail, verified) = if now.is_none() {
                (
                    format!(
                        "📨 Set and ended the edit: [{element_index}] {role} was set to {shown} and \
                         the focus moved to {}, which ends its editing, when {app} saves this \
                         field; its value could not be read again, so neither what it holds nor \
                         what {app} saved is verified.",
                        ended.focus
                    ),
                    None,
                )
            } else if now.as_deref() == Some(value) {
                (
                    format!(
                        "📨 Set and ended the edit: [{element_index}] {role} holds {shown} and the \
                         focus moved to {}, which ends its editing, when {app} saves this field. \
                         What {app} saved is not readable through accessibility, so the save is \
                         unverified.",
                        ended.focus
                    ),
                    None,
                )
            } else {
                (
                    format!(
                        "⚠️ Not kept: [{element_index}] {role} showed {shown}, but after its edit \
                         ended (focus moved to {}) it reads {}: {app} did not keep the value.",
                        ended.focus,
                        serde_json::json!(now)
                    ),
                    Some(false),
                )
            };
            return SetValueOutcome { detail, verified, changed: None };
        }
    }
    SetValueOutcome {
        detail: format!(
            "⚠️ Set, not committed: [{element_index}] {role} shows {shown}. {}",
            not_committed(field, &app)
        ),
        verified: None,
        changed: None,
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
        // Menu-item selection, read back from the pop-up's shown value.
        select_popup_option(element, element_index, pid, value, &element_title)
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
) -> anyhow::Result<SetValueOutcome> {
    let mut options = unsafe { popup_options(element) };
    let mut opened = false;
    if options.is_empty() {
        let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
        if app_name == "Safari" {
            // Safari/WebKit <select>: no AX children while closed.
            return set_select_via_js(element_index, element_title, value).map(|detail| {
                SetValueOutcome { detail, verified: None, changed: None }
            });
        }
        // A closed AppKit pop-up (a Save panel's encoding) shows its items
        // only while its menu is open: open it, pick, read back.
        opened = true;
        let err = unsafe { perform_action(element, "AXPress") };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while options.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            options = unsafe { popup_options(element) };
        }
        if options.is_empty() {
            let closed = unsafe { close_popup_menu(element, pid) };
            anyhow::bail!(
                "AXPopUpButton [{element_index}] \"{element_title}\" showed no options \
                 (pressing it to open its menu returned AX error {err}).{}",
                closed_note(closed)
            );
        }
    }
    let titles: Vec<String> = options.iter().map(|o| o.title.clone()).filter(|t| !t.is_empty()).collect();
    let chosen = match_option(&options, value);
    let result = match chosen {
        Some(i) => {
            let err = unsafe { perform_action(options[i].element, "AXPress") };
            if err == kAXErrorSuccess {
                Ok((options[i].title.clone(), options[i].value.clone()))
            } else {
                Err(format!("AXPress on option '{}' failed with AX error {err}", options[i].title))
            }
        }
        None => Err(format!(
            "No option matching '{value}' in AXPopUpButton [{element_index}] \"{element_title}\". \
             Available: [{}]",
            titles.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ")
        )),
    };
    for option in &options {
        unsafe { CFRelease(option.element as _) };
    }
    let (picked_title, picked_value) = match result {
        Ok(picked) => picked,
        Err(error) => {
            let closed = if opened { unsafe { close_popup_menu(element, pid) } } else { Some(true) };
            anyhow::bail!("{error}.{}", closed_note(closed));
        }
    };
    // The menu closes when its item is chosen; make sure it did.
    let closed = if opened {
        match crate::windows::wait_for_no_menu(pid) {
            Some(true) => Some(true),
            _ => unsafe { close_popup_menu(element, pid) },
        }
    } else {
        Some(true)
    };
    // The pop-up may show the chosen option's title or its own value.
    let is_picked = |shown: &str| shows_option(shown, &picked_title, &picked_value);
    let picked = if picked_title.is_empty() { picked_value.clone() } else { picked_title.clone() };
    let shown = read_back_popup(element, is_picked);
    let (verified, how) = match &shown {
        Some(now) if is_picked(now) => (Some(true), format!("it now shows '{now}'")),
        Some(now) => (Some(false), format!("it still shows '{now}'; verify via screenshot")),
        None => (None, "its shown value is not readable through AX; could not confirm".to_owned()),
    };
    let mark = if verified == Some(true) { "✅ Selected" } else { "📨 Picked (unverified)" };
    let route = if opened { "opened its menu and pressed the item" } else { "pressed the item without opening the menu" };
    Ok(SetValueOutcome {
        detail: format!(
            "{mark} '{picked}' in AXPopUpButton [{element_index}] \"{element_title}\" ({route}); {how}.{}",
            if closed == Some(true) { String::new() } else { closed_note(closed) }
        ),
        verified,
        changed: None,
    })
}

/// One option of a pop-up: its retained element and title.
struct PopupOption {
    element: AXUIElementRef,
    title: String,
    value: String,
}

/// The pop-up's options: its children, through the AXMenu AppKit puts
/// between a pop-up and its items while the menu is open. Each element is
/// retained; the caller releases them.
unsafe fn popup_options(element: AXUIElementRef) -> Vec<PopupOption> {
    let mut items = Vec::new();
    for child in copy_children(element) {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            items.extend(copy_children(child));
            CFRelease(child as _);
        } else {
            items.push(child);
        }
    }
    items
        .into_iter()
        .map(|item| {
            let title = copy_string_attr(item, "AXTitle").unwrap_or_default();
            let value = copy_string_attr(item, "AXValue").unwrap_or_default();
            PopupOption { element: item, title, value }
        })
        .collect()
}

/// Whether a pop-up's shown value names the option: its title, or its own
/// non-empty value.
fn shows_option(shown: &str, title: &str, value: &str) -> bool {
    shown.eq_ignore_ascii_case(title) || (!value.is_empty() && shown.eq_ignore_ascii_case(value))
}

/// The option whose title, or else non-empty value, matches (case-insensitive).
fn match_option(options: &[PopupOption], value: &str) -> Option<usize> {
    option_index(options.iter().map(|o| (o.title.as_str(), o.value.as_str())), value)
}

fn option_index<'a>(options: impl Iterator<Item = (&'a str, &'a str)>, value: &str) -> Option<usize> {
    let wanted = value.to_lowercase();
    options
        .into_iter()
        .position(|(title, v)| title.to_lowercase() == wanted || (!v.is_empty() && v.to_lowercase() == wanted))
}

/// The pop-up's shown choice, polled for up to a second until it reads
/// the picked option (AppKit updates it after the menu closes).
fn read_back_popup(element: AXUIElementRef, is_picked: impl Fn(&str) -> bool) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        // Only AXValue is the choice: a title can be a fixed label.
        let shown = unsafe { copy_string_attr(element, "AXValue") };
        if shown.as_deref().is_some_and(&is_picked)
            || std::time::Instant::now() >= deadline
        {
            return shown;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Cancel the pop-up's open menu and read WindowServer's list back.
unsafe fn close_popup_menu(element: AXUIElementRef, pid: i32) -> Option<bool> {
    for child in copy_children(element) {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            let _ = perform_action(child, "AXCancel");
        }
        CFRelease(child as _);
    }
    crate::windows::wait_for_no_menu(pid)
}

fn closed_note(closed: Option<bool>) -> String {
    match closed {
        Some(true) => String::new(),
        Some(false) => " Its menu is still open: press escape on the window before other input.".into(),
        None => " Whether its menu closed could not be read: check before other input.".into(),
    }
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
        route_with(role, settable, false, catalyst, false)
    }

    fn route_with(
        role: &str,
        settable: Option<bool>,
        file_name: bool,
        catalyst: CatalystText,
        search: bool,
    ) -> (SetValueAttempt, usize, usize, usize) {
        let (ancestry, focus, write) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let attempt = write_text_control(
            role,
            || settable,
            || file_name,
            || { ancestry.set(ancestry.get() + 1); catalyst },
            || search,
            || { focus.set(focus.get() + 1); Ok(()) },
            |_| { write.set(write.get() + 1); written().map(super::Written::Outcome) },
        )
        .unwrap();
        (attempt, ancestry.get(), focus.get(), write.get())
    }

    /// R1: a Catalyst search field is refused before any focus preparation
    /// or write; a read-only text control keeps its own refusal first.
    #[test]
    fn catalyst_search_text_is_refused_before_focus_or_write() {
        for role in ["AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"] {
            let (attempt, _, focus, write) = route_with(role, Some(true), false, CatalystText::Yes, true);
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

    /// G6: a Catalyst field that is not search-like is written, without the
    /// native focus preparation, and its read-back decides the result.
    #[test]
    fn catalyst_form_field_is_written_without_focus_preparation() {
        let (attempt, _, focus, write) = route_with("AXTextField", Some(true), false, CatalystText::Yes, false);
        assert!(matches!(attempt, SetValueAttempt::Applied(_, CatalystText::Yes)));
        assert_eq!((focus, write), (0, 1));
        let kept = super::catalyst_read_back(written().unwrap(), "Ada Lovelace", || Some("Ada Lovelace".into()));
        match kept {
            super::Written::Outcome(outcome) => {
                assert_eq!(outcome.verified, Some(true));
                assert!(outcome.detail.contains("not sent typed keys"), "{}", outcome.detail);
                assert!(outcome.detail.contains("unverified"), "{}", outcome.detail);
            }
            super::Written::DidNotTake(_) => panic!("the field holds the value"),
        }
        let lost = super::catalyst_read_back(written().unwrap(), "Ada Lovelace", || Some("Ada".into()));
        assert!(matches!(lost, super::Written::DidNotTake(Some(ref now)) if now == "Ada"));
        // Unreadable: unverified, not a failure.
        match super::catalyst_read_back(written().unwrap(), "Ada Lovelace", || None) {
            super::Written::Outcome(outcome) => assert_eq!(outcome.verified, None),
            super::Written::DidNotTake(_) => panic!("an unreadable value is not a failed write"),
        }
    }

    #[test]
    fn a_pop_up_option_matches_its_title_or_its_own_value_only() {
        let options = [("", ""), ("Daily", ""), ("Weekly", "w")];
        assert_eq!(super::option_index(options.into_iter(), ""), Some(0), "a blank item stays selectable");
        assert_eq!(super::option_index(options.into_iter(), "daily"), Some(1));
        assert_eq!(super::option_index(options.into_iter(), "W"), Some(2));
        assert_eq!(super::option_index(options.into_iter(), "Monthly"), None);
        assert!(super::shows_option("w", "Weekly", "w"), "a pop-up that reports the value");
        assert!(super::shows_option("weekly", "Weekly", "w"));
        assert!(!super::shows_option("", "Daily", ""), "an empty value proves nothing");
        assert!(!super::shows_option("Daily", "Weekly", "w"));
    }

    #[test]
    fn search_like_fields_are_named_by_role_subrole_or_their_text() {
        assert!(super::is_search_like("AXSearchField", None, &[]));
        assert!(super::is_search_like("AXTextField", Some("AXSearchField"), &[]));
        assert!(super::is_search_like("AXTextField", None, &[Some("Search messages".into())]));
        assert!(super::is_search_like("AXTextField", None, &[None, Some("probe-search".into())]));
        assert!(!super::is_search_like("AXTextField", None, &[Some("Display name".into()), None]));
    }

    /// G1: a file's name as a list shows it is refused before anything is
    /// focused or written, ahead of the Catalyst checks.
    #[test]
    fn a_file_name_cell_is_refused_before_focus_or_write() {
        let (attempt, ancestry, focus, write) = route_with("AXTextField", Some(true), true, CatalystText::No, false);
        assert!(matches!(attempt, SetValueAttempt::FileNameNeedsRename));
        assert_eq!((ancestry, focus, write), (0, 0, 0));
        // Only text fields: another role keeps its own path.
        let (attempt, _, _, write) = route_with("AXSlider", Some(true), true, CatalystText::No, false);
        assert!(matches!(attempt, SetValueAttempt::Applied(..)));
        assert_eq!(write, 1);
        let reason = super::file_name_needs_rename(7, 42).structured_content.unwrap()["reason"]
            .as_str()
            .unwrap()
            .to_owned();
        for needed in ["never the file", "nothing was written", "return", "cmd+a", "type_text"] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
    }

    #[test]
    fn catalyst_refusal_names_the_typing_route() {
        let result = super::catalyst_text_needs_typing(7, 42);
        let data = result.structured_content.as_ref().unwrap();
        assert_eq!(data["code"], "catalyst_text_needs_typing");
        assert_eq!(data["effect"], "refused");
        assert_eq!((data["pid"].as_i64(), data["window_id"].as_u64()), (Some(7), Some(42)));
        let reason = data["reason"].as_str().unwrap();
        for needed in ["search field", "nothing was written", "click the field", "select all", "type_text", "delivery_mode \"foreground\"", "results changed"] {
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
