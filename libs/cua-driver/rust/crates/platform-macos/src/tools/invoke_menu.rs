use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_contract::InvokeMenuInput;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    time::Duration,
};

use crate::ax::bindings::{
    ax_get_window_id, copy_action_names, copy_ax_windows, copy_bool_attr, copy_children,
    copy_element_attr, copy_string_attr, kAXErrorSuccess, perform_action, set_bool_attr_true,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};

pub struct InvokeMenuTool;

/// Where the front app ended up after a menu command run from behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrontAfter {
    Restored,
    NotConfirmed,
    /// The command opened an inline editor; handing the front back would
    /// end it.
    KeptForInlineEdit,
}

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;
const MAIN_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a menu command gets to open an inline editor (File > Rename)
/// before the previous front app is brought back.
const INLINE_EDIT_WAIT: Duration = Duration::from_millis(1200);
/// The same for a command known to open one (File > Rename, New Folder).
/// The wait ends when the editor shows (0.4 to 1.2 s after the menu in VM B);
/// the rest is spent only when none comes. Under load Finder showed the
/// new folder's name editor later than 1.2 s after its menu (item 26), and
/// handing the front back first ended the edit.
const EXPECTED_INLINE_EDIT_WAIT: Duration = Duration::from_millis(4000);
/// Whether `path` is a File menu item Finder answers with an inline name
/// editor (File > Rename, New Folder, New Folder with Selection). Matched by
/// its English titles, trailing ellipsis ignored.
// ponytail: English titles only; another language falls back to the generic
// 150 ms settle (the pre-item-26 behaviour), add localized titles if needed.
fn opens_inline_editor(path: &[String]) -> bool {
    let [menu, item] = path else {
        return false;
    };
    menu == "File" && {
        let title = item.trim().trim_end_matches(['…', '.']).trim_end();
        title == "Rename" || title == "New Folder" || title.starts_with("New Folder with Selection")
    }
}

/// How long a press made from behind gets to show its effect. A press that
/// shows none is not made again: the command may finish later (Slack's
/// Paste, TextEdit's New), and pressing it in front as well ran it twice.
const BACKGROUND_EFFECT_WAIT: Duration = Duration::from_millis(600);
const BACKGROUND_POLL: Duration = Duration::from_millis(50);

unsafe fn set_messaging_timeout(element: AXUIElementRef) {
    let _ = AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS);
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| {
        let contract =
            cua_driver_contract::tool_contract("invoke_menu").expect("invoke_menu contract");
        ToolDef {
            name: contract.name,
            description: contract.description,
            input_schema: contract.input_schema,
            read_only: contract.annotations.read_only,
            destructive: contract.annotations.destructive,
            idempotent: contract.annotations.idempotent,
            open_world: contract.annotations.open_world,
        }
    })
}

fn normalized_path(path: Vec<String>) -> Result<Vec<String>, String> {
    if path.is_empty() || path.len() > 16 {
        return Err("invoke_menu: path must contain between 1 and 16 segments".into());
    }
    path.into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let segment = segment.trim();
            if segment.is_empty() {
                Err(format!("invoke_menu: path segment {index} is empty"))
            } else {
                Ok(segment.to_owned())
            }
        })
        .collect()
}

/// Return the semantic menu children of an AX node. AppKit inserts an
/// untitled `AXMenu` container between a menu-bar/submenu item and its items;
/// callers express paths in visible labels, so that container is transparent.
unsafe fn semantic_children(parent: AXUIElementRef) -> Vec<AXUIElementRef> {
    let mut out = Vec::new();
    for child in copy_children(parent) {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            out.extend(copy_children(child));
            CFRelease(child as CFTypeRef);
        } else {
            out.push(child);
        }
    }
    out
}

/// A menu title as paths compare it: trimmed, with three periods read as
/// the ellipsis character macOS menu titles use ("Save As..." finds
/// "Save As…").
fn menu_title_key(title: &str) -> String {
    title.trim().replace("...", "\u{2026}")
}

/// What a menu holds, for a refusal that names a missing item: its titled
/// items in order, at most 30.
fn listing(titles: &[String]) -> String {
    let shown: Vec<&str> = titles
        .iter()
        .map(|title| title.trim())
        .filter(|title| !title.is_empty())
        .take(30)
        .collect();
    if shown.is_empty() {
        "no titled items".into()
    } else {
        shown.join(", ")
    }
}

unsafe fn resolve_exact_prefix(
    menu_bar: AXUIElementRef,
    prefix: &[String],
) -> Result<AXUIElementRef, String> {
    let mut current = menu_bar;
    let mut owns_current = false;

    for (depth, segment) in prefix.iter().enumerate() {
        let children = semantic_children(current);
        if owns_current {
            CFRelease(current as CFTypeRef);
        }

        let wanted = menu_title_key(segment);
        let mut matches = Vec::new();
        let mut titles = Vec::new();
        for child in children {
            let title = copy_string_attr(child, "AXTitle").unwrap_or_default();
            if menu_title_key(&title) == wanted {
                matches.push(child);
            } else {
                CFRelease(child as CFTypeRef);
            }
            titles.push(title);
        }

        if matches.len() != 1 {
            let match_count = matches.len();
            for candidate in matches {
                CFRelease(candidate as CFTypeRef);
            }
            let parent = if depth == 0 {
                "the menu bar".to_owned()
            } else {
                prefix[depth - 1].clone()
            };
            return Err(if match_count == 0 {
                format!(
                    "invoke_menu: path segment {depth} was not found: {segment:?} is not in {parent}; it has: {}",
                    listing(&titles)
                )
            } else {
                format!("invoke_menu: path segment {depth} is ambiguous")
            });
        }
        current = matches.pop().expect("one exact match");
        owns_current = true;
    }

    if owns_current {
        Ok(current)
    } else {
        Err("invoke_menu: path is empty".into())
    }
}

/// Close the menu a failed path opened from the menu bar item `top` with
/// AXCancel on that item's menu (which also ends its submenus), and read the
/// result back from WindowServer's window list. Menu windows in
/// `menus_before` were on screen before this call and do not count.
unsafe fn close_opened_menu(
    app: AXUIElementRef,
    pid: i32,
    top: &str,
    menus_before: &[u32],
) -> Option<bool> {
    // An action that reported an error can still open its menu a moment
    // later; give it the same settle time as a hop before looking.
    std::thread::sleep(Duration::from_millis(80));
    if crate::windows::new_menu_windows(pid, menus_before)? == 0 {
        return Some(true);
    }
    let item = copy_element_attr(app, "AXMenuBar").and_then(|bar| {
        set_messaging_timeout(bar);
        let item = resolve_exact_prefix(bar, std::slice::from_ref(&top.to_owned())).ok();
        CFRelease(bar as CFTypeRef);
        item
    });
    let Some(item) = item else {
        return Some(false);
    };
    set_messaging_timeout(item);
    for child in copy_children(item) {
        set_messaging_timeout(child);
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            let _ = perform_action(child, "AXCancel");
        }
        CFRelease(child as CFTypeRef);
    }
    CFRelease(item as CFTypeRef);
    crate::windows::wait_for_new_menus_closed(pid, menus_before)
}

/// The refusal for a path that failed after pressing a menu item, saying
/// whether the menu it opened was seen to close.
fn failure_after_press(error: String, top: &str, closed: Option<bool>) -> String {
    match closed {
        Some(true) => format!("{error}. No menu window this call opened is still on screen."),
        Some(false) | None => format!(
            "{error}. The {top} menu this call opened may still be open: press escape on the window before other input."
        ),
    }
}

fn choose_action(actions: &[String], final_segment: bool) -> Option<&'static str> {
    let supports = |name: &str| actions.iter().any(|action| action == name);
    let order: &[&str] = if final_segment {
        &["AXPress", "AXPick", "AXConfirm"]
    } else {
        &["AXPress", "AXPick", "AXShowMenu", "AXOpen"]
    };
    order.iter().copied().find(|action| supports(action))
}

