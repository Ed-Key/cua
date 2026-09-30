//! "Is the agent's window already fully visible to the user?" A session's
//! panel hides while the answer is yes: the user can watch the window
//! itself, so a mirror of it would only cover something else.

use crate::windows::WindowInfo;

use super::{capture_window, Area, Target};

/// Windows on this CGWindow layer and above are system chrome (Dock 20,
/// main menu 24, status items 25, pop-up menus 101, ...). The Dock and the
/// menu bar own screen-sized transparent windows, so counting them would
/// make every window look covered.
const SYSTEM_LAYER: i32 = 20;

/// Whether `window_id` is fully visible: listed on screen (so on the current
/// Space and not minimized), entirely inside one display, and not
/// intersected by any window above it in z-order. `front_to_back` is the
/// on-screen window list in WindowServer order; windows owned by `own_pid`
/// (the PiP panels and the cursor overlay) and system-chrome layers are
/// ignored. `displays` are display bounds in the same top-left-origin
/// global coordinates as window bounds.
///
/// A window spanning two displays counts as not fully visible; that errs
/// toward showing the panel, which is the safe side.
pub(super) fn fully_visible(
    window_id: u32,
    front_to_back: &[WindowInfo],
    displays: &[Area],
    own_pid: i32,
) -> bool {
    let Some(index) = front_to_back
        .iter()
        .position(|window| window.window_id == window_id)
    else {
        return false;
    };
    let target = &front_to_back[index];
    let rect = area(target);
    if !target.is_on_screen || !displays.iter().any(|display| contains(display, &rect)) {
        return false;
    }
    !front_to_back[..index].iter().any(|above| {
        above.pid != own_pid
            && (0..SYSTEM_LAYER).contains(&above.layer)
            && overlaps(&area(above), &rect)
    })
}

/// `fully_visible` for a PiP target. A pid-only target resolves to the
/// pid's frontmost normal window in the same snapshot.
pub(super) fn target_fully_visible(
    target: Target,
    front_to_back: &[WindowInfo],
    displays: &[Area],
    own_pid: i32,
) -> bool {
    let window_id = capture_window(target, |pid| {
        front_to_back
            .iter()
            .find(|window| window.pid == pid && window.layer == 0)
            .map(|window| window.window_id)
    });
    window_id.is_some_and(|id| fully_visible(id, front_to_back, displays, own_pid))
}

/// One WindowServer snapshot: composited on-screen windows front to back,
/// and the active displays' bounds.
pub(super) fn snapshot() -> (Vec<WindowInfo>, Vec<Area>) {
    use core_graphics::display::CGDisplay;
    let displays = CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let bounds = CGDisplay::new(id).bounds();
            Area {
                x: bounds.origin.x,
                y: bounds.origin.y,
                w: bounds.size.width,
                h: bounds.size.height,
            }
        })
        .collect();
    (crate::windows::composited_windows(), displays)
}

/// Every window WindowServer knows, on screen or off (any Space, minimized
/// included). `None` when the lookup failed: an empty answer is that, not
/// every window closing.
pub(super) fn known_windows() -> Option<std::collections::HashSet<u32>> {
    let known: std::collections::HashSet<u32> = crate::windows::all_windows_any_layer()
        .iter()
        .map(|window| window.window_id)
        .collect();
    (!known.is_empty()).then_some(known)
}

/// The frame of `window` in `windows` (CoreGraphics, top-left origin),
/// tagged with that window's id so it is never taken for another's.
pub(super) fn frame_of(windows: &[WindowInfo], window: Option<u32>) -> Option<(u32, Area)> {
    let id = window?;
    windows
        .iter()
        .find(|candidate| candidate.window_id == id)
        .map(|found| (id, area(found)))
}

pub(super) fn area(window: &WindowInfo) -> Area {
    Area {
        x: window.bounds.x,
        y: window.bounds.y,
        w: window.bounds.width,
        h: window.bounds.height,
    }
}

fn contains(outer: &Area, inner: &Area) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.w <= outer.x + outer.w
        && inner.y + inner.h <= outer.y + outer.h
}

