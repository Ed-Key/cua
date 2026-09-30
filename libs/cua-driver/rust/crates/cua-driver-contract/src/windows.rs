// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Portable native application discovery and exact-window observations.

use crate::{ToolInput, ToolOutput};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

fn string_schema(g: &mut SchemaGenerator) -> Schema {
    String::json_schema(g)
}
fn bool_schema(g: &mut SchemaGenerator) -> Schema {
    bool::json_schema(g)
}
fn pid_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer", "minimum":1, "maximum":4294967295_u64})
}
fn positive_integer_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer", "minimum":1})
}
fn nonnegative_integer_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"integer", "minimum":0})
}

fn element_fields_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string", "enum":["none","compact","full"]})
}

/// macOS: how much of each `elements` record `get_window_state` returns.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Enum,
)]
#[serde(rename_all = "snake_case")]
pub enum ElementFields {
    /// Omit per-element frame, depth and parent_index, omit enabled when
    /// true and selected when false, and drop the top-level _note.
    Compact,
    /// Every field.
    Full,
    // Declared last so the existing UniFFI ordinals keep their values.
    /// Omit the `elements` array; address elements by tree index with
    /// `element_token` = `<snapshot_id>:<index>` or `element_index` +
    /// `snapshot_id`. The macOS default.
    #[default]
    None,
}

fn nullable_pid_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":["integer","null"], "minimum":0, "maximum":4294967295_u64})
}

fn nullable_z_index_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":["integer","null"]})
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ListAppsInput {}

impl ToolInput for ListAppsInput {
    const TOOL_NAME: &'static str = "list_apps";
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct ListWindowsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "pid_schema")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    pub on_screen_only: Option<bool>,
}

impl ToolInput for ListWindowsInput {
    const TOOL_NAME: &'static str = "list_windows";

    fn validate(&self) -> Result<(), String> {
        if self.pid == Some(0) {
            return Err("window discovery requires a positive process ID".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct GetWindowStateInput {
    /// Pass with window_id, or pass app instead of both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "pid_schema")]
    #[uniffi(default = None)]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    #[uniffi(default = None)]
    pub window_id: Option<u64>,
    /// App name or bundle id; reads its only window on the current Space. Use
    /// pid + window_id when it has several. macOS only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    #[uniffi(default = None)]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub query: Option<String>,
    /// macOS only. Default false. With a nonblank query, also keep every row
    /// collected under each match, not only its ancestors. Display-only rows
    /// appear in tree_markdown; structured elements hold only actionable rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    #[uniffi(default = None)]
    pub query_context: Option<bool>,
    /// macOS only. Default true: after the first look at a window, return only
    /// rows added, changed, or removed since the previous look by this session.
    /// False forces the full outline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    #[uniffi(default = None)]
    pub diff: Option<bool>,
    /// macOS only. "none" (default) omits the elements array (tokens are
    /// `<snapshot_id>:<index>` from the tree); "compact" omits per-element
    /// frame, depth and parent_index, omits enabled when true and selected
    /// when false, and drops the _note; "full" returns every field. Other
    /// platforms accept and ignore it and always return full records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "element_fields_schema")]
    #[uniffi(default = None)]
    pub element_fields: Option<ElementFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    pub include_accessibility_tree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "bool_schema")]
    pub include_screenshot: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "string_schema")]
    pub screenshot_out_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    pub max_elements: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    pub max_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "positive_integer_schema")]
    pub max_dimension: Option<u32>,
    /// Optional per-call long-edge ceiling. Zero requests native resolution;
    /// omit it to preserve the configured session or global behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "nonnegative_integer_schema")]
    pub max_image_dimension: Option<u32>,
    /// Wall-clock budget for the accessibility walk in milliseconds
    /// (default 1000 on every platform). A walk that runs out returns a
    /// partial tree flagged `truncated` rather than failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "timeout_ms_schema")]
    #[uniffi(default = None)]
    pub timeout_ms: Option<u32>,
}

