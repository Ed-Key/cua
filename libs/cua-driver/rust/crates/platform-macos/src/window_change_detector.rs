//! Post-action focus protection and foreground-change reporting.
//!
//! Action tools (click, type_text, hotkey, ...) on a backgrounded app can make
//! another app activate: a link hands off to Safari, a helper app pops up.
//! `snapshot()` arms a **wildcard** focus-steal suppression lease before the
//! action. `detect()` returns immediately after the action: it reports a
//! foreground change it can already see, and hands the lease to a background
//! timer that keeps protecting focus until the observation bound elapses or
//! the next action ends it (`end_lingering_focus_guards`). While lingering it
//! yields to real user input, so the user's own app switch is never undone.
//! The tool result never waits for that window.
//!
//! New windows are not detected here any more. Waiting to prove that no window
//! will open cost every action its full timeout; `surface_observer` instead
//! reports new windows on the agent's next read.
//!
//! ```ignore
//! let prior_front = apps::frontmost_pid();
//! let snapshot = WindowChangeDetector::snapshot(prior_front);
//! // ... perform action ...
//! let changes = snapshot.detect();
//! // changes.result_suffix() -- append to ToolResult text.
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use cua_driver_core::window_observation::WindowObservationBounds;

use crate::apps;
use crate::focus_steal::{self, SuppressionLease};

/// State captured immediately before the action fires: the caller's
/// frontmost pid and the wildcard suppression lease
/// (`None` when suppression was not requested or nothing was frontmost).
pub struct Snapshot {
    front_pid: Option<i32>,
    _lease: Option<SuppressionLease>,
}

/// Result of `detect()`: a foreground change already visible when the
/// action returned.
#[derive(Debug, Clone)]
pub struct Changes {
    pub foreground_changed: bool,
}

impl Changes {
    pub fn no_change() -> Self {
        Self {
            foreground_changed: false,
        }
    }

    /// One-liner summary to append to a tool result, or empty string
    /// when nothing interesting happened.
    pub fn result_suffix(&self) -> String {
        if self.foreground_changed {
            "\n\n🔀 Action caused a different app to become frontmost.".to_string()
        } else {
            String::new()
        }
    }
}

/// Leases handed to background timers, ended early by the next action.
static LINGERING: LazyLock<Mutex<Vec<(u64, Box<dyn Send>)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static NEXT_LINGER_ID: AtomicU64 = AtomicU64::new(0);

fn lingering() -> std::sync::MutexGuard<'static, Vec<(u64, Box<dyn Send>)>> {
    LINGERING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keep `guard` alive until `until` without blocking the caller.
fn linger(guard: Box<dyn Send>, until: Instant) {
    let remaining = until.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return;
    }
    let id = NEXT_LINGER_ID.fetch_add(1, Ordering::Relaxed);
    lingering().push((id, guard));
    std::thread::spawn(move || {
        std::thread::sleep(remaining);
        let expired = {
            let mut guards = lingering();
            guards
                .iter()
                .position(|(held, _)| *held == id)
                .map(|index| guards.swap_remove(index))
        };
        // Dropped outside the lock: ending a lease can call into focus_steal.
        drop(expired);
    });
}

/// End every lingering post-action focus guard. Called before the next action
/// so a guard from the previous one cannot fight an intentional activation.
pub(crate) fn end_lingering_focus_guards() {
    let ended: Vec<_> = lingering().drain(..).collect();
    drop(ended);
}

/// How long the wildcard focus guard keeps protecting after an action
/// starts. It runs in the background; results never wait for it.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Poll setting kept for the shared host bounds; nothing polls here now.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(test)]
/// Resolve the post-action observation bounds from raw host values against
/// the macOS defaults. Pure; `cua_driver_core::window_observation` owns the
/// parsing and clamping rules shared with the Linux adapter.
fn observation_bounds_from(
    timeout_raw: Option<&str>,
    poll_raw: Option<&str>,
) -> WindowObservationBounds {
    WindowObservationBounds::from_raw(
        timeout_raw,
        poll_raw,
        DEFAULT_TIMEOUT,
        DEFAULT_POLL_INTERVAL,
    )
}

/// Post-action observation bounds chosen by the embedding host through
/// `CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS` / `CUA_DRIVER_WINDOW_CHANGE_POLL_MS`
/// on the daemon environment; `DEFAULT_TIMEOUT` / `DEFAULT_POLL_INTERVAL`
/// when unset or unparsable.
pub(crate) fn host_observation_bounds() -> WindowObservationBounds {
    WindowObservationBounds::from_env(DEFAULT_TIMEOUT, DEFAULT_POLL_INTERVAL)
}

