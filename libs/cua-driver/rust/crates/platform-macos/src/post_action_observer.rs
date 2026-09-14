//! Target-scoped post-action accessibility-root observation.
//!
//! The decorator in this module is the only action topology producer on
//! macOS. It snapshots the addressed process, invokes the actuator, then
//! attaches one typed delta to `ToolResult`. Native notifications and global
//! window lists are not used as causality evidence.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::action_record::{
    resolve_surface_delta, ActionSurfaceDelta, ActionSurfaceTarget,
};
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::tool::{ProtectedResourceOwnership, Tool, ToolDef};
use serde_json::Value;

use crate::ax::bindings::{
    ax_get_window_id_checked, copy_element_array_attr_checked, copy_geometry_attr_checked,
    copy_string_attr_checked, kAXErrorAttributeUnsupported as AX_ATTRIBUTE_UNSUPPORTED,
    kAXErrorFailure, kAXErrorNoValue as AX_NO_VALUE, kAXErrorSuccess, kAXValueCGPointType,
    kAXValueCGSizeType, AXError, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};

const OBSERVATION_TIMEOUT: Duration = Duration::from_millis(400);
const POLL_INTERVAL: Duration = Duration::from_millis(30);
const CATCH_UP_INTERVAL: Duration = Duration::from_millis(80);
const CATCH_UP_ATTEMPTS: usize = 3;

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

#[derive(Default)]
struct RootObservation {
    roots: Option<RootSnapshot>,
    window_signature: HashSet<u32>,
}

pub struct ObservedActionTool {
    inner: Box<dyn Tool>,
}

impl ObservedActionTool {
    pub fn new(inner: Box<dyn Tool>) -> Self {
        Self { inner }
    }

    // Keep the ordering boundary independent of macOS queries so lifecycle
    // behavior can be exercised without installing observers on a desktop.
    async fn invoke_observed<B, Start, Finish, End>(
        &self,
        pid: i32,
        args: Value,
        start: Start,
        finish: Finish,
    ) -> ToolResult
    where
        Start: std::future::Future<Output = B>,
        Finish: FnOnce(B, ToolResult) -> End,
        End: std::future::Future<Output = ToolResult>,
    {
        crate::background_mutation::with_observation_lease(pid, async {
            let before = start.await;
            let result = self.inner.invoke(args).await;
            finish(before, result).await
        })
        .await
    }
}

#[async_trait]
impl Tool for ObservedActionTool {
    fn def(&self) -> &ToolDef {
        self.inner.def()
    }

