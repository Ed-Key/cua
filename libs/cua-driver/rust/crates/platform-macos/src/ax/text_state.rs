//! Focused text observation: the focused text control's selection for
//! snapshots and typing readback. Accessibility reports remain best-effort on
//! web content.
use cua_driver_core::walk_budget::WalkBudget;
use super::{bindings::*, tree::AXNode};
use cua_driver_contract::TextSelection;
use core_foundation::base::{CFEqual, CFType, CFTypeRef, TCFType};
use cua_driver_core::text_insertion::TextSelectionRange;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXValueGetTypeID() -> core_foundation::base::CFTypeID;
}

#[repr(C)]
struct NativeRange {
    location: isize,
    length: isize,
}

fn checked_range(location: isize, length: isize) -> Option<TextSelectionRange> {
    if location < 0 || length < 0 || location == isize::MAX {
        return None;
    }
    location.checked_add(length)?;
    Some(TextSelectionRange {
        location: location as u64,
        length: length as u64,
    })
}

unsafe fn decode_range(value: &CFType) -> Option<TextSelectionRange> {
    if value.type_of() != AXValueGetTypeID()
        || AXValueGetType(value.as_CFTypeRef() as AXValueRef) != kAXValueCFRangeType
    {
        return None;
    }
    let mut range = NativeRange {
        location: 0,
        length: 0,
    };
    if !AXValueGetValue(
        value.as_CFTypeRef() as AXValueRef,
        kAXValueCFRangeType,
        &mut range as *mut NativeRange as *mut _,
    ) {
        return None;
    }
    checked_range(range.location, range.length)
}

pub(crate) unsafe fn read_range(element: AXUIElementRef) -> Option<TextSelectionRange> {
    #[cfg(test)]
    if let Some(range) = super::bindings::test_support::typing_focus_range(element) {
        return range;
    }
    let attr = core_foundation::string::CFString::new("AXSelectedTextRange");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let value = CFType::wrap_under_create_rule(value);
    decode_range(&value)
}

unsafe fn focused(pid: i32) -> Option<CFType> {
    focused_element_of_pid(pid).map(|element| CFType::wrap_under_create_rule(element as CFTypeRef))
}

/// A default range on an unfocused control is not an active caret. Both reads
/// must still identify this exact control. Callers separately exclude web AX.
fn is_text_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
    )
}

fn read_selection_if_focused(
    role: &str,
    is_focused: bool,
    read: impl FnOnce() -> Option<TextSelection>,
) -> Option<TextSelection> {
    if is_focused && is_text_role(role) {
        read()
    } else {
        None
    }
}

fn consistent_selection(
    mut read_range: impl FnMut() -> Option<TextSelectionRange>,
    read_text: impl FnOnce() -> Option<String>,
) -> Option<TextSelection> {
    let range = read_range();
    let mut text = read_text();
    if read_range() != range {
        return None;
    }
    if let (Some(range), Some(value)) = (range, &text) {
        if value.encode_utf16().count() as u64 != range.length {
            text = None;
        }
    }
    (range.is_some() || text.is_some()).then_some(TextSelection { text, range })
}

/// Prefer app-level focus identity and read selection only on its matched node.
/// Discard that view if focus changes. Missing or unstable app identity can use
/// the separate best-effort web focus observation below.
///
/// Like the walk, this overruns its deadline by at most one AX read: every
/// read checks the budget first, and a confirmation that cannot finish leaves
/// focus and selection unknown.
pub(crate) unsafe fn enrich_focused_state(pid: i32, nodes: &mut [AXNode], budget: &WalkBudget) {
    if budget.expired() {
        retain_stable_focus(nodes, false);
        return;
    }
    let Some(before) = focused(pid) else {
        enrich_web_reported_focus(nodes, budget);
        return;
    };
    for node in nodes.iter_mut().filter(|node| node.element_index.is_some()) {
        let same = CFEqual(before.as_CFTypeRef(), node.element_ptr as CFTypeRef) != 0;
        node.focused = (same || is_text_role(&node.role)).then_some(same);
        node.text_selection = read_selection_if_focused(&node.role, same, || {
            read_selection(node.element_ptr as AXUIElementRef, budget)
        });
    }
    let unchanged = !budget.expired()
        && focused(pid).is_some_and(|after| CFEqual(before.as_CFTypeRef(), after.as_CFTypeRef()) != 0);
    retain_stable_focus(nodes, unchanged);
    if !unchanged {
        enrich_web_reported_focus(nodes, budget);
    }
}

