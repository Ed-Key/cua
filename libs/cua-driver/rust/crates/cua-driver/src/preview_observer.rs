//! Same-executable preview companion. Input is metadata, never screenshot bytes.
//! The caller installs `publish` on the dedicated core preview worker so pipe
//! backpressure cannot reach an action task.

use std::process::{Child, ChildStdin, Command, Stdio};

use cua_driver_core::pip_hook::PipCaptureRequest;
use pip_preview::observer::{write_update, ObserverUpdate, WindowTarget};

pub struct ObserverProcess {
    child: Child,
    input: ChildStdin,
    generation: u64,
    target: Option<WindowTarget>,
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
            generation: 0,
            target: None,
        })
    }

    pub fn publish(&mut self, request: PipCaptureRequest) -> bool {
        let mut update = ObserverUpdate::new(
            self.generation,
            request.window_id,
            request.pid,
            &request.action_label,
            request.timestamp_ms,
        );
        if self.generation == 0 || self.target != update.target {
            let Some(generation) = self.generation.checked_add(1) else {
                return false;
            };
            self.generation = generation;
            self.target = update.target;
        }
        update.generation = self.generation;
        write_update(&mut self.input, &update).is_ok()
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
    fn child_receives_metadata_and_closed_pipe_disables_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("received.json");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "IFS= read -r line; printf '%s\\n' \"$line\" > \"$1\"",
                "preview-test",
            ])
            .arg(&path);
        let mut observer = ObserverProcess::spawn_command(&mut command).unwrap();
        assert!(observer.publish(request(Some(7))));
        wait_for_exit(&mut observer.child);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version":1,"generation":1,"target":{"pid":42,"window_id":7},
                "action_label":"click","timestamp_ms":12,
            })
        );
        assert!(!observer.publish(request(Some(8))));
    }

    #[test]
    fn only_target_changes_advance_capture_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec /usr/bin/head -n 4 > \"$1\"", "preview-test"])
            .arg(&path);
        let mut observer = ObserverProcess::spawn_command(&mut command).unwrap();
        for target in [Some(7), Some(7), None, Some(7)] {
            assert!(observer.publish(request(target)));
        }
        wait_for_exit(&mut observer.child);
        let bytes = std::fs::read(path).unwrap();
        let mut reader = std::io::Cursor::new(bytes);
        let updates: Vec<_> = (0..4)
            .map(|_| {
                pip_preview::observer::read_update(&mut reader)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            updates
                .iter()
                .map(|update| update.generation)
                .collect::<Vec<_>>(),
            vec![1, 1, 2, 3]
        );
        assert_eq!(updates[2].target, None);
        assert_eq!(
            updates[3].target,
            Some(WindowTarget {
                pid: 42,
                window_id: 7
            })
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
                if !observer.publish(request(Some(7))) {
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
