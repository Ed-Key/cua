// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
//! Up to eight page actions on one bound browser tab, then what the page changed.
use crate::{
    PageChanges, Platform, SchemaMode, ToolAnnotations, ToolContract, ToolInput, ToolOutput,
    MULTI_CALL_SESSION_DESCRIPTION,
};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn string(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string"})
}
fn boolean(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"boolean"})
}
fn session(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string","description":MULTI_CALL_SESSION_DESCRIPTION})
}
fn route(generator: &mut SchemaGenerator) -> Schema {
    BrowserStepRoute::json_schema(generator)
}
fn expect(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<BrowserStepExpect>()
}
fn steps(generator: &mut SchemaGenerator) -> Schema {
    let item = generator.subschema_for::<BrowserStep>();
    json_schema!({"type":"array","items":item,"minItems":1,"maxItems":MAX_BROWSER_STEPS})
}
fn yes() -> bool {
    true
}

/// Upper bound on steps in one `browser_steps` call.
pub const MAX_BROWSER_STEPS: usize = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum BrowserStepAction {
    Click,
    Type,
}
impl BrowserStepAction {
    /// The tool that runs the step, and whose checks admit it.
    pub fn tool(self) -> &'static str {
        match self {
            Self::Click => "browser_click",
            Self::Type => "browser_type",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum BrowserStepRoute {
    Trusted,
    DomEvent,
}

/// What must hold once a step has settled: some element with this role,
/// exact name and/or text (or, with `present: false`, none). The text may be
/// the element's own or that of anything inside it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct BrowserStepExpect {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub role: Option<String>,
    /// Exact accessible name.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// Text contained in the element's name or value, or in those of its descendants.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub text: Option<String>,
    /// `false`: no such element may be there. Default true.
    #[serde(default = "yes")]
    #[uniffi(default = true)]
    pub present: bool,
}

/// One step. Target it with `ref`, or with `role` and `name` (matched
/// exactly against the live page when the step runs).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct BrowserStep {
    pub action: BrowserStepAction,
    /// Page ref from an outline.
    #[serde(
        rename = "ref",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub reference: Option<String>,
    /// With name, instead of ref.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub role: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// type: the text.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "string")]
    #[uniffi(default = None)]
    pub text: Option<String>,
    /// type: replace the field's content instead of appending.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "boolean")]
    #[uniffi(default = None)]
    pub replace: Option<bool>,
    /// click: trusted (default) or dom_event (ref or role and name).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "route")]
    #[uniffi(default = None)]
    pub input_route: Option<BrowserStepRoute>,
    /// Checked after the step; the batch stops when it does not hold.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "expect")]
    #[uniffi(default = None)]
    pub expect: Option<BrowserStepExpect>,
}

