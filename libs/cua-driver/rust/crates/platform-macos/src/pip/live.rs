//! Live mirror: at most one ScreenCaptureKit stream per visible PiP panel.
//!
//! The main queue decides (through the pure [`stream_step`]) when a panel's
//! stream should start, switch to a new target, or stop, and hands the
//! request to [`Streams`]. A dedicated `cua-pip-stream` thread does the
//! blocking ScreenCaptureKit work (`SCShareableContent` lookup,
//! `startCapture`, `stopCapture`), so neither the action path nor the main
//! thread ever waits on it. Requests coalesce per session: only the newest
//! one for a session is acted on.
//!
//! Each stream captures one window (a desktop-independent-window filter),
//! scaled to the panel's image well at [`LIVE_FPS`]. Frames go to the main
//! queue through a one-frame slot per stream, so a busy main thread sees
//! the newest frame instead of a backlog. A stream that cannot start, or
//! that ScreenCaptureKit stops (window closed, permission revoked), reports
//! [`Event::Ended`]; the panel then falls back to its still screenshots.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use screencapturekit::cm::{CMTime, SCFrameStatus};
use screencapturekit::prelude::{
    CMSampleBuffer, CMSampleBufferExt, CMSampleBufferSCExt, SCContentFilter, SCShareableContent,
    SCStream, SCStreamConfiguration, SCStreamOutputType,
};
use screencapturekit::stream::delegate_trait::StreamCallbacks;
use screencapturekit::CVPixelBuffer;

use super::{lock, resolve_target_window, LatestPerSession, Target};

/// Frame rate of the live mirror. A preview, not a recording.
pub(super) const LIVE_FPS: i32 = 12;
/// Stream pixels per image-well point (Retina-sharp; 1x screens downscale).
const PIXEL_SCALE: f64 = 2.0;
/// Buffers ScreenCaptureKit may have in flight. The panel holds one (the
/// frame on screen) and the slot at most one more.
const QUEUE_DEPTH: u32 = 5;

/// What a panel's stream should do next.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum StreamStep {
    /// Start a stream of this target, replacing any stream the panel has.
    Start(Target),
    Stop,
    Keep,
}

/// A panel's stream runs only while the panel is shown and has a target.
/// `resolved` is the window a pid-only target currently resolves to (looked
/// up off the main thread; ignored when the target names its window).
/// `requested` is the resolved target the panel last asked a stream for
/// (running or failed); a failed stream is not retried until the resolved
/// window changes or the panel hides and shows again. A pid-only target
/// whose window cannot be resolved right now keeps a stream of the same pid
/// (a transient gap) but stops a stream of any other app.
pub(super) fn stream_step(
    shown: bool,
    target: Target,
    resolved: Option<u32>,
    requested: Option<Target>,
) -> StreamStep {
    let has_target = shown && target != (None, None);
    let wanted = has_target
        .then(|| match target {
            (_, Some(_)) => Some(target),
            (pid, None) => resolved.map(|window| (pid, Some(window))),
        })
        .flatten();
    match (wanted, requested) {
        (Some(want), Some(have)) if want == have => StreamStep::Keep,
        (Some(want), _) => StreamStep::Start(want),
        (None, Some(have)) if !has_target || target.0 != have.0 => StreamStep::Stop,
        (None, _) => StreamStep::Keep,
    }
}

/// What a panel knows about its stream. `requested` is the resolved target
/// last asked for (running or failed) and drives retry suppression;
/// `generation` (0 = none) is the only generation whose events are still
/// applied. Ending or stopping a stream clears the generation but keeps
/// `requested`, so a late frame from a dead stream is dropped while a failed
/// target is still not retried.
#[derive(Debug, Default)]
pub(super) struct StreamState {
    pub(super) requested: Option<Target>,
    generation: u64,
}

impl StreamState {
    /// Generations are handed out from 1, so 0 never matches an event.
    pub(super) fn begin(&mut self, target: Target, generation: u64) {
        self.requested = Some(target);
        self.generation = generation;
    }

    pub(super) fn stop(&mut self) {
        self.requested = None;
        self.generation = 0;
    }

    /// Whether an event from `generation` should be applied.
    pub(super) fn accepts(&self, generation: u64) -> bool {
        self.generation != 0 && self.generation == generation
    }

    /// The stream of `generation` ended. Returns whether that was the
    /// current stream; if so, later events from it are dropped.
    pub(super) fn end(&mut self, generation: u64) -> bool {
        let current = self.accepts(generation);
        if current {
            self.generation = 0;
        }
        current
    }
}

/// Stream size in pixels for a `window`-point window shown aspect-fit in a
/// `well`-point image well. Never upscales a window smaller than the well.
pub(super) fn stream_pixel_size(window: (f64, f64), well: (f64, f64)) -> (u32, u32) {
    let (w, h) = if window.0 > 0.0 && window.1 > 0.0 {
        let fit = (well.0 / window.0).min(well.1 / window.1).min(1.0);
        (window.0 * fit, window.1 * fit)
    } else {
        well
    };
    let px = |points: f64| (points * PIXEL_SCALE).round().max(2.0) as u32;
    (px(w), px(h))
}

