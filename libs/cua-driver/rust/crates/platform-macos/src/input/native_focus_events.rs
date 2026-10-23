//! Recovered native focus notifications. Construction never posts input.

use anyhow::{anyhow, ensure, Context, Result};
use core_foundation::base::{CFRetain, CFTypeRef};
use core_graphics::event::{CGEvent, CGEventFlags};
use core_graphics::geometry::{CGPoint, CGRect};
use foreign_types::ForeignType;
use objc2::{msg_send, rc::Retained, Encoding, RefEncode};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;
use std::ffi::c_void;
use std::sync::OnceLock;

#[repr(C)]
struct NativeCGEvent {
    _private: [u8; 0],
}

// The NSEvent property returns the opaque CoreGraphics pointer encoding.
unsafe impl RefEncode for NativeCGEvent {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__CGEvent", &[]));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Notification {
    Activate,
    Refocus,
    Deactivate,
}

/// The caller must prove these belong to the authorized target window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowActivation {
    pub window_id: u32,
    pub bounds: CGRect,
    pub activation_point: CGPoint,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSetTimestamp(event: *mut c_void, timestamp: u64);
    // CGPoint's two doubles use the same Darwin ABI as these existing bindings.
    fn CGEventSetLocation(event: *mut c_void, x: f64, y: f64);
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
}

fn own_cg_event(event: &NSEvent) -> Result<CGEvent> {
    let ptr: *mut NativeCGEvent = unsafe { msg_send![event, CGEvent] };
    ensure!(!ptr.is_null(), "NSEvent did not produce a CGEvent");
    // CGEvent is a borrowed property; retain it before the NSEvent is released.
    unsafe {
        CFRetain(ptr as CFTypeRef);
        Ok(CGEvent::from_ptr(ptr.cast()))
    }
}

fn other_ns_event(kind: Notification, window_id: u32) -> Result<Retained<NSEvent>> {
    let (event_type, subtype) = match kind {
        Notification::Activate => (13, 1),
        Notification::Refocus => (21, i16::MIN),
        Notification::Deactivate => (13, 2),
    };
    let flags = if kind == Notification::Activate && window_id != 0 {
        0xc0000
    } else {
        0
    };
    unsafe {
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType(event_type),
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::from_bits_retain(flags),
            0.0,
            window_id as isize,
            None,
            subtype,
            0,
            0,
        )
    }
    .context("native focus NSEvent construction failed")
}

fn other_event(kind: Notification, window_id: u32) -> Result<CGEvent> {
    let event = other_ns_event(kind, window_id)?;
    own_cg_event(&event)
}

pub(crate) fn notification(kind: Notification) -> Result<CGEvent> {
    other_event(kind, 0)
}

/// `None` is the native fallback for missing window/ID/activation-point input.
/// Missing bounds alone must not be represented as `None`: that native branch
/// emits a window notification without the pair and is not exposed here.
pub(crate) fn activation(window: Option<WindowActivation>) -> Result<Vec<CGEvent>> {
    let Some(window) = window else {
        return Ok(vec![notification(Notification::Activate)?]);
    };
    ensure!(window.window_id != 0, "native activation window ID is zero");
    let bounds = window.bounds;
    let point = window.activation_point;
    ensure!(
        [
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
            point.x,
            point.y,
        ]
        .into_iter()
        .all(f64::is_finite),
        "native activation geometry is not finite"
    );
    let local = CGPoint::new(
        point.x - bounds.origin.x.min(bounds.origin.x + bounds.size.width),
        point.y - bounds.origin.y.min(bounds.origin.y + bounds.size.height),
    );
    let mut events = vec![other_event(Notification::Activate, window.window_id)?];
    for (event_type, event_number) in [(1, 1), (2, 2)] {
        let event = unsafe {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType(event_type),
                NSPoint::new(point.x, point.y),
                NSEventModifierFlags::empty(),
                0.0,
                window.window_id as isize,
                None,
                event_number,
                1,
                1.0,
            )
        }
        .context("native activation mouse NSEvent construction failed")?;
        let event = own_cg_event(&event)?;
        event.set_flags(CGEventFlags::CGEventFlagNull);
        let ptr = event.as_ptr().cast();
        unsafe {
            CGEventSetLocation(ptr, point.x, point.y);
            for (field, value) in [
                (3, 0),
                (7, 3),
                (91, window.window_id as i64),
                (92, window.window_id as i64),
            ] {
                CGEventSetIntegerValueField(ptr, field, value);
            }
        }
        // The native subtype-3 pair requires this private coordinate field.
        ensure!(
            super::skylight::set_window_location(ptr, local.x, local.y),
            "native window activation requires CGEventSetWindowLocation"
        );
        events.push(event);
    }
    Ok(events)
}

