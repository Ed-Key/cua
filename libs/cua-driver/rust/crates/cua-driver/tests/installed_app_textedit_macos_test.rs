//! TextEdit background-delivery integration check for macOS.
//!
//! Covers the `{path, verified}` structured outcome on a real Cocoa app.
//!
//! The schema contract lives in `protocol_schema_test.rs`. This installed-app
//! check runs in the canonical logged-in macOS desktop lane.

#![cfg(target_os = "macos")]

// ── End-to-end ladder behavior (interactive; needs a GUI session) ────────────

/// On a NATIVE Cocoa field (TextEdit), `delivery_mode:"background"` lands via the
/// AX value-write and the driver confirms it with accessibility read-back. This is
/// the driver-verifiable happy path — no foreground needed, no screenshot needed.
#[test]
#[ignore]
fn background_type_on_native_cocoa_is_ax_verified() {
    use cua_driver_testkit::e2e::{
        execute_case, recording_evidence, CaseSpec, Delivery, DriverRoute, Observation, OracleKind,
        Scope, Targeting,
    };
    use cua_driver_testkit::observer::TargetWindow;
    use cua_driver_testkit::sentinel::run_with_background_oracles;
    use cua_driver_testkit::{Driver, McpDriver};

    let cell_id = "macos-textedit-type-text-ax-background";
    let case = CaseSpec::delivered(
        cell_id,
        "textedit",
        "appkit",
        "type_text",
        Targeting::Ax,
        Delivery::Background,
        Scope::Window,
        DriverRoute::MacosAxValue,
        vec![
            OracleKind::AxState,
            OracleKind::Focus,
            OracleKind::ZOrder,
            OracleKind::Cursor,
            OracleKind::NoLeakedInput,
        ],
    );
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());

        // Launch TextEdit and open a blank document.
        let launch = driver.call(
            "launch_app",
            serde_json::json!({ "bundle_id": "com.apple.TextEdit" }),
        );
        assert!(
            !launch.is_error(),
            "could not launch TextEdit: {}",
            launch.text()
        );
        let pid = launch.structured()["pid"].as_i64().expect("TextEdit pid");
        let mut windows = launch.structured()["windows"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(!windows.is_empty(), "TextEdit opened no window");

        // TextEdit can retain an off-screen restoration window alongside the
        // visible blank document. WindowServer does not guarantee their order,
        // so prefer visible windows and select the one that actually exposes
        // the editor instead of assuming `windows[0]` is the document.
        windows.sort_by_key(|window| !window["is_on_screen"].as_bool().unwrap_or(false));
        let (wid, el, snapshot_id) = windows
            .iter()
            .filter_map(|window| window["window_id"].as_u64())
            .find_map(|window_id| {
                let state = driver.call(
                    "get_window_state",
                    serde_json::json!({
                        "pid": pid,
                        "window_id": window_id,
                        "capture_mode": "ax"
                    }),
                );
                state.structured()["elements"]
                    .as_array()
                    .and_then(|elements| {
                        elements
                            .iter()
                            .find(|element| element["role"] == "AXTextArea")
                            .and_then(|element| element["element_index"].as_u64())
                    })
                    .map(|element_index| (window_id, element_index, state.snapshot_id().to_owned()))
            })
            .expect("TextEdit opened no window containing an AXTextArea");

        let (typed, mut passed) = run_with_background_oracles(
            &mut driver,
            TargetWindow {
                pid: pid as u32,
                native_id: wid,
            },
            |driver| {
                driver.call(
                    "type_text",
                    serde_json::json!({
                        "pid": pid, "window_id": wid, "element_index": el,
                        "snapshot_id": snapshot_id,
                        "text": "ladder", "delivery_mode": "background"
                    }),
                )
            },
        )
        .unwrap_or_else(|error| panic!("background TextEdit contract failed: {error}"));
        assert!(!typed.is_error(), "type_text errored: {}", typed.text());
        assert_eq!(
            typed.action_route(),
            Some("accessibility"),
            "native Cocoa field should land via AX: {}",
            typed.text()
        );
        assert_eq!(
            typed.action_effect(),
            Some("confirmed"),
            "AX write should be confirmed by read-back: {}",
            typed.text()
        );
        assert_eq!(
            typed.structured()["evidence"][0]["kind"],
            "value_readback",
            "confirmed actions must expose publishable evidence: {}",
            typed.text()
        );
        passed.push(OracleKind::AxState);
        Observation::delivered(passed, Default::default())
    });
}

