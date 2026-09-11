// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Ordered, bounded composition through the existing trusted registry boundary.

use crate::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::parse_typed_input,
};
use async_trait::async_trait;
use cua_driver_contract::{
    ActionEffect, ActionResult, RunSequenceInput, RunSequenceOutput, SequenceError, SequenceStatus,
    SequenceStepOutput, SequenceStopReason, ToolInput, VerificationStatus, VerifyStateOutput,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

pub struct RunSequenceTool {
    registry: Arc<Mutex<Weak<ToolRegistry>>>,
}
impl RunSequenceTool {
    pub fn new(registry: Arc<Mutex<Weak<ToolRegistry>>>) -> Self {
        Self { registry }
    }
}
#[async_trait]
impl Tool for RunSequenceTool {
    fn def(&self) -> &ToolDef {
        static DEF: OnceLock<ToolDef> = OnceLock::new();
        DEF.get_or_init(|| {
            let contract = cua_driver_contract::tool_contract(RunSequenceInput::TOOL_NAME)
                .expect("run_sequence contract");
            ToolDef {
                name: contract.name,
                description: contract.description,
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }
    async fn invoke(&self, mut args: Value) -> ToolResult {
        // Registry namespace metadata is trusted, but child calls require the public label.
        // An implicit transport session needs no public label: nested dispatch inherits its context.
        if args
            .get("session")
            .and_then(Value::as_str)
            .is_some_and(|label| label.starts_with("__cua_runtime_"))
        {
            let public_label = args.get("_public_session_label").cloned();
            let arguments = args.as_object_mut().expect("session belongs to an object");
            arguments.remove("session");
            if let Some(label) = public_label {
                arguments.insert("session".into(), label);
            }
        }
        let input: RunSequenceInput = match parse_typed_input("run_sequence", args) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if let Err(error) = validate_sequence(&input) {
            return ToolResult::error(error.clone()).with_structured(
                json!({"code":"invalid_arguments","tool":"run_sequence","detail":error}),
            );
        }
        let registry = match self.registry.lock().unwrap().upgrade() {
            Some(registry) => registry,
            None => return ToolResult::error("run_sequence registry is unavailable"),
        };
        let output = execute_sequence(&registry, input).await;
        ToolResult::text(format!(
            "run_sequence: {:?} after {} step(s)",
            output.status,
            output.steps.len()
        ))
        .with_structured(serde_json::to_value(output).expect("RunSequenceOutput serializes"))
    }
}

pub fn validate_sequence(input: &RunSequenceInput) -> Result<(), String> {
    input.validate()
}
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}
fn error(code: &str, message: impl Into<String>) -> SequenceError {
    let mut message = message.into();
    if message.len() > 2000 {
        let mut end = 1997;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str("...");
    }
    SequenceError {
        code: code.into(),
        message,
    }
}
fn structured_result<T: DeserializeOwned>(
    result: &ToolResult,
    stage: &str,
) -> Result<T, SequenceError> {
    if result.is_error == Some(true) {
        let message = result
            .content
            .iter()
            .find_map(|content| match content {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("child tool failed");
        return Err(error(stage, message));
    }
    let value = result
        .structured_content
        .clone()
        .ok_or_else(|| error(stage, "child tool omitted structured evidence"))?;
    serde_json::from_value(value).map_err(|detail| {
        error(
            stage,
            format!("invalid child structured evidence: {detail}"),
        )
    })
}

/// Execute a request already checked by `validate_sequence`.
/// Every child re-enters the registry, retaining the current trusted context.
pub async fn execute_sequence(
    registry: &ToolRegistry,
    input: RunSequenceInput,
) -> RunSequenceOutput {
    let started = Instant::now();
    let mut child_time = Duration::ZERO;
    let mut output = RunSequenceOutput {
        status: SequenceStatus::Completed,
        stopped_at: None,
        stop_reason: None,
        elapsed_ms: 0,
        executor_overhead_ms: 0,
        steps: Vec::with_capacity(input.steps.len()),
    };
    for (index, step) in input.steps.iter().enumerate() {
        let mut action_args =
            serde_json::to_value(&step.arguments).expect("SequenceArguments serializes");
        action_args["pid"] = json!(input.pid);
        action_args["window_id"] = json!(input.window_id);
        action_args["delivery_mode"] = json!("background");
        if let Some(session) = &input.session {
            action_args["session"] = json!(session);
        }
        let dispatch_started = Instant::now();
        let result = registry.invoke(step.tool.as_str(), action_args).await;
        let dispatch_duration = dispatch_started.elapsed();
        child_time += dispatch_duration;
        let mut row = SequenceStepOutput {
            index: index as u64,
            tool: step.tool,
            action: None,
            verification: None,
            error: None,
            dispatch_ms: millis(dispatch_duration),
            verification_ms: 0,
            observation_count: 0,
            image_bytes_returned: 0,
        };
        let mut stop_reason = None;
        match structured_result::<ActionResult>(&result, "action_error") {
            Err(error) => {
                row.error = Some(error);
                stop_reason = Some(SequenceStopReason::ActionError);
            }
            Ok(action) => {
                if let Err(detail) = action.validate_invariants() {
                    row.error = Some(error(
                        "action_error",
                        format!("invalid action result: {detail}"),
                    ));
                    stop_reason = Some(SequenceStopReason::ActionError);
                } else {
                    if matches!(action.effect, ActionEffect::Refused | ActionEffect::Partial) {
                        row.error = Some(error(
                            "action_error",
                            format!("action effect is {:?}", action.effect),
                        ));
                        stop_reason = Some(SequenceStopReason::ActionError);
                    }
                    row.action = Some(action);
                }
            }
        }
        if stop_reason.is_none() {
            let verification_args = serde_json::to_value(step.verification_input(&input))
                .expect("VerifyStateInput serializes");
            let verification_started = Instant::now();
            let result = registry.invoke("verify_state", verification_args).await;
            let verification_duration = verification_started.elapsed();
            child_time += verification_duration;
            row.verification_ms = millis(verification_duration);
            match structured_result::<VerifyStateOutput>(&result, "verification_error") {
                Err(error) => {
                    row.error = Some(error);
                    stop_reason = Some(SequenceStopReason::VerificationError);
                }
                Ok(verification) => {
                    row.observation_count = verification.samples;
                    stop_reason = match verification.status {
                        VerificationStatus::Satisfied if verification.stable => None,
                        VerificationStatus::Unsatisfied => Some(SequenceStopReason::Unsatisfied),
                        _ => Some(SequenceStopReason::Unknown),
                    };
                    row.verification = Some(verification);
                }
            }
        }
        output.steps.push(row);
        if let Some(reason) = stop_reason {
            output.status = SequenceStatus::Stopped;
            output.stopped_at = Some(index as u64);
            output.stop_reason = Some(reason);
            break;
        }
    }
    let elapsed = started.elapsed();
    output.elapsed_ms = millis(elapsed);
    output.executor_overhead_ms = millis(elapsed.saturating_sub(child_time));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequence_rejects_missing_or_malformed_structured_evidence_at_executor_boundary() {
        for result in [
            ToolResult::text("no structure"),
            ToolResult::text("malformed").with_structured(json!({"effect":"made_up"})),
        ] {
            assert!(structured_result::<ActionResult>(&result, "action_error").is_err());
            assert!(structured_result::<VerifyStateOutput>(&result, "verification_error").is_err());
        }
    }
    #[test]
    fn sequence_error_is_utf8_bounded() {
        let error = error("action_error", "é".repeat(2000));
        assert!(error.message.len() <= 2000);
        assert!(error.message.ends_with("..."));
    }
}
