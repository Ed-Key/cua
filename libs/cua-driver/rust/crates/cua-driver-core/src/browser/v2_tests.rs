//! End-to-end tests for the v2 DOM-ref slice against a deterministic
//! mock CDP endpoint: composed shadow DOM, same-process iframes,
//! capability-tested OOPIFs with containment, frame/document identity
//! revalidation, navigation invalidation, and unproven-capability
//! omission/refusal.

use std::io::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex as StdMutex,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use serde_json::{json, Value};

use crate::action_record::ActionExecutionRecord;
use crate::protocol::{Content, ToolResult};
use crate::tool::Tool;

use super::engine::BrowserEngine;
use super::mock_cdp::{MockCdpServer, MockEvent, MockHandler, MockReply};
use super::platform::{
    BrowserConsentOutcome, BrowserConsentRequest, BrowserPlatform, ExistingProfileSetupOutcome,
    ExistingProfileSetupRequest, PrepareAction, PrepareOutcome, PrepareRequest,
};
use super::pointer::BrowserPointerTool;
use super::refusal::BrowserRefusal;
use super::tools::{
    browser_protected_resource_scope, BrowserClickTool, BrowserNavigateTool, BrowserPrepareTool,
    BrowserTypeTool, GetBrowserStateTool,
};
use super::types::{
    BrowserClassification, BrowserEngineFamily, BrowserProcessRole, BrowserProduct,
    EndpointOwnershipMethod, EndpointOwnershipProof, EndpointTransport, NativeOwnershipMethod,
    NativeOwnershipProof, NativeWindowInfo, OwnedEndpoint, ProcessFingerprint, Rect,
};

// ── Scripted Chromium fixture ────────────────────────────────────────────────

/// Mutable knobs + a call log, shared between the mock handler and the
/// test body. Tests flip loaders/capabilities mid-run to simulate
/// navigations, frame removal, and capability regression.
/// A page has one document.title, published both in the DOM snapshot and as
/// the main accessibility root's name. None makes it unavailable in both.
fn set_page_title(st: &mut FixtureState, title: Option<&str>) {
    st.omit_snapshot_title = title.is_none();
    st.semantic_title = title.map(str::to_owned);
    if let Some(title) = title {
        st.main_title = title.to_owned();
    }
}