unsafe fn invoke_path(pid: i32, path: &[String]) -> Result<(), String> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err("invoke_menu: target application is unavailable".into());
    }
    set_messaging_timeout(app);

    // Whether this call pressed a menu item (a press that reports an error
    // can still open its menu), so a failure must close what it opened.
    let mut pressed = false;
    // Unreadable now counts every menu window later as this call's, which can
    // only make the closed claim more cautious.
    let menus_before = crate::windows::menu_window_ids(pid).unwrap_or_default();
    let result = (|| {
        for depth in 0..path.len() {
            // Resolve from the live app root for every hop. Opening a menu can
            // replace its AX objects and reorder unrelated snapshot indices.
            let menu_bar = copy_element_attr(app, "AXMenuBar")
                .ok_or_else(|| "invoke_menu: target exposes no AXMenuBar".to_owned())?;
            set_messaging_timeout(menu_bar);
            let target = resolve_exact_prefix(menu_bar, &path[..=depth]);
            CFRelease(menu_bar as CFTypeRef);
            let target = target?;
            set_messaging_timeout(target);

            if copy_bool_attr(target, "AXEnabled") == Some(false) {
                CFRelease(target as CFTypeRef);
                return Err(format!("invoke_menu: path segment {depth} is disabled"));
            }
            let actions = copy_action_names(target);
            let action = choose_action(&actions, depth + 1 == path.len()).ok_or_else(|| {
                format!("invoke_menu: path segment {depth} has no usable native menu action")
            });
            let action = match action {
                Ok(action) => action,
                Err(error) => {
                    CFRelease(target as CFTypeRef);
                    return Err(error);
                }
            };
            pressed = true;
            let error = perform_action(target, action);
            CFRelease(target as CFTypeRef);
            if error != kAXErrorSuccess {
                return Err(format!(
                    "invoke_menu: native action for path segment {depth} failed with AX error {error}"
                ));
            }
            if depth + 1 != path.len() {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
        Ok(())
    })();

    // A failure after a hop opened a menu: close it, so the app is not left
    // tracking a menu (where the next menu command reports success and does
    // nothing).
    let result = result.map_err(|error| {
        if !pressed {
            return error;
        }
        let closed = close_opened_menu(app, pid, &path[0], &menus_before);
        failure_after_press(error, &path[0], closed)
    });

    CFRelease(app as CFTypeRef);
    result
}

/// What a menu command can change that a read sees: whether the app still
/// runs (Quit), the clipboard, the
/// app's windows (opened, closed, moved, resized, shown or hidden), the
/// target window's facts as an outcome watch reads them, what the window
/// shows (every element's role and label), and the item's own title and
/// check mark. `None` is unknown: unread, or not steady before the press.
#[derive(Clone, Debug, Default, PartialEq)]
struct Observed {
    alive: Option<bool>,
    clipboard: Option<isize>,
    windows: Option<Vec<(u32, [i64; 4], bool)>>,
    facts: Option<crate::outcome::Facts>,
    ui: Option<u64>,
    item: Option<(Option<String>, Option<String>)>,
}

impl Observed {
    /// The fields two reads agree on; a field that changed with nothing
    /// pressed (a clock, a spinner) says nothing about the press.
    fn steady(self, again: &Observed) -> Observed {
        fn keep<T: PartialEq>(a: Option<T>, b: &Option<T>) -> Option<T> {
            a.filter(|a| b.as_ref() == Some(a))
        }
        Observed {
            alive: keep(self.alive, &again.alive),
            clipboard: keep(self.clipboard, &again.clipboard),
            windows: keep(self.windows, &again.windows),
            facts: keep(self.facts, &again.facts),
            ui: keep(self.ui, &again.ui),
            item: keep(self.item, &again.item),
        }
    }
}

unsafe fn observe(
    pid: i32,
    window_id: u32,
    window: Option<AXUIElementRef>,
    item: AXUIElementRef,
) -> Observed {
    // Menus (layer 101 and up) and helper windows under 100 pt come and go
    // on their own (a Catalyst app's menu windows moved after an AX read of
    // its menus, with nothing pressed).
    let mut windows: Vec<_> = crate::windows::all_windows_any_layer()
        .into_iter()
        .filter(|w| {
            w.pid == pid && w.layer < 100 && w.bounds.width >= 100.0 && w.bounds.height >= 100.0
        })
        .map(|w| {
            let b = w.bounds;
            (
                w.window_id,
                [b.x, b.y, b.width, b.height].map(|v| v.round() as i64),
                w.is_on_screen,
            )
        })
        .collect();
    windows.sort_unstable();
    // The target window exists, so an empty list is a failed enumeration.
    let windows = (!windows.is_empty()).then_some(windows);
    let facts = crate::outcome::command_facts(pid, window_id);
    Observed {
        // A Quit leaves every other read unknown; the process says it.
        alive: Some(libc::kill(pid, 0) == 0),
        clipboard: crate::outcome::pasteboard_change_count(),
        windows,
        facts,
        ui: window.and_then(|window| crate::outcome::shown_signature(window)),
        item: item_state(item),
    }
}

/// The target window's AX element (retained), when the app lists it.
unsafe fn ax_window(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    set_messaging_timeout(app);
    let mut found = None;
    for window in copy_ax_windows(app) {
        if found.is_none() && ax_get_window_id(window) == Some(window_id) {
            found = Some(window);
        } else {
            CFRelease(window as CFTypeRef);
        }
    }
    CFRelease(app as CFTypeRef);
    found
}

/// What differs between two reads, in words for the result. A field either
/// read does not know counts as unchanged.
fn changes(before: &Observed, after: &Observed) -> Vec<&'static str> {
    fn differs<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
        matches!((a, b), (Some(a), Some(b)) if a != b)
    }
    let mut out = Vec::new();
    if differs(&before.alive, &after.alive) {
        out.push("whether the app is running (it quit)");
    }
    if differs(&before.clipboard, &after.clipboard) {
        out.push("the clipboard");
    }
    if differs(&before.windows, &after.windows) {
        out.push("the app's windows");
    }
    let facts_differ = matches!(
        (&before.facts, &after.facts),
        (Some(a), Some(b)) if crate::outcome::known_facts_differ(a, b)
    );
    if facts_differ || differs(&before.ui, &after.ui) {
        out.push("what the window shows");
    }
    if differs(&before.item, &after.item) {
        out.push("the menu item's state");
    }
    out
}

/// How the item is pressed from behind.
#[derive(Clone, Debug, PartialEq)]
enum Press {
    /// Through accessibility, the closed menu's item pressed directly.
    Ax(&'static str),
    /// The item's key equivalent, sent to the app's process.
    Keys {
        key: String,
        modifiers: Vec<&'static str>,
    },
}

impl Press {
    fn words(&self) -> String {
        match self {
            Press::Ax(_) => "pressed it through accessibility".into(),
            Press::Keys { key, modifiers } => {
                format!(
                    "sent its key equivalent {}+{key} to the app",
                    modifiers.join("+")
                )
            }
        }
    }
}

/// An item's key equivalent as the hotkey tool names keys: AX gives the
/// character (upper case for letters) and modifier bits (1 shift, 2 option,
/// 4 control, 8 no command; `None` when they could not be read). A special
/// key (Delete, Return, an arrow) has a control character or none, and AX
/// names it by its virtual key: Finder's Move to Trash is U+0008 with
/// virtual key 51, Cmd+Delete. Never a bare key: at least one modifier.
fn key_equivalent(
    character: Option<&str>,
    virtual_key: Option<i64>,
    flags: Option<f64>,
) -> Option<(String, Vec<&'static str>)> {
    let mut chars = character.unwrap_or_default().chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_whitespace() && !c.is_control() => c.to_lowercase().to_string(),
        (Some(c), None) if c.is_control() => special_key_name(virtual_key?)?.to_owned(),
        (None, _) => special_key_name(virtual_key?)?.to_owned(),
        _ => return None,
    };
    let flags = flags?;
    if !(0.0..16.0).contains(&flags) || flags.fract() != 0.0 {
        return None;
    }
    let flags = flags as i64;
    let mut modifiers = Vec::new();
    if flags & 8 == 0 {
        modifiers.push("cmd");
    }
    if flags & 1 != 0 {
        modifiers.push("shift");
    }
    if flags & 2 != 0 {
        modifiers.push("option");
    }
    if flags & 4 != 0 {
        modifiers.push("ctrl");
    }
    (!modifiers.is_empty()).then_some((key, modifiers))
}

/// The key names (as press_key reads them) of the special keys a menu item's
/// virtual key can name. Their positions do not move with the layout.
const SPECIAL_KEYS: &[(i64, &str)] = &[
    (36, "return"),
    (48, "tab"),
    (51, "delete"),
    (53, "escape"),
    (115, "home"),
    (116, "pageup"),
    (117, "forward_delete"),
    (119, "end"),
    (121, "pagedown"),
    (123, "left"),
    (124, "right"),
    (125, "down"),
    (126, "up"),
];

fn special_key_name(virtual_key: i64) -> Option<&'static str> {
    SPECIAL_KEYS.iter().find(|(code, _)| *code == virtual_key).map(|(_, name)| *name)
}

