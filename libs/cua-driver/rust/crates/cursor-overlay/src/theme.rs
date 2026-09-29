//! Canonical cursor-theme semantics and `cua.default` runtime parameters.
//!
//! The default artwork is authored as a checked-in dotLottie archive and
//! compiled into the bounded artifact embedded by [`crate::theme_artifact`].
//! This module owns transport-neutral state, session colors, and the shared
//! runtime float transform. It does not parse Lottie at runtime.

use serde::{Deserialize, Serialize};

pub use cua_driver_contract::cursor::{session_fill_hex, session_fill_rgba, DEFAULT_CURSOR_FILL};
pub use cua_driver_contract::{
    CursorAction, CursorDelivery as DeliveryModifier, CursorPlayback as PlaybackKind,
    CursorReducedMotion as ReducedMotion, CursorTarget as TargetModifier,
};

pub const DEFAULT_THEME_ID: &str = "cua.default";
pub const DEFAULT_THEME_VERSION: &str = "2.0.0";
pub const THEME_PROFILE: &str = "cua-driver-actions-v2";
pub const CANVAS_SIZE: f32 = 128.0;
/// Points the 128-unit canvas spans on screen. 21 pt puts the arrow (fill
/// plus white outline, about 13 pt tall) at the size of Open Computer Use's
/// cursor, whose 252 px reference image is drawn into a 126 pt window with
/// a 13 pt arrow.
pub const DISPLAY_SIZE: f32 = 21.0;
/// Height in points of the arrow's fill-plus-outline silhouette at the
/// neutral heading: 26 of the 42 pt the canvas used to span (measured by
/// `arrow_height_matches_the_rendered_silhouette`).
pub const ARROW_HEIGHT: f32 = DISPLAY_SIZE * (26.0 / 42.0);
const FLOAT_DURATION_SECS: f32 = 4.0;

/// Where the artwork's anchor (the canvas centre, `RenderStateCore::pos`)
/// must sit so that the theme's `hotspot` (canvas units) lands on `tip`
/// when the arrow points along `heading` (radians, y down). The painter
/// rotates the canvas by `heading - π/4` about the anchor and scales it by
/// `DISPLAY_SIZE / CANVAS_SIZE`, so the hotspot's offset from the centre
/// is rotated and scaled the same way, then subtracted.
pub fn anchor_for_tip(tip: (f64, f64), heading: f64, hotspot: [u16; 2]) -> (f64, f64) {
    let scale = f64::from(DISPLAY_SIZE / CANVAS_SIZE);
    let half = f64::from(CANVAS_SIZE / 2.0);
    let (dx, dy) = (
        (f64::from(hotspot[0]) - half) * scale,
        (f64::from(hotspot[1]) - half) * scale,
    );
    let (sin, cos) = (heading - std::f64::consts::FRAC_PI_4).sin_cos();
    (tip.0 - (dx * cos - dy * sin), tip.1 - (dx * sin + dy * cos))
}

/// `anchor_for_tip` with the embedded default theme's hotspot, for callers
/// that have no theme in hand (drag tracking, examples).
pub fn default_anchor_for_tip(tip: (f64, f64), heading: f64) -> (f64, f64) {
    anchor_for_tip(
        tip,
        heading,
        crate::theme_artifact::embedded_default_theme().hotspot,
    )
}

