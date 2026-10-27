//! Optional per-process event taps with worker-owned native callback lifetimes.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, bail, Context, Result};
use core_foundation::base::{CFRelease, CFRetain};
use core_foundation::mach_port::{
    CFMachPortCreateRunLoopSource, CFMachPortInvalidate, CFMachPortIsValid, CFMachPortRef,
};
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, kCFRunLoopRunFinished, CFRunLoopAddSource, CFRunLoopGetCurrent,
    CFRunLoopRef, CFRunLoopRemoveSource, CFRunLoopRunInMode, CFRunLoopSourceInvalidate,
    CFRunLoopSourceIsValid, CFRunLoopSourceRef, CFRunLoopStop, CFRunLoopWakeUp,
};

type RawEvent = *mut c_void;
type Callback = unsafe extern "C" fn(*mut c_void, u32, RawEvent, *mut c_void) -> RawEvent;
type CreateTap = unsafe extern "C" fn(i32, u32, u32, u64, Callback, *mut c_void) -> CFMachPortRef;
type Handler = dyn Fn(u32, RawEvent) -> TapDecision + Send + Sync;
type Unavailable = dyn Fn() + Send + Sync;

const DISABLED_TIMEOUT: u32 = 0xffff_fffe;
const DISABLED_USER_INPUT: u32 = 0xffff_ffff;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventTapIsEnabled(tap: CFMachPortRef) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TapDecision {
    Pass,
    Drop,
}

struct Control {
    // Retained CFRunLoopRef; only Stop/Wake cross threads, never borrowed userdata.
    run_loop: Option<usize>,
    closing: AtomicBool,
    healthy: AtomicBool,
    on_unavailable: Arc<Unavailable>,
}

impl Control {
    fn unavailable(&self) {
        if self.healthy.swap(false, Ordering::AcqRel) {
            // Neither a native callback nor Drop may unwind into Core Foundation.
            let _ = catch_unwind(AssertUnwindSafe(|| (self.on_unavailable)()));
        }
    }

    fn request_close(&self) {
        self.closing.store(true, Ordering::Release);
        self.unavailable();
        if let Some(run_loop) = self.run_loop {
            // SAFETY: Control retains this run loop until its last Arc is dropped.
            unsafe {
                CFRunLoopStop(run_loop as CFRunLoopRef);
                CFRunLoopWakeUp(run_loop as CFRunLoopRef);
            }
        }
    }

    fn active(&self) -> bool {
        !self.closing.load(Ordering::Acquire) && self.healthy.load(Ordering::Acquire)
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Some(run_loop) = self.run_loop {
            // SAFETY: paired with the worker's retain of CFRunLoopGetCurrent().
            unsafe { CFRelease(run_loop as *const c_void) };
        }
    }
}

// Covers setup errors and worker unwinding as well as ordinary loop completion.
struct WorkerAvailability(Arc<Control>);

impl Drop for WorkerAvailability {
    fn drop(&mut self) {
        self.0.unavailable();
    }
}

struct CallbackContext {
    control: Arc<Control>,
    handler: Arc<Handler>,
}

impl CallbackContext {
    fn invoke(&self, event_type: u32, event: RawEvent) -> RawEvent {
        if matches!(event_type, DISABLED_TIMEOUT | DISABLED_USER_INPUT) {
            self.control.unavailable();
            return event;
        }
        if event.is_null() || !self.control.active() {
            return event;
        }
        match catch_unwind(AssertUnwindSafe(|| (self.handler)(event_type, event))) {
            Ok(TapDecision::Drop) => std::ptr::null_mut(),
            Ok(TapDecision::Pass) => event,
            Err(_) => {
                self.control.unavailable();
                event
            }
        }
    }
}

unsafe extern "C" fn callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: RawEvent,
    user_data: *mut c_void,
) -> RawEvent {
    if user_data.is_null() {
        return event;
    }
    // SAFETY: worker keeps this Box alive until its loop and native port are invalidated.
    let context = unsafe { &*(user_data as *const CallbackContext) };
    context.invoke(event_type, event)
}

struct NativeResources {
    port: CFMachPortRef,
    source: CFRunLoopSourceRef,
    run_loop: CFRunLoopRef,
}

impl Drop for NativeResources {
    fn drop(&mut self) {
        // SAFETY: all resources are worker-owned +1 references; callback has returned.
        unsafe {
            CGEventTapEnable(self.port, false);
            CFMachPortInvalidate(self.port);
            if !self.source.is_null() {
                CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopDefaultMode);
                CFRunLoopSourceInvalidate(self.source);
                CFRelease(self.source.cast());
            }
            CFRelease(self.port.cast());
        }
    }
}

