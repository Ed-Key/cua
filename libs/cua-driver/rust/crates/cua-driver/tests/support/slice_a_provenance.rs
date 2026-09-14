//! Test-local provenance admission. No generated native evidence.
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub fn validate_disabled_coverage(
    crop: [f64; 4],
    scale: f64,
    display: [f64; 4],
    display_scale: f64,
) -> Result<(), String> {
    if crop != display || scale != display_scale || !scale.is_finite() || scale <= 0.0 {
        return Err(
            "disabled evidence must cover the complete selected display at its native scale".into(),
        );
    }
    Ok(())
}
pub fn validate_record_order(log: &str, records: [&str; 3]) -> Result<(), String> {
    let offsets = records.map(|record| log.find(record).unwrap());
    if !(offsets[0] < offsets[1] && offsets[1] < offsets[2]) {
        return Err("approach records must occur in registered/ack_received/released order".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}
impl FileIdentity {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRecord {
    pub daemon_pid: i32,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub socket: PathBuf,
    pub command: Vec<String>,
    pub launched_epoch_ms: f64,
    pub process_started: String,
    pub environment: std::collections::BTreeMap<String, String>,
    pub stderr: PathBuf,
    pub stderr_identity: FileIdentity,
    pub transcript: super::Artifact,
}
pub fn parse_launch(bytes: &[u8]) -> Result<LaunchRecord, String> {
    let record: LaunchRecord = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if record.daemon_pid <= 0
        || !super::hex_hash(&record.executable_sha256)
        || !record.executable.is_absolute()
        || !record.socket.is_absolute()
        || !record.stderr.is_absolute()
        || record.command.is_empty()
        || record.command.iter().any(|arg| arg.is_empty())
        || record.process_started.trim().is_empty()
        || !record.launched_epoch_ms.is_finite()
        || record.launched_epoch_ms <= 0.
        || !super::hex_hash(&record.transcript.sha256)
        || record.transcript.path.is_empty()
    {
        return Err("incomplete launch provenance".into());
    }
    Ok(record)
}
pub fn validate_launch(
    record: &LaunchRecord,
    pid: i32,
    executable: &Path,
    hash: &str,
    socket: &Path,
    stderr: &Path,
    identity: &FileIdentity,
    timing: bool,
) -> Result<(), String> {
    if record.daemon_pid != pid
        || record.executable != executable
        || record.executable_sha256 != hash
        || record.socket != socket
        || record.stderr != stderr
        || &record.stderr_identity != identity
    {
        return Err(
            "launch PID/executable/hash/socket/stderr identity disagrees with running candidate"
                .into(),
        );
    }
    if timing
        && (record.environment.get("CUA_LOG").map(String::as_str)
            != Some("cua_cursor_approach=debug")
            || record
                .environment
                .get("CUA_PRIVATE_CURSOR_ORDER_TRACE")
                .is_some_and(|v| v == "1"))
    {
        return Err(
            "timing needs retained approach stderr and ordering diagnostics disabled".into(),
        );
    }
    Ok(())
}

/// Retain the same descriptor across RPC and read. A path that rotates to a
/// different inode is not the original daemon stream even if it grows longer.
pub struct LogSlice {
    file: std::fs::File,
    path: PathBuf,
    pub identity: FileIdentity,
    pub start: u64,
}
impl LogSlice {
    pub fn open(path: &Path, expected: &FileIdentity) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        let identity = FileIdentity::of(&metadata);
        if !metadata.is_file() || &identity != expected {
            return Err("approach file differs from launch stderr".into());
        }
        Ok(Self {
            file,
            path: path.into(),
            identity,
            start: metadata.len(),
        })
    }
    pub fn finish(mut self) -> Result<(String, u64), String> {
        use std::io::{Read, Seek, SeekFrom};
        let check = |file: &std::fs::File| -> Result<u64, String> {
            let fd = file.metadata().map_err(|e| e.to_string())?;
            let path = std::fs::metadata(&self.path).map_err(|e| e.to_string())?;
            if FileIdentity::of(&fd) != self.identity
                || FileIdentity::of(&path) != self.identity
                || fd.len() < self.start
            {
                return Err("approach stderr rotated or truncated during RPC/read".into());
            }
            Ok(fd.len())
        };
        let end = check(&self.file)?;
        if end - self.start > 1_048_576 {
            return Err("approach log slice exceeds 1 MiB; inspect original stderr".into());
        }
        self.file
            .seek(SeekFrom::Start(self.start))
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        (&mut self.file)
            .take(end - self.start)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if check(&self.file)? < end || bytes.len() as u64 != end - self.start {
            return Err("approach stderr changed during read".into());
        }
        Ok((String::from_utf8(bytes).map_err(|e| e.to_string())?, end))
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RpcBracket {
    pub started_epoch_ms: f64,
    pub returned_epoch_ms: f64,
    pub elapsed_ms: f64,
}
/// Same-host wall clocks with a conservative 5 ms bound for read placement and
/// log formatting. This is an admission tolerance, not claimed clock precision.
pub const RPC_CLOCK_TOLERANCE_MS: f64 = 5.;
pub fn timestamp_ms(line: &str) -> Result<f64, String> {
    let timestamp = line
        .split_whitespace()
        .next()
        .ok_or("missing log timestamp")?;
    let t = time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
        .map_err(|e| e.to_string())?;
    Ok(t.unix_timestamp_nanos() as f64 / 1_000_000.)
}
pub fn validate_rpc_records(records: [&str; 3], bracket: RpcBracket) -> Result<[f64; 3], String> {
    let b = bracket;
    let tolerance = RPC_CLOCK_TOLERANCE_MS;
    if !b.started_epoch_ms.is_finite()
        || !b.returned_epoch_ms.is_finite()
        || !b.elapsed_ms.is_finite()
        || b.started_epoch_ms <= 0.
        || b.elapsed_ms <= 0.
        || b.returned_epoch_ms < b.started_epoch_ms
        || (b.returned_epoch_ms - b.started_epoch_ms - b.elapsed_ms).abs() > tolerance
    {
        return Err("RPC wall and monotonic brackets disagree".into());
    }
    let stamps = [
        timestamp_ms(records[0])?,
        timestamp_ms(records[1])?,
        timestamp_ms(records[2])?,
    ];
    if !(stamps[0] <= stamps[1] && stamps[1] <= stamps[2])
        || stamps
            .iter()
            .any(|t| *t < b.started_epoch_ms - tolerance || *t > b.returned_epoch_ms + tolerance)
    {
        return Err("approach timestamps are reversed or outside actual RPC bracket".into());
    }
    Ok(stamps)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch() -> LaunchRecord {
        LaunchRecord {
            daemon_pid: 123,
            executable: "/candidate".into(),
            executable_sha256: "a".repeat(64),
            socket: "/socket".into(),
            command: vec!["/candidate".into(), "daemon".into()],
            launched_epoch_ms: 1000.,
            process_started: "synthetic test start".into(),
            environment: [("CUA_LOG".into(), "cua_cursor_approach=debug".into())].into(),
            stderr: "/stderr".into(),
            stderr_identity: FileIdentity {
                device: 1,
                inode: 2,
            },
            transcript: super::super::Artifact {
                path: "launch.txt".into(),
                sha256: "b".repeat(64),
            },
        }
    }
    #[test]
    fn correction_content_free_launch_is_rejected() {
        assert!(parse_launch(b"pending").is_err());
        assert!(parse_launch(b"{}").is_err());
    }
    #[test]
    fn correction_launch_identity_and_timing_environment_must_match() {
        let original = launch();
        for field in 0..8 {
            let mut bad = original.clone();
            match field {
                0 => bad.daemon_pid += 1,
                1 => bad.executable = "/other".into(),
                2 => bad.executable_sha256 = "c".repeat(64),
                3 => bad.socket = "/other".into(),
                4 => bad.stderr = "/other".into(),
                5 => bad.stderr_identity.inode += 1,
                6 => {
                    bad.environment.clear();
                }
                _ => {
                    bad.environment
                        .insert("CUA_PRIVATE_CURSOR_ORDER_TRACE".into(), "1".into());
                }
            }
            assert!(validate_launch(
                &bad,
                123,
                Path::new("/candidate"),
                &original.executable_sha256,
                Path::new("/socket"),
                Path::new("/stderr"),
                &original.stderr_identity,
                true
            )
            .is_err());
        }
        assert!(validate_launch(
            &original,
            123,
            Path::new("/candidate"),
            &original.executable_sha256,
            Path::new("/socket"),
            Path::new("/stderr"),
            &original.stderr_identity,
            true
        )
        .is_ok());
    }
    #[test]
    fn correction_log_rotation_is_rejected_even_when_replacement_is_longer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stderr");
        std::fs::write(&path, "before").unwrap();
        let identity = FileIdentity::of(&std::fs::metadata(&path).unwrap());
        let slice = LogSlice::open(&path, &identity).unwrap();
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        std::fs::write(&path, "a longer but unrelated replacement").unwrap();
        assert!(slice.finish().is_err());
        assert!(LogSlice::open(&path, &identity).is_err());
    }
    #[test]
    fn correction_retained_log_descriptor_reads_only_appended_bytes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stderr");
        std::fs::write(&path, "before").unwrap();
        let identity = FileIdentity::of(&std::fs::metadata(&path).unwrap());
        let slice = LogSlice::open(&path, &identity).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"after")
            .unwrap();
        assert_eq!(slice.finish().unwrap(), ("after".into(), 11));
    }
    #[test]
    fn correction_rpc_timestamp_bracket_rejects_unrelated_or_reversed_logs() {
        let stamps = [
            "2026-01-01T00:00:00.010Z registered",
            "2026-01-01T00:00:00.100Z ack",
            "2026-01-01T00:00:00.150Z released",
        ];
        let start = timestamp_ms("2026-01-01T00:00:00Z").unwrap();
        let bracket = RpcBracket {
            started_epoch_ms: start,
            returned_epoch_ms: start + 200.,
            elapsed_ms: 200.,
        };
        assert!(validate_rpc_records(stamps, bracket).is_ok());
        assert!(validate_rpc_records([stamps[2], stamps[1], stamps[0]], bracket).is_err());
        assert!(validate_rpc_records(
            stamps,
            RpcBracket {
                started_epoch_ms: start + 1000.,
                returned_epoch_ms: start + 1200.,
                ..bracket
            }
        )
        .is_err());
        assert!(validate_rpc_records(
            stamps,
            RpcBracket {
                elapsed_ms: 100.,
                ..bracket
            }
        )
        .is_err());
        assert!(validate_rpc_records(["no timestamp", stamps[1], stamps[2]], bracket).is_err());
    }
}
