//! No-wait post-action surface notes.
//!
//! Before an action, record which accessibility roots (windows, sheets,
//! dialogs, popovers) the target app has. The action then returns without
//! waiting. The next `get_window_state` for the same session and app compares
//! against that record and reports what appeared, once, as `window_change`.
//!
//! Nothing here waits for a window that may never come: proving that nothing
//! will open needs a timeout, while noticing that something did open only
//! needs a later look, and the agent's next read is that look. Roots come from
//! the app's own AX tree, not the screen-wide window list, because one file
//! dialog shows up there as several windows (an AX-empty accessory, an
//! out-of-process twin and the readable panel); only the readable one is an
//! AX root of the app.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_contract::{SurfaceWindow, WindowChange};
use cua_driver_core::protocol::{Content, ToolResult};
use cua_driver_core::tool::{ProtectedResourceOwnership, Tool, ToolDef};
use serde_json::Value;

use crate::ax::bindings::{
    ax_get_window_id_checked, copy_element_array_attr_checked, copy_geometry_attr_checked,
    copy_string_attr_checked, kAXErrorAttributeUnsupported as AX_ATTRIBUTE_UNSUPPORTED,
    kAXErrorFailure, kAXErrorNoValue as AX_NO_VALUE, kAXErrorSuccess, kAXValueCGPointType,
    kAXValueCGSizeType, AXError, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};

