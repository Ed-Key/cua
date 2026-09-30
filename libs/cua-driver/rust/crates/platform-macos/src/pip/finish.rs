//! What a session has finished, and the finished state (the "finale") a
//! panel plays on proof or when the session ends.
//!
//! Idle is not done. An agent thinking between two calls pauses longer than
//! any idle timer, so a session that goes quiet only has its panel fade:
//! nothing is marked finished and no finale plays.
//!
//! A window is FINISHED for a session when a `verify_state` on it was fully
//! satisfied at or after the session last acted in it, or when the session
//! ended after last acting in it, unless its latest verification (after
//! that action) was not satisfied. Finished back windows collapse to chips;
//! a finished front window shows a check badge on its card.
//!
//! The finale plays on proof and at session end. Proof is a satisfied claim
//! newer than the session's last action: its finale plays once the session
//! has been quiet for 8 s since the later of that action and the proof's
//! arrival, and any new action calls it off (the claims are kept for a later
//! finale). The finale shows the session's verified claims not yet shown as
//! a checklist (latest status per label, the five most recent, oldest first;
//! only a satisfied claim gets a check) under a "Verified n of m" line, or,
//! with none (at session end only), the windows it touched as a row of
//! chips. Rows come in one by one, then the panel holds and fades.
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
//! | Action note (on push) | records the action at its event ms (the session's last action is the newest of them), and the window it touched (identity and app, titled with the app name until a frame names it); ends a session finish older than it | none are dropped, but a claim older than the session's last action is no longer proof: it waits for a later finale | only if newer than the last applied action: cancels the finale (one playing, or a proof finale still waiting for its quiet period), restarts the idle deadline, lifts a user close | none |
//! | Captured frame | resolves a pid-only action to its window at the action's event ms (replacing that action's app-only chip) and names the touched window (idempotent; never ends a newer session finish) | none | none when its action was already applied (dedupe by event ms; the note always is, unless the panel did not exist yet); otherwise as its action note | still, header, front card, back items, window titles |
//! | Verification | latest by event ms per window (an older one never wins; a repeat with the same event time and status is not news) | latest by event ms per predicate (its identity on its window, not its display label, which may collide: two predicates with one label are two rows), kept across finales; an older one is ignored, and so is a repeat with the same event time and status | if it brought news: replays a playing finale. Proof (a satisfied claim newer than the session's last action) starts the proof timer; unsatisfied or unknown news starts nothing by itself. Before the session's first capture the panel does not exist yet: the evidence is kept for it, and its proof timer starts with the panel | chips and cards follow the verdicts; the front card shows its check badge while its window is finished; a shown panel stays up while proof waits for its finale |
//! | Idle timer (8 s after the last action) | none: idle is not done | none | none: no finale | the panel fades (a quiet hide), unless proof is waiting for its finale |
//! | Proof timer (8 s after the later of the session's last action and the proof's arrival, with no newer action) | none | the waiting proof is settled: it never comes due again by itself, played or not (its claims stay unshown until a finale displays them) | if a satisfied claim newer than the last action is still unshown and unsettled, the checklist plays (every unshown claim, latest status per predicate: a claim that flipped to unsatisfied shows as that): on the panel if it is up, and a quietly hidden panel is shown for it, unless the user closed the panel or the target window is fully visible (then nothing plays) | the panel fades after |
//! | `end_session`, or the session's control connection closing | the session finishes (now), always, whatever a finale is doing: each window it touched counts as finished unless its latest verification since its last action was not satisfied | none | a finale already playing keeps playing; otherwise one plays if anything is unshown (the checklist with claims, else the chips row) and the panel is up or may come up for it (not closed by the user, target window not fully visible); then the panel closes | back cards collapse to chips; the panel leaves the live set |
//! | Idle-TTL eviction (the core reclaimed the session after 300 s idle; its connection may revive it) | none: not a finish | none | none: no finale plays, one playing is cut | the panel closes quietly and leaves the live set; a revived session starts a new panel on its next frame |
//! | Finale timer | none | marks shown exactly what that finale displayed, as of when it was built (each claim by predicate, and each touched window a chip or checklist stood for by its action: app and event time, so it holds when the window resolves meanwhile); anything newer or later stays unshown; the watermarks stay | ends only the finale of its own generation | the panel fades |
//! | User close | none | none | ends any finale (its timer goes stale); closed until an action newer than the close, and nothing else (not `end_session`, not proof) shows the panel or plays a finale | the panel hides (an ending one closes) |
//! | Cursor update (the overlay's render thread, once per rendered frame, carrying the cursor's animated screen point, the window its last action targeted, and whether its click pulse is on) | none | none | none | only the front card's cursor sprite, and only while the cursor's window is the displayed one (a raised back card, or a new target whose capture is pending, hides it): it moves to the point mapped into the well from the target window's last known frame, looked up for that window (a raised card's window has none until the poll finds it) (see `cursor`), is re-placed when that frame, the displayed target or the well changes, and hides while the cursor is off that window, disabled or faded, or the panel has no frame; the first update of each click pulse that lands in the well logs "PiP cursor" once |
//!
//! | Overview open (menu bar item or the shortcut) | none | none | none | none: the overview (see `overview`) is a read-only view of every live panel's stack (front card first, finished windows badged), hidden panels included; it never makes the daemon the active app |
//! | Overview close (Esc, a click outside the sheet, the shortcut or the menu bar item again) | none | none | none | none: only the overview goes away |
//! | Overview focus click (a thumbnail, or Return on the selected one) | none | none | none: the window comes forward through the same path as the panel's Focus button (`bring_to_front`) | none: the overview closes first |
//! | Session acting while the overview is open | as the action note and captured frame rows | as those rows | as those rows | as those rows; the overview follows the stack on its next poll (a new front card, a new title) |
//! | Session verifying, ending or being evicted while the overview is open | as the verification, `end_session` and idle-TTL eviction rows | as those rows | as those rows | as those rows; the overview badges a window a verification finished on its next poll, drops an ended or evicted session's group, and shows its empty line when no live session is left |
//! | A window closing while the overview is open | as the captured frame row (the stack prunes it) | none | none | as that row; the overview drops the thumbnail on its next poll whether or not the stack still holds it (a kept front card, an idle session's window), since each poll checks every thumbnail's window against WindowServer's full list; a session left with no window keeps its row with a "No open windows" line; a focus click on a window closed since the poll is a logged no-op and the sheet stays up |
//! | Panel hidden or closed by the user while the overview is open | none | none | as the user close row | as that row; the overview keeps the session's group (a hidden panel is still a live session) |
//!
//! Everything here is pure (unit tested, one test per row); the cursor
//! row's logic and test live in `cursor`, the overview rows' in
//! `overview`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use pip_preview::PipSessionEnd;

use super::{Tag, IDLE_HIDE_AFTER};