/// None when the deadline passes before every read is done: a partial
/// selection could pair a range with text from different moments.
unsafe fn read_selection(element: AXUIElementRef, budget: &WalkBudget) -> Option<TextSelection> {
    if budget.expired()
        || copy_string_attr(element, "AXSubrole").as_deref() == Some("AXSecureTextField")
    {
        return None;
    }
    let cut = std::cell::Cell::new(false);
    let expired = || {
        cut.set(cut.get() || budget.expired());
        cut.get()
    };
    let selection = consistent_selection(
        || if expired() { None } else { read_range(element) },
        || if expired() { None } else { copy_string_attr(element, "AXSelectedText") },
    );
    (!cut.get()).then_some(selection).flatten()
}

fn unique_reported_focus(reported: &[Option<bool>]) -> Option<usize> {
    let mut matches = reported
        .iter()
        .enumerate()
        .filter(|(_, value)| **value == Some(true));
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

fn stable_reported_selection(
    selection: Option<TextSelection>,
    after: Option<bool>,
) -> Option<TextSelection> {
    (after == Some(true)).then_some(selection).flatten()
}

/// Background Electron can expose AXFocused on its editor while the app-level
/// focused-element lookup is unavailable. This is reported web AX state only;
/// native key confirmation must never use this fallback. It reads AXFocused
/// per web text control, so it stops at the walk's deadline and then leaves
/// focus unknown: a partial read cannot show that exactly one control claims it.
unsafe fn enrich_web_reported_focus(nodes: &mut [AXNode], budget: &WalkBudget) {
    let indices: Vec<_> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            node.element_index.is_some() && node.in_web_content && is_text_role(&node.role)
        })
        .map(|(index, _)| index)
        .collect();
    let mut reported = Vec::with_capacity(indices.len());
    for &index in &indices {
        if budget.expired() {
            for &index in &indices {
                nodes[index].focused = None;
                nodes[index].text_selection = None;
            }
            return;
        }
        reported.push(copy_bool_attr(nodes[index].element_ptr as AXUIElementRef, "AXFocused"));
    }
    for (&index, value) in indices.iter().zip(&reported) {
        // Multiple claimed focuses remain unknown, not multiple active editors.
        nodes[index].focused = (*value == Some(false)).then_some(false);
        nodes[index].text_selection = None;
    }
    let Some(position) = unique_reported_focus(&reported) else {
        return;
    };
    let node = &mut nodes[indices[position]];
    let element = node.element_ptr as AXUIElementRef;
    let selection = read_selection(element, budget);
    if budget.expired() {
        node.focused = None;
        node.text_selection = None;
        return;
    }
    let after = copy_bool_attr(element, "AXFocused");
    node.focused = (after == Some(true)).then_some(true);
    node.text_selection = stable_reported_selection(selection, after);
}

pub(super) fn retain_stable_focus(nodes: &mut [AXNode], unchanged: bool) {
    if !unchanged {
        for node in nodes {
            node.focused = None;
            node.text_selection = None;
        }
    }
}

/// A default range on an unfocused control is not an active caret. Both reads
/// must still identify this exact control. Callers separately exclude web AX.
pub(crate) unsafe fn focused_range(
    pid: i32,
    element: AXUIElementRef,
) -> Option<TextSelectionRange> {
    let before = focused(pid)?;
    if CFEqual(before.as_CFTypeRef(), element as CFTypeRef) == 0 {
        return None;
    }
    let range = read_range(element)?;
    let after = focused(pid)?;
    (CFEqual(before.as_CFTypeRef(), after.as_CFTypeRef()) != 0).then_some(range)
}

