//! Private, opt-in, fixed-size counters. No native calls, strings from users,
//! locks, or logging on AppKit main. Summary formatting happens at cleanup.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, OnceLock,
};

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(super) enum Event {
    OrderBusyRender,
    OrderBusyInbox,
    OrderMissingMap,
    OrderGeneration,
    OrderRevision,
    OrderControllerRoute,
    OrderCurrent,
    OrderSelected,
    OrderCached,
    OrderRejected,
    OrderRouteInvalidated,
    OrderRejectBusy,
    RoutePreBusy,
    RoutePostBusy,
    RouteApplyRejected,
    RoutePublished,
    SurfaceMissing,
    FrameQueued,
    FrameDrained,
    FramePreGeneration,
    FramePreBusy,
    FramePreRejected,
    FrameNativeStarted,
    FrameNativeReturned,
    FramePostBusy,
    FramePostRejected,
    FrameAckChecked,
    ImageUnavailable,
    StampQueuedGeneration,
    StampQueuedRevision,
    StampQueuedAge,
    StampQueuedPulse,
    StampPreGeneration,
    StampPreRevision,
    StampPreAge,
    StampPrePulse,
    StampPostGeneration,
    StampPostRevision,
    StampPostAge,
    StampPostPulse,
    AckGeneration,
    AckSurface,
    AckRoute,
    AckOwnership,
    AckRegistration,
    AckIdentity,
    AckExpired,
    AckSent,
    AckReceiverClosed,
}
const EVENTS: &[Event] = &[
    Event::OrderBusyRender,
    Event::OrderBusyInbox,
    Event::OrderMissingMap,
    Event::OrderGeneration,
    Event::OrderRevision,
    Event::OrderControllerRoute,
    Event::OrderCurrent,
    Event::OrderSelected,
    Event::OrderCached,
    Event::OrderRejected,
    Event::OrderRouteInvalidated,
    Event::OrderRejectBusy,
    Event::RoutePreBusy,
    Event::RoutePostBusy,
    Event::RouteApplyRejected,
    Event::RoutePublished,
    Event::SurfaceMissing,
    Event::FrameQueued,
    Event::FrameDrained,
    Event::FramePreGeneration,
    Event::FramePreBusy,
    Event::FramePreRejected,
    Event::FrameNativeStarted,
    Event::FrameNativeReturned,
    Event::FramePostBusy,
    Event::FramePostRejected,
    Event::FrameAckChecked,
    Event::ImageUnavailable,
    Event::StampQueuedGeneration,
    Event::StampQueuedRevision,
    Event::StampQueuedAge,
    Event::StampQueuedPulse,
    Event::StampPreGeneration,
    Event::StampPreRevision,
    Event::StampPreAge,
    Event::StampPrePulse,
    Event::StampPostGeneration,
    Event::StampPostRevision,
    Event::StampPostAge,
    Event::StampPostPulse,
    Event::AckGeneration,
    Event::AckSurface,
    Event::AckRoute,
    Event::AckOwnership,
    Event::AckRegistration,
    Event::AckIdentity,
    Event::AckExpired,
    Event::AckSent,
    Event::AckReceiverClosed,
];
#[derive(Clone, Copy)]
pub(super) enum FrameStage {
    Queued,
    Pre,
    Post,
}
impl FrameStage {
    pub fn rejection(self, generation: bool, revision: bool, pulse: bool) -> Event {
        use Event::*;
        let reasons = match self {
            Self::Queued => [
                StampQueuedGeneration,
                StampQueuedRevision,
                StampQueuedPulse,
                StampQueuedAge,
            ],
            Self::Pre => [
                StampPreGeneration,
                StampPreRevision,
                StampPrePulse,
                StampPreAge,
            ],
            Self::Post => [
                StampPostGeneration,
                StampPostRevision,
                StampPostPulse,
                StampPostAge,
            ],
        };
        reasons[if generation {
            0
        } else if revision {
            1
        } else if pulse {
            2
        } else {
            3
        }]
    }
}
#[derive(Clone, Default)]
pub(super) struct Trace(Option<Arc<[AtomicU64; EVENTS.len()]>>);
impl Trace {
    pub fn configured() -> Self {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        Self::new(
            *ENABLED.get_or_init(|| {
                std::env::var("CUA_PRIVATE_CONFIRMATION_TRACE").as_deref() == Ok("1")
            }),
        )
    }
    pub fn new(enabled: bool) -> Self {
        Self(enabled.then(|| Arc::new(std::array::from_fn(|_| AtomicU64::new(0)))))
    }
    pub fn record(&self, event: Event) {
        if let Some(counters) = &self.0 {
            let _ =
                counters[event as usize].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_add(1))
                });
        }
    }
    pub fn count(&self, event: Event) -> u64 {
        self.0
            .as_ref()
            .map_or(0, |c| c[event as usize].load(Ordering::Relaxed))
    }
}
impl std::fmt::Debug for Trace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_none() {
            return f.write_str("disabled");
        }
        let mut map = f.debug_map();
        for event in EVENTS {
            let count = self.count(*event);
            if count != 0 {
                map.entry(event, &count);
            }
        }
        map.finish()
    }
}
