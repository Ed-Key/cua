//! macOS agent-cursor overlay — transparent click-through NSWindow.
//!
//! ## Architecture
//!
//! The MCP/tokio server runs on a **background thread** (spawned in
//! `cua-driver/src/main.rs`).  AppKit MUST run on the **main thread**.
//! Producers publish into a small process-global inbox:
//!
//! - Legacy commands retain a bounded queue; action events coalesce per session.
//! - A capacity-one notification wakes the renderer without carrying visual history.
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

// ── Global overlay state ──────────────────────────────────────────────────

enum MacOverlayMsg {
    Wake,
}

static CMD_TX: OnceLock<std::sync::mpsc::SyncSender<MacOverlayMsg>> = OnceLock::new();
// Single-consumer slot; receiver is moved into run_on_main_thread().
static CMD_RX_CELL: Mutex<Option<std::sync::mpsc::Receiver<MacOverlayMsg>>> = Mutex::new(None);
static INBOX: OnceLock<Mutex<OverlayInbox>> = OnceLock::new();
static RENDER: Mutex<Option<RenderMap>> = Mutex::new(None);
static HOST: Mutex<Option<AppKitOverlayHost>> = Mutex::new(None);
static DISPLAY_GENERATION: AtomicU64 = AtomicU64::new(0);

// Only this small inbox is shared with action publishers.
#[derive(Default)]
struct OverlayInbox {
    visual: cursor_overlay::VisualMailbox,
    commands: Vec<(u64, OverlayMsg)>,
    motion: HashMap<CursorKey, MotionConfig>,
    template_motion: MotionConfig,
    explicitly_disabled: bool,
    enabled_overrides: HashMap<CursorKey, bool>,
    approaches: HashMap<CursorKey, ApproachRegistration>,
    applied_routes: HashMap<DisplayId, ZOrderRoute>,
    // Generic admission hands its context to one resolved action. Subsequent
    // events carry that action's context even after presentation expires.
    contexts: HashMap<
        CursorKey,
        (
            cursor_overlay::VisualActionId,
            cua_driver_contract::CursorSemantics,
            bool,
        ),
    >,
}

struct ApproachRegistration {
    surface_generation: u64,
    event: cursor_overlay::VisualEvent,
    sender: Option<tokio::sync::oneshot::Sender<()>>,
    deadline: tokio::time::Instant,
    presented: Option<(u64, DisplayId)>,
    timing: ApproachTiming,
}

/// Fixed-size per-action evidence, emitted once off AppKit main at guard cleanup.
#[derive(Debug)]
struct ApproachTiming {
    started: Instant,
    first_frame_ms: Option<f64>,
    target_frame_ms: Option<f64>,
    submission_ms: Option<f64>,
    acknowledgement_ms: Option<f64>,
    frames: u32,
    geometry_matches: bool,
    route_matches: bool,
    surface_matches: bool,
}
impl ApproachTiming {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            first_frame_ms: None,
            target_frame_ms: None,
            submission_ms: None,
            acknowledgement_ms: None,
            frames: 0,
            geometry_matches: false,
            route_matches: false,
            surface_matches: false,
        }
    }
    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
}

#[derive(Clone)]
struct TargetFrame {
    generation: u64,
    display_id: DisplayId,
    key: CursorKey,
    event: cursor_overlay::VisualEvent,
}

fn same_target(a: &cursor_overlay::VisualEvent, b: &cursor_overlay::VisualEvent) -> bool {
    a.id == b.id && a.target == b.target && a.window == b.window
}

impl OverlayInbox {
    fn approach_enabled(&self, key: &str) -> bool {
        self.enabled_overrides
            .get(key)
            .copied()
            .unwrap_or(!self.explicitly_disabled)
    }

    fn register_target(
        &mut self,
        key: &str,
        event: &cursor_overlay::VisualEvent,
        surface_generation: u64,
    ) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
        if !self.approach_enabled(key)
            || !self.visual.owns_action(key, event.id)
            || !event.is_valid()
            || event.target.is_none()
        {
            return Err("click target or action is no longer eligible".into());
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.approaches.insert(
            key.into(),
            ApproachRegistration {
                event: event.clone(),
                sender: Some(sender),
                presented: None,
                timing: ApproachTiming::new(),
                surface_generation,
                deadline: tokio::time::Instant::now() + Duration::from_millis(250),
            },
        );
        Ok(receiver)
    }

    fn release_target(
        &mut self,
        key: &str,
        event: &cursor_overlay::VisualEvent,
    ) -> Option<ApproachRegistration> {
        if self
            .approaches
            .get(key)
            .is_some_and(|pending| same_target(&pending.event, event))
        {
            self.approaches.remove(key)
        } else {
            None
        }
    }

    fn target_current(
        &self,
        key: &str,
        event: &cursor_overlay::VisualEvent,
        generation: u64,
    ) -> Result<(), String> {
        if self.approach_enabled(key)
            && self.visual.owns_action(key, event.id)
            && self.approaches.get(key).is_some_and(|pending| {
                same_target(&pending.event, event)
                    && pending
                        .presented
                        .is_some_and(|(surface_generation, display)| {
                            surface_generation == generation
                                && self.applied_routes.get(&display).is_some_and(|route| {
                                    route.generation == generation
                                        && route.target_wid == event.window
                                })
                        })
            })
        {
            Ok(())
        } else {
            Err("click target presentation was invalidated before input".into())
        }
    }

    fn invalidate_surface_frame(&mut self, generation: u64, display: DisplayId) {
        for pending in self.approaches.values_mut() {
            if pending.presented == Some((generation, display)) {
                pending.presented = None;
            }
        }
    }

    /// Replacing a target by the same proven geometry does not withdraw readiness
    /// during Core Animation submission. Omitted or obsolete targets do.
    fn prepare_surface_frame(
        &mut self,
        generation: u64,
        display: DisplayId,
        targets: &[TargetFrame],
    ) {
        for (key, pending) in &mut self.approaches {
            if pending.presented != Some((generation, display)) {
                continue;
            }
            let retained = self.visual.owns_action(key, pending.event.id)
                && self.applied_routes.get(&display).is_some_and(|route| {
                    route.generation == generation && route.target_wid == pending.event.window
                })
                && targets.iter().any(|target| {
                    target.generation == generation
                        && target.display_id == display
                        && target.key == *key
                        && same_target(&target.event, &pending.event)
                });
            if !retained {
                pending.presented = None;
            }
        }
    }

    // Called only after this frame's actual layer contents were applied and submitted.
    fn acknowledge_targets(
        &mut self,
        generation: u64,
        display: DisplayId,
        current_generation: u64,
        targets: &[TargetFrame],
    ) {
        if generation != current_generation {
            return;
        }
        // Readiness describes the last applied frame on that surface. A later
        // frame cannot silently leave a previously ready arrow acknowledged.
        self.invalidate_surface_frame(generation, display);
        for target in targets {
            if target.generation != generation || target.display_id != display {
                continue;
            }
            if !self.applied_routes.get(&display).is_some_and(|route| {
                route.generation == generation && route.target_wid == target.event.window
            }) || !self.visual.owns_action(&target.key, target.event.id)
            {
                continue;
            }
            if let Some(pending) = self.approaches.get_mut(&target.key) {
                if pending.surface_generation == generation
                    && same_target(&pending.event, &target.event)
                    && (pending.sender.is_none() || tokio::time::Instant::now() < pending.deadline)
                {
                    pending.presented = Some((generation, display));
                    if let Some(sender) = pending.sender.take() {
                        pending.timing.acknowledgement_ms = Some(pending.timing.elapsed_ms());
                        let _ = sender.send(());
                    }
                }
            }
        }
    }

    fn begin_action(&mut self, key: &str) -> Option<cursor_overlay::VisualActionId> {
        let id = self.visual.begin_action(key)?;
        self.approaches.remove(key);
        if let Some((_, semantics, true)) = self.contexts.remove(key) {
            self.contexts.insert(key.into(), (id, semantics, false));
        }
        Some(id)
    }
    fn publish(&mut self, key: &str, mut event: cursor_overlay::VisualEvent) -> bool {
        if let Some((id, semantics, _)) = self.contexts.get(key) {
            // Explicit native invocation context wins over the one-use fallback.
            if *id == event.id && event.modifiers.is_none() {
                event.modifiers = Some((semantics.delivery, semantics.target));
            }
        }
        let accepted = self.visual.publish(key, event.clone());
        if accepted
            && self.approaches.get(key).is_some_and(|pending| {
                !same_target(&pending.event, &event)
                    || event.phase == cursor_overlay::VisualPhase::End
            })
        {
            self.approaches.remove(key);
        }
        accepted
    }
    fn command(&mut self, message: OverlayMsg) {
        match message {
            OverlayMsg::Remove(key) => {
                self.visual.remove(&key);
                self.approaches.remove(&key);
                self.enabled_overrides.remove(&key);
                if key != "default" {
                    self.contexts.remove(&key);
                    self.motion.remove(&key);
                }
            }
            OverlayMsg::Revive(key) => {
                if !key.is_empty() && !self.visual.accepts_key(&key) {
                    self.motion
                        .insert(key.clone(), self.template_motion.clone());
                }
                self.visual.revive(&key);
            }
            message => {
                // Preserve the legacy bounded, drop-newest command behavior.
                if self.commands.len() < 4096 {
                    if let OverlayMsg::Cmd(ref keyed) = message {
                        if !self.visual.accepts_key(&keyed.key) {
                            return;
                        }
                        match &keyed.cmd {
                            OverlayCommand::SetEnabled(enabled) => {
                                self.enabled_overrides.insert(keyed.key.clone(), *enabled);
                                self.approaches.remove(&keyed.key);
                            }
                            OverlayCommand::MoveTo { .. }
                            | OverlayCommand::SnapTo { .. }
                            | OverlayCommand::SetTheme { .. }
                            | OverlayCommand::BeginAction { .. } => {
                                self.approaches.remove(&keyed.key);
                            }
                            OverlayCommand::PinAbove(window) => {
                                if self
                                    .approaches
                                    .get(&keyed.key)
                                    .is_some_and(|p| p.event.window != Some(*window))
                                {
                                    self.approaches.remove(&keyed.key);
                                }
                            }
                            _ => {}
                        }
                        let motion = self
                            .motion
                            .entry(keyed.key.clone())
                            .or_insert_with(|| self.template_motion.clone());
                        if let OverlayCommand::SetMotion(ref update) = keyed.cmd {
                            *motion = update.clone();
                        }
                    }
                    let order = self.visual.next_order();
                    self.commands.push((order, message));
                }
            }
        }
    }
    fn semantic(&mut self, event: cua_driver_core::cursor_events::CursorEvent) {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        match event {
            CursorEvent::SetSessionLabel { session, label } => {
                self.command(OverlayMsg::Cmd(KeyedOverlayCommand {
                    key: session,
                    cmd: OverlayCommand::SetSessionLabel(label),
                }))
            }
            CursorEvent::SelectTheme { session, selection } => {
                self.command(OverlayMsg::Cmd(KeyedOverlayCommand {
                    key: session,
                    cmd: OverlayCommand::SetTheme {
                        theme_id: selection.theme_id,
                        reduced_motion: selection.reduced_motion,
                    },
                }))
            }
            CursorEvent::Action {
                session,
                phase: CursorEventPhase::Begin,
                semantics,
            } => {
                // The core context has no invocation identity. Use a bounded fallback
                // cue at Begin; resolved handles take ownership later. An unscoped
                // End must never release a newer resolved action of the same kind.
                if let Some(id) = self.visual.begin_action(&session) {
                    self.contexts.insert(session.clone(), (id, semantics, true));
                    self.command(OverlayMsg::Cmd(KeyedOverlayCommand {
                        key: session.clone(),
                        cmd: OverlayCommand::BeginAction {
                            action: semantics.action,
                            delivery: semantics.delivery,
                            target: semantics.target,
                        },
                    }));
                    self.publish(
                        &session,
                        cursor_overlay::VisualEvent {
                            id,
                            timestamp: Instant::now(),
                            target: None,
                            window: None,
                            bounds: None,
                            action: semantics.action,
                            scroll_direction: None,
                            modifiers: None,
                            phase: cursor_overlay::VisualPhase::Intent,
                        },
                    );
                }
            }
            CursorEvent::Action {
                phase: CursorEventPhase::End,
                ..
            } => {}
        }
    }
    fn take(&mut self) -> InboxBatch {
        InboxBatch {
            commands: std::mem::take(&mut self.commands),
            pending: self.visual.take_pending(),
        }
    }
}

struct InboxBatch {
    commands: Vec<(u64, OverlayMsg)>,
    pending: HashMap<CursorKey, cursor_overlay::PendingVisualState>,
}

impl InboxBatch {
    fn apply(self, map: &mut RenderMap, now: Instant) -> bool {
        enum Item {
            Command(OverlayMsg),
            Visual(CursorKey, cursor_overlay::VisualEvent),
            Lifecycle(CursorKey, cursor_overlay::VisualLifecycle),
        }
        let mut items: Vec<_> = self
            .commands
            .into_iter()
            .map(|(order, message)| (order, Item::Command(message)))
            .collect();
        for (key, pending) in self.pending {
            if let Some((order, lifecycle)) = pending.lifecycle {
                items.push((order, Item::Lifecycle(key.clone(), lifecycle)));
            }
            for published in [pending.latest, pending.contact, pending.end]
                .into_iter()
                .flatten()
            {
                items.push((published.order, Item::Visual(key.clone(), published.event)));
            }
        }
        // Sorting and all renderer work happen on the detached batch.
        items.sort_by_key(|(order, _)| *order);
        let had_work = !items.is_empty();
        for (_, item) in items {
            match item {
                Item::Command(message) => apply_msg(map, message),
                Item::Lifecycle(key, lifecycle) => {
                    // Revive also removes an old render instance if Remove coalesced away.
                    apply_msg(map, OverlayMsg::Remove(key.clone()));
                    if lifecycle == cursor_overlay::VisualLifecycle::Revive {
                        apply_msg(map, OverlayMsg::Revive(key));
                    }
                }
                Item::Visual(key, event) => apply_visual_in_map(map, key, event, now),
            }
        }
        had_work
    }
}

fn apply_visual_in_map(
    map: &mut RenderMap,
    key: CursorKey,
    event: cursor_overlay::VisualEvent,
    now: Instant,
) {
    if key.is_empty() || map.ended.contains(&key) {
        return;
    }
    let target = (event.phase != cursor_overlay::VisualPhase::End)
        .then_some(event.target)
        .flatten();
    let display = target.and_then(|(x, y)| map.layout.display_at(x, y));
    if target.is_some() && (display.is_none() || !animation_enabled(map, &key)) {
        let state = map
            .cursors
            .entry(key.clone())
            .or_insert_with(|| render_state_for_key(&map.template, &key));
        state.target = target;
        state.invalidate_placement();
        return;
    }
    let bounds = display.map(|display| cursor_overlay::DisplayBounds {
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
    });
    let state = map
        .cursors
        .entry(key.clone())
        .or_insert_with(|| render_state_for_key(&map.template, &key));
    let quick = event.action == cursor_overlay::CursorAction::Click
        && event.phase == cursor_overlay::VisualPhase::Intent;
    if quick {
        tracing::debug!(target: "cua_cursor_approach", stage = "intent_drained", id = ?event.id,
            age_ms = event.timestamp.elapsed().as_secs_f64() * 1000.0,
            "click intent reached render worker");
    }
    let accepted = if quick {
        state.core.apply_click_approach(event, bounds, now)
    } else {
        state.core.apply_visual_event(event, bounds, now)
    };
    if accepted {
        if target.is_some() {
            state.target = target;
        }
        map.command_order.retain(|candidate| candidate != &key);
        map.command_order.push(key);
    }
}

