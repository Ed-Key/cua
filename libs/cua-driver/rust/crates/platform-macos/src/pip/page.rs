//! The page of a bound browser tab inside its window: a front card whose
//! frame came from a bound tab (`PipFrame::page`) shows only this area, not
//! the browser's tab strip, toolbar or infobars (like the "started debugging
//! this browser" banner).
//!
//! The area is the window's one top-level `AXWebArea`, read through
//! Accessibility and validated against the window's frame. Chrome builds its
//! pages' accessibility (that element with it) only once an assistive client
//! asks for it: until then its page is an unmarked group among the window's
//! views. A lookup that finds no page asks for it the way the driver's tree
//! walker does (`ensure_chromium_ax_enabled`: once per browser process, the
//! ask then waiting seconds for the tree, so on a thread of its own). Chrome
//! takes about two seconds to build it: until then the card is the whole
//! window, then the poll finds the page and the card takes its shape. It is looked up
//! only on the PiP's own workers (the capture helper, for an action's still,
//! and the visibility poll), each lookup bounded by `LOOKUP_BUDGET`, never on
//! the main queue and never awaited by a browser action. Every lookup starts
//! from the window again, so a navigation, a tab switch, a banner coming or
//! going or docked DevTools never leave a stale element behind. Anything
//! unknown, ambiguous or late is an `Err` with its reason: the card then
//! shows the whole window, never a guess.

use std::time::{Duration, Instant};

use core_foundation::base::{CFRelease, CFTypeRef};

use super::Area;
use crate::ax::bindings::{
    ax_get_window_id, copy_ax_windows_including, copy_children_reporting,
    copy_geometry_attr_checked, copy_string_attr, copy_url_attr, kAXValueCGPointType,
    kAXValueCGSizeType, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};
use crate::ax::enablement::ensure_chromium_ax_enabled;

/// Longest one lookup may take (it messages the browser over AX).
const LOOKUP_BUDGET: Duration = Duration::from_millis(300);
/// The lookup found no page and asked the browser to build its page's
/// accessibility.
const ASKED: &str = "the window shows no page yet (its accessibility was asked for)";
/// The page sits a few levels under the window; its own content is never
/// walked, so these only bound the browser's native views.
const MAX_DEPTH: u32 = 12;
const MAX_NODES: u32 = 400;
/// A page smaller than this (points) is not worth a card of its own.
const MIN_SIDE: f64 = 40.0;
/// AX and WindowServer round differently at the window's edges.
const EDGE_SLACK: f64 = 1.0;

/// The page's area in its window's points (origin at the window's top-left
/// corner), or why it is not known.
pub(super) type Crop = Result<Area, &'static str>;

/// `page` (screen points) as a crop of `window` (screen points): inside the
/// window (within a point of slack, then clamped to it) and not trivially
/// small.
pub(super) fn crop_in_window(window: Area, page: Area) -> Crop {
    let crop = Area {
        x: page.x - window.x,
        y: page.y - window.y,
        w: page.w,
        h: page.h,
    };
    if !(crop.w >= MIN_SIDE && crop.h >= MIN_SIDE) {
        return Err("the page is too small");
    }
    if crop.x < -EDGE_SLACK
        || crop.y < -EDGE_SLACK
        || crop.x + crop.w > window.w + EDGE_SLACK
        || crop.y + crop.h > window.h + EDGE_SLACK
    {
        return Err("the page is not inside its window");
    }
    let (x, y) = (crop.x.max(0.0), crop.y.max(0.0));
    Ok(Area {
        x,
        y,
        w: (crop.x + crop.w).min(window.w) - x,
        h: (crop.y + crop.h).min(window.h) - y,
    })
}

/// The pixel rectangle (x, y, w, h) of `crop` in a capture of `window` that
/// is `image` pixels: the capture's own scale, the same on both axes. `None`
/// when the capture does not match the window's shape (it moved to another
/// display or was resized in between), or the rectangle is empty.
pub(super) fn pixel_rect(crop: Area, window: Area, image: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let sx = f64::from(image.0) / window.w;
    let sy = f64::from(image.1) / window.h;
    if !(sx.is_finite() && sy.is_finite() && sx >= 0.5) || (sx - sy).abs() > 0.02 * sx {
        return None;
    }
    let left = (crop.x * sx).round().max(0.0);
    let top = (crop.y * sx).round().max(0.0);
    let right = ((crop.x + crop.w) * sx).round().min(f64::from(image.0));
    let bottom = ((crop.y + crop.h) * sx).round().min(f64::from(image.1));
    (right - left >= 2.0 && bottom - top >= 2.0).then(|| {
        (
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        )
    })
}

