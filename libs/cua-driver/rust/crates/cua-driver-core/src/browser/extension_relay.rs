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

use super::cdp_ws::CdpMethodPolicy;
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
            // Browsers always send Origin on WebSocket handshakes and the
            // engine's client never does: no web page may reach this relay.
            if request.headers().contains_key("origin") {
                link = None;
            }
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
    /// Relay tab session -> the cua session's cursor color, sent with each of
    /// its commands so a tab shared by two sessions shows whichever acts.
    colors: HashMap<String, Value>,
    /// Real chrome.debugger child sessions (out-of-process iframes) -> tabId.
    children: HashMap<String, i64>,
    /// The tab session that enabled Page, which receives its dialog events.
    page_enabled: HashMap<i64, String>,
}

struct Session {
    link: u64,
    bridge: Arc<ExtensionBridge>,
    routes: Arc<Mutex<Routes>>,
    /// Tabs this connection attached. The extension keeps a tab's debugger
    /// attached while any connection holds it and detaches once the last
    /// one closes (the engine drops its connection when the cua session ends).
    /// `None` once the connection closed, so a late attach holds nothing.
    held_tabs: Mutex<Option<std::collections::HashSet<i64>>>,
}

/// How many relay connections hold each (extension link, tab).
fn tab_holders() -> &'static Mutex<HashMap<(u64, i64), usize>> {
    static HOLDERS: std::sync::OnceLock<Mutex<HashMap<(u64, i64), usize>>> =
        std::sync::OnceLock::new();
    HOLDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Count one more holder of `tab`.
fn hold_tab(link: u64, tab: i64) {
    *tab_holders().lock().unwrap().entry((link, tab)).or_default() += 1;
}

/// Drop one holder of `tab`; true when it was the last.
fn release_tab(link: u64, tab: i64) -> bool {
    let mut holders = tab_holders().lock().unwrap();
    match holders.get_mut(&(link, tab)) {
        Some(count) if *count > 1 => {
            *count -= 1;
            false
        }
        Some(_) => {
            holders.remove(&(link, tab));
            true
        }
        None => false,
    }
}

type Reply = Value;

impl Session {
    fn new(link: u64) -> Self {
        Self {
            link,
            bridge: extension_bridge::global().clone(),
            routes: Arc::new(Mutex::new(Routes::default())),
            held_tabs: Mutex::new(Some(std::collections::HashSet::new())),
        }
    }

    /// The connection closed: let go of its tabs, and detach the debugger
    /// from each one no other connection holds.
    async fn release_held_tabs(&self) {
        let tabs = self.held_tabs.lock().unwrap().take().unwrap_or_default();
        for tab in tabs {
            if release_tab(self.link, tab) {
                let _ = self
                    .bridge
                    .request_on(self.link, "debugger.detach", json!({ "tabId": tab }))
                    .await;
            }
        }
    }

