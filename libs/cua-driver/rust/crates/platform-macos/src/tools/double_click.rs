use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_action_names, element_screen_center, kAXErrorSuccess, perform_action, AXUIElementRef,
};

use super::ToolState;

pub struct DoubleClickTool {
    state: Arc<ToolState>,
}

fn background_action_for_element(
    has_ax_open: bool,
) -> cua_driver_core::background_input::BackgroundAction {
    if has_ax_open {
        cua_driver_core::background_input::BackgroundAction::AxSemantic
    } else {
        cua_driver_core::background_input::BackgroundAction::WindowPointer
    }
}

/// Whether a background double_click opens this element from behind with
/// Finder's File > Open instead of AXOpen: an item of Finder's file view (its
/// icon, or its name in list and column view) while another app is in front.
/// AXOpen raises Finder's window above the user's app in the bench image
/// (item 43). Finder's sidebar cells advertise AXOpen too; they and any other
/// role keep AXOpen.
// ponytail: role allowlist from the bench image's icon, list and column views
// (gallery and desktop items are AXImage too); an unknown role keeps AXOpen.
fn opens_from_behind(
    foreground: bool,
    has_ax_open: bool,
    finder: bool,
    role: &str,
    other_app_in_front: bool,
) -> bool {
    !foreground
        && has_ax_open
        && finder
        && other_app_in_front
        && matches!(role, "AXImage" | "AXTextField")
}

/// The name a Finder item shows (an icon's title, a name field's value), so
/// the item opened is proven to be the one asked for; `None` when it has none.
fn item_name(element: AXUIElementRef) -> Option<String> {
    ["AXTitle", "AXValue", "AXDescription"]
        .into_iter()
        .filter_map(|attribute| unsafe { crate::ax::bindings::copy_string_attr(element, attribute) })
        .find(|name| !name.trim().is_empty())
}

const NOT_ALONE: &str = "the item could not be read back as Finder's only selection after the click, so File > Open could open something else";

/// The refusal for an element that names another item than it did.
fn renamed(idx: usize, asked: &str, now: Option<String>) -> NotFromBehind {
    NotFromBehind::Refuse(format!(
        "element [{idx}] named {asked:?} before the click and {} after it, so it was not opened; take a fresh snapshot",
        now.map_or_else(|| "nothing readable".to_owned(), |now| format!("{now:?}"))
    ))
}

/// Whether the nested click refused the element itself: it now names other
/// content than its snapshot row, or its token is stale (a newer snapshot).
fn token_refused(structured: Option<&Value>) -> bool {
    structured.is_some_and(|structured| {
        structured.get("code").and_then(Value::as_str) == Some("element_changed")
            || structured.pointer("/refusal/code").and_then(Value::as_str)
                == Some("stale_element_token")
    })
}

/// The identity fields of an element as its snapshot row renders them
/// (ax::tree::format_node_line): ` "title"`, ` = value`, ` [placeholder=..]`,
/// ` (description)`, from live reads, with the walk's trimming rules.
struct Identity {
    title: Option<String>,
    value: Option<String>,
    placeholder: Option<String>,
    description: Option<String>,
}

impl Identity {
    fn read(element: AXUIElementRef, role: &str) -> Self {
        let read = |attribute| unsafe { crate::ax::bindings::copy_string_attr(element, attribute) };
        let trimmed = |text: Option<String>| text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
        let value = unsafe { crate::ax::bindings::copy_stringish_attr(element, "AXValue") }
            .and_then(|copied| copied.string_value)
            .filter(|v| !v.trim().is_empty() || crate::ax::tree::is_text_entry_role(role));
        Identity {
            title: trimmed(read("AXTitle")),
            value,
            placeholder: read("AXPlaceholderValue").filter(|hint| !hint.trim().is_empty()),
            description: trimmed(read("AXDescription")),
        }
    }

