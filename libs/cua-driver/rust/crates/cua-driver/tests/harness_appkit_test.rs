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
use cua_driver_testkit::observer::{NativeObserver, ObserverBackend, TargetWindow};
use cua_driver_testkit::sentinel::run_with_background_oracles;
use cua_driver_testkit::{Driver, McpDriver, ToolResponse};

#[path = "support/appkit_snapshot_publication.rs"]
mod snapshot_publication;

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
            "capture_mode": "ax",
            // Tests look rows up by id on every read; a change-only diff has
            // no rows to search.
            "diff": false
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

/// One act_and_read call: the click lands once and the returned read already
/// shows its effect, with no second call.
#[test]
#[ignore = "signed local daemon and AppKit fixture required"]
fn harness_appkit_act_and_read_click_shows_effect() {
    run_background_case("act_and_read_click", DriverRoute::MacosAxAction, |pid, wid, driver| {
        let before = snapshot_elements(driver, pid, wid);
        assert!(before.tree_text().contains("counter=0"));
        let token = element_token_by_id(&before, "btn-increment");
        let response = driver.call(
            "act_and_read",
            serde_json::json!({"pid":pid,"window_id":wid,"action":"click","element_token":token}),
        );
        assert!(!response.is_error(), "{}", response.raw);
        let output = response.structured();
        println!("act_and_read timings={}", output["timings"]);
        assert_ne!(output["action"]["isError"], true, "{}", response.raw);
        let tree = output["observation"]["structuredContent"]["tree_markdown"]
            .as_str()
            .expect("observation tree");
        assert!(tree.contains("counter=1"), "read must show the click: {tree}");
        // Independent read: exactly one click landed.
        assert!(snapshot_elements(driver, pid, wid).tree_text().contains("counter=1"));
    });
}

/// A click that opens a window: the same response reports it as a surface
/// note, so the agent can rebind without another read.
#[test]
#[ignore = "signed local daemon and AppKit fixture required"]
fn harness_appkit_act_and_read_reports_new_window() {
    run_background_case_with_env(
        "act_and_read_new_window",
        Targeting::Ax,
        DriverRoute::MacosAxAction,
        &[("CUA_APPKIT_OPENER", "1")],
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let token = element_token_by_id(&before, "btn-open-window");
            let response = driver.call(
                "act_and_read",
                serde_json::json!({"pid":pid,"window_id":wid,"action":"click","element_token":token}),
            );
            assert!(!response.is_error(), "{}", response.raw);
            let output = response.structured();
            let change = &output["observation"]["structuredContent"]["window_change"];
            println!("act_and_read window_change={change}");
            let titles: Vec<_> = change["new_windows"]
                .as_array()
                .unwrap_or_else(|| panic!("no window_change in the read: {}", response.raw))
                .iter()
                .filter_map(|w| w["title"].as_str())
                .collect();
            assert!(titles.contains(&"CuaTestHarness Opened"), "{titles:?}");
            assert_eq!(change["rebind"]["title"], "CuaTestHarness Opened");
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_exact_activation_with_agent_cursor() {
    let mut case = native_foreground_case(
        "appkit",
        "exact_activation_with_agent_cursor",
        Targeting::NotApplicable,
        DriverRoute::WindowState,
    );
    case.oracles.extend([OracleKind::Focus, OracleKind::Cursor]);
    run_case(case, |pid, wid, driver| {
        let snapshot = snapshot_elements(driver, pid, wid);
        assert!(!snapshot.is_error(), "snapshot: {}", snapshot.text());
        let target = TargetWindow {
            pid,
            native_id: wid,
        };
        let observer = NativeObserver::new();
        let before = observer.snapshot(target).expect("observe native desktop");
        let socket = std::env::var("CUA_E2E_MACOS_DAEMON_SOCKET")
            .expect("canonical installed daemon socket");
        let mut peer = McpDriver::spawn_daemon_proxy_unrecorded(&socket)
            .expect("start concurrent cursor session");
        let verifies_target = |response: &ToolResponse| {
            let state = response.structured();
            !response.is_error()
                && state["activated"] == true
                && state["observed"]["focused_window_id"].as_u64() == Some(wid)
                && state["observed"]["frontmost_ordinary_window_id"].as_u64() == Some(wid)
                && state["observed"]["frontmost_pid"].as_u64() == Some(u64::from(pid))
        };
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let activated = std::thread::scope(|scope| {
            let moving = scope.spawn(|| {
                let snapshot = snapshot_elements(&mut peer, pid, wid);
                assert!(!snapshot.is_error(), "peer snapshot: {}", snapshot.text());
                let motion = peer.call(
                    "set_agent_cursor_motion",
                    serde_json::json!({"idle_hide_ms": 0, "glide_duration_ms": 0}),
                );
                assert!(!motion.is_error(), "cursor motion: {}", motion.text());
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                let mut first = true;
                let mut x = 120;
                while !stopped.load(std::sync::atomic::Ordering::Relaxed)
                    && std::time::Instant::now() < deadline
                {
                    let moved = peer.call(
                        "move_cursor",
                        serde_json::json!({
                            "target": {"kind": "window", "pid": pid, "window_id": wid},
                            "x": x,
                            "y": 100
                        }),
                    );
                    assert!(!moved.is_error(), "agent cursor: {}", moved.text());
                    if first {
                        ready.send(()).expect("cursor readiness");
                        first = false;
                    }
                    x = if x == 120 { 121 } else { 120 };
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(
                    stopped.load(std::sync::atomic::Ordering::Relaxed),
                    "cursor producer expired before the activation interval completed"
                );
            });
            started
                .recv_timeout(Duration::from_secs(15))
                .expect("live cursor ready");
            let mut result = driver.call(
                "bring_to_front",
                serde_json::json!({"pid": pid, "window_id": wid}),
            );
            for _ in 1..20 {
                if !verifies_target(&result) {
                    break;
                }
                result = driver.call(
                    "bring_to_front",
                    serde_json::json!({"pid": pid, "window_id": wid}),
                );
            }
            stopped.store(true, std::sync::atomic::Ordering::Relaxed);
            moving.join().expect("concurrent cursor transport");
            result
        });
        assert!(
            verifies_target(&activated),
            "active agent cursor must not invalidate exact activation: {}",
            activated.raw
        );
        let after = observer.snapshot(target).expect("observe activated target");
        assert_eq!(after.foreground, Some(u64::from(pid)));
        assert_eq!(after.cursor_pos, before.cursor_pos, "real pointer moved");
        Observation::delivered_with_fixture_state(vec![OracleKind::Focus, OracleKind::Cursor])
    });
}

#[test]
#[ignore]
fn harness_appkit_exact_activation_refuses_competing_window() {
    let mut case = native_foreground_case(
        "appkit",
        "exact_activation_competing_window",
        Targeting::NotApplicable,
        DriverRoute::WindowState,
    )
    .expecting_refusal(vec![RefusalCode::BringToFrontExactWindowUnverified]);
    case.oracles.push(OracleKind::Cursor);
    run_case(case, |pid, wid, driver| {
        let competitor = Harness::launch_with(None, None, &[("CUA_APPKIT_KEEP_ORDERED_FRONT", "1")]);
        let (competing_wid, _) = driver
            .find_window(competitor.pid as i64, "CuaTestHarness AppKit")
            .expect("find competing ordinary window");
        let snapshot = snapshot_elements(driver, pid, wid);
        assert!(!snapshot.is_error(), "target snapshot: {}", snapshot.text());
        let observer = NativeObserver::new();
        let target = TargetWindow {
            pid,
            native_id: wid,
        };
        let before = observer.snapshot(target).expect("observe competing window");
        let response = driver.call(
            "bring_to_front",
            serde_json::json!({"pid": pid, "window_id": wid}),
        );
        assert!(
            response.is_error(),
            "competing window must prevent verification"
        );
        assert_eq!(
            response.structured()["code"],
            "bring_to_front_exact_window_unverified"
        );
        assert_eq!(response.structured()["activated"], false);
        assert_eq!(response.structured()["process_activated"], true);
        assert_eq!(
            response.structured()["exact_window_effect"]["focused"],
            true
        );
        assert_eq!(
            response.structured()["observed"]["frontmost_ordinary_window_id"].as_u64(),
            Some(competing_wid)
        );
        let after = observer
            .snapshot(target)
            .expect("observe refused activation");
        assert_eq!(after.cursor_pos, before.cursor_pos, "real pointer moved");
        Observation::refused(
            RefusalCode::BringToFrontExactWindowUnverified,
            vec![OracleKind::FixtureState, OracleKind::Cursor],
            response.text(),
            Evidence::default(),
        )
    });
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
            // Window-scoped reads exclude the application menu bar; menus are
            // reached through invoke_menu, which reads the live menu bar.
            assert!(
                !text.contains("Harness Test Item"),
                "a window read must not include application menu items"
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
            assert!(first.tree_text().contains("counter=0"));
            let token = element_token_by_id(&first, "btn-increment");
            let index = element_index_by_id(first.tree_text(), "btn-increment").unwrap();
            let newer = snapshot_elements(driver, pid, wid);
            assert!(
                !newer.is_error(),
                "replacement read failed: {}",
                newer.text()
            );
            assert_ne!(first.snapshot_id(), newer.snapshot_id());
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
            let refused_index = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "snapshot_id": first.snapshot_id(),
                    "element_index": index
                }),
            );
            assert!(
                refused_index.is_error(),
                "stale snapshot/index was accepted"
            );
            assert_eq!(
                refused_index.structured()["refusal"]["code"].as_str(),
                Some("stale_element_token")
            );
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("counter=0"),
                "stale targeting mutated counter"
            );
            let fresh_token = element_token_by_id(&post, "btn-increment");
            let delivered = driver.call(
                "click",
                serde_json::json!({"pid": pid as i64, "element_token": fresh_token}),
            );
            assert!(
                !delivered.is_error(),
                "fresh recovery failed: {}",
                delivered.text()
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let recovered = snapshot_elements(driver, pid, wid);
                if recovered.tree_text().contains("counter=1") {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fresh recovery did not increment exactly once: {}",
                    recovered.tree_text()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
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

/// A text control does not advertise `AXPress`, so a click used to dispatch
/// one anyway and report `-25206` plus "Action may have been a no-op" on the
/// route that works. A click on a text role means "put the caret here": the
/// proof is that the next unaddressed `type_text` lands in that field.
#[test]
#[ignore]
fn harness_appkit_click_on_a_text_role_focuses_it() {
    run_background_case(
        "click_text_focus",
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let clicked = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "element_token": element_token_by_id(&before, "txt-input")
                }),
            );
            assert!(!clicked.is_error(), "click failed: {}", clicked.text());
            assert!(
                !clicked.text().contains("does not advertise"),
                "a text role still had an AXPress dispatched at it: {}",
                clicked.text()
            );
            assert_eq!(
                clicked.action_effect(),
                Some("confirmed"),
                "focusing a text control is read-back verifiable: {}",
                clicked.raw
            );

            let typed = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64,
                    "window_id": wid,
                    "text": "focus-cua"
                }),
            );
            assert!(!typed.is_error(), "type_text failed: {}", typed.text());
            std::thread::sleep(Duration::from_millis(250));
            let after = snapshot_elements(driver, pid, wid);
            assert!(
                after.tree_text().contains("focus-cua"),
                "the click did not leave the field focused:\n{}",
                after.tree_text()
            );
        },
    );
}

