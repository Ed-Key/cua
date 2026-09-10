//! macOS agent-cursor overlay — transparent click-through NSWindow.
//!
//! ## Architecture
//!
//! The MCP/tokio server runs on a **background thread** (spawned in
//! `cua-driver/src/main.rs`).  AppKit MUST run on the **main thread**.
//! The two sides communicate through a bounded process-global channel:
//!
//! - MCP tool calls → `send_command(OverlayCommand)` → `CMD_TX` (SyncSender)
//! - render worker → per-display pixmaps → main-thread `AppKitOverlayHost`
//!
//! One shared render map owns cursor state. The AppKit host owns one transparent
//! presentation surface per active display and replaces that surface set after
//! display-layout changes.
//!
//! ## Coordinate system
//!
//! All coordinates are **screen points** with the **top-left origin**
//! (matching `OverlayCommand::MoveTo` and AX element coordinates).  The
//! Display geometry records both CoreGraphics' global top-left frame and the
//! corresponding AppKit bottom-left window frame. Rendering subtracts each
//! display's global origin and uses that display's backing scale.
//!
//! ## Cross-platform note (2026-05 dedup audit)
//!
//! Animation state + render pipeline live in `cursor_overlay::render_state`
//! (`RenderStateCore`, `tick_swift_constants`, `apply_command_base`,
//! `render_frame`).  macOS uses the hardcoded Swift reference constants
//! (peakSpeed=900, springK=400, overshoot=0.8) and first-placement
//! variants of MoveTo / ClickPulse — see the wrapper around
//! `apply_command_base` below.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cursor_overlay::{
    CursorConfig, CursorKey, FocusRect, KeyedOverlayCommand, MotionConfig, OverlayCommand,
    OverlayMsg, RenderStateCore,
};
use indexmap::IndexMap;

use super::display_layout::{DisplayGeometry, DisplayId, DisplayLayout};

// ── Arrival-signal channels (one waiter slot per cursor key) ──────────────
//
// Each session's `animate_cursor_to` registers an arrival oneshot keyed by its
// own cursor key. A new animation only supersedes the SAME key's prior waiter,
// so concurrent sessions never cross-cancel each other's arrivals.

static ARRIVAL_TX: Mutex<Option<HashMap<CursorKey, tokio::sync::oneshot::Sender<()>>>> =
    Mutex::new(None);

fn arrival_register(key: CursorKey, tx: tokio::sync::oneshot::Sender<()>) {
    let mut guard = ARRIVAL_TX.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    // Cancel only the same key's previous waiter (superseded by new animation).
    if let Some(old_tx) = map.insert(key, tx) {
        let _ = old_tx.send(());
    }
}

fn arrival_fire(key: &CursorKey) {
    if let Ok(mut guard) = ARRIVAL_TX.lock() {
        if let Some(map) = guard.as_mut() {
            if let Some(tx) = map.remove(key) {
                let _ = tx.send(());
            }
        }
    }
}

// ── Global overlay state ──────────────────────────────────────────────────

enum MacOverlayMsg {
    Cursor(OverlayMsg),
    LayoutChanged,
}

static CMD_TX: OnceLock<std::sync::mpsc::SyncSender<MacOverlayMsg>> = OnceLock::new();
// Single-consumer slot; receiver is moved into run_on_main_thread().
static CMD_RX_CELL: Mutex<Option<std::sync::mpsc::Receiver<MacOverlayMsg>>> = Mutex::new(None);
static RENDER: Mutex<Option<RenderMap>> = Mutex::new(None);
static HOST: Mutex<Option<AppKitOverlayHost>> = Mutex::new(None);
static DISPLAY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The keyed, insertion-ordered collection of owned cursors that the render
/// loop composites every frame. Insertion order = stable z-order (later keys
/// paint on top). Display geometry is explicit and replaceable; cursor state
/// never owns an AppKit window or assumes a main-screen origin.
struct RenderMap {
    cursors: IndexMap<CursorKey, RenderState>,
    layout: DisplayLayout,
    /// Accepted command order, oldest first, separate from stable paint order.
    /// Each surface uses the latest commanded visible session painting there.
    /// Sessions on one display share this native order; their stored pin IDs
    /// remain independent, but one surface cannot stack above two windows separately.
    command_order: Vec<CursorKey>,
    /// Frozen launch-time config used as the template for lazily-created cursors.
    template: CursorConfig,
    /// Render-side tombstone of ended session cursor keys. A `Cmd`
    /// for a key in here is dropped WITHOUT get-or-create, so an in-flight
    /// click/move from another task that lands AFTER the owning session's
    /// `Remove` can never resurrect the just-removed cursor (the ghost-cursor
    /// resurrection race). An explicit owner-checked `start_session` revival
    /// clears this tombstone before the cursor is reused. "default" is never
    /// tombstoned.
    ended: HashSet<CursorKey>,
}

/// Build the `RenderState` for a lazily-created session cursor from the
/// process launch template.
fn render_state_for_key(template: &CursorConfig, key: &str) -> RenderState {
    let mut config = template.clone();
    config.cursor_id = key.to_owned();
    RenderState::new(config)
}

/// Apply one inbound [`OverlayMsg`] to the render map (drain step). Factored
/// out as a pure function so the per-session ownership + removal lifecycle is
/// unit-testable without AppKit.
///
fn apply_msg(map: &mut RenderMap, msg: OverlayMsg) {
    match msg {
        OverlayMsg::Remove(key) => {
            // The "default" cursor backs the anonymous / one-shot path and
            // must survive every session_end + the daemon lifetime.
            if key != "default" {
                map.cursors.shift_remove(&key);
                map.command_order.retain(|candidate| candidate != &key);
                if let Ok(mut guard) = ARRIVAL_TX.lock() {
                    if let Some(m) = guard.as_mut() {
                        m.remove(&key);
                    }
                }
                // Tombstone the key so a late in-flight Cmd from another task
                // (an animate/click racing the owning session's death) cannot
                // re-create the just-removed cursor. Never tombstone "default".
                map.ended.insert(key);
            }
        }
        OverlayMsg::Revive(key) => {
            if key != "default" {
                map.ended.remove(&key);
            }
        }
        OverlayMsg::Cmd(KeyedOverlayCommand { key, cmd }) => {
            // Drop a command for an already-ended session WITHOUT get-or-create
            // — this is the resurrection guard. Without it, a ClickPulse/MoveTo
            // landing after Remove would re-insert (and re-leak) the cursor.
            if key.is_empty() || map.ended.contains(&key) {
                arrival_fire(&key);
                return;
            }
            let target = match &cmd {
                OverlayCommand::MoveTo { x, y, .. }
                | OverlayCommand::SnapTo { x, y, .. }
                | OverlayCommand::ClickPulse { x, y } => Some((*x, *y)),
                _ => None,
            };
            if let Some((x, y)) = target {
                if !animation_enabled(map, &key) || map.layout.display_at(x, y).is_none() {
                    let rs = map
                        .cursors
                        .entry(key.clone())
                        .or_insert_with(|| render_state_for_key(&map.template, &key));
                    rs.target = Some((x, y));
                    rs.invalidate_placement();
                    arrival_fire(&key);
                    return;
                }
                if matches!(&cmd, OverlayCommand::MoveTo { .. }) {
                    seed_start_in_map(map, &key, x, y);
                }
            }
            let template = map.template.clone();
            let rs = map
                .cursors
                .entry(key.clone())
                .or_insert_with(|| render_state_for_key(&template, &key));
            if let Some(target) = target {
                rs.target = Some(target);
            }
            let ends_travel = matches!(
                &cmd,
                OverlayCommand::SnapTo { .. } | OverlayCommand::SetEnabled(false)
            );
            rs.apply_command(cmd);
            if ends_travel {
                arrival_fire(&key);
            }
            map.command_order.retain(|candidate| candidate != &key);
            map.command_order.push(key);
        }
    }
}

/// Initialise global overlay state (call once, before run_on_main_thread).
pub fn init(cfg: CursorConfig) {
    static INITIALIZED: OnceLock<()> = OnceLock::new();
    INITIALIZED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        CMD_TX
            .set(tx)
            .expect("cursor overlay sender is initialized exactly once");
        *CMD_RX_CELL.lock().unwrap() = Some(rx);
        *ARRIVAL_TX.lock().unwrap() = Some(HashMap::new());
        let mut cursors = IndexMap::new();
        cursors.insert("default".to_owned(), RenderState::new(cfg.clone()));
        *RENDER.lock().unwrap() = Some(RenderMap {
            cursors,
            layout: DisplayLayout::default(),
            command_order: Vec::new(),
            template: cfg,
            ended: HashSet::new(),
        });
    });
    cua_driver_core::cursor_events::install_cursor_event_sink(std::sync::Arc::new(
        |event: cua_driver_core::cursor_events::CursorEvent| {
            use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
            let (session, cmd) = match event {
                CursorEvent::SetSessionLabel { session, label } => {
                    (session, OverlayCommand::SetSessionLabel(label))
                }
                CursorEvent::Action {
                    session,
                    phase: CursorEventPhase::Begin,
                    semantics,
                } => (
                    session,
                    OverlayCommand::BeginAction {
                        action: semantics.action,
                        delivery: semantics.delivery,
                        target: semantics.target,
                    },
                ),
                CursorEvent::Action {
                    session,
                    phase: CursorEventPhase::End,
                    semantics,
                } => (session, OverlayCommand::EndAction(semantics.action)),
                CursorEvent::SelectTheme { session, selection } => (
                    session,
                    OverlayCommand::SetTheme {
                        theme_id: selection.theme_id,
                        reduced_motion: selection.reduced_motion,
                    },
                ),
            };
            send_command(session, cmd);
        },
    ));
}

