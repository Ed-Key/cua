//! PiP frame-push hook — registered once by `main.rs` when the
//! `--experimental-pip` flag is on argv.
//!
//! The trait + factory live in the `pip-preview` crate so the platform
//! backends can implement them without depending on `cua-driver-core`.
//! What lives here is just the per-process callback that the tool
//! dispatcher uses to push frames after each successful tool call —
//! a thin shim so `tool.rs` doesn't need to know about `pip-preview`
//! directly and we keep the dependency graph one-directional.
//!
//! Frames carry identity only, never pixels: the dispatcher must not
//! capture inside an action's own dispatch. Backends capture the target
//! on their own worker through `recording::screenshot_for` (the same
//! `SCREENSHOT_FN` the recorder uses).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Synthesized per-call frame payload. Kept structurally identical
/// to `pip_preview::PipFrame` — duplicated here to keep `cua-driver-core`
/// from importing `pip-preview` (the dependency would be circular once
/// platform backends pull both crates in).
pub struct PipHookFrame {
    pub action_label: String,
    pub timestamp_ms: u64,
    /// Private runtime session key (`_session_id`, or "default"). Keys the
    /// per-session panel and its color; never shown to the user.
    pub session_key: String,
    /// Public, caller-chosen session label. Display only.
    pub session_label: Option<String>,
    /// Self-reported MCP client name from `initialize` (e.g. "Claude Code").
    pub client_name: Option<String>,
    /// A process inside the MCP client's process tree (the stdio proxy that
    /// opened the daemon connection). Backends walk up from it to the app.
    pub client_pid: Option<i32>,
    pub target_pid: Option<i32>,
    pub target_window_id: Option<u32>,
}

/// What the daemon knows about the MCP client behind one transport session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipClient {
    pub name: Option<String>,
    pub pid: Option<i32>,
}

/// Transport session id (as the proxy minted it, without the runtime prefix)
/// → client identity. Filled only while a PiP backend is registered.
static CLIENTS: Mutex<Option<HashMap<String, PipClient>>> = Mutex::new(None);

fn with_client(transport_session: &str, f: impl FnOnce(&mut PipClient)) {
    if !pip_enabled() || transport_session.is_empty() {
        return;
    }
    let mut guard = CLIENTS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard
        .get_or_insert_with(HashMap::new)
        .entry(transport_session.to_owned())
        .or_default());
}

/// Record the peer pid of a transport session's control connection.
pub fn note_client_pid(transport_session: &str, pid: i32) {
    with_client(transport_session, |client| client.pid = Some(pid));
}

/// Record the MCP client name the proxy read from `initialize`. Bounded to
/// 64 characters; display only.
pub fn note_client_name(transport_session: &str, name: &str) {
    let name: String = name.trim().chars().take(64).collect();
    if name.is_empty() {
        return;
    }
    with_client(transport_session, |client| client.name = Some(name));
}

/// Drop a transport session's client identity when its connection closes.
pub fn forget_client(transport_session: &str) {
    if let Some(map) = CLIENTS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        map.remove(transport_session);
    }
}

pub fn client_for(transport_session: &str) -> PipClient {
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(transport_session).cloned())
        .unwrap_or_default()
}

type PipPushFnBox = Box<dyn Fn(PipHookFrame) + Send + Sync>;
static PIP_PUSH_FN: OnceLock<PipPushFnBox> = OnceLock::new();

/// Register the platform-side push callback. `main.rs` calls this
/// once after starting the PiP backend.
pub fn set_pip_push_fn(f: impl Fn(PipHookFrame) + Send + Sync + 'static) {
    let _ = PIP_PUSH_FN.set(Box::new(f));
}

/// True when a PiP backend is wired up. Tool dispatcher uses this to
/// skip the screenshot-bytes path when nothing would consume the
/// frame (avoiding wasted capture work in the common --pip-off case).
pub fn pip_enabled() -> bool {
    PIP_PUSH_FN.get().is_some()
}

// ── Bound browser tabs ────────────────────────────────────────────────────
//
// A browser tool names its tab by `target_id` and `tab_id`, not by pid and
// window. The macOS window that tab lives in is known to its binding, and
// whether the tab is the selected tab of that window is known only by
// asking the browser. Both happen inside the tool, so the browser engine
// leaves the answer here for the dispatcher that is running the tool on
// this task.

