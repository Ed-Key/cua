//! BrowserEngine — the semantic core behind the five browser tools.
//!
//! Owns the target/ref store, the CDP connection pool, and every
//! exact-or-refused decision. The platform adapter is consulted for OS
//! identity only; nothing here trusts a cached fact across a mutation
//! boundary — [`BrowserEngine::revalidate_for_mutation`] re-proves the
//! full chain (process fingerprint → native ownership/bounds → endpoint
//! ownership → CDP target type/window) before any input or navigation,
//! and [`BrowserEngine::frame_session_for_mutation`] additionally
//! re-proves the ref's frame/document identity (frame id + loader id,
//! and for OOPIFs the child target attached beneath the proven tab)
//! before the ref is touched.
//!
//! Snapshot composition (v2 DOM-ref slice):
//! - Shadow DOM is composed into the main-frame walk (`pierce: true`),
//!   skipping user-agent shadow roots.
//! - Same-process iframes are walked via `contentDocument` and their
//!   refs carry the child frame's identity from `Page.getFrameTree`.
//! - OOPIFs are reached only when `Target.setAutoAttach` capability-
//!   tests successfully on the tab's own session; child sessions are
//!   accepted solely from `Target.attachedToTarget` events scoped to
//!   that session (containment beneath the proven tab target).
//! - Whenever a frame's identity cannot be proven, its content is
//!   omitted from the snapshot — never guessed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::session::register_scoped_fallible_session_end_hook;

use super::binding::{
    cardinality_exact_candidate, correlate, selected_tab_target_id, BindingOutcome,
    CdpWindowCandidate,
};
use super::cdp_ws::{CdpConnection, CdpPool};
use super::grant::{ExistingProfileGrant, ExistingProfileGrants, GrantLookup};
use super::mutation::{MutationGates, MutationKey};
use super::observation::{
    DocumentIdentity, Fingerprint, FullReason, TabRefs, Told, ViewKind, ViewLine, ViewNode,
};
use super::platform::{
    BrowserConsentOutcome, BrowserConsentRequest, BrowserPlatform, BrowserVisualAction,
    BrowserVisualActionKind, ExistingProfileSetupRequest,
};
use super::prepare::ManagedBrowsers;
use super::reconnect::{ReconnectGates, ReconnectKey};
use super::refusal::{BrowserRefusal, BrowserRefusalCode};
use super::semantic::{
    ax_reading, build_dom_index, build_layout_index, compose_accessibility_tree, dom_reading,
    listed_ref, parse_viewport, snapshot_document_title, OmissionCounts, SemanticDocument,
    SemanticNode, DEFAULT_SEMANTIC_NODE_BUDGET, SEMANTIC_COMPUTED_STYLES,
};
use super::store::{
    format_ref, BrowserStore, FrameIdentity, FrameKind, FrameRef, RefEntry, SemanticCache,
    SemanticContinuation, SnapshotRecord, TabRecord, TargetRecord,
};
use super::types::{
    BindingQuality, BrowserClassification, BrowserEngineFamily, BrowserProcessRole,
    EndpointAccessClass, NativeWindowInfo, OwnedEndpoint, Rect,
};

/// Bounds tolerance (device pixels) for native ↔ CDP window correlation.
/// Absorbs window-shadow and DIP-rounding differences.
pub const BOUNDS_TOLERANCE_PX: f64 = 8.0;

/// Cap on refs minted per snapshot — keeps snapshots bounded on
/// pathological pages. The truncation is reported in the tool output.
pub const MAX_REFS_PER_SNAPSHOT: usize = 300;
/// Hard cap for decoded tab screenshots returned through MCP. This bounds a
/// compromised or malformed endpoint before its response reaches consumers.
const MAX_BROWSER_SCREENSHOT_BYTES: usize = 16 * 1024 * 1024;

pub struct BrowserEngine {
    pub(crate) platform: Arc<dyn BrowserPlatform>,
    pub(crate) store: BrowserStore,
    pub(crate) pool: Arc<CdpPool>,
    pub(crate) managed_browsers: ManagedBrowsers,
    pub(crate) existing_profile_grants: ExistingProfileGrants,
    pub(crate) approval_broker: Arc<crate::consent::ApprovalBroker>,
    pub(crate) protected_resource_ownership: Arc<crate::consent::ProtectedResourceOwnershipStore>,
    mutation_gates: MutationGates,
    /// One observation of a tab at a time per session: collecting the page
    /// and recording it happen as one step, so revisions are ordered.
    observation_gates: super::keyed_gates::KeyedGates<(String, String, String)>,
    pub(crate) reconnect_gates: ReconnectGates,
    pending_existing_profile_cleanups: Mutex<HashMap<String, Vec<ExistingProfileSetupRequest>>>,
    session_end_hook: Mutex<Option<crate::session::SessionEndHookRegistration>>,
}

fn refuse(code: BrowserRefusalCode, msg: impl Into<String>) -> BrowserRefusal {
    BrowserRefusal::new(code, msg)
}

fn authorize_live_browser_origin(
    manifest: Option<&crate::session_manifest::SessionManifest>,
    live_url: &str,
) -> Result<(), BrowserRefusal> {
    let manifest = manifest.ok_or_else(|| {
        refuse(
            BrowserRefusalCode::BrowserOriginOutsideScope,
            "the capability manifest is unavailable",
        )
    })?;
    manifest
        .authorize_browser_url(live_url)
        .map_err(|error| refuse(BrowserRefusalCode::BrowserOriginOutsideScope, error))
}

fn protected_live_origin_scope(raw: &str) -> Result<String, BrowserRefusal> {
    if raw.trim() == "about:blank" {
        return Ok("about:blank".to_owned());
    }
    let parsed = url::Url::parse(raw).map_err(|_| {
        refuse(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the live top-level browser URL is invalid",
        )
    })?;
    let origin = parsed.origin().ascii_serialization();
    if origin != "null" {
        return Ok(origin);
    }

    // Opaque origins (for example file: and data:) cannot safely share one
    // generic "null" grant. Bind them to a one-way document digest without
    // sending the full path/query/fragment to the consent provider.
    let digest = Sha256::digest(raw.as_bytes());
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("{}:sha256:{digest}", parsed.scheme()))
}

fn route_err(context: &str, err: impl std::fmt::Display) -> BrowserRefusal {
    refuse(
        BrowserRefusalCode::BrowserRouteUnavailable,
        format!("{context}: {err}"),
    )
}

pub(super) fn unsupported_engine_refusal(
    classification: &BrowserClassification,
    operation: &'static str,
) -> BrowserRefusal {
    let (message, protocol, limitation) = match classification.engine {
        BrowserEngineFamily::Gecko => (
            "Firefox browser tools require a WebDriver BiDi or Remote Agent route; attaching to an ordinary running profile is not supported",
            "webdriver_bidi",
            "remote_agent_requires_launch_time_enablement",
        ),
        BrowserEngineFamily::Webkit => (
            "Safari browser tools require a WebKit-native automation route; Safari does not expose an attachable CDP endpoint for an ordinary running profile",
            "webkit_automation",
            "no_attachable_runtime_endpoint",
        ),
        BrowserEngineFamily::Chromium => (
            "this Chromium-family browser does not expose a supported CDP route",
            "cdp",
            "cdp_unavailable",
        ),
        BrowserEngineFamily::Unknown => (
            "this browser engine has no supported typed browser route",
            "unknown",
            "engine_route_unavailable",
        ),
    };
    BrowserRefusal::new(BrowserRefusalCode::BrowserRouteUnavailable, message).with_detail(json!({
        "operation": operation,
        "engine_family": classification.engine,
        "product": classification.product_kind,
        "required_protocol": protocol,
        "limitation": limitation,
    }))
}

/// Name the exact call that approves a standalone profile, and what it costs,
/// on the consent refusal a bind returns.
fn existing_profile_next_call(
    refusal: BrowserRefusal,
    pid: i64,
    window_id: u64,
    extension_connected: bool,
) -> BrowserRefusal {
    if refusal.code != BrowserRefusalCode::BrowserConsentRequired {
        return refusal;
    }
    let mut detail = refusal.detail.clone().unwrap_or_else(|| json!({}));
    detail["next_call"] = json!({
        "tool": "browser_prepare",
        "arguments": {
            "pid": pid,
            "window_id": window_id,
            "strategy": { "kind": "existing_profile" },
        },
    });
    detail["extension_connected"] = json!(extension_connected);
    let call = format!(
        "browser_prepare {{\"pid\": {pid}, \"window_id\": {window_id}, \"strategy\": \
         {{\"kind\": \"existing_profile\"}}}}"
    );
    BrowserRefusal {
        message: if extension_connected {
            format!(
                "this Chrome profile needs existing-profile approval from this session's \
                 approval host or capability manifest: call {call}, then get_browser_state \
                 again. cua's Chrome extension is connected, so attaching changes no browser \
                 settings."
            )
        } else {
            format!(
                "this Chrome profile needs existing-profile approval before Cua can read it: \
                 call {call}, then get_browser_state again. With cua's Chrome extension \
                 connected in this Chrome that call changes no browser settings; without the \
                 extension it needs a runtime grant or an approval host and may turn on \
                 Chrome's remote debugging for this profile."
            )
        },
        detail: Some(detail),
        ..refusal
    }
}

fn endpoint_access_class(
    has_existing_profile_grant: bool,
    driver_owned: bool,
    process_role: BrowserProcessRole,
) -> Result<EndpointAccessClass, BrowserRefusal> {
    if has_existing_profile_grant {
        return Ok(EndpointAccessClass::ExistingProfileApproved);
    }
    if driver_owned {
        return Ok(EndpointAccessClass::DriverOwned);
    }
    match process_role {
        BrowserProcessRole::EmbeddedApplication => Ok(EndpointAccessClass::EmbeddedApplication),
        BrowserProcessRole::StandaloneConsumer => Err(refuse(
            BrowserRefusalCode::BrowserConsentRequired,
            "this standalone browser profile requires explicit existing-profile approval before Cua can inspect its DevTools endpoint",
        )
        .with_detail(json!({
            "reason": "consumer_profile_endpoint_requires_grant",
            "supported_strategies": ["existing_profile"],
            "next_action": "browser_prepare",
        }))),
        BrowserProcessRole::Helper => Err(refuse(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the requested pid is a browser renderer, GPU, or utility helper; select the exact top-level browser process instead",
        )),
        BrowserProcessRole::Unknown => Err(refuse(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "the browser process role is ambiguous, so endpoint access cannot be authorized",
        )),
    }
}

/// Everything revalidation proves before a mutation proceeds. The
/// `record`/`tab` evidence rides along for future callers even though
/// the v1 tools only need the attached connection.
#[allow(dead_code)]
pub(crate) struct ValidatedTab {
    pub conn: Arc<CdpConnection>,
    pub record: TargetRecord,
    pub tab: TabRecord,
    /// Live native metadata from this mutation's revalidation, rather than
    /// the bind-time geometry retained in `record`.
    pub native: NativeWindowInfo,
    /// The native window's title names this tab and no other tab of its
    /// window (the binding's own selected-tab proof, read again at this
    /// revalidation), or the endpoint has a single page.
    pub selected_by_title: bool,
    /// Flattened CDP session id attached to the tab's target.
    pub cdp_session: String,
    /// The target's committed URL as the browser process reports it.
    pub target_url: String,
}

fn viewport_point_to_screen(
    native: Rect,
    metrics: &Value,
    viewport_x: f64,
    viewport_y: f64,
) -> Option<(f64, f64)> {
    if !viewport_x.is_finite() || !viewport_y.is_finite() {
        return None;
    }
    let viewport = metrics
        .get("cssVisualViewport")
        .or_else(|| metrics.get("cssLayoutViewport"))?;
    let width = viewport.get("clientWidth")?.as_f64()?;
    let height = viewport.get("clientHeight")?.as_f64()?;
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || viewport_x < 0.0
        || viewport_y < 0.0
        || viewport_x > width
        || viewport_y > height
    {
        return None;
    }

    // CDP viewport coordinates and native/CDP window bounds are all DIPs.
    // Chromium centers the content viewport horizontally and places its
    // browser chrome above it. A negative inset means the geometry is not
    // trustworthy enough for visual feedback, so skip rather than mislead.
    let horizontal_inset = (native.width - width) / 2.0;
    let top_inset = native.height - height;
    if !horizontal_inset.is_finite()
        || !top_inset.is_finite()
        || horizontal_inset < -1.0
        || top_inset < -1.0
    {
        return None;
    }
    Some((
        native.x + horizontal_inset.max(0.0) + viewport_x,
        native.y + top_inset.max(0.0) + viewport_y,
    ))
}

/// Walks `levels` up from the control (a shadow root's next level is its
/// host and a frame document's its frame element, as the snapshot's DOM
/// index counts them) and names the row by the snapshot's rule
/// ([`super::semantic`]'s `name_controls_by_row`): its visible text in
/// order (children, then a shadow root, as the DOM index walks), open shadow
/// roots included, without text inside buttons,
/// links and fields, cleaned the same way, then compared with the row text the
/// ref keeps (the start of a longer row), ignoring case: a CSS text-transform
/// changes case in accessibility, not in the DOM. Accepted ceiling: a slot
/// reused for an item that differs only in case, or only after the kept 200
/// characters, still passes.
///
/// ponytail: visibility is the text's own element (visibility, opacity) plus
/// a display:none or content-visibility:hidden ancestor (which accessibility
/// drops), as the snapshot reads
/// it; text the snapshot drops for an
/// inline-styled hidden ancestor still counts here, and closed shadow roots
/// are not seen. Either reads as a changed row (a refusal, never a wrong
/// click). Move this check onto a fresh snapshot read if that bites.
const ROW_STILL_READS: &str = "function(levels, text, capped) { \
    let row = this; \
    for (let i = 0; i < levels && row; i++) { \
        row = row.parentNode || row.host || \
            (row.defaultView ? row.defaultView.frameElement : null) || null; \
    } \
    if (!row) return false; \
    const skip = new Set(['SCRIPT', 'STYLE', 'TEMPLATE', 'NOSCRIPT', 'BUTTON', 'INPUT', \
        'TEXTAREA', 'SELECT']); \
    const roles = /^(button|link|textbox|searchbox|combobox|menuitem|tab)$/; \
    const parts = [], stack = [row]; \
    while (stack.length) { \
        const node = stack.pop(); \
        if (node.nodeType === 3) { \
            const host = node.parentElement; \
            if (host) { \
                const style = getComputedStyle(host); \
                if (style.display === 'none' || style.visibility === 'hidden' || \
                    Number(style.opacity) <= 0) continue; \
            } \
            parts.push(node.nodeValue); \
            continue; \
        } \
        if (node.nodeType === 1 && node !== row && (skip.has(node.tagName) || \
            (node.tagName === 'A' && node.hasAttribute('href')) || \
            roles.test(node.getAttribute('role') || '') || \
            node.getAttribute('aria-hidden') === 'true' || node.hidden || \
            getComputedStyle(node).display === 'none' || \
            getComputedStyle(node).contentVisibility === 'hidden')) continue; \
        const kids = Array.from(node.childNodes || []); \
        if (node.shadowRoot) kids.push(node.shadowRoot); \
        for (let k = kids.length - 1; k >= 0; k--) stack.push(kids[k]); \
    } \
    const clean = (text) => text \
        .replace(/[\\uFEFF\\u200B\\u200C\\u200D\\u2060\\u00A0\\u2007\\u202F\\uE000-\\uF8FF]/g, ' ') \
        .split(/\\s+/).filter(Boolean).join(' ').toLowerCase(); \
    const now = clean(parts.join(' ')); \
    return capped ? now.startsWith(clean(text)) : now === clean(text); \
}";

/// Whether the row a control was named after (see
/// [`super::store::RefEntry::row`]) still reads as that name, by the rule in
/// [`ROW_STILL_READS`]. A control reused for another item is refused.
async fn row_still_reads(
    conn: &CdpConnection,
    cdp_session: &str,
    backend: i64,
    row: &super::store::RowName,
) -> bool {
    let Ok(resolved) = conn
        .call(
            Some(cdp_session),
            "DOM.resolveNode",
            json!({ "backendNodeId": backend }),
        )
        .await
    else {
        return false;
    };
    let Some(object_id) = resolved.pointer("/object/objectId").and_then(Value::as_str) else {
        return false;
    };
    conn.call(
        Some(cdp_session),
        "Runtime.callFunctionOn",
        json!({
            "objectId": object_id,
            "functionDeclaration": ROW_STILL_READS,
            "arguments": [
                { "value": row.levels },
                { "value": row.text },
                // The ref keeps this many characters of a longer row.
                { "value": row.text.chars().count() >= super::semantic::ROW_TEXT_CHARS },
            ],
            "returnByValue": true,
        }),
    )
    .await
    .ok()
    .and_then(|reply| reply.pointer("/result/value").and_then(Value::as_bool))
    .unwrap_or(false)
}

