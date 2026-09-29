//! Native input aimed at a Chrome web page while cua's Chrome extension is
//! connected to that Chrome.
//!
//! Page frameworks such as React own their inputs' state and ignore an
//! accessibility value write, so a native `type_text` or `set_value` into a
//! page can report success while the page never saw the text. When the
//! extension is connected, the typed browser tools reach the same page through
//! Chrome's own input pipeline and read the result back. Native input tools
//! therefore refuse page targets there, before sending anything, and name the
//! calls to make instead. Chrome's own UI (address bar, toolbar, permission
//! prompts, extension UI) has no web area and keeps native input, as do pages
//! an extension cannot reach (chrome:// pages, other extensions, the Web
//! Store).
//!
//! Redirect rule: refuse native input only when (1) the extension link for
//! this Chrome provably shows the exact target window and page (a window of
//! the link matches the native window's bounds and its active tab shows the
//! page's URL; incognito windows without extension access and other profiles'
//! windows share the browser pid but never appear on the link), and (2) the
//! browser tools support the control (text controls for typing and value
//! setting; any element for clicks). Anything else keeps the native path.

use crate::ax::bindings::{copy_string_attr, copy_url_attr, AXUIElementRef};
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use cua_driver_core::protocol::ToolResult;

/// Whether cua's Chrome extension running in browser process `pid` is linked
/// to this driver.
fn extension_connected(pid: i32) -> bool {
    cua_driver_core::browser::extension_bridge::global()
        .links()
        .iter()
        .any(|link| link.chrome_pid == Some(i64::from(pid)))
}

/// Whether Chrome lets an extension debug a page at `url`. Unknown (empty)
/// URLs count as unreachable, so native input stays available there.
fn extension_can_reach(url: &str) -> bool {
    let url = url.to_ascii_lowercase();
    !url.is_empty()
        && ![
            "chrome:",
            "chrome-extension:",
            "chrome-untrusted:",
            "chrome-search:",
            "devtools:",
            "https://chromewebstore.google.com",
            "https://chrome.google.com/webstore",
        ]
        .iter()
        .any(|prefix| url.starts_with(prefix))
}

/// The URL of the web page `start` sits in, or `None` when it is not inside
/// a web area (browser UI, native app). Borrows `start`.
///
/// # Safety
///
/// `start` must be a live AX element reference.
unsafe fn enclosing_page_url(start: AXUIElementRef) -> Option<String> {
    use crate::ax::bindings::AXUIElementCopyAttributeValue;
    use core_foundation::string::CFString;
    let parent_attr = CFString::new("AXParent");
    let mut current = start;
    let mut owned = false;
    let mut url = None;
    for _ in 0..40 {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWebArea") => {
                url = Some(copy_url_attr(current).unwrap_or_default());
                break;
            }
            Some("AXWindow") | Some("AXApplication") | None => break,
            _ => {}
        }
        let mut parent: CFTypeRef = std::ptr::null_mut();
        let error = AXUIElementCopyAttributeValue(
            current,
            parent_attr.as_concrete_TypeRef(),
            &mut parent,
        );
        if owned {
            CFRelease(current as CFTypeRef);
        }
        if error != crate::ax::bindings::kAXErrorSuccess || parent.is_null() {
            return url;
        }
        current = parent as AXUIElementRef;
        owned = true;
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    url
}

/// What a native input would act on inside a page: the element's AX role
/// and the URL of the page around it.
struct PageTarget {
    role: String,
    url: String,
}

/// # Safety
///
/// `element` must be a live AX element reference; it is borrowed.
unsafe fn page_target(element: AXUIElementRef) -> Option<PageTarget> {
    let url = enclosing_page_url(element)?;
    Some(PageTarget {
        role: copy_string_attr(element, "AXRole").unwrap_or_default(),
        url,
    })
}

/// The addressed element, or the exact window's focused element.
fn page_target_of(pid: i32, window_id: u32, element: Option<usize>) -> Option<PageTarget> {
    unsafe {
        match element {
            Some(ptr) => page_target(ptr as AXUIElementRef),
            None => {
                let focused = crate::ax::exact_target::focused_element_in_window(pid, window_id)?;
                let target = page_target(focused);
                CFRelease(focused as CFTypeRef);
                target
            }
        }
    }
}

/// The browser call that replaces a refused native one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NextCall {
    pub tool: &'static str,
    /// Arguments beyond target_id, tab_id, and ref, as a JSON object literal.
    pub arguments: &'static str,
}

