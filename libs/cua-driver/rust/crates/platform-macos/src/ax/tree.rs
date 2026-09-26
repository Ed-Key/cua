//! AX tree walker: produces the treeMarkdown string and element cache.
//!
//! Format (matching libs/cua-driver exactly):
//!   `INDENT- [N] AXRole "Title" [value="..." actions=[...]]`
//!   `INDENT- AXStaticText = "value"`  (non-indexed)
//!
//! Rules (from cua-driver reference):
//! - An element is addressable (gets an index) when it has ≥1 action name or
//!   exposes a writable AXValue control surface.
//! - Non-actionable leaf nodes with a value are rendered as `AXRole = "value"`.
//! - AXStaticText with no title/value is omitted.
//! - Tree is walked depth-first; element_index is assigned in DFS order.

use super::bindings::*;
use super::window_scope::{decide_window_scope, TopLevelCandidate, WindowScope};
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};
use cua_driver_core::walk_budget::{WalkBudget, WalkOutcome};

/// Default maximum depth for AX tree walks. Deep menus and complex web views
/// can nest deeply; 25 covers realistic app chrome without exploding on
/// pathological trees (mirrors Swift reference implementation).
///
/// Callers can override per-call via `walk_tree`'s `max_depth` parameter to
/// trade fidelity for context-window budget on AX-heavy apps (Electron,
/// Obsidian, large web apps — issue #22865).
pub const DEFAULT_MAX_DEPTH: usize = 25;

/// Default maximum total nodes visited during a single AX walk. Chromium-family
/// apps (Arc, VS Code, Chrome) can expose thousands of nodes; capping at 2 000
/// keeps the walk bounded while still covering realistic app chrome.
/// When the cap is hit the walk stops early and the partial tree is returned
/// with a warning line appended (mirrors Swift reference implementation).
///
/// Callers can override per-call via `walk_tree`'s `max_elements` parameter
/// (issue #22865).
pub const DEFAULT_MAX_ELEMENTS: usize = 2_000;

/// Bound each native AX request. Tokio cannot cancel a blocked
/// `AXUIElementCopyAttributeValue` after `spawn_blocking` starts, so the native
/// messaging timeout is what keeps an unresponsive app from retaining a worker
/// indefinitely.
const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;

unsafe fn set_messaging_timeout(element: AXUIElementRef) {
    let _ = AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS);
}

/// A single node in the AX tree.
#[derive(Debug, Clone)]
pub struct AXNode {
    /// Link destination, separate from AXValue.
    pub url: Option<String>,
    /// 0-based index (Some = actionable, None = non-actionable display-only node)
    pub element_index: Option<usize>,
    pub role: String,
    /// AXTitle — shown as `"title"` in the tree line.
    pub title: Option<String>,
    /// AXValue — shown as `= "value"` in the tree line.
    pub value: Option<String>,
    /// `AXPlaceholderValue`: the hint shown in an empty field. Never the
    /// field's content.
    pub placeholder: Option<String>,
    /// Whether `AXValue` is settable, for value-control roles; `None` when
    /// not read or the read failed (unknown, never assumed writable).
    pub value_settable: Option<bool>,
    /// Reported keyboard focus: set for the focused element and for other
    /// text controls (`false`); `None` is unknown.
    pub focused: Option<bool>,
    /// The focused text control's selection (UTF-16 range, selected text).
    pub text_selection: Option<cua_driver_contract::TextSelection>,
    /// AXDescription — shown as `(description)` in the tree line.
    /// Kept separate from `title` so `_find_calc_button("2")` can find
    /// Calculator buttons where AXTitle="" but AXDescription="2".
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
    pub actions: Vec<String>,
    /// The raw AXUIElementRef pointer value, for caching.
    pub element_ptr: usize,
    /// Depth in the rendered markdown tree (matches the indent level used in
    /// `tree_markdown`). Unnamed AXScrollArea/AXGroup layout wrappers collapse
    /// so children share the parent's depth; named groups retain their depth.
    pub depth: usize,
    /// `element_index` of the nearest actionable ancestor, if any. Walks the
    /// rendered tree (so it skips collapsed layout containers).
    pub parent_element_index: Option<usize>,
    /// Position in the walk's node list of the nearest kept ancestor, display
    /// rows included. Exact where indentation is not: the children of a
    /// dropped empty container render one level deeper than their real parent.
    pub parent_position: Option<usize>,
    /// Screen-coordinate bounding rect `[x, y, width, height]` captured at
    /// walk time. `None` when AX didn't report a usable position+size.
    pub frame: Option<[f64; 4]>,
    /// AXValue coerced to a string for ALL CF types (CFNumber → "8",
    /// CFBoolean → "1"/"0", CFString as-is). Kept separate from `value`
    /// (string-only) so tree_markdown and the has_content gate — both of
    /// which read `value` — stay byte-identical; only the structured
    /// `elements` array consumes this.
    pub value_state: Option<String>,
    /// AXValueDescription — human-readable value form (e.g. "8 dB").
    pub value_description: Option<String>,
    /// AXMinValue / AXMaxValue for range controls (sliders, steppers).
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    /// AXEnabled. `None` when the app doesn't report the attribute.
    pub enabled: Option<bool>,
    /// AXSelected. `None` when the app doesn't report the attribute.
    pub selected: Option<bool>,
    /// True when this node is an AX web-document root or descends from one.
    /// This trust marker is independent of actionable ancestry because
    /// AXWebArea is commonly non-actionable and therefore has no element index.
    pub in_web_content: bool,
}

