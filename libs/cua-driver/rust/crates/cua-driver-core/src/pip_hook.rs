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

static PIP_REQUESTS: OnceLock<watch::Sender<Option<PipCaptureRequest>>> = OnceLock::new();

enum Consumer {
    Legacy(Box<dyn Fn(PipHookFrame) + Send + Sync>),
    Observer(Box<dyn FnMut(PipCaptureRequest) -> bool + Send>),
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

fn start_worker(mut consumer: Consumer) {
    PIP_REQUESTS.get_or_init(|| {
        let (sender, mut receiver) = watch::channel::<Option<PipCaptureRequest>>(None);
        let started = std::thread::Builder::new()
            .name("cua-pip-publisher".into())
            .spawn(move || {
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
                        let request = receiver.borrow_and_update().clone();
                        let Some(request) = request else { continue };
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
        sender
    });
}

/// True while the preview publisher is running. This is not an acknowledgement
/// that the helper rendered a frame. A closed publisher disables further work.
pub fn pip_enabled() -> bool {
    PIP_REQUESTS.get().is_some_and(|sender| !sender.is_closed())
}

/// Replace pending preview work. Capture and renderer callbacks run only on
/// the worker, never on the action path. Intermediate previews may be skipped.
pub fn request_pip_frame(
    window_id: Option<u64>,
    pid: Option<i64>,
    action_label: String,
    timestamp_ms: u64,
) {
    if let Some(sender) = PIP_REQUESTS.get().filter(|sender| !sender.is_closed()) {
        sender.send_replace(Some(PipCaptureRequest {
            window_id,
            pid,
            action_label,
            timestamp_ms,
        }));
    }
}
