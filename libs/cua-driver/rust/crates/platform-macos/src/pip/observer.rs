//! Live exact-window preview owned by the private companion process.
//!
//! Stream configuration and retained CGImage-to-NSImage presentation adapt
//! httxoxiyx's contribution in trycua/cua#3497. Capture is restricted to the
//! selected window and all native capture lives in the companion.

use std::ffi::c_void;
use std::io::BufReader;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::Duration;

use pip_preview::{
    observer::{read_update, ObserverUpdate},
    PipConfig,
};
use screencapturekit::prelude::{
    CMSampleBufferExt, CMSampleBufferSCExt, CMTime, SCStream, SCStreamOutputType,
};

static DESIRED_GENERATION: AtomicU64 = AtomicU64::new(0);
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

fn request_refresh() {
    if !REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        super::dispatch_to_main((), render);
    }
}

#[derive(Default)]
struct Presentation<T> {
    generation: u64,
    frame: Option<T>,
    label: String,
    clear: bool,
    scheduled: bool,
    available: bool,
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

fn presentation() -> &'static Mutex<Presentation<screencapturekit::CGImage>> {
    static STATE: OnceLock<Mutex<Presentation<screencapturekit::CGImage>>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(Presentation {
            generation: 0,
            frame: None,
            label: String::new(),
            clear: true,
            scheduled: false,
            available: false,
        })
    })
}

fn select_display(generation: u64, label: String) {
    let schedule = presentation().lock().unwrap().select(generation, label);
    if schedule {
        request_refresh();
    }
}

fn unavailable_display(generation: u64) {
    let schedule = presentation().lock().unwrap().unavailable(generation);
    if schedule {
        request_refresh();
    }
}

fn activate_display(update: &ObserverUpdate) -> bool {
    let mut state = presentation().lock().unwrap();
    if DESIRED_GENERATION.load(Ordering::Acquire) != update.generation
        || !state.activate(update.generation)
    {
        return false;
    }
    let schedule = state.select(update.generation, update.action_label.clone());
    drop(state);
    if schedule {
        request_refresh();
    }
    true
}

fn publish_frame(generation: u64, image: screencapturekit::CGImage) {
    if DESIRED_GENERATION.load(Ordering::Acquire) != generation {
        return;
    }
    // Dropping a frame is preferable to making ScreenCaptureKit wait for AppKit.
    let schedule = match presentation().try_lock() {
        Ok(mut state) => state.frame(generation, image),
        Err(_) => false,
    };
    if schedule {
        request_refresh();
    }
}

fn publish_control(
    update: ObserverUpdate,
    sender: &tokio::sync::watch::Sender<Option<ObserverUpdate>>,
) {
    tracing::debug!(generation = update.generation, target = ?update.target, "preview target received");
    // This reader must stay independent of native capture AND AppKit, so it
    // can terminate the helper on EOF even if either subsystem hangs.
    DESIRED_GENERATION.store(update.generation, Ordering::Release);
    sender.send_replace(Some(update));
    request_refresh();
}

#[repr(C)]
struct NativeCGImage {
    _opaque: [u8; 0],
}
unsafe impl objc2::RefEncode for NativeCGImage {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGImage", &[]));
}

unsafe extern "C" fn render(ctx: *mut c_void) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSSize;
    drop(Box::from_raw(ctx as *mut ()));
    REFRESH_PENDING.store(false, Ordering::Release);
    // Capture can drop frames while this helper-only lock is held. The control
    // reader never takes it, and can still receive EOF during an AppKit stall.
    let mut state = presentation().lock().unwrap();
    state.scheduled = false;
    if state.generation != DESIRED_GENERATION.load(Ordering::Acquire) {
        state.frame = None;
        state.clear = true;
    }
    let handles = super::HANDLES.lock().unwrap();
    let Some(handles) = handles.as_ref() else {
        return;
    };
    let view = handles.image_view as *mut AnyObject;
    if state.clear {
        let _: () = msg_send![view, setImage: std::ptr::null_mut::<AnyObject>()];
        state.clear = false;
    }
    if let Some(frame) = state.frame.take() {
        let cg_image = frame.as_ptr() as *mut NativeCGImage;
        let alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let image: *mut AnyObject =
            msg_send![alloc, initWithCGImage: cg_image size: NSSize::new(0.0, 0.0)];
        if !image.is_null() {
            let _: () = msg_send![view, setImage: image];
            let _: () = msg_send![image, release];
        }
    }
    let changed = state.generation != DESIRED_GENERATION.load(Ordering::Acquire);
    if changed {
        let _: () = msg_send![view, setImage: std::ptr::null_mut::<AnyObject>()];
    }
    let label = if changed {
        "Switching preview"
    } else {
        state.label.as_str()
    };
    if let Ok(label) = std::ffi::CString::new(label) {
        let text: *mut AnyObject =
            msg_send![class!(NSString), stringWithUTF8String: label.as_ptr() as *const u8];
        let label = handles.label as *mut AnyObject;
        let _: () = msg_send![label, setStringValue: text];
    }
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

