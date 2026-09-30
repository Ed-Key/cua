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
//! or only its page (the configuration's source rectangle, see `page`),
//! scaled to the panel's image well at [`LIVE_FPS`]. A new crop reconfigures
//! the running stream; the panel adopts it for the cursor only once
//! ScreenCaptureKit applied it ([`Event::Reframed`]), so the rectangle it
//! maps into and the pixels it shows always change together. Frames go to the main
//! queue through a one-frame slot per stream, so a busy main thread sees
//! the newest frame instead of a backlog. A stream that cannot start, or
//! that ScreenCaptureKit stops (window closed, permission revoked), reports
//! [`Event::Ended`]; the panel then falls back to its still screenshots.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use screencapturekit::cm::{CMTime, SCFrameStatus};
use screencapturekit::prelude::{
    CMSampleBuffer, CMSampleBufferExt, SCContentFilter, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamOutputType,
};
use screencapturekit::stream::delegate_trait::StreamCallbacks;
use screencapturekit::CVPixelBuffer;

use super::{lock, resolve_target_window, Area, LatestPerSession, Target};

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
/// `requested` is the resolved target and framing (`page`) the panel last
/// asked a stream for (running or failed); a failed stream is not retried
/// until the resolved window or the framing changes or the panel hides and
/// shows again. A new framing of the same window is a new stream, so its
/// pixels never show under the old framing. A pid-only target whose window
/// cannot be resolved right now keeps a stream of the same pid (a transient
/// gap) but stops a stream of any other app.
pub(super) fn stream_step(
    shown: bool,
    target: Target,
    page: bool,
    resolved: Option<u32>,
    requested: Option<(Target, bool)>,
) -> StreamStep {
    let has_target = shown && target != (None, None);
    let wanted = has_target
        .then(|| match target {
            (_, Some(_)) => Some(target),
            (pid, None) => resolved.map(|window| (pid, Some(window))),
        })
        .flatten();
    match (wanted, requested) {
        (Some(want), Some(have)) if (want, page) == have => StreamStep::Keep,
        (Some(want), _) => StreamStep::Start(want),
        (None, Some((have, _))) if !has_target || target.0 != have.0 => StreamStep::Stop,
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
    pub(super) requested: Option<(Target, bool)>,
    generation: u64,
    /// Image well size (points) the stream was last sized for.
    well: (f64, f64),
    /// Page crop (window points) the stream was last asked for.
    crop: Option<Area>,
    /// Page crop the stream's frames show now (`None`: the whole window):
    /// what it opened with, then each reconfiguration once it succeeded.
    pub(super) shown_crop: Option<Area>,
    /// Live frames of the current generation, for logs.
    pub(super) frames: PanelFrames,
}

impl StreamState {
    /// Generations are handed out from 1, so 0 never matches an event.
    pub(super) fn begin(
        &mut self,
        view: (Target, bool),
        generation: u64,
        well: (f64, f64),
        crop: Option<Area>,
    ) {
        self.requested = Some(view);
        self.generation = generation;
        self.well = well;
        self.crop = crop;
        // Its frames all show the crop it opens with (a reconfiguration
        // folded into the open reports its own, see `reframed`).
        self.shown_crop = crop;
        self.frames = PanelFrames::default();
    }

    /// The current generation's generation number (0 = none), for logs.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a running stream must be reconfigured for a new `well` or
    /// page `crop` (its origin as much as its size). Records them when it
    /// must.
    pub(super) fn needs_resize(&mut self, well: (f64, f64), crop: Option<Area>) -> bool {
        let running = self.generation != 0 && (self.well != well || self.crop != crop);
        if running {
            self.well = well;
            self.crop = crop;
        }
        running
    }

    /// ScreenCaptureKit applied `crop` to the stream of `generation`: its
    /// frames show it from now on. Whether that is the current stream.
    pub(super) fn reframed(&mut self, generation: u64, crop: Option<Area>) -> bool {
        let current = self.accepts(generation);
        if current {
            self.shown_crop = crop;
        }
        current
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

/// Stream configuration for a `well`-point image well, of the whole window
/// or of its page `crop` (window points, which is what ScreenCaptureKit's
/// source rectangle takes for a window filter).
fn stream_config(well: (f64, f64), crop: Option<Area>) -> SCStreamConfiguration {
    let (width, height) = stream_pixel_size(well);
    let config = match crop {
        Some(crop) => SCStreamConfiguration::new().with_source_rect(
            screencapturekit::cg::CGRect::new(crop.x, crop.y, crop.w, crop.h),
        ),
        None => SCStreamConfiguration::new(),
    };
    config
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
    /// The stream's frames show `crop` from now on: it opened with it, or a
    /// reconfiguration to it succeeded.
    Reframed {
        key: String,
        generation: u64,
        crop: Option<Area>,
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
        /// Page crop in window points (`None`: the whole window).
        crop: Option<Area>,
    },
    /// The panel's image well changed size or its page crop changed;
    /// reconfigure the running stream.
    Resize {
        well: (f64, f64),
        crop: Option<Area>,
    },
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
        crop: Option<Area>,
        deliver: &Deliver,
    ) -> anyhow::Result<Self::Stream>;
    fn stop(&self, stream: &Self::Stream);
    fn resize(&self, stream: &Self::Stream, well: (f64, f64), crop: Option<Area>)
        -> anyhow::Result<()>;
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
    crop: Option<Area>,
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
        // A resize never cancels a pending open: the retry opens at the new
        // size and crop. (With nothing running or pending it is a no-op; the
        // panel keeps them for its next start.)
        if let Request::Resize { well, crop } = request {
            match self.retries.get_mut(&key) {
                Some(retry) => {
                    retry.well = well;
                    retry.crop = crop;
                }
                None => self.resize(&key, well, crop),
            }
            return;
        }
        // Any other newer request for the session supersedes a pending retry.
        self.retries.remove(&key);
        match request {
            Request::Stop => self.stop_running(&key),
            Request::Resize { .. } => {}
            Request::Start {
                generation,
                target,
                well,
                crop,
            } => {
                self.stop_running(&key);
                self.open(key, generation, target, well, crop, 0);
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
                    retry.crop,
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

    /// Reconfigure the session's stream for `well` and `crop`. Only a
    /// success reports the new crop ([`Event::Reframed`]): a failed call
    /// keeps the old stream showing the old crop, and one that times out
    /// abandons the stream as a stopped one.
    fn resize(&mut self, key: &str, well: (f64, f64), crop: Option<Area>) {
        let Some(running) = self.running.get(key) else {
            return;
        };
        let (generation, stream) = (running.generation, running.stream.clone());
        let backend = self.backend.clone();
        let started = Instant::now();
        let result = bounded(
            self.call_timeout,
            move || backend.resize(&stream, well, crop),
            |_| {},
        );
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Some(Ok(())) => {
                tracing::info!(target: "pip", session = %key, generation, ?well, ?crop, elapsed_ms, "PiP stream resized");
                (self.deliver)(Event::Reframed {
                    key: key.to_owned(),
                    generation,
                    crop,
                });
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

    fn open(
        &mut self,
        key: String,
        generation: u64,
        target: Target,
        well: (f64, f64),
        crop: Option<Area>,
        attempt: u32,
    ) {
        let started = Instant::now();
        let hung_stops = self.settle_stops();
        let backend = self.backend.clone();
        let late_backend = self.backend.clone();
        let deliver = self.deliver.clone();
        let call_key = key.clone();
        let result = bounded(
            self.open_timeout,
            move || backend.open(&call_key, generation, target, well, crop, &deliver),
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
                tracing::info!(target: "pip", session = %key, generation, ?target, ?crop, attempt, hung_stops, elapsed_ms, "PiP stream started");
                // The crop it opened with (a reconfiguration may have folded
                // into the open after the panel asked for it).
                (self.deliver)(Event::Reframed {
                    key: key.clone(),
                    generation,
                    crop,
                });
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
                        crop,
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
                // Not started yet: it just starts at the new size and crop.
                (
                    Request::Resize {
                        well: new,
                        crop: new_crop,
                    },
                    Some(Request::Start { well, crop, .. }),
                ) => {
                    *well = new;
                    *crop = new_crop;
                }
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
        crop: Option<Area>,
        deliver: &Deliver,
    ) -> anyhow::Result<SCStream> {
        open_stream(key, generation, target, well, crop, deliver)
    }

    fn stop(&self, stream: &SCStream) {
        let _ = stream.stop_capture();
    }

    fn resize(&self, stream: &SCStream, well: (f64, f64), crop: Option<Area>) -> anyhow::Result<()> {
        stream
            .update_configuration(&stream_config(well, crop))
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// What the stream does with one sample, by its `SCStreamFrameInfo.status`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FrameAction {
    /// Put its pixels on the panel.
    Show,
    /// No new pixels (idle, blank, suspended).
    Skip,
    /// The stream stopped (window gone): fall back to stills.
    Ended,
}

/// Complete and started frames carry new pixels. A frame whose status cannot
/// be read is shown if it has a pixel buffer rather than silently dropped.
fn frame_action(status: Option<SCFrameStatus>) -> FrameAction {
    match status {
        Some(SCFrameStatus::Complete | SCFrameStatus::Started) | None => FrameAction::Show,
        Some(SCFrameStatus::Stopped) => FrameAction::Ended,
        Some(SCFrameStatus::Idle | SCFrameStatus::Blank | SCFrameStatus::Suspended) => {
            FrameAction::Skip
        }
    }
}

/// A sample's `SCStreamFrameInfo.status`, read through CoreMedia.
///
/// Not `CMSampleBufferSCExt::frame_status`: screencapturekit 8.0.1's bridge
/// casts the attachment (an `NSNumber`) with `as? SCFrameStatus`, which is
/// always nil, so that returns `None` for every frame.
fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
    use core_foundation::number::{CFNumber, CFNumberGetTypeID, CFNumberRef};
    use core_foundation::string::CFStringRef;

    #[link(name = "CoreMedia", kind = "framework")]
    extern "C" {
        fn CMSampleBufferGetSampleAttachmentsArray(
            sample: *mut std::ffi::c_void,
            create_if_necessary: u8,
        ) -> CFArrayRef;
    }
    #[link(name = "ScreenCaptureKit", kind = "framework")]
    extern "C" {
        static SCStreamFrameInfoStatus: CFStringRef;
    }
    // SAFETY: `sample` is a live CMSampleBuffer; the attachments array and
    // its values follow the get rule (borrowed for the sample's lifetime).
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample.as_ptr(), 0);
        if attachments.is_null() || CFArrayGetCount(attachments) < 1 {
            return None;
        }
        let first = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
        if first.is_null() {
            return None;
        }
        let value = CFDictionaryGetValue(first, SCStreamFrameInfoStatus.cast());
        if value.is_null()
            || core_foundation::base::CFGetTypeID(value) != CFNumberGetTypeID()
        {
            return None;
        }
        let raw = CFNumber::wrap_under_get_rule(value as CFNumberRef).to_i32()?;
        SCFrameStatus::from_raw(raw)
    }
}

/// How often a stream (and the panel) logs its frame counts.
const STATS_EVERY: Duration = Duration::from_secs(2);

/// Frames one stream received on ScreenCaptureKit's queue, by outcome.
#[derive(Debug, Default)]
struct FrameStats {
    received: u64,
    complete: u64,
    started: u64,
    idle_blank_suspended: u64,
    stopped: u64,
    status_unreadable: u64,
    /// Show frames without a pixel buffer.
    no_buffer: u64,
    /// Handed to the main queue (the slot was empty).
    handed_off: u64,
    /// Replaced a frame the main queue had not taken yet.
    replaced: u64,
    last_summary: Option<Instant>,
}

impl FrameStats {
    /// Count one frame. Returns true for the stream's first frame.
    fn count(
        &mut self,
        status: Option<SCFrameStatus>,
        action: FrameAction,
        handed_off: Option<bool>,
    ) -> bool {
        self.received += 1;
        match status {
            Some(SCFrameStatus::Complete) => self.complete += 1,
            Some(SCFrameStatus::Started) => self.started += 1,
            Some(SCFrameStatus::Stopped) => self.stopped += 1,
            Some(_) => self.idle_blank_suspended += 1,
            None => self.status_unreadable += 1,
        }
        match (action, handed_off) {
            (FrameAction::Show, None) => self.no_buffer += 1,
            (_, Some(true)) => self.handed_off += 1,
            (_, Some(false)) => self.replaced += 1,
            _ => {}
        }
        self.received == 1
    }

    fn summary_due(&mut self, now: Instant) -> bool {
        summary_due(&mut self.last_summary, now)
    }
}

/// Whether a periodic summary is due at `now`. The first call only starts
/// the clock (the first frame has its own line).
fn summary_due(last: &mut Option<Instant>, now: Instant) -> bool {
    match *last {
        Some(at) if now.duration_since(at) < STATS_EVERY => false,
        Some(_) => {
            *last = Some(now);
            true
        }
        None => {
            *last = Some(now);
            false
        }
    }
}

/// Live frames the main queue got for a panel's current stream, by outcome.
#[derive(Debug, Default)]
pub(super) struct PanelFrames {
    /// Frame events for the current generation.
    pub(super) events: u64,
    /// Frames put on the live layer and left visible.
    pub(super) shown: u64,
    /// The slot was already empty (nothing to show).
    pub(super) empty_slot: u64,
    /// The pixel buffer had no IOSurface.
    pub(super) no_iosurface: u64,
    /// Put on the layer but hidden: its tag is not the current target's.
    pub(super) hidden_by_tags: u64,
    last_summary: Option<Instant>,
}

impl PanelFrames {
    pub(super) fn summary_due(&mut self, now: Instant) -> bool {
        summary_due(&mut self.last_summary, now)
    }
}

/// Build and start a window stream.
fn open_stream(
    key: &str,
    generation: u64,
    target: Target,
    well: (f64, f64),
    crop: Option<Area>,
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
    let config = stream_config(well, crop);

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
    let stats = Mutex::new(FrameStats::default());
    stream
        .add_output_handler(
            move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                if of_type != SCStreamOutputType::Screen {
                    return;
                }
                let status = frame_status(&sample);
                let action = frame_action(status);
                let buffer = match action {
                    FrameAction::Show => sample.image_buffer(),
                    _ => None,
                };
                // Wake the main queue only when the slot was empty; a frame
                // already waiting there is simply replaced by this newer one.
                let handed_off =
                    buffer.map(|buffer| lock(&slot).replace(buffer).is_none());
                let mut counts = lock(&stats);
                if counts.count(status, action, handed_off) {
                    tracing::info!(target: "pip", session = %frame_key, generation, ?status, ?action, ?handed_off, "PiP stream first frame");
                } else if counts.summary_due(Instant::now()) {
                    tracing::info!(target: "pip", session = %frame_key, generation, counts = ?*counts, "PiP stream frames");
                }
                drop(counts);
                match (action, handed_off) {
                    // The window is gone: fall back to stills.
                    (FrameAction::Ended, _) => {
                        tracing::info!(target: "pip", session = %frame_key, generation, "PiP live stream window gone (frame status Stopped)");
                        frames(Event::Ended {
                            key: frame_key.clone(),
                            generation,
                        });
                    }
                    (_, Some(true)) => frames(Event::Frame {
                        key: frame_key.clone(),
                        generation,
                        slot: slot.clone(),
                    }),
                    _ => {}
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
        assert_eq!(stream_step(true, A, false, None, None), StreamStep::Start(A));
        assert_eq!(stream_step(true, A, false, None, Some((A, false))), StreamStep::Keep);
    }

    #[test]
    fn a_new_target_switches_the_stream() {
        assert_eq!(stream_step(true, B, false, None, Some((A, false))), StreamStep::Start(B));
    }

    #[test]
    fn a_hidden_panel_stops_its_stream() {
        assert_eq!(stream_step(false, A, false, None, Some((A, false))), StreamStep::Stop);
        assert_eq!(stream_step(false, A, false, None, None), StreamStep::Keep);
    }

    #[test]
    fn no_target_means_no_stream() {
        assert_eq!(stream_step(true, (None, None), false, None, None), StreamStep::Keep);
        assert_eq!(
            stream_step(true, (None, None), false, None, Some((A, false))),
            StreamStep::Stop
        );
    }

    #[test]
    fn a_pid_only_target_follows_its_resolved_window() {
        const PID_ONLY: Target = (Some(42), None);
        let on = |window| (Some(42), Some(window));
        assert_eq!(
            stream_step(true, PID_ONLY, false, Some(5), None),
            StreamStep::Start(on(5))
        );
        assert_eq!(
            stream_step(true, PID_ONLY, false, Some(5), Some((on(5), false))),
            StreamStep::Keep
        );
        // The app raised another window: same pid-only target, new stream.
        assert_eq!(
            stream_step(true, PID_ONLY, false, Some(6), Some((on(5), false))),
            StreamStep::Start(on(6))
        );
        // No window to resolve right now: the same app's stream stays.
        assert_eq!(
            stream_step(true, PID_ONLY, false, None, Some((on(5), false))),
            StreamStep::Keep
        );
        // A different app with no resolvable window must not keep the old
        // app's live pixels under its label.
        assert_eq!(
            stream_step(true, (Some(43), None), false, None, Some((on(5), false))),
            StreamStep::Stop
        );
        assert_eq!(stream_step(true, PID_ONLY, false, None, None), StreamStep::Keep);
        // A hidden panel still stops.
        assert_eq!(
            stream_step(false, PID_ONLY, false, Some(5), Some((on(5), false))),
            StreamStep::Stop
        );
    }

    #[test]
    fn a_frame_arriving_after_the_stream_ended_is_dropped() {
        let mut state = StreamState::default();
        state.begin((A, false), 3, (320.0, 200.0), None);
        assert!(state.accepts(3));
        assert!(state.end(3));
        // SCK's sample handler can still deliver a frame of generation 3.
        assert!(!state.accepts(3));
        // Retry suppression survives: the failed target is still requested.
        assert_eq!(
            stream_step(true, A, false, None, state.requested),
            StreamStep::Keep
        );
        // A repeated or stale end changes nothing.
        assert!(!state.end(3));
        assert!(!state.end(2));
        // A new stream gets a fresh generation and works again.
        state.begin((B, false), 4, (320.0, 200.0), None);
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
        state.begin((A, false), 1, (320.0, 200.0), None);
        assert!(!state.needs_resize((320.0, 200.0), None));
        assert!(state.needs_resize((400.0, 250.0), None));
        assert!(!state.needs_resize((400.0, 250.0), None));
        // No running stream: nothing to reconfigure.
        state.stop();
        assert!(!state.needs_resize((500.0, 300.0), None));
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
                crop: None,
            },
        );
        streams.request("s", Request::Resize { well: (400.0, 250.0), crop: None });
        assert!(matches!(
            streams.next(None).unwrap(),
            (_, Request::Start { generation: 1, well, .. }) if well == (400.0, 250.0)
        ));
        streams.request("t", Request::Stop);
        streams.request("t", Request::Resize { well: (1.0, 1.0), crop: None });
        assert!(matches!(streams.next(None).unwrap(), (key, Request::Stop) if key == "t"));
        // With nothing pending, a resize is queued for the stream thread.
        streams.request("u", Request::Resize { well: (1.0, 1.0), crop: None });
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
            crop: None,
        };
        streams.request("s", start(1));
        streams.request("t", start(2));
        streams.request("s", Request::Stop);
        assert!(matches!(streams.next(None).unwrap(), (key, Request::Stop) if key == "s"));
        assert!(
            matches!(streams.next(None).unwrap(), (key, Request::Start { generation: 2, .. }) if key == "t")
        );
    }

    #[test]
    fn only_complete_and_started_frames_are_shown_and_stopped_ends() {
        use SCFrameStatus::*;
        assert_eq!(frame_action(Some(Complete)), FrameAction::Show);
        assert_eq!(frame_action(Some(Started)), FrameAction::Show);
        assert_eq!(frame_action(Some(Stopped)), FrameAction::Ended);
        for status in [Idle, Blank, Suspended] {
            assert_eq!(frame_action(Some(status)), FrameAction::Skip);
        }
        // An unreadable status must not silently drop a frame with pixels.
        assert_eq!(frame_action(None), FrameAction::Show);
    }

    /// A sample buffer carrying the status attachment the way ScreenCaptureKit
    /// sets it: an NSNumber under `SCStreamFrameInfoStatus`.
    fn sample_with_status(raw: i32) -> CMSampleBuffer {
        use core_foundation::base::TCFType;
        use core_foundation::number::CFNumber;
        use std::ffi::c_void;
        #[link(name = "CoreMedia", kind = "framework")]
        extern "C" {
            fn CMSampleBufferCreate(
                allocator: *const c_void,
                data_buffer: *const c_void,
                data_ready: u8,
                make_ready_callback: *const c_void,
                make_ready_refcon: *const c_void,
                format_description: *const c_void,
                num_samples: i64,
                num_timing_entries: i64,
                timing_array: *const c_void,
                num_size_entries: i64,
                size_array: *const c_void,
                out: *mut *mut c_void,
            ) -> i32;
            fn CMSampleBufferGetSampleAttachmentsArray(
                sample: *mut c_void,
                create_if_necessary: u8,
            ) -> *const c_void;
        }
        #[link(name = "ScreenCaptureKit", kind = "framework")]
        extern "C" {
            static SCStreamFrameInfoStatus: *const c_void;
        }
        unsafe {
            let mut sample = std::ptr::null_mut();
            let null = std::ptr::null();
            let status = CMSampleBufferCreate(
                null, null, 1, null, null, null, 1, 0, null, 0, null, &mut sample,
            );
            assert_eq!(status, 0, "CMSampleBufferCreate");
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 1);
            let first = core_foundation::array::CFArrayGetValueAtIndex(attachments.cast(), 0);
            let number = CFNumber::from(raw);
            core_foundation::dictionary::CFDictionarySetValue(
                first as _,
                SCStreamFrameInfoStatus,
                number.as_CFTypeRef(),
            );
            CMSampleBuffer::from_raw(sample).expect("sample")
        }
    }

    #[test]
    fn frame_status_reads_the_attachment_screencapturekit_sets() {
        use screencapturekit::prelude::CMSampleBufferSCExt;
        let complete = sample_with_status(0);
        assert_eq!(frame_status(&complete), Some(SCFrameStatus::Complete));
        assert_eq!(frame_status(&sample_with_status(1)), Some(SCFrameStatus::Idle));
        assert_eq!(frame_status(&sample_with_status(5)), Some(SCFrameStatus::Stopped));
        // The crate's reader casts the NSNumber to the enum and gets nil, so
        // it reports no status for every real frame; the old handler then
        // dropped them all.
        assert_eq!(complete.frame_status(), None);
    }

    #[test]
    fn summaries_start_after_the_first_frame_and_repeat_every_period() {
        let start = Instant::now();
        let mut last = None;
        assert!(!summary_due(&mut last, start));
        assert!(!summary_due(&mut last, start + STATS_EVERY / 2));
        assert!(summary_due(&mut last, start + STATS_EVERY));
        assert!(!summary_due(&mut last, start + STATS_EVERY + STATS_EVERY / 2));
        assert!(summary_due(&mut last, start + STATS_EVERY * 2));
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
        /// The well size each open asked for.
        open_wells: Mutex<Vec<(f64, f64)>>,
        hang_stop: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        hang_resize: std::sync::atomic::AtomicBool,
        fail_resize: std::sync::atomic::AtomicBool,
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
            well: (f64, f64),
            _crop: Option<Area>,
            _deliver: &Deliver,
        ) -> anyhow::Result<FakeStream> {
            use std::sync::atomic::Ordering::SeqCst;
            lock(&self.0.open_wells).push(well);
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

        fn resize(
            &self,
            _stream: &FakeStream,
            _well: (f64, f64),
            _crop: Option<Area>,
        ) -> anyhow::Result<()> {
            while self.0.hang_resize.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.0.fail_resize.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(anyhow::anyhow!("configuration rejected"));
            }
            Ok(())
        }
    }

    type Ended = Arc<Mutex<Vec<(String, u64)>>>;
    type Reframed = Arc<Mutex<Vec<(u64, Option<Area>)>>>;

    fn worker(fake: &Fake) -> (Worker<Fake>, Ended) {
        let (worker, ended, _) = worker_reporting(fake);
        (worker, ended)
    }

    fn worker_reporting(fake: &Fake) -> (Worker<Fake>, Ended, Reframed) {
        let ended: Ended = Arc::default();
        let reframed: Reframed = Arc::default();
        let (sink, crops) = (ended.clone(), reframed.clone());
        let deliver: Deliver = Arc::new(move |event| match event {
            Event::Ended { key, generation } => lock(&sink).push((key, generation)),
            Event::Reframed {
                generation, crop, ..
            } => lock(&crops).push((generation, crop)),
            Event::Frame { .. } => {}
        });
        let mut worker = Worker::new(fake.clone(), deliver);
        worker.call_timeout = Duration::from_millis(100);
        worker.open_timeout = Duration::from_millis(100);
        worker.retry_delay = Duration::from_millis(10);
        (worker, ended, reframed)
    }

    fn start(generation: u64) -> Request {
        Request::Start {
            generation,
            target: A,
            well: (320.0, 200.0),
            crop: None,
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

        worker.handle("a".into(), Request::Resize { well: (400.0, 250.0), crop: None });
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
    fn a_resize_keeps_a_pending_retry_and_it_opens_at_the_new_size() {
        let fake = Fake::default();
        lock(&fake.0.plan).push_back(Open::Err);
        let (mut worker, ended) = worker(&fake);
        worker.handle("a".into(), start(1));
        assert!(worker.next_retry().is_some());
        worker.handle("a".into(), Request::Resize { well: (400.0, 250.0), crop: None });
        assert!(worker.next_retry().is_some(), "the resize cancelled the retry");
        worker.retry_due(Instant::now() + Duration::from_secs(60));
        assert!(worker.running.contains_key("a"));
        assert_eq!(*lock(&fake.0.open_wells), [(320.0, 200.0), (400.0, 250.0)]);
        assert!(lock(&ended).is_empty());
        // Nothing running or pending: a resize is a no-op.
        worker.handle("b".into(), Request::Resize { well: (1.0, 1.0), crop: None });
        assert!(!worker.running.contains_key("b") && worker.next_retry().is_none());
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

    const PAGE: Area = Area {
        x: 0.0,
        y: 87.0,
        w: 1100.0,
        h: 702.0,
    };

    #[test]
    fn a_new_framing_of_the_same_window_is_a_new_stream() {
        // A browser action streams the page of window 7 ...
        assert_eq!(stream_step(true, A, true, None, None), StreamStep::Start(A));
        assert_eq!(stream_step(true, A, true, None, Some((A, true))), StreamStep::Keep);
        // ... a native action on the same window streams the whole window ...
        assert_eq!(stream_step(true, A, false, None, Some((A, true))), StreamStep::Start(A));
        // ... and a browser action again, the page.
        assert_eq!(stream_step(true, A, true, None, Some((A, false))), StreamStep::Start(A));
    }

    #[test]
    fn a_new_crop_reconfigures_even_at_the_same_size_and_is_shown_only_once_applied() {
        let mut state = StreamState::default();
        state.begin((A, true), 1, (320.0, 200.0), None);
        // Opened before the page was known: the whole window.
        assert_eq!(state.shown_crop, None);
        assert!(state.needs_resize((320.0, 200.0), Some(PAGE)));
        assert!(!state.needs_resize((320.0, 200.0), Some(PAGE)));
        // The banner went away: same size, the page's origin moved up.
        let moved = Area { y: 60.0, ..PAGE };
        assert!(state.needs_resize((320.0, 200.0), Some(moved)));
        // Asked for, not applied yet: the frames still show the old crop.
        assert_eq!(state.shown_crop, None);
        assert!(state.reframed(1, Some(moved)));
        assert_eq!(state.shown_crop, Some(moved));
        // A late report from an ended stream changes nothing.
        state.end(1);
        assert!(!state.reframed(1, Some(PAGE)));
        assert_eq!(state.shown_crop, Some(moved));
        // A new stream shows the crop it opens with.
        state.begin((A, true), 2, (320.0, 200.0), Some(PAGE));
        assert_eq!(state.shown_crop, Some(PAGE));
    }

    #[test]
    fn only_a_reconfiguration_that_succeeded_reports_its_crop() {
        use std::sync::atomic::Ordering::SeqCst;
        let fake = Fake::default();
        let (mut worker, ended, reframed) = worker_reporting(&fake);
        worker.handle("a".into(), start(1));
        // The open reports the crop it opened with.
        assert_eq!(*lock(&reframed), vec![(1, None)]);
        let resize = |crop| Request::Resize {
            well: (320.0, 200.0),
            crop,
        };
        // Applied: the new crop is reported.
        worker.handle("a".into(), resize(Some(PAGE)));
        assert_eq!(lock(&reframed).last(), Some(&(1, Some(PAGE))));
        // Rejected: the old stream (and its crop) stays, nothing reported.
        fake.0.fail_resize.store(true, SeqCst);
        worker.handle("a".into(), resize(None));
        assert_eq!(lock(&reframed).len(), 2);
        assert!(worker.running.contains_key("a") && lock(&ended).is_empty());
        fake.0.fail_resize.store(false, SeqCst);
        // Timed out: the stream is abandoned as a stopped one, nothing
        // reported.
        fake.0.hang_resize.store(true, SeqCst);
        worker.handle("a".into(), resize(None));
        fake.0.hang_resize.store(false, SeqCst);
        assert_eq!(lock(&reframed).len(), 2);
        assert_eq!(*lock(&ended), vec![("a".to_owned(), 1)]);
    }

    #[test]
    fn a_crop_asked_for_before_the_stream_opens_is_what_it_opens_with() {
        let streams = Streams {
            requests: Mutex::new(LatestPerSession::new()),
            ready: Condvar::new(),
        };
        streams.request("s", start(1));
        streams.request(
            "s",
            Request::Resize {
                well: (320.0, 200.0),
                crop: Some(PAGE),
            },
        );
        let (key, request) = streams.next(None).unwrap();
        assert!(matches!(request, Request::Start { crop: Some(PAGE), .. }));
        // And the open reports it, so the panel maps the cursor into it.
        let fake = Fake::default();
        let (mut worker, _, reframed) = worker_reporting(&fake);
        worker.handle(key, request);
        assert_eq!(*lock(&reframed), vec![(1, Some(PAGE))]);
    }
}