#[derive(Default)]
struct ControlState {
    value_state: Option<String>,
    value_description: Option<String>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    enabled: Option<bool>,
    selected: Option<bool>,
}

fn read_control_state_if_actionable<F>(is_actionable: bool, read: F) -> ControlState
where
    F: FnOnce() -> ControlState,
{
    if is_actionable {
        read()
    } else {
        ControlState::default()
    }
}

pub(crate) fn is_text_entry_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox")
}

fn role_supports_value_addressing(role: &str) -> bool {
    matches!(
        role,
        "AXTextField"
            | "AXTextArea"
            | "AXComboBox"
            | "AXSlider"
            | "AXStepper"
            | "AXCheckBox"
            | "AXRadioButton"
    )
}

fn is_addressable(actions_present: bool, value_settable: bool, enabled: Option<bool>) -> bool {
    (actions_present || value_settable) && enabled != Some(false)
}

pub struct TreeWalkResult {
    pub tree_markdown: String,
    pub nodes: Vec<AXNode>,
    /// True when the walk was cut short by its node or time budget.
    pub truncated: bool,
    /// Why and where the walk stopped (see [`cua_driver_core::walk_budget`]).
    pub walk: WalkOutcome,
    /// Whether the requested `window_id` actually resolved to an AX surface,
    /// and if not, why. `None` when no `window_id` was requested.
    ///
    /// Issue #2237: without this, an unresolvable id was indistinguishable
    /// from a clean snapshot — callers had no way to tell that the tree they
    /// were handed belonged to a different surface. Any variant other than
    /// [`WindowScope::Matched`] comes with an EMPTY walk, so `nodes` never
    /// describes a window other than the requested one.
    pub window_scope: Option<WindowScope>,
    /// What the walk saw that its output alone cannot show.
    pub sightings: WalkSightings,
}

/// Facts from a walk that pruning or unrecorded limits would otherwise hide.
#[derive(Clone, Copy, Debug, Default)]
pub struct WalkSightings {
    /// A web-content element was reached, even if it was not emitted as a node.
    pub web_content: bool,
    /// Some subtree was cut by the depth limit.
    pub depth_cut: bool,
    /// Reading some element's children failed.
    pub child_read_failed: bool,
}

impl WalkSightings {
    /// Whether an absence in this walk means anything: no budget stop, no
    /// depth cut, and no failed child read.
    pub fn complete(&self, walk: &WalkOutcome) -> bool {
        !walk.truncated() && !self.depth_cut && !self.child_read_failed
    }
}

/// Walk the AX tree of `pid`, optionally filtered to a specific window.
///
/// `window_id` — when Some, only the AXWindow matching that CGWindowID is
/// walked (plus non-window children like the menu bar). When None, all
/// top-level children are walked.
///
/// Key background-app fix: at the application root we union `AXChildren`
/// and `AXWindows`. macOS only puts windows in `AXChildren` when the app
/// is frontmost; `AXWindows` returns the window list regardless of focus
/// state. Without this union, Safari / any backgrounded app returns an
/// empty tree.
///
/// # Safety
/// Calls macOS AX API. Must be called on a thread that has a CF run loop.
pub fn walk_tree(pid: i32, window_id: Option<u32>, query: Option<Query>) -> TreeWalkResult {
    walk_tree_bounded(
        pid,
        window_id,
        query,
        DEFAULT_MAX_ELEMENTS,
        DEFAULT_MAX_DEPTH,
    )
}

/// Walk the AX tree with caller-supplied caps. See [`walk_tree`] for the
/// common case (defaults apply). `max_elements`/`max_depth` clamp the
/// rendered tree breadth-wise (DFS truncated when the element counter hits
/// the cap) and depth-wise (nodes whose markdown indent would exceed the cap
/// are omitted). Markdown and the `nodes` vec are truncated identically.
///
/// Issue #22865: caps protect against Electron / Obsidian / large web apps
/// that produce 10k+ element trees and blow context windows.
pub fn walk_tree_bounded(
    pid: i32,
    window_id: Option<u32>,
    query: Option<Query>,
    max_elements: usize,
    max_depth: usize,
) -> TreeWalkResult {
    walk_tree_budgeted(
        pid,
        window_id,
        query,
        max_depth,
        WalkBudget::nodes_only(max_elements),
    )
}

