//! Private, opt-in observations. Never issues ordering or presentation commands.
//! Keep raw WindowInfo (which contains titles) out of serialized diagnostics.

use super::{DisplayId, ZOrderRoute};
use crate::windows::WindowInfo;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static ENABLED: OnceLock<bool> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const OBSERVATION_LIMIT: u32 = 120;
const OBSERVATION_WINDOW: Duration = Duration::from_secs(2);

thread_local! {
    // Main-thread observations only, bounded to one probe per active display.
    static PROBES: RefCell<HashMap<DisplayId, Probe>> = RefCell::new(HashMap::new());
}

pub(super) fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var_os("CUA_PRIVATE_CURSOR_ORDER_TRACE").as_deref()
            == Some(std::ffi::OsStr::new("1"))
    })
}

#[derive(Clone)]
pub(super) struct Trace {
    id: u64,
    route: ZOrderRoute,
    started: Instant,
}

struct Probe {
    trace: Trace,
    remaining: u32,
}

pub(super) fn decision(
    route: &ZOrderRoute,
    cached: bool,
    repin_due: bool,
    cursor_commanded: bool,
    selected: bool,
) -> Option<Trace> {
    if !enabled() || (!selected && !cursor_commanded) {
        return None;
    }
    let trace = Trace {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        route: route.clone(),
        started: Instant::now(),
    };
    trace.event(
        "route_decision",
        json!({
            "cached": cached, "repin_due": repin_due,
            "cursor_commanded": cursor_commanded, "selected": selected,
            "decision": if selected { "enqueue" } else { "cache_skip" },
            "selected_command": if !selected { "none" } else if route.target_wid.is_some() {
                "order_relative_pending_enqueue_predicate"
            } else { "order_front" },
        }),
    );
    Some(trace)
}

impl Trace {
    pub(super) fn event(&self, phase: &str, data: Value) {
        let record = json!({
            "trace_id": self.id, "phase": phase,
            "process_pid": std::process::id(),
            "epoch_us": SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_micros()),
            "elapsed_us": self.started.elapsed().as_micros(),
            "generation": self.route.generation, "display_id": self.route.display_id,
            "target_cg_window_id": self.route.target_wid, "data": data,
        });
        // Private daemon stderr, independent of CUA_LOG. Ignore IO failures:
        // diagnostics must not turn an action into an error or panic.
        let _ = writeln!(std::io::stderr().lock(), "CUA_CURSOR_ORDER_TRACE {record}");
    }

    pub(super) fn enqueue(&self, pid: Option<i32>, windows: &[WindowInfo], raise: bool) {
        self.event("enqueue_predicate", json!({
            "foreground_pid": pid,
            "eligibility": eligibility(self.route.target_wid, pid, windows),
            "predicate_windows": window_fields(windows),
            "selected_command": if raise { "order_relative_then_order_front" } else { "order_relative" },
            "raise_front": raise,
        }));
    }

    pub(super) fn outcome(&self, command_ran: bool, route_recorded: bool) {
        self.event(
            "apply_outcome",
            json!({
                "command_ran": command_ran, "route_recorded": route_recorded,
                "reason": match (command_ran, route_recorded) {
                    (false, _) => "admission_busy_before_command",
                    (true, false) => "admission_busy_after_command",
                    (true, true) => "void_appkit_return_and_route_recorded",
                },
            }),
        );
    }

    pub(super) fn skipped(self) {
        self.sample("cache_skip_main");
        self.observe_presentations();
    }

    pub(super) fn observe_presentations(self) {
        require_main();
        PROBES.with(|probes| {
            let mut probes = probes.borrow_mut();
            probes.retain(|_, probe| {
                probe.trace.route.generation == self.route.generation
                    && probe.trace.started.elapsed() < OBSERVATION_WINDOW
            });
            probes.insert(
                self.route.display_id,
                Probe {
                    trace: self,
                    remaining: OBSERVATION_LIMIT,
                },
            );
        });
    }

    pub(super) fn sample(&self, phase: &str) {
        require_main();
        // Copy route identity while locked. Enumeration and logging follow after
        // releasing both renderer and host locks. No inbox lock is acquired.
        let current = super::RENDER.lock().unwrap().as_ref().and_then(|map| {
            super::z_order_routes(map)
                .into_iter()
                .find(|r| r.display_id == self.route.display_id)
        });
        let number = {
            let host = super::HOST.lock().unwrap();
            host.as_ref()
                .filter(|h| h.generation == self.route.generation)
                .and_then(|h| h.surfaces.get(&self.route.display_id))
                .map(|s| unsafe { window_number(s.win_ptr as *mut objc2::runtime::AnyObject) })
        };
        self.capture(phase, number, json!({
            "current_generation": super::DISPLAY_GENERATION.load(Ordering::Acquire),
            "current_route_matches": current.as_ref() == Some(&self.route),
            "current_route": current.map(|r| json!({
                "generation": r.generation, "display_id": r.display_id, "target_cg_window_id": r.target_wid,
            })),
        }));
    }

    // Called inside the native closure, after apply_surface_route releases the
    // inbox. The caller owns HOST, so this must not acquire HOST or RENDER.
    pub(super) unsafe fn native_sample(&self, phase: &str, win: *mut objc2::runtime::AnyObject) {
        require_main();
        self.capture(phase, Some(window_number(win)), Value::Null);
    }

    fn capture(&self, phase: &str, number: Option<i64>, state: Value) {
        let started = self.started.elapsed().as_micros();
        let pid = crate::apps::frontmost_pid();
        let windows = crate::windows::all_windows_any_layer();
        let ended = self.started.elapsed().as_micros();
        // Verify the AppKit number against an actual same-process CG record.
        // A mismatch stays null; do not silently invent a global window ID.
        let overlay = windows
            .iter()
            .find(|w| Some(i64::from(w.window_id)) == number && w.pid == std::process::id() as i32)
            .map(|w| w.window_id);
        let eligible: Vec<_> = windows
            .iter()
            .filter(|w| w.is_on_screen && w.layer == 0)
            .cloned()
            .collect();
        self.event(
            phase,
            json!({
                "sample_start_us": started, "sample_end_us": ended,
                "foreground_pid": pid, "appkit_window_number": number,
                "overlay_cg_window_id": overlay,
                "eligibility": eligibility(self.route.target_wid, pid, &eligible),
                "stack_order": "front_to_back_larger_z_is_front",
                "stack_empty_or_unavailable": windows.is_empty(),
                "stack": window_fields(&windows), "state": state,
            }),
        );
    }
}

