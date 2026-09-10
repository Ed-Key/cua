//! Resolved action visuals and bounded macOS click approach admission.

use cursor_overlay::{CursorAction, OverlayCommand, VisualActionId, VisualEvent, VisualPhase};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResolvedPointerTarget {
    pub x: f64,
    pub y: f64,
    pub window_id: Option<u32>,
    pub element_bounds: Option<[f64; 4]>,
}

impl ResolvedPointerTarget {
    pub(crate) fn from_bounds(window_id: u32, rect: [f64; 4]) -> Option<Self> {
        if !rect.iter().all(|v| v.is_finite()) || rect[2] <= 0.0 || rect[3] <= 0.0 {
            return None;
        }
        let x = rect[0] + rect[2] / 2.0;
        let y = rect[1] + rect[3] / 2.0;
        (x.is_finite() && y.is_finite()).then_some(Self {
            x,
            y,
            window_id: Some(window_id),
            element_bounds: Some(rect),
        })
    }
}

/// Captures action ownership and resolved coordinates independently of later registry changes.
#[derive(Clone)]
pub(crate) struct PointerVisualHandle {
    key: String,
    event: Option<VisualEvent>,
}

pub(crate) fn emit_pointer_target(
    registry: &super::CursorRegistry,
    sink: &dyn PointerVisualSink,
    key: &str,
    target: Option<ResolvedPointerTarget>,
) -> PointerVisualHandle {
    emit_action_target(registry, sink, key, target, CursorAction::Click)
}

pub(crate) fn emit_action_target(
    registry: &super::CursorRegistry,
    sink: &dyn PointerVisualSink,
    key: &str,
    target: Option<ResolvedPointerTarget>,
    action: CursorAction,
) -> PointerVisualHandle {
    let mut handle = PointerVisualHandle {
        key: key.into(),
        event: None,
    };
    if key.is_empty() || cua_driver_core::session::is_session_ended(key) {
        return handle;
    }
    if let Some(target) = target {
        if let Some(window) = target.window_id {
            sink.send(key, OverlayCommand::PinAbove(window as u64));
        }
        registry.update_position(key, target.x, target.y);
    }
    if let Some(id) = sink.begin(key) {
        let event = VisualEvent {
            id,
            timestamp: Instant::now(),
            target: target.map(|t| (t.x, t.y)),
            window: target.and_then(|t| t.window_id.map(u64::from)),
            bounds: target.and_then(|t| t.element_bounds),
            action,
            scroll_direction: None,
            modifiers: None,
            phase: VisualPhase::Intent,
        };
        sink.publish(key, event.clone());
        handle.event = Some(event);
    }
    handle
}

#[cfg(test)]
pub(crate) fn begin_pointer_action(
    registry: &super::CursorRegistry,
    sink: Arc<dyn PointerVisualSink>,
    key: &str,
    target: Option<ResolvedPointerTarget>,
    action: CursorAction,
) -> Arc<DeliveryReceipt> {
    let handle = emit_action_target(registry, sink.as_ref(), key, target, action);
    let receipt = Arc::new(DeliveryReceipt::default());
    receipt.attach(sink, handle);
    receipt
}

pub(crate) fn point(x: f64, y: f64, window_id: Option<u32>) -> Option<ResolvedPointerTarget> {
    (x.is_finite() && y.is_finite()).then_some(ResolvedPointerTarget {
        x,
        y,
        window_id,
        element_bounds: None,
    })
}

pub(crate) fn emit_pointer_contact(sink: &dyn PointerVisualSink, handle: PointerVisualHandle) {
    handle.publish(sink, VisualPhase::Contact, Instant::now());
}

