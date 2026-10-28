//! Window-scoped keyboard delivery to a proven hosted view service.

use anyhow::{bail, Context, Result};
use core_foundation::base::{CFEqual, CFRelease, CFTypeRef};
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};

use super::bindings::{actual_pid, copy_element_attr, AXUIElementRef};
use super::exact_target::focused_element_in_window;
use super::snapshot::RetainedElement;

/// Keep host authorization, focus, and readback separate from the event recipient.
pub(crate) fn with_keyboard_recipient<T>(
    host_pid: i32,
    window_id: Option<u32>,
    addressed_element: Option<usize>,
    dispatch: impl FnOnce(i32) -> Result<T>,
) -> Result<T> {
    let Some(window_id) = window_id else {
        return dispatch(host_pid);
    };
    let Some(focused) = read_focused(host_pid, window_id) else {
        return dispatch(host_pid);
    };
    let remote = remote_recipient(host_pid, &focused);
    tracing::debug!(
        host_pid,
        window_id,
        ?remote,
        "background keyboard recipient lookup"
    );
    with_proven_recipient(
        host_pid,
        remote,
        |pid| {
            crate::background_mutation::try_acquire(pid)
                .map_err(|_| anyhow::anyhow!("background keyboard recipient is busy"))
        },
        |pid| {
            if let Some(addressed) = addressed_element {
                if !same_element(addressed, focused.as_ptr()) {
                    bail!("background keyboard target is not the focused control");
                }
            }
            validate_recipient(host_pid, Some(window_id), Some(focused.as_ptr()), pid)?;
            tracing::debug!(
                host_pid,
                window_id,
                recipient_pid = pid,
                "dispatching to hosted view service"
            );
            Ok(())
        },
        dispatch,
    )
}

/// The view service that hosts the focused control of `window_id` (an Open
/// or Save panel's service), if any: keys for that window are routed there.
pub(crate) fn hosted_view_service(host_pid: i32, window_id: u32) -> Option<i32> {
    remote_recipient(host_pid, &read_focused(host_pid, window_id)?)
}

/// Repeat the proof after a caller's final focus preparation, before its first post.
pub(crate) fn validate_recipient(
    host_pid: i32,
    window_id: Option<u32>,
    expected_element: Option<usize>,
    recipient_pid: i32,
) -> Result<()> {
    if recipient_pid == host_pid {
        if let Some(fresh) = window_id.and_then(|wid| read_focused(host_pid, wid)) {
            if remote_recipient(host_pid, &fresh).is_some() {
                bail!("background keyboard recipient changed before dispatch");
            }
        }
        return Ok(());
    }
    let window_id = window_id.context("hosted keyboard delivery requires an exact window")?;
    if !is_view_service(recipient_pid) {
        bail!("background keyboard recipient is not a live view service");
    }
    let fresh = read_focused(host_pid, window_id)
        .context("background keyboard focus could not be revalidated")?;
    validate_remote_resolution(
        expected_element.is_some_and(|expected| same_element(expected, fresh.as_ptr())),
        recipient_pid,
        remote_recipient(host_pid, &fresh),
    )
}

const MAX_PARENT_DEPTH: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventHandler {
    RegularApp(i32),
    ViewService(i32),
}

impl EventHandler {
    fn remote_pid(self, host_pid: i32) -> Option<i32> {
        match self {
            Self::ViewService(pid) if pid != host_pid => Some(pid),
            _ => None,
        }
    }
}

// Native checks depth > 300: self is depth 0, including depth 300.
fn first_event_handler<E>(
    mut element: E,
    classify: impl Fn(&E) -> Option<EventHandler>,
    parent: impl Fn(&E) -> Option<E>,
) -> Option<(usize, EventHandler)> {
    for depth in 0..=MAX_PARENT_DEPTH {
        if let Some(handler) = classify(&element) {
            return Some((depth, handler));
        }
        if depth == MAX_PARENT_DEPTH {
            break;
        }
        element = parent(&element)?;
    }
    None
}

fn remote_recipient(host_pid: i32, focused: &RetainedElement) -> Option<i32> {
    let first = first_event_handler(
        focused.clone(),
        |element| {
            unsafe { actual_pid(element.as_ptr() as AXUIElementRef) }.and_then(classify_process)
        },
        |element| unsafe {
            let parent = copy_element_attr(element.as_ptr() as AXUIElementRef, "AXParent")?;
            let retained = RetainedElement::retain(parent as usize);
            CFRelease(parent as CFTypeRef);
            Some(retained)
        },
    );
    tracing::debug!(
        host_pid,
        ?first,
        "background keyboard first event-handling ancestor"
    );
    // A regular app or the host stops the search. Never continue to a remote
    // parent after rejecting that first eligible process.
    first.and_then(|(_, handler)| handler.remote_pid(host_pid))
}

