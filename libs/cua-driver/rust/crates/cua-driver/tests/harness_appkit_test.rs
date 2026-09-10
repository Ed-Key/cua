//! Integration test against the CuaTestHarness.AppKit Swift app.
//!
//! Mirror of `harness_wpf_test.rs` for the macOS AppKit hosting pattern.
//! The harness app lives at `libs/cua-driver/tests/fixtures/apps/macos/appkit`
//! and is published into `libs/cua-driver/rust/test-apps/harness-appkit/`
//! by `libs/cua-driver/tests/fixtures/build/macos.sh`.
//!
//! Scenarios (see `libs/cua-driver/tests/fixtures/shared/scenarios.json`
//! `appkit` section):
//!   - counter        : NSButton AXPress invocation increments counter
//!   - text_body      : get_window_state extracts known marker text
//!   - text_input     : type_text into NSTextField updates mirror label
//!   - click_target   : right_click / double_click recognised by NSView
//!   - scroll_target  : scroll updates VerticalOffset label
//!   - ns_menubar     : main menubar item enumerable (Mac-specific)
//!
//! Run locally (after `libs/cua-driver/tests/fixtures/build/macos.sh`):
//!   cargo test --test harness_appkit_test -- --ignored --nocapture
//!
//! Tests are `#[ignore]` so they don't run in plain `cargo test`.
//!
//! The macOS lane preflight verifies the installed daemon identity and TCC
//! grants before these tests run. Missing fixtures or AX trees fail here too.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use cua_driver_testkit::ax::{element_index_by_id, element_index_containing, has_id, looks_empty};
use cua_driver_testkit::e2e::{
    execute_case, native_background_case, native_foreground_case, native_readonly_case,
    recording_evidence, DriverRoute, Evidence, Observation, OracleKind, RefusalCode, Targeting,
};
use cua_driver_testkit::observer::TargetWindow;
use cua_driver_testkit::sentinel::run_with_background_oracles;
use cua_driver_testkit::{Driver, McpDriver, ToolResponse};

// ── paths ────────────────────────────────────────────────────────────────────

fn harness_app() -> PathBuf {
    if let Ok(p) = std::env::var("HARNESS_APPKIT_APP") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    cua_driver_testkit::harness_app("harness-appkit", "CuaTestHarness.AppKit.app")
}

fn harness_exe() -> PathBuf {
    harness_app().join("Contents/MacOS/CuaTestHarness.AppKit")
}

// ── harness fixture ──────────────────────────────────────────────────────────

struct Harness {
    _app: Child,
    pid: u32,
}

impl Harness {
    fn launch() -> Self {
        Self::launch_with_command_oracle(None)
    }

    fn launch_with_command_oracle(command_oracle: Option<&Path>) -> Self {
        Self::launch_with_oracles(command_oracle, None)
    }

