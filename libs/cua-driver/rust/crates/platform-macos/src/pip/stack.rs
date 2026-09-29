//! A panel's card stack: the window the agent is acting in is the front
//! card, and up to three windows it acted in recently are back items. A back
//! window the session is not finished with is a card fanned behind the
//! front one, each a few points up and to the left of the card in front of
//! it; a finished one collapses into a chip (app icon, green check) in a
//! column left of the cards. Back items live in a trail window that follows
//! the panel on a loose spring.
//!
//! Everything here is pure (unit tested): the stack model, where each item
//! rests, the springs that animate items back to rest (after a promotion)
//! and the trail after a drag, resize clamping, and the header layout at
//! small sizes. AppKit lives in `mod.rs`.

use std::time::{Duration, Instant};

use cursor_overlay::Spring;

use super::{Area, HEADER_HEIGHT};

/// Front card plus up to three back items (cards and chips together).
pub(super) const MAX_CARDS: usize = 4;
/// A back card drops this long after the session last acted in its window.
pub(super) const BACK_CARD_TTL: Duration = Duration::from_secs(30);
/// Each card sits this far up and left of the one in front of it. Also the
/// height of a back card's title strip, which is what peeks out above.
pub(super) const CARD_STEP: f64 = 14.0;
/// Transparent room above and left of the front card where the back cards
/// rest (they are drawn by the trail window, underneath). Always reserved,
/// so the front card never jumps when the stack grows and a card promoted
/// from the back animates inside the panel; fully transparent pixels let
/// clicks through.
pub(super) const STACK_MARGIN: f64 = (MAX_CARDS - 1) as f64 * CARD_STEP;
/// Scale of the card at each depth, about its top-left corner.
const DEPTH_SCALE: [f64; MAX_CARDS] = [1.0, 0.95, 0.9, 0.85];
/// Opacity of the card at each depth.
pub(super) const DEPTH_ALPHA: [f64; MAX_CARDS] = [1.0, 0.86, 0.66, 0.5];
/// Natural frequency of the restack spring (an item moving to a new place):
/// settles in ~250 ms.
pub(super) const RESTACK_OMEGA: f64 = 30.0;
/// Chip: a glass circle holding the app icon, with a caption under it.
pub(super) const CHIP: f64 = 40.0;
pub(super) const CHIP_ICON: f64 = 28.0;
pub(super) const CHIP_CAPTION: f64 = 12.0;
/// A chip's whole frame (circle and caption).
pub(super) const CHIP_W: f64 = 56.0;
pub(super) const CHIP_H: f64 = CHIP + 2.0 + CHIP_CAPTION;
/// Between chips in the column, and between the column and the cards.
const CHIP_ROW_GAP: f64 = 6.0;
const CHIP_GAP: f64 = 8.0;
/// The trail window extends this far left of the panel window, for chips.
pub(super) const TRAIL_PAD: f64 = CHIP_GAP + CHIP_W;
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

/// The window a click raises: the back card pressed (`pressed`, taken at
/// mouse-down) if it is still a back card in `keys` when the button comes
/// up; `None` (cancel) if it dropped or already came to the front.
pub(super) fn click_target<K: PartialEq + Copy>(pressed: Option<K>, keys: &[K]) -> Option<K> {
    let pressed = pressed?;
    keys.iter()
        .skip(1)
        .any(|key| *key == pressed)
        .then_some(pressed)
}

/// The tag rule for back cards: a card shows a still only if the still was
/// captured from the card's own window.
pub(super) fn own_pixels<K: PartialEq, P>(key: K, still: Option<&(K, P)>) -> Option<&P> {
    still
        .filter(|(tag, _)| *tag == key)
        .map(|(_, pixels)| pixels)
}

/// Where a stack item is drawn: the front card (in the panel), a back card
/// at a depth (1 = right behind the front), or a chip in the chip column
/// (0 = bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    Front,
    Card(usize),
    Chip(usize),
}

/// Views a panel has for its items: the front card, a card per back depth,
/// a chip per back item.
pub(super) const VIEWS: usize = 1 + 2 * (MAX_CARDS - 1);

impl Slot {
    /// Index of the view that draws this slot, `0..VIEWS`.
    pub(super) fn view(self) -> usize {
        match self {
            Slot::Front => 0,
            Slot::Card(depth) => depth,
            Slot::Chip(row) => MAX_CARDS + row,
        }
    }

