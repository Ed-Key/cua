//! The agent cursor inside the panel. The cursor overlay draws the agent's
//! cursor in its own screen-wide window, so the window-scoped live stream
//! and stills never contain it; the front card draws a sprite of it instead,
//! fed by the overlay's render thread (the cursor row of the table in
//! `finish`). Everything here is pure: mapping a screen point into the
//! well, the sprite's frame, and when a click is logged.

use super::{Area, Tag};

/// The sprite's pixmap is this many points square, centered on the
/// arrow's tip, for a theme with this `hotspot`: room for everything the
/// theme can paint at any heading (`cursor_overlay::tip_reach`). About 47 pt
/// for the default artwork, up to about 65 pt for a hotspot in a corner.
pub(crate) fn sprite_box(hotspot: [u16; 2]) -> f64 {
    2.0 * cursor_overlay::tip_reach(hotspot)
}

/// The sprite never shrinks the arrow below this many points tall: the
/// smallest size at which its fill, white outline and heading still read
/// at a glance on the panel (the outline stays a device pixel wide at 2x).
const MIN_SPRITE_ARROW: f64 = 10.0;

/// How much the sprite shrinks for a well of `well` size showing the target
/// window `window`: the preview's own scale (aspect-fit, as `cursor_in_well`
/// maps points), so the cursor looks like the real one shrunk with the
/// window, never enlarged past it, and floored so the arrow stays at least
/// `MIN_SPRITE_ARROW` tall.
pub(super) fn sprite_scale(window: Area, well: (f64, f64)) -> f64 {
    let fit = (well.0 / window.w).min(well.1 / window.h);
    fit.clamp(MIN_SPRITE_ARROW / f64::from(cursor_overlay::ARROW_HEIGHT), 1.0)
}

/// Where a screen point (CoreGraphics, top-left origin) inside the target
/// window `window` lands in a well of `well` size that shows the window
/// scaled to fit, preserving aspect and centered (as the live layer's
/// `resizeAspect` and the stream's scaling do). Well coordinates, top-left
/// origin. `None` when the point is outside the window.
pub(super) fn cursor_in_well(
    window: Area,
    point: (f64, f64),
    well: (f64, f64),
) -> Option<(f64, f64)> {
    if window.w <= 0.0 || window.h <= 0.0 {
        return None;
    }
    let (x, y) = (point.0 - window.x, point.1 - window.y);
    if x < 0.0 || y < 0.0 || x > window.w || y > window.h {
        return None;
    }
    let scale = (well.0 / window.w).min(well.1 / window.h);
    let (shown_w, shown_h) = (window.w * scale, window.h * scale);
    let (dx, dy) = ((well.0 - shown_w) / 2.0, (well.1 - shown_h) / 2.0);
    Some((dx + x * scale, dy + y * scale))
}

/// The sprite layer's frame in the well's coordinates (AppKit, bottom-left
/// origin) for a cursor at `point` (well coordinates, top-left origin) in
/// a well `well_h` tall: a `side` point square centered on the point. The
/// sprite is drawn around the tip, so the layer stretching it to the frame
/// shrinks the arrow about the tip, which stays on the point.
pub(super) fn sprite_frame(point: (f64, f64), well_h: f64, side: f64) -> Area {
    Area {
        x: point.0 - side / 2.0,
        y: well_h - point.1 - side / 2.0,
        w: side,
        h: side,
    }
}

/// The screen area a picture of `window` (screen points) shows: the page
/// `crop` of it (window points) when the picture is cut to its page, else
/// the whole window.
pub(super) fn shown_area(window: Area, crop: Option<Area>) -> Area {
    match crop {
        Some(crop) => Area {
            x: window.x + crop.x,
            y: window.y + crop.y,
            w: crop.w,
            h: crop.h,
        },
        None => window,
    }
}

