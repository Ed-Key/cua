use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::path::PathBuf;

pub struct LaunchAppTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "launch_app".into(),
        description:
            "Launch an app in the background without bringing it forward, by bundle_id \
             (preferred) or name. Returns pid, launch_state, a windows array (list_windows \
             shape) for get_window_state, and opened_windows (ids that appeared during the \
             call). urls opens files or folders in it; requested_windows then names the windows \
             showing them, reused_windows the ones of those that existed before the call. \
             self_activation_suppressed is false if the app came to the front at any point. \
             For browser DevTools use browser_prepare."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "bundle_id": {
                    "type": "string",
                    "description": "Bundle identifier, e.g. com.apple.calculator; wins over name."
                },
                "name": {
                    "type": "string",
                    "description": "App display name, used when bundle_id is absent."
                },
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "File paths or URLs to open, e.g. a folder for Finder."
                },
                "webkit_inspector_port": {
                    "type": "integer",
                    "description": "Open a WebKit inspector server on this port, for Tauri and WebKit apps."
                },
                "creates_new_application_instance": {
                    "type": "boolean",
                    "description": "Start a separate instance (open -n) so concurrent sessions do not share one window."
                },
                "additional_arguments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments appended after --args when launching."
                }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let bundle_id = args.opt_str("bundle_id");
        let name = args.opt_str("name");
        let mut response_bundle_id = bundle_id.clone();
        let response_requested_name = name.clone();
        let urls: Vec<String> = args
            .str_array("urls")
            .into_iter()
            .map(normalize_launch_url)
            .collect();
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        let webkit_inspector_port = args.opt_u64("webkit_inspector_port").map(|v| v as u16);
        let creates_new_instance = args.bool_or("creates_new_application_instance", false);
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if additional_arguments
            .iter()
            .any(|argument| argument == super::check_permissions::PERMISSIONS_HOST_REQUEST_ARG)
        {
            return protected_host_launch_refusal();
        }
        if additional_arguments
            .iter()
            .any(|argument| cua_driver_core::launch_guard::contains_remote_debugging_flag(argument))
        {
            return ToolResult::error(
                cua_driver_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL,
            );
        }

        if bundle_id.is_none() && name.is_none() {
            return ToolResult::error(
                "Provide either bundle_id or name to identify the app to launch.",
            );
        }
        if bundle_id.as_deref().is_some_and(is_cua_driver_bundle_id) {
            return protected_host_launch_refusal();
        }
        if let Some(ref bid) = bundle_id {
            if crate::apps::resolve_bundle_id_to_locator(bid).is_none() {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for bundle_id '{bid}'."),
                    serde_json::json!({ "bundle_id": bid }),
                );
            }
        } else if let Some(ref n) = name {
            let Some(locator) = crate::apps::locate_by_name(n) else {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for name '{n}'."),
                    serde_json::json!({ "name": n }),
                );
            };
            let (_, resolved_bundle_id) = locator.app_ref_and_bundle_id();
            response_bundle_id = resolved_bundle_id.clone();
            if resolved_bundle_id
                .as_deref()
                .is_some_and(is_cua_driver_bundle_id)
            {
                return protected_host_launch_refusal();
            }
        }
        if let Some(err) = preflight_file_urls(&urls) {
            return err;
        }

        // Build env dict for webkit inspector.
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Some(port) = webkit_inspector_port {
            env.insert(
                "WEBKIT_INSPECTOR_SERVER".to_string(),
                format!("127.0.0.1:{port}"),
            );
            env.insert("TAURI_WEBVIEW_AUTOMATION".to_string(), "1".to_string());
        }

        let port_summary = {
            let mut s = String::new();
            if let Some(port) = webkit_inspector_port {
                s.push_str(&format!("\nWebKit inspector available on port {port}."));
            }
            s
        };

        // What the call starts from: the activation log, the user's front
        // app, and (off the async runtime) the windows on screen. Afterwards
        // the result names the windows this call opened and says whether the
        // app came forward.
        let activation_mark = crate::focus_steal::activation_mark();
        let prior_frontmost = crate::apps::frontmost_pid();
        let plain_open = additional_arguments.is_empty() && env.is_empty() && !creates_new_instance;

        // A file the running app already shows is not sent again. An app
        // handed a document it has open brings that window forward and
        // activates itself (TextEdit did, for up to 20 ms, under the guard).
        let reuse_bundle_id = response_bundle_id
            .clone()
            .filter(|_| plain_open && !urls.is_empty());
        let reuse_urls = urls.clone();
        let Ok((windows_before, running_pid, reused)) = tokio::task::spawn_blocking(move || {
            let windows_before: Vec<(u32, i32)> = crate::windows::all_windows()
                .into_iter()
                .map(|w| (w.window_id, w.pid))
                .collect();
            let running_pid = reuse_bundle_id.as_deref().and_then(running_pid_for_bundle);
            let reused: Vec<(String, u32)> = running_pid
                .and_then(|pid| {
                    crate::ax::bindings::ax_window_documents_of_pid(pid, AX_DOCUMENTS_BUDGET)
                })
                .map(|documents| already_open(&reuse_urls, &documents))
                .unwrap_or_default();
            (windows_before, running_pid, reused)
        })
        .await
        else {
            return ToolResult::error("Task error: window snapshot before the launch failed");
        };
        let urls_to_open: Vec<String> = urls
            .iter()
            .filter(|url| !reused.iter().any(|(open, _)| open == *url))
            .cloned()
            .collect();
        // Every requested url was already open: nothing is sent at all.
        let reused_pid = running_pid.filter(|_| !urls.is_empty() && urls_to_open.is_empty());

        // ── Layer-3 focus-steal suppression (3-phase wrap) ───────────────
        //
        // Arms a wildcard suppression BEFORE the launch (covers
        // self-activations the target fires synchronously during `open()`),
        // then upgrades to a targeted suppression keyed to the launched pid,
        // holding BOTH leases briefly so an activation in the gap is still
        // caught (hoang17's Swift PR #1521). The targeted lease holds for the
        // activation window AND the window wait (an app that activates with
        // its first window is still caught); then the belt-and-braces loop
        // re-activates the prior frontmost if the target is still in front.
        // Finder folders take this same path: `openURLs` with
        // `activates = false` opens a background window there, where
        // `selectFile:inFileViewerRootedAtPath:` activated Finder before any
        // lease could see it.
        let wildcard_lease = prior_frontmost
            .filter(|_| reused_pid.is_none())
            .map(|prior| {
                crate::focus_steal::FocusStealPreventer::begin_suppression(
                    None,
                    prior,
                    "LaunchAppTool.pre",
                )
            });

        // The slow path (`openURLs:withApplicationAtURL:`) triggers a SECOND
        // activation when the file-open delivers, after the bundle-only
        // activation window; it sizes the suppression window below.
        let slow_launch_path = !urls_to_open.is_empty() || !plain_open;

        let launch_urls = urls_to_open.clone();
        let launch_result = match reused_pid {
            Some(pid) => Ok(Ok(pid)),
            None => {
                tokio::task::spawn_blocking(move || {
                    let pid = if let Some(ref bid) = bundle_id {
                        if launch_urls.is_empty() && plain_open {
                            crate::apps::launch_app(bid)?
                        } else {
                            crate::apps::launch_with_urls_by_bundle(
                                bid,
                                &launch_urls,
                                &additional_arguments,
                                &env,
                                creates_new_instance,
                            )?
                        }
                    } else {
                        let n = name.as_deref().unwrap();
                        if launch_urls.is_empty() && plain_open {
                            crate::apps::launch_app_by_name(n)?
                        } else {
                            crate::apps::launch_with_urls_by_name(
                                n,
                                &launch_urls,
                                &additional_arguments,
                                &env,
                                creates_new_instance,
                            )?
                        }
                    };
                    Ok::<_, anyhow::Error>(pid)
                })
                .await
            }
        };
        let launched_at = std::time::Instant::now();

        let pid = match launch_result {
            Ok(Ok(pid)) => pid,
            Ok(Err(e)) => return structured_launch_failure(&e),
            Err(e) => return ToolResult::error(format!("Task error: {e}")),
        };

        // The prior front app to guard, when something was sent to an app
        // that was not already in front.
        let guard_prior = prior_frontmost
            .filter(|prior| *prior != pid)
            .filter(|_| reused_pid.is_none());
        let targeted_lease = guard_prior.map(|prior| {
            crate::focus_steal::FocusStealPreventer::begin_suppression(
                Some(pid),
                prior,
                "LaunchAppTool.post",
            )
        });
        // Now safe to drop the wildcard: targeted is armed.
        drop(wildcard_lease);

        let cold = !windows_before.iter().any(|(_, owner)| *owner == pid);
        let wait = WindowWait {
            before: windows_before.iter().map(|(id, _)| *id).collect(),
            deadline: launched_at
                + std::time::Duration::from_millis(launch_wait_ms(!urls_to_open.is_empty(), cold)),
            sent_urls: urls_to_open,
            reused: reused.iter().map(|(_, id)| *id).collect(),
        };
        if targeted_lease.is_some() {
            // Hold the targeted lease over the whole post-launch activation
            // window: 500 ms covers `applicationDidFinishLaunching` plus any
            // reflex `NSApp.activate`; the slow path holds 2500 ms because
            // Electron apps re-`app.focus()` from their `open-file` handler
            // ~700-2000 ms after `openURLs` returns.
            let window_ms: u64 = if slow_launch_path { 2500 } else { 500 };
            tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
        }
        // The window wait runs after the activation window, still under the
        // lease, so what it reports is fresh at return. Its deadline counts
        // from the launch's return, so the lease time is not added to it.
        let found = tokio::task::spawn_blocking(move || {
            let found = await_launch_windows(pid, &wait);
            let prior_name = prior_frontmost
                .and_then(crate::apps::running_app)
                .map(|app| app.name);
            (found, crate::apps::running_app(pid), prior_name)
        })
        .await;
        drop(targeted_lease);
        let Ok((found, app_info, prior_name)) = found else {
            return ToolResult::error("Task error: window wait after the launch failed");
        };

        let mut seen_in_loop = false;
        if let Some(prior) = guard_prior {
            // Belt-and-braces loop: demote the target if it pops back to the
            // front after the lease dropped. Up to 5 x 200 ms, only while it
            // is in front.
            for _ in 0..5 {
                if crate::apps::frontmost_pid() != Some(pid) {
                    // Not frontmost: nothing to do this tick.
                    continue;
                }
                seen_in_loop = true;
                let activated = crate::apps::restore_prior_app(prior);
                if crate::apps::frontmost_pid() == Some(pid) {
                    tracing::warn!(
                        target: "platform_macos::tools::launch_app",
                        launched_pid = pid,
                        prior_pid = prior,
                        activate_pid_returned = activated,
                        "belt-and-braces demotion iteration failed: \
                         launched app remained frontmost after \
                         re-activating prior — will retry"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }

            // Detached late-activation watchdog (slow path only). Electron
            // apps with no workspace open re-activate when their Welcome
            // window loads, 4-8 s after `openURLs` returns. A fresh lease
            // demotes those for ~9 s more without holding up the caller. It
            // runs only while the daemon lives (`mcp` / `serve`), and real
            // user input ends it so a user's own click on the app wins.
            if slow_launch_path {
                let launched_pid = pid;
                let prior_pid = prior;
                tokio::spawn(async move {
                    let baseline = crate::focus_steal::read_input_activity();
                    let _lease = crate::focus_steal::FocusStealPreventer::begin_suppression(
                        Some(launched_pid),
                        prior_pid,
                        "LaunchAppTool.watchdog",
                    )
                    .linger_until(std::time::Instant::now() + std::time::Duration::from_secs(9));
                    let mut late_activations = 0u32;
                    for _ in 0..32 {
                        // 32 × 250ms = 8s
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        if crate::focus_steal::user_input_since(&baseline) {
                            break; // the user is acting; their choice wins
                        }
                        if crate::apps::frontmost_pid() == Some(launched_pid) {
                            late_activations += 1;
                            let _ = crate::apps::restore_prior_app(prior_pid);
                        }
                    }
                    if late_activations > 0 {
                        tracing::warn!(
                            target: "platform_macos::tools::launch_app",
                            launched_pid,
                            prior_pid,
                            late_activations,
                            "watchdog demoted post-RPC late activations \
                             — slow-path window may need tuning"
                        );
                    }
                });
            }
        }

        // `(seen_front, front_restored)`: whether the app was seen in front
        // at any point during the call (an activation notification, even one
        // a lease undid within milliseconds, or a front sample), and whether
        // the prior front app was in front at return. `None` when the check
        // did not apply (no prior front app, or the app was already in
        // front). Taken last, so the window wait counts too.
        let activation = prior_frontmost.filter(|prior| *prior != pid).map(|prior| {
            let final_frontmost = crate::apps::frontmost_pid();
            let seen_front = seen_in_loop
                || final_frontmost == Some(pid)
                || crate::focus_steal::activated_since(activation_mark, pid);
            (seen_front, final_frontmost == Some(prior))
        });

        let (app_name, bid) = response_identity(
            app_info.as_ref(),
            response_bundle_id.as_deref(),
            response_requested_name.as_deref(),
        );
        let summary = launch_summary(
            &app_name,
            pid,
            &port_summary,
            &found,
            !urls.is_empty(),
            activation,
            prior_name.as_deref(),
        );

        let windows_json: Vec<Value> = found
            .windows
            .iter()
            .map(|w| {
                let mut record = super::list_windows::window_record_json(w);
                record["input_readiness"] = input_readiness_pointer(pid, w.window_id);
                record
            })
            .collect();

        let mut structured = serde_json::json!({
            "pid": pid,
            "bundle_id": bid,
            "name": app_name,
            "windows": windows_json,
            "opened_windows": found.opened,
            "launch_state": launch_state(true, true, found.ready),
        });
        if !urls.is_empty() {
            structured["requested_windows"] = serde_json::json!(found.requested);
            structured["reused_windows"] = serde_json::json!(found.reused());
        }
        // Only when the activation check ran (see `activation`).
        if let Some((seen_front, front_restored)) = activation {
            structured["self_activation_suppressed"] = Value::Bool(!seen_front);
            structured["front_restored"] = Value::Bool(front_restored);
        }
        ToolResult::text(summary).with_structured(structured)
    }
}

