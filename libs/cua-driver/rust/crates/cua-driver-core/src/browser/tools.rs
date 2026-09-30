//! Browser state, preparation, navigation, input, and page-owned dialog tools.
//!
//! Schemas, annotations, and exact-or-refused semantics live here; OS
//! identity comes from the [`BrowserPlatform`] adapter; CDP mechanics
//! from [`super::engine`]. All structured outputs share the shape
//! `{"status": "ok" | "refused", ...}`.

use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use cua_driver_contract::{
    PageChangeOp, PageChangeOpKind, PageChanges, PageChangesKind, PageDialog,
};
use serde_json::{json, Value};

use crate::protocol::{Content, ToolResult};
use crate::recording_tools::ReplayRegistrySlot;
use crate::tool::{ProtectedResourceOwnership, Tool, ToolDef, ToolRegistry};
use crate::tool_args::ArgsExt;

use super::cdp_ws::{CdpConnection, CdpDialogState};
use super::download::BrowserDownloadTool;
use super::engine::{
    dialog_id, dialog_open_refusal, BrowserEngine, BrowserTabScreenshot, CallStopped, HeldView,
    PageWatch, SemanticSnapshotOutcome, Settled, ValidatedTab,
};
use super::observation::{DiffOp, FullReason, Told};
use super::platform::{BrowserVisualActionKind, PrepareProfile, PrepareRequest, PrepareStrategy};
use super::pointer::BrowserPointerTool;
use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use super::session_schema as schema_session;
use super::store::BrowserActionKind;
use super::types::BindingQuality;

/// Register the complete browser surface against one shared engine. Platform
/// crates call this from their `register_all` after constructing the
/// engine with their adapter.
pub fn register_browser_tools(engine: &Arc<BrowserEngine>, registry: &mut ToolRegistry) {
    // Page actions read the page afterwards through the registry, so that
    // read is authorized exactly as a get_browser_state call is.
    let slot = registry.composite_registry_slot();
    registry.register(Box::new(GetBrowserStateTool::new(engine.clone())));
    registry.register(Box::new(BrowserPrepareTool::new(engine.clone())));
    registry.register(Box::new(BrowserNavigateTool::with_registry(
        engine.clone(),
        slot.clone(),
    )));
    registry.register(Box::new(BrowserClickTool::with_registry(
        engine.clone(),
        slot.clone(),
    )));
    registry.register(Box::new(BrowserTypeTool::with_registry(
        engine.clone(),
        slot.clone(),
    )));
    registry.register(Box::new(super::steps::BrowserStepsTool::new(
        engine.clone(),
        slot,
    )));
    registry.register(Box::new(BrowserDialogTool::new(engine.clone())));
    registry.register(Box::new(BrowserSetInputFilesTool::new(engine.clone())));
    registry.register(Box::new(BrowserDownloadTool::new(engine.clone())));
    registry.register(Box::new(BrowserPointerTool::new(engine.clone())));
    registry.register(Box::new(super::tabs_tool::BrowserTabsTool::new(
        super::extension_bridge::global().clone(),
    )));
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// The public caller session id, falling back to the daemon's internal mirror.
pub(crate) fn session_of(args: &Value) -> String {
    args.opt_str("session")
        .or_else(|| args.opt_str("_session_id"))
        .unwrap_or_else(|| "default".into())
}

/// Target/ref minting requires an explicit (non-default) session so the
/// capability namespace has a real owner whose end event cleans it up.
fn require_explicit_session(args: &Value) -> Result<String, ToolResult> {
    let sid = session_of(args);
    if sid.is_empty() || sid == "default" {
        return Err(ToolResult::error(
            "Browser targets and page refs are session-scoped capabilities — declare an \
             explicit session (start_session) and pass its id on this call.",
        ));
    }
    Ok(sid)
}

fn schema_target_id() -> Value {
    json!({
        "type": "string",
        "description": "Target id from get_browser_state."
    })
}

fn schema_tab_id() -> Value {
    json!({
        "type": "string",
        "description": "Tab id from get_browser_state."
    })
}

fn schema_ref() -> Value {
    json!({
        "type": "string",
        "description": "Page ref from get_browser_state; stale after navigation or a \
            newer snapshot."
    })
}

pub(crate) fn browser_resource_ownership(
    engine: &BrowserEngine,
    args: &Value,
) -> ProtectedResourceOwnership {
    let Some(session) = args
        .get("session")
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty())
    else {
        return ProtectedResourceOwnership::UserOwned;
    };
    let runtime_session = crate::tool::current_dispatch_authorization_context()
        .map(|context| context.runtime_session_key(session))
        .unwrap_or_else(|| session.to_owned());
    let pid = args.get("pid").and_then(Value::as_i64).or_else(|| {
        args.get("target_id")
            .and_then(Value::as_str)
            .and_then(|target_id| engine.store.get_target(&runtime_session, target_id).ok())
            .map(|target| target.pid)
    });
    if pid.is_some_and(|pid| {
        engine.is_driver_owned_pid_for_session(&runtime_session, pid)
            || engine.is_driver_owned_pid_for_session(session, pid)
    }) {
        ProtectedResourceOwnership::DriverOwned
    } else {
        ProtectedResourceOwnership::UserOwned
    }
}

pub(crate) async fn browser_protected_resource_scope(
    engine: &BrowserEngine,
    args: &Value,
    tool_name: &str,
) -> Result<Option<Value>, String> {
    let session = args
        .get("session")
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty())
        .ok_or_else(|| "the browser operation requires an explicit session".to_owned())?;
    let target_id = args
        .get("target_id")
        .and_then(Value::as_str)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| "the browser operation requires an exact target_id".to_owned())?;
    let tab_id = args
        .get("tab_id")
        .and_then(Value::as_str)
        .filter(|tab| !tab.is_empty())
        .ok_or_else(|| "the browser operation requires an exact tab_id".to_owned())?;
    let runtime_session = crate::tool::current_dispatch_authorization_context()
        .map(|context| context.runtime_session_key(session))
        .unwrap_or_else(|| session.to_owned());
    let (validated, live_origin) = engine
        .attest_protected_tab(&runtime_session, target_id, tab_id)
        .await
        .map_err(|error| error.message)?;
    let target = validated.record;
    let tab = validated.tab;
    let requested_origin = if tool_name == "browser_navigate" {
        let requested = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "browser_navigate requires a destination URL".to_owned())?;
        if requested.to_ascii_lowercase().starts_with("about:") {
            Some("about:".to_owned())
        } else {
            let parsed = url::Url::parse(requested)
                .map_err(|_| "the destination URL is invalid".to_owned())?;
            Some(parsed.origin().ascii_serialization())
        }
    } else {
        None
    };
    let action_class = match tool_name {
        "get_browser_state" => "page_observation",
        "browser_navigate" => "navigation",
        "browser_dialog" => "page_dialog_resolution",
        _ => "page_input",
    };
    let mut resource = json!({
        "kind": "authenticated_browser_tab",
        "target_id": target_id,
        "tab_id": tab_id,
        "pid": target.pid,
        "process_fingerprint": target.fingerprint,
        "binding_generation": target.generation,
        "cdp_target_id": tab.cdp_target_id,
        "tab_generation": tab.generation,
        "live_origin": live_origin,
        "requested_origin": requested_origin,
        "action_class": action_class,
    });
    if tool_name == "browser_dialog" {
        resource["dialog_id"] = args.get("dialog_id").cloned().unwrap_or(Value::Null);
        resource["dialog_action"] = args.get("action").cloned().unwrap_or(Value::Null);
        resource["delivery_mode"] = Value::String(
            args.get("delivery_mode")
                .and_then(Value::as_str)
                .unwrap_or("background")
                .to_owned(),
        );
        resource["prompt_text_present"] =
            Value::Bool(args.get("prompt_text").and_then(Value::as_str).is_some());
    }
    Ok(Some(resource))
}

/// Sends the calls that put input into a page, and stops when a JavaScript
/// dialog opens: a page behind one answers nothing, the call that opened it
/// included.
struct Delivery<'a> {
    engine: &'a BrowserEngine,
    conn: &'a CdpConnection,
    cdp: &'a str,
    target: &'a str,
    /// The dialog the page opened while handling what was sent.
    opened: Option<CdpDialogState>,
}

impl Delivery<'_> {
    /// Whether a dialog is open now. A dialog can also open after a call was
    /// answered (a handler that defers its alert), so this is asked before
    /// every send and again before anything else is asked of the page.
    fn blocked(&mut self) -> bool {
        if self.opened.is_none() {
            self.opened = self.conn.dialog_state(self.target);
        }
        self.opened.is_some()
    }

    /// Send one call; `Ok(None)` when it was not sent because a dialog is
    /// open. A call the page answered is `Ok(Some(..))` even when the dialog
    /// opened while it ran: what it carried did reach the page.
    async fn send(&mut self, method: &str, params: Value) -> anyhow::Result<Option<Value>> {
        if self.blocked() {
            return Ok(None);
        }
        match self
            .engine
            .call_until_dialog(self.conn, self.cdp, self.target, method, params)
            .await
        {
            Ok(value) => {
                self.blocked();
                Ok(Some(value))
            }
            // The call that opened the dialog was delivered; its reply only
            // comes once the dialog is resolved.
            Err(CallStopped::Dialog(dialog)) => {
                self.opened = Some(dialog);
                Ok(Some(json!({})))
            }
            Err(CallStopped::Failed(error)) => Err(error),
        }
    }
}

/// The caller's own session label, for a child call through the registry
/// (which maps it back into the runtime namespace itself).
pub(crate) fn public_session(args: &Value) -> Option<String> {
    args.get("_public_session_label")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let session = args.get("session")?.as_str()?;
            match crate::tool::current_dispatch_runtime_scope() {
                Some(scope) => session
                    .strip_prefix(&format!("__cua_runtime_{scope}:"))
                    .map(str::to_owned),
                None => Some(session.to_owned()),
            }
        })
}

fn no_registry() -> ReplayRegistrySlot {
    Arc::new(Mutex::new(Weak::new()))
}

fn snapshot_changes(outcome: &SemanticSnapshotOutcome, reason: Option<FullReason>) -> PageChanges {
    PageChanges {
        kind: PageChangesKind::Snapshot,
        reason: reason.map(|reason| reason.as_str().to_owned()),
        snapshot_id: Some(format!("p{}", outcome.snapshot_id)),
        base_revision: None,
        revision: outcome.revision,
        ops: None,
        outline: Some(outcome.outline.clone()),
        url: Some(outcome.url.clone()),
        title: Some(outcome.title.clone()),
        complete: Some(outcome.complete),
        continuation: outcome.continuation.clone(),
        dialog: None,
        settled: None,
    }
}

/// What a read asked with `since_revision` tells: the keyed diff when one
/// was possible and is smaller than the snapshot it would replace, otherwise
/// the snapshot with the reason.
fn page_changes(outcome: &SemanticSnapshotOutcome) -> PageChanges {
    let (base_revision, ops, page_changed) = match &outcome.told {
        Told::Snapshot { reason } => return snapshot_changes(outcome, *reason),
        Told::Diff {
            base_revision,
            ops,
            page_changed,
        } => (*base_revision, ops, *page_changed),
    };
    let op =
        |kind, key: &str, line: Option<&String>, after: Option<&String>, gone: bool| PageChangeOp {
            op: kind,
            reference: key.to_owned(),
            line: line.cloned(),
            after: after.cloned(),
            gone: gone.then_some(true),
        };
    let diff = PageChanges {
        kind: PageChangesKind::Diff,
        reason: None,
        snapshot_id: Some(format!("p{}", outcome.snapshot_id)),
        base_revision: Some(base_revision),
        revision: outcome.revision,
        ops: Some(
            ops.iter()
                .map(|change| match change {
                    DiffOp::Leave { key, gone } => {
                        op(PageChangeOpKind::Leave, key, None, None, *gone)
                    }
                    DiffOp::Change { key, line } => {
                        op(PageChangeOpKind::Change, key, Some(line), None, false)
                    }
                    DiffOp::Add { key, after, line } => op(
                        PageChangeOpKind::Add,
                        key,
                        Some(line),
                        after.as_ref(),
                        false,
                    ),
                    DiffOp::Move { key, after, line } => op(
                        PageChangeOpKind::Move,
                        key,
                        Some(line),
                        after.as_ref(),
                        false,
                    ),
                })
                .collect(),
        ),
        outline: None,
        url: page_changed.then(|| outcome.url.clone()),
        title: page_changed.then(|| outcome.title.clone()),
        complete: None,
        continuation: None,
        dialog: None,
        settled: None,
    };
    let chars = |changes: &PageChanges| {
        serde_json::to_string(changes).map_or(usize::MAX, |json| json.chars().count())
    };
    let snapshot = snapshot_changes(outcome, Some(FullReason::DiffLargerThanSnapshot));
    if chars(&diff) < chars(&snapshot) {
        diff
    } else {
        snapshot
    }
}

fn unavailable_changes(reason: &str, dialog: Option<&CdpDialogState>) -> PageChanges {
    PageChanges {
        kind: PageChangesKind::Unavailable,
        reason: Some(reason.to_owned()),
        snapshot_id: None,
        base_revision: None,
        revision: None,
        ops: None,
        outline: None,
        url: None,
        title: None,
        complete: None,
        continuation: None,
        dialog: dialog.map(|dialog| PageDialog {
            dialog_id: dialog_id(dialog),
            kind: dialog.kind.clone(),
        }),
        settled: None,
    }
}

fn changes_value(changes: &PageChanges) -> Value {
    serde_json::to_value(changes).expect("page changes serialize")
}

/// After input reached the page: wait within bounds for the page to settle,
/// then read what changed since `held`.
///
/// The read is a `get_browser_state` call through the registry, so it is
/// admitted or refused exactly as the caller's own read would be (tool
/// allowlist, session, live origin, observation consent). `None` means the
/// result carries no `changes`: the session works from a `dom_refs_v1`
/// snapshot, whose refs a semantic read would end, or the tool runs outside
/// a registry.
// One action's tab, what its session held, and how the wait for the page
// is to be made or has already ended.
#[allow(clippy::too_many_arguments)]
async fn page_changes_after(
    engine: &BrowserEngine,
    registry: &ReplayRegistrySlot,
    args: &Value,
    target_id: &str,
    tab_id: &str,
    validated: &ValidatedTab,
    held: HeldView,
    settled: Option<Settled>,
    watch: Option<PageWatch>,
) -> Option<Value> {
    let dialog_changes = |dialog: &CdpDialogState| {
        Some(changes_value(&unavailable_changes(
            "javascript_dialog_open",
            Some(dialog),
        )))
    };
    // A dialog the dispatch itself ran into is reported whatever else holds.
    if let Some(Settled::Dialog(dialog)) = &settled {
        return dialog_changes(dialog);
    }
    // As a step of browser_steps: settle, so the next step meets a page at
    // rest, and say only what decides whether there is a next step. The
    // batch reads the page once at its end.
    let batch = super::steps::in_steps_batch();
    if !batch && held == HeldView::DomRefs {
        return None;
    }
    let registry = if batch {
        None
    } else {
        Some(registry.lock().unwrap().upgrade()?)
    };
    let settled = match settled {
        Some(settled) => settled,
        None => engine.settle(validated, watch).await,
    };
    if let Settled::Dialog(dialog) = &settled {
        return dialog_changes(dialog);
    }
    let Some(registry) = registry else {
        return matches!(settled, Settled::NewDocument { .. })
            .then(|| changes_value(&unavailable_changes("document_changed", None)));
    };
    let mut read = json!({
        "target_id": target_id,
        "tab_id": tab_id,
        "since_revision": match held {
            HeldView::Semantic(revision) => revision,
            _ => 0,
        },
    });
    if let Some(session) = public_session(args) {
        read["session"] = json!(session);
    }
    let result = registry.invoke("get_browser_state", read).await;
    let structured = result.structured_content.unwrap_or(Value::Null);
    let mut changes = match structured.get("changes") {
        Some(changes) if structured["status"] == "ok" => changes.clone(),
        _ => {
            // The action stands; only its observation was refused or failed.
            let reason = structured
                .pointer("/refusal/code")
                .or_else(|| structured.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("observation_failed");
            return Some(changes_value(&unavailable_changes(reason, None)));
        }
    };
    if matches!(
        settled,
        Settled::Deadline | Settled::NewDocument { loaded: false }
    ) {
        changes["settled"] = json!(false);
    }
    Some(changes)
}

/// Put `changes` on a result that has structured content.
fn with_changes(mut result: ToolResult, changes: Option<Value>) -> ToolResult {
    if let (Some(changes), Some(structured)) = (changes, result.structured_content.as_mut()) {
        structured["changes"] = changes;
    }
    result
}

/// The result of one semantic snapshot. The outline is the only place the
/// refs appear: `- role "name" [ref actions] = "value" (states)`.
fn semantic_snapshot_result(
    target_id: &str,
    tab_id: &str,
    outcome: &super::engine::SemanticSnapshotOutcome,
) -> ToolResult {
    let mut result = ToolResult::text(format!(
        "snapshot p{}: {} action ref(s), {} content ref(s) in the outline",
        outcome.snapshot_id, outcome.action_refs, outcome.content_refs
    ))
    .with_structured(json!({
        "status": "ok",
        "mode": "snapshot",
        "target_id": target_id,
        "tab_id": tab_id,
        "snapshot": {
            "id": format!("p{}", outcome.snapshot_id),
            "revision": outcome.revision,
            "format": "semantic_v2",
            "complete": outcome.complete,
            "scope": outcome.scope,
            "selected_nodes": outcome.selected_nodes,
            "total_nodes": outcome.total_nodes,
            "node_budget": super::semantic::DEFAULT_SEMANTIC_NODE_BUDGET,
            "outline_char_budget": outcome.outline_budget,
            "omitted": {
                "css_hidden": outcome.omissions.css_hidden,
                "offscreen": outcome.omissions.offscreen,
                "page_occluded": outcome.omissions.page_occluded,
                "no_layout": outcome.omissions.no_layout,
                "unknown": outcome.omissions.unknown,
                "budget": outcome.omissions.budget,
                "unprovable_frame": outcome.omissions.unprovable_frame,
                "no_dom_node": outcome.omissions.no_dom_node,
            },
            "continuation": outcome.continuation,
        },
        "page": {
            "url": outcome.url,
            "title": outcome.title,
        },
        "outline": outcome.outline,
        "oopif": {
            "status": outcome.oopif.as_str(),
            "frames": outcome.oopif.frames(),
        },
    }));
    if let (Some(listed), Some(structured)) = (&outcome.listed, result.structured_content.as_mut())
    {
        let (actions, content): (Vec<&Value>, Vec<&Value>) = listed
            .iter()
            .partition(|entry| entry["actions"].as_array().is_some_and(|a| !a.is_empty()));
        structured["refs"] = json!(actions);
        structured["content_refs"] = json!(content);
    }
    result
}

fn with_tab_screenshot(mut result: ToolResult, screenshot: BrowserTabScreenshot) -> ToolResult {
    if let Some(structured) = result.structured_content.as_mut() {
        structured["screenshot"] = json!({
            "source": "cdp_tab",
            "scope": "viewport",
            "mime_type": "image/png",
            "width": screenshot.width,
            "height": screenshot.height,
            "coordinate_space": "viewport_css_px",
            "viewport_css_width": screenshot.viewport_css_width,
            "viewport_css_height": screenshot.viewport_css_height,
            "pixel_to_css_scale_x": screenshot.pixel_to_css_scale_x,
            "pixel_to_css_scale_y": screenshot.pixel_to_css_scale_y,
            "tab_activation": "not_requested",
            "window_foregrounding": "not_requested",
        });
        structured["screenshot_width"] = json!(screenshot.width);
        structured["screenshot_height"] = json!(screenshot.height);
        structured["screenshot_mime_type"] = json!("image/png");
    }
    result
        .content
        .insert(0, Content::image_png(screenshot.data_base64));
    result
}

// ── get_browser_state ────────────────────────────────────────────────────────

pub struct GetBrowserStateTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
}