/// Checklist rows at most.
pub(super) const CHECKLIST_ROWS: usize = 5;
/// Chips in the no-claims finale row at most.
pub(super) const CHIP_ROW: usize = 5;
/// Labels whose watermark a session keeps.
// ponytail: past this many distinct labels the oldest watermark is dropped
// (an older check of that label could then show again); raise it if agents
// ever verify that many different things in one session.
const CLAIM_MEMORY: usize = 256;
/// Each row starts this long after the one above it: quick, but each row
/// still lands on its own.
pub(super) const STAGGER: Duration = Duration::from_millis(70);
/// A row fades and slides in over this long; its mark follows.
pub(super) const ROW_IN: Duration = Duration::from_millis(150);
/// A mark draws (and pops) over this long.
pub(super) const MARK_IN: Duration = Duration::from_millis(180);
/// The finished state stays up this long once every row is in.
pub(super) const HOLD: Duration = Duration::from_millis(2000);
/// Room a finale keeps from the well's top and bottom (and its sides, for
/// chips).
pub(super) const FINALE_PAD: f64 = 6.0;
/// A checklist row (a capsule) and the gap between rows, at full size and
/// at the smallest the finale shrinks them to before it hides rows.
pub(super) const ROW_HEIGHT: f64 = 26.0;
pub(super) const ROW_GAP: f64 = 4.0;
pub(super) const ROW_HEIGHT_MIN: f64 = 20.0;
pub(super) const ROW_GAP_MIN: f64 = 2.0;
/// The "Verified n of m" line above the rows and the gap under it.
pub(super) const CAPTION_LINE: f64 = 14.0;
pub(super) const CAPTION_GAP: f64 = 6.0;
/// The "+n more" line under the rows that fit.
pub(super) const MORE_LINE: f64 = 16.0;
/// Inset of the rows from the well's left edge, and of a row's content
/// from its capsule.
pub(super) const ROW_INSET: f64 = 14.0;
pub(super) const ROW_PAD: f64 = 10.0;
/// Size of a checklist mark.
pub(super) const MARK_SIZE: f64 = 16.0;
/// Where a row's label starts in its capsule: after the mark and a gap.
pub(super) const LABEL_X: f64 = ROW_PAD + MARK_SIZE + 8.0;

/// Width of a checklist capsule for a label `text_w` wide in a well
/// `well_w` wide: mark, gap and text with the pads, but never wider than
/// the well allows (the label then truncates with a tail ellipsis). The
/// finale is laid out again when the well resizes, so a narrowed panel
/// never clips a row.
pub(super) fn row_width(text_w: f64, well_w: f64) -> f64 {
    (LABEL_X + text_w + ROW_PAD).min((well_w - 2.0 * ROW_INSET).max(0.0))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Claim {
    /// The predicate's identity on its window (opaque, from the hook):
    /// claims are tracked by it. Labels are display only and may collide.
    pub(super) id: u64,
    pub(super) label: String,
    /// `Some(true)` satisfied, `Some(false)` unsatisfied, `None` unknown.
    pub(super) satisfied: Option<bool>,
}

/// A predicate's latest claim by event time.
struct ClaimRecord {
    label: String,
    at_ms: u64,
    /// Arrival order, breaking ties between equal event times.
    seq: u64,
    satisfied: Option<bool>,
    /// A finale displayed it.
    shown: bool,
}

/// What a verification brought.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum News {
    /// Nothing: older than what is known, or a repeat of it.
    None,
    /// A verdict or a claim changed, but it is not proof.
    Other,
    /// Proof: a satisfied claim newer than the session's last action.
    Proof,
}

/// A session's evidence about its windows.
#[derive(Default)]
pub(super) struct Verdicts {
    /// Event time of the session's newest action, in any window.
    last_action_ms: u64,
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
    /// When the session ended, until an action newer than that.
    session_done: Option<u64>,
    /// Label -> its latest claim by event time. Never cleared: a finale only
    /// marks what it displayed as shown.
    claims: HashMap<u64, ClaimRecord>,
    next_seq: u64,
    /// Claims that arrived up to here had their proof timer (see `settle`).
    settled_seq: u64,
    /// Windows acted in, most recent action first.
    touched: Vec<Touched>,
}

