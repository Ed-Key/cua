//! A panel's card stack: the window the agent is acting in is the front
//! card, and up to two windows it acted in recently are fanned behind it,
//! each a few points up and to the left of the card in front of it.
//!
//! Everything here is pure (unit tested): the stack model, where each card
//! rests, the spring that animates cards back to rest (after a promotion, or
//! trailing a drag), resize clamping, and the header layout at small sizes.
//! AppKit lives in `mod.rs`.

use std::time::{Duration, Instant};

use cursor_overlay::Spring;

use super::{Area, HEADER_HEIGHT};

/// Front card plus two behind it.
pub(super) const MAX_CARDS: usize = 3;
/// A back card drops this long after the session last acted in its window.
pub(super) const BACK_CARD_TTL: Duration = Duration::from_secs(30);
/// Each card sits this far up and left of the one in front of it. Also the
/// height of a back card's title strip, which is what peeks out above.
pub(super) const CARD_STEP: f64 = 14.0;
/// The farthest a back card trails its resting place while the panel is
/// dragged.
pub(super) const LAG_MAX: f64 = 10.0;
/// Transparent room above and left of the front card for the back cards and
/// their lag. Always reserved, so the front card never jumps when the stack
/// grows; fully transparent pixels let clicks through.
pub(super) const STACK_MARGIN: f64 = 2.0 * CARD_STEP + LAG_MAX;
/// Scale of the card at each depth, about its top-left corner.
const DEPTH_SCALE: [f64; MAX_CARDS] = [1.0, 0.95, 0.9];
/// Opacity of the card at each depth.
pub(super) const DEPTH_ALPHA: [f64; MAX_CARDS] = [1.0, 0.86, 0.66];
/// Natural frequency (rad/s) of the drag-follow spring: settles in ~150 ms.
pub(super) const FOLLOW_OMEGA: f64 = 40.0;
/// Natural frequency of the restack spring (a card moving to a new depth):
/// settles in ~250 ms.
pub(super) const RESTACK_OMEGA: f64 = 30.0;
/// Smallest front card the user can resize to.
pub(super) const MIN_CARD: (f64, f64) = (240.0, 180.0);
/// Largest front card, as a fraction of the screen's visible frame.
pub(super) const MAX_SCREEN_FRACTION: f64 = 0.6;
/// The live stream is resized once the well has been still this long.
pub(super) const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Width of the resize band just inside the front card's edges.
pub(super) const RESIZE_BAND: f64 = 6.0;
/// Pointer travel before a press on a card becomes a drag instead of a click.
pub(super) const DRAG_SLOP: f64 = 3.0;

// ── Stack model ───────────────────────────────────────────────────────────

pub(super) struct Card<K, V> {
    pub(super) key: K,
    /// When the session last acted in this window.
    pub(super) acted: Instant,
    pub(super) data: V,
}

/// Cards front to back: `cards[0]` is the front card.
pub(super) struct CardStack<K, V> {
    cards: Vec<Card<K, V>>,
}

impl<K: PartialEq + Copy, V: Default> CardStack<K, V> {
    pub(super) fn new() -> Self {
        Self { cards: Vec::new() }
    }

    pub(super) fn cards(&self) -> &[Card<K, V>] {
        &self.cards
    }

    pub(super) fn len(&self) -> usize {
        self.cards.len()
    }

    pub(super) fn keys(&self) -> Vec<K> {
        self.cards.iter().map(|card| card.key).collect()
    }

    pub(super) fn front_key(&self) -> Option<K> {
        self.cards.first().map(|card| card.key)
    }

    pub(super) fn front_mut(&mut self) -> Option<&mut Card<K, V>> {
        self.cards.first_mut()
    }

    /// The session acted in `key` at `now`: it becomes (or stays) the front
    /// card, the others keep their order behind it, and the deepest card
    /// beyond `MAX_CARDS` drops.
    pub(super) fn act(&mut self, key: K, now: Instant) {
        if !self.raise(key) {
            self.cards.insert(
                0,
                Card {
                    key,
                    acted: now,
                    data: V::default(),
                },
            );
            self.cards.truncate(MAX_CARDS);
        }
        self.cards[0].acted = now;
    }