impl GetBrowserStateTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        let def = ToolDef {
            name: "get_browser_state".into(),
            description: "Read-only browser inspection. Bind with pid + window_id to get a \
                target_id and tab ids; snapshot with target_id + tab_id to get the page \
                outline, one line per element with its ref and actions inline. Consent and \
                setup refusals give the browser_prepare call to make. \
                Details: skill://cua-driver/BROWSER.md"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Browser process id (bind)." },
                    "window_id": { "type": "integer", "description": "Browser window id owned by pid (bind)." },
                    "target_id": schema_target_id(),
                    "tab_id": schema_tab_id(),
                    "session": schema_session(),
                    "snapshot_format": {
                        "type": "string",
                        "enum": ["semantic_v2", "dom_refs_v1"],
                        "default": "semantic_v2",
                        "description": "semantic_v2: ranked outline with inline refs. dom_refs_v1: a flat ref list."
                    },
                    "max_chars": {
                        "type": "integer",
                        "default": super::engine::DEFAULT_SNAPSHOT_CHARS,
                        "minimum": super::engine::MIN_SNAPSHOT_CHARS,
                        "maximum": super::engine::MAX_SNAPSHOT_CHARS,
                        "description": "Size budget for a semantic_v2 result; the rest is reached by continuation."
                    },
                    "scope_ref": {
                        "type": "string",
                        "description": "Ref whose subtree to observe."
                    },
                    "query": {
                        "type": "string",
                        "description": "Match over role, accessible name, and visible text."
                    },
                    "continuation": {
                        "type": "string",
                        "description": "Continuation from an earlier response."
                    },
                    "include_refs": {
                        "type": "boolean",
                        "default": false,
                        "description": "Also return the outline's refs as lists (refs, content_refs) for programs; counts against max_chars."
                    },
                    "since_revision": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Return what changed since this snapshot.revision instead of the whole outline."
                    },
                    "include_screenshot": {
                        "type": "boolean",
                        "default": false,
                        "description": "Capture the tab viewport PNG via CDP without selecting the tab or raising the window."
                    },
                },
                "additionalProperties": true
            }),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        };
        Self { def, engine }
    }
}

#[async_trait]
impl Tool for GetBrowserStateTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn protected_resource_ownership(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> ProtectedResourceOwnership {
        if adapter_id == "private_observation" {
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
        if adapter_id == "private_observation"
            && args.get("target_id").and_then(Value::as_str).is_some()
        {
            browser_protected_resource_scope(&self.engine, args, "get_browser_state").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // Snapshot mode: target_id (+ tab_id) — uses existing capabilities.
        if let Some(target_id) = args.opt_str("target_id") {
            let session = match require_explicit_session(&args) {
                Ok(s) => s,
                Err(e) => return e,
            };
            let tab_id = match args.opt_str("tab_id") {
                Some(t) => t,
                None => {
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserTabRequired,
                        "snapshot mode requires a tab_id from a prior bind",
                    )
                    .to_tool_result()
                }
            };
            let snapshot_format = args
                .opt_str("snapshot_format")
                .unwrap_or_else(|| "semantic_v2".into());
            let include_screenshot = match args.get("include_screenshot") {
                None => false,
                Some(Value::Bool(include)) => *include,
                Some(_) => {
                    return ToolResult::error(
                        "Field include_screenshot has wrong type: expected boolean",
                    )
                }
            };
            if snapshot_format != "dom_refs_v1" && snapshot_format != "semantic_v2" {
                return ToolResult::error(format!(
                    "snapshot_format must be \"dom_refs_v1\" or \"semantic_v2\", got {snapshot_format:?}"
                ));
            }
            if snapshot_format == "dom_refs_v1"
                && (args.opt_str("scope_ref").is_some()
                    || args.opt_str("query").is_some()
                    || args.opt_str("continuation").is_some()
                    || args.get("max_chars").is_some()
                    || args.get("include_refs").is_some()
                    || args.get("since_revision").is_some())
            {
                return ToolResult::error(
                    "scope_ref, query, continuation, max_chars, include_refs, and since_revision require snapshot_format=\"semantic_v2\"",
                );
            }
            let since =
                match args.get("since_revision") {
                    None => None,
                    Some(value) => match value.as_u64() {
                        Some(revision) => Some(revision),
                        None => return ToolResult::error(
                            "since_revision must be a snapshot.revision (a non-negative integer)",
                        ),
                    },
                };
            if since.is_some()
                && (args.opt_str("scope_ref").is_some()
                    || args.opt_str("query").is_some()
                    || args.opt_str("continuation").is_some()
                    || args.get("include_refs").is_some())
            {
                return ToolResult::error(
                    "since_revision compares whole-page snapshots: it cannot be combined with scope_ref, query, continuation, or include_refs",
                );
            }
            let include_refs = match args.get("include_refs") {
                None => false,
                Some(Value::Bool(include)) => *include,
                Some(_) => {
                    return ToolResult::error("Field include_refs has wrong type: expected boolean")
                }
            };
            let max_chars = match args.get("max_chars") {
                None => super::engine::DEFAULT_SNAPSHOT_CHARS,
                Some(value) => match value.as_u64().map(|chars| chars as usize) {
                    Some(chars)
                        if (super::engine::MIN_SNAPSHOT_CHARS
                            ..=super::engine::MAX_SNAPSHOT_CHARS)
                            .contains(&chars) =>
                    {
                        chars
                    }
                    _ => {
                        return ToolResult::error(format!(
                            "max_chars must be an integer from {} to {}",
                            super::engine::MIN_SNAPSHOT_CHARS,
                            super::engine::MAX_SNAPSHOT_CHARS
                        ))
                    }
                },
            };
            if snapshot_format == "semantic_v2" {
                let snapshot = match self
                    .engine
                    .snapshot_tab_semantic(
                        &session,
                        &target_id,
                        &tab_id,
                        args.opt_str("scope_ref").as_deref(),
                        args.opt_str("query").as_deref(),
                        args.opt_str("continuation").as_deref(),
                        max_chars,
                        include_refs,
                        since,
                    )
                    .await
                {
                    Ok(outcome) if since.is_some() => {
                        let changes = page_changes(&outcome);
                        ToolResult::text(match changes.kind {
                            PageChangesKind::Diff => format!(
                                "{} change(s) since revision {}",
                                changes.ops.as_ref().map_or(0, Vec::len),
                                changes.base_revision.unwrap_or_default()
                            ),
                            _ => format!(
                                "snapshot p{} ({})",
                                outcome.snapshot_id,
                                changes.reason.as_deref().unwrap_or("full")
                            ),
                        })
                        .with_structured(json!({
                            "status": "ok",
                            "mode": "changes",
                            "target_id": target_id,
                            "tab_id": tab_id,
                            "changes": changes_value(&changes),
                        }))
                    }
                    Ok(outcome) => semantic_snapshot_result(&target_id, &tab_id, &outcome),
                    Err(refusal) => return refusal.to_tool_result(),
                };
                if include_screenshot {
                    return match self
                        .engine
                        .capture_tab_screenshot(&session, &target_id, &tab_id)
                        .await
                    {
                        Ok(screenshot) => with_tab_screenshot(snapshot, screenshot),
                        Err(refusal) => refusal.to_tool_result(),
                    };
                }
                return snapshot;
            }
            let snapshot = match self
                .engine
                .snapshot_tab(&session, &target_id, &tab_id)
                .await
            {
                Ok(outcome) => {
                    let ref_list: Vec<Value> = outcome
                        .refs
                        .iter()
                        .map(|(ext, entry)| {
                            json!({
                                "ref": ext,
                                "node": entry.node_name,
                                "label": entry.label,
                                "frame": entry.frame.kind.as_str(),
                            })
                        })
                        .collect();
                    ToolResult::text(format!(
                        "snapshot p{} of {}: {} interactive element(s)",
                        outcome.snapshot_id,
                        outcome.url,
                        ref_list.len()
                    ))
                    .with_structured(json!({
                        "status": "ok",
                        "mode": "snapshot",
                        "target_id": target_id,
                        "tab_id": tab_id,
                        "snapshot_id": format!("p{}", outcome.snapshot_id),
                        "url": outcome.url,
                        "refs": ref_list,
                        "truncated": outcome.truncated,
                        "oopif": {
                            "status": outcome.oopif.as_str(),
                            "frames": outcome.oopif.frames(),
                        },
                    }))
                }
                Err(refusal) => return refusal.to_tool_result(),
            };
            if include_screenshot {
                return match self
                    .engine
                    .capture_tab_screenshot(&session, &target_id, &tab_id)
                    .await
                {
                    Ok(screenshot) => with_tab_screenshot(snapshot, screenshot),
                    Err(refusal) => refusal.to_tool_result(),
                };
            }
            return snapshot;
        }

        // Bind mode: pid + window_id.
        let pid = match args.require_i64("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let window_id = match args.require_u64("window_id") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let session = match require_explicit_session(&args) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let transport_session = args.opt_str("_transport_session_id");
        match self
            .engine
            .bind_native(&session, transport_session.as_deref(), pid, window_id)
            .await
        {
            Ok((target_id, record)) => {
                let tabs: Vec<Value> = record
                    .tabs
                    .values()
                    .map(|t| {
                        json!({
                            "tab_id": t.tab_id,
                            "title": t.title,
                            "url": t.url,
                            "active": t.active,
                        })
                    })
                    .collect();
                let quality = match record.quality {
                    BindingQuality::Exact => "exact",
                    BindingQuality::Heuristic => "heuristic",
                };
                let binding_route = if record.cdp_window_id.is_some() {
                    "native_cdp_window"
                } else {
                    "embedded_single_page"
                };
                ToolResult::text(format!(
                    "bound target {target_id} ({quality}) with {} tab(s)",
                    tabs.len()
                ))
                .with_structured(json!({
                    "status": "ok",
                    "mode": "bind",
                    "target_id": target_id,
                    "binding_quality": quality,
                    "binding_route": binding_route,
                    "endpoint_transport": record.endpoint_transport,
                    "endpoint_access_class": record.endpoint_access_class,
                    "mutation_allowed": record.quality == BindingQuality::Exact,
                    "native_title": record.native_title,
                    "tabs": tabs,
                }))
            }
            Err(refusal) => refusal.to_tool_result(),
        }
    }
}

// ── browser_prepare ──────────────────────────────────────────────────────────

pub struct BrowserPrepareTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
}

impl BrowserPrepareTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        let def = ToolDef {
            name: "browser_prepare".into(),
            description: "Prepare a driver-owned DevTools endpoint for a browser; detecting an \
                existing endpoint has no side effects. allow_launch:true starts a separate \
                isolated profile. Attaching to an existing profile needs a runtime grant or \
                the Cua Driver Chrome extension; MCP approval alone never authorizes it. \
                Details: skill://cua-driver/BROWSER.md"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Browser pid; optional only for an allow_launch isolated launch." },
                    "window_id": { "type": "integer", "description": "Exact window anchor; required for strategy existing_profile." },
                    "allow_launch": {
                        "type": "boolean",
                        "default": false,
                        "description": "Allow launching a separate driver-owned isolated Chromium."
                    },
                    "profile": {
                        "type": "object",
                        "properties": {
                            "mode": { "type": "string", "enum": ["isolated_new", "isolated_named"] },
                            "name": { "type": "string", "description": "isolated_named only; 1-64 path-safe ASCII characters." }
                        },
                        "required": ["mode"],
                        "additionalProperties": false
                    },
                    "strategy": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["existing_profile"] }
                        },
                        "required": ["kind"],
                        "additionalProperties": false
                    },
                    "session": schema_session(),
                },
                // Keep the top-level input schema a plain object. Bedrock rejects
                // anyOf/oneOf/allOf at this level, so the conditional pid/profile
                // contract is described above and enforced by invoke instead.
                "required": [],
                "additionalProperties": true
            }),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        };
        Self { def, engine }
    }
}

#[async_trait]
impl Tool for BrowserPrepareTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = match require_explicit_session(&args) {
            Ok(session) => session,
            Err(error) => return error,
        };
        let profile = match args.get("profile") {
            None | Some(Value::Null) => None,
            Some(value) => match serde_json::from_value::<PrepareProfile>(value.clone()) {
                Ok(profile) => Some(profile),
                Err(error) => {
                    return ToolResult::error(format!("invalid browser profile request: {error}"))
                }
            },
        };
        let strategy = match args.get("strategy") {
            None | Some(Value::Null) => None,
            Some(value) => match serde_json::from_value::<PrepareStrategy>(value.clone()) {
                Ok(strategy) => Some(strategy),
                Err(error) => {
                    return ToolResult::error(format!(
                        "invalid browser preparation strategy: {error}"
                    ))
                }
            },
        };
        let allow_launch = args.opt_bool("allow_launch").unwrap_or(false);
        let pid = match args.get("pid") {
            None => None,
            Some(_) => match args.require_i64("pid") {
                Ok(pid) => Some(pid),
                Err(error) => return error,
            },
        };
        let pid_optional = strategy.is_none() && profile.is_some() && allow_launch;
        if pid.is_none() && !pid_optional {
            return match args.require_i64("pid") {
                Ok(_) => unreachable!("pid was already parsed"),
                Err(error) => error,
            };
        }
        let request = PrepareRequest {
            pid,
            window_id: args.opt_u64("window_id"),
            session,
            transport_session: args.opt_str("_transport_session_id"),
            strategy,
            profile,
            allow_launch,
        };
        match self.engine.prepare_browser(request).await {
            Ok(outcome) => {
                let prepared = outcome.endpoint.is_some();
                ToolResult::text(format!(
                    "browser_prepare: {} — {}",
                    if prepared {
                        "endpoint available"
                    } else {
                        "no endpoint"
                    },
                    outcome.message
                ))
                .with_structured(json!({
                    "status": "ok",
                    "prepared": prepared,
                    "action": outcome.action,
                    "message": outcome.message,
                    // The ws_url itself stays internal; expose only proof metadata.
                    "endpoint_ownership": outcome.endpoint.map(|e| e.ownership),
                    "prepared_pid": outcome.prepared_pid,
                    "side_effects": outcome.side_effects,
                    "attachment": outcome.attachment,
                }))
            }
            Err(refusal) => refusal.to_tool_result(),
        }
    }
}

// ── browser_navigate ─────────────────────────────────────────────────────────

pub struct BrowserNavigateTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
    registry: ReplayRegistrySlot,
}

