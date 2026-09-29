//! AX action dispatch — the preferred click/interaction path for indexed elements.

use crate::ax::bindings::*;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef, TCFType};

const MAX_SELECTION_ANCESTORS: usize = 8;

fn is_selectable_container_role(role: &str) -> bool {
    matches!(role, "AXRow" | "AXCell" | "AXListItem" | "AXImage")
}

/// Clicking one of these means operating the control, never selecting the
/// row it sits in.
fn is_control_role(role: &str) -> bool {
    matches!(
        role,
        "AXButton"
            | "AXCheckBox"
            | "AXRadioButton"
            | "AXPopUpButton"
            | "AXMenuButton"
            | "AXMenuItem"
            | "AXLink"
            | "AXDisclosureTriangle"
            | "AXSlider"
            | "AXIncrementor"
            | "AXComboBox"
            | "AXTextArea"
            | "AXSearchField"
            | "AXSecureTextField"
    )
}

/// One step of the walk from a clicked element up to its window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowCandidate {
    pub role: String,
    /// The element exposes a readable AXSelected.
    pub selectable: bool,
    /// Children of its parent that expose AXSelected, itself included.
    pub selectable_peers: usize,
    /// One of those peers (or itself) is selected right now.
    pub peer_selected: bool,
}

/// Which step of the chain (clicked element first) is the row a plain click
/// selects, if any. An AppKit row or list item wins, then a cell or icon;
/// otherwise a Catalyst row: something selectable among 2+ selectable
/// siblings, one of which is selected (Catalyst gives every element an
/// AXSelected, so a selection model is only proven by a selected peer).
/// A click on a control inside a row is not a row click.
pub(crate) fn choose_row(chain: &[RowCandidate]) -> Option<(usize, RowKind)> {
    let clicked = chain.first()?;
    if is_control_role(&clicked.role) {
        return None;
    }
    let find = |accept: &dyn Fn(&RowCandidate) -> bool| chain.iter().position(|c| c.selectable && accept(c));
    find(&|c| matches!(c.role.as_str(), "AXRow" | "AXListItem"))
        .or_else(|| find(&|c| is_selectable_container_role(&c.role)))
        .map(|at| (at, RowKind::Native))
        .or_else(|| {
            find(&|c| c.selectable_peers >= 2 && c.peer_selected).map(|at| (at, RowKind::Catalyst))
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// AppKit / SwiftUI collection row: accepts AX selection writes.
    Native,
    /// Catalyst row: ignores AX selection writes; selects on press or pointer.
    Catalyst,
}

/// A clicked list row, retained with its container, for exclusive selection
/// and its read-back.
pub struct RowSelection {
    row: AXUIElementRef,
    container: Option<AXUIElementRef>,
    pub role: String,
    pub kind: RowKind,
}

impl Drop for RowSelection {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.row as CFTypeRef);
            if let Some(container) = self.container {
                CFRelease(container as CFTypeRef);
            }
        }
    }
}

/// How many peers a read-back scans at most when the container has no
/// AXSelectedRows (a Catalyst list shows tens of rows, not thousands).
const MAX_SCANNED_PEERS: usize = 500;

/// The selection around a row: whether it is selected, and how many other
/// rows are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowReadback {
    pub target: bool,
    pub others: usize,
}

impl RowReadback {
    pub fn exclusive(self) -> bool {
        self.target && self.others == 0
    }
}

/// Selection among `parent`'s children (its rows, for a table), one
/// AXSelected read per child: (selectable children, selected ones other than
/// `row`, whether any is selected, whether the list is complete). The list
/// is incomplete when the children could not be read or there were more
/// than `MAX_SCANNED_PEERS`.
unsafe fn scan_selection(
    parent: AXUIElementRef,
    row: Option<AXUIElementRef>,
) -> (usize, usize, bool, bool) {
    let (children, complete) =
        match crate::ax::bindings::copy_element_array_attr_checked(parent, "AXRows", 20_000) {
            Ok(rows) => (rows, true),
            Err(_) => {
                let (children, failed) = copy_children_reporting(parent);
                (children, !failed)
            }
        };
    let complete = complete && children.len() <= MAX_SCANNED_PEERS;
    let (mut selectable, mut others, mut any) = (0, 0, false);
    for (at, child) in children.into_iter().enumerate() {
        if at < MAX_SCANNED_PEERS {
            if let Some(selected) = copy_bool_attr(child, "AXSelected") {
                selectable += 1;
                any |= selected;
                let is_row =
                    row.is_some_and(|row| CFEqual(child as CFTypeRef, row as CFTypeRef) != 0);
                others += usize::from(selected && !is_row);
            }
        }
        CFRelease(child as CFTypeRef);
    }
    (selectable, others, any, complete)
}

