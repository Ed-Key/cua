//! Link to the Cua Driver Chrome extension.
//!
//! The extension runs inside the user's own Chrome profile, so it reaches the
//! logged-in profile that Chrome 136+ no longer exposes on a remote-debugging
//! port. When it connects, Chrome starts the native messaging host (this
//! executable in host mode), which pipes Chrome's frames to the daemon over a
//! Unix socket served here. The frames are Chrome's: a 4-byte native-endian
//! length, then UTF-8 JSON. Messages are JSON-RPC 2.0: the daemon sends
//! requests, and the extension answers them and sends notifications (`hello`,
//! `debugger.event`, `debugger.detached`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Chrome caps a message to a native host at 64 MiB.
const MAX_FRAME: usize = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A notification the extension sent without being asked.
#[derive(Clone, Debug)]
pub struct ExtensionEvent {
    pub link: u64,
    pub method: String,
    pub params: Value,
}

/// One connected extension instance (one Chrome profile).
#[derive(Clone, Debug)]
pub struct LinkInfo {
    pub link: u64,
    /// The extension's `hello` parameters (id, version, user agent).
    pub hello: Option<Value>,
    /// The Chrome browser process this link belongs to, proven by the OS:
    /// the socket peer is this driver's own executable and its parent is
    /// Chrome (Chrome launches native hosts). `None` where the platform
    /// cannot prove it.
    pub chrome_pid: Option<i64>,
}

type Reply = Result<Value, String>;

struct Link {
    id: u64,
    hello: Mutex<Option<Value>>,
    chrome_pid: Mutex<Option<i64>>,
    outbox: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl Link {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the senders wakes every waiter with a closed-link error.
        self.pending.lock().unwrap().clear();
    }
}

pub struct ExtensionBridge {
    links: Mutex<Vec<Arc<Link>>>,
    next_link: AtomicU64,
    events: broadcast::Sender<ExtensionEvent>,
}

/// The process-wide bridge. The daemon serves it; tools read from it.
pub fn global() -> &'static Arc<ExtensionBridge> {
    static BRIDGE: OnceLock<Arc<ExtensionBridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| {
        Arc::new(ExtensionBridge {
            links: Mutex::new(Vec::new()),
            next_link: AtomicU64::new(1),
            events: broadcast::channel(1024).0,
        })
    })
}

/// Encode one message in Chrome's native-messaging framing.
pub fn frame(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).expect("JSON values always serialize");
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    framed.extend_from_slice(&body);
    framed
}

impl ExtensionBridge {
    /// Connected extension instances, oldest first.
    pub fn links(&self) -> Vec<LinkInfo> {
        self.links
            .lock()
            .unwrap()
            .iter()
            .filter(|link| !link.closed.load(Ordering::SeqCst))
            .map(|link| LinkInfo {
                link: link.id,
                hello: link.hello.lock().unwrap().clone(),
                chrome_pid: *link.chrome_pid.lock().unwrap(),
            })
            .collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionEvent> {
        self.events.subscribe()
    }

    /// Send one request to the most recently connected extension and wait for
    /// its answer. An error string from the extension becomes an `Err`.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.send(None, method, params).await
    }

    /// Like [`Self::request`], but to one exact link (one Chrome instance).
    pub async fn request_on(&self, link: u64, method: &str, params: Value) -> anyhow::Result<Value> {
        self.send(Some(link), method, params).await
    }

    async fn send(&self, link: Option<u64>, method: &str, params: Value) -> anyhow::Result<Value> {
        let link = self
            .links
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|candidate| {
                !candidate.closed.load(Ordering::SeqCst) && link.is_none_or(|id| candidate.id == id)
            })
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the Cua Driver Chrome extension is not connected: install it in Chrome \
                     and keep Chrome open"
                )
            })?;
        let id = link.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply_tx, reply_rx) = oneshot::channel();
        link.pending.lock().unwrap().insert(id, reply_tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if link.outbox.send(frame(&message)).is_err() {
            link.pending.lock().unwrap().remove(&id);
            anyhow::bail!("the Chrome extension link closed");
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(anyhow::anyhow!("Chrome extension: {message}")),
            Ok(Err(_)) => Err(anyhow::anyhow!("the Chrome extension link closed")),
            Err(_) => {
                link.pending.lock().unwrap().remove(&id);
                Err(anyhow::anyhow!("the Chrome extension did not answer {method} in time"))
            }
        }
    }

    fn dispatch(&self, link: &Link, message: Value) {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let waiter = link.pending.lock().unwrap().remove(&id);
            if let Some(waiter) = waiter {
                let reply = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(reply);
            }
            return;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if method == "hello" {
            *link.hello.lock().unwrap() = Some(params.clone());
        }
        let _ = self.events.send(ExtensionEvent {
            link: link.id,
            method: method.to_owned(),
            params,
        });
    }
}

#[cfg(unix)]
impl ExtensionBridge {
    /// Accept native-host connections on `path` until the listener fails.
    /// The socket is owner-only; a stale file from an earlier run is replaced.
    pub async fn serve(self: Arc<Self>, path: String) -> anyhow::Result<()> {
        let path = path.as_str();
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)
            .map_err(|error| anyhow::anyhow!("bind {path}: {error}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(self.clone().run_link(stream));
        }
    }