impl BrowserNavigateTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self::with_registry(engine, no_registry())
    }

    /// `registry` is what the read of the new page is dispatched through.
    pub fn with_registry(engine: Arc<BrowserEngine>, registry: ReplayRegistrySlot) -> Self {
        let def = ToolDef {
            name: "browser_navigate".into(),
            description: "Navigate one bound tab to an http, https, or about URL. Invalidates \
                the tab's page refs and returns the new page's outline in changes. Refused \
                for heuristic bindings."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target_id": schema_target_id(),
                    "tab_id": schema_tab_id(),
                    "url": { "type": "string", "description": "Destination URL." },
                    "session": schema_session(),
                },
                "required": ["target_id", "tab_id", "url"],
                "additionalProperties": true
            }),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: true,
        };
        Self {
            def,
            engine,
            registry,
        }
    }
}

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

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
            browser_protected_resource_scope(&self.engine, args, "browser_navigate").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (target_id, tab_id, url) = match (
            args.require_str("target_id"),
            args.require_str("tab_id"),
            args.require_str("url"),
        ) {
            (Ok(t), Ok(tab), Ok(u)) => (t, tab, u),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        let session = match require_explicit_session(&args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("about:"))
        {
            return ToolResult::error(format!(
                "browser_navigate only accepts http/https/about URLs, got: {url}"
            ));
        }

        let _mutation = match self
            .engine
            .lock_mutation(&session, &target_id, &tab_id)
            .await
        {
            Ok(guard) => guard,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let validated = match self
            .engine
            .revalidate_for_mutation(&session, &target_id, Some(&tab_id))
            .await
        {
            Ok(v) => v,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let held = self.engine.held_view(&session, &target_id, &tab_id);

        match validated
            .conn
            .call(
                Some(&validated.cdp_session),
                "Page.navigate",
                json!({ "url": url }),
            )
            .await
        {
            Ok(result) => {
                if let Some(err_text) = result.get("errorText").and_then(Value::as_str) {
                    return ToolResult::error(format!("navigation failed: {err_text}"));
                }
                // Refs die with the old document.
                self.engine
                    .store
                    .invalidate_tab_snapshots(&session, &target_id, &tab_id);
                // The new page, once it has loaded (bounded): a full snapshot,
                // since nothing of the old document can be compared with it.
                let changes = if held == HeldView::DomRefs {
                    None
                } else {
                    // Page.navigate answers once the new document has
                    // replaced the old one.
                    let loaded = self
                        .engine
                        .await_document(&validated, None, tokio::time::Instant::now())
                        .await;
                    page_changes_after(
                        &self.engine,
                        &self.registry,
                        &args,
                        &target_id,
                        &tab_id,
                        &validated,
                        HeldView::Nothing,
                        Some(loaded),
                        None,
                    )
                    .await
                };
                with_changes(
                    ToolResult::text(format!("navigated {tab_id} to {url}")).with_structured(
                        json!({
                            "status": "ok",
                            "target_id": target_id,
                            "tab_id": tab_id,
                            "url": url,
                            "refs_invalidated": true,
                        }),
                    ),
                    changes,
                )
            }
            Err(e) => ToolResult::error(format!("Page.navigate failed: {e}")),
        }
    }
}

// ── browser_click ────────────────────────────────────────────────────────────

pub struct BrowserClickTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
    registry: ReplayRegistrySlot,
}

impl BrowserClickTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self::with_registry(engine, no_registry())
    }

    /// `registry` is what the read after the click is dispatched through.
    pub fn with_registry(engine: Arc<BrowserEngine>, registry: ReplayRegistrySlot) -> Self {
        let def = ToolDef {
            name: "browser_click".into(),
            description: "Click a page ref or viewport x,y in a bound tab with trusted input; \
                refuses rather than raising a standalone browser. input_route \"dom_event\" \
                (ref required) only proves dispatch. Returns what the page changed in \
                changes. Refused for heuristic bindings."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target_id": schema_target_id(),
                    "tab_id": schema_tab_id(),
                    "session": schema_session(),
                    "ref": schema_ref(),
                    "x": { "type": "number", "description": "Viewport x in CSS px, instead of ref." },
                    "y": { "type": "number", "description": "Viewport y in CSS px." },
                    "input_route": {
                        "type": "string",
                        "enum": ["trusted", "dom_event"],
                        "default": "trusted",
                        "description": "trusted: CDP mouse events. dom_event: a synthetic DOM click; verify the effect."
                    },
                },
                "required": ["target_id", "tab_id"],
                "additionalProperties": true
            }),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: true,
        };
        Self {
            def,
            engine,
            registry,
        }
    }
}

#[async_trait]
impl Tool for BrowserClickTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

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
            browser_protected_resource_scope(&self.engine, args, "browser_click").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (target_id, tab_id) = match (args.require_str("target_id"), args.require_str("tab_id"))
        {
            (Ok(t), Ok(tab)) => (t, tab),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        let session = match require_explicit_session(&args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let route = args
            .opt_str("input_route")
            .unwrap_or_else(|| "trusted".into());
        if route != "trusted" && route != "dom_event" {
            return ToolResult::error(format!(
                "input_route must be \"trusted\" or \"dom_event\", got {route:?}"
            ));
        }
        let ext_ref = args.opt_str("ref");
        let coords = match (args.opt_f64("x"), args.opt_f64("y")) {
            (Some(x), Some(y)) => Some((x, y)),
            (None, None) => None,
            _ => return ToolResult::error("pass both x and y, or neither"),
        };
        if ext_ref.is_none() && coords.is_none() {
            return ToolResult::error("browser_click needs a ref or x/y coordinates");
        }
        if route == "dom_event" && ext_ref.is_none() {
            return ToolResult::error("input_route=dom_event requires a ref");
        }

        // Resolve the ref BEFORE revalidation? No — revalidate first so a
        // stale binding refuses before we touch the page at all.
        let _mutation = match self
            .engine
            .lock_mutation(&session, &target_id, &tab_id)
            .await
        {
            Ok(guard) => guard,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let validated = match self
            .engine
            .revalidate_for_mutation(&session, &target_id, Some(&tab_id))
            .await
        {
            Ok(v) => v,
            Err(refusal) => return refusal.to_tool_result(),
        };
        // The activation limitation describes the remote-debugging route. Input
        // through the extension's chrome.debugger does not activate Chrome: on
        // the lane VM, trusted typing and clicks landed with Finder in front
        // and with Chrome fully covered (8 of 8), and Finder stayed frontmost.
        if route == "trusted"
            && validated.record.cdp_window_id.is_some()
            && validated.record.endpoint_transport
                != super::types::EndpointTransport::ExtensionRelay
        {
            if let Some(limitation) = self
                .engine
                .platform
                .standalone_trusted_input_background_limitation()
            {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserInputTrustUnavailable,
                    format!(
                        "{limitation}; use input_route=\"dom_event\" with a ref for a synthetic full-background click"
                    ),
                )
                .with_detail(json!({
                    "requested_route": "trusted",
                    "limitation": limitation,
                    "alternative_route": "dom_event",
                    "alternative_requires_ref": true,
                    "trusted_delivery_attempted": false,
                }))
                .to_tool_result();
            }
        }

        // What the session holds now is what the changes are made against.
        let held = self.engine.held_view(&session, &target_id, &tab_id);
        // A click can open a JavaScript dialog, and a page behind one answers
        // nothing: hear about it, and do not send input into one already up.
        let cdp_target = validated.tab.cdp_target_id.as_str();
        self.engine
            .watch_dialogs(&validated.conn, &validated.cdp_session, cdp_target)
            .await;
        if let Some(dialog) = validated.conn.dialog_state(cdp_target) {
            return dialog_open_refusal(&dialog).to_tool_result();
        }
        // From before the input: a navigation it sets off is seen starting.
        let watch = self.engine.page_watch(&validated).await;

        // Ref path: re-prove the ref's frame/document identity and get
        // the session (tab or contained OOPIF child) its node lives in.
        let mut ref_frame = None;
        let (backend_node_id, frame_kind, cdp_session) = match &ext_ref {
            Some(r) => {
                let entry = match self
                    .engine
                    .store
                    .resolve_ref(&session, &target_id, &tab_id, r)
                {
                    Ok(entry) => entry,
                    Err(refusal) => return refusal.to_tool_result(),
                };
                if entry.semantic && !entry.actions.contains(&BrowserActionKind::Click) {
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserActionUnavailable,
                        format!("semantic ref {r} does not declare the click action"),
                    )
                    .to_tool_result();
                }
                let frame_session = match self
                    .engine
                    .frame_session_for_mutation(&session, &target_id, &tab_id, &validated, &entry)
                    .await
                {
                    Ok(s) => s,
                    Err(refusal) => return refusal.to_tool_result(),
                };
                ref_frame = Some(entry.frame.clone());
                (
                    Some(entry.backend_node_id),
                    Some(entry.frame.kind.as_str()),
                    frame_session,
                )
            }
            None => (None, None, validated.cdp_session.clone()),
        };

        let conn = &validated.conn;
        let cdp = cdp_session.as_str();

        // dom_event route: explicit opt-in only.
        if route == "dom_event" {
            let backend = backend_node_id.expect("checked above");
            let resolved = match conn
                .call(
                    Some(cdp),
                    "DOM.resolveNode",
                    json!({ "backendNodeId": backend }),
                )
                .await
            {
                Ok(v) => v,
                Err(_) => {
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserRefStale,
                        "the ref's node no longer resolves in the live page",
                    )
                    .to_tool_result()
                }
            };
            let object_id = match resolved
                .get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(Value::as_str)
            {
                Some(o) => o.to_owned(),
                None => {
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserRefStale,
                        "the ref's node has no live object in the page",
                    )
                    .to_tool_result()
                }
            };
            // Cursor feedback is best-effort and visual-only. A missing box
            // must not turn a valid DOM-event action into a refusal.
            let _ = conn
                .call(
                    Some(cdp),
                    "DOM.scrollIntoViewIfNeeded",
                    json!({ "backendNodeId": backend }),
                )
                .await;
            if let Ok(box_model) = conn
                .call(
                    Some(cdp),
                    "DOM.getBoxModel",
                    json!({ "backendNodeId": backend }),
                )
                .await
            {
                if let Some((x, y)) = quad_center(&box_model) {
                    self.engine
                        .visualize_browser_action(
                            &session,
                            &validated,
                            cdp,
                            x,
                            y,
                            BrowserVisualActionKind::Click,
                        )
                        .await;
                }
            }
            let opened = match self
                .engine
                .call_until_dialog(
                    conn,
                    cdp,
                    cdp_target,
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": "function() { this.click(); }",
                    }),
                )
                .await
            {
                Ok(_) => None,
                // The click handler opened a dialog: it ran.
                Err(CallStopped::Dialog(dialog)) => Some(Settled::Dialog(dialog)),
                Err(CallStopped::Failed(e)) => {
                    return ToolResult::error(format!("DOM click failed: {e}"))
                }
            };
            let changes = page_changes_after(
                &self.engine,
                &self.registry,
                &args,
                &target_id,
                &tab_id,
                &validated,
                held,
                opened,
                watch,
            )
            .await;
            return with_changes(
                ToolResult::text(format!(
                    "dispatched synthetic DOM click on {} in {tab_id}; application effect not \
                     verified (trust-gated controls may ignore untrusted events). Check changes, \
                     or read the page with get_browser_state, to verify; if the control \
                     ignored it, click again without input_route to use trusted input",
                    ext_ref.as_deref().unwrap_or("?")
                ))
                .with_structured(json!({
                    "status": "ok",
                    "effect": "unverifiable",
                    "route": "dom_event",
                    "target_id": target_id,
                    "tab_id": tab_id,
                    "ref": ext_ref,
                    "frame": frame_kind,
                    // No escalation: the page-action target would point at the
                    // legacy page tool, whose mutations are off by default. The
                    // summary names the real next step.
                })),
                changes,
            );
        }

        // Trusted route: resolve a click point, then Input.dispatchMouseEvent.
        let (x, y) =
            match (backend_node_id, coords) {
                (Some(backend), _) => {
                    // The point is the centre of the element's box, and the click
                    // goes to whatever is on top there. Ask the page what that
                    // is before sending it: once more after a scroll and a beat
                    // (a popover may still be moving), then refuse.
                    let mut attempt = 0;
                    loop {
                        // Best effort scroll-into-view; ignore failure (older Chromium).
                        let _ = conn
                            .call(
                                Some(cdp),
                                "DOM.scrollIntoViewIfNeeded",
                                json!({ "backendNodeId": backend }),
                            )
                            .await;
                        let box_model = match conn
                            .call(
                                Some(cdp),
                                "DOM.getBoxModel",
                                json!({ "backendNodeId": backend }),
                            )
                            .await
                        {
                            Ok(v) => v,
                            Err(_) => return BrowserRefusal::new(
                                BrowserRefusalCode::BrowserRefStale,
                                "the ref's node has no layout box — it left the DOM or is hidden",
                            )
                            .to_tool_result(),
                        };
                        let Some(point) = quad_center(&box_model) else {
                            return BrowserRefusal::new(
                                BrowserRefusalCode::BrowserRefStale,
                                "the ref's node returned an unusable layout box",
                            )
                            .to_tool_result();
                        };
                        let (hit, probe) = hit_test(conn, cdp, backend, point, &box_model).await;
                        let named = ext_ref.as_deref().unwrap_or("the ref");
                        let blocked = match hit {
                            Hit::Receives => break point,
                            Hit::Gone => {
                                return BrowserRefusal::new(
                                    BrowserRefusalCode::BrowserRefStale,
                                    "the ref's node left the page before the click",
                                )
                                .to_tool_result()
                            }
                            blocked => blocked,
                        };
                        if attempt == 0 {
                            attempt += 1;
                            tokio::time::sleep(HIT_TEST_RETRY).await;
                            continue;
                        }
                        // Name what is on top by the ref the session holds for
                        // it: a ref is something the caller already has. What the
                        // element says is page content, which only a read tells.
                        let on_top = match (&blocked, &probe, &ref_frame) {
                            (Hit::Covered { .. } | Hit::Container, Some(probe), Some(frame)) => {
                                match probe.element_on_top(conn, cdp).await {
                                    Some(covering) => self.engine.store.ref_of_node(
                                        &session, &target_id, &tab_id, frame, covering,
                                    ),
                                    None => None,
                                }
                            }
                            _ => None,
                        };
                        let by = match (&blocked, &on_top) {
                            (
                                Hit::Covered {
                                    own_indicator: true,
                                },
                                _,
                            ) => "Cua's own \"working in this tab\" pill".to_owned(),
                            (_, Some(reference)) => reference.clone(),
                            _ => "an element that has no ref in the outline you hold (read the \
                              page again to see it)"
                                .to_owned(),
                        };
                        let refusal = match blocked {
                            Hit::Covered { .. } => BrowserRefusal::new(
                                BrowserRefusalCode::BrowserTargetCovered,
                                format!(
                                "{named} is covered at its centre by {by}: a click there would \
                                 go to that element, so none was sent. Deal with what covers it \
                                 (close it or scroll it away), or act on it by its own ref"
                            ),
                            ),
                            Hit::Container => BrowserRefusal::new(
                                BrowserRefusalCode::BrowserTargetCovered,
                                format!(
                                    "{named} takes no click at its centre: the element around it, \
                                 {by}, would receive it (the ref's element takes no pointer \
                                 input there, or is drawn elsewhere), so none was sent"
                                ),
                            ),
                            Hit::Outside => BrowserRefusal::new(
                                BrowserRefusalCode::BrowserTargetCovered,
                                format!(
                                    "the centre of {named} is outside the visible page even after \
                                 scrolling to it, so no click was sent"
                                ),
                            ),
                            _ => BrowserRefusal::new(
                                BrowserRefusalCode::BrowserActionUnavailable,
                                format!(
                                    "the page did not say which element a click at the centre of \
                                 {named} would reach, so none was sent; input_route \
                                 \"dom_event\" clicks the element itself"
                                ),
                            ),
                        };
                        return refusal
                            .with_detail(json!({ "covered_by_ref": on_top, "click_sent": false }))
                            .to_tool_result();
                    }
                }
                (None, Some(pt)) => pt,
                (None, None) => unreachable!("validated above"),
            };

        self.engine
            .visualize_browser_action(
                &session,
                &validated,
                cdp,
                x,
                y,
                BrowserVisualActionKind::Click,
            )
            .await;

        if let Err(error) = conn
            .call(
                Some(cdp),
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
            )
            .await
        {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                format!(
                    "the target tab could not enter CDP focus emulation for trusted input: {error}"
                ),
            )
            .to_tool_result();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let mut delivery_error = None;
        let mut opened = None;
        for (event_type, click_count) in [("mousePressed", 1), ("mouseReleased", 1)] {
            match self
                .engine
                .call_until_dialog(
                    conn,
                    cdp,
                    cdp_target,
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": x,
                        "y": y,
                        "button": "left",
                        "clickCount": click_count,
                    }),
                )
                .await
            {
                Ok(_) => {}
                // The page's handler opened a dialog: the event was delivered,
                // and its reply only comes once the dialog is resolved.
                Err(CallStopped::Dialog(dialog)) => {
                    opened = Some(Settled::Dialog(dialog));
                    break;
                }
                Err(CallStopped::Failed(error)) => {
                    delivery_error = Some(error);
                    break;
                }
            }
        }
        // With a dialog up the page answers nothing, this included: the
        // emulation ends with the attachment session instead.
        let cleanup_error = if opened.is_some() {
            None
        } else {
            conn.call(
                Some(cdp),
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": false }),
            )
            .await
            .err()
        };
        if let Some(error) = delivery_error {
            // Trusted input is the contract; we never silently fall back to
            // synthetic events. Focus emulation has already been unwound.
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                format!(
                    "trusted Input route failed ({error}) — re-run with \
                     input_route=\"dom_event\" to explicitly request a synthetic click"
                ),
            )
            .to_tool_result();
        }
        if let Some(error) = cleanup_error {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                format!(
                    "trusted click was acknowledged but CDP focus emulation could not be restored ({error}); delivery is unknown and must not be retried automatically"
                ),
            )
            .with_detail(json!({ "delivery": "unknown", "retryable": false }))
            .to_tool_result();
        }
        let changes = page_changes_after(
            &self.engine,
            &self.registry,
            &args,
            &target_id,
            &tab_id,
            &validated,
            held,
            opened,
            watch,
        )
        .await;
        with_changes(
            ToolResult::text(format!("clicked ({x:.0}, {y:.0}) in {tab_id}")).with_structured(
                json!({
                    "status": "ok",
                    "route": "trusted",
                    "target_id": target_id,
                    "tab_id": tab_id,
                    "ref": ext_ref,
                    "frame": frame_kind,
                    "x": x,
                    "y": y,
                }),
            ),
            changes,
        )
    }
}

