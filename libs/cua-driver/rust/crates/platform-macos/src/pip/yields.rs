//! The PiP steps aside for the agent it shows.
//!
//! A panel floats above every app, so a foreground pointer gesture (click,
//! double-click, right-click, drag) at a point under one would reach the
//! panel instead of the agent's window, and the foreground admission check
//! (`input::skylight`) refuses it. So before admission, cua's own panels
//! that the gesture crosses (the click point, or any point of the drag's
//! straight path; the rectangular window, margin included, as the
//! admission check sees it) are ordered out at once, the admission runs
//! against what is underneath, the whole gesture is delivered, and then the
//! panels come back. Only window-scoped foreground pointer delivery does
//! this: background clicks, point scrolls and AX actions are addressed to
//! the target and never reach a panel; desktop-scope input and interactive
//! (stateful) input post globally without this admission and are not
//! covered. PiP exists only on macOS.
//!
//! | Event | Panel | Agent's gesture |
//! |---|---|---|
//! | Gesture crosses a panel nobody's hands are on | ordered out now (no fade), its count goes up | admitted against what is underneath, then delivered |
//! | Gesture crosses a panel the user presses, drags, resizes, or has the pointer on (it moved there), a missed release still pending included | untouched | waits (off the main queue) until that ends, then as above; refused `pip_held_by_user` after `USER_WAIT` |
//! | The user's clock after a release (8 s) | untouched | does not wait |
//! | Gesture crosses no panel | untouched | as before |
//! | Two gestures overlap on one panel | the count is per panel: it stays out until the last gesture ends | each as above |
//! | A panel appears (or shows again) across a gesture in flight | joins that gesture: stays out until it ends | unaffected |
//! | Frames, verdicts, finales, the user clock, session end while out | run as ever; only the window stays out | unaffected |
//! | Gesture ends (delivered, refused by admission, failed after the press, cancelled) | count down; back in front only at zero and only if it is still meant to show (not closed, ended, idle-hidden or hidden by the visibility rule meanwhile) | no input replayed |
//! | Back in front under the pointer the agent left there | a resting pointer: holds nothing until it moves | unaffected |
//!
//! Everything here is pure (unit tested); AppKit lives in `mod.rs`.

use std::time::{Duration, Instant};

use super::Area;

/// How long an agent gesture waits for the user's hands to leave a panel it
/// crosses before it is refused.
pub(super) const USER_WAIT: Duration = Duration::from_secs(5);

/// How often the waiting gesture looks again.
pub(super) const USER_POLL: Duration = Duration::from_millis(50);

/// Slack (points) around a panel's window: a point on its edge counts.
const SLACK: f64 = 1.0;

/// Whether `path` (screen points, CoreGraphics top-left origin: one point
/// for a click, the start and end of a straight drag) touches `area`.
pub(super) fn crosses(area: Area, path: &[(f64, f64)]) -> bool {
    let (x0, y0) = (area.x - SLACK, area.y - SLACK);
    let (x1, y1) = (area.x + area.w + SLACK, area.y + area.h + SLACK);
    let inside = |(x, y): (f64, f64)| x >= x0 && x <= x1 && y >= y0 && y <= y1;
    match path {
        [] => false,
        [point] => inside(*point),
        _ => path
            .windows(2)
            .any(|pair| segment_hits(pair[0], pair[1], (x0, y0, x1, y1))),
    }
}

/// Liang-Barsky: whether the segment `a`-`b` meets the closed box.
fn segment_hits(a: (f64, f64), b: (f64, f64), (x0, y0, x1, y1): (f64, f64, f64, f64)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, a.0 - x0),
        (dx, x1 - a.0),
        (-dy, a.1 - y0),
        (dy, y1 - a.1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            enter = enter.max(t);
        } else {
            leave = leave.min(t);
        }
        if enter > leave {
            return false;
        }
    }
    true
}

/// One of cua's panels as a gesture sees it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Candidate {
    pub(super) id: i64,
    /// Its window (CoreGraphics, top-left origin).
    pub(super) area: Area,
    /// The user's hands are on it: a press that started on it (a drag, a
    /// resize, a missed release still pending), or the pointer on it having
    /// moved there. The user clock after a release is not this.
    pub(super) user: bool,
}

#[derive(Debug, PartialEq)]
pub(super) enum Step {
    /// Order these panels out for the gesture (none: nothing is in the way).
    Aside(Vec<i64>),
    /// The user holds a panel the gesture crosses.
    Wait,
}

/// What a gesture along `path` does about `panels` (every panel on screen
/// or already out for another gesture).
pub(super) fn step(panels: &[Candidate], path: &[(f64, f64)]) -> Step {
    let crossed: Vec<&Candidate> = panels.iter().filter(|p| crosses(p.area, path)).collect();
    if crossed.iter().any(|p| p.user) {
        return Step::Wait;
    }
    Step::Aside(crossed.iter().map(|p| p.id).collect())
}

/// A gesture that started waiting at `since` is refused at `now`.
pub(super) fn give_up(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= USER_WAIT
}

/// A panel's count after one more gesture takes it out, and whether its
/// window has to be ordered out now (it was not out yet).
pub(super) fn take(count: u32) -> (u32, bool) {
    (count + 1, count == 0)
}