/// Whether a CDP error is Chromium's "method not implemented" shape.
/// Everything else stays a hard failure — a transient error must never
/// be misread as a capability gap.
fn is_method_unsupported(error: &anyhow::Error) -> bool {
    error.to_string().contains("(-32601)")
}

fn is_semantic_document_size_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    [
        "maximum depth",
        "object reference chain is too long",
        "message is too large",
        "serialization",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn is_semantic_document_fallback_error(error: &anyhow::Error) -> bool {
    if is_semantic_document_size_error(error) {
        return true;
    }
    is_semantic_document_timeout_error(error)
}

fn is_semantic_document_timeout_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("cdp dom.getdocument timed out after")
}

const SEMANTIC_DOM_FALLBACK_DEPTHS: &[i64] = &[256, 128, 64, 32, 16, 8, 4, 2, 1];
const SEMANTIC_DOM_TIMEOUT_FALLBACK_DEPTHS: &[i64] = &[8, 4, 2, 1];
const SEMANTIC_DOM_HYDRATION_DEPTH: i64 = 8;
const MAX_SEMANTIC_DOM_HYDRATION_CALLS: usize = 64;
const MAX_SEMANTIC_DOM_SCAN_NODES: usize = 50_000;

#[derive(Default)]
struct DomCoverageScan {
    truncated: Vec<i64>,
    visited_nodes: usize,
    budget_exhausted: bool,
}

fn scan_dom_coverage(value: &Value, scan: &mut DomCoverageScan) {
    if scan.budget_exhausted {
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                scan_dom_coverage(value, scan);
            }
        }
        Value::Object(object) => {
            if object.contains_key("nodeType") {
                scan.visited_nodes += 1;
                if scan.visited_nodes > MAX_SEMANTIC_DOM_SCAN_NODES {
                    scan.budget_exhausted = true;
                    return;
                }
                let expected = object
                    .get("childNodeCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let present = object
                    .get("children")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                if expected > present {
                    if let Some(backend_node_id) =
                        object.get("backendNodeId").and_then(Value::as_i64)
                    {
                        scan.truncated.push(backend_node_id);
                    }
                }
            }
            for value in object.values() {
                scan_dom_coverage(value, scan);
            }
        }
        _ => {}
    }
}

fn replace_dom_node(value: &mut Value, backend_node_id: i64, replacement: &Value) -> bool {
    match value {
        Value::Array(values) => values
            .iter_mut()
            .any(|value| replace_dom_node(value, backend_node_id, replacement)),
        Value::Object(object) => {
            if object.get("backendNodeId").and_then(Value::as_i64) == Some(backend_node_id) {
                *value = replacement.clone();
                return true;
            }
            object
                .values_mut()
                .any(|value| replace_dom_node(value, backend_node_id, replacement))
        }
        _ => false,
    }
}

/// Result of one tab snapshot: minted refs plus what was (and was not)
/// composable.
pub(crate) struct SnapshotOutcome {
    pub snapshot_id: u64,
    pub url: String,
    pub refs: Vec<(String, RefEntry)>,
    pub truncated: bool,
    pub oopif: OopifStatus,
}

pub(crate) struct SemanticSnapshotOutcome {
    pub snapshot_id: u64,
    pub url: String,
    pub title: String,
    /// One line per node, its ref and actions inline.
    pub outline: String,
    pub action_refs: usize,
    pub content_refs: usize,
    pub complete: bool,
    pub scope: &'static str,
    pub selected_nodes: usize,
    pub total_nodes: usize,
    pub omissions: OmissionCounts,
    pub continuation: Option<String>,
    pub oopif: OopifStatus,
    /// Characters the outline was allowed.
    pub outline_budget: usize,
    /// The same refs as a structured list, when it was asked for.
    pub listed: Option<Vec<Value>>,
    /// The session's baseline revision after this read.
    pub revision: Option<u64>,
    /// A diff when one was asked for and possible; otherwise the snapshot
    /// stands, with the reason a diff was not possible.
    pub told: Told,
}

/// Default size of a whole semantic snapshot result, in serialized characters.
pub(crate) const DEFAULT_SNAPSHOT_CHARS: usize = 6_000;
pub(crate) const MIN_SNAPSHOT_CHARS: usize = 1_500;
pub(crate) const MAX_SNAPSHOT_CHARS: usize = 60_000;
/// What a snapshot result spends outside the outline, the page URL and the
/// title: ids, snapshot metadata, the text summary and the JSON-RPC frame.
const SNAPSHOT_ENVELOPE_CHARS: usize = 900;
const MIN_OUTLINE_CHARS: usize = 600;

/// Characters the outline may take so the whole result stays in `max_chars`.
fn outline_budget(max_chars: usize, url: &str, title: &str) -> usize {
    let quoted =
        |text: &str| serde_json::to_string(text).map_or(text.len(), |json| json.chars().count());
    max_chars
        .saturating_sub(SNAPSHOT_ENVELOPE_CHARS + quoted(url) + quoted(title))
        .max(MIN_OUTLINE_CHARS)
}

fn outline_of(lines: &[ViewLine]) -> String {
    lines
        .iter()
        .map(|line| line.line.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The structured ref list for recorded lines: `facts` holds each line's
/// capability and value, in line order.
fn listed_refs(lines: &[ViewLine], facts: Vec<(RefEntry, Option<String>)>) -> Vec<Value> {
    lines
        .iter()
        .zip(facts)
        .map(|(line, (entry, value))| listed_ref(&line.key, &entry, value.as_deref()))
        .collect()
}

fn listed_facts(page: &super::semantic::SemanticPage) -> Vec<(RefEntry, Option<String>)> {
    page.view
        .iter()
        .map(|node| node.entry.clone())
        .zip(page.values.iter().cloned())
        .collect()
}

/// (action refs, content refs) among a view's lines.
fn ref_counts(view: &[ViewNode]) -> (usize, usize) {
    let actions = view
        .iter()
        .filter(|node| !node.entry.actions.is_empty())
        .count();
    (actions, view.len() - actions)
}

pub(crate) struct BrowserTabScreenshot {
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
    pub viewport_css_width: f64,
    pub viewport_css_height: f64,
    pub pixel_to_css_scale_x: f64,
    pub pixel_to_css_scale_y: f64,
}

#[derive(Debug, Clone, Copy)]
struct BrowserScreenshotViewport {
    page_x: f64,
    page_y: f64,
    width: f64,
    height: f64,
}

fn browser_screenshot_viewport(
    metrics: &Value,
) -> Result<BrowserScreenshotViewport, BrowserRefusal> {
    let viewport = metrics
        .get("cssVisualViewport")
        .or_else(|| metrics.get("visualViewport"))
        .ok_or_else(|| {
            route_err(
                "Page.getLayoutMetrics returned malformed data",
                "missing visual viewport metrics",
            )
        })?;
    let number = |field: &str| {
        viewport
            .get(field)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .ok_or_else(|| {
                route_err(
                    "Page.getLayoutMetrics returned malformed data",
                    format!("missing or non-finite visual viewport field {field}"),
                )
            })
    };
    let page_x = number("pageX")?;
    let page_y = number("pageY")?;
    let width = number("clientWidth")?;
    let height = number("clientHeight")?;
    if page_x < 0.0 || page_y < 0.0 {
        return Err(route_err(
            "Page.getLayoutMetrics returned malformed data",
            "visual viewport page offsets must be non-negative",
        ));
    }
    if width <= 0.0 || height <= 0.0 {
        return Err(route_err(
            "Page.getLayoutMetrics returned malformed data",
            "visual viewport dimensions must be positive",
        ));
    }
    Ok(BrowserScreenshotViewport {
        page_x,
        page_y,
        width,
        height,
    })
}

/// Whether OOPIF content could be composed into the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OopifStatus {
    /// Capability proven; `n` child frames were snapshotted beneath the
    /// tab's own session.
    Attached(usize),
    /// The capability (auto-attach and/or frame-tree identity) could
    /// not be proven — OOPIF content was omitted, not guessed.
    Unsupported,
}

impl OopifStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Attached(_) => "attached",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn frames(&self) -> usize {
        match self {
            Self::Attached(n) => *n,
            Self::Unsupported => 0,
        }
    }
}

/// One CDP session's local frame tree: frame id → loader id. OOPIF
/// children live in their own sessions and do NOT appear here — each
/// session proves exactly its own frames.
pub(crate) struct LocalFrameTree {
    main_frame_id: String,
    frames: HashMap<String, String>,
}

impl LocalFrameTree {
    fn main_identity(&self) -> FrameIdentity {
        FrameIdentity {
            frame_id: self.main_frame_id.clone(),
            loader_id: self.frames[&self.main_frame_id].clone(),
        }
    }

    fn identity_of(&self, frame_id: &str) -> Option<FrameIdentity> {
        self.frames.get(frame_id).map(|loader_id| FrameIdentity {
            frame_id: frame_id.to_owned(),
            loader_id: loader_id.clone(),
        })
    }

    /// Whether a snapshot-time identity still names a live document.
    fn proves(&self, identity: &FrameIdentity) -> bool {
        self.frames.get(&identity.frame_id) == Some(&identity.loader_id)
    }

    fn identities(&self) -> Vec<FrameIdentity> {
        let mut identities = self
            .frames
            .iter()
            .map(|(frame_id, loader_id)| FrameIdentity {
                frame_id: frame_id.clone(),
                loader_id: loader_id.clone(),
            })
            .collect::<Vec<_>>();
        identities.sort_by_key(|identity| identity.frame_id != self.main_frame_id);
        identities
    }
}

/// Parse a `Page.getFrameTree` result. Frames missing an id or loader
/// id are omitted (their refs will be omitted / refused, never
/// guessed); a malformed root fails the whole parse.
fn parse_frame_tree(v: &Value) -> Option<LocalFrameTree> {
    fn walk(node: &Value, frames: &mut HashMap<String, String>) -> Option<String> {
        let frame = node.get("frame")?;
        let id = frame.get("id").and_then(Value::as_str)?.to_owned();
        let loader = frame.get("loaderId").and_then(Value::as_str)?.to_owned();
        frames.insert(id.clone(), loader);
        if let Some(children) = node.get("childFrames").and_then(Value::as_array) {
            for child in children {
                let _ = walk(child, frames);
            }
        }
        Some(id)
    }
    let mut frames = HashMap::new();
    let main_frame_id = walk(v.get("frameTree")?, &mut frames)?;
    Some(LocalFrameTree {
        main_frame_id,
        frames,
    })
}

pub(crate) enum FrameTreeError {
    /// The endpoint does not implement `Page.getFrameTree` (embedded
    /// engines). Frame identity is unprovable; composition degrades to
    /// the v1 main-frame-only behavior.
    Unsupported,
    /// A real failure (transport, malformed reply).
    Failed(anyhow::Error),
}

/// One OOPIF child target attached (flattened) beneath a tab session.
pub(crate) struct AttachedChildFrame {
    pub session_id: String,
    pub target_id: String,
    #[allow(dead_code)]
    pub url: String,
}

pub(crate) enum AttachError {
    /// `Target.setAutoAttach` is not implemented on this session.
    Unsupported,
    Failed(anyhow::Error),
}

impl BrowserEngine {
    /// Create the engine and wire session-end cleanup for the
    /// capability store. Platform crates call this once and register
    /// the five tools via `register_browser_tools`.
    pub fn new(platform: Arc<dyn BrowserPlatform>) -> Arc<Self> {
        Self::new_with_runtime_services(
            platform,
            Arc::new(crate::consent::ApprovalBroker::unavailable()),
            Arc::new(crate::consent::ProtectedResourceOwnershipStore::default()),
        )
    }

    /// Create an engine with a provider installed by a trusted embedding host
    /// or platform adapter. Ordinary MCP/CLI callers cannot register or replace
    /// this provider after daemon startup.
    pub fn new_with_protected_consent_provider(
        platform: Arc<dyn BrowserPlatform>,
        provider: Option<Arc<dyn crate::consent::ProtectedConsentProvider>>,
    ) -> Arc<Self> {
        Self::new_with_runtime_services(
            platform,
            Arc::new(crate::consent::ApprovalBroker::new(provider)),
            Arc::new(crate::consent::ProtectedResourceOwnershipStore::default()),
        )
    }

    /// Create an engine using the runtime-owned broker shared by every
    /// protected resource adapter.
    pub fn new_with_approval_broker(
        platform: Arc<dyn BrowserPlatform>,
        approval_broker: Arc<crate::consent::ApprovalBroker>,
    ) -> Arc<Self> {
        Self::new_with_runtime_services(
            platform,
            approval_broker,
            Arc::new(crate::consent::ProtectedResourceOwnershipStore::default()),
        )
    }

