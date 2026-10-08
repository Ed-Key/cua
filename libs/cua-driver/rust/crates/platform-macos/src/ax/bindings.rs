//! Raw FFI bindings to the macOS Accessibility API (AXUIElement).
//!
//! We call the C-level AX API directly rather than using a crate wrapper,
//! because most available crates are incomplete or unmaintained.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]

use core_foundation::{
    array::CFArrayRef,
    base::{CFRelease, CFRetain, CFTypeID, CFTypeRef},
    string::CFStringRef,
};
use std::os::raw::{c_int, c_void};

// ── AXUIElement opaque type ──────────────────────────────────────────────────

#[repr(C)]
pub struct __AXUIElement(c_void);
pub type AXUIElementRef = *mut __AXUIElement;

// ── AXError ──────────────────────────────────────────────────────────────────

pub type AXError = c_int;
pub const kAXErrorSuccess: AXError = 0;
pub const kAXErrorFailure: AXError = -25200;
pub const kAXErrorInvalidUIElement: AXError = -25202;
pub const kAXErrorCannotComplete: AXError = -25204;
pub const kAXErrorAttributeUnsupported: AXError = -25205;
pub const kAXErrorActionUnsupported: AXError = -25206;
pub const kAXErrorNotImplemented: AXError = -25208;
pub const kAXErrorNoValue: AXError = -25212;
pub const kAXErrorAPIDisabled: AXError = -25211;

// ── AXValue opaque type ──────────────────────────────────────────────────────

#[repr(C)]
pub struct __AXValue(c_void);
pub type AXValueRef = *mut __AXValue;

pub type AXValueType = c_int;
pub const kAXValueCGPointType: AXValueType = 1;
pub const kAXValueCGSizeType: AXValueType = 2;
pub const kAXValueCGRectType: AXValueType = 3;
pub const kAXValueCFRangeType: AXValueType = 4;
pub const kAXValueIllegalType: AXValueType = 1_000;

// ── Link to AXUIElement functions ────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementCopyAttributeNames(
        element: AXUIElementRef,
        names: *mut CFArrayRef,
    ) -> AXError;
    pub fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    pub fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f32,
        y: f32,
        element: *mut AXUIElementRef,
    ) -> AXError;
    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementIsAttributeSettable(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> AXError;
    pub fn AXUIElementSetMessagingTimeout(
        element: AXUIElementRef,
        timeout_in_seconds: f32,
    ) -> AXError;
    pub fn AXUIElementGetTypeID() -> CFTypeID;
    pub fn AXValueGetTypeID() -> CFTypeID;
    pub fn AXIsProcessTrusted() -> bool;
    /// `AXIsProcessTrustedWithOptions(options)` — when called with
    /// `{kAXTrustedCheckOptionPrompt: true}` raises the system Accessibility
    /// prompt if the process isn't already trusted.  Returns the post-prompt
    /// trust state (may still be false if the user dismissed the prompt).
    pub fn AXIsProcessTrustedWithOptions(
        options: core_foundation::dictionary::CFDictionaryRef,
    ) -> bool;

    /// Private SPI: maps an AX window element to its CGWindowID.
    /// Stable since macOS 10.9.
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;

    /// Private SPI: materializes an AX element from its 20-byte remote token
    /// (pid, 0, `'coco'`, element id). Reaches windows on other Spaces, which
    /// `AXWindows` omits. Used by alt-tab-macos for the same purpose.
    pub fn _AXUIElementCreateWithRemoteToken(
        token: core_foundation::data::CFDataRef,
    ) -> AXUIElementRef;
}

/// Hit-test one process's accessibility tree at a screen point. The returned
/// element is retained and must be released by the caller.
///
/// # Safety
///
/// The caller must release any returned element exactly once with `CFRelease`.
pub unsafe fn element_at_screen_position(pid: i32, x: f64, y: f64) -> Option<AXUIElementRef> {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return None;
    }
    let mut element = std::ptr::null_mut();
    let error = AXUIElementCopyElementAtPosition(application, x as f32, y as f32, &mut element);
    CFRelease(application as CFTypeRef);
    (error == kAXErrorSuccess && !element.is_null()).then_some(element)
}

// ── AXValue functions ────────────────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXValueCreate(the_type: AXValueType, value_ptr: *const c_void) -> AXValueRef;
    pub fn AXValueGetType(value: AXValueRef) -> AXValueType;
    pub fn AXValueGetValue(
        value: AXValueRef,
        the_type: AXValueType,
        value_ptr: *mut c_void,
    ) -> bool;
}

#[repr(C)]
struct CGPointValue {
    x: f64,
    y: f64,
}

#[repr(C)]
struct CGSizeValue {
    width: f64,
    height: f64,
}

// ── Helper functions ──────────────────────────────────────────────────────────

use core_foundation::{array::CFArray, base::TCFType, string::CFString as CFStr};

#[cfg(test)]
#[path = "bindings_test_support.rs"]
pub(crate) mod test_support;

/// Whether an AX attribute is currently writable on this element.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn is_attribute_settable(element: AXUIElementRef, attr_name: &str) -> bool {
    let attr = CFStr::new(attr_name);
    let mut settable = 0_u8;
    AXUIElementIsAttributeSettable(element, attr.as_concrete_TypeRef(), &mut settable)
        == kAXErrorSuccess
        && settable != 0
}

/// Whether an AX attribute is writable: `Some(true/false)` as reported, or
/// `None` when the query itself failed (unknown, not "read-only").
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn attribute_settable(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    let attr = CFStr::new(attr_name);
    let mut settable = 0_u8;
    (AXUIElementIsAttributeSettable(element, attr.as_concrete_TypeRef(), &mut settable)
        == kAXErrorSuccess)
        .then_some(settable != 0)
}

/// Convert a borrowed URL attribute without changing control-value coercion.
unsafe fn coerce_url_value(value: CFTypeRef) -> Option<String> {
    use core_foundation::url::CFURL;
    if value.is_null() || core_foundation::base::CFGetTypeID(value) != CFURL::type_id() {
        return None;
    }
    let url = CFURL::wrap_under_get_rule(value as _)
        .absolute()
        .get_string()
        .to_string();
    (!url.is_empty()).then_some(url)
}

/// Read the dedicated AXURL attribute, whose documented type is CFURLRef.
///
/// # Safety
/// `element` must be a valid, live AXUIElementRef for the duration of the call.
pub unsafe fn copy_url_attr(element: AXUIElementRef) -> Option<String> {
    let attr = CFStr::new("AXURL");
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let url = coerce_url_value(value);
    CFRelease(value);
    url
}

/// Copy a string attribute from an AX element. Returns `None` on any error.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_string_attr(element: AXUIElementRef, attr_name: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(result) = test_support::copy_string_attr(element, attr_name) {
        return result;
    }
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let text = cf_plain_string(value);
    CFRelease(value);
    text
}

