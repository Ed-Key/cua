//! Background mouse event synthesis via SLEventPostToPid (SkyLight SPI),
//! with fallback to the public CGEvent::post_to_pid for older OS releases.
//!
//! SLEventPostToPid goes through the IOHIDPostEvent path which:
//! - Triggers CGSTickleActivityMonitor (required for Catalyst / Chromium)
//! - Reaches Mac Catalyst windows that CGEventPostToPid misses
//!
//! Mouse events do NOT attach the SLSEventAuthenticationMessage envelope
//! (Swift reference: `attachAuthMessage: false`) — the auth envelope routes
//! events through a direct Mach delivery path that bypasses
//! cgAnnotatedSessionEventTap, which Chromium's window handler subscribes to.

use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use foreign_types::ForeignType;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MousePostMode {
    /// Post the same event through SkyLight and again through the public API.
    /// An AppKit target receives both copies, so one logical event arrives
    /// twice. Kept only until the remaining callers move to
    /// [`MousePostMode::SkyLightPreferred`].
    Both,
    /// Post each event exactly once: through SkyLight `SLEventPostToPid`, or
    /// through the public `CGEvent::post_to_pid` only when the SPI is absent.
    SkyLightPreferred,
    PublicOnly,
    HidOnly,
}

/// The transport an event is handed to. [`post_route`] decides it from the
/// post mode, so the "how many times is this event delivered" question has one
/// answer that unit tests can check without a window server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostRoute {
    SkyLight,
    PublicPid,
    Hid,
    /// Legacy duplicate delivery: SkyLight, then the public API.
    SkyLightAndPublic,
}

fn post_route(mode: MousePostMode, skylight_available: bool) -> PostRoute {
    match mode {
        MousePostMode::Both => PostRoute::SkyLightAndPublic,
        MousePostMode::SkyLightPreferred if skylight_available => PostRoute::SkyLight,
        MousePostMode::SkyLightPreferred | MousePostMode::PublicOnly => PostRoute::PublicPid,
        MousePostMode::HidOnly => PostRoute::Hid,
    }
}

fn skylight_available() -> bool {
    #[cfg(test)]
    if let Some(available) = post_sink::skylight_override() {
        return available;
    }
    crate::input::skylight::is_available()
}

/// Hand one stamped event to its transport.
fn dispatch_mouse_event(route: PostRoute, pid: i32, event: &CGEvent) {
    #[cfg(test)]
    if post_sink::record(route, event) {
        return;
    }
    let event_ptr = event.as_ptr() as *mut std::ffi::c_void;
    match route {
        PostRoute::SkyLight => {
            crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, false);
        }
        PostRoute::PublicPid => event.post_to_pid(pid as libc::pid_t),
        // Through focus_steal so the theft detector counts the button events
        // cua posted itself.
        PostRoute::Hid => crate::focus_steal::post_hid_mouse_event(event),
        PostRoute::SkyLightAndPublic => {
            crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, false);
            event.post_to_pid(pid as libc::pid_t);
        }
    }
}

#[derive(Clone, Copy)]
pub enum WindowClickDelivery {
    Background,
    Foreground,
}

impl WindowClickDelivery {
    pub fn from_foreground(foreground: bool) -> Self {
        if foreground {
            Self::Foreground
        } else {
            Self::Background
        }
    }
}

/// Left-click at `(x, y)` screen coordinates, posted to `pid`.
///
/// Window-local coordinates for backgrounded targets: if `window_local` is
/// `Some((wx, wy))`, stamps a window-local point via `CGEventSetWindowLocation`
/// SPI so WindowServer's hit-test uses the local point directly.
pub fn click_at_xy(
    pid: i32,
    x: f64,
    y: f64,
    count: usize,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    click_at_xy_inner(
        pid,
        (x, y),
        None,
        None,
        count,
        modifiers,
        MousePostMode::Both,
    )
}

/// Screen-absolute click posted to the GLOBAL HID tap (`CGEventTapLocation::HID`),
/// NOT routed to any pid — the OS delivers it to whichever window owns the
/// screen point, the macOS analogue of Windows' `WindowFromPoint` + `SendInput`
/// desktop-scope click. This backs the `capture_scope="desktop"`, window-less
/// (no pid/window_id) branch of the `click` tool: the agent has located the
/// target by vision in `get_desktop_state` and clicks true screen pixels.
///
/// Unlike the pid-routed `click_at_xy`, this honors the real foreground/Z order
/// (it lands on whatever is visually on top at the point) — exactly the
/// foreground, vision-driven model that complements the background contract.
pub fn click_at_xy_desktop(x: f64, y: f64, count: usize, button: &str) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, count, button, &[], false).map(drop)
}

/// Global HID click with physical modifier transitions. The caller must guard
/// the exact foreground window because this transport has no pid addressing.
pub fn click_at_xy_desktop_with_modifiers(
    x: f64,
    y: f64,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, count, button, modifiers, false).map(drop)
}

/// Global HID modifier click that restores the user's hardware cursor after
/// the event pair has been queued.
pub fn click_at_xy_desktop_with_modifiers_preserving_cursor(
    x: f64,
    y: f64,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, count, button, modifiers, true).map(drop)
}

/// Posts the click at the HID tap, then waits until the system has passed
/// its button events on past every session event tap ([`HidPassage`]). An
/// event still held or dropped by a tap when the wait ends is
/// [`ClickNotConfirmed`]: the caller must not restore another app to the
/// front, or a late release lands on it.
fn click_at_xy_desktop_inner(
    x: f64,
    y: f64,
    count: usize,
    button: &str,
    modifiers: &[&str],
    preserve_cursor: bool,
) -> anyhow::Result<HidPassage> {
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEventTapLocation;
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let prior = if preserve_cursor {
        Some(
            CGEvent::new(source.clone())
                .map_err(|_| anyhow::anyhow!("CGEvent::new failed"))?
                .location(),
        )
    } else {
        None
    };
    let point = CGPoint::new(x, y);
    let (down_ty, up_ty, btn) = match button {
        "right" => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
        "middle" => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
        ),
        _ => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
    };
    // Warp the REAL cursor to the point first (the macOS peer of Linux XTest's
    // pointer warp). A synthetic MouseMoved event does not relocate the hardware
    // cursor, and AppKit hit-tests some clicks against the actual cursor
    // position, so without the warp the down/up can miss the target. Desktop
    // scope is the foreground modality, so moving the visible cursor is expected.
    let _ = CGDisplay::warp_mouse_cursor_position(point);
    // Re-couple cursor + mouse-delta so the synthesized click hit-tests at the
    // warped point, not the pre-warp one.
    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    std::thread::sleep(std::time::Duration::from_millis(40));
    let _dispatching = Dispatching::begin();
    let (baseline, backlog) = hid_baseline();
    let posted_pairs = std::cell::Cell::new(0u32);
    let result = super::keyboard::with_global_modifier_keys(modifiers, |flags| {
        if !modifiers.is_empty() {
            // Prime AppKit's cursor-tracking state after the physical modifiers
            // are down so the modified mouseDown preserves the existing
            // selection. Leave ordinary clicks on their established event path.
            let moved = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::MouseMoved,
                point,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(move) failed"))?;
            moved.set_flags(flags);
            moved.post(CGEventTapLocation::HID);
            std::thread::sleep(std::time::Duration::from_millis(12));
        }

        for pair_index in 0..count.max(1) {
            // Both halves exist before the press is posted, so no failure
            // can leave the button down.
            let down = CGEvent::new_mouse_event(source.clone(), down_ty, point, btn)
                .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(down) failed"))?;
            let up = CGEvent::new_mouse_event(source.clone(), up_ty, point, btn)
                .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(up) failed"))?;
            down.set_flags(flags);
            down.set_integer_value_field(
                core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
                (pair_index + 1) as i64,
            );
            crate::focus_steal::post_hid_mouse_event(&down);
            std::thread::sleep(std::time::Duration::from_millis(28));
            up.set_flags(flags);
            up.set_integer_value_field(
                core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
                (pair_index + 1) as i64,
            );
            crate::focus_steal::post_hid_mouse_event(&up);
            posted_pairs.set(posted_pairs.get() + 1);
            if count > 1 {
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }
        Ok(())
    });

    // The events must be past every session tap before anything else moves:
    // a tap can hold them (released later onto whatever is then at the
    // point) or drop them.
    // Checked whenever a pair went out, even if a later one failed to build.
    let own = posted_pairs.get();
    let passage = (own > 0).then(|| await_hid_passage(baseline, backlog, [down_ty, up_ty], own));
    // Let AppKit consume the up event before the pointer is restored. The
    // exact foreground guard remains active around this entire helper.
    std::thread::sleep(std::time::Duration::from_millis(40));
    if let Some(prior) = prior {
        let _ = CGDisplay::warp_mouse_cursor_position(prior);
        unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    }
    match (passage, result) {
        (Some(HidPassage::Pending), _) => {
            note_unresolved(baseline);
            Err(ClickNotConfirmed {
                tap_owner: foreign_active_tap(event_mask(&[down_ty, up_ty])),
                waited: HID_PASSAGE_TIMEOUT,
            }
            .into())
        }
        (_, Err(error)) => Err(error),
        (Some(passage), Ok(())) => Ok(passage),
        (None, Ok(())) => Ok(HidPassage::Unmeasured),
    }
}

/// Where a HID-posted click's button events are, from the window server's
/// per-type event counters: the HID system state counts an event when it is
/// posted, the combined session state only once it has passed every session
/// event tap (an active tap that holds an event delays that count; one that
/// drops it means it never comes; pid-posted events move neither, measured in
/// VM B 2026-10-23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidPassage {
    /// Every button event that entered the HID stream since the click began,
    /// the click's own included, has passed the session taps.
    Passed,
    /// Some of them have not (yet).
    Pending,
    /// The HID counters never showed the click's own events, so the counters
    /// say nothing here; passage is unknown, as before this check existed.
    Unmeasured,
}

/// The click's button events did not pass the session event taps in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickNotConfirmed {
    /// The pid of another process holding an active tap on these events, if
    /// one is listed now.
    pub tap_owner: Option<i32>,
    pub waited: std::time::Duration,
}