/// Whether a menu shortcut is Finder's New Folder (Cmd+Shift+N) or New
/// Folder with Selection (Ctrl+Cmd+N), whatever the menu language.
fn opens_editor_by_shortcut(key: &str, modifiers: &[&'static str]) -> bool {
    key == "n"
        && modifiers.contains(&"cmd")
        && (modifiers.contains(&"shift") || modifiers.contains(&"ctrl"))
}

/// Whether `path` is one of Finder's commands that open an inline editor:
/// by its English title, or by its shortcut (New Folder in any language).
fn item_opens_editor(pid: i32, path: &[String]) -> bool {
    opens_inline_editor(path)
        || unsafe {
            let Ok(item) = closed_menu_item(pid, path) else {
                return false;
            };
            let keys = key_equivalent(
                crate::ax::bindings::copy_string_attr_checked(item, "AXMenuItemCmdChar")
                    .ok()
                    .as_deref(),
                shortcut_virtual_key(item),
                shortcut_modifiers(item),
            );
            CFRelease(item as CFTypeRef);
            keys.is_some_and(|(key, modifiers)| opens_editor_by_shortcut(&key, &modifiers))
        }
}

/// Whether a special key sent from behind reaches the menu rather than the
/// focused control: not when the focus is a text control, or unreadable.
fn special_key_reaches_menu(focused_role: Option<&str>) -> bool {
    focused_role.is_some_and(|role| {
        !matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
    })
}

/// The role of the focused element of `pid`'s window `window_id` (its
/// subrole when that is AXSearchField), `None` when it could not be read or
/// the focus is in another window.
fn focused_role(pid: i32, window_id: u32) -> Option<String> {
    unsafe {
        let element = crate::ax::exact_target::focused_element_in_window(pid, window_id)?;
        let role = copy_string_attr(element, "AXRole");
        let search = copy_string_attr(element, "AXSubrole").as_deref() == Some("AXSearchField");
        CFRelease(element as CFTypeRef);
        if search { Some("AXSearchField".into()) } else { role }
    }
}

/// Whether the key sent for a shortcut must first be proven to type its
/// character in the current layout: only character keys; a special key is
/// the same key in every layout.
fn layout_check_applies(key: &str) -> bool {
    !SPECIAL_KEYS.iter().any(|(_, name)| *name == key)
}

/// Whether the key sent for a shortcut character types that character in
/// the current layout (`typed`, `None` when unknown, which is not proof).
fn same_key_in_layout(key: &str, typed: Option<&str>) -> bool {
    typed.is_some_and(|typed| typed.to_lowercase() == key.to_lowercase())
}

/// The item's `AXMenuItemCmdModifiers`: `Some(0.0)` (Command alone) when
/// the item names none, `None` when the read failed, so a shortcut is never
/// sent with modifiers guessed (Redo's Shift dropped would send Undo).
unsafe fn shortcut_modifiers(item: AXUIElementRef) -> Option<f64> {
    use core_foundation::number::CFNumber;
    match crate::ax::bindings::copy_attribute_checked(item, "AXMenuItemCmdModifiers") {
        Ok(value) => value
            .downcast::<CFNumber>()
            .and_then(|number| number.to_f64()),
        Err(
            crate::ax::bindings::kAXErrorNoValue
            | crate::ax::bindings::kAXErrorAttributeUnsupported,
        ) => Some(0.0),
        Err(_) => None,
    }
}

/// The item's `AXMenuItemCmdVirtualKey`, `None` when it has none or the read
/// failed.
unsafe fn shortcut_virtual_key(item: AXUIElementRef) -> Option<i64> {
    use core_foundation::number::CFNumber;
    crate::ax::bindings::copy_attribute_checked(item, "AXMenuItemCmdVirtualKey")
        .ok()?
        .downcast::<CFNumber>()
        .and_then(|number| number.to_i64())
}

/// Which press to make from behind. A closed menu's items were last checked
/// against the app's key window when the app updated them; from behind that
/// is no window, so an item that acts on the window's content (Copy, Paste,
/// Save) reads disabled, and an AX press of it does nothing. Its key
/// equivalent is checked again when it arrives, against the window made key
/// from behind, and runs only if the item is enabled there. An item that
/// reads enabled (Calculator's View > Scientific) is pressed through AX.
fn choose_press(
    enabled: Option<bool>,
    ax_action: Option<&'static str>,
    keys: Option<(String, Vec<&'static str>)>,
) -> Result<Press, &'static str> {
    match (enabled, ax_action, keys) {
        (Some(false), _, Some((key, modifiers))) | (_, None, Some((key, modifiers))) => {
            Ok(Press::Keys { key, modifiers })
        }
        (Some(false), _, None) => Err(
            "the item reads as disabled while the app is behind (menus check items that act on a window's content against the active window) and has no key equivalent to try from behind",
        ),
        (_, Some(action), _) => Ok(Press::Ax(action)),
        (_, None, None) => Err("the item has no press action while its menu is closed"),
    }
}

/// The press for a caller that needs the item's key equivalent, whatever
/// the item's enabled state reads (an AX press of an item that is disabled
/// from behind does nothing and still reads as sent): its shortcut, or none.
fn keys_press(keys: Option<(String, Vec<&'static str>)>) -> Result<Press, &'static str> {
    keys.map(|(key, modifiers)| Press::Keys { key, modifiers })
        .ok_or("the item has no key equivalent to send from behind")
}

/// How a press made from behind ended.
#[derive(Debug, PartialEq)]
enum Background {
    /// The item was pressed from behind: how, what changed during the wait
    /// (empty when nothing showed), whether the read at the end still
    /// showed it, whether the app came to the front anyway (it activated
    /// itself), and the error the press returned, if any (a menu item's
    /// AXPress can report one and still run the command).
    Pressed {
        press: Press,
        changed: Vec<&'static str>,
        lasted: bool,
        activated: bool,
        error: Option<String>,
    },
    /// Not pressed: why the foreground route runs.
    Skipped(String),
}

impl Background {
    fn skipped(why: impl Into<String>) -> Self {
        Background::Skipped(why.into())
    }
}

/// What the key-window run of a press from behind returned: the window
/// could not be made key, a check before the press refused it, or the press
/// was tried (its result, and what changed during the wait).
type BackgroundRun =
    anyhow::Result<Result<(Result<(), (String, bool)>, Vec<&'static str>), String>>;

/// How a press from behind ended. It counts as pressed once it was sent,
/// also when it returned an error or nothing changed; only a shortcut whose
/// key-down was never posted counts as not pressed.
fn verdict(ran: BackgroundRun, press: Press, lasted: bool, activated: bool) -> Background {
    match ran {
        Err(error) => Background::skipped(format!(
            "the window could not be made key from behind ({error})"
        )),
        Ok(Err(why)) => Background::skipped(why),
        Ok(Ok((Err((error, false)), _))) => Background::skipped(format!(
            "the item's shortcut could not be sent from behind ({error})"
        )),
        Ok(Ok((sent, changed))) => Background::Pressed {
            press,
            changed,
            lasted,
            activated,
            error: sent.err().map(|(error, _)| error),
        },
    }
}

/// Resolve `path` through the closed menus, no menu opened. `Err` names why
/// the press from behind is not made: the item is missing (some apps fill a
/// menu only when it opens), a menu above it reads disabled, or the item
/// opens a submenu.
unsafe fn closed_menu_item(pid: i32, path: &[String]) -> Result<AXUIElementRef, &'static str> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err("the target application is unavailable");
    }
    set_messaging_timeout(app);
    let bar = copy_element_attr(app, "AXMenuBar");
    CFRelease(app as CFTypeRef);
    let bar = bar.ok_or("the target exposes no AXMenuBar")?;
    set_messaging_timeout(bar);
    let mut found =
        Err("the item was not found in its closed menu (some apps fill a menu only when it opens)");
    for depth in 0..path.len() {
        let Ok(element) = resolve_exact_prefix(bar, &path[..=depth]) else {
            break;
        };
        set_messaging_timeout(element);
        if depth + 1 < path.len() {
            let disabled = copy_bool_attr(element, "AXEnabled") == Some(false);
            CFRelease(element as CFTypeRef);
            if disabled {
                found = Err("a menu above the item reads as disabled while the app is behind");
                break;
            }
            continue;
        }
        if !semantic_children(element)
            .into_iter()
            .map(|child| CFRelease(child as CFTypeRef))
            .collect::<Vec<_>>()
            .is_empty()
        {
            CFRelease(element as CFTypeRef);
            found = Err("the item opens a submenu");
            break;
        }
        found = Ok(element);
    }
    CFRelease(bar as CFTypeRef);
    found
}

/// The item's title and check mark, or `None` when either read failed (a
/// missing mark is an unchecked item, not a failure).
unsafe fn item_state(item: AXUIElementRef) -> Option<(Option<String>, Option<String>)> {
    let read = |attribute| match crate::ax::bindings::copy_string_attr_checked(item, attribute) {
        Ok(text) => Some(Some(text)),
        Err(
            crate::ax::bindings::kAXErrorNoValue
            | crate::ax::bindings::kAXErrorAttributeUnsupported,
        ) => Some(None),
        Err(_) => None,
    };
    Some((read("AXTitle")?, read("AXMenuItemMarkChar")?))
}

/// Why a shortcut cannot be sent from behind to this window, if it cannot:
/// the exact-target gate the hotkey tool uses (the window is the app's,
/// listed by accessibility, not minimized, the app not hidden). Other
/// windows of the app are allowed: the key window is read back before
/// anything is sent, and so is a focus outside web content.
fn shortcut_refused(pid: i32, window_id: u32) -> Option<String> {
    use cua_driver_core::background_input::{
        decide_background_input, refusal_codes, BackgroundAction, BackgroundInputDecision,
        ExactWindowTarget,
    };
    let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, None);
    match decide_background_input(
        ExactWindowTarget { pid, window_id },
        &facts,
        BackgroundAction::GenericKey,
    ) {
        BackgroundInputDecision::Refuse(refusal) => match refusal.code {
            refusal_codes::SAME_PID_KEYBOARD_AMBIGUITY => {}
            refusal_codes::MINIMIZED_OR_HIDDEN => {
                return Some(
                    "the app is hidden or the window is minimized (or that could not be read), so the item's shortcut cannot reach the window from behind".into(),
                );
            }
            code => {
                return Some(format!(
                    "the window could not be proven to take the item's shortcut from behind ({code})"
                ));
            }
        },
        BackgroundInputDecision::Execute { .. } => {}
    }
    None
}

/// Why a menu command of an app that ends an inline edit when it loses the
/// front (Finder) runs in front without a try from behind, if it does: the
/// command opens such an editor (Rename, New Folder), which needs the app in
/// front, or one is open now (or that could not be read), which a press from
/// behind could end. Every other command is tried from behind (item 41:
/// Move to Trash ran in front, over the user's app, and from there failed).
fn front_route_for_inline_edit(
    saves_on_end_editing: bool,
    opens_editor: impl FnOnce() -> bool,
    edit_open: impl FnOnce() -> Option<bool>,
) -> Option<&'static str> {
    if !saves_on_end_editing {
        return None;
    }
    if opens_editor() {
        return Some(
            "the command opens an inline editor (Rename, New Folder), and this app ends an inline edit when it loses the front, so it runs in front",
        );
    }
    (edit_open() != Some(false)).then_some(
        "this app has an inline edit open (or its focus could not be read), and it ends an inline edit when it loses the front, so its menu commands run in front until the edit ends",
    )
}

