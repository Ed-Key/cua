use async_trait::async_trait;
use cua_driver_contract::HotkeyInput;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use libc;
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;

use super::ToolState;

pub struct HotkeyTool {
    state: Arc<ToolState>,
}

impl HotkeyTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "hotkey".into(),
        description:
            "Press a key chord, modifiers first and one key last, e.g. [\"cmd\",\"c\"]. \
             Background by default without raising the window; for a menu command such as Cmd+S, \
             use invoke_menu; the browser omnibox may need delivery_mode \"foreground\". Never read back \
             (unverifiable), and a chord does not focus a text field. \
             Details: skill://cua-driver/MACOS.md"
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["keys"],
            "properties": {
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid": { "type": "integer", "description": "Target process ID." },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 2,
                    "description": "Modifiers (cmd, shift, option/alt, ctrl, fn) then one key."
                },
                "x": { "type": "number", "description": "X in screenshot pixels to click for focus before the chord, e.g. to paste there." },
                "y": { "type": "number", "description": "Y in the same screenshot pixels." },
                "window_id": {
                    "type": "integer",
                    "description": "Target window; required for foreground. Never raises it by itself."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Legacy frame; prefer target. \"desktop\" with no pid/window_id sends to the frontmost app." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

/// Modifier key names — split the keys array into modifiers + base key.
fn is_modifier(k: &str) -> bool {
    matches!(
        k.to_lowercase().as_str(),
        "cmd"
            | "command"
            | "super"
            | "meta"
            | "win"
            | "shift"
            | "option"
            | "alt"
            | "ctrl"
            | "control"
            | "fn"
    )
}

const HOTKEY_FOCUS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const HOTKEY_FOCUS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// A Mac Catalyst field ignores focus writes: without focus already, keys
/// would go to whatever has it, and waiting for a write to take never helps.
const CATALYST_HOTKEY_NOT_FOCUSED: &str = "cua could not confirm that this Mac Catalyst field has \
    keyboard focus, and Catalyst ignores focus requests, so nothing was sent. Next: click the field \
    (its element_token; a background click is enough), or, if the field already shows its cursor, \
    send the hotkey with window_id and no element_token: keys go to the focused field.";

fn focus_hotkey_element(pid: i32, element_ptr: usize) -> anyhow::Result<()> {
    if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
        return Ok(());
    }
    // SAFETY: the caller's element guard keeps `element_ptr` retained.
    let catalyst = unsafe {
        super::type_text::catalyst_text_control_of(element_ptr as crate::ax::bindings::AXUIElementRef)
    };
    if catalyst == super::type_text::CatalystText::Yes {
        anyhow::bail!("{CATALYST_HOTKEY_NOT_FOCUSED}");
    }
    let deadline = std::time::Instant::now() + HOTKEY_FOCUS_TIMEOUT;
    loop {
        crate::input::ax_actions::focus_element(element_ptr)?;
        if crate::input::ax_actions::is_element_focused(pid, element_ptr) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("requested hotkey element did not become focused");
        }
        std::thread::sleep(HOTKEY_FOCUS_POLL_INTERVAL);
    }
}

fn screen_sharing_modifier_delivery_error(
    is_screen_sharing: bool,
    has_modifiers: bool,
    foreground: bool,
    window_id: Option<u32>,
) -> Option<ToolResult> {
    if !is_screen_sharing || !has_modifiers || (foreground && window_id.is_some()) {
        return None;
    }
    Some(
        ToolResult::error(
            "Screen Sharing modifier hotkeys require delivery_mode:\"foreground\" and window_id \
             so Cua Driver can deliver physical modifier transitions safely.",
        )
        .with_structured(serde_json::json!({
            "code": "SCREEN_SHARING_REQUIRES_FOREGROUND_HID",
            "effect": "refused",
            "escalation": {
                "recommended": "foreground",
                "reason": "Screen Sharing does not forward modifier state from background \
                           PID-routed base-key events.",
                "requires": ["window_id"]
            }
        })),
    )
}

