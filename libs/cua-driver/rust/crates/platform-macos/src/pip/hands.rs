//! The user's hands on a panel: what keeps it up under the pointer, and the
//! card the user put in front.
//!
//! A panel has two clocks. The agent's (`Panel::last_action`, and the proof
//! timer in `finish`) counts from the session's last action and decides the
//! idle fade and every finale, exactly as before. The user's (`Hands`)
//! counts from the user's last interaction with the panel and only ever
//! keeps a shown panel up; it never restarts the agent's clock, so a proof
//! finale is not postponed by a resting pointer.
//!
//! A shown panel is HELD while the pointer is on it, while a press that
//! started on it lasts, or while the user's last interaction (the pointer
//! leaving, a release, a scroll, a click, the Focus button) is less than
//! the idle period old. A held panel does not fade when the agent goes
//! idle, does not hide when its target window becomes fully visible, and
//! drops no back card by age or capacity (see `stack::Keep`). The close
//! button still closes it, a session that ends still closes it, and a panel
//! that is not shown is never brought back by the pointer.
//!
//! "On the panel" is read from the real pointer position against every
//! visible surface of the panel (front card, bar, back cards, chips), by the
//! panel's pointer poll: entered and exited events are not trusted (a
//! warped pointer sends none, and their order across two panels is not
//! guaranteed). Moving between surfaces of one panel is not leaving it; the
//! transparent margin is not the panel; and another window over the panel
//! at that point (the overview sheet, a menu, another panel) means the
//! pointer is not on it.
//!
//! The PICK is the window whose card the user clicked: it stays the front
//! card until the user changes it. While a pick is set the agent acting in
//! another window does not take the front: that window joins or refreshes
//! as the first back card and its pictures go to its own card. Clicking the
//! card of the window the agent last acted in clears the pick and the panel
//! follows the agent again. The pick also goes when its window closes (the
//! panel then follows the agent's latest target, never another card in its
//! place) or the session ends.
//!
//! | Event | User clock | Hold | Pick | Back cards |
//! |---|---|---|---|---|
//! | Pointer comes onto the panel (any visible surface) | none | on while it stays | none | none expire or are evicted while held |
//! | Pointer leaves the panel | restarts: a full idle period starts now | lasts that period | none | as above until the hold is over; their own action times are never rewritten |
//! | Press on a card, the bar or a resize band | none | on until the release, wherever the pointer goes meanwhile | none | as above |
//! | Release (delivered, or found missing by the poll after `RELEASE_GRACE`), also outside the panel | restarts | lasts a full idle period from the release | a click (no drag, no resize) on a back card or chip: pick = that window, or cleared if it is the window the agent last acted in | as above |
//! | Agent acts in another window while a pick is set | none (the agent's clock and lifecycle run as ever) | none | stays in front | the acted window joins or refreshes as the first back card; its still goes to its own card |
//! | Agent acts in the picked window | none | none | stays | none |
//! | Picked window closes | none | none | cleared; the agent's latest window comes to the front if the stack still holds it | the closed window's card drops |
//! | Session ends | none | none: an ending panel closes as before | gone with the panel | as before |
//! | Scroll over a card or the bar (any phase, momentum, either axis) | restarts | lasts that period | none | none; the event is consumed, nothing scrolls and nothing under the panel scrolls (the transparent margin passes it through as before) |
//! | Focus button | restarts | lasts that period | none | none |
//! | Close button | none | over: close wins over every hold | none | none |
//! | Pointer over a resize band | none | as the pointer row | none | none; the system cursor is the matching resize arrow though the daemon is not the active app, and the arrow again once the pointer is off the band (see `cursor_step`) |
//! | Finale playing or due | none | a finale starts and ends on the agent's clock; the fade after it waits for the hold | none | none |
//! | Target window becomes fully visible | none | a held panel stays until the hold is over, then hides; a panel that is not shown stays hidden | kept | none |
//! | Panel hides | cleared | over | kept | as before |
//!
//! Everything here is pure (unit tested); AppKit lives in `mod.rs`.

