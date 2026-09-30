//! `browser_steps`: up to eight page steps in one call, then one read of what
//! the page changed.
//!
//! The tool only composes. Every step is a `browser_click` or `browser_type`
//! call and every read is a `get_browser_state` call, each dispatched through
//! the registry, so each is admitted or refused exactly as it would be alone:
//! tool allowlist and policy, session namespace, exact binding, live origin,
//! the ref's declared actions, protected-resource consent. Batching adds no
//! path around any of them; a step the caller could not make alone fails here
//! too, and stops the batch.
//!
//! What stops a batch (nothing is ever retried):
//! - a step that failed or was refused, or whose target was not exactly one
//!   element;
//! - typing whose read-back did not confirm the text (the next step may be
//!   the Send button);
//! - an `expect` that does not hold;
//! - a JavaScript dialog the step opened (the page cannot be read or acted on
//!   until `browser_dialog` resolves it);
//! - a new document: later steps were planned against the old one. The batch
//!   is pinned to the document it began on, whoever replaces it: a step that
//!   reports a navigation stops the ones after it, and a named step whose
//!   read lands in another ref space is not aimed at all.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use cua_driver_contract::{
    BrowserStep, BrowserStepAction, BrowserStepExpect, BrowserStepOutcome, BrowserStepRoute,
    BrowserStepStatus, BrowserStepsInput, BrowserStepsOutput, BrowserStepsStatus, PageChanges,
    PageChangesKind, ToolInput,
};
use serde_json::{json, Value};

use crate::protocol::{Content, ToolResult};
use crate::recording_tools::ReplayRegistrySlot;
use crate::tool::{ProtectedResourceOwnership, Tool, ToolDef, ToolRegistry};
use crate::tool_args::parse_typed_input;

use super::engine::{BrowserEngine, HeldView};
use super::semantic::{parse_outline_line, OutlineLine};
use super::tools::{
    browser_protected_resource_scope, browser_resource_ownership, public_session, session_of,
};

tokio::task_local! {
    /// Set while `browser_steps` runs its steps: a step's own tool then waits
    /// for the page to settle but leaves the read to the batch's one at the
    /// end. Callers cannot set it: it is not an argument.
    static IN_STEPS_BATCH: ();
}

/// Whether the current call is a step of a `browser_steps` batch.
pub(crate) fn in_steps_batch() -> bool {
    IN_STEPS_BATCH.try_with(|_| ()).is_ok()
}

/// How long an `expect` may take to come true after its step settled.
const EXPECT_WAIT: Duration = Duration::from_secs(2);
const EXPECT_POLL: Duration = Duration::from_millis(250);
/// Room for every match of a name or text in one read.
const RESOLVE_CHARS: usize = 60_000;
const MAX_CANDIDATES: usize = 8;
const MAX_DETAIL_CHARS: usize = 300;

/// Refusals that are decided before any input is sent. A step that failed
/// any other way may have reached the page.
const NOTHING_SENT: &[&str] = &[
    "browser_ref_stale",
    "browser_action_unavailable",
    "browser_binding_stale",
    "browser_binding_ambiguous",
    "browser_tab_required",
    "browser_tab_not_found",
    "browser_wrong_target_refused",
    "browser_dialog_open",
    "browser_target_covered",
    "browser_consent_required",
    "browser_consent_revoked",
    "browser_origin_outside_scope",
    "browser_route_unavailable",
    "browser_requires_setup",
    "browser_endpoint_owner_mismatch",
    "browser_reconnect_exhausted",
];

pub struct BrowserStepsTool {
    engine: Arc<BrowserEngine>,
    registry: ReplayRegistrySlot,
}

impl BrowserStepsTool {
    pub fn new(engine: Arc<BrowserEngine>, registry: ReplayRegistrySlot) -> Self {
        Self { engine, registry }
    }
}

fn first_text(result: &ToolResult) -> Option<String> {
    result.content.iter().find_map(|content| match content {
        Content::Text { text, .. } if !text.trim().is_empty() => {
            Some(text.chars().take(MAX_DETAIL_CHARS).collect())
        }
        _ => None,
    })
}

fn failed(code: &str, detail: Option<String>) -> BrowserStepOutcome {
    BrowserStepOutcome {
        status: BrowserStepStatus::Failed,
        reference: None,
        effect: None,
        code: Some(code.to_owned()),
        detail,
        delivered_count: None,
        retryable: None,
        candidates: None,
    }
}

