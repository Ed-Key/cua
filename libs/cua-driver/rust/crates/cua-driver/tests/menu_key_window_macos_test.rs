//! A native menu command keeps acting on the requested window after
//! `invoke_menu` returns.
//!
//! AppKit runs some menu work on a later run-loop turn (TextEdit writes a saved
//! document asynchronously), against whichever window is key then. The AppKit
//! harness item Window > Record Key Window Later reproduces that: 150 ms after
//! it runs it records the key window's number to `CUA_HARNESS_MENU_JOURNAL`.
//! With a sibling window of the same app key beforehand, invoke_menu on the
//! main window must leave the main window key for that work.
//!
//! Run with:
//! `cargo test -p cua-driver --test menu_key_window_macos_test -- --ignored --nocapture --test-threads=1`

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cua_driver_testkit::{harness_app, Driver, McpDriver};

struct Fixture {
    pid: u32,
    report: tempfile::NamedTempFile,
    journal: tempfile::NamedTempFile,
}

fn launch(driver: &mut McpDriver) -> Fixture {
    let exe = std::env::var("HARNESS_APPKIT_APP")
        .map(std::path::PathBuf::from)
        .ok()
        .filter(|path| path.exists())
        .unwrap_or_else(|| harness_app("harness-appkit", "CuaTestHarness.AppKit.app"))
        .join("Contents/MacOS/CuaTestHarness.AppKit");
    assert!(exe.exists(), "required AppKit harness missing at {exe:?}");
    let report = tempfile::NamedTempFile::new().expect("window report");
    let journal = tempfile::NamedTempFile::new().expect("menu journal");
    let mut command = Command::new(exe);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.env("CUA_HARNESS_WINDOW_REPORT", report.path());
    command.env("CUA_HARNESS_MENU_JOURNAL", journal.path());
    command.env("CUA_HARNESS_BRING_TO_FRONT_MODE", "two-windows");
    let child = cua_driver_testkit::spawn_in_job(&mut command).expect("launch AppKit harness");
    let pid = child.id();
    driver.reaper().push(child);
    Fixture { pid, report, journal }
}

fn window_ids(fixture: &Fixture) -> HashMap<String, u32> {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let report = std::fs::read_to_string(fixture.report.path()).unwrap_or_default();
        let ids: HashMap<String, u32> = report
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter_map(|(name, id)| id.parse().ok().map(|id| (name.to_owned(), id)))
            .collect();
        if ids.contains_key("main") && ids.contains_key("secondary") {
            std::thread::sleep(Duration::from_millis(500));
            return ids;
        }
        assert!(Instant::now() < deadline, "harness windows did not appear: {report:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore]
fn deferred_menu_work_acts_on_the_requested_window() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("macos-menu-key-window")
        .expect("start installed macOS daemon proxy");
    let fixture = launch(&mut driver);
    let ids = window_ids(&fixture);

    // Precondition: the sibling window of the same app is key.
    let fronted = driver.call(
        "bring_to_front",
        serde_json::json!({"pid": fixture.pid, "window_id": ids["secondary"]}),
    );
    assert!(!fronted.is_error(), "bring_to_front secondary failed: {}", fronted.text());

    let invoked = driver.call(
        "invoke_menu",
        serde_json::json!({"pid": fixture.pid, "window_id": ids["main"], "path": ["Window", "Record Key Window Later"]}),
    );
    assert!(!invoked.is_error(), "invoke_menu failed: {}", invoked.text());

    let deadline = Instant::now() + Duration::from_secs(3);
    let journal = loop {
        let text = std::fs::read_to_string(fixture.journal.path()).unwrap_or_default();
        if text.contains("key=") || Instant::now() >= deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        journal.trim(),
        format!("key={}", ids["main"]),
        "the deferred menu work saw another key window (secondary is {})",
        ids["secondary"]
    );
}
