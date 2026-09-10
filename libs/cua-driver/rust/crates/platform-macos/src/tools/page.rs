//! macOS implementation of the cross-platform `PageBackend` trait.
//!
//! Routes by `bundle_id` / Electron-detection / WKWebView-detection to:
//!   - **Apple Events** (Chromium-family + Safari) — zero-config, uses
//!     `osascript do JavaScript`.
//!   - **CDP** (Electron) — port discovered from the running process.
//!   - **AX-tree fallback** (WKWebView / Tauri) — read-only, no JS.
//!
//! The MCP tool definition (name, schema, action dispatch) lives in
//! `cua_driver_core::page` — this file only implements the backend.

use async_trait::async_trait;
use cua_driver_core::page::{ClickElementResult, PageBackend};
use std::sync::Arc;

use super::ToolState;
use crate::browser::{is_wk_web_view_app, AXPageReader, BrowserJs, CdpClient, ElectronJs};

pub struct MacOsPageBackend {
    pub state: Arc<ToolState>,
}

async fn dispatch_click_visual<T, E>(
    registry: &crate::cursor::CursorRegistry,
    sink: &dyn crate::cursor::visual::PointerVisualSink,
    target: Option<crate::cursor::visual::ResolvedPointerTarget>,
    native: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let handle = crate::cursor::visual::emit_pointer_target(registry, sink, "default", target);
    let result = native.await;
    if result.is_ok() {
        crate::cursor::visual::emit_pointer_contact(sink, handle);
    }
    result
}

impl MacOsPageBackend {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }

    /// Resolve `bundle_id` for `pid` via the running-apps list.
    async fn bundle_id_for(pid: i32) -> String {
        tokio::task::spawn_blocking(move || {
            crate::apps::list_running_apps()
                .into_iter()
                .find(|a| a.pid == pid)
                .and_then(|a| a.bundle_id)
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }
}

#[async_trait]
impl PageBackend for MacOsPageBackend {
    async fn get_text(&self, pid: i32, window_id: u64) -> anyhow::Result<String> {
        let bundle_id = Self::bundle_id_for(pid).await;

        let use_ax_fallback = !BrowserJs::supports(&bundle_id)
            && tokio::task::spawn_blocking(move || is_wk_web_view_app(pid))
                .await
                .unwrap_or(false);

        if use_ax_fallback {
            return ax_text_fallback(pid, window_id).await;
        }

        // Try JS path first; on error, fall back to AX walk.
        match execute_js("document.body.innerText", &bundle_id, pid, window_id).await {
            Ok(result) => Ok(result),
            Err(_) => ax_text_fallback(pid, window_id).await,
        }
    }

    async fn query_dom(
        &self,
        pid: i32,
        window_id: u64,
        css_selector: &str,
        attributes: &[String],
    ) -> anyhow::Result<String> {
        let bundle_id = Self::bundle_id_for(pid).await;

        let use_ax_fallback = !BrowserJs::supports(&bundle_id)
            && tokio::task::spawn_blocking(move || is_wk_web_view_app(pid))
                .await
                .unwrap_or(false);

        if use_ax_fallback {
            let results = ax_query_fallback(pid, window_id, css_selector).await?;
            return Ok(format_ax_elements(&results));
        }

        let js = build_query_selector_js(css_selector, attributes);
        match execute_js(&js, &bundle_id, pid, window_id).await {
            Ok(result) => Ok(result),
            Err(_) => {
                let results = ax_query_fallback(pid, window_id, css_selector).await?;
                Ok(format_ax_elements(&results))
            }
        }
    }

    async fn execute_javascript(
        &self,
        pid: i32,
        window_id: u64,
        javascript: &str,
    ) -> anyhow::Result<String> {
        let bundle_id = Self::bundle_id_for(pid).await;
        execute_js(javascript, &bundle_id, pid, window_id).await
    }

    async fn execute_javascript_targeted(
        &self,
        pid: i32,
        window_id: u64,
        javascript: &str,
        cdp_port: Option<u16>,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<String> {
        if cdp_port.is_none() && target_url_contains.is_none() {
            return self.execute_javascript(pid, window_id, javascript).await;
        }
        let port = resolve_cdp_port(pid, cdp_port, "execute_javascript").await?;
        self.state
            .cdp_sessions
            .evaluate(javascript, port, target_url_contains)
            .await
    }

    async fn click_element(
        &self,
        pid: i32,
        window_id: u64,
        selector: &str,
    ) -> anyhow::Result<ClickElementResult> {
        let selector_js = json_string(selector);
        let probe_js = format!(
            r#"(function() {{
  var selector = {selector_js};
  var el = document.querySelector(selector);
  if (!el) throw new Error("element_not_found:" + selector);
  var r = el.getBoundingClientRect();
  return JSON.stringify({{
    vx: r.left + r.width / 2,
    vy: r.top + r.height / 2,
    sx: window.screenX + (window.outerWidth - window.innerWidth) / 2,
    sy: window.screenY + (window.outerHeight - window.innerHeight),
    dpr: window.devicePixelRatio || 1
  }});
}})();"#
        );
        let probe_raw = self.execute_javascript(pid, window_id, &probe_js).await?;
        let parsed = parse_click_probe(&probe_raw)?;

        let vx = required_finite(&parsed, "vx", &probe_raw)?;
        let vy = required_finite(&parsed, "vy", &probe_raw)?;
        let sx = required_finite(&parsed, "sx", &probe_raw)?;
        let sy = required_finite(&parsed, "sy", &probe_raw)?;
        let dpr = parsed
            .get("dpr")
            .and_then(serde_json::Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(1.0);

        let screen_x = sx + vx * dpr;
        let screen_y = sy + vy * dpr;
        let click_js = format!(
            r#"(function() {{
  var selector = {selector_js};
  var el = document.querySelector(selector);
  if (!el) throw new Error("element_not_found_on_click:" + selector);
  el.click();
  return "clicked:" + selector;
}})();"#
        );
        dispatch_click_visual(
            &self.state.cursor_registry,
            &crate::cursor::visual::OverlayVisualSink,
            crate::cursor::visual::point(screen_x, screen_y, Some(window_id as u32)),
            self.execute_javascript(pid, window_id, &click_js),
        )
        .await?;

        Ok(ClickElementResult {
            screen_x,
            screen_y,
            viewport_x: vx,
            viewport_y: vy,
            message: format!(
                "Clicked {selector} at screen ({screen_x:.0},{screen_y:.0}) on pid {pid}."
            ),
        })
    }

    async fn enable_javascript_apple_events(&self, bundle_id: &str) -> anyhow::Result<String> {
        BrowserJs::enable_javascript_apple_events(bundle_id).await?;
        Ok("JavaScript from Apple Events has been enabled. The browser is restarting.".to_owned())
    }

    async fn insert_text(
        &self,
        pid: i32,
        _window_id: u64,
        text: &str,
        cdp_port: Option<u16>,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<String> {
        let port = resolve_cdp_port(pid, cdp_port, "insert_text").await?;
        self.state
            .cdp_sessions
            .insert_text(text, port, target_url_contains)
            .await?;
        Ok(format!(
            "Inserted {} character(s) via CDP Input.insertText.",
            text.chars().count()
        ))
    }

    async fn type_keystrokes(
        &self,
        pid: i32,
        _window_id: u64,
        text: &str,
        cdp_port: Option<u16>,
        target_url_contains: Option<&str>,
    ) -> anyhow::Result<String> {
        let port = resolve_cdp_port(pid, cdp_port, "type_keystrokes").await?;
        self.state
            .cdp_sessions
            .dispatch_keystrokes(text, port, target_url_contains)
            .await?;
        Ok(format!(
            "Typed {} character(s) via CDP keystroke events.",
            text.chars().count()
        ))
    }
}

