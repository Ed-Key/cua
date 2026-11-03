//! Transport selection for window-addressed pixel clicks (`click`,
//! `double_click`, `right_click`) on macOS.
//!
//! On macOS, plain foreground left clicks use pid delivery without moving the
//! hardware pointer, as native Computer Use does. Pointer-reading toolkits,
//! modified clicks, other buttons, desktop scope and drags keep HID delivery
//! with a pointer warp. Windows and X11 retain their existing foreground input
//! routes. Background delivery stays PID-routed and never moves the pointer;
//! a detectable pointer-reading toolkit refuses with `background_unavailable`.

use cua_driver_core::protocol::ToolResult;

use crate::input::pointer_toolkit::PointerReadingToolkit;

/// Transport for a pixel click once any background AX hit-test backend has
/// declined (or was not eligible).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PixelClickRoute {
    /// Activate the exact window, then dispatch via `foreground_click` (pid
    /// for plain left clicks, HID for pointer-reading toolkits or other clicks).
    /// The right_click tool keeps its HID route.
    Foreground,
    /// Background delivery routed to the target pid. The hardware pointer is
    /// never moved, so the result stays "not driver-verified".
    BackgroundPid,
    /// Background delivery refused: the target's toolkit reads the hardware
    /// pointer, so a PID-routed event would land wherever the pointer is.
    RefuseBackground(PointerReadingToolkit),
}

/// Pure route decision. `effective_foreground` is the requested foreground
/// rung; it requires an exact window for activation and the occlusion check.
/// The click primitive then selects the foreground transport.
pub(crate) fn pixel_click_route(
    effective_foreground: bool,
    has_window: bool,
    toolkit: Option<PointerReadingToolkit>,
) -> PixelClickRoute {
    if effective_foreground && has_window {
        return PixelClickRoute::Foreground;
    }
    match toolkit {
        Some(toolkit) if has_window => PixelClickRoute::RefuseBackground(toolkit),
        _ => PixelClickRoute::BackgroundPid,
    }
}

/// Resolve the route for a live target, probing the toolkit only for
/// window-addressed background clicks. Returns the structured refusal when
/// background delivery cannot reach the target.
pub(crate) async fn resolve(
    pid: i32,
    effective_foreground: bool,
    window_id: Option<u32>,
    event_kind: &str,
) -> Result<PixelClickRoute, ToolResult> {
    let has_window = window_id.is_some();
    let toolkit = if !effective_foreground && has_window {
        tokio::task::spawn_blocking(move || crate::input::pointer_toolkit::detect(pid))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    match pixel_click_route(effective_foreground, has_window, toolkit) {
        PixelClickRoute::RefuseBackground(toolkit) => {
            Err(pointer_reading_background_refusal(pid, toolkit, event_kind))
        }
        route => Ok(route),
    }
}

pub(crate) fn pointer_reading_background_refusal(
    pid: i32,
    toolkit: PointerReadingToolkit,
    event_kind: &str,
) -> ToolResult {
    let name = toolkit.name();
    cua_driver_core::delivery::background_unavailable_result(
        format!(
            "Background pixel click is not available for pid {pid}: its {name} toolkit \
             derives click locations from the hardware pointer, and background delivery \
             never moves the pointer, so the click would land wherever the pointer \
             currently is. Retry this action with delivery_mode:\"foreground\"; Cua \
             Driver will activate the window, move the pointer to the target, and click."
        ),
        "background_unavailable",
        "the target toolkit reads the hardware pointer position, which background \
         PID-routed events cannot move; retry with delivery_mode:\"foreground\".",
        serde_json::json!({
            "event_kind": event_kind,
            "reason": "pointer_reading_toolkit",
            "toolkit": name,
        }),
    )
}

/// Human-readable delivery note appended to a successful pixel click result.
pub(crate) fn delivery_note(route: PixelClickRoute) -> &'static str {
    match route {
        PixelClickRoute::Foreground => {
            "foreground: exact window activated and confirmed topmost at the point, \
             hardware pointer moved to the target, HID event tap; the click's effect is \
             not driver-verified — confirm via screenshot"
        }
        _ => {
            "background CGEvent routed to the pid; the hardware pointer was not moved, \
             so apps that read the pointer position may not receive it at the target; \
             not driver-verified: read the window again to confirm; if it did not land, \
             click the element by element_token (accessibility, also from behind)"
        }
    }
}

/// Legacy `path` label consumed by the action-truth normalizer:
/// `cgevent_fg` maps to the HID transport with foreground delivery and
/// `cgevent` to the PID transport with background delivery.
pub(crate) fn path_label(route: PixelClickRoute) -> &'static str {
    match route {
        PixelClickRoute::Foreground => "cgevent_fg",
        _ => "cgevent",
    }
}