/// Newest undelivered frame of one stream.
pub(super) type FrameSlot = Arc<Mutex<Option<CVPixelBuffer>>>;

pub(super) enum Event {
    /// A new frame is waiting in `slot`.
    Frame {
        key: String,
        generation: u64,
        slot: FrameSlot,
    },
    /// The stream could not start or was stopped by ScreenCaptureKit.
    Ended { key: String, generation: u64 },
}

pub(super) enum Request {
    Start {
        generation: u64,
        target: Target,
        /// Image well size in points.
        well: (f64, f64),
    },
    Stop,
}

type Deliver = Arc<dyn Fn(Event) + Send + Sync>;

pub(super) struct Streams {
    requests: Mutex<LatestPerSession<Request>>,
    ready: Condvar,
}

impl Streams {
    pub(super) fn start(
        deliver: impl Fn(Event) + Send + Sync + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let streams = Arc::new(Self {
            requests: Mutex::new(LatestPerSession::new()),
            ready: Condvar::new(),
        });
        let looping = streams.clone();
        let deliver: Deliver = Arc::new(deliver);
        std::thread::Builder::new()
            .name("cua-pip-stream".into())
            .spawn(move || {
                let mut running: HashMap<String, SCStream> = HashMap::new();
                loop {
                    let (key, request) = looping.next();
                    if let Some(old) = running.remove(&key) {
                        let _ = old.stop_capture();
                    }
                    let Request::Start {
                        generation,
                        target,
                        well,
                    } = request
                    else {
                        continue;
                    };
                    match open(&key, generation, target, well, &deliver) {
                        Ok(stream) => {
                            running.insert(key, stream);
                        }
                        Err(error) => {
                            tracing::info!(target: "pip", ?target, %error, "PiP live stream unavailable; using stills");
                            deliver(Event::Ended { key, generation });
                        }
                    }
                }
            })?;
        Ok(streams)
    }

    /// Queue a request; never blocks on ScreenCaptureKit.
    pub(super) fn request(&self, key: &str, request: Request) {
        lock(&self.requests).push(key.to_owned(), request);
        self.ready.notify_one();
    }