#[cfg(test)]
mod tests {
    #[test]
    fn web_fallback_at_the_deadline_reads_nothing_and_leaves_focus_unknown() {
        let mut budget = WalkBudget::new(0, 10);
        let _ = budget.admit(); // starts the clock; a zero budget refuses at once
        assert!(budget.expired());
        // A null element would crash any AX read, so reaching the end proves
        // none was made.
        let mut nodes = vec![AXNode {
            element_index: Some(0),
            role: "AXTextArea".into(),
            title: None,
            value: None,
            description: None,
            identifier: None,
            help: None,
            actions: vec![],
            element_ptr: 0,
            depth: 0,
            parent_element_index: None,
            frame: None,
            value_state: None,
            value_description: None,
            placeholder: None,
            value_settable: None,
            focused: Some(true),
            text_selection: Some(TextSelection { text: None, range: checked_range(0, 0) }),
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: true,
        }];
        unsafe { enrich_web_reported_focus(&mut nodes, &budget) };
        assert_eq!(nodes[0].focused, None);
        assert_eq!(nodes[0].text_selection, None);
    }

    #[test]
    fn web_fallback_requires_one_stable_reported_text_focus() {
        assert_eq!(
            unique_reported_focus(&[Some(false), Some(true), None]),
            Some(1)
        );
        assert_eq!(unique_reported_focus(&[Some(true), Some(true)]), None);
        assert_eq!(unique_reported_focus(&[Some(false), None]), None);
        let selection = TextSelection {
            text: Some("A".into()),
            range: checked_range(0, 1),
        };
        assert_eq!(
            stable_reported_selection(Some(selection.clone()), Some(true)),
            Some(selection)
        );
        assert_eq!(
            stable_reported_selection(
                Some(TextSelection {
                    text: None,
                    range: checked_range(0, 1)
                }),
                None
            ),
            None
        );
    }

    #[test]
    fn changing_selection_discards_mixed_text_and_range() {
        let mut ranges = [checked_range(0, 1), checked_range(0, 3)].into_iter();
        assert_eq!(
            consistent_selection(|| ranges.next().unwrap(), || Some("A😀".into())),
            None
        );
        let selection = consistent_selection(|| checked_range(0, 3), || Some("A".into())).unwrap();
        assert_eq!(selection.range, checked_range(0, 3));
        assert!(
            selection.text.is_none(),
            "mismatched text must not claim the range"
        );
        assert_eq!(
            consistent_selection(|| None, || Some("A".into())),
            Some(TextSelection {
                text: Some("A".into()),
                range: None
            })
        );
    }

    #[test]
    fn only_focused_text_controls_read_selection_and_empty_is_not_unknown() {
        for (role, focus) in [
            ("AXTextField", false),
            ("AXButton", true),
            ("AXSecureTextField", true),
        ] {
            assert_eq!(
                read_selection_if_focused(role, focus, || panic!("unexpected read")),
                None
            );
        }
        let selection = TextSelection {
            text: Some(String::new()),
            range: checked_range(5, 0),
        };
        assert_eq!(
            read_selection_if_focused("AXTextField", true, || Some(selection.clone())),
            Some(selection)
        );
        assert_eq!(
            read_selection_if_focused("AXTextField", true, || None),
            None
        );
    }

    use super::*;

    #[test]
    fn ranges_reject_negative_overflowing_and_sentinel_values() {
        assert_eq!(checked_range(-1, 0), None);
        assert_eq!(checked_range(0, -1), None);
        assert_eq!(checked_range(isize::MAX, 0), None);
        assert_eq!(checked_range(isize::MAX - 1, 5), None);
        assert_eq!(
            checked_range(3, 2),
            Some(TextSelectionRange { location: 3, length: 2 })
        );
    }
}