/// Public API. Mirrors Swift `enum WindowChangeDetector` — no state of
/// its own; all state lives inside the returned `Snapshot`.
pub struct WindowChangeDetector;

impl WindowChangeDetector {
    /// Capture the current window set + frontmost pid and arm the
    /// wildcard focus-steal suppressor. Call immediately before
    /// dispatching the action.
    ///
    /// `prior_front` is the frontmost pid the **caller** already
    /// observed — typically captured one line earlier via
    /// `apps::frontmost_pid()` for the surrounding `focus_guard`
    /// lease. We use the caller's value (not a fresh re-read) so the
    /// wildcard suppressor's `restore_to` matches what the focus-guard
    /// lease saw; a race where another app became frontmost between
    /// the caller's read and this method would otherwise leave the
    /// two leases targeting different pids.
    ///
    /// Returns `Snapshot`. Drop ends suppression (via the held
    /// `SuppressionLease`); call `Snapshot::detect()` to consume the
    /// snapshot and get a `Changes` summary.
    ///
    /// Safe to call from any thread — `CGWindowListCopyWindowInfo` is
    /// documented as thread-safe.
    pub fn snapshot(prior_front: Option<i32>) -> Snapshot {
        Self::capture(prior_front, true, None)
    }

    /// Capture the same before-state without arming reactive focus suppression.
    /// Foreground delivery owns its temporary activation and restoration, so a
    /// wildcard lease would race the target while the action is settling.
    pub fn snapshot_without_suppression(prior_front: Option<i32>) -> Snapshot {
        Self::capture(prior_front, false, None)
    }

    /// Capture the before-state and suppress cross-app activations while
    /// allowing one intentional target activation.
    ///
    /// The raw background pixel-click path needs this middle ground:
    /// focus-without-raise makes `allowed_pid` AppKit-active so its event queue
    /// accepts the click, but a link or hand-off that activates a different app
    /// must still restore the user's original foreground.
    pub fn snapshot_allowing_activation(prior_front: Option<i32>, allowed_pid: i32) -> Snapshot {
        Self::capture(prior_front, true, Some(allowed_pid))
    }

    fn capture(
        prior_front: Option<i32>,
        suppress_focus: bool,
        allowed_pid: Option<i32>,
    ) -> Snapshot {
        // Arm wildcard suppression — covers snapshot → detect window.
        // restore_to = caller-captured frontmost; target = wildcard
        // (any other pid). If there's no frontmost (rare — screensaver,
        // login window), we skip the lease; foreground-change tracking
        // still runs.
        let lease = prior_front
            .filter(|_| suppress_focus)
            .map(|restore_to| match allowed_pid {
                Some(pid) => focus_steal::begin_suppression_allowing(
                    pid,
                    restore_to,
                    "WindowChangeDetector.snapshot_allowing_activation",
                ),
                None => focus_steal::begin_suppression(
                    None, // wildcard
                    restore_to,
                    "WindowChangeDetector.snapshot",
                ),
            });

        Snapshot {
            front_pid: prior_front,
            _lease: lease,
        }
    }
}

impl Snapshot {
    /// Frontmost pid at snapshot time, if any.
    pub fn front_pid(&self) -> Option<i32> {
        self.front_pid
    }

    /// Report what is already visible and return at once. The suppression
    /// lease keeps protecting focus in the background until the host
    /// observation bound (`CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS`, default
    /// 1000ms after this call) elapses or the next action ends it. From
    /// here on it lets through activations that follow real user input.
    pub fn detect(self) -> Changes {
        self.detect_bounded(host_observation_bounds())
    }

    /// `detect()` with explicit bounds. A zero timeout ends the lease now.
    pub(crate) fn detect_bounded(mut self, bounds: WindowObservationBounds) -> Changes {
        let foreground_changed = matches!(
            (self.front_pid, apps::frontmost_pid()),
            (Some(before), Some(now)) if before != now
        );
        if let Some(lease) = self._lease.take() {
            if !bounds.skips_observation() {
                // Timed from the result, not the snapshot, so a long action
                // keeps the full protection window after it finishes.
                let until = Instant::now() + bounds.timeout;
                linger(Box::new(lease.linger_until(until)), until);
            }
        }
        Changes { foreground_changed }
    }

