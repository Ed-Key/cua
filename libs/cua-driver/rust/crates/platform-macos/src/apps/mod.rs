//! macOS app enumeration via NSWorkspace and NSRunningApplication.

pub mod nsworkspace;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub running: bool,
    pub active: bool,
    /// Per-platform "how launch_app would consume this entry".
    ///
    /// On macOS: filesystem path to the `.app` bundle (e.g. `/Applications/Safari.app`)
    /// when known. `None` for entries that came only from `NSWorkspace`'s
    /// runtime list and whose bundle path could not be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_path: Option<String>,
    /// Kind discriminator. macOS reports `"desktop"` for every `.app` bundle.
    /// Reserved for future use on platforms with packaged-app distinctions
    /// (e.g. Windows UWP packages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// RFC3339 timestamp of the launcher's filesystem `LastAccessTime` /
    /// `mtime`, when available. `None` when the field could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
}

/// Enumerate running apps with NSApplicationActivationPolicyRegular, one
/// entry per process, sorted by name then pid.
///
/// This stays inside AppKit and libproc, so listing or classifying an
/// application never triggers the macOS Automation permission for System
/// Events. `active` is the WindowServer's front process (see
/// [`frontmost_pid`]), read once for the whole list.
pub fn list_running_apps() -> Vec<AppInfo> {
    let front = frontmost_pid();
    let mut apps = list_running_apps_native(front);
    apps.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    apps
}

/// Every live pid, from the kernel's process table; `None` when it could
/// not be read.
fn live_pids() -> Option<Vec<i32>> {
    // SAFETY: a null buffer asks for the count; the second call writes at
    // most `buffer.len()` pids and returns how many it wrote.
    unsafe {
        let mut room = libc::proc_listallpids(std::ptr::null_mut(), 0);
        for _ in 0..3 {
            if room <= 0 {
                return None;
            }
            let mut buffer = vec![0i32; room as usize + 64];
            let bytes = (buffer.len() * std::mem::size_of::<i32>()) as libc::c_int;
            let written = libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes);
            if written <= 0 {
                return None;
            }
            // A full buffer may have cut processes started since the count.
            if (written as usize) < buffer.len() {
                buffer.truncate(written as usize);
                buffer.retain(|&pid| pid > 0);
                buffer.sort_unstable();
                return Some(buffer);
            }
            room = written * 2;
        }
        None
    }
}

/// The running applications, read fresh on every call.
///
/// `NSWorkspace.runningApplications` and `frontmostApplication` are copies
/// that only an NSApplication run loop refreshes, late (1-2 s) or never: in
/// the `--no-overlay` daemon (a bare CFRunLoop) a relaunched app kept its dead
/// pid and the frontmost app never changed. `NSRunningApplication` looked up
/// by pid answers from LaunchServices at call time (under 1 ms for 600
/// processes). Falls back to the workspace copy when the process table cannot
/// be read. Call inside an autorelease pool, with every property read: the
/// lookups autorelease temporaries that a long-lived thread never drains.
fn running_applications() -> Vec<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>> {
    use objc2_app_kit::{NSRunningApplication, NSWorkspace};
    let apps: Vec<_> = match live_pids() {
        Some(pids) => pids
            .into_iter()
            .filter_map(|pid| unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid) })
            .collect(),
        None => unsafe {
            let running = NSWorkspace::sharedWorkspace().runningApplications();
            (0..running.count()).map(|index| running.objectAtIndex(index)).collect()
        },
    };
    apps.into_iter().filter(|app| unsafe { !app.isTerminated() }).collect()
}

fn list_running_apps_native(front: Option<i32>) -> Vec<AppInfo> {
    objc2::rc::autoreleasepool(|_| {
        running_applications()
            .iter()
            .filter_map(|app| app_info(app, front))
            .collect()
    })
}