tokio::task_local! {
    static BOUND_WINDOW: std::cell::Cell<Option<(i32, u32)>>;
}

/// Run one tool dispatch. With its output: the macOS (pid, window) of the
/// bound browser tab it acted on, if the engine noted one.
pub async fn with_bound_window<T>(
    dispatch: impl std::future::Future<Output = T>,
) -> (T, Option<(i32, u32)>) {
    BOUND_WINDOW
        .scope(std::cell::Cell::new(None), async {
            let output = dispatch.await;
            (output, BOUND_WINDOW.with(std::cell::Cell::get))
        })
        .await
}

/// True inside a dispatch that would use a bound window: the engine skips
/// its page probe otherwise.
pub fn wants_bound_window() -> bool {
    pip_enabled() && BOUND_WINDOW.try_with(|_| ()).is_ok()
}

/// The window a bound tab's action is shown in: its binding's macOS pid and
/// window, and only while the tab is the `selected` tab of that window
/// (whether or not the window itself can be seen: the PiP decides about
/// that). A background tab (or one whose state could not be read) has no
/// picture: its window shows another tab.
pub fn bound_tab_window(pid: i64, window_id: u64, selected: bool) -> Option<(i32, u32)> {
    if !selected {
        return None;
    }
    Some((i32::try_from(pid).ok()?, u32::try_from(window_id).ok()?))
}

/// Leave `window` for the dispatcher running on this task. No-op outside
/// `with_bound_window`.
pub fn note_bound_window(window: Option<(i32, u32)>) {
    let _ = BOUND_WINDOW.try_with(|slot| slot.set(window));
}

/// Lifecycle, configuration, recording and other non-GUI tools never
/// update a panel, even when they happen to carry a pid. Nor does
/// `browser_tabs`: its `window_id` is Chrome's own window number, not a
/// macOS window, and its pid alone says nothing about which window a tab is
/// in. The bound-tab actions that follow show the run.
fn pip_meta_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "start_session"
            | "browser_tabs"
            | "end_session"
            | "escalate_session"
            | "set_config"
            | "start_recording"
            | "stop_recording"
            | "replay_trajectory"
            | "install_ffmpeg"
            | "install_extension"
            | "kill_app"
    ) || tool_name.starts_with("get_")
        || tool_name.starts_with("list_")
        || tool_name.starts_with("set_agent_cursor")
}

/// Whether this frame should reach the PiP at all: only GUI actions aimed
/// at a target (pid or window) by an agent session, meaning one with a
/// public session label or a known MCP client. One-shot CLI calls and
/// implicit SDK sessions have neither and get no panel.
pub fn pip_frame_wanted(tool_name: &str, frame: &PipHookFrame) -> bool {
    let agent = frame.session_label.is_some() || frame.client_name.is_some();
    let targeted = frame.target_pid.is_some() || frame.target_window_id.is_some();
    agent && targeted && !pip_meta_tool(tool_name)
}

/// Push a frame to the PiP window. No-op when no backend is registered.
pub fn push_pip_frame(frame: PipHookFrame) {
    if let Some(f) = PIP_PUSH_FN.get() {
        f(frame);
    }
}

// ── Verification events ───────────────────────────────────────────────────
//
// A `verify_state` result reaches the PiP as a list of short, human-readable
// claims ("text area holds \"hello\"") with their status. Labels are built
// here, on the dispatcher side, from the predicate the caller wrote, so
// observed values (which may be secrets) never cross the hook at all.

/// One predicate of a `verify_state` call, for display.
pub struct PipHookClaim {
    /// Which predicate this is (see `predicate_id`): claims are tracked by
    /// it, the label is display only. Opaque; never logged.
    pub id: u64,
    /// At most `CLAIM_MAX_CHARS` characters; never holds a secure field's value.
    pub label: String,
    /// `Some(true)` satisfied, `Some(false)` unsatisfied, `None` unknown.
    pub satisfied: Option<bool>,
}

/// A completed `verify_state` call by an agent session.
pub struct PipHookVerification {
    pub timestamp_ms: u64,
    pub session_key: String,
    pub target_pid: i32,
    pub target_window_id: u32,
    /// The call as a whole was satisfied (every predicate, stably).
    pub satisfied: bool,
    pub claims: Vec<PipHookClaim>,
}

