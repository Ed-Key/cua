//! macOS window enumeration via CGWindowList APIs.
//!
//! Uses the C-level CGWindowListCopyWindowInfo API which returns a CFArray
//! of CFDictionary objects describing each window.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub layer: i32,
    pub z_index: usize,
    pub is_on_screen: bool,
    /// Active Space on the display WindowServer associates with this window.
    /// This can differ between windows when displays use independent Spaces.
    pub current_space_id: Option<u64>,
    pub on_current_space: Option<bool>,
    pub space_ids: Option<Vec<u64>>,
}

pub(crate) struct WindowEnumeration {
    pub(crate) windows: Vec<WindowInfo>,
    pub(crate) current_space_id: Option<u64>,
}

// ── CGWindow option flags ─────────────────────────────────────────────────────
// Apple-canonical kCG* naming preserved to match the public Apple headers — the
// upper-case-globals lint would rename them to KCG_..., which would silently
// shadow the Apple-namespaced constant references in any future code that
// re-introduces them. Mirrors platform-windows::uia/windows_enum.rs which uses
// the same allow for UIA_* constants.
#[allow(non_upper_case_globals)]
const kCGWindowListExcludeDesktopElements: u32 = 16;
#[allow(non_upper_case_globals)]
const kCGWindowListOptionOnScreenOnly: u32 = 1;
#[allow(non_upper_case_globals)]
const kCGNullWindowID: u32 = 0;

// ── Internal CGWindowInfo parsing ─────────────────────────────────────────────
//
// We use `system_profiler` workaround via `CGWindowListCopyWindowInfo` which
// returns a plist-like structure. The simplest cross-compile-safe approach
// is to dump via `osascript` or use the Objective-C runtime.
//
// For the initial version we use the `core-foundation` crate + direct C linkage.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relativeToWindow: u32,
    ) -> core_foundation::array::CFArrayRef;
}

/// Enumerate all windows (including off-screen).
pub fn all_windows() -> Vec<WindowInfo> {
    all_windows_with_space_snapshot().windows
}

pub(crate) fn all_windows_with_space_snapshot() -> WindowEnumeration {
    enumerate_windows(kCGWindowListExcludeDesktopElements, LayerFilter::ZeroOnly)
}

/// Enumerate only on-screen windows.
pub fn visible_windows() -> Vec<WindowInfo> {
    visible_windows_with_space_snapshot().windows
}

pub(crate) fn visible_windows_with_space_snapshot() -> WindowEnumeration {
    enumerate_windows(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        LayerFilter::ZeroOnly,
    )
}

/// Enumerate windows on every CGWindow layer, including the accessory layers
/// (`layer != 0`) that [`all_windows`] hides.
///
/// Only used to answer "does this CGWindowID exist, and who owns it?" — the
/// question `list_windows` must NOT answer, because surfacing tooltips,
/// popovers, the Dock and every NSMenu window would swamp callers. Keeping the
/// layer filter on enumeration and off identity lookup is what lets
/// `get_window_state` tell "no such window" apart from "exists, but is not a
/// layer-0 window" (issue #2237).
pub(crate) fn all_windows_any_layer() -> Vec<WindowInfo> {
    enumerate_windows(kCGWindowListExcludeDesktopElements, LayerFilter::AnyLayer).windows
}

/// On-screen windows on every layer that WindowServer composites with a
/// non-zero alpha, front to back. Only on-screen windows of the current
/// Space(s) are listed. Used by the PiP preview to decide whether a window
/// is fully visible to the user.
pub(crate) fn composited_windows() -> Vec<WindowInfo> {
    enumerate_windows(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        LayerFilter::Composited,
    )
    .windows
}

/// CGWindow layers from here up are system chrome (Dock 20, main menu 24,
/// status items 25, pop-up menus 101). The Dock and Notification Center own
/// display-sized, mostly transparent windows there that do not take a click
/// meant for an app; smaller chrome (the menu bar, a menu, a banner) does.
const SYSTEM_LAYER: i32 = 20;

