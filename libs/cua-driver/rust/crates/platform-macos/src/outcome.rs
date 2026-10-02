//! Outcome lines for native actions: what the app shows changed after an
//! action, read from the app before and after it (see
//! [`cua_driver_core::outcome`]).
//!
//! A watch reads a small fixed set of facts about the action's window, never a
//! tree walk: its title, document and the document's file, the close button's
//! edited mark, its sheets, the focused element, the action's own element, and
//! the list, outline or table holding that element or the focus (item names
//! for lists of up to [`MAX_ITEMS`], the selected names). After the action it
//! reads again until the facts settle, then says what differs. A fact that
//! could not be read is unknown and never reported as unchanged.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use core_foundation::string::CFString;
use cua_driver_core::outcome::OutcomeWatch;
use serde_json::Value;

use crate::ax::bindings::{
    advertises_attribute, ax_get_window_id, copy_bool_attr,
    copy_children, copy_element_attr, copy_element_array_attr_checked, copy_string_attr,
    copy_stringish_attr, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};
use crate::ax::cache::{CachedSnapshot, RetainedElement};

/// Read the app this often after the action.
const POLL: Duration = Duration::from_millis(50);
/// With nothing changed by then, stop and say so.
const NO_CHANGE_WAIT: Duration = Duration::from_millis(600);
/// The same for a command (a menu item, a key shortcut): the app runs it
/// after the menu closes, and its work (a Finder file operation) can show
/// later than a click's. A Move to Trash once showed after 0.6 s.
const COMMAND_NO_CHANGE_WAIT: Duration = Duration::from_millis(1500);
/// Once something changed: settled when reads agree for this long.
const STABLE_FOR: Duration = Duration::from_millis(200);
/// Never wait longer than this for the app to settle.
const SETTLE_DEADLINE: Duration = Duration::from_secs(2);
/// One read of every fact gives up after this (an app that does not answer).
const READ_BUDGET: Duration = Duration::from_millis(150);
/// Item names are read for lists of at most this many items.
const MAX_ITEMS: usize = 60;
/// Selected names read at most.
const MAX_SELECTED: usize = 30;
/// Names shown in one list in the line.
const MAX_SHOWN: usize = 12;
/// Text controls longer than this are compared by length only (reading a
/// long document's whole text every poll would be slow).
const MAX_TEXT_CHARS: usize = 20_000;
/// Files compared with the window's text up to this size.
const MAX_FILE_BYTES: u64 = 256 * 1024;

extern "C" {
    fn AXUIElementGetAttributeValueCount(
        element: AXUIElementRef,
        attribute: core_foundation::string::CFStringRef,
        count: *mut isize,
    ) -> i32;
}

// ---------------------------------------------------------------------------
// Facts and the line (pure)

/// One element as the line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Element {
    pub role: String,
    pub label: String,
    /// AXValue as text (numbers as "0"/"1"); `None` when it has none, or
    /// when a text control holds more than [`MAX_TEXT_CHARS`].
    pub value: Option<String>,
    /// A long text control's length, its value left unread.
    pub length: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Collection {
    pub role: String,
    pub label: String,
    /// Item names, when the list has at most [`MAX_ITEMS`] and all were read.
    pub items: Option<Vec<String>>,
    pub count: Option<usize>,
    /// Selected names, when the list reports its selection.
    pub selected: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq)]
pub(crate) struct OtherWindow {
    pub id: u32,
    pub title: String,
}

impl PartialEq for OtherWindow {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub modified: SystemTime,
    pub len: u64,
}

/// The facts one read gathered. `None` is unknown (or absent), never "same".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// `Some(false)`: the app answered and the window is not among its windows.
    pub window_present: Option<bool>,
    pub title: Option<String>,
    /// The window's document as a decoded file path.
    pub document: Option<String>,
    /// The document file's stamp, when it may be read without a privacy prompt.
    pub file: Option<FileStamp>,
    pub edited: Option<bool>,
    pub sheets: Option<Vec<String>>,
    /// The app's other windows (an action can open one: Settings, a new
    /// document). Compared by id; a title change elsewhere is not a change.
    pub windows: Option<Vec<OtherWindow>>,
    pub focus: Option<Element>,
    pub target: Option<Element>,
    pub collection: Option<Collection>,
    /// The app's on-screen menu windows (pop-up menus, context menus, a
    /// menu bar menu), by id.
    pub menus: Option<Vec<u32>>,
    /// The element that opened the menu the action picks from (a pop-up
    /// button), read before and after the pick.
    pub opener: Option<Element>,
}

/// What the disk adds to a settled change (read once, after the facts).
/// Folder contents are not read: Finder's items carry file reference URLs,
/// and resolving one to a path could touch a protected folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiskNotes {
    /// The document file's text compared with the window's text area.
    pub file_text: Option<FileText>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileText {
    Matches(usize),
    Differs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settle {
    /// Nothing changed within [`NO_CHANGE_WAIT`].
    Unchanged,
    /// A command: nothing changed within [`COMMAND_NO_CHANGE_WAIT`], which
    /// does not show it did nothing (it can finish later).
    CommandUnchanged,
    /// Something changed and settled, then the last read matched the start
    /// again: the wait ended early, so neither "changed" nor "unchanged".
    Reverted,
    /// Changed, then reads agreed for [`STABLE_FOR`].
    Settled,
    /// Still changing at [`SETTLE_DEADLINE`].
    StillChanging,
}

fn quote(text: &str) -> String {
    serde_json::json!(text).to_string()
}

/// A value as the line shows it: short values whole, long ones by length and
/// their end (where typing lands).
fn shown_value(value: &str) -> String {
    let count = value.chars().count();
    if count <= 80 {
        quote(value)
    } else {
        let tail: String = value.chars().skip(count - 40).collect();
        format!("{count} characters ending {}", quote(&format!("…{tail}")))
    }
}

fn names(list: &[String]) -> String {
    let mut shown: Vec<String> = list.iter().take(MAX_SHOWN).cloned().collect();
    if list.len() > MAX_SHOWN {
        shown.push(format!("and {} more", list.len() - MAX_SHOWN));
    }
    shown.join(", ")
}

fn element_name(element: &Element) -> String {
    if element.label.is_empty() {
        element.role.clone()
    } else {
        format!("{} {}", element.role, quote(&element.label))
    }
}

fn toggle_word(role: &str, value: &str) -> &'static str {
    match (role, value) {
        ("AXCheckBox" | "AXRadioButton", "0") => " (off)",
        ("AXCheckBox" | "AXRadioButton", "1") => " (on)",
        ("AXCheckBox", "2") => " (mixed)",
        _ => "",
    }
}

fn same_element(a: &Element, b: &Element) -> bool {
    a.role == b.role && a.label == b.label
}

fn same_collection(a: &Collection, b: &Collection) -> bool {
    a.role == b.role && a.label == b.label
}

fn subtract(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|name| !b.contains(name)).cloned().collect()
}

fn selection_line(collection: &Collection) -> Option<String> {
    let selected = collection.selected.as_ref()?;
    let of = collection
        .count
        .map(|count| format!(" of {count}"))
        .unwrap_or_default();
    Some(if selected.is_empty() {
        format!("nothing selected in {}", collection.label)
    } else {
        format!(
            "selected in {}: {} ({}{of})",
            collection.label,
            names(selected),
            selected.len()
        )
    })
}

fn listing_line(collection: &Collection, verb: &str) -> Option<String> {
    let items = collection.items.as_ref()?;
    Some(if items.is_empty() {
        format!("{} {verb}: no items", collection.label)
    } else {
        format!("{} {verb}: {} ({})", collection.label, names(items), items.len())
    })
}

