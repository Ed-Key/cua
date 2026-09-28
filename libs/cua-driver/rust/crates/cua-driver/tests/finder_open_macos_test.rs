//! Opening a Finder item reports success.
//!
//! Finder's AXOpen on a folder icon navigates the window, destroying the icon
//! it was performed on, and the accessibility call then returns an error
//! although it ran. The driver must report the action as performed (an agent
//! told it failed retries and acts twice), and the window must really have
//! navigated. Finder is the native oracle here; the window title is read from
//! WindowServer, not from the driver's reply.
//!
//! Run with:
//! `cargo test -p cua-driver --test finder_open_macos_test -- --ignored --nocapture --test-threads=1`

#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::{Duration, Instant};

use cua_driver_testkit::{Driver, McpDriver, ToolResponse};

struct Folder {
    root: tempfile::TempDir,
    name: String,
}

fn folder_with_subfolder(tag: &str) -> Folder {
    let root = tempfile::Builder::new().prefix(&format!("cua-open-{tag}-")).tempdir().expect("temp folder");
    std::fs::create_dir(root.path().join("Inner")).expect("subfolder");
    std::fs::write(root.path().join("note.txt"), "note").expect("file");
    let name = root.path().file_name().unwrap().to_string_lossy().into_owned();
    Folder { root, name }
}

fn finder_window(driver: &mut McpDriver, title: &str) -> (u64, u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows = driver.call("list_windows", serde_json::json!({}));
        if let Some(window) = windows.structured()["windows"].as_array().and_then(|all| {
            all.iter().find(|w| {
                w["app_name"] == "Finder" && w["is_on_screen"] == true && w["title"] == title
            })
        }) {
            return (window["pid"].as_u64().unwrap(), window["window_id"].as_u64().unwrap());
        }
        assert!(Instant::now() < deadline, "Finder window {title:?} did not appear");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn window_title(driver: &mut McpDriver, window_id: u64) -> Option<String> {
    let windows = driver.call("list_windows", serde_json::json!({}));
    windows.structured()["windows"]
        .as_array()?
        .iter()
        .find(|w| w["window_id"].as_u64() == Some(window_id))
        .and_then(|w| w["title"].as_str().map(str::to_owned))
}

fn inner_icon_token(driver: &mut McpDriver, pid: u64, window_id: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state: ToolResponse = driver.call(
            "get_window_state",
            serde_json::json!({"pid": pid, "window_id": window_id, "include_screenshot": false, "diff": false, "timeout_ms": 4000}),
        );
        if let Some(token) = state.structured()["elements"].as_array().and_then(|elements| {
            elements
                .iter()
                .find(|e| e["label"] == "Inner" && e["actions"].as_array().is_some_and(|a| a.iter().any(|x| x == "AXOpen")))
                .and_then(|e| e["element_token"].as_str().map(str::to_owned))
        }) {
            return token;
        }
        assert!(Instant::now() < deadline, "no openable Inner icon in the Finder window");
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn assert_opens(tag: &str, call: impl Fn(&mut McpDriver, u64, u64, String) -> ToolResponse) {
    let mut driver = McpDriver::spawn_macos_daemon_proxy_named(&format!("macos-finder-open-{tag}"))
        .expect("start installed macOS daemon proxy");
    let folder = folder_with_subfolder(tag);
    let opened = Command::new("open").arg(folder.root.path()).status().expect("open folder in Finder");
    assert!(opened.success());
    let (pid, window_id) = finder_window(&mut driver, &folder.name);
    let token = inner_icon_token(&mut driver, pid, window_id);

    let result = call(&mut driver, pid, window_id, token);
    assert!(!result.is_error(), "opening a Finder folder reported failure: {}", result.text());

    let deadline = Instant::now() + Duration::from_secs(3);
    while window_title(&mut driver, window_id).as_deref() != Some("Inner") {
        assert!(Instant::now() < deadline, "the Finder window did not navigate into Inner");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore]
fn double_click_on_a_finder_folder_opens_it_and_reports_success() {
    assert_opens("double-click", |driver, pid, window_id, token| {
        driver.call(
            "double_click",
            serde_json::json!({"pid": pid, "window_id": window_id, "element_token": token}),
        )
    });
}

#[test]
#[ignore]
fn click_open_on_a_finder_folder_opens_it_and_reports_success() {
    assert_opens("click-open", |driver, pid, window_id, token| {
        driver.call(
            "click",
            serde_json::json!({"pid": pid, "window_id": window_id, "element_token": token, "action": "open"}),
        )
    });
}