/// Typing into a text control: browser_type appends at the caret, which it
/// accepts for inputs, textareas, and contenteditable alike.
pub(super) const TYPE_NEXT: NextCall = NextCall {
    tool: "browser_type",
    arguments: "{}",
};

/// Setting a text control's value: browser_type with replace:true. Its
/// set_value mode accepts only inputs and textareas, and AX cannot tell a
/// textarea from a contenteditable reliably, so the replace form, which
/// every text control accepts, is the one recommended.
pub(super) const SET_VALUE_NEXT: NextCall = NextCall {
    tool: "browser_type",
    arguments: r#"{"replace": true}"#,
};

/// Where a native click gesture goes. Gesture rule: only the gestures a
/// browser tool sends exactly are redirected: an unmodified single left
/// press (browser_click), an unmodified double left press and an unmodified
/// single right press (browser_pointer). Middle clicks, modifier clicks,
/// triple clicks, and AX actions other than press (show_menu, pick,
/// confirm, cancel, open, focus) keep native delivery.
pub(super) fn click_next(button: &str, count: usize, modified: bool, action: &str) -> Option<NextCall> {
    if modified || action != "press" {
        return None;
    }
    match (button, count) {
        ("left", 1) => Some(NextCall {
            tool: "browser_click",
            arguments: "{}",
        }),
        ("left", 2) => Some(NextCall {
            tool: "browser_pointer",
            arguments: r#"{"action": "double_click"}"#,
        }),
        ("right", 1) => Some(NextCall {
            tool: "browser_pointer",
            arguments: r#"{"action": "right_click"}"#,
        }),
        _ => None,
    }
}

/// Which controls a browser tool can take over from a native tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Control {
    /// browser_click: any element or point.
    Any,
    /// browser_type (text modes and set_value): text inputs, textareas, and
    /// contenteditable. Not selects (AXPopUpButton), range sliders, checkboxes.
    Text,
}

impl Control {
    fn supports(self, role: &str) -> bool {
        match self {
            Control::Any => true,
            Control::Text => matches!(
                role,
                "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
            ),
        }
    }
}

/// The same page, ignoring the fragment and a trailing slash.
fn same_page(a: &str, b: &str) -> bool {
    let strip = |url: &str| {
        let url = url.split('#').next().unwrap_or(url);
        url.trim_end_matches('/').to_owned()
    };
    !a.is_empty() && strip(a) == strip(b)
}

/// Whether an extension link's windows and tabs show the native window
/// (matched by bounds, within a few points) with `url` in its active tab.
fn link_shows(
    bounds: (f64, f64, f64, f64),
    windows: &serde_json::Value,
    tabs: &serde_json::Value,
    url: &str,
) -> bool {
    const TOLERANCE: f64 = 8.0;
    let (x, y, width, height) = bounds;
    let close = |value: &serde_json::Value, want: f64| {
        value.as_f64().is_some_and(|got| (got - want).abs() <= TOLERANCE)
    };
    let matching_windows: Vec<i64> = windows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|window| {
            close(&window["left"], x)
                && close(&window["top"], y)
                && close(&window["width"], width)
                && close(&window["height"], height)
        })
        .filter_map(|window| window["windowId"].as_i64())
        .collect();
    tabs.as_array().into_iter().flatten().any(|tab| {
        tab["active"].as_bool() == Some(true)
            && tab["windowId"]
                .as_i64()
                .is_some_and(|id| matching_windows.contains(&id))
            && tab["url"]
                .as_str()
                .is_some_and(|tab_url| same_page(tab_url, url))
    })
}

/// Whether a connected extension link of `pid` shows `window_id` on `url`.
async fn extension_shows_window(pid: i32, window_id: u32, url: &str) -> bool {
    let Some(bounds) = crate::windows::window_bounds_by_id(window_id) else {
        return false;
    };
    let bridge = cua_driver_core::browser::extension_bridge::global();
    let links: Vec<u64> = bridge
        .links()
        .into_iter()
        .filter(|link| link.chrome_pid == Some(i64::from(pid)))
        .map(|link| link.link)
        .collect();
    for link in links {
        let windows = bridge
            .request_on(link, "windows.list", serde_json::json!({}))
            .await;
        let tabs = bridge
            .request_on(link, "tabs.list", serde_json::json!({}))
            .await;
        let (Ok(windows), Ok(tabs)) = (windows, tabs) else {
            continue;
        };
        if link_shows(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            &windows,
            &tabs,
            url,
        ) {
            return true;
        }
    }
    false
}

