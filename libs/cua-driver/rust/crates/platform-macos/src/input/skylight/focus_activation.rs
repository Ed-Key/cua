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

pub(super) fn background(
    ax: &impl Access,
    pid: i32,
    wid: u32,
    admission: &dyn Fn() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if !ax.can_record() {
        return Ok(false);
    }
    let Some(previous) = ax.previous() else {
        return Ok(false);
    };
    let Some(target) = ax.target(pid, wid) else {
        return Ok(false);
    };
    let mut buf = [0u8; 248];
    buf[0x04] = 0xF8;
    buf[0x08] = 0x0D;
    buf[0x3C..0x40].copy_from_slice(&wid.to_le_bytes());
    admission()?;
    buf[0x8A] = 0x02;
    let defocus = ax.record(previous, &buf);
    buf[0x8A] = 0x01;
    let focus = ax.record(target, &buf);
    Ok(defocus && focus)
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
        fn record(&self, _: Psn, _: &[u8; 248]) -> bool {
            self.events.borrow_mut().push("record");
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
            _ => background(&ax, 2, 42, admission).and_then(|_| body()),
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
}