/// Handlers borrow the original event for this callback only. Do not retain its
/// pointer, use async locks, or unwind. Availability notification must be short.
pub(crate) struct EventTap {
    control: Arc<Control>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl EventTap {
    pub(crate) fn start(
        pid: i32,
        event_types: &[u32],
        handler: impl Fn(u32, RawEvent) -> TapDecision + Send + Sync + 'static,
        on_unavailable: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        if pid <= 0 {
            bail!("event tap target pid is invalid");
        }
        let create = create_tap().context("per-process event tap is unavailable")?;
        Self::start_with(create, pid, format!("cua-focus-tap-{pid}"), event_types, handler, on_unavailable)
    }

    /// An active tap on the whole login session at the point where each
    /// event has been annotated with the process it will be delivered to
    /// (`kCGAnnotatedSessionEventTap`): a handler that drops an event there
    /// keeps it from every application.
    pub(crate) fn start_annotated_session(
        event_types: &[u32],
        handler: impl Fn(u32, RawEvent) -> TapDecision + Send + Sync + 'static,
        on_unavailable: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::start_with(
            annotated_session_tap_create,
            ANNOTATED_SESSION_TAP,
            "cua-key-guard-tap".into(),
            event_types,
            handler,
            on_unavailable,
        )
    }

    fn start_with(
        create: CreateTap,
        pid: i32,
        name: String,
        event_types: &[u32],
        handler: impl Fn(u32, RawEvent) -> TapDecision + Send + Sync + 'static,
        on_unavailable: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let mask = event_mask(event_types)?;
        let (ready, receiver) = mpsc::sync_channel(1);
        let handler: Arc<Handler> = Arc::new(handler);
        let on_unavailable: Arc<Unavailable> = Arc::new(on_unavailable);
        let worker = thread::Builder::new()
            .name(name)
            .spawn(move || {
                let mut ready = Some(ready);
                let result = run_worker(create, pid, mask, handler, on_unavailable, &mut ready);
                if let Err(error) = result {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(error.to_string()));
                    }
                }
            })
            .context("could not start event tap worker")?;
        match receiver.recv() {
            Ok(Ok(control)) => Ok(Self {
                control,
                worker: Mutex::new(Some(worker)),
            }),
            result => {
                let _ = worker.join();
                match result {
                    Ok(Err(error)) => Err(anyhow!(error)),
                    Err(_) => Err(anyhow!("event tap worker stopped during startup")),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.control.active()
    }

    pub(crate) fn close(&self) {
        self.control.request_close();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(worker) = worker {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
            // Own-worker close detaches; worker retains native resources until callback returns.
        }
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        self.close();
    }
}

/// `kCGAnnotatedSessionEventTap`.
const ANNOTATED_SESSION_TAP: i32 = 2;

/// `CGEventTapCreate` in the shape of the per-process creator: its first
/// argument is the tap location instead of a pid.
unsafe extern "C" fn annotated_session_tap_create(
    location: i32,
    place: u32,
    options: u32,
    mask: u64,
    callback: Callback,
    user_info: *mut c_void,
) -> CFMachPortRef {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            mask: u64,
            callback: Callback,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
    }
    unsafe { CGEventTapCreate(location as u32, place, options, mask, callback, user_info) }
}

fn create_tap() -> Option<CreateTap> {
    static CREATE: OnceLock<Option<CreateTap>> = OnceLock::new();
    *CREATE.get_or_init(|| {
        // SAFETY: this known optional export has the raw-u32 callback ABI above.
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CGEventTapCreateForPid".as_ptr()) };
        (!symbol.is_null())
            .then(|| unsafe { std::mem::transmute::<*mut c_void, CreateTap>(symbol) })
    })
}

fn event_mask(types: &[u32]) -> Result<u64> {
    if types.is_empty() {
        bail!("event tap requires at least one event type");
    }
    types.iter().try_fold(0u64, |mask, event_type| {
        let bit = 1u64
            .checked_shl(*event_type)
            .context("event tap type exceeds its mask")?;
        Ok(mask | bit)
    })
}

fn run_worker(
    create: CreateTap,
    pid: i32,
    mask: u64,
    handler: Arc<Handler>,
    on_unavailable: Arc<Unavailable>,
    ready: &mut Option<mpsc::SyncSender<Result<Arc<Control>, String>>>,
) -> Result<()> {
    // SAFETY: worker's current run loop is retained for cross-thread Stop/Wake.
    let run_loop = unsafe { CFRunLoopGetCurrent() };
    if run_loop.is_null() {
        bail!("event tap run loop is unavailable");
    }
    unsafe {
        CFRetain(run_loop.cast());
    }
    let control = Arc::new(Control {
        run_loop: Some(run_loop as usize),
        closing: AtomicBool::new(false),
        healthy: AtomicBool::new(true),
        on_unavailable,
    });
    let _availability = WorkerAvailability(control.clone());
    let mut context = Box::new(CallbackContext {
        control: control.clone(),
        handler,
    });
    // Head insertion, default filtering options. No Rust enum sees private event values.
    let port = unsafe {
        create(
            pid,
            0,
            0,
            mask,
            callback,
            (&mut *context as *mut CallbackContext).cast(),
        )
    };
    if port.is_null() {
        bail!("event tap could not be installed");
    }
    let mut resources = NativeResources {
        port,
        source: std::ptr::null_mut(),
        run_loop,
    };
    resources.source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0) };
    if resources.source.is_null() {
        bail!("event tap run loop source is unavailable");
    }
    unsafe {
        CFRunLoopAddSource(run_loop, resources.source, kCFRunLoopDefaultMode);
        CGEventTapEnable(port, true);
    }
    if !unsafe { CGEventTapIsEnabled(port) } {
        bail!("event tap is disabled");
    }
    let startup = ready.take().expect("startup notification owned by worker");
    if startup.send(Ok(control.clone())).is_err() {
        control.request_close();
    }
    while control.active() {
        // A bounded run catches close arriving before CFRunLoopRunInMode starts.
        let reason = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 1) };
        // A dead source or externally disabled tap may stop delivering callbacks.
        if reason == kCFRunLoopRunFinished
            || !unsafe { CFMachPortIsValid(port) != 0 }
            || !unsafe { CFRunLoopSourceIsValid(resources.source) != 0 }
            || !unsafe { CGEventTapIsEnabled(port) }
        {
            control.unavailable();
        }
    }
    control.unavailable();
    drop(resources);
    drop(context);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn control(losses: Arc<AtomicUsize>) -> Arc<Control> {
        Arc::new(Control {
            run_loop: None,
            closing: AtomicBool::new(false),
            healthy: AtomicBool::new(true),
            on_unavailable: Arc::new(move || {
                losses.fetch_add(1, Ordering::SeqCst);
            }),
        })
    }

    #[test]
    fn native_private_and_keyboard_types_use_raw_masks() {
        assert_eq!(
            event_mask(&[10, 11, 12, 13, 19, 20]).unwrap(),
            (1 << 10) | (1 << 11) | (1 << 12) | (1 << 13) | (1 << 19) | (1 << 20)
        );
        assert!(event_mask(&[64]).is_err());
        assert!(event_mask(&[]).is_err());
    }

    #[test]
    fn callback_passes_or_drops_borrowed_event_without_enum_conversion() {
        let losses = Arc::new(AtomicUsize::new(0));
        let context = CallbackContext {
            control: control(losses.clone()),
            handler: Arc::new(|kind, _| {
                if kind == 19 {
                    TapDecision::Drop
                } else {
                    TapDecision::Pass
                }
            }),
        };
        let mut sentinel = 0u8;
        let event = (&mut sentinel as *mut u8).cast();
        assert_eq!(context.invoke(20, event), event);
        assert!(context.invoke(19, event).is_null());
        assert_eq!(context.invoke(12345, event), event);
        assert_eq!(losses.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn disabled_or_panicking_callback_notifies_loss_once_and_fails_open() {
        let losses = Arc::new(AtomicUsize::new(0));
        let context = CallbackContext {
            control: control(losses.clone()),
            handler: Arc::new(|_, _| panic!("handler failed")),
        };
        let mut sentinel = 0u8;
        let event = (&mut sentinel as *mut u8).cast();
        assert_eq!(context.invoke(10, event), event);
        assert!(!context.control.active());
        assert_eq!(
            context.invoke(DISABLED_TIMEOUT, std::ptr::null_mut()),
            std::ptr::null_mut()
        );
        assert_eq!(context.invoke(DISABLED_USER_INPUT, event), event);
        assert_eq!(losses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disabled_notification_with_no_event_invalidates_live_observation() {
        let losses = Arc::new(AtomicUsize::new(0));
        let context = CallbackContext {
            control: control(losses.clone()),
            handler: Arc::new(|_, _| panic!("disabled notification must not reach handler")),
        };
        assert!(context.control.active());
        assert!(context
            .invoke(DISABLED_TIMEOUT, std::ptr::null_mut())
            .is_null());
        assert!(!context.control.active());
        assert_eq!(losses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_prevents_future_callbacks() {
        let losses = Arc::new(AtomicUsize::new(0));
        let shared = control(losses.clone());
        shared.request_close();
        shared.request_close();
        let context = CallbackContext {
            control: shared,
            handler: Arc::new(|_, _| panic!("closed callback must not run")),
        };
        let mut sentinel = 0u8;
        let event = (&mut sentinel as *mut u8).cast();
        assert_eq!(context.invoke(10, event), event);
        assert_eq!(losses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn own_worker_close_does_not_join_itself() {
        let losses = Arc::new(AtomicUsize::new(0));
        let shared = control(losses.clone());
        let (start, receiver) = mpsc::channel::<Arc<EventTap>>();
        let (finished, completion) = mpsc::channel();
        let worker = thread::spawn(move || {
            let tap = receiver.recv().unwrap();
            // This models close from inside a callback, without installing a tap.
            tap.close();
            assert!(!tap.is_active());
            finished.send(()).unwrap();
        });
        let tap = Arc::new(EventTap {
            control: shared,
            worker: Mutex::new(Some(worker)),
        });
        start.send(tap.clone()).unwrap();
        completion
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("own-worker close must not join itself");
        tap.close();
        assert_eq!(losses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn worker_exit_invalidates_observation_without_an_event() {
        let losses = Arc::new(AtomicUsize::new(0));
        let shared = control(losses.clone());
        let availability = WorkerAvailability(shared.clone());
        assert!(shared.active());
        drop(availability);
        assert!(!shared.active());
        assert_eq!(losses.load(Ordering::SeqCst), 1);
    }
}