fn require_main() {
    assert!(objc2_foundation::MainThreadMarker::new().is_some());
}

unsafe fn window_number(win: *mut objc2::runtime::AnyObject) -> i64 {
    objc2::msg_send![win, windowNumber]
}

pub(super) fn presented(generation: u64, display: DisplayId) {
    if !enabled() {
        return;
    }
    require_main();
    let trace = PROBES.with(|probes| {
        let mut probes = probes.borrow_mut();
        let probe = probes.get_mut(&display)?;
        if probe.trace.route.generation != generation
            || probe.trace.started.elapsed() >= OBSERVATION_WINDOW
            || probe.remaining == 0
        {
            let probe = probes.remove(&display).unwrap();
            probe.trace.event(
                "observation_end",
                json!({"reason": "generation_age_or_sample_limit"}),
            );
            return None;
        }
        probe.remaining -= 1;
        Some(probe.trace.clone())
    });
    if let Some(trace) = trace {
        trace.sample("after_presentation_callback");
    }
}

fn window_fields(windows: &[WindowInfo]) -> Vec<Value> {
    windows
        .iter()
        .map(|w| {
            json!({
                "cg_window_id": w.window_id, "pid": w.pid,
                "layer": w.layer, "z_index": w.z_index, "on_screen": w.is_on_screen,
                "bounds": { "x": w.bounds.x, "y": w.bounds.y,
                    "width": w.bounds.width, "height": w.bounds.height },
            })
        })
        .collect()
}