    /// Bring a card already in the stack to the front without counting as
    /// the session acting there (a user click). Whether `key` is in the stack.
    pub(super) fn raise(&mut self, key: K) -> bool {
        let Some(index) = self.cards.iter().position(|card| card.key == key) else {
            return false;
        };
        let card = self.cards.remove(index);
        self.cards.insert(0, card);
        true
    }

    /// Drop back cards the session has not acted in for `BACK_CARD_TTL`, and
    /// back cards whose window is `gone`. The front card always stays.
    /// Whether anything dropped.
    pub(super) fn prune(&mut self, now: Instant, gone: impl Fn(&K) -> bool) -> bool {
        let before = self.cards.len();
        let mut depth = 0;
        self.cards.retain(|card| {
            depth += 1;
            depth == 1
                || (now.saturating_duration_since(card.acted) < BACK_CARD_TTL && !gone(&card.key))
        });
        self.cards.len() != before
    }
}

/// For each depth of the `new` stack order, the depth its window had in
/// the `old` order (`None` for a window new to the stack). Drives the
/// restack animation: a card starts where its window was drawn.
pub(super) fn previous_depths<K: PartialEq>(old: &[K], new: &[K]) -> Vec<Option<usize>> {
    new.iter()
        .map(|key| old.iter().position(|old| old == key))
        .collect()
}

/// The tag rule for back cards: a card shows a still only if the still was
/// captured from the card's own window.
pub(super) fn own_pixels<K: PartialEq, P>(key: K, still: Option<&(K, P)>) -> Option<&P> {
    still
        .filter(|(tag, _)| *tag == key)
        .map(|(_, pixels)| pixels)
}

// ── Layout ────────────────────────────────────────────────────────────────

/// Panel window size for a front card of `card` size.
pub(super) fn window_size(card: (f64, f64)) -> (f64, f64) {
    (card.0 + STACK_MARGIN, card.1 + STACK_MARGIN)
}

/// Front card size for a panel window of `window` size.
pub(super) fn card_size(window: (f64, f64)) -> (f64, f64) {
    (window.0 - STACK_MARGIN, window.1 - STACK_MARGIN)
}

/// Resting frame of the card at `depth` inside the panel (AppKit, origin
/// bottom-left). The front card fills the bottom-right; each card behind
/// it is `CARD_STEP` further up and left and slightly smaller, scaled about
/// its top-left corner so exactly its title strip and left edge peek out.
pub(super) fn rest_frame(card: (f64, f64), depth: usize) -> Area {
    let scale = DEPTH_SCALE[depth.min(MAX_CARDS - 1)];
    let step = depth as f64 * CARD_STEP;
    let (w, h) = (card.0 * scale, card.1 * scale);
    Area {
        x: STACK_MARGIN - step,
        y: card.1 + step - h,
        w,
        h,
    }
}

/// The card under `point`, front first: the lowest depth whose frame
/// contains it. `frames` is indexed by depth.
pub(super) fn card_at(point: (f64, f64), frames: &[Area]) -> Option<usize> {
    frames.iter().position(|frame| contains(frame, point))
}

fn contains(area: &Area, (x, y): (f64, f64)) -> bool {
    x >= area.x && x <= area.x + area.w && y >= area.y && y <= area.y + area.h
}

// ── Motion ────────────────────────────────────────────────────────────────

/// One critically damped spring step toward zero offset, with the cursor
/// overlay's integrator (semi-implicit Euler in four substeps, `Spring`
/// state). Critical damping (`c = 2ω`) settles without visible overshoot.
/// Returns whether the spring is still moving; a settled spring is zeroed.
pub(super) fn spring_step(spring: &mut Spring, omega: f64, dt: f64) -> bool {
    let (k, c) = (omega * omega, 2.0 * omega);
    let substeps = 4;
    let sdt = dt / substeps as f64;
    for _ in 0..substeps {
        spring.vx += (-k * spring.ox - c * spring.vx) * sdt;
        spring.vy += (-k * spring.oy - c * spring.vy) * sdt;
        spring.ox += spring.vx * sdt;
        spring.oy += spring.vy * sdt;
    }
    if spring.ox.hypot(spring.oy) < 0.25 && spring.vx.hypot(spring.vy) < 5.0 {
        *spring = Spring::default();
        return false;
    }
    true
}

