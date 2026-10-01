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
//!
//! Cua's own "Cua is working in this tab" pill (extensions/chrome
//! indicator.js, fixed at the page's bottom center) belongs to the tab, not
//! the card: once a lookup has seen it in a window, that window's crop ends
//! just above it, for as long as the driver runs, so the card does not change
//! shape each time the pill hides (4 s after the last command) or comes back.
//! Every lookup looks for it again, so a pill of another size (page zoom)
//! replaces the trim.

use std::time::{Duration, Instant};

use core_foundation::base::{CFRelease, CFRetain, CFTypeRef};

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
/// The lookup ran past its deadline (a busy browser), at whatever step.
const LATE: &str = "the lookup did not finish in time";
/// The page sits a few levels under the window; its own content is never
/// walked, so these only bound the browser's native views.
const MAX_DEPTH: u32 = 12;
const MAX_NODES: u32 = 400;
/// A page smaller than this (points) is not worth a card of its own.
const MIN_SIDE: f64 = 40.0;
/// AX and WindowServer round differently at the window's edges.
const EDGE_SLACK: f64 = 1.0;
/// The pill's label (indicator.js).
const PILL_TEXT: &str = "Cua is working in this tab";
/// The pill is appended last to the page's body, so it is searched for among
/// the last `PILL_TAIL` children at each of `PILL_DEPTH` levels under the
/// page (body, the indicator's host, the pill, its text), at most
/// `PILL_NODES` elements, within the lookup's own budget.
const PILL_TAIL: usize = 6;
const PILL_DEPTH: u32 = 4;
const PILL_NODES: u32 = 32;
/// Room above the pill for its shadow (points).
const PILL_GAP: f64 = 10.0;
/// The extension centers the pill and puts its bottom 16 CSS px above the
/// page's bottom: that, at any of Chrome's zooms (25% to 500%), is what
/// tells the pill from page text quoting its label. The center is the
/// page's less half a classic scrollbar (the page's area includes it, the
/// pill's viewport does not).
const PILL_CENTER_SLACK: f64 = 10.0;
const PILL_BOTTOM: std::ops::RangeInclusive<f64> = 3.0..=82.0;

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
    let started = Instant::now();
    let png = cua_driver_core::recording::screenshot_for(Some(u64::from(window_id)), None);
    tracing::debug!(target: "pip", window = window_id, ok = png.is_some(), elapsed_ms = started.elapsed().as_millis() as u64, "PiP page still captured");
    let png = png?;
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
    let seen = found.ok().and_then(|(page, pill)| pill_trim(page, pill?));
    let (trim, changed) = keep_trim(&mut TRIMS.lock().unwrap_or_else(|e| e.into_inner()), window_id, seen, started);
    if changed {
        tracing::info!(target: "pip", window = window_id, trim, "PiP page pill seen: the card ends above it from now on");
    }
    let crop = found.and_then(|(page, _)| crop_in_window(window, page));
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    remember(&mut known, window_id, (window.w, window.h), crop, started).map(|crop| trimmed(crop, trim))
}

/// How much of `page`'s bottom (screen points) to leave out of the card for
/// cua's pill at `pill` (screen points): from just above the pill (its
/// shadow) to the page's bottom. `None` when the pill is not where the
/// extension draws it (centered, just above the page's bottom) or the rest
/// of the page would be too small.
fn pill_trim(page: Area, pill: Area) -> Option<f64> {
    let centered = ((pill.x + pill.w / 2.0) - (page.x + page.w / 2.0)).abs() <= PILL_CENTER_SLACK;
    let above_bottom = PILL_BOTTOM.contains(&(page.y + page.h - (pill.y + pill.h)));
    let inside = centered && above_bottom;
    let trim = page.y + page.h - (pill.y - PILL_GAP);
    (inside && trim > 0.0 && page.h - trim >= MIN_SIDE).then_some(trim)
}

/// `crop` without its bottom `trim` points, unless that leaves too little.
fn trimmed(crop: Area, trim: Option<f64>) -> Area {
    match trim {
        Some(trim) if crop.h - trim >= MIN_SIDE => Area { h: crop.h - trim, ..crop },
        _ => crop,
    }
}