impl PointerVisualHandle {
    pub(crate) fn track(&self, sink: &dyn PointerVisualSink, x: f64, y: f64) {
        let mut next = self.clone();
        if let Some(event) = next.event.as_mut() {
            event.target = Some((x, y));
        }
        next.publish(sink, VisualPhase::Tracking, Instant::now());
    }
    pub(crate) fn scroll_contact(&self, sink: &dyn PointerVisualSink, dy: i32, dx: i32) {
        let Some((x, y)) = self.event.as_ref().and_then(|event| event.target) else {
            return;
        };
        self.scroll_contact_at(sink, x, y, dy, dx, Instant::now());
    }
    pub(crate) fn scroll_contact_at(
        &self,
        sink: &dyn PointerVisualSink,
        x: f64,
        y: f64,
        dy: i32,
        dx: i32,
        timestamp: Instant,
    ) {
        use cursor_overlay::ScrollDirection;
        let mut next = self.clone();
        if let Some(event) = next.event.as_mut() {
            event.target = Some((x, y));
            event.scroll_direction = if dy > 0 {
                Some(ScrollDirection::Up)
            } else if dy < 0 {
                Some(ScrollDirection::Down)
            } else if dx > 0 {
                Some(ScrollDirection::Left)
            } else if dx < 0 {
                Some(ScrollDirection::Right)
            } else {
                None
            };
        }
        next.publish(sink, VisualPhase::Contact, timestamp);
    }
    pub(crate) fn pin(&mut self, window: Option<u32>) {
        if let Some(event) = self.event.as_mut() {
            event.window = window.map(u64::from);
        }
    }
    pub(crate) fn publish(
        &self,
        sink: &dyn PointerVisualSink,
        phase: VisualPhase,
        timestamp: Instant,
    ) {
        if cua_driver_core::session::is_session_ended(&self.key) {
            return;
        }
        if let Some(mut event) = self.event.clone() {
            if phase == VisualPhase::Contact && event.target.is_none() {
                return;
            }
            event.phase = phase;
            event.timestamp = timestamp;
            sink.publish(&self.key, event);
        }
    }
}

pub(crate) struct DeliveryVisualGuard<'a> {
    pub(crate) handle: PointerVisualHandle,
    sink: &'a dyn PointerVisualSink,
}
impl<'a> DeliveryVisualGuard<'a> {
    pub(crate) fn retarget(
        &mut self,
        registry: &super::CursorRegistry,
        target: Option<ResolvedPointerTarget>,
    ) {
        if cua_driver_core::session::is_session_ended(&self.handle.key) {
            return;
        }
        if let Some(event) = self.handle.event.as_mut() {
            event.target = target.map(|target| (target.x, target.y));
            event.bounds = target.and_then(|target| target.element_bounds);
            event.window = target.and_then(|target| target.window_id.map(u64::from));
            if let Some(target) = target {
                registry.update_position(&self.handle.key, target.x, target.y);
            }
        }
        self.handle
            .publish(self.sink, VisualPhase::Tracking, Instant::now());
    }
    pub(crate) fn text(
        registry: &super::CursorRegistry,
        sink: &'a dyn PointerVisualSink,
        key: &str,
        target: Option<ResolvedPointerTarget>,
    ) -> Self {
        let handle = emit_action_target(registry, sink, key, target, CursorAction::Text);
        handle.publish(sink, VisualPhase::Tracking, Instant::now());
        Self { handle, sink }
    }
}
impl Drop for DeliveryVisualGuard<'_> {
    fn drop(&mut self) {
        self.handle
            .publish(self.sink, VisualPhase::End, Instant::now());
    }
}

/// Accepted input is recorded at the actuator boundary, before effect readback.
/// One receipt spans semantic and native fallback, so contact is emitted at most once.
#[derive(Default)]
pub(crate) struct DeliveryReceipt(Mutex<ReceiptState>);
#[derive(Default)]
struct ReceiptState {
    accepted: Option<Instant>,
    visual: Option<(Arc<dyn PointerVisualSink>, PointerVisualHandle)>,
    approach: Option<std::sync::Weak<ApproachGuard>>,
    #[cfg(test)]
    before_fallback_upgrade: Option<Arc<dyn Fn() + Send + Sync>>,
    revalidate: Option<Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>>,
}
/// Invocation-owned registration. Dropping a cancelled call removes only its waiter.
pub(crate) struct ApproachGuard {
    sink: Arc<dyn PointerVisualSink>,
    handle: PointerVisualHandle,
}
impl Drop for ApproachGuard {
    fn drop(&mut self) {
        if let Some(event) = &self.handle.event {
            self.sink.release_target(&self.handle.key, event);
        }
    }
}

pub(crate) fn approach_refusal(
    error: impl std::fmt::Display,
) -> cua_driver_core::protocol::ToolResult {
    tracing::debug!(target: "cua_cursor_approach", stage = "refused", reason = %error,
        "click approach refused");
    cua_driver_core::protocol::ToolResult::error(format!(
        "Click approach refused before input: {error}"
    ))
    .with_structured(
        serde_json::json!({"code": "cursor_approach_unavailable", "effect": "refused"}),
    )
}