/// A key with no effect (shift alone) must stay unverifiable even when the
/// foreground route's own activation and focus write move the field's
/// selection: the oracle may bracket only the key.
#[test]
#[ignore]
fn harness_appkit_foreground_no_op_key_stays_unverifiable() {
    run_case(
        native_foreground_case(
            "appkit",
            "press_key_no_op",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let field = element_token_by_id(&first, "txt-input");
            let set = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "element_token": field, "value": "no-op-cua"
                }),
            );
            assert!(!set.is_error(), "set_value failed: {}", set.text());
            let before = snapshot_elements(driver, pid, wid);
            let field = element_token_by_id(&before, "txt-input");
            let pressed = driver.call(
                "press_key",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid, "element_token": field,
                    "key": "shift", "delivery_mode": "foreground"
                }),
            );
            assert!(!pressed.is_error(), "press_key failed: {}", pressed.text());
            let after = snapshot_elements(driver, pid, wid);
            println!(
                "no-op key selection before={:?} after={:?}",
                before.tree_text().lines().find(|l| l.contains("selection_utf16")),
                after.tree_text().lines().find(|l| l.contains("selection_utf16"))
            );
            assert_eq!(
                pressed.action_effect(),
                Some("unverifiable"),
                "a key with no effect was confirmed: {}",
                pressed.raw
            );
            Observation::delivered_with_fixture_state(Vec::new())
        },
    );
}

/// Foreground type_text while another app is in front: the driver's own
/// activation of the target must not be reverted as a focus steal, and the
/// user's app must be back in front afterward (and stay there).
#[test]
#[ignore]
fn harness_appkit_foreground_type_text_from_behind_restores_front() {
    run_case(
        native_foreground_case(
            "appkit",
            "type_text_from_behind",
            Targeting::Ax,
            DriverRoute::MacosCgEventHid,
        ),
        |pid, wid, driver| {
            let launched = driver.call(
                "launch_app",
                serde_json::json!({"bundle_id":"com.apple.finder"}),
            );
            assert!(!launched.is_error(), "{}", launched.text());
            let windows = driver.call("list_windows", serde_json::json!({}));
            let finder = windows.structured()["windows"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|w| {
                    w["app_name"] == "Finder" && w["is_on_screen"] == true && w["layer"] == 0
                })
                .cloned()
                .expect("an on-screen Finder window");
            let fronted = driver.call(
                "bring_to_front",
                serde_json::json!({"pid":finder["pid"],"window_id":finder["window_id"]}),
            );
            assert_eq!(
                fronted.structured()["activated"],
                true,
                "Finder in front: {}",
                fronted.raw
            );
            // WindowServer's front process, not NSWorkspace's cached view.
            let finder_pid = finder["pid"].as_i64().unwrap() as i32;
            assert_eq!(
                platform_macos::input::skylight::front_pid_matches(finder_pid),
                Some(true),
                "Finder must be frontmost before typing"
            );

            let snap = snapshot_elements(driver, pid, wid);
            let typed = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "element_token": element_token_by_id(&snap, "txt-input"),
                    "text": "behind-cua", "delivery_mode": "foreground"
                }),
            );
            assert!(
                !typed.is_error(),
                "foreground type_text from behind: {}",
                typed.text()
            );
            for delay in [Duration::from_millis(300), Duration::from_millis(1200)] {
                std::thread::sleep(delay);
                assert_eq!(
                    platform_macos::input::skylight::front_pid_matches(finder_pid),
                    Some(true),
                    "Finder must be restored after foreground type_text"
                );
            }
            let post = snapshot_elements(driver, pid, wid);
            assert!(
                post.tree_text().contains("behind-cua"),
                "typing did not land in the target field:\n{}",
                post.tree_text()
            );
            Observation::delivered_with_fixture_state(Vec::new())
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
            assert_honest_return_effect(&commit);

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

/// Return on a text field: `unverifiable` unless the tool itself saw the same
/// native element change. Committing an AppKit field changes its selection,
/// so `confirmed` with value_readback evidence is honest too. Each caller then
/// proves the commit through the fixture, so a false `confirmed` still fails.
#[track_caller]
fn assert_honest_return_effect(result: &ToolResponse) {
    match result.action_effect() {
        Some("unverifiable") => {}
        Some("confirmed") => {
            let evidence = result.structured()["evidence"].clone();
            let kinds: Vec<_> = evidence
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["kind"].as_str())
                .collect();
            assert!(
                !kinds.is_empty() && kinds.iter().all(|k| *k == "value_readback"),
                "confirmed needs the tool's own readback: {}",
                result.raw
            );
        }
        other => panic!("press_key claimed {other:?}: {}", result.raw),
    }
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
                assert!(
                    focused.tree_text().contains("committed=none"),
                    "command ran before Return: {}",
                    focused.tree_text()
                );
                assert!(!oracle_path.exists(), "child process ran before Return");
                // Baseline before Return: nothing committed and no command run.
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
                assert_honest_return_effect(&pressed);
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
            // Focus through the element (AXFocused on a text role), not a
            // pixel click: a background pixel click into a text field
            // activates the fixture and fails the foreground-sentinel check,
            // which is a click-route question, not the readback under test.
            let _ = (x, y, w, h);
            let focused = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "element_token": element_token_by_id(&pre, "txt-input"),
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
            let config = driver.call(
                "set_config",
                serde_json::json!({"max_image_dimension": 200}),
            );
            assert!(
                !config.is_error(),
                "small capture config: {}",
                config.text()
            );
            let pre = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&pre, "btn-increment");
            let zoom = driver.call(
                "zoom",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "x1": x, "y1": y, "x2": x + width, "y2": y + height
                }),
            );
            assert!(
                !zoom.is_error(),
                "create owned zoom context: {}",
                zoom.text()
            );
            let small_width = pre.structured()["screenshot_width"]
                .as_u64()
                .expect("small screenshot width");
            let mut observer = driver
                .spawn_peer_unrecorded()
                .expect("start independent capture client on the same daemon");
            let config = observer.call("set_config", serde_json::json!({"max_image_dimension": 0}));
            assert!(
                !config.is_error(),
                "native capture config: {}",
                config.text()
            );
            let other = snapshot_elements(&mut observer, pid, wid);
            assert!(
                other.structured()["screenshot_width"]
                    .as_u64()
                    .expect("native screenshot width")
                    > small_width
            );
            for (tool, args) in [
                (
                    "double_click",
                    serde_json::json!({"pid":pid as i64,"window_id":wid,"x":x,"y":y,"delivery_mode":"foreground"}),
                ),
                (
                    "right_click",
                    serde_json::json!({"pid":pid as i64,"window_id":wid,"x":x,"y":y,"delivery_mode":"foreground"}),
                ),
                (
                    "scroll",
                    serde_json::json!({"pid":pid as i64,"window_id":wid,"x":x,"y":y,"direction":"down","amount":1,"delivery_mode":"foreground"}),
                ),
                (
                    "drag",
                    serde_json::json!({"pid":pid as i64,"window_id":wid,"from_x":x,"from_y":y,"to_x":x+1.0,"to_y":y+1.0,"duration_ms":0,"steps":1,"delivery_mode":"foreground"}),
                ),
            ] {
                let stale = driver.call(tool, args);
                assert_eq!(
                    stale.structured()["code"],
                    "screenshot_context_missing",
                    "{tool} must refuse another client's screenshot transform"
                );
            }
            for (tool, args) in [
                (
                    "click",
                    serde_json::json!({"pid":pid as i64,"window_id":wid,"x":1.0,"y":1.0,"from_zoom":true,"delivery_mode":"foreground"}),
                ),
                (
                    "drag",
                    serde_json::json!({"pid":pid as i64,"window_id":wid,"from_x":1.0,"from_y":1.0,"to_x":2.0,"to_y":2.0,"from_zoom":true,"duration_ms":0,"steps":1,"delivery_mode":"foreground"}),
                ),
            ] {
                let stale = driver.call(tool, args);
                assert_eq!(
                    stale.structured()["code"],
                    "zoom_context_missing",
                    "{tool} must refuse a zoom bound to the replaced snapshot"
                );
            }
            let stale = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "x": x + width / 2.0, "y": y + height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert_eq!(stale.structured()["code"], "screenshot_context_missing");

            let refreshed = snapshot_elements(driver, pid, wid);
            let (x, y, width, height) = element_pixel_frame(&refreshed, "btn-increment");
            let response = driver.call(
                "click",
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
                "AppKit PX background click failed: {}",
                response.text()
            );
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                snapshot_elements(driver, pid, wid)
                    .tree_text()
                    .contains("counter=1"),
                "AppKit PX background click did not advance counter"
            );
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
            assert_eq!(
                response.structured()["synthetic_target_focus"],
                true,
                "background double click must exercise target-only synthetic focus: {}",
                response.raw
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
        let (wid, title) = driver
            .find_window(harness.pid as i64, "CuaTestHarness AppKit")
            .expect("AppKit main window not found");
        // The shared helper matches substrings, which can select the optional
        // "CuaTestHarness AppKit Secondary" window. Bind the exact main window
        // before starting its recording and background oracles.
        let wid = if title == "CuaTestHarness AppKit" {
            wid
        } else {
            let windows = driver.call("list_windows", serde_json::json!({"pid": harness.pid}));
            let matches: Vec<_> = windows.structured()["windows"]
                .as_array()
                .expect("window list")
                .iter()
                .filter(|w| {
                    w["pid"].as_u64() == Some(harness.pid as u64)
                        && w["title"] == "CuaTestHarness AppKit"
                })
                .filter_map(|w| w["window_id"].as_u64())
                .collect();
            assert_eq!(matches.len(), 1, "unique AppKit main window required");
            matches[0]
        };
        if delivery != cua_driver_testkit::e2e::Delivery::Background {
            driver.start_behavior_recording();
        }
        test(harness.pid, wid, &mut driver)
    });
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