/// The running regular app with `pid` (one fresh lookup, not a scan), or
/// `None` when no such app runs.
pub fn running_app(pid: i32) -> Option<AppInfo> {
    use objc2_app_kit::NSRunningApplication;
    let front = frontmost_pid();
    objc2::rc::autoreleasepool(|_| {
        let app = unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid) }?;
        app_info(&app, front)
    })
}

/// The `.app` path a running application was launched from, when known.
fn bundle_path(app: &objc2_app_kit::NSRunningApplication) -> Option<String> {
    unsafe {
        app.bundleURL()
            .and_then(|url| url.path())
            .map(|path| path.to_string())
    }
}

/// A regular, live app as [`AppInfo`]; `None` for anything else. `front`
/// is the WindowServer's front pid.
fn app_info(app: &objc2_app_kit::NSRunningApplication, front: Option<i32>) -> Option<AppInfo> {
    use objc2_app_kit::NSApplicationActivationPolicy;
    unsafe {
        if app.isTerminated() || app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            return None;
        }
        let name = app.localizedName().map(|value| value.to_string())?;
        let pid = app.processIdentifier();
        if name.is_empty() || pid <= 0 {
            return None;
        }
        Some(AppInfo {
            name,
            pid,
            bundle_id: app.bundleIdentifier().map(|value| value.to_string()),
            running: true,
            active: front == Some(pid),
            launch_path: bundle_path(app),
            kind: Some("desktop".to_owned()),
            last_used: None,
        })
    }
}

/// Live `(pid, active, bundle path)` entries per bundle identifier.
type RunningAppStates = std::collections::HashMap<String, Vec<(i32, bool, Option<String>)>>;

/// Live state for every running process that reports a bundle identifier,
/// across ALL activation policies. The `Regular`-only filter of
/// [`list_running_apps`] must not decide whether an *installed* app is
/// running: bundles shipped with `LSUIElement = true` (Cua Driver itself,
/// many menu-bar apps) run as `Accessory`, never enter the standalone list,
/// and would otherwise surface as `running = false / pid = 0` while windows
/// and the accessibility tree see the live process (#3060). Read from the
/// same fresh lookups as the running list, with `active` from `front`.
fn running_app_states(front: Option<i32>) -> RunningAppStates {
    let mut states = RunningAppStates::new();
    objc2::rc::autoreleasepool(|_| {
        for app in running_applications() {
            let pid = unsafe { app.processIdentifier() };
            let Some(bundle_id) = (unsafe { app.bundleIdentifier() }).map(|value| value.to_string()) else {
                continue;
            };
            if pid <= 0 || bundle_id.is_empty() {
                continue;
            }
            states
                .entry(bundle_id)
                .or_default()
                .push((pid, front == Some(pid), bundle_path(&app)));
        }
    });
    states
}

/// Launch an app by bundle ID via NSWorkspace, background only (no focus
/// steal). Returns the pid on success.
///
/// Replaces a prior `open -g -b` shell-out. The NSWorkspace path:
///   * honors `activates = false` so LaunchServices doesn't bring the
///     target frontmost,
///   * attaches an `aevt/oapp` AppleEvent descriptor so cold-launched
///     apps (Calculator, etc) get their window-creation handler invoked
///     reliably (the shell-out path silently skipped this for state-
///     restored apps),
///   * returns the actual `NSRunningApplication.processIdentifier`
///     without needing a separate `list_running_apps` lookup, so we
///     can't race a same-bundle-id helper that happens to be running.
pub fn launch_app(bundle_id: &str) -> anyhow::Result<i32> {
    // Pass the bundle id straight through — `nsworkspace::resolve_application_url`
    // calls `URLForApplicationWithBundleIdentifier` and uses the resulting
    // NSURL verbatim. Going via a `path` string and back loses the
    // alias/cryptex metadata Safari (and other Cryptex-installed apps)
    // need to relaunch from `/System/Cryptexes/App/...`.
    let cfg = nsworkspace::OpenConfig {
        apple_event_bundle_id: Some(bundle_id.to_owned()),
        ..Default::default()
    };
    let running = nsworkspace::open_application(bundle_id, &cfg)
        .with_context(|| format!("Failed to launch {bundle_id}"))?;
    let pid: i32 = unsafe { running.processIdentifier() };
    Ok(pid)
}