/// Where a launched window's input readiness lives: the exact-window
/// `background_input` report of get_window_state, the same facts every
/// action gates on. A pointer, not a copy: readiness can change after launch
/// and every action checks again.
fn input_readiness_pointer(pid: i32, window_id: u32) -> Value {
    serde_json::json!({
        "see": "get_window_state",
        "field": "background_input",
        "pid": pid,
        "window_id": window_id,
    })
}

fn is_cua_driver_bundle_id(bundle_id: &str) -> bool {
    matches!(bundle_id, "com.trycua.driver" | "com.trycua.driver.local")
}

fn protected_host_launch_refusal() -> ToolResult {
    structured_launch_error(
        "PROTECTED_HOST_ENTRYPOINT",
        "launch_app cannot launch Cua Driver's protected host; operating-system permission UI must originate outside the agent tool stream".to_owned(),
        serde_json::json!({}),
    )
}

// ── Blocking helpers ──────────────────────────────────────────────────────────

/// The pid of the one running app with this bundle id. `None` when none or
/// several run: LaunchServices picks the instance that gets the urls, so a
/// window of another instance cannot stand in for one.
fn running_pid_for_bundle(bundle_id: &str) -> Option<i32> {
    let mut pids = crate::apps::list_running_apps()
        .into_iter()
        .filter(|app| app.bundle_id.as_deref() == Some(bundle_id))
        .map(|app| app.pid);
    let pid = pids.next()?;
    pids.next().is_none().then_some(pid)
}

