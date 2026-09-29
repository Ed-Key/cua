use async_trait::async_trait;
use cua_driver_contract::ElementFields;
use cua_driver_core::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use super::ToolState;

pub struct GetWindowStateTool {
    state: Arc<ToolState>,
}

impl GetWindowStateTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Total time `get_window_state(app)` spends reading the app's AX windows.
const AX_LOOKUP_BUDGET: std::time::Duration = std::time::Duration::from_millis(1500);

/// Slack past `timeout_ms` before the walk task is abandoned: one in-flight AX
/// call may still be waiting on its messaging timeout.
const AX_WALK_BACKSTOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "get_window_state".into(),
        description: "Read one window (pid, window_id, or app for its only window): \
            accessibility rows with element_token and element_index, plus a screenshot whose pixels are the x,y space for pixel \
            actions. On macOS later looks return only changed rows (diff); pair element_index \
            with the latest snapshot_id. Details: skill://cua-driver/WORKFLOW.md".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid": { "type": "integer", "description": "Target process ID." },
                "window_id": { "type": "integer", "description": "Window ID from list_windows." },
                "app": { "type": "string", "description": "App name or bundle id; reads its only window on the current Space. Use pid + window_id when it has several." },
                "query": { "type": "string", "description": "Case-insensitive filter: matching rows plus ancestors; indices unchanged." },
                "query_context": { "type": "boolean", "default": false, "description": "With query, also keep every row under each match." },
                "diff": { "type": "boolean", "default": true, "description": "Return only rows changed since this session's last look; false forces the full outline. macOS only." },
                "element_fields": cua_driver_core::tool_schema::element_fields_schema(),
                "capture_mode": cua_driver_core::capture_mode::capture_mode_schema(),
                "include_accessibility_tree": {
                    "type": "boolean",
                    "default": true,
                    "description": "false skips the tree walk and returns only the screenshot and window metadata; not with include_screenshot:false."
                },
                "include_screenshot": {
                    "type": "boolean",
                    "default": true,
                    "description": "false returns the tree only, which cannot ground a pixel action."
                },
                "screenshot_out_file": {
                    "type": "string",
                    "description": "Write the PNG to this path instead of inline base64."
                },
                "max_elements": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 2000,
                    "description": "Cap on elements walked; tree and elements truncate together."
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 25,
                    "description": "Cap on tree depth."
                },
                "timeout_ms": cua_driver_core::tool_schema::timeout_ms_schema(),
                "max_dimension": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Legacy long-edge cap; the tighter of this and max_image_dimension wins."
                },
                "max_image_dimension": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Per-call screenshot long-edge limit; 0 is native resolution."
                }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: false,
        open_world: false,
    })
}

fn chromium_browser_window(pid: i32) -> bool {
    let identity = format!(
        "{} {}",
        crate::apps::get_app_name_for_pid(pid).unwrap_or_default(),
        crate::apps::bundle_id_for_pid(pid).unwrap_or_default()
    )
    .to_ascii_lowercase();
    identity
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token,
                "chrome"
                    | "chromium"
                    | "brave"
                    | "edge"
                    | "vivaldi"
                    | "opera"
                    | "arc"
                    | "thorium"
                    | "iridium"
                    | "yandex"
            )
        })
}