/// Launch an app by display name via NSWorkspace. Background-only.
/// Returns the pid on success.
///
/// Mirror of Swift `AppLauncher.locate(name:)`: scan the standard
/// roots for `<Name>.app`, then fall back to a LaunchServices lookup
/// in case the caller passed a bundle identifier in the `name` slot.
pub fn launch_app_by_name(name: &str) -> anyhow::Result<i32> {
    let located = locate_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("Could not locate app with name '{name}'"))?;
    let (app_ref, bid) = located.app_ref_and_bundle_id();
    let cfg = nsworkspace::OpenConfig {
        apple_event_bundle_id: bid,
        ..Default::default()
    };
    let running = nsworkspace::open_application(&app_ref, &cfg)
        .with_context(|| format!("Failed to launch '{name}'"))?;
    let pid: i32 = unsafe { running.processIdentifier() };
    Ok(pid)
}

/// Launch a bundle with URL handoff. Mirrors Swift's
/// `NSWorkspace.open(urls:withApplicationAt:configuration:)` flow.
///
/// `additional_args` and `env` are merged into the `OpenConfig`.
/// `creates_new_instance` corresponds to AppKit's
/// `createsNewApplicationInstance = true`.
pub fn launch_with_urls_by_bundle(
    bundle_id: &str,
    urls: &[String],
    additional_args: &[String],
    env: &std::collections::HashMap<String, String>,
    creates_new_instance: bool,
) -> anyhow::Result<i32> {
    // Pass the bundle id directly — see `launch_app` rationale above
    // (Cryptex-installed apps).
    //
    // Only attach the `oapp` AppleEvent on the no-URL path. With URLs
    // present, the URL-handoff path delivers its own `aevt/odoc` to
    // the target and attaching `oapp` on top causes
    // openURLs:withApplicationAtURL: to bail with "application not
    // found" for Cryptex-installed apps (Safari). Verified empirically.
    let cfg = nsworkspace::OpenConfig {
        arguments: additional_args.to_vec(),
        environment: env.clone(),
        creates_new_instance,
        apple_event_bundle_id: if urls.is_empty() {
            Some(bundle_id.to_owned())
        } else {
            None
        },
    };
    let running = if urls.is_empty() {
        nsworkspace::open_application(bundle_id, &cfg)
    } else {
        nsworkspace::open_urls_with_application(urls, bundle_id, &cfg)
    }
    .with_context(|| format!("Failed to launch {bundle_id}"))?;
    let pid: i32 = unsafe { running.processIdentifier() };
    Ok(pid)
}

/// Launch by name with URL handoff. Same contract as
/// `launch_with_urls_by_bundle` but resolves the bundle URL by display
/// name first.
pub fn launch_with_urls_by_name(
    name: &str,
    urls: &[String],
    additional_args: &[String],
    env: &std::collections::HashMap<String, String>,
    creates_new_instance: bool,
) -> anyhow::Result<i32> {
    let located = locate_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("Could not locate app with name '{name}'"))?;
    let (app_ref, bid) = located.app_ref_and_bundle_id();
    // See `launch_with_urls_by_bundle` — skip `oapp` AppleEvent on
    // the URL-handoff path.
    let cfg = nsworkspace::OpenConfig {
        arguments: additional_args.to_vec(),
        environment: env.clone(),
        creates_new_instance,
        apple_event_bundle_id: if urls.is_empty() { bid } else { None },
    };
    let running = if urls.is_empty() {
        nsworkspace::open_application(&app_ref, &cfg)
    } else {
        nsworkspace::open_urls_with_application(urls, &app_ref, &cfg)
    }
    .with_context(|| format!("Failed to launch '{name}'"))?;
    let pid: i32 = unsafe { running.processIdentifier() };
    Ok(pid)
}