/// How long a covered target is given before it is looked at once more.
const HIT_TEST_RETRY: std::time::Duration = std::time::Duration::from_millis(150);

/// What is on top at a ref's click point, as the page reports it. Run on the
/// ref's node; `x`, `y` are the click point and `bx`, `by` the top-left of
/// the node's border box, both in the coordinates the click is sent in. The
/// node's own rectangle gives the same corner in its document's coordinates,
/// so the point is found there whatever frame the node is in. The walk goes
/// down through open shadow roots and through the (open or closed) roots the
/// node itself lives in, and up through shadow hosts when it asks whether
/// one element contains another. With `element` it returns the element on
/// top itself instead of the facts about it.
const HIT_TEST: &str = "function(x, y, bx, by, element) { \
    const target = this.nodeType === 1 ? this : this.parentElement; \
    if (!target || !target.isConnected) return element ? null : { connected: false }; \
    const rect = target.getBoundingClientRect(); \
    const px = x - bx + rect.left, py = y - by + rect.top; \
    const own = new Map(); \
    for (let root = target.getRootNode(); root && root.host; root = root.host.getRootNode()) \
        own.set(root.host, root); \
    let hit = target.ownerDocument.elementFromPoint(px, py); \
    while (hit) { \
        const root = hit.shadowRoot || own.get(hit); \
        const inner = root ? root.elementFromPoint(px, py) : null; \
        if (!inner || inner === hit) break; \
        hit = inner; \
    } \
    if (element) return hit; \
    if (!hit) return { connected: true, hit: false }; \
    const holds = (outer, node) => { \
        for (let n = node; n; n = n.parentNode || n.host) if (n === outer) return true; \
        return false; \
    }; \
    const label = hit.closest ? hit.closest('label') : null; \
    return { connected: true, hit: true, \
        inside_target: holds(target, hit), \
        contains_target: holds(hit, target), \
        label_of_target: !!label && (label.control === target || holds(label, target) \
            || (!!target.labels && Array.prototype.includes.call(target.labels, label))), \
        own_indicator: hit.id === 'cua-driver-indicator' }; \
}";

/// Who would receive a trusted click at a ref's click point.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hit {
    /// The ref's element: itself, something inside it, or its label.
    Receives,
    /// Another element is on top. `own_indicator`: it is the extension's
    /// own notice (see `semantic::CUA_INDICATOR_HOST_ID`).
    Covered { own_indicator: bool },
    /// An element around the ref's element: the ref's element takes no
    /// pointer input at that point, so its own handlers would not run.
    Container,
    /// Nothing: the point is outside the visible page.
    Outside,
    /// The ref's node is no longer in the page.
    Gone,
    /// The page did not answer the question: nothing is proven.
    Unknown,
}

/// Read the facts [`HIT_TEST`] returned. Only a positive answer lets a click
/// through.
fn classify_hit(facts: &Value) -> Hit {
    let flag = |name: &str| facts.get(name).and_then(Value::as_bool);
    match (flag("connected"), flag("hit")) {
        (Some(false), _) => return Hit::Gone,
        (Some(true), Some(false)) => return Hit::Outside,
        (Some(true), Some(true)) => {}
        _ => return Hit::Unknown,
    }
    match (
        flag("inside_target"),
        flag("label_of_target"),
        flag("contains_target"),
    ) {
        (Some(true), _, _) | (_, Some(true), _) => Hit::Receives,
        (Some(false), Some(false), Some(true)) => Hit::Container,
        (Some(false), Some(false), Some(false)) => Hit::Covered {
            own_indicator: flag("own_indicator") == Some(true),
        },
        _ => Hit::Unknown,
    }
}

/// One hit-test's node and arguments, kept to ask which element is on top.
struct HitProbe {
    object_id: String,
    arguments: Vec<Value>,
}

impl HitProbe {
    async fn ask(&self, conn: &CdpConnection, cdp: &str, element: bool) -> Option<Value> {
        let mut arguments = self.arguments.clone();
        arguments.push(json!({ "value": element }));
        conn.call(
            Some(cdp),
            "Runtime.callFunctionOn",
            json!({
                "objectId": self.object_id,
                "functionDeclaration": HIT_TEST,
                "arguments": arguments,
                "returnByValue": !element,
            }),
        )
        .await
        .ok()
    }

    /// The node id of the element on top at the click point.
    async fn element_on_top(&self, conn: &CdpConnection, cdp: &str) -> Option<i64> {
        let answer = self.ask(conn, cdp, true).await?;
        let object_id = answer.pointer("/result/objectId")?.as_str()?;
        conn.call(
            Some(cdp),
            "DOM.describeNode",
            json!({ "objectId": object_id }),
        )
        .await
        .ok()?
        .pointer("/node/backendNodeId")?
        .as_i64()
    }
}

/// Ask the page what is on top at the click point of a ref's node.
async fn hit_test(
    conn: &CdpConnection,
    cdp: &str,
    backend_node_id: i64,
    (x, y): (f64, f64),
    box_model: &Value,
) -> (Hit, Option<HitProbe>) {
    // The border box's top-left: the corner the node's own rectangle names.
    let corner = box_model
        .pointer("/model/border")
        .and_then(Value::as_array)
        .map(|quad| quad.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
        .filter(|quad| quad.len() == 8)
        .map(|quad| {
            (
                quad.iter()
                    .step_by(2)
                    .copied()
                    .fold(f64::INFINITY, f64::min),
                quad.iter()
                    .skip(1)
                    .step_by(2)
                    .copied()
                    .fold(f64::INFINITY, f64::min),
            )
        });
    let Some((bx, by)) = corner else {
        return (Hit::Unknown, None);
    };
    let Some(object_id) = conn
        .call(
            Some(cdp),
            "DOM.resolveNode",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await
        .ok()
        .and_then(|resolved| {
            resolved
                .pointer("/object/objectId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    else {
        return (Hit::Gone, None);
    };
    let probe = HitProbe {
        object_id,
        arguments: [x, y, bx, by]
            .into_iter()
            .map(|value| json!({ "value": value }))
            .collect(),
    };
    let hit = match probe.ask(conn, cdp, false).await {
        Some(answer) => classify_hit(answer.pointer("/result/value").unwrap_or(&Value::Null)),
        None => Hit::Unknown,
    };
    (hit, Some(probe))
}

/// Center of the content quad from a `DOM.getBoxModel` result.
fn quad_center(box_model: &Value) -> Option<(f64, f64)> {
    let quad = box_model.get("model")?.get("content")?.as_array()?;
    if quad.len() < 8 {
        return None;
    }
    let nums: Vec<f64> = quad.iter().filter_map(Value::as_f64).collect();
    if nums.len() < 8 {
        return None;
    }
    let xs = [nums[0], nums[2], nums[4], nums[6]];
    let ys = [nums[1], nums[3], nums[5], nums[7]];
    Some((xs.iter().sum::<f64>() / 4.0, ys.iter().sum::<f64>() / 4.0))
}

// ── browser_type ─────────────────────────────────────────────────────────────

/// Editability check evaluated ON the ref's node (`this`), so focus is
/// verified against the node's own root — Document or ShadowRoot — and
/// behaves the same in the main frame, composed shadow DOM,
/// same-process iframes, and OOPIF child documents.
const EDITABLE_AND_FOCUSED_CHECK: &str = "function() { \
    const root = this.getRootNode(); \
    const active = ('activeElement' in root) ? root.activeElement : null; \
    if (active !== this) return false; \
    if (this.isContentEditable) return true; \
    if (this.tagName === 'TEXTAREA') return !this.disabled && !this.readOnly; \
    if (this.tagName !== 'INPUT') return false; \
    return !this.disabled && !this.readOnly && \
        !['button','checkbox','color','file','hidden','image','radio','range','reset','submit']\
        .includes((this.type || 'text').toLowerCase()); \
}";

const FOCUS_EMULATION_READY_CHECK: &str = "function() { \
    const root = this.getRootNode(); \
    const active = ('activeElement' in root) ? root.activeElement : null; \
    return document.hasFocus() && active === this; \
}";

// What an editable node holds, read on the node itself: an input's or
// textarea's value and selection, or a contenteditable's text.
const READ_EDIT_STATE: &str = "function() { \
    const field = this.tagName === 'INPUT' || this.tagName === 'TEXTAREA'; \
    let start = null, end = null; \
    if (field) { try { start = this.selectionStart; end = this.selectionEnd; } catch (e) {} } \
    return { value: field ? String(this.value) : String(this.innerText || ''), start: start, \
        end: end, field: field, connected: this.isConnected, \
        password: this.tagName === 'INPUT' && (this.type || '').toLowerCase() === 'password' }; \
}";

// Set an input's or textarea's value the way frameworks observe: through the
// prototype's native setter (a plain `el.value = x` is swallowed by React's
// value tracker), then input and change events. Uses the node's own window so
// it works inside iframes.
const SET_VALUE_WITH_EVENTS: &str = "function(value) { \
    const view = this.ownerDocument.defaultView; \
    const proto = this.tagName === 'INPUT' ? view.HTMLInputElement.prototype : \
        this.tagName === 'TEXTAREA' ? view.HTMLTextAreaElement.prototype : null; \
    if (!proto) return false; \
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(this, value); \
    this.dispatchEvent(new view.Event('input', { bubbles: true })); \
    this.dispatchEvent(new view.Event('change', { bubbles: true })); \
    return true; \
}";

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditState {
    value: String,
    /// Selection in UTF-16 code units; `None` where the input type has no
    /// selection API (email, number).
    start: Option<usize>,
    end: Option<usize>,
    /// An input or textarea, as opposed to a contenteditable element.
    field: bool,
    password: bool,
    /// Still in the live document. A page that replaced the element keeps
    /// the old node's value, which then proves nothing about the page.
    connected: bool,
}

async fn read_edit_state(conn: &CdpConnection, cdp: &str, object_id: &str) -> Option<EditState> {
    let read = conn
        .call(
            Some(cdp),
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": READ_EDIT_STATE,
                "returnByValue": true,
            }),
        )
        .await
        .ok()?;
    let state = &read["result"]["value"];
    Some(EditState {
        value: state.get("value")?.as_str()?.to_owned(),
        start: state["start"].as_u64().map(|n| n as usize),
        end: state["end"].as_u64().map(|n| n as usize),
        field: state["field"].as_bool().unwrap_or(false),
        password: state["password"].as_bool().unwrap_or(false),
        connected: state["connected"].as_bool().unwrap_or(false),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Readback {
    Confirmed(String),
    Mismatch {
        actual: String,
        expected: Option<String>,
    },
    /// The edited node left the document (the page replaced it).
    Detached,
    /// The node holds a value the edit could have produced, but the evidence
    /// cannot tell whether it did (for example it replaced a selection the
    /// driver could not see). Carries the value it holds.
    Ambiguous(String),
    Unverifiable,
}

/// What browser_type asked the node to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditMode {
    /// Insert at the caret (insert_text or keystrokes).
    Insert,
    /// Select all, then insert (replace:true).
    Replace,
    /// The native value setter (mode set_value).
    SetValue,
}

/// The one postcondition every browser_type path checks. Each mode and node
/// kind has its own rule below.
///
/// Verdict rule: `Mismatch` only when the evidence proves the edit was
/// rejected or changed (no outcome the request allows matches the value).
/// When an allowed outcome matches but the evidence cannot tell it apart
/// from another (a selection the driver cannot see), the verdict is
/// `Ambiguous`, reported as unverifiable. Contenteditable text is compared
/// after one whitespace normalization applied identically to the whole
/// candidate and the whole observed value, never to pieces of either, so a
/// space the requested text itself carries is kept.
///
/// Distinguishability rule: a confirmation must tell the accepted outcome
/// apart from the rejected one. An insertion of non-empty text that leaves
/// the raw value unchanged, or (for contenteditable) leaves it unchanged
/// after normalization, and requested contenteditable text that normalizes
/// to nothing, cannot be confirmed: those verdicts become `Ambiguous`.
fn judge_edit(before: &EditState, after: &EditState, text: &str, mode: EditMode) -> Readback {
    let verdict = judge_edit_evidence(before, after, text, mode);
    let Readback::Confirmed(actual) = verdict else {
        return verdict;
    };
    let unchanged = mode == EditMode::Insert
        && !text.is_empty()
        && (after.value == before.value
            || (!before.field
                && normalize_rendered(&after.value) == normalize_rendered(&before.value)));
    let erased_by_normalization = !before.field
        && !text.is_empty()
        && normalize_rendered(text).is_empty()
        && after.value != text;
    if unchanged || erased_by_normalization {
        Readback::Ambiguous(actual)
    } else {
        Readback::Confirmed(actual)
    }
}

/// The postcondition per mode and node kind, before the distinguishability
/// rule in [`judge_edit`].
fn judge_edit_evidence(
    before: &EditState,
    after: &EditState,
    text: &str,
    mode: EditMode,
) -> Readback {
    if !after.connected {
        return Readback::Detached;
    }
    let actual = after.value.clone();
    let mismatch = |expected: Option<String>| Readback::Mismatch {
        actual: actual.clone(),
        expected,
    };
    match (mode, before.field) {
        // An input or textarea replaced or set holds exactly the text.
        (EditMode::Replace | EditMode::SetValue, true) => {
            if after.value == text {
                Readback::Confirmed(actual)
            } else {
                mismatch(Some(text.to_owned()))
            }
        }
        (EditMode::Replace | EditMode::SetValue, false) => {
            if same_rendered_text(&after.value, text) {
                Readback::Confirmed(actual)
            } else {
                mismatch(None)
            }
        }
        (EditMode::Insert, true) => match inserted_at_selection(before, text) {
            Some(expected) if after.value == expected => Readback::Confirmed(actual),
            Some(expected) => mismatch(Some(expected)),
            None => match field_splice(&before.value, &after.value, text) {
                Some(Splice::Inserted) => Readback::Confirmed(actual),
                Some(Splice::ReplacedRange) => Readback::Ambiguous(actual),
                None => mismatch(None),
            },
        },
        (EditMode::Insert, false) => match editable_splice(&before.value, &after.value, text) {
            Some(Splice::Inserted) => Readback::Confirmed(actual),
            Some(Splice::ReplacedRange) => Readback::Ambiguous(actual),
            None if editable_splice_search_bounded(&before.value) => mismatch(None),
            None => Readback::Ambiguous(actual),
        },
    }
}

/// How the typed text sits in the new value relative to the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Splice {
    /// Old value with the text inserted at one position.
    Inserted,
    /// Old value with one non-empty range replaced by the text.
    ReplacedRange,
}

/// Input or textarea with a known selection: the text replaces the selection
/// (UTF-16 offsets). `None` when the selection is unknown (email and number
/// inputs have no selection API, yet the caret can sit anywhere).
fn inserted_at_selection(before: &EditState, text: &str) -> Option<String> {
    let units: Vec<u16> = before.value.encode_utf16().collect();
    let (start, end) = match (before.start, before.end) {
        (Some(start), Some(end)) if start <= end && end <= units.len() => (start, end),
        _ => return None,
    };
    let mut expected = String::from_utf16_lossy(&units[..start]);
    expected.push_str(text);
    expected.push_str(&String::from_utf16_lossy(&units[end..]));
    Some(expected)
}

/// Exact values (input, textarea): is `after` the old value with `text` put
/// in at one position, replacing nothing or one range?
fn field_splice(before: &str, after: &str, text: &str) -> Option<Splice> {
    let (before, after, text): (Vec<char>, Vec<char>, Vec<char>) = (
        before.chars().collect(),
        after.chars().collect(),
        text.chars().collect(),
    );
    // Characters of the old value the edit removed.
    let removed = (before.len() + text.len()).checked_sub(after.len())?;
    if removed > before.len() {
        return None;
    }
    (0..=before.len() - removed)
        .find(|&at| {
            after[..at] == before[..at]
                && after[at..at + text.len()] == text[..]
                && after[at + text.len()..] == before[at + removed..]
        })
        .map(|_| {
            if removed == 0 {
                Splice::Inserted
            } else {
                Splice::ReplacedRange
            }
        })
}

/// Old values longer than this skip the range-replacement search for
/// contenteditable (it is cubic); an unmatched edit is then ambiguous.
const EDITABLE_RANGE_SEARCH_MAX_CHARS: usize = 200;
/// Old values longer than this skip the contenteditable search entirely.
const EDITABLE_INSERT_SEARCH_MAX_CHARS: usize = 4000;

fn editable_splice_search_bounded(before: &str) -> bool {
    before.chars().count() <= EDITABLE_RANGE_SEARCH_MAX_CHARS
}

/// Rendered text (contenteditable): build each candidate from the raw old
/// value and the raw typed text, then compare it with the observed value
/// under the same normalization.
fn editable_splice(before: &str, after: &str, text: &str) -> Option<Splice> {
    let chars: Vec<char> = before.chars().collect();
    if chars.len() > EDITABLE_INSERT_SEARCH_MAX_CHARS {
        return None;
    }
    let observed = normalize_rendered(after);
    let candidate = |start: usize, end: usize| {
        let mut value: String = chars[..start].iter().collect();
        value.push_str(text);
        value.extend(&chars[end..]);
        normalize_rendered(&value) == observed
    };
    if (0..=chars.len()).any(|at| candidate(at, at)) {
        return Some(Splice::Inserted);
    }
    if chars.len() <= EDITABLE_RANGE_SEARCH_MAX_CHARS
        && (0..chars.len()).any(|start| (start + 1..=chars.len()).any(|end| candidate(start, end)))
    {
        return Some(Splice::ReplacedRange);
    }
    None
}

/// A contenteditable replaced with `text` shows that text as rendered.
fn same_rendered_text(after: &str, text: &str) -> bool {
    normalize_rendered(after) == normalize_rendered(text)
}

/// Rendered-text normalization, applied to whole values only: whitespace runs
/// (including the no-break spaces editors use for typed spaces) become one
/// space, and the ends are trimmed.
fn normalize_rendered(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out.trim().to_owned()
}

/// Read the node back until it holds what the input should have produced,
/// for up to half a second (frameworks may re-render after the input event).
async fn await_edit_readback(
    conn: &CdpConnection,
    cdp: &str,
    object_id: &str,
    before: &EditState,
    text: &str,
    mode: EditMode,
) -> Readback {
    let mut judged = Readback::Unverifiable;
    for attempt in 0..10 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let Some(after) = read_edit_state(conn, cdp, object_id).await else {
            return judged;
        };
        judged = judge_edit(before, &after, text, mode);
        if matches!(judged, Readback::Confirmed(_) | Readback::Detached) {
            break;
        }
    }
    judged
}

const SHOWN_VALUE_CHARS: usize = 200;

fn truncate_value(value: &str) -> String {
    if value.chars().count() <= SHOWN_VALUE_CHARS {
        value.to_owned()
    } else {
        let mut shown: String = value.chars().take(SHOWN_VALUE_CHARS).collect();
        shown.push('…');
        shown
    }
}

/// A value for a message: quoted, shortened, never a password.
fn shown_value(value: &str, password: bool) -> String {
    if password {
        format!(
            "{} character(s) (a password; not shown)",
            value.chars().count()
        )
    } else {
        serde_json::to_string(&truncate_value(value)).unwrap_or_default()
    }
}

// Select the element's whole content so the next insertion replaces it instead
// of appending. Returns the number of characters selected, or -1 when the node
// is not a shape we know how to select. Input types without a selection API
// (email, number) are selected with select(), which cannot be confirmed here;
// the read-back after typing shows whether the value was replaced. Selection is the only clearing path
// that keeps input semantics intact: assigning `value` directly would skip the
// beforeinput/input events that frameworks bind their state to.
const SELECT_ALL_IN_ELEMENT: &str = "function() { \
    if (this.tagName === 'INPUT' || this.tagName === 'TEXTAREA') { \
        const value = this.value || ''; \
        try { this.setSelectionRange(0, value.length); } catch (e) { \
            try { this.select(); } catch (_) { return -1; } \
            if (this.selectionStart === null) return Array.from(value).length; \
        } \
        if (this.selectionStart !== 0 || this.selectionEnd !== value.length) return -1; \
        return Array.from(value).length; \
    } \
    if (this.isContentEditable) { \
        const root = this.getRootNode(); \
        const owner = ('getSelection' in root) ? root : this.ownerDocument; \
        const selection = owner.getSelection(); \
        if (!selection) return -1; \
        const range = this.ownerDocument.createRange(); \
        range.selectNodeContents(this); \
        selection.removeAllRanges(); \
        selection.addRange(range); \
        return Array.from(this.textContent || '').length; \
    } \
    return -1; \
}";

/// Put the element's entire content into the selection.
///
/// `browser_type` delivers through `Input.insertText` and the trusted key path,
/// and both insert at the caret. Without an explicit selection there is no way
/// to *set* a field that already holds text — a caller who tries ends up with
/// the old and the new value concatenated. Both delivery paths replace the
/// current selection, so selecting everything first turns "insert" into "set".
async fn select_all_in_element(
    conn: &CdpConnection,
    cdp: &str,
    object_id: &str,
) -> Result<usize, String> {
    let selected = conn
        .call(
            Some(cdp),
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": SELECT_ALL_IN_ELEMENT,
                "returnByValue": true,
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
    match selected["result"]["value"].as_i64() {
        Some(n) if n >= 0 => Ok(n as usize),
        _ => Err("the ref is not a text input, textarea, or contenteditable \
                  element, so its content cannot be selected for replacement"
            .into()),
    }
}

/// Establish Chromium's trusted-input focus state for an inactive tab.
///
/// Selection alone is not durable in a fully occluded window: without focus
/// emulation Chromium can accept `Input.insertText` while discarding the
/// selection, silently turning replacement back into append.
async fn enter_focus_emulation(
    conn: &CdpConnection,
    cdp: &str,
    backend_node_id: i64,
    object_id: &str,
) -> Result<(), String> {
    conn.call(
        Some(cdp),
        "Emulation.setFocusEmulationEnabled",
        json!({ "enabled": true }),
    )
    .await
    .map_err(|error| error.to_string())?;

    let mut focus_error = None;
    for _ in 0..20 {
        if let Err(error) = conn
            .call(
                Some(cdp),
                "DOM.focus",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await
        {
            focus_error = Some(error.to_string());
            break;
        }
        let ready = conn
            .call(
                Some(cdp),
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": FOCUS_EMULATION_READY_CHECK,
                    "returnByValue": true,
                }),
            )
            .await;
        if matches!(
            ready,
            Ok(ref value) if value["result"]["value"].as_bool() == Some(true)
        ) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Err(error) = conn
                .call(
                    Some(cdp),
                    "DOM.focus",
                    json!({ "backendNodeId": backend_node_id }),
                )
                .await
            {
                focus_error = Some(error.to_string());
                break;
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let _ = conn
        .call(
            Some(cdp),
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": false }),
        )
        .await;
    Err(format!(
        "the exact editable ref did not become focus-ready under CDP emulation{}",
        focus_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    ))
}

pub struct BrowserTypeTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
    registry: ReplayRegistrySlot,
}

impl BrowserTypeTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self::with_registry(engine, no_registry())
    }

    /// `registry` is what the read after typing is dispatched through.
    pub fn with_registry(engine: Arc<BrowserEngine>, registry: ReplayRegistrySlot) -> Self {
        let def = ToolDef {
            name: "browser_type".into(),
            description: "Type text into an editable page ref of a bound tab. Appends at the \
                caret unless replace:true, which replaces the content (empty text clears it). \
                Reads the field back: confirmed, or an error with the value it holds; changes \
                has what else the page changed. Refused for heuristic bindings."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target_id": schema_target_id(),
                    "tab_id": schema_tab_id(),
                    "session": schema_session(),
                    "text": { "type": "string", "description": "Text to type." },
                    "ref": schema_ref(),
                    "mode": {
                        "type": "string",
                        "enum": ["insert_text", "keystrokes", "set_value"],
                        "default": "insert_text",
                        "description": "insert_text: one bulk insert. keystrokes: per-character key events. set_value: set an input or textarea through its native value setter and fire input and change (React-safe; replaces the value)."
                    },
                    "replace": {
                        "type": "boolean",
                        "default": false,
                        "description": "Select the whole content first so the text replaces it."
                    },
                },
                "required": ["target_id", "tab_id", "ref", "text"],
                "additionalProperties": true
            }),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: true,
        };
        Self {
            def,
            engine,
            registry,
        }
    }
}

