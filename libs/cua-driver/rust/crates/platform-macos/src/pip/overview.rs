//! The all-agents overview: a frosted glass sheet centered on the active
//! screen (the one with keyboard focus), one row per live session (client icon, session name,
//! a dot in the session's color) with that session's windows as thumbnails
//! (the front card's live or still picture, the back windows' stills,
//! finished windows badged with a green check). Clicking a thumbnail brings
//! that window forward through the panel's Focus path and closes the sheet.
//!
//! Opened from the menu bar item (the cua koala, installed while the daemon
//! runs with `--experimental-pip`) or the global shortcut
//! (Ctrl-Option-A, a Carbon hot key: no Accessibility needed to listen).
//! Closed by Esc, a click outside the sheet, either trigger again, or a
//! focus click. Arrow keys move a selection; Return focuses it.
//!
//! The sheet is the key window while it is up, so it gets Esc and the
//! arrows, but it is a non-activating panel: the daemon never becomes the
//! active app and the user's frontmost app does not change until they pick
//! a window. It is a read-only view of the panels (see the overview rows of
//! the table in `finish`): while open, a poll re-reads the live panels four
//! times a second and rebuilds the rows when their structure changes.
//!
//! Everything that decides (the toggle, grouping, the layout for a screen
//! size and per-session window counts, hit testing, the selection) is pure
//! and unit tested; AppKit lives at the bottom of this file.

use std::ffi::c_void;
use std::time::Duration;

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::{
    add_subview, add_tracking, app_icon, area_of, cua_mark, decor_view_class,
    dispatch_to_main_after, focus_window, glass_background, host_layer, new_icon_view, new_label,
    new_mark, new_view, ns_rect, ns_string, on_glass, register_class, session_ns_color, set_text,
    try_with_state, with_state, Area, CGColor, Mark, Panel, State, Tag,
};

// ── Pure model ────────────────────────────────────────────────────────────

/// What opened or closed the overview (logged as `via=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Via {
    Menubar,
    Shortcut,
    Esc,
    Outside,
    Focus,
}

impl Via {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Via::Menubar => "menubar",
            Via::Shortcut => "shortcut",
            Via::Esc => "esc",
            Via::Outside => "outside",
            Via::Focus => "focus",
        }
    }
}

/// One window of a session, as the overview shows it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Thumb {
    pub(super) tag: Tag,
    pub(super) title: String,
    pub(super) finished: bool,
}

/// One live session: its panel's stack, front card first.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Group {
    pub(super) key: String,
    pub(super) label: String,
    pub(super) windows: Vec<Thumb>,
}

/// Display order: by label, then key, so rows never jump while the sheet
/// is up.
pub(super) fn display_order(a: &Group, b: &Group) -> std::cmp::Ordering {
    a.label.cmp(&b.label).then_with(|| a.key.cmp(&b.key))
}

/// The `groups=` field of the "PiP overview shown" line.
pub(super) fn groups_line(groups: &[Group]) -> String {
    let rows: Vec<String> = groups
        .iter()
        .map(|group| {
            let titles: Vec<String> = group
                .windows
                .iter()
                .map(|thumb| format!("{:?}", thumb.title))
                .collect();
            format!("{}: [{}]", group.label, titles.join(", "))
        })
        .collect();
    format!("[{}]", rows.join(", "))
}

/// The overview's state: whether it is up, what it shows, and which
/// thumbnail the arrow keys selected (group, window).
#[derive(Debug, Default)]
pub(super) struct Model {
    pub(super) open: bool,
    pub(super) groups: Vec<Group>,
    pub(super) selected: Option<(usize, usize)>,
}

/// What a trigger (the menu bar item or the shortcut) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Toggled {
    Opened,
    Closed,
}

impl Model {
    /// Open with `groups` (already sorted); a second open just refreshes.
    pub(super) fn open(&mut self, groups: Vec<Group>) {
        self.open = true;
        self.groups = groups;
        self.selected = None;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.groups.clear();
        self.selected = None;
    }

    /// A trigger: open when closed, close when open.
    pub(super) fn toggle(&mut self, groups: Vec<Group>) -> Toggled {
        if self.open {
            self.close();
            Toggled::Closed
        } else {
            self.open(groups);
            Toggled::Opened
        }
    }

    /// A poll while open re-read the panels: replace the groups, keep the
    /// selection on the same session and window where they still exist
    /// (else clear it). Whether the structure changed (the rows must be
    /// rebuilt).
    pub(super) fn update(&mut self, groups: Vec<Group>) -> bool {
        if groups == self.groups {
            return false;
        }
        self.selected = self.selected.and_then(|(g, w)| {
            let old = &self.groups[g];
            let tag = old.windows.get(w)?.tag;
            let ng = groups.iter().position(|group| group.key == old.key)?;
            let nw = groups[ng].windows.iter().position(|thumb| thumb.tag == tag)?;
            Some((ng, nw))
        });
        self.groups = groups;
        true
    }

    /// Move the selection with an arrow key; the first press selects the
    /// first thumbnail. Only rows that draw a thumbnail take part (`shown`
    /// is how many each drawn row has; a session with no resolved window
    /// draws none), and the column clamps to that row's thumbnails.
    pub(super) fn select(&mut self, dir: Dir, shown: &[usize]) -> Option<(usize, usize)> {
        let rows: Vec<usize> = (0..shown.len()).filter(|&g| shown[g] > 0).collect();
        let Some(&first) = rows.first() else {
            self.selected = None;
            return None;
        };
        let (g, w) = match self.selected.filter(|&(g, w)| shown.get(g).is_some_and(|&n| w < n)) {
            None => (first, 0),
            Some((g, w)) => {
                let at = rows.iter().position(|&row| row == g).unwrap_or(0);
                match dir {
                    Dir::Left => (g, w.saturating_sub(1)),
                    Dir::Right => (g, (w + 1).min(shown[g] - 1)),
                    Dir::Up => (rows[at.saturating_sub(1)], w),
                    Dir::Down => (rows[(at + 1).min(rows.len() - 1)], w),
                }
            }
        };
        self.selected = Some((g, w.min(shown[g] - 1)));
        self.selected
    }

    /// After a rebuild: a selection that is no longer drawn (its row past
    /// the rows that fit, or its thumbnail behind a "+n more" tile) moves
    /// to the nearest drawn thumbnail, or clears when nothing is drawn.
    pub(super) fn clamp(&mut self, shown: &[usize]) -> Option<(usize, usize)> {
        let Some((g, w)) = self.selected else {
            return None;
        };
        if shown.get(g).is_some_and(|&n| w < n) {
            return self.selected;
        }
        let nearest = (0..shown.len())
            .filter(|&row| shown[row] > 0)
            .min_by_key(|&row| (row.abs_diff(g), row));
        self.selected = nearest.map(|row| (row, w.min(shown[row] - 1)));
        self.selected
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dir {
    Left,
    Right,
    Up,
    Down,
}

// ── Layout ────────────────────────────────────────────────────────────────

/// Thumbnail widths tried in order: the largest at which every row fits
/// the sheet's height wins.
const THUMB_WIDTHS: [f64; 4] = [200.0, 170.0, 140.0, 120.0];
/// Thumbnail height as a fraction of its width (a 16:10 window).
const THUMB_ASPECT: f64 = 0.625;
/// Inset from the sheet's edge to its content.
pub(super) const SHEET_PAD: f64 = 20.0;
pub(super) const SHEET_RADIUS: f64 = 22.0;
/// Between rows, and between thumbnails in a row.
const ROW_GAP: f64 = 18.0;
const TILE_GAP: f64 = 12.0;
/// A row's header line (icon, name, dot) and the gap under it.
pub(super) const HEADER_H: f64 = 22.0;
const HEADER_GAP: f64 = 8.0;
/// A thumbnail's title line and the gap above it.
pub(super) const TITLE_H: f64 = 16.0;
const TITLE_GAP: f64 = 6.0;
/// The "+n more agents" line under the rows that fit.
pub(super) const MORE_LINE: f64 = 18.0;
/// The sheet is at most this much of the screen's visible frame.
const MAX_FRACTION: (f64, f64) = (0.8, 0.85);
/// Smallest sheet (the empty state fits it).
const MIN_SHEET: (f64, f64) = (320.0, 84.0);
/// Thumbnail corner radius.
pub(super) const THUMB_RADIUS: f64 = 8.0;

/// A thumbnail and its title line under it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Tile {
    pub(super) thumb: Area,
    pub(super) title: Area,
}

/// One session's row: its header line, the thumbnails that fit, and the
/// "+n more" tile taking the last slot when they did not all fit.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Row {
    pub(super) header: Area,
    pub(super) tiles: Vec<Tile>,
    pub(super) more: Option<(Area, usize)>,
}