// ── Bundle resolution ────────────────────────────────────────────────────────

/// What `locate_by_name` resolved a display name into.
///
/// Two shapes because Cryptex-installed apps (Safari on macOS Sonoma+,
/// and a growing set of other system apps) live under
/// `/System/Cryptexes/App/...` — flattening their LaunchServices NSURL
/// to a filesystem path via `-[NSURL path]` loses the
/// alias/cryptex metadata LaunchServices needs to relaunch the bundle.
/// The fix: when the resolver went through LaunchServices, hand the
/// bundle id back to the launch helpers verbatim — they pass it
/// straight through to `URLForApplicationWithBundleIdentifier` and
/// use the resulting NSURL unmodified.
pub(crate) enum AppLocator {
    /// Found by filesystem scan (`/Applications/...`, `~/Applications/...`).
    /// Path-based launch is safe here — these apps aren't Cryptex-installed.
    Path(String),
    /// Found via LaunchServices bundle-id lookup. Carry the bundle id
    /// (NOT the lossy `url.path()`) so the launch helpers re-resolve
    /// the live NSURL on demand and preserve cryptex metadata.
    BundleId(String),
}

impl AppLocator {
    /// `(app_ref_for_nsworkspace, optional_bundle_id_for_oapp_event)`.
    ///
    /// `app_ref` is the string the `nsworkspace::*` helpers consume —
    /// a filesystem path or a bundle id; either flows through
    /// `resolve_application_url` correctly. `bundle_id` is `Some(...)`
    /// when known (either because LaunchServices gave it to us or
    /// because we read it from the bundle's Info.plist) and `None`
    /// when it couldn't be determined — callers use it for the `oapp`
    /// AppleEvent attachment on the no-URL launch path.
    pub(crate) fn app_ref_and_bundle_id(self) -> (String, Option<String>) {
        match self {
            AppLocator::Path(p) => {
                let bid = bundle_id_for_app_path(&p);
                (p, bid)
            }
            AppLocator::BundleId(bid) => (bid.clone(), Some(bid)),
        }
    }
}

/// Resolve a bundle id to its `AppLocator::BundleId` form.
///
/// Returns `None` if LaunchServices can't find an app for the given
/// bundle id. Note: we deliberately do NOT call `-[NSURL path]` on
/// the resolved URL — that's the fix for CodeRabbit #3 (Cryptex
/// relaunch). Callers should pass the bundle id back to the
/// `nsworkspace::*` helpers, which re-resolve the live NSURL.
pub(crate) fn resolve_bundle_id_to_locator(bundle_id: &str) -> Option<AppLocator> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;
    unsafe {
        let ws = NSWorkspace::sharedWorkspace();
        let ns = NSString::from_str(bundle_id);
        // We only care about presence here — the live NSURL is
        // re-fetched inside `nsworkspace::resolve_application_url`
        // when the launch actually fires. Returning the bundle id
        // (not a flattened `url.path()`) preserves the alias/cryptex
        // metadata Safari needs to relaunch from `/System/Cryptexes/App/...`.
        let _url = ws.URLForApplicationWithBundleIdentifier(&ns)?;
        Some(AppLocator::BundleId(bundle_id.to_owned()))
    }
}