#[derive(Debug)]
struct FixtureState {
    oopif_supported: bool,
    oopif_present: bool,
    emit_rogue_attach: bool,
    main_url: String,
    main_title: String,
    omit_snapshot_title: bool,
    main_loader: String,
    iframe_loader: String,
    oopif_loader: String,
    tab_sessions: u64,
    oopif_sessions: u64,
    fail_key_down_after: Option<usize>,
    completed_key_pairs: usize,
    semantic_large_page: bool,
    semantic_title: Option<String>,
    semantic_link_urls: bool,
    semantic_main_root_present: bool,
    semantic_full_dom_fails: bool,
    semantic_full_dom_times_out: bool,
    semantic_truncated_dom: bool,
    screenshot_data: String,
    viewport_css_width: f64,
    viewport_css_height: f64,
    tab_visible: bool,
    /// A simulated input field behind every editable ref: `None` keeps the
    /// fixture's plain `true` answers, so reads cannot be verified.
    field_value: Option<String>,
    /// The simulated field keeps digits only, like a controlled React input
    /// that rejects letters.
    field_digits_only: bool,
    /// The page replaces the field on input: reads report a detached node.
    field_detached_after_input: bool,
    /// The field's caret, reported as its selection; keystroke `char`
    /// events insert there. `None` reports no selection and ignores keys.
    field_caret: Option<usize>,
    /// A focus handler that moves the caret to the end of the field. In an
    /// inactive tab it runs only once focus is emulated.
    field_focus_moves_caret_to_end: bool,
    focus_emulated: bool,
    /// Accessible names the page changed since the fixture was built, by
    /// backend node id (a node reused for another entity).
    renamed: std::collections::HashMap<i64, String>,
    /// Nodes the page removed, by backend node id.
    removed: std::collections::HashSet<i64>,
    /// The debugger was detached from the tab (the user cancelled Chrome's
    /// banner): reported, as the relay does, before the next attach answers.
    detached: bool,
    /// What the page does when it is clicked: rename nodes, remove nodes.
    click_renames: Vec<(i64, String)>,
    click_removes: Vec<i64>,
    /// The click handler calls alert(): the dialog opens and the page
    /// answers nothing until it is resolved.
    click_opens_dialog: bool,
    /// The click handler sets location.href: the main frame starts loading
    /// the document with this loader id.
    click_navigates: Option<String>,
    /// What the page answers the next this-many hit-tests at a ref's click
    /// point (the facts), and the node that is on top there.
    hit: Option<(usize, Value, i64)>,
    /// The input handler calls alert() when text arrives.
    type_opens_dialog: bool,
    /// The input handler defers its alert: the insert is answered, and the
    /// dialog is up by the time the answer arrives.
    type_opens_dialog_late: bool,
    /// The alert opens while the field is being read back, at this read
    /// (0-based, counted from the insert), which is then "unanswered", fails
    /// ("error"), is "answered" all the same, or finds the field "detached".
    readback_opens_dialog: Option<(usize, &'static str)>,
    readbacks: usize,
    /// The browser does not report a frame tree.
    frame_tree_unsupported: bool,
    /// A keydown handler calls alert() on this (0-based) key.
    key_down_opens_dialog_at: Option<usize>,
    key_downs: usize,
    dialog_open: bool,
    /// The session that enabled the Page domain (it hears dialog events).
    page_session: Option<String>,
    /// Mutation records the page made since the settle counter last read.
    pending_mutations: u64,
    /// The tab's title as the browser lists it. The fixture's native window
    /// is titled "Fixture - Chrome", so any other title leaves the window's
    /// active tab unproven.
    tab_title: String,
    /// The user switches to another tab when a debugger attaches to this
    /// one (a bind lists tabs; its read is the first to attach).
    hide_tab_on_attach: bool,
    /// Every incoming CDP call: (sessionId, method, params).
    calls: Vec<(Option<String>, String, Value)>,
}

impl Default for FixtureState {
    fn default() -> Self {
        Self {
            oopif_supported: true,
            oopif_present: true,
            emit_rogue_attach: false,
            main_url: "https://fixture.test/".into(),
            main_title: "Current fixture title".into(),
            omit_snapshot_title: false,
            main_loader: "L_MAIN_1".into(),
            iframe_loader: "L_IFRAME_1".into(),
            oopif_loader: "L_OOPIF_1".into(),
            tab_sessions: 0,
            oopif_sessions: 0,
            fail_key_down_after: None,
            completed_key_pairs: 0,
            semantic_large_page: false,
            semantic_title: Some("Current fixture title".into()),
            semantic_link_urls: false,
            semantic_main_root_present: true,
            semantic_full_dom_fails: false,
            semantic_full_dom_times_out: false,
            semantic_truncated_dom: false,
            screenshot_data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9ZJrAAAAAASUVORK5CYII=".into(),
            viewport_css_width: 800.0,
            viewport_css_height: 600.0,
            tab_visible: true,
            field_value: None,
            field_digits_only: false,
            field_detached_after_input: false,
            field_caret: None,
            field_focus_moves_caret_to_end: false,
            focus_emulated: false,
            renamed: Default::default(),
            removed: Default::default(),
            detached: false,
            click_renames: Vec::new(),
            click_removes: Vec::new(),
            click_opens_dialog: false,
            click_navigates: None,
            hit: None,
            type_opens_dialog: false,
            type_opens_dialog_late: false,
            readback_opens_dialog: None,
            readbacks: 0,
            frame_tree_unsupported: false,
            key_down_opens_dialog_at: None,
            key_downs: 0,
            dialog_open: false,
            page_session: None,
            pending_mutations: 0,
            tab_title: "Fixture".into(),
            hide_tab_on_attach: false,
            calls: Vec::new(),
        }
    }
}

fn screenshot_png_base64(width: u32, height: u32) -> String {
    let image = RgbaImage::from_pixel(width, height, Rgba([18, 171, 52, 255]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode screenshot fixture PNG");
    BASE64.encode(encoded.into_inner())
}

type SharedState = Arc<StdMutex<FixtureState>>;

/// Main-frame document: a plain button, an open shadow root with an
/// input, a user-agent shadow root (must be skipped), a same-process
/// iframe with a button, and an OOPIF placeholder (no contentDocument).
fn main_document() -> Value {
    json!({
        "root": {
            "backendNodeId": 90000,
            "nodeType": 9,
            "nodeName": "#document",
            "documentURL": "https://fixture.test/",
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
                        "attributes": ["id", "main-btn"],
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
                                "nodeName": "INPUT",
                                "backendNodeId": 20,
                                "attributes": ["aria-label", "Shadow Input", "type", "text"],
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
        }
    })
}

/// Large application document used to prove that hidden retained controls do
/// not consume the semantic snapshot budget ahead of the active view.
fn large_semantic_document() -> Value {
    let mut children = Vec::new();
    for id in 0..320_i64 {
        children.push(json!({
            "nodeType": 1,
            "nodeName": "BUTTON",
            "backendNodeId": 1_000 + id,
            "attributes": [
                "aria-hidden", "true",
                "style", "display:none",
                "aria-label", format!("Retained control {id}"),
            ],
        }));
    }
    children.extend([
        json!({
            "nodeType": 1,
            "nodeName": "H1",
            "backendNodeId": 2_000,
            "children": [{
                "nodeType": 3,
                "nodeName": "#text",
                "nodeValue": "Visible message",
                "backendNodeId": 2_001,
            }],
        }),
        json!({
            "nodeType": 1,
            "nodeName": "P",
            "backendNodeId": 2_002,
            "children": [{
                "nodeType": 3,
                "nodeName": "#text",
                "nodeValue": "Please review the attached fixture report.",
                "backendNodeId": 2_003,
            }],
        }),
        json!({
            "nodeType": 1,
            "nodeName": "TEXTAREA",
            "backendNodeId": 2_010,
            "attributes": ["aria-label", "Reply body"],
        }),
        json!({
            "nodeType": 1,
            "nodeName": "BUTTON",
            "backendNodeId": 2_011,
            "attributes": ["aria-label", "Reply"],
        }),
    ]);
    for id in 0..305_i64 {
        children.push(json!({
            "nodeType": 1,
            "nodeName": "BUTTON",
            "backendNodeId": 3_000 + id,
            "attributes": ["aria-label", format!("Archive item {id}")],
        }));
    }
    json!({
        "root": {
            "backendNodeId": 90001,
            "nodeType": 9,
            "nodeName": "#document",
            "documentURL": "https://fixture.test/inbox/item-2",
            "frameId": "F_MAIN",
            "children": [{
                "nodeType": 1,
                "nodeName": "HTML",
                "backendNodeId": 999,
                "children": children,
            }]
        }
    })
}

fn truncated_semantic_document() -> Value {
    json!({
        "root": {
            "backendNodeId": 90001,
            "nodeType": 9,
            "nodeName": "#document",
            "documentURL": "https://fixture.test/inbox/item-2",
            "frameId": "F_MAIN",
            "children": [{
                "nodeType": 1,
                "nodeName": "HTML",
                "backendNodeId": 999,
                "childNodeCount": 629,
                "children": [],
            }]
        }
    })
}

fn large_semantic_ax_tree(frame_id: &str) -> Value {
    if frame_id != "F_MAIN" {
        return json!({"nodes": [{
            "nodeId": format!("root-{frame_id}"),
            "ignored": false,
            "role": {"value": "RootWebArea"},
            "name": {"value": "Inner fixture"},
            "childIds": []
        }]});
    }
    let mut child_ids = vec![
        "heading".to_owned(),
        "body".to_owned(),
        "editor".to_owned(),
        "reply".to_owned(),
    ];
    child_ids.extend((0..305).map(|id| format!("archive-{id}")));
    let mut nodes = vec![
        json!({
            "nodeId": "root-main",
            "ignored": false,
            "role": {"value": "RootWebArea"},
            "name": {"value": "Fixture inbox"},
            "childIds": child_ids
        }),
        json!({
            "nodeId": "heading",
            "parentId": "root-main",
            "ignored": false,
            "backendDOMNodeId": 2000,
            "role": {"value": "heading"},
            "name": {"value": "Visible message"},
            "childIds": []
        }),
        json!({
            "nodeId": "body",
            "parentId": "root-main",
            "ignored": false,
            "backendDOMNodeId": 2003,
            "role": {"value": "StaticText"},
            "name": {"value": "Please review the attached fixture report."},
            "childIds": []
        }),
        json!({
            "nodeId": "editor",
            "parentId": "root-main",
            "ignored": false,
            "backendDOMNodeId": 2010,
            "role": {"value": "textbox"},
            "name": {"value": "Reply body"},
            "value": {"value": ""},
            "properties": [
                {"name": "editable", "value": {"value": "plaintext"}},
                {"name": "focusable", "value": {"value": true}}
            ],
            "childIds": []
        }),
        json!({
            "nodeId": "reply",
            "parentId": "root-main",
            "ignored": false,
            "backendDOMNodeId": 2011,
            "role": {"value": "button"},
            "name": {"value": "Reply"},
            "properties": [
                {"name": "disabled", "value": {"value": false}},
                {"name": "focusable", "value": {"value": true}}
            ],
            "childIds": []
        }),
    ];
    for id in 0..305_i64 {
        nodes.push(json!({
            "nodeId": format!("archive-{id}"),
            "parentId": "root-main",
            "ignored": false,
            "backendDOMNodeId": 3_000 + id,
            "role": {"value": "button"},
            "name": {"value": format!("Archive item {id}")},
            "childIds": []
        }));
    }
    json!({"nodes": nodes})
}

/// The document as DOM.getDocument reports it: the fixture's current URL, so
/// the layout snapshot describes the same root document.
fn with_url(mut document: Value, url: &str) -> Value {
    document["root"]["documentURL"] = json!(url);
    document
}

fn semantic_layout_snapshot(
    backends: &[i64],
    bounds: &[[f64; 4]],
    document: &Value,
    title: &str,
) -> Value {
    let styles = (0..backends.len())
        .map(|_| json!([0, 1, 2, 3, 3]))
        .collect::<Vec<_>>();
    let node_indices = (1..=backends.len()).collect::<Vec<_>>();
    let paint_orders = (1..=backends.len()).collect::<Vec<_>>();
    let mut node_backends = vec![document["root"]["backendNodeId"].as_i64().unwrap()];
    node_backends.extend_from_slice(backends);
    json!({
        "strings": ["block", "visible", "1", "auto", "pointer",
            document["root"]["documentURL"], document["root"]["frameId"], title],
        "documents": [{
            "documentURL": 5,
            "frameId": 6,
            "title": 7,
            "nodes": {"backendNodeId": node_backends},
            "layout": {
                "nodeIndex": node_indices,
                "bounds": bounds,
                "styles": styles,
                "paintOrders": paint_orders
            }
        }]
    })
}

fn oopif_document() -> Value {
    json!({
        "root": {
            "backendNodeId": 90002,
            "nodeType": 9,
            "nodeName": "#document",
            "documentURL": "https://ads.example/frame",
            "frameId": "F_OOPIF",
            "children": [{
                "nodeType": 1,
                "nodeName": "HTML",
                "backendNodeId": 90,
                "children": [{
                    "nodeType": 1,
                    "nodeName": "INPUT",
                    "backendNodeId": 100,
                    "attributes": ["id", "ad-input", "type", "text"],
                }]
            }]
        }
    })
}

/// The DOM the fixture page has now: its document minus removed nodes.
fn fixture_dom(st: &FixtureState, is_oopif: bool) -> Value {
    fn prune(node: &mut Value, removed: &std::collections::HashSet<i64>) {
        for key in ["children", "shadowRoots"] {
            if let Some(children) = node.get_mut(key).and_then(Value::as_array_mut) {
                children.retain(|child| {
                    !child["backendNodeId"]
                        .as_i64()
                        .is_some_and(|backend| removed.contains(&backend))
                });
                children.iter_mut().for_each(|child| prune(child, removed));
            }
        }
        if let Some(content) = node.get_mut("contentDocument") {
            prune(content, removed);
        }
    }
    let mut document = if is_oopif {
        oopif_document()
    } else if st.semantic_large_page {
        large_semantic_document()
    } else {
        main_document()
    };
    if !is_oopif {
        document["root"]["documentURL"] = json!(st.main_url);
    }
    prune(&mut document["root"], &st.removed);
    document
}

fn find_dom_node(node: &Value, backend: i64) -> Option<Value> {
    let node = node.get("root").unwrap_or(node);
    if node["backendNodeId"].as_i64() == Some(backend) {
        return Some(node.clone());
    }
    ["children", "shadowRoots"]
        .iter()
        .filter_map(|key| node.get(*key).and_then(Value::as_array))
        .flatten()
        .chain(node.get("contentDocument"))
        .find_map(|child| find_dom_node(child, backend))
}

/// The accessibility tree the fixture page has now for one frame.
fn fixture_ax_tree(st: &FixtureState, is_oopif: bool, frame_id: &str) -> Value {
    let mut tree = if is_oopif {
        json!({"nodes": [
            {"nodeId": "oopif-root", "ignored": false,
             "role": {"value": "RootWebArea"}, "childIds": ["oopif-input"]},
            {"nodeId": "oopif-input", "parentId": "oopif-root", "ignored": false,
             "backendDOMNodeId": 100, "role": {"value": "textbox"},
             "name": {"value": "Embedded input"},
             "properties": [{"name": "editable", "value": {"value": "plaintext"}}],
             "childIds": []}
        ]})
    } else if st.semantic_large_page {
        let mut tree = large_semantic_ax_tree(frame_id);
        if frame_id == "F_MAIN" {
            let nodes = tree["nodes"].as_array_mut().unwrap();
            if !st.semantic_main_root_present {
                nodes.remove(0);
            } else if let Some(title) = &st.semantic_title {
                nodes[0]["name"] = json!({"value": title});
            } else {
                nodes[0].as_object_mut().unwrap().remove("name");
            }
        }
        if st.semantic_link_urls {
            for node in tree["nodes"].as_array_mut().unwrap() {
                if node["name"]["value"] == "Reply" || node["name"]["value"] == "Archive item 304" {
                    node["role"] = json!({"value":"link"});
                    node["properties"] = json!([{"name":"url","value":{"type":"string","value":"https://example.test/book?slot=1#court"}}]);
                }
            }
        }
        tree
    } else {
        json!({"nodes": []})
    };
    let nodes = tree["nodes"].as_array_mut().unwrap();
    nodes.retain(|node| {
        !node["backendDOMNodeId"]
            .as_i64()
            .is_some_and(|backend| st.removed.contains(&backend))
    });
    if let Some(value) = &st.field_value {
        for node in nodes
            .iter_mut()
            .filter(|node| node["backendDOMNodeId"] == 2010)
        {
            node["value"] = json!({ "value": value });
        }
    }
    for node in nodes {
        if let Some(name) = node["backendDOMNodeId"]
            .as_i64()
            .and_then(|backend| st.renamed.get(&backend))
        {
            node["name"] = json!({ "value": name });
        }
    }
    tree
}

fn fixture_handler(state: SharedState) -> MockHandler {
    Arc::new(move |call| {
        let mut st = state.lock().unwrap();
        st.calls.push((
            call.session_id.clone(),
            call.method.clone(),
            call.params.clone(),
        ));
        let sess = call.session_id.clone().unwrap_or_default();
        let is_tab = sess.starts_with("tab-sess-");
        let is_oopif = sess.starts_with("oopif-sess-");

        // The page's own click handler, run by either click route.
        let clicked = (call.method == "Input.dispatchMouseEvent"
            && call.params["type"] == "mouseReleased")
            || (call.method == "Runtime.callFunctionOn"
                && call.params["functionDeclaration"]
                    .as_str()
                    .is_some_and(|function| function.contains("this.click()")));
        if clicked {
            let renames = std::mem::take(&mut st.click_renames);
            let removes = std::mem::take(&mut st.click_removes);
            st.pending_mutations += (renames.len() + removes.len()) as u64;
            st.renamed.extend(renames);
            st.removed.extend(removes);
            if let Some(loader) = st.click_navigates.take() {
                st.main_loader = loader;
                return MockReply::ok(json!({})).with_events(vec![MockEvent {
                    method: "Page.frameStartedLoading".into(),
                    session_id: st.page_session.clone(),
                    params: json!({"frameId": "F_MAIN"}),
                }]);
            }
            if std::mem::take(&mut st.click_opens_dialog) {
                st.dialog_open = true;
                return MockReply::ok(json!({}))
                    .with_events(vec![MockEvent {
                        method: "Page.javascriptDialogOpening".into(),
                        session_id: st.page_session.clone(),
                        params: json!({"type": "alert", "message": "private dialog text"}),
                    }])
                    .unanswered();
            }
        }
        // A page behind a dialog answers only what the browser process does.
        if st.dialog_open
            && (is_tab || is_oopif)
            && !matches!(
                call.method.as_str(),
                "Page.handleJavaScriptDialog" | "Page.enable"
            )
        {
            return MockReply::ok(json!({})).unanswered();
        }

        match call.method.as_str() {
            "Page.enable" if is_tab => {
                st.page_session = Some(sess.clone());
                MockReply::ok(json!({}))
            }
            "Page.handleJavaScriptDialog" if st.dialog_open => {
                st.dialog_open = false;
                MockReply::ok(json!({})).with_events(vec![MockEvent {
                    method: "Page.javascriptDialogClosed".into(),
                    session_id: st.page_session.clone(),
                    params: json!({"result": true, "userInput": ""}),
                }])
            }
            "Runtime.evaluate"
                if call.params["expression"]
                    .as_str()
                    .is_some_and(|expression| expression.contains("MutationObserver")) =>
            {
                MockReply::ok(json!({"result": {"type": "object", "objectId": "settle-counter"}}))
            }
            "Runtime.evaluate" if call.params["expression"] == "document.readyState" => {
                MockReply::ok(json!({"result": {"type": "string", "value": "complete"}}))
            }
            "Runtime.callFunctionOn" if call.params["objectId"] == "settle-counter" => {
                let records = std::mem::take(&mut st.pending_mutations);
                MockReply::ok(json!({"result": {"type": "number", "value": records}}))
            }
            "Target.getTargets" => MockReply::ok(json!({
                "targetInfos": [{
                    "targetId": "T1",
                    "type": "page",
                    "title": st.tab_title.clone(),
                    "url": "https://fixture.test/",
                    "attached": false,
                }]
            })),
            "Browser.getWindowForTarget" => MockReply::ok(json!({ "windowId": 11 })),
            "Browser.getWindowBounds" => MockReply::ok(json!({
                "bounds": { "left": 0.0, "top": 0.0, "width": 800.0, "height": 600.0 }
            })),
            "Target.attachToTarget" => {
                let mut events = Vec::new();
                if std::mem::take(&mut st.detached) {
                    events.push(MockEvent {
                        method: "Target.detachedFromTarget".into(),
                        session_id: None,
                        params: json!({
                            "sessionId": format!("tab-sess-{}", st.tab_sessions),
                            "targetId": "T1",
                            "reason": "canceled_by_user",
                        }),
                    });
                }
                st.tab_sessions += 1;
                if st.hide_tab_on_attach {
                    st.tab_visible = false;
                }
                MockReply::ok(json!({ "sessionId": format!("tab-sess-{}", st.tab_sessions) }))
                    .with_events(events)
            }
            "Page.getFrameTree" if st.frame_tree_unsupported => {
                MockReply::err(-32601, "'Page.getFrameTree' wasn't found")
            }
            "Page.getFrameTree" if is_tab => MockReply::ok(json!({
                "frameTree": {
                    "frame": {
                        "id": "F_MAIN",
                        "loaderId": st.main_loader.clone(),
                        "url": st.main_url.clone(),
                    },
                    "childFrames": [{
                        "frame": {
                            "id": "F_IFRAME",
                            "parentId": "F_MAIN",
                            "loaderId": st.iframe_loader.clone(),
                            "url": "https://fixture.test/inner",
                        }
                    }]
                }
            })),
            "Page.getFrameTree" if is_oopif => MockReply::ok(json!({
                "frameTree": {
                    "frame": {
                        "id": "F_OOPIF",
                        "loaderId": st.oopif_loader.clone(),
                        "url": "https://ads.example/frame",
                    }
                }
            })),
            "DOM.getDocument" if is_tab => {
                let depth = call.params["depth"].as_i64().unwrap_or(-1);
                if st.semantic_full_dom_times_out && (depth == -1 || depth > 8) {
                    MockReply::err(-32000, "CDP DOM.getDocument timed out after 20s")
                } else if st.semantic_full_dom_fails && (depth == -1 || depth > 8) {
                    MockReply::err(-32000, "Object reference chain is too long")
                } else if st.semantic_truncated_dom && depth == 8 {
                    MockReply::ok(truncated_semantic_document())
                } else {
                    MockReply::ok(fixture_dom(&st, false))
                }
            }
            "DOM.describeNode" if is_tab && call.params["backendNodeId"] == 999 => {
                MockReply::ok(json!({
                    "node": large_semantic_document()["root"]["children"][0].clone()
                }))
            }
            "DOM.describeNode" if call.params["objectId"] == "obj-on-top" => {
                let on_top = st.hit.as_ref().map_or(0, |(_, _, backend)| *backend);
                MockReply::ok(json!({"node": {"backendNodeId": on_top}}))
            }
            "DOM.describeNode" => {
                let backend = call.params["backendNodeId"].as_i64().unwrap_or(0);
                match find_dom_node(&fixture_dom(&st, is_oopif), backend) {
                    Some(node) => MockReply::ok(json!({ "node": node })),
                    None => MockReply::err(-32000, "No node with given id"),
                }
            }
            "DOM.getDocument" if is_oopif => MockReply::ok(oopif_document()),
            "Accessibility.getFullAXTree" if is_tab || is_oopif => {
                let frame_id = call.params["frameId"].as_str().unwrap_or("F_MAIN");
                MockReply::ok(fixture_ax_tree(&st, is_oopif, frame_id))
            }
            "Accessibility.getPartialAXTree" if is_tab || is_oopif => {
                let backend = call.params["backendNodeId"].as_i64();
                let node = ["F_MAIN", "F_IFRAME"]
                    .iter()
                    .flat_map(|frame_id| {
                        fixture_ax_tree(&st, is_oopif, frame_id)["nodes"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                    })
                    .find(|node| node["backendDOMNodeId"].as_i64() == backend);
                match node {
                    Some(node) => MockReply::ok(json!({ "nodes": [node] })),
                    // Chromium answers for any live DOM node; one with no
                    // accessibility object of its own comes back ignored.
                    None if find_dom_node(&fixture_dom(&st, is_oopif), backend.unwrap_or(0))
                        .is_some() =>
                    {
                        MockReply::ok(json!({ "nodes": [{
                            "nodeId": "ignored", "ignored": true, "backendDOMNodeId": backend,
                        }] }))
                    }
                    None => MockReply::err(-32000, "No node with given id"),
                }
            }
            "DOMSnapshot.captureSnapshot" if is_tab => {
                let mut snapshot = if st.semantic_large_page {
                    let mut backends = vec![999, 2000, 2003, 2010, 2011];
                    let mut bounds = vec![
                        [0.0, 0.0, 800.0, 600.0],
                        [20.0, 20.0, 500.0, 40.0],
                        [20.0, 80.0, 600.0, 80.0],
                        [20.0, 180.0, 600.0, 120.0],
                        [20.0, 320.0, 100.0, 36.0],
                    ];
                    for id in 0..305_i64 {
                        backends.push(3_000 + id);
                        bounds.push([20.0, 2_000.0 + id as f64 * 40.0, 160.0, 30.0]);
                    }
                    semantic_layout_snapshot(
                        &backends,
                        &bounds,
                        &with_url(large_semantic_document(), &st.main_url),
                        &st.main_title,
                    )
                } else {
                    semantic_layout_snapshot(
                        &[],
                        &[],
                        &with_url(main_document(), &st.main_url),
                        &st.main_title,
                    )
                };
                if st.omit_snapshot_title {
                    snapshot["documents"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("title");
                }
                MockReply::ok(snapshot)
            }
            "DOMSnapshot.captureSnapshot" if is_oopif => MockReply::ok(semantic_layout_snapshot(
                &[90, 100],
                &[[0.0, 0.0, 300.0, 100.0], [10.0, 10.0, 120.0, 30.0]],
                &oopif_document(),
                "Embedded frame title",
            )),
            "Page.getLayoutMetrics" => MockReply::ok(json!({
                "cssVisualViewport": {
                    "pageX": 0.0,
                    "pageY": 0.0,
                    "clientWidth": st.viewport_css_width,
                    "clientHeight": st.viewport_css_height
                }
            })),
            "Runtime.evaluate"
                if call.params["expression"] == "document.visibilityState === 'visible'" =>
            {
                MockReply::ok(json!({
                    "result": {
                        "type": "boolean",
                        "value": st.tab_visible
                    }
                }))
            }
            "Page.captureScreenshot" if is_tab => {
                MockReply::ok(json!({"data": st.screenshot_data.clone()}))
            }
            "Page.navigate" if is_tab => MockReply::ok(json!({
                "frameId": "F_MAIN",
                "loaderId": "L_MAIN_NAVIGATED",
            })),
            "Target.setAutoAttach" if is_tab => {
                if call.params["autoAttach"].as_bool() == Some(false) {
                    return MockReply::ok(json!({}));
                }
                if !st.oopif_supported {
                    return MockReply::method_not_found("Target.setAutoAttach");
                }
                let mut events = Vec::new();
                if st.oopif_present {
                    st.oopif_sessions += 1;
                    events.push(MockEvent {
                        method: "Target.attachedToTarget".into(),
                        session_id: Some(sess.clone()),
                        params: json!({
                            "sessionId": format!("oopif-sess-{}", st.oopif_sessions),
                            "targetInfo": {
                                "targetId": "T_OOPIF",
                                "type": "iframe",
                                "url": "https://ads.example/frame",
                                "attached": true,
                            },
                            "waitingForDebugger": false,
                        }),
                    });
                }
                if st.emit_rogue_attach {
                    // A child announced on a session we never proved.
                    events.push(MockEvent {
                        method: "Target.attachedToTarget".into(),
                        session_id: Some("unproven-sess".into()),
                        params: json!({
                            "sessionId": "rogue-sess",
                            "targetInfo": {
                                "targetId": "T_ROGUE",
                                "type": "iframe",
                                "url": "https://evil.example/",
                                "attached": true,
                            },
                            "waitingForDebugger": false,
                        }),
                    });
                    // A popup page target beneath the tab — wrong type.
                    events.push(MockEvent {
                        method: "Target.attachedToTarget".into(),
                        session_id: Some(sess.clone()),
                        params: json!({
                            "sessionId": "popup-sess",
                            "targetInfo": {
                                "targetId": "T_POPUP",
                                "type": "page",
                                "url": "https://fixture.test/popup",
                                "attached": true,
                            },
                            "waitingForDebugger": false,
                        }),
                    });
                }
                MockReply::ok(json!({})).with_events(events)
            }
            "DOM.scrollIntoViewIfNeeded" => MockReply::ok(json!({})),
            "DOM.getBoxModel" => {
                let backend = call.params["backendNodeId"].as_i64().unwrap_or(0);
                let known = if is_oopif {
                    backend == 100
                } else {
                    [10, 20, 21, 30].contains(&backend)
                };
                if known {
                    let (x, y) = ((backend * 10) as f64, (backend * 10) as f64);
                    MockReply::ok(json!({
                        "model": {
                            "content": [x, y, x + 20.0, y, x + 20.0, y + 10.0, x, y + 10.0],
                            "border": [x - 1.0, y - 1.0, x + 21.0, y - 1.0, x + 21.0, y + 11.0, x - 1.0, y + 11.0]
                        }
                    }))
                } else {
                    MockReply::err(-32000, "No node with given id")
                }
            }
            "Input.dispatchKeyEvent"
                if call.params["type"] == "keyDown" && {
                    st.key_downs += 1;
                    st.key_down_opens_dialog_at == Some(st.key_downs - 1)
                } =>
            {
                st.dialog_open = true;
                MockReply::ok(json!({}))
                    .with_events(vec![MockEvent {
                        method: "Page.javascriptDialogOpening".into(),
                        session_id: st.page_session.clone(),
                        params: json!({"type": "alert", "message": "private dialog text"}),
                    }])
                    .unanswered()
            }
            "Input.dispatchKeyEvent" => {
                let event_type = call.params["type"].as_str().unwrap_or_default();
                if event_type == "keyDown" && st.fail_key_down_after == Some(st.completed_key_pairs)
                {
                    MockReply::err(-32000, "fixture key delivery failure")
                } else {
                    if event_type == "keyUp" {
                        st.completed_key_pairs += 1;
                    }
                    if event_type == "char" {
                        let text = call.params["text"].as_str().unwrap_or_default().to_owned();
                        if let Some(caret) = st.field_caret {
                            if let Some(value) = st.field_value.as_mut() {
                                value.insert_str(caret, &text);
                                st.field_caret = Some(caret + text.len());
                            }
                        }
                    }
                    MockReply::ok(json!({}))
                }
            }
            "Input.insertText" if std::mem::take(&mut st.type_opens_dialog_late) => {
                st.dialog_open = true;
                if let Some(value) = st.field_value.as_mut() {
                    value.push_str(call.params["text"].as_str().unwrap_or_default());
                }
                MockReply::ok(json!({})).with_events(vec![MockEvent {
                    method: "Page.javascriptDialogOpening".into(),
                    session_id: st.page_session.clone(),
                    params: json!({"type": "alert", "message": "private dialog text"}),
                }])
            }
            "Input.insertText" if std::mem::take(&mut st.type_opens_dialog) => {
                st.dialog_open = true;
                MockReply::ok(json!({}))
                    .with_events(vec![MockEvent {
                        method: "Page.javascriptDialogOpening".into(),
                        session_id: st.page_session.clone(),
                        params: json!({"type": "confirm", "message": "private dialog text"}),
                    }])
                    .unanswered()
            }
            "Input.insertText" => {
                st.pending_mutations += 1;
                let digits_only = st.field_digits_only;
                if let Some(value) = st.field_value.as_mut() {
                    value.extend(
                        call.params["text"]
                            .as_str()
                            .unwrap_or_default()
                            .chars()
                            .filter(|ch| !digits_only || ch.is_ascii_digit()),
                    );
                }
                MockReply::ok(json!({}))
            }
            "DOM.focus" | "Emulation.setFocusEmulationEnabled" | "Input.dispatchMouseEvent" => {
                if call.method == "Emulation.setFocusEmulationEnabled" {
                    st.focus_emulated = call.params["enabled"].as_bool().unwrap_or(false);
                }
                if call.method == "DOM.focus"
                    && st.field_focus_moves_caret_to_end
                    && st.focus_emulated
                {
                    let end = st.field_value.as_ref().map(String::len);
                    if st.field_caret.is_some() {
                        st.field_caret = end;
                    }
                }
                MockReply::ok(json!({}))
            }
            "DOM.resolveNode" => MockReply::ok(json!({
                "object": { "objectId": format!("obj-{}", call.params["backendNodeId"]) }
            })),
            "Runtime.callFunctionOn"
                if call.params["functionDeclaration"]
                    .as_str()
                    .is_some_and(|function| function.contains("elementFromPoint")) =>
            {
                // The second form asks for the element on top itself.
                if call.params["arguments"][4]["value"] == true {
                    return MockReply::ok(
                        json!({"result": {"type": "object", "objectId": "obj-on-top"}}),
                    );
                }
                let scripted = match st.hit.as_mut() {
                    Some((remaining, facts, _)) if *remaining > 0 => {
                        *remaining -= 1;
                        Some(facts.clone())
                    }
                    _ => None,
                };
                MockReply::ok(
                    json!({"result": {"value": scripted.unwrap_or_else(|| json!({
                        "connected": true, "hit": true, "inside_target": true,
                        "contains_target": false, "label_of_target": false, "own_indicator": false,
                    }))}}),
                )
            }
            "Runtime.callFunctionOn"
                if st.readback_opens_dialog.is_some()
                    && st
                        .calls
                        .iter()
                        .any(|(_, method, _)| method == "Input.insertText")
                    && call.params["functionDeclaration"]
                        .as_str()
                        .is_some_and(|function| function.contains("selectionStart"))
                    && {
                        st.readbacks += 1;
                        st.readback_opens_dialog.map(|(at, _)| at) == Some(st.readbacks - 1)
                    } =>
            {
                st.dialog_open = true;
                let reply = match st.readback_opens_dialog.unwrap().1 {
                    "unanswered" => MockReply::ok(json!({})).unanswered(),
                    "error" => MockReply::err(-32000, "fixture read failure"),
                    read => MockReply::ok(json!({
                        "result": { "value": {
                            "value": st.field_value.clone(), "start": null, "end": null,
                            "field": true, "password": false,
                            "connected": read != "detached",
                        } }
                    })),
                };
                reply.with_events(vec![MockEvent {
                    method: "Page.javascriptDialogOpening".into(),
                    session_id: st.page_session.clone(),
                    params: json!({"type": "alert", "message": "private dialog text"}),
                }])
            }
            "Runtime.callFunctionOn" => {
                let function = call.params["functionDeclaration"]
                    .as_str()
                    .unwrap_or_default();
                let digits_only = st.field_digits_only;
                let typed = st
                    .calls
                    .iter()
                    .any(|(_, method, _)| method == "Input.insertText");
                let connected = !(st.field_detached_after_input && typed);
                let caret = st.field_caret;
                match st.field_value.as_mut() {
                    Some(value) if function.contains("selectionStart") => MockReply::ok(json!({
                        "result": { "value": {
                            "value": value.clone(), "start": caret, "end": caret,
                            "field": true, "password": false, "connected": connected,
                        } }
                    })),
                    Some(value) if function.contains("getOwnPropertyDescriptor") => {
                        *value = call.params["arguments"][0]["value"]
                            .as_str()
                            .unwrap_or_default()
                            .chars()
                            .filter(|ch| !digits_only || ch.is_ascii_digit())
                            .collect();
                        MockReply::ok(json!({ "result": { "value": true } }))
                    }
                    _ => MockReply::ok(json!({ "result": { "value": true } })),
                }
            }
            other => MockReply::method_not_found(other),
        }
    })
}

/// Platform adapter pointing at the fixture endpoint: pid 1 is a
/// Chromium browser owning native window 7 at (0,0,800,600).
struct FixturePlatform {
    ws_url: String,
    trusted_input_limited: bool,
    managed_endpoint_visible: bool,
    process_role: BrowserProcessRole,
    managed_discovery_invoked: Arc<AtomicBool>,
    existing_endpoint_visible: Arc<AtomicBool>,
    setup_invoked: Arc<AtomicBool>,
    setup_aborted: Arc<AtomicBool>,
    stall_consent: bool,
    existing_transport: EndpointTransport,
    /// Scripted results for successive existing-profile discoveries (`None`
    /// = no endpoint); once empty, discovery uses `existing_transport`.
    route_script: Arc<StdMutex<std::collections::VecDeque<Option<EndpointTransport>>>>,
}

#[async_trait]
impl BrowserPlatform for FixturePlatform {
    fn standalone_trusted_input_background_limitation(&self) -> Option<&'static str> {
        self.trusted_input_limited
            .then_some("fixture trusted input raises the standalone window")
    }

    /// "Fixture Browser" has one window (pid 1, window 7); "Two Windows" has
    /// two. Anything else is not running.
    async fn resolve_app_window(&self, app: &str) -> Result<(i64, u64), ToolResult> {
        match app {
            "Fixture Browser" => Ok((1, 7)),
            "Two Windows" => Err(ToolResult::error(
                "\"Two Windows\" has 2 windows on the current Space; pass pid + window_id for one:\n\
                 window_id 7 (pid 1): Inbox\nwindow_id 8 (pid 1): Docs",
            )
            .with_structured(json!({
                "code": "app_window_ambiguous",
                "app": app,
                "candidates": [
                    { "window_id": 7, "pid": 1, "title": "Inbox" },
                    { "window_id": 8, "pid": 1, "title": "Docs" },
                ],
                "suggestion": "pass pid + window_id for one of the candidates",
            }))),
            _ => Err(ToolResult::error(format!("no running app named \"{app}\""))
                .with_structured(json!({ "code": "app_not_running", "app": app }))),
        }
    }

    async fn classify_browser(&self, _pid: i64) -> Result<BrowserClassification, BrowserRefusal> {
        Ok(BrowserClassification {
            is_browser: true,
            engine: BrowserEngineFamily::Chromium,
            product_kind: BrowserProduct::GoogleChrome,
            product: Some("MockChrome".into()),
            channel: Some("stable".into()),
            // The mock endpoint is an explicit in-process harness, not a
            // personal standalone browser profile.
            process_role: self.process_role,
            supports_cdp: true,
        })
    }

    async fn native_window(
        &self,
        pid: i64,
        window_id: u64,
    ) -> Result<NativeWindowInfo, BrowserRefusal> {
        Ok(NativeWindowInfo {
            pid,
            window_id,
            title: "Fixture - Chrome".into(),
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
        pid: i64,
    ) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
        self.managed_discovery_invoked.store(true, Ordering::SeqCst);
        if !self.managed_endpoint_visible {
            return Ok(None);
        }
        self.discover_existing_profile_endpoint(pid).await
    }

    async fn extension_link_connected(&self, _pid: i64) -> bool {
        self.existing_endpoint_visible.load(Ordering::SeqCst)
            && self.existing_transport == EndpointTransport::ExtensionRelay
    }

    async fn discover_existing_profile_endpoint(
        &self,
        pid: i64,
    ) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
        if !self.existing_endpoint_visible.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let transport = match self.route_script.lock().unwrap().pop_front() {
            Some(None) => return Ok(None),
            Some(Some(transport)) => transport,
            None => self.existing_transport,
        };
        Ok(Some(OwnedEndpoint {
            ws_url: self.ws_url.clone(),
            http_port: None,
            transport,
            ownership: EndpointOwnershipProof {
                method: EndpointOwnershipMethod::ListeningSocketPid,
                owner_pid: pid,
                listener_pid: None,
                detail: None,
            },
        }))
    }

    async fn reprove_existing_profile_endpoint(
        &self,
        pid: i64,
        expected_ws_url: &str,
    ) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
        if expected_ws_url != self.ws_url {
            return Ok(None);
        }
        Ok(Some(OwnedEndpoint {
            ws_url: self.ws_url.clone(),
            http_port: None,
            transport: self.existing_transport,
            ownership: EndpointOwnershipProof {
                method: EndpointOwnershipMethod::ListeningSocketPid,
                owner_pid: pid,
                listener_pid: None,
                detail: Some("fixture exact approved endpoint".to_owned()),
            },
        }))
    }

    async fn setup_existing_profile_endpoint(
        &self,
        _request: ExistingProfileSetupRequest,
    ) -> Result<ExistingProfileSetupOutcome, BrowserRefusal> {
        self.setup_invoked.store(true, Ordering::SeqCst);
        Ok(ExistingProfileSetupOutcome {
            opened_setup_page: true,
            closed_setup_page: false,
            enabled_remote_debugging: true,
            used_bounded_pixel_fallback: false,
            focused_setup_address_field: true,
            foregrounded_window: false,
            injected_global_input: false,
            endpoint: Some(OwnedEndpoint {
                ws_url: self.ws_url.clone(),
                http_port: None,
                transport: EndpointTransport::DevToolsActivePort,
                ownership: EndpointOwnershipProof {
                    method: EndpointOwnershipMethod::ListeningSocketPid,
                    owner_pid: 1,
                    listener_pid: None,
                    detail: Some("fixture exact setup transition".to_owned()),
                },
            }),
        })
    }

    async fn commit_existing_profile_setup(
        &self,
        _request: ExistingProfileSetupRequest,
    ) -> Result<bool, BrowserRefusal> {
        Ok(true)
    }

    async fn abort_existing_profile_setup(
        &self,
        _request: ExistingProfileSetupRequest,
        error: BrowserRefusal,
    ) -> BrowserRefusal {
        self.setup_aborted.store(true, Ordering::SeqCst);
        error
    }

    async fn handle_existing_profile_consent(
        &self,
        _request: BrowserConsentRequest,
    ) -> Result<BrowserConsentOutcome, BrowserRefusal> {
        if self.stall_consent {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            return Ok(BrowserConsentOutcome::NotPresent);
        }
        Err(BrowserRefusal::new(
            super::refusal::BrowserRefusalCode::BrowserRouteUnavailable,
            "fixture consent prompt is unavailable",
        ))
    }

    async fn process_fingerprint(&self, pid: i64) -> Result<ProcessFingerprint, BrowserRefusal> {
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
        Ok(PrepareOutcome {
            action: PrepareAction::NoOp,
            endpoint: None,
            message: "fixture: nothing to do".into(),
            prepared_pid: None,
            side_effects: Default::default(),
            attachment: None,
        })
    }
}

// ── Scaffolding ──────────────────────────────────────────────────────────────

struct Fixture {
    state: SharedState,
    // Kept alive for the test's duration; dropping it kills the endpoint.
    _server: MockCdpServer,
    engine: Arc<BrowserEngine>,
    setup_invoked: Arc<AtomicBool>,
}

async fn fixture_with(configure: impl FnOnce(&mut FixtureState)) -> Fixture {
    fixture_with_platform(configure, false).await
}

async fn fixture_with_platform(
    configure: impl FnOnce(&mut FixtureState),
    trusted_input_limited: bool,
) -> Fixture {
    let mut initial = FixtureState::default();
    configure(&mut initial);
    let state = Arc::new(StdMutex::new(initial));
    let server = MockCdpServer::start(fixture_handler(state.clone())).await;
    let setup_invoked = Arc::new(AtomicBool::new(false));
    let engine = BrowserEngine::new(Arc::new(FixturePlatform {
        ws_url: server.ws_url(),
        trusted_input_limited,
        managed_endpoint_visible: true,
        process_role: BrowserProcessRole::EmbeddedApplication,
        managed_discovery_invoked: Arc::new(AtomicBool::new(false)),
        existing_endpoint_visible: Arc::new(AtomicBool::new(true)),
        setup_invoked: setup_invoked.clone(),
        setup_aborted: Arc::new(AtomicBool::new(false)),
        stall_consent: false,
        existing_transport: EndpointTransport::LegacyJsonVersion,
        route_script: Default::default(),
    }));
    Fixture {
        state,
        _server: server,
        engine,
        setup_invoked,
    }
}

async fn fixture() -> Fixture {
    fixture_with(|_| {}).await
}

struct FixtureProtectedProvider {
    consent_seen: AtomicBool,
}

#[async_trait]
impl crate::consent::ProtectedConsentProvider for FixtureProtectedProvider {
    fn provider_id(&self) -> &'static str {
        "test.browser-protected-provider"
    }

    async fn request_consent(
        &self,
        request: &crate::consent::ConsentRequest,
    ) -> Result<crate::consent::ProviderDecision, String> {
        self.consent_seen.store(true, Ordering::SeqCst);
        Ok(crate::consent::ProviderDecision {
            action: crate::consent::ConsentAction::Accept,
            request_digest: request.request_digest.clone(),
        })
    }
}

async fn protected_existing_profile_fixture() -> (Fixture, Arc<FixtureProtectedProvider>) {
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state.clone())).await;
    let setup_invoked = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(FixtureProtectedProvider {
        consent_seen: AtomicBool::new(false),
    });
    let engine = BrowserEngine::new_with_protected_consent_provider(
        Arc::new(FixturePlatform {
            ws_url: server.ws_url(),
            trusted_input_limited: false,
            managed_endpoint_visible: false,
            process_role: BrowserProcessRole::StandaloneConsumer,
            managed_discovery_invoked: Arc::new(AtomicBool::new(false)),
            existing_endpoint_visible: Arc::new(AtomicBool::new(true)),
            setup_invoked: setup_invoked.clone(),
            setup_aborted: Arc::new(AtomicBool::new(false)),
            stall_consent: false,
            existing_transport: EndpointTransport::LegacyJsonVersion,
            route_script: Default::default(),
        }),
        Some(provider.clone()),
    );
    (
        Fixture {
            state,
            _server: server,
            engine,
            setup_invoked,
        },
        provider,
    )
}

async fn existing_profile_setup_fixture() -> (Fixture, Arc<AtomicBool>) {
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state.clone())).await;
    let setup_invoked = Arc::new(AtomicBool::new(false));
    let engine = BrowserEngine::new_with_protected_consent_provider(
        Arc::new(FixturePlatform {
            ws_url: server.ws_url(),
            trusted_input_limited: false,
            managed_endpoint_visible: false,
            process_role: BrowserProcessRole::StandaloneConsumer,
            managed_discovery_invoked: Arc::new(AtomicBool::new(false)),
            existing_endpoint_visible: Arc::new(AtomicBool::new(false)),
            setup_invoked: setup_invoked.clone(),
            setup_aborted: Arc::new(AtomicBool::new(false)),
            stall_consent: false,
            existing_transport: EndpointTransport::LegacyJsonVersion,
            route_script: Default::default(),
        }),
        Some(Arc::new(FixtureProtectedProvider {
            consent_seen: AtomicBool::new(false),
        })),
    );
    (
        Fixture {
            state,
            _server: server,
            engine,
            setup_invoked: setup_invoked.clone(),
        },
        setup_invoked,
    )
}

fn structured(result: &ToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("structured content")
}

const SESSION: &str = "run-v2";