/// Where everything goes, in the host window's coordinates (AppKit, origin
/// at the bottom-left of the screen's visible frame).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Layout {
    pub(super) sheet: Area,
    pub(super) rows: Vec<Row>,
    /// Sessions past the rows that fit, and the line that says so.
    pub(super) more_groups: Option<(Area, usize)>,
    pub(super) thumb: (f64, f64),
}

impl Layout {
    /// Thumbnails drawn per row (for the selection).
    pub(super) fn shown(&self) -> Vec<usize> {
        self.rows.iter().map(|row| row.tiles.len()).collect()
    }
}

/// Lay the sheet out for a visible frame `screen` wide and high and one
/// window count per session (display order). Thumbnails shrink until every
/// session's row fits the sheet's height; past the smallest size the rows
/// that fit are laid out and a "+n more agents" line counts the rest. A
/// row wider than the sheet allows ends in a "+n more" tile.
pub(super) fn layout(screen: (f64, f64), counts: &[usize]) -> Layout {
    let max_w = (screen.0 * MAX_FRACTION.0).max(MIN_SHEET.0);
    let max_h = (screen.1 * MAX_FRACTION.1).max(MIN_SHEET.1);
    let inner_w = max_w - 2.0 * SHEET_PAD;
    let mut pick = None;
    for &tw in &THUMB_WIDTHS {
        let th = (tw * THUMB_ASPECT).round();
        let row_h = HEADER_H + HEADER_GAP + th + TITLE_GAP + TITLE_H;
        let rows_fit = (((max_h - 2.0 * SHEET_PAD + ROW_GAP) / (row_h + ROW_GAP)).floor() as usize).max(1);
        pick = Some((tw, th, row_h, rows_fit));
        if counts.len() <= rows_fit {
            break;
        }
    }
    let (tw, th, row_h, rows_fit) = pick.unwrap_or((120.0, 75.0, 0.0, 1));
    let per_row = (((inner_w + TILE_GAP) / (tw + TILE_GAP)).floor() as usize).max(1);
    // Rows past the sheet: keep room for the line that counts them.
    let (shown_rows, more_groups) = if counts.len() <= rows_fit {
        (counts.len(), 0)
    } else {
        let fit = (((max_h - 2.0 * SHEET_PAD - MORE_LINE) / (row_h + ROW_GAP)).floor() as usize).max(1);
        (fit.min(counts.len()), counts.len() - fit.min(counts.len()))
    };
    let slots = |n: usize| n.min(per_row);
    let widest = counts[..shown_rows].iter().map(|&n| slots(n)).max().unwrap_or(0);
    let content_w = if widest == 0 {
        0.0
    } else {
        widest as f64 * (tw + TILE_GAP) - TILE_GAP
    };
    let w = (content_w + 2.0 * SHEET_PAD).max(MIN_SHEET.0);
    let mut h = 2.0 * SHEET_PAD + shown_rows as f64 * row_h + shown_rows.saturating_sub(1) as f64 * ROW_GAP;
    if more_groups > 0 {
        h += ROW_GAP + MORE_LINE;
    }
    let h = h.max(MIN_SHEET.1);
    let sheet = Area {
        x: ((screen.0 - w) / 2.0).round(),
        y: ((screen.1 - h) / 2.0).round(),
        w,
        h,
    };
    let left = sheet.x + SHEET_PAD;
    let mut top = sheet.y + sheet.h - SHEET_PAD;
    let mut rows = Vec::with_capacity(shown_rows);
    for &n in &counts[..shown_rows] {
        let header = Area {
            x: left,
            y: top - HEADER_H,
            w: content_w.max(MIN_SHEET.0 - 2.0 * SHEET_PAD),
            h: HEADER_H,
        };
        let thumb_top = top - HEADER_H - HEADER_GAP;
        let (tiles_n, more) = if n > per_row {
            (per_row - 1, n - (per_row - 1))
        } else {
            (n, 0)
        };
        let tile_at = |index: usize| Tile {
            thumb: Area {
                x: left + index as f64 * (tw + TILE_GAP),
                y: thumb_top - th,
                w: tw,
                h: th,
            },
            title: Area {
                x: left + index as f64 * (tw + TILE_GAP),
                y: thumb_top - th - TITLE_GAP - TITLE_H,
                w: tw,
                h: TITLE_H,
            },
        };
        let tiles: Vec<Tile> = (0..tiles_n).map(tile_at).collect();
        let more = (more > 0).then(|| (tile_at(tiles_n).thumb, more));
        rows.push(Row { header, tiles, more });
        top -= row_h + ROW_GAP;
    }
    let more_groups = (more_groups > 0).then(|| {
        (
            Area {
                x: left,
                y: sheet.y + SHEET_PAD,
                w: w - 2.0 * SHEET_PAD,
                h: MORE_LINE,
            },
            more_groups,
        )
    });
    Layout {
        sheet,
        rows,
        more_groups,
        thumb: (tw, th),
    }
}

/// What is under a point of the host window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hit {
    /// A thumbnail: (group, window).
    Tile(usize, usize),
    /// On the sheet, but not on a thumbnail.
    Sheet,
    Outside,
}

fn contains(area: &Area, (x, y): (f64, f64)) -> bool {
    x >= area.x && x < area.x + area.w && y >= area.y && y < area.y + area.h
}

pub(super) fn hit(layout: &Layout, point: (f64, f64)) -> Hit {
    if !contains(&layout.sheet, point) {
        return Hit::Outside;
    }
    for (g, row) in layout.rows.iter().enumerate() {
        for (w, tile) in row.tiles.iter().enumerate() {
            if contains(&tile.thumb, point) || contains(&tile.title, point) {
                return Hit::Tile(g, w);
            }
        }
    }
    Hit::Sheet
}

/// Screen rectangle (CoreGraphics, top-left origin) of an area of the host
/// window whose AppKit origin is `window` on a primary screen `screen_h`
/// high. Logged per thumbnail so a check can click one.
pub(super) fn screen_rect(area: Area, window: (f64, f64), screen_h: f64) -> Area {
    Area {
        x: window.0 + area.x,
        y: screen_h - (window.1 + area.y + area.h),
        w: area.w,
        h: area.h,
    }
}

// ── AppKit ────────────────────────────────────────────────────────────────

/// The shortcut: Ctrl-Option-A (`kVK_ANSI_A`; `controlKey | optionKey`).
/// Ctrl-Option-Space is macOS's "select next input source", so not that.
const HOTKEY_CODE: u32 = 0x00;
const HOTKEY_MODIFIERS: u32 = 4096 | 2048;
pub(super) const HOTKEY_NAME: &str = "ctrl+option+a";
/// How often the open sheet re-reads the panels.
const POLL: Duration = Duration::from_millis(250);
/// NSModalPanelWindowLevel: above the PiP panels (floating, 3).
const LEVEL: i64 = 8;
/// Session-color dot in a row header.
const DOT: f64 = 8.0;
const HEADER_ICON: f64 = 18.0;
/// The check badge on a finished thumbnail.
const BADGE: f64 = 18.0;