/// The earliest a press from behind is judged: an app that activates itself
/// for a command does it within this, and the activation log needs the
/// notification.
const BACKGROUND_VERDICT_AFTER: Duration = Duration::from_millis(400);

/// Run the final item of `path` without opening its menus or activating the
/// app: the item is resolved through the closed menus, `window_id` is made
/// the app's key window from behind (the selection background keys use) and
/// read back as the app's focused window, and the result names what a read
/// within [`BACKGROUND_EFFECT_WAIT`] showed changed against two steady reads
/// before it (nothing, when the command shows its effect later or not at
/// all: it is still pressed, and is not pressed again).
fn press_in_background(
    pid: i32,
    window_id: u32,
    path: &[String],
    prior: i32,
    keys_only: bool,
) -> Background {
    if let Some(why) = front_route_for_inline_edit(
        crate::tools::edit_commit::app_saves_on_end_editing(pid),
        || item_opens_editor(pid, path),
        || crate::ax::bindings::inline_edit_state(pid),
    ) {
        return Background::skipped(why);
    }
    unsafe {
        let item = match closed_menu_item(pid, path) {
            Ok(item) => item,
            Err(why) => return Background::skipped(why),
        };
        let outcome = (|| {
            let keys = key_equivalent(
                crate::ax::bindings::copy_string_attr_checked(item, "AXMenuItemCmdChar")
                    .ok()
                    .as_deref(),
                shortcut_virtual_key(item),
                shortcut_modifiers(item),
            );
            let press = if keys_only {
                keys_press(keys)
            } else {
                choose_press(
                    copy_bool_attr(item, "AXEnabled"),
                    choose_action(&copy_action_names(item), true),
                    keys,
                )
            };
            let press = match press {
                Ok(press) => press,
                Err(why) => return Background::skipped(why),
            };
            if let Press::Keys { key, .. } = &press {
                // AX names the shortcut by character; the key sent is the US
                // position of that character, which another layout may put
                // elsewhere (Z and W swap on AZERTY: Undo would close).
                if layout_check_applies(key)
                    && !same_key_in_layout(key, crate::input::keyboard::layout_text_for(key).as_deref())
                {
                    return Background::skipped(
                        "the item's shortcut is not on the key this driver would send in the current keyboard layout",
                    );
                }
                if let Some(why) = shortcut_refused(pid, window_id) {
                    return Background::skipped(why);
                }
            }
            let window = ax_window(pid, window_id);
            let read = || observe(pid, window_id, window, item);
            let menus_before = crate::windows::menu_window_ids(pid).unwrap_or_default();
            let mark = crate::focus_steal::activation_mark();
            let lease =
                crate::focus_steal::begin_suppression(Some(pid), prior, "invoke_menu.background");
            let mut pressed_at = None;
            // The reads before the press and during the wait are made with
            // the window selected as key: selecting it changes what some
            // windows show, and that is not the command's effect.
            let ran = crate::input::skylight::with_background_key_window(pid, window_id, || {
                // The press must reach this window, not a sibling: the app's
                // focused window is read back before anything is sent.
                std::thread::sleep(BACKGROUND_POLL);
                let focused = crate::ax::bindings::focused_window_id_of_pid(pid);
                if focused != Some(window_id) {
                    return Ok(Err(match focused {
                        Some(other) => format!(
                            "selecting the window as key from behind left window {other} focused"
                        ),
                        None => "selecting the window as key from behind could not be read back"
                            .to_owned(),
                    }));
                }
                // A page can take a shortcut before the menu does: keys go
                // only to a focus proven to be outside web content.
                if matches!(press, Press::Keys { .. })
                    && !super::type_text::window_focus_is_proven_native(pid, window_id)
                {
                    return Ok(Err(
                        "the window's focus could not be proven to be outside web content, which can take the item's shortcut before the menu does"
                            .to_owned(),
                    ));
                }
                let first = read();
                std::thread::sleep(BACKGROUND_POLL);
                let before = first.steady(&read());
                // A text field takes a special key (Cmd+Delete deletes to
                // the line start) before a menu item that reads disabled:
                // the window's own focus is read right before the press.
                if matches!(&press, Press::Keys { key, .. } if !layout_check_applies(key))
                    && !special_key_reaches_menu(focused_role(pid, window_id).as_deref())
                {
                    return Ok(Err(
                        "the window's focus is a text field (or could not be read), which takes a special key such as Delete as editing before the menu does"
                            .to_owned(),
                    ));
                }
                pressed_at = Some(std::time::Instant::now());
                // An error says whether the press was made anyway: an AX
                // press was sent (menu items can report an error and still
                // run the command), a shortcut once its key-down was posted.
                let sent = match &press {
                    Press::Ax(action) => {
                        let error = perform_action(item, action);
                        (error == kAXErrorSuccess)
                            .then_some(())
                            .ok_or((format!("AX error {error}"), true))
                    }
                    Press::Keys { key, modifiers } => {
                        crate::input::keyboard::press_key_reporting_down(pid, key, modifiers)
                            .map_err(|(error, down)| (error.to_string(), down))
                    }
                };
                // Wait with the window still key: a command that runs on a
                // later turn of the app's loop finds the window it acts on.
                let deadline = std::time::Instant::now() + BACKGROUND_EFFECT_WAIT;
                let mut changed = Vec::new();
                while changed.is_empty() && std::time::Instant::now() < deadline {
                    std::thread::sleep(BACKGROUND_POLL);
                    changed = changes(&before, &read());
                }
                Ok(Ok((sent, before, changed)))
            });
            // Judge no earlier than the verdict delay after the press, and
            // with the window's key selection taken back: what the command
            // did must still show then.
            if let Some(left) =
                pressed_at.and_then(|at| BACKGROUND_VERDICT_AFTER.checked_sub(at.elapsed()))
            {
                std::thread::sleep(left);
            }
            let lasted = match &ran {
                Ok(Ok((_, before, changed))) if !changed.is_empty() => {
                    !changes(before, &read()).is_empty()
                }
                _ => false,
            };
            drop(lease);
            if let Some(window) = window {
                CFRelease(window as CFTypeRef);
            }
            let activated = crate::focus_steal::activated_since(mark, pid)
                || crate::apps::frontmost_pid() == Some(pid);
            // A press that opened a menu (it should not) is closed before
            // anything else runs.
            if crate::windows::new_menu_windows(pid, &menus_before).unwrap_or(0) > 0 {
                let app = AXUIElementCreateApplication(pid);
                if !app.is_null() {
                    set_messaging_timeout(app);
                    let _ = close_opened_menu(app, pid, &path[0], &menus_before);
                    CFRelease(app as CFTypeRef);
                }
            }
            verdict(
                ran.map(|ran| ran.map(|(sent, _, changed)| (sent, changed))),
                press,
                lasted,
                activated,
            )
        })();
        CFRelease(item as CFTypeRef);
        outcome
    }
}

fn select_frontmost_pid(
    front_process_matches: Option<bool>,
    workspace_fallback: impl FnOnce() -> Option<i32>,
    pid: i32,
) -> Option<i32> {
    match front_process_matches {
        Some(true) => Some(pid),
        Some(false) => None,
        None => workspace_fallback(),
    }
}

fn live_frontmost_pid(pid: i32, window_id: u32) -> Option<i32> {
    select_frontmost_pid(
        crate::input::skylight::front_process_matches(pid, window_id),
        crate::apps::frontmost_pid,
        pid,
    )
}

/// Make one exact application window key before resolving focus-sensitive
/// native menu state.
///
/// `SLPSSetFrontProcessWithOptions(..., kCPSNoWindows)` makes the application
/// active without broadly raising its windows. That is the right default for
/// input delivery, but it can leave the requested window non-key. macOS then
/// exposes contextual Window-menu commands (including Move & Resize) as
/// disabled even though the application itself is frontmost. Raise and mark
/// only the requested AX window, then require an exact focused-window readback
/// before menu resolution proceeds.
fn focus_ax_window(pid: i32, window_id: u32) -> Result<(), String> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err("invoke_menu: target application is unavailable".into());
        }
        set_messaging_timeout(app);

        let mut target = None;
        for window in copy_ax_windows(app) {
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);

        let Some(target) = target else {
            return Err("invoke_menu: target accessibility window is unavailable".into());
        };
        set_messaging_timeout(target);

        // SkyLight has requested native key status for this exact window. AX
        // raise/main/focus completes the corresponding visible and semantic
        // state; these writes remain best-effort for applications that expose
        // only a subset of the attributes.
        let _ = perform_action(target, "AXRaise");
        let _ = set_bool_attr_true(target, "AXMain");
        let _ = set_bool_attr_true(target, "AXFocused");
        CFRelease(target as CFTypeRef);
    }
    Ok(())
}

struct AxWindowFocusRequest {
    pid: i32,
    window_id: u32,
    tx: SyncSender<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
}

#[link(name = "System", kind = "framework")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

unsafe extern "C" fn focus_ax_window_on_main(context: *mut c_void) {
    let request = unsafe { Box::from_raw(context.cast::<AxWindowFocusRequest>()) };
    if request.cancelled.load(Ordering::Acquire) {
        return;
    }
    let result = focus_ax_window(request.pid, request.window_id);
    let _ = request.tx.send(result);
}