/// [`walk_tree_bounded`] under a caller's [`WalkBudget`]: the walk also stops
/// when the budget's `timeout_ms` runs out, returning the partial tree. Each
/// AX call is bounded by the per-element messaging timeout, so the walk
/// overruns the deadline by at most one attribute read.
pub fn walk_tree_budgeted(
    pid: i32,
    window_id: Option<u32>,
    query: Option<Query>,
    max_depth: usize,
    mut budget: WalkBudget,
) -> TreeWalkResult {
    let mut nodes: Vec<AXNode> = Vec::new();
    let mut index_counter = 0usize;
    let mut window_scope: Option<WindowScope> = None;
    let mut sightings = WalkSightings::default();

    unsafe {
        let app_elem = AXUIElementCreateApplication(pid);
        if app_elem.is_null() {
            return TreeWalkResult {
                tree_markdown: String::new(),
                nodes,
                truncated: false,
                walk: budget.outcome(),
                // No application AX element at all, so a requested window
                // certainly did not resolve.
                window_scope: window_id.map(|_| WindowScope::AxUnresolved { ax_window_count: 0 }),
                sightings,
            };
        }
        set_messaging_timeout(app_elem);

        // Chromium/Electron apps (Arc, VS Code, Electron shells) ship their
        // web-content AX tree OFF and only build it once an assistive client
        // asks for it. Without this, the first walk of such an app returns an
        // empty/title-bar-only tree (#1616). Flip the enablement attribute,
        // then — only when the flip actually took and only the first time we
        // see this process lifetime — let the asynchronously-built tree settle
        // before we read it. Native Cocoa apps reject the attribute, so they
        // pay no settle cost. This relies on the MAX_ELEMENTS node cap to keep
        // the now-materialized (potentially large) tree bounded.
        super::enablement::ensure_chromium_ax_enabled(pid, app_elem);

        // Union AXChildren + AXWindows — the only way to see background windows.
        // AXChildren omits windows when the app isn't frontmost (AppKit limitation).
        // AXWindows returns the window list regardless of activation state.
        let from_children = copy_children(app_elem);
        // A requested window on another Space is absent from AXWindows; the
        // `_including` variant recovers it by exact CGWindowID.
        let from_windows = match window_id {
            Some(wid) => copy_ax_windows_including(app_elem, pid, wid),
            None => copy_ax_windows(app_elem),
        };

        let mut top_level = from_children;
        for w in from_windows {
            // AXChildren and AXWindows can return different proxy pointers for
            // the same native window. CFEqual compares their AX identity;
            // pointer equality alone duplicates the whole subtree and can turn
            // one exact dialog action into a false ambiguity.
            if !top_level
                .iter()
                .any(|&e| CFEqual(e as CFTypeRef, w as CFTypeRef) != 0)
            {
                top_level.push(w);
            } else {
                // Already present — release the extra retain from copy_ax_windows.
                CFRelease(w as CFTypeRef);
            }
        }

        // Scope: keep non-window children (menu bar) + the target window —
        // but ONLY once the target window has actually been identified. When
        // nothing claims the requested id, `decide_window_scope` reports why
        // and walks nothing; it must never fall back to "everything that isn't
        // a window", which is how issue #2237 returned menu bars as panels.
        let walk_these: Vec<AXUIElementRef> = if let Some(wid) = window_id {
            let candidates: Vec<TopLevelCandidate> = top_level
                .iter()
                .map(|&child| {
                    set_messaging_timeout(child);
                    let role = copy_string_attr(child, "AXRole").unwrap_or_default();
                    let subrole = copy_string_attr(child, "AXSubrole");
                    let identifier = copy_string_attr(child, "AXIdentifier");
                    // Match AX window element → CGWindowID via private SPI.
                    // Only windows carry one, so skip the round-trip elsewhere.
                    let ax_window_id = if role == "AXWindow" {
                        ax_get_window_id(child)
                    } else {
                        None
                    };
                    TopLevelCandidate {
                        role,
                        subrole,
                        identifier,
                        ax_window_id,
                    }
                })
                .collect();
            let decision = decide_window_scope(&candidates, wid, || {
                crate::windows::resolve_window_owner(pid, wid)
            });
            let walk = decision
                .walk
                .iter()
                .map(|&index| top_level[index])
                .collect();
            window_scope = Some(decision.scope);
            walk
        } else {
            top_level.to_vec()
        };

        // Walk each top-level child at depth 0.
        for child in walk_these {
            walk_element(
                child,
                0,
                None,
                None,
                false,
                &mut nodes,
                &mut index_counter,
                &mut budget,
                &mut sightings,
                max_depth,
            );
        }

        // Focus and selection are read once the whole tree is known, so the
        // focused element is matched by AX identity and a focus change during
        // the read discards the view.
        super::text_state::enrich_focused_state(pid, &mut nodes, &budget);

        // Release all top-level elements (copy_children / copy_ax_windows both retain).
        for child in top_level {
            CFRelease(child as CFTypeRef);
        }

        CFRelease(app_elem as CFTypeRef);
    }

    let walk = budget.outcome();
    let tree_markdown = render_outline(&nodes, query, &walk);

    TreeWalkResult {
        tree_markdown,
        nodes,
        truncated: walk.truncated(),
        walk,
        window_scope,
        sightings,
    }
}

