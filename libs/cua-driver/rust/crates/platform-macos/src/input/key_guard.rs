//! Foreground keys reach only the process the call targeted.
//!
//! A key posted on the global HID queue has no process address: WindowServer
//! hands it to whichever app is in front when it routes it, which can be a
//! moment after the post. A foreground action fronts its target, posts, then
//! gives the user's app the front back; if anything brings the user's app
//! back before every key has been routed, the keys land there (item 38: a
//! Cmd+A meant for Safari selected all in the user's TextEdit document).
//!
//! A [`KeyScope`] covers one foreground action (from the target's activation
//! through the hand-back). Every HID key event cua posts on the scope's
//! thread goes through [`post_hid_key`], which:
//!
//! - before the first key, installs one active tap at the session's
//!   annotated point (where each event already names the process it will be
//!   delivered to), and refuses, sending nothing, when that tap cannot run;
//! - before every key down, requires the exact target window's process to
//!   be WindowServer's front process (a bounded wait for the first one), and
//!   stops once any key went astray, so no partial chord follows;
//! - tags the event with this scope's nonce.
//!
//! The tap passes a tagged event bound for the target (or its window's
//! owner) and counts it delivered. A tagged event bound for any other
//! process is withheld from it; a release (key up, modifier change) is then
//! given to the target by pid, so a modifier it saw pressed is released
//! there. [`KeyScope::settle`] waits until every posted event was routed one
//! way or the other, and the hand-back of the front runs only after it.
//! While a scope is open, cua's reactive focus guard does not restore the
//! user's app (`foreground_input_open`); the foreground turn already keeps
//! every other cua front change out.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_graphics::event::{CGEvent, CGEventTapLocation};
use crate::focus_event_tap::{EventTap, TapDecision};

const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
/// kCGKeyboardEventKeycode, kCGEventTargetUnixProcessID,
/// kCGEventSourceUnixProcessID, kCGEventSourceUserData.
const FIELD_TARGET_PID: u32 = 40;
const FIELD_SOURCE_PID: u32 = 41;
const FIELD_USER_DATA: u32 = 42;

/// How long the first key waits for the target to become the front process
/// (activation is asynchronous), the bound [`with_foreground_assist`] uses.
///
/// [`with_foreground_assist`]: crate::input::skylight::with_foreground_assist
const FRONT_WAIT: Duration = Duration::from_millis(400);
const FRONT_POLL: Duration = Duration::from_millis(10);
/// How long the hand-back waits for posted keys to be routed.
pub(crate) const SETTLE_TIMEOUT: Duration = Duration::from_millis(1000);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventGetIntegerValueField(event: *mut std::ffi::c_void, field: u32) -> i64;
    fn CGEventPostToPid(pid: i32, event: *mut std::ffi::c_void);
}

/// Open foreground scopes (any thread).
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// Whether a foreground action is between activating its target and handing
/// the front back. A restore of the user's app now would take the action's
/// input.
pub(crate) fn foreground_input_open() -> bool {
    OPEN.load(Ordering::SeqCst) > 0
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Recipient>>> = const { RefCell::new(None) };
}

/// What the tap reads and counts.
struct Ledger {
    nonce: i64,
    own_pid: i64,
    /// The target pid and its window's owner (the same process unless the
    /// window belongs to a helper process, read before the first key).
    accept: [AtomicI32; 2],
    posted: AtomicUsize,
    delivered: AtomicUsize,
    astray: AtomicUsize,
    astray_to: AtomicI32,
    lost: AtomicBool,
}

impl Ledger {
    fn routed(&self) -> usize {
        self.delivered.load(Ordering::SeqCst) + self.astray.load(Ordering::SeqCst)
    }

    fn accepts(&self, pid: i32) -> bool {
        pid > 0 && self.accept.iter().any(|a| a.load(Ordering::SeqCst) == pid)
    }

