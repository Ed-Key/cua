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
    for name in ["click", "set_value", "scroll", "get_window_state"] {
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
    "observe":{"query":"AXWindow","include_screenshot":true}})
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
    assert_eq!(calls[1].1["query"], "AXWindow");
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
async fn action_read_error_evidence_reaches_clients_that_only_read_text() {
    let _serial = SERIAL.lock().await;
    for (observation_error, action_error) in [(false, true), (true, false), (true, true)] {
        let (registry, log) = registry_with_action(observation_error, action_error);
        let result = registry
            .invoke_with_context("act_and_read", input(), context())
            .await;
        assert_eq!(result.is_error, Some(true));
        let text_evidence: Vec<Value> = result
            .content
            .iter()
            .filter_map(|part| match part {
                Content::Text { text, .. } => serde_json::from_str(text).ok(),
                _ => None,
            })
            .collect();
        assert_eq!(
            text_evidence.len(),
            1,
            "missing complete JSON text evidence"
        );
        let output = &text_evidence[0];
        assert_eq!(output["action"]["isError"] == true, action_error);
        assert_eq!(output["observation"]["isError"] == true, observation_error);
        if action_error {
            assert_eq!(output["action"]["structuredContent"]["detail"], "retained");
        }
        if !observation_error {
            assert_eq!(
                output["observation"]["structuredContent"]["snapshot_id"],
                "fresh"
            );
            assert_eq!(
                output["observation"]["structuredContent"]["platform_extension"]["keep"],
                true
            );
        }
        assert_eq!(Some(output), result.structured_content.as_ref());
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "fallback must not replay input or recapture"
        );
    }
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
    for (allowed, expected, args) in [
        ("act_and_read, click", vec!["click"], input()),
        (
            "act_and_read, get_window_state",
            vec!["get_window_state"],
            input(),
        ),
        ("act_and_read, scroll", vec!["scroll"], scroll_input()),
        (
            "act_and_read, get_window_state",
            vec!["get_window_state"],
            scroll_input(),
        ),
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
            .invoke_with_context("act_and_read", args, ctx)
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

fn scroll_input() -> Value {
    json!({"pid":42,"window_id":7,"action":"scroll","element_token":"fresh-token",
        "direction":"up","by":"page","amount":4})
}

#[tokio::test]
async fn action_read_scroll_preserves_child_results_and_never_replays() {
    let _serial = SERIAL.lock().await;
    for (action_error, observation_error) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let (registry, log) = registry_with_action(observation_error, action_error);
        let result = registry
            .invoke_with_context("act_and_read", scroll_input(), context())
            .await;
        let calls = log.lock().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            ["scroll", "get_window_state"]
        );
        assert_eq!(calls[0].1["direction"], "up");
        assert_eq!(calls[0].1["by"], "page");
        assert_eq!(calls[0].1["amount"], 4);
        assert_eq!(calls[0].1["delivery_mode"], "background");
        assert_eq!(calls[1].1["pid"], 42);
        assert_eq!(calls[1].1["window_id"], 7);
        assert_eq!(calls[1].1["include_screenshot"], false);
        assert_eq!(
            result.is_error == Some(true),
            action_error || observation_error
        );
        let output = result.structured_content.unwrap();
        if !action_error {
            assert_eq!(
                output["action"]["structuredContent"]["effect"],
                "unverifiable"
            );
        }
        if !observation_error {
            assert_eq!(
                output["observation"]["structuredContent"]["snapshot_id"],
                "fresh"
            );
        }
        assert_eq!(output["action"]["isError"] == true, action_error);
        assert_eq!(output["observation"]["isError"] == true, observation_error);
    }
}

#[tokio::test]
async fn action_read_scroll_omits_default_options_and_keeps_session_context() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry_with_action(false, false);
    let ctx = context();
    let result = registry
        .invoke_with_context(
            "act_and_read",
            json!({
        "pid":42,"window_id":7,"action":"scroll","element_token":"fresh-token",
        "direction":"left","session":"scroll-session"}),
            ctx.clone(),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let calls = log.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].1.get("by").is_none());
    assert!(calls[0].1.get("amount").is_none());
    assert_eq!(calls[0].1["direction"], "left");
    for (_, args) in calls.iter() {
        assert_eq!(args["session"], ctx.runtime_session_key("scroll-session"));
    }
}