/// A window the session acted in.
struct Touched {
    tag: Tag,
    title: String,
    /// Event time of the latest action in it.
    at_ms: u64,
    /// A finale displayed it as of that action.
    shown: bool,
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
        self.last_action_ms = self.last_action_ms.max(at_ms);
        if self.session_done.is_some_and(|done| at_ms > done) {
            self.session_done = None;
        }
    }

    /// The action note: the session acted on `target` at `at_ms`, in an
    /// app named `app_name`. The source of truth for touched windows: the
    /// window is touched now (titled with the app name until a frame names
    /// it), even if its capture never lands (coalesced, or dropped when the
    /// session ends).
    pub(super) fn note_action(&mut self, target: Tag, at_ms: u64, app_name: &str) {
        self.record_action(target, at_ms);
        self.touch(target, at_ms, None, app_name);
    }

    /// A captured frame of the session acting in `tag` (titled `title`) at
    /// `at_ms`: its window is resolved (for a pid-only action) and named.
    pub(super) fn act(&mut self, tag: Tag, title: &str, at_ms: u64) {
        self.record_action(tag, at_ms);
        self.touch(tag, at_ms, Some(title), "");
    }

    /// Touch `tag` for an action at `at_ms`: a frame's `title` names it; a
    /// note keeps a known title, else uses `fallback`. A newer action in a
    /// shown window is new work; the frame of an action already shown is
    /// not. A window resolved for a pid-only action replaces that action's
    /// app-only entry (and inherits whether it was shown).
    fn touch(&mut self, tag: Tag, at_ms: u64, title: Option<&str>, fallback: &str) {
        if tag == (None, None) {
            return;
        }
        let mut placeholder_shown = false;
        if let (Some(pid), Some(_)) = tag {
            self.touched.retain(|touched| {
                let placeholder = touched.tag == (Some(pid), None) && touched.at_ms <= at_ms;
                placeholder_shown |= placeholder && touched.at_ms == at_ms && touched.shown;
                !placeholder
            });
        }
        let (at_ms, shown, known_title) =
            match self.touched.iter().find(|touched| touched.tag == tag) {
                Some(known) if known.at_ms >= at_ms => {
                    (known.at_ms, known.shown, known.title.clone())
                }
                Some(known) => (at_ms, false, known.title.clone()),
                None => (at_ms, placeholder_shown, String::new()),
            };
        let title = match title.filter(|title| !title.is_empty()) {
            Some(title) => title.to_owned(),
            None if !known_title.is_empty() => known_title,
            None => fallback.to_owned(),
        };
        self.touched.retain(|touched| touched.tag != tag);
        let index = self
            .touched
            .partition_point(|touched| touched.at_ms > at_ms);
        self.touched.insert(
            index,
            Touched {
                tag,
                title,
                at_ms,
                shown,
            },
        );
        self.touched.truncate(CHIP_ROW);
    }

    /// A `verify_state` on `pid`'s `window` whose predicates were observed
    /// at `at_ms`. What it brought: a verdict or claim newer than what is
    /// known is news (at the same event time, only a changed status is: the
    /// later arrival wins, a repeat is nothing), and a satisfied claim newer
    /// than the session's last action is proof. An older verification
    /// changes nothing.
    pub(super) fn verify(
        &mut self,
        pid: i32,
        window: u32,
        at_ms: u64,
        satisfied: bool,
        claims: Vec<Claim>,
    ) -> News {
        self.pids.insert(window, pid);
        let (mut news, mut proof) = (false, false);
        // Older than what is known, or a repeat of it.
        let stale = |known_ms: u64, same: bool| known_ms > at_ms || (known_ms == at_ms && same);
        if self
            .verified
            .get(&window)
            .is_none_or(|&(previous, was)| !stale(previous, was == satisfied))
        {
            self.verified.insert(window, (at_ms, satisfied));
            news = true;
        }
        for claim in claims {
            if self
                .claims
                .get(&claim.id)
                .is_some_and(|record| stale(record.at_ms, record.satisfied == claim.satisfied))
            {
                continue;
            }
            proof |= claim.satisfied == Some(true) && at_ms > self.last_action_ms;
            self.next_seq += 1;
            self.claims.insert(
                claim.id,
                ClaimRecord {
                    label: claim.label,
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
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                self.claims.remove(&oldest);
            }
        }
        match (proof, news) {
            (true, _) => News::Proof,
            (false, true) => News::Other,
            (false, false) => News::None,
        }
    }

    /// Proof is waiting for its finale: a satisfied claim newer than the
    /// session's last action that no finale displayed and no proof timer
    /// settled. Read from the evidence itself, so an action (which makes
    /// every earlier claim older than it) and a claim flipping to
    /// unsatisfied both call it off.
    pub(super) fn proof_waiting(&self) -> bool {
        self.claims.values().any(|record| {
            !record.shown
                && record.satisfied == Some(true)
                && record.at_ms > self.last_action_ms
                && record.seq > self.settled_seq
        })
    }

    /// The proof timer ran for every claim that has arrived: none of them
    /// comes due again by itself (they stay unshown until a finale displays
    /// them).
    fn settle(&mut self) {
        self.settled_seq = self.next_seq;
    }

    /// The session ended at `at_ms`. Going idle is not this.
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

    /// What the finale shows now: the claims not yet shown (each label's
    /// latest status by event time, the `CHECKLIST_ROWS` most recent,
    /// oldest first), or with none the touched windows not yet shown. It
    /// records what it displays, as of which event, for `finale_shown`; a
    /// checklist stands for its stretch's touched windows too (as of now),
    /// so they do not come back as chips later.
    pub(super) fn finale(&self) -> Finale {
        let windows: Vec<&Touched> = self.touched.iter().filter(|t| !t.shown).collect();
        let window_marks = windows.iter().map(|touched| Displayed::Window {
            pid: touched.tag.0,
            window: touched.tag.1,
            at_ms: touched.at_ms,
        });
        let mut claims: Vec<(&u64, &ClaimRecord)> = self
            .claims
            .iter()
            .filter(|(_, record)| !record.shown)
            .collect();
        claims.sort_by_key(|(_, record)| (record.at_ms, record.seq));
        let claims = &claims[claims.len().saturating_sub(CHECKLIST_ROWS)..];
        if !claims.is_empty() {
            return Finale {
                rows: Rows::Checklist(
                    claims
                        .iter()
                        .map(|(id, record)| Claim {
                            id: **id,
                            label: record.label.clone(),
                            satisfied: record.satisfied,
                        })
                        .collect(),
                ),
                displayed: claims
                    .iter()
                    .map(|(id, record)| Displayed::Claim {
                        id: **id,
                        at_ms: record.at_ms,
                        seq: record.seq,
                    })
                    .chain(window_marks)
                    .collect(),
            };
        }
        Finale {
            rows: Rows::Chips(
                windows
                    .iter()
                    .map(|touched| FinaleChip {
                        tag: touched.tag,
                        title: touched.title.clone(),
                        finished: touched.tag.1.is_some_and(|window| self.finished(window)),
                    })
                    .collect(),
            ),
            displayed: window_marks.collect(),
        }
    }

    /// `finale` ran its course: exactly what it displayed is shown, each
    /// claim and window only as of the event it was displayed with (anything
    /// newer, or anything that landed while it played, stays unshown). The
    /// watermarks stay, so older evidence arriving later still loses.
    pub(super) fn finale_shown(&mut self, finale: &Finale) {
        for displayed in &finale.displayed {
            match displayed {
                Displayed::Claim { id, at_ms, seq } => {
                    if let Some(record) = self.claims.get_mut(id) {
                        if record.at_ms == *at_ms && record.seq == *seq {
                            record.shown = true;
                        }
                    }
                }
                // By the action (its app and event time), so a pid-only
                // action resolved to its window while this played is still
                // the entry it displayed.
                Displayed::Window { pid, window, at_ms } => {
                    for touched in &mut self.touched {
                        if touched.at_ms == *at_ms
                            && touched.tag.0 == *pid
                            && (window.is_none() || touched.tag.1 == *window)
                        {
                            touched.shown = true;
                        }
                    }
                }
            }
        }
    }

    /// Windows the session touched (most recent first), with titles.
    pub(super) fn touched(&self) -> impl Iterator<Item = (Tag, String)> + '_ {
        self.touched
            .iter()
            .map(|touched| (touched.tag, touched.title.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FinaleChip {
    pub(super) tag: Tag,
    pub(super) title: String,
    /// Only a finished window's chip gets a check.
    pub(super) finished: bool,
}

/// A finale's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Rows {
    Checklist(Vec<Claim>),
    Chips(Vec<FinaleChip>),
}

/// One item a finale displays, as of the event it displays it with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Displayed {
    /// A predicate's claim, as of its event time and arrival.
    Claim { id: u64, at_ms: u64, seq: u64 },
    /// A touched window as of the action that touched it: its app and event
    /// time (the window may be unresolved yet).
    Window {
        pid: Option<i32>,
        window: Option<u32>,
        at_ms: u64,
    },
}

/// A finale as built: its rows, and what they display (for `finale_shown`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Finale {
    pub(super) rows: Rows,
    displayed: Vec<Displayed>,
}

impl Finale {
    pub(super) fn kind(&self) -> &'static str {
        match self.rows {
            Rows::Checklist(_) => "checklist",
            Rows::Chips(_) => "chips",
        }
    }

    pub(super) fn len(&self) -> usize {
        match &self.rows {
            Rows::Checklist(rows) => rows.len(),
            Rows::Chips(chips) => chips.len(),
        }
    }

    /// The checklist's caption: "Verified n of m" (m rows, n satisfied);
    /// `None` for chips.
    pub(super) fn caption(&self) -> Option<String> {
        match &self.rows {
            Rows::Checklist(rows) => {
                let satisfied = rows.iter().filter(|row| row.satisfied == Some(true)).count();
                Some(format!("Verified {satisfied} of {}", rows.len()))
            }
            Rows::Chips(_) => None,
        }
    }

    /// One line per row, for logs: `satisfied: text area holds "hi"`,
    /// `finished: Notes`.
    pub(super) fn log_rows(&self) -> Vec<String> {
        match &self.rows {
            Rows::Checklist(rows) => rows
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
            Rows::Chips(chips) => chips
                .iter()
                .map(|chip| {
                    let status = if chip.finished { "finished" } else { "plain" };
                    format!("{status}: {}", chip.title)
                })
                .collect(),
        }
    }
}

/// How a checklist of `rows` rows fits a well `well_h` tall: at full size
/// when it can, else with rows and gaps shrunk (down to `ROW_HEIGHT_MIN` /
/// `ROW_GAP_MIN`), else with only the first `visible` rows and a "+n more"
/// line counting the rest, so every result is shown or counted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ChecklistFit {
    pub(super) row_height: f64,
    pub(super) gap: f64,
    pub(super) visible: usize,
    pub(super) hidden: usize,
}

