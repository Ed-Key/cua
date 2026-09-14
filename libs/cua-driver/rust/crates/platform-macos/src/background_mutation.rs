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
    static OBSERVATION_LEASE: (i32, Arc<OwnedMutexGuard<()>>);
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
pub(crate) async fn acquire(pid: i32) -> Arc<OwnedMutexGuard<()>> {
    if let Some(lease) = observation_lease(pid) {
        return lease;
    }
    Arc::new(process_lock(pid).lock_owned().await)
}

/// Serialize before/action/after observation with the existing mutation owner.
/// Inner actuators may share ownership within this task, but must still gather
/// fresh target facts. This deliberately does not set HELD_PID, which is the
/// separate proof used by an already-admitted keyboard focus operation.
pub(crate) async fn with_observation_lease<T>(pid: i32, future: impl Future<Output = T>) -> T {
    let lease = acquire(pid).await;
    OBSERVATION_LEASE.scope((pid, lease), future).await
}

pub(crate) fn observation_lease(pid: i32) -> Option<Arc<OwnedMutexGuard<()>>> {
    OBSERVATION_LEASE
        .try_with(|(owner, lease)| (*owner == pid).then(|| lease.clone()))
        .ok()
        .flatten()
}

/// Execute a synchronous AX observation away from the async executor. A started
/// blocking task survives cancellation of its caller, so it retains ownership
/// until the actual read returns rather than until the await is dropped.
pub(crate) async fn observe_blocking<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let lease = OBSERVATION_LEASE.try_with(|(_, lease)| lease.clone()).ok();
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        read()
    })
    .await
}

/// Run one nested tool call with non-forgeable proof that its caller already
/// owns this process's mutation lease.
pub(crate) async fn with_held_lease<T>(pid: i32, future: impl Future<Output = T>) -> T {
    HELD_PID.scope(pid, future).await
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
    async fn observation_ownership_is_task_local_without_implying_input_admission() {
        const PID: i32 = -91_007;
        with_observation_lease(PID, async {
            assert!(!held_by_current_task(PID));
            assert!(observation_lease(PID).is_some());
            assert!(observation_lease(PID + 1).is_none());
            let inner = tokio::time::timeout(Duration::from_secs(1), acquire(PID))
                .await
                .expect("the inner actuator must share ownership without deadlocking");
            drop(inner);
            let (sees_lease, sees_admission) = tokio::spawn(async {
                (observation_lease(PID).is_some(), held_by_current_task(PID))
            })
            .await
            .unwrap();
            assert!(!sees_lease && !sees_admission);
        })
        .await;
        assert!(observation_lease(PID).is_none());
        drop(
            tokio::time::timeout(Duration::from_secs(1), acquire(PID))
                .await
                .expect("scope exit must release ownership"),
        );
    }

    #[tokio::test]
    async fn cancelled_observation_retains_ownership_until_blocking_read_finishes() {
        const PID: i32 = -91_006;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let (ended_tx, ended_rx) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(with_observation_lease(PID, async move {
            observe_blocking(move || {
                started_tx.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                ended_tx.send(()).unwrap();
            })
            .await
        }));
        started_rx.await.unwrap();
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        let mut next = tokio::spawn(acquire(PID));
        let queued = tokio::time::timeout(Duration::from_millis(30), &mut next)
            .await
            .is_err();
        finish_tx.send(()).unwrap();
        ended_rx.await.unwrap();
        if queued {
            drop(
                tokio::time::timeout(Duration::from_secs(1), next)
                    .await
                    .expect("next mutation must proceed after read finishes")
                    .unwrap(),
            );
        }
        assert!(
            queued,
            "cancelled request must not release an active AX read's ownership"
        );
    }

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
        with_held_lease(-91_004, async {
            assert!(held_by_current_task(-91_004));
            assert!(!held_by_current_task(-91_005));
        })
        .await;
        assert!(!held_by_current_task(-91_004));
    }
}
