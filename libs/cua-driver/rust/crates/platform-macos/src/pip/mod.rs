//! macOS picture-in-picture preview: one floating glass panel per agent
//! session.
//!
//! Each session (keyed by its private runtime session key, the same key the
//! agent cursor uses) gets its own small panel showing the latest
//! post-action screenshot of the window it is driving, who is driving
//! (client app icon + session label), and where (target app icon + window
//! title). The border takes the session's cursor color so a panel and its
//! cursor read as one agent.
//!
//! ## Never takes focus
//!
//! Panels are instances of `CuaPipPanel`, an `NSPanel` subclass whose
//! `canBecomeKeyWindow` / `canBecomeMainWindow` return NO, created with
//! `NSWindowStyleMaskNonactivatingPanel` and `becomesKeyOnlyIfNeeded`.
//! Clicking or dragging one therefore never activates cua-driver or steals
//! the keyboard from the user's app; buttons use `CuaPipButton`
//! (`acceptsFirstMouse:` YES) so the first click on a non-key panel acts.
//! Panels are shown with `orderFrontRegardless`, never `makeKey`.
//!
//! ## Threading model
//!
//! Mirrors `cursor/overlay.rs`: AppKit runs on the main thread, which
//! `cua-driver/src/main.rs` parks in `NSApplication.run()`. `push_frame()`
//! is called from the tool dispatcher and only enqueues: frames carry no
//! pixels. A dedicated `cua-pip-capture` thread (never the main thread or
//! a tokio worker) captures the newest queued frame per session through
//! `recording::screenshot_for`, bounded by a 1.5 s timeout, then posts the
//! UI work to the main queue with `dispatch_async_f`. A failed or timed-out
//! capture keeps the previous image only if it was of the same window;
//! otherwise the panel shows "Preview unavailable". Frames carry their
//! session's epoch, and a frame whose session ended while it was being
//! captured is dropped instead of re-creating the panel.
//! All panel state lives in `STATE` and is only touched on the main queue
//! (the mutex exists to make the static `Sync`, not for contention).
//!
//! ## Lifecycle
//!
//! - A session's first frame creates its panel at the bottom-right corner of
//!   the main screen's visible frame (above the Dock, clear of notification
//!   banners); later panels stack upward, then wrap to a new column on the
//!   left, so two agents' panels do not overlap. Only shown panels hold a
//!   slot: a hidden panel releases its slot, and every time a panel is shown
//!   it moves to the lowest free one. A panel the user dragged or resized
//!   keeps its place and holds no slot, and an ended session's dragged
//!   position and resized size are remembered while the daemon runs.
//! - 8 s without a new frame for that session: fade out (0.25 s), then
//!   `orderOut`. The next frame fades it back in.
//! - While the session's target window is fully visible to the user (see
//!   `visibility`), the panel stays hidden; it returns when the window is
//!   covered, moves off screen or to another Space. Every frame carries a
//!   fresh answer, and a `cua-pip-visibility` thread re-checks active
//!   sessions every 500 ms between frames.
//! - The header's close button hides the panel until the session's next
//!   frame; the focus button brings the target window forward through the
//!   same code path as the `bring_to_front` tool.
//! - Session end: fade out, close, release.
//!
//! ## Live mirror
//!
//! While a panel is shown, a ScreenCaptureKit stream of its target window
//! (see `live`) draws into a layer over the screenshot well. Stills keep
//! arriving underneath: when no stream can run (no permission, window
//! gone) or it stops, the layer clears and the still (or "Preview
//! unavailable") shows through.
//!
//! ## Card stack
//!
//! The panel is a deck (see `stack`): the front card is the target described
//! above, live; up to three windows the session acted in recently are back
//! items. A back window the session has not finished with is a card, each
//! `CARD_STEP` up and left of the card in front, showing its last still
//! (only a still tagged with its own window) under a title strip. A window
//! it finished with (see `finish`) collapses into a chip: its app icon with
//! a green check, captioned with its title, in a column left of the cards.
//! Acting in a back item's window, or clicking it, springs it to the front
//! and tucks the old front behind; a click only re-targets the panel (never
//! focuses the window or activates cua-driver). A back item drops 30 s after
//! the session last acted in its window, or when the window closes.
//! Shown/hidden still follows the front card's window only.
//!
//! The front card is a view inside the panel, which reserves transparent
//! room above and left of it where back cards rest. Back items live in a
//! second window, the trail: a borderless non-activating `CuaPipPanel` like
//! the panel (never key, never main, fades with it, owned by this process so
//! the visibility checks and window-scoped captures never see it) ordered
//! just below the panel and widened left for the chip column. Its fully
//! transparent pixels let clicks through; items sit on hit plates. The
//! panel handles its own mouse: a press on a card and a drag moves the
//! panel while the trail hangs back on a loose spring (see
//! `stack::TRAIL_OMEGA`) and swings after it, a press in the band just
//! inside the front card's edges resizes it (60% of the screen at most,
//! remembered per session like a dragged position), and a click on a back
//! item raises it. The live stream is resized to the new well 150 ms after
//! resizing stops.
//!
//! ## Finished state
//!
//! `verify_state` results arrive as labelled claims (`push_verification`)
//! straight to the main queue. When the session finishes (8 s idle after
//! acting, or `end_session`) while its panel is up, the front card shows a
//! checklist of its recent claims, rows coming in 80 ms apart, (or, with no
//! claims, the windows it touched as chips), holds 2.5 s, then the panel
//! fades. A new action cancels it and the panel is live again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use pip_preview::{PipBackend, PipConfig, PipFrame};

mod finish;
mod live;
mod stack;
mod visibility;

use finish::{Claim, Finale, FinaleState, Verdicts};
use live::{Event, Request, StreamStep, Streams};
use pip_preview::PipVerification;
use stack::{
    back_cards, card_size, item_at, max_card, own_pixels, resize_edges, resize_settled,
    resize_window, slot_frame, trail_frame, window_size, CardStack, Motion, Slot, Trail,
    CHIP_REACH, DRAG_SLOP, MAX_CARDS, MIN_CARD, RESIZE_DEBOUNCE, TRAIL_PAD, VIEWS,
};

// ── CGColor objc2 encoding shim ────────────────────────────────────────────
//
// `[NSColor CGColor]` returns a `CGColorRef` whose Objective-C type encoding
// is `^{CGColor=}`. objc2's strict msg_send! enforcement rejects bare
// `*mut c_void` (`^v`), so declare a phantom struct with the matching
// encoding.

#[repr(C)]
struct CGColor {
    _opaque: [u8; 0],
}

unsafe impl objc2::RefEncode for CGColor {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGColor", &[]));
}

/// `CGPathRef` for `CAShapeLayer.path`, same idea.
#[repr(C)]
struct CGPath {
    _opaque: [u8; 0],
}

unsafe impl objc2::RefEncode for CGPath {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGPath", &[]));
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPathCreateMutable() -> *mut CGPath;
    fn CGPathMoveToPoint(path: *mut CGPath, transform: *const c_void, x: f64, y: f64);
    fn CGPathAddLineToPoint(path: *mut CGPath, transform: *const c_void, x: f64, y: f64);
    fn CGPathRelease(path: *mut CGPath);
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    fn CACurrentMediaTime() -> f64;
}

// ── Tunables ──────────────────────────────────────────────────────────────

const IDLE_HIDE_AFTER: Duration = Duration::from_secs(8);
const FADE: Duration = Duration::from_millis(250);
const CORNER_RADIUS: f64 = 14.0;
const BORDER_WIDTH: f64 = 2.0;
const HEADER_HEIGHT: f64 = 28.0;
const STATUS_HEIGHT: f64 = 16.0;
const PAD: f64 = 8.0;
/// Distance from the screen's visible-frame edge to the first panel.
const EDGE_INSET: f64 = 16.0;
/// Gap between stacked panels.
const STACK_GAP: f64 = 10.0;
/// How often active sessions re-check whether their window is fully visible.
const VISIBILITY_POLL: Duration = Duration::from_millis(500);

// ── Pure placement / timing decisions (unit tested) ───────────────────────

/// Rectangle in screen points (AppKit bottom-left origin for placement,
/// CoreGraphics top-left origin for window visibility).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Bottom-left origin for the panel in stacking `slot`. Slot 0's
/// bottom-right corner sits at `bottom_right`. Later slots stack in the
/// vertical direction with more room from slot 0 (up on ties), and a full
/// column wraps to a new column toward the side with more room (left on
/// ties), so the default bottom-right anchor stacks upward then wraps left
/// and a top-left anchor stacks downward then wraps right. Every frame is
/// inside `visible` (slot 0 is clamped first, so the rest follow from where
/// it really is); a full grid overlaps its last slot instead of going
/// off-screen.
fn stack_origin(
    bottom_right: (f64, f64),
    visible: Area,
    size: (f64, f64),
    slot: usize,
) -> (f64, f64) {
    let (w, h) = size;
    let clamp = |v: f64, lo: f64, size: f64, extent: f64| v.min(lo + extent - size).max(lo);
    // Slot 0 first, inside the visible frame: room, directions and offsets
    // all come from where it really is, so slots never overlap even for an
    // anchor that is off-screen.
    let x0 = clamp(bottom_right.0 - w, visible.x, w, visible.w);
    let y0 = clamp(bottom_right.1, visible.y, h, visible.h);
    let (step_y, step_x) = (h + STACK_GAP, w + STACK_GAP);
    // Space left beyond slot 0 in each direction, edge inset included.
    let up = visible.y + visible.h - EDGE_INSET - (y0 + h);
    let down = y0 - visible.y - EDGE_INSET;
    let left = x0 - visible.x - EDGE_INSET;
    let right = visible.x + visible.w - EDGE_INSET - (x0 + w);
    let (dir_y, room_y) = if up >= down { (1.0, up) } else { (-1.0, down) };
    let (dir_x, room_x) = if left >= right {
        (-1.0, left)
    } else {
        (1.0, right)
    };
    // Panels that fit in a column / row of columns, slot 0 included.
    let fit = |room: f64, step: f64| (room.max(0.0) / step).floor() as usize + 1;
    let (per_column, columns) = (fit(room_y, step_y), fit(room_x, step_x));
    let slot = slot.min(per_column * columns - 1);
    let (column, row) = (slot / per_column, slot % per_column);
    (
        x0 + dir_x * column as f64 * step_x,
        y0 + dir_y * row as f64 * step_y,
    )
}

/// Bottom-right corner of slot 0: the visible frame's bottom-right (above
/// the Dock) inset by `EDGE_INSET`, or, when `--experimental-pip-geometry
/// WxH+X+Y` gave a position, the panel whose top-left is at X,Y (top-left
/// screen origin).
fn first_slot_bottom_right(
    screen: Area,
    visible: Area,
    size: (f64, f64),
    anchor: Option<(i32, i32)>,
) -> (f64, f64) {
    let (w, h) = size;
    match anchor {
        Some((x, y)) => (screen.x + x as f64 + w, screen.y + screen.h - y as f64 - h),
        None => (visible.x + visible.w - EDGE_INSET, visible.y + EDGE_INSET),
    }
}

/// Origin of a panel window of `size` in cascade `slot`. Placement works on
/// the panel's footprint: the window plus the chip column that can reach
/// `CHIP_REACH` left of it, so chips stay on screen and clear of the
/// neighbouring column.
fn panel_origin(
    screen: Area,
    visible: Area,
    size: (f64, f64),
    anchor: Option<(i32, i32)>,
    slot: usize,
) -> (f64, f64) {
    let footprint = (size.0 + CHIP_REACH, size.1);
    let bottom_right = first_slot_bottom_right(screen, visible, footprint, anchor);
    let (x, y) = stack_origin(bottom_right, visible, footprint, slot);
    (x + CHIP_REACH, y)
}

/// Lowest cascade slot no live panel occupies.
fn free_slot(used: impl IntoIterator<Item = usize>) -> usize {
    let used: std::collections::HashSet<usize> = used.into_iter().collect();
    (0..).find(|slot| !used.contains(slot)).unwrap_or(0)
}

/// The slot a panel takes when it is shown, given the slots of the other
/// shown panels. A dragged panel keeps its position and takes none.
fn slot_on_show(others: impl IntoIterator<Item = usize>, dragged: bool) -> Option<usize> {
    (!dragged).then(|| free_slot(others))
}

/// Whether a panel whose last frame arrived at `last_frame` should fade out.
fn idle_hide_due(last_frame: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_frame) >= IDLE_HIDE_AFTER
}

/// A panel shows while its session is active, the user has not closed it
/// since the last frame, and the user cannot already see the window itself.
fn panel_should_show(active: bool, dismissed: bool, target_fully_visible: bool) -> bool {
    active && !dismissed && !target_fully_visible
}

/// A captured frame is applied only if its session is still in the epoch
/// the frame was pushed in. `None` means the session has ended.
fn should_apply(frame_epoch: u64, current_epoch: Option<u64>) -> bool {
    current_epoch == Some(frame_epoch)
}

/// The resolved target (pid, window id) that pixels came from. A pid-only
/// target is tagged with the window it resolved to when it was captured.
type Tag = Target;

/// The resolved target a panel is showing now: its window if it names one,
/// else the window its pid currently resolves to; `None` when unresolved.
fn current_tag(target: Target, resolved_window: Option<u32>) -> Option<Tag> {
    match target {
        (_, Some(_)) => Some(target),
        (Some(pid), None) => resolved_window.map(|window| (Some(pid), Some(window))),
        (None, None) => None,
    }
}

/// Which of the panel's three layers show. The image area only ever shows
/// pixels captured from the CURRENT resolved target: a layer whose tag is not
/// the current one is hidden (and its pixels dropped by the caller), and
/// "Preview unavailable" shows when neither layer qualifies.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layers {
    show_still: bool,
    show_live: bool,
    show_placeholder: bool,
}

/// `still_tag` / `live_tag` are `None` when that layer holds no pixels.
fn visible_layers(current: Option<Tag>, still_tag: Option<Tag>, live_tag: Option<Tag>) -> Layers {
    let matches = |tag: Option<Tag>| current.is_some() && tag == current;
    let live = matches(live_tag);
    let still = matches(still_tag);
    Layers {
        show_live: live,
        // The still is hidden under a live frame.
        show_still: still && !live,
        show_placeholder: !live && !still,
    }
}

/// Short display id for a private session key: drop the runtime namespace,
/// keep a few characters. Used only in the (invisible) window title.
fn short_key(key: &str) -> String {
    let public = key
        .strip_prefix("__cua_runtime_")
        .and_then(|rest| rest.split_once(':'))
        .map_or(key, |(_, public)| public);
    public.chars().take(12).collect()
}

/// First word of an MCP client name, lowercased, for matching a running
/// app: "Claude Code" → "claude", "codex-mcp-client" → "codex".
fn client_match_token(client_name: &str) -> Option<String> {
    let token = client_name
        .split(|c: char| !c.is_alphanumeric())
        .find(|word| !word.is_empty())?
        .to_lowercase();
    (token.len() >= 3).then_some(token)
}

// ── Panel state (main queue only) ─────────────────────────────────────────

/// Client identity a header was resolved from: (client name, client pid,
/// public session label).
type ClientIdentity = (Option<String>, Option<i32>, Option<String>);

/// Native pointers are stored as `usize` so the state is `Send`; they are
/// only dereferenced on the main queue.
struct Panel {
    id: i64,
    window: usize,
    image_view: usize,
    /// Layer-hosting view over the image well that shows live frames.
    live_view: usize,
    live_layer: usize,
    /// The live frame on screen, held so ScreenCaptureKit does not recycle
    /// its IOSurface while the layer shows it. `None` shows the still.
    live_frame: Option<screencapturekit::CVPixelBuffer>,
    /// Target the live frame came from (`None` with no live frame).
    live_tag: Option<Tag>,
    /// Target the still in `image_view` came from (`None` with no still).
    still_tag: Option<Tag>,
    /// The panel's stream request and which generation's events still count.
    stream: live::StreamState,
    /// The window a pid-only `target` currently resolves to, as last looked
    /// up off the main thread (the stream follows it).
    resolved_window: Option<u32>,
    /// "Preview unavailable", centered over the image well.
    placeholder: usize,
    status: usize,
    client_icon: usize,
    client_label: usize,
    target_icon: usize,
    target_title: usize,
    /// The front card's view (it holds everything above) and the views
    /// laid out with its size.
    front_view: usize,
    glass: usize,
    header: usize,
    focus: usize,
    close: usize,
    /// The trail window and its back item views.
    trail: TrailViews,
    /// Each view's hit plate (see `new_hit_plate`), indexed like
    /// `Slot::view`, framed and hidden with its view: the front card's in
    /// the panel, the rest in the trail.
    plates: [usize; VIEWS],
    /// Windows the session acted in, front card first.
    cards: CardStack<Tag, CardInfo>,
    /// Where each item of `cards` is drawn, front first (at least the
    /// front).
    layout: Vec<Slot>,
    /// Each view's animated offset from its resting frame.
    motion: [Motion; VIEWS],
    /// The trail window's lag behind the panel.
    trail_motion: Trail,
    /// What the session verified and finished.
    verdicts: Verdicts,
    /// Windows logged as finished (so each is logged once per finish).
    finished_seen: HashSet<u32>,
    finale: FinaleState,
    /// The finished-state overlay on the front card, while it is up.
    finale_view: Option<usize>,
    /// Private session key (for logs from callbacks that only have the
    /// panel).
    key: String,
    /// Front card size in points.
    card: (f64, f64),
    /// Front card size its views were last laid out for.
    laid_out: (f64, f64),
    /// Image well size the live stream is sized for: follows `card` once a
    /// resize has settled.
    stream_well: (f64, f64),
    /// When the front card last changed size.
    well_changed: Instant,
    /// The user resized the panel.
    resized: bool,
    /// Session label (or short key) for the window title.
    name: String,
    /// Cascade slot held while shown; `None` while hidden or dragged.
    slot: Option<usize>,
    /// The user moved the panel off the origin we last placed it at.
    dragged: bool,
    /// Origin (AppKit) we last placed the window at.
    placed: (f64, f64),
    last_frame: Instant,
    shown: bool,
    /// Closed with the x button since the last frame.
    dismissed: bool,
    /// The target window is fully visible to the user.
    target_visible: bool,
    target: (Option<i32>, Option<u32>),
    client: Option<ClientIdentity>,
}

/// A back card's views.
struct BackView {
    view: usize,
    image_view: usize,
    icon: usize,
    title: usize,
}

/// A chip's views.
struct ChipView {
    view: usize,
    icon: usize,
    caption: usize,
}

/// The trail window: back cards (depth 1 first) and chips (bottom first).
struct TrailViews {
    window: usize,
    cards: [BackView; MAX_CARDS - 1],
    chips: [ChipView; MAX_CARDS - 1],
}

/// What a card shows besides live pixels, kept so a window that goes behind
/// (or comes back to the front) keeps its title, icon and status.
#[derive(Default)]
struct CardInfo {
    title: String,
    pid: Option<i32>,
    status: String,
    /// The window's last still while its card is behind, with the window it
    /// was captured from (shown only if that is the card's own window).
    still: Option<(Tag, Image)>,
}

/// A retained `NSImage`, released on drop (like all panel state, only on
/// the main queue).
struct Image(usize);

impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.0 as *mut AnyObject, release];
        }
    }
}

/// What an ended session's panel leaves behind while the daemon runs.
#[derive(Debug, Default, Clone, Copy)]
struct Remembered {
    /// Origin of a panel the user dragged or resized.
    origin: Option<(f64, f64)>,
    /// Front card size of a panel the user resized.
    card: Option<(f64, f64)>,
}

