// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use crate::{
    CursorAction, CursorSemantics, EndSessionInput, EndSessionOutput, EscalateSessionInput,
    GetSessionInput, GetSessionStateInput, ListSessionsInput, ListSessionsOutput, Platform,
    SchemaMode, SessionOutput, SessionStateOutput, StartSessionInput, StartSessionOutput,
    ToolAnnotations, ToolContract, ToolInput, ToolOutput,
};

const ALL_PLATFORMS: [Platform; 3] = [Platform::Macos, Platform::Windows, Platform::Linux];

pub fn contracts() -> Vec<ToolContract> {
    vec![start(), escalate(), get(), list(), get_state(), end()]
}

fn contract<I: ToolInput, O: ToolOutput>(
    name: &str,
    description: &str,
    capabilities: &[&str],
    annotations: ToolAnnotations,
) -> ToolContract {
    assert_eq!(name, I::TOOL_NAME, "typed input is bound to the wrong tool");
    ToolContract {
        name: name.into(),
        description: description.into(),
        platforms: ALL_PLATFORMS.to_vec(),
        aliases: Vec::new(),
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        annotations,
        schema_mode: SchemaMode::CanonicalRuntime,
        cursor_semantics: Some(CursorSemantics::new(CursorAction::System)),
        input_schema: I::input_schema(),
        success_output_schema: Some(O::output_schema()),
        error_output_schema: None,
        output_validator: crate::validate_typed_output::<O>,
    }
}

fn start() -> ToolContract {
    contract::<StartSessionInput, StartSessionOutput>(
        "start_session",
        "Optional: create or return a session. Pass a short session label and repeat it on every later call; without one, the transport's implicit session is used. Use this to set the initial cursor theme or to revive an ended label; ordinary actions never revive ended labels. Idempotent.",
        &["session.lifecycle.start", "session.capture_scope"],
        ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn escalate() -> ToolContract {
    contract::<EscalateSessionInput, SessionStateOutput>(
        "escalate_session",
        "Deprecated capture-scope compatibility tool. Select window or desktop on each action instead.",
        &["session.capture_scope.escalate"],
        ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
    )
}

fn get_state() -> ToolContract {
    contract::<GetSessionStateInput, SessionStateOutput>(
        "get_session_state",
        "Deprecated: reads a legacy session's capture policy. Use get_session.",
        &["session.capture_scope.read"],
        ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn get() -> ToolContract {
    contract::<GetSessionInput, SessionOutput>(
        "get_session",
        "Read one session's lifecycle, cursor, recording, and idle status (no content). Omit session for your implicit session.",
        &["session.lifecycle.read"],
        ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn list() -> ToolContract {
    contract::<ListSessionsInput, ListSessionsOutput>(
        "list_sessions",
        "List this transport's sessions as content-free summaries. Other callers' sessions are not shown.",
        &["session.lifecycle.list"],
        ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn end() -> ToolContract {
    contract::<EndSessionInput, EndSessionOutput>(
        "end_session",
        "End a session and run its cursor, recording, and configuration cleanup once. Omit session to end your implicit session. Idempotent.",
        &["session.lifecycle.end"],
        ToolAnnotations {
            read_only: false,
            destructive: true,
            idempotent: true,
            open_world: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_description_explains_direct_naming_and_explicit_revival() {
        let description = start().description;
        assert!(description.contains("short session label"));
        assert!(description.contains("repeat it on every later call"));
        assert!(description.contains("implicit session"));
        assert!(description.contains("revive an ended label"));
        assert!(description.contains("ordinary actions never revive ended labels"));
    }
}