async fn bind(f: &Fixture) -> (String, String) {
    let tool = GetBrowserStateTool::new(f.engine.clone());
    let result = tool
        .invoke(json!({ "pid": 1, "window_id": 7, "session": SESSION }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "ok", "bind must succeed: {s}");
    assert_eq!(s["binding_quality"], "exact");
    let target_id = s["target_id"].as_str().unwrap().to_owned();
    let tab_id = s["tabs"][0]["tab_id"].as_str().unwrap().to_owned();
    (target_id, tab_id)
}

#[tokio::test]
async fn standalone_consumer_bind_without_grant_refuses_before_endpoint_discovery() {
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state)).await;
    let managed_discovery_invoked = Arc::new(AtomicBool::new(false));
    let setup_invoked = Arc::new(AtomicBool::new(false));
    let engine = BrowserEngine::new(Arc::new(FixturePlatform {
        ws_url: server.ws_url(),
        trusted_input_limited: false,
        managed_endpoint_visible: true,
        process_role: BrowserProcessRole::StandaloneConsumer,
        managed_discovery_invoked: managed_discovery_invoked.clone(),
        existing_endpoint_visible: Arc::new(AtomicBool::new(true)),
        setup_invoked: setup_invoked.clone(),
        setup_aborted: Arc::new(AtomicBool::new(false)),
        stall_consent: false,
        existing_transport: EndpointTransport::LegacyJsonVersion,
        route_script: Default::default(),
    }));

    let result = GetBrowserStateTool::new(engine)
        .invoke(json!({ "pid": 1, "window_id": 7, "session": SESSION }))
        .await;
    let refusal = structured(&result);
    assert_eq!(refusal["status"], "refused");
    assert_eq!(refusal["refusal"]["code"], "browser_consent_required");
    assert_eq!(
        refusal["refusal"]["detail"]["reason"],
        "consumer_profile_endpoint_requires_grant"
    );
    assert_eq!(
        refusal["refusal"]["detail"]["next_action"],
        "browser_prepare"
    );
    // The exact call, so an agent never assembles it by guessing.
    assert_eq!(
        refusal["refusal"]["detail"]["next_call"],
        json!({
            "tool": "browser_prepare",
            "arguments": { "pid": 1, "window_id": 7, "strategy": { "kind": "existing_profile" } }
        })
    );
    assert_eq!(refusal["refusal"]["detail"]["extension_connected"], false);
    assert!(refusal["refusal"]["message"]
        .as_str()
        .unwrap()
        .contains("changes no browser settings"));
    assert!(
        !managed_discovery_invoked.load(Ordering::SeqCst),
        "read-only bind must not inspect a consent-gated endpoint"
    );
    assert!(
        !setup_invoked.load(Ordering::SeqCst),
        "read-only bind must never invoke browser setup UI"
    );
}

#[tokio::test]
async fn approved_existing_profile_attach_claims_then_binds_one_generation() {
    // Match Chrome's per-instance toggle: the endpoint is discoverable only
    // through the approved existing-profile route, never as driver-managed.
    let (f, provider) = protected_existing_profile_fixture().await;
    let prepare = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": "transport-v2-attach",
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    let prepared = structured(&prepare);
    assert_eq!(prepared["status"], "ok", "{prepared}");
    assert_eq!(prepared["action"], "attached_existing_profile");
    assert_eq!(prepared["attachment"]["kind"], "existing_profile");
    assert_eq!(prepared["attachment"]["capabilities_invalidated"], true);
    assert_eq!(prepared["side_effects"]["displayed_consent_prompt"], false);
    assert!(provider.consent_seen.load(Ordering::SeqCst));
    assert!(!f.setup_invoked.load(Ordering::SeqCst));

    let state = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": "transport-v2-attach"
        }))
        .await;
    assert_eq!(structured(&state)["status"], "ok", "{}", structured(&state));
    crate::session::fire_session_end("transport-v2-attach");
}

#[tokio::test]
async fn an_existing_profile_attach_changes_its_claim_only_under_the_browser_gate() {
    const TRANSPORT: &str = "transport-v2-attach-gate";
    let (f, _provider) = protected_existing_profile_fixture().await;
    let fingerprint = f.engine.platform.process_fingerprint(1).await.unwrap();
    // The gate a reconnect of this endpoint holds.
    let gate = f
        .engine
        .reconnect_gates
        .lock(super::reconnect::ReconnectKey::new(&fingerprint))
        .await;
    let tool = BrowserPrepareTool::new(f.engine.clone());
    let prepare = tool.invoke(json!({
        "pid": 1,
        "window_id": 7,
        "session": SESSION,
        "_transport_session_id": TRANSPORT,
        "strategy": { "kind": "existing_profile" }
    }));
    tokio::pin!(prepare);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut prepare)
            .await
            .is_err(),
        "the attach must wait for the browser gate"
    );
    assert!(
        f.engine
            .existing_profile_grant(SESSION, Some(TRANSPORT), 1)
            .await
            .unwrap()
            .is_none(),
        "no grant is minted while another holder owns the gate"
    );
    drop(gate);
    let prepared = prepare.await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn a_cancelled_reprepare_never_leaves_a_claim_without_a_grant() {
    const TRANSPORT: &str = "transport-v2-reprepare-cancel";
    let (f, _provider) = protected_existing_profile_fixture().await;
    let url = f._server.ws_url();
    let tool = BrowserPrepareTool::new(f.engine.clone());
    let request = json!({
        "pid": 1,
        "window_id": 7,
        "session": SESSION,
        "_transport_session_id": TRANSPORT,
        "strategy": { "kind": "existing_profile" }
    });
    let prepared = tool.invoke(request.clone()).await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    let first = f
        .engine
        .existing_profile_grant(SESSION, Some(TRANSPORT), 1)
        .await
        .unwrap()
        .unwrap()
        .generation;

    // An ordinary dial stalled in its handshake holds the pool lock, so the
    // re-prepare stops at its first pool step; cancel it there.
    let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled_url = format!(
        "ws://127.0.0.1:{}/devtools/browser/stalled",
        stalled.local_addr().unwrap().port()
    );
    let holder = tokio::spawn({
        let engine = f.engine.clone();
        async move { engine.pool.get(&stalled_url).await.map(|_| ()) }
    });
    let _accepted = stalled.accept().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), tool.invoke(request))
            .await
            .is_err(),
        "the re-prepare must reach the stalled pool"
    );
    holder.abort();
    let _ = holder.await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The cancelled re-prepare changed nothing: the first grant is still
    // registered and still holds its claim.
    let surviving = f
        .engine
        .existing_profile_grant(SESSION, Some(TRANSPORT), 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(surviving.generation, first);
    assert!(f.engine.pool.get_existing(&url, first).await.is_ok());

    // Releasing it releases every claim.
    f.engine
        .revoke_existing_profile_grant(SESSION, Some(TRANSPORT), 1)
        .await;
    let Err(error) = f.engine.pool.get_existing(&url, first).await else {
        panic!("generation {first} still owns the socket after its session ended")
    };
    assert!(error.to_string().contains("missing"), "{error}");
}

#[tokio::test]
async fn a_revocation_cancelled_on_a_busy_pool_still_releases_the_claim() {
    const TRANSPORT: &str = "transport-v2-revoke-cancel";
    let (f, _provider) = protected_existing_profile_fixture().await;
    let url = f._server.ws_url();
    let prepared = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    let generation = f
        .engine
        .existing_profile_grant(SESSION, Some(TRANSPORT), 1)
        .await
        .unwrap()
        .unwrap()
        .generation;

    let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled_url = format!(
        "ws://127.0.0.1:{}/devtools/browser/stalled",
        stalled.local_addr().unwrap().port()
    );
    let holder = tokio::spawn({
        let engine = f.engine.clone();
        async move { engine.pool.get(&stalled_url).await.map(|_| ()) }
    });
    let _accepted = stalled.accept().await.unwrap();
    // The grant leaves the registry, then its release waits on the pool.
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            f.engine
                .revoke_existing_profile_grant(SESSION, Some(TRANSPORT), 1),
        )
        .await
        .is_err(),
        "the revocation must reach the stalled pool"
    );
    holder.abort();
    let _ = holder.await;

    let mut released = false;
    for _ in 0..50 {
        if f.engine.pool.get_existing(&url, generation).await.is_err() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        released,
        "generation {generation} kept the socket after its grant was revoked"
    );
}

#[tokio::test]
async fn a_session_ended_from_a_thread_without_a_runtime_releases_its_claim() {
    const TRANSPORT: &str = "transport-v2-sweeper-end";
    let (f, _provider) = protected_existing_profile_fixture().await;
    let url = f._server.ws_url();
    let prepared = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    let generation = f
        .engine
        .existing_profile_grant(SESSION, Some(TRANSPORT), 1)
        .await
        .unwrap()
        .unwrap()
        .generation;

    // As an SDK's idle sweeper does: end the session from a plain thread.
    std::thread::spawn(|| crate::session::fire_session_end(TRANSPORT))
        .join()
        .unwrap();
    let mut released = false;
    for _ in 0..50 {
        if f.engine.pool.get_existing(&url, generation).await.is_err() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        released,
        "generation {generation} kept the socket after its session ended"
    );
}

/// Forwards WebSocket connections to the mock endpoint after a delay, so a
/// claim outlasts the 500 ms prompt window; `cut` drops every open link.
struct SlowProxy {
    ws_url: String,
    links: Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>>,
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl SlowProxy {
    async fn start(target_ws_url: &str, delay: std::time::Duration) -> Self {
        let target = target_ws_url
            .trim_start_matches("ws://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let links = Arc::new(StdMutex::new(Vec::new()));
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (task_links, task_accepted) = (links.clone(), accepted.clone());
        let accept_task = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                task_accepted.fetch_add(1, Ordering::SeqCst);
                let target = target.clone();
                task_links.lock().unwrap().push(tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut outbound = tokio::net::TcpStream::connect(target).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }));
            }
        });
        Self {
            ws_url: format!("ws://127.0.0.1:{port}/devtools/browser/mock"),
            links,
            accepted,
            accept_task,
        }
    }

    fn cut(&self) {
        for link in self.links.lock().unwrap().drain(..) {
            link.abort();
        }
    }

    fn connections(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for SlowProxy {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.cut();
    }
}

#[tokio::test]
async fn a_prepare_cancelled_mid_handshake_leaves_a_grant_the_next_bind_can_use() {
    const TRANSPORT: &str = "transport-prepare-cancel-handshake";
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state)).await;
    // A slow endpoint: every handshake takes 400 ms.
    let proxy = SlowProxy::start(&server.ws_url(), std::time::Duration::from_millis(400)).await;
    let engine = BrowserEngine::new(Arc::new(standard_mode_platform(
        proxy.ws_url.clone(),
        EndpointTransport::ExtensionRelay,
    )));
    let args = json!({
        "pid": 1, "window_id": 7, "session": SESSION, "_transport_session_id": TRANSPORT,
    });
    let mut prepare_args = args.clone();
    prepare_args["strategy"] = json!({ "kind": "existing_profile" });
    let tool = BrowserPrepareTool::new(engine.clone());
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            tool.invoke(prepare_args)
        )
        .await
        .is_err(),
        "the prepare must still be in its handshake"
    );
    assert!(
        engine
            .existing_profile_grant(SESSION, Some(TRANSPORT), 1)
            .await
            .unwrap()
            .is_some(),
        "the cancelled prepare left its grant"
    );

    // The endpoint answers (slowly) again: the next bind dials for the grant.
    let bound = GetBrowserStateTool::new(engine.clone()).invoke(args).await;
    assert_eq!(structured(&bound)["status"], "ok", "{}", structured(&bound));
    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn a_second_window_binds_under_the_extension_grant() {
    const TRANSPORT: &str = "transport-extension-two-windows";
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state)).await;
    let engine = BrowserEngine::new(Arc::new(standard_mode_platform(
        server.ws_url(),
        EndpointTransport::ExtensionRelay,
    )));
    let bind = |window_id: u64| {
        let engine = engine.clone();
        async move {
            let bound = GetBrowserStateTool::new(engine)
                .invoke(json!({
                    "pid": 1, "window_id": window_id,
                    "session": SESSION, "_transport_session_id": TRANSPORT,
                }))
                .await;
            structured(&bound).clone()
        }
    };
    let first = bind(7).await;
    assert_eq!(first["status"], "ok", "{first}");
    let second = bind(8).await;
    assert_eq!(second["status"], "ok", "{second}");
    // The first window's binding still works.
    let tab = first["tabs"][0]["tab_id"].as_str().unwrap();
    let again = GetBrowserStateTool::new(engine.clone())
        .invoke(json!({
            "target_id": first["target_id"], "tab_id": tab,
            "session": SESSION, "_transport_session_id": TRANSPORT,
        }))
        .await;
    assert_eq!(structured(&again)["status"], "ok", "{}", structured(&again));
    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn an_explicitly_approved_grant_stays_tied_to_its_window() {
    const TRANSPORT: &str = "transport-approved-two-windows";
    let (f, _provider) = protected_existing_profile_fixture().await;
    let args = |window_id: u64| {
        json!({
            "pid": 1, "window_id": window_id,
            "session": SESSION, "_transport_session_id": TRANSPORT,
        })
    };
    let mut prepare = args(7);
    prepare["strategy"] = json!({ "kind": "existing_profile" });
    let prepared = BrowserPrepareTool::new(f.engine.clone())
        .invoke(prepare)
        .await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    let first = GetBrowserStateTool::new(f.engine.clone())
        .invoke(args(7))
        .await;
    assert_eq!(structured(&first)["status"], "ok", "{}", structured(&first));
    let second = GetBrowserStateTool::new(f.engine.clone())
        .invoke(args(8))
        .await;
    let second = structured(&second).clone();
    assert_eq!(second["status"], "refused", "{second}");
    assert_eq!(
        second["refusal"]["code"], "browser_binding_stale",
        "{second}"
    );
    crate::session::fire_session_end(TRANSPORT);
}

fn standard_mode_platform(ws_url: String, transport: EndpointTransport) -> FixturePlatform {
    FixturePlatform {
        ws_url,
        trusted_input_limited: false,
        managed_endpoint_visible: false,
        process_role: BrowserProcessRole::StandaloneConsumer,
        managed_discovery_invoked: Arc::new(AtomicBool::new(false)),
        existing_endpoint_visible: Arc::new(AtomicBool::new(true)),
        setup_invoked: Arc::new(AtomicBool::new(false)),
        setup_aborted: Arc::new(AtomicBool::new(false)),
        stall_consent: false,
        existing_transport: transport,
        route_script: Default::default(),
    }
}

#[tokio::test]
async fn connected_extension_is_consent_for_the_extension_route_only() {
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state)).await;
    let prepare = |platform: FixturePlatform, transport: &'static str| {
        let setup_invoked = platform.setup_invoked.clone();
        async move {
            let result = BrowserPrepareTool::new(BrowserEngine::new(Arc::new(platform)))
                .invoke(json!({
                    "pid": 1,
                    "window_id": 7,
                    "session": SESSION,
                    "_transport_session_id": transport,
                    "strategy": { "kind": "existing_profile" }
                }))
                .await;
            assert!(
                !setup_invoked.load(Ordering::SeqCst),
                "never the setup page"
            );
            structured(&result).clone()
        }
    };

    // Standard mode, no --grant, no approval host: the extension link is the consent.
    let attached = prepare(
        standard_mode_platform(server.ws_url(), EndpointTransport::ExtensionRelay),
        "transport-extension-consent",
    )
    .await;
    assert_eq!(attached["status"], "ok", "{attached}");
    assert_eq!(attached["action"], "attached_existing_profile");
    crate::session::fire_session_end("transport-extension-consent");

    // A slow extension link, on attach and on reconnect, must never reach
    // Chrome's remote-debugging prompt handler (the fixture's refuses).
    let proxy = SlowProxy::start(&server.ws_url(), std::time::Duration::from_millis(700)).await;
    let engine = BrowserEngine::new(Arc::new(standard_mode_platform(
        proxy.ws_url.clone(),
        EndpointTransport::ExtensionRelay,
    )));
    let slow = BrowserPrepareTool::new(engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": "transport-extension-slow",
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    assert_eq!(structured(&slow)["status"], "ok", "{}", structured(&slow));
    proxy.cut();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let rebound = GetBrowserStateTool::new(engine)
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": "transport-extension-slow"
        }))
        .await;
    assert_eq!(
        structured(&rebound)["status"],
        "ok",
        "{}",
        structured(&rebound)
    );
    assert!(proxy.connections() >= 2, "the bind must have reconnected");
    crate::session::fire_session_end("transport-extension-slow");

    // The same Chrome reachable only through its own debugging port is not consent.
    let refused = prepare(
        standard_mode_platform(server.ws_url(), EndpointTransport::LegacyJsonVersion),
        "transport-extension-absent",
    )
    .await;
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(refused["refusal"]["code"], "browser_consent_required");

    // The extension disconnects, or Chrome's debugging port appears instead,
    // between the consent check and attachment: refused, no setup page.
    for after_consent in [None, Some(EndpointTransport::LegacyJsonVersion)] {
        let platform = standard_mode_platform(server.ws_url(), EndpointTransport::ExtensionRelay);
        platform
            .route_script
            .lock()
            .unwrap()
            .extend([Some(EndpointTransport::ExtensionRelay), after_consent]);
        let changed = prepare(platform, "transport-extension-changed").await;
        assert_eq!(changed["status"], "refused", "{changed}");
        assert_eq!(changed["refusal"]["code"], "browser_consent_required");
    }
}

#[tokio::test]
async fn a_connected_extension_lets_the_bind_attach_without_a_prepare_step() {
    const TRANSPORT: &str = "transport-extension-bind";
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state)).await;
    let platform = standard_mode_platform(server.ws_url(), EndpointTransport::ExtensionRelay);
    let setup_invoked = platform.setup_invoked.clone();
    let engine = BrowserEngine::new(Arc::new(platform));
    let bound = GetBrowserStateTool::new(engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": TRANSPORT
        }))
        .await;
    let bound = structured(&bound).clone();
    assert_eq!(bound["status"], "ok", "{bound}");
    assert_eq!(bound["endpoint_access_class"], "existing_profile_approved");
    assert_eq!(bound["endpoint_transport"], "extension_relay");
    assert!(
        !setup_invoked.load(Ordering::SeqCst),
        "never the setup page"
    );
    crate::session::fire_session_end(TRANSPORT);

    // The extension appears connected but its route is gone by the time the
    // bind attaches: refused, never a fallback to another endpoint.
    let platform = standard_mode_platform(server.ws_url(), EndpointTransport::ExtensionRelay);
    platform.route_script.lock().unwrap().extend([
        Some(EndpointTransport::ExtensionRelay),
        Some(EndpointTransport::LegacyJsonVersion),
    ]);
    let setup_invoked = platform.setup_invoked.clone();
    let refused = GetBrowserStateTool::new(BrowserEngine::new(Arc::new(platform)))
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": "transport-extension-bind-gone"
        }))
        .await;
    let refused = structured(&refused).clone();
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(refused["refusal"]["code"], "browser_consent_required");
    assert!(
        !setup_invoked.load(Ordering::SeqCst),
        "never the setup page"
    );
}

#[tokio::test]
async fn a_shared_existing_profile_socket_outlives_one_of_its_sessions() {
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state)).await;
    let url = server.ws_url();
    let pool = super::cdp_ws::CdpPool::new();
    // Two Cua sessions' grants claim the same browser socket.
    let first = pool.claim_existing(&url, 1, || true).await.unwrap();
    let second = pool.claim_existing(&url, 2, || true).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    first.register_dialog_session("sess-b", "target-b");
    assert!(
        pool.get_existing(&url, 1).await.is_ok(),
        "an earlier claim stays usable"
    );

    // Ending the first session keeps the socket, and the second session's
    // dialog routing on it, alive.
    pool.release_existing(&url, 1).await;
    let still = pool.get_existing(&url, 2).await.unwrap();
    assert!(Arc::ptr_eq(&still, &second) && !still.is_closed());
    assert!(still.has_dialog_session("target-b"));
    assert!(pool.get_existing(&url, 1).await.is_err());

    // The last session's release closes it.
    pool.release_existing(&url, 2).await;
    assert!(pool.get_existing(&url, 2).await.is_err());
}

#[tokio::test]
async fn relay_tab_attach_carries_the_session_cursor_color() {
    const TRANSPORT: &str = "transport-session-color";
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state.clone())).await;
    let engine = BrowserEngine::new(Arc::new(standard_mode_platform(
        server.ws_url(),
        EndpointTransport::ExtensionRelay,
    )));
    let args = |extra: Value| {
        let mut args = json!({ "session": SESSION, "_transport_session_id": TRANSPORT });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };
    let prepared = BrowserPrepareTool::new(engine.clone())
        .invoke(args(
            json!({ "pid": 1, "window_id": 7, "strategy": { "kind": "existing_profile" } }),
        ))
        .await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    let bound = GetBrowserStateTool::new(engine.clone())
        .invoke(args(json!({ "pid": 1, "window_id": 7 })))
        .await;
    let bound = structured(&bound).clone();
    let tab = bound["tabs"][0]["tab_id"].as_str().unwrap().to_owned();
    GetBrowserStateTool::new(engine)
        .invoke(args(
            json!({ "target_id": bound["target_id"], "tab_id": tab }),
        ))
        .await;
    let attaches: Vec<Value> = state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(_, method, _)| method == "Target.attachToTarget")
        .map(|(_, _, params)| params.clone())
        .collect();
    assert!(!attaches.is_empty());
    let color = cua_driver_contract::cursor::session_fill_hex(SESSION);
    assert!(
        attaches
            .iter()
            .all(|params| params["cuaSessionColor"] == json!(color)),
        "{attaches:?}"
    );
    crate::session::fire_session_end(TRANSPORT);

    // A real DevTools endpoint never receives the relay-only field.
    let f = fixture().await;
    let (target_id, tab_id) = bind(&f).await;
    snapshot(&f, &target_id, &tab_id).await;
    let attaches = recorded_calls(&f, "Target.attachToTarget");
    assert!(!attaches.is_empty());
    assert!(attaches
        .iter()
        .all(|(_, params)| params.get("cuaSessionColor").is_none()));
}

/// Prepare and bind one Cua session on the relay fixture and return the
/// relay holder its tab attaches named.
async fn relay_bind(
    engine: &Arc<BrowserEngine>,
    state: &SharedState,
    session: &str,
    transport: &str,
) -> String {
    let attaches_before = relay_calls(state, "Target.attachToTarget").len();
    let args = |extra: Value| {
        let mut args = json!({ "session": session, "_transport_session_id": transport });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };
    let prepared = BrowserPrepareTool::new(engine.clone())
        .invoke(args(
            json!({ "pid": 1, "window_id": 7, "strategy": { "kind": "existing_profile" } }),
        ))
        .await;
    assert_eq!(
        structured(&prepared)["status"],
        "ok",
        "{}",
        structured(&prepared)
    );
    let bound = GetBrowserStateTool::new(engine.clone())
        .invoke(args(json!({ "pid": 1, "window_id": 7 })))
        .await;
    let bound = structured(&bound).clone();
    let tab = bound["tabs"][0]["tab_id"].as_str().unwrap().to_owned();
    GetBrowserStateTool::new(engine.clone())
        .invoke(args(
            json!({ "target_id": bound["target_id"], "tab_id": tab }),
        ))
        .await;
    let holders: Vec<String> = relay_calls(state, "Target.attachToTarget")
        .into_iter()
        .skip(attaches_before)
        .filter_map(|params| params["cuaSession"].as_str().map(str::to_owned))
        .filter(|holder| holder.starts_with(&format!("{session}#")))
        .collect();
    let last = holders.last().expect("the bind attached a tab").clone();
    assert!(holders.iter().all(|holder| *holder == last), "{holders:?}");
    last
}

fn relay_calls(state: &SharedState, method: &str) -> Vec<Value> {
    state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(_, m, _)| m == method)
        .map(|(_, _, params)| params.clone())
        .collect()
}