type PipVerifyFnBox = Box<dyn Fn(PipHookVerification) + Send + Sync>;
static PIP_VERIFY_FN: OnceLock<PipVerifyFnBox> = OnceLock::new();

/// Register the platform-side verification callback (non-blocking: it only
/// enqueues). `main.rs` calls this once next to `set_pip_push_fn`.
pub fn set_pip_verify_fn(f: impl Fn(PipHookVerification) + Send + Sync + 'static) {
    let _ = PIP_VERIFY_FN.set(Box::new(f));
}

/// Push a verification to the PiP. No-op when no backend is registered.
pub fn push_pip_verification(verification: PipHookVerification) {
    if let Some(f) = PIP_VERIFY_FN.get() {
        f(verification);
    }
}

/// Longest claim label, in characters.
pub const CLAIM_MAX_CHARS: usize = 40;
/// Longest quoted value or element label inside a claim.
const QUOTE_MAX_CHARS: usize = 16;

/// Build the PiP event for a finished `verify_state` call, from the frame
/// identity the dispatcher built (`pip_frame`), when the call started
/// (`started_ms`), the public input and the structured output. `None` for
/// calls no panel would show (not an agent session) and for malformed
/// input/output.
///
/// The event is stamped with when its predicates were last observed,
/// `started_ms + elapsed_ms` (the output's `elapsed_ms` stops at the final
/// sample, before any screenshot is taken), not when the call returned: an
/// action that lands during the screenshot is newer than this evidence.
pub fn verification_event(
    frame: PipHookFrame,
    started_ms: u64,
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
) -> Option<PipHookVerification> {
    if !pip_frame_wanted("verify_state", &frame) {
        return None;
    }
    let target = (frame.target_pid, frame.target_window_id);
    let expect: Vec<cua_driver_contract::StatePredicate> =
        serde_json::from_value(input.get("expect")?.clone()).ok()?;
    let output = output?;
    let outcomes = output.get("predicates")?.as_array()?;
    let claims = expect
        .iter()
        .enumerate()
        .map(|(index, predicate)| {
            let outcome = outcomes.iter().find(|outcome| {
                outcome.get("index").and_then(|i| i.as_u64()) == Some(index as u64)
            });
            let satisfied = match outcome
                .and_then(|o| o.get("status"))
                .and_then(|s| s.as_str())
            {
                Some("satisfied") => Some(true),
                Some("unsatisfied") => Some(false),
                _ => None,
            };
            // The observed element (bounded JSON) only feeds the secure-field
            // check; none of its values reach the label.
            let observed = outcome
                .and_then(|o| o.get("observed_json"))
                .and_then(|j| j.as_str())
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok());
            PipHookClaim {
                id: predicate_id(target, predicate),
                label: claim_label(predicate, observed.as_ref()),
                satisfied,
            }
        })
        .collect();
    let observed_ms = output
        .get("elapsed_ms")
        .and_then(|elapsed| elapsed.as_u64())
        .map_or(frame.timestamp_ms, |elapsed| {
            started_ms.saturating_add(elapsed).min(frame.timestamp_ms)
        });
    Some(PipHookVerification {
        timestamp_ms: observed_ms,
        session_key: frame.session_key,
        target_pid: frame.target_pid?,
        target_window_id: frame.target_window_id?,
        satisfied: output.get("status").and_then(|s| s.as_str()) == Some("satisfied"),
        claims,
    })
}

/// A predicate's identity, scoped to its target window: equal for the same
/// predicate on the same window, distinct for different ones even when
/// their (truncated) labels collide. A keyed hash of the canonical
/// predicate JSON with a per-process random key, so the value it covers
/// (possibly a secret) cannot be recovered or matched offline; it never
/// leaves the process and is never logged.
pub fn predicate_id(
    target: (Option<i32>, Option<u32>),
    predicate: &cua_driver_contract::StatePredicate,
) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    static KEY: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();
    let mut hasher = KEY
        .get_or_init(std::collections::hash_map::RandomState::new)
        .build_hasher();
    target.hash(&mut hasher);
    serde_json::to_string(predicate)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// A short, human-readable label for one predicate, at most
/// `CLAIM_MAX_CHARS` characters: `text area holds "hello"`, `"Save" button
/// visible`, `window open`. A value is shown only for fields that are not
/// secure (by role or by a password-like label, on the selector or the
/// observed element).
pub fn claim_label(
    predicate: &cua_driver_contract::StatePredicate,
    observed: Option<&serde_json::Value>,
) -> String {
    let label = match (&predicate.window, &predicate.element) {
        (Some(window), None) => window_label(window),
        (None, Some(element)) => element_label(element, observed),
        _ => "state check".to_owned(),
    };
    clip(&label, CLAIM_MAX_CHARS)
}