extern "C" {
    fn CFAttributedStringGetString(string: CFTypeRef) -> CFStringRef;
}

/// Text of a borrowed `CFString` or `CFAttributedString` (formatted text:
/// Catalyst and TextKit fields report `AXValue` that way). Anything else is
/// not text.
///
/// # Safety
///
/// `value` must be a valid CF object for the duration of the call.
pub unsafe fn cf_plain_string(value: CFTypeRef) -> Option<String> {
    use core_foundation::attributed_string::CFAttributedString;
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFStr::type_id() {
        return Some(CFStr::wrap_under_get_rule(value as _).to_string());
    }
    if type_id == CFAttributedString::type_id() {
        let string = CFAttributedStringGetString(value);
        return (!string.is_null()).then(|| CFStr::wrap_under_get_rule(string).to_string());
    }
    None
}

/// Copy a numeric attribute from an AX element as an `f64`. Returns `None` on
/// any error or if the attribute is not a `CFNumber`. SwiftUI sliders expose a
/// readable numeric `AXValue` even when that value is not settable — this lets
/// the stepping fallback read the control's current position for feedback.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_number_attr(element: AXUIElementRef, attr_name: &str) -> Option<f64> {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let cf_number_type_id = CFNumber::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_number_type_id {
        CFRelease(value);
        return None;
    }
    let n = CFNumber::wrap_under_create_rule(value as _);
    n.to_f64()
}

/// Copy a boolean attribute from an AX element. Returns `None` on any error
/// or if the attribute is neither a `CFBoolean` nor a `CFNumber` (some apps
/// report AXEnabled/AXSelected as a 0/1 CFNumber instead of a CFBoolean).
///
/// # Safety
///
/// `element` must be a valid Accessibility object reference for the duration
/// of this call.
pub unsafe fn copy_bool_attr(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    #[cfg(test)]
    if let Some(result) = test_support::copy_bool_attr(element, attr_name) {
        return result;
    }
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFBoolean::type_id() {
        let b = CFBoolean::wrap_under_create_rule(value as _);
        return Some(b.into());
    }
    if type_id == CFNumber::type_id() {
        let n = CFNumber::wrap_under_create_rule(value as _);
        return n.to_f64().map(|f| f != 0.0);
    }
    CFRelease(value);
    None
}

pub(crate) unsafe fn coerce_binary_value(value: CFTypeRef) -> Option<bool> {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFBoolean::type_id() {
        return Some(CFBoolean::wrap_under_get_rule(value as _).into());
    }
    if type_id == CFNumber::type_id() {
        return match CFNumber::wrap_under_get_rule(value as _).to_f64()? {
            0.0 => Some(false),
            1.0 => Some(true),
            _ => None,
        };
    }
    None
}

/// Read a boolean-valued AX attribute (CFBoolean, or a 0/1 CFNumber).
///
/// # Safety
///
/// `element` must be a valid, retained `AXUIElementRef` for the duration of
/// the call.
pub unsafe fn copy_binary_attr(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let result = coerce_binary_value(value);
    CFRelease(value);
    result
}

/// A copied AX attribute represented for both existing string-only consumers
/// and the wider structured control-state response.
#[derive(Debug, PartialEq, Eq)]
pub struct StringishAttrValue {
    /// Present only when the source value was text (CFString or CFAttributedString).
    pub string_value: Option<String>,
    /// CFString as-is, CFNumber as text, or CFBoolean as `"1"` / `"0"`.
    pub state_value: String,
}

/// Convert a borrowed CF value without taking ownership of it.
unsafe fn coerce_stringish_value(value: CFTypeRef) -> Option<StringishAttrValue> {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    if let Some(string) = cf_plain_string(value) {
        return Some(StringishAttrValue {
            string_value: Some(string.clone()),
            state_value: string,
        });
    }
    let type_id = core_foundation::base::CFGetTypeID(value);
    if type_id == CFNumber::type_id() {
        let n = CFNumber::wrap_under_get_rule(value as _);
        let f = n.to_f64()?;
        let state_value = if f == f.trunc() && f.abs() < 1e15 {
            format!("{}", f as i64)
        } else {
            format!("{f}")
        };
        return Some(StringishAttrValue {
            string_value: None,
            state_value,
        });
    }
    if type_id == CFBoolean::type_id() {
        let b = CFBoolean::wrap_under_get_rule(value as _);
        return Some(StringishAttrValue {
            string_value: None,
            state_value: if bool::from(b) {
                "1".into()
            } else {
                "0".into()
            },
        });
    }
    None
}

/// Copy an attribute that may be a `CFString`, `CFNumber`, or `CFBoolean`.
///
/// The returned pair lets the tree walker preserve its historical CFString-only
/// markdown while using the same single AX read for structured control state.
/// Numbers render without a trailing `.0` when integral (`8`, not `8.0`), and
/// booleans render as `1`/`0` to match AppKit's two-state controls.
///
/// # Safety
///
/// `element` must be a valid Accessibility object reference for the duration
/// of this call.
pub unsafe fn copy_stringish_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<StringishAttrValue> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let result = coerce_stringish_value(value);
    CFRelease(value);
    result
}

/// Whether the element lists `name` among its attribute names (it may still
/// have no value for it right now, as an empty selection has none).
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn advertises_attribute(element: AXUIElementRef, name: &str) -> bool {
    let mut names: CFArrayRef = std::ptr::null_mut();
    if AXUIElementCopyAttributeNames(element, &mut names) != kAXErrorSuccess || names.is_null() {
        return false;
    }
    let arr = CFArray::<CFStr>::wrap_under_create_rule(names);
    let found = (0..arr.len()).any(|i| arr.get(i).is_some_and(|cf| cf.to_string() == name));
    found
}

/// Get the action names for an AX element.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn copy_action_names(element: AXUIElementRef) -> Vec<String> {
    #[cfg(test)]
    if let Some(result) = test_support::copy_action_names(element) {
        return result;
    }
    let mut names: CFArrayRef = std::ptr::null_mut();
    let err = AXUIElementCopyActionNames(element, &mut names);
    if err != kAXErrorSuccess || names.is_null() {
        return vec![];
    }
    // Use CFArray<CFStr> (the typed wrapper) to satisfy FromVoid bound.
    let arr = CFArray::<CFStr>::wrap_under_create_rule(names);
    (0..arr.len())
        .filter_map(|i| {
            let cf = arr.get(i)?;
            Some(cf.to_string())
        })
        .collect()
}

/// Read the on-screen center of an AX element (AXPosition + AXSize → center).
/// Returns `(cx, cy)` in screen coordinates, or `None` if either attribute
/// is unavailable or the element has zero size.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn element_screen_center(element: AXUIElementRef) -> Option<(f64, f64)> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some((pos.x + sz.w / 2.0, pos.y + sz.h / 2.0))
}