impl ChecklistFit {
    /// Height of the whole block: caption, the visible rows, the more line.
    pub(super) fn height(&self) -> f64 {
        let rows = self.visible as f64 * self.row_height
            + (self.visible.saturating_sub(1)) as f64 * self.gap;
        let more = if self.hidden > 0 { MORE_LINE } else { 0.0 };
        CAPTION_LINE + CAPTION_GAP + rows + more
    }
}

pub(super) fn checklist_fit(well_h: f64, rows: usize) -> ChecklistFit {
    let available = (well_h - 2.0 * FINALE_PAD).max(0.0);
    let fit = |row_height, gap, visible, hidden| ChecklistFit {
        row_height,
        gap,
        visible,
        hidden,
    };
    let full = fit(ROW_HEIGHT, ROW_GAP, rows, 0);
    if full.height() <= available {
        return full;
    }
    let room = available - CAPTION_LINE - CAPTION_GAP;
    let need = rows as f64 * ROW_HEIGHT + rows.saturating_sub(1) as f64 * ROW_GAP;
    let scale = (room / need).clamp(0.0, 1.0);
    let row_height = (ROW_HEIGHT * scale).floor().max(ROW_HEIGHT_MIN);
    let gap = (ROW_GAP * scale).floor().max(ROW_GAP_MIN);
    let shrunk = fit(row_height, gap, rows, 0);
    if shrunk.height() <= available {
        return shrunk;
    }
    // Rows that fit above a "+n more" line (never all of them here).
    let room = (room - MORE_LINE).max(0.0);
    let visible = (((room + gap) / (row_height + gap)).floor() as usize).min(rows - 1);
    fit(row_height, gap, visible, rows - visible)
}

