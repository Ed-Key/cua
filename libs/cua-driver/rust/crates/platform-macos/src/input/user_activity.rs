//! Foreground input waits while the user is typing (item 42).
//!
//! A foreground action brings its target to the front. Keys the user types
//! meanwhile go to whatever is in front, so they land in the agent's target
//! (in item 42 the user's own words showed up in a Slack reply box). Before a
//! foreground action activates anything, it reads how long ago the last
//! hardware key-down was (`CGEventSourceSecondsSinceLastEventType` on the HID
//! system state; it needs no permission). When that is under [`QUIET`], it
//! waits up to [`WAIT`] for [`QUIET`] without typing, then refuses with
//! [`super::mouse::ForegroundBusy::UserActive`]: nothing activated or sent.
//!
//! cua's own HID key posts update the same clock, so each one is noted
//! ([`note_own_hid_key`]); a key-down whose age matches the last own post is
//! taken as cua's. A user key within [`OWN_MATCH`] of an own post is missed.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long without a key-down counts as the user not typing.
pub(crate) const QUIET: Duration = Duration::from_secs(2);
/// How long a foreground action waits for that before refusing.
pub(crate) const WAIT: Duration = Duration::from_secs(4);
const POLL: Duration = Duration::from_millis(100);
/// How close a key-down's age must be to cua's last own HID key post to be
/// taken as that post.
const OWN_MATCH: Duration = Duration::from_millis(60);

const HID_SYSTEM_STATE: i32 = 1;
const KEY_DOWN: u32 = 10;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

static OWN_KEY: Mutex<Option<Instant>> = Mutex::new(None);

/// Record that cua posted a key on the HID queue just now.
pub(crate) fn note_own_hid_key() {
    *OWN_KEY.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
}

/// How long ago the user last pressed a key, when it is known and was not
/// one of cua's own posts.
fn user_typed_ago() -> Option<Duration> {
    let since = unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, KEY_DOWN) };
    let own = *OWN_KEY.lock().unwrap_or_else(|p| p.into_inner());
    user_key_age(since, own.map(|at| at.elapsed()))
}

/// `since`: seconds since the last HID key-down; `own_ago`: since cua's last
/// own HID key post.
fn user_key_age(since: f64, own_ago: Option<Duration>) -> Option<Duration> {
    if !since.is_finite() || since < 0.0 {
        return None;
    }
    let since = Duration::from_secs_f64(since);
    match own_ago {
        Some(own) if own.abs_diff(since) <= OWN_MATCH => None,
        _ => Some(since),
    }
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
    fn a_key_down_matching_cuas_own_post_is_not_the_users() {
        let own = Some(Duration::from_millis(500));
        assert_eq!(user_key_age(0.52, own), None);
        assert_eq!(user_key_age(0.2, own), Some(Duration::from_millis(200)));
        assert_eq!(user_key_age(0.2, None), Some(Duration::from_millis(200)));
        assert_eq!(user_key_age(f64::NAN, None), None);
    }
}