/// Mirror of Swift's `AppLauncher.locate(name:)`.
///
/// 1. filesystem lookup by bundle filename in the canonical roots
///    (system first so /Applications wins over ~/Applications);
/// 2. LaunchServices bundle-id lookup, in case the caller passed a
///    bundle identifier in the `name` slot — preserved as
///    `AppLocator::BundleId` so the launch path uses the live NSURL
///    (Cryptex-safe — see CodeRabbit #3);
/// 3. (skipped) full localized-name scan — not yet needed by current
///    integration tests; can be added if we hit a non-English-name app
///    in the wild.
pub(crate) fn locate_by_name(name: &str) -> Option<AppLocator> {
    let app_name = if name.ends_with(".app") {
        name.to_owned()
    } else {
        format!("{name}.app")
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [
        "/Applications".to_owned(),
        "/System/Applications".to_owned(),
        "/System/Applications/Utilities".to_owned(),
        "/Applications/Utilities".to_owned(),
        format!("{home}/Applications"),
        format!("{home}/Applications/Chrome Apps.localized"),
    ];
    for root in &roots {
        let path = format!("{root}/{app_name}");
        if std::path::Path::new(&path).is_dir() {
            return Some(AppLocator::Path(path));
        }
    }
    // Fallback: maybe caller passed a bundle id as `name`. Use the
    // Cryptex-safe locator (carries the bundle id, never the lossy
    // url.path()).
    resolve_bundle_id_to_locator(name)
}

/// Read `CFBundleIdentifier` from an `.app` bundle's `Info.plist`.
/// Falls back to shelling out to `plutil` (already used elsewhere in
/// this file) to avoid pulling in a plist crate just for this.
fn bundle_id_for_app_path(app_path: &str) -> Option<String> {
    let plist = format!("{app_path}/Contents/Info.plist");
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-", &plist])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let bid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if bid.is_empty() {
        None
    } else {
        Some(bid)
    }
}

/// Return all apps: running apps merged with installed-but-not-running apps.
///
/// Single flat array. Each entry carries:
///   * `running` (true for currently-live processes, false for installed-only),
///   * `pid` (live pid when running, `0` otherwise),
///   * `launch_path` (filesystem `.app` path when known, else `None`),
///   * `kind` (`"desktop"` on macOS).
pub fn list_all_apps() -> Vec<AppInfo> {
    // One front pid for both views, so the running list and the installed
    // entries upgraded from live state agree on which app is active.
    let front = frontmost_pid();
    let mut running = list_running_apps_native(front);
    running.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    let running_states = running_app_states(front);
    let installed = scan_installed_apps();
    merge_app_lists(running, installed, &running_states)
}

/// Pure merge behind [`list_all_apps`] — extracted so the identity rules
/// are testable without a live NSWorkspace:
///
/// * standalone entries keep the `Regular`-only contract of
///   [`list_running_apps`] (helpers and UI agents stay out of the list);
/// * installed entries resolve `running` / `pid` / `active` against the
///   all-policies running-state map by bundle id and bundle path, so an installed `.app`
///   whose process runs as an accessory reports its live state instead
///   of the `pid = 0` scan defaults (#3060). Exact bundle paths distinguish
///   installed copies that share an identifier. Bundle-id-only fallback is
///   allowed only when the installed copy is unambiguous;
/// * installed entries already covered by a standalone running entry are
///   dropped — the standalone entry wins and is backfilled with the
///   `launch_path` / `last_used` the installed scan resolved.
pub(crate) fn merge_app_lists(
    mut running: Vec<AppInfo>,
    mut installed: Vec<AppInfo>,
    running_states: &RunningAppStates,
) -> Vec<AppInfo> {
    // Lookup: bundle_id → (launch_path, last_used) from the installed scan.
    let installed_by_bundle: std::collections::HashMap<String, (Option<String>, Option<String>)> =
        installed
            .iter()
            .filter_map(|a| {
                a.bundle_id
                    .clone()
                    .map(|b| (b, (a.launch_path.clone(), a.last_used.clone())))
            })
            .collect();
    // Backfill running entries with the launch_path the installed scan resolved.
    for app in running.iter_mut() {
        if let Some(bid) = &app.bundle_id {
            if let Some((path, last_used)) = installed_by_bundle.get(bid) {
                if app.launch_path.is_none() {
                    app.launch_path = path.clone();
                }
                if app.last_used.is_none() {
                    app.last_used = last_used.clone();
                }
            }
        }
    }

    let installed_bundle_counts = installed
        .iter()
        .filter_map(|app| app.bundle_id.clone())
        .fold(
            std::collections::HashMap::<String, usize>::new(),
            |mut counts, bundle| {
                *counts.entry(bundle).or_default() += 1;
                counts
            },
        );

    // Upgrade installed entries whose live process runs outside the Regular
    // list. Prefer the exact bundle path so duplicate debug/release copies do
    // not all inherit one process's live state.
    for app in installed.iter_mut() {
        let Some(bundle_id) = app.bundle_id.as_deref() else {
            continue;
        };
        let Some(candidates) = running_states.get(bundle_id) else {
            continue;
        };
        let exact = app.launch_path.as_deref().and_then(|installed_path| {
            candidates
                .iter()
                .find(|(_, _, live_path)| live_path.as_deref() == Some(installed_path))
        });
        let selected = exact.or_else(|| {
            (installed_bundle_counts.get(bundle_id) == Some(&1))
                .then(|| candidates.last())
                .flatten()
        });
        if let Some(&(pid, active, _)) = selected {
            app.running = true;
            app.pid = pid;
            app.active = active;
        }
    }

    let running_bundles: std::collections::HashSet<String> =
        running.iter().filter_map(|a| a.bundle_id.clone()).collect();

    // Remove apps already in running list.
    installed.retain(|a| {
        !a.bundle_id
            .as_ref()
            .is_some_and(|b| running_bundles.contains(b))
    });

    running.extend(installed);
    running
}

