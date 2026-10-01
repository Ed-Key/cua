//! The agent's turn, as a client's hooks report it, so a PiP panel lives for
//! the whole turn: shown from the turn's first action until it ends, then the
//! finished state, then a fade.
//!
//! MCP says nothing about turns. Claude Code's `mcp_tool` hooks call the
//! `pip_turn` tool on the session's own MCP connection (UserPromptSubmit,
//! Stop, StopFailure, SessionEnd), so the transport whose turn changed is the
//! trusted one the call arrived on, never a public label. Turn state is kept
//! per transport and shared by every session label on it; a label that did
//! not act during the turn gets nothing from it.
//!
//! A transport is in one of three states: never hooked (no entry: every path
//! behaves as before turns existed, also after a daemon restart until the
//! next UserPromptSubmit; an ordinary action never opens a turn), turn open
//! (`Open`, or `Stopping` while the Stop's visual debounce runs), or turn
//! closed (`Closed`).
//!
//! | Event | Never hooked | Open | Stopping | Closed |
//! |---|---|---|---|---|
//! | UserPromptSubmit | open a turn | the old turn was interrupted (no Stop fires on an interrupt): its labels end quietly; open a new turn | the Stop was real: its labels finish; open a new turn | open a new turn (a finale still playing keeps playing until the turn's first action) |
//! | Stop | ignored (end without start) | `Stopping`: the debounce starts | ignored (duplicate) | ignored (duplicate) |
//! | debounce due (1.5 s after the Stop, no action since) | - | - | `Closed`: the turn's labels finish (the finished state plays, the session stays alive) | - |
//! | StopFailure | ignored | `Closed`: the labels end quietly | as Open | ignored |
//! | SessionEnd | ignored | the labels' panels go as at a closed connection (fresh work only plays its finale); the entry goes | the labels' panels go as at `end_session` (one finale, never a second); the entry goes | as Stopping |
//! | Any other event (SubagentStop, SessionStart after compaction, ...) | ignored | ignored | ignored | ignored |
//! | Action (any tool call but `pip_turn`) | nothing | the lease restarts | back to `Open` (a blocked Stop: Claude went on), the lease restarts | the turn opens again (a Stop another hook blocked, or work after a quiet end) |
//! | A label acts (its frame or verification is pushed) | nothing | first time this turn: its panel is held open | as Open | as Open |
//! | Lease due (no action and no turn event for `lease()`, 5 min) | - | `Closed`: the labels end quietly | as Open | - |
//! | Connection closes | - | the entry goes; panels follow the connection-close rule | as Open | as Open |
//!
//! Stop is not proof that the turn is over: Claude Code runs Stop hooks in
//! parallel, so another Stop hook (a goal gate exiting 2) can block the stop
//! after this one already ran, and nothing tells cua. The finished state may
//! then show; the next action re-opens the turn and the panel comes back. No
//! timer pretends to detect it.
//!
//! While a transport's turn is open and inside its lease, the idle-TTL sweep
//! leaves its sessions alone (`defers_eviction`).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long after a Stop with no new action the finished state plays: a
/// visual debounce, not a correctness mechanism.
pub const STOP_DEBOUNCE: Duration = Duration::from_millis(1500);
/// A turn with no action and no turn event this long ends quietly.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(5 * 60);

/// The lease, overridable for checks with `CUA_DRIVER_RS_PIP_TURN_LEASE_SECS`.
pub fn lease() -> Duration {
    static LEASE: OnceLock<Duration> = OnceLock::new();
    *LEASE.get_or_init(|| {
        std::env::var("CUA_DRIVER_RS_PIP_TURN_LEASE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(DEFAULT_LEASE, Duration::from_secs)
    })
}

/// What one session's panel is told about its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipHookTurn {
    /// The session acts in an open turn: its panel stays up (no idle hide)
    /// and a proof's finale waits for the turn's end.
    Open,
    /// The turn ended: the finished state plays, the session stays alive.
    Finished,
    /// The turn ended without a finish: the panel fades, no finale.
    Quiet,
    /// The client's session ended: the panel goes, as at `end_session`
    /// (`finished`) or as at a closed connection.
    End { finished: bool },
}

/// A hook event, by Claude Code's `hook_event_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEvent {
    Start,
    Stop,
    StopFailure,
    SessionEnd,
}