fn inbox() -> &'static Mutex<OverlayInbox> {
    INBOX.get_or_init(|| Mutex::new(OverlayInbox::default()))
}

fn wake_renderer() {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(MacOverlayMsg::Wake);
    }
}

pub(crate) fn approach_enabled(key: &str) -> bool {
    inbox().lock().unwrap().approach_enabled(key)
}
pub(crate) fn register_target(
    key: &str,
    event: &cursor_overlay::VisualEvent,
) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
    let receiver = inbox().lock().unwrap().register_target(
        key,
        event,
        DISPLAY_GENERATION.load(Ordering::Acquire),
    )?;
    tracing::debug!(target: "cua_cursor_approach", stage = "registered", id = ?event.id,
        "click approach registered");
    wake_renderer();
    Ok(receiver)
}
pub(crate) fn target_current(key: &str, event: &cursor_overlay::VisualEvent) -> Result<(), String> {
    inbox()
        .lock()
        .unwrap()
        .target_current(key, event, DISPLAY_GENERATION.load(Ordering::Acquire))
}
pub(crate) fn release_target(key: &str, event: &cursor_overlay::VisualEvent) {
    let released = inbox().lock().unwrap().release_target(key, event);
    if let Some(pending) = released {
        tracing::debug!(target: "cua_cursor_approach", stage = "released", id = ?event.id,
            surface_generation = pending.surface_generation, timing = ?pending.timing,
            elapsed_ms = pending.timing.elapsed_ms(), "click approach frame evidence");
    }
}

/// Allocate an action in the active session without reading renderer state.
pub fn begin_visual_action(key: &str) -> Option<cursor_overlay::VisualActionId> {
    inbox().lock().unwrap().begin_action(key)
}

/// Publish presentation state without waiting for path planning or a renderer acknowledgement.
pub fn publish_visual_event(key: &str, event: cursor_overlay::VisualEvent) -> bool {
    let accepted = {
        let mut inbox = inbox().lock().unwrap();
        let accepted = inbox.publish(key, event);
        if accepted {
            let template = inbox.template_motion.clone();
            inbox.motion.entry(key.to_owned()).or_insert(template);
        }
        accepted
    };
    if accepted {
        wake_renderer();
    }
    accepted
}

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
            rs.apply_command(cmd);
            map.command_order.retain(|candidate| candidate != &key);
            map.command_order.push(key);
        }
    }
}

/// Initialise global overlay state (call once, before run_on_main_thread).
pub fn init(cfg: CursorConfig) {
    static INITIALIZED: OnceLock<()> = OnceLock::new();
    INITIALIZED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        CMD_TX
            .set(tx)
            .expect("cursor overlay sender is initialized exactly once");
        *CMD_RX_CELL.lock().unwrap() = Some(rx);
        {
            let mut inbox = inbox().lock().unwrap();
            inbox.template_motion = cfg.motion.clone();
            inbox.explicitly_disabled = !cfg.enabled;
        }
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
    cua_driver_core::cursor_events::install_cursor_event_sink(std::sync::Arc::new(|event| {
        inbox().lock().unwrap().semantic(event);
        wake_renderer();
    }));
}

/// Send a legacy keyed command. Configuration retains its bounded drop-newest behavior.
/// Action visuals use publish_visual_event and never enter this command queue.
pub fn send_command(key: CursorKey, cmd: OverlayCommand) {
    if key.is_empty() {
        return;
    }
    if CMD_TX.get().is_none() {
        return;
    }
    inbox()
        .lock()
        .unwrap()
        .command(OverlayMsg::Cmd(KeyedOverlayCommand { key, cmd }));
    wake_renderer();
}

/// Best-effort render acknowledgement for lifecycle completion. Contention means
/// no current acknowledgement (false), never a cached claim of visibility.
/// This reader cannot wait behind painting and never falls back to another key.
pub fn is_visible_for_session(key: &str) -> bool {
    RENDER
        .try_lock()
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
    inbox().lock().unwrap().command(OverlayMsg::Remove(key));
    wake_renderer();
}

/// Clear the render-side tombstone after a successful explicit session
/// revival. Cursor recreation remains lazy until the next render command.
pub fn revive_cursor(key: CursorKey) {
    if key.is_empty() {
        return;
    }
    inbox().lock().unwrap().command(OverlayMsg::Revive(key));
    wake_renderer();
}

/// Return the latest admitted motion configuration without a rendering lock.
/// Partial overrides compose with accepted configuration even before rendering.
/// Reads the motion of the cursor `key`, falling back to the
/// `"default"` cursor's motion when that key has no own entry yet (e.g. a
/// session whose first motion call precedes any move/enable).
pub fn current_motion(key: &str) -> MotionConfig {
    let inbox = inbox().lock().unwrap();
    inbox
        .motion
        .get(key)
        .or_else(|| inbox.motion.get("default"))
        .cloned()
        .unwrap_or_else(|| inbox.template_motion.clone())
}

/// Read-only diagnostic snapshot of rendered theme/playback. This alone retains
/// a blocking renderer lock; input admission and session completion never call it.
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

/// Legacy overlay placement retains the public configured duration, including zero.
/// Publication never reads rendering state or waits for placement.
pub fn publish_legacy_move(key: CursorKey, x: f64, y: f64) {
    send_command(
        key,
        OverlayCommand::MoveTo {
            x,
            y,
            end_heading_radians: std::f64::consts::FRAC_PI_4,
        },
    );
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
}

impl RenderState {
    fn new(cfg: CursorConfig) -> Self {
        RenderState {
            core: RenderStateCore::new(cfg),
            target: None,
        }
    }

    fn invalidate_placement(&mut self) {
        self.core.clear_visual_presentation();
        self.core.placed = false;
        self.core.path = None;
        self.core.spring = None;
        self.core.spring_tgt = None;
        self.core.dist = 0.0;
        self.core.click_t = None;
        self.core.session_badge_hovered = false;
    }

    fn tick_at(&mut self, dt: f64, now: Instant) -> bool {
        if !self.core.cfg.enabled || !self.core.visible || !self.core.placed {
            // Semantic-only actions still expire without inventing placement.
            self.core.advance_visual_presentation(now);
            return false;
        }
        self.core.tick_swift_constants_at(dt, now)
    }