    pub fn new_with_runtime_services(
        platform: Arc<dyn BrowserPlatform>,
        approval_broker: Arc<crate::consent::ApprovalBroker>,
        protected_resource_ownership: Arc<crate::consent::ProtectedResourceOwnershipStore>,
    ) -> Arc<Self> {
        let engine = Arc::new(Self {
            platform,
            store: BrowserStore::new(),
            pool: Arc::new(CdpPool::new()),
            managed_browsers: Default::default(),
            existing_profile_grants: ExistingProfileGrants::new(),
            approval_broker,
            protected_resource_ownership,
            mutation_gates: MutationGates::new(),
            observation_gates: super::keyed_gates::KeyedGates::new(),
            reconnect_gates: ReconnectGates::new(),
            pending_existing_profile_cleanups: Mutex::new(HashMap::new()),
            session_end_hook: Mutex::new(None),
        });
        let weak: Weak<Self> = Arc::downgrade(&engine);
        let registration =
            register_scoped_fallible_session_end_hook("browser_state", move |session_id| {
                let mut cleanup_errors = Vec::new();
                if let Some(engine) = weak.upgrade() {
                    engine.store.remove_session(session_id);
                    engine.cleanup_prepared_session(session_id);
                    let mut off_runtime = Vec::new();
                    let pending = {
                        let mut pending = engine.pending_existing_profile_cleanups.lock().unwrap();
                        let mut requests = pending.remove(session_id).unwrap_or_default();
                        for grant in engine.existing_profile_grants.remove_session(session_id) {
                            engine.pool.release_claim_marker(&grant.endpoint_ws_url);
                            if grant.cleanup_remote_debugging {
                                requests.push(ExistingProfileSetupRequest {
                                    pid: grant.pid,
                                    window_id: grant.window_id,
                                    browser: grant.browser_product,
                                });
                            }
                            if let Some(protected) = grant.protected_consent.as_ref() {
                                protected.revoke();
                            }
                            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                                let engine = engine.clone();
                                runtime.spawn(async move {
                                    engine.release_grant_socket(&grant).await;
                                    if let Some(protected) = grant.protected_consent.as_ref() {
                                        engine.approval_broker.revoke(protected).await;
                                    }
                                });
                            } else {
                                off_runtime.push(grant);
                            }
                        }
                        requests
                    };
                    // An SDK's idle sweeper ends sessions from a plain thread.
                    // The claim and the relay's release of the episode's tabs
                    // happen there without waiting, or at the pool's next
                    // operation.
                    for grant in off_runtime {
                        engine.pool.release_existing_now_or_later(
                            &grant.endpoint_ws_url,
                            grant.generation,
                            grant_relay_holder(&grant),
                        );
                    }

                    let mut failed = Vec::new();
                    for request in pending {
                        if let Err(error) = engine
                            .platform
                            .cleanup_existing_profile_setup(request.clone())
                        {
                            cleanup_errors.push(error.message);
                            failed.push(request);
                        }
                    }
                    let mut pending = engine.pending_existing_profile_cleanups.lock().unwrap();
                    if failed.is_empty() {
                        pending.remove(session_id);
                    } else {
                        pending.insert(session_id.to_owned(), failed);
                    }
                }
                if cleanup_errors.is_empty() {
                    Ok(())
                } else {
                    Err(cleanup_errors.join("; "))
                }
            });
        *engine.session_end_hook.lock().unwrap() = Some(registration);
        engine
    }

    // ── Endpoint / CDP plumbing ─────────────────────────────────────────

    pub(crate) async fn existing_profile_grant(
        &self,
        session: &str,
        transport_session: Option<&str>,
        pid: i64,
    ) -> Result<Option<ExistingProfileGrant>, BrowserRefusal> {
        match self
            .existing_profile_grants
            .lookup(session, transport_session, pid)
        {
            GrantLookup::Missing => Ok(None),
            GrantLookup::Live(grant) => Ok(Some(grant)),
            GrantLookup::Expired(grant) => {
                self.pool.release_claim_marker(&grant.endpoint_ws_url);
                if grant.cleanup_remote_debugging {
                    let request = ExistingProfileSetupRequest {
                        pid: grant.pid,
                        window_id: grant.window_id,
                        browser: grant.browser_product,
                    };
                    if self
                        .platform
                        .cleanup_existing_profile_setup(request.clone())
                        .is_err()
                    {
                        self.pending_existing_profile_cleanups
                            .lock()
                            .unwrap()
                            .entry(session.to_owned())
                            .or_default()
                            .push(request);
                    }
                }
                self.release_grant_socket(&grant).await;
                if let Some(protected) = grant.protected_consent.as_ref() {
                    self.approval_broker.revoke(protected).await;
                }
                Err(refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "the existing-profile grant expired; approve this attachment again",
                ))
            }
        }
    }

    pub(crate) async fn revoke_existing_profile_grant(
        &self,
        session: &str,
        transport_session: Option<&str>,
        pid: i64,
    ) {
        if let Some(grant) = self
            .existing_profile_grants
            .revoke(session, transport_session, pid)
        {
            self.pool.release_claim_marker(&grant.endpoint_ws_url);
            if grant.cleanup_remote_debugging {
                let request = ExistingProfileSetupRequest {
                    pid: grant.pid,
                    window_id: grant.window_id,
                    browser: grant.browser_product,
                };
                if self
                    .platform
                    .cleanup_existing_profile_setup(request.clone())
                    .is_err()
                {
                    self.pending_existing_profile_cleanups
                        .lock()
                        .unwrap()
                        .entry(session.to_owned())
                        .or_default()
                        .push(request);
                }
            }
            self.release_grant_socket(&grant).await;
            if let Some(protected) = grant.protected_consent.as_ref() {
                self.approval_broker.revoke(protected).await;
            }
        }
    }

    /// Release a grant that has already left the registry (see
    /// [`release_grant_claim`]). The release runs as its own task, awaited
    /// here, so a caller cancelled while the pool lock is busy cannot strand
    /// a claim that no grant will release.
    pub(crate) async fn release_grant_socket(&self, grant: &ExistingProfileGrant) {
        let pool = self.pool.clone();
        let grant = grant.clone();
        let _ = tokio::spawn(async move { release_grant_claim(&pool, &grant).await }).await;
    }

    pub(crate) async fn connect(&self, ws_url: &str) -> Result<Arc<CdpConnection>, BrowserRefusal> {
        match self.pool.get(ws_url).await {
            Ok(conn) => Ok(conn),
            Err(first_err) => {
                // One redial after eviction covers a browser restart on
                // the same port; a second failure is a real refusal.
                self.pool.evict(ws_url).await;
                self.pool
                    .get(ws_url)
                    .await
                    .map_err(|_| route_err("cannot connect to owned DevTools endpoint", first_err))
            }
        }
    }

    async fn connect_existing_profile(
        &self,
        session: &str,
        transport_session: Option<&str>,
        pid: i64,
    ) -> Result<(Arc<CdpConnection>, ExistingProfileGrant), BrowserRefusal> {
        let grant = self
            .existing_profile_grant(session, transport_session, pid)
            .await?
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "no live existing-profile grant remains for this browser session",
                )
            })?;
        if let Ok(conn) = self
            .pool
            .get_existing(&grant.endpoint_ws_url, grant.generation)
            .await
        {
            return Ok((conn, grant));
        }

        // One leader owns endpoint reproof and bounded redial. Followers
        // re-check the generation after acquiring this gate and reuse its
        // socket rather than opening another browser-level connection.
        let _leader = self
            .reconnect_gates
            .lock(ReconnectKey::new(&grant.fingerprint))
            .await;
        let mut grant = self
            .existing_profile_grant(session, transport_session, pid)
            .await?
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "the existing-profile grant ended while reconnecting",
                )
            })?;
        if let Ok(conn) = self
            .pool
            .get_existing(&grant.endpoint_ws_url, grant.generation)
            .await
        {
            return Ok((conn, grant));
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(32);
        let mut last_error = None;
        while tokio::time::Instant::now() < deadline && grant.reconnect_attempts_remaining > 0 {
            if grant.pid != pid || grant.browser != "chromium" {
                self.revoke_existing_profile_grant(session, transport_session, pid)
                    .await;
                return Err(refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "the reconnect request no longer matches the approved browser identity",
                ));
            }
            let classification = self.platform.classify_browser(pid).await?;
            if !classification.supports_cdp
                || classification.engine != super::types::BrowserEngineFamily::Chromium
            {
                self.revoke_existing_profile_grant(session, transport_session, pid)
                    .await;
                return Err(refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "the approved process is no longer a supported Chromium browser",
                ));
            }
            let fingerprint = self.platform.process_fingerprint(pid).await?;
            if !grant.fingerprint.matches(&fingerprint) {
                self.revoke_existing_profile_grant(session, transport_session, pid)
                    .await;
                return Err(refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "the browser process changed; existing-profile attachment needs fresh approval",
                ));
            }
            let endpoint = self
                .platform
                .reprove_existing_profile_endpoint(pid, &grant.endpoint_ws_url)
                .await?
                .ok_or_else(|| {
                    refuse(
                        BrowserRefusalCode::BrowserRequiresSetup,
                        "the approved browser endpoint disappeared during reconnect",
                    )
                })?;
            if endpoint.ownership.owner_pid != pid {
                return Err(refuse(
                    BrowserRefusalCode::BrowserEndpointOwnerMismatch,
                    "the reconnect endpoint is not owned by the approved browser process",
                ));
            }
            if endpoint.ws_url != grant.endpoint_ws_url {
                self.revoke_existing_profile_grant(session, transport_session, pid)
                    .await;
                return Err(refuse(
                    BrowserRefusalCode::BrowserEndpointOwnerMismatch,
                    "the browser DevTools endpoint changed during reconnect",
                ));
            }

            let old_generation = grant.generation;
            let attempt = super::grant::MAX_RECONNECT_ATTEMPTS
                .saturating_sub(grant.reconnect_attempts_remaining)
                .saturating_add(1);
            // The grant advances inside the pool's claim transfer, so no
            // cancellation or concurrent release can separate the two.
            let mut reconnect = Box::pin(self.pool.reconnect_existing(
                &endpoint.ws_url,
                old_generation,
                || {
                    let new_generation = self.existing_profile_grants.bump_generation(
                        session,
                        transport_session,
                        pid,
                        old_generation,
                    )?;
                    self.store
                        .invalidate_endpoint_generation(pid, old_generation);
                    Ok(new_generation)
                },
                |generation| {
                    self.existing_profile_grants.is_current(
                        session,
                        transport_session,
                        pid,
                        generation,
                    )
                },
            ));
            let reconnected = tokio::select! {
                result = &mut reconnect => result?,
                // As in prepare: no Chrome prompt exists on the extension route.
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)),
                    if endpoint.transport != super::types::EndpointTransport::ExtensionRelay => {
                    match self.platform.handle_existing_profile_consent(BrowserConsentRequest {
                        pid,
                        window_id: grant.window_id,
                        attempt,
                    }).await {
                        Ok(BrowserConsentOutcome::Accepted | BrowserConsentOutcome::NotPresent) => {
                            reconnect.await?
                        }
                        Err(error) => {
                            // The reconnect future may be waiting on browser
                            // consent. Cancel it before grant revocation so no
                            // socket-pool resource can outlive this refusal.
                            drop(reconnect);
                            if error.code == BrowserRefusalCode::BrowserConsentRevoked {
                                self.revoke_existing_profile_grant(session, transport_session, pid)
                                    .await;
                            }
                            return Err(error);
                        }
                    }
                }
            };
            match reconnected {
                Ok(conn) => {
                    grant = self
                        .existing_profile_grant(session, transport_session, pid)
                        .await?
                        .expect("grant exists after successful generation bump");
                    return Ok((conn, grant));
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    grant = self
                        .existing_profile_grant(session, transport_session, pid)
                        .await?
                        .expect("grant exists while reconnect budget remains");
                }
            }
        }
        self.revoke_existing_profile_grant(session, transport_session, pid)
            .await;
        Err(refuse(
            BrowserRefusalCode::BrowserReconnectExhausted,
            "the bounded existing-profile reconnect attempts did not establish a proven browser socket",
        )
        .with_detail(json!({
            "attempt_limit": super::grant::MAX_RECONNECT_ATTEMPTS,
            "last_error": last_error.map(|_| "connection_failed"),
            "retryable": false,
        })))
    }

    async fn connection_for_record(
        &self,
        session: &str,
        record: &TargetRecord,
    ) -> Result<Arc<CdpConnection>, BrowserRefusal> {
        if record.generation == 0 {
            return self.connect(&record.ws_url).await;
        }
        let (conn, grant) = self
            .connect_existing_profile(session, record.transport_session.as_deref(), record.pid)
            .await?;
        if grant.generation != record.generation {
            return Err(refuse(
                BrowserRefusalCode::BrowserBindingStale,
                "the browser reconnected and invalidated this target; re-run get_browser_state",
            ));
        }
        Ok(conn)
    }

    /// Discover + ownership-check the endpoint for `pid`.
    pub(crate) async fn owned_endpoint(&self, pid: i64) -> Result<OwnedEndpoint, BrowserRefusal> {
        let endpoint = self
            .platform
            .discover_owned_endpoint(pid)
            .await?
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserRequiresSetup,
                    format!(
                        "no owned DevTools endpoint for pid {pid} — run browser_prepare \
                     explicitly to set one up"
                    ),
                )
            })?;
        if endpoint.ownership.owner_pid != pid {
            return Err(refuse(
                BrowserRefusalCode::BrowserEndpointOwnerMismatch,
                format!(
                    "endpoint ownership proof attributes the endpoint to pid {} but the \
                     target is pid {pid}",
                    endpoint.ownership.owner_pid
                ),
            ));
        }
        Ok(endpoint)
    }

    /// Re-prove the endpoint exposed by an explicitly approved existing
    /// profile. This route is intentionally separate from driver-managed
    /// endpoint discovery: Chrome's per-instance remote-debugging toggle can
    /// expose a PID-owned listener whose exact WebSocket path is available
    /// only through the browser's default-profile DevToolsActivePort file.
    async fn existing_profile_endpoint(
        &self,
        pid: i64,
        expected_ws_url: &str,
    ) -> Result<OwnedEndpoint, BrowserRefusal> {
        let endpoint = self
            .platform
            .reprove_existing_profile_endpoint(pid, expected_ws_url)
            .await?
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserRequiresSetup,
                    "the approved existing-profile DevTools endpoint is no longer available",
                )
            })?;
        if endpoint.ownership.owner_pid != pid {
            return Err(refuse(
                BrowserRefusalCode::BrowserEndpointOwnerMismatch,
                "the existing-profile endpoint is not owned by the approved browser process",
            ));
        }
        Ok(endpoint)
    }

    /// List page-type CDP targets with their window geometry.
    async fn window_candidates(
        &self,
        conn: &CdpConnection,
    ) -> Result<Vec<CdpWindowCandidate>, BrowserRefusal> {
        let targets = conn
            .call(None, "Target.getTargets", json!({}))
            .await
            .map_err(|e| route_err("Target.getTargets failed", e))?;
        let infos = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    "Target.getTargets returned no targetInfos array",
                )
            })?;

        let mut out = Vec::new();
        for info in infos {
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let url = info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if url.starts_with("devtools://") {
                continue;
            }
            let target_id = info
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    refuse(
                        BrowserRefusalCode::BrowserRouteUnavailable,
                        "Target.getTargets returned a page without targetId",
                    )
                })?;
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();

            let window_geometry = match conn
                .call(
                    None,
                    "Browser.getWindowForTarget",
                    json!({ "targetId": target_id }),
                )
                .await
            {
                Ok(win) => {
                    let window_id =
                        win.get("windowId").and_then(Value::as_i64).ok_or_else(|| {
                            refuse(
                                BrowserRefusalCode::BrowserRouteUnavailable,
                                "Browser.getWindowForTarget returned no windowId",
                            )
                        })?;
                    let bounds_v = conn
                        .call(
                            None,
                            "Browser.getWindowBounds",
                            json!({ "windowId": window_id }),
                        )
                        .await
                        .map_err(|error| {
                            route_err(
                                "Browser.getWindowBounds failed while proving the native window",
                                error,
                            )
                        })?;
                    let b = bounds_v.get("bounds").ok_or_else(|| {
                        refuse(
                            BrowserRefusalCode::BrowserRouteUnavailable,
                            "Browser.getWindowBounds returned no bounds object",
                        )
                    })?;
                    let number = |field: &str| {
                        b.get(field).and_then(Value::as_f64).ok_or_else(|| {
                            refuse(
                                BrowserRefusalCode::BrowserRouteUnavailable,
                                format!("Browser.getWindowBounds returned no numeric {field}"),
                            )
                        })
                    };
                    Some((
                        window_id,
                        Rect::new(
                            number("left")?,
                            number("top")?,
                            number("width")?,
                            number("height")?,
                        ),
                    ))
                }
                // Electron's browser endpoint can omit this Browser-domain
                // method entirely. Retain only that explicit unsupported
                // shape; every transient/vanished-target error fails the
                // whole proof rather than shrinking it to a false unique set.
                Err(error) if is_method_unsupported(&error) => None,
                Err(error) => {
                    return Err(route_err(
                        "Browser.getWindowForTarget failed while proving the native window",
                        error,
                    ));
                }
            };
            out.push(CdpWindowCandidate {
                cdp_target_id: target_id,
                cdp_window_id: window_geometry.map(|(window_id, _)| window_id),
                title,
                url,
                bounds: window_geometry.map(|(_, bounds)| bounds),
            });
        }
        Ok(out)
    }

    /// Attach (flattened) to a tab's target and return the CDP session id.
    async fn attach(
        &self,
        conn: &CdpConnection,
        cdp_target_id: &str,
        session: &str,
        generation: u64,
        transport: super::types::EndpointTransport,
    ) -> Result<String, BrowserRefusal> {
        let mut params = json!({ "targetId": cdp_target_id, "flatten": true });
        // The Chrome extension draws its tab indicator in the session's cursor
        // color. Only the relay reads this field; Chrome itself never sees it.
        if transport == super::types::EndpointTransport::ExtensionRelay {
            params["cuaSessionColor"] =
                json!(cua_driver_contract::cursor::session_fill_hex(session));
            // The relay counts tab holders per Cua session episode (see
            // extension_relay::attach_gates and relay_holder).
            params["cuaSession"] = json!(relay_holder(session, generation));
        }
        let attached = conn
            .call(None, "Target.attachToTarget", params)
            .await
            .map_err(|e| {
                refuse(
                    BrowserRefusalCode::BrowserTabNotFound,
                    format!("cannot attach to tab target {cdp_target_id}: {e}"),
                )
            })?;
        attached
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserTabNotFound,
                    format!("attach to {cdp_target_id} returned no sessionId"),
                )
            })
    }

    // ── Binding (get_browser_state, pid + window_id mode) ──────────────

    /// Classify, inspect, discover, correlate — and mint a target
    /// capability on success. `session` must already be explicit.
    pub(crate) async fn bind_native(
        &self,
        session: &str,
        transport_session: Option<&str>,
        pid: i64,
        window_id: u64,
    ) -> Result<(String, TargetRecord), BrowserRefusal> {
        let class = self.platform.classify_browser(pid).await?;
        if !class.is_browser {
            return Err(refuse(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("pid {pid} is not a recognized browser process"),
            ));
        }
        if !class.supports_cdp {
            return Err(unsupported_engine_refusal(&class, "bind_native_window"));
        }

        let mut grant = self
            .existing_profile_grant(session, transport_session, pid)
            .await?;
        let driver_owned = self.is_driver_owned_pid_for_session(session, pid)
            || transport_session
                .is_some_and(|owner| self.is_driver_owned_pid_for_session(owner, pid));
        let mut extension_connected = false;
        if grant.is_none()
            && !driver_owned
            && class.process_role == BrowserProcessRole::StandaloneConsumer
        {
            extension_connected = self.platform.extension_link_connected(pid).await;
            if extension_connected {
                grant = self
                    .grant_through_extension(session, transport_session, pid, window_id)
                    .await?;
            }
        }
        let access_class = endpoint_access_class(grant.is_some(), driver_owned, class.process_role)
            .map_err(|refusal| {
                existing_profile_next_call(refusal, pid, window_id, extension_connected)
            })?;

        let native = self.native_window_checked(pid, window_id).await?;
        let fingerprint = self.platform.process_fingerprint(pid).await?;
        let endpoint = if let Some(live_grant) = &grant {
            self.existing_profile_endpoint(pid, &live_grant.endpoint_ws_url)
                .await?
        } else {
            self.owned_endpoint(pid).await?
        };
        // A grant names the window it was approved for. One made through the
        // extension covers every window of that Chrome whose consent is the
        // extension's too (installing it is the consent, and binding such a
        // window without a grant would mint one without asking), so a second
        // window of the same browser binds under it. Explicitly approved
        // grants stay tied to their window.
        let other_window_covered = match &grant {
            Some(grant) if grant.window_id != window_id => {
                grant.endpoint_transport == super::types::EndpointTransport::ExtensionRelay
                    && self.extension_consent_covers(pid, window_id).await
            }
            _ => false,
        };
        if let Some(grant) = &grant {
            if !grant.fingerprint.matches(&fingerprint)
                || grant.endpoint_ws_url != endpoint.ws_url
                || (grant.window_id != window_id && !other_window_covered)
            {
                return Err(refuse(
                    BrowserRefusalCode::BrowserBindingStale,
                    "the approved browser process, endpoint, or native window changed; approve the existing profile again",
                ));
            }
        }
        let conn = if grant.is_some() {
            let (conn, live_grant) = self
                .connect_existing_profile(session, transport_session, pid)
                .await?;
            grant = Some(live_grant);
            conn
        } else {
            self.connect(&endpoint.ws_url).await?
        };
        let candidates = self.window_candidates(&conn).await?;

        let correlation = correlate(&native, &candidates, BOUNDS_TOLERANCE_PX);
        let (candidate, quality) = match correlation {
            BindingOutcome::Bound {
                candidate,
                quality: BindingQuality::Exact,
            } => (candidate, BindingQuality::Exact),
            BindingOutcome::Bound {
                candidate,
                quality: BindingQuality::Heuristic,
            } => {
                let only_native_window = self
                    .platform
                    .is_only_exact_native_window(pid, window_id)
                    .await?;
                match cardinality_exact_candidate(&native.title, &candidates, only_native_window) {
                    Some(exact) => (exact, BindingQuality::Exact),
                    None => (candidate, BindingQuality::Heuristic),
                }
            }
            BindingOutcome::Ambiguous(candidate_count) => {
                return Err(refuse(
                    BrowserRefusalCode::BrowserBindingAmbiguous,
                    "multiple CDP targets match the native window and the title \
                     tie-break cannot pick a unique one",
                )
                .with_detail(json!({ "candidate_count": candidate_count })));
            }
            BindingOutcome::None => {
                let only_native_window = self
                    .platform
                    .is_only_exact_native_window(pid, window_id)
                    .await?;
                match cardinality_exact_candidate(&native.title, &candidates, only_native_window) {
                    Some(exact) => (exact, BindingQuality::Exact),
                    None => {
                        return Err(refuse(
                            BrowserRefusalCode::BrowserWrongTargetRefused,
                            format!(
                                "no CDP target correlates with native window {window_id} of \
                                 pid {pid} — refusing rather than guessing"
                            ),
                        ));
                    }
                }
            }
        };

        // Tabs = page targets living in the bound CDP window. Selection is a
        // separate proof from native-window correlation: a representative CDP
        // target is only a window handle and must never be reported as active.
        let selected_cdp_target_id =
            selected_tab_target_id(&native.title, &candidates, candidate.cdp_window_id);
        let mut tabs = HashMap::new();
        for c in candidates.iter().filter(|c| match candidate.cdp_window_id {
            Some(window_id) => c.cdp_window_id == Some(window_id),
            None => c.cdp_target_id == candidate.cdp_target_id,
        }) {
            let tab_id = self.store.mint_tab_id();
            tabs.insert(
                tab_id.clone(),
                TabRecord::new(
                    tab_id,
                    c.cdp_target_id.clone(),
                    c.title.clone(),
                    c.url.clone(),
                    selected_cdp_target_id.map(|selected| selected == c.cdp_target_id),
                    grant.as_ref().map_or(0, |grant| grant.generation),
                ),
            );
        }

        let record = TargetRecord {
            target_id: String::new(),
            pid,
            window_id,
            ws_url: endpoint.ws_url.clone(),
            endpoint_owner_pid: endpoint.ownership.owner_pid,
            endpoint_transport: endpoint.transport,
            endpoint_access_class: access_class,
            generation: grant.as_ref().map_or(0, |grant| grant.generation),
            transport_session: grant
                .as_ref()
                .map(|grant| grant.transport_session.clone())
                .or_else(|| transport_session.map(str::to_owned)),
            fingerprint,
            native_title: native.title.clone(),
            native_bounds: native.bounds,
            cdp_target_id: candidate.cdp_target_id.clone(),
            cdp_window_id: candidate.cdp_window_id,
            quality,
            tabs,
        };
        let target_id = self.store.mint_target(session, record.clone());
        let record = self.store.get_target(session, &target_id)?;
        Ok((target_id, record))
    }

    /// The bind's own existing-profile grant. With cua's Chrome extension
    /// connected in this browser, attaching through it is the grant
    /// `browser_prepare` would make without asking (installing the extension
    /// is the consent) and it changes no browser setting, so the agent is not
    /// sent through a separate prepare step. Returns `None` when the consent
    /// in force needs an explicit `browser_prepare` (an approval host or a
    /// bounded manifest).
    /// Whether the consent in force for this window lets the extension route
    /// attach without an explicit `browser_prepare` approval.
    async fn extension_consent_covers(&self, pid: i64, window_id: u64) -> bool {
        use super::prepare::ExistingProfileConsent as Consent;
        matches!(
            self.existing_profile_consent(pid, window_id).await,
            Ok((
                _,
                Consent::ExtensionInstalled | Consent::Unrestricted | Consent::LaunchGrant
            ))
        )
    }

    async fn grant_through_extension(
        &self,
        session: &str,
        transport_session: Option<&str>,
        pid: i64,
        window_id: u64,
    ) -> Result<Option<ExistingProfileGrant>, BrowserRefusal> {
        if !self.extension_consent_covers(pid, window_id).await {
            return Ok(None);
        }
        self.attach_existing_profile_via(
            super::platform::PrepareRequest {
                pid: Some(pid),
                window_id: Some(window_id),
                session: session.to_owned(),
                transport_session: transport_session.map(str::to_owned),
                strategy: Some(super::platform::PrepareStrategy::ExistingProfile),
                profile: None,
                allow_launch: false,
            },
            true,
        )
        .await?;
        self.existing_profile_grant(session, transport_session, pid)
            .await
    }

    pub(crate) async fn native_window_checked(
        &self,
        pid: i64,
        window_id: u64,
    ) -> Result<NativeWindowInfo, BrowserRefusal> {
        let native = self.platform.native_window(pid, window_id).await?;
        if native.ownership.owner_pid != pid {
            return Err(refuse(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!(
                    "native ownership proof attributes window {window_id} to pid {} — \
                     not the requested pid {pid}",
                    native.ownership.owner_pid
                ),
            ));
        }
        Ok(native)
    }

    // ── Revalidation (before every mutation) ────────────────────────────

    /// Re-prove the entire binding chain for a mutation on one tab.
    /// Exact-or-refused: heuristic bindings never reach the mutation
    /// path.
    pub(crate) async fn revalidate_for_mutation(
        &self,
        session: &str,
        target_id: &str,
        tab_id: Option<&str>,
    ) -> Result<ValidatedTab, BrowserRefusal> {
        let record = self.store.get_target(session, target_id)?;

        if record.quality != BindingQuality::Exact {
            return Err(refuse(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "this binding is heuristic (title-only) — mutations require an exact \
                 bounds- or cardinality-correlated binding",
            ));
        }

        let tab_id = tab_id.ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserTabRequired,
                "this operation requires an explicit tab_id from get_browser_state",
            )
        })?;
        let tab = record.tabs.get(tab_id).cloned().ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserTabNotFound,
                format!("tab {tab_id} is not known for target {target_id}"),
            )
        })?;

        if record.generation != tab.generation {
            return Err(refuse(
                BrowserRefusalCode::BrowserBindingStale,
                "the tab capability belongs to an older browser connection generation",
            ));
        }
        if record.generation > 0 {
            let grant = self
                .existing_profile_grant(session, record.transport_session.as_deref(), record.pid)
                .await?
                .ok_or_else(|| {
                    refuse(
                        BrowserRefusalCode::BrowserConsentRequired,
                        "the existing-profile grant ended; approve and bind the browser again",
                    )
                })?;
            if grant.generation != record.generation {
                return Err(refuse(
                    BrowserRefusalCode::BrowserBindingStale,
                    "the browser connection generation changed; re-run get_browser_state",
                ));
            }
        }

        match record.endpoint_access_class {
            EndpointAccessClass::DriverOwned => {
                let lifecycle_is_live = self.is_driver_owned_pid_for_session(session, record.pid)
                    || record.transport_session.as_deref().is_some_and(|owner| {
                        self.is_driver_owned_pid_for_session(owner, record.pid)
                    });
                if !lifecycle_is_live {
                    return Err(refuse(
                        BrowserRefusalCode::BrowserConsentRequired,
                        "the driver-owned browser lifecycle ended; prepare and bind it again",
                    ));
                }
            }
            EndpointAccessClass::ExistingProfileApproved => {
                if record.generation == 0 {
                    return Err(refuse(
                        BrowserRefusalCode::BrowserBindingStale,
                        "the approved existing-profile binding has no live connection generation",
                    ));
                }
            }
            EndpointAccessClass::EmbeddedApplication => {
                let classification = self.platform.classify_browser(record.pid).await?;
                if classification.process_role != BrowserProcessRole::EmbeddedApplication {
                    return Err(refuse(
                        BrowserRefusalCode::BrowserBindingStale,
                        "the process is no longer proven to be the approved embedded browser host",
                    ));
                }
            }
            EndpointAccessClass::ExternalConsumerBrowser => {
                return Err(refuse(
                    BrowserRefusalCode::BrowserConsentRequired,
                    "a standalone consumer browser cannot use the generation-zero endpoint route",
                ));
            }
        }

        // 1. Process fingerprint — pid reuse / restart detection.
        let fp_now = self.platform.process_fingerprint(record.pid).await?;
        if !record.fingerprint.matches(&fp_now) {
            return Err(refuse(
                BrowserRefusalCode::BrowserBindingStale,
                format!(
                    "process fingerprint for pid {} changed since binding — the browser \
                     restarted or the pid was reused",
                    record.pid
                ),
            ));
        }

        // 2. Native window still exists and is still owned by the pid.
        let native = self
            .native_window_checked(record.pid, record.window_id)
            .await?;

        // 3. Endpoint still owned and unchanged.
        let endpoint = if record.generation > 0 {
            self.existing_profile_endpoint(record.pid, &record.ws_url)
                .await?
        } else {
            self.owned_endpoint(record.pid).await?
        };
        if endpoint.ws_url != record.ws_url {
            return Err(refuse(
                BrowserRefusalCode::BrowserBindingStale,
                "the owned DevTools endpoint changed since binding — re-run \
                 get_browser_state",
            ));
        }
        if endpoint.transport != record.endpoint_transport {
            return Err(refuse(
                BrowserRefusalCode::BrowserBindingStale,
                "the DevTools endpoint transport changed since binding; prepare and bind again",
            ));
        }

        // 4. CDP target still a page in the bound CDP window, with either
        //    matching geometry or the same singleton cardinality proof.
        let conn = self.connection_for_record(session, &record).await?;
        let candidates = self.window_candidates(&conn).await?;
        let live = candidates
            .iter()
            .find(|c| c.cdp_target_id == tab.cdp_target_id)
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserTabNotFound,
                    format!("tab {tab_id} no longer has a live CDP page target"),
                )
            })?;
        if let Some(bound_window_id) = record.cdp_window_id {
            if live.cdp_window_id != Some(bound_window_id) {
                return Err(refuse(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    "the tab moved to a different browser window since binding",
                ));
            }
            let geometry_matches = live
                .bounds
                .is_some_and(|bounds| bounds.approx_eq(&native.bounds, BOUNDS_TOLERANCE_PX));
            let correlation_still_exact = if geometry_matches {
                true
            } else {
                let only_native_window = self
                    .platform
                    .is_only_exact_native_window(record.pid, record.window_id)
                    .await?;
                cardinality_exact_candidate(&native.title, &candidates, only_native_window)
                    .is_some_and(|candidate| candidate.cdp_window_id == Some(bound_window_id))
            };
            if !correlation_still_exact {
                return Err(refuse(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    "CDP window no longer has an exact geometry or singleton-cardinality \
                     correlation with the native window — refusing to mutate it",
                ));
            }
        } else if candidates.len() != 1
            || live.cdp_window_id.is_some()
            || self
                .platform
                .is_only_exact_native_window(record.pid, record.window_id)
                .await?
                != Some(true)
        {
            return Err(refuse(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the embedded browser is no longer provably single-page and single-window",
            ));
        }

        let target_url = live.url.clone();
        let selected_by_title =
            selected_tab_target_id(&native.title, &candidates, record.cdp_window_id)
                == Some(tab.cdp_target_id.as_str());
        let cdp_session = self
            .attach(
                &conn,
                &tab.cdp_target_id,
                session,
                record.generation,
                record.endpoint_transport,
            )
            .await?;
        let dispatch_context = crate::tool::current_dispatch_authorization_context();
        if dispatch_context
            .as_deref()
            .is_some_and(|context| context.capability_manifest().is_some())
        {
            let live_url = self
                .live_top_level_url(&conn, &cdp_session, &tab.cdp_target_id, &target_url)
                .await?;
            // A browser mutation admitted for a delegated session must use
            // that exact session's capability manifest. Falling back to the
            // process compatibility manifest would let a missing task-local
            // context borrow unrelated authority.
            let manifest = dispatch_context
                .as_deref()
                .ok_or_else(|| {
                    refuse(
                        BrowserRefusalCode::BrowserOriginOutsideScope,
                        "the browser authorization context is unavailable",
                    )
                })?
                .capability_manifest();
            authorize_live_browser_origin(manifest, &live_url)?;
        }
        Ok(ValidatedTab {
            conn,
            record,
            tab,
            native,
            selected_by_title,
            cdp_session,
            target_url,
        })
    }

    /// The live top-level document's URL, from the page's own frame tree.
    ///
    /// While a JavaScript dialog is open the page answers nothing, the
    /// frame tree included, and nothing can navigate it either. The browser
    /// process still reports the target's committed URL, which is what the
    /// frame tree would say; without it the dialog could never be inspected
    /// or resolved, since both are admitted against the live origin.
    async fn live_top_level_url(
        &self,
        conn: &CdpConnection,
        cdp_session: &str,
        cdp_target_id: &str,
        target_url: &str,
    ) -> Result<String, BrowserRefusal> {
        if conn.dialog_state(cdp_target_id).is_some() && !target_url.is_empty() {
            return Ok(target_url.to_owned());
        }
        let frame_tree = conn
            .call(Some(cdp_session), "Page.getFrameTree", json!({}))
            .await
            .map_err(|error| {
                route_err("could not prove the live top-level browser document", error)
            })?;
        frame_tree
            .pointer("/frameTree/frame/url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserOriginOutsideScope,
                    "the live top-level browser origin could not be proven",
                )
            })
    }

    pub(crate) async fn attest_protected_tab(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
    ) -> Result<(ValidatedTab, String), BrowserRefusal> {
        let validated = self
            .revalidate_for_mutation(session, target_id, Some(tab_id))
            .await?;
        let live_url = self
            .live_top_level_url(
                &validated.conn,
                &validated.cdp_session,
                &validated.tab.cdp_target_id,
                &validated.target_url,
            )
            .await?;
        let live_origin = protected_live_origin_scope(&live_url)?;
        Ok((validated, live_origin))
    }

    /// The macOS window a bound-tab action shows in, for the PiP: the
    /// binding's pid and window, and only while the tab is the selected tab
    /// of that window (see `pip_hook::bound_tab_window`).
    ///
    /// Selected is about the tab strip, not about what the user can see: it
    /// holds for a window that is covered, minimized or on another Space
    /// (the page's own `visibilityState` does not, and asking the page
    /// would put a renderer round trip in front of the action). The answer
    /// is what the daemon already holds: the Chrome extension's report of
    /// each window's selected tab where the platform follows them, else
    /// the binding's own proof, read again at this revalidation
    /// (`selected_by_title`). Unknown counts as not selected.
    ///
    /// Not async on purpose: it asks nobody, so it can never delay the
    /// action it is decided before or hold its mutation lock any longer.
    pub(crate) fn pip_window(&self, validated: &ValidatedTab) -> Option<(i32, u32)> {
        let selected = self
            .platform
            .selected_tab(&validated.tab.cdp_target_id)
            .unwrap_or(validated.selected_by_title);
        crate::pip_hook::bound_tab_window(
            validated.record.pid,
            validated.record.window_id,
            selected,
        )
    }

    /// Tell the PiP which macOS window this bound-tab action shows in
    /// (`pip_window`). Every tool that changes a bound tab calls this once
    /// it holds the validated tab. No-op unless a PiP is on and a dispatch
    /// is waiting for the answer.
    pub(crate) fn note_pip_window(&self, validated: &ValidatedTab) {
        if !crate::pip_hook::wants_bound_window() {
            return;
        }
        let window = self.pip_window(validated);
        if window.is_none() {
            tracing::info!(target: "pip", tab = %validated.tab.tab_id, window = validated.record.window_id, "PiP frame skipped: the tab is not the selected tab of its window");
        }
        crate::pip_hook::note_bound_window(window);
    }

    /// Animate platform-owned browser feedback without coupling it to input
    /// delivery. Only main-frame viewport points are mapped: child-frame CDP
    /// coordinates are not necessarily in the top-level viewport space, and a
    /// misleading cursor is worse than no cursor.
    pub(crate) async fn visualize_browser_action(
        &self,
        session: &str,
        validated: &ValidatedTab,
        cdp_session: &str,
        viewport_x: f64,
        viewport_y: f64,
        kind: BrowserVisualActionKind,
    ) {
        // `document.visibilityState` distinguishes the selected tab without
        // focusing its native window or invoking any CDP activation command.
        // Treat an unavailable or malformed proof as inactive: omitting
        // feedback is safer than drawing a cursor over another tab.
        let tab_is_active = validated
            .conn
            .call(
                Some(&validated.cdp_session),
                "Runtime.evaluate",
                json!({
                    "expression": "document.visibilityState === 'visible'",
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
            )
            .await
            .ok()
            .and_then(|result| result.pointer("/result/value").and_then(Value::as_bool))
            .unwrap_or(false);

        let screen_point = if cdp_session == validated.cdp_session {
            validated
                .conn
                .call(Some(cdp_session), "Page.getLayoutMetrics", json!({}))
                .await
                .ok()
                .and_then(|metrics| {
                    viewport_point_to_screen(
                        validated.native.bounds,
                        &metrics,
                        viewport_x,
                        viewport_y,
                    )
                })
        } else {
            // Child-frame coordinates are not necessarily in the top-level
            // viewport. Still update active-tab gating, but do not guess a
            // pointer position.
            None
        };
        let (screen_x, screen_y) = screen_point
            .map(|(x, y)| (Some(x), Some(y)))
            .unwrap_or((None, None));
        self.platform
            .visualize_browser_action(BrowserVisualAction {
                session: session.to_owned(),
                window_id: validated.native.window_id,
                cdp_target_id: validated.tab.cdp_target_id.clone(),
                tab_is_active,
                screen_x,
                screen_y,
                kind,
            })
            .await;
    }

    /// Serialize the full revalidate-dispatch-verify interval by the real CDP
    /// target rather than by a caller-controlled session id.
    pub(crate) async fn lock_mutation(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, BrowserRefusal> {
        let record = self.store.get_target(session, target_id)?;
        let tab = record.tabs.get(tab_id).ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserTabNotFound,
                format!("tab {tab_id} is not known for target {target_id}"),
            )
        })?;
        Ok(self
            .mutation_gates
            .lock(MutationKey::new(&record.fingerprint, &tab.cdp_target_id))
            .await)
    }

    // ── Frame identity / OOPIF plumbing ─────────────────────────────────

    /// Fetch and parse one session's local frame tree.
    async fn local_frame_tree(
        &self,
        conn: &CdpConnection,
        cdp_session: &str,
    ) -> Result<LocalFrameTree, FrameTreeError> {
        match conn
            .call(Some(cdp_session), "Page.getFrameTree", json!({}))
            .await
        {
            Ok(v) => parse_frame_tree(&v).ok_or_else(|| {
                FrameTreeError::Failed(anyhow::anyhow!(
                    "Page.getFrameTree returned a malformed frame tree"
                ))
            }),
            Err(e) if is_method_unsupported(&e) => Err(FrameTreeError::Unsupported),
            Err(e) => Err(FrameTreeError::Failed(e)),
        }
    }

    /// Capability-tested OOPIF discovery: enable flattened auto-attach
    /// on `tab_session` and collect the iframe children Chromium
    /// announces for existing targets *before* the setAutoAttach ack.
    ///
    /// Containment: only `Target.attachedToTarget` events scoped to
    /// this exact tab session are honored, and only `type == "iframe"`
    /// targets — a child announced on any other session (or a popup
    /// page target) is ignored. Each operation attaches its own fresh
    /// tab session, so the parent-session filter is per-operation
    /// unique even on the shared pooled connection.
    async fn attached_iframe_children(
        &self,
        conn: &CdpConnection,
        tab_session: &str,
    ) -> Result<Vec<AttachedChildFrame>, AttachError> {
        // Subscribe BEFORE issuing the command so pre-ack events are
        // guaranteed to be queued when the call returns.
        let mut events = conn.subscribe();
        match conn
            .call(
                Some(tab_session),
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
            )
            .await
        {
            Ok(_) => {}
            Err(e) if is_method_unsupported(&e) => return Err(AttachError::Unsupported),
            Err(e) => return Err(AttachError::Failed(e)),
        }
        let mut out = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.method != "Target.attachedToTarget" {
                continue;
            }
            if event.session_id.as_deref() != Some(tab_session) {
                continue; // not contained beneath the proven tab session
            }
            let info = &event.params["targetInfo"];
            if info["type"].as_str() != Some("iframe") {
                continue; // OOPIF slice covers iframes only, never popups/workers
            }
            let (Some(session_id), Some(target_id)) = (
                event.params["sessionId"].as_str(),
                info["targetId"].as_str(),
            ) else {
                continue;
            };
            out.push(AttachedChildFrame {
                session_id: session_id.to_owned(),
                target_id: target_id.to_owned(),
                url: info["url"].as_str().unwrap_or("").to_owned(),
            });
        }
        Ok(out)
    }

    /// Re-prove a ref before it is touched and return the CDP session its
    /// `backendNodeId` is valid in. Called after
    /// [`Self::revalidate_for_mutation`]. The frame's document identity is
    /// proven for every ref; a semantic ref must also still be on the
    /// attachment it was issued on and still read as what it named (see the
    /// ownership table in [`super::observation`]). Anything that cannot be
    /// proven is a refusal: a ref is never refreshed into validity here.
    pub(crate) async fn frame_session_for_mutation(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
        validated: &ValidatedTab,
        entry: &RefEntry,
    ) -> Result<String, BrowserRefusal> {
        let frame_session = self
            .frame_session(session, target_id, tab_id, validated, &entry.frame)
            .await?;
        if !entry.semantic {
            return Ok(frame_session);
        }
        if entry.attachment != Some(validated.conn.attachment(&validated.tab.cdp_target_id)) {
            self.store.invalidate_tab_refs(
                session,
                target_id,
                tab_id,
                FullReason::AttachmentChanged,
            );
            return Err(refuse(
                BrowserRefusalCode::BrowserRefStale,
                "the debugger was detached from this tab after the ref was issued, so the ref \
                 no longer proves its element; re-run get_browser_state to re-snapshot",
            ));
        }
        let live = self
            .live_fingerprint(&validated.conn, &frame_session, entry)
            .await?;
        if live != Some(Fingerprint::of(entry).accessible()) {
            // Stale for good, even if the node reads as before again later.
            self.store.retire_refused(session, target_id, tab_id, entry);
            // What it reads as now is page content: a read says that, to a
            // caller allowed to read. The refusal only says it changed.
            return Err(refuse(
                BrowserRefusalCode::BrowserRefStale,
                if live.is_some() {
                    "the element this ref named now reads as another element (its role, name \
                     or link destination changed after the ref was issued); re-run \
                     get_browser_state to re-snapshot"
                } else {
                    "the ref's node is no longer in the live page; re-run get_browser_state \
                     to re-snapshot"
                },
            ));
        }
        if let Some(row) = &entry.row {
            if !row_still_reads(&validated.conn, &frame_session, entry.backend_node_id, row).await {
                self.store.retire_refused(session, target_id, tab_id, entry);
                return Err(refuse(
                    BrowserRefusalCode::BrowserRefStale,
                    "the row this control was named after no longer reads as that name (the \
                     page may have reused the element for another item); re-run \
                     get_browser_state to re-snapshot",
                ));
            }
        }
        Ok(frame_session)
    }

    /// What a ref's node reads as right now, by the rules its snapshot used;
    /// `None` when the node is gone.
    async fn live_fingerprint(
        &self,
        conn: &CdpConnection,
        cdp_session: &str,
        entry: &RefEntry,
    ) -> Result<Option<Fingerprint>, BrowserRefusal> {
        let backend = entry.backend_node_id;
        let accessible = match conn
            .call(
                Some(cdp_session),
                "Accessibility.getPartialAXTree",
                json!({ "backendNodeId": backend, "fetchRelatives": false }),
            )
            .await
        {
            Ok(tree) => tree
                .get("nodes")
                .and_then(Value::as_array)
                .and_then(|nodes| {
                    nodes.iter().find(|node| {
                        node.get("backendDOMNodeId").and_then(Value::as_i64) == Some(backend)
                    })
                })
                .and_then(ax_reading),
            Err(error) if is_method_unsupported(&error) => {
                return Err(refuse(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    "the browser cannot report one node's accessibility data, so the ref \
                     cannot be re-proven before it is used",
                ))
            }
            // The node does not resolve; the DOM read below settles it.
            Err(_) => None,
        };
        let (role, name, mut destination) = match accessible {
            Some(reading) => reading,
            None => {
                let Ok(described) = conn
                    .call(
                        Some(cdp_session),
                        "DOM.describeNode",
                        json!({ "backendNodeId": backend }),
                    )
                    .await
                else {
                    return Ok(None);
                };
                let Some(node) = described.get("node") else {
                    return Ok(None);
                };
                let (role, name) = dom_reading(node);
                (role, name, None)
            }
        };
        if destination.is_none() && entry.destination.is_some() {
            // A link whose destination accessibility does not report: ask the
            // element, which resolves its href as the browser would follow it.
            destination = self.live_href(conn, cdp_session, backend).await;
        }
        Ok(Some(Fingerprint::read(role, name, destination)))
    }

    async fn live_href(
        &self,
        conn: &CdpConnection,
        cdp_session: &str,
        backend: i64,
    ) -> Option<String> {
        let resolved = conn
            .call(
                Some(cdp_session),
                "DOM.resolveNode",
                json!({ "backendNodeId": backend }),
            )
            .await
            .ok()?;
        let object_id = resolved.pointer("/object/objectId")?.as_str()?;
        conn.call(
            Some(cdp_session),
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration":
                    "function() { return typeof this.href === 'string' ? this.href : null; }",
                "returnByValue": true,
            }),
        )
        .await
        .ok()?
        .pointer("/result/value")?
        .as_str()
        .map(str::to_owned)
    }

    /// Re-prove a ref's frame/document identity and return the CDP
    /// session its `backendNodeId` is valid in. Any identity that cannot
    /// be re-proven is a refusal, and a stale document additionally
    /// invalidates the tab's snapshots.
    async fn frame_session(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
        validated: &ValidatedTab,
        frame: &FrameRef,
    ) -> Result<String, BrowserRefusal> {
        let conn = &validated.conn;
        let stale = |message: &str| {
            self.store
                .invalidate_tab_snapshots(session, target_id, tab_id);
            refuse(BrowserRefusalCode::BrowserRefStale, message)
        };
        let tree_err = |e: FrameTreeError| match e {
            FrameTreeError::Unsupported => refuse(
                BrowserRefusalCode::BrowserRouteUnavailable,
                "the browser no longer reports its frame tree — the ref's frame identity \
                 cannot be re-proven",
            ),
            FrameTreeError::Failed(err) => {
                route_err("Page.getFrameTree failed during frame revalidation", err)
            }
        };

        match &frame.oopif_target_id {
            None => {
                // v1-compat main-frame refs minted without a frame tree
                // carry no identity to re-check; node liveness (box
                // model / focus failures map to stale) is the backstop.
                let Some(identity) = &frame.identity else {
                    return Ok(validated.cdp_session.clone());
                };
                let tree = self
                    .local_frame_tree(conn, &validated.cdp_session)
                    .await
                    .map_err(tree_err)?;
                if tree.proves(identity) {
                    Ok(validated.cdp_session.clone())
                } else {
                    Err(stale(
                        "the ref's frame navigated or was removed since the snapshot — \
                         re-run get_browser_state to re-snapshot",
                    ))
                }
            }
            Some(oopif_target) => {
                let Some(identity) = &frame.identity else {
                    // Unreachable by construction (OOPIF refs are only
                    // minted with identity); refuse defensively.
                    return Err(refuse(
                        BrowserRefusalCode::BrowserRefStale,
                        "the OOPIF ref carries no provable frame identity",
                    ));
                };
                let children = self
                    .attached_iframe_children(conn, &validated.cdp_session)
                    .await
                    .map_err(|e| match e {
                        AttachError::Unsupported => refuse(
                            BrowserRefusalCode::BrowserRouteUnavailable,
                            "the browser no longer supports capability-tested OOPIF \
                             attachment — the ref's cross-process frame cannot be re-proven",
                        ),
                        AttachError::Failed(err) => {
                            route_err("Target.setAutoAttach failed during frame revalidation", err)
                        }
                    })?;
                let Some(child) = children.into_iter().find(|c| c.target_id == *oopif_target)
                else {
                    return Err(stale(
                        "the ref's cross-process frame is no longer attached beneath the \
                         bound tab — re-run get_browser_state to re-snapshot",
                    ));
                };
                let tree = self
                    .local_frame_tree(conn, &child.session_id)
                    .await
                    .map_err(tree_err)?;
                if tree.proves(identity) {
                    Ok(child.session_id)
                } else {
                    Err(stale(
                        "the ref's cross-process frame navigated since the snapshot — \
                         re-run get_browser_state to re-snapshot",
                    ))
                }
            }
        }
    }

    // ── Read-side: page snapshot (ref minting) ──────────────────────────

    /// Capture the exact page target's current viewport through its attached
    /// CDP session. This route never calls `Target.activateTarget`,
    /// `Page.bringToFront`, or a native foreground API, so an already-open
    /// inactive tab stays inactive.
    pub(crate) async fn capture_tab_screenshot(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
    ) -> Result<BrowserTabScreenshot, BrowserRefusal> {
        let record = self.store.get_target(session, target_id)?;
        let tab = record.tabs.get(tab_id).cloned().ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserTabNotFound,
                format!("tab {tab_id} is not known for target {target_id}"),
            )
        })?;
        let conn = self.connection_for_record(session, &record).await?;
        let cdp_session = self
            .attach(
                &conn,
                &tab.cdp_target_id,
                session,
                record.generation,
                record.endpoint_transport,
            )
            .await?;
        let metrics = conn
            .call(Some(&cdp_session), "Page.getLayoutMetrics", json!({}))
            .await
            .map_err(|error| route_err("Page.getLayoutMetrics failed", error))?;
        let viewport = browser_screenshot_viewport(&metrics)?;
        let response = conn
            .call(
                Some(&cdp_session),
                "Page.captureScreenshot",
                json!({
                    "format": "png",
                    "fromSurface": true,
                    "captureBeyondViewport": false,
                    "clip": {
                        "x": viewport.page_x,
                        "y": viewport.page_y,
                        "width": viewport.width,
                        "height": viewport.height,
                        "scale": 1.0,
                    },
                }),
            )
            .await
            .map_err(|error| route_err("Page.captureScreenshot failed", error))?;
        let data_base64 = response
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                route_err(
                    "Page.captureScreenshot returned malformed data",
                    "missing base64 PNG payload",
                )
            })?
            .to_owned();
        if data_base64.len() > MAX_BROWSER_SCREENSHOT_BYTES.saturating_mul(2) {
            return Err(route_err(
                "Page.captureScreenshot returned oversized data",
                "encoded payload exceeds the browser screenshot limit",
            ));
        }
        let png = BASE64.decode(&data_base64).map_err(|error| {
            route_err(
                "Page.captureScreenshot returned malformed data",
                format!("base64 decode failed: {error}"),
            )
        })?;
        if png.len() > MAX_BROWSER_SCREENSHOT_BYTES {
            return Err(route_err(
                "Page.captureScreenshot returned oversized data",
                format!(
                    "decoded PNG is {} bytes; limit is {MAX_BROWSER_SCREENSHOT_BYTES}",
                    png.len()
                ),
            ));
        }
        let (width, height) = crate::image_utils::png_dimensions(&png).map_err(|error| {
            route_err(
                "Page.captureScreenshot returned malformed data",
                format!("invalid PNG: {error}"),
            )
        })?;
        if width == 0 || height == 0 {
            return Err(route_err(
                "Page.captureScreenshot returned malformed data",
                "PNG dimensions must be non-zero",
            ));
        }
        Ok(BrowserTabScreenshot {
            data_base64,
            width,
            height,
            viewport_css_width: viewport.width,
            viewport_css_height: viewport.height,
            pixel_to_css_scale_x: viewport.width / f64::from(width),
            pixel_to_css_scale_y: viewport.height / f64::from(height),
        })
    }

    /// Snapshot one tab's composed DOM (main frame + shadow DOM +
    /// same-process iframes + capability-tested OOPIFs) and mint
    /// `p<snap>:<index>` refs for interactive elements. Read-only
    /// against the page.
    pub(crate) async fn snapshot_tab(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
    ) -> Result<SnapshotOutcome, BrowserRefusal> {
        let record = self.store.get_target(session, target_id)?;
        let tab = record.tabs.get(tab_id).cloned().ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserTabNotFound,
                format!("tab {tab_id} is not known for target {target_id}"),
            )
        })?;
        let conn = self.connection_for_record(session, &record).await?;
        let cdp_session = self
            .attach(
                &conn,
                &tab.cdp_target_id,
                session,
                record.generation,
                record.endpoint_transport,
            )
            .await?;

        let doc = conn
            .call(
                Some(&cdp_session),
                "DOM.getDocument",
                json!({ "depth": -1, "pierce": true }),
            )
            .await
            .map_err(|e| route_err("DOM.getDocument failed", e))?;
        let root = doc.get("root").cloned().unwrap_or(Value::Null);
        let url = root
            .get("documentURL")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // Frame/document identity for the tab session's local frames.
        // Unsupported degrades to the v1-compat main-frame-only path.
        let local_tree = match self.local_frame_tree(&conn, &cdp_session).await {
            Ok(tree) => Some(tree),
            Err(FrameTreeError::Unsupported) => None,
            Err(FrameTreeError::Failed(e)) => return Err(route_err("Page.getFrameTree failed", e)),
        };

        let mut collected = Vec::new();
        let root_frame_id = root
            .get("frameId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        collect_interactive(&root, root_frame_id.as_deref(), true, &mut collected);

        let mut entries: Vec<RefEntry> = Vec::new();
        for c in collected {
            let frame = match (c.in_root_frame, &local_tree) {
                (true, Some(tree)) => FrameRef {
                    kind: FrameKind::Main,
                    oopif_target_id: None,
                    identity: Some(tree.main_identity()),
                },
                (true, None) => FrameRef::main_unproven(),
                (false, Some(tree)) => {
                    let Some(identity) = c
                        .frame_id
                        .as_deref()
                        .and_then(|frame_id| tree.identity_of(frame_id))
                    else {
                        continue; // frame identity unprovable → omit
                    };
                    FrameRef {
                        kind: FrameKind::Iframe,
                        oopif_target_id: None,
                        identity: Some(identity),
                    }
                }
                // No frame tree → iframe identity unprovable → omit.
                (false, None) => continue,
            };
            entries.push(RefEntry {
                backend_node_id: c.backend_node_id,
                node_name: c.node_name,
                label: c.label,
                actions: Vec::new(),
                visibility: None,
                semantic: false,
                frame,
                destination: None,
                attachment: None,
                minted: None,
                row: None,
            });
        }

        // OOPIF children (C2): only with a provable frame tree AND a
        // successful capability test, contained beneath this tab's own
        // session. Anything unprovable is omitted, never guessed.
        let oopif = if local_tree.is_some() {
            match self.attached_iframe_children(&conn, &cdp_session).await {
                Ok(children) => {
                    let mut attached = 0usize;
                    for child in &children {
                        let Ok(child_tree) = self.local_frame_tree(&conn, &child.session_id).await
                        else {
                            continue; // identity unprovable → omit this frame
                        };
                        let Ok(child_doc) = conn
                            .call(
                                Some(&child.session_id),
                                "DOM.getDocument",
                                json!({ "depth": -1, "pierce": true }),
                            )
                            .await
                        else {
                            continue;
                        };
                        let child_root = child_doc.get("root").cloned().unwrap_or(Value::Null);
                        let child_root_frame = child_root
                            .get("frameId")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let mut child_collected = Vec::new();
                        collect_interactive(
                            &child_root,
                            child_root_frame.as_deref(),
                            true,
                            &mut child_collected,
                        );
                        for c in child_collected {
                            let identity = if c.in_root_frame {
                                Some(child_tree.main_identity())
                            } else {
                                c.frame_id
                                    .as_deref()
                                    .and_then(|frame_id| child_tree.identity_of(frame_id))
                            };
                            let Some(identity) = identity else { continue };
                            entries.push(RefEntry {
                                backend_node_id: c.backend_node_id,
                                node_name: c.node_name,
                                label: c.label,
                                actions: Vec::new(),
                                visibility: None,
                                semantic: false,
                                frame: FrameRef {
                                    kind: FrameKind::Oopif,
                                    oopif_target_id: Some(child.target_id.clone()),
                                    identity: Some(identity),
                                },
                                destination: None,
                                attachment: None,
                                minted: None,
                                row: None,
                            });
                        }
                        attached += 1;
                    }
                    // Contain the child sessions this read minted:
                    // stop auto-attaching (best effort).
                    let _ = conn
                        .call(
                            Some(&cdp_session),
                            "Target.setAutoAttach",
                            json!({
                                "autoAttach": false,
                                "waitForDebuggerOnStart": false,
                                "flatten": true
                            }),
                        )
                        .await;
                    for child in &children {
                        let _ = conn
                            .call(
                                Some(&cdp_session),
                                "Target.detachFromTarget",
                                json!({ "sessionId": child.session_id }),
                            )
                            .await;
                    }
                    OopifStatus::Attached(attached)
                }
                Err(AttachError::Unsupported) => OopifStatus::Unsupported,
                Err(AttachError::Failed(e)) => {
                    return Err(route_err("Target.setAutoAttach failed", e))
                }
            }
        } else {
            OopifStatus::Unsupported
        };

        let truncated = entries.len() > MAX_REFS_PER_SNAPSHOT;
        entries.truncate(MAX_REFS_PER_SNAPSHOT);
        if truncated {
            tracing::warn!(
                "browser snapshot truncated to {MAX_REFS_PER_SNAPSHOT} refs for tab {tab_id}"
            );
        }

        let snapshot_id = self.store.mint_snapshot_id();
        let mut refs = HashMap::new();
        let mut listed = Vec::new();
        for (i, entry) in entries.into_iter().enumerate() {
            let idx = i as u32;
            listed.push((format_ref(snapshot_id, idx), entry.clone()));
            refs.insert(idx, entry);
        }
        // A new snapshot supersedes prior ones for the tab: only the
        // latest namespace stays resolvable, so refs can never mix
        // across snapshots of a mutating page.
        self.store.update_target(session, target_id, |rec| {
            if let Some(tab) = rec.tabs.get_mut(tab_id) {
                // Another format: the semantic space and its refs end here.
                tab.snapshots.clear();
                tab.stable = TabRefs::default();
                tab.semantic = None;
                tab.snapshots.insert(
                    snapshot_id,
                    SnapshotRecord {
                        id: snapshot_id,
                        generation: record.generation,
                        url: url.clone(),
                        refs,
                    },
                );
            }
        });
        Ok(SnapshotOutcome {
            snapshot_id,
            url,
            refs: listed,
            truncated,
            oopif,
        })
    }

    async fn collect_semantic_session(
        &self,
        conn: &Arc<CdpConnection>,
        cdp_session: &str,
        document: &Value,
        tree: Option<&LocalFrameTree>,
        oopif_target_id: Option<&str>,
    ) -> Result<SemanticDocument, BrowserRefusal> {
        let root = document.get("root").cloned().unwrap_or(Value::Null);
        let styles = SEMANTIC_COMPUTED_STYLES
            .iter()
            .map(|value| Value::String((*value).to_owned()))
            .collect::<Vec<_>>();
        let (layout, metrics) = tokio::try_join!(
            conn.call(
                Some(cdp_session),
                "DOMSnapshot.captureSnapshot",
                json!({
                    "computedStyles": styles,
                    "includePaintOrder": true,
                    "includeDOMRects": true
                }),
            ),
            conn.call(Some(cdp_session), "Page.getLayoutMetrics", json!({})),
        )
        .map_err(|error| route_err("semantic layout collection failed", error))?;
        let dom = build_dom_index(&root);
        let title = snapshot_document_title(&layout, &root);
        let layout = build_layout_index(&layout);
        let viewport = parse_viewport(&metrics);

        let identities = tree.map(LocalFrameTree::identities).unwrap_or_default();
        let mut result = SemanticDocument::default();
        if identities.is_empty() {
            let ax = conn
                .call(Some(cdp_session), "Accessibility.getFullAXTree", json!({}))
                .await
                .map_err(|error| route_err("Accessibility.getFullAXTree failed", error))?;
            result = compose_accessibility_tree(
                &ax,
                &dom,
                &layout,
                &viewport,
                FrameRef::main_unproven(),
            );
        } else {
            for (index, identity) in identities.into_iter().enumerate() {
                let ax = conn
                    .call(
                        Some(cdp_session),
                        "Accessibility.getFullAXTree",
                        json!({ "frameId": identity.frame_id }),
                    )
                    .await
                    .map_err(|error| route_err("Accessibility.getFullAXTree failed", error))?;
                let kind = if oopif_target_id.is_some() {
                    FrameKind::Oopif
                } else if index == 0 {
                    FrameKind::Main
                } else {
                    FrameKind::Iframe
                };
                let mut frame_document = compose_accessibility_tree(
                    &ax,
                    &dom,
                    &layout,
                    &viewport,
                    FrameRef {
                        kind,
                        oopif_target_id: oopif_target_id.map(str::to_owned),
                        identity: Some(identity),
                    },
                );
                if index > 0 {
                    frame_document.css_hidden_dom_count = 0;
                }
                result.extend(frame_document);
            }
        }
        // The snapshot title when it is provably this root document's; else
        // the main frame's accessibility root name. Never the tab's cached one.
        result.title = title.or_else(|| result.document_title().map(str::to_owned));
        Ok(result)
    }

    async fn semantic_document(
        &self,
        conn: &Arc<CdpConnection>,
        cdp_session: &str,
    ) -> Result<(Value, bool), BrowserRefusal> {
        match conn
            .call(
                Some(cdp_session),
                "DOM.getDocument",
                json!({ "depth": -1, "pierce": true }),
            )
            .await
        {
            Ok(document) => Ok((document, true)),
            Err(error) if is_semantic_document_fallback_error(&error) => {
                let mut last_size_error = error.to_string();
                let fallback_depths = if is_semantic_document_timeout_error(&error) {
                    SEMANTIC_DOM_TIMEOUT_FALLBACK_DEPTHS
                } else {
                    SEMANTIC_DOM_FALLBACK_DEPTHS
                };
                for depth in fallback_depths {
                    match conn
                        .call(
                            Some(cdp_session),
                            "DOM.getDocument",
                            json!({ "depth": depth, "pierce": true }),
                        )
                        .await
                    {
                        Ok(document) => {
                            let document = self
                                .hydrate_semantic_document(conn, cdp_session, document)
                                .await?;
                            return Ok((document, false));
                        }
                        Err(error) if is_semantic_document_fallback_error(&error) => {
                            last_size_error = error.to_string();
                        }
                        Err(error) => {
                            return Err(route_err("bounded DOM.getDocument fallback failed", error))
                        }
                    }
                }
                Err(route_err(
                    "bounded DOM.getDocument fallback exhausted every accepted depth",
                    last_size_error,
                ))
            }
            Err(error) => Err(route_err("DOM.getDocument failed", error)),
        }
    }

    async fn hydrate_semantic_document(
        &self,
        conn: &Arc<CdpConnection>,
        cdp_session: &str,
        mut document: Value,
    ) -> Result<Value, BrowserRefusal> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut attempted = HashSet::new();
        for _ in 0..MAX_SEMANTIC_DOM_HYDRATION_CALLS {
            let mut scan = DomCoverageScan::default();
            scan_dom_coverage(&document, &mut scan);
            if scan.budget_exhausted {
                break;
            }
            let Some(backend_node_id) = scan
                .truncated
                .into_iter()
                .find(|backend_node_id| attempted.insert(*backend_node_id))
            else {
                break;
            };
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let response = match tokio::time::timeout(
                deadline - now,
                conn.call(
                    Some(cdp_session),
                    "DOM.describeNode",
                    json!({
                        "backendNodeId": backend_node_id,
                        "depth": SEMANTIC_DOM_HYDRATION_DEPTH,
                        "pierce": true
                    }),
                ),
            )
            .await
            {
                Err(_) => break,
                Ok(Ok(response)) => response,
                Ok(Err(error)) if is_semantic_document_size_error(&error) => continue,
                Ok(Err(error)) => {
                    return Err(route_err("DOM.describeNode hydration failed", error))
                }
            };
            let Some(node) = response.get("node") else {
                continue;
            };
            let _ = replace_dom_node(&mut document, backend_node_id, node);
        }
        Ok(document)
    }

    // One semantic read: its scope, its paging and its size budget.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn snapshot_tab_semantic(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
        scope_ref: Option<&str>,
        query: Option<&str>,
        continuation: Option<&str>,
        max_chars: usize,
        include_refs: bool,
        since: Option<u64>,
    ) -> Result<SemanticSnapshotOutcome, BrowserRefusal> {
        if let Some(token) = continuation {
            if scope_ref.is_some() || query.is_some() {
                return Err(refuse(
                    BrowserRefusalCode::BrowserRefStale,
                    "continuation cannot be combined with a new scope_ref or query",
                ));
            }
            let (cache, continuation) = self
                .store
                .resolve_semantic_continuation(session, target_id, tab_id, token)?;
            let record = self.store.get_target(session, target_id)?;
            let tab = record.tabs.get(tab_id).cloned().ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserTabNotFound,
                    format!("tab {tab_id} is not known for target {target_id}"),
                )
            })?;
            // A continuation pages the document recorded in a ref space and
            // takes its refs from it: prove that space is still the live
            // document on the live attachment, and do it one observation at
            // a time like any other read.
            let _observing = self
                .observation_gates
                .lock((session.to_owned(), target_id.to_owned(), tab_id.to_owned()))
                .await;
            let conn = self.connection_for_record(session, &record).await?;
            let cdp_session = self
                .attach(
                    &conn,
                    &tab.cdp_target_id,
                    session,
                    record.generation,
                    record.endpoint_transport,
                )
                .await?;
            let root = match self.local_frame_tree(&conn, &cdp_session).await {
                Ok(tree) => Some(tree.main_identity()),
                // No frame tree, no proof that the document is the one that
                // was recorded: the continuation is refused below.
                Err(FrameTreeError::Unsupported) => None,
                Err(FrameTreeError::Failed(error)) => {
                    return Err(route_err(
                        "Page.getFrameTree failed during semantic continuation revalidation",
                        error,
                    ))
                }
            };
            let live = DocumentIdentity {
                generation: record.generation,
                attachment: conn.attachment(&tab.cdp_target_id),
                cdp_target_id: tab.cdp_target_id.clone(),
                root,
            };
            if tab
                .stable
                .space()
                .is_none_or(|space| space.id != cache.space_id || !space.identity.proves(&live))
            {
                self.store
                    .invalidate_tab_snapshots(session, target_id, tab_id);
                return Err(refuse(
                    BrowserRefusalCode::BrowserRefStale,
                    "the page navigated or the debugger was detached since this semantic \
                     continuation was minted, or the browser cannot prove it did not; re-run \
                     get_browser_state to start a fresh snapshot (max_chars admits more in one \
                     read)",
                ));
            }
            let title = cache.document.title.clone().unwrap_or_default();
            let outline_budget = outline_budget(max_chars, &cache.url, &title);
            let page = cache.document.page_sized(
                continuation.offset,
                DEFAULT_SEMANTIC_NODE_BUDGET,
                outline_budget,
                continuation.query.as_deref(),
                continuation.scope_backend_node_id,
                include_refs,
                None,
            );
            let facts = include_refs.then(|| listed_facts(&page));
            let oopif = if continuation.oopif_supported {
                OopifStatus::Attached(continuation.oopif_frames)
            } else {
                OopifStatus::Unsupported
            };
            let next_offset = page.next_offset;
            let next_token = next_offset.map(|_| format!("bc-{}", Uuid::new_v4()));
            let (action_refs, content_refs) = ref_counts(&page.view);
            let mut lines = None;
            self.store.update_target(session, target_id, |record| {
                let Some(tab) = record.tabs.get_mut(tab_id) else {
                    return;
                };
                let Some(stored) = tab
                    .semantic
                    .as_mut()
                    .filter(|stored| stored.space_id == cache.space_id)
                else {
                    return;
                };
                // Single use: a token names one page of one recorded document.
                if stored.continuations.remove(token).is_none() {
                    return;
                }
                if let (Some(token), Some(offset)) = (next_token.clone(), next_offset) {
                    stored.continuations.insert(
                        token,
                        SemanticContinuation {
                            offset,
                            query: continuation.query.clone(),
                            scope_backend_node_id: continuation.scope_backend_node_id,
                            oopif_supported: continuation.oopif_supported,
                            oopif_frames: continuation.oopif_frames,
                        },
                    );
                }
                lines = tab.stable.extend(page.view);
            });
            let lines = lines.ok_or_else(|| {
                refuse(
                    BrowserRefusalCode::BrowserRefStale,
                    "the semantic continuation was used or superseded by a newer snapshot",
                )
            })?;
            return Ok(SemanticSnapshotOutcome {
                snapshot_id: cache.space_id,
                url: cache.url,
                title,
                outline: outline_of(&lines),
                action_refs,
                content_refs,
                complete: cache.document.complete && next_offset.is_none(),
                scope: "continuation",
                selected_nodes: page.selected_nodes,
                total_nodes: page.total_nodes,
                omissions: page.omissions,
                continuation: next_token,
                oopif,
                outline_budget,
                listed: facts.map(|facts| listed_refs(&lines, facts)),
                revision: None,
                told: Told::Snapshot { reason: None },
            });
        }

        let scope_backend_node_id = match scope_ref {
            Some(external) => Some(
                self.store
                    .resolve_ref(session, target_id, tab_id, external)?
                    .backend_node_id,
            ),
            None => None,
        };
        let record = self.store.get_target(session, target_id)?;
        let tab = record.tabs.get(tab_id).cloned().ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserTabNotFound,
                format!("tab {tab_id} is not known for target {target_id}"),
            )
        })?;
        // Collect and record as one step per session and tab (see the
        // ownership table: observations of a tab run one at a time).
        let _observing = self
            .observation_gates
            .lock((session.to_owned(), target_id.to_owned(), tab_id.to_owned()))
            .await;
        let conn = self.connection_for_record(session, &record).await?;
        let cdp_session = self
            .attach(
                &conn,
                &tab.cdp_target_id,
                session,
                record.generation,
                record.endpoint_transport,
            )
            .await?;
        // A page behind an open JavaScript dialog answers nothing: say so
        // instead of waiting for every read below to time out.
        self.watch_dialogs(&conn, &cdp_session, &tab.cdp_target_id)
            .await;
        if let Some(dialog) = conn.dialog_state(&tab.cdp_target_id) {
            return Err(dialog_open_refusal(&dialog));
        }
        let (document, document_complete) = self.semantic_document(&conn, &cdp_session).await?;
        let root = document.get("root").cloned().unwrap_or(Value::Null);
        let url = root
            .get("documentURL")
            .and_then(Value::as_str)
            .unwrap_or(&tab.url)
            .to_owned();
        let local_tree = match self.local_frame_tree(&conn, &cdp_session).await {
            Ok(tree) => Some(tree),
            Err(FrameTreeError::Unsupported) => None,
            Err(FrameTreeError::Failed(error)) => {
                return Err(route_err("Page.getFrameTree failed", error))
            }
        };
        let semantic_root_identity = local_tree.as_ref().map(LocalFrameTree::main_identity);
        let mut semantic = self
            .collect_semantic_session(&conn, &cdp_session, &document, local_tree.as_ref(), None)
            .await?;
        semantic.complete &= document_complete;
        // The title collected with this document. An unprovable title marks
        // the snapshot incomplete below; it never revives the tab's cached
        // title from an earlier document.
        let title = semantic.title.clone().unwrap_or_default();

        let oopif = if local_tree.is_some() {
            match self.attached_iframe_children(&conn, &cdp_session).await {
                Ok(children) => {
                    let mut attached = 0;
                    for child in &children {
                        let child_tree = match self.local_frame_tree(&conn, &child.session_id).await
                        {
                            Ok(tree) => tree,
                            Err(_) => {
                                semantic.unprovable_frame_count += 1;
                                semantic.complete = false;
                                continue;
                            }
                        };
                        let (child_document, child_complete) =
                            match self.semantic_document(&conn, &child.session_id).await {
                                Ok(document) => document,
                                Err(_) => {
                                    semantic.unprovable_frame_count += 1;
                                    semantic.complete = false;
                                    continue;
                                }
                            };
                        match self
                            .collect_semantic_session(
                                &conn,
                                &child.session_id,
                                &child_document,
                                Some(&child_tree),
                                Some(&child.target_id),
                            )
                            .await
                        {
                            Ok(document) => {
                                let mut document = document;
                                document.complete &= child_complete;
                                semantic.extend(document);
                                attached += 1;
                            }
                            Err(_) => {
                                semantic.unprovable_frame_count += 1;
                                semantic.complete = false;
                            }
                        }
                    }
                    let _ = conn
                        .call(
                            Some(&cdp_session),
                            "Target.setAutoAttach",
                            json!({
                                "autoAttach": false,
                                "waitForDebuggerOnStart": false,
                                "flatten": true
                            }),
                        )
                        .await;
                    for child in &children {
                        let _ = conn
                            .call(
                                Some(&cdp_session),
                                "Target.detachFromTarget",
                                json!({ "sessionId": child.session_id }),
                            )
                            .await;
                    }
                    OopifStatus::Attached(attached)
                }
                Err(AttachError::Unsupported) => OopifStatus::Unsupported,
                Err(AttachError::Failed(error)) => {
                    return Err(route_err("Target.setAutoAttach failed", error))
                }
            }
        } else {
            OopifStatus::Unsupported
        };

        semantic.complete &= semantic.title.is_some();
        let outline_budget = outline_budget(max_chars, &url, &title);
        // A read that will be diffed stops where the baseline was cut.
        let until = since.and_then(|since| {
            let target = self.store.get_target(session, target_id).ok()?;
            let space = target.tabs.get(tab_id)?.stable.space()?;
            space.baseline_tail(since).cloned()
        });
        let page = semantic.page_sized(
            0,
            DEFAULT_SEMANTIC_NODE_BUDGET,
            outline_budget,
            query,
            scope_backend_node_id,
            include_refs,
            until.as_ref(),
        );
        let tail = page.tail.clone();
        let facts = include_refs.then(|| listed_facts(&page));
        let next_offset = page.next_offset;
        let (scope, kind) = if scope_ref.is_some() {
            ("subtree", ViewKind::Side)
        } else if query.is_some() {
            ("query", ViewKind::Side)
        } else {
            ("viewport", ViewKind::Default)
        };
        let continuation = next_offset.map(|_| format!("bc-{}", Uuid::new_v4()));
        let (action_refs, content_refs) = ref_counts(&page.view);
        let document_entries: Vec<RefEntry> = semantic
            .nodes
            .iter()
            .filter_map(SemanticNode::to_ref_entry)
            .collect();
        let identity = DocumentIdentity {
            generation: record.generation,
            attachment: conn.attachment(&tab.cdp_target_id),
            cdp_target_id: tab.cdp_target_id.clone(),
            root: semantic_root_identity.clone(),
        };
        let complete = semantic.complete && next_offset.is_none();
        let (selected_nodes, total_nodes, omissions) =
            (page.selected_nodes, page.total_nodes, page.omissions);
        let view = page.view;
        let mut recorded = None;
        self.store
            .update_target(session, target_id, |stored_target| {
                let Some(stored_tab) = stored_target.tabs.get_mut(tab_id) else {
                    return;
                };
                stored_tab.url = url.clone();
                stored_tab.title = title.clone();
                // A semantic snapshot ends any dom_refs_v1 one. Its own refs
                // persist while the document and the attachment are proven
                // the same (the ownership table in observation.rs).
                stored_tab.snapshots.clear();
                let outcome = stored_tab.stable.record(
                    &mut || self.store.mint_snapshot_id(),
                    identity,
                    &document_entries,
                    semantic.complete,
                    view,
                    (&url, &title),
                    tail,
                    kind,
                    since,
                );
                let mut continuations = HashMap::new();
                if let (Some(token), Some(offset)) = (continuation.clone(), next_offset) {
                    continuations.insert(
                        token,
                        SemanticContinuation {
                            offset,
                            query: query.map(str::to_owned),
                            scope_backend_node_id,
                            oopif_supported: matches!(oopif, OopifStatus::Attached(_)),
                            oopif_frames: oopif.frames(),
                        },
                    );
                }
                stored_tab.semantic = Some(SemanticCache {
                    space_id: outcome.space_id,
                    url: url.clone(),
                    document: semantic,
                    continuations,
                });
                recorded = Some(outcome);
            });
        let recorded = recorded.ok_or_else(|| {
            refuse(
                BrowserRefusalCode::BrowserBindingStale,
                "the binding ended while the page was being read; re-run get_browser_state with pid + window_id",
            )
        })?;
        Ok(SemanticSnapshotOutcome {
            snapshot_id: recorded.space_id,
            url,
            title,
            outline: outline_of(&recorded.lines),
            action_refs,
            content_refs,
            complete,
            scope,
            selected_nodes,
            total_nodes,
            omissions,
            continuation,
            oopif,
            outline_budget,
            listed: facts.map(|facts| listed_refs(&recorded.lines, facts)),
            revision: recorded.revision,
            told: recorded.told,
        })
    }

    // ── Dialogs, settling, and what a session holds ─────────────────────

    /// Make this connection hear the tab's JavaScript dialog events. They
    /// need the Page domain enabled on one attached session; its id is
    /// registered first so an opening event that arrives before the reply
    /// is still attributed to the tab. Best effort and bounded: with a
    /// dialog already up, the enable itself may never answer, but Chrome
    /// announces the open dialog to the newly enabled session.
    pub(crate) async fn watch_dialogs(
        &self,
        conn: &CdpConnection,
        cdp_session: &str,
        cdp_target_id: &str,
    ) {
        if conn.dialog_state(cdp_target_id).is_some() || conn.has_dialog_session(cdp_target_id) {
            return;
        }
        conn.register_dialog_session(cdp_session, cdp_target_id);
        let enabled = tokio::time::timeout(
            DIALOG_WATCH_TIMEOUT,
            conn.call(Some(cdp_session), "Page.enable", json!({})),
        )
        .await;
        if !matches!(enabled, Ok(Ok(_))) && conn.dialog_state(cdp_target_id).is_none() {
            conn.unregister_dialog_session(cdp_session, cdp_target_id);
        }
    }

    /// Run one CDP command that page script answers (input dispatch), but
    /// stop waiting when a JavaScript dialog opens: the dialog blocks the
    /// page, so the command's reply would only come once it is resolved.
    pub(crate) async fn call_until_dialog(
        &self,
        conn: &CdpConnection,
        cdp_session: &str,
        cdp_target_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, CallStopped> {
        let call = conn.call(Some(cdp_session), method, params);
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => return result.map_err(CallStopped::Failed),
                _ = tokio::time::sleep(DIALOG_POLL) => {
                    if let Some(dialog) = conn.dialog_state(cdp_target_id) {
                        return Err(CallStopped::Dialog(dialog));
                    }
                }
            }
        }
    }

    /// Start watching a tab before an action is sent, so a navigation the
    /// action sets off is seen when it starts, not only once the new
    /// document has replaced the old. `None` when the browser reports no
    /// frame tree: then a new document is only noticed when the old one's
    /// script context goes away.
    pub(crate) async fn page_watch(&self, validated: &ValidatedTab) -> Option<PageWatch> {
        // Subscribed first: nothing between here and the action is missed.
        let events = validated.conn.subscribe();
        let tree = self
            .local_frame_tree(&validated.conn, &validated.cdp_session)
            .await
            .ok()?;
        Some(PageWatch {
            events,
            main: tree.main_identity(),
            navigating: false,
        })
    }

    /// Wait, within bounds, until the page has stopped changing after an
    /// action: two quiet polls of a mutation counter in a row, the deadline,
    /// a new document, or a JavaScript dialog. The counter lives in a page
    /// object only this session can reach; nothing is left for page script
    /// to find, and no page timer is used (timers are throttled in covered
    /// and background tabs).
    pub(crate) async fn settle(
        &self,
        validated: &ValidatedTab,
        mut watch: Option<PageWatch>,
    ) -> Settled {
        let conn = &validated.conn;
        let cdp = validated.cdp_session.as_str();
        let target = validated.tab.cdp_target_id.as_str();
        let started = tokio::time::Instant::now();
        let dialog = || conn.dialog_state(target).map(Settled::Dialog);
        if let Some(open) = dialog() {
            return open;
        }
        let main = watch.as_ref().map(|watch| watch.main.clone());
        let mut navigating = move || watch.as_mut().is_some_and(PageWatch::navigating);
        if navigating() {
            return self.await_document(validated, main.as_ref(), started).await;
        }
        let bounded = |method: &'static str, params: Value| async move {
            tokio::time::timeout(SETTLE_CALL_TIMEOUT, conn.call(Some(cdp), method, params)).await
        };
        let counter = match bounded(
            "Runtime.evaluate",
            json!({ "expression": SETTLE_COUNTER, "objectGroup": "cua-settle" }),
        )
        .await
        {
            Ok(Ok(value)) => value
                .pointer("/result/objectId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            // The document is being replaced under the call.
            Ok(Err(error)) if is_context_gone(&error) => {
                return self.await_document(validated, main.as_ref(), started).await
            }
            // The page cannot run the counter: nothing says it settled.
            Ok(Err(_)) => return Settled::Deadline,
            // No answer: a dialog, or a document still being replaced.
            Err(_) => None,
        };
        let Some(counter) = counter else {
            return match dialog() {
                Some(open) => open,
                None => self.await_document(validated, main.as_ref(), started).await,
            };
        };
        let mut quiet = 0;
        while started.elapsed() < SETTLE_DEADLINE {
            tokio::time::sleep(SETTLE_POLL).await;
            if let Some(open) = dialog() {
                return open;
            }
            // A page at rest that is about to be replaced has not settled.
            if navigating() {
                return self.await_document(validated, main.as_ref(), started).await;
            }
            match bounded(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": counter,
                    "functionDeclaration": SETTLE_TAKE,
                    "returnByValue": true,
                }),
            )
            .await
            {
                Ok(Ok(value)) => match value.pointer("/result/value").and_then(Value::as_u64) {
                    Some(0) => {
                        quiet += 1;
                        if quiet == SETTLE_QUIET_POLLS {
                            let _ = bounded(
                                "Runtime.callFunctionOn",
                                json!({ "objectId": counter, "functionDeclaration": SETTLE_STOP }),
                            )
                            .await;
                            return Settled::Quiet;
                        }
                    }
                    Some(_) => quiet = 0,
                    // The counter is gone with its document.
                    None => return self.await_document(validated, main.as_ref(), started).await,
                },
                // The counter's document was replaced.
                Ok(Err(_)) => return self.await_document(validated, main.as_ref(), started).await,
                // No answer in time: a dialog shows up at the next poll.
                Err(_) => {}
            }
        }
        let _ = bounded(
            "Runtime.callFunctionOn",
            json!({ "objectId": counter, "functionDeclaration": SETTLE_STOP }),
        )
        .await;
        Settled::Deadline
    }

    /// A new document is on its way: wait, within bounds, until it has
    /// replaced `old` (the main frame's document before the action, when
    /// known) and has loaded. When `old` is still there at the deadline the
    /// navigation did not happen, and the page is simply not settled.
    pub(crate) async fn await_document(
        &self,
        validated: &ValidatedTab,
        old: Option<&FrameIdentity>,
        started: tokio::time::Instant,
    ) -> Settled {
        let conn = &validated.conn;
        let target = validated.tab.cdp_target_id.as_str();
        let mut replaced = old.is_none();
        while started.elapsed() < NAVIGATION_DEADLINE {
            if let Some(dialog) = conn.dialog_state(target) {
                return Settled::Dialog(dialog);
            }
            if !replaced {
                let tree = tokio::time::timeout(
                    SETTLE_CALL_TIMEOUT,
                    self.local_frame_tree(conn, &validated.cdp_session),
                )
                .await;
                replaced = matches!(
                    (&tree, old),
                    (Ok(Ok(tree)), Some(old)) if tree.main_identity() != *old
                );
            }
            if replaced {
                let ready = tokio::time::timeout(
                    SETTLE_CALL_TIMEOUT,
                    conn.call(
                        Some(&validated.cdp_session),
                        "Runtime.evaluate",
                        json!({ "expression": "document.readyState", "returnByValue": true }),
                    ),
                )
                .await;
                match ready {
                    Ok(Ok(value))
                        if value.pointer("/result/value").and_then(Value::as_str)
                            == Some("complete") =>
                    {
                        // One beat for scripts that render on load.
                        tokio::time::sleep(SETTLE_POLL * 2).await;
                        return Settled::NewDocument { loaded: true };
                    }
                    // Still loading, between documents, or slow: ask again.
                    Ok(Ok(_)) | Err(_) => {}
                    Ok(Err(error)) if is_context_gone(&error) => {}
                    // The page cannot be asked at all.
                    Ok(Err(_)) => break,
                }
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
        if replaced {
            Settled::NewDocument { loaded: false }
        } else {
            Settled::Deadline
        }
    }

    /// The ref space (`p7`) the session holds for this tab, if any.
    pub(crate) fn held_space(
        &self,
        session: &str,
        target_id: &str,
        tab_id: &str,
    ) -> Option<String> {
        let target = self.store.get_target(session, target_id).ok()?;
        let space = target.tabs.get(tab_id)?.stable.space()?;
        Some(format!("p{}", space.id))
    }

    /// What the session holds for this tab: the baseline revision of its
    /// semantic space, a `dom_refs_v1` snapshot, or nothing.
    pub(crate) fn held_view(&self, session: &str, target_id: &str, tab_id: &str) -> HeldView {
        let Ok(target) = self.store.get_target(session, target_id) else {
            return HeldView::Nothing;
        };
        match target.tabs.get(tab_id) {
            Some(tab) if !tab.snapshots.is_empty() => HeldView::DomRefs,
            Some(tab) => match tab
                .stable
                .space()
                .and_then(|space| space.baseline_revision())
            {
                Some(revision) => HeldView::Semantic(revision),
                None => HeldView::Nothing,
            },
            None => HeldView::Nothing,
        }
    }
}

/// Whether a Runtime call failed because its document (execution context)
/// was replaced while the call was under way.
fn is_context_gone(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("context") || message.contains("inspected target navigated")
}

/// One tab watched from before an action: its main frame's document then,
/// and the connection's events since.
pub(crate) struct PageWatch {
    events: tokio::sync::mpsc::UnboundedReceiver<super::cdp_ws::CdpEvent>,
    main: FrameIdentity,
    navigating: bool,
}

impl PageWatch {
    /// Whether the main frame has started to load another document since
    /// the watch began. Child frames, new tabs and same-document history
    /// changes do not count.
    fn navigating(&mut self) -> bool {
        while let Ok(event) = self.events.try_recv() {
            let frame = match event.method.as_str() {
                "Page.frameRequestedNavigation"
                    if event
                        .params
                        .get("disposition")
                        .and_then(Value::as_str)
                        .is_none_or(|disposition| disposition == "currentTab") =>
                {
                    event.params.get("frameId")
                }
                "Page.frameScheduledNavigation" | "Page.frameStartedLoading" => {
                    event.params.get("frameId")
                }
                "Page.frameNavigated" => event.params.pointer("/frame/id"),
                _ => None,
            };
            if frame.and_then(Value::as_str) == Some(self.main.frame_id.as_str()) {
                self.navigating = true;
            }
        }
        self.navigating
    }
}

/// How a bounded wait for the page ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settled {
    Quiet,
    /// Still changing when the wait ended.
    Deadline,
    /// The document was replaced; `loaded` is false when it had not finished
    /// loading in time.
    NewDocument {
        loaded: bool,
    },
    /// A JavaScript dialog is open: the page cannot be read.
    Dialog(super::cdp_ws::CdpDialogState),
}