#[async_trait]
impl Tool for BrowserTypeTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

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
            browser_protected_resource_scope(&self.engine, args, "browser_type").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (target_id, tab_id, text) = match (
            args.require_str("target_id"),
            args.require_str("tab_id"),
            args.require_str("text"),
        ) {
            (Ok(t), Ok(tab), Ok(x)) => (t, tab, x),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        let session = match require_explicit_session(&args) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mode = args.opt_str("mode").unwrap_or_else(|| "insert_text".into());
        if !matches!(mode.as_str(), "insert_text" | "keystrokes" | "set_value") {
            return ToolResult::error(format!(
                "mode must be \"insert_text\", \"keystrokes\", or \"set_value\", got {mode:?}"
            ));
        }
        let replace = args.opt_bool("replace").unwrap_or(false);

        let _mutation = match self
            .engine
            .lock_mutation(&session, &target_id, &tab_id)
            .await
        {
            Ok(guard) => guard,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let validated = match self
            .engine
            .revalidate_for_mutation(&session, &target_id, Some(&tab_id))
            .await
        {
            Ok(v) => v,
            Err(refusal) => return refusal.to_tool_result(),
        };

        let held = self.engine.held_view(&session, &target_id, &tab_id);
        let cdp_target = validated.tab.cdp_target_id.as_str();
        self.engine
            .watch_dialogs(&validated.conn, &validated.cdp_session, cdp_target)
            .await;
        if let Some(dialog) = validated.conn.dialog_state(cdp_target) {
            return dialog_open_refusal(&dialog).to_tool_result();
        }
        // From before the input: a navigation it sets off is seen starting.
        let watch = self.engine.page_watch(&validated).await;

        let ext_ref = match args.require_str("ref") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let entry = match self
            .engine
            .store
            .resolve_ref(&session, &target_id, &tab_id, &ext_ref)
        {
            Ok(e) => e,
            Err(refusal) => return refusal.to_tool_result(),
        };
        if entry.semantic && !entry.actions.contains(&BrowserActionKind::Type) {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                format!("semantic ref {ext_ref} does not declare the type action"),
            )
            .to_tool_result();
        }
        // Re-prove the ref's frame identity; typing routes to the frame's
        // own session (tab, or the contained OOPIF child session).
        let cdp_session = match self
            .engine
            .frame_session_for_mutation(&session, &target_id, &tab_id, &validated, &entry)
            .await
        {
            Ok(s) => s,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let conn = &validated.conn;
        let cdp = cdp_session.as_str();

        if let Err(_e) = conn
            .call(
                Some(cdp),
                "DOM.focus",
                json!({ "backendNodeId": entry.backend_node_id }),
            )
            .await
        {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserRefStale,
                "the ref's node can no longer be focused — re-snapshot the tab",
            )
            .to_tool_result();
        }
        // Frame- and shadow-aware editability check: evaluated on the
        // ref's own node so it works identically for the main document,
        // shadow roots (getRootNode().activeElement), same-process
        // iframes, and OOPIF child documents.
        let object_id = match conn
            .call(
                Some(cdp),
                "DOM.resolveNode",
                json!({ "backendNodeId": entry.backend_node_id }),
            )
            .await
            .ok()
            .and_then(|resolved| {
                resolved
                    .get("object")
                    .and_then(|o| o.get("objectId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }) {
            Some(o) => o,
            None => {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserRefStale,
                    "the ref's node no longer resolves in the live page",
                )
                .to_tool_result()
            }
        };
        let editable = conn
            .call(
                Some(cdp),
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": EDITABLE_AND_FOCUSED_CHECK,
                    "returnByValue": true,
                }),
            )
            .await;
        if !matches!(
            editable,
            Ok(ref value) if value["result"]["value"].as_bool() == Some(true)
        ) {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                "the requested ref is not a focused editable element",
            )
            .to_tool_result();
        }

        // Give the recording a visual target before text delivery. Keep this
        // best-effort: editability/input semantics never depend on the overlay.
        let _ = conn
            .call(
                Some(cdp),
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": entry.backend_node_id }),
            )
            .await;
        if let Ok(box_model) = conn
            .call(
                Some(cdp),
                "DOM.getBoxModel",
                json!({ "backendNodeId": entry.backend_node_id }),
            )
            .await
        {
            if let Some((x, y)) = quad_center(&box_model) {
                self.engine
                    .visualize_browser_action(
                        &session,
                        &validated,
                        cdp,
                        x,
                        y,
                        BrowserVisualActionKind::Type,
                    )
                    .await;
            }
        }

        let requested_chars = text.chars().count();
        let mut replaced_chars = 0usize;
        // What the field held before any input, for the read-back below.
        // Keystrokes re-read it after their focus preparation (see there).
        let mut before = read_edit_state(conn, cdp, &object_id).await;
        let replaces = replace || mode == "set_value";
        let edit_mode = if mode == "set_value" {
            EditMode::SetValue
        } else if replace {
            EditMode::Replace
        } else {
            EditMode::Insert
        };
        // The calls that put the text into the page. A handler there may open
        // a JavaScript dialog, and a page behind one answers nothing: stop
        // sending and say so, instead of waiting out every later call.
        let mut delivery = Delivery {
            engine: &self.engine,
            conn,
            cdp,
            target: cdp_target,
            opened: None,
        };
        let (typed, delivered_chars) = if mode == "set_value" {
            match delivery
                .send(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": SET_VALUE_WITH_EVENTS,
                        "arguments": [{ "value": text }],
                        "returnByValue": true,
                    }),
                )
                .await
            {
                // Not sent at all: a dialog was already up.
                Ok(None) => (Ok(()), 0),
                // Sent, and the page opened a dialog while it handled it.
                Ok(Some(_)) if delivery.opened.is_some() => (Ok(()), requested_chars),
                Ok(Some(value)) if value["result"]["value"].as_bool() == Some(true) => {
                    replaced_chars = before
                        .as_ref()
                        .map_or(0, |state| state.value.chars().count());
                    (Ok(()), requested_chars)
                }
                Ok(Some(_)) => {
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserActionUnavailable,
                        "mode set_value needs an input or textarea ref; use insert_text or \
                         keystrokes for other editable elements",
                    )
                    .to_tool_result()
                }
                Err(error) => (Err(error), 0),
            }
        } else if mode == "insert_text" {
            if replace {
                if let Err(detail) =
                    enter_focus_emulation(conn, cdp, entry.backend_node_id, &object_id).await
                {
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserInputTrustUnavailable,
                        format!(
                            "the inactive tab could not prepare trusted replacement input: {detail}"
                        ),
                    )
                    .to_tool_result();
                }
                match select_all_in_element(conn, cdp, &object_id).await {
                    Ok(n) => replaced_chars = n,
                    Err(detail) => {
                        let _ = conn
                            .call(
                                Some(cdp),
                                "Emulation.setFocusEmulationEnabled",
                                json!({ "enabled": false }),
                            )
                            .await;
                        return BrowserRefusal::new(
                            BrowserRefusalCode::BrowserActionUnavailable,
                            format!("replace=true could not select the ref's content: {detail}"),
                        )
                        .to_tool_result();
                    }
                }
            }
            // An empty insertText is a no-op, so "clear the field" needs an
            // explicit deletion of the selection we just made. Delete keeps the
            // same event path as typing; it is not a value assignment.
            let mut call = if replace && text.is_empty() {
                if replaced_chars == 0 {
                    Ok(Some(json!({})))
                } else {
                    match delivery
                        .send(
                            "Input.dispatchKeyEvent",
                            json!({
                                "type": "keyDown",
                                "key": "Delete",
                                "code": "Delete",
                                "windowsVirtualKeyCode": 46,
                                "nativeVirtualKeyCode": 46,
                            }),
                        )
                        .await
                    {
                        Ok(None) => Ok(None),
                        Ok(Some(_)) => {
                            delivery
                                .send(
                                    "Input.dispatchKeyEvent",
                                    json!({
                                        "type": "keyUp",
                                        "key": "Delete",
                                        "code": "Delete",
                                        "windowsVirtualKeyCode": 46,
                                        "nativeVirtualKeyCode": 46,
                                    }),
                                )
                                .await
                        }
                        Err(error) => Err(error),
                    }
                }
            } else {
                delivery
                    .send("Input.insertText", json!({ "text": text }))
                    .await
            };
            if replace && !delivery.blocked() {
                if let Err(error) = conn
                    .call(
                        Some(cdp),
                        "Emulation.setFocusEmulationEnabled",
                        json!({ "enabled": false }),
                    )
                    .await
                {
                    call = Err(error);
                }
            }
            match call {
                Ok(Some(_)) => (Ok(()), requested_chars),
                // Nothing was sent: a dialog was already up.
                Ok(None) => (Ok(()), 0),
                Err(error) => (Err(error), 0),
            }
        } else {
            if let Err(error) = conn
                .call(
                    Some(cdp),
                    "Emulation.setFocusEmulationEnabled",
                    json!({ "enabled": true }),
                )
                .await
            {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserInputTrustUnavailable,
                    format!(
                        "the inactive tab could not enter CDP focus emulation for trusted keystrokes: {error}"
                    ),
                )
                .to_tool_result();
            }
            let mut focus_ready = false;
            let mut focus_error = None;
            for _ in 0..20 {
                if let Err(error) = conn
                    .call(
                        Some(cdp),
                        "DOM.focus",
                        json!({ "backendNodeId": entry.backend_node_id }),
                    )
                    .await
                {
                    focus_error = Some(error);
                    break;
                }
                let ready = conn
                    .call(
                        Some(cdp),
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": FOCUS_EMULATION_READY_CHECK,
                            "returnByValue": true,
                        }),
                    )
                    .await;
                if matches!(
                    ready,
                    Ok(ref value) if value["result"]["value"].as_bool() == Some(true)
                ) {
                    focus_ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            if !focus_ready {
                let _ = conn
                    .call(
                        Some(cdp),
                        "Emulation.setFocusEmulationEnabled",
                        json!({ "enabled": false }),
                    )
                    .await;
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserInputTrustUnavailable,
                    format!(
                        "the exact editable ref did not become focus-ready under CDP emulation{}",
                        focus_error
                            .as_ref()
                            .map(|error| format!(": {error}"))
                            .unwrap_or_default()
                    ),
                )
                .to_tool_result();
            }
            // Chromium acknowledges focus emulation before every renderer's
            // trusted-input path is ready. Edge on Linux can otherwise drop
            // the first one or two characters while still returning success.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Err(error) = conn
                .call(
                    Some(cdp),
                    "DOM.focus",
                    json!({ "backendNodeId": entry.backend_node_id }),
                )
                .await
            {
                let _ = conn
                    .call(
                        Some(cdp),
                        "Emulation.setFocusEmulationEnabled",
                        json!({ "enabled": false }),
                    )
                    .await;
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserRefStale,
                    format!("the exact editable ref became stale before trusted typing: {error}"),
                )
                .to_tool_result();
            }
            // Select AFTER the last DOM.focus above: re-focusing a node drops
            // the selection again, so selecting any earlier would silently
            // degrade replace=true back to append.
            if replace {
                match select_all_in_element(conn, cdp, &object_id).await {
                    Ok(n) => replaced_chars = n,
                    Err(detail) => {
                        // Leave focus emulation the way we found it, exactly as
                        // the other early returns in this branch do.
                        let _ = conn
                            .call(
                                Some(cdp),
                                "Emulation.setFocusEmulationEnabled",
                                json!({ "enabled": false }),
                            )
                            .await;
                        return BrowserRefusal::new(
                            BrowserRefusalCode::BrowserActionUnavailable,
                            format!("replace=true could not select the ref's content: {detail}"),
                        )
                        .to_tool_result();
                    }
                }
            }
            // The insertion baseline is what the field holds right before the
            // first key: focus emulation and DOM.focus can move the caret (a
            // focus handler that puts it at the end), and the read-back judges
            // the edit against this selection.
            if let Some(state) = read_edit_state(conn, cdp, &object_id).await {
                before = Some(state);
            }
            let mut result = Ok(());
            let mut delivered = 0;
            // No characters to type means the selection has to go away by
            // itself; the loop below would leave the old content selected but
            // present, and "cleared" would be a false report.
            if replace && text.is_empty() && replaced_chars > 0 {
                for phase in ["keyDown", "keyUp"] {
                    if let Err(error) = delivery
                        .send(
                            "Input.dispatchKeyEvent",
                            json!({
                                "type": phase,
                                "key": "Delete",
                                "code": "Delete",
                                "windowsVirtualKeyCode": 46,
                                "nativeVirtualKeyCode": 46,
                            }),
                        )
                        .await
                    {
                        result = Err(error);
                        break;
                    }
                }
            }
            for ch in text.chars() {
                let (key, key_text) = if ch == '\n' {
                    ("Enter".to_string(), "\r".to_string())
                } else {
                    (ch.to_string(), ch.to_string())
                };
                let down = delivery
                    .send(
                        "Input.dispatchKeyEvent",
                        json!({ "type": "keyDown", "key": key }),
                    )
                    .await;
                // The `char` event carries the text: a character counts as
                // delivered only when that event was sent. A dialog the
                // keyDown opened leaves it unsent and uncounted.
                let character = if matches!(down, Ok(Some(_))) {
                    delivery
                        .send(
                            "Input.dispatchKeyEvent",
                            json!({
                                "type": "char",
                                "key": key,
                                "text": key_text,
                                "unmodifiedText": key_text,
                            }),
                        )
                        .await
                } else {
                    Ok(None)
                };
                let sent = matches!(character, Ok(Some(_)));
                let up = if sent {
                    delivery
                        .send(
                            "Input.dispatchKeyEvent",
                            json!({ "type": "keyUp", "key": key }),
                        )
                        .await
                } else {
                    Ok(None)
                };
                if let Err(e) = down.and(character).and(up) {
                    result = Err(e);
                    break;
                }
                if sent {
                    delivered += 1;
                }
                if delivery.blocked() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
            if !delivery.blocked() {
                if let Err(error) = conn
                    .call(
                        Some(cdp),
                        "Emulation.setFocusEmulationEnabled",
                        json!({ "enabled": false }),
                    )
                    .await
                {
                    result = Err(error);
                }
            }
            (result, delivered)
        };

        // The page opened a dialog while it handled the input: the text went
        // in as far as counted, and the field cannot be read back until the
        // dialog is resolved.
        delivery.blocked();
        if let Some(dialog) = delivery.opened.take() {
            let changes = page_changes_after(
                &self.engine,
                &self.registry,
                &args,
                &target_id,
                &tab_id,
                &validated,
                held,
                Some(Settled::Dialog(dialog)),
                None,
            )
            .await;
            return with_changes(
                ToolResult::text(format!(
                    "typed {delivered_chars} of {requested_chars} char(s) into {tab_id}, and the \
                     page opened a JavaScript dialog while it handled the input, so the field \
                     could not be read back. Resolve the dialog, then read the field."
                ))
                .with_structured(json!({
                    "status": "ok",
                    "effect": "unverifiable",
                    "target_id": target_id,
                    "tab_id": tab_id,
                    "ref": ext_ref,
                    "mode": mode,
                    "requested_chars": requested_chars,
                    "delivered_chars": delivered_chars,
                    "readback": "javascript_dialog_open",
                })),
                changes,
            );
        }

        // Input was sent: from here every outcome also says what the page
        // changed, read once the page has settled.
        let outcome = 'outcome: {
            if typed.is_ok() {
                if let Some(before) = before.as_ref() {
                    match await_edit_readback(conn, cdp, &object_id, before, &text, edit_mode).await
                    {
                        Readback::Mismatch { actual, expected } => {
                            let shown = shown_value(&actual, before.password);
                            break 'outcome ToolResult::error(format!(
                                "typed {requested_chars} char(s) into {tab_id}, but the field now \
                                 holds {shown}{}: the page changed or rejected the input. Read the \
                                 page before typing again.",
                                expected
                                    .as_deref()
                                    .map(|expected| format!(
                                        " instead of {}",
                                        shown_value(expected, before.password)
                                    ))
                                    .unwrap_or_default()
                            ))
                            .with_structured(json!({
                                "code": "browser_type_mismatch",
                                "effect": "mismatch",
                                "target_id": target_id,
                                "tab_id": tab_id,
                                "ref": ext_ref,
                                "mode": mode,
                                "requested_chars": requested_chars,
                                "delivered_chars": delivered_chars,
                                "value": (!before.password).then(|| truncate_value(&actual)),
                                "expected": expected
                                    .filter(|_| !before.password)
                                    .map(|expected| truncate_value(&expected)),
                            }));
                        }
                        Readback::Confirmed(actual) => {
                            let shown = shown_value(&actual, before.password);
                            let summary = if replaces {
                                format!(
                                    "typed {requested_chars} char(s) into {tab_id}, replacing \
                                     {replaced_chars} char(s); the field now holds {shown}"
                                )
                            } else {
                                format!(
                                    "typed {requested_chars} char(s) into {tab_id}; the field now \
                                     holds {shown}"
                                )
                            };
                            break 'outcome ToolResult::text(summary).with_structured(json!({
                                "status": "ok",
                                "effect": "confirmed",
                                "evidence": [{
                                    "kind": "browser_readback",
                                    "detail": format!("the field holds {shown}"),
                                }],
                                "target_id": target_id,
                                "tab_id": tab_id,
                                "ref": ext_ref,
                                "frame": entry.frame.kind.as_str(),
                                "mode": mode,
                                "chars": requested_chars,
                                "requested_chars": requested_chars,
                                "delivered_chars": delivered_chars,
                                "replace": replaces,
                                "replaced_chars": replaced_chars,
                                "value": (!before.password).then(|| truncate_value(&actual)),
                            }));
                        }
                        Readback::Detached => {
                            break 'outcome ToolResult::text(format!(
                                "typed {requested_chars} char(s) into {tab_id}, but the page \
                                 replaced the field while it handled the input, so what it holds \
                                 now is unknown. Snapshot the tab again and read the new field."
                            ))
                            .with_structured(json!({
                                "status": "ok",
                                "effect": "unverifiable",
                                "target_id": target_id,
                                "tab_id": tab_id,
                                "ref": ext_ref,
                                "mode": mode,
                                "requested_chars": requested_chars,
                                "delivered_chars": delivered_chars,
                                "readback": "element_replaced",
                            }));
                        }
                        Readback::Ambiguous(actual) => {
                            let shown = shown_value(&actual, before.password);
                            break 'outcome ToolResult::text(format!(
                                "typed {requested_chars} char(s) into {tab_id}; the field now holds \
                                 {shown}, which the input could have produced (for example by \
                                 replacing a selection the driver could not see), but that cannot \
                                 be confirmed. Check the value before typing again."
                            ))
                            .with_structured(json!({
                                "status": "ok",
                                "effect": "unverifiable",
                                "target_id": target_id,
                                "tab_id": tab_id,
                                "ref": ext_ref,
                                "mode": mode,
                                "requested_chars": requested_chars,
                                "delivered_chars": delivered_chars,
                                "readback": "ambiguous",
                                "value": (!before.password).then(|| truncate_value(&actual)),
                            }));
                        }
                        Readback::Unverifiable => {}
                    }
                }
            }
            match typed {
                Ok(()) => ToolResult::text(if replaces {
                    format!(
                        "typed {requested_chars} char(s) into {tab_id}, replacing \
                         {replaced_chars} char(s)"
                    )
                } else {
                    format!("typed {requested_chars} char(s) into {tab_id}")
                })
                .with_structured(json!({
                    "status": "ok",
                    "target_id": target_id,
                    "tab_id": tab_id,
                    "ref": ext_ref,
                    "frame": entry.frame.kind.as_str(),
                    "mode": mode,
                    "chars": requested_chars,
                    "requested_chars": requested_chars,
                    "delivered_chars": delivered_chars,
                    // Report what was displaced, not just what was sent: a caller
                    // that asked to replace needs to distinguish "set an empty
                    // field" from "overwrote something" without re-reading the page.
                    "replace": replaces,
                    "replaced_chars": replaced_chars,
                })),
                Err(e) => BrowserRefusal::new(
                    BrowserRefusalCode::BrowserInputIncomplete,
                    format!(
                        "trusted Input typing stopped after {delivered_chars} of {requested_chars} character(s): {e}"
                    ),
                )
                .with_detail(json!({
                    "requested_chars": requested_chars,
                    "delivered_chars": delivered_chars,
                    "retryable": false,
                }))
                .to_tool_result(),
            }
        };
        let changes = page_changes_after(
            &self.engine,
            &self.registry,
            &args,
            &target_id,
            &tab_id,
            &validated,
            held,
            None,
            watch,
        )
        .await;
        with_changes(outcome, changes)
    }
}