#[async_trait]
impl Tool for GetWindowStateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    /// `app` that names exactly one window becomes that pid + window_id
    /// before authorization, so policy and consent judge the real window.
    /// Otherwise the call stays as `app`, is authorized like an unfiltered
    /// window listing, and carries the refusal for `invoke` to return. The
    /// lookup runs only here, so `invoke` never reads a window that
    /// authorization did not see.
    async fn resolve_target(&self, args: &mut Value) {
        if args.get("app").is_none() {
            return;
        }
        let resolved = window_target(args).await;
        let Some(fields) = args.as_object_mut() else {
            return;
        };
        match resolved {
            Ok((pid, window_id)) => {
                fields.remove("app");
                fields.insert("pid".into(), pid.into());
                fields.insert("window_id".into(), window_id.into());
            }
            Err(refusal) => {
                fields.insert(APP_REFUSAL_ARG.into(), stored_refusal(refusal));
            }
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let (pid, window_id) = match invoke_target(&args).await {
            Ok(target) => target,
            Err(e) => return e,
        };

        // Issue #2237: pre-flight the requested window against WindowServer
        // BEFORE the (timeout_ms-bounded) AX walk. An id that no window carries, or
        // that another process owns, used to fall through the scoped filter and
        // return the app's MENU BAR as a healthy snapshot of the requested
        // window — with a screenshot of the requested window beside it. macOS
        // hosts every sandboxed app's Open/Save panel in
        // `com.apple.appkit.xpc.openAndSavePanelService`, so the owner-mismatch
        // shape is routine, and the caller must be told the real owner pid.
        {
            let owner = match tokio::task::spawn_blocking(move || {
                crate::windows::resolve_window_owner(pid, window_id)
            })
            .await
            {
                Ok(owner) => owner,
                Err(e) => {
                    return ToolResult::error(format!(
                        "window ownership lookup for window_id {window_id} failed: {e}"
                    ))
                }
            };
            if let Some(scope) = crate::ax::window_scope::scope_from_owner(&owner) {
                if let Some(refusal) = window_scope_refusal(pid, window_id, &scope) {
                    return refusal;
                }
            }
        }

        let query = args.opt_str("query");
        let query_context = match args.get("query_context") {
            None | Some(serde_json::Value::Bool(false)) => false,
            Some(serde_json::Value::Bool(true)) => true,
            Some(_) => return ToolResult::error("query_context must be a boolean"),
        };
        if query_context && query.as_deref().is_none_or(|q| q.trim().is_empty()) {
            return ToolResult::error("query_context requires a nonblank query");
        }
        let screenshot_out_file = args.opt_str("screenshot_out_file").map(|s| {
            // Expand ~ prefix.
            if let Some(relative) = s.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{relative}")
            } else {
                s
            }
        });
        // Effective config resolves call-arg > session-override > global. The
        // daemon injects `_session_id` for named MCP sessions; absent => global.
        let session_id = args.opt_str("_session_id");
        let effective_max_dim = {
            let cfg = self.state.config.read().unwrap();
            self.state
                .session_config
                .effective_max_image_dimension(session_id.as_deref(), &cfg)
        };
        // `capture_mode` is DEPRECATED and ignored — get_window_state always
        // returns BOTH the tree and a screenshot now, so the agent grounds on
        // both and cross-checks (the AX tree lies often enough that a grounding
        // screenshot should always be present). The modality is chosen at action
        // time: an element ax action (element_index) or element px action (x,y).
        // We don't even read the arg; it stays in the schema only so old callers
        // don't trip additionalProperties:false.
        //
        // `include_screenshot` (default true) is the perf opt-out: set false to
        // skip the grab and return the tree only — the cheap path when you're
        // just re-indexing before an element ax action. `screenshot_out_file`
        // still forces a capture (an explicit "write the frame to disk").
        let include_screenshot = args.get("include_screenshot").and_then(|v| v.as_bool());
        let should_capture = include_screenshot != Some(false) || screenshot_out_file.is_some();
        // `include_accessibility_tree` (default true) is the mirror image of
        // `include_screenshot`: set false to SKIP the AX walk (the expensive
        // part) and return just the screenshot + window metadata — the
        // capture-only / preview path. With BOTH the tree and the screenshot
        // opted out there is nothing to return, so refuse rather than emit an
        // empty payload.
        let want_tree = args
            .get("include_accessibility_tree")
            .and_then(|v| v.as_bool())
            != Some(false);
        if !want_tree && !should_capture {
            return ToolResult::error(
                "Nothing to return: both include_accessibility_tree:false and \
                 include_screenshot:false. Set at least one to true, or pass \
                 screenshot_out_file to force a capture.",
            );
        }
        // Optional per-call cap on the returned screenshot's long edge, folded
        // with the session/global ceiling below (the tighter wins).
        let max_dimension = args
            .get("max_dimension")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as u32);
        let max_image_dimension = args
            .get("max_image_dimension")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        // Internal direct-tool mode used by verify_state. Registry ingress
        // strips underscore-prefixed arguments before public dispatch; only
        // a trusted direct in-process invocation can enable this mode.
        let observation_only = args
            .get("_observation_only")
            .and_then(|value| value.as_bool())
            == Some(true);
        // Optional caps — when omitted, fall back to the defaults baked into
        // the AX walker (#22865). minimum:1 keyed in the schema, but defend
        // against 0 here as well so a misbehaving client can't disable the
        // walk entirely.
        let max_elements = args
            .get("max_elements")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as usize)
            .unwrap_or(crate::ax::tree::DEFAULT_MAX_ELEMENTS);
        let max_depth = args
            .get("max_depth")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as usize)
            .unwrap_or(crate::ax::tree::DEFAULT_MAX_DEPTH);
        let timeout_ms = cua_driver_core::tool_schema::resolve_timeout_ms(args.get("timeout_ms"));
        // Change-only outline after the first look at a window (default on).
        // A query always renders the full filtered outline: its rows are few
        // and a filter is a fresh question, not a follow-up look. verify_state's
        // internal observation evaluates the full outline.
        let want_diff = match args.get("diff") {
            None | Some(serde_json::Value::Bool(true)) => query.is_none() && !observation_only,
            Some(serde_json::Value::Bool(false)) => false,
            Some(_) => return ToolResult::error("diff must be a boolean"),
        };
        let element_fields = match args.get("element_fields").map(|v| v.as_str()) {
            None | Some(Some("none")) => ElementFields::None,
            Some(Some("compact")) => ElementFields::Compact,
            Some(Some("full")) => ElementFields::Full,
            Some(_) => {
                return ToolResult::error(
                    "element_fields must be \"none\", \"compact\" or \"full\"",
                )
            }
        };

        // The walk leaves one retain on every actionable element. The owner
        // built inside the blocking task takes them over immediately, so a walk
        // abandoned by the backstop timeout still releases them when the task
        // drops its result.
        let mut walk_owner: Option<crate::ax::cache::CachedSnapshot> = None;
        let mut first_walk: Option<cua_driver_core::walk_budget::WalkOutcome> = None;
        let mut tree_result = if want_tree {
            let q = query.clone();
            // `timeout_ms` bounds the walk itself: it returns the partial tree
            // when the budget runs out. The outer deadline is only a backstop
            // for an AX call that ignores the per-element messaging timeout
            // (dropping a spawn_blocking JoinHandle cannot cancel it).
            // A walk that timed out after only a few nodes (an app slow to
            // answer its first reads) gets one retry with a larger budget.
            let retry_budget = |first: &cua_driver_core::walk_budget::WalkOutcome| {
                cua_driver_core::walk_budget::retry_timeout_ms(first)
            };
            let walk_future = tokio::task::spawn_blocking(move || {
                let walk = |budget_ms| {
                    crate::ax::tree::walk_tree_budgeted(
                        pid,
                        Some(window_id),
                        tree_query(q.as_deref(), query_context),
                        max_depth,
                        cua_driver_core::walk_budget::WalkBudget::new(budget_ms, max_elements),
                    )
                };
                let first = walk(timeout_ms);
                let (tree, first_walk) = match retry_budget(&first.walk) {
                    Some(budget_ms) => {
                        // Release the abandoned walk's element retains.
                        drop(crate::ax::cache::CachedSnapshot::from_nodes(&first.nodes));
                        (walk(budget_ms), Some(first.walk))
                    }
                    None => (first, None),
                };
                let owner = crate::ax::cache::CachedSnapshot::from_nodes(&tree.nodes);
                (tree, owner, first_walk)
            });
            let retry_ms = cua_driver_core::walk_budget::RETRY_CAP_MS.min(timeout_ms.saturating_mul(4));
            let backstop = std::time::Duration::from_millis(timeout_ms + retry_ms)
                + AX_WALK_BACKSTOP_GRACE;
            match tokio::time::timeout(backstop, walk_future).await {
                Ok(Ok((tree, owner, first))) => {
                    walk_owner = Some(owner);
                    first_walk = first;
                    Some(tree)
                }
                Ok(Err(e)) => return ToolResult::error(format!("AX tree walk failed: {e}")),
                Err(_elapsed) => {
                    return ToolResult::error(format!(
                        "AX tree walk for pid={pid} did not return within {} s: an \
                         accessibility call stopped answering past the {timeout_ms} ms \
                         timeout_ms budget. Retry, or act by pixel (x,y) off a \
                         screenshot-only get_window_state (include_accessibility_tree:false).",
                        backstop.as_secs()
                    ));
                }
            }
        } else {
            None
        };

        // The window can close, or its CGWindow can be re-parented onto another
        // process, between the pre-flight and the walk. Re-apply the same
        // refusals against what the walk actually observed.
        let window_scope = tree_result.as_ref().and_then(|r| r.window_scope.clone());
        if let Some(ref scope) = window_scope {
            if let Some(refusal) = window_scope_refusal(pid, window_id, scope) {
                return refusal;
            }
        }
        // `window_scope` is None only when no window_id was requested, which
        // this tool never does — so treat that as resolved.
        let scope_matched = window_scope.as_ref().is_none_or(|s| s.is_matched());

        if !scope_matched && !observation_only {
            self.state.element_cache.remove(pid, u64::from(window_id));
        }

        // Capture the screenshot and deliver it alongside the tree — the
        // grounding frame the agent cross-checks the (sometimes-lying) tree
        // against. Skipped only when `include_screenshot:false` (and no
        // screenshot_out_file). With `screenshot_out_file` set, write to disk and
        // surface the path instead of embedding base64; otherwise embed base64.
        // The portable `max_image_dimension` is an explicit per-call override,
        // including 0 for native resolution. Without it, preserve the existing
        // configured ceiling and legacy `max_dimension` tighter-cap behavior.
        let max_dim = cua_driver_core::image_utils::ImageDimensionLimits {
            configured: effective_max_dim,
            legacy_max_dimension: max_dimension,
            max_image_dimension,
        }
        .resolve();
        // Returns the exact delivered PNG bytes, optional file path, delivered
        // and native dimensions, the WindowServer bounds it was validated
        // against, and the raw capture's backing scale.
        let mut screenshot_frame_error = None;
        let mut screenshot_resize_scale = None;
        let screenshot = if should_capture {
            let out_file = screenshot_out_file.clone();
            let res = tokio::task::spawn_blocking(move || -> Result<
                (
                    Vec<u8>,
                    Option<String>,
                    u32,
                    u32,
                    u32,
                    u32,
                    crate::windows::WindowBounds,
                    f64,
                ),
                super::px_frame::PxFrameError,
            > {
                let bounds = crate::windows::window_bounds_by_id(window_id)
                    .filter(|b| b.width > 0.0 && b.height > 0.0)
                    .ok_or(super::px_frame::PxFrameError::WindowNotFound { window_id })?;
                let raw = crate::capture::screenshot_window_bytes(window_id).map_err(|e| {
                    super::px_frame::PxFrameError::CaptureUnavailable {
                        window_id,
                        reason: e.to_string(),
                    }
                })?;
                let (orig_w, orig_h) = crate::capture::png_dimensions(&raw).map_err(|e| {
                    super::px_frame::PxFrameError::CaptureUnavailable {
                        window_id,
                        reason: e.to_string(),
                    }
                })?;
                let scale =
                    super::px_frame::validate_capture_frame(window_id, &bounds, orig_w, orig_h)?;
                let png = crate::capture::resize_png_if_needed(&raw, max_dim).map_err(|e| {
                    super::px_frame::PxFrameError::CaptureUnavailable {
                        window_id,
                        reason: e.to_string(),
                    }
                })?;
                let (w, h) = crate::capture::png_dimensions(&png).map_err(|e| {
                    super::px_frame::PxFrameError::CaptureUnavailable {
                        window_id,
                        reason: e.to_string(),
                    }
                })?;
                if let Some(ref path) = out_file {
                    std::fs::write(path, &png).map_err(|e| {
                        super::px_frame::PxFrameError::CaptureUnavailable {
                            window_id,
                            reason: e.to_string(),
                        }
                    })?;
                    Ok((
                        png,
                        Some(path.clone()),
                        w,
                        h,
                        orig_w,
                        orig_h,
                        bounds,
                        scale,
                    ))
                } else {
                    Ok((
                        png,
                        None,
                        w,
                        h,
                        orig_w,
                        orig_h,
                        bounds,
                        scale,
                    ))
                }
            }).await;
            match res {
                Ok(Ok((png, file_path, w, h, orig_w, orig_h, bounds, scale))) => {
                    if !observation_only {
                        screenshot_resize_scale = Some(orig_w as f64 / w as f64);
                    }
                    Some((png, file_path, w, h, orig_w, orig_h, bounds, scale))
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        "Screenshot frame could not be verified for window {window_id}: {e:?}"
                    );
                    screenshot_frame_error = Some(e);
                    None
                }
                Err(e) => {
                    tracing::warn!("Screenshot task error for window {window_id}: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Capture screenshot dimensions before consuming.
        let screenshot_dims = screenshot.as_ref().map(|(_, _, w, h, _, _, _, _)| (*w, *h));
        let screenshot_file_path = screenshot
            .as_ref()
            .and_then(|(_, fp, _, _, _, _, _, _)| fp.clone());
        let screenshot_frame = screenshot
            .as_ref()
            .map(|(_, _, _, _, _, _, bounds, scale)| (bounds.clone(), *scale));

        // Number rows the way the previous look at this window did
        // (`diff::assign_stable_indices`) and, after that first look, send only
        // what changed (`diff::diff_outline`). The previous snapshot lends its
        // element handles and rendered rows; the walk owner is renumbered in
        // place and replaces it when published below.
        //
        // This runs after the capture on purpose: whether a diff is safe
        // depends on the screenshot transform actually delivered, not the one
        // requested, since omitted rows keep their previous screenshot_frame.
        //
        // The per-window lock stays held until that publication: two
        // overlapping looks must not both hand `next_id` to different new rows.
        let look_lock = self.state.look_lock(pid, u64::from(window_id));
        let _look_guard = look_lock.lock().await;
        let prior = self
            .state
            .element_cache
            .with_latest_payload(pid, u64::from(window_id), |p| p.prior_look());
        let mut outline_diff: Option<crate::ax::diff::OutlineDiff> = None;
        let prepared_snapshot = tree_result.as_mut().map(|r| {
            let screenshot_transform = match (screenshot_frame.as_ref(), screenshot_dims) {
                (Some((bounds, _)), Some((width, _))) if bounds.width > 0.0 => {
                    Some(crate::ax::cache::ScreenshotTransform::new(
                        (bounds.x, bounds.y),
                        f64::from(width) / bounds.width,
                    ))
                }
                _ => None,
            };
            let bounds = crate::ax::cache::LookBounds {
                max_elements,
                max_depth,
                screenshot: screenshot_transform,
                element_fields,
            };
            let mut next_id = prior.as_ref().map_or(0, |p| p.next_id);
            match prior.as_ref() {
                Some(p) => {
                    let pairs = p.identity_pairs();
                    // Safety: `p` retains every pointer in `pairs` and outlives
                    // the matcher.
                    let mut same = unsafe { crate::ax::diff::identity_matcher(&pairs) };
                    crate::ax::diff::assign_stable_indices(&mut r.nodes, |ptr| same(ptr), &mut next_id);
                }
                None => crate::ax::diff::assign_stable_indices(&mut r.nodes, |_| None, &mut next_id),
            }
            // Numbers may have changed; re-render the full outline.
            r.tree_markdown = crate::ax::tree::render_outline(&r.nodes, tree_query(query.as_deref(), query_context), &r.walk);
            // See `diff_baseline` for when a previous look can anchor a diff.
            //
            // ponytail: numbering history lives in the snapshot payload, so the
            // per-pid LRU (8 windows) or session retirement drops it; the next
            // look is then a fresh full outline numbered from 0, which the
            // caller sees whole. Keep a separate history map if agents start
            // juggling more windows per app than that.
            let comparable = diff_baseline(prior.as_ref(), &bounds, &session_id);
            if let (true, Some(p)) = (want_diff, comparable) {
                let title = r
                    .nodes
                    .iter()
                    .find(|n| n.role == "AXWindow")
                    .and_then(|n| n.title.clone())
                    .unwrap_or_default();
                let d = crate::ax::diff::diff_outline(&p.rows, &r.nodes, &title);
                if d.markdown.len() < r.tree_markdown.len() {
                    r.tree_markdown = d.markdown.clone();
                    outline_diff = Some(d);
                }
            }
            let mut owner = walk_owner
                .take()
                .expect("walk owner accompanies every tree result");
            owner.renumber(&r.nodes, next_id, bounds, session_id.clone(), query.is_none());
            owner
        });

        let element_count = tree_result
            .as_ref()
            .map(|r| r.nodes.iter().filter(|n| n.element_index.is_some()).count())
            .unwrap_or(0);
        // A screenshot-only look publishes no actionable rows, but it must not
        // erase the window's numbering history: the next tree look would start
        // at zero and hand a vanished row's number to another control.
        let snapshot_payload = prepared_snapshot.or_else(|| {
            screenshot_resize_scale.is_some().then(|| match prior {
                Some(p) => p.into_history_payload(),
                None => crate::ax::cache::CachedSnapshot::from_nodes(&[]),
            })
        });
        let snapshot_id = snapshot_payload
            .filter(|_| scope_matched && !observation_only)
            .and_then(|payload| {
                self.state.element_cache.publish_for_session(
                    pid,
                    u64::from(window_id),
                    payload,
                    session_id.as_deref(),
                    screenshot_resize_scale,
                )
            });
        if let Some(snapshot_id) = snapshot_id {
            self.state
                .zoom_registry
                .retire_replaced(pid, u64::from(window_id), snapshot_id);
        }
        let snapshot_handle = snapshot_id.map(|sid| {
            cua_driver_core::element_token::token_for(sid, 0)
                .trim_end_matches(":0")
                .to_string()
        });
        // Without element records the tree is the only index, so it says how
        // to form a token from any row.
        if let (ElementFields::None, Some(handle), Some(r)) = (
            element_fields,
            snapshot_handle.as_deref(),
            tree_result.as_mut(),
        ) {
            prepend_token_hint(&mut r.tree_markdown, handle);
        }

        // Build response.
        let mut content: Vec<Content> = Vec::new();

        if let Some((png, ref file_path, w, h, _, _, _, _)) = screenshot.as_ref() {
            if file_path.is_none() {
                use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
                content.push(Content::image_png(BASE64.encode(png)));
            }

            // Summary text line (matching Swift reference format).
            let element_count = tree_result
                .as_ref()
                .map(|r| r.nodes.iter().filter(|n| n.element_index.is_some()).count())
                .unwrap_or(0);
            let summary = if let Some(ref r) = tree_result {
                format!(
                    "window_id={window_id} pid={pid} size={}x{} elements={element_count}\n\n{}",
                    w, h, r.tree_markdown
                )
            } else {
                format!("window_id={window_id} pid={pid} size={}x{}", w, h)
            };
            content.push(Content::text(summary));
        } else if let Some(ref r) = tree_result {
            let element_count = r.nodes.iter().filter(|n| n.element_index.is_some()).count();
            content.push(Content::text(format!(
                "window_id={window_id} pid={pid} elements={element_count}\n\n{}",
                r.tree_markdown
            )));
        }

        if content.is_empty() {
            return ToolResult::error(
                "No content produced (neither AX tree nor screenshot succeeded)",
            );
        }

        let tree_md = tree_result
            .as_ref()
            .map(|r| r.tree_markdown.clone())
            .unwrap_or_default();

        let capture_id = match (snapshot_id, screenshot.as_ref()) {
            (Some(_), Some((png, _, width, height, native_width, native_height, _, _))) => {
                match self.state.capture_bindings.publish_window(
                    &args,
                    pid,
                    window_id,
                    png.clone(),
                    (*width, *height),
                    (*native_width, *native_height),
                ) {
                    Ok(capture_id) => Some(capture_id),
                    Err(error) => return error,
                }
            }
            _ => None,
        };

        // Build the structured `elements` array — one entry per actionable
        // node, matching the order (and indices) of the markdown rendering.
        // This is the preferred consumption path; `tree_markdown` is kept
        // alongside for back-compat with existing text-parsing callers
        // (Hermes' regex parser, Codex, Claude Code) and is signalled as
        // preferred-for-back-compat-only via the `_note` field below.
        let elements_json: Vec<serde_json::Value> = match (snapshot_id, tree_result.as_ref()) {
            (Some(sid), Some(r)) => build_elements_array_with_token(&r.nodes, Some(sid)),
            (None, Some(r)) if scope_matched && observation_only => {
                build_observation_elements_array(&r.nodes)
            }
            (None, Some(r)) if scope_matched => build_elements_array_with_token(&r.nodes, None),
            _ => Vec::new(),
        };
        // The same selection as the outline, from the walker's parent edges,
        // never re-parsed from the rendered text.
        let elements_json = match (tree_query(query.as_deref(), query_context), tree_result.as_ref()) {
            (Some(q), Some(r)) => {
                let kept: std::collections::HashSet<usize> = crate::ax::tree::query_positions(&r.nodes, q)
                    .into_iter()
                    .filter_map(|p| r.nodes[p].element_index)
                    .collect();
                elements_json
                    .into_iter()
                    .filter(|e| {
                        e.get("element_index")
                            .and_then(|v| v.as_u64())
                            .is_some_and(|i| kept.contains(&(i as usize)))
                    })
                    .collect()
            }
            _ => elements_json,
        };
        // Screenshot pixels of the delivered capture: window origin in screen
        // points, delivered pixels per point (backing scale x downsizing).
        let elements_json = match (screenshot_frame.as_ref(), screenshot_dims) {
            (Some((bounds, _)), Some((width, _))) if bounds.width > 0.0 => {
                cua_driver_core::element_frame::with_screenshot_frames(
                    elements_json,
                    (bounds.x, bounds.y),
                    f64::from(width) / bounds.width,
                )
            }
            _ => elements_json,
        };
        // In a diff response the structured side shrinks the same way as the
        // outline: only added or changed rows.
        let elements_json: Vec<serde_json::Value> = match outline_diff.as_ref() {
            Some(d) => {
                let touched = d.touched();
                elements_json
                    .into_iter()
                    .filter(|e| {
                        e.get("element_index")
                            .and_then(|v| v.as_u64())
                            .is_some_and(|i| touched.contains(&(i as usize)))
                    })
                    .collect()
            }
            None => elements_json,
        };
        let filtered_element_count = elements_json.len();
        // Public snapshots contain only actionable nodes. Observation-only
        // snapshots also retain display-only state, but AX child reads can
        // fail independently of the element/depth caps.
        // Until the walker exposes a proof over the projected search domain,
        // absence must remain unknown rather than being claimed complete.
        let elements_complete = false;

        let mut structured = serde_json::json!({
            "window_id": window_id,
            "pid": pid,
            "element_count": element_count,
            "total_element_count": element_count,
            "returned_element_count": filtered_element_count,
            "elements_complete": elements_complete,
            "tree_markdown": tree_md,
            "elements": elements_json,
            "_note": "Prefer `elements` — `tree_markdown` will continue to work \
                but new fields will only be added to the structured side. \
                Issue #22865: use `max_elements` / `max_depth` to bound the \
                AX walk on apps with very large trees."
        });
        project_elements(&mut structured, element_fields);
        if query.is_some() {
            structured["filtered_element_count"] = serde_json::json!(filtered_element_count);
        }
        if let Some(r) = tree_result.as_ref() {
            r.walk.apply(&mut structured);
            if let Some(first) = first_walk.as_ref() {
                r.walk.apply_retry_of(first, &mut structured);
            }
            let bounded = args.get("max_depth").is_some() || args.get("max_elements").is_some();
            if !bounded {
                if let Some(unexposed) = unexposed_web_content(pid, r) {
                    structured["web_content"] = unexposed;
                }
            }
        }
        if let Some(d) = outline_diff.as_ref() {
            structured["diff"] = serde_json::json!({
                "added": d.added,
                "changed": d.changed,
                "removed": d.removed,
                "note": "tree_markdown and elements list only rows added or changed since this \
                    session's previous get_window_state of this window. Unchanged rows keep \
                    their element_index; act on one by passing this response's snapshot_id \
                    together with its element_index (or element_token \
                    \"<snapshot_id>:<element_index>\"). Pass diff:false for the full outline."
            });
        }
        // Surface 6: an opaque snapshot identifier consumers can log
        // alongside the per-element tokens for debug correlation. Same value
        // embedded in every `element_token` emitted in `elements[]` above.
        // Additive — old consumers ignore it. Absent when no snapshot was
        // registered (unresolved window scope).
        if let Some(handle) = snapshot_handle {
            structured["snapshot_id"] = serde_json::json!(handle);
        }
        if let Some(capture_id) = capture_id {
            structured["capture_id"] = serde_json::json!(capture_id);
        }
        // Best-effort-background ladder, rung (2). Both rungs point the agent at
        // the same next move: an empty AX tree means element_index has nothing
        // to bind to, so the deliberate action is an element px action — read
        // the screenshot already in this response and click by pixel (x,y).
        // macOS can pixel-target in the background, so the recommendation is
        // `px`, not `foreground`.
        match degradation_for(tree_result.is_some(), element_count, window_scope.as_ref()) {
            Degradation::None => {}
            Degradation::AxTreeEmpty => {
                structured["degraded"] = serde_json::json!(true);
                structured["degraded_reason"] = serde_json::json!(
                    "ax_tree_empty: the AX walk returned no actionable elements. The \
                     window may be a non-AX surface (canvas/WebGL/custom-drawn) or its \
                     accessibility tree was not ready (Chromium/Electron require an \
                     AX-enable + settle). Do not treat element data as authoritative — \
                     re-snapshot if the app just launched, otherwise switch to the \
                     visual path."
                );
                structured["escalation"] = serde_json::json!({
                    "recommended": "px",
                    "reason": "non-AX surface — act by pixel (x,y) off the screenshot \
                               in this response (an element px action)."
                });
            }
            Degradation::AxWindowUnresolved { ax_window_count } => {
                structured["degraded"] = serde_json::json!(true);
                structured["degraded_reason"] = serde_json::json!(format!(
                    "ax_window_unresolved: window_id {window_id} exists and is owned by \
                     pid {pid}, but none of the {ax_window_count} AXWindow element(s) \
                     under that pid reports this CGWindowID. The tree is returned EMPTY \
                     on purpose: the accessibility elements reachable under this pid \
                     belong to other surfaces (the menu bar, other windows), not to the \
                     requested window, so presenting them would misground the next \
                     action."
                ));
                structured["escalation"] = serde_json::json!({
                    "recommended": "foreground",
                    "reason": "observation-only: the screenshot in this response IS the \
                               requested window, but background input (including px) is \
                               refused while its AX surface is unresolved — events could \
                               reach a same-process sibling window. Re-snapshot after the \
                               app settles, or act with delivery_mode:\"foreground\"."
                });
            }
        }
        // Additive read-only `background_input` capability section (macOS
        // background input v1): the same fresh facts that gate every
        // background mutation, reported per route so an agent can choose
        // before acting. Every action still revalidates — this is advisory,
        // not a promise. Old consumers ignore the extra field.
        {
            let capture_available = screenshot_dims.is_some();
            let report = tokio::task::spawn_blocking(move || {
                let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, None);
                cua_driver_core::background_input::background_input_capability_report(
                    cua_driver_core::background_input::ExactWindowTarget { pid, window_id },
                    &facts,
                    Some(capture_available),
                )
            })
            .await;
            // Sent once per (session, window) while unchanged: every copy is
            // re-read by the model on every later turn. diff:false is a full
            // look and repeats it; verify_state's internal look neither shows
            // it to the model nor counts as sent.
            if let Ok(report) = report {
                let full_look = args.get("diff") == Some(&Value::Bool(false));
                let key = (session_id.clone(), pid, window_id);
                if observation_only
                    || background_input_is_news(
                        &mut self.state.background_input_sent.lock().unwrap(),
                        key,
                        &report,
                        full_look,
                    )
                {
                    structured["background_input"] = report;
                }
            }
        }
        if let Some((sw, sh)) = screenshot_dims {
            structured["screenshot_width"] = serde_json::json!(sw);
            structured["screenshot_height"] = serde_json::json!(sh);
            // Surface 7: emit an explicit `screenshot_mime_type` on the
            // structured payload so consumers don't have to sniff the magic
            // bytes off the base64 PNG (`iVBOR` = PNG, `/9j/` = JPEG) to
            // know what they're holding. `Content::image_png` already carries
            // `mimeType` on the protocol image part — this mirrors it onto
            // the structured side. Additive: keeps every existing field.
            structured["screenshot_mime_type"] = serde_json::json!("image/png");
        }
        if let Some((bounds, scale)) = screenshot_frame {
            structured["window_bounds"] = serde_json::json!({
                "x": bounds.x,
                "y": bounds.y,
                "width": bounds.width,
                "height": bounds.height
            });
            structured["screenshot_scale"] = serde_json::json!(scale);
            structured["screenshot_frame_valid"] = serde_json::json!(true);
        }
        if let Some(error) = screenshot_frame_error {
            structured["screenshot_frame_valid"] = serde_json::json!(false);
            structured["screenshot_error"] = super::px_frame::error_structured(&error);
        }
        if let Some(ref fp) = screenshot_file_path {
            structured["screenshot_file_path"] = serde_json::json!(fp);
        }
        // Window identity metadata (additive): the owning app and the window's
        // title for the requested window_id. A cheap WindowServer lookup that
        // names the surface even on the capture-only path, where no AX tree is
        // present to identify it. Omitted per-field when WindowServer reports an
        // empty string.
        if let Some(info) = crate::windows::window_info_by_id(window_id) {
            if !info.app_name.is_empty() {
                structured["app_name"] = serde_json::json!(info.app_name);
            }
            if !info.title.is_empty() {
                structured["window_title"] = serde_json::json!(info.title);
            }
        }
        cua_driver_core::window_inspection::mark_browser_chrome_capture_coverage(
            &mut structured,
            chromium_browser_window(pid).then_some(
                cua_driver_core::window_inspection::BrowserChromeCaptureCoverage::MayBeIncomplete,
            ),
        );
        ToolResult {
            content,
            is_error: None,
            structured_content: Some(structured),
            action_record: None,
        }
    }
}

