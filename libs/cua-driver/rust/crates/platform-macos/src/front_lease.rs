//! The front cua owes back after an inline edit or a panel step.
//!
//! A foreground action that opens or types into an inline editor (Finder's
//! rename or new-folder name field) leaves its app in front: the app ends
//! the edit when it loses the front. A cua action can also bring an app with
//! an Open/Save panel or a file chooser forward without meaning to (a
//! desktop click on the panel activates its app); the app stays in front
//! while the panel is open, so the screen stays as the agent saw it. The app
//! that was in front before is remembered here, and gets the front back once
//! a later cua action finds the edit ended or the panel closed. Nothing else
//! is tracked: one lease (only one app can be in front), begun where the
//! front is kept or found moved, settled at the end of every native action,
//! ended by `bring_to_front` as the table says.
//!
//! | Event (settled after a cua action) | Lease | Front |
//! |---|---|---|
//! | the edit or panel is still open, the app still in front | kept | stays; the result says when it comes back |
//! | the front, the edit or the panel did not read | kept | left alone; the result says it did not read |
//! | the edit ended (commit, cancel, or by itself) or the panel closed, the app still in front | cleared | the previous app is brought back |
//! | the previous app is in front again | cleared | left alone, nothing said |
//! | another app is in front (the user's or the agent's choice) | cleared | left alone |
//! | a mouse press that was not cua's came during the edit, open or not | cleared | left alone (the user is working) |
//! | the previous app quit | cleared | left alone |
//! | the agent's `bring_to_front` moved the front, or chose the app itself | cleared | the agent's choice; its result names the app owed |
//! | the PiP Focus button | cleared | the user's choice |
//!
//! The agent's own `bring_to_front` is not handed back by cua (it cannot
//! tell when the agent's step that needed the front is over). Its result
//! names the app the front was taken from, carried through the agent's
//! later `bring_to_front` calls while the app it brought stays in front
//! (`Taking`).

use std::sync::Mutex;

use crate::ax::bindings::Panel;
use crate::ax::enablement::{process_start_stamp, ProcessStartStamp};
use crate::focus_steal::InputActivity;

/// What keeps the app in front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    /// An inline edit cua kept the front for.
    Edit,
    /// A panel shown when a cua action brought its app forward, and the
    /// window that holds it (another window's panel is another step).
    Panel(Panel, u32),
}

#[derive(Clone)]
struct Lease {
    hold: Hold,
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
        // The app in front was there because of an earlier keep (this edit's
        // or another app's): the app owed is still the one from before that.
        (previous, Some((held, owed))) if previous == held && owed != target => Some((owed, true)),
        // The app it owed is the one coming forward: nothing is owed now.
        (previous, Some((held, _))) if previous == held => None,
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
    /// The lease that stood then: another call's settle may clear it while
    /// this action runs, and a lease begun after it still owes its app.
    standing: Option<Lease>,
    /// The count of explicit front choices then (see [`CHOICES`]).
    choices: u64,
}