/// The outcome line for `before` and `after`. `complete`: every fact was
/// read both times within the budget.
pub(crate) fn describe(
    before: &Facts,
    after: &Facts,
    disk: &DiskNotes,
    settle: Settle,
    complete: bool,
) -> String {
    if before.window_present == Some(true) && after.window_present == Some(false) {
        return "this window closed".into();
    }
    let mut parts: Vec<String> = Vec::new();
    let navigated = matches!((&before.title, &after.title), (Some(a), Some(b)) if a != b);
    if navigated {
        parts.push(format!("window title now {}", quote(after.title.as_deref().unwrap_or(""))));
    }
    if let (Some(path), true) = (&after.document, before.document != after.document) {
        match &before.document {
            Some(old) => parts.push(format!("document now {path} (was {old})")),
            None => parts.push(format!("document now {path}")),
        }
    }
    let same_document = before.document.is_some() && before.document == after.document;
    let holds = match &disk.file_text {
        Some(FileText::Matches(chars)) => Some(format!("it holds the window's text ({chars} characters)")),
        Some(FileText::Differs) => Some("its text differs from the window's".to_owned()),
        None => None,
    };
    match (before.file, after.file, &after.document) {
        // The same file's stamp moved: it was written during the action.
        (Some(a), Some(b), Some(path)) if same_document && a != b => {
            let mut line = format!("file {path} modified during the action");
            if let Some(holds) = &holds {
                line.push_str(&format!("; {holds}"));
            }
            parts.push(line);
        }
        // A new document (Save As, a first save): its file's text, never a
        // claim about when it was written.
        (_, Some(_), Some(_)) if !same_document => {
            if let Some(holds) = &holds {
                parts.push(format!("its file: {holds}"));
            }
        }
        _ => {}
    }
    match (before.edited, after.edited) {
        (Some(true), Some(false)) => parts.push("no unsaved changes".into()),
        (Some(false), Some(true)) => parts.push("unsaved changes".into()),
        _ => {}
    }
    if let (Some(a), Some(b)) = (&before.sheets, &after.sheets) {
        let said = |entry: &str, verb: &str| match entry.split_once(": ") {
            Some((kind, name)) => format!("{kind} {verb}: {name}"),
            None => format!("{entry} {verb}"),
        };
        for surface in subtract(b, a) {
            parts.push(said(&surface, "opened"));
        }
        for surface in subtract(a, b) {
            parts.push(said(&surface, "closed"));
        }
    }
    match (&before.collection, &after.collection) {
        (Some(a), Some(b)) if same_collection(a, b) && !navigated => {
            if let (Some(old), Some(new)) = (&a.items, &b.items) {
                let added = subtract(new, old);
                let gone = subtract(old, new);
                if !added.is_empty() || !gone.is_empty() {
                    let mut line = listing_line(b, "now").unwrap_or_default();
                    if !added.is_empty() {
                        line.push_str(&format!("; added {}", names(&added)));
                    }
                    if !gone.is_empty() {
                        line.push_str(&format!("; gone from the list: {}", names(&gone)));
                    }
                    parts.push(line);
                }
            }
            if a.selected != b.selected {
                parts.extend(selection_line(b));
            }
        }
        (_, Some(b)) => {
            // A different list, or the same window showing another folder:
            // no diff, the new listing and selection.
            let listing = listing_line(b, "shows");
            let shows_change = navigated
                || before.collection.as_ref().is_none_or(|a| !same_collection(a, b));
            if shows_change {
                parts.extend(listing);
                if b.selected.as_ref().is_some_and(|s| !s.is_empty()) {
                    parts.extend(selection_line(b));
                }
            }
        }
        (Some(a), None) => parts.push(format!("{} is no longer readable (replaced or closed)", a.label)),
        _ => {}
    }
    if let (Some(a), Some(b)) = (&before.windows, &after.windows) {
        for w in b.iter().filter(|w| !a.contains(w)) {
            parts.push(format!("window opened: {} (window_id {})", quote(&w.title), w.id));
        }
        for w in a.iter().filter(|w| !b.contains(w)) {
            parts.push(format!("window closed: {} (window_id {})", quote(&w.title), w.id));
        }
    }
    // The target is the same retained element both times, so a new label
    // (a disclosure triangle's "show more" becoming "show less") is still it.
    if let (Some(a), Some(b)) = (&before.target, &after.target) {
        let it = a.role == b.role;
        if it && a.length != b.length {
            parts.extend(length_line(a, b));
        } else if it && a.value == b.value && a.label != b.label {
            parts.push(format!("{} is now labelled {}", element_name(a), quote(&b.label)));
        } else if it && a.value != b.value {
            if let Some(value) = &b.value {
                let was = a
                    .value
                    .as_deref()
                    .map(|old| format!(", was {}{}", short(old), toggle_word(&a.role, old)))
                    .unwrap_or_default();
                parts.push(format!(
                    "{} now {}{}{was}",
                    element_name(b),
                    short(value),
                    toggle_word(&b.role, value)
                ));
            }
        }
    }
    if let (Some(a), Some(b)) = (&before.opener, &after.opener) {
        if a.label != b.label {
            parts.push(format!("{} is now labelled {}", element_name(a), quote(&b.label)));
        } else if a.value != b.value {
            if let Some(value) = &b.value {
                parts.push(format!("{} now shows {}", element_name(b), shown_value(value)));
            }
        } else if let Some(value) = b.value.as_deref().filter(|v| !v.is_empty()) {
            parts.push(format!("{} still shows {} (the pick did not change it)", element_name(b), quote(value)));
        } else {
            // No readable choice: an unchanged label is not proof either way.
            parts.push(format!(
                "{} kept its label; whether the pick took is not readable",
                element_name(b)
            ));
        }
    }
    if let (Some(a), Some(b)) = (&before.menus, &after.menus) {
        if b.iter().any(|id| !a.contains(id)) {
            parts.push("a menu opened".into());
        } else if !a.is_empty() && b.is_empty() {
            parts.push("menu closed".into());
        }
    }
    let target_is_focus = matches!((&after.target, &after.focus), (Some(t), Some(f)) if same_element(t, f));
    match (&before.focus, &after.focus) {
        (Some(a), Some(b)) if same_element(a, b) => {
            if a.length != b.length && !target_is_focus {
                parts.extend(length_line(a, b));
            } else if a.value != b.value && !target_is_focus {
                if let Some(value) = &b.value {
                    parts.push(format!("{} now {}", element_name(b), shown_value(value)));
                }
            }
        }
        (_, Some(b)) if !target_is_focus => {
            let mut line = format!("focus: {}", element_name(b));
            if let Some(value) = b.value.as_deref().filter(|_| is_text_role(&b.role)) {
                line.push_str(&format!(" = {}", shown_value(value)));
            }
            parts.push(line);
        }
        _ => {}
    }
    // A sheet or popover open before and after is still waiting for the
    // agent (a rename popover a confirm did not close): say so every time.
    let mut still_open: Vec<String> = match (&before.sheets, &after.sheets) {
        (Some(a), Some(b)) => b.iter().filter(|s| a.contains(s)).cloned().collect(),
        _ => Vec::new(),
    };
    // A menu open before and after (one a failed menu path left behind
    // swallows later keys and menu commands).
    if let (Some(a), Some(b)) = (&before.menus, &after.menus) {
        if b.iter().any(|id| a.contains(id)) {
            still_open.push("a menu".into());
        }
    }
    let still_open = (!still_open.is_empty()).then(|| format!("still open: {}", names(&still_open)));
    if parts.is_empty() && before != after {
        let mut line =
            "the window changed in a way this line does not describe; read it if it matters".to_owned();
        if let Some(still) = &still_open {
            line.push_str(&format!("; {still}"));
        }
        return line;
    }
    if parts.is_empty() {
        let command = settle == Settle::CommandUnchanged;
        let seconds = if command { COMMAND_NO_CHANGE_WAIT } else { NO_CHANGE_WAIT }.as_secs_f32();
        let mut line = if complete && settle == Settle::Reverted {
            "a change came and went (focus, selection, list items, values, title, document, sheets, popovers, windows); not settled: read the window before repeating the action".to_owned()
        } else if complete && command {
            format!(
                "no change seen within {seconds:.1} s (focus, selection, list items, values, title, document, sheets, popovers, windows); not settled: a command can finish later, so read the window before repeating it"
            )
        } else if complete {
            format!(
                "nothing it watches changed within {seconds:.1} s (focus, selection, list items, values, title, document, sheets, popovers, menus, windows)"
            )
        } else {
            format!("no change seen within {seconds:.1} s, but the app did not answer every read")
        };
        if after.document.is_some() && after.file.is_none() {
            line.push_str("; the document's file was not checked (protected folder or unreadable)");
        }
        if let Some(still) = &still_open {
            line.push_str(&format!("; {still}"));
        }
        return line;
    }
    parts.extend(still_open);
    let mut line = parts.join("; ");
    if settle == Settle::StillChanging {
        line.push_str(&format!(
            "; still changing after {:.0} s",
            SETTLE_DEADLINE.as_secs_f32()
        ));
    }
    line
}