/// A step's tool result read as a step outcome.
pub(crate) struct Judged {
    pub(crate) outcome: BrowserStepOutcome,
    /// Why the batch stops here, when it does.
    pub(crate) stop: Option<&'static str>,
    /// What the step itself reported about the page: an open dialog or a new
    /// document (it reports nothing else inside a batch).
    pub(crate) reported: Option<PageChanges>,
}

/// Read one step's tool result, exactly as the registry returned it.
pub(crate) fn judge(action: BrowserStepAction, result: &ToolResult) -> Judged {
    let structured = result.structured_content.as_ref();
    let field = |pointer: &str| {
        structured
            .and_then(|structured| structured.pointer(pointer))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let reported: Option<PageChanges> = structured
        .and_then(|structured| structured.get("changes"))
        .and_then(|changes| serde_json::from_value(changes.clone()).ok());
    let effect = field("/effect");
    let delivered_count = structured
        .and_then(|structured| {
            structured
                .pointer("/delivery/delivered_count")
                .or_else(|| structured.get("delivered_chars"))
        })
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok());
    let mut outcome = BrowserStepOutcome {
        status: BrowserStepStatus::Ok,
        reference: None,
        effect: effect.clone(),
        code: None,
        detail: None,
        delivered_count: None,
        retryable: None,
        candidates: None,
    };

    // The registry refused the call, or the tool errored or refused.
    let refusal = field("/refusal/code");
    let error = result.is_error == Some(true);
    let refused = effect.as_deref() == Some("refused");
    let partial = effect.as_deref() == Some("partial");
    if error || refused || partial || refusal.is_some() {
        let code = refusal
            .or_else(|| field("/error/code"))
            .or_else(|| field("/code"))
            .unwrap_or_else(|| {
                if partial {
                    "browser_input_incomplete".to_owned()
                } else {
                    "tool_error".to_owned()
                }
            });
        outcome.status = BrowserStepStatus::Failed;
        outcome.detail = field("/error/hint")
            .or_else(|| field("/refusal/message"))
            .map(|detail| detail.chars().take(MAX_DETAIL_CHARS).collect())
            .or_else(|| first_text(result));
        // The registry's own refusals are decided before the tool runs.
        let nothing_sent =
            (error && field("/refusal/code").is_some()) || NOTHING_SENT.contains(&code.as_str());
        outcome.retryable = (!nothing_sent).then_some(false);
        outcome.delivered_count = delivered_count.filter(|_| !nothing_sent);
        outcome.code = Some(code);
        return Judged {
            outcome,
            stop: Some("step_failed"),
            reported,
        };
    }

    outcome.detail = field("/evidence/0/detail");
    let mut stop = None;
    // Typing that was not read back as the text is not something to build
    // the next step on: it may be the Send button.
    if action == BrowserStepAction::Type && effect.as_deref() != Some("confirmed") {
        outcome.status = BrowserStepStatus::Unconfirmed;
        outcome.retryable = Some(false);
        outcome.delivered_count = delivered_count;
        outcome.detail = outcome.detail.or_else(|| first_text(result));
        stop = Some("typing_unconfirmed");
    }
    Judged {
        outcome,
        stop,
        reported,
    }
}

/// How a `{role, name}` target resolved against one read of the page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Named {
    One(String),
    /// Not exactly one: the code, and the lines to choose from.
    Not(&'static str, Vec<String>),
}

/// The ref of the one element with exactly this role and name. `complete`
/// says the read covered every element that could match; without it a single
/// match is not proven to be the only one.
pub(crate) fn resolve_named(outline: &str, complete: bool, role: &str, name: &str) -> Named {
    let lines: Vec<OutlineLine> = outline.lines().filter_map(parse_outline_line).collect();
    let shown = |lines: Vec<&OutlineLine>| {
        lines
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|line| line.line.clone())
            .collect()
    };
    let matches: Vec<&OutlineLine> = lines
        .iter()
        .filter(|line| line.role == role && line.name.as_deref() == Some(name))
        .collect();
    match matches.as_slice() {
        [one] if complete => Named::One(one.reference.clone()),
        [_] => Named::Not("coverage_incomplete", shown(matches)),
        [] => Named::Not("target_not_found", shown(lines.iter().collect())),
        _ => Named::Not("target_ambiguous", shown(matches)),
    }
}