async fn relay_releases_eventually(state: &SharedState, holders: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        let released: Vec<String> = relay_calls(state, "Cua.releaseSession")
            .into_iter()
            .filter_map(|params| params["cuaSession"].as_str().map(str::to_owned))
            .collect();
        if holders
            .iter()
            .all(|holder| released.iter().any(|r| r == holder))
        {
            return released;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    relay_calls(state, "Cua.releaseSession")
        .into_iter()
        .filter_map(|params| params["cuaSession"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_late_release_from_an_ended_episode_never_names_the_restarted_one() {
    const TRANSPORT: &str = "transport-relay-episode";
    const EPISODE_SESSION: &str = "relay-episode-session";
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state.clone())).await;
    let engine = BrowserEngine::new(Arc::new(standard_mode_platform(
        server.ws_url(),
        EndpointTransport::ExtensionRelay,
    )));
    let first = relay_bind(&engine, &state, EPISODE_SESSION, TRANSPORT).await;
    let ended = engine
        .existing_profile_grant(EPISODE_SESSION, Some(TRANSPORT), 1)
        .await
        .unwrap()
        .unwrap();

    // End the session, start it again under the same name, and bind.
    crate::session::fire_session_end(TRANSPORT);
    let second = relay_bind(&engine, &state, EPISODE_SESSION, TRANSPORT).await;
    assert_ne!(
        first, second,
        "each episode holds its tabs under its own name"
    );

    // The ended episode's release lands only now.
    super::engine::release_grant_claim(&engine.pool, &ended).await;
    let released = relay_releases_eventually(&state, &[&first]).await;
    assert!(released.contains(&first), "{released:?}");
    assert!(
        !released.contains(&second),
        "a release from the ended episode named the live one: {released:?}"
    );
    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn an_episode_expired_off_runtime_still_releases_its_relay_tabs() {
    let state = Arc::new(StdMutex::new(FixtureState::default()));
    let server = MockCdpServer::start(fixture_handler(state.clone())).await;
    let engine = BrowserEngine::new(Arc::new(standard_mode_platform(
        server.ws_url(),
        EndpointTransport::ExtensionRelay,
    )));
    // Two Cua sessions share the relay socket and its tab.
    let expired = relay_bind(&engine, &state, "relay-expired", "transport-relay-expired").await;
    let live = relay_bind(&engine, &state, "relay-live", "transport-relay-live").await;

    // The expired session ends from an SDK sweeper's plain thread, which
    // cannot tell the relay itself.
    std::thread::spawn(|| crate::session::fire_session_end("transport-relay-expired"))
        .join()
        .unwrap();
    // Ending the live session releases both holds, so the tab detaches.
    crate::session::fire_session_end("transport-relay-live");
    let released = relay_releases_eventually(&state, &[&expired, &live]).await;
    assert!(
        released.contains(&expired) && released.contains(&live),
        "{released:?}"
    );
}

#[tokio::test]
async fn approved_existing_profile_tools_stay_within_the_reviewed_cdp_surface() {
    const TRANSPORT: &str = "transport-v2-method-policy";
    let (f, _) = protected_existing_profile_fixture().await;
    let prepare = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    assert_eq!(
        structured(&prepare)["status"],
        "ok",
        "{}",
        structured(&prepare)
    );

    let state = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "_transport_session_id": TRANSPORT
        }))
        .await;
    let bound = structured(&state);
    assert_eq!(bound["status"], "ok", "{bound}");
    let target = bound["target_id"].as_str().unwrap().to_owned();
    let tab = bound["tabs"][0]["tab_id"].as_str().unwrap().to_owned();

    let snap = snapshot(&f, &target, &tab).await;
    let button = ref_of(&snap, "main", "main-btn");
    let input = ref_of(&snap, "main", "Shadow Input");
    let clicked = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "ref": button,
            "input_route": "dom_event",
            "session": SESSION
        }))
        .await;
    assert_eq!(
        structured(&clicked)["status"],
        "ok",
        "{}",
        structured(&clicked)
    );
    let typed = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "ref": input,
            "text": "policy-check",
            "session": SESSION
        }))
        .await;
    assert_eq!(structured(&typed)["status"], "ok", "{}", structured(&typed));
    let navigated = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "url": "https://fixture.test/policy-check",
            "session": SESSION
        }))
        .await;
    assert_eq!(
        structured(&navigated)["status"],
        "ok",
        "{}",
        structured(&navigated)
    );

    let forbidden = [
        "Runtime.enable",
        "Target.setDiscoverTargets",
        "Page.addScriptToEvaluateOnNewDocument",
        "Network.enable",
        "Fetch.enable",
        "Emulation.setUserAgentOverride",
        "Emulation.setDeviceMetricsOverride",
        "Emulation.setTimezoneOverride",
    ];
    let calls = f.state.lock().unwrap().calls.clone();
    for method in forbidden {
        assert!(
            calls.iter().all(|(_, observed, _)| observed != method),
            "existing-profile operation emitted forbidden CDP method {method}: {calls:?}"
        );
    }

    crate::session::fire_session_end(TRANSPORT);
}

#[tokio::test]
async fn protected_provider_accepts_exact_attach_and_session_end_revokes_the_grant() {
    const PROTECTED_SESSION: &str = "protected-provider-v2";
    const PROTECTED_TRANSPORT: &str = "protected-transport-v2";
    let (f, provider) = protected_existing_profile_fixture().await;
    let prepare = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": PROTECTED_SESSION,
            "_transport_session_id": PROTECTED_TRANSPORT,
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    assert_eq!(
        structured(&prepare)["status"],
        "ok",
        "{}",
        structured(&prepare)
    );
    assert!(provider.consent_seen.load(Ordering::SeqCst));

    crate::session::fire_session_end(PROTECTED_TRANSPORT);
    tokio::task::yield_now().await;

    let state = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": PROTECTED_SESSION,
            "_transport_session_id": PROTECTED_TRANSPORT
        }))
        .await;
    assert_eq!(structured(&state)["status"], "refused");
}

#[tokio::test]
async fn approved_existing_profile_setup_reports_exact_side_effects() {
    let (f, setup_invoked) = existing_profile_setup_fixture().await;
    let prepare = BrowserPrepareTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "strategy": { "kind": "existing_profile" }
        }))
        .await;
    let prepared = structured(&prepare);
    assert_eq!(prepared["status"], "ok", "{prepared}");
    assert!(setup_invoked.load(Ordering::SeqCst));
    assert_eq!(prepared["side_effects"]["opened_setup_page"], true);
    assert_eq!(prepared["side_effects"]["closed_setup_page"], true);
    assert_eq!(prepared["side_effects"]["enabled_remote_debugging"], true);
    assert_eq!(prepared["side_effects"]["changed_preferences"], true);
    assert_eq!(
        prepared["side_effects"]["focused_setup_address_field"],
        true
    );

    let state = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION
        }))
        .await;
    assert_eq!(structured(&state)["status"], "ok", "{}", structured(&state));
}

#[tokio::test]
async fn refused_consent_cancels_stalled_claim_before_revoking_grant() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stalled_server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    });
    let engine = BrowserEngine::new_with_protected_consent_provider(
        Arc::new(FixturePlatform {
            ws_url: format!("ws://{address}/devtools/browser"),
            trusted_input_limited: false,
            managed_endpoint_visible: false,
            process_role: BrowserProcessRole::StandaloneConsumer,
            managed_discovery_invoked: Arc::new(AtomicBool::new(false)),
            existing_endpoint_visible: Arc::new(AtomicBool::new(false)),
            setup_invoked: Arc::new(AtomicBool::new(false)),
            setup_aborted: Arc::new(AtomicBool::new(false)),
            stall_consent: false,
            existing_transport: EndpointTransport::LegacyJsonVersion,
            route_script: Default::default(),
        }),
        Some(Arc::new(FixtureProtectedProvider {
            consent_seen: AtomicBool::new(false),
        })),
    );
    let prepared = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        BrowserPrepareTool::new(engine).invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "strategy": { "kind": "existing_profile" }
        })),
    )
    .await
    .expect("a refused consent route must not deadlock grant revocation");
    assert_eq!(structured(&prepared)["status"], "refused");
    assert_eq!(
        structured(&prepared)["refusal"]["code"],
        "browser_route_unavailable"
    );
    stalled_server.abort();
}

#[tokio::test]
async fn cancelled_prepare_aborts_the_exact_pending_setup() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stalled_server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    });
    let setup_aborted = Arc::new(AtomicBool::new(false));
    let engine = BrowserEngine::new_with_protected_consent_provider(
        Arc::new(FixturePlatform {
            ws_url: format!("ws://{address}/devtools/browser"),
            trusted_input_limited: false,
            managed_endpoint_visible: false,
            process_role: BrowserProcessRole::StandaloneConsumer,
            managed_discovery_invoked: Arc::new(AtomicBool::new(false)),
            existing_endpoint_visible: Arc::new(AtomicBool::new(false)),
            setup_invoked: Arc::new(AtomicBool::new(false)),
            setup_aborted: setup_aborted.clone(),
            stall_consent: true,
            existing_transport: EndpointTransport::LegacyJsonVersion,
            route_script: Default::default(),
        }),
        Some(Arc::new(FixtureProtectedProvider {
            consent_seen: AtomicBool::new(false),
        })),
    );
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(750),
        BrowserPrepareTool::new(engine).invoke(json!({
            "pid": 1,
            "window_id": 7,
            "session": SESSION,
            "strategy": { "kind": "existing_profile" }
        })),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "fixture must cancel during consent handling"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !setup_aborted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping prepare must schedule exact pending-setup rollback");
    stalled_server.abort();
}

/// A `dom_refs_v1` snapshot (the flat ref list).
async fn snapshot(f: &Fixture, target_id: &str, tab_id: &str) -> Value {
    let tool = GetBrowserStateTool::new(f.engine.clone());
    let result = tool
        .invoke(
            json!({ "target_id": target_id, "tab_id": tab_id, "session": SESSION,
            "snapshot_format": "dom_refs_v1" }),
        )
        .await;
    structured(&result).clone()
}

/// One outline line read back into its parts: `- role "name" [ref actions]
/// = "value" -> "url" (states)`.
fn parse_outline_line(line: &str) -> Value {
    fn quoted(text: &str) -> Option<(String, &str)> {
        let mut stream = serde_json::Deserializer::from_str(text).into_iter::<String>();
        let value = stream.next()?.ok()?;
        Some((value, &text[stream.byte_offset()..]))
    }
    let rest = line
        .trim_start()
        .strip_prefix("- ")
        .expect("an outline line");
    let (role, mut rest) = rest.split_once(' ').expect("a role and a bracket");
    let mut name = Value::Null;
    if rest.starts_with('"') {
        let (text, after) = quoted(rest).expect("a quoted name");
        name = json!(text);
        rest = after.trim_start();
    }
    let (bracket, mut rest) = rest
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .expect("a ref bracket");
    let (reference, actions) = bracket.split_once(' ').unwrap_or((bracket, ""));
    let actions: Vec<&str> = actions
        .split(',')
        .filter(|action| !action.is_empty())
        .collect();
    let (mut value, mut url) = (Value::Null, Value::Null);
    if let Some(after) = rest.strip_prefix(" = ") {
        let (text, after) = quoted(after).expect("a quoted value");
        value = json!(text);
        rest = after;
    }
    if let Some(after) = rest.strip_prefix(" -> ") {
        let (text, after) = quoted(after).expect("a quoted url");
        url = json!(text);
        rest = after;
    }
    let states: Vec<&str> = rest
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(", ")
        .filter(|state| !state.is_empty())
        .collect();
    let frame = ["iframe", "oopif"]
        .into_iter()
        .find(|kind| states.contains(kind))
        .unwrap_or("main");
    json!({
        "ref": reference, "role": role, "name": name, "value": value, "url": url,
        "actions": actions, "states": states, "frame": frame, "line": line,
    })
}

/// A semantic snapshot's outline as entries: `refs` for lines that declare
/// an action, `content_refs` for the rest. The result itself has only the
/// outline; these two keys exist for the assertions below.
fn with_outline_entries(mut snapshot: Value) -> Value {
    let Some(outline) = snapshot["outline"].as_str().map(str::to_owned) else {
        return snapshot;
    };
    assert!(snapshot.get("refs").is_none() && snapshot.get("content_refs").is_none());
    let (actions, content): (Vec<Value>, Vec<Value>) = outline
        .lines()
        .map(parse_outline_line)
        .partition(|entry| !entry["actions"].as_array().unwrap().is_empty());
    snapshot["refs"] = json!(actions);
    snapshot["content_refs"] = json!(content);
    snapshot
}

async fn semantic_snapshot(f: &Fixture, target_id: &str, tab_id: &str) -> Value {
    semantic_snapshot_with(f, target_id, tab_id, json!({})).await
}

async fn semantic_snapshot_with(f: &Fixture, target_id: &str, tab_id: &str, extra: Value) -> Value {
    let mut args = json!({
        "target_id": target_id,
        "tab_id": tab_id,
        "session": SESSION,
    });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(args)
        .await;
    with_outline_entries(structured(&result).clone())
}

/// Follow continuations from `page` until one lists an action named `name`.
async fn continue_to(
    f: &Fixture,
    target_id: &str,
    tab_id: &str,
    mut page: Value,
    name: &str,
) -> Value {
    for _ in 0..64 {
        if page["refs"]
            .as_array()
            .is_some_and(|refs| refs.iter().any(|entry| entry["name"] == name))
        {
            return page;
        }
        let token = page["snapshot"]["continuation"]
            .as_str()
            .unwrap_or_else(|| panic!("{name:?} was never reached: {page}"))
            .to_owned();
        page = semantic_snapshot_with(f, target_id, tab_id, json!({"continuation": token})).await;
        assert_eq!(page["status"], "ok", "{page}");
    }
    panic!("{name:?} was not reached within 64 pages")
}

/// The `ref` string of the first snapshot entry in the given frame kind
/// with the given backing label fragment (or any, if empty).
fn ref_of(snapshot: &Value, frame: &str, label_fragment: &str) -> String {
    snapshot["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["frame"] == frame
                && (label_fragment.is_empty()
                    || r["label"].as_str().unwrap_or("").contains(label_fragment))
        })
        .unwrap_or_else(|| panic!("no {frame} ref with label ~{label_fragment:?}: {snapshot}"))
        ["ref"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The PiP's question about a bound tab (which macOS window shows it, and is
/// it the selected tab there) sits in front of every bound-tab action, under
/// its mutation lock. It must never wait on the browser: a page busy in
/// JavaScript, or behind a dialog, answers nothing for the connection's
/// whole call timeout.
#[tokio::test]
async fn the_pip_window_of_a_bound_tab_is_decided_without_asking_the_browser() {
    let f = fixture().await;
    let (target_id, tab_id) = bind(&f).await;
    let mut validated = f
        .engine
        .revalidate_for_mutation(SESSION, &target_id, Some(&tab_id))
        .await
        .unwrap_or_else(|refusal| panic!("revalidation: {refusal:?}"));
    // From here on the page answers nothing at all.
    f.state.lock().unwrap().calls.clear();
    // Not a future: there is nothing in it that could wait. The native
    // window's title names this tab alone, so it is the selected tab, and
    // the binding's macOS pid and window are the picture's.
    let window: Option<(i32, u32)> = f.engine.pip_window(&validated);
    assert_eq!(window, Some((1, 7)));
    // A tab the title does not single out is not known to be selected.
    validated.selected_by_title = false;
    assert_eq!(f.engine.pip_window(&validated), None);
    assert!(
        f.state.lock().unwrap().calls.is_empty(),
        "no CDP call was made, so none could go unanswered"
    );
}

fn recorded_calls(f: &Fixture, method: &str) -> Vec<(Option<String>, Value)> {
    f.state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(_, m, _)| m == method)
        .map(|(s, _, p)| (s.clone(), p.clone()))
        .collect()
}

// ── Snapshot composition ─────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_composes_shadow_iframe_and_oopif_refs() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;

    assert_eq!(snap["status"], "ok", "{snap}");
    let frames: Vec<&str> = snap["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["frame"].as_str().unwrap())
        .collect();
    assert_eq!(
        frames,
        vec!["main", "main", "main", "iframe", "oopif"],
        "main button + shadow input + plain input, iframe button, oopif input: {snap}"
    );
    assert_eq!(snap["oopif"]["status"], "attached");
    assert_eq!(snap["oopif"]["frames"], 1);
    assert_eq!(snap["truncated"], false);

    // Composed shadow content keeps its labels; user-agent shadow
    // internals (backend 22, role=button) must not have been minted.
    let labels: Vec<&str> = snap["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap_or(""))
        .collect();
    assert!(labels.iter().any(|l| l.contains("Shadow Input")), "{snap}");
    assert!(
        !labels.iter().any(|l| l.contains("role=button")),
        "user-agent shadow content leaked: {snap}"
    );
}

#[tokio::test]
async fn semantic_snapshot_refreshes_bind_time_title_from_main_document() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot(&f, &target, &tab).await;

    assert_eq!(snap["status"], "ok", "{snap}");
    assert_eq!(snap["page"]["title"], "Current fixture title", "{snap}");
}

#[tokio::test]
async fn semantic_page_title_tracks_navigation_and_continuations() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        set_page_title(st, Some("First page"));
    })
    .await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(first["page"]["title"], "First page");

    let destination = "https://fixture.test/second";
    let navigation = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({"target_id": target, "tab_id": tab,
            "session": SESSION, "url": destination}))
        .await;
    assert_eq!(navigation.structured_content.unwrap()["status"], "ok");
    {
        // The browser independently publishes the newly loaded document.
        let mut state = f.state.lock().unwrap();
        state.main_url = destination.into();
        state.main_loader = "L_MAIN_2".into();
        set_page_title(&mut state, Some("Second page"));
    }
    let second = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(second["page"]["url"], destination);
    assert_eq!(second["page"]["title"], "Second page");
    let token = second["snapshot"]["continuation"].as_str().unwrap();
    let continued = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(continued["page"], second["page"]);
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn semantic_page_title_clears_previous_title_when_empty_or_unavailable() {
    for next_title in [Some(String::new()), None] {
        let f = fixture_with(|st| st.semantic_large_page = true).await;
        let (target, tab) = bind(&f).await;
        let first = semantic_snapshot(&f, &target, &tab).await;
        assert_eq!(first["page"]["title"], "Current fixture title");
        set_page_title(&mut f.state.lock().unwrap(), next_title.as_deref());
        let fresh = semantic_snapshot(&f, &target, &tab).await;
        assert_eq!(fresh["page"]["title"], "");
        let token = fresh["snapshot"]["continuation"].as_str().unwrap();
        let continued =
            semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
        assert_eq!(continued["page"]["title"], "");
    }
}

#[tokio::test]
async fn semantic_page_title_does_not_borrow_an_embedded_frame_title() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.semantic_main_root_present = false;
        // Hide the main title too, so the embedded frame's is the only one.
        st.omit_snapshot_title = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let fresh = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(fresh["page"]["title"], "");
}

#[tokio::test]
async fn semantic_snapshot_keeps_visible_content_after_hidden_node_pressure() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot(&f, &target, &tab).await;

    assert_eq!(snap["status"], "ok", "{snap}");
    assert_eq!(snap["snapshot"]["format"], "semantic_v2", "{snap}");
    assert!(
        snap["outline"]
            .as_str()
            .is_some_and(|outline| outline.contains("Visible message")
                && outline.contains("Please review the attached fixture report.")),
        "visible semantic content was omitted: {snap}"
    );
    let refs = snap["refs"].as_array().expect("semantic refs");
    assert!(
        refs.iter().any(|entry| entry["name"] == "Reply"
            && entry["actions"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| action == "click"))),
        "visible Reply action was omitted: {snap}"
    );
    assert!(
        refs.iter().any(|entry| entry["name"] == "Reply body"
            && entry["actions"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| action == "type"))),
        "visible editor was omitted: {snap}"
    );
    assert!(
        refs.iter().all(|entry| !entry["name"]
            .as_str()
            .unwrap_or("")
            .starts_with("Retained control")),
        "CSS-hidden retained controls leaked into refs: {snap}"
    );
    assert_eq!(snap["snapshot"]["omitted"]["css_hidden"], 320);
}

#[tokio::test]
async fn semantic_snapshot_can_capture_an_inactive_tab_without_activation_calls() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "snapshot_format": "semantic_v2",
            "include_screenshot": true
        }))
        .await;
    let snapshot = structured(&result);
    assert_eq!(snapshot["status"], "ok", "{snapshot}");
    assert_eq!(snapshot["screenshot"]["mime_type"], "image/png");
    assert_eq!(snapshot["screenshot"]["width"], 1);
    assert_eq!(snapshot["screenshot"]["height"], 1);
    assert_eq!(snapshot["screenshot"]["source"], "cdp_tab");
    assert_eq!(snapshot["screenshot_width"], 1);
    assert_eq!(snapshot["screenshot_height"], 1);
    assert_eq!(snapshot["screenshot_mime_type"], "image/png");
    assert_eq!(
        snapshot["screenshot"]["coordinate_space"],
        "viewport_css_px"
    );
    assert_eq!(snapshot["screenshot"]["viewport_css_width"], 800.0);
    assert_eq!(snapshot["screenshot"]["viewport_css_height"], 600.0);
    assert_eq!(snapshot["screenshot"]["pixel_to_css_scale_x"], 800.0);
    assert_eq!(snapshot["screenshot"]["pixel_to_css_scale_y"], 600.0);
    assert!(result.content.iter().any(|content| matches!(
        content,
        Content::Image { mime_type, .. } if mime_type == "image/png"
    )));

    let state = f.state.lock().unwrap();
    assert!(state.calls.iter().any(|(_, method, params)| {
        method == "Page.captureScreenshot"
            && params["format"] == "png"
            && params["fromSurface"] == true
            && params["captureBeyondViewport"] == false
            && params["clip"]["x"] == 0.0
            && params["clip"]["y"] == 0.0
            && params["clip"]["width"] == 800.0
            && params["clip"]["height"] == 600.0
            && params["clip"]["scale"] == 1.0
    }));
    assert!(state.calls.iter().all(|(_, method, _)| {
        method != "Target.activateTarget" && method != "Page.bringToFront"
    }));
}

#[tokio::test]
async fn semantic_snapshot_does_not_capture_unless_requested() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snapshot = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(snapshot["status"], "ok", "{snapshot}");
    assert_eq!(snapshot["screenshot"], Value::Null);
    assert_eq!(snapshot["screenshot_width"], Value::Null);
    assert_eq!(snapshot["screenshot_height"], Value::Null);
    assert_eq!(snapshot["screenshot_mime_type"], Value::Null);
    assert!(recorded_calls(&f, "Page.captureScreenshot").is_empty());
}

#[tokio::test]
async fn requested_tab_screenshot_maps_non_unit_png_pixels_to_viewport_css() {
    let f = fixture_with(|state| {
        state.viewport_css_width = 2.0;
        state.viewport_css_height = 1.0;
        state.screenshot_data = screenshot_png_base64(4, 2);
    })
    .await;
    let (target, tab) = bind(&f).await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "snapshot_format": "dom_refs_v1",
            "include_screenshot": true
        }))
        .await;
    let snapshot = structured(&result);
    assert_eq!(snapshot["status"], "ok", "{snapshot}");
    assert_eq!(snapshot["screenshot_width"], 4);
    assert_eq!(snapshot["screenshot_height"], 2);
    assert_eq!(snapshot["screenshot_mime_type"], "image/png");
    assert_eq!(snapshot["screenshot"]["viewport_css_width"], 2.0);
    assert_eq!(snapshot["screenshot"]["viewport_css_height"], 1.0);
    assert_eq!(snapshot["screenshot"]["pixel_to_css_scale_x"], 0.5);
    assert_eq!(snapshot["screenshot"]["pixel_to_css_scale_y"], 0.5);
    let capture = recorded_calls(&f, "Page.captureScreenshot");
    assert_eq!(capture.len(), 1, "{capture:?}");
    assert_eq!(capture[0].1["clip"]["width"], 2.0);
    assert_eq!(capture[0].1["clip"]["height"], 1.0);
    assert_eq!(capture[0].1["clip"]["scale"], 1.0);
}

#[tokio::test]
async fn requested_tab_screenshot_refuses_invalid_viewport_metrics_before_capture() {
    let f = fixture_with(|state| state.viewport_css_width = 0.0).await;
    let (target, tab) = bind(&f).await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "snapshot_format": "dom_refs_v1",
            "include_screenshot": true
        }))
        .await;
    // The page was read; only the screenshot is refused, beside the read.
    let read = structured(&result);
    assert_eq!(read["status"], "ok", "{read}");
    assert!(read["refs"].as_array().is_some_and(|refs| !refs.is_empty()));
    assert_eq!(read["screenshot"]["status"], "refused", "{read}");
    assert_eq!(
        read["screenshot"]["refusal"]["code"],
        "browser_route_unavailable"
    );
    assert!(read.get("screenshot_width").is_none());
    assert!(recorded_calls(&f, "Page.captureScreenshot").is_empty());
}

#[tokio::test]
async fn requested_tab_screenshot_refuses_malformed_image_data() {
    let f = fixture_with(|state| state.screenshot_data = "not-base64".into()).await;
    let (target, tab) = bind(&f).await;
    let result = GetBrowserStateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "snapshot_format": "semantic_v2",
            "include_screenshot": true
        }))
        .await;
    let read = structured(&result);
    assert_eq!(read["status"], "ok", "{read}");
    assert!(read["outline"].as_str().is_some_and(|o| !o.is_empty()));
    assert_eq!(read["screenshot"]["status"], "refused", "{read}");
    assert_eq!(
        read["screenshot"]["refusal"]["code"],
        "browser_route_unavailable"
    );
    assert!(matches!(
        &result.content[0],
        Content::Text { text, .. } if text.contains("no screenshot: refused (browser_route_unavailable)")
    ));
    assert!(result
        .content
        .iter()
        .all(|content| !matches!(content, Content::Image { .. })));
}