impl BrowserStep {
    pub fn validate(&self) -> Result<(), String> {
        let blank = |value: &Option<String>| value.as_deref().is_some_and(|v| v.trim().is_empty());
        if blank(&self.reference) || blank(&self.role) || blank(&self.name) {
            return Err("ref, role and name must be nonblank when given".into());
        }
        match (&self.reference, &self.role, &self.name) {
            (Some(_), None, None) | (None, Some(_), Some(_)) => {}
            _ => return Err("target the step with ref, or with role and name together".into()),
        }
        match self.action {
            BrowserStepAction::Type => {
                if self.text.is_none() {
                    return Err("text is required for type".into());
                }
                if self.input_route.is_some() {
                    return Err("input_route is only allowed for click".into());
                }
            }
            BrowserStepAction::Click => {
                if self.text.is_some() || self.replace.is_some() {
                    return Err("text and replace are only allowed for type".into());
                }
            }
        }
        if let Some(expect) = &self.expect {
            if blank(&expect.role) || blank(&expect.name) || blank(&expect.text) {
                return Err("expect fields must be nonblank when given".into());
            }
            if expect.role.is_none() && expect.name.is_none() && expect.text.is_none() {
                return Err("expect needs a role, a name or a text".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct BrowserStepsInput {
    /// Target id from get_browser_state.
    pub target_id: String,
    /// Tab id from get_browser_state.
    pub tab_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(schema_with = "session")]
    #[uniffi(default = None)]
    pub session: Option<String>,
    /// 1 to 8 steps, run in order.
    #[schemars(schema_with = "steps")]
    pub steps: Vec<BrowserStep>,
}
impl ToolInput for BrowserStepsInput {
    const TOOL_NAME: &'static str = "browser_steps";
    fn validate(&self) -> Result<(), String> {
        if self.target_id.trim().is_empty() || self.tab_id.trim().is_empty() {
            return Err("target_id and tab_id from get_browser_state are required".into());
        }
        if !(1..=MAX_BROWSER_STEPS).contains(&self.steps.len()) {
            return Err(format!(
                "steps must contain between 1 and {MAX_BROWSER_STEPS} steps"
            ));
        }
        for (index, step) in self.steps.iter().enumerate() {
            step.validate()
                .map_err(|error| format!("step {}: {error}", index + 1))?;
        }
        Ok(())
    }
}

/// How one step ended.
///
/// - `ok`: it ran and nothing says it failed.
/// - `unconfirmed`: text was typed, but the field could not be read back as
///   holding it.
/// - `failed`: refused, errored, no single target, or its expect did not hold.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum BrowserStepStatus {
    Ok,
    Unconfirmed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct BrowserStepOutcome {
    pub status: BrowserStepStatus,
    /// The ref the step acted on: the one given, or the one its role and
    /// name resolved to.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The effect the step's tool reported: confirmed, unverifiable, partial,
    /// mismatch, refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    /// Not ok: the tool's refusal or error code, or `target_not_found`,
    /// `target_ambiguous`, `coverage_incomplete`, `expectation_unmet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// What the step's tool said: the field's read-back, or the refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Characters delivered when typing stopped part way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_count: Option<u32>,
    /// `false`: input may have reached the page. Read the page before
    /// sending the step again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// The outline lines that matched the step's role and name, when it was
    /// not exactly one (or the nearest ones when there was none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum BrowserStepsStatus {
    Completed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct BrowserStepsOutput {
    pub status: BrowserStepsStatus,
    /// The steps that ran, in order.
    pub steps: Vec<BrowserStepOutcome>,
    /// 1-based index of the step the batch stopped at: the one that failed,
    /// or the first one not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<u32>,
    /// `step_failed`, `typing_unconfirmed`, `javascript_dialog_open`,
    /// `document_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// What the page changed over the whole batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<PageChanges>,
}
impl ToolOutput for BrowserStepsOutput {}

pub fn contracts() -> Vec<ToolContract> {
    vec![ToolContract {
        name: BrowserStepsInput::TOOL_NAME.into(),
        description: "Run up to 8 page steps (click, type) on one bound tab in order, then \
            return what the page changed. A step targets a ref, or an exact role and name \
            resolved when it runs. Use role and name for an element an earlier step reveals \
            or enables (a menu option, a dialog, a disabled button): it has no ref or no \
            action until then, and a name that matches nothing only stops the batch there \
            with the candidates. So plan the whole flow as one call. Stops at the first \
            failure, unconfirmed typing, dialog or navigation; never retries. \
            Details: skill://cua-driver/BROWSER.md"
            .into(),
        platforms: vec![Platform::Macos, Platform::Windows, Platform::Linux],
        aliases: vec![],
        capabilities: vec!["browser.steps".into()],
        annotations: ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: true,
        },
        schema_mode: SchemaMode::CanonicalRuntime,
        cursor_semantics: None,
        input_schema: BrowserStepsInput::input_schema(),
        success_output_schema: Some(BrowserStepsOutput::output_schema()),
        error_output_schema: None,
        output_validator: crate::validate_typed_output::<BrowserStepsOutput>,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(steps: serde_json::Value) -> Result<BrowserStepsInput, String> {
        let input: BrowserStepsInput =
            serde_json::from_value(json!({"target_id": "bt-1", "tab_id": "tab-1", "steps": steps}))
                .map_err(|error| error.to_string())?;
        input.validate().map(|()| input)
    }

    #[test]
    fn a_step_names_its_target_by_ref_or_by_role_and_name() {
        let parsed = input(json!([
            {"action": "type", "ref": "p3:4", "text": "ada@x.com", "replace": true},
            {"action": "click", "role": "button", "name": "Role"},
            {"action": "click", "role": "button", "name": "Send invite",
             "expect": {"text": "ada@x.com (Editor)"}},
        ]))
        .unwrap();
        assert_eq!(parsed.steps[0].reference.as_deref(), Some("p3:4"));
        assert_eq!(
            parsed.steps[2].expect.as_ref().map(|e| e.present),
            Some(true)
        );
        for (steps, error) in [
            (json!([{"action": "click"}]), "ref, or with role and name"),
            (
                json!([{"action": "click", "role": "button"}]),
                "ref, or with role and name",
            ),
            (
                json!([{"action": "click", "ref": "p1:1", "role": "button", "name": "x"}]),
                "ref, or with role and name",
            ),
            (json!([{"action": "click", "ref": " "}]), "nonblank"),
            (
                json!([{"action": "type", "ref": "p1:1"}]),
                "text is required",
            ),
            (
                json!([{"action": "click", "ref": "p1:1", "text": "x"}]),
                "only allowed for type",
            ),
            (
                json!([{"action": "type", "ref": "p1:1", "text": "x", "input_route": "dom_event"}]),
                "only allowed for click",
            ),
            (
                json!([{"action": "click", "ref": "p1:1", "expect": {"present": false}}]),
                "role, a name or a text",
            ),
            (json!([]), "between 1 and 8"),
            (
                json!([{"action": "select", "ref": "p1:1"}]),
                "unknown variant",
            ),
            (
                json!([{"action": "click", "ref": "p1:1", "wait": 5}]),
                "unknown field",
            ),
        ] {
            let refused = input(steps.clone()).unwrap_err();
            assert!(refused.contains(error), "{steps}: {refused}");
        }
        let nine = json!(vec![json!({"action": "click", "ref": "p1:1"}); 9]);
        assert!(input(nine).unwrap_err().contains("between 1 and 8"));
    }

    #[test]
    fn the_input_schema_is_a_plain_object_with_bounded_steps() {
        let schema = BrowserStepsInput::input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["target_id", "tab_id", "steps"]));
        let steps = &schema["properties"]["steps"];
        assert_eq!(
            (steps["minItems"].as_u64(), steps["maxItems"].as_u64()),
            (Some(1), Some(8))
        );
        assert_eq!(
            steps["items"]["properties"]["action"]["enum"],
            json!(["click", "type"])
        );
        assert!(steps["items"]["properties"].get("ref").is_some());
    }

    #[test]
    fn the_output_leaves_absent_facts_out() {
        let output = BrowserStepsOutput {
            status: BrowserStepsStatus::Stopped,
            steps: vec![BrowserStepOutcome {
                status: BrowserStepStatus::Unconfirmed,
                reference: Some("p3:4".into()),
                effect: Some("unverifiable".into()),
                code: None,
                detail: None,
                delivered_count: None,
                retryable: Some(false),
                candidates: None,
            }],
            stopped_at: Some(1),
            stop_reason: Some("typing_unconfirmed".into()),
            changes: None,
        };
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!({"status": "stopped", "stopped_at": 1, "stop_reason": "typing_unconfirmed",
                "steps": [{"status": "unconfirmed", "ref": "p3:4", "effect": "unverifiable",
                    "retryable": false}]})
        );
    }
}