use std::time::Instant;

use super::IDLE_HIDE_AFTER;

/// The user's side of one panel.
#[derive(Debug)]
pub(super) struct Hands<K> {
    /// The pointer is on the panel, as of the last poll.
    inside: bool,
    /// The user's last interaction, until the idle period after it is over.
    last: Option<Instant>,
    /// The window the user put in front.
    pick: Option<K>,
}

impl<K> Default for Hands<K> {
    fn default() -> Self {
        Self {
            inside: false,
            last: None,
            pick: None,
        }
    }
}

impl<K: Copy + PartialEq> Hands<K> {
    /// The poll's answer: whether the pointer is on the panel at `now`.
    /// Leaving is an interaction, so a full idle period follows it.
    pub(super) fn pointer(&mut self, inside: bool, now: Instant) {
        if self.inside && !inside {
            self.last = Some(now);
        }
        self.inside = inside;
    }

    /// The user did something with the panel at `now` (a release, a scroll,
    /// the Focus button).
    pub(super) fn touch(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// Whether the pointer is on the panel or the user's last interaction
    /// is less than the idle period old.
    pub(super) fn holds(&self, now: Instant) -> bool {
        self.inside
            || self
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < IDLE_HIDE_AFTER)
    }

    /// The user clock ran out with the pointer off the panel: true once, as
    /// the hold ends, so the caller re-checks the panel then.
    pub(super) fn lapse(&mut self, now: Instant) -> bool {
        let over = !self.inside && self.last.is_some() && !self.holds(now);
        if over {
            self.last = None;
        }
        over
    }

    /// The panel hid: nothing is held, and a pointer resting where it was
    /// does not bring it back. The pick stays for when it shows again.
    pub(super) fn hidden(&mut self) {
        self.inside = false;
        self.last = None;
    }

    pub(super) fn pick(&self) -> Option<K> {
        self.pick
    }

    /// The user clicked the back card or chip of `card`, which comes to the
    /// front: it is the pick, unless it is the window the agent last acted
    /// in (`agent`), which clears the pick so the panel follows the agent
    /// again.
    pub(super) fn click(&mut self, card: K, agent: Option<K>) {
        self.pick = (Some(card) != agent).then_some(card);
    }

    /// The pick is over (its window closed).
    pub(super) fn unpick(&mut self) {
        self.pick = None;
    }
}

/// Whether the user holds a panel up at `now`: a press that started on it
/// (`pressed`), or, only while it is `shown`, the pointer on it or a recent
/// interaction.
pub(super) fn held<K: Copy + PartialEq>(
    shown: bool,
    pressed: bool,
    hands: &Hands<K>,
    now: Instant,
) -> bool {
    pressed || (shown && hands.holds(now))
}

/// Whether a panel is up. `agent` is everything the agent's side keeps it
/// up for (active within the idle period, a finale playing, proof waiting
/// for its finale on a shown panel); `held` is the user's side. The user's
/// close wins over both; a fully visible target hides the panel unless a
/// finale plays or the user holds it.
pub(super) fn shows(
    agent: bool,
    held: bool,
    closed: bool,
    target_visible: bool,
    finale: bool,
) -> bool {
    super::panel_should_show(agent || held, closed, target_visible && !finale && !held)
}

/// Whether the agent acting in `acted` takes the front card: always with no
/// pick, and with one only in the picked window itself.
pub(super) fn agent_takes_front<K: PartialEq>(pick: Option<K>, acted: K) -> bool {
    pick.is_none_or(|pick| pick == acted)
}

/// Which panel last set the system cursor, and to which resize edges.
pub(super) type CursorOwner = Option<(i64, u8)>;

