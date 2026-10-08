//! The front cua owes back after an inline edit.
//!
//! A foreground action that opens or types into an inline editor (Finder's
//! rename or new-folder name field) leaves its app in front: the app ends
//! the edit when it loses the front. The app that was in front before is
//! remembered here, and gets the front back once a later cua action finds
//! the edit ended. Nothing else is tracked: one lease (only one app can be in
//! front), begun where the front is kept, settled at the end of every native
//! action's outcome watch, cleared by `bring_to_front`.
//!
//! | Event (settled after a cua action) | Lease | Front |
//! |---|---|---|
//! | the edit is still open, the app still in front | kept | stays; the result says when it comes back |
//! | the front or the edit did not read | kept | left alone; the result says it did not read |
//! | the edit ended (commit, cancel, or by itself), the app still in front | cleared | the previous app is brought back |
//! | another app is in front (the user's or the agent's choice) | cleared | left alone |
//! | a mouse press that was not cua's came during the edit, open or not | cleared | left alone (the user is working) |
//! | the previous app quit | cleared | left alone |
//! | `bring_to_front` | cleared | the agent's choice |

use std::sync::Mutex;

use crate::ax::enablement::{process_start_stamp, ProcessStartStamp};
use crate::focus_steal::InputActivity;

struct Lease {
    /// The app left in front for its inline edit (in any of its windows: a
    /// later command may open the next edit in another one), and its
    /// process start time.
    target: i32,
    target_app: Option<ProcessStartStamp>,
    /// The app in front before cua took the front for the edit, and its
    /// process start time (a pid can be reused once that app quits).
    previous: i32,
    previous_app: Option<ProcessStartStamp>,
    /// Its name then, for the result (the pid may name another app later).
    previous_name: String,
    /// Mouse buttons counted when the lease began.
    input: InputActivity,
}

static LEASE: Mutex<Option<Lease>> = Mutex::new(None);

fn lease() -> std::sync::MutexGuard<'static, Option<Lease>> {
    LEASE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The app to give the front back to when `target` is kept in front with
/// `previous` in front just before: `None` when nothing is owed. `current`
/// is the lease already held: (its target, its previous app). The flag says
/// the keep is chained: the target was in front because of that lease.
fn owed(current: Option<(i32, i32)>, target: i32, previous: Option<i32>) -> Option<(i32, bool)> {
    match (previous?, current) {
        // The target is in front because of an earlier keep: the app owed
        // is still the one from before that.
        (previous, Some((held, owed))) if previous == held && held == target => Some((owed, true)),
        // The target was already the user's front app: nothing to hand back.
        (previous, _) if previous == target => None,
        (previous, _) => Some((previous, false)),
    }
}

/// What a foreground action reads just before it acts, in case it keeps
/// its target in front: the front app, that process's start time, and the
/// mouse counters (a press during the action itself counts).
pub(crate) struct Before {
    previous: Option<i32>,
    stamp: Option<ProcessStartStamp>,
    input: InputActivity,
}

impl Before {
    pub(crate) fn now(front: Option<i32>) -> Self {
        Before {
            previous: front,
            stamp: front.and_then(process_start_stamp),
            input: crate::focus_steal::read_input_activity(),
        }
    }
}

/// A foreground action left `target` in front for an inline edit; `before`
/// is what it read just before acting.
pub(crate) fn begin(target: i32, before: Before) {
    let mut held = lease();
    let target_app = process_start_stamp(target);
    // A lease on a process that has since gone (its pid reused) is no chain.
    let current = held
        .as_ref()
        .filter(|lease| same_app(lease.target_app, target_app))
        .map(|lease| (lease.target, lease.previous));
    let Some((previous, chained)) = owed(current, target, before.previous) else {
        return;
    };
    // A chained keep is the same edit session: input since its start counts.
    // Any other keep starts a new one; its app is named only while it is
    // still the process that was in front.
    let (input, previous_app, previous_name) = match held.take() {
        Some(lease) if chained => (lease.input, lease.previous_app, lease.previous_name),
        _ if same_app(before.stamp, process_start_stamp(previous)) => {
            (before.input, before.stamp, name(previous))
        }
        _ => (before.input, None, format!("pid {previous}")),
    };
    *held = Some(Lease {
        target,
        target_app,
        previous,
        previous_app,
        previous_name,
        input,
    });
}

/// The agent (or the user, through the PiP) chose the front app.
pub(crate) fn end() {
    *lease() = None;
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// The edit is open: the lease stays.
    Keep,
    /// The front or the edit did not read: the lease stays, nothing claimed.
    Unread,
    Restore,
    /// Cleared without touching the front, and why.
    Leave(Why),
}