/// Owners of display-sized, click-through chrome windows. Any other app's
/// display-sized window above the normal level (a full-screen overlay) takes
/// the click and is an occluder.
fn is_system_chrome(app_name: &str) -> bool {
    matches!(
        app_name,
        "Dock" | "Notification Center" | "Window Server" | "Control Center" | "WindowManager"
    )
}

/// `kCGCursorWindowLevel`. WindowServer draws the pointer ("Cursor", when it
/// is composited as a window) and the capture and microphone dots
/// ("StatusIndicator") in windows of its own at this level.
const CURSOR_LAYER: i32 = 2_147_483_630;

/// Only a window that can receive a click covers a target. WindowServer's
/// own windows at the cursor level are images drawn over everything: a click
/// passes through them to the window beneath.
fn receives_no_clicks(window: &WindowInfo) -> bool {
    window.app_name == "Window Server" && window.layer >= CURSOR_LAYER
}

/// Who a pointer event at a screen point reaches.
#[derive(Debug)]
pub(crate) enum PointOwner<'a> {
    /// The target window, a window of its own (a sheet, a child window), or
    /// one of the target app's menus.
    Target,
    /// Another window is topmost at the point.
    Other(&'a WindowInfo),
    /// No window at the point.
    Nothing,
}

/// The owner of the topmost window at `(x, y)` in `front_to_back` (composited
/// on-screen windows in WindowServer order). WindowServer's pointer and
/// indicator images, display-sized system chrome and
/// the driver's own click-through windows (`own_click_through`: the cursor
/// overlay ignores mouse events) are transparent to the check; the driver's
/// interactive windows (PiP panels, the overview) take a click like any
/// other window and cover the target. `displays` are display bounds in the
/// same coordinates.
// ponytail: a display-sized chrome window is treated as click-through, so a
// point over the Dock bar (drawn inside the Dock's full-screen window) is not
// caught; hit-test the Dock's own geometry if that case shows up.
pub(crate) fn point_owner<'a>(
    front_to_back: &'a [WindowInfo],
    displays: &[WindowBounds],
    (x, y): (f64, f64),
    target_pid: i32,
    own_pid: i32,
    own_click_through: impl Fn(u32) -> bool,
    belongs_to_target: impl Fn(u32) -> bool,
) -> PointOwner<'a> {
    let covers_a_display = |b: &WindowBounds| {
        displays.iter().any(|d| {
            b.x <= d.x + 1.0
                && b.y <= d.y + 1.0
                && b.x + b.width >= d.x + d.width - 1.0
                && b.y + b.height >= d.y + d.height - 1.0
        })
    };
    for window in front_to_back {
        let b = &window.bounds;
        let inside = x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height;
        if !inside
            || receives_no_clicks(window)
            || (window.pid == own_pid && own_click_through(window.window_id))
        {
            continue;
        }
        if belongs_to_target(window.window_id)
            || (window.pid == target_pid && window.layer >= SYSTEM_LAYER)
        {
            return PointOwner::Target;
        }
        if window.layer >= SYSTEM_LAYER && covers_a_display(b) && is_system_chrome(&window.app_name) {
            continue;
        }
        return PointOwner::Other(window);
    }
    PointOwner::Nothing
}

/// Which CGWindow layers an enumeration admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerFilter {
    /// Normal application windows only — what `list_windows` reports.
    ZeroOnly,
    /// Every layer, accessory windows included.
    AnyLayer,
    /// Every layer, minus fully transparent (alpha 0) windows.
    Composited,
}