#[tokio::test]
async fn action_read_invalid_scroll_options_reject_before_either_child() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry_with_action(false, false);
    for patch in [
        json!({"direction":null}),
        json!({"direction":"diagonal"}),
        json!({"by":null}),
        json!({"by":"pixel"}),
        json!({"amount":null}),
        json!({"amount":0}),
        json!({"amount":51}),
        json!({"amount":1.5}),
        json!({"action":"click"}),
        json!({"action":"set_value","value":"text"}),
        json!({"value":"not-a-scroll-option"}),
    ] {
        let mut args = scroll_input();
        args.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let result = registry
            .invoke_with_context("act_and_read", args, context())
            .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
    }
    let mut missing = scroll_input();
    missing.as_object_mut().unwrap().remove("direction");
    assert_eq!(
        registry
            .invoke_with_context("act_and_read", missing, context())
            .await
            .is_error,
        Some(true)
    );
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn action_read_scroll_schema_accepts_bounded_options_and_rejects_nulls() {
    use cua_driver_contract::{ActAndReadInput, ToolInput};
    let schema = ActAndReadInput::input_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for direction in ["up", "down", "left", "right"] {
        for amount in [1, 50] {
            let mut value = scroll_input();
            value["direction"] = json!(direction);
            value["amount"] = json!(amount);
            assert!(validator.is_valid(&value));
            serde_json::from_value::<ActAndReadInput>(value)
                .unwrap()
                .validate()
                .unwrap();
        }
    }
    for patch in [
        json!({"direction":null}),
        json!({"by":null}),
        json!({"amount":null}),
        json!({"amount":0}),
        json!({"amount":51}),
        json!({"direction":"diagonal"}),
        json!({"by":"pixel"}),
    ] {
        let mut value = scroll_input();
        value
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(!validator.is_valid(&value), "{value}");
    }
}

const STEP_TOOLS: [&str; 6] = [
    "click",
    "set_value",
    "scroll",
    "type_text",
    "press_key",
    "hotkey",
];

/// Registry with the given child tools; only `failing` returns a tool error.
fn steps_registry(tools: &[&str], failing: Option<&str>) -> (Arc<ToolRegistry>, Log) {
    let mut registry = ToolRegistry::new();
    let log = Arc::new(Mutex::new(vec![]));
    for &name in tools.iter().chain(&["get_window_state"]) {
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
            observation_error: false,
            action_error: failing == Some(name),
        }));
    }
    registry.register_sequence_tools();
    registry.register_session_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    (registry, log)
}

fn all_steps() -> Value {
    json!([
        {"action":"click","element_token":"t-click"},
        {"action":"set_value","element_token":"t-field","value":"hello"},
        {"action":"type_text","text":" world"},
        {"action":"press_key","key":"return","modifiers":["shift"]},
        {"action":"hotkey","keys":["cmd","a"]},
        {"action":"scroll","element_token":"t-list","direction":"down","by":"page","amount":2}
    ])
}