/// The window frame to map the cursor into, or `None` to hide the sprite:
/// only when the window the cursor is working in (`cursor_window`, the
/// window its last action targeted) is the window the panel displays
/// (`displayed`), and the cached `frame` (tagged with its window's id) is
/// of that window. A cursor with no target window, a panel whose target is
/// unresolved or still the previous window (its new target's capture
/// pending), or a frame still describing the previous window (a raised
/// card before the poll looks its window up) shows nothing: the sprite
/// must never paint over another window's picture, or with another
/// window's geometry.
pub(super) fn sprite_window(
    cursor_window: Option<u32>,
    displayed: Option<Tag>,
    frame: Option<(u32, Area)>,
) -> Option<Area> {
    match (cursor_window, displayed, frame) {
        (Some(window), Some((_, Some(shown))), Some((of, area))) if window == shown && of == shown => {
            Some(area)
        }
        _ => None,
    }
}

/// Where the sprite goes for the cursor's latest screen `point` (`None`
/// while it has none to show), the target window's last known `window`
/// frame, and a well of `well` size: the sprite frame (its `sprite_box`
/// shrunk by `sprite_scale`), or `None` to hide it. Recomputed whenever any
/// of the three changes, so a window that moves or a well that resizes
/// never leaves the sprite where it was.
pub(super) fn sprite_placement(
    window: Option<Area>,
    point: Option<(f64, f64)>,
    well: (f64, f64),
    sprite_box: f64,
) -> Option<Area> {
    let window = window?;
    let point = point?;
    cursor_in_well(window, point, well)
        .map(|in_well| sprite_frame(in_well, well.1, sprite_box * sprite_scale(window, well)))
}

/// The sprite's state between updates: whether the last update was a
/// click pulse, so each click is logged once.
#[derive(Default)]
pub(super) struct Sprite {
    pulsing: bool,
}