    fn next(&self) -> (String, Request) {
        let mut requests = lock(&self.requests);
        loop {
            if let Some(item) = requests.pop() {
                return item;
            }
            requests = self.ready.wait(requests).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Build and start a window stream. Runs on the stream thread only.
fn open(
    key: &str,
    generation: u64,
    target: Target,
    well: (f64, f64),
    deliver: &Deliver,
) -> anyhow::Result<SCStream> {
    let window_id = resolve_target_window(target)
        .ok_or_else(|| anyhow::anyhow!("no window to stream"))?;
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!("SCShareableContent::get failed: {e}"))?;
    let window = content
        .windows()
        .into_iter()
        .find(|window| window.window_id() == window_id)
        .ok_or_else(|| anyhow::anyhow!("window {window_id} is not shareable"))?;
    let frame = window.frame();
    let (width, height) = stream_pixel_size((frame.size.width, frame.size.height), well);

    // Captures only this window, wherever it is and whatever covers it.
    let filter = SCContentFilter::create().with_window(&window).build();
    let config = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(true)
        .with_shows_cursor(false)
        .with_queue_depth(QUEUE_DEPTH)
        .with_minimum_frame_interval(&CMTime::new(1, LIVE_FPS));

    let ended = deliver.clone();
    let ended_key = key.to_owned();
    let callbacks = StreamCallbacks::new().on_error(move |error| {
        tracing::info!(target: "pip", %error, "PiP live stream stopped");
        ended(Event::Ended {
            key: ended_key.clone(),
            generation,
        });
    });
    let mut stream = SCStream::new_with_delegate(&filter, &config, callbacks);

    let slot: FrameSlot = Arc::new(Mutex::new(None));
    let frames = deliver.clone();
    let frame_key = key.to_owned();
    stream
        .add_output_handler(
            move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                if of_type != SCStreamOutputType::Screen {
                    return;
                }
                match sample.frame_status() {
                    Some(SCFrameStatus::Complete) => {}
                    // The window is gone: fall back to stills.
                    Some(SCFrameStatus::Stopped) => {
                        return frames(Event::Ended {
                            key: frame_key.clone(),
                            generation,
                        });
                    }
                    // Idle/blank/suspended frames carry no new pixels.
                    _ => return,
                }
                let Some(buffer) = sample.image_buffer() else {
                    return;
                };
                // Only wake the main queue when the slot was empty; a frame
                // already waiting there is simply replaced by this newer one.
                if lock(&slot).replace(buffer).is_none() {
                    frames(Event::Frame {
                        key: frame_key.clone(),
                        generation,
                        slot: slot.clone(),
                    });
                }
            },
            SCStreamOutputType::Screen,
        )
        .ok_or_else(|| anyhow::anyhow!("SCStream rejected the frame handler"))?;
    stream
        .start_capture()
        .map_err(|e| anyhow::anyhow!("SCStream::start_capture failed: {e}"))?;
    tracing::debug!(target: "pip", window_id, width, height, fps = LIVE_FPS, "PiP live stream started");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Target = (Some(42), Some(7));
    const B: Target = (Some(42), Some(8));

    #[test]
    fn a_shown_panel_starts_a_stream_for_its_target() {
        assert_eq!(stream_step(true, A, None, None), StreamStep::Start(A));
        assert_eq!(stream_step(true, A, None, Some(A)), StreamStep::Keep);
    }

    #[test]
    fn a_new_target_switches_the_stream() {
        assert_eq!(stream_step(true, B, None, Some(A)), StreamStep::Start(B));
    }

    #[test]
    fn a_hidden_panel_stops_its_stream() {
        assert_eq!(stream_step(false, A, None, Some(A)), StreamStep::Stop);
        assert_eq!(stream_step(false, A, None, None), StreamStep::Keep);
    }

    #[test]
    fn no_target_means_no_stream() {
        assert_eq!(stream_step(true, (None, None), None, None), StreamStep::Keep);
        assert_eq!(
            stream_step(true, (None, None), None, Some(A)),
            StreamStep::Stop
        );
    }

    #[test]
    fn a_pid_only_target_follows_its_resolved_window() {
        const PID_ONLY: Target = (Some(42), None);
        let on = |window| (Some(42), Some(window));
        assert_eq!(
            stream_step(true, PID_ONLY, Some(5), None),
            StreamStep::Start(on(5))
        );
        assert_eq!(
            stream_step(true, PID_ONLY, Some(5), Some(on(5))),
            StreamStep::Keep
        );
        // The app raised another window: same pid-only target, new stream.
        assert_eq!(
            stream_step(true, PID_ONLY, Some(6), Some(on(5))),
            StreamStep::Start(on(6))
        );
        // No window to resolve right now: the same app's stream stays.
        assert_eq!(
            stream_step(true, PID_ONLY, None, Some(on(5))),
            StreamStep::Keep
        );
        // A different app with no resolvable window must not keep the old
        // app's live pixels under its label.
        assert_eq!(
            stream_step(true, (Some(43), None), None, Some(on(5))),
            StreamStep::Stop
        );
        assert_eq!(stream_step(true, PID_ONLY, None, None), StreamStep::Keep);
        // A hidden panel still stops.
        assert_eq!(
            stream_step(false, PID_ONLY, Some(5), Some(on(5))),
            StreamStep::Stop
        );
    }

    #[test]
    fn a_frame_arriving_after_the_stream_ended_is_dropped() {
        let mut state = StreamState::default();
        state.begin(A, 3);
        assert!(state.accepts(3));
        assert!(state.end(3));
        // SCK's sample handler can still deliver a frame of generation 3.
        assert!(!state.accepts(3));
        // Retry suppression survives: the failed target is still requested.
        assert_eq!(
            stream_step(true, A, None, state.requested),
            StreamStep::Keep
        );
        // A repeated or stale end changes nothing.
        assert!(!state.end(3));
        assert!(!state.end(2));
        // A new stream gets a fresh generation and works again.
        state.begin(B, 4);
        assert!(state.accepts(4) && !state.accepts(3));
        state.stop();
        assert!(!state.accepts(4) && state.requested.is_none());
    }

    #[test]
    fn stream_size_fits_the_well_at_retina_scale() {
        // A 1600x1000 window in a 320x200 well: exact fit, doubled.
        assert_eq!(
            stream_pixel_size((1600.0, 1000.0), (320.0, 200.0)),
            (640, 400)
        );
        // A tall window is limited by the well's height.
        assert_eq!(
            stream_pixel_size((400.0, 1000.0), (320.0, 200.0)),
            (160, 400)
        );
        // A window smaller than the well is not upscaled.
        assert_eq!(stream_pixel_size((100.0, 50.0), (320.0, 200.0)), (200, 100));
        // Unknown window size falls back to the well.
        assert_eq!(stream_pixel_size((0.0, 0.0), (320.0, 200.0)), (640, 400));
    }

    #[test]
    fn requests_coalesce_to_the_newest_per_session() {
        let streams = Streams {
            requests: Mutex::new(LatestPerSession::new()),
            ready: Condvar::new(),
        };
        let start = |generation| Request::Start {
            generation,
            target: A,
            well: (320.0, 200.0),
        };
        streams.request("s", start(1));
        streams.request("t", start(2));
        streams.request("s", Request::Stop);
        assert!(matches!(streams.next(), (key, Request::Stop) if key == "s"));
        assert!(
            matches!(streams.next(), (key, Request::Start { generation: 2, .. }) if key == "t")
        );
    }
}
