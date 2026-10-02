//! Extended browser pointer actions.
//!
//! This module deliberately shares the exact-or-refused posture of the
//! original browser click tool. Every mutation is serialized by the real CDP
//! tab, then revalidates the native window, endpoint, tab, and (for refs) frame
//! identity before dispatch. It never activates a target or brings a page to
//! the foreground.
//!
//! Any listed element can be pointed at, whatever its line offers: a card or
//! a grid cell is plain text to accessibility but takes a drag or a
//! double-click. What keeps trusted input on the element is the live
//! hit-test browser_click uses (`browser_target_covered` when something else
//! is on top at its centre), not the actions the line declares.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::recording_tools::ReplayRegistrySlot;
use crate::tool::{ProtectedResourceOwnership, Tool, ToolDef};
use crate::tool_args::ArgsExt;

use super::cdp_ws::{CdpConnection, CdpEvent};
use super::engine::{dialog_open_refusal, BrowserEngine, Settled, ValidatedTab};
use super::platform::BrowserVisualActionKind;
use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use super::session_schema;
use super::store::{FrameKind, FrameRef};
use super::tools::{
    browser_protected_resource_scope, browser_resource_ownership, changes_will_be_read, live_point,
    no_registry, page_changes_after, quad_center, require_session, with_changes, Delivery, HeldRef,
    LiveInput,
};

/// Pointer moves between a drag's press and its release.
const DRAG_STEPS: u32 = 8;
/// How long a drag waits, after its last move, for Chrome to say it took the
/// drag over as an HTML5 drag-and-drop (`Input.dragIntercepted`).
const DRAG_INTERCEPT_WAIT: Duration = Duration::from_millis(150);
/// One animation frame between a drag's moves.
const DRAG_FRAME: Duration = Duration::from_millis(16);
/// How long cleanup after a stopped drag waits for each answer.
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerAction {
    Hover,
    RightClick,
    DoubleClick,
    Scroll,
    Drag,
}

impl PointerAction {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hover" => Ok(Self::Hover),
            "right_click" => Ok(Self::RightClick),
            "double_click" => Ok(Self::DoubleClick),
            "scroll" => Ok(Self::Scroll),
            "drag" => Ok(Self::Drag),
            _ => Err(format!(
                "action must be hover, right_click, double_click, scroll, or drag, got {value:?}"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::RightClick => "right_click",
            Self::DoubleClick => "double_click",
            Self::Scroll => "scroll",
            Self::Drag => "drag",
        }
    }

    /// What a hit-test refusal calls this input.
    fn noun(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::RightClick => "right-click",
            Self::DoubleClick => "double-click",
            Self::Scroll => "scroll",
            Self::Drag => "drag",
        }
    }

    /// What the result says was done.
    fn past(self) -> &'static str {
        match self {
            Self::Hover => "hovered over",
            Self::RightClick => "right-clicked",
            Self::DoubleClick => "double-clicked",
            Self::Scroll => "scrolled at",
            Self::Drag => "dragged",
        }
    }
}