/// The (pid, window_id) this call reads: given directly, or the only window of
/// `app` that the default `list_windows` would show.
async fn window_target(args: &Value) -> Result<(i32, u32), ToolResult> {
    use cua_driver_core::tool_args::ArgsExt;
    let app = args.opt_str("app");
    cua_driver_contract::window_state_target_form(
        app.as_deref(),
        args.get("pid").is_some(),
        args.get("window_id").is_some(),
    )
    .map_err(ToolResult::error)?;
    let Some(app) = app else {
        return Ok((args.require_i32("pid")?, args.require_u32("window_id")?));
    };
    let wanted = app.clone();
    let (apps, windows, ax_titles) = tokio::task::spawn_blocking(move || {
        let apps = crate::apps::list_running_apps();
        let ax_titles = ax_window_titles(&app_pids(&wanted, &apps));
        (apps, crate::windows::visible_windows(), ax_titles)
    })
    .await
    .map_err(|e| ToolResult::error(format!("app lookup failed: {e}")))?;
    select_app_window(&app, &apps, &windows, &ax_titles)
}

/// Private argument that carries a failed `app` lookup from `resolve_target`
/// to `invoke`. Clients cannot send it: the registry strips underscore
/// arguments before `resolve_target` runs.
const APP_REFUSAL_ARG: &str = "_app_resolution_refusal";

fn stored_refusal(refusal: ToolResult) -> Value {
    let message = refusal.content.iter().find_map(|c| match c {
        Content::Text { text, .. } => Some(text.clone()),
        _ => None,
    });
    serde_json::json!({"message": message, "structured": refusal.structured_content})
}

/// The target `invoke` reads. `app` is resolved only by `resolve_target`,
/// before authorization; here it is either that stored refusal or, on a path
/// that skipped resolution, refused outright.
async fn invoke_target(args: &Value) -> Result<(i32, u32), ToolResult> {
    if let Some(stored) = args.get(APP_REFUSAL_ARG) {
        let message = stored["message"].as_str().unwrap_or("app lookup failed");
        let refusal = ToolResult::error(message);
        return Err(match stored.get("structured").filter(|s| !s.is_null()) {
            Some(structured) => refusal.with_structured(structured.clone()),
            None => refusal,
        });
    }
    if args.get("app").is_some() {
        return Err(ToolResult::error(
            "app was not resolved before authorization; call list_windows and pass \
             pid + window_id.",
        )
        .with_structured(serde_json::json!({
            "code": "app_not_resolved",
            "suggestion": "call list_windows and pass pid + window_id"
        })));
    }
    window_target(args).await
}

/// Pids of the running apps whose name (any case) or exact bundle id is `app`.
fn app_pids(app: &str, apps: &[crate::apps::AppInfo]) -> Vec<i32> {
    let app = app.trim();
    let wanted = app.to_lowercase();
    apps.iter()
        .filter(|a| a.name.to_lowercase() == wanted || a.bundle_id.as_deref() == Some(app))
        .map(|a| a.pid)
        .collect()
}

