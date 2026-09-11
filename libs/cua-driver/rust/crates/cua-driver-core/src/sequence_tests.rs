// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use crate::{
    action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    },
    authorization::PermissionMode,
    expectation::{ObservationProvider, ObservationSample, ObservationSnapshot, VerifyStateTool},
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

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type Log = Arc<Mutex<Vec<(String, Value)>>>;

fn context() -> Arc<EffectiveAuthorizationContext> {
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap()
}

struct Action {
    def: ToolDef,
    log: Log,
    response: &'static str,
}
#[async_trait]
impl Tool for Action {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        self.log.lock().unwrap().push((self.def.name.clone(), args));
        match self.response {
            "error" => return ToolResult::error("é".repeat(2000)),
            "unstructured" => return ToolResult::text("missing action record"),
            "malformed" => {
                return ToolResult::text("bad action")
                    .with_structured(json!({"effect":"confirmed"}))
            }
            _ => {}
        }
        let effect = match self.response {
            "refused" => ActionEffect::Refused,
            "partial" => ActionEffect::Partial,
            _ => ActionEffect::Unverifiable,
        };
        let mut record = ActionExecutionRecord::builder(
            effect,
            ActionTransport::MacosAxAction,
            RequestedDelivery::Background,
        );
        if effect != ActionEffect::Refused {
            record = record.actual_delivery(ActualDelivery::Background);
        }
        if effect == ActionEffect::Partial {
            record = record.delivered_count(1);
        }
        let mut result = ToolResult::text("action").with_action_record(record.build().unwrap());
        result.content.push(Content::image_png("aW1hZ2U=".into()));
        result
    }
}
struct Provider {
    log: Log,
    snapshot: ObservationSnapshot,
}
#[async_trait]
impl ObservationProvider for Provider {
    async fn observe(
        &self,
        pid: i64,
        window_id: u64,
        _: bool,
        screenshot: bool,
    ) -> Result<ObservationSample, String> {
        assert!(!screenshot);
        self.log
            .lock()
            .unwrap()
            .push(("observe".into(), json!({"pid":pid,"window_id":window_id})));
        Ok(ObservationSample {
            snapshot: self.snapshot.clone(),
            visual_evidence: vec![],
        })
    }
}
fn snapshot(exists: bool) -> ObservationSnapshot {
    ObservationSnapshot {
        window: exists.then(|| json!({"pid":42,"window_id":7})),
        ..Default::default()
    }
}
fn registry(response: &'static str, snapshot: ObservationSnapshot) -> (Arc<ToolRegistry>, Log) {
    let log = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    for name in ["click", "type_text"] {
        registry.register(Box::new(Action {
            def: ToolDef {
                name: name.into(),
                description: "deterministic action".into(),
                input_schema: json!({"type":"object"}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            log: log.clone(),
            response,
        }));
    }
    registry.register(Box::new(VerifyStateTool::new(Arc::new(Provider {
        log: log.clone(),
        snapshot,
    }))));
    registry.register_sequence_tools();
    registry.register_session_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    (registry, log)
}
fn input() -> Value {
    json!({"pid":42,"window_id":7,"steps":[
        {"tool":"click","arguments":{"x":10,"y":20},"expect":[{"window":{"exists":true}}],"timeout_ms":0},
        {"tool":"type_text","arguments":{"text":"query"},"expect":[{"window":{"exists":true}}],"timeout_ms":0}
    ]})
}
async fn run(registry: &ToolRegistry, input: Value) -> Value {
    let result = registry
        .invoke_with_context("run_sequence", input, context())
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(result
        .content
        .iter()
        .all(|item| !matches!(item, Content::Image { .. })));
    let output = result.structured_content.unwrap();
    let schema = cua_driver_contract::tool_success_output_schema("run_sequence").unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&output),
        "sequence output violates advertised schema: {output}"
    );
    output
}
#[tokio::test]
async fn sequence_completed_orders_action_and_real_verifier() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let output = run(&registry, input()).await;
    assert_eq!(output["status"], "completed");
    assert_eq!(output["stopped_at"], Value::Null);
    assert_eq!(output["stop_reason"], Value::Null);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .map(|item| item.0.as_str())
            .collect::<Vec<_>>(),
        ["click", "observe", "type_text", "observe"]
    );
    for step in output["steps"].as_array().unwrap() {
        assert_eq!(step["action"]["effect"], "unverifiable");
        assert_eq!(step["verification"]["stable"], true);
        assert_eq!(step["observation_count"], 1);
        assert_eq!(step["image_bytes_returned"], 0);
    }
}
#[tokio::test]
async fn sequence_unsatisfied_omits_unattempted_steps() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(false));
    let output = run(&registry, input()).await;
    assert_eq!(output["status"], "stopped");
    assert_eq!(output["stopped_at"], 0);
    assert_eq!(output["stop_reason"], "unsatisfied");
    assert_eq!(output["steps"].as_array().unwrap().len(), 1);
    assert_eq!(log.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn sequence_untrusted_web_predicate_stops() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry(
        "ok",
        ObservationSnapshot {
            window: Some(json!({"pid":42,"window_id":7})),
            elements: Some(vec![
                json!({"element_index":1,"role":"AXWebArea"}),
                json!({"element_index":2,"parent_index":1,"role":"textbox","label":"Search","value":"query"}),
            ]),
            element_source_trusted: true,
            elements_complete: true,
        },
    );
    let mut args = input();
    args["steps"][0]["expect"] =
        json!([{"element":{"selector":{"role":"textbox"},"value_equals":"query"}}]);
    let output = run(&registry, args).await;
    assert_eq!(output["stop_reason"], "unknown");
    assert_eq!(
        output["steps"][0]["verification"]["predicates"][0]["unknown_reason"],
        "untrusted_source"
    );
    assert_eq!(log.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn sequence_action_failure_stops_before_observation() {
    let _serial = SERIAL.lock().await;
    for response in ["error", "refused", "partial"] {
        let (registry, log) = registry(response, snapshot(true));
        let output = run(&registry, input()).await;
        assert_eq!(
            output["stop_reason"], "action_error",
            "{response}: {output}"
        );
        assert_eq!(output["steps"].as_array().unwrap().len(), 1);
        assert!(output["steps"][0]["verification"].is_null());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(
            output["steps"][0]["error"]["message"]
                .as_str()
                .unwrap()
                .len()
                <= 2000
        );
    }
}
#[tokio::test]
async fn sequence_validates_all_steps_before_any_child() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let mut cases = vec![];
    for bad in [
        json!({"text":"bad","session":"escape"}),
        json!({"text":"bad","_session_id":"escape"}),
        json!({"x":1,"y":2}),
        json!({"text":"bad","delivery_mode":"foreground"}),
    ] {
        let mut args = input();
        args["steps"][1]["arguments"] = bad;
        cases.push(args);
    }
    for bad in [
        json!([]),
        json!([{"element":{"selector":{"role":" "}}}]),
        json!([{"element":{"selector":{"role":"button"},"exists":false}}]),
        json!([{"window":{"bogus":true}}]),
    ] {
        let mut args = input();
        args["steps"][1]["expect"] = bad;
        cases.push(args);
    }
    for count in [0, 9] {
        let mut args = input();
        args["steps"] = Value::Array(vec![args["steps"][0].clone(); count]);
        cases.push(args);
    }
    for (field, value) in [
        ("timeout_ms", 10001),
        ("stable_samples", 0),
        ("stable_samples", 2),
        ("stable_samples", 6),
    ] {
        let mut args = input();
        args["steps"][1][field] = json!(value);
        cases.push(args);
    }
    for args in cases {
        let result = registry
            .invoke_with_context("run_sequence", args.clone(), context())
            .await;
        assert_eq!(result.is_error, Some(true), "accepted {args}");
    }
    assert!(log.lock().unwrap().is_empty());
}
#[tokio::test]
async fn sequence_preserves_verifier_unknown_shapes_and_stability() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let mut args = input();
    args["steps"][0]["expect"] = json!([{}]);
    let output = run(&registry, args).await;
    assert_eq!(output["stop_reason"], "unknown");
    assert_eq!(
        output["steps"][0]["verification"]["predicates"][0]["unknown_reason"],
        "invalid_predicate"
    );
    assert_eq!(log.lock().unwrap().len(), 2);
    let mut args = input();
    args["steps"][0]["timeout_ms"] = json!(1);
    args["steps"][0]["stable_samples"] = json!(5);
    let output = run(&registry, args).await;
    assert_eq!(output["stop_reason"], "unknown");
    assert_eq!(output["steps"][0]["verification"]["stable"], false);
}