fn capture(update: &ObserverUpdate) -> anyhow::Result<SCStream> {
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
                if DESIRED_GENERATION.load(Ordering::Acquire) != generation
                    || kind != SCStreamOutputType::Screen
                    || sample
                        .frame_status()
                        .is_some_and(|status| !status.has_content())
                {
                    return;
                }
                if let Ok(image) = sample.cg_image() {
                    publish_frame(generation, image);
                }
            },
            SCStreamOutputType::Screen,
        )
        .ok_or_else(|| anyhow::anyhow!("preview stream rejected output"))?;
    // ScreenCaptureKit can deliver the only complete frame of a static window
    // before start_capture returns. Admit that frame before starting the stream.
    anyhow::ensure!(
        activate_display(update),
        "preview target superseded before capture"
    );
    if let Err(error) = stream.start_capture() {
        unavailable_display(generation);
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
    super::dispatch_to_main(cfg, super::init_cb);
    let (sender, mut receiver) = tokio::sync::watch::channel::<Option<ObserverUpdate>>(None);
    std::thread::Builder::new()
        .name("cua-preview-control".into())
        .spawn(move || {
            let mut reader = BufReader::new(std::io::stdin());
            let mut previous = None::<ObserverUpdate>;
            let code = loop {
                match read_update(&mut reader) {
                    Ok(Some(update)) => {
                        if previous.as_ref().is_some_and(|old| !update.follows(old)) {
                            break 1;
                        }
                        previous = Some(update.clone());
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
                let mut active = None::<SCStream>;
                let mut active_key = None;
                loop {
                    let update = receiver.borrow_and_update().clone();
                    if let Some(update) = update {
                        select_display(update.generation, update.action_label.clone());
                        let key = update.target.and_then(|target| {
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
                        });
                        if key.is_none() {
                            tracing::debug!(generation = update.generation, target = ?update.target,
                                "preview target absent or no longer matches WindowServer");
                        }
                        if key != active_key || (key.is_some() && active.is_none()) {
                            unavailable_display(update.generation);
                            if let Some(stream) = active.take() {
                                let _ = stream.stop_capture();
                            }
                            active_key = key;
                            if key.is_some() {
                                match capture(&update) {
                                    Ok(stream) => {
                                        // A newer target may have arrived during startup.
                                        if activate_display(&update) {
                                            active = Some(stream);
                                        } else {
                                            let _ = stream.stop_capture();
                                            active_key = None;
                                        }
                                    }
                                    Err(error) => {
                                        tracing::debug!(%error, "preview capture unavailable");
                                    }
                                }
                            }
                        }
                        if key.is_none() {
                            unavailable_display(update.generation);
                        }
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
    fn capture_dimensions_preserve_aspect_and_bound_large_and_thin_windows() {
        assert_eq!(preview_dimensions(3840, 2160), (1280, 720));
        assert_eq!(preview_dimensions(600, 800), (600, 800));
        assert_eq!(preview_dimensions(1, 10000), (1, 1280));
    }

    #[test]
    fn control_reader_does_not_wait_for_the_renderer_lock() {
        let held_ui = presentation().lock().unwrap();
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let (done, finished) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            publish_control(
                ObserverUpdate::new(1, Some(7), Some(42), "click", 12),
                &sender,
            );
            done.send(()).unwrap();
        });
        let early = finished.recv_timeout(Duration::from_secs(1));
        drop(held_ui);
        reader.join().unwrap();
        assert!(
            early.is_ok(),
            "control reader could not reach EOF while AppKit held the renderer lock"
        );
        assert_eq!(receiver.borrow().as_ref().unwrap().generation, 1);
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