fn validate_remote_resolution(
    same_focused_leaf: bool,
    expected_pid: i32,
    fresh_remote: Option<i32>,
) -> Result<()> {
    if !same_focused_leaf || fresh_remote != Some(expected_pid) {
        bail!("background keyboard recipient changed before dispatch");
    }
    Ok(())
}

fn classify_process(pid: i32) -> Option<EventHandler> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        classify_running_process(
            pid,
            app.isTerminated(),
            app.isFinishedLaunching(),
            app.activationPolicy(),
        )
    })
}

fn classify_running_process(
    pid: i32,
    terminated: bool,
    finished_launching: bool,
    policy: NSApplicationActivationPolicy,
) -> Option<EventHandler> {
    // CUA also rejects terminated applications; that extra safety predicate
    // was not demonstrated by the recovered native classifier.
    if terminated || !finished_launching {
        return None;
    }
    Some(if policy == NSApplicationActivationPolicy::Prohibited {
        EventHandler::ViewService(pid)
    } else {
        EventHandler::RegularApp(pid)
    })
}

fn is_view_service(pid: i32) -> bool {
    matches!(classify_process(pid), Some(EventHandler::ViewService(_)))
}

fn read_focused(pid: i32, window_id: u32) -> Option<RetainedElement> {
    unsafe {
        let element = focused_element_in_window(pid, window_id)?;
        let retained = RetainedElement::retain(element as usize);
        CFRelease(element as CFTypeRef);
        Some(retained)
    }
}

fn same_element(left: usize, right: usize) -> bool {
    unsafe { CFEqual(left as CFTypeRef, right as CFTypeRef) != 0 }
}