fn visual_kind(action: PointerAction) -> BrowserVisualActionKind {
    match action {
        PointerAction::Hover => BrowserVisualActionKind::Hover,
        PointerAction::RightClick => BrowserVisualActionKind::RightClick,
        PointerAction::DoubleClick => BrowserVisualActionKind::DoubleClick,
        PointerAction::Scroll => BrowserVisualActionKind::Scroll,
        PointerAction::Drag => BrowserVisualActionKind::Drag,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputRoute {
    Trusted,
    DomEvent,
}

impl InputRoute {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "trusted" => Ok(Self::Trusted),
            "dom_event" => Ok(Self::DomEvent),
            _ => Err(format!(
                "input_route must be \"trusted\" or \"dom_event\", got {value:?}"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::DomEvent => "dom_event",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Location {
    Ref(String),
    Coordinates(f64, f64),
}

#[derive(Debug, Clone, PartialEq)]
struct PointerRequest {
    action: PointerAction,
    route: InputRoute,
    origin: Location,
    destination: Option<Location>,
    delta_x: f64,
    delta_y: f64,
}

fn finite_pair(args: &Value, x_name: &str, y_name: &str) -> Result<Option<(f64, f64)>, String> {
    match (args.opt_f64(x_name), args.opt_f64(y_name)) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok(Some((x, y))),
        (Some(_), Some(_)) => Err(format!("{x_name} and {y_name} must be finite numbers")),
        _ => Err(format!("pass both {x_name} and {y_name}, or neither")),
    }
}

fn one_location(
    args: &Value,
    ref_name: &str,
    x_name: &str,
    y_name: &str,
) -> Result<Option<Location>, String> {
    let reference = args.opt_str(ref_name);
    let coords = finite_pair(args, x_name, y_name)?;
    match (reference, coords) {
        (Some(_), Some(_)) => Err(format!(
            "pass either {ref_name} or {x_name}/{y_name}, not both"
        )),
        (Some(reference), None) => Ok(Some(Location::Ref(reference))),
        (None, Some((x, y))) => Ok(Some(Location::Coordinates(x, y))),
        (None, None) => Ok(None),
    }
}

fn parse_request(args: &Value) -> Result<PointerRequest, String> {
    let action = PointerAction::parse(
        args.get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required string field: action".to_owned())?,
    )?;
    let route = InputRoute::parse(
        args.get("input_route")
            .and_then(Value::as_str)
            .unwrap_or("trusted"),
    )?;
    let origin = one_location(args, "ref", "x", "y")?
        .ok_or_else(|| "browser_pointer needs a ref or x/y coordinates".to_owned())?;
    let destination = one_location(args, "destination_ref", "to_x", "to_y")?;

    if route == InputRoute::DomEvent && !matches!(origin, Location::Ref(_)) {
        return Err("input_route=dom_event requires a ref".into());
    }
    if action == PointerAction::Drag && destination.is_none() {
        return Err("action=drag requires destination_ref or to_x/to_y".into());
    }
    if action != PointerAction::Drag && destination.is_some() {
        return Err("destination_ref and to_x/to_y are valid only for action=drag".into());
    }

    let delta_x = args.opt_f64("delta_x").unwrap_or(0.0);
    let delta_y = args.opt_f64("delta_y").unwrap_or(0.0);
    if !delta_x.is_finite() || !delta_y.is_finite() {
        return Err("delta_x and delta_y must be finite numbers".into());
    }
    if action == PointerAction::Scroll && delta_x == 0.0 && delta_y == 0.0 {
        return Err("action=scroll requires a non-zero delta_x or delta_y".into());
    }
    if action != PointerAction::Scroll
        && (args.get("delta_x").is_some() || args.get("delta_y").is_some())
    {
        return Err("delta_x and delta_y are valid only for action=scroll".into());
    }

    Ok(PointerRequest {
        action,
        route,
        origin,
        destination,
        delta_x,
        delta_y,
    })
}

struct ResolvedRef {
    external: String,
    backend_node_id: i64,
    frame: FrameRef,
    cdp_session: String,
}

fn same_exact_frame(left: &FrameRef, right: &FrameRef) -> bool {
    left.kind == right.kind
        && left.oopif_target_id == right.oopif_target_id
        && left.identity == right.identity
}

fn stale(message: impl Into<String>) -> ToolResult {
    BrowserRefusal::new(BrowserRefusalCode::BrowserRefStale, message).to_tool_result()
}

async fn resolve_object(
    conn: &CdpConnection,
    cdp_session: &str,
    backend_node_id: i64,
) -> Result<String, ToolResult> {
    let resolved = conn
        .call(
            Some(cdp_session),
            "DOM.resolveNode",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await
        .map_err(|_| stale("the ref's node no longer resolves in the live page"))?;
    resolved
        .get("object")
        .and_then(|object| object.get("objectId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| stale("the ref's node has no live object in the page"))
}

/// The element synthetic events go to: the ref's node, or for a text ref
/// (a card's or a cell's text) the element holding it.
async fn element_of(
    conn: &CdpConnection,
    cdp_session: &str,
    object_id: String,
) -> Result<String, ToolResult> {
    let reply = conn
        .call(
            Some(cdp_session),
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function() { return this.nodeType === 1 ? this : this.parentElement; }",
            }),
        )
        .await
        .map_err(|_| stale("the ref's node no longer resolves in the live page"))?;
    reply
        .pointer("/result/objectId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| stale("the ref's node is in no element of the live page"))
}

/// The centre of a ref's box, for the cursor drawn over a synthetic event.
async fn point_for_ref(
    conn: &CdpConnection,
    cdp_session: &str,
    backend_node_id: i64,
) -> Option<(f64, f64)> {
    let _ = conn
        .call(
            Some(cdp_session),
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await;
    let model = conn
        .call(
            Some(cdp_session),
            "DOM.getBoxModel",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await
        .ok()?;
    quad_center(&model)
}

/// One clause on what the page did, from the `changes` the action read: what
/// a caller learns without opening them. Never a bare "ok".
fn changes_summary(changes: Option<&Value>) -> String {
    let Some(changes) = changes else {
        return "the page was not read afterwards (this session holds a dom_refs_v1 \
                snapshot; read it again to see the effect)"
            .to_owned();
    };
    let still = if changes.get("settled") == Some(&Value::Bool(false)) {
        "; the page had not settled when it was read"
    } else {
        ""
    };
    let said = match changes.get("kind").and_then(Value::as_str) {
        Some("diff") => {
            let ops = changes
                .get("ops")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if ops.is_empty() {
                "nothing in the page's outline changed".to_owned()
            } else {
                let count = |kind: &str| {
                    ops.iter()
                        .filter(|op| op.get("op").and_then(Value::as_str) == Some(kind))
                        .count()
                };
                let parts = [
                    ("added", count("add")),
                    ("changed", count("change")),
                    ("moved", count("move")),
                    ("gone", count("leave")),
                ]
                .into_iter()
                .filter(|(_, n)| *n > 0)
                .map(|(word, n)| format!("{n} {word}"))
                .collect::<Vec<_>>()
                .join(", ");
                format!("the page changed: {parts} line(s), listed in changes")
            }
        }
        Some("snapshot") => match changes.get("reason").and_then(Value::as_str) {
            Some("document_changed") => {
                "a new document replaced the page; its outline is in changes".to_owned()
            }
            Some("diff_larger_than_snapshot") => {
                "the page changed; its outline is in changes".to_owned()
            }
            // No baseline to compare with: a read, not a proven change.
            _ => "the page was read again; its outline is in changes".to_owned(),
        },
        _ => match changes.get("reason").and_then(Value::as_str) {
            Some("javascript_dialog_open") => {
                "a JavaScript dialog opened during the input (answer it with browser_dialog)"
                    .to_owned()
            }
            Some(reason) => format!("the page could not be read afterwards ({reason})"),
            None => "the page could not be read afterwards".to_owned(),
        },
    };
    format!("{said}{still}")
}

/// A drag Chrome took over as HTML5 drag and drop: the data it would carry.
fn intercepted_drag(event: &CdpEvent, cdp_session: &str) -> Option<Value> {
    (event.method == "Input.dragIntercepted" && event.session_id.as_deref() == Some(cdp_session))
    .then(|| event.params.get("data").cloned())
    .flatten()
}

pub struct BrowserPointerTool {
    def: ToolDef,
    engine: Arc<BrowserEngine>,
    /// What the read after the input is dispatched through.
    registry: ReplayRegistrySlot,
}

impl BrowserPointerTool {
    pub fn new(engine: Arc<BrowserEngine>) -> Self {
        Self::with_registry(engine, no_registry())
    }

    pub fn with_registry(engine: Arc<BrowserEngine>, registry: ReplayRegistrySlot) -> Self {
        Self {
            def: ToolDef {
                name: "browser_pointer".into(),
                description: "Hover, right-click, double-click, scroll, or drag in a bound tab, \
                    without activating it. Any ref from the outline can be pointed at (cards, \
                    cells and text too, not only lines offering an action); trusted input goes \
                    to the ref's live centre, refused as browser_target_covered when something \
                    else is on top there. Drag from a ref or x,y to destination_ref or to_x,to_y \
                    (HTML5 drag and drop included). Returns what the page changed in changes. \
                    dom_event requires a ref."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target_id": { "type": "string", "description": "Target id from get_browser_state." },
                        "tab_id": { "type": "string", "description": "Tab id from get_browser_state." },
                        "session": session_schema(),
                        "action": { "type": "string", "enum": ["hover", "right_click", "double_click", "scroll", "drag"] },
                        "input_route": { "type": "string", "enum": ["trusted", "dom_event"], "default": "trusted" },
                        "ref": { "type": "string", "description": "Origin page ref, instead of x,y." },
                        "x": { "type": "number", "description": "Origin viewport x in CSS pixels." },
                        "y": { "type": "number", "description": "Origin viewport y in CSS pixels." },
                        "destination_ref": { "type": "string", "description": "Drag destination ref (a list, a cell) in the same frame; dropped at its centre." },
                        "to_x": { "type": "number", "description": "Drag destination x in CSS pixels." },
                        "to_y": { "type": "number", "description": "Drag destination y in CSS pixels." },
                        "delta_x": { "type": "number", "description": "Horizontal scroll in CSS pixels." },
                        "delta_y": { "type": "number", "description": "Vertical scroll in CSS pixels." }
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
            registry,
        }
    }

    async fn resolve_ref(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
        validated: &ValidatedTab,
        external: &str,
    ) -> Result<ResolvedRef, ToolResult> {
        let entry = self
            .engine
            .store
            .resolve_ref(session, target_id, tab_id, external)
            .map_err(|refusal| refusal.to_tool_result())?;
        let cdp_session = self
            .engine
            .frame_session_for_mutation(session, target_id, tab_id, validated, &entry)
            .await
            .map_err(|refusal| refusal.to_tool_result())?;
        Ok(ResolvedRef {
            external: external.to_owned(),
            backend_node_id: entry.backend_node_id,
            frame: entry.frame,
            cdp_session,
        })
    }

    /// Where trusted input goes for one location: the point given, or the
    /// ref's live centre once the page says its element is on top there.
    async fn live(
        &self,
        validated: &ValidatedTab,
        (session, target_id, tab_id): (&str, &str, &str),
        location: &Location,
        reference: Option<&ResolvedRef>,
        noun: &'static str,
        scroll: bool,
    ) -> Result<(f64, f64), ToolResult> {
        match (location, reference) {
            (Location::Coordinates(x, y), None) => Ok((*x, *y)),
            (Location::Ref(_), Some(reference)) => {
                let held = HeldRef {
                    session,
                    target_id,
                    tab_id,
                    frame: &reference.frame,
                    named: &reference.external,
                };
                live_point(
                    &self.engine,
                    &validated.conn,
                    &reference.cdp_session,
                    reference.backend_node_id,
                    &held,
                    LiveInput::Pointer(noun),
                    scroll,
                )
                .await
                .map_err(|refusal| refusal.to_tool_result())
            }
            _ => unreachable!("resolution matches the request"),
        }
    }

    fn trusted_background_refusal(&self, validated: &ValidatedTab) -> Option<ToolResult> {
        // The activation limitation describes the remote-debugging route. Input
        // through the extension's chrome.debugger does not activate Chrome: on
        // the lane VM, trusted typing and clicks landed with Finder in front
        // and with Chrome fully covered (8 of 8), and Finder stayed frontmost.
        if validated.record.cdp_window_id.is_some()
            && validated.record.endpoint_transport != super::types::EndpointTransport::ExtensionRelay
        {
            if let Some(limitation) = self
                .engine
                .platform
                .standalone_trusted_input_background_limitation()
            {
                return Some(
                    BrowserRefusal::new(
                        BrowserRefusalCode::BrowserInputTrustUnavailable,
                        format!(
                            "{limitation}; use input_route=\"dom_event\" with refs to explicitly request synthetic full-background pointer delivery"
                        ),
                    )
                    .with_detail(json!({
                        "requested_route": "trusted",
                        "limitation": limitation,
                        "alternative_route": "dom_event",
                        "alternative_requires_ref": true,
                        "trusted_delivery_attempted": false,
                    }))
                    .to_tool_result(),
                );
            }
        }
        None
    }

    /// Synthetic DOM events on the ref's element. `Ok` carries a dialog the
    /// page opened while it handled them.
    async fn dom_event(
        &self,
        session: &str,
        request: &PointerRequest,
        validated: &ValidatedTab,
        origin: &ResolvedRef,
        destination: Option<&ResolvedRef>,
    ) -> Result<Option<Settled>, ToolResult> {
        let conn = &validated.conn;
        let object_id = element_of(
            conn,
            &origin.cdp_session,
            resolve_object(conn, &origin.cdp_session, origin.backend_node_id).await?,
        )
        .await?;

        if let Some((x, y)) = point_for_ref(conn, &origin.cdp_session, origin.backend_node_id).await
        {
            self.engine
                .visualize_browser_action(
                    session,
                    validated,
                    &origin.cdp_session,
                    x,
                    y,
                    visual_kind(request.action),
                )
                .await;
        }

        let (function, arguments) = match request.action {
            PointerAction::Hover => (
                "function() { const o={bubbles:true,composed:true,view:this.ownerDocument.defaultView}; this.dispatchEvent(new PointerEvent('pointerover',o)); this.dispatchEvent(new PointerEvent('pointerenter',{...o,bubbles:false})); this.dispatchEvent(new MouseEvent('mouseover',o)); this.dispatchEvent(new MouseEvent('mouseenter',{...o,bubbles:false})); return true; }",
                json!([]),
            ),
            PointerAction::RightClick => (
                "function() { const o={bubbles:true,cancelable:true,composed:true,button:2,buttons:2,view:this.ownerDocument.defaultView}; this.dispatchEvent(new PointerEvent('pointerdown',o)); this.dispatchEvent(new MouseEvent('mousedown',o)); this.dispatchEvent(new MouseEvent('mouseup',{...o,buttons:0})); this.dispatchEvent(new PointerEvent('pointerup',{...o,buttons:0})); this.dispatchEvent(new MouseEvent('contextmenu',{...o,buttons:0})); return true; }",
                json!([]),
            ),
            PointerAction::DoubleClick => (
                "function() { const o={bubbles:true,cancelable:true,composed:true,button:0,view:this.ownerDocument.defaultView}; for (let detail=1; detail<=2; detail++) { this.dispatchEvent(new PointerEvent('pointerdown',{...o,buttons:1,detail})); this.dispatchEvent(new MouseEvent('mousedown',{...o,buttons:1,detail})); this.dispatchEvent(new MouseEvent('mouseup',{...o,buttons:0,detail})); this.dispatchEvent(new PointerEvent('pointerup',{...o,buttons:0,detail})); this.dispatchEvent(new MouseEvent('click',{...o,buttons:0,detail})); } this.dispatchEvent(new MouseEvent('dblclick',{...o,buttons:0,detail:2})); return true; }",
                json!([]),
            ),
            PointerAction::Scroll => (
                "function(dx,dy) { const o={deltaX:dx,deltaY:dy,bubbles:true,cancelable:true,composed:true,view:this.ownerDocument.defaultView}; this.dispatchEvent(new WheelEvent('wheel',o)); let n=this; while (n && n !== this.ownerDocument.documentElement) { const s=this.ownerDocument.defaultView.getComputedStyle(n); if (/(auto|scroll)/.test(s.overflow+s.overflowX+s.overflowY)) break; n=n.parentElement; } const target=n || this.ownerDocument.scrollingElement || this.ownerDocument.documentElement; const beforeX=target.scrollLeft; const beforeY=target.scrollTop; target.scrollBy(dx,dy); const changed=target.scrollLeft !== beforeX || target.scrollTop !== beforeY; if (changed) target.dispatchEvent(new Event('scroll')); return changed; }",
                json!([{ "value": request.delta_x }, { "value": request.delta_y }]),
            ),
            PointerAction::Drag => {
                let destination_argument = if let Some(destination) = destination {
                    let object_id = element_of(
                        conn,
                        &destination.cdp_session,
                        resolve_object(conn, &destination.cdp_session, destination.backend_node_id)
                            .await?,
                    )
                    .await?;
                    json!([{ "objectId": object_id }, { "value": Value::Null }, { "value": Value::Null }])
                } else if let Some(Location::Coordinates(x, y)) = &request.destination {
                    if origin.frame.kind != FrameKind::Main {
                        return Err(BrowserRefusal::new(
                            BrowserRefusalCode::BrowserWrongTargetRefused,
                            "coordinate drag destinations are only provably in the same frame for a main-frame origin; use destination_ref for iframe or OOPIF drag",
                        )
                        .to_tool_result());
                    }
                    json!([{ "value": Value::Null }, { "value": x }, { "value": y }])
                } else {
                    unreachable!("drag destination validated")
                };
                (
                    "function(destination,x,y) { const doc=this.ownerDocument; const dest=destination || doc.elementFromPoint(x,y); if (!dest || dest.ownerDocument !== doc) return false; let data=null; try { data=new DataTransfer(); } catch (_) {} const common={bubbles:true,cancelable:true,composed:true}; this.dispatchEvent(new PointerEvent('pointerdown',{...common,button:0,buttons:1})); this.dispatchEvent(new MouseEvent('mousedown',{...common,button:0,buttons:1})); this.dispatchEvent(new DragEvent('dragstart',{...common,dataTransfer:data})); dest.dispatchEvent(new DragEvent('dragenter',{...common,dataTransfer:data})); dest.dispatchEvent(new DragEvent('dragover',{...common,dataTransfer:data})); dest.dispatchEvent(new DragEvent('drop',{...common,dataTransfer:data})); this.dispatchEvent(new DragEvent('dragend',{...common,dataTransfer:data})); dest.dispatchEvent(new MouseEvent('mouseup',{...common,button:0,buttons:0})); dest.dispatchEvent(new PointerEvent('pointerup',{...common,button:0,buttons:0})); return true; }",
                    destination_argument,
                )
            }
        };

        let mut delivery = Delivery {
            engine: &self.engine,
            conn,
            cdp: &origin.cdp_session,
            target: validated.tab.cdp_target_id.as_str(),
            opened: None,
            sent: 0,
        };
        match delivery
            .send(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": function,
                    "arguments": arguments,
                    "returnByValue": true,
                }),
            )
            .await
        {
            Ok(None) => {
                Err(dialog_open_refusal(delivery.opened.as_ref().expect("blocked by a dialog"))
                    .to_tool_result())
            }
            Ok(Some(_)) if delivery.opened.is_some() => Ok(delivery.opened.map(Settled::Dialog)),
            Ok(Some(value))
                if value.get("exceptionDetails").is_none()
                    && (!matches!(request.action, PointerAction::Scroll | PointerAction::Drag)
                        || value.pointer("/result/value").and_then(Value::as_bool)
                            != Some(false)) =>
            {
                Ok(None)
            }
            Ok(Some(value)) if value.get("exceptionDetails").is_some() => Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                format!(
                    "synthetic {} raised a page-side exception and delivery was not proven",
                    request.action.as_str()
                ),
            )
            .to_tool_result()),
            Ok(Some(_)) if request.action == PointerAction::Drag => Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the drag destination did not resolve in the origin ref's exact document",
            )
            .to_tool_result()),
            Ok(Some(_)) => Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserActionUnavailable,
                "the synthetic scroll target did not move, so delivery was not proven",
            )
            .to_tool_result()),
            Err(error) => Err(ToolResult::error(format!(
                "synthetic {} failed: {error}",
                request.action.as_str()
            ))),
        }
    }

    /// Trusted CDP mouse input at `origin` (and `destination` for a drag).
    /// `Ok` carries a dialog the page opened while it handled the input.
    async fn trusted(
        &self,
        session: &str,
        request: &PointerRequest,
        validated: &ValidatedTab,
        cdp_session: &str,
        origin: (f64, f64),
        destination: Option<(f64, f64)>,
    ) -> Result<Option<Settled>, ToolResult> {
        let conn = &validated.conn;
        self.engine
            .visualize_browser_action(
                session,
                validated,
                cdp_session,
                origin.0,
                origin.1,
                visual_kind(request.action),
            )
            .await;

        if let Err(error) = conn
            .call(
                Some(cdp_session),
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
            )
            .await
        {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                format!("the target tab could not enter CDP focus emulation: {error}"),
            )
            .to_tool_result());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;

        let mut delivery = Delivery {
            engine: &self.engine,
            conn,
            cdp: cdp_session,
            target: validated.tab.cdp_target_id.as_str(),
            opened: None,
            sent: 0,
        };
        let sent = dispatch_trusted(
            &mut delivery,
            conn.clone(),
            &validated.cdp_session,
            request,
            origin,
            destination,
        )
        .await;
        let opened = delivery.opened.take();
        // Nothing of the action itself went out (at most the move that
        // brings the pointer to the element).
        let nothing_sent = match &sent {
            Ok(preparing) => delivery.sent <= *preparing,
            Err(_) => delivery.sent == 0,
        };
        // With a dialog up the page answers nothing, this included: the
        // emulation ends with the attachment session instead.
        let cleanup = if opened.is_some() {
            Ok(json!({}))
        } else {
            conn.call(
                Some(cdp_session),
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": false }),
            )
            .await
        };
        // A dialog that opened before the action itself went out (before
        // the press, or during the move to the element): nothing was done,
        // and the result must not say it was.
        if let (Some(dialog), true) = (&opened, nothing_sent) {
            return Err(dialog_open_refusal(dialog).to_tool_result());
        }
        if let Err(error) = sent {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                format!(
                    "trusted {} failed ({error}); no synthetic fallback was attempted",
                    request.action.as_str()
                ),
            )
            .to_tool_result());
        }
        if let Err(error) = cleanup {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserInputTrustUnavailable,
                format!(
                    "trusted {} was acknowledged but focus emulation could not be restored ({error}); delivery is unknown and must not be retried automatically",
                    request.action.as_str()
                ),
            )
            .with_detail(json!({ "delivery": "unknown", "retryable": false }))
            .to_tool_result());
        }
        Ok(opened.map(Settled::Dialog))
    }
}