fn focus_ax_window_with_thread_affinity(pid: i32, window_id: u32) -> Result<(), String> {
    let is_main_thread = objc2_foundation::MainThreadMarker::new().is_some();
    if pid != std::process::id() as i32 || is_main_thread {
        return focus_ax_window(pid, window_id);
    }

    // AX actions against another process execute in that process. An embedded
    // driver targeting its own window is different: AppKit services AXRaise
    // in this process, and window ordering is main-thread-only. Queue just the
    // self-process AX mutation onto AppKit's main queue, then return to the
    // blocking worker for readiness polling and menu traversal.
    let (tx, rx) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = Box::new(AxWindowFocusRequest {
        pid,
        window_id,
        tx,
        cancelled: Arc::clone(&cancelled),
    });
    unsafe {
        let main_queue = &raw const _dispatch_main_q as *const c_void;
        dispatch_async_f(
            main_queue,
            Box::into_raw(request).cast::<c_void>(),
            focus_ax_window_on_main,
        );
    }
    match rx.recv_timeout(MAIN_QUEUE_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            // If AppKit never serviced the request, prevent a stale focus
            // change from firing after this tool call has already failed.
            cancelled.store(true, Ordering::Release);
            Err(format!(
                "invoke_menu: failed waiting for the embedded host window on the AppKit main queue: {error}"
            ))
        }
    }
}