/// A long text control's change, by length.
fn length_line(before: &Element, after: &Element) -> Option<String> {
    let now = after.length.or_else(|| after.value.as_ref().map(|v| v.chars().count()))?;
    let was = before.length.or_else(|| before.value.as_ref().map(|v| v.chars().count()));
    Some(match was {
        Some(was) => format!("{} now {now} characters, was {was}", element_name(after)),
        None => format!("{} now {now} characters", element_name(after)),
    })
}

/// A control value in the line (checkbox states and short text whole).
fn short(value: &str) -> String {
    if value.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') && !value.is_empty() {
        value.to_owned()
    } else {
        shown_value(value)
    }
}

fn is_text_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
}

// ---------------------------------------------------------------------------
// Disk (only where reading cannot raise a privacy prompt)

/// Folders macOS guards with a consent prompt (or that hold app data).
const PROTECTED_HOME_FOLDERS: &[&str] = &[
    "Desktop", "Documents", "Downloads", "Library", "Pictures", "Movies", "Music", "Public",
];

/// The local path a `file://` URL names, when reading it cannot raise a
/// privacy prompt: decoded, absolute, without `..`, under the home folder but
/// outside its protected folders and hidden folders, or under /tmp, and not a
/// symlink. Everything else stays unread.
pub(crate) fn readable_path(url_or_path: &str, home: &Path) -> Option<PathBuf> {
    let raw = match url_or_path.strip_prefix("file://") {
        Some(rest) => {
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            crate::tools::launch_app::percent_decode_path(rest)
        }
        None => url_or_path.to_owned(),
    };
    let path = PathBuf::from(raw.trim_end_matches('/'));
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let allowed = if let Ok(rest) = path.strip_prefix(home) {
        match rest.components().next() {
            Some(Component::Normal(first)) => {
                let first = first.to_string_lossy();
                !first.starts_with('.') && !PROTECTED_HOME_FOLDERS.contains(&first.as_ref())
            }
            _ => false,
        }
    } else {
        path.starts_with("/tmp") || path.starts_with("/private/tmp")
    };
    allowed.then_some(path)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(FileStamp {
        modified: meta.modified().ok()?,
        len: meta.len(),
    })
}

// ---------------------------------------------------------------------------
// Reading the app

/// A retained AX element that is released when dropped.
struct Owned(AXUIElementRef);

/// An element's children, each owned before any of them is looked at, so an
/// early stop releases the rest.
unsafe fn kids(element: AXUIElementRef) -> Vec<Owned> {
    copy_children(element).into_iter().map(Owned).collect()
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

struct Reader {
    deadline: Instant,
    complete: bool,
}

impl Reader {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + READ_BUDGET,
            complete: true,
        }
    }

    /// Bound the next message to `element` by what is left of the budget.
    /// `false` (and the read is incomplete) once the budget is spent.
    fn admit(&mut self, element: AXUIElementRef) -> bool {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(2) {
            self.complete = false;
            return false;
        }
        unsafe {
            AXUIElementSetMessagingTimeout(element, left.min(Duration::from_millis(100)).as_secs_f32())
        };
        true
    }
}

unsafe fn text_attr(element: AXUIElementRef, name: &str) -> Option<String> {
    copy_string_attr(element, name)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

unsafe fn own_label(element: AXUIElementRef) -> Option<String> {
    text_attr(element, "AXTitle")
        .or_else(|| text_attr(element, "AXDescription"))
        .or_else(|| text_attr(element, "AXValue"))
}

/// An item's name: its own title, description or text, else the first one
/// a child or grandchild has (a Finder icon's image, a table row's cell).
unsafe fn item_name(reader: &mut Reader, element: AXUIElementRef) -> Option<String> {
    if !reader.admit(element) {
        return None;
    }
    if let Some(label) = own_label(element) {
        return Some(label);
    }
    for child in kids(element).into_iter().take(6) {
        if !reader.admit(child.0) {
            return None;
        }
        if let Some(label) = own_label(child.0) {
            return Some(label);
        }
        for grandchild in kids(child.0).into_iter().take(6) {
            if !reader.admit(grandchild.0) {
                return None;
            }
            if let Some(label) = own_label(grandchild.0) {
                return Some(label);
            }
        }
    }
    None
}

unsafe fn read_element(reader: &mut Reader, element: AXUIElementRef) -> Option<Element> {
    if !reader.admit(element) {
        return None;
    }
    let role = copy_string_attr(element, "AXRole")?;
    let label = text_attr(element, "AXTitle")
        .or_else(|| text_attr(element, "AXDescription"))
        .or_else(|| text_attr(element, "AXIdentifier").filter(|id| !id.starts_with("_NS:")))
        .unwrap_or_default();
    let length = is_text_role(&role)
        .then(|| crate::ax::bindings::copy_number_attr(element, "AXNumberOfCharacters"))
        .flatten()
        .map(|n| n as usize)
        .filter(|&n| n > MAX_TEXT_CHARS);
    let value = match length {
        Some(_) => None,
        None => copy_stringish_attr(element, "AXValue").map(|value| value.state_value),
    };
    Some(Element { role, label, value, length })
}

const COLLECTION_ROLES: &[&str] = &["AXList", "AXOutline", "AXTable", "AXGrid", "AXBrowser"];
/// Walking up stops here. A menu is not a list the action changes (it
/// closes when its item is chosen), nor is a toolbar (its buttons act on the
/// window's content, which the focus finds).
const STOP_ROLES: &[&str] = &[
    "AXWindow", "AXSheet", "AXApplication", "AXSystemWide", "AXMenu", "AXMenuBar", "AXMenuBarItem",
    "AXToolbar",
];

/// The list, outline or table holding `start` (itself or an ancestor up to
/// six levels), retained: the nearest one that reports a selection, else the
/// nearest list-like role. Finder's icon view nests an unnamed section list
/// inside the list that owns the selection.
unsafe fn find_collection(reader: &mut Reader, start: AXUIElementRef) -> Option<Owned> {
    core_foundation::base::CFRetain(start as CFTypeRef);
    let mut current = Owned(start);
    let mut fallback = None;
    for _ in 0..7 {
        if !reader.admit(current.0) {
            return fallback;
        }
        let role = copy_string_attr(current.0, "AXRole").unwrap_or_default();
        if STOP_ROLES.contains(&role.as_str()) {
            return fallback;
        }
        if advertises_attribute(current.0, "AXSelectedChildren")
            || advertises_attribute(current.0, "AXSelectedRows")
        {
            return Some(current);
        }
        let parent = copy_element_attr(current.0, "AXParent").map(Owned);
        if fallback.is_none() && COLLECTION_ROLES.contains(&role.as_str()) {
            fallback = Some(current);
        }
        current = parent?;
    }
    fallback
}

unsafe fn attribute_count(element: AXUIElementRef, name: &str) -> Option<usize> {
    let attr = CFString::new(name);
    let mut count: isize = 0;
    let error = AXUIElementGetAttributeValueCount(
        element,
        core_foundation::base::TCFType::as_concrete_TypeRef(&attr),
        &mut count,
    );
    (error == 0 && count >= 0).then_some(count as usize)
}

/// A list's items, through unnamed single sections (Finder's icon view
/// keeps its items in one unnamed inner list). `Err(count)` when there are
/// more than [`MAX_ITEMS`] (with the count when known) or the read failed.
unsafe fn list_items(
    reader: &mut Reader,
    element: AXUIElementRef,
    attribute: &str,
) -> Result<Vec<Owned>, Option<usize>> {
    let read = |element: AXUIElementRef, attribute: &str| -> Result<Vec<Owned>, Option<usize>> {
        let count = attribute_count(element, attribute);
        if count.is_none_or(|count| count > MAX_ITEMS) {
            return Err(count);
        }
        copy_element_array_attr_checked(element, attribute, MAX_ITEMS)
            .map(|items| items.into_iter().map(Owned).collect())
            .map_err(|_| None)
    };
    // A context menu opened over a list is its child while it shows; it is
    // not an item.
    let not_menu = |reader: &mut Reader, items: Vec<Owned>| -> Result<Vec<Owned>, Option<usize>> {
        let mut kept = Vec::with_capacity(items.len());
        for item in items {
            if !reader.admit(item.0) {
                return Err(None);
            }
            if copy_string_attr(item.0, "AXRole").as_deref() != Some("AXMenu") {
                kept.push(item);
            }
        }
        Ok(kept)
    };
    let mut items = not_menu(reader, read(element, attribute)?)?;
    for _ in 0..2 {
        let [only] = items.as_slice() else { break };
        if !reader.admit(only.0) {
            return Err(None);
        }
        let role = copy_string_attr(only.0, "AXRole").unwrap_or_default();
        if !matches!(role.as_str(), "AXList" | "AXGroup") || own_label(only.0).is_some() {
            break;
        }
        items = not_menu(reader, read(only.0, "AXChildren")?)?;
    }
    Ok(items)
}

/// A collection's facts.
unsafe fn read_collection(reader: &mut Reader, element: AXUIElementRef) -> Collection {
    let role = copy_string_attr(element, "AXRole").unwrap_or_default();
    let label = text_attr(element, "AXDescription")
        .or_else(|| text_attr(element, "AXTitle"))
        .or_else(|| text_attr(element, "AXIdentifier").filter(|id| !id.starts_with("_NS:")))
        .unwrap_or_else(|| role.clone());
    let rows = matches!(role.as_str(), "AXOutline" | "AXTable");
    let (items_attr, selected_attr) = if rows {
        ("AXRows", "AXSelectedRows")
    } else {
        ("AXChildren", "AXSelectedChildren")
    };
    let (items, count) = match list_items(reader, element, items_attr) {
        Ok(items) => {
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                // An unnamed item is a spacer; a read cut by the budget makes
                // the whole list unknown.
                match item_name(reader, item.0) {
                    Some(name) => names.push(name),
                    None if reader.complete => {}
                    None => break,
                }
            }
            let count = names.len();
            (reader.complete.then_some(names), reader.complete.then_some(count))
        }
        Err(count) => (None, count),
    };
    let selected = (reader.admit(element)
        && advertises_attribute(element, selected_attr))
    .then(|| match copy_element_array_attr_checked(element, selected_attr, MAX_SELECTED) {
        Ok(selected) => {
            let mut out = Vec::new();
            let selected: Vec<Owned> = selected.into_iter().map(Owned).collect();
            for item in selected {
                out.push(item_name(reader, item.0)?);
            }
            out.sort();
            Some(out)
        }
        // No selection reads as "no value" in some lists.
        Err(error) if error == crate::ax::bindings::kAXErrorNoValue => Some(Vec::new()),
        Err(_) => None,
    })
    .flatten();
    Collection {
        role,
        label,
        items,
        count,
        selected,
    }
}

