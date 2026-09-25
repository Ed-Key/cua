//! Stable row numbers across successive snapshots of one window, and the
//! change-only outline built from them.
//!
//! The first look at a window numbers its actionable rows 0..N exactly as
//! before. Every later look asks macOS whether each element is the same
//! object as one seen last time (`CFEqual` on the retained `AXUIElementRef`)
//! and, if so, keeps its number. New elements take fresh, never-reused
//! numbers. With numbers stable, the outline can be sent as a diff: rows
//! added, rows whose rendered line or control state changed, and the numbers
//! of rows that disappeared.
//!
//! Display-only rows (labels, static text, disabled controls) have no
//! retained element, so they are compared by content instead: a line that
//! did not exist under the same parent last time is reported as added, and a
//! line that vanished is reported as removed. That keeps a label change such
//! as `counter=0` → `counter=1` visible in a diff.

use std::collections::{HashMap, HashSet};

use core_foundation::base::{CFEqual, CFHash, CFTypeRef};

use super::bindings::AXUIElementRef;
use super::tree::{format_node_line, render_lines, AXNode};

/// Content key of a display-only row: nearest indexed ancestor, depth, line.
type DisplayKey = (Option<usize>, usize, String);

/// Everything the next diff needs from a snapshot.
#[derive(Clone, Default)]
pub struct Rows {
    /// element_index → (depth, signature) for indexed rows.
    pub indexed: HashMap<usize, (usize, String)>,
    /// Multiset of display-only rows.
    pub display: HashMap<DisplayKey, usize>,
}

/// The rendered line plus the structured state the line does not print:
/// control state, place in the tree, and whole-point geometry. A change in
/// any of these marks the row `~`, so a consumer of a diff never keeps a
/// stale frame or parent for a row it was not shown again.
fn signature(node: &AXNode) -> String {
    // Whole points: sub-point jitter from animation is not a change worth a
    // row, and the structured frame's fractions matter to nobody clicking it.
    let frame = node.frame.map(|[x, y, w, h]| [x.round(), y.round(), w.round(), h.round()]);
    format!(
        "{}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{}\u{1}{:?}",
        format_node_line(node),
        node.value_state,
        node.value_settable,
        node.selected,
        node.enabled,
        node.value_description,
        node.parent_element_index,
        node.depth,
        frame,
        node.min_value,
        node.max_value,
        node.in_web_content,
        node.actions
    )
}

pub fn rows_of(nodes: &[AXNode]) -> Rows {
    let mut rows = Rows::default();
    for n in nodes {
        match n.element_index {
            Some(id) => {
                rows.indexed.insert(id, (n.depth, signature(n)));
            }
            None => {
                *rows
                    .display
                    .entry((n.parent_element_index, n.depth, format_node_line(n)))
                    .or_default() += 1;
            }
        }
    }
    rows
}

/// Rewrite `element_index` (and `parent_element_index`) so a row keeps the
/// number it had in the previous snapshot when `match_prior(ptr)` recognises
/// its element. Unmatched rows take `next_id`, which only ever grows.
pub fn assign_stable_indices(
    nodes: &mut [AXNode],
    mut match_prior: impl FnMut(usize) -> Option<usize>,
    next_id: &mut usize,
) {
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut used: HashSet<usize> = HashSet::new();
    for node in nodes.iter_mut() {
        let Some(old) = node.element_index else { continue };
        let reused = match_prior(node.element_ptr).filter(|id| !used.contains(id));
        let id = reused.unwrap_or_else(|| {
            let id = *next_id;
            *next_id += 1;
            id
        });
        used.insert(id);
        remap.insert(old, id);
        node.element_index = Some(id);
    }
    // Fresh ids must stay above every reused one so later snapshots never
    // hand a new element a number a vanished row still owns.
    if let Some(&max) = used.iter().max() {
        *next_id = (*next_id).max(max + 1);
    }
    for node in nodes.iter_mut() {
        if let Some(p) = node.parent_element_index {
            node.parent_element_index = remap.get(&p).copied();
        }
    }
}

pub struct OutlineDiff {
    pub added: Vec<usize>,
    pub changed: Vec<usize>,
    pub removed: Vec<usize>,
    /// Display-only lines that appeared or vanished.
    pub display_added: usize,
    pub display_removed: usize,
    pub markdown: String,
}

impl OutlineDiff {
    pub fn touched(&self) -> HashSet<usize> {
        self.added.iter().chain(&self.changed).copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.changed.is_empty()
            && self.removed.is_empty()
            && self.display_added == 0
            && self.display_removed == 0
    }
}