/// Send a keyed command from any thread (MCP tool, etc.).  Non-blocking; drops
/// if the channel is full (old commands are less important than new ones).
pub fn send_command(key: CursorKey, cmd: OverlayCommand) {
    // An empty key disables cursors for direct platform calls
    // that bypass lifecycle dispatch.
    if key.is_empty() {
        return;
    }
    let arrival_key = matches!(&cmd, OverlayCommand::MoveTo { .. }).then(|| key.clone());
    let sent = CMD_TX.get().is_some_and(|tx| {
        tx.try_send(MacOverlayMsg::Cursor(OverlayMsg::Cmd(
            KeyedOverlayCommand { key, cmd },
        )))
        .is_ok()
    });
    if !sent {
        if let Some(key) = arrival_key {
            arrival_fire(&key);
        }
    }
}

/// Truthful render acknowledgement for lifecycle inspection. This never falls
/// back to the default cursor: an absent, unplaced, disabled, or idle-faded
/// session cursor is not reported as visible.
pub fn is_visible_for_session(key: &str) -> bool {
    RENDER
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .and_then(|map| map.cursors.get(key))
                .map(cursor_is_visible)
        })
        .unwrap_or(false)
}

/// Remove a session's owned cursor from the render collection (fired from the
/// `session_end` hook). The `"default"` key is guarded against removal on the
/// render side, so this is a no-op for it; removing an absent key (anonymous
/// session that never created a cursor) is a harmless no-op.
pub fn remove_cursor(key: CursorKey) {
    if key.is_empty() {
        return;
    }
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(MacOverlayMsg::Cursor(OverlayMsg::Remove(key)));
    }
}

/// Clear the render-side tombstone after a successful explicit session
/// revival. Cursor recreation remains lazy until the next render command.
pub fn revive_cursor(key: CursorKey) {
    if key.is_empty() {
        return;
    }
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(MacOverlayMsg::Cursor(OverlayMsg::Revive(key)));
    }
}

/// Return a snapshot of a cursor's current motion config (for use by
/// set_agent_cursor_motion to apply partial overrides without losing other
/// knobs). Reads the motion of the cursor `key`, falling back to the
/// `"default"` cursor's motion when that key has no own entry yet (e.g. a
/// session whose first motion call precedes any move/enable).
pub fn current_motion(key: &str) -> MotionConfig {
    let guard = RENDER.lock().unwrap();
    let Some(map) = guard.as_ref() else {
        return MotionConfig::default();
    };
    map.cursors
        .get(key)
        .or_else(|| map.cursors.get("default"))
        .map(|rs| rs.core.motion.clone())
        .unwrap_or_default()
}

/// Return the render-owned theme and semantic playback state for one cursor.
pub fn current_theme_state(
    key: &str,
) -> Option<(
    String,
    String,
    String,
    Option<String>,
    cursor_overlay::CursorVisualState,
)> {
    let guard = RENDER.lock().unwrap();
    let map = guard.as_ref()?;
    let state = map
        .cursors
        .get(key)
        .or_else(|| map.cursors.get("default"))?;
    let (id, version, profile, fallback) = state.core.active_theme_metadata();
    Some((id, version, profile, fallback, state.core.visual.clone()))
}

/// Called only while processing commands on the render worker, after tombstones.
fn seed_start_in_map(map: &mut RenderMap, key: &CursorKey, target_x: f64, target_y: f64) -> bool {
    if map.ended.contains(key) {
        return false;
    }
    let Some(display) = map.layout.display_at(target_x, target_y) else {
        return false;
    };
    let rs = map
        .cursors
        .entry(key.clone())
        .or_insert_with(|| render_state_for_key(&map.template, key));
    rs.core.initialize_near_target(
        (target_x, target_y),
        cursor_overlay::DisplayBounds {
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
        },
    )
}

fn animation_enabled(map: &RenderMap, key: &str) -> bool {
    !map.ended.contains(key)
        && map.cursors.get(key).map_or(map.template.enabled, |rs| {
            rs.core.cfg.enabled && rs.core.visible
        })
}

/// Animate the overlay cursor to `(x, y)` and suspend until the Dubins path
/// completes and the spring overshoot begins.
///
/// Placement occurs when the renderer consumes the command. This temporary
/// arrival transport is retained until the later nonblocking motion task.
pub async fn animate_cursor_to(key: CursorKey, x: f64, y: f64) {
    if key.is_empty() {
        return;
    }
    let should_animate = {
        let guard = RENDER.lock().unwrap();
        guard
            .as_ref()
            .is_some_and(|map| animation_enabled(map, &key))
    };
    if !should_animate {
        return;
    }

    // Create a one-shot channel; store the sender (keyed) so the render thread
    // can fire it when this cursor's path finishes.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    arrival_register(key.clone(), tx);

    // Send the MoveTo command (click offset applied inside apply_command).
    send_command(
        key,
        OverlayCommand::MoveTo {
            x,
            y,
            // Arrive pointing upper-left (45°), matching the macOS system-cursor
            // convention and Swift reference (`endAngleDegrees: 45`).
            end_heading_radians: std::f64::consts::FRAC_PI_4,
        },
    );

    // Await arrival signal (fired from render thread when Dubins path ends).
    let _ = rx.await;
}

/// Block the calling thread (must be the OS main thread) running the AppKit
/// event loop and the overlay window.  Never returns normally.
///
/// Call this from `main()` after spawning the tokio background thread.
pub fn run_on_main_thread() {
    // Take the receiver.
    let rx = match CMD_RX_CELL.lock().unwrap().take() {
        Some(r) => r,
        None => {
            // init() was never called — no overlay, just spin.
            loop {
                std::thread::park();
            }
        }
    };

    let enabled = RENDER
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|map| map.template.enabled);

    if !enabled {
        loop {
            std::thread::park();
        }
    }

    // AppKit's `+[NSApplication sharedApplication]` registers the process with
    // the Window Server and ABORTS the whole process (SIGABRT in
    // `_RegisterApplication`) when there's no graphic-session access — e.g.
    // `mcp` run as a stdio child from SSH, a LaunchDaemon, or headless CI.
    // Detect that without touching AppKit and run headless: the MCP server
    // keeps serving on its background thread while this thread just parks,
    // exactly as it does when the overlay is disabled. See issue #1724.
    if !crate::session::has_graphic_access() {
        tracing::warn!(
            "no Window Server / graphic-session access — skipping cursor \
             overlay and running headless (issue #1724)"
        );
        loop {
            std::thread::park();
        }
    }

    // ------------------------------------------------------------------
    // AppKit setup (all on the main thread).
    // ------------------------------------------------------------------
    unsafe { run_appkit(rx) };
}

// ── Animation / render state ──────────────────────────────────────────────
//
// The platform-agnostic fields + tick + apply_command + render pipeline live
// in `cursor_overlay::render_state` (2026-05 dedup audit). What stays here
// is macOS display presentation and the focus-rect overlay.

struct RenderState {
    core: RenderStateCore,
    /// Last resolved target, independent of the temporary seed or path position.
    target: Option<(f64, f64)>,
    /// Focus-highlight rectangle `[x, y, w, h]` in screen coords; None = not shown.
    focus_rect: Option<[f64; 4]>,
    /// Fade progress for the focus rect: 0.0 = fully visible, 1.0 = gone.
    focus_rect_t: f64,
}

impl RenderState {
    fn new(cfg: CursorConfig) -> Self {
        RenderState {
            core: RenderStateCore::new(cfg),
            target: None,
            focus_rect: None,
            focus_rect_t: 1.0,
        }
    }

    fn invalidate_placement(&mut self) {
        self.core.placed = false;
        self.core.path = None;
        self.core.spring = None;
        self.core.spring_tgt = None;
        self.core.dist = 0.0;
        self.core.click_t = None;
        self.core.session_badge_hovered = false;
        self.focus_rect = None;
        self.focus_rect_t = 1.0;
    }

    /// Advance the animation by `dt`.  Uses the Swift reference constants
    /// (peakSpeed=900, springK=400, overshoot=0.8) — see
    /// [`RenderStateCore::tick_swift_constants`].  Returns true if an
    /// arrival signal should be fired (the path just ended).
    fn tick(&mut self, dt: f64) -> bool {
        if !self.core.cfg.enabled || !self.core.visible || !self.core.placed {
            return false;
        }
        let fire_arrival = self.core.tick_swift_constants(dt);

        // Advance focus-rect fade (fades out over ~600ms).  macOS-only —
        // the shared core has no focus_rect concept.
        if self.focus_rect.is_some() {
            self.focus_rect_t = (self.focus_rect_t + dt / 0.6).min(1.0);
            if self.focus_rect_t >= 1.0 {
                self.focus_rect = None;
                self.focus_rect_t = 1.0;
            }
        }

        fire_arrival
    }

