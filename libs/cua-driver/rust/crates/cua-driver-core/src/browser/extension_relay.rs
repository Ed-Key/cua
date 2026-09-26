//! A browser-level DevTools endpoint backed by the Cua Driver extension.
//!
//! The engine speaks CDP to a browser WebSocket: Target and Browser methods on
//! the root, everything else on a flattened tab session. The extension reaches
//! tabs only through `chrome.debugger`, which has no Target or Browser root.
//! This relay listens on a loopback WebSocket whose unguessable path is its
//! only secret, answers the root methods the engine uses from
//! `chrome.debugger` and `chrome.tabs`/`chrome.windows`, and forwards every
//! session command to its tab. The rest of the engine (binding, grants, the
//! connection pool, the existing-profile method allowlist) is unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use super::extension_bridge::{self, ExtensionBridge, ExtensionEvent};

/// CDP's "method not found": the engine treats it as an unsupported feature.
const METHOD_NOT_FOUND: i64 = -32601;

struct Relay {
    port: u16,
    /// WebSocket path token -> bridge link.
    paths: Mutex<HashMap<String, u64>>,
}

static RELAY: tokio::sync::OnceCell<Arc<Relay>> = tokio::sync::OnceCell::const_new();

async fn relay() -> anyhow::Result<Arc<Relay>> {
    RELAY.get_or_try_init(start).await.cloned()
}

async fn start() -> anyhow::Result<Arc<Relay>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let relay = Arc::new(Relay {
        port: listener.local_addr()?.port(),
        paths: Mutex::new(HashMap::new()),
    });
    let serving = relay.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serving.clone().accept(stream));
        }
    });
    Ok(relay)
}

/// The relay endpoint for the extension link that belongs to Chrome process
/// `pid`, or `None` when no connected extension reported that process.
pub async fn endpoint_for_pid(pid: i64) -> Option<String> {
    let link = extension_bridge::global()
        .links()
        .into_iter()
        .rev()
        .find(|link| link.chrome_pid == Some(pid))?;
    let relay = relay().await.ok()?;
    let mut paths = relay.paths.lock().unwrap();
    let token = paths
        .iter()
        .find(|(_, existing)| **existing == link.link)
        .map(|(token, _)| token.clone())
        .unwrap_or_else(|| {
            let token = uuid::Uuid::new_v4().to_string();
            paths.insert(token.clone(), link.link);
            token
        });
    Some(format!("ws://127.0.0.1:{}/devtools/browser/{token}", relay.port))
}

/// Whether `ws_url` is this relay's endpoint for a still-connected extension
/// link of Chrome process `pid`.
pub async fn is_live_endpoint(ws_url: &str, pid: i64) -> bool {
    endpoint_for_pid(pid).await.as_deref() == Some(ws_url)
}

/// Whether `ws_url` names this relay at all (live or not).
pub fn is_relay_url(ws_url: &str) -> bool {
    RELAY.get().is_some_and(|relay| {
        ws_url.starts_with(&format!("ws://127.0.0.1:{}/devtools/browser/", relay.port))
    })
}

impl Relay {
    async fn accept(self: Arc<Self>, stream: tokio::net::TcpStream) {
        use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
        let mut link = None;
        let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            let token = request.uri().path().strip_prefix("/devtools/browser/").unwrap_or("");
            link = self.paths.lock().unwrap().get(token).copied();
            if link.is_some() {
                Ok(response)
            } else {
                let mut refused = ErrorResponse::new(None);
                *refused.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
                Err(refused)
            }
        };
        let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
            return;
        };
        if let Some(link) = link {
            Session::new(link).run(socket).await;
        }
    }
}