fn window_label(window: &cua_driver_contract::WindowPredicate) -> String {
    match (&window.bounds, window.exists) {
        (Some(b), _) => format!(
            "window at {:.0},{:.0} {:.0}\u{d7}{:.0}",
            b.x, b.y, b.width, b.height
        ),
        (None, Some(false)) => "window closed".to_owned(),
        (None, _) => "window open".to_owned(),
    }
}

fn element_label(
    element: &cua_driver_contract::ElementPredicate,
    observed: Option<&serde_json::Value>,
) -> String {
    let selector = &element.selector;
    let observed_str = |key: &str| observed.and_then(|o| o.get(key)).and_then(|v| v.as_str());
    // Fail closed: the expected value is shown only when the matched
    // element's role can never be a secure field. A secure field is often
    // role AXTextField with subrole AXSecureTextField, and the subrole is not
    // in the observation, so a text field (or an unknown role) never shows
    // its value. A password-like label hides it even on a safe role.
    let role_known_safe = observed_str("role")
        .or(selector.role.as_deref())
        .is_some_and(never_secure_role);
    let secure = !role_known_safe
        || [
            selector.role.as_deref(),
            selector.label_contains.as_deref(),
            observed_str("role"),
            observed_str("subrole"),
            observed_str("label"),
        ]
        .into_iter()
        .flatten()
        .any(looks_secure);
    let role = selector.role.as_deref().map(role_noun);
    let subject = match (selector.label_contains.as_deref(), role) {
        (Some(label), Some(role)) => format!("{} {role}", quote(label)),
        (Some(label), None) => quote(label),
        (None, Some(role)) => role,
        (None, None) => "element".to_owned(),
    };
    let mut clauses = Vec::new();
    if let Some(value) = &element.value_equals {
        clauses.push(if secure {
            "value matches".to_owned()
        } else {
            format!("holds {}", quote(value))
        });
    }
    match element.selected {
        Some(true) => clauses.push("selected".to_owned()),
        Some(false) => clauses.push("not selected".to_owned()),
        None => {}
    }
    match element.enabled {
        Some(true) => clauses.push("enabled".to_owned()),
        Some(false) => clauses.push("disabled".to_owned()),
        None => {}
    }
    // The selected text itself is never shown.
    if let Some(selection) = &element.text_selection {
        clauses.push(if selection.length == 0 {
            format!("caret at {}", selection.location)
        } else {
            format!("{} chars selected", selection.length)
        });
    }
    if clauses.is_empty() {
        clauses.push("visible".to_owned());
    }
    format!("{subject} {}", clauses.join(", "))
}

/// Roles whose value can never be a secure field's (normalized like the
/// evaluator's role match: letters and digits, lowercase, no `AX`). Text
/// fields and combo boxes are not here: either can be secure.
fn never_secure_role(role: &str) -> bool {
    let normalized: String = role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(|c| c.to_lowercase())
        .collect();
    matches!(
        normalized.strip_prefix("ax").unwrap_or(&normalized),
        "textarea"
            | "statictext"
            | "checkbox"
            | "radiobutton"
            | "popupbutton"
            | "slider"
            | "button"
            | "pushbutton"
    )
}

/// Roles and labels that mark a field whose value must never be shown.
fn looks_secure(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "secure",
        "password",
        "passcode",
        "passphrase",
        "secret",
        "token",
        "api key",
        "cvv",
        "otp",
    ]
    .iter()
    .any(|word| text.contains(word))
}

/// `AXTextArea` -> "text area", `AXPopUpButton` -> "pop-up button".
fn role_noun(role: &str) -> String {
    let bare = role.strip_prefix("AX").unwrap_or(role);
    let mut words = String::new();
    for (i, c) in bare.chars().enumerate() {
        if c == '_' || c == '-' || c == ' ' {
            words.push(' ');
        } else if c.is_uppercase() && i > 0 && !words.ends_with(' ') {
            words.push(' ');
            words.extend(c.to_lowercase());
        } else {
            words.extend(c.to_lowercase());
        }
    }
    match words.trim() {
        "push button" | "pushbutton" => "button".to_owned(),
        "check box" => "checkbox".to_owned(),
        "pop up button" => "pop-up button".to_owned(),
        "static text" => "text".to_owned(),
        other => other.to_owned(),
    }
}