/// The pictures a thumbnail shows (native pointers, main queue only): the
/// window's still (`NSImage`, 0 for none), its live IOSurface (0 for
/// none), and its app's icon.
#[derive(Clone, Copy, Default)]
struct Pictures {
    still: usize,
    live: usize,
    icon: usize,
}

/// A group's pictures: its client icon and one `Pictures` per window.
#[derive(Clone, Default)]
struct GroupPictures {
    client_icon: usize,
    windows: Vec<Pictures>,
}

struct TileViews {
    view: usize,
    live_layer: usize,
    image: usize,
    icon: usize,
}

/// The sheet's views while it is up.
struct Views {
    /// The glass sheet (owned by the content view).
    glass: usize,
    tiles: Vec<Vec<TileViews>>,
}

/// The overview, in `State`.
#[derive(Default)]
pub(super) struct Overview {
    pub(super) model: Model,
    /// The full-screen host window, made on first use and kept.
    window: usize,
    layout: Option<Layout>,
    views: Option<Views>,
    hovered: Option<(usize, usize)>,
    /// Bumped on each open, so a poll from an earlier showing stops.
    poll_gen: u64,
}

/// Install the menu bar item and the shortcut (main queue).
pub(super) unsafe fn install() {
    install_menubar();
    install_hotkey();
}

unsafe fn install_menubar() {
    let bar: *mut AnyObject = msg_send![class!(NSStatusBar), systemStatusBar];
    let item: *mut AnyObject = msg_send![bar, statusItemWithLength: -1.0_f64]; // NSVariableStatusItemLength
    if item.is_null() {
        tracing::warn!(target: "pip", "PiP menubar item could not be made");
        return;
    }
    let _: *mut AnyObject = msg_send![item, retain]; // kept for the daemon's life
    let button: *mut AnyObject = msg_send![item, button];
    let _: () = msg_send![button, setImage: menubar_image()];
    let _: () = msg_send![button, setToolTip: ns_string("Show all cua agents (ctrl-option-a)")];
    let _: () = msg_send![button, setTarget: menubar_target() as *mut AnyObject];
    let _: () = msg_send![button, setAction: sel!(pipMenubar:)];
    tracing::info!(target: "pip", "PiP menubar item installed");
    // Where it landed, once the menu bar has laid it out: WindowServer
    // lists the item under Control Center, so a check finds it by this line.
    dispatch_to_main_after(Duration::from_secs(1), button as usize, log_menubar_cb);
}

unsafe extern "C" fn log_menubar_cb(ctx: *mut c_void) {
    let button = *Box::from_raw(ctx as *mut usize) as *mut AnyObject;
    let window: *mut AnyObject = msg_send![button, window];
    if window.is_null() {
        tracing::warn!(target: "pip", "PiP menubar item has no window");
        return;
    }
    let frame: NSRect = msg_send![window, frame];
    let primary_h = primary_screen_height();
    let at = screen_rect(area_of(frame), (0.0, 0.0), primary_h);
    tracing::info!(target: "pip", x = at.x, y = at.y, w = at.w, h = at.h, "PiP menubar item at");
}



/// The koala mark as a template image at 18 pt with its 2x representation.
unsafe fn menubar_image() -> *mut AnyObject {
    static PNG_1X: &[u8] = include_bytes!("assets/cua-agents-menubar-18.png");
    static PNG_2X: &[u8] = include_bytes!("assets/cua-agents-menubar-36.png");
    let data = |png: &'static [u8]| -> *mut AnyObject {
        msg_send![
            class!(NSData),
            dataWithBytes: png.as_ptr() as *const c_void
            length: png.len()
        ]
    };
    let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
    let image: *mut AnyObject = msg_send![image, initWithData: data(PNG_1X)];
    if image.is_null() {
        return std::ptr::null_mut();
    }
    let rep: *mut AnyObject = msg_send![class!(NSBitmapImageRep), imageRepWithData: data(PNG_2X)];
    if !rep.is_null() {
        let _: () = msg_send![rep, setSize: NSSize::new(18.0, 18.0)];
        let _: () = msg_send![image, addRepresentation: rep];
    }
    let _: () = msg_send![image, setSize: NSSize::new(18.0, 18.0)];
    let _: () = msg_send![image, setTemplate: true];
    let _: *mut AnyObject = msg_send![image, autorelease];
    image
}

fn menubar_target() -> usize {
    static TARGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TARGET.get_or_init(|| {
        let class = register_class("CuaPipOverviewTarget", class!(NSObject), |builder| unsafe {
            builder.add_method(sel!(pipMenubar:), on_menubar as extern "C" fn(_, _, _));
        });
        let target: *mut AnyObject = unsafe { msg_send![class, new] };
        target as usize
    })
}

extern "C" fn on_menubar(_this: *mut AnyObject, _cmd: Sel, _sender: *mut AnyObject) {
    with_state(|state| unsafe { toggle(state, Via::Menubar) });
}

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
        num_types: usize, // ItemCount (unsigned long)
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out: *mut *mut c_void,
    ) -> i32;
}

const K_EVENT_CLASS_KEYBOARD: u32 = 0x6b65_7962; // 'keyb'
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;

extern "C" fn on_hotkey(_call: *mut c_void, _event: *mut c_void, _data: *mut c_void) -> i32 {
    with_state(|state| unsafe { toggle(state, Via::Shortcut) });
    0 // noErr
}

unsafe fn install_hotkey() {
    let spec = EventTypeSpec {
        class: K_EVENT_CLASS_KEYBOARD,
        kind: K_EVENT_HOT_KEY_PRESSED,
    };
    let mut handler = std::ptr::null_mut();
    let status = InstallEventHandler(
        GetApplicationEventTarget(),
        on_hotkey,
        1,
        &spec,
        std::ptr::null_mut(),
        &mut handler,
    );
    if status != 0 {
        tracing::warn!(target: "pip", status, "PiP overview shortcut handler failed");
        return;
    }
    let id = EventHotKeyID {
        signature: 0x6375_614f, // 'cuaO'
        id: 1,
    };
    let mut hotkey = std::ptr::null_mut();
    let status = RegisterEventHotKey(
        HOTKEY_CODE,
        HOTKEY_MODIFIERS,
        id,
        GetApplicationEventTarget(),
        0,
        &mut hotkey,
    );
    if status != 0 {
        tracing::warn!(target: "pip", status, keys = HOTKEY_NAME, "PiP overview shortcut failed");
        return;
    }
    tracing::info!(target: "pip", keys = HOTKEY_NAME, "PiP overview shortcut registered");
}

/// The menu bar item or the shortcut: open the sheet, or close it.
unsafe fn toggle(state: &mut State, via: Via) {
    let (groups, pictures) = snapshot(&state.panels);
    match state.overview.model.toggle(groups) {
        Toggled::Opened => open(state, pictures, via),
        Toggled::Closed => close(state, via),
    }
}

