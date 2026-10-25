//! The clock of hardware input events.
//!
//! A CGEvent's timestamp is nanoseconds of uptime (`mach_absolute_time`
//! scaled by the timebase), the moment the event happened. Events cua builds
//! start with timestamp 0, and receivers compare it with now: on a Mac with
//! days of uptime, Chromium apps let a hover through but did not act on clicks
//! stamped near boot (incident 20, 2026-10-23). Stamp every event as it is
//! handed to a transport.

use anyhow::{anyhow, Context, Result};
use core_graphics::event::CGEvent;
use foreign_types::ForeignType;
use std::sync::OnceLock;

extern "C" {
    fn CGEventSetTimestamp(event: core_graphics::sys::CGEventRef, timestamp: u64);
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

/// Stamp `event` with the current uptime, as a hardware event carries the
/// moment it happened. Fails, leaving the timestamp alone, only if the
/// timebase is unavailable.
pub(crate) fn stamp_now(event: &CGEvent) -> Result<()> {
    let now = uptime_nanoseconds()?;
    unsafe { CGEventSetTimestamp(event.as_ptr(), now) };
    Ok(())
}

/// The current uptime in nanoseconds, the clock events are stamped with.
#[cfg(test)]
pub(crate) fn uptime_now() -> Result<u64> {
    uptime_nanoseconds()
}
