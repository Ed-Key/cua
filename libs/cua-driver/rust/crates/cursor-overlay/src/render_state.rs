//! Shared cursor-overlay render state, animation tick, and pixel pipeline.
//!
//! Lifts the platform-agnostic render state out of the three per-OS
//! `overlay.rs` files (macOS / Windows / Linux). Before the 2026-05 dedup
//! audit each platform owned a ~600-line copy of the same animation logic
//! that differed only in a few constants and feature flags.
//!
//! ## What lives here
//!
//! - [`RenderStateCore`] — the platform-agnostic animation and semantic state.
//! - [`RenderStateCore::tick_motion`] — speed-profile + spring physics +
//!   click-pulse + idle-fade using runtime [`MotionConfig`] (Windows + Linux).
//! - [`RenderStateCore::tick_swift_constants`] — same physics but with the
//!   hardcoded Swift reference constants used by macOS; returns whether the
//!   path just ended (so the caller can fire arrival signals).
//! - [`RenderStateCore::apply_command_base`] — the OverlayCommand match arms
//!   that all three platforms implement identically (MoveTo / ClickPulse /
//!   SetEnabled / SetMotion / SetTheme / semantic action events / PinAbove).
//!   Returns `false` for variants the core doesn't handle so platforms can
//!   layer their own native behavior on top.
//! - [`render_frame`] — the tiny-skia paint of the selected cursor theme,
//!   parameterized by pixmap dimensions and a global origin offset.
//!
//! ## What stays per-platform
//!
//! - The OS window / surface (NSWindow / HWND / X11 Window) and its message
//!   loop or run-loop.
//! - Native surface presentation through Core Animation, `UpdateLayeredWindow`,
//!   or `XPutImage`.
//! - Translation from global coordinates into native surfaces.
//! - Projection of the shared focus highlight into each native surface.

use crate::{
    CompiledTheme, CursorAction, CursorConfig, CursorVisualState, DeliveryModifier, MotionConfig,
    OverlayCommand, PathPlanner, PathState, PlannedPath, Spring, TargetModifier,
};
use crate::{VisualActionId, VisualEvent, VisualPhase};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SESSION_BADGE_HOLD_SECS: f64 = 2.0;
pub const SESSION_BADGE_FADE_SECS: f64 = 0.4;

/// Platform-agnostic render state shared by macOS / Windows / Linux overlays.
///
/// Each platform wraps this in its own struct that adds OS-specific fields
/// (e.g. `virt_x/y/w/h` on Windows, `focus_rect` on macOS).
pub struct RenderStateCore {
    /// Frozen copy of the launch-time CursorConfig.
    pub cfg: CursorConfig,
    /// Current motion / timing config (mutable via [`OverlayCommand::SetMotion`]).
    pub motion: MotionConfig,
    /// Current rendered position in screen / overlay-window coordinates.
    pub pos: (f64, f64),
    /// Whether the cursor has received its first real placement.
    pub placed: bool,
    /// Visual heading in radians (tip direction = motion_dir + π).
    pub heading: f64,
    /// In-flight planned path; `None` = at rest.
    pub path: Option<PlannedPath>,
    /// Arc-distance travelled along the current path so far.
    pub dist: f64,
    /// Post-arrival spring-settle state.
    pub spring: Option<Spring>,
    /// Target the spring is settling toward: `(x, y, heading)`.
    pub spring_tgt: Option<(f64, f64, f64)>,
    /// Click-pulse phase 0..1; `None` = no pulse in flight.
    pub click_t: Option<f64>,
    pub contact: Option<ContactPresentation>,
    /// Ongoing delivery owns its highlight until End, then a separate 600 ms fade.
    pub focus_rect: Option<[f64; 4]>,
    pub focus_rect_t: f64,
    focus_rect_timestamp: Option<Instant>,
    delivery_active: bool,
    visual_owner: Option<(VisualActionId, Instant, VisualPhase)>,
    // Mac resolved-target opt-in, owned by the accepted visual action. Legacy
    // commands and other adapters retain their 16-point logical convention.
    registered_target: bool,
    visual_travel: Option<(Instant, Duration)>,
    // Retain the planned arrival after travel finishes so a late inbox drain
    // can age contact against the original schedule, never its drain time.
    visual_destination: Option<((f64, f64), Instant)>,
    pending_contact: Option<ContactPresentation>,
    visual_deadline: Option<Instant>,
    presentation_now: Option<Instant>,
    /// Whether a button is currently being held for this cursor.
    pub pressed: bool,
    /// Semantic action and animation playback state.
    pub visual: CursorVisualState,
    /// Decoded installed or embedded theme.
    pub theme: Option<Arc<CompiledTheme>>,
    /// Conservative logical radius touched by any frame of the active theme.
    theme_paint_radius: f64,
    /// Non-fatal launch-time fallback reason, if an installed theme failed.
    pub theme_fallback: Option<String>,
    /// User-controlled visibility.
    pub visible: bool,
    /// Idle-hide: elapsed seconds since last activity.
    pub idle_secs: f64,
    /// Idle-hide fade: 1.0 = fully visible, 0.0 = fully hidden.
    pub idle_alpha: f64,
    /// Window id the overlay should be pinned above (for z-ordering).
    pub pinned_wid: Option<u64>,
    /// Sanitized caller-facing label painted below the cursor.
    pub session_label: Option<String>,
    /// Elapsed time since the session label was revealed with the cursor.
    pub session_badge_secs: f64,
    /// Whether the user's hardware pointer is currently over this synthetic
    /// cursor. Hover temporarily reveals an already-faded session badge
    /// without changing its one-shot reveal timer.
    pub session_badge_hovered: bool,
    /// Last action-scoped delivery and target context shown in the badge.
    /// This is latched briefly after the semantic action ends so the chips
    /// can fade without keeping modifier artwork inside the Lottie theme.
    pub badge_modifiers: Option<(Option<DeliveryModifier>, Option<TargetModifier>)>,
    /// Elapsed chip fade time after the active semantic action clears.
    pub badge_modifier_fade_secs: Option<f64>,
    badge_modifier_fade_started: Option<Instant>,
}