/// WindowServer can publish a window's owner slightly after AX publishes the
/// root; retry ownership only (never the diff) this many times.
const OWNER_CATCH_UP_ATTEMPTS: usize = 3;
const OWNER_CATCH_UP_INTERVAL: Duration = Duration::from_millis(80);
/// ponytail: bounded map, oldest entry evicted; a session that acts on many
/// apps without reading them loses its oldest unreported note.
const MAX_PENDING: usize = 64;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RootKey {
    Native {
        window_id: u32,
        role: String,
        subrole: String,
    },
    Transient {
        parent_window_id: Option<u32>,
        role: String,
        subrole: String,
        title: String,
        frame: Option<[i64; 4]>,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Root {
    window_id: Option<u32>,
    title: String,
}

type RootSnapshot = HashMap<RootKey, Root>;


struct Pending {
    roots: RootSnapshot,
    recorded: Instant,
    /// A read already found nothing new against this baseline. The next action
    /// starts a fresh baseline instead of reporting stale windows later.
    read_since: bool,
}

type PendingKey = (String, i32);

static PENDING: LazyLock<Mutex<HashMap<PendingKey, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pending() -> std::sync::MutexGuard<'static, HashMap<PendingKey, Pending>> {
    PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn session_of(args: &Value) -> String {
    args.get("session")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn pid_of(args: &Value) -> Option<i32> {
    args.get("pid")
        .and_then(Value::as_i64)
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
}

/// Blocking. Record the app's roots before an action. An unreported baseline
/// is kept, so several actions before one read report everything since the
/// first of them.
fn record_before_action(session: &str, pid: i32) {
    let key = (session.to_owned(), pid);
    if pending().get(&key).is_some_and(|entry| !entry.read_since) {
        return;
    }
    // An unknown baseline must not make existing windows look new later.
    let Some(roots) = snapshot_roots(pid) else {
        pending().remove(&key);
        return;
    };
    let mut map = pending();
    if map.len() >= MAX_PENDING && !map.contains_key(&key) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, entry)| entry.recorded)
            .map(|(key, _)| key.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(
        key,
        Pending {
            roots,
            recorded: Instant::now(),
            read_since: false,
        },
    );
}

/// Blocking. Windows that appeared since the recorded baseline, reported once.
fn take_window_change(session: &str, pid: i32) -> Option<WindowChange> {
    let key = (session.to_owned(), pid);
    let before = pending().get(&key)?.roots.clone();
    // An unreadable tree is unknown: keep the baseline for a later read.
    let after = snapshot_roots(pid)?;
    let appeared = appeared_roots(&before, &after);
    let change = window_change(pid, &appeared);
    let mut map = pending();
    if change.is_some() {
        map.remove(&key);
    } else if let Some(entry) = map.get_mut(&key) {
        entry.read_since = true;
    }
    change
}

pub(crate) fn retire_session(session: &str) {
    pending().retain(|(owner, _), _| owner != session);
}

fn window_change(pid: i32, appeared: &[Root]) -> Option<WindowChange> {
    if appeared.is_empty() {
        return None;
    }
    let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
    let mut resolved =
        resolve_candidates(pid, &app_name, appeared, &crate::windows::all_windows());
    for _ in 0..OWNER_CATCH_UP_ATTEMPTS {
        if resolved.len() == appeared.len() {
            break;
        }
        std::thread::sleep(OWNER_CATCH_UP_INTERVAL);
        resolved = resolve_candidates(pid, &app_name, appeared, &crate::windows::all_windows());
    }
    change_from(resolved, appeared.len())
}

/// Several roots can share one address (a sheet reports its parent window),
/// so dedupe by address. Rebind only when every root resolved to an owner and
/// exactly one address remains; otherwise the caller picks.
fn change_from(candidates: Vec<SurfaceWindow>, root_count: usize) -> Option<WindowChange> {
    let complete = candidates.len() == root_count;
    let mut seen = HashSet::new();
    let new_windows: Vec<SurfaceWindow> = candidates
        .into_iter()
        .filter(|window| seen.insert((window.pid, window.window_id)))
        .collect();
    if new_windows.is_empty() {
        return None;
    }
    let rebind = (complete && new_windows.len() == 1).then(|| new_windows[0].clone());
    Some(WindowChange {
        new_windows,
        rebind,
    })
}

fn appeared_roots(before: &RootSnapshot, after: &RootSnapshot) -> Vec<Root> {
    after
        .iter()
        .filter(|(key, _)| !before.contains_key(*key))
        .map(|(_, root)| root.clone())
        .collect()
}

fn resolve_candidates(
    pid: i32,
    app_name: &str,
    roots: &[Root],
    windows: &[crate::windows::WindowInfo],
) -> Vec<SurfaceWindow> {
    roots
        .iter()
        .filter_map(|root| {
            let window_id = root.window_id?;
            let (owner_pid, owner_app_name) = surface_owner(windows, pid, window_id, app_name)?;
            Some(SurfaceWindow {
                pid: i64::from(owner_pid),
                window_id: u64::from(window_id),
                app_name: owner_app_name,
                title: root.title.clone(),
            })
        })
        .collect()
}

fn note_text(change: &WindowChange) -> String {
    let describe = |window: &SurfaceWindow| {
        format!(
            "\"{}\" ({}, pid {}, window_id {})",
            window.title, window.app_name, window.pid, window.window_id
        )
    };
    match &change.rebind {
        Some(window) => format!(
            "New since your last action: {}. Read it with get_window_state(pid: {}, window_id: {}).",
            describe(window),
            window.pid,
            window.window_id
        ),
        None => format!(
            "New since your last action: {}. Choose one to read with get_window_state.",
            change
                .new_windows
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Records a baseline and ends any lingering focus guard first.
    Action,
    /// Only ends lingering focus guards (the tool may activate an app on purpose).
    Activates,
    /// Attaches the pending note to a successful read.
    Read,
}

pub(crate) struct SurfaceNoted {
    inner: Box<dyn Tool>,
    role: Role,
}

pub(crate) fn action(inner: Box<dyn Tool>) -> Box<dyn Tool> {
    Box::new(SurfaceNoted { inner, role: Role::Action })
}

pub(crate) fn activates(inner: Box<dyn Tool>) -> Box<dyn Tool> {
    Box::new(SurfaceNoted { inner, role: Role::Activates })
}

pub(crate) fn read(inner: Box<dyn Tool>) -> Box<dyn Tool> {
    Box::new(SurfaceNoted { inner, role: Role::Read })
}

#[async_trait]
impl Tool for SurfaceNoted {
    fn def(&self) -> &ToolDef {
        self.inner.def()
    }

    fn has_independent_input_lane(&self, args: &Value) -> bool {
        self.inner.has_independent_input_lane(args)
    }

    async fn protected_resource_ownership(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> ProtectedResourceOwnership {
        self.inner.protected_resource_ownership(adapter_id, args).await
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        self.inner.protected_resource_scope(adapter_id, args).await
    }

    async fn validate_protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
        approved_scope: &Value,
    ) -> Result<(), String> {
        self.inner
            .validate_protected_resource_scope(adapter_id, args, approved_scope)
            .await
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if self.role != Role::Read {
            crate::window_change_detector::end_lingering_focus_guards();
        }
        let Some(pid) = pid_of(&args) else {
            return self.inner.invoke(args).await;
        };
        let session = session_of(&args);
        match self.role {
            Role::Activates => self.inner.invoke(args).await,
            Role::Action => {
                let recording_session = session.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    record_before_action(&recording_session, pid)
                })
                .await;
                self.inner.invoke(args).await
            }
            Role::Read => {
                let mut result = self.inner.invoke(args).await;
                if result.is_error == Some(true) {
                    return result;
                }
                let change = tokio::task::spawn_blocking(move || take_window_change(&session, pid))
                    .await
                    .ok()
                    .flatten();
                if let Some(change) = change {
                    attach(&mut result, &change);
                }
                result
            }
        }
    }
}

fn attach(result: &mut ToolResult, change: &WindowChange) {
    if let (Some(Value::Object(structured)), Ok(value)) =
        (result.structured_content.as_mut(), serde_json::to_value(change))
    {
        structured.insert("window_change".into(), value);
    }
    result.content.push(Content::text(note_text(change)));
}

struct OwnedAxElement(AXUIElementRef);

impl Drop for OwnedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SnapshotReadError {
    Attribute(AXError),
    TimeoutConfiguration(AXError),
    BudgetExhausted,
}

struct SnapshotReader {
    deadline: Instant,
}

impl SnapshotReader {
    fn checked_read<T>(
        &self,
        set_timeout: impl FnOnce(f32) -> AXError,
        read: impl FnOnce() -> Result<T, AXError>,
    ) -> Result<T, SnapshotReadError> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_millis(1) {
            return Err(SnapshotReadError::BudgetExhausted);
        }
        // Set each exact object before each message. Never set zero: AX treats
        // that as a request to restore its global default timeout.
        let timeout = remaining.min(Duration::from_millis(100)).as_secs_f32();
        let error = set_timeout(timeout);
        if error != kAXErrorSuccess {
            return Err(SnapshotReadError::TimeoutConfiguration(error));
        }
        let result = read().map_err(SnapshotReadError::Attribute)?;
        if Instant::now() >= self.deadline {
            return Err(SnapshotReadError::BudgetExhausted);
        }
        Ok(result)
    }

    unsafe fn read<T>(
        &self,
        element: AXUIElementRef,
        read: impl FnOnce() -> Result<T, AXError>,
    ) -> Result<T, SnapshotReadError> {
        self.checked_read(
            |timeout| AXUIElementSetMessagingTimeout(element, timeout),
            read,
        )
    }

    unsafe fn elements(
        &self,
        element: AXUIElementRef,
        attribute: &str,
    ) -> Result<Vec<OwnedAxElement>, SnapshotReadError> {
        let result = self.read(element, || {
            // Own every copied reference before the deadline can reject it.
            copy_element_array_attr_checked(element, attribute, 128)
                .map(|elements| elements.into_iter().map(OwnedAxElement).collect())
        });
        match result {
            Err(SnapshotReadError::Attribute(AX_NO_VALUE)) => Ok(Vec::new()),
            Err(SnapshotReadError::Attribute(AX_ATTRIBUTE_UNSUPPORTED))
                if attribute != "AXWindows" =>
            {
                Ok(Vec::new())
            }
            result => result,
        }
    }

    unsafe fn optional_string(
        &self,
        element: AXUIElementRef,
        attribute: &str,
    ) -> Result<String, SnapshotReadError> {
        match self.read(element, || copy_string_attr_checked(element, attribute)) {
            Err(SnapshotReadError::Attribute(AX_NO_VALUE | AX_ATTRIBUTE_UNSUPPORTED)) => {
                Ok(String::new())
            }
            result => result,
        }
    }

    unsafe fn frame(&self, element: AXUIElementRef) -> Result<Option<[i64; 4]>, SnapshotReadError> {
        let position = self.read(element, || {
            copy_geometry_attr_checked(element, "AXPosition", kAXValueCGPointType)
        });
        let position = match position {
            Err(SnapshotReadError::Attribute(AX_NO_VALUE | AX_ATTRIBUTE_UNSUPPORTED)) => {
                return Ok(None)
            }
            result => result?,
        };
        let size = match self.read(element, || {
            copy_geometry_attr_checked(element, "AXSize", kAXValueCGSizeType)
        }) {
            Err(SnapshotReadError::Attribute(AX_NO_VALUE | AX_ATTRIBUTE_UNSUPPORTED)) => {
                return Ok(None)
            }
            result => result?,
        };
        if size[0] < 1.0 || size[1] < 1.0 {
            return Ok(None);
        }
        Ok(Some(
            [position[0], position[1], size[0], size[1]].map(|value| value.round() as i64),
        ))
    }
}

fn snapshot_roots(pid: i32) -> Option<RootSnapshot> {
    let started = Instant::now();
    let result = (|| unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err(SnapshotReadError::Attribute(kAXErrorFailure));
        }
        let app = OwnedAxElement(app);
        let reader = SnapshotReader {
            deadline: Instant::now() + Duration::from_millis(250),
        };
        let windows = reader.elements(app.0, "AXWindows")?;
        let mut roots = HashMap::new();
        for window in windows {
            let parent_window_id = reader.read(window.0, || ax_get_window_id_checked(window.0))?;
            insert_root(&reader, &mut roots, window.0, parent_window_id)?;
            for attribute in ["AXSheets", "AXChildren"] {
                for child in reader.elements(window.0, attribute)? {
                    let role =
                        reader.read(child.0, || copy_string_attr_checked(child.0, "AXRole"))?;
                    if matches!(role.as_str(), "AXSheet" | "AXDialog" | "AXPopover") {
                        insert_root(&reader, &mut roots, child.0, parent_window_id)?;
                    }
                }
            }
        }
        Ok(roots)
    })();
    if let Err(ref error) = result {
        tracing::debug!(
            pid,
            ?error,
            "AX root snapshot unavailable; no surface delta will be inferred"
        );
    }
    tracing::debug!(pid, complete = result.is_ok(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        window_ids = ?result.as_ref().ok().map(|roots| roots.values()
            .filter_map(|root| root.window_id).collect::<Vec<_>>()),
        "surface observation AX snapshot complete");
    result.ok()
}