/// A card's animated offset from its resting frame: position and size each
/// on a spring.
#[derive(Clone, Copy, Default)]
pub(super) struct Motion {
    pos: Spring,
    size: Spring,
    omega: f64,
}

impl Motion {
    /// Where the card is drawn: its resting frame plus the offset.
    pub(super) fn frame(&self, rest: Area) -> Area {
        Area {
            x: rest.x + self.pos.ox,
            y: rest.y + self.pos.oy,
            w: rest.w + self.size.ox,
            h: rest.h + self.size.oy,
        }
    }

    pub(super) fn moving(&self) -> bool {
        self.pos.ox != 0.0
            || self.pos.oy != 0.0
            || self.pos.vx != 0.0
            || self.pos.vy != 0.0
            || self.size.ox != 0.0
            || self.size.oy != 0.0
            || self.size.vx != 0.0
            || self.size.vy != 0.0
    }

    /// Start at `from` (where the card's window was drawn) and settle at
    /// `rest`.
    pub(super) fn restack(&mut self, from: Area, rest: Area) {
        self.pos = Spring {
            ox: from.x - rest.x,
            oy: from.y - rest.y,
            ..Spring::default()
        };
        self.size = Spring {
            ox: from.w - rest.w,
            oy: from.h - rest.h,
            ..Spring::default()
        };
        self.omega = RESTACK_OMEGA;
    }

    /// The panel moved by `delta`: a back card stays where it was on screen
    /// (up to `LAG_MAX` behind its resting place), then springs after it.
    pub(super) fn trail(&mut self, delta: (f64, f64)) {
        (self.pos.ox, self.pos.oy) = lag((self.pos.ox, self.pos.oy), delta);
        self.omega = FOLLOW_OMEGA;
    }

    /// Advance by `dt` seconds. Whether it is still moving.
    pub(super) fn step(&mut self, dt: f64) -> bool {
        let omega = if self.omega > 0.0 {
            self.omega
        } else {
            FOLLOW_OMEGA
        };
        let pos = spring_step(&mut self.pos, omega, dt);
        let size = spring_step(&mut self.size, omega, dt);
        pos || size
    }
}

/// A back card's offset after the panel moved by `delta`: pushed back by
/// the move, at most `LAG_MAX` from rest.
pub(super) fn lag(offset: (f64, f64), delta: (f64, f64)) -> (f64, f64) {
    let (x, y) = (offset.0 - delta.0, offset.1 - delta.1);
    let length = x.hypot(y);
    if length <= LAG_MAX {
        (x, y)
    } else {
        (x * LAG_MAX / length, y * LAG_MAX / length)
    }
}

// ── Resize ────────────────────────────────────────────────────────────────

/// Edges of the front card being resized, as `NSCursorFrameResizePosition`
/// bits.
pub(super) const TOP: u8 = 1 << 0;
pub(super) const LEFT: u8 = 1 << 1;
pub(super) const BOTTOM: u8 = 1 << 2;
pub(super) const RIGHT: u8 = 1 << 3;

/// Which edges of the front card `point` is on (0 = none): within
/// `RESIZE_BAND` inside an edge, corners combining two.
pub(super) fn resize_edges(point: (f64, f64), card: Area) -> u8 {
    if !contains(&card, point) {
        return 0;
    }
    let (x, y) = point;
    let mut edges = 0;
    if x <= card.x + RESIZE_BAND {
        edges |= LEFT;
    } else if x >= card.x + card.w - RESIZE_BAND {
        edges |= RIGHT;
    }
    if y <= card.y + RESIZE_BAND {
        edges |= BOTTOM;
    } else if y >= card.y + card.h - RESIZE_BAND {
        edges |= TOP;
    }
    edges
}

/// Largest front card on a screen whose visible frame is `visible` (w, h).
pub(super) fn max_card(visible: (f64, f64)) -> (f64, f64) {
    (
        (visible.0 * MAX_SCREEN_FRACTION).max(MIN_CARD.0),
        (visible.1 * MAX_SCREEN_FRACTION).max(MIN_CARD.1),
    )
}

/// `size` kept between `MIN_CARD` and `max`.
pub(super) fn clamp_card(size: (f64, f64), max: (f64, f64)) -> (f64, f64) {
    (
        size.0.min(max.0).max(MIN_CARD.0),
        size.1.min(max.1).max(MIN_CARD.1),
    )
}

