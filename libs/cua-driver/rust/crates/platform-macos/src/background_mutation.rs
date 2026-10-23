//! Per-process serialization for exact-target background mutations.
//!
//! macOS keyboard delivery and focus suppression are process-scoped. Two
//! concurrent window-addressed mutations for one pid must not interleave their
//! fresh target proof, focus changes, dispatch, restoration, or verification.
//! Different processes remain independent.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

tokio::task_local! {
    static HELD_PID: i32;
    static HELD_GUARD: Arc<OwnedMutexGuard<()>>;
    static HELD_FOCUS: Option<Arc<crate::background_focus::HostController>>;
}

fn process_locks() -> &'static Mutex<HashMap<i32, Weak<AsyncMutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<i32, Weak<AsyncMutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn process_lock(pid: i32) -> Arc<AsyncMutex<()>> {
    let mut locks = process_locks()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&pid).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(pid, Arc::downgrade(&lock));
    lock
}

/// Acquire exclusive background-mutation ownership for one process.
///
/// The returned guard must live from immediately before fresh fact gathering
/// until dispatch, focus restoration, and postcondition verification finish.
pub(crate) async fn acquire(pid: i32) -> OwnedMutexGuard<()> {
    process_lock(pid).lock_owned().await
}

/// Avoid waiting for another process while a host mutation lease is held.
pub(crate) fn try_acquire(pid: i32) -> Result<OwnedMutexGuard<()>, tokio::sync::TryLockError> {
    process_lock(pid).try_lock_owned()
}

/// Run one nested tool call with non-forgeable proof that its caller already
/// owns this process's mutation lease.
pub(crate) async fn with_held_lease<T>(
    pid: i32,
    guard: Arc<OwnedMutexGuard<()>>,
    future: impl Future<Output = T>,
) -> T {
    HELD_FOCUS
        .scope(None, HELD_PID.scope(pid, HELD_GUARD.scope(guard, future)))
        .await
}

/// Nested focus mutations retain their trusted controller through blocking delivery.
pub(crate) async fn with_held_focus<T>(
    focus: Arc<crate::background_focus::HostController>,
    guard: Arc<OwnedMutexGuard<()>>,
    future: impl Future<Output = T>,
) -> T {
    let pid = focus.host_pid();
    HELD_FOCUS
        .scope(
            Some(focus),
            HELD_PID.scope(pid, HELD_GUARD.scope(guard, future)),
        )
        .await
}

/// Nested pixel-focus work must retain its parent lease after async cancellation.
pub(crate) fn spawn_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let held = HELD_GUARD.try_with(Arc::clone).ok();
    let focus = HELD_FOCUS.try_with(Clone::clone).ok().flatten();
    tokio::task::spawn_blocking(move || {
        // Reverse local drop order releases coordination before final retirement.
        let _focus_owner = focus;
        let _held_lease = held;
        operation()
    })
}

pub(crate) fn held_by_current_task(pid: i32) -> bool {
    HELD_PID
        .try_with(|held_pid| *held_pid == pid)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn same_pid_mutations_serialize_until_the_first_guard_drops() {
        let first = acquire(-91_001).await;
        let mut waiting = tokio::spawn(acquire(-91_001));

        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut waiting)
                .await
                .is_err(),
            "a second mutation for the same pid must remain queued"
        );
        drop(first);

        let second = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("same-pid waiter must proceed after the first guard drops")
            .expect("same-pid waiter task must not panic");
        drop(second);
    }

    #[tokio::test]
    async fn different_pids_do_not_block_each_other() {
        let first = acquire(-91_002).await;
        let second = tokio::time::timeout(Duration::from_millis(100), acquire(-91_003))
            .await
            .expect("independent pids must acquire concurrently");
        drop((first, second));
    }

    #[tokio::test]
    async fn nested_lease_proof_is_task_local_and_pid_bound() {
        assert!(!held_by_current_task(-91_004));
        with_held_lease(-91_004, Arc::new(acquire(-91_004).await), async {
            assert!(held_by_current_task(-91_004));
            assert!(!held_by_current_task(-91_005));
        })
        .await;
        assert!(!held_by_current_task(-91_004));
    }
}
