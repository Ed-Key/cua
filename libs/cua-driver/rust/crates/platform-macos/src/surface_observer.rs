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
    ax_get_window_id_checked, copy_element_array_attr_checked, copy_string_attr_checked, kAXErrorAttributeUnsupported as AX_ATTRIBUTE_UNSUPPORTED,
    kAXErrorFailure, kAXErrorNoValue as AX_NO_VALUE, kAXErrorSuccess, AXError, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};

/// WindowServer can publish a window's owner slightly after AX publishes the
/// root; retry ownership only (never the diff) this many times.
const OWNER_CATCH_UP_ATTEMPTS: usize = 3;
const OWNER_CATCH_UP_INTERVAL: Duration = Duration::from_millis(80);
/// ponytail: bounded map, oldest entry evicted; a session that acts on many
/// apps without reading them loses its oldest unreported note.
const MAX_PENDING: usize = 64;
/// Reads after which one root whose owner never resolves stops blocking.
const MAX_UNRESOLVED_READS: u32 = 3;

/// Unique across every baseline ever created, so a read that started
/// against a baseline that was since removed and recreated cannot match it.
static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// An AX element as an identity: retained, hashed with `CFHash`, compared
/// with `CFEqual` (equal hashes alone do not prove the same element).
struct AxIdentity(AXUIElementRef);

impl AxIdentity {
    /// # Safety
    /// `element` must be a live `AXUIElementRef`.
    unsafe fn retain(element: AXUIElementRef) -> Self {
        core_foundation::base::CFRetain(element as CFTypeRef);
        Self(element)
    }
}

impl Clone for AxIdentity {
    fn clone(&self) -> Self {
        unsafe { Self::retain(self.0) }
    }
}

impl Drop for AxIdentity {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

impl PartialEq for AxIdentity {
    fn eq(&self, other: &Self) -> bool {
        unsafe { core_foundation::base::CFEqual(self.0 as CFTypeRef, other.0 as CFTypeRef) != 0 }
    }
}

impl Eq for AxIdentity {}

impl std::hash::Hash for AxIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe { core_foundation::base::CFHash(self.0 as CFTypeRef) }.hash(state);
    }
}

impl std::fmt::Debug for AxIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AxIdentity({:p})", self.0)
    }
}