    async fn run(self, socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
        let (mut sink, mut stream) = socket.split();
        let mut events = self.bridge.subscribe();
        let (reply_tx, mut replies) = mpsc::unbounded_channel::<Reply>();
        let this = Arc::new(self);
        'serve: loop {
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
                        for event in this.translate(event) {
                            if sink.send(Message::Text(event.to_string().into())).await.is_err() { break 'serve; }
                        }
                    }
                    if sink.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            for event in this.translate(event) {
                                if sink.send(Message::Text(event.to_string().into())).await.is_err() { break 'serve; }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        this.release_held_tabs().await;
    }

    async fn answer(&self, command: Value) -> Reply {
        let id = command.get("id").cloned().unwrap_or(Value::Null);
        let method = command.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = command.get("params").cloned().unwrap_or_else(|| json!({}));
        let session = command.get("sessionId").and_then(Value::as_str);
        // The relay reaches the user's own profile, so it enforces the same
        // allowlist the engine applies to an existing-profile connection,
        // whoever holds the URL.
        let outcome = if !CdpMethodPolicy::ExistingProfile.allows(method) {
            Err((
                METHOD_NOT_FOUND,
                format!("'{method}' is not available for the user's own browser profile"),
            ))
        } else {
            match session {
                None => self.root(method, &params).await,
                Some(session) => self.forward(session, method, params).await,
            }
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
                let color = params.get("cuaSessionColor").cloned().unwrap_or(Value::Null);
                self.request("debugger.attach", json!({ "tabId": tab, "sessionColor": color }))
                    .await?;
                if let Some(held) = self.held_tabs.lock().unwrap().as_mut() {
                    if held.insert(tab) {
                        hold_tab(self.link, tab);
                    }
                }
                let session = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
                let mut routes = self.routes.lock().unwrap();
                routes.sessions.insert(session.clone(), tab);
                routes.colors.insert(session.clone(), color);
                Ok(json!({ "sessionId": session }))
            }
            "Target.detachFromTarget" => {
                let session = params.get("sessionId").and_then(Value::as_str).unwrap_or_default();
                // A relay tab session only stops being routed; the debugger
                // stays attached until this connection closes and no other
                // connection holds the tab.
                {
                    let mut routes = self.routes.lock().unwrap();
                    if let Some(tab) = routes.sessions.remove(session) {
                        routes.colors.remove(session);
                        routes.page_enabled.retain(|_, owner| owner != session);
                        // The tab's last session gone: its iframe routes go too.
                        if !routes.sessions.values().any(|owner| *owner == tab) {
                            routes.children.retain(|_, owner| *owner != tab);
                        }
                        return Ok(json!({}));
                    }
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
        let (tab, child, color) = {
            let mut routes = self.routes.lock().unwrap();
            if let Some(tab) = routes.sessions.get(session).copied() {
                if method == "Page.enable" {
                    routes.page_enabled.insert(tab, session.to_owned());
                }
                (tab, None, routes.colors.get(session).cloned().unwrap_or(Value::Null))
            } else if let Some(tab) = routes.children.get(session).copied() {
                // Chrome gives the extension one debugger attachment per tab,
                // so an iframe session is shared by every tab session there and
                // has no single owner: it keeps the color the tab already shows.
                (tab, Some(session.to_owned()), Value::Null)
            } else {
                return Err((-32001, format!("Session with given id not found: {session}")));
            }
        };
        let mut request =
            json!({ "tabId": tab, "method": method, "params": params, "sessionColor": color });
        if let Some(child) = child {
            request["sessionId"] = json!(child);
        }
        self.request("debugger.send", request).await
    }

    /// Turn an extension notification into the CDP events the engine expects.
    /// A tab event goes to every live session of that tab: the engine attaches
    /// per operation, operations can overlap, and each filters by its own
    /// session. Dialog events go to the session that enabled Page.
    fn translate(&self, event: ExtensionEvent) -> Vec<Value> {
        if event.link != self.link {
            return Vec::new();
        }
        let Some(tab) = event
            .params
            .get("source")
            .and_then(|source| source.get("tabId"))
            .and_then(Value::as_i64)
        else {
            return Vec::new();
        };
        let source_session = event
            .params
            .pointer("/source/sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut routes = self.routes.lock().unwrap();
        let tab_sessions: Vec<String> = routes
            .sessions
            .iter()
            .filter(|(_, owner)| **owner == tab)
            .map(|(session, _)| session.clone())
            .collect();
        match event.method.as_str() {
            "debugger.event" => {
                let Some(method) = event.params.get("method").and_then(Value::as_str) else {
                    return Vec::new();
                };
                let params = event.params.get("params").cloned().unwrap_or_else(|| json!({}));
                if let Some(child) = params.get("sessionId").and_then(Value::as_str) {
                    match method {
                        "Target.attachedToTarget" => {
                            routes.children.insert(child.to_owned(), tab);
                        }
                        "Target.detachedFromTarget" => {
                            routes.children.remove(child);
                        }
                        _ => {}
                    }
                }
                let sessions = match source_session {
                    Some(child) => vec![child],
                    None if method.starts_with("Page.javascriptDialog") => {
                        match routes.page_enabled.get(&tab) {
                            Some(session) => vec![session.clone()],
                            None => tab_sessions,
                        }
                    }
                    None => tab_sessions,
                };
                sessions
                    .into_iter()
                    .map(|session| json!({ "method": method, "params": params, "sessionId": session }))
                    .collect()
            }
            // Older extension builds released an idle debugger. Relay
            // sessions stay valid (the next command reattaches and replays
            // Page.enable); only out-of-process iframe sessions died with the
            // attachment.
            "debugger.released" => {
                routes.children.retain(|_, owner| *owner != tab);
                Vec::new()
            }
            "debugger.detached" => {
                // The debugger left the tab (session done, tab closed, or the
                // user cancelled): its sessions end; the next operation attaches again.
                routes.sessions.retain(|_, owner| *owner != tab);
                let Routes { sessions, colors, .. } = &mut *routes;
                colors.retain(|session, _| sessions.contains_key(session));
                routes.children.retain(|_, owner| *owner != tab);
                routes.page_enabled.remove(&tab);
                tab_sessions
                    .into_iter()
                    .map(|session| {
                        json!({
                            "method": "Target.detachedFromTarget",
                            "params": { "sessionId": session, "reason": event.params.get("reason") },
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn each_tab_session_keeps_its_own_color_on_a_shared_tab() {
        use tokio::io::AsyncWriteExt;
        let (bridge, mut extension, _dir) = extension_bridge::tests::connected().await;
        let session = Arc::new(Session {
            link: bridge.links()[0].link,
            bridge,
            routes: Arc::new(Mutex::new(Routes::default())),
            held_tabs: Mutex::new(Some(Default::default())),
        });
        session.routes.lock().unwrap().targets.insert("T".to_owned(), 4);
        // The fake extension answers every request and hands it to the test.
        let (seen_tx, mut seen) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            loop {
                let request = extension_bridge::tests::read_frame(&mut extension).await;
                let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{}});
                extension.write_all(&extension_bridge::frame(&reply)).await.unwrap();
                seen_tx.send(request).unwrap();
            }
        });
        let attach = |color: &str| {
            let session = session.clone();
            let params = json!({ "targetId": "T", "flatten": true, "cuaSessionColor": color });
            async move { session.root("Target.attachToTarget", &params).await.unwrap()["sessionId"].clone() }
        };
        let a = attach("#B284FF").await;
        assert_eq!(seen.recv().await.unwrap()["params"]["sessionColor"], "#B284FF");
        let b = attach("#F784AA").await;
        assert_eq!(seen.recv().await.unwrap()["params"]["sessionColor"], "#F784AA");
        for (tab_session, color) in [(&a, "#B284FF"), (&b, "#F784AA"), (&a, "#B284FF")] {
            session
                .forward(tab_session.as_str().unwrap(), "Runtime.evaluate", json!({}))
                .await
                .unwrap();
            let sent = seen.recv().await.unwrap();
            assert_eq!(sent["method"], "debugger.send");
            assert_eq!(sent["params"]["sessionColor"], color);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_debugger_is_detached_when_the_last_connection_holding_the_tab_closes() {
        use tokio::io::AsyncWriteExt;
        let (bridge, mut extension, _dir) = extension_bridge::tests::connected().await;
        let link = bridge.links()[0].link;
        let session = |bridge: Arc<ExtensionBridge>| {
            let session = Session {
                link,
                bridge,
                routes: Arc::new(Mutex::new(Routes::default())),
                held_tabs: Mutex::new(Some(Default::default())),
            };
            session.routes.lock().unwrap().targets.insert("T".to_owned(), 9);
            session
        };
        let (first, second) = (session(bridge.clone()), session(bridge));
        let (seen_tx, mut seen) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            loop {
                let request = extension_bridge::tests::read_frame(&mut extension).await;
                let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{}});
                extension.write_all(&extension_bridge::frame(&reply)).await.unwrap();
                seen_tx.send(request).unwrap();
            }
        });
        let attach = json!({ "targetId": "T", "flatten": true });
        for session in [&first, &second, &first] {
            session.root("Target.attachToTarget", &attach).await.unwrap();
            assert_eq!(seen.recv().await.unwrap()["method"], "debugger.attach");
        }
        // One connection closing leaves the tab attached for the other.
        first.release_held_tabs().await;
        assert!(seen.try_recv().is_err(), "no detach while a connection holds the tab");
        // A late attach on a closed connection holds nothing.
        first.root("Target.attachToTarget", &attach).await.unwrap();
        assert_eq!(seen.recv().await.unwrap()["method"], "debugger.attach");
        second.release_held_tabs().await;
        let detach = seen.recv().await.unwrap();
        assert_eq!(detach["method"], "debugger.detach");
        assert_eq!(detach["params"]["tabId"], 9);
        assert!(tab_holders().lock().unwrap().get(&(link, 9)).is_none());
    }
}
