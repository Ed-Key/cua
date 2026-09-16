//! Scoped AX responses for click receipt tests. Only the OS boundary is replaced.

use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

thread_local! {
    static SELECTION: RefCell<Option<SelectionFixture>> = const { RefCell::new(None) };
}

pub(crate) struct SelectionFixture {
    pub advertised_press: bool,
    pub reads: VecDeque<Option<bool>>,
    pub write_result: AXError,
    pub calls: Vec<&'static str>,
    pub before_readback: Option<Box<dyn FnOnce()>>,
    parent: Option<usize>,
    before_selected: VecDeque<Option<Box<dyn FnOnce()>>>,
}

pub(crate) struct SelectionScope {
    parent: Option<core_foundation::string::CFString>,
}

impl SelectionScope {
    pub fn install(advertised_press: bool, readback: Option<bool>, accepted: bool) -> Self {
        SELECTION.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(SelectionFixture {
                advertised_press,
                // Subsequent reads are unavailable, preventing any later success
                // or native selection capture from masking the first write.
                reads: [Some(false), readback].into(),
                write_result: if accepted {
                    kAXErrorSuccess
                } else {
                    kAXErrorFailure
                },
                calls: vec![],
                before_readback: None,
                parent: None,
                before_selected: VecDeque::new(),
            });
        });
        Self { parent: None }
    }

    pub fn ancestor_after_failed_write(&mut self) {
        use core_foundation::base::TCFType;
        let parent = core_foundation::string::CFString::new("Scoped selection parent");
        SELECTION.with(|slot| {
            let mut slot = slot.borrow_mut();
            let fixture = slot.as_mut().unwrap();
            fixture.parent = Some(parent.as_concrete_TypeRef() as usize);
            fixture.write_result = kAXErrorFailure;
            fixture.reads = [Some(false), Some(false), Some(true)].into();
        });
        self.parent = Some(parent);
    }
    pub fn before_selected(&self, read: usize, callback: impl FnOnce() + 'static) {
        SELECTION.with(|slot| {
            let mut slot = slot.borrow_mut();
            let fixture = slot.as_mut().unwrap();
            fixture.before_selected.resize_with(read + 1, || None);
            fixture.before_selected[read] = Some(Box::new(callback));
        });
    }

    pub fn before_readback(&self, callback: impl FnOnce() + 'static) {
        SELECTION.with(|slot| {
            slot.borrow_mut().as_mut().unwrap().before_readback = Some(Box::new(callback))
        });
    }

    pub fn element_ptr(&self) -> usize {
        usize::MAX
    }

    pub fn calls(&self) -> Vec<&'static str> {
        SELECTION.with(|slot| slot.borrow().as_ref().unwrap().calls.clone())
    }
}

impl Drop for SelectionScope {
    fn drop(&mut self) {
        SELECTION.with(|slot| *slot.borrow_mut() = None);
    }
}

fn with_fixture<T>(
    element: AXUIElementRef,
    f: impl FnOnce(&mut SelectionFixture) -> T,
) -> Option<T> {
    SELECTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let fixture = slot.as_mut()?;
        if element as usize != usize::MAX && fixture.parent != Some(element as usize) {
            return None;
        }
        Some(f(fixture))
    })
}

pub(super) fn copy_string_attr(element: AXUIElementRef, attr: &str) -> Option<Option<String>> {
    if let Some(value) = typing_focus_value(element, attr) {
        return Some(value);
    }
    if let Some(value) = with_editor(element, |fixture| match attr {
        "AXRole" => Some(fixture.role.clone()),
        "AXTitle" => Some("Editor".into()),
        "AXValue" if fixture.hide_value_after_write && !fixture.value.is_empty() => None,
        "AXValue" => Some(fixture.value.clone()),
        _ => panic!("unexpected editor attribute: {attr}"),
    }) {
        return Some(value);
    }
    with_fixture(element, |_| match attr {
        "AXRole" => Some("AXRow".into()),
        "AXTitle" => Some("Selection receipt fixture".into()),
        _ => panic!("unexpected string attribute: {attr}"),
    })
}

