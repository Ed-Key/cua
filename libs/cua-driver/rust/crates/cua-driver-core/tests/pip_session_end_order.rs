//! A session that ends while one of its calls is in flight defers the end to
//! the call's lifecycle guard; the guard's drop runs the session-end hooks
//! (which drop the PiP panel) synchronously. The PiP events for that call must
//! be enqueued before then, or the ending panel never sees the session's final
//! verification or action.
//!
//! Own test binary: the PiP hooks are process-global `OnceLock`s.

use async_trait::async_trait;
use cua_driver_core::pip_hook;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session;
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};
use std::sync::Mutex;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log(entry: String) {
    LOG.lock().unwrap().push(entry);
}

/// A tool whose session is ended (by the caller, elsewhere) while it runs.
struct EndsItsSession {
    def: ToolDef,
    output: Value,
}

#[async_trait]
impl Tool for EndsItsSession {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = args["_session_id"].as_str().unwrap().to_owned();
        session::end_session(&session);
        ToolResult::text("done").with_structured(self.output.clone())
    }
}

fn def(name: &str, read_only: bool) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: "fake".into(),
        input_schema: json!({"type": "object"}),
        read_only,
        destructive: false,
        idempotent: false,
        open_world: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pip_events_of_a_call_reach_the_queue_before_its_session_end() {
    pip_hook::set_pip_push_fn(|frame| log(format!("frame {}", frame.session_label.unwrap())));
    pip_hook::set_pip_verify_fn(|verification| {
        let label = verification.claims[0].label.clone();
        log(format!("verify {label}"));
    });
    session::register_session_end_hook(|session| {
        if let Some(label) = session.rsplit(':').next() {
            log(format!("end {label}"));
        }
    });

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EndsItsSession {
        def: def("verify_state", true),
        output: json!({"status": "unsatisfied", "stable": false, "elapsed_ms": 3,
            "samples": 1, "predicates": [{"index": 0, "status": "unsatisfied",
            "unknown_reason": null, "observed_json": null}]}),
    }));
    registry.register(Box::new(EndsItsSession {
        def: def("click", false),
        output: json!({}),
    }));

    registry
        .invoke(
            "verify_state",
            json!({"pid": 42, "window_id": 7, "session": "verifier",
                "expect": [{"window": {"exists": true}}]}),
        )
        .await;
    registry
        .invoke(
            "click",
            json!({"pid": 42, "window_id": 7, "x": 1, "y": 1, "session": "clicker"}),
        )
        .await;

    let log = LOG.lock().unwrap().clone();
    let position = |entry: &str| {
        log.iter()
            .position(|logged| logged == entry)
            .unwrap_or_else(|| panic!("{entry:?} missing from {log:?}"))
    };
    assert!(
        position("verify window open") < position("end verifier"),
        "{log:?}"
    );
    assert!(
        position("frame clicker") < position("end clicker"),
        "{log:?}"
    );
}