/// Explicit front choices: the agent's `bring_to_front` and the PiP Focus
/// button. A front that moved during an action while one was made is that
/// choice, not the action's doing.
static CHOICES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn chose() {
    CHOICES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

impl Before {
    pub(crate) fn now(front: Option<i32>) -> Self {
        let held = lease();
        Self::under(&held, front)
    }

    /// The front app read under the lease lock, with everything else: a
    /// settle that holds the lock (and may hand the front back) finishes
    /// first, so the front and the standing lease agree.
    pub(crate) fn read() -> Self {
        let held = lease();
        Self::under(&held, crate::apps::frontmost_pid())
    }

    fn under(held: &Option<Lease>, front: Option<i32>) -> Self {
        Before {
            previous: front,
            stamp: front.and_then(process_start_stamp),
            input: crate::focus_steal::read_input_activity(),
            standing: held.as_ref().filter(|lease| stands(lease)).cloned(),
            choices: CHOICES.load(std::sync::atomic::Ordering::SeqCst),
        }
    }
}

/// A foreground action left `target` in front for an inline edit; `before`
/// is what it read just before acting.
pub(crate) fn begin(target: i32, before: Before) {
    start(&mut lease(), target, before, Hold::Edit);
}

/// Begin (or chain) a lease on `target`; false when nothing is owed.
fn start(held: &mut Option<Lease>, target: i32, before: Before, hold: Hold) -> bool {
    start_with(held, target, before, hold, stands)
}

/// [`start`] with the test of the held lease injected (it reads live
/// process and mouse state).
fn start_with(
    held: &mut Option<Lease>,
    target: i32,
    before: Before,
    hold: Hold,
    stands: impl Fn(&Lease) -> bool,
) -> bool {
    let target_app = process_start_stamp(target);
    // A lease on a process that has since gone (its pid reused), or one a
    // mouse press that was not cua's has ended (the user worked there), is
    // no chain.
    // The lease held now, or the one that stood when the action began (a
    // concurrent settle may have cleared it since).
    // The one whose app was in front before this action first: another
    // app's lease begun meanwhile does not hide it.
    let candidates = [held.as_ref(), before.standing.as_ref()];
    let standing = || candidates.iter().flatten().filter(|lease| stands(lease));
    let source = standing()
        .find(|lease| Some(lease.target) == before.previous)
        .or_else(|| standing().next())
        .map(|lease| (*lease).clone());
    let current = source.as_ref().map(|lease| (lease.target, lease.previous));
    let Some((previous, chained)) = owed(current, target, before.previous) else {
        return false;
    };
    // A chained keep is the same edit session: input since its start counts.
    // Any other keep starts a new one; its app is named only while it is
    // still the process that was in front.
    let (input, previous_app, previous_name) = match source {
        Some(lease) if chained => (lease.input, lease.previous_app, lease.previous_name),
        _ if same_app(before.stamp, process_start_stamp(previous)) => {
            (before.input, before.stamp, name(previous))
        }
        _ => (before.input, None, format!("pid {previous}")),
    };
    *held = Some(Lease {
        hold,
        target,
        target_app,
        previous,
        previous_app,
        previous_name,
        input,
    });
    true
}

/// How long the front must stay moved before a panel lease begins: a focus
/// guard the action armed may still be putting the previous app back.
const MOVED_CONFIRM: std::time::Duration = std::time::Duration::from_millis(100);

/// How long an action without a window (a desktop-scope click) waits for the
/// panel or edit it may have closed, before it settles.
// ponytail: fixed bound; an outcome watch for desktop clicks would settle on change instead.
pub(crate) const WINDOWLESS_SETTLE: std::time::Duration = std::time::Duration::from_millis(600);

/// After any native action, errors and desktop-scope clicks included. When
/// it brought an app showing a panel to the front, a lease begins owing the
/// app in front before. `settle_here`: the action has no outcome watch to
/// settle in (no window, or it failed), so it settles here once the lease's
/// panel or edit closed, waiting at most that long. The words for the
/// result, if any. Blocking.
pub(crate) fn after_action(
    before: Before,
    settle_here: Option<std::time::Duration>,
) -> Option<String> {
    if let Some(words) = panel_front(before) {
        return settle_here.and(Some(words));
    }
    let wait = settle_here?;
    let (hold, target) = lease().as_ref().map(|lease| (lease.hold, lease.target))?;
    let deadline = std::time::Instant::now() + wait;
    while open_now(hold, target) == Some(true) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    settle()
}

/// Whether an action that ended with `front` in front begins a panel lease:
/// the front moved off the app read before (`previous`), no lease already
/// holds that app, no mouse press that was not cua's came (the user chose
/// it), and the app shows a panel (it read). The panel and its window, if
/// so: the one in the app's focused window (`focused`) first, and a file
/// panel before another sheet.
fn begins(
    previous: Option<i32>,
    front: i32,
    held: Option<i32>,
    user_input: bool,
    panels: Option<Vec<(Panel, u32)>>,
    focused: Option<u32>,
) -> Option<(Panel, u32)> {
    if previous? == front || held == Some(front) || user_input {
        return None;
    }
    let rank = |(panel, window): &(Panel, u32)| (Some(*window) != focused, *panel == Panel::Sheet);
    panels?.into_iter().min_by_key(rank)
}

fn panel_front(before: Before) -> Option<String> {
    let moved = |front: Option<i32>| front.filter(|front| Some(*front) != before.previous);
    let front = moved(crate::apps::frontmost_pid())?;
    // The lease first, held to the end: another call's settle must not clear
    // or hand back the lease this one may chain from while it reads.
    let mut held = lease();
    if CHOICES.load(std::sync::atomic::Ordering::SeqCst) != before.choices {
        return None;
    }
    std::thread::sleep(MOVED_CONFIRM);
    let front = moved(crate::apps::frontmost_pid()).filter(|again| *again == front)?;
    let user = crate::focus_steal::user_input_since(&before.input);
    let panels = crate::ax::bindings::panel_state(front).filter(|panels| !panels.is_empty())?;
    // With several panels, the one in the window the action left focused.
    let several = panels.len() > 1;
    let focused = several
        .then(|| crate::ax::bindings::focused_window_bounded(front))
        .flatten()
        .and_then(|focused| {
            panels
                .iter()
                .map(|(_, window)| *window)
                .find(|window| crate::ax::bindings::window_belongs_to(focused, *window))
        });
    // A lease the user ended (or whose app quit) holds nothing any more.
    let held_target = held
        .as_ref()
        .filter(|lease| stands(lease))
        .map(|lease| lease.target);
    let (panel, window) = begins(
        before.previous,
        front,
        held_target,
        user,
        Some(panels),
        focused,
    )?;
    if !start(&mut held, front, before, Hold::Panel(panel, window)) {
        return None;
    }
    let previous = held.as_ref()?.previous_name.clone();
    let target = name(front);
    let words = format!(
        "{target} came to the front with this action and stays there while its {} is open; cua \
         brings {previous} back when a cua action closes it",
        panel.name()
    );
    drop(held);
    Some(match chooser_hint(front) {
        Some(hint) => format!("{words}; {hint}"),
        None => words,
    })
}

/// The route that needs no chooser, when `pid` is a Chromium browser the
/// browser tools drive, showing an Open panel (a page's file chooser).
pub(crate) fn chooser_hint(pid: i32) -> Option<&'static str> {
    use cua_driver_core::browser::types::BrowserProduct as P;
    let app = crate::apps::running_app(pid)?;
    let browser = matches!(
        crate::browser::platform::browser_product(
            &app.name,
            app.bundle_id.as_deref().unwrap_or("")
        ),
        P::GoogleChrome
            | P::Chromium
            | P::MicrosoftEdge
            | P::Brave
            | P::Vivaldi
            | P::Opera
            | P::Arc
    );
    if !browser {
        return None;
    }
    hint_for(browser, &crate::ax::bindings::panel_state(pid)?)
}