#[cfg(feature = "yaml")]
fn limited_context(tools: &str) -> Arc<EffectiveAuthorizationContext> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "version: 3\nexpires_after: 1h\nidle_timeout: 30m\nallow:\n  tools: [{tools}]\nresources:\n  desktop:\n    windows:\n      - pid: 42\n        window_id: 7\n").unwrap();
    let manifest = Arc::new(crate::session_manifest::load_manifest(file.path()).unwrap());
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Bounded],
        false,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Bounded, Some(manifest))
        .unwrap()
}
#[cfg(feature = "yaml")]
#[tokio::test]
async fn sequence_nested_authorization_never_falls_back_to_legacy() {
    let _serial = SERIAL.lock().await;
    for (tools, reason, calls) in [
        ("run_sequence, click", "verification_error", 1),
        ("run_sequence, verify_state", "action_error", 0),
    ] {
        let (registry, log) = registry("ok", snapshot(true));
        let result = registry
            .invoke_with_context("run_sequence", input(), limited_context(tools))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let output = result.structured_content.unwrap();
        assert_eq!(output["status"], "stopped");
        assert_eq!(output["stop_reason"], reason);
        assert_eq!(output["steps"].as_array().unwrap().len(), 1);
        assert!(output["steps"][0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("capability manifest"));
        assert_eq!(log.lock().unwrap().len(), calls);
    }
}
#[tokio::test]
async fn sequence_sessions_are_projected_once_and_isolated_by_context() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let contexts = [context(), context()];
    for context in &contexts {
        let mut args = input();
        args["session"] = json!("shared-sequence-label");
        let result = registry
            .invoke_with_context("run_sequence", args, context.clone())
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["status"],
            "completed",
            "{result:?}"
        );
    }
    let log = log.lock().unwrap();
    for (offset, context) in [(0, &contexts[0]), (4, &contexts[1])] {
        for index in [offset, offset + 2] {
            assert_eq!(
                log[index].1["session"],
                context.runtime_session_key("shared-sequence-label")
            );
            assert_eq!(
                log[index].1["_public_session_label"],
                "shared-sequence-label"
            );
            assert_eq!(log[index].1["pid"], 42);
            assert_eq!(log[index].1["window_id"], 7);
            assert_eq!(log[index].1["delivery_mode"], "background");
        }
    }
    assert_ne!(log[0].1["session"], log[4].1["session"]);
}
#[tokio::test]
async fn sequence_ended_session_refuses_without_child_calls() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let context = context();
    let mut args = input();
    args["session"] = json!("sequence-ended");
    let result = registry
        .invoke_with_context("run_sequence", args.clone(), context.clone())
        .await;
    assert_ne!(result.is_error, Some(true));
    let end = registry
        .invoke_with_context(
            "end_session",
            json!({"session":"sequence-ended"}),
            context.clone(),
        )
        .await;
    assert_ne!(end.is_error, Some(true), "{end:?}");
    log.lock().unwrap().clear();
    let result = registry
        .invoke_with_context("run_sequence", args, context)
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(format!("{result:?}").contains("session_ended"));
    assert!(log.lock().unwrap().is_empty());
}
#[tokio::test]
async fn sequence_recording_contains_each_child_action_once() {
    let _serial = SERIAL.lock().await;
    let (registry, _) = registry("ok", snapshot(true));
    let directory = tempfile::tempdir().unwrap();
    registry
        .recording
        .start(directory.path().to_str().unwrap(), false, None)
        .unwrap();
    let output = run(&registry, input()).await;
    assert_eq!(output["status"], "completed");
    registry.recording.stop_owner(None).unwrap();
    let mut turns = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("turn-")
        })
        .collect::<Vec<_>>();
    turns.sort();
    assert_eq!(turns.len(), 2);
    for (path, tool) in turns.iter().zip(["click", "type_text"]) {
        let action: Value =
            serde_json::from_slice(&std::fs::read(path.join("action.json")).unwrap()).unwrap();
        assert_eq!(action["tool"], tool);
    }
}
#[tokio::test]
async fn sequence_rejects_explicit_null_fields_before_any_child() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    for key in ["x", "y", "element_token"] {
        let mut args = input();
        args["steps"][1]["arguments"][key] = Value::Null;
        let result = registry
            .invoke_with_context("run_sequence", args.clone(), context())
            .await;
        assert_eq!(result.is_error, Some(true), "accepted {args}");
    }
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sequence_uses_canonical_registry_normalization_for_legacy_actions() {
    let _serial = SERIAL.lock().await;
    for response in ["unstructured", "malformed"] {
        let (registry, log) = registry(response, snapshot(true));
        let output = run(&registry, input()).await;
        assert_eq!(output["status"], "completed", "{output}");
        assert_eq!(output["steps"][0]["action"]["effect"], "unverifiable");
        assert_eq!(log.lock().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn sequence_capture_scope_is_window_only() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let context = context();
    let session = context.runtime_session_key("sequence-desktop-scope");
    crate::capture_scope::bind_session(&session, Some(cua_driver_contract::CaptureScope::Desktop))
        .unwrap();
    let mut args = input();
    args["session"] = json!("sequence-desktop-scope");
    let result = registry
        .invoke_with_context("run_sequence", args, context)
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(log.lock().unwrap().is_empty());
    crate::session::end_session(&session);
}
#[test]
fn sequence_reviewed_risk_is_bounded_observation_and_input_without_file_egress() {
    use crate::authorization::{
        advertised_risk_for, classify_tool_call, enforcement_adapters_for_call, RiskClass,
        RiskEnforcement,
    };
    let risk = classify_tool_call("run_sequence", &input());
    assert_eq!(risk.class, RiskClass::R2);
    assert_eq!(risk.enforcement, RiskEnforcement::Active);
    assert_eq!(advertised_risk_for("run_sequence").class, RiskClass::R2);
    let adapters = enforcement_adapters_for_call("run_sequence", &input());
    assert_eq!(
        adapters
            .iter()
            .map(|adapter| adapter.id)
            .collect::<Vec<_>>(),
        ["private_observation", "desktop_input"]
    );
}

#[tokio::test]
async fn sequence_nested_dispatch_preserves_transport_owner() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let context = context();
    let evidence = crate::tool::TrustedInvocationEvidence::extract_from_adapter_args(
        &mut json!({"_session_id":"wire-sequence", "_transport_session_id":"mcp-connection-sequence"}),
    );
    let mut args = input();
    args["session"] = json!("wire-sequence");
    let result = registry
        .invoke_with_context_and_evidence("run_sequence", args, context.clone(), evidence)
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(result.structured_content.unwrap()["status"], "completed");
    for (_, args) in log
        .lock()
        .unwrap()
        .iter()
        .filter(|(tool, _)| tool != "observe")
    {
        assert_eq!(
            args["_session_id"],
            context.runtime_session_key("wire-sequence")
        );
        assert_eq!(
            args["_transport_session_id"],
            context.runtime_session_key("mcp-connection-sequence")
        );
        assert_eq!(args["_public_session_label"], "wire-sequence");
    }
}

#[tokio::test]
async fn sequence_preserves_public_labels_that_resemble_runtime_namespaces() {
    let _serial = SERIAL.lock().await;
    let (registry, log) = registry("ok", snapshot(true));
    let context = context();
    let label = "__cua_runtime_custom";
    let mut args = input();
    args["session"] = json!(label);
    let result = registry
        .invoke_with_context("run_sequence", args, context.clone())
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(result.structured_content.unwrap()["status"], "completed");
    let log = log.lock().unwrap();
    for index in [0, 2] {
        assert_eq!(log[index].1["session"], context.runtime_session_key(label));
        assert_eq!(
            log[index].1["_session_id"],
            context.runtime_session_key(label)
        );
    }
}