impl std::fmt::Display for ClickNotConfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let owner = match self.tap_owner {
            Some(pid) => format!(
                "pid {pid} ({})",
                crate::apps::get_app_name_for_pid(pid).unwrap_or_else(|| "unknown app".into())
            ),
            None => "another process".into(),
        };
        write!(
            f,
            "the click was posted at the HID event tap, but {} ms later the system had not passed \
             it on: an active event tap of {owner} dropped it or still holds it",
            self.waited.as_millis()
        )
    }
}

impl std::error::Error for ClickNotConfirmed {}

/// How long a HID click may take to pass the session taps (an active tap
/// holding each event 300 ms passes a click in about 620 ms).
pub const HID_PASSAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Button event types the counters track: left, right and other down and up.
const BUTTON_TYPES: [CGEventType; 6] = [
    CGEventType::LeftMouseDown,
    CGEventType::LeftMouseUp,
    CGEventType::RightMouseDown,
    CGEventType::RightMouseUp,
    CGEventType::OtherMouseDown,
    CGEventType::OtherMouseUp,
];

/// HID and combined-session counts of every button event type
/// ([`BUTTON_TYPES`]). A session tap handles events in order, so a click is
/// through only when every button event before it is too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ButtonCounters {
    hid: [u32; 6],
    session: [u32; 6],
}

impl ButtonCounters {
    fn read() -> Self {
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGEventSourceCounterForEventType(state_id: i32, event_type: u32) -> u32;
        }
        const COMBINED_SESSION_STATE: i32 = 0;
        const HID_SYSTEM_STATE: i32 = 1;
        let count = |state, ty: CGEventType| unsafe { CGEventSourceCounterForEventType(state, ty as u32) };
        Self {
            hid: BUTTON_TYPES.map(|ty| count(HID_SYSTEM_STATE, ty)),
            session: BUTTON_TYPES.map(|ty| count(COMBINED_SESSION_STATE, ty)),
        }
    }

    /// Per type: events that entered the HID stream since `base` and have
    /// not passed the session taps.
    fn outstanding_since(self, base: Self) -> [u32; 6] {
        std::array::from_fn(|i| {
            self.hid[i]
                .wrapping_sub(base.hid[i])
                .saturating_sub(self.session[i].wrapping_sub(base.session[i]))
        })
    }
}

/// An earlier HID click that never showed as passed: the counters when it
/// began, and when it was last found unresolved.
#[derive(Clone, Copy, Debug)]
struct Unresolved {
    base: ButtonCounters,
    noted: std::time::Instant,
}

/// How long an unresolved click counts as possibly in flight. A tap that
/// holds an event this long has dropped it for every practical purpose.
const UNRESOLVED_FOR: std::time::Duration = std::time::Duration::from_secs(5);

fn unresolved() -> &'static std::sync::Mutex<Option<Unresolved>> {
    static UNRESOLVED: std::sync::Mutex<Option<Unresolved>> = std::sync::Mutex::new(None);
    &UNRESOLVED
}

/// What the earlier unresolved clicks still owe at `now` (zero when none,
/// expired, or all through; the record is then cleared).
fn backlog_at(now: ButtonCounters) -> [u32; 6] {
    let mut record = unresolved().lock().unwrap_or_else(|p| p.into_inner());
    let Some(earlier) = *record else { return [0; 6] };
    let owed = now.outstanding_since(earlier.base);
    if earlier.noted.elapsed() >= UNRESOLVED_FOR || owed == [0; 6] {
        *record = None;
        return [0; 6];
    }
    owed
}

/// A click's baseline counters and the backlog of earlier unresolved clicks.
fn hid_baseline() -> (ButtonCounters, [u32; 6]) {
    let now = ButtonCounters::read();
    (now, backlog_at(now))
}

fn note_unresolved(base: ButtonCounters) {
    let mut record = unresolved().lock().unwrap_or_else(|p| p.into_inner());
    // Keep the earliest base, which spans every click still owed, and its
    // deadline: a dropped event is owed forever, so later clicks must not
    // renew the debt.
    *record = Some(record.unwrap_or(Unresolved { base, noted: std::time::Instant::now() }));
}

/// HID clicks being posted or awaiting passage right now.
static DISPATCHING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Marks a HID click from before its first post until its passage is
/// settled (passed, or noted unresolved).
struct Dispatching;

