//! Same-executable preview companion. Input is metadata, never screenshot bytes.
//! The caller installs `publish` on the dedicated core preview worker so pipe
//! backpressure cannot reach an action task.

use std::collections::BTreeMap;
use std::process::{Child, ChildStdin, Command, Stdio};

#[cfg(test)]
use cua_driver_core::pip_hook::PipCaptureRequest;
use cua_driver_core::pip_hook::PipSessionSnapshot;
use pip_preview::observer::{ObserverUpdate, ObserverVisual};
use pip_preview::session_observer::{write_snapshot, ObserverSnapshot};

pub struct ObserverProcess {
    child: Child,
    input: ChildStdin,
    sessions: BTreeMap<String, (u64, ObserverUpdate)>,
    next_preview_id: u64,
    revision: u64,
}

impl ObserverProcess {
    pub fn spawn(cfg: &pip_preview::PipConfig) -> anyhow::Result<Self> {
        let geometry = cfg.geometry;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("__pip-observer")
            .arg(geometry.width.to_string())
            .arg(geometry.height.to_string())
            .arg(geometry.x.map_or_else(|| "auto".into(), |x| x.to_string()))
            .arg(geometry.y.map_or_else(|| "auto".into(), |y| y.to_string()));
        Self::spawn_command(&mut command)
    }

    fn spawn_command(command: &mut Command) -> anyhow::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().expect("piped observer stdin");
        Ok(Self {
            child,
            input,
            sessions: BTreeMap::new(),
            next_preview_id: 0,
            revision: 0,
        })
    }

    pub fn publish_sessions(&mut self, snapshot: PipSessionSnapshot) -> bool {
        let result = self.send_sessions(snapshot);
        if let Err(error) = &result {
            tracing::warn!(%error, "PiP observer disabled; input remains available");
        }
        result.is_ok()
    }

    fn send_sessions(&mut self, snapshot: PipSessionSnapshot) -> anyhow::Result<()> {
        self.sessions
            .retain(|session, _| snapshot.contains_key(session));
        for (session, request) in snapshot {
            let mut update = ObserverUpdate::new(
                1,
                request.window_id,
                request.pid,
                &request.action_label,
                request.timestamp_ms,
            );
            update.visuals = request
                .visual
                .ordered()
                .into_iter()
                .filter_map(ObserverVisual::from_event)
                .collect();
            let id = if let Some((id, previous)) = self.sessions.get(&session) {
                update.generation = previous.generation;
                if previous.target != update.target {
                    update.generation = update
                        .generation
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("preview generation exhausted"))?;
                }
                *id
            } else {
                self.next_preview_id = self
                    .next_preview_id
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("preview IDs exhausted"))?;
                self.next_preview_id
            };
            self.sessions.insert(session, (id, update));
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("preview revision exhausted"))?;
        write_snapshot(
            &mut self.input,
            &ObserverSnapshot {
                version: 2,
                revision: self.revision,
                previews: self.sessions.values().cloned().collect(),
            },
        )
    }
}

/// Recognized before ordinary daemon/CLI startup. This path creates no MCP
/// endpoint, starts no permission flow, and runs only the observer UI.
pub fn run_if_requested() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("__pip-observer") {
        return None;
    }
    crate::init_logging();
    let result = parse_geometry(&args[1..]).and_then(|geometry| {
        platform_macos::pip::observer::run(pip_preview::PipConfig {
            enabled: true,
            geometry,
            ..Default::default()
        })
    });
    Some(match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cua-driver preview unavailable: {error}");
            1
        }
    })
}