/// Shared CDP-port lookup + actionable error for the `insert_text` /
/// `type_keystrokes` actions. An explicit `cdp_port` skips auto-discovery
/// entirely — needed when the port only answers the browser-level
/// `devtools/browser` endpoint (see `CdpSession`), which auto-discovery
/// can't confirm on its own.
async fn resolve_cdp_port(pid: i32, cdp_port: Option<u16>, action: &str) -> anyhow::Result<u16> {
    if let Some(port) = cdp_port {
        return Ok(port);
    }
    CdpClient::find_port_for_pid(pid).await.ok_or_else(|| {
        anyhow::anyhow!(
            "No Chrome DevTools Protocol port found for pid {pid}. {action} needs \
             the browser to have been launched with a CDP port on a NON-default profile — \
             Chrome refuses to open --remote-debugging-port on its default data directory \
             (even if you pass --user-data-dir explicitly pointing at that same default \
             path). Relaunch via launch_app with cdp_debugging_port AND \
             additional_arguments: [\"--user-data-dir=<some other path>\"], e.g. a \
             dedicated automation profile — this will not have the user's existing \
             logins/session. Alternatively, if you already enabled Chrome's own \
             remote-debugging toggle for this profile (chrome://inspect/#remote-debugging), \
             pass that port explicitly via cdp_port — auto-discovery can't confirm it."
        )
    })
}

/// Route JavaScript execution to the appropriate backend.
async fn execute_js(js: &str, bundle_id: &str, pid: i32, window_id: u64) -> anyhow::Result<String> {
    if BrowserJs::supports(bundle_id) {
        let window_id = u32::try_from(window_id)
            .map_err(|_| anyhow::anyhow!("macOS window_id {window_id} is out of u32 range"))?;
        return BrowserJs::execute(js, bundle_id, window_id).await;
    }
    let is_electron = tokio::task::spawn_blocking(move || ElectronJs::is_electron(pid)).await?;
    if is_electron {
        return ElectronJs::execute(js, pid).await;
    }
    let is_wk = tokio::task::spawn_blocking(move || is_wk_web_view_app(pid)).await?;
    if is_wk {
        anyhow::bail!(
            "execute_javascript is not available for WKWebView/Tauri apps. \
             Use get_text or query_dom instead."
        );
    }
    anyhow::bail!("Unsupported browser: bundle_id={bundle_id}");
}