fn hint_for(browser: bool, panels: &[(Panel, u32)]) -> Option<&'static str> {
    (browser && panels.iter().any(|(panel, _)| *panel == Panel::Open)).then_some(
        "if a page's file input opened this Open panel (an upload), browser_set_input_files sets \
         that input from behind with no chooser (press this panel's Cancel first)",
    )
}

/// Whether a held lease still stands for a new one to chain from or defer
/// to: its app is the process it began on, and no mouse press that was not
/// cua's came since (the user worked there), and the app it owes still runs.
fn stands(lease: &Lease) -> bool {
    same_app(lease.target_app, process_start_stamp(lease.target))
        && same_app(lease.previous_app, process_start_stamp(lease.previous))
        && !crate::focus_steal::user_input_since(&lease.input)
}

/// The user chose the front app through the PiP: nothing is owed to anyone.
pub(crate) fn end() {
    chose();
    *lease() = None;
    *taken() = None;
}

/// The front the agent's own `bring_to_front` took: the app it brought and
/// the app it took the front from, carried to the agent's next
/// `bring_to_front` while that app is still in front. cua does not give it
/// back itself (it cannot tell when the agent's step is over); the result
/// names it so the agent can.
struct Taken {
    brought: i32,
    brought_app: Option<ProcessStartStamp>,
    owed: Owed,
    /// Mouse buttons counted just before the front was taken.
    input: InputActivity,
}