/// A mouse press on a panel, until it is released.
struct Gesture {
    /// The pressed panel's id (it may be a live or an ending panel).
    id: i64,
    /// Pointer (screen points) and window frame at the press.
    mouse: (f64, f64),
    start: Area,
    /// Window origin after the last drag step.
    origin: (f64, f64),
    /// Front card edges being resized (0 = not a resize).
    edges: u8,
    /// The window of the back card pressed: a click raises that window, not
    /// whatever sits at the pressed depth when the button comes up.
    pressed: Option<Tag>,
    /// The pointer went past `DRAG_SLOP`: a drag, not a click.
    moved: bool,
    /// Largest front card on the panel's screen.
    max: (f64, f64),
}

struct State {
    image_size: (f64, f64),
    anchor: Option<(i32, i32)>,
    panels: HashMap<String, Panel>,
    /// Panels of ended sessions still playing their finale.
    ending: Vec<Panel>,
    /// Verifications of sessions whose first frame is still being captured.
    early: HashMap<String, Verdicts>,
    /// Each ended session's dragged position and resized size, kept while
    /// the daemon runs.
    // ponytail: one small entry per ended session; cap it if a daemon ever
    // sees many thousands of sessions.
    remembered: HashMap<String, Remembered>,
    next_id: i64,
    worker: Arc<CaptureWorker>,
    streams: Arc<Streams>,
    next_stream_generation: u64,
    gesture: Option<Gesture>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map(f)
}

/// `with_state` for AppKit callbacks that can also run inside a state
/// operation on the main queue (which already holds the lock, e.g. a view
/// resized while its panel is built): `None` then.
fn try_with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mut guard = match STATE.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    guard.as_mut().map(f)
}

// ── libdispatch glue ──────────────────────────────────────────────────────

#[link(name = "System", kind = "framework")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(
        when: u64,
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

const DISPATCH_TIME_NOW: u64 = 0;

fn main_queue() -> *const c_void {
    &raw const _dispatch_main_q as *const c_void
}

fn dispatch_to_main<T: Send + 'static>(payload: T, cb: unsafe extern "C" fn(*mut c_void)) {
    let boxed = Box::into_raw(Box::new(payload)) as *mut c_void;
    unsafe { dispatch_async_f(main_queue(), boxed, cb) };
}

fn dispatch_to_main_after<T: Send + 'static>(
    delay: Duration,
    payload: T,
    cb: unsafe extern "C" fn(*mut c_void),
) {
    let boxed = Box::into_raw(Box::new(payload)) as *mut c_void;
    let delta = delay.as_nanos().min(i64::MAX as u128) as i64;
    unsafe {
        dispatch_after_f(
            dispatch_time(DISPATCH_TIME_NOW, delta),
            main_queue(),
            boxed,
            cb,
        )
    };
}

// ── Capture worker ────────────────────────────────────────────────────────
//
// Frames arrive without pixels (the dispatcher must never capture inside an
// action). One worker thread captures each session's target through the
// shared `recording::screenshot_for`, with a hard per-capture timeout, and
// only for the newest frame a session has queued.

const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Latest pending item per session, sessions served in arrival order. A
/// newer item for a queued session replaces the older one in place.
struct LatestPerSession<T> {
    order: VecDeque<String>,
    latest: HashMap<String, T>,
}

impl<T> LatestPerSession<T> {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            latest: HashMap::new(),
        }
    }

    fn push(&mut self, key: String, item: T) {
        if self.latest.insert(key.clone(), item).is_none() {
            self.order.push_back(key);
        }
    }

    fn pop(&mut self) -> Option<(String, T)> {
        while let Some(key) = self.order.pop_front() {
            if let Some(item) = self.latest.remove(&key) {
                return Some((key, item));
            }
        }
        None
    }

    fn remove(&mut self, key: &str) {
        self.latest.remove(key);
    }
}

/// Epoch of each live session. A session's first frame starts a new epoch
/// and ending it drops the entry, so a session that ends and restarts under
/// the same key never matches frames from its earlier life.
#[derive(Default)]
struct SessionEpochs {
    live: HashMap<String, u64>,
    next: u64,
}

impl SessionEpochs {
    fn stamp(&mut self, key: &str) -> u64 {
        if let Some(&epoch) = self.live.get(key) {
            return epoch;
        }
        self.next += 1;
        self.live.insert(key.to_owned(), self.next);
        self.next
    }

    fn end(&mut self, key: &str) {
        self.live.remove(key);
    }

    fn current(&self, key: &str) -> Option<u64> {
        self.live.get(key).copied()
    }
}

/// (pid, window_id) of a capture target.
type Target = (Option<i32>, Option<u32>);
/// A still: the window it was captured from, and its PNG.
type Shot = (u32, Vec<u8>);
type CaptureFn = dyn Fn(Target) -> Option<Shot> + Send + Sync;

struct CaptureWorker {
    /// Each frame with the epoch its session was in when it was pushed.
    queue: Mutex<LatestPerSession<(PipFrame, u64)>>,
    epochs: Mutex<SessionEpochs>,
    ready: Condvar,
    capture: Arc<CaptureFn>,
    timeout: Duration,
    /// Targets with a capture still running, including ones that timed out.
    /// A stuck target gets no second capture thread until the first returns.
    in_flight: Arc<Mutex<HashSet<Target>>>,
    /// Each live session's latest target and when its last frame was pushed.
    active: Mutex<HashMap<String, (Target, Instant)>>,
    /// Windows of each session's back cards, checked for closing by the
    /// visibility poll.
    watched: Mutex<HashMap<String, Vec<u32>>>,
}

impl CaptureWorker {
    fn start(
        capture: Arc<CaptureFn>,
        timeout: Duration,
        deliver: impl Fn(PipFrame, u64, Option<Shot>) + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let worker = Arc::new(Self {
            queue: Mutex::new(LatestPerSession::new()),
            epochs: Mutex::new(SessionEpochs::default()),
            ready: Condvar::new(),
            capture,
            timeout,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            active: Mutex::new(HashMap::new()),
            watched: Mutex::new(HashMap::new()),
        });
        let looping = worker.clone();
        std::thread::Builder::new()
            .name("cua-pip-capture".into())
            .spawn(move || loop {
                let (frame, epoch) = looping.next();
                let png = looping.capture_bounded((frame.target_pid, frame.target_window_id));
                deliver(frame, epoch, png);
            })?;
        Ok(worker)
    }

    /// Enqueue only; never captures, never blocks on a capture.
    ///
    /// A session's first frame sets its polled target; after that only
    /// `retarget` does (the panel's displayed target is the one source of
    /// truth), so polls keep answering for what the panel shows while this
    /// frame is captured.
    fn push(&self, frame: PipFrame) {
        let target = (frame.target_pid, frame.target_window_id);
        let now = Instant::now();
        lock(&self.active)
            .entry(frame.session_key.clone())
            .and_modify(|(_, pushed)| *pushed = now)
            .or_insert((target, now));
        let epoch = lock(&self.epochs).stamp(&frame.session_key);
        lock(&self.queue).push(frame.session_key.clone(), (frame, epoch));
        self.ready.notify_one();
    }

    /// Drop the session's queued frame and end its epoch, so a capture
    /// already running for it is discarded on delivery.
    fn forget(&self, session_key: &str) {
        lock(&self.epochs).end(session_key);
        lock(&self.queue).remove(session_key);
        lock(&self.active).remove(session_key);
        lock(&self.watched).remove(session_key);
    }

    /// Sessions that pushed a frame within the idle window, with their
    /// latest target and the windows of their back cards.
    fn active_targets(&self, now: Instant) -> Vec<(String, Target, Vec<u32>)> {
        let watched = lock(&self.watched);
        lock(&self.active)
            .iter()
            .filter(|(_, (_, pushed))| !idle_hide_due(*pushed, now))
            .map(|(key, (target, _))| {
                let windows = watched.get(key).cloned().unwrap_or_default();
                (key.clone(), *target, windows)
            })
            .collect()
    }

    /// The windows of the session's back cards now. Ignored once the
    /// session ended (its panel may still be playing its finale).
    fn watch(&self, session_key: &str, windows: Vec<u32>) {
        if !self.is_live(session_key) {
            return;
        }
        lock(&self.watched).insert(session_key.to_owned(), windows);
    }

    /// The panel now shows `target`: polls answer for it. Called in the same
    /// step as every change of the displayed target (`set_panel_target`).
    fn retarget(&self, session_key: &str, target: Target) {
        if let Some((current, _)) = lock(&self.active).get_mut(session_key) {
            *current = target;
        }
    }

    /// A frame for the session was delivered at `at`. The session's activity
    /// deadline is the later of that and its last push, so the visibility
    /// poll and the panel's idle hide count from the same moment.
    fn mark_delivered(&self, session_key: &str, at: Instant) {
        if let Some((_, last)) = lock(&self.active).get_mut(session_key) {
            *last = (*last).max(at);
        }
    }

    fn is_current(&self, session_key: &str, epoch: u64) -> bool {
        should_apply(epoch, lock(&self.epochs).current(session_key))
    }

    /// The session has pushed a frame and not ended.
    fn is_live(&self, session_key: &str) -> bool {
        lock(&self.epochs).current(session_key).is_some()
    }

    fn next(&self) -> (PipFrame, u64) {
        let mut queue = lock(&self.queue);
        loop {
            if let Some((_, frame)) = queue.pop() {
                return frame;
            }
            queue = self.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Capture `target` on a helper thread, waiting at most `timeout`.
    /// `None` on timeout, failure, or while an earlier capture of the same
    /// target is still stuck.
    fn capture_bounded(&self, target: Target) -> Option<Shot> {
        if !lock(&self.in_flight).insert(target) {
            return None;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let capture = self.capture.clone();
        let in_flight = self.in_flight.clone();
        let spawned = std::thread::Builder::new()
            .name("cua-pip-shot".into())
            .spawn(move || {
                let png = capture(target);
                lock(&in_flight).remove(&target);
                let _ = sender.send(png);
            });
        if spawned.is_err() {
            lock(&self.in_flight).remove(&target);
            return None;
        }
        receiver.recv_timeout(self.timeout).ok().flatten()
    }
}

/// The one window a PiP capture may target: the frame's window, else the
/// pid's frontmost on-screen window, else nothing. Never the whole display
/// (that would film the PiP panels themselves).
fn capture_window(target: Target, frontmost_of: impl FnOnce(i32) -> Option<u32>) -> Option<u32> {
    match target {
        (_, Some(window_id)) => Some(window_id),
        (Some(pid), None) => frontmost_of(pid),
        (None, None) => None,
    }
}

/// The window a target's captures and stream use right now: `capture_window`
/// with the pid's main window as the fallback. Does WindowServer lookups, so
/// never call it on the main thread.
fn resolve_target_window(target: Target) -> Option<u32> {
    capture_window(target, |pid| {
        crate::windows::resolve_main_window_id(pid).ok()
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Backend ───────────────────────────────────────────────────────────────

pub struct MacosPipBackend {
    worker: Arc<CaptureWorker>,
}

struct FrameUpdate {
    frame: PipFrame,
    epoch: u64,
    /// Fresh screenshot, or `None` when the capture failed or timed out.
    png: Option<Shot>,
    /// Window title (or owning app name), looked up on the capture worker.
    target_title: Option<String>,
    /// Whether the user can already see the whole target window.
    target_visible: bool,
    /// The window a pid-only target resolves to (see `Panel::resolved_window`).
    resolved_window: Option<u32>,
}

/// A between-frames visibility re-check from the `cua-pip-visibility` thread.
struct VisibilityUpdate {
    key: String,
    target: Target,
    visible: bool,
    resolved_window: Option<u32>,
    /// Back-card windows that no longer exist.
    gone: Vec<u32>,
}

impl PipBackend for MacosPipBackend {
    fn push_frame(&self, frame: PipFrame) {
        // The action itself goes straight to the main queue: the capture
        // queue keeps only a session's latest frame, so a coalesced frame
        // must not take the record of its action with it.
        let action = Action {
            key: frame.session_key.clone(),
            target: (frame.target_pid, frame.target_window_id),
            timestamp_ms: frame.timestamp_ms,
        };
        self.worker.push(frame);
        dispatch_to_main(action, apply_action_cb);
    }

    /// Straight to the main queue: never waits on a capture, and the
    /// timestamps (not arrival order) decide what is finished.
    fn push_verification(&self, verification: PipVerification) {
        dispatch_to_main(verification, apply_verify_cb);
    }

    fn end_session(&self, session_key: &str) {
        self.worker.forget(session_key);
        dispatch_to_main(session_key.to_owned(), end_session_cb);
    }

    fn shutdown(self: Box<Self>) {
        dispatch_to_main((), shutdown_cb);
    }
}

/// Runs on the capture worker: look up the window title and visibility
/// (synchronous WindowServer calls) and hand the update to the main queue.
fn deliver_to_main(frame: PipFrame, epoch: u64, png: Option<Shot>) {
    let (windows, displays) = visibility::snapshot();
    let target_visible = visibility::target_fully_visible(
        (frame.target_pid, frame.target_window_id),
        &windows,
        &displays,
        std::process::id() as i32,
    );
    let resolved_window = resolve_target_window((frame.target_pid, frame.target_window_id));
    let target_title = frame
        .target_window_id
        .and_then(crate::windows::window_info_by_id)
        .map(|window| {
            if window.title.trim().is_empty() {
                window.app_name
            } else {
                window.title
            }
        })
        .filter(|title| !title.trim().is_empty());
    dispatch_to_main(
        FrameUpdate {
            frame,
            epoch,
            png,
            target_title,
            target_visible,
            resolved_window,
        },
        apply_frame_cb,
    );
}

/// Between frames, re-check every active session's target twice a second
/// and send the answer to the main queue (which ignores unchanged ones).
fn poll_visibility(worker: &CaptureWorker) {
    let own_pid = std::process::id() as i32;
    loop {
        std::thread::sleep(VISIBILITY_POLL);
        let active = worker.active_targets(Instant::now());
        if active.is_empty() {
            continue;
        }
        let (windows, displays) = visibility::snapshot();
        // Every window WindowServer knows (any Space, minimized included),
        // only when some back card needs checking. An empty answer is a
        // failed lookup, not every window closing.
        let known: Option<HashSet<u32>> = active
            .iter()
            .any(|(_, _, watched)| !watched.is_empty())
            .then(|| {
                crate::windows::all_windows_any_layer()
                    .iter()
                    .map(|window| window.window_id)
                    .collect()
            })
            .filter(|known: &HashSet<u32>| !known.is_empty());
        for (key, target, watched) in active {
            let visible = visibility::target_fully_visible(target, &windows, &displays, own_pid);
            let resolved_window = resolve_target_window(target);
            let gone = known.as_ref().map_or_else(Vec::new, |known| {
                watched
                    .into_iter()
                    .filter(|id| !known.contains(id))
                    .collect()
            });
            dispatch_to_main(
                VisibilityUpdate {
                    key,
                    target,
                    visible,
                    resolved_window,
                    gone,
                },
                visibility_cb,
            );
        }
    }
}

/// Runs on ScreenCaptureKit's queue: hand the live event to the main queue.
fn deliver_live(event: Event) {
    dispatch_to_main(event, live_event_cb);
}

pub fn start(cfg: &PipConfig) -> anyhow::Result<Box<dyn PipBackend>> {
    let capture: Arc<CaptureFn> = Arc::new(|target: Target| {
        let window_id = resolve_target_window(target)?;
        // Always window-scoped, so other PiP panels are never in the image.
        cua_driver_core::recording::screenshot_for(Some(u64::from(window_id)), None)
            .map(|png| (window_id, png))
    });
    let worker = CaptureWorker::start(capture, CAPTURE_TIMEOUT, deliver_to_main)?;
    let streams = Streams::start(deliver_live)?;
    let polled = worker.clone();
    std::thread::Builder::new()
        .name("cua-pip-visibility".into())
        .spawn(move || poll_visibility(&polled))?;
    // Panels are created lazily, on the main queue, by each session's first
    // frame; nothing native happens here.
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(State {
        image_size: (cfg.geometry.width as f64, cfg.geometry.height as f64),
        anchor: cfg.geometry.x.zip(cfg.geometry.y),
        panels: HashMap::new(),
        ending: Vec::new(),
        early: HashMap::new(),
        remembered: HashMap::new(),
        next_id: 1,
        worker: worker.clone(),
        streams,
        next_stream_generation: 0,
        gesture: None,
    });
    Ok(Box::new(MacosPipBackend { worker }))
}

// ── Main-queue callbacks ──────────────────────────────────────────────────

unsafe extern "C" fn apply_frame_cb(ctx: *mut c_void) {
    let update: FrameUpdate = *Box::from_raw(ctx as *mut FrameUpdate);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| apply_frame(state, update));
    });
}

unsafe fn apply_frame(state: &mut State, update: FrameUpdate) {
    let FrameUpdate {
        frame,
        epoch,
        png,
        target_title,
        target_visible,
        resolved_window,
    } = update;
    let key = frame.session_key.clone();
    // Checked here, on the main queue, so a delivery already queued behind
    // `end_session` is dropped too.
    if !state.worker.is_current(&key, epoch) {
        return;
    }
    if !state.panels.contains_key(&key) {
        let Some(panel) = create_panel(state, &key, frame.session_label.as_deref()) else {
            return;
        };
        state.panels.insert(key.clone(), panel);
    }
    let worker = state.worker.clone();
    let Some(panel) = state.panels.get_mut(&key) else {
        return;
    };

    let new_target = (frame.target_pid, frame.target_window_id);
    let now = Instant::now();
    // A new action cancels a finale in progress: back to live. The overlay
    // goes whether or not the finale is still playing: one that just ended
    // is still up during the fade, and this frame re-shows the panel.
    if panel.finale.cancel() {
        tracing::info!(target: "pip", session = %key, "PiP finished state cancelled by a new action");
    }
    remove_finale_view(panel);
    // Where: target app icon + window title.
    let title = show_target(panel, frame.target_pid, target_title);
    // The session acted in this window: it becomes the front card (before
    // the new still lands, so the old front takes its own still behind).
    let tag = current_tag(new_target, resolved_window);
    let mut restacked = false;
    if let Some(tag) = tag {
        restacked |= switch_front(panel, &key, &worker, tag, Some(now));
    }
    restacked |= restack(panel, &key, &worker, |panel| {
        panel.cards.prune(now, |_| false);
        if let Some(tag) = tag {
            panel.verdicts.act(tag, &title, frame.timestamp_ms);
        }
    });
    note_finished(panel, &key);

    let image_view = panel.image_view as *mut AnyObject;
    // A failed capture keeps the old still; `sync_layers` (from `refresh`,
    // below) drops it unless it is of the panel's current window.
    if let Some((window, png)) = png {
        // `dataWithBytes:length:` copies, so the Vec can drop.
        let data: *mut AnyObject = msg_send![
            class!(NSData),
            dataWithBytes: png.as_ptr() as *const c_void
            length: png.len()
        ];
        let image: *mut AnyObject = if data.is_null() {
            std::ptr::null_mut()
        } else {
            let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
            msg_send![image, initWithData: data]
        };
        let _: () = msg_send![image_view, setImage: image];
        panel.still_tag = (!image.is_null()).then_some((frame.target_pid, Some(window)));
        if !image.is_null() {
            let _: () = msg_send![image, release];
        }
    }
    set_text(panel.status, &frame.action_label);

    // Who: resolve the client icon + label only when the identity changes.
    let client = (
        frame.client_name.clone(),
        frame.client_pid,
        frame.session_label.clone(),
    );
    if panel.client.as_ref() != Some(&client) {
        let icon = client_icon(frame.client_name.as_deref(), frame.client_pid);
        let _: () = msg_send![panel.client_icon as *mut AnyObject, setImage: icon];
        let label = frame
            .session_label
            .as_deref()
            .or(frame.client_name.as_deref())
            .unwrap_or("agent");
        set_text(panel.client_label, label);
        panel.laid_out = (0.0, 0.0);
        apply_card_frames(panel);
        panel.client = Some(client);
    }

    if tag.is_some() {
        if let Some(front) = panel.cards.front_mut() {
            front.data.title = title;
            front.data.pid = frame.target_pid;
            front.data.status = frame.action_label.clone();
        }
    }
    if restacked {
        announce_stack(panel, &key);
    }
    set_panel_target(panel, &key, &worker, new_target);
    panel.target_visible = target_visible;
    panel.resolved_window = resolved_window;
    panel.dismissed = false;
    panel.last_frame = now;
    state.worker.mark_delivered(&key, panel.last_frame);
    refresh(state, &key);
    // Small slack so the monotonic check in the callback is past the bar.
    dispatch_to_main_after(
        IDLE_HIDE_AFTER + Duration::from_millis(20),
        key,
        idle_check_cb,
    );
}

unsafe extern "C" fn apply_verify_cb(ctx: *mut c_void) {
    let verification: PipVerification = *Box::from_raw(ctx as *mut PipVerification);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| apply_verify(state, verification));
    });
}

