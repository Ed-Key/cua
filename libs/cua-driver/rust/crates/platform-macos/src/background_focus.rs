//! Retained logical host focus and temporary, protected native preparation.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use core_foundation::base::{CFRelease, CFTypeRef};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use crate::ax::{bindings as ax, enablement::process_start_stamp};
use crate::focus_event_tap::{EventTap, TapDecision};
use crate::input::native_focus_events::{self as events, Notification, WindowActivation};
use crate::input::skylight;
use crate::logical_focus::{FocusOwner, HostFocusContext, LogicalFocusContexts, ProcessIdentity};

const RESTORE_GATE: Duration = Duration::from_millis(50);
const QUIET_GATE: Duration = Duration::from_millis(100);
const PREPARATION_LIMIT: Duration = Duration::from_secs(2);
const READINESS_LIMIT: Duration = Duration::from_millis(250);
// Candidate lookup budget, separate from native readiness and recovery gates.
const WINDOW_CAPTURE_LIMIT: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(10);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CGEventPostToPid(pid: i32, event: *mut c_void);
    fn CGWindowListCopyWindowInfo(
        options: u32,
        relative_to_window: u32,
    ) -> core_foundation::array::CFArrayRef;
}

fn identity(pid: i32) -> Option<ProcessIdentity> {
    (pid > 0).then_some(())?;
    Some(ProcessIdentity {
        pid,
        start_stamp: process_start_stamp(pid)?,
    })
}

fn alive(process: ProcessIdentity) -> bool {
    identity(process.pid) == Some(process)
}

fn foreground() -> Option<ProcessIdentity> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let app = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let workspace = identity(app.processIdentifier())?;
        // Workspace is the native capture source. WindowServer must agree
        // before CUA admits or restores physical foreground ownership.
        (skylight::front_pid().and_then(identity) == Some(workspace)).then_some(workspace)
    })
}

#[derive(Default)]
struct HostGate {
    operation: Mutex<()>,
    prepared_generation: AtomicU64,
}

#[derive(Default)]
struct Registry {
    logical: LogicalFocusContexts,
    controllers: HashMap<ProcessIdentity, Arc<HostController>>,
    gates: HashMap<i32, Weak<HostGate>>,
    pending: HashMap<(ProcessIdentity, u64), Arc<HostController>>,
}

#[derive(Default)]
pub(crate) struct FocusControllers {
    inner: Mutex<Registry>,
}

impl FocusControllers {
    /// Capture owns no taps or input. Dispatch must already have authorized PID.
    pub(crate) fn acquire(&self, owner: FocusOwner, pid: i32) -> Result<Arc<HostController>> {
        let host = identity(pid).context("native focus host identity is unavailable")?;
        let (controller, retired) = self.acquire_identity(owner, host);
        // Retired callbacks immediately pass. Native resources are cleaned by
        // bounded lifecycle retries, never as a side effect of capture.
        for retired in retired {
            self.pend(retired);
        }
        Ok(controller)
    }

