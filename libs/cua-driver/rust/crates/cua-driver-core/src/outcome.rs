//! What a native action changed in the app, read back after it.
//!
//! A platform starts a watch before the action (`Tool::begin_outcome`) and
//! the dispatcher finishes it once the action returned and the global input
//! lock is released, so the bounded wait for the app to settle never holds
//! up another agent's input. The line goes on the end of the action's first
//! text block, which the action contract publishes as `summary`: clients that
//! read only `structuredContent` see it, and no schema changes.

use async_trait::async_trait;

use crate::protocol::{Content, ToolResult};

#[async_trait]
pub trait OutcomeWatch: Send {
    /// Read the app again and say what changed since the watch began, or
    /// `None` when the watch could not read anything (never an error: the
    /// action already ran, so a failed read must not fail it).
    async fn finish(self: Box<Self>) -> Option<String>;
}

/// Append `Outcome: <line>` to the result's first text block (a new block
/// when it has none).
pub fn append(result: &mut ToolResult, line: &str) {
    let note = format!("Outcome: {line}");
    for content in &mut result.content {
        if let Content::Text { text, .. } = content {
            if !text.trim().is_empty() {
                text.push('\n');
                text.push_str(&note);
                return;
            }
        }
    }
    result.content.insert(0, Content::text(note));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_to_the_first_text_block_or_adds_one() {
        let mut result = ToolResult::text("✅ Pressed return on pid 1.");
        append(&mut result, "selected in icon view: a.pdf (1 of 3)");
        match &result.content[0] {
            Content::Text { text, .. } => assert_eq!(
                text,
                "✅ Pressed return on pid 1.\nOutcome: selected in icon view: a.pdf (1 of 3)"
            ),
            _ => panic!("text block"),
        }
        let mut empty = ToolResult::default();
        append(&mut empty, "x");
        assert!(matches!(&empty.content[0], Content::Text { text, .. } if text == "Outcome: x"));
    }
}
