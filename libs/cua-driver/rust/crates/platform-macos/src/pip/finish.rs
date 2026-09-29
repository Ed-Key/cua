//! What a session has finished, and the finished state (the "finale") a
//! panel plays when the session goes idle or ends.
//!
//! A window is FINISHED for a session when a `verify_state` on it was fully
//! satisfied after the session last acted in it, or when the session
//! finished after last acting in it, unless its latest verification (after
//! that action) was not satisfied. Finished back windows collapse to chips.
//!
//! The finale shows the session's verified claims not yet shown as a
//! checklist (latest status per label, the five most recent, oldest first;
//! only a satisfied claim gets a check), or, with none, the windows it
//! touched as a row of chips. Rows come in one by one, then the panel holds
//! and fades.
//!
//! ## Event ordering
//!
//! Every event carries its event time: wall-clock ms stamped where it
//! happened (an action when it is pushed, a verification when its
//! predicates were last observed). Events reach the main queue out of that
//! order (a frame waits for its capture, which is serialized across
//! sessions; a verification can run 10 s), so each kind may change only
//! what this table allows, and evidence is read latest by event time:
//!
//! | Event | Verdicts (per window) | Claims (per label) | Lifecycle: finale, idle deadline, user close | Picture and stack |
//! |---|---|---|---|---|
//! | Action note (on push) | records the action at its event ms; ends a session finish older than it | none | only if newer than the last applied action: cancels the finale, restarts the idle deadline, lifts a user close | none |
//! | Captured frame | resolves a pid-only action to its window at the action's event ms (idempotent; never ends a newer session finish) | none | none when its action was already applied (dedupe by event ms; the note always is, unless the panel did not exist yet); otherwise as its action note | still, header, front card, back items, touched windows |
//! | Verification | latest by event ms per window (an older one never wins) | latest by event ms per label, kept across finales; an older one is ignored | if it brought news: replays a playing finale, or owes a finale when the stretch's finale is over | chips and cards follow the verdicts |
//! | Idle timer | the session finishes (now) | none | the finale is due once per stretch: plays if the panel is up and not closed (or owed and may show), else settles silently | the panel fades after |
//! | `end_session` | the session finishes (now) | none | as the idle timer, then the panel closes | the panel leaves the live set |
//! | Finale timer | none | marks what was displayed as shown; the watermarks stay | ends only the finale of its own generation | the panel fades |
//! | User close | none | none | closed until an action newer than the close; no finale plays | the panel hides (an ending one closes) |
//!
//! Everything here is pure (unit tested, one test per row).

use std::collections::HashMap;
use std::time::Duration;

use super::Tag;

