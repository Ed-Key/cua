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
fn action(generator: &mut SchemaGenerator) -> Schema {
    ActionReadAction::json_schema(generator)
}
fn strings(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"array","items":{"type":"string"}})
}
fn keys(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"array","items":{"type":"string"},"minItems":1})
}
fn steps(generator: &mut SchemaGenerator) -> Schema {
    let item = generator.subschema_for::<ActionReadStep>();
    json_schema!({"type":"array","items":item,"minItems":1,"maxItems":MAX_STEPS})
}

/// Upper bound on actions in one `steps` call.
const MAX_STEPS: usize = 8;

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

// Step actions. Separate from ActionReadAction so the single-action form
// keeps exactly its original three actions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionReadStepKind {
    Click,
    SetValue,
    Scroll,
    TypeText,
    PressKey,
    Hotkey,
}
impl ActionReadStepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::SetValue => "set_value",
            Self::Scroll => "scroll",
            Self::TypeText => "type_text",
            Self::PressKey => "press_key",
            Self::Hotkey => "hotkey",
        }
    }
}

/// One action in `steps`. Every step targets the call's pid and window_id.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ActionReadStep {
    pub action: ActionReadStepKind,
    /// Required for click, set_value and scroll; optional for type_text; not allowed for
    /// press_key and hotkey.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub element_token: Option<String>,
    /// Required for set_value; not allowed for other actions.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub value: Option<String>,
    /// Required for type_text; not allowed for other actions.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub text: Option<String>,
    /// Required for press_key; not allowed for other actions.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub key: Option<String>,
    /// Optional for press_key; not allowed for other actions.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "strings")]
    #[uniffi(default = None)]
    pub modifiers: Option<Vec<String>>,
    /// Required for hotkey, for example ["cmd", "a"]; not allowed for other actions.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "keys")]
    #[uniffi(default = None)]
    pub keys: Option<Vec<String>>,
    /// Required for scroll; not allowed for other actions.
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
}
impl ActionReadStep {
    pub fn validate(&self) -> Result<(), String> {
        use ActionReadStepKind::*;
        let kind = self.action;
        let name = kind.as_str();
        let token = self.element_token.as_deref();
        match kind {
            Click | SetValue | Scroll if token.is_none_or(|t| t.trim().is_empty()) => {
                return Err(format!("a nonblank element_token is required for {name}"));
            }
            TypeText if token.is_some_and(|t| t.trim().is_empty()) => {
                return Err("element_token must be nonblank when given".into());
            }
            PressKey | Hotkey if token.is_some() => {
                return Err(format!("element_token is not allowed for {name}"));
            }
            _ => {}
        }
        // (field, present, the one kind that accepts it, required for that kind)
        for (field, present, owner, required) in [
            ("value", self.value.is_some(), SetValue, true),
            ("text", self.text.is_some(), TypeText, true),
            ("key", self.key.is_some(), PressKey, true),
            ("modifiers", self.modifiers.is_some(), PressKey, false),
            ("keys", self.keys.is_some(), Hotkey, true),
            ("direction", self.direction.is_some(), Scroll, true),
            ("by", self.by.is_some(), Scroll, false),
            ("amount", self.amount.is_some(), Scroll, false),
        ] {
            if present && kind != owner {
                return Err(format!("{field} is only allowed for {}", owner.as_str()));
            }
            if !present && kind == owner && required {
                return Err(format!("{field} is required for {name}"));
            }
        }
        if self.key.as_ref().is_some_and(|k| k.trim().is_empty()) {
            return Err("key must be nonblank".into());
        }
        if self
            .keys
            .as_ref()
            .is_some_and(|k| k.is_empty() || k.iter().any(|k| k.trim().is_empty()))
        {
            return Err("keys must be a non-empty array of nonblank strings".into());
        }
        if self.amount.is_some_and(|n| !(1..=50).contains(&n)) {
            return Err("scroll amount must be between 1 and 50".into());
        }
        Ok(())
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
    /// Single-action form: click, set_value or scroll. Omit when passing steps.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "action")]
    #[uniffi(default = None)]
    pub action: Option<ActionReadAction>,
    /// Single-action form: the target element. Omit when passing steps.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub element_token: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    /// Required for action set_value; not allowed for other actions.
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
    /// For multi-call work, prefer a short public session label and repeat it on every call that
    /// accepts it. Unnamed calls use the transport's implicit session.
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
    /// Instead of action: 1 to 8 actions on this window, run in order and stopped at the first
    /// failure, then one observation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "steps")]
    #[uniffi(default = None)]
    pub steps: Option<Vec<ActionReadStep>>,
}
impl ToolInput for ActAndReadInput {
    const TOOL_NAME: &'static str = "act_and_read";
    fn validate(&self) -> Result<(), String> {
        if self.pid == 0 || self.pid > i32::MAX as u32 || self.window_id == 0 {
            return Err("positive macOS process and window IDs are required".into());
        }
        if let Some(steps) = &self.steps {
            if self.action.is_some()
                || self.element_token.is_some()
                || self.value.is_some()
                || self.direction.is_some()
                || self.by.is_some()
                || self.amount.is_some()
            {
                return Err("pass either steps or the single-action fields (action, element_token, value, direction, by, amount), not both".into());
            }
            if !(1..=MAX_STEPS).contains(&steps.len()) {
                return Err(format!(
                    "steps must contain between 1 and {MAX_STEPS} actions"
                ));
            }
            for (i, step) in steps.iter().enumerate() {
                step.validate()
                    .map_err(|e| format!("step {}: {e}", i + 1))?;
            }
        } else {
            let Some(action) = self.action else {
                return Err("action and element_token are required unless steps is given".into());
            };
            if self
                .element_token
                .as_deref()
                .is_none_or(|t| t.trim().is_empty())
            {
                return Err("a nonblank element_token is required".into());
            }
            if (action == ActionReadAction::SetValue) != self.value.is_some() {
                return Err("value is required only for set_value".into());
            }
            if (action == ActionReadAction::Scroll) != self.direction.is_some()
                || (action != ActionReadAction::Scroll
                    && (self.by.is_some() || self.amount.is_some()))
            {
                return Err(
                    "direction is required for scroll; direction, by and amount are scroll-only"
                        .into(),
                );
            }
            if self.amount.is_some_and(|n| !(1..=50).contains(&n)) {
                return Err("scroll amount must be between 1 and 50".into());
            }
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    /// The single action's result, or for steps the last step that ran.
    pub action: ActionReadChildResult,
    pub observation: ActionReadChildResult,
    pub timings: ActionReadTimings,
    /// Steps form only: results of the steps that ran, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<ActionReadChildResult>>,
    /// Steps form only: 1-based index of the step that failed. Absent when all steps ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<u32>,
}
impl ToolOutput for ActAndReadOutput {}

pub fn contracts() -> Vec<ToolContract> {
    vec![ToolContract {
        name: ActAndReadInput::TOOL_NAME.into(),
        description: "macOS: act, then read the same window's fresh accessibility state in one call. Pass one token-targeted background click, set_value or scroll, or `steps`: up to 8 actions on this window (click, set_value, scroll, type_text, press_key, hotkey), run in order in the background and stopped at the first failure. Use steps for work whose targets are already visible, such as several button presses or a field then its submit button, instead of one call per action. Tree-only by default; observe selects query/context and optional screenshot. Returns each child result, including errors and which step stopped. No retries or semantic verification: the fresh read shows what happened. Not a transaction against other clients or user input. Windows and Linux are not yet supported.".into(),
        platforms:vec![Platform::Macos], aliases:vec![], capabilities:vec!["action.read".into()],
        annotations:ToolAnnotations { read_only:false,destructive:true,idempotent:false,open_world:false },
        schema_mode:SchemaMode::CanonicalRuntime,cursor_semantics:None,
        input_schema:ActAndReadInput::input_schema(),success_output_schema:Some(ActAndReadOutput::output_schema()),error_output_schema:None,
        output_validator:crate::validate_typed_output::<ActAndReadOutput>,
    }]
}
