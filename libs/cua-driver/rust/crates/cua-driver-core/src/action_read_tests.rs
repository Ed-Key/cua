//! Exercise composition through the real registry; only native app I/O is fake.
use crate::{
    authorization::PermissionMode,
    protocol::{Content, ToolResult},
    session_authorization::{
        EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
    },
    tool::{Tool, ToolDef, ToolRegistry},
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

type Log = Arc<Mutex<Vec<(String, Value)>>>;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

struct NativeIo {
    def: ToolDef,
    log: Log,
    observation_error: bool,
    action_error: bool,
}
#[async_trait]
impl Tool for NativeIo {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let screenshot = args["include_screenshot"] == true;
        self.log.lock().unwrap().push((self.def.name.clone(), args));
        if self.def.name != "get_window_state" {
            // Input may have landed despite the tool error. A fresh read must
            // remain available, and the error must not become success.
            if self.action_error {
                return ToolResult::error("delivery unknown")
                    .with_structured(json!({"detail":"retained"}));
            }
            use crate::action_record::{
                ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery,
                RequestedDelivery,
            };
            return ToolResult::text("input dispatched").with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::MacosAxAction,
                    RequestedDelivery::Background,
                )
                .actual_delivery(ActualDelivery::Background)
                .build()
                .unwrap(),
            );
        }
        if self.observation_error {
            return ToolResult::error("window disappeared");
        }
        let mut result = ToolResult::text("fresh window").with_structured(json!({
            "pid":42,"window_id":7,"snapshot_id":"fresh","tree_markdown":"draft changed",
            "platform_extension":{"keep":true}
        }));
        if screenshot {
            result.content.push(Content::image_png("aW1hZ2U=".into()));
        }
        result
    }
}

fn registry(observation_error: bool) -> (Arc<ToolRegistry>, Log) {
    registry_with_action(observation_error, true)
}
fn registry_with_action(observation_error: bool, action_error: bool) -> (Arc<ToolRegistry>, Log) {
    let mut registry = ToolRegistry::new();
    let log = Arc::new(Mutex::new(vec![]));
    for name in ["click", "set_value", "get_window_state"] {
        registry.register(Box::new(NativeIo {
            def: ToolDef {
                name: name.into(),
                description: "native boundary double".into(),
                input_schema: json!({"type":"object"}),
                read_only: name == "get_window_state",
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            log: log.clone(),
            observation_error,
            action_error,
        }));
    }
    registry.register_sequence_tools();
    registry.register_session_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    (registry, log)
}
fn input() -> Value {
    json!({"pid":42,"window_id":7,"action":"click","element_token":"fresh-token",
    "observe":{"query":"AXWindow","query_context":true,"include_screenshot":true}})
}

#[tokio::test]
async fn action_read_preserves_failed_action_and_fresh_observation_without_replay() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(false);
    let result = registry
        .invoke_with_context("act_and_read", input(), context())
        .await;
    let calls = log.lock().unwrap();
    assert_eq!(
        calls.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["click", "get_window_state"]
    );
    assert_eq!(calls[0].1["delivery_mode"], "background");
    assert_eq!(calls[1].1["query_context"], true);
    assert_eq!(calls[1].1["pid"], 42);
    assert_eq!(calls[1].1["window_id"], 7);
    assert_eq!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    assert_eq!(output["action"]["isError"], true);
    assert_eq!(output["action"]["structuredContent"]["detail"], "retained");
    assert_eq!(
        output["observation"]["structuredContent"]["snapshot_id"],
        "fresh"
    );
    assert_eq!(
        output["observation"]["structuredContent"]["platform_extension"]["keep"],
        true
    );
    assert!(output["action"].get("content").is_none());
    assert!(output["observation"].get("content").is_none());
    assert_eq!(
        result
            .content
            .iter()
            .filter(|c| matches!(c, Content::Image { .. }))
            .count(),
        1
    );
    assert!(result
        .content
        .iter()
        .any(|c| matches!(c,Content::Text{text,..} if text=="delivery unknown")));
}

#[tokio::test]
async fn action_read_bad_observation_options_reject_before_input() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(false);
    for observe in [
        json!({"query_context":true}),
        json!({"max_elements":0}),
        json!({"query_context":"true"}),
        json!({"include_screenshot":null}),
        json!({"pid":99}),
        json!({"screenshot_out_file":"/tmp/unwanted"}),
    ] {
        let mut args = input();
        args["observe"] = observe;
        let result = registry
            .invoke_with_context("act_and_read", args, context())
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            result
                .content
                .iter()
                .any(|c| matches!(c,Content::Text{text,..} if text.contains("invalid arguments"))),
            "{result:?}"
        );
    }
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn action_read_observation_failure_retains_both_errors() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(true);
    let result = registry
        .invoke_with_context("act_and_read", input(), context())
        .await;
    assert_eq!(log.lock().unwrap().len(), 2);
    assert_eq!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    assert_eq!(output["action"]["isError"], true);
    assert_eq!(output["observation"]["isError"], true);
    assert!(result
        .content
        .iter()
        .any(|c| matches!(c,Content::Text{text,..} if text=="window disappeared")));
}

#[tokio::test]
async fn action_read_set_value_and_default_read_preserve_unverifiable_effect() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry_with_action(false, false);
    let result = registry
        .invoke_with_context(
            "act_and_read",
            json!({"pid":42,"window_id":7,
        "action":"set_value","element_token":"fresh-token","value":"first\nsecond"}),
            context(),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let calls = log.lock().unwrap();
    assert_eq!(
        calls.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["set_value", "get_window_state"]
    );
    assert_eq!(calls[0].1["value"], "first\nsecond");
    assert!(calls[0].1.get("delivery_mode").is_none());
    assert_eq!(calls[1].1["include_screenshot"], false);
    assert!(!result
        .content
        .iter()
        .any(|c| matches!(c, Content::Image { .. })));
    let output = result.structured_content.unwrap();
    assert_eq!(
        output["action"]["structuredContent"]["effect"],
        "unverifiable"
    );
    assert!(output.get("status").is_none());
    let schema = cua_driver_contract::tool_success_output_schema("act_and_read").unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&output),
        "{output}"
    );
}

