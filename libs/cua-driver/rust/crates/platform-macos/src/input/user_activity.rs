//! Foreground input waits while the user is typing (item 42).
//!
//! A foreground action brings its target to the front. Keys the user types
//! meanwhile go to whatever is in front, so they land in the agent's target
//! (in item 42 the user's own words showed up in a Slack reply box). Before a
//! foreground action activates anything, it asks when the user last pressed
//! a key. Within [`QUIET`] it waits up to [`WAIT`] for [`QUIET`] without a
//! key press, then refuses with [`super::mouse::ForegroundBusy::UserActive`]:
//! nothing activated or sent.
//!
//! The source is the HID system key-down counter, sampled every [`SAMPLE`] by
//! one thread started with the first foreground admission (a press before
//! then is not seen). An idle timer does not work: in a VM the virtual
//! keyboard resets the key-down idle time continuously while no event is
//! counted (`crate::focus_steal`). The counter includes key-downs cua posts on
//! the HID queue itself, so each one is noted ([`note_own_key_post`]) and
//! credited against the next counted presses; only presses beyond that credit
//! are the user's. Credit not matched within [`CREDIT_LIFE`] lapses.
//!
//! This is a check before admission only: keys the user starts typing after
//! the target came to the front still reach it (the foreground key scope's
//! tap passes untagged events). Protecting the whole foreground interval is
//! a separate item.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

/// How long without a key press counts as the user not typing.
pub(crate) const QUIET: Duration = Duration::from_secs(2);
/// How long a foreground action waits for that before refusing.
pub(crate) const WAIT: Duration = Duration::from_secs(4);
const POLL: Duration = Duration::from_millis(100);
const SAMPLE: Duration = Duration::from_millis(50);
const CREDIT_LIFE: Duration = Duration::from_secs(1);

const HID_SYSTEM_STATE: i32 = 1;
const KEY_DOWN: u32 = 10;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceCounterForEventType(state: i32, event_type: u32) -> u32;
}

static OWN_KEY_DOWNS: AtomicU32 = AtomicU32::new(0);
static LAST_USER_KEY: Mutex<Option<Instant>> = Mutex::new(None);
static SAMPLER: Once = Once::new();

/// Note a key event cua is about to post on the HID queue (key-downs only).
pub(crate) fn note_own_key_post(event: &core_graphics::event::CGEvent) {
    if matches!(event.get_type(), core_graphics::event::CGEventType::KeyDown) {
        OWN_KEY_DOWNS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counted key-downs since the last sample that were not cua's own.
#[derive(Debug, Default)]
struct Credit {
    system: u32,
    own: u32,
    credit: u32,
    credited_at: Option<Instant>,
}

impl Credit {
    /// The user's presses among the counts read now.
    fn user_presses(&mut self, system: u32, own: u32, now: Instant) -> u32 {
        let new_system = system.wrapping_sub(self.system);
        let new_own = own.wrapping_sub(self.own);
        self.system = system;
        self.own = own;
        if new_own > 0 {
            self.credit += new_own;
            self.credited_at = Some(now);
        } else if self.credited_at.is_some_and(|at| now.duration_since(at) > CREDIT_LIFE) {
            self.credit = 0;
        }
        let matched = new_system.min(self.credit);
        self.credit -= matched;
        new_system - matched
    }
}

fn ensure_sampler() {
    SAMPLER.call_once(|| {
        let _ = std::thread::Builder::new().name("cua-user-keys".into()).spawn(|| {
            let read = || unsafe { CGEventSourceCounterForEventType(HID_SYSTEM_STATE, KEY_DOWN) };
            let mut credit = Credit { system: read(), own: OWN_KEY_DOWNS.load(Ordering::SeqCst), ..Credit::default() };
            loop {
                std::thread::sleep(SAMPLE);
                let now = Instant::now();
                if credit.user_presses(read(), OWN_KEY_DOWNS.load(Ordering::SeqCst), now) > 0 {
                    *LAST_USER_KEY.lock().unwrap_or_else(|p| p.into_inner()) = Some(now);
                }
            }
        });
    });
}

fn user_typed_ago() -> Option<Duration> {
    LAST_USER_KEY.lock().unwrap_or_else(|p| p.into_inner()).map(|at| at.elapsed())
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Go,
    Wait,
    Refuse,
}

fn step(typed_ago: Option<Duration>, waited: Duration) -> Step {
    match typed_ago {
        Some(ago) if ago < QUIET => {
            if waited >= WAIT {
                Step::Refuse
            } else {
                Step::Wait
            }
        }
        _ => Step::Go,
    }
}

/// Wait until the user has not typed for [`QUIET`], or refuse after [`WAIT`].
/// Returns at once when the user is idle.
pub(crate) fn wait_until_user_idle() -> Result<(), super::mouse::ForegroundBusy> {
    ensure_sampler();
    let started = Instant::now();
    loop {
        let ago = user_typed_ago();
        match step(ago, started.elapsed()) {
            Step::Go => return Ok(()),
            Step::Wait => std::thread::sleep(POLL),
            Step::Refuse => {
                return Err(super::mouse::ForegroundBusy::UserActive {
                    typed_ago: ago.unwrap_or_default(),
                    waited: started.elapsed(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_or_unknown_goes_at_once_and_recent_typing_waits_then_refuses() {
        assert_eq!(step(None, Duration::ZERO), Step::Go);
        assert_eq!(step(Some(Duration::from_secs(3)), Duration::ZERO), Step::Go);
        assert_eq!(step(Some(Duration::from_millis(300)), Duration::ZERO), Step::Wait);
        assert_eq!(step(Some(Duration::from_millis(300)), WAIT), Step::Refuse);
    }

    #[test]
    fn cuas_own_key_downs_are_credited_even_when_counted_late() {
        let t0 = Instant::now();
        let mut c = Credit { system: 100, own: 7, ..Credit::default() };
        assert_eq!(c.user_presses(100, 8, t0), 0, "cua noted its post, not yet counted");
        assert_eq!(c.user_presses(101, 8, t0 + SAMPLE), 0, "counted a sample later: still cua's");
        assert_eq!(c.user_presses(102, 8, t0 + SAMPLE * 2), 1, "a press beyond the credit is the user's");
        assert_eq!(c.user_presses(103, 9, t0 + SAMPLE * 3), 0);
        // Credit for a post that never got counted lapses.
        assert_eq!(c.user_presses(103, 10, t0 + SAMPLE * 4), 0);
        assert_eq!(c.user_presses(104, 10, t0 + SAMPLE * 4 + CREDIT_LIFE * 2), 1);
    }
}