pub(super) fn copy_bool_attr(element: AXUIElementRef, attr: &str) -> Option<Option<bool>> {
    with_fixture(element, |fixture| match attr {
        "AXEnabled" => Some(true),
        "AXSelected" => {
            if let Some(Some(callback)) = fixture.before_selected.pop_front() {
                callback();
            }
            if fixture.calls.last() == Some(&"write selected") {
                if let Some(callback) = fixture.before_readback.take() {
                    callback();
                }
            }
            fixture.calls.push("read selected");
            fixture.reads.pop_front().unwrap_or(None)
        }
        _ => panic!("unexpected boolean attribute: {attr}"),
    })
}

pub(super) fn copy_action_names(element: AXUIElementRef) -> Option<Vec<String>> {
    with_fixture(element, |fixture| {
        if fixture.advertised_press {
            vec!["AXPress".into()]
        } else {
            vec![]
        }
    })
}

pub(super) fn copy_element_attr(
    element: AXUIElementRef,
    attr: &str,
) -> Option<Option<AXUIElementRef>> {
    with_fixture(element, |fixture| {
        assert_eq!(attr, "AXParent");
        if element as usize == usize::MAX {
            fixture.parent.map(|ptr| unsafe {
                core_foundation::base::CFRetain(ptr as CFTypeRef) as AXUIElementRef
            })
        } else {
            None
        }
    })
}

pub(super) fn perform_action(element: AXUIElementRef, action: &str) -> Option<AXError> {
    with_fixture(element, |fixture| {
        assert_eq!(action, "AXPress");
        fixture.calls.push("failed press");
        kAXErrorFailure
    })
}

pub(super) fn set_bool_attr_true(element: AXUIElementRef, attr: &str) -> Option<AXError> {
    with_fixture(element, |fixture| {
        assert_eq!(attr, "AXSelected");
        fixture.calls.push("write selected");
        if element as usize == usize::MAX {
            fixture.write_result
        } else {
            kAXErrorSuccess
        }
    })
}