/// `png` (a capture of `window`) cropped to `crop`.
fn crop_png(png: &[u8], crop: Area, window: Area) -> Option<Vec<u8>> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
    let (x, y, w, h) = pixel_rect(crop, window, (image.width(), image.height()))?;
    let mut out = Vec::new();
    let encoder = PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive);
    image.crop_imm(x, y, w, h).write_with_encoder(encoder).ok()?;
    Some(out)
}

/// The window's frame (screen points) from WindowServer.
fn window_frame(window_id: u32) -> Option<Area> {
    crate::windows::window_info_by_id(window_id)
        .as_ref()
        .map(super::visibility::area)
}

/// An action's still of a bound tab's window, framed to its page: the
/// window's capture, and the page's crop looked up with it, applied only
/// when the window kept its frame across the capture and the lookup and
/// the capture's scale matches it. Otherwise the whole window, logged.
/// Capture worker only.
pub(super) fn capture_page(pid: Option<i32>, window_id: u32) -> Option<super::Shot> {
    let before = window_frame(window_id);
    let png = cua_driver_core::recording::screenshot_for(Some(u64::from(window_id)), None)?;
    let framed = before.ok_or("the window's frame is not known").and_then(|frame| {
        let crop = page_crop(pid, window_id, frame)?;
        if window_frame(window_id) != Some(frame) {
            return Err("the window moved or resized during the capture");
        }
        crop_png(&png, crop, frame)
            .map(|cropped| (crop, cropped))
            .ok_or("the capture does not match the window's frame")
    });
    match framed {
        Ok((crop, cropped)) => Some((window_id, cropped, Some(crop))),
        Err(reason) => {
            tracing::info!(target: "pip", window = window_id, reason, "PiP page not framed: the still shows the whole window");
            Some((window_id, png, None))
        }
    }
}

/// The page's crop in `window_id`, whose frame is `window` (screen points),
/// of app `pid`. Capture worker and visibility poll only.
pub(super) fn page_crop(pid: Option<i32>, window_id: u32, window: Area) -> Crop {
    let pid = pid.ok_or("the target has no app")?;
    let started = Instant::now();
    let found = unsafe { page_area(pid, window_id, started + LOOKUP_BUDGET) };
    tracing::debug!(target: "pip", window = window_id, ?found, elapsed_ms = started.elapsed().as_millis() as u64, "PiP page lookup");
    crop_in_window(window, found?)
}

/// Browsers whose page accessibility is being asked for now.
static ASKING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<i32>>> =
    std::sync::LazyLock::new(Default::default);

/// Ask `pid` to build its pages' accessibility, the tree walker's way, on a
/// thread of its own (the ask waits for the tree, up to seconds). The walker's
/// own cache makes a browser it already asked a no-op. Whether an ask is
/// running now.
fn ask_for_pages(pid: i32) -> bool {
    let asking = |f: &dyn Fn(&mut std::collections::HashSet<i32>) -> bool| {
        f(&mut ASKING.lock().unwrap_or_else(|e| e.into_inner()))
    };
    if !asking(&|set| set.insert(pid)) {
        return true;
    }
    let spawned = std::thread::Builder::new()
        .name("cua-pip-ax-ask".into())
        .spawn(move || {
            let started = Instant::now();
            unsafe {
                let app = AXUIElementCreateApplication(pid);
                if !app.is_null() {
                    ensure_chromium_ax_enabled(pid, app);
                    CFRelease(app as CFTypeRef);
                }
            }
            tracing::info!(target: "pip", pid, elapsed_ms = started.elapsed().as_millis() as u64, "PiP asked the browser for its pages' accessibility");
            asking(&|set| set.remove(&pid));
        });
    if spawned.is_err() {
        asking(&|set| set.remove(&pid));
        return false;
    }
    true
}

/// The messaging timeout (seconds) for the next AX message of a lookup that
/// must end by `deadline`: the time left, or `None` once under a millisecond
/// is left (the lookup stops there).
fn message_timeout(deadline: Instant, now: Instant) -> Option<f32> {
    let left = deadline.saturating_duration_since(now);
    (left >= Duration::from_millis(1)).then(|| left.as_secs_f32())
}

