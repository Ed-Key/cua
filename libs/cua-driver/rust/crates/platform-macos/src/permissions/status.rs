//! Low-level TCC permission status probes.
//!
//! Mirrors Swift `Permissions.currentStatus()` / `Permissions.requestAccessibility()`
//! / `Permissions.requestScreenRecording()` — bare booleans, no UI.  Used by
//! both the `check_permissions` MCP tool and the startup permissions gate
//! (`super::gate`).

/// Snapshot of which TCC grants are active for the current process.
///
/// Field naming matches the JSON shape used by the `check_permissions`
/// MCP tool: `accessibility` + `screen_recording`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PermissionsStatus {
    pub accessibility: bool,
    #[serde(rename = "screen_recording")]
    pub screen_recording: bool,
}

impl PermissionsStatus {
    /// True when **both** required TCC grants are active.
    pub fn all_granted(self) -> bool {
        self.accessibility && self.screen_recording
    }
}

/// Read the live TCC status for both required grants.  Cheap to call
/// repeatedly — both probes are quick C-level calls into the system
/// TCC daemon.
///
/// Mirrors Swift `Permissions.currentStatus()`.  Difference: Swift uses
/// `SCShareableContent.excludingDesktopWindows` (ScreenCaptureKit) for
/// the screen recording probe, which is unavailable from Rust without
/// large bindings.  `CGPreflightScreenCaptureAccess` is Apple's
/// documented preflight API for the same grant and is accurate on
/// macOS 11+.
pub fn current_status() -> PermissionsStatus {
    PermissionsStatus {
        accessibility: accessibility_granted(),
        screen_recording: screen_recording_granted(),
    }
}

/// Live AX trust state — `AXIsProcessTrusted()`.
pub fn accessibility_granted() -> bool {
    unsafe { crate::ax::bindings::AXIsProcessTrusted() }
}

/// Live Screen Recording grant state — `CGPreflightScreenCaptureAccess()`.
///
/// This is the only probe.  Earlier versions fell back to
/// `!all_windows().is_empty()` when preflight returned false, on the
/// theory that `CGWindowListCopyWindowInfo` returning real windows
/// implied the grant was active.  That theory is wrong:
/// `CGWindowListCopyWindowInfo` returns window IDs and bounds for **any**
/// process without requiring the Screen Recording grant — only window
/// titles are gated.  The fallback therefore returned `true` on any
/// populated desktop regardless of grant state, which (a) made
/// `check_permissions` report a false positive after `tccutil reset
/// ScreenCapture com.trycua.driver`, and (b) short-circuited the
/// startup permissions gate's prompt for users who had never granted SR.
pub fn screen_recording_granted() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Shown wherever a prompt was skipped because the executable is bare.
pub const BARE_EXECUTABLE_PROMPT_NOTE: &str =
    "This cua-driver executable is not inside an installed app bundle (for example a \
     cargo build), so it does not raise macOS permission prompts and runs on the \
     permissions of the app that launched it (your terminal or IDE). Grant \
     Accessibility and Screen Recording to that app in System Settings, or run the \
     installed CuaDriver app instead (`cua-driver permissions grant` sets it up).";

/// True when this process runs from `<Name>.app/Contents/MacOS/<exe>` inside an
/// app bundle that declares a bundle identifier (CuaDriver.app,
/// CuaDriverLocal.app, or an embedding host app).
///
/// Only such a process may raise TCC prompts. A bare executable (a cargo
/// build) gets a new ad-hoc code identity on every build, so each prompt adds
/// another System Settings row that can never stay granted. The executable is
/// canonicalized first so a CLI symlink into the installed app still counts.
pub fn running_from_app_bundle() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .is_ok_and(|exe| bundle_identifier_for_executable(&exe).is_some())
    })
}

/// Bundle identifier of the `.app` that directly contains `executable` as
/// `Contents/MacOS/<exe>`, or `None` for any other layout.
fn bundle_identifier_for_executable(executable: &std::path::Path) -> Option<String> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    if macos.file_name()? != "MacOS"
        || contents.file_name()? != "Contents"
        || app.extension()? != "app"
    {
        return None;
    }
    let url = core_foundation::url::CFURL::from_path(app, true)?;
    bundle_identifier(&core_foundation::bundle::CFBundle::new(url)?)
}

/// Non-empty `CFBundleIdentifier` of `bundle`.
pub(crate) fn bundle_identifier(bundle: &core_foundation::bundle::CFBundle) -> Option<String> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    unsafe {
        let id_ref = core_foundation::bundle::CFBundleGetIdentifier(bundle.as_concrete_TypeRef());
        if id_ref.is_null() {
            return None;
        }
        let id = CFString::wrap_under_get_rule(id_ref).to_string();
        (!id.is_empty()).then_some(id)
    }
}

/// Raise the Accessibility TCC prompt if not yet granted.  No-op when
/// already active, and never prompts outside an app bundle
/// ([`running_from_app_bundle`]).  Mirrors Swift `Permissions.requestAccessibility()`.
pub fn request_accessibility() -> bool {
    if !running_from_app_bundle() {
        return accessibility_granted();
    }
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let val = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), val.as_CFType())]);
    unsafe { crate::ax::bindings::AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
}

/// Raise the Screen Recording TCC prompt if not yet granted.  Never prompts
/// outside an app bundle ([`running_from_app_bundle`]).
/// Mirrors Swift `Permissions.requestScreenRecording()`.
pub fn request_screen_recording() -> bool {
    if !running_from_app_bundle() {
        return screen_recording_granted();
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    unsafe { CGRequestScreenCaptureAccess() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_app(root: &std::path::Path, info_plist: Option<&str>) -> std::path::PathBuf {
        let macos = root.join("Fake.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("cua-driver");
        std::fs::write(&exe, b"").unwrap();
        if let Some(plist) = info_plist {
            std::fs::write(root.join("Fake.app/Contents/Info.plist"), plist).unwrap();
        }
        exe
    }

    #[test]
    fn bundle_check_requires_app_layout_with_a_bundle_identifier() {
        let with_id = tempfile::tempdir().unwrap();
        let exe = fake_app(
            with_id.path(),
            Some(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.fake</string>
<key>CFBundleExecutable</key><string>cua-driver</string>
</dict></plist>"#,
            ),
        );
        assert_eq!(
            bundle_identifier_for_executable(&exe).as_deref(),
            Some("com.example.fake")
        );

        let without_plist = tempfile::tempdir().unwrap();
        let exe = fake_app(without_plist.path(), None);
        assert_eq!(bundle_identifier_for_executable(&exe), None);

        let bare = tempfile::tempdir().unwrap();
        let exe = bare.path().join("debug/cua-driver");
        assert_eq!(bundle_identifier_for_executable(&exe), None);
        // Inside an .app but not at Contents/MacOS/<exe>.
        let nested = with_id
            .path()
            .join("Fake.app/Contents/Resources/cua-driver");
        assert_eq!(bundle_identifier_for_executable(&nested), None);
    }

    #[test]
    fn bare_test_binary_is_not_a_bundle() {
        // `cargo test` runs this from target/debug/deps, outside any bundle, so
        // request_* take the non-prompting branch. (Calling them here would
        // raise a real prompt if that branch ever regressed.)
        assert!(!running_from_app_bundle());
    }
}