/// Read the on-screen bounding rect of an AX element.
/// Returns `[x, y, width, height]` in screen coordinates (top-left origin), or `None`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn element_screen_rect(element: AXUIElementRef) -> Option<[f64; 4]> {
    #[cfg(test)]
    if let Some(result) = test_support::editor_rect(element) {
        return result;
    }
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some([pos.x, pos.y, sz.w, sz.h])
}

/// Get the focused UI element of a running application by pid.
/// Returns a retained `AXUIElementRef` that the caller must release, or `None`.
///
/// # Safety
///
/// The caller must release any returned element exactly once with `CFRelease`.
pub unsafe fn focused_element_of_pid(pid: i32) -> Option<AXUIElementRef> {
    #[cfg(test)]
    if let Some(result) = test_support::typing_focus_element(pid, None) {
        return result;
    }
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let attr = CFStr::new("AXFocusedUIElement");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(app, attr.as_concrete_TypeRef(), &mut value);
    CFRelease(app as CFTypeRef);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let ax_type_id = AXUIElementGetTypeID();
    if core_foundation::base::CFGetTypeID(value) != ax_type_id {
        CFRelease(value);
        return None;
    }
    // Already retained by CopyAttributeValue — hand the raw pointer to the caller.
    Some(value as AXUIElementRef)
}

/// The window an AX surface belongs to for reads, actions and focus checks.
/// An `AXSheet` (an Open panel, a save prompt) has its own WindowServer id,
/// but it lives inside its parent window's accessibility tree and is read
/// and addressed through that window, so it folds into the parent's id
/// (see [`super::exact_target::owning_window_id`]).
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub unsafe fn surface_window_id(element: AXUIElementRef) -> Option<u32> {
    super::exact_target::owning_window_id(element)
}

/// Whether `window_id` is `target`, or a WindowServer child window of it.
///
/// Finder's inline rename field is a separate small child window whose AX
/// element names only the application as its parent, so AX alone cannot tie
/// it to its Finder window; WindowServer records the parent. Callers keep
/// exact ids (a child window can be targeted by its own id) and ask this only
/// when deciding whether something counts as part of the requested window.
pub fn window_belongs_to(window_id: u32, target: u32) -> bool {
    belongs_via(window_id, target, crate::input::skylight::window_parent_id)
}

/// [`window_belongs_to`] with an injectable parent lookup. Bounded, since
/// child windows can nest.
fn belongs_via(window_id: u32, target: u32, parent_of: impl Fn(u32) -> Option<u32>) -> bool {
    let mut current = window_id;
    for _ in 0..5 {
        if current == target {
            return true;
        }
        match parent_of(current) {
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// Best-effort completion of the one exact-window request. This addresses only
/// the requested AX window; it never orders every window owned by the process.
pub fn raise_exact_window(pid: i32, window_id: u32) -> bool {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return false;
        }
        let mut target = None;
        for window in copy_ax_windows(app) {
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);
        let Some(target) = target else {
            return false;
        };
        let raised = perform_action(target, "AXRaise") == 0;
        let main = set_bool_attr_true(target, "AXMain") == 0;
        let focused = set_bool_attr_true(target, "AXFocused") == 0;
        CFRelease(target as CFTypeRef);
        raised || main || focused
    }
}

/// Whether `pid`'s keyboard focus is a text control in a WindowServer child
/// window of `target`: an inline editor (Finder's rename field) or a
/// popover's field (a tag editor). Such an edit lives only while its app is
/// active: Finder ends an inline rename (saving the field) when it loses the front.
pub fn inline_edit_open(pid: i32, target: u32) -> bool {
    focus_state(pid, target) == FocusState::InlineEdit
}

/// Whether `pid` has an inline edit open in any of its windows: its focus is
/// a text control in a WindowServer child window (Finder's rename field, a
/// sheet's field). `Some(false)` only when the focused element and its role
/// answered (a text control also needs its window and that window's parent
/// query), `None` when they did not (a busy app).
pub fn inline_edit_state(pid: i32) -> Option<bool> {
    const READ_TIMEOUT_SECONDS: f32 = 0.2;
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(app, READ_TIMEOUT_SECONDS);
        let element = copy_element_attr(app, "AXFocusedUIElement");
        CFRelease(app as CFTypeRef);
        let element = element?;
        AXUIElementSetMessagingTimeout(element, READ_TIMEOUT_SECONDS);
        let role = copy_string_attr(element, "AXRole");
        let window = ax_get_window_id(element);
        CFRelease(element as CFTypeRef);
        let text = matches!(
            role.as_deref(),
            Some("AXTextField" | "AXTextArea" | "AXComboBox")
        );
        match (role, window) {
            (None, _) => None,
            (Some(_), _) if !text => Some(false),
            (Some(_), None) => None,
            (Some(_), Some(window)) => crate::input::skylight::window_parent(window)
                .map(|parent| parent.is_some_and(|parent| parent != window)),
        }
    }
}

/// Where `pid`'s keyboard focus is, as a menu command's wait reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusState {
    /// A text control in a child window of the target: an inline edit.
    InlineEdit,
    /// The application itself, or nothing: a menu is tracking or closing.
    InMenu,
    /// Any other element.
    Elsewhere,
}

fn focus_state(pid: i32, target: u32) -> FocusState {
    // Each read is bounded, so a busy app cannot stretch a wait that holds
    // the previous front app away from the user.
    const READ_TIMEOUT_SECONDS: f32 = 0.2;
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return FocusState::Elsewhere;
        }
        AXUIElementSetMessagingTimeout(app, READ_TIMEOUT_SECONDS);
        let element = copy_element_attr(app, "AXFocusedUIElement");
        CFRelease(app as CFTypeRef);
        let Some(element) = element else {
            return FocusState::InMenu;
        };
        AXUIElementSetMessagingTimeout(element, READ_TIMEOUT_SECONDS);
        let role = copy_string_attr(element, "AXRole");
        if matches!(role.as_deref(), Some("AXApplication" | "AXMenu" | "AXMenuItem" | "AXMenuBarItem")) {
            CFRelease(element as CFTypeRef);
            return FocusState::InMenu;
        }
        // The focused element's own window id: one bounded read (an inline
        // editor is its own child window; no ancestor walk needed).
        let window = ax_get_window_id(element);
        CFRelease(element as CFTypeRef);
        if is_inline_edit(role.as_deref(), window, target, window_belongs_to) {
            FocusState::InlineEdit
        } else {
            FocusState::Elsewhere
        }
    }
}