/// Focus theft: pressing btn-steal makes the fixture take focus `delay_ms`
/// later, the way an app reacting to a click often does. All background
/// oracles apply (cursor, stacking, leaked input, liveness), and focus with
/// a 150ms recovery budget: a reactive defense acts after the thief, so the
/// user's window may blur for an instant, but it must be back within the
/// budget. The thief's own trace must also show it lost focus within 100ms.
/// The body waits past the theft so a late one lands before the oracles run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TheftRoute {
    /// AX press on btn-steal.
    Ax,
    /// Pixel click on btn-steal (resolves to an AX press at the point).
    Px,
    /// Pixel click on a plain view: real mouse events.
    Raw,
}

fn run_focus_theft_case(delay_ms: u64, route: TheftRoute) {
    let journal = tempfile::tempdir().unwrap();
    let trace = journal.path().join("thief.jsonl");
    let pointer = journal.path().join("pointer.jsonl");
    std::fs::write(&pointer, "").unwrap();
    let delay = delay_ms.to_string();
    let raw = route == TheftRoute::Raw;
    let mut env = vec![
        ("CUA_APPKIT_THIEF_DELAY_MS", delay.as_str()),
        ("CUA_APPKIT_THIEF_TRACE", trace.to_str().unwrap()),
    ];
    if raw {
        env.push(("CUA_APPKIT_POINTER_ORACLE", pointer.to_str().unwrap()));
    }
    let _budget = cua_driver_testkit::sentinel::blur_recovery_budget(Duration::from_millis(150));
    let name = format!("focus_theft_{delay_ms}ms_{route:?}").to_lowercase();
    run_background_case_with_env(
        &name,
        if route == TheftRoute::Ax { Targeting::Ax } else { Targeting::Px },
        if raw { DriverRoute::MacosCgEventPid } else { DriverRoute::MacosAxAction },
        &env,
        |pid, wid, driver| {
            let response = if route == TheftRoute::Ax {
                let pre = snapshot_elements(driver, pid, wid);
                let token = element_token_by_id(&pre, "btn-steal");
                driver.call(
                    "click",
                    serde_json::json!({
                        "pid": pid as i64, "window_id": wid,
                        "element_token": token, "delivery_mode": "background"
                    }),
                )
            } else {
                let pre = driver.call(
                    "get_window_state",
                    serde_json::json!({"pid": pid as i64, "window_id": wid, "diff": false}),
                );
                let (x, y) = if raw {
                    (
                        pre.structured()["screenshot_width"].as_f64().expect("width") / 2.0,
                        pre.structured()["screenshot_height"].as_f64().expect("height") / 2.0,
                    )
                } else {
                    let (x, y, width, height) = element_pixel_frame(&pre, "btn-steal");
                    (x + width / 2.0, y + height / 2.0)
                };
                driver.call(
                    "click",
                    serde_json::json!({
                        "pid": pid as i64, "window_id": wid,
                        "x": x, "y": y, "delivery_mode": "background"
                    }),
                )
            };
            assert!(!response.is_error(), "steal click: {}", response.raw);
            std::thread::sleep(Duration::from_millis(delay_ms + 700));
            if raw {
                let events = std::fs::read_to_string(&pointer).unwrap();
                assert!(events.contains("\"kind\":\"down\""), "not delivered as raw events: {events}");
            }
            let raw_trace = std::fs::read_to_string(&trace)
                .expect("the fixture never tried to take focus: the click did not land");
            eprintln!("focus theft delay={delay_ms}ms route={route:?}; trace={raw_trace}");
            assert!(
                raw_trace.contains("\"active_after_100ms\":false"),
                "the thief still held focus 100ms after taking it: {raw_trace}"
            );
        },
    );
}

fn run_raw_focus_theft_case(delay_ms: u64) {
    run_focus_theft_case(delay_ms, TheftRoute::Raw);
}

/// `None`: a raw background click with no theft at all, the baseline for
/// whether the raw route itself disturbs the user's focus.
fn run_raw_click_case(thief_delay_ms: Option<u64>) {
    let journal = tempfile::tempdir().unwrap();
    let trace = journal.path().join("thief.jsonl");
    let pointer = journal.path().join("pointer.jsonl");
    std::fs::write(&pointer, "").unwrap();
    let delay_ms = thief_delay_ms.unwrap_or(0);
    let delay = delay_ms.to_string();
    let mut env = vec![
        ("CUA_APPKIT_THIEF_TRACE", trace.to_str().unwrap()),
        ("CUA_APPKIT_POINTER_ORACLE", pointer.to_str().unwrap()),
    ];
    if thief_delay_ms.is_some() {
        env.push(("CUA_APPKIT_THIEF_DELAY_MS", delay.as_str()));
    }
    let name = match thief_delay_ms {
        Some(ms) => format!("focus_theft_{ms}ms_raw"),
        None => "raw_pixel_click_keeps_focus".to_owned(),
    };
    run_background_case_with_env(
        &name,
        Targeting::Px,
        DriverRoute::MacosCgEventPid,
        &env,
        |pid, wid, driver| {
            let pre = driver.call(
                "get_window_state",
                serde_json::json!({"pid": pid as i64, "window_id": wid, "diff": false}),
            );
            let width = pre.structured()["screenshot_width"].as_f64().expect("screenshot width");
            let height = pre.structured()["screenshot_height"].as_f64().expect("screenshot height");
            let response = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "x": width / 2.0, "y": height / 2.0,
                    "delivery_mode": "background"
                }),
            );
            assert!(!response.is_error(), "raw steal click: {}", response.raw);
            std::thread::sleep(std::time::Duration::from_millis(delay_ms + 700));
            let events = std::fs::read_to_string(&pointer).unwrap();
            assert!(
                events.contains("\"kind\":\"down\""),
                "the click did not arrive as raw mouse events: {events}; response {}",
                response.raw
            );
            if thief_delay_ms.is_some() {
                let raw = std::fs::read_to_string(&trace)
                    .expect("the fixture never tried to take focus");
                eprintln!("raw focus theft delay={delay_ms}ms; trace={raw}");
            }
        },
    );
}

/// A raw background pixel click must not disturb the user's focus even when
/// the target never tries to take it.
#[test]
#[ignore]
fn harness_appkit_raw_pixel_click_keeps_focus() {
    run_raw_click_case(None);
}

// No 0ms raw case: an app that activates itself at the instant of a raw
// click is indistinguishable from the activation the click needs (Chromium's
// user-activation gate and remote-HID proxies require the target to become
// AppKit-active during delivery). cua reverts it ~50ms later, but the user's
// window sees a brief blur. Known limitation, recorded in the triage tracker.

#[test]
#[ignore]
fn harness_appkit_focus_theft_300ms_raw() {
    run_raw_focus_theft_case(300);
}