/// Points from the tip to the farthest point a theme with this `hotspot`
/// can paint, at any heading: the canvas corner farthest from the hotspot
/// (rotation about the canvas centre keeps that distance), plus the default
/// theme's float drift (`shared_float_motion`: at most 11 canvas units of
/// translation, and a 2.5 degree sway about the centre that moves a point
/// at most 4 units). Content outside the 128-unit canvas is not covered.
pub fn tip_reach(hotspot: [u16; 2]) -> f64 {
    const FLOAT_DRIFT: f64 = 15.0;
    let canvas = f64::from(CANVAS_SIZE);
    let far = |h: u16| f64::from(h).max(canvas - f64::from(h));
    (far(hotspot[0]).hypot(far(hotspot[1])) + FLOAT_DRIFT) * f64::from(DISPLAY_SIZE) / canvas
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CursorVisualState {
    pub requested_action: CursorAction,
    pub resolved_action: CursorAction,
    pub delivery: Option<DeliveryModifier>,
    pub target: Option<TargetModifier>,
    pub elapsed_secs: f64,
    /// Grace period remaining after a short tool call ends, so observe/key
    /// cues remain visible for at least one rendered frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ending_secs: Option<f64>,
    pub reduced_motion: ReducedMotion,
    pub preempted_count: u64,
}

impl Default for CursorVisualState {
    fn default() -> Self {
        Self {
            requested_action: CursorAction::Idle,
            resolved_action: CursorAction::Idle,
            delivery: None,
            target: None,
            elapsed_secs: 0.0,
            ending_secs: None,
            reduced_motion: ReducedMotion::Auto,
            preempted_count: 0,
        }
    }
}

impl CursorVisualState {
    pub fn begin(
        &mut self,
        action: CursorAction,
        delivery: Option<DeliveryModifier>,
        target: Option<TargetModifier>,
    ) {
        if self.resolved_action != CursorAction::Idle && self.resolved_action != action {
            self.preempted_count = self.preempted_count.saturating_add(1);
        }
        self.requested_action = action;
        self.resolved_action = action;
        self.delivery = delivery;
        self.target = target;
        self.elapsed_secs = 0.0;
        self.ending_secs = None;
    }

    pub fn end(&mut self, action: CursorAction) {
        if self.resolved_action == action
            && matches!(action.playback(), PlaybackKind::Held | PlaybackKind::Loop)
        {
            self.ending_secs = Some(0.4);
        }
    }

    pub fn to_idle(&mut self) {
        self.requested_action = CursorAction::Idle;
        self.resolved_action = CursorAction::Idle;
        self.delivery = None;
        self.target = None;
        self.elapsed_secs = 0.0;
        self.ending_secs = None;
    }

    pub fn tick(&mut self, dt: f64) {
        let dt = dt.max(0.0);
        self.elapsed_secs = (self.elapsed_secs + dt).min(86_400.0);
        if let Some(remaining) = self.ending_secs {
            if remaining <= dt {
                self.to_idle();
                return;
            }
            self.ending_secs = Some(remaining - dt);
        }
        let action = self.resolved_action;
        if action.playback() == PlaybackKind::OneShot && self.elapsed_secs >= action.duration_secs()
        {
            self.to_idle();
        }
    }

    pub fn phase(&self) -> &'static str {
        match self.resolved_action.playback() {
            PlaybackKind::Resting | PlaybackKind::Loop => "loop",
            PlaybackKind::Held => "sustain",
            PlaybackKind::OneShot => "one_shot",
        }
    }

    pub fn frame(&self) -> u32 {
        let duration = self.resolved_action.duration_secs().max(1.0 / 30.0);
        let elapsed = match self.resolved_action.playback() {
            PlaybackKind::Resting | PlaybackKind::Loop | PlaybackKind::Held => {
                self.elapsed_secs.rem_euclid(duration)
            }
            PlaybackKind::OneShot => self.elapsed_secs.min(duration),
        };
        (elapsed * 30.0).floor() as u32
    }
}

pub(crate) fn shared_float_motion(visual: &CursorVisualState) -> (f32, f32, f32) {
    if visual.reduced_motion == ReducedMotion::On {
        return (0.0, 0.0, 0.0);
    }

    let progress =
        (visual.elapsed_secs as f32).rem_euclid(FLOAT_DURATION_SECS) / FLOAT_DURATION_SECS;
    let angle = progress * std::f32::consts::TAU;
    (
        angle.sin() * 5.0,
        6.0 * angle.cos() - 5.0,
        2.5_f32.to_radians() * angle.cos(),
    )
}