/// A panel's count after one gesture lets it go, and whether its window
/// comes back in front now: at zero, and only if the panel is still meant
/// to be shown (`shown`: not closed, idle-hidden or hidden by the
/// visibility rule meanwhile; an ended and closed panel is gone and never
/// asked).
pub(super) fn give_back(count: u32, shown: bool) -> (u32, bool) {
    match count {
        0 => (0, false),
        _ => (count - 1, count == 1 && shown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panel window at x 1100..1420, y 600..820 (CoreGraphics).
    const PANEL: Area = Area {
        x: 1100.0,
        y: 600.0,
        w: 320.0,
        h: 220.0,
    };

    fn candidate(id: i64, area: Area, user: bool) -> Candidate {
        Candidate { id, area, user }
    }

    #[test]
    fn a_click_point_crosses_only_the_panel_it_is_on() {
        assert!(crosses(PANEL, &[(1200.0, 700.0)]));
        assert!(crosses(PANEL, &[(1100.0, 600.0)]), "the corner counts");
        assert!(crosses(PANEL, &[(1420.5, 820.5)]), "within the slack of the edge");
        assert!(!crosses(PANEL, &[(1000.0, 700.0)]));
        assert!(!crosses(PANEL, &[(1200.0, 590.0)]));
        assert!(!crosses(PANEL, &[]));
    }

    /// A drag crosses a panel anywhere along its straight path, not only at
    /// its ends.
    #[test]
    fn a_drag_crosses_every_panel_on_its_path() {
        assert!(crosses(PANEL, &[(1200.0, 700.0), (500.0, 700.0)]), "starts on it");
        assert!(crosses(PANEL, &[(500.0, 700.0), (1200.0, 700.0)]), "ends on it");
        assert!(crosses(PANEL, &[(1000.0, 700.0), (1500.0, 700.0)]), "passes over it");
        assert!(crosses(PANEL, &[(1000.0, 500.0), (1500.0, 900.0)]), "diagonally through it");
        assert!(!crosses(PANEL, &[(1000.0, 500.0), (1000.0, 900.0)]), "beside it");
        assert!(!crosses(PANEL, &[(900.0, 500.0), (1300.0, 580.0)]), "above it");
        assert!(crosses(PANEL, &[(1200.0, 700.0), (1200.0, 700.0)]), "a drag of no length on it");
    }

    #[test]
    fn a_gesture_takes_out_every_panel_it_crosses_and_no_other() {
        let other = Area { y: 300.0, ..PANEL };
        let panels = [candidate(1, PANEL, false), candidate(2, other, false)];
        assert_eq!(step(&panels, &[(1200.0, 700.0)]), Step::Aside(vec![1]));
        assert_eq!(step(&panels, &[(1200.0, 400.0), (1200.0, 700.0)]), Step::Aside(vec![1, 2]));
        assert_eq!(step(&panels, &[(200.0, 200.0)]), Step::Aside(vec![]));
    }

    /// The user's hands on a crossed panel make the gesture wait; hands on a
    /// panel it does not cross do not.
    #[test]
    fn the_users_hands_on_a_crossed_panel_make_the_gesture_wait() {
        let other = Area { y: 300.0, ..PANEL };
        let panels = [candidate(1, PANEL, false), candidate(2, other, true)];
        assert_eq!(step(&panels, &[(1200.0, 700.0)]), Step::Aside(vec![1]));
        assert_eq!(step(&panels, &[(1200.0, 400.0)]), Step::Wait);
        assert_eq!(step(&panels, &[(1200.0, 700.0), (1200.0, 350.0)]), Step::Wait);
    }

    /// The decision reads the pointer as of now through `Hands`: a pointer
    /// that moved onto the panel after the last hover poll holds it, and the
    /// gesture waits; one that was resting there when the panel appeared
    /// does not.
    #[test]
    fn a_pointer_that_arrived_after_the_last_poll_makes_the_gesture_wait() {
        use super::super::hands::Hands;
        let start = Instant::now();
        let (off, on) = ((10.0, 10.0), (1200.0, 700.0));
        let mut hands: Hands<u32> = Hands::default();
        hands.shown(off);
        hands.pointer(false, off, start); // the last poll: off the panel
        assert!(!hands.pointer_holds(), "as the poll left it");
        hands.pointer(true, on, start + Duration::from_millis(60)); // the decision's own look
        let panels = [candidate(1, PANEL, hands.pointer_holds())];
        assert_eq!(step(&panels, &[on]), Step::Wait);

        let mut resting: Hands<u32> = Hands::default();
        resting.shown(on);
        resting.pointer(true, on, start + Duration::from_millis(60));
        let panels = [candidate(1, PANEL, resting.pointer_holds())];
        assert_eq!(step(&panels, &[on]), Step::Aside(vec![1]));
    }

    #[test]
    fn a_waiting_gesture_is_refused_after_five_seconds() {
        let start = Instant::now();
        assert!(!give_up(start, start + Duration::from_millis(4950)));
        assert!(give_up(start, start + USER_WAIT));
    }

    /// Two gestures overlapping on one panel: the first to end does not
    /// bring it back while the other still has it out.
    #[test]
    fn overlapping_gestures_compose_on_one_panel() {
        let (count, order_out) = take(0);
        assert_eq!((count, order_out), (1, true));
        let (count, order_out) = take(count);
        assert_eq!((count, order_out), (2, false), "already out: no second order-out");
        let (count, back) = give_back(count, true);
        assert_eq!((count, back), (1, false), "the other gesture still has it out");
        let (count, back) = give_back(count, true);
        assert_eq!((count, back), (0, true));
    }

    /// A panel hidden meanwhile (closed by the user, idle, its target in
    /// full view) stays hidden when the gesture lets it go.
    #[test]
    fn a_panel_hidden_meanwhile_is_not_brought_back() {
        let (count, _) = take(0);
        assert_eq!(give_back(count, false), (0, false));
        assert_eq!(give_back(0, true), (0, false), "a stray give-back changes nothing");
    }
}