/// Send the mouse events. A dialog the page opens stops the sending; the
/// caller finds it in `delivery.opened`.
async fn dispatch_trusted(
    delivery: &mut Delivery<'_>,
    conn: Arc<CdpConnection>,
    page: &str,
    request: &PointerRequest,
    origin: (f64, f64),
    destination: Option<(f64, f64)>,
) -> anyhow::Result<usize> {
    // Calls that only bring the pointer to the element; what comes after
    // them is the requested action itself.
    let mut preparing = delivery.sent;
    match request.action {
        // The move is the hover itself.
        PointerAction::Hover => {
            delivery
                .send(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": origin.0, "y": origin.1, "button": "none" }),
                )
                .await?;
        }
        PointerAction::RightClick => {
            for kind in ["mousePressed", "mouseReleased"] {
                delivery
                    .send(
                        "Input.dispatchMouseEvent",
                        json!({ "type": kind, "x": origin.0, "y": origin.1, "button": "right", "clickCount": 1 }),
                    )
                    .await?;
            }
        }
        PointerAction::DoubleClick => {
            delivery
                .send(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": origin.0, "y": origin.1, "button": "none" }),
                )
                .await?;
            preparing = delivery.sent;
            for click_count in [1, 2] {
                for kind in ["mousePressed", "mouseReleased"] {
                    delivery
                        .send(
                            "Input.dispatchMouseEvent",
                            json!({ "type": kind, "x": origin.0, "y": origin.1, "button": "left", "clickCount": click_count }),
                        )
                        .await?;
                }
            }
        }
        PointerAction::Scroll => {
            delivery
                .send(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseWheel", "x": origin.0, "y": origin.1, "deltaX": request.delta_x, "deltaY": request.delta_y }),
                )
                .await?;
        }
        PointerAction::Drag => {
            preparing = drag(
                delivery,
                conn,
                page,
                origin,
                destination.expect("drag destination validated"),
            )
            .await?;
        }
    }
    Ok(preparing)
}

