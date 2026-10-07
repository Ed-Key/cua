//! set_value tool — matches the Swift reference in SetValueTool.swift.
//!
//! Two modes, determined by the element's AXRole:
//!
//! * **AXPopUpButton**: Find the option (a child, or an `AXMenuItem` under the
//!   popup's `AXMenu`) whose AXTitle or AXValue matches `value`
//!   (case-insensitive) and AXPress it.  AppKit and Chromium popups publish
//!   their items only while the menu is open, so the menu is opened for the
//!   selection and closed again.  Safari `<select>` elements are set through
//!   `osascript do JavaScript` instead.  The pop-up's shown value is read
//!   back to confirm the pick.
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
    copy_bool_attr, copy_children, copy_number_attr, copy_string_attr, copy_url_attr,
    kAXErrorSuccess, perform_action, set_number_attr, set_string_attr, AXUIElementRef,
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
            "Set an element's value by element_token. A popup or \
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
                    "description": "Target window ID; omit with element_token, which carries it."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
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
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let value = match args.require_str("value") {
            Ok(v) => v,
            Err(e) => return e,
        };

        let (element_index, window_id, element_guard) =
            match self.state.snapshots.resolve(pid, &args) {
                Ok(cua_driver_core::element_token::ResolvedElement::None) => {
                    return ToolResult::error(
                        "set_value requires element_token to address the target element.",
                    )
                }
                Ok(cua_driver_core::element_token::ResolvedElement::Element {
                    window_id,
                    element_index,
                    element,
                }) => match u32::try_from(window_id) {
                    Ok(window_id) => (element_index, window_id, element),
                    Err(_) => return ToolResult::error("window_id is out of range for macOS."),
                },
                Err(refusal) => return refusal,
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

        // A file's name as Finder lists it takes an AXValue write and reads it
        // back, but the file is never renamed. Refuse before anything moves.
        // Finder's Get Info Name field does the same.
        let name_guard = element_guard.clone();
        if let Ok(Some(reason)) = tokio::task::spawn_blocking(move || unsafe {
            let element = name_guard.as_ptr() as AXUIElementRef;
            if file_name_cell(element) {
                Some(LIST_RENAME_ROUTE)
            } else if get_info_name_field(pid, element) {
                Some(GET_INFO_RENAME_ROUTE)
            } else {
                None
            }
        })
        .await
        {
            return file_name_needs_rename(pid, window_id, reason);
        }

        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        let center_guard = element_guard.clone();
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
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::PinAbove(window_id as u64),
            );
            crate::cursor::overlay::animate_cursor_to_target(
                cursor_key.clone(),
                screen_x,
                screen_y,
                Some(window_id as u64),
                target_rect,
            )
            .await;
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
/// precedence), then a Mac Catalyst search-like field. (A file's name in a
/// Finder list or Get Info is refused earlier, before the cursor moves.) Only
/// then is the field prepared (`prepare_focus`, not
/// for Catalyst fields) and the value written.
#[allow(clippy::too_many_arguments)]
fn write_text_control(
    role: &str,
    read_settable: impl FnOnce() -> Option<bool>,
    catalyst: impl FnOnce() -> super::type_text::CatalystText,
    search: impl FnOnce() -> bool,
    prepare_focus: impl FnOnce() -> anyhow::Result<()>,
    write: impl FnOnce(super::type_text::CatalystText) -> anyhow::Result<Written>,
) -> anyhow::Result<SetValueAttempt> {
    use super::type_text::CatalystText;
    if text_value_not_settable(role, read_settable) {
        return Ok(SetValueAttempt::Refused);
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

// ── File name cells ──────────────────────────────────────────────────────────

const FILE_NAME_NEEDS_RENAME: &str = "file_name_needs_rename";

/// Whether `element` is a file's name as a list shows it (Finder's list and
/// icon views): a text field that names a file and is not being edited.
/// Finder's inline rename editor is focused, so it stays writable.
unsafe fn file_name_cell(element: AXUIElementRef) -> bool {
    copy_string_attr(element, "AXRole").as_deref() == Some("AXTextField")
        && is_file_name_cell(
            copy_string_attr(element, "AXFilename").as_deref(),
            copy_url_attr(element).as_deref(),
            copy_bool_attr(element, "AXFocused"),
        )
}

fn is_file_name_cell(filename: Option<&str>, url: Option<&str>, focused: Option<bool>) -> bool {
    filename.is_some_and(|name| !name.is_empty())
        && url.is_some_and(|url| url.starts_with("file://"))
        && focused != Some(true)
}

/// Whether `element` is the Name & Extension field of Finder's Get Info
/// window. It names no file through AXFilename or AXURL, so `file_name_cell`
/// misses it, yet an AXValue write there renames nothing either.
unsafe fn get_info_name_field(pid: i32, element: AXUIElementRef) -> bool {
    is_get_info_name_field(
        crate::apps::bundle_id_for_pid(pid).as_deref(),
        copy_string_attr(element, "AXRole").as_deref(),
        copy_string_attr(element, "AXIdentifier").as_deref(),
        copy_bool_attr(element, "AXFocused"),
    )
}

fn is_get_info_name_field(
    bundle_id: Option<&str>,
    role: Option<&str>,
    identifier: Option<&str>,
    focused: Option<bool>,
) -> bool {
    bundle_id == Some("com.apple.finder")
        && role == Some("AXTextField")
        && identifier == Some("Name")
        && focused != Some(true)
}

const LIST_RENAME_ROUTE: &str = "This is a file's name as the list shows it. Writing its AXValue \
    changes only what the list shows, never the file, so nothing was written. To rename the \
    file: click this element to select the item, then with Finder frontmost send press_key \
    return, hotkey cmd+a (Finder selects the name without its extension), type_text the full \
    new name, and press_key return, each with scope:\"desktop\". Then check the new name in a \
    fresh get_window_state.";

const GET_INFO_RENAME_ROUTE: &str = "This is the Name & Extension field of Finder's Get Info \
    window. Writing its AXValue changes only what the field shows, never the file, so nothing \
    was written. To rename the file: take a fresh get_window_state of this window and click the \
    field's centre in its screenshot pixels (pass its capture_id), then hotkey cmd+a, type_text \
    the full new name with its extension, and press_key return, each with \
    delivery_mode:\"foreground\" on this window. A changed extension makes Finder ask for \
    confirmation in a dialog first. Then check the new name in a fresh listing of the folder; \
    this window's title changes with it.";

fn file_name_needs_rename(pid: i32, window_id: u32, reason: &str) -> ToolResult {
    ToolResult::error(format!(
        "set_value refused ({FILE_NAME_NEEDS_RENAME}): {reason}"
    ))
    .with_structured(serde_json::json!({
        "code": FILE_NAME_NEEDS_RENAME,
        "effect": "refused",
        "path": "ax",
        "pid": pid,
        "window_id": window_id,
        "reason": reason,
    }))
}

// ── Blocking implementation (runs on spawn_blocking thread) ─────────────────

/// Outcome of a `set_value` write.
///
/// `verified` is `None` when nothing could be read back (Safari's DOM path for
/// a `<select>`, an unreadable AXValue or pop-up value), and
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
        } else if let Some(rest) = outcome.detail.strip_prefix("✅ Selected") {
            // A pop-up pick that apply_surface_trust downgraded (web content).
            outcome.detail = format!("📨 Picked (unverified){rest}");
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

/// How long to wait for a popup's menu to publish its items after AXPress.
const POPUP_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
/// Consecutive unchanged polls that mean the menu has finished filling in.
const POPUP_STABLE_POLLS: u32 = 3;
/// How long a menu gets to close at each closing step (after the pick, after
/// AXCancel, after Escape).
const POPUP_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);
const POPUP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// AX messaging timeout while opening the menu. The target app enters its
/// menu-tracking loop inside the AXPress, so the call would otherwise block
/// for the full default timeout (~1.5 s) with the menu already open.
const POPUP_PRESS_TIMEOUT_SECONDS: f32 = 0.5;
/// How long the pop-up's shown value may take to follow the pick (AppKit
/// updates it after the menu closes).
const POPUP_READ_BACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// One selectable entry of a popup: the retained AX element plus the title
/// and value it reports.
struct PopupOption {
    element: AXUIElementRef,
    title: String,
    value: String,
}

/// Release every retained option.
fn release_options(options: &[PopupOption]) {
    for option in options {
        unsafe { CFRelease(option.element as _) };
    }
}

/// A separator (or any entry with neither title nor value): never an option.
fn is_blank_option(title: &str, value: &str) -> bool {
    title.trim().is_empty() && value.trim().is_empty()
}

/// The popup's options as AX exposes them: its direct children, or, when a
/// child is an `AXMenu` (AppKit `NSPopUpButton`, Chromium `<select>`), that
/// menu's `AXMenuItem` children. Blank entries are released and left out, so
/// an unopened popup reads as empty instead of as one untitled option.
fn popup_options(popup: AXUIElementRef) -> Vec<PopupOption> {
    let mut options = Vec::new();
    for child in unsafe { copy_children(popup) } {
        let role = unsafe { copy_string_attr(child, "AXRole") }.unwrap_or_default();
        if role == "AXMenu" {
            for item in unsafe { copy_children(child) } {
                options.push(describe_option(item));
            }
            unsafe { CFRelease(child as _) };
        } else {
            options.push(describe_option(child));
        }
    }
    let (kept, blank): (Vec<_>, Vec<_>) = options
        .into_iter()
        .partition(|option| !is_blank_option(&option.title, &option.value));
    release_options(&blank);
    kept
}

fn describe_option(element: AXUIElementRef) -> PopupOption {
    PopupOption {
        element,
        title: unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default(),
        value: unsafe { copy_string_attr(element, "AXValue") }.unwrap_or_default(),
    }
}

/// Index of the option whose non-empty title, or else non-empty value, equals
/// `value`, ignoring case and surrounding whitespace. An option without an
/// AXValue is never matched through its (missing) value, and an empty request
/// matches nothing.
fn matching_option(options: &[(String, String)], value: &str) -> Option<usize> {
    let wanted = value.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    options.iter().position(|(title, option_value)| {
        title.trim().to_lowercase() == wanted || option_value.trim().to_lowercase() == wanted
    })
}

fn option_pairs(options: &[PopupOption]) -> Vec<(String, String)> {
    options
        .iter()
        .map(|option| (option.title.clone(), option.value.clone()))
        .collect()
}

fn describe_available(options: &[PopupOption]) -> String {
    options
        .iter()
        .map(|option| {
            let label = if option.title.is_empty() {
                &option.value
            } else {
                &option.title
            };
            format!("\"{label}\"")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the popup's menu is open right now: an `AXMenu` child that already
/// lists items. A closed AppKit popup has an empty `AXMenu` (or none), and a
/// closed Chromium popup lists only its selected item directly.
fn popup_menu_is_open(popup: AXUIElementRef) -> bool {
    let mut open = false;
    for child in unsafe { copy_children(popup) } {
        if unsafe { copy_string_attr(child, "AXRole") }.as_deref() == Some("AXMenu") {
            let items = unsafe { copy_children(child) };
            open |= !items.is_empty();
            for item in items {
                unsafe { CFRelease(item as _) };
            }
        }
        unsafe { CFRelease(child as _) };
    }
    open
}

/// Poll the pop-up's own menu until it reads closed, for one closing step.
fn popup_menu_closes(popup: AXUIElementRef) -> bool {
    let deadline = std::time::Instant::now() + POPUP_CLOSE_TIMEOUT;
    loop {
        if !popup_menu_is_open(popup) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POPUP_POLL_INTERVAL);
    }
}

/// Close the menu this call opened and say whether it closed: `Some(true)`
/// closed, `Some(false)` still open, `None` unknown.
///
/// Evidence comes from the pop-up's own menu first and WindowServer second.
/// A picked item closes its menu by itself, so after a pick the menu first
/// gets time to close. Then AXCancel goes to the pop-up's menu (enough for
/// AppKit). Escape (Chromium's menu ignores AXCancel) is sent only while the
/// pop-up's own menu still reads open and WindowServer does not show that no
/// menu window this call opened is left; Escape reaching a window instead of
/// a menu would cancel a sheet. When the pop-up's menu reads closed,
/// WindowServer must also show no menu window this call opened.
fn close_popup_menu(
    popup: AXUIElementRef,
    pid: i32,
    menus_before: &[u32],
    picked: bool,
) -> Option<bool> {
    if !(picked && popup_menu_closes(popup)) {
        for child in unsafe { copy_children(popup) } {
            if unsafe { copy_string_attr(child, "AXRole") }.as_deref() == Some("AXMenu") {
                let _ = unsafe { perform_action(child, "AXCancel") };
            }
            unsafe { CFRelease(child as _) };
        }
        if !popup_menu_closes(popup) {
            if crate::windows::new_menu_windows(pid, menus_before) == Some(0) {
                // The two readings disagree: say so rather than send a key.
                return None;
            }
            let _ = crate::input::keyboard::press_key_no_auth(pid, "escape", &[]);
            if !popup_menu_closes(popup) {
                return Some(false);
            }
        }
    }
    crate::windows::wait_for_new_menus_closed(pid, menus_before)
}

fn closed_note(closed: Option<bool>) -> String {
    match closed {
        Some(true) => String::new(),
        Some(false) => {
            " Its menu is still open: press escape on the window before other input.".into()
        }
        None => " Whether its menu closed could not be read: check before other input.".into(),
    }
}

/// Whether a pop-up's shown value names the picked option: its title, or its
/// own non-empty value. An empty shown value proves nothing.
fn shows_option(shown: &str, title: &str, value: &str) -> bool {
    let shown = shown.trim();
    let (title, value) = (title.trim(), value.trim());
    !shown.is_empty()
        && ((!title.is_empty() && shown.eq_ignore_ascii_case(title))
            || (!value.is_empty() && shown.eq_ignore_ascii_case(value)))
}

/// The pop-up's shown choice, polled until it reads the picked option or the
/// read-back timeout passes.
fn read_back_popup(element: AXUIElementRef, is_picked: impl Fn(&str) -> bool) -> Option<String> {
    let deadline = std::time::Instant::now() + POPUP_READ_BACK_TIMEOUT;
    loop {
        // Only AXValue is the choice: a title can be a fixed label.
        let shown = unsafe { copy_string_attr(element, "AXValue") };
        if shown.as_deref().is_some_and(&is_picked) || std::time::Instant::now() >= deadline {
            return shown;
        }
        std::thread::sleep(POPUP_POLL_INTERVAL);
    }
}

/// Wait for the thread whose AXPress opened the menu, so no AX call outlives
/// this tool call. The press returns once the menu closes or its timeout fires.
fn join_press(press_thread: &mut Option<std::thread::JoinHandle<()>>) {
    if let Some(press) = press_thread.take() {
        let _ = press.join();
    }
}

fn select_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    value: &str,
    element_title: &str,
) -> anyhow::Result<SetValueOutcome> {
    let mut options = popup_options(element);
    let mut opened = false;
    let mut press_thread = None;
    let mut menus_before = Vec::new();

    if matching_option(&option_pairs(&options), value).is_none() {
        // Safari/WebKit: no AX children while the popup is closed. Set the
        // <select> through the DOM instead of opening a menu.
        if options.is_empty() {
            let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
            if app_name == "Safari" {
                return set_select_via_js(element_index, element_title, value).map(|detail| {
                    SetValueOutcome {
                        detail,
                        verified: None,
                        changed: None,
                    }
                });
            }
        }
        // AppKit NSPopUpButton and Chromium <select> publish their items only
        // while the menu is open (a closed Chromium popup lists just the
        // selected one). Open it, wait for the full list, press the match,
        // and close the menu again whatever happens. While the menu is open
        // the app's menu tracking holds key focus, so the window that was key
        // loses it until the menu closes.
        //
        // The AXPress returns only once the app's menu tracking lets it (or
        // the messaging timeout fires), so run it on its own thread and read
        // the items as soon as they appear.
        let already_open = popup_menu_is_open(element);
        // A menu left open by an earlier click is this call's to close once it
        // presses an item there, so it is not counted as open before.
        if !already_open {
            menus_before = crate::windows::menu_window_ids(pid).unwrap_or_default();
        }
        let closed_titles = if already_open {
            Vec::new()
        } else {
            option_pairs(&options)
        };
        release_options(&options);
        opened = true;
        // Pressing an open popup again would close its menu.
        if !already_open {
            let popup_address = element as usize;
            press_thread = Some(std::thread::spawn(move || unsafe {
                let popup = popup_address as AXUIElementRef;
                crate::ax::bindings::AXUIElementSetMessagingTimeout(
                    popup,
                    POPUP_PRESS_TIMEOUT_SECONDS,
                );
                let _ = perform_action(popup, "AXPress");
                crate::ax::bindings::AXUIElementSetMessagingTimeout(popup, 0.0);
            }));
        }
        let deadline = std::time::Instant::now() + POPUP_OPEN_TIMEOUT;
        let mut last_seen: Vec<(String, String)> = Vec::new();
        let mut stable_polls = 0;
        loop {
            options = popup_options(element);
            let seen = option_pairs(&options);
            if matching_option(&seen, value).is_some() {
                break;
            }
            // The menu is open once the list differs from the closed one; it
            // is complete once the list stops changing.
            if !seen.is_empty() && seen != closed_titles {
                stable_polls = if seen == last_seen {
                    stable_polls + 1
                } else {
                    0
                };
                if stable_polls >= POPUP_STABLE_POLLS {
                    break;
                }
            }
            last_seen = seen;
            if std::time::Instant::now() >= deadline {
                break;
            }
            release_options(&options);
            std::thread::sleep(POPUP_POLL_INTERVAL);
        }
        if options.is_empty() {
            let closed = close_popup_menu(element, pid, &menus_before, false);
            join_press(&mut press_thread);
            anyhow::bail!(
                "AXPopUpButton [{element_index}] \"{element_title}\" exposed no options even \
                 after its menu was opened, so nothing was selected. Click the popup, then \
                 choose the option with press_key (down, return) or a pixel click on the item.{}",
                closed_note(closed)
            )
        }
    }

    let pick = match matching_option(&option_pairs(&options), value) {
        Some(i) => {
            let option = &options[i];
            let err = unsafe { perform_action(option.element, "AXPress") };
            if err == kAXErrorSuccess {
                Ok((option.title.clone(), option.value.clone()))
            } else {
                Err(format!(
                    "AXPress on option '{}' failed with AX error {err}",
                    option.title
                ))
            }
        }
        None => Err(format!(
            "No option matching '{value}' in AXPopUpButton [{element_index}] \"{element_title}\". \
             Available: [{}]",
            describe_available(&options)
        )),
    };
    release_options(&options);
    let (picked_title, picked_value) = match pick {
        Ok(picked) => picked,
        Err(error) => {
            let closed = if opened {
                close_popup_menu(element, pid, &menus_before, false)
            } else {
                Some(true)
            };
            join_press(&mut press_thread);
            anyhow::bail!("{error}.{}", closed_note(closed));
        }
    };
    let closed = if opened {
        close_popup_menu(element, pid, &menus_before, true)
    } else {
        Some(true)
    };
    join_press(&mut press_thread);

    // The pop-up may show the chosen option's title or its own value.
    let is_picked = |shown: &str| shows_option(shown, &picked_title, &picked_value);
    let picked = if picked_title.is_empty() {
        picked_value.clone()
    } else {
        picked_title.clone()
    };
    let shown = read_back_popup(element, is_picked);
    let (verified, how) = match &shown {
        Some(now) if is_picked(now) => (Some(true), format!("it now shows '{now}'")),
        Some(now) => (
            Some(false),
            format!("it still shows '{now}'; verify via screenshot"),
        ),
        None => (
            None,
            "its shown value is not readable through AX; could not confirm".to_owned(),
        ),
    };
    let mark = if verified == Some(true) {
        "✅ Selected"
    } else {
        "📨 Picked (unverified)"
    };
    let route = if opened {
        "opened its menu and pressed the item"
    } else {
        "pressed the item without opening the menu"
    };
    Ok(SetValueOutcome {
        detail: format!(
            "{mark} '{picked}' in AXPopUpButton [{element_index}] \"{element_title}\" ({route}); {how}.{}",
            closed_note(closed)
        ),
        verified,
        changed: None,
    })
}

#[cfg(test)]
mod popup_option_tests {
    use super::{is_blank_option, matching_option, shows_option};

    fn pairs(titles: &[&str]) -> Vec<(String, String)> {
        titles
            .iter()
            .map(|title| ((*title).to_owned(), String::new()))
            .collect()
    }

    #[test]
    fn matches_title_ignoring_case_and_whitespace() {
        let options = pairs(&["Open", "Duplicate", "Closed"]);
        assert_eq!(matching_option(&options, "duplicate"), Some(1));
        assert_eq!(matching_option(&options, "  Closed "), Some(2));
        assert_eq!(matching_option(&options, "Pending"), None);
    }

    #[test]
    fn matches_value_when_title_differs() {
        let options = vec![("Duplicate of".to_owned(), "dup".to_owned())];
        assert_eq!(matching_option(&options, "DUP"), Some(0));
    }

    /// T120: an option without an AXValue is not matched by an empty request
    /// (upstream picked "Daily" for ""), and an empty request matches nothing.
    #[test]
    fn a_missing_value_never_matches() {
        let options = vec![
            ("Daily".to_owned(), String::new()),
            (String::new(), "w".to_owned()),
            ("Weekly".to_owned(), "w".to_owned()),
        ];
        assert_eq!(matching_option(&options, ""), None);
        assert_eq!(matching_option(&options, "  "), None);
        assert_eq!(matching_option(&options, "daily"), Some(0));
        assert_eq!(matching_option(&options, "W"), Some(1));
        assert_eq!(matching_option(&options, "Monthly"), None);
    }

    #[test]
    fn separators_are_blank_options() {
        assert!(is_blank_option("", ""));
        assert!(is_blank_option(" ", ""));
        assert!(!is_blank_option("", "w"));
        assert!(!is_blank_option("Daily", ""));
    }

    #[test]
    fn a_shown_value_verifies_only_the_picked_option() {
        assert!(
            shows_option("w", "Weekly", "w"),
            "a pop-up that reports the value"
        );
        assert!(shows_option("weekly", "Weekly", "w"));
        assert!(shows_option(" Weekly ", "Weekly", ""));
        assert!(
            !shows_option("", "Daily", ""),
            "an empty value proves nothing"
        );
        assert!(
            !shows_option("", "", "x"),
            "an empty value never matches an empty title"
        );
        assert!(!shows_option("", "", ""));
        assert!(!shows_option("Daily", "Weekly", "w"));
        assert!(!shows_option("x", "", ""));
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

    use super::{
        file_name_needs_rename, is_file_name_cell, is_get_info_name_field, GET_INFO_RENAME_ROUTE,
        LIST_RENAME_ROUTE,
    };

    fn written() -> anyhow::Result<SetValueOutcome> {
        Ok(SetValueOutcome { detail: "✅ Set AXValue on [1] AXTextField.".into(), verified: Some(true), changed: Some(true) })
    }

    /// Runs the route with counters on every side effect.
    fn route(role: &str, settable: Option<bool>, catalyst: CatalystText) -> (SetValueAttempt, usize, usize, usize) {
        route_with(role, settable, catalyst, false)
    }

    fn route_with(
        role: &str,
        settable: Option<bool>,
        catalyst: CatalystText,
        search: bool,
    ) -> (SetValueAttempt, usize, usize, usize) {
        let (ancestry, focus, write) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let attempt = write_text_control(
            role,
            || settable,
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
            let (attempt, _, focus, write) = route_with(role, Some(true), CatalystText::Yes, true);
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
        let (attempt, _, focus, write) = route_with("AXTextField", Some(true), CatalystText::Yes, false);
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
    fn search_like_fields_are_named_by_role_subrole_or_their_text() {
        assert!(super::is_search_like("AXSearchField", None, &[]));
        assert!(super::is_search_like("AXTextField", Some("AXSearchField"), &[]));
        assert!(super::is_search_like("AXTextField", None, &[Some("Search messages".into())]));
        assert!(super::is_search_like("AXTextField", None, &[None, Some("probe-search".into())]));
        assert!(!super::is_search_like("AXTextField", None, &[Some("Display name".into()), None]));
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

    #[test]
    fn a_listed_file_name_is_a_file_name_cell() {
        let url = Some("file:///Users/me/lab/charlie.bin");
        assert!(is_file_name_cell(Some("charlie.bin"), url, Some(false)));
        assert!(is_file_name_cell(Some("charlie.bin"), url, None));
    }

    #[test]
    fn rename_editor_and_ordinary_fields_stay_writable() {
        let url = Some("file:///Users/me/lab/charlie.bin");
        // Finder's inline rename editor is focused while editing.
        assert!(!is_file_name_cell(Some("charlie.bin"), url, Some(true)));
        // A plain text field names no file.
        assert!(!is_file_name_cell(None, None, Some(false)));
        assert!(!is_file_name_cell(Some(""), url, Some(false)));
        assert!(!is_file_name_cell(Some("charlie.bin"), None, Some(false)));
        assert!(!is_file_name_cell(
            Some("page"),
            Some("https://example.com/page"),
            None
        ));
    }

    #[test]
    fn get_info_name_field_is_refused_and_its_neighbours_are_not() {
        const FINDER: Option<&str> = Some("com.apple.finder");
        // (bundle id, role, AXIdentifier, AXFocused, refused). Identifiers are
        // the ones Finder reported on macOS 26.4.
        let cases = [
            (FINDER, "AXTextField", Some("Name"), Some(false), true),
            (FINDER, "AXTextField", Some("Name"), None, true),
            // A real click starts an edit session; Return then commits an
            // AXValue write, so the focused field stays writable.
            (FINDER, "AXTextField", Some("Name"), Some(true), false),
            // The "Name & Extension" disclosure triangle shares the identifier.
            (FINDER, "AXDisclosureTriangle", Some("Name"), None, false),
            // Tags field, list inline rename editor, list name cell.
            (FINDER, "AXTextField", Some("_NS:34"), None, false),
            (
                FINDER,
                "AXTextField",
                Some("ShrinkToFit Text Field"),
                Some(true),
                false,
            ),
            (FINDER, "AXTextField", None, Some(false), false),
            (FINDER, "AXTextArea", Some("Comments"), None, false),
            // The same field shape in another app.
            (
                Some("com.example.notes"),
                "AXTextField",
                Some("Name"),
                None,
                false,
            ),
            (None, "AXTextField", Some("Name"), None, false),
        ];
        for (bundle, role, identifier, focused, refused) in cases {
            assert_eq!(
                is_get_info_name_field(bundle, Some(role), identifier, focused),
                refused,
                "{bundle:?} {role} {identifier:?} {focused:?}"
            );
        }
    }

    #[test]
    fn get_info_refusal_names_the_foreground_route() {
        let result = file_name_needs_rename(7, 42, GET_INFO_RENAME_ROUTE);
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "file_name_needs_rename");
        let reason = data["reason"].as_str().unwrap();
        for needed in [
            "Get Info",
            "nothing was written",
            "screenshot pixels",
            "capture_id",
            "cmd+a",
            "type_text",
            "return",
            "delivery_mode:\"foreground\"",
        ] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
    }

    #[test]
    fn file_name_refusal_names_the_rename_route() {
        let result = file_name_needs_rename(7, 42, LIST_RENAME_ROUTE);
        assert_eq!(result.is_error, Some(true));
        let data = result.structured_content.unwrap();
        assert_eq!(data["code"], "file_name_needs_rename");
        assert_eq!(data["effect"], "refused");
        assert_eq!(
            (data["pid"].as_i64(), data["window_id"].as_u64()),
            (Some(7), Some(42))
        );
        let reason = data["reason"].as_str().unwrap();
        for needed in [
            "never the file",
            "nothing was written",
            "return",
            "cmd+a",
            "type_text",
            "desktop",
        ] {
            assert!(reason.contains(needed), "missing {needed:?}: {reason}");
        }
    }

    /// A pop-up pick on web content that apply_surface_trust downgrades must
    /// not keep saying "Selected": the text and the effect agree.
    #[test]
    fn a_downgraded_pop_up_pick_does_not_say_selected() {
        let mut outcome = SetValueOutcome {
            detail: "✅ Selected 'Weekly' in AXPopUpButton [3] \"Digest\" (pressed the item \
                     without opening the menu); it now shows 'Weekly'."
                .to_owned(),
            verified: Some(true),
            changed: None,
        };
        apply_surface_trust(&mut outcome, true);
        apply_verification_label(&mut outcome);
        assert_eq!(outcome.verified, Some(false));
        assert!(!outcome.detail.contains("Selected"), "{}", outcome.detail);
        assert!(
            outcome
                .detail
                .starts_with("📨 Picked (unverified) 'Weekly'"),
            "{}",
            outcome.detail
        );
    }
}