// Explain the existing predicate without changing it. The actual predicate
// result is logged independently, and tests keep the explanation aligned.
fn eligibility(target: Option<u64>, pid: Option<i32>, windows: &[WindowInfo]) -> Value {
    let Some(id) = target else {
        return json!({"reason": "untargeted_order_front"});
    };
    let target = windows.iter().find(|w| u64::from(w.window_id) == id);
    let top = target.and_then(|target| {
        windows
            .iter()
            .filter(|w| {
                w.is_on_screen
                    && w.layer == 0
                    && w.pid == target.pid
                    && w.bounds.width > 1.0
                    && w.bounds.height > 1.0
            })
            .max_by_key(|w| w.z_index)
    });
    let reason = match target {
        None => "target_absent_from_predicate_input",
        Some(w) if !w.is_on_screen => "target_off_screen",
        Some(w) if w.layer != 0 => "target_nonzero_layer",
        Some(w) if pid != Some(w.pid) => "foreground_pid_mismatch_or_unknown",
        Some(_) if top.is_none() => "no_eligible_target_app_window",
        Some(_) if top.map(|w| u64::from(w.window_id)) != Some(id) => {
            "another_target_app_window_ahead"
        }
        Some(_) => "eligible",
    };
    json!({
        "reason": reason,
        "raise_front": super::target_is_frontmost_visible_window(id, pid, windows),
        "target": target.map(|w| window_fields(std::slice::from_ref(w)).remove(0)),
        "highest_eligible_target_app_window": top.map(|w| w.window_id),
        "candidate_checks": windows.iter().map(|w| json!({
            "cg_window_id": w.window_id,
            "reason": if !w.is_on_screen { "off_screen" }
                else if w.layer != 0 { "nonzero_layer" }
                else if target.is_none_or(|target| target.pid != w.pid) { "different_target_pid_or_missing_target" }
                else if !(w.bounds.width > 1.0 && w.bounds.height > 1.0) { "bounds_not_larger_than_one" }
                else { "eligible_target_app_candidate" },
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u32, pid: i32, z: usize) -> WindowInfo {
        WindowInfo {
            window_id: id,
            pid,
            z_index: z,
            app_name: "PRIVATE_APP_NAME".into(),
            title: "PRIVATE_WINDOW_TITLE".into(),
            bounds: crate::windows::WindowBounds {
                x: -50.0,
                y: 33.0,
                width: 756.0,
                height: 882.0,
            },
            layer: 0,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn private_order_trace_serializes_only_numeric_window_fields() {
        let windows = vec![window(26441, 1013, 151), window(30457, 64510, 33)];
        let records = window_fields(&windows);
        assert_eq!(
            records[0],
            json!({
                "cg_window_id": 26441, "pid": 1013, "layer": 0,
                "z_index": 151, "on_screen": true,
                "bounds": {"x": -50.0, "y": 33.0, "width": 756.0, "height": 882.0},
            })
        );
        let encoded = json!({"stack": records, "eligibility": eligibility(Some(26441), Some(1013), &windows)}).to_string();
        assert!(!encoded.contains("PRIVATE"));
        assert!(!encoded.contains("title"));
        assert!(!encoded.contains("app_name"));
    }

    #[test]
    fn private_order_trace_explanation_matches_existing_predicate() {
        let mut target = window(10, 100, 20);
        let mut cases = vec![
            (Some(100), vec![], "target_absent_from_predicate_input"),
            (Some(100), vec![target.clone()], "eligible"),
            (
                None,
                vec![target.clone()],
                "foreground_pid_mismatch_or_unknown",
            ),
            (
                Some(200),
                vec![target.clone()],
                "foreground_pid_mismatch_or_unknown",
            ),
            (
                Some(100),
                vec![target.clone(), window(11, 100, 30)],
                "another_target_app_window_ahead",
            ),
            (
                Some(100),
                vec![target.clone(), window(11, 200, 30)],
                "eligible",
            ),
        ];
        target.is_on_screen = false;
        cases.push((Some(100), vec![target.clone()], "target_off_screen"));
        target.is_on_screen = true;
        target.layer = 1;
        cases.push((Some(100), vec![target.clone()], "target_nonzero_layer"));
        target.layer = 0;
        target.bounds.width = 1.0;
        cases.push((
            Some(100),
            vec![target.clone()],
            "no_eligible_target_app_window",
        ));
        target.bounds.width = f64::NAN;
        cases.push((Some(100), vec![target], "no_eligible_target_app_window"));
        for (pid, windows, reason) in cases {
            let result = eligibility(Some(10), pid, &windows);
            assert_eq!(result["reason"], reason);
            assert_eq!(result["raise_front"], reason == "eligible");
        }
        assert_eq!(
            eligibility(None, None, &[])["reason"],
            "untargeted_order_front"
        );
    }
}