/// Chips per row when `chips` wrap in a well `well_w` wide, `chip_w` each
/// and `gap` apart (at least one per row), and how many rows that makes.
pub(super) fn chip_grid(well_w: f64, chips: usize, chip_w: f64, gap: f64) -> (usize, usize) {
    let room = (well_w - 2.0 * FINALE_PAD + gap).max(0.0);
    let per_row = (((room) / (chip_w + gap)).floor() as usize).max(1);
    (per_row, chips.div_ceil(per_row))
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
/// applied, whether the user closed the panel, and the finale (whether one
/// is playing, and which schedule is current). Whether a finale is due is
/// not kept here: it is read from the evidence (`proof_finale`,
/// `end_session`).
#[derive(Default)]
pub(super) struct Lifecycle {
    /// Event time of the newest action applied.
    last_action_ms: u64,
    /// Closed by the user since that action.
    closed: bool,
    generation: u64,
    playing: bool,
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
        self.generation += 1;
        Some(was)
    }

    /// The user closed the panel: any finale ends (its end timer goes
    /// stale), and the panel stays closed, with no finale, until a newer
    /// action.
    pub(super) fn close(&mut self) {
        self.closed = true;
        if self.playing {
            self.playing = false;
            self.generation += 1;
        }
    }

    pub(super) fn closed(&self) -> bool {
        self.closed
    }

    /// A finale is due. It plays only if `visible` (the panel is up or may
    /// come up for it; something to show) and the user has not closed the
    /// panel: then the generation for its end timer.
    fn start(&mut self, visible: bool) -> Option<u64> {
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

/// Whether the session has been quiet long enough for a proof finale at
/// `now`: `IDLE_HIDE_AFTER` since the later of its last action (when it was
/// applied) and the proof's arrival (`None`: the proof came before the
/// panel did, so the action is the later one).
pub(super) fn proof_quiet(last_action: Instant, proof: Option<Instant>, now: Instant) -> bool {
    let since = proof.map_or(last_action, |proof| proof.max(last_action));
    now.saturating_duration_since(since) >= IDLE_HIDE_AFTER
}

/// Whether a due finale has a panel to play on: one that is up, or a hidden
/// one that may come up for it (the user cannot see the whole target
/// window). The user's close is `Lifecycle::start`'s to refuse.
pub(super) fn may_show(shown: bool, target_visible: bool) -> bool {
    shown || !target_visible
}

/// The proof timer row: once the session is `quiet` (see `proof_quiet`), the
/// waiting proof is settled, and its checklist plays if the panel
/// `may_show` it and the user has not closed it: then the finale and the
/// generation for its end timer. Nothing while a finale plays (news replays
/// that one) or with no proof waiting (idle alone plays nothing).
pub(super) fn proof_finale(
    verdicts: &mut Verdicts,
    lifecycle: &mut Lifecycle,
    quiet: bool,
    may_show: bool,
) -> Option<(Finale, u64)> {
    if !quiet || lifecycle.playing() || !verdicts.proof_waiting() {
        return None;
    }
    verdicts.settle();
    let finale = verdicts.finale();
    let generation = lifecycle.start(may_show)?;
    Some((finale, generation))
}

/// A playing finale was rebuilt for news (`Lifecycle::restart`): what it now
/// shows had its proof timer.
pub(super) fn replayed(verdicts: &mut Verdicts) -> Finale {
    verdicts.settle();
    verdicts.finale()
}

/// How an ended session's panel goes away.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Ending {
    /// The finale already playing keeps playing; the panel closes when it
    /// is over.
    Playing,
    /// This finale plays under this generation; the panel closes when it is
    /// over.
    Play(Finale, u64),
    /// The panel closes now.
    Close,
}

/// The session's panel is going away at `at_ms` (the `end_session` and
/// idle-TTL eviction rows). An expired session is not finished: nothing is
/// marked and no finale plays. A finished one is done whatever its finale
/// is doing; then a playing finale keeps playing, or one starts if anything
/// is unshown and the panel `may_show` it and the user has not closed it.
pub(super) fn end_session(
    verdicts: &mut Verdicts,
    lifecycle: &mut Lifecycle,
    end: PipSessionEnd,
    at_ms: u64,
    may_show: bool,
) -> Ending {
    if end == PipSessionEnd::Expired {
        return Ending::Close;
    }
    verdicts.finish_session(at_ms);
    if lifecycle.playing() {
        return Ending::Playing;
    }
    let finale = verdicts.finale();
    match lifecycle.start(may_show && finale.len() > 0) {
        Some(generation) => Ending::Play(finale, generation),
        None => Ending::Close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Tag = (Some(1), Some(10));
    const B: Tag = (Some(2), Some(20));

    /// A claim whose predicate identity follows its label (one predicate
    /// per label), as most tests want.
    fn claim(label: &str, satisfied: Option<bool>) -> Claim {
        let id = label.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        });
        Claim {
            id,
            label: label.to_owned(),
            satisfied,
        }
    }

    fn checklist(verdicts: &Verdicts) -> Vec<Claim> {
        match verdicts.finale().rows {
            Rows::Checklist(rows) => rows,
            Rows::Chips(_) => Vec::new(),
        }
    }

    const FINISHED: PipSessionEnd = PipSessionEnd::Finished;

    /// A session that acted in A (at 100) and then proved "saved" there (at
    /// 200): the proof waits for its finale.
    fn proved() -> (Verdicts, Lifecycle) {
        let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
        assert_eq!(life.resume(100), Some(false));
        verdicts.note_action(A, 100, "Notes");
        let news = verdicts.verify(1, 10, 200, true, vec![claim("saved", Some(true))]);
        assert_eq!(news, News::Proof);
        (verdicts, life)
    }

    /// The proof timer fired with the session quiet and a panel that may
    /// show the finale.
    fn quiet(verdicts: &mut Verdicts, life: &mut Lifecycle) -> Option<(Finale, u64)> {
        proof_finale(verdicts, life, true, true)
    }

    // ── Row: action note ─────────────────────────────────────────────────

    #[test]
    fn row_action_note_resumes_only_when_newer_than_the_last_action() {
        let mut life = Lifecycle::default();
        assert_eq!(life.resume(100), Some(false));
        assert_eq!(life.resume(100), None, "the same action again");
        assert_eq!(life.resume(90), None, "an older action");
        // A newer action stops a playing finale...
        let finale = life.start(true).unwrap();
        assert_eq!(life.resume(200), Some(true));
        assert!(!life.playing());
        assert!(!life.end(finale), "its end timer is stale");
        // ...and lifts a close.
        life.close();
        assert_eq!(life.resume(300), Some(false));
        assert!(!life.closed());
        // In the verdicts: it ends an older session finish.
        let mut verdicts = Verdicts::default();
        verdicts.record_action(A, 100);
        verdicts.finish_session(150);
        assert!(verdicts.finished(10));
        verdicts.record_action(B, 200);
        assert!(!verdicts.finished(10), "working again");
    }

    #[test]
    fn row_action_note_cancels_a_waiting_or_playing_proof_finale() {
        // Proof is waiting out its quiet period when the agent acts again.
        let (mut verdicts, mut life) = proved();
        assert!(verdicts.proof_waiting());
        assert_eq!(life.resume(300), Some(false));
        verdicts.note_action(A, 300, "Notes");
        assert!(!verdicts.proof_waiting(), "older than the last action");
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        assert!(!verdicts.finished(10), "and its window is open again");
        // The claim is kept for a later finale.
        assert_eq!(checklist(&verdicts), [claim("saved", Some(true))]);
        // A playing proof finale stops too, and its end timer goes stale.
        let (mut verdicts, mut life) = proved();
        let (_, generation) = quiet(&mut verdicts, &mut life).unwrap();
        assert_eq!(life.resume(300), Some(true));
        verdicts.note_action(A, 300, "Notes");
        assert!(!life.playing() && !life.end(generation));
        assert_eq!(checklist(&verdicts), [claim("saved", Some(true))], "kept");
    }

    #[test]
    fn row_action_note_touches_its_window_without_any_frame() {
        // Act in a new window and end at once: end_session drops the pending
        // capture, so no frame ever lands. The chip is there, app-titled.
        let mut verdicts = Verdicts::default();
        verdicts.note_action(A, 100, "Notes");
        verdicts.finish_session(200);
        assert_eq!(
            verdicts.finale().rows,
            Rows::Chips(vec![FinaleChip {
                tag: A,
                title: "Notes".into(),
                finished: true
            }])
        );
        // Coalesced captures: two notes, only the later frame lands; both
        // windows keep their chips, the framed one with its window title.
        let mut verdicts = Verdicts::default();
        verdicts.note_action(A, 100, "Notes");
        verdicts.note_action(B, 110, "Mail");
        verdicts.act(B, "Inbox", 110);
        let titles: Vec<String> = verdicts.touched().map(|(_, title)| title).collect();
        assert_eq!(titles, ["Inbox", "Notes"]);
        // A later note does not rename a window a frame named.
        verdicts.note_action(B, 120, "Mail");
        assert_eq!(verdicts.touched().next().unwrap().1, "Inbox");
        // A pid-only action's app chip becomes its window's when the frame
        // resolves it: one chip, not two.
        let mut verdicts = Verdicts::default();
        verdicts.note_action((Some(3), None), 100, "Safari");
        verdicts.act((Some(3), Some(30)), "Docs", 100);
        let touched: Vec<(Tag, String)> = verdicts.touched().collect();
        assert_eq!(touched, [((Some(3), Some(30)), "Docs".to_owned())]);
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
        // The capture queue is backlogged past the proof timer: the action
        // note (t=1000) resumed the session, a check proved it, its finale
        // started, then the frame of that same action lands.
        let mut life = Lifecycle::default();
        let mut verdicts = Verdicts::default();
        assert!(life.resume(1_000).is_some());
        verdicts.record_action(A, 1_000);
        verdicts.verify(1, 10, 2_000, true, vec![claim("saved", Some(true))]);
        let (_, finale) = quiet(&mut verdicts, &mut life).unwrap();
        // The frame: no lifecycle change, and the window stays finished.
        assert_eq!(life.resume(1_000), None);
        verdicts.act(A, "Notes", 1_000);
        assert!(life.playing(), "the finale keeps playing");
        assert!(verdicts.finished(10), "the window stays finished");
        assert!(life.end(finale));
        assert_eq!(quiet(&mut verdicts, &mut life), None, "no second finale");
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
        let news = verdicts.verify(1, 10, 200, false, vec![claim("saved", Some(false))]);
        assert_eq!(news, News::Other);
        assert_eq!(checklist(&verdicts), [claim("saved", Some(false))]);
        verdicts.finale_shown(&verdicts.finale());
        let news = verdicts.verify(1, 10, 150, true, vec![claim("saved", Some(true))]);
        assert_eq!(news, News::None);
        assert!(!verdicts.finished(10));
        assert!(checklist(&verdicts).is_empty(), "no green row");
        // Newer evidence after the finale is news and shows next time.
        let news = verdicts.verify(1, 10, 300, true, vec![claim("saved", Some(true))]);
        assert_eq!(news, News::Proof);
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
    fn news_replays_a_playing_finale_and_later_proof_gets_its_own() {
        let (mut verdicts, mut life) = proved();
        assert_eq!(life.restart(), None, "nothing playing");
        let (_, first) = quiet(&mut verdicts, &mut life).unwrap();
        // News lands while it plays: it starts over with the news in it.
        let news = verdicts.verify(1, 10, 300, true, vec![claim("sent", Some(true))]);
        assert_eq!(news, News::Proof);
        let second = life.restart().unwrap();
        let finale = replayed(&mut verdicts);
        assert_eq!(finale.log_rows(), ["satisfied: saved", "satisfied: sent"]);
        assert!(!life.end(first), "replayed: the first schedule is stale");
        assert!(life.end(second));
        verdicts.finale_shown(&finale);
        assert_eq!(quiet(&mut verdicts, &mut life), None, "shown already");
        // Proof after it is over: a finale of its own, after its own quiet
        // period.
        verdicts.verify(1, 10, 400, true, vec![claim("filed", Some(true))]);
        assert_eq!(proof_finale(&mut verdicts, &mut life, false, true), None);
        let (finale, _) = quiet(&mut verdicts, &mut life).unwrap();
        assert_eq!(finale.log_rows(), ["satisfied: filed"]);
    }

    #[test]
    fn row_verification_unsatisfied_or_unknown_news_plays_nothing_by_itself() {
        let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
        life.resume(100);
        verdicts.note_action(A, 100, "Notes");
        let claims = vec![claim("saved", Some(false)), claim("sent", None)];
        assert_eq!(verdicts.verify(1, 10, 200, false, claims), News::Other);
        assert!(!verdicts.proof_waiting());
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        assert!(!life.playing());
        assert!(!verdicts.finished(10));
        // The claims are kept: they show when a finale does play.
        assert_eq!(checklist(&verdicts).len(), 2);
    }

    #[test]
    fn satisfied_evidence_delivered_after_a_newer_action_is_not_proof() {
        // The check observed the window at 200; the agent acted again at 300
        // and that note reached the panel first.
        let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
        life.resume(100);
        verdicts.note_action(A, 100, "Notes");
        life.resume(300);
        verdicts.note_action(A, 300, "Notes");
        let news = verdicts.verify(1, 10, 200, true, vec![claim("saved", Some(true))]);
        assert_eq!(news, News::Other, "news, but not proof");
        assert!(!verdicts.finished(10), "the window was acted in since");
        assert!(!verdicts.proof_waiting());
        assert_eq!(quiet(&mut verdicts, &mut life), None);
    }

    #[test]
    fn an_action_in_another_window_after_proof_keeps_that_window_verified() {
        let (mut verdicts, mut life) = proved();
        assert!(verdicts.finished(10));
        assert_eq!(life.resume(300), Some(false));
        verdicts.note_action(B, 300, "Mail");
        assert!(verdicts.finished(10), "A was not touched since its proof");
        assert!(!verdicts.finished(20));
        // But the session is working again: its proof finale is off.
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        // The claim shows when the session ends.
        let Ending::Play(finale, _) = end_session(&mut verdicts, &mut life, FINISHED, 400, true)
        else {
            panic!("a finale at the end");
        };
        assert_eq!(finale.log_rows(), ["satisfied: saved"]);
    }

    #[test]
    fn a_claim_flipping_to_unsatisfied_before_the_finale_shows_as_that() {
        // Two predicates proved, then one fails before the timer: the
        // checklist plays for the proof that stands, with each predicate's
        // latest status.
        let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
        life.resume(100);
        verdicts.note_action(A, 100, "Notes");
        let both = vec![claim("saved", Some(true)), claim("sent", Some(true))];
        assert_eq!(verdicts.verify(1, 10, 200, true, both), News::Proof);
        let flipped = vec![claim("saved", Some(false))];
        assert_eq!(verdicts.verify(1, 10, 300, false, flipped), News::Other);
        assert!(!verdicts.finished(10), "its latest check failed");
        let (finale, _) = quiet(&mut verdicts, &mut life).unwrap();
        assert_eq!(finale.log_rows(), ["satisfied: sent", "unsatisfied: saved"]);
        assert_eq!(finale.caption().as_deref(), Some("Verified 1 of 2"));
        // The only proof fails before the timer: nothing is proved any
        // more, so nothing plays.
        let (mut verdicts, mut life) = proved();
        verdicts.verify(1, 10, 300, false, vec![claim("saved", Some(false))]);
        assert!(!verdicts.proof_waiting());
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        assert!(!verdicts.finished(10));
    }

    #[test]
    fn a_repeated_verification_with_the_same_event_time_is_not_news() {
        let (mut verdicts, mut life) = proved();
        let (finale, generation) = quiet(&mut verdicts, &mut life).unwrap();
        assert!(life.end(generation));
        verdicts.finale_shown(&finale);
        // The same result again, observed at the same time: nothing.
        let repeat = verdicts.verify(1, 10, 200, true, vec![claim("saved", Some(true))]);
        assert_eq!(repeat, News::None);
        assert!(!verdicts.proof_waiting());
        assert!(checklist(&verdicts).is_empty(), "still shown");
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        // At the same event time only a changed status is news, and the
        // later arrival wins.
        let changed = verdicts.verify(1, 10, 200, false, vec![claim("saved", Some(false))]);
        assert_eq!(changed, News::Other);
        assert!(!verdicts.finished(10));
        assert_eq!(checklist(&verdicts), [claim("saved", Some(false))]);
    }

    #[test]
    fn a_verification_before_the_first_capture_is_kept_for_the_panel() {
        // The session acted (its first capture is pending, so it has no
        // panel) and verified: the evidence waits without a lifecycle.
        let mut early = Verdicts::default();
        early.note_action(A, 100, "Notes");
        let news = early.verify(1, 10, 200, true, vec![claim("saved", Some(true))]);
        assert_eq!(news, News::Proof);
        // The first frame lands: the panel takes the evidence and applies
        // the action.
        let mut life = Lifecycle::default();
        assert_eq!(life.resume(100), Some(false));
        early.act(A, "Notes", 100);
        assert!(early.finished(10));
        assert!(early.proof_waiting(), "the proof timer starts with the panel");
        let (finale, _) = quiet(&mut early, &mut life).unwrap();
        assert_eq!(finale.log_rows(), ["satisfied: saved"]);
    }

    #[test]
    fn row_verification_predicates_sharing_a_label_never_merge() {
        // Two different predicates whose truncated labels collide: the pass
        // of one never turns the other's failure green.
        let label = "text area holds \"abcdefghijklmno\u{2026}\"";
        let first = Claim {
            id: 1,
            label: label.into(),
            satisfied: Some(false),
        };
        let second = Claim {
            id: 2,
            label: label.into(),
            satisfied: Some(true),
        };
        let mut verdicts = Verdicts::default();
        verdicts.verify(1, 10, 100, false, vec![first.clone()]);
        verdicts.verify(1, 10, 200, true, vec![second.clone()]);
        assert_eq!(checklist(&verdicts), [first, second], "both rows show");
    }

    // ── Row: idle timer ──────────────────────────────────────────────────

    #[test]
    fn row_idle_timer_only_fades_and_finishes_nothing() {
        let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
        life.resume(100);
        verdicts.act(A, "Notes", 100);
        life.resume(110);
        verdicts.act(B, "Mail", 110);
        // Quiet for 8 s, or for an hour: no proof, so nothing plays and
        // nothing is finished.
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        assert!(!life.playing());
        assert!(!verdicts.finished(10) && !verdicts.finished(20));
        // Nothing was used up: both windows are still owed to the session's
        // end, neither with a check yet.
        assert_eq!(
            verdicts.finale().log_rows(),
            ["plain: Mail", "plain: Notes"]
        );
    }

    // ── Row: proof timer ─────────────────────────────────────────────────

    #[test]
    fn row_proof_timer_plays_the_checklist_after_eight_quiet_seconds() {
        // When: 8 s after the later of the last action and the proof.
        let start = Instant::now();
        let secs = Duration::from_secs;
        assert_eq!(IDLE_HIDE_AFTER, secs(8));
        assert!(!proof_quiet(start, Some(start + secs(3)), start + secs(8)));
        assert!(proof_quiet(start, Some(start + secs(3)), start + secs(11)));
        // An action applied after the proof arrived (its capture was slow,
        // and the panel is created by it): 8 s after the action.
        assert!(!proof_quiet(start + secs(3), Some(start), start + secs(10)));
        assert!(proof_quiet(start + secs(3), Some(start), start + secs(11)));
        // Proof from before the panel existed counts from the action.
        assert!(!proof_quiet(start, None, start + secs(7)));
        assert!(proof_quiet(start, None, start + secs(8)));
        // What: the checklist, once.
        let (mut verdicts, mut life) = proved();
        assert!(verdicts.finished(10), "the window is finished at once");
        assert_eq!(proof_finale(&mut verdicts, &mut life, false, true), None);
        assert!(!life.playing(), "not before the session is quiet");
        let (finale, generation) = quiet(&mut verdicts, &mut life).unwrap();
        assert_eq!(finale.kind(), "checklist");
        assert_eq!(finale.log_rows(), ["satisfied: saved"]);
        assert!(life.playing());
        assert_eq!(quiet(&mut verdicts, &mut life), None, "not while it plays");
        assert!(life.end(generation));
        verdicts.finale_shown(&finale);
        assert_eq!(quiet(&mut verdicts, &mut life), None, "and not again");
    }

    #[test]
    fn proof_for_a_quietly_hidden_panel_shows_it_unless_closed_or_in_full_view() {
        // A hidden panel comes up for the finale when the user cannot see
        // the whole target window.
        assert!(may_show(true, false));
        assert!(may_show(false, false));
        assert!(!may_show(false, true));
        let (mut verdicts, mut life) = proved();
        assert!(proof_finale(&mut verdicts, &mut life, true, may_show(false, false)).is_some());
        assert!(life.playing());
        // The window is in full view: nothing plays, and the proof is
        // settled (covering the window later does not bring it back).
        let (mut verdicts, mut life) = proved();
        assert_eq!(proof_finale(&mut verdicts, &mut life, true, may_show(false, true)), None);
        assert!(!life.playing() && !verdicts.proof_waiting());
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        // Its claim is kept for a later finale.
        assert_eq!(checklist(&verdicts), [claim("saved", Some(true))]);
        // The user closed the panel: the same.
        let (mut verdicts, mut life) = proved();
        life.close();
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        assert!(!life.playing() && !verdicts.proof_waiting());
    }

    #[test]
    fn with_claims_the_finale_is_a_checklist_and_without_them_chips() {
        let mut verdicts = Verdicts::default();
        verdicts.act(A, "Notes", 100);
        verdicts.act(B, "Mail", 110);
        verdicts.verify(2, 20, 120, false, vec![]);
        // Ended: windows count as finished, unless their latest check
        // failed.
        verdicts.finish_session(200);
        assert!(verdicts.finished(10));
        assert!(!verdicts.finished(20));
        assert_eq!(
            verdicts.finale().rows,
            Rows::Chips(vec![
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
        assert_eq!(finale.caption().as_deref(), Some("Verified 0 of 1"));
        verdicts.verify(1, 10, 400, true, vec![claim("saved", Some(true))]);
        assert_eq!(verdicts.finale().caption().as_deref(), Some("Verified 1 of 2"));
        let mut chips = Verdicts::default();
        chips.act(A, "Notes", 100);
        assert_eq!(chips.finale().caption(), None);
    }

    // ── Row: end_session ─────────────────────────────────────────────────

    #[test]
    fn row_end_session_always_finishes_and_plays_what_is_unshown() {
        let worked = || {
            let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
            life.resume(100);
            verdicts.note_action(A, 100, "Notes");
            life.resume(110);
            verdicts.note_action(B, 110, "Mail");
            verdicts.verify(2, 20, 120, false, vec![]);
            (verdicts, life)
        };
        // No claims: the chips row, a check on each finished window.
        let (mut verdicts, mut life) = worked();
        assert!(!verdicts.finished(10), "not before the session ends");
        let Ending::Play(finale, _) = end_session(&mut verdicts, &mut life, FINISHED, 200, true)
        else {
            panic!("the chips row");
        };
        assert_eq!(finale.log_rows(), ["plain: Mail", "finished: Notes"]);
        assert!(life.playing() && verdicts.finished(10) && !verdicts.finished(20));
        // A panel that may not show (hidden, its window in full view), or
        // one the user closed: done all the same, and no finale.
        let (mut verdicts, mut life) = worked();
        let ending = end_session(&mut verdicts, &mut life, FINISHED, 200, may_show(false, true));
        assert_eq!(ending, Ending::Close);
        assert!(verdicts.finished(10));
        let (mut verdicts, mut life) = worked();
        life.close();
        assert_eq!(end_session(&mut verdicts, &mut life, FINISHED, 200, true), Ending::Close);
        assert!(verdicts.finished(10) && !life.playing());
        // A session that touched nothing has nothing to show.
        let (mut verdicts, mut life) = (Verdicts::default(), Lifecycle::default());
        assert_eq!(end_session(&mut verdicts, &mut life, FINISHED, 200, true), Ending::Close);
    }

    #[test]
    fn ending_during_a_proof_finale_finishes_the_session_and_lets_it_play_out() {
        let (mut verdicts, mut life) = proved();
        verdicts.note_action(B, 90, "Mail");
        let (_, generation) = quiet(&mut verdicts, &mut life).unwrap();
        assert!(!verdicts.finished(20), "B has no proof");
        let ending = end_session(&mut verdicts, &mut life, FINISHED, 300, true);
        assert_eq!(ending, Ending::Playing);
        assert!(verdicts.finished(20), "done, whatever the finale is doing");
        assert!(life.end(generation), "the playing finale's timer closes the panel");
    }

    #[test]
    fn ending_after_a_proof_finale_finishes_the_session_and_plays_nothing_twice() {
        let (mut verdicts, mut life) = proved();
        verdicts.note_action(B, 90, "Mail");
        let (finale, generation) = quiet(&mut verdicts, &mut life).unwrap();
        assert!(life.end(generation));
        verdicts.finale_shown(&finale);
        let ending = end_session(&mut verdicts, &mut life, FINISHED, 300, true);
        assert_eq!(ending, Ending::Close, "the checklist stood for this stretch");
        assert!(verdicts.finished(20), "done all the same");
        // Work after that finale is unshown: it plays at the end.
        let (mut verdicts, mut life) = proved();
        let (finale, generation) = quiet(&mut verdicts, &mut life).unwrap();
        assert!(life.end(generation));
        verdicts.finale_shown(&finale);
        life.resume(400);
        verdicts.note_action(B, 400, "Mail");
        let Ending::Play(finale, _) = end_session(&mut verdicts, &mut life, FINISHED, 500, true)
        else {
            panic!("the new work's chips");
        };
        assert_eq!(finale.log_rows(), ["finished: Mail"]);
    }

    // ── Row: idle-TTL eviction ───────────────────────────────────────────

    #[test]
    fn row_idle_ttl_eviction_closes_without_marks_or_finale() {
        let expired = PipSessionEnd::Expired;
        let (mut verdicts, mut life) = proved();
        life.resume(300);
        verdicts.note_action(B, 300, "Mail");
        assert_eq!(end_session(&mut verdicts, &mut life, expired, 400, true), Ending::Close);
        assert!(!verdicts.finished(20), "an expired session did not finish");
        assert!(verdicts.finished(10), "what it proved stays proved");
        assert!(!life.playing(), "no finale");
        // A finale playing when it expires is cut: the panel closes now.
        let (mut verdicts, mut life) = proved();
        quiet(&mut verdicts, &mut life).unwrap();
        assert_eq!(end_session(&mut verdicts, &mut life, expired, 400, true), Ending::Close);
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
        verdicts.finale_shown(&verdicts.finale());
        assert_eq!(verdicts.finale().rows, Rows::Chips(vec![]), "A was shown");
        verdicts.act(B, "Mail", 300);
        let finale = verdicts.finale();
        let Rows::Chips(chips) = &finale.rows else {
            panic!("no new claims: chips");
        };
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].tag, B);
        verdicts.finale_shown(&finale);
        // A late frame of an action already shown is not new work.
        verdicts.act(A, "Notes", 100);
        assert_eq!(verdicts.finale().rows, Rows::Chips(vec![]));
    }

    #[test]
    fn row_finale_timer_marks_shown_only_what_its_finale_displayed() {
        // A chips finale is built showing B; while it plays, the backlogged
        // capture of an earlier action in A lands. The timer marks B only:
        // A's chip is still owed to a later finale.
        let a_window = (Some(1), Some(30));
        let mut verdicts = Verdicts::default();
        verdicts.act(B, "Mail", 500);
        let playing = verdicts.finale();
        verdicts.act(a_window, "Notes", 400);
        verdicts.finale_shown(&playing);
        let Rows::Chips(chips) = verdicts.finale().rows else {
            panic!("chips");
        };
        let tags: Vec<Tag> = chips.iter().map(|chip| chip.tag).collect();
        assert_eq!(tags, [a_window]);
        // A checklist stands for the windows touched when it was built, not
        // for one whose capture lands while it plays.
        let mut verdicts = Verdicts::default();
        verdicts.act(B, "Mail", 500);
        verdicts.verify(2, 20, 550, true, vec![claim("sent", Some(true))]);
        let playing = verdicts.finale();
        assert_eq!(playing.kind(), "checklist");
        verdicts.act(a_window, "Notes", 400);
        verdicts.finale_shown(&playing);
        let Rows::Chips(chips) = verdicts.finale().rows else {
            panic!("chips");
        };
        let tags: Vec<Tag> = chips.iter().map(|chip| chip.tag).collect();
        assert_eq!(tags, [a_window], "B was covered by the checklist");
        // A newer action in a displayed window during the finale stays
        // unshown too.
        let mut verdicts = Verdicts::default();
        verdicts.act(B, "Mail", 500);
        let playing = verdicts.finale();
        verdicts.act(B, "Mail", 600);
        verdicts.finale_shown(&playing);
        assert_eq!(verdicts.finale().len(), 1);
        // Same for claims: a newer claim for a displayed label (or a new
        // label) that lands before the timer is not marked shown.
        let mut verdicts = Verdicts::default();
        verdicts.verify(1, 10, 100, false, vec![claim("saved", Some(false))]);
        let playing = verdicts.finale();
        verdicts.verify(1, 10, 200, true, vec![claim("saved", Some(true))]);
        verdicts.verify(1, 10, 200, true, vec![claim("sent", Some(true))]);
        verdicts.finale_shown(&playing);
        assert_eq!(
            checklist(&verdicts),
            [claim("saved", Some(true)), claim("sent", Some(true))]
        );
    }

    #[test]
    fn row_finale_timer_keeps_an_action_shown_when_its_window_resolves_meanwhile() {
        // A pid-only action shows as an app chip; while that finale plays,
        // the backlogged capture resolves it to its window. The timer still
        // marks it shown: it does not replay in the next stretch.
        let mut verdicts = Verdicts::default();
        verdicts.note_action((Some(3), None), 100, "Safari");
        let playing = verdicts.finale();
        assert_eq!(playing.len(), 1);
        verdicts.act((Some(3), Some(30)), "Docs", 100);
        verdicts.finale_shown(&playing);
        assert_eq!(verdicts.finale().rows, Rows::Chips(vec![]));
        // A newer action in that app is still new work.
        verdicts.act((Some(3), Some(30)), "Docs", 200);
        assert_eq!(verdicts.finale().len(), 1);
    }

    #[test]
    fn the_finale_fits_the_well_by_shrinking_rows_then_counting_the_rest() {
        // The default well (320x200): five rows at full size.
        let fit = checklist_fit(200.0, 5);
        assert_eq!(fit, ChecklistFit { row_height: ROW_HEIGHT, gap: ROW_GAP, visible: 5, hidden: 0 });
        assert!(fit.height() <= 200.0 - 2.0 * FINALE_PAD);
        // The smallest well (228x144, from MIN_CARD 240x180): five rows
        // shrink to the floor and all stay visible.
        let fit = checklist_fit(144.0, 5);
        assert_eq!(fit.visible, 5);
        assert_eq!(fit.hidden, 0);
        assert!(fit.row_height >= ROW_HEIGHT_MIN && fit.row_height < ROW_HEIGHT);
        assert!(fit.height() <= 144.0 - 2.0 * FINALE_PAD, "{fit:?}");
        // A well too short even for the floor: the rest is counted.
        let fit = checklist_fit(100.0, 5);
        assert_eq!((fit.visible, fit.hidden), (2, 3), "{fit:?}");
        assert!(fit.height() <= 100.0 - 2.0 * FINALE_PAD, "{fit:?}");
        assert_eq!(fit.visible + fit.hidden, 5, "every result shown or counted");
        // One row always fits somewhere.
        assert_eq!(checklist_fit(10.0, 1).visible + checklist_fit(10.0, 1).hidden, 1);
        // Chips: five in one row at the default width, wrapped in two at
        // the smallest.
        assert_eq!(chip_grid(320.0, 5, 48.0, 12.0), (5, 1));
        assert_eq!(chip_grid(228.0, 5, 48.0, 12.0), (3, 2));
        assert_eq!(chip_grid(20.0, 2, 48.0, 12.0), (1, 2));
    }

    #[test]
    fn a_narrowed_well_shrinks_each_row_to_fit_and_truncates_its_label() {
        // A 500 pt row: its label is 500 minus the mark and pads.
        let text_w = 500.0 - LABEL_X - ROW_PAD;
        // Full width in a 600 pt well.
        assert_eq!(row_width(text_w, 600.0), 500.0);
        // Narrowed to 240: the capsule fits inside the well's insets, and
        // the label gets less room than its text (a tail ellipsis).
        let row_w = row_width(text_w, 240.0);
        assert!(ROW_INSET + row_w <= 240.0 - ROW_INSET, "{row_w}");
        assert!(row_w - LABEL_X - ROW_PAD < text_w);
        // A well narrower than its insets: no negative width.
        assert_eq!(row_width(text_w, 10.0), 0.0);
    }

    #[test]
    fn rows_come_in_70_ms_apart_and_the_panel_holds_before_fading() {
        assert_eq!(row_timing(0), (Duration::ZERO, ROW_IN));
        assert_eq!(
            row_timing(3),
            (Duration::from_millis(210), Duration::from_millis(360))
        );
        // Five rows: last row in at 280 ms, its mark lands at 430 + 180 ms,
        // then the 2 s hold: about 2.6 s in all.
        assert_eq!(finale_duration(5), Duration::from_millis(2610));
        assert_eq!(finale_duration(0), HOLD);
    }

    // ── Row: user close ──────────────────────────────────────────────────

    #[test]
    fn row_user_close_during_a_finale_ends_it_and_nothing_but_an_action_reopens() {
        let (mut verdicts, mut life) = proved();
        let (_, playing) = quiet(&mut verdicts, &mut life).unwrap();
        life.close();
        assert!(!life.playing(), "the close ends the finale");
        assert!(!life.end(playing), "its timer is stale");
        // New proof cannot bring it back.
        verdicts.verify(1, 10, 300, true, vec![claim("sent", Some(true))]);
        assert_eq!(quiet(&mut verdicts, &mut life), None);
        assert!(!life.playing());
        // Neither can end_session: the session is done, nothing plays.
        assert_eq!(end_session(&mut verdicts, &mut life, FINISHED, 400, true), Ending::Close);
        assert!(!life.playing());
        // Only a newer action does.
        assert_eq!(life.resume(500), Some(false));
        assert!(!life.closed());
    }

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
