//! Live exact-window preview owned by the private companion process.
//!
//! Stream configuration and retained CGImage-to-NSImage presentation adapt
//! httxoxiyx's contribution in trycua/cua#3497. Capture is restricted to the
//! selected window and all native capture lives in the companion.

use std::ffi::c_void;
use std::io::BufReader;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock,
};
use std::time::Duration;

use pip_preview::{
    observer::ObserverUpdate,
    session_observer::{read_snapshot, ObserverSnapshot, SnapshotReceiver},
    PipConfig,
};
use screencapturekit::prelude::{
    CMSampleBufferExt, CMSampleBufferSCExt, CMTime, SCStream, SCStreamOutputType,
};

use std::collections::BTreeMap;
static DESIRED: OnceLock<tokio::sync::watch::Receiver<Option<ObserverSnapshot>>> = OnceLock::new();
fn desired() -> Option<ObserverSnapshot> {
    DESIRED.get().and_then(|r| r.borrow().clone())
}
pub(super) fn current(id: u64, generation: u64) -> bool {
    DESIRED.get().is_some_and(|r| {
        r.borrow().as_ref().is_some_and(|s| {
            s.previews
                .get(&id)
                .is_some_and(|u| u.generation == generation)
        })
    })
}
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

fn request_refresh() {
    if !REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        super::dispatch_to_main((), render);
    }
}

pub(super) struct Presentation<T> {
    generation: u64,
    frame: Option<T>,
    label: String,
    clear: bool,
    scheduled: bool,
    available: bool,
    pub(super) hidden: bool,
}

impl<T> Default for Presentation<T> {
    fn default() -> Self {
        Self {
            generation: 0,
            frame: None,
            label: String::new(),
            clear: true,
            scheduled: false,
            available: false,
            hidden: false,
        }
    }
}
impl<T> Presentation<T> {
    fn select(&mut self, generation: u64, label: String) -> bool {
        if generation < self.generation {
            return false;
        }
        if generation != self.generation {
            self.frame = None;
            self.clear = true;
            self.available = false;
        }
        self.generation = generation;
        self.label = label;
        self.schedule()
    }

    fn frame(&mut self, generation: u64, frame: T) -> bool {
        if generation == 0 || generation != self.generation || !self.available {
            return false;
        }
        self.frame = Some(frame);
        self.schedule()
    }

    fn activate(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.available = true;
        true
    }

    fn unavailable(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.available = false;
        self.frame = None;
        self.clear = true;
        self.label = "Preview unavailable".into();
        self.schedule()
    }

    fn schedule(&mut self) -> bool {
        let schedule = !self.scheduled;
        self.scheduled = true;
        schedule
    }
}

#[derive(Default)]
pub(super) struct Sessions<T> {
    pub(super) entries: BTreeMap<u64, Presentation<T>>,
    revision: u64,
}
pub(super) struct Draw<T> {
    pub(super) generation: u64,
    pub(super) frame: Option<T>,
    pub(super) clear: bool,
    pub(super) label: String,
}
impl<T> Sessions<T> {
    fn sync(&mut self, snapshot: &ObserverSnapshot) {
        if snapshot.revision <= self.revision {
            return;
        }
        self.revision = snapshot.revision;
        self.entries
            .retain(|id, _| snapshot.previews.contains_key(id));
        for (&id, update) in &snapshot.previews {
            self.entries
                .entry(id)
                .or_default()
                .select(update.generation, update.action_label.clone());
        }
    }
    fn frame(&mut self, id: u64, generation: u64, image: T) -> bool {
        self.entries
            .get_mut(&id)
            .is_some_and(|p| !p.hidden && p.frame(generation, image))
    }
    fn detach(&mut self) -> BTreeMap<u64, Draw<T>> {
        self.entries
            .iter_mut()
            .map(|(&id, p)| {
                p.scheduled = false;
                (
                    id,
                    Draw {
                        generation: p.generation,
                        frame: p.frame.take(),
                        clear: std::mem::take(&mut p.clear),
                        label: p.label.clone(),
                    },
                )
            })
            .collect()
    }
}
pub(super) fn presentation() -> &'static Mutex<Sessions<screencapturekit::CGImage>> {
    static STATE: OnceLock<Mutex<Sessions<screencapturekit::CGImage>>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(Sessions {
            entries: BTreeMap::new(),
            revision: 0,
        })
    })
}
fn unavailable_display(id: u64, generation: u64) {
    if let Some(state) = presentation().lock().unwrap().entries.get_mut(&id) {
        state.unavailable(generation);
    }
    request_refresh();
}
fn activate_display(id: u64, update: &ObserverUpdate) -> bool {
    if !current(id, update.generation) {
        return false;
    }
    let mut states = presentation().lock().unwrap();
    let Some(state) = states.entries.get_mut(&id) else {
        return false;
    };
    if state.hidden || !state.activate(update.generation) {
        return false;
    }
    state.select(update.generation, update.action_label.clone());
    drop(states);
    request_refresh();
    true
}
fn publish_frame(id: u64, generation: u64, image: screencapturekit::CGImage) {
    if !current(id, generation) {
        return;
    }
    // The lock covers only mailbox transfer. AppKit never holds it.
    if presentation().lock().unwrap().frame(id, generation, image) {
        request_refresh();
    }
}
fn publish_control(
    snapshot: ObserverSnapshot,
    sender: &tokio::sync::watch::Sender<Option<ObserverSnapshot>>,
) {
    sender.send_replace(Some(snapshot));
    request_refresh();
}

