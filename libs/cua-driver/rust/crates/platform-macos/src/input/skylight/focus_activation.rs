//! Focus choreography with native effects supplied at the boundary.

use super::*;

type Psn = [u8; 8];
pub(super) trait Access {
    fn can_front(&self) -> bool;
    fn can_record(&self) -> bool;
    fn previous(&self) -> Option<Psn>;
    fn target(&self, pid: i32, wid: u32) -> Option<Psn>;
    fn front(&self, psn: Psn, wid: u32) -> i32;
    fn record(&self, psn: Psn, bytes: &[u8; 248]) -> bool;
    fn key(
        &self,
        pid: i32,
        wid: u32,
        admission: &dyn Fn() -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
    fn focused(&self, pid: i32) -> Option<u32>;
    fn wait(&self, pid: i32, wid: u32) -> bool;
    fn settle(&self);
}

pub(super) struct Native;
impl Access for Native {
    fn can_front(&self) -> bool {
        set_front_process_fn().is_some()
    }
    fn can_record(&self) -> bool {
        post_event_record_to_fn().is_some() && get_front_process_fn().is_some()
    }
    fn previous(&self) -> Option<Psn> {
        let mut psn = [0; 8];
        (unsafe { get_front_process_fn()?(psn.as_mut_ptr().cast()) } == 0).then_some(psn)
    }
    fn target(&self, pid: i32, wid: u32) -> Option<Psn> {
        let mut psn = [0; 8];
        get_process_psn_for_window(wid, pid, &mut psn).then_some(psn)
    }
    fn front(&self, psn: Psn, wid: u32) -> i32 {
        unsafe { set_front_process_fn().unwrap()(psn.as_ptr().cast(), wid, 0x400) }
    }
    fn record(&self, psn: Psn, bytes: &[u8; 248]) -> bool {
        unsafe { post_event_record_to_fn().unwrap()(psn.as_ptr().cast(), bytes.as_ptr()) == 0 }
    }
    fn key(
        &self,
        pid: i32,
        wid: u32,
        admission: &dyn Fn() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        make_exact_window_key_checked(pid, wid, admission).map(|_| ())
    }
    fn focused(&self, pid: i32) -> Option<u32> {
        crate::ax::bindings::focused_window_id_of_pid(pid)
    }
    fn wait(&self, pid: i32, wid: u32) -> bool {
        await_window_focused(pid, wid)
    }
    fn settle(&self) {
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}

// Target-only focus lifecycle adapted from trycua/cua#3530 (Rubin Carter,
// coauthor tisfeng), retaining this branch's checked native-access boundary.
struct TargetFocus<'a, A: Access> {
    ax: &'a A,
    target: Psn,
    window: u32,
    armed: bool,
}

fn focus_record(window: u32, focused: bool) -> [u8; 248] {
    let mut record = [0; 248];
    record[0x04] = 0xF8;
    record[0x08] = 0x0D;
    record[0x3C..0x40].copy_from_slice(&window.to_le_bytes());
    record[0x8A] = if focused { 1 } else { 2 };
    record
}

impl<A: Access> TargetFocus<'_, A> {
    fn finish(&mut self) -> anyhow::Result<()> {
        if !std::mem::take(&mut self.armed) {
            return Ok(());
        }
        let front = self.ax.previous().ok_or_else(|| {
            anyhow::anyhow!(
                "foreground identity unknown during target focus cleanup; target left unchanged"
            )
        })?;
        if front == self.target {
            // Never deactivate a process the user has actually brought forward.
            return Ok(());
        }
        anyhow::ensure!(
            self.ax
                .record(self.target, &focus_record(self.window, false)),
            "target focus cleanup record failed"
        );
        self.ax.settle();
        Ok(())
    }
}

impl<A: Access> Drop for TargetFocus<'_, A> {
    fn drop(&mut self) {
        // The synchronous worker owns this even if its async waiter is aborted.
        if let Err(error) = self.finish() {
            tracing::warn!(%error, "target-only focus cleanup during unwind failed");
        }
    }
}