fn scan_installed_apps() -> Vec<AppInfo> {
    let dirs = [
        "/Applications",
        "/Applications/Utilities",
        "/System/Applications",
        "/System/Applications/Utilities",
    ];
    let home = std::env::var("HOME").unwrap_or_default();
    let user_apps = format!("{home}/Applications");

    let mut result = Vec::new();
    let mut all_dirs: Vec<&str> = dirs.to_vec();
    let user_apps_str: &str = user_apps.as_str();
    all_dirs.push(user_apps_str);

    for dir in all_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            let plist_path = path.join("Contents/Info.plist");
            if let Some(mut info) = read_app_plist(&plist_path) {
                info.launch_path = path.to_str().map(str::to_owned);
                info.kind = Some("desktop".to_owned());
                info.last_used = fs_last_used(&path);
                result.push(info);
            }
        }
    }
    result
}

/// Read the bundle's filesystem `mtime` and serialize as RFC3339.
/// Used as `last_used` heuristic — macOS doesn't reliably surface
/// LaunchServices' true "last launched" timestamp without entitlements,
/// so we approximate with whichever of `atime`/`mtime` the filesystem
/// preserves (mtime is the more portable of the two).
fn fs_last_used(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    cua_driver_core::timestamp::unix_secs_to_rfc3339(duration.as_secs() as i64)
}

fn read_app_plist(plist_path: &std::path::Path) -> Option<AppInfo> {
    let bundle_id_out = Command::new("plutil")
        .args([
            "-extract",
            "CFBundleIdentifier",
            "raw",
            "-o",
            "-",
            plist_path.to_str()?,
        ])
        .output()
        .ok()?;
    if !bundle_id_out.status.success() {
        return None;
    }
    let bundle_id = String::from_utf8_lossy(&bundle_id_out.stdout)
        .trim()
        .to_string();
    if bundle_id.is_empty() {
        return None;
    }

    let name_out = Command::new("plutil")
        .args([
            "-extract",
            "CFBundleDisplayName",
            "raw",
            "-o",
            "-",
            plist_path.to_str()?,
        ])
        .output()
        .ok();
    let name = name_out
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // Fallback: CFBundleName.
            Command::new("plutil")
                .args([
                    "-extract",
                    "CFBundleName",
                    "raw",
                    "-o",
                    "-",
                    plist_path.to_str().unwrap_or(""),
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    plist_path
                        .parent()
                        .and_then(|p| p.parent())
                        .and_then(|p| p.file_stem())
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                })
        });

    if name.is_empty() {
        return None;
    }

    Some(AppInfo {
        name,
        pid: 0,
        bundle_id: Some(bundle_id),
        running: false,
        active: false,
        launch_path: None,
        kind: None,
        last_used: None,
    })
}

