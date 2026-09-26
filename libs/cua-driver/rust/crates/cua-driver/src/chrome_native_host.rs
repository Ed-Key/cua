//! Chrome native messaging for the Cua Driver extension.
//!
//! Chrome starts this executable when the extension connects, with the
//! extension's origin as the first argument. In that mode it is a byte pipe:
//! Chrome's framed messages on stdin and stdout, the daemon's extension bridge
//! (`cua_driver_core::browser::extension_bridge`) on a Unix socket, with
//! identical framing on both sides. It exits when either side closes; the
//! extension reconnects later.
//!
//! `cua-driver chrome-extension install` registers this executable with
//! Chrome as the host for the extension.

/// The extension id, fixed by the public key in its manifest.
pub const EXTENSION_ID: &str = "pojfnghfciahibpbglblhhnhecmplejm";
/// The native messaging host name the extension connects to.
pub const HOST_NAME: &str = "com.trycua.cua_driver";

/// The bridge socket beside the daemon socket at `daemon_socket`.
pub fn bridge_socket_path_for(daemon_socket: &str) -> String {
    std::path::Path::new(daemon_socket)
        .with_file_name("chrome-bridge.sock")
        .to_string_lossy()
        .into_owned()
}

/// Handle native-host mode or the `chrome-extension` command when this
/// process was started for one of them. Returns the exit code if it was.
pub fn run_if_requested() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(origin) = args.get(1).filter(|arg| arg.starts_with("chrome-extension://")) {
        return Some(run_host(origin));
    }
    if args.get(1).map(String::as_str) == Some("chrome-extension") {
        return Some(run_cli(&args[2..]));
    }
    None
}

#[cfg(unix)]
fn run_host(origin: &str) -> i32 {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    // Chrome enforces allowed_origins; refuse anything else regardless.
    if origin.trim_end_matches('/') != format!("chrome-extension://{EXTENSION_ID}") {
        eprintln!("cua-driver native host: refusing origin {origin}");
        return 1;
    }
    let path = bridge_socket_path_for(&crate::serve::default_socket_path());
    let Ok(socket) = UnixStream::connect(&path) else {
        // No daemon: exit so Chrome closes the port; the extension retries.
        eprintln!("cua-driver native host: the daemon is not running ({path})");
        return 1;
    };
    let Ok(mut to_daemon) = socket.try_clone() else {
        return 1;
    };
    // Tell the daemon which Chrome this link belongs to: Chrome launched this
    // host, so the parent process is the browser.
    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "host.hello",
        "params": { "chromePid": std::os::unix::process::parent_id() },
    });
    let frame = cua_driver_core::browser::extension_bridge::frame(&hello);
    if to_daemon.write_all(&frame).is_err() {
        return 1;
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_daemon);
        let _ = to_daemon.shutdown(std::net::Shutdown::Both);
    });
    // Stdout is line buffered and frames carry no newlines, so flush each
    // read; Chrome must see every frame as soon as the daemon sends it.
    let mut from_daemon = socket;
    let mut stdout = std::io::stdout().lock();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match from_daemon.read(&mut buffer) {
            Ok(0) | Err(_) => return 0,
            Ok(read) => {
                if stdout.write_all(&buffer[..read]).is_err() || stdout.flush().is_err() {
                    return 0;
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn run_host(_origin: &str) -> i32 {
    eprintln!("cua-driver native host: not supported on this platform yet");
    1
}

/// Chrome-family browsers that read native messaging hosts from the user's
/// Application Support folder on macOS.
#[cfg(target_os = "macos")]
const BROWSER_DIRS: [&str; 3] = [
    "Google/Chrome",
    "Google/Chrome Beta",
    "Google/Chrome Canary",
];

fn run_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("install") => match install() {
            Ok(paths) => {
                for path in paths {
                    println!("registered native host: {}", path.display());
                }
                println!(
                    "Now load the extension in Chrome: chrome://extensions, turn on Developer \
                     mode, Load unpacked, and pick the extensions/chrome folder. Its id must be \
                     {EXTENSION_ID}."
                );
                0
            }
            Err(error) => {
                eprintln!("chrome-extension install: {error}");
                1
            }
        },
        _ => {
            eprintln!("usage: cua-driver chrome-extension install");
            64
        }
    }
}

#[cfg(target_os = "macos")]
fn install() -> anyhow::Result<Vec<std::path::PathBuf>> {
    let executable = std::env::current_exe()?;
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "Cua Driver",
        "path": executable,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    let home = std::env::var("HOME")?;
    let mut written = Vec::new();
    for browser in BROWSER_DIRS {
        let browser_dir =
            std::path::Path::new(&home).join("Library/Application Support").join(browser);
        // Register only with browsers that are installed for this user.
        if !browser_dir.is_dir() {
            continue;
        }
        let hosts = browser_dir.join("NativeMessagingHosts");
        std::fs::create_dir_all(&hosts)?;
        let path = hosts.join(format!("{HOST_NAME}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
        written.push(path);
    }
    anyhow::ensure!(!written.is_empty(), "no Chrome profile folder found for this user");
    Ok(written)
}

#[cfg(not(target_os = "macos"))]
fn install() -> anyhow::Result<Vec<std::path::PathBuf>> {
    anyhow::bail!("chrome-extension install is macOS-only for now")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_socket_sits_beside_the_daemon_socket() {
        assert_eq!(
            bridge_socket_path_for("/Users/me/Library/Caches/cua-driver-local/cua-driver-local.sock"),
            "/Users/me/Library/Caches/cua-driver-local/chrome-bridge.sock"
        );
    }
}