/// Logical bounds of the display containing a resolved target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl DisplayBounds {
    pub fn contains(self, target: (f64, f64)) -> bool {
        [
            self.x,
            self.y,
            self.width,
            self.height,
            self.x + self.width,
            self.y + self.height,
            target.0,
            target.1,
        ]
        .iter()
        .all(|v| v.is_finite())
            && self.width > 0.0
            && self.height > 0.0
            && target.0 >= self.x
            && target.0 < self.x + self.width
            && target.1 >= self.y
            && target.1 < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ContactPresentation {
    pub target: (f64, f64),
    /// Actual accepted-input time, independent of presentation.
    pub timestamp: Instant,
    pub presentation_timestamp: Instant,
    pub progress: f64,
    pub direction: Option<crate::ScrollDirection>,
}

impl RenderStateCore {
    /// Apply one accepted action using monotonic event time. A missing target
    /// never creates placement, and first placement requires current display bounds.
    pub fn apply_visual_event(
        &mut self,
        event: VisualEvent,
        display: Option<DisplayBounds>,
        now: Instant,
    ) -> bool {
        self.apply_visual_event_with_approach(event, display, now, false, false)
    }

    /// Opt-in click timing for adapters that gate input on target presentation.
    /// Other adapters and legacy MoveTo retain their existing timing.
    pub fn apply_click_approach(
        &mut self,
        event: VisualEvent,
        display: Option<DisplayBounds>,
        now: Instant,
    ) -> bool {
        self.apply_visual_event_with_approach(event, display, now, true, true)
    }

    /// Mac explicit-window navigation shares registration without click gating
    /// or changing navigation travel timing. Other adapters keep the legacy path.
    pub fn apply_registered_navigation(
        &mut self,
        event: VisualEvent,
        display: Option<DisplayBounds>,
        now: Instant,
    ) -> bool {
        let register = event.action == CursorAction::Navigate && event.window.is_some();
        self.apply_visual_event_with_approach(event, display, now, false, register)
    }

    /// Current artwork has a defensible visible-tip definition. Custom theme
    /// metadata cannot promise this, even when its ID resembles the default.
    pub fn target_registration_supported(&self) -> bool {
        self.target_painted_offset().is_some()
    }

    fn target_painted_offset(&self) -> Option<tiny_skia::Point> {
        if !self
            .theme
            .as_ref()
            .is_some_and(|theme| Arc::ptr_eq(theme, &crate::embedded_default_theme()))
        {
            return None;
        }
        crate::theme_artifact::default_painted_tip_offset(&self.visual, self.heading as f32)
    }

    // One calculation drives paint anchor and arrival. The existing logical tip
    // follows the path; the actual transformed outline is translated onto it.
    fn registered_geometry(&self) -> Option<((f64, f64), (f64, f64))> {
        if !self.registered_target {
            return None;
        }
        let offset = self.target_painted_offset()?;
        let desired = (
            self.pos.0 - self.heading.cos() * 16.0,
            self.pos.1 - self.heading.sin() * 16.0,
        );
        let anchor = (
            desired.0 - f64::from(offset.x),
            desired.1 - f64::from(offset.y),
        );
        Some((
            anchor,
            (
                anchor.0 + f64::from(offset.x),
                anchor.1 + f64::from(offset.y),
            ),
        ))
    }

    /// Concrete arrow-tip geometry for this exact action, never a timer-only ack.
    pub fn is_target_frame(&self, event: &VisualEvent) -> bool {
        let Some(target) = event.target else {
            return false;
        };
        let Some((_, tip)) = self.registered_geometry() else {
            return false;
        };
        self.cfg.enabled
            && self.visible
            && self.placed
            && self.idle_alpha > 0.0
            && self.path.is_none()
            && self.spring.is_none()
            && self
                .visual_owner
                .is_some_and(|(id, _, phase)| id == event.id && phase == VisualPhase::Intent)
            && self.pinned_wid == event.window
            && (tip.0 - target.0).abs() < 0.001
            && (tip.1 - target.1).abs() < 0.001
    }

    fn apply_visual_event_with_approach(
        &mut self,
        event: VisualEvent,
        display: Option<DisplayBounds>,
        now: Instant,
        quick_approach: bool,
        register_tip: bool,
    ) -> bool {
        if !event.is_valid() || !self.cfg.enabled || !self.visible {
            return false;
        }
        // Action allocation determines ownership across producers. Timestamp and
        // phase regressions only constrain events belonging to the same action.
        if let Some((id, timestamp, phase)) = self.visual_owner {
            if event.id.generation != id.generation
                || event.id.action < id.action
                || (event.id == id
                    && (event.timestamp < timestamp
                        || phase == VisualPhase::End
                        || (matches!(phase, VisualPhase::Contact | VisualPhase::Tracking)
                            && event.phase == VisualPhase::Intent)))
            {
                return false;
            }
        }
        if event.phase != VisualPhase::End {
            if let Some(target) = event.target {
                if display.is_some_and(|bounds| !bounds.contains(target)) {
                    return false;
                }
                if !self.placed {
                    let Some(bounds) = display else {
                        return false;
                    };
                    if !self.initialize_near_target(target, bounds) {
                        return false;
                    }
                }
            }
        }
        if self.visual_owner.is_none_or(|(id, _, _)| id != event.id) {
            self.clear_visual_presentation();
        }
        self.visual_owner = Some((event.id, event.timestamp, event.phase));
        if event.phase == VisualPhase::Intent
            || !matches!(event.action, CursorAction::Click | CursorAction::Navigate)
        {
            self.registered_target = register_tip;
        }
        if quick_approach {
            self.pinned_wid = event.window;
        } else if let Some(window) = event.window {
            self.pinned_wid = Some(window);
        }
        if event.phase == VisualPhase::End {
            self.registered_target = false;
            if self.visual_travel.take().is_some() {
                self.path = None;
            }
            self.visual_destination = None;
            self.pending_contact = None;
            self.contact = None;
            if self.delivery_active && self.focus_rect.is_some() {
                self.focus_rect_timestamp = Some(event.timestamp);
                self.focus_rect_t = 0.0;
            }
            self.delivery_active = false;
            self.advance_focus_rect(0.0, now);
            self.pressed = false;
            self.visual.to_idle();
            if self.badge_modifiers.is_some() {
                self.badge_modifier_fade_started = Some(event.timestamp);
                self.badge_modifier_fade_secs =
                    Some(now.saturating_duration_since(event.timestamp).as_secs_f64());
            }
            self.visual_deadline = None;
            self.advance_visual_presentation(now);
            return true;
        }
        self.delivery_active = matches!(event.action, CursorAction::Text | CursorAction::Drag)
            && event.phase == VisualPhase::Tracking;
        if let Some(bounds) = event
            .bounds
            .filter(|rect| rect.iter().all(|v| v.is_finite()) && rect[2] > 0.0 && rect[3] > 0.0)
        {
            self.focus_rect = Some(bounds);
            self.focus_rect_t = 0.0;
            self.focus_rect_timestamp = Some(event.timestamp);
        } else {
            self.focus_rect = None;
            self.focus_rect_timestamp = None;
            self.focus_rect_t = 1.0;
        }
        self.advance_focus_rect(0.0, now);
        if event.action == CursorAction::Drag && event.phase == VisualPhase::Tracking {
            self.pressed = true;
        }
        let reveal_badge = !self.cursor_is_revealed();
        self.idle_secs = 0.0;
        self.idle_alpha = 1.0;
        if reveal_badge {
            self.reveal_session_badge();
        }
        let mut deadline = event.timestamp + Duration::from_millis(150);
        if let Some(target) = event.target {
            let OverlayCommand::SnapTo {
                x,
                y,
                heading_radians: Some(heading),
            } = crate::track_pointer_command(target.0, target.1)
            else {
                unreachable!()
            };
            // Only a click receipt at the same resolved destination follows the
            // intent's schedule. Wheel receipts and native tracking stay immediate.
            let arrival = self
                .visual_destination
                .filter(|(point, _)| *point == target);
            let follow_travel = event.phase == VisualPhase::Contact
                && event.action == CursorAction::Click
                && arrival.is_some();
            self.spring = None;
            self.spring_tgt = None;
            self.click_t = None;
            if !follow_travel {
                self.path = None;
                self.dist = 0.0;
                self.visual_travel = None;
                self.visual_destination = None;
                self.pending_contact = None;
                let tip_distance = (self.pos.0 - self.heading.cos() * 16.0 - target.0)
                    .hypot(self.pos.1 - self.heading.sin() * 16.0 - target.1);
                if event.phase == VisualPhase::Intent
                    && !(quick_approach && tip_distance < 0.001)
                    && self.visual.reduced_motion != crate::ReducedMotion::On
                    && (x - self.pos.0).hypot(y - self.pos.1) > 0.001
                {
                    let distance = (x - self.pos.0).hypot(y - self.pos.1);
                    let duration = Duration::from_secs_f64(if quick_approach {
                        (distance / 1600.0).clamp(0.080, 0.140)
                    } else {
                        (distance / 900.0).clamp(0.120, 0.220)
                    });
                    self.path = Some(PathPlanner::plan(
                        self.pos.0,
                        self.pos.1,
                        self.heading + std::f64::consts::PI,
                        x,
                        y,
                        heading + std::f64::consts::PI,
                        heading,
                        self.motion.turn_radius,
                    ));
                    // A promptly drained intent starts visible travel now; an
                    // older intent ages its original schedule. The adapter owns
                    // the separate presentation-confirmation deadline.
                    let start = if quick_approach
                        && now.saturating_duration_since(event.timestamp)
                            < Duration::from_millis(250)
                    {
                        now
                    } else {
                        event.timestamp
                    };
                    self.visual_travel = Some((start, duration));
                    deadline = start + duration;
                    self.visual_destination = Some((target, deadline));
                } else {
                    self.pos = (x, y);
                    self.heading = heading;
                }
            }
            if event.phase == VisualPhase::Contact {
                let presentation_timestamp = if follow_travel {
                    arrival.unwrap().1.max(event.timestamp)
                } else {
                    event.timestamp
                };
                self.contact = None;
                self.pending_contact = Some(ContactPresentation {
                    target,
                    timestamp: event.timestamp,
                    presentation_timestamp,
                    progress: 0.0,
                    direction: event.scroll_direction,
                });
                deadline = presentation_timestamp + Duration::from_millis(150);
            }
        }
        let (delivery, target) = event.modifiers.unwrap_or((None, None));
        self.visual.begin(event.action, delivery, target);
        self.badge_modifiers = event
            .modifiers
            .filter(|(delivery, target)| delivery.is_some() || target.is_some());
        self.badge_modifier_fade_secs = None;
        self.badge_modifier_fade_started = None;
        self.visual_deadline = (!self.delivery_active).then_some(deadline);
        self.advance_visual_presentation(now);
        true
    }

    /// Advance timed action effects before painting. Both tick paths use this
    /// clock; accumulated frame deltas never stretch action display durations.
    pub fn advance_visual_presentation(&mut self, now: Instant) -> bool {
        let now = self.presentation_now.map_or(now, |last| now.max(last));
        self.presentation_now = Some(now);
        self.advance_focus_rect(0.0, now);
        let mut arrived = false;
        if self.visual.reduced_motion == crate::ReducedMotion::On {
            if let Some((_, arrival)) = &mut self.visual_destination {
                *arrival = (*arrival).min(now);
            }
            if let Some(contact) = &mut self.pending_contact {
                contact.presentation_timestamp = contact.presentation_timestamp.min(now);
                self.visual_deadline =
                    Some(contact.presentation_timestamp + Duration::from_millis(150));
            }
        }
        if let Some((started, duration)) = self.visual_travel {
            let fraction = if self.visual.reduced_motion == crate::ReducedMotion::On {
                1.0
            } else {
                (now.saturating_duration_since(started).as_secs_f64() / duration.as_secs_f64())
                    .clamp(0.0, 1.0)
            };
            if let Some(path) = &self.path {
                // Smooth display interpolation, independent of the user's legacy timing.
                let eased = fraction * fraction * (3.0 - 2.0 * fraction);
                self.dist = path.length * eased;
                let sample = path.sample(self.dist);
                self.pos = (sample.x, sample.y);
                self.heading = if fraction >= 1.0 {
                    path.end_visual_heading
                } else {
                    sample.heading + std::f64::consts::PI
                };
            }
            if fraction >= 1.0 {
                self.path = None;
                self.visual_travel = None;
                self.spring = None;
                self.spring_tgt = None;
                arrived = true;
            }
        }
        if self
            .pending_contact
            .is_some_and(|contact| now >= contact.presentation_timestamp)
        {
            self.contact = self.pending_contact.take();
        }
        if let Some(contact) = &mut self.contact {
            contact.progress = now
                .saturating_duration_since(contact.presentation_timestamp)
                .as_secs_f64()
                / 0.150;
            if contact.progress >= 1.0 {
                self.contact = None;
            }
        }
        if let Some(deadline) = self.visual_deadline {
            if now >= deadline {
                self.visual.to_idle();
                if self.badge_modifiers.is_some() {
                    self.badge_modifier_fade_started = Some(deadline);
                    self.badge_modifier_fade_secs =
                        Some(now.saturating_duration_since(deadline).as_secs_f64());
                }
                self.visual_deadline = None;
            } else if let Some((_, timestamp, _)) = self.visual_owner {
                self.visual.elapsed_secs = now.saturating_duration_since(timestamp).as_secs_f64();
            }
        }
        if let Some(started) = self.badge_modifier_fade_started {
            self.badge_modifier_fade_secs =
                Some(now.saturating_duration_since(started).as_secs_f64());
        }
        if self
            .badge_modifier_fade_secs
            .is_some_and(|elapsed| elapsed >= SESSION_BADGE_FADE_SECS)
        {
            self.badge_modifiers = None;
            self.badge_modifier_fade_secs = None;
            self.badge_modifier_fade_started = None;
        }
        arrived
    }

    pub fn has_timed_presentation(&self) -> bool {
        self.visual_deadline.is_some() || self.badge_modifier_fade_started.is_some()
    }

    /// Clear action effects when placement or visibility is invalidated.
    /// Keep the ownership watermark so late events cannot rewind this instance.
    pub fn clear_visual_presentation(&mut self) {
        self.registered_target = false;
        self.badge_modifiers = None;
        self.badge_modifier_fade_secs = None;
        self.badge_modifier_fade_started = None;
        if self.visual_travel.take().is_some() {
            self.path = None;
        }
        self.contact = None;
        self.pending_contact = None;
        self.visual_destination = None;
        self.focus_rect = None;
        self.focus_rect_t = 1.0;
        self.focus_rect_timestamp = None;
        let was_delivering = std::mem::take(&mut self.delivery_active);
        self.pressed = false;
        if self.visual_deadline.take().is_some() || was_delivering {
            self.visual.to_idle();
        }
    }

    fn advance_focus_rect(&mut self, dt: f64, now: Instant) {
        if self.delivery_active {
            return;
        }
        if self.focus_rect.is_some() {
            let progress = self.focus_rect_timestamp.map_or_else(
                || self.focus_rect_t + dt / 0.6,
                |timestamp| {
                    (now.saturating_duration_since(timestamp).as_secs_f64() / 0.6)
                        .max(self.focus_rect_t)
                },
            );
            self.focus_rect_t = progress.min(1.0);
            if self.focus_rect_t >= 1.0 {
                self.focus_rect = None;
                self.focus_rect_timestamp = None;
            }
        }
    }

    /// Initialize a first move near its resolved target on the containing display.
    pub fn initialize_near_target(&mut self, target: (f64, f64), display: DisplayBounds) -> bool {
        if self.placed || !self.cfg.enabled || !self.visible || !display.contains(target) {
            return false;
        }
        // Adapted from PR3019: keep the seed within 140 units on each axis.
        // Cap the inset at a quarter dimension so tiny displays remain valid.
        let inset_x = 2.0_f64.min(display.width / 4.0);
        let inset_y = 2.0_f64.min(display.height / 4.0);
        let clamp = |x: f64, y: f64| {
            (
                x.clamp(display.x + inset_x, display.x + display.width - inset_x),
                y.clamp(display.y + inset_y, display.y + display.height - inset_y),
            )
        };
        let mut seed = clamp(target.0 - 140.0, target.1 - 140.0);
        if (seed.0 - target.0).abs() < 8.0 && (seed.1 - target.1).abs() < 8.0 {
            seed = clamp(target.0 + 140.0, target.1 + 140.0);
        }
        self.pos = seed;
        self.placed = true;
        true
    }

    /// Build the core from a launch-time CursorConfig.
    pub fn new(cfg: CursorConfig) -> Self {
        let motion = cfg.motion.clone();
        let visual = CursorVisualState {
            reduced_motion: cfg.reduced_motion,
            ..CursorVisualState::default()
        };
        let (theme, theme_fallback) = match crate::load_installed_theme(&cfg.theme_id) {
            Ok(theme) => (theme, None),
            Err(error) => (
                Some(crate::embedded_default_theme()),
                Some(format!(
                    "theme `{}` could not be loaded; using {}: {error}",
                    cfg.theme_id,
                    crate::DEFAULT_THEME_ID
                )),
            ),
        };
        let theme_paint_radius = theme.as_deref().map_or(64.0, CompiledTheme::paint_radius);
        Self {
            cfg,
            motion,
            visual,
            theme,
            theme_paint_radius,
            theme_fallback,
            pos: (0.0, 0.0),
            placed: false,
            heading: std::f64::consts::FRAC_PI_4,
            path: None,
            dist: 0.0,
            spring: None,
            spring_tgt: None,
            click_t: None,
            contact: None,
            focus_rect: None,
            focus_rect_t: 1.0,
            focus_rect_timestamp: None,
            delivery_active: false,
            visual_owner: None,
            registered_target: false,
            visual_travel: None,
            visual_destination: None,
            pending_contact: None,
            visual_deadline: None,
            presentation_now: None,
            pressed: false,
            visible: true,
            idle_secs: 0.0,
            idle_alpha: 1.0,
            pinned_wid: None,
            session_label: None,
            session_badge_secs: SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS,
            session_badge_hovered: false,
            badge_modifiers: None,
            badge_modifier_fade_secs: None,
            badge_modifier_fade_started: None,
        }
    }

    pub fn paint_radius(&self) -> f64 {
        // Cover host-owned bloom and click-pulse effects as well as theme art.
        let translation = self.registered_geometry().map_or(0.0, |(anchor, _)| {
            (anchor.0 - self.pos.0).hypot(anchor.1 - self.pos.1)
        });
        self.theme_paint_radius.max(64.0) + translation
    }

    pub fn cursor_is_revealed(&self) -> bool {
        self.cfg.enabled && self.visible && self.placed && self.idle_alpha >= 0.004
    }

    fn reveal_session_badge(&mut self) {
        if self.session_label.is_some() {
            self.session_badge_secs = 0.0;
        }
    }

    pub fn session_badge_alpha(&self) -> f32 {
        if self.session_label.is_none() {
            return 0.0;
        }
        if self.session_badge_hovered {
            return 1.0;
        }
        if self.session_badge_secs <= SESSION_BADGE_HOLD_SECS {
            return 1.0;
        }
        let fade = ((self.session_badge_secs - SESSION_BADGE_HOLD_SECS) / SESSION_BADGE_FADE_SECS)
            .clamp(0.0, 1.0);
        let smooth = fade * fade * (3.0 - 2.0 * fade);
        (1.0 - smooth) as f32
    }

    pub fn session_badge_chip_alpha(&self) -> f32 {
        if self.badge_modifiers.is_none() {
            return 0.0;
        }
        let Some(elapsed) = self.badge_modifier_fade_secs else {
            return 1.0;
        };
        let fade = (elapsed / SESSION_BADGE_FADE_SECS).clamp(0.0, 1.0);
        let smooth = fade * fade * (3.0 - 2.0 * fade);
        (1.0 - smooth) as f32
    }

    pub fn session_badge_is_visible(&self) -> bool {
        self.cursor_is_revealed()
            && (self.session_badge_alpha() > 0.001 || self.session_badge_chip_alpha() > 0.001)
    }

    pub fn session_badge_needs_frame_tick(&self) -> bool {
        self.cursor_is_revealed()
            && ((self.session_label.is_some()
                && self.session_badge_secs < SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS)
                || self.badge_modifier_fade_secs.is_some()
                || self.visual.resolved_action != CursorAction::Idle)
    }

    /// Whether the platform overlay should keep a low-frequency hardware
    /// pointer poll alive for hover-to-reveal. This is deliberately separate
    /// from [`Self::session_badge_needs_frame_tick`]: a faded badge needs hover
    /// hit-testing, not continuous 60 fps repainting.
    pub fn session_badge_needs_hover_poll(&self) -> bool {
        self.session_label.is_some() && self.cursor_is_revealed()
    }

    /// Update hover state from a platform-native hardware pointer sample.
    ///
    /// `self.pos` is the centre of the cursor artwork. The hit radius is a
    /// little larger than the 42 point production artwork so the interaction
    /// remains comfortable around the white outline and glow.
    pub fn update_session_badge_hover(&mut self, pointer: Option<(f64, f64)>) -> bool {
        const HOVER_RADIUS: f64 = crate::theme::DISPLAY_SIZE as f64 * 0.82;
        let hovered = self.session_badge_needs_hover_poll()
            && pointer.is_some_and(|(x, y)| {
                let dx = x - self.pos.0;
                let dy = y - self.pos.1;
                if dx * dx + dy * dy <= HOVER_RADIUS * HOVER_RADIUS {
                    return true;
                }
                crate::session_badge_layout(crate::SessionBadgeInput {
                    label: self.session_label.as_deref(),
                    delivery: self.badge_modifiers.and_then(|modifiers| modifiers.0),
                    target: self.badge_modifiers.and_then(|modifiers| modifiers.1),
                    cursor: (self.pos.0 as f32, self.pos.1 as f32),
                    backing_scale: 1.0,
                    label_alpha: self.session_badge_alpha(),
                    chip_alpha: self.session_badge_chip_alpha(),
                    clip: None,
                })
                .is_some_and(|layout| {
                    let rect = layout.rect;
                    x >= rect.x() as f64
                        && x <= (rect.x() + rect.width()) as f64
                        && y >= rect.y() as f64
                        && y <= (rect.y() + rect.height()) as f64
                })
            });
        let changed = hovered != self.session_badge_hovered;
        self.session_badge_hovered = hovered;
        changed
    }

    /// Return the theme that is actually being painted, including any
    /// non-fatal fallback from an unavailable launch-time selection.
    pub fn active_theme_metadata(&self) -> (String, String, String, Option<String>) {
        match self.theme.as_deref() {
            Some(theme) => (
                theme.id.clone(),
                theme.version.clone(),
                theme.profile.clone(),
                self.theme_fallback.clone(),
            ),
            None => (
                crate::DEFAULT_THEME_ID.into(),
                crate::DEFAULT_THEME_VERSION.into(),
                crate::THEME_PROFILE.into(),
                self.theme_fallback.clone(),
            ),
        }
    }

    /// Advance the animation by `dt` seconds using runtime [`MotionConfig`]
    /// for peak / floor / spring constants. Used by Windows + Linux.
    ///
    /// The speed profile is `16·u²·(1-u)²` (peaks at 1.0 at u=0.5) — the
    /// 1:1 port of `AgentCursorRenderer`'s smootherstep envelope. Floor
    /// speed switches from `min_start_speed` to `min_end_speed` at the
    /// midpoint so the cursor decelerates as it approaches the target.
    /// Spring overshoot is `0.5` (Windows/Linux convention).
    ///
    /// Returns `true` when the planned path just ended (so the caller can
    /// fire an arrival oneshot to unblock `animate_cursor_to`).
    pub fn tick_motion(&mut self, dt: f64) -> bool {
        self.tick_motion_at(dt, Instant::now())
    }

    pub fn tick_motion_at(&mut self, dt: f64, now: Instant) -> bool {
        let timed_travel = self.visual_travel.is_some();
        let arrived = self.advance_visual_presentation(now);
        if timed_travel {
            self.tick_idle(dt);
            return arrived;
        }
        let spring_k = self.motion.spring * 400.0;
        let spring_c = self.motion.spring * 20.0;

        let mut fire_arrival = false;

        if let Some(ref p) = self.path {
            let path_len = p.length.max(1.0);
            let path_frac = (self.dist / path_len).clamp(0.0, 1.0);
            let profile = 16.0 * path_frac * path_frac * (1.0 - path_frac) * (1.0 - path_frac);
            let floor = if path_frac < 0.5 {
                self.motion.min_start_speed
            } else {
                self.motion.min_end_speed
            };
            let speed_based = (floor + (self.motion.peak_speed - floor) * profile).max(floor);
            // Fixed-duration override: when `glide_duration_ms > 0` the move
            // takes exactly that long regardless of distance, so an orchestrator
            // can lock glides to a known cadence. `0` (the default) keeps the
            // speed-based timing untouched. Shared verbatim with the macOS
            // reference path (`tick_swift_constants`) — no platform drift.
            let speed = if self.motion.glide_duration_ms > 0.0 {
                path_len / (self.motion.glide_duration_ms / 1000.0)
            } else {
                speed_based
            };
            self.dist += speed * dt;

            if self.dist >= path_len {
                let end = p.sample(path_len);
                let end_heading = p.end_visual_heading;
                let vh = end.heading;
                // In fixed-duration mode the constant speed can be large; base
                // the settle impulse on the normal end-floor so the landing
                // stays as crisp as a speed-based glide instead of overshooting
                // proportionally to a short duration.
                let impulse = if self.motion.glide_duration_ms > 0.0 {
                    self.motion.min_end_speed
                } else {
                    speed
                };
                self.spring = Some(Spring {
                    ox: 0.0,
                    oy: 0.0,
                    vx: impulse * 0.5 * vh.cos(),
                    vy: impulse * 0.5 * vh.sin(),
                });
                self.spring_tgt = Some((end.x, end.y, end_heading));
                self.pos = (end.x, end.y);
                self.heading = end_heading;
                self.path = None;
                self.dist = 0.0;
                fire_arrival = true;
            } else {
                let s: PathState = p.sample(self.dist);
                self.pos = (s.x, s.y);
                // Point the arrow exactly along the path tangent (the renderer
                // adds π, so we store tangent+π). Assigned directly rather than
                // rate-limited toward it, so the tip actually tracks the
                // trajectory instead of lagging behind on fast/short glides.
                self.heading = s.heading + std::f64::consts::PI;
            }
        } else if let Some(mut s) = self.spring {
            if let Some((tx, ty, th)) = self.spring_tgt {
                let substeps = 4;
                let sdt = dt / substeps as f64;
                for _ in 0..substeps {
                    s.vx += (-spring_k * s.ox - spring_c * s.vx) * sdt;
                    s.vy += (-spring_k * s.oy - spring_c * s.vy) * sdt;
                    s.ox += s.vx * sdt;
                    s.oy += s.vy * sdt;
                }
                self.pos = (tx + s.ox, ty + s.oy);
                self.heading = th;
                if s.ox.hypot(s.oy) < 0.3 && s.vx.hypot(s.vy) < 2.0 {
                    self.pos = (tx, ty);
                    self.spring = None;
                } else {
                    self.spring = Some(s);
                }
            }
        }

        if let Some(t) = self.click_t {
            let next = t + dt * 4.0;
            self.click_t = if next >= 1.0 { None } else { Some(next) };
        }

        self.tick_idle(dt);

        fire_arrival
    }

    /// Advance the animation by `dt` seconds using the hardcoded Swift
    /// reference constants (`peakSpeed=900`, `minStart=300`, `minEnd=200`,
    /// `springK=400`, `springC=17`, `springOvershoot=0.8`).  Used by macOS,
    /// which mirrors `AgentCursorRenderer.swift` 1:1.
    ///
    /// Returns `true` when the path just ended (so the caller can fire its
    /// arrival oneshot to unblock `animate_cursor_to`).
    ///
    /// The speed profile is `(30·u²·(1-u)²) / 1.875` which is algebraically
    /// equivalent to the `16·u²·(1-u)²` form used by [`tick_motion`]; both
    /// peak at 1.0 at u=0.5.  The original Swift code uses the 30/1.875
    /// form so we preserve it here for parity.
    pub fn tick_swift_constants(&mut self, dt: f64) -> bool {
        self.tick_swift_constants_at(dt, Instant::now())
    }

    pub fn tick_swift_constants_at(&mut self, dt: f64, now: Instant) -> bool {
        let timed_travel = self.visual_travel.is_some();
        let arrived = self.advance_visual_presentation(now);
        if timed_travel {
            self.tick_idle(dt);
            return arrived;
        }
        const PEAK_SPEED: f64 = 900.0;
        const MIN_START_SPEED: f64 = 300.0;
        const MIN_END_SPEED: f64 = 200.0;
        const SPRING_K: f64 = 400.0;
        const SPRING_C: f64 = 17.0;
        const SPRING_OVERSHOOT: f64 = 0.8;

        let mut fire_arrival = false;

        if let Some(ref p) = self.path {
            let path_len = p.length.max(1.0);
            let u = (self.dist / path_len).min(1.0);

            // Smootherstep speed profile (normalised: peak = 1.0).
            let profile = (30.0 * u * u * (1.0 - u) * (1.0 - u)) / 1.875;
            let floor_speed = if u < 0.5 {
                MIN_START_SPEED
            } else {
                MIN_END_SPEED
            };
            let speed_based = floor_speed + (PEAK_SPEED - floor_speed) * profile;
            // Fixed-duration override: when `glide_duration_ms > 0` the move
            // takes exactly that long regardless of distance, so an orchestrator
            // can lock glides to a known cadence. `0` (the default) keeps the
            // speed-based timing untouched. Shared verbatim with the
            // Windows/Linux path (`tick_motion`) — no platform drift.
            let current_speed = if self.motion.glide_duration_ms > 0.0 {
                path_len / (self.motion.glide_duration_ms / 1000.0)
            } else {
                speed_based
            };
            self.dist += current_speed * dt;

            if self.dist >= path_len {
                // Transition to spring settle.
                let end = p.sample(path_len);
                let end_heading = p.end_visual_heading;
                let vh = end.heading;
                // In fixed-duration mode the constant speed can be large; base
                // the settle impulse on the normal end-floor so the landing
                // stays as crisp as a speed-based glide instead of overshooting
                // proportionally to a short duration.
                let impulse = if self.motion.glide_duration_ms > 0.0 {
                    MIN_END_SPEED
                } else {
                    current_speed
                };
                self.spring = Some(Spring {
                    ox: 0.0,
                    oy: 0.0,
                    vx: impulse * SPRING_OVERSHOOT * vh.cos(),
                    vy: impulse * SPRING_OVERSHOOT * vh.sin(),
                });
                self.spring_tgt = Some((end.x, end.y, end_heading));
                self.pos = (end.x, end.y);
                self.heading = end_heading;
                self.path = None;
                self.dist = 0.0;
                fire_arrival = true;
            } else {
                let s: PathState = p.sample(self.dist);
                self.pos = (s.x, s.y);
                // Point the arrow exactly along the path tangent (renderer adds
                // π, so store tangent+π). Direct assignment, not rate-limited, so
                // the tip tracks the trajectory instead of lagging on fast moves.
                self.heading = s.heading + std::f64::consts::PI;
            }
        } else if let Some(mut s) = self.spring {
            if let Some((tx, ty, th)) = self.spring_tgt {
                let substeps = 4;
                let sdt = dt / substeps as f64;
                for _ in 0..substeps {
                    s.vx += (-SPRING_K * s.ox - SPRING_C * s.vx) * sdt;
                    s.vy += (-SPRING_K * s.oy - SPRING_C * s.vy) * sdt;
                    s.ox += s.vx * sdt;
                    s.oy += s.vy * sdt;
                }
                self.pos = (tx + s.ox, ty + s.oy);
                self.heading = th;
                if s.ox.hypot(s.oy) < 0.3 && s.vx.hypot(s.vy) < 2.0 {
                    self.pos = (tx, ty);
                    self.spring = None;
                } else {
                    self.spring = Some(s);
                }
            }
        }

        // Advance click pulse.
        if let Some(t) = self.click_t {
            let next = t + dt * 4.0; // full pulse over 0.25s
            self.click_t = if next >= 1.0 { None } else { Some(next) };
        }

        self.tick_idle(dt);

        fire_arrival
    }

    /// Shared idle-hide / fade logic — accumulate idle time when nothing is
    /// moving, then fade `idle_alpha` from 1→0 over 180ms once
    /// `motion.idle_hide_ms` has elapsed.  Identical across all platforms.
    fn tick_idle(&mut self, dt: f64) {
        self.advance_focus_rect(dt, self.presentation_now.unwrap_or_else(Instant::now));
        let modifiers_before_tick = (self.visual.delivery, self.visual.target);
        if self.visual_deadline.is_none() && !self.delivery_active {
            self.visual.tick(dt);
        }
        let modifiers_after_tick = (self.visual.delivery, self.visual.target);
        if modifiers_after_tick.0.is_some() || modifiers_after_tick.1.is_some() {
            self.badge_modifiers = Some(modifiers_after_tick);
            self.badge_modifier_fade_secs = None;
        } else if (modifiers_before_tick.0.is_some() || modifiers_before_tick.1.is_some())
            && self.badge_modifiers.is_some()
            && self.badge_modifier_fade_secs.is_none()
        {
            self.badge_modifier_fade_secs = Some(0.0);
        }
        if let Some(elapsed) = self.badge_modifier_fade_secs {
            let next = if self.badge_modifier_fade_started.is_some() {
                elapsed
            } else {
                elapsed + dt.max(0.0)
            };
            if next >= SESSION_BADGE_FADE_SECS {
                self.badge_modifiers = None;
                self.badge_modifier_fade_secs = None;
            } else {
                self.badge_modifier_fade_secs = Some(next);
            }
        }
        if self.session_label.is_some() {
            self.session_badge_secs = (self.session_badge_secs + dt)
                .min(SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS);
        }
        let idle_hide_ms = self.motion.idle_hide_ms;
        if idle_hide_ms > 0.0 {
            let moving = self.path.is_some()
                || self.spring.is_some()
                || self.click_t.is_some()
                || self.contact.is_some()
                || self.delivery_active;
            if moving {
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
            } else {
                self.idle_secs += dt;
                let fade_start = idle_hide_ms / 1000.0;
                let fade_end = fade_start + 0.18; // 180ms fade like Windows ref
                if self.idle_secs > fade_end {
                    self.idle_alpha = 0.0;
                } else if self.idle_secs > fade_start {
                    let t = (self.idle_secs - fade_start) / 0.18;
                    self.idle_alpha = 1.0 - t.clamp(0.0, 1.0);
                }
            }
        } else {
            self.idle_alpha = 1.0;
        }
    }

    /// Handle the OverlayCommand variants that are identical across all
    /// three platforms.  Returns `true` if the command was consumed; `false`
    /// for variants the platform must handle itself (e.g. macOS's
    /// `ShowFocusRect`).
    ///
    /// The placement flags preserve each platform's existing first-move and
    /// click behavior without reserving any coordinate as a sentinel.
    pub fn apply_command_base(
        &mut self,
        cmd: OverlayCommand,
        move_to_places_unplaced: bool,
        click_pulse_unplaced_only: bool,
    ) -> bool {
        if matches!(
            &cmd,
            OverlayCommand::MoveTo { .. }
                | OverlayCommand::SnapTo { .. }
                | OverlayCommand::ClickPulse { .. }
                | OverlayCommand::SetEnabled(false)
        ) {
            self.clear_visual_presentation();
            // Legacy BeginAction context is still active when replacing legacy
            // movement. Invalidation must clear chips even in that case.
            if !matches!(&cmd, OverlayCommand::SetEnabled(false))
                && (self.visual.delivery.is_some() || self.visual.target.is_some())
            {
                self.badge_modifiers = Some((self.visual.delivery, self.visual.target));
            }
        }
        match cmd {
            OverlayCommand::MoveTo {
                x,
                y,
                end_heading_radians,
            } => {
                let reveal_badge = !self.cursor_is_revealed();
                // Apply click offset (16 pt along end_heading) before planning,
                // matching Swift `moveTo(point:endAngleRadians:)`:
                //   tx = clickPoint.x + cos(endAngle) * clickOffset
                //   ty = clickPoint.y + sin(endAngle) * clickOffset
                const CLICK_OFFSET: f64 = 16.0;
                let turn_radius = self.motion.turn_radius;
                let tx = x + end_heading_radians.cos() * CLICK_OFFSET;
                let ty = y + end_heading_radians.sin() * CLICK_OFFSET;

                if move_to_places_unplaced && !self.placed {
                    self.pos = (tx, ty);
                }
                let (x0, y0) = self.pos;
                self.placed = true;
                let th0 = self.heading + std::f64::consts::PI;
                let th1 = end_heading_radians + std::f64::consts::PI;
                let plan =
                    PathPlanner::plan(x0, y0, th0, tx, ty, th1, end_heading_radians, turn_radius);
                self.path = Some(plan);
                self.dist = 0.0;
                self.spring = None;
                self.spring_tgt = None;
                if matches!(
                    self.visual.resolved_action,
                    CursorAction::Idle | CursorAction::Navigate
                ) {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Navigate, delivery, target);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                if reveal_badge {
                    self.reveal_session_badge();
                }
                true
            }
            OverlayCommand::SnapTo {
                x,
                y,
                heading_radians,
            } => {
                let reveal_badge = !self.cursor_is_revealed();
                self.pos = (x, y);
                self.placed = true;
                if let Some(heading) = heading_radians {
                    self.heading = heading;
                }
                self.path = None;
                self.dist = 0.0;
                self.spring = None;
                self.spring_tgt = None;
                if matches!(
                    self.visual.resolved_action,
                    CursorAction::Idle | CursorAction::Navigate
                ) {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Navigate, delivery, target);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                if reveal_badge {
                    self.reveal_session_badge();
                }
                true
            }
            OverlayCommand::ClickPulse { x, y } => {
                let reveal_badge = !self.cursor_is_revealed();
                if !click_pulse_unplaced_only || !self.placed {
                    self.pos = if click_pulse_unplaced_only {
                        const CLICK_OFFSET: f64 = 16.0;
                        let angle = std::f64::consts::FRAC_PI_4;
                        (
                            x + angle.cos() * CLICK_OFFSET,
                            y + angle.sin() * CLICK_OFFSET,
                        )
                    } else {
                        (x, y)
                    };
                }
                self.placed = true;
                self.click_t = Some(0.0);
                if matches!(
                    self.visual.resolved_action,
                    CursorAction::Idle | CursorAction::Navigate | CursorAction::Click
                ) {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Click, delivery, target);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                if reveal_badge {
                    self.reveal_session_badge();
                }
                true
            }
            OverlayCommand::SetPressed(v) => {
                self.pressed = v;
                if v {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Drag, delivery, target);
                } else {
                    self.visual.end(CursorAction::Drag);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                true
            }
            OverlayCommand::SetEnabled(v) => {
                let reveal_badge = v && !self.visible;
                self.visible = v;
                if reveal_badge {
                    self.reveal_session_badge();
                }
                true
            }
            OverlayCommand::SetMotion(m) => {
                self.motion = m;
                true
            }
            OverlayCommand::PinAbove(wid) => {
                self.pinned_wid = Some(wid);
                true
            }
            OverlayCommand::BeginAction {
                action,
                delivery,
                target,
            } => {
                self.registered_target = false;
                self.visual.begin(action, delivery, target);
                self.badge_modifiers = if delivery.is_some() || target.is_some() {
                    Some((delivery, target))
                } else {
                    None
                };
                self.badge_modifier_fade_secs = None;
                self.badge_modifier_fade_started = None;
                true
            }
            OverlayCommand::EndAction(action) => {
                self.visual.end(action);
                true
            }
            OverlayCommand::SetTheme {
                theme_id,
                reduced_motion,
            } => {
                match crate::resolve_theme_selection(&theme_id) {
                    Ok(theme) => {
                        self.registered_target = false;
                        self.theme_paint_radius =
                            theme.as_deref().map_or(64.0, CompiledTheme::paint_radius);
                        self.theme = theme;
                        self.theme_fallback = None;
                        self.cfg.theme_id = theme_id;
                        self.cfg.reduced_motion = reduced_motion;
                        self.visual.reduced_motion = reduced_motion;
                    }
                    Err(error) => {
                        tracing::warn!(
                            theme_id,
                            error = %error,
                            "keeping the active cursor theme after selection failed"
                        );
                    }
                }
                true
            }
            OverlayCommand::SetSessionLabel(label) => {
                let session_label = crate::sanitize_session_label(&label);
                if session_label != self.session_label {
                    self.session_label = session_label;
                    self.session_badge_secs = 0.0;
                }
                true
            }
            OverlayCommand::ShowFocusRect(rect) => {
                self.focus_rect = rect;
                self.focus_rect_t = 0.0;
                self.focus_rect_timestamp = None;
                self.delivery_active = false;
                true
            }
        }
    }
}

// ── tiny-skia rendering ──────────────────────────────────────────────────

/// Optional focus-rect overlay drawn on top of the cursor (macOS only at
/// the moment — the other platforms always pass `None`).
#[derive(Clone, Copy)]
pub struct FocusRect {
    /// Rectangle `[x, y, w, h]` in screen coordinates (top-left origin),
    /// relative to the same origin the cursor `pos` uses.
    pub rect: [f64; 4],
    /// Fade progress 0.0 = fully visible, 1.0 = gone.
    pub t: f64,
}

/// Render the cursor + bloom + click-pulse + (optional) focus-rect into a
/// fresh tiny-skia [`tiny_skia::Pixmap`] of `(width, height)`.
///
/// `origin_x`, `origin_y` are subtracted from the cursor `core.pos` before
/// drawing. macOS passes each display's global origin; other adapters choose
/// their viewport origin.
///
/// `backing_scale` is the destination-pixmap-pixels per logical-point ratio
/// (e.g. 2.0 on a retina display where the pixmap is sized at physical
/// pixels). Pass `1.0` when the pixmap is sized at logical pixels.
pub fn render_frame(
    core: &RenderStateCore,
    width: u32,
    height: u32,
    origin_x: f64,
    origin_y: f64,
    focus_rect: Option<FocusRect>,
    backing_scale: f32,
) -> tiny_skia::Pixmap {
    let w = width.max(1);
    let h = height.max(1);
    let mut pm =
        tiny_skia::Pixmap::new(w, h).unwrap_or_else(|| tiny_skia::Pixmap::new(1, 1).unwrap());
    paint_cursor(&mut pm, core, origin_x, origin_y, focus_rect, backing_scale);
    pm
}

/// Paint a single cursor (bloom + click-pulse + optional focus-rect + arrow)
/// into a caller-owned [`tiny_skia::Pixmap`]. tiny-skia's `fill_*` / `stroke_*`
/// are alpha-over, so painting several cursors into the same pixmap composites
/// them with later calls drawn on top — this is what lets the macOS overlay
/// render N owned cursors into one buffer / one NSWindow.
///
/// `origin_x` / `origin_y` are subtracted from `core.pos` before drawing
/// (macOS passes the per-display origin; other adapters choose their viewport).
/// Both are in **logical** screen points, just like `core.pos`.
///
/// `backing_scale` is the destination-pixmap-pixels per logical-point ratio.
/// On a 2× retina macOS display the caller sizes the pixmap at the screen's
/// PHYSICAL pixel dimensions (logical × backing_scale) and passes `2.0` so
/// the cursor renders at native resolution instead of being upsampled by
/// Core Animation. When the pixmap is sized at LOGICAL pixels, pass `1.0`.
///
/// Everything that operates in pixmap-pixel space (the cursor anchor `px/py`,
/// bloom radius, click-pulse ring radius, stroke widths, focus-rect coords,
/// arrow `display_size`) is multiplied by `backing_scale` so the cursor still
/// occupies the same on-screen logical footprint but at higher pixel fidelity.
///
/// Quiescent / hidden cursors early-return before touching the pixmap, so an
/// idle session costs essentially nothing in the per-frame composite loop.
pub fn paint_cursor(
    pm: &mut tiny_skia::Pixmap,
    core: &RenderStateCore,
    origin_x: f64,
    origin_y: f64,
    focus_rect: Option<FocusRect>,
    backing_scale: f32,
) {
    paint_cursor_impl(
        pm,
        core,
        origin_x,
        origin_y,
        focus_rect,
        backing_scale,
        true,
    );
}

/// Paint cursor artwork on a neighboring display without duplicating the
/// display-clamped session badge owned by the cursor's anchor display.
pub fn paint_cursor_art(
    pm: &mut tiny_skia::Pixmap,
    core: &RenderStateCore,
    origin_x: f64,
    origin_y: f64,
    focus_rect: Option<FocusRect>,
    backing_scale: f32,
) {
    paint_cursor_impl(
        pm,
        core,
        origin_x,
        origin_y,
        focus_rect,
        backing_scale,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_cursor_impl(
    pm: &mut tiny_skia::Pixmap,
    core: &RenderStateCore,
    origin_x: f64,
    origin_y: f64,
    focus_rect: Option<FocusRect>,
    backing_scale: f32,
    paint_badge: bool,
) {
    if !core.cursor_is_revealed() {
        return;
    }
    let (cursor_x, cursor_y) = core.pos;

    let s = backing_scale.max(1.0) as f64; // logical-pt → pixmap-pixel scale
    let sf = s as f32;

    // Cursor anchor in pixmap-pixel space: subtract the (logical) origin
    // first, then scale into pixmap pixels.
    let (px, py) = ((cursor_x - origin_x) * s, (cursor_y - origin_y) * s);
    let heading = core.heading;
    let alpha_scale = core.idle_alpha as f32;

    // --- Focus rect highlight (macOS only — others pass None) ---
    // Cyan glow border + faint fill, matching Swift AgentCursor.showFocusRect.
    if let Some(fr) = focus_rect {
        let [fx, fy, fw, fh] = fr.rect;
        let t = fr.t as f32;
        let fade = (1.0 - t) * (1.0 - t); // quadratic ease-out
        let border_a = (230.0 * fade * alpha_scale) as u8;
        let fill_a = (20.0 * fade * alpha_scale) as u8;
        // Cyan: #5EC0E8
        let (cr, cg, cb) = (0x5Eu8, 0xC0u8, 0xE8u8);

        if let Some(rect) = tiny_skia::Rect::from_xywh(
            ((fx - origin_x) * s) as f32,
            ((fy - origin_y) * s) as f32,
            (fw * s) as f32,
            (fh * s) as f32,
        ) {
            // Faint fill
            let fill_paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                    cr, cg, cb, fill_a,
                )),
                ..Default::default()
            };
            pm.fill_rect(rect, &fill_paint, tiny_skia::Transform::identity(), None);

            // Border stroke (2px glow)
            let border_paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                    cr, cg, cb, border_a,
                )),
                anti_alias: true,
                ..Default::default()
            };
            let stroke = tiny_skia::Stroke {
                width: 2.5 * sf,
                ..Default::default()
            };
            let mut pb = tiny_skia::PathBuilder::new();
            pb.push_rect(rect);
            if let Some(path) = pb.finish() {
                pm.stroke_path(
                    &path,
                    &border_paint,
                    &stroke,
                    tiny_skia::Transform::identity(),
                    None,
                );
            }
        }
    }

    // Contact is anchored in global target coordinates, independent of artwork.
    if let Some(contact) = core.contact {
        let cx = ((contact.target.0 - origin_x) * s) as f32;
        let cy = ((contact.target.1 - origin_y) * s) as f32;
        let radius = (12.0 + 20.0 * contact.progress) as f32 * sf;
        let mut builder = tiny_skia::PathBuilder::new();
        builder.push_circle(cx, cy, radius);
        if let Some(direction) = contact.direction {
            let (dx, dy) = match direction {
                crate::ScrollDirection::Up => (0.0, -1.0),
                crate::ScrollDirection::Down => (0.0, 1.0),
                crate::ScrollDirection::Left => (-1.0, 0.0),
                crate::ScrollDirection::Right => (1.0, 0.0),
            };
            let tip = (cx + dx * 10.0 * sf, cy + dy * 10.0 * sf);
            builder.move_to(cx - dx * 7.0 * sf, cy - dy * 7.0 * sf);
            builder.line_to(tip.0, tip.1);
            builder.move_to(
                tip.0 - dx * 5.0 * sf - dy * 5.0 * sf,
                tip.1 - dy * 5.0 * sf + dx * 5.0 * sf,
            );
            builder.line_to(tip.0, tip.1);
            builder.line_to(
                tip.0 - dx * 5.0 * sf + dy * 5.0 * sf,
                tip.1 - dy * 5.0 * sf - dx * 5.0 * sf,
            );
        }
        if let Some(path) = builder.finish() {
            let mut paint = tiny_skia::Paint::default();
            paint.set_color_rgba8(
                94,
                192,
                232,
                (220.0 * (1.0 - contact.progress) * core.idle_alpha) as u8,
            );
            paint.anti_alias = true;
            let stroke = tiny_skia::Stroke {
                width: 2.0 * sf,
                ..Default::default()
            };
            pm.stroke_path(
                &path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    if let Some(theme) = core.theme.as_deref() {
        let (art_x, art_y) = core.registered_geometry().map_or((px, py), |(anchor, _)| {
            ((anchor.0 - origin_x) * s, (anchor.1 - origin_y) * s)
        });
        let tint = (theme.id == crate::DEFAULT_THEME_ID)
            .then(|| crate::session_fill_rgba(&core.cfg.cursor_id));
        crate::paint_compiled_theme_with_tint(
            pm,
            theme,
            &core.visual,
            art_x as f32,
            art_y as f32,
            heading as f32,
            backing_scale.max(1.0),
            alpha_scale,
            tint,
        );
    } else {
        // Defensive fallback for a manually constructed RenderStateCore. The
        // normal constructor always resolves either the requested theme or the
        // embedded default.
        crate::theme::paint_default_theme_with_fill(
            pm,
            &core.visual,
            px as f32,
            py as f32,
            heading as f32,
            backing_scale.max(1.0),
            alpha_scale,
            crate::session_fill_rgba(&core.cfg.cursor_id),
        );
    }

    if paint_badge {
        let (delivery, target) = core.badge_modifiers.unwrap_or((None, None));
        if let Some(layout) = crate::session_badge_layout(crate::SessionBadgeInput {
            label: core.session_label.as_deref(),
            delivery,
            target,
            cursor: (px as f32, py as f32),
            backing_scale: backing_scale.max(1.0),
            label_alpha: core.session_badge_alpha(),
            chip_alpha: core.session_badge_chip_alpha(),
            clip: Some((pm.width() as f32, pm.height() as f32)),
        }) {
            crate::paint_session_badge(
                pm,
                &layout,
                crate::session_fill_rgba(&core.cfg.cursor_id),
                alpha_scale,
            );
        }
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use crate::CursorConfig;

    #[test]
    fn visibility_uses_placement_not_coordinate_sign() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.pos = (-867.0, -200.0);
        assert!(!core.cursor_is_revealed());

        core.placed = true;
        assert!(core.cursor_is_revealed());
    }
}

#[cfg(test)]
mod glide_duration_tests {
    use super::*;
    use crate::{CursorConfig, PathPlanner};

    /// Run a glide of `dist_pts` to completion and return how many seconds it
    /// took. `tick` selects the platform path: `false` = `tick_motion`
    /// (Windows/Linux), `true` = `tick_swift_constants` (macOS reference).
    fn arrival_secs(glide_ms: f64, dist_pts: f64, swift: bool) -> f64 {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.motion.glide_duration_ms = glide_ms;
        core.motion.idle_hide_ms = 0.0;
        core.pos = (0.0, 0.0);
        core.placed = true;
        // Aligned headings → an effectively straight path of length ~dist_pts.
        core.path = Some(PathPlanner::plan(
            0.0, 0.0, 0.0, dist_pts, 0.0, 0.0, 0.0, 80.0,
        ));
        core.dist = 0.0;
        let dt = 1.0 / 240.0;
        let mut t = 0.0;
        for _ in 0..200_000 {
            let arrived = if swift {
                core.tick_swift_constants(dt)
            } else {
                core.tick_motion(dt)
            };
            t += dt;
            if arrived {
                break;
            }
        }
        t
    }

    #[test]
    fn fixed_duration_is_distance_independent_on_both_paths() {
        for swift in [false, true] {
            let short = arrival_secs(300.0, 120.0, swift);
            let long = arrival_secs(300.0, 1400.0, swift);
            // Both land in ~300ms regardless of distance (within a few ticks).
            assert!((short - 0.3).abs() < 0.05, "swift={swift} short={short}");
            assert!((long - 0.3).abs() < 0.05, "swift={swift} long={long}");
        }
    }

    #[test]
    fn zero_keeps_speed_based_timing() {
        // glide_duration_ms == 0 (the default) → longer paths take longer, on
        // both platform paths, exactly as before this field was implemented.
        for swift in [false, true] {
            let short = arrival_secs(0.0, 120.0, swift);
            let long = arrival_secs(0.0, 1400.0, swift);
            assert!(
                long > short + 0.2,
                "swift={swift} short={short} long={long}"
            );
        }
    }
}

#[cfg(test)]
mod session_badge_and_action_tests {
    use super::*;
    use crate::{CursorConfig, DeliveryModifier, TargetModifier};

    #[test]
    fn idle_hide_zero_keeps_a_positioned_session_cursor_visible() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.motion.idle_hide_ms = 0.0;
        assert!(core.apply_command_base(
            OverlayCommand::ClickPulse { x: 40.0, y: 60.0 },
            false,
            false,
        ));

        core.tick_motion(2.0);

        assert!(core.cursor_is_revealed());
        assert_eq!(core.pos, (40.0, 60.0));
        assert_eq!(core.idle_alpha, 1.0);
    }

    #[test]
    fn session_badge_holds_then_fades_once() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        assert_eq!(core.session_badge_alpha(), 0.0);
        assert!(core.apply_command_base(
            OverlayCommand::SetSessionLabel("Research".into()),
            false,
            false,
        ));
        assert_eq!(core.session_badge_alpha(), 1.0);

        core.tick_motion(SESSION_BADGE_HOLD_SECS - 0.05);
        assert_eq!(core.session_badge_alpha(), 1.0);
        core.tick_motion(SESSION_BADGE_FADE_SECS * 0.5 + 0.05);
        assert!(core.session_badge_alpha() > 0.0);
        assert!(core.session_badge_alpha() < 1.0);
        core.tick_motion(SESSION_BADGE_FADE_SECS);
        assert_eq!(core.session_badge_alpha(), 0.0);
    }

    #[test]
    fn repeated_session_label_metadata_does_not_restart_badge_timer() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.apply_command_base(
            OverlayCommand::SetSessionLabel("Research".into()),
            false,
            false,
        );
        core.tick_motion(SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS);
        assert_eq!(core.session_badge_alpha(), 0.0);

        core.apply_command_base(
            OverlayCommand::SetSessionLabel("Research".into()),
            false,
            false,
        );
        assert_eq!(core.session_badge_alpha(), 0.0);

        core.apply_command_base(
            OverlayCommand::SetSessionLabel("Writing".into()),
            false,
            false,
        );
        assert_eq!(core.session_badge_alpha(), 1.0);
    }

    #[test]
    fn revealing_hidden_cursor_restarts_badge_without_restarting_on_every_move() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.apply_command_base(
            OverlayCommand::SetSessionLabel("Research".into()),
            false,
            false,
        );
        core.tick_motion(SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS);
        assert_eq!(core.session_badge_alpha(), 0.0);

        core.apply_command_base(
            OverlayCommand::SnapTo {
                x: 100.0,
                y: 100.0,
                heading_radians: None,
            },
            false,
            false,
        );
        assert_eq!(core.session_badge_alpha(), 1.0);
        assert!(core.session_badge_needs_frame_tick());
        core.tick_motion(0.5);
        let elapsed = core.session_badge_secs;
        core.apply_command_base(
            OverlayCommand::SnapTo {
                x: 120.0,
                y: 120.0,
                heading_radians: None,
            },
            false,
            false,
        );
        assert_eq!(core.session_badge_secs, elapsed);
        core.tick_motion(SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS);
        assert!(!core.session_badge_needs_frame_tick());
    }

    #[test]
    fn hardware_pointer_hover_reveals_only_while_over_cursor() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.pos = (300.0, 240.0);
        core.placed = true;
        core.apply_command_base(
            OverlayCommand::SetSessionLabel("Research".into()),
            false,
            false,
        );
        core.tick_motion(SESSION_BADGE_HOLD_SECS + SESSION_BADGE_FADE_SECS);
        assert_eq!(core.session_badge_alpha(), 0.0);
        assert!(core.session_badge_needs_hover_poll());

        assert!(core.update_session_badge_hover(Some((302.0, 238.0))));
        assert_eq!(core.session_badge_alpha(), 1.0);
        assert!(!core.update_session_badge_hover(Some((304.0, 241.0))));
        assert_eq!(core.session_badge_alpha(), 1.0);

        assert!(core.update_session_badge_hover(Some((500.0, 500.0))));
        assert_eq!(core.session_badge_alpha(), 0.0);
    }

    #[test]
    fn movement_preserves_the_active_semantic_action() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.pos = (20.0, 20.0);
        core.placed = true;
        core.apply_command_base(
            OverlayCommand::BeginAction {
                action: CursorAction::Text,
                delivery: None,
                target: Some(TargetModifier::Ax),
            },
            false,
            false,
        );
        core.apply_command_base(
            OverlayCommand::MoveTo {
                x: 200.0,
                y: 100.0,
                end_heading_radians: 0.0,
            },
            false,
            false,
        );
        assert_eq!(core.visual.resolved_action, CursorAction::Text);
        assert_eq!(core.visual.target, Some(TargetModifier::Ax));
        core.apply_command_base(
            OverlayCommand::ClickPulse { x: 200.0, y: 100.0 },
            false,
            false,
        );
        assert_eq!(core.visual.resolved_action, CursorAction::Text);
        assert_eq!(core.visual.target, Some(TargetModifier::Ax));
    }

    #[test]
    fn modifiers_live_in_the_badge_then_fade_after_action_completion() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.pos = (200.0, 200.0);
        core.placed = true;
        core.apply_command_base(
            OverlayCommand::BeginAction {
                action: CursorAction::Click,
                delivery: Some(DeliveryModifier::Foreground),
                target: Some(TargetModifier::Pixel),
            },
            false,
            false,
        );
        assert_eq!(
            core.badge_modifiers,
            Some((
                Some(DeliveryModifier::Foreground),
                Some(TargetModifier::Pixel)
            ))
        );
        assert_eq!(core.session_badge_chip_alpha(), 1.0);
        assert!(core.session_badge_is_visible());

        let frame = 1.0 / 60.0;
        for _ in 0..=((CursorAction::Click.duration_secs() / frame).ceil() as usize) {
            core.tick_motion(frame);
        }
        assert!(core.session_badge_chip_alpha() > 0.0);
        assert!(core.session_badge_chip_alpha() < 1.0);
        assert!(core.session_badge_needs_frame_tick());

        core.tick_motion(SESSION_BADGE_FADE_SECS);
        assert_eq!(core.badge_modifiers, None);
        assert_eq!(core.session_badge_chip_alpha(), 0.0);
    }

    #[test]
    fn modifier_preemption_replaces_the_badge_context_without_cross_fading() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.apply_command_base(
            OverlayCommand::BeginAction {
                action: CursorAction::Observe,
                delivery: Some(DeliveryModifier::Background),
                target: Some(TargetModifier::Ax),
            },
            false,
            false,
        );
        core.apply_command_base(
            OverlayCommand::BeginAction {
                action: CursorAction::Text,
                delivery: Some(DeliveryModifier::Foreground),
                target: Some(TargetModifier::Browser),
            },
            false,
            false,
        );
        assert_eq!(
            core.badge_modifiers,
            Some((
                Some(DeliveryModifier::Foreground),
                Some(TargetModifier::Browser)
            ))
        );
        assert_eq!(core.badge_modifier_fade_secs, None);
        assert_eq!(core.session_badge_chip_alpha(), 1.0);
    }

    #[test]
    fn click_pulse_preserves_declared_context_until_the_action_fades() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.apply_command_base(
            OverlayCommand::BeginAction {
                action: CursorAction::Click,
                delivery: Some(DeliveryModifier::Background),
                target: Some(TargetModifier::Ax),
            },
            false,
            false,
        );
        core.apply_command_base(
            OverlayCommand::ClickPulse { x: 40.0, y: 60.0 },
            false,
            false,
        );
        assert_eq!(
            (core.visual.delivery, core.visual.target),
            (Some(DeliveryModifier::Background), Some(TargetModifier::Ax))
        );
        assert_eq!(
            core.badge_modifiers,
            Some((Some(DeliveryModifier::Background), Some(TargetModifier::Ax)))
        );
    }
}

