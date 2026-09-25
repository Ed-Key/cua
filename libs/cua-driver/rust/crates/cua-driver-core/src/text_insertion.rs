//! Classify an insertion from field values and an optional observed selection.
/// A half-open range measured in UTF-16 code units, the way macOS reports
/// `AXSelectedTextRange`. Zero length is a caret.
pub use cua_driver_contract::TextSelectionRange;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextInsertionProgress {
    Complete,
    Partial(usize),
    Unchanged,
    Unverifiable,
}

pub fn classify_insertion(
    before: Option<&str>,
    selection: Option<TextSelectionRange>,
    after: Option<&str>,
    after_selection: Option<TextSelectionRange>,
    text: &str,
) -> TextInsertionProgress {
    use TextInsertionProgress::*;
    if text.is_empty() {
        return Complete;
    }
    let (Some(before), Some(after)) = (before, after) else {
        return Unverifiable;
    };
    let selection = selection.or_else(|| {
        before.is_empty().then_some(TextSelectionRange {
            location: 0,
            length: 0,
        })
    });
    let Some(selection) = selection else {
        // Without a caret, only a full insertion which preserves the complete
        // old value can establish delivery. A net increase or an old substring
        // cannot supply a trustworthy partial count.
        return if full_insertion_visible(before, after, text) {
            Complete
        } else {
            Unverifiable
        };
    };
    let Some(end) = selection.location.checked_add(selection.length) else {
        return Unverifiable;
    };
    let (Some(start), Some(end)) = (
        byte_at_utf16(before, selection.location),
        byte_at_utf16(before, end),
    ) else {
        return Unverifiable;
    };
    let Some(inserted) = after
        .strip_prefix(&before[..start])
        .and_then(|tail| tail.strip_suffix(&before[end..]))
    else {
        return Unverifiable;
    };
    if !text.starts_with(inserted) {
        return Unverifiable;
    }
    if before == after && selection.length > 0 {
        // Replacing selected text with identical text leaves the value alone.
        // Require the expected collapsed caret as well, rather than claiming
        // either complete delivery or zero delivery from that value alone.
        let caret = TextSelectionRange {
            location: selection.location + inserted.encode_utf16().count() as u64,
            length: 0,
        };
        if after_selection != Some(caret) {
            return Unverifiable;
        }
    }
    if inserted == text {
        Complete
    } else if inserted.is_empty() && selection.length == 0 {
        Unchanged
    } else {
        Partial(inserted.chars().count())
    }
}

fn byte_at_utf16(text: &str, offset: u64) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += ch.len_utf16() as u64;
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(text.len())
}