/// Apply the redirect rule to a resolved page target.
async fn redirect_for(
    tool: &'static str,
    next: NextCall,
    control: Control,
    pid: i32,
    window_id: u32,
    target: PageTarget,
) -> Option<ToolResult> {
    (control.supports(&target.role)
        && extension_can_reach(&target.url)
        && extension_shows_window(pid, window_id, &target.url).await)
        .then(|| redirect_result(tool, next, pid, window_id))
}

/// Refuse native input to a page target under the redirect rule.
/// `element` is the addressed element, `None` for the window's focused element.
pub(super) async fn page_input_redirect(
    tool: &'static str,
    next: NextCall,
    control: Control,
    pid: i32,
    window_id: Option<u32>,
    element: Option<usize>,
) -> Option<ToolResult> {
    let window_id = window_id?;
    if !extension_connected(pid) {
        return None;
    }
    let target = tokio::task::spawn_blocking(move || page_target_of(pid, window_id, element))
        .await
        .ok()
        .flatten()?;
    redirect_for(tool, next, control, pid, window_id, target).await
}

/// [`page_input_redirect`] for a pixel target in the window's screenshot
/// coordinates: hit-tests the point.
pub(super) async fn page_input_redirect_at_pixel(
    tool: &'static str,
    next: NextCall,
    control: Control,
    pid: i32,
    window_id: Option<u32>,
    x: f64,
    y: f64,
) -> Option<ToolResult> {
    let window_id = window_id?;
    if !extension_connected(pid) {
        return None;
    }
    let target = tokio::task::spawn_blocking(move || {
        let frame = super::px_frame::resolve_window_px_frame(window_id).ok()?;
        let (screen_x, screen_y, _, _) = frame.to_screen(x, y);
        unsafe {
            let hit = crate::ax::bindings::element_at_screen_position(pid, screen_x, screen_y)?;
            let target = page_target(hit);
            CFRelease(hit as CFTypeRef);
            target
        }
    })
    .await
    .ok()
    .flatten()?;
    redirect_for(tool, next, control, pid, window_id, target).await
}