#[cfg(test)]
mod backing_scale_tests {
    use super::*;
    use crate::CursorConfig;

    fn visible_pixel_count(pm: &tiny_skia::Pixmap) -> u32 {
        // Count strongly visible coverage, not the halo's feather pixels.
        // Low-alpha gradient coverage is quantized differently across scales
        // and is not useful evidence for the backing-scale regression.
        pm.data().chunks_exact(4).filter(|px| px[3] > 96).count() as u32
    }

    fn visible_bounds(pm: &tiny_skia::Pixmap) -> (u32, u32) {
        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0;
        let mut max_y = 0;
        for (index, pixel) in pm.data().chunks_exact(4).enumerate() {
            if pixel[3] <= 96 {
                continue;
            }
            let x = index as u32 % pm.width();
            let y = index as u32 / pm.width();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        assert_ne!(min_x, u32::MAX, "render should have visible pixels");
        (max_x - min_x + 1, max_y - min_y + 1)
    }

    fn render_at(backing_scale: f32, logical_size: u32) -> tiny_skia::Pixmap {
        let mut core = RenderStateCore::new(CursorConfig::default());
        // Place the cursor at the centre of the logical area and disable
        // idle-fade so the arrow paints at full alpha regardless of timing.
        let centre = logical_size as f64 / 2.0;
        core.pos = (centre, centre);
        core.placed = true;
        core.idle_alpha = 1.0;
        core.visible = true;

        // The pixmap is sized in *pixmap* pixels (logical × backing_scale)
        // — that's the macOS retina pipeline: allocate at physical pixels,
        // then let paint_cursor scale into them.
        let pm_size = (logical_size as f32 * backing_scale) as u32;
        let mut pm = tiny_skia::Pixmap::new(pm_size, pm_size).unwrap();
        paint_cursor(&mut pm, &core, 0.0, 0.0, None, backing_scale);
        pm
    }

    /// The compiled artifact contains vector geometry. Skia must rasterize it
    /// at the destination backing scale, so linear dimensions grow 1:2:3 and
    /// strongly visible coverage grows approximately with the square.
    #[test]
    fn compiled_vectors_render_at_one_two_and_three_x() {
        let pm_1x = render_at(1.0, 200);
        let pm_2x = render_at(2.0, 200);
        let pm_3x = render_at(3.0, 200);

        let n_1x = visible_pixel_count(&pm_1x);
        let n_2x = visible_pixel_count(&pm_2x);
        let n_3x = visible_pixel_count(&pm_3x);

        assert!(n_1x > 0, "1× render should paint SOMETHING (got {n_1x})");
        assert!(n_2x > 0, "2× render should paint SOMETHING (got {n_2x})");
        assert!(n_3x > 0, "3× render should paint SOMETHING (got {n_3x})");

        let ratio_2x = n_2x as f64 / n_1x as f64;
        let ratio_3x = n_3x as f64 / n_1x as f64;
        assert!(
            ratio_2x > 3.0 && ratio_2x < 5.0,
            "2× backing_scale should produce ~4× more visible pixels: \
             got n_1x={n_1x}, n_2x={n_2x}, ratio={ratio_2x:.2}"
        );
        assert!(
            ratio_3x > 7.0 && ratio_3x < 11.0,
            "3× backing_scale should produce ~9× more visible pixels: \
             got n_1x={n_1x}, n_3x={n_3x}, ratio={ratio_3x:.2}"
        );

        let bounds_1x = visible_bounds(&pm_1x);
        let bounds_2x = visible_bounds(&pm_2x);
        let bounds_3x = visible_bounds(&pm_3x);
        for (one, two, three) in [
            (bounds_1x.0, bounds_2x.0, bounds_3x.0),
            (bounds_1x.1, bounds_2x.1, bounds_3x.1),
        ] {
            assert!(
                (two as f64 / one as f64 - 2.0).abs() < 0.15,
                "2× visible bounds should double: {one}, {two}"
            );
            assert!(
                (three as f64 / one as f64 - 3.0).abs() < 0.20,
                "3× visible bounds should triple: {one}, {three}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    fn timed_event(
        action: u64,
        phase: VisualPhase,
        timestamp: Instant,
        target: Option<(f64, f64)>,
    ) -> VisualEvent {
        VisualEvent {
            id: VisualActionId {
                generation: 1,
                action,
            },
            timestamp,
            target,
            window: None,
            bounds: None,
            action: CursorAction::Click,
            scroll_direction: None,
            modifiers: None,
            phase,
        }
    }
    fn timed_core() -> RenderStateCore {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.placed = true;
        core.pos = (111.31370849898476, 111.31370849898476);
        core.motion.idle_hide_ms = 0.0;
        core
    }
    fn tick_at(core: &mut RenderStateCore, swift: bool, dt: f64, now: Instant) -> bool {
        if swift {
            core.tick_swift_constants_at(dt, now)
        } else {
            core.tick_motion_at(dt, now)
        }
    }
    fn assert_tip(core: &RenderStateCore, target: (f64, f64)) {
        assert!((core.pos.0 - core.heading.cos() * 16.0 - target.0).abs() < 0.001);
        assert!((core.pos.1 - core.heading.sin() * 16.0 - target.1).abs() < 0.001);
    }
    #[test]
    fn slice_a_delivery_ownership_cleanup_and_semantic_only() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        let t = Instant::now();
        let id = VisualActionId {
            generation: 1,
            action: 1,
        };
        let mut event = VisualEvent {
            id,
            timestamp: t,
            target: None,
            window: None,
            bounds: None,
            action: CursorAction::Text,
            scroll_direction: None,
            modifiers: None,
            phase: VisualPhase::Tracking,
        };
        assert!(core.apply_visual_event(event.clone(), None, t));
        core.tick_swift_constants_at(5.0, t + Duration::from_secs(5));
        assert!(!core.placed);
        assert_eq!(core.visual.resolved_action, CursorAction::Text);
        event.id.action = 2;
        event.timestamp = t + Duration::from_secs(6);
        core.apply_visual_event(event.clone(), None, event.timestamp);
        let mut old_end = event.clone();
        old_end.id.action = 1;
        old_end.phase = VisualPhase::End;
        assert!(!core.apply_visual_event(old_end, None, event.timestamp));
        assert_eq!(core.visual.resolved_action, CursorAction::Text);
        core.clear_visual_presentation();
        assert_eq!(core.visual.resolved_action, CursorAction::Idle);
    }

    #[test]
    fn slice_a_scroll_contact_retains_direction_and_changes_pixels() {
        let t = Instant::now();
        let mut images = Vec::new();
        for direction in [
            crate::ScrollDirection::Up,
            crate::ScrollDirection::Down,
            crate::ScrollDirection::Left,
            crate::ScrollDirection::Right,
        ] {
            let mut core = RenderStateCore::new(CursorConfig::default());
            let event = VisualEvent {
                id: VisualActionId {
                    generation: 1,
                    action: 1,
                },
                timestamp: t,
                target: Some((60.0, 60.0)),
                window: None,
                bounds: None,
                action: CursorAction::Scroll,
                scroll_direction: Some(direction),
                modifiers: None,
                phase: VisualPhase::Contact,
            };
            let display = DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 120.0,
                height: 120.0,
            };
            assert!(core.apply_visual_event(event, Some(display), t));
            assert_eq!(core.contact.unwrap().direction, Some(direction));
            let mut image = tiny_skia::Pixmap::new(120, 120).unwrap();
            paint_cursor(&mut image, &core, 0.0, 0.0, None, 1.0);
            images.push(image.data().to_vec());
        }
        for i in 0..4 {
            for j in 0..i {
                assert_ne!(images[i], images[j]);
            }
        }
    }

    #[test]
    fn slice_a_fix_core_newer_action_precedes_timestamp() {
        let t = Instant::now();
        let now = t + Duration::from_millis(30);
        let mut core = timed_core();
        assert!(core.apply_visual_event(
            timed_event(
                1,
                VisualPhase::Contact,
                t + Duration::from_millis(20),
                Some((20.0, 30.0))
            ),
            None,
            now
        ));
        assert!(core.apply_visual_event(
            timed_event(
                2,
                VisualPhase::Intent,
                t + Duration::from_millis(10),
                Some((80.0, 30.0))
            ),
            None,
            now
        ));
        assert!(core.path.is_some());
        assert!(core.apply_visual_event(
            timed_event(
                2,
                VisualPhase::Tracking,
                t + Duration::from_millis(15),
                Some((90.0, 30.0))
            ),
            None,
            now
        ));
        assert!(!core.apply_visual_event(
            timed_event(
                2,
                VisualPhase::Tracking,
                t + Duration::from_millis(14),
                Some((50.0, 30.0))
            ),
            None,
            now
        ));
        assert!(!core.apply_visual_event(
            timed_event(1, VisualPhase::Contact, now, Some((20.0, 30.0))),
            None,
            now
        ));
        assert_tip(&core, (90.0, 30.0));
    }

    #[test]
    fn slice_a_fix_core_contact_and_tracking_bar_intent_but_allow_tracking_ties() {
        let t = Instant::now();
        for phase in [VisualPhase::Contact, VisualPhase::Tracking] {
            let mut core = timed_core();
            assert!(core.apply_visual_event(timed_event(1, phase, t, Some((20.0, 30.0))), None, t));
            assert!(!core.apply_visual_event(
                timed_event(1, VisualPhase::Intent, t, Some((10.0, 30.0))),
                None,
                t
            ));
            assert!(core.apply_visual_event(
                timed_event(1, VisualPhase::Tracking, t, Some((60.0, 30.0))),
                None,
                t
            ));
            assert!(!core.apply_visual_event(
                timed_event(1, VisualPhase::Intent, t, Some((20.0, 30.0))),
                None,
                t
            ));
            assert!(!core.apply_visual_event(
                timed_event(
                    1,
                    VisualPhase::Intent,
                    t + Duration::from_millis(1),
                    Some((20.0, 30.0))
                ),
                None,
                t
            ));
            assert!(core.apply_visual_event(
                timed_event(1, VisualPhase::Tracking, t, Some((80.0, 30.0))),
                None,
                t
            ));
            assert_tip(&core, (80.0, 30.0));
            assert!(core.path.is_none());
        }
    }

    #[test]
    fn slice_a_timed_theme_age_and_reduced_motion_use_explicit_now() {
        let t = Instant::now();
        for swift in [false, true] {
            let mut core = timed_core();
            core.apply_visual_event(
                timed_event(1, VisualPhase::Contact, t, Some((500.0, 300.0))),
                None,
                t + Duration::from_millis(80),
            );
            assert!((core.visual.elapsed_secs - 0.08).abs() < 0.0001);
            tick_at(&mut core, swift, 5.0, t + Duration::from_millis(100));
            assert!((core.visual.elapsed_secs - 0.10).abs() < 0.0001);
            assert!(core.contact.is_some());
            core.advance_visual_presentation(t + Duration::from_millis(90));
            assert!((core.visual.elapsed_secs - 0.10).abs() < 0.0001);
            let next = t + Duration::from_millis(200);
            core.apply_visual_event(
                timed_event(2, VisualPhase::Intent, next, Some((900.0, 600.0))),
                None,
                next,
            );
            core.visual.reduced_motion = crate::ReducedMotion::On;
            tick_at(&mut core, swift, 0.0, next + Duration::from_millis(1));
            assert!(core.path.is_none());
            assert_tip(&core, (900.0, 600.0));
        }
    }

    #[test]
    fn slice_a_timed_travel_uses_wall_time_policy_on_both_tick_paths_without_config_changes() {
        let t = Instant::now();
        for swift in [false, true] {
            for (distance, duration) in [(1.0, 120.0), (150.0, 166.666667), (2000.0, 220.0)] {
                let mut core = timed_core();
                core.motion.glide_duration_ms = 3000.0;
                let config = core.motion.clone();
                core.apply_visual_event(
                    timed_event(1, VisualPhase::Intent, t, Some((100.0 + distance, 100.0))),
                    None,
                    t,
                );
                tick_at(
                    &mut core,
                    swift,
                    0.0,
                    t + Duration::from_secs_f64((duration - 1.0) / 1000.0),
                );
                assert!(core.path.is_some(), "travel must last its display duration");
                assert!(tick_at(
                    &mut core,
                    swift,
                    0.0,
                    t + Duration::from_secs_f64((duration + 1.0) / 1000.0)
                ));
                assert!(core.path.is_none() && core.spring.is_none());
                assert_tip(&core, (100.0 + distance, 100.0));
                assert_eq!(core.motion, config);
            }
        }
    }
    #[test]
    fn slice_a_timed_second_intent_interrupts_from_current_rendered_position() {
        let mut core = timed_core();
        let t = Instant::now();
        core.apply_visual_event(
            timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0))),
            None,
            t,
        );
        core.advance_visual_presentation(t + Duration::from_millis(40));
        let interrupted = core.pos;
        let later = t + Duration::from_millis(40);
        core.apply_visual_event(
            timed_event(2, VisualPhase::Intent, later, Some((700.0, 100.0))),
            None,
            later,
        );
        assert_eq!(core.pos, interrupted);
        core.advance_visual_presentation(later + Duration::from_millis(221));
        assert_tip(&core, (700.0, 100.0));
        assert!(!core.apply_visual_event(
            timed_event(1, VisualPhase::Contact, later, Some((500.0, 300.0))),
            None,
            later
        ));
        assert_tip(&core, (700.0, 100.0));
    }
    // The explicit watchable-playback amendment replaces early-contact snapping.
    #[test]
    fn watchable_contact_keeps_original_travel_then_pulses_on_both_tick_paths() {
        let t = Instant::now();
        for swift in [false, true] {
            let mut core = timed_core();
            core.apply_visual_event(
                timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0))),
                None,
                t,
            );
            tick_at(&mut core, swift, 0.0, t + Duration::from_millis(40));
            let before_contact = core.pos;
            let accepted = t + Duration::from_millis(40);
            core.apply_visual_event(
                timed_event(1, VisualPhase::Contact, accepted, Some((500.0, 300.0))),
                None,
                accepted,
            );
            assert_eq!(
                core.pos, before_contact,
                "same-target contact cannot snap or restart"
            );
            assert!(core.path.is_some());
            assert!(core.contact.is_none(), "contact is hidden during travel");
            tick_at(&mut core, swift, 0.0, t + Duration::from_millis(100));
            assert_ne!(core.pos, before_contact, "intermediate frames must travel");
            assert!(core.contact.is_none());
            tick_at(&mut core, swift, 0.0, t + Duration::from_millis(219));
            assert!(core.path.is_some());
            assert!(core.contact.is_none());
            tick_at(&mut core, swift, 0.0, t + Duration::from_millis(220));
            assert_tip(&core, (500.0, 300.0));
            assert!(core.path.is_none() && core.spring.is_none());
            let pulse = core.contact.unwrap();
            assert_eq!(pulse.target, (500.0, 300.0));
            assert_eq!(pulse.timestamp, accepted, "actual input time is immutable");
            assert_eq!(pulse.progress, 0.0);
            tick_at(&mut core, swift, 0.0, t + Duration::from_millis(295));
            assert!((core.contact.unwrap().progress - 0.5).abs() < 1e-9);
            tick_at(&mut core, swift, 0.0, t + Duration::from_millis(370));
            assert!(core.contact.is_none());
            assert_eq!(core.visual.resolved_action, CursorAction::Idle);
        }
    }

    #[test]
    fn watchable_pulse_pixels_appear_only_at_arrival_and_center_on_target() {
        let t = Instant::now();
        for scale in [1.0_f32, 2.0] {
            let mut core = timed_core();
            core.apply_visual_event(
                timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0))),
                None,
                t,
            );
            let accepted = t + Duration::from_millis(1);
            core.apply_visual_event(
                timed_event(1, VisualPhase::Contact, accepted, Some((500.0, 300.0))),
                None,
                accepted,
            );
            let paint = |core: &RenderStateCore| {
                let mut image =
                    tiny_skia::Pixmap::new((100.0 * scale) as u32, (100.0 * scale) as u32).unwrap();
                paint_cursor(&mut image, core, 450.0, 250.0, None, scale);
                image
            };
            core.advance_visual_presentation(t + Duration::from_millis(219));
            assert!(core.contact.is_none());
            let before = paint(&core);
            assert!(before.pixels().iter().any(|p| p.alpha() > 0));
            core.advance_visual_presentation(t + Duration::from_millis(295));
            let with_ring = paint(&core);
            let contact = core.contact.take().unwrap();
            assert_eq!(contact.timestamp, accepted);
            assert_eq!(
                contact.presentation_timestamp,
                t + Duration::from_millis(220)
            );
            let without_ring = paint(&core);
            let changed: Vec<_> = with_ring
                .pixels()
                .iter()
                .zip(without_ring.pixels())
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .map(|(i, _)| {
                    (
                        (i as u32 % with_ring.width()) as f64 / scale as f64,
                        (i as u32 / with_ring.width()) as f64 / scale as f64,
                    )
                })
                .collect();
            assert!(!changed.is_empty());
            let min_x = changed.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
            let max_x = changed
                .iter()
                .map(|p| p.0)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_y = changed.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
            let max_y = changed
                .iter()
                .map(|p| p.1)
                .fold(f64::NEG_INFINITY, f64::max);
            assert!(((min_x + max_x) / 2.0 - 50.0).abs() <= 1.0);
            assert!(((min_y + max_y) / 2.0 - 50.0).abs() <= 1.0);
        }
    }

    #[test]
    fn watchable_new_owner_removes_old_pulse_even_without_a_target() {
        let t = Instant::now();
        for pending in [false, true] {
            let mut core = timed_core();
            if pending {
                core.apply_visual_event(
                    timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0))),
                    None,
                    t,
                );
            }
            core.apply_visual_event(
                timed_event(1, VisualPhase::Contact, t, Some((500.0, 300.0))),
                None,
                t,
            );
            let next = t + Duration::from_millis(10);
            core.apply_visual_event(timed_event(2, VisualPhase::Intent, next, None), None, next);
            assert!(
                core.contact.is_none(),
                "new owner must replace every old effect"
            );
            core.advance_visual_presentation(t + Duration::from_millis(220));
            assert!(
                core.contact.is_none(),
                "old pending pulse must never reappear"
            );
            assert!(core.path.is_none());
        }
    }

    #[test]
    fn watchable_already_arrived_intent_and_contact_pulse_immediately() {
        let t = Instant::now();
        let mut core = timed_core();
        core.apply_visual_event(
            timed_event(1, VisualPhase::Intent, t, Some((100.0, 100.0))),
            None,
            t,
        );
        core.apply_visual_event(
            timed_event(1, VisualPhase::Contact, t, Some((100.0, 100.0))),
            None,
            t,
        );
        assert!(core.path.is_none());
        assert_tip(&core, (100.0, 100.0));
        assert_eq!(core.contact.unwrap().timestamp, t);
        assert_eq!(core.contact.unwrap().progress, 0.0);
    }

    #[test]
    fn watchable_stalls_age_original_schedule_without_replay() {
        let t = Instant::now();
        for first_drain in [false, true] {
            let mut core = timed_core();
            let now = if first_drain {
                t + Duration::from_millis(300)
            } else {
                t
            };
            core.apply_visual_event(
                timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0))),
                None,
                now,
            );
            core.apply_visual_event(
                timed_event(
                    1,
                    VisualPhase::Contact,
                    t + Duration::from_millis(1),
                    Some((500.0, 300.0)),
                ),
                None,
                now,
            );
            core.advance_visual_presentation(t + Duration::from_millis(300));
            assert_tip(&core, (500.0, 300.0));
            assert!(core.path.is_none());
            assert!((core.contact.unwrap().progress - 80.0 / 150.0).abs() < 1e-9);
            core.advance_visual_presentation(t + Duration::from_millis(370));
            assert!(core.contact.is_none());
            core.advance_visual_presentation(t + Duration::from_millis(310));
            assert!(core.contact.is_none());
        }
    }

    #[test]
    fn watchable_reduced_motion_arrives_and_pulses_without_delaying_input_time() {
        let t = Instant::now();
        for enable_during_travel in [false, true] {
            let mut core = timed_core();
            if !enable_during_travel {
                core.visual.reduced_motion = crate::ReducedMotion::On;
            }
            core.apply_visual_event(
                timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0))),
                None,
                t,
            );
            let accepted = t + Duration::from_millis(1);
            core.apply_visual_event(
                timed_event(1, VisualPhase::Contact, accepted, Some((500.0, 300.0))),
                None,
                accepted,
            );
            core.visual.reduced_motion = crate::ReducedMotion::On;
            core.advance_visual_presentation(t + Duration::from_millis(2));
            assert_tip(&core, (500.0, 300.0));
            assert!(core.path.is_none());
            assert_eq!(core.contact.unwrap().timestamp, accepted);
            core.advance_visual_presentation(t + Duration::from_millis(152));
            assert!(core.contact.is_none());
        }
    }

    #[test]
    fn watchable_changed_contact_target_cannot_reuse_wrong_travel_or_bounds() {
        let t = Instant::now();
        let mut core = timed_core();
        let mut intent = timed_event(1, VisualPhase::Intent, t, Some((500.0, 300.0)));
        intent.bounds = Some([490.0, 290.0, 20.0, 20.0]);
        core.apply_visual_event(intent, None, t);
        let accepted = t + Duration::from_millis(1);
        core.apply_visual_event(
            timed_event(1, VisualPhase::Contact, accepted, Some((800.0, 400.0))),
            None,
            accepted,
        );
        assert_tip(&core, (800.0, 400.0));
        assert!(core.focus_rect.is_none());
        assert!(core.path.is_none());
        assert_eq!(core.contact.unwrap().target, (800.0, 400.0));
        assert_eq!(core.contact.unwrap().timestamp, accepted);
    }

    #[test]
    fn slice_a_timed_old_contact_expires_before_first_draw_and_cannot_rewind_newer_tracking() {
        let mut core = timed_core();
        let t = Instant::now();
        core.apply_visual_event(
            timed_event(1, VisualPhase::Contact, t, Some((500.0, 300.0))),
            None,
            t + Duration::from_secs(1),
        );
        assert_tip(&core, (500.0, 300.0));
        assert!(core.contact.is_none());
        assert_eq!(core.visual.resolved_action, CursorAction::Idle);
        let later = t + Duration::from_secs(2);
        core.apply_visual_event(
            timed_event(1, VisualPhase::Tracking, later, Some((800.0, 400.0))),
            None,
            later,
        );
        assert!(!core.apply_visual_event(
            timed_event(1, VisualPhase::Contact, t, Some((500.0, 300.0))),
            None,
            later
        ));
        assert_tip(&core, (800.0, 400.0));
    }
    #[test]
    fn slice_a_timed_ring_pixels_keep_independent_negative_origin_anchor() {
        let t = Instant::now();
        for scale in [1.0, 2.0] {
            let mut core = timed_core();
            core.apply_visual_event(
                timed_event(1, VisualPhase::Contact, t, Some((-100.0, -100.0))),
                None,
                t,
            );
            core.apply_visual_event(
                timed_event(
                    1,
                    VisualPhase::Tracking,
                    t + Duration::from_millis(10),
                    Some((500.0, 300.0)),
                ),
                None,
                t + Duration::from_millis(10),
            );
            core.advance_visual_presentation(t + Duration::from_millis(50));
            let mut pm =
                tiny_skia::Pixmap::new((200.0 * scale) as u32, (200.0 * scale) as u32).unwrap();
            paint_cursor(&mut pm, &core, -200.0, -200.0, None, scale as f32);
            let pixels: Vec<_> = pm
                .pixels()
                .iter()
                .enumerate()
                .filter(|(_, p)| p.alpha() > 0)
                .map(|(i, _)| {
                    (
                        (i as u32 % pm.width()) as f64 / scale,
                        (i as u32 / pm.width()) as f64 / scale,
                    )
                })
                .collect();
            assert!(!pixels.is_empty(), "ring must paint on its own surface");
            let min_x = pixels.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
            let max_x = pixels.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
            let min_y = pixels.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
            let max_y = pixels.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
            assert!(
                (min_x - 80.0).abs() <= 1.5 && (min_y - 80.0).abs() <= 1.5,
                "{min_x}, {min_y}"
            );
            assert!(
                (max_x - 120.0).abs() <= 1.5 && (max_y - 120.0).abs() <= 1.5,
                "{max_x}, {max_y}"
            );
            core.advance_visual_presentation(t + Duration::from_millis(151));
            pm.fill(tiny_skia::Color::TRANSPARENT);
            paint_cursor(&mut pm, &core, -200.0, -200.0, None, scale as f32);
            assert!(pm.pixels().iter().all(|p| p.alpha() == 0));
        }
    }
    #[test]
    fn slice_a_timed_reduced_motion_and_first_display_seed_and_label_only() {
        let t = Instant::now();
        let mut core = RenderStateCore::new(CursorConfig::default());
        let mut label = timed_event(1, VisualPhase::Intent, t, None);
        label.action = CursorAction::Text;
        assert!(core.apply_visual_event(label, None, t));
        assert!(!core.placed);
        let display = DisplayBounds {
            x: -1000.0,
            y: -800.0,
            width: 1000.0,
            height: 800.0,
        };
        core.apply_visual_event(
            timed_event(2, VisualPhase::Intent, t, Some((-500.0, -300.0))),
            Some(display),
            t,
        );
        assert_eq!(core.pos, (-640.0, -440.0));
        assert!(core.path.is_some());
        core.visual.reduced_motion = crate::ReducedMotion::On;
        core.apply_visual_event(
            timed_event(3, VisualPhase::Intent, t, Some((-200.0, -100.0))),
            Some(display),
            t,
        );
        assert_tip(&core, (-200.0, -100.0));
        assert!(core.path.is_none() && core.spring.is_none());
    }

    use super::*;

    fn viewport_pixels(origin: (f64, f64), scale: f32, focus: bool) -> tiny_skia::Pixmap {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.apply_command_base(
            OverlayCommand::SnapTo {
                x: origin.0 + 40.0,
                y: origin.1 + 40.0,
                heading_radians: Some(0.0),
            },
            true,
            true,
        );
        render_frame(
            &core,
            (192.0 * scale) as u32,
            (128.0 * scale) as u32,
            origin.0,
            origin.1,
            focus.then_some(FocusRect {
                rect: [origin.0 + 95.0, origin.1 + 60.0, 40.0, 20.0],
                t: 0.0,
            }),
            scale,
        )
    }

    #[test]
    fn slice_a_viewport_pointer_and_focus_share_origin_at_native_scales() {
        for scale in [1.0, 2.0] {
            let reference = viewport_pixels((0.0, 0.0), scale, true);
            let pointer = viewport_pixels((0.0, 0.0), scale, false);
            assert!(pointer.data().chunks_exact(4).any(|p| p[3] > 0));
            assert_ne!(
                reference.data(),
                pointer.data(),
                "focus must contribute pixels"
            );
            for origin in [(-1440.0, 0.0), (0.0, -900.0), (-1440.0, -900.0)] {
                let projected = viewport_pixels(origin, scale, true);
                assert_eq!(
                    projected.data(),
                    reference.data(),
                    "origin={origin:?}, scale={scale}"
                );
                assert_eq!(viewport_pixels(origin, scale, false).data(), pointer.data());
            }
            // The known focus edge is at local (95, 70), independently of the cursor.
            let edge = reference
                .pixel((95.0 * scale) as u32, (70.0 * scale) as u32)
                .unwrap();
            assert!(edge.alpha() > 100);
        }
    }

    #[test]
    fn slice_a_viewport_negative_1400_cursor_paints_only_selected_surface() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.apply_command_base(
            OverlayCommand::SnapTo {
                x: -1400.0,
                y: 40.0,
                heading_radians: None,
            },
            true,
            true,
        );
        let selected = render_frame(&core, 192, 128, -1440.0, 0.0, None, 1.0);
        let primary = render_frame(&core, 192, 128, 0.0, 0.0, None, 1.0);
        assert!(selected.data().chunks_exact(4).any(|p| p[3] > 0));
        assert!(primary.data().iter().all(|v| *v == 0));
    }

    #[test]
    fn slice_a_viewport_two_x_doubles_focus_pixel_extent() {
        for scale in [1.0, 2.0] {
            let pm = viewport_pixels((0.0, -900.0), scale, true);
            assert_eq!(
                (pm.width(), pm.height()),
                ((192.0 * scale) as u32, (128.0 * scale) as u32)
            );
            // Interior fill and exterior are separate from pointer artwork.
            assert!(
                pm.pixel((110.0 * scale) as u32, (70.0 * scale) as u32)
                    .unwrap()
                    .alpha()
                    > 0
            );
            assert_eq!(
                pm.pixel((145.0 * scale) as u32, (70.0 * scale) as u32)
                    .unwrap()
                    .alpha(),
                0
            );
        }
    }

    #[test]
    fn slice_a_seed_primary_negative_axes_edges_and_corners() {
        for origin in [(0.0, 0.0), (-1440.0, 0.0), (0.0, -900.0)] {
            let display = DisplayBounds {
                x: origin.0,
                y: origin.1,
                width: 1440.0,
                height: 900.0,
            };
            for (local, seed) in [
                ((400.0, 400.0), (260.0, 260.0)),
                ((0.0, 0.0), (140.0, 140.0)),
                ((2.0, 2.0), (142.0, 142.0)),
                ((1439.0, 899.0), (1299.0, 759.0)),
                ((0.0, 500.0), (2.0, 360.0)),
            ] {
                let target = (origin.0 + local.0, origin.1 + local.1);
                let mut core = RenderStateCore::new(CursorConfig::default());
                core.pos = (-1400.0, -1400.0);
                assert!(core.initialize_near_target(target, display));
                assert_eq!(core.pos, (origin.0 + seed.0, origin.1 + seed.1));
                assert!(core.placed);
                assert!((core.pos.0 - target.0).hypot(core.pos.1 - target.1) <= 200.0);
                let first = core.pos;
                assert!(!core.initialize_near_target((origin.0 + 800.0, origin.1 + 700.0), display));
                assert_eq!(core.pos, first);
            }
        }
    }

    #[test]
    fn slice_a_tiny_bounds_have_safe_margins() {
        for (width, height) in [(1.0, 0.5), (0.001, 0.002), (4.0, 4.0)] {
            let mut core = RenderStateCore::new(CursorConfig::default());
            assert!(core.initialize_near_target(
                (0.0, 0.0),
                DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width,
                    height
                }
            ));
            assert!(core.pos.0 > 0.0 && core.pos.0 < width);
            assert!(core.pos.1 > 0.0 && core.pos.1 < height);
            assert!(core.pos.0.hypot(core.pos.1) <= 200.0);
        }
    }

    #[test]
    fn slice_a_invalid_bounds_and_absent_targets_never_place() {
        let valid = DisplayBounds {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        for bounds in [
            DisplayBounds {
                width: 0.0,
                ..valid
            },
            DisplayBounds {
                height: -1.0,
                ..valid
            },
            DisplayBounds {
                x: f64::NAN,
                ..valid
            },
            DisplayBounds {
                y: f64::INFINITY,
                ..valid
            },
            DisplayBounds {
                width: f64::INFINITY,
                ..valid
            },
            DisplayBounds {
                x: f64::MAX,
                width: f64::MAX,
                ..valid
            },
        ] {
            let mut core = RenderStateCore::new(CursorConfig::default());
            assert!(!core.initialize_near_target((50.0, 50.0), bounds));
            assert!(!core.placed);
        }
        for target in [
            (100.0, 50.0),
            (-1.0, 0.0),
            (50.0, 100.0),
            (f64::NAN, 1.0),
            (1.0, f64::INFINITY),
        ] {
            let mut core = RenderStateCore::new(CursorConfig::default());
            assert!(!core.initialize_near_target(target, valid));
            assert!(!core.placed);
        }
    }

    #[test]
    fn slice_a_disabled_and_unplaced_states_do_not_paint() {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.pos = (40.0, 40.0);
        let mut pm = tiny_skia::Pixmap::new(100, 100).unwrap();
        paint_cursor(&mut pm, &core, 0.0, 0.0, None, 1.0);
        assert!(pm.data().iter().all(|v| *v == 0));
        core.placed = true;
        core.cfg.enabled = false;
        paint_cursor(&mut pm, &core, 0.0, 0.0, None, 1.0);
        assert!(pm.data().iter().all(|v| *v == 0));
    }
    #[test]
    fn quick_approach_requires_current_concrete_tip_and_preserves_legacy_policy() {
        let now = Instant::now();
        let display = DisplayBounds {
            x: 0.0,
            y: 0.0,
            width: 2000.0,
            height: 1200.0,
        };
        let event = VisualEvent {
            id: VisualActionId {
                generation: 1,
                action: 1,
            },
            timestamp: now,
            target: Some((800.0, 500.0)),
            window: Some(42),
            bounds: None,
            action: CursorAction::Click,
            scroll_direction: None,
            modifiers: None,
            phase: VisualPhase::Intent,
        };
        let mut core = RenderStateCore::new(CursorConfig::default());
        assert!(core.apply_click_approach(event.clone(), Some(display), now));
        assert!(!core.is_target_frame(&event));
        core.advance_visual_presentation(now + Duration::from_millis(79));
        assert!(!core.is_target_frame(&event));
        core.advance_visual_presentation(now + Duration::from_millis(140));
        assert!(core.is_target_frame(&event));
        core.pos.0 += 1.0;
        assert!(
            !core.is_target_frame(&event),
            "elapsed time is not arrival evidence"
        );
        core.pos.0 -= 1.0;
        let mut other = event.clone();
        other.id.action += 1;
        assert!(!core.is_target_frame(&other));
        other = event.clone();
        other.target = Some((801.0, 500.0));
        assert!(!core.is_target_frame(&other));
        core.heading = 1.7;
        core.pos = (
            800.0 + 16.0 * core.heading.cos(),
            500.0 + 16.0 * core.heading.sin(),
        );
        let mut next = event.clone();
        next.id.action += 1;
        next.timestamp = now + Duration::from_millis(141);
        assert!(core.apply_click_approach(next.clone(), Some(display), next.timestamp));
        assert!(
            core.path.is_none(),
            "an arrived tip needs no glide even with a different heading"
        );
        assert!(core.is_target_frame(&next));
        core.apply_command_base(OverlayCommand::SetEnabled(false), false, false);
        assert!(!core.is_target_frame(&next));
    }
}

#[cfg(test)]
mod registration_tests;