#[async_trait]
impl Tool for HotkeyTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<HotkeyInput>("hotkey", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let raw_keys = input.keys;
            if raw_keys.len() < 2 {
                return ToolResult::error("hotkey.keys must contain at least two keys.")
                    .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
            }
            let modifiers: Vec<String> = raw_keys
                .iter()
                .filter(|key| is_modifier(key))
                .cloned()
                .collect();
            let Some(key) = raw_keys.iter().rev().find(|key| !is_modifier(key)).cloned() else {
                return ToolResult::error(
                    "keys must include at least one non-modifier key for desktop hotkey",
                );
            };
            let display = raw_keys.join("+");
            let result = tokio::task::spawn_blocking(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                crate::input::keyboard::press_key_global(&key, &modifier_refs)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed desktop hotkey {display}."))
                    .with_structured(serde_json::json!({
                        "scope": "desktop",
                        "path": "hid",
                        "effect": "unverifiable"
                    })),
                Ok(Err(error)) => ToolResult::error(format!("desktop hotkey failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop hotkey task failed: {error}")),
            };
        }

        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };

        if args.get("keys").and_then(|v| v.as_array()).is_none() {
            return ToolResult::error("Missing required parameter: keys");
        }
        let raw_keys = args.str_array("keys");

        if raw_keys.is_empty() {
            return ToolResult::error("keys must be a non-empty array of strings.");
        }

        // Split: modifiers are all entries that are modifier names; base key is everything else.
        // Typically: last non-modifier is the key; all others are modifiers.
        let modifiers: Vec<String> = raw_keys
            .iter()
            .filter(|k| is_modifier(k))
            .cloned()
            .collect();
        let non_modifiers: Vec<String> = raw_keys
            .iter()
            .filter(|k| !is_modifier(k))
            .cloned()
            .collect();
        let cmd_chord = modifiers.iter().any(|k| {
            matches!(
                k.to_ascii_lowercase().as_str(),
                "cmd" | "command" | "super" | "meta" | "win"
            )
        });

        if non_modifiers.is_empty() {
            return ToolResult::error(
                "keys must include at least one non-modifier key (e.g. \"c\" in [\"cmd\", \"c\"]).",
            );
        }

        // Use the last non-modifier key; if there are multiple, treat earlier ones as extra keys.
        let key = non_modifiers.last().unwrap().clone();
        let key_display = raw_keys.join("+");
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid, &args) {
            Ok(resolved) => resolved,
            Err(error) => return error,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        // delivery_mode gates whether we raise: background (default) never fronts
        // the window — passing window_id only targets the combo. foreground is the
        // explicit NSMenu-activation rung for menu shortcuts that ignore a
        // background combo (matches click/type_text).
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let fg = delivery_mode.is_foreground();
        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to hotkey, not both.",
            );
        }

        let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());

        let screen_sharing_target = crate::input::keyboard::is_screen_sharing_pid(pid);
        if let Some(error) = screen_sharing_modifier_delivery_error(
            screen_sharing_target,
            !modifiers.is_empty(),
            fg,
            window_id,
        ) {
            return error;
        }

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background combo is process-scoped transport: it
        // must prove exact delivery to the requested window (fresh AXWindows
        // membership, not minimized/hidden, no competing same-pid keyboard
        // destination) BEFORE anything is sent — including the px focus
        // click. delivery_mode:"foreground" stays the caller's explicit last
        // resort and is not gated here.
        let _mutation_lease = if !fg {
            if let Some(wid) = window_id {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    element_ptr,
                    cua_driver_core::background_input::BackgroundAction::GenericKey,
                )
                .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            }
        } else {
            None
        };

        // Web-content AX nodes can acknowledge AXFocused without moving the
        // renderer's real first responder. That makes a direct focus write an
        // unsafe oracle for Chromium/WebKit hotkeys: the following PID/HID
        // chord can still land on the renderer's remembered control. Resolve
        // the requested AX node's exact center and reuse the proven PX focus
        // ladder for web areas. The request remains snapshot-bound AX
        // targeting; only the focus transport falls back through a hit-test
        // and, on the explicit foreground rung, a real click when required.
        let web_ax_focus_xy = if let (Some(guard), Some(wid), Some(index)) =
            (element_guard.clone(), window_id, element_index)
        {
            let web_guard = guard.clone();
            let is_web = tokio::task::spawn_blocking(move || {
                super::type_text::target_in_web_area(
                    pid,
                    Some((web_guard.as_ptr(), Some(index))),
                    Some(wid),
                )
            })
            .await
            .unwrap_or(true);
            if is_web {
                tokio::task::spawn_blocking(move || unsafe {
                    let (screen_x, screen_y) = crate::ax::bindings::element_screen_center(
                        guard.as_ptr() as crate::ax::bindings::AXUIElementRef,
                    )?;
                    let frame = super::px_frame::resolve_window_px_frame(wid).ok()?;
                    Some((
                        (screen_x - frame.bounds.x) * frame.scale,
                        (screen_y - frame.bounds.y) * frame.scale,
                    ))
                })
                .await
                .unwrap_or(None)
            } else {
                None
            }
        } else {
            None
        };

        // PX form, plus the web-content AX fallback above: focus the field
        // before sending the combo. Foreground delivery still needs to front
        // the target for the chord itself because the focus helper restores
        // the previous app before returning.
        let coordinate_focus = {
            // The web-AX fallback's center is driver-derived window pixels,
            // not a caller's screenshot pixels.
            let focus_xy = match ((px, py), web_ax_focus_xy) {
                ((Some(cx), Some(cy)), _) => Some((cx, cy, false)),
                ((None, None), Some((cx, cy))) => Some((cx, cy, true)),
                _ => None,
            };
            if let Some((cx, cy, window_native)) = focus_xy {
                let from_zoom = args
                    .get("from_zoom")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Err(e) = super::focus_by_pixel(
                    &self.state,
                    pid,
                    window_id,
                    cx,
                    cy,
                    fg,
                    args.opt_str("session"),
                    args.opt_str("_session_id"),
                    from_zoom,
                    window_native,
                    _mutation_lease.as_ref(),
                )
                .await
                {
                    return e;
                }
                true
            } else {
                false
            }
        };

        // A focus click already moved the cursor. Otherwise place a named
        // session's cursor on the element, its remembered position, or the
        // window centre, so a keyboard-first session stays visible.
        if !coordinate_focus {
            let element_center = match element_guard.clone() {
                Some(guard) => tokio::task::spawn_blocking(move || unsafe {
                    crate::ax::bindings::element_screen_center(
                        guard.as_ptr() as crate::ax::bindings::AXUIElementRef
                    )
                })
                .await
                .ok()
                .flatten(),
                None => None,
            };
            super::cursor_tools::position_keyboard_cursor(
                &self.state,
                &args,
                window_id,
                element_center,
            )
            .await;
        }

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Hotkeys like Cmd+N, Cmd+W, Cmd+T explicitly open/close
        // windows. The NSMenu path also briefly activates the target via
        // SLPSSetFrontProcessWithOptions which can race the wildcard
        // suppressor — wrapping ensures both side-effects are observed
        // and the prior frontmost is restored if the activation lingers.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "hotkey.CGEvent",
            || async move {
                tokio::task::spawn_blocking(move || {
                    let element_ptr = element_guard.as_ref().map(|guard| guard.as_ptr());
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    match (fg, coordinate_focus, window_id, element_ptr) {
                        // Chrome's native omnibox and Chromium/Electron inputs
                        // require a genuine foreground HID chord. Keep the exact
                        // target frontmost until both key events are consumed;
                        // otherwise Cmd+A/Cmd+V can be silently ignored.
                        (true, true, Some(wid), _) => {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    if screen_sharing_target {
                                        crate::input::keyboard::press_key_bare_global(&key, &m)
                                    } else {
                                        crate::input::keyboard::press_key_global(&key, &m)
                                    }
                                },
                            )?;
                            Ok(())
                        }
                        // An AX-addressed chord has the same renderer-focus
                        // requirement as the px form. Activate the exact window,
                        // establish and confirm the requested child focus after
                        // activation, then use the guarded global HID queue.
                        (true, false, Some(wid), Some(ptr)) => {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || {
                                    focus_hotkey_element(pid, ptr)?;
                                    crate::input::keyboard::press_key_bare_global(&key, &m)
                                },
                            )?;
                            Ok(())
                        }
                        // foreground rung: front the exact window, confirm it is
                        // key, then send a physical chord. Without that proof a
                        // chord posted while another app still held focus was
                        // dropped yet reported as pressed. The physical
                        // transitions also reach Screen Sharing guests.
                        (true, false, Some(wid), None) => {
                            crate::input::skylight::with_foreground_hid_activation(
                                pid as libc::pid_t,
                                wid,
                                || crate::input::keyboard::press_key_bare_global(&key, &m),
                            )?;
                            Ok(())
                        }
                        // background (default): auth-envelope post to the pid, no
                        // raise — even when window_id was supplied for targeting.
                        (false, false, _, Some(ptr)) => {
                            focus_hotkey_element(pid, ptr)?;
                            super::with_background_web_key_window(pid, window_id, Some(ptr), || {
                                crate::input::keyboard::hotkey(pid, &key, &m)
                            })
                        }
                        (false, _, _, element_ptr) => {
                            super::with_background_web_key_window(pid, window_id, element_ptr, || {
                                crate::input::keyboard::hotkey(pid, &key, &m)
                            })
                        }
                        _ => crate::input::keyboard::hotkey(pid, &key, &m),
                    }
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot).await;

        match result {
            Ok(Ok(())) => {
                let label = if fg {
                    " (delivery_mode:foreground)"
                } else {
                    ""
                };
                // A combo is never read-back-verifiable, and a delivered combo
                // is not evidence of a missed one: no escalation. Resending a
                // navigation or toggle chord that did land repeats it, so the
                // next step is a read.
                let structured = delivered_structured(fg);
                let menu_hint = menu_hint(fg, cmd_chord);
                ToolResult::text(format!(
                    "Pressed {key_display} on pid {pid}{label}.{menu_hint}{}",
                    changes.result_suffix()
                ))
                .with_structured(structured)
            }
            Ok(Err(e)) => ToolResult::error(format!("hotkey failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// What a delivered chord's result suggests next. Apps often drop a
/// background menu shortcut (Calculator ignored Cmd+C and TextEdit Cmd+S
/// sent this way to a native window, which is not made key first);
/// invoke_menu runs the same command by path, from behind when it can, so
/// it is named with the route it takes and when it would bring the app
/// forward.
fn menu_hint(foreground: bool, cmd_chord: bool) -> &'static str {
    if !foreground && cmd_chord {
        " Read the window before sending it again: only if a fresh read shows its effect missing, \
         run the same command with invoke_menu, e.g. path [\"File\",\"Save\"]. invoke_menu tries it \
         from behind first, with the window selected as the app's key window, and brings the app \
         to the front only when it cannot run from behind or shows no effect there; its result \
         says which route ran."
    } else {
        " Read the window before sending it again."
    }
}

/// Structured result of a delivered combo: unverifiable, and never an
/// escalation, since delivery is no evidence the combo was missed.
fn delivered_structured(foreground: bool) -> Value {
    serde_json::json!({
        "path": if foreground { "key_events_fg" } else { "key_events" },
        "verified": false,
        "effect": "unverifiable",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A background combo that was delivered carried a "resend it in the
    /// foreground" hint, and an agent resent navigation chords that had
    /// landed. Delivery alone must not suggest a resend.
    #[test]
    fn a_delivered_combo_suggests_no_resend() {
        for foreground in [false, true] {
            let structured = delivered_structured(foreground);
            assert!(structured.get("escalation").is_none());
            assert_eq!(structured["effect"], "unverifiable");
        }
    }

    /// A background Cmd chord that did nothing once sent an agent to
    /// invoke_menu, which then brought Calculator to the front (errand T6).
    /// The hint names invoke_menu only with the route it takes.
    #[test]
    fn the_menu_hint_says_invoke_menu_tries_from_behind_first() {
        let hint = menu_hint(false, true);
        assert!(hint.contains("invoke_menu"), "{hint}");
        assert!(
            hint.contains("from behind first") && hint.contains("to the front only when it cannot"),
            "{hint}"
        );
        for (foreground, cmd_chord) in [(true, true), (false, false), (true, false)] {
            assert!(!menu_hint(foreground, cmd_chord).contains("invoke_menu"));
        }
    }

    #[test]
    fn hotkey_contract_accepts_snapshot_bound_ax_targets() {
        let properties = def().input_schema["properties"]
            .as_object()
            .expect("hotkey properties");
        assert!(properties.contains_key("element_token"));
    }

    #[test]
    fn screen_sharing_modifier_hotkeys_fail_closed_without_foreground_window() {
        for (foreground, window_id) in [(false, None), (false, Some(7)), (true, None)] {
            let result = screen_sharing_modifier_delivery_error(true, true, foreground, window_id)
                .expect("unsafe Screen Sharing modifier route must be refused");
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["code"], "SCREEN_SHARING_REQUIRES_FOREGROUND_HID");
            assert_eq!(structured["effect"], "refused");
            assert_eq!(structured["escalation"]["recommended"], "foreground");
            assert_eq!(structured["escalation"]["requires"][0], "window_id");
        }
        assert!(screen_sharing_modifier_delivery_error(true, true, true, Some(7)).is_none());
        assert!(screen_sharing_modifier_delivery_error(true, false, false, None).is_none());
        assert!(screen_sharing_modifier_delivery_error(false, true, false, None).is_none());
    }
}