/// Why [`BrowserEngine::call_until_dialog`] did not return a reply.
#[derive(Debug)]
pub(crate) enum CallStopped {
    Dialog(super::cdp_ws::CdpDialogState),
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeldView {
    Nothing,
    /// A `dom_refs_v1` snapshot: actions leave its refs alone and return no
    /// page changes.
    DomRefs,
    /// The baseline revision an action's changes are made against.
    Semantic(u64),
}

pub(crate) fn dialog_id(dialog: &super::cdp_ws::CdpDialogState) -> String {
    format!("dialog-{}", dialog.generation)
}

pub(crate) fn dialog_open_refusal(dialog: &super::cdp_ws::CdpDialogState) -> BrowserRefusal {
    let dialog_id = dialog_id(dialog);
    refuse(
        BrowserRefusalCode::BrowserDialogOpen,
        format!(
            "a JavaScript {} dialog is open in this tab and blocks the page; resolve it with \
             browser_dialog (dialog_id {dialog_id}) before reading or acting",
            dialog.kind
        ),
    )
    .with_detail(json!({ "dialog_id": dialog_id, "kind": dialog.kind }))
}

const DIALOG_WATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const DIALOG_POLL: std::time::Duration = std::time::Duration::from_millis(50);
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(50);
const SETTLE_QUIET_POLLS: u32 = 2;
const SETTLE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1_500);
const SETTLE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
const NAVIGATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);
/// A mutation counter for the main document: `n` counts records the
/// observer's callback has seen, `o` is the observer.
// ponytail: the main document only (open shadow roots and frames are not
// observed), and the counter's handle is not released: Runtime.releaseObject
// is outside the existing-profile allowlist, and the handle is a few bytes
// that die with the document. Observe frames, and release the handle, if a
// long-lived single-page app ever shows either as a problem.
const SETTLE_COUNTER: &str = "(() => { const s = { n: 0 }; \
    s.o = new MutationObserver((records) => { s.n += records.length; }); \
    s.o.observe(document, { subtree: true, childList: true, attributes: true, characterData: true }); \
    return s; })()";