    fn apply_command(&mut self, cmd: OverlayCommand) {
        // First contact places exactly at its resolved target. Subsequent
        // pulses retain the existing glide behavior until the motion task.
        match cmd {
            OverlayCommand::ShowFocusRect(rect) => {
                self.focus_rect = rect;
                self.focus_rect_t = 0.0; // reset fade to fully visible
            }
            OverlayCommand::ClickPulse { x, y } if !self.core.placed => {
                let _ =
                    self.core
                        .apply_command_base(OverlayCommand::ClickPulse { x, y }, true, false);
            }
            other => {
                let _ = self.core.apply_command_base(other, true, true);
            }
        }
    }

    /// True while the render loop must wake at frame cadence because the next
    /// tick can change pixels. A brand-new unplaced cursor is deliberately
    /// quiescent, so `serve` with no agent activity can block on the command
    /// channel instead of compositing empty display pixmaps at 60fps.
    fn needs_frame_tick(&self) -> bool {
        if !self.core.cfg.enabled || !self.core.visible || !self.core.placed {
            return false;
        }
        self.core.path.is_some()
            || self.core.spring.is_some()
            || self.core.click_t.is_some()
            || self.focus_rect.is_some()
            || self.core.session_badge_needs_frame_tick()
            || (self.core.motion.idle_hide_ms > 0.0 && cursor_is_visible(self))
    }
}

fn render_map_needs_frame_tick(map: &RenderMap) -> bool {
    map.cursors.values().any(RenderState::needs_frame_tick)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ZOrderRoute {
    generation: u64,
    display_id: DisplayId,
    target_wid: Option<u64>,
}

fn z_order_routes(map: &RenderMap) -> Vec<ZOrderRoute> {
    map.layout
        .displays
        .iter()
        .copied()
        .filter_map(|display| {
            let state = map
                .command_order
                .iter()
                .rev()
                .filter_map(|key| map.cursors.get(key))
                .find(|state| state_paints_display(state, display))?;
            Some(ZOrderRoute {
                generation: map.layout.generation,
                display_id: display.id,
                target_wid: state.core.pinned_wid,
            })
        })
        .collect()
}

/// The same per-surface decisions restore new generations and periodically
/// repin existing surfaces. An unrelated session cannot suppress either route.
fn z_order_updates<'a>(
    routes: &'a [ZOrderRoute],
    presented: &[ZOrderRoute],
    repin_due: bool,
    cursor_commanded: bool,
) -> Vec<&'a ZOrderRoute> {
    routes
        .iter()
        .filter(|route| {
            !presented.contains(route)
                || if route.target_wid.is_some() {
                    repin_due
                } else {
                    cursor_commanded
                }
        })
        .collect()
}

// ── AppKit / CGImage plumbing ─────────────────────────────────────────────

struct NativeSurface {
    win_ptr: usize,
    layer_ptr: usize,
}

impl NativeSurface {
    unsafe fn close(self) {
        use objc2::runtime::AnyObject;
        let win = self.win_ptr as *mut AnyObject;
        let _: () = objc2::msg_send![win, orderOut: std::ptr::null_mut::<AnyObject>()];
        let _: () = objc2::msg_send![win, setReleasedWhenClosed: true];
        let _: () = objc2::msg_send![win, close];
    }
}

struct AppKitOverlayHost {
    generation: u64,
    surfaces: HashMap<DisplayId, NativeSurface>,
}

impl AppKitOverlayHost {
    unsafe fn create(layout: &DisplayLayout) -> Option<Self> {
        let primary_height = layout.primary_height()?;
        let mut surfaces = HashMap::<DisplayId, NativeSurface>::new();
        for geometry in &layout.displays {
            let Some(surface) = create_native_surface(*geometry, primary_height) else {
                for surface in surfaces.into_values() {
                    surface.close();
                }
                return None;
            };
            surfaces.insert(geometry.id, surface);
        }
        Some(Self {
            generation: layout.generation,
            surfaces,
        })
    }

    unsafe fn close(self) {
        for surface in self.surfaces.into_values() {
            surface.close();
        }
    }
}

unsafe fn create_native_surface(
    geometry: DisplayGeometry,
    primary_height: f64,
) -> Option<NativeSurface> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    let frame = geometry.appkit_frame(primary_height);
    let allocated: *mut AnyObject = msg_send![class!(NSWindow), alloc];
    let win: *mut AnyObject = msg_send![allocated,
        initWithContentRect: frame
        styleMask: 0u64
        backing: 2u64
        defer: false
    ];
    if win.is_null() {
        return None;
    }

    let _: () = msg_send![win, setOpaque: false];
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![win, setBackgroundColor: clear];
    let _: () = msg_send![win, setHasShadow: false];
    let _: () = msg_send![win, setIgnoresMouseEvents: true];
    let _: () = msg_send![win, setSharingType: 1u64];
    let _: () = msg_send![win, setLevel: 0i64];
    let _: () = msg_send![win, setCollectionBehavior: (1u64 | (1 << 8) | (1 << 4))];
    let _: () = msg_send![win, setReleasedWhenClosed: false];
    let _: () = msg_send![win, setHidesOnDeactivate: false];

    let content_view: *mut AnyObject = msg_send![win, contentView];
    let _: () = msg_send![content_view, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![content_view, layer];
    let _: () = msg_send![layer, setContentsScale: geometry.backing_scale.max(1.0)];
    let gravity_ns: *mut AnyObject = msg_send![class!(NSString),
        stringWithUTF8String: c"topLeft".as_ptr().cast::<u8>()
    ];
    let _: () = msg_send![layer, setContentsGravity: gravity_ns];
    // Keep new surfaces hidden until their controlling session's route orders
    // them. Surface creation alone must never promote a background cursor.

    Some(NativeSurface {
        win_ptr: win as usize,
        layer_ptr: layer as usize,
    })
}

fn replace_display_layout(map: &mut RenderMap, layout: DisplayLayout) {
    for (key, state) in &mut map.cursors {
        if state
            .target
            .is_some_and(|(x, y)| layout.display_at(x, y).is_none())
        {
            state.invalidate_placement();
            arrival_fire(key);
        }
    }
    map.layout = layout;
}

unsafe fn rebuild_appkit_host() {
    let generation = DISPLAY_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let mut layout = super::display_layout::active_layout(generation).unwrap_or_else(|error| {
        tracing::warn!(
            error,
            "macOS cursor overlay could not enumerate active displays"
        );
        DisplayLayout {
            generation,
            displays: vec![],
        }
    });
    let host = AppKitOverlayHost::create(&layout);
    if host.is_none() {
        // A failed or empty rebuild invalidates every old viewport. Publishing
        // an empty snapshot also rejects placement until a later rebuild succeeds.
        if !layout.displays.is_empty() {
            tracing::warn!("macOS cursor overlay could not create every display surface");
        }
        layout.displays.clear();
    }
    if let Some(map) = RENDER.lock().unwrap().as_mut() {
        replace_display_layout(map, layout);
    }
    let previous = std::mem::replace(&mut *HOST.lock().unwrap(), host);
    if let Some(previous) = previous {
        previous.close();
    }
}

unsafe fn run_appkit(rx: std::sync::mpsc::Receiver<MacOverlayMsg>) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    let _mtm = objc2_foundation::MainThreadMarker::new()
        .expect("run_appkit must be called from the main thread");

    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    let _: bool = msg_send![app, setActivationPolicy: 1i64];
    let _: () = msg_send![app, finishLaunching];

    rebuild_appkit_host();
    register_display_reconfiguration_callback();

    std::thread::spawn(move || render_loop(rx));
    let _: () = msg_send![app, run];
}

fn register_display_reconfiguration_callback() {
    use core_graphics::display::CGDisplayRegisterReconfigurationCallback;

    unsafe extern "C" fn changed(_display: u32, flags: u32, _context: *const c_void) {
        use core_graphics::display::CGDisplayChangeSummaryFlags;
        if flags & CGDisplayChangeSummaryFlags::kCGDisplayBeginConfigurationFlag.bits() != 0 {
            return;
        }
        dispatch_rebuild_appkit_host();
    }

    let result = unsafe { CGDisplayRegisterReconfigurationCallback(changed, std::ptr::null()) };
    if result != 0 {
        tracing::warn!(
            result,
            "macOS cursor overlay could not observe display changes"
        );
    }
}

fn dispatch_rebuild_appkit_host() {
    dispatch_on_main(Box::new(|| unsafe {
        rebuild_appkit_host();
        if let Some(tx) = CMD_TX.get() {
            let _ = tx.try_send(MacOverlayMsg::LayoutChanged);
        }
    }));
}

