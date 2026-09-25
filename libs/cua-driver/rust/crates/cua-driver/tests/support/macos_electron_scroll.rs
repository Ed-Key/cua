//! Focused native wheel tests, separate from full-occlusion delivery certification.
//! CDP reads fixture geometry/state only. All input uses the public scroll tool.
use super::*;
use cua_driver_testkit::observer::{DesktopObserver, NativeObserver};

fn run_scroll(covered: bool, element_target: bool) {
    if !covered {
        assert!(
            unsafe { platform_macos::ax::bindings::AXIsProcessTrusted() },
            "visible-scroll setup requires Accessibility for the test process to move its owned sentinel; the driver's separate grant does not authorize this process"
        );
    }
    let _lock = STANDALONE_BROWSER_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let spec = BrowserSpec {
        name: "electron".into(),
        executable: cua_driver_testkit::harness_app(
            "harness-electron",
            "CuaTestHarness.Electron.app/Contents/MacOS/Electron",
        ),
    };
    let html = standalone_fixture_html().replace(
        r#"<main class="harness-grid">"#,
        r#"<div id="history" role="region" aria-label="Message history"
          style="width:600px;height:300px;overflow:auto;border:2px solid black"></div>
<script>
// Electron's preload owns its normal journal transport. This standalone
// fixture announces readiness to its own BrowserFixtureServer explicitly.
fetch(window.__CUA_E2E_FIXTURE_JOURNAL_URL,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({ready:'WEB_HARNESS_MARKER_v1'})});
const historyPane = document.getElementById('history');
window.scrollProbe = {first:0,last:12,events:[]};
function renderRows() {
  const first = Math.max(0,Math.floor(historyPane.scrollTop/40)-2);
  const last = Math.min(99,first+12);
  historyPane.replaceChildren();
  const before = document.createElement('div'); before.style.height=(first*40)+'px';
  historyPane.append(before);
  for(let i=first;i<=last;i++) {
    const row=document.createElement('div');row.textContent='Message '+i;
    row.style.height='40px';historyPane.append(row);
  }
  const after=document.createElement('div');after.style.height=((99-last)*40)+'px';
  historyPane.append(after);Object.assign(window.scrollProbe,{first,last});
}
historyPane.addEventListener('scroll',renderRows);renderRows();
historyPane.addEventListener('wheel',e=>window.scrollProbe.events.push({trusted:e.isTrusted,dy:e.deltaY}));
</script>
<div role="region" aria-label="Offscreen history"
 style="position:absolute;top:3000px;width:400px;height:200px;overflow:auto">
 <div style="height:1000px">Offscreen rows must not be revealed by scrolling.</div>
</div><main class="harness-grid">"#,
    );
    let mut fixture = launch_browser_with_html(&spec, "electron-native-scroll", html);
    assert!(platform_macos::browser::ElectronJs::is_electron(
        fixture.pid as i32
    ));
    let ws = cdp_page_websocket_for_url(fixture.cdp_port, &fixture.server.page_url());
    let read = || {
        harness_cdp_call_at_url(&ws, "Runtime.evaluate", serde_json::json!({
        "expression":r#"(()=>{const pane=document.getElementById('history');const bounds=pane.getBoundingClientRect();return {...window.scrollProbe,top:pane.scrollTop,outer:window.scrollY,focused:document.hasFocus(),visibility:document.visibilityState,visible_rows:[...pane.children].filter(row=>{const r=row.getBoundingClientRect();return row.textContent.startsWith('Message ')&&r.top>=bounds.top&&r.bottom<=bounds.bottom}).map(row=>row.textContent)}})()"#,
        "returnByValue":true
    }))["result"]["value"].clone()
    };
    let geometry = harness_cdp_call_at_url(&ws,"Runtime.evaluate",serde_json::json!({
        "expression":"(()=>{const r=document.getElementById('history').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2+outerHeight-innerHeight}})()",
        "returnByValue":true
    }))["result"]["value"].clone();
    let state = fixture.driver.call(
        "get_window_state",
        serde_json::json!({
            "pid":fixture.pid,"window_id":fixture.window_id,"include_screenshot":true
        }),
    );
    let bounds = platform_macos::windows::window_bounds_by_id(fixture.window_id as u32).unwrap();
    let scale = state.structured()["screenshot_width"].as_f64().unwrap() / bounds.width;
    let x = geometry["x"].as_f64().unwrap() * scale;
    let y = geometry["y"].as_f64().unwrap() * scale;
    let sentinel = ForegroundSentinel::launch(&mut fixture.driver);
    let target = TargetWindow {
        pid: fixture.pid,
        native_id: fixture.window_id,
    };
    sentinel
        .prepare_background_observation(&mut fixture.driver, target)
        .unwrap();
    if !covered {
        // Move only our sentinel aside, keeping it frontmost and preserving its
        // input-leak journal. Derive placement from the actual target bounds.
        // Fixture setup uses AX geometry directly, avoiding another driver
        // action's deferred focus restoration inside the scroll observation.
        unsafe {
            use platform_macos::ax::bindings::*;
            let app = AXUIElementCreateApplication(sentinel.target().pid as i32);
            let windows = copy_ax_windows(app);
            core_foundation::base::CFRelease(app as _);
            let mut found = false;
            for window in windows {
                if ax_get_window_id(window) == Some(sentinel.target().native_id as u32) {
                    assert_eq!(set_size_attr(window, "AXSize", 300.0, 300.0), 0);
                    assert_eq!(
                        set_point_attr(
                            window,
                            "AXPosition",
                            bounds.x + bounds.width + 20.0,
                            bounds.y
                        ),
                        0
                    );
                    found = true;
                }
                core_foundation::base::CFRelease(window as _);
            }
            assert!(found, "owned sentinel window");
        }
        // The full-screen helper uses a fixed desktop setup click, which no
        // longer lies in the sentinel after resizing. Activate its exact window
        // without repeating that click, then verify the actual foreground PID.
        let front = fixture.driver.call(
            "bring_to_front",
            serde_json::json!({
                "pid":sentinel.target().pid,"window_id":sentinel.target().native_id
            }),
        );
        assert!(!front.is_error(), "{}", front.raw);
    } else {
        let windows = platform_macos::windows::visible_windows();
        let cover = windows
            .iter()
            .find(|w| w.window_id == sentinel.target().native_id as u32)
            .unwrap();
        let target = windows
            .iter()
            .find(|w| w.window_id == fixture.window_id as u32)
            .unwrap();
        assert!(cover.z_index > target.z_index);
        // Match NativeObserver's two-point WindowServer rounding allowance.
        // The VM can report a 685-point cover over a 686-point target frame.
        let tolerance = 2.0;
        assert!(
            cover.bounds.x <= bounds.x + tolerance
                && cover.bounds.y <= bounds.y + tolerance
                && cover.bounds.x + cover.bounds.width + tolerance >= bounds.x + bounds.width
                && cover.bounds.y + cover.bounds.height + tolerance >= bounds.y + bounds.height,
            "cover={:?}, current_target={:?}, initial_target={bounds:?}",
            cover.bounds,
            target.bounds
        );
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let state = read();
        let desktop = DesktopObserver::new(NativeObserver::new(), target)
            .snapshot()
            .unwrap();
        // Native geometry proves coverage. Electron's Page Visibility state
        // also depends on its version and renderer configuration, so a covered
        // window is not necessarily reported as hidden. Retain that observed
        // state rather than claiming this row proves renderer throttling.
        let visibility_ready = if covered {
            matches!(state["visibility"].as_str(), Some("visible" | "hidden"))
        } else {
            state["visibility"] == "visible"
        };
        if visibility_ready
            && state["focused"] == false
            && desktop.foreground == Some(sentinel.target().pid as u64)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture posture: {state}; native={desktop:?}; expected={:?}",
            sentinel.target()
        );
        thread::sleep(Duration::from_millis(50));
    }
    let before = read();
    let expected_visibility = before["visibility"].clone();
    assert_eq!(before["top"], 0);
    let action = || {
        // Keep the unproven untargeted route refused before any key dispatch.
        let untargeted = fixture.driver.call(
            "scroll",
            serde_json::json!({
                "pid":fixture.pid,"window_id":fixture.window_id,"direction":"down",
                "amount":1,"delivery_mode":"background"
            }),
        );
        assert!(
            untargeted.is_error(),
            "untargeted route must remain refused: {}",
            untargeted.raw
        );
        assert_eq!(untargeted.structured()["code"], "background_unavailable");
        // A cross-process window id must still refuse without mutation.
        let mismatch = fixture.driver.call(
            "scroll",
            serde_json::json!({
                "pid":fixture.pid,"window_id":sentinel.target().native_id,
                "x":x,"y":y,"direction":"down","delivery_mode":"background"
            }),
        );
        assert!(mismatch.is_error(), "{}", mismatch.raw);
        assert_eq!(read()["top"], 0);
        let outside = fixture.driver.call(
            "scroll",
            serde_json::json!({
                "pid":fixture.pid,"window_id":fixture.window_id,
                "x":100000,"y":100000,"direction":"down","delivery_mode":"background"
            }),
        );
        assert!(outside.is_error(), "{}", outside.raw);
        assert_eq!(read()["top"], 0);
        let mut arguments = serde_json::json!({
            "pid":fixture.pid,"window_id":fixture.window_id,"x":x,"y":y,
            "direction":"down","by":"page","amount":1,"delivery_mode":"background"
        });
        if element_target {
            // Resolve through the agent-facing AX response. CDP geometry above
            // remains only the pixel control's input and independent oracle.
            let snapshot = fixture.driver.call(
                "get_window_state",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":fixture.window_id,
                    "include_screenshot":false,"diff":false
                }),
            );
            assert!(!snapshot.is_error(), "{}", snapshot.raw);
            let matches: Vec<_> = snapshot.structured()["elements"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["label"] == "Message history")
                .cloned()
                .collect();
            assert_eq!(matches.len(), 1, "one AX message pane: {}", snapshot.text());
            let old_token = matches[0]["element_token"].as_str().unwrap();
            let refreshed = fixture.driver.call(
                "get_window_state",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":fixture.window_id,
                    "include_screenshot":false,"diff":false
                }),
            );
            assert!(!refreshed.is_error(), "{}", refreshed.raw);
            let stale = fixture.driver.call(
                "scroll",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":fixture.window_id,
                    "element_token":old_token,"x":x,"y":y,"direction":"down",
                    "delivery_mode":"background"
                }),
            );
            assert!(
                stale.is_error(),
                "stale element must not fall back to pixels: {}",
                stale.raw
            );
            let current = refreshed.structured();
            let find = |label: &str| {
                current["elements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["label"] == label)
                    .unwrap_or_else(|| panic!("missing {label}: {}", refreshed.text()))
                    ["element_token"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let token = find("Message history");
            let wrong_window = fixture.driver.call(
                "scroll",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":sentinel.target().native_id,
                    "element_token":token,"direction":"down","delivery_mode":"background"
                }),
            );
            assert!(
                wrong_window.is_error(),
                "wrong window must refuse: {}",
                wrong_window.raw
            );
            let offscreen = fixture.driver.call(
                "scroll",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":fixture.window_id,
                    "element_token":find("Offscreen history"),"direction":"down",
                    "delivery_mode":"background"
                }),
            );
            assert!(
                offscreen.is_error(),
                "offscreen target must not be revealed: {}",
                offscreen.raw
            );
            let unchanged = read();
            assert_eq!(unchanged["top"], 0);
            assert_eq!(unchanged["outer"], 0);
            assert_eq!(unchanged["events"].as_array().unwrap().len(), 0);
            arguments.as_object_mut().unwrap().remove("x");
            arguments.as_object_mut().unwrap().remove("y");
            arguments["element_token"] = serde_json::json!(token);
        }
        let scroll_started = Instant::now();
        let response = fixture.driver.call("scroll", arguments);
        let scroll_elapsed = scroll_started.elapsed();
        assert!(!response.is_error(), "{}", response.raw);
        assert_eq!(response.action_effect(), Some("unverifiable"));
        assert_eq!(response.action_route(), Some("synthetic_events"));
        let deadline = Instant::now() + Duration::from_secs(2);
        // Rows load into a buffer before smooth scrolling brings them into
        // view, so wait for a newly rendered row to be visible, not only loaded.
        let shows_new_row = |state: &serde_json::Value| {
            state["visible_rows"].as_array().is_some_and(|rows| {
                rows.iter().filter_map(|row| row.as_str()).any(|row| {
                    row.strip_prefix("Message ")
                        .and_then(|index| index.parse::<u64>().ok())
                        .is_some_and(|index| index > before["last"].as_u64().unwrap())
                })
            })
        };
        let after = loop {
            let state = read();
            let done = if covered {
                state["last"].as_u64() > before["last"].as_u64()
            } else {
                shows_new_row(&state)
            };
            if done || Instant::now() >= deadline {
                break state;
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(after["outer"], 0);
        assert_eq!(after["focused"], false);
        assert_eq!(after["visibility"], expected_visibility);
        if !covered {
            assert!(after["top"].as_f64().unwrap() > 0.0, "{after}");
            assert!(
                after["last"].as_u64() > before["last"].as_u64(),
                "new rows: {after}"
            );
            let events = after["events"].as_array().unwrap();
            assert!(!events.is_empty());
            assert!(events.iter().all(|e| e["trusted"] == true));
            // Pick an actually visible row that was absent from the initial
            // virtualized DOM, then require one fresh agent-facing AX read to
            // expose it. Wheel receipts and DOM growth alone do not prove this.
            let new_row = after["visible_rows"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|row| row.as_str())
                .find(|row| {
                    row.strip_prefix("Message ")
                        .and_then(|index| index.parse::<u64>().ok())
                        .is_some_and(|index| index > before["last"].as_u64().unwrap())
                })
                .unwrap_or_else(|| {
                    panic!("scroll must reveal a newly rendered message inside the viewport: before={before} after={after}")
                });
            // Chromium updates its AX tree after the DOM, so the agent may
            // need a second look. Bound it and record how many reads it took.
            let read_started = Instant::now();
            let ax_deadline = read_started + Duration::from_millis(1500);
            let mut reads = 0;
            let outline = loop {
                reads += 1;
                let snapshot = fixture.driver.call(
                    "get_window_state",
                    serde_json::json!({
                        "pid":fixture.pid,"window_id":fixture.window_id,"capture_mode":"ax",
                        "diff":false
                    }),
                );
                assert!(!snapshot.is_error(), "{}", snapshot.raw);
                // A message row is display text: the outline carries it, while
                // structured elements hold only actionable rows.
                let state = snapshot.structured();
                let in_elements = state["elements"]
                    .as_array()
                    .expect("structured accessibility elements")
                    .iter()
                    .any(|element| element["label"] == new_row || element["value"] == new_row);
                let outline = state["tree_markdown"].as_str().unwrap_or_default().to_owned();
                if in_elements || outline.contains(new_row) || Instant::now() >= ax_deadline {
                    break outline;
                }
                thread::sleep(Duration::from_millis(100));
            };
            let read_elapsed = read_started.elapsed();
            assert!(
                outline.contains(new_row),
                "AX reads omitted newly visible {new_row} for 1.5s: {outline}"
            );
            eprintln!(
                "[electron-scroll-timing] {}",
                serde_json::json!({
                    "scroll_call_ms":scroll_elapsed.as_secs_f64()*1000.0,
                    "following_ax_read_ms":read_elapsed.as_secs_f64()*1000.0,
                    "ax_reads":reads,
                    "new_row":new_row
                })
            );
        }
        // Covered renderers may stall. This row proves honest dispatch status
        // and isolation only, never certifies scrolling from a tool response.
        eprintln!("[electron-scroll] covered={covered} before={before} after={after}");
    };
    if covered {
        sentinel
            .observe_background(target, action)
            .expect("covered dispatch isolation");
    } else {
        sentinel
            .observe_desktop(|| {
                let mut observer = DesktopObserver::new(NativeObserver::new(), target);
                let (_, delta) = observer
                    .observe(
                        &[OracleKind::Focus, OracleKind::ZOrder, OracleKind::Cursor],
                        action,
                    )
                    .unwrap();
                delta.ensure_supported().unwrap();
                assert!(delta.violations().is_empty(), "{delta:?}");
                assert_eq!(delta.before.foreground, Some(sentinel.target().pid as u64));
                assert_eq!(delta.after.foreground, Some(sentinel.target().pid as u64));
            })
            .expect("visible background isolation");
    }
}