    async fn protected_resource_ownership(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> ProtectedResourceOwnership {
        self.inner
            .protected_resource_ownership(adapter_id, args)
            .await
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
        let Some(pid) = args
            .get("pid")
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
        else {
            return self.inner.invoke(args).await;
        };
        let suppress_cross_app = args
            .get("delivery_mode")
            .and_then(Value::as_str)
            .is_none_or(|mode| !mode.eq_ignore_ascii_case("foreground"));
        self.invoke_observed(
            pid,
            args,
            async {
                let prior_front = crate::apps::frontmost_pid();
                let suppression = prior_front
                    .filter(|_| suppress_cross_app)
                    .map(|restore_to| {
                        std::sync::Arc::new(crate::focus_steal::begin_suppression_allowing(
                            pid,
                            restore_to,
                            "ObservedActionTool",
                        ))
                    });
                let read_suppression = suppression.clone();
                let before = crate::background_mutation::observe_blocking(move || {
                    let _suppression = read_suppression;
                    begin_observation(pid)
                })
                .await
                .unwrap_or_default();
                (prior_front, before, suppression)
            },
            |(prior_front, before, _suppression), mut result| async move {
                let delta = crate::background_mutation::observe_blocking(move || {
                    // The blocking read can outlive cancellation of invoke().
                    let _suppression = _suppression;
                    observe_delta(pid, prior_front, before)
                })
                .await
                .ok()
                .flatten();
                if let Some(delta) = delta {
                    result.surface_delta = Some(delta);
                }
                result
            },
        )
        .await
    }
}

fn observe_delta(
    pid: i32,
    prior_front: Option<i32>,
    before: RootObservation,
) -> Option<ActionSurfaceDelta> {
    // Without a complete baseline, an existing root cannot be called new.
    before.roots.as_ref()?;
    let deadline = Instant::now() + OBSERVATION_TIMEOUT;
    let signaled = loop {
        if foreground_changed(prior_front)
            || target_window_signature(pid) != before.window_signature
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let mut after = snapshot_roots(pid);
    after.as_ref()?;
    let mut appeared = appeared_roots(&before.roots, &after);
    if appeared.is_empty() && signaled {
        for _ in 0..CATCH_UP_ATTEMPTS {
            std::thread::sleep(CATCH_UP_INTERVAL);
            after = snapshot_roots(pid);
            after.as_ref()?;
            appeared = appeared_roots(&before.roots, &after);
            if !appeared.is_empty() {
                break;
            }
        }
    }
    resolve_appeared_roots(pid, &appeared, foreground_changed(prior_front))
}

fn foreground_changed(prior_front: Option<i32>) -> bool {
    matches!(
        (prior_front, crate::apps::frontmost_pid()),
        (Some(before), Some(after)) if before != after
    )
}

fn appeared_roots(before: &Option<RootSnapshot>, after: &Option<RootSnapshot>) -> Vec<Root> {
    let (Some(before), Some(after)) = (before, after) else {
        return Vec::new();
    };
    after
        .iter()
        .filter(|(key, _)| !before.contains_key(*key))
        .map(|(_, root)| root.clone())
        .collect()
}

/// Resolve ownership after AX has established which roots appeared. WindowServer
/// may lag AX, so unresolved roots retry ownership only and block an exact target.
fn resolve_appeared_roots(
    pid: i32,
    roots: &[Root],
    foreground_changed: bool,
) -> Option<ActionSurfaceDelta> {
    if roots.is_empty() {
        return None;
    }
    let app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
    let mut resolved = resolve_candidates(pid, &app_name, roots, &crate::windows::all_windows());
    for _ in 0..CATCH_UP_ATTEMPTS {
        if resolved.len() == roots.len() {
            break;
        }
        std::thread::sleep(CATCH_UP_INTERVAL);
        resolved = resolve_candidates(pid, &app_name, roots, &crate::windows::all_windows());
    }
    let incomplete = resolved.len() != roots.len();
    let mut delta = resolve_surface_delta(resolved, foreground_changed)?;
    if incomplete {
        delta.rebind = None;
    }
    Some(delta)
}

fn resolve_candidates(
    pid: i32,
    app_name: &str,
    roots: &[Root],
    windows: &[crate::windows::WindowInfo],
) -> Vec<ActionSurfaceTarget> {
    roots
        .iter()
        .filter_map(|root| {
            let window_id = root.window_id?;
            let (owner_pid, owner_app_name) = surface_owner(windows, pid, window_id, app_name)?;
            Some(ActionSurfaceTarget {
                pid: i64::from(owner_pid),
                window_id: u64::from(window_id),
                app_name: owner_app_name,
                title: root.title.clone(),
            })
        })
        .collect()
}

fn begin_observation(pid: i32) -> RootObservation {
    RootObservation {
        roots: snapshot_roots(pid),
        window_signature: target_window_signature(pid),
    }
}

fn target_window_signature(pid: i32) -> HashSet<u32> {
    crate::windows::visible_windows()
        .into_iter()
        .filter(|window| window.pid == pid)
        .map(|window| window.window_id)
        .collect()
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
    fn failed_timeout_configuration_does_not_start_an_unbounded_read() {
        let reader = SnapshotReader {
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let read_started = std::cell::Cell::new(false);
        let result = reader.checked_read(
            |_| -25202,
            || {
                read_started.set(true);
                Ok(7)
            },
        );
        assert_eq!(result, Err(SnapshotReadError::TimeoutConfiguration(-25202)));
        assert!(!read_started.get());
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

    #[test]
    fn complete_empty_baseline_can_still_report_the_first_window() {
        let after = RootSnapshot::from([(
            RootKey::Native {
                window_id: 7,
                role: "AXWindow".into(),
                subrole: String::new(),
            },
            root(7, "First window"),
        )]);
        assert_eq!(
            appeared_roots(&Some(RootSnapshot::new()), &Some(after)),
            vec![root(7, "First window")]
        );
    }

    #[test]
    fn incomplete_after_read_does_not_establish_a_surface_delta() {
        assert!(appeared_roots(&Some(RootSnapshot::new()), &None).is_empty());
    }

    #[test]
    fn failed_before_read_cannot_make_existing_windows_appear_new() {
        let before = RootObservation::default();
        let after = RootSnapshot::from([(
            RootKey::Native {
                window_id: 7,
                role: "AXWindow".into(),
                subrole: "AXStandardWindow".into(),
            },
            root(7, "Existing document"),
        )]);
        assert!(
            appeared_roots(&before.roots, &Some(after)).is_empty(),
            "a failed baseline read is unknown, not proof that every later window is new"
        );
    }

    struct MutationProbe;

    #[async_trait]
    impl Tool for MutationProbe {
        fn def(&self) -> &ToolDef {
            static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
            DEF.get_or_init(|| ToolDef {
                name: "click".into(),
                description: "Observation lifecycle probe".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: false,
            })
        }

        async fn invoke(&self, args: Value) -> ToolResult {
            let pid = args["pid"].as_i64().unwrap() as i32;
            // Observation ownership must not impersonate focus_by_pixel's
            // already-admitted nested call or skip fresh target admission.
            assert!(!crate::background_mutation::held_by_current_task(pid));
            let _lease = crate::tools::acquire_background_mutation(pid).await;
            ToolResult::text("delivered")
        }
    }

    #[tokio::test]
    async fn observation_waits_for_prior_same_pid_mutation_before_snapshot() {
        const PID: i32 = 93101;
        let lease = crate::background_mutation::acquire(PID).await;
        let (started_tx, mut started_rx) = tokio::sync::oneshot::channel();
        let action = tokio::spawn(async move {
            ObservedActionTool::new(Box::new(MutationProbe))
                .invoke_observed(
                    PID,
                    serde_json::json!({"pid": PID}),
                    async {
                        started_tx.send(()).unwrap();
                    },
                    |(), result| async { result },
                )
                .await
        });
        let queued = tokio::time::timeout(Duration::from_millis(30), &mut started_rx)
            .await
            .is_err();
        drop(lease);
        let result = tokio::time::timeout(Duration::from_secs(1), action)
            .await
            .expect("nested actuator acquisition must not deadlock")
            .unwrap();
        assert_eq!(result.is_error, None);
        assert!(
            queued,
            "before snapshot must wait for the previous mutation"
        );
    }

    #[tokio::test]
    async fn observation_retains_same_pid_lease_after_actuator_returns() {
        const PID: i32 = 93102;
        let (observing_tx, observing_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let first = tokio::spawn(async move {
            ObservedActionTool::new(Box::new(MutationProbe))
                .invoke_observed(
                    PID,
                    serde_json::json!({"pid": PID}),
                    async {},
                    |(), result| async {
                        observing_tx.send(()).unwrap();
                        finish_rx.await.unwrap();
                        result
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), observing_rx)
            .await
            .expect("actuator must reach post-observation")
            .unwrap();
        // A different process is independent even while observation is pending.
        let other = tokio::time::timeout(
            Duration::from_secs(1),
            crate::background_mutation::acquire(PID + 100),
        )
        .await
        .expect("different pid must stay independent");
        drop(other);
        let mut sibling = tokio::spawn(crate::background_mutation::acquire(PID));
        let queued = tokio::time::timeout(Duration::from_millis(30), &mut sibling)
            .await
            .is_err();
        finish_tx.send(()).unwrap();
        first.await.unwrap();
        if queued {
            drop(
                tokio::time::timeout(Duration::from_secs(1), sibling)
                    .await
                    .expect("sibling should proceed after observation")
                    .unwrap(),
            );
        }
        assert!(
            queued,
            "sibling must wait after dispatch until observation finishes"
        );
    }

    fn root(window_id: u32, title: &str) -> Root {
        Root {
            window_id: Some(window_id),
            title: title.into(),
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

    #[test]
    fn root_diff_ignores_metadata_changes_and_reports_an_appeared_modal() {
        let parent = RootKey::Native {
            window_id: 7,
            role: "AXWindow".into(),
            subrole: "AXStandardWindow".into(),
        };
        let before = RootSnapshot::from([(parent.clone(), root(7, "Draft"))]);
        let mut after = RootSnapshot::from([(parent, root(7, "Draft — Edited"))]);
        assert!(appeared_roots(&Some(before.clone()), &Some(after.clone())).is_empty());

        let sheet = RootKey::Native {
            window_id: 8,
            role: "AXSheet".into(),
            subrole: String::new(),
        };
        after.insert(sheet, root(8, "Open"));
        assert_eq!(
            appeared_roots(&Some(before.clone()), &Some(after.clone())),
            vec![root(8, "Open")]
        );
    }

    #[test]
    fn owner_resolution_can_follow_an_ax_root_without_guessing() {
        let roots = vec![root(8, "Open")];
        assert!(resolve_candidates(42, "TextEdit", &roots, &[]).is_empty());

        let windows = vec![window(8, 99, "Open and Save Panel Service")];
        let candidates = resolve_candidates(42, "TextEdit", &roots, &windows);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pid, 99);
        assert_eq!(candidates[0].window_id, 8);
    }

    #[test]
    fn owner_resolution_preserves_an_addressable_same_pid_proxy() {
        let roots = vec![root(8, "Open")];
        let windows = vec![
            window(8, 42, "TextEdit"),
            window(9, 99, "Open and Save Panel Service"),
        ];

        let candidates = resolve_candidates(42, "TextEdit", &roots, &windows);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pid, 42);
        assert_eq!(candidates[0].window_id, 8);
        assert_eq!(candidates[0].app_name, "TextEdit");
    }

    #[test]
    fn unrelated_foreground_change_is_not_a_surface_delta() {
        assert_eq!(resolve_surface_delta(Vec::new(), true), None);
    }
}