    /// The tap's decision for one event.
    fn decide(&self, kind: u32, source: i64, user_data: i64, target: i32) -> Decision {
        if source != self.own_pid || user_data != self.nonce {
            return Decision::Pass;
        }
        if self.accepts(target) {
            self.delivered.fetch_add(1, Ordering::SeqCst);
            return Decision::Pass;
        }
        self.astray_to.store(target, Ordering::SeqCst);
        self.astray.fetch_add(1, Ordering::SeqCst);
        if kind == KEY_DOWN {
            Decision::Withhold
        } else {
            Decision::WithholdAndGiveToTarget
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Pass,
    Withhold,
    WithholdAndGiveToTarget,
}

struct Recipient {
    pid: i32,
    window_id: u32,
    ledger: Arc<Ledger>,
    tap: Mutex<Option<EventTap>>,
}

/// Why a foreground key was not sent or did not reach the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRefusal {
    /// The guard tap could not be installed or stopped running: nothing
    /// further was sent.
    GuardUnavailable(String),
    /// The target was not the front process when a key was due.
    NotFront { target: i32, front: Option<i32> },
    /// A key was routed to another process and withheld from it.
    Astray { target: i32, to: i32 },
    /// Posted keys were not all routed within the bound.
    Unsettled { target: i32, posted: usize, routed: usize },
}

fn app_name(pid: i32) -> String {
    crate::apps::get_app_name_for_pid(pid).unwrap_or_else(|| "another app".into())
}

impl std::fmt::Display for KeyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GuardUnavailable(why) => write!(
                f,
                "cua could not guard the foreground keys ({why}), so it sent no further key; \
                 nothing reached another app"
            ),
            Self::NotFront { target, front } => {
                let front = match front {
                    Some(pid) => format!("{} (pid {pid})", app_name(*pid)),
                    None => "unknown".into(),
                };
                write!(
                    f,
                    "{} (pid {target}) was not the frontmost app when its key was due (the front \
                     was {front}), so cua sent no further key; nothing reached another app. Read \
                     the window before retrying",
                    app_name(*target)
                )
            }
            Self::Astray { target, to } => write!(
                f,
                "the front moved to {} (pid {to}) while the keys for pid {target} were on their \
                 way; cua withheld every key bound for it, released any pressed modifier to pid \
                 {target} and sent nothing more, so the keys did not all arrive. Read the window \
                 before retrying",
                app_name(*to)
            ),
            Self::Unsettled { target, posted, routed } => write!(
                f,
                "{routed} of {posted} keys for pid {target} were routed within {} ms; any key \
                 still on its way after that is withheld from every other app. Read the window \
                 before retrying",
                SETTLE_TIMEOUT.as_millis()
            ),
        }
    }
}

impl std::error::Error for KeyRefusal {}

/// One foreground action's key guard: open from the target's activation
/// until dropped after the hand-back.
pub struct KeyScope {
    recipient: Arc<Recipient>,
    outer: Option<Arc<Recipient>>,
}

impl KeyScope {
    pub fn open(pid: i32, window_id: u32) -> Self {
        static NONCES: AtomicI64 = AtomicI64::new(0);
        // A value no other software is likely to put in the source user data.
        let nonce = 0x0C0A_3800_0000_0000 | NONCES.fetch_add(1, Ordering::SeqCst);
        let recipient = Arc::new(Recipient {
            pid,
            window_id,
            ledger: Arc::new(Ledger {
                nonce,
                own_pid: std::process::id() as i64,
                accept: [AtomicI32::new(pid), AtomicI32::new(0)],
                posted: AtomicUsize::new(0),
                delivered: AtomicUsize::new(0),
                astray: AtomicUsize::new(0),
                astray_to: AtomicI32::new(0),
                lost: AtomicBool::new(false),
            }),
            tap: Mutex::new(None),
        });
        OPEN.fetch_add(1, Ordering::SeqCst);
        let outer = CURRENT.with(|c| c.borrow_mut().replace(recipient.clone()));
        Self { recipient, outer }
    }