fn background<'a, A: Access>(
    ax: &'a A,
    pid: i32,
    wid: u32,
    admission: &dyn Fn() -> anyhow::Result<()>,
) -> anyhow::Result<TargetFocus<'a, A>> {
    anyhow::ensure!(
        ax.can_record(),
        "target-only background focus is unavailable"
    );
    let target = ax
        .target(pid, wid)
        .ok_or_else(|| anyhow::anyhow!("target process identity unavailable"))?;
    let previous = ax
        .previous()
        .ok_or_else(|| anyhow::anyhow!("foreground identity unavailable before input"))?;
    admission()?;
    let focus = TargetFocus {
        ax,
        target,
        window: wid,
        armed: previous != target,
    };
    if focus.armed {
        // Arm cleanup before posting: a rejected SPI call may have partially
        // changed native state. Cleanup can address only the captured target.
        anyhow::ensure!(
            ax.record(target, &focus_record(wid, true)),
            "target focus record failed"
        );
    }
    Ok(focus)
}

pub(super) fn with_background<T>(
    ax: &impl Access,
    pid: i32,
    wid: u32,
    admission: &dyn Fn() -> anyhow::Result<()>,
    body: impl FnOnce(bool) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut focus = background(ax, pid, wid, admission)?;
    let used = focus.armed;
    if used {
        ax.settle();
    }
    // Revalidate after settling, before handing control to mouse dispatch.
    let result = admission().and_then(|_| body(used));
    let cleanup = focus.finish();
    match (result, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => {
            Err(cleanup.context("input may already have landed; verify before repeating"))
        }
        (Err(input), Err(cleanup)) => Err(input.context(format!(
            "target cleanup also failed: {cleanup}; verify before repeating"
        ))),
    }
}

pub(super) fn assist(
    ax: &impl Access,
    pid: i32,
    wid: u32,
    admission: &dyn Fn() -> anyhow::Result<()>,
    body: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if !ax.can_front() {
        admission()?;
        body()?;
        return Ok(false);
    }
    let previous = ax.previous();
    let Some(target) = ax.target(pid, wid) else {
        admission()?;
        body()?;
        return Ok(false);
    };
    admission()?;
    with_cleanup(
        || {
            if let Some(previous) = previous {
                ax.front(previous, 0);
            }
        },
        || {
            ax.front(target, wid);
            admission()?;
            ax.key(pid, wid, admission)?;
            ax.wait(pid, wid);
            admission()?;
            body()
        },
    )?;
    Ok(true)
}

