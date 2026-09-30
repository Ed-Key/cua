//! `browser_steps` through the real registry. Only the page boundary is a
//! double: `browser_click`, `browser_type` and `get_browser_state` answer
//! from a script and log what they were asked, so each test reads off exactly
//! which calls a batch made and which it did not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::authorization::PermissionMode;
use crate::protocol::ToolResult;
use crate::session_authorization::{
    EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
};
use crate::tool::{Tool, ToolDef, ToolRegistry};

use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use super::steps::BrowserStepsTool;

const PAGE: &str = "- textbox \"Email\" [p3:1 type]\n\
    - button \"Role\" [p3:2 click] (collapsed)\n\
    - option \"Viewer\" [p3:7 click] (selected)\n\
    - option \"Editor\" [p3:8 click]\n\
    - button \"Send invite\" [p3:6 click]\n\
    - button \"Remove\" [p3:10 click]\n\
    - button \"Remove\" [p3:11 click]\n\
    - statictext \"ada@x.com (Editor)\" [p3:14]";

/// What the page does for one ref.
#[derive(Clone)]
enum Acts {
    /// A refusal from the step's own tool, before any input.
    Refuses(BrowserRefusalCode),
    /// Typed, but the field was not read back as holding the text.
    Unverifiable,
    /// The click opened a JavaScript dialog.
    OpensDialog,
    /// The click replaced the document.
    Navigates,
}

#[derive(Default)]
struct Script {
    /// Every call the doubles received: (tool, arguments).
    log: Vec<(String, Value)>,
    acts: HashMap<String, Acts>,
    /// The outline reads return; `None` refuses them.
    outline: Option<String>,
    complete: bool,
    /// The ref space reads report, and the one they report once any input
    /// tool has been called (another session loaded another page meanwhile).
    space: String,
    space_after_input: Option<String>,
}

type Shared = Arc<Mutex<Script>>;

struct PageDouble {
    def: ToolDef,
    script: Shared,
}

#[async_trait]
impl Tool for PageDouble {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let mut script = self.script.lock().unwrap();
        script.log.push((self.def.name.clone(), args.clone()));
        if self.def.name == "get_browser_state" {
            let Some(outline) = script.outline.clone() else {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserOriginOutsideScope,
                    "outside scope",
                )
                .to_tool_result();
            };
            return match args.get("since_revision") {
                Some(since) => ToolResult::text("changes").with_structured(json!({
                    "status": "ok", "mode": "changes",
                    "changes": {"kind": "diff", "snapshot_id": "p3", "base_revision": since,
                        "revision": 9, "ops": []},
                })),
                None => ToolResult::text("snapshot").with_structured(json!({
                    "status": "ok", "mode": "snapshot", "outline": outline,
                    "snapshot": {"id": script.space, "complete": script.complete},
                })),
            };
        }
        let reference = args["ref"].as_str().unwrap_or_default().to_owned();
        let typing = self.def.name == "browser_type";
        if let Some(space) = script.space_after_input.take() {
            script.space = space;
        }
        match script.acts.get(&reference).cloned() {
            Some(Acts::Refuses(code)) => BrowserRefusal::new(code, "refused by the page double").to_tool_result(),
            Some(Acts::Unverifiable) => ToolResult::text("typed; not read back").with_structured(json!({
                "status": "ok", "effect": "unverifiable", "mode": "insert_text",
                "requested_chars": 5, "delivered_chars": 5, "readback": "element_replaced",
            })),
            Some(Acts::OpensDialog) => ToolResult::text("clicked").with_structured(json!({
                "status": "ok", "route": "trusted",
                "changes": {"kind": "unavailable", "reason": "javascript_dialog_open",
                    "dialog": {"dialog_id": "dialog-5", "kind": "confirm"}},
            })),
            Some(Acts::Navigates) => ToolResult::text("clicked").with_structured(json!({
                "status": "ok", "route": "trusted",
                "changes": {"kind": "unavailable", "reason": "document_changed"},
            })),
            None if typing => ToolResult::text("typed").with_structured(json!({
                "status": "ok", "effect": "confirmed", "mode": "insert_text",
                "evidence": [{"kind": "browser_readback", "detail": "the field holds \"ada@x.com\""}],
                "requested_chars": 9, "delivered_chars": 9,
            })),
            None => ToolResult::text("clicked").with_structured(json!({"status": "ok", "route": "trusted"})),
        }
    }
}