/// Every live panel as a group (sorted for display) with its pictures.
unsafe fn snapshot(panels: &std::collections::HashMap<String, Panel>) -> (Vec<Group>, Vec<GroupPictures>) {
    let mut groups: Vec<(Group, GroupPictures)> = panels
        .iter()
        .map(|(key, panel)| {
            let mut windows = Vec::new();
            let mut pictures = Vec::new();
            for (depth, card) in panel.cards.cards().iter().enumerate() {
                let finished = card.key.1.is_some_and(|window| panel.verdicts.finished(window));
                windows.push(Thumb {
                    tag: card.key,
                    title: card.data.title.clone(),
                    finished,
                });
                let app: *mut AnyObject = match card.data.pid.or(card.key.0) {
                    Some(pid) => msg_send![
                        class!(NSRunningApplication),
                        runningApplicationWithProcessIdentifier: pid
                    ],
                    None => std::ptr::null_mut(),
                };
                let icon = app_icon(app) as usize;
                let (still, live) = if depth == 0 {
                    let still: *mut AnyObject = msg_send![panel.image_view as *mut AnyObject, image];
                    let live = panel
                        .live_frame
                        .as_ref()
                        .and_then(|frame| frame.io_surface())
                        .map_or(0, |surface| surface.as_ptr() as usize);
                    (still as usize, live)
                } else {
                    let still = super::stack::own_pixels(card.key, card.data.still.as_ref())
                        .map_or(0, |image| image.0);
                    (still, 0)
                };
                pictures.push(Pictures { still, live, icon });
            }
            let client_icon: *mut AnyObject = msg_send![panel.client_icon as *mut AnyObject, image];
            (
                Group {
                    key: key.clone(),
                    label: panel.name.clone(),
                    windows,
                },
                GroupPictures {
                    client_icon: client_icon as usize,
                    windows: pictures,
                },
            )
        })
        .collect();
    groups.sort_by(|a, b| display_order(&a.0, &b.0));
    groups.into_iter().unzip()
}

/// The visible frame of the active screen (`mainScreen`: the one with
/// keyboard focus), and the primary screen's height (for CoreGraphics
/// coordinates in logs). `None` headless. No walk of `[NSScreen screens]`:
/// that array is Swift-bridged and its `count` is a signed NSInteger, which
/// a debug build's message check rejects as NSUInteger.
unsafe fn active_screen() -> Option<(Area, f64)> {
    let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
    if screen.is_null() {
        return None;
    }
    let visible: NSRect = msg_send![screen, visibleFrame];
    Some((area_of(visible), primary_screen_height()))
}

/// Height of the primary screen (AppKit's coordinate origin).
unsafe fn primary_screen_height() -> f64 {
    let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
    let screen: *mut AnyObject = msg_send![screens, firstObject];
    if screen.is_null() {
        return 0.0;
    }
    let frame: NSRect = msg_send![screen, frame];
    frame.size.height
}

unsafe fn open(state: &mut State, pictures: Vec<GroupPictures>, via: Via) {
    let Some((visible, primary_h)) = active_screen() else {
        state.overview.model.close();
        return;
    };
    let overview = &mut state.overview;
    if overview.window == 0 {
        overview.window = new_host_window(ns_rect(visible)) as usize;
    }
    let window = overview.window as *mut AnyObject;
    let _: () = msg_send![window, setFrame: ns_rect(visible) display: false];
    build(overview, (visible.w, visible.h), &pictures);
    overview.hovered = None;
    log_tiles(overview, (visible.x, visible.y), primary_h);
    tracing::info!(
        target: "pip",
        groups = %groups_line(&overview.model.groups),
        via = %via.as_str(),
        "PiP overview shown"
    );
    // Key for Esc and the arrows; a non-activating panel, so the user's
    // frontmost app stays where it is.
    let _: () = msg_send![window, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
    overview.poll_gen += 1;
    dispatch_to_main_after(POLL, overview.poll_gen, poll_cb);
}

unsafe fn close(state: &mut State, via: Via) {
    let overview = &mut state.overview;
    overview.model.close();
    overview.hovered = None;
    if overview.window != 0 {
        let _: () = msg_send![overview.window as *mut AnyObject, orderOut: std::ptr::null_mut::<AnyObject>()];
    }
    tracing::info!(target: "pip", via = %via.as_str(), "PiP overview closed");
}

/// Where each thumbnail is on screen, for checks.
unsafe fn log_tiles(overview: &Overview, window: (f64, f64), primary_h: f64) {
    let Some(layout) = &overview.layout else {
        return;
    };
    for (row, group) in layout.rows.iter().zip(&overview.model.groups) {
        for (tile, thumb) in row.tiles.iter().zip(&group.windows) {
            let at = screen_rect(tile.thumb, window, primary_h);
            tracing::info!(
                target: "pip",
                session = %group.key,
                window = thumb.tag.1.unwrap_or(0),
                title = %thumb.title,
                finished = thumb.finished,
                x = at.x, y = at.y, w = at.w, h = at.h,
                "PiP overview tile"
            );
        }
    }
}

/// Re-read the panels while the sheet is up: rebuild the rows when their
/// structure changed, refresh the pictures either way, and look at the
/// pointer (a pointer warped into place, as a tool's move does, sends no
/// mouse-moved event; the panel's hover bar polls for the same reason).
unsafe extern "C" fn poll_cb(ctx: *mut c_void) {
    let generation: u64 = *Box::from_raw(ctx as *mut u64);
    objc2::rc::autoreleasepool(|_| {
        with_state(|state| {
            if !state.overview.model.open || state.overview.poll_gen != generation {
                return;
            }
            let (groups, pictures) = snapshot(&state.panels);
            let overview = &mut state.overview;
            let frame: NSRect = msg_send![overview.window as *mut AnyObject, frame];
            if overview.model.update(groups) {
                build(overview, (frame.size.width, frame.size.height), &pictures);
                let shown = overview.layout.as_ref().map(Layout::shown).unwrap_or_default();
                overview.model.clamp(&shown);
                overview.hovered = None;
                highlight(overview);
            } else {
                refresh_pictures(overview, &pictures);
            }
            let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
            hover(overview, (mouse.x - frame.origin.x, mouse.y - frame.origin.y), "poll");
            dispatch_to_main_after(POLL, generation, poll_cb);
        });
    });
}

/// Outline the thumbnail under `point` (host window coordinates), if that
/// changed.
unsafe fn hover(overview: &mut Overview, point: (f64, f64), via: &str) {
    let Some(layout) = &overview.layout else {
        return;
    };
    let hovered = match hit(layout, point) {
        Hit::Tile(g, w) => Some((g, w)),
        _ => None,
    };
    if hovered != overview.hovered {
        overview.hovered = hovered;
        let thumb = hovered.and_then(|(g, w)| overview.model.groups.get(g).map(|group| (group, w)));
        tracing::info!(
            target: "pip",
            session = thumb.map_or("", |(group, _)| group.key.as_str()),
            window = thumb.and_then(|(group, w)| group.windows.get(w)).and_then(|t| t.tag.1).unwrap_or(0),
            via, x = point.0, y = point.1,
            "PiP overview hover"
        );
        highlight(overview);
    }
}

/// (Re)build the sheet for the model's groups: throw the old glass away
/// and lay a new one in the host window's content view.
unsafe fn build(overview: &mut Overview, screen: (f64, f64), pictures: &[GroupPictures]) {
    let content: *mut AnyObject = msg_send![overview.window as *mut AnyObject, contentView];
    if let Some(old) = overview.views.take() {
        let _: () = msg_send![old.glass as *mut AnyObject, removeFromSuperview];
    }
    let counts: Vec<usize> = overview.model.groups.iter().map(|group| group.windows.len()).collect();
    let layout = layout(screen, &counts);
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(layout.sheet.w, layout.sheet.h));
    let body = new_view(decor_view_class(), bounds);
    let mut tiles = Vec::with_capacity(layout.rows.len());
    // Layout is in window coordinates; the body's origin is the sheet's.
    let local = |area: Area| Area {
        x: area.x - layout.sheet.x,
        y: area.y - layout.sheet.y,
        ..area
    };
    if overview.model.groups.is_empty() {
        let line = new_label(
            ns_rect(Area {
                x: SHEET_PAD,
                y: (layout.sheet.h - TITLE_H) / 2.0,
                w: layout.sheet.w - 2.0 * SHEET_PAD,
                h: TITLE_H,
            }),
            13.0,
            0.0,
            true,
        );
        on_glass(line, true);
        set_text(line as usize, "No agents are working right now");
        let _: () = msg_send![line, setAlignment: 2isize]; // NSTextAlignmentCenter
        let _: () = msg_send![body, addSubview: line];
    }
    for ((row, group), group_pictures) in layout.rows.iter().zip(&overview.model.groups).zip(pictures) {
        // Header: client icon, session name, a dot in the session's color.
        let header = local(row.header);
        let icon = new_icon_view(ns_rect(Area {
            x: header.x,
            y: header.y + (HEADER_H - HEADER_ICON) / 2.0,
            w: HEADER_ICON,
            h: HEADER_ICON,
        }));
        let tint: *mut AnyObject = msg_send![class!(NSColor), labelColor];
        let _: () = msg_send![icon, setContentTintColor: tint];
        let client_icon = if group_pictures.client_icon == 0 {
            cua_mark()
        } else {
            group_pictures.client_icon as *mut AnyObject
        };
        let _: () = msg_send![icon, setImage: client_icon];
        let _: () = msg_send![body, addSubview: icon];
        let name = new_label(
            ns_rect(Area {
                x: header.x + HEADER_ICON + 8.0,
                y: header.y + (HEADER_H - TITLE_H) / 2.0,
                w: header.w - HEADER_ICON - 8.0 - DOT - 8.0,
                h: TITLE_H,
            }),
            13.0,
            0.3, // NSFontWeightMedium
            false,
        );
        on_glass(name, false);
        set_text(name as usize, &group.label);
        let _: () = msg_send![name, sizeToFit];
        let name_frame: NSRect = msg_send![name, frame];
        let _: () = msg_send![body, addSubview: name];
        let dot = new_view(
            decor_view_class(),
            NSRect::new(
                NSPoint::new(
                    name_frame.origin.x + name_frame.size.width + 8.0,
                    header.y + (HEADER_H - DOT) / 2.0,
                ),
                NSSize::new(DOT, DOT),
            ),
        );
        let dot_layer = host_layer(dot);
        let _: () = msg_send![dot_layer, setCornerRadius: DOT / 2.0];
        let color: *mut AnyObject = session_ns_color(&group.key);
        let color: *mut CGColor = msg_send![color, CGColor];
        let _: () = msg_send![dot_layer, setBackgroundColor: color];
        add_subview(body, dot);

        let mut row_tiles = Vec::with_capacity(row.tiles.len());
        for ((tile, thumb), picture) in row.tiles.iter().zip(&group.windows).zip(&group_pictures.windows) {
            row_tiles.push(new_tile(body, local(tile.thumb), local(tile.title), thumb, *picture));
        }
        if let Some((area, more)) = row.more {
            let line = new_label(ns_rect(local(area)), 12.0, 0.0, true);
            on_glass(line, true);
            set_text(line as usize, &format!("+{more} more"));
            let _: () = msg_send![line, setAlignment: 2isize];
            let _: () = msg_send![body, addSubview: line];
        }
        tiles.push(row_tiles);
    }
    if let Some((area, more)) = layout.more_groups {
        let line = new_label(ns_rect(local(area)), 12.0, 0.0, true);
        on_glass(line, true);
        set_text(
            line as usize,
            &format!("+{more} more agent{}", if more == 1 { "" } else { "s" }),
        );
        let _: () = msg_send![line, setAlignment: 2isize];
        let _: () = msg_send![body, addSubview: line];
    }
    let glass = glass_background(bounds, body, SHEET_RADIUS);
    let _: () = msg_send![glass, setFrame: ns_rect(layout.sheet)];
    add_subview(content, glass);
    tracing::info!(target: "pip", rows = layout.rows.len(), tiles = tiles.iter().map(Vec::len).sum::<usize>(), "PiP overview built");
    overview.views = Some(Views {
        glass: glass as usize,
        tiles,
    });
    overview.layout = Some(layout);
}