#[tokio::test]
async fn semantic_title_follows_the_collected_document_after_navigation() {
    let f = fixture_with(|_| {}).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(first["page"]["title"], "Current fixture title", "{first}");
    {
        let mut st = f.state.lock().unwrap();
        st.main_title = "New article title".into();
        st.main_url = "https://fixture.test/inbox/item-2".into();
        st.main_loader = "L_MAIN_2".into();
        st.semantic_large_page = true;
    }
    let next = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(next["page"]["url"], "https://fixture.test/inbox/item-2");
    assert_eq!(next["page"]["title"], "New article title", "{next}");
    assert_ne!(next["page"]["title"], "Embedded frame title");
}

#[tokio::test]
async fn semantic_continuation_retains_its_collected_title() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"].as_str().unwrap();
    f.state.lock().unwrap().main_title = "Title changed after collection".into();
    let continued = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(
        continued["page"]["title"], "Current fixture title",
        "{continued}"
    );
    let fresh = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(
        fresh["page"]["title"], "Title changed after collection",
        "{fresh}"
    );
}

#[tokio::test]
async fn semantic_missing_title_is_incomplete_instead_of_using_the_cached_tab_title() {
    let f = fixture_with(|st| st.omit_snapshot_title = true).await;
    let (target, tab) = bind(&f).await;
    let snapshot = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(snapshot["status"], "ok", "{snapshot}");
    assert_eq!(snapshot["page"]["title"], "", "{snapshot}");
    assert_eq!(snapshot["snapshot"]["complete"], false, "{snapshot}");
}

/// With the snapshot title unavailable, an empty main-root title reaches
/// the page as "" (unit coverage: document_title_keeps_the_root_name_exactly).
#[tokio::test]
async fn semantic_empty_accessibility_title_fallback_is_valid() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.omit_snapshot_title = true;
        st.semantic_title = Some(String::new());
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snapshot = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(snapshot["page"]["title"], "", "{snapshot}");
}

#[tokio::test]
async fn semantic_empty_document_title_is_valid() {
    let f = fixture_with(|st| st.main_title.clear()).await;
    let (target, tab) = bind(&f).await;
    let snapshot = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(snapshot["page"]["title"], "", "{snapshot}");
    assert_eq!(snapshot["snapshot"]["complete"], true, "{snapshot}");
}

#[tokio::test]
async fn semantic_continuation_is_opaque_single_use_and_reaches_offscreen_content() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"]
        .as_str()
        .expect("large fixture continuation")
        .to_owned();
    assert!(token.starts_with("bc-"), "{first}");

    let continued = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(continued["status"], "ok", "{continued}");
    assert_eq!(continued["snapshot"]["scope"], "continuation");
    assert_eq!(
        continued["page"]["title"], "Current fixture title",
        "{continued}"
    );
    // Each page is cut to the size budget; the last action is some pages on.
    let last = continue_to(&f, &target, &tab, continued, "Archive item 304").await;
    assert!(last["snapshot"]["continuation"].is_null(), "{last}");
    assert_eq!(last["snapshot"]["complete"], true, "{last}");

    let reused = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(reused["status"], "refused", "{reused}");
    assert_eq!(reused["refusal"]["code"], "browser_ref_stale");
}

/// The `tools/call` response line a client receives for `result`.
fn wire_chars(result: &ToolResult) -> usize {
    json!({"jsonrpc": "2.0", "id": 123_456, "result": result})
        .to_string()
        .chars()
        .count()
}

#[tokio::test]
async fn a_semantic_snapshot_is_the_default_and_fits_its_size_budget() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        // A long address and title spend the budget too.
        st.main_url = format!("https://fixture.test/inbox/{}", "segment/".repeat(40));
        set_page_title(st, Some(&"A long page title ".repeat(12)));
    })
    .await;
    let (target, tab) = bind(&f).await;
    let tool = GetBrowserStateTool::new(f.engine.clone());
    let default = tool
        .invoke(json!({"target_id": target, "tab_id": tab, "session": SESSION}))
        .await;
    let snapshot = structured(&default);
    assert_eq!(snapshot["snapshot"]["format"], "semantic_v2", "{snapshot}");
    assert!(
        snapshot.get("refs").is_none(),
        "refs are inline in the outline"
    );
    assert!(
        wire_chars(&default) <= 6_000,
        "{} chars",
        wire_chars(&default)
    );
    assert!(
        snapshot["snapshot"]["continuation"].is_string(),
        "{snapshot}"
    );
    assert!(snapshot["snapshot"]["omitted"]["budget"].as_u64() > Some(0));

    let larger = tool
        .invoke(
            json!({"target_id": target, "tab_id": tab, "session": SESSION, "max_chars": 20_000}),
        )
        .await;
    assert!(wire_chars(&larger) <= 20_000 && wire_chars(&larger) > 6_000);
    assert!(
        structured(&larger)["snapshot"]["selected_nodes"].as_u64()
            > snapshot["snapshot"]["selected_nodes"].as_u64()
    );

    for bad in [json!(100), json!(1_000_000), json!("big")] {
        let refused = tool
            .invoke(
                json!({"target_id": target, "tab_id": tab, "session": SESSION, "max_chars": bad}),
            )
            .await;
        assert_eq!(refused.is_error, Some(true));
    }
    let legacy = tool
        .invoke(
            json!({"target_id": target, "tab_id": tab, "session": SESSION,
            "snapshot_format": "dom_refs_v1"}),
        )
        .await;
    assert!(
        structured(&legacy)["refs"].is_array(),
        "dom_refs_v1 stays available by name"
    );
}

#[tokio::test]
async fn include_refs_lists_the_outline_refs_for_programs_within_the_same_budget() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let tool = GetBrowserStateTool::new(f.engine.clone());
    let plain = tool
        .invoke(json!({"target_id": target, "tab_id": tab, "session": SESSION}))
        .await;
    let listed = tool
        .invoke(
            json!({"target_id": target, "tab_id": tab, "session": SESSION, "include_refs": true}),
        )
        .await;
    assert!(
        wire_chars(&listed) <= 6_000,
        "{} chars",
        wire_chars(&listed)
    );
    let listed = structured(&listed).clone();
    assert!(
        listed["snapshot"]["selected_nodes"].as_u64()
            < structured(&plain)["snapshot"]["selected_nodes"].as_u64(),
        "the list spends part of the budget"
    );
    // The lists say what the outline says, line for line.
    let outline = listed["outline"].as_str().unwrap();
    let from_outline: Vec<Value> = outline.lines().map(parse_outline_line).collect();
    let from_lists: Vec<&Value> = listed["refs"]
        .as_array()
        .unwrap()
        .iter()
        .chain(listed["content_refs"].as_array().unwrap())
        .collect();
    assert_eq!(from_lists.len(), from_outline.len());
    for entry in from_lists {
        let line = from_outline
            .iter()
            .find(|line| line["ref"] == entry["ref"])
            .unwrap_or_else(|| panic!("{entry} is not in the outline"));
        for field in ["role", "name", "value"] {
            assert_eq!(line[field], entry[field], "{entry}");
        }
    }
    let reply = listed["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Reply")
        .expect("Reply action");
    assert_eq!(reply["actions"][0], "click", "{reply}");
}

#[tokio::test]
async fn newer_semantic_snapshot_invalidates_prior_continuations() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"]
        .as_str()
        .expect("large fixture continuation")
        .to_owned();
    let newer = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(newer["status"], "ok", "{newer}");

    let stale = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(stale["status"], "refused", "{stale}");
    assert_eq!(stale["refusal"]["code"], "browser_ref_stale");
}

#[tokio::test]
async fn main_frame_navigation_invalidates_semantic_continuations() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"]
        .as_str()
        .expect("large fixture continuation")
        .to_owned();

    f.state.lock().unwrap().main_loader = "L_MAIN_2".into();

    let stale = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(stale["status"], "refused", "{stale}");
    assert_eq!(stale["refusal"]["code"], "browser_ref_stale");
}

#[tokio::test]
async fn semantic_query_and_content_scope_are_read_only_and_precise() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let queried =
        semantic_snapshot_with(&f, &target, &tab, json!({"query": "Archive item 304"})).await;
    assert_eq!(queried["snapshot"]["scope"], "query", "{queried}");
    assert_eq!(queried["refs"].as_array().unwrap().len(), 1, "{queried}");
    assert_eq!(queried["refs"][0]["name"], "Archive item 304");

    let natural =
        semantic_snapshot_with(&f, &target, &tab, json!({"query": "reply archive 304"})).await;
    assert_eq!(natural["snapshot"]["scope"], "query", "{natural}");
    // Ranked by the query: the best match and the visible Reply make the
    // first page, ahead of the archive items that only share a word.
    let named = |name: &str| {
        natural["refs"]
            .as_array()
            .is_some_and(|refs| refs.iter().any(|entry| entry["name"] == name))
    };
    assert!(named("Archive item 304") && named("Reply"), "{natural}");
    assert!(!named("Archive item 303"), "{natural}");
    assert!(natural["snapshot"]["continuation"].is_string(), "{natural}");

    let fresh = semantic_snapshot(&f, &target, &tab).await;
    let heading_ref = fresh["content_refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Visible message")
        .and_then(|entry| entry["ref"].as_str())
        .expect("heading content ref")
        .to_owned();
    let scoped = semantic_snapshot_with(&f, &target, &tab, json!({"scope_ref": heading_ref})).await;
    assert_eq!(scoped["snapshot"]["scope"], "subtree", "{scoped}");
    assert_eq!(scoped["refs"].as_array().unwrap().len(), 0, "{scoped}");
    assert_eq!(
        scoped["content_refs"].as_array().unwrap().len(),
        1,
        "{scoped}"
    );
    assert_eq!(scoped["content_refs"][0]["name"], "Visible message");
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn navigation_targets_an_inactive_tab_without_activating_it() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let result = BrowserNavigateTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "url": "https://fixture.test/background-navigation",
            "session": SESSION,
        }))
        .await;

    let structured = result
        .structured_content
        .as_ref()
        .expect("navigation structured content");
    assert_eq!(structured["status"], "ok", "{structured}");
    let navigate = recorded_calls(&f, "Page.navigate");
    assert_eq!(navigate.len(), 1, "{navigate:?}");
    assert_eq!(
        navigate[0].1["url"],
        "https://fixture.test/background-navigation"
    );
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn browser_visual_feedback_probes_live_tab_visibility_without_activating_it() {
    let f = fixture_with(|state| state.tab_visible = false).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let main_ref = ref_of(&snap, "main", "main-btn");

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "ref": main_ref,
            "input_route": "dom_event",
            "session": SESSION,
        }))
        .await;

    assert_eq!(
        structured(&result)["status"],
        "ok",
        "{}",
        structured(&result)
    );
    let visibility = recorded_calls(&f, "Runtime.evaluate");
    assert_eq!(visibility.len(), 1, "{visibility:?}");
    assert_eq!(
        visibility[0].1["expression"],
        "document.visibilityState === 'visible'"
    );
    assert_eq!(visibility[0].1["returnByValue"], true);
    assert_eq!(visibility[0].1["awaitPromise"], false);
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn semantic_refs_enforce_declared_action_kinds_before_delivery() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot(&f, &target, &tab).await;
    let content_ref = snap["content_refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Visible message")
        .and_then(|entry| entry["ref"].as_str())
        .unwrap();
    let click = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "ref": content_ref,
            "input_route": "dom_event"
        }))
        .await;
    assert_eq!(
        structured(&click)["refusal"]["code"],
        "browser_action_unavailable"
    );

    let pointer = BrowserPointerTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "ref": content_ref,
            "action": "hover",
            "input_route": "dom_event"
        }))
        .await;
    assert_eq!(
        structured(&pointer)["refusal"]["code"],
        "browser_action_unavailable"
    );

    let button_ref = snap["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Reply")
        .and_then(|entry| entry["ref"].as_str())
        .unwrap();
    let typed = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "ref": button_ref,
            "text": "must not deliver"
        }))
        .await;
    assert_eq!(
        structured(&typed)["refusal"]["code"],
        "browser_action_unavailable"
    );
    assert!(recorded_calls(&f, "Input.insertText").is_empty());
    assert!(recorded_calls(&f, "Runtime.callFunctionOn").is_empty());
}

// ── Stable semantic refs (the ownership table in observation.rs) ───────────

fn named_ref(snapshot: &Value, name: &str) -> String {
    snapshot["refs"]
        .as_array()
        .unwrap()
        .iter()
        .chain(snapshot["content_refs"].as_array().unwrap())
        .find(|entry| entry["name"] == name)
        .and_then(|entry| entry["ref"].as_str())
        .unwrap_or_else(|| panic!("no line named {name:?}: {snapshot}"))
        .to_owned()
}

async fn dom_click(f: &Fixture, target: &str, tab: &str, reference: &str) -> Value {
    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "session": SESSION,
            "ref": reference, "input_route": "dom_event"
        }))
        .await;
    structured(&result).clone()
}

#[tokio::test]
async fn a_semantic_ref_keeps_its_name_across_snapshots_of_one_document() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");

    let second = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(
        second["snapshot"]["id"], first["snapshot"]["id"],
        "{second}"
    );
    assert_eq!(named_ref(&second, "Reply"), reply);
    assert_eq!(second["outline"], first["outline"]);

    // The ref read in the first snapshot still acts after the second.
    let clicked = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(clicked["status"], "ok", "{clicked}");
    // It was re-read in the live page first.
    let reread = recorded_calls(&f, "Accessibility.getPartialAXTree");
    assert_eq!(reread.len(), 1);
    assert_eq!(reread[0].1["backendNodeId"], 2011);
}

#[tokio::test]
async fn a_ref_whose_node_became_another_element_is_stale_and_never_renamed() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");

    // The page reuses the button's node for something else.
    f.state
        .lock()
        .unwrap()
        .renamed
        .insert(2011, "Delete thread".into());
    let refused = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");
    assert!(
        recorded_calls(&f, "Runtime.callFunctionOn").is_empty(),
        "nothing was clicked"
    );

    // The next snapshot gives the new element a new ref; the old one stays dead.
    let second = semantic_snapshot(&f, &target, &tab).await;
    assert_eq!(second["snapshot"]["id"], first["snapshot"]["id"]);
    let delete = named_ref(&second, "Delete thread");
    assert_ne!(delete, reply);
    assert_eq!(
        dom_click(&f, &target, &tab, &reply).await["refusal"]["code"],
        "browser_ref_stale"
    );
    assert_eq!(dom_click(&f, &target, &tab, &delete).await["status"], "ok");
    // Its neighbours kept their refs.
    assert_eq!(
        named_ref(&second, "Reply body"),
        named_ref(&first, "Reply body")
    );
}

#[tokio::test]
async fn a_ref_whose_node_left_the_page_is_stale_at_use_and_after_the_next_snapshot() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");

    f.state.lock().unwrap().removed.insert(2011);
    let refused = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");

    let second = semantic_snapshot(&f, &target, &tab).await;
    assert!(
        !second["outline"].as_str().unwrap().contains("\"Reply\""),
        "{second}"
    );
    // Back in the page, the node is a new entity to this session's refs.
    f.state.lock().unwrap().removed.clear();
    let refused = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");
    let third = semantic_snapshot(&f, &target, &tab).await;
    assert_ne!(named_ref(&third, "Reply"), reply);
}

#[tokio::test]
async fn a_new_document_retires_every_semantic_ref() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");

    f.state.lock().unwrap().main_loader = "L_MAIN_2".into();
    let second = semantic_snapshot(&f, &target, &tab).await;
    assert_ne!(
        second["snapshot"]["id"], first["snapshot"]["id"],
        "{second}"
    );
    assert_ne!(
        named_ref(&second, "Reply"),
        reply,
        "same node id, another document"
    );
    assert_eq!(
        dom_click(&f, &target, &tab, &reply).await["refusal"]["code"],
        "browser_ref_stale"
    );
    assert!(recorded_calls(&f, "Runtime.callFunctionOn").is_empty());
}

#[tokio::test]
async fn a_debugger_detach_makes_every_semantic_ref_stale() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");

    // Detached and attached again: the node may well be the same, but the
    // attachment that proved it is gone.
    f.state.lock().unwrap().detached = true;
    let refused = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");
    assert!(
        refused["refusal"]["message"]
            .as_str()
            .unwrap()
            .contains("detached"),
        "{refused}"
    );
    assert!(recorded_calls(&f, "Accessibility.getPartialAXTree").is_empty());
    assert!(recorded_calls(&f, "Runtime.callFunctionOn").is_empty());

    let second = semantic_snapshot(&f, &target, &tab).await;
    assert_ne!(
        second["snapshot"]["id"], first["snapshot"]["id"],
        "{second}"
    );
    assert_eq!(
        dom_click(&f, &target, &tab, &reply).await["refusal"]["code"],
        "browser_ref_stale"
    );
    assert_eq!(
        dom_click(&f, &target, &tab, &named_ref(&second, "Reply")).await["status"],
        "ok"
    );
}

#[tokio::test]
async fn a_dom_refs_snapshot_replaces_the_semantic_refs_and_the_other_way_round() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let semantic = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&semantic, "Reply");

    let legacy = snapshot(&f, &target, &tab).await;
    assert_eq!(legacy["status"], "ok", "{legacy}");
    assert_eq!(
        dom_click(&f, &target, &tab, &reply).await["refusal"]["code"],
        "browser_ref_stale"
    );

    let legacy_ref = legacy["refs"][0]["ref"].as_str().unwrap().to_owned();
    let again = semantic_snapshot(&f, &target, &tab).await;
    assert_ne!(again["snapshot"]["id"], semantic["snapshot"]["id"]);
    assert_eq!(
        dom_click(&f, &target, &tab, &legacy_ref).await["refusal"]["code"],
        "browser_ref_stale"
    );
}

#[tokio::test]
async fn a_query_read_and_a_continuation_use_the_same_refs() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");
    let queried = semantic_snapshot_with(&f, &target, &tab, json!({"query": "Reply"})).await;
    assert_eq!(queried["snapshot"]["id"], first["snapshot"]["id"]);
    assert_eq!(named_ref(&queried, "Reply"), reply);

    // A node the first page did not reach gets its ref from a query, and
    // keeps it when a later default snapshot pages to it.
    let far = semantic_snapshot_with(&f, &target, &tab, json!({"query": "Archive item 304"})).await;
    let far_ref = named_ref(&far, "Archive item 304");
    let fresh = semantic_snapshot(&f, &target, &tab).await;
    let paged = continue_to(&f, &target, &tab, fresh, "Archive item 304").await;
    assert_eq!(named_ref(&paged, "Archive item 304"), far_ref);
    assert_eq!(dom_click(&f, &target, &tab, &far_ref).await["status"], "ok");
}

#[tokio::test]
async fn semantic_snapshot_uses_bounded_dom_fallback_only_for_known_size_failures() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.semantic_full_dom_fails = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot_with(&f, &target, &tab, json!({"query": "Reply"})).await;
    assert_eq!(snap["status"], "ok", "{snap}");
    assert_eq!(snap["snapshot"]["complete"], false, "{snap}");
    let document_calls = recorded_calls(&f, "DOM.getDocument");
    assert!(document_calls
        .iter()
        .any(|(_, params)| params["depth"] == -1));
    assert!(document_calls
        .iter()
        .any(|(_, params)| params["depth"] == 256));
    assert!(document_calls
        .iter()
        .any(|(_, params)| params["depth"] == 8));
}

#[tokio::test]
async fn semantic_snapshot_uses_bounded_dom_fallback_for_full_tree_timeout() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.semantic_full_dom_times_out = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot_with(&f, &target, &tab, json!({"query": "Reply"})).await;
    assert_eq!(snap["status"], "ok", "{snap}");
    assert_eq!(snap["snapshot"]["complete"], false, "{snap}");
    let document_calls = recorded_calls(&f, "DOM.getDocument");
    assert!(document_calls
        .iter()
        .any(|(_, params)| params["depth"] == -1));
    assert!(document_calls
        .iter()
        .any(|(_, params)| params["depth"] == 8));
    assert!(document_calls.iter().all(|(_, params)| {
        params["depth"] == -1 || params["depth"].as_i64().is_some_and(|depth| depth <= 8)
    }));
}

#[tokio::test]
async fn semantic_snapshot_hydrates_truncated_fallback_branches() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.semantic_full_dom_fails = true;
        st.semantic_truncated_dom = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot_with(&f, &target, &tab, json!({"query": "Reply"})).await;
    assert_eq!(snap["status"], "ok", "{snap}");
    assert_eq!(snap["snapshot"]["complete"], false, "{snap}");
    assert!(snap["refs"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|entry| entry["name"] == "Reply")));
    assert!(recorded_calls(&f, "DOM.describeNode")
        .iter()
        .any(|(_, params)| params["backendNodeId"] == 999));
}

#[tokio::test]
async fn unproven_oopif_capability_is_omitted_not_guessed() {
    let f = fixture_with(|st| st.oopif_supported = false).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;

    assert_eq!(
        snap["status"], "ok",
        "capability gap is not an error: {snap}"
    );
    assert_eq!(snap["oopif"]["status"], "unsupported");
    assert_eq!(snap["oopif"]["frames"], 0);
    let frames: Vec<&str> = snap["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["frame"].as_str().unwrap())
        .collect();
    assert_eq!(
        frames,
        vec!["main", "main", "main", "iframe"],
        "OOPIF content omitted, everything provable kept: {snap}"
    );
}

#[tokio::test]
async fn rogue_attach_announcements_are_contained() {
    let f = fixture_with(|st| st.emit_rogue_attach = true).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;

    let oopif_refs: Vec<&Value> = snap["refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["frame"] == "oopif")
        .collect();
    assert_eq!(
        oopif_refs.len(),
        1,
        "only the child attached beneath the proven tab session mints refs: {snap}"
    );
    assert_eq!(snap["oopif"]["frames"], 1);

    // The rogue/popup sessions must never have been spoken to.
    let touched: Vec<String> = f
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter_map(|(s, _, _)| s.clone())
        .collect();
    assert!(
        !touched
            .iter()
            .any(|s| s == "rogue-sess" || s == "popup-sess"),
        "core issued commands to an unproven session: {touched:?}"
    );
}

// ── Mutation routing + frame identity revalidation ──────────────────────────

#[tokio::test]
async fn protected_browser_scope_reproves_live_origin_and_omits_sensitive_url_text() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let args = json!({
        "target_id": target,
        "tab_id": tab,
        "session": SESSION,
        "x": 10,
        "y": 20,
    });

    let first = browser_protected_resource_scope(&f.engine, &args, "browser_click")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["live_origin"], "https://fixture.test");
    assert!(
        !first.to_string().contains("secret"),
        "the resource must not contain a full URL"
    );
    let observation = browser_protected_resource_scope(&f.engine, &args, "get_browser_state")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation["action_class"], "page_observation");
    assert_eq!(first["action_class"], "page_input");

    f.state.lock().unwrap().main_url = "https://bank.example/transfer?secret=one-time-token".into();
    let second = browser_protected_resource_scope(&f.engine, &args, "browser_click")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second["live_origin"], "https://bank.example");
    assert_ne!(
        first, second,
        "cross-origin navigation must rotate the grant scope"
    );
    assert!(
        !second.to_string().contains("one-time-token"),
        "query text must never reach the consent resource"
    );
}

#[tokio::test]
async fn click_routes_oopif_refs_through_the_contained_child_session() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let oopif_ref = ref_of(&snap, "oopif", "ad-input");

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": oopif_ref, "session": SESSION
        }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["frame"], "oopif");
    // Box-model center of backend 100 in the child session's space.
    assert_eq!(s["x"], 1010.0);
    assert_eq!(s["y"], 1005.0);

    let mouse = recorded_calls(&f, "Input.dispatchMouseEvent");
    assert!(!mouse.is_empty());
    assert!(
        mouse
            .iter()
            .all(|(sess, _)| sess.as_deref().unwrap_or("").starts_with("oopif-sess-")),
        "OOPIF input must dispatch on the child session: {mouse:?}"
    );
    let focus_emulation = recorded_calls(&f, "Emulation.setFocusEmulationEnabled");
    assert_eq!(focus_emulation.len(), 2);
    assert_eq!(focus_emulation[0].1["enabled"], true);
    assert_eq!(focus_emulation[1].1["enabled"], false);
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn click_validates_same_process_iframe_loader_and_uses_the_tab_session() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let iframe_ref = ref_of(&snap, "iframe", "inner-btn");

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": iframe_ref, "session": SESSION
        }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["frame"], "iframe");

    let mouse = recorded_calls(&f, "Input.dispatchMouseEvent");
    assert!(
        mouse
            .iter()
            .all(|(sess, _)| sess.as_deref().unwrap_or("").starts_with("tab-sess-")),
        "same-process iframe input stays on the tab session: {mouse:?}"
    );
    assert!(
        !recorded_calls(&f, "Page.getFrameTree").is_empty(),
        "frame identity must have been re-proven"
    );
}

