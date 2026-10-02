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
    /// cancel it.
    KeptForInlineEdit,
}

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;
const MAIN_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a menu command gets to open an inline editor (File > Rename)
/// before the previous front app is brought back.
const INLINE_EDIT_WAIT: Duration = Duration::from_millis(1200);

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

/// A menu title as paths compare it: trimmed, with "..." read as the
/// ellipsis character macOS menus use ("Save As..." finds "Save As…").
fn menu_title_key(title: &str) -> String {
    title.trim().replace("...", "…")
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

/// Close the menu a failed path opened from the menu bar item `top`:
/// AXCancel on its menu (which ends its submenus too), then a second press
/// of the item (menu bar items toggle), each read back from WindowServer's
/// window list.
unsafe fn close_opened_menu(app: AXUIElementRef, pid: i32, top: &str) -> Option<bool> {
    if crate::windows::open_menu_windows(pid)? == 0 {
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
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            let _ = perform_action(child, "AXCancel");
        }
        CFRelease(child as CFTypeRef);
    }
    let mut closed = crate::windows::wait_for_no_menu(pid);
    if closed == Some(false) {
        let _ = perform_action(item, "AXPress");
        closed = crate::windows::wait_for_no_menu(pid);
    }
    CFRelease(item as CFTypeRef);
    closed
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
    // in menu tracking (where later menu commands and keys do nothing).
    let result = result.map_err(|error| {
        if !pressed {
            return error;
        }
        match close_opened_menu(app, pid, &path[0]) {
            Some(true) => format!("{error}. No menu of the app is left open."),
            _ => format!(
                "{error}. The {} menu this call opened may still be open: press escape on the window before other input.",
                path[0]
            ),
        }
    });

    CFRelease(app as CFTypeRef);
    result
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
        && crate::apps::frontmost_pid() != Some(pid)
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
            crate::apps::frontmost_pid(),
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

        let outcome = tokio::task::spawn_blocking(move || {
            let prior_frontmost = crate::apps::frontmost_pid();
            let prior_frontmost_window =
                prior_frontmost.and_then(crate::ax::bindings::focused_window_id_of_pid);

            let result = focus_exact_window(pid, window_id)
                .and_then(|()| unsafe { invoke_path(pid, &path) });

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
                && crate::ax::bindings::await_inline_edit_after_menu(pid, window_id, INLINE_EDIT_WAIT);
            if kept {
                front_restored = Some(FrontAfter::KeptForInlineEdit);
            }
            if let Some(prior_pid) = other_front.filter(|_| !kept) {
                let restored_exact = prior_frontmost_window.is_some_and(|prior_window_id| {
                    focus_exact_window(prior_pid, prior_window_id).is_ok()
                });
                if !restored_exact {
                    let _ = crate::apps::restore_prior_app(prior_pid);
                }
                // Fallback activation is asynchronous and NSWorkspace's frontmost
                // app can lag it, so confirm within a short bound.
                let deadline = std::time::Instant::now() + Duration::from_millis(500);
                let mut restored = crate::apps::frontmost_pid() == Some(prior_pid);
                while !restored && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                    restored = crate::apps::frontmost_pid() == Some(prior_pid);
                }
                front_restored = Some(if restored { FrontAfter::Restored } else { FrontAfter::NotConfirmed });
            }
            result.map(|()| front_restored)
        })
        .await;

        match outcome {
            Ok(Ok(front_restored)) => ToolResult::text(format!(
                "Pressed the menu item (every step of the path resolved uniquely); what the command did is in the Outcome line.{}",
                match front_restored {
                    Some(FrontAfter::Restored) => " The target app was active only for the menu action; the previous front app is front again.",
                    Some(FrontAfter::NotConfirmed) => " The target app was activated for the menu action; the previous front app was not confirmed back in front.",
                    Some(FrontAfter::KeptForInlineEdit) => " The target app stays in front: the command opened an inline editor, which the app cancels when it loses the front, so the previous front app was not brought back.",
                    None => "",
                }
            ))
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosAxAction,
                    RequestedDelivery::Foreground,
                )
                .actual_delivery(ActualDelivery::Foreground)
                .evidence(ActionEvidence {
                    kind: EvidenceKind::NativeApiResult,
                    detail: "Every menu hop resolved uniquely and AX accepted the final action".into(),
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
    fn menu_titles_match_three_dots_and_the_ellipsis() {
        assert_eq!(menu_title_key(" Save As... "), menu_title_key("Save As…"));
        assert_eq!(menu_title_key("Save…"), "Save…");
        assert_ne!(menu_title_key("Save"), menu_title_key("Save…"));
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
}