/// Record a finished `verify_state`: its claims for the checklist, and
/// whether its window is now finished (a finished back window collapses to
/// a chip).
unsafe fn apply_verify(state: &mut State, verification: PipVerification) {
    let PipVerification {
        timestamp_ms,
        session_key: key,
        target_pid,
        target_window_id: window,
        satisfied,
        claims,
    } = verification;
    let count = claims.len();
    let claims: Vec<Claim> = claims
        .into_iter()
        .map(|claim| Claim {
            label: claim.label,
            satisfied: claim.satisfied,
        })
        .collect();
    tracing::info!(target: "pip", session = %key, pid = target_pid, window, satisfied, claims = count, "PiP verification");
    let worker = state.worker.clone();
    // Delivered in main-queue order: a panel still here is the session's,
    // even if `end_session` already ended its epoch (its `end_session_cb`
    // is queued behind this).
    let Some(panel) = state.panels.get_mut(&key) else {
        // No panel: the session's first frame is still being captured (keep
        // it for the panel), or it never acted or has ended (drop it).
        if !worker.is_live(&key) {
            return;
        }
        state.early.entry(key).or_default().verify(
            target_pid,
            window,
            timestamp_ms,
            satisfied,
            claims,
        );
        return;
    };
    let restacked = restack(panel, &key, &worker, |panel| {
        panel
            .verdicts
            .verify(target_pid, window, timestamp_ms, satisfied, claims)
    });
    note_finished(panel, &key);
    if restacked {
        announce_stack(panel, &key);
    }
    // A verification that lands while the finale plays (verify_state can
    // outlast the idle timer) replays it with the new claims, so nothing is
    // shown stale and nothing is cleared unseen.
    if let Some(generation) = panel.finale.restart() {
        play_finale(panel, &key, &panel.verdicts.finale(), generation);
    } else if panel.finale.claims_arrived() {
        // It landed after the finale for this stretch was over: it gets a
        // finale of its own (unless the user closed the panel).
        refresh(state, &key);
    }
}

/// An action as it was pushed, before its capture.
struct Action {
    key: String,
    target: Target,
    timestamp_ms: u64,
}

unsafe extern "C" fn apply_action_cb(ctx: *mut c_void) {
    let action: Action = *Box::from_raw(ctx as *mut Action);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| apply_action(state, action));
    });
}

/// Record an action in the session's verdicts (a finished window it acts
/// in is no longer finished), whether or not its frame survives the
/// capture queue.
unsafe fn apply_action(state: &mut State, action: Action) {
    let Action {
        key,
        target,
        timestamp_ms,
    } = action;
    let worker = state.worker.clone();
    // Main-queue order, as for verifications.
    let Some(panel) = state.panels.get_mut(&key) else {
        if !worker.is_live(&key) {
            return;
        }
        state
            .early
            .entry(key)
            .or_default()
            .record_action(target, timestamp_ms);
        return;
    };
    // The session resumed: it is active from this action, not from when its
    // (possibly slow, possibly coalesced) capture lands. A playing finale
    // stops here, and the idle timer counts from now.
    if panel.finale.cancel() {
        tracing::info!(target: "pip", session = %key, "PiP finished state cancelled by a new action");
    }
    remove_finale_view(panel);
    let now = Instant::now();
    panel.last_frame = panel.last_frame.max(now);
    let restacked = restack(panel, &key, &worker, |panel| {
        panel.verdicts.record_action(target, timestamp_ms)
    });
    note_finished(panel, &key);
    if restacked {
        announce_stack(panel, &key);
    }
    worker.mark_delivered(&key, now);
    dispatch_to_main_after(
        IDLE_HIDE_AFTER + Duration::from_millis(20),
        key,
        idle_check_cb,
    );
}

/// Log each window of the panel (in its stack or touched since the last
/// finale) the first time it counts as finished.
unsafe fn note_finished(panel: &mut Panel, key: &str) {
    let windows: Vec<(u32, String)> = panel
        .cards
        .cards()
        .iter()
        .map(|card| (card.key, card.data.title.clone()))
        .chain(panel.verdicts.touched().cloned())
        .filter_map(|(tag, title)| tag.1.map(|window| (window, title)))
        .collect();
    let mut finished = HashSet::new();
    for (window, title) in windows {
        if !panel.verdicts.finished(window) || !finished.insert(window) {
            continue;
        }
        if !panel.finished_seen.contains(&window) {
            tracing::info!(target: "pip", session = %key, window, title = %title, "PiP window finished");
        }
    }
    panel.finished_seen = finished;
}

/// Show the target app's icon and `title` (else the app's name) in the
/// header. Returns the title shown.
unsafe fn show_target(panel: &Panel, pid: Option<i32>, title: Option<String>) -> String {
    let app: *mut AnyObject = match pid {
        Some(pid) => msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ],
        None => std::ptr::null_mut(),
    };
    let _: () = msg_send![panel.target_icon as *mut AnyObject, setImage: app_icon(app)];
    let title = title
        .filter(|title| !title.is_empty())
        .or_else(|| {
            if app.is_null() {
                return None;
            }
            let name: *mut AnyObject = msg_send![app, localizedName];
            ns_to_string(name)
        })
        .unwrap_or_default();
    set_text(panel.target_title, &title);
    title
}

/// A running app's icon, or null.
unsafe fn app_icon(app: *mut AnyObject) -> *mut AnyObject {
    if app.is_null() {
        return std::ptr::null_mut();
    }
    msg_send![app, icon]
}

/// Where each of the panel's items is drawn, from which windows are
/// finished (at least the front card).
fn item_slots(panel: &Panel) -> Vec<Slot> {
    let finished: Vec<bool> = panel
        .cards
        .cards()
        .iter()
        .map(|card| {
            card.key
                .1
                .is_some_and(|window| panel.verdicts.finished(window))
        })
        .collect();
    let slots = stack::slots(&finished);
    if slots.is_empty() {
        vec![Slot::Front]
    } else {
        slots
    }
}

/// Apply `change` to the panel (its card stack or its verdicts). If the
/// order or any item's slot changed, each item starts where its window was
/// drawn and springs to its new place (a card collapsing into a chip, a
/// chip coming forward), the back items show their windows, and the poll
/// watches their windows. Whether anything changed.
unsafe fn restack(
    panel: &mut Panel,
    key: &str,
    worker: &CaptureWorker,
    change: impl FnOnce(&mut Panel),
) -> bool {
    let old = panel.cards.keys();
    let old_layout = panel.layout.clone();
    let drawn = item_frames(panel);
    change(panel);
    let new = panel.cards.keys();
    let layout = item_slots(panel);
    if new == old && layout == old_layout {
        return false;
    }
    panel.layout = layout.clone();
    let cards = back_cards(&layout);
    for (index, from) in stack::previous_depths(&old, &new).into_iter().enumerate() {
        let Some(&slot) = layout.get(index) else {
            continue;
        };
        let view = card_view(panel, slot);
        let rest = slot_frame(panel.card, slot, cards);
        match from {
            Some(from) => {
                // Moved, or its place moved (chips shift when cards come
                // and go): spring from where it was drawn.
                if drawn.get(from).is_some_and(|drawn| *drawn != rest) {
                    panel.motion[slot.view()].restack(drawn[from], rest);
                }
                let before = old_layout.get(from).copied().unwrap_or(slot);
                if before != slot {
                    fade_view(view, before.alpha(), slot.alpha());
                }
            }
            None => {
                panel.motion[slot.view()] = Motion::default();
                let _: () = msg_send![view as *mut AnyObject, setAlphaValue: slot.alpha()];
            }
        }
    }
    render_backs(panel);
    apply_card_frames(panel);
    start_ticking();
    let windows = panel.cards.cards()[1.min(new.len())..]
        .iter()
        .filter_map(|card| card.key.1)
        .collect();
    worker.watch(key, windows);
    true
}

/// Make `tag` the front card: `acted` when the session acted in its window,
/// `None` for a user click (only a card already in the stack). The old
/// front takes its still behind (only if it is of its own window); a card
/// coming forward brings its still, which is the new target's pixels.
/// Whether the stack changed.
unsafe fn switch_front(
    panel: &mut Panel,
    key: &str,
    worker: &CaptureWorker,
    tag: Tag,
    acted: Option<Instant>,
) -> bool {
    if panel.cards.front_key() == Some(tag) {
        if let (Some(now), Some(front)) = (acted, panel.cards.front_mut()) {
            front.acted = now;
        }
        return false;
    }
    if let Some(front) = panel.cards.front_mut() {
        let image: *mut AnyObject = msg_send![panel.image_view as *mut AnyObject, image];
        if panel.still_tag == Some(front.key) && !image.is_null() {
            let _: *mut AnyObject = msg_send![image, retain];
            front.data.still = Some((front.key, Image(image as usize)));
        }
    }
    let changed = restack(panel, key, worker, |panel| match acted {
        Some(now) => panel.cards.act(tag, now),
        None => {
            panel.cards.raise(tag);
        }
    });
    if let Some(front) = panel.cards.front_mut() {
        if let Some((still_tag, image)) = front.data.still.take() {
            if still_tag == tag {
                let _: () = msg_send![
                    panel.image_view as *mut AnyObject,
                    setImage: image.0 as *mut AnyObject
                ];
                panel.still_tag = Some(still_tag);
            }
        }
    }
    changed
}

/// Log the stack and put its size in the panel's window title, for checks.
unsafe fn announce_stack(panel: &Panel, key: &str) {
    let titles: Vec<&str> = panel
        .cards
        .cards()
        .iter()
        .map(|card| card.data.title.as_str())
        .collect();
    let count = titles.len();
    let chips = panel
        .layout
        .iter()
        .filter(|slot| matches!(slot, Slot::Chip(_)))
        .count();
    tracing::info!(target: "pip", session = %key, cards = count, chips, ?titles, "PiP card stack changed");
    let title = format!(
        "cua PiP · {} · {count} card{}",
        panel.name,
        if count == 1 { "" } else { "s" }
    );
    let _: () = msg_send![panel.window as *mut AnyObject, setTitle: ns_string(&title)];
}

/// The user clicked a back card: bring it to the front and point the panel
/// (header, live stream, focus button, visibility) at its window until the
/// session's next frame. Never focuses the window itself.
unsafe fn raise_card(state: &mut State, id: i64, tag: Tag) {
    let worker = state.worker.clone();
    let live = state.panels.values().any(|panel| panel.id == id);
    let Some(panel) = panel_by_id(state, id) else {
        return;
    };
    let key = panel.key.clone();
    let key = key.as_str();
    if !switch_front(panel, key, &worker, tag, None) {
        return;
    }
    let (title, pid, status) = match panel.cards.cards().first() {
        Some(front) => (
            front.data.title.clone(),
            front.data.pid,
            front.data.status.clone(),
        ),
        None => return,
    };
    show_target(panel, pid, Some(title));
    set_text(panel.status, &status);
    announce_stack(panel, key);
    if live {
        set_panel_target(panel, key, &worker, tag);
    } else {
        // Ended: the poll (and any new session under this key) is not ours.
        panel.target = tag;
    }
    panel.resolved_window = tag.1;
    // Shown until the poll says otherwise for this window.
    panel.target_visible = false;
    // An ending panel has no live state to refresh (and its key may belong
    // to a new session's panel by now).
    if live {
        refresh(state, key);
    }
}

/// The only place the panel's displayed target changes: the visibility
/// poll is pointed at the same target in the same step, so its answers are
/// never rejected as being about another target.
fn set_panel_target(panel: &mut Panel, key: &str, worker: &CaptureWorker, target: Target) {
    panel.target = target;
    worker.retarget(key, target);
}

unsafe extern "C" fn idle_check_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| refresh(state, &key));
}

unsafe extern "C" fn visibility_cb(ctx: *mut c_void) {
    let update: VisibilityUpdate = *Box::from_raw(ctx as *mut VisibilityUpdate);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| {
            let worker = state.worker.clone();
            let key = update.key.as_str();
            let Some(panel) = state.panels.get_mut(key) else {
                return;
            };
            // Back cards whose window closed or went quiet drop.
            let gone = &update.gone;
            let mut restacked = restack(panel, key, &worker, |panel| {
                panel.cards.prune(Instant::now(), |tag| {
                    tag.1.is_some_and(|window| gone.contains(&window))
                });
            });
            // An answer about an older target, or no change: nothing more.
            if panel.target != update.target
                || (panel.target_visible == update.visible
                    && panel.resolved_window == update.resolved_window)
            {
                if restacked {
                    announce_stack(panel, key);
                }
                return;
            }
            panel.target_visible = update.visible;
            panel.resolved_window = update.resolved_window;
            // A pid-only target that now resolves to a (new) window: that
            // window is the front card, labelled as the header already is.
            if let Some(tag) = current_tag(panel.target, panel.resolved_window) {
                if switch_front(panel, key, &worker, tag, Some(panel.last_frame)) {
                    let title =
                        ns_to_string(msg_send![panel.target_title as *mut AnyObject, stringValue]);
                    let status =
                        ns_to_string(msg_send![panel.status as *mut AnyObject, stringValue]);
                    if let Some(front) = panel.cards.front_mut() {
                        front.data.title = title.unwrap_or_default();
                        front.data.pid = tag.0;
                        front.data.status = status.unwrap_or_default();
                    }
                    restacked = true;
                }
            }
            if restacked {
                announce_stack(panel, key);
            }
            refresh(state, key);
        });
    });
}

unsafe extern "C" fn live_event_cb(ctx: *mut c_void) {
    let event: Event = *Box::from_raw(ctx as *mut Event);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| match event {
            Event::Frame {
                key,
                generation,
                slot,
            } => {
                let Some(panel) = state.panels.get_mut(&key) else {
                    return;
                };
                if !panel.stream.accepts(generation) {
                    // At most once per ended stream: its slot stays full, so
                    // it sends no further events.
                    tracing::info!(target: "pip", session = %key, generation, current = panel.stream.generation(), "PiP live frame from an ended stream dropped");
                    return;
                }
                panel.stream.frames.events += 1;
                // Tags before `show_live`, which drops a mismatched frame.
                let current = current_tag(panel.target, panel.resolved_window);
                let live = panel.stream.requested;
                let outcome = match lock(&slot).take() {
                    Some(frame) => Some(show_live(panel, frame)),
                    None => None,
                };
                let frames = &mut panel.stream.frames;
                match outcome {
                    None => frames.empty_slot += 1,
                    Some(LiveShown::NoSurface) => {
                        frames.no_iosurface += 1;
                        if frames.no_iosurface == 1 {
                            tracing::info!(target: "pip", session = %key, generation, "PiP live frame has no IOSurface");
                        }
                    }
                    Some(LiveShown::Hidden) => {
                        frames.hidden_by_tags += 1;
                        if frames.hidden_by_tags == 1 {
                            tracing::info!(target: "pip", session = %key, generation, ?current, ?live, still = ?panel.still_tag, "PiP live frame hidden: its tag is not the current target");
                        }
                    }
                    Some(LiveShown::Shown) => {
                        frames.shown += 1;
                        if frames.shown == 1 {
                            tracing::info!(target: "pip", session = %key, generation, shown = panel.shown, "PiP live frame shown");
                        }
                    }
                }
                if panel.stream.frames.summary_due(Instant::now()) {
                    tracing::info!(target: "pip", session = %key, generation, shown = panel.shown, frames = ?panel.stream.frames, "PiP live frames on the panel");
                }
            }
            Event::Ended { key, generation } => {
                let Some(panel) = state.panels.get_mut(&key) else {
                    return;
                };
                // Also invalidates the generation, so a frame still in
                // flight from this stream cannot bring the live layer back.
                // `panel.stream.requested` stays set, so this target is not
                // retried until it changes or the panel hides and shows
                // again; the Stop only releases the stream.
                let current = panel.stream.end(generation);
                tracing::info!(target: "pip", session = %key, generation, current, frames = ?panel.stream.frames, "PiP stream ended");
                if !current {
                    return;
                }
                clear_live(panel);
                state.streams.request(&key, Request::Stop);
            }
        });
    });
}

/// Bring a panel's visibility and live stream in line with its state.
unsafe fn refresh(state: &mut State, key: &str) {
    let State {
        panels,
        ending,
        streams,
        next_stream_generation,
        image_size,
        anchor,
        worker,
        ..
    } = state;
    // An ended session's panel keeps its slot while its finale is up.
    let others: Vec<usize> = panels
        .iter()
        .filter(|(other, _)| other.as_str() != key)
        .map(|(_, panel)| panel)
        .chain(ending.iter())
        .filter_map(|panel| panel.slot)
        .collect();
    let Some(panel) = panels.get_mut(key) else {
        return;
    };
    let active = !idle_hide_due(panel.last_frame, Instant::now());
    // Gone idle after acting: the session finished. The finale plays if the
    // panel is up; the panel stays up for it whatever else happens.
    if panel.finale.due(active) {
        finish_session(panel, key, worker);
    }
    let finale = panel.finale.playing();
    if panel_should_show(
        active || finale,
        panel.dismissed,
        panel.target_visible && !finale,
    ) {
        if !panel.shown {
            place_on_show(panel, others, *image_size, *anchor);
        }
        show(panel);
    } else {
        // A hidden panel releases its cascade slot.
        panel.slot = None;
        hide(panel, key);
    }
    match live::stream_step(
        panel.shown,
        panel.target,
        panel.resolved_window,
        panel.stream.requested,
    ) {
        StreamStep::Start(target) => {
            *next_stream_generation += 1;
            panel
                .stream
                .begin(target, *next_stream_generation, panel.stream_well);
            tracing::info!(target: "pip", session = %key, generation = *next_stream_generation, ?target, "PiP stream requested");
            // Never show one window's live pixels as another's preview.
            clear_live(panel);
            streams.request(
                key,
                Request::Start {
                    generation: *next_stream_generation,
                    target,
                    well: panel.stream_well,
                },
            );
        }
        StreamStep::Stop => {
            // The last live frame stays up while the panel fades out.
            tracing::info!(target: "pip", session = %key, generation = panel.stream.generation(), shown = panel.shown, frames = ?panel.stream.frames, "PiP stream release requested");
            panel.stream.stop();
            streams.request(key, Request::Stop);
            // A shown panel only stops for a new target it cannot stream:
            // its old live frame must not sit under the new label.
            if panel.shown {
                clear_live(panel);
            }
        }
        StreamStep::Keep => {
            if panel.stream.needs_resize(panel.stream_well) {
                streams.request(
                    key,
                    Request::Resize {
                        well: panel.stream_well,
                    },
                );
            }
        }
    }
    sync_layers(panel);
}