static TAKEN: Mutex<Option<Taken>> = Mutex::new(None);

fn taken() -> std::sync::MutexGuard<'static, Option<Taken>> {
    TAKEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The app the front was taken from, as the hint names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owed {
    pub(crate) pid: i32,
    stamp: Option<ProcessStartStamp>,
    pub(crate) name: String,
    pub(crate) bundle_id: Option<String>,
}

impl Owed {
    fn of(pid: i32) -> Self {
        let app = crate::apps::running_app(pid);
        Owed {
            pid,
            stamp: process_start_stamp(pid),
            // running_app lists regular apps only; an accessory app has one too.
            bundle_id: app
                .as_ref()
                .and_then(|app| app.bundle_id.clone())
                .or_else(|| crate::apps::bundle_id_for_pid(pid)),
            name: app.map(|app| app.name).unwrap_or_else(|| name(pid)),
        }
    }
}

/// Whether a claim on the front still stands for the front app now: the app
/// holding the front (Finder for a lease, the app the last `bring_to_front`
/// brought) is that same process, the app it was taken from still runs, and
/// no mouse press that was not cua's came since (the user is working there).
fn holds(holder: i32, holder_same: bool, front: i32, owed_running: bool, user_input: bool) -> bool {
    holder == front && holder_same && owed_running && !user_input
}

/// What happens to the carried record once the call is over.
#[derive(Debug, PartialEq, Eq)]
enum Record {
    /// The target did not come to the front: nothing changed.
    Keep,
    /// The front went back to the app it was owed to, or nothing is owed.
    Clear,
    /// The target is in front now, owing the named app.
    Set,
}

fn record_after(target: i32, target_in_front: bool, owed: Option<i32>) -> Record {
    match owed {
        // The front did not read before the call: what is carried stands.
        None => Record::Keep,
        _ if !target_in_front => Record::Keep,
        Some(owed) if owed != target => Record::Set,
        Some(_) => Record::Clear,
    }
}

/// Whether the agent's `bring_to_front` of `target` ends a lease on
/// `holder`: it does when it moved the front, or chose the holder itself; a
/// call that left the holder in front (a failed activation) leaves the lease.
fn ends_lease(holder: i32, target: i32, front_after: Option<i32>) -> bool {
    front_after.is_some_and(|front| front != holder || target == holder)
}

/// Whether the result names `owed`: not when the agent is handing the front
/// back to it, nor when it is still in front.
fn names(owed: i32, target: i32, front_after: Option<i32>) -> bool {
    owed != target && front_after != Some(owed)
}

/// What the agent's `bring_to_front` reads just before it activates (after
/// its arguments were checked): the app it takes the front from. It holds
/// the lease lock until `finish`, so an action's settle cannot hand the
/// front back in the middle of the activation (as `end()` serialized it).
pub(crate) struct Taking {
    owed: Option<Owed>,
    held: std::sync::MutexGuard<'static, Option<Lease>>,
    input: InputActivity,
}