/// A query on a window read: the text to match, and whether to keep each
/// match's collected descendants as well as its ancestors.
#[derive(Clone, Copy)]
pub struct Query<'a> {
    pub text: &'a str,
    pub context: bool,
}

/// Positions in `nodes` a query keeps: every row whose rendered line matches,
/// its real ancestors (from the walker's parent edges, never indentation) and,
/// with context, every row under a match.
pub(crate) fn query_positions(nodes: &[AXNode], query: Query) -> Vec<usize> {
    let needle = query.text.to_lowercase();
    cua_driver_core::element_query::select_parented_rows(
        nodes
            .iter()
            .map(|n| (n.parent_position, row_matches(&format_node_line(n), &needle))),
        query.context,
    )
}

/// Render `tree_markdown` from `nodes`: the whole outline, or only the rows a
/// query keeps, at their own depths. Callers that renumber rows after the
/// walk re-render with this.
pub(crate) fn render_outline(nodes: &[AXNode], query: Option<Query>, walk: &WalkOutcome) -> String {
    let positions: Vec<usize> = match query {
        Some(q) => query_positions(nodes, q),
        None => (0..nodes.len()).collect(),
    };
    let lines: Vec<(usize, String)> = positions
        .into_iter()
        .map(|p| (nodes[p].depth, format_node_line(&nodes[p])))
        .collect();
    let mut tree_markdown = render_lines(&lines);
    if let Some(note) = walk.note() {
        tree_markdown.push('\n');
        tree_markdown.push_str(&note);
    }
    tree_markdown
}

/// Keep named sections in the rendered hierarchy. Action names alone do not
/// identify a semantic group: Chromium exposes generic show-menu/scroll actions
/// even on anonymous layout wrappers. Addressability is evaluated separately.
fn collapse_layout_container(role: &str, title: Option<&str>, description: Option<&str>) -> bool {
    matches!(role, "AXGroup" | "AXScrollArea")
        && title.is_none_or(|text| text.trim().is_empty())
        && description.is_none_or(|text| text.trim().is_empty())
}

