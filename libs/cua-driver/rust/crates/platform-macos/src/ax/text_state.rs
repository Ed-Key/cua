//! Focused text selection readback for typing verification.
//!
//! ponytail: only what typing needs (the focused control's
//! `AXSelectedTextRange`). The text-selection topic grows this into the
//! full observation module and moves the range type onto the contract.
use super::bindings::*;
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
