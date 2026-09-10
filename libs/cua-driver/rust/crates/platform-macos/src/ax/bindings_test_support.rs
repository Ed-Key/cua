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
            });
        });
        Self
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