#[allow(clippy::too_many_arguments)]
unsafe fn walk_element(
    element: AXUIElementRef,
    depth: usize,
    parent_index: Option<usize>,
    parent_position: Option<usize>,
    in_web_content: bool,
    nodes: &mut Vec<AXNode>,
    counter: &mut usize,
    budget: &mut WalkBudget,
    sightings: &mut WalkSightings,
    max_depth: usize,
) {
    if depth > max_depth {
        sightings.depth_cut = true;
        return;
    }
    // Node and time budget: a refused node is counted as discovered but not
    // visited, and the walk unwinds with the partial tree it has.
    if !budget.admit() {
        return;
    }

    // Messaging timeouts are per AX object, not inherited from the application
    // element, so every descendant must be bounded before any attribute read.
    set_messaging_timeout(element);

    let role = copy_string_attr(element, "AXRole").unwrap_or_else(|| "AXUnknown".into());

    let in_web_content = in_web_content || is_web_content_role(&role);
    sightings.web_content |= in_web_content;

    // Read names before deciding whether a group is only a layout wrapper.
    // Reuse these reads below for retained nodes.
    let title = copy_string_attr(element, "AXTitle");
    let description = copy_string_attr(element, "AXDescription");
    if collapse_layout_container(&role, title.as_deref(), description.as_deref()) {
        // Still recurse — children may be interesting. Layout containers
        // collapse, so children inherit the parent's depth AND the same
        // parent_index (no actionable node was emitted here).
        let (children, failed) = copy_children_reporting(element);
        sightings.child_read_failed |= failed;
        for child in children {
            walk_element(
                child,
                depth,
                parent_index,
                parent_position,
                in_web_content,
                nodes,
                    counter,
                budget,
                sightings,
                max_depth,
            );
            CFRelease(child as CFTypeRef);
        }
        return;
    }

    // Keep AXTitle and AXDescription SEPARATE so that the tree format matches
    // the Swift reference: title → "title", description → (description).
    // This is critical for Calculator where AXTitle="" but AXDescription="2"
    // (digit buttons). Merging them would produce "2" (quoted) instead of (2)
    // (parens), breaking _find_calc_button which searches for "(2)".
    // Read AXValue once with enough type information to preserve the existing
    // string-only markdown while also exposing numeric/boolean control state.
    let copied_value = copy_stringish_attr(element, "AXValue");
    let value = copied_value
        .as_ref()
        .and_then(|copied| copied.string_value.clone());
    let placeholder =
        copy_string_attr(element, "AXPlaceholderValue").filter(|hint| !hint.trim().is_empty());
    let identifier = copy_string_attr(element, "AXIdentifier");
    let help = copy_string_attr(element, "AXHelp").filter(|h| !h.trim().is_empty());
    let actions: Vec<String> = copy_action_names(element)
        .into_iter()
        .map(display_action_name)
        .collect();

    let visible_title = title.as_deref().unwrap_or("").trim().to_owned();
    let visible_description = description.as_deref().unwrap_or("").trim().to_owned();
    let visible_value = value.as_deref().unwrap_or("").trim().to_owned();
    // The value as the app reports it, never trimmed and never the
    // placeholder. An empty value is shown only for text-entry roles, where
    // "empty" is a real state; elsewhere it would only add noise.
    let raw_value = value
        .clone()
        .filter(|v| !v.trim().is_empty() || is_text_entry_role(&role));

    let has_content = !visible_title.is_empty()
        || !visible_description.is_empty()
        || !visible_value.is_empty()
        || placeholder.is_some();
    // Some native controls expose no AX action names but do expose a writable
    // AXValue. Finder's transient inline-rename field is the important case:
    // rendering it without an element_index leaves an agent able to see the
    // field but unable to call set_value on it. Probe writability only for the
    // small family of value controls so arbitrary display nodes do not pay an
    // extra AX round trip.
    let value_writability = role_supports_value_addressing(&role)
        .then(|| attribute_settable(element, "AXValue"))
        .flatten();
    let value_settable = actions.is_empty() && value_writability == Some(true);
    // A closed submenu can keep its descendants in AXChildren while reporting
    // those controls disabled. Never assign such a row a live element index:
    // the same native state also causes dispatch to refuse it, and exposing an
    // index for it invites agents to retain an unusable menu target.
    let enabled = if !actions.is_empty() || value_settable {
        copy_bool_attr(element, "AXEnabled")
    } else {
        None
    };
    let is_actionable = is_addressable(!actions.is_empty(), value_settable, enabled);

    if !is_actionable && !has_content && role != "AXWindow" && role != "AXSheet" {
        let (children, failed) = copy_children_reporting(element);
        sightings.child_read_failed |= failed;
        for child in children {
            walk_element(
                child,
                depth + 1,
                parent_index,
                parent_position,
                in_web_content,
                nodes,
                    counter,
                budget,
                sightings,
                max_depth,
            );
            CFRelease(child as CFTypeRef);
        }
        return;
    }

    let element_ptr = element as usize;
    let frame = element_screen_rect(element);
    // Structured `elements` only contains actionable nodes. Keep all new AX
    // round-trips behind that same gate so display-only rows pay no cost.
    // A text field's state is its content, and empty is a real state. Keep
    // "" (so value_equals:"" can be answered) and never report the
    // placeholder hint as content. Reuses the AXValue read above.
    // An unreadable AXValue stays None (unknown), not the placeholder.
    let text_entry = is_text_entry_role(&role);
    // Lossless: whitespace is content too.
    let text_content = copied_value.as_ref().map(|c| c.state_value.clone());
    let mut control_state = read_control_state_if_actionable(is_actionable, || ControlState {
        value_state: copied_value
            .map(|copied| copied.state_value)
            .filter(|v| !v.trim().is_empty())
            .or_else(|| value.clone())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty()),
        value_description: copy_string_attr(element, "AXValueDescription")
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty()),
        min_value: copy_number_attr(element, "AXMinValue"),
        max_value: copy_number_attr(element, "AXMaxValue"),
        enabled,
        selected: copy_bool_attr(element, "AXSelected"),
    });
    if text_entry {
        control_state.value_state = text_content;
    }
    let node = if is_actionable {
        let idx = *counter;
        *counter += 1;
        // Retain so the element stays alive in the cache after `copy_children`
        // releases the per-child ref at the end of the caller's loop.
        CFRetain(element as CFTypeRef);
        AXNode {
            url: if role == "AXLink" {
                copy_url_attr(element)
            } else {
                None
            },
            element_index: Some(idx),
            role: role.clone(),
            title: if visible_title.is_empty() {
                None
            } else {
                Some(visible_title.clone())
            },
            value: raw_value.clone(),
            placeholder: placeholder.clone(),
            value_settable: value_writability,
            focused: None,
            text_selection: None,
            description: if visible_description.is_empty() {
                None
            } else {
                Some(visible_description.clone())
            },
            identifier: identifier.clone(),
            help: help.clone(),
            actions: actions.clone(),
            element_ptr,
            depth,
            parent_element_index: parent_index,
            parent_position,
            frame,
            value_state: control_state.value_state.clone(),
            value_description: control_state.value_description.clone(),
            min_value: control_state.min_value,
            max_value: control_state.max_value,
            enabled: control_state.enabled,
            selected: control_state.selected,
            in_web_content,
        }
    } else {
        AXNode {
            url: if role == "AXLink" {
                copy_url_attr(element)
            } else {
                None
            },
            element_index: None,
            role: role.clone(),
            title: if visible_title.is_empty() {
                None
            } else {
                Some(visible_title.clone())
            },
            value: raw_value.clone(),
            placeholder: placeholder.clone(),
            value_settable: value_writability,
            focused: None,
            text_selection: None,
            description: if visible_description.is_empty() {
                None
            } else {
                Some(visible_description.clone())
            },
            identifier: identifier.clone(),
            help: help.clone(),
            actions: vec![],
            element_ptr,
            depth,
            parent_element_index: parent_index,
            parent_position,
            frame,
            value_state: control_state.value_state.clone(),
            value_description: control_state.value_description.clone(),
            min_value: control_state.min_value,
            max_value: control_state.max_value,
            enabled: control_state.enabled,
            selected: control_state.selected,
            in_web_content,
        }
    };

    // Track this node as the parent for its descendants only when it was
    // assigned an element_index (mirrors what the markdown shows: only
    // indexed rows are addressable in click(element_index=N)).
    let next_parent = node.element_index.or(parent_index);

    let position = nodes.len();
    nodes.push(node);

    let (children, failed) = copy_children_reporting(element);
    sightings.child_read_failed |= failed;
    for child in children {
        walk_element(
            child,
            depth + 1,
            next_parent,
            Some(position),
            in_web_content,
            nodes,
            counter,
            budget,
            sightings,
            max_depth,
        );
        CFRelease(child as CFTypeRef);
    }
}