#[test]
#[ignore]
fn harness_appkit_focus_theft_800ms_raw() {
    run_raw_focus_theft_case(800);
}

// Real (HID) input and WindowServer's front app, for user-intent tests.
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateMouseEvent(
        source: *const std::ffi::c_void,
        kind: u32,
        at: CGPoint,
        button: u32,
    ) -> *mut std::ffi::c_void;
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CFRelease(value: *const std::ffi::c_void);
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: *const std::ffi::c_void,
        key: u16,
        down: bool,
    ) -> *mut std::ffi::c_void;
    fn CGEventSourceCounterForEventType(state: i32, kind: u32) -> u32;
}
/// Key-down events the window server has seen (combined session state).
fn key_down_count() -> u32 {
    unsafe { CGEventSourceCounterForEventType(0, 10) }
}
/// Real pointer motion and typing: activity that does not choose an app.
fn real_busy_user(around: CGPoint) {
    const HID_EVENT_TAP: u32 = 0;
    for step in 0..10 {
        unsafe {
            let at = CGPoint { x: around.x + step as f64 * 3.0, y: around.y };
            let moved = CGEventCreateMouseEvent(std::ptr::null(), 5, at, 0);
            CGEventPost(HID_EVENT_TAP, moved);
            CFRelease(moved);
            for down in [true, false] {
                let key = CGEventCreateKeyboardEvent(std::ptr::null(), 0, down); // "a"
                CGEventPost(HID_EVENT_TAP, key);
                CFRelease(key);
            }
        }
        std::thread::sleep(Duration::from_millis(60));
    }
}

/// Center of a Dock item, read through accessibility (Dock > AXList >
/// item titled `title`). The way a person switches apps, and immune to
/// desktop widgets or covering windows.
fn dock_item_center(title: &str) -> Option<CGPoint> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::string::CFString;
    type Element = *const std::ffi::c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> Element;
        fn AXUIElementCopyAttributeValue(
            element: Element,
            attribute: core_foundation::string::CFStringRef,
            value: *mut core_foundation::base::CFTypeRef,
        ) -> i32;
        fn AXValueGetValue(value: Element, kind: u32, out: *mut std::ffi::c_void) -> bool;
    }
    unsafe fn attribute(element: Element, name: &str) -> Option<CFType> {
        let mut value: core_foundation::base::CFTypeRef = std::ptr::null();
        let name = CFString::new(name);
        (AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef(), &mut value) == 0
            && !value.is_null())
        .then(|| CFType::wrap_under_create_rule(value))
    }
    unsafe fn children(element: Element) -> Vec<CFType> {
        attribute(element, "AXChildren")
            .and_then(|value| value.downcast::<CFArray>())
            .map(|array| {
                array
                    .iter()
                    .map(|item| CFType::wrap_under_get_rule(*item as core_foundation::base::CFTypeRef))
                    .collect()
            })
            .unwrap_or_default()
    }
    let dock = Command::new("pgrep").args(["-x", "Dock"]).output().ok()?;
    let dock: i32 = String::from_utf8_lossy(&dock.stdout).trim().parse().ok()?;
    unsafe {
        let app = AXUIElementCreateApplication(dock);
        let app = CFType::wrap_under_create_rule(app as _);
        for list in children(app.as_CFTypeRef() as Element) {
            for item in children(list.as_CFTypeRef() as Element) {
                let element = item.as_CFTypeRef() as Element;
                let named = attribute(element, "AXTitle")
                    .and_then(|value| value.downcast::<CFString>())
                    .is_some_and(|name| name.to_string() == title);
                if !named {
                    continue;
                }
                let mut origin = CGPoint { x: 0.0, y: 0.0 };
                let mut size = CGPoint { x: 0.0, y: 0.0 }; // CGSize has the same layout
                let position = attribute(element, "AXPosition")?;
                let extent = attribute(element, "AXSize")?;
                if AXValueGetValue(position.as_CFTypeRef() as Element, 1, (&mut origin as *mut CGPoint).cast())
                    && AXValueGetValue(extent.as_CFTypeRef() as Element, 2, (&mut size as *mut CGPoint).cast())
                {
                    return Some(CGPoint { x: origin.x + size.x / 2.0, y: origin.y + size.y / 2.0 });
                }
            }
        }
    }
    None
}

fn real_click(at: CGPoint) {
    const HID_EVENT_TAP: u32 = 0;
    for kind in [1u32, 2] {
        unsafe {
            let event = CGEventCreateMouseEvent(std::ptr::null(), kind, at, 0);
            assert!(!event.is_null(), "create HID mouse event");
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}
/// WindowServer's front app via lsappinfo, not a cached AppKit view.
fn front_pid() -> Option<u32> {
    let asn = Command::new("lsappinfo").arg("front").output().ok()?;
    let asn = String::from_utf8_lossy(&asn.stdout).trim().to_owned();
    let info = Command::new("lsappinfo")
        .args(["info", "-only", "pid", &asn])
        .output()
        .ok()?;
    String::from_utf8_lossy(&info.stdout)
        .rsplit('=')
        .next()?
        .trim()
        .parse()
        .ok()
}


/// The user's own app switch during a lingering focus guard must stick.
/// The theft cases prove cua undoes activations nobody asked for; this
/// proves it does not undo the user's. A real HID click (as a person would
/// make) on Finder in the Dock, right after a background cua click,
/// must leave the app the user switched to in front.
#[test]
#[ignore]
fn harness_appkit_user_switch_during_linger_sticks() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("appkit-user-switch-linger")
        .expect("start macOS daemon proxy");
    let harness = Harness::launch();
    let (wid, _) = driver
        .find_window(harness.pid as i64, "CuaTestHarness AppKit")
        .expect("find harness window");
    let sentinel = cua_driver_testkit::sentinel::ForegroundSentinel::launch(&mut driver);
    std::thread::sleep(Duration::from_millis(500));
    let user_app = front_pid().expect("front app");
    assert_ne!(user_app, harness.pid, "the user's app must start in front");
    eprintln!("user app pid={user_app}, sentinel target={:?}", sentinel.target().pid);

    let snapshot = snapshot_elements(&mut driver, harness.pid, wid);
    let token = element_token_by_id(&snapshot, "btn-increment");
    // The harness keeps the target fully covered by the user's app, so a
    // person switching away clicks Finder in the Dock. (The only uncovered
    // desktop is the widget block, and clicking a widget opens its app.)
    let title_bar = dock_item_center("Finder").expect("Finder in the Dock");
    let finder = Command::new("pgrep").args(["-x", "Finder"]).output().expect("pgrep Finder");
    let finder: u32 = String::from_utf8_lossy(&finder.stdout).trim().parse().expect("Finder pid");

    let click = driver.call(
        "click",
        serde_json::json!({
            "pid": harness.pid as i64, "window_id": wid,
            "element_token": token, "delivery_mode": "background"
        }),
    );
    assert!(!click.is_error(), "background click: {}", click.raw);
    // Inside the ~1s lingering guard: the user clicks the target app.
    std::thread::sleep(Duration::from_millis(150));
    real_click(title_bar);
    eprintln!("real click at ({:.0}, {:.0})", title_bar.x, title_bar.y);
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(
        front_pid(),
        Some(finder),
        "the user's switch to Finder was undone by the lingering focus guard"
    );
}

/// Launching an app in the background must not let it take focus. The
/// AppKit fixture activates itself as it finishes launching (as Chrome and
/// Electron apps do), so this is a launch-time theft.
#[test]
#[ignore]
fn harness_appkit_background_launch_keeps_focus() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("appkit-background-launch-focus")
        .expect("start macOS daemon proxy");
    let sentinel = cua_driver_testkit::sentinel::ForegroundSentinel::launch(&mut driver);
    std::thread::sleep(Duration::from_millis(500));
    let launched = driver.call(
        "launch_app",
        serde_json::json!({
            "bundle_id": "com.trycua.harness.appkit",
            "creates_new_application_instance": true
        }),
    );
    eprintln!("launch result: {}", launched.raw);
    assert!(!launched.is_error(), "launch_app: {}", launched.raw);
    let pid = launched.structured()["pid"].as_i64().expect("launched pid");
    std::thread::sleep(Duration::from_millis(2000));
    let (_, violations) = sentinel.observe();
    let _ = Command::new("kill").arg(pid.to_string()).status();
    assert!(
        violations.is_empty(),
        "background launch took the user's focus or the sentinel failed: {violations:?}; launch {}",
        launched.raw
    );
}