// ── browser_dialog ──────────────────────────────────────────────────────────

pub struct BrowserDialogTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
}

impl BrowserDialogTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self {
            def: ToolDef {
                name: "browser_dialog".into(),
                description: "Inspect, accept, or dismiss a page JavaScript alert, confirm, prompt, or beforeunload dialog on a bound tab. Accept and dismiss need the dialog_id from inspect. Not for permission prompts, native dialogs, or file pickers.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target_id": schema_target_id(),
                        "tab_id": schema_tab_id(),
                        "session": schema_session(),
                        "action": { "type": "string", "enum": ["inspect", "accept", "dismiss"] },
                        "dialog_id": { "type": "string", "description": "Current dialog id from inspect." },
                        "prompt_text": { "type": "string", "description": "Answer when accepting a prompt dialog." },
                        "delivery_mode": {
                            "type": "string",
                            "enum": ["background", "foreground"],
                            "default": "background",
                            "description": "For accept or dismiss; Linux Chromium requires foreground."
                        }
                    },
                    "required": ["target_id", "tab_id", "action"],
                    "additionalProperties": true
                }),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: true,
            },
            engine,
        }
    }
}

#[async_trait]
impl Tool for BrowserDialogTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn protected_resource_ownership(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> ProtectedResourceOwnership {
        if matches!(
            adapter_id,
            "private_observation" | "browser_consequential_action"
        ) {
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
        if matches!(
            adapter_id,
            "private_observation" | "browser_consequential_action"
        ) {
            browser_protected_resource_scope(&self.engine, args, "browser_dialog").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (target_id, tab_id, action) = match (
            args.require_str("target_id"),
            args.require_str("tab_id"),
            args.require_str("action"),
        ) {
            (Ok(target), Ok(tab), Ok(action)) => (target, tab, action),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => return error,
        };
        if !matches!(action.as_str(), "inspect" | "accept" | "dismiss") {
            return ToolResult::error("action must be inspect, accept, or dismiss");
        }
        let delivery_mode = args
            .opt_str("delivery_mode")
            .unwrap_or_else(|| "background".into());
        if !matches!(delivery_mode.as_str(), "background" | "foreground") {
            return ToolResult::error("delivery_mode must be background or foreground");
        }
        let session = match require_explicit_session(&args) {
            Ok(session) => session,
            Err(error) => return error,
        };
        let _mutation = match self
            .engine
            .lock_mutation(&session, &target_id, &tab_id)
            .await
        {
            Ok(guard) => guard,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let validated = match self
            .engine
            .revalidate_for_mutation(&session, &target_id, Some(&tab_id))
            .await
        {
            Ok(validated) => validated,
            Err(refusal) => return refusal.to_tool_result(),
        };
        if cfg!(target_os = "linux") && action != "inspect" && delivery_mode == "background" {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                "Chromium's native JavaScript modal cannot be resolved on Linux without changing foreground posture; retry with delivery_mode=\"foreground\"",
            )
            .with_detail(json!({ "supported_delivery_mode": "foreground" }))
            .to_tool_result();
        }
        let conn = &validated.conn;
        let cdp_session = validated.cdp_session.as_str();
        let cdp_target_id = validated.tab.cdp_target_id.as_str();
        if conn.dialog_state(cdp_target_id).is_none() && !conn.has_dialog_session(cdp_target_id) {
            // Register before Page.enable so an opening event delivered before
            // the command reply is still attributed to the exact target.
            conn.register_dialog_session(cdp_session, cdp_target_id);
            if let Err(error) = conn.call(Some(cdp_session), "Page.enable", json!({})).await {
                if conn.dialog_state(cdp_target_id).is_none() {
                    conn.unregister_dialog_session(cdp_session, cdp_target_id);
                    return BrowserRefusal::new(
                        BrowserRefusalCode::BrowserActionUnavailable,
                        format!("the exact tab does not expose JavaScript dialog events: {error}"),
                    )
                    .to_tool_result();
                }
            }
            tokio::task::yield_now().await;
        }

        let Some(dialog) = conn.dialog_state(cdp_target_id) else {
            return if action == "inspect" {
                ToolResult::text(format!(
                    "no page-owned JavaScript dialog is open in {tab_id}"
                ))
                .with_structured(json!({
                    "status": "ok", "target_id": target_id, "tab_id": tab_id,
                    "present": false
                }))
            } else {
                BrowserRefusal::new(
                    BrowserRefusalCode::BrowserActionUnavailable,
                    "no current page-owned JavaScript dialog exists on the exact tab",
                )
                .to_tool_result()
            };
        };
        let dialog_id = format!("dialog-{}", dialog.generation);
        if action == "inspect" {
            return ToolResult::text(format!("{} dialog is open in {tab_id}", dialog.kind))
                .with_structured(json!({
                    "status": "ok", "target_id": target_id, "tab_id": tab_id,
                    "present": true, "dialog_id": dialog_id, "kind": dialog.kind
                }));
        }

        let supplied_id = match args.require_str("dialog_id") {
            Ok(id) => id,
            Err(error) => return error,
        };
        if supplied_id != dialog_id {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                "the dialog capability is stale or belongs to another dialog",
            )
            .to_tool_result();
        }
        let prompt_text = args.opt_str("prompt_text");
        if prompt_text.is_some() && (action != "accept" || dialog.kind != "prompt") {
            return ToolResult::error("prompt_text is valid only when accepting a prompt dialog");
        }
        let mut params = json!({ "accept": action == "accept" });
        if let Some(text) = prompt_text {
            params["promptText"] = Value::String(text);
        }
        match conn
            .call(
                Some(&dialog.session_id),
                "Page.handleJavaScriptDialog",
                params,
            )
            .await
        {
            Ok(_) => {
                conn.clear_dialog_state(cdp_target_id, dialog.generation);
                ToolResult::text(format!("{action}ed {} dialog in {tab_id}", dialog.kind))
                    .with_structured(json!({
                        "status": "ok", "target_id": target_id, "tab_id": tab_id,
                        "dialog_id": dialog_id, "kind": dialog.kind, "action": action
                    }))
            }
            Err(error) => BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                format!("the exact JavaScript dialog could not be resolved: {error}"),
            )
            .to_tool_result(),
        }
    }
}

// ── browser_set_input_files ─────────────────────────────────────────────────

pub struct BrowserSetInputFilesTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
}

impl BrowserSetInputFilesTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self {
            def: ToolDef {
                name: "browser_set_input_files".into(),
                description: "Set absolute local files on a live <input type=file> ref through CDP, bypassing the native picker. Symlinks and non-regular files are refused.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target_id": schema_target_id(),
                        "tab_id": schema_tab_id(),
                        "session": schema_session(),
                        "ref": schema_ref(),
                        "files": {
                            "type": "array", "minItems": 1, "maxItems": 32,
                            "items": { "type": "string", "description": "Absolute path of a regular file." }
                        }
                    },
                    "required": ["target_id", "tab_id", "ref", "files"],
                    "additionalProperties": true
                }),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: true,
            },
            engine,
        }
    }
}

fn validated_upload_paths(args: &Value) -> Result<Vec<String>, ToolResult> {
    let Some(files) = args.get("files").and_then(Value::as_array) else {
        return Err(ToolResult::error(
            "files must be a non-empty array of absolute paths",
        ));
    };
    if files.is_empty() || files.len() > 32 {
        return Err(ToolResult::error(
            "files must contain between 1 and 32 paths",
        ));
    }
    files
        .iter()
        .map(|value| {
            let raw = value
                .as_str()
                .ok_or_else(|| ToolResult::error("every files entry must be a string"))?;
            let path = std::path::Path::new(raw);
            if !path.is_absolute() {
                return Err(ToolResult::error("every upload path must be absolute"));
            }
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|_| ToolResult::error("an approved upload path does not exist"))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ToolResult::error(
                    "upload paths must name regular files directly, not links or directories",
                ));
            }
            std::fs::canonicalize(path)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(|_| ToolResult::error("an upload path could not be canonicalized"))
        })
        .collect()
}