impl Dispatching {
    fn begin() -> Self {
        DISPATCHING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for Dispatching {
    fn drop(&mut self) {
        DISPATCHING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Whether a HID click cua posted may still be held by an event tap: one is
/// being posted or awaited now, or an earlier one is unresolved. While it
/// is, nothing that brings another app or window forward may run, or a late
/// release lands there.
pub fn click_in_flight() -> bool {
    DISPATCHING.load(std::sync::atomic::Ordering::SeqCst) > 0 || backlog_at(ButtonCounters::read()) != [0; 6]
}

/// Before activating or raising anything for a new action: wait while a HID
/// click may still be held (one being posted or awaited by another worker,
/// or an earlier one unresolved), until it passes or counts as dropped. Never
/// called while this thread posts one; bounded in case a worker hangs.
pub fn await_no_click_in_flight() {
    let give_up = std::time::Instant::now() + UNRESOLVED_FOR + HID_PASSAGE_TIMEOUT * 2;
    while click_in_flight() && std::time::Instant::now() < give_up {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Pure passage decision for `own` pairs of the button `kinds` (indices into
/// [`BUTTON_TYPES`]) posted since `baseline`, behind `backlog` earlier events.
fn hid_passage(baseline: ButtonCounters, now: ButtonCounters, backlog: [u32; 6], kinds: [usize; 2], own: u32) -> HidPassage {
    let entered = |i: usize| now.hid[i].wrapping_sub(baseline.hid[i]);
    let passed = |i: usize| now.session[i].wrapping_sub(baseline.session[i]);
    if kinds.iter().any(|&i| entered(i) < own) {
        HidPassage::Unmeasured
    } else if (0..6).all(|i| passed(i) >= entered(i).saturating_add(backlog[i])) {
        HidPassage::Passed
    } else {
        // ponytail: counts, not identities. A user's button event that a tap
        // drops during the wait reads as pending (errs toward "not
        // confirmed"); an earlier unresolved cua click is carried as a
        // backlog for UNRESOLVED_FOR only; a user's own click already held
        // in a tap when this one starts can pass first and read as this one.
        // Tag events (field 42) and watch them at the annotated session point
        // if that ever matters; it needs an event tap of cua's own.
        HidPassage::Pending
    }
}

fn await_hid_passage(baseline: ButtonCounters, backlog: [u32; 6], types: [CGEventType; 2], own: u32) -> HidPassage {
    let kinds = types.map(|ty| BUTTON_TYPES.iter().position(|t| *t as u32 == ty as u32).unwrap_or(0));
    let deadline = std::time::Instant::now() + HID_PASSAGE_TIMEOUT;
    loop {
        let passage = hid_passage(baseline, ButtonCounters::read(), backlog, kinds, own);
        if passage == HidPassage::Passed || std::time::Instant::now() >= deadline {
            return passage;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// `CGEventTapInformation` (CGEventTypes.h).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct EventTapInformation {
    event_tap_id: u32,
    tap_point: u32,
    options: u32,
    events_of_interest: u64,
    tapping_process: i32,
    process_being_tapped: i32,
    enabled: bool,
    min_usec_latency: f32,
    avg_usec_latency: f32,
    max_usec_latency: f32,
}

extern "C" {
    fn CGGetEventTapList(max: u32, list: *mut EventTapInformation, count: *mut u32) -> i32;
}

fn event_mask(types: &[CGEventType]) -> u64 {
    types.iter().fold(0, |mask, ty| mask | (1u64 << (*ty as u32)))
}

/// The tap among `taps` that can drop or hold a HID-posted event of `mask`:
/// enabled, active (options 0, not listen-only), on every process's events
/// (not a per-process tap), and owned by another process.
fn foreign_active_tap_in(taps: &[EventTapInformation], own_pid: i32, mask: u64) -> Option<i32> {
    taps.iter()
        .find(|tap| {
            tap.enabled
                && tap.options == 0
                && tap.process_being_tapped == 0
                && tap.tapping_process != own_pid
                && tap.events_of_interest & mask != 0
        })
        .map(|tap| tap.tapping_process)
}

/// The pid of another process holding an enabled, active global event tap
/// on any of `mask`'s events (Handy, Karabiner, BetterTouchTool...). Such a
/// tap sees, and can drop or hold, a HID-posted event; pid-posted events
/// bypass it. The list is a snapshot: a tap made after it is missed (the
/// HID passage check still catches what it does).
pub fn foreign_active_tap(mask: u64) -> Option<i32> {
    let mut list = [EventTapInformation::default(); 128];
    let mut count = 0u32;
    // SAFETY: the list holds 128 records and CGGetEventTapList writes at most `max`.
    if unsafe { CGGetEventTapList(list.len() as u32, list.as_mut_ptr(), &mut count) } != 0 {
        return None;
    }
    foreign_active_tap_in(&list[..(count as usize).min(list.len())], std::process::id() as i32, mask)
}

/// How a foreground click was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundClick {
    /// At the HID tap, and the system passed it on past every session tap.
    Hid(HidPassage),
    /// To the target pid with its window fields, because `tap_owner` holds an
    /// active tap on left clicks that could drop or hold a HID click.
    PidAroundTap { tap_owner: i32 },
}

/// The foreground click primitive, run inside the exact-window activation
/// guard (`with_foreground_pointer_activation`), after the occlusion check.
/// Decided before dispatch: a plain left click goes to the pid when another
/// process holds an active tap on left clicks (native Computer Use always
/// posts clicks to the pid); everything else is a HID click whose passage is
/// confirmed ([`click_at_xy_desktop_inner`]).
#[allow(clippy::too_many_arguments)]
pub fn foreground_click(
    pid: i32,
    wid: u32,
    (x, y): (f64, f64),
    (wx, wy): (f64, f64),
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> anyhow::Result<ForegroundClick> {
    if button == "left" && modifiers.is_empty() {
        let left = event_mask(&[CGEventType::LeftMouseDown, CGEventType::LeftMouseUp]);
        if let Some(tap_owner) = foreign_active_tap(left) {
            // The pointer goes to the target as on the HID route, so apps
            // that read it see it there.
            move_cursor_desktop(x, y)?;
            click_at_xy_with_window_local(
                pid, x, y, wx, wy, wid, count.max(1), &[], WindowClickDelivery::Foreground,
            )?;
            return Ok(ForegroundClick::PidAroundTap { tap_owner });
        }
    }
    click_at_xy_desktop_inner(x, y, count, button, modifiers, false).map(ForegroundClick::Hid)
}

/// Move the real hardware cursor to a logical desktop point.
pub fn move_cursor_desktop(x: f64, y: f64) -> anyhow::Result<()> {
    use core_graphics::display::CGDisplay;
    let point = CGPoint::new(x, y);
    CGDisplay::warp_mouse_cursor_position(point)
        .map_err(|error| anyhow::anyhow!("CGWarpMouseCursorPosition failed: {error:?}"))?;
    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    Ok(())
}

/// Scroll the foreground desktop surface at a logical screen point through the
/// global HID queue. Mirrors computer-server's pynput wheel behavior while
/// preserving cua-driver's explicit direction/amount contract.
pub fn scroll_wheel_desktop(
    x: f64,
    y: f64,
    delta_y_per_tick: i32,
    delta_x_per_tick: i32,
    ticks: usize,
) -> anyhow::Result<()> {
    use core_graphics::event::{CGEventTapLocation, ScrollEventUnit};

    move_cursor_desktop(x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(40));
    for _ in 0..ticks.max(1) {
        // AppKit does not reliably consume synthetic LINE-unit events posted
        // through the global HID queue. pynput's proven macOS desktop path uses
        // PIXEL units, a null source, and scales each logical wheel notch to ten
        // pixels. Keep that exact controller convention instead of attaching
        // synthetic source state unrelated to the physical pointer we warped.
        let wheel_y = (delta_y_per_tick / 12).clamp(-100, 100);
        let wheel_x = (delta_x_per_tick / 12).clamp(-100, 100);
        let event_ref = unsafe {
            CGEventCreateScrollWheelEvent2(
                std::ptr::null_mut(),
                ScrollEventUnit::PIXEL,
                2,
                wheel_y,
                wheel_x,
                0,
            )
        };
        if event_ref.is_null() {
            return Err(anyhow::anyhow!("CGEventCreateScrollWheelEvent2 failed"));
        }
        let event = unsafe { CGEvent::from_ptr(event_ref) };
        event.post(CGEventTapLocation::HID);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(())
}

/// Click one exact desktop point, then restore the user's cursor position.
///
/// This is intentionally narrower than the public desktop click path. It is
/// used only by approved, bounded setup flows after an accessibility element
/// has proven the exact target point and window.
pub fn click_at_xy_desktop_preserving_cursor(x: f64, y: f64) -> anyhow::Result<()> {
    click_at_xy_desktop_inner(x, y, 1, "left", &[], true).map(drop)
}

extern "C" {
    /// Quartz's non-variadic scroll-event constructor. The public desktop
    /// path intentionally passes a null source to match real mouse-controller
    /// libraries such as pynput.
    fn CGEventCreateScrollWheelEvent2(
        source: core_graphics::sys::CGEventSourceRef,
        units: core_graphics::event::CGScrollEventUnit,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> core_graphics::sys::CGEventRef;

    /// Reconnect the mouse-delta stream to the (just-warped) cursor position so a
    /// synthesized click hit-tests at the new location, not the pre-warp one.
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
}

/// Like `click_at_xy` but also targets a specific window. Foreground delivery
/// uses the supplied window-local point with one public pid post. Background
/// delivery keeps the translated screen point through the SkyLight route, with
/// a public fallback only when the private symbol is unavailable.
// The flattened arguments mirror the native event fields used by existing callers.
#[allow(clippy::too_many_arguments)]
pub fn click_at_xy_with_window_local(
    pid: i32,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    wid: u32,
    count: usize,
    modifiers: &[&str],
    delivery: WindowClickDelivery,
) -> anyhow::Result<()> {
    match delivery {
        WindowClickDelivery::Background => {
            click_at_xy_chromium(pid, x, y, wx, wy, wid, count, modifiers)
        }
        WindowClickDelivery::Foreground => click_at_xy_inner(
            pid,
            (x, y),
            Some((wx, wy)),
            Some(wid),
            count,
            modifiers,
            MousePostMode::PublicOnly,
        ),
    }
}

/// `screen` is the click point in screen coordinates.
fn click_at_xy_inner(
    pid: i32,
    screen: (f64, f64),
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    count: usize,
    modifiers: &[&str],
    post_mode: MousePostMode,
) -> anyhow::Result<()> {
    let (x, y) = screen;
    use std::time::{SystemTime, UNIX_EPOCH};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let point = CGPoint::new(x, y);
    let flags = parse_modifier_flags(modifiers);

    // Shared click-group ID (f58) when window_id is known: keeps all pairs
    // in one gesture so WindowServer / Chromium coalesce them correctly.
    let click_group_id: Option<i64> = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });

    super::keyboard::with_pid_modifier_keys(pid, modifiers, || {
        // Prime the target window's cursor-tracking state with a leading
        // mouseMoved so an AppKit NSButton / NSView hit-tests the down at the
        // right point. Without it the synthetic mouseDown on a backgrounded
        // AppKit control is silently ignored.
        post_mouse_moved_primer(
            pid,
            &source,
            point,
            window_local,
            wid,
            click_group_id,
            post_mode,
        );
        std::thread::sleep(std::time::Duration::from_millis(12));

        for pair_index in 0..count {
            let click_state = (pair_index + 1) as i64;

            let down = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::LeftMouseDown,
                point,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(down) failed"))?;
            // Always stamp the requested flags: the HID-state source would
            // otherwise carry whatever modifiers the user physically holds.
            down.set_flags(flags);

            post_mouse_event_with_mode(
                pid,
                &down,
                window_local,
                wid,
                click_group_id,
                click_state,
                0,
                3,
                post_mode,
            );
            // 28 ms down→up gap: an NSButton's mouseDown enters a modal
            // tracking loop that polls for the matching mouseUp; too tight a
            // gap can race the loop's first poll and the click is dropped.
            std::thread::sleep(std::time::Duration::from_millis(28));

            let up = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::LeftMouseUp,
                point,
                CGMouseButton::Left,
            )
            .map_err(|_| anyhow::anyhow!("CGEvent::new_mouse_event(up) failed"))?;
            // Always stamp the requested flags: the HID-state source would
            // otherwise carry whatever modifiers the user physically holds.
            up.set_flags(flags);

            post_mouse_event_with_mode(
                pid,
                &up,
                window_local,
                wid,
                click_group_id,
                click_state,
                0,
                3,
                post_mode,
            );

            if count > 1 {
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }
        Ok(())
    })
}

/// Prepare a raw background pixel click by making the target AppKit-active
/// without raising or restacking its window.
///
/// The Swift implementation ran this immediately before the stamped event
/// stream. The original Rust port retained the SkyLight primitive but omitted
/// this call while cursor-overlay repinning was incomplete. Callers should
/// re-pin their overlay after this returns, then post the click sequence.
///
/// Returns whether the private focus-without-raise recipe succeeded. Event
/// posting remains best-effort when the private APIs are unavailable.
pub fn prepare_background_pixel_click(pid: i32, wid: u32) -> bool {
    let activated = crate::input::skylight::activate_without_raise(pid as libc::pid_t, wid);
    // Match Swift's settle interval so AppKit updates its active/key-window
    // routing before the mouseMoved + primer + target stream arrives.
    std::thread::sleep(std::time::Duration::from_millis(50));
    activated
}

/// Event kinds in the pure route plan for the Chromium-compatible background
/// left-click recipe.
///
/// The sequence stays PID/window-routed throughout. The caller must first run
/// [`prepare_background_pixel_click`] for background delivery, then re-pin any
/// cursor overlay before entering this event stream.
///  1. Stamped `mouseMoved` at target coords (f0=2, cursor-state primer).
///  2. Off-screen primer down/up at (-1, -1) (f0=1/2) — satisfies Chromium's
///     user-activation gate without hitting any DOM element.
///  3. Target down/up pair(s) at real coordinates (f0=3/3), clickState 1→N.
///
/// All events carry:
///  - f0  = gesture phase marker (move=2, primerDown=1, primerUp=2, target=3)
///  - f1  = mouseEventClickState (1 for single, 1→2 for double)
///  - f3  = 0  (left button)
///  - f7  = 3  (NSEventSubtypeTouch)
///  - f40 = target pid   (Chromium synthetic-event filter)
///  - f51 / f91 / f92 = CGWindowID (window routing)
///  - f58 = constant click-group ID across all events (gesture coalescing)
///  - `CGEventSetWindowLocation` per-event (screen point for both routes)
///
/// Uses the SkyLight route required by Chromium-compatible targets, with the
/// public API only as a fallback when the private symbol is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChromiumClickEventKind {
    Move,
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChromiumClickRouteStep {
    kind: ChromiumClickEventKind,
    event_location: (f64, f64),
    skylight_window_location: (f64, f64),
    public_window_location: (f64, f64),
    click_state: i64,
    phase: i64,
    button_number: i64,
    subtype: i64,
    delay_after_ms: u64,
}

fn chromium_click_route_plan(
    screen_x: f64,
    screen_y: f64,
    win_local_x: f64,
    win_local_y: f64,
    count: usize,
) -> Vec<ChromiumClickRouteStep> {
    let screen_target = (screen_x, screen_y);
    let _ = (win_local_x, win_local_y);
    let off_screen = (-1.0, -1.0);
    let mut steps = vec![
        ChromiumClickRouteStep {
            kind: ChromiumClickEventKind::Move,
            event_location: screen_target,
            skylight_window_location: screen_target,
            public_window_location: screen_target,
            click_state: 0,
            phase: 2,
            button_number: 0,
            subtype: 3,
            delay_after_ms: 15,
        },
        ChromiumClickRouteStep {
            kind: ChromiumClickEventKind::Down,
            event_location: off_screen,
            skylight_window_location: off_screen,
            public_window_location: off_screen,
            click_state: 1,
            phase: 1,
            button_number: 0,
            subtype: 3,
            delay_after_ms: 1,
        },
        ChromiumClickRouteStep {
            kind: ChromiumClickEventKind::Up,
            event_location: off_screen,
            skylight_window_location: off_screen,
            public_window_location: off_screen,
            click_state: 1,
            phase: 2,
            button_number: 0,
            subtype: 3,
            delay_after_ms: 100,
        },
    ];

    let click_pairs = count.clamp(1, 2);
    for pair_index in 1..=click_pairs {
        let click_state = pair_index as i64;
        steps.push(ChromiumClickRouteStep {
            kind: ChromiumClickEventKind::Down,
            event_location: screen_target,
            skylight_window_location: screen_target,
            public_window_location: screen_target,
            click_state,
            phase: 3,
            button_number: 0,
            subtype: 3,
            delay_after_ms: 1,
        });
        steps.push(ChromiumClickRouteStep {
            kind: ChromiumClickEventKind::Up,
            event_location: screen_target,
            skylight_window_location: screen_target,
            public_window_location: screen_target,
            click_state,
            phase: 3,
            button_number: 0,
            subtype: 3,
            delay_after_ms: if pair_index < click_pairs { 80 } else { 0 },
        });
    }
    steps
}

/// Post the route plan for the Chromium-compatible background left-click
/// recipe. `CGEvent::new_mouse_event` starts with the screen-space location.
/// `CGEventSetWindowLocation` uses the screen point for both SkyLight and the
/// public PID fallback. macOS interprets the stamped location in screen space
/// even when the public route is used for a background process.
#[allow(clippy::too_many_arguments)]
pub fn click_at_xy_chromium(
    pid: i32,
    screen_x: f64,
    screen_y: f64,
    win_local_x: f64,
    win_local_y: f64,
    wid: u32,
    count: usize,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let route_plan = chromium_click_route_plan(screen_x, screen_y, win_local_x, win_local_y, count);
    let flags = parse_modifier_flags(modifiers);
    let window_id = wid as i64;

    // All 5 events share the same click-group ID so WindowServer / Chromium
    // treat the sequence as one gesture (Swift: field 58).
    let click_group_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as i64;

    // Stamp required fields onto a CGEvent.  All captured values are Copy so
    // this closure is Fn (callable multiple times).
    let stamp = |event: &CGEvent, step: &ChromiumClickRouteStep| {
        let ptr = event.as_ptr() as *mut std::ffi::c_void;
        let set = |f: u32, v: i64| {
            crate::input::skylight::set_integer_field(ptr, f, v);
        };
        set(0, step.phase); // kCGMouseEventNumber (gesture phase)
        set(1, step.click_state); // kCGMouseEventClickState
        set(3, step.button_number); // kCGMouseEventButtonNumber (left)
        set(7, step.subtype); // kCGMouseEventSubtype (NSEventSubtypeTouch)
        set(40, pid as i64); // Chromium synthetic-event filter
        if window_id != 0 {
            set(51, window_id); // windowNumber (NSEvent bridge equivalent)
            set(91, window_id); // kCGMouseEventWindowUnderMousePointer
            set(92, window_id); // kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent
        }
        set(58, click_group_id); // click-group ID (gesture coalescing)
        crate::input::skylight::set_window_location(
            ptr,
            step.skylight_window_location.0,
            step.skylight_window_location.1,
        );
        if flags != CGEventFlags::CGEventFlagNull {
            event.set_flags(flags);
        }
    };

    let post = |event: &CGEvent, _step: &ChromiumClickRouteStep| {
        dispatch_mouse_event(
            post_route(MousePostMode::SkyLightPreferred, skylight_available()),
            pid,
            event,
        );
    };

    for step in route_plan {
        let (event_type, error_label) = match step.kind {
            ChromiumClickEventKind::Move => (CGEventType::MouseMoved, "mouseMoved"),
            ChromiumClickEventKind::Down if step.phase == 1 => {
                (CGEventType::LeftMouseDown, "primer down")
            }
            ChromiumClickEventKind::Up if step.phase == 2 => {
                (CGEventType::LeftMouseUp, "primer up")
            }
            ChromiumClickEventKind::Down => (CGEventType::LeftMouseDown, "target down"),
            ChromiumClickEventKind::Up => (CGEventType::LeftMouseUp, "target up"),
        };
        let event = CGEvent::new_mouse_event(
            source.clone(),
            event_type,
            CGPoint::new(step.event_location.0, step.event_location.1),
            CGMouseButton::Left,
        )
        .map_err(|_| anyhow::anyhow!("{error_label} event creation failed"))?;
        stamp(&event, &step);
        post(&event, &step);
        if step.delay_after_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(step.delay_after_ms));
        }
    }

    Ok(())
}

/// Press-drag-release gesture from `(from_x, from_y)` to `(to_x, to_y)` in
/// screen coordinates, addressed to `pid` and the optional exact window.
///
/// `duration_ms` is the wall-clock budget for the drag path; `steps` is the
/// number of intermediate `leftMouseDragged` events linearly interpolated
/// along the path. `modifiers` are held across the entire gesture.
///
/// Like the Swift reference `MouseInput.drag`, uses the SkyLight path for
/// backgrounded-target delivery.
// The drag primitive deliberately exposes its complete native event contract.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy(
    pid: i32,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    from_local: Option<(f64, f64)>,
    to_local: Option<(f64, f64)>,
    wid: Option<u32>,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    foreground_hid: bool,
) -> anyhow::Result<()> {
    drag_at_xy_observed(
        pid,
        from_x,
        from_y,
        to_x,
        to_y,
        from_local,
        to_local,
        wid,
        duration_ms,
        steps,
        modifiers,
        button,
        foreground_hid,
        |_, _| {},
    )
}

/// Window drag with an observer called for every native pointer position.
///
/// Background delivery keeps the established PID-routed sequence. When
/// `foreground_hid` is true, the same stamped gesture is posted once per event
/// through the global HID queue while the hardware cursor follows the path.
///
/// The cursor overlay uses this for the same reason as
/// [`drag_at_xy_foreground_observed`]: the synthetic cursor should follow the
/// actual dispatched path rather than jumping to the endpoint after release.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy_observed<F>(
    pid: i32,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    from_local: Option<(f64, f64)>,
    to_local: Option<(f64, f64)>,
    wid: Option<u32>,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    foreground_hid: bool,
    mut observe: F,
) -> anyhow::Result<()>
where
    F: FnMut(f64, f64),
{
    use std::time::{SystemTime, UNIX_EPOCH};

    if foreground_hid {
        return drag_at_xy_window_foreground_observed(
            pid,
            from_x,
            from_y,
            to_x,
            to_y,
            from_local,
            to_local,
            wid,
            duration_ms,
            steps,
            modifiers,
            button,
            observe,
        );
    }

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let flags = parse_modifier_flags(modifiers);

    let (cg_button, down_type, dragged_type, up_type) = match button {
        DragButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
        ),
        DragButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
        ),
        DragButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
        ),
    };
    // f3 button number must match the dragged button (0=left, 1=right, 2=middle).
    let button_number: i64 = match button {
        DragButton::Left => 0,
        DragButton::Right => 1,
        DragButton::Middle => 2,
    };

    let click_group_id: Option<i64> = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });

    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };

    // Prime the routed target's cursor-tracking state immediately before the
    // press, matching the click path's stamped mouseMoved + settle interval.
    let from_pt = CGPoint::new(from_x, from_y);
    post_mouse_moved_primer(
        pid,
        &source,
        from_pt,
        from_local,
        wid,
        click_group_id,
        MousePostMode::Both,
    );
    std::thread::sleep(std::time::Duration::from_millis(12));

    // MouseDown at start.
    let down = CGEvent::new_mouse_event(source.clone(), down_type, from_pt, cg_button)
        .map_err(|_| anyhow::anyhow!("drag mouseDown failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        down.set_flags(flags);
    }
    post_mouse_event(
        pid,
        &down,
        from_local,
        wid,
        click_group_id,
        1,
        button_number,
        0,
    );
    observe(from_x, from_y);
    std::thread::sleep(std::time::Duration::from_millis(16));

    // Interpolated drag steps.
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ix = from_x + (to_x - from_x) * t;
        let iy = from_y + (to_y - from_y) * t;
        let il = from_local
            .zip(to_local)
            .map(|((fx, fy), (tx, ty))| (fx + (tx - fx) * t, fy + (ty - fy) * t));
        let drag_pt = CGPoint::new(ix, iy);
        let drag = CGEvent::new_mouse_event(source.clone(), dragged_type, drag_pt, cg_button)
            .map_err(|_| anyhow::anyhow!("drag mouseDragged failed"))?;
        if flags != CGEventFlags::CGEventFlagNull {
            drag.set_flags(flags);
        }
        post_mouse_event(pid, &drag, il, wid, click_group_id, 1, button_number, 0);
        observe(ix, iy);
        if step_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(step_delay_ms));
        }
    }

    // MouseUp at end.
    // Give Chromium one run-loop turn to process the final dragged event at
    // the drop point before releasing pointer capture.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let to_pt = CGPoint::new(to_x, to_y);
    let up = CGEvent::new_mouse_event(source.clone(), up_type, to_pt, cg_button)
        .map_err(|_| anyhow::anyhow!("drag mouseUp failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        up.set_flags(flags);
    }
    post_mouse_event(pid, &up, to_local, wid, click_group_id, 1, button_number, 0);
    // Chromium may process the final pointerup on the next run-loop turn. In
    // the foreground rung the caller restores the previous app immediately
    // after this function returns, so let the target consume the release and
    // complete pointer capture before that restore.
    std::thread::sleep(std::time::Duration::from_millis(100));

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn post_drag_mouse_event(
    pid: i32,
    event_type: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    click_state: i64,
    button_number: i64,
    subtype: i64,
    mode: MousePostMode,
    flags: CGEventFlags,
    error_context: &str,
) -> anyhow::Result<()> {
    debug_assert_eq!(mode, MousePostMode::HidOnly);
    let event = new_global_mouse_event(event_type, point, button)
        .map_err(|_| anyhow::anyhow!(error_context.to_owned()))?;
    if flags != CGEventFlags::CGEventFlagNull {
        event.set_flags(flags);
    }
    post_mouse_event_with_mode(
        pid,
        &event,
        window_local,
        wid,
        click_group_id,
        click_state,
        button_number,
        subtype,
        mode,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drag_at_xy_window_foreground_observed<F>(
    pid: i32,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    from_local: Option<(f64, f64)>,
    to_local: Option<(f64, f64)>,
    wid: Option<u32>,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    mut observe: F,
) -> anyhow::Result<()>
where
    F: FnMut(f64, f64),
{
    use core_graphics::display::CGDisplay;
    use std::time::{SystemTime, UNIX_EPOCH};

    let (cg_button, down_type, dragged_type, up_type, button_number) = match button {
        DragButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
            0,
        ),
        DragButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
            1,
        ),
        DragButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
            2,
        ),
    };
    let click_group_id = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };
    let events = foreground_drag_events(from_x, from_y, to_x, to_y, steps);

    super::keyboard::with_global_modifier_keys(modifiers, |flags| {
        // Where the button is held, between the press and the release: a
        // failure there releases it before returning, never replaying input.
        let mut held: Option<CGPoint> = None;
        for spec in events {
            let (event_type, click_state, event_button, event_button_number, subtype) = match spec
                .kind
            {
                ForegroundDragEventKind::Move => {
                    (CGEventType::MouseMoved, 0, CGMouseButton::Left, 0, 3)
                }
                ForegroundDragEventKind::Down => (down_type, 1, cg_button, button_number, 0),
                ForegroundDragEventKind::Dragged => (dragged_type, 1, cg_button, button_number, 0),
                ForegroundDragEventKind::Up => (up_type, 1, cg_button, button_number, 0),
            };
            let local = from_local.zip(to_local).map(|((fx, fy), (tx, ty))| {
                (
                    fx + (tx - fx) * spec.progress,
                    fy + (ty - fy) * spec.progress,
                )
            });

            match spec.kind {
                ForegroundDragEventKind::Move => {
                    let _ = CGDisplay::warp_mouse_cursor_position(spec.point);
                    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
                ForegroundDragEventKind::Dragged => {
                    let _ = CGDisplay::warp_mouse_cursor_position(spec.point);
                }
                ForegroundDragEventKind::Up => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                ForegroundDragEventKind::Down => {}
            }

            let posted = post_drag_mouse_event(
                pid,
                event_type,
                spec.point,
                event_button,
                local,
                wid,
                click_group_id,
                click_state,
                event_button_number,
                subtype,
                MousePostMode::HidOnly,
                flags,
                "foreground drag event creation failed",
            );
            if let Err(error) = posted {
                if let Some(at) = held {
                    let _ = post_drag_mouse_event(
                        pid,
                        up_type,
                        at,
                        cg_button,
                        None,
                        wid,
                        click_group_id,
                        1,
                        button_number,
                        0,
                        MousePostMode::HidOnly,
                        flags,
                        "foreground drag release failed",
                    );
                }
                return Err(error);
            }
            held = match spec.kind {
                ForegroundDragEventKind::Down | ForegroundDragEventKind::Dragged => Some(spec.point),
                ForegroundDragEventKind::Up => None,
                ForegroundDragEventKind::Move => held,
            };

            match spec.kind {
                ForegroundDragEventKind::Move => {
                    std::thread::sleep(std::time::Duration::from_millis(12));
                }
                ForegroundDragEventKind::Down => {
                    observe(spec.point.x, spec.point.y);
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
                ForegroundDragEventKind::Dragged => {
                    observe(spec.point.x, spec.point.y);
                    if step_delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(step_delay_ms));
                    }
                }
                ForegroundDragEventKind::Up => {}
            }
        }
        Ok(())
    })?;

    std::thread::sleep(std::time::Duration::from_millis(100));
    Ok(())
}