/// Bound the next AX message to `element` by the time left before
/// `deadline` (timeouts stick to the element, so this goes before EVERY
/// message). Whether any time is left.
unsafe fn bound(element: AXUIElementRef, deadline: Instant) -> bool {
    match message_timeout(deadline, Instant::now()) {
        Some(seconds) => {
            AXUIElementSetMessagingTimeout(element, seconds);
            true
        }
        None => false,
    }
}

/// The screen area of the one top-level `AXWebArea` in `window_id`.
unsafe fn page_area(pid: i32, window_id: u32, deadline: Instant) -> Result<Area, &'static str> {
    // A reference of our own: messaging timeouts stick to a reference.
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err("the app has no accessibility element");
    }
    let mut root = None;
    if bound(app, deadline) {
        for window in copy_ax_windows_including(app, pid, window_id) {
            if root.is_none() && ax_get_window_id(window) == Some(window_id) {
                root = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
    }
    let Some(root) = root else {
        CFRelease(app as CFTypeRef);
        return Err("the window is not in its app's accessibility tree");
    };
    let mut walk = Walk {
        nodes: MAX_NODES,
        deadline,
        pages: Vec::new(),
        complete: true,
    };
    if walk.bound(root) {
        walk.children(root, MAX_DEPTH);
    }
    CFRelease(root as CFTypeRef);
    // A message that timed out reads as a missing value: past the deadline
    // nothing the walk saw can be trusted to be all of it.
    let found = if !walk.complete || Instant::now() >= deadline {
        Err("the lookup did not finish in time")
    } else {
        match walk.pages[..] {
            [page] => Ok(page),
            [] => {
                Err(if ask_for_pages(pid) {
                    ASKED
                } else {
                    "the window shows no page"
                })
            }
            _ => Err("the window shows more than one page"),
        }
    };
    CFRelease(app as CFTypeRef);
    found
}

struct Walk {
    /// Elements still allowed to be read.
    nodes: u32,
    deadline: Instant,
    /// Areas of the top-level web areas found (DevTools' own left out).
    pages: Vec<Area>,
    /// Every element that was reached was read: `pages` is all of them.
    complete: bool,
}

impl Walk {
    /// `bound` for the walk's next message to `element`; a spent budget ends
    /// the walk incomplete.
    unsafe fn bound(&mut self, element: AXUIElementRef) -> bool {
        self.complete &= bound(element, self.deadline);
        self.complete
    }

    /// Read `element`'s children down to `depth` more levels, never into a
    /// web area. The caller bounded this message.
    unsafe fn children(&mut self, element: AXUIElementRef, depth: u32) {
        let (children, failed) = copy_children_reporting(element);
        if failed {
            self.complete = false;
        }
        for child in children {
            if self.complete {
                self.visit(child, depth);
            }
            CFRelease(child as CFTypeRef);
        }
    }

    unsafe fn visit(&mut self, element: AXUIElementRef, depth: u32) {
        if self.nodes == 0 {
            self.complete = false;
            return;
        }
        self.nodes -= 1;
        if !self.bound(element) {
            return;
        }
        if copy_string_attr(element, "AXRole").as_deref() == Some("AXWebArea") {
            if !self.bound(element) {
                return;
            }
            if copy_url_attr(element).is_some_and(|url| url.starts_with("devtools://")) {
                return;
            }
            match self.area(element) {
                Some(area) => self.pages.push(area),
                None => self.complete = false,
            }
        } else if depth > 0 && self.bound(element) {
            self.children(element, depth - 1);
        }
    }

    /// `element`'s screen area: its position and its size, each message
    /// bounded by the time left.
    unsafe fn area(&mut self, element: AXUIElementRef) -> Option<Area> {
        if !self.bound(element) {
            return None;
        }
        let [x, y] = copy_geometry_attr_checked(element, "AXPosition", kAXValueCGPointType).ok()?;
        if !self.bound(element) {
            return None;
        }
        let [w, h] = copy_geometry_attr_checked(element, "AXSize", kAXValueCGSizeType).ok()?;
        Some(Area { x, y, w, h })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Area = Area {
        x: 100.0,
        y: 30.0,
        w: 1100.0,
        h: 789.0,
    };

    #[test]
    fn every_message_gets_only_the_time_left_and_none_once_it_is_spent() {
        let start = Instant::now();
        let deadline = start + LOOKUP_BUDGET;
        // The first message may take the whole budget ...
        assert_eq!(message_timeout(deadline, start), Some(0.3));
        // ... a later one only what is left, never the first one's timeout
        // again (which is how a slow browser overran the budget).
        let later = message_timeout(deadline, start + Duration::from_millis(250)).unwrap();
        assert!((later - 0.05).abs() < 1e-4, "{later}");
        // Spent (or under a millisecond left): the lookup stops.
        assert_eq!(message_timeout(deadline, deadline - Duration::from_micros(500)), None);
        assert_eq!(message_timeout(deadline, deadline), None);
        assert_eq!(message_timeout(deadline, deadline + Duration::from_secs(1)), None);
    }

    #[test]
    fn a_page_under_the_toolbar_is_cropped_to_window_points() {
        // Chrome's steps page in VM A: 87 pt of tab strip and toolbar.
        let page = Area {
            x: 100.0,
            y: 117.0,
            w: 1100.0,
            h: 702.0,
        };
        assert_eq!(
            crop_in_window(WINDOW, page),
            Ok(Area {
                x: 0.0,
                y: 87.0,
                w: 1100.0,
                h: 702.0
            })
        );
        // A page that fills the window (full screen, no toolbar) is the window.
        let whole = Area {
            y: 30.0,
            h: 789.0,
            ..page
        };
        assert_eq!(
            crop_in_window(WINDOW, whole),
            Ok(Area {
                x: 0.0,
                y: 0.0,
                w: 1100.0,
                h: 789.0
            })
        );
        // Half a point over the edge is rounding: clamped.
        let over = Area { h: 702.5, ..page };
        assert_eq!(crop_in_window(WINDOW, over).map(|crop| crop.h), Ok(702.0));
    }

    #[test]
    fn a_page_outside_its_window_or_too_small_is_unknown() {
        let page = |x, y, w, h| Area { x, y, w, h };
        assert!(crop_in_window(WINDOW, page(90.0, 117.0, 1100.0, 702.0)).is_err());
        assert!(crop_in_window(WINDOW, page(100.0, 117.0, 1100.0, 720.0)).is_err());
        assert!(crop_in_window(WINDOW, page(900.0, 0.0, 300.0, 500.0)).is_err());
        assert!(crop_in_window(WINDOW, page(100.0, 117.0, 1100.0, 20.0)).is_err());
        assert!(crop_in_window(WINDOW, page(100.0, 117.0, f64::NAN, 500.0)).is_err());
    }

    #[test]
    fn the_crop_is_converted_with_the_captures_own_scale() {
        let crop = Area {
            x: 0.0,
            y: 87.0,
            w: 1100.0,
            h: 702.0,
        };
        // A 2x capture of the window.
        assert_eq!(pixel_rect(crop, WINDOW, (2200, 1578)), Some((0, 174, 2200, 1404)));
        // A 1x capture.
        assert_eq!(pixel_rect(crop, WINDOW, (1100, 789)), Some((0, 87, 1100, 702)));
        // The window was resized between the capture and the lookup: its
        // shape no longer matches the image, so no crop.
        assert_eq!(pixel_rect(crop, WINDOW, (2200, 1200)), None);
        assert_eq!(pixel_rect(crop, WINDOW, (0, 0)), None);
    }

    #[test]
    fn a_cropped_still_holds_exactly_the_pages_pixels() {
        // A 2x capture whose top 87 pt are grey toolbar and the rest white page.
        let (w, h) = (220u32, 158u32);
        let window = Area {
            x: 0.0,
            y: 0.0,
            w: 110.0,
            h: 79.0,
        };
        let image = image::RgbaImage::from_fn(w, h, |_, y| {
            if y < 20 {
                image::Rgba([200, 200, 205, 255])
            } else {
                image::Rgba([255, 255, 255, 255])
            }
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let crop = Area {
            x: 0.0,
            y: 10.0,
            w: 110.0,
            h: 69.0,
        };
        let cropped = image::load_from_memory(&crop_png(&png, crop, window).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(cropped.dimensions(), (220, 138));
        assert!(cropped.pixels().all(|px| px.0 == [255, 255, 255, 255]));
    }
}
