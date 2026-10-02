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

/// How a watch's line starts when a command (a menu item, a key shortcut)
/// showed no effect at all: the result then leads with a warning, so the
/// action's own words ("Pressed ...") never read as done.
pub const NO_EFFECT: &str = "no effect seen";

/// The warning a [`NO_EFFECT`] line puts first in the summary.
const NO_EFFECT_WARNING: &str = "⚠️ No effect seen (see the Outcome line). ";

/// Append `Outcome: <line>` to the result's first text block (a new block
/// when it has none).
pub fn append(result: &mut ToolResult, line: &str) {
    let note = format!("Outcome: {line}");
    let warning = if line.starts_with(NO_EFFECT) { NO_EFFECT_WARNING } else { "" };
    for content in &mut result.content {
        if let Content::Text { text, .. } = content {
            if !text.trim().is_empty() {
                *text = format!("{warning}{text}\n{note}");
                return;
            }
        }
    }
    result.content.insert(0, Content::text(format!("{warning}{note}")));
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
        let mut command = ToolResult::text("Pressed the menu item.");
        append(&mut command, "no effect seen within 1.5 s; not settled");
        assert!(matches!(&command.content[0], Content::Text { text, .. }
            if text == "⚠️ No effect seen (see the Outcome line). Pressed the menu item.\nOutcome: no effect seen within 1.5 s; not settled"));
    }
}
