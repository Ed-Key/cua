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
    check_first_snapshot(false, false, None);
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn first_background_electron_snapshot_contains_ready_web_controls() {
    check_first_snapshot(true, false, None);
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn first_background_electron_snapshot_control_accepts_first_click() {
    check_first_snapshot(true, true, None);
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_coordinate_typing_reaches_unfocused_renderer() {
    check_first_snapshot(true, false, Some(TypingScenario::Fresh));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_coordinate_typing_preserves_selected_replacement() {
    check_first_snapshot(true, false, Some(TypingScenario::ReplaceSelection));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_coordinate_typing_recovers_after_addressed_attempt() {
    check_first_snapshot(true, false, Some(TypingScenario::AfterAddressedAttempt));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_coordinate_typing_preserves_selection_before_first_pointer() {
    check_first_snapshot(true, false, Some(TypingScenario::SelectionBeforePointer));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_web_selection_is_observed_without_confirming_keys() {
    check_first_snapshot(true, false, Some(TypingScenario::ObserveSelection));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_web_addressed_key_moves_caret_without_changing_text() {
    check_first_snapshot(true, false, Some(TypingScenario::AddressedKey));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_web_pixel_key_moves_caret_without_changing_text() {
    check_first_snapshot(true, false, Some(TypingScenario::PixelKey));
}

#[test]
#[ignore = "requires the staged Electron fixture and an authorized macOS daemon"]
fn background_web_addressed_hotkey_moves_caret_without_changing_text() {
    check_first_snapshot(true, false, Some(TypingScenario::AddressedHotkey));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypingScenario {
    AddressedKey,
    PixelKey,
    AddressedHotkey,
    Fresh,
    ReplaceSelection,
    AfterAddressedAttempt,
    SelectionBeforePointer,
    ObserveSelection,
}

fn log_editor_focus(pid: i32, x: f64, y: f64, label: &str) -> Option<bool> {
    use core_foundation::base::CFRelease;
    use platform_macos::ax::bindings::*;
    unsafe {
        if let Some(element) = element_at_screen_position(pid, x, y) {
            let role = copy_string_attr(element, "AXRole");
            let focused = copy_bool_attr(element, "AXFocused");
            let app_focus =
                platform_macos::input::ax_actions::is_element_focused(pid, element as usize);
            let window_focus = copy_element_attr(element, "AXWindow").map(|window| {
                let result = (
                    copy_bool_attr(window, "AXFocused"),
                    copy_bool_attr(window, "AXMain"),
                );
                CFRelease(window as _);
                result
            });
            eprintln!("{label}: role={role:?}, AXFocused={focused:?}, app_focus={app_focus}, window_focused_main={window_focus:?}");
            CFRelease(element as _);
            return window_focus.and_then(|(_, main)| main);
        }
    }
    None
}

fn check_first_snapshot(background: bool, click_first: bool, typing: Option<TypingScenario>) {
    let mut executable = harness_app(
        "harness-electron",
        "CuaTestHarness.Electron.app/Contents/MacOS/Electron",
    );
    assert!(
        executable.exists(),
        "missing Electron fixture: {executable:?}"
    );
    // Match a never-activated Electron composer, not a text field whose
    // window was first foregrounded by the standard fixture startup.
    let owned_fixture = typing.map(|_| tempfile::tempdir().expect("owned typing fixture"));
    if let Some(root) = &owned_fixture {
        let source_app = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let app = root.path().join("TypingFixture.app");
        assert!(Command::new("/usr/bin/ditto")
            .arg(source_app)
            .arg(&app)
            .status()
            .unwrap()
            .success());
        let resources = app.join("Contents/Resources/app");
        let host_path = resources.join("main.js");
        let mut host = std::fs::read_to_string(&host_path).unwrap();
        for (before, after) in [
            (
                "const { app, BrowserWindow, ipcMain } = require('electron');",
                "const { app, BrowserWindow, ipcMain } = require('electron');\napp.dock.hide();",
            ),
            (
                "show: !sentinelMode || customCuaCompositor,",
                "show: false,",
            ),
            (
                "          mainWindow.show();\n          mainWindow.focus();",
                "          mainWindow.showInactive();",
            ),
        ] {
            assert_eq!(host.matches(before).count(), 1, "known fixture host layout");
            host = host.replacen(before, after, 1);
        }
        std::fs::write(host_path, host).unwrap();
        let html_path = resources.join("web/index.html");
        let mut html = std::fs::read_to_string(&html_path).unwrap();
        let start = html.find("<input type=\"text\" id=\"txt-input\"").unwrap();
        let end = start + html[start..].find("/>").unwrap() + 2;
        html.replace_range(
            start..end,
            r#"<textarea id="txt-input" data-cua-id="txt-input"
            aria-label="txt-input" rows="2" cols="30"></textarea>
            <button id="select-input" data-cua-id="select-input"
              onclick="const field=document.getElementById('txt-input');field.focus();field.select()">Select input text</button>
            <button aria-label="Select suffix" onclick="const f=document.getElementById('txt-input');f.focus();f.setSelectionRange(5,8)">Select suffix</button>
            <button aria-label="Place caret at start" onclick="const f=document.getElementById('txt-input');f.focus();f.setSelectionRange(0,0)">Place caret at start</button>
            <button aria-label="Place caret at end" onclick="const f=document.getElementById('txt-input');f.focus();f.setSelectionRange(8,8)">Place caret at end</button>"#,
        );
        let anchor = "if ('value' in element) entry.value = element.value;";
        assert_eq!(html.matches(anchor).count(), 1);
        html = html.replacen(anchor, &format!("{anchor}\nif (element.tagName === 'TEXTAREA') {{ entry.selectionStart=element.selectionStart; entry.selectionEnd=element.selectionEnd; entry.focused=document.activeElement===element; }}"), 1);
        std::fs::write(html_path, html).unwrap();
        assert!(Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Add :LSUIElement bool true"])
            .arg(app.join("Contents/Info.plist"))
            .status()
            .unwrap()
            .success());
        assert!(Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--deep",
                "--sign",
                "-",
                "--preserve-metadata=entitlements,flags,runtime"
            ])
            .arg(&app)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app)
            .status()
            .unwrap()
            .success());
        executable = app.join("Contents/MacOS/Electron");
    }
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
                "pid": pid, "window_id": wid, "include_screenshot": typing.is_some(),
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
        if let Some(scenario) = typing {
            assert!(
                !state.is_error(),
                "typing snapshot failed: {}",
                state.text()
            );
            let data = state.structured();
            assert_eq!(data["screenshot_frame_valid"], true);
            let fields: Vec<_> = data["elements"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["label"] == "txt-input" && e["role"] == "AXTextArea")
                .collect();
            assert_eq!(fields.len(), 1, "one fixture editor");
            let field = &fields[0]["frame"];
            let bounds = platform_macos::windows::window_bounds_by_id(wid as u32).unwrap();
            let scale = data["screenshot_width"].as_f64().unwrap() / bounds.width;
            let x = (field["x"].as_f64().unwrap() + field["w"].as_f64().unwrap() / 2.0 - bounds.x)
                * scale;
            let y = (field["y"].as_f64().unwrap() + field["h"].as_f64().unwrap() / 2.0 - bounds.y)
                * scale;
            assert_eq!(journal.text("lbl-input-mirror").as_deref(), Some("mirror="));
            let before_number = journal.snapshot()["number-input"].clone();
            if matches!(
                scenario,
                TypingScenario::AddressedKey
                    | TypingScenario::PixelKey
                    | TypingScenario::AddressedHotkey
            ) {
                let seeded = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid":pid, "window_id":wid, "element_token":fields[0]["element_token"],
                        "value":"KEEP abc"
                    }),
                );
                assert!(!seeded.is_error(), "seed failed: {}", seeded.text());
                let deadline = Instant::now() + Duration::from_secs(2);
                while journal.snapshot()["txt-input"]["value"] != "KEEP abc" {
                    assert!(Instant::now() < deadline, "seed did not reach renderer");
                    std::thread::sleep(Duration::from_millis(25));
                }
                let current = driver.call(
                    "get_window_state",
                    serde_json::json!({
                        "pid":pid,"window_id":wid,"include_screenshot":false
                    }),
                );
                let token = current.structured()["elements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["role"] == "AXButton" && e["label"] == "Place caret at start")
                    .unwrap()["element_token"]
                    .clone();
                let selected = driver.call(
                    "click",
                    serde_json::json!({
                        "pid":pid,"window_id":wid,"element_token":token,"delivery_mode":"background"
                    }),
                );
                assert!(
                    !selected.is_error(),
                    "caret setup failed: {}",
                    selected.text()
                );
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    let actual = journal.snapshot()["txt-input"].clone();
                    if actual["focused"] == true
                        && actual["selectionStart"] == 0
                        && actual["selectionEnd"] == 0
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "caret setup not established: {actual}"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                let current = driver.call(
                    "get_window_state",
                    serde_json::json!({
                        "pid":pid,"window_id":wid,"include_screenshot":false
                    }),
                );
                let token = current.structured()["elements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["role"] == "AXTextArea" && e["label"] == "txt-input")
                    .unwrap()["element_token"]
                    .clone();
                let mut args =
                    serde_json::json!({"pid":pid,"window_id":wid,"delivery_mode":"background"});
                if scenario == TypingScenario::PixelKey {
                    // A center click alone can put the caret at the end and
                    // conceal a dropped key. Click at the start of the line.
                    args["x"] =
                        serde_json::json!((field["x"].as_f64().unwrap() + 4.0 - bounds.x) * scale);
                    args["y"] =
                        serde_json::json!((field["y"].as_f64().unwrap() + 15.0 - bounds.y) * scale);
                } else {
                    args["element_token"] = token;
                }
                let tool = if scenario == TypingScenario::AddressedHotkey {
                    // A text-editing chord (Cmd+A is a menu key equivalent,
                    // which background delivery does not promise; use the
                    // foreground rung for menu shortcuts). From the caret at 0,
                    // only the chord selects to the end: a focus click alone
                    // leaves an empty selection.
                    args["keys"] = serde_json::json!(["cmd", "shift", "right"]);
                    "hotkey"
                } else {
                    args["key"] = serde_json::json!("right");
                    args["modifiers"] = serde_json::json!(["cmd"]);
                    "press_key"
                };
                if scenario == TypingScenario::PixelKey {
                    // ed/main's contract: pixels refer to the latest screenshot
                    // this session took. Addressed keys keep their token's
                    // snapshot, which a fresh read would supersede.
                    let shot = driver.call(
                        "get_window_state",
                        serde_json::json!({"pid":pid,"window_id":wid,"include_screenshot":true}),
                    );
                    assert!(!shot.is_error(), "{}", shot.text());
                }
                let key = driver.call(tool, args);
                assert!(!key.is_error(), "key delivery failed: {}", key.text());
                assert_eq!(
                    key.action_effect(),
                    Some("unverifiable"),
                    "web key success must not be inferred from AX"
                );
                let (start, end) = if scenario == TypingScenario::AddressedHotkey {
                    (0, 8)
                } else {
                    (8, 8)
                };
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    let actual = journal.snapshot()["txt-input"].clone();
                    assert_eq!(actual["value"], "KEEP abc", "navigation changed the text");
                    if actual["selectionStart"] == start && actual["selectionEnd"] == end {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "background {tool} did not move the actual selection: {actual}"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                assert_eq!(
                    journal.snapshot()["number-input"],
                    before_number,
                    "other editor changed"
                );
                return (state, elapsed);
            }
            if scenario == TypingScenario::AfterAddressedAttempt {
                let addressed = driver.call(
                    "type_text",
                    serde_json::json!({
                        "pid":pid, "window_id":wid, "element_token":fields[0]["element_token"],
                        "text":"Original text", "delivery_mode":"background"
                    }),
                );
                eprintln!("initial addressed typing: {}", addressed.text());
                let refreshed = driver.call(
                    "get_window_state",
                    serde_json::json!({
                        "pid":pid, "window_id":wid, "include_screenshot":true
                    }),
                );
                assert!(
                    !refreshed.is_error(),
                    "refresh after addressed typing failed"
                );
                let observed = journal.text("lbl-input-mirror");
                // An improved addressed route may complete directly. Never
                // append another copy when the independent app state proves it.
                if observed.as_deref() == Some("mirror=Original text") {
                    assert_eq!(journal.snapshot()["number-input"], before_number);
                    return (state, elapsed);
                }
                assert_eq!(
                    observed.as_deref(),
                    Some("mirror="),
                    "do not retry a partial edit"
                );
                let _ = log_editor_focus(
                    pid as i32,
                    x / scale + bounds.x,
                    y / scale + bounds.y,
                    "after addressed attempt",
                );
                eprintln!(
                    "renderer state before coordinate recovery: {}",
                    journal.snapshot()["txt-input"]
                );
            }
            if scenario == TypingScenario::SelectionBeforePointer {
                let seeded = driver.call(
                    "set_value",
                    serde_json::json!({
                        "pid":pid, "window_id":wid, "element_token":fields[0]["element_token"],
                        "value":"Original text", "delivery_mode":"background"
                    }),
                );
                let deadline = Instant::now() + Duration::from_secs(2);
                while journal.text("lbl-input-mirror").as_deref() != Some("mirror=Original text") {
                    assert!(
                        Instant::now() < deadline,
                        "initial value did not land: {}",
                        seeded.text()
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            for payload in if matches!(
                scenario,
                TypingScenario::ReplaceSelection | TypingScenario::ObserveSelection
            ) {
                vec!["Original text", "Replacement"]
            } else if scenario == TypingScenario::SelectionBeforePointer {
                vec!["Replacement"]
            } else {
                vec!["Original text"]
            } {
                if payload == "Replacement" {
                    // Select through a visible fixture control and prove the
                    // actual range. Background Cmd+A is a separate unsupported
                    // path in the current diagnostic and must not be assumed.
                    let current = driver.call(
                        "get_window_state",
                        serde_json::json!({
                            "pid":pid, "window_id":wid, "include_screenshot":false
                        }),
                    );
                    assert!(!current.is_error(), "selection control read failed");
                    let current = current.structured();
                    let controls: Vec<_> = current["elements"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|e| e["role"] == "AXButton" && e["label"] == "Select input text")
                        .collect();
                    assert_eq!(controls.len(), 1);
                    let selected = driver.call("click", serde_json::json!({
                        "pid":pid, "window_id":wid, "element_token":controls[0]["element_token"],
                        "delivery_mode":"background"
                    }));
                    assert!(
                        !selected.is_error(),
                        "selection failed: {}",
                        selected.text()
                    );
                    let deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        let observed = journal.snapshot()["txt-input"].clone();
                        if observed["selectionStart"] == 0 && observed["selectionEnd"] == 13 {
                            eprintln!("selection before replacement: {observed}");
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "Fixture control did not establish selection: {}; observed: {}",
                            selected.text(),
                            observed
                        );
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                if scenario == TypingScenario::ObserveSelection && payload == "Replacement" {
                    let current = driver.call(
                        "get_window_state",
                        serde_json::json!({
                            "pid":pid,"window_id":wid,"include_screenshot":false
                        }),
                    );
                    let field = current.structured()["elements"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|e| e["label"] == "txt-input" && e["role"] == "AXTextArea")
                        .unwrap();
                    eprintln!(
                        "web selection observation: {field}; renderer: {}",
                        journal.snapshot()["txt-input"]
                    );
                    let _ = log_editor_focus(
                        pid as i32,
                        x / scale + bounds.x,
                        y / scale + bounds.y,
                        "selection observation",
                    );
                    assert_eq!(field["in_web_content"], true);
                    assert_eq!(field["focused"], true);
                    assert_eq!(
                        field["text_selection"]["range"],
                        serde_json::json!({"location":0,"length":13})
                    );
                    assert_eq!(field["text_selection"]["text"], "Original text");
                    let key = driver.call(
                        "press_key",
                        serde_json::json!({
                            "pid":pid,"window_id":wid,"element_token":field["element_token"],
                            "key":"right","modifiers":["cmd"],"delivery_mode":"background"
                        }),
                    );
                    assert!(!key.is_error(), "web key: {}", key.text());
                    assert_eq!(
                        key.action_effect(),
                        Some("unverifiable"),
                        "web AX must not confirm a key: {}",
                        key.text()
                    );
                    assert_eq!(
                        journal.text("lbl-input-mirror").as_deref(),
                        Some("mirror=Original text")
                    );
                    return (state, elapsed);
                }
                let main = log_editor_focus(
                    pid as i32,
                    x / scale + bounds.x,
                    y / scale + bounds.y,
                    payload,
                );
                if scenario == TypingScenario::SelectionBeforePointer {
                    assert_eq!(
                        main,
                        Some(false),
                        "selection must predate native window focus"
                    );
                }
                // ed/main's contract: pixels refer to the latest screenshot this
                // session took, so read with one right before the pixel action.
                let shot = driver.call(
                    "get_window_state",
                    serde_json::json!({"pid":pid,"window_id":wid,"include_screenshot":true}),
                );
                assert!(!shot.is_error(), "{}", shot.text());
                let inserted = driver.call(
                    "type_text",
                    serde_json::json!({
                        "pid":pid, "window_id":wid, "x":x, "y":y, "text":payload,
                        "delivery_mode":"background"
                    }),
                );
                eprintln!("coordinate typing result: {}", inserted.text());
                let expected = format!("mirror={payload}");
                let deadline = Instant::now() + Duration::from_secs(2);
                while journal.text("lbl-input-mirror").as_deref() != Some(expected.as_str()) {
                    assert!(
                        Instant::now() < deadline,
                        "coordinate typing did not reach renderer: {}; journal: {}",
                        inserted.text(),
                        journal.snapshot()
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                assert!(
                    !inserted.is_error(),
                    "typing reported failure after delivery: {}",
                    inserted.text()
                );
                assert_eq!(
                    journal.snapshot()["number-input"],
                    before_number,
                    "other editor changed"
                );
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
