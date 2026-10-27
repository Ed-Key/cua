//! Foreground keys reach only the process the call targeted.
//!
//! A key posted on the global HID queue has no process address: WindowServer
//! hands it to whichever app is in front when it routes the key, which can be
//! well after the post. A foreground action fronts its target, posts, then
//! gives the user's app the front back. It used to hand back 40 ms after its
//! last post; in VM B the keys of a freshly activated target were routed up
//! to about 80 ms after they were posted, so the hand-back overtook them and
//! they landed in the user's app (item 38: a Cmd+A meant for Safari selected
//! all in the user's TextEdit document, 12 of 60 probe cases on 605b7dd8b).
//!
//! A [`KeyScope`] covers one foreground action (from the target's activation
//! through the hand-back). Every HID key event cua posts on the scope's
//! thread goes through [`post_hid_key`], which:
//!
//! - before the first key, installs one active tap at the session's
//!   annotated point (where each event already names the process it will be
//!   delivered to), and refuses, sending nothing, when that tap cannot run;
//! - before every press, requires WindowServer's front process to be the
//!   target (or the owner of its window), with a bounded wait for the first
//!   one, and stops once any press went astray, so no partial chord follows;
//! - tags the event with this scope's nonce and whether it presses or
//!   releases.
//!
//! The tap passes a tagged press bound for the target and remembers which
//! process got it; a press bound anywhere else is withheld. A release goes to
//! the process that got its press: passed when routed there, otherwise
//! withheld and given to that process by pid; a release whose press was never
//! delivered is withheld unless it is bound for the target.
//! [`KeyScope::settle`] waits until every posted event was routed; the
//! hand-back of the front runs only after that, and not at all while any key
//! is still on its way (the tap then stays until it is routed). While a scope
//! is open, cua's reactive focus guard does not restore the user's app
//! ([`foreground_input_open`]); the foreground turn already keeps every other
//! cua front change out.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_graphics::event::{CGEvent, CGEventTapLocation};
use foreign_types::ForeignType;

use crate::focus_event_tap::{EventTap, TapDecision};

const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
/// kCGKeyboardEventKeycode, kCGEventTargetUnixProcessID,
/// kCGEventSourceUnixProcessID, kCGEventSourceUserData.
const FIELD_KEYCODE: u32 = 9;
const FIELD_TARGET_PID: u32 = 40;
const FIELD_SOURCE_PID: u32 = 41;
const FIELD_USER_DATA: u32 = 42;
/// The low bit of the tag marks a release.
const RELEASE_BIT: i64 = 1;

/// How long the first key waits for the target to become the front process
/// (activation is asynchronous), the bound `with_foreground_assist` uses.
const FRONT_WAIT: Duration = Duration::from_millis(400);
const FRONT_POLL: Duration = Duration::from_millis(10);
/// How long the hand-back waits for posted keys to be routed.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long the tap outlives its scope for keys still on their way (keys
/// are routed within milliseconds; this bounds a stalled WindowServer).
const LATE_KEY_GUARD: Duration = Duration::from_secs(30);

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
    /// Keys whose press was delivered, and the process that got it.
    held: Mutex<Vec<(i64, i32)>>,
    posted: AtomicUsize,
    routed: AtomicUsize,
    astray: AtomicUsize,
    astray_to: AtomicI32,
    lost: AtomicBool,
}

impl Ledger {
    fn new(nonce: i64, own_pid: i64, target: i32) -> Self {
        Ledger {
            nonce,
            own_pid,
            accept: [AtomicI32::new(target), AtomicI32::new(0)],
            held: Mutex::new(Vec::new()),
            posted: AtomicUsize::new(0),
            routed: AtomicUsize::new(0),
            astray: AtomicUsize::new(0),
            astray_to: AtomicI32::new(0),
            lost: AtomicBool::new(false),
        }
    }