#[derive(Debug, PartialEq, Eq)]
enum Why {
    FrontChanged(i32),
    TargetGone,
    UserInput,
    PreviousGone,
}

/// What to do with the lease after an action. `front`: the front app now;
/// `target_running`: the target is still the process the lease began on;
/// `edit_open`: `None` when it did not read. Another front app or the
/// user's own mouse press ends the lease first: then cua promises nothing.
fn decide(
    target: i32,
    target_running: bool,
    front: Option<i32>,
    user_input: bool,
    previous_running: bool,
    edit_open: Option<bool>,
) -> Step {
    match front {
        None => Step::Unread,
        Some(_) if !target_running => Step::Leave(Why::TargetGone),
        Some(front) if front != target => Step::Leave(Why::FrontChanged(front)),
        Some(_) if user_input => Step::Leave(Why::UserInput),
        Some(_) if !previous_running => Step::Leave(Why::PreviousGone),
        Some(_) => match edit_open {
            Some(true) => Step::Keep,
            None => Step::Unread,
            Some(false) => Step::Restore,
        },
    }
}

fn name(pid: i32) -> String {
    crate::apps::running_app(pid)
        .map(|app| app.name)
        .or_else(|| crate::apps::get_app_name_for_pid(pid))
        .unwrap_or_else(|| format!("pid {pid}"))
}

/// Read everything [`decide`] needs, the front app last: a reading taken
/// before a slow AX read could be stale by the time it is acted on.
fn read_and_decide(lease: &Lease) -> Step {
    let edit = crate::ax::bindings::inline_edit_state(lease.target);
    let user = crate::focus_steal::user_input_since(&lease.input);
    let target_running = same_app(lease.target_app, process_start_stamp(lease.target));
    let running = same_app(lease.previous_app, process_start_stamp(lease.previous));
    decide(
        lease.target,
        target_running,
        crate::apps::frontmost_pid(),
        user,
        running,
        edit,
    )
}

/// Whether the process under the owed pid now is the one that was in front:
/// the same start time, both read (a pid can be reused once that app quits).
fn same_app(owed: Option<ProcessStartStamp>, now: Option<ProcessStartStamp>) -> bool {
    owed.is_some() && owed == now
}

/// Bring `previous` back, deciding once more right before the switch (the
/// first decision took time: the edit may have reopened, the user may have
/// switched or clicked). `Err` carries what that second decision chose
/// instead. The switch is a deliberate activation: the focus guards an
/// action left armed would otherwise take it for a theft and put `target`
/// back. `Ok(false)`: switched but not confirmed in front.
fn hand_back(lease: &Lease) -> Result<bool, Step> {
    crate::window_change_detector::end_lingering_focus_guards();
    let _intentional = crate::focus_steal::allow_intentional_activation(lease.previous);
    let mut again = Step::Restore;
    let switched = crate::input::skylight::restore_front_pid(lease.previous, &mut || {
        again = read_and_decide(lease);
        again == Step::Restore
    });
    if again != Step::Restore {
        return Err(again);
    }
    Ok(switched && crate::apps::confirm_front(lease.previous))
}