// ── Finished state ────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The session finished (idle after acting, or ended): its windows count as
/// finished, so back cards collapse into chips, and if the panel is up the
/// finale plays, ending on its own timer.
unsafe fn finish_session(panel: &mut Panel, key: &str, worker: &CaptureWorker) {
    let at = now_ms();
    if restack(panel, key, worker, |panel| {
        panel.verdicts.finish_session(at)
    }) {
        announce_stack(panel, key);
    }
    note_finished(panel, key);
    let finale = panel.verdicts.finale();
    // Late claims (a verification that finished after the last finale) are
    // shown even if that finale's fade hid the panel, wherever the panel
    // would be allowed to show; a user's close is always respected.
    let late = panel.finale.late();
    let visible =
        !panel.dismissed && finale.len() > 0 && (panel.shown || (late && !panel.target_visible));
    let Some(generation) = panel.finale.start(visible) else {
        return;
    };
    play_finale(panel, key, &finale, generation);
}

/// Show `finale` (from the top) and time its end under `generation`.
unsafe fn play_finale(panel: &mut Panel, key: &str, finale: &Finale, generation: u64) {
    tracing::info!(target: "pip", session = %key, rows = ?finale.log_rows(), kind = %finale.kind(), "PiP finished state");
    show_finale_view(panel, finale);
    dispatch_to_main_after(
        finish::finale_duration(finale.len()),
        (panel.id, generation),
        finale_end_cb,
    );
}

/// A finale's time is up: an ended session's panel closes; a live one
/// fades (unless a new action already cancelled the finale).
unsafe extern "C" fn finale_end_cb(ctx: *mut c_void) {
    let (id, generation) = *Box::from_raw(ctx as *mut (i64, u64));
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| {
            if let Some(index) = state.ending.iter().position(|panel| panel.id == id) {
                // Only the finale that is playing closes it: an idle finale
                // cancelled by an action, then a new one started at session
                // end, leaves a stale timer behind.
                if state.ending[index].finale.end(generation) {
                    close_panel(state.ending.remove(index));
                }
                return;
            }
            let key = state
                .panels
                .iter_mut()
                .find(|(_, panel)| panel.id == id)
                .and_then(|(key, panel)| {
                    panel.finale.end(generation).then(|| {
                        panel.verdicts.finale_shown();
                        key.clone()
                    })
                });
            if let Some(key) = key {
                refresh(state, &key);
            }
        });
    });
}

/// Take the finale overlay off the front card.
unsafe fn remove_finale_view(panel: &mut Panel) {
    if let Some(view) = panel.finale_view.take() {
        let _: () = msg_send![view as *mut AnyObject, removeFromSuperview];
    }
}

/// Height of a checklist row.
const ROW_HEIGHT: f64 = 22.0;
/// Size of a checklist mark, and of a finale chip's app icon.
const MARK_SIZE: f64 = 15.0;
const FINALE_ICON: f64 = 28.0;

/// Put the finale over the front card's image well: a dark scrim with the
/// checklist rows (or the touched windows' chips) animating in one by one.
unsafe fn show_finale_view(panel: &mut Panel, finale: &Finale) {
    remove_finale_view(panel);
    let (well_w, well_h) = well_size(panel.card);
    let overlay = new_view(
        class!(NSView),
        ns_rect(Area {
            x: PAD,
            y: 6.0 + STATUS_HEIGHT + 4.0,
            w: well_w,
            h: well_h,
        }),
    );
    let _: () = msg_send![overlay, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![overlay, layer];
    let scrim: *mut AnyObject = msg_send![
        class!(NSColor),
        colorWithSRGBRed: 0.0_f64
        green: 0.0_f64
        blue: 0.0_f64
        alpha: 0.66_f64
    ];
    let scrim: *mut CGColor = msg_send![scrim, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: scrim];
    let _: () = msg_send![layer, setCornerRadius: 8.0_f64];
    let _: () = msg_send![overlay, setAutoresizingMask: 18u64];
    let white: *mut AnyObject = msg_send![class!(NSColor), whiteColor];
    let start = CACurrentMediaTime();
    match finale {
        Finale::Checklist(rows) => {
            let top = (well_h + rows.len() as f64 * ROW_HEIGHT) / 2.0;
            for (index, row) in rows.iter().enumerate() {
                let view = new_view(
                    class!(NSView),
                    ns_rect(Area {
                        x: 0.0,
                        y: top - (index + 1) as f64 * ROW_HEIGHT,
                        w: well_w,
                        h: ROW_HEIGHT,
                    }),
                );
                let _: () = msg_send![view, setWantsLayer: true];
                // Width sizable, flexible top and bottom: stays centered.
                let _: () = msg_send![view, setAutoresizingMask: 2u64 | 8 | 32];
                let kind = match row.satisfied {
                    Some(true) => Mark::Check,
                    Some(false) => Mark::Warning,
                    None => Mark::Unknown,
                };
                let (mark, glyph) = new_mark(MARK_SIZE, kind);
                let _: () = msg_send![
                    mark,
                    setFrame: NSRect::new(
                        NSPoint::new(12.0, (ROW_HEIGHT - MARK_SIZE) / 2.0),
                        NSSize::new(MARK_SIZE, MARK_SIZE)
                    )
                ];
                let label = new_label(
                    NSRect::new(
                        NSPoint::new(12.0 + MARK_SIZE + 8.0, (ROW_HEIGHT - 16.0) / 2.0),
                        NSSize::new((well_w - 44.0).max(0.0), 16.0),
                    ),
                    12.0,
                    0.23,
                    false,
                );
                let _: () = msg_send![label, setTextColor: white];
                let _: () = msg_send![label, setAutoresizingMask: 2u64];
                set_text(label as usize, &row.label);
                let _: () = msg_send![view, addSubview: label];
                let view_layer: *mut AnyObject = msg_send![view, layer];
                let _: () = msg_send![view_layer, addSublayer: mark];
                animate_row(view_layer, mark, glyph, index, start, (-8.0, 0.0));
                add_subview(overlay, view);
            }
        }
        Finale::Chips(chips) => {
            let gap = 10.0;
            let count = chips.len() as f64;
            let total = count * FINALE_ICON + (count - 1.0).max(0.0) * gap;
            let (x0, y) = ((well_w - total) / 2.0, (well_h - FINALE_ICON) / 2.0);
            for (index, chip) in chips.iter().enumerate() {
                let view = new_view(
                    class!(NSView),
                    ns_rect(Area {
                        x: x0 + index as f64 * (FINALE_ICON + gap),
                        y,
                        w: FINALE_ICON,
                        h: FINALE_ICON,
                    }),
                );
                let _: () = msg_send![view, setWantsLayer: true];
                // Flexible margins: stays centered.
                let _: () = msg_send![view, setAutoresizingMask: 1u64 | 4 | 8 | 32];
                let _: () = msg_send![view as *mut AnyObject, setToolTip: ns_string(&chip.title)];
                let icon = new_icon_view(NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(FINALE_ICON, FINALE_ICON),
                ));
                let app: *mut AnyObject = match chip.tag.0 {
                    Some(pid) => msg_send![
                        class!(NSRunningApplication),
                        runningApplicationWithProcessIdentifier: pid
                    ],
                    None => std::ptr::null_mut(),
                };
                let _: () = msg_send![icon, setImage: app_icon(app)];
                let _: () = msg_send![view, addSubview: icon];
                let view_layer: *mut AnyObject = msg_send![view, layer];
                // A check only on a finished window's chip.
                let (mark, glyph) = if chip.finished {
                    let size = 13.0;
                    let (mark, glyph) = new_mark(size, Mark::Check);
                    let _: () = msg_send![
                        mark,
                        setFrame: NSRect::new(
                            NSPoint::new(FINALE_ICON - size + 3.0, -3.0),
                            NSSize::new(size, size)
                        )
                    ];
                    let _: () = msg_send![view_layer, addSublayer: mark];
                    (mark, glyph)
                } else {
                    (std::ptr::null_mut(), std::ptr::null_mut())
                };
                animate_row(view_layer, mark, glyph, index, start, (0.0, -6.0));
                add_subview(overlay, view);
            }
        }
    }
    let _: () = msg_send![panel.front_view as *mut AnyObject, addSubview: overlay];
    let _: () = msg_send![overlay, release];
    panel.finale_view = Some(overlay as usize);
}

/// A mark's look: a check on green (satisfied, finished), a cross on orange
/// (unsatisfied), a dash on gray (unknown). Only `Check` is ever green.
#[derive(Clone, Copy)]
enum Mark {
    Check,
    Warning,
    Unknown,
}

/// A round mark `size` points wide: a filled circle layer with its glyph
/// stroked on a shape layer (returned too, to draw it in). Autoreleased.
unsafe fn new_mark(size: f64, kind: Mark) -> (*mut AnyObject, *mut AnyObject) {
    let fill: *mut AnyObject = match kind {
        Mark::Check => msg_send![class!(NSColor), systemGreenColor],
        Mark::Warning => msg_send![class!(NSColor), systemOrangeColor],
        Mark::Unknown => msg_send![class!(NSColor), systemGrayColor],
    };
    let fill: *mut CGColor = msg_send![fill, CGColor];
    let white: *mut AnyObject = msg_send![class!(NSColor), whiteColor];
    let white: *mut CGColor = msg_send![white, CGColor];
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let clear: *mut CGColor = msg_send![clear, CGColor];
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size));
    let circle: *mut AnyObject = msg_send![class!(CALayer), layer];
    let _: () = msg_send![circle, setFrame: bounds];
    let _: () = msg_send![circle, setCornerRadius: size / 2.0];
    let _: () = msg_send![circle, setBackgroundColor: fill];
    // Unit-square strokes (y up, like the layer).
    let strokes: &[&[(f64, f64)]] = match kind {
        Mark::Check => &[&[(0.28, 0.52), (0.44, 0.36), (0.73, 0.66)]],
        Mark::Warning => &[&[(0.34, 0.34), (0.66, 0.66)], &[(0.34, 0.66), (0.66, 0.34)]],
        Mark::Unknown => &[&[(0.3, 0.5), (0.7, 0.5)]],
    };
    let path = CGPathCreateMutable();
    for stroke in strokes {
        for (index, &(x, y)) in stroke.iter().enumerate() {
            if index == 0 {
                CGPathMoveToPoint(path, std::ptr::null(), x * size, y * size);
            } else {
                CGPathAddLineToPoint(path, std::ptr::null(), x * size, y * size);
            }
        }
    }
    let glyph: *mut AnyObject = msg_send![class!(CAShapeLayer), layer];
    let _: () = msg_send![glyph, setFrame: bounds];
    let _: () = msg_send![glyph, setPath: path as *const CGPath];
    CGPathRelease(path);
    let _: () = msg_send![glyph, setStrokeColor: white];
    let _: () = msg_send![glyph, setFillColor: clear];
    let _: () = msg_send![glyph, setLineWidth: (size * 0.13).max(1.5)];
    let _: () = msg_send![glyph, setLineCap: ns_string("round")];
    let _: () = msg_send![glyph, setLineJoin: ns_string("round")];
    let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
    let scale: f64 = if screen.is_null() {
        2.0
    } else {
        msg_send![screen, backingScaleFactor]
    };
    let _: () = msg_send![glyph, setContentsScale: scale];
    let _: () = msg_send![circle, setContentsScale: scale];
    let _: () = msg_send![circle, addSublayer: glyph];
    (circle, glyph)
}

/// Animate finale row `index` in (see `finish::row_timing`): the row fades
/// and slides from `slide` over `ROW_IN`, then its mark pops (a spring from
/// 30% scale) while its glyph draws. `mark`/`glyph` may be null.
unsafe fn animate_row(
    row: *mut AnyObject,
    mark: *mut AnyObject,
    glyph: *mut AnyObject,
    index: usize,
    start: f64,
    slide: (f64, f64),
) {
    let (appear, mark_at) = finish::row_timing(index);
    let (appear, mark_at) = (start + appear.as_secs_f64(), start + mark_at.as_secs_f64());
    let row_in = finish::ROW_IN.as_secs_f64();
    add_animation(row, "opacity", 0.0, 1.0, appear, row_in);
    if slide.0 != 0.0 {
        add_animation(row, "transform.translation.x", slide.0, 0.0, appear, row_in);
    }
    if slide.1 != 0.0 {
        add_animation(row, "transform.translation.y", slide.1, 0.0, appear, row_in);
    }
    if mark.is_null() {
        return;
    }
    add_animation(mark, "opacity", 0.0, 1.0, mark_at, 0.06);
    let pop: *mut AnyObject = msg_send![
        class!(CASpringAnimation),
        animationWithKeyPath: ns_string("transform.scale")
    ];
    let _: () = msg_send![pop, setMass: 1.0_f64];
    let _: () = msg_send![pop, setStiffness: 320.0_f64];
    let _: () = msg_send![pop, setDamping: 16.0_f64];
    let settle: f64 = msg_send![pop, settlingDuration];
    configure_animation(pop, 0.3, 1.0, mark_at, settle);
    let _: () = msg_send![mark, addAnimation: pop forKey: ns_string("pop")];
    if !glyph.is_null() {
        add_animation(
            glyph,
            "strokeEnd",
            0.0,
            1.0,
            mark_at,
            finish::MARK_IN.as_secs_f64(),
        );
    }
}

/// An ease-out animation of `key_path` from `from` to `to`, starting at
/// media time `begin`, showing `from` until then (fill backwards).
unsafe fn add_animation(
    layer: *mut AnyObject,
    key_path: &str,
    from: f64,
    to: f64,
    begin: f64,
    duration: f64,
) {
    let animation: *mut AnyObject = msg_send![
        class!(CABasicAnimation),
        animationWithKeyPath: ns_string(key_path)
    ];
    let ease: *mut AnyObject = msg_send![
        class!(CAMediaTimingFunction),
        functionWithName: ns_string("easeOut")
    ];
    let _: () = msg_send![animation, setTimingFunction: ease];
    configure_animation(animation, from, to, begin, duration);
    let _: () = msg_send![layer, addAnimation: animation forKey: ns_string(key_path)];
}

unsafe fn configure_animation(
    animation: *mut AnyObject,
    from: f64,
    to: f64,
    begin: f64,
    duration: f64,
) {
    let from: *mut AnyObject = msg_send![class!(NSNumber), numberWithDouble: from];
    let to: *mut AnyObject = msg_send![class!(NSNumber), numberWithDouble: to];
    let _: () = msg_send![animation, setFromValue: from];
    let _: () = msg_send![animation, setToValue: to];
    let _: () = msg_send![animation, setBeginTime: begin];
    let _: () = msg_send![animation, setDuration: duration];
    let _: () = msg_send![animation, setFillMode: ns_string("backwards")];
}

/// Whether the window sits somewhere other than where we placed it.
fn moved_from(here: (f64, f64), placed: (f64, f64)) -> bool {
    (here.0 - placed.0).abs() > 0.5 || (here.1 - placed.1).abs() > 0.5
}

/// Called as a hidden panel is about to be shown: take the lowest free
/// cascade slot and move there, unless the user dragged the panel.
unsafe fn place_on_show(
    panel: &mut Panel,
    others: Vec<usize>,
    image_size: (f64, f64),
    anchor: Option<(i32, i32)>,
) {
    let window = panel.window as *mut AnyObject;
    let frame: NSRect = msg_send![window, frame];
    panel.dragged |= moved_from((frame.origin.x, frame.origin.y), panel.placed);
    panel.slot = slot_on_show(others, panel.dragged);
    let Some(slot) = panel.slot else {
        return;
    };
    let Some(origin) = slot_origin(window_size(panel_size(image_size)), anchor, slot) else {
        return;
    };
    let _: () = msg_send![window, setFrameOrigin: NSPoint::new(origin.0, origin.1)];
    panel.placed = origin;
}

/// What happened to a live frame handed to the panel.
enum LiveShown {
    /// On the live layer and visible.
    Shown,
    /// Dropped: its tag is not the panel's current target.
    Hidden,
    /// The pixel buffer had no IOSurface to show.
    NoSurface,
}

/// Put a live frame's IOSurface on the live layer, tagged with the target
/// of the stream it came from.
unsafe fn show_live(panel: &mut Panel, frame: screencapturekit::CVPixelBuffer) -> LiveShown {
    let Some(surface) = frame.io_surface() else {
        return LiveShown::NoSurface;
    };
    let _: () = msg_send![class!(CATransaction), begin];
    // No implicit cross-fade between frames.
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![
        panel.live_layer as *mut AnyObject,
        setContents: surface.as_ptr() as *mut AnyObject
    ];
    let _: () = msg_send![class!(CATransaction), commit];
    // Replacing the previous frame releases it back to ScreenCaptureKit.
    panel.live_frame = Some(frame);
    panel.live_tag = panel.stream.requested;
    if sync_layers(panel).show_live {
        LiveShown::Shown
    } else {
        LiveShown::Hidden
    }
}

/// Release the live frame and blank the live layer.
unsafe fn drop_live(panel: &mut Panel) {
    panel.live_tag = None;
    let Some(frame) = panel.live_frame.take() else {
        return;
    };
    let _: () = msg_send![
        panel.live_layer as *mut AnyObject,
        setContents: std::ptr::null_mut::<AnyObject>()
    ];
    drop(frame);
}

/// Remove the live frame so the still screenshot (if current) shows again.
unsafe fn clear_live(panel: &mut Panel) {
    drop_live(panel);
    sync_layers(panel);
}

/// The one place layer visibility is decided: drop pixels that are not of
/// the panel's current resolved target, then show what `visible_layers` says.
/// Call after any change to the target, the still, or the live frame.
unsafe fn sync_layers(panel: &mut Panel) -> Layers {
    let current = current_tag(panel.target, panel.resolved_window);
    if panel.live_tag.is_some() && panel.live_tag != current {
        drop_live(panel);
    }
    if panel.still_tag.is_some() && panel.still_tag != current {
        let _: () = msg_send![
            panel.image_view as *mut AnyObject,
            setImage: std::ptr::null_mut::<AnyObject>()
        ];
        panel.still_tag = None;
    }
    let layers = visible_layers(current, panel.still_tag, panel.live_tag);
    // The image view stays up (its tint is the well) unless live covers it.
    let _: () = msg_send![panel.image_view as *mut AnyObject, setHidden: layers.show_live];
    let _: () = msg_send![panel.live_view as *mut AnyObject, setHidden: !layers.show_live];
    let _: () = msg_send![
        panel.placeholder as *mut AnyObject,
        setHidden: !layers.show_placeholder
    ];
    layers
}

/// Fade completion: order the panel out unless a frame re-showed it.
unsafe extern "C" fn order_out_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| {
        if let Some(panel) = state.panels.get_mut(&key) {
            if !panel.shown {
                for window in [panel.window, panel.trail.window] {
                    let _: () = msg_send![
                        window as *mut AnyObject,
                        orderOut: std::ptr::null_mut::<AnyObject>()
                    ];
                }
                // The fade is over: release the retained live frame.
                clear_live(panel);
                remove_finale_view(panel);
            }
        }
    });
}

unsafe extern "C" fn end_session_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| {
        state.early.remove(&key);
        let Some(mut panel) = state.panels.remove(&key) else {
            return;
        };
        state.streams.request(&key, Request::Stop);
        let frame: NSRect = msg_send![panel.window as *mut AnyObject, frame];
        let here = (frame.origin.x, frame.origin.y);
        let remembered = Remembered {
            origin: (panel.dragged || moved_from(here, panel.placed)).then_some(here),
            card: panel.resized.then_some(panel.card),
        };
        if remembered.origin.is_some() || remembered.card.is_some() {
            state.remembered.insert(key.clone(), remembered);
        }
        // Ending is finishing: the finale plays (or keeps playing) if the
        // panel is up, and the panel closes when it is over.
        if panel.finale.due(false) {
            finish_session(&mut panel, &key, &state.worker);
        }
        if panel.finale.playing() {
            // Owed late claims can bring a hidden panel back for its finale.
            if !panel.shown {
                let others: Vec<usize> = state
                    .panels
                    .values()
                    .chain(state.ending.iter())
                    .filter_map(|panel| panel.slot)
                    .collect();
                place_on_show(&mut panel, others, state.image_size, state.anchor);
                show(&mut panel);
            }
            state.ending.push(panel);
        } else {
            close_panel(panel);
        }
    });
}