fn render_loop(rx: std::sync::mpsc::Receiver<MacOverlayMsg>) {
    let target_frame_ms = Duration::from_millis(16);
    let hover_poll_ms = Duration::from_millis(80);
    let mut last_tick = Instant::now();
    let mut frame_tick_needed = false;
    let mut hover_poll_needed = false;
    let mut presented_displays = HashSet::<DisplayId>::new();
    let mut presented_z_order = Vec::<ZOrderRoute>::new();
    let mut repin_frames: u32 = 0;

    loop {
        let (first_msg, hover_poll_tick) = if frame_tick_needed {
            (None, hover_poll_needed)
        } else if hover_poll_needed {
            match rx.recv_timeout(hover_poll_ms) {
                Ok(msg) => (Some(msg), true),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (None, true),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(msg) => (Some(msg), false),
                Err(_) => break,
            }
        };

        let woke_from_idle = first_msg.is_some();
        let now = Instant::now();
        let dt = if woke_from_idle {
            0.0
        } else {
            now.duration_since(last_tick).as_secs_f64().min(0.05)
        };
        last_tick = now;

        let (
            z_order,
            arrived,
            had_msg,
            cursor_commanded,
            hover_changed,
            next_frame_tick_needed,
            next_hover_poll_needed,
        ) = {
            let mut guard = RENDER.lock().unwrap();
            let Some(map) = guard.as_mut() else {
                break;
            };
            let mut had_msg = false;
            let mut cursor_commanded = false;

            let mut apply = |message: MacOverlayMsg| {
                had_msg = true;
                match message {
                    MacOverlayMsg::Cursor(message) => {
                        cursor_commanded |= match &message {
                            OverlayMsg::Cmd(command) => !map.ended.contains(&command.key),
                            _ => false,
                        };
                        apply_msg(map, message);
                    }
                    MacOverlayMsg::LayoutChanged => {}
                }
            };
            if let Some(message) = first_msg {
                apply(message);
            }
            while let Ok(message) = rx.try_recv() {
                apply(message);
            }

            let mut arrived = Vec::new();
            if frame_tick_needed || had_msg {
                for (key, state) in map.cursors.iter_mut() {
                    if state.tick(dt) {
                        arrived.push(key.clone());
                    }
                }
            }

            let pointer = if hover_poll_tick
                || map
                    .cursors
                    .values()
                    .any(|state| state.core.session_badge_needs_hover_poll())
            {
                hardware_cursor_position()
            } else {
                None
            };
            let mut hover_changed = false;
            if pointer.is_some() || hover_poll_tick {
                for state in map.cursors.values_mut() {
                    hover_changed |= state.core.update_session_badge_hover(pointer);
                }
            }

            let z_order = z_order_routes(map);
            let next_frame_tick_needed = render_map_needs_frame_tick(map);
            let next_hover_poll_needed = map
                .cursors
                .values()
                .any(|state| state.core.session_badge_needs_hover_poll());

            (
                z_order,
                arrived,
                had_msg,
                cursor_commanded,
                hover_changed,
                next_frame_tick_needed,
                next_hover_poll_needed,
            )
        };

        for key in &arrived {
            arrival_fire(key);
        }

        if frame_tick_needed || had_msg {
            repin_frames += 1;
            for route in z_order_updates(
                &z_order,
                &presented_z_order,
                repin_frames >= 60,
                cursor_commanded,
            ) {
                match route.target_wid {
                    Some(target_wid) => {
                        dispatch_pin_above(route.generation, route.display_id, target_wid)
                    }
                    None => dispatch_order_front(route.generation, route.display_id),
                }
            }
            presented_z_order = z_order;
            if repin_frames >= 60 {
                repin_frames = 0;
            }
        }

        if had_msg || hover_changed || frame_tick_needed || next_frame_tick_needed {
            let frames = {
                let guard = RENDER.lock().unwrap();
                let Some(map) = guard.as_ref() else {
                    break;
                };
                let painted = painted_display_ids(map);
                let displays_to_present = presented_displays
                    .union(&painted)
                    .copied()
                    .collect::<HashSet<_>>();
                let frames = displays_to_present
                    .into_iter()
                    .filter_map(|display_id| {
                        map.layout
                            .displays
                            .iter()
                            .copied()
                            .find(|display| display.id == display_id)
                            .map(|display| {
                                (
                                    map.layout.generation,
                                    display.id,
                                    render_display(map, display),
                                )
                            })
                    })
                    .collect::<Vec<_>>();
                presented_displays = painted;
                frames
            };

            for (generation, display_id, pixmap) in frames {
                dispatch_present(generation, display_id, pixmap);
            }
        }

        frame_tick_needed = next_frame_tick_needed;
        hover_poll_needed = next_hover_poll_needed;
        if frame_tick_needed {
            let elapsed = Instant::now().duration_since(last_tick);
            if let Some(remaining) = target_frame_ms.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
        }
    }
}

/// Displays whose buffers can contain pixels in the current frame. This is
/// based on painted bounds, not cursor ownership: artwork may span a seam.
fn painted_display_ids(map: &RenderMap) -> HashSet<DisplayId> {
    map.cursors
        .values()
        .flat_map(|state| {
            map.layout
                .displays
                .iter()
                .copied()
                .filter(|display| state_paints_display(state, *display))
                .map(|display| display.id)
        })
        .collect()
}

fn state_paints_display(state: &RenderState, display: DisplayGeometry) -> bool {
    if !cursor_is_visible(state) {
        return false;
    }
    let (x, y) = state.core.pos;
    let radius = state.core.paint_radius();
    let cursor_intersects = rectangles_intersect(
        (x - radius, y - radius, radius * 2.0, radius * 2.0),
        display,
    );
    let focus_intersects = state
        .focus_rect
        .is_some_and(|[x, y, width, height]| rectangles_intersect((x, y, width, height), display));
    cursor_intersects || focus_intersects
}

fn rectangles_intersect(rect: (f64, f64, f64, f64), display: DisplayGeometry) -> bool {
    let (x, y, width, height) = rect;
    width > 0.0
        && height > 0.0
        && x < display.x + display.width
        && x + width > display.x
        && y < display.y + display.height
        && y + height > display.y
}

fn render_display(map: &RenderMap, display: DisplayGeometry) -> tiny_skia::Pixmap {
    let (width, height) = display.pixel_size();
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .unwrap_or_else(|| tiny_skia::Pixmap::new(1, 1).unwrap());
    for state in map.cursors.values() {
        if !state_paints_display(state, display) {
            continue;
        }
        let focus = state.focus_rect.map(|rect| FocusRect {
            rect,
            t: state.focus_rect_t,
        });
        let anchor_display = state
            .core
            .placed
            .then_some(state.core.pos)
            .and_then(|(x, y)| map.layout.display_at(x, y))
            .map(|display| display.id);
        let painter = if anchor_display == Some(display.id) {
            cursor_overlay::paint_cursor
        } else {
            cursor_overlay::paint_cursor_art
        };
        painter(
            &mut pixmap,
            &state.core,
            display.x,
            display.y,
            focus,
            display.backing_scale as f32,
        );
    }
    pixmap
}

fn hardware_cursor_position() -> Option<(f64, f64)> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let location = event.location();
    Some((location.x, location.y))
}

fn cursor_is_visible(state: &RenderState) -> bool {
    state.core.cfg.enabled && state.core.cursor_is_revealed()
}

type MainWork = Box<dyn FnOnce() + Send>;

fn dispatch_on_main(work: MainWork) {
    #[link(name = "dispatch", kind = "dylib")]
    extern "C" {
        static _dispatch_main_q: u8;
        fn dispatch_async_f(
            queue: *const c_void,
            context: *mut c_void,
            work: unsafe extern "C" fn(*mut c_void),
        );
    }

    unsafe extern "C" fn run(ctx: *mut c_void) {
        let work: Box<MainWork> = Box::from_raw(ctx.cast());
        work();
    }

    let payload = Box::new(work);
    unsafe {
        let main_queue = &raw const _dispatch_main_q as *const c_void;
        dispatch_async_f(main_queue, Box::into_raw(payload).cast(), run);
    }
}

/// Present one display-local pixmap. AppKit objects remain main-thread-owned;
/// stale frames are rejected by display-layout generation.
fn dispatch_present(generation: u64, display_id: DisplayId, pixmap: tiny_skia::Pixmap) {
    let Some(cg_image_ptr) = pixmap_to_cgimage(&pixmap) else {
        return;
    };
    dispatch_on_main(Box::new(move || unsafe {
        extern "C" {
            fn CGImageRelease(image: *mut c_void);
        }

        let host = HOST.lock().unwrap();
        if let Some(surface) = host
            .as_ref()
            .filter(|host| host.generation == generation)
            .and_then(|host| host.surfaces.get(&display_id))
        {
            let layer = surface.layer_ptr as *mut objc2::runtime::AnyObject;
            let image = cg_image_ptr as *mut objc2::runtime::AnyObject;
            let _: () = objc2::msg_send![layer, setContents: image];
        }
        CGImageRelease(cg_image_ptr as *mut c_void);
    }));
}

/// Raise the normal-level overlay without activating the driver application.
///
/// This is used only for an externally visible cursor with no target window.
/// Target-bound actions continue to use [`dispatch_pin_above`] so background
/// delivery remains below unrelated foreground applications.
fn dispatch_order_front(generation: u64, display_id: DisplayId) {
    dispatch_on_main(Box::new(move || unsafe {
        let host = HOST.lock().unwrap();
        if let Some(surface) = host
            .as_ref()
            .filter(|host| host.generation == generation)
            .and_then(|host| host.surfaces.get(&display_id))
        {
            let win = surface.win_ptr as *mut objc2::runtime::AnyObject;
            let _: () = objc2::msg_send![win, orderFrontRegardless];
        }
    }));
}

