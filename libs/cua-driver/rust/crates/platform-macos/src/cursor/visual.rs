//! Resolved click visuals. The transport remains replaceable by later Slice A work.

use async_trait::async_trait;
use cursor_overlay::OverlayCommand;

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

/// One resolved visual action. Consuming it prevents duplicate contact emission.
pub(crate) struct PointerVisualHandle {
    key: String,
    target: Option<ResolvedPointerTarget>,
}

/// Publish intent before input. This is not evidence of delivery.
pub(crate) async fn emit_pointer_target(
    registry: &super::CursorRegistry,
    sink: &dyn PointerVisualSink,
    key: &str,
    target: Option<ResolvedPointerTarget>,
) -> PointerVisualHandle {
    let handle = PointerVisualHandle {
        key: key.to_owned(),
        target,
    };
    if key.is_empty() || cua_driver_core::session::is_session_ended(key) {
        return handle;
    }
    if let Some(target) = target {
        if let Some(window_id) = target.window_id {
            sink.send(key, OverlayCommand::PinAbove(window_id as u64));
        }
        registry.update_position(key, target.x, target.y);
        sink.travel(key, target.x, target.y).await;
    } else {
        // A semantic action has no screen location without trustworthy bounds.
        sink.send(
            key,
            OverlayCommand::BeginAction {
                action: cursor_overlay::CursorAction::Click,
                delivery: None,
                target: None,
            },
        );
    }
    handle
}

/// Delivery feedback only. A contact cue does not certify an application effect.
pub(crate) fn emit_pointer_contact(sink: &dyn PointerVisualSink, handle: PointerVisualHandle) {
    if handle.key.is_empty() || cua_driver_core::session::is_session_ended(&handle.key) {
        return;
    }
    if let Some(target) = handle.target {
        if let Some(bounds) = target.element_bounds {
            sink.send(&handle.key, OverlayCommand::ShowFocusRect(Some(bounds)));
        }
        sink.send(
            &handle.key,
            OverlayCommand::ClickPulse {
                x: target.x,
                y: target.y,
            },
        );
    }
}

#[async_trait]
pub(crate) trait PointerVisualSink: Send + Sync {
    fn send(&self, key: &str, command: OverlayCommand);
    async fn travel(&self, key: &str, x: f64, y: f64);
}

pub(crate) struct OverlayVisualSink;

#[async_trait]
impl PointerVisualSink for OverlayVisualSink {
    fn send(&self, key: &str, command: OverlayCommand) {
        super::overlay::send_command(key.to_owned(), command);
    }

    async fn travel(&self, key: &str, x: f64, y: f64) {
        super::overlay::animate_cursor_to(key.to_owned(), x, y).await;
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
    pub struct RecordingSink(pub Mutex<Vec<Event>>);

    #[async_trait]
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
        async fn travel(&self, key: &str, x: f64, y: f64) {
            self.0.lock().unwrap().push(Event::Target(key.into(), x, y));
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
        )
        .await;
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
            )
            .await;
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
        )
        .await;
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
