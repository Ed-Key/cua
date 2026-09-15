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

/// Select trusted preorder row positions, without interpreting display text.
pub fn select_preorder_rows(
    rows: impl IntoIterator<Item = (usize, bool)>,
    include_descendants: bool,
) -> Vec<usize> {
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut parented = Vec::new();
    for (depth, matches) in rows {
        while stack
            .last()
            .is_some_and(|(parent_depth, _)| *parent_depth >= depth)
        {
            stack.pop();
        }
        let parent = stack.last().map(|(_, position)| *position);
        parented.push((parent, matches));
        stack.push((depth, parented.len() - 1));
    }
    select_parented_rows(parented, include_descendants)
}

/// Select rows using the walker's nearest retained parent positions. A parent
/// must precede its child. Unlike rendered depths, these edges remain reliable
/// when a native container is omitted from the snapshot.
pub fn select_parented_rows(
    rows: impl IntoIterator<Item = (Option<usize>, bool)>,
    include_descendants: bool,
) -> Vec<usize> {
    let mut parents: Vec<Option<usize>> = Vec::new();
    let mut under_match: Vec<bool> = Vec::new();
    let mut selected: Vec<bool> = Vec::new();
    for (parent, matches) in rows {
        assert!(
            parent.is_none_or(|p| p < parents.len()),
            "parents must precede children"
        );
        let ancestor_matches = parent.is_some_and(|position| under_match[position]);
        parents.push(parent);
        under_match.push(matches || ancestor_matches);
        selected.push(matches || (include_descendants && ancestor_matches));
    }
    // Propagate selection upward only after descendant selection is finished,
    // so a retained ancestor cannot accidentally pull in a sibling branch.
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
mod preorder_tests {
    use super::select_preorder_rows;

    #[test]
    fn query_matches_keep_only_their_actual_ancestors() {
        let rows = [
            (0, false),
            (1, true),
            (2, false),
            (1, false),
            (2, true),
            (0, false),
        ];
        assert_eq!(select_preorder_rows(rows, false), vec![0, 1, 3, 4]);
    }

    #[test]
    fn context_includes_descendants_without_leaking_to_siblings_or_other_roots() {
        let rows = [
            (0, false),
            (1, true),
            (2, false),
            (1, false),
            (2, true),
            (0, false),
        ];
        assert_eq!(select_preorder_rows(rows, true), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn skipped_depths_still_use_the_nearest_actual_ancestor() {
        assert_eq!(
            select_preorder_rows([(0, false), (4, false), (5, false), (4, true)], false),
            vec![0, 3]
        );
        assert!(select_preorder_rows([(0, false), (1, false)], true).is_empty());
        assert!(select_preorder_rows([], true).is_empty());
    }
}