    /// Resting opacity.
    pub(super) fn alpha(self) -> f64 {
        match self {
            Slot::Front => DEPTH_ALPHA[0],
            Slot::Card(depth) => DEPTH_ALPHA[depth.min(MAX_CARDS - 1)],
            Slot::Chip(_) => 1.0,
        }
    }
}

/// Each item's slot, front first, from whether each item's window is
/// finished: back items the session finished with are chips (in stack
/// order, bottom up), the rest are cards (in stack order, front to back).
/// The front card stays a card either way.
pub(super) fn slots(finished: &[bool]) -> Vec<Slot> {
    let (mut cards, mut chips) = (0, 0);
    finished
        .iter()
        .enumerate()
        .map(|(index, &done)| match (index, done) {
            (0, _) => Slot::Front,
            (_, true) => {
                chips += 1;
                Slot::Chip(chips - 1)
            }
            (_, false) => {
                cards += 1;
                Slot::Card(cards)
            }
        })
        .collect()
}

/// Resting frame of `slot` in panel coordinates (AppKit, origin
/// bottom-left of the panel window) for a front card of `card` size, with
/// `back_cards` cards behind it. Chips sit in a column just left of the
/// leftmost card, from the bottom up; they may extend left of the panel
/// window (into the trail window's `TRAIL_PAD`).
pub(super) fn slot_frame(card: (f64, f64), slot: Slot, back_cards: usize) -> Area {
    match slot {
        Slot::Front => rest_frame(card, 0),
        Slot::Card(depth) => rest_frame(card, depth),
        Slot::Chip(row) => Area {
            x: STACK_MARGIN - back_cards as f64 * CARD_STEP - CHIP_GAP - CHIP_W,
            y: row as f64 * (CHIP_H + CHIP_ROW_GAP),
            w: CHIP_W,
            h: CHIP_H,
        },
    }
}

/// The stack item under `point` (panel coordinates): the front card when
/// the press is in the panel, else a back item in the trail window, chips
/// first (they never overlap cards), then cards front to back. `frames` are
/// where each item of `layout` is drawn.
pub(super) fn item_at(
    point: (f64, f64),
    layout: &[Slot],
    frames: &[Area],
    in_trail: bool,
) -> Option<usize> {
    let mut order: Vec<usize> = (0..layout.len().min(frames.len()))
        .filter(|&index| (layout[index] != Slot::Front) == in_trail)
        .collect();
    order.sort_by_key(|&index| match layout[index] {
        Slot::Front | Slot::Chip(_) => 0,
        Slot::Card(depth) => depth,
    });
    order
        .into_iter()
        .find(|&index| contains(&frames[index], point))
}

