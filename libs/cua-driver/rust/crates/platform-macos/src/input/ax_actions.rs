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
    /// Its parent advertises AXSelectedChildren: a list with a selection
    /// model, even while nothing is selected (UIKit table and collection
    /// views under Catalyst).
    pub parent_lists_selection: bool,
}

/// Which step of the chain (clicked element first) is the row a plain click
/// selects, if any. An AppKit row or list item wins, then a cell or icon;
/// otherwise a Catalyst row: something selectable whose parent lists a
/// selection (AXSelectedChildren), or that sits among 2+ selectable siblings
/// one of which is selected (Catalyst gives every element an AXSelected, so
/// AXSelected alone proves no selection model).
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
            find(&|c| c.parent_lists_selection || (c.selectable_peers >= 2 && c.peer_selected))
                .map(|at| (at, RowKind::Catalyst))
        })
}

/// `choose_row`, reading a step's selectable peers (`scan_peers(at)`: count,
/// whether one is selected, and whether the parent lists a selection) only
/// when no AppKit row claims the click.
/// Peers are read bottom up and the climb stops at the first Catalyst row,
/// since Catalyst answers each read slowly and a Finder list can hold
/// thousands of rows. A click on a control scans nothing.
pub(crate) fn find_row(
    chain: &mut [RowCandidate],
    mut scan_peers: impl FnMut(usize) -> Option<(usize, bool, bool)>,
) -> Option<(usize, RowKind)> {
    if chain.first().is_some_and(|clicked| is_control_role(&clicked.role)) {
        return None;
    }
    if let found @ Some(_) = choose_row(chain) {
        return found;
    }
    for at in 0..chain.len() {
        if !chain[at].selectable {
            continue;
        }
        if let Some((peers, any, lists)) = scan_peers(at) {
            chain[at].selectable_peers = peers;
            chain[at].peer_selected = any;
            chain[at].parent_lists_selection = lists;
            if let found @ Some(_) = choose_row(&chain[..=at]) {
                return found;
            }
        }
    }
    None
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

/// Longest a selection scan may take; past it the scan is incomplete.
const SCAN_BUDGET: std::time::Duration = std::time::Duration::from_millis(1000);
/// Bound for one peer read during a scan.
const PEER_READ_TIMEOUT_SECONDS: f32 = 0.25;

/// Selection among `parent`'s children (its rows, for a table), one
/// AXSelected read per child: (selectable children, selected ones other than
/// `row`, whether any is selected, whether the list is complete). The list
/// is incomplete when the children could not be read, there were more than
/// `MAX_SCANNED_PEERS`, a child's selection could not be read (not just
/// unsupported), or the scan ran past `SCAN_BUDGET`.
unsafe fn scan_selection(
    parent: AXUIElementRef,
    row: Option<AXUIElementRef>,
) -> (usize, usize, bool, bool) {
    let deadline = std::time::Instant::now() + SCAN_BUDGET;
    // The list read is bounded too; the container keeps its action timeout.
    AXUIElementSetMessagingTimeout(parent, 0.5);
    let (children, mut complete) =
        match crate::ax::bindings::copy_element_array_attr_checked(parent, "AXRows", 20_000) {
            Ok(rows) => (rows, true),
            Err(_) => {
                let (children, failed) = copy_children_reporting(parent);
                (children, !failed)
            }
        };
    AXUIElementSetMessagingTimeout(parent, crate::ax::tree::AX_MESSAGING_TIMEOUT_SECONDS);
    complete &= children.len() <= MAX_SCANNED_PEERS;
    let (mut selectable, mut others, mut any) = (0, 0, false);
    for (at, child) in children.into_iter().enumerate() {
        // Read the first peers even when the list is too long to prove
        // anything: they still show whether a selection model exists.
        if at < MAX_SCANNED_PEERS {
            if std::time::Instant::now() >= deadline {
                complete = false;
            } else {
                AXUIElementSetMessagingTimeout(child, PEER_READ_TIMEOUT_SECONDS);
                let read = crate::ax::bindings::copy_attribute_checked(child, "AXSelected");
                AXUIElementSetMessagingTimeout(
                    child,
                    crate::ax::tree::AX_MESSAGING_TIMEOUT_SECONDS,
                );
                match read {
                    Ok(value) => {
                        let selected =
                            crate::ax::bindings::coerce_binary_value(value.as_CFTypeRef());
                        match selected {
                            Some(selected) => {
                                selectable += 1;
                                any |= selected;
                                let is_row = row.is_some_and(|row| {
                                    CFEqual(child as CFTypeRef, row as CFTypeRef) != 0
                                });
                                others += usize::from(selected && !is_row);
                            }
                            // A selection value that is neither on nor off.
                            None => complete = false,
                        }
                    }
                    // Not a selectable child (a column, a header).
                    Err(kAXErrorAttributeUnsupported | kAXErrorNoValue) => {}
                    Err(_) => complete = false,
                }
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
            // A control click is never a row click: stop before walking its
            // ancestors (Catalyst exposes AXSelected on nearly all of them).
            let clicked_role = copy_string_attr(element_ptr as AXUIElementRef, "AXRole");
            if clicked_role.as_deref().is_some_and(is_control_role) {
                return None;
            }
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
                    parent_lists_selection: false,
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
            let chosen = find_row(&mut chain, |at| {
                elements[at].1.map(|parent| {
                    if advertises_attribute(parent, "AXSelectedChildren") {
                        return (0, false, true);
                    }
                    let (peers, _, any, _) = scan_selection(parent, None);
                    (peers, any, false)
                })
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

    /// The row around the element an AX hit test returned for a pointer
    /// click. A read-only text area there (a message bubble) is the row's
    /// content, so the walk starts at its parent; an editable one or any
    /// other control is the click's target, never its row.
    pub fn capture_for_hit(hit_ptr: usize) -> Option<Self> {
        unsafe {
            let hit = hit_ptr as AXUIElementRef;
            if copy_string_attr(hit, "AXRole").as_deref() != Some("AXTextArea") {
                return Self::capture(hit_ptr);
            }
            if attribute_settable(hit, "AXValue") == Some(true) {
                return None;
            }
            let parent = copy_element_attr(hit, "AXParent")?;
            let row = Self::capture(parent as usize);
            CFRelease(parent as CFTypeRef);
            row
        }
    }

    /// Whether a screen point lies inside the row as it is now (2 points in
    /// from its edges): a pointer click there still lands on this row.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        unsafe { element_screen_rect(self.row) }.is_some_and(|[rx, ry, rw, rh]| {
            x >= rx + 2.0 && x <= rx + rw - 2.0 && y >= ry + 2.0 && y <= ry + rh - 2.0
        })
    }

    /// What a click at the row's centre reaches, when it is something inside
    /// the row (not the row itself) that takes AXPress and is not a control:
    /// a Catalyst row has no AXPress of its own, but its content does, and
    /// pressing it selects the row as a tap there would. Retained; release
    /// with CFRelease.
    pub fn press_target_at_centre(&self, pid: i32) -> Option<AXUIElementRef> {
        let (x, y) = self.center()?;
        unsafe {
            let hit = element_at_screen_position(pid, x, y)?;
            let role = copy_string_attr(hit, "AXRole").unwrap_or_default();
            let content = !is_control_role(&role)
                || (role == "AXTextArea" && attribute_settable(hit, "AXValue") != Some(true));
            let usable = content
                && CFEqual(hit as CFTypeRef, self.row as CFTypeRef) == 0
                && self.is_ancestor_of(hit)
                && copy_action_names(hit).iter().any(|action| action == "AXPress");
            if usable {
                Some(hit)
            } else {
                CFRelease(hit as CFTypeRef);
                None
            }
        }
    }

    unsafe fn is_ancestor_of(&self, element: AXUIElementRef) -> bool {
        let mut current = copy_element_attr(element, "AXParent");
        for _ in 0..MAX_SELECTION_ANCESTORS {
            let Some(parent) = current else { return false };
            let found = CFEqual(parent as CFTypeRef, self.row as CFTypeRef) != 0;
            current = if found { None } else { copy_element_attr(parent, "AXParent") };
            CFRelease(parent as CFTypeRef);
            if found {
                return true;
            }
        }
        if let Some(parent) = current {
            CFRelease(parent as CFTypeRef);
        }
        false
    }

    /// The row's name (Catalyst rows carry only a description).
    pub fn name(&self) -> String {
        ["AXTitle", "AXDescription"]
            .into_iter()
            .filter_map(|attribute| unsafe { copy_string_attr(self.row, attribute) })
            .find(|name| !name.trim().is_empty())
            .unwrap_or_default()
    }

    /// `name`, read so that a failed read is told apart from no name: None
    /// when the row did not answer (a name that cannot be read proves
    /// nothing about which row this is).
    pub fn read_name(&self) -> Option<String> {
        let mut name = String::new();
        for attribute in ["AXTitle", "AXDescription"] {
            match unsafe { copy_string_attr_checked(self.row, attribute) } {
                Ok(text) if name.trim().is_empty() => name = text,
                Ok(_) => {}
                Err(kAXErrorAttributeUnsupported | kAXErrorNoValue) => {}
                Err(_) => return None,
            }
        }
        Some(name)
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

    /// Ask for this row as the only selection with one AX write: the owning
    /// table's AXSelectedRows when it is settable, else the row's
    /// AXSelected. Returns the write's error (success is not proof it took).
    pub fn select_via_ax(&self) -> AXError {
        unsafe {
            if let Some(container) = self.container {
                if attribute_settable(container, "AXSelectedRows") == Some(true) {
                    let rows = core_foundation::array::CFArray::from_CFTypes(&[
                        core_foundation::base::CFType::wrap_under_get_rule(self.row as CFTypeRef),
                    ]);
                    let name = core_foundation::string::CFString::new("AXSelectedRows");
                    return AXUIElementSetAttributeValue(
                        container,
                        name.as_concrete_TypeRef(),
                        rows.as_CFTypeRef(),
                    );
                }
            }
            set_bool_attr_true(self.row, "AXSelected")
        }
    }
}

/// Whether an AX write's error means the app refused it before acting, so
/// nothing changed. Any other error (a timeout, a generic failure) may hide
/// a write that took effect.
pub(crate) fn ax_write_rejected(err: AXError) -> bool {
    const ILLEGAL_ARGUMENT: AXError = -25201;
    const ACTION_UNSUPPORTED: AXError = -25206;
    [
        kAXErrorAttributeUnsupported,
        kAXErrorNotImplemented,
        kAXErrorAPIDisabled,
        kAXErrorInvalidUIElement,
        ILLEGAL_ARGUMENT,
        ACTION_UNSUPPORTED,
    ]
    .contains(&err)
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
            parent_lists_selection: false,
        }
    }

    fn in_list(mut candidate: RowCandidate) -> RowCandidate {
        candidate.parent_lists_selection = true;
        candidate
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
        // CatalystSearch / Messages (Catalyst UITableView): a row group whose
        // list advertises AXSelectedChildren, with nothing selected yet and
        // one row left after a search.
        let message = [
            in_list(step("AXGroup", true, 1, false)),
            step("AXGroup", true, 1, false),
            step("AXGroup", true, 2, false),
        ];
        assert_eq!(choose_row(&message), Some((0, RowKind::Catalyst)));
        // A plain view inside the row (its list is the row's parent).
        let inside = [step("AXGroup", true, 1, false), in_list(step("AXGroup", true, 1, false))];
        assert_eq!(choose_row(&inside), Some((1, RowKind::Catalyst)));
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
        // A list that lists its selection still never claims a control.
        let button_in_list = [in_list(step("AXButton", true, 1, false))];
        assert_eq!(choose_row(&button_in_list), None);
        // Listing a selection needs a selectable child.
        assert_eq!(choose_row(&[in_list(step("AXGroup", false, 0, false))]), None);
        assert_eq!(choose_row(&[]), None);
    }

    #[test]
    fn only_a_definite_refusal_counts_as_an_unsent_write() {
        assert!(ax_write_rejected(kAXErrorAttributeUnsupported));
        assert!(ax_write_rejected(-25201), "illegal argument");
        assert!(!ax_write_rejected(kAXErrorSuccess));
        assert!(!ax_write_rejected(kAXErrorCannotComplete), "a timeout may have taken");
        assert!(!ax_write_rejected(kAXErrorFailure));
    }

    /// A control click scans no peers, however many selectable ancestors it
    /// has; a Catalyst row click still scans until the first row proves out.
    #[test]
    fn a_control_click_does_no_peer_scan() {
        for control in ["AXButton", "AXLink", "AXCheckBox"] {
            let mut chain = vec![
                step(control, true, 0, false),
                step("AXGroup", true, 0, false),
                step("AXGroup", true, 0, false),
            ];
            let mut scans = 0;
            let found = find_row(&mut chain, |_| {
                scans += 1;
                Some((11, true, true))
            });
            assert_eq!((found, scans), (None, 0), "{control}");
        }
        let mut stocks = vec![
            step("AXGenericElement", true, 0, false),
            step("AXGroup", true, 0, false),
            step("AXGroup", true, 0, false),
        ];
        let mut scanned = vec![];
        let found = find_row(&mut stocks, |at| {
            scanned.push(at);
            Some(if at == 0 { (1, false, false) } else { (11, true, false) })
        });
        assert_eq!(found, Some((1, RowKind::Catalyst)));
        assert_eq!(scanned, [0, 1], "stops at the first Catalyst row");
        // A row whose list lists its selection is found at once.
        let mut message = vec![step("AXGroup", true, 0, false), step("AXGroup", true, 0, false)];
        let mut scanned = vec![];
        let found = find_row(&mut message, |at| {
            scanned.push(at);
            Some((0, false, at == 0))
        });
        assert_eq!((found, scanned), (Some((0, RowKind::Catalyst)), vec![0]));
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