/// A thumbnail in `body`: a rounded dark well holding the still (or the
/// app's icon while there is none) with a live layer over it, a green
/// check badge on a finished window, and the title line under it.
unsafe fn new_tile(
    body: *mut AnyObject,
    thumb_area: Area,
    title_area: Area,
    thumb: &Thumb,
    picture: Pictures,
) -> TileViews {
    let view = new_view(decor_view_class(), ns_rect(thumb_area));
    let layer = host_layer(view);
    let _: () = msg_send![layer, setCornerRadius: THUMB_RADIUS];
    let _: () = msg_send![layer, setCornerCurve: ns_string("continuous")];
    let _: () = msg_send![layer, setMasksToBounds: true];
    let backing: *mut AnyObject = msg_send![
        class!(NSColor),
        colorWithSRGBRed: 0.0_f64
        green: 0.0_f64
        blue: 0.0_f64
        alpha: 0.22_f64
    ];
    let backing: *mut CGColor = msg_send![backing, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: backing];
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(thumb_area.w, thumb_area.h));
    // The app icon, centered, for a window with no picture yet.
    let icon_size = (thumb_area.h * 0.4).round();
    let icon = new_icon_view(NSRect::new(
        NSPoint::new((thumb_area.w - icon_size) / 2.0, (thumb_area.h - icon_size) / 2.0),
        NSSize::new(icon_size, icon_size),
    ));
    let _: () = msg_send![icon, setImage: picture.icon as *mut AnyObject];
    let _: () = msg_send![view, addSubview: icon];
    let image = new_icon_view(bounds);
    let _: () = msg_send![view, addSubview: image];
    let live = new_view(decor_view_class(), bounds);
    let live_layer: *mut AnyObject = msg_send![class!(CALayer), layer];
    let _: () = msg_send![live_layer, setContentsGravity: ns_string("resizeAspect")];
    let _: () = msg_send![live, setLayer: live_layer];
    let _: () = msg_send![live, setWantsLayer: true];
    add_subview(view, live);
    add_subview(body, view);
    if thumb.finished {
        let badge = new_view(
            decor_view_class(),
            NSRect::new(
                NSPoint::new(thumb_area.x + thumb_area.w - BADGE + 4.0, thumb_area.y - 4.0),
                NSSize::new(BADGE, BADGE),
            ),
        );
        let badge_layer = host_layer(badge);
        let (mark, _) = new_mark(BADGE, Mark::Check);
        let white: *mut AnyObject = msg_send![class!(NSColor), whiteColor];
        let white: *mut CGColor = msg_send![white, CGColor];
        let _: () = msg_send![mark, setBorderWidth: 1.5_f64];
        let _: () = msg_send![mark, setBorderColor: white];
        let _: () = msg_send![badge_layer, addSublayer: mark];
        add_subview(body, badge);
    }
    let title = new_label(ns_rect(title_area), 11.0, 0.0, true);
    on_glass(title, true);
    set_text(title as usize, &thumb.title);
    let _: () = msg_send![title, setAlignment: 2isize];
    let _: () = msg_send![body, addSubview: title];
    let views = TileViews {
        view: view as usize,
        live_layer: live_layer as usize,
        image: image as usize,
        icon: icon as usize,
    };
    set_pictures(&views, picture);
    views
}

unsafe fn set_pictures(tile: &TileViews, picture: Pictures) {
    let _: () = msg_send![tile.image as *mut AnyObject, setImage: picture.still as *mut AnyObject];
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![tile.live_layer as *mut AnyObject, setContents: picture.live as *mut AnyObject];
    let _: () = msg_send![class!(CATransaction), commit];
    let _: () = msg_send![tile.icon as *mut AnyObject, setHidden: picture.still != 0 || picture.live != 0];
}