/// The refusal for native `tool` input on a page the extension reaches.
fn redirect_result(tool: &str, next: NextCall, pid: i32, window_id: u32) -> ToolResult {
    let arguments: serde_json::Value =
        serde_json::from_str(next.arguments).unwrap_or_else(|_| serde_json::json!({}));
    let with = if next.arguments == "{}" {
        String::new()
    } else {
        format!(" with {}", next.arguments)
    };
    ToolResult::error(format!(
        "{tool} refused before sending input: the target is a web page in Chrome and cua's \
         Chrome extension is connected there, so page input goes through the browser tools, \
         which the page's own scripts (React and similar) observe and which read the result \
         back. Call get_browser_state {{\"pid\": {pid}, \"window_id\": {window_id}}} to bind, \
         get_browser_state with the target_id and tab_id it returns to get refs, then {}{with} \
         and the element's ref. Chrome's own UI (address bar, dialogs, extension UI) still \
         takes native input.",
        next.tool
    ))
    .with_structured(serde_json::json!({
        "code": "browser_route_required",
        "effect": "refused",
        "next_calls": [
            { "tool": "get_browser_state", "arguments": { "pid": pid, "window_id": window_id } },
            { "tool": next.tool, "arguments": arguments },
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pages_an_extension_can_debug_are_redirected() {
        for url in [
            "https://claude.ai/chat/1",
            "http://127.0.0.1:8765/react-form/",
            "about:blank",
            "file:///tmp/a.html",
            "data:text/html,<p>x",
        ] {
            assert!(extension_can_reach(url), "{url}");
        }
        for url in [
            "",
            "chrome://extensions/",
            "Chrome://settings",
            "chrome-extension://abc/popup.html",
            "devtools://devtools/bundled/inspector.html",
            "https://chromewebstore.google.com/detail/x",
        ] {
            assert!(!extension_can_reach(url), "{url}");
        }
    }

    #[test]
    fn the_refusal_names_the_calls_to_make() {
        let result = redirect_result("type_text", TYPE_NEXT, 42, 7);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["code"], "browser_route_required");
        assert_eq!(
            structured["next_calls"][0],
            serde_json::json!({
                "tool": "get_browser_state",
                "arguments": { "pid": 42, "window_id": 7 }
            })
        );
        assert_eq!(structured["next_calls"][1]["tool"], "browser_type");
    }

    #[test]
    fn only_controls_the_browser_tools_support_are_redirected() {
        for role in ["AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"] {
            assert!(Control::Text.supports(role), "{role}");
        }
        // Selects and range sliders keep set_value's native option and
        // numeric paths; browser_type refuses them.
        for role in ["AXPopUpButton", "AXSlider", "AXCheckBox", "AXButton", "AXWebArea"] {
            assert!(!Control::Text.supports(role), "{role}");
        }
        assert!(Control::Any.supports("AXPopUpButton"));
    }

    #[test]
    fn only_a_window_the_extension_shows_is_redirected() {
        let bounds = (0.0, 30.0, 1100.0, 794.0);
        let url = "http://127.0.0.1:8765/react-form/";
        let windows = serde_json::json!([
            { "windowId": 5, "left": 0, "top": 30, "width": 1100, "height": 794 },
            { "windowId": 6, "left": 400, "top": 100, "width": 800, "height": 600 },
        ]);
        let tabs = |window: i64, active: bool, url: &str| {
            serde_json::json!([{ "tabId": 1, "windowId": window, "active": active, "url": url }])
        };
        assert!(link_shows(bounds, &windows, &tabs(5, true, url), url));
        assert!(link_shows(
            bounds,
            &windows,
            &tabs(5, true, "http://127.0.0.1:8765/react-form#x"),
            url
        ));
        // The window is not on the link: an incognito window without
        // extension access, or another profile's window, in the same process.
        let other_only = serde_json::json!([windows[1].clone()]);
        assert!(!link_shows(bounds, &other_only, &tabs(6, true, url), url));
        // Right window, but its active tab shows another page, or the page
        // is only in a background tab.
        assert!(!link_shows(bounds, &windows, &tabs(5, true, "https://example.com/"), url));
        assert!(!link_shows(bounds, &windows, &tabs(5, false, url), url));
        // The page in a different window of the link does not count.
        assert!(!link_shows(bounds, &windows, &tabs(6, true, url), url));
        assert!(!same_page("", ""));
    }

    #[test]
    fn every_click_gesture_goes_to_the_tool_that_sends_it_or_stays_native() {
        let redirected = [
            (("left", 1), ("browser_click", serde_json::json!({}))),
            (("left", 2), ("browser_pointer", serde_json::json!({ "action": "double_click" }))),
            (("right", 1), ("browser_pointer", serde_json::json!({ "action": "right_click" }))),
        ];
        let actions = ["press", "show_menu", "pick", "confirm", "cancel", "open", "focus"];
        for button in ["left", "right", "middle"] {
            for count in 1..=3 {
                for modified in [false, true] {
                    for action in actions {
                        let got = click_next(button, count, modified, action);
                        let want = redirected
                            .iter()
                            .find(|(gesture, _)| *gesture == (button, count))
                            .filter(|_| !modified && action == "press")
                            .map(|(_, next)| next);
                        match (got, want) {
                            (None, None) => {}
                            (Some(next), Some((tool, arguments))) => {
                                assert_eq!(next.tool, *tool);
                                let parsed: serde_json::Value =
                                    serde_json::from_str(next.arguments).unwrap();
                                assert_eq!(&parsed, arguments);
                            }
                            (got, want) => panic!(
                                "{button} x{count} modified={modified} {action}: got {got:?}, want {want:?}"
                            ),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn each_text_control_is_sent_to_a_browser_type_mode_it_accepts() {
        // browser_type accepts insert_text (the default mode, with or without
        // replace) for inputs, textareas, and contenteditable; its set_value
        // mode only for inputs and textareas.
        let accepts = |arguments: &serde_json::Value, control: &str| {
            match arguments.get("mode").and_then(serde_json::Value::as_str).unwrap_or("insert_text") {
                "insert_text" | "keystrokes" => true,
                "set_value" => control != "contenteditable",
                _ => false,
            }
        };
        for next in [TYPE_NEXT, SET_VALUE_NEXT] {
            assert_eq!(next.tool, "browser_type");
            let arguments: serde_json::Value = serde_json::from_str(next.arguments).unwrap();
            for control in ["input", "textarea", "contenteditable", "combobox input", "search input"] {
                assert!(accepts(&arguments, control), "{} for {control}", next.arguments);
            }
        }
        let set: serde_json::Value = serde_json::from_str(SET_VALUE_NEXT.arguments).unwrap();
        assert_eq!(set["replace"], true, "setting a value replaces it");
    }
}