fn parse_geometry(args: &[String]) -> anyhow::Result<pip_preview::PipGeometry> {
    anyhow::ensure!(args.len() == 4, "preview expects width height x y");
    let width: u32 = args[0].parse()?;
    let height: u32 = args[1].parse()?;
    anyhow::ensure!(
        (1..=4096).contains(&width) && (1..=4096).contains(&height),
        "invalid preview geometry"
    );
    let coordinate = |value: &str| -> anyhow::Result<Option<i32>> {
        if value == "auto" {
            Ok(None)
        } else {
            Ok(Some(value.parse()?))
        }
    };
    Ok(pip_preview::PipGeometry {
        width,
        height,
        x: coordinate(&args[2])?,
        y: coordinate(&args[3])?,
    })
}

impl Drop for ObserverProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn request(window_id: Option<u64>) -> PipCaptureRequest {
        PipCaptureRequest {
            visual: Default::default(),
            window_id,
            pid: Some(42),
            action_label: "click".into(),
            timestamp_ms: 12,
        }
    }

    fn wait_for_exit(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "observer did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn sessions_keep_independent_ids_and_generations_across_the_actual_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.jsonl");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec /usr/bin/head -n 4 > \"$1\"", "preview-test"])
            .arg(&path);
        let mut observer = ObserverProcess::spawn_command(&mut command).unwrap();
        let mut sessions = PipSessionSnapshot::from([
            ("private-a".into(), request(Some(7))),
            ("private-b".into(), request(Some(8))),
        ]);
        assert!(observer.publish_sessions(sessions.clone()));
        sessions.insert("private-a".into(), request(Some(9)));
        assert!(observer.publish_sessions(sessions.clone()));
        sessions.remove("private-a");
        assert!(observer.publish_sessions(sessions.clone()));
        sessions.insert("private-c".into(), request(Some(7)));
        assert!(observer.publish_sessions(sessions));
        wait_for_exit(&mut observer.child);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            !text.contains("private-"),
            "runtime session identity leaked to helper"
        );
        let updates: Vec<serde_json::Value> = text
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(updates[0]["previews"]["1"]["target"]["window_id"], 7);
        assert_eq!(updates[0]["previews"]["2"]["target"]["window_id"], 8);
        assert_eq!(updates[1]["previews"]["1"]["generation"], 2);
        assert_eq!(updates[1]["previews"]["2"]["generation"], 1);
        assert_eq!(
            updates[2]["previews"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["2"]
        );
        assert_eq!(updates[3]["previews"]["3"]["target"]["window_id"], 7);
        assert_eq!(
            updates
                .iter()
                .map(|s| s["revision"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert!(
            !observer.publish_sessions(PipSessionSnapshot::new()),
            "closed pipe remained available"
        );
    }

    #[test]
    fn dropping_observer_reaps_its_owned_process() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let observer = ObserverProcess::spawn_command(&mut command).unwrap();
        let pid = observer.child.id() as i32;
        drop(observer);
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == 0 {
            // Reap even the deliberately failing implementation's child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
        }
        assert_eq!(
            result, -1,
            "dropping preview left an owned child alive or unreaped"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn stalled_pipe_is_released_when_helper_is_killed() {
        // An actual pipe fills while the owned child deliberately never reads.
        // Core dispatcher tests separately prove publication remains independent
        // of this blocked callback. The helper is terminated within the test.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let mut observer = ObserverProcess::spawn_command(&mut command).unwrap();
        let pid = observer.child.id() as i32;
        let (entered, starting) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            entered.send(()).unwrap();
            let mut delivered = 0;
            for _ in 0..100_000 {
                if !observer.publish_sessions(PipSessionSnapshot::from([(
                    "stalled-agent".into(),
                    request(Some(7)),
                )])) {
                    break;
                }
                delivered += 1;
            }
            finished.send(delivered).unwrap();
        });
        starting.recv_timeout(Duration::from_secs(3)).unwrap();
        let stalled = done.recv_timeout(Duration::from_millis(50));
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let delivered = done.recv_timeout(Duration::from_secs(3)).unwrap();
        writer.join().unwrap();
        assert!(matches!(stalled, Err(mpsc::RecvTimeoutError::Timeout)));
        assert!(delivered < 100_000);
    }
}
