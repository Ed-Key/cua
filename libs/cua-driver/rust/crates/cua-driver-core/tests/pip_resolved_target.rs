//! Real registry and token resolver, with only native input replaced.
use async_trait::async_trait;
use cua_driver_core::{
    element_token, pip_hook,
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::ArgsExt,
};
use serde_json::{json, Value};
use std::{sync::mpsc, time::Duration};
struct NativeInput(ToolDef);
#[async_trait]
impl Tool for NativeInput {
    fn def(&self) -> &ToolDef {
        &self.0
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if self.0.name == "get_window_state" {
            let snapshot = element_token::global().register_snapshot(42, 77, 2);
            return ToolResult::text(element_token::format_token(snapshot, 1));
        }
        if let Err(e) = element_token::resolve_element_args_wide(
            42,
            None,
            args.get("element_token").and_then(Value::as_str),
            None,
            None,
            "click",
        ) {
            return e;
        }
        if args.get("element_token").is_none() {
            return ToolResult::text("raw landed");
        }
        let key = args.opt_str("_session_id").unwrap();
        let mut mailbox = cursor_overlay::VisualMailbox::default();
        let id = mailbox.begin_action(&key).unwrap();
        for phase in [
            cursor_overlay::VisualPhase::Intent,
            cursor_overlay::VisualPhase::Contact,
            cursor_overlay::VisualPhase::Tracking,
            cursor_overlay::VisualPhase::End,
        ] {
            let event = cursor_overlay::VisualEvent {
                id,
                phase,
                timestamp: std::time::Instant::now(),
                target: Some((120.0, 140.0)),
                window: Some(77),
                bounds: None,
                action: cursor_overlay::CursorAction::Click,
                scroll_direction: None,
                modifiers: None,
            };
            assert!(mailbox.publish(&key, event.clone()));
            pip_hook::publish_visual(
                &key,
                cursor_overlay::visual_events::PublishedVisualEvent {
                    order: mailbox.current_order(),
                    event,
                },
            );
        }
        assert!(
            mailbox.take_pending()[&key].contact.is_some(),
            "preview stole desktop contact"
        );
        ToolResult::text("landed")
    }
}
#[test]
fn token_resolution_and_visuals_survive_a_blocked_observer_and_end() {
    let (tx, rx) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    pip_hook::set_pip_session_observer_fn(move |s| {
        tx.send(s).unwrap();
        if first {
            first = false;
            blocked.recv().unwrap();
        }
        true
    });
    let mut registry = ToolRegistry::new();
    for name in ["get_window_state", "click", "type_text"] {
        registry.register(Box::new(NativeInput(ToolDef {
            name: name.into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            read_only: name == "get_window_state",
            destructive: false,
            idempotent: false,
            open_world: false,
        })));
    }
    registry.register(Box::new(cua_driver_core::session_tools::EndSessionTool));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let token = runtime.block_on(registry.invoke(
        "get_window_state",
        json!({"pid":42,"window_id":77,"session":"token-owner"}),
    ));
    let token = match &token.content[0] {
        cua_driver_core::protocol::Content::Text { text, .. } => text.clone(),
        _ => panic!("token"),
    };
    let invalid = runtime.block_on(registry.invoke(
        "click",
        json!({"pid":42,"element_token":"invalid","session":"token-owner"}),
    ));
    assert_eq!(invalid.is_error, Some(true));
    let unavailable = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(unavailable.values().all(|r| r.window_id.is_none()));
    let result = runtime.block_on(registry.invoke(
        "click",
        json!({"pid":42,"element_token":token,"session":"token-owner"}),
    ));
    assert_ne!(result.is_error, Some(true), "{result:?}");
    release.send(()).unwrap();
    let mut snapshot = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    while !snapshot.values().any(|r| r.visual.end.is_some()) {
        snapshot = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    let (key, request) = snapshot.iter().next().unwrap();
    assert_eq!((request.pid, request.window_id), (Some(42), Some(77)));
    let phases: Vec<_> = request
        .visual
        .ordered()
        .iter()
        .map(|p| p.event.phase)
        .collect();
    assert_eq!(
        phases,
        vec![
            cursor_overlay::VisualPhase::Contact,
            cursor_overlay::VisualPhase::Tracking,
            cursor_overlay::VisualPhase::End
        ]
    );
    let late = request.visual.end.clone().unwrap();
    let key = key.clone();
    for _ in 0..2 {
        let result = runtime.block_on(registry.invoke(
            "click",
            json!({"pid":42,"element_token":token,"session":"token-owner"}),
        ));
        assert_ne!(result.is_error, Some(true));
        let next = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(
            next[&key].window_id,
            Some(77),
            "same token target was cleared and capture would restart"
        );
    }
    while rx.try_recv().is_ok() {}
    for (pid, window) in [(43, 88), (44, 99)] {
        let result = runtime.block_on(registry.invoke(
            "click",
            json!({"pid":pid,"window_id":window,"session":"token-owner"}),
        ));
        assert_ne!(result.is_error, Some(true));
        let mut next = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        while next[&key].window_id != Some(window) {
            next = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        assert_eq!(next[&key].pid, Some(pid));
        assert!(
            next[&key].visual.ordered().is_empty(),
            "old cursor survived target change"
        );
        while rx.try_recv().is_ok() {}
    }
    let typed = runtime.block_on(registry.invoke(
        "type_text",
        json!({"pid":44,"window_id":99,"text":"private typed payload","session":"token-owner"}),
    ));
    assert_ne!(typed.is_error, Some(true));
    loop {
        let next = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        if next[&key].action_label == "type_text" {
            break;
        }
        assert!(!next[&key].action_label.contains("private typed payload"));
    }
    runtime.block_on(registry.invoke("end_session", json!({"session":"token-owner"})));
    while !rx.recv_timeout(Duration::from_secs(3)).unwrap().is_empty() {}
    pip_hook::publish_visual(&key, late);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
}