/// After a menu command: whether it opened an inline edit of `target`
/// (File > Rename, New Folder). The editor shows only after the menu has
/// closed (Finder: about 0.4 s after the press, the app reporting itself as
/// focused until then), so wait while the focus is in the menu, and stop
/// once it has settled on anything else for a moment, or at `max`.
pub fn await_inline_edit_after_menu(pid: i32, target: u32, max: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + max;
    let mut elsewhere_since: Option<std::time::Instant> = None;
    loop {
        match focus_state(pid, target) {
            FocusState::InlineEdit => return true,
            FocusState::InMenu => elsewhere_since = None,
            FocusState::Elsewhere => {
                let since = *elsewhere_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= std::time::Duration::from_millis(150) {
                    return false;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// [`inline_edit_open`]'s decision, with an injectable parent lookup.
fn is_inline_edit(
    role: Option<&str>,
    window: Option<u32>,
    target: u32,
    belongs: impl Fn(u32, u32) -> bool,
) -> bool {
    matches!(role, Some("AXTextField" | "AXTextArea" | "AXComboBox"))
        && window.is_some_and(|window| window != target && belongs(window, target))
}

/// [`inline_edit_open`], polled for up to `budget`: an editor opened by a
/// key or menu command appears a moment after the command.
pub fn await_inline_edit(pid: i32, target: u32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if inline_edit_open(pid, target) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(test)]
mod inline_edit_tests {
    #[test]
    fn only_a_focused_text_control_in_a_child_window_is_an_inline_edit() {
        let child_of_7 = |window: u32, target: u32| window == 70 && target == 7;
        assert!(super::is_inline_edit(Some("AXTextField"), Some(70), 7, child_of_7));
        assert!(super::is_inline_edit(Some("AXTextArea"), Some(70), 7, child_of_7));
        // A field in the window itself, a button in the child, another
        // window's field, or an unknown window: not an inline edit.
        assert!(!super::is_inline_edit(Some("AXTextField"), Some(7), 7, |_, _| true));
        assert!(!super::is_inline_edit(Some("AXButton"), Some(70), 7, child_of_7));
        assert!(!super::is_inline_edit(Some("AXTextField"), Some(80), 7, child_of_7));
        assert!(!super::is_inline_edit(Some("AXTextField"), None, 7, child_of_7));
    }
}

/// A focused window reading, reported as `target` when it is part of it.
pub fn focused_as_target(focused: Option<u32>, target: u32) -> Option<u32> {
    focused.map(|window| if window_belongs_to(window, target) { target } else { window })
}

/// The own WindowServer id of a sheet (an Open panel, a save prompt)
/// attached to the app's window `window_id`, if one is attached.
pub fn attached_sheet_of_window(pid: i32, window_id: u32) -> Option<u32> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let windows = copy_ax_windows(app);
        CFRelease(app as CFTypeRef);
        let mut found = None;
        for window in windows {
            if found.is_none() && ax_get_window_id(window) == Some(window_id) {
                for child in copy_children(window) {
                    if found.is_none()
                        && copy_string_attr(child, "AXRole").as_deref() == Some("AXSheet")
                    {
                        found = ax_get_window_id(child).filter(|id| *id != window_id);
                    }
                    CFRelease(child as CFTypeRef);
                }
            }
            CFRelease(window as CFTypeRef);
        }
        found
    }
}

/// Return the CGWindowID of the application's focused AX window.
///
/// This is a narrow read-only proof used before global keyboard delivery: an
/// already focused exact window must not be re-activated, because doing so can
/// make a focus-proxy renderer drop its current key target.
pub fn focused_window_id_of_pid(pid: i32) -> Option<u32> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let window = copy_element_attr(app, "AXFocusedWindow");
        let Some(window) = window else {
            // Finder reports no focused window while its inline rename field
            // (a child window) has focus; the focused element still resolves.
            let element = copy_element_attr(app, "AXFocusedUIElement");
            CFRelease(app as CFTypeRef);
            let element = element?;
            let window_id = crate::ax::exact_target::element_window_id(element);
            CFRelease(element as CFTypeRef);
            return window_id;
        };
        CFRelease(app as CFTypeRef);
        // A focused sheet means its parent window is the focused one.
        let window_id = surface_window_id(window);
        CFRelease(window as CFTypeRef);
        window_id
    }
}

/// Get the children of an AX element.
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_children(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    copy_children_reporting(element).0
}

/// [`copy_children`], plus whether the read itself failed, as opposed to the
/// element having no children (no value, or no AXChildren attribute).
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_children_reporting(element: AXUIElementRef) -> (Vec<AXUIElementRef>, bool) {
    let (children, error) = copy_children_error(element);
    (children, error.is_some())
}

/// [`copy_children`], plus the error when the read itself failed
/// (`kAXErrorCannotComplete` when the app did not answer in time).
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_children_error(element: AXUIElementRef) -> (Vec<AXUIElementRef>, Option<AXError>) {
    let attr = CFStr::new("AXChildren");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return (vec![], None);
    }
    if err != kAXErrorSuccess || value.is_null() {
        return (vec![], Some(if err == kAXErrorSuccess { kAXErrorFailure } else { err }));
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return (vec![], Some(kAXErrorFailure));
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    let children = (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                // Retain so we own it — caller is responsible for releasing.
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect();
    (children, None)
}

/// Copy an AX element-valued attribute. The returned element is retained and
/// must be released by the caller.
///
/// # Safety
///
/// `element` must be valid, and the caller must release any returned element.
pub unsafe fn copy_element_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Option<AXUIElementRef> {
    #[cfg(test)]
    if let Some(result) = test_support::copy_element_attr(element, attr_name) {
        return result;
    }
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    if core_foundation::base::CFGetTypeID(value) != AXUIElementGetTypeID() {
        CFRelease(value);
        return None;
    }
    Some(value as AXUIElementRef)
}

/// Perform an AX action using a string attribute name.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
/// Whether an element currently answers accessibility requests (it has not
/// been destroyed). Read right before an action, so a stale handle is never
/// mistaken for one the action replaced.
///
/// # Safety
///
/// `element` must be a valid (retained) `AXUIElementRef`.
pub unsafe fn element_is_alive(element: AXUIElementRef) -> bool {
    let attr = CFStr::new("AXRole");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if !value.is_null() {
        CFRelease(value);
    }
    err != kAXErrorInvalidUIElement
}

/// Whether an action error means the action ran and replaced its element: the
/// element was alive when dispatched, the action did not fail for being
/// invalid (a stale handle fails that way before anything happens), and the
/// element is gone afterwards.
pub fn action_replaced_element(action_error: AXError, alive_before: bool, gone_after: impl FnOnce() -> bool) -> bool {
    action_error != kAXErrorSuccess
        && action_error != kAXErrorInvalidUIElement
        && alive_before
        && gone_after()
}

/// Whether an element stopped existing right after an action on it.
///
/// An action can replace the very element it was performed on: Finder's
/// AXOpen on a folder icon navigates the window, destroying the icon, and the
/// action call then returns an error although it ran. Afterwards the element
/// answers every attribute with kAXErrorInvalidUIElement. Finder takes a few
/// hundred milliseconds to tear the icon down, so this polls for up to 800 ms
/// of wall-clock time, with each native request bounded by what is left of
/// that budget (an unresponsive app would otherwise hold each read for the
/// element's normal messaging timeout). It only runs after an action already
/// failed, so it never slows a success.
///
/// # Safety
///
/// `element` must be a valid (retained) `AXUIElementRef`.
pub unsafe fn element_gone_after_action(element: AXUIElementRef) -> bool {
    const BUDGET: std::time::Duration = std::time::Duration::from_millis(800);
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);
    let deadline = std::time::Instant::now() + BUDGET;
    let mut gone = false;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let _ = AXUIElementSetMessagingTimeout(element, remaining.as_secs_f32().max(0.05));
        let attr = CFStr::new("AXRole");
        let mut value: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
        if !value.is_null() {
            CFRelease(value);
        }
        if err == kAXErrorInvalidUIElement {
            gone = true;
            break;
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(std::time::Instant::now())));
    }
    // Restore the element's normal per-request bound for any later use.
    let _ = AXUIElementSetMessagingTimeout(element, crate::ax::tree::AX_MESSAGING_TIMEOUT_SECONDS);
    gone
}