/// After a native action: hand the front back when the edit has ended, and
/// say where the front is. `None` when no lease is held. Blocking (AX reads
/// and a short confirm poll).
pub(crate) fn settle() -> Option<String> {
    let mut held = lease();
    let lease = held.as_ref()?;
    let target = lease.target;
    let previous_name = lease.previous_name.clone();
    let mut step = read_and_decide(lease);
    let mut confirmed = false;
    if step == Step::Restore {
        match hand_back(lease) {
            Ok(back) => confirmed = back,
            Err(again) => step = again,
        }
    }
    let target_name = name(target);
    let words = match step {
        Step::Keep => {
            return Some(format!(
                "{target_name} stays in front while its inline edit is open (it ends the edit when it \
                 loses the front); cua brings {previous_name} back when a cua action ends the edit"
            ));
        }
        Step::Unread => {
            return Some(format!(
                "cua could not read whether {target_name}'s inline edit is still open, so the front \
                 was left as it is; cua brings {previous_name} back once a cua action reads the edit ended"
            ));
        }
        Step::Restore if confirmed => {
            format!("the inline edit ended, so cua brought {previous_name} back to the front")
        }
        Step::Restore => {
            let now = crate::apps::frontmost_pid().map_or_else(
                || "the front app did not read".into(),
                |pid| format!("{} is in front", name(pid)),
            );
            format!("the inline edit ended, but {previous_name} did not come back to the front ({now})")
        }
        Step::Leave(Why::FrontChanged(front)) => format!(
            "{} is in front now, so cua did not bring {previous_name} back after {target_name}'s edit",
            name(front)
        ),
        Step::Leave(Why::UserInput) => format!(
            "a mouse press that was not cua's came during {target_name}'s edit, so cua did not bring \
             {previous_name} back; {target_name} stays in front"
        ),
        Step::Leave(Why::TargetGone) => format!(
            "the app cua left in front for its inline edit is no longer running, so cua did not \
             bring {previous_name} back"
        ),
        Step::Leave(Why::PreviousGone) => {
            format!("{previous_name} is no longer running; {target_name} stays in front")
        }
    };
    *held = None;
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINDER: i32 = 10;
    const TERMINAL: i32 = 20;
    const TEXTEDIT: i32 = 30;

    /// Rows rename_commit, rename_cancel, new_folder, closes_by_itself,
    /// other_app_foreground: Finder still in front, the edit has ended.
    #[test]
    fn an_ended_edit_hands_the_front_back() {
        assert_eq!(
            decide(FINDER, true, Some(FINDER), false, true, Some(false)),
            Step::Restore
        );
    }

    /// Rows task_ends_open, other_app_background: the edit is still open.
    #[test]
    fn an_open_edit_keeps_the_front() {
        assert_eq!(
            decide(FINDER, true, Some(FINDER), false, true, Some(true)),
            Step::Keep
        );
    }

    /// Row user_switch: another app is in front, whoever chose it.
    #[test]
    fn a_changed_front_is_left_alone() {
        assert_eq!(
            decide(FINDER, true, Some(TEXTEDIT), false, true, None),
            Step::Leave(Why::FrontChanged(TEXTEDIT))
        );
        assert_eq!(
            decide(FINDER, true, Some(TERMINAL), false, true, None),
            Step::Leave(Why::FrontChanged(TERMINAL))
        );
    }

    /// A mouse press that was not cua's: the user may be working in Finder,
    /// so nothing is promised even while the edit is still open.
    #[test]
    fn user_input_during_the_edit_is_left_alone() {
        assert_eq!(
            decide(FINDER, true, Some(FINDER), true, true, Some(false)),
            Step::Leave(Why::UserInput)
        );
        assert_eq!(
            decide(FINDER, true, Some(FINDER), true, true, Some(true)),
            Step::Leave(Why::UserInput)
        );
    }

    #[test]
    /// Finder quit and its pid now names another process: whatever is in
    /// front is not Finder's, so nothing is handed back.
    #[test]
    fn a_target_that_quit_ends_the_lease() {
        assert_eq!(
            decide(FINDER, false, Some(FINDER), false, true, Some(false)),
            Step::Leave(Why::TargetGone)
        );
    }

    #[test]
    fn a_previous_app_that_quit_is_not_brought_back() {
        assert!(same_app(Some((100, 5)), Some((100, 5))));
        assert!(!same_app(Some((100, 5)), None));
        // Its pid reused by another process; a start time never read.
        assert!(!same_app(Some((100, 5)), Some((200, 7))));
        assert!(!same_app(None, None));
        assert_eq!(
            decide(FINDER, true, Some(FINDER), false, false, Some(false)),
            Step::Leave(Why::PreviousGone)
        );
    }

    /// A read that did not answer is not an ended edit: handing the front
    /// back would end an edit that may still be open.
    #[test]
    fn an_unread_front_or_edit_keeps_the_lease() {
        assert_eq!(decide(FINDER, true, None, false, true, None), Step::Unread);
        assert_eq!(
            decide(FINDER, true, Some(FINDER), false, true, None),
            Step::Unread
        );
    }

    /// Row finder_was_front: nothing is owed when Finder was the user's
    /// front app; a keep while Finder is front because of an earlier keep
    /// owes the app from before that; a new keep from another app owes that app.
    #[test]
    fn only_the_app_from_before_the_edit_is_owed() {
        assert_eq!(owed(None, FINDER, Some(FINDER)), None);
        assert_eq!(owed(None, FINDER, None), None);
        assert_eq!(owed(None, FINDER, Some(TERMINAL)), Some((TERMINAL, false)));
        assert_eq!(
            owed(Some((FINDER, TERMINAL)), FINDER, Some(FINDER)),
            Some((TERMINAL, true))
        );
        assert_eq!(
            owed(Some((FINDER, TERMINAL)), FINDER, Some(TEXTEDIT)),
            Some((TEXTEDIT, false))
        );
        // The user went back to Terminal and cua opened a new edit before the
        // old lease settled: a new session (fresh input baseline), not a chain.
        assert_eq!(
            owed(Some((FINDER, TERMINAL)), FINDER, Some(TERMINAL)),
            Some((TERMINAL, false))
        );
    }
}