    fn launch_with_oracles(command_oracle: Option<&Path>, pointer_oracle: Option<&Path>) -> Self {
        let exe = harness_exe();
        assert!(
            exe.exists(),
            "required AppKit harness is missing at {exe:?}; run the fixture build"
        );
        // Launch the binary directly (not via `open`) so we control the pid
        // and can kill it cleanly on Drop. The app still installs an AppKit
        // window via NSApp.run().
        let mut command = Command::new(&exe);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(path) = command_oracle {
            command.env("CUA_APPKIT_COMMAND_ORACLE", path);
        }
        if let Some(path) = pointer_oracle {
            command.env("CUA_APPKIT_POINTER_ORACLE", path);
        }
        let app = command
            .spawn()
            .unwrap_or_else(|error| panic!("launch AppKit harness {exe:?}: {error}"));
        let pid = app.id();
        // Settle for window creation + activation.
        std::thread::sleep(Duration::from_millis(800));
        Self { _app: app, pid }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self._app.kill();
        let _ = self._app.wait();
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ── window / element helpers ─────────────────────────────────────────────────

fn snapshot_elements(driver: &mut McpDriver, pid: u32, window_id: u64) -> ToolResponse {
    driver.call(
        "get_window_state",
        serde_json::json!({
            "pid": pid as i64,
            "window_id": window_id,
            "capture_mode": "ax"
        }),
    )
}

fn element_token_by_id(snapshot: &ToolResponse, identifier: &str) -> String {
    let index = element_index_by_id(snapshot.tree_text(), identifier)
        .unwrap_or_else(|| panic!("{identifier} element_index not found"));
    snapshot.structured()["elements"]
        .as_array()
        .and_then(|elements| {
            elements
                .iter()
                .find(|element| element["element_index"].as_u64() == Some(index))
        })
        .and_then(|element| element["element_token"].as_str())
        .unwrap_or_else(|| panic!("{identifier} element_token not found"))
        .to_owned()
}

fn element_pixel_frame(snapshot: &ToolResponse, identifier: &str) -> (f64, f64, f64, f64) {
    let index = element_index_by_id(snapshot.tree_text(), identifier)
        .unwrap_or_else(|| panic!("{identifier} element_index not found"));
    let elements = snapshot.structured()["elements"]
        .as_array()
        .expect("AppKit structured elements");
    let element = elements
        .iter()
        .find(|element| element["element_index"].as_u64() == Some(index))
        .unwrap_or_else(|| panic!("{identifier} element frame not found"));
    let window = elements
        .iter()
        .find(|element| element["role"].as_str() == Some("AXWindow"))
        .expect("AppKit window frame");
    let scale = snapshot.structured()["screenshot_width"]
        .as_f64()
        .unwrap_or(1.0)
        / window["frame"]["w"].as_f64().unwrap_or(1.0).max(1.0);
    (
        (element["frame"]["x"].as_f64().unwrap_or(0.0)
            - window["frame"]["x"].as_f64().unwrap_or(0.0))
            * scale,
        (element["frame"]["y"].as_f64().unwrap_or(0.0)
            - window["frame"]["y"].as_f64().unwrap_or(0.0))
            * scale,
        element["frame"]["w"].as_f64().unwrap_or(0.0) * scale,
        element["frame"]["h"].as_f64().unwrap_or(0.0) * scale,
    )
}

fn run_case(
    case: cua_driver_testkit::e2e::CaseSpec,
    test: impl FnOnce(u32, u64, &mut McpDriver) -> Observation,
) {
    let cell_id = case.cell_id.clone();
    let delivery = case.delivery;
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(&cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let harness = Harness::launch();
        let (wid, _) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("AppKit main window not found");
        if delivery != cua_driver_testkit::e2e::Delivery::Background {
            driver.start_behavior_recording();
        }
        test(harness.pid, wid, &mut driver)
    });
}

fn run_background_case(
    action: &str,
    route: DriverRoute,
    test: impl FnOnce(u32, u64, &mut McpDriver),
) {
    run_background_case_targeting(action, Targeting::Ax, route, test);
}

fn run_background_case_targeting(
    action: &str,
    targeting: Targeting,
    route: DriverRoute,
    test: impl FnOnce(u32, u64, &mut McpDriver),
) {
    run_case(
        native_background_case("appkit", action, targeting, route),
        |pid, wid, driver| {
            let (_, passed) = run_with_background_oracles(
                driver,
                TargetWindow {
                    pid,
                    native_id: wid,
                },
                |driver| test(pid, wid, driver),
            )
            .unwrap_or_else(|error| panic!("background desktop contract failed: {error}"));
            Observation::delivered_with_fixture_state(passed)
        },
    );
}

// ── tests ────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn harness_appkit_foreground_single_click_has_one_ordered_native_pair() {
    let case = native_foreground_case(
        "appkit",
        "single_click_native_pair",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
    );
    execute_case(case, |evidence| {
        let mut driver =
            McpDriver::spawn_macos_daemon_proxy_named("appkit-single-click-native-pair")
                .expect("start macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let directory = tempfile::tempdir().expect("create native pointer journal directory");
        let journal = directory.path().join("pointer.jsonl");
        std::fs::write(&journal, "").expect("initialize native pointer journal");
        let harness = Harness::launch_with_oracles(None, Some(&journal));
        let (wid, _) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("find native receiver window");
        driver.start_behavior_recording();
        let read_events = || -> Vec<serde_json::Value> {
            std::fs::read_to_string(&journal)
                .expect("read native pointer journal")
                .lines()
                .map(|line| serde_json::from_str(line).expect("parse native pointer event"))
                .collect()
        };
        let initial = read_events();
        assert_eq!(
            initial.len(),
            1,
            "receiver must be idle before the request: {initial:?}"
        );
        assert_eq!(initial[0]["kind"], "ready");
        assert_eq!(initial[0]["window_id"].as_u64(), Some(wid));
        let snapshot = snapshot_elements(&mut driver, harness.pid, wid);
        assert!(
            !snapshot.is_error(),
            "capture receiver: {}",
            snapshot.text()
        );
        let width = snapshot.structured()["screenshot_width"]
            .as_f64()
            .expect("screenshot width");
        let height = snapshot.structured()["screenshot_height"]
            .as_f64()
            .expect("screenshot height");
        assert!(width > 0.0 && height > 0.0);
        let response = driver.call(
            "click",
            serde_json::json!({
                "pid": harness.pid,
                "window_id": wid,
                "x": width / 2.0,
                "y": height / 2.0,
                "count": 1,
                "delivery_mode": "foreground"
            }),
        );
        assert!(
            !response.is_error(),
            "single click request failed: {}",
            response.text()
        );
        std::thread::sleep(Duration::from_millis(750));
        let events = read_events();
        let received = &events[1..];
        assert_eq!(
        received.len(),
        2,
        "one request must deliver one native down/up pair: {received:?}; receiver={initial:?}; screenshot={width}x{height}"
    );
        assert_eq!(received[0]["kind"], "down");
        assert_eq!(received[1]["kind"], "up");
        let expected_x = initial[0]["width"].as_f64().unwrap() / 2.0;
        let expected_y = initial[0]["height"].as_f64().unwrap() / 2.0;
        for event in received {
            assert_eq!(event["window_id"].as_u64(), Some(wid));
            assert_eq!(event["click_count"], 1);
            assert!(
                (event["x"].as_f64().unwrap() - expected_x).abs() <= 1.0,
                "wrong horizontal target: {event}"
            );
            assert!(
                (event["y"].as_f64().unwrap() - expected_y).abs() <= 1.0,
                "wrong vertical target: {event}"
            );
        }
        assert!(
            received[0]["timestamp"].as_f64().unwrap()
                <= received[1]["timestamp"].as_f64().unwrap()
        );
        println!("native pointer events: {received:?}");
        Observation::delivered_with_fixture_state(vec![])
    });
}

#[test]
#[ignore]
fn harness_appkit_smoke() {
    run_case(
        native_readonly_case(
            "appkit",
            "ax_tree",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        |pid, wid, driver| {
            let snap = snapshot_elements(driver, pid, wid);

            assert!(
                !looks_empty(snap.tree_text()),
                "required AppKit AX tree is empty"
            );

            let text = snap.tree_text();
            println!("snapshot:\n{text}");

            // AppKit AX quirk (mirrors the WPF behavior documented in
            // harness_wpf_test.rs::harness_wpf_smoke): NSTextField in label mode
            // and other AXStaticText leaves do NOT propagate
            // setAccessibilityIdentifier into the AX tree's identifier slot, so
            // we don't assert on ids for labels. We assert on text-presence for
            // those, and on AX ids only for actionable controls whose AppKit
            // identifiers are actually propagated (Buttons and TextFields).
            // NSMenuItem behaves like the static leaves here: its title is
            // exposed, but setAccessibilityIdentifier is not.
            for aid in [
                "wnd-main", // NSWindow
                "btn-increment",
                "btn-reset", // NSButton
                "txt-input", // editable NSTextField
                "btn-exit",
            ] {
                assert!(
                    has_id(snap.tree_text(), aid),
                    "missing AX identifier {aid} in AppKit snapshot"
                );
            }

            // text_body marker carried by the visible string of the NSTextField
            assert!(
                text.contains("HARNESS_TEXT_MARKER_v1"),
                "text_body marker not in AppKit snapshot"
            );
            // The two label-mode NSTextFields under click_target render as
            // AXStaticText nodes — assert on their starting text instead of ids.
            assert!(text.contains("counter=0"), "counter label missing");
            assert!(text.contains("clicks=0"), "click_count label missing");
            assert!(
                text.contains("Harness Test Item"),
                "AppKit menu item title missing"
            );
            assert!(
                text.contains("last_action=none"),
                "last_action label missing"
            );
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_query_projects_structured_elements() {
    run_case(
        native_readonly_case(
            "appkit",
            "query_projection",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        |pid, wid, driver| {
            let response = driver.call(
                "get_window_state",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "query": "btn-increment",
                    "include_screenshot": false
                }),
            );
            assert!(
                !response.is_error(),
                "query snapshot failed: {}",
                response.text()
            );
            let total = response.structured()["total_element_count"]
                .as_u64()
                .expect("total_element_count");
            let returned = response.structured()["returned_element_count"]
                .as_u64()
                .expect("returned_element_count");
            let elements = response.structured()["elements"]
                .as_array()
                .expect("projected elements");
            assert_eq!(returned as usize, elements.len());
            assert!(
                returned < total,
                "query did not compact {returned}/{total} elements"
            );
            assert!(has_id(response.tree_text(), "btn-increment"));
            let _ = element_token_by_id(&response, "btn-increment");
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_stale_element_token_fails_closed() {
    run_case(
        native_readonly_case(
            "appkit",
            "stale_element_token",
            Targeting::Ax,
            DriverRoute::AxRead,
            vec![OracleKind::AxState],
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let token = element_token_by_id(&first, "btn-increment");
            let _newer = snapshot_elements(driver, pid, wid);
            let refused = driver.call(
                "click",
                serde_json::json!({"pid": pid as i64, "element_token": token}),
            );
            assert!(
                refused.is_error(),
                "stale token was accepted: {}",
                refused.text()
            );
            assert_eq!(
                refused.structured()["refusal"]["code"].as_str(),
                Some("stale_element_token")
            );
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("counter=0"),
                "stale click mutated counter"
            );
            Observation::delivered(vec![OracleKind::AxState], Evidence::default())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_invoke_menu_live_path() {
    run_case(
        native_foreground_case(
            "appkit",
            "invoke_menu",
            Targeting::Ax,
            DriverRoute::MacosAxAction,
        ),
        |pid, wid, driver| {
            let refused = driver.call(
                "invoke_menu",
                serde_json::json!({
                    "pid": pid,
                    "window_id": wid,
                    "path": ["Window", "Arrange", "Missing"]
                }),
            );
            assert!(refused.is_error(), "missing menu path was accepted");
            assert!(snapshot_elements(driver, pid, wid)
                .tree_text()
                .contains("menu_action=none"));

            // A second native process deliberately steals AppKit activation
            // and key-window status. The target menu item validates against
            // both, so an AXFocused-only implementation cannot pass this cell.
            let _distractor = Harness::launch();

            let invoked = driver.call(
                "invoke_menu",
                serde_json::json!({
                    "pid": pid,
                    "window_id": wid,
                    "path": ["Window", "Arrange", "Left"]
                }),
            );
            assert!(
                !invoked.is_error(),
                "invoke_menu failed: {}",
                invoked.text()
            );
            assert_eq!(invoked.action_effect(), Some("unverifiable"));
            std::thread::sleep(Duration::from_millis(300));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("menu_action=window_arrange_left"),
                "menu action did not reach fixture: {}",
                post.tree_text()
            );
            Observation::delivered(vec![OracleKind::FixtureState], Evidence::default())
        },
    );
}

/// text_input: type_text into the NSTextField, verify the mirror label
/// shows the typed string. Exercises the AX type_text path
/// (AXSetAttribute on AXValue, or CGEvent fallback).
#[test]
#[ignore]
fn harness_appkit_text_input() {
    run_background_case(
        "set_value",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                !looks_empty(snap_pre.tree_text()),
                "required AppKit AX tree is empty"
            );
            let idx = element_index_by_id(snap_pre.tree_text(), "txt-input")
                .expect("txt-input element_index not found");

            // set_value via AX is the deterministic background path; type_text would
            // also work but races with cursor focus on cold-launched windows.
            let resp = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "value": "hello-cua"
                }),
            );
            assert!(!resp.is_error(), "AppKit set_value failed: {}", resp.text());
            println!("set_value resp: {}", resp.text());

            std::thread::sleep(Duration::from_millis(250));
            let snap_post = snapshot_elements(driver, pid, wid);
            let post_text = snap_post.tree_text().to_owned();
            assert!(
                post_text.contains("hello-cua"),
                "text_input value did not propagate to mirror; snapshot:\n{post_text}"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_element_foreground_press_key_commits_edit() {
    run_case(
        native_foreground_case(
            "appkit",
            "press_key_commit",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let field = element_token_by_id(&first, "txt-input");
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": field,
                    "value": "inline-cua"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());

            let second = snapshot_elements(driver, pid, wid);
            assert!(
                second.tree_text().contains("inline-cua"),
                "transient edit value was not readable:\n{}",
                second.tree_text()
            );
            assert!(
                second.tree_text().contains("committed=none"),
                "fixture reported a commit before Return:\n{}",
                second.tree_text()
            );
            let field = element_token_by_id(&second, "txt-input");
            let commit = driver.call(
                "press_key",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": field,
                    "key": "return",
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !commit.is_error(),
                "foreground element press_key failed: {}",
                commit.text()
            );
            assert_eq!(
                commit.action_route(),
                Some("global_input"),
                "foreground press_key used the wrong public route: {}",
                commit.raw
            );
            assert_eq!(
                commit.action_delivery_mode(),
                Some("foreground"),
                "foreground press_key reported the wrong delivery: {}",
                commit.raw
            );
            assert_eq!(
                commit.action_effect(),
                Some("unverifiable"),
                "press_key claimed more truth than the tool itself observed: {}",
                commit.raw
            );

            std::thread::sleep(Duration::from_millis(250));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("committed=inline-cua"),
                "Return did not commit the addressed edit:\n{}",
                post.tree_text()
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_px_background_press_key_reports_honest_delivery_truth() {
    let case = native_background_case(
        "appkit",
        "press_key_command",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
    );
    let cell_id = case.cell_id.clone();
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(&cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let oracle_dir = tempfile::tempdir().expect("create command oracle directory");
        let oracle_path = oracle_dir.path().join("child-process-output.txt");
        let harness = Harness::launch_with_command_oracle(Some(&oracle_path));
        let (wid, _) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("AppKit main window not found");

        let (_, passed) = run_with_background_oracles(
            &mut driver,
            TargetWindow {
                pid: harness.pid,
                native_id: wid,
            },
            |driver| {
                let first = snapshot_elements(driver, harness.pid, wid);
                let field = element_token_by_id(&first, "txt-input");
                let set = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid": harness.pid as i64,
                        "window_id": wid,
                        "element_token": field,
                        "value": "printf cua-press-key"
                    }),
                );
                assert!(!set.is_error(), "set command failed: {}", set.text());

                let focused = snapshot_elements(driver, harness.pid, wid);
                let (x, y, width, height) = element_pixel_frame(&focused, "txt-input");
                let pressed = driver.call(
                    "press_key",
                    serde_json::json!({
                        "pid": harness.pid as i64,
                        "window_id": wid,
                        "x": x + width / 2.0,
                        "y": y + height / 2.0,
                        "key": "return",
                        "delivery_mode": "background"
                    }),
                );
                assert!(
                    !pressed.is_error(),
                    "background Return failed: {}",
                    pressed.text()
                );
                assert_eq!(pressed.action_route(), Some("synthetic_events"));
                assert_eq!(pressed.action_delivery_mode(), Some("background"));
                assert_eq!(pressed.action_effect(), Some("unverifiable"));
                assert!(
                    pressed.structured()["escalation"].is_null(),
                    "accepted post without a positive oracle must not claim delivery_failed: {}",
                    pressed.raw
                );

                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                loop {
                    if std::fs::read_to_string(&oracle_path)
                        .is_ok_and(|value| value == "cua-press-key")
                    {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "background Return did not execute the controlled child process"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }

                let mut exited = Command::new("/usr/bin/true")
                    .spawn()
                    .expect("spawn posting-failure fixture");
                let exited_pid = exited.id();
                exited.wait().expect("wait for posting-failure fixture");
                let failed = driver.call(
                    "press_key",
                    serde_json::json!({
                        "pid": exited_pid,
                        // An explicit target bypasses the PID-only window resolver so
                        // this negative oracle reaches the posting preflight. Without
                        // one, the earlier and equally truthful result is
                        // window_target_not_found because /usr/bin/true owns no window.
                        "window_id": wid,
                        "key": "return",
                        "delivery_mode": "background"
                    }),
                );
                assert!(failed.is_error(), "dead-pid post unexpectedly succeeded");
                assert_eq!(failed.structured()["code"], "delivery_failed");
            },
        )
        .unwrap_or_else(|error| panic!("background desktop contract failed: {error}"));

        Observation::delivered_with_fixture_state(passed)
    });
}

#[test]
#[ignore]
fn harness_appkit_modified_click_preserves_selection() {
    run_case(
        native_foreground_case(
            "appkit",
            "modified_click_selection",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let alpha = element_token_by_id(&first, "selection-alpha");
            let select_alpha = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": alpha
                }),
            );
            assert!(
                !select_alpha.is_error(),
                "select alpha failed: {}",
                select_alpha.text()
            );
            std::thread::sleep(Duration::from_millis(200));

            let second = snapshot_elements(driver, pid, wid);
            assert!(
                second.tree_text().contains("selection=alpha"),
                "alpha was not selected:\n{}",
                second.tree_text()
            );
            let beta = element_token_by_id(&second, "selection-beta");
            let refused_background = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": beta,
                    "modifier": ["cmd"]
                }),
            );
            assert!(
                refused_background.is_error(),
                "background modified click was not refused: {}",
                refused_background.text()
            );
            assert_eq!(
                refused_background.structured()["code"],
                "background_unavailable",
                "background modified click returned the wrong refusal: {}",
                refused_background.structured()
            );
            std::thread::sleep(Duration::from_millis(300));
            let after_refusal = snapshot_elements(driver, pid, wid);
            assert!(
                after_refusal.tree_text().contains("selection=alpha"),
                "refused modified click changed the prior selection:\n{}",
                after_refusal.tree_text()
            );

            let beta = element_token_by_id(&after_refusal, "selection-beta");
            let add_beta = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": beta,
                    "modifier": ["cmd"],
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !add_beta.is_error(),
                "foreground modified click failed: {}",
                add_beta.text()
            );
            assert_eq!(
                add_beta.structured()["effect"],
                "confirmed",
                "modified click lacked settled selection proof: {}",
                add_beta.structured()
            );

            std::thread::sleep(Duration::from_millis(250));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("selection=alpha,beta"),
                "modified click replaced or lost the prior selection:\n{}",
                post.tree_text()
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// type_text: synthesize a keystroke into the NSTextField (CGEvent
/// path, distinct from set_value's AX path). Verifies the keyboard
/// dispatch chain reaches a backgrounded Cocoa text input.
#[test]
#[ignore]
fn harness_appkit_type_text_background() {
    run_background_case(
        "type_text",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                !looks_empty(snap_pre.tree_text()),
                "required AppKit AX tree is empty"
            );
            let idx = element_index_by_id(snap_pre.tree_text(), "txt-input")
                .expect("txt-input element_index not found");

            // Address the field through type_text itself. AXTextField does not
            // advertise AXPress, so a preparatory click would test an invalid
            // action and fail before the keyboard/value delivery path runs.
            let resp = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "text": "kbd-cua", "delivery_mode": "background"
                }),
            );
            assert!(!resp.is_error(), "AppKit type_text failed: {}", resp.text());
            println!("type_text resp: {}", resp.text());
            std::thread::sleep(Duration::from_millis(250));

            let snap_post = snapshot_elements(driver, pid, wid);
            let post = snap_post.tree_text().to_owned();
            assert!(
                post.contains("kbd-cua"),
                "type_text keystroke did not land in the text field; snapshot:\n{post}"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_scroll_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "scroll",
            Targeting::Ax,
            DriverRoute::MacosAxAction,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            assert!(pre.tree_text().contains("scroll_offset=0"));
            let index = element_index_by_id(pre.tree_text(), "scroll-tall")
                .or_else(|| element_index_containing(pre.tree_text(), "SCROLL_TOP_MARKER_v1"))
                .unwrap_or_else(|| {
                    panic!("scroll-tall element_index not found:\n{}", pre.tree_text())
                });
            let response = driver.call(
                "scroll",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": index,
                    "snapshot_id": pre.snapshot_id(),
                    "direction": "down",
                    "amount": 5,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit foreground scroll failed: {}; raw={}",
                response.text(),
                response.raw
            );
            std::thread::sleep(Duration::from_millis(300));
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                !post.tree_text().contains("scroll_offset=0"),
                "AppKit foreground scroll did not move the NSScrollView; response={}; raw={}",
                response.text(),
                response.raw
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_scroll_background() {
    run_background_case("scroll", DriverRoute::MacosAxAction, |pid, wid, driver| {
        let pre = snapshot_elements(driver, pid, wid);
        assert!(pre.tree_text().contains("scroll_offset=0"));
        let index = element_index_by_id(pre.tree_text(), "scroll-tall")
            .or_else(|| element_index_containing(pre.tree_text(), "SCROLL_TOP_MARKER_v1"))
            .unwrap_or_else(|| panic!("scroll-tall element_index not found:\n{}", pre.tree_text()));
        let response = driver.call(
            "scroll",
            serde_json::json!({
                "pid": pid as i64,
                "window_id": wid,
                "element_index": index,
                "snapshot_id": pre.snapshot_id(),
                "direction": "down",
                "amount": 5,
                "delivery_mode": "background"
            }),
        );
        assert!(
            !response.is_error(),
            "AppKit background scroll failed: {}; raw={}",
            response.text(),
            response.raw
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !snapshot_elements(driver, pid, wid)
                .tree_text()
                .contains("scroll_offset=0"),
            "AppKit background AX scroll did not move the NSScrollView"
        );
    });
}

/// counter: click the increment button via element_index, verify the
/// counter label flips from 0 to 1.
#[test]
#[ignore]
fn harness_appkit_counter() {
    run_background_case(
        "left_click",
        DriverRoute::MacosAxAction,
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            assert!(
                !looks_empty(snap_pre.tree_text()),
                "required AppKit AX tree is empty"
            );
            let pre_text = snap_pre.tree_text().to_owned();
            assert!(
                pre_text.contains("counter=0"),
                "counter not 0 pre-click; snapshot:\n{pre_text}"
            );

            let idx = element_index_by_id(snap_pre.tree_text(), "btn-increment")
                .expect("btn-increment element_index not found");

            let click_resp = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "action": "press",
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !click_resp.is_error(),
                "AppKit counter click failed: {}",
                click_resp.text()
            );
            println!("click resp: {}", click_resp.text());

            // Let the AppKit run-loop process the press and refresh the label.
            std::thread::sleep(Duration::from_millis(200));

            let snap_post = snapshot_elements(driver, pid, wid);
            let post_text = snap_post.tree_text().to_owned();
            assert!(
                post_text.contains("counter=1"),
                "counter did not advance to 1 after press; post snapshot:\n{post_text}"
            );
        },
    );
}