/// Bounds of the accessibility-walk budget, shared with every live backend
/// schema (`cua_driver_core::tool_schema::timeout_ms_schema`).
pub const TIMEOUT_MS_MIN: u32 = 100;
pub const TIMEOUT_MS_MAX: u32 = 120_000;

fn timeout_ms_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": TIMEOUT_MS_MIN,
        "maximum": TIMEOUT_MS_MAX
    })
}

impl ToolInput for GetWindowStateInput {
    const TOOL_NAME: &'static str = "get_window_state";
    fn validate(&self) -> Result<(), String> {
        window_state_target_form(
            self.app.as_deref(),
            self.pid.is_some(),
            self.window_id.is_some(),
        )?;
        if self.pid == Some(0) || self.window_id == Some(0) {
            return Err("window observation requires positive process and window IDs".into());
        }
        if [self.max_elements, self.max_depth, self.max_dimension].contains(&Some(0)) {
            return Err("window observation limits must be positive".into());
        }
        if self.query_context == Some(true)
            && self.query.as_deref().is_none_or(|q| q.trim().is_empty())
        {
            return Err("query_context requires a nonblank query".into());
        }
        if self
            .timeout_ms
            .is_some_and(|ms| !(TIMEOUT_MS_MIN..=TIMEOUT_MS_MAX).contains(&ms))
        {
            return Err(format!(
                "timeout_ms must be between {TIMEOUT_MS_MIN} and {TIMEOUT_MS_MAX} milliseconds"
            ));
        }
        if self.include_accessibility_tree == Some(false) && self.include_screenshot == Some(false)
        {
            return Err("window observation requires accessibility or screenshot capture".into());
        }
        Ok(())
    }
}