fn is_web_content_role(role: &str) -> bool {
    let normalized = role
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("webarea") || normalized.contains("documentweb") || normalized == "document"
}

#[cfg(test)]
mod web_content_role_tests {
    use super::is_web_content_role;

    #[test]
    fn recognizes_native_web_document_roles_without_marking_app_chrome() {
        for role in ["AXWebArea", "AXDocumentWeb", "document"] {
            assert!(is_web_content_role(role), "{role} must start web trust");
        }
        for role in ["AXWindow", "AXButton", "AXToolbar"] {
            assert!(!is_web_content_role(role), "{role} stays native");
        }
    }
}

/// UIKit/SwiftUI custom accessibility actions surface through
/// AXUIElementCopyActionNames as the description of the action object, e.g.
/// `Name:Heart\nTarget:0x0\nSelector:(null)`. Only the name carries meaning;
/// the target and selector are always placeholders. Standard `AX*` names pass
/// through unchanged.
fn display_action_name(raw: String) -> String {
    match raw.strip_prefix("Name:") {
        Some(rest) => rest.split('\n').next().unwrap_or(rest).trim().to_owned(),
        None => raw,
    }
}

/// Action names for the markdown outline. Every element answers
/// `AXScrollToVisible` and `AXShowMenu`, and every row inside a scroll view
/// answers the page-scroll pair, whose presence flips with scroll position and
/// would mark whole transcripts "changed" in a diff. The `scroll` and
/// `right_click` tools already cover all four, so listing them per row is pure
/// noise. Addressability and dispatch still use the full `AXNode::actions` list.
fn rendered_action_names(actions: &[String]) -> Vec<String> {
    actions
        .iter()
        .filter(|a| {
            !matches!(
                a.as_str(),
                "AXScrollToVisible" | "AXShowMenu" | "AXScrollUpByPage" | "AXScrollDownByPage"
            )
        })
        .map(|a| a.strip_prefix("AX").unwrap_or(a).to_lowercase())
        .collect()
}

pub(crate) fn format_node_line(node: &AXNode) -> String {
    let mut parts = String::new();

    // Common prefix (with or without index).
    if let Some(idx) = node.element_index {
        parts.push_str(&format!("- [{}] {}", idx, node.role));
    } else {
        parts.push_str(&format!("- {}", node.role));
    }

    // AXTitle → "title"
    if let Some(t) = &node.title {
        parts.push_str(&format!(" \"{}\"", t));
    }
    // AXValue as a JSON string: lossless, and a newline inside a value can
    // never fabricate another tree row.
    if let Some(v) = &node.value {
        parts.push_str(&format!(" = {}", serde_json::json!(v)));
    }
    if let Some(placeholder) = &node.placeholder {
        parts.push_str(&format!(" [placeholder={}]", serde_json::json!(placeholder)));
    }
    // AXDescription → (description) — critical for Calculator digit buttons
    // where AXTitle="" but AXDescription="2".
    if let Some(d) = &node.description {
        parts.push_str(&format!(" ({})", d));
    }

    // Bracketed metadata block (identifier, help, actions).
    if node.element_index.is_some() {
        let mut attrs: Vec<String> = Vec::new();
        if let Some(id) = &node.identifier {
            attrs.push(format!("id={}", id));
        }
        if let Some(h) = &node.help {
            attrs.push(format!("help=\"{}\"", h));
        }
        let action_str = rendered_action_names(&node.actions).join(",");
        if !action_str.is_empty() {
            attrs.push(format!("actions=[{}]", action_str));
        }
        if node.focused == Some(true) {
            attrs.push("focused".into());
            if let Some(selection) = &node.text_selection {
                if let Some(range) = selection.range {
                    attrs.push(format!("selection_utf16={}:{}", range.location, range.length));
                }
                if let Some(text) = &selection.text {
                    attrs.push(format!("selected_text={}", serde_json::json!(text)));
                }
            }
        }
        if !attrs.is_empty() {
            parts.push_str(" [");
            parts.push_str(&attrs.join(" "));
            parts.push(']');
        }
    }

    parts
}

