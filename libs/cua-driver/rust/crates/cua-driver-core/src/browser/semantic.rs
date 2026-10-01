//! Deterministic semantic browser snapshots.
//!
//! The collector joins Chrome's accessibility tree with pierced DOM metadata
//! and layout snapshot evidence. Accessibility supplies readable semantics,
//! DOM backend ids preserve exact mutation capabilities, and layout evidence
//! keeps hidden retained application state from displacing the active view.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use super::observation::{NodeKey, ViewNode, REF_SLOT};
use super::store::{BrowserActionKind, BrowserVisibility, FrameKind, FrameRef, RefEntry, RowName};

pub(crate) const SEMANTIC_COMPUTED_STYLES: &[&str] = &[
    "display",
    "visibility",
    "opacity",
    "pointer-events",
    "cursor",
    "position",
    "z-index",
    "overflow-x",
    "overflow-y",
];
pub(crate) const DEFAULT_SEMANTIC_NODE_BUDGET: usize = 300;
/// Width a ref is counted at when an outline is fitted to its character
/// budget, before the ref is assigned (`p` + id + `:` + index).
const BUDGETED_REF: &str = "p0000000:000";
const NEAR_VIEWPORT_MARGIN: f64 = 1_000.0;
/// The element the Cua Driver Chrome extension adds to a page while Cua
/// works in it (HOST_ID in extensions/chrome/indicator.js). It is Cua's own
/// notice to the user, not page content: snapshots leave it out, so it never
/// shows up as a page change and its Stop button is never offered as a ref.
const CUA_INDICATOR_HOST_ID: &str = "cua-driver-indicator";
const MAX_SEMANTIC_TEXT_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn from_value(value: &Value) -> Option<Self> {
        let values = value.as_array()?;
        Some(Self {
            x: values.first()?.as_f64()?,
            y: values.get(1)?.as_f64()?,
            width: values.get(2)?.as_f64()?,
            height: values.get(3)?.as_f64()?,
        })
    }

    fn has_area(self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }

    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    fn expanded(self, margin: f64) -> Self {
        Self {
            x: self.x - margin,
            y: self.y - margin,
            width: self.width + margin * 2.0,
            height: self.height + margin * 2.0,
        }
    }

    fn covers(self, other: Self) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && self.x + self.width >= other.x + other.width
            && self.y + self.height >= other.y + other.height
    }
}

#[derive(Debug, Clone, Default)]
struct DomMeta {
    base_url: Option<String>,
    tag: String,
    attrs: HashMap<String, String>,
    order: usize,
    css_hidden: bool,
    /// Inside Cua's own indicator (see [`CUA_INDICATOR_HOST_ID`]).
    own_indicator: bool,
    parent_backend_node_id: Option<i64>,
    frame_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DomIndex {
    nodes: HashMap<i64, DomMeta>,
    pub(crate) css_hidden_count: usize,
}

#[derive(Debug, Clone, Default)]
struct LayoutMeta {
    bounds: Option<Rect>,
    client_rect: Option<Rect>,
    scroll_rect: Option<Rect>,
    styles: HashMap<String, String>,
    paint_order: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LayoutIndex {
    nodes: HashMap<i64, LayoutMeta>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Viewport {
    rect: Option<Rect>,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticNode {
    pub(crate) ax_id: String,
    pub(crate) parent_ax_id: Option<String>,
    pub(crate) child_ax_ids: Vec<String>,
    pub(crate) backend_node_id: Option<i64>,
    pub(crate) role: String,
    pub(crate) name: Option<String>,
    /// What an unnamed checkbox, radio or switch is called after its row's
    /// text (see [`name_controls_by_row`]). Shown, searched and matched by
    /// name like a name, but not the accessible name: a ref's fingerprint
    /// keeps `name`, which is what its live re-proof reads.
    pub(crate) row_name: Option<RowName>,
    /// A web-area root's name exactly as reported, for the page title:
    /// cleanup would turn an empty title into "missing" and rewrite others.
    pub(crate) root_title: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) states: BTreeMap<String, Value>,
    pub(crate) frame: FrameRef,
    pub(crate) visibility: BrowserVisibility,
    pub(crate) actions: Vec<BrowserActionKind>,
    pub(crate) document_order: usize,
}

impl SemanticNode {
    /// The name an outline shows: the accessible name, else the row's.
    fn shown_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.row_name.as_ref().map(|row| row.name.as_str()))
    }

