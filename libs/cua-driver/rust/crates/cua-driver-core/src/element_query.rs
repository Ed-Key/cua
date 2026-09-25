//! Shared projection for query-filtered accessibility snapshots.
//!
//! Platform walkers keep their complete native cache so original handles stay
//! resolvable. This module only projects the protocol response to the indexed
//! rows rendered by the filtered Markdown tree.

use serde_json::Value;
use std::collections::HashSet;

pub fn project_elements_for_query(
    mut elements: Vec<Value>,
    query: Option<&str>,
    filtered_markdown: &str,
) -> Vec<Value> {
    if query.is_none() {
        return elements;
    }

    let visible_indices: HashSet<u64> = filtered_markdown
        .lines()
        .filter_map(|line| {
            let rendered = line.trim_start().strip_prefix("- [")?;
            let (index, _) = rendered.split_once(']')?;
            index.parse().ok()
        })
        .collect();
    elements.retain(|entry| {
        entry
            .get("element_index")
            .and_then(Value::as_u64)
            .is_some_and(|index| visible_indices.contains(&index))
    });
    elements
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projection_preserves_original_indices_for_matches_and_ancestors() {
        let elements = (0..5)
            .map(|element_index| json!({ "element_index": element_index }))
            .collect();
        let markdown = "- [0] Window\n  - [1] Window\n    - [3] Left\n";
        let projected = project_elements_for_query(elements, Some("Left"), markdown);
        let indices: Vec<_> = projected
            .iter()
            .map(|entry| entry["element_index"].as_u64().unwrap())
            .collect();
        assert_eq!(indices, vec![0, 1, 3]);
    }

    #[test]
    fn absent_query_returns_every_element() {
        let elements = vec![json!({ "element_index": 7 })];
        assert_eq!(
            project_elements_for_query(elements.clone(), None, ""),
            elements
        );
    }
}

/// Select row positions for a query from the walker's parent edges: each row
/// is `(parent position, matches)`, and a parent precedes its child. Keeps
/// every match and its real ancestors; with `include_descendants`, also every
/// row under a match. Unlike rendered indentation, parent edges stay right
/// when the walker drops an empty container but keeps its children.
pub fn select_parented_rows(
    rows: impl IntoIterator<Item = (Option<usize>, bool)>,
    include_descendants: bool,
) -> Vec<usize> {
    let mut parents: Vec<Option<usize>> = Vec::new();
    let mut under_match: Vec<bool> = Vec::new();
    let mut selected: Vec<bool> = Vec::new();
    for (parent, matches) in rows {
        // A parent that does not precede its child is a walker bug; treat the
        // row as a root instead of failing the whole read.
        debug_assert!(parent.is_none_or(|p| p < parents.len()));
        let parent = parent.filter(|&p| p < parents.len());
        let ancestor_matches = parent.is_some_and(|p| under_match[p]);
        parents.push(parent);
        under_match.push(matches || ancestor_matches);
        selected.push(matches || (include_descendants && ancestor_matches));
    }
    // Pull in ancestors only after descendants are chosen, so a kept ancestor
    // never drags in a sibling branch.
    for position in (0..selected.len()).rev() {
        if selected[position] {
            if let Some(parent) = parents[position] {
                selected[parent] = true;
            }
        }
    }
    selected
        .into_iter()
        .enumerate()
        .filter_map(|(i, keep)| keep.then_some(i))
        .collect()
}

#[cfg(test)]
mod parented_tests {
    use super::select_parented_rows;

    /// 0 Window
    ///   1 List          (match)
    ///     2 Row
    ///   3 Group
    ///     4 Row         (match)
    /// 5 Menu bar
    fn rows(matches: [bool; 6]) -> Vec<(Option<usize>, bool)> {
        [None, Some(0), Some(1), Some(0), Some(3), None]
            .into_iter()
            .zip(matches)
            .collect()
    }

    #[test]
    fn matches_keep_only_their_real_ancestors() {
        let r = rows([false, true, false, false, true, false]);
        assert_eq!(select_parented_rows(r, false), vec![0, 1, 3, 4]);
    }

    #[test]
    fn context_adds_descendants_without_siblings_or_other_roots() {
        let r = rows([false, true, false, false, false, false]);
        assert_eq!(select_parented_rows(r, true), vec![0, 1, 2]);
    }

    /// The case indentation gets wrong: row 2's native parent (an empty
    /// container) was dropped, so it renders at row 1's child depth although
    /// it hangs off row 0. A match on row 2 must not pull in row 1.
    #[test]
    fn a_dropped_container_does_not_make_a_false_parent() {
        let r = vec![(None, false), (Some(0), false), (Some(0), true)];
        assert_eq!(select_parented_rows(r, false), vec![0, 2]);
    }

    #[test]
    fn a_parent_that_does_not_precede_its_child_is_treated_as_a_root() {
        let r = vec![(None, false), (Some(5), true)];
        let result = std::panic::catch_unwind(|| select_parented_rows(r.clone(), false));
        if cfg!(debug_assertions) {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), vec![1]);
        }
    }
}
