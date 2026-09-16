//! The observer route must publish metadata without taking a driver screenshot.
use async_trait::async_trait;
use cua_driver_core::{
    authorization::PermissionMode,
    pip_hook,
    protocol::ToolResult,
    recording,
    session_authorization::{
        EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
    },
    tool::{Tool, ToolDef, ToolRegistry},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

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
fn observer_backpressure_and_exit_never_capture_or_change_action_results() {
    let captures = Arc::new(AtomicUsize::new(0));
    let capture_count = captures.clone();
    recording::set_screenshot_fn(move |_, _| {
        capture_count.fetch_add(1, Ordering::SeqCst);
        Some(vec![1])
    });
    let (published, requests) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    pip_hook::set_pip_observer_fn(move |request| {
        published.send(request).unwrap();
        blocked.recv().unwrap();
        false // The helper closed after accepting this request.
    });
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(Input(ToolDef {
        name: "click".into(),
        description: "input boundary".into(),
        input_schema: json!({"type":"object"}),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    })));
    let registry = Arc::new(registry);
    let (done, completed) = mpsc::channel();
    let actions = registry.clone();
    let action_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for window in 7..=107 {
            let result = runtime.block_on(actions.invoke_with_context(
                "click",
                json!({"pid":42,"window_id":window,"x":10,"y":20}),
                context(),
            ));
            assert_ne!(result.is_error, Some(true));
        }
        done.send(()).unwrap();
    });
    let first = requests.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(first.pid, Some(42));
    assert!(first.window_id.is_some_and(|id| (7..=107).contains(&id)));
    let early = completed.recv_timeout(Duration::from_secs(3));
    release.send(()).unwrap();
    assert!(early.is_ok(), "input waited for observer pipe backpressure");
    action_thread.join().unwrap();
    assert_eq!(
        captures.load(Ordering::SeqCst),
        0,
        "observer captured in the driver"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while pip_hook::pip_enabled() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(!pip_hook::pip_enabled(), "closed observer remained enabled");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(registry.invoke_with_context(
        "click",
        json!({"pid":42,"window_id":108,"x":10,"y":20}),
        context(),
    ));
    assert_ne!(result.is_error, Some(true));
    assert!(requests.try_recv().is_err());
    assert_eq!(captures.load(Ordering::SeqCst), 0);
}