/// Fade the panel and its trail out, then close both.
unsafe fn close_panel(panel: Panel) {
    animate_alpha(panel.window, 0.0);
    animate_alpha(panel.trail.window, 0.0);
    dispatch_to_main_after(FADE, (panel.window, panel.trail.window), close_window_cb);
}

unsafe extern "C" fn close_window_cb(ctx: *mut c_void) {
    let (window, trail) = *Box::from_raw(ctx as *mut (usize, usize));
    close_window(window as *mut AnyObject);
    close_window(trail as *mut AnyObject);
}

unsafe extern "C" fn shutdown_cb(_ctx: *mut c_void) {
    let panels = with_state(|state| {
        for key in state.panels.keys() {
            state.streams.request(key, Request::Stop);
        }
        let mut panels: Vec<Panel> = std::mem::take(&mut state.panels).into_values().collect();
        panels.append(&mut state.ending);
        panels
    })
    .unwrap_or_default();
    for panel in panels {
        close_window(panel.window as *mut AnyObject);
        close_window(panel.trail.window as *mut AnyObject);
    }
}

unsafe fn close_window(window: *mut AnyObject) {
    let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
    let _: () = msg_send![window, close];
    // Balances the alloc in `create_panel` (releasedWhenClosed is NO).
    let _: () = msg_send![window, release];
}

// ── Show / hide ───────────────────────────────────────────────────────────

unsafe fn show(panel: &mut Panel) {
    if panel.shown {
        return;
    }
    panel.shown = true;
    let window = panel.window as *mut AnyObject;
    let visible: bool = msg_send![window, isVisible];
    if !visible {
        let _: () = msg_send![window, setAlphaValue: 0.0_f64];
    }
    // Never makeKey: the user's app keeps keyboard focus.
    let _: () = msg_send![window, orderFrontRegardless];
    animate_alpha(panel.window, 1.0);
    // The trail sits just below its panel, at rest.
    panel.trail_motion.snap();
    sync_trail(panel);
    let trail = panel.trail.window as *mut AnyObject;
    let trail_visible: bool = msg_send![trail, isVisible];
    if !trail_visible {
        let _: () = msg_send![trail, setAlphaValue: 0.0_f64];
    }
    let number: isize = msg_send![window, windowNumber];
    // NSWindowBelow
    let _: () = msg_send![trail, orderWindow: -1isize relativeTo: number];
    animate_alpha(panel.trail.window, 1.0);
}

unsafe fn hide(panel: &mut Panel, key: &str) {
    if !panel.shown {
        return;
    }
    panel.shown = false;
    animate_alpha(panel.window, 0.0);
    animate_alpha(panel.trail.window, 0.0);
    dispatch_to_main_after(FADE, key.to_owned(), order_out_cb);
}

unsafe fn animate_alpha(window: usize, alpha: f64) {
    let _: () = msg_send![class!(NSAnimationContext), beginGrouping];
    let context: *mut AnyObject = msg_send![class!(NSAnimationContext), currentContext];
    let _: () = msg_send![context, setDuration: FADE.as_secs_f64()];
    let animator: *mut AnyObject = msg_send![window as *mut AnyObject, animator];
    let _: () = msg_send![animator, setAlphaValue: alpha];
    let _: () = msg_send![class!(NSAnimationContext), endGrouping];
}

// ── Header buttons and ObjC classes ───────────────────────────────────────

extern "C" fn on_focus(_this: *mut AnyObject, _cmd: Sel, sender: *mut AnyObject) {
    let id: i64 = unsafe { msg_send![sender, tag] };
    let target = with_state(|state| panel_by_id(state, id).map(|panel| panel.target)).flatten();
    let Some((Some(pid), window_id)) = target else {
        return;
    };
    // bring_to_front polls for up to ~1 s; keep the UI thread free.
    std::thread::spawn(move || {
        let mut args = serde_json::json!({ "pid": pid });
        if let Some(window_id) = window_id {
            args["window_id"] = window_id.into();
        }
        let result = crate::tools::bring_to_front::bring_to_front_blocking(args);
        if result.is_error == Some(true) {
            tracing::info!(target: "pip", pid, ?window_id, "PiP focus was not verified");
        }
    });
}

extern "C" fn on_hide(_this: *mut AnyObject, _cmd: Sel, sender: *mut AnyObject) {
    let id: i64 = unsafe { msg_send![sender, tag] };
    with_state(|state| {
        // An ended session's panel playing its finale: close it now.
        if let Some(index) = state.ending.iter().position(|panel| panel.id == id) {
            unsafe { close_panel(state.ending.remove(index)) };
            return;
        }
        let Some(key) = state
            .panels
            .iter_mut()
            .find(|(_, panel)| panel.id == id)
            .map(|(key, panel)| {
                panel.dismissed = true;
                key.clone()
            })
        else {
            return;
        };
        unsafe { refresh(state, &key) };
    });
}

extern "C" fn returns_no(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    Bool::NO
}

extern "C" fn accepts_first_mouse(
    _this: *mut AnyObject,
    _cmd: Sel,
    _event: *mut AnyObject,
) -> Bool {
    Bool::YES
}

/// Register an Objective-C class; callers memoise (one registration per
/// process).
fn register_class(
    name: &str,
    superclass: &AnyClass,
    add: impl FnOnce(&mut objc2::declare::ClassBuilder),
) -> &'static AnyClass {
    let mut builder = objc2::declare::ClassBuilder::new(name, superclass)
        .unwrap_or_else(|| panic!("{name} already registered"));
    add(&mut builder);
    builder.register()
}

/// `NSPanel` that can never become key or main.
fn panel_class() -> &'static AnyClass {
    static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
    CLASS.get_or_init(|| {
        register_class("CuaPipPanel", class!(NSPanel), |builder| unsafe {
            builder.add_method(
                sel!(canBecomeKeyWindow),
                returns_no as extern "C" fn(_, _) -> _,
            );
            builder.add_method(
                sel!(canBecomeMainWindow),
                returns_no as extern "C" fn(_, _) -> _,
            );
        })
    })
}

/// `NSButton` that acts on the first click into a non-key panel.
fn button_class() -> &'static AnyClass {
    static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
    CLASS.get_or_init(|| {
        register_class("CuaPipButton", class!(NSButton), |builder| unsafe {
            builder.add_method(
                sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> _,
            );
        })
    })
}

/// Shared target object for every panel's header buttons (lives forever).
fn button_target() -> usize {
    static TARGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TARGET.get_or_init(|| {
        let class = register_class("CuaPipTarget", class!(NSObject), |builder| unsafe {
            builder.add_method(sel!(pipFocus:), on_focus as extern "C" fn(_, _, _));
            builder.add_method(sel!(pipHide:), on_hide as extern "C" fn(_, _, _));
        });
        let target: *mut AnyObject = unsafe { msg_send![class, new] };
        target as usize
    })
}

// ── Panel construction ────────────────────────────────────────────────────

/// Height of the front card around its image well: header, gaps, status.
const CARD_CHROME_HEIGHT: f64 = HEADER_HEIGHT + 4.0 + 4.0 + STATUS_HEIGHT + 6.0;

/// Front card size in points for an image well of `image_size`.
fn panel_size((image_w, image_h): (f64, f64)) -> (f64, f64) {
    (image_w + 2.0 * PAD, image_h + CARD_CHROME_HEIGHT)
}

/// Image well size of a front card of `card` size (inverse of `panel_size`).
fn well_size((card_w, card_h): (f64, f64)) -> (f64, f64) {
    (
        (card_w - 2.0 * PAD).max(1.0),
        (card_h - CARD_CHROME_HEIGHT).max(1.0),
    )
}

fn area_of(rect: NSRect) -> Area {
    Area {
        x: rect.origin.x,
        y: rect.origin.y,
        w: rect.size.width,
        h: rect.size.height,
    }
}

fn ns_rect(area: Area) -> NSRect {
    NSRect::new(NSPoint::new(area.x, area.y), NSSize::new(area.w, area.h))
}

unsafe fn set_frame(view: usize, area: Area) {
    let _: () = msg_send![view as *mut AnyObject, setFrame: ns_rect(area)];
}

/// AppKit origin of cascade `slot` on the main screen for a panel window of
/// `size`; `None` when there is no screen (headless).
unsafe fn slot_origin(
    size: (f64, f64),
    anchor: Option<(i32, i32)>,
    slot: usize,
) -> Option<(f64, f64)> {
    let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
    if screen.is_null() {
        return None;
    }
    let screen_frame: NSRect = msg_send![screen, frame];
    let visible_frame: NSRect = msg_send![screen, visibleFrame];
    Some(panel_origin(
        area_of(screen_frame),
        area_of(visible_frame),
        size,
        anchor,
        slot,
    ))
}

/// The visible frame of the screen `window` is on (else the main screen's).
unsafe fn visible_frame_of(window: *mut AnyObject) -> Option<Area> {
    let mut screen: *mut AnyObject = msg_send![window, screen];
    if screen.is_null() {
        screen = msg_send![class!(NSScreen), mainScreen];
    }
    if screen.is_null() {
        return None;
    }
    let visible: NSRect = msg_send![screen, visibleFrame];
    Some(area_of(visible))
}

unsafe fn create_panel(state: &mut State, key: &str, label: Option<&str>) -> Option<Panel> {
    // A remembered (dragged or resized) panel keeps its place and size; any
    // other panel starts at slot 0 and moves to its real slot when shown.
    let remembered = state.remembered.get(key).copied().unwrap_or_default();
    let card = remembered
        .card
        .unwrap_or_else(|| panel_size(state.image_size));
    let (width, height) = window_size(card);
    let origin = match remembered.origin {
        Some(origin) => origin,
        None => slot_origin((width, height), state.anchor, 0)?, // None: headless (CI)
    };
    let rect = NSRect::new(NSPoint::new(origin.0, origin.1), NSSize::new(width, height));
    let window = new_pip_window(rect)?;
    let name = label.map(str::to_owned).unwrap_or_else(|| short_key(key));
    let _: () = msg_send![window, setTitle: ns_string(&format!("cua PiP · {name}"))];
    let trail_rect = ns_rect(trail_frame(area_of(rect), (0.0, 0.0)));
    let Some(trail_window) = new_pip_window(trail_rect) else {
        close_window(window);
        return None;
    };
    let _: () = msg_send![
        trail_window,
        setTitle: ns_string(&format!("cua PiP trail · {name}"))
    ];

    let [r, g, b, _] = cursor_overlay::session_fill_rgba(key);
    let session_color = |alpha: f64| -> *mut CGColor {
        let color: *mut AnyObject = msg_send![
            class!(NSColor),
            colorWithSRGBRed: r as f64 / 255.0
            green: g as f64 / 255.0
            blue: b as f64 / 255.0
            alpha: alpha
        ];
        msg_send![color, CGColor]
    };

    // Content: a clear view that holds the cards and handles the mouse.
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    let stack_view = new_view(stack_view_class(), bounds);
    let _: () = msg_send![window, setContentView: stack_view];
    let _: () = msg_send![stack_view, release];
    // MouseEnteredAndExited (0x01) | MouseMoved (0x02) | ActiveAlways (0x80)
    // | InVisibleRect (0x200): resize cursors over a non-key panel.
    let tracking: *mut AnyObject = msg_send![class!(NSTrackingArea), alloc];
    let tracking: *mut AnyObject = msg_send![
        tracking,
        initWithRect: NSRect::ZERO
        options: 0x283u64
        owner: stack_view
        userInfo: std::ptr::null_mut::<AnyObject>()
    ];
    let _: () = msg_send![stack_view, addTrackingArea: tracking];
    let _: () = msg_send![tracking, release];

    // Hit plates under the cards, bottom-most: WindowServer passes presses
    // on fully transparent pixels to the window below (activating it), and
    // a card's rounded corners are transparent. A near-invisible square fill
    // under every drawn card keeps each press on a card, its corners and
    // resize band included, in this panel. The margin elsewhere stays clear
    // and click-through.
    let front_plate = new_hit_plate(stack_view);

    // The trail: back items over their hit plates, deepest card first so
    // depth 1 draws over depth 2, chips on top.
    let trail_bounds = NSRect::new(NSPoint::new(0.0, 0.0), trail_rect.size);
    let trail_view = new_view(trail_view_class(), trail_bounds);
    let _: () = msg_send![trail_window, setContentView: trail_view];
    let _: () = msg_send![trail_view, release];
    let mut plates = [front_plate; VIEWS];
    for plate in &mut plates[1..] {
        *plate = new_hit_plate(trail_view);
    }
    let mut backs: Vec<BackView> = (1..MAX_CARDS)
        .rev()
        .map(|depth| {
            new_back_card(
                trail_view,
                card,
                depth,
                session_color(0.5),
                session_color(0.16),
            )
        })
        .collect();
    backs.reverse();
    let chips: Vec<ChipView> = (1..MAX_CARDS)
        .map(|_| new_chip(trail_view, session_color(0.6)))
        .collect();
    let trail = TrailViews {
        window: trail_window as usize,
        cards: backs.try_into().ok()?,
        chips: chips.try_into().ok()?,
    };

    // Front card: rounded, with the session-colored border. A layer's border
    // composites above its sublayers, so it rims the glass.
    let front_view = new_view(class!(NSView), ns_rect(slot_frame(card, Slot::Front, 0)));
    let _: () = msg_send![front_view, setWantsLayer: true];
    let front_layer: *mut AnyObject = msg_send![front_view, layer];
    let _: () = msg_send![front_layer, setCornerRadius: CORNER_RADIUS];
    let _: () = msg_send![front_layer, setBorderWidth: BORDER_WIDTH];
    let _: () = msg_send![front_layer, setBorderColor: session_color(1.0)];

    // Everything visible sits in `body`, hosted by the glass background.
    // `layout_front` sizes it all.
    let card_bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(card.0, card.1));
    let body = new_view(class!(NSView), card_bounds);
    let glass = glass_background(card_bounds, body, CORNER_RADIUS);
    let _: () = msg_send![front_view, addSubview: glass];
    let _: () = msg_send![glass, release];

    // Header strip: a faint wash of the session color inside the glass.
    let header = new_view(class!(NSView), NSRect::ZERO);
    let _: () = msg_send![header, setWantsLayer: true];
    let header_layer: *mut AnyObject = msg_send![header, layer];
    let _: () = msg_send![header_layer, setBackgroundColor: session_color(0.16)];

    let client_icon = new_icon_view(NSRect::ZERO);
    let client_label = new_label(NSRect::ZERO, 12.0, 0.23, false); // NSFontWeightMedium
    let id = state.next_id;
    state.next_id += 1;
    let focus = new_button(
        "arrow.up.forward.app",
        "Bring this window forward",
        sel!(pipFocus:),
        id,
        NSRect::ZERO,
    );
    let close = new_button(
        "xmark",
        "Hide until this agent's next action",
        sel!(pipHide:),
        id,
        NSRect::ZERO,
    );
    let target_icon = new_icon_view(NSRect::ZERO);
    let target_title = new_label(NSRect::ZERO, 11.0, 0.0, true);
    for view in [
        client_icon,
        client_label,
        target_icon,
        target_title,
        focus,
        close,
    ] {
        let _: () = msg_send![header, addSubview: view];
    }
    let _: () = msg_send![body, addSubview: header];
    let _: () = msg_send![header, release];

    // Screenshot well: the latest still.
    let image_view = new_view(class!(NSImageView), NSRect::ZERO);
    let _: () = msg_send![image_view, setImageScaling: 3u64]; // proportionally up or down
    let _: () = msg_send![image_view, setWantsLayer: true];
    let image_layer: *mut AnyObject = msg_send![image_view, layer];
    let _: () = msg_send![image_layer, setCornerRadius: 8.0_f64];
    let _: () = msg_send![image_layer, setMasksToBounds: true];
    let well: *mut AnyObject = msg_send![
        class!(NSColor),
        colorWithSRGBRed: 0.0_f64
        green: 0.0_f64
        blue: 0.0_f64
        alpha: 0.22_f64
    ];
    let well_cg: *mut CGColor = msg_send![well, CGColor];
    let _: () = msg_send![image_layer, setBackgroundColor: well_cg];
    add_subview(body, image_view);

    // Live mirror over the well: a layer-hosting view whose layer shows
    // ScreenCaptureKit frames, hidden until the first one arrives.
    let live_view = new_view(class!(NSView), NSRect::ZERO);
    let live_layer: *mut AnyObject = msg_send![class!(CALayer), layer];
    let _: () = msg_send![live_layer, setContentsGravity: ns_string("resizeAspect")];
    let _: () = msg_send![live_layer, setCornerRadius: 8.0_f64];
    let _: () = msg_send![live_layer, setMasksToBounds: true];
    // setLayer before setWantsLayer: the view hosts this layer as is.
    let _: () = msg_send![live_view, setLayer: live_layer];
    let _: () = msg_send![live_view, setWantsLayer: true];
    let _: () = msg_send![live_view, setHidden: true];
    add_subview(body, live_view);

    let placeholder = new_label(NSRect::ZERO, 11.0, 0.0, true);
    set_text(placeholder as usize, "Preview unavailable");
    let _: () = msg_send![placeholder, setHidden: true];
    let _: () = msg_send![body, addSubview: placeholder];

    let status = new_label(NSRect::ZERO, 11.0, 0.0, true);
    let _: () = msg_send![body, addSubview: status];
    add_subview(stack_view, front_view);

    let mut panel = Panel {
        id,
        window: window as usize,
        image_view: image_view as usize,
        live_view: live_view as usize,
        live_layer: live_layer as usize,
        live_frame: None,
        live_tag: None,
        still_tag: None,
        stream: live::StreamState::default(),
        resolved_window: None,
        placeholder: placeholder as usize,
        status: status as usize,
        client_icon: client_icon as usize,
        client_label: client_label as usize,
        target_icon: target_icon as usize,
        target_title: target_title as usize,
        front_view: front_view as usize,
        glass: glass as usize,
        header: header as usize,
        focus: focus as usize,
        close: close as usize,
        trail,
        plates,
        cards: CardStack::new(),
        layout: vec![Slot::Front],
        motion: Default::default(),
        trail_motion: Trail::default(),
        verdicts: state.early.remove(key).unwrap_or_default(),
        finished_seen: HashSet::new(),
        finale: FinaleState::default(),
        finale_view: None,
        key: key.to_owned(),
        card,
        laid_out: (0.0, 0.0),
        stream_well: well_size(card),
        well_changed: Instant::now(),
        resized: remembered.card.is_some(),
        name,
        slot: None,
        dragged: remembered.origin.is_some(),
        placed: origin,
        last_frame: Instant::now(),
        shown: false,
        dismissed: false,
        target_visible: false,
        target: (None, None),
        client: None,
    };
    apply_card_frames(&mut panel);
    render_backs(&mut panel);
    Some(panel)
}

