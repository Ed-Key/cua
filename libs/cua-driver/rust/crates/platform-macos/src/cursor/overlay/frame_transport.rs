//! Main-queue frame transport. Native objects are created only by the consumer.
use super::DisplayId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(super) const MAX_FRAME_AGE: Duration = Duration::from_millis(64);

#[derive(Clone, Copy)]
pub(super) struct Stamp {
    pub generation: u64,
    pub revision: u64,
    pub painted_at: Instant,
}
impl Stamp {
    pub fn current(self, generation: u64, revision: u64, now: Instant) -> bool {
        self.generation == generation
            && self.revision == revision
            && now.saturating_duration_since(self.painted_at) < MAX_FRAME_AGE
    }
}

pub(super) struct Mailbox<T> {
    pending: HashMap<DisplayId, T>,
    scheduled: bool,
}
impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            scheduled: false,
        }
    }
}
impl<T> Mailbox<T> {
    pub fn push(&mut self, display: DisplayId, frame: T) -> bool {
        self.pending.insert(display, frame);
        !std::mem::replace(&mut self.scheduled, true)
    }
    pub fn take(&mut self) -> Vec<(DisplayId, T)> {
        std::mem::take(&mut self.pending).into_iter().collect()
    }
    pub fn finish(&mut self) -> bool {
        self.scheduled = !self.pending.is_empty();
        self.scheduled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Pixels {
        live: Arc<AtomicUsize>,
        value: usize,
    }
    impl Drop for Pixels {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }
    fn pixels(live: &Arc<AtomicUsize>, value: usize) -> Pixels {
        live.fetch_add(1, Ordering::SeqCst);
        Pixels {
            live: live.clone(),
            value,
        }
    }
    #[test]
    fn correction_stalled_main_queue_retains_only_latest_frame_per_display() {
        let live = Arc::new(AtomicUsize::new(0));
        let mut queue = Mailbox::default();
        assert!(queue.push(1, pixels(&live, 1)));
        for index in 2..1000 {
            assert!(!queue.push(1, pixels(&live, index)));
        }
        assert!(!queue.push(2, pixels(&live, 9)));
        assert_eq!(
            live.load(Ordering::SeqCst),
            2,
            "replaced pixels must be released immediately"
        );
        // Remove the painted session while main is held: a clear replaces art.
        assert!(!queue.push(1, pixels(&live, 0)));
        let active = queue.take();
        assert_eq!(active.len(), 2);
        assert_eq!(active.iter().find(|(d, _)| *d == 1).unwrap().1.value, 0);
        // Updates during submission get one subsequent drain, no lost wakeup.
        assert!(!queue.push(1, pixels(&live, 1000)));
        assert!(queue.finish());
        drop(active);
        assert_eq!(live.load(Ordering::SeqCst), 1);
        let last = queue.take();
        assert!(!queue.finish());
        drop(last);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(queue.push(1, pixels(&live, 4)));
    }
    #[test]
    fn correction_frame_admission_rejects_lifecycle_revision_and_age() {
        let start = Instant::now();
        let frame = Stamp {
            generation: 1,
            revision: 8,
            painted_at: start,
        };
        assert!(frame.current(1, 8, start));
        assert!(!frame.current(2, 8, start));
        assert!(
            !frame.current(1, 9, start),
            "supersession/removal invalidates pending pixels"
        );
        assert!(
            !frame.current(1, 8, start + MAX_FRAME_AGE),
            "old animation is not replayed"
        );
    }
}