#[tokio::test]
async fn trusted_click_refuses_when_standalone_background_posture_is_unavailable() {
    let f = fixture_with_platform(|_| {}, true).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let main_ref = ref_of(&snap, "main", "main-btn");

    let trusted = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": main_ref,
            "input_route": "trusted", "session": SESSION
        }))
        .await;
    assert_eq!(
        structured(&trusted)["refusal"]["code"],
        "browser_input_trust_unavailable"
    );
    assert_eq!(
        structured(&trusted)["refusal"]["detail"]["alternative_route"],
        "dom_event"
    );
    assert_eq!(
        structured(&trusted)["refusal"]["detail"]["trusted_delivery_attempted"],
        false
    );
    assert!(recorded_calls(&f, "Input.dispatchMouseEvent").is_empty());

    let synthetic_args = json!({
        "target_id": target, "tab_id": tab, "ref": main_ref,
        "input_route": "dom_event", "session": SESSION
    });
    let synthetic = BrowserClickTool::new(f.engine.clone())
        .invoke(synthetic_args.clone())
        .await;
    assert_eq!(structured(&synthetic)["status"], "ok");
    assert_eq!(structured(&synthetic)["effect"], "unverifiable");
    assert!(structured(&synthetic).get("escalation").is_none());
    assert!(synthetic.content.iter().any(|content| matches!(
        content,
        crate::protocol::Content::Text { text, .. } if text.contains("get_browser_state")
    )));
    assert!(synthetic.content.iter().any(|content| matches!(
        content,
        crate::protocol::Content::Text { text, .. }
            if text.contains("application effect not verified")
                && text.contains("trust-gated controls")
    )));
    let public = ActionExecutionRecord::from_legacy(
        "browser_click",
        &synthetic_args,
        structured(&synthetic),
    )
    .expect("browser click action record")
    .public_result()
    .expect("public browser click result");
    let public = serde_json::to_value(public).expect("serialize public result");
    assert_eq!(public["effect"], "unverifiable", "{public}");
    assert_eq!(public["route"], "dom", "{public}");
    assert_eq!(public["delivery"]["mode"], "background", "{public}");
    // No dead-end escalation; the summary names get_browser_state instead.
    assert!(public.get("escalation").is_none(), "{public}");
    assert!(public.get("status").is_none(), "{public}");
    assert!(!recorded_calls(&f, "Runtime.callFunctionOn").is_empty());
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn main_frame_navigation_invalidates_refs_via_loader_identity() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let main_ref = ref_of(&snap, "main", "main-btn");

    f.state.lock().unwrap().main_loader = "L_MAIN_2".into();

    let click = BrowserClickTool::new(f.engine.clone());
    let result = click
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": main_ref, "session": SESSION
        }))
        .await;
    assert_eq!(
        structured(&result)["refusal"]["code"],
        "browser_ref_stale",
        "loader change means a new document"
    );
    assert!(
        recorded_calls(&f, "Input.dispatchMouseEvent").is_empty(),
        "no input may reach a document that cannot be re-proven"
    );

    // The whole snapshot namespace was invalidated, so a retry refuses
    // at resolution already.
    let retry = click
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": main_ref, "session": SESSION
        }))
        .await;
    assert_eq!(structured(&retry)["refusal"]["code"], "browser_ref_stale");
}

#[tokio::test]
async fn same_process_iframe_navigation_invalidates_its_refs() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let iframe_ref = ref_of(&snap, "iframe", "inner-btn");

    f.state.lock().unwrap().iframe_loader = "L_IFRAME_2".into();

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": iframe_ref, "session": SESSION
        }))
        .await;
    assert_eq!(structured(&result)["refusal"]["code"], "browser_ref_stale");
    assert!(recorded_calls(&f, "Input.dispatchMouseEvent").is_empty());
}

#[tokio::test]
async fn oopif_navigation_invalidates_its_refs() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let oopif_ref = ref_of(&snap, "oopif", "ad-input");

    f.state.lock().unwrap().oopif_loader = "L_OOPIF_2".into();

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": oopif_ref, "session": SESSION
        }))
        .await;
    assert_eq!(structured(&result)["refusal"]["code"], "browser_ref_stale");
    assert!(recorded_calls(&f, "Input.dispatchMouseEvent").is_empty());
}

#[tokio::test]
async fn removed_oopif_frame_is_stale_not_guessed() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let oopif_ref = ref_of(&snap, "oopif", "ad-input");

    f.state.lock().unwrap().oopif_present = false;

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": oopif_ref, "session": SESSION
        }))
        .await;
    assert_eq!(structured(&result)["refusal"]["code"], "browser_ref_stale");
    assert!(recorded_calls(&f, "Input.dispatchMouseEvent").is_empty());
}

#[tokio::test]
async fn oopif_capability_regression_refuses_rather_than_reroutes() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let oopif_ref = ref_of(&snap, "oopif", "ad-input");

    f.state.lock().unwrap().oopif_supported = false;

    let result = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": oopif_ref, "session": SESSION
        }))
        .await;
    assert_eq!(
        structured(&result)["refusal"]["code"],
        "browser_route_unavailable",
        "a lost capability is a refusal, never a fallback to the wrong session"
    );
    assert!(recorded_calls(&f, "Input.dispatchMouseEvent").is_empty());
}

// ── Typing routes ────────────────────────────────────────────────────────────

#[tokio::test]
async fn typing_into_composed_shadow_input_uses_the_tab_session() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let shadow_ref = ref_of(&snap, "main", "Shadow Input");

    let result = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": shadow_ref,
            "text": "hi", "session": SESSION
        }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["frame"], "main");

    let inserts = recorded_calls(&f, "Input.insertText");
    assert_eq!(inserts.len(), 1);
    assert!(inserts[0].0.as_deref().unwrap().starts_with("tab-sess-"));
    assert_eq!(inserts[0].1["text"], "hi");
    assert!(
        !recorded_calls(&f, "Runtime.callFunctionOn").is_empty(),
        "editability must be checked on the node itself"
    );
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn typing_reports_what_the_field_holds_afterwards() {
    let f = fixture_with(|state| state.field_value = Some("ada".into())).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let input = ref_of(&snap, "main", "Shadow Input");
    let typed = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": input,
            "text": "@x.io", "session": SESSION
        }))
        .await;
    let s = structured(&typed);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(s["value"], "ada@x.io");
    assert_eq!(s["evidence"][0]["kind"], "browser_readback");
}

#[tokio::test]
async fn keystrokes_judge_the_caret_where_focus_left_it() {
    // "old" with the caret at 0; once focus is emulated the page's focus
    // handler moves it to the end, so the keys land after "old".
    let f = fixture_with(|state| {
        state.field_value = Some("old".into());
        state.field_caret = Some(0);
        state.field_focus_moves_caret_to_end = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let input = ref_of(&snap, "main", "Shadow Input");
    let typed = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": input,
            "text": "X", "mode": "keystrokes", "session": SESSION
        }))
        .await;
    let s = structured(&typed);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(s["value"], "oldX");
}

#[tokio::test]
async fn a_field_that_rejects_input_is_reported_as_a_mismatch() {
    let f = fixture_with(|state| {
        state.field_value = Some(String::new());
        state.field_digits_only = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let input = ref_of(&snap, "main", "Shadow Input");
    let typed = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": input,
            "text": "12ab34", "session": SESSION
        }))
        .await;
    assert_eq!(typed.is_error, Some(true));
    let s = structured(&typed);
    assert_eq!(s["code"], "browser_type_mismatch", "{s}");
    assert_eq!(s["effect"], "mismatch");
    assert_eq!(s["value"], "1234");
    // The fixture field reports no selection (like an email input), so no
    // single expected value is claimed.
    assert!(s["expected"].is_null(), "{s}");
}

#[tokio::test]
async fn a_field_the_page_replaced_is_unverifiable_not_confirmed() {
    let f = fixture_with(|state| {
        state.field_value = Some(String::new());
        state.field_detached_after_input = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let input = ref_of(&snap, "main", "Shadow Input");
    let typed = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": input,
            "text": "ada", "session": SESSION
        }))
        .await;
    let s = structured(&typed);
    assert_eq!(s["effect"], "unverifiable", "{s}");
    assert_eq!(s["readback"], "element_replaced");
}

#[tokio::test]
async fn set_value_uses_the_native_setter_and_verifies() {
    let f = fixture_with(|state| state.field_value = Some("old".into())).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let input = ref_of(&snap, "main", "Shadow Input");
    let set = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": input,
            "text": "new", "mode": "set_value", "session": SESSION
        }))
        .await;
    let s = structured(&set);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["effect"], "confirmed");
    assert_eq!(s["value"], "new");
    assert_eq!(s["replaced_chars"], 3);
    assert!(recorded_calls(&f, "Input.insertText").is_empty());
    assert!(recorded_calls(&f, "Runtime.callFunctionOn")
        .iter()
        .any(|(_, params)| {
            params["arguments"][0]["value"] == "new"
                && params["functionDeclaration"]
                    .as_str()
                    .unwrap()
                    .contains("dispatchEvent(new view.Event('input'")
        }));
}

#[tokio::test]
async fn typing_into_an_oopif_input_routes_to_the_child_session() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let oopif_ref = ref_of(&snap, "oopif", "ad-input");

    let result = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target, "tab_id": tab, "ref": oopif_ref,
            "text": "q", "session": SESSION
        }))
        .await;
    let s = structured(&result);
    assert_eq!(s["status"], "ok", "{s}");
    assert_eq!(s["frame"], "oopif");

    let inserts = recorded_calls(&f, "Input.insertText");
    assert_eq!(inserts.len(), 1);
    assert!(
        inserts[0].0.as_deref().unwrap().starts_with("oopif-sess-"),
        "typing must land in the contained child session: {inserts:?}"
    );
    let focuses = recorded_calls(&f, "DOM.focus");
    assert!(focuses
        .iter()
        .all(|(sess, _)| sess.as_deref().unwrap().starts_with("oopif-sess-")));
}

#[tokio::test]
async fn partial_keystrokes_report_exact_delivered_prefix() {
    let f = fixture_with(|state| state.fail_key_down_after = Some(2)).await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let shadow_ref = ref_of(&snap, "main", "Shadow Input");

    let result = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "ref": shadow_ref,
            "text": "four",
            "mode": "keystrokes",
            "session": SESSION
        }))
        .await;
    let refusal = &structured(&result)["refusal"];
    assert_eq!(refusal["code"], "browser_input_incomplete");
    assert_eq!(refusal["detail"]["requested_chars"], 4);
    assert_eq!(refusal["detail"]["delivered_chars"], 2);
    assert_eq!(refusal["detail"]["retryable"], false);
}

#[tokio::test]
async fn keystrokes_use_char_events_for_text_delivery() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let shadow_ref = ref_of(&snap, "main", "Shadow Input");

    let result = BrowserTypeTool::new(f.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "ref": shadow_ref,
            "text": "a\n",
            "mode": "keystrokes",
            "session": SESSION
        }))
        .await;
    assert_eq!(structured(&result)["status"], "ok");

    let events = recorded_calls(&f, "Input.dispatchKeyEvent");
    assert_eq!(events.len(), 6);
    let params: Vec<&Value> = events.iter().map(|(_, params)| params).collect();
    assert_eq!(params[0]["type"], "keyDown");
    assert!(params[0].get("text").is_none());
    assert_eq!(params[1]["type"], "char");
    assert_eq!(params[1]["text"], "a");
    assert_eq!(params[1]["unmodifiedText"], "a");
    assert_eq!(params[2]["type"], "keyUp");
    assert!(params[2].get("text").is_none());
    assert_eq!(params[3]["type"], "keyDown");
    assert_eq!(params[3]["key"], "Enter");
    assert_eq!(params[4]["type"], "char");
    assert_eq!(params[4]["key"], "Enter");
    assert_eq!(params[4]["text"], "\r");
    assert_eq!(params[4]["unmodifiedText"], "\r");
    assert_eq!(params[5]["type"], "keyUp");
    let focus_emulation = recorded_calls(&f, "Emulation.setFocusEmulationEnabled");
    assert_eq!(focus_emulation.len(), 2);
    assert_eq!(focus_emulation[0].1["enabled"], true);
    assert_eq!(focus_emulation[1].1["enabled"], false);
    let readiness_checks = recorded_calls(&f, "Runtime.callFunctionOn");
    assert!(readiness_checks.iter().any(|(_, params)| {
        params["functionDeclaration"]
            .as_str()
            .is_some_and(|declaration| {
                declaration.contains("document.hasFocus()")
                    && declaration.contains("active === this")
            })
    }));
    assert!(recorded_calls(&f, "Page.bringToFront").is_empty());
    assert!(recorded_calls(&f, "Target.activateTarget").is_empty());
}

#[tokio::test]
async fn semantic_link_urls_reach_query_and_continuation_outputs() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.semantic_link_urls = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = first["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Reply")
        .unwrap();
    assert_eq!(reply["url"], "https://example.test/book?slot=1#court");
    assert!(reply["value"].is_null());
    let token = first["snapshot"]["continuation"].as_str().unwrap();
    let continued = semantic_snapshot_with(&f, &target, &tab, json!({"continuation":token})).await;
    let continued = continue_to(&f, &target, &tab, continued, "Archive item 304").await;
    let archive = continued["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Archive item 304")
        .unwrap();
    assert_eq!(archive["url"], "https://example.test/book?slot=1#court");
    let queried = semantic_snapshot_with(&f, &target, &tab, json!({"query":"Reply"})).await;
    let reply = queried["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Reply")
        .unwrap();
    assert_eq!(reply["url"], "https://example.test/book?slot=1#court");
}

// ── Page changes in action results (through the real registry) ──────────────
//
// The tools run inside a ToolRegistry here, as they do in the daemon: the
// read after an action is a get_browser_state call the registry dispatches.

fn unrestricted() -> Arc<crate::session_authorization::EffectiveAuthorizationContext> {
    use crate::authorization::PermissionMode;
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Unrestricted],
            true,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(PermissionMode::Unrestricted, None)
    .unwrap()
}

/// A registry with the browser tools of one fixture, bound to a tab, and a
/// session label of its own.
struct Agent {
    registry: Arc<crate::tool::ToolRegistry>,
    context: Arc<crate::session_authorization::EffectiveAuthorizationContext>,
    session: String,
    target: String,
    tab: String,
    /// What the bind returned: the binding, and its read of the active tab.
    bound: Value,
}

impl Agent {
    async fn bound(f: &Fixture, session: &str) -> Self {
        Self::bound_as(f, session, unrestricted()).await
    }

    async fn bound_as(
        f: &Fixture,
        session: &str,
        context: Arc<crate::session_authorization::EffectiveAuthorizationContext>,
    ) -> Self {
        let mut registry = crate::tool::ToolRegistry::new();
        super::tools::register_browser_tools(&f.engine, &mut registry);
        let registry = Arc::new(registry);
        registry.init_self_weak();
        let mut agent = Self {
            registry,
            context,
            session: session.to_owned(),
            target: String::new(),
            tab: String::new(),
            bound: Value::Null,
        };
        let bound = agent
            .call("get_browser_state", json!({ "pid": 1, "window_id": 7 }))
            .await;
        assert_eq!(bound["status"], "ok", "{bound}");
        agent.target = bound["target_id"].as_str().unwrap().to_owned();
        agent.tab = bound["tabs"][0]["tab_id"].as_str().unwrap().to_owned();
        agent.bound = bound;
        agent
    }

    /// Call a tool on the bound tab; returns its structured content.
    async fn call(&self, name: &str, mut args: Value) -> Value {
        let object = args.as_object_mut().unwrap();
        object.insert("session".into(), json!(self.session));
        if !self.target.is_empty() && !object.contains_key("pid") {
            object.insert("target_id".into(), json!(self.target));
            object.insert("tab_id".into(), json!(self.tab));
        }
        let result = self
            .registry
            .invoke_with_context(name, args, self.context.clone())
            .await;
        result.structured_content.unwrap_or_else(|| {
            panic!(
                "{name} returned no structured content: {:?}",
                result.content
            )
        })
    }

    async fn snapshot(&self) -> Value {
        with_outline_entries(self.call("get_browser_state", json!({})).await)
    }
}

/// Apply a diff's keyed ops to an outline, as an agent would.
fn apply_changes(outline: &str, changes: &Value) -> String {
    let mut lines: Vec<(String, String)> = outline
        .lines()
        .map(|line| {
            (
                parse_outline_line(line)["ref"].as_str().unwrap().to_owned(),
                line.to_owned(),
            )
        })
        .collect();
    let ops = changes["ops"].as_array().expect("diff ops");
    for op in ops {
        let key = op["ref"].as_str().unwrap();
        match op["op"].as_str().unwrap() {
            "leave" | "move" => lines.retain(|(held, _)| held != key),
            "change" => {
                lines
                    .iter_mut()
                    .find(|(held, _)| held == key)
                    .expect("changed line")
                    .1 = op["line"].as_str().unwrap().to_owned()
            }
            _ => {}
        }
    }
    for op in ops {
        if matches!(op["op"].as_str(), Some("add" | "move")) {
            let at = match op["after"].as_str() {
                None => 0,
                Some(after) => {
                    lines
                        .iter()
                        .position(|(held, _)| held == after)
                        .expect("anchor")
                        + 1
                }
            };
            lines.insert(
                at,
                (
                    op["ref"].as_str().unwrap().to_owned(),
                    op["line"].as_str().unwrap().to_owned(),
                ),
            );
        }
    }
    lines
        .into_iter()
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Two outlines of one page state agree line for line up to where the size
/// budget cut the shorter one. (A diffed read stops where its baseline was
/// cut; a plain read is cut afresh.)
fn assert_same_page(applied: &str, fresh: &str) {
    let (shorter, longer) = if applied.len() <= fresh.len() {
        (applied, fresh)
    } else {
        (fresh, applied)
    };
    assert!(
        longer.starts_with(shorter) && shorter.lines().count() > 3,
        "the diff does not lead to the page as read:\n{applied}\n--- fresh:\n{fresh}"
    );
}

#[tokio::test]
async fn typing_returns_the_changed_line_as_a_diff_from_the_held_revision() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
    })
    .await;
    let agent = Agent::bound(&f, "changes-type").await;
    let first = agent.snapshot().await;
    let editor = named_ref(&first, "Reply body");

    let typed = agent
        .call("browser_type", json!({ "ref": editor, "text": "hello" }))
        .await;
    assert_eq!(typed["effect"], "confirmed", "{typed}");
    let changes = &typed["changes"];
    assert_eq!(changes["kind"], "diff", "{typed}");
    assert_eq!(
        changes["base_revision"], first["snapshot"]["revision"],
        "{typed}"
    );
    assert_eq!(changes["snapshot_id"], first["snapshot"]["id"]);
    let ops = changes["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "{typed}");
    assert_eq!(ops[0]["op"], "change");
    assert_eq!(ops[0]["ref"], editor);
    assert_eq!(
        ops[0]["line"],
        format!("- textbox \"Reply body\" [{editor} type] = \"hello\"")
    );
    // The whole result is small: one changed line, not a page.
    assert!(typed.to_string().chars().count() < 1_500, "{typed}");

    // Applying the diff to the outline held gives what a fresh read shows.
    let fresh = agent.snapshot().await;
    assert_same_page(
        &apply_changes(first["outline"].as_str().unwrap(), changes),
        fresh["outline"].as_str().unwrap(),
    );
    assert!(fresh["snapshot"]["revision"].as_u64() > changes["revision"].as_u64());
}

#[tokio::test]
async fn a_click_reports_gone_and_new_elements_and_what_the_page_did_by_itself() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "changes-click").await;
    let first = agent.snapshot().await;
    let reply = named_ref(&first, "Reply");

    {
        let mut state = f.state.lock().unwrap();
        // Before the action, by itself, the page dropped a visible line.
        state.removed.insert(2003);
        // The click turns the Reply button into a Sent button.
        state.click_renames.push((2011, "Sent".into()));
    }
    let clicked = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    let changes = &clicked["changes"];
    assert_eq!(changes["kind"], "diff", "{clicked}");
    let ops = changes["ops"].as_array().unwrap();
    let gone: Vec<&str> = ops
        .iter()
        .filter(|op| op["op"] == "leave" && op["gone"] == true)
        .map(|op| op["ref"].as_str().unwrap())
        .collect();
    assert!(
        gone.contains(&reply.as_str()),
        "the old button is gone: {clicked}"
    );
    assert_eq!(
        gone.len(),
        2,
        "and so is the line the page dropped by itself: {clicked}"
    );
    let added: Vec<&Value> = ops.iter().filter(|op| op["op"] == "add").collect();
    assert_eq!(added.len(), 1, "{clicked}");
    assert!(added[0]["line"]
        .as_str()
        .unwrap()
        .contains("button \"Sent\""));
    assert_ne!(added[0]["ref"], reply, "another element, another ref");

    let fresh = agent.snapshot().await;
    assert_same_page(
        &apply_changes(first["outline"].as_str().unwrap(), changes),
        fresh["outline"].as_str().unwrap(),
    );
    // The new ref acts; the old one is stale.
    let sent = added[0]["ref"].as_str().unwrap();
    let again = agent
        .call(
            "browser_click",
            json!({ "ref": sent, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(again["changes"]["kind"], "diff", "{again}");
    assert_eq!(
        again["changes"]["ops"],
        json!([]),
        "nothing changed this time"
    );
    let stale = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(stale["error"]["code"], "browser_ref_stale", "{stale}");
}

#[tokio::test]
async fn navigation_returns_the_new_page_as_a_full_snapshot_with_the_reason() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "changes-navigate").await;
    let first = agent.snapshot().await;
    let reply = named_ref(&first, "Reply");

    {
        // The browser publishes the new document when Page.navigate lands.
        let mut state = f.state.lock().unwrap();
        state.main_loader = "L_MAIN_2".into();
        state.main_url = "https://fixture.test/second".into();
        set_page_title(&mut state, Some("Second page"));
    }
    let navigated = agent
        .call(
            "browser_navigate",
            json!({ "url": "https://fixture.test/second" }),
        )
        .await;
    assert_eq!(navigated["status"], "ok", "{navigated}");
    let changes = &navigated["changes"];
    assert_eq!(changes["kind"], "snapshot", "{navigated}");
    assert_eq!(changes["reason"], "document_changed");
    assert_eq!(changes["url"], "https://fixture.test/second");
    assert_eq!(changes["title"], "Second page");
    assert!(changes["outline"]
        .as_str()
        .unwrap()
        .contains("button \"Reply\""));
    assert_ne!(changes["snapshot_id"], first["snapshot"]["id"]);

    let stale = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(stale["error"]["code"], "browser_ref_stale", "{stale}");
    // The snapshot in the result is a baseline like any other.
    let next = with_outline_entries(json!({ "outline": changes["outline"] }));
    let clicked = agent
        .call(
            "browser_click",
            json!({ "ref": named_ref(&next, "Reply"), "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(clicked["changes"]["kind"], "diff", "{clicked}");
    assert_eq!(clicked["changes"]["base_revision"], changes["revision"]);
}

#[tokio::test]
async fn an_action_with_nothing_held_returns_a_full_snapshot() {
    // A bind that could not prove which tab the window shows read none.
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.tab_title = "Another title".into();
    })
    .await;
    let agent = Agent::bound(&f, "changes-nothing-held").await;
    let clicked = agent
        .call("browser_click", json!({ "x": 30, "y": 40 }))
        .await;
    assert_eq!(clicked["changes"]["kind"], "snapshot", "{clicked}");
    assert_eq!(clicked["changes"]["reason"], "no_baseline");
    assert!(clicked["changes"]["outline"]
        .as_str()
        .unwrap()
        .contains("Reply body"));
}

#[tokio::test]
async fn a_session_on_dom_refs_gets_no_changes_and_keeps_its_refs() {
    let f = fixture().await;
    let agent = Agent::bound(&f, "changes-dom-refs").await;
    // The bind read the page once, semantically; nothing reads it so again.
    let semantic_reads = recorded_calls(&f, "Accessibility.getFullAXTree").len();
    let legacy = agent
        .call(
            "get_browser_state",
            json!({ "snapshot_format": "dom_refs_v1" }),
        )
        .await;
    let button = ref_of(&legacy, "main", "main-btn");
    for _ in 0..2 {
        let clicked = agent.call("browser_click", json!({ "ref": button })).await;
        assert_eq!(clicked["route"], "trusted_input", "{clicked}");
        assert!(clicked.get("changes").is_none(), "{clicked}");
    }
    assert_eq!(
        recorded_calls(&f, "Accessibility.getFullAXTree").len(),
        semantic_reads
    );
}

#[tokio::test]
async fn a_dialog_the_click_opened_is_reported_with_its_capability_and_nothing_else_moves() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "changes-dialog").await;
    let first = agent.snapshot().await;
    let reply = named_ref(&first, "Reply");

    f.state.lock().unwrap().click_opens_dialog = true;
    let started = std::time::Instant::now();
    let clicked = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "no timeout was waited out"
    );
    let changes = &clicked["changes"];
    assert_eq!(changes["kind"], "unavailable", "{clicked}");
    assert_eq!(changes["reason"], "javascript_dialog_open");
    assert_eq!(changes["dialog"]["kind"], "alert");
    let dialog_id = changes["dialog"]["dialog_id"].as_str().unwrap().to_owned();

    // While it is up, reads and input say so at once instead of hanging.
    let read = agent.call("get_browser_state", json!({})).await;
    assert_eq!(read["refusal"]["code"], "browser_dialog_open", "{read}");
    assert_eq!(read["refusal"]["detail"]["dialog_id"], dialog_id);
    let blocked = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(blocked["error"]["code"], "browser_dialog_open", "{blocked}");
    assert!(blocked["error"]["hint"]
        .as_str()
        .unwrap()
        .contains(&dialog_id));

    // The capability resolves it, and the baseline held before still diffs.
    let accepted = agent
        .call(
            "browser_dialog",
            json!({ "action": "accept", "dialog_id": dialog_id }),
        )
        .await;
    assert_eq!(accepted["status"], "ok", "{accepted}");
    let after = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(after["changes"]["kind"], "diff", "{after}");
    assert_eq!(
        after["changes"]["base_revision"],
        first["snapshot"]["revision"]
    );
}