fn page_registry() -> (Arc<ToolRegistry>, Shared) {
    let script = Arc::new(Mutex::new(Script {
        outline: Some(PAGE.to_owned()),
        complete: true,
        space: "p3".to_owned(),
        ..Default::default()
    }));
    let mut registry = ToolRegistry::new();
    for name in ["browser_click", "browser_type", "get_browser_state"] {
        registry.register(Box::new(PageDouble {
            def: ToolDef {
                name: name.into(),
                description: "page boundary double".into(),
                input_schema: json!({"type": "object"}),
                read_only: name == "get_browser_state",
                destructive: false,
                idempotent: false,
                open_world: true,
            },
            script: script.clone(),
        }));
    }
    registry.register(Box::new(BrowserStepsTool::new(
        super::tools::tests::engine(),
        registry.composite_registry_slot(),
    )));
    registry.register_session_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    (registry, script)
}

fn unrestricted() -> Arc<EffectiveAuthorizationContext> {
    SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Unrestricted],
            true,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(PermissionMode::Unrestricted, None)
    .unwrap()
}

async fn steps(registry: &ToolRegistry, session: &str, steps: Value) -> Value {
    let result = registry
        .invoke_with_context(
            "browser_steps",
            json!({"target_id": "bt-1", "tab_id": "tab-1", "session": session, "steps": steps}),
            unrestricted(),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.expect("structured output")
}

/// The calls the doubles saw, as `tool ref-or-query`.
fn calls(script: &Shared) -> Vec<String> {
    script
        .lock()
        .unwrap()
        .log
        .iter()
        .map(|(tool, args)| {
            let what = args
                .get("ref")
                .or_else(|| args.get("query"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    args.get("since_revision")
                        .map(|since| format!("since {since}"))
                })
                .unwrap_or_default();
            format!("{tool} {what}")
        })
        .collect()
}

#[tokio::test]
async fn every_step_is_its_own_tool_call_in_order_and_the_page_is_read_once_at_the_end() {
    let (registry, script) = page_registry();
    let output = steps(
        &registry,
        "steps-order",
        json!([
            {"action": "type", "ref": "p3:1", "text": "ada@x.com", "replace": true},
            {"action": "click", "role": "button", "name": "Role"},
            {"action": "click", "role": "option", "name": "Editor"},
            {"action": "click", "ref": "p3:6", "input_route": "dom_event",
             "expect": {"text": "ada@x.com (Editor)"}},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        [
            "browser_type p3:1",
            "get_browser_state Role",
            "browser_click p3:2",
            "get_browser_state Editor",
            "browser_click p3:8",
            "browser_click p3:6",
            "get_browser_state ada@x.com (Editor)",
            "get_browser_state since 0",
        ]
    );
    assert_eq!(output["status"], "completed", "{output}");
    assert!(output.get("stopped_at").is_none() && output.get("stop_reason").is_none());
    let outcomes = output["steps"].as_array().unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(
        outcomes.iter().all(|outcome| outcome["status"] == "ok"),
        "{output}"
    );
    assert_eq!(outcomes[0]["effect"], "confirmed");
    assert_eq!(outcomes[0]["detail"], "the field holds \"ada@x.com\"");
    // A named target reports the ref it resolved to.
    assert_eq!(outcomes[2]["ref"], "p3:8");
    assert_eq!(
        output["changes"]["kind"], "diff",
        "one diff for the whole batch"
    );

    // Each child call is the caller's own: its session label (mapped into the
    // runtime's namespace by the registry), its tab, and only its step's
    // fields.
    let script = script.lock().unwrap();
    for (tool, args) in &script.log {
        assert_eq!(args["_public_session_label"], "steps-order", "{tool}");
        assert_eq!(
            (&args["target_id"], &args["tab_id"]),
            (&json!("bt-1"), &json!("tab-1"))
        );
    }
    assert_eq!(script.log[0].1["text"], "ada@x.com");
    assert_eq!(script.log[0].1["replace"], true);
    assert!(script.log[2].1.get("text").is_none() && script.log[2].1.get("input_route").is_none());
    assert_eq!(script.log[5].1["input_route"], "dom_event");
    // Reads for aiming ask for room to see every match.
    assert_eq!(script.log[1].1["max_chars"], 60_000);
}

#[tokio::test]
async fn a_batch_stops_at_the_first_failure_and_still_reads_the_page() {
    let (registry, script) = page_registry();
    script.lock().unwrap().acts.insert(
        "p3:2".into(),
        Acts::Refuses(BrowserRefusalCode::BrowserRefStale),
    );
    let output = steps(
        &registry,
        "steps-failure",
        json!([
            {"action": "type", "ref": "p3:1", "text": "ada@x.com"},
            {"action": "click", "ref": "p3:2"},
            {"action": "click", "ref": "p3:6"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        [
            "browser_type p3:1",
            "browser_click p3:2",
            "get_browser_state since 0"
        ],
        "the third step was never sent, and nothing was retried"
    );
    assert_eq!(output["status"], "stopped");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("step_failed"))
    );
    let outcomes = output["steps"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2, "only the steps that ran");
    assert_eq!(outcomes[1]["status"], "failed");
    assert_eq!(outcomes[1]["code"], "browser_ref_stale");
    assert_eq!(outcomes[1]["effect"], "refused");
    assert!(
        outcomes[1].get("retryable").is_none(),
        "a stale ref sent nothing"
    );
    assert_eq!(
        output["changes"]["kind"], "diff",
        "what the first step changed is still told"
    );
}

#[tokio::test]
async fn typing_that_was_not_confirmed_stops_the_batch_before_the_next_step() {
    let (registry, script) = page_registry();
    script
        .lock()
        .unwrap()
        .acts
        .insert("p3:1".into(), Acts::Unverifiable);
    let output = steps(
        &registry,
        "steps-unconfirmed",
        json!([
            {"action": "type", "ref": "p3:1", "text": "ada@x.com"},
            {"action": "click", "role": "button", "name": "Send invite"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        ["browser_type p3:1", "get_browser_state since 0"],
        "Send was neither aimed nor pressed on text nobody confirmed"
    );
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(1), &json!("typing_unconfirmed"))
    );
    let typed = &output["steps"][0];
    assert_eq!(typed["status"], "unconfirmed");
    assert_eq!(typed["effect"], "unverifiable");
    assert_eq!(typed["retryable"], false, "the text may be in the page");
}

#[tokio::test]
async fn a_target_that_is_not_exactly_one_element_fails_with_candidates_and_sends_nothing() {
    let (registry, script) = page_registry();
    let ambiguous = steps(
        &registry,
        "steps-ambiguous",
        json!([
            {"action": "click", "role": "button", "name": "Remove"},
            {"action": "click", "ref": "p3:6"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        ["get_browser_state Remove", "get_browser_state since 0"]
    );
    assert_eq!(
        (&ambiguous["stopped_at"], &ambiguous["stop_reason"]),
        (&json!(1), &json!("step_failed"))
    );
    assert_eq!(ambiguous["steps"][0]["code"], "target_ambiguous");
    assert_eq!(
        ambiguous["steps"][0]["candidates"],
        json!([
            "- button \"Remove\" [p3:10 click]",
            "- button \"Remove\" [p3:11 click]"
        ])
    );
    assert!(
        ambiguous["steps"][0].get("ref").is_none(),
        "nothing was chosen"
    );

    let (registry, script) = page_registry();
    let missing = steps(
        &registry,
        "steps-missing",
        json!([{"action": "click", "role": "button", "name": "Delete"}]),
    )
    .await;
    assert_eq!(
        calls(&script),
        ["get_browser_state Delete", "get_browser_state since 0"]
    );
    assert_eq!(missing["steps"][0]["code"], "target_not_found");

    // One match on a page that was not read completely is not proven unique.
    let (registry, script) = page_registry();
    script.lock().unwrap().complete = false;
    let partial = steps(
        &registry,
        "steps-partial",
        json!([{"action": "click", "role": "option", "name": "Editor"}]),
    )
    .await;
    assert_eq!(partial["steps"][0]["code"], "coverage_incomplete");
    assert!(calls(&script)
        .iter()
        .all(|call| !call.starts_with("browser_click")));
}

#[tokio::test]
async fn a_dialog_a_step_opened_stops_the_batch_and_is_passed_on_with_its_capability() {
    let (registry, script) = page_registry();
    script
        .lock()
        .unwrap()
        .acts
        .insert("p3:10".into(), Acts::OpensDialog);
    let output = steps(
        &registry,
        "steps-dialog",
        json!([
            {"action": "click", "ref": "p3:10"},
            {"action": "click", "ref": "p3:6"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        ["browser_click p3:10"],
        "no later step, and no read of a page that cannot answer"
    );
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(1), &json!("javascript_dialog_open"))
    );
    assert_eq!(
        output["steps"][0]["status"], "ok",
        "the click itself landed"
    );
    assert_eq!(output["changes"]["kind"], "unavailable");
    assert_eq!(
        output["changes"]["dialog"],
        json!({"dialog_id": "dialog-5", "kind": "confirm"})
    );
}

#[tokio::test]
async fn a_new_document_stops_the_steps_planned_against_the_old_one() {
    let (registry, script) = page_registry();
    script
        .lock()
        .unwrap()
        .acts
        .insert("p3:2".into(), Acts::Navigates);
    let output = steps(
        &registry,
        "steps-navigation",
        json!([
            {"action": "click", "ref": "p3:2"},
            {"action": "click", "role": "button", "name": "Send invite"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        ["browser_click p3:2", "get_browser_state since 0"],
        "the second step was not even aimed at the new page"
    );
    assert_eq!(output["status"], "stopped");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("document_changed"))
    );
    assert_eq!(output["steps"].as_array().unwrap().len(), 1);

    // As the last step, a navigation is simply how the batch ended.
    let (registry, script) = page_registry();
    script
        .lock()
        .unwrap()
        .acts
        .insert("p3:2".into(), Acts::Navigates);
    let output = steps(
        &registry,
        "steps-navigation-last",
        json!([{"action": "click", "ref": "p3:2"}]),
    )
    .await;
    assert_eq!(output["status"], "completed", "{output}");
}

#[tokio::test]
async fn a_named_step_is_not_aimed_at_a_document_the_batch_did_not_begin_on() {
    // Nothing the batch did navigated: between two steps another session (or
    // the page itself) loaded a page that has a button of the same name.
    let (registry, script) = page_registry();
    script.lock().unwrap().space_after_input = Some("p9".into());
    let output = steps(
        &registry,
        "steps-replaced",
        json!([
            {"action": "click", "role": "button", "name": "Role"},
            {"action": "click", "role": "button", "name": "Send invite"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        [
            "get_browser_state Role",
            "browser_click p3:2",
            "get_browser_state Send invite",
            "get_browser_state since 0",
        ],
        "Send invite was found on the new page and not clicked"
    );
    assert_eq!(output["status"], "stopped");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("document_changed"))
    );
    assert_eq!(output["steps"].as_array().unwrap().len(), 1);

    // An expect may look at the page a step led to; the steps after it may not.
    let (registry, script) = page_registry();
    script.lock().unwrap().space_after_input = Some("p9".into());
    let output = steps(
        &registry,
        "steps-replaced-expect",
        json!([
            {"action": "click", "role": "button", "name": "Role",
             "expect": {"text": "ada@x.com (Editor)"}},
            {"action": "click", "ref": "p3:6"},
        ]),
    )
    .await;
    assert_eq!(output["steps"][0]["status"], "ok", "{output}");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("document_changed"))
    );
    assert!(!calls(&script).contains(&"browser_click p3:6".to_owned()));
}

#[tokio::test]
async fn an_expectation_that_does_not_hold_stops_the_batch() {
    let (registry, script) = page_registry();
    let output = steps(
        &registry,
        "steps-expect",
        json!([
            {"action": "click", "ref": "p3:6", "expect": {"text": "grace@x.com (Viewer)"}},
            {"action": "click", "ref": "p3:10"},
        ]),
    )
    .await;
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(1), &json!("step_failed"))
    );
    assert_eq!(output["steps"][0]["status"], "failed");
    assert_eq!(output["steps"][0]["code"], "expectation_unmet");
    let calls = calls(&script);
    assert_eq!(
        calls.first().map(String::as_str),
        Some("browser_click p3:6")
    );
    assert!(!calls.contains(&"browser_click p3:10".to_owned()));
    assert!(
        calls
            .iter()
            .filter(|call| call.contains("grace@x.com"))
            .count()
            > 1,
        "the page was given time to get there: {calls:?}"
    );

    // Absence: true once the text is gone, and only on a page read completely.
    let (registry, script) = page_registry();
    let gone = steps(
        &registry,
        "steps-expect-absent",
        json!([{"action": "click", "ref": "p3:6", "expect": {"text": "grace@x.com", "present": false}}]),
    )
    .await;
    assert_eq!(gone["status"], "completed", "{gone}");
    script.lock().unwrap().complete = false;
    let unproven = steps(
        &registry,
        "steps-expect-absent",
        json!([{"action": "click", "ref": "p3:6", "expect": {"text": "grace@x.com", "present": false}}]),
    )
    .await;
    assert_eq!(
        unproven["steps"][0]["code"], "expectation_unmet",
        "{unproven}"
    );
}

#[tokio::test]
async fn a_read_that_is_refused_fails_the_step_it_was_for_and_is_reported_for_the_batch() {
    let (registry, script) = page_registry();
    script.lock().unwrap().outline = None;
    let output = steps(
        &registry,
        "steps-read-refused",
        json!([
            {"action": "click", "ref": "p3:2"},
            {"action": "click", "role": "option", "name": "Editor"},
        ]),
    )
    .await;
    assert_eq!(
        calls(&script),
        [
            "browser_click p3:2",
            "get_browser_state Editor",
            "get_browser_state since 0"
        ]
    );
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("step_failed"))
    );
    assert_eq!(output["steps"][1]["code"], "browser_origin_outside_scope");
    // The step that ran stands; what it changed could not be read.
    assert_eq!(output["steps"][0]["status"], "ok");
    assert_eq!(output["changes"]["kind"], "unavailable");
    assert_eq!(output["changes"]["reason"], "browser_origin_outside_scope");
}

#[tokio::test]
async fn invalid_batches_are_refused_before_anything_is_sent() {
    let (registry, script) = page_registry();
    for bad in [
        json!([]),
        json!([{"action": "click"}]),
        json!([{"action": "press_key", "ref": "p3:1"}]),
        json!(vec![json!({"action": "click", "ref": "p3:6"}); 9]),
    ] {
        let result = registry
            .invoke_with_context(
                "browser_steps",
                json!({"target_id": "bt-1", "tab_id": "tab-1", "session": "steps-invalid", "steps": bad}),
                unrestricted(),
            )
            .await;
        assert_eq!(result.is_error, Some(true), "{bad}");
    }
    assert!(calls(&script).is_empty());
}

#[test]
fn the_batch_is_tab_input_and_its_output_fits_its_contract() {
    use crate::authorization::{advertised_risk_for, enforcement_adapters_for_call, RiskClass};
    assert_eq!(advertised_risk_for("browser_steps").class, RiskClass::R2);
    assert_eq!(
        enforcement_adapters_for_call("browser_steps", &json!({}))
            .iter()
            .map(|adapter| adapter.id)
            .collect::<Vec<_>>(),
        ["browser_bound_input"]
    );
    let contract = cua_driver_contract::tool_contract("browser_steps").unwrap();
    assert_eq!(contract.capabilities, ["browser.steps"]);
    assert!(!contract.annotations.read_only && !contract.annotations.idempotent);
}