#[allow(deprecated)] // Existing libc bindings expose the platform uptime ABI.
fn uptime_nanoseconds() -> Result<u64> {
    static TIMEBASE: OnceLock<Result<(u32, u32), i32>> = OnceLock::new();
    let (numer, denom) = TIMEBASE
        .get_or_init(|| {
            let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
            let status = unsafe { libc::mach_timebase_info(&mut info) };
            if status == 0 && info.denom != 0 {
                Ok((info.numer, info.denom))
            } else {
                Err(status)
            }
        })
        .as_ref()
        .map_err(|status| anyhow!("could not resolve uptime timebase: {status}"))?;
    // Native send uses DispatchTime.now().uptimeNanoseconds. Both use the
    // uptime clock; exact Swift/libdispatch conversion code was not recovered.
    let ticks = unsafe { libc::mach_absolute_time() };
    u64::try_from((ticks as u128 * *numer as u128) / *denom as u128)
        .context("uptime timestamp overflow")
}

/// Post the same owned events directly, without the automation auth envelope.
/// A successful return reports posting attempts, not target consumption.
pub(crate) fn post(pid: i32, events: &[CGEvent]) -> Result<()> {
    ensure!(pid > 0, "native focus recipient PID is invalid");
    for event in events {
        let timestamp = uptime_nanoseconds()?;
        unsafe { CGEventSetTimestamp(event.as_ptr().cast(), timestamp) };
        event.post_to_pid(pid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::geometry::CGSize;

    extern "C" {
        // Raw type avoids interpreting the private type 21 as a Rust enum.
        fn CGEventGetType(event: *mut c_void) -> u32;
        fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    }

    fn field(event: &CGEvent, field: u32) -> i64 {
        unsafe { CGEventGetIntegerValueField(event.as_ptr().cast(), field) }
    }

    #[test]
    fn notifications_keep_native_types_and_signed_subtype() {
        objc2::rc::autoreleasepool(|_| {
            for (kind, event_type, subtype) in [
                (Notification::Activate, 13, 1),
                (Notification::Refocus, 21, i16::MIN),
                (Notification::Deactivate, 13, 2),
            ] {
                let ns_event = other_ns_event(kind, 0).unwrap();
                assert_eq!(unsafe { ns_event.subtype().0 }, subtype);
                let event = own_cg_event(&ns_event).unwrap();
                assert_eq!(unsafe { CGEventGetType(event.as_ptr().cast()) }, event_type);
                // The bridge encodes type-13 subtype here, but not private 21.
                if event_type == 13 {
                    assert_eq!(field(&event, 83) as i16, subtype);
                }
                assert_eq!(event.get_flags(), CGEventFlags::CGEventFlagNull);
            }
        });
    }

    #[test]
    fn missing_inputs_use_only_activation_notification() {
        objc2::rc::autoreleasepool(|_| {
            let events = activation(None).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(field(&events[0], 83), 1);
            assert_eq!(field(&events[0], 51), 0);
        });
    }

    #[test]
    fn window_activation_is_tagged_and_never_silently_falls_back() {
        objc2::rc::autoreleasepool(|_| {
            let window = WindowActivation {
                window_id: 42,
                bounds: CGRect::new(&CGPoint::new(100.0, 200.0), &CGSize::new(400.0, 300.0)),
                activation_point: CGPoint::new(120.0, 240.0),
            };
            let events = match activation(Some(window)) {
                Ok(events) => events,
                Err(error) => {
                    assert_eq!(
                        error.to_string(),
                        "native window activation requires CGEventSetWindowLocation"
                    );
                    return;
                }
            };
            assert_eq!(events.len(), 3);
            assert_eq!(events[0].get_flags().bits(), 0xc0000);
            assert_eq!(field(&events[0], 51), 42);
            for (event, event_type) in events[1..].iter().zip([1, 2]) {
                assert_eq!(unsafe { CGEventGetType(event.as_ptr().cast()) }, event_type);
                assert_eq!(field(event, 3), 0);
                assert_eq!(field(event, 7), 3);
                assert_eq!(field(event, 91), 42);
                assert_eq!(field(event, 92), 42);
                assert_eq!(event.get_flags(), CGEventFlags::CGEventFlagNull);
            }
        });
    }

    #[test]
    fn invalid_window_inputs_are_refused_before_construction() {
        let window = WindowActivation {
            window_id: 0,
            bounds: CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(400.0, 300.0)),
            activation_point: CGPoint::new(1.0, 1.0),
        };
        for (window, expected) in [
            (window, "native activation window ID is zero"),
            (
                WindowActivation {
                    window_id: 42,
                    activation_point: CGPoint::new(f64::NAN, 1.0),
                    ..window
                },
                "native activation geometry is not finite",
            ),
        ] {
            let error = match activation(Some(window)) {
                Err(error) => error,
                Ok(_) => panic!("invalid window inputs accepted"),
            };
            assert_eq!(error.to_string(), expected);
        }
    }
}