    fn apply_command(&mut self, cmd: OverlayCommand) {
        // First contact places exactly at its resolved target. Subsequent
        // pulses retain the existing glide behavior until the motion task.
        match cmd {
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
        if self.core.has_timed_presentation() {
            return true;
        }
        if !self.core.cfg.enabled || !self.core.visible || !self.core.placed {
            return false;
        }
        self.core.path.is_some()
            || self.core.spring.is_some()
            || self.core.click_t.is_some()
            || self.core.contact.is_some()
            || self.core.focus_rect.is_some()
            || self.core.session_badge_needs_frame_tick()
            || (self.core.motion.idle_hide_ms > 0.0 && cursor_is_visible(self))
    }
}

fn render_map_needs_frame_tick(map: &RenderMap) -> bool {
    map.cursors.values().any(RenderState::needs_frame_tick)
}

fn render_frame_tick_needed(map: &RenderMap, inbox: &OverlayInbox) -> bool {
    render_map_needs_frame_tick(map)
        || inbox.approaches.values().any(|pending| {
            pending.sender.is_some() && tokio::time::Instant::now() < pending.deadline
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ZOrderRoute {
    generation: u64,
    display_id: DisplayId,
    target_wid: Option<u64>,
}

/// Invalidate before ordering, then publish the applied route after AppKit returns.
/// Both admission sections are nonblocking and never enclose native work.
fn apply_surface_route(
    admission: &Mutex<OverlayInbox>,
    route: ZOrderRoute,
    apply: impl FnOnce(),
) -> bool {
    let Ok(mut state) = admission.try_lock() else {
        return false;
    };
    if state.applied_routes.get(&route.display_id) != Some(&route) {
        state.invalidate_surface_frame(route.generation, route.display_id);
        state.applied_routes.remove(&route.display_id);
    }
    drop(state);
    apply();
    let Ok(mut state) = admission.try_lock() else {
        return false;
    };
    state
        .applied_routes
        .retain(|_, old| old.generation == route.generation);
    state.applied_routes.insert(route.display_id, route);
    true
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
    for state in map.cursors.values_mut() {
        if state
            .target
            .is_some_and(|(x, y)| layout.display_at(x, y).is_none())
        {
            state.invalidate_placement();
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
        wake_renderer();
    }));
}

fn render_loop(rx: std::sync::mpsc::Receiver<MacOverlayMsg>) {
    let target_frame_ms = Duration::from_millis(16);
    let hover_poll_ms = Duration::from_millis(80);
    let mut last_tick = Instant::now();
    let mut frame_tick_needed = false;
    let mut hover_poll_needed = false;
    let mut presented_displays = HashSet::<DisplayId>::new();
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
        let mut received_wake = woke_from_idle;
        let now = Instant::now();
        let dt = if woke_from_idle {
            0.0
        } else {
            now.duration_since(last_tick).as_secs_f64().min(0.05)
        };
        last_tick = now;

        // The notification carries no command history. Release the producer
        // lock before acquiring renderer state, planning paths or touching AppKit.
        while rx.try_recv().is_ok() {
            received_wake = true;
        }
        let batch = inbox().lock().unwrap().take();

        let (
            z_order,
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
            let cursor_commanded = batch.apply(map, now);
            let had_msg = received_wake || cursor_commanded;

            if frame_tick_needed || had_msg {
                for state in map.cursors.values_mut() {
                    state.tick_at(dt, now);
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
            let next_frame_tick_needed = render_frame_tick_needed(map, &inbox().lock().unwrap());
            let next_hover_poll_needed = map
                .cursors
                .values()
                .any(|state| state.core.session_badge_needs_hover_poll());

            (
                z_order,
                had_msg,
                cursor_commanded,
                hover_changed,
                next_frame_tick_needed,
                next_hover_poll_needed,
            )
        };

        if frame_tick_needed || had_msg {
            repin_frames += 1;
            let applied_routes: Vec<_> = inbox()
                .lock()
                .unwrap()
                .applied_routes
                .values()
                .cloned()
                .collect();
            for route in z_order_updates(
                &z_order,
                &applied_routes,
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
                                    target_frames(map, display, &mut inbox().lock().unwrap()),
                                )
                            })
                    })
                    .collect::<Vec<_>>();
                presented_displays = painted;
                frames
            };

            for (generation, display_id, pixmap, targets) in frames {
                dispatch_present(generation, display_id, pixmap, targets);
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
        .core
        .focus_rect
        .is_some_and(|[x, y, width, height]| rectangles_intersect((x, y, width, height), display));
    let contact_intersects = state.core.contact.is_some_and(|contact| {
        let radius = 34.0;
        rectangles_intersect(
            (
                contact.target.0 - radius,
                contact.target.1 - radius,
                radius * 2.0,
                radius * 2.0,
            ),
            display,
        )
    });
    cursor_intersects || focus_intersects || contact_intersects
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
        let focus = state.core.focus_rect.map(|rect| FocusRect {
            rect,
            t: state.core.focus_rect_t,
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

fn target_frames(
    map: &RenderMap,
    display: DisplayGeometry,
    inbox: &mut OverlayInbox,
) -> Vec<TargetFrame> {
    let route = z_order_routes(map)
        .into_iter()
        .find(|route| route.display_id == display.id);
    inbox
        .approaches
        .iter_mut()
        .filter_map(|(key, pending)| {
            let target = pending.event.target?;
            let state = map.cursors.get(key)?;
            let timing = &mut pending.timing;
            let elapsed = timing.elapsed_ms();
            timing.frames = timing.frames.saturating_add(1);
            timing.first_frame_ms.get_or_insert(elapsed);
            timing.surface_matches = pending.surface_generation == map.layout.generation
                && display.contains(target.0, target.1);
            timing.route_matches = route
                .as_ref()
                .is_some_and(|route| route.target_wid == pending.event.window);
            timing.geometry_matches = state.core.is_target_frame(&pending.event);
            let ready = timing.surface_matches && timing.route_matches && timing.geometry_matches;
            if ready {
                timing.target_frame_ms.get_or_insert(elapsed);
            }
            ready.then(|| TargetFrame {
                key: key.clone(),
                event: pending.event.clone(),
                generation: map.layout.generation,
                display_id: display.id,
            })
        })
        .collect()
}

/// Apply pixels without keeping admission locked across native submission.
fn apply_surface_frame(
    admission: &Mutex<OverlayInbox>,
    generation: u64,
    display: DisplayId,
    current_generation: impl Fn() -> u64,
    targets: &[TargetFrame],
    apply: impl FnOnce(),
) -> bool {
    if generation != current_generation() {
        return false;
    }
    let Ok(mut state) = admission.try_lock() else {
        return false;
    };
    state.prepare_surface_frame(generation, display, targets);
    for target in targets {
        if let Some(pending) = state.approaches.get_mut(&target.key) {
            if same_target(&pending.event, &target.event) {
                let elapsed = pending.timing.elapsed_ms();
                pending.timing.submission_ms.get_or_insert(elapsed);
            }
        }
    }
    drop(state);
    apply();
    let Ok(mut state) = admission.try_lock() else {
        return false;
    };
    state.acknowledge_targets(generation, display, current_generation(), targets);
    true
}

/// Present one display-local pixmap. AppKit objects remain main-thread-owned;
/// stale frames are rejected by display-layout generation.
fn dispatch_present(
    generation: u64,
    display_id: DisplayId,
    pixmap: tiny_skia::Pixmap,
    targets: Vec<TargetFrame>,
) {
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
            // AppKit/Core Animation submission, not physical scanout or occlusion proof.
            // Admission never encloses these calls, including a stalled flush.
            if !apply_surface_frame(
                inbox(),
                generation,
                display_id,
                || DISPLAY_GENERATION.load(Ordering::Acquire),
                &targets,
                || {
                    let _: () = objc2::msg_send![objc2::class!(CATransaction), begin];
                    let _: () =
                        objc2::msg_send![objc2::class!(CATransaction), setDisableActions: true];
                    let _: () = objc2::msg_send![layer, setContents: image];
                    let _: () = objc2::msg_send![objc2::class!(CATransaction), commit];
                    let _: () = objc2::msg_send![objc2::class!(CATransaction), flush];
                },
            ) {
                wake_renderer();
            }
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
            if !apply_surface_route(
                inbox(),
                ZOrderRoute {
                    generation,
                    display_id,
                    target_wid: None,
                },
                || {
                    let _: () = objc2::msg_send![win, orderFrontRegardless];
                },
            ) {
                wake_renderer();
            }
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
            if !apply_surface_route(
                inbox(),
                ZOrderRoute {
                    generation,
                    display_id,
                    target_wid: Some(target_wid),
                },
                || {
                    let _: () =
                        objc2::msg_send![win, orderWindow: 1i64 relativeTo: target_wid as i64];
                    if raise_front {
                        let _: () = objc2::msg_send![win, orderFrontRegardless];
                    }
                },
            ) {
                wake_renderer();
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

    fn mailbox_event(
        id: cursor_overlay::VisualActionId,
        x: f64,
        phase: cursor_overlay::VisualPhase,
    ) -> cursor_overlay::VisualEvent {
        cursor_overlay::VisualEvent {
            id,
            timestamp: Instant::now(),
            target: Some((x, 30.0)),
            window: Some(111),
            bounds: None,
            action: cursor_overlay::CursorAction::Click,
            scroll_direction: None,
            modifiers: None,
            phase,
        }
    }

    fn assert_mailbox_tip(map: &RenderMap, target: (f64, f64)) {
        let core = &map.cursors["one"].core;
        assert!((core.pos.0 - core.heading.cos() * 16.0 - target.0).abs() < 0.001);
        assert!((core.pos.1 - core.heading.sin() * 16.0 - target.1).abs() < 0.001);
    }

    fn newer_action_before_older_timestamp(applied: bool) {
        use cursor_overlay::VisualPhase::*;
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let t = Instant::now();
        let a = inbox.begin_action("one").unwrap();
        let b = inbox.begin_action("one").unwrap();
        // B is captured first, then descheduled while A's contact publishes.
        let mut intent = mailbox_event(b, 80.0, Intent);
        intent.timestamp = t + Duration::from_millis(10);
        let mut contact = mailbox_event(a, 20.0, Contact);
        contact.timestamp = t + Duration::from_millis(20);
        assert!(inbox.publish("one", contact));
        if applied {
            assert!(inbox.take().apply(&mut map, t + Duration::from_millis(20)));
            assert_mailbox_tip(&map, (20.0, 30.0));
        }
        assert!(inbox.publish("one", intent));
        assert!(inbox.take().apply(&mut map, t + Duration::from_millis(25)));
        assert_eq!(map.cursors["one"].target, Some((80.0, 30.0)));
        assert!(map.cursors["one"].core.path.is_some());
        let mut tracking = mailbox_event(b, 90.0, Tracking);
        tracking.timestamp = t + Duration::from_millis(15);
        assert!(inbox.publish("one", tracking.clone()));
        tracking.timestamp = t + Duration::from_millis(14);
        tracking.target = Some((50.0, 30.0));
        assert!(!inbox.publish("one", tracking));
        let mut stale = mailbox_event(a, 20.0, Contact);
        stale.timestamp = t + Duration::from_millis(30);
        assert!(!inbox.publish("one", stale));
        assert!(inbox.take().apply(&mut map, t + Duration::from_millis(30)));
        assert_eq!(map.cursors["one"].target, Some((90.0, 30.0)));
        assert_mailbox_tip(&map, (90.0, 30.0));
        assert!(map.cursors["one"].core.path.is_none());
    }

    #[test]
    fn slice_a_fix_late_motion_cannot_survive_removal_and_revival() {
        for drain in [false, true] {
            let mut inbox = OverlayInbox::default();
            let mut map = empty_map();
            let mut old = MotionConfig::default();
            old.glide_duration_ms = 850.0;
            inbox.command(command("default", OverlayCommand::SetMotion(old.clone())));
            inbox.command(command("one", OverlayCommand::SetMotion(old.clone())));
            inbox.take().apply(&mut map, Instant::now());
            inbox.command(OverlayMsg::Remove("one".into()));
            if drain {
                inbox.take().apply(&mut map, Instant::now());
            }
            inbox.command(command("one", OverlayCommand::SetMotion(old.clone())));
            inbox.command(command("", OverlayCommand::SetMotion(old)));
            let late_motion = inbox.motion.get("one").cloned();
            let empty_motion = inbox.motion.get("").cloned();
            inbox.command(OverlayMsg::Revive("one".into()));
            // Use the production snapshot reader and partial-override pattern.
            let prior = std::mem::replace(&mut *super::inbox().lock().unwrap(), inbox);
            let revived = current_motion("one");
            let partial = revived.with_overrides(
                None,
                None,
                None,
                None,
                Some(0.5),
                None,
                None,
                None,
                None,
                None,
            );
            let mut inbox = std::mem::replace(&mut *super::inbox().lock().unwrap(), prior);
            inbox.command(command("one", OverlayCommand::SetMotion(partial)));
            inbox.take().apply(&mut map, Instant::now());
            assert!(
                late_motion.is_none(),
                "late motion admitted after Remove, drain={drain}"
            );
            assert!(empty_motion.is_none());
            assert_eq!(revived, map.template.motion);
            assert_eq!(
                map.cursors["one"].core.motion.glide_duration_ms,
                map.template.motion.glide_duration_ms
            );
            assert_eq!(map.cursors["one"].core.motion.spring, 0.5);
        }
    }

    #[test]
    fn slice_a_fix_semantics_survive_expiry_before_resolved_delivery() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        for phase in [
            cursor_overlay::VisualPhase::Intent,
            cursor_overlay::VisualPhase::Tracking,
            cursor_overlay::VisualPhase::Contact,
        ] {
            let mut inbox = OverlayInbox::default();
            let mut map = empty_map();
            let semantics = cua_driver_contract::classify_cursor_semantics(
                "type_text",
                &serde_json::json!({"delivery_mode":"background","element_index":1}),
            )
            .unwrap();
            inbox.semantic(CursorEvent::Action {
                session: "one".into(),
                phase: CursorEventPhase::Begin,
                semantics,
            });
            let t = Instant::now();
            inbox.take().apply(&mut map, t);
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, t + Duration::from_secs(1));
            let id = inbox.begin_action("one").unwrap();
            let mut event = mailbox_event(id, 40.0, phase);
            event.action = cursor_overlay::CursorAction::Text;
            event.timestamp = t + Duration::from_secs(2);
            inbox.publish("one", event.clone());
            inbox.take().apply(&mut map, event.timestamp);
            assert_eq!(map.cursors["one"].core.visual.delivery, semantics.delivery);
            assert_eq!(map.cursors["one"].core.visual.target, semantics.target);
            assert_eq!(map.cursors["one"].core.session_badge_chip_alpha(), 1.0);
            if phase == cursor_overlay::VisualPhase::Intent {
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, t + Duration::from_secs(3));
                event.phase = cursor_overlay::VisualPhase::Contact;
                event.timestamp = t + Duration::from_secs(4);
                inbox.publish("one", event.clone());
                inbox.take().apply(&mut map, event.timestamp);
                assert_eq!(map.cursors["one"].core.visual.delivery, semantics.delivery);
            }
        }
    }

    #[test]
    fn slice_a_fix_new_owner_does_not_recover_an_unrelated_fading_badge() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let semantics = cua_driver_contract::classify_cursor_semantics(
            "click",
            &serde_json::json!({"delivery_mode":"background","x":1,"y":2}),
        )
        .unwrap();
        inbox.semantic(CursorEvent::Action {
            session: "one".into(),
            phase: CursorEventPhase::Begin,
            semantics,
        });
        let id = inbox.begin_action("one").unwrap();
        let first = mailbox_event(id, 40.0, cursor_overlay::VisualPhase::Contact);
        let t = first.timestamp;
        inbox.publish("one", first);
        inbox.take().apply(&mut map, t);
        map.cursors
            .get_mut("one")
            .unwrap()
            .tick_at(0.0, t + Duration::from_millis(200));
        let id = inbox.begin_action("one").unwrap();
        let mut second = mailbox_event(id, 80.0, cursor_overlay::VisualPhase::Contact);
        second.timestamp = t + Duration::from_millis(210);
        inbox.publish("one", second.clone());
        inbox.take().apply(&mut map, second.timestamp);
        assert_eq!(map.cursors["one"].core.visual.delivery, None);
        assert_eq!(map.cursors["one"].core.visual.target, None);
        assert_eq!(map.cursors["one"].core.badge_modifiers, None);
    }

    #[test]
    fn slice_a_fix_partial_chip_fade_uses_event_clock_and_invalidation_clears_it() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        for invalidate in [false, true] {
            let mut inbox = OverlayInbox::default();
            let mut map = empty_map();
            let semantics = cua_driver_contract::classify_cursor_semantics(
                "click",
                &serde_json::json!({"delivery_mode":"background","x":1,"y":2}),
            )
            .unwrap();
            inbox.semantic(CursorEvent::Action {
                session: "one".into(),
                phase: CursorEventPhase::Begin,
                semantics,
            });
            let id = inbox.begin_action("one").unwrap();
            let event = mailbox_event(id, 40.0, cursor_overlay::VisualPhase::Contact);
            let t = event.timestamp;
            inbox.publish("one", event);
            inbox.take().apply(&mut map, t);
            if invalidate {
                apply_msg(&mut map, command("one", OverlayCommand::SetEnabled(false)));
                apply_msg(&mut map, command("one", OverlayCommand::SetEnabled(true)));
            } else {
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, t + Duration::from_millis(200));
                assert!(map.cursors["one"].core.session_badge_chip_alpha() > 0.0);
            }
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, t + Duration::from_secs(2));
            assert_eq!(
                map.cursors["one"].core.badge_modifiers, None,
                "invalidate={invalidate}"
            );
        }
    }

    #[test]
    fn slice_a_fix_unplaced_semantic_cue_requests_ticks_until_expiry() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        inbox.semantic(CursorEvent::Action {
            session: "one".into(),
            phase: CursorEventPhase::Begin,
            semantics: cua_driver_contract::CursorSemantics::new(
                cursor_overlay::CursorAction::Click,
            ),
        });
        let t = Instant::now();
        inbox.take().apply(&mut map, t);
        assert!(!map.cursors["one"].core.placed);
        assert!(map.cursors["one"].needs_frame_tick());
        map.cursors
            .get_mut("one")
            .unwrap()
            .tick_at(0.0, t + Duration::from_secs(2));
        assert!(!map.cursors["one"].needs_frame_tick());
        assert!(!map.cursors["one"].core.placed);
    }

    #[test]
    fn slice_a_fix_sparse_drag_stays_active_until_owned_end() {
        let mut map = empty_map();
        let mut mailbox = cursor_overlay::VisualMailbox::default();
        let id = mailbox.begin_action("one").unwrap();
        let mut event = mailbox_event(id, 40.0, cursor_overlay::VisualPhase::Tracking);
        event.action = cursor_overlay::CursorAction::Drag;
        let t = event.timestamp;
        apply_visual_in_map(&mut map, "one".into(), event.clone(), t);
        map.cursors.get_mut("one").unwrap().core.motion.idle_hide_ms = 10.0;
        map.cursors
            .get_mut("one")
            .unwrap()
            .tick_at(1.0, t + Duration::from_secs(1));
        let core = &map.cursors["one"].core;
        assert_eq!(
            core.visual.resolved_action,
            cursor_overlay::CursorAction::Drag
        );
        assert!(core.pressed);
        assert_eq!(core.idle_alpha, 1.0);
        assert!(core.path.is_none());
        event.phase = cursor_overlay::VisualPhase::End;
        event.timestamp = t + Duration::from_secs(1);
        apply_visual_in_map(&mut map, "one".into(), event.clone(), event.timestamp);
        assert!(!map.cursors["one"].core.pressed);
        assert_eq!(
            map.cursors["one"].core.visual.resolved_action,
            cursor_overlay::CursorAction::Idle
        );
    }

    #[test]
    fn slice_a_fix_generic_metadata_survives_resolved_events_and_chips_expire() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        for resolved in [true, false] {
            for drain in [false, true] {
                let mut inbox = OverlayInbox::default();
                let mut map = empty_map();
                let semantics = cua_driver_contract::classify_cursor_semantics(
                    "click",
                    &serde_json::json!({"delivery_mode":"background","x":1,"y":2}),
                )
                .unwrap();
                inbox.semantic(CursorEvent::Action {
                    session: "one".into(),
                    phase: CursorEventPhase::Begin,
                    semantics,
                });
                let t = Instant::now();
                if drain {
                    inbox.take().apply(&mut map, t);
                    assert_eq!(map.cursors["one"].core.visual.delivery, semantics.delivery);
                    assert_eq!(map.cursors["one"].core.visual.target, semantics.target);
                }
                if resolved {
                    let id = inbox.begin_action("one").unwrap();
                    for phase in [
                        cursor_overlay::VisualPhase::Intent,
                        cursor_overlay::VisualPhase::Contact,
                    ] {
                        let mut event = mailbox_event(id, 40.0, phase);
                        event.timestamp = t;
                        assert!(inbox.publish("one", event));
                        if drain {
                            inbox.take().apply(&mut map, t);
                        }
                    }
                }
                inbox.semantic(CursorEvent::Action {
                    session: "one".into(),
                    phase: CursorEventPhase::End,
                    semantics,
                });
                inbox.take().apply(&mut map, t);
                let core = &map.cursors["one"].core;
                assert_eq!(core.visual.delivery, semantics.delivery);
                assert_eq!(core.visual.target, semantics.target);
                assert_eq!(core.session_badge_chip_alpha(), 1.0);
                if !resolved {
                    assert!(!core.placed);
                    assert!(core.contact.is_none());
                }
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, t + Duration::from_secs(2));
                let core = &map.cursors["one"].core;
                assert_eq!(core.visual.delivery, None);
                assert_eq!(core.visual.target, None);
                assert_eq!(core.badge_modifiers, None);
                assert_eq!(core.session_badge_chip_alpha(), 0.0);
            }
        }
    }

    #[test]
    fn slice_a_motion_snapshot_materializes_timed_keys_from_launch_template() {
        let mut custom = MotionConfig::default();
        custom.glide_duration_ms = 850.0;
        let (prior, template) = {
            let mut inbox = inbox().lock().unwrap();
            let template = inbox.template_motion.clone();
            (inbox.motion.insert("default".into(), custom), template)
        };
        let id = begin_visual_action("motion-event-key").unwrap();
        publish_visual_event(
            "motion-event-key",
            cursor_overlay::VisualEvent {
                id,
                timestamp: Instant::now(),
                target: None,
                bounds: None,
                window: None,
                action: cursor_overlay::CursorAction::Text,
                phase: cursor_overlay::VisualPhase::Intent,
                scroll_direction: None,
                modifiers: None,
            },
        );
        let actual = current_motion("motion-event-key");
        {
            let mut inbox = inbox().lock().unwrap();
            inbox.motion.remove("motion-event-key");
            if let Some(prior) = prior {
                inbox.motion.insert("default".into(), prior);
            } else {
                inbox.motion.remove("default");
            }
        }
        assert_eq!(
            actual, template,
            "a materialized cursor uses the launch template, not a sibling default override"
        );
    }

    #[test]
    fn slice_a_generic_context_preserves_label_theme_and_delivery_modifiers() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        inbox.semantic(CursorEvent::SetSessionLabel {
            session: "context".into(),
            label: "Research".into(),
        });
        inbox.semantic(CursorEvent::SelectTheme {
            session: "context".into(),
            selection: cua_driver_contract::CursorThemeSelection {
                theme_id: cursor_overlay::DEFAULT_THEME_ID.into(),
                reduced_motion: cursor_overlay::ReducedMotion::On,
            },
        });
        let semantics = cua_driver_contract::classify_cursor_semantics(
            "click",
            &serde_json::json!({"delivery_mode":"background","x":1,"y":2}),
        )
        .unwrap();
        inbox.semantic(CursorEvent::Action {
            session: "context".into(),
            phase: CursorEventPhase::Begin,
            semantics,
        });
        inbox.take().apply(&mut map, Instant::now());
        let core = &map.cursors["context"].core;
        assert_eq!(core.session_label.as_deref(), Some("Research"));
        assert_eq!(
            core.visual.reduced_motion,
            cursor_overlay::ReducedMotion::On
        );
        assert_eq!(core.visual.delivery, semantics.delivery);
        assert_eq!(core.visual.target, semantics.target);
    }

    #[test]
    fn slice_a_type_highlight_survives_full_delivery_then_fades_from_end() {
        let mut map = empty_map();
        let t = Instant::now();
        let mut mailbox = cursor_overlay::VisualMailbox::default();
        let id = mailbox.begin_action("editor").unwrap();
        let mut event = cursor_overlay::VisualEvent {
            id,
            timestamp: t,
            target: Some((50.0, 60.0)),
            window: Some(42),
            bounds: Some([10.0, 40.0, 80.0, 40.0]),
            action: cursor_overlay::CursorAction::Text,
            phase: cursor_overlay::VisualPhase::Tracking,
            scroll_direction: None,
            modifiers: None,
        };
        apply_visual_in_map(
            &mut map,
            "editor".into(),
            event.clone(),
            t + Duration::from_secs(5),
        );
        map.cursors
            .get_mut("editor")
            .unwrap()
            .tick_at(5.0, t + Duration::from_secs(5));
        assert_eq!(map.cursors["editor"].core.focus_rect, event.bounds);
        assert_eq!(map.cursors["editor"].core.focus_rect_t, 0.0);
        assert!(cursor_is_visible(&map.cursors["editor"]));
        event.phase = cursor_overlay::VisualPhase::End;
        event.timestamp = t + Duration::from_secs(6);
        apply_visual_in_map(
            &mut map,
            "editor".into(),
            event.clone(),
            event.timestamp + Duration::from_millis(300),
        );
        assert!((map.cursors["editor"].core.focus_rect_t - 0.5).abs() < 1e-9);
        map.cursors
            .get_mut("editor")
            .unwrap()
            .tick_at(0.0, event.timestamp + Duration::from_secs(1));
        assert!(map.cursors["editor"].core.focus_rect.is_none());
    }

    #[test]
    fn watchable_fast_click_before_first_inbox_drain_glides_from_target_display_seed() {
        use cursor_overlay::VisualPhase::*;
        for drain_intent in [false, true] {
            let mut inbox = OverlayInbox::default();
            let (mut map, _) = bounds_map();
            let t = Instant::now();
            let id = inbox.begin_action("one").unwrap();
            let mut intent = mailbox_event(id, -100.0, Intent);
            intent.timestamp = t;
            inbox.publish("one", intent.clone());
            if drain_intent {
                inbox.take().apply(&mut map, t);
            }
            let mut contact = intent;
            contact.phase = Contact;
            contact.timestamp = t + Duration::from_millis(1);
            inbox.publish("one", contact.clone());
            inbox.take().apply(&mut map, t + Duration::from_millis(2));
            let core = &map.cursors["one"].core;
            assert!(core.placed && core.pos.0 >= -200.0 && core.pos.0 < 0.0);
            assert!(
                core.path.is_some(),
                "coalesced fast input must leave visible travel"
            );
            assert!(core.contact.is_none());
            let first = core.pos;
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, t + Duration::from_millis(60));
            assert_ne!(map.cursors["one"].core.pos, first);
            assert!(map.cursors["one"].core.contact.is_none());
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, t + Duration::from_millis(221));
            assert_mailbox_tip(&map, (-100.0, 30.0));
            assert_eq!(
                map.cursors["one"].core.contact.unwrap().timestamp,
                contact.timestamp
            );
            assert_eq!(
                map.cursors["one"].core.contact.unwrap().target,
                (-100.0, 30.0)
            );
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, t + Duration::from_millis(371));
            assert!(map.cursors["one"].core.contact.is_none());
        }
    }

    /// Controlled native surface using the real inbox, render geometry and gate.
    /// Only AppKit submission and actual input are replaced by explicit test steps.
    struct QuickApproachSink {
        inbox: Mutex<OverlayInbox>,
        map: Mutex<RenderMap>,
        events: Mutex<Vec<cursor_overlay::VisualEvent>>,
    }
    impl QuickApproachSink {
        fn new(disabled: bool, reduced: bool) -> std::sync::Arc<Self> {
            let mut map = empty_map();
            map.template.reduced_motion = if reduced {
                cursor_overlay::ReducedMotion::On
            } else {
                cursor_overlay::ReducedMotion::Off
            };
            std::sync::Arc::new(Self {
                inbox: Mutex::new(OverlayInbox {
                    explicitly_disabled: disabled,
                    ..Default::default()
                }),
                map: Mutex::new(map),
                events: Mutex::new(Vec::new()),
            })
        }
        fn frame(&self, now: Instant) -> Vec<TargetFrame> {
            let batch = self.inbox.lock().unwrap().take();
            let mut map = self.map.lock().unwrap();
            batch.apply(&mut map, now);
            for state in map.cursors.values_mut() {
                state.tick_at(0.0, now);
            }
            let display = map.layout.displays[0];
            let pixels = render_display(&map, display);
            let frames = target_frames(&map, display, &mut self.inbox.lock().unwrap());
            if !frames.is_empty() {
                assert!(pixels.data().chunks_exact(4).any(|pixel| pixel[3] != 0));
            }
            frames
        }
        fn present(&self, frames: &[TargetFrame], generation: u64, display: DisplayId) {
            if let Some(frame) = frames.first() {
                assert!(apply_surface_route(
                    &self.inbox,
                    ZOrderRoute {
                        generation,
                        display_id: display,
                        target_wid: frame.event.window,
                    },
                    || {}
                ));
            }
            self.inbox.lock().unwrap().acknowledge_targets(
                generation,
                display,
                self.map.lock().unwrap().layout.generation,
                frames,
            );
        }
        fn start(&self) -> Instant {
            self.events.lock().unwrap()[0].timestamp
        }
        fn pending(&self) -> usize {
            self.inbox.lock().unwrap().approaches.len()
        }
    }
    impl super::super::visual::PointerVisualSink for QuickApproachSink {
        fn approach_enabled(&self, key: &str) -> bool {
            self.inbox.lock().unwrap().approach_enabled(key)
        }
        fn send(&self, key: &str, cmd: OverlayCommand) {
            self.inbox.lock().unwrap().command(command(key, cmd));
        }
        fn begin(&self, key: &str) -> Option<cursor_overlay::VisualActionId> {
            self.inbox.lock().unwrap().begin_action(key)
        }
        fn publish(&self, key: &str, event: cursor_overlay::VisualEvent) {
            self.events.lock().unwrap().push(event.clone());
            self.inbox.lock().unwrap().publish(key, event);
        }
        fn register_target(
            &self,
            key: &str,
            event: &cursor_overlay::VisualEvent,
        ) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
            self.inbox.lock().unwrap().register_target(
                key,
                event,
                self.map.lock().unwrap().layout.generation,
            )
        }
        fn target_current(
            &self,
            key: &str,
            event: &cursor_overlay::VisualEvent,
        ) -> Result<(), String> {
            self.inbox.lock().unwrap().target_current(
                key,
                event,
                self.map.lock().unwrap().layout.generation,
            )
        }
        fn release_target(&self, key: &str, event: &cursor_overlay::VisualEvent) {
            self.inbox.lock().unwrap().release_target(key, event);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_all_routes_wait_for_applied_frame_then_contact_before_readback() {
        use crate::cursor::visual::{point, DeliveryReceipt, InvocationVisualSink};
        use crate::tools::{ClickTool, DoubleClickTool, RightClickTool, ToolState};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        for route in [
            "left",
            "middle",
            "button_right",
            "ax_token",
            "pixel_ax",
            "right_click",
            "double_click",
        ] {
            for reduced in [false, true] {
                let sink = QuickApproachSink::new(false, reduced);
                // Exercise forwarding through the real invocation decorator, as type children do.
                let bound = InvocationVisualSink::bind(
                    "click",
                    &serde_json::json!({"x": 80, "y": 30}),
                    "one",
                    sink.clone(),
                );
                let state = Arc::new(ToolState::default());
                let click = ClickTool::new(state.clone()).with_visual_sink(bound.clone());
                let right = RightClickTool::new(state.clone()).with_visual_sink(bound.clone());
                let double = DoubleClickTool::new(state).with_visual_sink(bound);
                let receipt = DeliveryReceipt::default();
                let dispatches = AtomicUsize::new(0);
                let native = async {
                    receipt
                        .dispatch_checked(|| {
                            dispatches.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .unwrap();
                    let contact = sink.events.lock().unwrap().last().unwrap().clone();
                    assert_eq!(contact.phase, cursor_overlay::VisualPhase::Contact);
                    receipt.accepted();
                    assert_eq!(
                        sink.events.lock().unwrap().last().unwrap().timestamp,
                        contact.timestamp
                    );
                    cua_driver_core::protocol::ToolResult::error("post-delivery readback failed")
                };
                let target = point(80.0, 30.0, Some(42));
                let call = async {
                    match route {
                        "right_click" => {
                            right
                                .dispatch_resolved("one", target, &receipt, native)
                                .await
                        }
                        "double_click" => {
                            double
                                .dispatch_resolved("one", target, &receipt, native)
                                .await
                        }
                        "ax_token" | "pixel_ax" => {
                            click
                                .dispatch_resolved(
                                    "one",
                                    target,
                                    &receipt,
                                    async { Some(native.await) },
                                    async { panic!("semantic acceptance must not poll fallback") },
                                )
                                .await
                        }
                        _ => {
                            click
                                .dispatch_resolved("one", target, &receipt, async { None }, native)
                                .await
                        }
                    }
                };
                tokio::pin!(call);
                assert!(futures_util::poll!(&mut call).is_pending(), "{route}");
                let start = sink.start();
                let initial = sink.frame(start);
                if reduced {
                    assert_eq!(initial.len(), 1);
                } else {
                    assert!(initial.is_empty());
                }
                assert!(
                    futures_util::poll!(&mut call).is_pending(),
                    "rendering without submission is insufficient"
                );
                if !reduced {
                    sink.present(&sink.frame(start + Duration::from_millis(79)), 1, 1);
                    assert!(futures_util::poll!(&mut call).is_pending());
                }
                let frames = sink.frame(start + Duration::from_millis(140));
                assert_eq!(frames.len(), 1, "{route}");
                assert_eq!(dispatches.load(Ordering::SeqCst), 0);
                sink.present(&frames, 0, 1);
                assert!(futures_util::poll!(&mut call).is_pending());
                sink.present(&frames, 1, 1);
                let result = call.await;
                assert_eq!(result.is_error, Some(true));
                assert_eq!(
                    dispatches.load(Ordering::SeqCst),
                    1,
                    "one atomic gesture for {route}"
                );
                assert!(receipt.was_accepted());
                assert_eq!(sink.pending(), 0);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_timeout_cancel_end_supersede_and_surface_loss_never_dispatch() {
        use crate::cursor::visual::{point, DeliveryReceipt, PointerVisualSink};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::Arc;
        for failure in [
            "timeout",
            "cancel",
            "end",
            "supersede",
            "retarget",
            "surface",
        ] {
            let sink = QuickApproachSink::new(false, false);
            let tool =
                ClickTool::new(Arc::new(ToolState::default())).with_visual_sink(sink.clone());
            let receipt = DeliveryReceipt::default();
            let mut call = Box::pin(tool.dispatch_resolved(
                "one",
                point(80.0, 30.0, Some(42)),
                &receipt,
                async { panic!("semantic input on {failure}") },
                async { panic!("native input on {failure}") },
            ));
            assert!(futures_util::poll!(&mut call).is_pending());
            assert_eq!(sink.pending(), 1);
            let start = sink.start();
            sink.frame(start);
            let frames = sink.frame(start + Duration::from_millis(140));
            match failure {
                "cancel" => {
                    drop(call);
                    assert_eq!(sink.pending(), 0);
                    continue;
                }
                "end" => sink
                    .inbox
                    .lock()
                    .unwrap()
                    .command(OverlayMsg::Remove("one".into())),
                "supersede" => {
                    sink.begin("one");
                }
                "retarget" => {
                    let mut event = frames[0].event.clone();
                    event.target = Some((81.0, 30.0));
                    sink.publish("one", event);
                }
                "surface" => {
                    sink.map.lock().unwrap().layout.generation += 1;
                }
                _ => {}
            }
            if failure != "timeout" {
                sink.present(&frames, 1, 1);
            }
            tokio::time::advance(Duration::from_millis(250)).await;
            let result = call.await;
            assert_eq!(result.is_error, Some(true), "{failure}");
            assert!(!receipt.was_accepted());
            assert_eq!(sink.pending(), 0);
            assert!(sink
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|e| e.phase != cursor_overlay::VisualPhase::Contact));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_disabled_immediate_missing_target_refused_and_fallback_cannot_move() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::Arc;
        for (disabled, target, changed) in [
            (true, None, false),
            (false, None, false),
            (false, point(80.0, 30.0, Some(42)), true),
            (false, point(80.0, 30.0, Some(42)), false),
        ] {
            let sink = QuickApproachSink::new(disabled, true);
            let state = Arc::new(ToolState::default());
            let tool = ClickTool::new(state.clone()).with_visual_sink(sink.clone());
            let receipt = DeliveryReceipt::default();
            let call = tool.dispatch_resolved("one", target, &receipt, async { None }, async {
                let result = receipt.dispatch_at(
                    &state.cursor_registry,
                    if changed { 81.0 } else { 80.0 },
                    30.0,
                    42,
                    |_, _| {
                        assert!(
                            !changed,
                            "changed native coordinates must refuse before mutation"
                        );
                        Ok(())
                    },
                );
                match result {
                    Ok(()) => cua_driver_core::protocol::ToolResult::text("accepted"),
                    Err(e) => super::super::visual::approach_refusal(e),
                }
            });
            tokio::pin!(call);
            if !disabled && target.is_some() {
                assert!(futures_util::poll!(&mut call).is_pending());
                let frames = sink.frame(sink.start());
                sink.present(&frames, 1, 1);
            }
            let result = call.await;
            assert_eq!(
                result.is_error == Some(true),
                !disabled && (target.is_none() || changed)
            );
            assert_eq!(
                receipt.was_accepted(),
                disabled || (target.is_some() && !changed)
            );
            assert_eq!(sink.pending(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_arrived_isolation_and_bounded_cleanup() {
        use crate::cursor::visual::{point, DeliveryReceipt, PointerVisualSink};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::Arc;
        let sink = QuickApproachSink::new(false, false);
        let mut seed = mailbox_event(
            sink.begin("one").unwrap(),
            80.0,
            cursor_overlay::VisualPhase::Contact,
        );
        seed.window = Some(42);
        sink.publish("one", seed.clone());
        sink.frame(seed.timestamp);
        let tool = ClickTool::new(Arc::new(ToolState::default())).with_visual_sink(sink.clone());
        let receipt = DeliveryReceipt::default();
        let call = tool.dispatch_resolved(
            "one",
            point(80.0, 30.0, Some(42)),
            &receipt,
            async { None },
            async {
                receipt.dispatch_checked(|| Ok(())).unwrap();
                cua_driver_core::protocol::ToolResult::text("accepted")
            },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        let frame = sink.frame(Instant::now());
        assert_eq!(frame.len(), 1, "already arrived requires no minimum glide");
        assert!(sink.map.lock().unwrap().cursors["one"].core.path.is_none());
        let mut unrelated = frame[0].clone();
        unrelated.key = "two".into();
        sink.present(&[unrelated], 1, 1);
        assert!(futures_util::poll!(&mut call).is_pending());
        sink.present(&frame, 1, 2);
        assert!(
            futures_util::poll!(&mut call).is_pending(),
            "wrong surface cannot acknowledge"
        );
        sink.present(&frame, 1, 1);
        assert_ne!(call.await.is_error, Some(true));
        assert_eq!(sink.pending(), 0);
        for _ in 0..1000 {
            let receipt = DeliveryReceipt::default();
            let mut call = Box::pin(tool.dispatch_resolved(
                "one",
                point(80.0, 30.0, Some(42)),
                &receipt,
                async { panic!("cancelled action must not dispatch") },
                async { unreachable!() },
            ));
            assert!(futures_util::poll!(&mut call).is_pending());
            assert_eq!(sink.pending(), 1);
            drop(call);
            assert_eq!(sink.pending(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_ready_frame_and_live_target_can_be_invalidated_before_dispatch() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        for change in ["live_target", "new_frame", "late_ack", "new_surface"] {
            let sink = QuickApproachSink::new(false, true);
            let live = Arc::new(AtomicBool::new(true));
            let receipt = DeliveryReceipt::default();
            let check = live.clone();
            receipt.set_revalidation(move || {
                if !check.load(Ordering::SeqCst) {
                    anyhow::bail!("live target moved")
                }
                Ok(())
            });
            let tool =
                ClickTool::new(Arc::new(ToolState::default())).with_visual_sink(sink.clone());
            let call = tool.dispatch_resolved(
                "one",
                point(80.0, 30.0, Some(42)),
                &receipt,
                async { panic!("invalidated approach dispatched: {change}") },
                async { unreachable!() },
            );
            tokio::pin!(call);
            assert!(futures_util::poll!(&mut call).is_pending());
            let frame = sink.frame(sink.start());
            if change == "late_ack" {
                tokio::time::advance(Duration::from_millis(250)).await;
            }
            sink.present(&frame, 1, 1);
            match change {
                "live_target" => live.store(false, Ordering::SeqCst),
                "new_frame" => sink.present(&[], 1, 1),
                "new_surface" => {
                    sink.map.lock().unwrap().layout.generation = 2;
                    let fresh = sink.frame(Instant::now());
                    sink.present(&fresh, 2, 1);
                }
                _ => {}
            }
            assert_eq!(call.await.is_error, Some(true));
            assert!(!receipt.was_accepted());
            assert_eq!(sink.pending(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_ownership_is_checked_after_live_readback() {
        use crate::cursor::visual::{point, DeliveryReceipt, PointerVisualSink};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::Arc;
        let sink = QuickApproachSink::new(false, true);
        let receipt = DeliveryReceipt::default();
        let during_readback = sink.clone();
        receipt.set_revalidation(move || {
            during_readback.begin("one");
            Ok(())
        });
        let tool = ClickTool::new(Arc::new(ToolState::default())).with_visual_sink(sink.clone());
        let call = tool.dispatch_resolved(
            "one",
            point(80.0, 30.0, Some(42)),
            &receipt,
            async { panic!("ownership changed during readback") },
            async { unreachable!() },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        sink.present(&sink.frame(sink.start()), 1, 1);
        assert_eq!(call.await.is_error, Some(true));
        assert!(!receipt.was_accepted());
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_two_sessions_and_revived_generation_keep_separate_waiters() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::Arc;
        let sink = QuickApproachSink::new(false, true);
        let tool = ClickTool::new(Arc::new(ToolState::default())).with_visual_sink(sink.clone());
        let first = DeliveryReceipt::default();
        let second = DeliveryReceipt::default();
        let old = DeliveryReceipt::default();
        let mut old_call = Box::pin(tool.dispatch_resolved(
            "one",
            point(80.0, 30.0, Some(42)),
            &old,
            async { panic!("ended generation dispatched") },
            async { unreachable!() },
        ));
        assert!(futures_util::poll!(&mut old_call).is_pending());
        let obsolete = sink.frame(sink.start());
        sink.inbox
            .lock()
            .unwrap()
            .command(OverlayMsg::Remove("one".into()));
        sink.inbox
            .lock()
            .unwrap()
            .command(OverlayMsg::Revive("one".into()));
        let a = tool.dispatch_resolved(
            "one",
            point(80.0, 30.0, Some(42)),
            &first,
            async { None },
            async {
                first.dispatch_checked(|| Ok(())).unwrap();
                cua_driver_core::protocol::ToolResult::text("one")
            },
        );
        let b = tool.dispatch_resolved(
            "two",
            point(40.0, 60.0, Some(42)),
            &second,
            async { None },
            async {
                second.dispatch_checked(|| Ok(())).unwrap();
                cua_driver_core::protocol::ToolResult::text("two")
            },
        );
        tokio::pin!(a, b);
        assert!(futures_util::poll!(&mut a).is_pending());
        assert!(futures_util::poll!(&mut b).is_pending());
        assert_eq!(sink.pending(), 2);
        drop(old_call);
        assert_eq!(
            sink.pending(),
            2,
            "old guard cleanup cannot remove the revived waiter"
        );
        sink.present(&obsolete, 1, 1);
        assert!(futures_util::poll!(&mut a).is_pending());
        assert!(futures_util::poll!(&mut b).is_pending());
        let frames = sink.frame(Instant::now());
        assert_eq!(frames.len(), 2);
        let one: Vec<_> = frames.iter().filter(|f| f.key == "one").cloned().collect();
        sink.present(&one, 1, 1);
        assert_ne!(a.await.is_error, Some(true));
        assert!(futures_util::poll!(&mut b).is_pending());
        assert_eq!(sink.pending(), 1);
        let two: Vec<_> = frames.iter().filter(|f| f.key == "two").cloned().collect();
        sink.present(&two, 1, 1);
        assert_ne!(b.await.is_error, Some(true));
        assert_eq!(sink.pending(), 0);
        assert!(!old.was_accepted());
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_waiter_keeps_production_frame_scheduler_awake_until_deadline() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        let sink = QuickApproachSink::new(false, true);
        sink.map.lock().unwrap().template.motion.idle_hide_ms = 0.0;
        let tool = ClickTool::new(std::sync::Arc::new(ToolState::default()))
            .with_visual_sink(sink.clone());
        let receipt = DeliveryReceipt::default();
        let call = tool.dispatch_resolved(
            "one",
            point(80.0, 30.0, Some(42)),
            &receipt,
            async { panic!("no applied target yet") },
            async { unreachable!() },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        // Renderer progressed, but submission did not acknowledge. No timer-only admission.
        sink.frame(sink.start() + Duration::from_secs(2));
        let map = sink.map.lock().unwrap();
        assert!(!render_map_needs_frame_tick(&map));
        assert!(render_frame_tick_needed(&map, &sink.inbox.lock().unwrap()));
        drop(map);
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(call.await.is_error, Some(true));
        assert!(!receipt.was_accepted());
        assert_eq!(sink.pending(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_selection_read_cannot_outlive_admission() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::input::ax_actions::{select_nearest_container_with, SelectionAccess};
        use crate::tools::{ClickTool, ToolState};
        use std::cell::Cell;
        struct Ax<'a> {
            sink: &'a QuickApproachSink,
            writes: Cell<usize>,
            releases: Cell<usize>,
            invalidate: bool,
        }
        impl SelectionAccess for Ax<'_> {
            fn role(&self, element: usize) -> String {
                if element == 1 { "AXTextField" } else { "AXRow" }.into()
            }
            fn selected(&self, _: usize) -> Option<bool> {
                if self.invalidate {
                    self.sink.inbox.lock().unwrap().begin_action("one");
                }
                Some(false) // accepted writes deliberately fail readback as well
            }
            fn set_selected(&self, _: usize) -> i32 {
                self.writes.set(self.writes.get() + 1);
                0
            }
            fn parent(&self, element: usize) -> Option<usize> {
                (element == 1).then_some(2)
            }
            fn release(&self, _: usize) {
                self.releases.set(self.releases.get() + 1);
            }
        }
        for invalidate in [true, false] {
            let sink = QuickApproachSink::new(false, true);
            let tool = ClickTool::new(std::sync::Arc::new(ToolState::default()))
                .with_visual_sink(sink.clone());
            let receipt = DeliveryReceipt::default();
            let ax = Ax {
                sink: &sink,
                writes: Cell::new(0),
                releases: Cell::new(0),
                invalidate,
            };
            let call = tool.dispatch_resolved(
                "one",
                point(80.0, 30.0, Some(42)),
                &receipt,
                async {
                    let result = select_nearest_container_with(1, &receipt, &ax);
                    assert_eq!(result.is_err(), invalidate);
                    Some(cua_driver_core::protocol::ToolResult::error(
                        "selection not verified",
                    ))
                },
                async { panic!("selection admission failure reached native fallback") },
            );
            tokio::pin!(call);
            assert!(futures_util::poll!(&mut call).is_pending());
            let frames = sink.frame(sink.start());
            sink.present(&frames, 1, 1);
            assert_eq!(call.await.is_error, Some(true));
            assert_eq!(ax.writes.get(), usize::from(!invalidate));
            assert_eq!(ax.releases.get(), 1);
            assert_eq!(receipt.was_accepted(), !invalidate);
            let contacts: Vec<_> = sink
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.phase == cursor_overlay::VisualPhase::Contact)
                .cloned()
                .collect();
            assert_eq!(contacts.len(), usize::from(!invalidate));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_matching_frame_keeps_admission_through_submission() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        for replacement in ["same", "omitted", "changed"] {
            let sink = QuickApproachSink::new(false, true);
            let tool = ClickTool::new(std::sync::Arc::new(ToolState::default()))
                .with_visual_sink(sink.clone());
            let receipt = DeliveryReceipt::default();
            let (resume, wait) = tokio::sync::oneshot::channel::<()>();
            let call = tool.dispatch_resolved(
                "one",
                point(80.0, 30.0, Some(42)),
                &receipt,
                async { None },
                async {
                    wait.await.unwrap();
                    match receipt.dispatch_checked(|| Ok(())) {
                        Ok(()) => cua_driver_core::protocol::ToolResult::text("accepted"),
                        Err(e) => cua_driver_core::protocol::ToolResult::error(e.to_string()),
                    }
                },
            );
            tokio::pin!(call);
            assert!(futures_util::poll!(&mut call).is_pending());
            let frames = sink.frame(sink.start());
            sink.present(&frames, 1, 1);
            assert!(futures_util::poll!(&mut call).is_pending());
            assert!(receipt.ensure_current().is_ok());
            assert!(apply_surface_route(
                &sink.inbox,
                ZOrderRoute {
                    generation: 1,
                    display_id: 1,
                    target_wid: Some(42),
                },
                || {
                    assert!(receipt.ensure_current().is_ok());
                }
            ));
            let mut next = frames.clone();
            if replacement == "omitted" {
                next.clear();
            }
            if replacement == "changed" {
                next[0].event.target = Some((81.0, 30.0));
            }
            assert!(apply_surface_frame(
                &sink.inbox,
                1,
                1,
                || 1,
                &next,
                || {
                    // A live readback may return during setContents/commit/flush.
                    assert_eq!(receipt.ensure_current().is_ok(), replacement == "same");
                }
            ));
            resume.send(()).unwrap();
            assert_eq!(call.await.is_error == Some(true), replacement != "same");
            assert_eq!(receipt.was_accepted(), replacement == "same");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_route_change_invalidates_before_native_ordering() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        let sink = QuickApproachSink::new(false, true);
        let tool = ClickTool::new(std::sync::Arc::new(ToolState::default()))
            .with_visual_sink(sink.clone());
        let receipt = DeliveryReceipt::default();
        let call = tool.dispatch_resolved(
            "one",
            point(80.0, 30.0, Some(42)),
            &receipt,
            async { None },
            async {
                receipt
                    .dispatch_checked(|| -> anyhow::Result<()> {
                        panic!("obsolete surface dispatched")
                    })
                    .unwrap();
                unreachable!()
            },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        let frames = sink.frame(sink.start());
        sink.present(&frames, 1, 1);
        let event = frames[0].event.clone();
        assert!(sink
            .inbox
            .lock()
            .unwrap()
            .target_current("one", &event, 1)
            .is_ok());
        // Session two has a different window on the same physical surface.
        let id = sink.inbox.lock().unwrap().begin_action("two").unwrap();
        let mut other = event.clone();
        other.id = id;
        other.window = Some(77);
        sink.inbox.lock().unwrap().publish("two", other);
        assert!(apply_surface_route(
            &sink.inbox,
            ZOrderRoute {
                generation: 1,
                display_id: 1,
                target_wid: Some(77),
            },
            || {
                assert!(sink
                    .inbox
                    .lock()
                    .unwrap()
                    .target_current("one", &event, 1)
                    .is_err());
            }
        ));
        // A queued old frame cannot establish readiness under the new route.
        sink.inbox
            .lock()
            .unwrap()
            .acknowledge_targets(1, 1, 1, &frames);
        assert!(sink
            .inbox
            .lock()
            .unwrap()
            .target_current("one", &event, 1)
            .is_err());
        assert_eq!(call.await.is_error, Some(true));
        assert!(!receipt.was_accepted());
    }

    #[tokio::test(start_paused = true)]
    async fn quick_approach_deadline_does_not_take_the_stalled_render_lock() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        let _stalled = RENDER.lock().unwrap();
        let tool = ClickTool::new(std::sync::Arc::new(ToolState::default()));
        let receipt = DeliveryReceipt::default();
        let call = tool.dispatch_resolved(
            "quick-real-stalled",
            point(80.0, 30.0, Some(42)),
            &receipt,
            async { panic!("stalled renderer dispatched") },
            async { unreachable!() },
        );
        tokio::pin!(call);
        assert!(futures_util::poll!(&mut call).is_pending());
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(call.await.is_error, Some(true));
        assert!(!receipt.was_accepted());
        assert!(!inbox()
            .lock()
            .unwrap()
            .approaches
            .contains_key("quick-real-stalled"));
    }

    #[tokio::test]
    async fn quick_approach_cancelled_call_cannot_keep_registration_alive_during_native_readback() {
        use crate::cursor::visual::{point, DeliveryReceipt};
        use crate::tools::{ClickTool, ToolState};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        for fallback in [false, true] {
            let sink = QuickApproachSink::new(false, true);
            let receipt = Arc::new(DeliveryReceipt::default());
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let started_tx = Mutex::new(Some(started_tx));
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            let reads = AtomicUsize::new(0);
            receipt.set_revalidation(move || {
                if reads.fetch_add(1, Ordering::SeqCst) == if fallback { 3 } else { 2 } {
                    started_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                }
                Ok(())
            });
            let tool =
                ClickTool::new(Arc::new(ToolState::default())).with_visual_sink(sink.clone());
            let delivered = Arc::new(AtomicUsize::new(0));
            let observed = delivered.clone();
            let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
            let invocation = tokio::spawn(async move {
                let worker_receipt = receipt.clone();
                tool.dispatch_resolved(
                    "one",
                    point(80.0, 30.0, Some(42)),
                    &receipt,
                    async { None },
                    async {
                        tokio::task::spawn_blocking(move || {
                            let mutate = || {
                                observed.fetch_add(1, Ordering::SeqCst);
                                Ok(())
                            };
                            let result = if fallback {
                                worker_receipt.dispatch_at(
                                    &super::super::CursorRegistry::new(),
                                    80.0,
                                    30.0,
                                    42,
                                    |_, _| mutate(),
                                )
                            } else {
                                worker_receipt.dispatch_checked(mutate)
                            };
                            finished_tx.send(result.is_err()).unwrap();
                        })
                        .await
                        .unwrap();
                        cua_driver_core::protocol::ToolResult::text("finished")
                    },
                )
                .await
            });
            // The publication barrier is a yield, with no real timer or native input.
            while sink.pending() == 0 {
                tokio::task::yield_now().await;
            }
            sink.present(&sink.frame(sink.start()), 1, 1);
            started_rx.await.unwrap();
            invocation.abort();
            assert!(invocation.await.unwrap_err().is_cancelled());
            let registrations_after_cancel = sink.pending();
            release_tx.send(()).unwrap();
            let refused = finished_rx.await.unwrap();
            assert_eq!(registrations_after_cancel, 0);
            assert!(refused);
            assert_eq!(delivered.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn quick_approach_registration_rejects_obsolete_action_target_and_surface() {
        let mut inbox = OverlayInbox::default();
        let id = inbox.begin_action("quick").unwrap();
        let event = cursor_overlay::VisualEvent {
            id,
            timestamp: Instant::now(),
            target: Some((20.0, 30.0)),
            window: Some(42),
            bounds: None,
            action: cursor_overlay::CursorAction::Click,
            phase: cursor_overlay::VisualPhase::Intent,
            scroll_direction: None,
            modifiers: None,
        };
        inbox.publish("quick", event.clone());
        let mut receiver = inbox.register_target("quick", &event, 2).unwrap();
        assert!(receiver.try_recv().is_err());
        let candidate = TargetFrame {
            key: "quick".into(),
            event: event.clone(),
            generation: 2,
            display_id: 1,
        };
        inbox.acknowledge_targets(1, 1, 2, &[candidate.clone()]);
        assert!(
            receiver.try_recv().is_err(),
            "obsolete surface must not acknowledge"
        );
        let mut changed = candidate.clone();
        changed.event.target = Some((21.0, 30.0));
        inbox.acknowledge_targets(2, 1, 2, &[changed]);
        assert!(receiver.try_recv().is_err());
        inbox.applied_routes.insert(
            1,
            ZOrderRoute {
                generation: 2,
                display_id: 1,
                target_wid: event.window,
            },
        );
        inbox.acknowledge_targets(2, 1, 2, &[candidate]);
        assert_eq!(receiver.try_recv(), Ok(()));
        assert!(inbox.target_current("quick", &event, 2).is_ok());
        inbox.begin_action("quick");
        assert!(inbox.target_current("quick", &event, 2).is_err());
        assert!(inbox.approaches.is_empty());
    }

    #[derive(Default)]
    struct WatchableInboxSink(
        Mutex<OverlayInbox>,
        Mutex<Vec<cursor_overlay::VisualEvent>>,
        Mutex<Duration>,
    );
    impl super::super::visual::PointerVisualSink for WatchableInboxSink {
        fn approach_enabled(&self, _key: &str) -> bool {
            false
        }
        fn send(&self, key: &str, cmd: OverlayCommand) {
            self.0.lock().unwrap().command(command(key, cmd));
        }
        fn begin(&self, key: &str) -> Option<cursor_overlay::VisualActionId> {
            self.0.lock().unwrap().begin_action(key)
        }
        fn publish(&self, key: &str, mut event: cursor_overlay::VisualEvent) {
            // Advance only the test clock; production publishers still stamp real time.
            event.timestamp += *self.2.lock().unwrap();
            self.1.lock().unwrap().push(event.clone());
            self.0.lock().unwrap().publish(key, event);
        }
    }

    fn multi_stage_begin(sink: &WatchableInboxSink, tool: &str, args: &serde_json::Value) {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        sink.0.lock().unwrap().semantic(CursorEvent::Action {
            session: "one".into(),
            phase: CursorEventPhase::Begin,
            semantics: cua_driver_contract::classify_cursor_semantics(tool, args).unwrap(),
        });
    }

    fn multi_stage_assert(
        sink: &WatchableInboxSink,
        map: &mut RenderMap,
        delivery: cua_driver_contract::CursorDelivery,
        target: cua_driver_contract::CursorTarget,
    ) {
        let now = sink.1.lock().unwrap().last().unwrap().timestamp;
        sink.0.lock().unwrap().take().apply(map, now);
        let core = &map.cursors["one"].core;
        assert_eq!(core.visual.delivery, Some(delivery));
        assert_eq!(core.visual.target, Some(target));
        assert_eq!(core.badge_modifiers, Some((Some(delivery), Some(target))));
        assert_eq!(core.session_badge_chip_alpha(), 1.0);
    }

    #[test]
    fn multi_stage_ax_scroll_keeps_invocation_modifiers() {
        use crate::cursor::visual::ResolvedPointerTarget;
        use cua_driver_contract::{CursorDelivery::Background, CursorTarget::Ax};
        for schedule in ["coalesced", "split", "expired"] {
            let registry = super::super::CursorRegistry::new();
            let sink = std::sync::Arc::new(WatchableInboxSink::default());
            let args = serde_json::json!({"pid":7,"window_id":42,"element_index":1,
                "direction":"down","amount":2,"delivery_mode":"background"});
            multi_stage_begin(&sink, "scroll", &args);
            let visual_sink = crate::cursor::visual::InvocationVisualSink::bind(
                "scroll",
                &args,
                "one",
                sink.clone(),
            );
            let mut map = empty_map();
            if schedule == "expired" {
                let now = Instant::now();
                sink.0.lock().unwrap().take().apply(&mut map, now);
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, now + Duration::from_secs(2));
                assert_eq!(map.cursors["one"].core.badge_modifiers, None);
                *sink.2.lock().unwrap() = Duration::from_secs(3);
            }
            for stage in 0..2 {
                let result = crate::tools::dispatch_scroll_visual(
                    &registry,
                    visual_sink.as_ref(),
                    "one",
                    ResolvedPointerTarget::from_bounds(42, [10.0, 20.0, 40.0, 40.0]),
                    -1,
                    0,
                    || Ok::<_, ()>(17),
                );
                assert_eq!(result, Ok(17));
                if schedule != "coalesced" || stage == 1 {
                    multi_stage_assert(&sink, &mut map, Background, Ax);
                    assert_eq!(
                        map.cursors["one"].core.contact.unwrap().direction,
                        Some(cursor_overlay::ScrollDirection::Down)
                    );
                }
                multi_stage_expire_between(&sink, &mut map, schedule);
            }
            let events = sink.1.lock().unwrap();
            assert_ne!(events[0].id, events[2].id);
            drop(events);
            multi_stage_cleanup_and_unrelated(&sink, &registry, &mut map);
        }
    }

    #[tokio::test]
    async fn multi_stage_pixel_focus_then_type_keeps_invocation_modifiers() {
        use crate::cursor::visual::{DeliveryReceipt, ResolvedPointerTarget};
        use cua_driver_contract::{CursorDelivery::Foreground, CursorTarget::Pixel};
        use std::sync::Arc;
        for schedule in ["coalesced", "split", "expired"] {
            let state = Arc::new(crate::tools::ToolState::default());
            let sink = Arc::new(WatchableInboxSink::default());
            let args = serde_json::json!({"pid":7,"window_id":42,"x":20,"y":30,
                "text":"hello","delivery_mode":"foreground"});
            multi_stage_begin(&sink, "type_text", &args);
            let visual_sink = crate::cursor::visual::InvocationVisualSink::bind(
                "type_text",
                &args,
                "one",
                sink.clone(),
            );
            let mut map = empty_map();
            if schedule == "expired" {
                let now = Instant::now();
                sink.0.lock().unwrap().take().apply(&mut map, now);
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, now + Duration::from_secs(2));
                assert_eq!(map.cursors["one"].core.badge_modifiers, None);
                *sink.2.lock().unwrap() = Duration::from_secs(3);
            }
            let click =
                crate::tools::ClickTool::new(state.clone()).with_visual_sink(visual_sink.clone());
            let receipt = DeliveryReceipt::default();
            let result = click
                .dispatch_resolved(
                    "one",
                    crate::cursor::visual::point(20.0, 30.0, Some(42)),
                    &receipt,
                    async {
                        receipt.accepted();
                        Some(cua_driver_core::protocol::ToolResult::text("focused"))
                    },
                    async { panic!("semantic focus accepted; native fallback must stay lazy") },
                )
                .await;
            assert_eq!(result.is_error, None);
            if schedule != "coalesced" {
                multi_stage_assert(&sink, &mut map, Foreground, Pixel);
            }
            multi_stage_expire_between(&sink, &mut map, schedule);
            let result = crate::tools::with_type_visual_updates(
                &state.cursor_registry,
                visual_sink.as_ref(),
                "one",
                None,
                |update| {
                    update(ResolvedPointerTarget::from_bounds(
                        42,
                        [10.0, 20.0, 80.0, 30.0],
                    ));
                    multi_stage_assert(&sink, &mut map, Foreground, Pixel);
                    assert_eq!(
                        map.cursors["one"].core.visual.requested_action,
                        cursor_overlay::CursorAction::Text
                    );
                    23
                },
            );
            assert_eq!(result, 23);
            let events = sink.1.lock().unwrap();
            assert_ne!(events[0].id, events[2].id);
            assert_eq!(
                events.last().unwrap().phase,
                cursor_overlay::VisualPhase::End
            );
            drop(events);
            multi_stage_cleanup_and_unrelated(&sink, &state.cursor_registry, &mut map);
        }
    }

    #[test]
    fn multi_stage_menu_hops_keep_invocation_modifiers() {
        use cua_driver_contract::{CursorDelivery::Foreground, CursorTarget::Desktop};
        for schedule in ["coalesced", "split", "expired"] {
            let registry = super::super::CursorRegistry::new();
            let sink = std::sync::Arc::new(WatchableInboxSink::default());
            let args = serde_json::json!({"pid":7,"window_id":42,
                "path":["File","Open"],"delivery_mode":"foreground"});
            multi_stage_begin(&sink, "invoke_menu", &args);
            let visual_sink = crate::cursor::visual::InvocationVisualSink::bind(
                "invoke_menu",
                &args,
                "one",
                sink.clone(),
            );
            let mut map = empty_map();
            if schedule == "expired" {
                let now = Instant::now();
                sink.0.lock().unwrap().take().apply(&mut map, now);
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, now + Duration::from_secs(2));
                assert_eq!(map.cursors["one"].core.badge_modifiers, None);
                *sink.2.lock().unwrap() = Duration::from_secs(3);
            }
            for (stage, bounds) in [Some([10.0, 20.0, 40.0, 20.0]), None]
                .into_iter()
                .enumerate()
            {
                let result = crate::tools::dispatch_menu_visual(
                    &registry,
                    visual_sink.as_ref(),
                    "one",
                    bounds,
                    42,
                    || Ok::<_, ()>(19),
                );
                assert_eq!(result, Ok(19));
                if schedule != "coalesced" || stage == 1 {
                    multi_stage_assert(&sink, &mut map, Foreground, Desktop);
                }
                multi_stage_expire_between(&sink, &mut map, schedule);
            }
            multi_stage_cleanup_and_unrelated(&sink, &registry, &mut map);
        }
    }

    fn multi_stage_expire_between(sink: &WatchableInboxSink, map: &mut RenderMap, schedule: &str) {
        if schedule == "expired" {
            let now = sink.1.lock().unwrap().last().unwrap().timestamp;
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, now + Duration::from_secs(2));
            // Text delivery is scoped by End, so it need not expire mid-delivery.
            *sink.2.lock().unwrap() += Duration::from_secs(3);
        }
    }

    fn multi_stage_cleanup_and_unrelated(
        sink: &WatchableInboxSink,
        registry: &super::super::CursorRegistry,
        map: &mut RenderMap,
    ) {
        use crate::cursor::visual::{emit_action_target, PointerVisualSink};
        use cursor_overlay::{CursorAction, VisualPhase};
        let stale = sink.1.lock().unwrap().first().unwrap().clone();
        let now = sink.1.lock().unwrap().last().unwrap().timestamp;
        sink.0.lock().unwrap().take().apply(map, now);
        map.cursors
            .get_mut("one")
            .unwrap()
            .tick_at(0.0, now + Duration::from_secs(2));
        assert_eq!(map.cursors["one"].core.badge_modifiers, None);
        assert!(map.cursors["one"].core.contact.is_none());
        *sink.2.lock().unwrap() += Duration::from_secs(3);
        // A raw new owner has no invocation context, even in the same session.
        let unrelated = emit_action_target(registry, sink, "one", None, CursorAction::Text);
        unrelated.publish(sink, VisualPhase::Tracking, Instant::now());
        // Old-stage publications cannot replace this owner or restore its badge.
        sink.publish("one", stale);
        let now = Instant::now() + *sink.2.lock().unwrap();
        sink.0.lock().unwrap().take().apply(map, now);
        let core = &map.cursors["one"].core;
        assert_eq!(core.visual.delivery, None);
        assert_eq!(core.visual.target, None);
        assert_eq!(core.badge_modifiers, None);
    }

    #[test]
    fn multi_stage_explicit_new_invocation_does_not_borrow_unresolved_generic_context() {
        use crate::cursor::visual::{emit_action_target, InvocationVisualSink};
        let sink = std::sync::Arc::new(WatchableInboxSink::default());
        multi_stage_begin(
            &sink,
            "scroll",
            &serde_json::json!({
                "delivery_mode":"background", "element_index":1
            }),
        );
        let registry = super::super::CursorRegistry::new();
        let args = serde_json::json!({"x":20,"y":30,"text":"next"});
        let current = InvocationVisualSink::bind("type_text", &args, "one", sink.clone());
        emit_action_target(
            &registry,
            current.as_ref(),
            "one",
            None,
            cursor_overlay::CursorAction::Text,
        );
        let mut map = empty_map();
        sink.0
            .lock()
            .unwrap()
            .take()
            .apply(&mut map, Instant::now());
        let core = &map.cursors["one"].core;
        assert_eq!(core.visual.delivery, None);
        assert_eq!(
            core.visual.target,
            Some(cua_driver_contract::CursorTarget::Pixel)
        );
    }

    #[test]
    fn multi_stage_context_is_local_and_lifecycle_cleanup_rejects_old_stages() {
        use crate::cursor::visual::{emit_action_target, InvocationVisualSink, PointerVisualSink};
        use cursor_overlay::{CursorAction, VisualPhase};
        for cleanup in ["end", "disable", "remove"] {
            let sink = std::sync::Arc::new(WatchableInboxSink::default());
            let args = serde_json::json!({"direction":"down", "amount":2,
                "element_token":"opaque", "delivery_mode":"background"});
            multi_stage_begin(&sink, "scroll", &args);
            let registry = super::super::CursorRegistry::new();
            let invocation = InvocationVisualSink::bind("scroll", &args, "one", sink.clone());
            for _ in 0..2 {
                crate::tools::dispatch_scroll_visual(
                    &registry,
                    invocation.as_ref(),
                    "one",
                    crate::cursor::visual::point(30.0, 40.0, Some(42)),
                    -1,
                    0,
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            }
            let mut map = empty_map();
            multi_stage_assert(
                &sink,
                &mut map,
                cua_driver_contract::CursorDelivery::Background,
                cua_driver_contract::CursorTarget::Ax,
            );
            let last = sink.1.lock().unwrap().last().unwrap().clone();
            match cleanup {
                "end" => {
                    let mut end = last.clone();
                    end.phase = VisualPhase::End;
                    end.timestamp = Instant::now();
                    invocation.publish("one", end);
                }
                "disable" => {
                    sink.send("one", OverlayCommand::SetEnabled(false));
                    sink.send("one", OverlayCommand::SetEnabled(true));
                }
                "remove" => {
                    sink.0
                        .lock()
                        .unwrap()
                        .command(OverlayMsg::Remove("one".into()));
                    // Publication after tombstoning is refused even with explicit context.
                    invocation.publish("one", last.clone());
                    sink.0
                        .lock()
                        .unwrap()
                        .take()
                        .apply(&mut map, Instant::now());
                    assert!(!map.cursors.contains_key("one"));
                    sink.0
                        .lock()
                        .unwrap()
                        .command(OverlayMsg::Revive("one".into()));
                }
                _ => unreachable!(),
            }
            sink.0
                .lock()
                .unwrap()
                .take()
                .apply(&mut map, Instant::now());
            if let Some(state) = map.cursors.get_mut("one") {
                state.tick_at(0.0, Instant::now() + Duration::from_secs(2));
                assert!(state.core.contact.is_none());
                assert_eq!(state.core.badge_modifiers, None);
            }
            // Binding a publisher never lends its context to another session.
            emit_action_target(
                &registry,
                invocation.as_ref(),
                "two",
                None,
                CursorAction::Text,
            );
            let now = Instant::now();
            sink.0.lock().unwrap().take().apply(&mut map, now);
            assert_eq!(map.cursors["two"].core.badge_modifiers, None);
            assert_eq!(map.cursors["two"].core.visual.target, None);
            drop(invocation);
            *sink.2.lock().unwrap() = Duration::from_secs(3);
            let next =
                emit_action_target(&registry, sink.as_ref(), "one", None, CursorAction::Text);
            next.publish(sink.as_ref(), VisualPhase::Tracking, Instant::now());
            let new_id = sink.1.lock().unwrap().last().unwrap().id;
            assert_ne!(last.id, new_id);
            for phase in [VisualPhase::Intent, VisualPhase::Contact, VisualPhase::End] {
                let mut stale = last.clone();
                stale.phase = phase;
                stale.timestamp = Instant::now();
                sink.publish("one", stale);
            }
            sink.0
                .lock()
                .unwrap()
                .take()
                .apply(&mut map, Instant::now() + Duration::from_secs(3));
            let core = &map.cursors["one"].core;
            assert_eq!(core.visual.requested_action, CursorAction::Text);
            assert_eq!(core.visual.delivery, None);
            assert_eq!(core.visual.target, None);
            assert_eq!(core.badge_modifiers, None);
        }
    }

    #[test]
    fn watchable_actual_receipt_fallback_glides_to_delivered_point_with_fresh_bounds() {
        use super::super::visual::{begin_pointer_action, ResolvedPointerTarget};
        use std::sync::Arc;
        for route in ["click", "moved", "missing"] {
            for drain_intent in [false, true] {
                let registry = super::super::CursorRegistry::new();
                let sink = Arc::new(WatchableInboxSink::default());
                let mut map = empty_map();
                map.layout.displays[0].width = 1000.0;
                map.layout.displays[0].height = 800.0;
                let receipt = begin_pointer_action(
                    &registry,
                    sink.clone(),
                    "one",
                    (route != "missing").then_some(ResolvedPointerTarget {
                        x: if route == "click" { 700.0 } else { 100.0 },
                        y: if route == "click" { 500.0 } else { 100.0 },
                        window_id: Some(42),
                        element_bounds: (route != "click").then_some([90.0, 90.0, 20.0, 20.0]),
                    }),
                    cursor_overlay::CursorAction::Click,
                );
                let original = sink.1.lock().unwrap()[0].clone();
                if drain_intent {
                    sink.0
                        .lock()
                        .unwrap()
                        .take()
                        .apply(&mut map, Instant::now());
                }
                let result: anyhow::Result<u32> = if route == "click" {
                    receipt.dispatch(|| Ok(73))
                } else {
                    receipt.dispatch_at(&registry, 700.0, 500.0, 42, |x, y| {
                        assert_eq!((x, y), (700.0, 500.0));
                        Ok(73)
                    })
                };
                assert_eq!(result.unwrap(), 73);
                assert!(receipt.was_accepted());
                let batch = sink.0.lock().unwrap().take();
                let intent = batch.pending["one"]
                    .latest
                    .as_ref()
                    .map_or(&original, |published| &published.event);
                let contact = &batch.pending["one"].contact.as_ref().unwrap().event;
                let start = intent.timestamp;
                let accepted = contact.timestamp;
                assert_eq!(intent.id, contact.id);
                assert_eq!(contact.target, Some((700.0, 500.0)));
                assert!(contact.bounds.is_none());
                assert!(accepted >= start);
                batch.apply(&mut map, accepted);
                assert!(map.cursors["one"].core.path.is_some());
                assert!(map.cursors["one"].core.contact.is_none());
                assert!(map.cursors["one"].core.focus_rect.is_none());
                assert_eq!(map.cursors["one"].core.pinned_wid, Some(42));
                let logical = registry.get("one").unwrap().position.unwrap();
                assert_eq!((logical.x, logical.y), (700.0, 500.0));
                map.cursors
                    .get_mut("one")
                    .unwrap()
                    .tick_at(0.0, start + Duration::from_millis(220));
                assert_mailbox_tip(&map, (700.0, 500.0));
                let pulse = map.cursors["one"].core.contact.unwrap();
                assert_eq!(pulse.target, (700.0, 500.0));
                assert_eq!(pulse.timestamp, accepted);
                // This event-only fixture models disabled admission. Mac visual
                // travel now finishes earlier; the true receipt time stays intact.
                let expected_age = (start + Duration::from_millis(220))
                    .saturating_duration_since(pulse.presentation_timestamp)
                    .as_secs_f64()
                    / 0.150;
                assert!((pulse.progress - expected_age).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn watchable_inbox_reduced_motion_and_already_arrived_clicks_pulse_immediately() {
        use cursor_overlay::VisualPhase::*;
        for reduced in [false, true] {
            let mut inbox = OverlayInbox::default();
            let mut map = empty_map();
            if reduced {
                map.template.reduced_motion = cursor_overlay::ReducedMotion::On;
            }
            let t = Instant::now();
            if !reduced {
                let prior = inbox.begin_action("one").unwrap();
                let mut event = mailbox_event(prior, 80.0, Contact);
                event.timestamp = t;
                inbox.publish("one", event);
                inbox.take().apply(&mut map, t);
            }
            let id = inbox.begin_action("one").unwrap();
            for phase in [Intent, Contact] {
                let mut event = mailbox_event(id, 80.0, phase);
                event.timestamp = t + Duration::from_millis(1);
                inbox.publish("one", event);
            }
            inbox.take().apply(&mut map, t + Duration::from_millis(1));
            assert_mailbox_tip(&map, (80.0, 30.0));
            let core = &map.cursors["one"].core;
            assert!(core.path.is_none());
            let pulse = core.contact.unwrap();
            assert_eq!(pulse.timestamp, t + Duration::from_millis(1));
            assert_eq!(pulse.presentation_timestamp, pulse.timestamp);
            assert_eq!(pulse.progress, 0.0);
            map.cursors
                .get_mut("one")
                .unwrap()
                .tick_at(0.0, t + Duration::from_millis(151));
            assert!(map.cursors["one"].core.contact.is_none());
        }
    }

    #[test]
    fn watchable_inbox_stall_and_lifecycle_never_replay_pending_contact() {
        use cursor_overlay::VisualPhase::*;
        for cleanup in ["stall", "disable", "remove", "display", "end", "new_owner"] {
            let mut inbox = OverlayInbox::default();
            let mut map = empty_map();
            let t = Instant::now();
            let id = inbox.begin_action("one").unwrap();
            for phase in [Intent, Contact] {
                let mut event = mailbox_event(id, 80.0, phase);
                event.timestamp = t;
                inbox.publish("one", event);
            }
            if cleanup == "stall" {
                inbox.take().apply(&mut map, t + Duration::from_secs(2));
            } else {
                inbox.take().apply(&mut map, t);
                assert!(map.cursors["one"].core.contact.is_none());
                match cleanup {
                    "disable" => {
                        inbox.command(command("one", OverlayCommand::SetEnabled(false)));
                        inbox.command(command("one", OverlayCommand::SetEnabled(true)));
                    }
                    "remove" => {
                        inbox.command(OverlayMsg::Remove("one".into()));
                        inbox.command(OverlayMsg::Revive("one".into()));
                    }
                    "display" => {
                        map.layout.displays.clear();
                        let mut event = mailbox_event(id, 80.0, Contact);
                        event.timestamp = t + Duration::from_millis(1);
                        inbox.publish("one", event);
                    }
                    "end" => {
                        let mut event = mailbox_event(id, 80.0, End);
                        event.timestamp = t + Duration::from_millis(1);
                        inbox.publish("one", event);
                    }
                    "new_owner" => {
                        let newer = inbox.begin_action("one").unwrap();
                        let mut event = mailbox_event(newer, 20.0, Intent);
                        event.timestamp = t + Duration::from_millis(1);
                        inbox.publish("one", event);
                    }
                    _ => unreachable!(),
                }
                inbox.take().apply(&mut map, t + Duration::from_millis(1));
            }
            if let Some(state) = map.cursors.get_mut("one") {
                state.tick_at(0.0, t + Duration::from_millis(250));
                assert!(state.core.contact.is_none(), "cleanup={cleanup}");
                assert!(state.core.path.is_none(), "cleanup={cleanup}");
                state.tick_at(0.0, t + Duration::from_secs(3));
                assert!(state.core.contact.is_none());
            }
        }
    }

    #[test]
    fn slice_a_generic_end_cannot_cancel_resolved_or_restart_expired_cue() {
        use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let semantic = |phase| CursorEvent::Action {
            session: "one".into(),
            phase,
            semantics: cua_driver_contract::CursorSemantics::new(
                cursor_overlay::CursorAction::Click,
            ),
        };
        inbox.semantic(semantic(CursorEventPhase::Begin));
        let id = inbox.begin_action("one").unwrap();
        let t = Instant::now();
        let event = cursor_overlay::VisualEvent {
            id,
            timestamp: t,
            target: Some((40.0, 50.0)),
            window: Some(42),
            bounds: None,
            action: cursor_overlay::CursorAction::Click,
            scroll_direction: None,
            modifiers: None,
            phase: cursor_overlay::VisualPhase::Contact,
        };
        inbox.publish("one", event);
        inbox.semantic(semantic(CursorEventPhase::End));
        inbox.take().apply(&mut map, t + Duration::from_millis(75));
        assert_eq!(
            map.cursors["one"].core.visual.resolved_action,
            cursor_overlay::CursorAction::Click
        );
        assert!((map.cursors["one"].core.contact.unwrap().progress - 0.5).abs() < 1e-9);
        map.cursors
            .get_mut("one")
            .unwrap()
            .tick_at(0.0, t + Duration::from_secs(2));
        inbox.semantic(semantic(CursorEventPhase::End));
        inbox.take().apply(&mut map, t + Duration::from_secs(2));
        assert!(map.cursors["one"].core.contact.is_none());
        assert_eq!(
            map.cursors["one"].core.visual.resolved_action,
            cursor_overlay::CursorAction::Idle
        );
        inbox.semantic(semantic(CursorEventPhase::Begin));
        inbox
            .take()
            .apply(&mut map, Instant::now() + Duration::from_secs(2));
        assert_eq!(
            map.cursors["one"].core.visual.resolved_action,
            cursor_overlay::CursorAction::Idle
        );
    }

    #[test]
    fn slice_a_legacy_move_and_admitted_motion_keep_configured_duration() {
        for duration in [0.0, 850.0] {
            let mut inbox = OverlayInbox::default();
            let mut map = empty_map();
            let mut motion = MotionConfig::default();
            motion.glide_duration_ms = duration;
            inbox.command(command("legacy", OverlayCommand::SetMotion(motion.clone())));
            inbox.command(move_msg("legacy", 80.0, 90.0));
            assert_eq!(inbox.motion["legacy"], motion);
            inbox.take().apply(&mut map, Instant::now());
            assert_eq!(map.cursors["legacy"].core.motion, motion);
            assert!(map.cursors["legacy"].core.path.is_some());
            // Legacy motion is not a timestamped action with a 220 ms deadline.
            map.cursors
                .get_mut("legacy")
                .unwrap()
                .tick_at(0.0, Instant::now() + Duration::from_secs(1));
            assert!(map.cursors["legacy"].core.path.is_some());
        }
    }

    #[test]
    fn slice_a_producer_and_completion_do_not_wait_for_render() {
        let held_render = RENDER.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let registry = super::super::CursorRegistry::new();
                let sink = super::super::visual::OverlayVisualSink;
                let handle = super::super::visual::emit_pointer_target(
                    &registry,
                    &sink,
                    "stalled-native",
                    Some(super::super::visual::ResolvedPointerTarget {
                        x: 80.0,
                        y: 90.0,
                        window_id: Some(42),
                        element_bounds: None,
                    }),
                );
                let native = || {
                    assert_eq!(
                        registry.get("stalled-native").unwrap().position.unwrap().x,
                        80.0
                    );
                };
                native();
                super::super::visual::emit_pointer_contact(&sink, handle);
                current_motion("stalled-native");
                is_visible_for_session("stalled-native");
                remove_cursor("stalled-native".into());
                tx.send(()).unwrap();
            });
        });
        let completed = rx.recv_timeout(Duration::from_millis(200));
        drop(held_render);
        worker.join().unwrap();
        assert!(
            completed.is_ok(),
            "input and explicit completion must finish with renderer held and zero drains"
        );
    }

    #[test]
    fn slice_a_fix_newer_action_replaces_pending_older_contact() {
        newer_action_before_older_timestamp(false);
    }

    #[test]
    fn slice_a_fix_newer_action_replaces_applied_older_contact() {
        newer_action_before_older_timestamp(true);
    }

    fn late_intent_after_tracking(detach: bool) {
        use cursor_overlay::VisualPhase::*;
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let t = Instant::now();
        let id = inbox.begin_action("one").unwrap();
        let publish = |inbox: &mut OverlayInbox, phase, x| {
            let mut event = mailbox_event(id, x, phase);
            event.timestamp = t;
            inbox.publish("one", event)
        };
        assert!(publish(&mut inbox, Contact, 20.0));
        if detach {
            assert!(inbox.take().apply(&mut map, t));
        }
        assert!(publish(&mut inbox, Tracking, 60.0));
        let detached = detach.then(|| inbox.take());
        if let Some(batch) = &detached {
            assert_eq!(batch.pending.len(), 1);
            assert_eq!(
                batch.pending["one"].latest.as_ref().unwrap().event.phase,
                Tracking
            );
        }
        assert!(!publish(&mut inbox, Intent, 20.0));
        if let Some(batch) = detached {
            assert!(batch.apply(&mut map, t));
            assert!(!inbox.take().apply(&mut map, t));
        } else {
            assert!(inbox.take().apply(&mut map, t));
        }
        assert_eq!(map.cursors["one"].target, Some((60.0, 30.0)));
        assert_mailbox_tip(&map, (60.0, 30.0));
        assert!(map.cursors["one"].core.path.is_none());
        assert!(publish(&mut inbox, Tracking, 80.0));
        assert!(inbox.take().apply(&mut map, t));
        assert_mailbox_tip(&map, (80.0, 30.0));
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111))]);
    }

    #[test]
    fn slice_a_fix_undrained_tracking_rejects_late_intent() {
        late_intent_after_tracking(false);
    }

    #[test]
    fn slice_a_fix_nonempty_detached_tracking_rejects_late_intent() {
        late_intent_after_tracking(true);
    }

    fn bounds_map() -> (RenderMap, DisplayGeometry) {
        let mut map = empty_map();
        map.template.motion.idle_hide_ms = 0.0;
        let left = DisplayGeometry {
            id: 2,
            x: -200.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
            backing_scale: 2.0,
            is_primary: false,
        };
        map.layout.displays.push(left);
        (map, left)
    }

    fn publish_contact_bounds(inbox: &mut OverlayInbox, t: Instant) {
        let id = inbox.begin_action("one").unwrap();
        let mut contact = mailbox_event(id, 90.0, cursor_overlay::VisualPhase::Contact);
        contact.timestamp = t;
        contact.bounds = Some([-150.0, 10.0, 40.0, 40.0]);
        assert!(inbox.publish("one", contact));
    }

    fn assert_bounds_expired(map: &RenderMap, left: DisplayGeometry) {
        assert!(map.cursors["one"].core.focus_rect.is_none());
        assert!(map.cursors["one"].core.contact.is_none());
        assert_eq!(painted_display_ids(map), HashSet::from([1]));
        assert_eq!(ordering_pairs(map), vec![(1, Some(111))]);
        assert!(render_display(map, left)
            .pixels()
            .iter()
            .all(|p| p.alpha() == 0));
        assert!(!render_map_needs_frame_tick(map));
    }

    #[test]
    fn slice_a_fix_old_contact_bounds_expire_before_first_paint() {
        let mut inbox = OverlayInbox::default();
        let (mut map, left) = bounds_map();
        let t = Instant::now();
        publish_contact_bounds(&mut inbox, t);
        let now = t + Duration::from_secs(1);
        assert!(inbox.take().apply(&mut map, now));
        assert_bounds_expired(&map, left);
        map.cursors.get_mut("one").unwrap().tick_at(0.0, now);
        assert_bounds_expired(&map, left);
    }

    #[test]
    fn slice_a_fix_contact_bounds_age_and_expire_on_zero_delta_tick() {
        let mut inbox = OverlayInbox::default();
        let (mut map, left) = bounds_map();
        let t = Instant::now();
        publish_contact_bounds(&mut inbox, t);
        let now = t + Duration::from_millis(300);
        assert!(inbox.take().apply(&mut map, now));
        map.cursors.get_mut("one").unwrap().tick_at(0.0, now);
        assert!((map.cursors["one"].core.focus_rect_t - 0.5).abs() < 1e-9);
        assert_eq!(painted_display_ids(&map), HashSet::from([1, 2]));
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111)), (2, Some(111))]);
        assert!(render_display(&map, left)
            .pixels()
            .iter()
            .any(|p| p.alpha() > 0));
        map.cursors
            .get_mut("one")
            .unwrap()
            .tick_at(0.0, t + Duration::from_millis(600));
        assert_bounds_expired(&map, left);
    }

    #[test]
    fn slice_a_fix_legacy_focus_rect_keeps_delta_based_fade() {
        let mut inbox = OverlayInbox::default();
        let (mut map, left) = bounds_map();
        let t = Instant::now();
        publish_contact_bounds(&mut inbox, t);
        assert!(inbox.take().apply(&mut map, t + Duration::from_millis(300)));
        inbox.command(command(
            "one",
            OverlayCommand::ShowFocusRect(Some([-150.0, 10.0, 40.0, 40.0])),
        ));
        let now = t + Duration::from_secs(2);
        assert!(inbox.take().apply(&mut map, now));
        map.cursors.get_mut("one").unwrap().tick_at(0.0, now);
        assert_eq!(map.cursors["one"].core.focus_rect_t, 0.0);
        assert!(render_display(&map, left)
            .pixels()
            .iter()
            .any(|p| p.alpha() > 0));
        map.cursors.get_mut("one").unwrap().tick_at(0.3, now);
        assert!((map.cursors["one"].core.focus_rect_t - 0.5).abs() < 1e-9);
        map.cursors.get_mut("one").unwrap().tick_at(0.3, now);
        assert_bounds_expired(&map, left);
        inbox.command(command("one", OverlayCommand::ShowFocusRect(None)));
        assert!(inbox.take().apply(&mut map, now));
        assert!(map.cursors["one"].core.focus_rect.is_none());
    }

    #[test]
    fn slice_a_mailbox_equal_timestamps_keep_accepted_order_across_drains() {
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let t = Instant::now();
        let one = inbox.begin_action("one").unwrap();
        let two = inbox.begin_action("two").unwrap();
        for (key, id, x, window) in [
            ("one", one, 20.0, 111),
            ("two", two, 40.0, 222),
            ("one", one, 60.0, 111),
        ] {
            let mut event = mailbox_event(id, x, cursor_overlay::VisualPhase::Tracking);
            event.timestamp = t;
            event.window = Some(window);
            assert!(inbox.publish(key, event));
            inbox.take().apply(&mut map, t);
        }
        assert_eq!(map.cursors["one"].target, Some((60.0, 30.0)));
        assert_eq!(map.command_order, vec!["two", "one"]);
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111))]);
    }

    #[test]
    fn slice_a_mailbox_contact_routes_its_own_surface_and_expires_quiescently() {
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let left = DisplayGeometry {
            id: 2,
            x: -200.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
            backing_scale: 2.0,
            is_primary: false,
        };
        map.layout.displays.push(left);
        map.template.motion.idle_hide_ms = 0.0;
        let t = Instant::now();
        let id = inbox.begin_action("one").unwrap();
        let mut contact = mailbox_event(id, -100.0, cursor_overlay::VisualPhase::Contact);
        contact.timestamp = t;
        inbox.publish("one", contact);
        let mut tracking = mailbox_event(id, 90.0, cursor_overlay::VisualPhase::Tracking);
        tracking.timestamp = t + Duration::from_millis(10);
        inbox.publish("one", tracking);
        inbox.take().apply(&mut map, t + Duration::from_millis(10));
        let core = &mut map.cursors.get_mut("one").unwrap().core;
        core.advance_visual_presentation(t + Duration::from_millis(50));
        assert_eq!(core.contact.unwrap().target, (-100.0, 30.0));
        assert_eq!(map.cursors["one"].target, Some((90.0, 30.0)));
        assert_eq!(painted_display_ids(&map), HashSet::from([1, 2]));
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111)), (2, Some(111))]);
        let pm = render_display(&map, left);
        let painted: Vec<_> = pm
            .pixels()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alpha() > 0)
            .map(|(i, _)| (i as u32 % pm.width(), i as u32 / pm.width()))
            .collect();
        assert!(!painted.is_empty());
        assert!(painted
            .iter()
            .all(|(x, y)| (158..=242).contains(x) && (18..=102).contains(y)));
        map.cursors
            .get_mut("one")
            .unwrap()
            .core
            .tick_swift_constants_at(0.0, t + Duration::from_millis(201));
        assert_eq!(painted_display_ids(&map), HashSet::from([1]));
        assert!(!render_map_needs_frame_tick(&map));
    }

    #[test]
    fn slice_a_mailbox_detached_renderer_and_saturated_commands_do_not_block_visuals() {
        use std::sync::{mpsc, Arc};
        let inbox = Arc::new(Mutex::new(OverlayInbox::default()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let consumer = inbox.clone();
        let worker = std::thread::spawn(move || {
            let batch = consumer.lock().unwrap().take();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            batch.apply(&mut empty_map(), Instant::now());
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let producer = inbox.clone();
        let publisher = std::thread::spawn(move || {
            let mut inbox = producer.lock().unwrap();
            for _ in 0..5000 {
                inbox.command(command("old", OverlayCommand::PinAbove(7)));
            }
            let id = inbox.begin_action("old").unwrap();
            inbox.publish(
                "old",
                mailbox_event(id, 50.0, cursor_overlay::VisualPhase::Intent),
            );
            inbox.command(OverlayMsg::Remove("old".into()));
            let fresh = inbox.begin_action("new").unwrap();
            let accepted = inbox.publish(
                "new",
                mailbox_event(fresh, 70.0, cursor_overlay::VisualPhase::Intent),
            );
            done_tx.send(accepted).unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        publisher.join().unwrap();
        assert!(result.unwrap());
        let mut map = empty_map();
        inbox.lock().unwrap().take().apply(&mut map, Instant::now());
        assert!(!map.cursors.contains_key("old"));
        assert_eq!(map.cursors["new"].target, Some((70.0, 30.0)));
    }

    #[test]
    fn slice_a_mailbox_two_sessions_follow_accepted_order_after_coalescing() {
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let one = inbox.begin_action("one").unwrap();
        let two = inbox.begin_action("two").unwrap();
        inbox.publish(
            "one",
            mailbox_event(one, 20.0, cursor_overlay::VisualPhase::Intent),
        );
        let mut e = mailbox_event(two, 40.0, cursor_overlay::VisualPhase::Intent);
        e.window = Some(222);
        inbox.publish("two", e);
        inbox.publish(
            "one",
            mailbox_event(one, 60.0, cursor_overlay::VisualPhase::Intent),
        );
        inbox.take().apply(&mut map, Instant::now());
        assert_eq!(map.command_order, vec!["two", "one"]);
        assert_eq!(ordering_pairs(&map), vec![(1, Some(111))]);
        assert_eq!(map.cursors["one"].target, Some((60.0, 30.0)));
        inbox.command(command("two", OverlayCommand::PinAbove(333)));
        inbox.take().apply(&mut map, Instant::now());
        assert_eq!(ordering_pairs(&map), vec![(1, Some(333))]);
    }

    #[test]
    fn slice_a_mailbox_remove_late_contact_revive_before_drain_is_fresh() {
        let now = Instant::now();
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        let old = inbox.begin_action("one").unwrap();
        inbox.publish(
            "one",
            mailbox_event(old, 20.0, cursor_overlay::VisualPhase::Intent),
        );
        inbox.take().apply(&mut map, now);
        assert!(map.cursors.contains_key("one"));
        inbox.command(OverlayMsg::Remove("one".into()));
        assert!(!inbox.publish(
            "one",
            mailbox_event(old, 20.0, cursor_overlay::VisualPhase::Contact)
        ));
        inbox.command(OverlayMsg::Revive("one".into()));
        assert!(!inbox.publish(
            "one",
            mailbox_event(old, 20.0, cursor_overlay::VisualPhase::Contact)
        ));
        let fresh = inbox.begin_action("one").unwrap();
        assert_ne!(fresh.generation, old.generation);
        inbox.publish(
            "one",
            mailbox_event(fresh, 80.0, cursor_overlay::VisualPhase::Intent),
        );
        inbox.take().apply(&mut map, now);
        assert_eq!(map.cursors["one"].target, Some((80.0, 30.0)));
        assert_eq!(map.cursors["one"].core.pos, (2.0, 2.0));
        inbox.command(OverlayMsg::Remove("one".into()));
        inbox.take().apply(&mut map, now);
        assert!(!map.cursors.contains_key("one"));
        assert!(map.command_order.is_empty());
    }

    #[test]
    fn slice_a_mailbox_stalled_adapter_retains_newest_without_queued_visual_commands() {
        let mut inbox = OverlayInbox::default();
        let mut map = empty_map();
        for n in 0..10_000 {
            let id = inbox.begin_action("one").unwrap();
            assert!(inbox.publish(
                "one",
                mailbox_event(id, (n % 90) as f64, cursor_overlay::VisualPhase::Intent)
            ));
        }
        assert!(inbox.commands.is_empty());
        let batch = inbox.take();
        assert_eq!(batch.pending.len(), 1);
        batch.apply(&mut map, Instant::now());
        assert_eq!(map.cursors["one"].target, Some((9.0, 30.0)));
    }

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
    fn slice_a_layout_loss_invalidates_pending_target() {
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
        replace_display_layout(
            &mut map,
            DisplayLayout {
                generation: 2,
                displays: vec![primary],
            },
        );
        assert!(!map.cursors["lost"].core.placed);
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
    fn slice_a_missing_display_never_places() {
        for empty in [false, true] {
            let mut map = empty_map();
            if empty {
                map.layout.displays.clear();
            }
            let key = format!("slice-a-missing-{empty}");
            apply_msg(&mut map, move_msg(&key, -1400.0, -700.0));
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
        apply_msg(&mut map, move_msg("late", 50.0, 50.0));
        assert!(!map.cursors.contains_key("late"));
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
        impl super::super::visual::PointerVisualSink for RenderSink {
            fn send(&self, key: &str, cmd: OverlayCommand) {
                apply_msg(&mut self.0.lock().unwrap(), command(key, cmd));
            }
            fn begin(&self, key: &str) -> Option<cursor_overlay::VisualActionId> {
                begin_visual_action(key)
            }
            fn publish(&self, key: &str, event: cursor_overlay::VisualEvent) {
                let now = event.timestamp;
                apply_visual_in_map(&mut self.0.lock().unwrap(), key.into(), event, now);
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
        );
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
        rs.core.focus_rect = None;
        rs.core.idle_alpha = 0.0;
        assert!(
            !render_map_needs_frame_tick(&map),
            "fully hidden idle cursor should quiesce"
        );
    }
}