/// The pill trim of each window that has shown the pill, and when the
/// lookup that saw it started: never dropped (see the module notes).
static TRIMS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u32, (f64, Instant)>>> =
    std::sync::LazyLock::new(Default::default);

/// `window_id`'s trim after a lookup that saw the pill's trim `seen` (or
/// no pill): a pill seen replaces the kept trim (a zoom changed its size),
/// none keeps it, and so does a lookup that started before the one that
/// saw the kept trim (two workers look up concurrently). Whether the kept
/// trim changed.
fn keep_trim(
    trims: &mut std::collections::HashMap<u32, (f64, Instant)>,
    window_id: u32,
    seen: Option<f64>,
    started: Instant,
) -> (Option<f64>, bool) {
    let kept = trims.get(&window_id).copied();
    let newer = kept.is_none_or(|(_, at)| started > at);
    let Some(seen) = seen.filter(|_| newer) else {
        return (kept.map(|(trim, _)| trim), false);
    };
    let changed = kept.is_none_or(|(trim, _)| (trim - seen).abs() > 0.5);
    // ponytail: dropped wholesale past 64 windows, as `remember` does.
    if kept.is_none() && trims.len() >= 64 {
        trims.clear();
    }
    trims.insert(window_id, (seen, started));
    (Some(seen), changed)
}

/// The last answer each window's lookups gave: the window's size then, the
/// crop (`None`: the newest answer was not a page), and when that lookup
/// started.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Known {
    size: (f64, f64),
    crop: Option<Area>,
    started: Instant,
}

static KNOWN: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u32, Known>>> =
    std::sync::LazyLock::new(Default::default);