    /// Wait until every key posted in this scope was routed. `Ok` when none
    /// were posted or all reached the target.
    pub fn settle(&self) -> Result<(), KeyRefusal> {
        let ledger = &self.recipient.ledger;
        let posted = ledger.posted.load(Ordering::SeqCst);
        if posted == 0 {
            return Ok(());
        }
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while ledger.routed() < posted && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let astray = ledger.astray.load(Ordering::SeqCst);
        if astray > 0 {
            return Err(KeyRefusal::Astray {
                target: self.recipient.pid,
                to: ledger.astray_to.load(Ordering::SeqCst),
            });
        }
        let routed = ledger.routed();
        if routed < posted {
            return Err(KeyRefusal::Unsettled { target: self.recipient.pid, posted, routed });
        }
        Ok(())
    }
}

impl Drop for KeyScope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.outer.take());
        // A key still on its way past the hand-back stays guarded briefly.
        let ledger = &self.recipient.ledger;
        let posted = ledger.posted.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_millis(200);
        while ledger.routed() < posted && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if let Some(tap) = self.recipient.tap.lock().unwrap_or_else(|p| p.into_inner()).take() {
            tap.close();
        }
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How a key post relates to the chord: a press may start something the
/// target must finish; a release ends what was pressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KeyRole {
    Press,
    Release,
}

/// Post one keyboard event on the global HID queue. Outside a
/// [`KeyScope`] (desktop scope: the front app is the target by contract) it
/// is posted as is.
pub(crate) fn post_hid_key(event: &CGEvent, role: KeyRole) -> anyhow::Result<()> {
    let Some(recipient) = CURRENT.with(|c| c.borrow().clone()) else {
        event.post(CGEventTapLocation::HID);
        return Ok(());
    };
    recipient.post(event, role)
}

impl Recipient {
    fn post(&self, event: &CGEvent, role: KeyRole) -> anyhow::Result<()> {
        let ledger = &self.ledger;
        let mut tap = self.tap.lock().unwrap_or_else(|p| p.into_inner());
        if role == KeyRole::Press {
            if ledger.astray.load(Ordering::SeqCst) > 0 {
                return Err(KeyRefusal::Astray {
                    target: self.pid,
                    to: ledger.astray_to.load(Ordering::SeqCst),
                }
                .into());
            }
            if tap.is_none() {
                *tap = Some(self.install_tap().map_err(KeyRefusal::GuardUnavailable)?);
            }
            if ledger.lost.load(Ordering::SeqCst) || !tap.as_ref().is_some_and(EventTap::is_active) {
                return Err(KeyRefusal::GuardUnavailable("its event tap stopped".into()).into());
            }
            let first = ledger.posted.load(Ordering::SeqCst) == 0;
            if first {
                if let Some(window) = crate::windows::window_info_by_id(self.window_id) {
                    ledger.accept[1].store(window.pid, Ordering::SeqCst);
                }
            }
            self.await_front(if first { FRONT_WAIT } else { Duration::ZERO })?;
        } else if tap.is_none() {
            // Nothing was pressed under this guard.
            event.post(CGEventTapLocation::HID);
            return Ok(());
        }
        event.set_integer_value_field(FIELD_USER_DATA, ledger.nonce);
        ledger.posted.fetch_add(1, Ordering::SeqCst);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn await_front(&self, wait: Duration) -> Result<(), KeyRefusal> {
        let deadline = Instant::now() + wait;
        loop {
            let front = crate::input::skylight::front_pid();
            if front.is_some_and(|pid| self.ledger.accepts(pid)) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(KeyRefusal::NotFront { target: self.pid, front });
            }
            std::thread::sleep(FRONT_POLL);
        }
    }