impl RowSelection {
    /// The row a plain click on `element_ptr` should select, if it sits in a
    /// list with a readable selection model.
    pub fn capture(element_ptr: usize) -> Option<Self> {
        unsafe {
            let mut chain = Vec::new();
            let mut elements = Vec::new();
            let mut current = element_ptr as AXUIElementRef;
            CFRetain(current as CFTypeRef);
            for step in 0..MAX_SELECTION_ANCESTORS {
                let parent = copy_element_attr(current, "AXParent");
                chain.push(RowCandidate {
                    role: copy_string_attr(current, "AXRole").unwrap_or_default(),
                    selectable: copy_bool_attr(current, "AXSelected").is_some(),
                    selectable_peers: 0,
                    peer_selected: false,
                });
                elements.push((current, parent));
                let Some(parent) = parent else { break };
                if step + 1 == MAX_SELECTION_ANCESTORS
                    || copy_string_attr(parent, "AXRole").as_deref() == Some("AXWindow")
                {
                    break;
                }
                // The next step owns `current` separately from this step's
                // copy of the parent; each is released once below.
                CFRetain(parent as CFTypeRef);
                current = parent;
            }
            // Peers are read only when no AppKit row claims the click (a
            // Finder list can hold thousands of rows), bottom up, stopping at
            // the first Catalyst row: Catalyst answers each read slowly.
            let chosen = choose_row(&chain).or_else(|| {
                for at in 0..chain.len() {
                    if let (true, Some(parent)) = (chain[at].selectable, elements[at].1) {
                        let (peers, _, any, _) = scan_selection(parent, None);
                        chain[at].selectable_peers = peers;
                        chain[at].peer_selected = any;
                        if let found @ Some(_) = choose_row(&chain[..=at]) {
                            return found;
                        }
                    }
                }
                None
            });
            let mut result = None;
            for (at, (element, parent)) in elements.into_iter().enumerate() {
                match chosen {
                    Some((row_at, kind)) if row_at == at => {
                        result = Some(RowSelection {
                            row: element,
                            container: parent,
                            role: chain[at].role.clone(),
                            kind,
                        });
                    }
                    _ => {
                        CFRelease(element as CFTypeRef);
                        if let Some(parent) = parent {
                            CFRelease(parent as CFTypeRef);
                        }
                    }
                }
            }
            result
        }
    }

    /// Whether the row still answers (it was not replaced).
    pub fn readable(&self) -> bool {
        unsafe { copy_bool_attr(self.row, "AXSelected") }.is_some()
    }

    /// The row's centre in screen points.
    pub fn center(&self) -> Option<(f64, f64)> {
        unsafe { element_screen_rect(self.row) }
            .map(|rect| (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0))
    }

    pub fn observe(&self) -> Option<RowReadback> {
        unsafe {
            let target = copy_bool_attr(self.row, "AXSelected")?;
            let container = self.container?;
            let others = match crate::ax::bindings::copy_element_array_attr_checked(
                container,
                "AXSelectedRows",
                20_000,
            ) {
                Ok(selected) => {
                    let count = selected
                        .iter()
                        .filter(|&&row| CFEqual(row as CFTypeRef, self.row as CFTypeRef) == 0)
                        .count();
                    for row in selected {
                        CFRelease(row as CFTypeRef);
                    }
                    count
                }
                Err(_) => {
                    // Without a complete peer list, exclusivity is unproven.
                    let (_, others, _, complete) = scan_selection(container, Some(self.row));
                    if !complete {
                        return None;
                    }
                    others
                }
            };
            Some(RowReadback { target, others })
        }
    }

    /// Ask for this row as the only selection: the owning table's
    /// AXSelectedRows when it accepts that, else the row's AXSelected.
    /// Returns whether the app accepted a write (not whether it took).
    pub fn select_via_ax(&self) -> bool {
        unsafe {
            if let Some(container) = self.container {
                if attribute_settable(container, "AXSelectedRows") == Some(true) {
                    let rows = core_foundation::array::CFArray::from_CFTypes(&[
                        core_foundation::base::CFType::wrap_under_get_rule(self.row as CFTypeRef),
                    ]);
                    let name = core_foundation::string::CFString::new("AXSelectedRows");
                    if AXUIElementSetAttributeValue(
                        container,
                        name.as_concrete_TypeRef(),
                        rows.as_CFTypeRef(),
                    ) == kAXErrorSuccess
                    {
                        return true;
                    }
                }
            }
            set_bool_attr_true(self.row, "AXSelected") == kAXErrorSuccess
        }
    }
}