pub unsafe fn perform_action(element: AXUIElementRef, action_name: &str) -> AXError {
    #[cfg(test)]
    if let Some(result) = test_support::perform_action(element, action_name) {
        return result;
    }
    let action = CFStr::new(action_name);
    AXUIElementPerformAction(element, action.as_concrete_TypeRef())
}

/// Set an AX attribute to a CFString value.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_string_attr(element: AXUIElementRef, attr_name: &str, value: &str) -> AXError {
    #[cfg(test)]
    if let Some(result) = test_support::editor_write(element, attr_name, value) {
        return result;
    }
    let attr = CFStr::new(attr_name);
    let cf_value = CFStr::new(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX attribute to a CFNumber (double) value. Numeric controls — most
/// notably `AXSlider` (NSSlider) and `AXStepper` — expose a numeric `AXValue`
/// reject a `CFString` write — `-25200` (kAXErrorFailure, observed live on a
/// SwiftUI `AXSlider`) or `-25201` (kAXErrorIllegalArgument); only a `CFNumber`
/// is accepted. Text fields, by contrast, take a `CFString`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_number_attr(element: AXUIElementRef, attr_name: &str, value: f64) -> AXError {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let cf_value = CFNumber::from(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX CGPoint attribute such as `AXPosition`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_point_attr(element: AXUIElementRef, attr_name: &str, x: f64, y: f64) -> AXError {
    let attr = CFStr::new(attr_name);
    let point = CGPointValue { x, y };
    let value = AXValueCreate(
        kAXValueCGPointType,
        &point as *const CGPointValue as *const c_void,
    );
    if value.is_null() {
        return kAXErrorFailure;
    }
    let result =
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    result
}

/// Set an AX CGSize attribute such as `AXSize`.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_size_attr(
    element: AXUIElementRef,
    attr_name: &str,
    width: f64,
    height: f64,
) -> AXError {
    let attr = CFStr::new(attr_name);
    let size = CGSizeValue { width, height };
    let value = AXValueCreate(
        kAXValueCGSizeType,
        &size as *const CGSizeValue as *const c_void,
    );
    if value.is_null() {
        return kAXErrorFailure;
    }
    let result =
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    result
}

/// Set an AX attribute to a CFBoolean true value.
///
/// # Safety
///
/// `element` must be a valid, live `AXUIElementRef` for the duration of the call.
pub unsafe fn set_bool_attr_true(element: AXUIElementRef, attr_name: &str) -> AXError {
    #[cfg(test)]
    if let Some(result) = test_support::set_bool_attr_true(element, attr_name) {
        return result;
    }
    use core_foundation::boolean::CFBoolean;
    let attr = CFStr::new(attr_name);
    let cf_true = CFBoolean::true_value();
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_true.as_CFTypeRef())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityOptIn {
    ManualAccessibility,
    EnhancedUserInterface,
    /// The legacy setter may have scheduled work before its superclass errored.
    EnhancedUserInterfaceUnconfirmed,
    NotAccepted,
}

/// Signal to a Chromium/Electron application root that a real assistive client
/// is present so it materializes its full web-content accessibility tree.
///
/// `AXManualAccessibility` is the modern opt-in with no screen-reader side
/// effects; `AXEnhancedUserInterface` is the legacy fallback some Electron
/// builds expose instead (the modern attribute returns
/// `kAXErrorAttributeUnsupported` on those builds). Every AppKit and Catalyst
/// app also accepts the legacy attribute, and it changes how they behave
/// (screen-reader mode), so it is sent only when `legacy_allowed` says the
/// process is Chromium or Electron.
///
/// # Safety
///
/// `app_element` must be a valid, live application `AXUIElementRef`.
pub unsafe fn enable_chromium_accessibility(
    app_element: AXUIElementRef,
    legacy_allowed: bool,
) -> AccessibilityOptIn {
    let manual = set_bool_attr_true(app_element, "AXManualAccessibility");
    if manual == kAXErrorSuccess {
        return AccessibilityOptIn::ManualAccessibility;
    }
    if manual != kAXErrorAttributeUnsupported || !legacy_allowed {
        // A transient error (e.g. timeout / app busy) rather than a hard
        // "this app has no such attribute" — don't bother with the legacy
        // fallback, and don't claim enablement happened.
        return AccessibilityOptIn::NotAccepted;
    }
    match set_bool_attr_true(app_element, "AXEnhancedUserInterface") {
        kAXErrorSuccess => AccessibilityOptIn::EnhancedUserInterface,
        kAXErrorNotImplemented => AccessibilityOptIn::EnhancedUserInterfaceUnconfirmed,
        _ => AccessibilityOptIn::NotAccepted,
    }
}

/// Get the CGWindowID of an AX window element via the private `_AXUIElementGetWindow` SPI.
/// Returns `None` if the element is not a composited window.
///
/// # Safety
///
/// `element` must be a valid, live window `AXUIElementRef`.
pub unsafe fn ax_get_window_id(element: AXUIElementRef) -> Option<u32> {
    ax_get_window_id_checked(element).ok().flatten()
}

pub(crate) unsafe fn ax_get_window_id_checked(
    element: AXUIElementRef,
) -> Result<Option<u32>, AXError> {
    let mut wid: u32 = 0;
    let err = _AXUIElementGetWindow(element, &mut wid);
    checked_window_id(err, wid)
}

fn checked_window_id(error: AXError, window_id: u32) -> Result<Option<u32>, AXError> {
    if error != kAXErrorSuccess {
        return Err(error);
    }
    Ok((window_id != 0).then_some(window_id))
}