pub(super) fn hid(
    ax: &impl Access,
    pid: i32,
    wid: u32,
    admission: &dyn Fn() -> anyhow::Result<()>,
    body: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    anyhow::ensure!(ax.can_front(), "foreground HID delivery is unavailable");
    let previous = ax.previous();
    let target = ax.target(pid, wid).ok_or_else(|| {
        anyhow::anyhow!("could not resolve target window for foreground HID delivery")
    })?;
    let focused = ax.focused(pid);
    if preserves_exact_existing_focus(
        previous.is_some(),
        previous.unwrap_or_default(),
        target,
        focused,
        wid,
    ) {
        admission()?;
        return body();
    }
    admission()?;
    with_cleanup(
        || {
            if let Some(previous) = previous {
                ax.front(previous, 0);
            }
        },
        || {
            anyhow::ensure!(
                ax.front(target, wid) == 0,
                "WindowServer rejected foreground HID activation"
            );
            admission()?;
            ax.key(pid, wid, admission)?;
            anyhow::ensure!(
                ax.wait(pid, wid),
                "exact target window did not become focused for foreground HID delivery"
            );
            let result = admission().and_then(|_| body());
            ax.settle();
            result
        },
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        events: RefCell<Vec<&'static str>>,
        current: Cell<bool>,
        invalidate_on_read: bool,
        invalidate_on_key_lookup: bool,
    }
    impl Access for Fake {
        fn can_front(&self) -> bool {
            true
        }
        fn can_record(&self) -> bool {
            true
        }
        fn previous(&self) -> Option<Psn> {
            Some([1; 8])
        }
        fn target(&self, _: i32, _: u32) -> Option<Psn> {
            if self.invalidate_on_read {
                self.current.set(false);
            }
            Some([2; 8])
        }
        fn front(&self, _: Psn, wid: u32) -> i32 {
            self.events
                .borrow_mut()
                .push(if wid == 0 { "restore" } else { "front" });
            0
        }
        fn record(&self, psn: Psn, bytes: &[u8; 248]) -> bool {
            self.events.borrow_mut().push(match (psn, bytes[0x8a]) {
                ([1, 1, 1, 1, 1, 1, 1, 1], _) => "foreground_record",
                (_, 1) => "target_focus",
                (_, 2) => "target_defocus",
                _ => "record",
            });
            true
        }
        fn key(
            &self,
            _: i32,
            wid: u32,
            admission: &dyn Fn() -> anyhow::Result<()>,
        ) -> anyhow::Result<()> {
            make_exact_window_key_with(
                wid,
                || {
                    self.events.borrow_mut().push("key_lookup");
                    if self.invalidate_on_key_lookup {
                        self.current.set(false);
                    }
                    Some([2; 8])
                },
                |psn, target| {
                    assert_eq!((psn, target), ([2; 8], 42));
                    self.events.borrow_mut().push("key");
                    true
                },
                |psn, record| {
                    assert_eq!(psn, [2; 8]);
                    assert_eq!(&record[0x3c..0x40], &42u32.to_le_bytes());
                    self.events.borrow_mut().push(match record[0x08] {
                        1 => "key_down",
                        2 => "key_up",
                        _ => panic!("unexpected make-key record"),
                    });
                    true
                },
                admission,
            )
            .map(|_| ())
        }
        fn focused(&self, _: i32) -> Option<u32> {
            None
        }
        fn wait(&self, _: i32, _: u32) -> bool {
            true
        }
        fn settle(&self) {
            self.events.borrow_mut().push("settle");
        }
    }
    pub(crate) fn exercise(
        kind: &str,
        admission: &dyn Fn() -> anyhow::Result<()>,
        body: impl FnOnce() -> anyhow::Result<()>,
    ) -> (anyhow::Result<()>, Vec<&'static str>) {
        let ax = Fake {
            events: RefCell::default(),
            current: Cell::new(true),
            invalidate_on_read: false,
            invalidate_on_key_lookup: false,
        };
        let result = match kind {
            "assist" => assist(&ax, 2, 42, admission, body).map(|_| ()),
            "hid" => hid(&ax, 2, 42, admission, body),
            _ => with_background(&ax, 2, 42, admission, |_| body()),
        };
        (result, ax.events.into_inner())
    }

    fn exercise_inner_key_lookup(cancel: bool) {
        for kind in ["assist", "hid"] {
            let ax = Fake {
                events: RefCell::default(),
                current: Cell::new(true),
                invalidate_on_read: false,
                invalidate_on_key_lookup: cancel,
            };
            let admit = || {
                anyhow::ensure!(
                    ax.current.get(),
                    "ownership invalidated during inner lookup"
                );
                Ok(())
            };
            let body = || {
                ax.events.borrow_mut().push("click");
                Ok(())
            };
            let result = if kind == "assist" {
                assist(&ax, 2, 42, &admit, body).map(|_| ())
            } else {
                hid(&ax, 2, 42, &admit, body)
            };
            if cancel {
                assert!(result.is_err());
                assert_eq!(
                    *ax.events.borrow(),
                    ["front", "key_lookup", "restore"],
                    "{kind}"
                );
            } else {
                assert!(result.is_ok());
                let mut expected =
                    vec!["front", "key_lookup", "key", "key_down", "key_up", "click"];
                if kind == "hid" {
                    expected.push("settle");
                }
                expected.push("restore");
                assert_eq!(*ax.events.borrow(), expected, "{kind}");
            }
        }
    }

    #[test]
    fn correction_foreground_restore_survives_body_error_and_unwind() {
        for kind in ["assist", "hid"] {
            let (result, events) = exercise(kind, &|| Ok(()), || anyhow::bail!("input failed"));
            assert!(result.is_err());
            assert_eq!(events.last(), Some(&"restore"));
            let ax = Fake {
                events: RefCell::default(),
                current: Cell::new(true),
                invalidate_on_read: false,
                invalidate_on_key_lookup: false,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if kind == "assist" {
                    assist(&ax, 2, 42, &|| Ok(()), || panic!("unwind")).map(|_| ())
                } else {
                    hid(&ax, 2, 42, &|| Ok(()), || panic!("unwind"))
                }
            }));
            assert!(result.is_err());
            assert_eq!(ax.events.borrow().last(), Some(&"restore"));
        }
    }

    #[test]
    fn correction_cancelled_activation_cannot_front_key_or_post_records() {
        for kind in ["assist", "hid", "background"] {
            for during_read in [false, true] {
                let ax = Fake {
                    events: RefCell::default(),
                    current: Cell::new(during_read),
                    invalidate_on_read: during_read,
                    invalidate_on_key_lookup: false,
                };
                let admit = || {
                    anyhow::ensure!(ax.current.get(), "cancelled");
                    Ok(())
                };
                let body = || {
                    ax.events.borrow_mut().push("input_and_contact");
                    Ok(())
                };
                let result = match kind {
                    "assist" => assist(&ax, 2, 42, &admit, body).map(|_| ()),
                    "hid" => hid(&ax, 2, 42, &admit, body),
                    _ => background(&ax, 2, 42, &admit).map(|_| ()),
                };
                assert!(result.is_err());
                assert!(
                    ax.events.borrow().is_empty(),
                    "{kind} during_read={during_read}: {:?}",
                    ax.events.borrow()
                );
            }
        }
    }

    // Uses the native make-key orchestration, including its own PSN lookup,
    // through the same checked key boundary used by both foreground routes.
    #[test]
    fn inner_key_lookup_cancellation_blocks_key_pair_and_click_but_restores() {
        exercise_inner_key_lookup(true);
    }

    #[test]
    fn admitted_inner_key_lookup_keeps_exact_native_pair_and_restoration() {
        exercise_inner_key_lookup(false);
    }

    #[test]
    fn background_never_sends_a_record_to_the_foreground_process() {
        let ax = Fake {
            events: RefCell::default(),
            current: Cell::new(true),
            invalidate_on_read: false,
            invalidate_on_key_lookup: false,
        };
        let _activation = background(&ax, 2, 42, &|| Ok(())).unwrap();
        assert_eq!(*ax.events.borrow(), ["target_focus"]);
    }

    struct Probe {
        records: RefCell<Vec<(Psn, [u8; 248])>>,
        front: Cell<Option<Psn>>,
        available: bool,
        fail_record: Option<u8>,
        cancel_on_settle: bool,
        current: Cell<bool>,
    }

    impl Default for Probe {
        fn default() -> Self {
            Self {
                records: RefCell::default(),
                front: Cell::new(Some([1; 8])),
                available: true,
                fail_record: None,
                cancel_on_settle: false,
                current: Cell::new(true),
            }
        }
    }

    impl Access for Probe {
        fn can_record(&self) -> bool {
            self.available
        }
        fn previous(&self) -> Option<Psn> {
            self.front.get()
        }
        fn target(&self, pid: i32, wid: u32) -> Option<Psn> {
            assert_eq!((pid, wid), (2, 42));
            Some([2; 8])
        }
        fn record(&self, psn: Psn, bytes: &[u8; 248]) -> bool {
            self.records.borrow_mut().push((psn, *bytes));
            self.fail_record != Some(bytes[0x8a])
        }
        fn settle(&self) {
            if self.cancel_on_settle {
                self.current.set(false);
            }
        }
        fn can_front(&self) -> bool {
            panic!("background must not front")
        }
        fn front(&self, _: Psn, _: u32) -> i32 {
            panic!("background must not front")
        }
        fn key(&self, _: i32, _: u32, _: &dyn Fn() -> anyhow::Result<()>) -> anyhow::Result<()> {
            panic!("background must not make a real key window")
        }
        fn focused(&self, _: i32) -> Option<u32> {
            panic!("unexpected AX focus lookup")
        }
        fn wait(&self, _: i32, _: u32) -> bool {
            panic!("unexpected foreground wait")
        }
    }

    fn assert_target_records(probe: &Probe, transitions: &[u8]) {
        let records = probe.records.borrow();
        assert_eq!(records.len(), transitions.len());
        for ((psn, bytes), transition) in records.iter().zip(transitions) {
            assert_eq!(*psn, [2; 8]);
            assert_eq!(bytes[4], 0xf8);
            assert_eq!(bytes[8], 0x0d);
            assert_eq!(&bytes[0x3c..0x40], &[42, 0, 0, 0]);
            assert_eq!(bytes[0x8a], *transition);
        }
    }

    #[test]
    fn target_focus_wraps_input_and_cleans_up_exactly_once() {
        let probe = Probe::default();
        let result = with_background(&probe, 2, 42, &|| Ok(()), |used| {
            assert!(used);
            assert_target_records(&probe, &[1]);
            Ok(7)
        });
        assert_eq!(result.unwrap(), 7);
        assert_target_records(&probe, &[1, 2]);
    }

    #[test]
    fn target_focus_cancellation_after_settle_blocks_input_and_cleans_up() {
        let probe = Probe {
            cancel_on_settle: true,
            ..Probe::default()
        };
        let result = with_background::<()>(
            &probe,
            2,
            42,
            &|| {
                anyhow::ensure!(probe.current.get(), "cancelled during settling");
                Ok(())
            },
            |_| panic!("cancelled input must not run"),
        );
        assert!(result.is_err());
        assert_target_records(&probe, &[1, 2]);
    }

    #[test]
    fn target_focus_error_and_unwind_keep_worker_cleanup() {
        for unwind in [false, true] {
            let probe = Probe::default();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_background::<()>(&probe, 2, 42, &|| Ok(()), |_| {
                    if unwind {
                        panic!("input unwind");
                    }
                    anyhow::bail!("input failed")
                })
            }));
            if unwind {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            assert_target_records(&probe, &[1, 2]);
        }
    }

    #[test]
    fn target_focus_cleanup_respects_current_foreground_identity() {
        for front in [Some([2; 8]), Some([3; 8]), None] {
            let probe = Probe::default();
            let result = with_background(&probe, 2, 42, &|| Ok(()), |_| {
                probe.front.set(front);
                Ok(())
            });
            assert_eq!(result.is_ok(), front.is_some());
            if front == Some([3; 8]) {
                assert_target_records(&probe, &[1, 2]);
            } else {
                assert_target_records(&probe, &[1]);
            }
        }
    }

    #[test]
    fn target_focus_failed_records_do_not_repeat_input_or_cleanup() {
        for failure in [1, 2] {
            let probe = Probe {
                fail_record: Some(failure),
                ..Probe::default()
            };
            let calls = Cell::new(0);
            let result = with_background(&probe, 2, 42, &|| Ok(()), |_| {
                calls.set(calls.get() + 1);
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(calls.get(), if failure == 1 { 0 } else { 1 });
            assert_target_records(&probe, &[1, 2]);
        }
    }

    #[test]
    fn target_focus_unavailable_or_unknown_stops_before_input() {
        for available in [false, true] {
            let probe = Probe {
                available,
                front: Cell::new(None),
                ..Probe::default()
            };
            let result = with_background::<()>(&probe, 2, 42, &|| Ok(()), |_| {
                panic!("unavailable background route must not dispatch")
            });
            assert!(result.is_err());
            assert_target_records(&probe, &[]);
        }
    }

    #[test]
    fn target_focus_already_foreground_needs_no_synthetic_records() {
        let probe = Probe {
            front: Cell::new(Some([2; 8])),
            ..Probe::default()
        };
        with_background(&probe, 2, 42, &|| Ok(()), |used| {
            assert!(!used);
            Ok(())
        })
        .unwrap();
        assert_target_records(&probe, &[]);
    }

    #[tokio::test]
    async fn target_focus_worker_cleans_up_after_async_waiter_cancellation() {
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, pause) = std::sync::mpsc::channel();
        let (cleaned, cleanup) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let probe = Probe::default();
                let result = with_background::<()>(&probe, 2, 42, &|| Ok(()), |_| {
                    entered.send(()).unwrap();
                    pause.recv().unwrap();
                    anyhow::bail!("input failed after cancellation")
                });
                assert!(result.is_err());
                assert_target_records(&probe, &[1, 2]);
                cleaned.send(()).unwrap();
            })
            .await
        });
        waiting.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        cleanup.await.unwrap();
    }
}