/// CG window id -> AXTitle ("" when it has none) for every AX window of `pids`.
/// Empty when Accessibility is not granted. The whole lookup is bounded by
/// `AX_LOOKUP_BUDGET`; a pid not finished in time contributes nothing, so its
/// windows fall back to CG titles.
fn ax_window_titles(pids: &[i32]) -> HashMap<u32, String> {
    use crate::ax::bindings::{
        ax_get_window_id_checked, copy_ax_windows, copy_string_attr, AXUIElementCreateApplication,
        AXUIElementRef, AXUIElementSetMessagingTimeout,
    };
    use core_foundation::base::{CFRelease, CFTypeRef};
    let deadline = std::time::Instant::now() + AX_LOOKUP_BUDGET;
    // AX messaging timeouts are per element, so each one gets what is left.
    let bound = |element: AXUIElementRef| -> bool {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let _ = unsafe { AXUIElementSetMessagingTimeout(element, left.as_secs_f32().max(0.05)) };
        true
    };
    let mut titles = HashMap::new();
    for &pid in pids {
        let mut reads = Vec::new();
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                continue;
            }
            let windows = if bound(app) {
                copy_ax_windows(app)
            } else {
                Vec::new()
            };
            for window in windows {
                let read = if bound(window) {
                    ax_get_window_id_checked(window)
                        .map(|id| {
                            id.map(|id| {
                                (id, copy_string_attr(window, "AXTitle").unwrap_or_default())
                            })
                        })
                        .map_err(drop)
                } else {
                    Err(())
                };
                reads.push(read);
                CFRelease(window as CFTypeRef);
            }
            CFRelease(app as CFTypeRef);
        }
        let in_time = std::time::Instant::now() < deadline;
        titles.extend(complete_ax_windows(reads, in_time).unwrap_or_default());
    }
    titles
}

/// One process's AX windows, or `None` when any window id read failed or the
/// deadline cut the lookup short. Partial data would hide that process's
/// other windows, so it falls back to CG titles instead. `Ok(None)` is a
/// window with no CG window (not composited) and is simply skipped.
fn complete_ax_windows(
    reads: Vec<Result<Option<(u32, String)>, ()>>,
    in_time: bool,
) -> Option<Vec<(u32, String)>> {
    if !in_time {
        return None;
    }
    reads
        .into_iter()
        .filter_map(Result::transpose)
        .collect::<Result<_, _>>()
        .ok()
}

/// Pick `app`'s window among `windows` (the default `list_windows` set:
/// current Space, on screen, layer 0). `ax_titles` maps CG window ids to the
/// app's AX windows and their AXTitle. A window with an AX window is a
/// candidate, titled from AX (CG titles are redacted without Screen
/// Recording); surfaces with none, such as Chrome's toolbar strips, are not.
/// For a process with no AX windows (Accessibility not granted, or the lookup
/// timed out), its titled CG windows are the candidates. A process with
/// on-screen windows and no candidate refuses the whole lookup, and several
/// candidates are refused rather than guessed.
fn select_app_window(
    app: &str,
    apps: &[crate::apps::AppInfo],
    windows: &[crate::windows::WindowInfo],
    ax_titles: &HashMap<u32, String>,
) -> Result<(i32, u32), ToolResult> {
    let app = app.trim();
    let pids = app_pids(app, apps);
    if pids.is_empty() {
        return Err(ToolResult::error(format!(
            "no running app named \"{app}\" (match the app name or bundle id from list_apps). \
             Call launch_app, or list_windows to find the window."
        ))
        .with_structured(serde_json::json!({
            "code": "app_not_running",
            "app": app,
            "suggestion": "call launch_app, or list_windows to find the window"
        })));
    }
    let app_windows: Vec<&crate::windows::WindowInfo> =
        windows.iter().filter(|w| pids.contains(&w.pid)).collect();
    // Per process: one instance's AX windows must not hide another instance
    // whose AX lookup failed or timed out.
    let ax_pids: Vec<i32> = app_windows
        .iter()
        .filter(|w| ax_titles.contains_key(&w.window_id))
        .map(|w| w.pid)
        .collect();
    let mut candidates: Vec<(&crate::windows::WindowInfo, &str)> = app_windows
        .iter()
        .filter_map(|w| {
            let ax_title = ax_titles.get(&w.window_id).map(|t| t.trim());
            let title = ax_title.filter(|t| !t.is_empty()).unwrap_or(w.title.trim());
            match ax_title {
                Some(_) => Some((*w, title)),
                None if !ax_pids.contains(&w.pid) && !title.is_empty() => Some((*w, title)),
                None => None,
            }
        })
        .collect();
    // A process with on-screen windows but no candidate could hide the real
    // target, so no other instance's window is picked in its place.
    let mut unidentified: Vec<i32> = app_windows
        .iter()
        .map(|w| w.pid)
        .filter(|pid| !candidates.iter().any(|(w, _)| w.pid == *pid))
        .collect();
    unidentified.sort_unstable();
    unidentified.dedup();
    if !unidentified.is_empty() {
        let pid_list: Vec<String> = unidentified.iter().map(i32::to_string).collect();
        return Err(ToolResult::error(format!(
            "\"{app}\" has on-screen windows that could not be identified (pid {}). This may \
             be a permission limit (Accessibility or Screen Recording not granted; see \
             check_permissions). Call list_windows and pass pid + window_id.",
            pid_list.join(", ")
        ))
        .with_structured(serde_json::json!({
            "code": "app_window_unidentified",
            "app": app,
            "pids": unidentified,
            "suggestion": "call check_permissions, or list_windows and pass pid + window_id"
        })));
    }
    candidates.sort_by(|a, b| b.0.z_index.cmp(&a.0.z_index));
    match candidates.as_slice() {
        [(only, _)] => Ok((only.pid, only.window_id)),
        [] => Err(ToolResult::error(format!(
            "\"{app}\" has no on-screen window on the current Space. Call launch_app to \
             open one, or list_windows to find it."
        ))
        .with_structured(serde_json::json!({
            "code": "app_window_not_found",
            "app": app,
            "suggestion": "call launch_app, or list_windows to find the window"
        }))),
        several => {
            let lines: Vec<String> = several
                .iter()
                .map(|(w, title)| format!("window_id {} (pid {}): {title}", w.window_id, w.pid))
                .collect();
            Err(ToolResult::error(format!(
                "\"{app}\" has {} windows on the current Space; pass pid + window_id for one:\n{}",
                several.len(),
                lines.join("\n")
            ))
            .with_structured(serde_json::json!({
                "code": "app_window_ambiguous",
                "app": app,
                "candidates": several
                    .iter()
                    .map(|(w, title)| serde_json::json!({
                        "window_id": w.window_id,
                        "pid": w.pid,
                        "title": title
                    }))
                    .collect::<Vec<_>>(),
                "suggestion": "pass pid + window_id for one of the candidates"
            })))
        }
    }
}

/// (session, pid, window_id): the scope a `background_input` report is sent
/// once for. The session is the runtime `_session_id` the registry injects
/// (the transport's implicit session, or the named one), the same key the
/// snapshot diff baseline uses, so a second client still gets its own copy.
pub(crate) type BackgroundInputKey = (Option<String>, i32, u32);

pub(crate) type BackgroundInputSent = std::sync::Mutex<HashMap<BackgroundInputKey, Value>>;

/// Forget what an ended session was sent, so a session restarted under the
/// same name gets `background_input` again on its first read.
pub(crate) fn retire_background_input(sent: &BackgroundInputSent, session_id: &str) {
    sent.lock()
        .unwrap()
        .retain(|(session, _, _), _| session.as_deref() != Some(session_id));
}

/// Whether this look should carry `report`: the first for its key, a changed
/// report, or a full look. Records what was sent.
fn background_input_is_news(
    sent: &mut HashMap<BackgroundInputKey, Value>,
    key: BackgroundInputKey,
    report: &Value,
    full_look: bool,
) -> bool {
    if !full_look && sent.get(&key) == Some(report) {
        return false;
    }
    // ponytail: wholesale reset bounds memory across closed windows; each key
    // then re-sends once. Ended sessions are retired by the session-end hook.
    if sent.len() >= 512 && !sent.contains_key(&key) {
        sent.clear();
    }
    sent.insert(key, report.clone());
    true
}

/// Turn an unresolvable window scope into a structured refusal, or `None` when
/// the scope is one the caller can still be served (issue #2237).
///
/// Refusing is the point: the pre-fix behaviour returned the app's menu bar
/// under the requested `window_id`, which reads as a healthy snapshot and gets
/// clicked by `element_index`. Both refusals name the exact retry, matching the
/// remedy-in-the-refusal shape the rest of the driver uses.
///
/// The owner pid is REPORTED, not followed: `element_cache`, the element-token
/// registry and snapshot-owned screenshot transform are keyed on the caller-supplied pid, so
/// walking under `owner_pid` while echoing the requested pid would hand back
/// indices the caller replays against the wrong key. One retry with the named
/// pid is correct and cheap.
fn window_scope_refusal(
    pid: i32,
    window_id: u32,
    scope: &crate::ax::WindowScope,
) -> Option<ToolResult> {
    use crate::ax::WindowScope;
    match scope {
        // Resolved, or resolvable-as-degraded — the caller gets a response.
        WindowScope::Matched | WindowScope::AxUnresolved { .. } => None,
        WindowScope::NotFound => Some(
            ToolResult::error(format!(
                "window_id {window_id} is not a live window (closed, or the id is stale). \
                 Refusing to return an accessibility tree, because the elements reachable \
                 under pid {pid} belong to other surfaces — not to the window you asked \
                 for. Call list_windows for current window_ids."
            ))
            .with_structured(serde_json::json!({
                "code": "window_id_not_found",
                "pid": pid,
                "window_id": window_id,
                "suggestion": "call list_windows for current window_ids; the window may have closed"
            })),
        ),
        WindowScope::OwnerPidMismatch {
            owner_pid,
            owner_app_name,
        } => Some(
            ToolResult::error(format!(
                "window_id {window_id} is owned by pid {owner_pid} (\"{owner_app_name}\"), \
                 not pid {pid}. macOS hosts a sandboxed app's Open/Save panel \
                 out-of-process, so the panel's CGWindowID belongs to the panel service \
                 rather than the app that opened it. Refusing to return pid {pid}'s \
                 accessibility tree for it. Re-call get_window_state with pid={owner_pid} \
                 and the same window_id."
            ))
            .with_structured(serde_json::json!({
                "code": "window_owner_pid_mismatch",
                "pid": pid,
                "window_id": window_id,
                "owner_pid": owner_pid,
                "owner_app_name": owner_app_name,
                "suggestion": format!(
                    "window_id {window_id} is owned by pid {owner_pid}, not pid {pid} \
                     (macOS hosts sandboxed Open/Save panels out-of-process). Re-call \
                     get_window_state with pid={owner_pid} and the same window_id."
                )
            })),
        ),
    }
}

/// Which degradation rung a snapshot lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Degradation {
    /// Clean snapshot — no `degraded` field is emitted.
    None,
    /// A walk ran and produced no actionable elements.
    AxTreeEmpty,
    /// The requested window is live and owned by this pid, but no AXWindow
    /// claims its CGWindowID, so the walk deliberately covered nothing.
    AxWindowUnresolved { ax_window_count: usize },
}

/// Decide the degradation rung. Pure: `walk_attempted` is false in the
/// screenshot-only path (an empty tree is expected there, not degraded), and
/// the unresolved-scope rung outranks the generic empty-tree rung because it
/// explains *why* the tree is empty.
fn degradation_for(
    walk_attempted: bool,
    element_count: usize,
    scope: Option<&crate::ax::WindowScope>,
) -> Degradation {
    if !walk_attempted {
        return Degradation::None;
    }
    if let Some(crate::ax::WindowScope::AxUnresolved { ax_window_count }) = scope {
        return Degradation::AxWindowUnresolved {
            ax_window_count: *ax_window_count,
        };
    }
    if element_count == 0 {
        return Degradation::AxTreeEmpty;
    }
    Degradation::None
}

/// Render the actionable nodes from the AX walk into the
/// `structuredContent.elements` array shape described on the tool: one entry
/// per node with an `element_index`, carrying role, label (built from
/// title/description/value/identifier), frame, parent_index, depth, and —
/// Surface 6 — an opaque `element_token` for the same row.
///
/// Order matches the markdown rendering exactly (DFS, same indices). Only
/// nodes that received an `element_index` (i.e. are addressable via
/// click(element_index=N)) appear — non-actionable display-only rows are
/// omitted to match the contract on the tool description.
pub(crate) fn build_elements_array_with_token(
    nodes: &[crate::ax::tree::AXNode],
    snapshot_id: Option<u32>,
) -> Vec<serde_json::Value> {
    build_elements_array(nodes, snapshot_id, false)
}

