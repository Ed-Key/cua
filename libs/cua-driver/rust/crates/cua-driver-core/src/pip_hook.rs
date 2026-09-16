//! PiP frame-push hook, registered once by `main.rs` when the
//! `--experimental-pip` flag is on argv.
//!
//! The trait + factory live in the `pip-preview` crate so the platform
//! backends can implement them without depending on `cua-driver-core`.
//! The dispatcher publishes capture requests without waiting for native
//! capture or presentation. One worker owns callback delivery; one replaceable
//! pending request prevents a slow preview from accumulating old actions.
//! The isolated observer receives only metadata. The legacy callback still
//! captures post-action frames on this worker for older embedders.
//!
//! The PNG bytes pushed through here come from the existing
//! screenshot callback used by the recording pipeline. Preview capture
//! happens independently and may show a later state than recording evidence.
//! A preview frame must not be used to verify a particular action's outcome.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use tokio::sync::watch;

use crate::recording::screenshot_for;

/// Synthesized per-call frame payload. Kept structurally identical
/// to `pip_preview::PipFrame`, duplicated here to keep `cua-driver-core`
/// from importing `pip-preview` (the dependency would be circular once
/// platform backends pull both crates in).
pub struct PipHookFrame {
    pub png_bytes: Vec<u8>,
    pub action_label: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug)]
pub struct PipCaptureRequest {
    pub window_id: Option<u64>,
    pub pid: Option<i64>,
    pub action_label: String,
    pub timestamp_ms: u64,
}

#[derive(Clone)]
struct PendingPreview {
    session: Option<String>,
    request: PipCaptureRequest,
}

/// Desired previews indexed by runtime-private session identity. Local callers
/// must not expose these keys as user-facing task names or log them.
pub type PipSessionSnapshot = BTreeMap<String, PipCaptureRequest>;

#[derive(Clone, Default)]
struct PreviewState {
    latest: Option<PendingPreview>,
    sessions: PipSessionSnapshot,
}

struct Publisher {
    sender: watch::Sender<PreviewState>,
    session_scoped: bool,
    session_snapshots: bool,
}
static PIP_REQUESTS: OnceLock<Publisher> = OnceLock::new();

enum Consumer {
    Legacy(Box<dyn Fn(PipHookFrame) + Send + Sync>),
    Observer(Box<dyn FnMut(PipCaptureRequest) -> bool + Send>),
    Sessions(Box<dyn FnMut(PipSessionSnapshot) -> bool + Send>),
}

/// Register the platform-side push callback. `main.rs` calls this
/// once after starting the PiP backend.
pub fn set_pip_push_fn(f: impl Fn(PipHookFrame) + Send + Sync + 'static) {
    start_worker(Consumer::Legacy(Box::new(f)));
}

/// Register an isolated observer's metadata publisher. Return false if the
/// observer closed. The publisher runs only on the dedicated preview worker.
pub fn set_pip_observer_fn(f: impl FnMut(PipCaptureRequest) -> bool + Send + 'static) {
    start_worker(Consumer::Observer(Box::new(f)));
}

/// Publish the latest desired state of every live preview session. A busy
/// observer may skip intermediate states, but never loses another session.
/// Removing an entry closes only that preview. An empty snapshot closes all.
pub fn set_pip_session_observer_fn(f: impl FnMut(PipSessionSnapshot) -> bool + Send + 'static) {
    start_worker(Consumer::Sessions(Box::new(f)));
}

