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
</script><main class="harness-grid">"#,
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
        assert!(
            cover.bounds.x <= bounds.x
                && cover.bounds.y <= bounds.y
                && cover.bounds.x + cover.bounds.width >= bounds.x + bounds.width
                && cover.bounds.y + cover.bounds.height >= bounds.y + bounds.height
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
                    "include_screenshot":false
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
            let token = matches[0]["element_token"].as_str().unwrap();
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
        let after = loop {
            let state = read();
            if state["last"].as_u64() > before["last"].as_u64() || Instant::now() >= deadline {
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
                .expect("scroll must reveal a newly rendered message inside the viewport");
            let read_started = Instant::now();
            let snapshot = fixture.driver.call(
                "get_window_state",
                serde_json::json!({
                    "pid":fixture.pid,"window_id":fixture.window_id,"capture_mode":"ax"
                }),
            );
            let read_elapsed = read_started.elapsed();
            assert!(!snapshot.is_error(), "{}", snapshot.raw);
            assert!(
                snapshot.structured()["elements"]
                    .as_array()
                    .expect("structured accessibility elements")
                    .iter()
                    .any(|element| element["label"] == new_row || element["value"] == new_row),
                "fresh AX read omitted newly visible {new_row}: {}",
                snapshot.text()
            );
            eprintln!(
                "[electron-scroll-timing] {}",
                serde_json::json!({
                    "scroll_call_ms":scroll_elapsed.as_secs_f64()*1000.0,
                    "following_ax_read_ms":read_elapsed.as_secs_f64()*1000.0,
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
