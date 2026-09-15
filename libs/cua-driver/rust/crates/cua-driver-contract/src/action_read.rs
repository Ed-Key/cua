// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
//! One background action and its subsequent observation. macOS initial scope.
use crate::{Platform, SchemaMode, ToolAnnotations, ToolContract, ToolInput, ToolOutput};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn string(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string"})
}
fn boolean(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"boolean"})
}
fn pid(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1,"maximum":2147483647})
}
fn window(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1,"maximum":4294967295_u64})
}
fn elements(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1,"maximum":2000})
}
fn depth(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1,"maximum":100})
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionReadAction {
    Click,
    SetValue,
}
impl ActionReadAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::SetValue => "set_value",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionReadObservation {
    #[serde(default)]
    pub include_screenshot: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    pub query: Option<String>,
    #[serde(default)]
    pub query_context: bool,
    /// AX nodes visited before query filtering, including containers and menus.
    /// Omit for normal reads (default 2000). Use query/query_context to reduce
    /// returned text. Lower this to bound collection work; later content or a
    /// matching target may then be omitted even when the response is small.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "elements")]
    pub max_elements: Option<u32>,
    /// AX traversal depth before query filtering. Omit for normal reads
    /// (default 25). Lowering this can exclude deeper matching content.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "depth")]
    pub max_depth: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActAndReadInput {
    #[schemars(schema_with = "pid")]
    pub pid: u32,
    #[schemars(schema_with = "window")]
    pub window_id: u32,
    pub action: ActionReadAction,
    pub element_token: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    pub value: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    pub session: Option<String>,
    #[serde(default)]
    pub observe: ActionReadObservation,
}
impl ToolInput for ActAndReadInput {
    const TOOL_NAME: &'static str = "act_and_read";
    fn validate(&self) -> Result<(), String> {
        if self.pid == 0 || self.pid > i32::MAX as u32 || self.window_id == 0 {
            return Err("positive macOS process and window IDs are required".into());
        }
        if self.element_token.trim().is_empty() {
            return Err("a nonblank element_token is required".into());
        }
        if (self.action == ActionReadAction::SetValue) != self.value.is_some() {
            return Err("value is required only for set_value".into());
        }
        if self.observe.query_context
            && self
                .observe
                .query
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
        {
            return Err("query_context requires a nonblank query".into());
        }
        if self
            .observe
            .max_elements
            .is_some_and(|n| !(1..=2000).contains(&n))
            || self
                .observe
                .max_depth
                .is_some_and(|n| !(1..=100).contains(&n))
        {
            return Err("observation limits exceed the bounded action/read contract".into());
        }
        Ok(())
    }
}

/// Public child result metadata. Content blocks are forwarded once in the outer
/// MCP content array, in action then observation order.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionReadChildResult {
    #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "boolean")]
    pub is_error: Option<bool>,
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionReadTimings {
    pub action_ms: u64,
    pub observation_ms: u64,
    pub total_ms: u64,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActAndReadOutput {
    pub action: ActionReadChildResult,
    pub observation: ActionReadChildResult,
    pub timings: ActionReadTimings,
}
impl ToolOutput for ActAndReadOutput {}

pub fn contracts() -> Vec<ToolContract> {
    vec![ToolContract {
        name: ActAndReadInput::TOOL_NAME.into(),
        description: "macOS: run one token-targeted background click or set_value, then read the same window's fresh accessibility state. Use when you already intend to act and inspect. Tree-only by default; observe selects query/context and optional screenshot. Returns both child results separately, including action errors. No retries or semantic verification: a fresh read does not prove the action succeeded. Not a transaction against other clients or user input. Windows and Linux are not yet supported.".into(),
        platforms:vec![Platform::Macos], aliases:vec![], capabilities:vec!["action.read".into()],
        annotations:ToolAnnotations { read_only:false,destructive:true,idempotent:false,open_world:false },
        schema_mode:SchemaMode::CanonicalRuntime,cursor_semantics:None,
        input_schema:ActAndReadInput::input_schema(),success_output_schema:Some(ActAndReadOutput::output_schema()),
        output_validator:crate::validate_typed_output::<ActAndReadOutput>,
    }]
}