// SAFETY: CF retain/release/equal/hash are thread-safe, and the element is
// only used through those calls.
unsafe impl Send for AxIdentity {}
unsafe impl Sync for AxIdentity {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RootKey {
    Native {
        window_id: u32,
        role: String,
        subrole: String,
    },
    /// No window id of its own. Identified by the AX element itself, so a
    /// retitled or moved existing surface is not mistaken for a new one.
    Transient {
        parent_window_id: Option<u32>,
        role: String,
        subrole: String,
        element: AxIdentity,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Root {
    /// The address `get_window_state` can read: a top-level window's own id,
    /// or the parent window for a sheet, dialog or popover inside it.
    window_id: Option<u32>,
    title: String,
}

type RootSnapshot = HashMap<RootKey, Root>;


struct Pending {
    /// Roots already known to the agent: the baseline plus anything reported.
    roots: RootSnapshot,
    recorded: Instant,
    /// Bumped whenever a read changes this entry. A read only reports if the
    /// generation it started from is still current, so two concurrent reads
    /// cannot both report the same window.
    generation: u64,
    /// A read already found nothing new against this baseline. The next action
    /// starts a fresh baseline instead of reporting stale windows later.
    read_since: bool,
    /// Per root: reads so far that found its owner still unknown.
    unresolved_reads: HashMap<RootKey, u32>,
}

type PendingKey = (String, i32);

static PENDING: LazyLock<Mutex<HashMap<PendingKey, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pending() -> std::sync::MutexGuard<'static, HashMap<PendingKey, Pending>> {
    PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The runtime session id the element cache also uses; the public label is
/// only a fallback (implicit sessions carry `_session_id` alone).
fn session_of(args: &Value) -> String {
    ["_session_id", "session"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
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
    let observed = match pending().get(&key) {
        Some(entry) if !entry.read_since => return,
        Some(entry) => Some(entry.generation),
        None => None,
    };
    let roots = snapshot_roots(pid);
    let mut map = pending();
    // Replace only what was seen before the snapshot started. A read that
    // finished meanwhile (for example one that kept an unresolved root
    // pending) or another action's baseline wins.
    let current = map.get(&key).map(|entry| (entry.generation, entry.read_since));
    let unchanged = match (observed, current) {
        (None, None) => true,
        (Some(seen), Some((generation, true))) => seen == generation,
        _ => false,
    };
    if !unchanged {
        return;
    }
    // An unknown baseline must not make existing windows look new later.
    let Some(roots) = roots else {
        map.remove(&key);
        return;
    };
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
            generation: next_generation(),
            read_since: false,
            unresolved_reads: HashMap::new(),
        },
    );
}

/// One appeared root after ownership resolution.
enum Outcome {
    /// Addressable window with a known owner: reported now.
    Reported(SurfaceWindow),
    /// Has an address but WindowServer does not know its owner yet: kept for
    /// a later read, and blocks any rebind now.
    Unresolved,
    /// No address at all: nothing to report, ever.
    Unaddressable,
}

/// Blocking. Windows that appeared since the recorded baseline, each reported
/// once.
fn take_window_change(session: &str, pid: i32) -> Option<WindowChange> {
    let key = (session.to_owned(), pid);
    let (before, generation) = {
        let map = pending();
        let entry = map.get(&key)?;
        (entry.roots.clone(), entry.generation)
    };
    // An unreadable tree is unknown: keep the baseline for a later read.
    let after = snapshot_roots(pid)?;
    let appeared = appeared_roots(&before, &after);
    let outcomes = resolve_outcomes(pid, &appeared);
    finish_take(&key, generation, appeared, outcomes)
}

/// Apply one read's findings to the pending entry and build its report.
fn finish_take(
    key: &PendingKey,
    generation: u64,
    appeared: Vec<(RootKey, Root)>,
    outcomes: Vec<Outcome>,
) -> Option<WindowChange> {
    let pid = key.1;
    let mut map = pending();
    let entry = map.get_mut(key)?;
    if entry.generation != generation {
        return None; // another read or a new baseline got here first
    }
    entry.generation = next_generation();
    let mut reported = Vec::new();
    let mut still_unresolved = HashMap::new();
    for ((root_key, root), outcome) in appeared.into_iter().zip(outcomes) {
        match outcome {
            Outcome::Reported(window) => {
                entry.roots.insert(root_key, root);
                reported.push(window);
            }
            Outcome::Unresolved => {
                // Each root has its own budget; one that never resolves
                // stops blocking without taking newer roots with it.
                let tries = entry.unresolved_reads.get(&root_key).copied().unwrap_or(0) + 1;
                if tries >= MAX_UNRESOLVED_READS {
                    entry.roots.insert(root_key, root);
                } else {
                    still_unresolved.insert(root_key, tries);
                }
            }
            Outcome::Unaddressable => {
                entry.roots.insert(root_key, root);
            }
        }
    }
    let unresolved = !still_unresolved.is_empty();
    entry.unresolved_reads = still_unresolved;
    if unresolved {
        // Keep this baseline across the next action so the root can still
        // be reported once its owner resolves.
        entry.read_since = false;
    } else if reported.is_empty() {
        entry.read_since = true;
    } else {
        map.remove(key);
    }
    change_from(reported, unresolved, pid)
}

pub(crate) fn retire_session(session: &str) {
    pending().retain(|(owner, _), _| owner != session);
}

fn resolve_outcomes(pid: i32, appeared: &[(RootKey, Root)]) -> Vec<Outcome> {
    if appeared.is_empty() {
        return Vec::new();
    }
    let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
    let mut outcomes = Vec::new();
    for attempt in 0..=OWNER_CATCH_UP_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(OWNER_CATCH_UP_INTERVAL);
        }
        let windows = crate::windows::all_windows_any_layer();
        outcomes = appeared
            .iter()
            .map(|(_, root)| outcome_for(pid, &app_name, root, &windows))
            .collect();
        if !outcomes.iter().any(|outcome| matches!(outcome, Outcome::Unresolved)) {
            break;
        }
    }
    outcomes
}