impl DeliveryReceipt {
    pub(crate) async fn prepare_click(&self) -> Result<Option<Arc<ApproachGuard>>, String> {
        let (sink, handle) = self
            .0
            .lock()
            .unwrap()
            .visual
            .clone()
            .ok_or("missing click visual")?;
        if !sink.approach_enabled(&handle.key) {
            return Ok(None);
        }
        if handle.key.is_empty() || cua_driver_core::session::is_session_ended(&handle.key) {
            return Err("click session is ended or missing".into());
        }
        let event = handle
            .event
            .as_ref()
            .filter(|e| e.target.is_some() && e.is_valid())
            .ok_or("enabled overlay has no trustworthy resolved click target")?
            .clone();
        let guard = Arc::new(ApproachGuard { sink, handle });
        let receiver = guard.sink.register_target(&guard.handle.key, &event)?;
        tokio::time::timeout(std::time::Duration::from_millis(250), receiver)
            .await
            .map_err(|_| "renderer did not present the click target within 250 ms")?
            .map_err(|_| "click approach cancelled, superseded, or surface invalidated")?;
        tracing::debug!(target: "cua_cursor_approach", stage = "ack_received", id = ?event.id,
            age_ms = event.timestamp.elapsed().as_secs_f64() * 1000.0,
            "click target submission acknowledged");
        guard.sink.target_current(&guard.handle.key, &event)?;
        self.0.lock().unwrap().approach = Some(Arc::downgrade(&guard));
        Ok(Some(guard))
    }

    /// Retaining the AX object here also owns it inside detached blocking input tasks.
    pub(crate) fn validate_ax_target(
        &self,
        pid: i32,
        window: u32,
        element: Arc<crate::ax::cache::RetainedElement>,
        target: Option<ResolvedPointerTarget>,
    ) {
        let window_frame = crate::windows::window_bounds_by_id(window);
        self.set_revalidation(move || {
            let expected = target.ok_or_else(|| anyhow::anyhow!("missing AX click target"))?;
            let ptr = element.as_ptr() as crate::ax::bindings::AXUIElementRef;
            let live = unsafe { crate::ax::bindings::element_screen_rect(ptr) }
                .and_then(|rect| ResolvedPointerTarget::from_bounds(window, rect));
            if live != Some(expected)
                || unsafe { crate::ax::exact_target::element_window_id(ptr) } != Some(window) {
                anyhow::bail!("AX click target moved or changed ownership during approach; take a fresh snapshot");
            }
            let frame = window_frame.as_ref().ok_or_else(|| anyhow::anyhow!("missing AX click window frame"))?;
            validate_live_window(pid, window, Some(frame))
        });
    }

