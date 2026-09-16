//! Independent agent targets must survive a stalled preview consumer.
use async_trait::async_trait;
use cua_driver_core::{
    authorization::PermissionMode,
    pip_hook,
    protocol::ToolResult,
    session_authorization::{
        EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
    },
    session_tools::EndSessionTool,
    tool::{Tool, ToolDef, ToolRegistry},
};
use serde_json::{json, Value};
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

struct Input(ToolDef);
#[async_trait]
impl Tool for Input {
    fn def(&self) -> &ToolDef {
        &self.0
    }
    async fn invoke(&self, _: Value) -> ToolResult {
        ToolResult::text("landed")
    }
}
fn context() -> Arc<EffectiveAuthorizationContext> {
    SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Unrestricted],
            true,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(PermissionMode::Unrestricted, None)
    .unwrap()
}

#[test]
fn busy_preview_retains_each_agent_latest_target() {
    // Break caught: a single global pending target discards all but the last
    // agent while its observer is busy. Timeouts are deadlock watchdogs.
    let (published, snapshots) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    pip_hook::set_pip_session_observer_fn(move |snapshot| {
        published.send(snapshot).unwrap();
        if first {
            first = false;
            blocked.recv().unwrap();
        }
        true
    });
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EndSessionTool));
    registry.register(Box::new(Input(ToolDef {
        name: "click".into(),
        description: String::new(),
        input_schema: json!({"type":"object"}),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    })));
    let registry = Arc::new(registry);
    let a = context();
    let b = context();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(registry.invoke_with_context(
        "click",
        json!({"session":"Research","pid":42,"window_id":7,"x":10,"y":20}),
        a.clone(),
    ));
    assert_ne!(result.is_error, Some(true));
    let initial = snapshots.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(initial.len(), 1);
    let a_key = initial.keys().next().unwrap().clone();
    assert_eq!(initial[&a_key].window_id, Some(7));
    let (done, completed) = mpsc::channel();
    let input = registry.clone();
    let a_input = a.clone();
    let b_input = b.clone();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for (client, window) in [(a_input, 8), (b_input, 9)] {
            let result = runtime.block_on(input.invoke_with_context(
                "click",
                json!({"session":"Research","pid":42,"window_id":window,"x":10,"y":20}),
                client,
            ));
            assert_ne!(result.is_error, Some(true));
        }
        done.send(()).unwrap();
    });
    let returned = completed.recv_timeout(Duration::from_secs(3));
    release.send(()).unwrap();
    assert!(returned.is_ok(), "input waited for preview");
    worker.join().unwrap();
    let both = snapshots.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut targets: Vec<_> = both.values().map(|r| r.window_id.unwrap()).collect();
    targets.sort();
    assert_eq!(
        targets,
        vec![8, 9],
        "one agent replaced another's pending preview"
    );
    assert_eq!(both[&a_key].window_id, Some(8));
    let b_key = both.keys().find(|id| **id != a_key).unwrap().clone();
    assert_eq!(both[&b_key].window_id, Some(9));

    let ended = runtime.block_on(registry.invoke_with_context(
        "end_session",
        json!({"session":"Research"}),
        a.clone(),
    ));
    assert_ne!(ended.is_error, Some(true));
    let remaining = snapshots.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[&b_key].window_id, Some(9));
    let late = runtime.block_on(registry.invoke_with_context(
        "click",
        json!({"session":"Research","pid":42,"window_id":10,"x":10,"y":20}),
        a,
    ));
    assert_eq!(late.is_error, Some(true));
    assert!(snapshots.recv_timeout(Duration::from_millis(100)).is_err());

    let ended = runtime.block_on(registry.invoke_with_context(
        "end_session",
        json!({"session":"Research"}),
        b,
    ));
    assert_ne!(ended.is_error, Some(true));
    assert!(snapshots
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .is_empty());
}