/// Whether a foreground pointer failure is one of the refusals
/// [`foreground_unavailable`] names (sent no input, with its own code).
pub(crate) fn is_pointer_refusal(cause: &anyhow::Error) -> bool {
    cause.is::<crate::input::skylight::TargetOccluded>() || cause.is::<crate::pip::PipHeldByUser>()
}

/// The delivery note and legacy `path` label for a click sent on `route`;
/// `sent` is how the foreground rung sent it (None: not that rung).
pub(crate) fn sent_note(
    route: PixelClickRoute,
    sent: Option<crate::input::mouse::ForegroundClick>,
) -> (String, &'static str) {
    use crate::input::mouse::{ForegroundClick, HidPassage};
    match sent {
        Some(ForegroundClick::Hid(HidPassage::Passed)) => (
            "foreground: exact window activated and confirmed topmost at the point, hardware \
             pointer moved to the target, HID event tap; the system passed the click on past \
             every event tap before the previous app was restored; the click's effect is not \
             driver-verified — confirm via screenshot"
                .into(),
            path_label(route),
        ),
        Some(ForegroundClick::Pid { tap_owner }) => {
            let tap_note = tap_owner
                .map(|owner| {
                    format!(
                        "; bypassed the active click event tap held by pid {owner} ({})",
                        crate::apps::get_app_name_for_pid(owner)
                            .unwrap_or_else(|| "unknown app".into())
                    )
                })
                .unwrap_or_default();
            (
                format!(
                    "foreground: exact window activated and confirmed topmost at the point, \
                     posted to the pid with window-local coordinates; the hardware pointer was \
                     not moved{tap_note}; the click's effect is not driver-verified: confirm \
                     via screenshot"
                ),
                "cgevent_pid_fg",
            )
        }
        Some(ForegroundClick::Hid(_)) | None => (delivery_note(route).into(), path_label(route)),
    }
}

/// A foreground click whose HID events an event tap dropped or still holds.
/// Not an error: the click may still arrive (on the target, which was left in
/// front), so the answer says what is known and asks for a read before any
/// repeat.
pub(crate) fn not_confirmed(action: &str, cause: &anyhow::Error) -> Option<ToolResult> {
    let held = cause.downcast_ref::<crate::input::mouse::ClickNotConfirmed>()?;
    Some(
        ToolResult::text(format!(
            "{action} not confirmed: {held}. The target app was left in front, so a late release \
             lands on it and not on another app. Read the window before repeating the click."
        ))
        .with_structured(serde_json::json!({
            "path": "cgevent_fg",
            "verified": false,
            "effect": "unverifiable",
        })),
    )
}

