//! Hidden-helper argument failures must exit before permissions or AppKit.
#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn malformed_observer_geometry_exits_without_starting_the_daemon() {
    for args in [
        vec!["__pip-observer"],
        vec!["__pip-observer", "0", "200", "auto", "auto"],
        vec!["__pip-observer", "320", "5000", "auto", "auto"],
        vec!["__pip-observer", "320", "200", "bad", "auto"],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_cua-driver"))
            .args(args)
            .env("CUA_DRIVER_RS_HOME", directory.path())
            .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "0")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("preview unavailable"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
