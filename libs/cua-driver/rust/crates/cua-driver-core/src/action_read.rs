// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
//! Bounded action and observation using the existing registry boundary.
use crate::{
    protocol::{Content, ToolResult},
    tool::{current_dispatch_runtime_scope, Tool, ToolDef, ToolRegistry},
    tool_args::parse_typed_input,
};
use async_trait::async_trait;
use cua_driver_contract::{
    ActAndReadInput, ActAndReadOutput, ActionReadAction, ActionReadChildResult, ActionReadStep,
    ActionReadStepKind, ActionReadTimings, ToolInput,
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

pub struct ActAndReadTool {
    registry: Arc<Mutex<Weak<ToolRegistry>>>,
}
impl ActAndReadTool {
    pub fn new(registry: Arc<Mutex<Weak<ToolRegistry>>>) -> Self {
        Self { registry }
    }
}
/// Pause before a key step that follows another step (see the steps loop).
const KEY_STEP_SETTLE: Duration = Duration::from_millis(500);

fn millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}
fn split(result: ToolResult) -> (Vec<Content>, ActionReadChildResult) {
    (
        result.content,
        ActionReadChildResult {
            is_error: result.is_error,
            structured_content: result.structured_content,
        },
    )
}
#[async_trait]
impl Tool for ActAndReadTool {
    fn def(&self) -> &ToolDef {
        static DEF: OnceLock<ToolDef> = OnceLock::new();
        DEF.get_or_init(|| {
            ToolDef::from_contract(
                &cua_driver_contract::tool_contract("act_and_read").expect("action/read contract"),
            )
        })
    }
    async fn invoke(&self, mut args: Value) -> ToolResult {
        // Child invocation takes the public label. The registry restores the
        // trusted runtime namespace and implicit transport identity itself.
        let public_session = args
            .get("_public_session_label")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                let prefix = format!("__cua_runtime_{}:", current_dispatch_runtime_scope()?);
                args.get("session")?
                    .as_str()?
                    .strip_prefix(&prefix)
                    .map(str::to_owned)
            });
        if let Some(session) = public_session {
            args["session"] = json!(session);
        }
        let input: ActAndReadInput = match parse_typed_input("act_and_read", args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if let Err(e) = input.validate() {
            return ToolResult::error(format!("act_and_read: invalid arguments: {e}"));
        }
        let Some(registry) = self.registry.lock().unwrap().upgrade() else {
            return ToolResult::error("act_and_read registry is unavailable");
        };
        // Child calls in order: one for the single-action form, one per step otherwise.
        let base = json!({"pid":input.pid,"window_id":input.window_id});
        let steps_form = input.steps.is_some();
        let mut calls: Vec<(&'static str, Value)> = match &input.steps {
            Some(steps) => steps.iter().map(|step| step_call(&base, step)).collect(),
            None => {
                let kind = input.action.expect("validated single action");
                let mut action = base.clone();
                action["element_token"] = json!(input.element_token);
                if matches!(kind, ActionReadAction::Click | ActionReadAction::Scroll) {
                    action["delivery_mode"] = json!("background");
                }
                if let Some(value) = &input.value {
                    action["value"] = json!(value);
                }
                if let Some(direction) = input.direction {
                    action["direction"] = json!(direction);
                }
                if let Some(by) = input.by {
                    action["by"] = json!(by);
                }
                if let Some(amount) = input.amount {
                    action["amount"] = json!(amount);
                }
                vec![(kind.as_str(), action)]
            }
        };
        for name in calls
            .iter()
            .map(|(name, _)| *name)
            .chain(["get_window_state"])
        {
            if registry.get_def(name).is_none() {
                return ToolResult::error(format!(
                    "act_and_read requires registered {name}; no input dispatched"
                ));
            }
        }
        let mut observation = serde_json::to_value(input.observe).expect("observation serializes");
        observation["pid"] = json!(input.pid);
        observation["window_id"] = json!(input.window_id);
        if let Some(session) = input.session {
            for (_, call) in &mut calls {
                call["session"] = json!(session);
            }
            observation["session"] = json!(session);
        }
        let started = Instant::now();
        let mut results = Vec::with_capacity(calls.len());
        let mut stopped_at = None;
        for (i, (name, call)) in calls.into_iter().enumerate() {
            // A key step sent milliseconds after the previous step can race the
            // app's handling of it: in TextEdit, Cmd+S right after inserted text
            // sometimes neither saved nor answered for about 15 s, while a 0.5 s
            // gap saved every time. Agents calling one tool at a time always had
            // that gap.
            // ponytail: fixed settle before key steps; wait on an app-idle signal if 500 ms proves too short or too slow.
            if i > 0 && matches!(name, "press_key" | "hotkey") {
                tokio::time::sleep(KEY_STEP_SETTLE).await;
            }
            let result = registry.invoke(name, call).await;
            let failed = result.is_error == Some(true);
            results.push(split(result));
            if failed {
                // Later steps assumed this one landed. Stop; never retry.
                stopped_at = Some(i as u32 + 1);
                break;
            }
        }
        let action_ms = millis(started.elapsed());
        // A completed tool error is not evidence that input never landed.
        // Observe once, retaining the error. Never replay the action.
        let observation_started = Instant::now();
        let observation_result = registry.invoke("get_window_state", observation).await;
        let timings = ActionReadTimings {
            action_ms,
            observation_ms: millis(observation_started.elapsed()),
            total_ms: millis(started.elapsed()),
        };
        let (observation_content, observation) = split(observation_result);
        let is_error = stopped_at.is_some() || observation.is_error == Some(true);
        let mut content = vec![];
        let mut step_results = vec![];
        for (i, (child_content, child)) in results.into_iter().enumerate() {
            content.push(Content::text(if steps_form {
                format!("STEP {} RESULT (unchanged child content follows)", i + 1)
            } else {
                "ACTION RESULT (unchanged child content follows)".to_owned()
            }));
            content.extend(child_content);
            step_results.push(child);
        }
        content.push(Content::text(
            "OBSERVATION RESULT (unchanged child content follows)",
        ));
        content.extend(observation_content);
        let action = step_results
            .last()
            .expect("at least one action ran")
            .clone();
        let output = serde_json::to_value(ActAndReadOutput {
            action,
            observation,
            timings,
            steps: steps_form.then_some(step_results),
            stopped_at: stopped_at.filter(|_| steps_form),
        })
        .expect("action/read output serializes");
        if is_error {
            // Some MCP clients expose only text content on tool errors. Keep
            // the refusal and fresh observation metadata available to those
            // clients without clearing the error or replaying either child.
            content.push(Content::text(output.to_string()));
        }
        ToolResult {
            content,
            is_error: Some(is_error),
            structured_content: Some(output),
            ..Default::default()
        }
    }
}

/// Build one step's child call: the call's window plus the step's own fields,
/// which share the child tools' argument names. Every step runs in the
/// background; set_value is an AX write with no delivery_mode parameter.
fn step_call(base: &Value, step: &ActionReadStep) -> (&'static str, Value) {
    let mut call = base.clone();
    let Value::Object(fields) = serde_json::to_value(step).expect("step serializes") else {
        unreachable!("a step is a JSON object")
    };
    let object = call.as_object_mut().expect("base call is an object");
    object.extend(fields.into_iter().filter(|(name, _)| name != "action"));
    if step.action != ActionReadStepKind::SetValue {
        object.insert("delivery_mode".into(), json!("background"));
    }
    (step.action.as_str(), call)
}
