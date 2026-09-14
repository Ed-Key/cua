//! Cold Electron accessibility coverage through the installed macOS daemon.
//!
//! Unlike helpers that poll AX until it becomes ready, this checks exactly one
//! snapshot after the fixture-owned journal reports that the page is ready.
//! Run in a logged-in, TCC-authorized macOS session:
//! cargo test -p cua-driver --test electron_first_snapshot_macos_test -- --ignored --nocapture --test-threads=1
//! CUA_E2E_FIRST_AX_BUDGET_MS optionally bounds a controlled local measurement.
//! A fast readable tree is insufficient: its first control must also accept
//! input. Do not trade first-action correctness for a shorter snapshot time.

#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cua_driver_testkit::observer::TargetWindow;
use cua_driver_testkit::sentinel::ForegroundSentinel;
use cua_driver_testkit::{harness_app, spawn_in_job, Driver, FixtureJournal, McpDriver};

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn first_electron_snapshot_contains_ready_web_controls() {
    check_first_snapshot(false, false);
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn first_background_electron_snapshot_contains_ready_web_controls() {
    check_first_snapshot(true, false);
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn first_background_electron_snapshot_control_accepts_first_click() {
    check_first_snapshot(true, true);
}

fn check_first_snapshot(background: bool, click_first: bool) {
    let executable = harness_app(
        "harness-electron",
        "CuaTestHarness.Electron.app/Contents/MacOS/Electron",
    );
    assert!(
        executable.exists(),
        "missing Electron fixture: {executable:?}"
    );
    let profile = tempfile::tempdir().expect("fresh Electron profile");
    let journal = FixtureJournal::start();
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("electron-first-ax-snapshot")
        .expect("authorized macOS daemon proxy");
    let sentinel = background.then(|| ForegroundSentinel::launch(&mut driver));
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("allocate fixture port")
        .local_addr()
        .expect("fixture port")
        .port();
    let mut command = Command::new(executable);
    command
        .env("CUA_E2E_USER_DATA_DIR", profile.path())
        .env("CUA_E2E_FIXTURE_JOURNAL_URL", journal.url())
        .env("CUA_ELECTRON_CDP_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let child = spawn_in_job(&mut command).expect("launch fresh Electron process");
    let pid = child.id();
    driver.reaper().push(child);

    // Readiness comes from the app, never an earlier AX read or CDP enablement.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !journal.contains("WEB_HARNESS_MARKER_v1") {
        assert!(
            Instant::now() < deadline,
            "fixture page did not become ready"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let wid = loop {
        let windows = driver.call("list_windows", serde_json::json!({"pid": pid}));
        assert!(
            !windows.is_error(),
            "window lookup failed: {}",
            windows.text()
        );
        if let Some(wid) = windows.structured()["windows"]
            .as_array()
            .and_then(|items| {
                items.iter().find_map(|window| {
                    window["title"]
                        .as_str()
                        .filter(|title| title.starts_with("CuaTestHarness Electron"))
                        .and_then(|_| window["window_id"].as_u64())
                })
            })
        {
            break wid;
        }
        assert!(Instant::now() < deadline, "fixture window did not appear");
        std::thread::sleep(Duration::from_millis(25));
    };

    let target = TargetWindow {
        pid,
        native_id: wid,
    };
    if let Some(sentinel) = &sentinel {
        sentinel
            .prepare_background_observation(&mut driver, target)
            .expect("owned sentinel covers the fresh Electron target");
    }
    let mut read = || {
        if click_first {
            assert_eq!(journal.text("lbl-counter").as_deref(), Some("counter=0"));
        }
        let start = Instant::now();
        let state = driver.call(
            "get_window_state",
            serde_json::json!({
                "pid": pid, "window_id": wid, "include_screenshot": false,
            }),
        );
        let elapsed = start.elapsed();
        if click_first {
            assert!(!state.is_error(), "first snapshot failed: {}", state.text());
            let data = state.structured();
            let targets: Vec<_> = data["elements"]
                .as_array()
                .expect("structured AX elements")
                .iter()
                .filter(|element| {
                    element["role"] == "AXButton"
                        && element["label"] == "Increment"
                        && element["enabled"] == true
                })
                .collect();
            assert_eq!(targets.len(), 1, "one enabled Increment control");
            let token = targets[0]["element_token"].as_str().expect("fresh token");
            // No second snapshot, readiness sleep, or retry. An AXPress
            // acknowledgement does not prove the renderer accepted the action.
            let click = driver.call(
                "click",
                serde_json::json!({
                    "pid": pid, "window_id": wid, "element_token": token,
                    "delivery_mode": "background",
                }),
            );
            assert!(!click.is_error(), "first click failed: {}", click.text());
            let deadline = Instant::now() + Duration::from_secs(2);
            while journal.text("lbl-counter").as_deref() != Some("counter=1") {
                assert!(
                    Instant::now() < deadline,
                    "first AX click did not reach the renderer: {}; journal: {}",
                    click.text(),
                    journal.snapshot()
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        (state, elapsed)
    };
    let (state, elapsed) = if let Some(sentinel) = &sentinel {
        let (result, oracles) = sentinel
            .observe_background(target, read)
            .expect("first read preserves foreground, cursor, z-order and input isolation");
        eprintln!("first background snapshot oracles: {oracles:?}");
        result
    } else {
        read()
    };
    eprintln!("first Electron AX snapshot: {elapsed:?}");
    assert!(!state.is_error(), "first snapshot failed: {}", state.text());
    let data = state.structured();
    let elements = data["elements"].as_array().expect("structured AX elements");
    assert!(
        elements
            .iter()
            .any(|element| element["role"] == "AXWebArea"),
        "first snapshot omitted the ready web subtree: {}",
        state.text()
    );
    assert!(
        elements
            .iter()
            .any(|element| element["role"] == "AXButton" && element["label"] == "Increment"),
        "first snapshot omitted the known Increment control: {}",
        state.text()
    );
    if let Ok(budget) = std::env::var("CUA_E2E_FIRST_AX_BUDGET_MS") {
        let budget = Duration::from_millis(budget.parse().expect("positive millisecond budget"));
        assert!(!budget.is_zero(), "latency budget must be positive");
        assert!(
            elapsed <= budget,
            "first useful snapshot exceeded the local latency budget: {elapsed:?} > {budget:?}"
        );
    }
}