/// Wall-clock cap on one read of an app's window documents, so a slow app
/// cannot stall the launch.
const AX_DOCUMENTS_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// A local path or `file://` URL as a comparable path: decoded, without a
/// trailing slash, symlinks resolved when the file exists (/tmp is
/// /private/tmp in AXDocument). `None` for remote and custom URLs.
fn document_path(raw: &str) -> Option<PathBuf> {
    let path = local_file_target(raw)?;
    let path = PathBuf::from(path.to_string_lossy().trim_end_matches('/'));
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

/// The requested urls a window already shows, as `(url, window_id)`, from
/// the app's `(window_id, AXDocument)` list.
fn already_open(urls: &[String], documents: &[(u32, Option<String>)]) -> Vec<(String, u32)> {
    urls.iter()
        .filter_map(|url| {
            let wanted = document_path(url)?;
            documents
                .iter()
                .find(|(_, document)| {
                    document.as_deref().and_then(document_path).as_ref() == Some(&wanted)
                })
                .map(|(id, _)| (url.clone(), *id))
        })
        .collect()
}

/// How long after the launch returns the window wait may run: 3 s when urls
/// were sent, 2 s for a cold launch (500 ms was too short under load), else
/// 500 ms (a running app answers with the windows it has).
fn launch_wait_ms(urls_sent: bool, cold: bool) -> u64 {
    if urls_sent {
        3000
    } else if cold {
        2000
    } else {
        500
    }
}

/// What the window wait looks for.
struct WindowWait {
    /// Every window id on screen before the call.
    before: std::collections::HashSet<u32>,
    /// When to stop waiting: counted from the launch's return, so the time
    /// the suppression leases already held counts toward it.
    deadline: std::time::Instant,
    /// The urls sent to the app (requested urls minus the reused ones).
    sent_urls: Vec<String>,
    /// Windows that already showed a requested url; nothing was sent for them.
    reused: Vec<u32>,
}

/// The app's windows after the launch, and which of them answer the call.
struct LaunchWindows {
    /// Every layer-0 window of the pid: requested first, then the rest.
    windows: Vec<crate::windows::WindowInfo>,
    /// Windows that appeared during the call.
    opened: Vec<u32>,
    /// Windows showing the requested urls (opened or existing).
    requested: Vec<u32>,
    /// Existing windows that already showed a requested file: nothing was
    /// sent for those urls.
    skipped: Vec<u32>,
    /// Existing windows the app opened nothing next to, matched by title
    /// (a Finder folder that was already open; Finder names no document).
    matched_by_title: Vec<u32>,
    /// With urls: every requested url has its window. Without: a window exists.
    ready: bool,
}

impl LaunchWindows {
    /// Requested windows that existed before the call.
    fn reused(&self) -> Vec<u32> {
        self.requested
            .iter()
            .copied()
            .filter(|id| !self.opened.contains(id))
            .collect()
    }
}

/// Which windows answer the request: `(requested, matched_by_title, ready)`.
/// Pure, so the rules are testable. `skipped` are the windows that already
/// showed a requested file (nothing sent for it) and are still on screen;
/// `skipped_missing` says one of them closed meanwhile, which leaves its url
/// unanswered. `titles` is the window snapshot, `(id, title)`.
/// - no urls sent: the skipped windows (all urls were open already) or, for a
///   plain launch, any window;
/// - only local files sent, and the app names documents (AXDocument): the
///   windows of the snapshot whose document is a sent file, once every sent
///   file has one;
/// - otherwise (remote urls, or an app that names no document on any window,
///   like a Finder folder window) the windows that appeared during the call,
///   ready once there is one per sent url. An unreadable document list
///   (`None`) is never taken for "names no document".
/// - on the last poll, when no window appeared at all, each local folder or
///   file whose name is the title of exactly one window, a different window
///   per url, is taken as shown there (Finder brings an open folder's window
///   forward instead of opening one). That matches the name, not the path,
///   and the result says so. Never earlier: a new window may still come.
#[allow(clippy::too_many_arguments)]
fn requested_windows(
    sent_urls: &[String],
    skipped: &[u32],
    skipped_missing: bool,
    opened: &[u32],
    documents: Option<&[(u32, Option<String>)]>,
    titles: &[(u32, &str)],
    has_window: bool,
    last_poll: bool,
) -> (Vec<u32>, Vec<u32>, bool) {
    let mut requested = skipped.to_vec();
    if sent_urls.is_empty() {
        let ready = if requested.is_empty() && !skipped_missing {
            has_window
        } else {
            !skipped_missing
        };
        return (requested, Vec::new(), ready);
    }
    let files: Vec<PathBuf> = sent_urls.iter().filter_map(|u| document_path(u)).collect();
    let all_files = files.len() == sent_urls.len();
    let Some(documents) = documents.or((!all_files).then_some(&[][..])) else {
        return (requested, Vec::new(), false);
    };
    // Only windows in the snapshot count: one the AX read found after the
    // window list was taken is picked up on the next poll.
    let in_snapshot = |id: &u32| titles.iter().any(|(window, _)| window == id);
    if all_files && documents.iter().any(|(_, document)| document.is_some()) {
        let matched: Vec<u32> = files
            .iter()
            .filter_map(|file| {
                documents
                    .iter()
                    .filter(|(id, _)| in_snapshot(id))
                    .find(|(_, d)| d.as_deref().and_then(document_path).as_ref() == Some(file))
                    .map(|(id, _)| *id)
            })
            .collect();
        let ready = matched.len() == files.len() && !skipped_missing;
        for id in matched {
            if !requested.contains(&id) {
                requested.push(id);
            }
        }
        return (requested, Vec::new(), ready);
    }
    if opened.is_empty() && last_poll && all_files {
        let mut by_title: Vec<u32> = Vec::new();
        for file in &files {
            let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
                break;
            };
            let mut hits = titles.iter().filter(|(_, title)| *title == name);
            match (hits.next(), hits.next()) {
                (Some((id, _)), None) if !by_title.contains(id) => by_title.push(*id),
                _ => break,
            }
        }
        if by_title.len() == files.len() {
            requested.extend(by_title.iter().filter(|id| !skipped.contains(id)));
            return (requested, by_title, !skipped_missing);
        }
    }
    requested.extend(opened.iter().filter(|id| !skipped.contains(id)));
    (
        requested,
        Vec::new(),
        opened.len() >= sent_urls.len() && !skipped_missing,
    )
}