/// Press at `origin`, move in steps, release at `destination`. Drags are
/// intercepted first: an element that starts an HTML5 drag-and-drop would
/// otherwise hand the drag to the operating system, which follows the real
/// mouse, not these events. Chrome then reports the drag's data instead
/// (`Input.dragIntercepted`), and the drop is carried to the destination as
/// drag events. A drag that page script draws from pointer events (dragula,
/// SortableJS's fallback) is never intercepted and is the moves themselves,
/// a frame apart, so a library that tracks the pointer per animation frame
/// sees each one. Whatever stops the drag part way, the button is released
/// and interception is turned off again.
///
/// `page` is the tab's own session: Chrome intercepts a drag, and reports
/// it, on the top-level page even when the element is in an out-of-process
/// frame, whose own session (`delivery.cdp`) takes the mouse and drag events.
async fn drag(
    delivery: &mut Delivery<'_>,
    owned: Arc<CdpConnection>,
    page: &str,
    origin: (f64, f64),
    destination: (f64, f64),
) -> anyhow::Result<usize> {
    let conn = delivery.conn;
    let cdp = page;
    // Armed before interception is asked for: a call cancelled at any point
    // after it still turns interception off. It holds the event stream, so a
    // drag Chrome took over during a move that was cancelled is still found.
    let mut cleanup = DragCleanup {
        events: conn.subscribe(),
        conn: owned,
        cdp: cdp.to_owned(),
        input_cdp: delivery.cdp.to_owned(),
        release_at: destination,
        pressed: false,
        intercepted: None,
        armed: true,
    };
    // Without interception an HTML5 drag would go to the operating system
    // and never end: send nothing.
    conn.call(Some(cdp), "Input.setInterceptDrags", json!({ "enabled": true }))
        .await
        .map_err(|error| anyhow::anyhow!("Chrome would not intercept the drag ({error}), so none was started"))?;
    let DragCleanup { pressed, intercepted, events, .. } = &mut cleanup;
    let mut preparing = delivery.sent;
    let result = async {
        delivery
            .send(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": origin.0, "y": origin.1, "button": "none" }),
            )
            .await?;
        preparing = delivery.sent;
        *pressed = delivery
            .send(
                "Input.dispatchMouseEvent",
                json!({ "type": "mousePressed", "x": origin.0, "y": origin.1, "button": "left", "buttons": 1, "clickCount": 1 }),
            )
            .await?
            .is_some();
        let take = |events: &mut tokio::sync::mpsc::UnboundedReceiver<CdpEvent>| {
            std::iter::from_fn(|| events.try_recv().ok()).find_map(|event| intercepted_drag(&event, cdp))
        };
        let mut data = None;
        for step in 1..=DRAG_STEPS {
            tokio::time::sleep(DRAG_FRAME).await;
            let progress = f64::from(step) / f64::from(DRAG_STEPS);
            let x = origin.0 + (destination.0 - origin.0) * progress;
            let y = origin.1 + (destination.1 - origin.1) * progress;
            delivery
                .send(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": x, "y": y, "button": "left", "buttons": 1 }),
                )
                .await?;
            data = take(events);
            if data.is_some() {
                break;
            }
        }
        if data.is_none() && delivery.opened.is_none() {
            let deadline = tokio::time::Instant::now() + DRAG_INTERCEPT_WAIT;
            while data.is_none() {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Some(event)) => data = intercepted_drag(&event, cdp),
                    _ => break,
                }
            }
        }
        if let Some(data) = data {
            *intercepted = Some(data.clone());
            let mut dropped = false;
            for kind in ["dragEnter", "dragOver", "drop"] {
                dropped = delivery
                    .send(
                        "Input.dispatchDragEvent",
                        json!({ "type": kind, "x": destination.0, "y": destination.1, "data": data }),
                    )
                    .await?
                    .is_some();
            }
            // Dropped: nothing left to cancel. A dialog that stopped the drop
            // leaves the drag to be cancelled.
            if dropped && delivery.opened.is_none() {
                *intercepted = None;
            }
        } else {
            // The page's own drag: let it see the pointer rest over the drop.
            tokio::time::sleep(DRAG_FRAME * 3).await;
        }
        delivery
            .send(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseReleased", "x": destination.0, "y": destination.1, "button": "left", "buttons": 0, "clickCount": 1 }),
            )
            .await?;
        *pressed = false;
        anyhow::Ok(())
    }
    .await;
    cleanup.finish(delivery.opened.is_some()).await;
    result.map(|()| preparing)
}