/// The pid of the frontmost application, read fresh from the WindowServer.
/// `NSWorkspace.frontmostApplication` went stale in the daemon like its app
/// list (see [`running_applications`]). Without the WindowServer symbols:
/// the running application that says it is active. `None` if there isn't
/// one (rare, e.g. screensaver).
pub fn frontmost_pid() -> Option<i32> {
    crate::input::skylight::front_pid().or_else(|| {
        objc2::rc::autoreleasepool(|_| {
            running_applications()
                .into_iter()
                .find(|app| unsafe { app.isActive() })
                .map(|app| unsafe { app.processIdentifier() })
        })
    })
}

/// Re-activate the app with `pid` via
/// `NSRunningApplication.runningApplicationWithProcessIdentifier(pid)?.activateWithOptions([])`.
/// Put the user's previous app back in front after cua's target took focus.
/// Goes through WindowServer: AppKit activation requested by a background
/// daemon took ~1s to return and did not stick on current macOS (measured
/// with the focus-theft fixture). AppKit is only the fallback when the
/// private call is unavailable.
pub fn restore_prior_app(pid: i32) -> bool {
    crate::input::skylight::restore_front_pid(pid, &mut || true) || activate_pid(pid)
}

/// Returns `true` if the app was found and activate was attempted.
/// For restoring the user's app prefer [`restore_prior_app`].
pub fn activate_pid(pid: i32) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    unsafe {
        match NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            Some(app) => app.activateWithOptions(NSApplicationActivationOptions(0)),
            None => false,
        }
    }
}

/// Return the bundle identifier of the running process for `pid`, via
/// `NSRunningApplication.runningApplicationWithProcessIdentifier(pid)?.bundleIdentifier`.
///
/// Returns `None` when:
/// - the pid is unknown to NSWorkspace (non-AppKit processes, e.g. raw
///   command-line tools)
/// - the running app exposes no bundle id (rare: unbundled `.app`-less
///   processes).
///
/// Used by [`crate::terminal::is_terminal_pid`] to route `type_text` past
/// the AX path when the target window belongs to a terminal emulator.
pub fn bundle_id_for_pid(pid: i32) -> Option<String> {
    use objc2_app_kit::NSRunningApplication;
    unsafe {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        let ns = app.bundleIdentifier()?;
        Some(ns.to_string())
    }
}

/// Return the localized application name for a running process by PID.
/// Uses `ps -p {pid} -o comm=` which gives the command name without path.
/// Returns `None` if the PID is unknown or the command fails.
pub fn get_app_name_for_pid(pid: i32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    // Strip path prefix: "/Applications/Safari.app/Contents/MacOS/Safari" → "Safari"
    Some(
        std::path::Path::new(&raw)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&raw)
            .to_string(),
    )
}

