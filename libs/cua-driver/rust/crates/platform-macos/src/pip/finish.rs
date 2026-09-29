//! What a session has finished, and the finished state (the "finale") a
//! panel plays when the session goes idle or ends.
//!
//! A window is FINISHED for a session when a `verify_state` on it was fully
//! satisfied after the session last acted in it, or when the session
//! finished after last acting in it, unless its latest verification (after
//! that action) was not satisfied. Finished back windows collapse to chips.
//!
//! The finale shows the session's recent verified claims as a checklist
//! (latest status per label, the five most recent, oldest first; only a
//! satisfied claim gets a check), or, with no claims, the windows it touched
//! as a row of chips. Rows come in one by one, then the panel holds and
//! fades. A new action cancels it.
//!
//! Everything here is pure (unit tested). Timestamps are wall-clock ms from
//! the frames and verifications themselves, so the order they reach the
//! main queue in (a frame waits for its capture, a verification does not)
//! never changes the answer.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::Tag;

/// Checklist rows at most.
pub(super) const CHECKLIST_ROWS: usize = 5;
/// Chips in the no-claims finale row at most.
pub(super) const CHIP_ROW: usize = 5;
/// Claims remembered per session.
const CLAIM_MEMORY: usize = 32;
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

/// A session's evidence about its windows.
#[derive(Default)]
pub(super) struct Verdicts {
    /// Window -> when the session last acted in it.
    acted: HashMap<u32, u64>,
    /// Window -> when it was last verified, and whether fully satisfied.
    verified: HashMap<u32, (u64, bool)>,
    /// When the session finished, until it acts again.
    session_done: Option<u64>,
    /// Recent claims, oldest first.
    claims: VecDeque<Claim>,
    /// Windows acted in since the last finale, most recent first, with
    /// their titles.
    touched: Vec<(Tag, String)>,
}

impl Verdicts {
    /// The session acted in `tag` (titled `title`) at `at_ms`. Also ends a
    /// session-level finish: the session is working again.
    pub(super) fn act(&mut self, tag: Tag, title: &str, at_ms: u64) {
        if let Some(window) = tag.1 {
            let acted = self.acted.entry(window).or_default();
            *acted = (*acted).max(at_ms);
        }
        self.session_done = None;
        self.touched.retain(|(touched, _)| *touched != tag);
        self.touched.insert(0, (tag, title.to_owned()));
        self.touched.truncate(CHIP_ROW);
    }

    /// A `verify_state` on `window` completed at `at_ms`.
    pub(super) fn verify(&mut self, window: u32, at_ms: u64, satisfied: bool, claims: Vec<Claim>) {
        if self
            .verified
            .get(&window)
            .is_none_or(|(previous, _)| at_ms >= *previous)
        {
            self.verified.insert(window, (at_ms, satisfied));
        }
        self.claims.extend(claims);
        while self.claims.len() > CLAIM_MEMORY {
            self.claims.pop_front();
        }
    }

    /// The session finished (went idle, or ended) at `at_ms`.
    pub(super) fn finish_session(&mut self, at_ms: u64) {
        self.session_done = Some(at_ms);
    }

    /// The finished rule (see the module docs).
    pub(super) fn finished(&self, window: u32) -> bool {
        let acted = self.acted.get(&window).copied().unwrap_or(0);
        match self.verified.get(&window) {
            // The latest evidence since the last action decides.
            Some(&(at, satisfied)) if at >= acted => satisfied,
            _ => self.session_done.is_some_and(|done| done >= acted),
        }
    }

    /// What the finale shows now.
    pub(super) fn finale(&self) -> Finale {
        let rows = checklist(&self.claims);
        if !rows.is_empty() {
            return Finale::Checklist(rows);
        }
        Finale::Chips(
            self.touched
                .iter()
                .map(|(tag, title)| FinaleChip {
                    tag: *tag,
                    title: title.clone(),
                    finished: tag.1.is_some_and(|window| self.finished(window)),
                })
                .collect(),
        )
    }

    /// A finale ran its course: the next one covers only newer work.
    pub(super) fn finale_shown(&mut self) {
        self.claims.clear();
        self.touched.clear();
    }

