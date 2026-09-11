// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Bounded, ordered actions and existing state verification for one window.

use crate::{
    ActionResult, Platform, SchemaMode, StatePredicate, ToolAnnotations, ToolContract, ToolInput,
    ToolOutput, VerifyStateInput, VerifyStateOutput,
};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

fn present_value<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn positive_integer_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1})
}
fn number_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"number"})
}
fn string_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string"})
}
fn timeout_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":0,"maximum":10000,"default":1000})
}
fn stability_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1,"maximum":5,"default":2})
}

fn nullable_schema<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf": [generator.subschema_for::<T>(), {"type": "null"}]})
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SequenceTool {
    Click,
    TypeText,
}
impl SequenceTool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::TypeText => "type_text",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct SequenceArguments {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "number_schema")]
    pub x: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "number_schema")]
    pub y: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "string_schema")]
    pub element_token: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "string_schema")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct SequenceStep {
    pub tool: SequenceTool,
    pub arguments: SequenceArguments,
    #[schemars(length(min = 1, max = 8))]
    pub expect: Vec<StatePredicate>,
    /// Zero performs one sample, using one stable sample when stability is omitted.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "timeout_schema")]
    pub timeout_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "stability_schema")]
    pub stable_samples: Option<u64>,
}
impl SequenceStep {
    pub fn verification_input(&self, input: &RunSequenceInput) -> VerifyStateInput {
        VerifyStateInput {
            pid: input.pid,
            window_id: input.window_id,
            session: input.session.clone(),
            expect: self.expect.clone(),
            timeout_ms: Some(self.timeout_ms.unwrap_or(1000)),
            stable_samples: self.stable_samples,
            include_screenshot: Some(false),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct RunSequenceInput {
    #[schemars(schema_with = "positive_integer_schema")]
    pub pid: i64,
    #[schemars(schema_with = "positive_integer_schema")]
    pub window_id: u64,
    /// Public lifecycle session label, shared by all child calls.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    #[schemars(schema_with = "string_schema")]
    pub session: Option<String>,
    #[schemars(length(min = 1, max = 8))]
    pub steps: Vec<SequenceStep>,
}
impl ToolInput for RunSequenceInput {
    const TOOL_NAME: &'static str = "run_sequence";
    fn validate(&self) -> Result<(), String> {
        RunSequenceInput::validate(self)
    }
}
impl RunSequenceInput {
    pub fn validate(&self) -> Result<(), String> {
        if self.pid <= 0 || self.window_id == 0 {
            return Err("run_sequence requires pid > 0 and window_id > 0".into());
        }
        if !(1..=8).contains(&self.steps.len()) {
            return Err("run_sequence requires between 1 and 8 steps".into());
        }
        for (index, step) in self.steps.iter().enumerate() {
            let args = &step.arguments;
            let valid = match step.tool {
                SequenceTool::Click => {
                    args.text.is_none()
                        && match (&args.element_token, args.x, args.y) {
                            (Some(token), None, None) => !token.trim().is_empty(),
                            (None, Some(x), Some(y)) => x.is_finite() && y.is_finite(),
                            _ => false,
                        }
                }
                SequenceTool::TypeText => {
                    args.text.is_some()
                        && args.x.is_none()
                        && args.y.is_none()
                        && args.element_token.is_none()
                }
            };
            if !valid {
                return Err(format!("run_sequence step {index}: click requires exactly a nonempty element_token or finite x and y; type_text requires exactly text"));
            }
            step.verification_input(self)
                .validate()
                .map_err(|error| format!("run_sequence step {index}: {error}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SequenceStatus {
    Completed,
    Stopped,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SequenceStopReason {
    Unsatisfied,
    Unknown,
    ActionError,
    VerificationError,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct SequenceError {
    pub code: String,
    /// UTF-8 diagnostic capped at 2000 bytes.
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct SequenceStepOutput {
    pub index: u64,
    pub tool: SequenceTool,
    #[schemars(required, schema_with = "nullable_schema::<ActionResult>")]
    pub action: Option<ActionResult>,
    #[schemars(required, schema_with = "nullable_schema::<VerifyStateOutput>")]
    pub verification: Option<VerifyStateOutput>,
    #[schemars(required, schema_with = "nullable_schema::<SequenceError>")]
    pub error: Option<SequenceError>,
    pub dispatch_ms: u64,
    pub verification_ms: u64,
    pub observation_count: u64,
    pub image_bytes_returned: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct RunSequenceOutput {
    pub status: SequenceStatus,
    #[schemars(required, schema_with = "nullable_schema::<u64>")]
    pub stopped_at: Option<u64>,
    #[schemars(required, schema_with = "nullable_schema::<SequenceStopReason>")]
    pub stop_reason: Option<SequenceStopReason>,
    pub elapsed_ms: u64,
    pub executor_overhead_ms: u64,
    pub steps: Vec<SequenceStepOutput>,
}
impl ToolOutput for RunSequenceOutput {}

pub fn contracts() -> Vec<ToolContract> {
    vec![ToolContract {
        name:RunSequenceInput::TOOL_NAME.into(),
        description:"Run one to eight ordered click/type_text actions against one exact window with background delivery. Each action is followed by the existing verify_state predicates; continue only when satisfied and stable. Stop on unknown, unsatisfied, action refusal, partial delivery or error. Unattempted steps are omitted. No retries, rollback or screenshots. This is not a transaction; a satisfied postcondition does not prove causation. Typed and existing verification request-validation errors reject the whole request before any child. Predicate shapes the verifier classifies unknown remain normal stopped outcomes.".into(),
        platforms:vec![Platform::Macos,Platform::Windows,Platform::Linux], aliases:vec![], capabilities:vec!["sequence.run".into()],
        annotations:ToolAnnotations { read_only:false, destructive:true, idempotent:false, open_world:false },
        schema_mode:SchemaMode::PortableSubset, cursor_semantics:None,
        input_schema:RunSequenceInput::input_schema(), success_output_schema:Some(RunSequenceOutput::output_schema()), error_output_schema:None,
        output_validator:crate::validate_typed_output::<RunSequenceOutput>,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn input() -> RunSequenceInput {
        serde_json::from_value(json!({"pid":42,"window_id":7,"steps":[{"tool":"click","arguments":{"x":0,"y":0},"expect":[{"window":{"exists":true}}]}]})).unwrap()
    }
    #[test]
    fn sequence_rejects_nonfinite_coordinates_and_invalid_targets() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut request = input();
            request.steps[0].arguments.x = Some(value);
            assert!(request.validate().is_err());
            assert!(ToolInput::validate(&request).is_err());
        }
        let mut request = input();
        request.pid = 0;
        assert!(request.validate().is_err());
        let mut request = input();
        request.window_id = 0;
        assert!(request.validate().is_err());
    }
    #[test]
    fn sequence_defaults_and_zero_timeout_share_verifier_validation() {
        let mut request = input();
        assert_eq!(
            request.steps[0].verification_input(&request).timeout_ms,
            Some(1000)
        );
        assert!(request.validate().is_ok());
        request.steps[0].timeout_ms = Some(0);
        assert!(request.validate().is_ok());
        request.steps[0].stable_samples = Some(2);
        assert!(request.validate().is_err());
        request.steps[0].stable_samples = Some(1);
        assert!(request.validate().is_ok());
    }
}
