//! Versioned desired state for one floating preview per agent.
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
};

use serde::{Deserialize, Serialize};

use crate::observer::ObserverUpdate;

/// A byte budget, not an agent-count limit. Exceeding it disables observation
/// with an explicit error; it must never silently discard some agents.
pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverSnapshot {
    pub version: u8,
    pub revision: u64,
    #[serde(deserialize_with = "unique_previews")]
    pub previews: BTreeMap<u64, ObserverUpdate>,
}

fn unique_previews<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<u64, ObserverUpdate>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<u64, ObserverUpdate>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("distinct numeric preview IDs")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut previews = BTreeMap::new();
            while let Some((id, update)) = map.next_entry()? {
                if previews.insert(id, update).is_some() {
                    return Err(serde::de::Error::custom("duplicate preview ID"));
                }
            }
            Ok(previews)
        }
    }
    deserializer.deserialize_map(Visitor)
}

impl ObserverSnapshot {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 2, "unsupported session preview protocol");
        anyhow::ensure!(self.revision > 0, "invalid preview revision");
        for (id, update) in &self.previews {
            anyhow::ensure!(*id > 0, "invalid preview ID");
            update.validate()?;
        }
        Ok(())
    }
}

pub fn read_snapshot(reader: &mut impl BufRead) -> anyhow::Result<Option<ObserverSnapshot>> {
    let mut line = Vec::new();
    if reader
        .take(MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?
        == 0
    {
        return Ok(None);
    }
    anyhow::ensure!(
        line.len() <= MAX_SNAPSHOT_BYTES,
        "preview session snapshot exceeds byte budget"
    );
    anyhow::ensure!(line.last() == Some(&b'\n'), "incomplete preview snapshot");
    let snapshot: ObserverSnapshot = serde_json::from_slice(&line)?;
    snapshot.validate()?;
    Ok(Some(snapshot))
}

/// Called on the publisher worker only. Serialize and check the entire snapshot
/// before writing so an over-budget update cannot select a partial agent set.
pub fn write_snapshot(writer: &mut impl Write, snapshot: &ObserverSnapshot) -> anyhow::Result<()> {
    snapshot.validate()?;
    let mut bytes = serde_json::to_vec(snapshot)?;
    bytes.push(b'\n');
    anyhow::ensure!(
        bytes.len() <= MAX_SNAPSHOT_BYTES,
        "preview session snapshot exceeds byte budget"
    );
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Tracks removals without retaining a tombstone for every ended agent.
#[derive(Default)]
pub struct SnapshotReceiver {
    previous: Option<ObserverSnapshot>,
    highest_id: u64,
}

impl SnapshotReceiver {
    pub fn accept(&mut self, snapshot: &ObserverSnapshot) -> anyhow::Result<()> {
        snapshot.validate()?;
        if let Some(previous) = &self.previous {
            anyhow::ensure!(
                snapshot.revision > previous.revision,
                "stale preview snapshot"
            );
        }
        for (id, update) in &snapshot.previews {
            if let Some(old) = self.previous.as_ref().and_then(|p| p.previews.get(id)) {
                anyhow::ensure!(update.follows(old), "stale or rebound preview target");
            } else {
                anyhow::ensure!(*id > self.highest_id, "reused preview ID");
            }
        }
        self.highest_id = self
            .highest_id
            .max(snapshot.previews.keys().next_back().copied().unwrap_or(0));
        self.previous = Some(snapshot.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_two_independent_previews_then_owner_removal() {
        // The old single-window codec cannot represent both agents or remove
        // just one. Literal wire input also checks the actual JSON boundary.
        let mut reader = Cursor::new(concat!(
            "{\"version\":2,\"revision\":1,\"previews\":{\"1\":{\"version\":1,\"generation\":3,\"target\":{\"pid\":42,\"window_id\":7},\"action_label\":\"click\",\"timestamp_ms\":12},\"2\":{\"version\":1,\"generation\":1,\"target\":{\"pid\":43,\"window_id\":8},\"action_label\":\"scroll\",\"timestamp_ms\":13}}}\n",
            "{\"version\":2,\"revision\":2,\"previews\":{\"2\":{\"version\":1,\"generation\":1,\"target\":{\"pid\":43,\"window_id\":8},\"action_label\":\"scroll\",\"timestamp_ms\":13}}}\n",
            "{\"version\":2,\"revision\":3,\"previews\":{}}\n"
        ));
        let first = read_snapshot(&mut reader).unwrap().unwrap();
        assert_eq!(first.previews.len(), 2);
        assert_eq!(first.previews[&1].target.unwrap().window_id, 7);
        assert_eq!(first.previews[&2].target.unwrap().window_id, 8);
        let second = read_snapshot(&mut reader).unwrap().unwrap();
        assert_eq!(second.previews.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert_eq!(second.previews[&2].generation, 1);
        assert!(read_snapshot(&mut reader)
            .unwrap()
            .unwrap()
            .previews
            .is_empty());
        assert!(read_snapshot(&mut reader).unwrap().is_none());
    }

    #[test]
    fn malformed_snapshot_cannot_select_unvalidated_windows() {
        let valid = serde_json::json!({"version":2,"revision":1,"previews":{
            "1":{"version":1,"generation":1,"target":{"pid":42,"window_id":7},
                 "action_label":"click","timestamp_ms":12}
        }});
        for pointer in [
            "/version",
            "/revision",
            "/previews/1/generation",
            "/previews/1/target/pid",
            "/previews/1/target/window_id",
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = serde_json::json!(0);
            let bytes = format!("{invalid}\n");
            assert!(
                read_snapshot(&mut Cursor::new(bytes)).is_err(),
                "accepted {pointer}"
            );
        }
        for key in ["0", "-1", "private-session-id"] {
            let mut invalid = valid.clone();
            let entry = invalid["previews"]
                .as_object_mut()
                .unwrap()
                .remove("1")
                .unwrap();
            invalid["previews"][key] = entry;
            assert!(read_snapshot(&mut Cursor::new(format!("{invalid}\n"))).is_err());
        }
        let mut invalid = valid.clone();
        invalid["desktop"] = true.into();
        assert!(read_snapshot(&mut Cursor::new(format!("{invalid}\n"))).is_err());
        assert!(read_snapshot(&mut Cursor::new(valid.to_string())).is_err());
        let mut oversized = Cursor::new(vec![b' '; 1_048_576]);
        assert!(read_snapshot(&mut oversized).is_err());
        assert!(oversized.position() < 1_048_576);
    }

    fn snapshot(revision: u64, entries: &[(u64, u64, u64)]) -> ObserverSnapshot {
        ObserverSnapshot {
            version: 2,
            revision,
            previews: entries
                .iter()
                .map(|&(id, generation, window)| {
                    (
                        id,
                        ObserverUpdate::new(generation, Some(window), Some(42), "click", 12),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn snapshots_write_both_agents_and_fail_before_partial_oversize_delivery() {
        let mut bytes = Vec::new();
        write_snapshot(&mut bytes, &snapshot(1, &[(1, 3, 7), (2, 1, 8)])).unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire["version"], 2);
        assert_eq!(wire["previews"]["1"]["target"]["window_id"], 7);
        assert_eq!(wire["previews"]["2"]["target"]["window_id"], 8);
        assert_eq!(bytes.last(), Some(&b'\n'));
        let huge = snapshot(2, &(1..=10_000).map(|id| (id, 1, 7)).collect::<Vec<_>>());
        bytes.clear();
        assert!(write_snapshot(&mut bytes, &huge).is_err());
        assert!(bytes.is_empty(), "partial snapshot reached the helper");
    }

    #[test]
    fn stale_state_cannot_retarget_or_resurrect_an_ended_preview() {
        let mut receiver = SnapshotReceiver::default();
        receiver
            .accept(&snapshot(1, &[(1, 3, 7), (2, 1, 8)]))
            .unwrap();
        assert!(receiver
            .accept(&snapshot(1, &[(1, 3, 7), (2, 1, 8)]))
            .is_err());
        assert!(receiver
            .accept(&snapshot(2, &[(1, 2, 7), (2, 1, 8)]))
            .is_err());
        assert!(receiver
            .accept(&snapshot(2, &[(1, 3, 9), (2, 1, 8)]))
            .is_err());
        receiver.accept(&snapshot(2, &[(2, 1, 8)])).unwrap();
        assert!(receiver
            .accept(&snapshot(3, &[(1, 4, 7), (2, 1, 8)]))
            .is_err());
        receiver
            .accept(&snapshot(3, &[(2, 2, 9), (3, 1, 7)]))
            .unwrap();
        receiver.accept(&snapshot(4, &[])).unwrap();
        assert!(receiver.accept(&snapshot(5, &[(2, 3, 9)])).is_err());
        receiver.accept(&snapshot(5, &[(4, 1, 9)])).unwrap();
    }

    #[test]
    fn duplicate_numeric_ids_cannot_silently_discard_a_preview() {
        let entry = r#"{"version":1,"generation":1,"target":{"pid":42,"window_id":7},"action_label":"click","timestamp_ms":12}"#;
        for second in ["1", "01"] {
            let wire = format!("{{\"version\":2,\"revision\":1,\"previews\":{{\"1\":{entry},\"{second}\":{entry}}}}}\n");
            assert!(
                read_snapshot(&mut Cursor::new(wire)).is_err(),
                "duplicate ID {second} was silently merged"
            );
        }
    }
}