    pub(crate) fn to_ref_entry(&self) -> Option<RefEntry> {
        let backend_node_id = self.backend_node_id?;
        Some(RefEntry {
            backend_node_id,
            node_name: self.role.clone(),
            label: self.name.clone(),
            actions: self.actions.clone(),
            visibility: Some(self.visibility),
            semantic: true,
            frame: self.frame.clone(),
            destination: self.url.clone(),
            attachment: None,
            minted: None,
            row: self.row_name.clone(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OmissionCounts {
    pub(crate) css_hidden: usize,
    pub(crate) offscreen: usize,
    pub(crate) page_occluded: usize,
    pub(crate) no_layout: usize,
    pub(crate) unknown: usize,
    pub(crate) budget: usize,
    pub(crate) unprovable_frame: usize,
    /// Accessibility nodes with no DOM node behind them: they cannot carry a
    /// ref, so the outline does not list them.
    pub(crate) no_dom_node: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticPage {
    /// Outline lines in tree order, each awaiting its ref.
    pub(crate) view: Vec<ViewNode>,
    /// Each line's value, for the optional structured ref list.
    pub(crate) values: Vec<Option<String>>,
    /// The lowest-ranked node selected, when a budget left others out.
    pub(crate) tail: Option<NodeKey>,
    /// The ranked nodes this page selected (their ancestors are in `view`).
    #[cfg(test)]
    pub(crate) selected: Vec<SemanticNode>,
    pub(crate) selected_nodes: usize,
    pub(crate) total_nodes: usize,
    pub(crate) next_offset: Option<usize>,
    pub(crate) omissions: OmissionCounts,
}

impl SemanticPage {
    /// The outline with every ref written as `fill`.
    #[cfg(test)]
    pub(crate) fn outline_with(&self, fill: &str) -> String {
        self.view
            .iter()
            .map(|node| node.template.replace(REF_SLOT, fill))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SemanticDocument {
    pub(crate) title: Option<String>,
    pub(crate) nodes: Vec<SemanticNode>,
    pub(crate) css_hidden_dom_count: usize,
    pub(crate) unprovable_frame_count: usize,
    pub(crate) complete: bool,
}

impl SemanticDocument {
    pub(crate) fn document_title(&self) -> Option<&str> {
        self.nodes
            .iter()
            .find(|node| {
                node.frame.kind == FrameKind::Main
                    && node.parent_ax_id.is_none()
                    && matches!(node.role.as_str(), "rootwebarea" | "webarea")
            })
            .and_then(|node| node.root_title.as_deref())
    }

    pub(crate) fn extend(&mut self, mut other: Self) {
        let was_empty = self.nodes.is_empty();
        let offset = self.nodes.len();
        for node in &mut other.nodes {
            node.document_order += offset;
            // Accessibility ids are unique per process, not across the frames
            // merged here: keep each document's tree apart.
            if !was_empty {
                let scoped = |id: &String| format!("{offset}/{id}");
                node.ax_id = scoped(&node.ax_id);
                node.parent_ax_id = node.parent_ax_id.as_ref().map(scoped);
                node.child_ax_ids = node.child_ax_ids.iter().map(scoped).collect();
            }
        }
        self.nodes.extend(other.nodes);
        self.css_hidden_dom_count += other.css_hidden_dom_count;
        self.unprovable_frame_count += other.unprovable_frame_count;
        self.complete = if was_empty {
            other.complete
        } else {
            self.complete && other.complete
        };
    }

    /// One page of the ranked working set: at most `node_budget` nodes whose
    /// outline (with ancestors, JSON-escaped) fits `char_budget`, and never
    /// fewer than one node.
    #[cfg(test)]
    pub(crate) fn page(
        &self,
        offset: usize,
        node_budget: usize,
        char_budget: usize,
        query: Option<&str>,
        scope_backend_node_id: Option<i64>,
    ) -> SemanticPage {
        self.page_sized(
            offset,
            node_budget,
            char_budget,
            query,
            scope_backend_node_id,
            false,
            None,
        )
    }

    /// [`Self::page`], with `listed` when the result also carries the
    /// structured ref list, whose entries then count against the budget.
    ///
    /// `until` is where the view this one will be compared with was cut
    /// (its lowest-ranked node). While that node is still ranked, this page
    /// stops there too, within twice the character budget: what entered or
    /// left above the cut is then the page's doing, not the budget's.
    // The parameters are one request for one page of the ranked set.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn page_sized(
        &self,
        offset: usize,
        node_budget: usize,
        char_budget: usize,
        query: Option<&str>,
        scope_backend_node_id: Option<i64>,
        listed: bool,
        until: Option<&NodeKey>,
    ) -> SemanticPage {
        let by_ax_id = ax_index(&self.nodes);
        let mut candidates = scoped_indices(&self.nodes, query, scope_backend_node_id);
        candidates.retain(|idx| {
            !matches!(
                self.nodes[*idx].visibility,
                BrowserVisibility::CssHidden | BrowserVisibility::PageOccluded
            ) && !is_structure_only(&self.nodes, &by_ax_id, *idx)
        });
        let no_dom_node = candidates
            .iter()
            .filter(|idx| self.nodes[**idx].backend_node_id.is_none())
            .count();
        candidates.retain(|idx| self.nodes[*idx].backend_node_id.is_some());
        candidates.sort_by_key(|idx| {
            (
                Reverse(query.map_or(0, |query| query_score(&self.nodes[*idx], query))),
                rank(&self.nodes[*idx]),
                self.nodes[*idx].document_order,
            )
        });

        let key_of = |idx: usize| {
            self.nodes[idx]
                .to_ref_entry()
                .map(|entry| NodeKey::of(&entry))
        };
        let start = offset.min(candidates.len());
        let mut widest = (start + node_budget.max(1)).min(candidates.len());
        let mut char_budget = char_budget;
        if let Some(cut) = until.and_then(|until| {
            candidates[start..widest]
                .iter()
                .position(|idx| key_of(*idx).as_ref() == Some(until))
        }) {
            widest = start + cut + 1;
            char_budget = char_budget.saturating_mul(2);
        }
        let render = |end: usize| {
            render_view(
                &self.nodes,
                &by_ax_id,
                &with_ancestors(&self.nodes, &candidates[start..end]),
            )
        };
        let serialized_chars = |view: &(Vec<ViewNode>, Vec<Option<String>>)| {
            serialized_chars(&view.0)
                + if listed {
                    listed_chars(&view.0, &view.1)
                } else {
                    0
                }
        };
        let mut end = widest;
        let mut view = render(end);
        if serialized_chars(&view) > char_budget {
            // Each node only adds lines, so the fitting prefixes are contiguous.
            let (mut fits, mut over) = ((start + 1).min(widest), widest);
            while fits < over {
                let middle = (fits + over).div_ceil(2);
                if serialized_chars(&render(middle)) <= char_budget {
                    fits = middle;
                } else {
                    over = middle - 1;
                }
            }
            end = fits;
            view = render(end);
        }
        let (view, values) = view;
        let tail = (end < candidates.len() && end > start)
            .then(|| key_of(candidates[end - 1]))
            .flatten();
        let page_slice = &candidates[start..end];
        #[cfg(test)]
        let selected = page_slice
            .iter()
            .map(|idx| self.nodes[*idx].clone())
            .collect::<Vec<_>>();
        let semantic_css_hidden = self
            .nodes
            .iter()
            .filter(|node| node.visibility == BrowserVisibility::CssHidden)
            .count();
        let mut omissions = OmissionCounts {
            css_hidden: self.css_hidden_dom_count.max(semantic_css_hidden),
            unprovable_frame: self.unprovable_frame_count,
            no_dom_node,
            ..Default::default()
        };
        for node in &self.nodes {
            match node.visibility {
                BrowserVisibility::CssHidden => {}
                BrowserVisibility::Offscreen => omissions.offscreen += 1,
                BrowserVisibility::PageOccluded => omissions.page_occluded += 1,
                BrowserVisibility::NoLayout => omissions.no_layout += 1,
                BrowserVisibility::Unknown => omissions.unknown += 1,
                BrowserVisibility::InViewport | BrowserVisibility::NearViewport => {}
            }
        }
        omissions.budget = candidates.len().saturating_sub(end);

        SemanticPage {
            view,
            values,
            tail,
            #[cfg(test)]
            selected,
            selected_nodes: page_slice.len(),
            total_nodes: candidates.len(),
            next_offset: (end < candidates.len()).then_some(end),
            omissions,
        }
    }
}

impl DomIndex {
    fn is_ancestor_of(&self, ancestor: i64, mut descendant: i64) -> bool {
        let mut visited = HashSet::new();
        while visited.insert(descendant) {
            let Some(parent) = self
                .nodes
                .get(&descendant)
                .and_then(|node| node.parent_backend_node_id)
            else {
                return false;
            };
            if parent == ancestor {
                return true;
            }
            descendant = parent;
        }
        false
    }

    fn shares_dom_branch(&self, left: i64, right: i64) -> bool {
        self.is_ancestor_of(left, right) || self.is_ancestor_of(right, left)
    }
}

pub(crate) fn build_dom_index(root: &Value) -> DomIndex {
    #[allow(clippy::too_many_arguments)]
    fn walk(
        node: &Value,
        inherited_hidden: bool,
        inherited_indicator: bool,
        parent_backend_node_id: Option<i64>,
        inherited_frame_id: Option<&str>,
        inherited_base_url: Option<&str>,
        order: &mut usize,
        index: &mut DomIndex,
    ) {
        let node_type = node.get("nodeType").and_then(Value::as_i64).unwrap_or(0);
        let base_url = if node_type == 9 {
            node.get("baseURL")
                .or_else(|| node.get("documentURL"))
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
        } else {
            inherited_base_url
        };
        let attrs = attributes(node);
        let tag = node
            .get("nodeName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let hidden = inherited_hidden || statically_hidden(&tag, &attrs);
        let own_indicator = inherited_indicator
            || attrs
                .get("id")
                .is_some_and(|id| id == CUA_INDICATOR_HOST_ID);
        let backend_node_id = node.get("backendNodeId").and_then(Value::as_i64);
        let frame_id = if node_type == 9 {
            node.get("frameId").and_then(Value::as_str)
        } else {
            inherited_frame_id
        };
        if let Some(backend) = backend_node_id {
            if node_type == 1 && hidden && !inherited_hidden {
                index.css_hidden_count += 1;
            }
            index.nodes.insert(
                backend,
                DomMeta {
                    base_url: base_url.map(str::to_owned),
                    tag,
                    attrs,
                    order: *order,
                    css_hidden: hidden,
                    own_indicator,
                    parent_backend_node_id,
                    frame_id: frame_id.map(str::to_owned),
                },
            );
            *order += 1;
        }
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            for child in children {
                walk(
                    child,
                    hidden,
                    own_indicator,
                    backend_node_id.or(parent_backend_node_id),
                    frame_id,
                    base_url,
                    order,
                    index,
                );
            }
        }
        if let Some(shadow_roots) = node.get("shadowRoots").and_then(Value::as_array) {
            for shadow_root in shadow_roots {
                if shadow_root.get("shadowRootType").and_then(Value::as_str) == Some("user-agent") {
                    continue;
                }
                walk(
                    shadow_root,
                    hidden,
                    own_indicator,
                    backend_node_id.or(parent_backend_node_id),
                    frame_id,
                    base_url,
                    order,
                    index,
                );
            }
        }
        if let Some(content_document) = node.get("contentDocument") {
            walk(
                content_document,
                hidden,
                own_indicator,
                backend_node_id.or(parent_backend_node_id),
                content_document.get("frameId").and_then(Value::as_str),
                None,
                order,
                index,
            );
        }
    }

    let mut index = DomIndex::default();
    let mut order = 0;
    walk(
        root,
        false,
        false,
        None,
        root.get("frameId").and_then(Value::as_str),
        None,
        &mut order,
        &mut index,
    );
    index
}

pub(crate) fn build_layout_index(snapshot: &Value) -> LayoutIndex {
    let Some(strings) = snapshot.get("strings").and_then(Value::as_array) else {
        return LayoutIndex::default();
    };
    let string_at = |idx: i64| -> Option<String> {
        usize::try_from(idx)
            .ok()
            .and_then(|idx| strings.get(idx))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };

    let mut out = LayoutIndex::default();
    let Some(documents) = snapshot.get("documents").and_then(Value::as_array) else {
        return out;
    };
    for document in documents {
        let Some(backend_ids) = document
            .pointer("/nodes/backendNodeId")
            .and_then(Value::as_array)
        else {
            continue;
        };
        let layout = document.get("layout").unwrap_or(&Value::Null);
        let node_indices = layout
            .get("nodeIndex")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let bounds = layout
            .get("bounds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let styles = layout
            .get("styles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let paint_orders = layout
            .get("paintOrders")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let client_rects = layout
            .get("clientRects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let scroll_rects = layout
            .get("scrollRects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for (layout_idx, node_index) in node_indices.iter().enumerate() {
            let Some(node_index) = node_index.as_u64().and_then(|v| usize::try_from(v).ok()) else {
                continue;
            };
            let Some(backend) = backend_ids.get(node_index).and_then(Value::as_i64) else {
                continue;
            };
            let mut computed = HashMap::new();
            if let Some(style_indices) = styles.get(layout_idx).and_then(Value::as_array) {
                for (name, value_idx) in SEMANTIC_COMPUTED_STYLES
                    .iter()
                    .zip(style_indices.iter().filter_map(Value::as_i64))
                {
                    if let Some(value) = string_at(value_idx) {
                        computed.insert((*name).to_owned(), value);
                    }
                }
            }
            out.nodes.insert(
                backend,
                LayoutMeta {
                    bounds: bounds.get(layout_idx).and_then(Rect::from_value),
                    client_rect: client_rects.get(layout_idx).and_then(Rect::from_value),
                    scroll_rect: scroll_rects.get(layout_idx).and_then(Rect::from_value),
                    styles: computed,
                    paint_order: paint_orders.get(layout_idx).and_then(Value::as_i64),
                },
            );
        }
    }
    out
}

pub(crate) fn snapshot_document_title(snapshot: &Value, root: &Value) -> Option<String> {
    let strings = snapshot.get("strings")?.as_array()?;
    let document = snapshot.get("documents")?.as_array()?.first()?;
    let string_at = |field: &str| {
        let index = usize::try_from(document.get(field)?.as_u64()?).ok()?;
        strings.get(index)?.as_str()
    };
    // The tab's cached title can belong to the page that was bound before
    // navigation. Use the collected document, never a child frame or a label
    // from accessibility, and omit the title if its identity cannot be matched.
    if document
        .get("nodes")?
        .get("backendNodeId")?
        .as_array()?
        .first()?
        .as_i64()?
        != root.get("backendNodeId")?.as_i64()?
        || string_at("documentURL")? != root.get("documentURL")?.as_str()?
    {
        return None;
    }
    if let Some(frame_id) = root.get("frameId").and_then(Value::as_str) {
        if string_at("frameId")? != frame_id {
            return None;
        }
    }
    string_at("title").map(str::to_owned)
}

pub(crate) fn parse_viewport(metrics: &Value) -> Viewport {
    let viewport = metrics
        .get("cssVisualViewport")
        .or_else(|| metrics.get("visualViewport"));
    let Some(viewport) = viewport else {
        return Viewport::default();
    };
    let x = viewport
        .get("pageX")
        .or_else(|| viewport.get("offsetX"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let y = viewport
        .get("pageY")
        .or_else(|| viewport.get("offsetY"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let width = viewport.get("clientWidth").and_then(Value::as_f64);
    let height = viewport.get("clientHeight").and_then(Value::as_f64);
    Viewport {
        rect: width.zip(height).map(|(width, height)| Rect {
            x,
            y,
            width,
            height,
        }),
    }
}

pub(crate) fn compose_accessibility_tree(
    ax_tree: &Value,
    dom: &DomIndex,
    layout: &LayoutIndex,
    viewport: &Viewport,
    frame: FrameRef,
) -> SemanticDocument {
    let Some(ax_nodes) = ax_tree.get("nodes").and_then(Value::as_array) else {
        return SemanticDocument {
            complete: false,
            css_hidden_dom_count: dom.css_hidden_count,
            ..Default::default()
        };
    };

    let mut nodes = Vec::new();
    for (fallback_order, ax) in ax_nodes.iter().enumerate() {
        if ax.get("ignored").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let ax_id = ax
            .get("nodeId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if ax_id.is_empty() {
            continue;
        }
        let role = ax_value_string(ax.get("role"))
            .unwrap_or_else(|| "unknown".to_owned())
            .to_ascii_lowercase();
        if role == "inlinetextbox" {
            continue;
        }
        let backend_node_id = ax.get("backendDOMNodeId").and_then(Value::as_i64);
        let dom_meta = backend_node_id.and_then(|backend| dom.nodes.get(&backend));
        if dom_meta.is_some_and(|meta| meta.own_indicator) {
            continue;
        }
        let layout_meta = backend_node_id.and_then(|backend| layout.nodes.get(&backend));
        let states = ax_states(ax);
        let visibility = classify_visibility(dom_meta, layout_meta, viewport);
        let actions = action_kinds(&role, dom_meta, &states, layout_meta);
        let root_title = matches!(role.as_str(), "rootwebarea" | "webarea")
            .then(|| ax_value_string(ax.get("name")))
            .flatten();
        let name = ax_value_string(ax.get("name")).and_then(clean_semantic_text);
        let value = ax_value_string(ax.get("value")).and_then(clean_semantic_text);
        let document_order = dom_meta.map_or(fallback_order, |meta| meta.order);
        nodes.push(SemanticNode {
            ax_id,
            parent_ax_id: ax
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            child_ax_ids: ax
                .get("childIds")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            backend_node_id,
            url: link_destination(&role, Some(ax), dom_meta),
            role,
            name,
            row_name: None,
            root_title,
            value,
            states,
            frame: frame.clone(),
            visibility,
            actions,
            document_order,
        });
    }

    relink_past_dropped_nodes(&mut nodes, ax_nodes);
    supplement_dom_actions(&mut nodes, dom, layout, viewport, &frame);
    apply_page_occlusion(&mut nodes, dom, layout);
    // Before redundant text goes: a named cell's text is part of its row.
    name_controls_by_row(&mut nodes, dom);
    remove_redundant_static_text(&mut nodes);
    SemanticDocument {
        title: None,
        nodes,
        css_hidden_dom_count: dom.css_hidden_count,
        unprovable_frame_count: 0,
        complete: true,
    }
}

/// Chrome reports the nodes it ignores (a plain wrapper `div`, a table's row
/// group) with their children still pointing at them. The snapshot drops
/// them, so each kept node hangs under its nearest kept ancestor, and a
/// dropped child's own children are listed in its place. Without this the
/// children of TodoMVC's `div.view` showed at the outline's root and its
/// `listitem` lines were empty.
fn relink_past_dropped_nodes(nodes: &mut [SemanticNode], ax_nodes: &[Value]) {
    let raw: HashMap<&str, &Value> = ax_nodes
        .iter()
        .filter_map(|ax| Some((ax.get("nodeId")?.as_str()?, ax)))
        .collect();
    let kept: HashSet<String> = nodes.iter().map(|node| node.ax_id.clone()).collect();
    let field = |id: &str, name: &str| raw.get(id).and_then(|ax| ax.get(name)).cloned();
    for node in nodes.iter_mut() {
        let mut parent = node.parent_ax_id.take();
        let mut seen = HashSet::new();
        while let Some(id) = parent.clone() {
            if kept.contains(&id) || !seen.insert(id.clone()) {
                break;
            }
            parent = field(&id, "parentId").and_then(|v| v.as_str().map(str::to_owned));
        }
        node.parent_ax_id = parent.filter(|id| kept.contains(id));

        let mut children = Vec::with_capacity(node.child_ax_ids.len());
        let mut stack: Vec<String> = node.child_ax_ids.drain(..).rev().collect();
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if kept.contains(&id) {
                children.push(id);
            } else if let Some(Value::Array(ids)) = field(&id, "childIds") {
                stack.extend(ids.iter().rev().filter_map(Value::as_str).map(str::to_owned));
            }
        }
        node.child_ax_ids = children;
    }
}

/// Longest row name, in characters.
const ROW_NAME_CHARS: usize = 80;
/// How far up the DOM a control's row is looked for.
const ROW_LEVELS: usize = 5;
/// How far up from a piece of text, or another control, its row may be.
const ROW_TEXT_LEVELS: usize = ROW_LEVELS + 6;

/// Name each checkbox, radio and switch that has no accessible name after
/// the text of its row: the nearest DOM ancestor, up to [`ROW_LEVELS`] up,
/// whose subtree holds visible text and no other such control. Text inside a
/// button, link or field belongs to that element, not to the row. TodoMVC's
/// item toggle has no label (the item's text is a sibling `<label>`), nor
/// has a plain `<li><input type=checkbox> Water plants`; without a name
/// neither can be told apart in an outline or aimed at by name.
fn name_controls_by_row(nodes: &mut [SemanticNode], dom: &DomIndex) {
    let toggle = |role: &str| matches!(role, "checkbox" | "radio" | "switch");
    if !nodes
        .iter()
        .any(|node| toggle(&node.role) && node.name.is_none())
    {
        return;
    }
    let parent = |backend: i64| dom.nodes.get(&backend)?.parent_backend_node_id;
    let owners: HashSet<i64> = nodes
        .iter()
        .filter(|node| {
            matches!(
                node.role.as_str(),
                "button" | "link" | "textbox" | "searchbox" | "combobox" | "menuitem" | "tab"
            )
        })
        .filter_map(|node| node.backend_node_id)
        .collect();
    // Each ancestor's text (in document order) and number of toggles.
    let mut texts: HashMap<i64, Vec<(usize, &str)>> = HashMap::new();
    let mut toggles: HashMap<i64, usize> = HashMap::new();
    for node in nodes.iter() {
        let Some(backend) = node.backend_node_id else {
            continue;
        };
        if toggle(&node.role) {
            let mut above = parent(backend);
            for _ in 0..ROW_TEXT_LEVELS {
                let Some(ancestor) = above else { break };
                *toggles.entry(ancestor).or_default() += 1;
                above = parent(ancestor);
            }
        } else if matches!(node.role.as_str(), "statictext" | "text")
            && node.visibility != BrowserVisibility::CssHidden
        {
            let Some(text) = node.name.as_deref() else {
                continue;
            };
            let mut above = parent(backend);
            for _ in 0..ROW_TEXT_LEVELS {
                let Some(ancestor) = above else { break };
                texts
                    .entry(ancestor)
                    .or_default()
                    .push((node.document_order, text));
                if owners.contains(&ancestor) {
                    break;
                }
                above = parent(ancestor);
            }
        }
    }
    let mut names = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if !toggle(&node.role) || node.name.is_some() {
            continue;
        }
        let Some(backend) = node.backend_node_id else {
            continue;
        };
        let mut above = parent(backend);
        for levels in 1..=ROW_LEVELS {
            let Some(ancestor) = above else { break };
            if toggles.get(&ancestor).copied().unwrap_or(0) > 1 {
                break;
            }
            if let Some(found) = texts.get(&ancestor) {
                let mut found = found.clone();
                found.sort_by_key(|(order, _)| *order);
                let joined = found.iter().map(|(_, text)| *text).collect::<Vec<_>>().join(" ");
                if let Some(mut name) = clean_semantic_text(joined) {
                    if name.chars().count() > ROW_NAME_CHARS {
                        name = name.chars().take(ROW_NAME_CHARS - 1).collect::<String>();
                        name.push('…');
                    }
                    names.push((index, RowName { name, levels }));
                }
                break;
            }
            above = parent(ancestor);
        }
    }
    for (index, row) in names {
        nodes[index].row_name = Some(row);
    }
}

fn apply_page_occlusion(nodes: &mut [SemanticNode], dom: &DomIndex, layout: &LayoutIndex) {
    for node in nodes {
        if node.visibility != BrowserVisibility::InViewport {
            continue;
        }
        let Some(target_backend) = node.backend_node_id else {
            continue;
        };
        let Some(target) = layout.nodes.get(&target_backend) else {
            continue;
        };
        let (Some(target_bounds), Some(target_paint)) = (target.bounds, target.paint_order) else {
            continue;
        };
        let covered = layout.nodes.iter().any(|(&backend, overlay)| {
            if backend == target_backend
                || dom.shares_dom_branch(backend, target_backend)
                || overlay
                    .paint_order
                    .is_none_or(|paint| paint <= target_paint)
                || !overlay
                    .styles
                    .get("position")
                    .is_some_and(|position| matches!(position.as_str(), "fixed" | "absolute"))
                || overlay
                    .styles
                    .get("pointer-events")
                    .is_some_and(|value| value == "none")
                || layout_hidden(overlay)
            {
                return false;
            }
            overlay
                .bounds
                .is_some_and(|bounds| bounds.has_area() && bounds.covers(target_bounds))
        });
        if covered {
            node.visibility = BrowserVisibility::PageOccluded;
        }
    }
}

fn supplement_dom_actions(
    nodes: &mut Vec<SemanticNode>,
    dom: &DomIndex,
    layout: &LayoutIndex,
    viewport: &Viewport,
    frame: &FrameRef,
) {
    let existing = nodes
        .iter()
        .filter_map(|node| node.backend_node_id)
        .collect::<HashSet<_>>();
    let expected_frame = frame
        .identity
        .as_ref()
        .map(|identity| identity.frame_id.as_str());
    let by_backend = nodes
        .iter()
        .filter_map(|node| Some((node.backend_node_id?, node.ax_id.clone())))
        .collect::<HashMap<_, _>>();
    let mut candidates = dom.nodes.iter().collect::<Vec<_>>();
    candidates.sort_by_key(|(_, meta)| meta.order);
    for (&backend_node_id, meta) in candidates {
        if existing.contains(&backend_node_id)
            || meta.own_indicator
            || expected_frame.is_some_and(|expected| meta.frame_id.as_deref() != Some(expected))
        {
            continue;
        }
        let layout_meta = layout.nodes.get(&backend_node_id);
        let visibility = classify_visibility(Some(meta), layout_meta, viewport);
        if matches!(
            visibility,
            BrowserVisibility::CssHidden | BrowserVisibility::PageOccluded
        ) {
            continue;
        }
        let role = dom_role(&meta.tag, &meta.attrs);
        let mut states = BTreeMap::new();
        if meta.attrs.contains_key("disabled") {
            states.insert("disabled".to_owned(), Value::Bool(true));
        }
        let actions = action_kinds(&role, Some(meta), &states, layout_meta);
        if actions.is_empty() {
            continue;
        }
        let name = dom_name(&meta.attrs);
        nodes.push(SemanticNode {
            ax_id: format!("dom-{backend_node_id}"),
            // The nearest DOM ancestor the outline has (its parent may be a
            // wrapper accessibility ignores).
            parent_ax_id: std::iter::successors(meta.parent_backend_node_id, |id| {
                dom.nodes.get(id)?.parent_backend_node_id
            })
            .take(dom.nodes.len())
            .find_map(|parent| by_backend.get(&parent).cloned()),
            child_ax_ids: Vec::new(),
            backend_node_id: Some(backend_node_id),
            url: link_destination(&role, None, Some(meta)),
            role,
            name,
            row_name: None,
            root_title: None,
            value: meta
                .attrs
                .get("value")
                .cloned()
                .and_then(clean_semantic_text),
            states,
            frame: frame.clone(),
            visibility,
            actions,
            document_order: meta.order,
        });
    }
}

/// The role a DOM node is given when it has no accessibility node.
fn dom_role(tag: &str, attrs: &HashMap<String, String>) -> String {
    attrs
        .get("role")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| match tag {
            "a" => "link".to_owned(),
            "button" => "button".to_owned(),
            "input" => match attrs.get("type").map(String::as_str) {
                Some("checkbox") => "checkbox".to_owned(),
                Some("radio") => "radio".to_owned(),
                Some("button" | "submit" | "reset" | "image") => "button".to_owned(),
                Some("range") => "slider".to_owned(),
                _ => "textbox".to_owned(),
            },
            "textarea" => "textbox".to_owned(),
            "select" => "combobox".to_owned(),
            "option" => "option".to_owned(),
            "summary" => "summary".to_owned(),
            _ => "generic".to_owned(),
        })
}

/// The name a DOM node is given when it has no accessibility node.
fn dom_name(attrs: &HashMap<String, String>) -> Option<String> {
    ["aria-label", "placeholder", "title", "name", "id"]
        .iter()
        .find_map(|key| attrs.get(*key).cloned())
        .and_then(clean_semantic_text)
}

/// What one accessibility node reads as, by the rules a snapshot uses: its
/// role, name, and the link destination the node itself reports. `None` for
/// a node accessibility ignores.
pub(crate) fn ax_reading(ax: &Value) -> Option<(String, Option<String>, Option<String>)> {
    if ax.get("ignored").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let role = ax_value_string(ax.get("role"))
        .unwrap_or_else(|| "unknown".to_owned())
        .to_ascii_lowercase();
    let name = ax_value_string(ax.get("name")).and_then(clean_semantic_text);
    let destination = link_destination(&role, Some(ax), None);
    Some((role, name, destination))
}

/// The same for a node as `DOM.describeNode` reports it (the DOM
/// supplement's rules).
pub(crate) fn dom_reading(node: &Value) -> (String, Option<String>) {
    let tag = node
        .get("nodeName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let attrs = attributes(node);
    (dom_role(&tag, &attrs), dom_name(&attrs))
}

// Keep destination data separate from display text: text cleanup can truncate
// or rewrite a URL. CDP's accessibility URL is already resolved by the browser.
fn link_destination(role: &str, ax: Option<&Value>, dom: Option<&DomMeta>) -> Option<String> {
    if role != "link" && !dom.is_some_and(|meta| matches!(meta.tag.as_str(), "a" | "area")) {
        return None;
    }
    if let Some(url) = ax
        .and_then(|node| node.get("properties"))
        .and_then(Value::as_array)
        .and_then(|properties| properties.iter().find(|p| p["name"] == "url"))
        .and_then(|property| property.pointer("/value/value"))
        .and_then(Value::as_str)
        .filter(|value| url::Url::parse(value).is_ok())
    {
        return Some(url.to_owned());
    }
    let dom = dom?;
    if !matches!(dom.tag.as_str(), "a" | "area") {
        return None;
    }
    let href = dom.attrs.get("href")?;
    // Resolve against the containing document's base as a browser does: a
    // same-scheme reference such as "https:book" is relative, not absolute.
    // Only without a base does the href stand alone.
    match dom
        .base_url
        .as_deref()
        .and_then(|base| url::Url::parse(base).ok())
    {
        Some(base) => base.join(href).ok().map(Into::into),
        None => url::Url::parse(href).ok().map(Into::into),
    }
}

fn attributes(node: &Value) -> HashMap<String, String> {
    node.get("attributes")
        .and_then(Value::as_array)
        .map(|attrs| {
            attrs
                .chunks_exact(2)
                .filter_map(|pair| {
                    Some((
                        pair[0].as_str()?.to_ascii_lowercase(),
                        pair[1].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn statically_hidden(tag: &str, attrs: &HashMap<String, String>) -> bool {
    if attrs.contains_key("hidden") || attrs.get("aria-hidden").is_some_and(|v| v == "true") {
        return true;
    }
    let style = attrs
        .get("style")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    style.split(';').any(|declaration| {
        let Some((property, value)) = declaration.split_once(':') else {
            return false;
        };
        let value = value.trim().trim_end_matches("!important").trim();
        match property.trim() {
            "display" => value == "none",
            "visibility" => value == "hidden",
            "opacity" => {
                !native_control(tag, attrs) && value.parse::<f64>().is_ok_and(|opacity| opacity <= 0.0)
            }
            _ => false,
        }
    })
}

/// A native form control: pages lay a transparent (opacity 0) one over the
/// control they draw, and the click lands on it.
fn native_control(tag: &str, attrs: &HashMap<String, String>) -> bool {
    tag == "select"
        || (tag == "input"
            && !attrs
                .get("type")
                .is_some_and(|kind| kind.eq_ignore_ascii_case("hidden")))
}

/// Hidden by its computed style, for the working set. Transparency alone
/// does not hide a native control that still takes clicks (see
/// [`native_control`]); display and visibility always hide.
fn hidden_by_layout(dom: Option<&DomMeta>, layout: &LayoutMeta) -> bool {
    if !layout_hidden(layout) {
        return false;
    }
    let style = |name: &str, value: &str| {
        layout
            .styles
            .get(name)
            .is_some_and(|held| held.eq_ignore_ascii_case(value))
    };
    let transparent_only =
        !style("display", "none") && !style("visibility", "hidden") && !style("pointer-events", "none");
    !(transparent_only && dom.is_some_and(|meta| native_control(&meta.tag, &meta.attrs)))
}

fn layout_hidden(layout: &LayoutMeta) -> bool {
    layout
        .styles
        .get("display")
        .is_some_and(|value| value.eq_ignore_ascii_case("none"))
        || layout
            .styles
            .get("visibility")
            .is_some_and(|value| value.eq_ignore_ascii_case("hidden"))
        || layout
            .styles
            .get("opacity")
            .and_then(|value| value.parse::<f64>().ok())
            .is_some_and(|opacity| opacity <= 0.0)
}

fn classify_visibility(
    dom: Option<&DomMeta>,
    layout: Option<&LayoutMeta>,
    viewport: &Viewport,
) -> BrowserVisibility {
    if dom.is_some_and(|meta| meta.css_hidden)
        || layout.is_some_and(|layout| hidden_by_layout(dom, layout))
    {
        return BrowserVisibility::CssHidden;
    }
    let Some(layout) = layout else {
        return BrowserVisibility::Unknown;
    };
    let Some(bounds) = layout.bounds else {
        return BrowserVisibility::NoLayout;
    };
    if !bounds.has_area() {
        return BrowserVisibility::NoLayout;
    }
    let Some(viewport) = viewport.rect else {
        return BrowserVisibility::Unknown;
    };
    if bounds.intersects(viewport) {
        BrowserVisibility::InViewport
    } else if bounds.intersects(viewport.expanded(NEAR_VIEWPORT_MARGIN)) {
        BrowserVisibility::NearViewport
    } else {
        BrowserVisibility::Offscreen
    }
}

fn action_kinds(
    role: &str,
    dom: Option<&DomMeta>,
    states: &BTreeMap<String, Value>,
    layout: Option<&LayoutMeta>,
) -> Vec<BrowserActionKind> {
    if states.get("disabled").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if matches!(
        role,
        "button"
            | "link"
            | "checkbox"
            | "radio"
            | "switch"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "option"
            | "treeitem"
            | "slider"
            | "spinbutton"
            | "combobox"
            | "listbox"
            | "summary"
    ) {
        actions.push(BrowserActionKind::Click);
    }
    let tag = dom.map(|meta| meta.tag.as_str()).unwrap_or("");
    let editable = states.get("editable").is_some_and(|value| {
        value.as_bool() == Some(true) || value.as_str().is_some_and(|value| value != "false")
    }) || dom.is_some_and(|meta| {
        meta.attrs
            .get("contenteditable")
            .is_some_and(|value| value.is_empty() || value.eq_ignore_ascii_case("true"))
    });
    let file_input = tag == "input"
        && dom.is_some_and(|meta| {
            meta.attrs
                .get("type")
                .is_some_and(|value| value.eq_ignore_ascii_case("file"))
        });
    // An input that takes no text (a checkbox, a button) is not typed into;
    // the same list as browser_type's own editability check.
    let text_input = tag == "input"
        && !dom
            .and_then(|meta| meta.attrs.get("type"))
            .is_some_and(|kind| {
                matches!(
                    kind.to_ascii_lowercase().as_str(),
                    "button"
                        | "checkbox"
                        | "color"
                        | "hidden"
                        | "image"
                        | "radio"
                        | "range"
                        | "reset"
                        | "submit"
                )
            });
    if file_input {
        actions.push(BrowserActionKind::Upload);
    } else if matches!(role, "textbox" | "searchbox") || text_input || tag == "textarea" || editable
    {
        actions.push(BrowserActionKind::Type);
    }
    if actions.is_empty()
        && dom.is_some_and(|meta| {
            meta.attrs.contains_key("onclick")
                || meta
                    .attrs
                    .get("tabindex")
                    .is_some_and(|value| value != "-1")
                || (!matches!(meta.tag.as_str(), "html" | "body")
                    && layout.is_some_and(|layout| {
                        layout
                            .styles
                            .get("cursor")
                            .is_some_and(|cursor| cursor.eq_ignore_ascii_case("pointer"))
                            && !layout
                                .styles
                                .get("pointer-events")
                                .is_some_and(|value| value.eq_ignore_ascii_case("none"))
                    }))
        })
        && layout.is_some_and(|meta| meta.bounds.is_some_and(Rect::has_area))
    {
        actions.push(BrowserActionKind::Click);
    }
    if let Some(layout) = layout.filter(|meta| {
        meta.bounds.is_some_and(Rect::has_area)
            && !meta
                .styles
                .get("pointer-events")
                .is_some_and(|value| value.eq_ignore_ascii_case("none"))
    }) {
        if !actions.is_empty() {
            actions.push(BrowserActionKind::Pointer);
        }
        if layout_is_scrollable(layout) {
            actions.push(BrowserActionKind::Scroll);
        }
    }
    actions
}

fn layout_is_scrollable(layout: &LayoutMeta) -> bool {
    let (Some(client), Some(scroll)) = (layout.client_rect, layout.scroll_rect) else {
        return false;
    };
    let scrollable_axis = |overflow: Option<&String>, scroll_extent: f64, client_extent: f64| {
        overflow.is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "auto" | "scroll" | "overlay"
            )
        }) && scroll_extent > client_extent + 0.5
    };
    scrollable_axis(layout.styles.get("overflow-x"), scroll.width, client.width)
        || scrollable_axis(
            layout.styles.get("overflow-y"),
            scroll.height,
            client.height,
        )
}

fn ax_states(node: &Value) -> BTreeMap<String, Value> {
    const KEPT: &[&str] = &[
        "checked",
        "disabled",
        "editable",
        "expanded",
        "focused",
        "focusable",
        "pressed",
        "required",
        "selected",
    ];
    node.get("properties")
        .and_then(Value::as_array)
        .map(|properties| {
            properties
                .iter()
                .filter_map(|property| {
                    let name = property.get("name").and_then(Value::as_str)?;
                    KEPT.contains(&name).then(|| {
                        (
                            name.to_owned(),
                            property
                                .pointer("/value/value")
                                .cloned()
                                .unwrap_or(Value::Null),
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn ax_value_string(value: Option<&Value>) -> Option<String> {
    let value = value?.get("value")?;
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn clean_semantic_text(value: String) -> Option<String> {
    let normalized = value
        .chars()
        .map(|ch| match ch {
            '\u{feff}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{00a0}'
            | '\u{2007}' | '\u{202f}' => ' ',
            '\u{e000}'..='\u{f8ff}' => ' ',
            _ => ch,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(normalized.chars().take(MAX_SEMANTIC_TEXT_CHARS).collect())
}

fn remove_redundant_static_text(nodes: &mut Vec<SemanticNode>) {
    let names: HashMap<String, String> = nodes
        .iter()
        .filter_map(|node| node.name.clone().map(|name| (node.ax_id.clone(), name)))
        .collect();
    nodes.retain(|node| {
        if node.role != "statictext" && node.role != "text" {
            return true;
        }
        let Some(name) = node.name.as_deref() else {
            return false;
        };
        node.parent_ax_id
            .as_ref()
            .and_then(|parent| names.get(parent))
            .is_none_or(|parent_name| parent_name != name)
    });
}

fn rank(node: &SemanticNode) -> u8 {
    let priority_context = node.states.get("focused").and_then(Value::as_bool) == Some(true)
        || matches!(node.role.as_str(), "dialog" | "alertdialog");
    if priority_context {
        return 0;
    }
    match (node.visibility, node.actions.is_empty()) {
        (BrowserVisibility::InViewport, false) => 1,
        (BrowserVisibility::InViewport, true) => 2,
        (BrowserVisibility::NearViewport, false) => 3,
        (BrowserVisibility::NearViewport, true) => 4,
        (BrowserVisibility::Unknown | BrowserVisibility::NoLayout, false) => 5,
        (BrowserVisibility::Unknown | BrowserVisibility::NoLayout, true) => 6,
        (BrowserVisibility::Offscreen, false) => 7,
        (BrowserVisibility::Offscreen, true) => 8,
        (BrowserVisibility::CssHidden | BrowserVisibility::PageOccluded, _) => 9,
    }
}

fn scoped_indices(
    nodes: &[SemanticNode],
    query: Option<&str>,
    scope_backend_node_id: Option<i64>,
) -> Vec<usize> {
    let by_ax_id: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.ax_id.as_str(), idx))
        .collect();
    let mut allowed = HashSet::new();
    if let Some(scope_backend) = scope_backend_node_id {
        if let Some(scope) = nodes
            .iter()
            .find(|node| node.backend_node_id == Some(scope_backend))
        {
            let mut stack = vec![scope.ax_id.as_str()];
            while let Some(ax_id) = stack.pop() {
                if !allowed.insert(ax_id.to_owned()) {
                    continue;
                }
                if let Some(idx) = by_ax_id.get(ax_id) {
                    stack.extend(nodes[*idx].child_ax_ids.iter().map(String::as_str));
                }
            }
        }
    }
    let query = query.map(|value| value.trim().to_ascii_lowercase());
    let in_scope =
        |node: &SemanticNode| scope_backend_node_id.is_none() || allowed.contains(&node.ax_id);
    let exact_match_exists = query.as_ref().is_some_and(|query| {
        !query.is_empty()
            && nodes
                .iter()
                .any(|node| in_scope(node) && node_contains_query(node, query))
    });
    nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            in_scope(node)
                && query.as_ref().is_none_or(|query| {
                    query.is_empty()
                        || if exact_match_exists {
                            node_contains_query(node, query)
                        } else {
                            query_score(node, query) > 0
                        }
                })
        })
        .map(|(idx, _)| idx)
        .collect()
}

fn node_contains_query(node: &SemanticNode, query: &str) -> bool {
    node.role.to_ascii_lowercase().contains(query)
        || node
            .shown_name()
            .is_some_and(|name| name.to_ascii_lowercase().contains(query))
        || node
            .value
            .as_ref()
            .is_some_and(|value| value.to_ascii_lowercase().contains(query))
}

fn query_score(node: &SemanticNode, query: &str) -> usize {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return 0;
    }
    if node_contains_query(node, &query) {
        return usize::MAX;
    }
    let fields = [
        Some(node.role.as_str()),
        node.shown_name(),
        node.value.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_ascii_lowercase)
    .collect::<Vec<_>>();
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .filter(|term| fields.iter().any(|field| field.contains(term)))
        .count()
}

fn with_ancestors(nodes: &[SemanticNode], selected: &[usize]) -> HashSet<usize> {
    let by_ax_id: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.ax_id.as_str(), idx))
        .collect();
    let mut keep: HashSet<usize> = selected.iter().copied().collect();
    for idx in selected {
        let mut parent = nodes[*idx].parent_ax_id.as_deref();
        while let Some(parent_id) = parent {
            let Some(parent_idx) = by_ax_id.get(parent_id).copied() else {
                break;
            };
            if !keep.insert(parent_idx) {
                break;
            }
            parent = nodes[parent_idx].parent_ax_id.as_deref();
        }
    }
    keep
}

fn ax_index(nodes: &[SemanticNode]) -> HashMap<&str, usize> {
    nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.ax_id.as_str(), idx))
        .collect()
}

/// A node that says nothing by itself: a document root, a list bullet, an
/// unnamed wrapper with no action, the inner editing box of a field that is
/// itself listed, or the text inside a field that repeats the field's value.
/// Its children are shown under the nearest listed ancestor.
fn is_structure_only(nodes: &[SemanticNode], by_ax_id: &HashMap<&str, usize>, idx: usize) -> bool {
    let node = &nodes[idx];
    if matches!(node.role.as_str(), "rootwebarea" | "webarea" | "listmarker") {
        return true;
    }
    let parent = |node: &SemanticNode| {
        node.parent_ax_id
            .as_deref()
            .and_then(|parent| by_ax_id.get(parent))
            .map(|parent| &nodes[*parent])
    };
    if matches!(node.role.as_str(), "statictext" | "text") && node.name.is_some() {
        // An input's text sits under its inner editing box: look two up.
        let mut above = parent(node);
        for _ in 0..2 {
            let Some(field) = above else { break };
            if field.actions.contains(&BrowserActionKind::Type) && field.value == node.name {
                return true;
            }
            above = parent(field);
        }
    }
    if !matches!(node.role.as_str(), "generic" | "none" | "presentation")
        || node.name.is_some()
        || node.value.is_some()
    {
        return false;
    }
    node.actions.is_empty()
        || parent(node).is_some_and(|parent| parent.actions.contains(&BrowserActionKind::Type))
}

/// JSON-escaped length of the outline these lines make, refs counted at
/// [`BUDGETED_REF`] width.
fn serialized_chars(view: &[ViewNode]) -> usize {
    view.iter()
        .map(|node| {
            let line = node.template.replace(REF_SLOT, BUDGETED_REF);
            serde_json::to_string(&line).map_or(line.len(), |json| json.chars().count() - 2)
        })
        .sum::<usize>()
        + view.len().saturating_sub(1) * 2
}

/// One entry of the optional structured ref list.
pub(crate) fn listed_ref(reference: &str, entry: &RefEntry, value: Option<&str>) -> Value {
    let mut listed = serde_json::json!({
        "ref": reference,
        "role": entry.node_name,
        "name": entry.label,
        "value": value,
        "actions": entry.actions.iter().map(|action| action.as_str()).collect::<Vec<_>>(),
    });
    if let Some(url) = &entry.destination {
        listed["url"] = Value::String(url.clone());
    }
    listed
}

/// Serialized length of the structured ref list for these lines.
fn listed_chars(view: &[ViewNode], values: &[Option<String>]) -> usize {
    view.iter()
        .zip(values)
        .map(|(node, value)| {
            listed_ref(BUDGETED_REF, &node.entry, value.as_deref())
                .to_string()
                .chars()
                .count()
                + 1
        })
        .sum()
}

/// The kept nodes as outline lines with their values, walked as a tree: a
/// line sits under its nearest listed ancestor, siblings in document order.
fn render_view(
    nodes: &[SemanticNode],
    by_ax_id: &HashMap<&str, usize>,
    keep: &HashSet<usize>,
) -> (Vec<ViewNode>, Vec<Option<String>>) {
    let listed: HashSet<usize> = keep
        .iter()
        .copied()
        .filter(|idx| {
            nodes[*idx].backend_node_id.is_some() && !is_structure_only(nodes, by_ax_id, *idx)
        })
        .collect();
    let mut ordered: Vec<usize> = listed.iter().copied().collect();
    ordered.sort_by_key(|idx| (nodes[*idx].document_order, *idx));
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for idx in ordered {
        let mut above = nodes[idx].parent_ax_id.as_deref();
        let mut parent = None;
        // Bounded by the node count, so a malformed parent cycle ends.
        for _ in 0..nodes.len() {
            let Some(parent_idx) = above.and_then(|id| by_ax_id.get(id)).copied() else {
                break;
            };
            if listed.contains(&parent_idx) {
                parent = Some(parent_idx);
                break;
            }
            above = nodes[parent_idx].parent_ax_id.as_deref();
        }
        children.entry(parent).or_default().push(idx);
    }
    let mut view = Vec::with_capacity(listed.len());
    let mut values = Vec::with_capacity(listed.len());
    let mut stack: Vec<(usize, usize)> = children
        .get(&None)
        .into_iter()
        .flatten()
        .rev()
        .map(|idx| (*idx, 0))
        .collect();
    while let Some((idx, depth)) = stack.pop() {
        if let Some(entry) = nodes[idx].to_ref_entry() {
            view.push(ViewNode {
                entry,
                template: line_template(&nodes[idx], depth),
            });
            values.push(nodes[idx].value.clone());
        }
        stack.extend(
            children
                .get(&Some(idx))
                .into_iter()
                .flatten()
                .rev()
                .map(|child| (*child, depth + 1)),
        );
    }
    (view, values)
}

/// An outline line read back into the parts a program matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutlineLine {
    pub(crate) role: String,
    pub(crate) name: Option<String>,
    pub(crate) reference: String,
    pub(crate) actions: Vec<String>,
    pub(crate) value: Option<String>,
    /// The line without its indentation.
    pub(crate) line: String,
}

/// Read one line written by [`line_template`] (with its ref filled in).
pub(crate) fn parse_outline_line(line: &str) -> Option<OutlineLine> {
    fn quoted(text: &str) -> Option<(String, &str)> {
        let mut stream = serde_json::Deserializer::from_str(text).into_iter::<String>();
        let value = stream.next()?.ok()?;
        Some((value, &text[stream.byte_offset()..]))
    }
    let trimmed = line.trim_start();
    let (role, mut rest) = trimmed.strip_prefix("- ")?.split_once(' ')?;
    let mut name = None;
    if rest.starts_with('"') {
        let (text, after) = quoted(rest)?;
        name = Some(text);
        rest = after.strip_prefix(' ')?;
    }
    let (bracket, rest) = rest.strip_prefix('[')?.split_once(']')?;
    let (reference, actions) = bracket.split_once(' ').unwrap_or((bracket, ""));
    let value = rest
        .strip_prefix(" = ")
        .and_then(quoted)
        .map(|(value, _)| value);
    Some(OutlineLine {
        role: role.to_owned(),
        name,
        reference: reference.to_owned(),
        actions: actions
            .split(',')
            .filter(|action| !action.is_empty())
            .map(str::to_owned)
            .collect(),
        value,
        line: trimmed.to_owned(),
    })
}

/// One outline line: `- role "name" [ref actions] = "value" -> "url" (states)`.
/// The bracket holds the ref and what it may be used for; a ref with no
/// action there only scopes a later read.
fn line_template(node: &SemanticNode, depth: usize) -> String {
    let quoted = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned());
    let mut line = format!("{}- {}", "  ".repeat(depth), node.role);
    if let Some(name) = node.shown_name() {
        line.push(' ');
        line.push_str(&quoted(name));
    }
    line.push_str(" [");
    line.push(REF_SLOT);
    // `pointer` comes with any other action on a node that has a box; leave
    // it off the line.
    let actions = node
        .actions
        .iter()
        .filter(|action| **action != BrowserActionKind::Pointer)
        .map(|action| action.as_str())
        .collect::<Vec<_>>();
    if !actions.is_empty() {
        line.push(' ');
        line.push_str(&actions.join(","));
    }
    line.push(']');
    if let Some(value) = node
        .value
        .as_ref()
        .filter(|value| node.shown_name() != Some(value.as_str()))
    {
        line.push_str(" = ");
        line.push_str(&quoted(value));
    }
    if let Some(url) = &node.url {
        line.push_str(" -> ");
        line.push_str(&quoted(url));
    }
    let mut states = Vec::new();
    for (name, value) in &node.states {
        let word = match (name.as_str(), value) {
            // What the role and the bracket already say.
            ("focusable" | "editable", _) => continue,
            ("expanded", Value::Bool(false)) => "collapsed".to_owned(),
            ("checked", Value::Bool(false)) => "unchecked".to_owned(),
            (_, Value::Bool(true)) => name.clone(),
            (_, Value::Bool(false)) | (_, Value::Null) => continue,
            (_, Value::String(text)) if text == "true" => name.clone(),
            (_, Value::String(text)) if text == "false" => match name.as_str() {
                "expanded" => "collapsed".to_owned(),
                "checked" => "unchecked".to_owned(),
                _ => continue,
            },
            (_, Value::String(text)) => format!("{name}={text}"),
            (_, other) => format!("{name}={other}"),
        };
        states.push(word);
    }
    if node.frame.kind != FrameKind::Main {
        states.push(node.frame.kind.as_str().to_owned());
    }
    if !matches!(
        node.visibility,
        BrowserVisibility::InViewport | BrowserVisibility::Unknown
    ) {
        states.push(node.visibility.as_str().to_owned());
    }
    if !states.is_empty() {
        line.push_str(&format!(" ({})", states.join(", ")));
    }
    line
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn document_title_requires_matching_root_metadata() {
        let root =
            json!({"backendNodeId": 7, "documentURL": "https://fixture.test/", "frameId": "MAIN"});
        let snapshot = json!({
            "strings": ["https://fixture.test/", "MAIN", "Current title", "CHILD"],
            "documents": [{"documentURL": 0, "frameId": 1, "title": 2,
                "nodes": {"backendNodeId": [7]}}]
        });
        assert_eq!(
            snapshot_document_title(&snapshot, &root).as_deref(),
            Some("Current title")
        );
        for (field, value) in [
            ("title", json!(-1)),
            ("title", json!(999)),
            ("title", json!("unindexed title")),
            ("documentURL", json!(3)),
            ("frameId", json!(3)),
            ("nodes", json!({"backendNodeId": [8]})),
        ] {
            let mut changed = snapshot.clone();
            changed["documents"][0][field] = value;
            assert_eq!(snapshot_document_title(&changed, &root), None, "{field}");
        }
        let mut missing = snapshot.clone();
        missing["documents"][0]
            .as_object_mut()
            .unwrap()
            .remove("title");
        assert_eq!(snapshot_document_title(&missing, &root), None);
        let mut empty = snapshot;
        empty["strings"][2] = json!("");
        assert_eq!(snapshot_document_title(&empty, &root).as_deref(), Some(""));
    }

    #[test]
    fn document_title_does_not_select_an_embedded_document() {
        let root =
            json!({"backendNodeId": 7, "documentURL": "https://fixture.test/", "frameId": "MAIN"});
        let snapshot = json!({
            "strings": ["https://fixture.test/", "MAIN", "Main title", "CHILD", "Child title"],
            "documents": [
                {"documentURL": 0, "frameId": 1, "title": 2, "nodes": {"backendNodeId": [7]}},
                {"documentURL": 0, "frameId": 3, "title": 4, "nodes": {"backendNodeId": [8]}}
            ]
        });
        assert_eq!(
            snapshot_document_title(&snapshot, &root).as_deref(),
            Some("Main title")
        );
        let mut reordered = snapshot;
        reordered["documents"].as_array_mut().unwrap().reverse();
        assert_eq!(snapshot_document_title(&reordered, &root), None);
    }

    #[test]
    fn link_destination_uses_frame_base_and_preserves_exact_ax_url() {
        let root = json!({"nodeType":9,"baseURL":"https://outer.test/base/", "children":[
            {"nodeType":1,"nodeName":"A","backendNodeId":1,"attributes":["href","../book?q=a%20b#slot"]},
            {"nodeType":1,"nodeName":"IFRAME","contentDocument":{"nodeType":9,"baseURL":"https://inner.test/custom/",
                "children":[{"nodeType":1,"nodeName":"A","backendNodeId":2,"attributes":["href","book"]}]}},
            {"nodeType":1,"nodeName":"IFRAME","contentDocument":{"nodeType":9,
                "children":[{"nodeType":1,"nodeName":"A","backendNodeId":3,"attributes":["href","book"]}]}}
        ]});
        let dom = build_dom_index(&root);
        assert_eq!(
            link_destination("link", None, dom.nodes.get(&1)).as_deref(),
            Some("https://outer.test/book?q=a%20b#slot")
        );
        assert_eq!(
            link_destination("link", None, dom.nodes.get(&2)).as_deref(),
            Some("https://inner.test/custom/book")
        );
        assert!(link_destination("link", None, dom.nodes.get(&3)).is_none());
        let scheme_relative = json!({"nodeType":9,"baseURL":"https://example.test/base/page",
            "children":[{"nodeType":1,"nodeName":"A","backendNodeId":4,"attributes":["href","https:book"]}]});
        let dom = build_dom_index(&scheme_relative);
        assert_eq!(
            link_destination("link", None, dom.nodes.get(&4)).as_deref(),
            Some("https://example.test/base/book"),
            "same-scheme reference resolves against the base"
        );
        let no_base = json!({"nodeType":9,
            "children":[{"nodeType":1,"nodeName":"A","backendNodeId":5,"attributes":["href","HTTPS://Example.test/a b"]}]});
        let dom = build_dom_index(&no_base);
        assert_eq!(
            link_destination("link", None, dom.nodes.get(&5)).as_deref(),
            Some("https://example.test/a%20b"),
            "without a base, the parsed URL is serialized"
        );
        let dom = build_dom_index(&root);
        let long_url = format!("https://resolved.test/?q={}%2F#slot", "x".repeat(1200));
        let ax = json!({"properties":[{"name":"url","value":{"type":"string","value":long_url}}]});
        assert_eq!(
            link_destination("link", Some(&ax), dom.nodes.get(&1)),
            Some(long_url)
        );
        assert!(link_destination("textbox", Some(&ax), None).is_none());
        assert!(link_destination("link", None, None).is_none());
    }

    #[test]
    fn link_destination_resolves_empty_fragment_and_protocol_relative_hrefs() {
        for (href, expected) in [
            ("", "https://example.test/base/page"),
            ("#court", "https://example.test/base/page#court"),
            ("?q=a%2Fb", "https://example.test/base/page?q=a%2Fb"),
            ("//other.test/book", "https://other.test/book"),
        ] {
            let dom = build_dom_index(
                &json!({"nodeType":9,"baseURL":"https://example.test/base/page",
                "children":[{"nodeType":1,"nodeName":"A","backendNodeId":1,"attributes":["href",href]}]}),
            );
            assert_eq!(
                link_destination("link", None, dom.nodes.get(&1)).as_deref(),
                Some(expected)
            );
        }
    }

    /// The title fallback reads the main root's name as reported: an empty
    /// title stays "" (the page has none), and text cleanup never rewrites it.
    #[test]
    fn document_title_keeps_the_root_name_exactly() {
        for (reported, expected) in [("", ""), ("  Two  spaces \u{a0}", "  Two  spaces \u{a0}")] {
            let ax = json!({"nodes":[{"nodeId":"root","role":{"value":"RootWebArea"},
                "name":{"value":reported}}]});
            let doc = compose_accessibility_tree(
                &ax,
                &DomIndex::default(),
                &LayoutIndex::default(),
                &Viewport::default(),
                frame(),
            );
            assert_eq!(doc.document_title(), Some(expected), "{reported:?}");
        }
    }

    #[test]
    fn link_destination_survives_semantic_composition_and_outline() {
        let dom = build_dom_index(&json!({"nodeType":9,"baseURL":"https://example.test/base/",
            "children":[{"nodeType":1,"nodeName":"A","backendNodeId":1,
                "attributes":["href","../book?q=a%20b#time"]}]}));
        let ax = json!({"nodes":[{"nodeId":"link","backendDOMNodeId":1,
            "role":{"value":"link"},"name":{"value":"Book a court"}}]});
        let doc = compose_accessibility_tree(
            &ax,
            &dom,
            &LayoutIndex::default(),
            &Viewport::default(),
            frame(),
        );
        let page = doc.page(0, 300, usize::MAX, None, None);
        assert_eq!(
            page.outline_with("p1:0"),
            "- link \"Book a court\" [p1:0 click] -> \"https://example.test/book?q=a%20b#time\""
        );
    }

    fn ax_node(
        id: &str,
        parent: Option<&str>,
        backend: Option<i64>,
        role: &str,
        name: Option<&str>,
    ) -> Value {
        let mut node = json!({"nodeId": id, "ignored": false, "role": {"value": role}});
        if let Some(parent) = parent {
            node["parentId"] = json!(parent);
        }
        if let Some(backend) = backend {
            node["backendDOMNodeId"] = json!(backend);
        }
        if let Some(name) = name {
            node["name"] = json!({"value": name});
        }
        node
    }

    fn share_form() -> SemanticDocument {
        let dom = build_dom_index(&json!({"nodeType": 9, "children": [
            {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 1},
            {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 2, "attributes": ["type", "email"]},
            {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 3},
            {"nodeType": 1, "nodeName": "UL", "backendNodeId": 4},
            {"nodeType": 1, "nodeName": "LI", "backendNodeId": 5},
            {"nodeType": 1, "nodeName": "LI", "backendNodeId": 6},
            {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 7, "attributes": ["disabled", ""]}
        ]}));
        let mut nodes = vec![
            ax_node("root", None, Some(90), "RootWebArea", Some("Share")),
            ax_node("card", Some("root"), Some(1), "generic", None),
            ax_node("email", Some("card"), Some(2), "textbox", Some("Email")),
            // The field's inner editing box and the text in it: no DOM
            // entry, typable by state, and only repeating the field's value.
            ax_node("inner", Some("email"), Some(20), "generic", None),
            ax_node(
                "typed",
                Some("inner"),
                Some(21),
                "StaticText",
                Some("ada@x.com"),
            ),
            ax_node("bullet", Some("viewer"), Some(22), "ListMarker", Some("•")),
            ax_node("role", Some("card"), Some(3), "button", Some("Role")),
            // A mock accessibility object: no DOM node at all.
            ax_node("popup", Some("role"), None, "menulistpopup", None),
            ax_node(
                "list",
                Some("popup"),
                Some(4),
                "listbox",
                Some("Role options"),
            ),
            ax_node("viewer", Some("list"), Some(5), "option", Some("Viewer")),
            ax_node("editor", Some("list"), Some(6), "option", Some("Editor")),
            ax_node("send", Some("card"), Some(7), "button", Some("Send invite")),
        ];
        nodes[2]["value"] = json!({"value": "ada@x.com"});
        nodes[2]["properties"] = json!([
            {"name": "focused", "value": {"value": true}},
            {"name": "focusable", "value": {"value": true}},
            {"name": "editable", "value": {"value": "plaintext"}}]);
        nodes[3]["properties"] = json!([{"name": "editable", "value": {"value": "plaintext"}}]);
        nodes[4]["properties"] = json!([{"name": "editable", "value": {"value": "plaintext"}}]);
        nodes[6]["properties"] = json!([{"name": "expanded", "value": {"value": true}}]);
        nodes[9]["properties"] = json!([{"name": "selected", "value": {"value": true}}]);
        nodes[11]["properties"] = json!([{"name": "disabled", "value": {"value": true}}]);
        compose_accessibility_tree(
            &json!({ "nodes": nodes }),
            &dom,
            &LayoutIndex::default(),
            &Viewport::default(),
            frame(),
        )
    }

    #[test]
    fn outline_lines_carry_the_ref_its_actions_the_value_and_the_states() {
        let page = share_form().page(0, 300, usize::MAX, None, None);
        assert_eq!(
            page.outline_with("R"),
            [
                "- textbox \"Email\" [R type] = \"ada@x.com\" (focused)",
                "- button \"Role\" [R click] (expanded)",
                "  - listbox \"Role options\" [R click]",
                "    - option \"Viewer\" [R click] (selected)",
                "    - option \"Editor\" [R click]",
                "- button \"Send invite\" [R] (disabled)",
            ]
            .join("\n")
        );
        // The unnamed wrapper, the field's inner box and its text, the list
        // bullet and the page root say nothing; the mock popup has no DOM
        // node to hang a ref on.
        assert_eq!(page.omissions.no_dom_node, 1);
        assert_eq!(page.selected_nodes, 6);
    }

    #[test]
    fn an_outline_line_reads_back_as_the_node_it_was_written_from() {
        let page = share_form().page(0, 300, usize::MAX, None, None);
        let outline = page.outline_with("p3:9");
        let lines: Vec<OutlineLine> = outline.lines().filter_map(parse_outline_line).collect();
        assert_eq!(lines.len(), outline.lines().count(), "every line parses");
        assert_eq!(
            (
                lines[0].role.as_str(),
                lines[0].name.as_deref(),
                lines[0].value.as_deref()
            ),
            ("textbox", Some("Email"), Some("ada@x.com"))
        );
        assert_eq!(lines[0].actions, vec!["type"]);
        assert_eq!(lines[3].reference, "p3:9");
        assert_eq!(
            lines[3].line, "- option \"Viewer\" [p3:9 click] (selected)",
            "indent dropped"
        );
        assert!(
            lines[5].actions.is_empty(),
            "a disabled button declares no action"
        );
        // A name with quotes, a bracket and a newline-free escape survives.
        let tricky = SemanticNode {
            name: Some("Say \"hi\" [now] = x".into()),
            ..page_node("button")
        };
        let line = line_template(&tricky, 2).replace(REF_SLOT, "p1:0");
        let parsed = parse_outline_line(&line).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("Say \"hi\" [now] = x"));
        assert_eq!(parsed.reference, "p1:0");
        assert_eq!(parse_outline_line("not an outline line"), None);
    }

    fn page_node(role: &str) -> SemanticNode {
        SemanticNode {
            ax_id: "n".into(),
            parent_ax_id: None,
            child_ax_ids: Vec::new(),
            backend_node_id: Some(1),
            role: role.into(),
            name: None,
            row_name: None,
            root_title: None,
            value: None,
            url: None,
            states: BTreeMap::new(),
            frame: frame(),
            visibility: BrowserVisibility::InViewport,
            actions: vec![BrowserActionKind::Click],
            document_order: 0,
        }
    }

    #[test]
    fn outline_stops_at_the_character_budget_and_continues_from_there() {
        let document = share_form();
        let whole = document.page(0, 300, usize::MAX, None, None);
        let budget = serialized_chars(&whole.view) - 1;
        let first = document.page(0, 300, budget, None, None);
        assert!(serialized_chars(&first.view) <= budget);
        assert!(first.selected_nodes < whole.selected_nodes);
        assert_eq!(
            first.omissions.budget,
            whole.selected_nodes - first.selected_nodes
        );
        let rest = document.page(first.next_offset.unwrap(), 300, usize::MAX, None, None);
        assert_eq!(
            first.selected_nodes + rest.selected_nodes,
            whole.selected_nodes
        );
        // A budget too small for anything still returns one node, not nothing.
        let tiny = document.page(0, 300, 1, None, None);
        assert_eq!(tiny.selected_nodes, 1);
        assert_eq!(tiny.next_offset, Some(1));
    }

    #[test]
    fn a_page_read_to_be_compared_stops_where_the_earlier_one_was_cut() {
        let document = share_form();
        let whole = document.page(0, 300, usize::MAX, None, None);
        assert_eq!(whole.tail, None, "nothing was left out");
        // A budget that leaves the last two nodes out.
        let cut = document.page(0, whole.selected_nodes - 2, usize::MAX, None, None);
        let tail = cut.tail.clone().expect("the view was cut");

        // Read again with room for everything: it still stops at that node.
        let again = document.page_sized(0, 300, usize::MAX, None, None, false, Some(&tail));
        assert_eq!(again.selected_nodes, cut.selected_nodes);
        assert_eq!(again.outline_with("R"), cut.outline_with("R"));
        assert_eq!(again.tail, Some(tail.clone()));

        // A line above the cut grew: the cut does not move for it, even
        // though the old character budget alone would now fit one node less.
        let budget = serialized_chars(&cut.view);
        let mut grown = share_form();
        let longer = Some("a much longer address than before@example.com".to_owned());
        grown
            .nodes
            .iter_mut()
            .find(|node| node.role == "textbox")
            .unwrap()
            .value = longer.clone();
        grown
            .nodes
            .iter_mut()
            .find(|node| node.role == "statictext")
            .unwrap()
            .name = longer;
        let refit = grown.page(0, 300, budget, None, None);
        assert!(refit.selected_nodes < cut.selected_nodes);
        let pinned = grown.page_sized(0, 300, budget, None, None, false, Some(&tail));
        assert_eq!(pinned.selected_nodes, cut.selected_nodes);
    }

    #[test]
    fn budget_counts_the_outline_as_it_is_serialized() {
        let page = share_form().page(0, 300, usize::MAX, None, None);
        let outline = page.outline_with(BUDGETED_REF);
        assert_eq!(
            serialized_chars(&page.view),
            serde_json::to_string(&outline).unwrap().chars().count() - 2
        );
    }

    #[test]
    fn merged_frame_documents_keep_their_own_trees() {
        let dom = DomIndex::default();
        let tree = |backend: i64, name: &str| {
            compose_accessibility_tree(
                &json!({"nodes": [
                    ax_node("1", None, Some(backend), "RootWebArea", None),
                    ax_node("2", Some("1"), Some(backend + 1), "group", Some(name)),
                    ax_node("3", Some("2"), Some(backend + 2), "button", Some(&format!("{name} button"))),
                ]}),
                &dom,
                &LayoutIndex::default(),
                &Viewport::default(),
                frame(),
            )
        };
        // Two processes number their accessibility nodes alike.
        let mut document = tree(10, "Outer");
        document.extend(tree(20, "Inner"));
        let page = document.page(0, 300, usize::MAX, None, None);
        assert_eq!(
            page.outline_with("R"),
            [
                "- group \"Outer\" [R]",
                "  - button \"Outer button\" [R click]",
                "- group \"Inner\" [R]",
                "  - button \"Inner button\" [R click]",
            ]
            .join("\n")
        );
    }

    #[test]
    fn the_extensions_own_indicator_is_not_page_content() {
        let dom = build_dom_index(&json!({"nodeType": 9, "children": [
            {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 1},
            {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 2,
             "attributes": ["id", "cua-driver-indicator"],
             "shadowRoots": [{"nodeType": 11, "shadowRootType": "closed", "backendNodeId": 3,
                "children": [
                    {"nodeType": 1, "nodeName": "SPAN", "backendNodeId": 4},
                    {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 5,
                     "attributes": ["onclick", "stop()"]}]}]}
        ]}));
        let document = compose_accessibility_tree(
            &json!({"nodes": [
                ax_node("root", None, Some(90), "RootWebArea", None),
                ax_node("send", Some("root"), Some(1), "button", Some("Send")),
                ax_node("label", Some("root"), Some(4), "StaticText", Some("Cua is working in this tab")),
            ]}),
            &dom,
            &LayoutIndex::default(),
            &Viewport::default(),
            frame(),
        );
        let page = document.page(0, 300, usize::MAX, None, None);
        // Neither the accessibility node nor the clickable Stop the DOM
        // supplement would add; and nothing is reported as hidden page content.
        assert_eq!(page.outline_with("R"), "- button \"Send\" [R click]");
        assert_eq!(page.omissions.css_hidden, 0);
    }

    #[test]
    fn file_inputs_expose_upload_instead_of_text_typing() {
        let dom = DomMeta {
            base_url: None,
            tag: "input".into(),
            attrs: HashMap::from([("type".into(), "file".into())]),
            order: 0,
            css_hidden: false,
            own_indicator: false,
            parent_backend_node_id: None,
            frame_id: None,
        };
        assert_eq!(
            action_kinds("textbox", Some(&dom), &BTreeMap::new(), None),
            vec![BrowserActionKind::Upload]
        );
    }
    use crate::browser::store::FrameRef;

    fn frame() -> FrameRef {
        FrameRef::main_unproven()
    }

    #[test]
    fn hidden_dom_nodes_do_not_enter_the_visible_working_set() {
        let dom = build_dom_index(&json!({
            "nodeType": 9,
            "children": [
                {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 1,
                 "attributes": ["aria-hidden", "true"]},
                {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 2,
                 "attributes": ["aria-label", "Reply"]}
            ]
        }));
        let ax = json!({"nodes": [
            {"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
             "childIds": ["retained", "reply"]},
            {"nodeId": "retained", "parentId": "root", "ignored": false,
             "backendDOMNodeId": 1, "role": {"value": "button"},
             "name": {"value": "Retained"}},
            {"nodeId": "reply", "parentId": "root", "ignored": false,
             "backendDOMNodeId": 2, "role": {"value": "button"},
             "name": {"value": "Reply"}}
        ]});
        let document = compose_accessibility_tree(
            &ax,
            &dom,
            &LayoutIndex::default(),
            &Viewport::default(),
            frame(),
        );
        let page = document.page(0, 300, usize::MAX, None, None);
        assert_eq!(page.omissions.css_hidden, 1);
        let actionable = page
            .selected
            .iter()
            .filter(|node| !node.actions.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].name.as_deref(), Some("Reply"));
    }

    #[test]
    fn dom_supplement_adds_only_explicit_visible_custom_actions() {
        let dom = build_dom_index(&json!({
            "nodeType": 9,
            "frameId": "F_MAIN",
            "children": [
                {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 1,
                 "attributes": ["aria-label", "Custom action", "onclick", "run()"]},
                {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 2,
                 "attributes": ["aria-label", "Static panel"]}
            ]
        }));
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto"],
            "documents": [{
                "nodes": {"backendNodeId": [1, 2]},
                "layout": {
                    "nodeIndex": [0, 1],
                    "bounds": [[10, 10, 100, 30], [10, 50, 100, 30]],
                    "styles": [[0, 1, 2, 3, 3, 3, 3], [0, 1, 2, 3, 3, 3, 3]],
                    "paintOrders": [1, 2]
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let document = compose_accessibility_tree(
            &json!({"nodes": [{"nodeId": "root", "ignored": false,
                "role": {"value": "RootWebArea"}, "childIds": []}]}),
            &dom,
            &layout,
            &viewport,
            frame(),
        );
        let page = document.page(0, 300, usize::MAX, None, None);
        assert!(page.selected.iter().any(|node| {
            node.name.as_deref() == Some("Custom action")
                && node.actions == vec![BrowserActionKind::Click, BrowserActionKind::Pointer]
        }));
        assert!(page
            .selected
            .iter()
            .all(|node| node.name.as_deref() != Some("Static panel")));
    }

    #[test]
    fn dom_supplement_exposes_scrollable_containers_without_click_authority() {
        let dom = build_dom_index(&json!({
            "nodeType": 9,
            "frameId": "F_MAIN",
            "children": [
                {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 1,
                 "attributes": ["aria-label", "Scrollable archive"]},
                {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 2,
                 "attributes": ["aria-label", "Overflow-visible panel"]}
            ]
        }));
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto", "default", "static", "0", "scroll"],
            "documents": [{
                "nodes": {"backendNodeId": [1, 2]},
                "layout": {
                    "nodeIndex": [0, 1],
                    "bounds": [[10, 10, 100, 50], [10, 80, 100, 50]],
                    "clientRects": [[10, 10, 100, 50], [10, 80, 100, 50]],
                    "scrollRects": [[0, 0, 100, 250], [0, 0, 100, 250]],
                    "styles": [
                        [0, 1, 2, 3, 4, 5, 6, 3, 7],
                        [0, 1, 2, 3, 4, 5, 6, 1, 1]
                    ],
                    "paintOrders": [1, 2]
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let document = compose_accessibility_tree(
            &json!({"nodes": [{"nodeId": "root", "ignored": false,
                "role": {"value": "RootWebArea"}, "childIds": []}]}),
            &dom,
            &layout,
            &viewport,
            frame(),
        );
        let page = document.page(0, 300, usize::MAX, None, None);
        let scrollable = page
            .selected
            .iter()
            .find(|node| node.name.as_deref() == Some("Scrollable archive"))
            .expect("scrollable DOM supplement");
        assert_eq!(scrollable.actions, vec![BrowserActionKind::Scroll]);
        assert!(page
            .selected
            .iter()
            .all(|node| node.name.as_deref() != Some("Overflow-visible panel")));
    }

    #[test]
    fn layout_ranks_in_viewport_actions_before_offscreen_content() {
        let dom = build_dom_index(&json!({
            "nodeType": 9,
            "children": [
                {"nodeType": 1, "nodeName": "P", "backendNodeId": 1},
                {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 2}
            ]
        }));
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto", "pointer"],
            "documents": [{
                "nodes": {"backendNodeId": [1, 2]},
                "layout": {
                    "nodeIndex": [0, 1],
                    "bounds": [[0, 5000, 100, 20], [10, 10, 100, 30]],
                    "styles": [[0, 1, 2, 3, 4], [0, 1, 2, 3, 4]],
                    "paintOrders": [1, 2]
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let ax = json!({"nodes": [
            {"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
             "childIds": ["text", "reply"]},
            {"nodeId": "text", "parentId": "root", "ignored": false,
             "backendDOMNodeId": 1, "role": {"value": "StaticText"},
             "name": {"value": "Old content"}},
            {"nodeId": "reply", "parentId": "root", "ignored": false,
             "backendDOMNodeId": 2, "role": {"value": "button"},
             "name": {"value": "Reply"}}
        ]});
        let document = compose_accessibility_tree(&ax, &dom, &layout, &viewport, frame());
        let page = document.page(0, 1, usize::MAX, None, None);
        assert_eq!(page.selected[0].name.as_deref(), Some("Reply"));
        assert_eq!(page.next_offset, Some(1));
    }

    #[test]
    fn fixed_painted_overlay_marks_covered_action_as_page_occluded() {
        let dom = build_dom_index(&json!({
            "nodeType": 9,
            "frameId": "F_MAIN",
            "children": [
                {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 1},
                {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 2}
            ]
        }));
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto", "fixed", "100"],
            "documents": [{
                "nodes": {"backendNodeId": [1, 2]},
                "layout": {
                    "nodeIndex": [0, 1],
                    "bounds": [[10, 10, 100, 30], [0, 0, 800, 600]],
                    "styles": [[0, 1, 2, 3, 3, 3, 3], [0, 1, 2, 3, 3, 4, 5]],
                    "paintOrders": [1, 2]
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let ax = json!({"nodes": [
            {"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
             "childIds": ["button"]},
            {"nodeId": "button", "parentId": "root", "ignored": false,
             "backendDOMNodeId": 1, "role": {"value": "button"},
             "name": {"value": "Covered"}}
        ]});
        let document = compose_accessibility_tree(&ax, &dom, &layout, &viewport, frame());
        let page = document.page(0, 300, usize::MAX, None, None);
        assert!(page
            .selected
            .iter()
            .all(|node| node.name.as_deref() != Some("Covered")));
        assert_eq!(page.omissions.page_occluded, 1);
    }

    #[test]
    fn dialog_container_does_not_occlude_its_own_actions() {
        let dom = build_dom_index(&json!({
            "nodeType": 9,
            "frameId": "F_MAIN",
            "children": [{
                "nodeType": 1,
                "nodeName": "DIV",
                "backendNodeId": 10,
                "attributes": ["role", "alertdialog", "aria-label", "Remove app"],
                "children": [
                    {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 11,
                     "attributes": ["aria-label", "Cancel"]},
                    {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 12,
                     "attributes": ["aria-label", "Remove"]}
                ]
            }]
        }));
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto", "default", "static", "0", "fixed", "100"],
            "documents": [{
                "nodes": {"backendNodeId": [10, 11, 12]},
                "layout": {
                    "nodeIndex": [0, 1, 2],
                    "bounds": [[0, 0, 800, 600], [250, 400, 120, 36], [390, 400, 120, 36]],
                    "styles": [
                        [0, 1, 2, 3, 4, 7, 8, 3, 3],
                        [0, 1, 2, 3, 4, 5, 6, 3, 3],
                        [0, 1, 2, 3, 4, 5, 6, 3, 3]
                    ],
                    "paintOrders": [100, 10, 11]
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let ax = json!({"nodes": [
            {"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
             "childIds": ["dialog"]},
            {"nodeId": "dialog", "parentId": "root", "ignored": false,
             "backendDOMNodeId": 10, "role": {"value": "alertdialog"},
             "name": {"value": "Remove app"}, "childIds": ["cancel", "remove"]},
            {"nodeId": "cancel", "parentId": "dialog", "ignored": false,
             "backendDOMNodeId": 11, "role": {"value": "button"},
             "name": {"value": "Cancel"}, "childIds": []},
            {"nodeId": "remove", "parentId": "dialog", "ignored": false,
             "backendDOMNodeId": 12, "role": {"value": "button"},
             "name": {"value": "Remove"}, "childIds": []}
        ]});
        let document = compose_accessibility_tree(&ax, &dom, &layout, &viewport, frame());
        let page = document.page(0, 300, usize::MAX, None, None);
        let actions = page
            .selected
            .iter()
            .filter(|node| !node.actions.is_empty())
            .filter_map(|node| node.name.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(actions, vec!["Cancel", "Remove"]);
        assert_eq!(page.omissions.page_occluded, 0);
    }

    /// TodoMVC's list: `li > div.view (ignored) > [input.toggle (opacity 0,
    /// no label), label > "text", button "Delete todo" > "×"]`.
    fn todo_list() -> SemanticDocument {
        let item = |li: i64, title: &str| {
            json!({"nodeType": 1, "nodeName": "LI", "backendNodeId": li, "children": [
                {"nodeType": 1, "nodeName": "DIV", "backendNodeId": li + 1, "children": [
                    {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": li + 2,
                     "attributes": ["class", "toggle", "type", "checkbox"]},
                    {"nodeType": 1, "nodeName": "LABEL", "backendNodeId": li + 3, "children": [
                        {"nodeType": 3, "nodeName": "#text", "backendNodeId": li + 4, "nodeValue": title}]},
                    {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": li + 5, "children": [
                        {"nodeType": 3, "nodeName": "#text", "backendNodeId": li + 6}]}]}]})
        };
        let dom = build_dom_index(&json!({"nodeType": 9, "children": [
            {"nodeType": 1, "nodeName": "UL", "backendNodeId": 1,
             "children": [item(10, "Buy milk"), item(20, "Call the plumber")]}]}));
        // Every node shown; the toggles are transparent but take clicks.
        let shown: Vec<i64> = vec![1, 10, 11, 13, 14, 15, 16, 20, 21, 23, 24, 25, 26];
        let mut backends = shown.clone();
        backends.extend([12, 22]);
        let styles: Vec<Value> = shown
            .iter()
            .map(|_| json!([0, 1, 2, 3, 4]))
            .chain([json!([0, 1, 5, 3, 4]), json!([0, 1, 5, 3, 4])])
            .collect();
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto", "default", "0"],
            "documents": [{
                "nodes": {"backendNodeId": backends},
                "layout": {
                    "nodeIndex": (0..backends.len()).collect::<Vec<_>>(),
                    "bounds": backends.iter().map(|_| json!([0, 10, 300, 40])).collect::<Vec<_>>(),
                    "styles": styles,
                    "paintOrders": (0..backends.len()).collect::<Vec<_>>()
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let row = |li: i64, title: &str| {
            let id = |suffix: &str| format!("{li}{suffix}");
            vec![
                json!({"nodeId": id("li"), "parentId": "list", "ignored": false, "backendDOMNodeId": li,
                       "role": {"value": "listitem"}, "childIds": [id("view")]}),
                json!({"nodeId": id("view"), "parentId": id("li"), "ignored": true,
                       "backendDOMNodeId": li + 1, "role": {"value": "generic"},
                       "childIds": [id("toggle"), id("label"), id("destroy")]}),
                json!({"nodeId": id("toggle"), "parentId": id("view"), "ignored": false,
                       "backendDOMNodeId": li + 2, "role": {"value": "checkbox"},
                       "properties": [{"name": "checked", "value": {"value": "false"}}]}),
                json!({"nodeId": id("label"), "parentId": id("view"), "ignored": false,
                       "backendDOMNodeId": li + 3, "role": {"value": "LabelText"},
                       "childIds": [id("text")]}),
                json!({"nodeId": id("text"), "parentId": id("label"), "ignored": false,
                       "backendDOMNodeId": li + 4, "role": {"value": "StaticText"},
                       "name": {"value": title}}),
                json!({"nodeId": id("destroy"), "parentId": id("view"), "ignored": false,
                       "backendDOMNodeId": li + 5, "role": {"value": "button"},
                       "name": {"value": "Delete todo"}, "childIds": [id("x")]}),
                json!({"nodeId": id("x"), "parentId": id("destroy"), "ignored": false,
                       "backendDOMNodeId": li + 6, "role": {"value": "StaticText"},
                       "name": {"value": "×"}}),
            ]
        };
        let mut nodes = vec![
            json!({"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
                   "childIds": ["list"]}),
            json!({"nodeId": "list", "parentId": "root", "ignored": false, "backendDOMNodeId": 1,
                   "role": {"value": "list"}, "childIds": ["10li", "20li"]}),
        ];
        nodes.extend(row(10, "Buy milk"));
        nodes.extend(row(20, "Call the plumber"));
        compose_accessibility_tree(&json!({ "nodes": nodes }), &dom, &layout, &viewport, frame())
    }

    #[test]
    fn unlabeled_row_checkboxes_are_listed_under_their_row_named_by_its_text() {
        let document = todo_list();
        let page = document.page(0, 300, usize::MAX, None, None);
        let outline = page.outline_with("r");
        let expected = [
            "- list [r]",
            "  - listitem [r]",
            "    - checkbox \"Buy milk\" [r click] (unchecked)",
            "    - labeltext [r]",
            "      - statictext \"Buy milk\" [r]",
            "    - button \"Delete todo\" [r click]",
        ];
        assert!(
            outline.starts_with(&expected.join("\n")),
            "the toggle, the label and the button sit under their listitem:\n{outline}"
        );
        let toggle = document
            .nodes
            .iter()
            .find(|node| node.backend_node_id == Some(22))
            .unwrap();
        let entry = toggle.to_ref_entry().unwrap();
        // The row name is shown, not fingerprinted: the live re-proof reads
        // the accessible name, which is still empty.
        assert_eq!(entry.label, None);
        assert_eq!(
            entry.row,
            Some(RowName {
                name: "Call the plumber".into(),
                levels: 1
            })
        );
        assert!(!entry.actions.contains(&BrowserActionKind::Type));
        // A query finds it by the row's text; a scoped read of the listitem
        // reaches the children of the ignored wrapper.
        let found = document.page(0, 300, usize::MAX, Some("plumber"), None);
        assert!(found.selected.iter().any(|node| node.backend_node_id == Some(22)));
        let scoped = document.page(0, 300, usize::MAX, None, Some(20));
        assert!(scoped.selected.iter().any(|node| node.backend_node_id == Some(24)));
        let line = parse_outline_line("    - checkbox \"Buy milk\" [p1:2 click] (unchecked)").unwrap();
        assert_eq!(line.name.as_deref(), Some("Buy milk"));
    }

    #[test]
    fn a_row_with_two_checkboxes_names_neither_and_button_text_stays_out() {
        let dom = build_dom_index(&json!({"nodeType": 9, "children": [
            {"nodeType": 1, "nodeName": "LI", "backendNodeId": 1, "children": [
                {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 2, "attributes": ["type", "checkbox"]},
                {"nodeType": 3, "nodeName": "#text", "backendNodeId": 3},
                {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 4, "children": [
                    {"nodeType": 3, "nodeName": "#text", "backendNodeId": 5}]}]},
            {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 6, "children": [
                {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 7, "attributes": ["type", "checkbox"]},
                {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 8, "attributes": ["type", "checkbox"]},
                {"nodeType": 3, "nodeName": "#text", "backendNodeId": 9}]}]}));
        let ax = json!({"nodes": [
            {"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"},
             "childIds": ["li", "div"]},
            {"nodeId": "li", "parentId": "root", "ignored": false, "backendDOMNodeId": 1,
             "role": {"value": "listitem"}, "childIds": ["a", "a-text", "edit"]},
            {"nodeId": "a", "parentId": "li", "ignored": false, "backendDOMNodeId": 2,
             "role": {"value": "checkbox"}},
            {"nodeId": "a-text", "parentId": "li", "ignored": false, "backendDOMNodeId": 3,
             "role": {"value": "StaticText"}, "name": {"value": "Water plants"}},
            {"nodeId": "edit", "parentId": "li", "ignored": false, "backendDOMNodeId": 4,
             "role": {"value": "button"}, "name": {"value": "Edit"}, "childIds": ["edit-text"]},
            {"nodeId": "edit-text", "parentId": "edit", "ignored": false, "backendDOMNodeId": 5,
             "role": {"value": "StaticText"}, "name": {"value": "Edit"}},
            {"nodeId": "div", "parentId": "root", "ignored": false, "backendDOMNodeId": 6,
             "role": {"value": "generic"}, "childIds": ["b", "c", "shared"]},
            {"nodeId": "b", "parentId": "div", "ignored": false, "backendDOMNodeId": 7,
             "role": {"value": "checkbox"}},
            {"nodeId": "c", "parentId": "div", "ignored": false, "backendDOMNodeId": 8,
             "role": {"value": "checkbox"}},
            {"nodeId": "shared", "parentId": "div", "ignored": false, "backendDOMNodeId": 9,
             "role": {"value": "StaticText"}, "name": {"value": "Both options"}}
        ]});
        let document = compose_accessibility_tree(
            &ax,
            &dom,
            &LayoutIndex::default(),
            &Viewport::default(),
            frame(),
        );
        let row_name = |backend: i64| {
            document
                .nodes
                .iter()
                .find(|node| node.backend_node_id == Some(backend))
                .and_then(|node| node.row_name.clone())
                .map(|row| row.name)
        };
        assert_eq!(row_name(2).as_deref(), Some("Water plants"));
        assert_eq!(row_name(7), None);
        assert_eq!(row_name(8), None);
    }

    #[test]
    fn transparency_hides_a_wrapper_but_not_a_native_control() {
        let dom = build_dom_index(&json!({"nodeType": 9, "children": [
            {"nodeType": 1, "nodeName": "DIV", "backendNodeId": 1, "attributes": ["onclick", "go()"]},
            {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 2, "attributes": ["type", "checkbox"]},
            {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 3,
             "attributes": ["type", "file", "style", "opacity: 0"]},
            {"nodeType": 1, "nodeName": "BUTTON", "backendNodeId": 4,
             "attributes": ["style", "opacity:0.5"]},
            {"nodeType": 1, "nodeName": "INPUT", "backendNodeId": 5,
             "attributes": ["type", "checkbox", "style", "pointer-events:none"]}]}));
        // Computed opacity 0 on the div, the checkbox and the last checkbox,
        // which also takes no clicks.
        let layout = build_layout_index(&json!({
            "strings": ["block", "visible", "1", "auto", "default", "0", "none"],
            "documents": [{
                "nodes": {"backendNodeId": [1, 2, 3, 4, 5]},
                "layout": {
                    "nodeIndex": [0, 1, 2, 3, 4],
                    "bounds": [[0, 0, 50, 20], [0, 30, 20, 20], [0, 60, 80, 20],
                               [0, 90, 80, 20], [0, 120, 20, 20]],
                    "styles": [[0, 1, 5, 3, 4], [0, 1, 5, 3, 4], [0, 1, 2, 3, 4],
                               [0, 1, 2, 3, 4], [0, 1, 5, 6, 4]],
                    "paintOrders": [1, 2, 3, 4, 5]
                }
            }]
        }));
        let viewport = parse_viewport(&json!({
            "cssVisualViewport": {"pageX": 0.0, "pageY": 0.0,
                                  "clientWidth": 800.0, "clientHeight": 600.0}
        }));
        let ax = json!({"nodes": [
            {"nodeId": "root", "ignored": false, "role": {"value": "RootWebArea"}},
            {"nodeId": "2", "parentId": "root", "ignored": false, "backendDOMNodeId": 2,
             "role": {"value": "checkbox"}, "name": {"value": "Done"}},
            {"nodeId": "3", "parentId": "root", "ignored": false, "backendDOMNodeId": 3,
             "role": {"value": "button"}, "name": {"value": "Choose file"}},
            {"nodeId": "4", "parentId": "root", "ignored": false, "backendDOMNodeId": 4,
             "role": {"value": "button"}, "name": {"value": "Half"}},
            {"nodeId": "5", "parentId": "root", "ignored": false, "backendDOMNodeId": 5,
             "role": {"value": "checkbox"}, "name": {"value": "Inert"}}
        ]});
        let document = compose_accessibility_tree(&ax, &dom, &layout, &viewport, frame());
        let visibility = |backend: i64| {
            document
                .nodes
                .iter()
                .find(|node| node.backend_node_id == Some(backend))
                .map(|node| node.visibility)
        };
        // The transparent div with a click handler is not added at all.
        assert_eq!(visibility(1), None);
        assert_eq!(visibility(2), Some(BrowserVisibility::InViewport));
        assert_eq!(visibility(3), Some(BrowserVisibility::InViewport));
        assert_eq!(visibility(4), Some(BrowserVisibility::InViewport));
        assert_eq!(visibility(5), Some(BrowserVisibility::CssHidden));
    }

    #[test]
    fn semantic_text_removes_icon_glyphs_and_nonbreaking_spaces() {
        assert_eq!(
            clean_semantic_text("\u{e001} Reply\u{00a0}now \u{f8ff}".to_owned()).as_deref(),
            Some("Reply now")
        );
    }
}