#[tokio::test]
async fn since_revision_answers_with_a_diff_only_from_the_revision_held() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "changes-since").await;
    let first = agent.snapshot().await;
    let revision = first["snapshot"]["revision"].as_u64().unwrap();

    f.state
        .lock()
        .unwrap()
        .renamed
        .insert(2003, "Edited message".into());
    let changed = agent
        .call("get_browser_state", json!({ "since_revision": revision }))
        .await;
    assert_eq!(changed["mode"], "changes", "{changed}");
    assert_eq!(changed["changes"]["kind"], "diff");
    // Text that says something else: the same ref, a changed line.
    assert_eq!(changed["changes"]["ops"][0]["op"], "change", "{changed}");
    assert_eq!(
        changed["changes"]["ops"][0]["ref"],
        named_ref(&first, "Please review the attached fixture report.")
    );

    // The revision the agent held is no longer the baseline.
    let stale = agent
        .call("get_browser_state", json!({ "since_revision": revision }))
        .await;
    assert_eq!(stale["changes"]["kind"], "snapshot", "{stale}");
    assert_eq!(stale["changes"]["reason"], "revision_unknown");
    assert!(stale["changes"]["outline"]
        .as_str()
        .unwrap()
        .contains("Edited message"));

    let refused = agent
        .registry
        .invoke_with_context(
            "get_browser_state",
            json!({ "target_id": agent.target, "tab_id": agent.tab, "session": agent.session,
                "since_revision": revision, "query": "Reply" }),
            agent.context.clone(),
        )
        .await;
    assert_eq!(
        refused.is_error,
        Some(true),
        "a diff is of the whole-page view only"
    );
}

// ── browser_steps against the scripted page ─────────────────────────────────

#[tokio::test]
async fn steps_run_as_single_tools_and_return_one_diff_for_the_batch() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
    })
    .await;
    let agent = Agent::bound(&f, "steps-real").await;
    let first = agent.snapshot().await;
    f.state
        .lock()
        .unwrap()
        .click_renames
        .push((2011, "Sent".into()));
    let reads_before = recorded_calls(&f, "Accessibility.getFullAXTree").len();

    let output = agent
        .call(
            "browser_steps",
            json!({ "steps": [
                {"action": "type", "ref": named_ref(&first, "Reply body"), "text": "hello"},
                {"action": "click", "role": "button", "name": "Reply", "input_route": "dom_event",
                 "expect": {"role": "button", "name": "Sent"}},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "completed", "{output}");
    let outcomes = output["steps"].as_array().unwrap();
    assert_eq!(outcomes[0]["effect"], "confirmed", "{output}");
    assert_eq!(
        outcomes[1]["ref"],
        named_ref(&first, "Reply"),
        "resolved to the ref held"
    );

    // One diff, from the revision held before the batch, with both steps in it.
    let changes = &output["changes"];
    assert_eq!(changes["kind"], "diff", "{output}");
    assert_eq!(changes["base_revision"], first["snapshot"]["revision"]);
    let ops = changes["ops"].as_array().unwrap();
    assert!(
        ops.iter()
            .any(|op| op["op"] == "change" && op["line"].as_str().unwrap().contains("= \"hello\"")),
        "{output}"
    );
    assert!(ops.iter().any(|op| op["op"] == "add"
        && op["line"].as_str().unwrap().contains("button \"Sent\"")), "{output}");
    assert!(output.to_string().chars().count() < 1_500, "{output}");

    // The steps did not each read the page: one read to aim the named step,
    // one for its expect, one at the end. Each main-frame read is two trees.
    let reads = recorded_calls(&f, "Accessibility.getFullAXTree")
        .into_iter()
        .skip(reads_before)
        .filter(|(_, params)| params["frameId"] == "F_MAIN")
        .count();
    assert_eq!(reads, 3, "aim, expect, final");
    // Both steps re-read their own node before acting on it.
    assert_eq!(
        recorded_calls(&f, "Accessibility.getPartialAXTree").len(),
        2
    );
}

#[tokio::test]
async fn unconfirmed_typing_stops_a_real_batch_before_the_click() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
        st.field_detached_after_input = true;
    })
    .await;
    let agent = Agent::bound(&f, "steps-real-unconfirmed").await;
    let first = agent.snapshot().await;
    let output = agent
        .call(
            "browser_steps",
            json!({ "steps": [
                {"action": "type", "ref": named_ref(&first, "Reply body"), "text": "hello"},
                {"action": "click", "ref": named_ref(&first, "Reply"), "input_route": "dom_event"},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "stopped", "{output}");
    assert_eq!(output["stop_reason"], "typing_unconfirmed");
    assert_eq!(output["steps"].as_array().unwrap().len(), 1);
    assert_eq!(output["steps"][0]["status"], "unconfirmed");
    assert!(
        recorded_calls(&f, "Runtime.callFunctionOn")
            .iter()
            .all(|(_, params)| !params["functionDeclaration"]
                .as_str()
                .unwrap_or_default()
                .contains("this.click()")),
        "the button was never clicked"
    );
    assert_eq!(output["changes"]["kind"], "diff", "{output}");
}

#[tokio::test]
async fn a_dialog_stops_a_real_batch_and_the_capability_resolves_it() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "steps-real-dialog").await;
    let first = agent.snapshot().await;
    f.state.lock().unwrap().click_opens_dialog = true;
    let output = agent
        .call(
            "browser_steps",
            json!({ "steps": [
                {"action": "click", "ref": named_ref(&first, "Reply"), "input_route": "dom_event"},
                {"action": "click", "ref": named_ref(&first, "Archive item 0"), "input_route": "dom_event"},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "stopped", "{output}");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(1), &json!("javascript_dialog_open"))
    );
    let dialog_id = output["changes"]["dialog"]["dialog_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let accepted = agent
        .call(
            "browser_dialog",
            json!({ "action": "accept", "dialog_id": dialog_id }),
        )
        .await;
    assert_eq!(accepted["status"], "ok", "{accepted}");
}

#[cfg(feature = "yaml")]
#[tokio::test]
async fn every_step_and_read_of_a_batch_is_admitted_as_its_own_tool() {
    use crate::authorization::PermissionMode;
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    // One runtime, so a session keeps its binding when its manifest narrows.
    let runtime = SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Bounded],
            false,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .unwrap(),
    );
    let allowing = |tools: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.yaml");
        std::fs::write(
            &path,
            format!(
                "version: 3\nexpires_after: 1h\nidle_timeout: 30m\nallow:\n  tools: [{tools}]\n\
                 resources:\n  desktop:\n    windows:\n      - pid: 1\n        window_id: 7\n  \
                 browser:\n    origins: [\"https://fixture.test\"]\n"
            ),
        )
        .unwrap();
        let manifest = Arc::new(crate::session_manifest::load_manifest(&path).unwrap());
        runtime
            .compatibility_context(PermissionMode::Bounded, Some(manifest))
            .unwrap()
    };
    let everything = "get_browser_state, browser_steps, browser_click, browser_type";

    // The batch tool is allowed, typing is not: the typing step is refused
    // exactly as a browser_type call would be, and nothing after it runs.
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
    })
    .await;
    let mut agent = Agent::bound_as(&f, "steps-no-type", allowing(everything)).await;
    let first = agent.snapshot().await;
    assert_eq!(first["status"], "ok", "{first}");
    agent.context = allowing("get_browser_state, browser_steps, browser_click");
    let output = agent
        .call(
            "browser_steps",
            json!({ "steps": [
                {"action": "click", "ref": named_ref(&first, "Reply"), "input_route": "dom_event"},
                {"action": "type", "ref": named_ref(&first, "Reply body"), "text": "hello"},
                {"action": "click", "ref": named_ref(&first, "Archive item 0"), "input_route": "dom_event"},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "stopped", "{output}");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("step_failed"))
    );
    assert_eq!(output["steps"][0]["status"], "ok", "{output}");
    assert_eq!(output["steps"][1]["code"], "permission_denied", "{output}");
    assert!(
        output["steps"][1].get("retryable").is_none(),
        "nothing was typed"
    );
    assert!(recorded_calls(&f, "Input.insertText").is_empty());
    assert_eq!(output["steps"].as_array().unwrap().len(), 2);
    assert_eq!(
        output["changes"]["kind"], "diff",
        "reading is allowed here: {output}"
    );

    // Steps and clicks allowed, reading not: a step aimed by role and name
    // cannot be aimed, and what the batch changed cannot be told. No read
    // slips through because it happened inside a batch.
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let mut agent = Agent::bound_as(&f, "steps-no-read", allowing(everything)).await;
    let first = agent.snapshot().await;
    let reply = named_ref(&first, "Reply");
    agent.context = allowing("browser_steps, browser_click");
    let trees = recorded_calls(&f, "Accessibility.getFullAXTree").len();
    let output = agent
        .call(
            "browser_steps",
            json!({ "steps": [
                {"action": "click", "ref": reply, "input_route": "dom_event"},
                {"action": "click", "role": "button", "name": "Archive item 0", "input_route": "dom_event"},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "stopped", "{output}");
    assert_eq!(output["steps"][0]["status"], "ok", "{output}");
    assert_eq!(output["steps"][1]["code"], "permission_denied", "{output}");
    assert_eq!(output["changes"]["kind"], "unavailable", "{output}");
    assert_eq!(output["changes"]["reason"], "permission_denied");
    assert_eq!(
        recorded_calls(&f, "Accessibility.getFullAXTree").len(),
        trees,
        "the page was not read at all"
    );

    // And the batch tool itself is a tool like any other.
    agent.context = allowing("get_browser_state, browser_click, browser_type");
    let refused = agent
        .call(
            "browser_steps",
            json!({ "steps": [{"action": "click", "ref": reply}] }),
        )
        .await;
    assert_eq!(refused["refusal"]["code"], "permission_denied", "{refused}");
}

#[tokio::test]
async fn a_step_that_navigates_stops_the_batch_and_the_result_is_the_new_page() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "steps-real-navigation").await;
    let first = agent.snapshot().await;
    {
        let mut state = f.state.lock().unwrap();
        state.click_navigates = Some("L_MAIN_2".into());
        state.renamed.insert(2000, "The next page".into());
    }
    let output = agent
        .call(
            "browser_steps",
            json!({ "steps": [
                {"action": "click", "ref": named_ref(&first, "Reply"), "input_route": "dom_event"},
                // On the new page there is a button of this name too; the
                // step was planned against the old one and must not reach it.
                {"action": "click", "role": "button", "name": "Archive item 0", "input_route": "dom_event"},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "stopped", "{output}");
    assert_eq!(
        (&output["stopped_at"], &output["stop_reason"]),
        (&json!(2), &json!("document_changed"))
    );
    assert_eq!(output["steps"].as_array().unwrap().len(), 1);
    let clicks = recorded_calls(&f, "Runtime.callFunctionOn")
        .iter()
        .filter(|(_, params)| {
            params["functionDeclaration"]
                .as_str()
                .unwrap_or_default()
                .contains("this.click()")
        })
        .count();
    assert_eq!(clicks, 1, "only the first step clicked");
    let changes = &output["changes"];
    assert_eq!(changes["kind"], "snapshot", "{output}");
    assert_eq!(changes["reason"], "document_changed");
    assert!(changes["outline"]
        .as_str()
        .unwrap()
        .contains("The next page"));
    assert_ne!(changes["snapshot_id"], first["snapshot"]["id"]);
}

#[tokio::test]
async fn a_single_click_that_navigates_returns_the_new_page_not_a_diff_of_the_old() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "changes-click-navigates").await;
    let first = agent.snapshot().await;
    f.state.lock().unwrap().click_navigates = Some("L_MAIN_2".into());
    let clicked = agent
        .call(
            "browser_click",
            json!({ "ref": named_ref(&first, "Reply"), "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(clicked["changes"]["kind"], "snapshot", "{clicked}");
    assert_eq!(clicked["changes"]["reason"], "document_changed");
}

// ── Hit-test before a trusted click ─────────────────────────────────────────

fn covered() -> Value {
    json!({"connected": true, "hit": true, "inside_target": false,
        "contains_target": false, "label_of_target": false, "own_indicator": false})
}

#[tokio::test]
async fn a_trusted_click_on_a_covered_ref_is_refused_and_names_the_cover_by_its_ref() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = semantic_snapshot(&f, &target, &tab).await;
    let button = named_ref(&snap, "main-btn");
    let cover = named_ref(&snap, "Shadow Input");
    // Node 20 (the shadow input) is on top at the button's centre.
    f.state.lock().unwrap().hit = Some((2, covered(), 20));

    let click = BrowserClickTool::new(f.engine.clone());
    let refused = click
        .invoke(json!({"target_id": target, "tab_id": tab, "ref": button, "session": SESSION}))
        .await;
    let refused = structured(&refused);
    assert_eq!(
        refused["refusal"]["code"], "browser_target_covered",
        "{refused}"
    );
    assert_eq!(
        refused["refusal"]["detail"]["covered_by_ref"], cover,
        "{refused}"
    );
    assert_eq!(refused["refusal"]["detail"]["click_sent"], false);
    let message = refused["refusal"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("covered at its centre by {cover}")),
        "{message}"
    );
    // The refusal names a ref the session holds, and nothing the page says.
    assert!(!message.contains("Shadow Input"), "{message}");
    assert!(
        recorded_calls(&f, "Input.dispatchMouseEvent").is_empty(),
        "the element on top must not receive the click"
    );
    // It was looked at twice: before and after the scroll and the beat.
    assert_eq!(recorded_calls(&f, "DOM.scrollIntoViewIfNeeded").len(), 2);
}

#[tokio::test]
async fn a_cover_the_session_holds_no_ref_for_is_not_described() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let button = ref_of(&snap, "main", "main-btn");
    f.state.lock().unwrap().hit = Some((2, covered(), 20));
    let refused = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({"target_id": target, "tab_id": tab, "ref": button, "session": SESSION}))
        .await;
    let refused = structured(&refused);
    assert_eq!(
        refused["refusal"]["code"], "browser_target_covered",
        "{refused}"
    );
    assert!(refused["refusal"]["detail"]["covered_by_ref"].is_null());
    assert!(
        refused["refusal"]["message"]
            .as_str()
            .unwrap()
            .contains("has no ref in the outline you hold"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_click_is_not_sent_on_an_unproven_or_container_hit() {
    for (facts, code, says) in [
        // The page did not answer the question.
        (
            json!(true),
            "browser_action_unavailable",
            "did not say which element",
        ),
        // The element around the button would receive it, not the button.
        (
            json!({"connected": true, "hit": true, "inside_target": false,
                "contains_target": true, "label_of_target": false, "own_indicator": false}),
            "browser_target_covered",
            "takes no click at its centre",
        ),
        (
            json!({"connected": true, "hit": false}),
            "browser_target_covered",
            "outside the visible page",
        ),
        (
            json!({"connected": true, "hit": true, "inside_target": false,
                "contains_target": false, "label_of_target": false, "own_indicator": true}),
            "browser_target_covered",
            "Cua's own",
        ),
    ] {
        let f = fixture().await;
        let (target, tab) = bind(&f).await;
        let snap = snapshot(&f, &target, &tab).await;
        let button = ref_of(&snap, "main", "main-btn");
        f.state.lock().unwrap().hit = Some((2, facts.clone(), 0));
        let refused = BrowserClickTool::new(f.engine.clone())
            .invoke(json!({"target_id": target, "tab_id": tab, "ref": button, "session": SESSION}))
            .await;
        let refused = structured(&refused);
        assert_eq!(refused["refusal"]["code"], code, "{facts}: {refused}");
        assert!(
            refused["refusal"]["message"]
                .as_str()
                .unwrap()
                .contains(says),
            "{facts}: {refused}"
        );
        assert!(
            recorded_calls(&f, "Input.dispatchMouseEvent").is_empty(),
            "{facts}"
        );
    }
}

#[tokio::test]
async fn a_cover_that_clears_within_the_beat_does_not_stop_the_click() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let button = ref_of(&snap, "main", "main-btn");
    f.state.lock().unwrap().hit = Some((1, covered(), 20));
    let clicked = BrowserClickTool::new(f.engine.clone())
        .invoke(json!({"target_id": target, "tab_id": tab, "ref": button, "session": SESSION}))
        .await;
    assert_eq!(structured(&clicked)["status"], "ok", "{clicked:?}");
    assert_eq!(recorded_calls(&f, "Input.dispatchMouseEvent").len(), 2);
}

#[tokio::test]
async fn a_coordinate_click_and_a_dom_event_click_are_not_hit_tested() {
    let f = fixture().await;
    let (target, tab) = bind(&f).await;
    let snap = snapshot(&f, &target, &tab).await;
    let button = ref_of(&snap, "main", "main-btn");
    f.state.lock().unwrap().hit = Some((9, covered(), 20));
    let click = BrowserClickTool::new(f.engine.clone());
    // The caller named a point, not an element: there is no target to be covered.
    let by_point = click
        .invoke(
            json!({"target_id": target, "tab_id": tab, "x": 105.0, "y": 105.0, "session": SESSION}),
        )
        .await;
    assert_eq!(structured(&by_point)["status"], "ok");
    // A DOM click is dispatched on the element itself, whatever is on top.
    let synthetic = click
        .invoke(json!({"target_id": target, "tab_id": tab, "ref": button,
            "input_route": "dom_event", "session": SESSION}))
        .await;
    assert_eq!(structured(&synthetic)["status"], "ok");
    assert!(recorded_calls(&f, "Runtime.callFunctionOn")
        .iter()
        .all(|(_, params)| !params["functionDeclaration"]
            .as_str()
            .unwrap()
            .contains("elementFromPoint")));
}

// ── Found by review ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_stale_refusal_says_nothing_the_page_says_and_retires_the_ref_for_good() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let reply = named_ref(&first, "Reply");

    f.state
        .lock()
        .unwrap()
        .renamed
        .insert(2011, "Wire 4,000 USD".into());
    let refused = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");
    assert!(
        !refused.to_string().contains("Wire"),
        "what the element reads as now is for a read to tell: {refused}"
    );

    // The page puts the old name back before anything is read again: the
    // ref was refused once and does not come back.
    f.state.lock().unwrap().renamed.clear();
    let again = dom_click(&f, &target, &tab, &reply).await;
    assert_eq!(again["refusal"]["code"], "browser_ref_stale", "{again}");
    assert!(
        recorded_calls(&f, "Runtime.callFunctionOn").is_empty(),
        "nothing was clicked"
    );
    let second = semantic_snapshot(&f, &target, &tab).await;
    assert_ne!(named_ref(&second, "Reply"), reply);
}

#[tokio::test]
async fn a_continuation_is_refused_after_a_detach_even_on_the_same_document() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"]
        .as_str()
        .unwrap()
        .to_owned();

    f.state.lock().unwrap().detached = true;
    let stale = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(stale["refusal"]["code"], "browser_ref_stale", "{stale}");
    let fresh = semantic_snapshot(&f, &target, &tab).await;
    assert_ne!(fresh["snapshot"]["id"], first["snapshot"]["id"]);
}

#[tokio::test]
async fn typing_that_opens_a_dialog_returns_at_once_with_the_dialog() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
        st.type_opens_dialog = true;
    })
    .await;
    let agent = Agent::bound(&f, "changes-type-dialog").await;
    let first = agent.snapshot().await;
    let started = std::time::Instant::now();
    let typed = agent
        .call(
            "browser_type",
            json!({ "ref": named_ref(&first, "Reply body"), "text": "hello" }),
        )
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "no call was waited out behind the dialog: {:?}",
        started.elapsed()
    );
    assert_eq!(typed["effect"], "unverifiable", "{typed}");
    assert_eq!(typed["changes"]["kind"], "unavailable", "{typed}");
    assert_eq!(typed["changes"]["reason"], "javascript_dialog_open");
    assert_eq!(typed["changes"]["dialog"]["kind"], "confirm");
    // The tab is not held: the dialog tool gets to it.
    let dialog_id = typed["changes"]["dialog"]["dialog_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let dismissed = agent
        .call(
            "browser_dialog",
            json!({ "action": "dismiss", "dialog_id": dialog_id }),
        )
        .await;
    assert_eq!(dismissed["status"], "ok", "{dismissed}");
}

#[cfg(feature = "yaml")]
#[tokio::test]
async fn a_caller_that_may_click_but_not_read_learns_nothing_the_page_says() {
    use crate::authorization::PermissionMode;
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    let runtime = SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Bounded],
            false,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .unwrap(),
    );
    let allowing = |tools: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.yaml");
        std::fs::write(
            &path,
            format!(
                "version: 3\nexpires_after: 1h\nidle_timeout: 30m\nallow:\n  tools: [{tools}]\n\
                 resources:\n  desktop:\n    windows:\n      - pid: 1\n        window_id: 7\n  \
                 browser:\n    origins: [\"https://fixture.test\"]\n"
            ),
        )
        .unwrap();
        let manifest = Arc::new(crate::session_manifest::load_manifest(&path).unwrap());
        runtime
            .compatibility_context(PermissionMode::Bounded, Some(manifest))
            .unwrap()
    };
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let mut agent = Agent::bound_as(
        &f,
        "click-no-read",
        allowing("get_browser_state, browser_click"),
    )
    .await;
    let first = agent.snapshot().await;
    let (reply, archive) = (
        named_ref(&first, "Reply"),
        named_ref(&first, "Archive item 0"),
    );
    // Reading is taken away; the refs already held still act.
    agent.context = allowing("browser_click");
    {
        let mut state = f.state.lock().unwrap();
        state.renamed.insert(2011, "Wire 4,000 USD".into());
        state
            .click_renames
            .push((2000, "Balance: 12,000 USD".into()));
    }
    let stale = agent
        .call(
            "browser_click",
            json!({ "ref": reply, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(stale["error"]["code"], "browser_ref_stale", "{stale}");
    let clicked = agent
        .call(
            "browser_click",
            json!({ "ref": archive, "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(
        clicked["effect"], "unverifiable",
        "the click itself is allowed: {clicked}"
    );
    assert_eq!(clicked["changes"]["kind"], "unavailable", "{clicked}");
    assert_eq!(clicked["changes"]["reason"], "permission_denied");
    for result in [&stale, &clicked] {
        let said = result.to_string();
        assert!(
            !said.contains("Wire") && !said.contains("Balance"),
            "{said}"
        );
    }
}

#[tokio::test]
async fn a_dialog_that_opens_after_the_insert_was_answered_is_still_seen_at_once() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
        st.type_opens_dialog_late = true;
    })
    .await;
    let agent = Agent::bound(&f, "changes-type-late-dialog").await;
    let first = agent.snapshot().await;
    let started = std::time::Instant::now();
    let typed = agent
        .call(
            "browser_type",
            json!({ "ref": named_ref(&first, "Reply body"), "text": "hello" }),
        )
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        typed["changes"]["reason"], "javascript_dialog_open",
        "{typed}"
    );
    assert_eq!(
        typed["delivery"]["delivered_count"], 5,
        "the text was inserted: {typed}"
    );
    // Nothing was asked of the blocked page after the insert.
    let after_insert: Vec<String> = {
        let state = f.state.lock().unwrap();
        let insert = state
            .calls
            .iter()
            .position(|(_, method, _)| method == "Input.insertText")
            .unwrap();
        state.calls[insert + 1..]
            .iter()
            .map(|(_, method, _)| method.clone())
            .collect()
    };
    assert!(after_insert.is_empty(), "{after_insert:?}");
}