#[repr(C)]
pub(super) struct NativeCGImage {
    _opaque: [u8; 0],
}
unsafe impl objc2::RefEncode for NativeCGImage {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGImage", &[]));
}

unsafe extern "C" fn render(ctx: *mut c_void) {
    drop(Box::from_raw(ctx as *mut ()));
    REFRESH_PENDING.store(false, Ordering::Release);
    let Some(snapshot) = desired() else {
        return;
    };
    let draws = {
        let mut state = presentation().lock().unwrap();
        state.sync(&snapshot);
        state.detach()
    };
    super::observer_panels::render(&snapshot, draws);
}

fn preview_dimensions(width: u32, height: u32) -> (u32, u32) {
    let width = f64::from(width.max(1));
    let height = f64::from(height.max(1));
    let scale = (1280.0 / width.max(height)).min(1.0);
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
}

fn capture(id: u64, update: &ObserverUpdate) -> anyhow::Result<SCStream> {
    let target = update
        .target
        .ok_or_else(|| anyhow::anyhow!("preview has no target"))?;
    let (filter, config) =
        crate::capture::preview_window_capture_plan(target.window_id, target.pid)?;
    let (width, height) = preview_dimensions(config.width(), config.height());
    let config = config
        .with_width(width)
        .with_height(height)
        .with_queue_depth(3)
        .with_minimum_frame_interval(&CMTime::new(1, 8))
        .with_shows_cursor(false);
    let generation = update.generation;
    let mut stream = SCStream::new(&filter, &config);
    stream
        .add_output_handler(
            move |sample: screencapturekit::cm::CMSampleBuffer, kind| {
                if !current(id, generation)
                    || kind != SCStreamOutputType::Screen
                    || sample
                        .frame_status()
                        .is_some_and(|status| !status.has_content())
                {
                    return;
                }
                if let Ok(image) = sample.cg_image() {
                    publish_frame(id, generation, image);
                }
            },
            SCStreamOutputType::Screen,
        )
        .ok_or_else(|| anyhow::anyhow!("preview stream rejected output"))?;
    // ScreenCaptureKit can deliver the only complete frame of a static window
    // before start_capture returns. Admit that frame before starting the stream.
    anyhow::ensure!(
        activate_display(id, update),
        "preview target superseded before capture"
    );
    if let Err(error) = stream.start_capture() {
        unavailable_display(id, generation);
        anyhow::bail!("preview stream: {error}");
    }
    Ok(stream)
}