/// Verification observes display-only state without creating action tokens or
/// changing the public actionable projection and its existing snapshot cache.
/// A row's human-readable name: title, then description, then a non-blank
/// value, then the placeholder hint, then the identifier, trimmed.
fn derive_label(node: &crate::ax::tree::AXNode) -> Option<String> {
    let nonblank = |text: &Option<String>| {
        text.as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    nonblank(&node.title)
        .or_else(|| nonblank(&node.description))
        .or_else(|| nonblank(&node.value))
        .or_else(|| nonblank(&node.placeholder))
        .or_else(|| nonblank(&node.identifier))
}

fn build_observation_elements_array(nodes: &[crate::ax::tree::AXNode]) -> Vec<serde_json::Value> {
    build_elements_array(nodes, None, true)
}

fn build_elements_array(
    nodes: &[crate::ax::tree::AXNode],
    snapshot_id: Option<u32>,
    include_display_only: bool,
) -> Vec<serde_json::Value> {
    nodes
        .iter()
        .filter_map(|node| {
            if node.element_index.is_none() && !include_display_only {
                return None;
            }
            // `label` is a best-effort human-readable string: title first,
            // then description, then value, then identifier. Mirrors what
            // a human reading the markdown row would call this element.
            let label = derive_label(node);
            let frame = node
                .frame
                .map(|[x, y, w, h]| serde_json::json!({ "x": x, "y": y, "w": w, "h": h }));
            let mut entry = serde_json::json!({
                "role": node.role,
                "depth": node.depth,
            });
            // Surface 6: opaque token paired to the integer index.
            // Tools accept either; the token has explicit validity
            // (invalidated when the next snapshot supersedes this
            // one in the per-pid LRU). See cua-driver-core's
            // `element_token` module.
            if let Some(idx) = node.element_index {
                entry["element_index"] = serde_json::json!(idx);
                if let Some(sid) = snapshot_id {
                    entry["element_token"] =
                        serde_json::json!(cua_driver_core::element_token::token_for(sid, idx));
                }
            }
            if let Some(url) = &node.url {
                entry["url"] = serde_json::json!(url);
            }
            if let Some(label) = label {
                entry["label"] = serde_json::Value::String(label);
            }
            // Surface the element's AXValue separately from `label`. `label`
            // collapses title→description→value→identifier into one display
            // string, so on a control that has BOTH a title/description AND a
            // value (e.g. a "Compose message" text field holding typed text),
            // the value is shadowed and invisible to a caller reading the
            // structured side — it only showed up in `tree_markdown`, forcing a
            // markdown grep to verify what landed. Emit it explicitly so the
            // verify-then-escalate loop can read the typed text structurally.
            // `value_state` widens the string-only AXValue read to all CF
            // types (CFNumber sliders → "8", CFBoolean checkboxes/radios →
            // "1"/"0") — controls whose state was previously invisible here.
            // Falls back to `value` so the field never regresses for
            // string-valued elements.
            // value_state may be "" for an empty text field; keep it. Text
            // fields never fall back to `value`, which can hold the
            // placeholder hint rather than content.
            let fallback = (!crate::ax::tree::is_text_entry_role(&node.role))
                .then(|| node.value.clone().filter(|v| !v.is_empty()))
                .flatten();
            if let Some(value) = node.value_state.clone().or(fallback) {
                entry["value"] = serde_json::Value::String(value);
            }
            if let Some(placeholder) = &node.placeholder {
                entry["placeholder"] = serde_json::Value::String(placeholder.clone());
            }
            if let Some(settable) = node.value_settable {
                entry["value_settable"] = serde_json::Value::Bool(settable);
            }
            if let Some(focused) = node.focused {
                entry["focused"] = serde_json::json!(focused);
            }
            if let Some(selection) = &node.text_selection {
                entry["text_selection"] = serde_json::json!(selection);
            }
            if let Some(desc) = node.value_description.clone() {
                entry["value_description"] = serde_json::Value::String(desc);
            }
            // Only surface a real range: WebKit reports AXMinValue/AXMaxValue
            // as 0.0/0.0 on non-range controls (checkboxes, radios), which
            // would be pure noise on every two-state element.
            if let (Some(min), Some(max)) = (node.min_value, node.max_value) {
                if max > min {
                    entry["min"] = serde_json::json!(min);
                    entry["max"] = serde_json::json!(max);
                }
            }
            if let Some(enabled) = node.enabled {
                entry["enabled"] = serde_json::Value::Bool(enabled);
            }
            let selected = node.selected.or_else(|| {
                let role = node.role.to_ascii_lowercase();
                if role.contains("checkbox") || role.contains("radiobutton") {
                    node.value_state.as_deref().and_then(|value| match value {
                        "1" | "true" | "on" => Some(true),
                        "0" | "false" | "off" => Some(false),
                        _ => None,
                    })
                } else {
                    None
                }
            });
            if let Some(selected) = selected {
                entry["selected"] = serde_json::Value::Bool(selected);
            }
            if !node.actions.is_empty() {
                entry["actions"] = serde_json::json!(node.actions);
            }
            if node.in_web_content {
                entry["in_web_content"] = serde_json::Value::Bool(true);
            }
            if let Some(frame) = frame {
                entry["frame"] = frame;
            }
            if let Some(parent) = node.parent_element_index {
                entry["parent_index"] = serde_json::json!(parent);
            }
            Some(entry)
        })
        .collect()
}

/// The previous look a diff may be relative to: one this session actually
/// received in full, taken with the same bounds. Another session never saw
/// the outline the diff is relative to; a query look delivered only its
/// matches; different walk bounds would present bound differences as
/// application changes; a different screenshot transform would leave omitted
/// rows with frames for another image; a different `element_fields`
/// projection would leave unchanged rows with fields the caller never got.
fn diff_baseline<'a>(
    prior: Option<&'a crate::ax::cache::PriorLook>,
    bounds: &crate::ax::cache::LookBounds,
    session: &Option<String>,
) -> Option<&'a crate::ax::cache::PriorLook> {
    prior.filter(|p| {
        p.full_delivered
            && p.bounds == *bounds
            && p.session == *session
            && (!p.rows.indexed.is_empty() || !p.rows.display.is_empty())
    })
}

/// Apply the `element_fields` projection: "none" drops the records (and the
/// `_note` recommending them), "compact" trims them, "full" keeps them.
fn project_elements(structured: &mut Value, fields: ElementFields) {
    match fields {
        ElementFields::Full => {}
        ElementFields::Compact => compact_elements(structured),
        ElementFields::None => {
            if let Some(map) = structured.as_object_mut() {
                map.remove("elements");
                map.remove("_note");
            }
        }
    }
}

/// First line of a "none" tree: how to turn a row's `[index]` into a token.
fn prepend_token_hint(tree_markdown: &mut String, snapshot_handle: &str) {
    tree_markdown.insert_str(0, &format!("element_token = {snapshot_handle}:<index>\n"));
}

/// The `element_fields:"compact"` projection. Agents re-read every result on
/// later turns, so drop what they pay for without using: screen-point frames
/// (`screenshot_frame` is the pixel-action space, and `frame` misled agents
/// into clicking screen points as pixels), structure the markdown indentation
/// already shows, default states, and the `_note` boilerplate.
fn compact_elements(structured: &mut Value) {
    let Some(map) = structured.as_object_mut() else {
        return;
    };
    map.remove("_note");
    let Some(elements) = map.get_mut("elements").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in elements.iter_mut().filter_map(Value::as_object_mut) {
        for key in ["frame", "depth", "parent_index"] {
            entry.remove(key);
        }
        if entry.get("enabled") == Some(&Value::Bool(true)) {
            entry.remove("enabled");
        }
        if entry.get("selected") == Some(&Value::Bool(false)) {
            entry.remove("selected");
        }
    }
}

fn tree_query(text: Option<&str>, context: bool) -> Option<crate::ax::tree::Query<'_>> {
    text.map(|text| crate::ax::tree::Query { text, context })
}

#[cfg(test)]
mod window_scope_contract_tests {
    use super::*;
    use crate::ax::WindowScope;

    fn panel_mismatch() -> WindowScope {
        WindowScope::OwnerPidMismatch {
            owner_pid: 900,
            owner_app_name: "Open and Save Panel Service".into(),
        }
    }

    fn structured(result: ToolResult) -> serde_json::Value {
        assert_eq!(result.is_error, Some(true), "must be an error result");
        result
            .structured_content
            .expect("refusals carry structured content")
    }

    #[test]
    fn stale_window_id_is_a_structured_not_found() {
        let s = structured(
            window_scope_refusal(800, 67340, &WindowScope::NotFound).expect("must refuse"),
        );
        assert_eq!(s["code"], "window_id_not_found");
        assert_eq!(s["pid"], 800);
        assert_eq!(s["window_id"], 67340);
        assert!(s["suggestion"].as_str().unwrap().contains("list_windows"));
    }

    /// Issue #2237's reported case: TextEdit's Open panel window belongs to the
    /// out-of-process panel service. The refusal must name the real owner pid
    /// so the caller can retry, and must NOT redirect on its own (the element
    /// caches are keyed on the caller-supplied pid).
    #[test]
    fn owner_pid_mismatch_names_the_owner_and_the_retry() {
        let refusal = window_scope_refusal(800, 67340, &panel_mismatch()).expect("must refuse");
        let text = format!("{:?}", refusal.content);
        let s = structured(refusal);
        assert_eq!(s["code"], "window_owner_pid_mismatch");
        assert_eq!(s["owner_pid"], 900);
        assert_eq!(s["owner_app_name"], "Open and Save Panel Service");
        assert_eq!(s["pid"], 800, "the requested pid is echoed, not replaced");
        assert!(
            s["suggestion"].as_str().unwrap().contains("pid=900"),
            "the retry must name the owner pid: {}",
            s["suggestion"]
        );
        assert!(
            !text.contains("AXMenuBar"),
            "the refusal must never carry menu-bar content"
        );
    }

    #[test]
    fn resolvable_scopes_are_not_refused() {
        assert!(window_scope_refusal(800, 11, &WindowScope::Matched).is_none());
        assert!(
            window_scope_refusal(800, 11, &WindowScope::AxUnresolved { ax_window_count: 2 })
                .is_none(),
            "a live same-pid window degrades; it does not error"
        );
    }

    /// The reported failure signature: a wrong-surface walk returns a healthy
    /// non-zero element count, so the pre-fix `element_count == 0` rung stayed
    /// silent. An unresolved scope now degrades on its own evidence.
    #[test]
    fn unresolved_scope_degrades_with_its_own_reason() {
        assert_eq!(
            degradation_for(
                true,
                0,
                Some(&WindowScope::AxUnresolved { ax_window_count: 3 })
            ),
            Degradation::AxWindowUnresolved { ax_window_count: 3 }
        );
    }

    #[test]
    fn empty_tree_still_degrades_as_ax_tree_empty() {
        // Back-compat with the pre-existing rung.
        assert_eq!(
            degradation_for(true, 0, Some(&WindowScope::Matched)),
            Degradation::AxTreeEmpty
        );
    }

    #[test]
    fn resolved_window_with_elements_is_not_degraded() {
        assert_eq!(
            degradation_for(true, 42, Some(&WindowScope::Matched)),
            Degradation::None
        );
    }

    #[test]
    fn screenshot_only_path_does_not_degrade() {
        assert_eq!(degradation_for(false, 0, None), Degradation::None);
    }

    #[test]
    fn skill_documents_the_window_scope_error_codes() {
        // The tool description stays short; the tool reference in the skill
        // pack carries the error codes an agent needs to recover.
        let tools_md = include_str!("../../../../Skills/cua-driver/TOOLS.md");
        for code in [
            "window_id_not_found",
            "window_owner_pid_mismatch",
            "ax_window_unresolved",
        ] {
            assert!(tools_md.contains(code), "TOOLS.md must document {code}");
        }
        assert!(def().description.contains("skill://cua-driver/"));
    }