/// Structured error when foreground activation or dispatch failed. A window
/// covering the point after the raise is named by `target_occluded`.
pub(crate) fn foreground_unavailable(
    action: &str,
    window_id: u32,
    cause: &anyhow::Error,
) -> ToolResult {
    if let Some(held) = cause.downcast_ref::<crate::pip::PipHeldByUser>() {
        return ToolResult::error(format!("{action} refused: {held}.")).with_structured(
            serde_json::json!({
                "code": "pip_held_by_user",
                "effect": "refused",
                "window_id": window_id,
                "point": [held.point.0, held.point.1],
                "waited_ms": held.waited.as_millis() as u64,
                "retryable": true,
            }),
        );
    }
    if let Some(occluded) = cause.downcast_ref::<crate::input::skylight::TargetOccluded>() {
        return ToolResult::error(format!("{action} refused: {occluded}.")).with_structured(
            serde_json::json!({
                "code": "target_occluded",
                "effect": "refused",
                "window_id": window_id,
                "point": [occluded.point.0, occluded.point.1],
                "covering": occluded.covering.as_ref().map(|window| serde_json::json!({
                    "pid": window.pid,
                    "app_name": window.app_name,
                    "window_id": window.window_id,
                    "title": window.title,
                })),
                "decided_by": if occluded.geometry_only { "window_list_geometry" } else { "window_server_hit_test" },
            }),
        );
    }
    ToolResult::error(format!(
        "{action} failed: foreground delivery to window {window_id} was not possible: \
         {cause}"
    ))
    .with_structured(serde_json::json!({ "code": "foreground_unavailable" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Foreground delivery needs an exact window; background delivery refuses
    /// a known pointer-reading toolkit before dispatch.
    #[test]
    fn pixel_click_route_requires_a_foreground_window_and_refuses_tk_background() {
        let tk = Some(PointerReadingToolkit::Tk);
        assert_eq!(
            pixel_click_route(true, true, None),
            PixelClickRoute::Foreground
        );
        assert_eq!(
            pixel_click_route(true, true, tk),
            PixelClickRoute::Foreground,
            "foreground delivery lets the click primitive use HID for Tk"
        );
        assert_eq!(
            pixel_click_route(false, true, None),
            PixelClickRoute::BackgroundPid
        );
        assert_eq!(
            pixel_click_route(false, true, tk),
            PixelClickRoute::RefuseBackground(PointerReadingToolkit::Tk)
        );
        assert_eq!(
            pixel_click_route(false, false, tk),
            PixelClickRoute::BackgroundPid,
            "legacy window-less pid clicks keep their established route"
        );
        assert_eq!(
            pixel_click_route(true, false, None),
            PixelClickRoute::BackgroundPid,
            "foreground delivery requires an exact window"
        );
    }

    #[test]
    fn route_labels_say_what_actually_happened() {
        assert_eq!(path_label(PixelClickRoute::Foreground), "cgevent_fg");
        assert_eq!(path_label(PixelClickRoute::BackgroundPid), "cgevent");
        assert!(delivery_note(PixelClickRoute::Foreground).contains("hardware pointer moved"));
        assert!(delivery_note(PixelClickRoute::BackgroundPid).contains("pointer was not moved"));
    }

    #[test]
    fn foreground_pid_results_report_no_pointer_move_and_the_optional_tap_owner() {
        use crate::input::mouse::ForegroundClick;
        let (note, path) = sent_note(
            PixelClickRoute::Foreground,
            Some(ForegroundClick::Pid { tap_owner: None }),
        );
        assert_eq!(path, "cgevent_pid_fg");
        assert!(note.contains("posted to the pid with window-local coordinates"));
        assert!(note.contains("hardware pointer was not moved"));
        assert!(!note.contains("active click event tap"));

        // No process can have this pid on macOS, so the app name is deterministic.
        let (note, path) = sent_note(
            PixelClickRoute::Foreground,
            Some(ForegroundClick::Pid {
                tap_owner: Some(i32::MAX),
            }),
        );
        assert_eq!(path, "cgevent_pid_fg");
        assert!(note.contains("hardware pointer was not moved"));
        assert!(note.contains("active click event tap held by pid 2147483647 (unknown app)"));
    }

    #[test]
    fn foreground_hid_results_keep_the_hid_path_and_passage_note() {
        use crate::input::mouse::{ForegroundClick, HidPassage};
        let (note, path) = sent_note(
            PixelClickRoute::Foreground,
            Some(ForegroundClick::Hid(HidPassage::Passed)),
        );
        assert_eq!(path, "cgevent_fg");
        assert!(note.contains("hardware pointer moved"));
        assert!(note.contains("past every event tap"));
    }

    #[test]
    fn pointer_reading_refusal_uses_the_shared_background_unavailable_contract() {
        let result =
            pointer_reading_background_refusal(42, PointerReadingToolkit::Tk, "mouse_click");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "background_unavailable");
        assert_eq!(structured["reason"], "pointer_reading_toolkit");
        assert_eq!(structured["toolkit"], "tk");
        assert_eq!(structured["event_kind"], "mouse_click");
        assert_eq!(structured["escalation"]["recommended"], "foreground");
    }

    /// An agent gesture refused because the user holds the preview says so,
    /// with its own code, and is recognised as a refusal.
    #[test]
    fn a_preview_held_by_the_user_refuses_as_pip_held_by_user() {
        let held = anyhow::Error::new(crate::pip::PipHeldByUser {
            point: (1200.0, 700.0),
            waited: std::time::Duration::from_secs(5),
        });
        assert!(is_pointer_refusal(&held));
        let result = foreground_unavailable("click", 7, &held);
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.clone().expect("structured");
        assert_eq!(structured["code"], "pip_held_by_user");
        assert_eq!(structured["effect"], "refused");
        assert!(format!("{:?}", result.content).contains("holding"));
        assert!(!is_pointer_refusal(&anyhow::anyhow!("window never focused")));
    }

    #[test]
    fn foreground_failure_is_structured() {
        let result = foreground_unavailable("click", 7, &anyhow::anyhow!("window never focused"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.expect("structured")["code"],
            "foreground_unavailable"
        );
    }

    /// A click that would land on the window covering the target refuses and
    /// names that window.
    #[test]
    fn a_covered_target_refuses_as_target_occluded_naming_the_cover() {
        let occluded = crate::input::skylight::TargetOccluded {
            window_id: 7,
            point: (429.0, 470.0),
            covering: Some(crate::input::skylight::CoveringWindow {
                pid: 99,
                app_name: "Finder".into(),
                window_id: 12,
                title: "cover".into(),
            }),
            geometry_only: false,
        };
        let result = foreground_unavailable("click", 7, &anyhow::Error::new(occluded));
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.clone().expect("structured");
        assert_eq!(structured["code"], "target_occluded");
        assert_eq!(structured["covering"]["app_name"], "Finder");
        assert!(format!("{:?}", result.content).contains("Finder"));
    }
}