/// A background shortcut that opens TextEdit's AppKit file panel reports one
/// owner-verified rebind target while preserving the actuator's effect and the
/// user's foreground application.
#[test]
#[ignore]
fn background_open_panel_returns_a_typed_rebind() {
    use cua_driver_testkit::e2e::{
        execute_case, recording_evidence, CaseSpec, Delivery, DriverRoute, Observation, OracleKind,
        Scope, Targeting,
    };
    use cua_driver_testkit::observer::TargetWindow;
    use cua_driver_testkit::sentinel::run_with_background_oracles;
    use cua_driver_testkit::{Driver, McpDriver};

    let cell_id = "macos-textedit-open-panel-background-rebind";
    let case = CaseSpec::delivered(
        cell_id,
        "textedit",
        "appkit",
        "hotkey",
        Targeting::Ax,
        Delivery::Background,
        Scope::Window,
        DriverRoute::MacosCgEventPid,
        vec![
            OracleKind::AxState,
            OracleKind::Focus,
            OracleKind::ZOrder,
            OracleKind::Cursor,
            OracleKind::NoLeakedInput,
        ],
    );
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());

        // Open a file owned by this case rather than depending on TextEdit's
        // first-launch document chooser or borrowing a user's Untitled window.
        let fixture = tempfile::tempdir().expect("TextEdit fixture directory");
        let document_name = format!(
            "cua-observer-{}.txt",
            fixture.path().file_name().unwrap().to_string_lossy()
        );
        let document = fixture.path().join(&document_name);
        let marker = "Cua observer document readiness marker";
        std::fs::write(&document, marker).expect("write TextEdit fixture");
        let before_apps = driver.call("list_apps", serde_json::json!({}));
        assert!(
            !before_apps.is_error(),
            "pre-launch application inventory: {}",
            before_apps.text()
        );
        let existing_pids: Vec<_> = before_apps.structured()["apps"]
            .as_array()
            .expect("pre-launch applications")
            .iter()
            .filter_map(|app| app["pid"].as_i64())
            .collect();
        let mut reaper = cua_driver_testkit::ChildReaper::new();
        let launch = driver.call(
            "launch_app",
            serde_json::json!({
                "bundle_id": "com.apple.TextEdit",
                "urls": [document.to_str().expect("fixture path")],
                "creates_new_application_instance": true,
                "additional_arguments": ["-ApplePersistenceIgnoreState", "YES"]
            }),
        );
        assert!(
            !launch.is_error(),
            "could not launch TextEdit: {}",
            launch.text()
        );
        let pid = launch.structured()["pid"].as_i64().expect("TextEdit pid");
        assert!(
            pid > 1 && !existing_pids.contains(&pid),
            "fixture must own a fresh process"
        );
        reaper.track_pid(pid.try_into().expect("owned TextEdit pid"));
        eprintln!(
            "[textedit-fixture] pid={pid} document={}",
            document.display()
        );
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let window_id = loop {
            let windows = driver.call("list_windows", serde_json::json!({"pid": pid}));
            assert!(
                !windows.is_error(),
                "TextEdit discovery: {}",
                windows.text()
            );
            let ready = windows.structured()["windows"]
                .as_array()
                .expect("TextEdit windows")
                .iter()
                .filter(|window| {
                    let title = window["title"].as_str().unwrap_or("");
                    title == document_name || title == document_name.trim_end_matches(".txt")
                })
                .filter_map(|window| window["window_id"].as_u64())
                .find(|window_id| {
                    let state = driver.call(
                        "get_window_state",
                        serde_json::json!({
                            "pid": pid, "window_id": window_id, "include_screenshot": false
                        }),
                    );
                    !state.is_error()
                        && state.structured()["elements"]
                            .as_array()
                            .is_some_and(|elements| {
                                elements.iter().any(|element| {
                                    element["role"] == "AXTextArea"
                                        && element["value"]
                                            .as_str()
                                            .is_some_and(|value| value.contains(marker))
                                })
                            })
                });
            if let Some(window_id) = ready {
                break window_id;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "owned TextEdit document never exposed its contents: {}",
                windows.text()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        let (opened, mut passed) = run_with_background_oracles(
            &mut driver,
            TargetWindow {
                pid: pid as u32,
                native_id: window_id,
            },
            |driver| {
                driver.call(
                    "hotkey",
                    serde_json::json!({
                        "pid": pid,
                        "window_id": window_id,
                        "keys": ["cmd", "o"],
                        "delivery_mode": "background"
                    }),
                )
            },
        )
        .unwrap_or_else(|error| panic!("background Open-panel contract failed: {error}"));
        eprintln!("[textedit-open-panel] {}", opened.raw);
        assert!(!opened.is_error(), "hotkey errored: {}", opened.text());
        assert_eq!(
            opened.action_effect(),
            Some("unverifiable"),
            "topology must not promote the hotkey effect: {}",
            opened.text()
        );

        let change = &opened.structured()["window_change"];
        let candidates = change["new_windows"]
            .as_array()
            .unwrap_or_else(|| panic!("missing typed window_change candidates: {}", opened.raw));
        assert_eq!(candidates.len(), 1, "expected one Open-panel root");
        let candidate = &candidates[0];
        let panel_pid = candidate["pid"].as_i64().expect("panel target pid");
        let panel_window_id = candidate["window_id"].as_u64().expect("panel window id");
        assert_eq!(opened.structured()["escalation"]["target"], "rebind");
        assert_eq!(
            opened.structured()["escalation"]["window"],
            *candidate,
            "exact rebind must be one observed owner-verified candidate"
        );

        let panel = driver.call(
            "get_window_state",
            serde_json::json!({
                "pid": panel_pid,
                "window_id": panel_window_id,
                "include_screenshot": false
            }),
        );
        assert!(
            !panel.is_error(),
            "rebound panel was not addressable: {}",
            panel.text()
        );
        let panel_elements = panel.structured()["elements"]
            .as_array()
            .expect("rebound panel elements");
        assert!(
            panel_elements.len() > 1,
            "rebound target did not expose the Open panel accessibility tree: {}",
            panel.text()
        );
        passed.push(OracleKind::AxState);

        // Resolve Cancel from a fresh snapshot. Escape may acknowledge delivery
        // without closing this background AppKit panel.
        let panel = driver.call(
            "get_window_state",
            serde_json::json!({
                "pid": panel_pid, "window_id": panel_window_id, "include_screenshot": false
            }),
        );
        assert!(
            !panel.is_error(),
            "panel cleanup snapshot: {}",
            panel.text()
        );
        let cancel_buttons: Vec<_> = panel.structured()["elements"]
            .as_array()
            .expect("panel cleanup elements")
            .iter()
            .filter(|element| {
                element["role"] == "AXButton"
                    && element["label"] == "Cancel"
                    && element["enabled"] == true
            })
            .collect();
        assert_eq!(
            cancel_buttons.len(),
            1,
            "expected one enabled panel Cancel button"
        );
        let cancel = driver.call(
            "click",
            serde_json::json!({
                "pid": panel_pid, "window_id": panel_window_id,
                "element_token": cancel_buttons[0]["element_token"].as_str().expect("Cancel token"),
                "delivery_mode": "background"
            }),
        );
        assert!(!cancel.is_error(), "panel Cancel: {}", cancel.text());
        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let windows = driver.call("list_windows", serde_json::json!({"pid": panel_pid}));
            assert!(
                !windows.is_error(),
                "panel cleanup read: {}",
                windows.text()
            );
            let still_visible = windows.structured()["windows"]
                .as_array()
                .expect("panel cleanup windows")
                .iter()
                .any(|window| {
                    window["window_id"].as_u64() == Some(panel_window_id)
                        && window["is_on_screen"] == true
                });
            if !still_visible {
                break;
            }
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "Open panel remained visible after cleanup: {}",
                windows.text()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let document_state = driver.call(
            "get_window_state",
            serde_json::json!({
                "pid": pid, "window_id": window_id, "include_screenshot": false
            }),
        );
        assert!(
            !document_state.is_error(),
            "owned document before close: {}",
            document_state.text()
        );
        // The public snapshot does not label native traffic-light buttons.
        // Read AXCloseButton only as a fixture oracle, then deliver the cleanup
        // click through Cua using its fresh token. No fixed screen coordinates.
        let close_frame = textedit_close_button_frame(pid as i32, window_id)
            .expect("owned document AXCloseButton frame");
        let close_buttons: Vec<_> = document_state.structured()["elements"]
            .as_array()
            .expect("document cleanup elements")
            .iter()
            .filter(|element| {
                element["role"] == "AXButton"
                    && element["enabled"] == true
                    && ["x", "y", "w", "h"]
                        .iter()
                        .zip(close_frame)
                        .all(|(axis, want)| {
                            element["frame"][axis]
                                .as_f64()
                                .is_some_and(|got| (got - want).abs() < 0.5)
                        })
            })
            .collect();
        assert_eq!(
            close_buttons.len(),
            1,
            "unique owned document close control"
        );
        let closed = driver.call(
            "click",
            serde_json::json!({
                "pid": pid, "window_id": window_id,
                "element_token": close_buttons[0]["element_token"].as_str().expect("close token"),
                "delivery_mode": "background"
            }),
        );
        assert!(
            !closed.is_error(),
            "close owned document: {}",
            closed.text()
        );
        let close_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let windows = driver.call("list_windows", serde_json::json!({"pid": pid}));
            assert!(
                !windows.is_error(),
                "document cleanup read: {}",
                windows.text()
            );
            let present = windows.structured()["windows"]
                .as_array()
                .expect("document cleanup windows")
                .iter()
                .any(|window| {
                    window["window_id"].as_u64() == Some(window_id)
                        && window["is_on_screen"] == true
                });
            if !present && !textedit_has_ax_window(pid as i32, window_id) {
                break;
            }
            assert!(
                std::time::Instant::now() < close_deadline,
                "owned TextEdit document remained after close: {}",
                windows.text()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            std::fs::read_to_string(&document).unwrap(),
            marker,
            "dialog test must not modify its document"
        );
        fixture.close().expect("remove TextEdit fixture directory");
        Observation::delivered(passed, Default::default())
    });
}

