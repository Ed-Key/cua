//! Focused text observation. Accessibility reports remain best-effort on web content.
use super::{bindings::*, tree::AXNode};
use core_foundation::base::{CFEqual, CFType, CFTypeRef, TCFType};
use cua_driver_contract::{TextSelection, TextSelectionRange};

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
    let value = copy_attribute_checked(element, "AXSelectedTextRange").ok()?;
    decode_range(&value)
}

unsafe fn focused(pid: i32) -> Option<CFType> {
    focused_element_of_pid(pid).map(|element| CFType::wrap_under_create_rule(element as CFTypeRef))
}

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

/// Read only the matched focused text node. Other nodes do not pay per-node AX
/// focus queries. If focus moves during the read, discard the whole focus view.
pub(crate) unsafe fn enrich_focused_state(pid: i32, nodes: &mut [AXNode]) {
    let Some(before) = focused(pid) else { return };
    for node in nodes.iter_mut().filter(|node| node.element_index.is_some()) {
        let same = CFEqual(before.as_CFTypeRef(), node.element_ptr as CFTypeRef) != 0;
        node.focused = (same || is_text_role(&node.role)).then_some(same);
        node.text_selection = read_selection_if_focused(&node.role, same, || {
            let element = node.element_ptr as AXUIElementRef;
            if copy_string_attr(element, "AXSubrole").as_deref() == Some("AXSecureTextField") {
                return None;
            }
            consistent_selection(
                || read_range(element),
                || copy_string_attr(element, "AXSelectedText"),
            )
        });
    }
    let unchanged =
        focused(pid).is_some_and(|after| CFEqual(before.as_CFTypeRef(), after.as_CFTypeRef()) != 0);
    retain_stable_focus(nodes, unchanged);
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
    use super::*;
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
    fn range_preserves_caret_and_utf16_offsets_and_rejects_invalid_values() {
        assert_eq!(
            checked_range(5, 0),
            Some(TextSelectionRange {
                location: 5,
                length: 0
            })
        );
        assert_eq!(
            checked_range(0, 3),
            Some(TextSelectionRange {
                location: 0,
                length: 3
            })
        );
        for (location, length) in [(-1, 0), (0, -1), (isize::MAX, 0), (isize::MAX - 1, 2)] {
            assert_eq!(checked_range(location, length), None);
        }
    }
    #[test]
    fn decodes_only_cf_range_ax_values() {
        unsafe {
            let range = NativeRange {
                location: 0,
                length: 3,
            };
            let raw = AXValueCreate(kAXValueCFRangeType, &range as *const _ as *const _);
            assert!(!raw.is_null());
            let value = CFType::wrap_under_create_rule(raw as CFTypeRef);
            assert_eq!(decode_range(&value), checked_range(0, 3));
            let string = core_foundation::string::CFString::new("not a range").as_CFType();
            assert_eq!(decode_range(&string), None);
            let point = [1.0_f64, 2.0];
            let raw = AXValueCreate(kAXValueCGPointType, point.as_ptr() as *const _);
            let value = CFType::wrap_under_create_rule(raw as CFTypeRef);
            assert_eq!(decode_range(&value), None);
        }
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
}