/// One look at the pointer for panel `id`: `edges` are the front card's
/// resize edges under it (0 when it is on no band of this panel, or not on
/// this panel at all). The cursor to set now, if any: the resize cursor for
/// new edges, or the arrow (`Some(0)`) when this panel's resize cursor is
/// showing and the pointer is off its bands. A panel that does not own the
/// cursor never resets it, so a late look from the panel the pointer left
/// cannot clear the cursor of the panel it is on now.
pub(super) fn cursor_step(owner: &mut CursorOwner, id: i64, edges: u8) -> Option<u8> {
    match (*owner, edges) {
        (Some((owning, showing)), _) if owning == id && showing == edges => None,
        (Some((owning, _)), 0) if owning == id => {
            *owner = None;
            Some(0)
        }
        (_, 0) => None,
        _ => {
            *owner = Some((id, edges));
            Some(edges)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::finish;
    use super::*;
    use std::time::Duration;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    /// Two clocks: the pointer resting on the panel, then leaving, holds it
    /// for a full idle period, and never moves the agent's clock, so the
    /// proof finale comes due exactly when it would have.
    #[test]
    fn the_user_clock_holds_the_panel_and_leaves_the_agents_clock_alone() {
        let t = Instant::now();
        let last_action = t;
        let proof = Some(at(t, 1_000));
        let mut hands = Hands::<u32>::default();
        assert!(!hands.holds(t), "nobody touched it");
        hands.pointer(true, at(t, 2_000));
        // 15 s past the agent's idle deadline, still held.
        assert!(hands.holds(at(t, 23_000)));
        assert!(held(true, false, &hands, at(t, 23_000)));
        assert!(shows(false, held(true, false, &hands, at(t, 23_000)), false, false, false));
        // The agent's side is untouched: idle at 8 s, proof quiet 8 s after
        // the proof, whatever the pointer does.
        assert!(super::super::idle_hide_due(last_action, at(t, 8_000)));
        assert!(!finish::proof_quiet(last_action, proof, at(t, 8_999)));
        assert!(finish::proof_quiet(last_action, proof, at(t, 9_000)));
        // Leaving starts a full idle period; then the hold is over, once.
        hands.pointer(false, at(t, 30_000));
        assert!(hands.holds(at(t, 37_999)));
        assert!(!hands.lapse(at(t, 37_999)));
        assert!(!hands.holds(at(t, 38_000)));
        assert!(hands.lapse(at(t, 38_000)));
        assert!(!hands.lapse(at(t, 38_100)), "reported once");
        assert!(!shows(false, held(true, false, &hands, at(t, 38_000)), false, false, false));
        // Moving between surfaces of the panel is not leaving: no stamp.
        let mut resting = Hands::<u32>::default();
        resting.pointer(true, t);
        resting.pointer(true, at(t, 5_000));
        resting.pointer(false, at(t, 20_000));
        assert!(resting.holds(at(t, 27_999)), "the period counts from the real leave");
    }

    /// A press holds its panel until the release wherever the pointer is;
    /// the release (delivered or synthesized, inside or outside the panel)
    /// stamps the user clock, so a full idle period follows it.
    #[test]
    fn a_press_holds_until_its_release_and_the_release_starts_a_full_period() {
        let t = Instant::now();
        let mut hands = Hands::<u32>::default();
        hands.pointer(true, t);
        // Dragged off the panel (a resize past its largest size) at 1 s:
        // the pointer leaving stamps, but the press is what holds.
        hands.pointer(false, at(t, 1_000));
        assert!(held(true, true, &hands, at(t, 12_000)), "the press holds past idle");
        assert!(!held(true, false, &hands, at(t, 12_000)), "without it the panel would go");
        // Released outside at 12 s.
        hands.touch(at(t, 12_000));
        assert!(held(true, false, &hands, at(t, 19_999)));
        assert!(!held(true, false, &hands, at(t, 20_000)));
        assert!(hands.lapse(at(t, 20_000)));
    }

    /// The show rule with a hold: the hold keeps an idle panel up and
    /// postpones the visibility hide; the close button wins over it; a
    /// panel that is not shown is not brought back by the pointer.
    #[test]
    fn a_hold_postpones_the_idle_and_visibility_hides_but_never_the_close() {
        let t = Instant::now();
        let mut hands = Hands::<u32>::default();
        hands.pointer(true, t);
        let now = at(t, 20_000);
        // Idle agent, pointer on the shown panel.
        assert!(shows(false, held(true, false, &hands, now), false, false, false));
        // H13: the target becomes fully visible under the pointer.
        assert!(shows(true, held(true, false, &hands, now), false, true, false));
        assert!(!shows(true, false, false, true, false), "no hold: hidden as before");
        // The close button wins.
        assert!(!shows(true, held(true, false, &hands, now), true, false, false));
        // Not shown: the pointer where the panel was holds nothing.
        assert!(!held(false, false, &hands, now));
        assert!(!shows(false, held(false, false, &hands, now), false, true, false));
        // Hiding clears the hold; the pick survives it.
        hands.click(7, Some(9));
        hands.hidden();
        assert!(!hands.holds(now));
        assert_eq!(hands.pick(), Some(7));
    }

    /// H5 to H8: a clicked card is the pick; the agent then takes the front
    /// only in the picked window; clicking the agent's own card clears the
    /// pick.
    #[test]
    fn a_pick_stays_until_the_user_changes_it() {
        let mut hands = Hands::<u32>::default();
        let agent = Some(2);
        assert!(agent_takes_front(hands.pick(), 2), "no pick: the panel follows the agent");
        // H5: the user clicks window 1's back card.
        hands.click(1, agent);
        assert_eq!(hands.pick(), Some(1));
        // H6: the agent acts in another window.
        assert!(!agent_takes_front(hands.pick(), 2));
        assert!(!agent_takes_front(hands.pick(), 3));
        // H7: the agent acts in the picked window.
        assert!(agent_takes_front(hands.pick(), 1));
        assert_eq!(hands.pick(), Some(1), "acting never changes the pick");
        // Another card is a new pick.
        hands.click(4, Some(3));
        assert_eq!(hands.pick(), Some(4));
        // H8: the card of the window the agent last acted in.
        hands.click(3, Some(3));
        assert_eq!(hands.pick(), None);
        assert!(agent_takes_front(hands.pick(), 5));
    }

    /// A pick whose window closed is cleared, and the panel follows the
    /// agent's latest target only if its card is still behind: never
    /// another card in its place.
    #[test]
    fn a_stale_pick_clears_and_the_agents_window_comes_forward() {
        use super::super::stack::click_target;
        let mut hands = Hands::<u32>::default();
        hands.click(1, Some(2));
        hands.unpick();
        assert_eq!(hands.pick(), None);
        // The stack still holds the agent's window behind the old pick.
        assert_eq!(click_target(Some(2), &[1, 3, 2]), Some(2));
        // It does not (expired, closed): nothing is raised in its place.
        assert_eq!(click_target(Some(2), &[1, 3]), None);
        // It is already the front card, or the agent never acted.
        assert_eq!(click_target(Some(1), &[1, 3]), None);
        assert_eq!(click_target(None::<u32>, &[1, 3]), None);
    }

    /// H11 with two panels: the pointer goes from panel 1's band straight
    /// to panel 2's, and panel 1's late look must not reset the cursor.
    #[test]
    fn a_panel_only_resets_a_cursor_it_set() {
        let mut owner = None;
        assert_eq!(cursor_step(&mut owner, 1, 0), None, "nothing to reset");
        assert_eq!(cursor_step(&mut owner, 1, 8), Some(8));
        assert_eq!(cursor_step(&mut owner, 1, 8), None, "already showing");
        assert_eq!(cursor_step(&mut owner, 1, 9), Some(9), "onto the corner");
        // Onto panel 2's band before panel 1 looked again.
        assert_eq!(cursor_step(&mut owner, 2, 2), Some(2));
        assert_eq!(cursor_step(&mut owner, 1, 0), None, "panel 2's cursor stays");
        assert_eq!(owner, Some((2, 2)));
        // Off panel 2's band: the arrow, once.
        assert_eq!(cursor_step(&mut owner, 2, 0), Some(0));
        assert_eq!(cursor_step(&mut owner, 2, 0), None);
        assert_eq!(owner, None);
    }
}