/// Session bookkeeping for one WebSocket connection.
#[derive(Default)]
struct Routes {
    /// Page targetId -> tabId, from the last Target.getTargets.
    targets: HashMap<String, i64>,
    /// Relay-minted tab session -> tabId. The engine attaches per operation
    /// and never detaches, so several sessions can name one tab.
    sessions: HashMap<String, i64>,
    /// Real chrome.debugger child sessions (out-of-process iframes) -> tabId.
    children: HashMap<String, i64>,
    /// The tab session most recently used, which receives the tab's events.
    latest: HashMap<i64, String>,
    /// The tab session that enabled Page, which receives its dialog events.
    page_enabled: HashMap<i64, String>,
}

struct Session {
    link: u64,
    bridge: Arc<ExtensionBridge>,
    routes: Arc<Mutex<Routes>>,
}

type Reply = Value;

impl Session {
    fn new(link: u64) -> Self {
        Self {
            link,
            bridge: extension_bridge::global().clone(),
            routes: Arc::new(Mutex::new(Routes::default())),
        }
    }

    async fn run(self, socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
        let (mut sink, mut stream) = socket.split();
        let mut events = self.bridge.subscribe();
        let (reply_tx, mut replies) = mpsc::unbounded_channel::<Reply>();
        let this = Arc::new(self);
        loop {
            tokio::select! {
                incoming = stream.next() => {
                    let Some(Ok(message)) = incoming else { break };
                    let text = match message {
                        Message::Text(text) => text.to_string(),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    let Ok(command) = serde_json::from_str::<Value>(&text) else { continue };
                    let session = this.clone();
                    let reply_tx = reply_tx.clone();
                    tokio::spawn(async move {
                        let _ = reply_tx.send(session.answer(command).await);
                    });
                }
                Some(reply) = replies.recv() => {
                    // Events the extension sent before this reply are already
                    // queued; CDP delivers them first, and the engine relies on it.
                    while let Ok(event) = events.try_recv() {
                        if let Some(event) = this.translate(event) {
                            if sink.send(Message::Text(event.to_string().into())).await.is_err() { return; }
                        }
                    }
                    if sink.send(Message::Text(reply.to_string().into())).await.is_err() { return; }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            if let Some(event) = this.translate(event) {
                                if sink.send(Message::Text(event.to_string().into())).await.is_err() { return; }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    }

    async fn answer(&self, command: Value) -> Reply {
        let id = command.get("id").cloned().unwrap_or(Value::Null);
        let method = command.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = command.get("params").cloned().unwrap_or_else(|| json!({}));
        let session = command.get("sessionId").and_then(Value::as_str);
        let outcome = match session {
            None => self.root(method, &params).await,
            Some(session) => self.forward(session, method, params).await,
        };
        let mut reply = match outcome {
            Ok(result) => json!({ "id": id, "result": result }),
            Err((code, message)) => json!({ "id": id, "error": { "code": code, "message": message } }),
        };
        if let Some(session) = session {
            reply["sessionId"] = json!(session);
        }
        reply
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        self.bridge
            .request_on(self.link, method, params)
            .await
            .map_err(|error| cdp_error(&error.to_string()))
    }

    async fn tab_of_target(&self, target_id: &str) -> Result<i64, (i64, String)> {
        if let Some(tab) = self.routes.lock().unwrap().targets.get(target_id) {
            return Ok(*tab);
        }
        self.targets().await?;
        self.routes
            .lock()
            .unwrap()
            .targets
            .get(target_id)
            .copied()
            .ok_or_else(|| (-32602, format!("No target with given id found: {target_id}")))
    }

    /// Page targets the extension can attach to, remembering their tabs.
    async fn targets(&self) -> Result<Vec<Value>, (i64, String)> {
        let targets = self.request("debugger.targets", json!({})).await?;
        let mut infos = Vec::new();
        let mut routes = HashMap::new();
        for target in targets.as_array().into_iter().flatten() {
            let (Some(target_id), Some(tab)) = (
                target.get("id").and_then(Value::as_str),
                target.get("tabId").and_then(Value::as_i64),
            ) else {
                continue;
            };
            if target.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            routes.insert(target_id.to_owned(), tab);
            infos.push(json!({
                "targetId": target_id,
                "type": "page",
                "title": target.get("title").cloned().unwrap_or(json!("")),
                "url": target.get("url").cloned().unwrap_or(json!("")),
                "attached": target.get("attached").cloned().unwrap_or(json!(false)),
                "canAccessOpener": false,
            }));
        }
        self.routes.lock().unwrap().targets = routes;
        Ok(infos)
    }

    async fn tab_info(&self, tab: i64) -> Result<Value, (i64, String)> {
        let tabs = self.request("tabs.list", json!({})).await?;
        tabs.as_array()
            .into_iter()
            .flatten()
            .find(|info| info.get("tabId").and_then(Value::as_i64) == Some(tab))
            .cloned()
            .ok_or_else(|| (-32602, format!("No tab with id {tab}")))
    }

    async fn window_bounds(&self, window: i64) -> Result<Value, (i64, String)> {
        let windows = self.request("windows.list", json!({})).await?;
        let info = windows
            .as_array()
            .into_iter()
            .flatten()
            .find(|info| info.get("windowId").and_then(Value::as_i64) == Some(window))
            .cloned()
            .ok_or_else(|| (-32602, format!("Browser window not found: {window}")))?;
        Ok(json!({
            "left": info["left"],
            "top": info["top"],
            "width": info["width"],
            "height": info["height"],
            "windowState": info["state"],
        }))
    }

    /// Browser-root methods, answered from the extension's own APIs.
    async fn root(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        let target_id = || params.get("targetId").and_then(Value::as_str).unwrap_or_default();
        match method {
            "Target.getTargets" => Ok(json!({ "targetInfos": self.targets().await? })),
            "Target.attachToTarget" => {
                let tab = self.tab_of_target(target_id()).await?;
                self.request("debugger.attach", json!({ "tabId": tab })).await?;
                let session = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
                let mut routes = self.routes.lock().unwrap();
                routes.sessions.insert(session.clone(), tab);
                routes.latest.insert(tab, session.clone());
                Ok(json!({ "sessionId": session }))
            }
            "Target.detachFromTarget" => {
                let session = params.get("sessionId").and_then(Value::as_str).unwrap_or_default();
                // A relay tab session only stops being routed; the debugger
                // stays attached for the tab's other sessions.
                if self.routes.lock().unwrap().sessions.remove(session).is_some() {
                    return Ok(json!({}));
                }
                let tab = self.routes.lock().unwrap().children.get(session).copied();
                match tab {
                    Some(tab) => {
                        self.request(
                            "debugger.send",
                            json!({ "tabId": tab, "method": "Target.detachFromTarget", "params": { "sessionId": session } }),
                        )
                        .await
                    }
                    None => Err((-32602, format!("No session with given id: {session}"))),
                }
            }
            "Target.activateTarget" => {
                let tab = self.tab_of_target(target_id()).await?;
                self.request("tabs.update", json!({ "tabId": tab, "active": true })).await?;
                Ok(json!({}))
            }
            "Browser.getWindowForTarget" => {
                let tab = self.tab_of_target(target_id()).await?;
                let window = self.tab_info(tab).await?["windowId"].as_i64().unwrap_or(-1);
                Ok(json!({ "windowId": window, "bounds": self.window_bounds(window).await? }))
            }
            "Browser.getWindowBounds" => {
                let window = params.get("windowId").and_then(Value::as_i64).unwrap_or(-1);
                Ok(json!({ "bounds": self.window_bounds(window).await? }))
            }
            _ => Err((
                METHOD_NOT_FOUND,
                format!("'{method}' wasn't found (not available through the Chrome extension)"),
            )),
        }
    }

    /// Session methods go to the tab, on the child session when it is one.
    async fn forward(&self, session: &str, method: &str, params: Value) -> Result<Value, (i64, String)> {
        let (tab, child) = {
            let mut routes = self.routes.lock().unwrap();
            if let Some(tab) = routes.sessions.get(session).copied() {
                routes.latest.insert(tab, session.to_owned());
                if method == "Page.enable" {
                    routes.page_enabled.insert(tab, session.to_owned());
                }
                (tab, None)
            } else if let Some(tab) = routes.children.get(session).copied() {
                (tab, Some(session.to_owned()))
            } else {
                return Err((-32001, format!("Session with given id not found: {session}")));
            }
        };
        let mut request = json!({ "tabId": tab, "method": method, "params": params });
        if let Some(child) = child {
            request["sessionId"] = json!(child);
        }
        self.request("debugger.send", request).await
    }

    /// Turn an extension notification into the CDP event the engine expects.
    fn translate(&self, event: ExtensionEvent) -> Option<Value> {
        if event.link != self.link {
            return None;
        }
        let source = event.params.get("source")?;
        let tab = source.get("tabId").and_then(Value::as_i64)?;
        let mut routes = self.routes.lock().unwrap();
        match event.method.as_str() {
            "debugger.event" => {
                let method = event.params.get("method").and_then(Value::as_str)?;
                let params = event.params.get("params").cloned().unwrap_or_else(|| json!({}));
                match method {
                    "Target.attachedToTarget" => {
                        if let Some(child) = params.get("sessionId").and_then(Value::as_str) {
                            routes.children.insert(child.to_owned(), tab);
                        }
                    }
                    "Target.detachedFromTarget" => {
                        if let Some(child) = params.get("sessionId").and_then(Value::as_str) {
                            routes.children.remove(child);
                        }
                    }
                    _ => {}
                }
                let session = match source.get("sessionId").and_then(Value::as_str) {
                    Some(child) => child.to_owned(),
                    None if method.starts_with("Page.javascriptDialog") => routes
                        .page_enabled
                        .get(&tab)
                        .or_else(|| routes.latest.get(&tab))?
                        .clone(),
                    None => routes.latest.get(&tab)?.clone(),
                };
                Some(json!({ "method": method, "params": params, "sessionId": session }))
            }
            "debugger.detached" => {
                // The tab closed or the user cancelled debugging: its sessions end.
                let ended: Vec<String> = routes
                    .sessions
                    .iter()
                    .filter(|(_, owner)| **owner == tab)
                    .map(|(session, _)| session.clone())
                    .collect();
                routes.sessions.retain(|_, owner| *owner != tab);
                routes.children.retain(|_, owner| *owner != tab);
                routes.latest.remove(&tab);
                routes.page_enabled.remove(&tab);
                let session = ended.first()?;
                Some(json!({
                    "method": "Target.detachedFromTarget",
                    "params": { "sessionId": session, "reason": event.params.get("reason") },
                }))
            }
            _ => None,
        }
    }
}

/// `chrome.debugger.sendCommand` rejects with the CDP error serialized as
/// JSON text; keep its code so capability checks (-32601) still work.
fn cdp_error(message: &str) -> (i64, String) {
    let text = message.strip_prefix("Chrome extension: ").unwrap_or(message);
    if let Ok(error) = serde_json::from_str::<Value>(text) {
        if let (Some(code), Some(message)) = (
            error.get("code").and_then(Value::as_i64),
            error.get("message").and_then(Value::as_str),
        ) {
            return (code, message.to_owned());
        }
    }
    (-32000, text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cdp_error_from_the_extension_keeps_its_code() {
        assert_eq!(
            cdp_error(r#"Chrome extension: {"code":-32601,"message":"'Foo.bar' wasn't found"}"#),
            (-32601, "'Foo.bar' wasn't found".to_owned())
        );
        assert_eq!(
            cdp_error("Chrome extension: Debugger is not attached to the tab with id: 4."),
            (-32000, "Debugger is not attached to the tab with id: 4.".to_owned())
        );
    }
}