#[test]
fn action_read_validates_exactly_one_form_and_each_step() {
    use cua_driver_contract::{ActAndReadInput, ToolInput};
    let check = |patch: Value| -> Result<(), String> {
        let mut args = json!({"pid":42,"window_id":7});
        args.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        serde_json::from_value::<ActAndReadInput>(args)
            .map_err(|e| format!("parse: {e}"))?
            .validate()
    };
    check(json!({"steps":all_steps()})).unwrap();
    check(json!({"action":"click","element_token":"t"})).unwrap();
    for (steps, message) in [
        (
            json!([{"action":"type_text","text":"x","element_token":"t"}]),
            None,
        ),
        (
            json!([{"action":"click"}]),
            Some("step 1: a nonblank element_token is required for click"),
        ),
        (
            json!([{"action":"set_value","element_token":" ","value":"v"}]),
            Some("step 1: a nonblank element_token is required for set_value"),
        ),
        (
            json!([{"action":"scroll","direction":"up"}]),
            Some("step 1: a nonblank element_token is required for scroll"),
        ),
        (
            json!([{"action":"click","element_token":"t"},{"action":"set_value","element_token":"t"}]),
            Some("step 2: value is required for set_value"),
        ),
        (
            json!([{"action":"scroll","element_token":"t"}]),
            Some("step 1: direction is required for scroll"),
        ),
        (
            json!([{"action":"scroll","element_token":"t","direction":"up","amount":51}]),
            Some("step 1: scroll amount must be between 1 and 50"),
        ),
        (
            json!([{"action":"click","element_token":"t","amount":2}]),
            Some("step 1: amount is only allowed for scroll"),
        ),
        (
            json!([{"action":"click","element_token":"t","value":"v"}]),
            Some("step 1: value is only allowed for set_value"),
        ),
        (
            json!([{"action":"type_text"}]),
            Some("step 1: text is required for type_text"),
        ),
        (
            json!([{"action":"type_text","text":"x","element_token":" "}]),
            Some("step 1: element_token must be nonblank when given"),
        ),
        (
            json!([{"action":"type_text","text":"x","key":"a"}]),
            Some("step 1: key is only allowed for press_key"),
        ),
        (
            json!([{"action":"press_key"}]),
            Some("step 1: key is required for press_key"),
        ),
        (
            json!([{"action":"press_key","key":" "}]),
            Some("step 1: key must be nonblank"),
        ),
        (
            json!([{"action":"press_key","key":"a","element_token":"t"}]),
            Some("step 1: element_token is not allowed for press_key"),
        ),
        (
            json!([{"action":"press_key","key":"a","keys":["cmd","a"]}]),
            Some("step 1: keys is only allowed for hotkey"),
        ),
        (
            json!([{"action":"hotkey"}]),
            Some("step 1: keys is required for hotkey"),
        ),
        (
            json!([{"action":"hotkey","keys":[]}]),
            Some("step 1: keys must be a non-empty array of nonblank strings"),
        ),
        (
            json!([{"action":"hotkey","keys":["cmd","a"],"element_token":"t"}]),
            Some("step 1: element_token is not allowed for hotkey"),
        ),
        (
            json!([{"action":"hotkey","keys":["cmd","a"],"modifiers":["cmd"]}]),
            Some("step 1: modifiers is only allowed for press_key"),
        ),
        (
            json!([]),
            Some("steps must contain between 1 and 8 actions"),
        ),
        (
            json!(vec![json!({"action":"press_key","key":"a"}); 9]),
            Some("steps must contain between 1 and 8 actions"),
        ),
    ] {
        let result = check(json!({"steps":steps}));
        match message {
            None => result.unwrap(),
            Some(message) => assert_eq!(result.unwrap_err(), message, "{steps}"),
        }
    }
    check(json!({"steps":vec![json!({"action":"press_key","key":"a"}); 8]})).unwrap();
    for mixed in [
        json!({"action":"click"}),
        json!({"element_token":"t"}),
        json!({"value":"v"}),
        json!({"direction":"up"}),
        json!({"by":"line"}),
        json!({"amount":1}),
    ] {
        let mut patch = json!({"steps":all_steps()});
        patch
            .as_object_mut()
            .unwrap()
            .extend(mixed.as_object().unwrap().clone());
        assert!(check(patch).unwrap_err().starts_with("pass either steps"));
    }
    assert_eq!(
        check(json!({})).unwrap_err(),
        "action and element_token are required unless steps is given"
    );
    assert_eq!(
        check(json!({"action":"click"})).unwrap_err(),
        "a nonblank element_token is required"
    );
    for bad in [
        json!({"steps":null}),
        json!({"steps":[{"action":"drag","element_token":"t"}]}),
        json!({"steps":[{"action":"click","element_token":"t","delivery_mode":"foreground"}]}),
        json!({"steps":[{"action":"press_key","key":null}]}),
    ] {
        assert!(
            check(bad.clone()).unwrap_err().starts_with("parse:"),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn action_read_steps_run_in_order_then_observe_once() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = steps_registry(&STEP_TOOLS, None);
    let ctx = context();
    let result = registry
        .invoke_with_context(
            "act_and_read",
            json!({"pid":42,"window_id":7,"session":"steps","steps":all_steps(),
                "observe":{"include_screenshot":true}}),
            ctx.clone(),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let calls = log.lock().unwrap();
    assert_eq!(
        calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        [
            "click",
            "set_value",
            "type_text",
            "press_key",
            "hotkey",
            "scroll",
            "get_window_state"
        ]
    );
    for (name, args) in calls.iter() {
        assert_eq!(args["pid"], 42, "{name}");
        assert_eq!(args["window_id"], 7, "{name}");
        assert_eq!(args["session"], ctx.runtime_session_key("steps"), "{name}");
        assert!(args.get("action").is_none(), "{name}");
        let background = !matches!(name.as_str(), "set_value" | "get_window_state");
        assert_eq!(
            args.get("delivery_mode") == Some(&json!("background")),
            background,
            "{name}"
        );
    }
    assert_eq!(calls[0].1["element_token"], "t-click");
    assert_eq!(calls[1].1["value"], "hello");
    assert_eq!(calls[2].1["text"], " world");
    assert!(calls[2].1.get("element_token").is_none());
    assert_eq!(calls[3].1["key"], "return");
    assert_eq!(calls[3].1["modifiers"], json!(["shift"]));
    assert_eq!(calls[4].1["keys"], json!(["cmd", "a"]));
    assert_eq!(calls[5].1["direction"], "down");
    assert_eq!(calls[5].1["by"], "page");
    assert_eq!(calls[5].1["amount"], 2);
    assert_eq!(calls[6].1["include_screenshot"], true);

    let output = result.structured_content.clone().unwrap();
    assert_eq!(output["steps"].as_array().unwrap().len(), 6);
    assert!(output.get("stopped_at").is_none());
    assert_eq!(output["action"], output["steps"][5]);
    assert_eq!(
        output["observation"]["structuredContent"]["snapshot_id"],
        "fresh"
    );
    let schema = cua_driver_contract::tool_success_output_schema("act_and_read").unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&output),
        "{output}"
    );
    let headers: Vec<&str> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text, .. }
                if text.ends_with("RESULT (unchanged child content follows)") =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(headers.len(), 7);
    assert!(headers[0].starts_with("STEP 1 RESULT"));
    assert!(headers[5].starts_with("STEP 6 RESULT"));
    assert!(headers[6].starts_with("OBSERVATION RESULT"));
    assert!(!headers.iter().any(|h| h.starts_with("ACTION RESULT")));
}