    fn install_tap(&self) -> Result<EventTap, String> {
        let ledger = self.ledger.clone();
        let target = self.pid;
        let lost = self.ledger.clone();
        EventTap::start_annotated_session(
            &[KEY_DOWN, KEY_UP, FLAGS_CHANGED],
            move |kind, event| {
                // SAFETY: the tap borrows `event` for this callback only.
                let (source, user_data, to) = unsafe {
                    (
                        CGEventGetIntegerValueField(event, FIELD_SOURCE_PID),
                        CGEventGetIntegerValueField(event, FIELD_USER_DATA),
                        CGEventGetIntegerValueField(event, FIELD_TARGET_PID) as i32,
                    )
                };
                match ledger.decide(kind, source, user_data, to) {
                    Decision::Pass => TapDecision::Pass,
                    Decision::Withhold => TapDecision::Drop,
                    Decision::WithholdAndGiveToTarget => {
                        // SAFETY: as above; posting copies the event.
                        unsafe { CGEventPostToPid(target, event) };
                        TapDecision::Drop
                    }
                }
            },
            move || lost.lost.store(true, Ordering::SeqCst),
        )
        .map_err(|error| error.to_string())
        .and_then(|tap| if tap.is_active() { Ok(tap) } else { Err("its event tap is disabled".into()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(target: i32, owner: i32) -> Ledger {
        Ledger {
            nonce: 77,
            own_pid: 500,
            accept: [AtomicI32::new(target), AtomicI32::new(owner)],
            posted: AtomicUsize::new(0),
            delivered: AtomicUsize::new(0),
            astray: AtomicUsize::new(0),
            astray_to: AtomicI32::new(0),
            lost: AtomicBool::new(false),
        }
    }

    /// Item 38: Cmd down reached Safari, then the user's TextEdit came back
    /// in front and A down, A up and the Cmd release were routed there.
    /// Under the guard TextEdit gets none of them; the releases go to Safari.
    #[test]
    fn keys_routed_to_another_app_are_withheld_and_releases_go_to_the_target() {
        let l = ledger(716, 716);
        assert_eq!(l.decide(FLAGS_CHANGED, 500, 77, 716), Decision::Pass);
        assert_eq!(l.decide(KEY_DOWN, 500, 77, 918), Decision::Withhold);
        assert_eq!(l.decide(KEY_UP, 500, 77, 918), Decision::WithholdAndGiveToTarget);
        assert_eq!(l.decide(FLAGS_CHANGED, 500, 77, 918), Decision::WithholdAndGiveToTarget);
        assert_eq!(l.delivered.load(Ordering::SeqCst), 1);
        assert_eq!(l.astray.load(Ordering::SeqCst), 3);
        assert_eq!(l.astray_to.load(Ordering::SeqCst), 918);
    }

    /// The tap sees every key in the session: the user's own keys, other
    /// cua calls' pid-routed keys and another scope's keys pass untouched.
    #[test]
    fn only_this_scopes_own_events_are_judged() {
        let l = ledger(716, 716);
        assert_eq!(l.decide(KEY_DOWN, 0, 0, 918), Decision::Pass, "the user's key");
        assert_eq!(l.decide(KEY_DOWN, 500, 0, 918), Decision::Pass, "an untagged cua key");
        assert_eq!(l.decide(KEY_DOWN, 500, 78, 918), Decision::Pass, "another scope's key");
        assert_eq!(l.decide(KEY_DOWN, 501, 77, 918), Decision::Pass, "another process");
        assert_eq!(l.routed(), 0);
    }

    /// A window owned by a helper process (a panel's view service) counts
    /// as the target.
    #[test]
    fn the_target_windows_owner_counts_as_the_target() {
        let l = ledger(716, 4242);
        assert_eq!(l.decide(KEY_DOWN, 500, 77, 4242), Decision::Pass);
        assert_eq!(l.decide(KEY_DOWN, 500, 77, 0), Decision::Withhold, "unrouted is not the target");
        assert_eq!(l.delivered.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_scope_marks_foreground_input_open_until_dropped() {
        {
            let scope = KeyScope::open(1, 1);
            assert!(foreground_input_open());
            assert!(CURRENT.with(|c| c.borrow().is_some()));
            assert_eq!(scope.settle(), Ok(()), "no keys posted");
        }
        // OPEN is process-wide and other tests may hold a scope meanwhile.
        assert!(CURRENT.with(|c| c.borrow().is_none()));
    }

    #[test]
    fn nested_scopes_restore_the_outer_recipient() {
        let outer = KeyScope::open(1, 1);
        {
            let _inner = KeyScope::open(2, 2);
            assert_eq!(CURRENT.with(|c| c.borrow().as_ref().map(|r| r.pid)), Some(2));
        }
        assert_eq!(CURRENT.with(|c| c.borrow().as_ref().map(|r| r.pid)), Some(1));
        drop(outer);
    }
}