/// Resolve the native AppKit button from a screenshot-space PX target, then
/// deliver through the background-safe AX hit-test bridge while another app
/// remains fully foreground.
#[test]
#[ignore]
fn harness_appkit_counter_px_background() {
    run_background_case_targeting(
        "left_click",
        Targeting::Px,
        DriverRoute::MacosAxAction,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let counter = |snapshot: &ToolResponse| -> u64 {
                snapshot
                    .tree_text()
                    .split("counter=")
                    .nth(1)
                    .expect("fixture counter label")
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .expect("numeric fixture counter")
            };
            assert_eq!(counter(&pre), 0, "fixture must start at zero");
            let index = element_index_by_id(pre.tree_text(), "btn-increment").unwrap();
            let element = pre.structured()["elements"]
                .as_array()
                .unwrap()
                .iter()
                .find(|element| element["element_index"].as_u64() == Some(index))
                .unwrap();
            let frame = &element["frame"];
            let expected_x = frame["x"].as_f64().unwrap() + frame["w"].as_f64().unwrap() / 2.0;
            let expected_y = frame["y"].as_f64().unwrap() + frame["h"].as_f64().unwrap() / 2.0;
            let session = "slice-a-task6-counter";
            let seed = driver.call(
                "move_cursor",
                serde_json::json!({
                    "session": session, "x": 5.0, "y": 5.0
                }),
            );
            assert!(!seed.is_error(), "cursor seed failed: {}", seed.raw);
            let before = driver.call(
                "get_agent_cursor_state",
                serde_json::json!({"session": session}),
            );
            assert!(!before.is_error(), "cursor read failed: {}", before.raw);
            assert_eq!(before.structured()["position"]["x"].as_f64(), Some(5.0));
            assert_eq!(before.structured()["position"]["y"].as_f64(), Some(5.0));
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-increment");
            let response = driver.call(
                "click",
                serde_json::json!({
                    "session": session,
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit PX background click failed: {}",
                response.text()
            );
            assert_eq!(
                response.action_route(),
                Some("accessibility"),
                "pixel counter row must actually use AX delivery: {}",
                response.raw
            );
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(
                counter(&snapshot_elements(driver, pid, wid)),
                1,
                "one pixel click must advance the fixture counter exactly once"
            );
            let cursor = driver.call(
                "get_agent_cursor_state",
                serde_json::json!({"session": session}),
            );
            assert!(!cursor.is_error(), "cursor read failed: {}", cursor.raw);
            let position = &cursor.structured()["position"];
            let actual_x = position["x"].as_f64().expect("resolved cursor x");
            let actual_y = position["y"].as_f64().expect("resolved cursor y");
            assert!((actual_x - expected_x).abs() < 0.5 && (actual_y - expected_y).abs() < 0.5,
                "cursor ({actual_x}, {actual_y}) did not reach resolved button center ({expected_x}, {expected_y})");
            println!("Task 6 native row: route=accessibility counter=0->1 cursor=({actual_x},{actual_y})");
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_right_click_px_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "right_click",
            Targeting::Px,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "right_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit right click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("last_action=right_click"),
                "AppKit right-click handler did not fire"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_right_click_px_background() {
    run_background_case_targeting(
        "right_click",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "right_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit right click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("last_action=right_click"),
                "AppKit background right-click handler did not fire"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_double_click_px_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "double_click",
            Targeting::Px,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "double_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit double click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("last_action=double_click"),
                "AppKit double-click handler did not fire"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_double_click_px_background() {
    run_background_case_targeting(
        "double_click",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let response = driver.call(
                "double_click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "x": x + width / 2.0,
                    "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit double click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(250));
            let receiver_snapshot = snapshot_elements(driver, pid, wid);
            let receiver = receiver_snapshot.tree_text();
            assert!(
                receiver.contains("last_action=double_click") && receiver.contains("clicks=2"),
                "AppKit background double-click receiver did not record exactly two clicks"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_slider_drag_px_foreground() {
    run_case(
        native_foreground_case(
            "appkit",
            "slider_drag",
            Targeting::Px,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            assert!(pre.tree_text().contains("slider_value=0"));
            let (x, y, width, height) = element_pixel_frame(&pre, "sld-value");
            let response = driver.call(
                "drag",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "from_x": x + width * 0.05,
                    "from_y": y + height / 2.0,
                    "to_x": x + width * 0.90,
                    "to_y": y + height / 2.0,
                    "duration_ms": 500,
                    "steps": 30,
                    "delivery_mode": "foreground"
                }),
            );
            assert!(
                !response.is_error(),
                "AppKit slider drag failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(300));
            assert!(
                !snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("slider_value=0"),
                "AppKit foreground drag did not move the slider"
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_slider_drag_px_background() {
    let case = native_background_case(
        "appkit",
        "slider_drag",
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
    )
    .expecting_refusal(vec![RefusalCode::BackgroundUnavailable]);
    run_case(case, |pid, wid, driver| {
        let pre = snapshot_elements(driver, pid, wid);
        assert!(pre.tree_text().contains("slider_value=0"));
        let (x, y, width, height) = element_pixel_frame(&pre, "sld-value");
        let (response, mut passed) = run_with_background_oracles(
            driver,
            TargetWindow {
                pid,
                native_id: wid,
            },
            |driver| {
                driver.call(
                    "drag",
                    serde_json::json!({
                        "pid": pid as i64,
                        "window_id": wid,
                        "from_x": x + width * 0.05,
                        "from_y": y + height / 2.0,
                        "to_x": x + width * 0.90,
                        "to_y": y + height / 2.0,
                        "duration_ms": 500,
                        "steps": 30,
                        "delivery_mode": "background"
                    }),
                )
            },
        )
        .unwrap_or_else(|error| panic!("background desktop contract failed: {error}"));
        assert!(
            response.is_error(),
            "AppKit background drag unexpectedly reported delivery: {}",
            response.text()
        );
        assert_eq!(
            response.structured()["code"].as_str(),
            Some("background_unavailable"),
            "AppKit background drag returned the wrong refusal: {}",
            response.text()
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            snapshot_elements(driver, pid, wid)
                .tree_text()
                .contains("slider_value=0"),
            "refused AppKit background drag changed the slider"
        );
        passed.push(OracleKind::FixtureState);
        Observation::refused(
            RefusalCode::BackgroundUnavailable,
            passed,
            response.text(),
            Evidence::default(),
        )
    });
}

// File hashes corroborate the candidate artifact but cannot identify a loaded image.
fn validate_slice_a_build_identity(
    candidate_sha: &str,
    config: &serde_json::Value,
    executable_file_hash: &str,
    built_hash: &str,
    manifest_hash: Option<&str>,
) -> Result<(), &'static str> {
    let source_sha = config["source_sha"]
        .as_str()
        .ok_or("get_config.source_sha is missing or not a string")?;
    for sha in [candidate_sha, source_sha] {
        if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("candidate and daemon source_sha must be full 40-digit Git SHAs");
        }
    }
    if source_sha != candidate_sha {
        return Err("running daemon get_config.source_sha does not match the candidate");
    }
    if executable_file_hash != built_hash || manifest_hash != Some(built_hash) {
        return Err("candidate executable file hashes do not match");
    }
    Ok(())
}

fn slice_a_file_sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .expect("hash candidate evidence file");
    assert!(out.status.success(), "file hash failed");
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

#[cfg(test)]
mod slice_a_identity_tests {
    use super::*;
    use serde_json::json;

    const CANDIDATE: &str = "64892c4e0ee152990c4d9f57ba4bc289e953f5ec";
    const STALE: &str = "8df29cecb3ee299482a85bcb95283f43d901329b";
    const HASH: &str = "candidate-file-hash";

    #[test]
    fn stale_reported_sha_is_rejected_with_matching_filepath_and_filehash() {
        let artifact = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(artifact.path(), b"newly rebuilt candidate artifact").unwrap();
        // Both path lookups see the replaced candidate file. The daemon still
        // reports its old embedded identity, which must veto that corroboration.
        let daemon_path = artifact.path();
        let candidate_path = artifact.path();
        assert_eq!(daemon_path, candidate_path);
        let daemon_file_hash = slice_a_file_sha256(daemon_path);
        let candidate_file_hash = slice_a_file_sha256(candidate_path);
        assert_eq!(daemon_file_hash, candidate_file_hash);
        assert!(validate_slice_a_build_identity(
            CANDIDATE,
            &json!({"source_sha": STALE}),
            &daemon_file_hash,
            &candidate_file_hash,
            Some(&candidate_file_hash)
        )
        .is_err());
    }

    #[test]
    fn missing_sha_is_rejected() {
        for config in [json!({}), json!({"source_sha": null})] {
            assert!(
                validate_slice_a_build_identity(CANDIDATE, &config, HASH, HASH, Some(HASH))
                    .is_err()
            );
        }
    }

    #[test]
    fn malformed_sha_is_rejected() {
        for source in [
            json!(""),
            json!("64892c4"),
            json!("g".repeat(40)),
            json!(format!("{CANDIDATE}\n")),
            json!(123),
            json!([]),
            json!({}),
        ] {
            assert!(
                validate_slice_a_build_identity(
                    CANDIDATE,
                    &json!({"source_sha": source}),
                    HASH,
                    HASH,
                    Some(HASH)
                )
                .is_err(),
                "accepted malformed identity: {source}"
            );
        }
        assert!(validate_slice_a_build_identity(
            "bad",
            &json!({"source_sha": "bad"}),
            HASH,
            HASH,
            Some(HASH)
        )
        .is_err());
    }

    #[test]
    fn matching_sha_and_corroborating_hashes_are_accepted() {
        assert!(validate_slice_a_build_identity(
            CANDIDATE,
            &json!({"source_sha": CANDIDATE}),
            HASH,
            HASH,
            Some(HASH)
        )
        .is_ok());
    }

    #[test]
    fn matching_sha_still_requires_corroborating_hashes() {
        for (file_hash, manifest_hash) in [
            ("old-file", Some(HASH)),
            (HASH, None),
            (HASH, Some("old-file")),
        ] {
            assert!(validate_slice_a_build_identity(
                CANDIDATE,
                &json!({"source_sha": CANDIDATE}),
                file_hash,
                HASH,
                manifest_hash
            )
            .is_err());
        }
    }
}

/// Verify controller-captured native display evidence without launching or
/// replacing a daemon. Requires an unmirrored 2x display plus a 1x secondary
/// display with a negative x or y origin. A synthetic image is not acceptance.
///
/// CUA_SLICE_A_DISPLAY_EVIDENCE points to a JSON manifest containing candidate_sha,
/// daemon_pid, candidate_binary_sha256, and captures. Each capture has display_id,
/// bounds [x,y,w,h], scale, screenshot, screenshot_sha256, target [global x,y],
/// and measured_tip_pixels [local x,y], independently annotated on that capture.
/// Capture the full display at native scale from the isolated candidate daemon.
/// Build the actual daemon with CUA_DRIVER_SOURCE_SHA set to the clean candidate's
/// full Git SHA. This row queries get_config through a persistent MCP proxy to the
/// explicit isolated socket and refuses a missing, malformed, or mismatched SHA.
/// Executable path and file hashes only corroborate artifacts, not the loaded image.
/// The controller must retain build and launch provenance, its persistent MCP
/// get_config response from the capture session, screenshots, and measurement method.
/// That provenance must bind the captures to this candidate daemon; querying a newer
/// process later cannot certify older captures. No new diagnostic tool is required.
#[test]
#[ignore = "requires controller candidate daemon and native 1x/2x negative-origin display captures"]
fn slice_a_cursor_display_geometry() {
    use std::ffi::{c_void, CStr};

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Size {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Rect {
        origin: Point,
        size: Size,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetActiveDisplayList(max: u32, ids: *mut u32, count: *mut u32) -> i32;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayMirrorsDisplay(id: u32) -> u32;
        fn CGDisplayBounds(id: u32) -> Rect;
        fn CGDisplayCopyDisplayMode(id: u32) -> *const c_void;
        fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
        fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
        fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    }
    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }
    fn output(command: &mut Command) -> String {
        let out = command.output().expect("run acceptance precondition");
        assert!(
            out.status.success(),
            "precondition command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    let mut ids = [0u32; 32];
    let mut count = 0;
    assert_eq!(
        unsafe { CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut count) },
        0
    );
    assert!(count < 32, "display inventory exceeded acceptance capacity");
    let primary = unsafe { CGMainDisplayID() };
    let displays: Vec<_> = ids[..count as usize]
        .iter()
        .copied()
        .filter_map(|id| unsafe {
            if CGDisplayMirrorsDisplay(id) != 0 {
                return None;
            }
            let bounds = CGDisplayBounds(id);
            let mode = CGDisplayCopyDisplayMode(id);
            assert!(!mode.is_null(), "display {id} has no current mode");
            let logical = (CGDisplayModeGetWidth(mode), CGDisplayModeGetHeight(mode));
            let pixels = (
                CGDisplayModeGetPixelWidth(mode),
                CGDisplayModeGetPixelHeight(mode),
            );
            core_foundation::base::CFRelease(mode);
            assert!(logical.0 > 0 && logical.1 > 0 && pixels.0 > 0 && pixels.1 > 0);
            let scale = pixels.0 as f64 / logical.0 as f64;
            assert!((pixels.1 as f64 / logical.1 as f64 - scale).abs() < 0.01);
            Some((id, bounds, scale, pixels))
        })
        .collect();
    println!("Slice A attached display inventory: {displays:?}");
    let retina = displays
        .iter()
        .find(|d| (d.2 - 2.0).abs() < 0.01)
        .expect("strict precondition: attach an unmirrored 2x display");
    let secondary = displays.iter().find(|d| d.0 != primary && (d.2 - 1.0).abs() < 0.01
        && (d.1.origin.x < 0.0 || d.1.origin.y < 0.0))
        .expect("strict precondition: attach an unmirrored 1x secondary display left of or above primary");

    let manifest_path = PathBuf::from(
        std::env::var("CUA_SLICE_A_DISPLAY_EVIDENCE")
            .expect("controller must supply native capture and independent tip measurements"),
    );
    let evidence: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let head = output(
        Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(["rev-parse", "HEAD"]),
    );
    assert_eq!(evidence["candidate_sha"].as_str(), Some(head.as_str()));
    assert!(
        output(Command::new("git").arg("-C").arg(&workspace).args([
            "status",
            "--porcelain",
            "--untracked-files=no"
        ]))
        .is_empty(),
        "candidate acceptance requires a clean tracked tree"
    );
    let socket = PathBuf::from(
        std::env::var("CUA_E2E_MACOS_DAEMON_SOCKET")
            .expect("controller must explicitly select the isolated candidate socket"),
    );
    let installed_socket = PathBuf::from(std::env::var("HOME").unwrap())
        .join("Library/Caches/cua-driver/cua-driver.sock");
    assert_ne!(
        socket.canonicalize().unwrap(),
        installed_socket.canonicalize().unwrap_or(installed_socket),
        "installed release daemon cannot stand in for the candidate"
    );
    let pid = i32::try_from(
        evidence["daemon_pid"]
            .as_i64()
            .expect("candidate daemon pid"),
    )
    .unwrap();
    assert!(pid > 0);
    let pids = output(Command::new("lsof").args(["-nP", "-t", "--"]).arg(&socket));
    assert!(
        pids.lines().any(|line| line.parse::<i32>() == Ok(pid)),
        "candidate must own the explicit socket"
    );
    let mut path = [0u8; 4096];
    assert!(unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) } > 0);
    let executable_path = PathBuf::from(
        unsafe { CStr::from_ptr(path.as_ptr().cast()) }
            .to_str()
            .unwrap(),
    );
    let built_hash = slice_a_file_sha256(Path::new(env!("CARGO_BIN_EXE_cua-driver")));
    // This proxy connects to the controller's existing daemon only. It neither
    // launches a daemon nor starts a behavior recording or native fixture.
    let mut driver = McpDriver::spawn_daemon_proxy_unrecorded(
        socket.to_str().expect("isolated socket path must be UTF-8"),
    )
    .expect("connect persistent MCP to the isolated candidate daemon");
    let config = driver.call("get_config", serde_json::json!({}));
    assert!(
        !config.is_error(),
        "candidate get_config failed: {}",
        config.text()
    );
    validate_slice_a_build_identity(
        &head,
        config.structured(),
        &slice_a_file_sha256(&executable_path),
        &built_hash,
        evidence["candidate_binary_sha256"].as_str(),
    )
    .expect("candidate build identity precondition");
    println!("Slice A daemon identity: pid={pid} source_sha={} executable_path={} corroborating_file_sha256={built_hash}",
        config.structured()["source_sha"], executable_path.display());
    let captures = evidence["captures"]
        .as_array()
        .expect("native capture rows");
    for display in [retina, secondary] {
        let (id, bounds, scale, pixels) = display;
        let matching: Vec<_> = captures
            .iter()
            .filter(|c| c["display_id"].as_u64() == Some(*id as u64))
            .collect();
        assert_eq!(matching.len(), 1, "one native capture per required display");
        let capture = matching[0];
        assert_eq!(
            capture["bounds"],
            serde_json::json!([
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                bounds.size.height
            ])
        );
        assert!((capture["scale"].as_f64().unwrap() - scale).abs() < 0.01);
        let image_path = manifest_path
            .parent()
            .unwrap()
            .join(capture["screenshot"].as_str().unwrap());
        assert_eq!(
            capture["screenshot_sha256"].as_str(),
            Some(slice_a_file_sha256(&image_path).as_str())
        );
        let image = image::open(&image_path).expect("native full-display capture");
        assert_eq!(
            (image.width() as usize, image.height() as usize),
            *pixels,
            "capture must retain native pixel size"
        );
        let target = &capture["target"];
        let target = (target[0].as_f64().unwrap(), target[1].as_f64().unwrap());
        assert!(cursor_overlay::DisplayBounds {
            x: bounds.origin.x,
            y: bounds.origin.y,
            width: bounds.size.width,
            height: bounds.size.height
        }
        .contains(target));
        let tip = &capture["measured_tip_pixels"];
        let tip = (tip[0].as_f64().unwrap(), tip[1].as_f64().unwrap());
        assert!(tip.0 >= 0.0 && tip.0 < pixels.0 as f64 && tip.1 >= 0.0 && tip.1 < pixels.1 as f64);
        let error = (tip.0 - (target.0 - bounds.origin.x) * scale)
            .hypot(tip.1 - (target.1 - bounds.origin.y) * scale);
        assert!(
            error <= 2.0,
            "native arrow-tip error {error} pixels exceeds 2 pixels"
        );
        println!("Slice A native evidence: SHA={head} display={id} bounds={bounds:?} scale={scale} screenshot={}x{} arrow_tip_error_px={error} image={}",
            pixels.0,pixels.1,image_path.display());
    }
}