impl Taking {
    /// `target`: the app the call brings forward.
    pub(crate) fn read(target: i32) -> Self {
        // The lease first: a settle in progress (it holds the lock while it
        // hands the front back) finishes before the mouse counters and the
        // front are read.
        let mut held = lease();
        let input = crate::focus_steal::read_input_activity();
        let front = crate::apps::frontmost_pid();
        // The app the agent's last bring_to_front took the front from, while
        // `holder` (the app it brought) still holds it.
        let carried_for = |holder: i32| {
            taken().as_ref().and_then(|taken| {
                holds(
                    taken.brought,
                    same_app(taken.brought_app, process_start_stamp(taken.brought)),
                    holder,
                    same_app(taken.owed.stamp, process_start_stamp(taken.owed.pid)),
                    crate::focus_steal::user_input_since(&taken.input),
                )
                .then(|| taken.owed.clone())
            })
        };
        let lease_holder = held.as_ref().map(|lease| lease.target);
        let from_lease = held.as_ref().and_then(|lease| {
            let stands = holds(
                lease.target,
                same_app(lease.target_app, process_start_stamp(lease.target)),
                front?,
                same_app(lease.previous_app, process_start_stamp(lease.previous)),
                crate::focus_steal::user_input_since(&lease.input),
            );
            // The lease owes the app the agent brought: that one owes on.
            stands.then(|| {
                carried_for(lease.previous).unwrap_or_else(|| Owed {
                    pid: lease.previous,
                    stamp: lease.previous_app,
                    name: lease.previous_name.clone(),
                    bundle_id: crate::apps::bundle_id_for_pid(lease.previous),
                })
            })
        });
        // The agent chose the lease's own app: its choice is final.
        if lease_holder == Some(target) {
            *held = None;
        }
        Taking {
            owed: from_lease
                .or_else(|| carried_for(front?))
                .or_else(|| front.map(Owed::of)),
            held,
            input,
        }
    }

    /// After the activation: end the lease it replaced, carry the owed app
    /// while `target` is in front, and return the app the result names.
    pub(crate) fn finish(mut self, target: i32) -> Option<Owed> {
        chose();
        let front_after = crate::apps::frontmost_pid();
        let holder = self.held.as_ref().map(|lease| lease.target);
        if holder.is_some_and(|holder| ends_lease(holder, target, front_after)) {
            *self.held = None;
        }
        let owed_pid = self.owed.as_ref().map(|owed| owed.pid);
        match record_after(target, front_after == Some(target), owed_pid) {
            Record::Keep => {}
            Record::Clear => *taken() = None,
            Record::Set => {
                *taken() = Some(Taken {
                    brought: target,
                    brought_app: process_start_stamp(target),
                    owed: self.owed.clone()?,
                    input: self.input,
                })
            }
        }
        self.owed
            .filter(|owed| names(owed.pid, target, front_after))
    }
}

/// With no lease held: `pid`'s inline edit is open while it is behind. An
/// editor that opens after the front was handed back (New Folder on a
/// Desktop window, measured) stays open there, and the agent can end it from
/// behind; without this the result reads as if the app needed the front.
pub(crate) fn edit_open_behind(pid: i32) -> Option<String> {
    if !crate::tools::edit_commit::app_saves_on_end_editing(pid)
        || crate::apps::frontmost_pid()? == pid
        || crate::ax::bindings::inline_edit_state(pid) != Some(true)
    {
        return None;
    }
    let app = name(pid);
    Some(format!(
        "{app}'s inline edit is open with {app} behind, and stays open there: set_value on its \
         field, then press_key return (delivery_mode:\"foreground\" if the key is refused), end \
         it; bring_to_front is not needed"
    ))
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
    let edit = open_now(lease.hold, lease.target);
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

/// Whether what holds the front is still open: the edit, or any panel.
fn open_now(hold: Hold, target: i32) -> Option<bool> {
    match hold {
        Hold::Edit => crate::ax::bindings::inline_edit_state(target),
        // The panel the lease began on: another window's panel is another step.
        Hold::Panel(panel, window) => {
            crate::ax::bindings::panel_state(target).map(|open| open.contains(&(panel, window)))
        }
    }
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
    let (target, previous, hold) = (lease.target, lease.previous, lease.hold);
    let previous_name = lease.previous_name.clone();
    let mut step = read_and_decide(lease);
    let mut confirmed = false;
    if step == Step::Restore {
        match hand_back(lease) {
            Ok(back) => confirmed = back,
            Err(again) => step = again,
        }
    }
    let words = match step {
        // The app owed is in front again (a focus guard put it back, or
        // the agent did): nothing is owed, nothing to say.
        Step::Leave(Why::FrontChanged(front)) if front == previous => None,
        Step::Restore if !confirmed => {
            let now = crate::apps::frontmost_pid().map_or_else(
                || "the front app did not read".into(),
                |pid| format!("{} is in front", name(pid)),
            );
            Some(format!(
                "{}, but {previous_name} did not come back to the front ({now})",
                ended(hold)
            ))
        }
        _ => Some(say(hold, &step, &name(target), &previous_name)),
    };
    if !matches!(step, Step::Keep | Step::Unread) {
        *held = None;
    }
    words
}

/// What the lease waits on, as the result names it.
fn what(hold: Hold) -> &'static str {
    match hold {
        Hold::Edit => "inline edit",
        Hold::Panel(panel, _) => panel.name(),
    }
}