impl TurnEvent {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "UserPromptSubmit" => Some(Self::Start),
            "Stop" => Some(Self::Stop),
            "StopFailure" => Some(Self::StopFailure),
            "SessionEnd" => Some(Self::SessionEnd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    Stopping,
    Closed,
}

struct Turn {
    phase: Phase,
    /// Bumped each time a turn opens: the lease timer's identity.
    epoch: u64,
    /// Bumped by every Stop and everything that calls one off: the
    /// debounce's identity.
    stop: u64,
    /// The last action or turn event.
    last: Instant,
    /// Session keys that acted during this turn (each was told `Open`).
    acted: BTreeSet<String>,
    /// Every session key that acted on this transport since its first turn.
    keys: BTreeSet<String>,
}

/// What happens to a transport's turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    Hook(TurnEvent),
    Action,
    Acted(String),
    DebounceDue(u64),
    LeaseDue(u64),
}

/// A timer to arm for a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timer {
    Debounce(u64),
    Lease(u64),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Effects {
    notes: Vec<(String, PipHookTurn)>,
    timer: Option<(Timer, Instant)>,
}

#[derive(Default)]
struct Turns {
    map: HashMap<String, Turn>,
    next: u64,
}

impl Turns {
    fn apply(&mut self, transport: &str, input: Input, now: Instant, lease: Duration) -> Effects {
        let mut effects = Effects::default();
        if let Input::Hook(TurnEvent::Start) = input {
            self.next += 1;
            let epoch = self.next;
            let turn = self
                .map
                .entry(transport.to_owned())
                .or_insert_with(|| Turn {
                    phase: Phase::Closed,
                    epoch,
                    stop: 0,
                    last: now,
                    acted: BTreeSet::new(),
                    keys: BTreeSet::new(),
                });
            let end = match turn.phase {
                Phase::Open => Some(PipHookTurn::Quiet),
                Phase::Stopping => Some(PipHookTurn::Finished),
                Phase::Closed => None,
            };
            if let Some(end) = end {
                effects.notes = turn.acted.iter().map(|key| (key.clone(), end)).collect();
            }
            turn.acted.clear();
            turn.phase = Phase::Open;
            turn.epoch = epoch;
            turn.stop += 1;
            turn.last = now;
            effects.timer = Some((Timer::Lease(epoch), now + lease));
            return effects;
        }
        if let Input::Hook(TurnEvent::SessionEnd) = input {
            if let Some(turn) = self.map.remove(transport) {
                let finished = turn.phase != Phase::Open;
                effects.notes = turn
                    .keys
                    .into_iter()
                    .map(|key| (key, PipHookTurn::End { finished }))
                    .collect();
            }
            return effects;
        }
        self.next += 1;
        let next = self.next;
        let Some(turn) = self.map.get_mut(transport) else {
            return effects;
        };
        let close = |turn: &mut Turn, end: PipHookTurn, notes: &mut Vec<(String, PipHookTurn)>| {
            turn.phase = Phase::Closed;
            turn.stop += 1;
            notes.extend(
                std::mem::take(&mut turn.acted)
                    .into_iter()
                    .map(|key| (key, end)),
            );
        };
        match input {
            Input::Hook(TurnEvent::Stop) => {
                if turn.phase == Phase::Open {
                    turn.phase = Phase::Stopping;
                    turn.stop += 1;
                    turn.last = now;
                    effects.timer = Some((Timer::Debounce(turn.stop), now + STOP_DEBOUNCE));
                }
            }
            Input::Hook(TurnEvent::StopFailure) => {
                if turn.phase != Phase::Closed {
                    close(turn, PipHookTurn::Quiet, &mut effects.notes);
                }
            }
            Input::Action | Input::Acted(_) => {
                match turn.phase {
                    Phase::Open => {}
                    Phase::Stopping => {
                        turn.phase = Phase::Open;
                        turn.stop += 1;
                    }
                    Phase::Closed => {
                        turn.phase = Phase::Open;
                        turn.epoch = next;
                        effects.timer = Some((Timer::Lease(next), now + lease));
                    }
                }
                turn.last = now;
                if let Input::Acted(key) = input {
                    turn.keys.insert(key.clone());
                    if turn.acted.insert(key.clone()) {
                        effects.notes.push((key, PipHookTurn::Open));
                    }
                }
            }
            Input::DebounceDue(stop) => {
                if turn.phase == Phase::Stopping && turn.stop == stop {
                    close(turn, PipHookTurn::Finished, &mut effects.notes);
                }
            }
            Input::LeaseDue(epoch) => {
                if turn.phase != Phase::Closed && turn.epoch == epoch {
                    let due = turn.last + lease;
                    if now >= due {
                        close(turn, PipHookTurn::Quiet, &mut effects.notes);
                    } else {
                        effects.timer = Some((Timer::Lease(epoch), due));
                    }
                }
            }
            Input::Hook(TurnEvent::Start | TurnEvent::SessionEnd) => unreachable!(),
        }
        effects
    }