    /// The capture-only fold-in: get_window_state advertises the new
    /// `include_accessibility_tree` / `max_dimension` controls and documents
    /// the degenerate both-false case on the include_accessibility_tree
    /// property. pid + window_id are not schema-required because `app` is the
    /// other target form; the tool checks the form at runtime.
    #[test]
    fn schema_advertises_capture_only_controls() {
        let d = def();
        let props = &d.input_schema["properties"];
        assert!(
            props.get("include_accessibility_tree").is_some(),
            "schema must advertise include_accessibility_tree"
        );
        assert!(
            props.get("max_dimension").is_some(),
            "schema must advertise max_dimension"
        );
        assert_eq!(props["max_image_dimension"]["minimum"], 0);
        assert!(
            d.input_schema.get("required").is_none(),
            "pid + window_id and app are alternatives, so none is schema-required"
        );
        assert_eq!(props["app"]["type"], "string");
        assert!(
            props["include_accessibility_tree"]["description"]
                .as_str()
                .unwrap()
                .contains("include_screenshot:false"),
            "include_accessibility_tree must document the both-false error"
        );
    }
}

#[cfg(test)]
mod app_target_tests {
    use super::*;

    fn app(name: &str, pid: i32, bundle_id: &str) -> crate::apps::AppInfo {
        crate::apps::AppInfo {
            name: name.into(),
            pid,
            bundle_id: Some(bundle_id.into()),
            running: true,
            active: false,
            launch_path: None,
            kind: None,
            last_used: None,
        }
    }

    fn window(window_id: u32, pid: i32, title: &str, z_index: usize) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: String::new(),
            title: title.into(),
            bounds: crate::windows::WindowBounds {
                x: 0.,
                y: 0.,
                width: 800.,
                height: 600.,
            },
            layer: 0,
            z_index,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: Some(true),
            space_ids: None,
        }
    }

    fn refusal(result: Result<(i32, u32), ToolResult>) -> (String, Value) {
        let result = result.expect_err("must refuse");
        assert_eq!(result.is_error, Some(true));
        let text = format!("{:?}", result.content);
        (text, result.structured_content.expect("structured refusal"))
    }

    fn no_ax() -> HashMap<u32, String> {
        HashMap::new()
    }

    fn ax(entries: &[(u32, &str)]) -> HashMap<u32, String> {
        entries.iter().map(|(id, t)| (*id, t.to_string())).collect()
    }

    #[test]
    fn one_titled_window_is_the_target() {
        let apps = [
            app("Google Chrome", 10, "com.google.Chrome"),
            app("TextEdit", 20, "com.apple.TextEdit"),
        ];
        // Without AX windows, Chrome's untitled toolbar strips are not candidates.
        let windows = [
            window(1, 10, "", 3),
            window(2, 10, "Docs", 2),
            window(3, 20, "Untitled", 1),
        ];
        assert_eq!(
            select_app_window("google chrome", &apps, &windows, &no_ax()).unwrap(),
            (10, 2)
        );
        assert_eq!(
            select_app_window("com.apple.TextEdit", &apps, &windows, &no_ax()).unwrap(),
            (20, 3)
        );
        // Bundle ids match exactly, names ignore case only.
        assert!(select_app_window("com.apple.textedit", &apps, &windows, &no_ax()).is_err());
        assert!(select_app_window("Chrome", &apps, &windows, &no_ax()).is_err());
    }

    /// Without Screen Recording every CG title is empty; AX still names the
    /// real window, and toolbar strips have no AX window.
    #[test]
    fn redacted_cg_titles_resolve_through_ax_windows() {
        let apps = [app("Google Chrome", 10, "com.google.Chrome")];
        let windows = [
            window(1, 10, "", 3),
            window(2, 10, "", 2),
            window(5, 10, "", 1),
        ];
        assert_eq!(
            select_app_window("Google Chrome", &apps, &windows, &ax(&[(2, "Docs")])).unwrap(),
            (10, 2)
        );
        // An AX window with an empty AXTitle is still a real window.
        assert_eq!(
            select_app_window("Google Chrome", &apps, &windows, &ax(&[(5, "")])).unwrap(),
            (10, 5)
        );
    }

    #[test]
    fn titled_cg_surfaces_without_an_ax_window_are_ignored() {
        let apps = [app("Google Chrome", 10, "com.google.Chrome")];
        let windows = [window(1, 10, "Strip", 3), window(2, 10, "Docs", 2)];
        assert_eq!(
            select_app_window("Google Chrome", &apps, &windows, &ax(&[(2, "Docs")])).unwrap(),
            (10, 2)
        );
    }

    #[test]
    fn two_ax_windows_are_listed_with_ax_titles() {
        let apps = [app("TextEdit", 20, "com.apple.TextEdit")];
        let windows = [
            window(3, 20, "", 1),
            window(4, 20, "", 2),
            window(6, 20, "", 3),
        ];
        let (text, s) = refusal(select_app_window(
            "TextEdit",
            &apps,
            &windows,
            &ax(&[(3, "A.txt"), (4, "B.txt")]),
        ));
        assert_eq!(s["code"], "app_window_ambiguous");
        assert_eq!(
            s["candidates"],
            serde_json::json!([
                {"window_id": 4, "pid": 20, "title": "B.txt"},
                {"window_id": 3, "pid": 20, "title": "A.txt"}
            ])
        );
        assert!(text.contains("window_id 3 (pid 20): A.txt"), "{text}");
    }

    /// One instance's AX windows must not hide another instance whose AX
    /// lookup came back empty: its titled CG window still counts.
    #[test]
    fn ax_fallback_is_decided_per_process() {
        let apps = [
            app("TextEdit", 20, "com.apple.TextEdit"),
            app("TextEdit", 21, "com.apple.TextEdit"),
        ];
        let windows = [
            window(3, 20, "", 1),
            window(7, 20, "", 3),
            window(4, 21, "B.txt", 2),
        ];
        let (_, s) = refusal(select_app_window(
            "TextEdit",
            &apps,
            &windows,
            &ax(&[(3, "A.txt")]),
        ));
        assert_eq!(s["code"], "app_window_ambiguous");
        assert_eq!(
            s["candidates"],
            serde_json::json!([
                {"window_id": 4, "pid": 21, "title": "B.txt"},
                {"window_id": 3, "pid": 20, "title": "A.txt"}
            ])
        );
    }

    /// A process whose AX window-id reads did not all succeed has no AX data,
    /// so its titled CG windows still count.
    #[test]
    fn incomplete_ax_reads_fall_back_to_cg_titles() {
        let full = complete_ax_windows(vec![Ok(Some((3, "A.txt".into()))), Ok(None)], true);
        assert_eq!(full, Some(vec![(3, "A.txt".to_owned())]));
        assert_eq!(
            complete_ax_windows(vec![Ok(Some((3, "A.txt".into())))], false),
            None
        );
        let failed = complete_ax_windows(vec![Ok(Some((5, "C.txt".into()))), Err(())], true);
        assert_eq!(failed, None);

        let apps = [
            app("TextEdit", 20, "com.apple.TextEdit"),
            app("TextEdit", 21, "com.apple.TextEdit"),
        ];
        let windows = [
            window(3, 20, "", 1),
            window(5, 21, "C.txt", 2),
            window(6, 21, "D.txt", 3),
        ];
        let ax_titles: HashMap<u32, String> = full.into_iter().chain(failed).flatten().collect();
        let (_, s) = refusal(select_app_window("TextEdit", &apps, &windows, &ax_titles));
        assert_eq!(s["code"], "app_window_ambiguous");
        assert_eq!(
            s["candidates"],
            serde_json::json!([
                {"window_id": 6, "pid": 21, "title": "D.txt"},
                {"window_id": 5, "pid": 21, "title": "C.txt"},
                {"window_id": 3, "pid": 20, "title": "A.txt"}
            ])
        );
    }

    /// A lookup that failed before authorization is what invoke returns; it
    /// does not look again, so a window that appears or closes in between is
    /// never read unauthorized. (No such TextEdit windows exist live.)
    #[tokio::test]
    async fn invoke_returns_the_resolution_refusal_without_looking_again() {
        let apps = [app("TextEdit", 20, "com.apple.TextEdit")];
        let windows = [window(3, 20, "A.txt", 1), window(4, 20, "B.txt", 2)];
        let ambiguous = select_app_window("TextEdit", &apps, &windows, &no_ax()).unwrap_err();
        let args = serde_json::json!({
            "app": "TextEdit",
            APP_REFUSAL_ARG: stored_refusal(ambiguous),
        });
        let (text, s) = refusal(invoke_target(&args).await);
        assert_eq!(s["code"], "app_window_ambiguous");
        assert_eq!(s["candidates"].as_array().unwrap().len(), 2);
        assert!(text.contains("window_id 3 (pid 20): A.txt"), "{text}");

        // A path that skipped resolve_target is refused, not resolved here.
        let (_, s) = refusal(invoke_target(&serde_json::json!({"app": "TextEdit"})).await);
        assert_eq!(s["code"], "app_not_resolved");
    }

    /// Two instances, Screen Recording off (CG titles empty), AX data only for
    /// pid 20: pid 21's window cannot be identified, so pid 20's window is not
    /// picked in its place.
    #[test]
    fn an_unidentified_instance_refuses_the_whole_lookup() {
        let apps = [
            app("TextEdit", 20, "com.apple.TextEdit"),
            app("TextEdit", 21, "com.apple.TextEdit"),
        ];
        let windows = [window(3, 20, "", 1), window(4, 21, "", 2)];
        let (text, s) = refusal(select_app_window(
            "TextEdit",
            &apps,
            &windows,
            &ax(&[(3, "A.txt")]),
        ));
        assert_eq!(s["code"], "app_window_unidentified");
        assert_eq!(s["pids"], serde_json::json!([21]));
        assert!(text.contains("pid 21"), "{text}");
    }

    #[test]
    fn no_window_or_no_app_is_refused_with_a_next_step() {
        let apps = [app("TextEdit", 20, "com.apple.TextEdit")];
        let (text, s) = refusal(select_app_window("TextEdit", &apps, &[], &no_ax()));
        assert_eq!(s["code"], "app_window_not_found");
        assert!(text.contains("launch_app") && text.contains("list_windows"));
        let (text, s) = refusal(select_app_window("Notes", &apps, &[], &no_ax()));
        assert_eq!(s["code"], "app_not_running");
        assert!(text.contains("launch_app") && text.contains("list_windows"));
    }

    /// Windows exist but neither AX nor CG titles identify one: say it may be
    /// permissions instead of telling the agent to launch an open app.
    #[test]
    fn unidentifiable_windows_point_at_permissions() {
        let apps = [app("TextEdit", 20, "com.apple.TextEdit")];
        let (text, s) = refusal(select_app_window(
            "TextEdit",
            &apps,
            &[window(9, 20, "", 0)],
            &no_ax(),
        ));
        assert_eq!(s["code"], "app_window_unidentified");
        assert!(
            text.contains("permission") && !text.contains("launch_app"),
            "{text}"
        );
    }

    #[test]
    fn several_windows_are_listed_not_guessed() {
        // Two running instances with the same name pool their windows.
        let apps = [
            app("TextEdit", 20, "com.apple.TextEdit"),
            app("TextEdit", 21, "com.apple.TextEdit"),
        ];
        let windows = [window(3, 20, "A.txt", 1), window(4, 21, "B.txt", 2)];
        let (text, s) = refusal(select_app_window("TextEdit", &apps, &windows, &no_ax()));
        assert_eq!(s["code"], "app_window_ambiguous");
        assert_eq!(
            s["candidates"],
            serde_json::json!([
                {"window_id": 4, "pid": 21, "title": "B.txt"},
                {"window_id": 3, "pid": 20, "title": "A.txt"}
            ])
        );
        assert!(text.contains("window_id 3 (pid 20): A.txt"), "{text}");
        assert!(text.contains("pass pid + window_id"), "{text}");
    }

    #[test]
    fn ended_session_gets_background_input_again() {
        let sent = BackgroundInputSent::default();
        let report = serde_json::json!({"routes": []});
        let key = |s: &str| (Some(s.to_owned()), 7, 9);
        assert!(background_input_is_news(
            &mut sent.lock().unwrap(),
            key("a"),
            &report,
            false
        ));
        assert!(background_input_is_news(
            &mut sent.lock().unwrap(),
            key("b"),
            &report,
            false
        ));
        retire_background_input(&sent, "a");
        assert!(
            background_input_is_news(&mut sent.lock().unwrap(), key("a"), &report, false),
            "restarted session's first read"
        );
        assert!(
            !background_input_is_news(&mut sent.lock().unwrap(), key("b"), &report, false),
            "other session untouched"
        );
    }

    #[test]
    fn background_input_is_sent_once_per_session_and_window() {
        let mut sent = HashMap::new();
        let key = |session: &str| (Some(session.to_owned()), 7, 9);
        let report =
            serde_json::json!({"routes": [{"route": "accessibility", "status": "available"}]});
        let changed =
            serde_json::json!({"routes": [{"route": "accessibility", "status": "refused"}]});
        assert!(
            background_input_is_news(&mut sent, key("a"), &report, false),
            "first read"
        );
        assert!(
            !background_input_is_news(&mut sent, key("a"), &report, false),
            "unchanged"
        );
        assert!(
            background_input_is_news(&mut sent, key("a"), &report, true),
            "diff:false"
        );
        assert!(
            background_input_is_news(&mut sent, key("b"), &report, false),
            "other session"
        );
        assert!(
            background_input_is_news(&mut sent, (Some("a".into()), 7, 10), &report, false),
            "other window"
        );
        assert!(
            background_input_is_news(&mut sent, key("a"), &changed, false),
            "changed"
        );
        assert!(
            !background_input_is_news(&mut sent, key("a"), &changed, false),
            "unchanged again"
        );
    }
}