/// A borderless, non-activating floating `CuaPipPanel` (never key or main)
/// with a clear background, owned (+1) by the caller. Used for the panel
/// and its trail.
unsafe fn new_pip_window(rect: NSRect) -> Option<*mut AnyObject> {
    // NSWindowStyleMaskBorderless (0) | NonactivatingPanel (1 << 7). Not
    // Resizable: the panel resizes itself from the band inside the front
    // card's edges (one path, clamped by `resize_window`), so AppKit's own
    // edge tracking on a borderless window never competes for those presses.
    let style_mask: u64 = 1 << 7;
    let window: *mut AnyObject = msg_send![panel_class(), alloc];
    let window: *mut AnyObject = msg_send![
        window,
        initWithContentRect: rect
        styleMask: style_mask
        backing: 2u64
        defer: false
    ];
    if window.is_null() {
        return None;
    }
    let _: () = msg_send![window, setReleasedWhenClosed: false];
    let _: () = msg_send![window, setFloatingPanel: true];
    let _: () = msg_send![window, setLevel: 3i64]; // NSFloatingWindowLevel
    let _: () = msg_send![window, setBecomesKeyOnlyIfNeeded: true];
    // NSPanel hides when its app deactivates by default; cua-driver is never
    // the active app, so that would hide the panel at once.
    let _: () = msg_send![window, setHidesOnDeactivate: false];
    // CanJoinAllSpaces (1 << 0) | IgnoresCycle (1 << 6) | FullScreenAuxiliary (1 << 8)
    let behavior: u64 = (1 << 0) | (1 << 6) | (1 << 8);
    let _: () = msg_send![window, setCollectionBehavior: behavior];
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![window, setBackgroundColor: clear];
    let _: () = msg_send![window, setOpaque: false];
    let _: () = msg_send![window, setHasShadow: true];
    Some(window)
}

/// A square view with an almost transparent fill (alpha 0.01, invisible)
/// in `parent`, so presses on it are never passed to the window below.
unsafe fn new_hit_plate(parent: *mut AnyObject) -> usize {
    let plate = new_view(class!(NSView), NSRect::ZERO);
    let _: () = msg_send![plate, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![plate, layer];
    let fill: *mut AnyObject = msg_send![
        class!(NSColor),
        colorWithSRGBRed: 1.0_f64
        green: 1.0_f64
        blue: 1.0_f64
        alpha: 0.01_f64
    ];
    let fill: *mut CGColor = msg_send![fill, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: fill];
    add_subview(parent, plate);
    plate as usize
}

/// A back card inside `parent`: glass under a title strip (app icon and
/// window title, the part that peeks out) over the window's last still.
/// Hidden until the stack has a card at its depth. Its views follow the
/// card's frame through autoresizing.
unsafe fn new_back_card(
    parent: *mut AnyObject,
    card: (f64, f64),
    depth: usize,
    border: *mut CGColor,
    wash: *mut CGColor,
) -> BackView {
    let frame = slot_frame(card, Slot::Card(depth), 0);
    let (w, h) = (frame.w, frame.h);
    let view = new_view(class!(NSView), ns_rect(frame));
    let _: () = msg_send![view, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![view, layer];
    let _: () = msg_send![layer, setCornerRadius: CORNER_RADIUS];
    let _: () = msg_send![layer, setBorderWidth: 1.0_f64];
    let _: () = msg_send![layer, setBorderColor: border];

    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
    let body = new_view(class!(NSView), bounds);
    let glass = glass_background(bounds, body, CORNER_RADIUS);
    let _: () = msg_send![glass, setAutoresizingMask: 18u64]; // width + height sizable
    add_subview(view, glass);

    // Autoresizing: 2 width sizable, 4 max-x margin, 8 min-y margin (pinned
    // to the top), 16 height sizable.
    let strip_y = h - stack::CARD_STEP;
    let strip = new_view(
        class!(NSView),
        NSRect::new(NSPoint::new(0.0, strip_y), NSSize::new(w, stack::CARD_STEP)),
    );
    let _: () = msg_send![strip, setWantsLayer: true];
    let strip_layer: *mut AnyObject = msg_send![strip, layer];
    let _: () = msg_send![strip_layer, setBackgroundColor: wash];
    let _: () = msg_send![strip, setAutoresizingMask: 10u64];
    add_subview(body, strip);

    let icon = new_icon_view(NSRect::new(
        NSPoint::new(10.0, strip_y + 2.0),
        NSSize::new(10.0, 10.0),
    ));
    let _: () = msg_send![icon, setAutoresizingMask: 12u64];
    let _: () = msg_send![body, addSubview: icon];
    let title = new_label(
        NSRect::new(
            NSPoint::new(24.0, strip_y),
            NSSize::new((w - 34.0).max(0.0), stack::CARD_STEP - 1.0),
        ),
        10.0,
        0.0,
        true,
    );
    let _: () = msg_send![title, setAutoresizingMask: 10u64];
    let _: () = msg_send![body, addSubview: title];

    let image_view = new_view(
        class!(NSImageView),
        NSRect::new(
            NSPoint::new(5.0, 5.0),
            NSSize::new((w - 10.0).max(0.0), (strip_y - 7.0).max(0.0)),
        ),
    );
    let _: () = msg_send![image_view, setImageScaling: 3u64];
    let _: () = msg_send![image_view, setWantsLayer: true];
    let image_layer: *mut AnyObject = msg_send![image_view, layer];
    let _: () = msg_send![image_layer, setCornerRadius: 6.0_f64];
    let _: () = msg_send![image_layer, setMasksToBounds: true];
    let _: () = msg_send![image_view, setAutoresizingMask: 18u64];
    add_subview(body, image_view);

    let _: () = msg_send![view, setHidden: true];
    add_subview(parent, view);
    BackView {
        view: view as usize,
        image_view: image_view as usize,
        icon: icon as usize,
        title: title as usize,
    }
}

/// Liquid Glass (`NSGlassEffectView`, macOS 26) hosting `body`, or an
/// `NSVisualEffectView` HUD material on older systems. Rounded either way.
/// Takes ownership of `body`; returns an owned (+1) view.
unsafe fn glass_background(bounds: NSRect, body: *mut AnyObject, radius: f64) -> *mut AnyObject {
    // Width + height sizable, so the body tracks the background.
    let _: () = msg_send![body, setAutoresizingMask: 18u64];
    if let Some(glass_class) = AnyClass::get("NSGlassEffectView") {
        let glass = new_view(glass_class, bounds);
        let _: () = msg_send![glass, setCornerRadius: radius];
        let _: () = msg_send![glass, setContentView: body];
        let _: () = msg_send![body, release];
        return glass;
    }
    let effect = new_view(class!(NSVisualEffectView), bounds);
    let _: () = msg_send![effect, setMaterial: 13i64]; // HUDWindow
    let _: () = msg_send![effect, setBlendingMode: 0i64]; // behindWindow
    let _: () = msg_send![effect, setState: 1i64]; // active
    let _: () = msg_send![effect, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![effect, layer];
    let _: () = msg_send![layer, setCornerRadius: radius];
    let _: () = msg_send![layer, setMasksToBounds: true];
    add_subview(effect, body);
    effect
}

/// A chip inside `parent` (hidden until the stack has a chip in its row):
/// a glass circle with a `border` ring holding the app icon, a green check
/// badge on its lower right, and the window title as a tiny caption under
/// it. Its parts stay centered (flexible margins) while the chip's frame
/// springs from where its window was drawn.
unsafe fn new_chip(parent: *mut AnyObject, border: *mut CGColor) -> ChipView {
    use stack::{CHIP, CHIP_CAPTION, CHIP_H, CHIP_ICON, CHIP_W};
    // NSViewMinXMargin 1 | MaxXMargin 4 | MinYMargin 8 | MaxYMargin 32.
    const CENTERED: u64 = 1 | 4 | 8 | 32;
    let view = new_view(
        class!(NSView),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(CHIP_W, CHIP_H)),
    );
    let circle = NSRect::new(
        NSPoint::new((CHIP_W - CHIP) / 2.0, CHIP_CAPTION + 2.0),
        NSSize::new(CHIP, CHIP),
    );
    let ring = new_view(class!(NSView), circle);
    let _: () = msg_send![ring, setWantsLayer: true];
    let ring_layer: *mut AnyObject = msg_send![ring, layer];
    let _: () = msg_send![ring_layer, setCornerRadius: CHIP / 2.0];
    let _: () = msg_send![ring_layer, setBorderWidth: 1.0_f64];
    let _: () = msg_send![ring_layer, setBorderColor: border];
    let _: () = msg_send![ring, setAutoresizingMask: CENTERED];
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(CHIP, CHIP));
    let body = new_view(class!(NSView), bounds);
    let inset = (CHIP - CHIP_ICON) / 2.0;
    let icon = new_icon_view(NSRect::new(
        NSPoint::new(inset, inset),
        NSSize::new(CHIP_ICON, CHIP_ICON),
    ));
    let _: () = msg_send![body, addSubview: icon];
    add_subview(ring, glass_background(bounds, body, CHIP / 2.0));
    add_subview(view, ring);

    let badge_size = 15.0;
    let badge = new_view(
        class!(NSView),
        NSRect::new(
            NSPoint::new(
                circle.origin.x + CHIP - badge_size + 2.0,
                circle.origin.y - 2.0,
            ),
            NSSize::new(badge_size, badge_size),
        ),
    );
    let _: () = msg_send![badge, setWantsLayer: true];
    let badge_layer: *mut AnyObject = msg_send![badge, layer];
    let (mark, _) = new_mark(badge_size, Mark::Check);
    let _: () = msg_send![badge_layer, addSublayer: mark];
    let _: () = msg_send![badge, setAutoresizingMask: CENTERED];
    add_subview(view, badge);

    let caption = new_label(
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(CHIP_W, CHIP_CAPTION)),
        9.0,
        0.0,
        true,
    );
    let _: () = msg_send![caption, setAlignment: 2u64]; // NSTextAlignmentCenter (AppKit)
    let _: () = msg_send![caption, setAutoresizingMask: 1u64 | 4 | 32];
    let _: () = msg_send![view, addSubview: caption];

    let _: () = msg_send![view, setHidden: true];
    add_subview(parent, view);
    ChipView {
        view: view as usize,
        icon: icon as usize,
        caption: caption as usize,
    }
}

/// Lay the front card's views out for a card of `size` (nothing to do when
/// they already are): header across the top, image well, status line.
unsafe fn layout_front(panel: &mut Panel, (w, h): (f64, f64)) {
    if panel.laid_out == (w, h) {
        return;
    }
    panel.laid_out = (w, h);
    set_frame(
        panel.glass,
        Area {
            x: 0.0,
            y: 0.0,
            w,
            h,
        },
    );
    set_frame(
        panel.header,
        Area {
            x: 0.0,
            y: h - HEADER_HEIGHT,
            w,
            h: HEADER_HEIGHT,
        },
    );
    let label = panel.client_label as *mut AnyObject;
    let _: () = msg_send![label, sizeToFit];
    let fitted: NSRect = msg_send![label, frame];
    let layout = stack::header_layout(w, fitted.size.width);
    for (view, area) in [
        (panel.client_icon, layout.client_icon),
        (panel.client_label, layout.client_label),
        (panel.target_icon, layout.target_icon),
        (panel.target_title, layout.target_title),
        (panel.focus, layout.focus),
        (panel.close, layout.close),
    ] {
        set_frame(view, area);
    }
    let (well_w, well_h) = well_size((w, h));
    let well = Area {
        x: PAD,
        y: 6.0 + STATUS_HEIGHT + 4.0,
        w: well_w,
        h: well_h,
    };
    set_frame(panel.image_view, well);
    set_frame(panel.live_view, well);
    let placeholder = panel.placeholder as *mut AnyObject;
    let _: () = msg_send![placeholder, sizeToFit];
    let fitted: NSRect = msg_send![placeholder, frame];
    let (text_w, text_h) = (fitted.size.width.min(well_w), fitted.size.height);
    set_frame(
        panel.placeholder,
        Area {
            x: well.x + (well_w - text_w) / 2.0,
            y: well.y + (well_h - text_h) / 2.0,
            w: text_w,
            h: text_h,
        },
    );
    set_frame(
        panel.status,
        Area {
            x: PAD + 2.0,
            y: 6.0,
            w: (well_w - 4.0).max(0.0),
            h: STATUS_HEIGHT,
        },
    );
}

/// Where the view for `slot` is drawn now, in panel coordinates: its
/// resting frame (with `back_cards` cards in the stack) plus its offset.
fn view_frame(panel: &Panel, slot: Slot, back_cards: usize) -> Area {
    panel.motion[slot.view()].frame(slot_frame(panel.card, slot, back_cards))
}

/// Where each item of the stack is drawn now, front first, in panel
/// coordinates.
fn item_frames(panel: &Panel) -> Vec<Area> {
    let cards = back_cards(&panel.layout);
    panel
        .layout
        .iter()
        .map(|slot| view_frame(panel, *slot, cards))
        .collect()
}

/// The view that draws `slot`.
fn card_view(panel: &Panel, slot: Slot) -> usize {
    match slot {
        Slot::Front => panel.front_view,
        Slot::Card(depth) => panel.trail.cards[depth - 1].view,
        Slot::Chip(row) => panel.trail.chips[row].view,
    }
}

/// A panel-coordinate frame in the trail window's coordinates.
fn to_trail(area: Area) -> Area {
    Area {
        x: area.x + TRAIL_PAD,
        ..area
    }
}

/// Put every item view (and its hit plate) where it is drawn now.
unsafe fn apply_card_frames(panel: &mut Panel) {
    let cards = back_cards(&panel.layout);
    for slot in panel.layout.clone() {
        let frame = view_frame(panel, slot, cards);
        let placed = if slot == Slot::Front {
            frame
        } else {
            to_trail(frame)
        };
        set_frame(card_view(panel, slot), placed);
        set_frame(panel.plates[slot.view()], placed);
    }
    let front = view_frame(panel, Slot::Front, cards);
    layout_front(panel, (front.w, front.h));
    // The window shadows follow the items' outline once they are at rest.
    if !panel.motion.iter().any(Motion::moving) {
        let _: () = msg_send![panel.window as *mut AnyObject, invalidateShadow];
        let _: () = msg_send![panel.trail.window as *mut AnyObject, invalidateShadow];
    }
}

/// Show each back item's window in the view for its slot (a card: title,
/// app icon, own still; a chip: app icon and title), and hide the views
/// (and plates) no item uses.
unsafe fn render_backs(panel: &mut Panel) {
    let app_of = |pid: Option<i32>| -> *mut AnyObject {
        match pid {
            Some(pid) => app_icon(msg_send![
                class!(NSRunningApplication),
                runningApplicationWithProcessIdentifier: pid
            ]),
            None => std::ptr::null_mut(),
        }
    };
    let mut used = [false; VIEWS];
    used[0] = true;
    for (card, slot) in panel.cards.cards().iter().zip(&panel.layout) {
        used[slot.view()] = true;
        match *slot {
            Slot::Front => {}
            Slot::Card(depth) => {
                let back = &panel.trail.cards[depth - 1];
                set_text(back.title, &card.data.title);
                let _: () = msg_send![back.icon as *mut AnyObject, setImage: app_of(card.data.pid)];
                let image = own_pixels(card.key, card.data.still.as_ref())
                    .map_or(std::ptr::null_mut(), |image| image.0 as *mut AnyObject);
                let _: () = msg_send![back.image_view as *mut AnyObject, setImage: image];
            }
            Slot::Chip(row) => {
                let chip = &panel.trail.chips[row];
                set_text(chip.caption, &card.data.title);
                let _: () = msg_send![chip.icon as *mut AnyObject, setImage: app_of(card.data.pid)];
            }
        }
    }
    for depth in 1..MAX_CARDS {
        let back = &panel.trail.cards[depth - 1];
        let slot = Slot::Card(depth);
        if !used[slot.view()] {
            let _: () = msg_send![
                back.image_view as *mut AnyObject,
                setImage: std::ptr::null_mut::<AnyObject>()
            ];
        }
    }
    for index in 1..VIEWS {
        let view = match index {
            depth if depth < MAX_CARDS => panel.trail.cards[depth - 1].view,
            row => panel.trail.chips[row - MAX_CARDS].view,
        };
        let _: () = msg_send![view as *mut AnyObject, setHidden: !used[index]];
        let _: () = msg_send![panel.plates[index] as *mut AnyObject, setHidden: !used[index]];
        if !used[index] {
            panel.motion[index] = Motion::default();
        }
    }
}

/// Move the trail window to follow the panel: the panel's frame widened
/// for chips, offset by the trail's lag.
unsafe fn sync_trail(panel: &Panel) {
    let frame: NSRect = msg_send![panel.window as *mut AnyObject, frame];
    let trail = trail_frame(area_of(frame), panel.trail_motion.offset());
    let _: () = msg_send![
        panel.trail.window as *mut AnyObject,
        setFrame: ns_rect(trail)
        display: false
    ];
}

/// Log the trail's lag once it has settled after a drag.
fn report_trail(panel: &mut Panel, now: Instant) {
    if let Some((max, settle)) = panel.trail_motion.settled(now) {
        tracing::info!(target: "pip", session = %panel.key, max_pt = %format_args!("{max:.1}"), settle_ms = settle.as_millis() as u64, "PiP trail lag");
    }
}

/// Fade a view's opacity from `from` to `to` over the restack.
unsafe fn fade_view(view: usize, from: f64, to: f64) {
    let view = view as *mut AnyObject;
    let _: () = msg_send![view, setAlphaValue: from];
    let _: () = msg_send![class!(NSAnimationContext), beginGrouping];
    let context: *mut AnyObject = msg_send![class!(NSAnimationContext), currentContext];
    let _: () = msg_send![context, setDuration: FADE.as_secs_f64()];
    let animator: *mut AnyObject = msg_send![view, animator];
    let _: () = msg_send![animator, setAlphaValue: to];
    let _: () = msg_send![class!(NSAnimationContext), endGrouping];
}

// ── Card motion ticker ────────────────────────────────────────────────────

/// Animation step while any card is moving (~60 Hz).
const TICK: Duration = Duration::from_millis(16);
/// A tick is scheduled (main queue only).
static TICKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn start_ticking() {
    if !TICKING.swap(true, std::sync::atomic::Ordering::Relaxed) {
        dispatch_to_main_after(TICK, Instant::now(), tick_cb);
    }
}