unsafe fn insert_root(
    reader: &SnapshotReader,
    roots: &mut RootSnapshot,
    element: AXUIElementRef,
    parent_window_id: Option<u32>,
) -> Result<(), SnapshotReadError> {
    let role = reader.read(element, || copy_string_attr_checked(element, "AXRole"))?;
    let subrole = reader.optional_string(element, "AXSubrole")?;
    let title = reader.optional_string(element, "AXTitle")?;
    let own_window_id = reader.read(element, || ax_get_window_id_checked(element))?;
    let effective_window_id = own_window_id.or(parent_window_id);
    let key = match own_window_id {
        Some(window_id) => RootKey::Native {
            window_id,
            role,
            subrole,
        },
        None => RootKey::Transient {
            parent_window_id,
            role,
            subrole,
            title: title.clone(),
            frame: reader.frame(element)?,
        },
    };
    roots.insert(
        key,
        Root {
            window_id: effective_window_id,
            title,
        },
    );
    Ok(())
}

fn surface_owner(
    windows: &[crate::windows::WindowInfo],
    target_pid: i32,
    window_id: u32,
    target_app_name: &str,
) -> Option<(i32, String)> {
    // Keep the AX root's exact WindowServer identity. AppKit can publish an
    // addressable same-process proxy beside an AX-empty XPC duplicate; replacing
    // that proxy by title or geometry would make the rebind less usable.
    match crate::windows::resolve_window_owner_in(windows, target_pid, window_id) {
        crate::windows::WindowOwner::SamePid => Some((target_pid, target_app_name.to_owned())),
        crate::windows::WindowOwner::ForeignPid {
            owner_pid,
            owner_app_name,
        } => Some((owner_pid, owner_app_name)),
        crate::windows::WindowOwner::Unknown => None,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn root(window_id: u32, title: &str) -> Root {
        Root {
            window_id: Some(window_id),
            title: title.into(),
        }
    }

    fn native(window_id: u32, role: &str) -> RootKey {
        RootKey::Native {
            window_id,
            role: role.into(),
            subrole: String::new(),
        }
    }

    fn window(window_id: u32, pid: i32, app_name: &str) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: app_name.into(),
            title: "Open".into(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 640.0,
                height: 480.0,
            },
            layer: 0,
            z_index: 1,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    fn surface(pid: i64, window_id: u64) -> SurfaceWindow {
        SurfaceWindow {
            pid,
            window_id,
            app_name: "TextEdit".into(),
            title: "Open".into(),
        }
    }

    #[test]
    fn root_diff_ignores_metadata_changes_and_reports_an_appeared_modal() {
        let parent = native(7, "AXWindow");
        let before = RootSnapshot::from([(parent.clone(), root(7, "Draft"))]);
        let mut after = RootSnapshot::from([(parent, root(7, "Draft - Edited"))]);
        assert!(appeared_roots(&before, &after).is_empty());
        after.insert(native(8, "AXSheet"), root(8, "Open"));
        assert_eq!(appeared_roots(&before, &after), vec![root(8, "Open")]);
    }

    #[test]
    fn one_new_window_is_a_rebind_target() {
        let change = change_from(vec![surface(42, 8)], 1).expect("change");
        assert_eq!(change.rebind, Some(surface(42, 8)));
        assert_eq!(change.new_windows, vec![surface(42, 8)]);
    }

    #[test]
    fn roots_sharing_one_address_are_one_rebind_target() {
        let change = change_from(vec![surface(42, 8), surface(42, 8)], 2).expect("change");
        assert_eq!(change.rebind, Some(surface(42, 8)));
    }

    #[test]
    fn several_new_windows_are_listed_without_a_guess() {
        let change = change_from(vec![surface(42, 8), surface(42, 9)], 2).expect("change");
        assert_eq!(change.rebind, None);
        assert_eq!(change.new_windows.len(), 2);
    }

    #[test]
    fn an_unresolved_owner_blocks_the_rebind() {
        let change = change_from(vec![surface(42, 8)], 2).expect("change");
        assert_eq!(change.rebind, None, "one of two roots had no known owner");
    }

    #[test]
    fn nothing_new_is_no_change() {
        assert_eq!(change_from(Vec::new(), 0), None);
    }

    #[test]
    fn owner_resolution_can_follow_an_ax_root_without_guessing() {
        let roots = vec![root(8, "Open")];
        assert!(resolve_candidates(42, "TextEdit", &roots, &[]).is_empty());
        let windows = vec![window(8, 99, "Open and Save Panel Service")];
        let candidates = resolve_candidates(42, "TextEdit", &roots, &windows);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pid, 99);
    }

    #[test]
    fn owner_resolution_preserves_an_addressable_same_pid_proxy() {
        let roots = vec![root(8, "Open")];
        let windows = vec![
            window(8, 42, "TextEdit"),
            window(9, 99, "Open and Save Panel Service"),
        ];
        let candidates = resolve_candidates(42, "TextEdit", &roots, &windows);
        assert_eq!(candidates, vec![surface(42, 8)]);
    }

    #[test]
    fn note_text_names_the_rebind_call() {
        let change = change_from(vec![surface(42, 8)], 1).unwrap();
        assert!(note_text(&change).contains("get_window_state(pid: 42, window_id: 8)"));
    }

    #[test]
    fn attach_adds_typed_change_and_a_text_note() {
        let mut result = ToolResult::text("state").with_structured(serde_json::json!({"pid": 42}));
        attach(&mut result, &change_from(vec![surface(42, 8)], 1).unwrap());
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["window_change"]["rebind"]["window_id"], 8);
        assert_eq!(result.content.len(), 2);
    }

    #[test]
    fn exhausted_snapshot_budget_starts_no_more_ax_messages() {
        let reader = SnapshotReader {
            deadline: Instant::now() - Duration::from_secs(1),
        };
        let calls = std::cell::Cell::new(0);
        let result = reader.checked_read(
            |_| {
                calls.set(calls.get() + 1);
                kAXErrorSuccess
            },
            || {
                calls.set(calls.get() + 1);
                Ok(7)
            },
        );
        assert_eq!(result, Err(SnapshotReadError::BudgetExhausted));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn every_ax_read_gets_a_positive_bounded_timeout_before_dispatch() {
        let reader = SnapshotReader {
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let configured = std::cell::Cell::new(false);
        let result = reader.checked_read(
            |timeout| {
                assert!(timeout > 0.0 && timeout <= 0.1);
                configured.set(true);
                kAXErrorSuccess
            },
            || {
                assert!(configured.get());
                Ok(7)
            },
        );
        assert_eq!(result, Ok(7));
    }
}