    fn defers_eviction(&self, transport: &str, now: Instant, lease: Duration) -> bool {
        self.map
            .get(transport)
            .is_some_and(|turn| turn.phase != Phase::Closed && now < turn.last + lease)
    }
}

static TURNS: Mutex<Option<Turns>> = Mutex::new(None);

type TurnFnBox = Box<dyn Fn(&str, PipHookTurn) + Send + Sync>;
static PIP_TURN_FN: OnceLock<TurnFnBox> = OnceLock::new();

/// Register the platform-side turn callback (non-blocking: it only
/// enqueues). `main.rs` calls this once next to `set_pip_push_fn`.
pub fn set_pip_turn_fn(f: impl Fn(&str, PipHookTurn) + Send + Sync + 'static) {
    let _ = PIP_TURN_FN.set(Box::new(f));
}

/// Apply `input` to `transport`'s turn. Its notes go out under the lock, so
/// a timer's note can never overtake an action's that came after it.
fn apply(transport: &str, input: Input) {
    if !crate::pip_hook::pip_enabled() || transport.is_empty() {
        return;
    }
    let mut guard = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    let turns = guard.get_or_insert_with(Turns::default);
    let effects = turns.apply(transport, input, Instant::now(), lease());
    if !effects.notes.is_empty() {
        tracing::info!(target: "pip", notes = ?effects.notes.iter().map(|(_, note)| note).collect::<Vec<_>>(), "PiP turn notes");
    }
    if let Some(notify) = PIP_TURN_FN.get() {
        for (key, note) in &effects.notes {
            notify(key, *note);
        }
    }
    drop(guard);
    if let Some((timer, at)) = effects.timer {
        let transport = transport.to_owned();
        let spawned = std::thread::Builder::new()
            .name("cua-pip-turn".into())
            .spawn(move || {
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                let input = match timer {
                    Timer::Debounce(stop) => Input::DebounceDue(stop),
                    Timer::Lease(epoch) => Input::LeaseDue(epoch),
                };
                apply(&transport, input);
            });
        if let Err(error) = spawned {
            tracing::warn!(target: "pip", %error, "PiP turn timer did not start");
        }
    }
}

/// A `pip_turn` call on `transport` (the trusted runtime transport id):
/// `event` is the client's hook event name; anything else is ignored.
pub fn hook(transport: &str, event: &str) {
    let parsed = TurnEvent::parse(event);
    tracing::info!(target: "pip", event = %event.chars().take(32).collect::<String>(), known = parsed.is_some(), "PiP turn event");
    if let Some(event) = parsed {
        apply(transport, Input::Hook(event));
    }
}

/// Any other tool call on `transport` (subagents share the connection, so
/// theirs count too).
pub fn action(transport: &str) {
    apply(transport, Input::Action);
}

/// The session `session_key` on `transport` acted: its frame or
/// verification is about to be pushed.
pub fn acted(transport: &str, session_key: &str) {
    apply(transport, Input::Acted(session_key.to_owned()));
}

/// The transport's connection closed: its turn state goes. Its panels
/// follow the connection-close rule through the session end hooks.
pub fn forget(transport: &str) {
    if let Some(turns) = TURNS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        turns.map.remove(transport);
    }
}