/// Step every moving item's spring, and every lagging trail, by the real
/// time since the last tick.
unsafe extern "C" fn tick_cb(ctx: *mut c_void) {
    let last: Instant = *Box::from_raw(ctx as *mut Instant);
    let now = Instant::now();
    // A stalled main thread must not fling the springs.
    let dt = now
        .saturating_duration_since(last)
        .as_secs_f64()
        .min(1.0 / 30.0);
    let moving = objc2::rc::autoreleasepool(|_| {
        with_state(|state| {
            let mut moving = false;
            for panel in state.panels.values_mut().chain(state.ending.iter_mut()) {
                if panel.motion.iter().any(Motion::moving) {
                    for motion in &mut panel.motion {
                        moving |= motion.step(dt);
                    }
                    apply_card_frames(panel);
                }
                if panel.trail_motion.moving() {
                    moving |= panel.trail_motion.step(dt);
                    sync_trail(panel);
                    report_trail(panel, now);
                }
            }
            moving
        })
    })
    .unwrap_or(false);
    if moving {
        dispatch_to_main_after(TICK, now, tick_cb);
    } else {
        TICKING.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

// ── Panel mouse: drag, resize, raise ──────────────────────────────────────
//
// The content view (`CuaPipStack`) takes every press that is not on a
// header button (its `hitTest:` returns itself over any card, nothing over
// the transparent margin) and accepts the first mouse, so the panel never
// needs to become key. Resizing sets the window frame outside the state
// lock: AppKit calls `setFrameSize:` synchronously, which lays the cards out.

/// The panel (live, or ended and playing its finale) whose panel or trail
/// window is `window`, and whether it is the trail.
fn panel_for(state: &mut State, window: usize) -> Option<(&mut Panel, bool)> {
    state
        .panels
        .values_mut()
        .chain(state.ending.iter_mut())
        .find(|panel| panel.window == window || panel.trail.window == window)
        .map(|panel| {
            let trail = panel.trail.window == window;
            (panel, trail)
        })
}

/// The panel (live or ending) with `id`.
fn panel_by_id(state: &mut State, id: i64) -> Option<&mut Panel> {
    state
        .panels
        .values_mut()
        .chain(state.ending.iter_mut())
        .find(|panel| panel.id == id)
}

/// A point in a panel's or trail's content view, in panel coordinates.
fn panel_point((x, y): (f64, f64), in_trail: bool) -> (f64, f64) {
    if in_trail {
        (x - TRAIL_PAD, y)
    } else {
        (x, y)
    }
}

/// An event's location in `view`'s coordinates.
unsafe fn event_point(view: *mut AnyObject, event: *mut AnyObject) -> (f64, f64) {
    let in_window: NSPoint = msg_send![event, locationInWindow];
    let local: NSPoint = msg_send![
        view,
        convertPoint: in_window
        fromView: std::ptr::null_mut::<AnyObject>()
    ];
    (local.x, local.y)
}

unsafe fn window_of(view: *mut AnyObject) -> usize {
    let window: *mut AnyObject = msg_send![view, window];
    window as usize
}

unsafe fn mouse_location() -> (f64, f64) {
    let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
    (mouse.x, mouse.y)
}

extern "C" fn stack_hit_test(this: *mut AnyObject, _cmd: Sel, point: NSPoint) -> *mut AnyObject {
    unsafe {
        let hit: *mut AnyObject = msg_send![super(this, class!(NSView)), hitTest: point];
        if hit.is_null() {
            return hit;
        }
        let is_button: bool = msg_send![hit, isKindOfClass: button_class()];
        if is_button {
            return hit;
        }
        // `point` is in the superview's coordinates.
        let superview: *mut AnyObject = msg_send![this, superview];
        let local: NSPoint = msg_send![this, convertPoint: point fromView: superview];
        let window = window_of(this);
        let over_card = try_with_state(|state| {
            panel_for(state, window).is_some_and(|(panel, in_trail)| {
                let point = panel_point((local.x, local.y), in_trail);
                item_at(point, &panel.layout, &item_frames(panel), in_trail).is_some()
            })
        });
        // Busy state (a re-entrant call): keep the press.
        if over_card.unwrap_or(true) {
            this
        } else {
            std::ptr::null_mut()
        }
    }
}

extern "C" fn stack_mouse_down(this: *mut AnyObject, _cmd: Sel, event: *mut AnyObject) {
    unsafe {
        let window = window_of(this);
        let point = event_point(this, event);
        let mouse = mouse_location();
        let max = visible_frame_of(window as *mut AnyObject)
            .map_or(MIN_CARD, |visible| max_card((visible.w, visible.h)));
        with_state(|state| {
            let Some((panel, in_trail)) = panel_for(state, window) else {
                return;
            };
            let (key, id) = (panel.key.clone(), panel.id);
            // A drag moves the panel, whichever window was pressed.
            let frame: NSRect = msg_send![panel.window as *mut AnyObject, frame];
            let point = panel_point(point, in_trail);
            let frames = item_frames(panel);
            let item = item_at(point, &panel.layout, &frames, in_trail);
            let edges = if item == Some(0) {
                resize_edges(point, frames[0])
            } else {
                0
            };
            let pressed = item
                .filter(|item| *item > 0)
                .and_then(|item| panel.cards.cards().get(item))
                .map(|card| card.key);
            let region = stack::press_region(point, item, edges, frames[0]);
            tracing::info!(target: "pip", session = %key, region, x = point.0, y = point.1, "PiP panel press");
            state.gesture = Some(Gesture {
                id,
                mouse,
                start: area_of(frame),
                origin: (frame.origin.x, frame.origin.y),
                edges,
                pressed,
                moved: false,
                max,
            });
        });
    }
}

extern "C" fn stack_mouse_dragged(_this: *mut AnyObject, _cmd: Sel, _event: *mut AnyObject) {
    unsafe {
        let mouse = mouse_location();
        let resize = with_state(|state| {
            let State {
                gesture,
                panels,
                ending,
                ..
            } = state;
            let gesture = gesture.as_mut()?;
            let panel = panels
                .values_mut()
                .chain(ending.iter_mut())
                .find(|panel| panel.id == gesture.id)?;
            let delta = (mouse.0 - gesture.mouse.0, mouse.1 - gesture.mouse.1);
            if gesture.edges != 0 {
                let frame = resize_window(gesture.start, gesture.edges, delta, gesture.max);
                return Some((panel.window, frame));
            }
            if !gesture.moved && delta.0.hypot(delta.1) < DRAG_SLOP {
                return None;
            }
            gesture.moved = true;
            let origin = (gesture.start.x + delta.0, gesture.start.y + delta.1);
            let step = (origin.0 - gesture.origin.0, origin.1 - gesture.origin.1);
            gesture.origin = origin;
            // The trail stays put on screen, then swings after the panel.
            panel.trail_motion.panel_dragged(step);
            let _: () = msg_send![
                panel.window as *mut AnyObject,
                setFrameOrigin: NSPoint::new(origin.0, origin.1)
            ];
            sync_trail(panel);
            start_ticking();
            None
        })
        .flatten();
        if let Some((window, frame)) = resize {
            let _: () = msg_send![window as *mut AnyObject, setFrame: ns_rect(frame) display: true];
            // The trail follows the resized panel at once (no lag).
            with_state(|state| {
                if let Some((panel, _)) = panel_for(state, window) {
                    panel.trail_motion.snap();
                    sync_trail(panel);
                }
            });
        }
    }
}

extern "C" fn stack_mouse_up(_this: *mut AnyObject, _cmd: Sel, _event: *mut AnyObject) {
    with_state(|state| {
        let Some(gesture) = state.gesture.take() else {
            return;
        };
        let Some(panel) = panel_by_id(state, gesture.id) else {
            return;
        };
        if gesture.moved {
            // The user placed it: keep it there and free its slot.
            panel.dragged = true;
            panel.slot = None;
            panel.trail_motion.release(Instant::now());
            report_trail(panel, Instant::now());
            start_ticking();
            return;
        }
        // A click (not a resize) on a back card raises its window, if that
        // window is still behind the front card.
        let pressed = gesture.pressed.filter(|_| gesture.edges == 0);
        if let Some(tag) = stack::click_target(pressed, &panel.cards.keys()) {
            unsafe { raise_card(state, gesture.id, tag) };
        }
    });
}

extern "C" fn stack_mouse_moved(this: *mut AnyObject, _cmd: Sel, event: *mut AnyObject) {
    unsafe {
        let window = window_of(this);
        let point = event_point(this, event);
        let edges = try_with_state(|state| {
            panel_for(state, window).map(|(panel, _)| {
                resize_edges(
                    point,
                    view_frame(panel, Slot::Front, back_cards(&panel.layout)),
                )
            })
        })
        .flatten()
        .unwrap_or(0);
        set_resize_cursor(edges);
    }
}

extern "C" fn stack_mouse_exited(_this: *mut AnyObject, _cmd: Sel, _event: *mut AnyObject) {
    unsafe { set_resize_cursor(0) };
}

/// The frame-resize cursor for `edges` (macOS 15+), else the arrow.
unsafe fn set_resize_cursor(edges: u8) {
    let cursor: *mut AnyObject = if edges == 0 {
        msg_send![class!(NSCursor), arrowCursor]
    } else {
        let responds: bool = msg_send![
            class!(NSCursor),
            respondsToSelector: sel!(frameResizeCursorFromPosition:inDirections:)
        ];
        if !responds {
            return;
        }
        // NSCursorFrameResizeDirectionsAll = 3; `edges` uses the position bits.
        msg_send![
            class!(NSCursor),
            frameResizeCursorFromPosition: edges as u64
            inDirections: 3u64
        ]
    };
    if !cursor.is_null() {
        let _: () = msg_send![cursor, set];
    }
}

extern "C" fn stack_set_frame_size(this: *mut AnyObject, _cmd: Sel, size: NSSize) {
    unsafe {
        let _: () = msg_send![super(this, class!(NSView)), setFrameSize: size];
        let window = window_of(this);
        // Inside a state operation (the panel being built) the caller lays
        // out itself.
        try_with_state(|state| on_resized(state, window, (size.width, size.height)));
    }
}

/// The panel window is now `size`: lay the cards out for the new front card
/// and resize the live stream once the size settles.
unsafe fn on_resized(state: &mut State, window: usize, size: (f64, f64)) {
    let Some((panel, false)) = panel_for(state, window) else {
        return;
    };
    let key = panel.key.clone();
    let card = card_size(size);
    if card == panel.card || card.0 <= 0.0 || card.1 <= 0.0 {
        return;
    }
    panel.card = card;
    // A panel the user sized stays where they put it, like a dragged one.
    panel.resized = true;
    panel.dragged = true;
    panel.slot = None;
    apply_card_frames(panel);
    panel.well_changed = Instant::now();
    dispatch_to_main_after(
        RESIZE_DEBOUNCE + Duration::from_millis(5),
        key,
        resize_settle_cb,
    );
}

/// Debounced end of a resize: size the live stream to the new well.
unsafe extern "C" fn resize_settle_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| {
        let Some(panel) = state.panels.get_mut(&key) else {
            return;
        };
        // A later change re-armed the timer.
        if !resize_settled(panel.well_changed, Instant::now()) {
            return;
        }
        panel.stream_well = well_size(panel.card);
        refresh(state, &key);
    });
}

/// The panel's content view: holds the cards and handles the mouse.
fn stack_view_class() -> &'static AnyClass {
    static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
    CLASS.get_or_init(|| {
        register_class("CuaPipStack", class!(NSView), |builder| unsafe {
            builder.add_method(
                sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> _,
            );
            builder.add_method(
                sel!(hitTest:),
                stack_hit_test as extern "C" fn(_, _, _) -> _,
            );
            builder.add_method(sel!(mouseDown:), stack_mouse_down as extern "C" fn(_, _, _));
            builder.add_method(
                sel!(mouseDragged:),
                stack_mouse_dragged as extern "C" fn(_, _, _),
            );
            builder.add_method(sel!(mouseUp:), stack_mouse_up as extern "C" fn(_, _, _));
            builder.add_method(
                sel!(mouseMoved:),
                stack_mouse_moved as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(mouseExited:),
                stack_mouse_exited as extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(setFrameSize:),
                stack_set_frame_size as extern "C" fn(_, _, _),
            );
        })
    })
}

/// The trail window's content view: holds the back items and takes presses
/// on them (a click raises, a drag moves the panel); clicks elsewhere pass
/// through.
fn trail_view_class() -> &'static AnyClass {
    static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
    CLASS.get_or_init(|| {
        register_class("CuaPipTrail", class!(NSView), |builder| unsafe {
            builder.add_method(
                sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> _,
            );
            builder.add_method(
                sel!(hitTest:),
                stack_hit_test as extern "C" fn(_, _, _) -> _,
            );
            builder.add_method(sel!(mouseDown:), stack_mouse_down as extern "C" fn(_, _, _));
            builder.add_method(
                sel!(mouseDragged:),
                stack_mouse_dragged as extern "C" fn(_, _, _),
            );
            builder.add_method(sel!(mouseUp:), stack_mouse_up as extern "C" fn(_, _, _));
        })
    })
}

// ── Small AppKit helpers ──────────────────────────────────────────────────

/// `[[class alloc] initWithFrame:]`, owned (+1) by the caller.
unsafe fn new_view(class: &AnyClass, frame: NSRect) -> *mut AnyObject {
    let view: *mut AnyObject = msg_send![class, alloc];
    msg_send![view, initWithFrame: frame]
}

/// Add an owned `child` and hand its ownership to `parent`.
unsafe fn add_subview(parent: *mut AnyObject, child: *mut AnyObject) {
    let _: () = msg_send![parent, addSubview: child];
    let _: () = msg_send![child, release];
}

/// Aspect-fit icon view (autoreleased).
unsafe fn new_icon_view(frame: NSRect) -> *mut AnyObject {
    let view = new_view(class!(NSImageView), frame);
    let _: () = msg_send![view, setImageScaling: 3u64];
    let _: *mut AnyObject = msg_send![view, autorelease];
    view
}

/// One-line, non-selectable, tail-truncating label (autoreleased).
unsafe fn new_label(frame: NSRect, size: f64, weight: f64, secondary: bool) -> *mut AnyObject {
    let label: *mut AnyObject = msg_send![class!(NSTextField), labelWithString: ns_string("")];
    let _: () = msg_send![label, setFrame: frame];
    let _: () = msg_send![label, setSelectable: false];
    let _: () = msg_send![label, setLineBreakMode: 4u64]; // byTruncatingTail
    let _: () = msg_send![label, setMaximumNumberOfLines: 1i64];
    let font: *mut AnyObject = msg_send![class!(NSFont), systemFontOfSize: size weight: weight];
    let _: () = msg_send![label, setFont: font];
    let color: *mut AnyObject = if secondary {
        msg_send![class!(NSColor), secondaryLabelColor]
    } else {
        msg_send![class!(NSColor), labelColor]
    };
    let _: () = msg_send![label, setTextColor: color];
    label
}

/// Borderless SF Symbol button wired to the shared target (autoreleased).
unsafe fn new_button(
    symbol: &str,
    tooltip: &str,
    action: Sel,
    tag: i64,
    frame: NSRect,
) -> *mut AnyObject {
    let image = symbol_image(symbol);
    let target = button_target() as *mut AnyObject;
    let button: *mut AnyObject = msg_send![
        button_class(),
        buttonWithImage: image
        target: target
        action: action
    ];
    let _: () = msg_send![button, setFrame: frame];
    let _: () = msg_send![button, setBordered: false];
    let _: () = msg_send![button, setImagePosition: 1u64]; // imageOnly
    let _: () = msg_send![button, setTag: tag];
    let _: () = msg_send![button, setToolTip: ns_string(tooltip)];
    let tint: *mut AnyObject = msg_send![class!(NSColor), secondaryLabelColor];
    let _: () = msg_send![button, setContentTintColor: tint];
    let config: *mut AnyObject = msg_send![
        class!(NSImageSymbolConfiguration),
        configurationWithPointSize: 12.0_f64
        weight: 0.23_f64
    ];
    let _: () = msg_send![button, setSymbolConfiguration: config];
    button
}

unsafe fn symbol_image(name: &str) -> *mut AnyObject {
    msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: ns_string(name)
        accessibilityDescription: std::ptr::null_mut::<AnyObject>()
    ]
}

/// The MCP client's app icon: a regular running app whose name or bundle id
/// matches the client name, else the first regular app among the ancestors
/// of the process that opened the MCP connection, else an SF Symbol.
unsafe fn client_icon(client_name: Option<&str>, client_pid: Option<i32>) -> *mut AnyObject {
    let regular = |app: *mut AnyObject| -> bool {
        if app.is_null() {
            return false;
        }
        let policy: i64 = msg_send![app, activationPolicy];
        policy == 0 // NSApplicationActivationPolicyRegular
    };
    if let Some(token) = client_name.and_then(client_match_token) {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let apps: *mut AnyObject = msg_send![workspace, runningApplications];
        let count: usize = msg_send![apps, count];
        for index in 0..count {
            let app: *mut AnyObject = msg_send![apps, objectAtIndex: index];
            if !regular(app) {
                continue;
            }
            let name: *mut AnyObject = msg_send![app, localizedName];
            let bundle: *mut AnyObject = msg_send![app, bundleIdentifier];
            let matches = ns_to_string(name)
                .is_some_and(|name| name.to_lowercase().starts_with(&token))
                || ns_to_string(bundle).is_some_and(|id| id.to_lowercase().contains(&token));
            if matches {
                return msg_send![app, icon];
            }
        }
    }
    let mut pid = client_pid.unwrap_or(0);
    for _ in 0..16 {
        if pid <= 1 {
            break;
        }
        let app: *mut AnyObject = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ];
        if regular(app) {
            return msg_send![app, icon];
        }
        pid = parent_pid(pid).unwrap_or(0);
    }
    symbol_image("cursorarrow.rays")
}

fn parent_pid(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut c_void,
            size,
        )
    };
    (read == size).then_some(info.pbi_ppid as i32)
}

unsafe fn set_text(label: usize, text: &str) {
    let _: () = msg_send![label as *mut AnyObject, setStringValue: ns_string(text)];
}

/// Autoreleased NSString (interior NULs dropped).
unsafe fn ns_string(text: &str) -> *mut AnyObject {
    let cstring = std::ffi::CString::new(text.replace('\0', "")).unwrap_or_default();
    msg_send![
        class!(NSString),
        stringWithUTF8String: cstring.as_ptr() as *const u8
    ]
}

unsafe fn ns_to_string(string: *mut AnyObject) -> Option<String> {
    if string.is_null() {
        return None;
    }
    let utf8: *const u8 = msg_send![string, UTF8String];
    if utf8.is_null() {
        return None;
    }
    Some(
        std::ffi::CStr::from_ptr(utf8.cast())
            .to_string_lossy()
            .into_owned(),
    )
}

// ── AppKit main loop helper for Serve mode ───────────────────────────────