/// Read the selection state of the nearest collection-like element without
/// mutating it. This is used to verify a coordinate fallback when AppKit
/// exposes `AXSelected` but refuses to set it directly (notably Finder icon
/// views).
pub fn nearest_container_selection_state(element_ptr: usize) -> Option<(String, bool)> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role) {
            if let Some(selected) = unsafe { copy_bool_attr(current, "AXSelected") } {
                if owns_current {
                    unsafe { CFRelease(current as CFTypeRef) };
                }
                return Some((role, selected));
            }
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        let Some(parent) = parent else {
            return None;
        };
        current = parent;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
}

/// A retained selection context for proving the settled result of a modified
/// pointer click.
///
/// Reading only the target's `AXSelected` bit is insufficient for multi-select:
/// AppKit can expose a transient target transition while it is still resolving
/// the synthetic gesture, and a modifier-less outcome can replace the prior
/// selection with the target. Keep the target and every selected sibling alive
/// across delivery so callers can require both the intended target transition
/// and preservation of the pre-existing selection.
pub struct SelectionReadback {
    role: String,
    target: AXUIElementRef,
    peer_model_observed: bool,
    previously_selected_peers: Vec<AXUIElementRef>,
}

impl SelectionReadback {
    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn observe(&self) -> Option<(bool, bool)> {
        let target_selected = unsafe { copy_bool_attr(self.target, "AXSelected") }?;
        let peers_preserved = self.peer_model_observed
            && self
                .previously_selected_peers
                .iter()
                .all(|&peer| unsafe { copy_bool_attr(peer, "AXSelected") } == Some(true));
        Some((target_selected, peers_preserved))
    }
}

impl Drop for SelectionReadback {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.target as CFTypeRef);
            for peer in self.previously_selected_peers.drain(..) {
                CFRelease(peer as CFTypeRef);
            }
        }
    }
}

/// Capture the nearest selectable container and its currently-selected peers.
/// Returns `None` when the platform does not expose a readable selection model;
/// callers must then leave the action unverifiable instead of inventing proof.
pub fn capture_nearest_container_selection(element_ptr: usize) -> Option<SelectionReadback> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role)
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            if !owns_current {
                unsafe { CFRetain(current as CFTypeRef) };
            }
            let target = current;
            let parent = unsafe { copy_element_attr(target, "AXParent") };
            let mut peers = Vec::new();
            let mut peer_model_observed = false;
            if let Some(parent) = parent {
                for child in unsafe { copy_children(parent) } {
                    let is_target =
                        unsafe { CFEqual(child as CFTypeRef, target as CFTypeRef) != 0 };
                    peer_model_observed |= is_target;
                    if !is_target && unsafe { copy_bool_attr(child, "AXSelected") } == Some(true) {
                        peers.push(child);
                    } else {
                        unsafe { CFRelease(child as CFTypeRef) };
                    }
                }
                unsafe { CFRelease(parent as CFTypeRef) };
            }
            return Some(SelectionReadback {
                role,
                target,
                peer_model_observed,
                previously_selected_peers: peers,
            });
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        let Some(parent) = parent else {
            return None;
        };
        current = parent;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
}

fn ensure_ax_enabled(enabled: Option<bool>, action: &str) -> anyhow::Result<()> {
    if enabled == Some(false) {
        anyhow::bail!(
            "refusing {action}: the target reports AXEnabled=false. \
             Retry this action with delivery_mode:\"foreground\" or call bring_to_front first"
        );
    }
    Ok(())
}

