//! Real dispatcher and preview hooks, with only native input/capture replaced.
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
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

struct NativeInput(ToolDef);

#[async_trait]
impl Tool for NativeInput {
    fn def(&self) -> &ToolDef {
        &self.0
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        ToolResult::text("input completed")
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
fn stalled_preview_does_not_hold_actions_and_only_latest_pending_target_is_captured() {
    // A synchronous capture, an unbounded queue, or capturing a stale pending
    // target must fail this test. The timeout is a deadlock watchdog, not a
    // performance claim. This integration binary isolates process-wide hooks.
    let (capture_tx, captures) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(gate);
    recording::set_screenshot_fn(move |window, pid| {
        capture_tx.send((window, pid)).unwrap();
        if window == Some(7) {
            gate.lock().unwrap().recv().unwrap();
        }
        Some(window.unwrap().to_be_bytes().to_vec())
    });
    let (frame_tx, frames) = mpsc::channel();
    let (render_release, render_gate) = mpsc::channel();
    let render_gate = Mutex::new(render_gate);
    pip_hook::set_pip_push_fn(move |frame| {
        let stall = frame.png_bytes == 107_u64.to_be_bytes();
        frame_tx.send(frame).unwrap();
        if stall {
            render_gate.lock().unwrap().recv().unwrap();
        }
    });
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(NativeInput(ToolDef {
        name: "click".into(),
        description: "native input boundary".into(),
        input_schema: json!({"type":"object"}),
        read_only: false,
        destructive: false,
        idempotent: false,
        open_world: false,
    })));
    let registry = Arc::new(registry);
    let (returned_tx, returned) = mpsc::channel();
    let first_registry = registry.clone();
    let first = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(first_registry.invoke_with_context(
            "click",
            json!({"pid":42,"window_id":7,"x":10,"y":20}),
            context(),
        ));
        returned_tx.send(result).unwrap();
    });
    assert_eq!(
        captures.recv_timeout(Duration::from_secs(3)).unwrap(),
        (Some(7), Some(42))
    );
    let early_result = returned.recv_timeout(Duration::from_secs(2));
    if early_result.is_err() {
        release.send(()).unwrap();
        first.join().unwrap();
        panic!("action waited for stalled PiP capture");
    }
    assert_ne!(early_result.unwrap().is_error, Some(true));
    first.join().unwrap();
    let (burst_done, completed) = mpsc::channel();
    let burst_registry = registry.clone();
    let burst = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for window in 8..=107 {
            let result = runtime.block_on(burst_registry.invoke_with_context(
                "click",
                json!({"pid":42,"window_id":window,"x":10,"y":20}),
                context(),
            ));
            assert_ne!(result.is_error, Some(true));
        }
        burst_done.send(()).unwrap();
    });
    if completed.recv_timeout(Duration::from_secs(3)).is_err() {
        let _ = release.send(());
        let _ = render_release.send(());
        panic!("action burst waited for preview capacity");
    }
    burst.join().unwrap();
    assert!(
        captures.try_recv().is_err(),
        "concurrent preview captures escaped the bound"
    );
    release.send(()).unwrap();
    let frame = frames.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(frame.png_bytes, 107_u64.to_be_bytes());
    assert_eq!(
        captures.try_iter().collect::<Vec<_>>(),
        vec![(Some(107), Some(42))]
    );
    assert!(
        frames.try_recv().is_err(),
        "superseded target was presented"
    );

    // The renderer is now stalled after receiving frame107. This must not
    // hold either an action result or the publication lock for later actions.
    let (next_done, next_completed) = mpsc::channel();
    let next = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(registry.invoke_with_context(
            "click",
            json!({"pid":42,"window_id":108,"x":10,"y":20}),
            context(),
        ));
        next_done.send(result).unwrap();
    });
    let result = next_completed.recv_timeout(Duration::from_secs(2));
    if result.is_err() {
        let _ = render_release.send(());
        panic!("action waited for stalled PiP renderer");
    }
    assert_ne!(result.unwrap().is_error, Some(true));
    next.join().unwrap();
    assert!(captures.try_recv().is_err());
    render_release.send(()).unwrap();
    let next_frame = frames.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(next_frame.png_bytes, 108_u64.to_be_bytes());
    assert_eq!(captures.try_recv().unwrap(), (Some(108), Some(42)));
}