    fn rendered(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            out.push_str(&format!(" \"{t}\""));
        }
        if let Some(v) = &self.value {
            out.push_str(&format!(" = {}", serde_json::json!(v)));
        }
        if let Some(p) = &self.placeholder {
            out.push_str(&format!(" [placeholder={}]", serde_json::json!(p)));
        }
        if let Some(d) = &self.description {
            out.push_str(&format!(" ({d})"));
        }
        out
    }
}

/// Whether a snapshot row (`- [idx] ROLE` and the identity fields, then the
/// bracketed id, help, actions and state block) shows exactly these identity
/// fields: the row is this prefix, then that block or nothing. No row known:
/// nothing to compare against.
fn row_shows(row: Option<&str>, idx: usize, role: &str, identity: &Identity) -> bool {
    row.is_none_or(|row| {
        let line = row.split('\u{1}').next().unwrap_or_default();
        let prefix = format!("- [{idx}] {role}{}", identity.rendered());
        line.strip_prefix(&prefix).is_some_and(|rest| {
            rest.is_empty()
                || [" [id=", " [help=", " [actions=", " [selected", " [focused", " [value="]
                    .iter()
                    .any(|block| rest.starts_with(block))
        })
    })
}

/// Why the open from behind did not run.
enum NotFromBehind {
    /// Nothing was opened: AXOpen runs, and the result says why.
    AxOpen(String),
    /// The element now names another item: neither route runs.
    Refuse(String),
}

/// Finder's Open command, run from behind by its key equivalent Cmd+O.
// ponytail: English menu titles; another language does not find the item and
// keeps AXOpen (the result says why).
const FINDER_OPEN: [&str; 2] = ["File", "Open"];