/// Compare the current rows against `prior` and render only the difference.
/// Lines keep their indentation; `+` marks a new row, `~` a changed indexed
/// row, and `x` a display-only line that vanished.
pub fn diff_outline(prior: &Rows, nodes: &[AXNode], window_title: &str) -> OutlineDiff {
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut display_seen: HashMap<DisplayKey, usize> = HashMap::new();
    let mut display_added = 0;
    for node in nodes {
        let line = format_node_line(node);
        let marker = match node.element_index {
            Some(id) => {
                seen.insert(id);
                match prior.indexed.get(&id) {
                    None => {
                        added.push(id);
                        "+"
                    }
                    Some((_, old)) if *old != signature(node) => {
                        changed.push(id);
                        "~"
                    }
                    Some(_) => continue,
                }
            }
            None => {
                let key = (node.parent_element_index, node.depth, line.clone());
                let n = display_seen.entry(key.clone()).or_default();
                *n += 1;
                if *n > prior.display.get(&key).copied().unwrap_or(0) {
                    display_added += 1;
                    "+"
                } else {
                    continue;
                }
            }
        };
        lines.push((node.depth, format!("{marker}{}", line.trim_start_matches('-'))));
    }
    let mut removed: Vec<usize> = prior.indexed.keys().filter(|id| !seen.contains(id)).copied().collect();
    removed.sort_unstable();
    let mut display_removed_lines: Vec<(usize, String)> = Vec::new();
    for (key, &count) in &prior.display {
        let still = display_seen.get(key).copied().unwrap_or(0);
        for _ in still..count {
            display_removed_lines.push((key.1, format!("x{}", key.2.trim_start_matches('-'))));
        }
    }
    display_removed_lines.sort();
    let display_removed = display_removed_lines.len();

    let none = added.is_empty()
        && changed.is_empty()
        && removed.is_empty()
        && display_added == 0
        && display_removed == 0;
    let mut markdown = if none {
        format!("No changes since your last look at \"{window_title}\". Rows keep their numbers; pair element_index with this response's snapshot_id.\n")
    } else {
        format!(
            "Changes since your last look at \"{window_title}\" (+ added, ~ changed, x vanished text; unchanged rows keep their numbers, pair element_index with this response's snapshot_id):\n"
        )
    };
    if !removed.is_empty() {
        markdown.push_str(&format!("removed rows: {}\n", ranges(&removed)));
    }
    markdown.push_str(&render_lines(&lines));
    markdown.push_str(&render_lines(&display_removed_lines));
    OutlineDiff {
        added,
        changed,
        removed,
        display_added,
        display_removed,
        markdown,
    }
}

/// `[1,2,3,7,9,10]` → `"1-3, 7, 9-10"`.
pub fn ranges(sorted: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        out.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    out.join(", ")
}

