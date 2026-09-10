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
}

pub(crate) struct SelectionScope;

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
            });
        });
        Self
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
    if element as usize != usize::MAX {
        return None;
    }
    SELECTION.with(|slot| {
        Some(f(slot
            .borrow_mut()
            .as_mut()
            .expect("active selection fixture")))
    })
}

pub(super) fn copy_string_attr(element: AXUIElementRef, attr: &str) -> Option<Option<String>> {
    if let Some(value) = with_editor(element, |fixture| match attr {
        "AXRole" => Some(fixture.role.clone()),
        "AXTitle" => Some("Editor".into()),
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
    with_fixture(element, |_| {
        assert_eq!(attr, "AXParent");
        None
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
        fixture.write_result
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
                before_write: Some(Box::new(before_write)),
            });
        });
        Self { _element: element }
    }
}
impl Drop for EditorScope {
    fn drop(&mut self) {
        EDITOR.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(crate) fn focused_editor(pid: i32, wid: u32) -> Option<Option<AXUIElementRef>> {
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
