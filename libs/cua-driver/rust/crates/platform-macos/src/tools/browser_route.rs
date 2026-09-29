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

/// Which element a native input would act on: the addressed element, or the
/// exact window's focused element.
fn page_url_of_target(pid: i32, window_id: u32, element: Option<usize>) -> Option<String> {
    unsafe {
        match element {
            Some(ptr) => enclosing_page_url(ptr as AXUIElementRef),
            None => {
                let focused = crate::ax::exact_target::focused_element_in_window(pid, window_id)?;
                let url = enclosing_page_url(focused);
                CFRelease(focused as CFTypeRef);
                url
            }
        }
    }
}

/// The refusal for native `tool` input on a page the extension reaches.
fn redirect_result(tool: &str, next: &str, pid: i32, window_id: u32) -> ToolResult {
    ToolResult::error(format!(
        "{tool} refused before sending input: the target is a web page in Chrome and cua's \
         Chrome extension is connected there, so page input goes through the browser tools, \
         which the page's own scripts (React and similar) observe and which read the result \
         back. Call get_browser_state {{\"pid\": {pid}, \"window_id\": {window_id}}} to bind, \
         get_browser_state with the target_id and tab_id it returns to get refs, then {next} \
         with the field's ref. Chrome's own UI (address bar, dialogs, extension UI) still takes \
         native input."
    ))
    .with_structured(serde_json::json!({
        "code": "browser_route_required",
        "effect": "refused",
        "next_calls": [
            { "tool": "get_browser_state", "arguments": { "pid": pid, "window_id": window_id } },
            { "tool": next },
        ],
    }))
}

/// Refuse native input to a page target when the extension is connected to
/// its Chrome. `element` is the addressed element, `None` for the window's
/// focused element.
pub(super) async fn page_input_redirect(
    tool: &'static str,
    next: &'static str,
    pid: i32,
    window_id: Option<u32>,
    element: Option<usize>,
) -> Option<ToolResult> {
    let window_id = window_id?;
    if !extension_connected(pid) {
        return None;
    }
    let url = tokio::task::spawn_blocking(move || page_url_of_target(pid, window_id, element))
        .await
        .ok()
        .flatten()?;
    extension_can_reach(&url).then(|| redirect_result(tool, next, pid, window_id))
}

/// [`page_input_redirect`] for a pixel target in the window's screenshot
/// coordinates: hit-tests the point.
pub(super) async fn page_input_redirect_at_pixel(
    tool: &'static str,
    next: &'static str,
    pid: i32,
    window_id: Option<u32>,
    x: f64,
    y: f64,
) -> Option<ToolResult> {
    let window_id = window_id?;
    if !extension_connected(pid) {
        return None;
    }
    let url = tokio::task::spawn_blocking(move || {
        let frame = super::px_frame::resolve_window_px_frame(window_id).ok()?;
        let (screen_x, screen_y, _, _) = frame.to_screen(x, y);
        unsafe {
            let hit = crate::ax::bindings::element_at_screen_position(pid, screen_x, screen_y)?;
            let url = enclosing_page_url(hit);
            CFRelease(hit as CFTypeRef);
            url
        }
    })
    .await
    .ok()
    .flatten()?;
    extension_can_reach(&url).then(|| redirect_result(tool, next, pid, window_id))
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
        let result = redirect_result("type_text", "browser_type", 42, 7);
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
}