/// Foreground drag through the global HID event tap.
///
/// A frontmost Chromium/WebKit surface expects a real HID-origin gesture for
/// pointer capture and drag tracking. PID-routed `post_to_pid` events are
/// suitable for background delivery, but they can be silently filtered by the
/// renderer even when the target is frontmost.
// The drag primitive deliberately exposes its complete native event contract.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy_foreground(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
) -> anyhow::Result<()> {
    drag_at_xy_foreground_observed(
        from_x,
        from_y,
        to_x,
        to_y,
        duration_ms,
        steps,
        modifiers,
        button,
        |_, _| {},
    )
}

/// Foreground drag with an observer called for every native pointer position.
///
/// The cursor overlay uses this to follow the same interpolated path and
/// cadence as the HID gesture instead of gliding only after the real pointer
/// has already completed the drag.
#[allow(clippy::too_many_arguments)]
pub fn drag_at_xy_foreground_observed<F>(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    duration_ms: u64,
    steps: usize,
    modifiers: &[&str],
    button: DragButton,
    mut observe: F,
) -> anyhow::Result<()>
where
    F: FnMut(f64, f64),
{
    use core_graphics::display::CGDisplay;

    let (cg_button, down_type, dragged_type, up_type) = match button {
        DragButton::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
        ),
        DragButton::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
        ),
        DragButton::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
        ),
    };
    let events = foreground_drag_events(from_x, from_y, to_x, to_y, steps);
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };

    // Keep WindowServer's hardware cursor and event stream coupled. AppKit
    // hit-tests some pointer-capture surfaces against the actual cursor even
    // when the HID event carries an explicit location.
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(from_x, from_y));
    unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    observe(from_x, from_y);
    std::thread::sleep(std::time::Duration::from_millis(40));

    super::keyboard::with_global_modifier_keys(modifiers, |flags| {
        for spec in events {
            let event_type = match spec.kind {
                ForegroundDragEventKind::Move => CGEventType::MouseMoved,
                ForegroundDragEventKind::Down => down_type,
                ForegroundDragEventKind::Dragged => dragged_type,
                ForegroundDragEventKind::Up => up_type,
            };
            // A null source matches macOS's established global mouse-controller
            // path. WindowServer then carries the pressed-button state from the
            // down through every dragged event instead of treating each event as
            // an independent HIDSystemState snapshot.
            let event =
                new_global_mouse_event(event_type, spec.point, cg_button).map_err(|_| {
                    anyhow::anyhow!("foreground drag {:?} event creation failed", spec.kind)
                })?;
            event.set_flags(flags);
            crate::focus_steal::post_hid_mouse_event(&event);

            match spec.kind {
                ForegroundDragEventKind::Move => {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                ForegroundDragEventKind::Down => {
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
                ForegroundDragEventKind::Dragged => {
                    observe(spec.point.x, spec.point.y);
                    if step_delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(step_delay_ms));
                    }
                }
                ForegroundDragEventKind::Up => {}
            }
        }
        Ok(())
    })?;
    // The foreground wrapper restores the previous app immediately after this
    // function returns. Let the target's run loop consume the queued HID
    // gesture, including pointer-capture release, before that restore happens.
    std::thread::sleep(std::time::Duration::from_millis(100));
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundDragEventKind {
    Move,
    Down,
    Dragged,
    Up,
}