/// `"value"`, control characters flattened, clipped to `QUOTE_MAX_CHARS`.
fn quote(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("\"{}\"", clip(flat.trim(), QUOTE_MAX_CHARS))
}

/// `text` cut to at most `max` characters, ending in an ellipsis when cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(label: Option<&str>, client: Option<&str>, pid: Option<i32>) -> PipHookFrame {
        PipHookFrame {
            action_label: "click".into(),
            timestamp_ms: 0,
            session_key: "k".into(),
            session_label: label.map(str::to_owned),
            client_name: client.map(str::to_owned),
            client_pid: None,
            target_pid: pid,
            target_window_id: None,
        }
    }

    #[test]
    fn only_labeled_or_client_known_sessions_get_a_panel() {
        assert!(pip_frame_wanted(
            "click",
            &frame(Some("alpha"), None, Some(1))
        ));
        assert!(pip_frame_wanted(
            "click",
            &frame(None, Some("Claude Code"), Some(1))
        ));
        assert!(!pip_frame_wanted("click", &frame(None, None, Some(1))));
    }

    #[test]
    fn lifecycle_meta_and_untargeted_calls_get_no_frame() {
        for tool in [
            "start_session",
            "end_session",
            "get_session_state",
            "list_sessions",
            "start_recording",
            "stop_recording",
            "set_config",
            "set_agent_cursor_enabled",
            "kill_app",
        ] {
            assert!(
                !pip_frame_wanted(tool, &frame(Some("alpha"), None, Some(1))),
                "{tool}"
            );
        }
        assert!(!pip_frame_wanted(
            "click",
            &frame(Some("alpha"), None, None)
        ));
        assert!(pip_frame_wanted(
            "type_text",
            &frame(Some("alpha"), None, Some(1))
        ));
    }

    #[test]
    fn row_b4_browser_tabs_never_gets_a_frame() {
        // With Chrome's pid and its own window number, or the pid alone.
        let mut numbered = frame(Some("alpha"), None, Some(1));
        numbered.target_window_id = Some(446_425_629);
        assert!(!pip_frame_wanted("browser_tabs", &numbered));
        assert!(!pip_frame_wanted(
            "browser_tabs",
            &frame(Some("alpha"), None, Some(1))
        ));
    }

    #[test]
    fn row_b1_b3_a_bound_tab_is_shown_in_its_window_only_while_it_is_selected() {
        // B1: the binding's macOS pid and window.
        assert_eq!(bound_tab_window(42, 7, true), Some((42, 7)));
        // B3: a background tab, or one whose state could not be read.
        assert_eq!(bound_tab_window(42, 7, false), None);
        // Ids that are no macOS pid or window.
        assert_eq!(bound_tab_window(i64::MAX, 7, true), None);
        assert_eq!(bound_tab_window(42, u64::MAX, true), None);
    }

    #[tokio::test]
    async fn a_bound_window_reaches_only_the_dispatch_that_noted_it() {
        // Noted inside a dispatch: returned with its output.
        let (output, window) = with_bound_window(async {
            note_bound_window(Some((42, 7)));
            "done"
        })
        .await;
        assert_eq!((output, window), ("done", Some((42, 7))));
        // A nested dispatch (a browser_steps step) keeps its own.
        let (inner, outer) = with_bound_window(async {
            with_bound_window(async { note_bound_window(Some((42, 7))) })
                .await
                .1
        })
        .await;
        assert_eq!((inner, outer), (Some((42, 7)), None));
        // Outside a dispatch: nothing to note into, and no panic.
        note_bound_window(Some((42, 7)));
        assert!(!wants_bound_window());
    }

    fn predicate(json: serde_json::Value) -> cua_driver_contract::StatePredicate {
        serde_json::from_value(json).unwrap()
    }

    fn label(json: serde_json::Value) -> String {
        claim_label(&predicate(json), None)
    }

    #[test]
    fn claim_labels_read_as_short_plain_sentences() {
        assert_eq!(
            label(
                serde_json::json!({"element": {"selector": {"role": "AXTextArea"}, "value_equals": "hello"}})
            ),
            "text area holds \"hello\""
        );
        assert_eq!(
            label(
                serde_json::json!({"element": {"selector": {"role": "AXButton", "label_contains": "Save"}, "exists": true}})
            ),
            "\"Save\" button visible"
        );
        assert_eq!(
            label(
                serde_json::json!({"element": {"selector": {"label_contains": "Bold"}, "selected": true}})
            ),
            "\"Bold\" selected"
        );
        assert_eq!(
            label(
                serde_json::json!({"element": {"selector": {"role": "AXCheckBox"}, "enabled": false}})
            ),
            "checkbox disabled"
        );
        assert_eq!(
            label(
                serde_json::json!({"element": {"selector": {"role": "AXTextField"},
                "text_selection": {"location": 3, "length": 0}}})
            ),
            "text field caret at 3"
        );
        assert_eq!(
            label(serde_json::json!({"window": {"exists": true}})),
            "window open"
        );
        assert_eq!(
            label(serde_json::json!({"window": {"exists": false}})),
            "window closed"
        );
        assert_eq!(
            label(
                serde_json::json!({"window": {"bounds": {"x": 10, "y": 20, "width": 800, "height": 600}}})
            ),
            "window at 10,20 800\u{d7}600"
        );
    }

    #[test]
    fn claim_labels_are_truncated_and_never_show_selected_text() {
        let long = label(serde_json::json!({"element": {
            "selector": {"role": "AXTextArea", "label_contains": "A very long field label"},
            "value_equals": "a long value that goes on and on\nwith a newline"}}));
        assert!(long.chars().count() <= CLAIM_MAX_CHARS, "{long}");
        assert!(long.ends_with('\u{2026}'), "{long}");
        assert!(!long.contains('\n'));
        let selection = label(
            serde_json::json!({"element": {"selector": {"role": "AXTextArea"},
            "text_selection": {"location": 0, "length": 6, "text": "secret"}}}),
        );
        assert_eq!(selection, "text area 6 chars selected");
    }

    #[test]
    fn secure_field_values_never_reach_a_label() {
        for (json, observed) in [
            // By the selector's role or label.
            (
                serde_json::json!({"element": {"selector": {"role": "AXSecureTextField"}, "value_equals": "hunter2"}}),
                None,
            ),
            (
                serde_json::json!({"element": {"selector": {"label_contains": "Password"}, "value_equals": "hunter2"}}),
                None,
            ),
            // By what the check matched: a plain selector on a secure field.
            (
                serde_json::json!({"element": {"selector": {"role": "AXTextField"}, "value_equals": "hunter2"}}),
                Some(serde_json::json!({"role": "AXTextField", "subrole": "AXSecureTextField"})),
            ),
            // Fail closed: a text field (maybe secure, subrole unseen), a
            // combo box, and no role at all never show the value.
            (
                serde_json::json!({"element": {"selector": {"role": "AXTextField"}, "value_equals": "hunter2"}}),
                None,
            ),
            (
                serde_json::json!({"element": {"selector": {"role": "AXComboBox"}, "value_equals": "hunter2"}}),
                None,
            ),
            (
                serde_json::json!({"element": {"selector": {"label_contains": "Name"}, "value_equals": "hunter2"}}),
                None,
            ),
            // A safe-looking selector whose match is a text field.
            (
                serde_json::json!({"element": {"selector": {"label_contains": "Name"}, "value_equals": "hunter2"}}),
                Some(serde_json::json!({"role": "AXTextField", "label": "Name"})),
            ),
            (
                serde_json::json!({"element": {"selector": {"role": "AXTextField"}, "value_equals": "hunter2"}}),
                Some(serde_json::json!({"role": "AXTextField", "label": "API token"})),
            ),
        ] {
            let text = claim_label(&predicate(json), observed.as_ref());
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.ends_with("value matches"), "{text}");
        }
    }

    #[test]
    fn a_verification_event_carries_each_claim_with_its_status() {
        let mut agent = frame(Some("alpha"), None, Some(42));
        agent.target_window_id = Some(7);
        let input = serde_json::json!({"pid": 42, "window_id": 7, "expect": [
            {"element": {"selector": {"role": "AXTextArea"}, "value_equals": "hi"}},
            {"window": {"exists": true}},
            {"element": {"selector": {"role": "AXButton", "label_contains": "Send"}, "enabled": true}},
        ]});
        let output = serde_json::json!({"status": "unsatisfied", "stable": false, "elapsed_ms": 5,
            "samples": 1, "predicates": [
            {"index": 0, "status": "satisfied", "unknown_reason": null,
             "observed_json": "{\"role\":\"AXTextArea\",\"value\":\"hi\"}"},
            {"index": 1, "status": "unsatisfied", "unknown_reason": null, "observed_json": null},
            {"index": 2, "status": "unknown", "unknown_reason": "multi_match", "observed_json": null},
        ]});
        agent.timestamp_ms = 10_000;
        let event = verification_event(agent, 1_000, &input, Some(&output)).unwrap();
        // Stamped when the predicates were observed (start + elapsed_ms),
        // not when the call returned (after an include_screenshot capture).
        assert_eq!(event.timestamp_ms, 1_005);
        assert_eq!((event.target_pid, event.target_window_id), (42, 7));
        assert!(!event.satisfied);
        let claims: Vec<(&str, Option<bool>)> = event
            .claims
            .iter()
            .map(|claim| (claim.label.as_str(), claim.satisfied))
            .collect();
        assert_eq!(
            claims,
            [
                ("text area holds \"hi\"", Some(true)),
                ("window open", Some(false)),
                ("\"Send\" button enabled", None),
            ]
        );
        // A one-shot call (no label, no client) gets no event.
        let mut anonymous = frame(None, None, Some(42));
        anonymous.target_window_id = Some(7);
        assert!(verification_event(anonymous, 0, &input, Some(&output)).is_none());
    }

    #[test]
    fn a_secure_field_matched_through_the_real_evaluator_never_shows_its_value() {
        // What get_window_state reports for an NSSecureTextField: role
        // AXTextField, no subrole. Run the predicate through the production
        // evaluator and output projection, then build the PiP event.
        use crate::expectation::{evaluate_predicates, ObservationSnapshot};
        let snapshot = |role: &str, value: &str| ObservationSnapshot {
            window: Some(serde_json::json!({"window_id": 7, "pid": 42})),
            elements: Some(vec![serde_json::json!({
                "element_index": 0, "role": role, "label": "", "value": value
            })]),
            element_source_trusted: true,
            elements_complete: true,
        };
        let event_for = |role: &str, value: &str| {
            let input = serde_json::json!({"pid": 42, "window_id": 7, "expect": [
                {"element": {"selector": {"role": role}, "value_equals": value}}]});
            let expect: Vec<cua_driver_contract::StatePredicate> =
                serde_json::from_value(input["expect"].clone()).unwrap();
            let outcomes = evaluate_predicates(&expect, &snapshot(role, value));
            let output = serde_json::to_value(cua_driver_contract::VerifyStateOutput {
                status: outcomes[0].status,
                stable: true,
                elapsed_ms: 1,
                samples: 2,
                predicates: outcomes,
            })
            .unwrap();
            let mut agent = frame(Some("alpha"), None, Some(42));
            agent.target_window_id = Some(7);
            verification_event(agent, 0, &input, Some(&output)).unwrap()
        };
        let secure = event_for("AXTextField", "hunter2");
        assert_eq!(secure.claims[0].satisfied, Some(true));
        assert_eq!(secure.claims[0].label, "text field value matches");
        // A text area can never be secure: its value shows.
        let area = event_for("AXTextArea", "hello");
        assert_eq!(area.claims[0].label, "text area holds \"hello\"");
    }

    #[test]
    fn claims_are_identified_by_predicate_and_window_not_by_their_label() {
        let first = predicate(
            serde_json::json!({"element": {"selector": {"role": "AXTextArea"},
            "value_equals": "abcdefghijklmnop-first"}}),
        );
        let second = predicate(
            serde_json::json!({"element": {"selector": {"role": "AXTextArea"},
            "value_equals": "abcdefghijklmnop-second"}}),
        );
        // The truncated labels collide...
        assert_eq!(claim_label(&first, None), claim_label(&second, None));
        // ...the identities do not.
        let window = (Some(42), Some(7));
        assert_ne!(predicate_id(window, &first), predicate_id(window, &second));
        // Stable for the same predicate on the same window, scoped by window.
        assert_eq!(predicate_id(window, &first), predicate_id(window, &first));
        assert_ne!(
            predicate_id(window, &first),
            predicate_id((Some(42), Some(8)), &first)
        );
    }
}