    fn accepts(&self, pid: i32) -> bool {
        pid > 0 && self.accept.iter().any(|a| a.load(Ordering::SeqCst) == pid)
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<(i64, i32)>> {
        self.held.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The process that got the press of `keycode`, if one did.
    fn holder(&self, keycode: i64) -> Option<i32> {
        self.held().iter().rev().find(|(code, _)| *code == keycode).map(|(_, pid)| *pid)
    }

    /// The tap's decision for one event.
    fn decide(&self, source: i64, user_data: i64, keycode: i64, target: i32) -> Decision {
        if source != self.own_pid || user_data & !RELEASE_BIT != self.nonce {
            return Decision::Pass;
        }
        self.routed.fetch_add(1, Ordering::SeqCst);
        if user_data & RELEASE_BIT == 0 {
            // Once a press was withheld, the chord is over: a later press
            // (queued before the first was routed) is withheld wherever it
            // goes, so the target never gets part of the chord.
            if self.astray.load(Ordering::SeqCst) == 0 && self.accepts(target) {
                self.held().push((keycode, target));
                return Decision::Pass;
            }
            // A latched press bound for the target keeps the first stray
            // process as the one named.
            if !self.accepts(target) {
                self.astray_to.store(target, Ordering::SeqCst);
            }
            self.astray.fetch_add(1, Ordering::SeqCst);
            return Decision::Withhold;
        }
        let holder = {
            let mut held = self.held();
            let at = held.iter().rposition(|(code, _)| *code == keycode);
            at.map(|at| held.remove(at).1)
        };
        match holder {
            Some(pid) if pid == target => Decision::Pass,
            Some(pid) => Decision::GiveTo(pid),
            // Its press never arrived: a release for the target is harmless.
            None if self.accepts(target) => Decision::Pass,
            None => Decision::Withhold,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Pass,
    Withhold,
    /// Withhold, and post the event to this process instead.
    GiveTo(i32),
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
    /// further was pressed.
    GuardUnavailable(String),
    /// The target was not the front process when a press was due.
    NotFront { target: i32, front: Option<i32> },
    /// A press was routed to another process and withheld from it.
    Astray { target: i32, to: i32 },
    /// Posted keys were not all routed within the bound; the front stays
    /// with the target.
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
                "cua could not guard the foreground keys ({why}), so it pressed no further key; \
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
                     was {front}), so cua pressed no further key; nothing reached another app. \
                     Read the window before retrying",
                    app_name(*target)
                )
            }
            Self::Astray { target, to } => write!(
                f,
                "the front moved to {} (pid {to}) while the keys for pid {target} were on their \
                 way; cua withheld every key bound for it, released each pressed key to the app \
                 that got it and pressed nothing more, so the keys did not all arrive. Read the \
                 window before retrying",
                app_name(*to)
            ),
            Self::Unsettled { target, posted, routed } => write!(
                f,
                "only {routed} of {posted} key events for pid {target} were routed within {} ms, \
                 so cua left {} in front for the rest instead of handing the front back; any \
                 still on its way is withheld from every other app. Read the window before \
                 retrying",
                SETTLE_TIMEOUT.as_millis(),
                app_name(*target)
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
        // A value no other software is likely to put in the source user data;
        // its low bit is the release flag.
        let nonce = 0x0C0A_3800_0000_0000 | (NONCES.fetch_add(1, Ordering::SeqCst) << 1);
        let recipient = Arc::new(Recipient {
            pid,
            window_id,
            ledger: Arc::new(Ledger::new(nonce, std::process::id() as i64, pid)),
            tap: Mutex::new(None),
        });
        OPEN.fetch_add(1, Ordering::SeqCst);
        let outer = CURRENT.with(|c| c.borrow_mut().replace(recipient.clone()));
        Self { recipient, outer }
    }

    /// Wait until every key posted in this scope was routed. `Ok` when none
    /// were posted or every press reached the target.
    pub fn settle(&self) -> Result<(), KeyRefusal> {
        let ledger = &self.recipient.ledger;
        let posted = ledger.posted.load(Ordering::SeqCst);
        if posted == 0 {
            return Ok(());
        }
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while ledger.routed.load(Ordering::SeqCst) < posted && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let routed = ledger.routed.load(Ordering::SeqCst);
        // A tap lost meanwhile cannot vouch for what was routed: the target
        // stays in front so a key still queued reaches it.
        if routed < posted || ledger.lost.load(Ordering::SeqCst) {
            return Err(KeyRefusal::Unsettled { target: self.recipient.pid, posted, routed });
        }
        if ledger.astray.load(Ordering::SeqCst) > 0 {
            return Err(KeyRefusal::Astray {
                target: self.recipient.pid,
                to: ledger.astray_to.load(Ordering::SeqCst),
            });
        }
        Ok(())
    }
}

impl Drop for KeyScope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.outer.take());
        OPEN.fetch_sub(1, Ordering::SeqCst);
        let Some(tap) = self.recipient.tap.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        let ledger = self.recipient.ledger.clone();
        let posted = ledger.posted.load(Ordering::SeqCst);
        if ledger.routed.load(Ordering::SeqCst) >= posted {
            tap.close();
            return;
        }
        // A key still on its way stays guarded until it is routed.
        std::thread::spawn(move || {
            let deadline = Instant::now() + LATE_KEY_GUARD;
            while ledger.routed.load(Ordering::SeqCst) < posted && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            tap.close();
        });
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
        match role {
            KeyRole::Press => {
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
                if !self.guarded(tap.as_ref()) {
                    return Err(KeyRefusal::GuardUnavailable("its event tap stopped".into()).into());
                }
                if ledger.posted.load(Ordering::SeqCst) == 0 {
                    if let Some(window) = crate::windows::window_info_by_id(self.window_id) {
                        ledger.accept[1].store(window.pid, Ordering::SeqCst);
                    }
                    self.await_front(FRONT_WAIT)?;
                } else {
                    self.await_front(Duration::ZERO)?;
                }
                event.set_integer_value_field(FIELD_USER_DATA, ledger.nonce);
            }
            KeyRole::Release => {
                if tap.is_none() {
                    // Nothing was pressed under this guard.
                    event.post(CGEventTapLocation::HID);
                    return Ok(());
                }
                event.set_integer_value_field(FIELD_USER_DATA, ledger.nonce | RELEASE_BIT);
                if !self.guarded(tap.as_ref()) {
                    // Without the tap a HID release could reach whichever app
                    // is in front: give it to the process that got the press.
                    let keycode = event.get_integer_value_field(FIELD_KEYCODE);
                    let to = ledger.holder(keycode).unwrap_or(self.pid);
                    // SAFETY: a valid event; posting copies it.
                    unsafe { CGEventPostToPid(to, event.as_ptr().cast()) };
                    return Ok(());
                }
            }
        }
        ledger.posted.fetch_add(1, Ordering::SeqCst);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn guarded(&self, tap: Option<&EventTap>) -> bool {
        !self.ledger.lost.load(Ordering::SeqCst) && tap.is_some_and(EventTap::is_active)
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
        let lost = self.ledger.clone();
        EventTap::start_annotated_session(
            &[KEY_DOWN, KEY_UP, FLAGS_CHANGED],
            move |_, event| {
                // SAFETY: the tap borrows `event` for this callback only.
                let read = |field| unsafe { CGEventGetIntegerValueField(event, field) };
                let decision = ledger.decide(
                    read(FIELD_SOURCE_PID),
                    read(FIELD_USER_DATA),
                    read(FIELD_KEYCODE),
                    read(FIELD_TARGET_PID) as i32,
                );
                match decision {
                    Decision::Pass => TapDecision::Pass,
                    Decision::Withhold => TapDecision::Drop,
                    Decision::GiveTo(pid) => {
                        // SAFETY: as above; posting copies the event.
                        unsafe { CGEventPostToPid(pid, event) };
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

    const PRESS: i64 = 76;
    const RELEASE: i64 = 77;

    fn ledger(target: i32, owner: i32) -> Ledger {
        let l = Ledger::new(PRESS, 500, target);
        l.accept[1].store(owner, Ordering::SeqCst);
        l
    }

    /// Item 38: Cmd down reached Safari, then the user's TextEdit came back
    /// in front and A down, A up and the Cmd release were routed there.
    /// Under the guard TextEdit gets none of them: A was never pressed, so
    /// its release is withheld too, and Cmd's release goes to Safari.
    #[test]
    fn keys_routed_to_another_app_are_withheld_and_releases_go_to_their_press() {
        let l = ledger(716, 716);
        assert_eq!(l.decide(500, PRESS, 55, 716), Decision::Pass, "Cmd down");
        assert_eq!(l.decide(500, PRESS, 0, 918), Decision::Withhold, "A down");
        assert_eq!(l.decide(500, RELEASE, 0, 918), Decision::Withhold, "A up, never pressed");
        assert_eq!(l.decide(500, RELEASE, 55, 918), Decision::GiveTo(716), "Cmd up");
        assert_eq!(l.routed.load(Ordering::SeqCst), 4);
        assert_eq!(l.astray.load(Ordering::SeqCst), 1);
        assert_eq!(l.astray_to.load(Ordering::SeqCst), 918);
        assert!(l.held().is_empty());
    }

    /// Cmd down was withheld (TextEdit in front), then Safari came back
    /// before A down was routed: A must not reach Safari without its Cmd.
    #[test]
    fn after_a_withheld_press_no_later_press_reaches_the_target() {
        let l = ledger(716, 716);
        assert_eq!(l.decide(500, PRESS, 55, 918), Decision::Withhold, "Cmd down");
        assert_eq!(l.decide(500, PRESS, 0, 716), Decision::Withhold, "A down");
        assert_eq!(l.decide(500, RELEASE, 0, 716), Decision::Pass, "A up: harmless at the target");
        assert!(l.held().is_empty());
        assert_eq!(l.astray_to.load(Ordering::SeqCst), 918, "the stray app stays named");
    }

    /// A modifier press routed elsewhere is withheld, not given to the
    /// target: both arrive as flags-changed events, so the tag says which.
    #[test]
    fn a_modifier_press_routed_elsewhere_is_withheld() {
        let l = ledger(716, 716);
        assert_eq!(l.decide(500, PRESS, 55, 918), Decision::Withhold);
        assert_eq!(l.decide(500, RELEASE, 55, 918), Decision::Withhold);
    }

    /// The press went to the window's helper process and the release to the
    /// target: the release belongs to the helper.
    #[test]
    fn a_release_goes_to_the_process_that_got_its_press() {
        let l = ledger(716, 4242);
        assert_eq!(l.decide(500, PRESS, 55, 4242), Decision::Pass);
        assert_eq!(l.decide(500, RELEASE, 55, 716), Decision::GiveTo(4242));
        assert_eq!(l.decide(500, PRESS, 0, 0), Decision::Withhold, "unrouted is not the target");
    }

    /// The tap sees every key in the session: the user's own keys, other
    /// cua calls' pid-routed keys and another scope's keys pass untouched.
    #[test]
    fn only_this_scopes_own_events_are_judged() {
        let l = ledger(716, 716);
        assert_eq!(l.decide(0, 0, 0, 918), Decision::Pass, "the user's key");
        assert_eq!(l.decide(500, 0, 0, 918), Decision::Pass, "an untagged cua key");
        assert_eq!(l.decide(500, PRESS + 2, 0, 918), Decision::Pass, "another scope's key");
        assert_eq!(l.decide(501, PRESS, 0, 918), Decision::Pass, "another process");
        assert_eq!(l.routed.load(Ordering::SeqCst), 0);
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