/// Whether the idle-TTL sweep must leave `transport`'s sessions alone: its
/// turn is open and inside its lease.
pub fn defers_eviction(transport: &str) -> bool {
    TURNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|turns| turns.defers_eviction(transport, Instant::now(), lease()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: Duration = Duration::from_secs(300);
    const T: &str = "transport-a";

    struct Run {
        turns: Turns,
        t0: Instant,
    }

    impl Run {
        fn new() -> Self {
            Self {
                turns: Turns::default(),
                t0: Instant::now(),
            }
        }

        fn at(&mut self, secs: f64, transport: &str, input: Input) -> Effects {
            let now = self.t0 + Duration::from_secs_f64(secs);
            self.turns.apply(transport, input, now, LEASE)
        }

        fn phase(&self, transport: &str) -> Option<Phase> {
            self.turns.map.get(transport).map(|turn| turn.phase)
        }

        /// The debounce armed by the last Stop, fired on time.
        fn debounce(&mut self, effects: &Effects, secs: f64) -> Effects {
            let Some((Timer::Debounce(stop), _)) = effects.timer else {
                panic!("no debounce armed: {effects:?}");
            };
            self.at(secs, T, Input::DebounceDue(stop))
        }
    }

    fn notes(effects: &Effects) -> Vec<(&str, PipHookTurn)> {
        effects
            .notes
            .iter()
            .map(|(key, note)| (key.as_str(), *note))
            .collect()
    }

    fn acted(key: &str) -> Input {
        Input::Acted(key.to_owned())
    }

    const START: Input = Input::Hook(TurnEvent::Start);
    const STOP: Input = Input::Hook(TurnEvent::Stop);

    #[test]
    fn hook_event_names_map_to_turn_events_and_others_are_ignored() {
        assert_eq!(TurnEvent::parse("UserPromptSubmit"), Some(TurnEvent::Start));
        assert_eq!(TurnEvent::parse("Stop"), Some(TurnEvent::Stop));
        assert_eq!(
            TurnEvent::parse("StopFailure"),
            Some(TurnEvent::StopFailure)
        );
        assert_eq!(TurnEvent::parse("SessionEnd"), Some(TurnEvent::SessionEnd));
        // Subagents share the parent's connection; their stop is not its.
        // A SessionStart (after compaction too) is no new turn.
        for other in ["SubagentStop", "SessionStart", "PreCompact", "start", ""] {
            assert_eq!(TurnEvent::parse(other), None, "{other}");
        }
    }

    #[test]
    fn t1_t3_a_turn_holds_its_labels_and_finishes_after_the_debounce() {
        let mut run = Run::new();
        // T1: the start creates nothing, it arms the lease.
        let start = run.at(0.0, T, START);
        assert!(start.notes.is_empty());
        assert_eq!(start.timer.map(|(timer, _)| timer), Some(Timer::Lease(1)));
        // T2: each label is held once per turn.
        assert_eq!(
            notes(&run.at(1.0, T, acted("a"))),
            [("a", PipHookTurn::Open)]
        );
        assert!(run.at(2.0, T, acted("a")).notes.is_empty());
        assert_eq!(
            notes(&run.at(3.0, T, acted("b"))),
            [("b", PipHookTurn::Open)]
        );
        // T3: the Stop only arms the debounce...
        let stop = run.at(4.0, T, STOP);
        assert!(stop.notes.is_empty());
        let (_, due) = stop.timer.unwrap();
        assert_eq!(due, run.t0 + Duration::from_secs(4) + STOP_DEBOUNCE);
        // ...which finishes both labels.
        let finished = run.debounce(&stop, 5.5);
        assert_eq!(
            notes(&finished),
            [("a", PipHookTurn::Finished), ("b", PipHookTurn::Finished)]
        );
        assert_eq!(run.phase(T), Some(Phase::Closed));
    }

    #[test]
    fn t4_an_action_inside_the_debounce_calls_the_finish_off() {
        let mut run = Run::new();
        run.at(0.0, T, START);
        run.at(1.0, T, acted("a"));
        let stop = run.at(2.0, T, STOP);
        // Another Stop hook blocked the stop; Claude acts again.
        run.at(2.5, T, Input::Action);
        assert_eq!(run.phase(T), Some(Phase::Open));
        assert!(
            run.debounce(&stop, 3.5).notes.is_empty(),
            "a stale debounce"
        );
        // The next Stop finishes it.
        let again = run.at(9.0, T, STOP);
        assert_eq!(
            notes(&run.debounce(&again, 10.5)),
            [("a", PipHookTurn::Finished)]
        );
    }

    #[test]
    fn t4_after_a_finish_the_next_action_reopens_the_turn() {
        let mut run = Run::new();
        run.at(0.0, T, START);
        run.at(1.0, T, acted("a"));
        let stop = run.at(2.0, T, STOP);
        run.debounce(&stop, 3.5);
        // The stop was blocked after all: the work goes on.
        let reopened = run.at(20.0, T, acted("a"));
        assert_eq!(notes(&reopened), [("a", PipHookTurn::Open)]);
        assert!(matches!(reopened.timer, Some((Timer::Lease(_), _))));
        assert_eq!(run.phase(T), Some(Phase::Open));
        // A read alone reopens the turn too, without holding anyone.
        let stop = run.at(21.0, T, STOP);
        run.debounce(&stop, 22.5);
        let read = run.at(30.0, T, Input::Action);
        assert!(read.notes.is_empty());
        assert_eq!(run.phase(T), Some(Phase::Open));
    }

    #[test]
    fn a_turn_with_no_action_creates_nothing_and_replays_nothing() {
        let mut run = Run::new();
        run.at(0.0, T, START);
        run.at(1.0, T, acted("a"));
        let stop = run.at(2.0, T, STOP);
        run.debounce(&stop, 3.5);
        // A turn in which the label never acts: nothing for it.
        assert!(run.at(10.0, T, START).notes.is_empty());
        let stop = run.at(12.0, T, STOP);
        assert!(run.debounce(&stop, 13.5).notes.is_empty());
    }

    #[test]
    fn t5_a_silent_turn_ends_quietly_at_its_lease_and_activity_moves_it() {
        let mut run = Run::new();
        let start = run.at(0.0, T, START);
        let Some((Timer::Lease(epoch), due)) = start.timer else {
            panic!()
        };
        assert_eq!(due, run.t0 + LEASE);
        run.at(100.0, T, acted("a"));
        // Due at the first deadline, but the action moved it.
        let early = run.at(300.0, T, Input::LeaseDue(epoch));
        assert!(early.notes.is_empty());
        assert_eq!(
            early.timer,
            Some((Timer::Lease(epoch), run.t0 + Duration::from_secs(400)))
        );
        // Interrupted: no Stop ever fires.
        assert_eq!(
            notes(&run.at(400.0, T, Input::LeaseDue(epoch))),
            [("a", PipHookTurn::Quiet)]
        );
        assert_eq!(run.phase(T), Some(Phase::Closed));
        // A later action re-opens it.
        assert_eq!(
            notes(&run.at(500.0, T, acted("a"))),
            [("a", PipHookTurn::Open)]
        );
    }

    #[test]
    fn a_stale_lease_timer_from_an_earlier_turn_does_nothing() {
        let mut run = Run::new();
        let first = run.at(0.0, T, START);
        let Some((Timer::Lease(old), _)) = first.timer else {
            panic!()
        };
        run.at(1.0, T, START);
        run.at(2.0, T, acted("a"));
        assert!(run.at(301.0, T, Input::LeaseDue(old)).notes.is_empty());
        assert_eq!(run.phase(T), Some(Phase::Open));
    }

    #[test]
    fn stop_failure_ends_quietly() {
        let mut run = Run::new();
        run.at(0.0, T, START);
        run.at(1.0, T, acted("a"));
        let stop = run.at(2.0, T, STOP);
        let failure = run.at(2.2, T, Input::Hook(TurnEvent::StopFailure));
        assert_eq!(notes(&failure), [("a", PipHookTurn::Quiet)]);
        assert!(run.debounce(&stop, 3.5).notes.is_empty());
        // A duplicate does nothing.
        assert!(run
            .at(4.0, T, Input::Hook(TurnEvent::StopFailure))
            .notes
            .is_empty());
    }

    #[test]
    fn a_new_start_ends_the_turn_before_it() {
        // Interrupted (no Stop): quietly.
        let mut run = Run::new();
        run.at(0.0, T, START);
        run.at(1.0, T, acted("a"));
        assert_eq!(notes(&run.at(5.0, T, START)), [("a", PipHookTurn::Quiet)]);
        // A Stop still in its debounce: the stop was real, it finishes.
        run.at(6.0, T, acted("a"));
        let stop = run.at(7.0, T, STOP);
        assert_eq!(
            notes(&run.at(7.5, T, START)),
            [("a", PipHookTurn::Finished)]
        );
        assert!(
            run.debounce(&stop, 8.5).notes.is_empty(),
            "the debounce went stale"
        );
        // A start during a finale (turn closed) says nothing: the finale
        // plays on until the new turn's first action.
        run.at(9.0, T, acted("a"));
        let stop = run.at(10.0, T, STOP);
        run.debounce(&stop, 11.5);
        assert!(run.at(12.0, T, START).notes.is_empty());
    }

    #[test]
    fn duplicate_and_unmatched_events_are_ignored() {
        let mut run = Run::new();
        // End without start: never hooked, nothing happens and no turn opens.
        assert_eq!(run.at(0.0, T, STOP), Effects::default());
        assert_eq!(
            run.at(0.0, T, Input::Hook(TurnEvent::StopFailure)),
            Effects::default()
        );
        assert_eq!(
            run.at(0.0, T, Input::Hook(TurnEvent::SessionEnd)),
            Effects::default()
        );
        assert_eq!(run.phase(T), None);
        run.at(1.0, T, START);
        run.at(2.0, T, acted("a"));
        let stop = run.at(3.0, T, STOP);
        // A duplicate Stop neither rearms nor finishes.
        assert_eq!(run.at(3.2, T, STOP), Effects::default());
        assert_eq!(
            notes(&run.debounce(&stop, 4.5)),
            [("a", PipHookTurn::Finished)]
        );
        assert_eq!(run.at(5.0, T, STOP), Effects::default());
    }

    #[test]
    fn t8_a_transport_never_hooked_or_restarted_infers_no_turn_from_actions() {
        let mut run = Run::new();
        // An ordinary action (or a daemon restart's lost start) opens nothing.
        assert_eq!(run.at(0.0, T, acted("a")), Effects::default());
        assert_eq!(run.at(1.0, T, Input::Action), Effects::default());
        assert_eq!(run.phase(T), None);
        assert!(!run.turns.defers_eviction(T, run.t0, LEASE));
    }

    #[test]
    fn session_end_tears_down_every_label_the_transport_ever_used() {
        let mut run = Run::new();
        run.at(0.0, T, START);
        run.at(1.0, T, acted("a"));
        let stop = run.at(2.0, T, STOP);
        run.debounce(&stop, 3.5);
        run.at(10.0, T, START);
        run.at(11.0, T, acted("b"));
        let stop = run.at(12.0, T, STOP);
        // SessionEnd lands inside the debounce (-p mode exits at once): one
        // finale, as at end_session, never a second.
        assert_eq!(
            notes(&run.at(12.1, T, Input::Hook(TurnEvent::SessionEnd))),
            [
                ("a", PipHookTurn::End { finished: true }),
                ("b", PipHookTurn::End { finished: true })
            ]
        );
        assert_eq!(run.phase(T), None);
        assert!(run.debounce(&stop, 13.5).notes.is_empty());
        // Mid-turn (interrupted, then the client exits): as a closed
        // connection.
        run.at(20.0, T, START);
        run.at(21.0, T, acted("c"));
        assert_eq!(
            notes(&run.at(22.0, T, Input::Hook(TurnEvent::SessionEnd))),
            [("c", PipHookTurn::End { finished: false })]
        );
    }

    #[test]
    fn two_transports_with_identical_labels_stay_independent() {
        let mut run = Run::new();
        let (a, b) = ("runtime-1:proxy", "runtime-2:proxy");
        run.at(0.0, a, START);
        run.at(0.0, b, START);
        assert_eq!(
            notes(&run.at(1.0, a, acted("runtime-1:alpha"))),
            [("runtime-1:alpha", PipHookTurn::Open)]
        );
        assert_eq!(
            notes(&run.at(1.0, b, acted("runtime-2:alpha"))),
            [("runtime-2:alpha", PipHookTurn::Open)]
        );
        let stop = run.at(2.0, a, STOP);
        let Some((Timer::Debounce(gen), _)) = stop.timer else {
            panic!()
        };
        assert_eq!(
            notes(&run.at(3.5, a, Input::DebounceDue(gen))),
            [("runtime-1:alpha", PipHookTurn::Finished)]
        );
        assert_eq!(run.phase(b), Some(Phase::Open));
        assert!(run
            .turns
            .defers_eviction(b, run.t0 + Duration::from_secs(4), LEASE));
        assert!(!run
            .turns
            .defers_eviction(a, run.t0 + Duration::from_secs(4), LEASE));
    }

    #[test]
    fn t6_eviction_defers_only_inside_an_open_turns_lease() {
        let mut run = Run::new();
        run.at(0.0, T, START);
        let t0 = run.t0;
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        assert!(run.turns.defers_eviction(T, at(299), LEASE));
        assert!(!run.turns.defers_eviction(T, at(300), LEASE));
        assert!(!run.turns.defers_eviction("other", at(1), LEASE));
        let stop = run.at(10.0, T, STOP);
        // Stopping still defers; closed does not.
        assert!(run.turns.defers_eviction(T, at(11), LEASE));
        run.debounce(&stop, 11.5);
        assert!(!run.turns.defers_eviction(T, at(12), LEASE));
    }
}