thread_local! {
    static EDITOR: RefCell<Option<EditorFixture>> = const { RefCell::new(None) };
}
struct EditorFixture {
    element: usize,
    role: String,
    bounds: Option<[f64; 4]>,
    value: String,
    hide_value_after_write: bool,
    before_write: Option<Box<dyn FnOnce()>>,
}
pub(crate) struct EditorScope {
    _element: core_foundation::string::CFString,
}
impl EditorScope {
    pub fn install(
        role: &str,
        bounds: Option<[f64; 4]>,
        before_write: impl FnOnce() + 'static,
    ) -> Self {
        use core_foundation::base::TCFType;
        let element = core_foundation::string::CFString::new("Scoped fake editor");
        EDITOR.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(EditorFixture {
                element: element.as_concrete_TypeRef() as usize,
                role: role.into(),
                bounds,
                value: String::new(),
                hide_value_after_write: false,
                before_write: Some(Box::new(before_write)),
            });
        });
        Self { _element: element }
    }

    pub fn hide_value_after_write(&self) {
        EDITOR.with(|slot| slot.borrow_mut().as_mut().unwrap().hide_value_after_write = true);
    }
}
impl Drop for EditorScope {
    fn drop(&mut self) {
        EDITOR.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(crate) fn focused_editor(pid: i32, wid: u32) -> Option<Option<AXUIElementRef>> {
    if let Some(value) = typing_focus_element(pid, Some(wid)) {
        return Some(value);
    }
    if pid != -9876 {
        return None;
    }
    assert_eq!(
        wid, 42,
        "the exact requested window must reach the OS boundary"
    );
    EDITOR.with(|slot| {
        slot.borrow().as_ref().map(|fixture| {
            Some(
                unsafe { core_foundation::base::CFRetain(fixture.element as CFTypeRef) }
                    as AXUIElementRef,
            )
        })
    })
}

thread_local! {
    static TYPING_FOCUS: RefCell<Option<TypingFocusFixture>> = const { RefCell::new(None) };
}
struct TypingFocusFixture {
    elements: [usize; 2],
    values: [Option<String>; 2],
    ranges: [Option<cua_driver_core::text_insertion::TextSelectionRange>; 2],
    focused: Option<usize>,
    window: u32,
    switch_on_read: bool,
}
pub(crate) struct TypingFocusScope {
    _elements: [core_foundation::string::CFString; 2],
}
impl TypingFocusScope {
    pub fn install() -> Self {
        use core_foundation::{base::TCFType, string::CFString};
        let elements = [
            CFString::new("Typing original editor"),
            CFString::new("Typing other editor"),
        ];
        TYPING_FOCUS.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(TypingFocusFixture {
                elements: elements
                    .each_ref()
                    .map(|value| value.as_concrete_TypeRef() as usize),
                values: [Some(String::new()), Some("hello".into())],
                ranges: [None, None],
                focused: Some(0),
                window: 42,
                switch_on_read: false,
            });
        });
        Self {
            _elements: elements,
        }
    }
    pub fn focus(&self, index: Option<usize>, window: u32) {
        TYPING_FOCUS.with(|slot| {
            let mut slot = slot.borrow_mut();
            let fixture = slot.as_mut().unwrap();
            fixture.focused = index;
            fixture.window = window;
        });
    }
    pub fn value(&self, index: usize, value: Option<&str>) {
        TYPING_FOCUS.with(|slot| {
            slot.borrow_mut().as_mut().unwrap().values[index] = value.map(str::to_owned)
        });
    }
    pub fn switch_on_next_read(&self) {
        TYPING_FOCUS.with(|slot| slot.borrow_mut().as_mut().unwrap().switch_on_read = true);
    }
    pub fn range(&self, index: usize, location: u64, length: u64) {
        TYPING_FOCUS.with(|slot| {
            slot.borrow_mut().as_mut().unwrap().ranges[index] =
                Some(cua_driver_core::text_insertion::TextSelectionRange { location, length });
        });
    }
    pub fn element_ptr(&self, index: usize) -> usize {
        TYPING_FOCUS.with(|slot| slot.borrow().as_ref().unwrap().elements[index])
    }
}
impl Drop for TypingFocusScope {
    fn drop(&mut self) {
        TYPING_FOCUS.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(crate) fn typing_focus_element(
    pid: i32,
    window: Option<u32>,
) -> Option<Option<AXUIElementRef>> {
    if pid != -9880 {
        return None;
    }
    TYPING_FOCUS.with(|slot| {
        let slot = slot.borrow();
        let fixture = slot.as_ref().expect("typing scope required");
        Some(
            fixture
                .focused
                .filter(|_| window.is_none_or(|wid| wid == fixture.window))
                .map(|index| unsafe {
                    core_foundation::base::CFRetain(fixture.elements[index] as CFTypeRef)
                        as AXUIElementRef
                }),
        )
    })
}
fn typing_focus_value(element: AXUIElementRef, attr: &str) -> Option<Option<String>> {
    TYPING_FOCUS.with(|slot| {
        let mut slot = slot.borrow_mut();
        let fixture = slot.as_mut()?;
        let index = fixture
            .elements
            .iter()
            .position(|&ptr| ptr == element as usize)?;
        assert_eq!(attr, "AXValue");
        let value = fixture.values[index].clone();
        if fixture.switch_on_read {
            fixture.focused = Some(1);
            fixture.switch_on_read = false;
        }
        Some(value)
    })
}
pub(crate) fn typing_focus_range(
    element: AXUIElementRef,
) -> Option<Option<cua_driver_core::text_insertion::TextSelectionRange>> {
    TYPING_FOCUS.with(|slot| {
        let slot = slot.borrow();
        let fixture = slot.as_ref()?;
        let index = fixture
            .elements
            .iter()
            .position(|&ptr| ptr == element as usize)?;
        Some(fixture.ranges[index])
    })
}
fn with_editor<T>(element: AXUIElementRef, f: impl FnOnce(&mut EditorFixture) -> T) -> Option<T> {
    EDITOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        let fixture = slot.as_mut()?;
        (fixture.element == element as usize).then(|| f(fixture))
    })
}
pub(super) fn editor_rect(element: AXUIElementRef) -> Option<Option<[f64; 4]>> {
    with_editor(element, |fixture| fixture.bounds)
}
pub(super) fn editor_write(element: AXUIElementRef, attr: &str, value: &str) -> Option<AXError> {
    with_editor(element, |fixture| {
        assert_eq!(attr, "AXSelectedText");
        if let Some(callback) = fixture.before_write.take() {
            callback();
        }
        fixture.value.push_str(value);
        kAXErrorSuccess
    })
}
