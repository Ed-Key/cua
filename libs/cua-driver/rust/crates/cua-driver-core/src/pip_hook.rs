//! PiP frame-push hook — registered once by `main.rs` when the
//! `--experimental-pip` flag is on argv.
//!
//! The trait + factory live in the `pip-preview` crate so the platform
//! backends can implement them without depending on `cua-driver-core`.
//! What lives here is just the per-process callback that the tool
//! dispatcher uses to push frames after each successful tool call —
//! a thin shim so `tool.rs` doesn't need to know about `pip-preview`
//! directly and we keep the dependency graph one-directional.
//!
//! Frames carry identity only, never pixels: the dispatcher must not
//! capture inside an action's own dispatch. Backends capture the target
//! on their own worker through `recording::screenshot_for` (the same
//! `SCREENSHOT_FN` the recorder uses).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Synthesized per-call frame payload. Kept structurally identical
/// to `pip_preview::PipFrame` — duplicated here to keep `cua-driver-core`
/// from importing `pip-preview` (the dependency would be circular once
/// platform backends pull both crates in).
pub struct PipHookFrame {
    pub action_label: String,
    pub timestamp_ms: u64,
    /// Private runtime session key (`_session_id`, or "default"). Keys the
    /// per-session panel and its color; never shown to the user.
    pub session_key: String,
    /// Public, caller-chosen session label. Display only.
    pub session_label: Option<String>,
    /// Self-reported MCP client name from `initialize` (e.g. "Claude Code").
    pub client_name: Option<String>,
    /// A process inside the MCP client's process tree (the stdio proxy that
    /// opened the daemon connection). Backends walk up from it to the app.
    pub client_pid: Option<i32>,
    pub target_pid: Option<i32>,
    pub target_window_id: Option<u32>,
}

/// What the daemon knows about the MCP client behind one transport session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipClient {
    pub name: Option<String>,
    pub pid: Option<i32>,
}

/// Transport session id (as the proxy minted it, without the runtime prefix)
/// → client identity. Filled only while a PiP backend is registered.
static CLIENTS: Mutex<Option<HashMap<String, PipClient>>> = Mutex::new(None);

fn with_client(transport_session: &str, f: impl FnOnce(&mut PipClient)) {
    if !pip_enabled() || transport_session.is_empty() {
        return;
    }
    let mut guard = CLIENTS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard
        .get_or_insert_with(HashMap::new)
        .entry(transport_session.to_owned())
        .or_default());
}

/// Record the peer pid of a transport session's control connection.
pub fn note_client_pid(transport_session: &str, pid: i32) {
    with_client(transport_session, |client| client.pid = Some(pid));
}

/// Record the MCP client name the proxy read from `initialize`. Bounded to
/// 64 characters; display only.
pub fn note_client_name(transport_session: &str, name: &str) {
    let name: String = name.trim().chars().take(64).collect();
    if name.is_empty() {
        return;
    }
    with_client(transport_session, |client| client.name = Some(name));
}

/// Drop a transport session's client identity when its connection closes.
pub fn forget_client(transport_session: &str) {
    if let Some(map) = CLIENTS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        map.remove(transport_session);
    }
}

pub fn client_for(transport_session: &str) -> PipClient {
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(transport_session).cloned())
        .unwrap_or_default()
}

type PipPushFnBox = Box<dyn Fn(PipHookFrame) + Send + Sync>;
static PIP_PUSH_FN: OnceLock<PipPushFnBox> = OnceLock::new();

/// Register the platform-side push callback. `main.rs` calls this
/// once after starting the PiP backend.
pub fn set_pip_push_fn(f: impl Fn(PipHookFrame) + Send + Sync + 'static) {
    let _ = PIP_PUSH_FN.set(Box::new(f));
}

/// True when a PiP backend is wired up. Tool dispatcher uses this to
/// skip the screenshot-bytes path when nothing would consume the
/// frame (avoiding wasted capture work in the common --pip-off case).
pub fn pip_enabled() -> bool {
    PIP_PUSH_FN.get().is_some()
}

/// Lifecycle, configuration, recording and other non-GUI tools never
/// update a panel, even when they happen to carry a pid.
fn pip_meta_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "start_session"
            | "end_session"
            | "escalate_session"
            | "set_config"
            | "start_recording"
            | "stop_recording"
            | "replay_trajectory"
            | "install_ffmpeg"
            | "install_extension"
            | "kill_app"
    ) || tool_name.starts_with("get_")
        || tool_name.starts_with("list_")
        || tool_name.starts_with("set_agent_cursor")
}

/// Whether this frame should reach the PiP at all: only GUI actions aimed
/// at a target (pid or window) by an agent session, meaning one with a
/// public session label or a known MCP client. One-shot CLI calls and
/// implicit SDK sessions have neither and get no panel.
pub fn pip_frame_wanted(tool_name: &str, frame: &PipHookFrame) -> bool {
    let agent = frame.session_label.is_some() || frame.client_name.is_some();
    let targeted = frame.target_pid.is_some() || frame.target_window_id.is_some();
    agent && targeted && !pip_meta_tool(tool_name)
}

/// Push a frame to the PiP window. No-op when no backend is registered.
pub fn push_pip_frame(frame: PipHookFrame) {
    if let Some(f) = PIP_PUSH_FN.get() {
        f(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(label: Option<&str>, client: Option<&str>, pid: Option<i32>) -> PipHookFrame {
        PipHookFrame {
            action_label: "click".into(),
            timestamp_ms: 0,
            session_key: "k".into(),
            session_label: label.map(str::to_owned),
            client_name: client.map(str::to_owned),
            client_pid: None,
            target_pid: pid,
            target_window_id: None,
        }
    }

    #[test]
    fn only_labeled_or_client_known_sessions_get_a_panel() {
        assert!(pip_frame_wanted(
            "click",
            &frame(Some("alpha"), None, Some(1))
        ));
        assert!(pip_frame_wanted(
            "click",
            &frame(None, Some("Claude Code"), Some(1))
        ));
        assert!(!pip_frame_wanted("click", &frame(None, None, Some(1))));
    }

    #[test]
    fn lifecycle_meta_and_untargeted_calls_get_no_frame() {
        for tool in [
            "start_session",
            "end_session",
            "get_session_state",
            "list_sessions",
            "start_recording",
            "stop_recording",
            "set_config",
            "set_agent_cursor_enabled",
            "kill_app",
        ] {
            assert!(
                !pip_frame_wanted(tool, &frame(Some("alpha"), None, Some(1))),
                "{tool}"
            );
        }
        assert!(!pip_frame_wanted(
            "click",
            &frame(Some("alpha"), None, None)
        ));
        assert!(pip_frame_wanted(
            "type_text",
            &frame(Some("alpha"), None, Some(1))
        ));
    }
}