#[derive(Clone, Copy, Debug)]
struct ForegroundDragEvent {
    kind: ForegroundDragEventKind,
    point: CGPoint,
    progress: f64,
}

fn foreground_drag_events(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    steps: usize,
) -> Vec<ForegroundDragEvent> {
    let steps = steps.max(1);
    let mut events = Vec::with_capacity(steps + 3);
    let from = CGPoint::new(from_x, from_y);
    events.push(ForegroundDragEvent {
        kind: ForegroundDragEventKind::Move,
        point: from,
        progress: 0.0,
    });
    events.push(ForegroundDragEvent {
        kind: ForegroundDragEventKind::Down,
        point: from,
        progress: 0.0,
    });
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        events.push(ForegroundDragEvent {
            kind: ForegroundDragEventKind::Dragged,
            point: CGPoint::new(from_x + (to_x - from_x) * t, from_y + (to_y - from_y) * t),
            progress: t,
        });
    }
    events.push(ForegroundDragEvent {
        kind: ForegroundDragEventKind::Up,
        point: CGPoint::new(to_x, to_y),
        progress: 1.0,
    });
    events
}

fn new_global_mouse_event(
    event_type: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
) -> Result<CGEvent, ()> {
    let event_ref =
        unsafe { CGEventCreateMouseEvent(std::ptr::null_mut(), event_type, point, button) };
    if event_ref.is_null() {
        Err(())
    } else {
        Ok(unsafe { CGEvent::from_ptr(event_ref) })
    }
}

/// Mouse button for drag gestures.
#[derive(Clone, Copy, Debug)]
pub enum DragButton {
    Left,
    Right,
    Middle,
}

/// Middle-click at `(x, y)` with optional modifier keys.
///
/// Posts an `OtherMouseDown` / `OtherMouseUp` pair with `CGMouseButton::Center`
/// to the target pid through the same SkyLight + public-API postBoth path the
/// left- and right-click primitives use. Window-local stamping mirrors
/// `right_click_at_xy_with_window_local`.
pub fn middle_click_at_xy(pid: i32, x: f64, y: f64, modifiers: &[&str]) -> anyhow::Result<()> {
    middle_click_at_xy_inner(pid, x, y, None, modifiers)
}

/// Like `middle_click_at_xy` but stamps the window-local `(wx, wy)` point.
pub fn middle_click_at_xy_with_window_local(
    pid: i32,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    middle_click_at_xy_inner(pid, x, y, Some((wx, wy)), modifiers)
}