/// The panel window frame (AppKit) after dragging `edges` of the front card
/// by `delta` from a window frame of `start`. The opposite edges stay put.
pub(super) fn resize_window(start: Area, edges: u8, delta: (f64, f64), max: (f64, f64)) -> Area {
    let (w0, h0) = card_size((start.w, start.h));
    let dw = match edges {
        e if e & RIGHT != 0 => delta.0,
        e if e & LEFT != 0 => -delta.0,
        _ => 0.0,
    };
    let dh = match edges {
        e if e & TOP != 0 => delta.1,
        e if e & BOTTOM != 0 => -delta.1,
        _ => 0.0,
    };
    let (w, h) = window_size(clamp_card((w0 + dw, h0 + dh), max));
    Area {
        x: if edges & LEFT != 0 {
            start.x + start.w - w
        } else {
            start.x
        },
        y: if edges & BOTTOM != 0 {
            start.y + start.h - h
        } else {
            start.y
        },
        w,
        h,
    }
}

/// Whether a well that last changed size at `changed` has been still long
/// enough to resize the live stream.
pub(super) fn resize_settled(changed: Instant, now: Instant) -> bool {
    now.saturating_duration_since(changed) >= RESIZE_DEBOUNCE
}

// ── Header layout ─────────────────────────────────────────────────────────

/// Frames (in the header's coordinates) of the header's views.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct HeaderLayout {
    pub(super) client_icon: Area,
    pub(super) client_label: Area,
    pub(super) target_icon: Area,
    pub(super) target_title: Area,
    pub(super) focus: Area,
    pub(super) close: Area,
}