    /// Windows touched since the last finale.
    pub(super) fn touched(&self) -> impl Iterator<Item = &(Tag, String)> {
        self.touched.iter()
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

/// The checklist for `claims` (oldest first): each label once with its
/// latest status, the `CHECKLIST_ROWS` most recently claimed, oldest first.
pub(super) fn checklist<'a>(claims: impl IntoIterator<Item = &'a Claim>) -> Vec<Claim> {
    let claims: Vec<&Claim> = claims.into_iter().collect();
    let mut rows: Vec<Claim> = Vec::new();
    for claim in claims.into_iter().rev() {
        if rows.len() == CHECKLIST_ROWS {
            break;
        }
        if !rows.iter().any(|row| row.label == claim.label) {
            rows.push(claim.clone());
        }
    }
    rows.reverse();
    rows
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

/// Whether a panel's finale is playing, has played for the current stretch
/// of activity, and which schedule is current.
#[derive(Default)]
pub(super) struct FinaleState {
    generation: u64,
    playing: bool,
    played: bool,
}

impl FinaleState {
    /// Whether the session just finished: it is no longer active (idle or
    /// ended) and has not finished since its last action.
    pub(super) fn due(&self, active: bool) -> bool {
        !active && !self.playing && !self.played
    }

    /// The session finished. The finale plays only if `visible` (the panel
    /// is on screen, not closed by the user, and has something to show):
    /// then the generation to hand to its end timer.
    pub(super) fn start(&mut self, visible: bool) -> Option<u64> {
        self.played = true;
        if !visible {
            return None;
        }
        self.generation += 1;
        self.playing = true;
        Some(self.generation)
    }

    /// The session acted: stop a playing finale (whether one was playing)
    /// and let the next idle play again. Any pending end timer goes stale.
    pub(super) fn cancel(&mut self) -> bool {
        let was = self.playing;
        self.playing = false;
        self.played = false;
        self.generation += 1;
        was
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

    #[test]
    fn a_satisfied_verification_after_the_last_action_finishes_a_window() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        assert!(!verdicts.finished(10));
        verdicts.verify(10, 200, true, vec![]);
        assert!(verdicts.finished(10));
        // Acting in it again reopens it; a failed check keeps it open.
        verdicts.act(A, "Notes", 300);
        assert!(!verdicts.finished(10));
        verdicts.verify(10, 400, false, vec![]);
        assert!(!verdicts.finished(10));
        verdicts.verify(10, 500, true, vec![]);
        assert!(verdicts.finished(10));
    }

    #[test]
    fn arrival_order_does_not_matter_only_timestamps() {
        // The action's frame (pushed at 100) reaches the main queue after the
        // verification (pushed at 200), because the frame waited for its
        // capture: the window is still finished.
        let mut verdicts = Verdicts::default();
        verdicts.verify(10, 200, true, vec![]);
        verdicts.act(A, "Notes", 100);
        assert!(verdicts.finished(10));
        // An older verification landing late never overrides a newer one.
        verdicts.verify(10, 150, false, vec![]);
        assert!(verdicts.finished(10));
    }

    #[test]
    fn a_finished_session_finishes_its_windows_unless_a_check_failed() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.act(B, "Mail", 110);
        verdicts.verify(20, 120, false, vec![]);
        verdicts.finish_session(200);
        assert!(
            verdicts.finished(10),
            "never verified: finished with the session"
        );
        assert!(!verdicts.finished(20), "its latest check failed");
        // The session acting again undoes the session-level finish.
        verdicts.act(B, "Mail", 300);
        assert!(!verdicts.finished(10));
    }

    #[test]
    fn the_checklist_keeps_the_latest_status_of_the_five_most_recent_labels() {
        let claims = [
            claim("one", Some(true)),
            claim("two", Some(false)),
            claim("three", Some(true)),
            claim("two", Some(true)), // re-checked: now satisfied
            claim("four", None),
            claim("five", Some(true)),
            claim("six", Some(true)),
        ];
        let rows = checklist(&claims);
        assert_eq!(
            rows,
            [
                claim("three", Some(true)),
                claim("two", Some(true)),
                claim("four", None),
                claim("five", Some(true)),
                claim("six", Some(true)),
            ],
            "deduped, most recent five, oldest first; `one` fell off"
        );
        assert!(checklist(&[]).is_empty());
    }

    #[test]
    fn with_claims_the_finale_is_a_checklist_and_without_them_chips() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.act(B, "Mail", 110);
        verdicts.finish_session(200);
        assert_eq!(
            verdicts.finale(),
            Finale::Chips(vec![
                FinaleChip {
                    tag: B,
                    title: "Mail".into(),
                    finished: true
                },
                FinaleChip {
                    tag: A,
                    title: "Notes".into(),
                    finished: true
                },
            ])
        );
        verdicts.verify(
            10,
            300,
            false,
            vec![claim("text area holds \"hi\"", Some(false))],
        );
        let finale = verdicts.finale();
        assert_eq!(finale.kind(), "checklist");
        assert_eq!(finale.log_rows(), ["unsatisfied: text area holds \"hi\""]);
        // Once shown, the next finale covers only newer work.
        verdicts.finale_shown();
        assert_eq!(verdicts.finale(), Finale::Chips(vec![]));
    }

    #[test]
    fn a_chip_row_has_a_check_only_for_finished_windows() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.act(B, "Mail", 110);
        verdicts.verify(20, 120, false, vec![]);
        verdicts.finish_session(200);
        let Finale::Chips(chips) = verdicts.finale() else {
            panic!("no claims: chips");
        };
        let checks: Vec<(&str, bool)> = chips
            .iter()
            .map(|chip| (chip.title.as_str(), chip.finished))
            .collect();
        assert_eq!(checks, [("Mail", false), ("Notes", true)]);
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
        let titles: Vec<&str> = verdicts
            .touched()
            .map(|(_, title)| title.as_str())
            .collect();
        assert_eq!(titles, ["w5", "w7", "w6", "w4", "w3"]);
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

    #[test]
    fn the_finale_plays_once_per_idle_and_a_new_action_cancels_it() {
        let mut finale = FinaleState::default();
        assert!(!finale.due(true), "still active");
        assert!(finale.due(false));
        let first = finale.start(true).unwrap();
        assert!(finale.playing());
        assert!(!finale.due(false), "already playing");
        // A new action: cancelled, and its end timer is stale.
        assert!(finale.cancel());
        assert!(!finale.playing());
        assert!(!finale.end(first));
        // The next idle plays again and ends on its own timer.
        assert!(finale.due(false));
        let second = finale.start(true).unwrap();
        assert!(finale.end(second));
        assert!(!finale.playing());
        // Finished for this stretch: not again until the session acts.
        assert!(!finale.due(false));
        assert!(!finale.cancel(), "nothing was playing");
        // A hidden panel (or one closed by the user) finishes silently.
        assert!(finale.due(false));
        assert_eq!(finale.start(false), None);
        assert!(!finale.playing());
        assert!(!finale.due(false));
    }
}
