//! Child windows count as part of their parent window, and stay targetable by
//! their own id.
//!
//! Finder's inline rename field is a separate borderless child window of the
//! Finder window. The AppKit harness reproduces it with
//! `CUA_HARNESS_BRING_TO_FRONT_MODE=child-field`: an editable field in its own
//! child window over the main window, holding keyboard focus. The field logs
//! every change to `CUA_HARNESS_CHILD_JOURNAL`, which is the oracle here; the
//! driver's own reply is not trusted.
//!
//! Run with:
//! `cargo test -p cua-driver --test child_window_macos_test -- --ignored --nocapture --test-threads=1`

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cua_driver_testkit::ax::element_index_by_id;
use cua_driver_testkit::{harness_app, Driver, McpDriver, ToolResponse};

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
    let journal = tempfile::NamedTempFile::new().expect("child journal");
    let mut command = Command::new(exe);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.env("CUA_HARNESS_WINDOW_REPORT", report.path());
    command.env("CUA_HARNESS_CHILD_JOURNAL", journal.path());
    command.env("CUA_HARNESS_BRING_TO_FRONT_MODE", "child-field");
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
        if ids.contains_key("main") && ids.contains_key("child") {
            // Let AppKit finish making the child key before acting.
            std::thread::sleep(Duration::from_millis(500));
            return ids;
        }
        assert!(Instant::now() < deadline, "child-field windows did not appear: {report:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn journal(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.journal.path()).unwrap_or_default()
}

fn wait_for_journal(fixture: &Fixture, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = journal(fixture);
        if text.contains(needle) || Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn snapshot(driver: &mut McpDriver, pid: u32, window_id: u32) -> ToolResponse {
    let result = driver.call(
        "get_window_state",
        serde_json::json!({"pid": pid, "window_id": window_id, "include_screenshot": false, "diff": false}),
    );
    assert!(!result.is_error(), "get_window_state failed: {}", result.text());
    result
}

fn child_field_token(snapshot: &ToolResponse) -> String {
    let index = element_index_by_id(snapshot.tree_text(), "txt-child-field")
        .unwrap_or_else(|| panic!("txt-child-field not in snapshot:\n{}", snapshot.tree_text()));
    snapshot.structured()["elements"]
        .as_array()
        .and_then(|elements| elements.iter().find(|e| e["element_index"].as_u64() == Some(index)))
        .and_then(|element| element["element_token"].as_str())
        .expect("txt-child-field element_token")
        .to_owned()
}

/// Finder's case: the focused field lives in a child window, and the agent
/// names the parent window. A foreground key must reach the field instead of
/// failing the exact-window focus check or re-keying the parent (which would
/// end the edit and drop the key).
#[test]
#[ignore]
fn foreground_key_for_parent_reaches_focused_child_field() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("macos-child-window-key")
        .expect("start installed macOS daemon proxy");
    let fixture = launch(&mut driver);
    let ids = window_ids(&fixture);

    let result = driver.call(
        "press_key",
        serde_json::json!({"pid": fixture.pid, "window_id": ids["main"], "key": "x", "delivery_mode": "foreground"}),
    );
    assert!(!result.is_error(), "foreground key for the parent failed: {}", result.text());
    // Taking focus selects the field's text, so the key replaces it; either
    // way the field's last value ends with the typed character.
    let journal = wait_for_journal(&fixture, "value=");
    let last_value = journal.lines().rev().find_map(|line| line.strip_prefix("value="));
    assert!(
        last_value.is_some_and(|value| value.ends_with('x')),
        "the key did not reach the child field; journal={journal:?}"
    );
}

/// Codex review case: a child window stays addressable by its own id, so an
/// element in it is proven to belong to that id.
#[test]
#[ignore]
fn child_window_is_targetable_by_its_own_id() {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named("macos-child-window-exact")
        .expect("start installed macOS daemon proxy");
    let fixture = launch(&mut driver);
    let ids = window_ids(&fixture);

    let state = snapshot(&mut driver, fixture.pid, ids["child"]);
    let token = child_field_token(&state);
    let result = driver.call(
        "set_value",
        serde_json::json!({"pid": fixture.pid, "window_id": ids["child"], "element_token": token, "value": "final.txt"}),
    );
    assert!(!result.is_error(), "set_value on the child by its own id failed: {}", result.text());
    let after = snapshot(&mut driver, fixture.pid, ids["child"]);
    assert!(
        after.tree_text().contains("final.txt"),
        "the child field did not change:\n{}",
        after.tree_text()
    );
}