#[async_trait]
impl Tool for BrowserSetInputFilesTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (target_id, tab_id, ext_ref) = match (
            args.require_str("target_id"),
            args.require_str("tab_id"),
            args.require_str("ref"),
        ) {
            (Ok(target), Ok(tab), Ok(ext_ref)) => (target, tab, ext_ref),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => return error,
        };
        let files = match validated_upload_paths(&args) {
            Ok(files) => files,
            Err(error) => return error,
        };
        let session = match require_explicit_session(&args) {
            Ok(session) => session,
            Err(error) => return error,
        };
        let _mutation = match self
            .engine
            .lock_mutation(&session, &target_id, &tab_id)
            .await
        {
            Ok(guard) => guard,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let validated = match self
            .engine
            .revalidate_for_mutation(&session, &target_id, Some(&tab_id))
            .await
        {
            Ok(validated) => validated,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let entry = match self
            .engine
            .store
            .resolve_ref(&session, &target_id, &tab_id, &ext_ref)
        {
            Ok(entry) => entry,
            Err(refusal) => return refusal.to_tool_result(),
        };
        if entry.semantic && !entry.actions.contains(&BrowserActionKind::Upload) {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                "the semantic ref is not a file-upload control",
            )
            .to_tool_result();
        }
        let cdp_session = match self
            .engine
            .frame_session_for_mutation(&session, &target_id, &tab_id, &validated, &entry)
            .await
        {
            Ok(session) => session,
            Err(refusal) => return refusal.to_tool_result(),
        };
        let described = match validated
            .conn
            .call(
                Some(&cdp_session),
                "DOM.describeNode",
                json!({ "backendNodeId": entry.backend_node_id }),
            )
            .await
        {
            Ok(node) => node,
            Err(_) => {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserRefStale,
                    "the upload ref no longer resolves in the live page",
                )
                .to_tool_result()
            }
        };
        let node = &described["node"];
        let attributes = node["attributes"].as_array().cloned().unwrap_or_default();
        let is_file_input = node["nodeName"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("input"))
            && attributes.chunks_exact(2).any(|pair| {
                pair[0]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("type"))
                    && pair[1]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case("file"))
            });
        if !is_file_input {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                "the live ref is not an input[type=file] element",
            )
            .to_tool_result();
        }
        match validated
            .conn
            .call(
                Some(&cdp_session),
                "DOM.setFileInputFiles",
                json!({ "backendNodeId": entry.backend_node_id, "files": files }),
            )
            .await
        {
            Ok(_) => ToolResult::text(format!("assigned {} file(s) in {tab_id}", files.len()))
                .with_structured(json!({
                    "status": "ok", "target_id": target_id, "tab_id": tab_id,
                    "ref": ext_ref, "frame": entry.frame.kind.as_str(), "file_count": files.len()
                })),
            Err(error) => BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                format!("the browser refused the exact file input assignment: {error}"),
            )
            .to_tool_result(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn every_browser_type_mode_has_one_postcondition() {
        use EditMode::{Insert, Replace, SetValue};
        enum Want {
            Confirmed,
            Mismatch(Option<&'static str>),
            Ambiguous,
            Detached,
        }
        let field = |value: &str, selection: Option<(usize, usize)>| EditState {
            value: value.to_owned(),
            start: selection.map(|(start, _)| start),
            end: selection.map(|(_, end)| end),
            field: true,
            password: false,
            connected: true,
        };
        let editable = |value: &str| EditState {
            field: false,
            ..field(value, None)
        };
        let detached = |value: &str| EditState {
            connected: false,
            ..field(value, None)
        };
        #[rustfmt::skip]
        let cases: Vec<(&str, EditState, EditState, &str, EditMode, Want)> = vec![
            // Input or textarea with a known caret or selection.
            ("caret insert", field("abcd", Some((2, 2))), field("abXcd", None), "X", Insert, Want::Confirmed),
            ("selection replaced", field("abcd", Some((2, 3))), field("abXd", None), "X", Insert, Want::Confirmed),
            ("caret insert, UTF-16", field("😀z", Some((2, 2))), field("😀yz", None), "y", Insert, Want::Confirmed),
            ("caret insert, wrong spot", field("abcd", Some((2, 2))), field("abcdX", None), "X", Insert, Want::Mismatch(Some("abXcd"))),
            ("rejected characters", field("", Some((0, 0))), field("1234", None), "12ab34", Insert, Want::Mismatch(Some("12ab34"))),
            ("React reset the field", field("", Some((0, 0))), field("", None), "ada@x.io", Insert, Want::Mismatch(Some("ada@x.io"))),
            // Email and number inputs: no selection API, caret anywhere.
            ("unknown caret, mid-field", field("a@b.com", None), field("ax@b.com", None), "x", Insert, Want::Confirmed),
            ("unknown caret, at the end", field("a@b.com", None), field("a@b.comx", None), "x", Insert, Want::Confirmed),
            ("unknown caret, text lost", field("a@b.com", None), field("a@b.co", None), "x", Insert, Want::Mismatch(None)),
            ("unknown caret, empty field", field("", None), field("ada@x.io", None), "ada@x.io", Insert, Want::Confirmed),
            ("unknown selection replaced", field("hello world", None), field("hello there", None), "there", Insert, Want::Ambiguous),
            // Replace and set_value on an input or textarea.
            ("replace", field("old", Some((3, 3))), field("new", None), "new", Replace, Want::Confirmed),
            ("replace appended", field("old", None), field("oldnew", None), "new", Replace, Want::Mismatch(Some("new"))),
            ("clear", field("old", None), field("", None), "", Replace, Want::Confirmed),
            ("set_value", field("old", None), field("new", None), "new", SetValue, Want::Confirmed),
            ("set_value ignored", field("old", None), field("old", None), "new", SetValue, Want::Mismatch(Some("new"))),
            // Contenteditable.
            ("editable insert", editable("Hello"), editable("Hello world"), " world", Insert, Want::Confirmed),
            ("editable insert, extra whitespace", editable("Hello\n"), editable("Hello  world\n"), " world", Insert, Want::Confirmed),
            ("editable retyped over a selection", editable("Hello"), editable("Hello"), "Hello", Insert, Want::Ambiguous),
            ("editable insert lost", editable("Hello"), editable("Hello"), "abc", Insert, Want::Mismatch(None)),
            ("editable trailing space typed", editable(""), editable("hello "), "hello ", Insert, Want::Confirmed),
            ("editable typed space as nbsp", editable(""), editable("hello\u{a0}"), "hello ", Insert, Want::Confirmed),
            ("editable append after a space", editable("hello "), editable("hello world"), "world", Insert, Want::Confirmed),
            ("editable selection replaced", editable("hello world"), editable("hello there"), "there", Insert, Want::Ambiguous),
            ("editable autocorrected", editable(""), editable("the"), "teh", Insert, Want::Mismatch(None)),
            // Normalization cannot tell these apart from a rejected edit.
            ("editable space into empty", editable(""), editable(""), " ", Insert, Want::Ambiguous),
            ("editable trailing space dropped", editable("hello"), editable("hello"), " ", Insert, Want::Ambiguous),
            ("editable trailing space as newline", editable("hello"), editable("hello\n"), " ", Insert, Want::Ambiguous),
            ("editable replaced with spaces", editable("x"), editable(""), "  ", Replace, Want::Ambiguous),
            ("field retyped over the same selection", field("abc", Some((0, 3))), field("abc", None), "abc", Insert, Want::Ambiguous),
            ("field space rejected", field("", Some((0, 0))), field("", None), " ", Insert, Want::Mismatch(Some(" "))),
            ("editable replace, same text", editable("Hello"), editable("Hello"), "Hello", Replace, Want::Confirmed),
            ("editable replace appended", editable("Hello"), editable("Hello world"), "world", Replace, Want::Mismatch(None)),
            ("editable replace", editable("Hello"), editable("world\n"), "world", Replace, Want::Confirmed),
            ("editable clear", editable("Hello"), editable("\n"), "", Replace, Want::Confirmed),
            // The page replaced the edited element.
            ("detached node", field("", Some((0, 0))), detached("ada@x.io"), "ada@x.io", Insert, Want::Detached),
        ];
        for (name, before, after, text, mode, want) in cases {
            let got = judge_edit(&before, &after, text, mode);
            let ok = match (&want, &got) {
                (Want::Confirmed, Readback::Confirmed(value)) => *value == after.value,
                (
                    Want::Mismatch(expected),
                    Readback::Mismatch {
                        actual,
                        expected: got,
                    },
                ) => *actual == after.value && got.as_deref() == *expected,
                (Want::Detached, Readback::Detached) => true,
                (Want::Ambiguous, Readback::Ambiguous(value)) => *value == after.value,
                _ => false,
            };
            assert!(ok, "{name}: got {got:?}");
        }
    }

    #[test]
    fn a_confirmed_insertion_always_changed_the_value() {
        let values = [
            "",
            " ",
            "a",
            "hello",
            "hello ",
            "hello\n",
            "hello world",
            "ahello",
        ];
        let texts = [" ", "a", "hello", " world", "  "];
        for field in [true, false] {
            for selection in [None, Some((0, 0)), Some((5, 5))] {
                for before in values {
                    for after in values {
                        for text in texts {
                            let state =
                                |value: &str, selection: Option<(usize, usize)>| EditState {
                                    value: value.to_owned(),
                                    start: selection.map(|(start, _)| start),
                                    end: selection.map(|(_, end)| end),
                                    field,
                                    password: false,
                                    connected: true,
                                };
                            let verdict = judge_edit(
                                &state(before, if field { selection } else { None }),
                                &state(after, None),
                                text,
                                EditMode::Insert,
                            );
                            if matches!(verdict, Readback::Confirmed(_)) {
                                assert_ne!(
                                    before, after,
                                    "{before:?} -> {after:?} typing {text:?}"
                                );
                                if !field {
                                    assert_ne!(
                                        normalize_rendered(before),
                                        normalize_rendered(after),
                                        "{before:?} -> {after:?} typing {text:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn passwords_are_never_shown() {
        assert_eq!(
            shown_value("hunter2", true),
            "7 character(s) (a password; not shown)"
        );
        assert_eq!(shown_value("a\"b", false), "\"a\\\"b\"");
        assert_eq!(truncate_value(&"x".repeat(250)).chars().count(), 201);
    }

    use super::*;
    use crate::browser::platform::{BrowserPlatform, PrepareOutcome, PrepareRequest};
    use crate::browser::types::{
        BrowserClassification, BrowserEngineFamily, BrowserProduct, NativeWindowInfo,
        OwnedEndpoint, ProcessFingerprint,
    };

    /// Minimal adapter: pid 1 is a CDP-capable browser with no endpoint
    /// (setup required); pid 2 is not a browser; pid 3 is Safari-like
    /// (browser, no CDP). Prepare requires consent.
    struct MockPlatform;

    #[async_trait]
    impl BrowserPlatform for MockPlatform {
        async fn classify_browser(
            &self,
            pid: i64,
        ) -> Result<BrowserClassification, BrowserRefusal> {
            Ok(match pid {
                1 => BrowserClassification {
                    is_browser: true,
                    engine: BrowserEngineFamily::Chromium,
                    product_kind: BrowserProduct::GoogleChrome,
                    product: Some("MockChrome".into()),
                    channel: Some("stable".into()),
                    process_role: crate::browser::types::BrowserProcessRole::StandaloneConsumer,
                    supports_cdp: true,
                },
                3 => BrowserClassification {
                    is_browser: true,
                    engine: BrowserEngineFamily::Webkit,
                    product_kind: BrowserProduct::Safari,
                    product: Some("MockSafari".into()),
                    channel: None,
                    process_role: crate::browser::types::BrowserProcessRole::StandaloneConsumer,
                    supports_cdp: false,
                },
                4 => BrowserClassification {
                    is_browser: true,
                    engine: BrowserEngineFamily::Gecko,
                    product_kind: BrowserProduct::Firefox,
                    product: Some("MockFirefox".into()),
                    channel: None,
                    process_role: crate::browser::types::BrowserProcessRole::StandaloneConsumer,
                    supports_cdp: false,
                },
                _ => BrowserClassification {
                    is_browser: false,
                    engine: BrowserEngineFamily::Unknown,
                    product_kind: BrowserProduct::Other,
                    product: None,
                    channel: None,
                    process_role: crate::browser::types::BrowserProcessRole::Unknown,
                    supports_cdp: false,
                },
            })
        }

        async fn native_window(
            &self,
            pid: i64,
            window_id: u64,
        ) -> Result<NativeWindowInfo, BrowserRefusal> {
            assert!(
                !matches!(pid, 3 | 4),
                "unsupported browser engines must refuse before native-window probing"
            );
            use crate::browser::types::{NativeOwnershipMethod, NativeOwnershipProof, Rect};
            Ok(NativeWindowInfo {
                pid,
                window_id,
                title: "Mock - Chrome".into(),
                bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                geometry_exact: true,
                ownership: NativeOwnershipProof {
                    method: NativeOwnershipMethod::WindowServerOwner,
                    owner_pid: pid,
                    detail: None,
                },
            })
        }

        async fn is_only_exact_native_window(
            &self,
            _pid: i64,
            _window_id: u64,
        ) -> Result<Option<bool>, BrowserRefusal> {
            Ok(Some(true))
        }

        async fn discover_owned_endpoint(
            &self,
            _pid: i64,
        ) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
            Ok(None)
        }

        async fn process_fingerprint(
            &self,
            pid: i64,
        ) -> Result<ProcessFingerprint, BrowserRefusal> {
            Ok(ProcessFingerprint {
                pid,
                start_time: Some(1),
                executable: None,
            })
        }

        async fn prepare_endpoint(
            &self,
            _request: PrepareRequest,
        ) -> Result<PrepareOutcome, BrowserRefusal> {
            Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserRequiresSetup,
                "mock endpoint needs acting setup",
            ))
        }
    }

    pub(crate) fn engine() -> Arc<BrowserEngine> {
        BrowserEngine::new(Arc::new(MockPlatform))
    }

    fn structured(result: &ToolResult) -> &Value {
        result
            .structured_content
            .as_ref()
            .expect("structured content")
    }

    fn outcome(outline: &str, told: Told) -> SemanticSnapshotOutcome {
        SemanticSnapshotOutcome {
            snapshot_id: 7,
            url: "https://example.test/".into(),
            title: "Example".into(),
            outline: outline.into(),
            action_refs: 1,
            content_refs: 0,
            complete: true,
            scope: "viewport",
            selected_nodes: 1,
            total_nodes: 1,
            omissions: Default::default(),
            continuation: None,
            oopif: crate::browser::engine::OopifStatus::Unsupported,
            outline_budget: 5_000,
            listed: None,
            revision: Some(12),
            told,
        }
    }

    #[test]
    fn a_diff_is_returned_only_while_it_is_smaller_than_the_snapshot() {
        let change = |index: usize| DiffOp::Change {
            key: format!("p7:{index}"),
            line: format!("- button \"Row {index}\" [p7:{index} click] (pressed)"),
        };
        let small = page_changes(&outcome(
            &"- button \"Row\" [p7:0 click]\n".repeat(40),
            Told::Diff {
                base_revision: 11,
                ops: vec![change(3)],
                page_changed: false,
            },
        ));
        assert_eq!(small.kind, PageChangesKind::Diff);
        assert_eq!((small.base_revision, small.revision), (Some(11), Some(12)));
        assert_eq!(small.ops.as_ref().map(Vec::len), Some(1));
        assert_eq!((small.outline, small.url), (None, None));

        // Every line changed: the ops repeat the page, keyed. Send the page.
        let rows = 0..8;
        let page = rows
            .clone()
            .map(|index| format!("- button \"Row {index}\" [p7:{index} click] (pressed)"))
            .collect::<Vec<_>>()
            .join("\n");
        let large = page_changes(&outcome(
            &page,
            Told::Diff {
                base_revision: 11,
                ops: rows.map(change).collect(),
                page_changed: false,
            },
        ));
        assert_eq!(large.kind, PageChangesKind::Snapshot);
        assert_eq!(large.reason.as_deref(), Some("diff_larger_than_snapshot"));
        assert_eq!(large.revision, Some(12));
        assert!(large.ops.is_none() && large.outline.is_some());
    }

    #[test]
    fn a_diff_names_the_page_address_only_when_it_changed() {
        let told = |page_changed| Told::Diff {
            base_revision: 11,
            ops: Vec::new(),
            page_changed,
        };
        let outline = "- button \"Row\" [p7:0 click]\n".repeat(10);
        assert_eq!(page_changes(&outcome(&outline, told(false))).url, None);
        let moved = page_changes(&outcome(&outline, told(true)));
        assert_eq!(moved.url.as_deref(), Some("https://example.test/"));
        assert_eq!(moved.title.as_deref(), Some("Example"));
    }

    #[test]
    fn page_changes_fit_the_closed_action_result_contract() {
        let dialog = CdpDialogState {
            generation: 3,
            kind: "confirm".into(),
            session_id: "s".into(),
        };
        for changes in [
            unavailable_changes("javascript_dialog_open", Some(&dialog)),
            page_changes(&outcome(
                "- button \"Row\" [p7:0 click]",
                Told::Snapshot {
                    reason: Some(FullReason::DocumentChanged),
                },
            )),
        ] {
            let value = changes_value(&changes);
            assert!(
                !value.to_string().contains("null"),
                "absent fields are left out: {value}"
            );
            let back: PageChanges = serde_json::from_value(value).expect("the typed contract");
            assert_eq!(back, changes);
        }
        assert_eq!(
            changes_value(&unavailable_changes(
                "javascript_dialog_open",
                Some(&dialog)
            ))["dialog"],
            json!({"dialog_id": "dialog-3", "kind": "confirm"})
        );
    }

    #[test]
    fn a_click_goes_out_only_when_the_page_says_the_refs_element_receives_it() {
        let facts = |inside: bool, contains: bool, label: bool, own: bool| {
            json!({"connected": true, "hit": true, "inside_target": inside,
                "contains_target": contains, "label_of_target": label, "own_indicator": own})
        };
        for (case, facts, expected) in [
            (
                "the element itself, or a child of it",
                facts(true, false, false, false),
                Hit::Receives,
            ),
            (
                "a child, in a closed shadow root it is the host of",
                facts(true, true, false, false),
                Hit::Receives,
            ),
            ("its label", facts(false, false, true, false), Hit::Receives),
            (
                "an element around it: it takes no pointer input there itself",
                facts(false, true, false, false),
                Hit::Container,
            ),
            (
                "an unrelated element on top",
                facts(false, false, false, false),
                Hit::Covered {
                    own_indicator: false,
                },
            ),
            (
                "the extension's own pill",
                facts(false, false, false, true),
                Hit::Covered {
                    own_indicator: true,
                },
            ),
            (
                "nothing at the point",
                json!({"connected": true, "hit": false}),
                Hit::Outside,
            ),
            (
                "the node left the page",
                json!({"connected": false}),
                Hit::Gone,
            ),
            ("no answer proves nothing", json!(true), Hit::Unknown),
            (
                "nor does half an answer",
                json!({"connected": true, "hit": true}),
                Hit::Unknown,
            ),
            (
                "nor one without its first fact",
                json!({"hit": true, "inside_target": true}),
                Hit::Unknown,
            ),
        ] {
            assert_eq!(classify_hit(&facts), expected, "{case}");
        }
    }

    #[test]
    fn tool_annotations_and_schemas() {
        let e = engine();
        let state = GetBrowserStateTool::new(e.clone());
        assert!(
            state.def().read_only,
            "get_browser_state must be strictly read-only"
        );
        assert!(state.def().idempotent);
        assert_eq!(
            state.def().input_schema["properties"]["include_screenshot"]["default"],
            false
        );

        let prepare = BrowserPrepareTool::new(e.clone());
        assert!(prepare.def().destructive);
        assert!(!prepare.def().idempotent);
        let prepare_properties = prepare.def().input_schema["properties"]
            .as_object()
            .expect("browser_prepare properties");
        assert!(!prepare_properties.contains_key("consent"));
        assert!(!prepare_properties.contains_key("allow_restart"));
        assert!(!prepare_properties.contains_key("approval_token"));

        let dialog = BrowserDialogTool::new(e.clone());
        assert_eq!(
            dialog.def().input_schema["properties"]["delivery_mode"]["default"],
            "background"
        );

        for (def, name) in [
            (
                BrowserPrepareTool::new(e.clone()).def().clone(),
                "browser_prepare",
            ),
            (
                BrowserNavigateTool::new(e.clone()).def().clone(),
                "browser_navigate",
            ),
            (
                BrowserClickTool::new(e.clone()).def().clone(),
                "browser_click",
            ),
            (
                BrowserTypeTool::new(e.clone()).def().clone(),
                "browser_type",
            ),
            (
                BrowserDialogTool::new(e.clone()).def().clone(),
                "browser_dialog",
            ),
            (
                BrowserSetInputFilesTool::new(e.clone()).def().clone(),
                "browser_set_input_files",
            ),
            (
                BrowserDownloadTool::new(e.clone()).def().clone(),
                "browser_download",
            ),
            (
                BrowserPointerTool::new(e.clone()).def().clone(),
                "browser_pointer",
            ),
        ] {
            assert_eq!(def.name, name);
            assert!(!def.read_only, "{name} is a mutation");
            assert!(def.input_schema["properties"].is_object(), "{name} schema");
        }
        // Every browser mutation can affect web or browser state outside the
        // client, even when the immediate delivery route is local CDP.
        for def in [
            BrowserPrepareTool::new(e.clone()).def().clone(),
            BrowserNavigateTool::new(e.clone()).def().clone(),
            BrowserClickTool::new(e.clone()).def().clone(),
            BrowserTypeTool::new(e.clone()).def().clone(),
            BrowserDialogTool::new(e.clone()).def().clone(),
            BrowserSetInputFilesTool::new(e.clone()).def().clone(),
            BrowserDownloadTool::new(e.clone()).def().clone(),
            BrowserPointerTool::new(e.clone()).def().clone(),
        ] {
            assert!(def.open_world, "{} can affect open-world state", def.name);
        }
        assert!(BrowserDownloadTool::new(e.clone()).def().destructive);
    }

    #[test]
    fn registration_registers_all_browser_tools() {
        let e = engine();
        let mut registry = ToolRegistry::new();
        register_browser_tools(&e, &mut registry);
        let names: Vec<&str> = registry.tool_names().collect();
        assert_eq!(
            names,
            vec![
                "get_browser_state",
                "browser_prepare",
                "browser_navigate",
                "browser_click",
                "browser_type",
                "browser_steps",
                "browser_dialog",
                "browser_set_input_files",
                "browser_download",
                "browser_pointer",
                "browser_tabs"
            ]
        );
    }

    #[test]
    fn upload_paths_are_absolute_regular_files_and_outputs_need_no_path() {
        let path = std::env::temp_dir().join(format!("cua-upload-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"fixture").unwrap();
        let args = json!({ "files": [path.to_string_lossy()] });
        let validated = validated_upload_paths(&args).unwrap();
        assert_eq!(validated.len(), 1);
        assert!(std::path::Path::new(&validated[0]).is_absolute());
        assert!(validated_upload_paths(&json!({ "files": ["relative.txt"] })).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn bind_requires_an_explicit_session() {
        let e = engine();
        let tool = GetBrowserStateTool::new(e);
        for args in [
            json!({ "pid": 1, "window_id": 7 }),
            json!({ "pid": 1, "window_id": 7, "_session_id": "default" }),
        ] {
            let result = tool.invoke(args).await;
            assert_eq!(result.is_error, Some(true));
        }
    }

    #[tokio::test]
    async fn snapshot_rejects_non_boolean_include_screenshot() {
        let result = GetBrowserStateTool::new(engine())
            .invoke(json!({
                "target_id": "bt-fixture",
                "tab_id": "tab-fixture",
                "session": "browser-run",
                "include_screenshot": "yes"
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(matches!(
            &result.content[0],
            Content::Text { text, .. } if text.contains("expected boolean")
        ));
    }

    #[tokio::test]
    async fn public_session_field_is_the_primary_capability_namespace() {
        let tool = GetBrowserStateTool::new(engine());
        let result = tool
            .invoke(json!({ "pid": 2, "window_id": 7, "session": "public-run" }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_route_unavailable"
        );
        assert_ne!(
            result.is_error,
            Some(true),
            "public session must pass capability-namespace validation"
        );
    }

    #[tokio::test]
    async fn non_browser_pid_refuses_route_unavailable() {
        let tool = GetBrowserStateTool::new(engine());
        let result = tool
            .invoke(json!({ "pid": 2, "window_id": 7, "_session_id": "run-1" }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_route_unavailable"
        );
    }

    #[tokio::test]
    async fn cdp_less_browser_refuses_route_unavailable() {
        let tool = GetBrowserStateTool::new(engine());
        let result = tool
            .invoke(json!({ "pid": 3, "window_id": 7, "_session_id": "run-1" }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_route_unavailable"
        );
        assert_eq!(
            structured(&result)["refusal"]["detail"]["engine_family"],
            "webkit"
        );
        assert_eq!(
            structured(&result)["refusal"]["detail"]["product"],
            "safari"
        );
        assert_eq!(
            structured(&result)["refusal"]["detail"]["limitation"],
            "no_attachable_runtime_endpoint"
        );
    }

    #[tokio::test]
    async fn firefox_refusal_names_the_required_protocol_without_probing_the_window() {
        let tool = GetBrowserStateTool::new(engine());
        let result = tool
            .invoke(json!({ "pid": 4, "window_id": 7, "_session_id": "run-1" }))
            .await;
        let refusal = &structured(&result)["refusal"];
        assert_eq!(refusal["code"], "browser_route_unavailable");
        assert_eq!(refusal["detail"]["engine_family"], "gecko");
        assert_eq!(refusal["detail"]["product"], "firefox");
        assert_eq!(refusal["detail"]["required_protocol"], "webdriver_bidi");
        assert_eq!(
            refusal["detail"]["limitation"],
            "remote_agent_requires_launch_time_enablement"
        );
    }

    #[tokio::test]
    async fn standalone_consumer_refuses_existing_profile_approval_without_preparing() {
        let tool = GetBrowserStateTool::new(engine());
        let result = tool
            .invoke(json!({ "pid": 1, "window_id": 7, "_session_id": "run-1" }))
            .await;
        let s = structured(&result);
        assert_eq!(s["status"], "refused");
        assert_eq!(s["refusal"]["code"], "browser_consent_required");
        assert_eq!(
            s["refusal"]["detail"]["reason"],
            "consumer_profile_endpoint_requires_grant"
        );
        assert_eq!(s["refusal"]["detail"]["next_action"], "browser_prepare");
    }

    #[tokio::test]
    async fn isolated_prepare_uses_runtime_authorization_without_a_token() {
        let tool = BrowserPrepareTool::new(engine());
        let result = tool
            .invoke(json!({
                "pid": 1,
                "allow_launch": true,
                "profile": { "mode": "isolated_new" },
                "session": "run-1"
            }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_route_unavailable"
        );
    }

    #[tokio::test]
    async fn isolated_launch_accepts_omitted_pid() {
        let tool = BrowserPrepareTool::new(engine());
        let result = tool
            .invoke(json!({
                "allow_launch": true,
                "profile": { "mode": "isolated_new" },
                "session": "pid-free-isolated-run"
            }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_route_unavailable"
        );
    }

    #[tokio::test]
    async fn pid_remains_required_without_a_complete_isolated_launch_request() {
        let tool = BrowserPrepareTool::new(engine());
        for request in [
            json!({
                "allow_launch": true,
                "session": "pid-free-without-profile"
            }),
            json!({
                "profile": { "mode": "isolated_new" },
                "session": "pid-free-without-allow-launch"
            }),
            json!({
                "window_id": 7,
                "strategy": { "kind": "existing_profile" },
                "session": "existing-profile-without-pid"
            }),
            json!({
                "allow_launch": true,
                "profile": { "mode": "isolated_new" },
                "strategy": { "kind": "existing_profile" },
                "session": "conflicting-strategy-without-pid"
            }),
        ] {
            let result = tool.invoke(request).await;
            assert_eq!(result.is_error, Some(true));
            let body = serde_json::to_string(&result.content).expect("serialize tool error");
            assert!(
                body.contains("Missing required integer field: pid"),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn existing_profile_requires_runtime_authorization() {
        let tool = BrowserPrepareTool::new(engine());
        let result = tool
            .invoke(json!({
                "pid": 1,
                "window_id": 7,
                "strategy": { "kind": "existing_profile" },
                "session": "existing-profile-run"
            }))
            .await;
        let structured = structured(&result);
        assert_eq!(structured["refusal"]["code"], "browser_consent_required");
        assert_eq!(
            structured["refusal"]["detail"]["authorization_required"],
            true
        );
    }

    #[tokio::test]
    async fn existing_strategy_conflicts_fail_before_platform_setup() {
        let tool = BrowserPrepareTool::new(engine());
        let result = tool
            .invoke(json!({
                "pid": 1,
                "window_id": 7,
                "strategy": { "kind": "existing_profile" },
                "profile": { "mode": "isolated_new" },
                "session": "existing-conflict"
            }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_consent_required"
        );
    }

    #[tokio::test]
    async fn mutations_on_unknown_targets_refuse_binding_stale() {
        let e = engine();
        for result in [
            BrowserNavigateTool::new(e.clone())
                .invoke(json!({
                    "target_id": "bt999", "tab_id": "tab1",
                    "url": "https://example.test", "_session_id": "run-1"
                }))
                .await,
            BrowserClickTool::new(e.clone())
                .invoke(json!({
                    "target_id": "bt999", "tab_id": "tab1", "ref": "p1:0",
                    "_session_id": "run-1"
                }))
                .await,
            BrowserTypeTool::new(e)
                .invoke(json!({
                    "target_id": "bt999", "tab_id": "tab1", "text": "hi",
                    "_session_id": "run-1"
                }))
                .await,
        ] {
            assert_eq!(
                structured(&result)["refusal"]["code"],
                "browser_binding_stale",
                "unknown capability must refuse, not error"
            );
        }
    }

    #[tokio::test]
    async fn mutations_reject_bad_url_and_bad_route_before_binding() {
        let e = engine();
        let nav = BrowserNavigateTool::new(e.clone())
            .invoke(json!({
                "target_id": "bt1", "tab_id": "tab1", "url": "file:///etc/passwd",
                "_session_id": "run-1"
            }))
            .await;
        assert_eq!(nav.is_error, Some(true));

        let click = BrowserClickTool::new(e)
            .invoke(json!({
                "target_id": "bt1", "tab_id": "tab1", "ref": "p1:0",
                "input_route": "sneaky", "_session_id": "run-1"
            }))
            .await;
        assert_eq!(click.is_error, Some(true));
    }

    #[tokio::test]
    async fn heuristic_bindings_refuse_mutation() {
        use crate::browser::types::{BindingQuality, Rect};
        use std::collections::HashMap;

        let e = engine();
        // Seed a heuristic binding directly into the store.
        let mut tabs = HashMap::new();
        let tab_id = e.store.mint_tab_id();
        tabs.insert(
            tab_id.clone(),
            crate::browser::store::TabRecord::new(
                tab_id.clone(),
                "CDPX".into(),
                "Mock".into(),
                "https://example.test".into(),
                Some(true),
                0,
            ),
        );
        let target_id = e.store.mint_target(
            "run-1",
            crate::browser::store::TargetRecord {
                target_id: String::new(),
                pid: 1,
                window_id: 7,
                ws_url: "ws://127.0.0.1:9222/devtools/browser/x".into(),
                endpoint_owner_pid: 1,
                endpoint_transport: crate::browser::types::EndpointTransport::LegacyJsonVersion,
                endpoint_access_class:
                    crate::browser::types::EndpointAccessClass::EmbeddedApplication,
                generation: 0,
                transport_session: None,
                fingerprint: ProcessFingerprint {
                    pid: 1,
                    start_time: Some(1),
                    executable: None,
                },
                native_title: "Mock - Chrome".into(),
                native_bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                cdp_target_id: "CDPX".into(),
                cdp_window_id: Some(5),
                quality: BindingQuality::Heuristic,
                tabs,
            },
        );

        let result = BrowserTypeTool::new(e)
            .invoke(json!({
                "target_id": target_id, "tab_id": tab_id, "text": "hi",
                "_session_id": "run-1"
            }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_wrong_target_refused",
            "heuristic bindings are read-only"
        );
    }

    #[tokio::test]
    async fn stale_fingerprint_refuses_before_any_cdp_traffic() {
        use crate::browser::types::{BindingQuality, Rect};
        use std::collections::HashMap;

        let e = engine();
        let mut tabs = HashMap::new();
        let tab_id = e.store.mint_tab_id();
        tabs.insert(
            tab_id.clone(),
            crate::browser::store::TabRecord::new(
                tab_id.clone(),
                "CDPX".into(),
                "Mock".into(),
                "https://example.test".into(),
                Some(true),
                0,
            ),
        );
        // Bound fingerprint has start_time 999; MockPlatform now reports 1.
        let target_id = e.store.mint_target(
            "run-1",
            crate::browser::store::TargetRecord {
                target_id: String::new(),
                pid: 1,
                window_id: 7,
                ws_url: "ws://127.0.0.1:9222/devtools/browser/x".into(),
                endpoint_owner_pid: 1,
                endpoint_transport: crate::browser::types::EndpointTransport::LegacyJsonVersion,
                endpoint_access_class:
                    crate::browser::types::EndpointAccessClass::EmbeddedApplication,
                generation: 0,
                transport_session: None,
                fingerprint: ProcessFingerprint {
                    pid: 1,
                    start_time: Some(999),
                    executable: None,
                },
                native_title: "Mock - Chrome".into(),
                native_bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                cdp_target_id: "CDPX".into(),
                cdp_window_id: Some(5),
                quality: BindingQuality::Exact,
                tabs,
            },
        );

        let result = BrowserNavigateTool::new(e)
            .invoke(json!({
                "target_id": target_id, "tab_id": tab_id,
                "url": "https://example.test", "_session_id": "run-1"
            }))
            .await;
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "browser_binding_stale"
        );
    }

    #[tokio::test]
    async fn session_end_hook_drops_the_capability_namespace() {
        use crate::browser::types::{BindingQuality, Rect};
        use std::collections::HashMap;

        let e = engine();
        let sid = "browser-store-cleanup-session-771";
        e.store.mint_target(
            sid,
            crate::browser::store::TargetRecord {
                target_id: String::new(),
                pid: 1,
                window_id: 7,
                ws_url: "ws://127.0.0.1:9222/devtools/browser/x".into(),
                endpoint_owner_pid: 1,
                endpoint_transport: crate::browser::types::EndpointTransport::LegacyJsonVersion,
                endpoint_access_class:
                    crate::browser::types::EndpointAccessClass::EmbeddedApplication,
                generation: 0,
                transport_session: None,
                fingerprint: ProcessFingerprint {
                    pid: 1,
                    start_time: Some(1),
                    executable: None,
                },
                native_title: "T".into(),
                native_bounds: Rect::new(0.0, 0.0, 1.0, 1.0),
                cdp_target_id: "C".into(),
                cdp_window_id: Some(1),
                quality: BindingQuality::Exact,
                tabs: HashMap::new(),
            },
        );
        assert_eq!(e.store.target_count(sid), 1);
        crate::session::fire_session_end(sid);
        assert_eq!(
            e.store.target_count(sid),
            0,
            "session end must clean the store"
        );
    }
}