const SETTLE_TAKE: &str =
    "function() { const n = this.n + this.o.takeRecords().length; this.n = 0; return n; }";
const SETTLE_STOP: &str = "function() { this.o.disconnect(); }";

/// Attribute names that make an element interactive-enough to ref.
const INTERACTIVE_ATTRS: &[&str] = &["onclick", "role", "contenteditable", "tabindex", "href"];
/// Element names always considered interactive.
const INTERACTIVE_TAGS: &[&str] = &[
    "a", "button", "input", "select", "textarea", "option", "summary", "label",
];
/// Attributes surfaced into the human-readable ref label.
const LABEL_ATTRS: &[&str] = &[
    "aria-label",
    "placeholder",
    "name",
    "id",
    "type",
    "value",
    "href",
    "role",
];

/// One interactive element found by the composed DOM walk, before
/// frame identity has been resolved against the frame tree.
struct CollectedNode {
    backend_node_id: i64,
    node_name: String,
    label: Option<String>,
    /// Frame id of the containing frame when known (from the iframe
    /// element's / document's `frameId`).
    frame_id: Option<String>,
    /// Whether the node lives in the session's root frame (as opposed
    /// to a descended same-process `contentDocument`).
    in_root_frame: bool,
}

/// Walk a pierced `DOM.getDocument` node tree collecting interactive
/// elements in document order. Composition rules:
/// - `shadowRoots` are descended (composed into their host's frame),
///   except user-agent shadow roots (internal control chrome).
/// - Same-process iframes are descended via `contentDocument`, tagged
///   with the child frame's id.
/// - OOPIF placeholders (iframe elements without a `contentDocument`)
///   are NOT descended here — they are reached only through
///   capability-tested child sessions.
fn collect_interactive(
    node: &Value,
    frame_id: Option<&str>,
    in_root_frame: bool,
    out: &mut Vec<CollectedNode>,
) {
    let node_type = node.get("nodeType").and_then(Value::as_i64).unwrap_or(0);
    if node_type == 1 {
        let name = node
            .get("nodeName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let attrs: Vec<String> = node
            .get("attributes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let attr = |key: &str| -> Option<&str> {
            attrs
                .chunks_exact(2)
                .find(|kv| kv[0].eq_ignore_ascii_case(key))
                .map(|kv| kv[1].as_str())
        };
        let interactive = INTERACTIVE_TAGS.contains(&name.as_str())
            || INTERACTIVE_ATTRS.iter().any(|a| attr(a).is_some());
        if interactive {
            if let Some(backend) = node.get("backendNodeId").and_then(Value::as_i64) {
                let label = LABEL_ATTRS
                    .iter()
                    .filter_map(|k| attr(k).map(|v| format!("{k}={v}")))
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push(CollectedNode {
                    backend_node_id: backend,
                    node_name: name,
                    label: (!label.is_empty()).then_some(label),
                    frame_id: frame_id.map(str::to_owned),
                    in_root_frame,
                });
            }
        }
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            collect_interactive(child, frame_id, in_root_frame, out);
        }
    }
    if let Some(shadow_roots) = node.get("shadowRoots").and_then(Value::as_array) {
        for shadow_root in shadow_roots {
            if shadow_root.get("shadowRootType").and_then(Value::as_str) == Some("user-agent") {
                continue;
            }
            collect_interactive(shadow_root, frame_id, in_root_frame, out);
        }
    }
    if let Some(content_document) = node.get("contentDocument") {
        // Same-process iframe. The child frame id lives on the iframe
        // element (and again on the content document); without it the
        // frame's identity is unprovable and its refs get omitted.
        let child_frame_id = node
            .get("frameId")
            .or_else(|| content_document.get("frameId"))
            .and_then(Value::as_str);
        collect_interactive(content_document, child_frame_id, false, out);
    }
}