pub(crate) fn render_lines(lines: &[(usize, String)]) -> String {
    let mut out = String::new();
    for (depth, line) in lines {
        for _ in 0..*depth {
            out.push_str("  ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Decode the JSON string token at the start of `s` (as rendered for values
/// and placeholders). Returns the text and the rest after the token.
pub(crate) fn decode_json_token(s: &str) -> Option<(String, &str)> {
    if !s.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    let end = s[1..].char_indices().find_map(|(i, c)| {
        let close = !escaped && c == '"';
        escaped = !escaped && c == '\\';
        close.then_some(i + 1)
    })?;
    let text = serde_json::from_str::<String>(&s[..=end]).ok()?;
    Some((text, &s[end + 1..]))
}

/// A rendered line with its value and placeholder tokens JSON-decoded and
/// titles left as they are, for matching only.
fn unescape_rendered(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = ["= \"", "[placeholder=\""]
        .iter()
        .filter_map(|marker| rest.find(marker).map(|i| (i, marker.len() - 1)))
        .min()
    {
        let (start, prefix) = at;
        out.push_str(&rest[..start + prefix]);
        match decode_json_token(&rest[start + prefix..]) {
            Some((text, after)) => {
                out.push_str(&text);
                rest = after;
            }
            None => {
                // Not a JSON token (a raw title can contain `= "`): keep
                // the quote and scan on for the real value.
                out.push('"');
                rest = &rest[start + prefix + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether a rendered row matches a lowercased query. Values render
/// JSON-escaped, so the decoded text is matched too: a query for
/// `Say "hello"` still finds that value.
fn row_matches(line: &str, needle: &str) -> bool {
    line.to_lowercase().contains(needle) || unescape_rendered(line).to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_title_containing_an_equals_quote_does_not_hide_the_value() {
        let line = format!(r#"- [1] AXTextField "Regex = "\d+"" = {}"#, serde_json::json!(r#"Say "hello""#));
        assert!(row_matches(&line, &r#"Say "hello""#.to_lowercase()));
    }

    #[test]
    fn query_decoding_keeps_backslash_sequences_intact() {
        let line = format!("- AXTextField = {}", serde_json::json!(r"C:\new"));
        assert_eq!(unescape_rendered(&line), r"- AXTextField = C:\new");
        assert!(row_matches(&line, &r"C:\new".to_lowercase()));
    }

    #[test]
    fn query_matches_values_with_rendered_escapes() {
        let line = "- AXStaticText = \"Say \\\"hello\\\"\"";
        assert!(row_matches(line, &r#"Say "hello""#.to_lowercase()));
    }

    #[test]
    fn rendered_values_are_quoted_and_cannot_add_rows() {
        let mut node = AXNode {
            element_index: Some(0),
            role: "AXTextArea".into(),
            title: None,
            value: Some("line one\n- [9] AXButton \"Fake\"".into()),
            placeholder: Some("Ask for follow-up changes".into()),
            value_settable: Some(true),
            focused: None,
            text_selection: None,
            description: None,
            identifier: None,
            help: None,
            actions: vec![],
            element_ptr: 0,
            depth: 0,
            parent_element_index: None,
            parent_position: None,
            url: None,
            frame: None,
            value_state: None,
            value_description: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        };
        let rendered = format_node_line(&node);
        assert_eq!(rendered.lines().count(), 1, "a newline in a value stays inside the row");
        assert!(rendered.contains("[placeholder=\"Ask for follow-up changes\"]"));
        node.value = Some(String::new());
        assert!(format_node_line(&node).contains(" = \"\""), "an empty field shows as empty");
    }

    use super::*;
    use std::cell::Cell;

    #[test]
    fn named_layout_groups_survive_but_empty_wrappers_collapse() {
        for role in ["AXGroup", "AXScrollArea"] {
            assert!(!collapse_layout_container(role, Some("Billing"), None));
            assert!(!collapse_layout_container(role, None, Some("Profile")));
            assert!(!collapse_layout_container(
                role,
                Some("  "),
                Some("Billing")
            ));
            assert!(collapse_layout_container(role, None, None));
            assert!(collapse_layout_container(role, Some(" "), Some("\n\t")));
        }
        assert!(!collapse_layout_container("AXButton", None, None));
        assert!(!collapse_layout_container("AXWebArea", None, None));
    }

    /// One display or actionable row for query tests.
    fn row(index: Option<usize>, role: &str, title: &str, depth: usize, parent: Option<usize>) -> AXNode {
        AXNode {
            element_index: index,
            role: role.into(),
            title: (!title.is_empty()).then(|| title.into()),
            value: None,
            placeholder: None,
            value_settable: None,
            focused: None,
            text_selection: None,
            description: None,
            identifier: None,
            help: None,
            actions: vec![],
            element_ptr: 0,
            depth,
            parent_element_index: None,
            parent_position: parent,
            url: None,
            frame: None,
            value_state: None,
            value_description: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        }
    }

    fn outline(nodes: &[AXNode], text: &str, context: bool) -> String {
        let walk = cua_driver_core::walk_budget::WalkBudget::nodes_only(1000).outcome();
        render_outline(nodes, Some(Query { text, context }), &walk)
    }

    #[test]
    fn save_query_retains_each_named_group_with_its_own_button() {
        let nodes = [
            row(Some(0), "AXWebArea", "", 0, None),
            row(Some(1), "AXGroup", "Profile", 1, Some(0)),
            row(Some(2), "AXButton", "Save", 2, Some(1)),
            row(None, "AXStaticText", "Unrelated", 2, Some(1)),
            row(Some(3), "AXGroup", "Billing", 1, Some(0)),
            row(Some(4), "AXButton", "Save", 2, Some(4)),
        ];
        assert_eq!(
            outline(&nodes, "save", false),
            "- [0] AXWebArea\n  - [1] AXGroup \"Profile\"\n    - [2] AXButton \"Save\"\n  - [3] AXGroup \"Billing\"\n    - [4] AXButton \"Save\"\n"
        );
    }

    /// A message heading with its body: context keeps the body (display
    /// text) under the match, never the sibling message.
    #[test]
    fn context_keeps_a_matched_rows_descendants_only() {
        let nodes = [
            row(None, "AXWindow", "Chat", 0, None),
            row(None, "AXGroup", "From Ada", 1, Some(0)),
            row(None, "AXStaticText", "Lunch at noon?", 2, Some(1)),
            row(None, "AXGroup", "From Bob", 1, Some(0)),
            row(None, "AXStaticText", "Running late", 2, Some(3)),
        ];
        let plain = outline(&nodes, "from ada", false);
        assert!(!plain.contains("Lunch"), "{plain}");
        let context = outline(&nodes, "from ada", true);
        assert!(context.contains("Lunch at noon?"), "{context}");
        assert!(!context.contains("Bob") && !context.contains("Running"), "{context}");
    }

    /// The walker dropped an empty container, so "Save" renders at the depth
    /// of Profile's children although its real parent is the window. The old
    /// text filter, reading indentation, pulled "Profile" in as its parent.
    #[test]
    fn a_dropped_container_does_not_pull_in_a_false_parent() {
        let nodes = [
            row(None, "AXWindow", "W", 0, None),
            row(None, "AXGroup", "Profile", 1, Some(0)),
            row(Some(0), "AXButton", "Save", 2, Some(0)),
        ];
        let text = outline(&nodes, "save", false);
        assert!(!text.contains("Profile"), "{text}");
        assert!(text.contains("AXButton \"Save\""), "{text}");
    }

    #[test]
    fn writable_value_controls_are_addressable_without_actions() {
        assert!(is_addressable(false, true, Some(true)));
        assert!(is_addressable(true, false, None));
        assert!(!is_addressable(false, false, Some(true)));
        assert!(!is_addressable(true, false, Some(false)));

        for role in [
            "AXTextField",
            "AXTextArea",
            "AXComboBox",
            "AXSlider",
            "AXStepper",
            "AXCheckBox",
            "AXRadioButton",
        ] {
            assert!(role_supports_value_addressing(role), "{role}");
        }
        for role in ["AXStaticText", "AXImage", "AXWindow", "AXGroup"] {
            assert!(!role_supports_value_addressing(role), "{role}");
        }
    }

    #[test]
    fn control_state_reads_are_gated_by_actionability() {
        let reads = Cell::new(0);
        let display_only = read_control_state_if_actionable(false, || {
            reads.set(reads.get() + 1);
            ControlState {
                enabled: Some(true),
                ..ControlState::default()
            }
        });
        assert_eq!(reads.get(), 0, "display-only nodes must not read state");
        assert_eq!(display_only.enabled, None);

        let actionable = read_control_state_if_actionable(true, || {
            reads.set(reads.get() + 1);
            ControlState {
                enabled: Some(true),
                ..ControlState::default()
            }
        });
        assert_eq!(reads.get(), 1, "actionable nodes must read state once");
        assert_eq!(actionable.enabled, Some(true));
    }
}

#[cfg(test)]
mod outline_action_tests {
    use super::{display_action_name, rendered_action_names};

    #[test]
    fn custom_action_names_keep_only_the_name() {
        assert_eq!(
            display_action_name("Name:Tapback Details…\nTarget:0x0\nSelector:(null)".into()),
            "Tapback Details…"
        );
        assert_eq!(display_action_name("Name:Pin\nTarget:0x0\nSelector:(null)".into()), "Pin");
        assert_eq!(display_action_name("AXPress".into()), "AXPress");
        assert_eq!(display_action_name("Name:".into()), "");
    }

    #[test]
    fn outline_hides_universal_actions_but_keeps_the_rest() {
        let actions: Vec<String> = [
            "AXPress",
            "AXScrollToVisible",
            "AXCancel",
            "AXShowMenu",
            "AXScrollUpByPage",
            "AXScrollDownByPage",
            "Heart",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(rendered_action_names(&actions), ["press", "cancel", "heart"]);
        let only_universal: Vec<String> = ["AXScrollToVisible", "AXShowMenu", "AXScrollDownByPage"]
            .map(str::to_owned)
            .to_vec();
        assert!(rendered_action_names(&only_universal).is_empty());
    }
}