fn enumerate_windows(options: u32, layers: LayerFilter) -> WindowEnumeration {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFTypeRef, TCFType},
        boolean::CFBoolean,
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let space_query = (layers == LayerFilter::ZeroOnly)
        .then(crate::input::skylight::SpaceQuery::new)
        .flatten();
    let current_space_id = space_query
        .as_ref()
        .and_then(|query| query.current_space_id());

    let raw_ref = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw_ref.is_null() {
        return WindowEnumeration {
            windows: vec![],
            current_space_id,
        };
    }

    let raw: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(raw_ref as _) };
    let total = raw.len() as usize;
    let mut results = Vec::new();

    for (idx, item) in raw.iter().enumerate() {
        let item = *item;
        // Each item should be a CFDictionary.
        let dict_type = CFDictionary::<*const c_void, *const c_void>::type_id();
        if unsafe { CFGetTypeID(item) } != dict_type {
            continue;
        }

        let dict: CFDictionary<*const c_void, *const c_void> =
            unsafe { CFDictionary::wrap_under_get_rule(item as _) };

        // Helper: get number from dict by key string.
        let get_num = |key: &str| -> i64 {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFNumber::type_id() {
                        CFNumber::wrap_under_get_rule(v as _).to_i64()
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        };

        let get_str = |key: &str| -> String {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFString::type_id() {
                        Some(CFString::wrap_under_get_rule(v as _).to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };

        let get_bool = |key: &str| -> bool {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .map(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFBoolean::type_id() {
                        bool::from(CFBoolean::wrap_under_get_rule(v as _))
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        };

        let window_id = get_num("kCGWindowNumber") as u32;
        let pid = get_num("kCGWindowOwnerPID") as i32;
        let app_name = get_str("kCGWindowOwnerName");
        let title = get_str("kCGWindowName");
        let layer = get_num("kCGWindowLayer") as i32;
        let is_on_screen = get_bool("kCGWindowIsOnscreen");

        // Only include layer-0 windows, unless the caller asked for every layer.
        if layer != 0 && layers == LayerFilter::ZeroOnly {
            continue;
        }
        if layers == LayerFilter::Composited && get_bounds_num(&dict, "kCGWindowAlpha") <= 0.0 {
            continue;
        }

        // Parse bounds dict.
        let bounds = {
            let bk = CFString::new("kCGWindowBounds");
            dict.find(bk.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFDictionary::<*const c_void, *const c_void>::type_id() {
                        let bd: CFDictionary<*const c_void, *const c_void> =
                            CFDictionary::wrap_under_get_rule(v as _);
                        let x = get_bounds_num(&bd, "X");
                        let y = get_bounds_num(&bd, "Y");
                        let w = get_bounds_num(&bd, "Width");
                        let h = get_bounds_num(&bd, "Height");
                        Some(WindowBounds {
                            x,
                            y,
                            width: w,
                            height: h,
                        })
                    } else {
                        None
                    }
                })
                .unwrap_or(WindowBounds {
                    x: 0.,
                    y: 0.,
                    width: 0.,
                    height: 0.,
                })
        };

        // z_index: CGWindowList front-to-back → assign reverse index.
        let z_index = cua_driver_core::window_target::z_index_from_front_to_back(total, idx);

        results.push(WindowInfo {
            window_id,
            pid,
            app_name,
            title,
            bounds,
            layer,
            z_index,
            is_on_screen,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        });
    }

    if layers == LayerFilter::ZeroOnly {
        let Some(query) = &space_query else {
            return WindowEnumeration {
                windows: results,
                current_space_id,
            };
        };
        for window in &mut results {
            let space_ids = query.window_space_ids(window.window_id);
            let display_space_id = space_ids
                .as_ref()
                .and_then(|_| query.current_space_for_window(window.window_id));
            apply_window_space_metadata(window, space_ids, display_space_id);
        }
    }

    WindowEnumeration {
        windows: results,
        current_space_id,
    }
}

fn apply_window_space_metadata(
    window: &mut WindowInfo,
    space_ids: Option<Vec<u64>>,
    current_space_id: Option<u64>,
) {
    window.on_current_space = window_on_current_space(space_ids.as_deref(), current_space_id);
    window.current_space_id = current_space_id;
    window.space_ids = space_ids;
}

fn window_on_current_space(
    space_ids: Option<&[u64]>,
    current_space_id: Option<u64>,
) -> Option<bool> {
    Some(space_ids?.contains(&current_space_id?))
}

fn get_bounds_num(
    dict: &core_foundation::dictionary::CFDictionary<
        *const std::os::raw::c_void,
        *const std::os::raw::c_void,
    >,
    key: &str,
) -> f64 {
    use core_foundation::{
        base::{CFGetTypeID, TCFType},
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let k = CFString::new(key);
    dict.find(k.as_concrete_TypeRef() as *const c_void)
        .and_then(|v| unsafe {
            let v = *v;
            if CFGetTypeID(v) == CFNumber::type_id() {
                CFNumber::wrap_under_get_rule(v as _).to_f64()
            } else {
                None
            }
        })
        .unwrap_or(0.0)
}

/// Look up a window by its CGWindowID across every layer.
///
/// Returns `None` only when WindowServer has no record of the id at all —
/// which is precisely the "closed or fabricated window_id" signal callers need.
pub fn window_info_by_id(window_id: u32) -> Option<WindowInfo> {
    all_windows_any_layer()
        .into_iter()
        .find(|w| w.window_id == window_id)
}

/// Whether `window_id` is on the current Space of its display. `None` when
/// WindowServer does not know the window or Space membership is unreadable.
pub fn window_on_current_space_by_id(window_id: u32) -> Option<bool> {
    all_windows()
        .into_iter()
        .find(|w| w.window_id == window_id)?
        .on_current_space
}

/// Look up a window's bounds by its CGWindowID.
///
/// Returns `None` if the window is not currently known to WindowServer
/// (e.g. it was closed or the window_id is stale).
pub fn window_bounds_by_id(window_id: u32) -> Option<WindowBounds> {
    window_info_by_id(window_id).map(|w| w.bounds)
}

/// Who owns a requested CGWindowID, as seen by a caller that asked about `pid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOwner {
    /// The window exists and `pid` owns it.
    SamePid,
    /// The window exists, but a different process owns it. macOS hosts a
    /// sandboxed app's Open/Save panel in
    /// `com.apple.appkit.xpc.openAndSavePanelService`, so the panel's
    /// CGWindowID belongs to that service and not to the app that opened it
    /// (issue #2237).
    ForeignPid {
        owner_pid: i32,
        owner_app_name: String,
    },
    /// WindowServer has no record of the id — closed, stale, or fabricated.
    Unknown,
}

/// Pure form of [`resolve_window_owner`] over an already-enumerated window
/// list, so the ownership decision is testable without a WindowServer.
pub fn resolve_window_owner_in(windows: &[WindowInfo], pid: i32, window_id: u32) -> WindowOwner {
    match windows.iter().find(|w| w.window_id == window_id) {
        None => WindowOwner::Unknown,
        Some(w) if w.pid == pid => WindowOwner::SamePid,
        Some(w) => WindowOwner::ForeignPid {
            owner_pid: w.pid,
            owner_app_name: w.app_name.clone(),
        },
    }
}

/// Resolve whether `pid` really owns `window_id`. Blocking (one CGWindowList
/// enumeration).
pub fn resolve_window_owner(pid: i32, window_id: u32) -> WindowOwner {
    resolve_window_owner_in(&all_windows_any_layer(), pid, window_id)
}

/// Select the best window_id for a pid.
pub fn resolve_main_window_id(pid: i32) -> anyhow::Result<u32> {
    let windows = all_windows();
    let pid_windows: Vec<&WindowInfo> = windows.iter().filter(|w| w.pid == pid).collect();
    if pid_windows.is_empty() {
        anyhow::bail!("pid {pid} has no windows");
    }
    let mut on_screen: Vec<&&WindowInfo> = pid_windows.iter().filter(|w| w.is_on_screen).collect();
    if !on_screen.is_empty() {
        on_screen.sort_by_key(|window| std::cmp::Reverse(window.z_index));
        return Ok(on_screen[0].window_id);
    }
    let largest = pid_windows.iter().max_by(|a, b| {
        let area_a = a.bounds.width * a.bounds.height;
        let area_b = b.bounds.width * b.bounds.height;
        area_a
            .partial_cmp(&area_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(largest.unwrap().window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(window_id: u32, pid: i32, layer: i32, (x, y, width, height): (f64, f64, f64, f64)) -> WindowInfo {
        WindowInfo {
            window_id,
            pid,
            app_name: format!("app{pid}"),
            title: String::new(),
            bounds: WindowBounds { x, y, width, height },
            layer,
            z_index: 0,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    const SCREENS: [WindowBounds; 1] = [WindowBounds { x: 0.0, y: 0.0, width: 2000.0, height: 2000.0 }];

    /// The Stocks row was covered by a Finder window: a HID click there
    /// reached Finder while the result said the target was activated.
    #[test]
    fn the_topmost_window_at_a_point_decides_who_gets_a_click() {
        const TARGET: u32 = 10;
        let belongs = |id: u32| id == TARGET || id == 11; // 11: the target's sheet
        let click_through = |id: u32| id == 1; // the driver's cursor overlay
        let cursor = at(1, 7, 0, (0.0, 0.0, 2000.0, 2000.0));
        let dock = WindowInfo { app_name: "Dock".into(), ..at(2, 50, 20, (0.0, 0.0, 2000.0, 2000.0)) };
        let finder = at(3, 60, 0, (300.0, 300.0, 400.0, 300.0));
        let target = at(TARGET, 40, 0, (200.0, 200.0, 800.0, 600.0));
        let windows = [cursor.clone(), dock.clone(), finder.clone(), target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (400.0, 400.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 3, .. })
        ));
        assert!(matches!(point_owner(&windows, &SCREENS, (900.0, 700.0), 40, 7, click_through, belongs), PointOwner::Target));
        assert!(matches!(point_owner(&windows, &SCREENS, (100.0, 100.0), 40, 7, click_through, belongs), PointOwner::Nothing));
        // The target's own sheet and its open menu (layer 101) count as the target.
        let sheet = at(11, 40, 0, (300.0, 300.0, 400.0, 300.0));
        let menu = at(12, 40, 101, (300.0, 300.0, 200.0, 200.0));
        let windows = [menu, sheet, finder.clone(), target.clone()];
        assert!(matches!(point_owner(&windows, &SCREENS, (350.0, 350.0), 40, 7, click_through, belongs), PointOwner::Target));
        assert!(matches!(point_owner(&windows, &SCREENS, (600.0, 550.0), 40, 7, click_through, belongs), PointOwner::Target));
        // Smaller chrome (the menu bar, another app's menu, a banner) covers it.
        let menu_bar = at(4, 50, 24, (0.0, 0.0, 2000.0, 30.0));
        let other_menu = at(5, 60, 101, (850.0, 650.0, 100.0, 100.0));
        let windows = [menu_bar, other_menu, target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (900.0, 700.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 5, .. })
        ));
        assert!(matches!(
            point_owner(&windows, &SCREENS, (900.0, 10.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 4, .. })
        ));
        // Another app's full-screen overlay above the normal level takes clicks.
        let overlay = at(6, 70, 25, (0.0, 0.0, 2000.0, 2000.0));
        let windows = [overlay, target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (900.0, 700.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 6, .. })
        ));
        // The driver's own PiP panel takes mouse input: it covers the target,
        // while the click-through cursor overlay above it does not.
        let pip = at(20, 7, 3, (850.0, 650.0, 200.0, 150.0));
        let windows = [cursor.clone(), pip, target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (900.0, 700.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 20, pid: 7, .. })
        ));
        assert!(matches!(point_owner(&windows, &SCREENS, (400.0, 400.0), 40, 7, click_through, belongs), PointOwner::Target));
        // The real pointer sitting on the point (WindowServer's "Cursor"
        // window) and the capture indicator are images, not click targets;
        // another app's window at that level still takes the click.
        let server = |id, name: &str, frame| WindowInfo {
            app_name: "Window Server".into(),
            title: name.into(),
            ..at(id, 175, CURSOR_LAYER, frame)
        };
        let pointer = server(16, "Cursor", (0.0, 0.0, 2000.0, 2000.0));
        let indicator = server(14, "StatusIndicator", (880.0, 680.0, 29.0, 29.0));
        let windows = [pointer.clone(), indicator, target.clone()];
        assert!(matches!(point_owner(&windows, &SCREENS, (900.0, 700.0), 40, 7, click_through, belongs), PointOwner::Target));
        // A real cover under the pointer still covers.
        let windows = [pointer, finder.clone(), target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (400.0, 400.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 3, pid: 60, .. })
        ));
        let top_level_app = at(15, 70, CURSOR_LAYER, (890.0, 690.0, 28.0, 40.0));
        let windows = [top_level_app, target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (900.0, 700.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 15, .. })
        ));
        // WindowServer's menu bar (layer 24) does take the click.
        let menu_bar = WindowInfo { app_name: "Window Server".into(), ..at(4, 175, 24, (0.0, 0.0, 2000.0, 30.0)) };
        let windows = [menu_bar, target.clone()];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (900.0, 10.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 4, .. })
        ));
        // Another window of the same app that is not the target's covers it.
        let other_own = at(13, 40, 0, (300.0, 300.0, 400.0, 300.0));
        let windows = [other_own, target];
        assert!(matches!(
            point_owner(&windows, &SCREENS, (400.0, 400.0), 40, 7, click_through, belongs),
            PointOwner::Other(WindowInfo { window_id: 13, .. })
        ));
    }

    #[test]
    fn space_membership_checks_all_spaces_for_a_window() {
        assert_eq!(window_on_current_space(Some(&[2, 4]), Some(4)), Some(true));
        assert_eq!(window_on_current_space(Some(&[2, 4]), Some(3)), Some(false));
    }

    #[test]
    fn space_membership_stays_unknown_without_either_side() {
        assert_eq!(window_on_current_space(None, Some(4)), None);
        assert_eq!(window_on_current_space(Some(&[4]), None), None);
    }

    #[test]
    fn per_window_current_space_is_the_one_used_for_membership() {
        let mut secondary_display_window = window(42, 800, "TextEdit");
        apply_window_space_metadata(&mut secondary_display_window, Some(vec![2, 4]), Some(4));

        assert_eq!(secondary_display_window.current_space_id, Some(4));
        assert_eq!(secondary_display_window.space_ids, Some(vec![2, 4]));
        assert_eq!(secondary_display_window.on_current_space, Some(true));
        assert!(secondary_display_window
            .space_ids
            .as_deref()
            .is_some_and(|spaces| spaces.contains(
                &secondary_display_window
                    .current_space_id
                    .expect("display Space must be present")
            )));
    }

    fn window(window_id: u32, pid: i32, app_name: &str) -> WindowInfo {
        WindowInfo {
            window_id,
            pid,
            app_name: app_name.into(),
            title: String::new(),
            bounds: WindowBounds {
                x: 0.,
                y: 580.,
                width: 500.,
                height: 500.,
            },
            layer: 0,
            z_index: 1,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn owner_resolves_same_pid() {
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 42),
            WindowOwner::SamePid
        );
    }

    /// Issue #2237: TextEdit's Open panel is a layer-0 CGWindow owned by
    /// `com.apple.appkit.xpc.openAndSavePanelService`, not by TextEdit. The
    /// caller must be told the real owner pid, not handed TextEdit's menu bar.
    #[test]
    fn owner_detects_out_of_process_panel_host() {
        let windows = vec![
            window(41, 800, "TextEdit"),
            window(42, 900, "Open and Save Panel Service"),
        ];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 42),
            WindowOwner::ForeignPid {
                owner_pid: 900,
                owner_app_name: "Open and Save Panel Service".into(),
            }
        );
    }

    #[test]
    fn owner_is_unknown_for_fabricated_id() {
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 0xFFFF_FFF0),
            WindowOwner::Unknown
        );
    }

    #[test]
    fn owner_is_unknown_for_zero_id() {
        // kCGNullWindowID is never a real window number.
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 0),
            WindowOwner::Unknown
        );
    }

    #[test]
    fn owner_is_unknown_after_the_window_closes() {
        // Stale id: it was enumerated once, then the panel was dismissed.
        let before = vec![window(42, 900, "Open and Save Panel Service")];
        assert_eq!(
            resolve_window_owner_in(&before, 900, 42),
            WindowOwner::SamePid
        );
        assert_eq!(resolve_window_owner_in(&[], 900, 42), WindowOwner::Unknown);
    }
}