fn middle_click_at_xy_inner(
    pid: i32,
    x: f64,
    y: f64,
    window_local: Option<(f64, f64)>,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let point = CGPoint::new(x, y);
    let flags = parse_modifier_flags(modifiers);

    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::OtherMouseDown,
        point,
        CGMouseButton::Center,
    )
    .map_err(|_| anyhow::anyhow!("middle mouse down failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        down.set_flags(flags);
    }
    post_mouse_event(pid, &down, window_local, None, None, 1, 2, 3);
    std::thread::sleep(std::time::Duration::from_millis(16));

    let up = CGEvent::new_mouse_event(
        source,
        CGEventType::OtherMouseUp,
        point,
        CGMouseButton::Center,
    )
    .map_err(|_| anyhow::anyhow!("middle mouse up failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        up.set_flags(flags);
    }
    post_mouse_event(pid, &up, window_local, None, None, 1, 2, 3);

    Ok(())
}

/// Right-click at `(x, y)` with optional modifier keys (no window routing).
pub fn right_click_at_xy(pid: i32, x: f64, y: f64, modifiers: &[&str]) -> anyhow::Result<()> {
    right_click_at_xy_inner(pid, x, y, None, None, modifiers)
}

/// Like `right_click_at_xy` but stamps `CGEventSetWindowLocation` with the
/// window-local `(wx, wy)` point AND the window-routing fields (f51/f91/f92) so
/// the `rightMouseDown` reaches a backgrounded `NSView`.
///
/// `wid` is required for the routing fields: without a window number stamped,
/// WindowServer falls back to a screen-location hit-test that skips non-key
/// (backgrounded) windows, so the right-down never reached the NSView — the
/// reported "right-click does not fire rightMouseDown" bug. The left-click path
/// already threaded `wid`; right-click did not, which is why it broke.
///
/// The `mouseMoved` primer, `rightMouseDown` and `rightMouseUp` are each posted
/// once, through SkyLight (the public API only when the SPI is absent). Posting
/// the same event through both transports delivered it to an AppKit view twice,
/// so one right click arrived as down, down, up, up (#4679).
pub fn right_click_at_xy_with_window_local(
    pid: i32,
    x: f64,
    y: f64,
    wx: f64,
    wy: f64,
    wid: u32,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    right_click_at_xy_inner(pid, x, y, Some((wx, wy)), Some(wid), modifiers)
}

fn right_click_at_xy_inner(
    pid: i32,
    x: f64,
    y: f64,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let point = CGPoint::new(x, y);
    let flags = parse_modifier_flags(modifiers);

    let click_group_id: Option<i64> = wid.map(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as i64
    });

    // Prime cursor-tracking state at the target so AppKit hit-tests the
    // right-down at the right NSView (same rationale as the left path).
    post_mouse_moved_primer(
        pid,
        &source,
        point,
        window_local,
        wid,
        click_group_id,
        MousePostMode::SkyLightPreferred,
    );
    std::thread::sleep(std::time::Duration::from_millis(12));

    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::RightMouseDown,
        point,
        CGMouseButton::Right,
    )
    .map_err(|_| anyhow::anyhow!("right mouse down failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        down.set_flags(flags);
    }
    // button_number = 1 (right). Stamping 0 here routes the event as a left
    // button-number on the receiving side even though the type is rightMouseDown.
    post_mouse_event_with_mode(
        pid,
        &down,
        window_local,
        wid,
        click_group_id,
        1,
        1,
        3,
        MousePostMode::SkyLightPreferred,
    );
    std::thread::sleep(std::time::Duration::from_millis(28));

    let up = CGEvent::new_mouse_event(
        source,
        CGEventType::RightMouseUp,
        point,
        CGMouseButton::Right,
    )
    .map_err(|_| anyhow::anyhow!("right mouse up failed"))?;
    if flags != CGEventFlags::CGEventFlagNull {
        up.set_flags(flags);
    }
    post_mouse_event_with_mode(
        pid,
        &up,
        window_local,
        wid,
        click_group_id,
        1,
        1,
        3,
        MousePostMode::SkyLightPreferred,
    );

    Ok(())
}

/// Post a mouse event to `pid`.
///
/// Matches Swift's `MouseInput.postBoth(_:toPid:)`:
/// - Fires `SLEventPostToPid` (SkyLight path — reaches backgrounded Chromium/Catalyst).
/// - Also fires `CGEvent::post_to_pid` (public path — lands on AppKit targets where
///   SkyLight mouse delivery drops).
///
/// Both are always posted in sequence regardless of whether the other succeeded.
///
/// Field stamps applied (always):
/// - f40 = `pid`  (Chromium's synthetic-event filter)
/// - `CGEventSetWindowLocation` = window-local point (if provided)
///
/// Additional stamps when `wid` is provided (Chromium window-routing fields):
/// - f1  = `click_state`    (kCGMouseEventClickState)
/// - f3  = `button_number`  (kCGMouseEventButtonNumber: 0=left, 1=right, 2=middle)
/// - f7  = 3                (kCGMouseEventSubtype = NSEventSubtypeTouch)
/// - f51 = window_id        (windowNumber, NSEvent bridge equivalent)
/// - f58 = click_group_id   (gesture coalescing across pairs)
/// - f91 = window_id        (kCGMouseEventWindowUnderMousePointer)
/// - f92 = window_id        (kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent)
///
/// `button_number` MUST match the button encoded in the event type (right-down
/// stamped with f3=0 routes as a left-click on the receiving side — this was the
/// right-click-lands-as-nothing bug). Left=0, Right=1, Middle=2.
// These arguments are the individual CGEvent fields stamped by this low-level primitive.
#[allow(clippy::too_many_arguments)]
pub(super) fn post_mouse_event(
    pid: i32,
    event: &CGEvent,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    click_state: i64,
    button_number: i64,
    subtype: i64,
) {
    post_mouse_event_with_mode(
        pid,
        event,
        window_local,
        wid,
        click_group_id,
        click_state,
        button_number,
        subtype,
        MousePostMode::Both,
    );
}

#[allow(clippy::too_many_arguments)]
fn post_mouse_event_with_mode(
    pid: i32,
    event: &CGEvent,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    click_state: i64,
    button_number: i64,
    subtype: i64,
    mode: MousePostMode,
) {
    let event_ptr = event.as_ptr() as *mut std::ffi::c_void;

    // Stamp window-local point for backgrounded window targeting.
    if let Some((wx, wy)) = window_local {
        crate::input::skylight::set_window_location(event_ptr, wx, wy);
    }

    // Chromium / AppKit window-routing fields — stamp when window_id is known.
    if let (Some(wid), Some(cgid)) = (wid, click_group_id) {
        let window_id = wid as i64;
        let set = |f: u32, v: i64| {
            crate::input::skylight::set_integer_field(event_ptr, f, v);
        };
        set(1, click_state); // kCGMouseEventClickState
        set(3, button_number); // kCGMouseEventButtonNumber (0=left, 1=right, 2=middle)
        set(7, subtype); // kCGMouseEventSubtype (touch for clicks, normal for drags)
        set(51, window_id); // windowNumber
        set(58, cgid); // click-group ID (gesture coalescing)
        set(91, window_id); // kCGMouseEventWindowUnderMousePointer
        set(92, window_id); // kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent
    }

    // Always stamp f40 = target pid (Chromium synthetic-event filter).
    crate::input::skylight::set_integer_field(event_ptr, 40, pid as i64);

    dispatch_mouse_event(post_route(mode, skylight_available()), pid, event);
}

/// Post a stamped `mouseMoved` to `pid` at `point` before a down/up pair.
///
/// AppKit hit-tests a button/`NSView` against the cursor-tracking state the
/// window last saw; a backgrounded window that never received a move event has
/// stale tracking state, so the synthetic `mouseDown` lands "outside" the
/// control and `-mouseDown:` never fires (the synthetic-NSButton-ignored bug).
/// A leading `mouseMoved` at the target primes that state — this is exactly the
/// Step-3 `mouseMoved` of the Swift `clickViaAuthSignedPost` recipe, which the
/// default Rust pixel path had dropped. Click-state 0 / no button (move events
/// carry no button); window-routing fields still stamped so the move reaches the
/// right backgrounded window.
fn post_mouse_moved_primer(
    pid: i32,
    source: &CGEventSource,
    point: CGPoint,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    click_group_id: Option<i64>,
    mode: MousePostMode,
) {
    if let Ok(mv) = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::MouseMoved,
        point,
        CGMouseButton::Left,
    ) {
        post_mouse_event_with_mode(pid, &mv, window_local, wid, click_group_id, 0, 0, 3, mode);
    }
}

/// Synthesize a targeted mouse-wheel scroll at `(screen_x, screen_y)`,
/// posted to `pid`.
///
/// This is the targeted wheel path. Unlike the keystroke path (PageDown /
/// arrow keys), which only ever drives the *focused* / page scroller, a real
/// wheel event is hit-tested by the renderer at the cursor point: whatever
/// element sits under `(screen_x, screen_y)` receives the scroll. That is the
/// only way to scroll a nested `overflow:auto` div that has no `tabindex` and
/// therefore can never take keyboard focus (verified no-op via keystrokes on
/// WKWebView's inner `scroll-tall` and WebView2).
///
/// `CGEventCreateScrollWheelEvent2` builds a line-unit event; we then anchor it
/// at the target point with `CGEventSetLocation` so the renderer routes it
/// correctly, and stamp the same background-delivery fields the click
/// primitives use (window-local point + f40 pid filter + window-routing fields)
/// so it reaches backgrounded Chromium/Catalyst/WKWebView targets.
///
/// Sign convention (macOS): a POSITIVE `delta_y_per_tick` scrolls the content
/// toward the top (reveals content ABOVE); NEGATIVE reveals content BELOW.
/// POSITIVE `delta_x_per_tick` reveals content to the LEFT; NEGATIVE to the
/// RIGHT. The direction→delta mapping lives in the `scroll` tool; this primitive
/// stays sign-agnostic (if a live target scrolls inverted, flip there).
///
/// `ticks` discrete wheel events are posted (one per notch), mirroring the
/// keystroke path's `amount` repetitions, each separated by a short gap so the
/// renderer animates per-notch instead of coalescing into a single jump.
///
/// `window_local`/`wid`: when known, stamp the window-local point and the
/// Chromium window-routing fields (f51/f91/f92) for backgrounded delivery —
/// identical in spirit to `post_mouse_event`.
// These arguments are the individual wheel-event fields used by existing callers.
#[allow(clippy::too_many_arguments)]
pub fn scroll_wheel_at_xy(
    pid: i32,
    screen_x: f64,
    screen_y: f64,
    window_local: Option<(f64, f64)>,
    wid: Option<u32>,
    delta_y_per_tick: i32,
    delta_x_per_tick: i32,
    ticks: usize,
) -> anyhow::Result<()> {
    use core_graphics::event::ScrollEventUnit;

    // Prime AppKit/WebKit's tracking state at the target before the wheel
    // gesture. Background windows can retain a stale hit-test location; a
    // nested overflow scroller then receives the wheel nowhere even though
    // the event is successfully posted to the target pid.
    let primer_source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    post_mouse_moved_primer(
        pid,
        &primer_source,
        CGPoint::new(screen_x, screen_y),
        window_local,
        wid,
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as i64,
        ),
        MousePostMode::Both,
    );
    std::thread::sleep(std::time::Duration::from_millis(12));

    for _ in 0..ticks.max(1) {
        // Fresh source per event, matching the click primitives.
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
        // wheel_count = 2 → both axes carried (vertical = wheel1/axis-1,
        // horizontal = wheel2/axis-2). Convert the driver's pixel-tuned step
        // into a bounded line delta for the event API.
        let wheel_y = (delta_y_per_tick / 120).clamp(-10, 10);
        let wheel_x = (delta_x_per_tick / 120).clamp(-10, 10);
        let event =
            CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, wheel_y, wheel_x, 0)
                .map_err(|_| anyhow::anyhow!("CGEvent::new_scroll_event failed"))?;

        let event_ptr = event.as_ptr() as *mut std::ffi::c_void;

        // Anchor the event at the target screen point so the renderer's wheel
        // hit-test routes the scroll to the element under the cursor.
        unsafe { CGEventSetLocation(event_ptr, screen_x, screen_y) };

        // Background-delivery stamps (mirror post_mouse_event).
        if let Some((wx, wy)) = window_local {
            crate::input::skylight::set_window_location(event_ptr, wx, wy);
        }
        if let Some(wid) = wid {
            let window_id = wid as i64;
            let set = |f: u32, v: i64| {
                crate::input::skylight::set_integer_field(event_ptr, f, v);
            };
            set(51, window_id); // windowNumber
            set(91, window_id); // kCGMouseEventWindowUnderMousePointer
            set(92, window_id); // ...ThatCanHandleThisEvent
        }
        // f40 = target pid (Chromium synthetic-event filter).
        crate::input::skylight::set_integer_field(event_ptr, 40, pid as i64);

        // Belt+suspenders post: SkyLight reaches backgrounded Chromium/Catalyst;
        // the public path lands on AppKit/WKWebView. Mouse-class → no auth envelope.
        crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, false);
        event.post_to_pid(pid as libc::pid_t);

        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(())
}