/// Extract page text via the AX tree.
async fn ax_text_fallback(pid: i32, window_id: u64) -> anyhow::Result<String> {
    let window_id = u32::try_from(window_id)
        .map_err(|_| anyhow::anyhow!("macOS window_id {window_id} is out of u32 range"))?;
    let result =
        tokio::task::spawn_blocking(move || crate::ax::tree::walk_tree(pid, Some(window_id), None))
            .await
            .map_err(|e| anyhow::anyhow!("AX walk task failed: {e}"))?;
    Ok(AXPageReader::extract_text(&result.tree_markdown))
}

/// Query AX tree by CSS selector.
async fn ax_query_fallback(
    pid: i32,
    window_id: u64,
    selector: &str,
) -> anyhow::Result<Vec<crate::browser::ax_page_reader::AXElement>> {
    let window_id = u32::try_from(window_id)
        .map_err(|_| anyhow::anyhow!("macOS window_id {window_id} is out of u32 range"))?;
    let sel = selector.to_owned();
    let result =
        tokio::task::spawn_blocking(move || crate::ax::tree::walk_tree(pid, Some(window_id), None))
            .await
            .map_err(|e| anyhow::anyhow!("AX walk task failed: {e}"))?;
    Ok(AXPageReader::query(&sel, &result.tree_markdown))
}

fn format_ax_elements(elements: &[crate::browser::ax_page_reader::AXElement]) -> String {
    if elements.is_empty() {
        return "No elements found.".to_owned();
    }
    let mut lines = Vec::new();
    for el in elements {
        let index_str = el.index.map(|i| format!("[{i}] ")).unwrap_or_default();
        let title = if !el.title.is_empty() {
            format!(" \"{}\"", el.title)
        } else {
            String::new()
        };
        let value = if !el.value.is_empty() {
            format!(" = \"{}\"", el.value)
        } else {
            String::new()
        };
        let desc = if !el.description.is_empty() {
            format!(" ({})", el.description)
        } else {
            String::new()
        };
        lines.push(format!("- {index_str}{}{title}{value}{desc}", el.role));
    }
    lines.join("\n")
}

/// Build a querySelectorAll JS snippet.
fn build_query_selector_js(selector: &str, attributes: &[String]) -> String {
    let escaped_sel = json_string(selector);
    let attrs_js = if attributes.is_empty() {
        "['tagName','textContent','href','id','class','type','value','placeholder','name']"
            .to_owned()
    } else {
        let parts: Vec<String> = attributes.iter().map(|a| json_string(a)).collect();
        format!("[{}]", parts.join(","))
    };
    format!(
        r#"(function(){{
  var sel = {escaped_sel};
  var attrs = {attrs_js};
  var els = Array.from(document.querySelectorAll(sel));
  return JSON.stringify(els.map(function(el){{
    var obj = {{}};
    attrs.forEach(function(a){{ obj[a] = el[a] !== undefined ? el[a] : el.getAttribute(a); }});
    obj.textContent = (el.textContent||'').trim().substring(0,200);
    return obj;
  }}));
}})();"#
    )
}

fn json_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn parse_click_probe(raw: &str) -> anyhow::Result<serde_json::Value> {
    let first = serde_json::from_str::<serde_json::Value>(raw.trim()).map_err(|error| {
        anyhow::anyhow!("click_element: could not parse coord JSON from probe {raw:?}: {error}")
    })?;
    match first {
        serde_json::Value::String(inner) => serde_json::from_str(&inner).map_err(|error| {
            anyhow::anyhow!(
                "click_element: could not parse inner coord JSON from probe {raw:?}: {error}"
            )
        }),
        other => Ok(other),
    }
}

fn required_finite(value: &serde_json::Value, key: &str, raw: &str) -> anyhow::Result<f64> {
    value
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|number| number.is_finite())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "click_element: probe JSON missing/invalid required field '{key}' (raw: {raw:?})"
            )
        })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn slice_a_page_dispatch_emits_contact_only_after_accepted_javascript() {
        for failed in [false, true] {
            let registry = crate::cursor::CursorRegistry::new();
            let sink = crate::cursor::visual::test_support::RecordingSink::default();
            let result = super::dispatch_click_visual(
                &registry,
                &sink,
                crate::cursor::visual::point(90.0, 80.0, Some(42)),
                async {
                    let events = sink.1.lock().unwrap();
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0].target, Some((90.0, 80.0)));
                    assert_eq!(events[0].phase, cursor_overlay::VisualPhase::Intent);
                    if failed {
                        Err("JS dispatch failed")
                    } else {
                        Ok("clicked")
                    }
                },
            )
            .await;
            assert_eq!(result.is_err(), failed);
            let events = sink.1.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.phase == cursor_overlay::VisualPhase::Contact)
                    .count(),
                usize::from(!failed)
            );
        }
    }
}