/// Format the app list in the same text style as libs/cua-driver.
pub fn format_app_list(apps: &[AppInfo]) -> String {
    let running: Vec<&AppInfo> = apps.iter().filter(|a| a.running).collect();
    let total = apps.len();
    // Match Swift `ListAppsTool.swift` `summary(_:)` text format 1:1.
    let mut lines = vec![format!(
        "✅ Found {} app(s): {} running, {} installed-not-running.",
        total,
        running.len(),
        total - running.len()
    )];
    for app in &running {
        let bundle = app
            .bundle_id
            .as_deref()
            .map(|b| format!(" [{b}]"))
            .unwrap_or_default();
        lines.push(format!("- {} (pid {}){}", app.name, app.pid, bundle));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{live_pids, merge_app_lists, AppInfo};

    fn app(name: &str, pid: i32, bundle: Option<&str>, running: bool) -> AppInfo {
        AppInfo {
            name: name.to_owned(),
            pid,
            bundle_id: bundle.map(str::to_owned),
            running,
            active: false,
            launch_path: None,
            kind: None,
            last_used: None,
        }
    }

    fn states(pairs: &[(&str, i32, bool, Option<&str>)]) -> super::RunningAppStates {
        pairs.iter().fold(
            super::RunningAppStates::new(),
            |mut states, (bundle, pid, active, path)| {
                states.entry((*bundle).to_owned()).or_default().push((
                    *pid,
                    *active,
                    path.map(str::to_owned),
                ));
                states
            },
        )
    }

    #[test]
    fn installed_app_running_as_accessory_reports_live_state() {
        // The #3060 shape: CuaDriver.app ships LSUIElement=true, so its live
        // process runs as Accessory and never enters the Regular list.
        let mut entry = app("Cua Driver", 0, Some("com.trycua.driver"), false);
        entry.launch_path = Some("/Applications/CuaDriver.app".to_owned());
        let merged = merge_app_lists(
            vec![],
            vec![entry],
            &states(&[(
                "com.trycua.driver",
                31438,
                false,
                Some("/Applications/CuaDriver.app"),
            )]),
        );
        assert_eq!(merged.len(), 1);
        assert!(merged[0].running);
        assert_eq!(merged[0].pid, 31438);
        // The upgrade must not clobber the fields the installed scan owns.
        assert_eq!(
            merged[0].launch_path.as_deref(),
            Some("/Applications/CuaDriver.app")
        );
    }

    #[test]
    fn installed_app_not_running_keeps_scan_defaults() {
        let merged = merge_app_lists(
            vec![],
            vec![app("TextEdit", 0, Some("com.apple.TextEdit"), false)],
            &states(&[]),
        );
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].running);
        assert_eq!(merged[0].pid, 0);
    }

    #[test]
    fn regular_running_app_wins_over_installed_entry() {
        let running = vec![app("Safari", 100, Some("com.apple.Safari"), true)];
        let installed = vec![{
            let mut entry = app("Safari", 0, Some("com.apple.Safari"), false);
            entry.launch_path = Some("/Applications/Safari.app".to_owned());
            entry
        }];
        let merged = merge_app_lists(
            running,
            installed,
            &states(&[(
                "com.apple.Safari",
                100,
                true,
                Some("/Applications/Safari.app"),
            )]),
        );
        assert_eq!(merged.len(), 1);
        assert!(merged[0].running);
        assert_eq!(merged[0].pid, 100);
        assert_eq!(
            merged[0].launch_path.as_deref(),
            Some("/Applications/Safari.app")
        );
    }

    #[test]
    fn accessory_process_without_installed_bundle_adds_no_entry() {
        // A background helper with a bundle id but no installed .app must not
        // materialize a row just because it appears in the states map.
        let merged = merge_app_lists(
            vec![],
            vec![],
            &states(&[("dev.helper.agent", 9, false, None)]),
        );
        assert!(merged.is_empty());
    }

    #[test]
    fn duplicate_bundle_ids_upgrade_only_the_live_bundle_path() {
        let mut release = app("Cua Driver", 0, Some("com.trycua.driver"), false);
        release.launch_path = Some("/Applications/CuaDriver.app".to_owned());
        let mut debug = app("Cua Driver", 0, Some("com.trycua.driver"), false);
        debug.launch_path = Some("/Users/test/CuaDriver.app".to_owned());

        let merged = merge_app_lists(
            vec![],
            vec![release, debug],
            &states(&[(
                "com.trycua.driver",
                42,
                false,
                Some("/Users/test/CuaDriver.app"),
            )]),
        );

        assert!(!merged[0].running);
        assert_eq!(merged[0].pid, 0);
        assert!(merged[1].running);
        assert_eq!(merged[1].pid, 42);
    }

    /// The process table this module reads sees this very process.
    #[test]
    fn live_pids_include_this_process() {
        let own = std::process::id() as i32;
        assert!(live_pids().expect("readable process table").contains(&own));
    }
}