/// What a drag leaves to undo: the pressed button and drag interception.
/// `finish` undoes it on every way out of the drag; if the call is cancelled
/// instead (its future dropped), the drop undoes it in the background.
/// Interception left on would swallow the user's own drags in this tab.
struct DragCleanup {
    events: tokio::sync::mpsc::UnboundedReceiver<CdpEvent>,
    conn: Arc<CdpConnection>,
    /// The page's session: interception and its events.
    cdp: String,
    /// The element's frame session: the mouse and drag events.
    input_cdp: String,
    release_at: (f64, f64),
    pressed: bool,
    /// The data of an HTML5 drag Chrome handed over and that was not
    /// dropped: until it is cancelled the tab takes no mouse input.
    intercepted: Option<Value>,
    armed: bool,
}

impl DragCleanup {
    /// The drag Chrome took over, when no drop has ended it: what was seen,
    /// else an interception still waiting in the event stream.
    fn undropped(&mut self) -> Option<Value> {
        if self.intercepted.is_none() && self.pressed {
            let cdp = self.cdp.clone();
            self.intercepted = std::iter::from_fn(|| self.events.try_recv().ok())
                .find_map(|event| intercepted_drag(&event, &cdp));
        }
        self.intercepted.clone()
    }

    async fn finish(mut self, dialog_open: bool) {
        // A page behind a dialog answers nothing, so no release then; the
        // browser answers the interception call even with a dialog up.
        let cancel = self.undropped().map(|data| (self.release_at, data));
        self.pressed &= !dialog_open;
        undo_drag(
            &self.conn,
            &self.cdp,
            &self.input_cdp,
            self.pressed.then_some(self.release_at),
            cancel,
        )
        .await;
        // Disarmed only once undone: a call cancelled meanwhile undoes it
        // again from the drop (twice is harmless).
        self.armed = false;
    }
}

