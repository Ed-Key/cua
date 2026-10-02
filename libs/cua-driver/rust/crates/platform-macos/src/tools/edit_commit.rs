//! Text fields whose app saves them only when editing ends.
//!
//! AppKit tells a field's owner about an edit when editing ends (the field
//! resigns first responder: Tab, Return in a one-line field, a click on
//! another control). Finder saves its Get Info fields (name, comments, tags)
//! and its inline rename field only then: an accessibility value write or
//! typed text shows in the field, but nothing is saved until the edit ends.
//! A result about such a field must say so instead of "confirmed".

use crate::ax::bindings::{
    attribute_settable, copy_children, copy_element_attr, copy_string_attr, set_bool_attr_true,
    AXUIElementRef,
};
use core_foundation::base::{CFRelease, CFTypeRef};

/// Apps that save a text field only when its editing ends.
/// ponytail: a bundle-id list; add an app when its fields are seen to wait
/// for the end of editing.
const SAVES_ON_END_EDITING: &[&str] = &["com.apple.finder"];

/// How a pending edit of one field is ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    /// A one-line field: Return ends the edit.
    OneLine,
    /// A multi-line text area: Return is a new line; only moving the focus
    /// (Tab) ends the edit.
    MultiLine,
}

/// The pending-commit field kind for a text control of `pid`, or `None`
/// when its app saves as the text changes (or it is a search field, which
/// acts on each keystroke).
pub(crate) fn pending_field(pid: i32, role: &str, search_like: bool) -> Option<Field> {
    field_kind(app_saves_on_end_editing(pid), role, search_like)
}

/// Whether `pid`'s app saves its text fields only when editing ends.
pub(crate) fn app_saves_on_end_editing(pid: i32) -> bool {
    crate::apps::bundle_id_for_pid(pid).is_some_and(|id| SAVES_ON_END_EDITING.contains(&id.as_str()))
}

/// What a refusal of a read-only field adds for such an app: Finder's Get
/// Info fields are read-only while the item is locked.
pub(crate) fn read_only_note(pid: i32) -> &'static str {
    if app_saves_on_end_editing(pid) {
        " If this is Finder's Get Info for a locked item, its fields stay read-only while the \
         item is locked: clear its Locked checkbox, write the field and end the edit, then check \
         Locked again."
    } else {
        ""
    }
}

fn field_kind(app_saves_late: bool, role: &str, search_like: bool) -> Option<Field> {
    if !app_saves_late || search_like {
        return None;
    }
    match role {
        "AXTextField" | "AXComboBox" => Some(Field::OneLine),
        "AXTextArea" => Some(Field::MultiLine),
        _ => None,
    }
}

/// [`pending_field`] for a live element.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub(crate) unsafe fn pending_field_of(pid: i32, element: AXUIElementRef) -> Option<Field> {
    let role = copy_string_attr(element, "AXRole").unwrap_or_default();
    let subrole = copy_string_attr(element, "AXSubrole");
    let search = role == "AXSearchField" || subrole.as_deref() == Some("AXSearchField");
    pending_field(pid, &role, search)
}

/// The words for a field that shows text its app has not saved.
pub(crate) fn not_committed(field: Field, app: &str) -> String {
    let key = match field {
        Field::OneLine => "press_key return",
        Field::MultiLine => "press_key tab (or click another control)",
    };
    format!(
        "{app} saves this field only when its editing ends, so nothing is saved yet. End the \
         edit: {key} on this window (delivery_mode:\"foreground\" if it is refused), then check \
         what {app} saved."
    )
}

/// What ended an edit, as the result names it.
pub(crate) struct Ended {
    pub(crate) focus: String,
}

/// End the edit of `field` the way Tab does: move the keyboard focus to the
/// first control of its window, in reading order, that is not a text field
/// and accepts focus. Returns what took the focus, once `field` reads as no
/// longer focused; `None` when nothing could take it.
///
/// # Safety
///
/// `field` must be a valid `AXUIElementRef` for the duration of the call.
pub(crate) unsafe fn end_edit(field: AXUIElementRef) -> Option<Ended> {
    // Bounded in time and per read, so a slow app cannot hold set_value:
    // past the budget the result falls back to "not committed".
    const READ_TIMEOUT_SECONDS: f32 = 0.2;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let window = copy_element_attr(field, "AXWindow")?;
    let mut queue = vec![window];
    let mut seen = 0usize;
    let mut ended = None;
    // Breadth first, bounded: Get Info's icon sits near the top.
    while !queue.is_empty() && seen < 300 && ended.is_none() && std::time::Instant::now() < deadline {
        let current = queue.remove(0);
        crate::ax::bindings::AXUIElementSetMessagingTimeout(current, READ_TIMEOUT_SECONDS);
        seen += 1;
        let role = copy_string_attr(current, "AXRole").unwrap_or_default();
        if seen > 1
            && takes_focus_without_editing(&role)
            && attribute_settable(current, "AXFocused") == Some(true)
            && set_bool_attr_true(current, "AXFocused") == 0
        {
            std::thread::sleep(std::time::Duration::from_millis(150));
            // Only a read that says the field lost the focus counts.
            if crate::ax::bindings::copy_bool_attr(field, "AXFocused") == Some(false) {
                let label = ["AXTitle", "AXDescription", "AXIdentifier"]
                    .iter()
                    .find_map(|name| copy_string_attr(current, name).filter(|t| !t.is_empty()))
                    .unwrap_or_default();
                ended = Some(Ended { focus: format!("{role} {}", serde_json::json!(label)) });
            }
        } else if !matches!(role.as_str(), "AXMenuBar" | "AXMenu") {
            queue.extend(copy_children(current));
        }
        CFRelease(current as CFTypeRef);
    }
    for rest in queue {
        CFRelease(rest as CFTypeRef);
    }
    ended
}