fn start_worker(mut consumer: Consumer) {
    PIP_REQUESTS.get_or_init(|| {
        let (sender, mut receiver) = watch::channel(PreviewState::default());
        let session_scoped = !matches!(&consumer, Consumer::Legacy(_));
        let session_snapshots = matches!(&consumer, Consumer::Sessions(_));
        // Own the hook with the worker, so a closed observer cannot leave a
        // live cleanup callback behind. The legacy image callback cannot clear
        // its renderer and therefore retains its historical lifecycle.
        let cleanup = session_scoped.then(|| {
            let sender = sender.clone();
            crate::session::register_scoped_session_end_hook(move |session| {
                clear_session_preview(&sender, session, session_snapshots);
            })
        });
        let started = std::thread::Builder::new()
            .name("cua-pip-publisher".into())
            .spawn(move || {
                let _cleanup = cleanup;
                let runtime = match tokio::runtime::Builder::new_current_thread().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "PiP capture worker unavailable");
                        return;
                    }
                };
                runtime.block_on(async {
                    while receiver.changed().await.is_ok() {
                        // Release the watch borrow before any native or embedder
                        // work. Publishers must never wait for either callback.
                        let state = receiver.borrow_and_update().clone();
                        if let Consumer::Sessions(publish) = &mut consumer {
                            if !publish(state.sessions) {
                                break;
                            }
                            continue;
                        }
                        let Some(pending) = state.latest else {
                            continue;
                        };
                        let request = pending.request;
                        if let Consumer::Observer(publish) = &mut consumer {
                            if !publish(request) {
                                break;
                            }
                            continue;
                        }
                        let png = screenshot_for(request.window_id, request.pid);
                        if receiver.has_changed().unwrap_or(true) {
                            // Capture completed after another action selected a
                            // newer preview. Do not present the obsolete frame.
                            continue;
                        }
                        if let (Consumer::Legacy(f), Some(png_bytes)) = (&consumer, png) {
                            f(PipHookFrame {
                                png_bytes,
                                action_label: request.action_label,
                                timestamp_ms: request.timestamp_ms,
                            });
                        }
                    }
                });
            });
        if let Err(error) = started {
            tracing::warn!(%error, "PiP capture worker could not start");
        }
        Publisher {
            sender,
            session_scoped,
            session_snapshots,
        }
    });
}

/// True while the preview publisher is running. This is not an acknowledgement
/// that the helper rendered a frame. A closed publisher disables further work.
pub fn pip_enabled() -> bool {
    PIP_REQUESTS
        .get()
        .is_some_and(|publisher| !publisher.sender.is_closed())
}

/// The observer follows session-owned native actions. Legacy screenshot
/// consumers retain their original non-read-only tool eligibility.
pub fn uses_session_ownership() -> bool {
    PIP_REQUESTS
        .get()
        .is_some_and(|publisher| publisher.session_scoped)
}

/// Replace pending preview work. Capture and renderer callbacks run only on
/// the worker, never on the action path. Intermediate previews may be skipped.
pub fn request_pip_frame(
    window_id: Option<u64>,
    pid: Option<i64>,
    action_label: String,
    timestamp_ms: u64,
) {
    publish_preview(None, window_id, pid, action_label, timestamp_ms);
}

/// Publish a target owned by the registry's runtime-private session identity.
/// Public labels alone are not unique across independently authorized clients.
pub(crate) fn request_pip_frame_for_session(
    session: &str,
    window_id: Option<u64>,
    pid: Option<i64>,
    action_label: String,
    timestamp_ms: u64,
) {
    publish_preview(Some(session), window_id, pid, action_label, timestamp_ms);
}

fn publish_preview(
    session: Option<&str>,
    window_id: Option<u64>,
    pid: Option<i64>,
    action_label: String,
    timestamp_ms: u64,
) {
    if let Some(publisher) = PIP_REQUESTS
        .get()
        .filter(|publisher| !publisher.sender.is_closed())
    {
        let sender = &publisher.sender;
        sender.send_if_modified(|pending| {
            // Serialize this check with clearing the same pending slot. End
            // hooks run after the session tombstone lock has been released.
            // A late action must not resurrect a session's cleared preview.
            if session.is_some_and(crate::session::is_session_ending) {
                return false;
            }
            let request = PipCaptureRequest {
                window_id,
                pid,
                action_label,
                timestamp_ms,
            };
            if publisher.session_snapshots {
                let Some(session) = session else { return false };
                pending.sessions.insert(session.to_owned(), request);
            } else {
                pending.latest = Some(PendingPreview {
                    session: session.map(str::to_owned),
                    request,
                });
            }
            true
        });
    }
}

fn clear_session_preview(sender: &watch::Sender<PreviewState>, session: &str, all_sessions: bool) {
    sender.send_if_modified(|state| {
        if all_sessions {
            return state.sessions.remove(session).is_some();
        }
        let Some(pending) = &mut state.latest else {
            return false;
        };
        if pending.session.as_deref() != Some(session) {
            return false;
        }
        pending.session = None;
        pending.request = PipCaptureRequest {
            window_id: None,
            pid: None,
            action_label: "Session ended".into(),
            timestamp_ms: crate::recording::now_ms(),
        };
        true
    });
}