/// Poll the pid's windows until the requested ones are there or the deadline
/// passes (see `launch_wait_ms`). The window list read before the launch
/// tells new windows from old ones, which a plain "any window" check cannot:
/// a running app answered with its old windows before the new one registered.
fn await_launch_windows(pid: i32, wait: &WindowWait) -> LaunchWindows {
    loop {
        let last_poll = std::time::Instant::now() >= wait.deadline;
        let mut windows: Vec<_> = crate::windows::all_windows()
            .into_iter()
            .filter(|w| w.pid == pid && w.layer == 0)
            .filter(|w| w.bounds.width > 1.0 && w.bounds.height > 1.0)
            .collect();
        let opened: Vec<u32> = windows
            .iter()
            .map(|w| w.window_id)
            .filter(|id| !wait.before.contains(id))
            .collect();
        let documents = if wait.sent_urls.is_empty() {
            None
        } else {
            crate::ax::bindings::ax_window_documents_of_pid(pid, AX_DOCUMENTS_BUDGET)
        };
        // A window that already showed a requested file and closed meanwhile
        // leaves that url unanswered.
        let skipped: Vec<u32> = wait
            .reused
            .iter()
            .copied()
            .filter(|id| windows.iter().any(|w| w.window_id == *id))
            .collect();
        let skipped_missing = skipped.len() < wait.reused.len();
        let titles: Vec<(u32, &str)> = windows
            .iter()
            .map(|w| (w.window_id, w.title.as_str()))
            .collect();
        let (requested, matched_by_title, ready) = requested_windows(
            &wait.sent_urls,
            &skipped,
            skipped_missing,
            &opened,
            documents.as_deref(),
            &titles,
            !windows.is_empty(),
            last_poll,
        );
        if ready || last_poll {
            rank_launch_windows(&mut windows);
            windows.sort_by_key(|w| !requested.contains(&w.window_id));
            return LaunchWindows {
                windows,
                opened,
                requested,
                skipped,
                matched_by_title,
                ready,
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn window_line(window: &crate::windows::WindowInfo) -> String {
    let title = if window.title.is_empty() {
        "(no title)".to_owned()
    } else {
        format!("\"{}\"", window.title)
    };
    format!("{title} [window_id: {}]", window.window_id)
}

/// The text result: what was opened or reused, and what happened to the front.
fn launch_summary(
    app_name: &str,
    pid: i32,
    port_summary: &str,
    found: &LaunchWindows,
    urls_requested: bool,
    activation: Option<(bool, bool)>,
    prior_name: Option<&str>,
) -> String {
    // Only a check that ran may say "not activated"; without one (no prior
    // front app, or the app was already in front) the text claims nothing.
    let not_activated = matches!(activation, Some((false, _)));
    let mut summary = if not_activated {
        format!("Launched {app_name} (pid {pid}) in background.{port_summary}")
    } else {
        format!("Launched {app_name} (pid {pid}).{port_summary}")
    };
    if let Some((true, restored)) = activation {
        let prior = prior_name.unwrap_or("the previous front app");
        summary.push_str(&if restored {
            format!(
                "\n{app_name} came to the front during the call; {prior} was put back in front."
            )
        } else {
            format!(
                "\n{app_name} came to the front during the call and {prior} is not back in front."
            )
        });
    }
    if urls_requested {
        let by_id = |id: &u32| found.windows.iter().find(|w| w.window_id == *id);
        for id in &found.requested {
            let line = by_id(id)
                .map(window_line)
                .unwrap_or_else(|| format!("[window_id: {id}]"));
            if found.skipped.contains(id) {
                summary.push_str(&format!(
                    "\nAlready open, reused (nothing sent to the app): {line}"
                ));
            } else if found.matched_by_title.contains(id) {
                summary.push_str(&format!(
                    "\nAlready open: the app opened no new window, and this is the one window \
                     titled with the requested name (matched by name, not by path): {line}"
                ));
            } else if found.opened.contains(id) {
                summary.push_str(&format!("\nOpened for the request: {line}"));
            } else {
                summary.push_str(&format!(
                    "\nAlready open, the app showed this existing window: {line}"
                ));
            }
        }
        if !found.ready {
            summary.push_str(
                "\nNo window for every requested url appeared in time; the app may have \
                 opened them in an existing window or tab, or later. Check with list_windows.",
            );
        }
    }
    if !found.windows.is_empty() {
        summary.push_str("\n\nWindows:");
        for w in &found.windows {
            let new = if found.opened.contains(&w.window_id) {
                " (new)"
            } else {
                ""
            };
            summary.push_str(&format!("\n- {}{new}", window_line(w)));
        }
        summary.push_str(&format!(
            "\n→ Call get_window_state(pid: {pid}, window_id) to inspect. {}",
            if not_activated {
                "The app was not activated; that read's background_input reports which input routes the window has now."
            } else {
                "Its background_input reports which input routes the window has now."
            }
        ));
    }
    summary
}

fn rank_launch_windows(windows: &mut [crate::windows::WindowInfo]) {
    windows.sort_by_key(|window| std::cmp::Reverse(!window.title.trim().is_empty()));
}

fn structured_launch_error(code: &str, message: String, details: serde_json::Value) -> ToolResult {
    let mut payload = serde_json::json!({
        "error": code,
    });

    match details {
        serde_json::Value::Object(details) => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.extend(details);
            }
        }
        details => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.insert("details".to_string(), details);
            }
        }
    }

    ToolResult::error(message).with_structured(payload)
}