    /// Kept async for existing call sites; it no longer waits.
    pub async fn detect_async(self) -> Changes {
        self.detect()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_result_suffix_no_change_is_empty() {
        let c = Changes::no_change();
        assert_eq!(c.result_suffix(), "");
    }

    #[test]
    fn changes_result_suffix_foreground_change_only() {
        let c = Changes {
            foreground_changed: true,
        };
        assert_eq!(
            c.result_suffix(),
            "\n\n🔀 Action caused a different app to become frontmost."
        );
    }

    #[test]
    fn host_bounds_unset_keep_macos_defaults() {
        let b = observation_bounds_from(None, None);
        assert_eq!(b.timeout, DEFAULT_TIMEOUT);
        assert_eq!(b.poll, DEFAULT_POLL_INTERVAL);
        assert!(!b.skips_observation());
    }

    #[test]
    fn host_bounds_valid_values_are_honored() {
        let b = observation_bounds_from(Some("200"), Some("20"));
        assert_eq!(b.timeout, Duration::from_millis(200));
        assert_eq!(b.poll, Duration::from_millis(20));
    }

    #[test]
    fn host_bounds_zero_timeout_skips_observation() {
        let b = observation_bounds_from(Some("0"), None);
        assert!(b.skips_observation());
        assert!(!b.poll.is_zero());
    }

    #[test]
    fn host_bounds_invalid_values_keep_macos_defaults() {
        for raw in ["", "abc", "-5", "2.5", "99999999999999999999"] {
            let b = observation_bounds_from(Some(raw), Some(raw));
            assert_eq!(b.timeout, DEFAULT_TIMEOUT, "timeout for {raw:?}");
            assert_eq!(b.poll, DEFAULT_POLL_INTERVAL, "poll for {raw:?}");
        }
    }

    #[test]
    fn host_bounds_too_large_values_are_clamped() {
        use cua_driver_core::window_observation::{
            MAX_WINDOW_CHANGE_POLL, MAX_WINDOW_CHANGE_TIMEOUT,
        };
        let b = observation_bounds_from(Some("3600000"), Some("60000"));
        assert_eq!(b.timeout, MAX_WINDOW_CHANGE_TIMEOUT);
        assert_eq!(b.poll, MAX_WINDOW_CHANGE_POLL);
    }

    /// A zero timeout returns immediately with no change instead of
    /// sleeping a poll interval. Reads the window list once (in
    /// `snapshot`) and sends no input.
    #[test]
    fn zero_timeout_detect_returns_without_polling() {
        let snap = WindowChangeDetector::snapshot(None);
        let started = Instant::now();
        let changes = snap.detect_bounded(observation_bounds_from(Some("0"), None));
        assert!(started.elapsed() < DEFAULT_POLL_INTERVAL);
        assert_eq!(changes.result_suffix(), "");
    }

    /// Regression: `snapshot(prior_front)` must store the caller's
    /// captured front pid verbatim (rather than re-reading it inside
    /// the function and racing with concurrent activations).
    #[test]
    fn snapshot_stores_caller_prior_front() {
        // Use an obviously bogus pid so we'd notice if the impl silently
        // fell back to the live frontmost on this test runner.
        let bogus_prior = Some(424242_i32);
        let snap = WindowChangeDetector::snapshot(bogus_prior);
        assert_eq!(snap.front_pid(), bogus_prior);

        // None must round-trip too — and must skip the lease without
        // panicking (no frontmost to restore to).
        let snap_none = WindowChangeDetector::snapshot(None);
        assert_eq!(snap_none.front_pid(), None);
    }

    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn flag() -> (DropFlag, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (DropFlag(dropped.clone()), dropped)
    }

    /// The guard outlives the call that armed it, then ends on its own.
    /// One test: the lingering list is process-global.
    #[test]
    fn lingering_guard_expires_or_ends_at_the_next_action() {
        let (guard, dropped) = flag();
        linger(Box::new(guard), Instant::now() + Duration::from_millis(80));
        assert!(!dropped.load(Ordering::SeqCst), "must not end before its bound");
        std::thread::sleep(Duration::from_millis(300));
        assert!(dropped.load(Ordering::SeqCst), "must end once its bound elapses");

        let (guard, dropped) = flag();
        linger(Box::new(guard), Instant::now() + Duration::from_secs(30));
        end_lingering_focus_guards();
        assert!(dropped.load(Ordering::SeqCst), "the next action must end it early");

        let (guard, dropped) = flag();
        linger(Box::new(guard), Instant::now());
        assert!(dropped.load(Ordering::SeqCst), "an elapsed bound ends it at once");
    }

    /// detect() must not wait for the observation bound.
    #[test]
    fn detect_returns_before_the_observation_bound() {
        let snap = WindowChangeDetector::snapshot(None);
        let started = Instant::now();
        snap.detect_bounded(observation_bounds_from(Some("1000"), None));
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