/// Apply the existing best-effort target-relative order to one display
/// surface. AppKit does not guarantee cross-process `z+1`; when the exact
/// target is already frontmost we safely raise the click-through overlay.
fn target_is_frontmost_visible_window(
    target_wid: u64,
    frontmost_pid: Option<i32>,
    windows: &[crate::windows::WindowInfo],
) -> bool {
    let Some(target) = windows
        .iter()
        .find(|window| u64::from(window.window_id) == target_wid)
    else {
        return false;
    };
    if !target.is_on_screen || target.layer != 0 || frontmost_pid != Some(target.pid) {
        return false;
    }

    windows
        .iter()
        .filter(|window| {
            window.is_on_screen
                && window.layer == 0
                && window.pid == target.pid
                && window.bounds.width > 1.0
                && window.bounds.height > 1.0
        })
        .max_by_key(|window| window.z_index)
        .is_some_and(|window| u64::from(window.window_id) == target_wid)
}

fn dispatch_pin_above(generation: u64, display_id: DisplayId, target_wid: u64) {
    let windows = crate::windows::visible_windows();
    let raise_front =
        target_is_frontmost_visible_window(target_wid, crate::apps::frontmost_pid(), &windows);
    dispatch_on_main(Box::new(move || unsafe {
        let host = HOST.lock().unwrap();
        if let Some(surface) = host
            .as_ref()
            .filter(|host| host.generation == generation)
            .and_then(|host| host.surfaces.get(&display_id))
        {
            let win = surface.win_ptr as *mut objc2::runtime::AnyObject;
            let _: () = objc2::msg_send![win, orderWindow: 1i64 relativeTo: target_wid as i64];
            if raise_front {
                let _: () = objc2::msg_send![win, orderFrontRegardless];
            }
        }
    }));
}

/// Create a `CGImage` from a `tiny_skia::Pixmap` (premultiplied RGBA).
/// Returns a `+1` retained pointer that the caller must release.
fn pixmap_to_cgimage(pixmap: &tiny_skia::Pixmap) -> Option<usize> {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }

    let data = pixmap.data();
    let bytes_per_row = w * 4;

    // tiny-skia produces premultiplied RGBA with bytes in memory order [R, G, B, A].
    // CGImage flag breakdown (Apple CGBitmapInfo / CGImageAlphaInfo enums):
    //   kCGImageAlphaPremultipliedLast = 0x0001  → alpha is the LAST channel  (RGBA)
    //   kCGImageAlphaPremultipliedFirst = 0x0002 → alpha is the FIRST channel (ARGB)  ← NOT what we want
    //   kCGBitmapByteOrder32Big        = 0x4000  → big-endian 32-bit pixel,
    //     so memory order is the same as component order (bytes = [R, G, B, A]).
    // Combined: kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big = 0x4001
    // This correctly maps tiny-skia's [R, G, B, A] bytes to the display RGB channels.
    const BITMAP_INFO: u32 = 0x0001 | 0x4000; // kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big

    // Release callback: CGDataProvider calls this when it is done with the buffer.
    // `info` is the Box<Vec<u8>> we passed as the `info` argument below.
    unsafe extern "C" fn release_pixel_data(info: *mut c_void, _data: *const c_void, _size: usize) {
        // Re-box and drop to free the buffer.
        drop(Box::from_raw(info as *mut Vec<u8>));
    }

    unsafe {
        extern "C" {
            fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
            fn CGColorSpaceRelease(cs: *mut c_void);
            fn CGDataProviderCreateWithData(
                info: *mut c_void,
                data: *const c_void,
                size: usize,
                release_data: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
            ) -> *mut c_void;
            fn CGDataProviderRelease(provider: *mut c_void);
            fn CGImageCreate(
                width: usize,
                height: usize,
                bits_per_component: usize,
                bits_per_pixel: usize,
                bytes_per_row: usize,
                color_space: *mut c_void,
                bitmap_info: u32,
                provider: *mut c_void,
                decode: *const f64,
                should_interpolate: bool,
                intent: u32,
            ) -> *mut c_void;
        }

        // Copy the pixel data into a heap Vec; the data provider will own it
        // and free it via release_pixel_data when the CGImage is released.
        let copied: Vec<u8> = data.to_vec();
        let len = copied.len();
        let ptr = copied.as_ptr();
        // Leak the Vec into a raw Box so we can pass it as the `info` opaque pointer.
        let copied_box: *mut Vec<u8> = Box::into_raw(Box::new(copied));

        let cs = CGColorSpaceCreateDeviceRGB();
        let provider = CGDataProviderCreateWithData(
            copied_box as *mut c_void,
            ptr as *const c_void,
            len,
            Some(release_pixel_data), // frees copied_box when provider is released
        );
        let img = CGImageCreate(
            w,
            h,
            8,  // bits_per_component
            32, // bits_per_pixel
            bytes_per_row,
            cs,
            BITMAP_INFO,
            provider,
            std::ptr::null(),
            false,
            0, // kCGRenderingIntentDefault
        );

        CGColorSpaceRelease(cs);
        CGDataProviderRelease(provider);
        // Do NOT drop copied_box here — release_pixel_data owns it now.

        if img.is_null() {
            None
        } else {
            Some(img as usize)
        }
    }
}

