//! A capacity-one owning handoff. Neither side borrows the published value.
use std::marker::PhantomData;
use std::sync::atomic::{AtomicPtr, Ordering};

pub(super) struct Latest<T> {
    value: AtomicPtr<T>,
    // Shared access transfers ownership, so both Send and Sync require T: Send.
    // Like Mutex, no shared reference to T is exposed, so T need not be Sync.
    owns: PhantomData<std::sync::Mutex<T>>,
}
impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self {
            value: AtomicPtr::new(std::ptr::null_mut()),
            owns: PhantomData,
        }
    }
}
impl<T> Latest<T> {
    pub(super) fn publish(&self, value: T) {
        let previous = self
            .value
            .swap(Box::into_raw(Box::new(value)), Ordering::AcqRel);
        if !previous.is_null() {
            // Each swap transfers exclusive ownership of the previous allocation.
            // No reference to a published allocation is ever exposed.
            unsafe {
                drop(Box::from_raw(previous));
            }
        }
    }
    pub(super) fn take(&self) -> Option<T> {
        let value = self.value.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if value.is_null() {
            None
        } else {
            // This swap is the only consumer of this particular allocation.
            Some(unsafe { *Box::from_raw(value) })
        }
    }
}
impl<T> Drop for Latest<T> {
    fn drop(&mut self) {
        // Exclusive access to self excludes concurrent publication or consumption.
        drop(self.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Arc};
    #[test]
    fn renderer_correction_latest_auto_traits_require_send_not_sync() {
        use static_assertions::{assert_impl_all, assert_not_impl_any};
        use std::cell::Cell;
        use std::sync::MutexGuard;

        assert_impl_all!(Cell<()>: Send);
        assert_not_impl_any!(Cell<()>: Sync);
        assert_impl_all!(Latest<Cell<()>>: Send, Sync);

        assert_impl_all!(MutexGuard<'static, ()>: Sync);
        assert_not_impl_any!(MutexGuard<'static, ()>: Send);
        assert_not_impl_any!(Latest<MutexGuard<'static, ()>>: Send, Sync);
    }

    struct Counted(usize, Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[test]
    fn renderer_correction_latest_owns_and_drops_each_value_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let slot = Latest::default();
        for n in 0..1000 {
            slot.publish(Counted(n, drops.clone()));
        }
        assert_eq!(drops.load(Ordering::Relaxed), 999);
        let value = slot.take().unwrap();
        assert_eq!(value.0, 999);
        assert!(slot.take().is_none());
        drop(value);
        slot.publish(Counted(1000, drops.clone()));
        drop(slot);
        assert_eq!(drops.load(Ordering::Relaxed), 1001);
    }
    #[test]
    fn renderer_correction_latest_concurrent_transfer_drops_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let slot = Latest::default();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 0..1000 {
                    slot.publish(Counted(n, drops.clone()));
                }
            });
            scope.spawn(|| {
                for _ in 0..1000 {
                    drop(slot.take());
                }
            });
        });
        drop(slot);
        assert_eq!(drops.load(Ordering::Relaxed), 1000);
    }
}