/// Read the `AXWindows` attribute of an application element.
/// Unlike `AXChildren`, this returns the window list regardless of whether
/// the app is frontmost. Returns a Vec of retained AXUIElementRefs.
///
/// # Safety
///
/// `element` must be valid, and the caller must release every returned element.
pub unsafe fn copy_ax_windows(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    let attr = CFStr::new("AXWindows");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return vec![];
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return vec![];
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect()
}

/// Copy an attribute without conflating an AX failure with an absent value.
/// The returned Core Foundation object owns the reference from the AX copy.
pub(crate) unsafe fn copy_attribute_checked(
    element: AXUIElementRef,
    attribute: &str,
) -> Result<core_foundation::base::CFType, AXError> {
    let attr = CFStr::new(attribute);
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    let value =
        (!value.is_null()).then(|| core_foundation::base::CFType::wrap_under_create_rule(value));
    checked_attribute_value(error, value)
}

fn checked_attribute_value(
    error: AXError,
    value: Option<core_foundation::base::CFType>,
) -> Result<core_foundation::base::CFType, AXError> {
    if error != kAXErrorSuccess {
        return Err(error);
    }
    value.ok_or(kAXErrorFailure)
}

pub(crate) unsafe fn copy_string_attr_checked(
    element: AXUIElementRef,
    attribute: &str,
) -> Result<String, AXError> {
    let value = copy_attribute_checked(element, attribute)?;
    cf_plain_string(value.as_CFTypeRef()).ok_or(kAXErrorFailure)
}

pub(crate) unsafe fn copy_geometry_attr_checked(
    element: AXUIElementRef,
    attribute: &str,
    value_type: AXValueType,
) -> Result<[f64; 2], AXError> {
    let value = copy_attribute_checked(element, attribute)?;
    if value.type_of() != AXValueGetTypeID()
        || !matches!(value_type, kAXValueCGPointType | kAXValueCGSizeType)
    {
        return Err(kAXErrorFailure);
    }
    let mut pair = [0.0_f64; 2];
    if !AXValueGetValue(
        value.as_CFTypeRef() as AXValueRef,
        value_type,
        pair.as_mut_ptr() as _,
    ) || !pair.iter().all(|value| value.is_finite())
    {
        return Err(kAXErrorFailure);
    }
    Ok(pair)
}

/// Strict bounded array read for observation. Unlike the legacy projection,
/// malformed members make the entire read unknown instead of silently hiding
/// a possible second surface. Every successful returned element is retained.
pub(crate) unsafe fn copy_element_array_attr_checked(
    element: AXUIElementRef,
    attribute: &str,
    limit: usize,
) -> Result<Vec<AXUIElementRef>, AXError> {
    checked_element_array(copy_attribute_checked(element, attribute)?, limit)
}

unsafe fn checked_element_array(
    value: core_foundation::base::CFType,
    limit: usize,
) -> Result<Vec<AXUIElementRef>, AXError> {
    if value.type_of() != CFArray::<CFTypeRef>::type_id() {
        return Err(kAXErrorFailure);
    }
    let array = CFArray::<CFTypeRef>::wrap_under_get_rule(value.as_CFTypeRef() as _);
    if array.len() as usize > limit {
        return Err(kAXErrorFailure);
    }
    let ax_type_id = AXUIElementGetTypeID();
    for i in 0..array.len() {
        let item = *array.get(i).ok_or(kAXErrorFailure)?;
        if item.is_null() || core_foundation::base::CFGetTypeID(item) != ax_type_id {
            return Err(kAXErrorFailure);
        }
    }
    Ok((0..array.len())
        .map(|i| {
            let item = *array.get(i).expect("validated array member");
            CFRetain(item);
            item as AXUIElementRef
        })
        .collect())
}

/// CGWindowIDs of `pid`'s `AXWindows`, or `None` when that list cannot be
/// read: the process is not trusted for Accessibility, the app does not
/// answer within a short timeout, or AX reports an error. `None` means
/// "unknown", never "no windows", so callers must not treat it as proof that a
/// window lacks an AX counterpart.
pub fn ax_window_ids_of_pid(pid: i32) -> Option<std::collections::HashSet<u32>> {
    unsafe {
        if !AXIsProcessTrusted() {
            return None;
        }
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(app, AX_WINDOW_IDS_TIMEOUT_SECONDS);
        let attr = CFStr::new("AXWindows");
        let mut value: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(app, attr.as_concrete_TypeRef(), &mut value);
        CFRelease(app as CFTypeRef);
        if err != kAXErrorSuccess || value.is_null() {
            return None;
        }
        if core_foundation::base::CFGetTypeID(value) != CFArray::<CFTypeRef>::type_id() {
            CFRelease(value);
            return None;
        }
        let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
        let ax_type_id = AXUIElementGetTypeID();
        Some(
            (0..arr.len())
                .filter_map(|i| {
                    let item = *arr.get(i)?;
                    (core_foundation::base::CFGetTypeID(item) == ax_type_id)
                        .then(|| ax_get_window_id(item as AXUIElementRef))
                        .flatten()
                })
                .collect(),
        )
    }
}

/// `pid`'s `AXWindows` as `(CGWindowID, AXDocument)`: the document is the
/// file URL a document window shows (TextEdit, Preview), `None` for windows
/// without one. `None` overall when the list cannot be read in full within
/// `budget` (not trusted, an AX error on the list, a window id or a document,
/// or a slow app), so an unknown answer is never taken for "no document".
pub fn ax_window_documents_of_pid(
    pid: i32,
    budget: std::time::Duration,
) -> Option<Vec<(u32, Option<String>)>> {
    const MAX_WINDOWS: usize = 64;
    unsafe {
        if !AXIsProcessTrusted() {
            return None;
        }
        let deadline = std::time::Instant::now() + budget;
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(app, AX_WINDOW_IDS_TIMEOUT_SECONDS);
        let windows = copy_element_array_attr_checked(app, "AXWindows", MAX_WINDOWS);
        CFRelease(app as CFTypeRef);
        let windows = windows.ok()?;
        let mut documents = Some(Vec::with_capacity(windows.len()));
        for &window in &windows {
            if std::time::Instant::now() >= deadline {
                documents = None;
            }
            let Some(list) = documents.as_mut() else {
                break;
            };
            AXUIElementSetMessagingTimeout(window, AX_WINDOW_IDS_TIMEOUT_SECONDS);
            // A failed read (a timeout, an app that cannot answer) makes the
            // whole list unknown; only "no such attribute or value" means the
            // window shows no document.
            let document = match copy_attribute_checked(window, "AXDocument") {
                Ok(value) => match cf_plain_string(value.as_CFTypeRef()) {
                    Some(text) => Some(text),
                    // A value that is not text is unknown, not "no document".
                    None => {
                        documents = None;
                        continue;
                    }
                },
                Err(kAXErrorNoValue | kAXErrorAttributeUnsupported) => None,
                Err(_) => {
                    documents = None;
                    continue;
                }
            };
            match ax_get_window_id_checked(window) {
                Ok(Some(id)) => list.push((id, document)),
                // A window without a readable id (Finder's desktop answers
                // -25201) cannot be one of the snapshot's windows. Only one that
                // names a document could hide the requested one.
                Ok(None) | Err(_) if document.is_none() => {}
                Ok(None) | Err(_) => documents = None,
            }
        }
        for window in windows {
            CFRelease(window as CFTypeRef);
        }
        // A read that finished past its budget is not trusted either.
        documents.filter(|_| std::time::Instant::now() < deadline)
    }
}