/// Matcher from prior `(element_index, retained ptr)` pairs to the current
/// snapshot's raw pointers, keyed by `CFHash` so each lookup does one bucket
/// scan of `CFEqual` rather than a full pass.
///
/// # Safety
/// Every pointer in `prior` must stay retained while the returned closure is
/// alive; `CFHash`/`CFEqual` dereference them.
pub unsafe fn identity_matcher(prior: &[(usize, usize)]) -> impl FnMut(usize) -> Option<usize> + '_ {
    let mut buckets: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for &(id, ptr) in prior {
        if ptr != 0 {
            buckets
                .entry(CFHash(ptr as AXUIElementRef as CFTypeRef))
                .or_default()
                .push((id, ptr));
        }
    }
    move |ptr: usize| {
        if ptr == 0 {
            return None;
        }
        let key = CFHash(ptr as AXUIElementRef as CFTypeRef);
        buckets.get(&key)?.iter().find_map(|&(id, prior_ptr)| {
            (CFEqual(ptr as AXUIElementRef as CFTypeRef, prior_ptr as AXUIElementRef as CFTypeRef) != 0)
                .then_some(id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: Option<usize>, depth: usize, parent: Option<usize>, value: &str, ptr: usize) -> AXNode {
        AXNode {
            element_index: index,
            role: if index.is_some() { "AXButton".into() } else { "AXStaticText".into() },
            title: None,
            value: Some(value.into()),
            description: None,
            identifier: None,
            help: None,
            actions: if index.is_some() { vec!["AXPress".into()] } else { vec![] },
            element_ptr: ptr,
            depth,
            parent_element_index: parent,
            frame: None,
            value_state: None,
            value_description: None,
            placeholder: None,
            value_settable: None,
            focused: None,
            text_selection: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        }
    }

    #[test]
    fn writability_change_marks_the_row_changed() {
        let mut a = node(Some(1), 0, None, "v", 1);
        let mut b = node(Some(1), 0, None, "v", 1);
        a.value_settable = Some(true);
        b.value_settable = Some(false);
        assert_ne!(signature(&a), signature(&b));
        b.value_settable = None;
        assert_ne!(signature(&a), signature(&b), "unknown is a change too");
    }

    #[test]
    fn rows_keep_numbers_across_a_second_look_and_new_rows_never_reuse_one() {
        // First look: three buttons numbered 0..3 in walk order plus a label.
        let mut first = vec![
            node(Some(0), 0, None, "Save", 10),
            node(Some(1), 1, Some(0), "Cancel", 11),
            node(Some(2), 1, Some(0), "Help", 12),
            node(None, 1, Some(0), "counter=0", 0),
        ];
        let mut next = 0;
        assign_stable_indices(&mut first, |_| None, &mut next);
        assert_eq!(next, 3);
        let prior = rows_of(&first);

        // Second look: Cancel is gone, Help moved up, a new Send button appeared,
        // Save's value changed, and the label counted up.
        let mut second = vec![
            node(Some(0), 0, None, "Save…", 10),
            node(Some(1), 1, Some(0), "Help", 12),
            node(Some(2), 1, Some(0), "Send", 13),
            node(None, 1, Some(0), "counter=1", 0),
        ];
        let by_ptr = |ptr: usize| match ptr {
            10 => Some(0),
            12 => Some(2),
            _ => None,
        };
        assign_stable_indices(&mut second, by_ptr, &mut next);
        let ids: Vec<_> = second.iter().filter_map(|n| n.element_index).collect();
        assert_eq!(ids, vec![0, 2, 3], "Help keeps 2, Send gets a fresh 3, 1 is never reused");
        assert_eq!(second[1].parent_element_index, Some(0));
        assert_eq!(next, 4);

        let diff = diff_outline(&prior, &second, "Dialog");
        assert_eq!(diff.changed, vec![0]);
        assert_eq!(diff.added, vec![3]);
        assert_eq!(diff.removed, vec![1]);
        assert_eq!((diff.display_added, diff.display_removed), (1, 1));
        assert!(diff.markdown.contains("removed rows: 1\n"), "{}", diff.markdown);
        assert!(diff.markdown.contains("~ [0] AXButton = \"Save…\""), "{}", diff.markdown);
        assert!(diff.markdown.contains("  + [3] AXButton = \"Send\""), "{}", diff.markdown);
        assert!(diff.markdown.contains("  + AXStaticText = \"counter=1\""), "{}", diff.markdown);
        assert!(diff.markdown.contains("  x AXStaticText = \"counter=0\""), "{}", diff.markdown);
        assert!(!diff.markdown.contains("Help"), "unchanged rows are omitted");

        let same = diff_outline(&rows_of(&second), &second, "Dialog");
        assert!(same.markdown.starts_with("No changes"));
        assert!(same.is_empty() && same.touched().is_empty());
    }

    #[test]
    fn control_state_changes_mark_a_row_changed_even_when_its_line_is_identical() {
        let mut checkbox = node(Some(0), 0, None, "", 10);
        checkbox.value_state = Some("0".into());
        let prior = rows_of(std::slice::from_ref(&checkbox));
        checkbox.value_state = Some("1".into());
        let diff = diff_outline(&prior, std::slice::from_ref(&checkbox), "Prefs");
        assert_eq!(diff.changed, vec![0]);
        checkbox.value_state = Some("0".into());
        checkbox.selected = Some(true);
        assert_eq!(diff_outline(&prior, std::slice::from_ref(&checkbox), "Prefs").changed, vec![0]);
    }

    #[test]
    fn duplicate_identity_matches_do_not_share_a_number() {
        let mut nodes = vec![node(Some(0), 0, None, "a", 1), node(Some(1), 0, None, "b", 1)];
        let mut next = 5;
        assign_stable_indices(&mut nodes, |_| Some(4), &mut next);
        assert_eq!(nodes[0].element_index, Some(4));
        assert_eq!(nodes[1].element_index, Some(5));
        assert_eq!(next, 6);
    }

    #[test]
    fn removed_rows_collapse_into_ranges() {
        assert_eq!(ranges(&[1, 2, 3, 7, 9, 10]), "1-3, 7, 9-10");
        assert_eq!(ranges(&[]), "");
        assert_eq!(ranges(&[4]), "4");
    }
}