fn launch_state(requested: bool, process_running: bool, window_ready: bool) -> serde_json::Value {
    serde_json::json!({
        "requested": requested,
        "process_running": process_running,
        "window_ready": window_ready,
    })
}

fn response_identity(
    app_info: Option<&crate::apps::AppInfo>,
    requested_bundle_id: Option<&str>,
    requested_name: Option<&str>,
) -> (String, String) {
    let bundle_id = app_info
        .and_then(|app| app.bundle_id.as_deref())
        .filter(|value| !value.is_empty())
        .or(requested_bundle_id)
        .unwrap_or("?")
        .to_owned();

    let name = app_info
        .map(|app| app.name.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| requested_app_name(requested_name, requested_bundle_id));

    (name, bundle_id)
}

fn requested_app_name(requested_name: Option<&str>, requested_bundle_id: Option<&str>) -> String {
    if let Some(name) = requested_name.filter(|name| Some(*name) != requested_bundle_id) {
        let file_name = std::path::Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(name);
        return file_name
            .strip_suffix(".app")
            .unwrap_or(file_name)
            .to_owned();
    }

    requested_bundle_id
        .and_then(|bundle_id| bundle_id.rsplit('.').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("?")
        .to_owned()
}

fn structured_launch_failure(error: &anyhow::Error) -> ToolResult {
    use crate::apps::nsworkspace::LaunchError;

    let (code, requested) = if let Some(launch_error) = error.downcast_ref::<LaunchError>() {
        match launch_error {
            LaunchError::Cocoa(_) => ("NSWORKSPACE_LAUNCH_FAILED", true),
            LaunchError::NoApp => ("LAUNCH_RESULT_MISSING", true),
            LaunchError::Timeout => ("LAUNCH_CALLBACK_TIMEOUT", true),
            LaunchError::BadUrl(_) => ("APP_URL_INVALID", false),
        }
    } else {
        ("LAUNCH_FAILED", false)
    };

    structured_launch_error(
        code,
        format!("Launch failed: {error:#}"),
        serde_json::json!({
            "launch_state": launch_state(requested, false, false),
        }),
    )
}

fn preflight_file_urls(urls: &[String]) -> Option<ToolResult> {
    for raw in urls {
        let Some(path) = local_file_target(raw) else {
            continue;
        };
        if !path.exists() {
            return Some(structured_launch_error(
                "FILE_NOT_FOUND",
                format!(
                    "Local launch_app url target does not exist: {}",
                    path.display()
                ),
                serde_json::json!({
                    "url": raw,
                    "path": path.display().to_string(),
                }),
            ));
        }
    }
    None
}

fn normalize_launch_url(raw: String) -> String {
    local_file_target(&raw)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(raw)
}

fn local_file_target(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return Some(PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode_path(path);
        return Some(expand_tilde(&decoded));
    }
    let looks_like_url = raw.contains(':') && !raw.starts_with('/') && !raw.starts_with('~');
    if looks_like_url {
        return None;
    }
    Some(expand_tilde(raw))
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

pub(crate) fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                decoded.push((high << 4) | low);
                i += 3;
                continue;
            }
        }

        decoded.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        already_open, is_cua_driver_bundle_id, launch_summary, local_file_target,
        normalize_launch_url, preflight_file_urls, rank_launch_windows, requested_windows,
        response_identity, structured_launch_failure, LaunchAppTool, LaunchWindows,
    };
    use cua_driver_core::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;

    fn window(window_id: u32, title: &str, width: f64, height: f64) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid: 42,
            app_name: "Calculator".to_owned(),
            title: title.to_owned(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            layer: 0,
            z_index: window_id as usize,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn launch_windows_prefer_titled_document_windows_over_chrome_strips() {
        let mut windows = vec![
            window(10174, "", 1470.0, 33.0),
            window(10169, "", 1920.0, 30.0),
            window(10166, "Calculator", 674.0, 408.0),
        ];

        rank_launch_windows(&mut windows);

        assert_eq!(
            windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![10166, 10174, 10169]
        );
    }

    #[test]
    fn launch_window_ranking_preserves_window_server_order_within_groups() {
        let mut windows = vec![
            window(3, "New small document", 300.0, 200.0),
            window(2, "Old large document", 1200.0, 900.0),
            window(1, "", 1920.0, 30.0),
        ];

        rank_launch_windows(&mut windows);

        assert_eq!(
            windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    fn sent(urls: &[&str]) -> Vec<String> {
        urls.iter().map(|u| u.to_string()).collect()
    }

    fn docs(list: &[(u32, Option<&str>)]) -> Vec<(u32, Option<String>)> {
        list.iter()
            .map(|(id, d)| (*id, d.map(str::to_owned)))
            .collect()
    }

    /// A file already shown in a window is matched through AXDocument, file
    /// URL or path, percent-encoded or not; other urls are left to send.
    #[test]
    fn already_open_matches_axdocument_file_urls() {
        let documents = docs(&[
            (5, Some("file:///nonexistent-cua/My%20Notes.txt")),
            (6, None),
        ]);
        let urls = vec![
            "/nonexistent-cua/My Notes.txt".to_owned(),
            "/nonexistent-cua/other.txt".to_owned(),
            "https://example.com".to_owned(),
        ];
        assert_eq!(
            already_open(&urls, &documents),
            vec![("/nonexistent-cua/My Notes.txt".to_owned(), 5)]
        );
    }

    /// `requested_windows` for a mid-wait poll with the given snapshot.
    fn poll(
        sent_urls: &[String],
        opened: &[u32],
        documents: Option<&[(u32, Option<String>)]>,
        titles: &[(u32, &str)],
    ) -> (Vec<u32>, Vec<u32>, bool) {
        requested_windows(
            sent_urls,
            &[],
            false,
            opened,
            documents,
            titles,
            true,
            false,
        )
    }

    /// The requested window of a document app is the one whose AXDocument
    /// is the sent file, not an older window that appeared first (TextEdit
    /// restoring a previous document on a cold launch).
    #[test]
    fn requested_window_is_the_document_window_once_it_names_the_file() {
        let file = sent(&["/nonexistent-cua/probe.txt"]);
        let restored_only = docs(&[(7, Some("file:///nonexistent-cua/old.txt"))]);
        assert_eq!(
            poll(&file, &[7], Some(&restored_only), &[(7, "old.txt")]),
            (vec![], vec![], false),
            "a restored window is not the requested one"
        );
        let both = docs(&[
            (7, Some("file:///nonexistent-cua/old.txt")),
            (8, Some("file:///nonexistent-cua/probe.txt")),
        ]);
        let snapshot = [(7, "old.txt"), (8, "probe.txt")];
        assert_eq!(
            poll(&file, &[7, 8], Some(&both), &snapshot),
            (vec![8], vec![], true)
        );
        assert_eq!(
            poll(&file, &[7], Some(&both), &[(7, "old.txt")]),
            (vec![], vec![], false),
            "a document window the window list does not have yet waits for the next poll"
        );
    }

    /// An app that names no document on any window (a Finder folder window)
    /// and remote urls fall back to the windows the call opened, one per url;
    /// no new window means not ready, never "ready with old windows".
    #[test]
    fn requested_windows_fall_back_to_opened_windows() {
        let folder = sent(&["/nonexistent-cua/folder"]);
        let finder = docs(&[(1, None), (9, None)]);
        let snapshot = [(1, "Desktop"), (9, "folder")];
        assert_eq!(
            poll(&folder, &[9], Some(&finder), &snapshot),
            (vec![9], vec![], true)
        );
        assert_eq!(
            poll(&folder, &[], Some(&finder), &snapshot),
            (vec![], vec![], false)
        );
        let two = sent(&["/nonexistent-cua/a", "/nonexistent-cua/b"]);
        assert_eq!(
            poll(&two, &[9], Some(&finder), &snapshot),
            (vec![9], vec![], false),
            "one new window does not answer two urls"
        );
        let remote = sent(&["https://example.com"]);
        assert_eq!(poll(&remote, &[], None, &[]), (vec![], vec![], false));
        assert_eq!(
            poll(&remote, &[4], None, &[(4, "")]),
            (vec![4], vec![], true)
        );
    }

    /// Finder brings an already-open folder's window forward instead of
    /// opening one: on the last poll, with no new window, the one window
    /// titled with the folder's name answers. Never before the last poll,
    /// never when two windows share the title, never one window for two urls.
    #[test]
    fn existing_window_matched_by_title_only_on_the_last_poll() {
        let last = |urls: &[String], opened: &[u32], titles: &[(u32, &str)]| {
            let finder = docs(&[(1, None), (6, None)]);
            requested_windows(urls, &[], false, opened, Some(&finder), titles, true, true)
        };
        let folder = sent(&["/nonexistent-cua/trip"]);
        let titles = [(1, "Desktop"), (6, "trip")];
        assert_eq!(
            poll(&folder, &[], Some(&docs(&[(1, None), (6, None)])), &titles),
            (vec![], vec![], false),
            "a new window may still be on its way"
        );
        assert_eq!(last(&folder, &[], &titles), (vec![6], vec![6], true));
        assert_eq!(
            last(&folder, &[], &[(1, "trip"), (6, "trip")]),
            (vec![], vec![], false)
        );
        assert_eq!(
            last(&folder, &[9], &titles),
            (vec![9], vec![], true),
            "a new window wins over a title match"
        );
        let same_name = sent(&["/nonexistent-cua/a/trip", "/nonexistent-cua/b/trip"]);
        assert_eq!(last(&same_name, &[], &titles), (vec![], vec![], false));
    }

    /// An unreadable document list is unknown, not "no documents": local
    /// files are not ready on a new window alone.
    #[test]
    fn unreadable_documents_never_make_a_file_request_ready() {
        let file = sent(&["/nonexistent-cua/probe.txt"]);
        assert_eq!(
            poll(&file, &[9], None, &[(9, "x")]),
            (vec![], vec![], false)
        );
    }

    /// Nothing sent: a fully reused request is ready with its windows; a
    /// plain launch is ready when the app has any window; a reused window
    /// that closed leaves its url unanswered, here and alongside sent urls.
    #[test]
    fn requested_windows_without_sent_urls_and_closed_reused_windows() {
        let none: &[String] = &[];
        let plain = |skipped: &[u32], missing: bool, has_window: bool| {
            requested_windows(none, skipped, missing, &[], None, &[], has_window, false)
        };
        assert_eq!(plain(&[3], false, true), (vec![3], vec![], true));
        assert_eq!(plain(&[], false, true), (vec![], vec![], true));
        assert_eq!(plain(&[], false, false), (vec![], vec![], false));
        assert_eq!(plain(&[], true, true), (vec![], vec![], false));
        let file = sent(&["/nonexistent-cua/b.txt"]);
        let documents = docs(&[(8, Some("file:///nonexistent-cua/b.txt"))]);
        assert_eq!(
            requested_windows(
                &file,
                &[],
                true,
                &[8],
                Some(&documents),
                &[(8, "b.txt")],
                true,
                false
            ),
            (vec![8], vec![], false),
            "the closed reused window's url is still unanswered"
        );
    }

    fn found(requested: &[u32], skipped: &[u32], ready: bool) -> LaunchWindows {
        LaunchWindows {
            windows: vec![
                window(9, "trip", 800.0, 600.0),
                window(1, "Desktop", 800.0, 600.0),
            ],
            opened: vec![9],
            requested: requested.to_vec(),
            skipped: skipped.to_vec(),
            matched_by_title: Vec::new(),
            ready,
        }
    }

    /// The text never says "not activated" when the app was seen in front,
    /// and names the opened or reused window.
    #[test]
    fn summary_reports_activation_and_the_requested_window() {
        let quiet = launch_summary(
            "Finder",
            42,
            "",
            &found(&[9], &[], true),
            true,
            Some((false, true)),
            Some("Terminal"),
        );
        assert!(quiet.starts_with("Launched Finder (pid 42) in background."));
        assert!(quiet.contains("Opened for the request: \"trip\" [window_id: 9]"));
        assert!(quiet.contains("- \"trip\" [window_id: 9] (new)"));
        assert!(quiet.contains("The app was not activated"));

        let flashed = launch_summary(
            "Finder",
            42,
            "",
            &found(&[9], &[], true),
            true,
            Some((true, true)),
            Some("Terminal"),
        );
        assert!(!flashed.contains("in background"));
        assert!(!flashed.contains("not activated"));
        assert!(flashed
            .contains("Finder came to the front during the call; Terminal was put back in front."));

        let stuck = launch_summary(
            "Finder",
            42,
            "",
            &found(&[9], &[], true),
            true,
            Some((true, false)),
            Some("Terminal"),
        );
        assert!(stuck.contains("Terminal is not back in front"));

        let reused = launch_summary(
            "Finder",
            42,
            "",
            &found(&[1], &[1], true),
            true,
            Some((false, true)),
            None,
        );
        assert!(reused.contains(
            "Already open, reused (nothing sent to the app): \"Desktop\" [window_id: 1]"
        ));

        let unchecked = launch_summary("Finder", 42, "", &found(&[9], &[], true), true, None, None);
        assert!(!unchecked.contains("in background"));
        assert!(!unchecked.contains("not activated"));

        let missing = launch_summary("Finder", 42, "", &found(&[], &[], false), true, None, None);
        assert!(missing.contains("No window for every requested url appeared in time"));
    }

    #[test]
    fn local_file_target_treats_plain_paths_as_files() {
        assert_eq!(
            local_file_target("/tmp/does-not-exist.md"),
            Some(PathBuf::from("/tmp/does-not-exist.md"))
        );
        assert_eq!(
            local_file_target("relative/path.md"),
            Some(PathBuf::from("relative/path.md"))
        );
    }

    #[test]
    fn launch_url_normalization_expands_home_relative_paths() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for macOS"));

        assert_eq!(
            PathBuf::from(normalize_launch_url("~/Desktop/BenchInbox".to_owned())),
            home.join("Desktop/BenchInbox")
        );
        assert_eq!(
            normalize_launch_url("https://example.com".to_owned()),
            "https://example.com"
        );
    }

    #[test]
    fn local_file_target_skips_remote_and_custom_schemes() {
        assert_eq!(local_file_target("https://example.com"), None);
        assert_eq!(local_file_target("about:blank"), None);
        assert_eq!(local_file_target("myapp://open/item"), None);
    }

    #[test]
    fn preflight_file_urls_returns_structured_file_not_found() {
        let missing = "/tmp/cua-driver-definitely-missing-file-for-test.md".to_string();
        let result = preflight_file_urls(&[missing]).expect("missing file should error");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured error");
        assert_eq!(structured["error"], "FILE_NOT_FOUND");
        assert_eq!(
            structured["path"],
            "/tmp/cua-driver-definitely-missing-file-for-test.md"
        );
        assert!(structured.get("details").is_none());
    }

    #[test]
    fn local_file_target_percent_decodes_file_urls_before_path_checks() {
        assert_eq!(
            local_file_target("file:///tmp/My%20Doc.txt"),
            Some(PathBuf::from("/tmp/My Doc.txt"))
        );
        assert_eq!(
            local_file_target("file://localhost/tmp/%E2%9C%93.txt"),
            Some(PathBuf::from("/tmp/✓.txt"))
        );
    }

    #[test]
    fn recognizes_release_and_local_protected_host_bundle_ids() {
        assert!(is_cua_driver_bundle_id("com.trycua.driver"));
        assert!(is_cua_driver_bundle_id("com.trycua.driver.local"));
        assert!(!is_cua_driver_bundle_id("com.trycua.harness.tauri"));
    }

    #[test]
    fn launch_timeout_reports_requested_without_process_or_window() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::Timeout)
            .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured["error"], "LAUNCH_CALLBACK_TIMEOUT");
        assert_eq!(structured["launch_state"]["requested"], true);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn invalid_url_reports_request_was_not_sent() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::BadUrl(
            "bad url".to_owned(),
        ))
        .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(structured["error"], "APP_URL_INVALID");
        assert_eq!(structured["launch_state"]["requested"], false);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn process_only_response_falls_back_to_requested_identity() {
        assert_eq!(
            response_identity(None, Some("com.apple.Safari"), None),
            ("Safari".to_owned(), "com.apple.Safari".to_owned())
        );
        assert_eq!(
            response_identity(
                None,
                Some("com.example.Editor"),
                Some("/Applications/Example Editor.app"),
            ),
            ("Example Editor".to_owned(), "com.example.Editor".to_owned())
        );
    }

    #[tokio::test]
    async fn launch_app_cannot_reach_private_permission_host_entrypoint() {
        let result = LaunchAppTool
            .invoke(json!({
                "bundle_id": "com.example.not-installed",
                "additional_arguments": [
                    "__permissions-host-request",
                    "--result-file",
                    "/tmp/cua-driver-permissions-forged.json"
                ]
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );

        let result = LaunchAppTool
            .invoke(json!({ "bundle_id": "com.trycua.driver" }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );
    }
}