/// Number of back cards among `slots`.
pub(super) fn back_cards(slots: &[Slot]) -> usize {
    slots
        .iter()
        .filter(|slot| matches!(slot, Slot::Card(_)))
        .count()
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

fn contains(area: &Area, (x, y): (f64, f64)) -> bool {
    x >= area.x && x <= area.x + area.w && y >= area.y && y <= area.y + area.h
}

// ── Motion ────────────────────────────────────────────────────────────────

/// One critically damped spring step toward zero offset (see
/// `damped_step`). Critical damping settles without visible overshoot.
pub(super) fn spring_step(spring: &mut Spring, omega: f64, dt: f64) -> bool {
    damped_step(spring, omega, 1.0, dt)
}

/// One spring step toward zero offset with damping ratio `zeta`, with the
/// cursor overlay's integrator (semi-implicit Euler in four substeps,
/// `Spring` state). Damping acts on the spring's own (on-screen) velocity.
/// Returns whether the spring is still moving; a settled spring is zeroed.
pub(super) fn damped_step(spring: &mut Spring, omega: f64, zeta: f64, dt: f64) -> bool {
    let (k, c) = (omega * omega, 2.0 * zeta * omega);
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

    /// Advance by `dt` seconds. Whether it is still moving.
    pub(super) fn step(&mut self, dt: f64) -> bool {
        let omega = if self.omega > 0.0 {
            self.omega
        } else {
            RESTACK_OMEGA
        };
        let pos = spring_step(&mut self.pos, omega, dt);
        let size = spring_step(&mut self.size, omega, dt);
        pos || size
    }
}

// ── Loose trail ───────────────────────────────────────────────────────────

/// The feel of the trail (back cards and chips) following a dragged panel,
/// tuned to Codex Computer Use's PiP: loose and a little playful. At a
/// normal drag speed (~800 pt/s) the trail hangs ~83 pt behind (measured
/// right after each drag event, when it has not moved yet); when the drag
/// stops it swings ~7 pt past its resting place and settles in ~0.6 s.
/// Between events the lag is `2·ζ·v/ω`: lower `TRAIL_OMEGA` is looser (more
/// lag, slower settle), lower `TRAIL_ZETA` is bouncier (0.65 gives a small
/// visible overshoot; 1.0 would give none).
pub(super) const TRAIL_OMEGA: f64 = 14.5;
pub(super) const TRAIL_ZETA: f64 = 0.65;
/// Lag never exceeds this, so a violent fling cannot throw the trail far
/// off its panel.
pub(super) const TRAIL_LAG_CAP: f64 = 140.0;

/// The trail window's offset from its resting place, and the measurement
/// of the last drag's lag (for logs).
#[derive(Default)]
pub(super) struct Trail {
    spring: Spring,
    /// A drag is moving the panel.
    dragging: bool,
    /// Largest lag since the current drag started.
    max_lag: f64,
    /// When the last drag ended, until its settle is reported.
    released: Option<Instant>,
}

impl Trail {
    pub(super) fn offset(&self) -> (f64, f64) {
        (self.spring.ox, self.spring.oy)
    }

    pub(super) fn moving(&self) -> bool {
        self.spring.ox != 0.0
            || self.spring.oy != 0.0
            || self.spring.vx != 0.0
            || self.spring.vy != 0.0
    }

    /// The panel was dragged by `delta`: the trail stays where it is on
    /// screen (at most `TRAIL_LAG_CAP` behind), and springs after it.
    pub(super) fn panel_dragged(&mut self, delta: (f64, f64)) {
        if !self.dragging {
            self.dragging = true;
            self.max_lag = 0.0;
            self.released = None;
        }
        let (x, y) = (self.spring.ox - delta.0, self.spring.oy - delta.1);
        let length = x.hypot(y);
        let scale = if length > TRAIL_LAG_CAP {
            TRAIL_LAG_CAP / length
        } else {
            1.0
        };
        (self.spring.ox, self.spring.oy) = (x * scale, y * scale);
        self.measure();
    }

    /// The panel moved without a drag (placed, resized): no trailing.
    pub(super) fn snap(&mut self) {
        self.spring = Spring::default();
    }

    /// The drag ended at `now`.
    pub(super) fn release(&mut self, now: Instant) {
        if self.dragging {
            self.dragging = false;
            self.released = Some(now);
        }
    }

    /// Advance by `dt` seconds. Whether it is still moving.
    pub(super) fn step(&mut self, dt: f64) -> bool {
        let moving = damped_step(&mut self.spring, TRAIL_OMEGA, TRAIL_ZETA, dt);
        self.measure();
        moving
    }

    fn measure(&mut self) {
        if self.dragging || self.released.is_some() {
            self.max_lag = self.max_lag.max(self.spring.ox.hypot(self.spring.oy));
        }
    }

    /// Once per drag, when the trail has come to rest after it: the largest
    /// lag it showed (pt) and how long it took to settle after release.
    pub(super) fn settled(&mut self, now: Instant) -> Option<(f64, Duration)> {
        if self.dragging || self.moving() {
            return None;
        }
        let released = self.released.take()?;
        Some((self.max_lag, now.saturating_duration_since(released)))
    }
}

/// The trail window's frame (AppKit) for a panel window at `panel` with the
/// trail `offset` from rest: the panel's frame widened left by `TRAIL_PAD`,
/// moved by the offset. Panel coordinates map to trail coordinates by
/// adding `TRAIL_PAD` to x.
pub(super) fn trail_frame(panel: Area, offset: (f64, f64)) -> Area {
    Area {
        x: panel.x - TRAIL_PAD + offset.0,
        y: panel.y + offset.1,
        w: panel.w + TRAIL_PAD,
        h: panel.h,
    }
}

// ── Resize ────────────────────────────────────────────────────────────────

/// Edges of the front card being resized, as `NSCursorFrameResizePosition`
/// bits.
pub(super) const TOP: u8 = 1 << 0;
pub(super) const LEFT: u8 = 1 << 1;
pub(super) const BOTTOM: u8 = 1 << 2;
pub(super) const RIGHT: u8 = 1 << 3;

/// Where a press on the panel landed, for logs: "margin", "back-card",
/// "corner" or "edge" (the front card's resize band), "header", "body".
pub(super) fn press_region(
    point: (f64, f64),
    depth: Option<usize>,
    edges: u8,
    front: Area,
) -> &'static str {
    match depth {
        None => "margin",
        Some(0) if edges.count_ones() == 2 => "corner",
        Some(0) if edges != 0 => "edge",
        Some(0) if point.1 >= front.y + front.h - HEADER_HEIGHT => "header",
        Some(0) => "body",
        Some(_) => "back-card",
    }
}

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
    fn acting_in_a_new_window_pushes_it_in_front_and_keeps_three_back_items() {
        let t = Instant::now();
        let mut stack = Stack::new();
        stack.act(1, t);
        assert_eq!(stack.keys(), [1]);
        stack.act(2, t);
        assert_eq!(stack.keys(), [2, 1]);
        for key in [3, 4, 5] {
            stack.act(key, t);
        }
        assert_eq!(
            stack.keys(),
            [5, 4, 3, 2],
            "the front plus three back items; the oldest drops"
        );
    }

    #[test]
    fn finished_back_windows_are_chips_and_the_rest_stay_cards() {
        use Slot::*;
        // Front finished or not, it stays the front card.
        assert_eq!(slots(&[true]), [Front]);
        // Back items: finished ones are chips bottom-up, the rest cards by depth.
        assert_eq!(
            slots(&[false, true, false, true]),
            [Front, Chip(0), Card(1), Chip(1)]
        );
        assert_eq!(
            slots(&[false, false, false, false]),
            [Front, Card(1), Card(2), Card(3)]
        );
        assert_eq!(
            slots(&[true, true, true, true]),
            [Front, Chip(0), Chip(1), Chip(2)]
        );
        // Every slot has its own view.
        let all = [Front, Card(1), Card(2), Card(3), Chip(0), Chip(1), Chip(2)];
        let mut views: Vec<usize> = all.iter().map(|slot| slot.view()).collect();
        views.sort();
        assert_eq!(views, (0..VIEWS).collect::<Vec<_>>());
    }

    #[test]
    fn chips_sit_left_of_the_cards_inside_the_trail_window() {
        let (win_w, win_h) = window_size(CARD);
        for finished in [
            [false, true, true, true],
            [false, false, true, true],
            [false, false, false, true],
            [false, true, false, false],
        ] {
            let slots = slots(&finished);
            let cards = back_cards(&slots);
            let frames: Vec<Area> = slots
                .iter()
                .map(|slot| slot_frame(CARD, *slot, cards))
                .collect();
            for (slot, frame) in slots.iter().zip(&frames) {
                // Inside the trail window (the panel widened by TRAIL_PAD).
                assert!(
                    frame.x >= -TRAIL_PAD && frame.x + frame.w <= win_w,
                    "{slot:?}"
                );
                assert!(frame.y >= 0.0 && frame.y + frame.h <= win_h, "{slot:?}");
                if let Slot::Chip(_) = slot {
                    // Clear of every card (front and back), and of each other.
                    for (other, rect) in slots.iter().zip(&frames) {
                        if other != slot {
                            let apart = frame.x + frame.w <= rect.x
                                || rect.x + rect.w <= frame.x
                                || frame.y + frame.h <= rect.y
                                || rect.y + rect.h <= frame.y;
                            assert!(apart, "{slot:?} overlaps {other:?}");
                        }
                    }
                }
            }
        }
        // Three chips fit beside the smallest front card.
        let top = slot_frame(MIN_CARD, Slot::Chip(2), 0);
        assert!(top.y + top.h <= MIN_CARD.1);
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
    fn a_click_raises_the_pressed_window_even_if_the_stack_reordered() {
        // [A, B, C], press B; an action in C reorders to [C, A, B] before
        // the button comes up: B is raised, not A (now at B's old depth).
        let (a, b, c) = (1, 2, 3);
        assert_eq!(click_target(Some(b), &[c, a, b]), Some(b));
        // B dropped meanwhile, or an action already brought it to the
        // front: the click is cancelled.
        assert_eq!(click_target(Some(b), &[c, a]), None);
        assert_eq!(click_target(Some(b), &[b, c, a]), None);
        assert_eq!(click_target::<u32>(None, &[c, a, b]), None);
    }

    #[test]
    fn presses_find_the_front_in_the_panel_and_back_items_in_the_trail() {
        let layout = slots(&[false, false, true, false]);
        let cards = back_cards(&layout);
        let frames: Vec<Area> = layout
            .iter()
            .map(|slot| slot_frame(CARD, *slot, cards))
            .collect();
        let front = frames[0];
        let inside = |area: &Area| (area.x + 4.0, area.y + area.h - 4.0);
        // In the panel only the front card is hit, even over a back card.
        assert_eq!(item_at(inside(&front), &layout, &frames, false), Some(0));
        assert_eq!(item_at(inside(&frames[1]), &layout, &frames, false), None);
        // In the trail: the strip of card depth 1, the one above it (depth
        // 2), and the chip; the front card is not the trail's.
        assert_eq!(item_at(inside(&frames[1]), &layout, &frames, true), Some(1));
        assert_eq!(item_at(inside(&frames[3]), &layout, &frames, true), Some(3));
        assert_eq!(item_at(inside(&frames[2]), &layout, &frames, true), Some(2));
        assert_eq!(
            item_at((front.x + 100.0, 50.0), &layout, &frames, true),
            Some(1),
            "under the front card lies depth 1"
        );
        assert_eq!(
            item_at((-TRAIL_PAD + 1.0, 250.0), &layout, &frames, true),
            None
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
            // card.
            assert!(card.w < above.w && card.h < above.h);
            assert!(card.x + card.w < front.x + front.w);
            assert!(card.y >= 0.0);
            // Its top-left stays inside the window.
            assert!(card.x >= 0.0);
            assert!(card.y + card.h <= win_h + 1e-9);
        }
        // Same at the minimum card size.
        let small = rest_frame(MIN_CARD, MAX_CARDS - 1);
        assert!(small.x + small.w < STACK_MARGIN + MIN_CARD.0);
    }

    /// The item a press lands on: the panel (front card) is above the
    /// trail, and a press it does not take passes to the trail.
    fn pressed(point: (f64, f64), layout: &[Slot], frames: &[Area]) -> Option<usize> {
        item_at(point, layout, frames, false).or_else(|| item_at(point, layout, frames, true))
    }

    #[test]
    fn clicks_hit_the_frontmost_card_under_the_pointer() {
        let layout = slots(&[false; MAX_CARDS]);
        let frames: Vec<Area> = (0..MAX_CARDS)
            .map(|depth| rest_frame(CARD, depth))
            .collect();
        let front = frames[0];
        // Inside the front card, even where back cards lie beneath it.
        assert_eq!(
            pressed((front.x + 20.0, front.y + front.h - 5.0), &layout, &frames),
            Some(0)
        );
        // The strip peeking above the front card belongs to depth 1, the one
        // above that to depth 2.
        assert_eq!(
            pressed((front.x + 40.0, front.y + front.h + 7.0), &layout, &frames),
            Some(1)
        );
        assert_eq!(
            pressed(
                (front.x + 40.0, front.y + front.h + CARD_STEP + 7.0),
                &layout,
                &frames
            ),
            Some(2)
        );
        // The empty corner of the margin is nobody's.
        assert_eq!(pressed((1.0, 1.0), &layout, &frames), None);
        // With one card, the peek area is empty.
        assert_eq!(
            pressed(
                (front.x + 40.0, front.y + front.h + 7.0),
                &layout[..1],
                &frames[..1]
            ),
            None
        );
    }

    /// Drag the panel along x at a speed ramping to `speed` pt/s over 100 ms,
    /// hold it for 400 ms, and stop, in 60 Hz steps (as mouse events and the
    /// ticker do). Returns the largest lag, the largest overshoot past rest
    /// after the stop, and the settle time the trail reports.
    fn drag_and_release(speed: f64) -> (f64, f64, Duration) {
        let dt = 1.0 / 60.0;
        let start = Instant::now();
        let mut trail = Trail::default();
        let mut t = 0.0;
        while t < 0.5 {
            trail.panel_dragged(((t / 0.1_f64).min(1.0) * speed * dt, 0.0));
            trail.step(dt);
            t += dt;
        }
        let released = start + Duration::from_secs_f64(t);
        trail.release(released);
        let mut overshoot = 0.0_f64;
        let mut after = 0.0;
        while trail.step(dt) {
            after += dt;
            overshoot = overshoot.max(trail.offset().0);
            assert!(trail.settled(released).is_none(), "reported while moving");
            assert!(after < 2.0, "still moving after {after:.3}s");
        }
        after += dt;
        let (max_lag, settle) = trail
            .settled(released + Duration::from_secs_f64(after))
            .unwrap();
        assert!(trail.settled(released).is_none(), "reported once per drag");
        assert_eq!(trail.offset(), (0.0, 0.0));
        (max_lag, overshoot, settle)
    }

    #[test]
    fn the_trail_hangs_70_to_90_pt_behind_a_normal_drag_and_swings_back() {
        let (max_lag, overshoot, settle) = drag_and_release(800.0);
        assert!((70.0..=90.0).contains(&max_lag), "max lag {max_lag:.1}");
        // Slightly underdamped: a small but visible swing past rest.
        assert!(overshoot >= 1.0, "overshoot {overshoot:.2}");
        assert!(overshoot <= 0.12 * max_lag, "overshoot {overshoot:.2}");
        let ms = settle.as_millis();
        assert!((500..=700).contains(&ms), "settled in {ms} ms");
    }

    #[test]
    fn the_trail_lag_scales_with_speed_but_is_capped() {
        let (slow, _, _) = drag_and_release(200.0);
        assert!(slow < 30.0, "{slow}");
        let (fling, _, _) = drag_and_release(4000.0);
        assert!((fling - TRAIL_LAG_CAP).abs() < 1e-6, "{fling}");
    }

    #[test]
    fn the_trail_window_is_the_panel_widened_for_chips_and_moved_by_the_lag() {
        let panel = Area {
            x: 1000.0,
            y: 100.0,
            w: 378.0,
            h: 300.0,
        };
        assert_eq!(
            trail_frame(panel, (0.0, 0.0)),
            Area {
                x: 1000.0 - TRAIL_PAD,
                y: 100.0,
                w: 378.0 + TRAIL_PAD,
                h: 300.0
            }
        );
        assert_eq!(
            trail_frame(panel, (-80.0, 5.0)).x,
            1000.0 - TRAIL_PAD - 80.0
        );
        // A snap (placement, resize) drops any lag without a report.
        let mut trail = Trail::default();
        trail.panel_dragged((50.0, 0.0));
        trail.snap();
        assert!(!trail.moving());
        assert!(trail.settled(Instant::now()).is_none());
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
            while spring_step(&mut spring, RESTACK_OMEGA, 1.0 / 60.0) {
                steps += 1;
                assert!(steps < 120, "no convergence from {ox}, {vx}");
            }
            assert_eq!((spring.ox, spring.vx), (0.0, 0.0));
        }
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
    fn presses_are_classified_by_region() {
        let layout = slots(&[false; MAX_CARDS]);
        let frames: Vec<Area> = (0..MAX_CARDS)
            .map(|depth| rest_frame(CARD, depth))
            .collect();
        let front = frames[0];
        let region = |point: (f64, f64)| {
            let depth = pressed(point, &layout, &frames);
            let edges = if depth == Some(0) {
                resize_edges(point, front)
            } else {
                0
            };
            press_region(point, depth, edges, front)
        };
        // 3 pt inside the window's bottom-right corner: the front card's
        // resize corner (this press used to fall through the panel).
        assert_eq!(region((front.x + front.w - 3.0, 3.0)), "corner");
        assert_eq!(region((front.x + front.w - 3.0, front.h / 2.0)), "edge");
        assert_eq!(region((front.x + front.w / 2.0, front.h - 12.0)), "header");
        assert_eq!(region((front.x + front.w / 2.0, front.h / 2.0)), "body");
        assert_eq!(region((front.x + 40.0, front.h + 7.0)), "back-card");
        assert_eq!(region((1.0, 1.0)), "margin");
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