/// AX messaging timeout for [`ax_window_ids_of_pid`], in seconds. Window
/// enumeration calls it once per app, so a hung app must not stall it.
const AX_WINDOW_IDS_TIMEOUT_SECONDS: f32 = 0.25;

/// Highest AX element id probed when looking for an off-Space window.
/// Window elements are allocated early in an app's lifetime (Calculator's
/// main window is id 42); alt-tab-macos probes the same order of magnitude.
const MAX_REMOTE_TOKEN_ELEMENT_ID: u64 = 2_000;

/// Wall-clock ceiling for one remote-token probe, so an unresponsive app
/// cannot stall a snapshot or an action decision.
const REMOTE_TOKEN_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(300);

/// Per-candidate AX messaging timeout during the probe, in seconds.
const REMOTE_TOKEN_CANDIDATE_TIMEOUT_SECONDS: f32 = 0.05;

/// The 20-byte remote token AX uses to identify one element of `pid`.
fn remote_token_bytes(pid: i32, element_id: u64) -> [u8; 20] {
    const COCOA_TOKEN_MAGIC: i32 = 0x636f_636f; // 'coco'
    let mut token = [0u8; 20];
    token[0..4].copy_from_slice(&pid.to_ne_bytes());
    token[8..12].copy_from_slice(&COCOA_TOKEN_MAGIC.to_ne_bytes());
    token[12..20].copy_from_slice(&element_id.to_ne_bytes());
    token
}

/// Find the `AXWindow` element for `window_id` when `AXWindows` omits it —
/// macOS drops windows on other Spaces from that list. Probes the app's AX
/// element ids through `_AXUIElementCreateWithRemoteToken` and returns only an
/// element whose role is `AXWindow` AND whose `_AXUIElementGetWindow` equals
/// `window_id`, so the result is exactly as strong as an `AXWindows` match.
/// Returns a retained element the caller must release.
///
/// # Safety
///
/// The caller must release any returned element exactly once with `CFRelease`.
pub unsafe fn copy_ax_window_by_remote_token(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let started = std::time::Instant::now();
    for element_id in 0..MAX_REMOTE_TOKEN_ELEMENT_ID {
        if started.elapsed() > REMOTE_TOKEN_PROBE_DEADLINE {
            return None;
        }
        let token =
            core_foundation::data::CFData::from_buffer(&remote_token_bytes(pid, element_id));
        let element = _AXUIElementCreateWithRemoteToken(token.as_concrete_TypeRef());
        if element.is_null() {
            continue;
        }
        AXUIElementSetMessagingTimeout(element, REMOTE_TOKEN_CANDIDATE_TIMEOUT_SECONDS);
        if copy_string_attr(element, "AXRole").as_deref() == Some("AXWindow")
            && ax_get_window_id(element) == Some(window_id)
        {
            // The snapshot walk reads the whole subtree through this element;
            // restore the system default so slow apps are not cut off at the
            // probe's per-candidate timeout (issue #4082).
            AXUIElementSetMessagingTimeout(element, 0.0);
            return Some(element);
        }
        CFRelease(element as CFTypeRef);
    }
    None
}

/// Whether to run the remote-token probe for a window `AXWindows` omitted.
///
/// Windows WindowServer reports on another Space qualify. So does a window it
/// places on the current Space but reports off screen: that combination is a
/// stale or mid-transition Space view (issue #4437), in which AX drops the
/// window from `AXWindows` exactly as it does for an off-Space one. An
/// on-screen current-Space or unknown window that AX cannot map fails fast
/// instead of paying the probe's deadline on every call (issue #4083). The
/// WindowServer view is only queried for unlisted windows, so listed windows
/// skip that read.
fn should_probe_off_space_window(
    listed_in_ax_windows: bool,
    space_view: impl FnOnce() -> Option<crate::windows::WindowSpaceView>,
) -> bool {
    if listed_in_ax_windows {
        return false;
    }
    match space_view() {
        Some(view) => match view.on_current_space {
            Some(false) => true,
            Some(true) => !view.is_on_screen,
            None => false,
        },
        None => false,
    }
}