    fn acquire_identity(
        &self,
        owner: FocusOwner,
        host: ProcessIdentity,
    ) -> (Arc<HostController>, Vec<Arc<HostController>>) {
        let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let (context, retired) = registry.logical.acquire(owner, host);
        let retired = remove_retired(&mut registry, retired);
        registry.gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = registry
            .gates
            .get(&host.pid)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let gate = Arc::new(HostGate::default());
                registry.gates.insert(host.pid, Arc::downgrade(&gate));
                gate
            });
        let controller = registry
            .controllers
            .entry(host)
            .or_insert_with(|| Arc::new(HostController::new(context, gate)))
            .clone();
        (controller, retired)
    }

    /// Caller must serialize retirement with in-flight per-host actions.
    pub(crate) fn retire_session(&self, runtime: &str, session: &str) -> Result<(), String> {
        let mut retired = {
            let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let contexts = registry.logical.retire_session(runtime, session);
            remove_retired(&mut registry, contexts)
        };
        retired.extend(self.take_pending());
        self.cleanup(retired)
    }

    pub(crate) fn retire_runtime(&self, runtime: &str) -> Result<(), String> {
        let mut retired = {
            let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let contexts = registry.logical.retire_runtime(runtime);
            remove_retired(&mut registry, contexts)
        };
        retired.extend(self.take_pending());
        self.cleanup(retired)
    }

    pub(crate) fn retire_all(&self) -> Result<(), String> {
        let mut retired = {
            let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let contexts = registry.logical.retire_all();
            remove_retired(&mut registry, contexts)
        };
        retired.extend(self.take_pending());
        self.cleanup(retired)
    }

    /// One bounded attempt per pending controller, never an idle timer.
    pub(crate) fn retry_pending(&self) -> Result<(), String> {
        self.cleanup(self.take_pending())
    }

    fn take_pending(&self) -> Vec<Arc<HostController>> {
        let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut registry.pending)
            .into_values()
            .collect()
    }

    fn pend(&self, controller: Arc<HostController>) {
        let key = (
            controller.context.identity(),
            controller.context.generation(),
        );
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .insert(key, controller);
    }

    fn cleanup(&self, controllers: Vec<Arc<HostController>>) -> Result<(), String> {
        let mut errors = Vec::new();
        for controller in controllers {
            if let Err(error) = controller.teardown() {
                errors.push(error.to_string());
                self.pend(controller);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

fn remove_retired(
    registry: &mut Registry,
    contexts: Vec<Arc<HostFocusContext>>,
) -> Vec<Arc<HostController>> {
    contexts
        .into_iter()
        .filter_map(|context| {
            let key = context.identity();
            if registry
                .controllers
                .get(&key)
                .is_some_and(|controller| controller.context.generation() == context.generation())
            {
                registry.controllers.remove(&key)
            } else {
                None
            }
        })
        .collect()
}

pub(crate) struct HostController {
    context: Arc<HostFocusContext>,
    operation: Arc<HostGate>,
    observer: Mutex<Option<EventTap>>,
    suppress: Arc<AtomicBool>,
    cleaned: AtomicBool,
}

impl HostController {
    fn new(context: Arc<HostFocusContext>, operation: Arc<HostGate>) -> Self {
        Self {
            context,
            operation,
            observer: Mutex::new(None),
            suppress: Arc::new(AtomicBool::new(false)),
            cleaned: AtomicBool::new(false),
        }
    }

    pub(crate) fn host_pid(&self) -> i32 {
        self.context.host_pid()
    }

    fn observer_active(&self) -> bool {
        let active = self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(EventTap::is_active);
        if !active {
            self.context.set_observation_available(false);
        }
        active
    }

    fn ensure_observer(&self) -> Result<()> {
        let mut observer = self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if observer.as_ref().is_some_and(EventTap::is_active) {
            return Ok(());
        }
        self.context.set_observation_available(false);
        *observer = None;
        let weak = Arc::downgrade(&self.context);
        let lost = weak.clone();
        let suppress = self.suppress.clone();
        let tap = EventTap::start(
            self.context.host_pid(),
            &[13, 19, 20],
            move |kind, event| {
                let Some(context) = weak.upgrade() else {
                    return TapDecision::Pass;
                };
                if context.is_retired() || !alive(context.identity()) {
                    context.set_observation_available(false);
                    return TapDecision::Pass;
                }
                let subtype = unsafe { CGEventGetIntegerValueField(event, 83) };
                context.observe_actual_foreground(foreground());
                if suppress.load(Ordering::Acquire) && suppression_event(kind, subtype) {
                    return TapDecision::Drop;
                }
                context.observe_activation(kind, subtype);
                TapDecision::Pass
            },
            move || {
                if let Some(context) = lost.upgrade() {
                    context.set_observation_available(false);
                }
            },
        )?;
        self.context.set_observation_available(tap.is_active());
        *observer = Some(tap);
        Ok(())
    }

    /// Root holds its async per-host mutation lease through input and readback.
    pub(crate) fn prepare(&self, window_id: Option<u32>) -> Result<()> {
        self.prepare_with(window_id, || Ok(()))
    }

    pub(crate) fn prepare_with(
        &self,
        window_id: Option<u32>,
        prepare_element: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        // The preparation brings the host forward and restores the original:
        // an activation, so it passes the foreground admission and holds the
        // turn through the restore.
        let _turn = crate::input::mouse::foreground_turn()?;
        let started = Instant::now();
        let _operation = self
            .operation
            .operation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !self.context.is_retired() && alive(self.context.identity()),
            "native focus host was retired or replaced"
        );
        let observer_result = self.ensure_observer();
        let actual = foreground()
            .context("native focus foreground ownership is unavailable or inconsistent")?;
        self.context.observe_actual_foreground(Some(actual));
        match Some(actual).filter(|original| *original != self.context.identity()) {
            None => {
                if observer_result.is_err() {
                    self.context.set_observation_available(false);
                }
                prepare_then_recover(
                    || {
                        self.enforce(window_id, || Ok(()))?;
                        wait_frontmost(self.context.identity(), READINESS_LIMIT, || Ok(()))
                    },
                    prepare_element,
                    || Ok(()),
                )
            }
            Some(original) => {
                observer_result.context("native background focus observer is unavailable")?;
                let protection = Preparation::start(self, original, started)?;
                protection.check_original_front()?;
                let result = skylight::with_updates_disabled(|| {
                    protection.check_original_front()?;
                    protection.suppress.store(true, Ordering::Release);
                    prepare_then_recover(
                        || {
                            self.enforce(window_id, || protection.check())?;
                            wait_frontmost(self.context.identity(), READINESS_LIMIT, || {
                                protection.check()
                            })
                            .context("native focus preparation lost protection")
                        },
                        || {
                            protection.check()?;
                            prepare_element()?;
                            protection.check()
                        },
                        || protection.recover(),
                    )
                });
                // Reenable updates before releasing filtering and temporary taps.
                drop(protection);
                result
            }
        }
    }

    fn enforce(&self, window_id: Option<u32>, check: impl Fn() -> Result<()>) -> Result<()> {
        check()?;
        ensure!(
            !self.context.is_retired() && alive(self.context.identity()),
            "native focus host changed before posting"
        );
        self.observer_active();
        let state = self.context.snapshot();
        if state.believed_focus == Some(true) && state.observation_available {
            self.operation
                .prepared_generation
                .store(self.context.generation(), Ordering::Release);
            return Ok(());
        }
        if state.believed_active == Some(true) && state.observation_available {
            let sequence = [events::notification(Notification::Refocus)?];
            check()?;
            ensure!(
                !self.context.is_retired() && alive(self.context.identity()),
                "native focus host changed before posting"
            );
            events::post(self.context.host_pid(), &sequence)?;
            self.context.mark_key_focus_returned_posted();
        } else {
            // Native activation alone reads the optional window. Cached focus
            // and refocus branches do not inspect AXActivationPoint or bounds.
            let window = capture_activation(self.context.identity(), window_id)?;
            check()?;
            let sequence = events::activation(window)?;
            check()?;
            ensure!(
                !self.context.is_retired() && alive(self.context.identity()),
                "native focus host changed before posting"
            );
            events::post(self.context.host_pid(), &sequence)?;
            self.context.mark_activation_posted();
        }
        self.operation
            .prepared_generation
            .store(self.context.generation(), Ordering::Release);
        Ok(())
    }

    fn teardown(&self) -> Result<()> {
        if self.cleaned.load(Ordering::Acquire) {
            return Ok(());
        }
        let _mutation = crate::background_mutation::try_acquire(self.host_pid())
            .context("native focus retirement host is busy")?;
        let _operation = self
            .operation
            .operation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.cleaned.load(Ordering::Acquire) {
            return Ok(());
        }
        let state = self.context.snapshot();
        let latest = self.operation.prepared_generation.load(Ordering::Acquire);
        // A new generation explicitly prepared this host. Old cleanup closes
        // its resources without deactivating the new controller's application.
        let superseded = latest != 0 && latest != self.context.generation();
        if !superseded && state.believed_active == Some(true) && alive(self.context.identity()) {
            let actual = foreground()
                .context("cannot safely retire native focus with unknown foreground")?;
            if actual != self.context.identity() {
                events::post(
                    self.context.host_pid(),
                    &[events::notification(Notification::Deactivate)?],
                )?;
            }
        }
        let _ = self.operation.prepared_generation.compare_exchange(
            self.context.generation(),
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.suppress.store(false, Ordering::Release);
        *self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.cleaned.store(true, Ordering::Release);
        Ok(())
    }
}

// Field preparation runs after enforcement, while recovery protection is held.
// A field error still requires recovery before its native resources are released.
fn prepare_then_recover(
    enforce: impl FnOnce() -> Result<()>,
    prepare_element: impl FnOnce() -> Result<()>,
    recover: impl FnOnce() -> Result<()>,
) -> Result<()> {
    enforce()?;
    let prepared = prepare_element();
    let recovered = recover();
    match (prepared, recovered) {
        (Err(field), Err(recovery)) => Err(anyhow::anyhow!(
            "{field:#}; native focus recovery also failed: {recovery:#}"
        )),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

impl Drop for HostController {
    fn drop(&mut self) {
        // A cancelled async invocation can leave its blocking actuator alive
        // after the registry's one retirement attempt. The actuator retains
        // this controller and releases its delivery lease before its captured
        // Arc, so final ownership provides one last cleanup opportunity.
        if self.context.is_retired() && !self.cleaned.load(Ordering::Acquire) {
            if let Err(error) = self.teardown() {
                tracing::warn!(pid = self.host_pid(), %error, "final native focus retirement failed");
            }
        }
    }
}

fn suppression_event(kind: u32, subtype: i64) -> bool {
    kind == 19 || (kind == 13 && matches!(subtype, 2 | 22 | 23))
}

fn original_filter_event(kind: u32, subtype: i64) -> bool {
    kind == 20 || suppression_event(kind, subtype) || (kind == 13 && subtype == 1)
}

struct Preparation<'a> {
    controller: &'a HostController,
    started: Instant,
    host: ProcessIdentity,
    original: ProcessIdentity,
    original_window: Option<CapturedWindow>,
    suppress: Arc<AtomicBool>,
    unavailable: Arc<AtomicBool>,
    // Another app came to the front during preparation: the user's choice.
    // Cleanup must not restore over it.
    user_switched: Arc<AtomicBool>,
    last_event: Arc<Mutex<Instant>>,
    original_tap: EventTap,
    keyboard_tap: EventTap,
}

impl<'a> Preparation<'a> {
    fn start(
        controller: &'a HostController,
        original: ProcessIdentity,
        started: Instant,
    ) -> Result<Self> {
        let host = controller.context.identity();
        ensure!(
            alive(host) && alive(original) && foreground() == Some(original),
            "foreground changed before native preparation"
        );
        let helper = identity(std::process::id() as i32)
            .context("helper process identity is unavailable")?;
        let original_window = capture_original_window(original)?;
        let unavailable = Arc::new(AtomicBool::new(false));
        let user_switched = Arc::new(AtomicBool::new(false));
        let last_event = Arc::new(Mutex::new(Instant::now()));
        let active = controller.suppress.clone();
        let clock = last_event.clone();
        let loss = unavailable.clone();
        let lost = (unavailable.clone(), user_switched.clone());
        let original_tap = EventTap::start(
            original.pid,
            &[13, 19, 20],
            move |kind, event| {
                if !active.load(Ordering::Acquire) || !alive(original) {
                    return TapDecision::Pass;
                }
                if !protection_holds(host, original, &active, &lost.0, &lost.1) {
                    return TapDecision::Pass;
                }
                *clock.lock().unwrap_or_else(|error| error.into_inner()) = Instant::now();
                if original_filter_event(kind, unsafe { CGEventGetIntegerValueField(event, 83) }) {
                    TapDecision::Drop
                } else {
                    TapDecision::Pass
                }
            },
            move || {
                loss.store(true, Ordering::Release);
            },
        )?;
        let active = controller.suppress.clone();
        let loss = unavailable.clone();
        let lost = (unavailable.clone(), user_switched.clone());
        let keyboard_tap = EventTap::start(
            host.pid,
            &[10, 11, 12],
            move |_, event| {
                if !active.load(Ordering::Acquire)
                    || !alive(host)
                    || !alive(original)
                    || !alive(helper)
                {
                    return TapDecision::Pass;
                }
                if !protection_holds(host, original, &active, &lost.0, &lost.1) {
                    return TapDecision::Pass;
                }
                let source = unsafe { CGEventGetIntegerValueField(event, 41) };
                let target = unsafe { CGEventGetIntegerValueField(event, 40) };
                if !forward_keyboard(source, target, helper.pid, host.pid) {
                    return TapDecision::Pass;
                }
                if !alive(original) || !alive(host) {
                    return TapDecision::Pass;
                }
                unsafe { CGEventPostToPid(original.pid, event) };
                TapDecision::Drop
            },
            move || {
                loss.store(true, Ordering::Release);
            },
        )?;
        ensure!(
            original_tap.is_active() && keyboard_tap.is_active(),
            "native preparation taps are unavailable"
        );
        Ok(Self {
            controller,
            started,
            host,
            original,
            original_window,
            suppress: controller.suppress.clone(),
            unavailable,
            user_switched,
            last_event,
            original_tap,
            keyboard_tap,
        })
    }

    fn check(&self) -> Result<()> {
        ensure!(
            self.started.elapsed() < PREPARATION_LIMIT,
            "native focus preparation exceeded its total bound"
        );
        ensure!(
            !self.unavailable.load(Ordering::Acquire)
                && self.controller.observer_active()
                && self.original_tap.is_active()
                && self.keyboard_tap.is_active(),
            "native focus protection was lost"
        );
        ensure!(
            alive(self.host) && alive(self.original),
            "native preparation process was replaced"
        );
        ensure!(
            matches!(
                recovery_choice(self.host, self.original, foreground()),
                Recovery::Stay | Recovery::Restore
            ),
            "user foreground choice changed during native preparation"
        );
        Ok(())
    }

    /// Before CUA posts its own activation, the target in front can only be
    /// the user's choice, so preparation stops instead of restoring over it.
    fn check_original_front(&self) -> Result<()> {
        self.check()?;
        if foreground() != Some(self.original) {
            // Recorded so cleanup does not restore over the user's choice.
            self.user_switched.store(true, Ordering::Release);
            bail!("user foreground choice changed before native preparation");
        }
        Ok(())
    }

    fn restore(&self) -> bool {
        if self.user_switched.load(Ordering::Acquire) {
            return false;
        }
        if !alive(self.host) || !alive(self.original) || foreground() != Some(self.host) {
            return false;
        }
        // Match the native options-zero application restoration; do not raise a window.
        crate::apps::activate_pid(self.original.pid)
    }

    fn recover(&self) -> Result<()> {
        let mut last_restore = self.started;
        while self.started.elapsed() < PREPARATION_LIMIT {
            self.check()?;
            match recovery_choice(self.host, self.original, foreground()) {
                Recovery::Stay => {
                    if self
                        .last_event
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .elapsed()
                        >= QUIET_GATE
                    {
                        if self.raise_original_window_if_needed()? {
                            *self
                                .last_event
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) = Instant::now();
                            continue;
                        }
                        return Ok(());
                    }
                }
                Recovery::Restore if last_restore.elapsed() >= RESTORE_GATE => {
                    ensure!(
                        self.restore(),
                        "original application restoration was rejected"
                    );
                    last_restore = Instant::now();
                }
                Recovery::Restore => {}
                Recovery::Other | Recovery::Unknown => {
                    bail!("foreground changed during native preparation")
                }
            }
            std::thread::sleep(POLL);
        }
        bail!("native foreground recovery exceeded its preparation bound")
    }

    fn raise_original_window_if_needed(&self) -> Result<bool> {
        let Some(window) = &self.original_window else {
            return Ok(false);
        };
        let front_to_back = on_screen_window_order()?;
        if !needs_original_raise(&front_to_back, window.window_id, self.host.pid) {
            return Ok(false);
        }
        self.check()?;
        ensure!(
            foreground() == Some(self.original),
            "foreground changed before original window restoration"
        );
        let current_id = unsafe { ax::ax_get_window_id_checked(window.element.0) }
            .map_err(|error| anyhow::anyhow!("original window identity read failed: {error}"))?;
        ensure!(
            current_id == Some(window.window_id),
            "original window identity changed before restoration"
        );
        // AXRaise has no authority to select another process or window. The
        // captured reference remains local to this protected preparation.
        self.check()?;
        ensure!(
            foreground() == Some(self.original),
            "foreground changed before original window restoration"
        );
        let status = unsafe { ax::perform_action(window.element.0, "AXRaise") };
        ensure!(
            status == 0,
            "original window AXRaise was rejected: {status}"
        );
        Ok(true)
    }
}

impl Drop for Preparation<'_> {
    fn drop(&mut self) {
        if foreground() == Some(self.host) && !self.restore() {
            tracing::warn!(
                pid = self.host.pid,
                "native foreground recovery failed during cleanup"
            );
        }
        self.suppress.store(false, Ordering::Release);
        self.original_tap.close();
        self.keyboard_tap.close();
    }
}

/// A tap callback's foreground decision. Another app in front is the user's
/// choice: filtering stops and the loss is recorded, so `Preparation::check`
/// ends the preparation instead of continuing unprotected. A momentary
/// disagreement between NSWorkspace and WindowServer (Unknown) keeps the
/// protection; `check` itself still refuses to proceed while it lasts.
fn protection_holds(
    host: ProcessIdentity,
    original: ProcessIdentity,
    active: &AtomicBool,
    lost: &AtomicBool,
    user_switched: &AtomicBool,
) -> bool {
    if recovery_choice(host, original, foreground()) == Recovery::Other {
        active.store(false, Ordering::Release);
        user_switched.store(true, Ordering::Release);
        lost.store(true, Ordering::Release);
        return false;
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    Stay,
    Restore,
    Other,
    Unknown,
}

fn recovery_choice(
    host: ProcessIdentity,
    original: ProcessIdentity,
    actual: Option<ProcessIdentity>,
) -> Recovery {
    match actual {
        Some(actual) if actual == original => Recovery::Stay,
        Some(actual) if actual == host => Recovery::Restore,
        Some(_) => Recovery::Other,
        None => Recovery::Unknown,
    }
}

fn forward_keyboard(source: i64, target: i64, helper: i32, host: i32) -> bool {
    source != helper as i64 && target == host as i64
}

struct Element(ax::AXUIElementRef);
impl Drop for Element {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

struct CapturedWindow {
    element: Element,
    window_id: u32,
}

fn capture_original_window(original: ProcessIdentity) -> Result<Option<CapturedWindow>> {
    let app = app_element(original.pid)?;
    let Some(raw) = (unsafe { ax::copy_element_attr(app.0, "AXFocusedWindow") }) else {
        ensure!(
            alive(original),
            "original application changed during window capture"
        );
        return Ok(None);
    };
    let element = Element(raw);
    unsafe {
        ax::AXUIElementSetMessagingTimeout(element.0, 0.05);
    }
    let window_id = unsafe { ax::ax_get_window_id_checked(element.0) }.map_err(|error| {
        anyhow::anyhow!("original focused window identity read failed: {error}")
    })?;
    ensure!(
        alive(original),
        "original application changed during window capture"
    );
    Ok(window_id
        .filter(|id| *id != 0)
        .map(|window_id| CapturedWindow { element, window_id }))
}

#[derive(Clone, Copy, Debug)]
struct WindowOrder {
    window_id: u32,
    pid: i32,
    layer: i32,
}

fn needs_original_raise(front_to_back: &[WindowOrder], original_id: u32, host_pid: i32) -> bool {
    let Some(index) = front_to_back
        .iter()
        .position(|window| window.window_id == original_id)
    else {
        return false;
    };
    let layer = front_to_back[index].layer;
    front_to_back[..index]
        .iter()
        .any(|window| window.pid == host_pid && window.layer == layer)
}

fn on_screen_window_order() -> Result<Vec<WindowOrder>> {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, TCFType},
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };
    // Native options: on-screen only and exclude desktop elements. All layers
    // remain present, including transparent windows and geometry-less records.
    let raw = unsafe { CGWindowListCopyWindowInfo(0x11, 0) };
    ensure!(
        !raw.is_null(),
        "original window recovery list is unavailable"
    );
    let array: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(raw) };
    ensure!(
        unsafe { CFGetTypeID(raw.cast()) } == CFArray::<CFTypeRef>::type_id(),
        "original window recovery list is malformed"
    );
    array
        .iter()
        .map(|item| {
            let item = *item;
            ensure!(
                unsafe { CFGetTypeID(item) }
                    == CFDictionary::<*const c_void, *const c_void>::type_id(),
                "original window recovery record is malformed"
            );
            let dict: CFDictionary<*const c_void, *const c_void> =
                unsafe { CFDictionary::wrap_under_get_rule(item.cast()) };
            let number = |key: &str| -> Result<i64> {
                let key = CFString::new(key);
                let value = dict
                    .find(key.as_concrete_TypeRef().cast())
                    .context("original window recovery field is missing")?;
                ensure!(
                    unsafe { CFGetTypeID(*value) } == CFNumber::type_id(),
                    "original window recovery field is malformed"
                );
                unsafe { CFNumber::wrap_under_get_rule((*value).cast()) }
                    .to_i64()
                    .context("original window recovery field is unreadable")
            };
            Ok(WindowOrder {
                window_id: u32::try_from(number("kCGWindowNumber")?)
                    .context("original recovery window ID is out of range")?,
                pid: i32::try_from(number("kCGWindowOwnerPID")?)
                    .context("original recovery owner PID is out of range")?,
                layer: i32::try_from(number("kCGWindowLayer")?)
                    .context("original recovery window layer is out of range")?,
            })
        })
        .collect()
}

fn app_element(pid: i32) -> Result<Element> {
    let app = unsafe { ax::AXUIElementCreateApplication(pid) };
    ensure!(
        !app.is_null(),
        "native application AX element is unavailable"
    );
    unsafe { ax::AXUIElementSetMessagingTimeout(app, 0.05) };
    Ok(Element(app))
}

fn capture_activation(
    host: ProcessIdentity,
    window_id: Option<u32>,
) -> Result<Option<WindowActivation>> {
    let Some(window_id) = window_id else {
        return Ok(None);
    };
    ensure!(
        window_id != 0 && alive(host),
        "native activation window identity is invalid"
    );
    let app = app_element(host.pid)?;
    let started = Instant::now();
    // The same window list as the exact-target gate: AXWindows plus the
    // requested window recovered by its remote token when it sits on another
    // Space, so a window the gate admitted can also be activated.
    let (windows, read_failed) =
        unsafe { ax::copy_ax_windows_including_reporting(app.0, host.pid, window_id) };
    let windows: Vec<_> = windows.into_iter().map(Element).collect();
    let mut found = None;
    for window in windows {
        ensure!(
            started.elapsed() < WINDOW_CAPTURE_LIMIT,
            "native activation window lookup exceeded its capture bound"
        );
        unsafe { ax::AXUIElementSetMessagingTimeout(window.0, 0.05) };
        if unsafe { ax::ax_get_window_id_checked(window.0) }
            .ok()
            .flatten()
            != Some(window_id)
        {
            continue;
        }
        let point = unsafe {
            ax::copy_geometry_attr_checked(window.0, "AXActivationPoint", ax::kAXValueCGPointType)
        };
        let point = match point {
            Ok(point) => point,
            Err(ax::kAXErrorAttributeUnsupported | ax::kAXErrorNoValue) => return Ok(None),
            Err(error) => bail!("native AXActivationPoint read failed: {error}"),
        };
        let bounds = unsafe { ax::element_screen_rect(window.0) }
            .context("native window-aware activation bounds are unavailable")?;
        found = Some(WindowActivation {
            window_id,
            bounds: CGRect::new(
                &CGPoint::new(bounds[0], bounds[1]),
                &CGSize::new(bounds[2], bounds[3]),
            ),
            activation_point: CGPoint::new(point[0], point[1]),
        });
        break;
    }
    ensure!(
        alive(host),
        "native activation host changed during AX capture"
    );
    match found {
        Some(found) => Ok(Some(found)),
        None if read_failed => bail!("native activation window list read failed"),
        None => bail!("native activation window was not found"),
    }
}

fn wait_frontmost(
    host: ProcessIdentity,
    limit: Duration,
    mut check: impl FnMut() -> Result<()>,
) -> Result<()> {
    let start = Instant::now();
    let app = app_element(host.pid)?;
    while start.elapsed() < limit {
        check()?;
        ensure!(alive(host), "native readiness host was replaced");
        if unsafe { ax::copy_bool_attr(app.0, "AXFrontmost") } == Some(true) {
            return Ok(());
        }
        std::thread::sleep(QUIET_GATE);
    }
    // Native acknowledgment waiting is best-effort, not a strict AX success gate.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_preparation_follows_enforcement_and_recovers_even_on_field_error() {
        let cases: &[&[&str]] = &[
            &[],
            &["enforce"],
            &["field"],
            &["recover"],
            &["field", "recover"],
        ];
        for failed_stages in cases {
            let stages = std::cell::RefCell::new(Vec::new());
            let run = |stage| {
                stages.borrow_mut().push(stage);
                if failed_stages.contains(&stage) {
                    bail!("{stage} failed");
                }
                Ok(())
            };
            let result =
                prepare_then_recover(|| run("enforce"), || run("field"), || run("recover"));
            match *failed_stages {
                [] => result.unwrap(),
                [stage] => assert_eq!(result.unwrap_err().to_string(), format!("{stage} failed")),
                _ => assert_eq!(
                    result.unwrap_err().to_string(),
                    "field failed; native focus recovery also failed: recover failed"
                ),
            }
            assert_eq!(
                stages.into_inner(),
                if failed_stages.contains(&"enforce") {
                    vec!["enforce"]
                } else {
                    vec!["enforce", "field", "recover"]
                },
            );
        }
    }

    fn process(pid: i32, stamp: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_stamp: (stamp, 0),
        }
    }
    fn owner(runtime: &str, session: &str) -> FocusOwner {
        FocusOwner {
            runtime: runtime.into(),
            session: Some(session.into()),
        }
    }

    #[test]
    fn capture_is_pure_and_retirement_waits_for_last_owner() {
        let registry = FocusControllers::default();
        let (first, _) = registry.acquire_identity(owner("a", "s"), process(-91_905, 1));
        let (same, _) = registry.acquire_identity(owner("b", "s"), process(-91_905, 1));
        assert!(Arc::ptr_eq(&first, &same));
        assert!(first.observer.lock().unwrap().is_none());
        registry.retire_session("a", "s").unwrap();
        assert!(!first.context.is_retired());
        registry.retire_runtime("b").unwrap();
        assert!(first.context.is_retired());
        assert!(first.cleaned.load(Ordering::Acquire));
    }

    #[test]
    fn busy_retirement_retains_resources_until_one_bounded_retry() {
        let registry = FocusControllers::default();
        let (controller, _) = registry.acquire_identity(owner("busy", "s"), process(-91_900, 1));
        let action = crate::background_mutation::try_acquire(-91_900).unwrap();
        assert_eq!(
            registry.retire_session("busy", "s").unwrap_err(),
            "native focus retirement host is busy"
        );
        assert!(controller.context.is_retired());
        assert!(!controller.cleaned.load(Ordering::Acquire));
        assert_eq!(registry.inner.lock().unwrap().pending.len(), 1);
        assert_eq!(
            registry.retry_pending().unwrap_err(),
            "native focus retirement host is busy"
        );
        assert_eq!(registry.inner.lock().unwrap().pending.len(), 1);
        drop(action);
        // The owner was already removed by the first attempt. Repeating its
        // lifecycle hook must still retry the retained native resources.
        registry.retire_session("busy", "s").unwrap();
        assert!(controller.cleaned.load(Ordering::Acquire));
        assert!(registry.inner.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn retired_final_owner_cleans_after_blocking_delivery_releases_its_lease() {
        let registry = FocusControllers::default();
        let host = process(-91_904, 1);
        let (controller, _) = registry.acquire_identity(owner("closing", "s"), host);
        controller.context.mark_activation_posted();
        let generation = controller.context.generation();
        let gate = controller.operation.clone();
        gate.prepared_generation
            .store(generation, Ordering::Release);
        let weak = Arc::downgrade(&controller);
        let lease = Arc::new(crate::background_mutation::try_acquire(host.pid).unwrap());
        let worker_lease = lease.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _delivery_lease = worker_lease;
            started.send(()).unwrap();
            released.recv().unwrap();
            // As in the keyboard closures, keep the controller in the closure
            // captures. Its last Arc drops after the body-local delivery lease.
            assert!(controller.context.is_retired());
        });
        ready.recv().unwrap();
        assert_eq!(
            registry.retire_all().unwrap_err(),
            "native focus retirement host is busy"
        );
        drop(lease);
        drop(registry);
        assert!(weak.upgrade().is_some());
        assert_eq!(gate.prepared_generation.load(Ordering::Acquire), generation);
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(gate.prepared_generation.load(Ordering::Acquire), 0);
        assert!(crate::background_mutation::try_acquire(host.pid).is_ok());
    }

    #[tokio::test]
    async fn cancelled_nested_focus_retires_after_continued_blocking_delivery() {
        let registry = FocusControllers::default();
        let host = process(-91_907, 1);
        let (controller, _) = registry.acquire_identity(owner("nested", "s"), host);
        controller.context.mark_activation_posted();
        let generation = controller.context.generation();
        let gate = controller.operation.clone();
        gate.prepared_generation
            .store(generation, Ordering::Release);
        let weak = Arc::downgrade(&controller);
        let lease = Arc::new(crate::background_mutation::acquire(host.pid).await);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            crate::background_mutation::with_held_focus(controller, lease, async move {
                crate::background_mutation::spawn_blocking(move || {
                    let _ = started.send(());
                    released.recv_timeout(Duration::from_secs(2)).unwrap();
                })
                .await
                .unwrap();
            })
            .await;
        });
        ready.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(
            registry.retire_all().unwrap_err(),
            "native focus retirement host is busy"
        );
        drop(registry);
        assert!(weak.upgrade().is_some());
        assert_eq!(gate.prepared_generation.load(Ordering::Acquire), generation);
        assert!(crate::background_mutation::try_acquire(host.pid).is_err());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak.upgrade().is_some() || gate.prepared_generation.load(Ordering::Acquire) != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Weak absence precedes destructor completion. Wait for teardown's
        // own mutation guard to release after its generation was cleared.
        let released = tokio::time::timeout(
            Duration::from_secs(1),
            crate::background_mutation::acquire(host.pid),
        )
        .await
        .unwrap();
        assert_eq!(gate.prepared_generation.load(Ordering::Acquire), 0);
        drop(released);
    }

    #[test]
    fn old_pending_cleanup_preserves_new_prepared_generation() {
        let registry = FocusControllers::default();
        let host = process(-91_903, 1);
        let (old, _) = registry.acquire_identity(owner("old", "s"), host);
        old.context.mark_activation_posted();
        old.operation
            .prepared_generation
            .store(old.context.generation(), Ordering::Release);
        let action = crate::background_mutation::try_acquire(host.pid).unwrap();
        assert_eq!(
            registry.retire_session("old", "s").unwrap_err(),
            "native focus retirement host is busy"
        );
        let (new, _) = registry.acquire_identity(owner("new", "s"), host);
        assert_eq!(new.context.snapshot().believed_active, None);
        assert_eq!(new.context.snapshot().believed_focus, None);
        assert_eq!(
            new.operation.prepared_generation.load(Ordering::Acquire),
            old.context.generation()
        );
        new.context.mark_activation_posted();
        new.operation
            .prepared_generation
            .store(new.context.generation(), Ordering::Release);
        drop(action);
        // Superseded cleanup reaches no process/foreground lookup or posting.
        registry.retry_pending().unwrap();
        assert!(old.cleaned.load(Ordering::Acquire));
        assert_eq!(new.context.snapshot().believed_active, Some(true));
        assert_eq!(
            new.operation.prepared_generation.load(Ordering::Acquire),
            new.context.generation()
        );
    }

    #[test]
    fn retire_all_removes_each_owner_and_cleans_each_pure_capture() {
        let registry = FocusControllers::default();
        let (first, _) = registry.acquire_identity(owner("a", "s"), process(-91_901, 1));
        let (second, _) = registry.acquire_identity(owner("b", "s"), process(-91_902, 1));
        registry.retire_all().unwrap();
        assert!(first.context.is_retired() && second.context.is_retired());
        assert!(first.cleaned.load(Ordering::Acquire) && second.cleaned.load(Ordering::Acquire));
        assert!(registry.inner.lock().unwrap().controllers.is_empty());
        assert!(registry.inner.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn reused_pid_shares_operation_gate_but_not_context_or_beliefs() {
        let registry = FocusControllers::default();
        let (old, _) = registry.acquire_identity(owner("r", "s"), process(321, 1));
        old.context.mark_activation_posted();
        let (new, retired) = registry.acquire_identity(owner("r", "s"), process(321, 2));
        assert_eq!(retired.len(), 1);
        assert!(old.context.is_retired());
        assert!(Arc::ptr_eq(&old.operation, &new.operation));
        assert!(!new.context.snapshot().can_skip_preparation());
        assert!(!old.context.observe_activation(20, 0));
    }

    #[test]
    fn recovery_never_restores_against_third_process_or_unknown_choice() {
        let host = process(321, 1);
        let original = process(432, 1);
        assert_eq!(
            recovery_choice(host, original, Some(host)),
            Recovery::Restore
        );
        assert_eq!(
            recovery_choice(host, original, Some(original)),
            Recovery::Stay
        );
        assert_eq!(
            recovery_choice(host, original, Some(process(543, 1))),
            Recovery::Other
        );
        assert_eq!(
            recovery_choice(host, original, Some(process(321, 2))),
            Recovery::Other
        );
        assert_eq!(recovery_choice(host, original, None), Recovery::Unknown);
    }

    #[test]
    fn filtering_keeps_helper_input_and_unknown_events_unmodified() {
        assert!(!forward_keyboard(99, 321, 99, 321));
        assert!(forward_keyboard(0, 321, 99, 321));
        assert!(!forward_keyboard(0, 432, 99, 321));
        assert!(forward_keyboard(i64::MAX, 321, 99, 321));
        assert!(forward_keyboard(-1, 321, 99, 321));
        assert!(!forward_keyboard(0, i64::MAX, 99, 321));
        assert!(suppression_event(19, 0));
        for subtype in [2, 22, 23] {
            assert!(suppression_event(13, subtype));
        }
        assert!(!suppression_event(20, 0));
        assert!(!suppression_event(13, 1));
        assert!(original_filter_event(20, 0));
        assert!(original_filter_event(13, 1));
        assert!(!original_filter_event(13, 42));
    }

    #[test]
    fn original_raise_requires_host_window_before_original_at_same_layer() {
        let original = WindowOrder {
            window_id: 10,
            pid: 100,
            layer: 8,
        };
        let target = WindowOrder {
            window_id: 20,
            pid: 200,
            layer: 8,
        };
        assert!(needs_original_raise(&[target, original], 10, 200));
        assert!(!needs_original_raise(&[original, target], 10, 200));
        assert!(!needs_original_raise(
            &[WindowOrder { layer: 0, ..target }, original],
            10,
            200
        ));
        assert!(!needs_original_raise(
            &[WindowOrder { pid: 100, ..target }, original],
            10,
            200
        ));
        assert!(!needs_original_raise(&[target], 10, 200));
        assert!(!needs_original_raise(&[], 10, 200));
        // No layer-zero, alpha, overlap, or bounds filtering participates.
        // A target window at accessory layer 8 still establishes the branch.
        assert!(needs_original_raise(
            &[
                WindowOrder {
                    window_id: 30,
                    pid: 300,
                    layer: 99
                },
                target,
                original
            ],
            10,
            200
        ));
    }
}