/// Main-thread entry point in the isolated, signed helper. No permission
/// requests are made here. A missing grant disables this observer only.
pub fn run(cfg: PipConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        crate::session::has_graphic_access(),
        "preview needs a GUI session"
    );
    anyhow::ensure!(
        crate::permissions::status::screen_recording_granted(),
        "preview needs an existing Screen Recording grant"
    );
    super::observer_panels::configure(cfg);
    let (sender, mut receiver) = tokio::sync::watch::channel::<Option<ObserverSnapshot>>(None);
    let _ = DESIRED.set(receiver.clone());
    std::thread::Builder::new()
        .name("cua-preview-control".into())
        .spawn(move || {
            let mut reader = BufReader::new(std::io::stdin());
            let mut previous = SnapshotReceiver::default();
            let code = loop {
                match read_snapshot(&mut reader) {
                    Ok(Some(update)) => {
                        if previous.accept(&update).is_err() {
                            break 1;
                        }
                        publish_control(update, &sender);
                    }
                    Ok(None) => break 0,
                    Err(error) => {
                        eprintln!("preview control: {error}");
                        break 1;
                    }
                }
            };
            // EOF owns this private helper's lifetime, including a native capture
            // call that never returns. No global endpoint or reconnection exists.
            std::process::exit(code);
        })?;
    std::thread::Builder::new()
        .name("cua-preview-capture".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("preview runtime: {error}");
                    std::process::exit(1);
                }
            };
            runtime.block_on(async {
                // ponytail: one capture worker serializes native startup; use per-owner workers if startup latency becomes material.
                let mut active = BTreeMap::<u64, (SCStream, (u64, i32, u32, u64, u64))>::new();
                loop {
                    let snapshot = receiver.borrow_and_update().clone();
                    if let Some(snapshot) = snapshot {
                        presentation().lock().unwrap().sync(&snapshot);
                        let removed: Vec<_> = active
                            .keys()
                            .filter(|id| !snapshot.previews.contains_key(id))
                            .copied()
                            .collect();
                        for id in removed {
                            if let Some((stream, _)) = active.remove(&id) {
                                let _ = stream.stop_capture();
                            }
                        }
                        for (&id, update) in &snapshot.previews {
                            let hidden = presentation()
                                .lock()
                                .unwrap()
                                .entries
                                .get(&id)
                                .is_none_or(|p| p.hidden);
                            let key = if hidden {
                                None
                            } else {
                                update.target.and_then(|target| {
                                    crate::windows::window_info_by_id(target.window_id)
                                        .filter(|window| window.pid == target.pid)
                                        .map(|window| {
                                            (
                                                update.generation,
                                                target.pid,
                                                target.window_id,
                                                window.bounds.width.to_bits(),
                                                window.bounds.height.to_bits(),
                                            )
                                        })
                                })
                            };
                            if active.get(&id).map(|(_, key)| *key) != key {
                                unavailable_display(id, update.generation);
                                if let Some((stream, _)) = active.remove(&id) {
                                    let _ = stream.stop_capture();
                                }
                            }
                            if let Some(key) = key {
                                if !active.contains_key(&id) {
                                    match capture(id, update) {
                                        Ok(stream) if current(id, update.generation) => {
                                            active.insert(id, (stream, key));
                                        }
                                        Ok(stream) => {
                                            let _ = stream.stop_capture();
                                        }
                                        Err(error) => {
                                            tracing::debug!(%error, "preview capture unavailable");
                                        }
                                    }
                                }
                            } else {
                                unavailable_display(id, update.generation);
                            }
                        }
                        request_refresh();
                    }
                    tokio::select! {
                        changed = receiver.changed() => { if changed.is_err() { return; } },
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    }
                }
            });
        })?;
    super::run_appkit_main_loop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn delayed_snapshot_cannot_resurrect_owner_or_reset_unavailable() {
        let mut state = Sessions::<u8>::default();
        state.sync(&snapshot(&[(1, 1)]));
        state.entries.get_mut(&1).unwrap().unavailable(1);
        state.sync(&snapshot(&[(1, 1)]));
        assert_eq!(state.entries[&1].label, "Preview unavailable");
        state.sync(&ObserverSnapshot {
            revision: 2,
            ..snapshot(&[])
        });
        state.sync(&snapshot(&[(1, 1)]));
        assert!(state.entries.is_empty());
    }

    #[test]
    fn control_reader_does_not_wait_for_presentation() {
        let held = presentation().lock().unwrap();
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            publish_control(snapshot(&[(1, 1)]), &sender);
            done.send(()).unwrap();
        });
        let early = finished.recv_timeout(Duration::from_secs(1));
        drop(held);
        worker.join().unwrap();
        assert!(early.is_ok());
        assert_eq!(receiver.borrow().as_ref().unwrap().previews.len(), 1);
    }

    #[test]
    fn owners_are_independent_and_removed_frames_are_rejected() {
        let mut state = Sessions::<u8>::default();
        state.sync(&snapshot(&[(1, 1), (2, 1)]));
        state.entries.get_mut(&1).unwrap().activate(1);
        state.entries.get_mut(&2).unwrap().activate(1);
        state.frame(1, 1, 10);
        state.frame(2, 1, 20);
        assert_eq!(state.entries[&1].frame, Some(10));
        assert_eq!(state.entries[&2].frame, Some(20));
        state.sync(&ObserverSnapshot {
            revision: 2,
            ..snapshot(&[(2, 1)])
        });
        assert!(!state.frame(1, 1, 99));
        assert_eq!(state.entries[&2].frame, Some(20));
    }

    #[test]
    fn detached_render_keeps_the_next_static_frame_and_hidden_state() {
        let mut state = Sessions::<u8>::default();
        state.sync(&snapshot(&[(1, 1)]));
        state.entries.get_mut(&1).unwrap().activate(1);
        let detached = state.detach();
        state.frame(1, 1, 42);
        drop(detached);
        assert_eq!(state.detach()[&1].frame, Some(42));
        state.entries.get_mut(&1).unwrap().hidden = true;
        state.sync(&ObserverSnapshot {
            revision: 2,
            ..snapshot(&[(1, 2)])
        });
        assert!(state.entries[&1].hidden);
        assert!(!state.frame(1, 2, 43));
    }

    fn snapshot(entries: &[(u64, u64)]) -> ObserverSnapshot {
        ObserverSnapshot {
            version: 2,
            revision: 1,
            previews: entries
                .iter()
                .map(|&(id, generation)| {
                    (
                        id,
                        ObserverUpdate::new(generation, Some(7), Some(42), "click", 1),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn capture_dimensions_preserve_aspect_and_bound_large_and_thin_windows() {
        assert_eq!(preview_dimensions(3840, 2160), (1280, 720));
        assert_eq!(preview_dimensions(600, 800), (600, 800));
        assert_eq!(preview_dimensions(1, 10000), (1, 1280));
    }

    #[test]
    fn switching_window_discards_pending_frame_and_rejects_old_capture() {
        let mut presentation = Presentation::<u8>::default();
        assert!(presentation.select(1, "first".into()));
        assert!(presentation.activate(1));
        assert!(!presentation.frame(1, 10));
        assert!(!presentation.select(2, "second".into()));
        assert!(presentation.clear);
        assert_eq!(presentation.frame, None);
        assert!(!presentation.frame(1, 11));
        assert_eq!(presentation.frame, None);
        assert!(presentation.activate(2));
        assert!(!presentation.frame(2, 20));
        assert_eq!(presentation.frame, Some(20));
    }

    #[test]
    fn repeated_actions_in_one_window_keep_the_pending_frame() {
        let mut presentation = Presentation::<u8>::default();
        presentation.select(1, "click".into());
        presentation.activate(1);
        presentation.frame(1, 10);
        presentation.select(1, "scroll".into());
        assert_eq!(presentation.frame, Some(10));
        assert_eq!(presentation.label, "scroll");
    }

    #[test]
    fn delayed_old_target_cannot_replace_a_newer_selection() {
        let mut presentation = Presentation::<u8>::default();
        presentation.select(2, "new".into());
        assert!(!presentation.select(1, "old".into()));
        assert!(!presentation.activate(1));
        presentation.frame(1, 10);
        assert_eq!(presentation.frame, None);
        assert_eq!(presentation.label, "new");
    }

    #[test]
    fn unavailable_target_rejects_frames_until_capture_is_ready_again() {
        let mut presentation = Presentation::<u8>::default();
        presentation.select(1, "click".into());
        assert!(!presentation.frame(1, 1));
        assert_eq!(presentation.frame, None);
        presentation.activate(1);
        presentation.frame(1, 1);
        presentation.unavailable(1);
        presentation.select(1, "scroll".into());
        presentation.frame(1, 2);
        assert_eq!(presentation.frame, None);
        assert!(presentation.clear);
        presentation.activate(1);
        presentation.frame(1, 3);
        assert_eq!(presentation.frame, Some(3));
    }

    #[test]
    fn superseded_frames_release_resources_without_queuing_more_draws() {
        struct Image(Arc<AtomicUsize>);
        impl Drop for Image {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let releases = Arc::new(AtomicUsize::new(0));
        let mut presentation = Presentation {
            generation: 1,
            frame: None,
            label: String::new(),
            clear: false,
            scheduled: false,
            available: true,
            hidden: false,
        };
        assert!(presentation.frame(1, Image(releases.clone())));
        for _ in 0..100 {
            assert!(!presentation.frame(1, Image(releases.clone())));
        }
        assert_eq!(releases.load(Ordering::SeqCst), 100);
        drop(presentation);
        assert_eq!(releases.load(Ordering::SeqCst), 101);
    }
}
