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
    ActAndReadInput, ActAndReadOutput, ActionReadAction, ActionReadChildResult, ActionReadTimings,
    ToolInput,
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
        for name in [input.action.as_str(), "get_window_state"] {
            if registry.get_def(name).is_none() {
                return ToolResult::error(format!(
                    "act_and_read requires registered {name}; no input dispatched"
                ));
            }
        }
        let mut action = json!({"pid":input.pid,"window_id":input.window_id,"element_token":input.element_token});
        if matches!(
            input.action,
            ActionReadAction::Click | ActionReadAction::Scroll
        ) {
            action["delivery_mode"] = json!("background");
        }
        if let Some(value) = input.value {
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
        let mut observation = serde_json::to_value(input.observe).expect("observation serializes");
        observation["pid"] = json!(input.pid);
        observation["window_id"] = json!(input.window_id);
        if let Some(session) = input.session {
            action["session"] = json!(session);
            observation["session"] = action["session"].clone();
        }
        let started = Instant::now();
        let action_result = registry.invoke(input.action.as_str(), action).await;
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
        let (action_content, action) = split(action_result);
        let (observation_content, observation) = split(observation_result);
        let is_error = action.is_error == Some(true) || observation.is_error == Some(true);
        let mut content = vec![Content::text(
            "ACTION RESULT (unchanged child content follows)",
        )];
        content.extend(action_content);
        content.push(Content::text(
            "OBSERVATION RESULT (unchanged child content follows)",
        ));
        content.extend(observation_content);
        let output = serde_json::to_value(ActAndReadOutput {
            action,
            observation,
            timings,
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