/// What one read needs to know about the action.
#[derive(Clone)]
struct Scope {
    pid: i32,
    window_id: u32,
    target: Option<usize>,
    /// The list the watch began with (retained by the watch), read again
    /// after the action even when focus moved out of it (Finder's rename
    /// field is a child window with no list above it).
    collection: Option<usize>,
    /// For a pick from a menu: the element that opened it (retained by the
    /// watch).
    opener: Option<usize>,
    /// Whether the app's menu windows are watched. Not for invoke_menu: the
    /// menu it walks fades out after the command, and reading that as "a
    /// menu opened" would also end the wait before a sheet the command opens.
    menus: bool,
    /// A menu command or key shortcut: waits [`COMMAND_NO_CHANGE_WAIT`].
    command: bool,
}

/// One read of every fact; `keep` also returns the retained window for the
/// disk notes.
struct Pass {
    facts: Facts,
    complete: bool,
    window: Option<Owned>,
    /// The list this read used.
    holder: Option<RetainedElement>,
}

unsafe fn read_pass(scope: &Scope, keep: bool) -> Pass {
    let mut reader = Reader::new();
    let mut facts = Facts::default();
    let app = AXUIElementCreateApplication(scope.pid);
    if app.is_null() {
        return Pass { facts, complete: false, window: None, holder: None };
    }
    let app = Owned(app);
    let mut window = None;
    let windows = if reader.admit(app.0) && copy_string_attr(app.0, "AXRole").is_some() {
        app_windows(app.0, scope.pid, scope.window_id)
    } else {
        None
    };
    if let Some(windows) = windows {
        // Ids first, for every window: presence never depends on the budget.
        let mut others = Vec::new();
        let mut rest = Vec::new();
        for w in windows {
            match ax_get_window_id(w.0) {
                Some(id) if id == scope.window_id => window = Some(w),
                Some(id) => rest.push((id, w)),
                None => {}
            }
        }
        let mut titled = true;
        for (id, w) in rest {
            if !reader.admit(w.0) {
                titled = false;
                break;
            }
            let title = copy_string_attr(w.0, "AXTitle").unwrap_or_default();
            others.push(OtherWindow { id, title });
        }
        others.sort_by_key(|w| w.id);
        facts.windows = titled.then_some(others);
        facts.window_present = Some(window.is_some());
    } else {
        reader.complete = false;
    }
    if let Some(w) = &window {
        if reader.admit(w.0) {
            facts.title = copy_string_attr(w.0, "AXTitle");
            if let Some(url) = copy_string_attr(w.0, "AXDocument") {
                // A document in a protected folder is still named; its file
                // is never touched.
                let readable = home().and_then(|home| readable_path(&url, &home));
                facts.file = readable.as_deref().and_then(file_stamp);
                facts.document = Some(match readable {
                    Some(path) => path.to_string_lossy().into_owned(),
                    None => match url.strip_prefix("file://") {
                        Some(rest) => crate::tools::launch_app::percent_decode_path(
                            rest.strip_prefix("localhost").unwrap_or(rest),
                        ),
                        None => url,
                    },
                });
            }
        }
        if let Some(close) = copy_element_attr(w.0, "AXCloseButton").map(Owned) {
            if reader.admit(close.0) {
                facts.edited = copy_bool_attr(close.0, "AXEdited");
            }
        }
        if reader.admit(w.0) {
            let mut sheets = Vec::new();
            for child in kids(w.0) {
                if !reader.admit(child.0) {
                    break;
                }
                let role = copy_string_attr(child.0, "AXRole");
                if role.as_deref() == Some("AXPopover") {
                    sheets.push(surface("popover", child.0));
                }
                if role.as_deref() == Some("AXSheet") {
                    sheets.push(surface("sheet", child.0));
                    // A sheet on a sheet (Go to Folder over a Save panel).
                    for inner in kids(child.0) {
                        if !reader.admit(inner.0) {
                            break;
                        }
                        if copy_string_attr(inner.0, "AXRole").as_deref() == Some("AXSheet") {
                            sheets.push(surface("sheet", inner.0));
                        }
                    }
                }
            }
            facts.sheets = reader.complete.then_some(sheets);
        }
    }
    let focus = if facts.window_present == Some(true) {
        focused_in_window(&mut reader, app.0, scope.window_id)
    } else {
        None
    };
    if let Some(f) = &focus {
        facts.focus = read_element(&mut reader, f.0);
        if facts.focus.as_ref().is_some_and(|f| !is_text_role(&f.role)) {
            // Only text controls show their value as focus.
            if let Some(f) = facts.focus.as_mut() {
                f.value = None;
            }
        }
    }
    if let Some(target) = scope.target {
        facts.target = read_element(&mut reader, target as AXUIElementRef);
    }
    if let Some(opener) = scope.opener {
        facts.opener = read_element(&mut reader, opener as AXUIElementRef);
    }
    facts.menus = scope.menus.then(|| crate::windows::menu_windows_of(scope.pid)).flatten().map(|windows| {
        let mut ids: Vec<u32> = windows.iter().map(|w| w.window_id).collect();
        ids.sort_unstable();
        ids
    });
    let alive = |ptr: usize| crate::ax::bindings::element_is_alive(ptr as AXUIElementRef);
    let holder = scope
        .collection
        .filter(|&ptr| alive(ptr))
        .map(|ptr| Owned(retained(ptr)))
        .or_else(|| scope.target.and_then(|t| find_collection(&mut reader, t as AXUIElementRef)))
        .or_else(|| focus.as_ref().and_then(|f| find_collection(&mut reader, f.0)))
        .or_else(|| remembered_collection(scope.pid, scope.window_id).map(Owned));
    let mut used = None;
    if let Some(list) = holder {
        facts.collection = Some(read_collection(&mut reader, list.0));
        used = Some(RetainedElement::retain(list.0 as usize));
    }
    // The target is the element cache's own object, which the action is
    // about to use: give it back the action timeout the reads shortened.
    if let Some(target) = scope.target {
        AXUIElementSetMessagingTimeout(
            target as AXUIElementRef,
            crate::ax::tree::AX_MESSAGING_TIMEOUT_SECONDS,
        );
    }
    Pass {
        facts,
        complete: reader.complete,
        window: if keep { window } else { None },
        holder: used,
    }
}