/// A busy user (moving the pointer, typing in their own app) has not chosen
/// another app, so a theft during that activity is still undone.
#[test]
#[ignore]
fn harness_appkit_busy_user_does_not_disarm_the_guard() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("appkit-busy-user-guard")
        .expect("start macOS daemon proxy");
    let journal = tempfile::tempdir().unwrap();
    let trace = journal.path().join("thief.jsonl");
    let harness = Harness::launch_with_env(&[
        ("CUA_APPKIT_THIEF_DELAY_MS", "800"),
        ("CUA_APPKIT_THIEF_TRACE", trace.to_str().unwrap()),
    ]);
    let (wid, _) = driver
        .find_window(harness.pid as i64, "CuaTestHarness AppKit")
        .expect("find harness window");
    let _sentinel = cua_driver_testkit::sentinel::ForegroundSentinel::launch(&mut driver);
    std::thread::sleep(Duration::from_millis(500));
    let user_app = front_pid().expect("front app");
    assert_ne!(user_app, harness.pid);
    let snapshot = snapshot_elements(&mut driver, harness.pid, wid);
    let token = element_token_by_id(&snapshot, "btn-steal");
    let click = driver.call(
        "click",
        serde_json::json!({
            "pid": harness.pid as i64, "window_id": wid,
            "element_token": token, "delivery_mode": "background"
        }),
    );
    assert!(!click.is_error(), "steal click: {}", click.raw);
    let keys_before = key_down_count();
    real_busy_user(CGPoint { x: 200.0, y: 200.0 }); // ~600ms of activity
    assert!(
        key_down_count().wrapping_sub(keys_before) >= 10,
        "the busy-user input never reached the window server"
    );
    std::thread::sleep(Duration::from_millis(1200));
    let raw = std::fs::read_to_string(&trace).expect("the fixture never tried to take focus");
    eprintln!("busy user; trace={raw}");
    assert_eq!(
        front_pid(),
        Some(user_app),
        "pointer motion or typing disarmed the guard and the theft stuck"
    );
}

/// After a background launch, the user clicking the launched app must win.
/// The launch watchdog keeps demoting the launched app for several seconds
/// in case it activates itself late; it must stop as soon as real input
/// shows the user chose it.
#[test]
#[ignore]
fn harness_appkit_user_choice_after_launch_sticks() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("appkit-launch-user-choice")
        .expect("start macOS daemon proxy");
    // The user's app is Finder, brought forward by a real desktop click (the
    // foreground sentinel would cover the launched window, so nobody could
    // click it).
    let screen = driver.call("get_screen_size", serde_json::json!({}));
    let (screen_w, screen_h) = (
        screen.structured()["width"].as_f64().expect("screen width"),
        screen.structured()["height"].as_f64().expect("screen height"),
    );
    let open_windows = driver.call("list_windows", serde_json::json!({}));
    let covered: Vec<(f64, f64, f64, f64)> = open_windows.structured()["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .filter(|w| w["is_on_screen"] == true)
        .map(|w| {
            let b = &w["bounds"];
            (b["x"].as_f64().unwrap(), b["y"].as_f64().unwrap(),
             b["width"].as_f64().unwrap(), b["height"].as_f64().unwrap())
        })
        .collect();
    // Away from the desktop-widget block (see the linger test).
    let desktop = (2..19)
        .flat_map(|i| (2..16).map(move |j| (screen_w * i as f64 / 20.0, screen_h * j as f64 / 20.0)))
        .filter(|(x, y)| !(*x < screen_w * 0.4 && *y < screen_h * 0.5))
        .find(|(x, y)| !covered.iter().any(|(wx, wy, ww, wh)| x >= wx && *x < wx + ww && y >= wy && *y < wy + wh))
        .map(|(x, y)| CGPoint { x, y })
        .expect("an uncovered desktop point");
    real_click(desktop);
    std::thread::sleep(Duration::from_millis(500));
    let finder = front_pid().expect("front app after desktop click");
    let launched = driver.call(
        "launch_app",
        serde_json::json!({
            "bundle_id": "com.trycua.harness.appkit",
            "creates_new_application_instance": true
        }),
    );
    assert!(!launched.is_error(), "launch_app: {}", launched.raw);
    let pid = launched.structured()["pid"].as_i64().expect("launched pid") as u32;
    std::thread::sleep(Duration::from_millis(1000));
    let all = driver.call("list_windows", serde_json::json!({}));
    let windows = all.structured()["windows"].as_array().expect("windows").clone();
    let bounds = |w: &serde_json::Value| {
        let b = &w["bounds"];
        (b["x"].as_f64().unwrap(), b["y"].as_f64().unwrap(),
         b["width"].as_f64().unwrap(), b["height"].as_f64().unwrap())
    };
    let target = windows
        .iter()
        .find(|w| w["pid"].as_u64() == Some(pid as u64) && w["is_on_screen"] == true && w["layer"] == 0)
        .expect("launched window on screen")
        .clone();
    let (tx, ty, tw, th) = bounds(&target);
    let target_z = target["z_index"].as_u64().expect("launched window stacking order");
    // A visible point of the launched window: not under a window in front of it.
    let above: Vec<_> = windows
        .iter()
        // Higher z_index is further in front.
        .filter(|w| w["is_on_screen"] == true && w["z_index"].as_u64().is_some_and(|z| z > target_z))
        .map(bounds)
        .collect();
    // Aim along the title bar: list_windows omits the Dock and menu bar, so
    // a point low in the window can land on the Dock (it once hit FaceTime).
    let _ = th;
    let point = (1..10)
        .map(|i| (tx + tw * i as f64 / 10.0, ty + 12.0))
        .find(|(x, y)| !above.iter().any(|(wx, wy, ww, wh)| x >= wx && *x < wx + ww && y >= wy && *y < wy + wh))
        .map(|(x, y)| CGPoint { x, y })
        .expect("a visible point on the launched window's title bar");
    eprintln!("launched pid={pid}; real click at ({:.0}, {:.0})", point.x, point.y);
    real_click(point);
    std::thread::sleep(Duration::from_millis(2000));
    let front = front_pid();
    let _ = Command::new("kill").arg(pid.to_string()).status();
    assert_ne!(finder, pid);
    assert_eq!(front, Some(pid), "the launch watchdog demoted the app the user clicked");
}

/// A text field's placeholder is reported as `placeholder`, never as its
/// value, both in structured elements and in the outline; its writability
/// is reported; and set_value refuses a text field that reports its value
/// read-only, without changing it.
#[test]
#[ignore]
fn harness_appkit_placeholder_value_and_writability() {
    run_background_case_with_env(
        "placeholder_value_writability",
        Targeting::Ax,
        DriverRoute::MacosAxAction,
        &[("CUA_APPKIT_READONLY_FIELD", "1")],
        |pid, wid, driver| {
            let snapshot = snapshot_elements(driver, pid, wid);
            let elements = snapshot.structured()["elements"].as_array().expect("elements").clone();
            let by_id = |id: &str| {
                let index = element_index_by_id(snapshot.tree_text(), id)
                    .unwrap_or_else(|| panic!("{id} not in tree"));
                elements
                    .iter()
                    .find(|e| e["element_index"].as_u64() == Some(index))
                    .cloned()
                    .unwrap_or_else(|| panic!("{id} element"))
            };
            let input = by_id("txt-input");
            eprintln!("txt-input element: {input}");
            assert_eq!(input["value"], "", "the empty field's content, not its hint");
            assert_eq!(input["placeholder"], "Type here…");
            assert_eq!(input["value_settable"], true);
            assert!(
                snapshot.tree_text().contains("[placeholder=\"Type here…\"]"),
                "outline shows the hint as a hint: {}",
                snapshot.tree_text()
            );
            assert!(
                !snapshot.tree_text().contains("= \"Type here…\""),
                "outline must not show the hint as the value"
            );

            let readonly = by_id("txt-readonly");
            eprintln!("txt-readonly element: {readonly}");
            assert_eq!(readonly["value_settable"], false);
            let refused = driver.call(
                "set_value",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "element_token": readonly["element_token"], "value": "changed"
                }),
            );
            eprintln!("set_value on read-only: {}", refused.raw);
            assert_eq!(refused.structured()["code"], "AX_VALUE_NOT_SETTABLE");
            let after = snapshot_elements(driver, pid, wid);
            assert!(after.tree_text().contains("fixed text"), "value unchanged");
            assert!(!after.tree_text().contains("changed"));
        },
    );
}

macro_rules! focus_theft_tests {
    ($($name:ident: $delay:expr, $targeting:expr;)*) => {$(
        #[test]
        #[ignore]
        fn $name() {
            run_focus_theft_case($delay, $targeting);
        }
    )*};
}

focus_theft_tests! {
    harness_appkit_focus_theft_0ms_ax: 0, TheftRoute::Ax;
    harness_appkit_focus_theft_300ms_ax: 300, TheftRoute::Ax;
    harness_appkit_focus_theft_800ms_ax: 800, TheftRoute::Ax;
    harness_appkit_focus_theft_0ms_px: 0, TheftRoute::Px;
    harness_appkit_focus_theft_300ms_px: 300, TheftRoute::Px;
    harness_appkit_focus_theft_800ms_px: 800, TheftRoute::Px;
}

