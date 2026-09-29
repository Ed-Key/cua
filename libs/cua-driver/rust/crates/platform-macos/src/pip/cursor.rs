//! The agent cursor inside the panel. The cursor overlay draws the agent's
//! cursor in its own screen-wide window, so the window-scoped live stream
//! and stills never contain it; the front card draws a sprite of it instead,
//! fed by the overlay's render thread (the cursor row of the table in
//! `finish`). Everything here is pure: mapping a screen point into the
//! well, the sprite's frame, and when a click is logged.

use super::Area;

/// The sprite's pixmap is this many points square, centered on the
/// cursor's anchor: room for the arrow at any heading and its click pulse.
pub(crate) const SPRITE_BOX: f64 = 96.0;

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
/// a well `well_h` tall: the `SPRITE_BOX` square centered on the point.
pub(super) fn sprite_frame(point: (f64, f64), well_h: f64) -> Area {
    Area {
        x: point.0 - SPRITE_BOX / 2.0,
        y: well_h - point.1 - SPRITE_BOX / 2.0,
        w: SPRITE_BOX,
        h: SPRITE_BOX,
    }
}

/// Where the sprite goes for the cursor's latest screen `point` (`None`
/// while it has none to show), the target window's last known `window`
/// frame, and a well of `well` size: the sprite frame, or `None` to hide
/// it. Recomputed whenever any of the three changes, so a window that
/// moves or a well that resizes never leaves the sprite where it was.
pub(super) fn sprite_placement(
    window: Option<Area>,
    point: Option<(f64, f64)>,
    well: (f64, f64),
) -> Option<Area> {
    let window = window?;
    let point = point?;
    cursor_in_well(window, point, well).map(|in_well| sprite_frame(in_well, well.1))
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
    fn the_sprite_is_a_box_centered_on_the_point_in_appkit_coordinates() {
        let frame = sprite_frame((160.0, 100.0), 200.0);
        assert_eq!(
            frame,
            Area {
                x: 160.0 - SPRITE_BOX / 2.0,
                y: 100.0 - SPRITE_BOX / 2.0,
                w: SPRITE_BOX,
                h: SPRITE_BOX
            }
        );
        // The well's top-left maps to the top-left of the AppKit frame.
        assert_eq!(sprite_frame((0.0, 0.0), 200.0).y, 200.0 - SPRITE_BOX / 2.0);
    }

    #[test]
    fn the_sprite_follows_the_window_and_the_well_not_just_new_renders() {
        let well = (320.0, 200.0);
        let point = Some((500.0, 350.0));
        let before = sprite_placement(Some(WINDOW), point, well).unwrap();
        // The window moves 30 pt right with no new cursor render: the same
        // screen point is now 10 pt (a third) further left in the well.
        let moved = Area { x: WINDOW.x + 30.0, ..WINDOW };
        let after = sprite_placement(Some(moved), point, well).unwrap();
        assert!((before.x - after.x - 10.0).abs() < 1e-9, "{before:?} {after:?}");
        assert_eq!(before.y, after.y);
        // It moves so far the point is outside: the sprite hides.
        let far = Area { x: 600.0, ..WINDOW };
        assert_eq!(sprite_placement(Some(far), point, well), None);
        // The well resizes: the sprite is re-placed for the new scale.
        let bigger = sprite_placement(Some(WINDOW), point, (640.0, 400.0)).unwrap();
        assert!((bigger.x + SPRITE_BOX / 2.0 - 2.0 * (before.x + SPRITE_BOX / 2.0)).abs() < 1e-9);
        // No window frame or no cursor point: hidden.
        assert_eq!(sprite_placement(None, point, well), None);
        assert_eq!(sprite_placement(Some(WINDOW), None, well), None);
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