impl Drop for DragCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (conn, cdp, input) = (self.conn.clone(), self.cdp.clone(), self.input_cdp.clone());
        let cancel = self.undropped().map(|data| (self.release_at, data));
        let release = self.pressed.then_some(self.release_at);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { undo_drag(&conn, &cdp, &input, release, cancel).await });
        }
    }
}

async fn undo_drag(
    conn: &CdpConnection,
    cdp: &str,
    input: &str,
    release: Option<(f64, f64)>,
    cancel: Option<((f64, f64), Value)>,
) {
    if let Some(((x, y), data)) = cancel {
        let _ = tokio::time::timeout(
            CLEANUP_TIMEOUT,
            conn.call(
                Some(input),
                "Input.dispatchDragEvent",
                json!({ "type": "dragCancel", "x": x, "y": y, "data": data }),
            ),
        )
        .await;
    }
    if let Some((x, y)) = release {
        let _ = tokio::time::timeout(
            CLEANUP_TIMEOUT,
            conn.call(
                Some(input),
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 }),
            ),
        )
        .await;
    }
    let _ = tokio::time::timeout(
        CLEANUP_TIMEOUT,
        conn.call(Some(cdp), "Input.setInterceptDrags", json!({ "enabled": false })),
    )
    .await;
}


#[async_trait]
impl Tool for BrowserPointerTool {
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
            browser_protected_resource_scope(&self.engine, args, "browser_pointer").await
        } else {
            Ok(None)
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let target_id = match args.require_str("target_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let tab_id = match args.require_str("tab_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let session = match require_session(&args) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let request = match parse_request(&args) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(error),
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
            Ok(value) => value,
            Err(refusal) => return refusal.to_tool_result(),
        };
        self.engine.note_pip_window(&validated);
        if request.route == InputRoute::Trusted {
            if let Some(refusal) = self.trusted_background_refusal(&validated) {
                return refusal;
            }
        }

        // What the session holds now is what the changes are made against.
        let held = self.engine.held_view(&session, &target_id, &tab_id);
        // A double-click can open a JavaScript dialog, and a page behind one
        // answers nothing: hear about it, and send nothing into one already up.
        let cdp_target = validated.tab.cdp_target_id.as_str();
        self.engine
            .watch_dialogs(&validated.conn, &validated.cdp_session, cdp_target)
            .await;
        if let Some(dialog) = validated.conn.dialog_state(cdp_target) {
            return dialog_open_refusal(&dialog).to_tool_result();
        }

        let origin_ref = match &request.origin {
            Location::Ref(external) => match self
                .resolve_ref(&session, &target_id, &tab_id, &validated, external)
                .await
            {
                Ok(reference) => Some(reference),
                Err(result) => return result,
            },
            Location::Coordinates(_, _) => None,
        };
        let destination_ref = match &request.destination {
            Some(Location::Ref(external)) => match self
                .resolve_ref(&session, &target_id, &tab_id, &validated, external)
                .await
            {
                Ok(reference) => Some(reference),
                Err(result) => return result,
            },
            _ => None,
        };

        if let (Some(origin), Some(destination)) = (&origin_ref, &destination_ref) {
            if !same_exact_frame(&origin.frame, &destination.frame)
                || origin.cdp_session != destination.cdp_session
            {
                return BrowserRefusal::new(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    "drag origin and destination refs must belong to the exact same live frame",
                )
                .to_tool_result();
            }
        }
        if request.action == PointerAction::Drag
            && origin_ref.is_none()
            && destination_ref.is_some()
        {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "a coordinate drag origin cannot prove it shares a frame with destination_ref; use two refs or two coordinate pairs",
            )
            .to_tool_result();
        }
        if request.action == PointerAction::Drag
            && matches!(&request.destination, Some(Location::Coordinates(_, _)))
            && origin_ref
                .as_ref()
                .is_some_and(|reference| reference.frame.kind != FrameKind::Main)
        {
            return BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "a viewport-coordinate drag destination is only in the same provable frame as a main-frame ref; use destination_ref for iframe or OOPIF drag",
            )
            .to_tool_result();
        }

        // From before the input: a navigation it sets off is seen starting.
        let mut watch = self.engine.page_watch(&validated).await;
        let reads = changes_will_be_read(&self.registry, held);
        let (sent, points) = match request.route {
            InputRoute::DomEvent => (
                {
                    if reads {
                        self.engine.count_from_here(&validated, &mut watch).await;
                    }
                    self.dom_event(
                    &session,
                    &request,
                    &validated,
                    origin_ref.as_ref().expect("dom route requires ref"),
                    destination_ref.as_ref(),
                )
                .await
                },
                None,
            ),
            InputRoute::Trusted => {
                let cdp_session = origin_ref
                    .as_ref()
                    .map(|reference| reference.cdp_session.clone())
                    .unwrap_or_else(|| validated.cdp_session.clone());
                let ids = (session.as_str(), target_id.as_str(), tab_id.as_str());
                let origin = match self
                    .live(&validated, ids, &request.origin, origin_ref.as_ref(), request.action.noun(), true)
                    .await
                {
                    Ok(point) => point,
                    Err(result) => return result,
                };
                let destination = match &request.destination {
                    Some(location) => match self
                        .live(&validated, ids, location, destination_ref.as_ref(), "drop", true)
                        .await
                    {
                        Ok(point) => Some(point),
                        Err(result) => return result,
                    },
                    None => None,
                };
                // Scrolling to the drop point may have moved the origin: look
                // at it again where it is now, without scrolling.
                let origin = match (&destination_ref, &origin_ref) {
                    (Some(_), Some(reference)) => match self
                        .live(&validated, ids, &request.origin, Some(reference), "drag", false)
                        .await
                    {
                        Ok(point) => point,
                        Err(_) => {
                            return BrowserRefusal::new(
                                BrowserRefusalCode::BrowserTargetCovered,
                                format!(
                                    "{} and {} are not both reachable at once: once the drop \
                                     point was scrolled into view, the centre of {} was out of \
                                     view or covered, so no drag was sent. Scroll so both are \
                                     visible, or drag in shorter moves",
                                    reference.external,
                                    destination_ref.as_ref().map_or("", |r| r.external.as_str()),
                                    reference.external
                                ),
                            )
                            .with_detail(json!({ "input_sent": false }))
                            .to_tool_result()
                        }
                    },
                    _ => origin,
                };
                if reads {
                    self.engine.count_from_here(&validated, &mut watch).await;
                }
                (
                    self.trusted(
                        &session,
                        &request,
                        &validated,
                        &cdp_session,
                        origin,
                        destination,
                    )
                    .await,
                    Some((origin, destination)),
                )
            }
        };
        let opened = match sent {
            Ok(opened) => opened,
            Err(refused) => return refused,
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

        let at = |point: Option<(f64, f64)>| {
            point.map_or(String::new(), |(x, y)| format!(" at ({x:.0}, {y:.0})"))
        };
        let origin_named = match &request.origin {
            Location::Ref(reference) => reference.clone(),
            Location::Coordinates(_, _) => "the point".to_owned(),
        };
        let mut said = format!(
            "{} {origin_named}{}",
            request.action.past(),
            at(points.map(|(origin, _)| origin))
        );
        if let Some(destination) = &request.destination {
            let named = match destination {
                Location::Ref(reference) => reference.clone(),
                Location::Coordinates(_, _) => "the point".to_owned(),
            };
            said.push_str(&format!(
                " to {named}{}",
                at(points.and_then(|(_, destination)| destination))
            ));
        }
        if request.action == PointerAction::Scroll {
            said.push_str(&format!(" by ({}, {})", request.delta_x, request.delta_y));
        }
        let route = match request.route {
            InputRoute::Trusted => "",
            InputRoute::DomEvent => " with synthetic DOM events (trust-gated handlers may ignore them)",
        };
        said.push_str(&format!(
            "{route} in {tab_id}; {}",
            changes_summary(changes.as_ref())
        ));

        let external = |reference: Option<&ResolvedRef>| reference.map(|r| r.external.clone());
        let structured = json!({
            "status": "ok",
            "action": request.action.as_str(),
            "route": request.route.as_str(),
            "target_id": target_id,
            "tab_id": tab_id,
            "ref": external(origin_ref.as_ref()),
            "frame": origin_ref.as_ref().map(|r| r.frame.kind.as_str()),
            "destination_ref": external(destination_ref.as_ref()),
            "x": points.map(|(origin, _)| origin.0),
            "y": points.map(|(origin, _)| origin.1),
            "to_x": points.and_then(|(_, destination)| destination).map(|point| point.0),
            "to_y": points.and_then(|(_, destination)| destination).map(|point| point.1),
            "delta_x": (request.action == PointerAction::Scroll).then_some(request.delta_x),
            "delta_y": (request.action == PointerAction::Scroll).then_some(request.delta_y),
        });
        with_changes(ToolResult::text(said).with_structured(structured), changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_action_and_coordinate_origin() {
        for action in ["hover", "right_click", "double_click"] {
            let parsed = parse_request(&json!({
                "action": action,
                "x": 12.5,
                "y": 8,
            }))
            .unwrap();
            assert_eq!(parsed.origin, Location::Coordinates(12.5, 8.0));
            assert_eq!(parsed.route, InputRoute::Trusted);
        }
    }

    #[test]
    fn dom_event_requires_a_ref() {
        let error = parse_request(&json!({
            "action": "hover",
            "input_route": "dom_event",
            "x": 1,
            "y": 2,
        }))
        .unwrap_err();
        assert!(error.contains("requires a ref"), "{error}");
    }

    #[test]
    fn drag_requires_exactly_one_destination_shape() {
        assert!(parse_request(&json!({ "action": "drag", "ref": "p1:0" }))
            .unwrap_err()
            .contains("requires destination"));
        assert!(parse_request(&json!({
            "action": "drag",
            "ref": "p1:0",
            "destination_ref": "p1:1",
            "to_x": 3,
            "to_y": 4,
        }))
        .unwrap_err()
        .contains("not both"));
    }

    #[test]
    fn scroll_requires_a_nonzero_finite_delta() {
        assert!(parse_request(&json!({ "action": "scroll", "ref": "p1:0" }))
            .unwrap_err()
            .contains("non-zero"));
        let parsed = parse_request(&json!({
            "action": "scroll",
            "ref": "p1:0",
            "delta_y": 240,
        }))
        .unwrap();
        assert_eq!(parsed.delta_y, 240.0);
    }

    #[test]
    fn exact_frame_comparison_includes_document_identity() {
        let first = FrameRef::main_unproven();
        let second = FrameRef::main_unproven();
        assert!(same_exact_frame(&first, &second));

        let navigated = FrameRef {
            kind: FrameKind::Main,
            oopif_target_id: None,
            identity: Some(super::super::store::FrameIdentity {
                frame_id: "main".into(),
                loader_id: "new-loader".into(),
            }),
        };
        assert!(!same_exact_frame(&first, &navigated));
    }

    #[test]
    fn a_result_says_what_the_page_did_never_a_bare_ok() {
        let table = [
            (None, "the page was not read afterwards"),
            (
                Some(json!({"kind": "diff", "ops": []})),
                "nothing in the page's outline changed",
            ),
            (
                Some(json!({"kind": "diff", "ops": [
                    {"op": "move", "ref": "p1:3"}, {"op": "change", "ref": "p1:4"},
                    {"op": "move", "ref": "p1:5"}]})),
                "the page changed: 1 changed, 2 moved line(s), listed in changes",
            ),
            (
                Some(json!({"kind": "snapshot", "reason": "document_changed", "outline": "-"})),
                "a new document replaced the page",
            ),
            (
                Some(json!({"kind": "unavailable", "reason": "javascript_dialog_open"})),
                "a JavaScript dialog opened",
            ),
            (
                Some(json!({"kind": "diff", "ops": [{"op": "add", "ref": "p1:9"}], "settled": false})),
                "1 added line(s), listed in changes; the page had not settled",
            ),
        ];
        for (changes, says) in table {
            let said = changes_summary(changes.as_ref());
            assert!(said.contains(says), "{said:?} should say {says:?}");
        }
    }

    #[test]
    fn only_this_tabs_intercepted_drag_carries_data() {
        let event = |session: Option<&str>, method: &str| CdpEvent {
            method: method.into(),
            session_id: session.map(str::to_owned),
            params: json!({"data": {"items": [], "dragOperationsMask": 1}}),
        };
        assert!(intercepted_drag(&event(Some("s1"), "Input.dragIntercepted"), "s1").is_some());
        assert!(intercepted_drag(&event(None, "Input.dragIntercepted"), "s1").is_none());
        assert!(intercepted_drag(&event(Some("s2"), "Input.dragIntercepted"), "s1").is_none());
        assert!(intercepted_drag(&event(Some("s1"), "Page.frameNavigated"), "s1").is_none());
    }
}