/// A lookup in `window_id` (its window `size` now) that started at
/// `started` answered `crop`: what the card goes by. A late lookup (a busy
/// browser) keeps the last crop seen in that window at the same size, so
/// the card does not flicker to the whole window and back; with none, or a
/// resized window, it stays unknown. Any other answer replaces the kept
/// crop (a page, no page, several pages), unless a lookup that started
/// later already answered (two workers look up concurrently).
fn remember(
    known: &mut std::collections::HashMap<u32, Known>,
    window_id: u32,
    size: (f64, f64),
    crop: Crop,
    started: Instant,
) -> Crop {
    if crop == Err(LATE) {
        return match known.get(&window_id) {
            Some(Known { size: was, crop: Some(last), .. }) if *was == size => Ok(*last),
            _ => crop,
        };
    }
    if known.get(&window_id).is_some_and(|last| last.started > started) {
        return crop;
    }
    // ponytail: dropped wholesale past 64 windows; per-window removal on
    // close if a long session ever needs it.
    if known.len() >= 64 && !known.contains_key(&window_id) {
        known.clear();
    }
    // A non-page answer stays as a marker, so an older lookup's page cannot
    // come back after it.
    known.insert(window_id, Known { size, crop: crop.ok(), started });
    crop
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

/// The screen area of the one top-level `AXWebArea` in `window_id`, and
/// cua's pill in it (searched only once the page's answer is settled, so
/// the search never makes the page late).
unsafe fn page_area(pid: i32, window_id: u32, deadline: Instant) -> Result<(Area, Option<Area>), &'static str> {
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
        // Past the deadline the AXWindows read itself timed out.
        return Err(if message_timeout(deadline, Instant::now()).is_none() {
            LATE
        } else {
            "the window is not in its app's accessibility tree"
        });
    };
    let mut walk = Walk {
        nodes: MAX_NODES,
        deadline,
        pages: Vec::new(),
        complete: true,
        element: None,
    };
    if walk.bound(root) {
        walk.children(root, MAX_DEPTH);
    }
    CFRelease(root as CFTypeRef);
    // A message that timed out reads as a missing value: past the deadline
    // nothing the walk saw can be trusted to be all of it. With time left,
    // an incomplete walk (node budget, a failed read) is not late.
    // Under a millisecond left counts as spent (`message_timeout`).
    let found = if message_timeout(deadline, Instant::now()).is_none() {
        Err(LATE)
    } else if !walk.complete {
        Err("the lookup could not read all of the window's views")
    } else {
        match walk.pages[..] {
            [page] => Ok((
                page,
                walk.element.and_then(|element| find_pill_in(element, deadline)),
            )),
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
    if let Some(element) = walk.element {
        CFRelease(element as CFTypeRef);
    }
    CFRelease(app as CFTypeRef);
    found
}

/// Cua's pill under `page`: the parent of the static text `PILL_TEXT`
/// among the page's last children (see `PILL_TAIL`). Any failure, the
/// deadline included, is `None`.
unsafe fn find_pill_in(page: AXUIElementRef, deadline: Instant) -> Option<Area> {
    let mut nodes = PILL_NODES;
    pill_under(page, PILL_DEPTH, deadline, &mut nodes)
}

unsafe fn pill_under(parent: AXUIElementRef, depth: u32, deadline: Instant, nodes: &mut u32) -> Option<Area> {
    if !bound(parent, deadline) {
        return None;
    }
    let (children, _) = copy_children_reporting(parent);
    let tail = children.len().saturating_sub(PILL_TAIL);
    let mut found = None;
    for (index, &child) in children.iter().enumerate().rev() {
        if found.is_none() && index >= tail && *nodes > 0 {
            *nodes -= 1;
            let role = if bound(child, deadline) { copy_string_attr(child, "AXRole") } else { None };
            found = if role.as_deref() == Some("AXStaticText") {
                (bound(child, deadline) && copy_string_attr(child, "AXValue").as_deref() == Some(PILL_TEXT))
                    .then(|| area_of(parent, deadline))
                    .flatten()
            } else if depth > 1 && role.is_some() {
                pill_under(child, depth - 1, deadline, nodes)
            } else {
                None
            };
        }
        CFRelease(child as CFTypeRef);
    }
    found
}

/// `element`'s screen area: its position and its size, each message
/// bounded by the time left.
unsafe fn area_of(element: AXUIElementRef, deadline: Instant) -> Option<Area> {
    if !bound(element, deadline) {
        return None;
    }
    let [x, y] = copy_geometry_attr_checked(element, "AXPosition", kAXValueCGPointType).ok()?;
    if !bound(element, deadline) {
        return None;
    }
    let [w, h] = copy_geometry_attr_checked(element, "AXSize", kAXValueCGSizeType).ok()?;
    Some(Area { x, y, w, h })
}

struct Walk {
    /// Elements still allowed to be read.
    nodes: u32,
    deadline: Instant,
    /// Areas of the top-level web areas found (DevTools' own left out).
    pages: Vec<Area>,
    /// Every element that was reached was read: `pages` is all of them.
    complete: bool,
    /// The first page's element (retained), for the pill search.
    element: Option<AXUIElementRef>,
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
            match area_of(element, self.deadline) {
                Some(area) => {
                    if self.element.is_none() {
                        CFRetain(element as CFTypeRef);
                        self.element = Some(element);
                    }
                    self.pages.push(area);
                }
                None => self.complete = false,
            }
        } else if depth > 0 && self.bound(element) {
            self.children(element, depth - 1);
        }
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
    fn a_late_lookup_keeps_the_last_crop_of_the_same_window_at_the_same_size() {
        let mut known = std::collections::HashMap::new();
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        let page = Area { x: 0.0, y: 87.0, w: 1100.0, h: 702.0 };
        let size = (1100.0, 789.0);
        // Late with nothing seen yet: unknown (the whole window), never a guess.
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(0)), Err(LATE));
        // A page, then a late lookup: the page again, not the whole window.
        assert_eq!(remember(&mut known, 7, size, Ok(page), at(500)), Ok(page));
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(1000)), Ok(page));
        // Not for another window, nor once this one was resized.
        assert_eq!(remember(&mut known, 8, size, Err(LATE), at(1000)), Err(LATE));
        assert_eq!(remember(&mut known, 7, (1000.0, 789.0), Err(LATE), at(1100)), Err(LATE));
        // Any other answer replaces it: a page that moved, or no page at all.
        let moved = Area { y: 120.0, h: 669.0, ..page };
        assert_eq!(remember(&mut known, 7, size, Ok(moved), at(1500)), Ok(moved));
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(2000)), Ok(moved));
        assert_eq!(remember(&mut known, 7, size, Err(ASKED), at(2500)), Err(ASKED));
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(3000)), Err(LATE));
        // An incomplete walk with time left is an answer, not late.
        let incomplete = "the lookup could not read all of the window's views";
        remember(&mut known, 7, size, Ok(page), at(3500));
        assert_eq!(remember(&mut known, 7, size, Err(incomplete), at(4000)), Err(incomplete));
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(4500)), Err(LATE));
    }

    #[test]
    fn an_older_lookup_answering_last_never_replaces_a_newer_crop() {
        let mut known = std::collections::HashMap::new();
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        let size = (1100.0, 789.0);
        let old = Area { x: 0.0, y: 87.0, w: 1100.0, h: 702.0 };
        let new = Area { y: 120.0, h: 669.0, ..old };
        remember(&mut known, 7, size, Ok(new), at(600));
        // The capture helper's lookup started first and answers now: it is
        // its own still's answer, but the kept crop stays the newer one.
        assert_eq!(remember(&mut known, 7, size, Ok(old), at(100)), Ok(old));
        assert_eq!(remember(&mut known, 7, size, Err(ASKED), at(100)), Err(ASKED));
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(900)), Ok(new));
        // A newer "no page" is not undone by an older lookup's page: a late
        // lookup after both shows the whole window.
        assert_eq!(remember(&mut known, 7, size, Err(ASKED), at(1200)), Err(ASKED));
        assert_eq!(remember(&mut known, 7, size, Ok(new), at(1000)), Ok(new));
        assert_eq!(remember(&mut known, 7, size, Err(LATE), at(1500)), Err(LATE));
    }

    #[test]
    fn the_crop_ends_just_above_cuas_pill() {
        // VM A's steps page (screen points) and the pill as indicator.js
        // draws it: 16 pt above the page's bottom, about 30 pt tall.
        let page = Area { x: 100.0, y: 117.0, w: 1100.0, h: 702.0 };
        let pill = Area { x: 530.0, y: 117.0 + 702.0 - 16.0 - 30.0, w: 240.0, h: 30.0 };
        let trim = pill_trim(page, pill).unwrap();
        assert_eq!(trim, 16.0 + 30.0 + PILL_GAP);
        let crop = crop_in_window(WINDOW, page).unwrap();
        let card = trimmed(crop, Some(trim));
        assert_eq!(card, Area { h: 702.0 - 56.0, ..crop });
        // The card's bottom, back in screen points, is the gap above the pill.
        assert_eq!(WINDOW.y + card.y + card.h, pill.y - PILL_GAP);
        // No trim, no change.
        assert_eq!(trimmed(crop, None), crop);
    }

    #[test]
    fn a_pill_not_where_the_extension_draws_it_trims_nothing() {
        let page = Area { x: 100.0, y: 117.0, w: 1100.0, h: 702.0 };
        let at = |x, y| Area { x, y, w: 240.0, h: 30.0 };
        // Page text quoting the label: off center, or centered but not just
        // above the bottom.
        assert_eq!(pill_trim(page, at(530.0, 300.0)), None);
        assert_eq!(pill_trim(page, at(400.0, 773.0)), None);
        assert_eq!(pill_trim(page, at(530.0, 600.0)), None);
        // Twice the zoom: twice as far up and tall, still the pill.
        let zoomed = Area { x: 410.0, y: 819.0 - 32.0 - 60.0, w: 480.0, h: 60.0 };
        assert_eq!(pill_trim(page, zoomed), Some(32.0 + 60.0 + PILL_GAP));
        // Chrome's zoom ends: 25% (4 pt up) and 500% (80 pt up).
        let quarter = Area { x: 620.0, y: 819.0 - 4.0 - 7.5, w: 60.0, h: 7.5 };
        assert_eq!(pill_trim(page, quarter), Some(4.0 + 7.5 + PILL_GAP));
        let fivefold = Area { x: 50.0 + 100.0, y: 819.0 - 80.0 - 150.0, w: 1000.0, h: 150.0 };
        assert_eq!(pill_trim(page, fivefold), Some(80.0 + 150.0 + PILL_GAP));
        // ... also in a short page, where the pill reaches its upper half.
        let low = Area { y: 117.0, h: 400.0, ..page };
        let tall = Area { y: 517.0 - 80.0 - 170.0, h: 170.0, ..fivefold };
        assert_eq!(pill_trim(low, tall), Some(80.0 + 170.0 + PILL_GAP));
        // Outside the page.
        assert_eq!(pill_trim(page, at(1000.0, 773.0)), None);
        assert_eq!(pill_trim(page, at(530.0, 800.0)), None);
        // A page too short for a card once trimmed.
        let short = Area { y: 117.0, h: 90.0, ..page };
        assert_eq!(pill_trim(short, at(530.0, 161.0)), None);
        assert_eq!(pill_trim(page, at(530.0, 773.0)), Some(56.0));
        // A trim kept from a taller page never leaves a sliver.
        let crop = Area { x: 0.0, y: 87.0, w: 1100.0, h: 90.0 };
        assert_eq!(trimmed(crop, Some(56.0)), crop);
    }

    #[test]
    fn the_trim_is_kept_for_its_window_once_the_pill_was_seen() {
        let mut trims = std::collections::HashMap::new();
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        // No pill seen yet: nothing to trim.
        assert_eq!(keep_trim(&mut trims, 7, None, at(0)), (None, false));
        // Seen: kept ...
        assert_eq!(keep_trim(&mut trims, 7, Some(56.0), at(500)), (Some(56.0), true));
        // ... through lookups that no longer see it (the pill hid 4 s after
        // the last command, Stop, a tab without it), and a lookup that
        // started before it was kept reads it too ...
        assert_eq!(keep_trim(&mut trims, 7, None, at(1000)), (Some(56.0), false));
        assert_eq!(keep_trim(&mut trims, 7, None, at(100)), (Some(56.0), false));
        assert_eq!(keep_trim(&mut trims, 7, Some(56.2), at(1500)), (Some(56.2), false));
        // ... until a pill of another size (page zoom) replaces it ...
        assert_eq!(keep_trim(&mut trims, 7, Some(102.0), at(2000)), (Some(102.0), true));
        assert_eq!(keep_trim(&mut trims, 7, None, at(2500)), (Some(102.0), false));
        // ... and an older lookup answering last never undoes that.
        assert_eq!(keep_trim(&mut trims, 7, Some(56.0), at(1800)), (Some(102.0), false));
        // Another window has its own.
        assert_eq!(keep_trim(&mut trims, 8, None, at(3000)), (None, false));
        // A full cache drops only for a new window, never on an update.
        for window in 100..162 {
            keep_trim(&mut trims, window, Some(56.0), at(3000));
        }
        assert_eq!(trims.len(), 63);
        keep_trim(&mut trims, 162, Some(56.0), at(3100));
        keep_trim(&mut trims, 7, Some(56.0), at(3200));
        assert_eq!(trims.len(), 64);
        keep_trim(&mut trims, 163, Some(56.0), at(3300));
        assert_eq!(trims.len(), 1);
    }

    #[test]
    fn a_late_lookup_keeps_the_trimmed_crop() {
        // remember keeps the page's own crop; the trim goes on top, so the
        // card a late lookup keeps is the trimmed one.
        let mut known = std::collections::HashMap::new();
        let t = Instant::now();
        let page = Area { x: 0.0, y: 87.0, w: 1100.0, h: 702.0 };
        let size = (1100.0, 789.0);
        let _ = remember(&mut known, 7, size, Ok(page), t);
        let late = remember(&mut known, 7, size, Err(LATE), t + Duration::from_millis(500));
        assert_eq!(late.map(|crop| trimmed(crop, Some(56.0)).h), Ok(646.0));
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
