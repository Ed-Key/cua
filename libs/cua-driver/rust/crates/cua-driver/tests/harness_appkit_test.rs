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

#[path = "support/slice_a_latency.rs"]
mod slice_a_latency;

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

    fn launch_with_env(env: &[(&str, &str)]) -> Self {
        Self::launch_with(None, None, env)
    }

    fn launch_with_command_oracle(command_oracle: Option<&Path>) -> Self {
        Self::launch_with_oracles(command_oracle, None)
    }

    fn launch_with_oracles(command_oracle: Option<&Path>, pointer_oracle: Option<&Path>) -> Self {
        Self::launch_with(command_oracle, pointer_oracle, &[])
    }

    fn launch_with(
        command_oracle: Option<&Path>,
        pointer_oracle: Option<&Path>,
        env: &[(&str, &str)],
    ) -> Self {
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
        for (name, value) in env {
            command.env(name, value);
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
    run_case_with_env(case, &[], test);
}

fn run_case_with_env(
    case: cua_driver_testkit::e2e::CaseSpec,
    env: &[(&str, &str)],
    test: impl FnOnce(u32, u64, &mut McpDriver) -> Observation,
) {
    let cell_id = case.cell_id.clone();
    let delivery = case.delivery;
    execute_case(case, |evidence| {
        let mut driver = McpDriver::spawn_macos_daemon_proxy_named(&cell_id)
            .expect("start installed macOS daemon proxy");
        *evidence = recording_evidence(driver.recording_dir());
        let harness = Harness::launch_with_env(env);
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
    run_background_case_with_env(action, targeting, route, &[], test);
}

fn run_background_case_with_env(
    action: &str,
    targeting: Targeting,
    route: DriverRoute,
    env: &[(&str, &str)],
    test: impl FnOnce(u32, u64, &mut McpDriver),
) {
    run_case_with_env(
        native_background_case("appkit", action, targeting, route),
        env,
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

/// A heartbeat is not the receipt for a setup click. Delayed setup input must
/// be consumed before a background read enters its observation boundary.
#[test]
#[ignore]
fn harness_appkit_background_setup_waits_for_click_receipt() {
    run_case(
        native_background_case(
            "appkit",
            "setup_click_receipt",
            Targeting::Ax,
            DriverRoute::AxRead,
        ),
        |pid, wid, driver| {
            let target = TargetWindow {
                pid,
                native_id: wid,
            };
            let sentinel = cua_driver_testkit::sentinel::ForegroundSentinel::launch_with_env(
                driver,
                &[("CUA_E2E_SENTINEL_CLICK_RECEIPT_DELAY_MS", "600")],
            );
            sentinel
                .assert_background_posture(target)
                .expect("occluded target");
            driver.start_behavior_recording();
            sentinel
                .prepare_background_observation(driver, target)
                .expect("ready observation boundary");
            let (_, passed) = sentinel
                .observe_background(target, || {
                    let state = snapshot_elements(driver, pid, wid);
                    assert!(state.tree_text().contains("HARNESS_TEXT_MARKER_v1"));
                    assert!(state.tree_text().contains("counter=0"));
                })
                .expect("a read must not inherit the sentinel's setup click");
            Observation::delivered_with_fixture_state(passed)
        },
    );
}

/// Diagnostic negative control for focused background runs that do not enable
/// the full preflight's video capture. This deliberately changes foreground
/// focus and injects a key into the sentinel, so run only on a test desktop.
#[test]
#[ignore]
fn harness_appkit_background_guard_canary() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("appkit-background-guard-canary")
        .expect("start installed macOS daemon proxy");
    let harness = Harness::launch();
    let (wid, _) = driver
        .find_window(harness.pid as i64, "CuaTestHarness AppKit")
        .expect("find canary target window");
    let sentinel = cua_driver_testkit::sentinel::ForegroundSentinel::launch(&mut driver);
    sentinel
        .assert_guard_canaries(
            &mut driver,
            TargetWindow {
                pid: harness.pid,
                native_id: wid,
            },
        )
        .expect("observer must detect deliberate leaked input and native focus loss");
    println!(
        "Detected deliberate leaked input and native window blur; restored background posture."
    );
}

fn run_sequence_counter_case(action: &str, stop_after_first: bool) {
    run_background_case(action, DriverRoute::MacosAxAction, |pid, wid, driver| {
        let before = snapshot_elements(driver, pid, wid);
        let read_counter = |snapshot: &ToolResponse| -> u64 {
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
        assert_eq!(read_counter(&before), 0);
        let token = element_token_by_id(&before, "btn-increment");
        let step = |expected: u64| {
            serde_json::json!({
                "tool":"click", "arguments":{"element_token":token},
                "expect":[{"element":{
                    "selector":{"role":"AXStaticText","label_contains":"counter="},
                    "value_equals":format!("counter={expected}")
                }}],
                "timeout_ms":if stop_after_first { 250 } else { 1000 },
                "stable_samples":if stop_after_first { 1 } else { 2 }
            })
        };
        // Reusing the original token in the second step also checks that the
        // verification provider does not invalidate the action snapshot cache.
        let response = driver.call(
            "run_sequence",
            serde_json::json!({
                "pid":pid,"window_id":wid,
                "steps":[step(if stop_after_first { 99 } else { 1 }),step(2)]
            }),
        );
        assert!(!response.is_error(), "{}", response.raw);
        let output = response.structured();
        println!("sequence outcome={output}");
        // Read independently before assertions so failed runs retain the
        // fixture state as well as the executor's reported evidence.
        let after = snapshot_elements(driver, pid, wid);
        let actual_counter = read_counter(&after);
        println!("independent counter={actual_counter}");
        let steps = output["steps"]
            .as_array()
            .expect("attempted sequence steps");
        assert_eq!(steps.len(), if stop_after_first { 1 } else { 2 });
        assert_eq!(
            output["status"],
            if stop_after_first {
                "stopped"
            } else {
                "completed"
            }
        );
        if stop_after_first {
            assert_eq!(output["stopped_at"], 0);
            assert_eq!(output["stop_reason"], "unsatisfied");
            assert_eq!(steps[0]["verification"]["status"], "unsatisfied");
        } else {
            assert!(output["stopped_at"].is_null());
            assert!(output["stop_reason"].is_null());
            for step in steps {
                assert_eq!(step["verification"]["status"], "satisfied");
                assert_eq!(step["verification"]["stable"], true);
                assert!(step["verification"]["samples"].as_u64().unwrap() >= 2);
            }
        }
        for step in steps {
            assert_eq!(step["action"]["route"], "accessibility");
            assert_eq!(step["image_bytes_returned"], 0);
        }
        assert!(response.raw["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "image"));
        // The independent count catches a second mutation after a failed check.
        assert_eq!(actual_counter, if stop_after_first { 1 } else { 2 });
    });
}

#[test]
#[ignore = "signed local daemon and AppKit fixture required"]
fn harness_appkit_sequence_counter_completed() {
    run_sequence_counter_case("sequence_counter_completed", false);
}

#[test]
#[ignore = "signed local daemon and AppKit fixture required"]
fn harness_appkit_sequence_counter_stops() {
    run_sequence_counter_case("sequence_counter_stops", true);
}

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

            for raw in ["", "\n", " \tΩ café\n"] {
                let before = snapshot_elements(driver, pid, wid);
                let set = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid": pid as i64,
                        "window_id": wid,
                        "element_token": element_token_by_id(&before, "txt-input"),
                        "value": raw
                    }),
                );
                assert!(!set.is_error(), "set_value failed: {}", set.text());
                let after = snapshot_elements(driver, pid, wid);
                let index = element_index_by_id(after.tree_text(), "txt-input")
                    .expect("txt-input remains addressable");
                let field = after.structured()["elements"]
                    .as_array()
                    .and_then(|elements| {
                        elements
                            .iter()
                            .find(|element| element["element_index"].as_u64() == Some(index))
                    })
                    .expect("txt-input structured state");
                assert_eq!(field["value"], raw, "AXValue must remain lossless");
                assert_eq!(
                    field["placeholder"], "Type here…",
                    "placeholder must stay a hint, never the value"
                );
            }
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

/// A field whose `AXValue` catches up with the write over the next second is
/// not a partially typed field. The AX rung used to read the value back once,
/// microseconds after the write returned, and published the prefix it caught
/// as `type_text_incomplete` — measured in Contacts as "delivered 6 of 14"
/// for a phone number the card in fact held in full.
#[test]
#[ignore]
fn harness_appkit_type_text_waits_for_a_lagging_value_readback() {
    let trace_dir = tempfile::tempdir().expect("create lag trace directory");
    let trace_path = trace_dir.path().join("lag-readback.jsonl");
    run_background_case_with_env(
        "type_text_lagging_readback",
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[
            ("CUA_APPKIT_AX_VALUE_LAG_MS", "900"),
            ("CUA_APPKIT_AX_VALUE_TRACE", trace_path.to_str().unwrap()),
        ],
        |pid, wid, driver| {
            let snap_pre = snapshot_elements(driver, pid, wid);
            // Enter the field editor before testing AXSelectedText. An unfocused
            // NSTextField can reject that attribute and silently exercise the
            // already-drained keyboard route instead of this regression.
            let (x, y, w, h) = element_pixel_frame(&snap_pre, "txt-input");
            let focused = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "x": x + w / 2.0, "y": y + h / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !focused.is_error(),
                "focus field editor: {}",
                focused.text()
            );
            let snap_pre = snapshot_elements(driver, pid, wid);
            let idx = element_index_by_id(snap_pre.tree_text(), "txt-input")
                .expect("txt-input element_index not found");
            let text = "lagging-readback-cua";
            let resp = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_index": idx,
                    "snapshot_id": snap_pre.snapshot_id(),
                    "text": text, "delivery_mode": "background"
                }),
            );
            let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
            eprintln!("lag probe response: {}; getter trace: {trace}", resp.raw);
            assert!(
                resp.structured()["route"] == "accessibility" || resp.structured()["path"] == "ax",
                "lag regression must exercise the AXSelectedText route, got {}",
                resp.raw
            );
            assert!(
                trace.lines().any(|line| {
                    let row: serde_json::Value =
                        serde_json::from_str(line).expect("parse lag trace");
                    row["actual"] == text && row["reported"] != text
                }),
                "lag fixture did not expose delayed readback during typing"
            );
            assert!(
                !resp.is_error(),
                "a value the field was still publishing was reported as a failure: {}",
                resp.text()
            );
            assert_eq!(
                resp.structured()["delivery"]["delivered_count"],
                serde_json::json!(text.chars().count()),
                "type_text under-counted a complete insertion: {}",
                resp.raw
            );

            std::thread::sleep(Duration::from_millis(1200));
            let post = snapshot_elements(driver, pid, wid).tree_text().to_owned();
            assert!(
                post.contains(text),
                "the fixture never took the whole string:\n{post}"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_type_text_does_not_replay_an_unreadable_ax_write() {
    let trace_dir = tempfile::tempdir().expect("create readback trace directory");
    let trace_path = trace_dir.path().join("unreadable-readback.jsonl");
    run_background_case_with_env(
        "type_text_unreadable_readback",
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[
            ("CUA_APPKIT_AX_VALUE_UNREADABLE", "1"),
            ("CUA_APPKIT_AX_VALUE_TRACE", trace_path.to_str().unwrap()),
        ],
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, w, h) = element_pixel_frame(&pre, "txt-input");
            eprintln!(
                "unreadable-focus {}",
                serde_json::json!({
                    "phase": "before-click", "x": x + w / 2.0, "y": y + h / 2.0,
                    "at_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
                })
            );
            let focused = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "x": x + w / 2.0, "y": y + h / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(
                !focused.is_error(),
                "focus field editor: {}",
                focused.text()
            );
            let pre = snapshot_elements(driver, pid, wid);
            let index = element_index_by_id(pre.tree_text(), "txt-input").unwrap();
            let text = "one-insertion-cua";
            let response = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "element_index": index, "snapshot_id": pre.snapshot_id(),
                    "text": text, "delivery_mode": "background"
                }),
            );
            let _post = snapshot_elements(driver, pid, wid);
            let trace = std::fs::read_to_string(&trace_path).expect("fixture readback trace");
            eprintln!(
                "unreadable probe response: {}; getter trace: {trace}",
                response.raw
            );
            let rows: Vec<serde_json::Value> = trace
                .lines()
                .map(|line| serde_json::from_str(line).expect("parse fixture trace"))
                .collect();
            assert!(
                rows.iter()
                    .any(|row| row["actual"] == text && row["reported"].is_null()),
                "fixture must accept the complete text while its AX value is unreadable"
            );
            assert!(rows.iter().all(|row| row["actual"] == text),
                "one type_text request inserted additional text after an accepted AX write: {trace}");
            assert_eq!(
                response.structured()["route"],
                "accessibility",
                "{}",
                response.raw
            );
            assert_eq!(
                response.structured()["effect"],
                "unverifiable",
                "{}",
                response.raw
            );
            assert!(
                response.structured()["delivery"]
                    .get("delivered_count")
                    .is_none(),
                "unreadable state cannot prove a delivered count"
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

/// Verification can read a display-only label without publishing a click target
/// or replacing the action snapshot retained by this session.
#[test]
#[ignore]
fn harness_appkit_verify_display_text_preserves_action_snapshot() {
    run_background_case(
        "verify_display_text",
        DriverRoute::MacosAxAction,
        |pid, wid, driver| {
            let snapshot = driver.call(
                "get_window_state",
                serde_json::json!({
                    "pid": pid, "window_id": wid, "include_screenshot": false,
                    // Public ingress must strip this private provider flag.
                    "_observation_only": true
                }),
            );
            assert!(!snapshot.is_error(), "{}", snapshot.text());
            assert!(snapshot.tree_text().contains("counter=0"));
            assert!(snapshot.structured()["snapshot_id"].is_string());
            let public_elements = snapshot.structured()["elements"].as_array().unwrap();
            assert!(public_elements
                .iter()
                .all(|row| row["element_index"].is_u64()));
            assert!(!public_elements
                .iter()
                .any(|row| row["value"] == "counter=0"));
            let token = element_token_by_id(&snapshot, "btn-increment");

            for counter in 0..=2 {
                if counter > 0 {
                    let clicked = driver.call(
                        "click",
                        serde_json::json!({
                            "pid": pid, "window_id": wid, "element_token": token,
                            "delivery_mode": "background"
                        }),
                    );
                    assert!(
                        !clicked.is_error(),
                        "original token failed: {}",
                        clicked.text()
                    );
                }
                let verified = driver.call(
                    "verify_state",
                    serde_json::json!({
                        "pid": pid, "window_id": wid,
                        "expect": [{"element": {
                            "selector": {"role": "AXStaticText", "label_contains": "counter="},
                            "value_equals": format!("counter={counter}")
                        }}],
                        "timeout_ms": 1500, "stable_samples": 2, "include_screenshot": false
                    }),
                );
                println!("display verification {counter}: {}", verified.structured());
                assert!(!verified.is_error(), "{}", verified.text());
                assert_eq!(verified.structured()["status"], "satisfied");
                assert_eq!(verified.structured()["stable"], true);
                let observed: serde_json::Value = serde_json::from_str(
                    verified.structured()["predicates"][0]["observed_json"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(observed["value"], format!("counter={counter}"));
                assert!(observed.get("element_index").is_none());
                assert!(observed.get("element_token").is_none());
            }
            let wrong = driver.call(
                "verify_state",
                serde_json::json!({
                    "pid": pid, "window_id": wid,
                    "expect": [{"element": {
                        "selector": {"role": "AXStaticText", "label_contains": "counter="},
                        "value_equals": "counter=99"
                    }}], "timeout_ms": 0, "stable_samples": 1
                }),
            );
            assert_eq!(wrong.structured()["status"], "unsatisfied");
            let missing = driver.call("verify_state", serde_json::json!({
                "pid": pid, "window_id": wid,
                "expect": [{"element": {
                    "selector": {"role": "AXStaticText", "label_contains": "missing-test-label"},
                    "exists": true
                }}], "timeout_ms": 0, "stable_samples": 1
            }));
            assert_eq!(missing.structured()["status"], "unknown");
        },
    );
}

/// Manual agent boundary reuses the fixture and background oracles. The agent
/// receives only fixture identity and task text; an independent raw AX reader
/// checks the counter before and after the completed answer is published.
#[cfg(feature = "manual-agent-trials")]
#[test]
#[ignore = "external agent coordinator and raw AX observer required"]
fn harness_appkit_agent_counter_trial() {
    let artifacts = PathBuf::from(std::env::var("CUA_AGENT_TRIAL_DIR").expect("trial directory"));
    assert!(artifacts.is_absolute());
    assert!(!artifacts.exists(), "use a fresh trial directory");
    std::fs::create_dir_all(artifacts.join("workspace")).unwrap();
    let observer = PathBuf::from(std::env::var("CUA_AGENT_OBSERVER_BIN").expect("raw AX observer"));
    assert!(observer.is_absolute() && observer.is_file());
    // Agent targeting and backend are audited from its transcript, not prescribed here.
    run_background_case_targeting(
        "agent_counter",
        Targeting::NotApplicable,
        DriverRoute::Composite,
        |pid, wid, _driver| {
            let observe = |name: &str| {
                let out = Command::new(&observer)
                    .arg(pid.to_string())
                    .output()
                    .unwrap();
                std::fs::write(artifacts.join(format!("{name}.stderr")), &out.stderr).unwrap();
                assert!(out.status.success(), "independent observer failed");
                std::fs::write(artifacts.join(format!("{name}.json")), &out.stdout).unwrap();
                let state: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
                let counters: Vec<&str> = state["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| row["AXRole"] == "AXStaticText")
                    .filter_map(|row| row["AXValue"].as_str())
                    .filter(|value| value.starts_with("counter="))
                    .collect();
                assert_eq!(counters.len(), 1, "counter must be uniquely observable");
                counters[0].to_owned()
            };
            assert_eq!(observe("initial-state"), "counter=0");
            let identity =
                serde_json::json!({"app_pid": pid, "window_id": wid, "app_path": harness_app()});
            std::fs::write(
                artifacts.join("workspace/process.json"),
                identity.to_string(),
            )
            .unwrap();
            std::fs::write(artifacts.join("manual-ready.json"), identity.to_string()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(360);
            while !artifacts.join("manual-complete.json").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "agent completion deadline exceeded"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            let marker: serde_json::Value = serde_json::from_slice(
                &std::fs::read(artifacts.join("manual-complete.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                marker["exit_code"], 0,
                "agent did not complete successfully"
            );
            let answer: serde_json::Value =
                serde_json::from_slice(&std::fs::read(artifacts.join("answer.json")).unwrap())
                    .unwrap();
            let final_value = observe("final-state");
            assert_eq!(
                final_value, "counter=3",
                "agent answer cannot substitute for actual fixture state"
            );
            assert_eq!(answer["counter"], 3);
            assert_eq!(answer["completed"], true);
        },
    );
}

/// Click the increment button via element_index and verify the counter flips from 0 to 1.
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
    background_double_click("double_click", "double_click");
}

/// `click` with count two bypasses its single-click AX hit-test shortcut and
/// reaches the target-only synthetic-focus path. The standalone double_click
/// tool has a separate native dispatcher and does not exercise that scope.
#[test]
#[ignore]
fn harness_appkit_raw_double_click_px_background() {
    background_double_click("click", "raw_double_click");
}

fn background_double_click(tool: &str, action: &str) {
    run_background_case_targeting(
        action,
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
        |pid, wid, driver| {
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-clicktarget");
            let mut args = serde_json::json!({
                "pid": pid as i64,
                "window_id": wid,
                "x": x + width / 2.0,
                "y": y + height / 2.0,
                "delivery_mode": "background"
            });
            if tool == "click" {
                args["button"] = serde_json::json!("left");
                args["count"] = serde_json::json!(2);
            }
            let response = driver.call(tool, args);
            assert!(
                !response.is_error(),
                "AppKit double click failed: {}",
                response.text()
            );
            // MCP publishes ActionResult, which deliberately excludes private
            // producer diagnostics such as synthetic_target_focus. Verify its
            // public delivery facts and the independent receiver/sentinel below.
            assert_eq!(response.action_route(), Some("synthetic_events"));
            assert_eq!(response.action_delivery_mode(), Some("background"));
            assert_eq!(response.action_effect(), Some("unverifiable"));
            println!("{tool} response: {}", response.raw);
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
/// daemon_pid, candidate_binary_sha256, build_profile, and captures. Each capture
/// has display_id, bounds [x,y,w,h], scale, screenshot, screenshot_sha256, target
/// [global x,y], separate registry coordinates, a capture_log artifact, and
/// measured_tip_pixels [local x,y], independently annotated on that capture.
/// CUA_SLICE_A_CANDIDATE supplies build and launch artifacts. See the focused
/// Slice A section of docs/test-harnesses-guide.md for the complete schema.
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
    let oracles = vec![OracleKind::Pixels, OracleKind::Protocol];
    execute_case(
        native_readonly_case(
            "appkit",
            "slice_a_cursor_display_geometry",
            Targeting::Px,
            DriverRoute::MacosAxAction,
            oracles.clone(),
        ),
        |evidence| {
            evidence.log = std::env::var("CUA_SLICE_A_DISPLAY_EVIDENCE").ok();
            slice_a_verify_display_geometry();
            Observation::delivered(oracles, evidence.clone())
        },
    );
}

fn slice_a_verify_display_geometry() {
    let socket = slice_a_socket();
    let displays = slice_a_displays();
    let retina = displays
        .iter()
        .find(|d| (d.scale - 2.0).abs() < 0.01)
        .expect("strict precondition: attach an unmirrored 2x display");
    let secondary = displays.iter().find(|d| !d.primary && (d.scale-1.0).abs() < 0.01
        && (d.bounds[0] < 0.0 || d.bounds[1] < 0.0))
        .expect("strict precondition: attach an unmirrored 1x secondary display left of or above primary");
    let manifest_path = PathBuf::from(
        std::env::var("CUA_SLICE_A_DISPLAY_EVIDENCE")
            .expect("native mixed-display capture manifest required"),
    );
    let evidence = slice_a_json(&manifest_path);
    let mut driver = McpDriver::spawn_daemon_proxy_unrecorded(socket.to_str().unwrap())
        .expect("connect persistent MCP to candidate daemon");
    let candidate = slice_a_candidate(&mut driver);
    for key in [
        "candidate_sha",
        "candidate_binary_sha256",
        "daemon_pid",
        "build_profile",
    ] {
        assert_eq!(
            evidence[key], candidate[key],
            "display capture identity: {key}"
        );
    }
    let captures = evidence["captures"]
        .as_array()
        .expect("native captures required");
    assert_eq!(
        captures.len(),
        2,
        "exactly the required native 1x and 2x captures"
    );
    for display in [retina, secondary] {
        let matching: Vec<_> = captures
            .iter()
            .filter(|c| c["display_id"].as_u64() == Some(display.id as u64))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "one original capture per required physical display"
        );
        let c = matching[0];
        assert_eq!(c["bounds"], serde_json::json!(display.bounds));
        assert_eq!(c["scale"].as_f64(), Some(display.scale));
        let screenshot = slice_a_latency::Artifact {
            path: c["screenshot"]
                .as_str()
                .expect("native capture path")
                .into(),
            sha256: c["screenshot_sha256"]
                .as_str()
                .expect("native capture hash")
                .into(),
        };
        let image_path = slice_a_artifact(manifest_path.parent().unwrap(), &screenshot);
        let image = image::open(&image_path).expect("native full-display image");
        assert_eq!(
            [image.width(), image.height()],
            display.pixels,
            "native pixel size required"
        );
        let target: [f64; 2] = serde_json::from_value(c["target"].clone()).expect("global target");
        let registry: [f64; 2] = serde_json::from_value(c["registry"].clone())
            .expect("separate recorded registry coordinates");
        let tip: [f64; 2] = serde_json::from_value(c["measured_tip_pixels"].clone())
            .expect("independently annotated artwork tip");
        for axis in 0..2 {
            assert!(
                target[axis].is_finite()
                    && target[axis] >= display.bounds[axis]
                    && target[axis] < display.bounds[axis] + display.bounds[axis + 2]
            );
            assert!(registry[axis].is_finite() && (registry[axis] - target[axis]).abs() <= 1.0);
            assert!(
                tip[axis].is_finite()
                    && tip[axis] >= 0.0
                    && tip[axis] < display.pixels[axis] as f64
            );
        }
        let error = (tip[0] - (target[0] - display.bounds[0]) * display.scale)
            .hypot(tip[1] - (target[1] - display.bounds[1]) * display.scale);
        assert!(
            error <= 2.0,
            "painted arrow error {error} exceeds 2 native pixels"
        );
        let provenance: slice_a_latency::Artifact =
            serde_json::from_value(c["capture_log"].clone()).expect("capture session provenance");
        let log = slice_a_json(&slice_a_artifact(
            manifest_path.parent().unwrap(),
            &provenance,
        ));
        for key in [
            "candidate_sha",
            "candidate_binary_sha256",
            "daemon_pid",
            "build_profile",
        ] {
            assert_eq!(log[key], candidate[key]);
        }
        assert_eq!(log["screenshot_sha256"], screenshot.sha256);
        assert!(log["measurement_method"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()));
        eprintln!(
            "native display={} bounds={:?} scale={} arrow_error_px={error} image={}",
            display.id,
            display.bounds,
            display.scale,
            image_path.display()
        );
    }
}

// Task 14 uses external original frames for pixels, not get_agent_cursor_state.
// Capture phases deliberately remain failed until annotations are verified.
// See the guide for the two-phase protocol and the candidate provenance schema.

fn slice_a_output(command: &mut Command) -> String {
    let out = command.output().expect("run local evidence command");
    assert!(
        out.status.success(),
        "evidence command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn slice_a_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("required evidence file"))
        .expect("strict evidence JSON")
}

fn slice_a_write(path: &Path, value: &impl serde::Serialize) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap_or_else(|e| panic!("evidence must not be overwritten: {}: {e}", path.display()));
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
    file.sync_all().unwrap();
}

fn slice_a_artifact(root: &Path, artifact: &slice_a_latency::Artifact) -> PathBuf {
    assert!(
        slice_a_latency::hex_hash(&artifact.sha256),
        "required SHA-256"
    );
    let path = root.join(&artifact.path);
    assert!(
        path.is_file() && std::fs::metadata(&path).unwrap().len() > 0,
        "missing or empty {}",
        path.display()
    );
    assert_eq!(
        slice_a_file_sha256(&path),
        artifact.sha256,
        "artifact hash mismatch"
    );
    path
}

fn slice_a_verify_profile(evidence: &serde_json::Value, root: &Path) {
    let profile = evidence["build_profile"]
        .as_str()
        .expect("actual candidate build_profile");
    assert!(
        ["debug", "release"].contains(&profile),
        "unsupported build profile"
    );
    let log: slice_a_latency::Artifact =
        serde_json::from_value(evidence["build_log"].clone()).expect("build log provenance");
    let log = std::fs::read_to_string(slice_a_artifact(root, &log)).unwrap();
    let cargo_profile = if profile == "debug" { "dev" } else { "release" };
    assert!(
        log.contains(&format!("Finished `{cargo_profile}` profile")),
        "build log does not substantiate actual profile"
    );
    let sha = evidence["candidate_sha"].as_str().expect("candidate SHA");
    assert!(
        log.contains(&format!("CUA_DRIVER_SOURCE_SHA={sha}")),
        "build invocation must record embedded source SHA"
    );
    let launch: slice_a_latency::Artifact =
        serde_json::from_value(evidence["launch_log"].clone()).expect("launch provenance");
    let launch_path = slice_a_artifact(root, &launch);
    slice_a_latency::provenance::parse_launch(&std::fs::read(launch_path).unwrap())
        .expect("structured launch provenance");
}

fn slice_a_socket() -> PathBuf {
    let socket = PathBuf::from(
        std::env::var("CUA_E2E_MACOS_DAEMON_SOCKET")
            .expect("explicit dedicated CuaDriverLocal socket required"),
    );
    let local = PathBuf::from(std::env::var("HOME").unwrap())
        .join("Library/Caches/cua-driver-local/cua-driver-local.sock");
    assert_eq!(
        socket.canonicalize().unwrap(),
        local.canonicalize().unwrap(),
        "use the dedicated CuaDriverLocal socket"
    );
    assert!(
        std::env::var_os("CUA_TEST_DRIVER_BIN").is_some(),
        "explicit installed candidate executable required"
    );
    socket
}

fn slice_a_candidate(driver: &mut McpDriver) -> serde_json::Value {
    use std::ffi::{c_void, CStr};
    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }
    let path = PathBuf::from(
        std::env::var("CUA_SLICE_A_CANDIDATE").expect("candidate provenance JSON required"),
    );
    let mut meta = slice_a_json(&path);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let head = slice_a_output(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"]),
    );
    assert_eq!(meta["candidate_sha"].as_str(), Some(head.as_str()));
    assert!(
        slice_a_output(Command::new("git").arg("-C").arg(root).args([
            "status",
            "--porcelain",
            "--untracked-files=no"
        ]))
        .is_empty(),
        "clean tracked candidate required"
    );
    let socket = slice_a_socket();
    let pid = i32::try_from(meta["daemon_pid"].as_i64().expect("daemon PID")).unwrap();
    assert!(pid > 0);
    let pids = slice_a_output(Command::new("lsof").args(["-nP", "-t", "--"]).arg(&socket));
    assert!(
        pids.lines().any(|s| s.parse::<i32>() == Ok(pid)),
        "candidate PID must own dedicated socket"
    );
    let mut buffer = [0u8; 4096];
    assert!(unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) } > 0);
    let executable = PathBuf::from(
        unsafe { CStr::from_ptr(buffer.as_ptr().cast()) }
            .to_str()
            .unwrap(),
    );
    let binary = cua_driver_testkit::driver_binary();
    let hash = slice_a_file_sha256(&binary);
    let config = driver.call("get_config", serde_json::json!({}));
    assert!(!config.is_error(), "get_config: {}", config.raw);
    validate_slice_a_build_identity(
        &head,
        config.structured(),
        &slice_a_file_sha256(&executable),
        &hash,
        meta["candidate_binary_sha256"].as_str(),
    )
    .expect("running embedded SHA must match candidate");
    slice_a_verify_profile(&meta, path.parent().unwrap());
    let launch_artifact: slice_a_latency::Artifact =
        serde_json::from_value(meta["launch_log"].clone()).unwrap();
    let launch = slice_a_latency::provenance::parse_launch(
        &std::fs::read(slice_a_artifact(path.parent().unwrap(), &launch_artifact)).unwrap(),
    )
    .unwrap();
    slice_a_artifact(path.parent().unwrap(), &launch.transcript);
    let stderr_output =
        slice_a_output(Command::new("lsof").args(["-a", "-p", &pid.to_string(), "-d", "2", "-Fn"]));
    let stderr_paths: Vec<_> = stderr_output
        .lines()
        .filter_map(|s| s.strip_prefix('n'))
        .collect();
    assert_eq!(
        stderr_paths.len(),
        1,
        "candidate must retain stderr in one regular file"
    );
    let stderr = PathBuf::from(stderr_paths[0]).canonicalize().unwrap();
    let stderr_metadata = std::fs::metadata(&stderr).unwrap();
    assert!(stderr_metadata.is_file());
    let stderr_identity = slice_a_latency::provenance::FileIdentity::of(&stderr_metadata);
    slice_a_latency::provenance::validate_launch(
        &launch,
        pid,
        &executable.canonicalize().unwrap(),
        &hash,
        &socket.canonicalize().unwrap(),
        &stderr,
        &stderr_identity,
        false,
    )
    .unwrap();
    assert_eq!(
        launch.process_started,
        slice_a_output(Command::new("ps").env("LC_ALL", "C").args([
            "-p",
            &pid.to_string(),
            "-o",
            "lstart="
        ])),
        "launch record must identify this process lifetime"
    );
    assert!(
        launch.launched_epoch_ms <= slice_a_epoch_ms(),
        "launch timestamp is in the future"
    );
    // Inspect only the two diagnostic switches; never persist the process's
    // complete environment, which can contain unrelated credentials.
    let live_environment =
        slice_a_output(Command::new("ps").args(["eww", "-p", &pid.to_string(), "-o", "command="]));
    for name in ["CUA_LOG", "CUA_PRIVATE_CURSOR_ORDER_TRACE"] {
        let prefix = format!("{name}=");
        let values: Vec<_> = live_environment
            .split_whitespace()
            .filter_map(|v| v.strip_prefix(&prefix))
            .collect();
        assert_eq!(
            values,
            launch
                .environment
                .get(name)
                .map(|v| vec![v.as_str()])
                .unwrap_or_default(),
            "launch diagnostic environment disagrees with running candidate: {name}"
        );
    }
    meta["verified_launch"] = serde_json::to_value(&launch).unwrap();
    assert_eq!(
        config.structured()["experimental_pip"].as_bool(),
        Some(false),
        "candidate must have PiP disabled before focused acceptance"
    );
    // Signature and file hashes corroborate build/launch records. The runtime
    // embedded SHA, not a path to a replaced executable, identifies the source.
    slice_a_output(
        Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(&binary),
    );
    meta["get_config"] = config.structured().clone();
    meta["binary"] = serde_json::json!(binary);
    meta["socket"] = serde_json::json!(socket);
    meta["os"] = serde_json::json!(slice_a_output(&mut Command::new("sw_vers")));
    meta["displays"] = serde_json::to_value(slice_a_displays()).unwrap();
    meta
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceADisplay {
    id: u32,
    primary: bool,
    bounds: [f64; 4],
    scale: f64,
    pixels: [u32; 2],
}

fn slice_a_displays() -> Vec<SliceADisplay> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    struct Size {
        w: f64,
        h: f64,
    }
    #[repr(C)]
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
    let mut ids = [0u32; 32];
    let mut count = 0;
    assert_eq!(
        unsafe { CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut count) },
        0
    );
    assert!(count > 0 && count < 32);
    ids[..count as usize]
        .iter()
        .copied()
        .filter_map(|id| unsafe {
            if CGDisplayMirrorsDisplay(id) != 0 {
                return None;
            }
            let b = CGDisplayBounds(id);
            let mode = CGDisplayCopyDisplayMode(id);
            assert!(!mode.is_null());
            let w = CGDisplayModeGetWidth(mode);
            let h = CGDisplayModeGetHeight(mode);
            let pw = CGDisplayModeGetPixelWidth(mode);
            let ph = CGDisplayModeGetPixelHeight(mode);
            core_foundation::base::CFRelease(mode);
            assert!(w > 0 && h > 0 && pw > 0 && ph > 0);
            let scale = pw as f64 / w as f64;
            assert!((scale - ph as f64 / h as f64).abs() < 0.01);
            Some(SliceADisplay {
                id,
                primary: id == CGMainDisplayID(),
                bounds: [b.origin.x, b.origin.y, b.size.w, b.size.h],
                scale,
                pixels: [pw as u32, ph as u32],
            })
        })
        .collect()
}

fn slice_a_retina() -> SliceADisplay {
    slice_a_displays().into_iter().find(|d| (d.scale-2.0).abs() < 0.01)
        .expect("strict precondition: physical unmirrored 2x display; synthetic geometry cannot substitute")
}

struct SliceANativeTree(platform_macos::ax::TreeWalkResult);
impl std::ops::Deref for SliceANativeTree {
    type Target = platform_macos::ax::TreeWalkResult;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for SliceANativeTree {
    fn drop(&mut self) {
        // walk_tree transfers one retain for each actionable cache entry.
        for node in &self.0.nodes {
            if node.element_index.is_some() {
                unsafe {
                    core_foundation::base::CFRelease(
                        node.element_ptr as core_foundation::base::CFTypeRef,
                    );
                }
            }
        }
    }
}
fn slice_a_tree(pid: u32, wid: u64) -> SliceANativeTree {
    SliceANativeTree(platform_macos::ax::walk_tree(
        pid as i32,
        Some(wid.try_into().unwrap()),
        None,
    ))
}

fn slice_a_place_fixture(pid: u32, wid: u64, origin: [f64; 2]) {
    use platform_macos::ax::bindings::*;
    // Fixture setup only. Its fixed-size AppKit window is not AX-resizable.
    // Cursor movement and click mutations under test always use candidate MCP.
    let error = unsafe {
        let app = AXUIElementCreateApplication(pid as i32);
        assert!(!app.is_null());
        AXUIElementSetMessagingTimeout(app, 2.0);
        let windows = copy_ax_windows(app);
        let result = windows
            .iter()
            .copied()
            .find(|w| ax_get_window_id(*w) == Some(wid as u32))
            .map(|w| set_point_attr(w, "AXPosition", origin[0], origin[1]));
        for w in windows {
            core_foundation::base::CFRelease(w.cast());
        }
        core_foundation::base::CFRelease(app.cast());
        result
    };
    assert_eq!(
        error,
        Some(kAXErrorSuccess),
        "fixture position setup failed"
    );
    std::thread::sleep(Duration::from_millis(300));
    let b = platform_macos::windows::window_bounds_by_id(wid.try_into().unwrap()).unwrap();
    assert!(
        (b.x - origin[0]).abs() <= 1.0 && (b.y - origin[1]).abs() <= 1.0,
        "native fixture position differs from requested setup"
    );
}

fn slice_a_counter(pid: u32, wid: u64) -> u64 {
    let tree = slice_a_tree(pid, wid);
    assert!(!tree.truncated, "native AX oracle truncated");
    tree.tree_markdown
        .split("counter=")
        .nth(1)
        .expect("native counter oracle")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("native numeric counter")
}

fn slice_a_native_geometry(pid: u32, wid: u64) -> ([f64; 4], [f64; 4]) {
    let window = platform_macos::windows::window_bounds_by_id(wid.try_into().unwrap())
        .expect("independent CGWindow bounds");
    let tree = slice_a_tree(pid, wid);
    assert!(!tree.truncated);
    let control = tree
        .nodes
        .iter()
        .find(|n| n.identifier.as_deref() == Some("btn-increment"))
        .and_then(|n| n.frame)
        .expect("independent native AX button bounds");
    ([window.x, window.y, window.width, window.height], control)
}

fn slice_a_recording_off(driver: &mut McpDriver) {
    assert!(
        std::env::var_os("CUA_E2E_RECORDINGS_ROOT").is_none(),
        "no behavior recording during latency"
    );
    let state = driver.call("get_recording_state", serde_json::json!({}));
    assert!(!state.is_error());
    assert_eq!(
        state.structured()["enabled"].as_bool(),
        Some(false),
        "controller must stop recording before measurement"
    );
}

fn slice_a_cursor_position(driver: &mut McpDriver, session: &str, enabled: bool) -> [f64; 2] {
    let state = driver.call(
        "get_agent_cursor_state",
        serde_json::json!({"session":session}),
    );
    assert!(!state.is_error());
    assert_eq!(state.structured()["enabled"].as_bool(), Some(enabled));
    let config = driver.call("get_config", serde_json::json!({"session":session}));
    assert!(!config.is_error());
    slice_a_latency::validate_session_settings(state.structured(), config.structured())
        .expect("effective per-session settings");
    [
        state.structured()["position"]["x"]
            .as_f64()
            .expect("registry x"),
        state.structured()["position"]["y"]
            .as_f64()
            .expect("registry y"),
    ]
}

fn slice_a_configure_session(driver: &mut McpDriver, session: &str) -> serde_json::Value {
    let config = driver.call(
        "set_config",
        serde_json::json!({"session":session,"max_image_dimension":4096}),
    );
    assert!(
        !config.is_error(),
        "session capture override: {}",
        config.raw
    );
    let theme = driver.call(
        "set_agent_cursor_theme",
        serde_json::json!({"session":session,"theme_id":"cua.default","reduced_motion":"off"}),
    );
    assert!(!theme.is_error(), "session theme: {}", theme.raw);
    let config = driver.call("get_config", serde_json::json!({"session":session}));
    assert!(!config.is_error());
    for _ in 0..20 {
        let cursor = driver.call(
            "get_agent_cursor_state",
            serde_json::json!({"session":session}),
        );
        assert!(!cursor.is_error());
        if slice_a_latency::validate_session_settings(cursor.structured(), config.structured())
            .is_ok()
        {
            return serde_json::json!({"cursor":cursor.structured(),"config":config.structured()});
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("renderer did not report effective named-session theme off and capture dimension 4096");
}

fn slice_a_set_enabled(driver: &mut McpDriver, session: &str, enabled: bool) {
    let response = driver.call(
        "set_agent_cursor_enabled",
        serde_json::json!({"session":session,"enabled":enabled}),
    );
    assert!(!response.is_error(), "set overlay: {}", response.raw);
}

fn slice_a_capture_geometry(
    driver: &mut McpDriver,
    session: &str,
    pid: u32,
    wid: u64,
    display: &SliceADisplay,
    dimension: u32,
    path: &Path,
) -> slice_a_latency::Geometry {
    use base64::Engine;
    let snapshot = driver.call("get_window_state", serde_json::json!({"session":session,"pid":pid,"window_id":wid,"include_accessibility_tree":true,"include_screenshot":true,"max_dimension":dimension}));
    assert!(!snapshot.is_error());
    let content = snapshot.raw["result"]["content"]
        .as_array()
        .expect("actual MCP screenshot content");
    let encoded = content
        .iter()
        .find(|c| c["type"] == "image")
        .and_then(|c| c["data"].as_str())
        .expect("actual screenshot, not dimensions only");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let image = image::load_from_memory(&bytes).expect("decode actual window screenshot");
    std::fs::write(path, &bytes).unwrap();
    let (native_bounds, control_bounds) = slice_a_native_geometry(pid, wid);
    let (x, y, w, h) = element_pixel_frame(&snapshot, "btn-increment");
    let request = [x + w / 2.0, y + h / 2.0];
    let resize_ratio = native_bounds[2] * display.scale / image.width() as f64;
    slice_a_latency::Geometry {
        scale: display.scale,
        display_bounds: display.bounds,
        native_bounds,
        control_bounds,
        screenshot_size: [image.width(), image.height()],
        resize_ratio,
        request,
        registry: [0.0, 0.0],
    }
}

fn slice_a_epoch_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        * 1000.0
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceAStage {
    name: String,
    geometry: slice_a_latency::Geometry,
    screenshot: slice_a_latency::Artifact,
    response: slice_a_latency::Artifact,
    call_started_epoch_ms: f64,
    call_returned_epoch_ms: f64,
    counter_before: u64,
    counter_after: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceATrace {
    candidate: serde_json::Value,
    session: String,
    session_settings: serde_json::Value,
    pid: u32,
    window_id: u64,
    stages: Vec<SliceAStage>,
    passed_oracles: Vec<OracleKind>,
    invalid_frame_refused: bool,
}

fn slice_a_saved_artifact(path: &Path) -> slice_a_latency::Artifact {
    slice_a_latency::Artifact {
        path: path.file_name().unwrap().to_str().unwrap().into(),
        sha256: slice_a_file_sha256(path),
    }
}

fn slice_a_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CUA_SLICE_A_RUN_DIR").expect("unique CUA_SLICE_A_RUN_DIR required"),
    )
}

fn slice_a_visual_case(first: bool) {
    let name = if first {
        "slice_a_cursor_first_target"
    } else {
        "slice_a_cursor_window_geometry"
    };
    // This row observes overlay placement with the fixture visible. Background
    // delivery certification stays in the existing occluded AX-counter row.
    let required = vec![
        OracleKind::FixtureState,
        OracleKind::Pixels,
        OracleKind::Cursor,
        OracleKind::Protocol,
    ];
    let case = native_readonly_case(
        "appkit",
        name,
        Targeting::Px,
        DriverRoute::MacosAxAction,
        required.clone(),
    );
    execute_case(case, |evidence| {
        let dir = slice_a_dir();
        let phase = std::env::var("CUA_SLICE_A_PHASE").expect("capture or verify phase required");
        slice_a_socket();
        let mut driver =
            McpDriver::spawn_macos_daemon_proxy_named(name).expect("connect candidate proxy");
        let candidate = slice_a_candidate(&mut driver);
        let trace_path = dir.join("trace.json");
        evidence.log = Some(trace_path.to_string_lossy().into());
        if phase == "verify" {
            slice_a_verify_visual_trace(&dir, &candidate, first, true);
            return Observation::delivered(required, evidence.clone());
        }
        assert_eq!(phase, "capture");
        std::fs::create_dir(&dir).expect("capture requires a new run directory");
        let display = slice_a_retina();
        let fixture = Harness::launch();
        let (wid, _) = driver
            .find_window(fixture.pid as i64, "CuaTestHarness AppKit")
            .expect("fixture window");
        let session = format!(
            "task14-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let session_settings = slice_a_configure_session(&mut driver, &session);
        slice_a_set_enabled(&mut driver, &session, true);
        let before = driver.call(
            "get_agent_cursor_state",
            serde_json::json!({"session":session}),
        );
        assert!(
            !before.is_error() && before.structured()["position"].is_null(),
            "fresh session must have no prior positional target"
        );
        slice_a_write(
            &dir.join("ready.json"),
            &serde_json::json!({"candidate":candidate,"session":session,"pid":fixture.pid,"window_id":wid,"display":display,"ready_epoch_ms":slice_a_epoch_ms()}),
        );
        // Untimed only: allows an already-running external capture to include
        // a clean pre-action frame. No measured call includes this delay.
        std::thread::sleep(Duration::from_millis(500));
        let mut stages = Vec::new();
        let mut passed = Vec::new();
        let stage_defs: Vec<_> = if first {
            vec![("first-click", 4096, 0.0)]
        } else {
            vec![
                ("right-half", 4096, 0.0),
                ("moved", 4096, -50.0),
                ("resized", 600, -50.0),
            ]
        };
        for (stage, dimension, shift) in stage_defs {
            let [x, y, w, _h] = display.bounds;
            slice_a_place_fixture(fixture.pid, wid, [x + w / 2.0 + shift, y + 60.0]);
            let image = dir.join(format!("{stage}-window.png"));
            let mut geometry = slice_a_capture_geometry(
                &mut driver,
                &session,
                fixture.pid,
                wid,
                &display,
                dimension,
                &image,
            );
            if stage == "right-half" || first {
                assert!(
                    geometry.native_bounds[0] >= x + w / 2.0 - 1.0,
                    "fixture must occupy selected display right half"
                );
            }
            slice_a_latency::validate_capture_size(&geometry, dimension)
                .expect("actual capture size and scale");
            let counter_before = slice_a_counter(fixture.pid, wid);
            assert_eq!(
                counter_before, 0,
                "fresh fixture counter must remain zero before tested click"
            );
            let mut observer = cua_driver_testkit::observer::DesktopObserver::new(
                cua_driver_testkit::observer::NativeObserver::new(),
                TargetWindow {
                    pid: fixture.pid,
                    native_id: wid,
                },
            );
            let args = serde_json::json!({"session":session,"pid":fixture.pid,"window_id":wid,"x":geometry.request[0],"y":geometry.request[1],"delivery_mode":"background"});
            let mut move_args = args.clone();
            move_args.as_object_mut().unwrap().remove("delivery_mode");
            let tool = if first { "click" } else { "move_cursor" };
            let ((response, started, returned), delta) = observer
                .observe(&[OracleKind::Cursor], || {
                    let started = slice_a_epoch_ms();
                    let response = driver.call(
                        tool,
                        if first {
                            args.clone()
                        } else {
                            move_args.clone()
                        },
                    );
                    (response, started, slice_a_epoch_ms())
                })
                .expect("independent desktop cursor observer");
            delta.ensure_supported().unwrap();
            assert!(delta.violations().is_empty(), "{:?}", delta.violations());
            passed = delta.passed().to_vec();
            assert!(!response.is_error(), "target call: {}", response.raw);
            if first {
                assert_eq!(response.action_route(), Some("accessibility"));
            }
            // Readbacks and visual settling are outside the call timestamps.
            std::thread::sleep(Duration::from_millis(500));
            geometry.registry = slice_a_cursor_position(&mut driver, &session, true);
            slice_a_latency::parse_geometry(&serde_json::to_vec(&geometry).unwrap())
                .expect("strict native geometry");
            assert_eq!(
                slice_a_native_geometry(fixture.pid, wid),
                (geometry.native_bounds, geometry.control_bounds),
                "window/control moved during observation"
            );
            let counter_after = slice_a_counter(fixture.pid, wid);
            assert_eq!(counter_after, counter_before + u64::from(first));
            let response_path = dir.join(format!("{stage}-call.json"));
            slice_a_write(
                &response_path,
                &serde_json::json!({"tool":tool,"arguments":if first {args} else {move_args},"response":response.raw}),
            );
            stages.push(SliceAStage {
                name: stage.into(),
                geometry,
                screenshot: slice_a_saved_artifact(&image),
                response: slice_a_saved_artifact(&response_path),
                call_started_epoch_ms: started,
                call_returned_epoch_ms: returned,
                counter_before,
                counter_after,
            });
        }
        let mut invalid_frame_refused = false;
        if !first {
            // A nonexistent exact window is the native invalid-frame case.
            let before = slice_a_cursor_position(&mut driver, &session, true);
            let refused = driver.call("move_cursor",serde_json::json!({"session":session,"pid":fixture.pid,"window_id":u32::MAX,"x":10,"y":10}));
            assert!(refused.is_error(), "invalid exact window must refuse");
            assert_eq!(slice_a_cursor_position(&mut driver, &session, true), before);
            slice_a_write(&dir.join("invalid-frame.json"), &refused.raw);
            invalid_frame_refused = true;
        }
        slice_a_set_enabled(&mut driver, &session, false);
        let disabled_start = slice_a_epoch_ms();
        std::thread::sleep(Duration::from_millis(500));
        slice_a_write(
            &dir.join("disabled.json"),
            &serde_json::json!({"session":session,"start_epoch_ms":disabled_start,"end_epoch_ms":slice_a_epoch_ms(),"registry":slice_a_cursor_position(&mut driver,&session,false)}),
        );
        passed.extend([OracleKind::FixtureState, OracleKind::Protocol]);
        slice_a_write(
            &trace_path,
            &SliceATrace {
                candidate,
                session,
                session_settings,
                pid: fixture.pid,
                window_id: wid,
                stages,
                passed_oracles: passed,
                invalid_frame_refused,
            },
        );
        Observation::error("Native observations saved. Pixel acceptance is pending: annotate original external frames, then run this row with CUA_SLICE_A_PHASE=verify. Do not repeat capture to verify artifacts.",evidence.clone())
    });
}

#[test]
#[ignore = "controller-owned native 2x capture, then external artifact verification"]
fn slice_a_cursor_window_geometry() {
    slice_a_visual_case(false);
}

#[test]
#[ignore = "controller-owned fresh-session watchable first-click capture and frame annotations"]
fn slice_a_cursor_first_target() {
    slice_a_visual_case(true);
}

fn slice_a_verify_frames(root: &Path, v: &slice_a_latency::VisualEvidence) {
    let movie = slice_a_artifact(root, &v.video);
    let probe = slice_a_output(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_frames",
                "-show_entries",
                "frame=best_effort_timestamp_time,width,height",
                "-of",
                "json",
            ])
            .arg(&movie),
    );
    let probe: serde_json::Value =
        serde_json::from_str(&probe).expect("actual decoded video timing");
    let frames = probe["frames"].as_array().expect("decoded frames");
    let end = v
        .first_frame
        .checked_add(v.frames.len())
        .expect("frame range overflow");
    assert!(
        end <= frames.len(),
        "annotated range exceeds original video"
    );
    let frames = &frames[v.first_frame..end];
    let decoded = tempfile::tempdir().unwrap();
    slice_a_output(
        Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&movie)
            .args([
                "-map",
                "0:v:0",
                "-vf",
                &format!("select=between(n\\,{0}\\,{1})", v.first_frame, end - 1),
                "-fps_mode",
                "passthrough",
                "-start_number",
                &v.first_frame.to_string(),
            ])
            .arg(decoded.path().join("%06d.png")),
    );
    let mut gaps = Vec::new();
    for (i, (actual, annotation)) in frames.iter().zip(&v.frames).enumerate() {
        let pts: f64 = actual["best_effort_timestamp_time"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (pts - annotation.pts).abs() <= 0.000001,
            "PTS must come from source video"
        );
        assert_eq!(actual["width"].as_u64(), Some(v.pixel_size[0] as u64));
        assert_eq!(actual["height"].as_u64(), Some(v.pixel_size[1] as u64));
        let path = slice_a_artifact(root, &annotation.image);
        let annotated = image::open(path).unwrap().to_rgba8();
        let original = image::open(decoded.path().join(format!("{:06}.png", annotation.index)))
            .unwrap()
            .to_rgba8();
        assert!(
            annotated == original,
            "annotation frame must equal original decoded pixels"
        );
        if i > 0 {
            gaps.push(pts - v.frames[i - 1].pts);
        }
    }
    eprintln!("{} measured frame gaps seconds: {:?}", v.stage, gaps);
}

fn slice_a_verify_visual_trace(
    dir: &Path,
    candidate: &serde_json::Value,
    first: bool,
    require_ordering: bool,
) -> (SliceATrace, Option<slice_a_latency::PlaybackEvidence>) {
    let mut playback = None;
    let path = dir.join("trace.json");
    let trace: SliceATrace =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).expect("complete native trace");
    for key in [
        "candidate_sha",
        "candidate_binary_sha256",
        "build_profile",
        "daemon_pid",
        "socket",
        "displays",
        "os",
    ] {
        assert_eq!(
            trace.candidate[key], candidate[key],
            "capture candidate/environment mismatch: {key}"
        );
        assert!(!candidate[key].is_null());
    }
    assert!(!trace.session.is_empty() && trace.pid > 0 && trace.window_id > 0);
    slice_a_latency::validate_session_settings(
        &trace.session_settings["cursor"],
        &trace.session_settings["config"],
    )
    .expect("captured session configuration");
    let names: &[&str] = if first {
        &["first-click"]
    } else {
        &["right-half", "moved", "resized"]
    };
    assert_eq!(trace.stages.len(), names.len());
    assert_eq!(trace.invalid_frame_refused, !first);
    for oracle in [
        OracleKind::FixtureState,
        OracleKind::Cursor,
        OracleKind::Protocol,
    ] {
        assert!(
            trace.passed_oracles.contains(&oracle),
            "missing captured oracle {oracle:?}"
        );
    }
    for (stage, name) in trace.stages.iter().zip(names) {
        assert_eq!(&stage.name, name);
        assert!(
            stage.call_started_epoch_ms.is_finite()
                && stage.call_returned_epoch_ms.is_finite()
                && stage.call_started_epoch_ms > 0.0
                && stage.call_returned_epoch_ms > stage.call_started_epoch_ms
        );
        assert_eq!(stage.counter_after, stage.counter_before + u64::from(first));
        let g =
            slice_a_latency::parse_geometry(&serde_json::to_vec(&stage.geometry).unwrap()).unwrap();
        assert!((g.scale - 2.0).abs() < 0.01);
        slice_a_latency::validate_capture_size(&g, if *name == "resized" { 600 } else { 4096 })
            .unwrap();
        let screenshot = image::open(slice_a_artifact(dir, &stage.screenshot)).unwrap();
        assert_eq!([screenshot.width(), screenshot.height()], g.screenshot_size);
        slice_a_artifact(dir, &stage.response);
        let visual_path = dir.join(format!("{name}-visual.json"));
        let visual = slice_a_latency::parse_visual(
            &std::fs::read(&visual_path).expect("required external annotated frames"),
        )
        .unwrap();
        assert_eq!(&visual.stage, name);
        assert_eq!(visual.trace_sha256, slice_a_file_sha256(&path));
        assert_eq!(visual.scale, g.scale);
        slice_a_latency::validate_capture_window(
            &visual,
            stage.call_started_epoch_ms,
            stage.call_returned_epoch_ms,
        )
        .expect("capture clock and selected frame range");
        slice_a_verify_capture_log(dir, &visual, &trace);
        if first {
            assert_eq!(visual.crop_bounds,g.display_bounds,"first-placement proof requires the complete selected display; a crop cannot rule out an earlier arrow elsewhere");
            let onset = visual
                .frames
                .iter()
                .find(|f| !f.tip.is_empty())
                .expect("painted first frame");
            assert!(
                visual.video_started_epoch_ms + onset.pts * 1000.0 + visual.clock_uncertainty_ms
                    >= stage.call_started_epoch_ms,
                "arrow predates fresh session first positional call"
            );
        }
        let target = [
            g.control_bounds[0] + g.control_bounds[2] / 2.0,
            g.control_bounds[1] + g.control_bounds[3] / 2.0,
        ];
        slice_a_verify_frames(dir, &visual);
        if first {
            assert_eq!(
                visual.frames.first().unwrap().counter,
                Some(stage.counter_before)
            );
            assert_eq!(
                visual.frames.last().unwrap().counter,
                Some(stage.counter_after)
            );
            let result = slice_a_latency::validate_playback(&visual, target)
                .expect("quick approach frame evidence");
            eprintln!(
                "quick approach observed evidence: {}",
                serde_json::to_string(&result).unwrap()
            );
            if require_ordering {
                assert!(
                    matches!(
                        result.ordering,
                        slice_a_latency::OrderingEvidence::ArrivalBeforeCounter
                    ),
                    "indeterminate visual evidence: {}",
                    result.reason
                );
            }
            playback = Some(result);
        } else {
            assert!(
                visual
                    .frames
                    .iter()
                    .rev()
                    .take(3)
                    .all(|f| slice_a_latency::near(
                        &f.tip,
                        slice_a_latency::target_pixels(&visual, target)
                    )),
                "final painted tip within 2 native pixels"
            );
        }
    }
    if !first {
        assert_ne!(
            trace.stages[0].geometry.native_bounds,
            trace.stages[1].geometry.native_bounds
        );
        assert!(trace.stages[2].geometry.resize_ratio > 1.0);
        assert_eq!(
            slice_a_json(&dir.join("invalid-frame.json"))["result"]["isError"],
            true
        );
    }
    let disabled = slice_a_latency::parse_visual(
        &std::fs::read(dir.join("disabled-visual.json"))
            .expect("untimed disabled-overlay frames required"),
    )
    .unwrap();
    assert_eq!(disabled.stage, "disabled");
    assert_eq!(disabled.trace_sha256, slice_a_file_sha256(&path));
    slice_a_verify_capture_log(dir, &disabled, &trace);
    slice_a_latency::provenance::validate_disabled_coverage(
        disabled.crop_bounds,
        disabled.scale,
        trace.stages[0].geometry.display_bounds,
        trace.stages[0].geometry.scale,
    )
    .expect("disabled coverage");
    let disabled_interval = slice_a_json(&dir.join("disabled.json"));
    assert_eq!(disabled_interval["session"], trace.session);
    let off_start = disabled_interval["start_epoch_ms"].as_f64().unwrap();
    let off_end = disabled_interval["end_epoch_ms"].as_f64().unwrap();
    assert!(off_start.is_finite() && off_end.is_finite() && off_end - off_start >= 500.0);
    assert!(disabled.video_started_epoch_ms + disabled.frames[0].pts * 1000.0 >= off_start + 50.0);
    assert!(
        disabled.video_started_epoch_ms + disabled.frames.last().unwrap().pts * 1000.0 <= off_end
    );
    slice_a_verify_frames(dir, &disabled);
    assert!(disabled.frames.last().unwrap().pts - disabled.frames[0].pts >= 0.3);
    assert!(
        disabled
            .frames
            .iter()
            .all(|f| f.tip.is_empty() && f.pulse_center.is_empty()),
        "disabled session must have no painted arrow or pulse"
    );
    (trace, playback)
}

#[test]
#[ignore = "controller-owned signed release candidate, completed untimed visual evidence, no active recording"]
fn slice_a_cursor_latency() {
    let case = native_background_case(
        "appkit",
        "slice_a_cursor_latency",
        Targeting::Px,
        DriverRoute::MacosAxAction,
    );
    execute_case(case, |evidence| {
        assert_eq!(
            std::env::var("CUA_SLICE_A_PHASE").as_deref(),
            Ok("measure"),
            "latency requires explicit measure phase, never recapture during verify"
        );
        let socket = slice_a_socket();
        let mut driver = McpDriver::spawn_daemon_proxy_unrecorded(socket.to_str().unwrap())
            .expect("persistent unrecorded candidate MCP");
        let candidate = slice_a_candidate(&mut driver);
        assert_eq!(
            candidate["build_profile"], "release",
            "debug visual milestones are not final latency evidence"
        );
        slice_a_recording_off(&mut driver);
        let approach_log = PathBuf::from(
            std::env::var("CUA_SLICE_A_APPROACH_LOG")
                .expect("candidate private timing log path required"),
        );
        let launch: slice_a_latency::provenance::LaunchRecord =
            serde_json::from_value(candidate["verified_launch"].clone()).unwrap();
        assert_eq!(
            approach_log.canonicalize().unwrap(),
            launch.stderr,
            "timing must read this candidate's actual stderr"
        );
        slice_a_latency::provenance::validate_launch(
            &launch,
            launch.daemon_pid,
            &launch.executable,
            &launch.executable_sha256,
            &launch.socket,
            &launch.stderr,
            &launch.stderr_identity,
            true,
        )
        .unwrap();
        let visual_dir = PathBuf::from(
            std::env::var("CUA_SLICE_A_PREFLIGHT_DIR")
                .expect("completed separate first-target visual evidence required"),
        );
        let (preflight, preflight_ordering) =
            slice_a_verify_visual_trace(&visual_dir, &candidate, true, false);
        // Driver state independently guards behavior recording. External capture
        // must also have stopped, as attested by the live-acceptance controller.
        assert_eq!(
            std::env::var("CUA_SLICE_A_EXTERNAL_CAPTURE_STOPPED").as_deref(),
            Ok("1")
        );
        let dir = slice_a_dir();
        std::fs::create_dir(&dir)
            .expect("latency run directory must be new; no automatic benchmark retries");
        let report_path = dir.join("latency-report.json");
        evidence.log = Some(report_path.to_string_lossy().into());
        slice_a_write(&dir.join("candidate.json"), &candidate);
        slice_a_write(
            &dir.join("preflight.json"),
            &serde_json::json!({"directory":visual_dir,"trace_sha256":slice_a_file_sha256(&visual_dir.join("trace.json"))}),
        );
        let fixture = Harness::launch();
        let (wid, _) = driver
            .find_window(fixture.pid as i64, "CuaTestHarness AppKit")
            .expect("fixture main window");
        let mode: slice_a_latency::TimingMode =
            serde_json::from_value(serde_json::json!(std::env::var("CUA_SLICE_A_TIMING_MODE")
                .expect("same_point or different_target required")))
            .expect("valid timing mode");
        let session = format!(
            "task14-timing-{}-{}",
            std::process::id(),
            slice_a_epoch_ms()
        );
        let session_settings = slice_a_configure_session(&mut driver, &session);
        let previous = &preflight.stages[0].geometry;
        let [x, y, _, _] = previous.native_bounds;
        slice_a_place_fixture(fixture.pid, wid, [x, y]);
        let mut geometry = slice_a_capture_geometry(
            &mut driver,
            &session,
            fixture.pid,
            wid,
            &slice_a_retina(),
            4096,
            &dir.join("window.png"),
        );
        assert_eq!(
            geometry.native_bounds, previous.native_bounds,
            "preflight and timing must use same geometry"
        );
        assert_eq!(geometry.control_bounds, previous.control_bounds);
        slice_a_latency::validate_capture_size(&geometry, 4096)
            .expect("timing uses unresized native screenshot");
        let mut samples = slice_a_latency::Samples {
            mode,
            warmups: Vec::new(),
            pairs: Vec::new(),
        };
        // Existing sentinel uses DesktopObserver and the leaked input journal.
        // This unrecorded proxy makes its recording-start call a no-op.
        let (_, passed) = run_with_background_oracles(
            &mut driver,
            TargetWindow {
                pid: fixture.pid,
                native_id: wid,
            },
            |driver| {
                slice_a_recording_off(driver);
                for i in 0..10 {
                    let ns = slice_a_timed_click(
                        driver,
                        fixture.pid,
                        wid,
                        &session,
                        mode,
                        i % 2 == 0,
                        &mut geometry,
                        &dir,
                        format!("warmup-{i:02}"),
                    );
                    samples.warmups.push(slice_a_latency::Warmup {
                        enabled: i % 2 == 0,
                        ns,
                    });
                }
                for block in 0..3 {
                    for pair in 0..30 {
                        let enabled_first = block % 2 == 0;
                        let mut times = [0.0; 2];
                        for enabled in [enabled_first, !enabled_first] {
                            let ns = slice_a_timed_click(
                                driver,
                                fixture.pid,
                                wid,
                                &session,
                                mode,
                                enabled,
                                &mut geometry,
                                &dir,
                                format!(
                                    "block-{block}-pair-{pair:02}-{}",
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                            );
                            times[usize::from(enabled)] = ns;
                        }
                        samples.pairs.push(slice_a_latency::Pair {
                            block,
                            pair,
                            enabled_first,
                            enabled_ns: times[1],
                            disabled_ns: times[0],
                        });
                    }
                    slice_a_write(
                        &dir.join(format!("samples-through-block-{block}.json")),
                        &samples,
                    );
                }
                slice_a_recording_off(driver);
            },
        )
        .expect("background fixture, foreground, z-order, real cursor and leaked-input oracles");
        slice_a_set_enabled(&mut driver, &session, false);
        slice_a_write(&dir.join("samples.json"), &samples);
        let report =
            slice_a_latency::latency_report(&samples).expect("strict complete paired samples");
        slice_a_write(
            &report_path,
            &serde_json::json!({"calculation":report,"candidate":candidate,"session":session,"pid":fixture.pid,"window_id":wid,
            "geometry":geometry,"preflight_ordering":preflight_ordering,"session_settings":session_settings,"passed_oracles":passed,"recording":false,"external_capture_stopped":true,
            "overlay_conditions":[true,false],"pip":false,"measurement":"Instant immediately before MCP call until returned ToolResponse; toggles, observations and file IO outside interval"}),
        );
        // Successful execution certifies the fixture/oracles and complete data,
        // not a performance threshold. Intentional approach is part of RPC time.
        Observation::delivered_with_fixture_state(passed)
    });
}

fn slice_a_timed_click(
    driver: &mut McpDriver,
    pid: u32,
    wid: u64,
    session: &str,
    mode: slice_a_latency::TimingMode,
    enabled: bool,
    geometry: &mut slice_a_latency::Geometry,
    dir: &Path,
    label: String,
) -> f64 {
    slice_a_set_enabled(driver, session, enabled);
    let setup = match mode {
        slice_a_latency::TimingMode::SamePoint => geometry.request,
        slice_a_latency::TimingMode::DifferentTarget => [
            geometry.request[0] + 100.0 * geometry.scale / geometry.resize_ratio,
            geometry.request[1],
        ],
    };
    assert!(
        setup[0] < geometry.screenshot_size[0] as f64,
        "different target must stay in window screenshot"
    );
    let moved = driver.call(
        "move_cursor",
        serde_json::json!({"session":session,"pid":pid,"window_id":wid,"x":setup[0],"y":setup[1]}),
    );
    assert!(!moved.is_error(), "timing setup move: {}", moved.raw);
    // Untimed setup pacing. A same-point request is not proof of physical arrival;
    // approach logs and separate frame evidence describe the actual acknowledgement.
    std::thread::sleep(Duration::from_millis(400));
    slice_a_recording_off(driver);
    assert_eq!(
        slice_a_native_geometry(pid, wid),
        (geometry.native_bounds, geometry.control_bounds)
    );
    let before = slice_a_counter(pid, wid);
    let args = serde_json::json!({"session":session,"pid":pid,"window_id":wid,"x":geometry.request[0],"y":geometry.request[1],"delivery_mode":"background"});
    let log_path = PathBuf::from(std::env::var("CUA_SLICE_A_APPROACH_LOG").unwrap());
    let candidate_path = PathBuf::from(std::env::var("CUA_SLICE_A_CANDIDATE").unwrap());
    let candidate = slice_a_json(&candidate_path);
    let launch_artifact: slice_a_latency::Artifact =
        serde_json::from_value(candidate["launch_log"].clone()).unwrap();
    let launch = slice_a_latency::provenance::parse_launch(
        &std::fs::read(slice_a_artifact(
            candidate_path.parent().unwrap(),
            &launch_artifact,
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(log_path.canonicalize().unwrap(), launch.stderr);
    let log_slice = slice_a_latency::provenance::LogSlice::open(&log_path, &launch.stderr_identity)
        .expect("same retained daemon stderr");
    let log_start = log_slice.start;
    let log_identity = log_slice.identity.clone();
    let epoch = slice_a_epoch_ms();
    let start = std::time::Instant::now();
    let response = driver.call("click", args.clone());
    let ns = start.elapsed().as_nanos() as f64;
    let returned_epoch_ms = slice_a_epoch_ms();
    let bracket = slice_a_latency::provenance::RpcBracket {
        started_epoch_ms: epoch,
        returned_epoch_ms,
        elapsed_ms: ns / 1_000_000.0,
    };
    let (log, log_end) = log_slice
        .finish()
        .expect("preserved stderr descriptor and path identity across RPC/read");
    let log_artifact = dir.join(format!("{label}-approach.log"));
    std::fs::write(&log_artifact, &log).unwrap();
    slice_a_write(
        &dir.join(format!("{label}.json")),
        &serde_json::json!({"arguments":args,"response":response.raw,"elapsed_ns":ns,"started_epoch_ms":epoch,"returned_epoch_ms":returned_epoch_ms,"enabled":enabled,"setup_target_pixels":setup,"timing_mode":mode}),
    );
    assert!(!response.is_error(), "timed click failed: {}", response.raw);
    let approach = slice_a_latency::parse_approach_timing(&log, bracket, enabled)
        .expect("complete per-action private timing evidence");
    slice_a_write(
        &dir.join(format!("{label}-timing.json")),
        &serde_json::json!({"approach":approach,"rpc_bracket":bracket,"stderr_identity":log_identity,"log":log_path,"byte_range":[log_start,log_end],"sha256":slice_a_file_sha256(&log_artifact),"rpc_ms":ns/1_000_000.0}),
    );
    assert_eq!(response.action_route(), Some("accessibility"));
    assert_eq!(
        slice_a_counter(pid, wid),
        before + 1,
        "exactly one native fixture counter increment"
    );
    geometry.registry = slice_a_cursor_position(driver, session, enabled);
    slice_a_latency::parse_geometry(&serde_json::to_vec(&*geometry).unwrap()).unwrap();
    assert_eq!(
        slice_a_native_geometry(pid, wid),
        (geometry.native_bounds, geometry.control_bounds)
    );
    slice_a_recording_off(driver);
    let cursor = driver.call(
        "get_agent_cursor_state",
        serde_json::json!({"session":session}),
    );
    assert!(!cursor.is_error());
    slice_a_write(
        &dir.join(format!("{label}-oracles.json")),
        &serde_json::json!({"counter_before":before,"counter_after":before+1,"geometry":geometry,"cursor_registry_and_configuration":cursor.raw}),
    );
    ns
}

fn slice_a_verify_capture_log(
    dir: &Path,
    visual: &slice_a_latency::VisualEvidence,
    trace: &SliceATrace,
) {
    let log = slice_a_json(&slice_a_artifact(dir, &visual.capture_log));
    assert_eq!(log["candidate_sha"], trace.candidate["candidate_sha"]);
    assert_eq!(log["daemon_pid"], trace.candidate["daemon_pid"]);
    assert_eq!(log["session"], trace.session);
    assert_eq!(log["video_sha256"], visual.video.sha256);
    assert_eq!(
        log["video_started_epoch_ms"].as_f64(),
        Some(visual.video_started_epoch_ms)
    );
    assert_eq!(
        log["clock_uncertainty_ms"].as_f64(),
        Some(visual.clock_uncertainty_ms)
    );
    assert_eq!(log["crop_bounds"], serde_json::json!(visual.crop_bounds));
    assert_eq!(log["pixel_size"], serde_json::json!(visual.pixel_size));
    assert_eq!(log["scale"].as_f64(), Some(visual.scale));
    let command = log["capture_command"]
        .as_array()
        .expect("exact external capture invocation");
    assert!(
        !command.is_empty()
            && command
                .iter()
                .all(|a| a.as_str().is_some_and(|s| !s.trim().is_empty()))
    );
    assert!(
        log["clock_alignment_method"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "document epoch-to-video clock alignment"
    );
    let display = trace.stages[0].geometry.display_bounds;
    for axis in 0..2 {
        assert!(
            visual.crop_bounds[axis] >= display[axis]
                && visual.crop_bounds[axis] + visual.crop_bounds[axis + 2]
                    <= display[axis] + display[axis + 2],
            "capture must be within observed physical display"
        );
    }
}