/// Header layout for a card `width` wide whose client label wants
/// `label_width`. The buttons are pinned right; the client label gets at
/// most 120 pt and 40% of the room left of them, and the (truncating) target
/// title takes the rest, so every view stays inside the header at any width.
pub(super) fn header_layout(width: f64, label_width: f64) -> HeaderLayout {
    let y = (HEADER_HEIGHT - 16.0) / 2.0;
    let icon = |x: f64| Area {
        x,
        y,
        w: 16.0,
        h: 16.0,
    };
    let close_x = width - 8.0 - 20.0;
    let focus_x = close_x - 2.0 - 20.0;
    let room = (focus_x - 4.0 - 32.0).max(0.0);
    let label_w = label_width.min(120.0).min(room * 0.4).max(0.0);
    let target_x = 32.0 + label_w + 10.0;
    let title_x = target_x + 20.0;
    let button = |x: f64| Area {
        x,
        y: 4.0,
        w: 20.0,
        h: 20.0,
    };
    HeaderLayout {
        client_icon: icon(10.0),
        client_label: Area {
            x: 32.0,
            y,
            w: label_w,
            h: 16.0,
        },
        target_icon: icon(target_x),
        target_title: Area {
            x: title_x,
            y,
            w: (focus_x - 4.0 - title_x).max(0.0),
            h: 16.0,
        },
        focus: button(focus_x),
        close: button(close_x),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Stack = CardStack<u32, ()>;

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn acting_in_a_new_window_pushes_it_in_front_and_caps_at_three() {
        let t = Instant::now();
        let mut stack = Stack::new();
        stack.act(1, t);
        assert_eq!(stack.keys(), [1]);
        stack.act(2, t);
        assert_eq!(stack.keys(), [2, 1]);
        stack.act(3, t);
        stack.act(4, t);
        assert_eq!(stack.keys(), [4, 3, 2], "the oldest card drops past three");
    }

    #[test]
    fn acting_in_a_back_card_promotes_it_and_the_old_front_tucks_behind() {
        let t = Instant::now();
        let mut stack = Stack::new();
        for key in [1, 2, 3] {
            stack.act(key, t);
        }
        stack.act(1, at(t, 5));
        assert_eq!(stack.keys(), [1, 3, 2]);
        // Acting in the front card changes nothing but its time.
        stack.act(1, at(t, 6));
        assert_eq!(stack.keys(), [1, 3, 2]);
        assert_eq!(stack.cards()[0].acted, at(t, 6));
    }

    #[test]
    fn a_user_raise_reorders_without_counting_as_acting() {
        let t = Instant::now();
        let mut stack = Stack::new();
        stack.act(1, t);
        stack.act(2, at(t, 10));
        assert!(stack.raise(1));
        assert_eq!(stack.keys(), [1, 2]);
        assert_eq!(stack.cards()[0].acted, t);
        assert!(!stack.raise(9), "only windows already in the stack");
        assert_eq!(stack.keys(), [1, 2]);
    }

    #[test]
    fn back_cards_expire_after_thirty_quiet_seconds_but_the_front_never_does() {
        let t = Instant::now();
        let mut stack = Stack::new();
        stack.act(1, t);
        stack.act(2, at(t, 20));
        stack.act(3, at(t, 25));
        assert!(!stack.prune(at(t, 29), |_| false));
        assert!(stack.prune(at(t, 30), |_| false));
        assert_eq!(stack.keys(), [3, 2]);
        assert!(stack.prune(at(t, 100), |_| false));
        assert_eq!(stack.keys(), [3], "the front card stays however old");
    }

    #[test]
    fn a_back_card_drops_when_its_window_is_gone() {
        let t = Instant::now();
        let mut stack = Stack::new();
        for key in [1, 2, 3] {
            stack.act(key, t);
        }
        assert!(stack.prune(t, |key| *key == 2));
        assert_eq!(stack.keys(), [3, 1]);
        // The front card's window is the panel's business, not the stack's.
        assert!(!stack.prune(t, |key| *key == 3));
        assert_eq!(stack.keys(), [3, 1]);
    }

    #[test]
    fn restacked_cards_start_where_their_window_was() {
        // Promote 3 from the back: it came from depth 2; 1 and 2 shift back.
        assert_eq!(
            previous_depths(&[1, 2, 3], &[3, 1, 2]),
            [Some(2), Some(0), Some(1)]
        );
        // A new window in front has no previous depth.
        assert_eq!(
            previous_depths(&[1, 2], &[4, 1, 2]),
            [None, Some(0), Some(1)]
        );
    }

    #[test]
    fn back_cards_only_show_stills_of_their_own_window() {
        assert_eq!(own_pixels(5, Some(&(5, "w5"))), Some(&"w5"));
        assert_eq!(own_pixels(5, Some(&(6, "w6"))), None);
        assert_eq!(own_pixels::<u32, &str>(5, None), None);
    }

    const CARD: (f64, f64) = (336.0, 258.0);

    #[test]
    fn back_cards_fan_up_and_left_one_step_each_inside_the_window() {
        let (win_w, win_h) = window_size(CARD);
        let front = rest_frame(CARD, 0);
        assert_eq!(
            front,
            Area {
                x: STACK_MARGIN,
                y: 0.0,
                w: CARD.0,
                h: CARD.1
            }
        );
        assert_eq!(
            (front.x + front.w, front.y),
            (win_w, 0.0),
            "front fills the bottom-right"
        );
        for depth in 1..MAX_CARDS {
            let (card, above) = (rest_frame(CARD, depth), rest_frame(CARD, depth - 1));
            // Top-left corner one step up and left of the card in front.
            assert_eq!(card.x, above.x - CARD_STEP);
            assert!((card.y + card.h - (above.y + above.h + CARD_STEP)).abs() < 1e-9);
            // Smaller, and its right and bottom stay hidden behind the front
            // card even when trailing a drag by the full lag.
            assert!(card.w < above.w && card.h < above.h);
            assert!(card.x + card.w + LAG_MAX < front.x + front.w);
            assert!(card.y - LAG_MAX >= 0.0);
            // Its top-left, lag included, stays inside the window.
            assert!(card.x - LAG_MAX >= 0.0);
            assert!(card.y + card.h + LAG_MAX <= win_h);
        }
        // Same at the minimum card size.
        let small = rest_frame(MIN_CARD, 2);
        assert!(small.x + small.w + LAG_MAX < STACK_MARGIN + MIN_CARD.0);
    }

    #[test]
    fn clicks_hit_the_frontmost_card_under_the_pointer() {
        let frames: Vec<Area> = (0..MAX_CARDS)
            .map(|depth| rest_frame(CARD, depth))
            .collect();
        let front = frames[0];
        // Inside the front card, even where back cards lie beneath it.
        assert_eq!(
            card_at((front.x + 20.0, front.y + front.h - 5.0), &frames),
            Some(0)
        );
        // The strip peeking above the front card belongs to depth 1, the one
        // above that to depth 2.
        assert_eq!(
            card_at((front.x + 40.0, front.y + front.h + 7.0), &frames),
            Some(1)
        );
        assert_eq!(
            card_at(
                (front.x + 40.0, front.y + front.h + CARD_STEP + 7.0),
                &frames
            ),
            Some(2)
        );
        // The empty corner of the margin is nobody's.
        assert_eq!(card_at((1.0, 1.0), &frames), None);
        // With one card, the peek area is empty.
        assert_eq!(
            card_at((front.x + 40.0, front.y + front.h + 7.0), &frames[..1]),
            None
        );
    }

    fn settle(motion: &mut Motion, max_secs: f64) -> (f64, Vec<Area>) {
        let rest = rest_frame(CARD, 1);
        let dt = 1.0 / 60.0;
        let mut t = 0.0;
        let mut frames = vec![motion.frame(rest)];
        while motion.step(dt) {
            t += dt;
            frames.push(motion.frame(rest));
            assert!(t < max_secs, "still moving after {t:.3}s");
        }
        frames.push(motion.frame(rest));
        (t, frames)
    }

    #[test]
    fn a_trailing_card_catches_up_in_about_150_ms_without_overshoot() {
        let rest = rest_frame(CARD, 1);
        let mut motion = Motion::default();
        // The panel jumped 40pt right and 25pt up: the card trails by LAG_MAX.
        motion.trail((40.0, 25.0));
        let start = motion.frame(rest);
        assert!(((start.x - rest.x).hypot(start.y - rest.y) - LAG_MAX).abs() < 1e-9);
        let (secs, frames) = settle(&mut motion, 0.25);
        assert!((0.1..=0.2).contains(&secs), "settled in {secs:.3}s");
        assert_eq!(*frames.last().unwrap(), rest);
        // Critically damped: never passes rest by more than a hair.
        for frame in &frames {
            assert!(
                frame.x <= rest.x + 0.05 && frame.y <= rest.y + 0.05,
                "{frame:?}"
            );
        }
    }

    #[test]
    fn a_promoted_card_springs_to_the_front_in_about_a_quarter_second() {
        let (back, front) = (rest_frame(CARD, 2), rest_frame(CARD, 0));
        let mut motion = Motion::default();
        motion.restack(back, front);
        assert_eq!(motion.frame(front), back, "starts where the card was");
        let dt = 1.0 / 60.0;
        let mut t = 0.0;
        let mut widest = 0.0_f64;
        while motion.step(dt) {
            t += dt;
            widest = widest.max(motion.frame(front).w);
            assert!(t < 0.4, "still moving after {t:.3}s");
        }
        assert!((0.18..=0.35).contains(&t), "settled in {t:.3}s");
        assert_eq!(motion.frame(front), front);
        assert!(widest <= front.w + 0.5, "overshoot {}", widest - front.w);
    }

    #[test]
    fn the_spring_converges_from_any_offset_and_velocity() {
        for (ox, vx) in [(100.0, 0.0), (-50.0, 800.0), (0.0, -2000.0), (0.1, 0.0)] {
            let mut spring = Spring {
                ox,
                oy: -ox,
                vx,
                vy: vx,
            };
            let mut steps = 0;
            while spring_step(&mut spring, FOLLOW_OMEGA, 1.0 / 60.0) {
                steps += 1;
                assert!(steps < 120, "no convergence from {ox}, {vx}");
            }
            assert_eq!((spring.ox, spring.vx), (0.0, 0.0));
        }
    }

    #[test]
    fn lag_is_capped_and_keeps_its_direction() {
        assert_eq!(lag((0.0, 0.0), (3.0, 4.0)), (-3.0, -4.0));
        let (x, y) = lag((0.0, 0.0), (30.0, 40.0));
        assert!((x + 6.0).abs() < 1e-9 && (y + 8.0).abs() < 1e-9);
        // Moving back toward rest shrinks the lag.
        assert_eq!(lag((-3.0, -4.0), (-3.0, -4.0)), (0.0, 0.0));
    }

    #[test]
    fn resize_is_clamped_between_the_minimum_and_sixty_percent_of_the_screen() {
        let max = max_card((1440.0, 875.0));
        assert_eq!(max, (864.0, 525.0));
        assert_eq!(clamp_card((100.0, 100.0), max), MIN_CARD);
        assert_eq!(clamp_card((2000.0, 2000.0), max), max);
        assert_eq!(clamp_card((400.0, 300.0), max), (400.0, 300.0));
        // A screen too small for the maximum never pushes below the minimum.
        assert_eq!(max_card((300.0, 200.0)), MIN_CARD);
    }

    #[test]
    fn resizing_moves_only_the_dragged_edges() {
        let (w, h) = window_size(CARD);
        let start = Area {
            x: 1000.0,
            y: 100.0,
            w,
            h,
        };
        let max = (864.0, 525.0);
        // Right edge 50pt right: wider, origin fixed.
        let r = resize_window(start, RIGHT, (50.0, 7.0), max);
        assert_eq!(
            r,
            Area {
                w: w + 50.0,
                ..start
            }
        );
        // Bottom-left corner dragged down and left: the top-right stays put.
        let r = resize_window(start, BOTTOM | LEFT, (-40.0, -30.0), max);
        assert_eq!((r.w, r.h), (w + 40.0, h + 30.0));
        assert_eq!(
            (r.x + r.w, r.y + r.h),
            (start.x + start.w, start.y + start.h)
        );
        // Top edge far down: clamped at the minimum, bottom stays put.
        let r = resize_window(start, TOP, (0.0, -1000.0), max);
        assert_eq!(card_size((r.w, r.h)).1, MIN_CARD.1);
        assert_eq!(r.y, start.y);
        // Left edge far left: clamped at the maximum, right stays put.
        let r = resize_window(start, LEFT, (-5000.0, 0.0), max);
        assert_eq!(card_size((r.w, r.h)).0, max.0);
        assert_eq!(r.x + r.w, start.x + start.w);
    }

    #[test]
    fn resize_edges_are_a_thin_band_inside_the_front_card() {
        let card = rest_frame(CARD, 0);
        let mid = (card.x + card.w / 2.0, card.y + card.h / 2.0);
        assert_eq!(resize_edges(mid, card), 0);
        assert_eq!(resize_edges((card.x + card.w - 2.0, mid.1), card), RIGHT);
        assert_eq!(
            resize_edges((card.x + 2.0, card.y + 2.0), card),
            LEFT | BOTTOM
        );
        assert_eq!(
            resize_edges((card.x + card.w - 1.0, card.y + card.h - 1.0), card),
            TOP | RIGHT
        );
        assert_eq!(
            resize_edges((card.x - 2.0, mid.1), card),
            0,
            "outside the card"
        );
    }

    #[test]
    fn the_stream_resizes_only_after_the_well_is_still_for_150_ms() {
        let t = Instant::now();
        assert!(!resize_settled(t, t));
        assert!(!resize_settled(t, t + Duration::from_millis(149)));
        assert!(resize_settled(t, t + RESIZE_DEBOUNCE));
        assert!(!resize_settled(t + Duration::from_secs(1), t));
    }

    #[test]
    fn the_header_fits_at_every_width_with_buttons_pinned_right() {
        for width in [MIN_CARD.0, 280.0, 336.0, 600.0, 1200.0] {
            for label in [0.0, 60.0, 400.0] {
                let layout = header_layout(width, label);
                let views = [
                    layout.client_icon,
                    layout.client_label,
                    layout.target_icon,
                    layout.target_title,
                ];
                for view in views {
                    assert!(view.w >= 0.0 && view.x >= 0.0, "{width} {label}: {view:?}");
                    assert!(
                        view.x + view.w <= layout.focus.x,
                        "{width} {label}: {view:?} under focus"
                    );
                }
                assert_eq!(layout.close.x + layout.close.w, width - 8.0);
                assert!(layout.focus.x + layout.focus.w <= layout.close.x);
                assert!(layout.client_label.w <= 120.0);
            }
        }
        // The title keeps usable room at the minimum width.
        assert!(header_layout(MIN_CARD.0, 400.0).target_title.w >= 50.0);
    }
}