unsafe fn refresh_pictures(overview: &Overview, pictures: &[GroupPictures]) {
    let Some(views) = &overview.views else {
        return;
    };
    for (row, group_pictures) in views.tiles.iter().zip(pictures) {
        for (tile, picture) in row.iter().zip(&group_pictures.windows) {
            set_pictures(tile, *picture);
        }
    }
}

/// Outline the hovered and the selected thumbnails in the accent color.
unsafe fn highlight(overview: &Overview) {
    let Some(views) = &overview.views else {
        return;
    };
    let accent: *mut AnyObject = msg_send![class!(NSColor), controlAccentColor];
    let accent: *mut CGColor = msg_send![accent, CGColor];
    for (g, row) in views.tiles.iter().enumerate() {
        for (w, tile) in row.iter().enumerate() {
            let on = overview.hovered == Some((g, w)) || overview.model.selected == Some((g, w));
            let layer: *mut AnyObject = msg_send![tile.view as *mut AnyObject, layer];
            let _: () = msg_send![layer, setBorderWidth: if on { 2.5_f64 } else { 0.0_f64 }];
            let _: () = msg_send![layer, setBorderColor: accent];
        }
    }
}

/// Bring `thumb`'s window forward (the panel's Focus path) and close.
unsafe fn focus(state: &mut State, group: usize, window: usize) {
    let Some(thumb) = state
        .overview
        .model
        .groups
        .get(group)
        .and_then(|group| group.windows.get(window))
    else {
        return;
    };
    let key = state.overview.model.groups[group].key.clone();
    let (tag, title) = (thumb.tag, thumb.title.clone());
    tracing::info!(target: "pip", session = %key, window = tag.1.unwrap_or(0), title = %title, "PiP overview focus");
    close(state, Via::Focus);
    if let Some(pid) = tag.0 {
        focus_window(pid, tag.1);
    }
}

// ── Host window and its classes ───────────────────────────────────────────

/// A borderless, non-activating panel covering the screen's visible frame,
/// clear except for the sheet; it can become key (for Esc and the arrows)
/// without activating the daemon. Owned (+1) by the caller, kept forever.
unsafe fn new_host_window(rect: NSRect) -> *mut AnyObject {
    let style_mask: u64 = 1 << 7; // Borderless | NonactivatingPanel
    let window: *mut AnyObject = msg_send![host_window_class(), alloc];
    let window: *mut AnyObject = msg_send![
        window,
        initWithContentRect: rect
        styleMask: style_mask
        backing: 2u64
        defer: false
    ];
    let _: () = msg_send![window, setReleasedWhenClosed: false];
    let _: () = msg_send![window, setLevel: LEVEL];
    let _: () = msg_send![window, setHidesOnDeactivate: false];
    // CanJoinAllSpaces (1 << 0) | IgnoresCycle (1 << 6) | FullScreenAuxiliary (1 << 8)
    let behavior: u64 = (1 << 0) | (1 << 6) | (1 << 8);
    let _: () = msg_send![window, setCollectionBehavior: behavior];
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![window, setBackgroundColor: clear];
    let _: () = msg_send![window, setOpaque: false];
    let _: () = msg_send![window, setHasShadow: false];
    let _: () = msg_send![window, setTitle: ns_string("cua PiP overview")];
    let _: () = msg_send![window, setAcceptsMouseMovedEvents: true];
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), rect.size);
    let content = new_view(host_view_class(), bounds);
    add_tracking(content);
    let _: () = msg_send![window, setContentView: content];
    let _: () = msg_send![content, release];
    window
}

fn host_window_class() -> &'static AnyClass {
    static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
    CLASS.get_or_init(|| {
        register_class("CuaPipOverviewPanel", class!(NSPanel), |builder| unsafe {
            builder.add_method(sel!(canBecomeKeyWindow), returns_yes as extern "C" fn(_, _) -> _);
            builder.add_method(sel!(canBecomeMainWindow), returns_no as extern "C" fn(_, _) -> _);
            builder.add_method(sel!(keyDown:), key_down as extern "C" fn(_, _, _));
            builder.add_method(sel!(resignKeyWindow), resign_key as extern "C" fn(_, _));
        })
    })
}

fn host_view_class() -> &'static AnyClass {
    static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
    CLASS.get_or_init(|| {
        register_class("CuaPipOverviewView", class!(NSView), |builder| unsafe {
            // Every press and move is the host view's, whatever subview of
            // the sheet (an image view, a label) sits under the pointer.
            builder.add_method(sel!(hitTest:), hit_self as extern "C" fn(_, _, _) -> _);
            builder.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(_, _, _));
            builder.add_method(sel!(mouseMoved:), mouse_moved as extern "C" fn(_, _, _));
            builder.add_method(sel!(mouseExited:), mouse_exited as extern "C" fn(_, _, _));
            builder.add_method(sel!(acceptsFirstMouse:), accepts_first_mouse as extern "C" fn(_, _, _) -> _);
        })
    })
}

extern "C" fn returns_yes(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    Bool::YES
}

extern "C" fn returns_no(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    Bool::NO
}

extern "C" fn hit_self(this: *mut AnyObject, _cmd: Sel, _point: NSPoint) -> *mut AnyObject {
    this
}

extern "C" fn accepts_first_mouse(_this: *mut AnyObject, _cmd: Sel, _event: *mut AnyObject) -> Bool {
    Bool::YES
}

const KEY_ESCAPE: u16 = 53;
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_LEFT: u16 = 123;
const KEY_RIGHT: u16 = 124;
const KEY_DOWN: u16 = 125;
const KEY_UP: u16 = 126;

extern "C" fn key_down(_this: *mut AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let code: u16 = unsafe { msg_send![event, keyCode] };
    with_state(|state| unsafe {
        if !state.overview.model.open {
            return;
        }
        let dir = match code {
            KEY_ESCAPE => return close(state, Via::Esc),
            KEY_RETURN | KEY_ENTER => {
                if let Some((g, w)) = state.overview.model.selected {
                    focus(state, g, w);
                }
                return;
            }
            KEY_LEFT => Dir::Left,
            KEY_RIGHT => Dir::Right,
            KEY_UP => Dir::Up,
            KEY_DOWN => Dir::Down,
            _ => return,
        };
        let shown = state.overview.layout.as_ref().map(Layout::shown).unwrap_or_default();
        state.overview.model.select(dir, &shown);
        highlight(&state.overview);
    });
}

/// Something else took the keyboard (the user switched apps): the sheet
/// goes with it, like a menu.
extern "C" fn resign_key(this: *mut AnyObject, _cmd: Sel) {
    unsafe {
        let _: () = msg_send![super(this, class!(NSPanel)), resignKeyWindow];
    }
    // Inside a state operation (our own orderOut resigns key): nothing to do,
    // the sheet is already closing.
    try_with_state(|state| unsafe {
        if state.overview.model.open {
            close(state, Via::Outside);
        }
    });
}

unsafe fn event_point(this: *mut AnyObject, event: *mut AnyObject) -> (f64, f64) {
    let in_window: NSPoint = msg_send![event, locationInWindow];
    let local: NSPoint = msg_send![
        this,
        convertPoint: in_window
        fromView: std::ptr::null_mut::<AnyObject>()
    ];
    (local.x, local.y)
}

extern "C" fn mouse_down(this: *mut AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let point = unsafe { event_point(this, event) };
    with_state(|state| unsafe {
        let Some(layout) = &state.overview.layout else {
            return;
        };
        match hit(layout, point) {
            Hit::Tile(g, w) => focus(state, g, w),
            Hit::Outside => close(state, Via::Outside),
            Hit::Sheet => {}
        }
    });
}

extern "C" fn mouse_moved(this: *mut AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let point = unsafe { event_point(this, event) };
    with_state(|state| unsafe { hover(&mut state.overview, point, "event") });
}