fn outcome_for(
    pid: i32,
    app_name: &str,
    root: &Root,
    windows: &[crate::windows::WindowInfo],
) -> Outcome {
    let Some(window_id) = root.window_id else {
        return Outcome::Unaddressable;
    };
    match surface_owner(windows, pid, window_id, app_name) {
        Some((owner_pid, owner_app_name)) => Outcome::Reported(SurfaceWindow {
            pid: i64::from(owner_pid),
            window_id: u64::from(window_id),
            app_name: owner_app_name,
            title: root.title.clone(),
        }),
        None => Outcome::Unresolved,
    }
}

/// Several roots can share one address (a sheet reports its parent window),
/// so dedupe by address. Rebind only when nothing is unresolved, exactly one
/// address remains, and the acted-on app owns it: a window owned by another
/// process is listed but not offered as a readable target.
fn change_from(candidates: Vec<SurfaceWindow>, unresolved: bool, pid: i32) -> Option<WindowChange> {
    let mut seen = HashSet::new();
    let new_windows: Vec<SurfaceWindow> = candidates
        .into_iter()
        .filter(|window| seen.insert((window.pid, window.window_id)))
        .collect();
    if new_windows.is_empty() {
        return None;
    }
    let rebind = (!unresolved && new_windows.len() == 1 && new_windows[0].pid == i64::from(pid))
        .then(|| new_windows[0].clone());
    Some(WindowChange {
        new_windows,
        rebind,
    })
}