    async fn run_link(self: Arc<Self>, stream: tokio::net::UnixStream) {
        // Identity comes from the OS, never from what the peer says: another
        // process could claim any Chrome and answer for it.
        let chrome_pid = match peer_chrome_pid(&stream) {
            Ok(pid) => pid,
            Err(reason) => {
                tracing::warn!("rejected a Chrome extension bridge connection: {reason}");
                return;
            }
        };
        let (mut reader, mut writer) = stream.into_split();
        let (outbox, mut frames) = mpsc::unbounded_channel::<Vec<u8>>();
        let link = Arc::new(Link {
            id: self.next_link.fetch_add(1, Ordering::SeqCst),
            hello: Mutex::new(None),
            chrome_pid: Mutex::new(chrome_pid),
            outbox,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        self.links.lock().unwrap().push(link.clone());
        let writer_task = tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                if writer.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        loop {
            let mut length = [0u8; 4];
            if reader.read_exact(&mut length).await.is_err() {
                break;
            }
            let length = u32::from_ne_bytes(length) as usize;
            if length > MAX_FRAME {
                break;
            }
            let mut body = vec![0u8; length];
            if reader.read_exact(&mut body).await.is_err() {
                break;
            }
            if let Ok(message) = serde_json::from_slice::<Value>(&body) {
                self.dispatch(&link, message);
            }
        }
        link.close();
        writer_task.abort();
        self.links
            .lock()
            .unwrap()
            .retain(|other| !Arc::ptr_eq(other, &link));
    }
}

/// The Chrome process behind a bridge connection. The peer must run as this
/// user and be this driver's own executable in native-host mode, and Chrome is
/// its parent. A process that spawns the driver itself becomes the parent, so
/// it can never pose as a Chrome it is not.
#[cfg(target_os = "macos")]
fn peer_chrome_pid(stream: &tokio::net::UnixStream) -> Result<Option<i64>, String> {
    let credentials = stream.peer_cred().map_err(|error| error.to_string())?;
    if credentials.uid() != unsafe { libc::geteuid() } {
        return Err("the peer belongs to another user".to_owned());
    }
    let pid = credentials.pid().ok_or("the peer pid is unavailable")?;
    let executable = pid_path(pid).ok_or("the peer executable is unavailable")?;
    let ours = std::env::current_exe().map_err(|error| error.to_string())?;
    let canonical = |path: &std::path::Path| std::fs::canonicalize(path).ok();
    if canonical(&executable).is_none() || canonical(&executable) != canonical(&ours) {
        return Err(format!("the peer {} is not this driver", executable.display()));
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
    };
    if read != size {
        return Err("the peer's parent is unavailable".to_owned());
    }
    Ok(Some(i64::from(info.pbi_ppid)))
}

#[cfg(target_os = "macos")]
fn pid_path(pid: libc::pid_t) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

/// Other platforms cannot prove the Chrome process yet: the link serves tab
/// requests but never binds the page engine to a Chrome.
#[cfg(all(unix, not(target_os = "macos")))]
fn peer_chrome_pid(_stream: &tokio::net::UnixStream) -> Result<Option<i64>, String> {
    Ok(None)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn bridge() -> Arc<ExtensionBridge> {
        Arc::new(ExtensionBridge {
            links: Mutex::new(Vec::new()),
            next_link: AtomicU64::new(1),
            events: broadcast::channel(16).0,
        })
    }

    /// A fake extension connected to a fresh bridge, after its hello.
    pub(crate) async fn connected() -> (Arc<ExtensionBridge>, tokio::net::UnixStream, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.sock");
        let bridge = bridge();
        tokio::spawn(bridge.clone().serve(path.to_str().unwrap().to_owned()));
        let mut extension = loop {
            match tokio::net::UnixStream::connect(&path).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        extension
            .write_all(&frame(&json!({"jsonrpc":"2.0","method":"hello","params":{"version":"0.1.0"}})))
            .await
            .unwrap();
        while bridge.links().first().and_then(|link| link.hello.clone()).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (bridge, extension, dir)
    }

    pub(crate) async fn read_frame(stream: &mut tokio::net::UnixStream) -> Value {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).await.unwrap();
        let mut body = vec![0u8; u32::from_ne_bytes(length) as usize];
        stream.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn a_request_round_trips_through_a_connected_extension() {
        let (bridge, mut extension, _dir) = connected().await;

        let pending = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.request("tabs.list", json!({})).await }
        });
        let request = read_frame(&mut extension).await;
        assert_eq!(request["method"], "tabs.list");
        extension
            .write_all(&frame(&json!({"jsonrpc":"2.0","id":request["id"],"result":[{"tabId":7}]})))
            .await
            .unwrap();
        assert_eq!(pending.await.unwrap().unwrap(), json!([{"tabId":7}]));

        let failing = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.request("tabs.remove", json!({"tabIds":[1]})).await }
        });
        let request = read_frame(&mut extension).await;
        extension
            .write_all(&frame(&json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"No tab with id: 1."}})))
            .await
            .unwrap();
        let error = failing.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("No tab with id: 1."), "{error}");

        drop(extension);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !bridge.links().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "closed link still listed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let error = bridge.request("ping", json!({})).await.unwrap_err().to_string();
        assert!(error.contains("not connected"), "{error}");
    }
}
