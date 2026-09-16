//! Private metadata protocol for the separate preview process.
//! A missing target means clear, never permission to capture the desktop.

use std::io::{BufRead, Read, Write};

use serde::{Deserialize, Serialize};

pub const MAX_UPDATE_BYTES: usize = 4096;
const MAX_LABEL_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowTarget {
    pub pid: i32,
    pub window_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverUpdate {
    pub version: u8,
    pub generation: u64,
    pub target: Option<WindowTarget>,
    pub action_label: String,
    pub timestamp_ms: u64,
}

impl ObserverUpdate {
    /// The pipe is ordered. Equal generations may update the label, but may
    /// never change the window. Returning to an earlier target needs a new generation.
    pub fn follows(&self, previous: &Self) -> bool {
        self.generation > previous.generation
            || (self.generation == previous.generation && self.target == previous.target)
    }
    pub fn new(
        generation: u64,
        window_id: Option<u64>,
        pid: Option<i64>,
        action_label: &str,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            version: 1,
            generation,
            target: window_id.zip(pid).and_then(|(window_id, pid)| {
                let target = WindowTarget {
                    pid: i32::try_from(pid).ok()?,
                    window_id: u32::try_from(window_id).ok()?,
                };
                (target.pid > 0 && target.window_id > 0).then_some(target)
            }),
            action_label: action_label
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_LABEL_CHARS)
                .collect(),
            timestamp_ms,
        }
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported preview protocol");
        anyhow::ensure!(self.generation > 0, "invalid preview generation");
        anyhow::ensure!(
            self.target
                .is_none_or(|target| target.pid > 0 && target.window_id > 0),
            "invalid preview target"
        );
        anyhow::ensure!(
            self.action_label.chars().count() <= MAX_LABEL_CHARS
                && !self.action_label.chars().any(char::is_control),
            "invalid preview label"
        );
        Ok(())
    }
}

pub fn read_update(reader: &mut impl BufRead) -> anyhow::Result<Option<ObserverUpdate>> {
    let mut line = Vec::new();
    if reader
        .take(MAX_UPDATE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?
        == 0
    {
        return Ok(None);
    }
    anyhow::ensure!(
        line.len() <= MAX_UPDATE_BYTES,
        "preview update is too large"
    );
    anyhow::ensure!(line.last() == Some(&b'\n'), "incomplete preview update");
    let update: ObserverUpdate = serde_json::from_slice(&line)?;
    update.validate()?;
    Ok(Some(update))
}

/// Called only from the preview publisher worker. Pipe backpressure must never
/// run on a tool task. No response or acknowledgement is required from the child.
pub fn write_update(writer: &mut impl Write, update: &ObserverUpdate) -> anyhow::Result<()> {
    update.validate()?;
    let mut bytes = serde_json::to_vec(update)?;
    bytes.push(b'\n');
    anyhow::ensure!(
        bytes.len() <= MAX_UPDATE_BYTES,
        "preview update is too large"
    );
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn invalid_or_incomplete_window_identity_clears_instead_of_wrapping_ids() {
        for (window, pid) in [
            (None, Some(42)),
            (Some(7), None),
            (Some(0), Some(42)),
            (Some(7), Some(0)),
            (Some(7), Some(-1)),
            (Some(u64::from(u32::MAX) + 1), Some(42)),
            (Some(7), Some(i64::from(i32::MAX) + 1)),
        ] {
            assert_eq!(
                ObserverUpdate::new(1, window, pid, "click", 12).target,
                None
            );
        }
    }

    #[test]
    fn label_size_is_bounded_without_cutting_utf8_or_preserving_controls() {
        let update = ObserverUpdate::new(1, Some(7), Some(42), &"🎾\n".repeat(10_000), 12);
        assert_eq!(update.action_label, "🎾".repeat(256));
        assert!(serde_json::to_vec(&update).unwrap().len() < MAX_UPDATE_BYTES);
    }

    #[test]
    fn reads_exact_target_and_clear_then_eof() {
        let mut input = Cursor::new(concat!(
            "{\"version\":1,\"generation\":1,\"target\":{\"pid\":42,\"window_id\":7},\"action_label\":\"click\",\"timestamp_ms\":12}\n",
            "{\"version\":1,\"generation\":2,\"target\":null,\"action_label\":\"end_session\",\"timestamp_ms\":13}\n"
        ));
        let first = read_update(&mut input).unwrap().unwrap();
        assert_eq!(
            first.target,
            Some(WindowTarget {
                pid: 42,
                window_id: 7
            })
        );
        assert_eq!(first.action_label, "click");
        assert_eq!(first.timestamp_ms, 12);
        let clear = read_update(&mut input).unwrap().unwrap();
        assert_eq!(clear.generation, 2);
        assert_eq!(clear.target, None);
        assert!(read_update(&mut input).unwrap().is_none());
    }

    #[test]
    fn refuses_invalid_scope_version_and_partial_messages() {
        for bytes in [
            br#"{"version":2,"generation":1,"target":null,"action_label":"click","timestamp_ms":12}
"#.as_slice(),
            br#"{"version":1,"generation":0,"target":null,"action_label":"click","timestamp_ms":12}
"#.as_slice(),
            br#"{"version":1,"generation":1,"target":{"pid":0,"window_id":7},"action_label":"click","timestamp_ms":12}
"#.as_slice(),
            br#"{"version":1,"generation":1,"target":{"pid":42,"window_id":0},"action_label":"click","timestamp_ms":12}
"#.as_slice(),
            br#"{"version":1,"generation":1,"target":null,"action_label":"click","timestamp_ms":12,"desktop":true}
"#.as_slice(),
            br#"{"version":1,"generation":1,"target":null,"action_label":"click","timestamp_ms":12}"#.as_slice(),
        ] {
            assert!(read_update(&mut Cursor::new(bytes)).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn refuses_oversized_input_before_consuming_the_entire_line() {
        let mut input = Cursor::new(vec![b' '; MAX_UPDATE_BYTES * 16]);
        assert!(read_update(&mut input).is_err());
        assert!(input.position() <= MAX_UPDATE_BYTES as u64 + 1);
    }

    #[test]
    fn stale_or_rebound_same_generation_updates_are_rejected() {
        let current = ObserverUpdate::new(4, Some(7), Some(42), "click", 12);
        assert!(!ObserverUpdate::new(3, Some(7), Some(42), "click", 12).follows(&current));
        assert!(!ObserverUpdate::new(4, Some(8), Some(42), "click", 12).follows(&current));
        assert!(!ObserverUpdate::new(4, None, None, "clear", 12).follows(&current));
        assert!(ObserverUpdate::new(4, Some(7), Some(42), "scroll", 13).follows(&current));
        assert!(ObserverUpdate::new(5, None, None, "clear", 13).follows(&current));
    }
}
