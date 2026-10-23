//! Retained host focus beliefs, separate from temporary preparation and recipients.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::ax::enablement::ProcessStartStamp;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: i32,
    pub(crate) start_stamp: ProcessStartStamp,
}

/// Values come from canonical dispatch ownership, never a public session label.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FocusOwner {
    pub(crate) runtime: String,
    pub(crate) session: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BeliefEvidence {
    Unknown,
    Posted,
    Observed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FocusSnapshot {
    pub(crate) believed_active: Option<bool>,
    pub(crate) believed_focus: Option<bool>,
    pub(crate) active_evidence: BeliefEvidence,
    pub(crate) focus_evidence: BeliefEvidence,
    pub(crate) actual_foreground: Option<bool>,
    pub(crate) observation_available: bool,
    pub(crate) retired: bool,
}

impl Default for FocusSnapshot {
    fn default() -> Self {
        Self {
            believed_active: None,
            believed_focus: None,
            active_evidence: BeliefEvidence::Unknown,
            focus_evidence: BeliefEvidence::Unknown,
            actual_foreground: None,
            observation_available: false,
            retired: false,
        }
    }
}

impl FocusSnapshot {
    pub(crate) fn can_skip_preparation(self) -> bool {
        !self.retired && self.observation_available && self.believed_focus == Some(true)
    }

    fn invalidate_beliefs(&mut self) {
        self.believed_active = None;
        self.believed_focus = None;
        self.active_evidence = BeliefEvidence::Unknown;
        self.focus_evidence = BeliefEvidence::Unknown;
    }
}

/// Native callbacks retain only this context; they never need the async host lock.
pub(crate) struct HostFocusContext {
    host: ProcessIdentity,
    generation: u64,
    state: Mutex<FocusSnapshot>,
}

impl HostFocusContext {
    fn new(host: ProcessIdentity, generation: u64) -> Self {
        Self {
            host,
            generation,
            state: Mutex::new(FocusSnapshot::default()),
        }
    }

    pub(crate) fn identity(&self) -> ProcessIdentity {
        self.host
    }

    pub(crate) fn host_pid(&self) -> i32 {
        self.host.pid
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn snapshot(&self) -> FocusSnapshot {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.snapshot().retired
    }

    fn update(&self, change: impl FnOnce(&mut FocusSnapshot)) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.retired {
            return false;
        }
        change(&mut state);
        true
    }

    pub(crate) fn set_observation_available(&self, available: bool) -> bool {
        self.update(|state| {
            state.observation_available = available;
            if !available {
                state.invalidate_beliefs();
            }
        })
    }

    pub(crate) fn invalidate_beliefs(&self) -> bool {
        self.update(FocusSnapshot::invalidate_beliefs)
    }

    pub(crate) fn mark_activation_posted(&self) -> bool {
        self.set_beliefs(true, BeliefEvidence::Posted)
    }

    pub(crate) fn mark_key_focus_returned_posted(&self) -> bool {
        self.update(|state| {
            state.believed_focus = Some(true);
            state.focus_evidence = BeliefEvidence::Posted;
        })
    }

    pub(crate) fn mark_deactivation_posted(&self) -> bool {
        self.set_beliefs(false, BeliefEvidence::Posted)
    }

    fn set_beliefs(&self, active: bool, evidence: BeliefEvidence) -> bool {
        self.update(|state| {
            state.believed_active = Some(active);
            state.believed_focus = Some(active);
            state.active_evidence = evidence;
            state.focus_evidence = evidence;
        })
    }

    /// Recognized native activation/deactivation events update both belief bytes.
    pub(crate) fn observe_activation(&self, event_type: u32, subtype: i64) -> bool {
        match (event_type, subtype) {
            (20, _) | (13, 1) => self.set_beliefs(true, BeliefEvidence::Observed),
            (19, _) | (13, 2) => self.set_beliefs(false, BeliefEvidence::Observed),
            _ => false,
        }
    }

    pub(crate) fn observe_actual_foreground(&self, foreground: Option<ProcessIdentity>) -> bool {
        self.update(|state| {
            state.actual_foreground = foreground.map(|identity| identity == self.host)
        })
    }

    fn retire(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retired = true;
    }
}

struct Entry {
    context: Arc<HostFocusContext>,
    owners: HashSet<FocusOwner>,
}

#[derive(Default)]
struct RegistryState {
    entries: HashMap<ProcessIdentity, Entry>,
    next_generation: u64,
}

#[derive(Default)]
pub(crate) struct LogicalFocusContexts {
    inner: Mutex<RegistryState>,
}

impl LogicalFocusContexts {
    /// Caller cleans replaced contexts outside the registry lock before preparing input.
    pub(crate) fn acquire(
        &self,
        owner: FocusOwner,
        host: ProcessIdentity,
    ) -> (Arc<HostFocusContext>, Vec<Arc<HostFocusContext>>) {
        let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut retired = Vec::new();
        registry.entries.retain(|identity, entry| {
            if identity.pid == host.pid && *identity != host {
                entry.context.retire();
                retired.push(entry.context.clone());
                false
            } else {
                true
            }
        });
        let RegistryState {
            entries,
            next_generation,
        } = &mut *registry;
        let entry = entries.entry(host).or_insert_with(|| {
            *next_generation = next_generation
                .checked_add(1)
                .expect("logical focus generation exhausted");
            Entry {
                context: Arc::new(HostFocusContext::new(host, *next_generation)),
                owners: HashSet::new(),
            }
        });
        entry.owners.insert(owner);
        (entry.context.clone(), retired)
    }

    pub(crate) fn retire_session(
        &self,
        runtime: &str,
        session: &str,
    ) -> Vec<Arc<HostFocusContext>> {
        self.retire_owners(|owner| {
            owner.runtime == runtime && owner.session.as_deref() == Some(session)
        })
    }

    pub(crate) fn retire_runtime(&self, runtime: &str) -> Vec<Arc<HostFocusContext>> {
        self.retire_owners(|owner| owner.runtime == runtime)
    }

    pub(crate) fn retire_all(&self) -> Vec<Arc<HostFocusContext>> {
        self.retire_owners(|_| true)
    }

    fn retire_owners(&self, remove: impl Fn(&FocusOwner) -> bool) -> Vec<Arc<HostFocusContext>> {
        let mut registry = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut retired = Vec::new();
        registry.entries.retain(|_, entry| {
            entry.owners.retain(|owner| !remove(owner));
            if entry.owners.is_empty() {
                entry.context.retire();
                retired.push(entry.context.clone());
                false
            } else {
                true
            }
        });
        retired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(stamp: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid: 91_201,
            start_stamp: (stamp, 0),
        }
    }

    fn owner(runtime: &str, session: Option<&str>) -> FocusOwner {
        FocusOwner {
            runtime: runtime.into(),
            session: session.map(str::to_owned),
        }
    }

    #[test]
    fn cached_object_loses_logical_focus_after_observed_deactivation() {
        let registry = LogicalFocusContexts::default();
        let (context, _) = registry.acquire(owner("runtime", Some("session")), host(1));
        assert_eq!(context.snapshot(), FocusSnapshot::default());
        context.set_observation_available(true);
        context.mark_activation_posted();
        assert_eq!(context.snapshot().active_evidence, BeliefEvidence::Posted);
        assert!(context.snapshot().can_skip_preparation());
        context.observe_actual_foreground(Some(ProcessIdentity {
            pid: 99,
            start_stamp: (2, 0),
        }));
        assert_eq!(context.snapshot().actual_foreground, Some(false));
        assert!(context.observe_activation(13, 2));
        let (same, retired) = registry.acquire(owner("runtime", Some("session")), host(1));
        assert!(Arc::ptr_eq(&context, &same));
        assert!(retired.is_empty());
        assert!(!same.snapshot().can_skip_preparation());
        assert_eq!(same.snapshot().active_evidence, BeliefEvidence::Observed);
        assert_eq!(same.snapshot().actual_foreground, Some(false));
    }

    #[test]
    fn native_events_and_observer_loss_do_not_conflate_real_and_believed_focus() {
        let registry = LogicalFocusContexts::default();
        let (context, _) = registry.acquire(owner("r", None), host(1));
        context.observe_actual_foreground(Some(host(1)));
        context.mark_activation_posted();
        assert!(!context.snapshot().can_skip_preparation());
        context.set_observation_available(true);
        assert!(context.observe_activation(19, 0));
        assert_eq!(context.snapshot().actual_foreground, Some(true));
        assert!(context.observe_activation(20, 0));
        assert!(!context.observe_activation(13, 99));
        context.set_observation_available(false);
        assert_eq!(context.snapshot().believed_focus, None);
        context.mark_key_focus_returned_posted();
        assert_eq!(context.snapshot().focus_evidence, BeliefEvidence::Posted);
        assert!(!context.snapshot().can_skip_preparation());
    }

    #[test]
    fn only_last_matching_owner_retirement_returns_context_for_cleanup() {
        let registry = LogicalFocusContexts::default();
        let (context, _) = registry.acquire(owner("a", Some("shared-label")), host(1));
        let (same, _) = registry.acquire(owner("b", Some("shared-label")), host(1));
        assert!(Arc::ptr_eq(&context, &same));
        assert!(registry.retire_session("a", "shared-label").is_empty());
        assert!(!context.is_retired());
        assert!(registry.retire_runtime("a").is_empty());
        let retired = registry.retire_runtime("b");
        assert_eq!(retired.len(), 1);
        assert!(Arc::ptr_eq(&context, &retired[0]));
        assert!(context.is_retired());
        assert!(registry.retire_runtime("b").is_empty());
    }

    #[test]
    fn pid_reuse_retires_old_context_and_stale_callbacks_cannot_update_it() {
        let registry = LogicalFocusContexts::default();
        let (old, _) = registry.acquire(owner("r", None), host(1));
        old.mark_activation_posted();
        let (new, retired) = registry.acquire(owner("r", None), host(2));
        assert_eq!(retired.len(), 1);
        assert_eq!(new.identity(), host(2));
        assert_eq!(new.host_pid(), host(2).pid);
        assert_ne!(old.generation(), new.generation());
        assert!(!old.observe_activation(20, 0));
        assert!(!old.mark_deactivation_posted());
        assert!(!old.invalidate_beliefs());
        assert_eq!(new.snapshot(), FocusSnapshot::default());
    }

    #[test]
    fn model_gaps_do_not_expire_context_and_reacquisition_after_retirement_is_fresh() {
        let registry = LogicalFocusContexts::default();
        let (context, _) = registry.acquire(owner("r", None), host(1));
        context.set_observation_available(true);
        context.mark_activation_posted();
        for _ in 0..100 {
            let (same, retired) = registry.acquire(owner("r", None), host(1));
            assert!(Arc::ptr_eq(&context, &same));
            assert!(retired.is_empty());
            assert!(same.snapshot().can_skip_preparation());
        }
        registry.retire_runtime("r");
        let (new, _) = registry.acquire(owner("r", None), host(1));
        assert_ne!(context.generation(), new.generation());
        assert!(!context.observe_activation(13, 1));
        assert_eq!(new.snapshot(), FocusSnapshot::default());
    }
}