extern "C" {
    /// Quartz mouse constructor with a null source, matching established
    /// foreground controller libraries and the global scroll path above.
    fn CGEventCreateMouseEvent(
        source: core_graphics::sys::CGEventSourceRef,
        event_type: core_graphics::event::CGEventType,
        point: CGPoint,
        button: core_graphics::event::CGMouseButton,
    ) -> core_graphics::sys::CGEventRef;

    /// `void CGEventSetLocation(CGEventRef event, CGPoint location)`.
    ///
    /// `CGPoint { double x, double y }` is classified as two FP eightbytes on
    /// arm64 / x86-64, so passing the two doubles as separate args is
    /// ABI-identical to passing the struct by value — the same trick the
    /// SkyLight bridge uses for `CGEventSetWindowLocation`.
    fn CGEventSetLocation(event: *mut std::ffi::c_void, x: f64, y: f64);
}

fn parse_modifier_flags(modifiers: &[&str]) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for m in modifiers {
        match m.to_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" | "win" => {
                flags |= CGEventFlags::CGEventFlagCommand
            }
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            _ => {}
        }
    }
    flags
}

/// Test seam for [`dispatch_mouse_event`]. A test starts a [`Capture`] on its
/// own thread, runs a real input primitive, and reads back the transport each
/// event would have been handed to. Nothing is posted while a capture is
/// active, so the tests need no window server, target app, or TCC grant.
#[cfg(test)]
mod post_sink {
    use super::PostRoute;
    use core_graphics::event::{CGEvent, EventField};
    use std::cell::RefCell;

    pub(super) const MODIFIER_MASK: u64 = 0x0002_0000 // shift
        | 0x0004_0000 // control
        | 0x0008_0000 // option
        | 0x0010_0000; // command

    /// One event handed to a transport: its type name, the transport, and the
    /// button number stamped on it (0 left, 1 right, 2 middle).
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(super) struct Posted {
        pub event: String,
        pub route: PostRoute,
        pub button: i64,
    }

    impl Posted {
        pub(super) fn new(event: &str, route: PostRoute, button: i64) -> Self {
            Self {
                event: event.to_owned(),
                route,
                button,
            }
        }
    }

    struct State {
        skylight_available: bool,
        posted: Vec<Posted>,
        /// Modifier flags carried by each posted event, parallel to `posted`.
        modifiers: Vec<u64>,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    pub(super) struct Capture;

    impl Capture {
        /// Start capturing on this thread, with the SkyLight SPI reported as
        /// present or absent.
        pub(super) fn start(skylight_available: bool) -> Self {
            STATE.with(|state| {
                *state.borrow_mut() = Some(State {
                    skylight_available,
                    posted: Vec::new(),
                    modifiers: Vec::new(),
                });
            });
            Self
        }

        /// Shift/control/option/command flags of each posted event. The rest of
        /// the flag word reflects whatever keys the developer holds while the
        /// tests run, so it is masked out.
        pub(super) fn modifiers(&self) -> Vec<u64> {
            STATE.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .map(|state| state.modifiers.clone())
                    .unwrap_or_default()
            })
        }