/// Positive-area intersection; windows that only touch do not overlap.
fn overlaps(a: &Area, b: &Area) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowBounds;

    const OWN: i32 = 1;
    const SCREEN: Area = Area {
        x: 0.0,
        y: 0.0,
        w: 1512.0,
        h: 982.0,
    };

    fn window(id: u32, pid: i32, layer: i32, (x, y, w, h): (f64, f64, f64, f64)) -> WindowInfo {
        WindowInfo {
            window_id: id,
            pid,
            app_name: String::new(),
            title: String::new(),
            bounds: WindowBounds {
                x,
                y,
                width: w,
                height: h,
            },
            layer,
            z_index: 0,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    /// Target 7 (pid 42) with the real-world system chrome above it: the
    /// menu bar strip and the Dock's screen-sized window.
    fn desk(above: Vec<WindowInfo>) -> Vec<WindowInfo> {
        let mut list = vec![
            window(90, 500, 25, (1200.0, 0.0, 40.0, 33.0)),
            window(91, 501, 24, (0.0, 0.0, 1512.0, 33.0)),
            window(92, 502, 20, (0.0, 0.0, 1512.0, 982.0)),
        ];
        list.extend(above);
        list.push(window(7, 42, 0, (100.0, 100.0, 800.0, 600.0)));
        list.push(window(8, 43, 0, (0.0, 33.0, 1512.0, 900.0))); // below: irrelevant
        list
    }

    #[test]
    fn an_uncovered_window_is_fully_visible_despite_dock_and_menu_bar() {
        assert!(fully_visible(7, &desk(vec![]), &[SCREEN], OWN));
    }

    #[test]
    fn a_window_above_that_overlaps_covers_it() {
        let list = desk(vec![window(9, 44, 0, (850.0, 650.0, 200.0, 200.0))]);
        assert!(!fully_visible(7, &list, &[SCREEN], OWN));
        // A floating panel of another app (layer 3) covers too.
        let list = desk(vec![window(9, 44, 3, (120.0, 120.0, 50.0, 50.0))]);
        assert!(!fully_visible(7, &list, &[SCREEN], OWN));
    }

    #[test]
    fn a_window_above_that_only_touches_or_misses_does_not_cover_it() {
        let list = desk(vec![
            window(9, 44, 0, (900.0, 100.0, 200.0, 200.0)), // touches the right edge
            window(10, 45, 0, (1000.0, 700.0, 300.0, 200.0)),
        ]);
        assert!(fully_visible(7, &list, &[SCREEN], OWN));
    }

    #[test]
    fn pip_panels_and_the_cursor_overlay_are_ignored() {
        let list = desk(vec![
            window(11, OWN, 3, (150.0, 150.0, 336.0, 258.0)), // a PiP panel
            window(12, OWN, 0, (0.0, 0.0, 1512.0, 982.0)),    // the overlay
        ]);
        assert!(fully_visible(7, &list, &[SCREEN], OWN));
    }

    #[test]
    fn a_partially_off_screen_window_is_not_fully_visible() {
        let mut list = desk(vec![]);
        list.iter_mut().find(|w| w.window_id == 7).unwrap().bounds.x = 1000.0;
        assert!(!fully_visible(7, &list, &[SCREEN], OWN));
        // Fully on a second display is fine.
        let right = Area {
            x: 1512.0,
            ..SCREEN
        };
        list.iter_mut().find(|w| w.window_id == 7).unwrap().bounds.x = 1600.0;
        assert!(fully_visible(7, &list, &[SCREEN, right], OWN));
    }

    #[test]
    fn a_window_on_another_space_or_minimized_is_not_fully_visible() {
        // Off the current Space: absent from the on-screen list.
        let list: Vec<_> = desk(vec![])
            .into_iter()
            .filter(|w| w.window_id != 7)
            .collect();
        assert!(!fully_visible(7, &list, &[SCREEN], OWN));
        let mut list = desk(vec![]);
        list.iter_mut()
            .find(|w| w.window_id == 7)
            .unwrap()
            .is_on_screen = false;
        assert!(!fully_visible(7, &list, &[SCREEN], OWN));
    }

    #[test]
    fn a_pid_only_target_uses_the_pids_frontmost_window() {
        let list = desk(vec![]);
        assert!(target_fully_visible(
            (Some(42), None),
            &list,
            &[SCREEN],
            OWN
        ));
        // pid 43's window sits below 7 and overlaps it.
        assert!(!target_fully_visible(
            (Some(43), None),
            &list,
            &[SCREEN],
            OWN
        ));
        assert!(!target_fully_visible((None, None), &list, &[SCREEN], OWN));
    }
}