impl Sprite {
    /// An update landed: the cursor is at `point` in the well (`None` when
    /// it is hidden) and its click pulse is `pulsing`. Whether this update
    /// is the start of a click that should be logged: the first pulsing
    /// update of a click, and only when the cursor is in the well.
    pub(super) fn update(&mut self, point: Option<(f64, f64)>, pulsing: bool) -> bool {
        let edge = pulsing && !self.pulsing;
        self.pulsing = pulsing;
        edge && point.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Area = Area {
        x: 100.0,
        y: 50.0,
        w: 800.0,
        h: 600.0,
    };

    #[test]
    fn a_screen_point_maps_into_the_well_where_the_window_is_drawn() {
        // 800x600 into 320x200: scale 1/3, 266.7 wide, centered with a
        // 26.7 pt band on each side.
        let well = (320.0, 200.0);
        let (x, y) = cursor_in_well(WINDOW, (500.0, 350.0), well).unwrap();
        assert!((x - (400.0 / 3.0 + 80.0 / 3.0)).abs() < 1e-9, "{x}");
        assert!((y - 100.0).abs() < 1e-9, "{y}");
        // The window's corners land on the drawn image's corners.
        assert_eq!(
            cursor_in_well(WINDOW, (100.0, 50.0), well).map(|(x, y)| (x.round(), y)),
            Some((27.0, 0.0))
        );
        assert_eq!(
            cursor_in_well(WINDOW, (900.0, 650.0), well).map(|(x, y)| (x.round(), y)),
            Some((293.0, 200.0))
        );
        // A tall well letterboxes above and below instead.
        let (x, y) = cursor_in_well(WINDOW, (100.0, 50.0), (200.0, 300.0)).unwrap();
        assert_eq!((x, y), (0.0, 75.0));
    }

    #[test]
    fn a_point_outside_the_window_hides_the_cursor() {
        let well = (320.0, 200.0);
        assert_eq!(cursor_in_well(WINDOW, (99.0, 300.0), well), None);
        assert_eq!(cursor_in_well(WINDOW, (500.0, 651.0), well), None);
        assert_eq!(
            cursor_in_well(WINDOW, (-200.0, -200.0), well),
            None,
            "the sentinel"
        );
        let empty = Area { w: 0.0, ..WINDOW };
        assert_eq!(cursor_in_well(empty, (100.0, 50.0), well), None);
    }

    #[test]
    fn a_page_only_picture_maps_the_cursor_into_the_page_and_hides_it_over_the_toolbar() {
        // The window's top 100 pt are tab strip and toolbar: the picture is
        // the 800x500 page, which fills a 320x200 well exactly.
        let crop = Area {
            x: 0.0,
            y: 100.0,
            w: 800.0,
            h: 500.0,
        };
        let page = shown_area(WINDOW, Some(crop));
        assert_eq!(page, Area { y: 150.0, h: 500.0, ..WINDOW });
        let well = (320.0, 200.0);
        // The page's top-left corner is the well's.
        assert_eq!(cursor_in_well(page, (100.0, 150.0), well), Some((0.0, 0.0)));
        // Mid page lands mid well, at the page's own scale (2/5), not the window's.
        assert_eq!(cursor_in_well(page, (500.0, 400.0), well), Some((160.0, 100.0)));
        // Over the toolbar: outside the picture, so no sprite.
        assert_eq!(cursor_in_well(page, (500.0, 120.0), well), None);
        assert_eq!(sprite_placement(Some(page), Some((500.0, 120.0)), well, 48.0), None);
        // The sprite shrinks with the page's scale (above the floor), not
        // the window's.
        assert_eq!(sprite_scale(page, (640.0, 400.0)), 0.8);
        // Without a crop the picture is the whole window.
        assert_eq!(shown_area(WINDOW, None), WINDOW);
    }

    #[test]
    fn the_sprite_is_a_box_centered_on_the_point_in_appkit_coordinates() {
        let side = 48.0;
        let frame = sprite_frame((160.0, 100.0), 200.0, side);
        assert_eq!(
            frame,
            Area {
                x: 160.0 - side / 2.0,
                y: 100.0 - side / 2.0,
                w: side,
                h: side
            }
        );
        // The well's top-left maps to the top-left of the AppKit frame.
        assert_eq!(sprite_frame((0.0, 0.0), 200.0, side).y, 200.0 - side / 2.0);
        // A smaller box shrinks about the point, so the tip stays put.
        let half = sprite_frame((160.0, 100.0), 200.0, side / 2.0);
        assert_eq!((half.w, half.h), (side / 2.0, side / 2.0));
        assert_eq!(
            (half.x + half.w / 2.0, half.y + half.h / 2.0),
            (frame.x + frame.w / 2.0, frame.y + frame.h / 2.0)
        );
    }

    #[test]
    fn the_sprite_shrinks_with_the_preview_down_to_a_readable_arrow() {
        let floor = MIN_SPRITE_ARROW / f64::from(cursor_overlay::ARROW_HEIGHT);
        assert!(floor > 0.5 && floor < 1.0, "{floor}");
        // 800x600 shown in 640x480: the preview is 0.8 scale, and so is the sprite.
        assert!((sprite_scale(WINDOW, (640.0, 480.0)) - 0.8).abs() < 1e-9);
        // In 320x200 the preview is a third: below the floor, so the arrow
        // stays MIN_SPRITE_ARROW tall.
        let scale = sprite_scale(WINDOW, (320.0, 200.0));
        assert_eq!(scale, floor);
        assert!((f64::from(cursor_overlay::ARROW_HEIGHT) * scale - MIN_SPRITE_ARROW).abs() < 1e-9);
        // A well larger than the window never enlarges the cursor.
        assert_eq!(sprite_scale(WINDOW, (1600.0, 1200.0)), 1.0);
    }

    const HEADINGS: [f64; 9] = [
        0.0,
        0.8,
        std::f64::consts::FRAC_PI_4,
        std::f64::consts::FRAC_PI_2,
        2.4,
        3.1,
        -0.7,
        -std::f64::consts::FRAC_PI_2,
        -2.3,
    ];

    /// Whether any visible pixel touches the pixmap's border.
    fn touches_edge(pm: &tiny_skia::Pixmap) -> bool {
        let side = pm.width();
        pm.data().chunks_exact(4).enumerate().any(|(i, px)| {
            let (x, y) = (i as u32 % side, i as u32 / side);
            px[3] > 4 && (x == 0 || y == 0 || x == side - 1 || y == side - 1)
        })
    }

    #[test]
    fn the_sprite_box_centered_on_the_tip_holds_every_action_at_any_heading() {
        use cursor_overlay::{CursorAction, CursorVisualState};
        let scale = 2.0;
        let hotspot = cursor_overlay::embedded_default_theme().hotspot;
        let side = (sprite_box(hotspot) * scale).ceil() as u32;
        let center = f64::from(side) / 2.0;
        for heading in HEADINGS {
            for action in CursorAction::ALL {
                let mut visual = CursorVisualState::default();
                visual.begin(action, None, None);
                for t in [0.1, 0.4, 0.7] {
                    visual.elapsed_secs = action.duration_secs() * t;
                    let mut pm = tiny_skia::Pixmap::new(side, side).unwrap();
                    cursor_overlay::theme::paint_default_theme(
                        &mut pm,
                        &visual,
                        // The painter draws the hotspot (the tip) here.
                        center as f32,
                        center as f32,
                        heading as f32,
                        scale as f32,
                        1.0,
                    );
                    assert!(
                        !touches_edge(&pm),
                        "{} at heading {heading}, t {t}: clipped",
                        action.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn the_sprite_box_holds_a_theme_that_fills_its_canvas_for_any_hotspot() {
        use cursor_overlay::theme_artifact::{
            CompiledDrawCommand, CompiledFrame, CompiledGeometry, CompiledTransform,
        };
        let scale = 2.0;
        // A valid custom theme whose every action fills the whole canvas.
        let mut theme = (*cursor_overlay::embedded_default_theme()).clone();
        theme.id = "com.example.full-canvas".into();
        let full = CompiledFrame {
            commands: vec![CompiledDrawCommand {
                geometries: vec![CompiledGeometry::Rectangle {
                    center: [64.0, 64.0],
                    size: [128.0, 128.0],
                    roundness: 0.0,
                }],
                transform: CompiledTransform::default(),
                opacity: 1.0,
                fill: Some([200, 40, 40, 255]),
                stroke: None,
            }],
        };
        for animation in theme.actions.values_mut() {
            animation.frames = vec![full.clone()];
            animation.still_frame = 0;
        }
        let visual = cursor_overlay::CursorVisualState::default();
        // Hotspots at a corner, the centre and the far corner (the last
        // valid canvas unit).
        for hotspot in [[0, 0], [64, 64], [127, 127], [0, 127]] {
            theme.hotspot = hotspot;
            let side = (sprite_box(hotspot) * scale).ceil() as u32;
            let center = f64::from(side) / 2.0;
            let mut reach = 0.0f64;
            for heading in HEADINGS {
                let mut pm = tiny_skia::Pixmap::new(side, side).unwrap();
                cursor_overlay::paint_compiled_theme(
                    &mut pm,
                    &theme,
                    &visual,
                    center as f32,
                    center as f32,
                    heading as f32,
                    scale as f32,
                    1.0,
                );
                assert!(
                    !touches_edge(&pm),
                    "{hotspot:?} at heading {heading}: clipped"
                );
                for (i, px) in pm.data().chunks_exact(4).enumerate() {
                    if px[3] > 4 {
                        let (x, y) = (f64::from(i as u32 % side), f64::from(i as u32 / side));
                        reach = reach.max((x + 0.5 - center).hypot(y + 0.5 - center) / scale);
                    }
                }
            }
            // The canvas really reaches its farthest corner, so the box is
            // not passing only because the art is small.
            let far = |h: u16| f64::from(h).max(128.0 - f64::from(h));
            let corner = far(hotspot[0]).hypot(far(hotspot[1]))
                * f64::from(cursor_overlay::DISPLAY_SIZE)
                / 128.0;
            assert!(
                reach > corner - 1.0,
                "{hotspot:?}: reach {reach}, corner {corner}"
            );
        }
    }

    #[test]
    fn the_sprite_shows_only_over_the_window_the_cursor_works_in() {
        let displayed: Option<Tag> = Some((Some(1), Some(10)));
        let frame = Some((10, WINDOW));
        // Matching window, with its own frame: shown.
        assert_eq!(sprite_window(Some(10), displayed, frame), Some(WINDOW));
        // The user raised a back card while the agent stays in window 10:
        // hidden, whatever the geometry.
        assert_eq!(sprite_window(Some(10), Some((Some(1), Some(20))), frame), None);
        // The agent moved to window 20 but its capture is pending, so the
        // panel still displays 10: hidden until the frame lands.
        assert_eq!(sprite_window(Some(20), displayed, frame), None);
        // No target window known on either side: hidden.
        assert_eq!(sprite_window(None, displayed, frame), None);
        assert_eq!(sprite_window(Some(10), Some((Some(1), None)), frame), None);
        assert_eq!(sprite_window(Some(10), None, frame), None);
        // A raised card that is the cursor's window, while the cached frame
        // still describes the previous window: hidden until its own frame
        // lands, never mapped with the old window's bounds.
        let raised: Option<Tag> = Some((Some(1), Some(20)));
        assert_eq!(sprite_window(Some(20), raised, frame), None);
        let own = Area { x: 400.0, ..WINDOW };
        assert_eq!(sprite_window(Some(20), raised, Some((20, own))), Some(own));
        // No frame at all: hidden.
        assert_eq!(sprite_window(Some(10), displayed, None), None);
    }

    #[test]
    fn the_sprite_follows_the_window_and_the_well_not_just_new_renders() {
        let well = (320.0, 200.0);
        let point = Some((500.0, 350.0));
        let before = sprite_placement(Some(WINDOW), point, well, 48.0).unwrap();
        // The window moves 30 pt right with no new cursor render: the same
        // screen point is now 10 pt (a third) further left in the well.
        let moved = Area { x: WINDOW.x + 30.0, ..WINDOW };
        let after = sprite_placement(Some(moved), point, well, 48.0).unwrap();
        assert!((before.x - after.x - 10.0).abs() < 1e-9, "{before:?} {after:?}");
        assert_eq!(before.y, after.y);
        // It moves so far the point is outside: the sprite hides.
        let far = Area { x: 600.0, ..WINDOW };
        assert_eq!(sprite_placement(Some(far), point, well, 48.0), None);
        // The well resizes: the sprite is re-placed for the new scale.
        let bigger = sprite_placement(Some(WINDOW), point, (640.0, 400.0), 48.0).unwrap();
        assert!((bigger.x + bigger.w / 2.0 - 2.0 * (before.x + before.w / 2.0)).abs() < 1e-9);
        // No window frame or no cursor point: hidden.
        assert_eq!(sprite_placement(None, point, well, 48.0), None);
        assert_eq!(sprite_placement(Some(WINDOW), None, well, 48.0), None);
    }

    // ── Row: cursor update ───────────────────────────────────────────────

    #[test]
    fn row_cursor_update_logs_each_click_once_and_only_in_the_well() {
        let mut sprite = Sprite::default();
        let inside = Some((10.0, 10.0));
        // Moves do not log.
        assert!(!sprite.update(inside, false));
        // The first pulsing update logs; the pulse's later frames do not.
        assert!(sprite.update(inside, true));
        assert!(!sprite.update(inside, true));
        assert!(!sprite.update(inside, false));
        // A second click logs again.
        assert!(sprite.update(inside, true));
        sprite.update(inside, false);
        // A click while the cursor is off the window (hidden) is not the
        // panel's to log, and it does not carry over to the next update.
        assert!(!sprite.update(None, true));
        assert!(!sprite.update(inside, true));
    }
}