/// Checklist rows at most.
pub(super) const CHECKLIST_ROWS: usize = 5;
/// Chips in the no-claims finale row at most.
pub(super) const CHIP_ROW: usize = 5;
/// Labels whose watermark a session keeps.
// ponytail: past this many distinct labels the oldest watermark is dropped
// (an older check of that label could then show again); raise it if agents
// ever verify that many different things in one session.
const CLAIM_MEMORY: usize = 256;
/// Each row starts this long after the one above it.
pub(super) const STAGGER: Duration = Duration::from_millis(80);
/// A row fades and slides in over this long; its mark follows.
pub(super) const ROW_IN: Duration = Duration::from_millis(150);
/// A mark draws (and pops) over this long.
pub(super) const MARK_IN: Duration = Duration::from_millis(200);
/// The finished state stays up this long once every row is in.
pub(super) const HOLD: Duration = Duration::from_millis(2500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Claim {
    pub(super) label: String,
    /// `Some(true)` satisfied, `Some(false)` unsatisfied, `None` unknown.
    pub(super) satisfied: Option<bool>,
}

/// A label's latest claim by event time.
struct ClaimRecord {
    at_ms: u64,
    /// Arrival order, breaking ties between equal event times.
    seq: u64,
    satisfied: Option<bool>,
    /// A finale displayed it.
    shown: bool,
}

/// A session's evidence about its windows.
#[derive(Default)]
pub(super) struct Verdicts {
    /// Window -> when the session last acted in it.
    acted: HashMap<u32, u64>,
    /// Pid -> when the session last acted on it without naming a window
    /// (reopens every window of that app).
    acted_pid: HashMap<i32, u64>,
    /// Window -> the pid that owns it, as far as the session has seen.
    pids: HashMap<u32, i32>,
    /// Window -> its latest verification by event time, and whether fully
    /// satisfied.
    verified: HashMap<u32, (u64, bool)>,
    /// When the session finished, until an action newer than that.
    session_done: Option<u64>,
    /// Label -> its latest claim by event time. Never cleared: a finale only
    /// marks what it displayed as shown.
    claims: HashMap<String, ClaimRecord>,
    next_seq: u64,
    /// Windows acted in, most recent action first, with their titles and
    /// that action's event time.
    touched: Vec<(Tag, String, u64)>,
    /// Touched windows up to this event time were shown by a finale.
    touched_shown: u64,
}

impl Verdicts {
    /// The session acted on `target` at `at_ms`: a window, or (pid only)
    /// every window of that app. A session finish older than the action
    /// ends: the session is working again.
    pub(super) fn record_action(&mut self, target: Tag, at_ms: u64) {
        let bump = |at: &mut u64| *at = (*at).max(at_ms);
        match target {
            (pid, Some(window)) => {
                bump(self.acted.entry(window).or_default());
                if let Some(pid) = pid {
                    self.pids.insert(window, pid);
                }
            }
            (Some(pid), None) => bump(self.acted_pid.entry(pid).or_default()),
            (None, None) => {}
        }
        if self.session_done.is_some_and(|done| at_ms > done) {
            self.session_done = None;
        }
    }

    /// A captured frame of the session acting in `tag` (titled `title`) at
    /// `at_ms`: its window is resolved (for a pid-only action) and touched.
    pub(super) fn act(&mut self, tag: Tag, title: &str, at_ms: u64) {
        self.record_action(tag, at_ms);
        let at_ms = self
            .touched
            .iter()
            .find(|(touched, _, _)| *touched == tag)
            .map_or(at_ms, |(_, _, at)| (*at).max(at_ms));
        self.touched.retain(|(touched, _, _)| *touched != tag);
        let index = self.touched.partition_point(|(_, _, at)| *at > at_ms);
        self.touched.insert(index, (tag, title.to_owned(), at_ms));
        self.touched.truncate(CHIP_ROW);
    }

    /// A `verify_state` on `pid`'s `window` whose predicates were observed
    /// at `at_ms`. Whether it brought news (a verdict or claim at least as
    /// new as what is known); an older verification changes nothing.
    pub(super) fn verify(
        &mut self,
        pid: i32,
        window: u32,
        at_ms: u64,
        satisfied: bool,
        claims: Vec<Claim>,
    ) -> bool {
        self.pids.insert(window, pid);
        let mut news = false;
        if self
            .verified
            .get(&window)
            .is_none_or(|(previous, _)| at_ms >= *previous)
        {
            self.verified.insert(window, (at_ms, satisfied));
            news = true;
        }
        for claim in claims {
            if self
                .claims
                .get(&claim.label)
                .is_some_and(|record| record.at_ms > at_ms)
            {
                continue;
            }
            self.next_seq += 1;
            self.claims.insert(
                claim.label,
                ClaimRecord {
                    at_ms,
                    seq: self.next_seq,
                    satisfied: claim.satisfied,
                    shown: false,
                },
            );
            news = true;
        }
        while self.claims.len() > CLAIM_MEMORY {
            let oldest = self
                .claims
                .iter()
                .min_by_key(|(_, record)| (record.at_ms, record.seq))
                .map(|(label, _)| label.clone());
            if let Some(oldest) = oldest {
                self.claims.remove(&oldest);
            }
        }
        news
    }

    /// The session finished (went idle, or ended) at `at_ms`.
    pub(super) fn finish_session(&mut self, at_ms: u64) {
        self.session_done = Some(at_ms);
    }

    /// The finished rule (see the module docs).
    pub(super) fn finished(&self, window: u32) -> bool {
        let by_pid = self
            .pids
            .get(&window)
            .and_then(|pid| self.acted_pid.get(pid))
            .copied()
            .unwrap_or(0);
        let acted = self.acted.get(&window).copied().unwrap_or(0).max(by_pid);
        match self.verified.get(&window) {
            // The latest evidence since the last action decides.
            Some(&(at, satisfied)) if at >= acted => satisfied,
            _ => self.session_done.is_some_and(|done| done >= acted),
        }
    }

    /// The claims not yet shown: each label's latest status by event time,
    /// the `CHECKLIST_ROWS` most recent, oldest first.
    fn checklist(&self) -> Vec<Claim> {
        let mut rows: Vec<(&String, &ClaimRecord)> = self
            .claims
            .iter()
            .filter(|(_, record)| !record.shown)
            .collect();
        rows.sort_by_key(|(_, record)| (record.at_ms, record.seq));
        let skip = rows.len().saturating_sub(CHECKLIST_ROWS);
        rows.into_iter()
            .skip(skip)
            .map(|(label, record)| Claim {
                label: label.clone(),
                satisfied: record.satisfied,
            })
            .collect()
    }

    /// What the finale shows now.
    pub(super) fn finale(&self) -> Finale {
        let rows = self.checklist();
        if !rows.is_empty() {
            return Finale::Checklist(rows);
        }
        Finale::Chips(
            self.touched
                .iter()
                .filter(|(_, _, at)| *at > self.touched_shown)
                .map(|(tag, title, _)| FinaleChip {
                    tag: *tag,
                    title: title.clone(),
                    finished: tag.1.is_some_and(|window| self.finished(window)),
                })
                .collect(),
        )
    }

    /// A finale ran its course: what it displayed is shown. The watermarks
    /// stay, so older evidence arriving later still loses.
    pub(super) fn finale_shown(&mut self) {
        for record in self.claims.values_mut() {
            record.shown = true;
        }
        if let Some((_, _, newest)) = self.touched.first() {
            self.touched_shown = self.touched_shown.max(*newest);
        }
    }

    /// Windows the session touched (most recent first), with titles.
    pub(super) fn touched(&self) -> impl Iterator<Item = (Tag, String)> + '_ {
        self.touched
            .iter()
            .map(|(tag, title, _)| (*tag, title.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FinaleChip {
    pub(super) tag: Tag,
    pub(super) title: String,
    /// Only a finished window's chip gets a check.
    pub(super) finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Finale {
    Checklist(Vec<Claim>),
    Chips(Vec<FinaleChip>),
}

impl Finale {
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Finale::Checklist(_) => "checklist",
            Finale::Chips(_) => "chips",
        }
    }

    pub(super) fn len(&self) -> usize {
        match self {
            Finale::Checklist(rows) => rows.len(),
            Finale::Chips(chips) => chips.len(),
        }
    }

    /// One line per row, for logs: `satisfied: text area holds "hi"`,
    /// `finished: Notes`.
    pub(super) fn log_rows(&self) -> Vec<String> {
        match self {
            Finale::Checklist(rows) => rows
                .iter()
                .map(|row| {
                    let status = match row.satisfied {
                        Some(true) => "satisfied",
                        Some(false) => "unsatisfied",
                        None => "unknown",
                    };
                    format!("{status}: {}", row.label)
                })
                .collect(),
            Finale::Chips(chips) => chips
                .iter()
                .map(|chip| {
                    let status = if chip.finished { "finished" } else { "plain" };
                    format!("{status}: {}", chip.title)
                })
                .collect(),
        }
    }
}

/// When row `index` of a finale starts to come in, and when its mark
/// starts to draw (as the row lands), from the start of the finale.
pub(super) fn row_timing(index: usize) -> (Duration, Duration) {
    let appear = STAGGER * index as u32;
    (appear, appear + ROW_IN)
}

/// How long a finale of `rows` rows stays up before the panel fades.
pub(super) fn finale_duration(rows: usize) -> Duration {
    match rows.checked_sub(1) {
        Some(last) => row_timing(last).1 + MARK_IN + HOLD,
        None => HOLD,
    }
}

/// A panel's lifecycle (the table's lifecycle column): the last action
/// applied, whether the user closed the panel, and the finale (playing,
/// played for this stretch of activity, owed to late news, and which
/// schedule is current).
#[derive(Default)]
pub(super) struct Lifecycle {
    /// Event time of the newest action applied.
    last_action_ms: u64,
    /// Closed by the user since that action.
    closed: bool,
    generation: u64,
    playing: bool,
    played: bool,
    /// News arrived after the finale for this stretch was over (a long
    /// verification): it is owed a finale of its own.
    late: bool,
}

impl Lifecycle {
    /// An action at `at_ms` (its note, or a frame whose note never reached
    /// a panel). Only an action newer than the last applied one resumes the
    /// session: the finale stops (its end timer goes stale), the user close
    /// lifts, and the caller restarts the idle deadline; then `Some`
    /// (whether a finale was playing). An action already applied: `None`,
    /// nothing changes.
    pub(super) fn resume(&mut self, at_ms: u64) -> Option<bool> {
        if at_ms <= self.last_action_ms {
            return None;
        }
        self.last_action_ms = at_ms;
        let was = self.playing;
        self.closed = false;
        self.playing = false;
        self.played = false;
        self.late = false;
        self.generation += 1;
        Some(was)
    }

    /// The user closed the panel: it stays closed, and no finale plays,
    /// until a newer action.
    pub(super) fn close(&mut self) {
        self.closed = true;
    }

    pub(super) fn closed(&self) -> bool {
        self.closed
    }

    /// Whether the session just finished: it is no longer active (idle or
    /// ended) and has not finished since its last action, or news is owed
    /// a finale.
    pub(super) fn due(&self, active: bool) -> bool {
        !active && !self.playing && (!self.played || self.late)
    }

    /// A verification brought news. If the finale for this stretch is
    /// already over, the news is late (see `due`): whether it is.
    pub(super) fn news_arrived(&mut self) -> bool {
        self.late |= self.played && !self.playing;
        self.late
    }

    /// News is waiting for a finale that has not shown it.
    pub(super) fn late(&self) -> bool {
        self.late
    }

    /// The session finished. The finale plays only if `visible` (the panel
    /// is on screen, or owed and may show; something to show) and the user
    /// has not closed the panel: then the generation for its end timer.
    pub(super) fn start(&mut self, visible: bool) -> Option<u64> {
        self.played = true;
        self.late = false;
        if !visible || self.closed {
            return None;
        }
        self.generation += 1;
        self.playing = true;
        Some(self.generation)
    }

    /// The finale's content changed while it plays (news landed): it starts
    /// over under a new generation, and the old end timer goes stale.
    /// `None` when none is playing.
    pub(super) fn restart(&mut self) -> Option<u64> {
        if !self.playing {
            return None;
        }
        self.generation += 1;
        Some(self.generation)
    }

    /// The end timer of `generation` fired: whether that finale was still
    /// playing (and now is over).
    pub(super) fn end(&mut self, generation: u64) -> bool {
        if self.playing && generation == self.generation {
            self.playing = false;
            return true;
        }
        false
    }

    pub(super) fn playing(&self) -> bool {
        self.playing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Tag = (Some(1), Some(10));
    const B: Tag = (Some(2), Some(20));

    fn claim(label: &str, satisfied: Option<bool>) -> Claim {
        Claim {
            label: label.to_owned(),
            satisfied,
        }
    }

    fn checklist(verdicts: &Verdicts) -> Vec<Claim> {
        match verdicts.finale() {
            Finale::Checklist(rows) => rows,
            Finale::Chips(_) => Vec::new(),
        }
    }

    // ── Row: action note ─────────────────────────────────────────────────

    #[test]
    fn row_action_note_resumes_only_when_newer_than_the_last_action() {
        let mut life = Lifecycle::default();
        assert_eq!(life.resume(100), Some(false));
        assert_eq!(life.resume(100), None, "the same action again");
        assert_eq!(life.resume(90), None, "an older action");
        // A newer action stops a playing finale and lifts a close.
        let finale = life.start(true).unwrap();
        life.close();
        assert_eq!(life.resume(200), Some(true));
        assert!(!life.playing() && !life.closed());
        assert!(!life.end(finale), "its end timer is stale");
        // In the verdicts: it ends an older session finish.
        let mut verdicts = Verdicts::default();
        verdicts.record_action(A, 100);
        verdicts.finish_session(150);
        assert!(verdicts.finished(10));
        verdicts.record_action(B, 200);
        assert!(!verdicts.finished(10), "working again");
    }

    #[test]
    fn an_action_whose_frame_was_coalesced_still_reopens_its_window() {
        // A verified; the action in A is only pushed (its frame is replaced
        // by B's before capture): the note alone reopens A.
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.verify(1, 10, 200, true, vec![]);
        assert!(verdicts.finished(10));
        verdicts.record_action(A, 300);
        verdicts.record_action(B, 310);
        verdicts.act(B, "Mail", 310);
        assert!(!verdicts.finished(10));
    }

    #[test]
    fn a_pid_only_action_reopens_that_apps_windows() {
        let mut verdicts = Verdicts::default();
        verdicts.verify(1, 10, 200, true, vec![]);
        verdicts.verify(2, 20, 200, true, vec![]);
        verdicts.record_action((Some(1), None), 300);
        assert!(!verdicts.finished(10), "app 1 acted on after the check");
        assert!(verdicts.finished(20), "another app is untouched");
        verdicts.verify(1, 10, 400, true, vec![]);
        assert!(verdicts.finished(10));
    }

    // ── Row: captured frame ──────────────────────────────────────────────

    #[test]
    fn row_captured_frame_of_an_applied_action_changes_no_lifecycle() {
        // The capture queue is backlogged past the idle timer: the action
        // note (t=1000) resumed the session, it went idle and its finale
        // started at 9000, then the frame of that same action lands.
        let mut life = Lifecycle::default();
        let mut verdicts = Verdicts::default();
        assert!(life.resume(1_000).is_some());
        verdicts.record_action(A, 1_000);
        verdicts.finish_session(9_000);
        let finale = life.start(true).unwrap();
        // The frame: no lifecycle change, and the session stays finished.
        assert_eq!(life.resume(1_000), None);
        verdicts.act(A, "Notes", 1_000);
        assert!(life.playing(), "the finale keeps playing");
        assert!(verdicts.finished(10), "the window stays finished");
        assert!(life.end(finale));
        assert!(!life.due(false), "no second active stretch");
        // A frame whose action note never reached a panel (it did not exist
        // yet) is that action, once.
        let mut fresh = Lifecycle::default();
        assert_eq!(fresh.resume(1_000), Some(false));
    }

    #[test]
    fn touched_windows_are_most_recent_first_deduped_and_capped() {
        let mut verdicts = Verdicts::default();
        for window in 0..8u32 {
            verdicts.act(
                (Some(1), Some(window)),
                &format!("w{window}"),
                window as u64,
            );
        }
        verdicts.act((Some(1), Some(5)), "w5", 50);
        let titles: Vec<String> = verdicts.touched().map(|(_, title)| title).collect();
        assert_eq!(titles, ["w5", "w7", "w6", "w4", "w3"]);
    }

    // ── Row: verification ────────────────────────────────────────────────

    #[test]
    fn a_satisfied_verification_after_the_last_action_finishes_a_window() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        assert!(!verdicts.finished(10));
        verdicts.verify(1, 10, 200, true, vec![]);
        assert!(verdicts.finished(10));
        // Acting in it again reopens it; a failed check keeps it open.
        verdicts.act(A, "Notes", 300);
        assert!(!verdicts.finished(10));
        verdicts.verify(1, 10, 400, false, vec![]);
        assert!(!verdicts.finished(10));
        verdicts.verify(1, 10, 500, true, vec![]);
        assert!(verdicts.finished(10));
    }

    #[test]
    fn row_verification_older_evidence_never_wins_even_after_a_finale() {
        // A newer unsatisfied check is displayed and marked shown; then an
        // older satisfied check (delayed by its screenshot) lands.
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        assert!(verdicts.verify(1, 10, 200, false, vec![claim("saved", Some(false))]));
        assert_eq!(checklist(&verdicts), [claim("saved", Some(false))]);
        verdicts.finale_shown();
        assert!(!verdicts.verify(1, 10, 150, true, vec![claim("saved", Some(true))]));
        assert!(!verdicts.finished(10));
        assert!(checklist(&verdicts).is_empty(), "no green row");
        // Newer evidence after the finale is news and shows next time.
        assert!(verdicts.verify(1, 10, 300, true, vec![claim("saved", Some(true))]));
        assert!(verdicts.finished(10));
        assert_eq!(checklist(&verdicts), [claim("saved", Some(true))]);
    }

    #[test]
    fn the_checklist_keeps_the_latest_status_of_the_five_most_recent_labels() {
        let mut verdicts = Verdicts::default();
        for (at, row) in [
            claim("one", Some(true)),
            claim("two", Some(false)),
            claim("three", Some(true)),
            claim("two", Some(true)), // re-checked: now satisfied
            claim("four", None),
            claim("five", Some(true)),
            claim("six", Some(true)),
        ]
        .into_iter()
        .enumerate()
        {
            verdicts.verify(1, 10, at as u64 * 10, true, vec![row]);
        }
        assert_eq!(
            checklist(&verdicts),
            [
                claim("three", Some(true)),
                claim("two", Some(true)),
                claim("four", None),
                claim("five", Some(true)),
                claim("six", Some(true)),
            ],
            "deduped, most recent five, oldest first; `one` fell off"
        );
    }

    #[test]
    fn claims_and_verdicts_arriving_out_of_order_read_the_latest_by_event_time() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.verify(1, 10, 200, false, vec![claim("done", Some(false))]);
        verdicts.verify(1, 10, 150, true, vec![claim("done", Some(true))]);
        assert!(!verdicts.finished(10));
        assert_eq!(checklist(&verdicts), [claim("done", Some(false))]);
        verdicts.verify(2, 20, 120, true, vec![claim("early", Some(true))]);
        let labels: Vec<String> = checklist(&verdicts)
            .into_iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(labels, ["early", "done"], "rows in event order");
        // The frame of the action (pushed at 100) landing after the check
        // (at 200) changes nothing.
        let mut late_frame = Verdicts::default();
        late_frame.verify(1, 10, 200, true, vec![]);
        late_frame.act(A, "Notes", 100);
        assert!(late_frame.finished(10));
    }

    #[test]
    fn news_replays_a_playing_finale_or_is_owed_one() {
        let mut life = Lifecycle::default();
        life.resume(100);
        assert_eq!(life.restart(), None, "nothing playing");
        assert!(!life.news_arrived(), "before the finale: it will show it");
        let first = life.start(true).unwrap();
        let second = life.restart().unwrap();
        assert!(!life.end(first), "replayed: the first schedule is stale");
        assert!(life.end(second));
        assert!(!life.due(false), "shown already");
        assert!(life.news_arrived(), "after it: owed");
        assert!(life.late() && life.due(false));
        life.start(true).unwrap();
        assert!(!life.late() && !life.due(false));
    }

    // ── Row: idle timer ──────────────────────────────────────────────────

    #[test]
    fn row_idle_timer_finishes_once_per_stretch_and_plays_only_if_visible() {
        let mut life = Lifecycle::default();
        life.resume(100);
        assert!(!life.due(true), "still active");
        assert!(life.due(false));
        // Hidden panel: settles silently, no retry loop.
        assert_eq!(life.start(false), None);
        assert!(!life.due(false));
        // Next stretch: plays.
        life.resume(200);
        assert!(life.due(false));
        assert!(life.start(true).is_some());
        // The session finishes in the verdicts: windows count as finished,
        // unless their latest check failed.
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.act(B, "Mail", 110);
        verdicts.verify(2, 20, 120, false, vec![]);
        verdicts.finish_session(200);
        assert!(verdicts.finished(10));
        assert!(!verdicts.finished(20));
    }

    #[test]
    fn with_claims_the_finale_is_a_checklist_and_without_them_chips() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.act(B, "Mail", 110);
        verdicts.verify(2, 20, 120, false, vec![]);
        verdicts.finish_session(200);
        assert_eq!(
            verdicts.finale(),
            Finale::Chips(vec![
                FinaleChip {
                    tag: B,
                    title: "Mail".into(),
                    finished: false
                },
                FinaleChip {
                    tag: A,
                    title: "Notes".into(),
                    finished: true
                },
            ]),
            "a check only on finished windows"
        );
        verdicts.verify(
            1,
            10,
            300,
            false,
            vec![claim("text area holds \"hi\"", Some(false))],
        );
        let finale = verdicts.finale();
        assert_eq!(finale.kind(), "checklist");
        assert_eq!(finale.log_rows(), ["unsatisfied: text area holds \"hi\""]);
    }

    // ── Row: end_session ─────────────────────────────────────────────────

    #[test]
    fn row_end_session_plays_what_is_owed_and_nothing_twice() {
        // Ended mid-stretch: the finale is due.
        let mut life = Lifecycle::default();
        life.resume(100);
        assert!(life.due(false));
        // Ended after the idle finale already played: nothing more is due...
        let finale = life.start(true).unwrap();
        assert!(life.end(finale));
        assert!(!life.due(false));
        // ...unless a verification brought news after it.
        life.news_arrived();
        assert!(life.due(false));
        // Closed by the user: a due finale settles without playing.
        life.close();
        assert_eq!(life.start(true), None);
    }

    // ── Row: finale timer ────────────────────────────────────────────────

    #[test]
    fn row_finale_timer_ends_only_its_own_finale_and_marks_what_it_showed() {
        // A cancelled finale's timer cannot end the next one.
        let mut life = Lifecycle::default();
        life.resume(100);
        let idle = life.start(true).unwrap();
        life.resume(200);
        let at_end = life.start(true).unwrap();
        assert!(!life.end(idle));
        assert!(life.playing());
        assert!(life.end(at_end));
        // Shown: the next finale covers only newer work.
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.verify(1, 10, 150, true, vec![claim("saved", Some(true))]);
        verdicts.finale_shown();
        assert_eq!(verdicts.finale(), Finale::Chips(vec![]));
        verdicts.act(B, "Mail", 300);
        let Finale::Chips(chips) = verdicts.finale() else {
            panic!("no new claims: chips");
        };
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].tag, B);
        // A late frame of an action before the finale is not new work.
        verdicts.finale_shown();
        verdicts.act(A, "Notes", 100);
        assert_eq!(verdicts.finale(), Finale::Chips(vec![]));
    }

    #[test]
    fn rows_come_in_80_ms_apart_and_the_panel_holds_before_fading() {
        assert_eq!(row_timing(0), (Duration::ZERO, ROW_IN));
        assert_eq!(
            row_timing(3),
            (Duration::from_millis(240), Duration::from_millis(390))
        );
        // Five rows: last row in at 320 ms, its mark lands at 470 + 200 ms,
        // then the 2.5 s hold.
        assert_eq!(finale_duration(5), Duration::from_millis(3170));
        assert_eq!(finale_duration(0), HOLD);
    }

    // ── Row: user close ──────────────────────────────────────────────────

    #[test]
    fn row_user_close_holds_until_a_newer_action() {
        let mut life = Lifecycle::default();
        life.resume(100);
        life.close();
        assert!(life.closed());
        // The frame of the action before the close does not reopen it.
        assert_eq!(life.resume(100), None);
        assert!(life.closed());
        assert_eq!(life.start(true), None, "no finale while closed");
        // A newer action does.
        assert!(life.resume(200).is_some());
        assert!(!life.closed());
    }
}