extern "C" fn mouse_exited(_this: *mut AnyObject, _cmd: Sel, _event: *mut AnyObject) {
    with_state(|state| unsafe {
        if state.overview.hovered.take().is_some() {
            highlight(&state.overview);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: (f64, f64) = (1440.0, 875.0);

    fn thumb(pid: i32, window: u32, title: &str, finished: bool) -> Thumb {
        Thumb {
            tag: (Some(pid), Some(window)),
            title: title.to_owned(),
            finished,
        }
    }

    fn group(label: &str, windows: Vec<Thumb>) -> Group {
        Group {
            key: format!("__cua_runtime_x:{label}"),
            label: label.to_owned(),
            windows,
        }
    }

    fn two_groups() -> Vec<Group> {
        vec![
            group("ov-a", vec![thumb(1, 10, "doc a", false)]),
            group("ov-b", vec![thumb(1, 20, "doc b", false), thumb(1, 21, "doc c", true)]),
        ]
    }

    // ── Table rows (see the overview rows in `finish`) ──

    #[test]
    fn open_shows_every_live_session_and_logs_them() {
        let mut model = Model::default();
        assert_eq!(model.toggle(two_groups()), Toggled::Opened);
        assert!(model.open);
        assert_eq!(model.groups.len(), 2);
        assert_eq!(
            groups_line(&model.groups),
            r#"[ov-a: ["doc a"], ov-b: ["doc b", "doc c"]]"#
        );
        assert_eq!(groups_line(&[]), "[]");
        // Display order is by label, whatever order the panels came in.
        let mut reversed = two_groups();
        reversed.reverse();
        reversed.sort_by(display_order);
        assert_eq!(reversed, two_groups());
    }

    #[test]
    fn close_and_a_second_trigger_only_take_the_sheet_down() {
        let mut model = Model::default();
        model.toggle(two_groups());
        assert_eq!(model.toggle(Vec::new()), Toggled::Closed);
        assert!(!model.open);
        assert!(model.groups.is_empty());
        for via in [Via::Esc, Via::Outside, Via::Shortcut, Via::Menubar, Via::Focus] {
            assert!(!via.as_str().is_empty());
        }
    }

    #[test]
    fn focus_click_names_the_thumbnail_under_the_pointer() {
        let groups = two_groups();
        let counts: Vec<usize> = groups.iter().map(|g| g.windows.len()).collect();
        let layout = layout(SCREEN, &counts);
        let tile = &layout.rows[1].tiles[0].thumb;
        let inside = (tile.x + tile.w / 2.0, tile.y + tile.h / 2.0);
        assert_eq!(hit(&layout, inside), Hit::Tile(1, 0));
        assert_eq!(groups[1].windows[0].tag, (Some(1), Some(20)));
        // The title under it counts too; the sheet's padding does not.
        let title = &layout.rows[1].tiles[0].title;
        assert_eq!(hit(&layout, (title.x + 1.0, title.y + 1.0)), Hit::Tile(1, 0));
        // Every row's thumbnails, not only the bottom row's.
        for (g, row) in layout.rows.iter().enumerate() {
            for (w, tile) in row.tiles.iter().enumerate() {
                let center = (tile.thumb.x + tile.thumb.w / 2.0, tile.thumb.y + tile.thumb.h / 2.0);
                assert_eq!(hit(&layout, center), Hit::Tile(g, w), "row {g} tile {w} at {center:?}: {layout:?}");
            }
        }
        assert_eq!(hit(&layout, (layout.sheet.x + 2.0, layout.sheet.y + 2.0)), Hit::Sheet);
        assert_eq!(hit(&layout, (0.0, 0.0)), Hit::Outside);
    }

    #[test]
    fn a_session_acting_while_open_updates_its_row_and_keeps_the_selection() {
        let mut model = Model::default();
        model.toggle(two_groups());
        model.select(Dir::Down, &[1, 2]); // the first press selects the first
        model.select(Dir::Down, &[1, 2]);
        model.select(Dir::Right, &[1, 2]);
        assert_eq!(model.selected, Some((1, 1)));
        // ov-b acts in doc c: it comes to the front, and doc b is named.
        let mut groups = two_groups();
        groups[1].windows = vec![thumb(1, 21, "doc c", false), thumb(1, 20, "doc b (edited)", false)];
        assert!(model.update(groups.clone()));
        assert_eq!(model.groups, groups);
        // The selection followed doc c to its new place.
        assert_eq!(model.selected, Some((1, 0)));
        // Nothing new: no rebuild.
        assert!(!model.update(groups));
    }

    #[test]
    fn a_session_ending_while_open_drops_its_group_and_finishing_badges_its_window() {
        let mut model = Model::default();
        model.toggle(two_groups());
        model.select(Dir::Down, &[1, 2]);
        model.select(Dir::Down, &[1, 2]);
        assert_eq!(model.selected, Some((1, 0)));
        let mut groups = two_groups();
        groups[0].windows[0].finished = true;
        assert!(model.update(groups));
        assert!(model.groups[0].windows[0].finished);
        // ov-b ends: its group goes, and the selection on it clears.
        assert!(model.update(vec![two_groups().remove(0)]));
        assert_eq!(model.groups.len(), 1);
        assert_eq!(model.selected, None);
        // The last one ends too: still open, with nothing to show.
        assert!(model.update(Vec::new()));
        assert!(model.open);
        assert_eq!(groups_line(&model.groups), "[]");
    }

    #[test]
    fn a_window_closing_while_open_drops_its_thumbnail() {
        let mut model = Model::default();
        model.toggle(two_groups());
        model.select(Dir::Down, &[1, 2]);
        model.select(Dir::Down, &[1, 2]);
        model.select(Dir::Right, &[1, 2]);
        assert_eq!(model.selected, Some((1, 1)));
        let mut groups = two_groups();
        groups[1].windows.remove(1);
        assert!(model.update(groups));
        assert_eq!(model.groups[1].windows.len(), 1);
        assert_eq!(model.selected, None);
    }

    #[test]
    fn a_panel_the_user_hid_keeps_its_group() {
        // The snapshot reads every live panel, shown or not: the model has
        // no notion of a hidden panel, so an unchanged group is unchanged.
        let mut model = Model::default();
        model.toggle(two_groups());
        assert!(!model.update(two_groups()));
        assert_eq!(model.groups.len(), 2);
    }

    // ── Layout ──

    fn assert_inside(inner: &Area, outer: &Area) {
        assert!(inner.x >= outer.x && inner.y >= outer.y, "{inner:?} in {outer:?}");
        assert!(inner.x + inner.w <= outer.x + outer.w + 0.01, "{inner:?} in {outer:?}");
        assert!(inner.y + inner.h <= outer.y + outer.h + 0.01, "{inner:?} in {outer:?}");
    }

    fn assert_fits(layout: &Layout, screen: (f64, f64)) {
        let visible = Area {
            x: 0.0,
            y: 0.0,
            w: screen.0,
            h: screen.1,
        };
        assert_inside(&layout.sheet, &visible);
        for row in &layout.rows {
            assert_inside(&row.header, &layout.sheet);
            for tile in &row.tiles {
                assert_inside(&tile.thumb, &layout.sheet);
                assert_inside(&tile.title, &layout.sheet);
                assert_eq!((tile.thumb.w, tile.thumb.h), layout.thumb);
            }
            if let Some((more, _)) = &row.more {
                assert_inside(more, &layout.sheet);
            }
        }
        if let Some((line, _)) = &layout.more_groups {
            assert_inside(line, &layout.sheet);
        }
    }

    #[test]
    fn one_agent_one_window_is_a_small_centered_sheet() {
        let layout = layout(SCREEN, &[1]);
        assert_fits(&layout, SCREEN);
        assert_eq!(layout.thumb, (200.0, 125.0));
        assert_eq!(layout.rows.len(), 1);
        assert_eq!(layout.rows[0].tiles.len(), 1);
        assert_eq!(layout.rows[0].more, None);
        assert_eq!(layout.more_groups, None);
        assert_eq!(layout.sheet.w, 320.0);
        let center = (layout.sheet.x + layout.sheet.w / 2.0, layout.sheet.y + layout.sheet.h / 2.0);
        assert!((center.0 - 720.0).abs() <= 0.5 && (center.1 - 437.5).abs() <= 0.5);
        // The header sits above the thumbnail, the title under it.
        let row = &layout.rows[0];
        assert!(row.header.y > row.tiles[0].thumb.y + row.tiles[0].thumb.h);
        assert!(row.tiles[0].title.y + row.tiles[0].title.h <= row.tiles[0].thumb.y);
    }

    #[test]
    fn three_agents_with_four_windows_each_fit_at_full_size() {
        let layout = layout(SCREEN, &[4, 4, 4]);
        assert_fits(&layout, SCREEN);
        assert_eq!(layout.thumb, (200.0, 125.0));
        assert_eq!(layout.rows.len(), 3);
        assert!(layout.rows.iter().all(|row| row.tiles.len() == 4 && row.more.is_none()));
        assert_eq!(layout.sheet.w, 4.0 * 212.0 - 12.0 + 40.0);
        // Rows never overlap.
        for pair in layout.rows.windows(2) {
            assert!(pair[1].header.y + pair[1].header.h <= pair[0].tiles[0].title.y);
        }
    }

    #[test]
    fn eight_agents_shrink_the_thumbnails_and_count_the_rest() {
        let layout = layout(SCREEN, &[2; 8]);
        assert_fits(&layout, SCREEN);
        assert!(layout.thumb.0 < 200.0);
        assert_eq!(layout.rows.len() + layout.more_groups.map_or(0, |(_, n)| n), 8);
        assert!(layout.sheet.h <= SCREEN.1 * 0.85);
        // A short screen: the smallest size still leaves rows out, counted.
        let short = layout_for((1440.0, 400.0), &[2; 8]);
        assert_fits(&short, (1440.0, 400.0));
        assert_eq!(short.thumb.0, 120.0);
        let (_, left_out) = short.more_groups.expect("rows past the sheet are counted");
        assert_eq!(short.rows.len() + left_out, 8);
        assert!(left_out > 0);
    }

    fn layout_for(screen: (f64, f64), counts: &[usize]) -> Layout {
        layout(screen, counts)
    }

    #[test]
    fn a_row_too_wide_ends_in_a_more_tile() {
        let layout = layout(SCREEN, &[9]);
        assert_fits(&layout, SCREEN);
        let row = &layout.rows[0];
        let (more_area, more) = row.more.expect("the row overflows");
        assert_eq!(row.tiles.len() + more, 9);
        assert!(more_area.x > row.tiles.last().unwrap().thumb.x);
        assert_eq!(layout.shown(), vec![row.tiles.len()]);
    }

    #[test]
    fn no_agents_is_the_smallest_sheet() {
        let layout = layout(SCREEN, &[]);
        assert_fits(&layout, SCREEN);
        assert!(layout.rows.is_empty());
        assert_eq!((layout.sheet.w, layout.sheet.h), (320.0, 84.0));
    }

    #[test]
    fn arrows_select_and_clamp_to_what_is_shown() {
        let mut model = Model::default();
        model.toggle(two_groups());
        assert_eq!(model.select(Dir::Left, &[1, 2]), Some((0, 0)));
        assert_eq!(model.select(Dir::Right, &[1, 2]), Some((0, 0)));
        assert_eq!(model.select(Dir::Down, &[1, 2]), Some((1, 0)));
        assert_eq!(model.select(Dir::Right, &[1, 2]), Some((1, 1)));
        assert_eq!(model.select(Dir::Right, &[1, 2]), Some((1, 1)));
        assert_eq!(model.select(Dir::Up, &[1, 2]), Some((0, 0)));
        let (g, w) = model.selected.unwrap();
        assert_eq!(model.groups[g].windows[w].title, "doc a");
        assert_eq!(model.select(Dir::Down, &[]), None);
    }

    #[test]
    fn arrows_skip_rows_with_no_thumbnail_and_reach_every_tile() {
        // A session with no resolved window draws no thumbnail: row 0 here.
        let mut model = Model::default();
        model.toggle(vec![
            group("none", vec![]),
            group("two", vec![thumb(1, 10, "a", false), thumb(1, 11, "b", false)]),
            group("one", vec![thumb(1, 20, "c", false)]),
        ]);
        let shown = [0, 2, 1];
        assert_eq!(model.select(Dir::Down, &shown), Some((1, 0)));
        assert_eq!(model.select(Dir::Right, &shown), Some((1, 1)));
        assert_eq!(model.select(Dir::Right, &shown), Some((1, 1)));
        assert_eq!(model.select(Dir::Down, &shown), Some((2, 0)));
        assert_eq!(model.select(Dir::Down, &shown), Some((2, 0)));
        assert_eq!(model.select(Dir::Up, &shown), Some((1, 0)));
        assert_eq!(model.select(Dir::Up, &shown), Some((1, 0)));
        assert_eq!(model.select(Dir::Left, &shown), Some((1, 0)));
        // Return acts on the selected thumbnail: a real one.
        let (g, w) = model.selected.unwrap();
        assert_eq!(model.groups[g].windows[w].tag, (Some(1), Some(10)));
        // With shown = [0, 1] the second row is reachable at once.
        model.selected = None;
        assert_eq!(model.select(Dir::Down, &[0, 1]), Some((1, 0)));
    }

    #[test]
    fn a_rebuild_moves_a_selection_that_is_no_longer_drawn() {
        // Row overflow: 4 sessions fit at 1440x875; an alphabetically
        // earlier 5th arrives and the selected 4th row is pushed off.
        let mut model = Model::default();
        let four: Vec<Group> = ["b", "c", "d", "e"].iter().map(|l| group(l, vec![thumb(1, 1, "w", false)])).collect();
        model.toggle(four.clone());
        for _ in 0..4 {
            model.select(Dir::Down, &[1, 1, 1, 1]);
        }
        assert_eq!(model.selected, Some((3, 0)));
        let mut five = vec![group("a", vec![thumb(1, 2, "w", false)])];
        five.extend(four);
        assert!(model.update(five.clone()));
        assert_eq!(model.selected, Some((4, 0)), "the selection followed its session");
        let counts: Vec<usize> = five.iter().map(|g| g.windows.len()).collect();
        let shown = layout(SCREEN, &counts).shown();
        assert!(shown.len() < 5, "the fifth row does not fit: {shown:?}");
        assert_eq!(model.clamp(&shown), Some((shown.len() - 1, 0)));
        // The "+n more" tile: the selected 9th thumbnail is behind it.
        let mut model = Model::default();
        let wide = vec![group("x", (0..9).map(|i| thumb(1, i, "w", false)).collect())];
        model.toggle(wide.clone());
        model.selected = Some((0, 8));
        let shown = layout(SCREEN, &[9]).shown();
        assert!(shown[0] < 9);
        assert_eq!(model.clamp(&shown), Some((0, shown[0] - 1)));
        // A drawn selection is left alone; nothing drawn clears it.
        assert_eq!(model.clamp(&shown), Some((0, shown[0] - 1)));
        assert_eq!(model.clamp(&[0]), None);
    }

    #[test]
    fn screen_rects_flip_to_top_left_origin() {
        let tile = Area {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 125.0,
        };
        // Host window at the visible frame's origin (0, 70) on a 900 pt screen.
        let at = screen_rect(tile, (0.0, 70.0), 900.0);
        assert_eq!((at.x, at.y, at.w, at.h), (100.0, 900.0 - 245.0, 200.0, 125.0));
    }
}