/// Park the main thread in `NSApplication.run()`. Used by `cua-driver
/// serve --experimental-pip` so the dispatch_async_f → main queue
/// path PiP frames go through can be drained. Mirrors the cursor
/// overlay's `run_appkit` startup (Accessory activation policy →
/// finishLaunching → run) without installing the overlay's
/// CALayer-backed window itself.
///
/// Never returns — the background `serve::run_serve_cmd` thread calls
/// `std::process::exit` when it finishes, which tears down NSApp at
/// the same time.
pub fn run_appkit_main_loop() {
    let _mtm = objc2_foundation::MainThreadMarker::new()
        .expect("run_appkit_main_loop must be called from the main thread");
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        // Accessory policy: no Dock icon, no menu bar. Keeps the
        // daemon out of the user's application switcher, same as
        // the cursor overlay's NSApp setup.
        let _: bool = msg_send![app, setActivationPolicy: 1i64];
        let _: () = msg_send![app, finishLaunching];
        let _: () = msg_send![app, run];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VISIBLE: Area = Area {
        x: 0.0,
        y: 0.0,
        w: 1440.0,
        h: 875.0,
    };
    const SIZE: (f64, f64) = (336.0, 258.0);

    #[test]
    fn first_panel_sits_in_the_bottom_right_corner() {
        // A visible frame that starts above a 70pt Dock.
        let visible = Area {
            y: 70.0,
            h: 805.0,
            ..VISIBLE
        };
        let bottom_right = first_slot_bottom_right(VISIBLE, visible, SIZE, None);
        assert_eq!(bottom_right, (1424.0, 86.0));
        assert_eq!(
            stack_origin(bottom_right, visible, SIZE, 0),
            (1424.0 - 336.0, 86.0)
        );
    }

    #[test]
    fn a_1440x900_screen_with_a_dock_puts_slot_0_bottom_right_and_slot_1_above_it() {
        let screen = Area {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        };
        // AppKit visibleFrame: the Dock takes 70pt at the bottom, the menu
        // bar 25pt at the top.
        let visible = Area {
            x: 0.0,
            y: 70.0,
            w: 1440.0,
            h: 805.0,
        };
        let bottom_right = first_slot_bottom_right(screen, visible, SIZE, None);
        let slot0 = stack_origin(bottom_right, visible, SIZE, 0);
        let slot1 = stack_origin(bottom_right, visible, SIZE, 1);
        assert_eq!(slot0, (1440.0 - 16.0 - 336.0, 70.0 + 16.0));
        assert_eq!(slot1, (slot0.0, slot0.1 + SIZE.1 + STACK_GAP));
        // Slot 0 in CoreGraphics top-left coordinates: bottom edge 16pt above
        // the Dock.
        assert_eq!(screen.h - slot0.1 - SIZE.1, 900.0 - 70.0 - 16.0 - 258.0);
    }

    #[test]
    fn later_panels_stack_upward_without_overlap_then_wrap_left() {
        let bottom_right = first_slot_bottom_right(VISIBLE, VISIBLE, SIZE, None);
        let top = VISIBLE.y + VISIBLE.h;
        let origins: Vec<_> = (0..4)
            .map(|slot| stack_origin(bottom_right, VISIBLE, SIZE, slot))
            .collect();
        // One column holds three 258pt panels in an 875pt visible frame.
        assert_eq!(
            origins[1],
            (origins[0].0, origins[0].1 + SIZE.1 + STACK_GAP)
        );
        assert_eq!(origins[2].0, origins[0].0);
        assert!(origins[2].1 + SIZE.1 <= top - EDGE_INSET);
        assert_eq!(
            origins[3],
            (origins[0].0 - SIZE.0 - STACK_GAP, origins[0].1)
        );
        // No two panels overlap.
        for (i, a) in origins.iter().enumerate() {
            for b in &origins[i + 1..] {
                let apart_x = (a.0 - b.0).abs() >= SIZE.0;
                let apart_y = (a.1 - b.1).abs() >= SIZE.1;
                assert!(apart_x || apart_y, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn geometry_anchor_places_first_panel_top_left() {
        let screen = Area {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        };
        let bottom_right = first_slot_bottom_right(screen, VISIBLE, SIZE, Some((20, 40)));
        let top = 900.0 - 40.0 - 258.0;
        let origin = |slot| stack_origin(bottom_right, VISIBLE, SIZE, slot);
        assert_eq!(origin(0), (20.0, top));
        // No room above the anchor, so the column runs downward: three
        // panels fit, each fully on screen.
        assert_eq!(origin(1), (20.0, top - SIZE.1 - STACK_GAP));
        assert_eq!(origin(2), (20.0, top - 2.0 * (SIZE.1 + STACK_GAP)));
        // The anchor is in the left half, so the next column is to the right.
        assert_eq!(origin(3), (20.0 + SIZE.0 + STACK_GAP, top));
        for slot in 0..4 {
            let (x, y) = origin(slot);
            assert!(x >= 0.0 && x + SIZE.0 <= VISIBLE.w, "slot {slot} x {x}");
            assert!(y >= 0.0 && y + SIZE.1 <= VISIBLE.h, "slot {slot} y {y}");
        }
    }

    #[test]
    fn slots_never_overlap_even_for_an_off_screen_anchor() {
        let screen = Area {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        };
        let visible = Area {
            x: 0.0,
            y: 70.0,
            w: 1440.0,
            h: 805.0,
        };
        for anchor in [
            Some((0, 0)),
            Some((-500, -500)),
            Some((5000, 5000)),
            Some((1400, 0)),
            Some((0, 880)),
            None,
        ] {
            let bottom_right = first_slot_bottom_right(screen, visible, SIZE, anchor);
            let origins: Vec<_> = (0..=20)
                .map(|slot| stack_origin(bottom_right, visible, SIZE, slot))
                .collect();
            // Past the grid, slots repeat the last one; before it, all differ.
            let last = origins
                .iter()
                .position(|o| o == origins.last().unwrap())
                .unwrap();
            for (i, a) in origins[..=last].iter().enumerate() {
                for b in &origins[i + 1..=last] {
                    let apart = (a.0 - b.0).abs() >= SIZE.0 || (a.1 - b.1).abs() >= SIZE.1;
                    assert!(apart, "{anchor:?}: {a:?} overlaps {b:?}");
                }
            }
        }
        // The reported case: 320x200+0+0 puts slot 0 at the top-left corner
        // and slot 1 a full panel plus gap below it.
        let bottom_right = first_slot_bottom_right(screen, visible, SIZE, Some((0, 0)));
        let slot0 = stack_origin(bottom_right, visible, SIZE, 0);
        let slot1 = stack_origin(bottom_right, visible, SIZE, 1);
        assert_eq!(slot0, (0.0, 875.0 - 258.0));
        assert_eq!(slot1, (0.0, slot0.1 - 258.0 - STACK_GAP));
    }

    #[test]
    fn every_slot_stays_inside_the_visible_frame() {
        // Default and anchored placements on screens from barely one panel
        // to large; slots far beyond the grid overlap its last slot.
        for (w, h) in [
            (340.0, 262.0),
            (400.0, 300.0),
            (700.0, 600.0),
            (1440.0, 875.0),
        ] {
            let visible = Area {
                x: 0.0,
                y: 70.0,
                w,
                h,
            };
            let screen = Area {
                y: 0.0,
                h: h + 95.0,
                ..visible
            };
            for anchor in [None, Some((20, 40)), Some((5000, 5000))] {
                let bottom_right = first_slot_bottom_right(screen, visible, SIZE, anchor);
                for slot in 0..=20 {
                    let (x, y) = stack_origin(bottom_right, visible, SIZE, slot);
                    assert!(
                        x >= visible.x && x + SIZE.0 <= visible.x + visible.w,
                        "{w}x{h} {anchor:?} slot {slot}: x {x}"
                    );
                    assert!(
                        y >= visible.y && y + SIZE.1 <= visible.y + visible.h,
                        "{w}x{h} {anchor:?} slot {slot}: y {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn placement_keeps_the_chip_column_on_screen_and_clear_of_other_columns() {
        let screen = Area {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        };
        let visible = Area {
            x: 0.0,
            y: 70.0,
            w: 1440.0,
            h: 805.0,
        };
        for anchor in [None, Some((0, 0)), Some((20, 40)), Some((-500, 0))] {
            let origins: Vec<_> = (0..12)
                .map(|slot| panel_origin(screen, visible, SIZE, anchor, slot))
                .collect();
            for (slot, &(x, y)) in origins.iter().enumerate() {
                // Chips reach CHIP_REACH left of the window: still on screen.
                assert!(x - CHIP_REACH >= visible.x, "{anchor:?} slot {slot}: {x}");
                assert!(
                    x + SIZE.0 <= visible.x + visible.w,
                    "{anchor:?} slot {slot}"
                );
                assert!(y >= visible.y && y + SIZE.1 <= visible.y + visible.h);
            }
            // Distinct slots' footprints (chips included) never overlap.
            let last = origins
                .iter()
                .position(|o| o == origins.last().unwrap())
                .unwrap();
            for (i, a) in origins[..=last].iter().enumerate() {
                for b in &origins[i + 1..=last] {
                    let apart =
                        (a.0 - b.0).abs() >= SIZE.0 + CHIP_REACH || (a.1 - b.1).abs() >= SIZE.1;
                    assert!(apart, "{anchor:?}: {a:?} and {b:?} overlap");
                }
            }
        }
        // The default corner still puts the window's right edge at the inset.
        let (x, _) = panel_origin(screen, visible, SIZE, None, 0);
        assert_eq!(x + SIZE.0, visible.w - EDGE_INSET);
    }

    #[test]
    fn panels_show_only_for_active_undismissed_sessions_whose_window_is_not_in_view() {
        assert!(panel_should_show(true, false, false));
        assert!(!panel_should_show(false, false, false)); // idle
        assert!(!panel_should_show(true, true, false)); // closed with x
        assert!(!panel_should_show(true, false, true)); // window fully visible
    }

    #[test]
    fn free_slot_reuses_the_lowest_gap() {
        assert_eq!(free_slot([]), 0);
        assert_eq!(free_slot([0, 1, 2]), 3);
        assert_eq!(free_slot([0, 2]), 1);
    }

    #[test]
    fn a_hidden_panels_slot_is_reused_by_the_next_shown_panel() {
        // alpha is hidden (holds nothing); beta is shown and takes slot 0.
        assert_eq!(slot_on_show([], false), Some(0));
        // With alpha shown at 0, beta takes 1.
        assert_eq!(slot_on_show([0], false), Some(1));
    }

    #[test]
    fn re_showing_an_undragged_panel_takes_the_lowest_free_slot() {
        // It held slot 0 before; meanwhile 0 was taken and 1 freed.
        assert_eq!(slot_on_show([0, 2], false), Some(1));
        assert_eq!(slot_on_show([1, 2], false), Some(0));
    }

    #[test]
    fn a_dragged_panel_keeps_its_position_and_takes_no_slot() {
        assert_eq!(slot_on_show([], true), None);
        assert_eq!(slot_on_show([0, 1], true), None);
        assert!(moved_from((100.0, 50.0), (1088.0, 86.0)));
        assert!(!moved_from((1088.2, 86.0), (1088.0, 86.0)));
    }

    #[test]
    fn idle_hide_after_eight_quiet_seconds() {
        let start = Instant::now();
        assert!(!idle_hide_due(start, start));
        assert!(!idle_hide_due(start, start + Duration::from_millis(7_999)));
        assert!(idle_hide_due(start, start + IDLE_HIDE_AFTER));
        // A newer frame than `now` is never idle.
        assert!(!idle_hide_due(start + Duration::from_secs(1), start));
    }

    #[test]
    fn short_key_drops_runtime_namespace() {
        assert_eq!(
            short_key("__cua_runtime_0123456789abcdef0123456789abcdef:research-run-long"),
            "research-run"
        );
        assert_eq!(short_key("default"), "default");
    }

    fn frame(session: &str, label: &str) -> PipFrame {
        PipFrame {
            action_label: label.into(),
            timestamp_ms: 0,
            session_key: session.into(),
            session_label: None,
            client_name: None,
            client_pid: None,
            target_pid: Some(42),
            target_window_id: Some(7),
        }
    }

    type Delivered = mpsc::Receiver<(String, u64, Option<Shot>)>;

    fn worker(capture: Arc<CaptureFn>, timeout: Duration) -> (Arc<CaptureWorker>, Delivered) {
        let (sender, delivered) = mpsc::channel();
        let sender = Mutex::new(sender);
        let worker = CaptureWorker::start(capture, timeout, move |frame, epoch, png| {
            let _ = lock(&sender).send((frame.action_label, epoch, png));
        })
        .unwrap();
        (worker, delivered)
    }

    /// Next delivery as (action label, png), ignoring the epoch.
    fn recv(delivered: &Delivered) -> (String, Option<Shot>) {
        let (label, _, png) = delivered.recv_timeout(Duration::from_secs(5)).unwrap();
        (label, png)
    }

    #[test]
    fn frames_apply_only_in_their_sessions_current_epoch() {
        assert!(should_apply(3, Some(3)));
        assert!(!should_apply(3, Some(4)));
        assert!(!should_apply(3, None));

        let mut epochs = SessionEpochs::default();
        let first = epochs.stamp("s");
        assert_eq!(epochs.stamp("s"), first);
        epochs.end("s");
        assert_eq!(epochs.current("s"), None);
        let restarted = epochs.stamp("s");
        assert_ne!(restarted, first);
        assert!(!should_apply(first, epochs.current("s")));
        assert!(should_apply(restarted, epochs.current("s")));
    }

    #[test]
    fn a_capture_in_flight_when_its_session_ends_is_discarded() {
        let (started_tx, started) = mpsc::channel::<()>();
        let (release, release_rx) = mpsc::channel::<()>();
        let (started_tx, release_rx) = (Mutex::new(started_tx), Mutex::new(release_rx));
        let capture: Arc<CaptureFn> = Arc::new(move |_| {
            let _ = lock(&started_tx).send(());
            let _ = lock(&release_rx).recv();
            Some((7, vec![1]))
        });
        let (worker, delivered) = worker(capture, Duration::from_secs(5));

        worker.push(frame("s", "before end"));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.forget("s"); // end_session while the capture runs
        release.send(()).unwrap();
        let (label, epoch, _) = delivered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(label, "before end");
        assert!(!worker.is_current("s", epoch));

        // The same key starting again gets a fresh epoch that does apply.
        worker.push(frame("s", "restarted"));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        let (label, epoch, _) = delivered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(label, "restarted");
        assert!(worker.is_current("s", epoch));
    }

    const W5: Tag = (Some(42), Some(5));
    const W6: Tag = (Some(42), Some(6));
    const SHOW_LIVE: Layers = Layers {
        show_still: false,
        show_live: true,
        show_placeholder: false,
    };
    const SHOW_STILL: Layers = Layers {
        show_still: true,
        show_live: false,
        show_placeholder: false,
    };
    const SHOW_NOTHING: Layers = Layers {
        show_still: false,
        show_live: false,
        show_placeholder: true,
    };

    #[test]
    fn the_current_tag_resolves_a_pid_only_target() {
        assert_eq!(current_tag((Some(42), Some(5)), None), Some(W5));
        assert_eq!(current_tag((Some(42), None), Some(6)), Some(W6));
        assert_eq!(current_tag((Some(42), None), None), None);
        assert_eq!(current_tag((None, None), Some(6)), None);
    }

    #[test]
    fn only_pixels_of_the_current_target_are_shown() {
        // Matching live only: live.
        assert_eq!(visible_layers(Some(W5), None, Some(W5)), SHOW_LIVE);
        // Live from another window: not shown.
        assert_eq!(visible_layers(Some(W6), None, Some(W5)), SHOW_NOTHING);
        // Still from another window: placeholder.
        assert_eq!(visible_layers(Some(W6), Some(W5), None), SHOW_NOTHING);
        // Matching still alone: still.
        assert_eq!(visible_layers(Some(W5), Some(W5), None), SHOW_STILL);
        // Live over a matching still: live only, still hidden under it.
        assert_eq!(visible_layers(Some(W5), Some(W5), Some(W5)), SHOW_LIVE);
        // Stale live over a matching still: the still shows, not the live.
        assert_eq!(visible_layers(Some(W6), Some(W6), Some(W5)), SHOW_STILL);
        // Live matches but the still is stale (window reshaped): live only.
        assert_eq!(visible_layers(Some(W6), Some(W5), Some(W6)), SHOW_LIVE);
        // Nothing captured yet.
        assert_eq!(visible_layers(Some(W5), None, None), SHOW_NOTHING);
    }

    #[test]
    fn an_unresolved_target_shows_nothing_old() {
        assert_eq!(visible_layers(None, Some(W5), Some(W5)), SHOW_NOTHING);
    }

    #[test]
    fn re_showing_after_hide_with_a_different_target_waits_for_a_matching_capture() {
        // Hidden with W5's still and live retained; the next action targets
        // pid 43, whose window is unresolved, then window 9.
        let other = (Some(43), Some(9));
        assert_eq!(visible_layers(None, Some(W5), Some(W5)), SHOW_NOTHING);
        assert_eq!(
            visible_layers(Some(other), Some(W5), Some(W5)),
            SHOW_NOTHING
        );
        // Its own capture arrives and shows.
        assert_eq!(
            visible_layers(Some(other), Some(other), Some(W5)),
            SHOW_STILL
        );
    }

    #[test]
    fn captures_are_window_scoped_never_the_display() {
        let none = |_| None;
        assert_eq!(capture_window((None, None), |_| Some(9)), None);
        assert_eq!(capture_window((Some(42), None), none), None);
        assert_eq!(
            capture_window((Some(42), None), |pid| (pid == 42).then_some(9)),
            Some(9)
        );
        assert_eq!(capture_window((Some(42), Some(7)), |_| Some(9)), Some(7));
    }

    #[test]
    fn latest_per_session_keeps_only_the_newest_item() {
        let mut queue = LatestPerSession::new();
        queue.push("a".into(), 1);
        queue.push("b".into(), 2);
        queue.push("a".into(), 3);
        assert_eq!(queue.pop(), Some(("a".into(), 3)));
        assert_eq!(queue.pop(), Some(("b".into(), 2)));
        assert_eq!(queue.pop(), None);
        queue.push("c".into(), 4);
        queue.remove("c");
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn push_never_captures_and_captures_coalesce_to_the_latest_frame() {
        let (started_tx, started) = mpsc::channel::<()>();
        let (release, release_rx) = mpsc::channel::<()>();
        let (started_tx, release_rx) = (Mutex::new(started_tx), Mutex::new(release_rx));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let capture: Arc<CaptureFn> = Arc::new(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = lock(&started_tx).send(());
            let _ = lock(&release_rx).recv();
            Some((7, vec![1]))
        });
        let (worker, delivered) = worker(capture, Duration::from_secs(5));

        let pushing = Instant::now();
        worker.push(frame("s", "first"));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        // The capture of "first" is blocked; pushes still return at once.
        worker.push(frame("s", "second"));
        worker.push(frame("s", "third"));
        assert!(pushing.elapsed() < Duration::from_secs(1));

        release.send(()).unwrap();
        assert_eq!(recv(&delivered), ("first".to_owned(), Some((7, vec![1]))));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        assert_eq!(recv(&delivered), ("third".to_owned(), Some((7, vec![1]))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn polling_follows_the_displayed_target_across_a_click_and_an_in_flight_capture() {
        let (worker, _delivered) = worker(Arc::new(|_| None), Duration::from_secs(5));
        let polled = |worker: &CaptureWorker| worker.active_targets(Instant::now())[0].1;
        let a = (Some(42), Some(7));
        let (b, c) = ((Some(42), Some(8)), (Some(43), Some(9)));
        // The first frame sets the polled target.
        worker.push(frame("s", "act in A"));
        assert_eq!(polled(&worker), a);
        // The user clicks back card B: the panel shows B, polls follow.
        worker.retarget("s", b);
        assert_eq!(polled(&worker), b);
        // A new action's push does not move polling away from what the
        // panel shows while its capture runs.
        worker.push(PipFrame {
            target_pid: c.0,
            target_window_id: c.1,
            ..frame("s", "act in C")
        });
        assert_eq!(polled(&worker), b);
        // A capture of A that was in flight lands: the panel shows A again
        // and polling moves with it in the same step.
        worker.retarget("s", a);
        assert_eq!(polled(&worker), a);
    }

    #[test]
    fn polling_and_the_panel_share_one_activity_deadline() {
        let (worker, _delivered) = worker(Arc::new(|_| Some((7, vec![1]))), Duration::from_secs(5));
        worker.push(frame("s", "act"));
        let pushed = Instant::now();
        // Delivered 5 s after the push (a slow capture).
        let delivered = pushed + Duration::from_secs(5);
        worker.mark_delivered("s", delivered);
        // The panel hides 8 s after delivery; polling must run until then.
        let hide_at = delivered + IDLE_HIDE_AFTER;
        let before_hide = hide_at - Duration::from_millis(1);
        assert!(!idle_hide_due(delivered, before_hide));
        assert_eq!(worker.active_targets(before_hide).len(), 1);
        assert!(idle_hide_due(delivered, hide_at));
        assert!(worker.active_targets(hide_at).is_empty());
        // An older delivery never pulls the deadline back, and an ended
        // session is not resurrected.
        worker.mark_delivered("s", pushed);
        assert_eq!(worker.active_targets(before_hide).len(), 1);
        worker.forget("s");
        worker.mark_delivered("s", delivered);
        assert!(worker.active_targets(before_hide).is_empty());
    }

    #[test]
    fn a_stuck_capture_times_out_and_is_not_retried_while_stuck() {
        let (_hold, stuck) = mpsc::channel::<()>();
        let stuck = Mutex::new(stuck);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let capture: Arc<CaptureFn> = Arc::new(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = lock(&stuck).recv(); // never answered
            Some((7, vec![1]))
        });
        let (worker, delivered) = worker(capture, Duration::from_millis(50));

        worker.push(frame("s", "first"));
        assert_eq!(recv(&delivered), ("first".to_owned(), None));
        worker.push(frame("s", "second"));
        assert_eq!(recv(&delivered), ("second".to_owned(), None));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn client_names_reduce_to_an_app_token() {
        assert_eq!(client_match_token("Claude Code").as_deref(), Some("claude"));
        assert_eq!(
            client_match_token("codex-mcp-client").as_deref(),
            Some("codex")
        );
        assert_eq!(client_match_token("ab"), None);
    }
}