/// Chromium can withhold a page from accessibility when it is asked while its
/// window is fully covered (lane VM, Chrome 154: 13 of 13 cold reads of a
/// covered window had no AXWebArea after 10 s, and disabling Chromium's
/// occlusion tracking did not change that; a first read while visible kept the
/// page after the window was covered). The walk then shows browser controls
/// only, which reads like an empty page. Report it when the driver's own
/// web-content wait for this process timed out and a complete, non-empty walk
/// of this window reached no web content, counting elements pruned from the
/// output; an incomplete walk (budget stop, depth cut, failed child read)
/// proves nothing.
// ponytail: the wait is tracked per process, so once any window of the process
// exposes a page, a different covered window gets no note. Track readiness per
// window if that case shows up.
fn unexposed_web_content(pid: i32, walk: &crate::ax::tree::TreeWalkResult) -> Option<Value> {
    if !walk.sightings.complete(&walk.walk)
        || walk.nodes.is_empty()
        || walk.sightings.web_content
        || !crate::ax::enablement::web_content_wait_timed_out(pid)
    {
        return None;
    }
    Some(serde_json::json!({
        "exposed": false,
        "reason": "No page content was found in this browser window, and the browser did not \
            expose page content when the driver enabled accessibility for it. If this window \
            should show a page, the browser may be withholding it from accessibility; this has \
            been seen when the window was fully covered at that moment.",
        "next": "Use the browser_* tools for page content, or bring_to_front this window and \
            read it again (in testing, the page then appeared and stayed exposed after the \
            window was covered again)."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::tree::AXNode;
    use serde_json::json;

    fn node(
        idx: Option<usize>,
        role: &str,
        title: Option<&str>,
        depth: usize,
        parent: Option<usize>,
        frame: Option<[f64; 4]>,
        actions: Vec<String>,
    ) -> AXNode {
        AXNode {
            url: None,
            element_index: idx,
            role: role.into(),
            title: title.map(|s| s.to_string()),
            value: None,
            description: None,
            identifier: None,
            help: None,
            actions,
            element_ptr: 0,
            depth,
            parent_element_index: parent,
            parent_position: None,
            frame,
            value_state: None,
            value_description: None,
            placeholder: None,
            value_settable: None,
            focused: None,
            text_selection: None,
            min_value: None,
            max_value: None,
            enabled: None,
            selected: None,
            in_web_content: false,
        }
    }

    #[test]
    fn link_url_is_serialized_without_replacing_label_or_value() {
        let mut link = node(
            Some(0),
            "AXLink",
            Some("Book"),
            0,
            None,
            None,
            vec!["AXPress".into()],
        );
        link.url = Some("https://example.test/book".into());
        let entry = &build_elements_array_with_token(&[link], None)[0];
        assert_eq!(entry["url"], "https://example.test/book");
        assert_eq!(entry["label"], "Book");
        assert!(entry.get("value").is_none());
        let mut field = node(
            Some(1),
            "AXTextField",
            Some("Search"),
            0,
            None,
            None,
            vec![],
        );
        // As the walker fills a text field: display value and lossless state.
        field.value = Some("pickleball".into());
        field.value_state = Some("pickleball".into());
        let entry = &build_elements_array_with_token(&[field], None)[0];
        assert_eq!(entry["value"], "pickleball");
        assert!(entry.get("url").is_none());
    }

    #[test]
    fn elements_match_indexed_node_count() {
        // Mix of indexed + non-indexed nodes; only indexed should surface.
        let nodes = vec![
            node(
                Some(0),
                "AXWindow",
                Some("Doc"),
                0,
                None,
                Some([0.0, 0.0, 800.0, 600.0]),
                vec![],
            ),
            node(None, "AXStaticText", Some("hint"), 1, Some(0), None, vec![]),
            node(
                Some(1),
                "AXButton",
                Some("OK"),
                1,
                Some(0),
                Some([10.0, 20.0, 60.0, 24.0]),
                vec![],
            ),
            node(
                Some(2),
                "AXButton",
                Some("Cancel"),
                1,
                Some(0),
                Some([80.0, 20.0, 60.0, 24.0]),
                vec![],
            ),
        ];
        let elements = build_elements_array_with_token(&nodes, None);
        assert_eq!(
            elements.len(),
            3,
            "non-actionable rows must be filtered out"
        );
        let indices: Vec<u64> = elements
            .iter()
            .map(|e| e["element_index"].as_u64().unwrap())
            .collect();
        assert_eq!(
            indices,
            vec![0, 1, 2],
            "ordering must match DFS / element_index assignment"
        );
    }

    #[test]
    fn elements_shape_carries_role_label_frame_parent_depth() {
        let nodes = vec![node(
            Some(7),
            "AXButton",
            Some("Go"),
            3,
            Some(2),
            Some([1.5, 2.5, 33.0, 44.0]),
            vec![],
        )];
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert_eq!(entry["element_index"], 7);
        assert_eq!(entry["role"], "AXButton");
        assert_eq!(entry["label"], "Go");
        assert_eq!(entry["depth"], 3);
        assert_eq!(entry["parent_index"], 2);
        let frame = &entry["frame"];
        assert_eq!(frame["x"], 1.5);
        assert_eq!(frame["y"], 2.5);
        assert_eq!(frame["w"], 33.0);
        assert_eq!(frame["h"], 44.0);
    }

    #[test]
    fn elements_surface_value_separately_from_label() {
        // A field with BOTH a title and a value (e.g. WhatsApp's "Compose
        // message" box holding typed text): label is the title, but the typed
        // value must ALSO be exposed so the caller can verify what landed.
        let mut nodes = vec![node(
            Some(0),
            "AXTextArea",
            Some("Compose message"),
            1,
            None,
            None,
            vec![],
        )];
        nodes[0].value = Some("i love u".into());
        // The tree reader sets a text field's content as value_state.
        nodes[0].value_state = Some("i love u".into());
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert_eq!(entry["label"], "Compose message", "label stays the title");
        assert_eq!(
            entry["value"], "i love u",
            "value must be surfaced separately"
        );
    }

    #[test]
    fn elements_surface_control_state_fields() {
        // A slider whose AXValue is a CFNumber: `value` comes from the
        // coerced value_state, alongside value_description, min/max,
        // enabled, and selected.
        let mut nodes = vec![node(
            Some(0),
            "AXSlider",
            Some("Stationary noise suppression"),
            1,
            None,
            None,
            vec![],
        )];
        nodes[0].value_state = Some("8".into());
        nodes[0].value_description = Some("8 dB".into());
        nodes[0].min_value = Some(2.0);
        nodes[0].max_value = Some(8.0);
        nodes[0].enabled = Some(true);
        nodes[0].selected = Some(false);
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert_eq!(
            entry["value"], "8",
            "numeric AXValue surfaces via value_state"
        );
        assert_eq!(entry["value_description"], "8 dB");
        assert_eq!(entry["min"], 2.0);
        assert_eq!(entry["max"], 8.0);
        assert_eq!(entry["enabled"], true);
        assert_eq!(entry["selected"], false);
    }

    #[test]
    fn elements_surface_inherited_web_content_trust_marker() {
        let mut nodes = vec![node(
            Some(0),
            "AXButton",
            Some("Renderer button"),
            2,
            None,
            None,
            vec![],
        )];
        nodes[0].in_web_content = true;
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert_eq!(entry["in_web_content"], true);
    }

    #[test]
    fn checkbox_value_state_normalizes_to_selected() {
        let mut nodes = vec![node(
            Some(0),
            "AXCheckBox",
            Some("I agree"),
            0,
            None,
            None,
            vec![],
        )];
        nodes[0].value_state = Some("0".into());
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert_eq!(entry["selected"], false);
    }

    #[test]
    fn elements_control_state_fields_omitted_when_absent() {
        // Stock behaviour is unchanged for elements without control state.
        let nodes = vec![node(Some(0), "AXButton", Some("OK"), 0, None, None, vec![])];
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        for key in ["value_description", "min", "max", "enabled", "selected"] {
            assert!(entry.get(key).is_none(), "{key} must be omitted");
        }
    }

    #[test]
    fn elements_omit_degenerate_min_max_range() {
        // WebKit reports AXMinValue/AXMaxValue as 0.0/0.0 on non-range
        // controls (checkboxes, radios) — a degenerate range is omitted.
        let mut nodes = vec![node(
            Some(0),
            "AXCheckBox",
            Some("On"),
            0,
            None,
            None,
            vec![],
        )];
        nodes[0].min_value = Some(0.0);
        nodes[0].max_value = Some(0.0);
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert!(entry.get("min").is_none(), "degenerate min must be omitted");
        assert!(entry.get("max").is_none(), "degenerate max must be omitted");
    }

    #[test]
    fn elements_value_state_falls_back_to_string_value() {
        // Non-text string-valued elements keep their `value` even with no
        // value_state. (Text fields do not: see
        // empty_text_field_reports_empty_value_not_placeholder.)
        let mut nodes = vec![node(Some(0), "AXPopUpButton", None, 0, None, None, vec![])];
        nodes[0].value = Some("Medium".into());
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert_eq!(entry["value"], "Medium");
    }

    #[test]
    fn empty_value_survives_public_and_private_observations() {
        for index in [None, Some(0)] {
            let mut field = node(index, "AXTextField", Some("Draft"), 0, None, None, vec![]);
            field.value = Some(String::new());
            // The tree reader carries a text field's content in value_state.
            field.value_state = Some(String::new());
            let nodes = [field];
            let observed = build_observation_elements_array(&nodes);
            assert_eq!(observed[0]["value"], "");
            let public = build_elements_array_with_token(&nodes, Some(12));
            if index.is_some() {
                assert_eq!(public[0]["value"], "");
                assert_eq!(public[0]["element_token"], "s0000000c:0");
            } else {
                assert!(public.is_empty());
            }
        }
    }

    #[test]
    fn placeholder_and_writability_are_reported_separately_from_value() {
        let mut field = node(Some(0), "AXTextField", None, 0, None, None, vec![]);
        field.value_state = Some(String::new());
        field.placeholder = Some("Search".into());
        field.value_settable = Some(false);
        let entry = &build_elements_array_with_token(&[field], None)[0];
        assert_eq!(entry["value"], "", "the empty content, not the hint");
        assert_eq!(entry["placeholder"], "Search");
        assert_eq!(entry["value_settable"], false);
        assert_eq!(entry["label"], "Search", "an empty field is named by its hint");
    }

    #[test]
    fn unknown_writability_is_omitted() {
        let entry = &build_elements_array_with_token(
            &[node(Some(0), "AXTextField", None, 0, None, None, vec![])],
            None,
        )[0];
        assert!(entry.get("value_settable").is_none());
    }

    #[test]
    fn label_prefers_title_then_description_then_value_then_placeholder() {
        let mut n = node(Some(0), "AXTextField", None, 0, None, None, vec![]);
        n.identifier = Some("txt-id".into());
        assert_eq!(derive_label(&n).as_deref(), Some("txt-id"));
        n.placeholder = Some("  Hint ".into());
        assert_eq!(derive_label(&n).as_deref(), Some("Hint"));
        n.value = Some("   ".into());
        assert_eq!(derive_label(&n).as_deref(), Some("Hint"), "a blank value does not name it");
        n.value = Some(" typed ".into());
        assert_eq!(derive_label(&n).as_deref(), Some("typed"));
        n.description = Some("Desc".into());
        assert_eq!(derive_label(&n).as_deref(), Some("Desc"));
        n.title = Some("Title".into());
        assert_eq!(derive_label(&n).as_deref(), Some("Title"));
    }

    #[test]
    fn elements_omit_empty_value() {
        // An empty AXValue must not emit a `value` field (matches the other
        // optional fields' omit-when-absent contract).
        let mut nodes = vec![node(Some(0), "AXButton", Some("OK"), 0, None, None, vec![])];
        nodes[0].value = Some(String::new());
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert!(entry.get("value").is_none(), "empty value must be omitted");
    }

    #[test]
    fn elements_omit_optional_fields_when_missing() {
        let nodes = vec![node(Some(0), "AXUnknown", None, 0, None, None, vec![])];
        let entry = &build_elements_array_with_token(&nodes, None)[0];
        assert!(
            entry.get("label").is_none(),
            "label must be omitted when title/value/desc/id are all empty"
        );
        assert!(
            entry.get("frame").is_none(),
            "frame must be omitted when no rect was captured"
        );
        assert!(
            entry.get("parent_index").is_none(),
            "parent_index must be omitted at the root"
        );
        assert_eq!(entry["role"], "AXUnknown");
        assert_eq!(entry["depth"], 0);
    }

    #[test]
    fn elements_label_fallback_chain() {
        // title missing → description → value → identifier
        let nodes = vec![
            node(Some(0), "AXButton", None, 0, None, None, vec![]),
            node(Some(1), "AXButton", None, 0, None, None, vec![]),
            node(Some(2), "AXButton", None, 0, None, None, vec![]),
        ];
        let mut nodes = nodes;
        nodes[0].description = Some("from-desc".into());
        nodes[1].value = Some("from-val".into());
        nodes[2].identifier = Some("from-id".into());
        let elements = build_elements_array_with_token(&nodes, None);
        assert_eq!(elements[0]["label"], "from-desc");
        assert_eq!(elements[1]["label"], "from-val");
        assert_eq!(elements[2]["label"], "from-id");
    }

    #[test]
    fn observation_projection_keeps_display_only_counter_without_action_identity() {
        let mut counter = node(
            None,
            "AXStaticText",
            None,
            1,
            Some(7),
            Some([10.0, 20.0, 90.0, 18.0]),
            vec![],
        );
        counter.value = Some("counter=1".into());
        let button = node(
            Some(7),
            "AXButton",
            Some("Increment"),
            0,
            None,
            None,
            vec!["AXPress".into()],
        );
        let nodes = vec![button, counter];
        let public = build_elements_array_with_token(&nodes, Some(123));
        assert_eq!(public.len(), 1, "public snapshots remain actionable-only");
        assert_eq!(public[0]["element_index"], 7);
        assert!(public[0].get("element_token").is_some());

        let observed = build_observation_elements_array(&nodes);
        assert_eq!(
            observed.len(),
            2,
            "verification must retain the display-only counter"
        );
        assert_eq!(observed[0]["element_index"], 7);
        assert_eq!(observed[0]["actions"], json!(["AXPress"]));
        let counter = &observed[1];
        assert_eq!(counter["role"], "AXStaticText");
        assert_eq!(counter["label"], "counter=1");
        assert_eq!(counter["value"], "counter=1");
        assert_eq!(
            counter["frame"],
            json!({"x":10.0,"y":20.0,"w":90.0,"h":18.0})
        );
        assert_eq!(counter["parent_index"], 7);
        assert!(counter.get("element_index").is_none());
        assert!(observed
            .iter()
            .all(|entry| entry.get("element_token").is_none()));
    }

    #[test]
    fn empty_text_field_reports_empty_value_not_placeholder() {
        let mut field = node(Some(0), "AXTextField", None, 0, None, None, vec![]);
        // The tree reader keeps the placeholder in `value` for markdown, and
        // the real (empty) content in `value_state`.
        field.value = Some("Search".into());
        field.value_state = Some(String::new());
        for entries in [
            build_elements_array_with_token(std::slice::from_ref(&field), Some(1)),
            build_observation_elements_array(std::slice::from_ref(&field)),
        ] {
            assert_eq!(entries[0]["value"], "", "empty is a real text state");
        }
        // AXValue unreadable: no value (verification says unknown), never
        // the placeholder.
        field.value_state = None;
        for entries in [
            build_elements_array_with_token(std::slice::from_ref(&field), Some(1)),
            build_observation_elements_array(std::slice::from_ref(&field)),
        ] {
            assert!(entries[0].get("value").is_none());
        }
    }

    #[test]
    fn observation_projection_preserves_display_only_web_trust_marker() {
        let mut counter = node(None, "AXStaticText", None, 0, None, None, vec![]);
        counter.value = Some("counter=1".into());
        counter.in_web_content = true;
        let observed = build_observation_elements_array(&[counter]);
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0]["in_web_content"], true);
        assert!(observed[0].get("element_index").is_none());
        assert!(observed[0].get("element_token").is_none());
    }

    #[test]
    fn build_elements_array_with_token_emits_actions_when_present() {
        let nodes = vec![node(
            Some(0),
            "AXButton",
            Some("OK"),
            1,
            None,
            None,
            vec!["AXPress".to_owned(), "AXShowMenu".to_owned()],
        )];
        let entries = build_elements_array_with_token(&nodes, None);
        assert_eq!(entries[0]["actions"], json!(["AXPress", "AXShowMenu"]));
    }

    #[test]
    fn build_elements_array_with_token_omits_actions_when_empty() {
        let nodes = vec![node(Some(0), "AXButton", Some("OK"), 1, None, None, vec![])];
        let entries = build_elements_array_with_token(&nodes, None);
        assert!(entries[0].get("actions").is_none());
    }

    #[test]
    fn build_elements_array_with_token_emits_element_token_per_row() {
        let cache = crate::ax::cache::ElementCache::new();
        let pid = 0x6abc_0001_i32;
        let nodes = vec![
            node(Some(0), "AXButton", Some("A"), 1, None, None, vec![]),
            node(Some(1), "AXButton", Some("B"), 1, None, None, vec![]),
            node(Some(2), "AXButton", Some("C"), 1, None, None, vec![]),
        ];
        let sid = cache.publish(pid, 9, crate::ax::cache::CachedSnapshot::from_nodes(&nodes));
        let entries = build_elements_array_with_token(&nodes, Some(sid));
        assert_eq!(entries.len(), 3);
        // Every entry must have BOTH fields (additive contract).
        for e in &entries {
            assert!(
                e.get("element_index").is_some(),
                "element_index must remain"
            );
            let tok = e
                .get("element_token")
                .and_then(|v| v.as_str())
                .expect("element_token must be a string");
            assert!(tok.starts_with('s'), "token must use the 's' prefix: {tok}");
            assert!(tok.contains(':'), "token must be `s{{hex}}:{{idx}}`: {tok}");
        }
        for e in &entries {
            let idx = e["element_index"].as_u64().unwrap() as usize;
            let tok = e["element_token"].as_str().unwrap();
            let (resolved_idx, wid, _) = cache
                .resolve_element_args(pid, None, Some(tok), None, None, "click")
                .expect("token must resolve")
                .into_parts(None);
            assert_eq!(wid, Some(9));
            assert_eq!(resolved_idx, Some(idx));
        }
    }

    #[test]
    fn build_elements_array_with_token_observation_only_has_actions_no_token() {
        let nodes = vec![node(
            Some(0),
            "AXButton",
            Some("OK"),
            1,
            None,
            None,
            vec!["AXPress".to_owned(), "AXShowMenu".to_owned()],
        )];
        let entries = build_observation_elements_array(&nodes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["actions"], json!(["AXPress", "AXShowMenu"]));
        assert!(
            entries[0].get("element_token").is_none(),
            "observation-only entries must not emit unregistered element_token: {}",
            entries[0]
        );
    }

    fn compact_fixture() -> Value {
        let mut nodes = vec![
            node(
                Some(0),
                "AXGroup",
                Some("Toolbar"),
                1,
                None,
                Some([10.0, 20.0, 300.0, 40.0]),
                vec![],
            ),
            node(
                Some(1),
                "AXButton",
                Some("Save"),
                2,
                Some(0),
                Some([12.0, 22.0, 60.0, 30.0]),
                vec!["AXPress".into()],
            ),
            node(
                Some(2),
                "AXCheckBox",
                Some("Bold"),
                2,
                Some(0),
                Some([80.0, 22.0, 20.0, 20.0]),
                vec![],
            ),
        ];
        nodes[1].enabled = Some(true);
        nodes[1].selected = Some(false);
        nodes[1].value_settable = Some(false);
        nodes[2].enabled = Some(false);
        nodes[2].selected = Some(true);
        let elements = cua_driver_core::element_frame::with_screenshot_frames(
            build_elements_array_with_token(&nodes, Some(7)),
            (0.0, 0.0),
            2.0,
        );
        json!({
            "window_id": 9,
            "tree_markdown": "- AXGroup",
            "elements": elements,
            "_note": "Prefer `elements`",
        })
    }

    #[test]
    fn compact_elements_drop_geometry_structure_defaults_and_note() {
        let mut structured = compact_fixture();
        compact_elements(&mut structured);
        assert!(structured.get("_note").is_none());
        assert_eq!(structured["tree_markdown"], "- AXGroup");
        let elements = structured["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 3);
        for entry in elements {
            for key in ["frame", "depth", "parent_index"] {
                assert!(entry.get(key).is_none(), "{key} must be omitted: {entry}");
            }
            assert!(entry.get("screenshot_frame").is_some(), "{entry}");
            assert!(entry.get("element_token").is_some(), "{entry}");
        }
        let save = &elements[1];
        assert!(save.get("enabled").is_none(), "enabled:true is the default");
        assert!(
            save.get("selected").is_none(),
            "selected:false is the default"
        );
        assert_eq!(save["value_settable"], false);
        assert_eq!(save["actions"], json!(["AXPress"]));
        assert_eq!(
            save["screenshot_frame"],
            json!({"x": 24, "y": 44, "w": 120, "h": 60})
        );
        let bold = &elements[2];
        assert_eq!(bold["enabled"], false, "non-default enabled is kept");
        assert_eq!(bold["selected"], true, "non-default selected is kept");
    }

    #[test]
    fn full_elements_keep_every_field() {
        // Full mode never runs the projection, so records are what the builder emits.
        let full = compact_fixture();
        let save = &full["elements"][1];
        assert_eq!(save["depth"], 2);
        assert_eq!(save["parent_index"], 0);
        assert_eq!(save["enabled"], true);
        assert_eq!(save["selected"], false);
        assert_eq!(
            save["frame"],
            json!({"x": 12.0, "y": 22.0, "w": 60.0, "h": 30.0})
        );
        assert!(full.get("_note").is_some());
    }

    #[test]
    fn schema_advertises_element_fields() {
        let property = &def().input_schema["properties"]["element_fields"];
        assert_eq!(property["enum"], json!(["none", "compact", "full"]));
        assert!(property["description"]
            .as_str()
            .unwrap()
            .contains("none\" (default)"));
    }

    #[test]
    fn none_projection_omits_elements_and_keeps_the_tree() {
        let mut structured = compact_fixture();
        project_elements(&mut structured, ElementFields::None);
        assert!(structured.get("elements").is_none());
        assert!(structured.get("_note").is_none());
        assert_eq!(structured["tree_markdown"], "- AXGroup");
        assert_eq!(structured["window_id"], 9);
        let mut compact = compact_fixture();
        project_elements(&mut compact, ElementFields::Compact);
        assert_eq!(compact["elements"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn none_tree_names_the_token_form() {
        let mut tree = "- [0] AXButton (Save)".to_owned();
        prepend_token_hint(&mut tree, "s0000000d");
        assert_eq!(
            tree,
            "element_token = s0000000d:<index>\n- [0] AXButton (Save)"
        );
    }

    #[test]
    fn switching_element_fields_forces_a_full_look() {
        let nodes = vec![node(Some(0), "AXButton", Some("Save"), 1, None, None, vec![])];
        let session = Some("s".to_owned());
        let all = [ElementFields::None, ElementFields::Compact, ElementFields::Full];
        for before in all {
            let prior_bounds = crate::ax::cache::LookBounds {
                max_elements: 10,
                max_depth: 5,
                element_fields: before,
                ..Default::default()
            };
            let prior = crate::ax::cache::PriorLook {
                elements: Vec::new(),
                rows: crate::ax::diff::rows_of(&nodes),
                next_id: 1,
                bounds: prior_bounds,
                session: Some("s".into()),
                full_delivered: true,
            };
            for after in all {
                let bounds = crate::ax::cache::LookBounds {
                    element_fields: after,
                    ..prior_bounds
                };
                assert_eq!(
                    diff_baseline(Some(&prior), &bounds, &session).is_some(),
                    before == after,
                    "{before:?} -> {after:?} must diff only when the projection is unchanged"
                );
            }
        }
    }
}