/// Paint one frame of the embedded actions-v2 theme.
///
/// `anchor_x/y` is the existing overlay's cursor centre. The Lottie canvas is
/// centred there so the established click offset and path physics remain
/// unchanged. `heading` rotates the artwork around the canvas centre.
pub fn paint_default_theme(
    pm: &mut tiny_skia::Pixmap,
    visual: &CursorVisualState,
    anchor_x: f32,
    anchor_y: f32,
    heading: f32,
    backing_scale: f32,
    alpha: f32,
) {
    paint_default_theme_with_fill(
        pm,
        visual,
        anchor_x,
        anchor_y,
        heading,
        backing_scale,
        alpha,
        DEFAULT_CURSOR_FILL,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn paint_default_theme_with_fill(
    pm: &mut tiny_skia::Pixmap,
    visual: &CursorVisualState,
    anchor_x: f32,
    anchor_y: f32,
    heading: f32,
    backing_scale: f32,
    alpha: f32,
    fill_rgba: [u8; 4],
) {
    let theme = crate::theme_artifact::embedded_default_theme();
    crate::theme_artifact::paint_compiled_theme_with_tint(
        pm,
        &theme,
        visual,
        anchor_x,
        anchor_y,
        heading,
        backing_scale,
        alpha,
        Some(fill_rgba),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_uses_compact_21_point_footprint() {
        assert_eq!(DISPLAY_SIZE, 21.0);
    }

    /// Bounds of the pixels `pred` accepts: (x0, y0, x1, y1), inclusive.
    fn bounds(pixmap: &tiny_skia::Pixmap, pred: &dyn Fn(&[u8]) -> bool) -> (u32, u32, u32, u32) {
        let mut result = (u32::MAX, u32::MAX, 0, 0);
        for (index, pixel) in pixmap.data().chunks_exact(4).enumerate() {
            if pred(pixel) {
                let x = index as u32 % pixmap.width();
                let y = index as u32 / pixmap.width();
                result.0 = result.0.min(x);
                result.1 = result.1.min(y);
                result.2 = result.2.max(x);
                result.3 = result.3.max(y);
            }
        }
        result
    }

    /// Paint the still default cursor at `heading` with its anchor at
    /// `anchor`, `scale` pixels per point.
    fn paint_still(anchor: (f32, f32), heading: f32, scale: f32) -> tiny_skia::Pixmap {
        let mut pixmap = tiny_skia::Pixmap::new(512, 512).unwrap();
        let visual = CursorVisualState {
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        paint_default_theme_with_fill(
            &mut pixmap,
            &visual,
            anchor.0,
            anchor.1,
            heading,
            scale,
            1.0,
            [12, 34, 56, 255],
        );
        pixmap
    }

    #[test]
    fn arrow_height_matches_the_rendered_silhouette() {
        let scale = 4.0;
        let pixmap = paint_still((256.0, 256.0), std::f32::consts::FRAC_PI_4, scale);
        let ink = bounds(&pixmap, &|pixel| pixel[3] >= 250);
        let height = (ink.3 - ink.1 + 1) as f32 / scale;
        assert!(
            (height - ARROW_HEIGHT).abs() <= 1.0,
            "silhouette is {height} pt tall, ARROW_HEIGHT is {ARROW_HEIGHT}"
        );
    }

    #[test]
    fn the_hotspot_lands_on_the_target_at_any_heading() {
        use std::f64::consts::PI;
        // Points, painted at `scale` pixels per point.
        let scale = 4.0f64;
        let target = (64.0, 64.0);
        for heading in [PI / 4.0, 0.0, PI / 2.0, 3.0 * PI / 4.0, PI, -PI / 4.0, -2.0] {
            let anchor = default_anchor_for_tip(target, heading);
            let pixmap = paint_still(
                ((anchor.0 * scale) as f32, (anchor.1 * scale) as f32),
                heading as f32,
                scale as f32,
            );
            // The tip is the outlined pixels farthest along the arrow's axis,
            // which points back along the heading: the rounded end's extreme
            // row is a run of pixels, so average the ones within half a
            // point of the extreme.
            let (dx, dy) = ((heading + PI).cos(), (heading + PI).sin());
            let mut ink = Vec::new();
            for (index, pixel) in pixmap.data().chunks_exact(4).enumerate() {
                if pixel[3] >= 250 {
                    let x = f64::from(index as u32 % pixmap.width()) / scale;
                    let y = f64::from(index as u32 / pixmap.width()) / scale;
                    ink.push(((x - anchor.0) * dx + (y - anchor.1) * dy, x, y));
                }
            }
            let farthest = ink.iter().map(|p| p.0).fold(f64::MIN, f64::max);
            let end: Vec<_> = ink.iter().filter(|p| p.0 >= farthest - 0.5).collect();
            let tip = (
                end.iter().map(|p| p.1).sum::<f64>() / end.len() as f64,
                end.iter().map(|p| p.2).sum::<f64>() / end.len() as f64,
            );
            let gap = ((tip.0 - target.0).powi(2) + (tip.1 - target.1).powi(2)).sqrt();
            assert!(gap <= 1.0, "heading {heading}: tip is {gap:.2} pt from the target");
        }
    }

    #[test]
    fn anchor_offset_scales_with_the_display_size_and_rotates_with_the_heading() {
        // The neutral heading is π/4: no rotation, so the offset is the
        // hotspot's canvas offset scaled to points.
        let hotspot = [46, 30];
        let s = f64::from(DISPLAY_SIZE / CANVAS_SIZE);
        let (x, y) = anchor_for_tip((100.0, 100.0), std::f64::consts::FRAC_PI_4, hotspot);
        assert!((x - (100.0 + 18.0 * s)).abs() < 1e-9 && (y - (100.0 + 34.0 * s)).abs() < 1e-9);
        // A quarter turn swaps the axes.
        let (x, y) = anchor_for_tip((100.0, 100.0), 3.0 * std::f64::consts::FRAC_PI_4, hotspot);
        assert!((x - (100.0 - 34.0 * s)).abs() < 1e-9 && (y - (100.0 + 18.0 * s)).abs() < 1e-9);
        // The canvas centre is its own hotspot.
        assert_eq!(anchor_for_tip((7.0, 9.0), 1.3, [64, 64]), (7.0, 9.0));
    }

    #[test]
    fn default_cursor_keeps_original_blue_fill() {
        assert_eq!(session_fill_rgba("default"), DEFAULT_CURSOR_FILL);
        assert_eq!(session_fill_hex("default"), "#5EC0E8");
    }

    #[test]
    fn named_session_colors_are_stable_and_distinct() {
        assert_eq!(session_fill_rgba("agent-1"), session_fill_rgba("agent-1"));
        assert_ne!(session_fill_rgba("agent-1"), session_fill_rgba("agent-2"));
        assert_ne!(session_fill_rgba("agent-2"), DEFAULT_CURSOR_FILL);
    }

    #[test]
    fn default_cursor_uses_parameterized_fill_white_ink_and_matching_glow() {
        let mut pixmap = tiny_skia::Pixmap::new(256, 256).unwrap();
        paint_default_theme_with_fill(
            &mut pixmap,
            &CursorVisualState::default(),
            128.0,
            128.0,
            std::f32::consts::FRAC_PI_4,
            2.0,
            1.0,
            [12, 34, 56, 255],
        );

        let pixels = pixmap.data().chunks_exact(4).collect::<Vec<_>>();
        assert!(pixels.iter().any(|pixel| *pixel == [12, 34, 56, 255]));
        assert!(pixels.iter().any(|pixel| *pixel == [255, 255, 255, 255]));
        assert!(pixels.iter().any(|pixel| pixel[2] > pixel[1]
            && pixel[1] > pixel[0]
            && pixel[0] > 0
            && pixel[3] > 16
            && pixel[3] < 200));
    }

    #[test]
    fn matching_glow_surrounds_the_full_pointer_silhouette() {
        let mut pixmap = tiny_skia::Pixmap::new(256, 256).unwrap();
        let visual = CursorVisualState {
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        paint_default_theme_with_fill(
            &mut pixmap,
            &visual,
            128.0,
            128.0,
            std::f32::consts::FRAC_PI_4,
            2.0,
            1.0,
            [12, 34, 56, 255],
        );

        let bounds = |predicate: &dyn Fn(&[u8]) -> bool| {
            let mut result = (u32::MAX, u32::MAX, 0, 0);
            for (index, pixel) in pixmap.data().chunks_exact(4).enumerate() {
                if predicate(pixel) {
                    let x = index as u32 % pixmap.width();
                    let y = index as u32 / pixmap.width();
                    result.0 = result.0.min(x);
                    result.1 = result.1.min(y);
                    result.2 = result.2.max(x);
                    result.3 = result.3.max(y);
                }
            }
            result
        };
        let body = bounds(&|pixel| pixel == [12, 34, 56, 255]);
        let glow = bounds(&|pixel| {
            pixel[2] > pixel[1]
                && pixel[1] > pixel[0]
                && pixel[0] > 0
                && pixel[3] > 8
                && pixel[3] < 220
        });

        assert!(
            glow.0 + 4 <= body.0,
            "glow should cover the pointer's left edge: body={body:?}, glow={glow:?}"
        );
        assert!(
            glow.1 + 4 <= body.1,
            "glow should cover the pointer's top edge: body={body:?}, glow={glow:?}"
        );
        assert!(
            glow.2 >= body.2 + 4,
            "glow should cover the pointer's right edge: body={body:?}, glow={glow:?}"
        );
        assert!(
            glow.3 >= body.3 + 4,
            "glow should cover the pointer's bottom edge: body={body:?}, glow={glow:?}"
        );
    }

    #[test]
    fn action_theme_ignores_host_owned_modifiers() {
        let mut base = tiny_skia::Pixmap::new(128, 128).unwrap();
        let mut pixmap = tiny_skia::Pixmap::new(128, 128).unwrap();
        let base_visual = CursorVisualState {
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        let visual = CursorVisualState {
            delivery: Some(DeliveryModifier::Foreground),
            target: Some(TargetModifier::Pixel),
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        paint_default_theme_with_fill(
            &mut base,
            &base_visual,
            64.0,
            64.0,
            std::f32::consts::FRAC_PI_4,
            1.0,
            1.0,
            [12, 34, 56, 255],
        );
        paint_default_theme_with_fill(
            &mut pixmap,
            &visual,
            64.0,
            64.0,
            std::f32::consts::FRAC_PI_4,
            1.0,
            1.0,
            [12, 34, 56, 255],
        );

        assert_eq!(pixmap.data(), base.data());
    }

    #[test]
    fn every_action_inherits_the_same_floating_base_motion() {
        let idle = CursorVisualState {
            elapsed_secs: 0.75,
            ..CursorVisualState::default()
        };
        let expected = shared_float_motion(&idle);
        assert_ne!(expected, (0.0, 0.0, 0.0));

        for action in CursorAction::ALL {
            let mut visual = CursorVisualState::default();
            visual.begin(action, None, None);
            visual.elapsed_secs = 0.75;
            assert_eq!(
                shared_float_motion(&visual),
                expected,
                "{} did not inherit the shared floating motion",
                action.as_str()
            );
        }
    }

    #[test]
    fn reduced_motion_disables_shared_floating_motion() {
        let visual = CursorVisualState {
            elapsed_secs: 0.75,
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        assert_eq!(shared_float_motion(&visual), (0.0, 0.0, 0.0));
    }

    #[test]
    fn one_shot_returns_to_idle() {
        let mut state = CursorVisualState::default();
        state.begin(CursorAction::Click, None, Some(TargetModifier::Pixel));
        state.tick(CursorAction::Click.duration_secs() + 0.01);
        assert_eq!(state.resolved_action, CursorAction::Idle);
        assert_eq!(state.target, None);
    }

    #[test]
    fn held_action_waits_for_matching_end() {
        let mut state = CursorVisualState::default();
        state.begin(CursorAction::Text, Some(DeliveryModifier::Foreground), None);
        state.tick(30.0);
        assert_eq!(state.resolved_action, CursorAction::Text);
        state.end(CursorAction::Click);
        assert_eq!(state.resolved_action, CursorAction::Text);
        state.end(CursorAction::Text);
        assert_eq!(state.resolved_action, CursorAction::Text);
        state.tick(0.41);
        assert_eq!(state.resolved_action, CursorAction::Idle);
    }

    #[test]
    fn every_action_renders_pixels_at_one_and_two_x() {
        for action in CursorAction::ALL {
            for scale in [1.0, 2.0] {
                let mut pm = tiny_skia::Pixmap::new(256, 256).unwrap();
                let mut visual = CursorVisualState::default();
                visual.begin(
                    action,
                    Some(DeliveryModifier::Background),
                    Some(TargetModifier::Ax),
                );
                visual.elapsed_secs = action.duration_secs() * 0.4;
                paint_default_theme(&mut pm, &visual, 128.0, 128.0, 0.0, scale, 1.0);
                assert!(
                    pm.data().chunks_exact(4).any(|pixel| pixel[3] != 0),
                    "{} did not render at {scale}x",
                    action.as_str()
                );
            }
        }
    }
}
