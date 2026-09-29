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
//! No ScreenCaptureKit call can wedge that thread: opens and resizes run on
//! helper threads with a bounded wait (a timeout abandons the stream and
//! reports [`Event::Ended`]), stops run on their own threads and an open
//! waits for them at most one call timeout, and a stream that fails to open
//! is retried a few times before the panel gives up on live.
//! Each start, stop, resize, retry and timeout logs a `pip` tracing line
//! with the session key, generation and elapsed time.
//!
//! Each stream captures one window (a desktop-independent-window filter),
//! scaled to the panel's image well at [`LIVE_FPS`]. Frames go to the main
//! queue through a one-frame slot per stream, so a busy main thread sees
//! the newest frame instead of a backlog. A stream that cannot start, or
//! that ScreenCaptureKit stops (window closed, permission revoked), reports
//! [`Event::Ended`]; the panel then falls back to its still screenshots.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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
    /// Image well size (points) the stream was last sized for.
    well: (f64, f64),
    /// A frame of the current generation has been shown.
    framed: bool,
}

impl StreamState {
    /// Generations are handed out from 1, so 0 never matches an event.
    pub(super) fn begin(&mut self, target: Target, generation: u64, well: (f64, f64)) {
        self.requested = Some(target);
        self.generation = generation;
        self.well = well;
        self.framed = false;
    }

    /// The current generation's generation number (0 = none), for logs.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// True for the first frame shown of the current generation only.
    pub(super) fn first_frame(&mut self) -> bool {
        !std::mem::replace(&mut self.framed, true)
    }