/// One recipient for the complete dispatch, locked and revalidated before input.
fn with_proven_recipient<T, G>(
    host_pid: i32,
    remote_pid: Option<i32>,
    acquire: impl FnOnce(i32) -> Result<G>,
    verify: impl FnOnce(i32) -> Result<()>,
    dispatch: impl FnOnce(i32) -> Result<T>,
) -> Result<T> {
    let Some(remote_pid) = remote_pid.filter(|pid| *pid != host_pid) else {
        return dispatch(host_pid);
    };
    if remote_pid <= 0 {
        bail!("background keyboard recipient pid is invalid");
    }
    let _recipient_lease = acquire(remote_pid)?;
    verify(remote_pid)?;
    dispatch(remote_pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn nearest_finished_handler_stops_before_remote_routing_decision() {
        let host = Some(EventHandler::RegularApp(91_120));
        let host_service = Some(EventHandler::ViewService(91_120));
        let remote = Some(EventHandler::ViewService(91_121));
        let foreign_regular = Some(EventHandler::RegularApp(91_122));
        let cases: &[(&[Option<EventHandler>], Option<i32>)] = &[
            (&[host, remote], None),
            (&[host_service, remote], None),
            (&[None, remote], Some(91_121)),
            (&[foreign_regular, remote], None),
            (&[None, None, remote], Some(91_121)),
            (&[None, host, remote], None),
        ];
        for &(chain, expected) in cases {
            let visited = std::cell::RefCell::new(Vec::new());
            let first = first_event_handler(
                0,
                |index| {
                    visited.borrow_mut().push(*index);
                    chain[*index]
                },
                |index| (index + 1 < chain.len()).then_some(index + 1),
            );
            assert_eq!(
                first.and_then(|(_, handler)| handler.remote_pid(91_120)),
                expected
            );
            let first_eligible = chain
                .iter()
                .position(Option::is_some)
                .unwrap_or(chain.len() - 1);
            assert_eq!(
                visited.into_inner(),
                (0..=first_eligible).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn unfinished_and_terminated_processes_are_skipped_before_policy_selection() {
        for policy in [
            NSApplicationActivationPolicy::Prohibited,
            NSApplicationActivationPolicy::Regular,
            NSApplicationActivationPolicy::Accessory,
        ] {
            assert_eq!(classify_running_process(91_127, false, false, policy), None);
            assert_eq!(classify_running_process(91_127, true, true, policy), None);
            assert_eq!(
                classify_running_process(91_127, false, true, policy),
                Some(if policy == NSApplicationActivationPolicy::Prohibited {
                    EventHandler::ViewService(91_127)
                } else {
                    EventHandler::RegularApp(91_127)
                }),
            );
        }
    }

    #[test]
    fn ancestor_depth_is_inclusive_and_cycles_are_finite() {
        let classified = Cell::new(0);
        let parents = Cell::new(0);
        let scan = |qualifying_depth| {
            classified.set(0);
            parents.set(0);
            first_event_handler(
                0usize,
                |depth| {
                    classified.set(classified.get() + 1);
                    (*depth == qualifying_depth).then_some(EventHandler::ViewService(91_123))
                },
                |depth| {
                    parents.set(parents.get() + 1);
                    Some(depth + 1)
                },
            )
        };
        assert_eq!(scan(300), Some((300, EventHandler::ViewService(91_123))));
        assert_eq!(classified.get(), 301);
        assert_eq!(parents.get(), 300);
        assert_eq!(scan(301), None);
        assert_eq!(classified.get(), 301);
        assert_eq!(parents.get(), 300);
        assert_eq!(first_event_handler(0, |_| None, |node| Some(*node)), None);
    }

    #[test]
    fn changed_ancestor_or_focused_leaf_refuses_before_post_and_releases_recipient() {
        for (same_leaf, fresh_first) in [
            (true, Some(EventHandler::RegularApp(91_124))),
            (true, Some(EventHandler::ViewService(91_126))),
            (true, None),
            (false, Some(EventHandler::ViewService(91_125))),
        ] {
            let posts = Cell::new(0);
            let error = with_proven_recipient(
                91_124,
                Some(91_125),
                |pid| crate::background_mutation::try_acquire(pid).map_err(Into::into),
                |pid| {
                    let fresh = first_event_handler(0, |_| fresh_first, |_| None)
                        .and_then(|(_, handler)| handler.remote_pid(91_124));
                    validate_remote_resolution(same_leaf, pid, fresh)
                },
                |_| {
                    posts.set(posts.get() + 1);
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "background keyboard recipient changed before dispatch"
            );
            assert_eq!(posts.get(), 0);
            assert!(crate::background_mutation::try_acquire(91_125).is_ok());
        }
    }

    #[test]
    fn unknown_or_local_recipient_keeps_host_delivery() {
        for recipient in [None, Some(91_101)] {
            let delivered = with_proven_recipient(
                91_101,
                recipient,
                |_| -> Result<()> { panic!("local delivery does not acquire a second lease") },
                |_| panic!("local delivery retains its existing proof"),
                Ok,
            )
            .unwrap();
            assert_eq!(delivered, 91_101);
        }
    }

    #[test]
    fn remote_dispatch_holds_recipient_lease_and_releases_it_afterward() {
        let delivered = with_proven_recipient(
            91_102,
            Some(91_103),
            |pid| crate::background_mutation::try_acquire(pid).map_err(Into::into),
            |pid| {
                assert_eq!(pid, 91_103);
                Ok(())
            },
            |pid| {
                assert!(crate::background_mutation::try_acquire(pid).is_err());
                Ok(pid)
            },
        )
        .unwrap();
        assert_eq!(delivered, 91_103);
        assert!(crate::background_mutation::try_acquire(91_103).is_ok());
    }

    #[test]
    fn busy_shared_service_posts_nothing_without_waiting_for_another_host() {
        let _first_host = crate::background_mutation::try_acquire(91_106).unwrap();
        let posts = Cell::new(0);
        let error = with_proven_recipient(
            91_107,
            Some(91_106),
            |pid| {
                crate::background_mutation::try_acquire(pid)
                    .map_err(|_| anyhow::anyhow!("background keyboard recipient is busy"))
            },
            |_| panic!("busy recipient must refuse before verification"),
            |_| {
                posts.set(posts.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "background keyboard recipient is busy");
        assert_eq!(posts.get(), 0);
    }

    #[test]
    fn invalid_recipient_never_acquires_or_posts() {
        for pid in [0, -1] {
            let error = with_proven_recipient(
                91_108,
                Some(pid),
                |_| -> Result<()> { panic!("invalid recipient must not acquire") },
                |_| panic!("invalid recipient must not query AX"),
                |_| -> Result<()> { panic!("invalid recipient must not post") },
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "background keyboard recipient pid is invalid"
            );
        }
    }
}