        pub(super) fn posted(&self) -> Vec<Posted> {
            STATE.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .map(|state| state.posted.clone())
                    .unwrap_or_default()
            })
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            STATE.with(|state| *state.borrow_mut() = None);
        }
    }

    pub(super) fn skylight_override() -> Option<bool> {
        STATE.with(|state| {
            state
                .borrow()
                .as_ref()
                .map(|state| state.skylight_available)
        })
    }

    /// Record `event` when a capture is active; returns whether it did.
    pub(super) fn record(route: PostRoute, event: &CGEvent) -> bool {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let Some(state) = state.as_mut() else {
                return false;
            };
            state.posted.push(Posted {
                event: format!("{:?}", event.get_type()),
                route,
                button: event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER),
            });
            state
                .modifiers
                .push(event.get_flags().bits() & MODIFIER_MASK);
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::post_sink::{Capture, Posted};
    use super::*;

    #[test]
    fn capture_bound_background_route_preserves_nonzero_origin_hit() {
        let frame = crate::tools::px_frame::WindowPxFrame {
            bounds: crate::windows::WindowBounds {
                x: 170.0,
                y: 194.0,
                width: 760.0,
                height: 460.0,
            },
            scale: 2.0,
        };
        let capture_local = (394.0, 310.0);
        let admitted_native = (capture_local.0 * frame.scale, capture_local.1 * frame.scale);
        let (screen_x, screen_y, local_x, local_y) =
            frame.to_screen(admitted_native.0, admitted_native.1);
        assert_eq!((screen_x, screen_y), (564.0, 504.0));
        assert_eq!((local_x, local_y), capture_local);

        let plan = chromium_click_route_plan(screen_x, screen_y, local_x, local_y, 1);
        assert_eq!(
            plan,
            vec![
                ChromiumClickRouteStep {
                    kind: ChromiumClickEventKind::Move,
                    event_location: (564.0, 504.0),
                    skylight_window_location: (564.0, 504.0),
                    public_window_location: (564.0, 504.0),
                    click_state: 0,
                    phase: 2,
                    button_number: 0,
                    subtype: 3,
                    delay_after_ms: 15,
                },
                ChromiumClickRouteStep {
                    kind: ChromiumClickEventKind::Down,
                    event_location: (-1.0, -1.0),
                    skylight_window_location: (-1.0, -1.0),
                    public_window_location: (-1.0, -1.0),
                    click_state: 1,
                    phase: 1,
                    button_number: 0,
                    subtype: 3,
                    delay_after_ms: 1,
                },
                ChromiumClickRouteStep {
                    kind: ChromiumClickEventKind::Up,
                    event_location: (-1.0, -1.0),
                    skylight_window_location: (-1.0, -1.0),
                    public_window_location: (-1.0, -1.0),
                    click_state: 1,
                    phase: 2,
                    button_number: 0,
                    subtype: 3,
                    delay_after_ms: 100,
                },
                ChromiumClickRouteStep {
                    kind: ChromiumClickEventKind::Down,
                    event_location: (564.0, 504.0),
                    skylight_window_location: (564.0, 504.0),
                    public_window_location: (564.0, 504.0),
                    click_state: 1,
                    phase: 3,
                    button_number: 0,
                    subtype: 3,
                    delay_after_ms: 1,
                },
                ChromiumClickRouteStep {
                    kind: ChromiumClickEventKind::Up,
                    event_location: (564.0, 504.0),
                    skylight_window_location: (564.0, 504.0),
                    public_window_location: (564.0, 504.0),
                    click_state: 1,
                    phase: 3,
                    button_number: 0,
                    subtype: 3,
                    delay_after_ms: 0,
                },
            ]
        );

        let screen_to_canvas = (-frame.bounds.x, -frame.bounds.y);
        let unconverted_journal_point = (
            capture_local.0 + screen_to_canvas.0,
            capture_local.1 + screen_to_canvas.1,
        );
        assert_eq!(unconverted_journal_point, (224.0, 116.0));
        assert!(!(292.0..=496.0).contains(&unconverted_journal_point.0));

        let delivered = plan
            .iter()
            .find(|step| step.kind == ChromiumClickEventKind::Down && step.phase == 3)
            .expect("target mouse-down step");
        assert_eq!(delivered.event_location, (564.0, 504.0));
        assert_eq!(delivered.skylight_window_location, (564.0, 504.0));
        assert_eq!(delivered.public_window_location, (564.0, 504.0));
        let skylight_journal_point = (
            delivered.skylight_window_location.0 + screen_to_canvas.0,
            delivered.skylight_window_location.1 + screen_to_canvas.1,
        );
        assert_eq!(skylight_journal_point, capture_local);
        assert!((292.0..=496.0).contains(&skylight_journal_point.0));
        assert!((132.0..=310.0).contains(&skylight_journal_point.1));
    }

    #[test]
    fn foreground_drag_plan_is_one_coherent_gesture() {
        let events = foreground_drag_events(10.0, 20.0, 40.0, 80.0, 3);
        let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            [
                ForegroundDragEventKind::Move,
                ForegroundDragEventKind::Down,
                ForegroundDragEventKind::Dragged,
                ForegroundDragEventKind::Dragged,
                ForegroundDragEventKind::Dragged,
                ForegroundDragEventKind::Up,
            ]
        );
        let points: Vec<_> = events
            .iter()
            .map(|event| (event.point.x, event.point.y))
            .collect();
        assert_eq!(
            points,
            [
                (10.0, 20.0),
                (10.0, 20.0),
                (20.0, 40.0),
                (30.0, 60.0),
                (40.0, 80.0),
                (40.0, 80.0),
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == ForegroundDragEventKind::Up)
                .count(),
            1,
            "the HID gesture must release exactly once"
        );
        let cursor_plan: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    ForegroundDragEventKind::Move | ForegroundDragEventKind::Dragged
                )
            })
            .map(|event| (event.point.x, event.point.y))
            .collect();
        assert_eq!(
            cursor_plan,
            [(10.0, 20.0), (20.0, 40.0), (30.0, 60.0), (40.0, 80.0)]
        );
    }

    #[test]
    fn foreground_drag_plan_keeps_a_drag_sample_for_zero_steps() {
        let events = foreground_drag_events(1.0, 2.0, 3.0, 4.0, 0);
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].kind, ForegroundDragEventKind::Dragged);
        assert_eq!((events[2].point.x, events[2].point.y), (3.0, 4.0));
    }

    #[test]
    fn global_drag_events_use_hardware_like_source_and_button_fields() {
        use core_graphics::event::EventField;

        let point = CGPoint::new(10.0, 20.0);
        let cases = [
            (CGEventType::LeftMouseDown, 1, 1.0),
            (CGEventType::LeftMouseDragged, 1, 1.0),
            (CGEventType::LeftMouseUp, 0, 0.0),
        ];
        for (event_type, click_state, pressure) in cases {
            let event = new_global_mouse_event(event_type, point, CGMouseButton::Left).unwrap();
            assert_eq!(
                event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID),
                CGEventSourceStateID::CombinedSessionState as i64
            );
            assert_eq!(
                event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE),
                click_state
            );
            assert_eq!(
                event.get_double_value_field(EventField::MOUSE_EVENT_PRESSURE),
                pressure
            );
        }
    }

    #[test]
    fn skylight_preferred_posts_once_and_never_through_both_transports() {
        use MousePostMode::*;
        use PostRoute::*;
        assert_eq!(post_route(SkyLightPreferred, true), SkyLight);
        assert_eq!(
            post_route(SkyLightPreferred, false),
            PublicPid,
            "the public API is only a fallback for a missing SPI"
        );
        for available in [true, false] {
            assert_eq!(post_route(PublicOnly, available), PublicPid);
            assert_eq!(post_route(HidOnly, available), Hid);
        }
    }

    /// The left background click has posted each event once since the
    /// duplicate-delivery fix. This pins that sequence so the other pointer
    /// primitives can be held to the same shape.
    #[test]
    fn background_left_click_posts_one_move_and_one_down_up_pair_through_skylight() {
        let capture = Capture::start(true);
        click_at_xy_chromium(1, 100.0, 100.0, 10.0, 10.0, 7, 1, &[]).unwrap();
        assert_eq!(
            capture.posted(),
            [
                Posted::new("MouseMoved", PostRoute::SkyLight, 0),
                // Off-screen primer pair that satisfies Chromium's activation gate.
                Posted::new("LeftMouseDown", PostRoute::SkyLight, 0),
                Posted::new("LeftMouseUp", PostRoute::SkyLight, 0),
                Posted::new("LeftMouseDown", PostRoute::SkyLight, 0),
                Posted::new("LeftMouseUp", PostRoute::SkyLight, 0),
            ]
        );
    }

    #[test]
    fn background_left_click_falls_back_to_the_public_api_once_without_the_spi() {
        let capture = Capture::start(false);
        click_at_xy_chromium(1, 100.0, 100.0, 10.0, 10.0, 7, 1, &[]).unwrap();
        let posted = capture.posted();
        assert_eq!(posted.len(), 5);
        assert!(posted.iter().all(|p| p.route == PostRoute::PublicPid));
    }

    #[test]
    fn background_right_click_posts_one_move_and_one_down_up_pair_through_skylight() {
        let capture = Capture::start(true);
        right_click_at_xy_with_window_local(1, 100.0, 100.0, 10.0, 10.0, 7, &[]).unwrap();
        assert_eq!(
            capture.posted(),
            [
                Posted::new("MouseMoved", PostRoute::SkyLight, 0),
                Posted::new("RightMouseDown", PostRoute::SkyLight, 1),
                Posted::new("RightMouseUp", PostRoute::SkyLight, 1),
            ]
        );
    }

    #[test]
    fn background_right_click_falls_back_to_the_public_api_once_without_the_spi() {
        let capture = Capture::start(false);
        right_click_at_xy_with_window_local(1, 100.0, 100.0, 10.0, 10.0, 7, &[]).unwrap();
        assert_eq!(
            capture.posted(),
            [
                Posted::new("MouseMoved", PostRoute::PublicPid, 0),
                Posted::new("RightMouseDown", PostRoute::PublicPid, 1),
                Posted::new("RightMouseUp", PostRoute::PublicPid, 1),
            ]
        );
    }

    #[test]
    fn window_less_right_click_posts_one_down_up_pair() {
        let capture = Capture::start(true);
        right_click_at_xy(1, 100.0, 100.0, &[]).unwrap();
        let events: Vec<_> = capture
            .posted()
            .into_iter()
            .map(|p| (p.event, p.route))
            .collect();
        assert_eq!(
            events,
            [
                ("MouseMoved".to_owned(), PostRoute::SkyLight),
                ("RightMouseDown".to_owned(), PostRoute::SkyLight),
                ("RightMouseUp".to_owned(), PostRoute::SkyLight),
            ]
        );
    }

    #[test]
    fn right_click_modifiers_ride_on_the_single_down_and_up() {
        let capture = Capture::start(true);
        right_click_at_xy_with_window_local(1, 100.0, 100.0, 10.0, 10.0, 7, &["cmd", "shift"])
            .unwrap();
        let posted = capture.posted();
        let modifiers = capture.modifiers();
        let names: Vec<_> = posted.iter().map(|p| p.event.as_str()).collect();
        assert_eq!(names, ["MouseMoved", "RightMouseDown", "RightMouseUp"]);
        let expected =
            CGEventFlags::CGEventFlagCommand.bits() | CGEventFlags::CGEventFlagShift.bits();
        assert_eq!(&modifiers[1..], [expected, expected]);
    }

    fn tap(options: u32, mask: u64, owner: i32, tapped: i32, enabled: bool) -> EventTapInformation {
        EventTapInformation {
            options,
            events_of_interest: mask,
            tapping_process: owner,
            process_being_tapped: tapped,
            enabled,
            ..Default::default()
        }
    }

    /// The record matches CGEventTapInformation's C layout (48 bytes).
    #[test]
    fn event_tap_information_has_the_c_layout() {
        assert_eq!(std::mem::size_of::<EventTapInformation>(), 48);
        assert_eq!(std::mem::offset_of!(EventTapInformation, events_of_interest), 16);
        assert_eq!(std::mem::offset_of!(EventTapInformation, enabled), 32);
    }

    /// Only another process's enabled, active, global tap on the click's own
    /// events can drop or hold a HID click (Handy's mask is 0x6001c1e).
    #[test]
    fn only_a_foreign_active_global_tap_on_the_buttons_counts() {
        let left = event_mask(&[CGEventType::LeftMouseDown, CGEventType::LeftMouseUp]);
        assert_eq!(left, 0b110);
        let handy = 0x600_1c1e;
        let own = 10;
        assert_eq!(foreign_active_tap_in(&[tap(0, handy, 77, 0, true)], own, left), Some(77));
        // Listen-only, disabled, per-process, own, or not on these events.
        assert_eq!(foreign_active_tap_in(&[tap(1, handy, 77, 0, true)], own, left), None);
        assert_eq!(foreign_active_tap_in(&[tap(0, handy, 77, 0, false)], own, left), None);
        assert_eq!(foreign_active_tap_in(&[tap(0, handy, 77, 55, true)], own, left), None);
        assert_eq!(foreign_active_tap_in(&[tap(0, handy, own, 0, true)], own, left), None);
        assert_eq!(foreign_active_tap_in(&[tap(0, 1 << 10, 77, 0, true)], own, left), None);
        // A keyboard-only tap among others does not hide the mouse one.
        let taps = [tap(0, 1 << 10, 66, 0, true), tap(1, handy, 88, 0, true), tap(0, handy, 77, 0, true)];
        assert_eq!(foreign_active_tap_in(&taps, own, left), Some(77));
    }

    /// The passage rule over the HID and combined-session counters, from the
    /// VM B measurements: no tap passes at once; a tap holding each event
    /// 300 ms leaves the click pending until it releases both; a dropping
    /// tap leaves it pending; counters that never show the click say nothing.
    #[test]
    fn hid_passage_follows_the_session_counters() {
        let at = |hid: [u32; 2], session: [u32; 2]| ButtonCounters {
            hid: [hid[0], hid[1], 7, 7, 3, 3],
            session: [session[0], session[1], 7, 7, 3, 3],
        };
        let left = [0, 1];
        let none = [0; 6];
        let base = at([100, 100], [95, 95]);
        assert_eq!(hid_passage(base, at([101, 101], [96, 96]), none, left, 1), HidPassage::Passed);
        assert_eq!(hid_passage(base, at([101, 101], [95, 95]), none, left, 1), HidPassage::Pending);
        assert_eq!(hid_passage(base, at([101, 101], [96, 95]), none, left, 1), HidPassage::Pending);
        assert_eq!(hid_passage(base, at([101, 100], [95, 95]), none, left, 1), HidPassage::Unmeasured);
        // Two pairs (a double click); a user's click in the same window
        // counts on both sides once it passes.
        assert_eq!(hid_passage(base, at([102, 102], [97, 97]), none, left, 2), HidPassage::Passed);
        assert_eq!(hid_passage(base, at([103, 103], [97, 97]), none, left, 2), HidPassage::Pending);
        assert_eq!(hid_passage(base, at([103, 103], [98, 98]), none, left, 2), HidPassage::Passed);
        // Counters wrap.
        let wrap = at([u32::MAX, u32::MAX], [u32::MAX, u32::MAX]);
        assert_eq!(hid_passage(wrap, at([0, 0], [0, 0]), none, left, 1), HidPassage::Passed);
        // An earlier held click released now must not read as this one
        // passing: its two events are owed first.
        let owed = [1, 1, 0, 0, 0, 0];
        assert_eq!(hid_passage(base, at([101, 101], [96, 96]), owed, left, 1), HidPassage::Pending);
        assert_eq!(hid_passage(base, at([101, 101], [97, 97]), owed, left, 1), HidPassage::Passed);
        // A held right button event holds a left click behind it.
        let mut right_held = at([101, 101], [96, 96]);
        right_held.hid[2] += 1;
        assert_eq!(hid_passage(base, right_held, none, left, 1), HidPassage::Pending);
    }

    /// Outstanding events since a base: entered minus passed, per type.
    #[test]
    fn outstanding_counts_entered_but_not_passed() {
        let base = ButtonCounters { hid: [10; 6], session: [8; 6] };
        let mut now = base;
        now.hid[0] += 2;
        now.session[0] += 1;
        now.session[1] += 3; // more passed than entered: earlier history, not owed
        assert_eq!(now.outstanding_since(base), [1, 0, 0, 0, 0, 0]);
    }

}