/// `AXWindows` of `pid`'s application element, plus the requested window when
/// `AXWindows` does not list it because it is on another Space. Returns
/// retained elements the caller must release.
///
/// # Safety
///
/// `app` must be the valid application element of `pid`, and the caller must
/// release every returned element.
pub unsafe fn copy_ax_windows_including(
    app: AXUIElementRef,
    pid: i32,
    window_id: u32,
) -> Vec<AXUIElementRef> {
    let mut windows = copy_ax_windows(app);
    let listed = windows
        .iter()
        .any(|&window| ax_get_window_id(window) == Some(window_id));
    if should_probe_off_space_window(listed, || {
        crate::windows::window_space_view_by_id(window_id)
    }) {
        windows.extend(copy_ax_window_by_remote_token(pid, window_id));
    }
    windows
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_an_action_that_destroyed_a_live_element_counts_as_replaced() {
        use super::{action_replaced_element, kAXErrorInvalidUIElement, kAXErrorSuccess};
        let attribute_unsupported = -25205;
        // Finder: alive at dispatch, error, gone afterwards.
        assert!(action_replaced_element(attribute_unsupported, true, || true));
        // Stale handle: dead before dispatch, or the action failed as invalid.
        assert!(!action_replaced_element(attribute_unsupported, false, || true));
        assert!(!action_replaced_element(kAXErrorInvalidUIElement, true, || true));
        // Still there afterwards: a real failure.
        assert!(!action_replaced_element(attribute_unsupported, true, || false));
        // Success needs no reinterpretation, and the after-probe is not run.
        assert!(!action_replaced_element(kAXErrorSuccess, true, || panic!("probed after success")));
    }

    #[test]
    fn child_windows_belong_to_their_ancestors_only() {
        // 21 is a child of 10; 30 is a child of 21; 11 is unrelated.
        let parent_of = |id| match id {
            21 => Some(10),
            30 => Some(21),
            _ => None,
        };
        assert!(super::belongs_via(10, 10, parent_of));
        assert!(super::belongs_via(21, 10, parent_of));
        assert!(super::belongs_via(30, 10, parent_of));
        assert!(!super::belongs_via(10, 21, parent_of), "a parent is not part of its child");
        assert!(!super::belongs_via(11, 10, parent_of));
    }

    use super::*;
    use crate::windows::WindowSpaceView;
    use core_foundation::{boolean::CFBoolean, number::CFNumber};

    #[test]
    fn remote_token_layout_is_pid_zero_coco_element_id() {
        let token = remote_token_bytes(0x0102_0304, 42);
        assert_eq!(&token[0..4], &0x0102_0304i32.to_ne_bytes());
        assert_eq!(&token[4..8], &[0, 0, 0, 0]);
        assert_eq!(&token[8..12], &0x636f_636fi32.to_ne_bytes());
        assert_eq!(&token[12..20], &42u64.to_ne_bytes());
    }

    fn space_view(on_current_space: Option<bool>, is_on_screen: bool) -> WindowSpaceView {
        WindowSpaceView {
            on_current_space,
            is_on_screen,
        }
    }

    #[test]
    fn off_space_probe_runs_only_for_unlisted_off_space_windows() {
        assert!(should_probe_off_space_window(false, || Some(space_view(
            Some(false),
            false
        ))));
        assert!(!should_probe_off_space_window(false, || Some(space_view(
            Some(true),
            true
        ))));
        assert!(!should_probe_off_space_window(false, || None));
        assert!(!should_probe_off_space_window(false, || Some(space_view(
            None, false
        ))));
        assert!(!should_probe_off_space_window(true, || {
            panic!("listed windows must not query Space membership")
        }));
    }

    /// Issue #4437: WindowServer placed a visible window on the reported
    /// current Space yet marked it off screen, and AX omitted it. That stale
    /// Space view must re-resolve through the exact-id probe rather than
    /// refuse the window as `ax_unresolved`.
    #[test]
    fn off_space_probe_runs_for_a_current_space_window_reported_off_screen() {
        assert!(should_probe_off_space_window(false, || Some(space_view(
            Some(true),
            false
        ))));
    }

    #[test]
    fn binary_value_accepts_booleans_and_exact_zero_or_one() {
        let true_value = CFBoolean::true_value();
        let false_value = CFBoolean::false_value();
        let zero = CFNumber::from(0.0);
        let one = CFNumber::from(1.0);
        let fractional = CFNumber::from(0.5);
        let other = CFNumber::from(2.0);
        let string = CFStr::new("1");

        assert_eq!(
            unsafe { coerce_binary_value(true_value.as_CFTypeRef()) },
            Some(true)
        );
        assert_eq!(
            unsafe { coerce_binary_value(false_value.as_CFTypeRef()) },
            Some(false)
        );
        assert_eq!(
            unsafe { coerce_binary_value(zero.as_CFTypeRef()) },
            Some(false)
        );
        assert_eq!(
            unsafe { coerce_binary_value(one.as_CFTypeRef()) },
            Some(true)
        );
        assert_eq!(
            unsafe { coerce_binary_value(fractional.as_CFTypeRef()) },
            None
        );
        assert_eq!(unsafe { coerce_binary_value(other.as_CFTypeRef()) }, None);
        assert_eq!(unsafe { coerce_binary_value(string.as_CFTypeRef()) }, None);
    }

    #[test]
    fn binary_value_rejects_near_binary_and_non_finite_numbers() {
        for value in [
            1e-20,
            -1e-20,
            f64::from_bits(1),
            -f64::from_bits(1),
            f64::from_bits(1.0_f64.to_bits() - 1),
            f64::from_bits(1.0_f64.to_bits() + 1),
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let number = CFNumber::from(value);
            assert_eq!(
                unsafe { coerce_binary_value(number.as_CFTypeRef()) },
                None,
                "unexpected binary state for {value:?}"
            );
        }
    }

    #[test]
    fn link_url_attribute_decodes_cfurl_without_changing_values() {
        use core_foundation::url::{CFURLCreateWithString, CFURL};
        let text = CFStr::new("https://example.test/book?q=a%20b#slot");
        let raw = unsafe {
            CFURLCreateWithString(
                std::ptr::null(),
                text.as_concrete_TypeRef(),
                std::ptr::null(),
            )
        };
        assert!(!raw.is_null());
        let url = unsafe { CFURL::wrap_under_create_rule(raw) };
        assert_eq!(
            unsafe { coerce_url_value(url.as_CFTypeRef()) }.as_deref(),
            Some("https://example.test/book?q=a%20b#slot")
        );
        assert!(unsafe { coerce_stringish_value(url.as_CFTypeRef()) }.is_none());
        assert!(unsafe { coerce_url_value(CFNumber::from(8).as_CFTypeRef()) }.is_none());
    }

    #[test]
    fn stringish_value_coerces_cfstring_cfnumber_and_cfboolean() {
        let string = CFStr::new("Search");
        let integer = CFNumber::from(8.0);
        let decimal = CFNumber::from(2.5);
        let true_value = CFBoolean::true_value();
        let false_value = CFBoolean::false_value();

        let string_result = unsafe { coerce_stringish_value(string.as_CFTypeRef()) }.unwrap();
        assert_eq!(string_result.string_value.as_deref(), Some("Search"));
        assert_eq!(string_result.state_value, "Search");

        let integer_result = unsafe { coerce_stringish_value(integer.as_CFTypeRef()) }.unwrap();
        assert_eq!(integer_result.string_value, None);
        assert_eq!(integer_result.state_value, "8");

        let decimal_result = unsafe { coerce_stringish_value(decimal.as_CFTypeRef()) }.unwrap();
        assert_eq!(decimal_result.string_value, None);
        assert_eq!(decimal_result.state_value, "2.5");

        let true_result = unsafe { coerce_stringish_value(true_value.as_CFTypeRef()) }.unwrap();
        assert_eq!(true_result.string_value, None);
        assert_eq!(true_result.state_value, "1");

        let false_result = unsafe { coerce_stringish_value(false_value.as_CFTypeRef()) }.unwrap();
        assert_eq!(false_result.string_value, None);
        assert_eq!(false_result.state_value, "0");
    }

    /// Formatted text (Catalyst compose fields, TextKit views) reports its
    /// AXValue as a CFAttributedString; it must read as its text, not as no
    /// value.
    #[test]
    fn attributed_string_value_reads_as_its_plain_text() {
        use core_foundation::attributed_string::CFAttributedString;
        let attributed = CFAttributedString::new(&CFStr::new("Hello from Catalyst"));
        let copied = unsafe { coerce_stringish_value(attributed.as_CFTypeRef()) }.unwrap();
        assert_eq!(copied.string_value.as_deref(), Some("Hello from Catalyst"));
        assert_eq!(copied.state_value, "Hello from Catalyst");
        assert_eq!(
            unsafe { cf_plain_string(attributed.as_CFTypeRef()) }.as_deref(),
            Some("Hello from Catalyst")
        );
        assert!(unsafe { cf_plain_string(CFNumber::from(3).as_CFTypeRef()) }.is_none());
    }
}