// Native metadata is fixture setup/cleanup only. The measured hotkey and both
// cleanup clicks still use the public daemon transport.
fn textedit_close_button_frame(pid: i32, window_id: u64) -> Option<[f64; 4]> {
    use core_foundation::base::{CFRelease, CFTypeRef};
    use platform_macos::ax::bindings::*;
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(app, 1.0);
        let windows = copy_ax_windows(app);
        let mut frame = None;
        for window in windows {
            AXUIElementSetMessagingTimeout(window, 1.0);
            if ax_get_window_id(window).map(u64::from) == Some(window_id) {
                if let Some(close) = copy_element_attr(window, "AXCloseButton") {
                    AXUIElementSetMessagingTimeout(close, 1.0);
                    frame = element_screen_rect(close);
                    CFRelease(close as CFTypeRef);
                }
            }
            CFRelease(window as CFTypeRef);
        }
        CFRelease(app as CFTypeRef);
        frame
    }
}

// WindowServer can retain a closed NSWindow's record. Require a successful AX
// window-list read as well as disappearance from the screen before cleanup passes.
fn textedit_has_ax_window(pid: i32, window_id: u64) -> bool {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use platform_macos::ax::bindings::*;
    unsafe {
        let app_ref = AXUIElementCreateApplication(pid);
        assert!(!app_ref.is_null(), "cleanup application AX object");
        let _app = CFType::wrap_under_create_rule(app_ref as CFTypeRef);
        assert_eq!(
            AXUIElementSetMessagingTimeout(app_ref, 1.0),
            kAXErrorSuccess
        );
        let attribute = CFString::new("AXWindows");
        let mut raw = std::ptr::null();
        let status =
            AXUIElementCopyAttributeValue(app_ref, attribute.as_concrete_TypeRef(), &mut raw);
        // Own even an unexpected non-null result before an assertion can unwind.
        let value = (!raw.is_null()).then(|| CFType::wrap_under_create_rule(raw));
        assert_eq!(status, kAXErrorSuccess, "cleanup AXWindows read failed");
        let value = value.expect("cleanup AXWindows value");
        assert_eq!(
            value.type_of(),
            CFArray::<CFType>::type_id(),
            "cleanup AXWindows type"
        );
        let windows = CFArray::<CFType>::wrap_under_get_rule(value.as_CFTypeRef() as CFArrayRef);
        windows.iter().any(|window| {
            assert_eq!(
                window.type_of(),
                AXUIElementGetTypeID(),
                "cleanup AXWindow type"
            );
            let window = window.as_CFTypeRef() as AXUIElementRef;
            assert_eq!(AXUIElementSetMessagingTimeout(window, 1.0), kAXErrorSuccess);
            let mut id = 0;
            assert_eq!(
                _AXUIElementGetWindow(window, &mut id),
                kAXErrorSuccess,
                "cleanup window identity read failed"
            );
            u64::from(id) == window_id
        })
    }
}
