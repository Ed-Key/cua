//! Coalesced action presentation. Adapters synchronize only publication and take_pending.
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::{CursorAction, CursorKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VisualActionId {
    pub generation: u64,
    pub action: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualPhase {
    Intent,
    Contact,
    Tracking,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug)]
pub struct VisualEvent {
    pub id: VisualActionId,
    pub timestamp: Instant,
    pub target: Option<(f64, f64)>,
    pub window: Option<u64>,
    pub bounds: Option<[f64; 4]>,
    pub action: CursorAction,
    pub scroll_direction: Option<ScrollDirection>,
    pub phase: VisualPhase,
}

impl VisualEvent {
    pub fn is_valid(&self) -> bool {
        if let Some((x, y)) = self.target {
            if !x.is_finite() || !y.is_finite() {
                return false;
            }
        }
        self.phase == VisualPhase::End
            || self.target.is_some()
            || self.phase == VisualPhase::Intent
            || (self.phase == VisualPhase::Tracking && self.action == CursorAction::Text)
    }
}

#[derive(Clone, Debug)]
pub struct PublishedVisualEvent {
    pub order: u64,
    pub event: VisualEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualLifecycle {
    Remove,
    Revive,
}

#[derive(Default, Debug)]
pub struct PendingVisualState {
    pub lifecycle: Option<(u64, VisualLifecycle)>,
    pub latest: Option<PublishedVisualEvent>,
    pub contact: Option<PublishedVisualEvent>,
    pub end: Option<PublishedVisualEvent>,
}

#[derive(Default)]
pub struct VisualMailbox {
    order: u64,
    generation: u64,
    sessions: HashMap<CursorKey, Session>,
    ended: HashSet<CursorKey>,
    pending: HashMap<CursorKey, PendingVisualState>,
}
struct Session {
    generation: u64,
    next_action: u64,
    owner: Option<(VisualActionId, Instant, VisualPhase)>,
}

impl VisualMailbox {
    pub fn next_order(&mut self) -> u64 {
        self.order += 1;
        self.order
    }
    pub fn begin_action(&mut self, key: &str) -> Option<VisualActionId> {
        if key.is_empty() || self.ended.contains(key) {
            return None;
        }
        let session = self.sessions.entry(key.to_owned()).or_insert_with(|| {
            self.generation += 1;
            Session {
                generation: self.generation,
                next_action: 0,
                owner: None,
            }
        });
        session.next_action += 1;
        Some(VisualActionId {
            generation: session.generation,
            action: session.next_action,
        })
    }
    pub fn publish(&mut self, key: &str, event: VisualEvent) -> bool {
        if !event.is_valid() {
            return false;
        }
        let Some(session) = self.sessions.get_mut(key) else {
            return false;
        };
        if event.id.generation != session.generation
            || event.id.action == 0
            || event.id.action > session.next_action
        {
            return false;
        }
        // Action allocation determines ownership across producers. Timestamp and
        // phase regressions only constrain events belonging to the same action.
        if let Some((id, timestamp, phase)) = session.owner {
            if event.id < id
                || (event.id == id
                    && (event.timestamp < timestamp
                        || phase == VisualPhase::End
                        || (matches!(phase, VisualPhase::Contact | VisualPhase::Tracking)
                            && event.phase == VisualPhase::Intent)))
            {
                return false;
            }
        }
        let new_owner = session.owner.is_none_or(|(id, _, _)| id != event.id);
        session.owner = Some((event.id, event.timestamp, event.phase));
        let order = self.next_order();
        let pending = self.pending.entry(key.to_owned()).or_default();
        if new_owner {
            pending.latest = None;
            pending.contact = None;
            pending.end = None;
        }
        let published = PublishedVisualEvent { order, event };
        match published.event.phase {
            VisualPhase::Contact => pending.contact = Some(published),
            VisualPhase::End => pending.end = Some(published),
            VisualPhase::Intent | VisualPhase::Tracking => pending.latest = Some(published),
        }
        true
    }
    pub fn remove(&mut self, key: &str) {
        if key.is_empty() || key == "default" {
            return;
        }
        self.sessions.remove(key);
        self.ended.insert(key.to_owned());
        let order = self.next_order();
        self.pending.insert(
            key.to_owned(),
            PendingVisualState {
                lifecycle: Some((order, VisualLifecycle::Remove)),
                ..Default::default()
            },
        );
    }
    pub fn revive(&mut self, key: &str) {
        if !self.ended.remove(key) {
            return;
        }
        let order = self.next_order();
        self.pending.insert(
            key.to_owned(),
            PendingVisualState {
                lifecycle: Some((order, VisualLifecycle::Revive)),
                ..Default::default()
            },
        );
    }
    /// Detach under the producer lock, then sort, route and paint after releasing it.
    pub fn take_pending(&mut self) -> HashMap<CursorKey, PendingVisualState> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc, Mutex},
        time::Duration,
    };

    fn event(id: VisualActionId, phase: VisualPhase, timestamp: Instant, x: f64) -> VisualEvent {
        VisualEvent {
            id,
            phase,
            timestamp,
            target: Some((x, 30.0)),
            window: None,
            bounds: None,
            action: CursorAction::Click,
            scroll_direction: None,
        }
    }
    #[test]
    fn slice_a_fix_mailbox_newer_action_precedes_timestamp() {
        let mut m = VisualMailbox::default();
        let t = Instant::now();
        let a = m.begin_action("a").unwrap();
        let b = m.begin_action("a").unwrap();
        assert!(m.publish(
            "a",
            event(a, VisualPhase::Contact, t + Duration::from_millis(20), 20.0)
        ));
        assert!(m.publish(
            "a",
            event(b, VisualPhase::Intent, t + Duration::from_millis(10), 80.0)
        ));
        let batch = m.take_pending();
        assert!(batch["a"].contact.is_none());
        assert_eq!(
            batch["a"].latest.as_ref().unwrap().event.target,
            Some((80.0, 30.0))
        );
        assert!(!m.publish("a", event(b, VisualPhase::Tracking, t, 10.0)));
        assert!(!m.publish(
            "a",
            event(a, VisualPhase::Contact, t + Duration::from_millis(30), 20.0)
        ));
        assert!(m.take_pending().is_empty());
    }

    #[test]
    fn slice_a_fix_mailbox_phase_barrier_survives_tracking_and_drain() {
        for phase in [VisualPhase::Contact, VisualPhase::Tracking] {
            let mut m = VisualMailbox::default();
            let t = Instant::now();
            let id = m.begin_action("a").unwrap();
            assert!(m.publish("a", event(id, phase, t, 20.0)));
            assert!(!m.publish("a", event(id, VisualPhase::Intent, t, 10.0)));
            assert!(m.publish("a", event(id, VisualPhase::Tracking, t, 60.0)));
            m.take_pending();
            assert!(!m.publish("a", event(id, VisualPhase::Intent, t, 20.0)));
            assert!(!m.publish(
                "a",
                event(id, VisualPhase::Intent, t + Duration::from_millis(1), 20.0)
            ));
            assert!(m.publish("a", event(id, VisualPhase::Tracking, t, 80.0)));
            assert_eq!(
                m.take_pending()["a"].latest.as_ref().unwrap().event.target,
                Some((80.0, 30.0))
            );
        }
    }

    #[test]
    fn stalled_consumer_retains_constant_state_and_newest_of_ten_thousand() {
        let mut mailbox = VisualMailbox::default();
        let now = Instant::now();
        for n in 0..10_000 {
            let id = mailbox.begin_action("a").expect("active session");
            assert!(mailbox.publish("a", event(id, VisualPhase::Intent, now, n as f64)));
            assert_eq!(mailbox.pending.len(), 1);
            assert_eq!(mailbox.sessions.len(), 1);
            let pending = &mailbox.pending["a"];
            assert!(pending.contact.is_none() && pending.end.is_none());
        }
        let pending = mailbox.take_pending();
        assert_eq!(
            pending["a"].latest.as_ref().unwrap().event.target,
            Some((9999.0, 30.0))
        );
        assert!(mailbox.take_pending().is_empty());
    }
    #[test]
    fn newer_action_discards_undrained_contact_but_tracking_keeps_its_own_contact() {
        let mut mailbox = VisualMailbox::default();
        let t = Instant::now();
        let first = mailbox.begin_action("a").unwrap();
        mailbox.publish("a", event(first, VisualPhase::Intent, t, 10.0));
        mailbox.publish("a", event(first, VisualPhase::Contact, t, 10.0));
        mailbox.publish(
            "a",
            event(
                first,
                VisualPhase::Tracking,
                t + Duration::from_millis(1),
                20.0,
            ),
        );
        assert_eq!(
            mailbox.pending["a"].contact.as_ref().unwrap().event.target,
            Some((10.0, 30.0))
        );
        assert_eq!(
            mailbox.pending["a"].latest.as_ref().unwrap().event.target,
            Some((20.0, 30.0))
        );
        let second = mailbox.begin_action("a").unwrap();
        mailbox.publish(
            "a",
            event(
                second,
                VisualPhase::Intent,
                t + Duration::from_millis(2),
                30.0,
            ),
        );
        let batch = mailbox.take_pending();
        assert!(batch["a"].contact.is_none());
        assert_eq!(
            batch["a"].latest.as_ref().unwrap().event.target,
            Some((30.0, 30.0))
        );
    }

    #[test]
    fn ownership_and_timestamps_survive_draining() {
        let mut m = VisualMailbox::default();
        let t = Instant::now();
        let a = m.begin_action("a").unwrap();
        assert!(m.publish("a", event(a, VisualPhase::Intent, t, 1.0)));
        assert!(m.publish("a", event(a, VisualPhase::Contact, t, 1.0)));
        assert!(m.take_pending()["a"].contact.is_some());
        let b = m.begin_action("a").unwrap();
        assert!(m.publish(
            "a",
            event(b, VisualPhase::Intent, t + Duration::from_millis(20), 2.0)
        ));
        assert!(!m.publish(
            "a",
            event(a, VisualPhase::Contact, t + Duration::from_millis(30), 1.0)
        ));
        assert!(!m.publish("a", event(b, VisualPhase::Tracking, t, 1.0)));
        let batch = m.take_pending();
        assert!(batch["a"].contact.is_none());
        assert_eq!(
            batch["a"].latest.as_ref().unwrap().event.target,
            Some((2.0, 30.0))
        );
        assert!(!m.publish("a", event(b, VisualPhase::Contact, t, 1.0)));
    }
    #[test]
    fn removal_expires_state_and_revive_requires_fresh_generation() {
        let mut m = VisualMailbox::default();
        let t = Instant::now();
        let old = m.begin_action("a").unwrap();
        m.publish("a", event(old, VisualPhase::Intent, t, 1.0));
        m.remove("a");
        assert!(m.sessions.is_empty());
        assert!(m.begin_action("a").is_none());
        assert!(!m.publish("a", event(old, VisualPhase::Contact, t, 1.0)));
        m.revive("a");
        let fresh = m.begin_action("a").unwrap();
        assert_ne!(old.generation, fresh.generation);
        assert!(!m.publish("a", event(old, VisualPhase::Contact, t, 1.0)));
        assert!(m.publish("a", event(fresh, VisualPhase::Intent, t, 2.0)));
        let batch = m.take_pending();
        assert_eq!(batch["a"].lifecycle.unwrap().1, VisualLifecycle::Revive);
        assert_eq!(
            batch["a"].latest.as_ref().unwrap().event.target,
            Some((2.0, 30.0))
        );
        let default = m.begin_action("default").unwrap();
        m.remove("default");
        assert!(m.publish("default", event(default, VisualPhase::Intent, t, 5.0)));
    }
    #[test]
    fn detached_blocked_renderer_does_not_block_producer() {
        let mailbox = Arc::new(Mutex::new(VisualMailbox::default()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let consumer = mailbox.clone();
        let renderer = std::thread::spawn(move || {
            let _batch = consumer.lock().unwrap().take_pending();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            let mut m = mailbox.lock().unwrap();
            let id = m.begin_action("a").unwrap();
            done_tx
                .send(m.publish("a", event(id, VisualPhase::Intent, Instant::now(), 4.0)))
                .unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        renderer.join().unwrap();
        producer.join().unwrap();
        assert_eq!(result.unwrap(), true);
    }
    #[test]
    fn semantic_intent_and_text_delivery_never_invent_contact_coordinates() {
        let mut m = VisualMailbox::default();
        let id = m.begin_action("a").unwrap();
        let mut e = event(id, VisualPhase::Intent, Instant::now(), 1.0);
        e.target = None;
        assert!(m.publish("a", e.clone()));
        e.phase = VisualPhase::Contact;
        assert!(!m.publish("a", e.clone()));
        e.phase = VisualPhase::Tracking;
        assert!(!m.publish("a", e.clone()));
        e.action = CursorAction::Text;
        assert!(m.publish("a", e));
        assert_eq!(
            m.take_pending()["a"].latest.as_ref().unwrap().event.target,
            None
        );
    }
}