fn ended(hold: Hold) -> String {
    match hold {
        Hold::Edit => "the inline edit ended".into(),
        Hold::Panel(panel, _) => format!("the {} closed", panel.name()),
    }
}

/// The words for a settle step (a hand-back here is a confirmed one).
fn say(hold: Hold, step: &Step, target: &str, previous: &str) -> String {
    let what = what(hold);
    match (step, hold) {
        (Step::Keep, Hold::Edit) => format!(
            "{target} stays in front while its inline edit is open (it ends the edit when it \
             loses the front); cua brings {previous} back when a cua action ends the edit"
        ),
        (Step::Keep, Hold::Panel(..)) => format!(
            "{target} stays in front while its {what} is open; cua brings {previous} back when a \
             cua action closes it"
        ),
        (Step::Unread, _) => format!(
            "cua could not read whether {target}'s {what} is still open, so the front was left as \
             it is; cua brings {previous} back once a cua action reads it {}",
            if hold == Hold::Edit { "ended" } else { "closed" }
        ),
        (Step::Restore, _) => format!("{}, so cua brought {previous} back to the front", ended(hold)),
        (Step::Leave(Why::FrontChanged(front)), _) => format!(
            "{} is in front now, so cua did not bring {previous} back after {target}'s {}",
            name(*front),
            if hold == Hold::Edit { "edit" } else { what }
        ),
        (Step::Leave(Why::UserInput), _) => format!(
            "a mouse press that was not cua's came while {target}'s {what} was open, so cua did not \
             bring {previous} back; {target} stays in front"
        ),
        (Step::Leave(Why::TargetGone), _) => format!(
            "the app cua left in front for its {what} is no longer running, so cua did not bring \
             {previous} back"
        ),
        (Step::Leave(Why::PreviousGone), _) => {
            format!("{previous} is no longer running; {target} stays in front")
        }
    }
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

    /// The bring_to_front hint's table (research/2026-10-08-bring-to-front):
    /// a claim names its owed app only while its holder is the same process
    /// in front, the owed app runs, and the user has not pressed the mouse.
    #[test]
    fn a_claim_names_its_owed_app_only_while_it_holds_the_front() {
        // Finder in front for its edit (lease), or TextEdit brought by the
        // agent (carried): Terminal is named.
        assert!(holds(FINDER, true, FINDER, true, false));
        assert!(holds(TEXTEDIT, true, TEXTEDIT, true, false));
        // The user switched to Calculator: the front app is named instead.
        assert!(!holds(TEXTEDIT, true, 40, true, false));
        // The user pressed the mouse in the app the agent brought.
        assert!(!holds(TEXTEDIT, true, TEXTEDIT, true, true));
        // The owed app quit, or the holder's pid now names another process.
        assert!(!holds(TEXTEDIT, true, TEXTEDIT, false, false));
        assert!(!holds(TEXTEDIT, false, TEXTEDIT, true, false));
    }

    #[test]
    fn the_carried_record_follows_what_the_call_did() {
        // TextEdit came to the front from Terminal: carry Terminal.
        assert_eq!(record_after(TEXTEDIT, true, Some(TERMINAL)), Record::Set);
        // Terminal is given the front back: nothing is owed any more.
        assert_eq!(record_after(TERMINAL, true, Some(TERMINAL)), Record::Clear);
        // The hand-back failed (Terminal is not in front): keep owing it.
        assert_eq!(record_after(TERMINAL, false, Some(TERMINAL)), Record::Keep);
        assert_eq!(record_after(TEXTEDIT, false, Some(TERMINAL)), Record::Keep);
        // The front did not read before the call: what is carried stands.
        assert_eq!(record_after(TEXTEDIT, true, None), Record::Keep);
    }

    #[test]
    fn bring_to_front_ends_a_lease_only_when_it_moved_the_front_or_chose_its_app() {
        assert!(ends_lease(FINDER, TEXTEDIT, Some(TEXTEDIT)));
        assert!(ends_lease(FINDER, FINDER, Some(FINDER)));
        // A failed activation left Finder (and its edit) in front.
        assert!(!ends_lease(FINDER, TEXTEDIT, Some(FINDER)));
        assert!(!ends_lease(FINDER, TEXTEDIT, None));
    }

    #[test]
    fn the_result_names_the_owed_app_unless_it_has_the_front() {
        assert!(names(TERMINAL, TEXTEDIT, Some(TEXTEDIT)));
        assert!(names(TERMINAL, TEXTEDIT, None));
        // Handing back, or nothing moved.
        assert!(!names(TERMINAL, TERMINAL, Some(TERMINAL)));
        assert!(!names(TERMINAL, TEXTEDIT, Some(TERMINAL)));
    }

    /// The panel table (research/2026-10-08-panel-front): when an action's
    /// end front begins a panel lease.
    #[test]
    fn a_panel_lease_begins_only_when_an_action_brought_its_app_forward() {
        let save = || Some(vec![(Panel::Save, 7)]);
        // sheet_click_*, panel_window, chrome_chooser_click, sheet_other_app:
        // a desktop click brought TextEdit (or Chrome) forward over Terminal.
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, false, save(), None),
            Some((Panel::Save, 7))
        );
        // textedit_was_front, and every foreground action that restored the
        // front: it did not move.
        assert_eq!(
            begins(Some(TEXTEDIT), TEXTEDIT, None, false, save(), None),
            None
        );
        // The front did not read before the action: nothing to owe.
        assert_eq!(begins(None, TEXTEDIT, None, false, save(), None), None);
        // A lease already holds that app (Finder kept for its edit).
        assert_eq!(
            begins(
                Some(TERMINAL),
                TEXTEDIT,
                Some(TEXTEDIT),
                false,
                save(),
                None
            ),
            None
        );
        // sheet_user_switch: a mouse press that was not cua's chose the app.
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, true, save(), None),
            None
        );
        // No panel, or the panel did not read.
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, false, Some(vec![]), None),
            None
        );
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, false, None, None),
            None
        );
    }

    /// Which panel a lease waits on: the one in the focused window, then a
    /// file panel before another sheet.
    #[test]
    fn a_panel_lease_waits_on_the_panel_the_action_left_focused() {
        let panels = || Some(vec![(Panel::Sheet, 5), (Panel::Save, 6), (Panel::Save, 7)]);
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, false, panels(), Some(7)),
            Some((Panel::Save, 7))
        );
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, false, panels(), Some(5)),
            Some((Panel::Sheet, 5))
        );
        assert_eq!(
            begins(Some(TERMINAL), TEXTEDIT, None, false, panels(), None),
            Some((Panel::Save, 6))
        );
    }

    /// A panel brought forward while Finder held the front for its edit owes
    /// the app from before the edit; one brought forward by the app the
    /// lease owed owes nothing.
    #[test]
    fn a_panel_after_a_kept_edit_owes_the_app_from_before_it() {
        assert_eq!(
            owed(Some((FINDER, TERMINAL)), TEXTEDIT, Some(FINDER)),
            Some((TERMINAL, true))
        );
        assert_eq!(owed(Some((FINDER, TEXTEDIT)), TEXTEDIT, Some(FINDER)), None);
    }

    /// Through `start` itself, with live pids: a Panel lease begun over the
    /// app a held lease kept in front owes that lease's app, and an Edit keep
    /// begun over a Panel lease's app does the same.
    #[test]
    fn a_lease_begun_over_another_leases_app_keeps_its_debt() {
        let me = std::process::id() as i32;
        let parent = unsafe { libc::getppid() };
        let user = 1; // launchd: always running, never this test
        let before = |front: i32| Before {
            previous: Some(front),
            stamp: process_start_stamp(front),
            input: InputActivity::default(),
            standing: None,
            choices: 0,
        };
        let mut held = None;
        // The held lease stands (live process check only; no mouse state).
        let stands = |lease: &Lease| same_app(lease.target_app, process_start_stamp(lease.target));
        assert!(start_with(&mut held, me, before(user), Hold::Edit, stands));
        assert!(start_with(
            &mut held,
            parent,
            before(me),
            Hold::Panel(Panel::Save, 7),
            stands
        ));
        let lease = held.as_ref().unwrap();
        assert_eq!(
            (lease.target, lease.previous, lease.hold),
            (parent, user, Hold::Panel(Panel::Save, 7))
        );
        assert!(start_with(
            &mut held,
            me,
            before(parent),
            Hold::Edit,
            stands
        ));
        assert_eq!(
            held.as_ref().map(|lease| (lease.target, lease.previous)),
            Some((me, user))
        );
        // Another call's settle cleared the lease during the action: the one
        // that stood when it began still carries the debt.
        let standing = held.take();
        let mut cleared = None;
        let during = Before {
            standing,
            ..before(me)
        };
        assert!(start_with(
            &mut cleared,
            parent,
            during,
            Hold::Panel(Panel::Open, 8),
            stands
        ));
        assert_eq!(
            cleared.as_ref().map(|lease| (lease.target, lease.previous)),
            Some((parent, user))
        );
        held = cleared;
        // The app owed comes forward: nothing is owed, the lease is left to settle.
        assert!(!start_with(
            &mut held,
            user,
            before(parent),
            Hold::Panel(Panel::Save, 7),
            stands
        ));
    }

    #[test]
    fn the_words_name_the_panel_and_keep_the_edit_text() {
        let panel = Hold::Panel(Panel::Save, 7);
        assert_eq!(
            say(panel, &Step::Keep, "TextEdit", "Terminal"),
            "TextEdit stays in front while its Save panel is open; cua brings Terminal back when a \
             cua action closes it"
        );
        assert_eq!(
            say(panel, &Step::Restore, "TextEdit", "Terminal"),
            "the Save panel closed, so cua brought Terminal back to the front"
        );
        assert!(
            say(panel, &Step::Leave(Why::UserInput), "TextEdit", "Terminal").contains(
                "while TextEdit's Save panel was open, so cua did not bring Terminal back"
            )
        );
        assert_eq!(
            say(Hold::Edit, &Step::Keep, "Finder", "Terminal"),
            "Finder stays in front while its inline edit is open (it ends the edit when it loses \
             the front); cua brings Terminal back when a cua action ends the edit"
        );
        assert_eq!(
            say(Hold::Edit, &Step::Restore, "Finder", "Terminal"),
            "the inline edit ended, so cua brought Terminal back to the front"
        );
    }

    /// Row chrome_chooser_click: only a browser's Open panel is its chooser.
    #[test]
    fn the_chooser_hint_is_for_a_browser_open_panel() {
        assert!(hint_for(true, &[(Panel::Open, 7)])
            .is_some_and(|hint| hint.contains("browser_set_input_files")));
        assert_eq!(hint_for(true, &[(Panel::Save, 7)]), None);
        assert_eq!(hint_for(true, &[]), None);
        assert_eq!(hint_for(false, &[(Panel::Open, 7)]), None);
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