/// `get_window_state` takes exactly one target form: `app` alone, or `pid` +
/// `window_id`. Shared by the typed contract and the live macOS tool so both
/// refuse the same way.
pub fn window_state_target_form(
    app: Option<&str>,
    has_pid: bool,
    has_window_id: bool,
) -> Result<(), String> {
    match (app, has_pid, has_window_id) {
        (Some(app), false, false) if !app.trim().is_empty() => Ok(()),
        (Some(_), false, false) => Err("app must be a nonblank app name or bundle id".into()),
        (Some(_), _, _) => Err("pass either app, or pid + window_id, not both".into()),
        (None, true, true) => Ok(()),
        (None, _, _) => Err("get_window_state needs app, or pid + window_id".into()),
    }
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
    pub running: bool,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct ListAppsOutput {
    pub apps: Vec<AppInfo>,
}

impl ToolOutput for ListAppsOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowInfo {
    pub window_id: u64,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_pid_schema")]
    pub pid: Option<u32>,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub is_on_screen: bool,
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(required, schema_with = "nullable_z_index_schema")]
    /// Higher values are closer to the front. Null is unknown; callers must not infer an order from array position.
    pub z_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimized: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_space_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_current_space: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_ids: Option<Vec<u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct ListWindowsOutput {
    pub windows: Vec<WindowInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_space_id: Option<u64>,
    /// macOS: how many windows an unfiltered call left out because they are
    /// not on the current Space. Absent when nothing was filtered by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub off_screen_omitted: Option<u32>,
}

impl ToolOutput for ListWindowsOutput {
    fn output_schema() -> serde_json::Value {
        let mut schema = crate::outputs::output_schema_with_additional_properties::<Self>(true);
        // Stacking semantics are part of the existing live discovery contract.
        schema["properties"]["windows"]["items"]["properties"]["z_index"]["description"] =
            serde_json::json!("Higher values are closer to the front. Null is unknown; callers must not infer an order from array position.");
        schema
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct ElementFrame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowElement {
    pub element_index: u64,
    pub role: String,
    /// Absent in macOS compact element records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Reported keyboard focus. Currently populated on macOS; absent is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub focused: Option<bool>,
    /// Only read for the focused text control. Unsupported attributes stay absent.
    /// Web accessibility state is observational, not renderer verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub text_selection: Option<TextSelection>,
    /// The hint an empty field shows (`AXPlaceholderValue`); never content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub placeholder: Option<String>,
    /// A link's destination (macOS AXURL), kept apart from its label and value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub url: Option<String>,
    /// Whether the value can be written; absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub value_settable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_description: Option<String>,
    /// macOS compact records omit it when true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// macOS compact records omit it when false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_web_content: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    /// Absent in macOS compact element records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_index: Option<u64>,
    /// Screen coordinates. Absent in macOS compact element records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<ElementFrame>,
    /// The same rectangle in pixels of this response's screenshot, the space
    /// of window pixel `x`,`y`. Present when a screenshot was delivered and
    /// the element has geometry; kept in compact records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub screenshot_frame: Option<ElementFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct SnapshotImage {
    pub mime_type: String,
    pub data_base64: String,
}

/// A half-open range measured in UTF-16 code units. Zero length is a caret.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Eq, PartialEq, uniffi::Record)]
pub struct TextSelectionRange {
    pub location: u64,
    pub length: u64,
}

/// Best-effort platform accessibility state, not proof of a rendered web edit.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Eq, PartialEq, uniffi::Record)]
pub struct TextSelection {
    /// Empty string means a readable empty selection; absence means unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TextSelectionRange>,
}

/// One window an app opened after an action, addressable with
/// `get_window_state(pid, window_id)`. `pid` is the window's real owner, which
/// can differ from the acted-on app for out-of-process panels.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct SurfaceWindow {
    pub pid: i64,
    pub window_id: u64,
    pub app_name: String,
    pub title: String,
}

/// New windows reported on the first read after an action. `rebind` is set
/// only when exactly one new window appeared and every candidate's owner was
/// resolved; otherwise the caller chooses from `new_windows` itself.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct WindowChange {
    pub new_windows: Vec<SurfaceWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebind: Option<SurfaceWindow>,
    /// System indicator windows that also appeared and were left out
    /// (screen-sharing badges, tiny or above-normal-level overlays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_windows: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, uniffi::Record)]
pub struct WindowStateOutput {
    pub pid: u32,
    pub window_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elements: Option<Vec<WindowElement>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned_element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered_element_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elements_complete: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_frame_valid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_bounds: Option<WindowBounds>,
    /// Windows the target app opened since the last action on it that has
    /// not been reported yet. Present only when something new appeared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[uniffi(default = None)]
    pub window_change: Option<WindowChange>,
    /// Image content belongs to the MCP envelope, never structuredContent.
    #[serde(skip)]
    #[schemars(skip)]
    pub images: Vec<SnapshotImage>,
}

impl ToolOutput for WindowStateOutput {
    fn validate(&self) -> Result<(), String> {
        if let Some(change) = &self.window_change {
            if change.new_windows.is_empty() {
                return Err("window_change must list at least one new window".into());
            }
            if let Some(rebind) = &change.rebind {
                if change.new_windows.len() != 1 || change.new_windows[0] != *rebind {
                    return Err("window_change.rebind must be the only new window".into());
                }
            }
        }
        match (self.screenshot_width, self.screenshot_height) {
            (None, None) => {}
            (Some(width), Some(height)) if width > 0 && height > 0 => {}
            _ => return Err("screenshot dimensions must be a positive width/height pair".into()),
        }
        if self
            .screenshot_scale
            .is_some_and(|scale| !scale.is_finite() || scale <= 0.0)
        {
            return Err("screenshot_scale must be finite and positive".into());
        }
        if let (Some(elements), Some(returned)) = (&self.elements, self.returned_element_count) {
            if elements.len() as u64 != returned {
                return Err("returned_element_count does not match elements".into());
            }
        }
        if let (Some(total), Some(returned)) =
            (self.total_element_count, self.returned_element_count)
        {
            if returned > total {
                return Err("returned_element_count exceeds total_element_count".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_app_success_shapes_allow_unavailable_metadata() {
        for metadata in [
            json!({"bundle_id":"org.example.App", "kind":"desktop", "launch_path":"/Applications/Example.app", "last_used":null}),
            json!({"bundle_id":null, "kind":"desktop", "launch_path":"C:\\Example.exe", "last_used":null}),
            json!({"bundle_id":null, "kind":null, "launch_path":null, "last_used":null}),
        ] {
            let mut app =
                json!({"pid":7,"name":"Example","running":true,"active":false,"windows":[]});
            app.as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            let output: ListAppsOutput = serde_json::from_value(json!({"apps":[app]})).unwrap();
            assert_eq!(output.apps[0].pid, 7);
        }
    }

    #[test]
    fn native_window_success_shapes_preserve_unknown_pid_and_order() {
        for metadata in [
            json!({"pid":7,"z_index":4,"layer":0,"space_ids":[1],"current_space_id":1,"on_current_space":true}),
            json!({"pid":7,"z_index":0,"layer":0,"minimized":false}),
            json!({"pid":null,"z_index":null,"x":-10,"y":0,"width":100,"height":80}),
        ] {
            let mut window = json!({"window_id":9007199254740993_u64,"app_name":"Example","title":"Document","bounds":{"x":-10,"y":0,"width":100,"height":80},"is_on_screen":true});
            window
                .as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            let output: ListWindowsOutput =
                serde_json::from_value(json!({"windows":[window]})).unwrap();
            assert_eq!(output.windows[0].window_id, 9007199254740993);
        }
        let missing = json!({"windows":[{"window_id":1,"pid":null,"app_name":"Example","title":"Document","bounds":{"x":0,"y":0,"width":100,"height":80},"is_on_screen":true}]});
        assert!(serde_json::from_value::<ListWindowsOutput>(missing).is_err());
    }

    #[test]
    fn window_state_supports_capture_only_and_degraded_native_shapes() {
        let capture: WindowStateOutput = serde_json::from_value(json!({
            "pid":7,"window_id":9,"screenshot_width":100,"screenshot_height":80,
            "screenshot_mime_type":"image/png","screenshot_file_path":"/tmp/example.png",
            "window_bounds":{"x":0,"y":0,"width":100,"height":80},"screenshot_scale":1.0
        }))
        .unwrap();
        assert!(capture.elements.is_none());
        assert!(capture.images.is_empty());
        for value in [
            json!({"pid":7,"window_id":9,"element_count":0,"elements":[],"tree_markdown":"","elements_complete":false,"degraded":true,"degraded_reason":"ax_window_unresolved"}),
            json!({"pid":7,"window_id":9,"element_count":1,"elements":[{"element_index":0,"role":"button","depth":1,"element_token":"s1:0","label":"Apply","frame":{"x":10,"y":20,"w":30,"h":40},"enabled":true}],"elements_complete":false,"screenshot_error":"capture unavailable"}),
            json!({"pid":7,"window_id":9,"elements":[{"element_index":0,"role":"button","depth":1}],"degraded":true,"degraded_reason":"accessibility_window_identity_unproven"}),
            // macOS element_fields:"compact" records: no frame, depth or parent_index.
            json!({"pid":7,"window_id":9,"elements":[{"element_index":0,"role":"AXButton","element_token":"s1:0","label":"Apply","screenshot_frame":{"x":20,"y":40,"w":60,"h":80}},{"element_index":1,"role":"AXCheckBox","enabled":false,"selected":true}],"elements_complete":false}),
        ] {
            let output: WindowStateOutput = serde_json::from_value(value).unwrap();
            assert!(output.elements.is_some());

            output.validate().unwrap();
        }
        let compact: WindowElement = serde_json::from_value(json!({
            "element_index": 0, "role": "AXButton",
            "screenshot_frame": {"x": 20, "y": 40, "w": 60, "h": 80}
        }))
        .unwrap();
        assert_eq!(compact.depth, None);
        assert_eq!(
            compact.screenshot_frame,
            Some(ElementFrame { x: 20.0, y: 40.0, w: 60.0, h: 80.0 }),
            "typed compact records keep screenshot geometry"
        );
    }

    #[test]
    fn images_are_envelope_only_and_not_structured_schema_fields() {
        let mut output: WindowStateOutput =
            serde_json::from_value(json!({"pid":7,"window_id":9})).unwrap();
        output.images.push(SnapshotImage {
            mime_type: "image/png".into(),
            data_base64: "aGVsbG8=".into(),
        });
        assert!(serde_json::to_value(&output)
            .unwrap()
            .get("images")
            .is_none());
        assert!(WindowStateOutput::output_schema()["properties"]
            .get("images")
            .is_none());
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["max_elements"]["minimum"],
            1
        );
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["max_image_dimension"]["minimum"],
            0
        );
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["max_dimension"]["minimum"],
            1
        );
        let native_resolution: GetWindowStateInput = serde_json::from_value(json!({
            "pid": 7,
            "window_id": 9,
            "max_image_dimension": 0
        }))
        .unwrap();
        native_resolution.validate().unwrap();
        assert_eq!(ListAppsInput::input_schema()["properties"], json!({}));
        assert_eq!(
            GetWindowStateInput::input_schema()["properties"]["element_fields"]["enum"],
            json!(["none", "compact", "full"])
        );
        let none: GetWindowStateInput = serde_json::from_value(json!({
            "pid": 7, "window_id": 9, "element_fields": "none"
        }))
        .unwrap();
        assert_eq!(none.element_fields, Some(ElementFields::None));
        let full: GetWindowStateInput = serde_json::from_value(json!({
            "pid": 7, "window_id": 9, "element_fields": "full"
        }))
        .unwrap();
        assert_eq!(full.element_fields, Some(ElementFields::Full));
        assert!(serde_json::from_value::<GetWindowStateInput>(json!({
            "pid": 7, "window_id": 9, "element_fields": "all"
        }))
        .is_err());
    }

    #[test]
    fn get_window_state_takes_app_alone_or_pid_and_window_id() {
        let parse = |value| serde_json::from_value::<GetWindowStateInput>(value).unwrap();
        parse(json!({"app": "TextEdit"})).validate().unwrap();
        parse(json!({"app": "com.apple.TextEdit"}))
            .validate()
            .unwrap();
        parse(json!({"pid": 7, "window_id": 9})).validate().unwrap();
        for (value, error) in [
            (
                json!({"app": "TextEdit", "pid": 7, "window_id": 9}),
                "not both",
            ),
            (json!({"app": "TextEdit", "pid": 7}), "not both"),
            (json!({}), "needs app, or pid + window_id"),
            (json!({"pid": 7}), "needs app, or pid + window_id"),
            (json!({"window_id": 9}), "needs app, or pid + window_id"),
            (json!({"app": "  "}), "nonblank"),
            (json!({"pid": 0, "window_id": 9}), "positive"),
        ] {
            let got = parse(value.clone()).validate().unwrap_err();
            assert!(got.contains(error), "{value}: {got}");
        }
        let schema = GetWindowStateInput::input_schema();
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.contains(&json!("pid")) && !required.contains(&json!("window_id")));
        assert_eq!(schema["properties"]["app"]["type"], "string");
    }

    #[test]
    fn invalid_window_observation_metadata_fails_validation() {
        for extra in [
            json!({"screenshot_width":100}),
            json!({"screenshot_width":0,"screenshot_height":80}),
            json!({"screenshot_scale":0}),
            json!({"elements":[],"returned_element_count":1}),
            json!({"total_element_count":1,"returned_element_count":2}),
        ] {
            let mut value = json!({"pid":7,"window_id":9});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let output: WindowStateOutput = serde_json::from_value(value).unwrap();
            assert!(output.validate().is_err());
        }
    }
}