    pub(crate) fn validate_pixel_frame(
        &self,
        pid: i32,
        window: Option<u32>,
        bounds: Option<crate::windows::WindowBounds>,
    ) {
        if let Some(window) = window {
            self.set_revalidation(move || {
                let bounds = bounds
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("missing click window frame"))?;
                validate_live_window(pid, window, Some(bounds))
            });
        }
    }

    pub(crate) fn set_revalidation(
        &self,
        check: impl Fn() -> anyhow::Result<()> + Send + Sync + 'static,
    ) {
        self.0.lock().unwrap().revalidate = Some(Arc::new(check));
    }

    pub(crate) fn ensure_current(&self) -> anyhow::Result<()> {
        let (approach, revalidate) = {
            let state = self.0.lock().unwrap();
            (state.approach.clone(), state.revalidate.clone())
        };
        if let Some(weak) = approach {
            let (sink, handle) = {
                let guard = weak
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("click approach cancelled"))?;
                (guard.sink.clone(), guard.handle.clone())
            };
            // Readback must not extend the invocation's registration lifetime.
            // If the caller aborts while AX blocks, its guard drops immediately
            // and the final ownership check below refuses the pending mutation.
            if cua_driver_core::session::is_session_ended(&handle.key) {
                anyhow::bail!("click session ended before input");
            }
            sink.target_current(&handle.key, handle.event.as_ref().unwrap())
                .map_err(anyhow::Error::msg)?;
            if let Some(check) = revalidate {
                check()?;
            }
            // AX/WindowServer readback can cross a concurrent publication. Check
            // ownership again after those reads, immediately before the actuator.
            sink.target_current(&handle.key, handle.event.as_ref().unwrap())
                .map_err(anyhow::Error::msg)?;
            if cua_driver_core::session::is_session_ended(&handle.key) {
                anyhow::bail!("click session ended during target revalidation");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn dispatch_checked<T>(
        &self,
        native: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ensure_current()?;
        self.dispatch(native)
    }

    pub(crate) fn attach(&self, sink: Arc<dyn PointerVisualSink>, handle: PointerVisualHandle) {
        let mut state = self.0.lock().unwrap();
        if let Some(timestamp) = state.accepted {
            handle.publish(sink.as_ref(), VisualPhase::Contact, timestamp);
        } else {
            state.visual = Some((sink, handle));
        }
    }
    pub(crate) fn accepted(&self) {
        self.accepted_at(Instant::now());
    }
    pub(crate) fn accepted_at(&self, timestamp: Instant) {
        let mut state = self.0.lock().unwrap();
        if state.accepted.is_some() {
            return;
        }
        state.accepted = Some(timestamp);
        if let Some((sink, handle)) = state.visual.take() {
            handle.publish(sink.as_ref(), VisualPhase::Contact, timestamp);
        }
    }
    #[cfg(test)]
    pub(crate) fn was_accepted(&self) -> bool {
        self.0.lock().unwrap().accepted.is_some()
    }
    #[cfg(test)]
    pub(crate) fn before_fallback_upgrade_for_test(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.0.lock().unwrap().before_fallback_upgrade = Some(Arc::new(hook));
    }

    /// Re-pin only the still-owned action after native activation settles.
    pub(crate) fn repin_current_target(&self) -> anyhow::Result<()> {
        self.ensure_current()?;
        let (guarded, visual) = {
            let state = self.0.lock().unwrap();
            (state.approach.is_some(), state.visual.clone())
        };
        // Disabled overlays keep immediate input and need no native ordering.
        if !guarded {
            return Ok(());
        }
        let (sink, handle) = visual.ok_or_else(|| anyhow::anyhow!("missing click visual"))?;
        sink.repin_target(
            &handle.key,
            handle
                .event
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing click action"))?,
        )
        .map_err(anyhow::Error::msg)
    }

    /// Mouse acceptance comes only from the primitive's actual target posts.
    pub(crate) fn dispatch_mouse<T>(
        &self,
        native: impl FnOnce(&mut dyn FnMut(Instant)) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ensure_current()?;
        native(&mut |timestamp| self.accepted_at(timestamp))
    }

    pub(crate) fn dispatch_mouse_at<T>(
        &self,
        registry: &super::CursorRegistry,
        x: f64,
        y: f64,
        window: u32,
        native: impl FnOnce(f64, f64, &mut dyn FnMut(Instant)) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.dispatch_at_admitted(registry, x, y, window, |x, y| {
            self.dispatch_mouse(|observed| native(x, y, observed))
        })
    }

    #[cfg(test)]
    pub(crate) fn dispatch_at<T>(
        &self,
        registry: &super::CursorRegistry,
        x: f64,
        y: f64,
        window: u32,
        native: impl FnOnce(f64, f64) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.dispatch_at_admitted(registry, x, y, window, |x, y| {
            self.dispatch(|| native(x, y))
        })
    }

    fn dispatch_at_admitted<T>(
        &self,
        registry: &super::CursorRegistry,
        x: f64,
        y: f64,
        window: u32,
        native: impl FnOnce(f64, f64) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ensure_current()?;
        #[cfg(test)]
        {
            let hook = self.0.lock().unwrap().before_fallback_upgrade.clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        {
            let state = self.0.lock().unwrap();
            if let Some(weak) = state.approach.as_ref() {
                let guard = weak
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("click approach cancelled"))?;
                let event = guard.handle.event.as_ref().unwrap();
                if event.target != Some((x, y)) || event.window != Some(u64::from(window)) {
                    anyhow::bail!("native fallback target changed after click approach; refusing input, take a fresh snapshot");
                }
                drop(state);
                drop(guard);
                self.ensure_current()?;
                return native(x, y);
            }
        }
        {
            let mut state = self.0.lock().unwrap();
            if let Some((sink, handle)) = state.visual.as_mut() {
                if !cua_driver_core::session::is_session_ended(&handle.key) {
                    if let (Some(target), Some(event)) =
                        (point(x, y, Some(window)), handle.event.as_mut())
                    {
                        // Keep the native fallback's exact coordinates and action ID.
                        // Its fresh center does not establish fresh element bounds.
                        event.target = Some((target.x, target.y));
                        event.window = Some(u64::from(window));
                        event.bounds = None;
                        sink.send(&handle.key, OverlayCommand::PinAbove(u64::from(window)));
                        registry.update_position(&handle.key, x, y);
                        handle.publish(sink.as_ref(), VisualPhase::Intent, Instant::now());
                    }
                }
            }
        }
        native(x, y)
    }

    #[cfg(test)]
    pub(crate) fn dispatch<T, E>(&self, native: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let result = native();
        if result.is_ok() {
            self.accepted();
        }
        result
    }
}

fn validate_live_window(
    pid: i32,
    window: u32,
    expected: Option<&crate::windows::WindowBounds>,
) -> anyhow::Result<()> {
    let windows = crate::windows::visible_windows();
    let live = windows
        .iter()
        .find(|w| {
            w.window_id == window
                && w.pid == pid
                && w.is_on_screen
                && w.on_current_space != Some(false)
        })
        .ok_or_else(|| {
            anyhow::anyhow!("click window is no longer visible or owned by the target process")
        })?;
    if let Some(expected) = expected {
        let a = &live.bounds;
        if (a.x, a.y, a.width, a.height)
            != (expected.x, expected.y, expected.width, expected.height)
        {
            anyhow::bail!("click window frame changed during approach; take a fresh snapshot");
        }
    }
    Ok(())
}

pub(crate) trait PointerVisualSink: Send + Sync {
    fn approach_enabled(&self, _key: &str) -> bool {
        true
    }
    fn register_target(
        &self,
        _key: &str,
        _event: &VisualEvent,
    ) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
        Err("renderer does not support click target acknowledgement".into())
    }
    fn target_current(&self, _key: &str, _event: &VisualEvent) -> Result<(), String> {
        Err("click target has no current presentation".into())
    }
    fn repin_target(&self, _key: &str, _event: &VisualEvent) -> Result<(), String> {
        Err("renderer does not support action-bound re-pin".into())
    }
    fn release_target(&self, _key: &str, _event: &VisualEvent) {}
    fn send(&self, key: &str, command: OverlayCommand);
    fn begin(&self, key: &str) -> Option<VisualActionId>;
    fn publish(&self, key: &str, event: VisualEvent);
}
/// A bounded publisher owned by one native tool invocation, including its child stages.
/// Classification uses the same authorized, normalized arguments as core admission.
/// No session history is consulted or retained by later invocations.
pub(crate) struct InvocationVisualSink {
    inner: Arc<dyn PointerVisualSink>,
    key: String,
    semantics: Option<cua_driver_contract::CursorSemantics>,
}
impl InvocationVisualSink {
    pub(crate) fn bind(
        tool: &str,
        args: &serde_json::Value,
        key: &str,
        inner: Arc<dyn PointerVisualSink>,
    ) -> Arc<dyn PointerVisualSink> {
        Arc::new(Self {
            inner,
            key: key.into(),
            semantics: cua_driver_contract::classify_cursor_semantics(tool, args),
        })
    }
}
impl PointerVisualSink for InvocationVisualSink {
    fn approach_enabled(&self, key: &str) -> bool {
        self.inner.approach_enabled(key)
    }
    fn register_target(
        &self,
        key: &str,
        event: &VisualEvent,
    ) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
        self.inner.register_target(key, event)
    }
    fn target_current(&self, key: &str, event: &VisualEvent) -> Result<(), String> {
        self.inner.target_current(key, event)
    }
    fn repin_target(&self, key: &str, event: &VisualEvent) -> Result<(), String> {
        self.inner.repin_target(key, event)
    }
    fn release_target(&self, key: &str, event: &VisualEvent) {
        self.inner.release_target(key, event);
    }
    fn send(&self, key: &str, command: OverlayCommand) {
        self.inner.send(key, command);
    }
    fn begin(&self, key: &str) -> Option<VisualActionId> {
        self.inner.begin(key)
    }
    fn publish(&self, key: &str, mut event: VisualEvent) {
        if key == self.key {
            if let Some(semantics) = self.semantics {
                event.modifiers = Some((semantics.delivery, semantics.target));
            }
        }
        self.inner.publish(key, event);
    }
}

