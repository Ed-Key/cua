// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
//! One background action and its subsequent observation. macOS initial scope.
use crate::{
    Platform, SchemaMode, ScrollBy, ScrollDirection, ToolAnnotations, ToolContract, ToolInput,
    ToolOutput,
};
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

fn direction(generator: &mut SchemaGenerator) -> Schema {
    ScrollDirection::json_schema(generator)
}
fn granularity(generator: &mut SchemaGenerator) -> Schema {
    ScrollBy::json_schema(generator)
}
fn amount(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer","minimum":1,"maximum":50})
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionReadAction {
    Click,
    SetValue,
    Scroll,
}
impl ActionReadAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::SetValue => "set_value",
            Self::Scroll => "scroll",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionReadObservation {
    #[serde(default)]
    #[uniffi(default = false)]
    pub include_screenshot: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub query: Option<String>,
    #[serde(default)]
    #[uniffi(default = false)]
    pub query_context: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "elements")]
    #[uniffi(default = None)]
    pub max_elements: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "depth")]
    #[uniffi(default = None)]
    pub max_depth: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, uniffi::Record)]
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
    #[uniffi(default = None)]
    pub value: Option<String>,
    /// Required only for scroll. Uses the existing background scroll route.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "direction")]
    #[uniffi(default = None)]
    pub direction: Option<ScrollDirection>,
    /// Scroll only. Omit to retain the scroll tool's line default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "granularity")]
    #[uniffi(default = None)]
    pub by: Option<ScrollBy>,
    /// Scroll only, 1 through 50. Omit to retain the scroll tool's default of 3.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "amount")]
    #[uniffi(default = None)]
    pub amount: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub session: Option<String>,
    #[serde(default)]
    #[uniffi(default)]
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
        if (self.action == ActionReadAction::Scroll) != self.direction.is_some()
            || (self.action != ActionReadAction::Scroll
                && (self.by.is_some() || self.amount.is_some()))
        {
            return Err(
                "direction is required for scroll; direction, by and amount are scroll-only".into(),
            );
        }
        if self.amount.is_some_and(|n| !(1..=50).contains(&n)) {
            return Err("scroll amount must be between 1 and 50".into());
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
        description: "macOS: run one token-targeted background click, set_value or scroll, then read the same window's fresh accessibility state. Use when you already intend to act and inspect. Scroll requires direction, accepts optional by and amount, and uses the existing scroll tool defaults. Tree-only by default; observe selects query/context and optional screenshot. Returns both child results separately, including action errors. No retries or semantic verification: a fresh read does not prove the action succeeded. Not a transaction against other clients or user input. Windows and Linux are not yet supported.".into(),
        platforms:vec![Platform::Macos], aliases:vec![], capabilities:vec!["action.read".into()],
        annotations:ToolAnnotations { read_only:false,destructive:true,idempotent:false,open_world:false },
        schema_mode:SchemaMode::CanonicalRuntime,cursor_semantics:None,
        input_schema:ActAndReadInput::input_schema(),success_output_schema:Some(ActAndReadOutput::output_schema()),error_output_schema:None,
        output_validator:crate::validate_typed_output::<ActAndReadOutput>,
    }]
}