    /// Whether a running stream must be reconfigured for a new `well`.
    /// Records the new size when it must.
    pub(super) fn needs_resize(&mut self, well: (f64, f64)) -> bool {
        let running = self.generation != 0 && self.well != well;
        if running {
            self.well = well;
        }
        running
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

/// Stream buffer size in pixels for a `well`-point image well: the whole
/// well at [`PIXEL_SCALE`], whatever the window's bounds. ScreenCaptureKit
/// scales the window into it preserving aspect, so a window that changes
/// shape needs no reconfiguration.
pub(super) fn stream_pixel_size(well: (f64, f64)) -> (u32, u32) {
    let px = |points: f64| (points * PIXEL_SCALE).round().max(2.0) as u32;
    (px(well.0), px(well.1))
}

/// Stream configuration for a `well`-point image well.
fn stream_config(well: (f64, f64)) -> SCStreamConfiguration {
    let (width, height) = stream_pixel_size(well);
    SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(true)
        .with_shows_cursor(false)
        .with_queue_depth(QUEUE_DEPTH)
        .with_minimum_frame_interval(&CMTime::new(1, LIVE_FPS))
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
    /// The panel's image well changed size; reconfigure the running stream.
    Resize { well: (f64, f64) },
    Stop,
}

type Deliver = Arc<dyn Fn(Event) + Send + Sync>;

/// Longest the stream thread waits on one blocking stop or resize call.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest it waits for a stream to open (`SCShareableContent` lookup plus
/// `startCapture`), which is slower while windows churn.
const OPEN_TIMEOUT: Duration = Duration::from_secs(4);
/// A stream that fails to open (a just-launched window is often not yet in
/// ScreenCaptureKit's list) is retried this many times, this far apart,
/// before the panel gives up on live for that target.
const OPEN_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// The blocking ScreenCaptureKit calls, behind a trait so the stream
/// thread's timeout and retry decisions are tested with a fake.
trait Backend: Send + Sync + 'static {
    type Stream: Send + Sync + 'static;
    fn open(
        &self,
        key: &str,
        generation: u64,
        target: Target,
        well: (f64, f64),
        deliver: &Deliver,
    ) -> anyhow::Result<Self::Stream>;
    fn stop(&self, stream: &Self::Stream);
    fn resize(&self, stream: &Self::Stream, well: (f64, f64)) -> anyhow::Result<()>;
}

/// Result of a call running on a helper thread, shared with its waiter.
enum Call<T> {
    Pending,
    Done(T),
    /// The waiter gave up; the helper cleans up its own result.
    Abandoned,
}

/// Run `f` on a helper thread and wait at most `timeout` for it. On timeout
/// the thread is left behind (it may be stuck inside ScreenCaptureKit for
/// good), the call counts as failed, and if `f` ever does return, its value
/// goes to `late` on that helper thread instead of being lost.
fn bounded<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
    late: impl FnOnce(T) + Send + 'static,
) -> Option<T> {
    let shared = Arc::new((Mutex::new(Call::Pending), Condvar::new()));
    let helper = shared.clone();
    std::thread::Builder::new()
        .name("cua-pip-sck".into())
        .spawn(move || {
            let value = f();
            let mut call = lock(&helper.0);
            if matches!(*call, Call::Abandoned) {
                drop(call);
                late(value);
            } else {
                *call = Call::Done(value);
                helper.1.notify_one();
            }
        })
        .ok()?;
    let deadline = Instant::now() + timeout;
    let mut call = lock(&shared.0);
    loop {
        if matches!(*call, Call::Done(_)) {
            let Call::Done(value) = std::mem::replace(&mut *call, Call::Abandoned) else {
                unreachable!()
            };
            return Some(value);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            *call = Call::Abandoned;
            return None;
        }
        call = shared
            .1
            .wait_timeout(call, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

struct Running<S> {
    generation: u64,
    stream: Arc<S>,
}

/// A failed open waiting to be tried again.
struct Retry {
    at: Instant,
    generation: u64,
    target: Target,
    well: (f64, f64),
    /// Retries already made.
    attempt: u32,
}

/// The stream thread's state and decisions. One stuck ScreenCaptureKit call
/// delays it by at most a timeout and never wedges it: stops are fire and
/// forget, and a stream whose call timed out is abandoned, never used again.
struct Worker<B: Backend> {
    backend: Arc<B>,
    deliver: Deliver,
    call_timeout: Duration,
    open_timeout: Duration,
    retry_delay: Duration,
    running: HashMap<String, Running<B::Stream>>,
    retries: HashMap<String, Retry>,
    /// Stops still inside ScreenCaptureKit, by id, with when each began.
    stops: Arc<(Mutex<HashMap<u64, Instant>>, Condvar)>,
    next_stop: u64,
}

impl<B: Backend> Worker<B> {
    fn new(backend: B, deliver: Deliver) -> Self {
        Self {
            backend: Arc::new(backend),
            deliver,
            call_timeout: CALL_TIMEOUT,
            open_timeout: OPEN_TIMEOUT,
            retry_delay: RETRY_DELAY,
            running: HashMap::new(),
            retries: HashMap::new(),
            stops: Arc::default(),
            next_stop: 0,
        }
    }

    fn handle(&mut self, key: String, request: Request) {
        // Any newer request for the session supersedes a pending retry.
        self.retries.remove(&key);
        match request {
            Request::Stop => self.stop_running(&key),
            Request::Resize { well } => self.resize(&key, well),
            Request::Start {
                generation,
                target,
                well,
            } => {
                self.stop_running(&key);
                self.open(key, generation, target, well, 0);
            }
        }
    }

    /// Earliest pending retry, for the loop's wake-up.
    fn next_retry(&self) -> Option<Instant> {
        self.retries.values().map(|retry| retry.at).min()
    }

    /// Run the retries that are due at `now`.
    fn retry_due(&mut self, now: Instant) {
        let due: Vec<String> = self
            .retries
            .iter()
            .filter(|(_, retry)| retry.at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            if let Some(retry) = self.retries.remove(&key) {
                self.open(
                    key,
                    retry.generation,
                    retry.target,
                    retry.well,
                    retry.attempt + 1,
                );
            }
        }
    }

    fn end(&self, key: String, generation: u64) {
        (self.deliver)(Event::Ended { key, generation });
    }

    /// Stop the session's stream without waiting for ScreenCaptureKit: a
    /// stream whose window is gone can hang in `stopCapture`. The next open
    /// waits a little for it (see [`Self::settle_stops`]).
    fn stop_running(&mut self, key: &str) {
        let Some(running) = self.running.remove(key) else {
            return;
        };
        let backend = self.backend.clone();
        let (key, generation) = (key.to_owned(), running.generation);
        let stream = running.stream;
        let id = self.next_stop;
        self.next_stop += 1;
        lock(&self.stops.0).insert(id, Instant::now());
        let stops = self.stops.clone();
        tracing::info!(target: "pip", session = %key, generation, "PiP stream stopping");
        let spawned = std::thread::Builder::new()
            .name("cua-pip-stop".into())
            .spawn(move || {
                let started = Instant::now();
                backend.stop(&stream);
                lock(&stops.0).remove(&id);
                stops.1.notify_all();
                tracing::info!(target: "pip", session = %key, generation, elapsed_ms = started.elapsed().as_millis() as u64, "PiP stream stopped");
            });
        if let Err(error) = spawned {
            lock(&self.stops.0).remove(&id);
            tracing::warn!(target: "pip", %error, "PiP could not spawn a stop thread; stream abandoned");
        }
    }

    /// Wait for stops still inside ScreenCaptureKit to finish, each for at
    /// most `call_timeout` from when it began, so an open never overlaps a
    /// teardown ScreenCaptureKit is still doing (the order the stream thread
    /// had when stops were synchronous). Returns how many stops are still
    /// running (hung past their timeout; not waited on again).
    fn settle_stops(&self) -> usize {
        let (stops, done) = &*self.stops;
        let mut stops = lock(stops);
        loop {
            let now = Instant::now();
            let Some(until) = stops
                .values()
                .map(|began| *began + self.call_timeout)
                .filter(|until| *until > now)
                .max()
            else {
                return stops.len();
            };
            stops = done
                .wait_timeout(stops, until - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn resize(&mut self, key: &str, well: (f64, f64)) {
        let Some(running) = self.running.get(key) else {
            return;
        };
        let (generation, stream) = (running.generation, running.stream.clone());
        let backend = self.backend.clone();
        let started = Instant::now();
        let result = bounded(
            self.call_timeout,
            move || backend.resize(&stream, well),
            |_| {},
        );
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Some(Ok(())) => {
                tracing::info!(target: "pip", session = %key, generation, ?well, elapsed_ms, "PiP stream resized");
            }
            Some(Err(error)) => {
                tracing::info!(target: "pip", session = %key, generation, %error, elapsed_ms, "PiP stream resize failed; keeping the stream");
            }
            None => {
                tracing::warn!(target: "pip", session = %key, generation, elapsed_ms, "PiP stream resize timed out; abandoning the stream");
                self.stop_running(key);
                self.end(key.to_owned(), generation);
            }
        }
    }

    fn open(&mut self, key: String, generation: u64, target: Target, well: (f64, f64), attempt: u32) {
        let started = Instant::now();
        let hung_stops = self.settle_stops();
        let backend = self.backend.clone();
        let late_backend = self.backend.clone();
        let deliver = self.deliver.clone();
        let call_key = key.clone();
        let result = bounded(
            self.open_timeout,
            move || backend.open(&call_key, generation, target, well, &deliver),
            move |stream| {
                // The waiter gave up, so nothing owns this stream: stop it.
                if let Ok(stream) = stream {
                    late_backend.stop(&stream);
                    tracing::info!(target: "pip", generation, "PiP stream that opened after its timeout was stopped");
                }
            },
        );
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Some(Ok(stream)) => {
                tracing::info!(target: "pip", session = %key, generation, ?target, attempt, hung_stops, elapsed_ms, "PiP stream started");
                self.running.insert(
                    key,
                    Running {
                        generation,
                        stream: Arc::new(stream),
                    },
                );
            }
            Some(Err(error)) if attempt < OPEN_RETRIES => {
                tracing::info!(target: "pip", session = %key, generation, ?target, attempt, hung_stops, %error, elapsed_ms, "PiP stream did not open; retrying");
                self.retries.insert(
                    key,
                    Retry {
                        at: Instant::now() + self.retry_delay,
                        generation,
                        target,
                        well,
                        attempt,
                    },
                );
            }
            Some(Err(error)) => {
                tracing::info!(target: "pip", session = %key, generation, ?target, attempt, hung_stops, %error, elapsed_ms, "PiP live stream unavailable; using stills");
                self.end(key, generation);
            }
            None => {
                tracing::warn!(target: "pip", session = %key, generation, ?target, attempt, hung_stops, elapsed_ms, "PiP stream open timed out; using stills");
                self.end(key, generation);
            }
        }
    }
}

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
        let mut worker = Worker::new(SckBackend, Arc::new(deliver));
        std::thread::Builder::new()
            .name("cua-pip-stream".into())
            .spawn(move || loop {
                match looping.next(worker.next_retry()) {
                    Some((key, request)) => worker.handle(key, request),
                    None => worker.retry_due(Instant::now()),
                }
            })?;
        Ok(streams)
    }

    /// Queue a request; never blocks on ScreenCaptureKit.
    pub(super) fn request(&self, key: &str, request: Request) {
        {
            let mut requests = lock(&self.requests);
            match (request, requests.latest.get_mut(key)) {
                // Not started yet: it just starts at the new size.
                (Request::Resize { well: new }, Some(Request::Start { well, .. })) => *well = new,
                // About to stop: nothing to resize.
                (Request::Resize { .. }, Some(Request::Stop)) => {}
                (request, _) => requests.push(key.to_owned(), request),
            }
        }
        self.ready.notify_one();
    }

    /// The next request, or `None` once `wake` (a retry deadline) passes.
    fn next(&self, wake: Option<Instant>) -> Option<(String, Request)> {
        let mut requests = lock(&self.requests);
        loop {
            if let Some(item) = requests.pop() {
                return Some(item);
            }
            requests = match wake {
                None => self.ready.wait(requests).unwrap_or_else(|e| e.into_inner()),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    self.ready
                        .wait_timeout(requests, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }
}

/// The real ScreenCaptureKit backend.
struct SckBackend;

impl Backend for SckBackend {
    type Stream = SCStream;

    fn open(
        &self,
        key: &str,
        generation: u64,
        target: Target,
        well: (f64, f64),
        deliver: &Deliver,
    ) -> anyhow::Result<SCStream> {
        open_stream(key, generation, target, well, deliver)
    }

    fn stop(&self, stream: &SCStream) {
        let _ = stream.stop_capture();
    }

    fn resize(&self, stream: &SCStream, well: (f64, f64)) -> anyhow::Result<()> {
        stream
            .update_configuration(&stream_config(well))
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// Build and start a window stream.
fn open_stream(
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
    let (width, height) = stream_pixel_size(well);

    // Captures only this window, wherever it is and whatever covers it.
    let filter = SCContentFilter::create().with_window(&window).build();
    let config = stream_config(well);

    let ended = deliver.clone();
    let ended_key = key.to_owned();
    let callbacks = StreamCallbacks::new().on_error(move |error| {
        tracing::info!(target: "pip", session = %ended_key, generation, %error, "PiP live stream stopped by ScreenCaptureKit");
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
                        tracing::info!(target: "pip", session = %frame_key, generation, "PiP live stream window gone (frame status Stopped)");
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
    tracing::debug!(target: "pip", session = %key, generation, window_id, width, height, fps = LIVE_FPS, "PiP live stream configured");
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
        state.begin(A, 3, (320.0, 200.0));
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
        state.begin(B, 4, (320.0, 200.0));
        assert!(state.accepts(4) && !state.accepts(3));
        state.stop();
        assert!(!state.accepts(4) && state.requested.is_none());
    }

    #[test]
    fn stream_buffer_is_the_whole_well_at_retina_scale() {
        // Independent of the window: a 320x200 well is always 640x400.
        assert_eq!(stream_pixel_size((320.0, 200.0)), (640, 400));
        assert_eq!(stream_pixel_size((100.0, 50.0)), (200, 100));
        assert_eq!(stream_pixel_size((0.0, 0.0)), (2, 2));
    }

    #[test]
    fn a_running_stream_resizes_only_when_the_well_changes() {
        let mut state = StreamState::default();
        state.begin(A, 1, (320.0, 200.0));
        assert!(!state.needs_resize((320.0, 200.0)));
        assert!(state.needs_resize((400.0, 250.0)));
        assert!(!state.needs_resize((400.0, 250.0)));
        // No running stream: nothing to reconfigure.
        state.stop();
        assert!(!state.needs_resize((500.0, 300.0)));
    }

    #[test]
    fn a_resize_folds_into_a_pending_start_and_dies_with_a_pending_stop() {
        let streams = Streams {
            requests: Mutex::new(LatestPerSession::new()),
            ready: Condvar::new(),
        };
        streams.request(
            "s",
            Request::Start {
                generation: 1,
                target: A,
                well: (320.0, 200.0),
            },
        );
        streams.request("s", Request::Resize { well: (400.0, 250.0) });
        assert!(matches!(
            streams.next(None).unwrap(),
            (_, Request::Start { generation: 1, well, .. }) if well == (400.0, 250.0)
        ));
        streams.request("t", Request::Stop);
        streams.request("t", Request::Resize { well: (1.0, 1.0) });
        assert!(matches!(streams.next(None).unwrap(), (key, Request::Stop) if key == "t"));
        // With nothing pending, a resize is queued for the stream thread.
        streams.request("u", Request::Resize { well: (1.0, 1.0) });
        assert!(matches!(streams.next(None).unwrap(), (key, Request::Resize { .. }) if key == "u"));
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
        assert!(matches!(streams.next(None).unwrap(), (key, Request::Stop) if key == "s"));
        assert!(
            matches!(streams.next(None).unwrap(), (key, Request::Start { generation: 2, .. }) if key == "t")
        );
    }

    // ── Worker with a fake ScreenCaptureKit ───────────────────────────────

    enum Open {
        Ok,
        Err,
        /// Blocks until the sender is used or dropped, then succeeds.
        Hang(std::sync::mpsc::Receiver<()>),
    }

    #[derive(Default)]
    struct FakeInner {
        plan: Mutex<std::collections::VecDeque<Open>>,
        opens: std::sync::atomic::AtomicU32,
        stopped: Mutex<Vec<u32>>,
        /// How many stops had finished when each open began.
        stops_done_at_open: Mutex<Vec<usize>>,
        hang_stop: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        hang_resize: std::sync::atomic::AtomicBool,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<FakeInner>);

    struct FakeStream(u32);

    impl Backend for Fake {
        type Stream = FakeStream;

        fn open(
            &self,
            _key: &str,
            _generation: u64,
            _target: Target,
            _well: (f64, f64),
            _deliver: &Deliver,
        ) -> anyhow::Result<FakeStream> {
            use std::sync::atomic::Ordering::SeqCst;
            let id = self.0.opens.fetch_add(1, SeqCst);
            let done = lock(&self.0.stopped).len();
            lock(&self.0.stops_done_at_open).push(done);
            let step = lock(&self.0.plan).pop_front().unwrap_or(Open::Ok);
            match step {
                Open::Ok => Ok(FakeStream(id)),
                Open::Err => Err(anyhow::anyhow!("window is not shareable")),
                Open::Hang(gate) => {
                    let _ = gate.recv();
                    Ok(FakeStream(id))
                }
            }
        }

        fn stop(&self, stream: &FakeStream) {
            if let Some(gate) = lock(&self.0.hang_stop).take() {
                let _ = gate.recv();
            }
            lock(&self.0.stopped).push(stream.0);
        }

        fn resize(&self, _stream: &FakeStream, _well: (f64, f64)) -> anyhow::Result<()> {
            while self.0.hang_resize.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
    }

    type Ended = Arc<Mutex<Vec<(String, u64)>>>;

    fn worker(fake: &Fake) -> (Worker<Fake>, Ended) {
        let ended: Ended = Arc::default();
        let sink = ended.clone();
        let deliver: Deliver = Arc::new(move |event| {
            if let Event::Ended { key, generation } = event {
                lock(&sink).push((key, generation));
            }
        });
        let mut worker = Worker::new(fake.clone(), deliver);
        worker.call_timeout = Duration::from_millis(100);
        worker.open_timeout = Duration::from_millis(100);
        worker.retry_delay = Duration::from_millis(10);
        (worker, ended)
    }

    fn start(generation: u64) -> Request {
        Request::Start {
            generation,
            target: A,
            well: (320.0, 200.0),
        }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_stuck_open_times_out_without_blocking_other_sessions() {
        let fake = Fake::default();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        lock(&fake.0.plan).push_back(Open::Hang(gate));
        let (mut worker, ended) = worker(&fake);

        let began = Instant::now();
        worker.handle("a".into(), start(1));
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(*lock(&ended), vec![("a".to_owned(), 1)]);
        assert!(!worker.running.contains_key("a"));

        // The next session starts at once.
        worker.handle("b".into(), start(2));
        assert!(worker.running.contains_key("b"));

        // The stuck open finally returns: nobody owns that stream, so it is
        // stopped instead of leaking a capture.
        drop(release);
        wait_until("the late stream to be stopped", || {
            lock(&fake.0.stopped).contains(&0)
        });
    }

    #[test]
    fn stopping_a_hung_stream_never_blocks_the_thread() {
        let fake = Fake::default();
        let (_hold, gate) = std::sync::mpsc::channel::<()>();
        let (mut worker, _) = worker(&fake);
        worker.handle("a".into(), start(1));
        *lock(&fake.0.hang_stop) = Some(gate);

        let began = Instant::now();
        worker.handle("a".into(), Request::Stop);
        assert!(began.elapsed() < Duration::from_millis(50));
        assert!(!worker.running.contains_key("a"));
        // The next open waits for the stop at most one call timeout.
        worker.handle("b".into(), start(2));
        assert!(began.elapsed() < worker.call_timeout + Duration::from_millis(80));
        assert!(worker.running.contains_key("b"));
    }

    /// The VM sequence: session A streams, idles out and hides (its stop is
    /// still inside ScreenCaptureKit), then session B's panel appears and
    /// opens a stream. B's open waits for A's teardown instead of racing it.
    #[test]
    fn an_open_waits_for_a_stop_still_in_flight() {
        let fake = Fake::default();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let (mut worker, _) = worker(&fake);
        worker.handle("a".into(), start(1));
        *lock(&fake.0.hang_stop) = Some(gate);
        worker.handle("a".into(), Request::Stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(release);
        });

        worker.handle("b".into(), start(2));
        assert!(worker.running.contains_key("b"));
        // A's open saw no stops; B's saw A's stop finished.
        assert_eq!(*lock(&fake.0.stops_done_at_open), vec![0, 1]);
        assert_eq!(worker.settle_stops(), 0);
    }

    #[test]
    fn a_hung_stop_delays_opens_by_at_most_one_call_timeout() {
        let fake = Fake::default();
        let (_hold, gate) = std::sync::mpsc::channel::<()>();
        let (mut worker, _) = worker(&fake);
        worker.handle("a".into(), start(1));
        *lock(&fake.0.hang_stop) = Some(gate);
        worker.handle("a".into(), Request::Stop);

        let began = Instant::now();
        worker.handle("b".into(), start(2));
        let waited = began.elapsed();
        assert!(worker.running.contains_key("b"));
        // It waited for the stop (whose clock started just before `began`).
        assert!(waited >= worker.call_timeout / 2 && waited < Duration::from_secs(2));
        // The stop is past its timeout: later opens do not wait on it again.
        let began = Instant::now();
        worker.handle("c".into(), start(3));
        assert!(began.elapsed() < worker.call_timeout);
        assert_eq!(worker.settle_stops(), 1);
    }

    #[test]
    fn a_resize_that_times_out_abandons_the_stream() {
        let fake = Fake::default();
        let (mut worker, ended) = worker(&fake);
        worker.handle("a".into(), start(7));
        fake.0
            .hang_resize
            .store(true, std::sync::atomic::Ordering::SeqCst);

        worker.handle("a".into(), Request::Resize { well: (400.0, 250.0) });
        fake.0
            .hang_resize
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(*lock(&ended), vec![("a".to_owned(), 7)]);
        assert!(!worker.running.contains_key("a"));
        // The abandoned stream is stopped, never reused.
        wait_until("the abandoned stream to be stopped", || {
            lock(&fake.0.stopped).contains(&0)
        });
    }

    #[test]
    fn a_failed_open_is_retried_a_few_times_then_reported() {
        let fake = Fake::default();
        lock(&fake.0.plan).extend((0..10).map(|_| Open::Err));
        let (mut worker, ended) = worker(&fake);

        worker.handle("a".into(), start(1));
        assert!(worker.next_retry().is_some());
        assert!(lock(&ended).is_empty());
        let far = Instant::now() + Duration::from_secs(60);
        while worker.next_retry().is_some() {
            worker.retry_due(far);
        }
        // The first try plus OPEN_RETRIES retries, then Ended, once.
        assert_eq!(
            fake.0.opens.load(std::sync::atomic::Ordering::SeqCst),
            OPEN_RETRIES + 1
        );
        assert_eq!(*lock(&ended), vec![("a".to_owned(), 1)]);
    }

    #[test]
    fn a_retry_that_opens_the_stream_reports_nothing() {
        let fake = Fake::default();
        lock(&fake.0.plan).push_back(Open::Err);
        let (mut worker, ended) = worker(&fake);
        worker.handle("a".into(), start(1));
        worker.retry_due(Instant::now() + Duration::from_secs(60));
        assert!(worker.running.contains_key("a"));
        assert!(worker.next_retry().is_none());
        assert!(lock(&ended).is_empty());
    }

    #[test]
    fn a_newer_request_cancels_a_pending_retry() {
        let fake = Fake::default();
        lock(&fake.0.plan).push_back(Open::Err);
        let (mut worker, _) = worker(&fake);
        worker.handle("a".into(), start(1));
        assert!(worker.next_retry().is_some());
        worker.handle("a".into(), Request::Stop);
        assert!(worker.next_retry().is_none());
    }
}