/// Refuse AX actions that macOS reports as disabled.
///
/// This must be checked immediately before dispatch rather than trusting the
/// cached snapshot value: foreground delivery can make a menu item live after
/// it was resolved, while backgrounding can disable it in the other direction.
pub fn ensure_ax_action_enabled(element_ptr: usize, action: &str) -> anyhow::Result<()> {
    let enabled = unsafe { copy_bool_attr(element_ptr as AXUIElementRef, "AXEnabled") };
    ensure_ax_enabled(enabled, action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_elements_are_refused_before_dispatch() {
        let error = ensure_ax_enabled(Some(false), "AXPick").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AXEnabled=false"));
        assert!(message.contains("delivery_mode:\"foreground\""));
        assert!(message.contains("bring_to_front"));
    }

    #[test]
    fn enabled_or_unreported_state_is_allowed() {
        assert!(ensure_ax_enabled(Some(true), "AXPress").is_ok());
        assert!(ensure_ax_enabled(None, "AXPress").is_ok());
    }

    fn step(role: &str, selectable: bool, peers: usize, peer_selected: bool) -> RowCandidate {
        RowCandidate {
            role: role.into(),
            selectable,
            selectable_peers: peers,
            peer_selected,
        }
    }

    #[test]
    fn a_row_click_finds_the_row_that_owns_the_selection() {
        // Finder list: name field -> cell (selectable, the row's other cells
        // are its peers) -> row. The row wins over the cell.
        let finder = [
            step("AXTextField", false, 0, false),
            step("AXCell", true, 4, false),
            step("AXRow", true, 5, true),
            step("AXOutline", false, 0, false),
        ];
        assert_eq!(choose_row(&finder), Some((2, RowKind::Native)));
        // System Settings sidebar: label -> row.
        let settings = [step("AXStaticText", false, 0, false), step("AXRow", true, 36, true)];
        assert_eq!(choose_row(&settings), Some((1, RowKind::Native)));
        // Finder icon view: the icon itself.
        assert_eq!(choose_row(&[step("AXImage", true, 4, false)]), Some((0, RowKind::Native)));
        // Stocks (Catalyst): the element is selectable but alone in its
        // wrapper; the wrapper sits among selectable rows, one selected.
        let stocks = [
            step("AXGenericElement", true, 1, false),
            step("AXGroup", true, 11, true),
            step("AXGroup", true, 3, false),
        ];
        assert_eq!(choose_row(&stocks), Some((1, RowKind::Catalyst)));
    }

    #[test]
    fn controls_and_unproven_catalyst_lists_are_not_row_clicks() {
        // A button inside a row operates the button.
        let button = [step("AXButton", true, 1, false), step("AXRow", true, 5, true)];
        assert_eq!(choose_row(&button), None);
        // Catalyst reports AXSelected on everything: without a selected peer
        // nothing proves a selection model (a toolbar group among groups).
        let toolbar = [step("AXGenericElement", true, 1, false), step("AXGroup", true, 3, false)];
        assert_eq!(choose_row(&toolbar), None);
        assert_eq!(choose_row(&[]), None);
    }

    #[test]
    fn only_an_exclusive_selection_confirms_a_row_click() {
        assert!(RowReadback { target: true, others: 0 }.exclusive());
        assert!(!RowReadback { target: true, others: 1 }.exclusive(), "additive");
        assert!(!RowReadback { target: false, others: 0 }.exclusive());
    }

    #[test]
    fn selection_fallback_is_limited_to_collection_item_roles() {
        for role in ["AXRow", "AXCell", "AXListItem", "AXImage"] {
            assert!(is_selectable_container_role(role), "{role}");
        }
        for role in ["AXButton", "AXTextField", "AXWindow", "AXOutline"] {
            assert!(!is_selectable_container_role(role), "{role}");
        }
    }
}

/// Set AXFocused=true on an element (for pre-focusing before key press).
pub fn focus_element(element_ptr: usize) -> anyhow::Result<()> {
    let err = unsafe { set_bool_attr_true(element_ptr as AXUIElementRef, "AXFocused") };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        // Focus errors are often benign (element doesn't support focus).
        tracing::warn!("AXSetAttribute(AXFocused) returned {err}");
        Ok(())
    }
}

/// Report whether `element_ptr` is the application's currently focused element.
///
/// This is a read-only confirmation for the foreground typing rung: an
/// `AXFocused` write can be accepted by the element and then immediately
/// clobbered when AppKit installs the window's remembered first responder, so
/// "the write returned success" is not evidence that focus stuck. Identity is
/// compared with `CFEqual` because the app hands back a fresh `AXUIElementRef`
/// for the same underlying element.
///
/// A `false` return is deliberately conservative: an app whose
/// `AXFocusedUIElement` is unreadable reports not-focused, which at worst costs
/// one extra re-apply.
pub fn is_element_focused(pid: i32, element_ptr: usize) -> bool {
    unsafe {
        let Some(focused) = crate::ax::bindings::focused_element_of_pid(pid) else {
            return false;
        };
        let same = CFEqual(focused as CFTypeRef, element_ptr as CFTypeRef) != 0;
        CFRelease(focused as CFTypeRef);
        same
    }
}

/// Set the AXValue of an element (for dropdowns, text fields, etc.).
pub fn set_ax_value(element_ptr: usize, value: &str) -> anyhow::Result<()> {
    let err = unsafe { set_string_attr(element_ptr as AXUIElementRef, "AXValue", value) };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
    }
}