fn focus_exact_window(pid: i32, window_id: u32) -> Result<(), String> {
    let native_key_requested = crate::input::skylight::make_exact_window_key(pid, window_id);
    if !native_key_requested
        && live_frontmost_pid(pid, window_id) != Some(pid)
        && !crate::apps::activate_pid(pid)
    {
        return Err("invoke_menu: target application could not be activated".into());
    }
    if !native_key_requested {
        std::thread::sleep(Duration::from_millis(120));
    }

    focus_ax_window_with_thread_affinity(pid, window_id)?;

    // AXFocusedWindow can lead AppKit's native `isKeyWindow` state while the
    // WindowServer activation is still settling. Menu validation observes the
    // latter. Require the exact app/window pair to remain stable briefly so a
    // loaded desktop cannot expose a transiently disabled contextual item.
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    let mut stable_since = None;
    loop {
        let now = std::time::Instant::now();
        if exact_window_is_ready(
            live_frontmost_pid(pid, window_id),
            pid,
            crate::ax::bindings::focused_as_target(
                crate::ax::bindings::focused_window_id_of_pid(pid),
                window_id,
            ),
            window_id,
        ) {
            let since = stable_since.get_or_insert(now);
            if now.duration_since(*since) >= Duration::from_millis(120) {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if now >= deadline {
            return Err(format!(
                "invoke_menu: target window {window_id} did not become stably key and frontmost"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn exact_window_is_ready(
    frontmost_pid: Option<i32>,
    target_pid: i32,
    focused_window_id: Option<u32>,
    target_window_id: u32,
) -> bool {
    frontmost_pid == Some(target_pid) && focused_window_id == Some(target_window_id)
}

/// Which route ran the command.
#[derive(Debug, PartialEq)]
enum Ran {
    /// Pressed from behind. `changed`: what a read during the wait showed
    /// changed; `lasted`: the read at the end still showed it; `activated`:
    /// the app came to the front anyway; `restored`: the previous front app
    /// is front again; `error`: what the press returned, if an error.
    Background {
        press: Press,
        changed: Vec<&'static str>,
        lasted: bool,
        activated: bool,
        restored: bool,
        error: Option<String>,
    },
    /// The target was activated for the menu action. `why`: why the press
    /// from behind was not used (`None` when the target was already in front
    /// or no other app was). `unhidden`: the app was hidden before and the
    /// activation showed it.
    Foreground {
        front: Option<FrontAfter>,
        why: Option<String>,
        unhidden: bool,
    },
}

impl Ran {
    /// Whether the target app never came to the front.
    fn stayed_behind(&self) -> bool {
        matches!(
            self,
            Ran::Background {
                activated: false,
                ..
            }
        )
    }

    fn transport(&self) -> ActionTransport {
        match self {
            Ran::Background {
                press: Press::Keys { .. },
                ..
            } => ActionTransport::MacosCgEventPid,
            _ => ActionTransport::MacosAxAction,
        }
    }
}

/// What the call does after the attempt from behind: `Ok` ends it with that
/// route, `Err` runs the foreground route, for that reason. Once the item
/// was pressed from behind the call ends, also when nothing showed or the
/// press returned an error: the command may still run, and pressing it in
/// front too can run it twice. `restore` confirms the previous front app is
/// front again; it runs only when the app came to the front.
fn after_background(outcome: Background, restore: impl FnOnce() -> bool) -> Result<Ran, String> {
    match outcome {
        Background::Skipped(why) => Err(why),
        Background::Pressed {
            press,
            changed,
            lasted,
            activated,
            error,
        } => Ok(Ran::Background {
            press,
            changed,
            lasted,
            activated,
            restored: !activated || restore(),
            error,
        }),
    }
}

/// What a press from behind only ends with: `Ok` with the call's words once
/// the item was pressed (also when the press returned an error or nothing
/// showed: the caller must not run the command another way), `Err` with why
/// it was not pressed.
fn behind_only(outcome: Background, restore: impl FnOnce() -> bool) -> Result<String, String> {
    after_background(outcome, restore).map(|ran| summary(&ran))
}

/// Run the final item of `path` from behind and never in front, by its key
/// equivalent only, for a tool that has a route of its own when this one
/// cannot run (double_click opens a Finder item with File > Open). Holds the per-process lease (or reuses
/// the caller's) and the foreground turn, as [`InvokeMenuTool`] does.
pub(crate) async fn press_from_behind_only(
    pid: i32,
    window_id: u32,
    path: Vec<String>,
    // Run once the foreground turn is held, before anything is sent; `Err`
    // stops the press with that reason.
    ready: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<String, String> {
    let mutation = if crate::background_mutation::held_by_current_task(pid) {
        None
    } else {
        Some(crate::background_mutation::acquire(pid).await)
    };
    // The worker keeps the lease (its own, or the caller's it reuses) when
    // the call is cancelled: background_mutation::spawn_blocking carries it.
    crate::background_mutation::spawn_blocking(move || {
        let _mutation = mutation;
        let _turn = crate::input::mouse::foreground_turn().map_err(|busy| busy.to_string())?;
        let prior = crate::apps::frontmost_pid()
            .filter(|prior| *prior != pid)
            .ok_or_else(|| "the app is in front (or no other app is)".to_owned())?;
        ready()?;
        let restore = || {
            crate::apps::confirm_front(prior)
                || (crate::apps::restore_prior_app(prior) && crate::apps::confirm_front(prior))
        };
        behind_only(press_in_background(pid, window_id, &path, prior, true), restore)
    })
    .await
    .unwrap_or_else(|error| Err(format!("the press from behind did not run ({error})")))
}

fn summary(ran: &Ran) -> String {
    match ran {
        Ran::Background {
            press,
            changed,
            activated,
            restored,
            error,
            ..
        } if changed.is_empty() => {
            let mut text = format!(
                "Pressed the menu item from behind (every step of the path resolved uniquely; {}",
                press.words()
            );
            if let Some(error) = error {
                text.push_str(&format!("; the press returned an error ({error})"));
            }
            if *activated {
                text.push_str(&format!(
                    "), but the app came to the front by itself during the command; {}",
                    if *restored {
                        "the previous front app is front again."
                    } else {
                        "the previous front app was not confirmed back in front."
                    }
                ));
            } else {
                text.push_str("): the app was not brought to the front.");
            }
            text.push_str(&format!(
                " Nothing changed within {} ms after the press. The command may finish later, so it was not pressed again; read the window before repeating it, because repeating it can run it twice. What the command did is in the Outcome line.",
                BACKGROUND_EFFECT_WAIT.as_millis()
            ));
            text
        }
        Ran::Background {
            press,
            changed,
            lasted,
            activated,
            restored,
            error,
        } => {
            let mut text = format!(
                "Ran the menu command from behind (every step of the path resolved uniquely; {}{})",
                press.words(),
                error
                    .as_ref()
                    .map(|error| format!("; the press returned an error ({error})"))
                    .unwrap_or_default()
            );
            if *activated {
                text.push_str(&format!(
                    " and {} changed, but the app came to the front by itself during the command; {}",
                    words(changed),
                    if *restored {
                        "the previous front app is front again."
                    } else {
                        "the previous front app was not confirmed back in front."
                    }
                ));
            } else {
                text.push_str(&format!(
                    ": the app was not brought to the front, and {} changed after it.",
                    words(changed)
                ));
            }
            if !lasted {
                text.push_str(
                    " That change no longer showed at the end of the call, so the command was not pressed again; read the window before repeating it.",
                );
            }
            text.push_str(" What the command did is in the Outcome line.");
            text
        }
        Ran::Foreground {
            front,
            why,
            unhidden,
        } => {
            let mut text = "Pressed the menu item (every step of the path resolved uniquely); what the command did is in the Outcome line.".to_owned();
            if let Some(why) = why {
                text.push_str(&format!(" It ran in the foreground because {why}."));
            }
            text.push_str(match front {
                Some(FrontAfter::Restored) => " The target app was active only for the menu action; the previous front app is front again.",
                Some(FrontAfter::NotConfirmed) => " The target app was activated for the menu action; the previous front app was not confirmed back in front.",
                Some(FrontAfter::KeptForInlineEdit) => " The target app stays in front: the command opened an inline editor, which the app ends when it loses the front (the Outcome line says when the previous front app comes back).",
                None => "",
            });
            if *unhidden {
                text.push_str(" The app was hidden; activating it showed it, and it stays shown.");
            }
            text
        }
    }
}

/// Whether the app with `pid` is hidden (Hide <App>), `None` when unknown.
fn app_hidden(pid: i32) -> Option<bool> {
    use objc2_app_kit::NSRunningApplication;
    unsafe {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid).map(|app| app.isHidden())
    }
}

/// "the clipboard", "the clipboard and the app's windows", "a, b and c".
fn words(list: &[&str]) -> String {
    match list {
        [] => "nothing".into(),
        [one] => (*one).into(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": { "code": "menu_path_unavailable", "message": message }
    }))
}

#[async_trait]
impl Tool for InvokeMenuTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let input: InvokeMenuInput =
            match cua_driver_core::tool_args::parse_typed_input("invoke_menu", args) {
                Ok(input) => input,
                Err(result) => return result,
            };
        let path = match normalized_path(input.path) {
            Ok(path) => path,
            Err(error) => return refusal(error),
        };
        let pid = match i32::try_from(input.pid) {
            Ok(pid) => pid,
            Err(_) => return refusal("invoke_menu: pid is out of range".into()),
        };
        let window_id = match u32::try_from(input.window_id) {
            Ok(window_id) => window_id,
            Err(_) => return refusal("invoke_menu: window_id is out of range".into()),
        };
        if !crate::windows::all_windows()
            .iter()
            .any(|window| window.pid == pid && window.window_id == window_id)
        {
            return refusal("invoke_menu: window_id does not belong to pid".into());
        }

        // Background input to one process is serialized (the hotkey tool
        // holds the same lease): another session's keys must not interleave
        // with the press from behind, its key-window selection or its reads.
        let mutation = if crate::background_mutation::held_by_current_task(pid) {
            None
        } else {
            Some(crate::background_mutation::acquire(pid).await)
        };
        let outcome = tokio::task::spawn_blocking(move || {
            // Owned by the worker: a cancelled call must not release it while
            // the worker is still sending and reading.
            let _mutation = mutation;
            // The foreground admission, before the front is read: either
            // route may change the front (the foreground route activates, the
            // background one restores), and the turn is held through the
            // hand-back.
            let _turn = crate::input::mouse::turn_without_user_wait()
                .map_err(|busy| format!("invoke_menu: {busy}"))?;
            // apps::frontmost_pid reads WindowServer first, so it is live here.
            let prior_frontmost = crate::apps::frontmost_pid();
            // Read with it, in case the foreground route keeps the app in
            // front for an inline edit (see `crate::front_lease`).
            let before = crate::front_lease::Before::now(prior_frontmost);
            // From behind first, when another app is in front: the command
            // then runs without the target coming forward.
            let mut why_foreground = None;
            if let Some(prior) = prior_frontmost.filter(|prior| *prior != pid) {
                // The lease puts the previous app back when the target
                // activates itself; confirm it, and ask once more if not.
                let restore = || {
                    crate::apps::confirm_front(prior)
                        || (crate::apps::restore_prior_app(prior)
                            && crate::apps::confirm_front(prior))
                };
                match after_background(press_in_background(pid, window_id, &path, prior, false), restore) {
                    Ok(ran) => return Ok(ran),
                    Err(why) => why_foreground = Some(why),
                }
            }
            // The foreground route brings the target forward: it waits while
            // the user is typing, as every foreground action does.
            crate::input::user_activity::wait_until_user_idle()
                .map_err(|busy| format!("invoke_menu: {busy}"))?;
            let prior_frontmost_window =
                prior_frontmost.and_then(crate::ax::bindings::focused_window_id_of_pid);
            let hidden_before = app_hidden(pid) == Some(true);

            // Read once the target window is key and before the press: a
            // command can retitle its item (New Folder with Selection), and
            // the editor wait needs the answer.
            // ponytail: a localized title that the menu has not yet updated
            // for this window falls back to the shortcut, then to the generic
            // wait (as before item 41); English titles match by prefix.
            let focused = focus_exact_window(pid, window_id);
            let opens_editor = focused.is_ok()
                && crate::tools::edit_commit::app_saves_on_end_editing(pid)
                && item_opens_editor(pid, &path);
            let result = focused.and_then(|()| unsafe { invoke_path(pid, &path) });

            // Restore the exact prior key window of another application when one
            // was observable, falling back to app activation for apps without
            // an AX window. Within the target application the requested window
            // stays key: the menu command acts on the key window after this
            // call returns (TextEdit writes a saved document asynchronously),
            // and re-keying a sibling document dropped the save.
            let mut front_restored = None;
            // A command that opened an inline editor (File > Rename) keeps
            // the app in front: the app cancels that edit when it loses the
            // front, so handing it back would undo the command.
            let other_front = prior_frontmost.filter(|prior_pid| *prior_pid != pid);
            let kept = other_front.is_some()
                && result.is_ok()
                && crate::tools::edit_commit::app_saves_on_end_editing(pid)
                && {
                    let expected = opens_editor;
                    let wait = if expected { EXPECTED_INLINE_EDIT_WAIT } else { INLINE_EDIT_WAIT };
                    crate::ax::bindings::await_inline_edit_after_menu(pid, window_id, wait, expected)
                };
            if kept {
                front_restored = Some(FrontAfter::KeptForInlineEdit);
                crate::front_lease::begin(pid, before);
            }
            if let Some(prior_pid) = other_front.filter(|_| !kept) {
                let restored_exact = prior_frontmost_window.is_some_and(|prior_window_id| {
                    focus_exact_window(prior_pid, prior_window_id).is_ok()
                });
                if !restored_exact {
                    let _ = crate::apps::restore_prior_app(prior_pid);
                }
                front_restored = Some(if crate::apps::confirm_front(prior_pid) {
                    FrontAfter::Restored
                } else {
                    FrontAfter::NotConfirmed
                });
            }
            result
                .map(|()| Ran::Foreground {
                    front: front_restored,
                    why: why_foreground.clone(),
                    unhidden: hidden_before && app_hidden(pid) == Some(false),
                })
                .map_err(|error| match &why_foreground {
                    Some(why) => format!("{error} (it ran in the foreground because {why})"),
                    None => error,
                })
        })
        .await;

        match outcome {
            Ok(Ok(ran)) => ToolResult::text(summary(&ran)).with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ran.transport(),
                    RequestedDelivery::Background,
                )
                .actual_delivery(if ran.stayed_behind() {
                    ActualDelivery::Background
                } else {
                    ActualDelivery::Foreground
                })
                .evidence(ActionEvidence {
                    kind: EvidenceKind::NativeApiResult,
                    detail: match &ran {
                        Ran::Background { changed, .. } if changed.is_empty() => format!(
                            "Every menu hop resolved uniquely and the press from behind was made; no read within {} ms after it differed",
                            BACKGROUND_EFFECT_WAIT.as_millis()
                        ),
                        Ran::Background { .. } => "Every menu hop resolved uniquely and a read after the press from behind differed".into(),
                        Ran::Foreground { .. } => "Every menu hop resolved uniquely and AX accepted the final action".into(),
                    },
                })
                .build()
                .expect("invoke_menu record is valid"),
            ),
            Ok(Err(error)) => refusal(error),
            Err(error) => refusal(format!("invoke_menu: blocking task failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finders_rename_and_new_folder_items_are_known_to_open_an_editor() {
        let path = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(opens_inline_editor(&path(&["File", "Rename"])));
        assert!(opens_inline_editor(&path(&["File", "Rename…"])));
        assert!(opens_inline_editor(&path(&["File", "New Folder"])));
        assert!(opens_inline_editor(&path(&["File", "New Folder with Selection (2 Items)"])));
        assert!(!opens_inline_editor(&path(&["File", "New Finder Window"])));
        assert!(!opens_inline_editor(&path(&["View", "as List"])));
        assert!(!opens_inline_editor(&path(&["Rename", "Other"])));
        assert!(!opens_inline_editor(&path(&["Edit", "Rename"])));
        assert!(!opens_inline_editor(&path(&["File", "New Folder Template"])));
        assert!(!opens_inline_editor(&path(&["File", "Sub", "Rename"])));
    }

    #[test]
    fn path_normalization_rejects_empty_segments() {
        assert_eq!(
            normalized_path(vec![" Window ".into(), " Left ".into()]).unwrap(),
            vec!["Window", "Left"]
        );
        assert!(normalized_path(vec!["Window".into(), "  ".into()]).is_err());
    }

    #[test]
    fn action_selection_is_explicit_and_ordered() {
        let actions = vec!["AXShowMenu".into(), "AXPress".into()];
        assert_eq!(choose_action(&actions, false), Some("AXPress"));
        assert_eq!(choose_action(&actions, true), Some("AXPress"));
        assert_eq!(choose_action(&["AXShowMenu".into()], true), None);
    }

    #[test]
    fn a_failed_path_says_whether_its_menu_was_closed() {
        let error = || "invoke_menu: path segment 1 was not found".to_owned();
        assert_eq!(
            failure_after_press(error(), "File", Some(true)),
            "invoke_menu: path segment 1 was not found. No menu window this call opened is still on screen."
        );
        for unconfirmed in [Some(false), None] {
            assert!(failure_after_press(error(), "File", unconfirmed)
                .ends_with("The File menu this call opened may still be open: press escape on the window before other input."));
        }
    }

    #[test]
    fn three_periods_match_the_ellipsis_in_menu_titles() {
        assert_eq!(
            menu_title_key(" Save As... "),
            menu_title_key("Save As\u{2026}")
        );
        assert_eq!(menu_title_key("Save As..."), menu_title_key("Save As..."));
        assert_ne!(menu_title_key("Save As"), menu_title_key("Save As\u{2026}"));
        assert_ne!(
            menu_title_key("save as..."),
            menu_title_key("Save As\u{2026}")
        );
    }

    #[test]
    fn a_missing_item_lists_what_the_menu_holds() {
        let titles = vec!["New".to_owned(), "".to_owned(), "Open…".to_owned()];
        assert_eq!(listing(&titles), "New, Open…");
        assert_eq!(listing(&[]), "no titled items");
        let many: Vec<String> = (0..40).map(|n| n.to_string()).collect();
        assert_eq!(listing(&many).split(", ").count(), 30);
    }

    #[test]
    fn menu_focus_requires_the_exact_frontmost_app_and_window() {
        assert!(exact_window_is_ready(Some(7), 7, Some(42), 42));
        assert!(!exact_window_is_ready(Some(8), 7, Some(42), 42));
        assert!(!exact_window_is_ready(Some(7), 7, Some(41), 42));
        assert!(!exact_window_is_ready(Some(7), 7, None, 42));
    }

    fn workspace_frontmost_must_not_be_read() -> Option<i32> {
        panic!("stale workspace state must not be consulted");
    }

    #[test]
    fn windowserver_mismatch_never_falls_back_to_stale_workspace_state() {
        assert_eq!(
            select_frontmost_pid(Some(true), workspace_frontmost_must_not_be_read, 7),
            Some(7)
        );
        assert_eq!(
            select_frontmost_pid(Some(false), workspace_frontmost_must_not_be_read, 7),
            None
        );
        assert_eq!(select_frontmost_pid(None, || Some(8), 7), Some(8));
        assert_eq!(select_frontmost_pid(None, || None, 7), None);
    }
    #[test]
    fn a_key_equivalent_reads_ax_modifier_bits() {
        let keys = |c: &str, flags: f64| key_equivalent(Some(c), None, Some(flags));
        assert_eq!(keys("C", 0.0), Some(("c".into(), vec!["cmd"])));
        // Save As: shift and option on top of command.
        assert_eq!(
            keys("S", 3.0),
            Some(("s".into(), vec!["cmd", "shift", "option"]))
        );
        assert_eq!(keys("T", 1.0), Some(("t".into(), vec!["cmd", "shift"])));
        assert_eq!(keys(",", 4.0), Some((",".into(), vec!["cmd", "ctrl"])));
        // Bit 8 is "no command": control alone still makes a chord.
        assert_eq!(keys("A", 12.0), Some(("a".into(), vec!["ctrl"])));
        assert_eq!(keys("A", 8.0), None);
        // Unreadable or malformed modifiers: no shortcut is guessed.
        assert_eq!(key_equivalent(Some("Z"), None, None), None);
        assert_eq!(keys("Z", 16.0), None);
        assert_eq!(keys("Z", 1.5), None);
        for none in ["", "AB", " ", "\u{7f}"] {
            assert_eq!(keys(none, 0.0), None, "{none:?}");
        }
        assert_eq!(key_equivalent(None, None, Some(0.0)), None);
    }

    /// Item 41: Finder's Move to Trash reads as U+0008 with virtual key 51
    /// and Command (measured in VM B), and was never tried from behind.
    #[test]
    fn a_special_key_equivalent_is_read_from_its_virtual_key() {
        let keys = |c: Option<&str>, vkey: i64, flags: f64| key_equivalent(c, Some(vkey), Some(flags));
        assert_eq!(keys(Some("\u{8}"), 51, 0.0), Some(("delete".into(), vec!["cmd"])));
        // Delete Immediately: Command and Option.
        assert_eq!(keys(Some("\u{8}"), 51, 2.0), Some(("delete".into(), vec!["cmd", "option"])));
        assert_eq!(keys(None, 126, 0.0), Some(("up".into(), vec!["cmd"])));
        assert_eq!(keys(Some(""), 117, 1.0), Some(("forward_delete".into(), vec!["cmd", "shift"])));
        // Never a bare key, never a key this driver cannot name.
        assert_eq!(keys(Some("\u{8}"), 51, 8.0), None);
        assert_eq!(keys(None, 200, 0.0), None);
        assert_eq!(key_equivalent(Some("\u{8}"), None, Some(0.0)), None);
        // A printable character wins over the virtual key.
        assert_eq!(keys(Some("C"), 8, 0.0), Some(("c".into(), vec!["cmd"])));
    }

    /// Review round 1: a focused text field takes Cmd+Delete as editing when
    /// the menu item reads disabled; a localized New Folder is known by its
    /// shortcut.
    #[test]
    fn a_special_key_is_not_sent_into_a_text_field() {
        assert!(special_key_reaches_menu(Some("AXList")));
        assert!(special_key_reaches_menu(Some("AXOutline")));
        for text in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert!(!special_key_reaches_menu(Some(text)), "{text}");
        }
        assert!(!special_key_reaches_menu(None));
        assert!(opens_editor_by_shortcut("n", &["cmd", "shift"]));
        assert!(opens_editor_by_shortcut("n", &["cmd", "ctrl"]));
        assert!(!opens_editor_by_shortcut("n", &["cmd"]));
        assert!(!opens_editor_by_shortcut("d", &["cmd", "shift"]));
    }

    #[test]
    fn a_special_key_shortcut_needs_no_layout_check() {
        assert!(!layout_check_applies("delete"));
        assert!(!layout_check_applies("up"));
        assert!(layout_check_applies("z"));
        assert!(layout_check_applies(","));
    }

    /// Item 41: every Finder menu command ran in front, over the user's app,
    /// although only the commands that open an inline editor need it.
    #[test]
    fn finders_other_menu_commands_are_tried_from_behind() {
        let path = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let trash = path(&["File", "Move to Trash"]);
        let route = |p: Vec<String>, open: Option<bool>| {
            front_route_for_inline_edit(true, || opens_inline_editor(&p), || open)
        };
        assert_eq!(route(trash.clone(), Some(false)), None);
        assert_eq!(route(path(&["Edit", "Copy"]), Some(false)), None);
        assert!(route(path(&["File", "New Folder"]), Some(false)).is_some());
        assert!(route(path(&["File", "Rename"]), Some(false)).is_some());
        // A localized New Folder, known by its shortcut.
        assert!(front_route_for_inline_edit(true, || true, || Some(false)).is_some());
        // An edit open now, or a focus that did not read: today's route.
        assert!(route(trash.clone(), Some(true)).is_some());
        assert!(route(trash, None).is_some());
        // Other apps are never held to it, and nothing is read.
        assert_eq!(front_route_for_inline_edit(false, || panic!("item"), || panic!("focus")), None);
    }

    #[test]
    fn a_shortcut_is_sent_only_where_the_layout_types_its_character() {
        assert!(same_key_in_layout("z", Some("z")));
        assert!(same_key_in_layout("z", Some("Z")));
        // AZERTY: the US Z key types w.
        assert!(!same_key_in_layout("z", Some("w")));
        assert!(!same_key_in_layout(",", Some(";")));
        assert!(!same_key_in_layout("c", None));
    }

    #[test]
    fn an_item_that_reads_disabled_from_behind_is_tried_by_its_shortcut() {
        let copy = || Some(("c".to_owned(), vec!["cmd"]));
        assert_eq!(
            choose_press(Some(true), Some("AXPress"), copy()),
            Ok(Press::Ax("AXPress"))
        );
        assert_eq!(
            choose_press(None, Some("AXPress"), None),
            Ok(Press::Ax("AXPress"))
        );
        assert_eq!(
            choose_press(Some(false), Some("AXPress"), copy()),
            Ok(Press::Keys {
                key: "c".into(),
                modifiers: vec!["cmd"]
            })
        );
        assert_eq!(
            choose_press(Some(true), None, copy()),
            Ok(Press::Keys {
                key: "c".into(),
                modifiers: vec!["cmd"]
            })
        );
        // Finder's Rename: disabled from behind, no shortcut: foreground.
        assert!(choose_press(Some(false), Some("AXPress"), None)
            .unwrap_err()
            .contains("no key equivalent"));
        assert!(choose_press(Some(true), None, None).is_err());
    }

    fn observed(clipboard: isize) -> Observed {
        Observed {
            alive: Some(true),
            clipboard: Some(clipboard),
            windows: Some(vec![(1, [0, 0, 200, 200], true)]),
            facts: Some(crate::outcome::Facts::default()),
            ui: Some(7),
            item: Some((Some("Copy".into()), None)),
        }
    }

    #[test]
    fn only_a_field_both_reads_know_counts_as_a_change() {
        let before = observed(1);
        assert!(changes(&before, &before.clone()).is_empty());
        assert_eq!(changes(&before, &observed(2)), vec!["the clipboard"]);
        // Quit: every other read goes unknown, the process is gone.
        let quit = Observed {
            alive: Some(false),
            ..Observed::default()
        };
        assert_eq!(
            changes(&before, &quit),
            vec!["whether the app is running (it quit)"]
        );
        let mut after = before.clone();
        after.ui = Some(8);
        after.item = Some((Some("Copy".into()), Some("\u{2713}".into())));
        assert_eq!(
            changes(&before, &after),
            vec!["what the window shows", "the menu item's state"]
        );
        // An unknown read is no change in either direction.
        let unknown = Observed::default();
        assert!(changes(&before, &unknown).is_empty());
        assert!(changes(&unknown, &before).is_empty());
    }

    #[test]
    fn a_field_that_moves_with_nothing_pressed_is_dropped() {
        let first = observed(1);
        let mut again = observed(1);
        again.ui = Some(9);
        let steady = first.steady(&again);
        assert_eq!(steady.ui, None);
        assert_eq!(steady.clipboard, Some(1));
        // The clock that moved says nothing about the press.
        let mut after = observed(1);
        after.ui = Some(10);
        assert!(changes(&steady, &after).is_empty());
    }

    #[test]
    fn delivery_is_background_only_when_the_app_never_came_forward() {
        let keys = || Press::Keys {
            key: "c".into(),
            modifiers: vec!["cmd"],
        };
        let behind = Ran::Background {
            press: keys(),
            changed: vec!["the clipboard"],
            lasted: true,
            activated: false,
            restored: true,
            error: None,
        };
        assert!(behind.stayed_behind());
        assert_eq!(behind.transport(), ActionTransport::MacosCgEventPid);
        let text = summary(&behind);
        assert!(
            text.starts_with("Ran the menu command from behind"),
            "{text}"
        );
        assert!(
            text.contains("cmd+c")
                && text.contains("not brought to the front")
                && text.contains("the clipboard changed")
                && !text.contains("no longer showed"),
            "{text}"
        );

        let came = Ran::Background {
            press: Press::Ax("AXPress"),
            changed: vec!["the app's windows"],
            lasted: true,
            activated: true,
            restored: true,
            error: None,
        };
        assert!(!came.stayed_behind());
        assert_eq!(came.transport(), ActionTransport::MacosAxAction);
        let text = summary(&came);
        assert!(
            text.contains("came to the front by itself") && !text.contains("not brought"),
            "{text}"
        );
        let lost = Ran::Background {
            press: Press::Ax("AXPress"),
            changed: vec!["what the window shows"],
            lasted: false,
            activated: true,
            restored: false,
            error: None,
        };
        let text = summary(&lost);
        assert!(text.contains("not confirmed back in front"), "{text}");
        // A change that went away is said, and the command is not run again.
        assert!(
            text.contains("no longer showed") && text.contains("not pressed again"),
            "{text}"
        );

        let fg = Ran::Foreground {
            front: Some(FrontAfter::Restored),
            why: Some(
                "the item was not found in its closed menu (some apps fill a menu only when it opens)"
                    .into(),
            ),
            unhidden: true,
        };
        assert!(!fg.stayed_behind());
        let text = summary(&fg);
        assert!(
            text.contains("It ran in the foreground because the item was not found"),
            "{text}"
        );
        assert!(
            text.contains("previous front app is front again") && text.contains("was hidden"),
            "{text}"
        );
        let front = Ran::Foreground {
            front: None,
            why: None,
            unhidden: false,
        };
        assert!(!summary(&front).contains("foreground because"));
        let kept = Ran::Foreground {
            front: Some(FrontAfter::KeptForInlineEdit),
            why: Some("this app ends an inline edit (Rename, New Folder) when it loses the front, so its menu commands run in front".into()),
            unhidden: false,
        };
        let text = summary(&kept);
        assert!(
            text.contains("stays in front") && text.contains("inline edit"),
            "{text}"
        );
    }

    fn pressed(changed: Vec<&'static str>, activated: bool, error: Option<&str>) -> Background {
        Background::Pressed {
            press: Press::Keys {
                key: "v".into(),
                modifiers: vec!["cmd"],
            },
            changed,
            lasted: true,
            activated,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn a_press_from_behind_with_nothing_seen_is_not_pressed_again_in_front() {
        // Slack's Paste: the text showed after the wait, and the foreground
        // route pasted it a second time.
        let ran = after_background(pressed(vec![], false, None), || {
            panic!("the app never came to the front")
        })
        .expect("the press from behind ends the call");
        assert!(ran.stayed_behind());
        assert_eq!(ran.transport(), ActionTransport::MacosCgEventPid);
        let text = summary(&ran);
        assert!(
            text.contains("from behind")
                && text.contains("cmd+v")
                && text.contains("600 ms")
                && text.contains("may finish later")
                && text.contains("read the window before repeating it")
                && text.contains("twice")
                && !text.contains("foreground"),
            "{text}"
        );
    }

    #[test]
    fn a_press_from_behind_that_returned_an_error_is_not_pressed_again() {
        // A menu item's AXPress can report an error and still run the command.
        let ran = after_background(
            Background::Pressed {
                press: Press::Ax("AXPress"),
                changed: vec![],
                lasted: false,
                activated: false,
                error: Some("AX error -25204".into()),
            },
            || true,
        )
        .expect("the press from behind ends the call");
        assert!(ran.stayed_behind());
        let text = summary(&ran);
        assert!(
            text.contains("AX error -25204")
                && text.contains("may finish later")
                && text.contains("twice"),
            "{text}"
        );
    }

    #[test]
    fn a_press_from_behind_with_nothing_seen_reports_an_app_that_came_forward() {
        let mut asked = false;
        let ran = after_background(pressed(vec![], true, None), || {
            asked = true;
            false
        })
        .expect("the press from behind ends the call");
        assert!(asked, "the previous front app is confirmed");
        assert!(!ran.stayed_behind());
        let text = summary(&ran);
        assert!(
            text.contains("came to the front by itself")
                && text.contains("not confirmed back in front"),
            "{text}"
        );
    }

    #[test]
    fn a_change_seen_after_the_press_ends_the_call_as_before() {
        let ran = after_background(pressed(vec!["the clipboard"], false, None), || true)
            .expect("the press from behind ends the call");
        assert!(summary(&ran).contains("the clipboard changed"));
    }

    #[test]
    fn a_press_counts_once_it_was_sent() {
        let keys = || Press::Keys {
            key: "v".into(),
            modifiers: vec!["cmd"],
        };
        // AXPress reported an error and nothing showed: pressed.
        assert_eq!(
            verdict(
                Ok(Ok((Err(("AX error -25204".into(), true)), vec![]))),
                Press::Ax("AXPress"),
                false,
                false
            ),
            Background::Pressed {
                press: Press::Ax("AXPress"),
                changed: vec![],
                lasted: false,
                activated: false,
                error: Some("AX error -25204".into()),
            }
        );
        // Sent with nothing seen: pressed.
        assert!(matches!(
            verdict(Ok(Ok((Ok(()), vec![]))), keys(), false, false),
            Background::Pressed { error: None, .. }
        ));
        // The key-down went out and the key-up failed: pressed.
        assert!(matches!(
            verdict(
                Ok(Ok((Err(("key-up".into(), true)), vec![]))),
                keys(),
                false,
                false
            ),
            Background::Pressed { error: Some(_), .. }
        ));
        // The key-down was never posted: not pressed, the foreground route runs.
        assert!(matches!(
            verdict(Ok(Ok((Err(("CGEventSource::new failed".into(), false)), vec![]))), keys(), false, false),
            Background::Skipped(why) if why.contains("could not be sent from behind")
        ));
        // Refused before the press, or the window was never key: not pressed.
        assert!(matches!(
            verdict(Ok(Err("focus not proven".into())), keys(), false, false),
            Background::Skipped(_)
        ));
        assert!(matches!(
            verdict(Err(anyhow::anyhow!("rejected")), keys(), false, false),
            Background::Skipped(why) if why.contains("could not be made key")
        ));
    }

    #[test]
    fn a_press_error_is_said_also_when_something_changed() {
        let ran = after_background(
            Background::Pressed {
                press: Press::Ax("AXPress"),
                changed: vec!["the app's windows"],
                lasted: true,
                activated: false,
                error: Some("AX error -25204".into()),
            },
            || true,
        )
        .unwrap();
        let text = summary(&ran);
        assert!(
            text.contains("AX error -25204") && text.contains("the app's windows changed"),
            "{text}"
        );
    }

    #[test]
    fn a_keys_only_press_is_the_shortcut_whatever_the_item_reads() {
        // An item that reads enabled (or unreadable) with AXPress would get an
        // AX press from choose_press; keys only takes its shortcut instead.
        let cmd_o = || Some(("o".to_owned(), vec!["cmd"]));
        assert_eq!(choose_press(Some(true), Some("AXPress"), cmd_o()), Ok(Press::Ax("AXPress")));
        assert_eq!(choose_press(None, Some("AXPress"), cmd_o()), Ok(Press::Ax("AXPress")));
        assert_eq!(keys_press(cmd_o()), Ok(Press::Keys { key: "o".into(), modifiers: vec!["cmd"] }));
        assert!(keys_press(None).is_err());
    }

    #[test]
    fn a_press_from_behind_only_counts_once_sent_whatever_it_showed() {
        let keys = || Press::Keys { key: "o".into(), modifiers: vec!["cmd"] };
        for outcome in [
            pressed(vec![], false, None),
            pressed(vec![], false, Some("AX error -25200")),
            Background::Pressed { press: keys(), changed: vec![], lasted: false, activated: false, error: None },
            Background::Pressed { press: keys(), changed: vec!["the app's windows"], lasted: true, activated: true, error: None },
        ] {
            assert!(behind_only(outcome, || true).is_ok(), "a sent press must end the call");
        }
        assert_eq!(
            behind_only(Background::skipped("the app has an inline edit open"), || true),
            Err("the app has an inline edit open".to_owned())
        );
    }

    #[test]
    fn nothing_pressed_from_behind_still_runs_in_front() {
        let why =
            "the item was not found in its closed menu (some apps fill a menu only when it opens)";
        let fallback = after_background(Background::skipped(why), || true)
            .expect_err("the foreground route runs");
        assert!(format!("{fallback:?}").contains("not found in its closed menu"));
    }

    #[test]
    fn a_change_list_reads_as_words() {
        assert_eq!(words(&[]), "nothing");
        assert_eq!(words(&["the clipboard"]), "the clipboard");
        assert_eq!(words(&["a", "b", "c"]), "a, b and c");
    }
}