fn run_editor_identity_case(mode: &str) {
    let journal = tempfile::tempdir().unwrap();
    let trace = journal.path().join("editor-identity.jsonl");
    run_background_case_with_env(
        &format!("editor_identity_{mode}"),
        Targeting::Ax,
        DriverRoute::MacosCgEventPid,
        &[
            ("CUA_APPKIT_EDITOR_TRANSITION", mode),
            ("CUA_APPKIT_EDITOR_TRACE", trace.to_str().unwrap()),
        ],
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            eprintln!(
                "editor identity initial mode={mode}; snapshot={}",
                before.raw
            );
            let token = element_token_by_id(&before, "txt-transition-target");
            let response = driver.call(
                "type_text",
                serde_json::json!({
                    "pid": pid, "window_id": wid, "element_token": token,
                    "text": "hello", "delay_ms": 40, "delivery_mode": "background"
                }),
            );
            let after = snapshot_elements(driver, pid, wid);
            let raw = std::fs::read_to_string(&trace).expect("app-owned editor journal");
            eprintln!(
                "editor identity mode={mode}; response={}; journal={raw}; snapshot={}",
                response.raw,
                after.tree_text()
            );
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let last = rows.last().expect("journal rows");
            assert_eq!(last["keys"], 5, "fixture must receive every character once");
            assert_eq!(
                last["other"], "hello",
                "other field must keep its initial text"
            );
            assert!(
                response.structured()["route"] == "synthetic_events"
                    || response.structured()["path"] == "key_events",
                "must exercise post-keystroke readback: {}",
                response.raw
            );
            if mode == "divert" {
                assert_eq!(last["target"], "", "target deliberately took no text");
                assert_eq!(last["transitions"], 1);
                assert_ne!(
                    response.structured()["effect"],
                    "confirmed",
                    "text in another field cannot confirm the requested edit"
                );
            } else {
                assert_eq!(last["target"], "hello");
                assert_eq!(last["transitions"], if mode == "replace" { 1 } else { 0 });
                assert!(
                    !response.is_error(),
                    "complete edit reported an error: {}",
                    response.raw
                );
                assert_eq!(
                    response.structured()["effect"],
                    if mode == "replace" {
                        "unverifiable"
                    } else {
                        "confirmed"
                    },
                    "a replaced addressed element requires fresh observation"
                );
            }
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_editor_identity_stable() {
    run_editor_identity_case("stable");
}

#[test]
#[ignore]
fn harness_appkit_editor_identity_replacement() {
    run_editor_identity_case("replace");
}

#[test]
#[ignore]
fn harness_appkit_editor_identity_diversion() {
    run_editor_identity_case("divert");
}

#[test]
#[ignore]
fn harness_appkit_typing_preexisting_ax_noop() {
    let journal = tempfile::tempdir().unwrap();
    let trace = journal.path().join("typing-preexisting.jsonl");
    run_background_case_with_env(
        "typing_preexisting_ax_noop",
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[
            ("CUA_APPKIT_EDITOR_TRANSITION", "stable"),
            ("CUA_APPKIT_EDITOR_INITIAL", "hello"),
            ("CUA_APPKIT_EDITOR_TRACE", trace.to_str().unwrap()),
        ],
        |pid, wid, driver| {
            if let Ok(probe) = std::env::var("CUA_TYPING_CAPABILITY_PROBE") {
                let output = std::process::Command::new(probe)
                    .arg(pid.to_string())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "read-only capability probe: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                eprintln!(
                    "typing capability probe={}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }
            let before = snapshot_elements(driver, pid, wid);
            let token = element_token_by_id(&before, "txt-transition-target");
            let response = driver.call(
                "type_text",
                serde_json::json!({
                    "pid":pid,"window_id":wid,"element_token":token,
                    "text":"hello","delay_ms":40,"delivery_mode":"background"
                }),
            );
            let after = snapshot_elements(driver, pid, wid);
            let raw = std::fs::read_to_string(&trace).expect("app-owned key journal");
            eprintln!(
                "preexisting AX response={}; journal={raw}; snapshot={}",
                response.raw,
                after.tree_text()
            );
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                rows.len(),
                1,
                "no key input or retry after the accepted AX write"
            );
            assert_eq!(rows[0]["target"], "hello");
            assert_eq!(rows[0]["other"], "hello");
            assert_eq!(rows[0]["keys"], 0);
            assert_eq!(rows[0]["transitions"], 0);
            assert!(!response.is_error());
            assert_eq!(response.structured()["route"], "accessibility");
            assert_ne!(
                response.structured()["effect"],
                "confirmed",
                "an unchanged pre-existing payload cannot confirm the new insertion"
            );
        },
    );
}

fn run_editor_sequence_case(mode: &str, ambiguous: bool) {
    let journal = tempfile::tempdir().unwrap();
    let trace = journal.path().join("editor-sequence.jsonl");
    run_background_case_with_env(
        &format!(
            "editor_sequence_{}",
            if ambiguous { "ambiguous" } else { mode }
        ),
        Targeting::Ax,
        DriverRoute::MacosCgEventPid,
        &[
            ("CUA_APPKIT_EDITOR_TRANSITION", mode),
            ("CUA_APPKIT_EDITOR_TRACE", trace.to_str().unwrap()),
        ],
        |pid, wid, driver| {
            let before = snapshot_elements(driver, pid, wid);
            let target = element_token_by_id(&before, "txt-transition-target");
            let increment = element_token_by_id(&before, "btn-increment");
            let selector = if ambiguous {
                serde_json::json!({"role":"AXTextField"})
            } else {
                serde_json::json!({"role":"AXTextField","label_contains":"Transition target"})
            };
            let response = driver.call(
                "run_sequence",
                serde_json::json!({
                    "pid":pid,"window_id":wid,
                    "steps":[
                        {"tool":"click","arguments":{"element_token":target},
                         "expect":[{"element":{
                             "selector":{"role":"AXTextField","label_contains":"Transition target"},
                             "value_equals":""}}],"timeout_ms":1000,"stable_samples":2},
                        {"tool":"type_text","arguments":{"text":"hello"},
                         "expect":[{"element":{"selector":selector,"value_equals":"hello"}}],
                         "timeout_ms":1000,"stable_samples":2},
                        {"tool":"click","arguments":{"element_token":increment},
                         "expect":[{"element":{
                             "selector":{"role":"AXStaticText","label_contains":"counter="},
                             "value_equals":"counter=1"}}],"timeout_ms":1000,"stable_samples":2}
                    ]
                }),
            );
            let after = snapshot_elements(driver, pid, wid);
            let raw = std::fs::read_to_string(&trace).expect("app-owned editor journal");
            eprintln!("editor sequence mode={mode} ambiguous={ambiguous}; response={}; journal={raw}; snapshot={}",
                response.raw, after.raw);
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let last = rows.last().unwrap();
            assert_eq!(last["keys"], 5, "one payload, with no retries");
            assert_eq!(last["target"], if mode == "divert" { "" } else { "hello" });
            assert_eq!(last["other"], "hello");
            assert!(!response.is_error(), "{}", response.raw);
            let output = response.structured();
            let steps = output["steps"].as_array().unwrap();
            let changed_editor = matches!(mode, "replace" | "divert");
            assert_eq!(
                steps[1]["action"]["effect"],
                if changed_editor {
                    "unverifiable"
                } else {
                    "confirmed"
                },
                "implicit typing cannot confirm delivery using a different focused editor"
            );
            if changed_editor {
                assert!(steps[1]["action"]["delivery"]["delivered_count"].is_null());
            }
            let stopped = mode == "divert" || ambiguous;
            assert_eq!(
                output["status"],
                if stopped { "stopped" } else { "completed" }
            );
            assert_eq!(steps.len(), if stopped { 2 } else { 3 });
            if stopped {
                assert_eq!(output["stopped_at"], 1);
                assert!(
                    after.tree_text().contains("counter=0"),
                    "later click must not execute"
                );
                assert!(!after.tree_text().contains("counter=1"));
                if ambiguous {
                    assert_eq!(output["stop_reason"], "unknown");
                    assert_eq!(steps[1]["verification"]["status"], "unknown");
                    assert_eq!(
                        steps[1]["verification"]["predicates"][0]["unknown_reason"],
                        "multi_match"
                    );
                } else {
                    assert_eq!(
                        output["stop_reason"], "unsatisfied",
                        "fresh target evidence must stop after the unverifiable action"
                    );
                    assert_eq!(steps[1]["verification"]["status"], "unsatisfied");
                }
            } else {
                assert!(after.tree_text().contains("counter=1"));
                assert!(!after.tree_text().contains("counter=0"));
                for step in steps {
                    assert_eq!(step["verification"]["status"], "satisfied");
                    assert_eq!(step["verification"]["stable"], true);
                    assert!(step["observation_count"].as_u64().unwrap() >= 2);
                }
            }
            for step in steps {
                assert_eq!(step["image_bytes_returned"], 0);
            }
            assert!(response.raw["result"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|block| block["type"] != "image"));
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_editor_sequence_stable() {
    run_editor_sequence_case("stable", false);
}

#[test]
#[ignore]
fn harness_appkit_editor_sequence_replacement() {
    run_editor_sequence_case("replace", false);
}

#[test]
#[ignore]
fn harness_appkit_editor_sequence_diversion() {
    run_editor_sequence_case("divert", false);
}

#[test]
#[ignore]
fn harness_appkit_editor_sequence_ambiguous() {
    run_editor_sequence_case("stable", true);
}

#[test]
#[ignore]
fn harness_appkit_typing_repeated_and_selected_text() {
    run_typing_repeated_and_selected_text(false);
}

#[test]
#[ignore]
fn harness_appkit_addressed_typing_preserves_selection() {
    run_typing_repeated_and_selected_text(true);
}

fn run_typing_repeated_and_selected_text(addressed: bool) {
    run_background_case(
        if addressed {
            "addressed_typing_selected"
        } else {
            "typing_repeated_selected"
        },
        DriverRoute::MacosAxValue,
        |pid, wid, driver| {
            let first = snapshot_elements(driver, pid, wid);
            let (x, y, w, h) = element_pixel_frame(&first, "txt-input");
            let focus = driver.call(
                "click",
                serde_json::json!({
                    "pid":pid,"window_id":wid,"x":x+w/2.0,"y":y+h/2.0,"delivery_mode":"background"
                }),
            );
            assert!(!focus.is_error(), "focus: {}", focus.text());
            for (phase, text, select_all, expected) in [
                ("initial", "hello", false, "hello"),
                ("duplicate_append", "hello", false, "hellohello"),
                ("shorter_replacement", "A😀B", true, "A😀B"),
                ("identical_replacement", "A😀B", true, "A😀B"),
            ] {
                if select_all {
                    let before_selection = snapshot_elements(driver, pid, wid);
                    let selected = driver.call("press_key", serde_json::json!({
                    "pid":pid,"window_id":wid,"element_token":element_token_by_id(&before_selection,"txt-input"),
                    "key":"left","modifiers":["cmd","shift"],"delivery_mode":"background"
                }));
                    assert!(!selected.is_error(), "select all: {}", selected.text());
                    let selection = snapshot_elements(driver, pid, wid);
                    let index = element_index_by_id(selection.tree_text(), "txt-input").unwrap();
                    let field = selection.structured()["elements"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|e| e["element_index"].as_u64() == Some(index))
                        .unwrap();
                    eprintln!(
                        "selection setup phase={phase}; response={}; field={field}",
                        selected.raw
                    );
                    assert_eq!(
                        field["text_selection"]["range"],
                        serde_json::json!({
                            "location":0,"length":field["value"].as_str().unwrap().encode_utf16().count()
                        }),
                        "replacement setup must select the entire value before any input"
                    );
                }
                let mut arguments = serde_json::json!({
                    "pid":pid,"window_id":wid,"text":text,"delivery_mode":"background"
                });
                if addressed {
                    let current = snapshot_elements(driver, pid, wid);
                    arguments["element_token"] =
                        serde_json::json!(element_token_by_id(&current, "txt-input"));
                }
                let response = driver.call("type_text", arguments);
                let native = slice_a_tree(pid, wid);
                assert_eq!(
                    native
                        .nodes
                        .iter()
                        .find(|node| node.identifier.as_deref() == Some("txt-input"))
                        .and_then(|node| node.value.as_deref()),
                    Some(expected),
                    "independent native value must reflect exactly one edit"
                );
                let after = snapshot_elements(driver, pid, wid);
                let index = element_index_by_id(after.tree_text(), "txt-input").unwrap();
                let field = after.structured()["elements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["element_index"].as_u64() == Some(index))
                    .unwrap();
                eprintln!(
                    "insertion control phase={phase}; response={}; field={field}",
                    response.raw
                );
                assert_eq!(
                    field["value"], expected,
                    "one exact insertion or replacement"
                );
                assert_eq!(
                    field["text_selection"]["range"],
                    serde_json::json!({
                        "location":expected.encode_utf16().count(),"length":0
                    })
                );
                assert!(!response.is_error(), "{}", response.raw);
                assert_eq!(
                    response.structured()["effect"],
                    "confirmed",
                    "{phase}: {}",
                    response.raw
                );
                assert_eq!(
                    response.structured()["delivery"]["delivered_count"],
                    text.chars().count()
                );
            }
        },
    );
}

// Compare native value assignment and insertion with the app's own change
// and commit labels. Diagnostic cases record the mirror; the regression
// requires a notification without accepting submission as a substitute.
fn run_value_notification_probe(
    prepare: bool,
    typing: bool,
    require_notification: bool,
    sibling: bool,
) {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use platform_macos::ax::bindings::{copy_children, copy_string_attr, AXUIElementRef};
    let label = if sibling && prepare {
        "sibling_prepared"
    } else if sibling {
        "sibling_regression"
    } else if require_notification {
        "regression"
    } else if typing {
        "typing"
    } else if prepare {
        "prepared"
    } else {
        "unprepared"
    };
    run_background_case_with_env(
        &format!("value_notification_{label}"),
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        if sibling {
            &[("CUA_HARNESS_BRING_TO_FRONT_MODE", "ordinary")]
        } else {
            &[]
        },
        |pid, wid, driver| {
            let native = slice_a_tree(pid, wid);
            assert!(!native.truncated);
            let field = native
                .nodes
                .iter()
                .find(|n| n.identifier.as_deref() == Some("txt-input"))
                .unwrap();
            assert_eq!(field.role, "AXTextField");
            let ptr = field.element_ptr;
            let window = native.nodes.iter().find(|n| n.role == "AXWindow").unwrap();
            assert!(
                window.element_index.is_some(),
                "retain owner must hold the window"
            );
            let observe = || {
                // The display tree deliberately omits empty static labels.
                // Read the fixture's direct window children without that filter;
                // CF owners release every copied child even if an assertion fails.
                let children: Vec<CFType> = unsafe {
                    copy_children(window.element_ptr as AXUIElementRef)
                        .into_iter()
                        .map(|child| CFType::wrap_under_create_rule(child as CFTypeRef))
                        .collect()
                };
                let value = |id: &str| {
                    let matches: Vec<_> = children
                        .iter()
                        .filter(|child| unsafe {
                            copy_string_attr(child.as_CFTypeRef() as AXUIElementRef, "AXIdentifier")
                                .as_deref()
                                == Some(id)
                        })
                        .collect();
                    assert_eq!(matches.len(), 1, "unique native field {id}");
                    unsafe {
                        copy_string_attr(matches[0].as_CFTypeRef() as AXUIElementRef, "AXValue")
                    }
                    .expect("readable native value")
                };
                serde_json::json!({
                    "field":value("txt-input"), "mirror":value("lbl-input-mirror"),
                    "commit":value("lbl-input-commit"), "counter":value("lbl-counter"),
                    "focused":platform_macos::input::ax_actions::is_element_focused(pid as i32, ptr)
                })
            };
            if sibling {
                let facts = platform_macos::ax::exact_target::gather_background_facts(
                    pid as i32,
                    wid as u32,
                    Some(ptr),
                );
                assert!(
                    facts
                        .competing_keyboard_destinations > 0,
                    "the semantic-only control must have a competing destination"
                );
            }
            let before = observe();
            assert_eq!(before["field"], "");
            assert_eq!(before["mirror"], "");
            assert_eq!(before["focused"], false);
            if prepare {
                platform_macos::input::ax_actions::focus_element(ptr).unwrap();
            }
            let prepared = observe();
            assert_eq!(prepared["focused"], prepare);
            assert_eq!(prepared["field"], "");
            assert_eq!(
                prepared["mirror"], "",
                "focus alone must not fabricate an edit"
            );
            // Replacing an existing value, preserving numeric-looking text,
            // Unicode, clearing, and an idempotent repeat must all leave the
            // app's change mirror consistent without ending editing.
            let payloads: &[&str] = if require_notification {
                &["Research ready", "007", "Ω café", "", "", "Research ready"]
            } else {
                &["Research ready"]
            };
            for &payload in payloads {
                let snapshot = snapshot_elements(driver, pid, wid);
                let mut arguments = serde_json::json!({
                    "pid":pid, "window_id":wid,
                    "element_token":element_token_by_id(&snapshot, "txt-input")
                });
                if typing {
                    arguments["text"] = payload.into();
                    arguments["delivery_mode"] = "background".into();
                } else {
                    arguments["value"] = payload.into();
                }
                let started = std::time::Instant::now();
                let response =
                    driver.call(if typing { "type_text" } else { "set_value" }, arguments);
                let call_ms = started.elapsed().as_millis();
                let after = observe();
                eprintln!(
                    "value notification {}",
                    serde_json::json!({
                        "prepare":prepare,"typing":typing,"require_notification":require_notification,
                        "before":before,"prepared":prepared,"after":after,"call_ms":call_ms,"response":response.raw
                    })
                );
                assert!(!response.is_error(), "{}", response.raw);
                assert_eq!(after["field"], payload);
                assert_eq!(
                    after["commit"], "committed=none",
                    "do not submit the field to obtain a notification"
                );
                assert_eq!(after["counter"], "counter=0");
                if typing || require_notification {
                    assert_eq!(
                        after["mirror"], payload,
                        "the app must observe the text change without committing"
                    );
                }
            }
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_value_notification_unprepared() {
    run_value_notification_probe(false, false, false, false);
}

#[test]
#[ignore]
fn harness_appkit_value_notification_prepared() {
    run_value_notification_probe(true, false, false, false);
}

#[test]
#[ignore]
fn harness_appkit_value_notification_typing() {
    run_value_notification_probe(false, true, false, false);
}

#[test]
#[ignore]
fn harness_appkit_set_value_notifies_without_commit() {
    run_value_notification_probe(false, false, true, false);
}

#[test]
#[ignore]
fn harness_appkit_set_value_sibling_notifies_without_commit() {
    run_value_notification_probe(false, false, true, true);
}

// Operator probe: isolate exact-element AX preparation from the product's
// conservative keyboard eligibility check. It must keep all background oracles
// while notifying the target's delegate with a competing same-PID window.
#[test]
#[ignore]
fn harness_appkit_value_notification_sibling_prepared() {
    run_value_notification_probe(true, false, true, true);
}

// Exercise two real AppKit field editors, including a preexisting sibling
// draft. The app-owned labels expose both change and end-editing callbacks.
#[test]
#[ignore]
fn harness_appkit_set_value_two_editable_windows() {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use platform_macos::ax::bindings::{copy_children, copy_string_attr, AXUIElementRef};
    run_background_case_with_env(
        "set_value_two_editable_windows",
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[("CUA_HARNESS_BRING_TO_FRONT_MODE", "editable")],
        |pid, wid, driver| {
            let listing = driver.call("list_windows", serde_json::json!({"pid":pid}));
            let siblings: Vec<_> = listing.structured()["windows"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|w| w["pid"] == pid && w["title"] == "CuaTestHarness AppKit Secondary")
                .filter_map(|w| w["window_id"].as_u64())
                .collect();
            assert_eq!(siblings.len(), 1);
            let sibling_wid = siblings[0];
            assert_ne!(wid, sibling_wid);
            // Keep both independent AX trees alive for their retained pointers.
            let main_tree = slice_a_tree(pid, wid);
            let sibling_tree = slice_a_tree(pid, sibling_wid);
            assert!(!main_tree.truncated && !sibling_tree.truncated);
            let main_ptr = main_tree
                .nodes
                .iter()
                .find(|n| n.role == "AXWindow")
                .unwrap()
                .element_ptr;
            let sibling_ptr = sibling_tree
                .nodes
                .iter()
                .find(|n| n.role == "AXWindow")
                .unwrap()
                .element_ptr;
            let read = |ptr: usize, ids: &[&str]| {
                let children: Vec<CFType> = unsafe {
                    copy_children(ptr as AXUIElementRef)
                        .into_iter()
                        .map(|p| CFType::wrap_under_create_rule(p as CFTypeRef))
                        .collect()
                };
                ids.iter()
                    .map(|id| {
                        let matches: Vec<_> = children
                            .iter()
                            .filter(|c| unsafe {
                                copy_string_attr(c.as_CFTypeRef() as AXUIElementRef, "AXIdentifier")
                                    .as_deref()
                                    == Some(*id)
                            })
                            .collect();
                        assert_eq!(matches.len(), 1, "unique fixture value {id}");
                        unsafe {
                            copy_string_attr(matches[0].as_CFTypeRef() as AXUIElementRef, "AXValue")
                        }
                        .expect("readable fixture value")
                    })
                    .collect::<Vec<_>>()
            };
            let observe = || {
                serde_json::json!({
                    "main":read(main_ptr, &["txt-input", "lbl-input-mirror", "lbl-input-commit", "lbl-counter"]),
                    "sibling":read(sibling_ptr, &["txt-sibling-input", "lbl-sibling-mirror", "lbl-sibling-commit"])
                })
            };
            let initial = observe();
            assert_eq!(
                initial["main"],
                serde_json::json!(["", "", "committed=none", "counter=0"])
            );
            assert_eq!(
                initial["sibling"],
                serde_json::json!(["", "", "committed=none"])
            );
            let mut main_value = "";
            let mut sibling_value = "";
            for (target_wid, id, value) in [
                (sibling_wid, "txt-sibling-input", "Sibling draft"),
                (wid, "txt-input", "Main draft"),
                (sibling_wid, "txt-sibling-input", "Sibling revised"),
                (wid, "txt-input", "Main revised"),
            ] {
                let before = observe();
                let snapshot = snapshot_elements(driver, pid, target_wid);
                let response = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid":pid, "window_id":target_wid,
                        "element_token":element_token_by_id(&snapshot, id), "value":value
                    }),
                );
                let after = observe();
                eprintln!(
                    "two editable windows {}",
                    serde_json::json!({
                        "target_window":target_wid,"value":value,"before":before,"after":after,"response":response.raw
                    })
                );
                assert!(!response.is_error(), "{}", response.raw);
                if target_wid == wid {
                    main_value = value;
                } else {
                    sibling_value = value;
                }
                assert_eq!(
                    after["main"],
                    serde_json::json!([main_value, main_value, "committed=none", "counter=0"])
                );
                assert_eq!(
                    after["sibling"],
                    serde_json::json!([sibling_value, sibling_value, "committed=none"])
                );
            }
            // Positive control: the commit oracle must change when the fixture
            // deliberately ends sibling editing, while the main edit survives.
            let snapshot = snapshot_elements(driver, pid, sibling_wid);
            let response = driver.call(
                "click",
                serde_json::json!({
                    "pid":pid, "window_id":sibling_wid,
                    "element_token":element_token_by_id(&snapshot, "btn-sibling-end-edit"),
                    "delivery_mode":"background"
                }),
            );
            let after = observe();
            eprintln!(
                "two editable commit control {}",
                serde_json::json!({"after":after,"response":response.raw})
            );
            assert!(!response.is_error(), "{}", response.raw);
            assert_eq!(
                after["main"],
                serde_json::json!([main_value, main_value, "committed=none", "counter=0"])
            );
            assert_eq!(
                after["sibling"],
                serde_json::json!([
                    sibling_value,
                    sibling_value,
                    format!("committed={sibling_value}")
                ])
            );
        },
    );
}

// Isolate the existing native focus helper from input and verification timing.
fn run_typing_preparation_probe(prepare: bool) {
    use core_foundation::{base::TCFType, string::CFString};
    use platform_macos::ax::bindings::*;
    let label = if prepare { "focused" } else { "unprepared" };
    run_background_case_with_env(
        &format!("typing_preparation_{label}"),
        Targeting::Ax,
        DriverRoute::MacosAxValue,
        &[],
        |pid, wid, driver| {
            // Reuse the independent native tree's retain owner. The preparation
            // is a fixture diagnostic, not a second driver or input retry.
            let native = slice_a_tree(pid, wid);
            assert!(!native.truncated);
            let field = native
                .nodes
                .iter()
                .find(|node| node.identifier.as_deref() == Some("txt-input"))
                .expect("native fixture text field");
            assert_eq!(field.role, "AXTextField");
            assert!(field.element_index.is_some());
            let ptr = field.element_ptr;
            let evidence = || {
                let mut writable = 0;
                let name = CFString::new("AXSelectedText");
                let error = unsafe {
                    AXUIElementIsAttributeSettable(
                        ptr as AXUIElementRef,
                        name.as_concrete_TypeRef(),
                        &mut writable,
                    )
                };
                serde_json::json!({
                    "focused": platform_macos::input::ax_actions::is_element_focused(pid as i32, ptr),
                    "value": unsafe { copy_string_attr(ptr as AXUIElementRef, "AXValue") },
                    "selected_text_settable_error": error,
                    "selected_text_settable": if error == kAXErrorSuccess { Some(writable != 0) } else { None },
                })
            };
            let before = evidence();
            assert_eq!(before["focused"], false, "fresh field starts unfocused");
            assert_eq!(before["value"], "");
            let preparation_started = std::time::Instant::now();
            if prepare {
                platform_macos::input::ax_actions::focus_element(ptr).unwrap();
            }
            let prepared = evidence();
            let preparation_ms = preparation_started.elapsed().as_millis();
            assert_eq!(prepared["focused"], prepare, "check actual focus identity");
            assert_eq!(prepared["value"], "", "preparation cannot insert text");
            let snapshot = snapshot_elements(driver, pid, wid);
            let started = std::time::Instant::now();
            let response = driver.call(
                "type_text",
                serde_json::json!({
                    "pid":pid,"window_id":wid,
                    "element_token":element_token_by_id(&snapshot,"txt-input"),
                    "text":"focus-cua","delivery_mode":"background"
                }),
            );
            let call_ms = started.elapsed().as_millis();
            let after = slice_a_tree(pid, wid);
            let value = after
                .nodes
                .iter()
                .find(|node| node.identifier.as_deref() == Some("txt-input"))
                .and_then(|node| node.value.as_deref())
                .expect("fresh native field value");
            eprintln!(
                "typing preparation {}",
                serde_json::json!({
                    "prepare":prepare,"before":before,"prepared":prepared,
                    "preparation_ms":preparation_ms,"call_ms":call_ms,
                    "response":response.raw,"native_after":value,
                })
            );
            assert!(!response.is_error(), "{}", response.raw);
            assert_eq!(value, "focus-cua", "exact native field contents");
            assert_eq!(response.structured()["effect"], "confirmed");
            assert_eq!(response.structured()["delivery"]["delivered_count"], 9);
            assert_eq!(
                response.structured()["route"],
                "accessibility",
                "addressed native typing should prepare the field before its AX write"
            );
        },
    );
}

#[test]
#[ignore]
fn harness_appkit_typing_preparation_unprepared() {
    run_typing_preparation_probe(false);
}

#[test]
#[ignore]
fn harness_appkit_typing_preparation_focused() {
    run_typing_preparation_probe(true);
}

/// A field whose `AXValue` catches up with the write over the next second is
/// not a partially typed field. The AX rung used to read the value back once,
/// microseconds after the write returned, and published the prefix it caught
/// as `type_text_incomplete`: measured in Contacts as "delivered 6 of 14"
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
            // Focus through the element (AXFocused on a text role), not a
            // pixel click: a background pixel click into a text field
            // activates the fixture and fails the foreground-sentinel check,
            // which is a click-route question, not the readback under test.
            let _ = (x, y, w, h);
            let focused = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid as i64, "window_id": wid,
                    "element_token": element_token_by_id(&snap_pre, "txt-input"),
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

fn slice_a_tree(pid: u32, wid: u64) -> SliceANativeTree {
    SliceANativeTree(platform_macos::ax::walk_tree(
        pid as i32,
        Some(wid.try_into().unwrap()),
        None,
    ))
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
