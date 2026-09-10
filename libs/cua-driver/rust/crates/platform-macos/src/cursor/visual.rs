//! Resolved action visuals published without renderer acknowledgements.

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
            phase: VisualPhase::Intent,
        };
        sink.publish(key, event.clone());
        handle.event = Some(event);
    }
    handle
}

pub(crate) fn begin_pointer_action(
    registry: &super::CursorRegistry,
    key: &str,
    target: Option<ResolvedPointerTarget>,
    action: CursorAction,
) -> Arc<DeliveryReceipt> {
    let sink: Arc<dyn PointerVisualSink> = Arc::new(OverlayVisualSink);
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
    fn publish(&self, sink: &dyn PointerVisualSink, phase: VisualPhase, timestamp: Instant) {
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

/// Accepted input is recorded at the actuator boundary, before effect readback.
/// One receipt spans semantic and native fallback, so contact is emitted at most once.
#[derive(Default)]
pub(crate) struct DeliveryReceipt(Mutex<ReceiptState>);
#[derive(Default)]
struct ReceiptState {
    accepted: Option<Instant>,
    visual: Option<(Arc<dyn PointerVisualSink>, PointerVisualHandle)>,
}
impl DeliveryReceipt {
    pub(crate) fn attach(&self, sink: Arc<dyn PointerVisualSink>, handle: PointerVisualHandle) {
        let mut state = self.0.lock().unwrap();
        if let Some(timestamp) = state.accepted {
            handle.publish(sink.as_ref(), VisualPhase::Contact, timestamp);
        } else {
            state.visual = Some((sink, handle));
        }
    }
    pub(crate) fn accepted(&self) {
        let mut state = self.0.lock().unwrap();
        if state.accepted.is_some() {
            return;
        }
        let timestamp = Instant::now();
        state.accepted = Some(timestamp);
        if let Some((sink, handle)) = state.visual.take() {
            handle.publish(sink.as_ref(), VisualPhase::Contact, timestamp);
        }
    }
    pub(crate) fn was_accepted(&self) -> bool {
        self.0.lock().unwrap().accepted.is_some()
    }
    pub(crate) fn dispatch<T, E>(&self, native: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let result = native();
        if result.is_ok() {
            self.accepted();
        }
        result
    }
}

pub(crate) trait PointerVisualSink: Send + Sync {
    fn send(&self, key: &str, command: OverlayCommand);
    fn begin(&self, key: &str) -> Option<VisualActionId>;
    fn publish(&self, key: &str, event: VisualEvent);
}
pub(crate) struct OverlayVisualSink;
impl PointerVisualSink for OverlayVisualSink {
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