/// A sheet or popover as the line names it: "sheet: save", "popover".
unsafe fn surface(kind: &str, element: AXUIElementRef) -> String {
    match text_attr(element, "AXDescription")
        .or_else(|| text_attr(element, "AXTitle"))
        .or_else(|| text_attr(element, "AXIdentifier").filter(|id| !id.starts_with("_NS:")))
    {
        Some(name) => format!("{kind}: {name}"),
        None => kind.to_owned(),
    }
}

/// The app's windows, with the requested one even on another Space; `None`
/// when the app did not answer (never read as "no windows").
unsafe fn app_windows(app: AXUIElementRef, pid: i32, window_id: u32) -> Option<Vec<Owned>> {
    let mut windows: Vec<Owned> = match copy_element_array_attr_checked(app, "AXWindows", 256) {
        Ok(windows) => windows.into_iter().map(Owned).collect(),
        Err(error) if error == crate::ax::bindings::kAXErrorNoValue => Vec::new(),
        Err(_) => return None,
    };
    if !windows.iter().any(|w| ax_get_window_id(w.0) == Some(window_id)) {
        windows.extend(crate::ax::bindings::copy_ax_window_by_remote_token(pid, window_id).map(Owned));
    }
    Some(windows)
}

/// The focused element when it is in `window_id` (or a child window of it),
/// every message bounded by the read's budget.
unsafe fn focused_in_window(reader: &mut Reader, app: AXUIElementRef, window_id: u32) -> Option<Owned> {
    if !reader.admit(app) {
        return None;
    }
    let focused = Owned(copy_element_attr(app, "AXFocusedUIElement")?);
    if !reader.admit(focused.0) {
        return None;
    }
    let in_window = crate::ax::exact_target::element_window_id(focused.0)
        .is_some_and(|id| crate::ax::bindings::window_belongs_to(id, window_id));
    in_window.then_some(focused)
}

/// The app's focused window, for an action that named none; bounded.
unsafe fn focused_window_id(pid: i32) -> Option<u32> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let app = Owned(app);
    let mut reader = Reader::new();
    if !reader.admit(app.0) {
        return None;
    }
    let window = Owned(copy_element_attr(app.0, "AXFocusedWindow")?);
    reader.admit(window.0).then(|| crate::ax::bindings::surface_window_id(window.0)).flatten()
}

/// The menu a cua action opened, per window: the element pressed and the
/// menu windows that appeared. A pick from a detached menu (a Catalyst
/// pop-up's menu has no AX parent) is known to come from that window only
/// through this record.
struct MenuOpener {
    pid: i32,
    window_id: u32,
    opener: RetainedElement,
    menus: Vec<u32>,
}

/// ponytail: one record per window, 16 windows, oldest dropped.
static MENU_OPENERS: std::sync::Mutex<Vec<MenuOpener>> = std::sync::Mutex::new(Vec::new());

/// The app's on-screen menu window ids, for [`note_menu_opened`].
pub(crate) fn menu_window_ids(pid: i32) -> Option<Vec<u32>> {
    crate::windows::menu_windows_of(pid).map(|windows| windows.iter().map(|w| w.window_id).collect())
}