/// A control that can hold the keyboard focus without starting an edit or
/// acting (no text entry, no button or checkbox that a later Space would
/// press).
fn takes_focus_without_editing(role: &str) -> bool {
    matches!(role, "AXImage" | "AXStaticText" | "AXList" | "AXOutline" | "AXTable")
}

#[cfg(test)]
mod tests {
    use super::{field_kind, not_committed, takes_focus_without_editing, Field};

    #[test]
    fn only_text_fields_of_an_app_that_saves_late_are_pending() {
        assert_eq!(field_kind(true, "AXTextField", false), Some(Field::OneLine));
        assert_eq!(field_kind(true, "AXComboBox", false), Some(Field::OneLine));
        assert_eq!(field_kind(true, "AXTextArea", false), Some(Field::MultiLine));
        assert_eq!(field_kind(true, "AXTextField", true), None, "a search acts on each key");
        assert_eq!(field_kind(true, "AXCheckBox", false), None);
        assert_eq!(field_kind(false, "AXTextArea", false), None, "other apps keep today's results");
    }

    #[test]
    fn the_words_name_the_key_that_ends_the_edit() {
        let one = not_committed(Field::OneLine, "Finder");
        assert!(one.contains("nothing is saved yet") && one.contains("press_key return"), "{one}");
        let multi = not_committed(Field::MultiLine, "Finder");
        assert!(multi.contains("press_key tab") && !multi.contains("return"), "{multi}");
    }

    #[test]
    fn focus_moves_only_to_controls_that_do_not_edit_or_act() {
        assert!(takes_focus_without_editing("AXImage"));
        for role in ["AXTextField", "AXTextArea", "AXButton", "AXCheckBox", "AXComboBox"] {
            assert!(!takes_focus_without_editing(role), "{role}");
        }
    }
}

/// The typing result for a field whose app has not saved the text yet: the
/// summary says so first and the effect is no longer "confirmed". Only a
/// confirmed insertion is changed; every other result keeps its own words.
pub(crate) fn mark_typed_not_committed(
    mut result: cua_driver_core::protocol::ToolResult,
    field: Field,
    app: &str,
) -> cua_driver_core::protocol::ToolResult {
    use cua_driver_core::protocol::Content;
    let confirmed = result
        .structured_content
        .as_ref()
        .is_some_and(|data| data["effect"] == "confirmed");
    if !confirmed {
        return result;
    }
    if let Some(Content::Text { text, .. }) = result.content.first_mut() {
        if let Some(rest) = text.strip_prefix("✅ Inserted") {
            *text = format!("⚠️ Typed, not committed: inserted{rest} {}", not_committed(field, app));
        }
    }
    if let Some(data) = result.structured_content.as_mut() {
        data["effect"] = serde_json::json!("unverifiable");
        data["verified"] = serde_json::json!(false);
    }
    result
}

#[cfg(test)]
mod typing_tests {
    use super::{mark_typed_not_committed, Field};
    use cua_driver_core::protocol::{Content, ToolResult};

    #[test]
    fn a_confirmed_insertion_into_a_late_saving_field_is_not_confirmed() {
        let typed = ToolResult::text("✅ Inserted 5 char(s) into [3] AXTextArea \"\".")
            .with_structured(serde_json::json!({"effect": "confirmed", "verified": true}));
        let marked = mark_typed_not_committed(typed, Field::MultiLine, "Finder");
        let data = marked.structured_content.as_ref().unwrap();
        assert_eq!((data["effect"].as_str(), data["verified"].as_bool()), (Some("unverifiable"), Some(false)));
        match &marked.content[0] {
            Content::Text { text, .. } => {
                assert!(text.starts_with("⚠️ Typed, not committed: inserted 5 char(s)"), "{text}");
                assert!(text.contains("press_key tab"), "{text}");
            }
            _ => panic!("text"),
        }
        // Unconfirmed typing keeps its own words.
        let unsure = ToolResult::text("⚠️ Not confirmed: the keys were sent")
            .with_structured(serde_json::json!({"effect": "unverifiable", "verified": false}));
        let kept = mark_typed_not_committed(unsure, Field::OneLine, "Finder");
        assert!(matches!(&kept.content[0], Content::Text { text, .. } if text == "⚠️ Not confirmed: the keys were sent"));
    }
}