#[tokio::test]
async fn a_dialog_that_opens_during_the_read_back_is_not_waited_out() {
    // The page keeps only digits, so the first read is a mismatch and the
    // read-back asks again. The alert opens at one of those reads, which the
    // blocked page never answers, fails, or answered just before.
    for (at, read) in [
        (1, "unanswered"),
        (1, "error"),
        (0, "error"),
        (0, "answered"),
        (0, "detached"),
    ] {
        let f = fixture_with(|st| {
            st.semantic_large_page = true;
            st.field_value = Some(String::new());
            st.field_digits_only = true;
            st.readback_opens_dialog = Some((at, read));
        })
        .await;
        let agent = Agent::bound(&f, "changes-readback-dialog").await;
        let first = agent.snapshot().await;
        let started = std::time::Instant::now();
        let typed = agent
            .call(
                "browser_type",
                json!({ "ref": named_ref(&first, "Reply body"), "text": "hello" }),
            )
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{read} at {at}: {:?}",
            started.elapsed()
        );
        assert_eq!(typed["effect"], "unverifiable", "{read} at {at}: {typed}");
        assert_eq!(
            typed["changes"]["reason"], "javascript_dialog_open",
            "{read} at {at}: {typed}"
        );
        let said = typed.to_string();
        assert!(!said.contains("browser_type_mismatch"), "{said}");
        assert!(!said.contains("private dialog text"), "{said}");
    }
}

#[tokio::test]
async fn a_dialog_during_the_read_back_is_reported_to_a_dom_refs_caller_too() {
    // No page changes come back with dom_refs_v1, so the result itself has
    // to say a dialog is why the field was not read.
    for read in ["unanswered", "detached"] {
        let f = fixture_with(|st| {
            st.field_value = Some(String::new());
            st.readback_opens_dialog = Some((0, read));
        })
        .await;
        let (target, tab) = bind(&f).await;
        let snap = snapshot(&f, &target, &tab).await;
        let typed = BrowserTypeTool::new(f.engine.clone())
            .invoke(json!({
                "target_id": target, "tab_id": tab, "session": SESSION,
                "ref": ref_of(&snap, "main", "Shadow Input"), "text": "ada"
            }))
            .await;
        let s = structured(&typed);
        assert_eq!(s["effect"], "unverifiable", "{read}: {s}");
        assert_eq!(s["readback"], "javascript_dialog_open", "{read}: {s}");
    }
}

#[tokio::test]
async fn a_key_whose_keydown_opened_a_dialog_is_not_counted_as_typed() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
        st.field_caret = Some(0);
        // The second key's keydown handler opens the dialog.
        st.key_down_opens_dialog_at = Some(1);
    })
    .await;
    let agent = Agent::bound(&f, "changes-keys-dialog").await;
    let first = agent.snapshot().await;
    let typed = agent
        .call(
            "browser_type",
            json!({ "ref": named_ref(&first, "Reply body"), "text": "abc", "mode": "keystrokes" }),
        )
        .await;
    assert_eq!(
        typed["changes"]["reason"], "javascript_dialog_open",
        "{typed}"
    );
    assert_eq!(
        typed["delivery"]["delivered_count"], 1,
        "only the first character's text event was sent: {typed}"
    );
    assert_eq!(f.state.lock().unwrap().field_value.as_deref(), Some("a"));
    let chars = recorded_calls(&f, "Input.dispatchKeyEvent")
        .iter()
        .filter(|(_, params)| params["type"] == "char")
        .count();
    assert_eq!(chars, 1);
}

#[tokio::test]
async fn a_browser_that_cannot_prove_its_document_gets_no_continuation() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"]
        .as_str()
        .unwrap()
        .to_owned();
    // The space was recorded with a document identity; drop it, as a browser
    // that reports no frame tree would never have had one.
    f.engine.store.update_target(SESSION, &target, |record| {
        let tab = record.tabs.get_mut(&tab).unwrap();
        let mut unproven = tab.stable.space().unwrap().identity.clone();
        unproven.root = None;
        tab.stable.set_identity_for_test(unproven);
    });
    let refused = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");
}

#[tokio::test]
async fn a_browser_without_a_frame_tree_gets_no_continuation() {
    // Neither the read that issued the token nor the one that uses it can
    // name the document: two unknowns are not the same document.
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.frame_tree_unsupported = true;
    })
    .await;
    let (target, tab) = bind(&f).await;
    let first = semantic_snapshot(&f, &target, &tab).await;
    let token = first["snapshot"]["continuation"].as_str().unwrap();
    let refused = semantic_snapshot_with(&f, &target, &tab, json!({"continuation": token})).await;
    assert_eq!(refused["refusal"]["code"], "browser_ref_stale", "{refused}");
}

// ── A session the caller did not name (through the real registry) ───────────
//
// An MCP transport that gets no `session` argument runs the call in its own
// implicit session: the lease it holds. These calls are delivered as that
// transport delivers them, in standard mode, where a read attests its tab.

fn standard() -> Arc<crate::session_authorization::EffectiveAuthorizationContext> {
    use crate::authorization::PermissionMode;
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Standard],
            false,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .unwrap(),
    )
    .compatibility_context(PermissionMode::Standard, None)
    .unwrap()
}

/// One transport's calls on a fixture's browser tools, none naming a session.
struct Unnamed {
    registry: Arc<crate::tool::ToolRegistry>,
    context: Arc<crate::session_authorization::EffectiveAuthorizationContext>,
    transport: &'static str,
}

impl Unnamed {
    fn over(
        f: &Fixture,
        context: &Arc<crate::session_authorization::EffectiveAuthorizationContext>,
        transport: &'static str,
    ) -> Self {
        let mut registry = crate::tool::ToolRegistry::new();
        super::tools::register_browser_tools(&f.engine, &mut registry);
        let registry = Arc::new(registry);
        registry.init_self_weak();
        Self {
            registry,
            context: context.clone(),
            transport,
        }
    }

    async fn call(&self, name: &str, mut args: Value) -> Value {
        assert!(args.get("session").is_none(), "these calls name no session");
        args["_session_id"] = json!(self.transport);
        args["_transport_session_id"] = json!(self.transport);
        let evidence = crate::tool::TrustedInvocationEvidence::extract_from_adapter_args(&mut args);
        let result = self
            .registry
            .invoke_with_context_and_evidence(name, args, self.context.clone(), evidence)
            .await;
        result.structured_content.unwrap_or_else(|| {
            panic!(
                "{name} returned no structured content: {:?}",
                result.content
            )
        })
    }

    /// The transport closed: its implicit session ends.
    fn end(&self) {
        crate::session::fire_session_end(&self.context.runtime_session_key(self.transport));
    }
}

#[tokio::test]
async fn a_bind_and_a_read_agree_on_a_session_the_caller_did_not_name() {
    let f = fixture().await;
    let agent = Unnamed::over(&f, &standard(), "unnamed-bind-read");
    let bound = agent
        .call("get_browser_state", json!({ "pid": 1, "window_id": 7 }))
        .await;
    assert_eq!(bound["status"], "ok", "{bound}");
    let tab = json!({ "target_id": bound["target_id"], "tab_id": bound["tabs"][0]["tab_id"] });
    let read = agent.call("get_browser_state", tab.clone()).await;
    assert_eq!(read["status"], "ok", "{read}");
    assert!(read["outline"].as_str().is_some_and(|o| !o.is_empty()));
    agent.end();

    // And on one it did name.
    let named = Agent::bound_as(&f, "named-bind-read", standard()).await;
    assert_eq!(named.snapshot().await["status"], "ok");
}

#[tokio::test]
async fn an_unnamed_session_runs_a_whole_batch_from_the_binds_own_read() {
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.field_value = Some(String::new());
    })
    .await;
    let agent = Unnamed::over(&f, &standard(), "unnamed-batch");
    // Call 1: the bind, with the active tab's outline in it.
    let bound = agent
        .call("get_browser_state", json!({ "pid": 1, "window_id": 7 }))
        .await;
    assert_eq!(bound["status"], "ok", "{bound}");
    assert_eq!(bound["tab_id"], bound["tabs"][0]["tab_id"]);
    let first = with_outline_entries(bound.clone());
    f.state
        .lock()
        .unwrap()
        .click_renames
        .push((2011, "Sent".into()));

    // Call 2: a step by ref, a step aimed by role and name, an expect, and
    // the diff from what the bind read. Every nested read and step runs in
    // the transport's session, as the two calls themselves did.
    let output = agent
        .call(
            "browser_steps",
            json!({ "target_id": bound["target_id"], "tab_id": bound["tab_id"], "steps": [
                {"action": "type", "ref": named_ref(&first, "Reply body"), "text": "hello"},
                {"action": "click", "role": "button", "name": "Reply", "input_route": "dom_event",
                 "expect": {"role": "button", "name": "Sent"}},
            ]}),
        )
        .await;
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(output["steps"][0]["effect"], "confirmed", "{output}");
    assert_eq!(output["steps"][1]["ref"], named_ref(&first, "Reply"));
    assert_eq!(output["changes"]["kind"], "diff", "{output}");
    assert_eq!(
        output["changes"]["base_revision"],
        bound["snapshot"]["revision"]
    );
    agent.end();
}

#[tokio::test]
async fn two_transports_that_name_no_session_do_not_share_bindings() {
    let f = fixture().await;
    let context = standard();
    let first = Unnamed::over(&f, &context, "unnamed-transport-a");
    let second = Unnamed::over(&f, &context, "unnamed-transport-b");
    let bound = first
        .call("get_browser_state", json!({ "pid": 1, "window_id": 7 }))
        .await;
    let tab = json!({ "target_id": bound["target_id"], "tab_id": bound["tab_id"] });

    // The other transport holds no such target: nothing is read or sent.
    let reads = recorded_calls(&f, "Accessibility.getFullAXTree").len();
    let foreign = second.call("get_browser_state", tab.clone()).await;
    assert_eq!(foreign["status"], "refused", "{foreign}");
    let mut click = tab.clone();
    click["x"] = json!(30);
    click["y"] = json!(40);
    let foreign = second.call("browser_click", click).await;
    assert_eq!(foreign["status"], "refused", "{foreign}");
    assert_eq!(
        recorded_calls(&f, "Accessibility.getFullAXTree").len(),
        reads
    );
    assert!(recorded_calls(&f, "Input.dispatchMouseEvent").is_empty());

    // Its owner still reads it; once the owner's transport ends, nobody does.
    assert_eq!(
        first.call("get_browser_state", tab.clone()).await["status"],
        "ok"
    );
    first.end();
    let ended = first.call("get_browser_state", tab).await;
    assert_eq!(ended["status"], "refused", "{ended}");
    assert_eq!(
        f.engine
            .store
            .target_count(&context.runtime_session_key("unnamed-transport-a")),
        0
    );
    second.end();
}

// ── One call in: a bind that reads the active tab ───────────────────────────

#[tokio::test]
async fn a_binds_own_read_is_the_baseline_the_next_action_diffs_against() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let agent = Agent::bound(&f, "bind-reads").await;
    let bound = &agent.bound;
    assert_eq!(bound["mode"], "bind", "{bound}");
    assert_eq!(bound["tab_id"], bound["tabs"][0]["tab_id"]);
    assert_eq!(bound["page"]["url"], "https://fixture.test/");
    assert!(bound.get("observation").is_none(), "{bound}");
    // The page was read once.
    let reads = recorded_calls(&f, "Accessibility.getFullAXTree")
        .into_iter()
        .filter(|(_, params)| params["frameId"] == "F_MAIN")
        .count();
    assert_eq!(reads, 1);

    let first = with_outline_entries(bound.clone());
    f.state
        .lock()
        .unwrap()
        .click_renames
        .push((2011, "Sent".into()));
    let clicked = agent
        .call(
            "browser_click",
            json!({ "ref": named_ref(&first, "Reply"), "input_route": "dom_event" }),
        )
        .await;
    assert_eq!(clicked["changes"]["kind"], "diff", "{clicked}");
    assert_eq!(
        clicked["changes"]["base_revision"],
        bound["snapshot"]["revision"]
    );
}

#[tokio::test]
async fn a_bind_passes_a_first_reads_options_on_and_nothing_else() {
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let mut registry = crate::tool::ToolRegistry::new();
    super::tools::register_browser_tools(&f.engine, &mut registry);
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let result = registry
        .invoke_with_context(
            "get_browser_state",
            json!({ "pid": 1, "window_id": 7, "session": "bind-options",
                "include_screenshot": true, "query": "Reply", "since_revision": 3 }),
            unrestricted(),
        )
        .await;
    let bound = structured(&result);
    assert_eq!(bound["status"], "ok", "{bound}");
    assert_eq!(bound["screenshot"]["source"], "cdp_tab", "{bound}");
    assert!(matches!(result.content[0], Content::Image { .. }));
    let outline = bound["outline"].as_str().unwrap();
    assert!(
        outline.contains("Reply") && !outline.contains("Archive item 0"),
        "{outline}"
    );
    assert!(
        bound.get("changes").is_none(),
        "a first read is never a diff"
    );
}

#[tokio::test]
async fn a_read_whose_screenshot_failed_is_still_the_outline_the_next_diff_is_from() {
    // The capture fails after the page was read and that read recorded.
    let f = fixture_with(|st| {
        st.semantic_large_page = true;
        st.screenshot_data = "not-base64".into();
    })
    .await;
    let mut registry = crate::tool::ToolRegistry::new();
    super::tools::register_browser_tools(&f.engine, &mut registry);
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let context = unrestricted();
    let call = |name: &'static str, mut args: Value| {
        let (registry, context) = (registry.clone(), context.clone());
        async move {
            args["session"] = json!("bind-screenshot-failed");
            registry.invoke_with_context(name, args, context).await
        }
    };
    let result = call(
        "get_browser_state",
        json!({ "pid": 1, "window_id": 7, "include_screenshot": true }),
    )
    .await;
    let bound = structured(&result).clone();
    assert_eq!((&bound["status"], &bound["mode"]), (&json!("ok"), &json!("bind")));
    // The caller holds what the session holds: the outline and its revision.
    assert!(bound.get("observation").is_none(), "{bound}");
    assert_eq!(bound["tab_id"], bound["tabs"][0]["tab_id"]);
    let first = with_outline_entries(bound.clone());
    assert_eq!(bound["screenshot"]["status"], "refused", "{bound}");
    assert_eq!(
        bound["screenshot"]["refusal"]["code"],
        "browser_route_unavailable"
    );
    assert!(bound.get("screenshot_width").is_none());
    assert!(result
        .content
        .iter()
        .all(|content| !matches!(content, Content::Image { .. })));
    assert!(matches!(
        &result.content[0],
        Content::Text { text, .. } if text.starts_with("bound target") && text.contains("no screenshot: refused")
    ));

    // So the next action's diff is from that outline, and says what changed.
    f.state
        .lock()
        .unwrap()
        .click_renames
        .push((2011, "Sent".into()));
    let clicked = call(
        "browser_click",
        json!({ "target_id": bound["target_id"], "tab_id": bound["tab_id"],
            "ref": named_ref(&first, "Reply"), "input_route": "dom_event" }),
    )
    .await;
    let clicked = structured(&clicked);
    assert_eq!(clicked["changes"]["kind"], "diff", "{clicked}");
    assert_eq!(
        clicked["changes"]["base_revision"],
        bound["snapshot"]["revision"]
    );
    let applied = apply_changes(bound["outline"].as_str().unwrap(), &clicked["changes"]);
    assert!(applied.contains("button \"Sent\""), "{applied}");
}

#[tokio::test]
async fn a_bind_whose_read_is_refused_keeps_the_binding_and_says_why() {
    // A JavaScript dialog is up when the second session binds the window.
    let f = fixture_with(|st| st.semantic_large_page = true).await;
    let opener = Agent::bound(&f, "bind-dialog-opener").await;
    f.state.lock().unwrap().click_opens_dialog = true;
    let opened = opener
        .call(
            "browser_click",
            json!({ "ref": named_ref(&with_outline_entries(opener.bound.clone()), "Reply"),
                "input_route": "dom_event" }),
        )
        .await;
    let dialog_id = opened["changes"]["dialog"]["dialog_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_owned();

    let agent = Agent::bound(&f, "bind-dialog").await;
    let bound = &agent.bound;
    assert_eq!(
        (&bound["status"], &bound["mode"]),
        (&json!("ok"), &json!("bind"))
    );
    assert!(bound["target_id"].as_str().is_some() && bound["tabs"][0]["tab_id"].is_string());
    let observation = &bound["observation"];
    assert_eq!(observation["status"], "refused", "{bound}");
    assert_eq!(observation["refusal"]["code"], "browser_dialog_open");
    assert_eq!(observation["tab_id"], bound["tabs"][0]["tab_id"]);
    assert!(
        observation["refusal"]["message"]
            .as_str()
            .unwrap()
            .contains("browser_dialog"),
        "the refusal names the call that clears it: {observation}"
    );
    // Nothing of a page that was not read.
    for absent in ["outline", "snapshot", "page", "tab_id"] {
        assert!(bound.get(absent).is_none(), "{absent}: {bound}");
    }

    // And no baseline: once the dialog is gone, the first action's result is
    // a full snapshot, not a diff from a read that never happened.
    let accepted = opener
        .call(
            "browser_dialog",
            json!({ "action": "accept", "dialog_id": dialog_id }),
        )
        .await;
    assert_eq!(accepted["status"], "ok", "{accepted}");
    let clicked = agent
        .call("browser_click", json!({ "x": 30, "y": 40 }))
        .await;
    assert_eq!(clicked["changes"]["kind"], "snapshot", "{clicked}");
    assert_eq!(clicked["changes"]["reason"], "no_baseline");
}

#[tokio::test]
async fn a_bind_reads_no_tab_when_the_active_one_is_not_proven() {
    // The window's title names no tab of it.
    let f = fixture_with(|st| st.tab_title = "Another title".into()).await;
    let agent = Agent::bound(&f, "bind-no-active-tab").await;
    let bound = &agent.bound;
    assert_eq!(bound["tabs"].as_array().unwrap().len(), 1);
    assert_ne!(bound["tabs"][0]["active"], true, "{bound}");
    assert_eq!(bound["observation"]["status"], "refused", "{bound}");
    assert_eq!(
        bound["observation"]["refusal"]["code"],
        "browser_tab_required"
    );
    assert!(bound["observation"]["refusal"]["message"]
        .as_str()
        .unwrap()
        .contains("a tab_id from tabs"));
    for absent in ["outline", "snapshot", "page", "tab_id"] {
        assert!(bound.get(absent).is_none(), "{absent}: {bound}");
    }
    assert!(
        recorded_calls(&f, "Accessibility.getFullAXTree").is_empty(),
        "the only tab was not read in the active one's place"
    );
    // The tab is still there to read by id.
    assert_eq!(agent.snapshot().await["status"], "ok");
}

#[tokio::test]
async fn a_tab_the_user_left_between_the_bind_and_its_read_is_read_where_it_is() {
    let f = fixture_with(|st| st.hide_tab_on_attach = true).await;
    let agent = Agent::bound(&f, "bind-tab-switched").await;
    let bound = &agent.bound;
    assert!(!f.state.lock().unwrap().tab_visible, "the read attached");
    // The result is about the tab it names: that tab's id and that tab's
    // page. The tab was not brought back to the front to be read.
    assert_eq!(bound["tab_id"], bound["tabs"][0]["tab_id"], "{bound}");
    assert_eq!(bound["page"]["url"], "https://fixture.test/");
    assert!(bound["outline"].as_str().is_some_and(|o| !o.is_empty()));
    assert!(f.state.lock().unwrap().calls.iter().all(|(_, method, _)| {
        method != "Target.activateTarget" && method != "Page.bringToFront"
    }));
}

#[tokio::test]
async fn app_is_resolved_to_its_one_window_or_refused_before_anything_is_bound() {
    let f = fixture().await;
    let mut registry = crate::tool::ToolRegistry::new();
    super::tools::register_browser_tools(&f.engine, &mut registry);
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let call = |args: Value| {
        let registry = registry.clone();
        async move {
            let mut args = args;
            args["session"] = json!("bind-by-app");
            registry
                .invoke_with_context("get_browser_state", args, unrestricted())
                .await
        }
    };
    let text = |result: &ToolResult| match &result.content[0] {
        Content::Text { text, .. } => text.clone(),
        other => panic!("{other:?}"),
    };

    // One window: bound and read, in the one call.
    let result = call(json!({ "app": "Fixture Browser" })).await;
    let bound = structured(&result);
    assert_eq!(bound["status"], "ok", "{bound}");
    assert_eq!(bound["native_title"], "Fixture - Chrome");
    assert_eq!(bound["tab_id"], bound["tabs"][0]["tab_id"]);
    assert!(bound["outline"].as_str().is_some_and(|o| !o.is_empty()));

    // Several windows, or none: the resolver's own refusal, and no browser
    // was asked anything.
    let asked = recorded_calls(&f, "Target.getTargets").len();
    let several = call(json!({ "app": "Two Windows" })).await;
    assert_eq!(several.is_error, Some(true));
    let refused = structured(&several);
    assert_eq!(refused["code"], "app_window_ambiguous", "{refused}");
    assert_eq!(
        refused["candidates"],
        json!([
            { "window_id": 7, "pid": 1, "title": "Inbox" },
            { "window_id": 8, "pid": 1, "title": "Docs" },
        ])
    );
    assert!(text(&several).contains("pass pid + window_id for one"));
    let absent = call(json!({ "app": "Not Running" })).await;
    assert_eq!(absent.is_error, Some(true));
    assert_eq!(structured(&absent)["code"], "app_not_running");

    // One target form per call.
    for mixed in [
        json!({ "app": "Fixture Browser", "pid": 1, "window_id": 7 }),
        json!({ "app": "Fixture Browser", "pid": 1 }),
        json!({ "app": "Fixture Browser", "target_id": bound["target_id"], "tab_id": bound["tab_id"] }),
    ] {
        let result = call(mixed).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text(&result).contains("pass one target form"),
            "{}",
            text(&result)
        );
    }
    let blank = call(json!({ "app": " " })).await;
    assert!(text(&blank).contains("nonblank"), "{}", text(&blank));
    assert_eq!(recorded_calls(&f, "Target.getTargets").len(), asked);

    // A platform with no app resolver says what to pass instead.
    let mut registry = crate::tool::ToolRegistry::new();
    super::tools::register_browser_tools(&super::tools::tests::engine(), &mut registry);
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let elsewhere = registry
        .invoke_with_context(
            "get_browser_state",
            json!({ "app": "Google Chrome", "session": "bind-by-app-elsewhere" }),
            unrestricted(),
        )
        .await;
    assert_eq!(elsewhere.is_error, Some(true));
    assert!(
        text(&elsewhere).contains("macOS-only") && text(&elsewhere).contains("pid + window_id")
    );
}

#[cfg(feature = "yaml")]
#[tokio::test]
async fn a_manifest_judges_the_window_app_resolved_to_and_the_binds_read() {
    use crate::authorization::PermissionMode;
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    let runtime = SessionAuthorizationRegistry::with_ceiling(
        SessionModeCeiling::for_trusted_sessions(
            [PermissionMode::Bounded],
            false,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(30),
        )
        .unwrap(),
    );
    let allowing = |window: u64, origin: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.yaml");
        std::fs::write(
            &path,
            format!(
                "version: 3\nexpires_after: 1h\nidle_timeout: 30m\nallow:\n  tools: [get_browser_state]\n\
                 resources:\n  desktop:\n    windows:\n      - pid: 1\n        window_id: {window}\n  \
                 browser:\n    origins: [\"{origin}\"]\n"
            ),
        )
        .unwrap();
        let manifest = Arc::new(crate::session_manifest::load_manifest(&path).unwrap());
        runtime
            .compatibility_context(PermissionMode::Bounded, Some(manifest))
            .unwrap()
    };
    let f = fixture().await;
    let mut registry = crate::tool::ToolRegistry::new();
    super::tools::register_browser_tools(&f.engine, &mut registry);
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let by_app = |session: &'static str, context| {
        let registry = registry.clone();
        async move {
            let result = registry
                .invoke_with_context(
                    "get_browser_state",
                    json!({ "app": "Fixture Browser", "session": session }),
                    context,
                )
                .await;
            structured(&result).clone()
        }
    };

    // The window app names is not the one the manifest allows: refused as
    // pid 1, window 7 would be, and no browser was asked anything.
    let other_window = by_app("manifest-other-window", allowing(8, "https://fixture.test")).await;
    assert_eq!(other_window["status"], "refused", "{other_window}");
    assert!(recorded_calls(&f, "Target.getTargets").is_empty());

    // The window is allowed, its page's origin is not: bound, not read.
    let other_origin = by_app(
        "manifest-other-origin",
        allowing(7, "https://elsewhere.test"),
    )
    .await;
    assert_eq!(other_origin["status"], "ok", "{other_origin}");
    assert!(other_origin["target_id"].as_str().is_some());
    assert_eq!(
        other_origin["observation"]["status"], "refused",
        "{other_origin}"
    );
    assert!(other_origin["observation"]["refusal"]["code"].is_string());
    for absent in ["outline", "snapshot", "page", "tab_id"] {
        assert!(
            other_origin.get(absent).is_none(),
            "{absent}: {other_origin}"
        );
    }
    assert!(recorded_calls(&f, "Accessibility.getFullAXTree").is_empty());

    // Both allowed: bound and read.
    let allowed = by_app("manifest-allowed", allowing(7, "https://fixture.test")).await;
    assert_eq!(allowed["status"], "ok", "{allowed}");
    assert!(
        allowed["outline"].as_str().is_some_and(|o| !o.is_empty()),
        "{allowed}"
    );
}