// ── Headless unit tests for the keyed render collection ───────────────────
//
// These prove the per-session ownership data model, the session_end removal
// lifecycle, the "default" guard, and per-key arrival isolation WITHOUT any
// AppKit / NSWindow. The on-screen rendering (CGImage / CALayer setContents)
// still needs a real display and is verified separately on the macOS VM.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn command(key: &str, cmd: OverlayCommand) -> OverlayMsg {
        OverlayMsg::Cmd(KeyedOverlayCommand {
            key: key.into(),
            cmd,
        })
    }

    #[test]
    fn slice_a_negative_y_surface_paints_pointer_and_focus_at_two_x() {
        let mut map = empty_map();
        let above = DisplayGeometry {
            id: 2,
            x: 0.0,
            y: -900.0,
            width: 1440.0,
            height: 900.0,
            backing_scale: 2.0,
            is_primary: false,
        };
        map.layout.displays.push(above);
        apply_msg(
            &mut map,
            command(
                "above",
                OverlayCommand::SnapTo {
                    x: 40.0,
                    y: -860.0,
                    heading_radians: Some(0.0),
                },
            ),
        );
        apply_msg(
            &mut map,
            command(
                "above",
                OverlayCommand::ShowFocusRect(Some([95.0, -840.0, 40.0, 20.0])),
            ),
        );
        assert_eq!(painted_display_ids(&map), HashSet::from([2]));
        let pm = render_display(&map, above);
        assert_eq!((pm.width(), pm.height()), (2880, 1800));
        assert!(pm.pixel(190, 140).unwrap().alpha() > 100);
        assert!(pm.data().chunks_exact(4).any(|pixel| pixel[3] > 0));
        assert!(render_display(&map, map.layout.displays[0])
            .data()
            .iter()
            .all(|v| *v == 0));
    }

    #[test]
    fn slice_a_layout_loss_invalidates_pending_target_and_arrival() {
        let mut map = empty_map();
        let primary = map.layout.displays[0];
        let secondary = DisplayGeometry {
            id: 2,
            x: -1440.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
            backing_scale: 2.0,
            is_primary: false,
        };
        map.layout.displays.push(secondary);
        apply_msg(&mut map, command("lost", OverlayCommand::PinAbove(777)));
        apply_msg(
            &mut map,
            command(
                "lost",
                OverlayCommand::SnapTo {
                    x: 50.0,
                    y: 50.0,
                    heading_radians: None,
                },
            ),
        );
        apply_msg(&mut map, move_msg("lost", -1400.0, 300.0));
        // The rendered anchor is still on primary while the resolved target is on secondary.
        assert_eq!(map.cursors["lost"].core.pos, (50.0, 50.0));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        arrival_register("lost".into(), tx);
        replace_display_layout(
            &mut map,
            DisplayLayout {
                generation: 2,
                displays: vec![primary],
            },
        );
        assert!(!map.cursors["lost"].core.placed);
        assert!(rx.try_recv().is_ok());
        assert!(!map.cursors["lost"].needs_frame_tick());
        assert!(painted_display_ids(&map).is_empty());
        assert_eq!(map.cursors["lost"].core.pinned_wid, Some(777));
        replace_display_layout(
            &mut map,
            DisplayLayout {
                generation: 3,
                displays: vec![primary, secondary],
            },
        );
        assert!(
            !map.cursors["lost"].core.placed,
            "reattaching alone must not resurrect old visuals"
        );
        apply_msg(&mut map, move_msg("lost", -1400.0, 300.0));
        assert_eq!(map.cursors["lost"].core.pos, (-1438.0, 160.0));
    }

    #[test]
    fn slice_a_empty_rebuild_clears_all_viewports_and_pending_motion() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("live", 50.0, 50.0));
        replace_display_layout(
            &mut map,
            DisplayLayout {
                generation: 2,
                displays: vec![],
            },
        );
        assert!(map.layout.displays.is_empty());
        assert!(!map.cursors["live"].core.placed);
        assert!(!render_map_needs_frame_tick(&map));
        assert!(z_order_routes(&map).is_empty());
    }

    #[test]
    fn slice_a_out_of_layout_command_clears_prior_presentation() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("live", 50.0, 50.0));
        apply_msg(&mut map, move_msg("live", -1400.0, 50.0));
        assert!(!map.cursors["live"].core.placed);
        assert!(!render_map_needs_frame_tick(&map));
    }

    fn ordering_fixture() -> RenderMap {
        let mut map = empty_map();
        map.layout.displays[0].width = 1000.0;
        map.layout.displays[0].height = 1000.0;
        map.layout.displays.push(DisplayGeometry {
            id: 2,
            x: -1000.0,
            is_primary: false,
            ..map.layout.displays[0]
        });
        for (key, wid, x) in [("one", 111, 500.0), ("two", 222, -500.0)] {
            apply_msg(&mut map, command(key, OverlayCommand::PinAbove(wid)));
            apply_msg(
                &mut map,
                command(
                    key,
                    OverlayCommand::SnapTo {
                        x,
                        y: 500.0,
                        heading_radians: None,
                    },
                ),
            );
        }
        map
    }

    fn ordering_pairs(map: &RenderMap) -> Vec<(DisplayId, Option<u64>)> {
        z_order_routes(map)
            .into_iter()
            .map(|route| (route.display_id, route.target_wid))
            .collect()
    }

    #[test]
    fn slice_a_ordering_rebuild_restores_both_surviving_display_pins() {
        let mut map = ordering_fixture();
        assert_eq!(map.command_order.last().map(String::as_str), Some("two"));
        let layout = DisplayLayout {
            generation: 2,
            ..map.layout.clone()
        };
        replace_display_layout(&mut map, layout);
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111)), (2, Some(222))]);
    }

    #[test]
    fn slice_a_ordering_initial_routes_include_every_painted_surface() {
        let map = ordering_fixture();
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111)), (2, Some(222))]);
    }

    #[test]
    fn slice_a_ordering_initial_rebuild_and_periodic_updates_cover_both_displays() {
        let mut map = ordering_fixture();
        let routes = z_order_routes(&map);
        let pairs = |updates: Vec<&ZOrderRoute>| {
            updates
                .into_iter()
                .map(|route| (route.display_id, route.target_wid))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pairs(z_order_updates(&routes, &[], false, false)),
            vec![(1, Some(111)), (2, Some(222))]
        );
        assert!(z_order_updates(&routes, &routes, false, false).is_empty());
        assert_eq!(
            pairs(z_order_updates(&routes, &routes, true, false)),
            vec![(1, Some(111)), (2, Some(222))]
        );
        let layout = DisplayLayout {
            generation: 2,
            ..map.layout.clone()
        };
        replace_display_layout(&mut map, layout);
        assert_eq!(
            pairs(z_order_updates(
                &z_order_routes(&map),
                &routes,
                false,
                false
            )),
            vec![(1, Some(111)), (2, Some(222))]
        );
    }

    #[test]
    fn slice_a_ordering_unpinned_controller_is_explicit_and_missing_keys_are_skipped() {
        let mut map = ordering_fixture();
        apply_msg(
            &mut map,
            command(
                "desktop",
                OverlayCommand::SnapTo {
                    x: 500.0,
                    y: 500.0,
                    heading_radians: None,
                },
            ),
        );
        assert_eq!(ordering_pairs(&map), vec![(1, None), (2, Some(222))]);
        let routes = z_order_routes(&map);
        assert_eq!(
            z_order_updates(&routes, &routes, false, true),
            vec![&routes[0]]
        );
        assert_eq!(
            z_order_updates(&routes, &routes, true, false),
            vec![&routes[1]]
        );
        // Missing state must not prevent another visible session controlling
        // this surface, even if an ordering key remains in a snapshot.
        map.cursors.shift_remove("desktop");
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111)), (2, Some(222))]);
        apply_msg(&mut map, command("", OverlayCommand::PinAbove(999)));
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111)), (2, Some(222))]);
    }

    #[test]
    fn slice_a_ordering_removal_restores_last_commanded_survivor() {
        let mut map = ordering_fixture();
        apply_msg(&mut map, command("three", OverlayCommand::PinAbove(333)));
        apply_msg(
            &mut map,
            command(
                "three",
                OverlayCommand::SnapTo {
                    x: 500.0,
                    y: 500.0,
                    heading_radians: None,
                },
            ),
        );
        // Insertion order is one, two, three. Command order now puts one first.
        apply_msg(&mut map, command("one", OverlayCommand::PinAbove(111)));
        apply_msg(&mut map, OverlayMsg::Remove("one".into()));
        assert_eq!(ordering_pairs(&map), vec![(1, Some(333)), (2, Some(222))]);
        apply_msg(&mut map, OverlayMsg::Remove("three".into()));
        assert_eq!(ordering_pairs(&map), vec![(2, Some(222))]);
    }

    #[test]
    fn slice_a_ordering_missing_target_and_unplaced_session_do_not_erase_other_routes() {
        let mut map = ordering_fixture();
        apply_msg(&mut map, move_msg("two", -2000.0, 500.0));
        apply_msg(&mut map, command("unplaced", OverlayCommand::PinAbove(333)));
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111))]);
        apply_msg(&mut map, command("one", OverlayCommand::SetEnabled(false)));
        assert!(ordering_pairs(&map).is_empty());
        replace_display_layout(
            &mut map,
            DisplayLayout {
                generation: 2,
                displays: vec![],
            },
        );
        assert!(ordering_pairs(&map).is_empty());
        assert!(ordering_pairs(&empty_map()).is_empty());
    }

    #[test]
    fn slice_a_sessions_preserve_stored_pins_on_shared_surface() {
        let mut map = empty_map();
        for (key, wid) in [("one", 111), ("two", 222)] {
            apply_msg(&mut map, command(key, OverlayCommand::PinAbove(wid)));
            apply_msg(
                &mut map,
                command(
                    key,
                    OverlayCommand::SnapTo {
                        x: 50.0,
                        y: 50.0,
                        heading_radians: None,
                    },
                ),
            );
        }
        assert_eq!(map.cursors["one"].core.pinned_wid, Some(111));
        assert_eq!(map.cursors["two"].core.pinned_wid, Some(222));
        assert_eq!(z_order_routes(&map)[0].target_wid, Some(222));
        apply_msg(&mut map, move_msg("one", 60.0, 60.0));
        assert_eq!(z_order_routes(&map)[0].target_wid, Some(111));
        assert_eq!(map.cursors["two"].core.pinned_wid, Some(222));
    }

    #[test]
    fn slice_a_action_admission_does_not_place_or_reject_a_fresh_cursor() {
        let mut map = empty_map();
        assert!(animation_enabled(&map, "fresh"));
        assert!(!map.cursors.contains_key("fresh"));
        map.ended.insert("ended".into());
        assert!(!animation_enabled(&map, "ended"));
        apply_msg(
            &mut map,
            command("disabled", OverlayCommand::SetEnabled(false)),
        );
        assert!(!animation_enabled(&map, "disabled"));
        assert!(!map.cursors["disabled"].core.placed);
    }

    #[test]
    fn slice_a_first_command_seeds_on_each_target_display() {
        for (x, y) in [(0.0, 0.0), (-1440.0, 0.0), (0.0, -900.0)] {
            let mut map = empty_map();
            map.layout.displays[0].x = x;
            map.layout.displays[0].y = y;
            map.layout.displays[0].width = 1440.0;
            map.layout.displays[0].height = 900.0;
            let target = (x + 400.0, y + 400.0);
            apply_msg(&mut map, move_msg("first-command", target.0, target.1));
            let core = &map.cursors["first-command"].core;
            assert!(core.placed);
            assert_eq!(core.pos, (x + 260.0, y + 260.0));
            assert!(core.path.is_some());
        }
    }

    #[test]
    fn slice_a_missing_display_never_places_or_keeps_waiter() {
        for empty in [false, true] {
            let mut map = empty_map();
            if empty {
                map.layout.displays.clear();
            }
            let key = format!("slice-a-missing-{empty}");
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            arrival_register(key.clone(), tx);
            apply_msg(&mut map, move_msg(&key, -1400.0, -700.0));
            assert!(
                rx.try_recv().is_ok(),
                "unavailable display must complete temporary arrival"
            );
            let rs = &map.cursors[&key];
            assert!(!rs.core.placed);
            assert!(!rs.needs_frame_tick());
            assert!(!cursor_is_visible(rs));
        }
    }

    #[test]
    fn slice_a_disabled_and_nonpositional_commands_are_quiescent() {
        let mut map = empty_map();
        apply_msg(
            &mut map,
            command("disabled", OverlayCommand::SetEnabled(false)),
        );
        apply_msg(&mut map, move_msg("disabled", 50.0, 50.0));
        assert!(!map.cursors["disabled"].core.placed);
        assert!(!map.cursors["disabled"].needs_frame_tick());
        apply_msg(
            &mut map,
            command(
                "fresh",
                OverlayCommand::ShowFocusRect(Some([1.0, 2.0, 3.0, 4.0])),
            ),
        );
        assert!(!map.cursors["fresh"].needs_frame_tick());
        assert!(!cursor_is_visible(&map.cursors["fresh"]));
        apply_msg(
            &mut map,
            command("disabled", OverlayCommand::SetEnabled(true)),
        );
        apply_msg(&mut map, move_msg("disabled", 50.0, 50.0));
        assert_eq!(map.cursors["disabled"].core.pos, (2.0, 2.0));
    }

    #[test]
    fn slice_a_remove_before_first_command_and_revive_seed_anew() {
        let mut map = empty_map();
        apply_msg(&mut map, OverlayMsg::Remove("late".into()));
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        arrival_register("late".into(), tx);
        apply_msg(&mut map, move_msg("late", 50.0, 50.0));
        assert!(!map.cursors.contains_key("late"));
        assert!(rx.try_recv().is_ok());
        apply_msg(&mut map, OverlayMsg::Revive("late".into()));
        apply_msg(&mut map, move_msg("late", 50.0, 50.0));
        assert_eq!(map.cursors["late"].core.pos, (2.0, 2.0));
        apply_msg(&mut map, move_msg("late", 80.0, 80.0));
        assert_eq!(map.cursors["late"].core.pos, (2.0, 2.0));
    }

    #[test]
    fn slice_a_contact_and_snap_initialize_at_resolved_position() {
        for cmd in [
            OverlayCommand::SnapTo {
                x: 40.0,
                y: 45.0,
                heading_radians: None,
            },
            OverlayCommand::ClickPulse { x: 40.0, y: 45.0 },
        ] {
            let mut map = empty_map();
            apply_msg(&mut map, command("contact", cmd));
            let core = &map.cursors["contact"].core;
            assert!(core.placed);
            assert_eq!(core.pos, (40.0, 45.0));
            assert!(core.path.is_none());
        }
    }

    #[tokio::test]
    async fn slice_a_seed_never_changes_logical_target() {
        // Replace only the queue and native thread boundary. Both the resolved
        // visual publisher and renderer command processing remain production code.
        struct RenderSink(Mutex<RenderMap>);
        #[async_trait::async_trait]
        impl super::super::visual::PointerVisualSink for RenderSink {
            fn send(&self, key: &str, cmd: OverlayCommand) {
                apply_msg(&mut self.0.lock().unwrap(), command(key, cmd));
            }
            async fn travel(&self, key: &str, x: f64, y: f64) {
                apply_msg(&mut self.0.lock().unwrap(), move_msg(key, x, y));
            }
        }
        let registry = super::super::CursorRegistry::new();
        let sink = RenderSink(Mutex::new(empty_map()));
        super::super::visual::emit_pointer_target(
            &registry,
            &sink,
            "intent",
            Some(super::super::visual::ResolvedPointerTarget {
                x: 60.0,
                y: 60.0,
                window_id: Some(123),
                element_bounds: None,
            }),
        )
        .await;
        assert_eq!(
            sink.0.lock().unwrap().cursors["intent"].core.pos,
            (2.0, 2.0)
        );
        let position = registry.get("intent").unwrap().position.unwrap();
        assert_eq!((position.x, position.y), (60.0, 60.0));
        assert_eq!(
            sink.0.lock().unwrap().cursors["intent"].core.pinned_wid,
            Some(123)
        );
    }

    #[test]
    fn slice_a_tiny_display_seed_stays_inside() {
        let mut map = empty_map();
        map.layout.displays[0].width = 1.0;
        map.layout.displays[0].height = 0.5;
        apply_msg(&mut map, move_msg("tiny", 0.1, 0.1));
        let p = map.cursors["tiny"].core.pos;
        assert!(p.0 > 0.0 && p.0 < 1.0 && p.1 > 0.0 && p.1 < 0.5, "{p:?}");
    }

    #[test]
    fn keyed_render_state_carries_the_session_color_identity() {
        let state = render_state_for_key(&CursorConfig::default(), "session-blueprint");
        assert_eq!(state.core.cfg.cursor_id, "session-blueprint");
    }

    fn window(window_id: u32, pid: i32, z_index: usize) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: format!("app-{pid}"),
            title: String::new(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            z_index,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    fn empty_map() -> RenderMap {
        let mut cursors = IndexMap::new();
        cursors.insert(
            "default".to_owned(),
            RenderState::new(CursorConfig::default()),
        );
        RenderMap {
            cursors,
            layout: DisplayLayout {
                generation: 1,
                displays: vec![DisplayGeometry {
                    id: 1,
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                    backing_scale: 1.0,
                    is_primary: true,
                }],
            },
            command_order: Vec::new(),
            template: CursorConfig::default(),
            ended: HashSet::new(),
        }
    }

    #[test]
    fn frontmost_target_can_raise_overlay_without_covering_another_app() {
        let target_pid = 100;
        let mut windows = vec![window(10, target_pid, 20), window(11, 200, 10)];
        // WindowServer may retain another app's window ahead in its global
        // list; the active app identity is the authoritative cross-app guard.
        windows[1].z_index = 30;
        assert!(target_is_frontmost_visible_window(
            10,
            Some(target_pid),
            &windows,
        ));

        // A different foreground app blocks the fallback raise.
        assert!(!target_is_frontmost_visible_window(10, Some(200), &windows,));

        // So does another visible window belonging to the active target app.
        windows.push(window(13, target_pid, 40));
        assert!(!target_is_frontmost_visible_window(
            10,
            Some(target_pid),
            &windows,
        ));
    }

    fn move_msg(key: &str, x: f64, y: f64) -> OverlayMsg {
        OverlayMsg::Cmd(KeyedOverlayCommand {
            key: key.to_owned(),
            cmd: OverlayCommand::MoveTo {
                x,
                y,
                end_heading_radians: 0.0,
            },
        })
    }

    #[test]
    fn two_sessions_produce_two_distinct_render_entries() {
        let mut map = empty_map();
        apply_msg(
            &mut map,
            OverlayMsg::Cmd(KeyedOverlayCommand {
                key: "sessA".to_owned(),
                cmd: OverlayCommand::SetEnabled(true),
            }),
        );
        apply_msg(&mut map, move_msg("sessB", 42.0, 24.0));
        // default + sessA + sessB = 3 distinct owned cursors (the core
        // regression today is that they would clobber to one).
        assert_eq!(map.cursors.len(), 3);
        assert!(map.cursors.contains_key("sessA"));
        assert!(map.cursors.contains_key("sessB"));
        assert!(map.cursors.contains_key("default"));
    }

    #[test]
    fn session_end_removes_only_that_session() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        apply_msg(&mut map, move_msg("sessB", 20.0, 20.0));
        assert_eq!(map.cursors.len(), 3);

        // session_end(A): A gone, B + default retained.
        apply_msg(&mut map, OverlayMsg::Remove("sessA".to_owned()));
        assert!(!map.cursors.contains_key("sessA"));
        assert!(map.cursors.contains_key("sessB"));
        assert!(map.cursors.contains_key("default"));
        assert_eq!(map.cursors.len(), 2);

        // Remove("default") is guarded — default survives.
        apply_msg(&mut map, OverlayMsg::Remove("default".to_owned()));
        assert!(map.cursors.contains_key("default"));

        // Remove of an absent key (anonymous session that never created a
        // cursor) is a harmless no-op.
        let before = map.cursors.len();
        apply_msg(&mut map, OverlayMsg::Remove("never-existed".to_owned()));
        assert_eq!(map.cursors.len(), before);
    }

    #[test]
    fn lazily_created_cursors_inherit_the_selected_theme() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        apply_msg(&mut map, move_msg("sessB", 20.0, 20.0));
        assert_eq!(
            map.cursors["sessA"].core.cfg.theme_id,
            map.cursors["default"].core.cfg.theme_id
        );
        assert_eq!(
            map.cursors["sessB"].core.cfg.theme_id,
            map.cursors["default"].core.cfg.theme_id
        );
    }

    #[test]
    fn insertion_order_is_stable_z_order() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("first", 1.0, 1.0));
        apply_msg(&mut map, move_msg("second", 2.0, 2.0));
        // Re-touching "first" must NOT move it to the back (IndexMap keeps the
        // original insertion slot), so z-order is stable frame to frame.
        apply_msg(&mut map, move_msg("first", 3.0, 3.0));
        let keys: Vec<&String> = map.cursors.keys().collect();
        assert_eq!(keys, vec!["default", "first", "second"]);
    }

    #[test]
    fn tombstone_blocks_resurrection_after_remove() {
        // The resurrection race: a Cmd for a session lands AFTER its Remove
        // (an in-flight click from another task as the session dies). The
        // tombstone must drop it so the just-removed cursor is NOT re-created.
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        assert_eq!(map.cursors.len(), 2); // default + sessA

        // Session ends → cursor removed, key tombstoned.
        apply_msg(&mut map, OverlayMsg::Remove("sessA".to_owned()));
        assert!(!map.cursors.contains_key("sessA"));
        assert_eq!(map.cursors.len(), 1);

        // A late in-flight Cmd for the ended session must be dropped WITHOUT
        // re-inserting (no get-or-create resurrection).
        apply_msg(&mut map, move_msg("sessA", 99.0, 99.0));
        assert!(
            !map.cursors.contains_key("sessA"),
            "tombstone must block resurrection"
        );
        assert_eq!(
            map.cursors.len(),
            1,
            "render map length must stay at default only"
        );
    }

    #[test]
    fn explicit_revival_clears_tombstone_and_recreates_lazily() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        apply_msg(&mut map, OverlayMsg::Remove("sessA".to_owned()));
        apply_msg(&mut map, move_msg("sessA", 20.0, 20.0));
        assert!(!map.cursors.contains_key("sessA"));

        apply_msg(&mut map, OverlayMsg::Revive("sessA".to_owned()));
        assert!(!map.cursors.contains_key("sessA"));
        assert!(!map.ended.contains("sessA"));

        apply_msg(&mut map, move_msg("sessA", 30.0, 30.0));
        assert!(map.cursors.contains_key("sessA"));
        assert_eq!(map.command_order.last().map(String::as_str), Some("sessA"));
    }

    #[test]
    fn default_is_never_tombstoned() {
        // Remove("default") is guarded, so default is never tombstoned and a
        // subsequent Cmd on default still renders.
        let mut map = empty_map();
        apply_msg(&mut map, OverlayMsg::Remove("default".to_owned()));
        assert!(map.cursors.contains_key("default"));
        assert!(!map.ended.contains("default"));

        apply_msg(&mut map, move_msg("default", 5.0, 5.0));
        assert!(map.cursors.contains_key("default"));
        assert_eq!(
            map.command_order.last().map(String::as_str),
            Some("default")
        );
    }

    #[test]
    fn seed_places_cursor_inside_a_display_for_first_action() {
        // The first MoveTo needs a display-valid start to produce a visible glide.
        let mut map = empty_map(); // 100x100 frame
                                   // No "sessA" cursor exists yet — the seed must get-or-create it.
        let seeded = seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        assert!(seeded, "unplaced cursor must be seeded");
        let pos = map.cursors["sessA"].core.pos;
        assert!(
            map.layout.display_at(pos.0, pos.1).is_some(),
            "seed must be inside a display, got {pos:?}"
        );
        // It must differ from the target so there is a glide.
        assert!(
            (pos.0 - 60.0).abs() > 4.0 || (pos.1 - 60.0).abs() > 4.0,
            "seed must differ from target to produce a visible glide, got {pos:?}"
        );
    }

    #[test]
    fn seed_is_noop_when_cursor_is_already_placed() {
        let mut map = empty_map();
        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        map.cursors.get_mut("sessA").unwrap().core.pos = (30.0, 30.0);
        map.cursors.get_mut("sessA").unwrap().core.placed = true;
        let seeded_again = seed_start_in_map(&mut map, &"sessA".to_owned(), 80.0, 80.0);
        assert!(!seeded_again, "placed cursor must not be re-seeded");
        assert_eq!(
            map.cursors["sessA"].core.pos,
            (30.0, 30.0),
            "pos must be untouched"
        );
    }

    #[test]
    fn negative_display_coordinates_are_visible_and_not_reseeded() {
        let mut map = empty_map();
        map.layout.displays.push(DisplayGeometry {
            id: 2,
            x: -1440.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
            backing_scale: 1.0,
            is_primary: false,
        });
        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        map.cursors.get_mut("sessA").unwrap().core.pos = (-867.0, 400.0);
        map.cursors.get_mut("sessA").unwrap().core.placed = true;

        assert!(!seed_start_in_map(
            &mut map,
            &"sessA".to_owned(),
            80.0,
            80.0
        ));
        assert_eq!(map.cursors["sessA"].core.pos, (-867.0, 400.0));
        assert!(cursor_is_visible(&map.cursors["sessA"]));

        apply_msg(&mut map, move_msg("sessA", -500.0, 400.0));
        assert_eq!(
            map.cursors["sessA"].core.pos,
            (-867.0, 400.0),
            "negative placement must remain the path start"
        );
    }

    #[test]
    fn negative_x_cursor_paints_into_the_secondary_display_buffer() {
        let mut map = empty_map();
        let secondary = DisplayGeometry {
            id: 2,
            x: -1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            backing_scale: 1.0,
            is_primary: false,
        };
        map.layout.displays.push(secondary);
        seed_start_in_map(&mut map, &"sessA".to_owned(), -867.0, 400.0);
        map.cursors.get_mut("sessA").unwrap().core.pos = (-867.0, 400.0);
        map.cursors.get_mut("sessA").unwrap().core.placed = true;

        assert_eq!(painted_display_ids(&map), HashSet::from([2]));
        let pixmap = render_display(&map, secondary);
        assert!(pixmap.data().chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn cursor_art_routes_to_both_sides_of_a_display_seam_then_clears() {
        let mut map = empty_map();
        map.layout.displays[0].width = 1440.0;
        map.layout.displays[0].height = 900.0;
        let secondary = DisplayGeometry {
            id: 2,
            x: -1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            backing_scale: 1.0,
            is_primary: false,
        };
        map.layout.displays.push(secondary);
        seed_start_in_map(&mut map, &"sessA".to_owned(), -867.0, 400.0);

        map.cursors.get_mut("sessA").unwrap().core.pos = (-1.0, 400.0);
        map.cursors.get_mut("sessA").unwrap().core.placed = true;
        assert_eq!(painted_display_ids(&map), HashSet::from([1, 2]));
        let primary = map.layout.displays[0];
        let primary_art = render_display(&map, primary);
        let secondary_art = render_display(&map, secondary);
        for pixmap in [&primary_art, &secondary_art] {
            assert!(pixmap.data().chunks_exact(4).any(|pixel| pixel[3] > 0));
        }

        map.cursors
            .get_mut("sessA")
            .unwrap()
            .apply_command(OverlayCommand::SetSessionLabel("research".to_owned()));
        assert_eq!(
            render_display(&map, primary).data(),
            primary_art.data(),
            "the neighboring display must not duplicate the anchor display's badge"
        );
        assert_ne!(render_display(&map, secondary).data(), secondary_art.data());

        map.cursors.get_mut("sessA").unwrap().core.pos = (400.0, 400.0);
        map.cursors.get_mut("sessA").unwrap().core.placed = true;
        assert_eq!(painted_display_ids(&map), HashSet::from([1]));
        assert!(render_display(&map, secondary)
            .data()
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 0));
    }

    #[test]
    fn z_order_follows_painted_surfaces_and_layout_generation() {
        let mut map = empty_map();
        map.layout.displays[0].width = 1440.0;
        map.layout.displays[0].height = 900.0;
        map.layout.displays.push(DisplayGeometry {
            id: 2,
            x: -1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            backing_scale: 1.0,
            is_primary: false,
        });
        seed_start_in_map(&mut map, &"sessA".to_owned(), 400.0, 400.0);
        apply_msg(
            &mut map,
            OverlayMsg::Cmd(KeyedOverlayCommand {
                key: "sessA".to_owned(),
                cmd: OverlayCommand::PinAbove(77),
            }),
        );

        assert_eq!(
            z_order_routes(&map),
            vec![ZOrderRoute {
                generation: 1,
                display_id: 1,
                target_wid: Some(77),
            }]
        );

        map.cursors.get_mut("sessA").unwrap().core.pos = (-1.0, 400.0);
        assert_eq!(ordering_pairs(&map), vec![(1, Some(77)), (2, Some(77))]);

        map.cursors.get_mut("sessA").unwrap().core.pos = (-400.0, 400.0);
        assert_eq!(ordering_pairs(&map), vec![(2, Some(77))]);

        map.layout.generation = 2;
        assert_eq!(z_order_routes(&map)[0].generation, 2);
    }

    #[test]
    fn seed_does_not_resurrect_ended_session() {
        // The seed shares the resurrection guard: it must not re-create a cursor
        // whose session already ended.
        let mut map = empty_map();
        map.ended.insert("sessA".to_owned());
        let seeded = seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        assert!(!seeded, "ended session must not be seeded");
        assert!(
            !map.cursors.contains_key("sessA"),
            "ended session must not be resurrected"
        );
    }

    #[test]
    fn unplaced_default_cursor_does_not_require_frame_ticks() {
        // An untouched cursor must let the render loop block instead of
        // repainting at 60 fps.
        let map = empty_map();
        assert!(!render_map_needs_frame_tick(&map));
    }

    #[test]
    fn only_enabled_placed_cursor_is_visible() {
        let mut map = empty_map();
        assert!(!cursor_is_visible(&map.cursors["default"]));

        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        assert!(cursor_is_visible(&map.cursors["sessA"]));

        map.cursors.get_mut("sessA").unwrap().core.cfg.enabled = false;
        assert!(!cursor_is_visible(&map.cursors["sessA"]));
    }

    #[test]
    fn active_or_fading_cursor_requires_frame_ticks() {
        let mut map = empty_map();
        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        apply_msg(&mut map, move_msg("sessA", 80.0, 80.0));
        assert!(
            render_map_needs_frame_tick(&map),
            "planned path should tick"
        );

        let rs = map.cursors.get_mut("sessA").unwrap();
        rs.core.path = None;
        rs.core.spring = None;
        rs.core.click_t = None;
        rs.focus_rect = None;
        rs.core.idle_alpha = 0.0;
        assert!(
            !render_map_needs_frame_tick(&map),
            "fully hidden idle cursor should quiesce"
        );
    }

    #[test]
    fn per_key_arrival_isolation() {
        // Two concurrent waiters keyed A and B; firing A must not cancel B.
        // This mirrors the ARRIVAL_TX HashMap logic in isolation (no statics).
        let mut waiters: HashMap<CursorKey, tokio::sync::oneshot::Sender<()>> = HashMap::new();
        let (txa, mut rxa) = tokio::sync::oneshot::channel::<()>();
        let (txb, mut rxb) = tokio::sync::oneshot::channel::<()>();
        waiters.insert("A".to_owned(), txa);
        waiters.insert("B".to_owned(), txb);

        // Fire A's arrival.
        if let Some(tx) = waiters.remove("A") {
            let _ = tx.send(());
        }
        // A resolved, B still pending.
        assert!(matches!(rxa.try_recv(), Ok(())));
        assert!(matches!(
            rxb.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }
}