/// The relay's holder name for one Cua session episode. A session that ends
/// and starts again gets a new grant generation, so a late release from the
/// ended episode can never remove the new episode's hold on a tab.
pub(crate) fn relay_holder(session: &str, generation: u64) -> String {
    format!("{session}#{generation}")
}

/// The relay holder a grant's tabs were attached under, if it uses the relay.
fn grant_relay_holder(grant: &ExistingProfileGrant) -> Option<String> {
    (grant.endpoint_transport == super::types::EndpointTransport::ExtensionRelay)
        .then(|| relay_holder(&grant.public_session, grant.generation))
}

/// Release one grant's claim on its browser socket. Through the extension
/// relay the socket may be shared with other Cua sessions, so the relay is
/// first told this session's tabs are released; the socket itself closes only
/// when its last grant releases it.
pub(crate) async fn release_grant_claim(pool: &CdpPool, grant: &ExistingProfileGrant) {
    if let Some(holder) = grant_relay_holder(grant) {
        if let Ok(conn) = pool
            .get_existing(&grant.endpoint_ws_url, grant.generation)
            .await
        {
            super::cdp_ws::release_relay_holder(&conn, &holder).await;
        }
    }
    pool.release_existing(&grant.endpoint_ws_url, grant.generation)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_access_policy_requires_grants_for_standalone_consumers() {
        assert_eq!(
            endpoint_access_class(true, false, BrowserProcessRole::StandaloneConsumer).unwrap(),
            EndpointAccessClass::ExistingProfileApproved
        );
        assert_eq!(
            endpoint_access_class(false, true, BrowserProcessRole::StandaloneConsumer).unwrap(),
            EndpointAccessClass::DriverOwned
        );
        assert_eq!(
            endpoint_access_class(false, false, BrowserProcessRole::EmbeddedApplication).unwrap(),
            EndpointAccessClass::EmbeddedApplication
        );

        let consumer = endpoint_access_class(false, false, BrowserProcessRole::StandaloneConsumer)
            .unwrap_err();
        assert_eq!(consumer.code, BrowserRefusalCode::BrowserConsentRequired);
        assert_eq!(
            consumer.detail.unwrap()["reason"],
            "consumer_profile_endpoint_requires_grant"
        );
        assert_eq!(
            endpoint_access_class(false, false, BrowserProcessRole::Helper)
                .unwrap_err()
                .code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
        assert_eq!(
            endpoint_access_class(false, false, BrowserProcessRole::Unknown)
                .unwrap_err()
                .code,
            BrowserRefusalCode::BrowserRouteUnavailable
        );
    }

    #[test]
    fn viewport_point_maps_below_browser_chrome_in_live_native_bounds() {
        let point = viewport_point_to_screen(
            Rect::new(100.0, 50.0, 1000.0, 800.0),
            &json!({
                "cssVisualViewport": {
                    "clientWidth": 980.0,
                    "clientHeight": 700.0
                }
            }),
            240.0,
            120.0,
        );
        assert_eq!(point, Some((350.0, 270.0)));
    }

    #[test]
    fn viewport_point_refuses_untrustworthy_or_out_of_view_geometry() {
        let native = Rect::new(0.0, 0.0, 800.0, 600.0);
        assert_eq!(
            viewport_point_to_screen(
                native,
                &json!({"cssVisualViewport":{"clientWidth":900.0,"clientHeight":600.0}}),
                10.0,
                10.0,
            ),
            None
        );
        assert_eq!(
            viewport_point_to_screen(
                native,
                &json!({"cssVisualViewport":{"clientWidth":800.0,"clientHeight":500.0}}),
                801.0,
                10.0,
            ),
            None
        );
    }

    fn fixture_doc() -> Value {
        json!({
            "nodeType": 9,
            "nodeName": "#document",
            "frameId": "F_MAIN",
            "children": [{
                "nodeType": 1,
                "nodeName": "HTML",
                "backendNodeId": 1,
                "children": [
                    {
                        "nodeType": 1,
                        "nodeName": "BUTTON",
                        "backendNodeId": 10,
                        "attributes": ["aria-label", "Submit"],
                    },
                    {
                        "nodeType": 1,
                        "nodeName": "DIV",
                        "backendNodeId": 11,
                        "shadowRoots": [{
                            "nodeType": 11,
                            "nodeName": "#document-fragment",
                            "shadowRootType": "open",
                            "backendNodeId": 12,
                            "children": [{
                                "nodeType": 1,
                                "nodeName": "BUTTON",
                                "backendNodeId": 20,
                                "attributes": ["aria-label", "Shadow Go"],
                            }]
                        }]
                    },
                    {
                        "nodeType": 1,
                        "nodeName": "INPUT",
                        "backendNodeId": 21,
                        "attributes": ["type", "text"],
                        "shadowRoots": [{
                            "nodeType": 11,
                            "nodeName": "#document-fragment",
                            "shadowRootType": "user-agent",
                            "backendNodeId": 23,
                            "children": [{
                                "nodeType": 1,
                                "nodeName": "DIV",
                                "backendNodeId": 22,
                                "attributes": ["role", "button"],
                            }]
                        }]
                    },
                    {
                        "nodeType": 1,
                        "nodeName": "SPAN",
                        "backendNodeId": 24,
                    },
                    {
                        "nodeType": 1,
                        "nodeName": "IFRAME",
                        "backendNodeId": 13,
                        "frameId": "F_IFRAME",
                        "contentDocument": {
                            "nodeType": 9,
                            "nodeName": "#document",
                            "frameId": "F_IFRAME",
                            "children": [{
                                "nodeType": 1,
                                "nodeName": "BUTTON",
                                "backendNodeId": 30,
                                "attributes": ["id", "inner-btn"],
                            }]
                        }
                    },
                    {
                        "nodeType": 1,
                        "nodeName": "IFRAME",
                        "backendNodeId": 14,
                        "frameId": "F_OOPIF",
                    }
                ]
            }]
        })
    }

    #[test]
    fn collector_composes_shadow_dom_and_same_process_iframes() {
        let doc = fixture_doc();
        let mut out = Vec::new();
        collect_interactive(&doc, Some("F_MAIN"), true, &mut out);
        let backends: Vec<i64> = out.iter().map(|e| e.backend_node_id).collect();
        assert_eq!(
            backends,
            vec![10, 20, 21, 30],
            "main button + open-shadow button + input + same-process iframe button; \
             no span, no user-agent shadow content, no OOPIF placeholder descent"
        );
        assert_eq!(out[0].label.as_deref(), Some("aria-label=Submit"));

        let shadow = out.iter().find(|e| e.backend_node_id == 20).unwrap();
        assert!(
            shadow.in_root_frame,
            "shadow DOM composes into its host frame"
        );
        assert_eq!(shadow.frame_id.as_deref(), Some("F_MAIN"));

        let inner = out.iter().find(|e| e.backend_node_id == 30).unwrap();
        assert!(!inner.in_root_frame);
        assert_eq!(inner.frame_id.as_deref(), Some("F_IFRAME"));
    }

    #[test]
    fn parse_frame_tree_maps_frames_and_omits_malformed_children() {
        let tree = parse_frame_tree(&json!({
            "frameTree": {
                "frame": { "id": "F_MAIN", "loaderId": "L1", "url": "https://a.test/" },
                "childFrames": [
                    { "frame": { "id": "F_CHILD", "loaderId": "L2" } },
                    { "frame": { "id": "F_NO_LOADER" } }
                ]
            }
        }))
        .expect("valid tree");
        assert!(tree.proves(&FrameIdentity {
            frame_id: "F_MAIN".into(),
            loader_id: "L1".into()
        }));
        assert!(tree.proves(&FrameIdentity {
            frame_id: "F_CHILD".into(),
            loader_id: "L2".into()
        }));
        assert!(!tree.proves(&FrameIdentity {
            frame_id: "F_CHILD".into(),
            loader_id: "L9".into()
        }));
        assert!(
            tree.identity_of("F_NO_LOADER").is_none(),
            "a frame without a loader id is unprovable and omitted"
        );
        assert_eq!(tree.main_identity().frame_id, "F_MAIN");

        assert!(
            parse_frame_tree(&json!({ "frameTree": { "frame": { "id": "x" } } })).is_none(),
            "a root without a loader id fails the parse"
        );
        assert!(parse_frame_tree(&json!({})).is_none());
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn live_browser_origin_decision_refuses_redirect_outside_manifest_scope() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session-policy.yaml");
        std::fs::write(
            &path,
            r#"
version: 1
mode: bounded
expires_after: 1h
idle_timeout: 10m
resources:
  browser:
    origins: [https://app.example.com]
allow:
  tools: [browser_navigate, browser_click]
"#,
        )
        .unwrap();
        let manifest = crate::session_manifest::load_manifest(&path).unwrap();

        authorize_live_browser_origin(Some(&manifest), "https://app.example.com/work").unwrap();
        let refusal =
            authorize_live_browser_origin(Some(&manifest), "https://attacker.example/redirected")
                .unwrap_err();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserOriginOutsideScope);
        assert!(authorize_live_browser_origin(None, "https://app.example.com").is_err());
    }
}