pub(crate) struct OverlayVisualSink;
impl PointerVisualSink for OverlayVisualSink {
    fn approach_enabled(&self, key: &str) -> bool {
        super::overlay::approach_enabled(key)
    }
    fn register_target(
        &self,
        key: &str,
        event: &VisualEvent,
    ) -> Result<tokio::sync::oneshot::Receiver<()>, String> {
        super::overlay::register_target(key, event)
    }
    fn target_current(&self, key: &str, event: &VisualEvent) -> Result<(), String> {
        super::overlay::target_current(key, event)
    }
    fn repin_target(&self, key: &str, event: &VisualEvent) -> Result<(), String> {
        super::overlay::repin_target(key, event)
    }
    fn release_target(&self, key: &str, event: &VisualEvent) {
        super::overlay::release_target(key, event);
    }
    fn send(&self, key: &str, command: OverlayCommand) {
        super::overlay::send_command(key.into(), command);
    }
    fn begin(&self, key: &str) -> Option<VisualActionId> {
        super::overlay::begin_visual_action(key)
    }
    fn publish(&self, key: &str, event: VisualEvent) {
        super::overlay::publish_visual_event(key, event);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq)]
    pub enum Event {
        Pin(String, u64),
        Target(String, f64, f64),
        Contact(String, f64, f64),
        Bounds(String, Option<[f64; 4]>),
        Semantic(String),
    }

    #[derive(Default)]
    pub struct RecordingSink(pub Mutex<Vec<Event>>, pub Mutex<Vec<VisualEvent>>);

    impl PointerVisualSink for RecordingSink {
        // Legacy event-only fixtures explicitly model an overlay-disabled caller.
        fn approach_enabled(&self, _key: &str) -> bool {
            false
        }
        fn send(&self, key: &str, command: OverlayCommand) {
            let event = match command {
                OverlayCommand::PinAbove(wid) => Event::Pin(key.into(), wid),
                OverlayCommand::ClickPulse { x, y } => Event::Contact(key.into(), x, y),
                OverlayCommand::ShowFocusRect(rect) => Event::Bounds(key.into(), rect),
                OverlayCommand::BeginAction { .. } => Event::Semantic(key.into()),
                other => panic!("unexpected visual command: {other:?}"),
            };
            self.0.lock().unwrap().push(event);
        }
        fn begin(&self, key: &str) -> Option<VisualActionId> {
            super::super::overlay::begin_visual_action(key)
        }
        fn publish(&self, key: &str, event: VisualEvent) {
            self.1.lock().unwrap().push(event.clone());
            match event.phase {
                VisualPhase::Intent => self.0.lock().unwrap().push(match event.target {
                    Some((x, y)) => Event::Target(key.into(), x, y),
                    None => Event::Semantic(key.into()),
                }),
                VisualPhase::Contact => {
                    if let Some(rect) = event.bounds {
                        self.0
                            .lock()
                            .unwrap()
                            .push(Event::Bounds(key.into(), Some(rect)));
                    }
                    if let Some((x, y)) = event.target {
                        self.0
                            .lock()
                            .unwrap()
                            .push(Event::Contact(key.into(), x, y));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Event, RecordingSink};
    use super::*;
    use crate::cursor::CursorRegistry;

    #[test]
    fn correction_mouse_receipt_latches_first_post_time_through_partial_failure() {
        let receipt = DeliveryReceipt::default();
        let start = Instant::now() - std::time::Duration::from_millis(50);
        let result = receipt.dispatch_mouse(|observed| {
            crate::input::mouse::run_mouse_pairs(
                2,
                |pair| Ok(start + std::time::Duration::from_millis(pair as u64 * 20)),
                |_| Err::<Instant, _>(anyhow::anyhow!("up allocation failed")),
                observed,
                |_, _| assert_eq!(receipt.0.lock().unwrap().accepted, Some(start)),
            )
        });
        assert!(result.is_err());
        receipt.accepted_at(Instant::now());
        assert_eq!(receipt.0.lock().unwrap().accepted, Some(start));
        let empty = DeliveryReceipt::default();
        empty.dispatch_mouse(|_| Ok(())).unwrap();
        assert!(
            !empty.was_accepted(),
            "a successful helper with no post is not contact"
        );
    }

    #[test]
    fn slice_a_accepted_dispatch_and_fallback_publish_contact_once() {
        let registry = CursorRegistry::new();
        let sink = Arc::new(RecordingSink::default());
        let handle = emit_pointer_target(
            &registry,
            sink.as_ref(),
            "fallback-contact",
            point(20.0, 30.0, Some(42)),
        );
        let receipt = DeliveryReceipt::default();
        receipt.attach(sink.clone(), handle);
        assert!(receipt.dispatch(|| Err::<(), _>("refused")).is_err());
        assert!(!receipt.was_accepted());
        assert_eq!(sink.1.lock().unwrap().len(), 1);
        receipt.dispatch(|| Ok::<_, ()>(())).unwrap();
        let timestamp = sink.1.lock().unwrap().last().unwrap().timestamp;
        receipt.dispatch(|| Ok::<_, ()>(())).unwrap();
        let events = sink.1.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.phase == VisualPhase::Contact)
                .count(),
            1
        );
        assert_eq!(events.last().unwrap().timestamp, timestamp);
    }

    #[test]
    fn trusted_bounds_require_finite_positive_rectangle() {
        let target = ResolvedPointerTarget::from_bounds(42, [-100.0, 200.0, 40.0, 60.0]).unwrap();
        assert_eq!((target.x, target.y), (-80.0, 230.0));
        assert_eq!(target.window_id, Some(42));
        for rect in [
            [0.0, 0.0, 0.0, 40.0],
            [0.0, 0.0, 40.0, -1.0],
            [f64::NAN, 0.0, 40.0, 40.0],
            [0.0, f64::INFINITY, 40.0, 40.0],
            [f64::MAX, 0.0, f64::MAX, 40.0],
        ] {
            assert!(ResolvedPointerTarget::from_bounds(42, rect).is_none());
        }
    }

    #[tokio::test]
    async fn contact_uses_captured_bounds_and_target_even_if_registry_moves() {
        let registry = CursorRegistry::new();
        let sink = RecordingSink::default();
        let handle = emit_pointer_target(
            &registry,
            &sink,
            "visual-captured",
            ResolvedPointerTarget::from_bounds(42, [-100.0, 200.0, 40.0, 60.0]),
        );
        registry.update_position("visual-captured", 900.0, 800.0);
        emit_pointer_contact(&sink, handle);
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![
                Event::Pin("visual-captured".into(), 42),
                Event::Target("visual-captured".into(), -80.0, 230.0),
                Event::Bounds("visual-captured".into(), Some([-100.0, 200.0, 40.0, 60.0])),
                Event::Contact("visual-captured".into(), -80.0, 230.0),
            ]
        );
        let position = registry.get("visual-captured").unwrap().position.unwrap();
        assert_eq!((position.x, position.y), (900.0, 800.0));
    }

    #[tokio::test]
    async fn empty_and_ended_sessions_emit_nothing_and_do_not_resurrect() {
        let registry = CursorRegistry::new();
        let sink = RecordingSink::default();
        cua_driver_core::session::fire_session_end("visual-ended-task6");
        for key in ["", "visual-ended-task6"] {
            let handle = emit_pointer_target(
                &registry,
                &sink,
                key,
                ResolvedPointerTarget::from_bounds(42, [0.0, 0.0, 40.0, 60.0]),
            );
            emit_pointer_contact(&sink, handle);
            assert!(registry.get(key).is_none());
        }
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_end_between_intent_and_delivery_suppresses_contact() {
        let registry = CursorRegistry::new();
        let sink = RecordingSink::default();
        let handle = emit_pointer_target(
            &registry,
            &sink,
            "visual-ended-mid-task6",
            ResolvedPointerTarget::from_bounds(42, [0.0, 0.0, 40.0, 60.0]),
        );
        cua_driver_core::session::fire_session_end("visual-ended-mid-task6");
        registry.remove("visual-ended-mid-task6");
        emit_pointer_contact(&sink, handle);
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![
                Event::Pin("visual-ended-mid-task6".into(), 42),
                Event::Target("visual-ended-mid-task6".into(), 20.0, 30.0),
            ]
        );
        assert!(registry.get("visual-ended-mid-task6").is_none());
    }
}