/// Whether `expect` holds on one read of the page. Absence is only proven by
/// a read that covered every element that could match.
pub(crate) fn expect_holds(outline: &str, complete: bool, expect: &BrowserStepExpect) -> bool {
    let found = outline.lines().filter_map(parse_outline_line).any(|line| {
        expect.role.as_ref().is_none_or(|role| &line.role == role)
            && expect
                .name
                .as_ref()
                .is_none_or(|name| line.name.as_ref() == Some(name))
            && expect.text.as_ref().is_none_or(|text| {
                [&line.name, &line.value]
                    .into_iter()
                    .flatten()
                    .any(|held| held.contains(text.as_str()))
            })
    });
    if expect.present {
        found
    } else {
        !found && complete
    }
}

/// One read of the page for a step, as `get_browser_state` answered it.
struct Read {
    outline: String,
    complete: bool,
    /// The ref space the read was recorded in (`p7`): another one means
    /// another document.
    space: Option<String>,
}

impl BrowserStepsTool {
    /// Read the page, filtered to `query`, through the registry. A read is a
    /// side read: it shares the session's refs and leaves its baseline alone.
    async fn read(
        registry: &ToolRegistry,
        base: &Value,
        query: &str,
    ) -> Result<Read, BrowserStepOutcome> {
        let mut call = base.clone();
        call["query"] = json!(query);
        call["max_chars"] = json!(RESOLVE_CHARS);
        let result = registry.invoke("get_browser_state", call).await;
        let structured = result.structured_content.as_ref();
        match structured
            .and_then(|structured| structured.get("outline"))
            .and_then(Value::as_str)
        {
            Some(outline) if result.is_error != Some(true) => Ok(Read {
                outline: outline.to_owned(),
                complete: structured
                    .and_then(|structured| structured.pointer("/snapshot/complete"))
                    .and_then(Value::as_bool)
                    == Some(true),
                space: structured
                    .and_then(|structured| structured.pointer("/snapshot/id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            // The read was refused or failed: the step cannot be aimed.
            _ => Err(failed(
                structured
                    .and_then(|structured| {
                        structured
                            .pointer("/refusal/code")
                            .or_else(|| structured.get("code"))
                    })
                    .and_then(Value::as_str)
                    .unwrap_or("observation_failed"),
                structured
                    .and_then(|structured| structured.pointer("/refusal/message"))
                    .and_then(Value::as_str)
                    .map(|detail| detail.chars().take(MAX_DETAIL_CHARS).collect())
                    .or_else(|| first_text(&result)),
            )),
        }
    }

    /// Wait, within bounds, for `expect` to hold. `Ok` carries the ref space
    /// of the read that showed it.
    async fn expect(
        registry: &ToolRegistry,
        base: &Value,
        expect: &BrowserStepExpect,
    ) -> Result<Option<String>, BrowserStepOutcome> {
        let query = expect
            .text
            .as_deref()
            .or(expect.name.as_deref())
            .or(expect.role.as_deref())
            .unwrap_or_default();
        let started = tokio::time::Instant::now();
        loop {
            let read = Self::read(registry, base, query).await?;
            if expect_holds(&read.outline, read.complete, expect) {
                return Ok(read.space);
            }
            if started.elapsed() >= EXPECT_WAIT {
                return Err(failed(
                    "expectation_unmet",
                    Some(if expect.present {
                        "no element matched expect".to_owned()
                    } else if read.complete {
                        "an element still matches expect".to_owned()
                    } else {
                        "the page was not read completely, so absence is not proven".to_owned()
                    }),
                ));
            }
            tokio::time::sleep(EXPECT_POLL).await;
        }
    }

    /// `document` is the ref space the session held when the batch began.
    async fn run(
        &self,
        registry: &ToolRegistry,
        input: &BrowserStepsInput,
        held: HeldView,
        mut document: Option<String>,
    ) -> BrowserStepsOutput {
        let mut base = json!({ "target_id": input.target_id, "tab_id": input.tab_id });
        if let Some(session) = &input.session {
            base["session"] = json!(session);
        }
        let mut outcomes = Vec::with_capacity(input.steps.len());
        let mut stopped: Option<(u32, &'static str)> = None;
        let mut reported: Option<PageChanges> = None;

        for (index, step) in input.steps.iter().enumerate() {
            let number = index as u32 + 1;
            let last = index + 1 == input.steps.len();
            // Aim: the ref given, or the one element with this role and name
            // on the page as it is now.
            let reference = match (&step.reference, &step.role, &step.name) {
                (Some(reference), _, _) => reference.clone(),
                (None, Some(role), Some(name)) => {
                    let read = match Self::read(registry, &base, name).await {
                        Ok(read) => read,
                        Err(outcome) => {
                            outcomes.push(outcome);
                            stopped = Some((number, "step_failed"));
                            break;
                        }
                    };
                    // The batch is pinned to the document it began on. If
                    // anything replaced it meanwhile (another session, the
                    // page itself), a name found on the new page is not the
                    // element this step was written for. A ref needs no such
                    // check here: its own document is proven when it is used.
                    match (&document, &read.space) {
                        (Some(began), Some(now)) if began != now => {
                            stopped = Some((number, "document_changed"));
                            break;
                        }
                        (None, Some(now)) => document = Some(now.clone()),
                        _ => {}
                    }
                    let resolved = resolve_named(&read.outline, read.complete, role, name);
                    match resolved {
                        Named::One(reference) => reference,
                        Named::Not(code, candidates) => {
                            let mut outcome = failed(code, None);
                            outcome.candidates = Some(candidates);
                            outcomes.push(outcome);
                            stopped = Some((number, "step_failed"));
                            break;
                        }
                    }
                }
                _ => unreachable!("validated: a ref, or a role and a name"),
            };

            let result = registry
                .invoke(step.action.tool(), step_call(&base, step, &reference))
                .await;
            let Judged {
                mut outcome,
                mut stop,
                reported: step_reported,
            } = judge(step.action, &result);
            outcome.reference = Some(reference);

            // What the step reported about the page decides whether anything
            // after it can run.
            let dialog = step_reported
                .as_ref()
                .is_some_and(|changes| changes.dialog.is_some());
            let mut new_document = step_reported
                .as_ref()
                .is_some_and(|changes| changes.reason.as_deref() == Some("document_changed"));
            if dialog {
                reported = step_reported;
            }
            if stop.is_none() && dialog && (!last || step.expect.is_some()) {
                stop = Some("javascript_dialog_open");
            }
            if stop.is_none() && !dialog {
                if let Some(expect) = &step.expect {
                    match Self::expect(registry, &base, expect).await {
                        // An expect may be about the page the step led to.
                        Ok(space) => {
                            new_document |= matches!(
                                (&document, &space),
                                (Some(began), Some(now)) if began != now
                            );
                            if document.is_none() {
                                document = space;
                            }
                        }
                        Err(unmet) => {
                            outcome.status = BrowserStepStatus::Failed;
                            outcome.code = unmet.code;
                            outcome.detail = unmet.detail;
                            stop = Some("step_failed");
                        }
                    }
                }
            }
            outcomes.push(outcome);
            if let Some(reason) = stop {
                stopped = Some((number, reason));
                break;
            }
            // Later steps were planned against the document that was there.
            if new_document && !last {
                stopped = Some((number + 1, "document_changed"));
                break;
            }
        }

        // One read for the whole batch, against what the session held before
        // it. A dialog that is still up has already said the page is closed.
        // A session working from a dom_refs_v1 snapshot keeps it: a semantic
        // read would end its refs.
        let changes = match reported {
            Some(dialog) => Some(dialog),
            None if held == HeldView::DomRefs => None,
            None => final_changes(registry, &base, held).await,
        };
        BrowserStepsOutput {
            status: if stopped.is_some() {
                BrowserStepsStatus::Stopped
            } else {
                BrowserStepsStatus::Completed
            },
            steps: outcomes,
            stopped_at: stopped.map(|(step, _)| step),
            stop_reason: stopped.map(|(_, reason)| reason.to_owned()),
            changes,
        }
    }
}

/// One step's own tool call: the batch's tab, the step's ref and its fields.
fn step_call(base: &Value, step: &BrowserStep, reference: &str) -> Value {
    let mut call = base.clone();
    call["ref"] = json!(reference);
    match step.action {
        BrowserStepAction::Type => {
            call["text"] = json!(step.text);
            if let Some(replace) = step.replace {
                call["replace"] = json!(replace);
            }
        }
        BrowserStepAction::Click => {
            if let Some(route) = step.input_route {
                call["input_route"] = json!(match route {
                    BrowserStepRoute::Trusted => "trusted",
                    BrowserStepRoute::DomEvent => "dom_event",
                });
            }
        }
    }
    call
}

/// The batch's one read: what changed since the session's baseline.
async fn final_changes(
    registry: &ToolRegistry,
    base: &Value,
    held: HeldView,
) -> Option<PageChanges> {
    let mut read = base.clone();
    read["since_revision"] = json!(match held {
        HeldView::Semantic(revision) => revision,
        _ => 0,
    });
    let result = registry.invoke("get_browser_state", read).await;
    let structured = result.structured_content.unwrap_or(Value::Null);
    if let Some(changes) = structured
        .get("changes")
        .and_then(|changes| serde_json::from_value::<PageChanges>(changes.clone()).ok())
    {
        return Some(changes);
    }
    // The steps stand; only the read after them was refused or failed.
    Some(PageChanges {
        kind: PageChangesKind::Unavailable,
        reason: Some(
            structured
                .pointer("/refusal/code")
                .or_else(|| structured.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("observation_failed")
                .to_owned(),
        ),
        snapshot_id: None,
        base_revision: None,
        revision: None,
        ops: None,
        outline: None,
        url: None,
        title: None,
        complete: None,
        continuation: None,
        dialog: None,
        settled: None,
    })
}

#[async_trait]
impl Tool for BrowserStepsTool {
    fn def(&self) -> &ToolDef {
        static DEF: OnceLock<ToolDef> = OnceLock::new();
        DEF.get_or_init(|| {
            ToolDef::from_contract(
                &cua_driver_contract::tool_contract("browser_steps")
                    .expect("browser_steps contract"),
            )
        })
    }

    // The batch as a whole is tab input on this exact tab; each step is
    // admitted again as its own tool.
    async fn protected_resource_ownership(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> ProtectedResourceOwnership {
        if adapter_id == "browser_bound_input" {
            browser_resource_ownership(&self.engine, args)
        } else {
            ProtectedResourceOwnership::UserOwned
        }
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id == "browser_bound_input" {
            browser_protected_resource_scope(&self.engine, args, "browser_steps").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, mut args: Value) -> ToolResult {
        // The session's own key names what it holds; its public label is
        // what child calls carry (the registry maps it back).
        let runtime_session = session_of(&args);
        match public_session(&args) {
            Some(session) => args["session"] = json!(session),
            None => {
                if let Some(arguments) = args.as_object_mut() {
                    arguments.remove("session");
                }
            }
        }
        let input: BrowserStepsInput = match parse_typed_input("browser_steps", args) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if let Err(error) = input.validate() {
            return ToolResult::error(format!("browser_steps: invalid arguments: {error}"));
        }
        let Some(registry) = self.registry.lock().unwrap().upgrade() else {
            return ToolResult::error("browser_steps registry is unavailable");
        };
        for name in ["browser_click", "browser_type", "get_browser_state"] {
            if registry.get_def(name).is_none() {
                return ToolResult::error(format!(
                    "browser_steps requires registered {name}; no input dispatched"
                ));
            }
        }
        let held = self
            .engine
            .held_view(&runtime_session, &input.target_id, &input.tab_id);
        let document = self
            .engine
            .held_space(&runtime_session, &input.target_id, &input.tab_id);
        let output = IN_STEPS_BATCH
            .scope((), self.run(&registry, &input, held, document))
            .await;
        let ran = output.steps.len();
        let summary = match (&output.stopped_at, &output.stop_reason) {
            (Some(step), Some(reason)) => format!(
                "browser_steps stopped at step {step} of {} ({reason}); {ran} step(s) ran",
                input.steps.len()
            ),
            _ => format!("browser_steps ran {ran} of {} step(s)", input.steps.len()),
        };
        ToolResult::text(summary)
            .with_structured(serde_json::to_value(output).expect("browser_steps output serializes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(structured: Value) -> ToolResult {
        ToolResult::text("the tool's own words").with_structured(structured)
    }

    #[test]
    fn a_step_result_is_read_without_adding_to_what_the_tool_said() {
        use BrowserStepAction::{Click, Type};
        use BrowserStepStatus::{Failed, Ok as Fine, Unconfirmed};
        let error = |structured: Value| ToolResult {
            is_error: Some(true),
            ..result(structured)
        };
        // (what ran, its result, status, code, retryable, delivered, stop)
        let table = [
            (
                "a click reports no proof and goes on",
                Click,
                result(json!({"effect": "unverifiable", "route": "trusted_input"})),
                Fine,
                None,
                None,
                None,
                None,
            ),
            (
                "typing read back",
                Type,
                result(
                    json!({"effect": "confirmed", "evidence": [{"kind": "value_readback", "detail": "the field holds \"ada\""}]}),
                ),
                Fine,
                None,
                None,
                None,
                None,
            ),
            (
                "typing not read back stops the batch",
                Type,
                result(
                    json!({"effect": "unverifiable", "delivery": {"mode": "background", "delivered_count": 3}}),
                ),
                Unconfirmed,
                None,
                Some(false),
                Some(3),
                Some("typing_unconfirmed"),
            ),
            (
                "a field that rejected the text",
                Type,
                error(
                    json!({"code": "browser_type_mismatch", "effect": "mismatch", "delivered_chars": 6, "value": "1234"}),
                ),
                Failed,
                Some("browser_type_mismatch"),
                Some(false),
                Some(6),
                Some("step_failed"),
            ),
            (
                "typing that stopped part way",
                Type,
                result(
                    json!({"effect": "partial", "delivery": {"mode": "background", "delivered_count": 2}}),
                ),
                Failed,
                Some("browser_input_incomplete"),
                Some(false),
                Some(2),
                Some("step_failed"),
            ),
            (
                "a stale ref sent nothing",
                Click,
                result(
                    json!({"effect": "refused", "error": {"code": "browser_ref_stale", "hint": "snapshot again"}}),
                ),
                Failed,
                Some("browser_ref_stale"),
                None,
                None,
                Some("step_failed"),
            ),
            (
                "a refusal after delivery is not retryable",
                Click,
                result(
                    json!({"effect": "refused", "error": {"code": "browser_input_trust_unavailable", "hint": "delivery is unknown"}}),
                ),
                Failed,
                Some("browser_input_trust_unavailable"),
                Some(false),
                None,
                Some("step_failed"),
            ),
            (
                "the registry refused the step's tool",
                Type,
                error(
                    json!({"status": "refused", "refusal": {"code": "permission_denied", "message": "browser_type is not allowed"}}),
                ),
                Failed,
                Some("permission_denied"),
                None,
                None,
                Some("step_failed"),
            ),
            (
                "a tool error with no structure",
                Click,
                ToolResult::error("DOM click failed: socket closed"),
                Failed,
                Some("tool_error"),
                Some(false),
                None,
                Some("step_failed"),
            ),
        ];
        for (case, action, result, status, code, retryable, delivered, stop) in table {
            let judged = judge(action, &result);
            assert_eq!(judged.outcome.status, status, "{case}");
            assert_eq!(judged.outcome.code.as_deref(), code, "{case}");
            assert_eq!(judged.outcome.retryable, retryable, "{case}");
            assert_eq!(judged.outcome.delivered_count, delivered, "{case}");
            assert_eq!(judged.stop, stop, "{case}");
        }
        let refused = judge(
            Click,
            &result(json!({"effect": "refused",
            "error": {"code": "browser_ref_stale", "hint": "snapshot again"}})),
        );
        assert_eq!(refused.outcome.detail.as_deref(), Some("snapshot again"));
        let confirmed = judge(
            Type,
            &result(json!({"effect": "confirmed",
            "evidence": [{"kind": "value_readback", "detail": "the field holds \"ada\""}]})),
        );
        assert_eq!(
            confirmed.outcome.detail.as_deref(),
            Some("the field holds \"ada\"")
        );
    }

    #[test]
    fn a_step_passes_on_the_dialog_or_the_new_document_it_reported() {
        let dialog = judge(
            BrowserStepAction::Click,
            &result(
                json!({"effect": "unverifiable", "changes": {"kind": "unavailable",
                "reason": "javascript_dialog_open",
                "dialog": {"dialog_id": "dialog-4", "kind": "confirm"}}}),
            ),
        );
        assert_eq!(
            dialog.outcome.status,
            BrowserStepStatus::Ok,
            "the click itself landed"
        );
        assert_eq!(
            dialog.reported.unwrap().dialog.unwrap().dialog_id,
            "dialog-4"
        );
        let navigated = judge(
            BrowserStepAction::Click,
            &result(json!({"effect": "unverifiable",
                "changes": {"kind": "unavailable", "reason": "document_changed"}})),
        );
        assert_eq!(
            navigated.reported.unwrap().reason.as_deref(),
            Some("document_changed")
        );
    }

    const PAGE: &str = "- textbox \"Email\" [p3:1 type]\n\
        - button \"Role\" [p3:2 click] (expanded)\n\
        \x20 - option \"Viewer\" [p3:7 click] (selected)\n\
        \x20 - option \"Editor\" [p3:8 click]\n\
        - button \"Remove\" [p3:10 click]\n\
        - button \"Remove\" [p3:11 click] (iframe)\n\
        - link \"Remove\" [p3:12 click] -> \"https://x.test/remove\"\n\
        - status \"state\" [p3:13]\n\
        \x20 - statictext \"ada@x.com (Editor)\" [p3:14]\n\
        - combobox \"Region\" [p3:15 click] = \"Europe\" (collapsed)";

    #[test]
    fn a_named_target_is_exactly_one_element_with_that_role_and_name() {
        assert_eq!(
            resolve_named(PAGE, true, "option", "Editor"),
            Named::One("p3:8".into())
        );
        // The role is part of the match: the link named Remove is not a button.
        assert_eq!(
            resolve_named(PAGE, true, "link", "Remove"),
            Named::One("p3:12".into())
        );
        // Exact, not a prefix, not another case.
        for (role, name) in [("option", "Edit"), ("option", "editor"), ("tab", "Editor")] {
            let Named::Not(code, candidates) = resolve_named(PAGE, true, role, name) else {
                panic!("{role} {name:?} must not resolve")
            };
            assert_eq!(code, "target_not_found");
            assert_eq!(candidates.len(), MAX_CANDIDATES.min(PAGE.lines().count()));
        }
    }

    #[test]
    fn an_ambiguous_target_fails_with_the_candidates_and_is_never_guessed() {
        let Named::Not(code, candidates) = resolve_named(PAGE, true, "button", "Remove") else {
            panic!("two buttons named Remove must not resolve")
        };
        assert_eq!(code, "target_ambiguous");
        assert_eq!(
            candidates,
            vec![
                "- button \"Remove\" [p3:10 click]".to_owned(),
                "- button \"Remove\" [p3:11 click] (iframe)".to_owned(),
            ],
            "each carries its ref, across frames, to choose from"
        );
    }

    #[test]
    fn one_match_in_a_partly_read_page_is_not_proven_unique() {
        let Named::Not(code, candidates) = resolve_named(PAGE, false, "option", "Editor") else {
            panic!("uniqueness needs the whole page")
        };
        assert_eq!(code, "coverage_incomplete");
        assert_eq!(
            candidates,
            vec!["- option \"Editor\" [p3:8 click]".to_owned()]
        );
    }

    #[test]
    fn expect_matches_role_exact_name_and_contained_text() {
        let expect = |role: Option<&str>, name: Option<&str>, text: Option<&str>, present: bool| {
            BrowserStepExpect {
                role: role.map(str::to_owned),
                name: name.map(str::to_owned),
                text: text.map(str::to_owned),
                present,
            }
        };
        for (case, predicate, complete, holds) in [
            (
                "text in a name",
                expect(None, None, Some("(Editor)"), true),
                true,
                true,
            ),
            (
                "text in a value",
                expect(Some("combobox"), None, Some("Europe"), true),
                true,
                true,
            ),
            (
                "exact name",
                expect(Some("option"), Some("Editor"), None, true),
                true,
                true,
            ),
            (
                "name is exact, not contained",
                expect(None, Some("Edit"), None, true),
                true,
                false,
            ),
            (
                "role must agree",
                expect(Some("button"), Some("Editor"), None, true),
                true,
                false,
            ),
            (
                "absent",
                expect(None, None, Some("ada@y.com"), false),
                true,
                true,
            ),
            (
                "still there",
                expect(None, None, Some("ada@x.com"), false),
                true,
                false,
            ),
            (
                "absence needs the whole page",
                expect(None, None, Some("ada@y.com"), false),
                false,
                false,
            ),
            (
                "presence does not",
                expect(None, None, Some("ada@x.com"), true),
                false,
                true,
            ),
        ] {
            assert_eq!(expect_holds(PAGE, complete, &predicate), holds, "{case}");
        }
    }
}