#[tokio::test]
async fn action_read_steps_stop_at_first_error_and_still_observe() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = steps_registry(&STEP_TOOLS, Some("set_value"));
    let result = registry
        .invoke_with_context(
            "act_and_read",
            json!({"pid":42,"window_id":7,"steps":all_steps()}),
            context(),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .map(|c| c.0.as_str())
            .collect::<Vec<_>>(),
        ["click", "set_value", "get_window_state"]
    );
    let output = result.structured_content.clone().unwrap();
    assert_eq!(output["stopped_at"], 2);
    let steps = output["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_ne!(steps[0]["isError"], true);
    assert_eq!(steps[1]["isError"], true);
    assert_eq!(steps[1]["structuredContent"]["detail"], "retained");
    assert_eq!(output["action"], steps[1]);
    assert_eq!(
        output["observation"]["structuredContent"]["snapshot_id"],
        "fresh"
    );
    // Text-only clients get the same evidence, including where it stopped.
    let text_evidence: Vec<Value> = result
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => serde_json::from_str(text).ok(),
            _ => None,
        })
        .collect();
    assert_eq!(text_evidence, [output]);
}

#[tokio::test]
async fn action_read_steps_require_every_child_tool_before_dispatch() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = steps_registry(&["click", "set_value", "scroll"], None);
    let result = registry
        .invoke_with_context(
            "act_and_read",
            json!({"pid":42,"window_id":7,"steps":all_steps()}),
            context(),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(result.content.iter().any(
        |c| matches!(c,Content::Text{text,..} if text.contains("requires registered type_text"))
    ));
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn action_read_single_form_output_has_no_step_fields() {
    let _serial = SERIAL.lock().await;
    for action_error in [false, true] {
        let (registry, _) = registry_with_action(false, action_error);
        let result = registry
            .invoke_with_context("act_and_read", input(), context())
            .await;
        let output = result.structured_content.unwrap();
        assert!(output.get("steps").is_none());
        assert!(output.get("stopped_at").is_none());
        assert!(matches!(&result.content[0],
            Content::Text{text,..} if text == "ACTION RESULT (unchanged child content follows)"));
    }
}
