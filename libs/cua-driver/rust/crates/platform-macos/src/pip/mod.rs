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
//!   it moves to the lowest free one. A panel the user dragged keeps its
//!   place and holds no slot, and an ended session's dragged position is
//!   remembered while the daemon runs.
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use pip_preview::{PipBackend, PipConfig, PipFrame};

mod live;
mod visibility;

use live::{Event, Request, StreamStep, Streams};

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
/// bottom-right corner sits at `bottom_right`; later slots stack upward, and
/// a full column wraps to a new column on the left. `visible_top` bounds a
/// column.
fn stack_origin(
    bottom_right: (f64, f64),
    visible_top: f64,
    size: (f64, f64),
    slot: usize,
) -> (f64, f64) {
    let (w, h) = size;
    let step = h + STACK_GAP;
    let room = visible_top - EDGE_INSET - bottom_right.1 + STACK_GAP;
    let per_column = ((room / step).floor() as usize).max(1);
    let (column, row) = (slot / per_column, slot % per_column);
    (
        bottom_right.0 - w - column as f64 * (w + STACK_GAP),
        bottom_right.1 + row as f64 * step,
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum ImageUpdate {
    Replace,
    Keep,
    /// Blank the image and show "Preview unavailable".
    Clear,
}

/// A failed capture keeps the old image only when it shows the same target;
/// otherwise the panel would show one window's pixels as another's preview.
fn image_after_capture(prev_target: Target, new_target: Target, capture_ok: bool) -> ImageUpdate {
    if capture_ok {
        ImageUpdate::Replace
    } else if prev_target == new_target {
        ImageUpdate::Keep
    } else {
        ImageUpdate::Clear
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

struct State {
    image_size: (f64, f64),
    anchor: Option<(i32, i32)>,
    panels: HashMap<String, Panel>,
    /// Last origin of each ended session's dragged panel, kept while the daemon runs.
    // ponytail: one small entry per ended session; cap it if a daemon ever
    // sees many thousands of sessions.
    remembered: HashMap<String, (f64, f64)>,
    next_id: i64,
    worker: Arc<CaptureWorker>,
    streams: Arc<Streams>,
    next_stream_generation: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map(f)
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
type CaptureFn = dyn Fn(Target) -> Option<Vec<u8>> + Send + Sync;

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
}

impl CaptureWorker {
    fn start(
        capture: Arc<CaptureFn>,
        timeout: Duration,
        deliver: impl Fn(PipFrame, u64, Option<Vec<u8>>) + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let worker = Arc::new(Self {
            queue: Mutex::new(LatestPerSession::new()),
            epochs: Mutex::new(SessionEpochs::default()),
            ready: Condvar::new(),
            capture,
            timeout,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            active: Mutex::new(HashMap::new()),
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
    fn push(&self, frame: PipFrame) {
        let target = (frame.target_pid, frame.target_window_id);
        lock(&self.active).insert(frame.session_key.clone(), (target, Instant::now()));
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
    }

    /// Sessions that pushed a frame within the idle window, with their
    /// latest target.
    fn active_targets(&self, now: Instant) -> Vec<(String, Target)> {
        lock(&self.active)
            .iter()
            .filter(|(_, (_, pushed))| !idle_hide_due(*pushed, now))
            .map(|(key, (target, _))| (key.clone(), *target))
            .collect()
    }

    fn is_current(&self, session_key: &str, epoch: u64) -> bool {
        should_apply(epoch, lock(&self.epochs).current(session_key))
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
    fn capture_bounded(&self, target: Target) -> Option<Vec<u8>> {
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
    png: Option<Vec<u8>>,
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
}

impl PipBackend for MacosPipBackend {
    fn push_frame(&self, frame: PipFrame) {
        self.worker.push(frame);
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
fn deliver_to_main(frame: PipFrame, epoch: u64, png: Option<Vec<u8>>) {
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
        for (key, target) in active {
            let visible = visibility::target_fully_visible(target, &windows, &displays, own_pid);
            let resolved_window = resolve_target_window(target);
            dispatch_to_main(
                VisibilityUpdate {
                    key,
                    target,
                    visible,
                    resolved_window,
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
        remembered: HashMap::new(),
        next_id: 1,
        worker: worker.clone(),
        streams,
        next_stream_generation: 0,
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
    let Some(panel) = state.panels.get_mut(&key) else {
        return;
    };

    let new_target = (frame.target_pid, frame.target_window_id);
    let image_view = panel.image_view as *mut AnyObject;
    match image_after_capture(panel.target, new_target, png.is_some()) {
        ImageUpdate::Replace => {
            // `dataWithBytes:length:` copies, so the Vec can drop.
            let png = png.unwrap_or_default();
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
            if !image.is_null() {
                let _: () = msg_send![image, release];
            }
        }
        ImageUpdate::Keep => {}
        ImageUpdate::Clear => {
            let _: () = msg_send![image_view, setImage: std::ptr::null_mut::<AnyObject>()];
        }
    }
    sync_placeholder(panel);
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
        layout_header(panel);
        panel.client = Some(client);
    }

    // Where: target app icon + window title.
    let app: *mut AnyObject = match frame.target_pid {
        Some(pid) => msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ],
        None => std::ptr::null_mut(),
    };
    let app_icon: *mut AnyObject = if app.is_null() {
        std::ptr::null_mut()
    } else {
        msg_send![app, icon]
    };
    let _: () = msg_send![panel.target_icon as *mut AnyObject, setImage: app_icon];
    let title = target_title.or_else(|| {
        if app.is_null() {
            return None;
        }
        let name: *mut AnyObject = msg_send![app, localizedName];
        ns_to_string(name)
    });
    set_text(panel.target_title, title.as_deref().unwrap_or(""));
    panel.target = new_target;
    panel.target_visible = target_visible;
    panel.resolved_window = resolved_window;
    panel.dismissed = false;
    panel.last_frame = Instant::now();
    refresh(state, &key);
    // Small slack so the monotonic check in the callback is past the bar.
    dispatch_to_main_after(
        IDLE_HIDE_AFTER + Duration::from_millis(20),
        key,
        idle_check_cb,
    );
}

unsafe extern "C" fn idle_check_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| refresh(state, &key));
}

unsafe extern "C" fn visibility_cb(ctx: *mut c_void) {
    let update: VisibilityUpdate = *Box::from_raw(ctx as *mut VisibilityUpdate);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| {
            let Some(panel) = state.panels.get_mut(&update.key) else {
                return;
            };
            // An answer about an older target, or no change: nothing to do.
            if panel.target != update.target
                || (panel.target_visible == update.visible
                    && panel.resolved_window == update.resolved_window)
            {
                return;
            }
            panel.target_visible = update.visible;
            panel.resolved_window = update.resolved_window;
            refresh(state, &update.key);
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
                    return;
                }
                if let Some(frame) = lock(&slot).take() {
                    show_live(panel, frame);
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
                if !panel.stream.end(generation) {
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
        streams,
        next_stream_generation,
        image_size,
        anchor,
        ..
    } = state;
    let others: Vec<usize> = panels
        .iter()
        .filter(|(other, _)| other.as_str() != key)
        .filter_map(|(_, panel)| panel.slot)
        .collect();
    let Some(panel) = panels.get_mut(key) else {
        return;
    };
    let active = !idle_hide_due(panel.last_frame, Instant::now());
    if panel_should_show(active, panel.dismissed, panel.target_visible) {
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
            panel.stream.begin(target, *next_stream_generation);
            // Never show one window's live pixels as another's preview.
            clear_live(panel);
            streams.request(
                key,
                Request::Start {
                    generation: *next_stream_generation,
                    target,
                    well: *image_size,
                },
            );
        }
        StreamStep::Stop => {
            // The last live frame stays up while the panel fades out.
            panel.stream.stop();
            streams.request(key, Request::Stop);
        }
        StreamStep::Keep => {}
    }
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
    let Some(origin) = slot_origin(image_size, anchor, slot) else {
        return;
    };
    let _: () = msg_send![window, setFrameOrigin: NSPoint::new(origin.0, origin.1)];
    panel.placed = origin;
}

/// Put a live frame's IOSurface on the live layer.
unsafe fn show_live(panel: &mut Panel, frame: screencapturekit::CVPixelBuffer) {
    let Some(surface) = frame.io_surface() else {
        return;
    };
    let _: () = msg_send![class!(CATransaction), begin];
    // No implicit cross-fade between frames.
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![
        panel.live_layer as *mut AnyObject,
        setContents: surface.as_ptr() as *mut AnyObject
    ];
    let _: () = msg_send![class!(CATransaction), commit];
    let _: () = msg_send![panel.live_view as *mut AnyObject, setHidden: false];
    // Replacing the previous frame releases it back to ScreenCaptureKit.
    panel.live_frame = Some(frame);
    sync_placeholder(panel);
}

/// Remove the live frame so the still screenshot shows again.
unsafe fn clear_live(panel: &mut Panel) {
    let Some(frame) = panel.live_frame.take() else {
        return;
    };
    let _: () = msg_send![panel.live_view as *mut AnyObject, setHidden: true];
    let _: () = msg_send![
        panel.live_layer as *mut AnyObject,
        setContents: std::ptr::null_mut::<AnyObject>()
    ];
    drop(frame);
    sync_placeholder(panel);
}

/// "Preview unavailable" shows only when neither a live frame nor a still
/// is on screen.
unsafe fn sync_placeholder(panel: &Panel) {
    let image: *mut AnyObject = msg_send![panel.image_view as *mut AnyObject, image];
    let covered = panel.live_frame.is_some() || !image.is_null();
    let _: () = msg_send![panel.placeholder as *mut AnyObject, setHidden: covered];
}

/// Fade completion: order the panel out unless a frame re-showed it.
unsafe extern "C" fn order_out_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| {
        if let Some(panel) = state.panels.get(&key) {
            if !panel.shown {
                let _: () = msg_send![
                    panel.window as *mut AnyObject,
                    orderOut: std::ptr::null_mut::<AnyObject>()
                ];
            }
        }
    });
}

unsafe extern "C" fn end_session_cb(ctx: *mut c_void) {
    let key: String = *Box::from_raw(ctx as *mut String);
    with_state(|state| {
        let Some(panel) = state.panels.remove(&key) else {
            return;
        };
        state.streams.request(&key, Request::Stop);
        let frame: NSRect = msg_send![panel.window as *mut AnyObject, frame];
        let here = (frame.origin.x, frame.origin.y);
        if panel.dragged || moved_from(here, panel.placed) {
            state.remembered.insert(key, here);
        }
        animate_alpha(panel.window, 0.0);
        dispatch_to_main_after(FADE, panel.window, close_window_cb);
    });
}

unsafe extern "C" fn close_window_cb(ctx: *mut c_void) {
    let window = *Box::from_raw(ctx as *mut usize) as *mut AnyObject;
    close_window(window);
}

unsafe extern "C" fn shutdown_cb(_ctx: *mut c_void) {
    let panels = with_state(|state| {
        for key in state.panels.keys() {
            state.streams.request(key, Request::Stop);
        }
        std::mem::take(&mut state.panels)
    })
    .unwrap_or_default();
    for panel in panels.into_values() {
        close_window(panel.window as *mut AnyObject);
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
}

unsafe fn hide(panel: &mut Panel, key: &str) {
    if !panel.shown {
        return;
    }
    panel.shown = false;
    animate_alpha(panel.window, 0.0);
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
    let target = with_state(|state| {
        state
            .panels
            .values()
            .find(|panel| panel.id == id)
            .map(|panel| panel.target)
    })
    .flatten();
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

/// Panel size in points for an image well of `image_size`.
fn panel_size((image_w, image_h): (f64, f64)) -> (f64, f64) {
    (
        image_w + 2.0 * PAD,
        HEADER_HEIGHT + 4.0 + image_h + 4.0 + STATUS_HEIGHT + 6.0,
    )
}

/// AppKit origin of cascade `slot` on the main screen; `None` when there is
/// no screen (headless).
unsafe fn slot_origin(
    image_size: (f64, f64),
    anchor: Option<(i32, i32)>,
    slot: usize,
) -> Option<(f64, f64)> {
    let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
    if screen.is_null() {
        return None;
    }
    let screen_frame: NSRect = msg_send![screen, frame];
    let visible_frame: NSRect = msg_send![screen, visibleFrame];
    let area = |r: NSRect| Area {
        x: r.origin.x,
        y: r.origin.y,
        w: r.size.width,
        h: r.size.height,
    };
    let size = panel_size(image_size);
    let bottom_right =
        first_slot_bottom_right(area(screen_frame), area(visible_frame), size, anchor);
    let visible_top = visible_frame.origin.y + visible_frame.size.height;
    Some(stack_origin(bottom_right, visible_top, size, slot))
}

unsafe fn create_panel(state: &mut State, key: &str, label: Option<&str>) -> Option<Panel> {
    let (image_w, image_h) = state.image_size;
    let (width, height) = panel_size(state.image_size);
    // A remembered (dragged) origin is kept; any other panel starts at slot 0
    // and moves to its real slot when it is shown.
    let remembered = state.remembered.get(key).copied();
    let origin = match remembered {
        Some(origin) => origin,
        None => slot_origin(state.image_size, state.anchor, 0)?, // None: headless (CI)
    };
    let rect = NSRect::new(NSPoint::new(origin.0, origin.1), NSSize::new(width, height));

    // NSWindowStyleMaskBorderless (0) | NSWindowStyleMaskNonactivatingPanel (1 << 7)
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
    let _: () = msg_send![window, setMovableByWindowBackground: true];
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![window, setBackgroundColor: clear];
    let _: () = msg_send![window, setOpaque: false];
    let _: () = msg_send![window, setHasShadow: true];
    let title = format!(
        "cua PiP · {}",
        label.map(str::to_owned).unwrap_or_else(|| short_key(key))
    );
    let _: () = msg_send![window, setTitle: ns_string(&title)];

    let [r, g, b, _] = cursor_overlay::session_fill_rgba(key);
    let session_color = |alpha: f64| -> *mut AnyObject {
        msg_send![
            class!(NSColor),
            colorWithSRGBRed: r as f64 / 255.0
            green: g as f64 / 255.0
            blue: b as f64 / 255.0
            alpha: alpha
        ]
    };

    // Content view: rounded, with the session-colored border. A layer's
    // border composites above its sublayers, so it rims the glass.
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    let content_view: *mut AnyObject = msg_send![window, contentView];
    let _: () = msg_send![content_view, setWantsLayer: true];
    let content_layer: *mut AnyObject = msg_send![content_view, layer];
    let _: () = msg_send![content_layer, setCornerRadius: CORNER_RADIUS];
    let _: () = msg_send![content_layer, setBorderWidth: BORDER_WIDTH];
    let border_cg: *mut CGColor = msg_send![session_color(1.0), CGColor];
    let _: () = msg_send![content_layer, setBorderColor: border_cg];

    // Everything visible sits in `body`, hosted by the glass background.
    let body = new_view(class!(NSView), bounds);
    let background = glass_background(bounds, body);
    add_subview(content_view, background);

    // Header strip: a faint wash of the session color inside the glass.
    let header = new_view(
        class!(NSView),
        NSRect::new(
            NSPoint::new(0.0, height - HEADER_HEIGHT),
            NSSize::new(width, HEADER_HEIGHT),
        ),
    );
    let _: () = msg_send![header, setWantsLayer: true];
    let header_layer: *mut AnyObject = msg_send![header, layer];
    let wash_cg: *mut CGColor = msg_send![session_color(0.16), CGColor];
    let _: () = msg_send![header_layer, setBackgroundColor: wash_cg];

    let icon_y = (HEADER_HEIGHT - 16.0) / 2.0;
    let client_icon = new_icon_view(NSRect::new(
        NSPoint::new(10.0, icon_y),
        NSSize::new(16.0, 16.0),
    ));
    let client_label = new_label(
        NSRect::new(NSPoint::new(32.0, icon_y), NSSize::new(110.0, 16.0)),
        12.0,
        0.23, // NSFontWeightMedium
        false,
    );
    let close_x = width - 8.0 - 20.0;
    let focus_x = close_x - 2.0 - 20.0;
    let id = state.next_id;
    state.next_id += 1;
    let focus = new_button(
        "arrow.up.forward.app",
        "Bring this window forward",
        sel!(pipFocus:),
        id,
        NSRect::new(NSPoint::new(focus_x, 4.0), NSSize::new(20.0, 20.0)),
    );
    let close = new_button(
        "xmark",
        "Hide until this agent's next action",
        sel!(pipHide:),
        id,
        NSRect::new(NSPoint::new(close_x, 4.0), NSSize::new(20.0, 20.0)),
    );
    let target_icon = new_icon_view(NSRect::new(
        NSPoint::new(150.0, icon_y),
        NSSize::new(16.0, 16.0),
    ));
    let target_title = new_label(
        NSRect::new(
            NSPoint::new(170.0, icon_y),
            NSSize::new(focus_x - 4.0 - 170.0, 16.0),
        ),
        11.0,
        0.0,
        true,
    );
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
    add_subview(body, header);

    // Screenshot well: the latest still.
    let image_view = new_view(
        class!(NSImageView),
        NSRect::new(
            NSPoint::new(PAD, 6.0 + STATUS_HEIGHT + 4.0),
            NSSize::new(image_w, image_h),
        ),
    );
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
    let live_view = new_view(
        class!(NSView),
        NSRect::new(
            NSPoint::new(PAD, 6.0 + STATUS_HEIGHT + 4.0),
            NSSize::new(image_w, image_h),
        ),
    );
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
    let _: () = msg_send![placeholder, sizeToFit];
    let fitted: NSRect = msg_send![placeholder, frame];
    let (text_w, text_h) = (fitted.size.width.min(image_w), fitted.size.height);
    let _: () = msg_send![
        placeholder,
        setFrame: NSRect::new(
            NSPoint::new(
                PAD + (image_w - text_w) / 2.0,
                6.0 + STATUS_HEIGHT + 4.0 + (image_h - text_h) / 2.0,
            ),
            NSSize::new(text_w, text_h),
        )
    ];
    let _: () = msg_send![placeholder, setHidden: true];
    let _: () = msg_send![body, addSubview: placeholder];

    let status = new_label(
        NSRect::new(
            NSPoint::new(PAD + 2.0, 6.0),
            NSSize::new(image_w - 4.0, STATUS_HEIGHT),
        ),
        11.0,
        0.0,
        true,
    );
    let _: () = msg_send![body, addSubview: status];

    Some(Panel {
        id,
        window: window as usize,
        image_view: image_view as usize,
        live_view: live_view as usize,
        live_layer: live_layer as usize,
        live_frame: None,
        stream: live::StreamState::default(),
        resolved_window: None,
        placeholder: placeholder as usize,
        status: status as usize,
        client_icon: client_icon as usize,
        client_label: client_label as usize,
        target_icon: target_icon as usize,
        target_title: target_title as usize,
        slot: None,
        dragged: remembered.is_some(),
        placed: origin,
        last_frame: Instant::now(),
        shown: false,
        dismissed: false,
        target_visible: false,
        target: (None, None),
        client: None,
    })
}

/// Liquid Glass (`NSGlassEffectView`, macOS 26) hosting `body`, or an
/// `NSVisualEffectView` HUD material on older systems. Rounded either way.
/// Takes ownership of `body`; returns an owned (+1) view.
unsafe fn glass_background(bounds: NSRect, body: *mut AnyObject) -> *mut AnyObject {
    // Width + height sizable, so the body tracks the background.
    let _: () = msg_send![body, setAutoresizingMask: 18u64];
    if let Some(glass_class) = AnyClass::get("NSGlassEffectView") {
        let glass = new_view(glass_class, bounds);
        let _: () = msg_send![glass, setCornerRadius: CORNER_RADIUS];
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
    let _: () = msg_send![layer, setCornerRadius: CORNER_RADIUS];
    let _: () = msg_send![layer, setMasksToBounds: true];
    add_subview(effect, body);
    effect
}

/// Size the client label to its text and put the target group right after
/// it, keeping the title's right edge where it was.
unsafe fn layout_header(panel: &Panel) {
    let y = (HEADER_HEIGHT - 16.0) / 2.0;
    let label = panel.client_label as *mut AnyObject;
    let _: () = msg_send![label, sizeToFit];
    let fitted: NSRect = msg_send![label, frame];
    let label_w = fitted.size.width.min(120.0);
    let _: () = msg_send![
        label,
        setFrame: NSRect::new(NSPoint::new(32.0, y), NSSize::new(label_w, 16.0))
    ];
    let title = panel.target_title as *mut AnyObject;
    let title_frame: NSRect = msg_send![title, frame];
    let right = title_frame.origin.x + title_frame.size.width;
    let icon_x = 32.0 + label_w + 10.0;
    let _: () = msg_send![
        panel.target_icon as *mut AnyObject,
        setFrame: NSRect::new(NSPoint::new(icon_x, y), NSSize::new(16.0, 16.0))
    ];
    let title_x = icon_x + 20.0;
    let _: () = msg_send![
        title,
        setFrame: NSRect::new(
            NSPoint::new(title_x, y),
            NSSize::new((right - title_x).max(0.0), 16.0)
        )
    ];
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
            stack_origin(bottom_right, 875.0, SIZE, 0),
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
        let top = visible.y + visible.h;
        let slot0 = stack_origin(bottom_right, top, SIZE, 0);
        let slot1 = stack_origin(bottom_right, top, SIZE, 1);
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
            .map(|slot| stack_origin(bottom_right, top, SIZE, slot))
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
        assert_eq!(
            stack_origin(bottom_right, 875.0, SIZE, 0),
            (20.0, 900.0 - 40.0 - 258.0)
        );
        // No room above the anchor: the next panel wraps left, never off the top.
        let second = stack_origin(bottom_right, 875.0, SIZE, 1);
        assert_eq!(second, (20.0 - SIZE.0 - STACK_GAP, 900.0 - 40.0 - 258.0));
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

    type Delivered = mpsc::Receiver<(String, u64, Option<Vec<u8>>)>;

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
    fn recv(delivered: &Delivered) -> (String, Option<Vec<u8>>) {
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
            Some(vec![1])
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

    #[test]
    fn a_failed_capture_keeps_the_image_only_for_the_same_window() {
        let a = (Some(42), Some(7));
        let b = (Some(42), Some(8));
        assert_eq!(image_after_capture(a, a, true), ImageUpdate::Replace);
        assert_eq!(image_after_capture(a, b, true), ImageUpdate::Replace);
        assert_eq!(image_after_capture(a, a, false), ImageUpdate::Keep);
        assert_eq!(image_after_capture(a, b, false), ImageUpdate::Clear);
        // A new panel whose first capture fails shows the placeholder.
        assert_eq!(
            image_after_capture((None, None), a, false),
            ImageUpdate::Clear
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
            Some(vec![1])
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
        assert_eq!(recv(&delivered), ("first".to_owned(), Some(vec![1])));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        assert_eq!(recv(&delivered), ("third".to_owned(), Some(vec![1])));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
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
            Some(vec![1])
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
