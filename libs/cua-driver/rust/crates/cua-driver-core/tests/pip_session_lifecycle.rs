//! An unrelated transport ending cannot clear another session's preview.
use async_trait::async_trait;
use cua_driver_core::{
    authorization::PermissionMode,
    pip_hook,
    protocol::ToolResult,
    session::{self, SessionEndReason},
    session_authorization::{
        EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
    },
    session_tools::EndSessionTool,
    tool::{Tool, ToolDef, ToolRegistry},
};
use serde_json::{json, Value};
use std::sync::{mpsc, Arc};
use std::time::Duration;

struct Action {
    def: ToolDef,
    owners: mpsc::Sender<(String, String)>,
}

#[async_trait]
impl Tool for Action {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let owner = args["_session_id"].as_str().unwrap();
        // Direct registry calls have no transport envelope. Like the real
        // session tools, use the private session itself as their owner.
        let transport = args["_transport_session_id"].as_str().unwrap_or(owner);
        self.owners
            .send((owner.to_owned(), transport.to_owned()))
            .unwrap();
        ToolResult::text("done")
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
fn preview_survives_unrelated_session_end_and_clears_on_owner_transport_end() {
    let context = context();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (published, updates) = mpsc::channel();
    pip_hook::set_pip_observer_fn(move |update| published.send(update).is_ok());
    let (owner_sender, owners) = mpsc::channel();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EndSessionTool));
    for name in ["click", "set_agent_cursor_motion"] {
        registry.register(Box::new(Action {
            def: ToolDef {
                name: name.into(),
                description: String::new(),
                input_schema: json!({"type":"object"}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            owners: owner_sender.clone(),
        }));
    }
    let clicked = runtime.block_on(registry.invoke_with_context(
        "click",
        json!({"session":"active","pid":42,"window_id":7,"x":10,"y":20}),
        context.clone(),
    ));
    assert_ne!(clicked.is_error, Some(true));
    let (owner, transport) = owners.recv_timeout(Duration::from_secs(2)).unwrap();
    let selected = updates.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!((selected.pid, selected.window_id), (Some(42), Some(7)));

    let ended = runtime.block_on(registry.invoke_with_context(
        "end_session",
        json!({"session":"unrelated-metadata-client"}),
        context.clone(),
    ));
    assert_ne!(ended.is_error, Some(true));
    assert!(
        matches!(
            updates.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "unrelated session changed the preview"
    );

    // Cursor configuration is not a new target, even in the owning session.
    let configured = runtime.block_on(registry.invoke_with_context(
        "set_agent_cursor_motion",
        json!({"session":"active"}),
        context.clone(),
    ));
    assert_ne!(configured.is_error, Some(true));
    assert_eq!(
        owners.recv_timeout(Duration::from_secs(2)).unwrap().0,
        owner
    );
    assert!(
        matches!(
            updates.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "configuration changed the preview"
    );

    // Exercise real owner teardown, not a replacement end-session mock. EOF
    // reaches this common path with the private owner supplied by the registry.
    assert_eq!(
        session::end_sessions_for_owner(&transport, SessionEndReason::ProcessExit),
        1
    );
    let cleared = updates.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!((cleared.pid, cleared.window_id), (None, None));
    let late = runtime.block_on(registry.invoke_with_context(
        "click",
        json!({"session":"active","pid":42,"window_id":7,"x":10,"y":20}),
        context.clone(),
    ));
    assert_eq!(late.is_error, Some(true));
    assert!(owners.try_recv().is_err(), "late action reached input");

    // Public labels can collide across independent clients. Ending the old
    // client's session must not erase the newer client's displayed target.
    let other = self::context();
    for (client, window) in [(context.clone(), 8), (other.clone(), 9)] {
        let clicked = runtime.block_on(registry.invoke_with_context(
            "click",
            json!({"session":"same-label","pid":42,"window_id":window,"x":10,"y":20}),
            client,
        ));
        assert_ne!(clicked.is_error, Some(true));
        assert_eq!(
            updates
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .window_id,
            Some(window)
        );
    }
    let first_owner = owners.recv_timeout(Duration::from_secs(2)).unwrap();
    let second_owner = owners.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_ne!(first_owner.0, second_owner.0);
    let ended = runtime.block_on(registry.invoke_with_context(
        "end_session",
        json!({"session":"same-label"}),
        context,
    ));
    assert_ne!(ended.is_error, Some(true));
    assert!(session::is_session_ended(&first_owner.0));
    assert!(
        matches!(
            updates.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "same public label erased a different client's preview"
    );

    let ended = runtime.block_on(registry.invoke_with_context(
        "end_session",
        json!({"session":"same-label"}),
        other,
    ));
    assert_ne!(ended.is_error, Some(true));
    assert!(session::is_session_ended(&second_owner.0));
    let cleared = updates.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!((cleared.pid, cleared.window_id), (None, None));
}
