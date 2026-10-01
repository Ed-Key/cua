//! A panel's card stack: the window the agent is acting in is the front
//! card, and up to three windows it acted in recently are back items. A back
//! window the session is not finished with is a card fanned behind the
//! front one, each a few points up and to the left of the card in front of
//! it; a finished one collapses into a chip (app icon, green check) in a
//! column left of the cards. Back items trail the front card on a loose
//! spring when the panel is dragged. Near the top or left edge of the
//! screen the whole stack is mirrored through the front card (`Fan`):
//! below it, or right of it, so it stays on screen.
//!
//! The front card takes its window's shape (`card_shape`): the window's
//! proportions at the size the panel's size box allows, so a tall window
//! gives a tall card and a wide one a wide card, and the picture fills it.
//! The box is orientation-neutral: it sets the card's longest side and its
//! area, not a landscape frame to fit into. The panel window is sized for
//! the square that holds any such card (`hold`), and that square's
//! bottom-right corner is the card's anchor: a shape change grows or shrinks
//! it up and left (`shaped_frame`), and the back items follow its top-left.
//!
//! Everything here is pure (unit tested): the stack model, the front card's
//! shape, where each item rests, the springs that animate items back to rest (after a promotion)
//! and the trail after a drag, resize clamping, and the header layout at
//! small sizes. AppKit lives in `mod.rs`.
//!
//! ## Coordinates
//!
//! Panel coordinates have their origin at the bottom-left of the DECK: the
//! front card plus the `STACK_MARGIN` above and left of it where back cards
//! rest. The window is the deck with `LAG_ROOM` on every side (and the chip
//! column on the left), so trailing items never leave it; `to_window` maps
//! panel coordinates into it.

use std::time::{Duration, Instant};

use cursor_overlay::Spring;

use super::Area;

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
/// Scale of the card at each depth, about its top-left corner. Back cards
/// stay nearly full size so they still read as windows.
const DEPTH_SCALE: [f64; MAX_CARDS] = [1.0, 0.97, 0.94, 0.91];
/// Opacity of the card at each depth.
pub(super) const DEPTH_ALPHA: [f64; MAX_CARDS] = [1.0, 0.92, 0.8, 0.7];
/// Natural frequency of the restack spring (an item moving to a new place):
/// settles in ~250 ms.
pub(super) const RESTACK_OMEGA: f64 = 30.0;
/// Chip: a glass circle holding the app icon, a check badge on its lower
/// right (the window title is its tooltip).
pub(super) const CHIP: f64 = 44.0;
pub(super) const CHIP_ICON: f64 = 32.0;
/// The check badge on a chip, and its white ring.
pub(super) const CHIP_BADGE: f64 = 16.0;
pub(super) const CHIP_BADGE_RING: f64 = 1.5;
/// A chip's whole frame: the circle plus the badge's overhang.
pub(super) const CHIP_W: f64 = CHIP + 4.0;
pub(super) const CHIP_H: f64 = CHIP + 4.0;
/// Between chips in the column (just past the glass merge distance, so
/// resting chips stay round), and between the column and the cards (a
/// resting chip's circle sits inside the merge distance, so it fuses with
/// the front card like liquid and pulls free when it lags).
const CHIP_ROW_GAP: f64 = 12.0;
const CHIP_GAP: f64 = 2.0;
/// Glass views closer than this merge into one shape.
pub(super) const GLASS_SPACING: f64 = 11.0;
/// The chip column's width, left of the deck.
pub(super) const TRAIL_PAD: f64 = CHIP_GAP + CHIP_W;
/// Room on every side of the deck for trailing items, inside the window.
pub(super) const LAG_ROOM: f64 = TRAIL_LAG_CAP;
/// The window's inset left of the deck: the chip column plus lag room.
pub(super) const INSET_LEFT: f64 = TRAIL_PAD + LAG_ROOM;
/// The farthest a chip reaches left of the panel window: with the most back
/// cards that still leave room for a chip (placement reserves this).
pub(super) const CHIP_REACH: f64 =
    CHIP_GAP + CHIP_W - STACK_MARGIN + (MAX_CARDS - 2) as f64 * CARD_STEP;
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
    /// card and the others keep their order behind it. Nothing drops here:
    /// `prune` decides who leaves.
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
        }
        self.cards[0].acted = now;
    }

    /// The session acted in `key` at `now` while the user's pick is the
    /// front card: the window joins, or refreshes, as the first back card
    /// and the front stays. Acting in the front card itself only refreshes
    /// its time.
    pub(super) fn act_behind(&mut self, key: K, now: Instant) {
        let mut card = match self.cards.iter().position(|card| card.key == key) {
            Some(0) => return self.cards[0].acted = now,
            Some(index) => self.cards.remove(index),
            None => Card {
                key,
                acted: now,
                data: V::default(),
            },
        };
        card.acted = now;
        self.cards.insert(1.min(self.cards.len()), card);
    }

    pub(super) fn card_mut(&mut self, key: K) -> Option<&mut Card<K, V>> {
        self.cards.iter_mut().find(|card| card.key == key)
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

    /// Who leaves the stack at `now`. A back card whose window is `gone`
    /// always does. Unless `keep` holds the stack (the user's hands are on
    /// the panel), so does a back card the session has not acted in for
    /// `BACK_CARD_TTL`, and the deepest back cards beyond `MAX_CARDS`; a
    /// window `keep` protects (the user's pick) stays whatever its age or
    /// depth, and the next deepest goes instead. The front card always
    /// stays. Whether anything dropped.
    pub(super) fn prune(&mut self, now: Instant, keep: &Keep<K>, gone: impl Fn(&K) -> bool) -> bool {
        let before = self.cards.len();
        let mut depth = 0;
        self.cards.retain(|card| {
            depth += 1;
            depth == 1
                || (!gone(&card.key)
                    && (keep.held
                        || keep.protects(&card.key)
                        || now.saturating_duration_since(card.acted) < BACK_CARD_TTL))
        });
        while !keep.held && self.cards.len() > MAX_CARDS {
            let deepest = self.cards.iter().rposition(|card| !keep.protects(&card.key));
            match deepest {
                Some(index) if index > 0 => self.cards.remove(index),
                _ => break,
            };
        }
        self.cards.len() != before
    }
}

/// What `CardStack::prune` may not drop now (see `hands`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Keep<K> {
    /// The user holds the panel (the pointer is on it, a press, or an
    /// interaction less than the idle period old): no back card expires and
    /// none is evicted by capacity.
    pub(super) held: bool,
    /// Windows that stay whatever their age or depth: the user's pick, and
    /// behind it the window the agent last acted in (what the panel follows
    /// again when the pick is over).
    pub(super) windows: [Option<K>; 2],
}