impl DoubleClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }

    /// Open a Finder item from behind: select it through the click tool's own
    /// element path, prove it is the only selection, then run File > Open
    /// from behind (invoke_menu's route; never in front). One per-process
    /// lease covers both, so no other input to Finder lands between them.
    /// `Ok`: the open was sent (it is never sent again another way). `Err`:
    /// why nothing was opened: AXOpen, or a refusal when the element now
    /// names another item.
    async fn open_finder_item_from_behind(
        &self,
        args: &Value,
        pid: i32,
        wid: u32,
        idx: usize,
        element_guard: crate::ax::snapshot::RetainedElement,
        role: &str,
        snapshot_row: Option<String>,
        lease: &super::BackgroundMutationLease,
    ) -> Result<String, NotFromBehind> {
        use cua_driver_core::tool::Tool;
        let mut click_args = args.clone();
        if let Some(map) = click_args.as_object_mut() {
            map.remove("delivery_mode");
        }
        let click = super::click::ClickTool::new(self.state.clone());
        let name_guard = element_guard.clone();
        let asked = crate::background_mutation::spawn_blocking(move || {
            item_name(name_guard.as_ptr() as AXUIElementRef)
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            NotFromBehind::Refuse(format!(
                "element [{idx}]'s name could not be read, so it could not be proven to be the item the snapshot showed. Nothing was sent. Take a fresh snapshot"
            ))
        })?;
        // The name the snapshot showed for this element: a list that refreshed
        // after the snapshot can have handed the element to another item.
        let identity_guard = element_guard.clone();
        let identity_role = role.to_owned();
        let identity = crate::background_mutation::spawn_blocking(move || {
            Identity::read(identity_guard.as_ptr() as AXUIElementRef, &identity_role)
        })
        .await
        .ok();
        if !identity.is_some_and(|identity| row_shows(snapshot_row.as_deref(), idx, role, &identity)) {
            return Err(NotFromBehind::Refuse(format!(
                "element [{idx}] now names {asked:?}, not what the snapshot showed; the app reused it for other content. Nothing was sent. Take a fresh snapshot"
            )));
        }
        // Whether the element still names the asked item and is Finder's only
        // selection: Cmd+O then opens exactly it. A list that refreshed can
        // hand the element to another item.
        let state = move |element: &crate::ax::snapshot::RetainedElement| {
            let ptr = element.as_ptr();
            let named = item_name(ptr as AXUIElementRef);
            let alone = crate::input::ax_actions::RowSelection::capture(ptr)
                .and_then(|row| row.observe())
                .is_some_and(|readback| readback.exclusive());
            (named, alone)
        };
        crate::background_mutation::with_held_lease(pid, lease._guard.clone(), async move {
            let selected = click.invoke(click_args).await;
            let said = selected
                .content
                .iter()
                .find_map(|content| match content {
                    cua_driver_core::protocol::Content::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            // Every way out reads the element again: one that now names
            // another item is neither opened by Cmd+O nor by AXOpen.
            let read_guard = element_guard.clone();
            let (now, alone) = crate::background_mutation::spawn_blocking(move || state(&read_guard))
                .await
                .unwrap_or((None, false));
            if now.as_deref() != Some(asked.as_str()) {
                return Err(renamed(idx, &asked, now));
            }
            // The click compares the element with the snapshot's row, and the
            // token with the latest snapshot: an element the app reused for
            // other content, or a token a newer snapshot replaced, is not
            // opened either.
            if token_refused(selected.structured_content.as_ref()) {
                return Err(NotFromBehind::Refuse(said));
            }
            if selected.is_error == Some(true) {
                return Err(NotFromBehind::AxOpen(format!(
                    "selecting the item from behind failed ({said})"
                )));
            }
            if !alone {
                return Err(NotFromBehind::AxOpen(NOT_ALONE.into()));
            }
            // Read again once the foreground turn is held, right before the
            // press: a foreground click (which takes no per-process lease) can
            // change Finder's selection in between.
            let path = FINDER_OPEN.iter().map(|segment| (*segment).to_owned()).collect();
            let check_guard = element_guard.clone();
            let check_asked = asked.clone();
            let opened = super::invoke_menu::press_from_behind_only(pid, wid, path, move || {
                match state(&check_guard) {
                    (Some(named), true) if named == check_asked => Ok(()),
                    (Some(named), _) if named != check_asked => {
                        Err(format!("the element names {named:?} now"))
                    }
                    _ => Err(NOT_ALONE.into()),
                }
            })
            .await;
            match opened {
                Ok(opened) => Ok(format!(
                    "Opened element [{idx}] from behind: selected it (the only selection), then ran Finder's File > Open from behind instead of AXOpen, which raises Finder's window. {opened}"
                )),
                Err(why) => {
                    let read_guard = element_guard.clone();
                    let now = crate::background_mutation::spawn_blocking(move || state(&read_guard).0)
                        .await
                        .unwrap_or(None);
                    if now.as_deref() != Some(asked.as_str()) {
                        Err(renamed(idx, &asked, now))
                    } else {
                        Err(NotFromBehind::AxOpen(why))
                    }
                }
            }
        })
        .await
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "double_click".into(),
        description:
            "Double-click an element or point. An element_token gets AXOpen when the element \
             offers it, else a double-click at its center; x,y are get_window_state screenshot \
             pixels of window_id. Background by default."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid"],
            "properties": {
                "session": cua_driver_core::tool_schema::session_schema(),
                "pid":           { "type": "integer", "description": "Target process ID." },
                "x":             { "type": "number",  "description": "X in screenshot pixels (pixel path)." },
                "y":             { "type": "number",  "description": "Y in screenshot pixels (pixel path)." },
                "window_id":     { "type": "integer", "description": "Target window ID; required with x,y, carried by element_token." },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  false,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for DoubleClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match super::target_pid(&self.state, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // delivery_mode: foreground briefly fronts the window before the pixel
        // double-click (the explicit last resort for surfaces that drop
        // background CGEvents), via the same skylight assist click uses.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, element_guard) = resolved.into_parts(window_id_arg);
        let window_id = match super::native_window_id(window_id) {
            Ok(window_id) => window_id,
            Err(error) => return error,
        };
        // The token's row as its snapshot showed it, read with the token: a
        // newer snapshot later makes the token stale, which the nested click
        // of the Finder route refuses.
        let snapshot_row = match (element_index, window_id) {
            (Some(idx), Some(wid)) => self
                .state
                .snapshots
                .with_latest_payload(pid, u64::from(wid), |payload| {
                    payload.rows.indexed.get(&idx).map(|(_, signature)| signature.clone())
                })
                .flatten(),
            _ => None,
        };

        // ── AX element path ──────────────────────────────────────────────────
        if let (Some(idx), Some(wid), Some(element_guard)) =
            (element_index, window_id, element_guard)
        {
            let element_ptr = element_guard.as_ptr();

            // Choose one background actuator before dispatch. An element that
            // advertises AXOpen uses the exact semantic route; all other
            // elements require the stricter routed-pointer proof. Do not let a
            // failed AXOpen silently cross into an ungated pointer fallback.
            let probe_guard = element_guard.clone();
            let (has_ax_open, role) = tokio::task::spawn_blocking(move || unsafe {
                let element = probe_guard.as_ptr() as AXUIElementRef;
                (
                    copy_action_names(element).iter().any(|action| action == "AXOpen"),
                    crate::ax::bindings::copy_string_attr(element, "AXRole").unwrap_or_default(),
                )
            })
            .await
            .unwrap_or((false, String::new()));
            let _mutation_lease = if !delivery_mode.is_foreground() {
                let action = background_action_for_element(has_ax_open);
                match super::gate_background_window_action(pid, wid, Some(element_ptr), action)
                    .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            };

            // A Finder item opens from behind (File > Open) where it can; AXOpen
            // runs only when that route did not send anything, and says why.
            let mut not_from_behind = None;
            if let Some(lease) = _mutation_lease.as_ref().filter(|_| {
                opens_from_behind(
                    delivery_mode.is_foreground(),
                    has_ax_open,
                    crate::apps::bundle_id_for_pid(pid).as_deref() == Some("com.apple.finder"),
                    &role,
                    crate::apps::frontmost_pid().is_some_and(|front| front != pid),
                )
            }) {
                match self
                    .open_finder_item_from_behind(
                        &args,
                        pid,
                        wid,
                        idx,
                        element_guard.clone(),
                        &role,
                        snapshot_row.clone(),
                        lease,
                    )
                    .await
                {
                    Ok(text) => return ToolResult::text(text),
                    Err(NotFromBehind::Refuse(why)) => {
                        return ToolResult::error(format!("double_click refused: {why}"))
                    }
                    Err(NotFromBehind::AxOpen(why)) => not_from_behind = Some(why),
                }
            }

            // Thread the resolved session cursor key into the blocking AX path
            // so its ClickPulse lands on THIS session's cursor, not "default".
            let ck = cursor_key.clone();
            // An AXOpen needs no activation, so guard it as click guards its
            // AXPress: AXOpen on an Open/Save panel row activates the panel's
            // app (measured, and it fails there with -25205).
            let foreground = delivery_mode.is_foreground();
            // The worker holds the lease too: a cancelled call must not free
            // Finder for other input while the AXOpen still runs.
            let worker_lease = _mutation_lease.clone();
            let result = crate::focus_guard::with_focus_suppressed(
                (!foreground).then_some(pid),
                crate::apps::frontmost_pid(),
                "double_click.AXOpen",
                || {
                    tokio::task::spawn_blocking(move || {
                        let _lease = worker_lease;
                        ax_double_click(
                            pid,
                            wid,
                            element_guard.as_ptr(),
                            idx,
                            &ck,
                            has_ax_open,
                            foreground,
                        )
                    })
                },
            )
            .await;

            return match result {
                Ok(Ok(msg)) => ToolResult::text(match not_from_behind {
                    Some(why) => format!(
                        "{msg} It was opened with AXOpen because the open from behind (select, then File > Open) could not run: {why}."
                    ),
                    None => msg,
                }),
                Ok(Err(e)) => ToolResult::error(format!("double_click failed: {e}")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // ── Pixel path ───────────────────────────────────────────────────────
        let mut cx = match args.get("x").and_then(|v| v.as_f64()) {
            Some(v) => v,
            None => return ToolResult::error("Either element_token or x + y must be provided."),
        };
        let mut cy = match args.get("y").and_then(|v| v.as_f64()) {
            Some(v) => v,
            None => return ToolResult::error("Missing required parameter: y"),
        };

        // Scale back from downscaled-image space to native pixels when needed.
        let ratio = match super::screenshot_scale(&self.state, &args, pid, window_id) {
            Ok(ratio) => ratio,
            Err(refusal) => return refusal,
        };
        cx *= ratio;
        cy *= ratio;

        // Window-local → screen coordinate translation + win-local logical coords
        // for CGEventSetWindowLocation (shared with click.rs via px_frame, which
        // refuses a window with no live frame instead of silently treating the
        // local point as screen-absolute).
        let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
            match super::px_frame::resolve_or_refuse(wid).await {
                Ok(frame) => {
                    let translated = frame.to_screen(cx, cy);
                    if !delivery_mode.is_foreground()
                        && (translated.2 < 0.0
                            || translated.3 < 0.0
                            || translated.2 > frame.bounds.width
                            || translated.3 > frame.bounds.height)
                    {
                        return ToolResult::error(format!(
                            "double_click: window-local point ({:.1}, {:.1}) pt lies outside \
                             window {wid}'s {:.0}×{:.0} pt frame; background delivery refused",
                            translated.2, translated.3, frame.bounds.width, frame.bounds.height
                        ));
                    }
                    translated
                }
                Err(refusal) => return refusal,
            }
        } else {
            (cx, cy, cx, cy)
        };

        let _mutation_lease = if !delivery_mode.is_foreground() {
            if let Some(wid) = window_id {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    None,
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
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

        // Pin overlay above the target window before animating.
        if let Some(wid) = window_id {
            crate::cursor::overlay::send_command(
                cursor_key.clone(),
                cursor_overlay::OverlayCommand::PinAbove(wid as u64),
            );
        }
        // Animate cursor to the click point; wait for arrival before firing.
        crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y, window_id.map(|wid| wid as u64)).await;
        crate::cursor::overlay::send_command(
            cursor_key.clone(),
            cursor_overlay::OverlayCommand::ClickPulse {
                x: screen_x,
                y: screen_y,
            },
        );

        let fg = delivery_mode.is_foreground() && window_id.is_some();
        let route =
            match super::pixel_route::resolve(pid, fg, window_id, "mouse_double_click").await {
                Ok(route) => route,
                Err(refusal) => return refusal,
            };
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<crate::input::mouse::ForegroundClick>> {
            let do_click = move || -> anyhow::Result<Option<crate::input::mouse::ForegroundClick>> {
                if let (super::pixel_route::PixelClickRoute::ForegroundHid, Some(wid)) = (route, window_id) {
                    // Warp the hardware pointer and post at the HID tap (or to
                    // the pid around another process's active tap); see
                    // `pixel_route` for the cross-platform foreground contract.
                    return crate::input::mouse::foreground_click(
                        pid,
                        wid,
                        (screen_x, screen_y),
                        (win_local_x, win_local_y),
                        2,
                        "left",
                        &[],
                    )
                    .map(Some);
                }
                let sent = if let Some(wid) = window_id {
                    crate::input::mouse::click_at_xy_with_window_local(
                        pid,
                        screen_x,
                        screen_y,
                        win_local_x,
                        win_local_y,
                        wid,
                        2,
                        &[],
                        crate::input::mouse::WindowClickDelivery::from_foreground(fg),
                    )
                } else {
                    crate::input::mouse::click_at_xy(pid, screen_x, screen_y, 2, &[])
                };
                sent.map(|()| None)
            };
            // Foreground rung: front the exact window → HID double-click →
            // restore the prior frontmost. No input is sent unless the exact
            // window is proven focused.
            match (route, window_id) {
                (super::pixel_route::PixelClickRoute::ForegroundHid, Some(wid)) => {
                    crate::input::skylight::with_foreground_pointer_activation(
                        pid as libc::pid_t,
                        wid,
                        (screen_x, screen_y),
                        do_click,
                    )
                }
                _ => do_click(),
            }
        })
        .await;

        match result {
            Ok(Ok(sent)) => {
                let (note, path) = super::pixel_route::sent_note(route, sent);
                ToolResult::text(format!("✅ Double-clicked at ({screen_x:.1}, {screen_y:.1}) ({note})."))
                    .with_structured(serde_json::json!({
                        "path": path, "verified": false, "effect": "unverifiable"
                    }))
            }
            Ok(Err(e)) if e.is::<crate::input::mouse::ClickNotConfirmed>() => {
                super::pixel_route::not_confirmed(
                    &format!("The double-click at ({screen_x:.1}, {screen_y:.1})"),
                    &e,
                )
                .expect("checked by the guard")
            }
            Ok(Err(e)) if route == super::pixel_route::PixelClickRoute::ForegroundHid => {
                super::pixel_route::foreground_unavailable(
                    "Double-click",
                    window_id.unwrap_or_default(),
                    &e,
                )
            }
            Ok(Err(e)) => ToolResult::error(format!("Double-click failed: {e}")),
            Err(e)     => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── Blocking AX path ─────────────────────────────────────────────────────────

fn ax_double_click(
    pid: i32,
    wid: u32,
    element_ptr: usize,
    idx: usize,
    cursor_key: &str,
    has_ax_open: bool,
    foreground: bool,
) -> anyhow::Result<String> {
    let element = element_ptr as AXUIElementRef;

    // Try AXOpen first (Finder items, openable list rows, document cells).
    if has_ax_open {
        let alive_before = unsafe { crate::ax::bindings::element_is_alive(element) };
        let err = unsafe { perform_action(element, "AXOpen") };
        if err == kAXErrorSuccess {
            return Ok(format!("AXOpen performed on element [{idx}]."));
        }
        // Finder's AXOpen navigates the window, destroying the icon it was
        // performed on, and then reports an error although it ran.
        if crate::ax::bindings::action_replaced_element(err, alive_before, || unsafe {
            crate::ax::bindings::element_gone_after_action(element)
        }) {
            return Ok(format!(
                "AXOpen performed on element [{idx}]; the element no longer exists afterwards \
                 (the action replaced it; AX returned {err}). Take a fresh snapshot before \
                 acting again: do not retry this action."
            ));
        }
        if !foreground {
            anyhow::bail!(
                "AXOpen returned {err} for element [{idx}]; background delivery will not \
                 improvise a pointer fallback after choosing the semantic route"
            );
        }
        tracing::debug!(
            "AXOpen returned {err} for element [{idx}], falling back to pixel double-click"
        );
    }

    // Resolve screen center and fall back to pixel double-click.
    let (cx, cy) = unsafe { element_screen_center(element) }
        .ok_or_else(|| anyhow::anyhow!("Cannot resolve screen center for element [{idx}]"))?;

    // Drive THIS session's cursor (threaded in via `cursor_key`), not "default".
    crate::cursor::overlay::send_command(
        cursor_key.to_owned(),
        cursor_overlay::OverlayCommand::ClickPulse { x: cx, y: cy },
    );
    // Use the window-local primitive (not bare click_at_xy): a plain
    // click_at_xy does NOT reliably reach a backgrounded / non-key window — it
    // no-ops on AppKit controls that hit-test the window-local stamp. Mirror the
    // delivering path the `click` pixel branch uses so the double-click actually
    // lands without foregrounding the app.
    // Window-routed target: we MUST have the window's bounds to translate the
    // screen center into a window-local stamp. If bounds are missing, refuse
    // rather than stamping screen coords as window-local — that no-ops or hits
    // the wrong location while still routing to `wid`.
    let (wx, wy) = crate::windows::window_bounds_by_id(wid)
        .map(|b| (cx - b.x, cy - b.y))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot resolve window bounds for window_id {wid}; refusing to stamp \
             screen coordinates as window-local for element [{idx}]."
            )
        })?;
    crate::input::mouse::click_at_xy_with_window_local(
        pid,
        cx,
        cy,
        wx,
        wy,
        wid,
        2,
        &[],
        crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
    )?;
    Ok(format!(
        "✅ Double-clicked element [{idx}] at ({cx:.1}, {cy:.1})."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_row_must_show_exactly_the_items_identity() {
        let named = |title: &str, description: Option<&str>| Identity {
            title: Some(title.into()),
            value: None,
            placeholder: None,
            description: description.map(Into::into),
        };
        let icon = "- [50] AXImage \"Draft (Q3) Copy\" (Draft (Q3) Copy) [id=Draft (Q3) Copy actions=[open,show_menu]]\u{1}None";
        assert!(row_shows(Some(icon), 50, "AXImage", &named("Draft (Q3) Copy", Some("Draft (Q3) Copy"))));
        for part in ["Q3", "Draft", "Copy", "Draft (Q3)"] {
            assert!(!row_shows(Some(icon), 50, "AXImage", &named(part, Some(part))), "{part} matched");
        }
        // Another index or role is another row.
        assert!(!row_shows(Some(icon), 51, "AXImage", &named("Draft (Q3) Copy", Some("Draft (Q3) Copy"))));
        // A title that holds a quote and a bracket does not match its first part.
        let tricky = "- [5] AXImage \"Draft\" [old\" [actions=[open]]";
        assert!(!row_shows(Some(tricky), 5, "AXImage", &named("Draft", None)));
        assert!(row_shows(Some(tricky), 5, "AXImage", &named("Draft\" [old", None)));
        let field = "- [74] AXTextField = \"Report \\\"Q3\\\".txt\" [actions=[open,show_menu,confirm] selected]";
        let valued = |value: &str| Identity { title: None, value: Some(value.into()), placeholder: None, description: None };
        assert!(row_shows(Some(field), 74, "AXTextField", &valued("Report \"Q3\".txt")));
        assert!(!row_shows(Some(field), 74, "AXTextField", &valued("Q3")));
        assert!(row_shows(None, 1, "AXImage", &valued("anything")));
    }

    #[test]
    fn a_click_that_refused_the_element_itself_is_not_followed_by_axopen() {
        assert!(token_refused(Some(&serde_json::json!({"code": "element_changed", "effect": "refused"}))));
        assert!(token_refused(Some(&serde_json::json!({
            "status": "refused", "refusal": {"code": "stale_element_token", "message": "stale"}
        }))));
        assert!(!token_refused(Some(&serde_json::json!({"code": "background_refused"}))));
        assert!(!token_refused(None));
    }

    #[test]
    fn a_finder_item_opens_from_behind_only_while_another_app_is_in_front() {
        // icon view, list and column view names
        assert!(opens_from_behind(false, true, true, "AXImage", true));
        assert!(opens_from_behind(false, true, true, "AXTextField", true));
        // Finder in front: nothing to raise above, AXOpen as before
        assert!(!opens_from_behind(false, true, true, "AXImage", false));
        // a sidebar cell, or any other role
        assert!(!opens_from_behind(false, true, true, "AXCell", true));
        assert!(!opens_from_behind(false, true, true, "AXRow", true));
        // other apps, no AXOpen, the foreground rung
        assert!(!opens_from_behind(false, true, false, "AXImage", true));
        assert!(!opens_from_behind(false, false, true, "AXImage", true));
        assert!(!opens_from_behind(true, true, true, "AXImage", true));
    }

    #[test]
    fn background_element_route_does_not_guess_across_actuator_classes() {
        assert_eq!(
            background_action_for_element(true),
            cua_driver_core::background_input::BackgroundAction::AxSemantic
        );
        assert_eq!(
            background_action_for_element(false),
            cua_driver_core::background_input::BackgroundAction::WindowPointer
        );
    }
}