fn full_insertion_visible(before: &str, after: &str, text: &str) -> bool {
    if before.len().checked_add(text.len()) != Some(after.len()) {
        return false;
    }
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let suffix: usize = before
        .chars()
        .rev()
        .zip(after.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let start = before.len() - suffix;
    // Every insertion point in this interval preserves the old prefix/suffix.
    // Searching the interval handles overlapping repeated payloads as well.
    start <= prefix
        && after
            .get(start..prefix + text.len())
            .is_some_and(|slice| slice.contains(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use TextInsertionProgress::*;

    fn range(location: u64, length: u64) -> Option<TextSelectionRange> {
        Some(TextSelectionRange { location, length })
    }

    #[test]
    fn caret_distinguishes_a_prefix_from_an_old_suffix() {
        assert_eq!(
            classify_insertion(
                Some("llo"),
                range(0, 0),
                Some("hello"),
                range(2, 0),
                "hello"
            ),
            Partial(2)
        );
        assert_eq!(
            classify_insertion(Some("llo"), None, Some("hello"), None, "hello"),
            Unverifiable
        );
    }

    #[test]
    fn selected_replacement_counts_input_instead_of_net_growth() {
        assert_eq!(
            classify_insertion(Some("world"), range(0, 5), Some("he"), range(2, 0), "hello"),
            Partial(2)
        );
        assert_eq!(
            classify_insertion(
                Some("world"),
                range(0, 5),
                Some("hello"),
                range(5, 0),
                "hello"
            ),
            Complete
        );
    }

    #[test]
    fn identical_replacement_requires_the_selection_to_collapse() {
        assert_eq!(
            classify_insertion(
                Some("hello"),
                range(0, 5),
                Some("hello"),
                range(0, 5),
                "hello"
            ),
            Unverifiable
        );
        assert_eq!(
            classify_insertion(Some("hello"), range(0, 5), Some("hello"), None, "hello"),
            Unverifiable
        );
        assert_eq!(
            classify_insertion(
                Some("hello"),
                range(0, 5),
                Some("hello"),
                range(5, 0),
                "hello"
            ),
            Complete
        );
    }

    #[test]
    fn insertion_and_missing_caret_have_distinct_unchanged_evidence() {
        assert_eq!(
            classify_insertion(
                Some("hello"),
                range(5, 0),
                Some("hello"),
                range(5, 0),
                "hello"
            ),
            Unchanged
        );
        assert_eq!(
            classify_insertion(Some("hello"), None, Some("hello"), None, "hello"),
            Unverifiable
        );
    }

    #[test]
    fn utf16_ranges_do_not_split_surrogates_or_count_bytes_as_characters() {
        assert_eq!(
            classify_insertion(Some("A😀B"), range(1, 2), Some("AéB"), range(2, 0), "é猫"),
            Partial(1)
        );
        assert_eq!(
            classify_insertion(Some("A😀B"), range(2, 1), Some("AéB"), None, "é"),
            Unverifiable
        );
        assert_eq!(
            classify_insertion(Some("x"), range(u64::MAX, 1), Some("xy"), None, "y"),
            Unverifiable
        );
    }

    #[test]
    fn unrelated_changes_and_unreadable_before_are_not_progress() {
        assert_eq!(
            classify_insertion(Some("A:B"), range(2, 0), Some("X:hiB"), None, "hi"),
            Unverifiable
        );
        assert_eq!(
            classify_insertion(Some("old"), None, Some("oldXYZ"), None, "hello"),
            Unverifiable
        );
        assert_eq!(
            classify_insertion(None, None, Some("hello"), None, "hello"),
            Unverifiable
        );
    }

    #[test]
    fn full_insertions_without_a_range_preserve_every_old_character() {
        for before in ["", "a", "ab", "abab", "😀a", "aba"] {
            for text in ["a", "ba", "aba", "😀", "a😀"] {
                for at in before
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(before.len()))
                {
                    let after = format!("{}{}{}", &before[..at], text, &before[at..]);
                    assert_eq!(
                        classify_insertion(Some(before), None, Some(&after), None, text),
                        Complete,
                        "before={before:?}, text={text:?}, insertion={at}"
                    );
                }
            }
        }
    }

    #[test]
    fn empty_before_proves_the_only_possible_insertion_location() {
        assert_eq!(
            classify_insertion(Some(""), None, Some("he"), None, "hello"),
            Partial(2)
        );
        assert_eq!(
            classify_insertion(Some(""), None, Some(""), None, "hello"),
            Unchanged
        );
        assert_eq!(classify_insertion(None, None, None, None, ""), Complete);
    }

    #[test]
    fn selected_deletion_is_not_an_unchanged_field() {
        assert_eq!(
            classify_insertion(Some("old"), range(0, 3), Some(""), range(0, 0), "new"),
            Partial(0)
        );
    }

    #[test]
    fn range_free_completion_matches_literal_insertions_for_short_unicode_values() {
        let mut values = vec![String::new()];
        for a in ["a", "b", "é", "😀"] {
            values.push(a.to_owned());
            for b in ["a", "b", "é", "😀"] {
                values.push(format!("{a}{b}"));
            }
        }
        for before in &values {
            for after in &values {
                for text in values.iter().filter(|s| !s.is_empty()) {
                    let expected = before
                        .char_indices()
                        .map(|(i, _)| i)
                        .chain(std::iter::once(before.len()))
                        .any(|i| format!("{}{}{}", &before[..i], text, &before[i..]) == *after);
                    assert_eq!(
                        classify_insertion(Some(before), None, Some(after), None, text) == Complete,
                        expected,
                        "before={before:?}, after={after:?}, text={text:?}"
                    );
                }
            }
        }
    }
}