/// Called by a press on `element` in window `window_id` while the action
/// still holds the input lock (so no other action can open a menu in
/// between): when menu windows of the app appeared that `before` did not
/// hold, remember `element` as their opener. A pop-up-like element (a
/// pop-up or menu button, or a button that shows a menu) gets up to 250 ms
/// for its menu to appear.
///
/// # Safety
///
/// `element` must be a valid AXUIElementRef for the duration of the call.
pub(crate) unsafe fn note_menu_opened(pid: i32, window_id: u32, element: usize, before: Option<Vec<u32>>) {
    let Some(before) = before else { return };
    let role = copy_string_attr(element as AXUIElementRef, "AXRole").unwrap_or_default();
    if role == "AXMenuItem" {
        return;
    }
    let shows_menu = matches!(role.as_str(), "AXPopUpButton" | "AXMenuButton")
        || (role == "AXButton"
            && crate::ax::bindings::copy_action_names(element as AXUIElementRef)
                .iter()
                .any(|action| action == "AXShowMenu"));
    let deadline = Instant::now() + if shows_menu { Duration::from_millis(250) } else { Duration::ZERO };
    loop {
        let new: Vec<u32> = menu_window_ids(pid)
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        if !new.is_empty() {
            let mut openers = MENU_OPENERS.lock().unwrap_or_else(|e| e.into_inner());
            openers.retain(|o| (o.pid, o.window_id) != (pid, window_id));
            if openers.len() >= 16 {
                openers.remove(0);
            }
            let opener = RetainedElement::retain(element);
            openers.push(MenuOpener { pid, window_id, opener, menus: new });
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether `element` is an item of a menu with no AX parent (or the
/// application as parent): a menu that names no window.
pub(crate) unsafe fn in_detached_menu(element: AXUIElementRef) -> bool {
    core_foundation::base::CFRetain(element as CFTypeRef);
    let mut current = Owned(element);
    for _ in 0..6 {
        let role = copy_string_attr(current.0, "AXRole").unwrap_or_default();
        let parent = copy_element_attr(current.0, "AXParent").map(Owned);
        if role == "AXMenu" {
            return parent
                .as_ref()
                .is_none_or(|p| copy_string_attr(p.0, "AXRole").as_deref() == Some("AXApplication"));
        }
        if matches!(role.as_str(), "AXWindow" | "AXSheet" | "AXApplication" | "") {
            return false;
        }
        match parent {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

/// The opener a cua action on window `window_id` recorded for the menu that
/// holds `element`, when `element` is in a detached menu, the recorded menu
/// window is still on screen and the element's centre lies inside it.
/// Drops records whose menus are gone.
pub(crate) unsafe fn recorded_menu_opener(pid: i32, window_id: u32, element: AXUIElementRef) -> Option<RetainedElement> {
    if !in_detached_menu(element) {
        return None;
    }
    let menus = crate::windows::menu_windows_of(pid)?;
    let mut openers = MENU_OPENERS.lock().unwrap_or_else(|e| e.into_inner());
    openers.retain(|o| o.pid != pid || o.menus.iter().any(|id| menus.iter().any(|m| m.window_id == *id)));
    let record = openers.iter().find(|o| (o.pid, o.window_id) == (pid, window_id))?;
    if !crate::ax::bindings::element_is_alive(record.opener.as_ptr() as AXUIElementRef) {
        return None;
    }
    let (x, y) = crate::ax::bindings::element_screen_center(element)?;
    let inside = menus.iter().filter(|m| record.menus.contains(&m.window_id)).any(|m| {
        let b = &m.bounds;
        x >= b.x && x <= b.x + b.width && y >= b.y && y <= b.y + b.height
    });
    inside.then(|| record.opener.clone())
}

/// What opened the menu `element` is in, for the pick's outcome: an AppKit
/// menu's parent (the pop-up button), or the recorded opener of a detached
/// menu. `None` when `element` is not in a menu.
unsafe fn menu_opener_of(pid: i32, window_id: u32, element: AXUIElementRef) -> Option<RetainedElement> {
    if copy_string_attr(element, "AXRole").as_deref() != Some("AXMenuItem") {
        return None;
    }
    if let Some(menu) = copy_element_attr(element, "AXParent").map(Owned) {
        if copy_string_attr(menu.0, "AXRole").as_deref() == Some("AXMenu") {
            if let Some(parent) = copy_element_attr(menu.0, "AXParent").map(Owned) {
                let role = copy_string_attr(parent.0, "AXRole").unwrap_or_default();
                if !matches!(role.as_str(), "AXApplication" | "AXMenuBarItem" | "AXMenuItem" | "") {
                    return Some(RetainedElement::retain(parent.0 as usize));
                }
            }
        }
    }
    recorded_menu_opener(pid, window_id, element)
}

unsafe fn retained(ptr: usize) -> AXUIElementRef {
    core_foundation::base::CFRetain(ptr as CFTypeRef);
    ptr as AXUIElementRef
}

/// The last list a watch used per window, so an action whose focus has no
/// list above it (Finder's rename field) still reads the list it changes.
/// ponytail: 16 windows, oldest dropped; a window id reused by another
/// window is caught by the element's own window check.
static REMEMBERED: std::sync::Mutex<Vec<((i32, u32), RetainedElement)>> =
    std::sync::Mutex::new(Vec::new());

fn remember_collection(pid: i32, window_id: u32, list: &RetainedElement) {
    let mut remembered = REMEMBERED.lock().unwrap_or_else(|e| e.into_inner());
    remembered.retain(|(key, _)| *key != (pid, window_id));
    if remembered.len() >= 16 {
        remembered.remove(0);
    }
    remembered.push(((pid, window_id), list.clone()));
}

unsafe fn remembered_collection(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let remembered = REMEMBERED.lock().unwrap_or_else(|e| e.into_inner());
    let (_, list) = remembered.iter().find(|(key, _)| *key == (pid, window_id))?;
    let ptr = list.as_ptr() as AXUIElementRef;
    let in_window = crate::ax::exact_target::element_window_id(ptr)
        .is_some_and(|id| crate::ax::bindings::window_belongs_to(id, window_id));
    in_window.then(|| retained(list.as_ptr()))
}

/// The disk notes for a settled change: whether a modified document file
/// holds the window's text.
unsafe fn disk_notes(before: &Facts, pass: &Pass) -> DiskNotes {
    let mut notes = DiskNotes::default();
    let after = &pass.facts;
    if let (Some(path), Some(window)) = (&after.document, &pass.window) {
        if after.file.is_some() && (after.file != before.file || after.document != before.document) {
            notes.file_text = file_text(Path::new(path), window.0);
        }
    }
    notes
}

/// The window's text area (three levels down at most), as TextEdit has it;
/// every message bounded by a fresh read budget.
unsafe fn window_text(window: AXUIElementRef) -> Option<String> {
    let mut reader = Reader::new();
    if !reader.admit(window) {
        return None;
    }
    let mut level = kids(window);
    for _ in 0..3 {
        let mut next = Vec::new();
        for element in level {
            if !reader.admit(element.0) {
                return None;
            }
            if copy_string_attr(element.0, "AXRole").as_deref() == Some("AXTextArea") {
                return copy_string_attr(element.0, "AXValue");
            }
            next.extend(kids(element.0).into_iter().take(12));
        }
        level = next;
    }
    None
}

unsafe fn file_text(path: &Path, window: AXUIElementRef) -> Option<FileText> {
    let stamp = file_stamp(path)?;
    if stamp.len > MAX_FILE_BYTES {
        return None;
    }
    let on_disk = String::from_utf8(std::fs::read(path).ok()?).ok()?;
    let shown = window_text(window)?;
    if on_disk == shown {
        return Some(FileText::Matches(shown.chars().count()));
    }
    // A rich text, HTML or other formatted file never equals its shown
    // text; only a plain text file's difference means something.
    let plain = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PLAIN_TEXT.contains(&e.to_ascii_lowercase().as_str()));
    plain.then_some(FileText::Differs)
}

const PLAIN_TEXT: &[&str] = &["txt", "text", "md", "markdown", "csv", "tsv", "log", "json", "yaml", "yml"];

// ---------------------------------------------------------------------------
// The watch

struct Watch {
    scope: Scope,
    before: Facts,
    before_complete: bool,
    /// Keep the target, the list and a menu's opener alive for the after-reads.
    _target: Option<RetainedElement>,
    _collection: Option<RetainedElement>,
    _opener: Option<RetainedElement>,
}

/// The facts as settling compares them: focus that went nowhere (a menu
/// open, the app activating) is a moment in the action, not its outcome,
/// so it neither counts as a change nor ends the wait.
fn settle_key(mut facts: Facts, before: &Facts) -> Facts {
    if facts.focus.is_none() {
        facts.focus = before.focus.clone();
    }
    facts
}

/// Start a watch for one native action, or `None` when the action has no
/// window to watch (desktop scope, scroll, an unknown window).
pub(crate) async fn begin(tool: &str, args: &Value) -> Option<Box<dyn OutcomeWatch>> {
    if tool == "scroll" || args.get("scope").and_then(Value::as_str) == Some("desktop") {
        return None;
    }
    let pid = i32::try_from(args.get("pid")?.as_i64()?).ok()?;
    let window_id = args
        .get("window_id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok());
    // The action's own element, resolved the way the action resolves it (in
    // this dispatch, so the session's snapshots are the ones consulted).
    let (target, window_id) = if args.get("element_token").is_some() || args.get("element_index").is_some() {
        let resolved = cua_driver_core::element_cache::current_runtime_cache::<CachedSnapshot>()
            .and_then(|cache| {
                cache
                    .resolve_element_args(
                        pid,
                        args.get("element_index").and_then(Value::as_u64).map(|i| i as usize),
                        args.get("element_token").and_then(Value::as_str),
                        args.get("snapshot_id").and_then(Value::as_str),
                        window_id.map(u64::from),
                        "outcome",
                    )
                    .ok()
            })
            .map(|resolved| resolved.into_parts(window_id.map(u64::from)));
        match resolved {
            // A token carries its window: watch that one, not the focused one.
            Some((_, token_window, element)) => (
                element,
                window_id.or_else(|| token_window.and_then(|id| u32::try_from(id).ok())),
            ),
            None => (None, window_id),
        }
    } else {
        (None, window_id)
    };
    let watch_menus = tool != "invoke_menu";
    let command = tool == "invoke_menu" || tool == "hotkey";
    tokio::task::spawn_blocking(move || {
        let window_id = window_id.or_else(|| unsafe { focused_window_id(pid) })?;
        let opener = target
            .as_ref()
            .and_then(|t| unsafe { menu_opener_of(pid, window_id, t.as_ptr() as AXUIElementRef) });
        let mut scope = Scope {
            pid,
            window_id,
            target: target.as_ref().map(RetainedElement::as_ptr),
            collection: None,
            opener: opener.as_ref().map(RetainedElement::as_ptr),
            menus: watch_menus,
            command,
        };
        let pass = unsafe { read_pass(&scope, false) };
        if pass.facts.window_present != Some(true) {
            return None;
        }
        if let Some(list) = &pass.holder {
            remember_collection(pid, window_id, list);
        }
        scope.collection = pass.holder.as_ref().map(RetainedElement::as_ptr);
        Some(Box::new(Watch {
            scope,
            before: pass.facts,
            before_complete: pass.complete,
            _target: target,
            _collection: pass.holder,
            _opener: opener,
        }) as Box<dyn OutcomeWatch>)
    })
    .await
    .ok()
    .flatten()
}

#[async_trait]
impl OutcomeWatch for Watch {
    async fn finish(self: Box<Self>) -> Option<String> {
        let watch = *self;
        if !watch.before_complete {
            // Nothing to compare with: say so rather than guess.
            return Some("unknown: the app did not answer every read before the action".into());
        }
        let started = Instant::now();
        // The last read that answered in full; reads cut by the budget (an
        // app busy opening a sheet) say nothing either way.
        let mut previous: Option<Facts> = None;
        let mut last_change = started;
        let settle = loop {
            tokio::time::sleep(POLL).await;
            let scope = watch.scope.clone();
            let (now, complete) = tokio::task::spawn_blocking(move || unsafe {
                let pass = read_pass(&scope, false);
                (pass.facts, pass.complete)
            })
            .await
            .ok()?;
            let elapsed = started.elapsed();
            if complete {
                let now = settle_key(now, &watch.before);
                if previous.as_ref() != Some(&now) {
                    last_change = Instant::now();
                }
                let changed = now != watch.before;
                previous = Some(now);
                if changed && last_change.elapsed() >= STABLE_FOR {
                    break Settle::Settled;
                }
                if !changed && !watch.scope.command && elapsed >= NO_CHANGE_WAIT {
                    break Settle::Unchanged;
                }
                if !changed && elapsed >= COMMAND_NO_CHANGE_WAIT {
                    break Settle::CommandUnchanged;
                }
            }
            if elapsed >= SETTLE_DEADLINE {
                break match &previous {
                    Some(facts) if *facts != watch.before => Settle::StillChanging,
                    _ if watch.scope.command => Settle::CommandUnchanged,
                    _ => Settle::Unchanged,
                };
            }
        };
        // Read once more, keeping what the disk notes need; when that read
        // is cut short, describe the last full one without disk notes.
        let scope = watch.scope.clone();
        let before = watch.before.clone();
        let complete_before = watch.before_complete;
        let line = tokio::task::spawn_blocking(move || unsafe {
            let mut pass = read_pass(&scope, true);
            for _ in 0..2 {
                if pass.complete {
                    break;
                }
                std::thread::sleep(POLL);
                pass = read_pass(&scope, true);
            }
            if let Some(list) = &pass.holder {
                remember_collection(scope.pid, scope.window_id, list);
            }
            let (after, disk, complete) = if pass.complete {
                let disk = disk_notes(&before, &pass);
                (settle_key(pass.facts, &before), disk, true)
            } else {
                match previous {
                    Some(facts) => (facts, DiskNotes::default(), true),
                    None => (pass.facts, DiskNotes::default(), false),
                }
            };
            // A change that went back by the last read is not settled.
            let settle = if settle == Settle::Settled && after == before { Settle::Reverted } else { settle };
            describe(&before, &after, &disk, settle, complete_before && complete)
        })
        .await
        .ok()?;
        drop(watch);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str], selected: &[&str]) -> Collection {
        Collection {
            role: "AXList".into(),
            label: "icon view".into(),
            items: Some(items.iter().map(|s| s.to_string()).collect()),
            count: Some(items.len()),
            selected: Some(selected.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn window(title: &str) -> Facts {
        Facts {
            window_present: Some(true),
            title: Some(title.into()),
            sheets: Some(vec![]),
            windows: Some(vec![]),
            ..Facts::default()
        }
    }

    const FILES: [&str; 5] = ["notes.txt", "photo.jpg", "receipt-feb.pdf", "receipt-jan.pdf", "receipt-mar.pdf"];

    fn button(label: &str) -> Element {
        Element { role: "AXButton".into(), label: label.into(), value: None, length: None }
    }

    /// G2: a pop-up press says its menu opened; the pick names the pop-up's
    /// new choice and the menu closing; a menu left open is still named.
    #[test]
    fn menus_opening_closing_and_the_openers_new_choice_are_named() {
        let mut before = window("CatalystProfile");
        before.menus = Some(vec![]);
        let mut after = before.clone();
        after.menus = Some(vec![27410]);
        assert_eq!(describe(&before, &after, &DiskNotes::default(), Settle::Settled, true), "a menu opened");

        let mut before = window("CatalystProfile");
        before.menus = Some(vec![27410]);
        before.opener = Some(button("Daily"));
        let mut after = before.clone();
        after.menus = Some(vec![]);
        after.opener = Some(button("Weekly"));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "AXButton \"Daily\" is now labelled \"Weekly\"; menu closed"
        );
        after.opener = Some(button("Daily"));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "AXButton \"Daily\" kept its label; whether the pick took is not readable; menu closed"
        );

        let mut before = window("note.txt");
        before.menus = Some(vec![9]);
        let after = before.clone();
        assert!(describe(&before, &after, &DiskNotes::default(), Settle::Unchanged, true)
            .ends_with("; still open: a menu"));
    }

    /// A command (invoke_menu, hotkey) with nothing seen is not called
    /// "nothing changed": Finder's Move to Trash once showed after 0.6 s.
    #[test]
    fn a_command_with_no_change_seen_is_not_settled() {
        let before = window("cleanup");
        let line = describe(&before, &before.clone(), &DiskNotes::default(), Settle::CommandUnchanged, true);
        assert!(line.starts_with("no change seen within 1.5 s"), "{line}");
        assert!(line.contains("not settled") && !line.contains("nothing it watches changed"), "{line}");
        let line = describe(&before, &before.clone(), &DiskNotes::default(), Settle::Unchanged, true);
        assert!(line.starts_with("nothing it watches changed within 0.6 s"), "{line}");
        // A change that settled, then read back as the start, claims no wait.
        let line = describe(&before, &before.clone(), &DiskNotes::default(), Settle::Reverted, true);
        assert!(line.starts_with("a change came and went") && line.contains("not settled"), "{line}");
    }

    #[test]
    fn a_selection_change_names_the_whole_selected_set() {
        let mut before = window("inbox");
        before.collection = Some(list(&FILES, &["receipt-feb.pdf"]));
        let mut after = before.clone();
        after.collection = Some(list(&FILES, &["receipt-feb.pdf", "receipt-jan.pdf"]));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "selected in icon view: receipt-feb.pdf, receipt-jan.pdf (2 of 5)"
        );
    }

    #[test]
    fn a_new_folder_with_selection_names_added_gone_and_the_rename_field() {
        let mut before = window("inbox");
        before.collection = Some(list(&FILES, &["receipt-feb.pdf", "receipt-jan.pdf", "receipt-mar.pdf"]));
        before.focus = Some(Element { role: "AXList".into(), label: "icon view".into(), value: None, length: None });
        let mut after = before.clone();
        after.collection = Some(list(&["notes.txt", "photo.jpg", "New Folder With Items"], &["New Folder With Items"]));
        after.focus = Some(Element {
            role: "AXTextField".into(),
            label: String::new(),
            value: Some("New Folder With Items".into()),
            length: None,
        });
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "icon view now: notes.txt, photo.jpg, New Folder With Items (3); added New Folder With Items; \
             gone from the list: receipt-feb.pdf, receipt-jan.pdf, receipt-mar.pdf; selected in icon view: \
             New Folder With Items (1 of 3); focus: AXTextField = \"New Folder With Items\""
        );
    }

    #[test]
    fn a_rename_commit_names_the_committed_name() {
        let mut before = window("inbox");
        before.collection = Some(list(&["notes.txt", "photo.jpg", "New Folder With Items"], &["New Folder With Items"]));
        before.focus = Some(Element { role: "AXTextField".into(), label: String::new(), value: Some("Receipts".into()), length: None });
        let mut after = before.clone();
        after.collection = Some(list(&["notes.txt", "photo.jpg", "Receipts"], &["Receipts"]));
        after.focus = Some(Element { role: "AXList".into(), label: "icon view".into(), value: None, length: None });
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "icon view now: notes.txt, photo.jpg, Receipts (3); added Receipts; gone from the list: \
             New Folder With Items; selected in icon view: Receipts (1 of 3); focus: AXList \"icon view\""
        );
    }

    #[test]
    fn a_drag_into_a_folder_says_the_items_left_the_list() {
        let mut before = window("inbox");
        before.collection = Some(list(&["Receipts", "notes.txt", "receipt-feb.pdf"], &["receipt-feb.pdf"]));
        let mut after = before.clone();
        after.collection = Some(list(&["Receipts", "notes.txt"], &[]));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "icon view now: Receipts, notes.txt (2); gone from the list: receipt-feb.pdf; nothing selected in icon view"
        );
    }

    #[test]
    fn opening_a_folder_lists_it_instead_of_a_diff() {
        let mut before = window("inbox");
        before.collection = Some(list(&["notes.txt", "Receipts"], &["Receipts"]));
        let mut after = window("Receipts");
        after.collection = Some(list(&["receipt-feb.pdf", "receipt-jan.pdf"], &[]));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "window title now \"Receipts\"; icon view shows: receipt-feb.pdf, receipt-jan.pdf (2)"
        );
    }

    #[test]
    fn a_save_names_the_file_and_whether_it_holds_the_windows_text() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut before = window("note.txt");
        before.document = Some("/Users/lume/lab/work/note.txt".into());
        before.file = Some(FileStamp { modified: t0, len: 6 });
        let mut after = before.clone();
        after.file = Some(FileStamp { modified: t0 + Duration::from_secs(5), len: 7 });
        let disk = DiskNotes { file_text: Some(FileText::Matches(7)), ..DiskNotes::default() };
        assert_eq!(
            describe(&before, &after, &disk, Settle::Settled, true),
            "file /Users/lume/lab/work/note.txt modified during the action; it holds the window's text (7 characters)"
        );
        let disk = DiskNotes { file_text: Some(FileText::Differs), ..DiskNotes::default() };
        assert!(describe(&before, &after, &disk, Settle::Settled, true).ends_with("its text differs from the window's"));
    }

    #[test]
    fn a_file_first_seen_after_the_action_is_not_called_written() {
        let mut before = window("note.txt");
        before.document = Some("/Users/lume/lab/work/note.txt".into());
        let mut after = before.clone();
        after.file = Some(FileStamp { modified: SystemTime::UNIX_EPOCH, len: 6 });
        let disk = DiskNotes { file_text: Some(FileText::Matches(6)), ..DiskNotes::default() };
        let line = describe(&before, &after, &disk, Settle::Settled, true);
        assert!(!line.contains("modified") && !line.contains("written"), "{line}");
    }

    #[test]
    fn a_save_as_names_the_sheet_the_new_document_and_the_new_file() {
        let mut before = window("note.txt");
        before.document = Some("/Users/lume/lab/work/note.txt".into());
        before.file = Some(FileStamp { modified: SystemTime::UNIX_EPOCH, len: 6 });
        before.sheets = Some(vec!["sheet: save".into()]);
        let mut after = window("groceries.txt");
        after.document = Some("/Users/lume/lab/work/groceries.txt".into());
        after.file = Some(FileStamp { modified: SystemTime::UNIX_EPOCH, len: 52 });
        let disk = DiskNotes { file_text: Some(FileText::Matches(52)), ..DiskNotes::default() };
        assert_eq!(
            describe(&before, &after, &disk, Settle::Settled, true),
            "window title now \"groceries.txt\"; document now /Users/lume/lab/work/groceries.txt (was \
             /Users/lume/lab/work/note.txt); its file: it holds the window's text (52 characters); \
             sheet closed: save"
        );
    }

    #[test]
    fn a_menu_that_opens_a_sheet_or_popover_says_so() {
        let before = window("note.txt");
        let mut after = before.clone();
        after.sheets = Some(vec!["sheet: save".into(), "popover".into()]);
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "sheet opened: save; popover opened"
        );
    }

    #[test]
    fn a_checkbox_click_names_its_new_value() {
        let mut before = window("General");
        let checkbox = |value: &str| Element {
            role: "AXCheckBox".into(),
            label: "Check spelling as you type".into(),
            value: Some(value.into()),
            length: None,
        };
        before.target = Some(checkbox("1"));
        before.focus = before.target.clone();
        let mut after = before.clone();
        after.target = Some(checkbox("0"));
        after.focus = after.target.clone();
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "AXCheckBox \"Check spelling as you type\" now 0 (off), was 1 (on)"
        );
    }

    #[test]
    fn a_menu_that_opens_another_window_names_it_with_its_id() {
        let before = window("note.txt");
        let mut after = before.clone();
        after.windows = Some(vec![OtherWindow { id: 21016, title: "General".into() }]);
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "window opened: \"General\" (window_id 21016)"
        );
        // Another window's new title alone is not a change.
        let mut retitled = after.clone();
        retitled.windows = Some(vec![OtherWindow { id: 21016, title: "Open and Save".into() }]);
        assert_eq!(after, retitled);
    }

    #[test]
    fn a_relabelled_target_is_still_the_target() {
        let mut before = window("note.txt");
        let triangle = |label: &str, value: &str| Element {
            role: "AXDisclosureTriangle".into(),
            label: label.into(),
            value: Some(value.into()),
            length: None,
        };
        before.target = Some(triangle("show more options", "0"));
        let mut after = before.clone();
        after.target = Some(triangle("show less options", "1"));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "AXDisclosureTriangle \"show less options\" now 1, was 0"
        );
    }

    #[test]
    fn a_popover_left_open_is_named_on_every_line() {
        let mut before = window("note copy");
        before.sheets = Some(vec!["popover".into()]);
        before.document = Some("/Users/lume/lab/work/note copy.txt".into());
        let mut after = before.clone();
        after.title = Some("groceries".into());
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "window title now \"groceries\"; still open: popover"
        );
        let line = describe(&before, &before, &DiskNotes::default(), Settle::Unchanged, true);
        assert!(line.starts_with("nothing it watches changed") && line.ends_with("; still open: popover"), "{line}");
    }

    #[test]
    fn a_set_value_echoes_the_text() {
        let mut before = window("note.txt");
        let area = |value: &str| Element {
            role: "AXTextArea".into(),
            label: String::new(),
            value: Some(value.into()),
            length: None,
        };
        before.target = Some(area("draft\n"));
        let mut after = before.clone();
        after.target = Some(area("shopping list\n- milk\n"));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "AXTextArea now \"shopping list\\n- milk\\n\", was \"draft\\n\""
        );
    }

    #[test]
    fn a_long_document_is_compared_by_length() {
        let mut before = window("book.txt");
        let area = |length: usize| Element {
            role: "AXTextArea".into(),
            label: String::new(),
            value: None,
            length: Some(length),
        };
        before.focus = Some(area(25_000));
        let mut after = before.clone();
        after.focus = Some(area(25_007));
        assert_eq!(
            describe(&before, &after, &DiskNotes::default(), Settle::Settled, true),
            "AXTextArea now 25007 characters, was 25000"
        );
    }

    #[test]
    fn nothing_changed_is_said_only_when_every_read_answered() {
        let mut before = window("note.txt");
        before.document = Some("/Users/x/Documents/a.txt".into());
        let after = before.clone();
        let line = describe(&before, &after, &DiskNotes::default(), Settle::Unchanged, true);
        assert!(line.starts_with("nothing it watches changed within 0.6 s"), "{line}");
        assert!(line.ends_with("the document's file was not checked (protected folder or unreadable)"), "{line}");
        let line = describe(&before, &after, &DiskNotes::default(), Settle::Unchanged, false);
        assert!(line.starts_with("no change seen within 0.6 s, but the app did not answer every read"), "{line}");
    }

    #[test]
    fn a_closed_window_is_only_reported_when_the_app_answered() {
        let before = window("note.txt");
        let closed = Facts { window_present: Some(false), ..Facts::default() };
        assert_eq!(describe(&before, &closed, &DiskNotes::default(), Settle::Settled, true), "this window closed");
        let unknown = Facts::default();
        let line = describe(&before, &unknown, &DiskNotes::default(), Settle::Unchanged, false);
        assert!(!line.contains("closed"), "{line}");
    }

    #[test]
    fn a_change_still_in_progress_says_so() {
        let mut before = window("inbox");
        before.collection = Some(list(&["a"], &[]));
        let mut after = before.clone();
        after.collection = Some(list(&["a", "b"], &[]));
        assert!(describe(&before, &after, &DiskNotes::default(), Settle::StillChanging, true)
            .ends_with("; still changing after 2 s"));
    }

    #[test]
    fn unknown_lists_never_produce_added_or_gone() {
        let mut before = window("inbox");
        before.collection = Some(Collection { items: None, ..list(&[], &[]) });
        let mut after = before.clone();
        after.collection = Some(list(&["a"], &[]));
        let line = describe(&before, &after, &DiskNotes::default(), Settle::Settled, true);
        assert!(!line.contains("added") && !line.contains("gone"), "{line}");
    }

    #[test]
    fn long_lists_and_values_are_shortened() {
        let many: Vec<String> = (0..20).map(|i| format!("f{i}")).collect();
        assert!(names(&many).ends_with("f11, and 8 more"));
        let long = "x".repeat(200);
        assert_eq!(shown_value(&long), format!("200 characters ending \"…{}\"", "x".repeat(40)));
    }

    #[test]
    fn disk_reads_stay_out_of_protected_folders() {
        let home = Path::new("/Users/lume");
        assert_eq!(
            readable_path("file:///Users/lume/lab/work/note%20a.txt", home),
            Some(PathBuf::from("/Users/lume/lab/work/note a.txt"))
        );
        assert_eq!(
            readable_path("file:///Users/lume/lab/work/inbox/Receipts/", home),
            Some(PathBuf::from("/Users/lume/lab/work/inbox/Receipts"))
        );
        for url in [
            "file:///Users/lume/Documents/a.txt",
            "file:///Users/lume/Desktop/a.txt",
            "file:///Users/lume/Downloads/a",
            "file:///Users/lume/Library/Mobile%20Documents/a",
            "file:///Users/lume/.Trash/a",
            "file:///Users/lume/lab/../Documents/a.txt",
            "file:///Volumes/USB/a.txt",
            "file:///Users/other/lab/a.txt",
            "file:///Users/lume",
            "relative/path",
        ] {
            assert_eq!(readable_path(url, home), None, "{url}");
        }
        assert!(readable_path("file:///private/tmp/x.txt", home).is_some());
    }
}