#[tokio::test]
async fn action_read_sessions_are_isolated_and_ended_sessions_cannot_dispatch() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(false);
    let contexts = [context(), context()];
    for ctx in &contexts {
        let mut args = input();
        args["session"] = json!("shared-label");
        registry
            .invoke_with_context("act_and_read", args, ctx.clone())
            .await;
    }
    {
        let calls = log.lock().unwrap();
        assert_eq!(calls.len(), 4);
        for (offset, ctx) in [(0, &contexts[0]), (2, &contexts[1])] {
            for i in [offset, offset + 1] {
                assert_eq!(
                    calls[i].1["session"],
                    ctx.runtime_session_key("shared-label")
                );
                assert_eq!(calls[i].1["_public_session_label"], "shared-label");
            }
        }
        assert_ne!(calls[0].1["session"], calls[2].1["session"]);
    }
    registry
        .invoke_with_context(
            "end_session",
            json!({"session":"shared-label"}),
            contexts[0].clone(),
        )
        .await;
    let mut args = input();
    args["session"] = json!("shared-label");
    let result = registry
        .invoke_with_context("act_and_read", args, contexts[0].clone())
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(log.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn action_read_rejects_unbounded_or_ambiguous_action_arguments() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(false);
    for patch in [
        json!({"action":"type_text"}),
        json!({"action":"set_value"}),
        json!({"value":"unused"}),
        json!({"element_token":"  "}),
        json!({"element_token":null}),
        json!({"observe":null}),
        json!({"pid":2147483648_u64}),
        json!({"window_id":4294967296_u64}),
        json!({"delivery_mode":"foreground"}),
        json!({"session":null}),
    ] {
        let mut args = input();
        args.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let result = registry
            .invoke_with_context("act_and_read", args, context())
            .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
    }
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn action_read_inherits_transport_owner_for_both_children() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(false);
    let ctx = context();
    let evidence = crate::tool::TrustedInvocationEvidence::extract_from_adapter_args(&mut json!({
        "_session_id":"wire","_transport_session_id":"connection"}));
    let mut args = input();
    args["session"] = json!("wire");
    registry
        .invoke_with_context_and_evidence("act_and_read", args, ctx.clone(), evidence)
        .await;
    let calls = log.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for (_, args) in calls.iter() {
        assert_eq!(args["_session_id"], ctx.runtime_session_key("wire"));
        assert_eq!(
            args["_transport_session_id"],
            ctx.runtime_session_key("connection")
        );
    }
}

#[test]
fn action_read_has_observation_and_input_authorization_without_a_parent_cursor_action() {
    use crate::authorization::{advertised_risk_for, enforcement_adapters_for_call, RiskClass};
    assert_eq!(advertised_risk_for("act_and_read").class, RiskClass::R2);
    assert_eq!(
        enforcement_adapters_for_call("act_and_read", &input())
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
        ["private_observation", "desktop_input"]
    );
    let contract = cua_driver_contract::tool_contract("act_and_read").unwrap();
    assert!(contract.cursor_semantics.is_none());
    assert_eq!(contract.platforms, [cua_driver_contract::Platform::Macos]);
}

#[cfg(feature = "yaml")]
#[tokio::test]
async fn action_read_child_permissions_never_fall_back_to_unrestricted() {
    use std::io::Write;
    let _serial = SERIAL.lock().await;
    for (allowed, expected) in [
        ("act_and_read, click", vec!["click"]),
        ("act_and_read, get_window_state", vec!["get_window_state"]),
    ] {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file,"version: 3\nexpires_after: 1h\nidle_timeout: 30m\nallow:\n  tools: [{allowed}]\nresources:\n  desktop:\n    windows:\n      - pid: 42\n        window_id: 7\n").unwrap();
        let manifest = Arc::new(crate::session_manifest::load_manifest(file.path()).unwrap());
        let ceiling = SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Bounded],
            false,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap();
        let ctx = SessionAuthorizationRegistry::with_ceiling(ceiling)
            .compatibility_context(PermissionMode::Bounded, Some(manifest))
            .unwrap();
        let (registry, log) = registry(false);
        let result = registry
            .invoke_with_context("act_and_read", input(), ctx)
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .map(|x| x.0.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(result
            .content
            .iter()
            .any(|c| matches!(c,Content::Text{text,..} if text.contains("capability manifest"))));
    }
}

#[tokio::test]
async fn action_read_records_only_the_child_action() {
    let _serial = SERIAL.lock().await;
    let (registry, _) = registry_with_action(false, false);
    let directory = tempfile::tempdir().unwrap();
    registry
        .recording
        .start(directory.path().to_str().unwrap(), false, None)
        .unwrap();
    let result = registry
        .invoke_with_context("act_and_read", input(), context())
        .await;
    assert_ne!(result.is_error, Some(true));
    registry.recording.stop_owner(None).unwrap();
    let turns = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("turn-")
        })
        .collect::<Vec<_>>();
    assert_eq!(turns.len(), 1);
    let action: Value =
        serde_json::from_slice(&std::fs::read(turns[0].join("action.json")).unwrap()).unwrap();
    assert_eq!(action["tool"], "click");
}
