//! A connection that closes while its hooked turn is open, with a call still
//! in flight: the sessions' teardown waits for that call, but the call pushes
//! no frame (it would bring the panel back) and the late teardown still ends
//! the panel quietly, never as a finished session. A connection that never
//! reported a turn keeps today's rule.
//!
//! Own test binary: the PiP hooks are process-global `OnceLock`s.

use async_trait::async_trait;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use cua_driver_core::{pip_hook, pip_turn, session};
use serde_json::{json, Value};
use std::sync::Mutex;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log(entry: String) {
    LOG.lock().unwrap().push(entry);
}

fn label(key: &str) -> &str {
    key.rsplit(':').next().unwrap_or(key)
}

/// An action that just succeeds.
struct Acts(ToolDef);

#[async_trait]
impl Tool for Acts {
    fn def(&self) -> &ToolDef {
        &self.0
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        ToolResult::text("done")
    }
}

/// An action during which its connection closes, as the daemon handles a
/// proxy's EOF: the turn goes, then the transport's sessions end (deferred,
/// since this call is in flight).
struct ConnectionClosesMeanwhile(ToolDef);

#[async_trait]
impl Tool for ConnectionClosesMeanwhile {
    fn def(&self) -> &ToolDef {
        &self.0
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let owner = args["_transport_session_id"].as_str().unwrap().to_owned();
        pip_turn::forget(&owner);
        session::end_sessions_for_owner(&owner, session::SessionEndReason::ProcessExit);
        ToolResult::text("done")
    }
}

fn def(name: &str) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: "fake".into(),
        input_schema: json!({"type": "object"}),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    }
}

async fn call(registry: &ToolRegistry, tool: &str, transport: &str, session: &str) {
    registry
        .invoke_from_trusted_adapter(
            tool,
            json!({"pid": 42, "window_id": 7, "x": 1, "y": 1, "session": session,
                "_transport_session_id": transport}),
        )
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_flight_at_an_open_turns_disconnect_brings_back_no_panel_and_no_finish() {
    std::env::remove_var(cua_driver_core::policy::POLICY_FILE_ENV);
    std::env::remove_var(cua_driver_core::policy::MANAGED_POLICY_FILE_ENV);
    pip_hook::set_pip_push_fn(|frame| log(format!("frame {}", frame.session_label.unwrap())));
    pip_turn::set_pip_turn_fn(|key, turn| log(format!("turn {} {turn:?}", label(key))));
    // What the daemon's session end hook decides for the panel.
    session::register_session_end_hook(|key| {
        log(format!(
            "end {} quiet={}",
            label(key),
            pip_turn::ends_quietly(key)
        ))
    });

    let mut registry = ToolRegistry::new();
    registry.register_session_tools();
    registry.register(Box::new(Acts(def("scroll"))));
    registry.register(Box::new(ConnectionClosesMeanwhile(def("click"))));

    // Hooked: the turn is open when the connection closes mid-call.
    registry
        .invoke_from_trusted_adapter(
            "pip_turn",
            json!({"event": "UserPromptSubmit", "_transport_session_id": "proxy-hooked"}),
        )
        .await;
    call(&registry, "scroll", "proxy-hooked", "alpha").await;
    call(&registry, "click", "proxy-hooked", "alpha").await;
    // Never hooked: today's connection-close rule.
    call(&registry, "scroll", "proxy-plain", "beta").await;
    call(&registry, "click", "proxy-plain", "beta").await;

    let log = LOG.lock().unwrap().clone();
    let count = |entry: &str| log.iter().filter(|logged| *logged == entry).count();
    assert_eq!(count("turn alpha Open"), 1, "{log:?}");
    assert_eq!(count("turn alpha End { finished: false }"), 1, "{log:?}");
    assert_eq!(
        count("frame alpha"),
        1,
        "the in-flight call pushed a frame: {log:?}"
    );
    assert_eq!(count("end alpha quiet=true"), 1, "{log:?}");
    assert_eq!(count("frame beta"), 2, "{log:?}");
    assert_eq!(count("end beta quiet=false"), 1, "{log:?}");
    assert!(
        !log.iter().any(|logged| logged.starts_with("turn beta")),
        "{log:?}"
    );
    // Remembered only until the session's teardown ran.
    assert!(!log
        .iter()
        .any(|logged| logged.contains("quiet=true") && logged.contains("beta")));
}
