//! Per-browser reconnect leadership.
//!
//! A public session is not browser identity. Reconnect callers therefore
//! single-flight on the approved process fingerprint while unrelated browsers
//! remain independent. The key deliberately ignores the endpoint: a prepare
//! that moves a session to another endpoint of the same browser must still
//! serialize with a reconnect of the old one, or each acts on a generation
//! the other already replaced.

use super::keyed_gates::KeyedGates;
use super::types::ProcessFingerprint;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReconnectKey {
    pid: i64,
    start_time: Option<u64>,
    executable: Option<String>,
}

impl ReconnectKey {
    pub fn new(fingerprint: &ProcessFingerprint) -> Self {
        Self {
            pid: fingerprint.pid,
            start_time: fingerprint.start_time,
            executable: fingerprint.executable.clone(),
        }
    }
}

pub(crate) type ReconnectGates = KeyedGates<ReconnectKey>;