#[test]
#[ignore = "requires the built Electron fixture and an authorized macOS daemon"]
fn visible_background_scroll_loads_rows() {
    run_scroll(false, false);
}

#[test]
#[ignore = "requires the built Electron fixture and an authorized macOS daemon"]
fn covered_background_dispatch_stays_unverified() {
    run_scroll(true, false);
}

#[test]
#[ignore = "requires the built Electron fixture and an authorized macOS daemon"]
fn visible_background_element_scroll_loads_rows() {
    run_scroll(false, true);
}

#[test]
#[ignore = "requires the built Electron fixture and an authorized macOS daemon"]
fn covered_background_element_dispatch_stays_unverified() {
    run_scroll(true, true);
}

#[test]
#[ignore = "requires the built Electron fixture and an authorized macOS daemon"]
fn native_foreground_observer_detects_a_real_transition() {
    let _lock = STANDALONE_BROWSER_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let spec = BrowserSpec {
        name: "electron".into(),
        executable: cua_driver_testkit::harness_app(
            "harness-electron",
            "CuaTestHarness.Electron.app/Contents/MacOS/Electron",
        ),
    };
    let html = standalone_fixture_html().replace(
        r#"<main class="harness-grid">"#,
        r#"<script>
fetch(window.__CUA_E2E_FIXTURE_JOURNAL_URL,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({ready:'WEB_HARNESS_MARKER_v1'})});
</script><main class="harness-grid">"#,
    );
    let mut fixture = launch_browser_with_html(&spec, "foreground-observer-canary", html);
    let sentinel = ForegroundSentinel::launch(&mut fixture.driver);
    let target = TargetWindow {
        pid: fixture.pid,
        native_id: fixture.window_id,
    };
    sentinel
        .prepare_background_observation(&mut fixture.driver, target)
        .unwrap();
    let mut observer = DesktopObserver::new(NativeObserver::new(), target);
    assert_eq!(
        observer.snapshot().unwrap().foreground,
        Some(sentinel.target().pid as u64)
    );
    let fresh_foreground = || {
        let output = std::process::Command::new("/usr/bin/osascript")
            .args(["-l", "JavaScript", "-e", "ObjC.import('AppKit'); $.NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier"])
            .output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
    };
    // A new process supplies independent current foreground evidence. No GUI
    // input goes through that process; activation stays in the fixture setup.
    assert_eq!(fresh_foreground(), sentinel.target().pid as u64);
    let (_, delta) = observer
        .observe(&[OracleKind::Focus], || {
            let front = fixture.driver.call(
                "bring_to_front",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":fixture.window_id
                }),
            );
            assert!(!front.is_error(), "{}", front.raw);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let fresh = fresh_foreground();
                if fresh == fixture.pid as u64 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture activation did not land: {fresh}"
                );
                thread::sleep(Duration::from_millis(50));
            }
        })
        .unwrap();
    eprintln!(
        "[native-focus-canary] fresh_pid={} delta={delta:?}",
        fresh_foreground()
    );
    assert_eq!(
        delta.after.foreground,
        Some(fixture.pid as u64),
        "{delta:?}"
    );
    assert!(
        !delta.violations().is_empty(),
        "the intentional focus change must fail the focus oracle"
    );
}