fn appeared_roots(before: &RootSnapshot, after: &RootSnapshot) -> Vec<(RootKey, Root)> {
    after
        .iter()
        .filter(|(key, _)| !before.contains_key(*key))
        .map(|(key, root)| (key.clone(), root.clone()))
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
            insert_root(&reader, &mut roots, window.0, parent_window_id, true)?;
            for attribute in ["AXSheets", "AXChildren"] {
                for child in reader.elements(window.0, attribute)? {
                    let role =
                        reader.read(child.0, || copy_string_attr_checked(child.0, "AXRole"))?;
                    if matches!(role.as_str(), "AXSheet" | "AXDialog" | "AXPopover") {
                        insert_root(&reader, &mut roots, child.0, parent_window_id, false)?;
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
    top_level: bool,
) -> Result<(), SnapshotReadError> {
    let role = reader.read(element, || copy_string_attr_checked(element, "AXRole"))?;
    let subrole = reader.optional_string(element, "AXSubrole")?;
    let title = reader.optional_string(element, "AXTitle")?;
    let own_window_id = reader.read(element, || ax_get_window_id_checked(element))?;
    // get_window_state reads top-level AX windows; a child surface is read
    // only through its parent, even when WindowServer gives it its own id.
    let effective_window_id = if top_level {
        own_window_id
    } else {
        parent_window_id
    };
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
            element: AxIdentity::retain(element),
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

    /// Seed a pending baseline under a key unique to one test.
    fn seed(session: &str, pid: i32) -> (PendingKey, u64) {
        let key = (session.to_owned(), pid);
        pending().insert(
            key.clone(),
            Pending {
                roots: RootSnapshot::new(),
                recorded: Instant::now(),
                generation: 7,
                read_since: false,
                unresolved_reads: HashMap::new(),
            },
        );
        (key, 7)
    }

    #[test]
    fn root_diff_ignores_metadata_changes_and_reports_an_appeared_modal() {
        let parent = native(7, "AXWindow");
        let before = RootSnapshot::from([(parent.clone(), root(7, "Draft"))]);
        let mut after = RootSnapshot::from([(parent, root(7, "Draft - Edited"))]);
        assert!(appeared_roots(&before, &after).is_empty());
        after.insert(native(8, "AXSheet"), root(8, "Open"));
        assert_eq!(
            appeared_roots(&before, &after),
            vec![(native(8, "AXSheet"), root(8, "Open"))]
        );
    }

    #[test]
    fn one_new_window_of_the_app_is_a_rebind_target() {
        let change = change_from(vec![surface(42, 8)], false, 42).expect("change");
        assert_eq!(change.rebind, Some(surface(42, 8)));
    }

    #[test]
    fn roots_sharing_one_address_are_one_rebind_target() {
        let change = change_from(vec![surface(42, 8), surface(42, 8)], false, 42).unwrap();
        assert_eq!(change.new_windows.len(), 1);
        assert_eq!(change.rebind, Some(surface(42, 8)));
    }

    #[test]
    fn several_new_windows_are_listed_without_a_guess() {
        let change = change_from(vec![surface(42, 8), surface(42, 9)], false, 42).unwrap();
        assert_eq!(change.rebind, None);
        assert_eq!(change.new_windows.len(), 2);
    }

    #[test]
    fn an_unresolved_owner_blocks_the_rebind() {
        let change = change_from(vec![surface(42, 8)], true, 42).unwrap();
        assert_eq!(change.rebind, None);
    }

    #[test]
    fn a_window_owned_by_another_process_is_listed_not_rebound() {
        let change = change_from(vec![surface(99, 8)], false, 42).unwrap();
        assert_eq!(change.rebind, None);
        assert_eq!(change.new_windows, vec![surface(99, 8)]);
    }

    #[test]
    fn owner_lookup_follows_an_ax_root_without_guessing() {
        let open = root(8, "Open");
        assert!(matches!(outcome_for(42, "TextEdit", &open, &[]), Outcome::Unresolved));
        let foreign = [window(8, 99, "Open and Save Panel Service")];
        assert!(matches!(
            outcome_for(42, "TextEdit", &open, &foreign),
            Outcome::Reported(SurfaceWindow { pid: 99, window_id: 8, .. })
        ));
        let proxy = [window(8, 42, "TextEdit"), window(9, 99, "Open and Save Panel Service")];
        assert!(matches!(
            outcome_for(42, "TextEdit", &open, &proxy),
            Outcome::Reported(SurfaceWindow { pid: 42, window_id: 8, .. })
        ));
        let no_address = Root { window_id: None, title: String::new() };
        assert!(matches!(outcome_for(42, "TextEdit", &no_address, &[]), Outcome::Unaddressable));
    }

    #[test]
    fn a_report_is_delivered_once_and_ends_the_baseline() {
        let (key, generation) = seed("report-once", 4101);
        let appeared = vec![(native(8, "AXWindow"), root(8, "Open"))];
        let change = finish_take(&key, generation, appeared.clone(), vec![Outcome::Reported(surface(4101, 8))]);
        assert_eq!(change.unwrap().rebind, Some(surface(4101, 8)));
        assert!(!pending().contains_key(&key), "reported baseline is consumed");
        assert_eq!(
            finish_take(&key, generation, appeared, vec![Outcome::Reported(surface(4101, 8))]),
            None
        );
    }

    #[test]
    fn a_read_that_lost_the_race_reports_nothing() {
        let (key, generation) = seed("race", 4102);
        let appeared = vec![(native(8, "AXWindow"), root(8, "Open"))];
        assert!(finish_take(&key, generation, appeared.clone(), vec![Outcome::Reported(surface(4102, 8))]).is_some());
        let (key, generation) = seed("race-2", 4102);
        pending().get_mut(&key).unwrap().generation += 1; // a concurrent read won
        assert_eq!(
            finish_take(&key, generation, appeared, vec![Outcome::Reported(surface(4102, 8))]),
            None
        );
    }

    #[test]
    fn unresolved_windows_stay_pending_for_a_later_read() {
        let (key, generation) = seed("partial", 4103);
        let appeared = vec![
            (native(8, "AXWindow"), root(8, "Open")),
            (native(9, "AXWindow"), root(9, "Other")),
        ];
        let change = finish_take(
            &key,
            generation,
            appeared,
            vec![Outcome::Reported(surface(4103, 8)), Outcome::Unresolved],
        )
        .unwrap();
        assert_eq!(change.new_windows, vec![surface(4103, 8)]);
        assert_eq!(change.rebind, None, "an unresolved sibling blocks the rebind");
        let map = pending();
        let entry = map.get(&key).expect("unresolved root keeps the baseline");
        assert!(entry.roots.contains_key(&native(8, "AXWindow")), "reported root is now known");
        assert!(!entry.roots.contains_key(&native(9, "AXWindow")), "unresolved root can still be reported");
    }

    #[test]
    fn an_unresolved_root_clears_an_earlier_refresh_mark() {
        let (key, generation) = seed("unresolved-after-empty", 4105);
        pending().get_mut(&key).unwrap().read_since = true;
        let appeared = vec![(native(9, "AXWindow"), root(9, "Late"))];
        assert_eq!(finish_take(&key, generation, appeared, vec![Outcome::Unresolved]), None);
        assert!(!pending().get(&key).unwrap().read_since, "the next action must keep this baseline");
    }

    #[test]
    fn a_root_that_never_resolves_stops_blocking() {
        let (key, _) = seed("never-resolves", 4106);
        let appeared = vec![(native(9, "AXWindow"), root(9, "Ghost"))];
        for _ in 0..MAX_UNRESOLVED_READS {
            let generation = pending().get(&key).unwrap().generation;
            finish_take(&key, generation, appeared.clone(), vec![Outcome::Unresolved]);
        }
        let map = pending();
        let entry = map.get(&key).unwrap();
        assert!(entry.roots.contains_key(&native(9, "AXWindow")), "given up and treated as known");
        assert!(entry.read_since);
    }

    #[test]
    fn each_root_has_its_own_retry_budget() {
        let (key, _) = seed("per-root-budget", 4107);
        let old = (native(9, "AXWindow"), root(9, "Ghost"));
        let new = (native(10, "AXWindow"), root(10, "Late"));
        for _ in 0..MAX_UNRESOLVED_READS - 1 {
            let generation = pending().get(&key).unwrap().generation;
            finish_take(&key, generation, vec![old.clone()], vec![Outcome::Unresolved]);
        }
        let generation = pending().get(&key).unwrap().generation;
        finish_take(
            &key,
            generation,
            vec![old.clone(), new.clone()],
            vec![Outcome::Unresolved, Outcome::Unresolved],
        );
        let map = pending();
        let entry = map.get(&key).unwrap();
        assert!(entry.roots.contains_key(&old.0), "exhausted root is given up");
        assert!(!entry.roots.contains_key(&new.0), "a newer root keeps its own retries");
        assert_eq!(entry.unresolved_reads.get(&new.0), Some(&1));
    }

    #[test]
    fn generations_are_unique_across_recreated_baselines() {
        let first = next_generation();
        assert_ne!(first, next_generation());
    }

    #[test]
    fn nothing_new_marks_the_baseline_for_refresh() {
        let (key, generation) = seed("nothing", 4104);
        assert_eq!(finish_take(&key, generation, Vec::new(), Vec::new()), None);
        assert!(pending().get(&key).unwrap().read_since);
    }

    #[test]
    fn implicit_sessions_are_keyed_by_runtime_session_id() {
        let args = serde_json::json!({"_session_id": "runtime/a", "session": "label"});
        assert_eq!(session_of(&args), "runtime/a");
        assert_eq!(session_of(&serde_json::json!({"session": "label"})), "label");
    }

    #[test]
    fn note_text_names_the_rebind_call() {
        let change = change_from(vec![surface(42, 8)], false, 42).unwrap();
        assert!(note_text(&change).contains("get_window_state(pid: 42, window_id: 8)"));
    }

    #[test]
    fn attach_adds_typed_change_and_a_text_note() {
        let mut result = ToolResult::text("state").with_structured(serde_json::json!({"pid": 42}));
        attach(&mut result, &change_from(vec![surface(42, 8)], false, 42).unwrap());
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