impl<K: PartialEq> Keep<K> {
    fn protects(&self, key: &K) -> bool {
        self.windows.iter().flatten().any(|window| window == key)
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

// ── Card shape ────────────────────────────────────────────────────────────

/// The front card is never wider than this times its height, nor taller
/// than this times its width. A window past that keeps its own proportions
/// inside the clamped card.
pub(super) const MAX_ASPECT: f64 = 2.2;
/// A window smaller than the size box is mirrored at its own size, but
/// never below this fraction of the size that fills the box (a tiny window
/// still gives a usable card).
pub(super) const SMALL_FLOOR: f64 = 0.75;

/// The largest size with `shape`'s proportions inside `bounds` (`bounds`
/// itself for a degenerate shape).
pub(super) fn fit(bounds: (f64, f64), shape: (f64, f64)) -> (f64, f64) {
    if shape.0 <= 0.0 || shape.1 <= 0.0 {
        return bounds;
    }
    let scale = (bounds.0 / shape.0).min(bounds.1 / shape.1);
    (shape.0 * scale, shape.1 * scale)
}

/// The square that holds every card of the size box `bounds`, whatever its
/// shape: the box's long side each way. The panel window is sized for it,
/// so the window never changes with the target's shape.
pub(super) fn hold(bounds: (f64, f64)) -> (f64, f64) {
    let side = bounds.0.max(bounds.1);
    (side, side)
}

/// Front card size (whole points) for a target window of `window` size in
/// a panel whose size box is `bounds` (the default box, or the one the user
/// set by resizing). The box is orientation-neutral: it gives the card's
/// longest side (the box's) and its area (the box's), so a tall window gets
/// as much room as a wide one. The card is the window's proportions,
/// clamped to `MAX_ASPECT`, at the largest size within both. A window
/// smaller than that keeps its own size, raised to the `SMALL_FLOOR` and
/// never past the limits. The box itself while the window's size is
/// unknown.
pub(super) fn card_shape(bounds: (f64, f64), window: Option<(f64, f64)>) -> (f64, f64) {
    let Some((w, h)) = window.filter(|(w, h)| *w > 0.0 && *h > 0.0) else {
        return bounds;
    };
    // The card that holds the window at its own size.
    let own = if w > h * MAX_ASPECT {
        (w, w / MAX_ASPECT)
    } else if h > w * MAX_ASPECT {
        (h / MAX_ASPECT, h)
    } else {
        (w, h)
    };
    let (long, area) = (bounds.0.max(bounds.1), bounds.0 * bounds.1);
    let fill = (long / own.0.max(own.1)).min((area / (own.0 * own.1)).sqrt());
    let scale = fill.min(1f64.max(SMALL_FLOOR * fill));
    ((own.0 * scale).round().max(1.0), (own.1 * scale).round().max(1.0))
}

/// Resting frame of `slot` for a front card of `card` size in a panel whose
/// window holds cards up to `bounds` (see `hold`): `slot_frame`, with the
/// front card's bottom-right corner on that square's. That corner is the
/// panel's anchor (its cascade slot,
/// or where the user dragged it), so a card that changes shape grows or
/// shrinks up and left, and the back items keep their places against its
/// top and left edges.
pub(super) fn shaped_frame(
    bounds: (f64, f64),
    card: (f64, f64),
    slot: Slot,
    back_cards: usize,
) -> Area {
    let frame = slot_frame(card, slot, back_cards);
    Area {
        x: frame.x + bounds.0 - card.0,
        ..frame
    }
}

/// Which way the back items fan out from the front card. By default up and
/// left (title strips peek above, chips sit left); near the top of the
/// screen's visible frame they hang below instead, near its left edge they
/// go right, so they stay on screen where a click reaches them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Fan {
    pub(super) below: bool,
    pub(super) right: bool,
}

/// How far the back items reach left of the front card at most: the
/// deepest card that still leaves room for a chip, and that chip.
pub(super) const LEFT_REACH: f64 = (MAX_CARDS - 2) as f64 * CARD_STEP + CHIP_GAP + CHIP_W;

/// The fan for a front card drawn at `card` (screen rect, AppKit
/// bottom-left origin) on a screen whose visible frame is `visible`: below
/// when the strips above would leave the frame's top, right when the cards
/// and chips on the left would leave its left edge.
pub(super) fn fan_for(card: Area, visible: Area) -> Fan {
    Fan {
        below: visible.y + visible.h - (card.y + card.h) < STACK_MARGIN,
        right: card.x - visible.x < LEFT_REACH,
    }
}

/// `frame` (a resting frame, panel coordinates) mirrored through the
/// front card `front` along the axes `fan` flips, so a back card peeks
/// below or right of it and a chip column sits on its right.
pub(super) fn fanned(frame: Area, front: Area, fan: Fan) -> Area {
    Area {
        x: if fan.right {
            2.0 * front.x + front.w - frame.x - frame.w
        } else {
            frame.x
        },
        y: if fan.below {
            2.0 * front.y + front.h - frame.y - frame.h
        } else {
            frame.y
        },
        ..frame
    }
}

/// The stack item under `point` (panel coordinates): the front card first,
/// then chips (they never overlap cards), then cards front to back. `frames`
/// are where each item of `layout` is drawn.
pub(super) fn item_at(point: (f64, f64), layout: &[Slot], frames: &[Area]) -> Option<usize> {
    let mut order: Vec<usize> = (0..layout.len().min(frames.len())).collect();
    order.sort_by_key(|&index| match layout[index] {
        Slot::Front => 0,
        Slot::Chip(_) => 1,
        Slot::Card(depth) => 1 + depth,
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

/// Deck size for a front card of `card` size: the card plus the margin
/// above and left of it where back cards rest. What placement works on.
pub(super) fn deck_size(card: (f64, f64)) -> (f64, f64) {
    (card.0 + STACK_MARGIN, card.1 + STACK_MARGIN)
}

/// Panel window size for a front card of `card` size: the deck with the
/// chip column and lag room around it.
pub(super) fn window_size(card: (f64, f64)) -> (f64, f64) {
    let (w, h) = deck_size(card);
    (w + INSET_LEFT + LAG_ROOM, h + 2.0 * LAG_ROOM)
}

/// Front card size for a panel window of `window` size.
pub(super) fn card_size(window: (f64, f64)) -> (f64, f64) {
    (
        window.0 - INSET_LEFT - LAG_ROOM - STACK_MARGIN,
        window.1 - 2.0 * LAG_ROOM - STACK_MARGIN,
    )
}

/// A panel-coordinate frame in the window's coordinates.
pub(super) fn to_window(area: Area) -> Area {
    Area {
        x: area.x + INSET_LEFT,
        y: area.y + LAG_ROOM,
        ..area
    }
}

/// A window-coordinate point in panel coordinates.
pub(super) fn panel_point((x, y): (f64, f64)) -> (f64, f64) {
    (x - INSET_LEFT, y - LAG_ROOM)
}

/// Window origin (AppKit) for a deck placed at `deck` (AppKit origin).
pub(super) fn window_origin(deck: (f64, f64)) -> (f64, f64) {
    (deck.0 - INSET_LEFT, deck.1 - LAG_ROOM)
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

/// The feel of the chips following a dragged panel, tuned to Codex Computer
/// Use's PiP: loose and playful. At a normal drag speed (~800 pt/s) a chip
/// hangs ~90 pt behind (measured right after each drag event, when it has
/// not moved yet); when the drag stops it swings ~15 pt past its resting
/// place (one visible overshoot) and settles in ~0.7 s. Between events the
/// lag is `2·ζ·v/ω`: lower `TRAIL_OMEGA` is looser (more lag, slower
/// settle), lower `TRAIL_ZETA` is bouncier (0.55 gives one visible
/// overshoot; 1.0 would give none).
pub(super) const TRAIL_OMEGA: f64 = 12.5;
pub(super) const TRAIL_ZETA: f64 = 0.55;
/// Back cards follow more firmly than chips: less lag, no visible swing.
pub(super) const CARD_TRAIL_OMEGA: f64 = 14.5;
pub(super) const CARD_TRAIL_ZETA: f64 = 0.7;
/// Lag never exceeds this, so a violent fling cannot throw the trail far
/// off its panel (and never out of the window's `LAG_ROOM`).
pub(super) const TRAIL_LAG_CAP: f64 = 140.0;

/// The back items' offsets from their resting places (chips on one spring,
/// back cards on a firmer one), and the measurement of the last drag's chip
/// lag (for logs).
#[derive(Default)]
pub(super) struct Trail {
    chips: Spring,
    cards: Spring,
    /// A drag is moving the panel.
    dragging: bool,
    /// Largest chip lag since the current drag started.
    max_lag: f64,
    /// When the last drag ended, until its settle is reported.
    released: Option<Instant>,
}

fn spring_moving(spring: &Spring) -> bool {
    spring.ox != 0.0 || spring.oy != 0.0 || spring.vx != 0.0 || spring.vy != 0.0
}

impl Trail {
    /// The offset of the item in `slot` from its resting place.
    pub(super) fn offset(&self, slot: Slot) -> (f64, f64) {
        match slot {
            Slot::Front => (0.0, 0.0),
            Slot::Chip(_) => (self.chips.ox, self.chips.oy),
            Slot::Card(_) => (self.cards.ox, self.cards.oy),
        }
    }

    pub(super) fn moving(&self) -> bool {
        spring_moving(&self.chips) || spring_moving(&self.cards)
    }

    /// The panel was dragged by `delta`: the back items stay where they
    /// are on screen (at most `TRAIL_LAG_CAP` behind), and spring after it.
    pub(super) fn panel_dragged(&mut self, delta: (f64, f64)) {
        if !self.dragging {
            self.dragging = true;
            self.max_lag = 0.0;
            self.released = None;
        }
        for spring in [&mut self.chips, &mut self.cards] {
            let (x, y) = (spring.ox - delta.0, spring.oy - delta.1);
            let length = x.hypot(y);
            let scale = if length > TRAIL_LAG_CAP {
                TRAIL_LAG_CAP / length
            } else {
                1.0
            };
            (spring.ox, spring.oy) = (x * scale, y * scale);
        }
        self.measure();
    }

    /// The panel moved without a drag (placed, resized): no trailing.
    pub(super) fn snap(&mut self) {
        self.chips = Spring::default();
        self.cards = Spring::default();
    }

    /// The drag ended at `now`.
    pub(super) fn release(&mut self, now: Instant) {
        if self.dragging {
            self.dragging = false;
            self.released = Some(now);
        }
    }

    /// Advance by `dt` seconds. Whether anything is still moving.
    pub(super) fn step(&mut self, dt: f64) -> bool {
        let chips = damped_step(&mut self.chips, TRAIL_OMEGA, TRAIL_ZETA, dt);
        let cards = damped_step(&mut self.cards, CARD_TRAIL_OMEGA, CARD_TRAIL_ZETA, dt);
        self.measure();
        chips || cards
    }

    fn measure(&mut self) {
        if self.dragging || self.released.is_some() {
            self.max_lag = self.max_lag.max(self.chips.ox.hypot(self.chips.oy));
        }
    }

    /// Once per drag, when the trail has come to rest after it: the largest
    /// chip lag it showed (pt) and how long it took to settle after release.
    pub(super) fn settled(&mut self, now: Instant) -> Option<(f64, Duration)> {
        if self.dragging || self.moving() {
            return None;
        }
        let released = self.released.take()?;
        Some((self.max_lag, now.saturating_duration_since(released)))
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
/// "corner" or "edge" (the front card's resize band), "bar" (the hover bar
/// above the front card, when it is up), "body".
pub(super) fn press_region(
    point: (f64, f64),
    depth: Option<usize>,
    edges: u8,
    front: Area,
    bar: Option<Area>,
) -> &'static str {
    let _ = front;
    match depth {
        None => "margin",
        Some(0) if edges.count_ones() == 2 => "corner",
        Some(0) if edges != 0 => "edge",
        Some(0) if bar.is_some_and(|bar| contains(&bar, point)) => "bar",
        Some(0) => "body",
        Some(_) => "back-card",
    }
}

/// The item a press on `point` lands on when the hover bar is `bar`: the
/// bar (when shown) counts as the front card and wins over anything under
/// it, such as a back card's strip it overlaps (a drag, never a raise);
/// else `item`.
pub(super) fn pressed_item(point: (f64, f64), item: Option<usize>, bar: Option<Area>) -> Option<usize> {
    if bar.is_some_and(|bar| contains(&bar, point)) {
        return Some(0);
    }
    item
}

/// The front card's edges a press on `point` resizes (0 = none): only a
/// press on the front card (`item` 0) in its resize band, also where the
/// hover bar `bar` (when shown) overlaps the band (its bottom over the
/// card's top edge, or its top when it drops inside the card near the
/// screen's top), but never on the bar's buttons. The rest of the bar is a
/// drag handle.
pub(super) fn press_edges(point: (f64, f64), item: Option<usize>, front: Area, bar: Option<Area>) -> u8 {
    if item != Some(0) {
        return 0;
    }
    if let Some(bar) = bar {
        let layout = bar_layout(bar.w);
        let local = (point.0 - bar.x, point.1 - bar.y);
        if [layout.focus, layout.close].iter().any(|button| contains(button, local)) {
            return 0;
        }
    }
    resize_edges(point, front)
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

/// How far to move a card drawn at `card` (screen rect, AppKit bottom-left
/// origin) so it lies inside `visible` (the screen's visible frame: below
/// the menu bar, above the Dock), by the least distance: (dx, dy). A card
/// wider or taller than the frame keeps its left edge, or its top, inside.
pub(super) fn keep_inside(card: Area, visible: Area) -> (f64, f64) {
    let axis = |at: f64, size: f64, lo: f64, extent: f64, keep_high: bool| {
        let hi = lo + extent - size;
        let to = if hi < lo {
            if keep_high { hi } else { lo }
        } else {
            at.clamp(lo, hi)
        };
        to - at
    };
    (
        axis(card.x, card.w, visible.x, visible.w, false),
        axis(card.y, card.h, visible.y, visible.h, true),
    )
}

/// The screen (index into `screens`, AppKit frames) a card drawn at `card`
/// (screen rect) is on: the one holding its center, else the one it
/// overlaps most; `None` when it overlaps none. Never the window's screen
/// (the window reaches far past its card, so most of it can be on another
/// display) nor the pointer's (a missed release is handled a second later,
/// wherever the pointer has gone).
pub(super) fn card_screen(card: Area, screens: &[Area]) -> Option<usize> {
    let center = (card.x + card.w / 2.0, card.y + card.h / 2.0);
    let overlap = |s: &Area| {
        let w = (card.x + card.w).min(s.x + s.w) - card.x.max(s.x);
        let h = (card.y + card.h).min(s.y + s.h) - card.y.max(s.y);
        w.max(0.0) * h.max(0.0)
    };
    screens.iter().position(|s| contains(s, center)).or_else(|| {
        screens
            .iter()
            .enumerate()
            .filter(|(_, s)| overlap(s) > 0.0)
            .max_by(|a, b| overlap(a.1).total_cmp(&overlap(b.1)))
            .map(|(index, _)| index)
    })
}

/// Largest size box on a screen whose visible frame is `visible` (w, h):
/// `MAX_SCREEN_FRACTION` of the screen's shorter side each way, so the
/// square that holds its cards (`hold`) fits the screen.
pub(super) fn max_card(visible: (f64, f64)) -> (f64, f64) {
    let side = visible.0.min(visible.1) * MAX_SCREEN_FRACTION;
    (side.max(MIN_CARD.0), side.max(MIN_CARD.1))
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

/// The size box and the panel window frame (AppKit) after dragging `edges`
/// of the front card by `delta`, from a window frame of `start` and a size
/// box of `card`. The drag resizes the box as `resize_window` would a window
/// sized for it; the window is the square that holds the new box's cards,
/// its bottom-right corner (the card's anchor) where that window's would be.
pub(super) fn resize_panel(
    start: Area,
    card: (f64, f64),
    edges: u8,
    delta: (f64, f64),
    max: (f64, f64),
) -> ((f64, f64), Area) {
    let anchored = |corner: Area, (w, h): (f64, f64)| Area {
        x: corner.x + corner.w - w,
        y: corner.y,
        w,
        h,
    };
    let boxed = resize_window(anchored(start, window_size(card)), edges, delta, max);
    let card = card_size((boxed.w, boxed.h));
    (card, anchored(boxed, window_size(hold(card))))
}

/// Whether a well that last changed size at `changed` has been still long
/// enough to resize the live stream.
pub(super) fn resize_settled(changed: Instant, now: Instant) -> bool {
    now.saturating_duration_since(changed) >= RESIZE_DEBOUNCE
}

// ── Hover bar ─────────────────────────────────────────────────────────────

/// The bar that appears above the front card on hover: its height, how far
/// it overlaps the card's top edge (the rest protrudes above), and how much
/// narrower than the card it is on each side.
pub(super) const BAR_HEIGHT: f64 = 30.0;
pub(super) const BAR_OVERLAP: f64 = 6.0;
pub(super) const BAR_INSET: f64 = 12.0;
/// The narrowest bar: the client icon, the dot, a few letters of title and
/// the two buttons.
pub(super) const BAR_MIN_WIDTH: f64 = 136.0;
/// The bar fades in this fast on hover and out this long after the pointer
/// leaves the card and the bar.
pub(super) const BAR_FADE_IN: Duration = Duration::from_millis(150);
pub(super) const BAR_FADE_OUT: Duration = Duration::from_millis(250);
/// The bar's buttons: solid circles this big.
pub(super) const BAR_BUTTON: f64 = 22.0;
pub(super) const BAR_ICON: f64 = 18.0;
pub(super) const BAR_DOT: f64 = 6.0;

/// Where the bar sits for a front card at `card` (panel coordinates) with
/// `room_above` points of screen above the card's top: attached to the top
/// edge, protruding above it, unless there is no room, when it sits just
/// inside the card's top instead. It spans the card's width less the inset,
/// but a narrow card (a tall window's) gets a bar of `BAR_MIN_WIDTH` that
/// reaches past its left edge.
pub(super) fn bar_frame(card: Area, room_above: f64) -> Area {
    let w = (card.w - 2.0 * BAR_INSET).max(BAR_MIN_WIDTH);
    let top = card.y + card.h;
    let protrude = BAR_HEIGHT - BAR_OVERLAP;
    let y = if room_above >= protrude {
        top - BAR_OVERLAP
    } else {
        top - BAR_HEIGHT
    };
    Area {
        x: card.x + card.w - BAR_INSET - w,
        y,
        w,
        h: BAR_HEIGHT,
    }
}

/// Frames (in the bar's coordinates) of the bar's views: client icon, the
/// session-color dot, the (truncating) window title, and the two buttons
/// pinned right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct BarLayout {
    pub(super) client_icon: Area,
    pub(super) dot: Area,
    pub(super) title: Area,
    pub(super) focus: Area,
    pub(super) close: Area,
}

pub(super) fn bar_layout(width: f64) -> BarLayout {
    let button_y = (BAR_HEIGHT - BAR_BUTTON) / 2.0;
    let close_x = width - 6.0 - BAR_BUTTON;
    let focus_x = close_x - 4.0 - BAR_BUTTON;
    let icon_x = 8.0;
    let dot_x = icon_x + BAR_ICON + 5.0;
    let title_x = dot_x + BAR_DOT + 6.0;
    BarLayout {
        client_icon: Area {
            x: icon_x,
            y: (BAR_HEIGHT - BAR_ICON) / 2.0,
            w: BAR_ICON,
            h: BAR_ICON,
        },
        dot: Area {
            x: dot_x,
            y: (BAR_HEIGHT - BAR_DOT) / 2.0,
            w: BAR_DOT,
            h: BAR_DOT,
        },
        title: Area {
            x: title_x,
            y: (BAR_HEIGHT - 16.0) / 2.0,
            w: (focus_x - 6.0 - title_x).max(0.0),
            h: 16.0,
        },
        focus: Area {
            x: focus_x,
            y: button_y,
            w: BAR_BUTTON,
            h: BAR_BUTTON,
        },
        close: Area {
            x: close_x,
            y: button_y,
            w: BAR_BUTTON,
            h: BAR_BUTTON,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default size box.
    const BOX: (f64, f64) = (320.0, 200.0);

    #[test]
    fn the_front_card_takes_its_windows_shape_inside_the_size_box() {
        // The box gives the long side (320) and the area (64,000), whichever
        // way the window is turned.
        let (long, area) = (320.0, 64_000.0);
        for (name, window, card) in [
            // Bigger than the box allows: scaled down to its long side or
            // its area, whichever comes first.
            ("wide, 800x420", (800.0, 420.0), (320.0, 168.0)),
            ("landscape 3:2", (900.0, 600.0), (310.0, 207.0)),
            ("a TextEdit document", (673.0, 439.0), (313.0, 204.0)),
            ("Calculator", (230.0, 408.0), (180.0, 320.0)),
            ("square", (400.0, 400.0), (253.0, 253.0)),
            ("the box's own shape", (1280.0, 800.0), (320.0, 200.0)),
            ("the box's shape, turned", (800.0, 1280.0), (200.0, 320.0)),
            // Smaller than that: its own size, raised to 75% of the fill.
            ("tiny", (200.0, 120.0), (240.0, 144.0)),
            ("small, above the floor", (300.0, 190.0), (300.0, 190.0)),
            // Past 2.2:1 either way: the clamped card (the picture keeps its
            // own shape inside it).
            ("very wide", (1000.0, 300.0), (320.0, 145.0)),
            ("very tall", (300.0, 1400.0), (145.0, 320.0)),
        ] {
            let got = card_shape(BOX, Some(window));
            assert_eq!(got, card, "{name}");
            // Rounded to whole points, so half a point of slack each way.
            assert!(got.0.max(got.1) <= long, "{name}: the long side");
            assert!((got.0 - 0.5) * (got.1 - 0.5) <= area, "{name}: the area");
            let held = hold(BOX);
            assert!(got.0 <= held.0 && got.1 <= held.1, "{name}: the window holds it");
            let aspect = got.0 / got.1;
            assert!(aspect < MAX_ASPECT + 0.02 && aspect > 1.0 / MAX_ASPECT - 0.02, "{name}");
        }
        // A window and its quarter turn get the same card, turned.
        let (w, h) = card_shape(BOX, Some((673.0, 439.0)));
        assert_eq!(card_shape(BOX, Some((439.0, 673.0))), (h, w));
        assert_eq!(card_shape((200.0, 320.0), Some((673.0, 439.0))), (w, h), "a turned box");
        // Unknown or degenerate: the box itself.
        assert_eq!(card_shape(BOX, None), BOX);
        assert_eq!(card_shape(BOX, Some((0.0, 300.0))), BOX);
        // The user's resize sets the box (its long side and area together);
        // the shape still follows the window.
        assert_eq!(card_shape((410.0, 260.0), Some((673.0, 439.0))), (404.0, 264.0));
        assert_eq!(card_shape((410.0, 260.0), Some((230.0, 408.0))), (230.0, 408.0));
        // A box bigger than the window: the window's own size, not blown up.
        assert_eq!(card_shape((800.0, 600.0), Some((673.0, 439.0))), (673.0, 439.0));
        // The picture inside a clamped card keeps the window's proportions.
        let (w, h) = fit((320.0, 145.0), (1400.0, 300.0));
        assert_eq!((w, h.round()), (320.0, 69.0));
        assert_eq!(fit(BOX, (0.0, 0.0)), BOX);
    }

    #[test]
    fn a_shape_change_keeps_the_front_cards_bottom_right_corner() {
        let corner = |frame: Area| (frame.x + frame.w, frame.y);
        // The window holds the square of the box's long side.
        let held = hold(BOX);
        assert_eq!(held, (320.0, 320.0));
        assert_eq!(hold((200.0, 320.0)), held);
        let anchor = corner(shaped_frame(held, BOX, Slot::Front, 0));
        assert_eq!(anchor, (STACK_MARGIN + held.0, 0.0), "the square's corner");
        let deck = deck_size(held);
        for window in [(900.0, 600.0), (230.0, 408.0), (300.0, 1400.0), (1400.0, 300.0), (200.0, 120.0)] {
            let card = card_shape(BOX, Some(window));
            let front = shaped_frame(held, card, Slot::Front, 2);
            // The tallest card and its back cards stay inside the deck the
            // window is sized for.
            let deepest = shaped_frame(held, card, Slot::Card(MAX_CARDS - 1), 3);
            assert!(deepest.x >= 0.0 && deepest.y + deepest.h <= deck.1, "{window:?}");
            assert_eq!(corner(front), anchor, "{window:?}");
            assert_eq!((front.w, front.h), card);
            // The back items keep their places against its top-left: each
            // card a step up and left, the chips left of the last card.
            let back = shaped_frame(held, card, Slot::Card(1), 2);
            assert_eq!(back.x, front.x - CARD_STEP);
            assert_eq!(back.y + back.h, front.y + front.h + CARD_STEP);
            let chip = shaped_frame(held, card, Slot::Chip(0), 2);
            assert_eq!(chip.x + chip.w, front.x - 2.0 * CARD_STEP - CHIP_GAP);
            assert_eq!(chip.y, 0.0);
        }
    }

    #[test]
    fn a_narrow_cards_bar_keeps_its_buttons_apart() {
        // A tall window's card is narrower than the bar's content: the bar
        // keeps its least width and its right end, reaching past the card's
        // left edge.
        let narrow = shaped_frame(hold(BOX), (112.0, 200.0), Slot::Front, 0);
        let bar = bar_frame(narrow, 100.0);
        assert_eq!(bar.w, BAR_MIN_WIDTH);
        assert_eq!(bar.x + bar.w, narrow.x + narrow.w - BAR_INSET);
        let layout = bar_layout(bar.w);
        assert!(layout.title.w >= 20.0, "{layout:?}");
        assert!(layout.title.x + layout.title.w < layout.focus.x);
        // A card wide enough: inset on both sides, as before.
        let wide = shaped_frame(hold(BOX), BOX, Slot::Front, 0);
        let bar = bar_frame(wide, 100.0);
        assert_eq!((bar.x, bar.w), (wide.x + BAR_INSET, wide.w - 2.0 * BAR_INSET));
    }

    type Stack = CardStack<u32, ()>;

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    /// Nobody's hands on the panel and no pick.
    const NOBODY: Keep<u32> = Keep { held: false, windows: [None, None] };

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
        assert!(stack.prune(t, &NOBODY, |_| false));
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
        let (win_w, win_h) = deck_size(CARD);
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
                // Inside the deck widened left by the chip column.
                assert!(
                    frame.x >= -TRAIL_PAD && frame.x + frame.w <= win_w,
                    "{slot:?}"
                );
                assert!(frame.y >= 0.0 && frame.y + frame.h <= win_h, "{slot:?}");
                // And inside the window even at the largest lag.
                let drawn = to_window(*frame);
                assert!(drawn.x - TRAIL_LAG_CAP >= 0.0, "{slot:?}");
                assert!(drawn.y - TRAIL_LAG_CAP >= 0.0, "{slot:?}");
                let (window_w, window_h) = window_size(CARD);
                assert!(drawn.x + drawn.w + TRAIL_LAG_CAP <= window_w, "{slot:?}");
                assert!(drawn.y + drawn.h + TRAIL_LAG_CAP <= window_h, "{slot:?}");
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
        // CHIP_REACH is exactly the farthest any chip reaches left.
        let reach = (0..MAX_CARDS - 1)
            .map(|cards| -slot_frame(CARD, Slot::Chip(0), cards).x)
            .fold(f64::MIN, f64::max);
        assert_eq!(reach, CHIP_REACH);
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
        assert!(!stack.prune(at(t, 29), &NOBODY, |_| false));
        assert!(stack.prune(at(t, 30), &NOBODY, |_| false));
        assert_eq!(stack.keys(), [3, 2]);
        assert!(stack.prune(at(t, 100), &NOBODY, |_| false));
        assert_eq!(stack.keys(), [3], "the front card stays however old");
    }

    #[test]
    fn a_back_card_drops_when_its_window_is_gone() {
        let t = Instant::now();
        let mut stack = Stack::new();
        for key in [1, 2, 3] {
            stack.act(key, t);
        }
        assert!(stack.prune(t, &NOBODY, |key| *key == 2));
        assert_eq!(stack.keys(), [3, 1]);
        // The front card's window is the panel's business, not the stack's.
        assert!(!stack.prune(t, &NOBODY, |key| *key == 3));
        assert_eq!(stack.keys(), [3, 1]);
    }

    /// The back cards column of the hands table (see `hands`): nothing
    /// expires and nothing is evicted while the user holds the panel, and
    /// their action times are left alone, so they go once the hold is over.
    #[test]
    fn a_held_panel_drops_no_back_card_by_age_or_capacity() {
        let t = Instant::now();
        let held = Keep { held: true, windows: [None, None] };
        let mut stack = Stack::new();
        for key in [1, 2, 3, 4] {
            stack.act(key, t);
        }
        assert!(!stack.prune(at(t, 100), &held, |_| false), "no expiry under the user's hands");
        // A fifth window the agent acts in joins without evicting anyone.
        stack.act(5, at(t, 100));
        assert!(!stack.prune(at(t, 100), &held, |_| false));
        assert_eq!(stack.keys(), [5, 4, 3, 2, 1]);
        assert_eq!(stack.cards()[4].acted, t, "the hold rewrites no action time");
        // A closed window still goes.
        assert!(stack.prune(at(t, 100), &held, |key| *key == 3));
        assert_eq!(stack.keys(), [5, 4, 2, 1]);
        // The hold over, the old back cards expire as ever.
        assert!(stack.prune(at(t, 101), &NOBODY, |_| false));
        assert_eq!(stack.keys(), [5]);
    }

    /// The pick rows: the agent acting elsewhere joins behind the picked
    /// front card, and the pick (with the agent's latest window behind it)
    /// keeps its place in the stack whatever its age or depth.
    #[test]
    fn a_pick_stays_in_front_and_keeps_its_membership() {
        let t = Instant::now();
        let mut stack = Stack::new();
        stack.act(1, t);
        stack.act(2, at(t, 1));
        assert!(stack.raise(1), "the user picks window 1");
        // H6: the agent acts in another window, new or known.
        stack.act_behind(3, at(t, 2));
        assert_eq!(stack.keys(), [1, 3, 2]);
        stack.act_behind(2, at(t, 3));
        assert_eq!(stack.keys(), [1, 2, 3], "a known window refreshes as the first back card");
        assert_eq!(stack.cards()[1].acted, at(t, 3));
        // H7: the agent acts in the picked window.
        stack.act_behind(1, at(t, 4));
        assert_eq!(stack.keys(), [1, 2, 3]);
        assert_eq!(stack.cards()[0].acted, at(t, 4));
        // Capacity evicts the deepest item that is not protected.
        let keep = Keep { held: false, windows: [Some(1), Some(3)] };
        stack.act_behind(4, at(t, 5));
        stack.act_behind(5, at(t, 5));
        assert_eq!(stack.keys(), [1, 5, 4, 2, 3]);
        assert!(stack.prune(at(t, 5), &keep, |_| false));
        assert_eq!(stack.keys(), [1, 5, 4, 3], "window 2 goes, not the protected window 3 behind it");
        // Age does not expire a protected back card; the rest expire.
        assert!(stack.prune(at(t, 60), &keep, |_| false));
        assert_eq!(stack.keys(), [1, 3]);
        // A pick that went behind (the agent's card was clicked later)
        // would be protected the same way.
        let picked_behind = Keep { held: false, windows: [Some(3), None] };
        assert!(!stack.prune(at(t, 600), &picked_behind, |_| false));
        // A closed window goes even when protected.
        assert!(stack.prune(at(t, 600), &picked_behind, |key| *key == 3));
        assert_eq!(stack.keys(), [1]);
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
    fn presses_find_the_front_card_first_then_chips_then_cards_by_depth() {
        let layout = slots(&[false, false, true, false]);
        let cards = back_cards(&layout);
        let frames: Vec<Area> = layout
            .iter()
            .map(|slot| slot_frame(CARD, *slot, cards))
            .collect();
        let front = frames[0];
        let inside = |area: &Area| (area.x + 4.0, area.y + area.h - 4.0);
        // The front card wins wherever it lies over a back card.
        assert_eq!(item_at(inside(&front), &layout, &frames), Some(0));
        assert_eq!(
            item_at((front.x + 100.0, 50.0), &layout, &frames),
            Some(0),
            "over the front card, not the depth-1 card under it"
        );
        // The strip of card depth 1, the one above it (depth 2), the chip.
        assert_eq!(item_at(inside(&frames[1]), &layout, &frames), Some(1));
        assert_eq!(item_at(inside(&frames[3]), &layout, &frames), Some(3));
        assert_eq!(item_at(inside(&frames[2]), &layout, &frames), Some(2));
        assert_eq!(item_at((-TRAIL_PAD + 1.0, 250.0), &layout, &frames), None);
        // Window coordinates round-trip through the insets.
        let (x, y) = panel_point((to_window(front).x, to_window(front).y));
        assert_eq!((x, y), (front.x, front.y));
        assert_eq!(window_origin((100.0, 50.0)), (100.0 - INSET_LEFT, 50.0 - LAG_ROOM));
        assert_eq!(card_size(window_size(CARD)), CARD);
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
        let (win_w, win_h) = deck_size(CARD);
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

    /// The item a press lands on.
    fn pressed(point: (f64, f64), layout: &[Slot], frames: &[Area]) -> Option<usize> {
        item_at(point, layout, frames)
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
            overshoot = overshoot.max(trail.offset(Slot::Chip(0)).0);
            assert!(trail.settled(released).is_none(), "reported while moving");
            assert!(after < 2.0, "still moving after {after:.3}s");
        }
        after += dt;
        let (max_lag, settle) = trail
            .settled(released + Duration::from_secs_f64(after))
            .unwrap();
        assert!(trail.settled(released).is_none(), "reported once per drag");
        assert_eq!(trail.offset(Slot::Chip(0)), (0.0, 0.0));
        (max_lag, overshoot, settle)
    }

    #[test]
    fn chips_hang_80_to_100_pt_behind_a_normal_drag_and_swing_back_once() {
        let (max_lag, overshoot, settle) = drag_and_release(800.0);
        assert!((80.0..=100.0).contains(&max_lag), "max lag {max_lag:.1}");
        // Underdamped: one visible swing past rest (about 16% of the lag),
        // the second one too small to see.
        assert!(overshoot >= 8.0, "overshoot {overshoot:.2}");
        assert!(overshoot <= 0.2 * max_lag, "overshoot {overshoot:.2}");
        let ms = settle.as_millis();
        assert!((600..=900).contains(&ms), "settled in {ms} ms");
        // Back cards follow the same drag more firmly: less lag, no swing.
        let dt = 1.0 / 60.0;
        let mut trail = Trail::default();
        let mut card_lag = 0.0_f64;
        let mut t = 0.0;
        while t < 0.5 {
            trail.panel_dragged(((t / 0.1_f64).min(1.0) * 800.0 * dt, 0.0));
            trail.step(dt);
            card_lag = card_lag.max(trail.offset(Slot::Card(1)).0.abs());
            t += dt;
        }
        assert!(card_lag < max_lag, "cards {card_lag:.1} vs chips {max_lag:.1}");
        trail.release(Instant::now());
        let mut card_overshoot = 0.0_f64;
        while trail.step(dt) {
            card_overshoot = card_overshoot.max(trail.offset(Slot::Card(1)).0);
        }
        assert!(card_overshoot < 6.0, "card overshoot {card_overshoot:.2}");
    }

    #[test]
    fn the_trail_lag_scales_with_speed_but_is_capped() {
        let (slow, _, _) = drag_and_release(200.0);
        assert!(slow < 35.0, "{slow}");
        let (fling, _, _) = drag_and_release(4000.0);
        assert!((fling - TRAIL_LAG_CAP).abs() < 1e-6, "{fling}");
    }

    #[test]
    fn a_snap_drops_any_lag_without_a_report() {
        let mut trail = Trail::default();
        trail.panel_dragged((50.0, 0.0));
        assert_eq!(trail.offset(Slot::Chip(0)), (-50.0, 0.0));
        assert_eq!(trail.offset(Slot::Front), (0.0, 0.0), "the front never lags");
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

    /// Break-it finding: a card dragged onto the Dock (or past any edge of
    /// the visible frame) comes back inside it, by the least distance.
    #[test]
    fn card_screen_is_the_one_holding_the_card() {
        // A (a short display) left of B (a tall one), C further right.
        let screens = [
            Area { x: 0.0, y: 0.0, w: 1440.0, h: 900.0 },
            Area { x: 1440.0, y: 0.0, w: 1920.0, h: 1200.0 },
            Area { x: 3360.0, y: 0.0, w: 1440.0, h: 900.0 },
        ];
        // Center on A, the window (as big as 1600 wide) mostly on B, the
        // pointer (not an input) on C: A.
        let card = Area { x: 1200.0, y: 600.0, w: 400.0, h: 250.0 };
        let window = Area { x: 1000.0, y: 400.0, w: 1600.0, h: 700.0 };
        assert!(window.x + window.w - 1440.0 > 1440.0 - window.x, "the window is mostly on B");
        assert_eq!(card_screen(card, &screens), Some(0));
        // Center above every screen: the one it overlaps most (B).
        let high = Area { x: 1300.0, y: 1100.0, w: 400.0, h: 400.0 };
        assert_eq!(card_screen(high, &screens), Some(1));
        // On no screen at all: none (the caller falls back).
        let off = Area { x: -900.0, y: 0.0, w: 400.0, h: 250.0 };
        assert_eq!(card_screen(off, &screens), None);
        assert_eq!(card_screen(card, &[]), None);
    }

    #[test]
    fn a_card_released_outside_the_visible_frame_comes_back_inside() {
        // 1440x900 screen, Dock 70 pt, menu bar 25 pt.
        let visible = Area { x: 0.0, y: 70.0, w: 1440.0, h: 805.0 };
        let card = |x, y| Area { x, y, w: 320.0, h: 200.0 };
        assert_eq!(keep_inside(card(500.0, 300.0), visible), (0.0, 0.0), "inside: stays");
        assert_eq!(keep_inside(card(500.0, 20.0), visible), (0.0, 50.0), "on the Dock: up just above it");
        assert_eq!(keep_inside(card(1300.0, 300.0), visible), (-180.0, 0.0), "past the right edge");
        assert_eq!(keep_inside(card(-40.0, 700.0), visible), (40.0, -25.0), "past the left edge and under the menu bar");
        // Larger than the frame: its left edge and its top stay inside.
        let big = Area { x: -10.0, y: 0.0, w: 1500.0, h: 900.0 };
        assert_eq!(keep_inside(big, visible), (10.0, -25.0));
    }

    #[test]
    fn resize_is_clamped_between_the_minimum_and_sixty_percent_of_the_screen() {
        // Of the shorter side, both ways: the square that holds the box's
        // cards fits the screen.
        let max = max_card((1440.0, 875.0));
        assert_eq!(max, (525.0, 525.0));
        assert_eq!(hold(max), max);
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
    fn resizing_the_panel_resizes_the_box_and_keeps_the_cards_corner_under_the_pointer() {
        // The window is the square for the box's long side; the card sits in
        // its bottom-right corner.
        let (w, h) = window_size(hold(CARD));
        let start = Area {
            x: 1000.0,
            y: 100.0,
            w,
            h,
        };
        let max = (525.0, 525.0);
        let corner = |frame: Area| (frame.x + frame.w, frame.y);
        let square = |card: (f64, f64), frame: Area| {
            assert_eq!((frame.w, frame.h), window_size(hold(card)));
        };
        // Right edge 50 pt right: a wider box, and the corner goes with the
        // pointer.
        let (card, frame) = resize_panel(start, CARD, RIGHT, (50.0, 7.0), max);
        assert_eq!(card, (CARD.0 + 50.0, CARD.1));
        assert_eq!(corner(frame), (start.x + start.w + 50.0, start.y));
        square(card, frame);
        // Bottom-left dragged down and left: the corner moves down only.
        let (card, frame) = resize_panel(start, CARD, BOTTOM | LEFT, (-40.0, -30.0), max);
        assert_eq!(card, (CARD.0 + 40.0, CARD.1 + 30.0));
        assert_eq!(corner(frame), (start.x + start.w, start.y - 30.0));
        square(card, frame);
        // Top and left edges: the corner stays put, clamped at the limits.
        let (card, frame) = resize_panel(start, CARD, TOP, (0.0, -1000.0), max);
        assert_eq!(card, (CARD.0, MIN_CARD.1));
        assert_eq!(corner(frame), corner(start));
        let (card, frame) = resize_panel(start, CARD, LEFT, (-5000.0, 0.0), max);
        assert_eq!(card, (max.0, CARD.1));
        assert_eq!(corner(frame), corner(start));
        square(card, frame);
        // A turned box is the same panel.
        let (card, frame) = resize_panel(start, (260.0, 320.0), TOP, (0.0, 20.0), max);
        assert_eq!(card, (260.0, 340.0));
        assert_eq!(corner(frame), corner(start));
        square(card, frame);
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
            press_region(point, depth, edges, front, None)
        };
        // 3 pt inside the window's bottom-right corner: the front card's
        // resize corner (this press used to fall through the panel).
        assert_eq!(region((front.x + front.w - 3.0, 3.0)), "corner");
        assert_eq!(region((front.x + front.w - 3.0, front.h / 2.0)), "edge");
        assert_eq!(region((front.x + front.w / 2.0, front.h - 12.0)), "body");
        assert_eq!(region((front.x + front.w / 2.0, front.h / 2.0)), "body");
        assert_eq!(region((front.x + 40.0, front.h + 7.0)), "back-card");
        assert_eq!(region((1.0, 1.0)), "margin");
        // The hover bar protrudes above the front card: a press there is
        // the front card's (a drag), and logged as "bar".
        let bar = bar_frame(front, 100.0);
        let on_bar = (bar.x + bar.w / 2.0, bar.y + bar.h - 2.0);
        assert_eq!(pressed(on_bar, &layout[..1], &frames[..1]), None);
        assert_eq!(pressed_item(on_bar, None, Some(bar)), Some(0));
        assert_eq!(press_region(on_bar, Some(0), 0, front, Some(bar)), "bar");
        assert_eq!(pressed_item((1.0, 1.0), None, Some(bar)), None);
        // The bar overlaps the strip of the card behind the front one: a
        // press there is the bar's (a drag), not a click that raises the
        // back card. Without the bar, the strip is the back card's.
        let strip = (bar.x + bar.w / 2.0, front.y + front.h + 3.0);
        assert!(contains(&bar, strip) && contains(&frames[1], strip));
        assert_eq!(pressed(strip, &layout, &frames), Some(1));
        assert_eq!(pressed_item(strip, Some(1), Some(bar)), Some(0));
        assert_eq!(pressed_item(strip, Some(1), None), Some(1));
        // The bar's bottom overlaps the card's top resize band: the band
        // wins (R1, the polish round: the card's top edge resizes like its
        // other edges), with the bar up or not. Above the band the bar is
        // still a drag handle.
        let overlap = (bar.x + bar.w / 2.0, front.y + front.h - 2.0);
        assert!(contains(&bar, overlap) && resize_edges(overlap, front) == TOP);
        assert_eq!(press_edges(overlap, Some(0), front, Some(bar)), TOP);
        assert_eq!(press_region(overlap, Some(0), TOP, front, Some(bar)), "edge");
        assert_eq!(press_edges(overlap, Some(0), front, None), TOP);
        let above = (bar.x + bar.w / 2.0, front.y + front.h + 8.0);
        assert_eq!(press_edges(above, Some(0), front, Some(bar)), 0);
        assert_eq!(press_region(above, Some(0), 0, front, Some(bar)), "bar");
        // Near the screen's top the bar drops inside the card: its top edge
        // sits in the band too, and resizes; the rest of it drags.
        let inside = bar_frame(front, 0.0);
        let top = (inside.x + inside.w / 2.0, inside.y + inside.h - 1.0);
        assert!(resize_edges(top, front) == TOP);
        assert_eq!(press_edges(top, Some(0), front, Some(inside)), TOP);
        let lower = (inside.x + inside.w / 2.0, inside.y + inside.h / 2.0);
        assert_eq!(press_edges(lower, Some(0), front, Some(inside)), 0);
        // Never on the bar's buttons (R3), where a button dips into the
        // band: the button takes the press, and no resize arrow shows.
        let layout = bar_layout(inside.w);
        let close = (
            inside.x + layout.close.x + layout.close.w / 2.0,
            inside.y + layout.close.y + layout.close.h - 1.0,
        );
        assert!(resize_edges(close, front) == TOP, "the close button reaches into the band");
        assert_eq!(press_edges(close, Some(0), front, Some(inside)), 0);
        let focus = (
            inside.x + layout.focus.x + 2.0,
            inside.y + layout.focus.y + layout.focus.h - 1.0,
        );
        assert_eq!(press_edges(focus, Some(0), front, Some(inside)), 0);
        // The card's top edge beside the bar still resizes, and a press
        // off the front card never does.
        let beside = (front.x + 2.0, front.y + front.h - 2.0);
        assert!(!contains(&bar, beside));
        assert_eq!(press_edges(beside, Some(0), front, Some(bar)), TOP | LEFT);
        assert_eq!(press_edges(overlap, Some(1), front, None), 0);
    }

    #[test]
    fn the_stack_fans_below_near_the_top_and_right_near_the_left_edge() {
        let visible = Area { x: 0.0, y: 0.0, w: 1440.0, h: 875.0 };
        // A card `top` points under the visible top, its left edge at `x`.
        let card = |x: f64, top: f64| Area { x, y: visible.h - top - 233.0, w: 274.0, h: 233.0 };
        // The default spot (bottom right) and the middle: up and left.
        let home = Area { x: 1150.0, y: 16.0, w: 274.0, h: 233.0 };
        assert_eq!(fan_for(home, visible), Fan::default());
        assert_eq!(fan_for(card(600.0, 300.0), visible), Fan::default());
        // Under the visible top by less than the stack's margin: below.
        assert_eq!(fan_for(card(600.0, 0.0), visible), Fan { below: true, right: false });
        assert!(fan_for(card(600.0, STACK_MARGIN - 1.0), visible).below);
        assert!(!fan_for(card(600.0, STACK_MARGIN), visible).below);
        // Closer to the left edge than the deepest card and its chip: right.
        assert_eq!(fan_for(card(0.0, 300.0), visible), Fan { below: false, right: true });
        assert!(fan_for(card(LEFT_REACH - 1.0, 300.0), visible).right);
        assert!(!fan_for(card(LEFT_REACH, 300.0), visible).right);
        // The top-left corner: both.
        assert_eq!(fan_for(card(0.0, 0.0), visible), Fan { below: true, right: true });
        // On a second screen the same rule holds against its own frame.
        let other = Area { x: 1440.0, ..visible };
        let there = Area { x: 1440.0, ..card(0.0, 0.0) };
        assert_eq!(fan_for(there, other), Fan { below: true, right: true });
    }

    #[test]
    fn a_fanned_stack_is_the_default_one_mirrored_through_the_front_card() {
        let card = (274.0, 233.0);
        let layout = slots(&[false, false, true, true]);
        let cards = back_cards(&layout);
        let front = slot_frame(card, Slot::Front, cards);
        let both = Fan { below: true, right: true };
        for slot in &layout {
            let normal = slot_frame(card, *slot, cards);
            let flipped = fanned(normal, front, both);
            assert_eq!((flipped.w, flipped.h), (normal.w, normal.h), "{slot:?} keeps its size");
            assert_eq!(fanned(flipped, front, both), normal, "{slot:?}: mirrored twice is itself");
        }
        assert_eq!(fanned(front, front, both), front, "the front card stays put");
        // A back card peeks below and right by its step.
        let back = fanned(slot_frame(card, Slot::Card(1), cards), front, both);
        assert_eq!(back.x + back.w, front.x + front.w + CARD_STEP);
        assert_eq!(back.y, front.y - CARD_STEP);
        // Chips sit right of the cards, hanging from the card's top down.
        let chip = fanned(slot_frame(card, Slot::Chip(0), cards), front, both);
        assert_eq!(chip.x, front.x + front.w + cards as f64 * CARD_STEP + CHIP_GAP);
        assert_eq!(chip.y + chip.h, front.y + front.h);
        // Only the axis that flips moves.
        let normal = slot_frame(card, Slot::Card(1), cards);
        assert_eq!(fanned(normal, front, Fan { below: false, right: true }).y, normal.y);
        assert_eq!(fanned(normal, front, Fan { below: true, right: false }).x, normal.x);
    }

    #[test]
    fn a_fanned_stack_stays_inside_the_screen_and_its_window_at_the_corners() {
        // Screen 1440x900, visible frame below a 25 pt menu bar (AppKit,
        // bottom-left origin); the card kept inside it at each spot.
        let visible = Area { x: 0.0, y: 0.0, w: 1440.0, h: 875.0 };
        let bounds = hold((320.0, 200.0));
        let (ww, wh) = window_size(bounds);
        for card in [(274.0, 233.0), (320.0, 145.0), (145.0, 320.0)] {
            // Three chips (the tallest chip column), and two cards with a chip.
            for finished in [[false, true, true, true], [false, false, false, true]] {
                let layout = slots(&finished);
                let cards = back_cards(&layout);
                let front = shaped_frame(bounds, card, Slot::Front, cards);
                let spots = [
                    (0.0, visible.h - card.1),
                    (visible.w - card.0, visible.h - card.1),
                    ((visible.w - card.0) / 2.0, visible.h - card.1),
                    (0.0, 300.0),
                    (0.0, 0.0),
                    (visible.w - card.0, 0.0),
                ];
                for (x, y) in spots {
                    // The panel origin that puts the front card there.
                    let origin = (x - front.x, y - front.y);
                    let fan = fan_for(Area { x, y, w: card.0, h: card.1 }, visible);
                    for slot in &layout {
                        let item = fanned(shaped_frame(bounds, card, *slot, cards), front, fan);
                        let screen = Area { x: item.x + origin.0, y: item.y + origin.1, ..item };
                        let at = (x, y, card, slot, fan);
                        assert!(
                            screen.x >= visible.x - 0.5 && screen.x + screen.w <= visible.x + visible.w + 0.5,
                            "{at:?}: {screen:?} leaves the screen sideways"
                        );
                        assert!(
                            screen.y >= visible.y - 0.5 && screen.y + screen.h <= visible.y + visible.h + 0.5,
                            "{at:?}: {screen:?} leaves the visible frame"
                        );
                        let w = to_window(item);
                        assert!(w.x >= 0.0 && w.y >= 0.0 && w.x + w.w <= ww && w.y + w.h <= wh, "{at:?}: {w:?}");
                    }
                }
            }
        }
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
    fn the_bar_sits_above_the_card_and_drops_inside_at_the_screen_top() {
        let front = rest_frame(CARD, 0);
        let above = bar_frame(front, 100.0);
        // Narrower than the card, centered, overlapping its top edge by
        // BAR_OVERLAP and protruding the rest.
        assert_eq!(above.w, front.w - 2.0 * BAR_INSET);
        assert_eq!(above.x, front.x + BAR_INSET);
        assert_eq!(above.y, front.y + front.h - BAR_OVERLAP);
        assert_eq!(above.y + above.h, front.y + front.h + BAR_HEIGHT - BAR_OVERLAP);
        // Exactly enough room above still protrudes; less drops it inside.
        assert_eq!(bar_frame(front, BAR_HEIGHT - BAR_OVERLAP).y, above.y);
        let inside = bar_frame(front, 10.0);
        assert_eq!(inside.y + inside.h, front.y + front.h);
        assert_eq!(inside.w, above.w);
    }

    #[test]
    fn the_bar_fits_at_every_width_with_buttons_pinned_right() {
        for width in [MIN_CARD.0 - 2.0 * BAR_INSET, 200.0, 296.0, 600.0, 1200.0] {
            let layout = bar_layout(width);
            for view in [layout.client_icon, layout.dot, layout.title] {
                assert!(view.w >= 0.0 && view.x >= 0.0, "{width}: {view:?}");
                assert!(view.x + view.w <= layout.focus.x, "{width}: {view:?} under focus");
                assert!(view.y >= 0.0 && view.y + view.h <= BAR_HEIGHT);
            }
            assert_eq!(layout.close.x + layout.close.w, width - 6.0);
            assert!(layout.focus.x + layout.focus.w <= layout.close.x);
            assert_eq!(layout.close.w, BAR_BUTTON);
        }
        // The title keeps usable room at the minimum width.
        assert!(bar_layout(MIN_CARD.0 - 2.0 * BAR_INSET).title.w >= 80.0);
    }
}
