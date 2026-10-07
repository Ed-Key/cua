# Tool reference

Tool descriptions in `tools/list` are kept short: what the tool does, its key
arguments, and the rules that prevent a wrong call. This file holds the rest,
per tool. Read the section for a tool when its result surprises you or before
using an unfamiliar parameter. Route choice and the observe, act, verify loop
live in [WORKFLOW.md](WORKFLOW.md); macOS delivery details live in
[MACOS.md](MACOS.md). Facts already in those files are not repeated here.
Sections describe the macOS tools; Windows and Linux schemas can differ, so
check `describe <tool>` there.

## list_apps

- Fields per app: `running` (pid is 0 when false), `active` (system frontmost, implies running), `launch_path` (the `.app` bundle path when known; pass it to `launch_app` for a cold start), `kind` (`"desktop"` for `.app` bundles), and `last_used` (RFC 3339 from the bundle's modification time, or null).
- Only apps with the regular activation policy are listed; background helpers and system UI agents are filtered out. Installed apps come from /Applications, /Applications/Utilities, ~/Applications, /System/Applications, and /System/Applications/Utilities.

## list_windows

- Lists layer-0 top-level windows known to WindowServer. Without `pid`, `off_screen_omitted` counts the windows left out (minimized, on another Space, or launched hidden). With `pid`, all of that app's windows are listed.
- Per record: `window_id`, `pid`, `app_name`, `title`, `bounds` (x, y, width, height, top-left origin), `z_index`, `is_on_screen`, `space_ids`, `current_space_id` (the active Space on that window's display), and `on_current_space`. The top-level `current_space_id` is the main active Space and can differ from a record's when displays use separate Spaces.
- If every `z_index` is null, choose the window by an explicit rule (title, bounds, identity), not by array order.

## launch_app

- If both `bundle_id` and `name` are given, `bundle_id` wins. `urls` are handed to the app as open targets; for Finder, a folder path opens a background Finder window there.
- `webkit_inspector_port` sets `WEBKIT_INSPECTOR_SERVER=127.0.0.1:N` and `TAURI_WEBVIEW_AUTOMATION=1`. `additional_arguments` are appended after `--args`.
- `creates_new_application_instance`: use it when another agent or session may drive the same app. Single-instance apps (Calculator, many utilities) otherwise hand every caller the same window, and the sessions fight over it.
- The result has `pid`, `bundle_id`, `name`, `windows`, and `launch_state` (whether the request was sent, the process is running, and a window is ready). On macOS each window's `input_readiness` points to `get_window_state`'s `background_input` report for that window; launch does not activate the app, so do not assume key or pointer input is available. When the target was not already frontmost, `self_activation_suppressed` reports whether focus stayed with the prior frontmost app (true) or the launched app kept it despite the driver's re-demotion (false).

## get_window_state

- Target with `pid` + `window_id`, or on macOS with `app` alone (app name, any case, or exact bundle id). `app` reads the app's only titled window on the current Space. It fails with `app_not_running`, `app_window_not_found`, or `app_window_ambiguous` (which lists `candidates` with `window_id`, `pid`, and `title`). Other platforms refuse `app`.
- macOS `background_input` (the per-route background capability report) comes on the first read of a window in a session and again when it changes, on a full look (`diff:false`), or with `verbose:true`; otherwise it is omitted because it is unchanged.
- Upstream's shape arguments are accepted on macOS as aliases of the fork's, not with upstream's output. `full_output:true` defaults `element_fields` to `"full"` and `diff` to false (explicit `element_fields` or `diff` wins; the typed SDK and the test kit send `full_output` only when no shape argument is passed). `tree_format:"markdown"` is `element_fields:"none"`; `"elements"` and `"both"` are `"full"`, and `tree_markdown` stays in the response either way. Passing `tree_format` with `element_fields` is refused. `since:<snapshot_id>` asks for the diff: when it names this window's latest snapshot and that look is comparable, the response is the fork's change list (`tree_markdown` plus the `diff` object), never upstream's `tree_diff`, `since_status` or `diff_counts`; any other `since` returns the full outline. `verbose:true` sends `background_input` on every read. The default node budget stays 2000 (upstream's 250 applies on Windows and Linux).
- Returns `tree_markdown`, the window's tree as text with actionable rows tagged `[N]` (their `element_index`). On macOS the default `element_fields:"none"` omits `structuredContent.elements`; the tree's first line reads `element_token = <snapshot_id>:<index>`, so row `[N]` has token `<snapshot_id>:N`. Pass `element_fields:"compact"` or `"full"` to get element records. Other platforms always return `elements`.
- Record fields: `element_index`, `element_token`, `role`, `label`, `value` (the element's AXValue; use it to check what a field holds), `actions` (AX action names, omitted when empty), and `screenshot_frame`. Compact records omit `frame`, `parent_index`, `depth`, `enabled` when true, and `selected` when false; `element_fields:"full"` returns every field.
- `include_accessibility_tree:false` skips the tree walk and returns the screenshot plus `window_bounds`, `screenshot_scale`, `screenshot_width`/`screenshot_height`, `app_name`, and `window_title` (the capture-only path, for example a live preview). Setting both `include_accessibility_tree:false` and `include_screenshot:false` is an error.
- `max_image_dimension` overrides the configured screenshot long edge for one call (0 is native resolution). The legacy `max_dimension` applies on top of it; the tighter cap wins.
- With `query`, `element_count` still reports the whole snapshot and `filtered_element_count` the projected rows. Ancestors come from the real accessibility hierarchy, not indentation.
- `max_elements` (default 2000) and `max_depth` (default 25) truncate the markdown and the elements identically. Lower them for Electron and large web apps with 10k+ element trees; rows past them are missing. On macOS, `coverage` reports a depth cut (text below it is unknown) and, with a `query`, `query_excluded_text_nodes`: collected rows with text the query left out.
- When `timeout_ms` runs out, the partial tree returns with `truncated:true`, `truncation_reason`, `nodes_visited`, `nodes_pending`, and `elements_complete:false`. Retry with a larger budget (for example 5000). On macOS a `query` filters after the walk and does not shorten it.
- The read is scoped to `window_id`, and never returns another surface's elements under it. A window on another Space still resolves by its exact id.
  - `window_id_not_found`: the window no longer exists; refresh `list_windows`.
  - `window_owner_pid_mismatch`: another process owns the window; retry with the reported `owner_pid`. A sandboxed app's Open/Save panel is hosted by a separate panel process.
  - `degraded_reason: ax_window_unresolved`: the window is live but its accessibility surface is not; the tree is empty, the screenshot is present, and background input is refused until it resolves. Re-read, or act with `delivery_mode:"foreground"`.
  - `degraded_reason: ax_app_launching` with `truncation_reason: app_lookup_timeout`: the app has not finished launching and did not answer accessibility within `timeout_ms`; the tree is empty. Re-read in a moment or with a larger `timeout_ms`.
  - `px_frame_mismatch` or `px_capture_unavailable`: the screenshot or pixel frame could not be proven against the window bounds (a coherent 1x or 2x image), so it is omitted instead of guessed. The accessibility payload is still valid.

## get_desktop_state

- When `max_image_dimension` downsizes the PNG, the response reports `screenshot_original_width` and `screenshot_original_height`. `x`,`y` read off the smaller image are mapped back to the full-size frame for this session's later desktop actions, or when passed with that capture's `capture_id`.

## zoom

- After a zoom, `from_zoom:true` on `click` or `drag` maps zoom-image pixels back to full-window space.
- Coordinate actions return `screenshot_context_missing` when the latest snapshot has no screenshot owned by this session. `from_zoom` actions return `zoom_context_missing` when the zoom was never created or was replaced; call `get_window_state`, then `zoom`, again on the same connection.

## click

- Element path (`element_token`): an accessibility action on the cached element. It works on background, hidden, minimized, and other-Space windows without moving the real pointer or stealing focus, and the element's cached role and label tell you what you clicked. The visible agent cursor follows the configured motion policy.
- Pixel path (`x`,`y`): synthesized mouse events posted to the pid. It needs a visible on-screen window to anchor the conversion. Use it for canvas, video, WebGL, and custom-drawn surfaces that are absent from the tree.
- `button`: on the pixel path, the matching mouse button. On the element path, `"right"` performs `AXShowMenu` (the same as `right_click`) and `"middle"`, which has no accessibility equivalent, falls back to a pixel middle-click at the element's center.
- `capture_id`: with `x`,`y`, the driver admits and consumes that exact capture before dispatch. A stale, mismatched, or out-of-bounds capture is refused without fallback.
- `debug_image_out` is incompatible with `from_zoom`.
- A generic click has no postcondition read-back, except the selection of list-like rows whose `AXSelected` state can be confirmed. Confirm other effects from a fresh `get_window_state`.

## double_click, right_click, drag

- `double_click` pixel path sends two down/up pairs about 80 ms apart.
- `right_click` on an element performs `AXShowMenu` on the cached element (pure accessibility, no pointer move or focus steal). `modifier` forces the pixel path because accessibility actions do not carry modifier keys. Pass exactly one of an element or `x`,`y`.
- `drag`: `window_id` may be omitted only when the pid owns exactly one eligible top-level window; otherwise the call refuses with `ambiguous_window_target`. Raise `duration_ms` and `steps` for slower, more human drags; lower them for snap gestures. Steps are interpolated linearly.

## scroll

- Targeted wheel path (an element or `x`,`y`): a real mouse-wheel event at that screen point. The renderer hit-tests it like a physical wheel, so it lands on whatever is under the point. It is the only way to scroll a nested `overflow:auto` region, which never takes keyboard focus. Use it for inner scrollers in web views.
- Keystroke path (no target, just `pid` and `direction`): PageDown/PageUp for `by:"page"`, Down/Up arrows for `by:"line"`, Left/Right arrows horizontally. It drives only the focused or page scroller.
- `amount` counts wheel notches on the targeted path and keystroke repeats on the keystroke path.

## type_text

- Background delivery first inserts through `AXSetAttribute(kAXSelectedText)`, which works for standard Cocoa text fields and views. For Chromium and Electron inputs without `kAXSelectedText`, it falls back to synthesized characters when the estimated route fits the daemon transport budget. Longer synthesized routes are refused before any character is sent and report a safe chunk size; one-call accessibility insertion has no cap.
- Without an element or `x`,`y`, text goes to the pid's focused element.
- Web content (Chromium, WebKit, Electron: browser tabs, Slack, VS Code) is detected by an `AXWebArea` ancestor. There, `AXValue` does not prove the renderer saw the write, so the driver never reports a false `confirmed`: Electron targets that cannot be proven native refuse background delivery before mutation (use the `x`,`y` form or foreground), and other web paths return `effect:"unverifiable"` with an escalation. A browser's native address bar and toolbar stay trusted.
- For a browser tab, the typed browser tools ([BROWSER.md](BROWSER.md)) are the reliable route. For an embedded web view, use the `x`,`y` form.
- The typing response has no screenshot; request one with `get_window_state` when accessibility cannot establish the result.
- The summary's first words state the evidence: `✅ Inserted` only when a read-back shows the text; otherwise `⚠️ Not confirmed:` with the reason. Read the field or take a screenshot before typing again.
- Mac Catalyst text views (WhatsApp, Messages, Stocks search) take only typed keys, which reach the field with keyboard focus. An addressed one without focus is refused with `catalyst_text_needs_focus` before any input, in either delivery mode: click it (background is fine), then call `type_text` again.
- Background keystrokes can miss other focus-sensitive surfaces. Retry with `delivery_mode:"foreground"` only when a fresh read shows the text did not appear. Foreground delivery requires explicit authorization.

## press_key and hotkey

- Background (the default) posts to the pid without fronting or raising it, through the macOS 14+ authenticated-message envelope that Chromium and Electron accept as trusted input. With an element target, that element is focused first. `window_id` only targets; it never raises.
- Foreground fronts the exact window, confirms it became key (failing instead of sending if it does not), focuses an addressed element or `x`,`y` when given, sends a genuine HID transition, then restores the prior frontmost app. It reaches Chromium page content, inline editors, native menu equivalents, menu-bar shortcuts such as Cmd+Z and Cmd+W, and the Chromium omnibox. It requires `window_id`.
- `press_key` is confirmed only when a bounded native value or selection read-back changes on the same control. Otherwise an attempted post stays `effect:"unverifiable"`, which does not mean delivery failed. `hotkey` has no read-back; confirm from a screenshot.
- `hotkey` modifiers: cmd/command, shift, option/alt, ctrl/control, fn. The final key uses the `press_key` vocabulary.
- A chord does not focus a text field. To type into a background Electron input, focus it with a pixel click first (or use the `type_text` `x`,`y` form). If an app only accepts paste, call `clipboard_write`, then `clipboard_read` to verify the types (and text) before selecting or replacing content, and only then send Cmd+V.

## set_value

- A popup button (`AXPopUpButton`, an HTML `<select>` in Safari, any native `NSPopUpButton`) gets the option whose title or value matches `value` (case-insensitive): pressed directly when the popup lists its options, otherwise its menu is opened, the option pressed and the menu closed again. Confirmed only when the popup then shows that option; no match lists the options.
- Other elements get `AXValue` written directly; the accessibility layer coerces the string to the element's native type.
- A file's name as a list shows it (Finder's name cell: `AXFilename` and a file URL, not being edited) is refused with `file_name_needs_rename`: a value write changes only what Finder shows, never the file. Rename: click the item, press return, cmd+a, `type_text` the full name, press return.
- A Mac Catalyst text field is written and read back after 300 ms; the result says the app was not sent typed keys, so its reaction is unverified. A Catalyst search field (search role, or "search" in its placeholder or name) is refused with `catalyst_text_needs_typing` before anything is written: a search does not run on a value write. Click it, confirm focus, select all if replacing, then `type_text`.

## act_and_read

- `observe` selects `query`, `query_context`, `element_fields` (none by default, so the read is the tree only), and an optional screenshot for the final read.
- The result carries each child result, including errors, and `stopped_at` (1-based) when a step stopped the run. There is no semantic verification; the fresh read shows what happened.
- It is not a transaction against other clients or user input. macOS only; Windows and Linux are not supported yet.

## run_actions

- Listed only in the full tool profile (`cua-driver mcp --tools full`). Steps are `{tool, args}` for click, double_click, right_click, set_value, type_text, press_key, hotkey, scroll or drag; all are validated before the first runs, and the batch stops at the first failure.
- Each step's report carries only the first line of that tool's result, so outcome lines are not included. `observe` reads once at the end (default `include_screenshot:false`, `max_elements:200`). On macOS prefer `act_and_read` with `steps` for one window.

## run_sequence

- Each step continues only when its predicates are satisfied and stable. Steps that were never attempted are omitted from the result. No screenshots are returned.
- It is not a transaction, and a satisfied postcondition does not prove the step caused it.
- Typed request errors and the verifier's request-validation errors reject the whole request before any step runs. Predicate shapes the verifier classifies as unknown are normal stopped outcomes, not request errors.

## verify_state

- Accessibility projections are conservative: absence stays `unknown` unless the searched domain is proven exhaustive, which is why `exists:false` is rejected instead of returning a predicate that can never resolve.
- `timeout_ms:0` takes one sample.

## Browser tools

[BROWSER.md](BROWSER.md) owns the workflow. Tool facts not repeated there:

- `get_browser_state` binding classifies the native browser window and correlates it to a CDP target exactly or refuses. Target ids are never CDP ids. `app` (macOS) is resolved to `pid` + `window_id` before authorization by the resolver behind `get_window_state {app}`, with the same refusals. A bind reads the active tab through a `get_browser_state {target_id, tab_id}` dispatch of its own; when that read is refused the bind stands and the refusal is under `observation`. `semantic_v2` (the default format) joins accessibility, DOM, layout, and viewport state, ranks visible content before retained or offscreen content, and returns an outline with each element's ref and actions inline, fitted to `max_chars`, with scoped reads and a continuation; `since_revision` returns the change since a revision instead. `dom_refs_v1` returns composed DOM refs as a flat list. When `include_screenshot` cannot be captured, the read is still returned (it is already the session's baseline) with the capture's refusal under `screenshot`.
- `browser_steps` is a composition: each step is dispatched as `browser_click` or `browser_type` and each read as `get_browser_state`, so every one passes the same admission as a single call. It reads the page once at the end. A step's own tool waits for the page to settle (two quiet polls of a mutation counter, 1.5 s at most) but does not read it.
- `browser_prepare` without `pid` accepts only a platform-attested system Chrome or Edge installation (or a root-owned package payload on Linux); redirects and user-controlled locations are refused. Isolated setup never copies, modifies, or terminates the user's profile.
- `browser_click` trusted route is `Input.dispatchMouseEvent`, sent only after `elementFromPoint` at the click point (through open shadow roots, and the closed ones the element itself lives in) finds the ref's element, something inside it, or its label. Refusals say nothing the page says: a stale ref is only reported as changed, and a covering element is named only by a ref the session already holds. Trust-gated controls may ignore a `dom_event` click, so it proves dispatch, not activation; read `changes` and verify.
- `browser_click`, `browser_type` and `browser_navigate` read the page after acting through a `get_browser_state` dispatch of their own; when that read is refused the action's outcome stands and `changes.kind` is `unavailable`.
- `browser_type` `insert_text` uses `Input.insertText`; `keystrokes` uses per-character `Input.dispatchKeyEvent`; `set_value` calls the prototype value setter and fires `input` and `change`. `replace` works through the selection, so `beforeinput` and `input` still fire and framework state stays consistent. Each mode reads the node back; a changed or rejected value is an error with `code: browser_type_mismatch` and the actual value (password values are never echoed). One trailing `\n` is Enter, pressed only after the read-back confirms the text (single-line inputs in every mode, any field in `keystrokes`); `enter` in the result says what the field holds after it.
- `browser_dialog` never handles browser permission UI, extension UI, native dialogs, or file pickers. Resolution defaults to background delivery; Linux must request foreground because Chromium's native modal cannot be resolved there otherwise. `prompt_text` is treated as sensitive.
- `browser_set_input_files` never returns local paths.
- `browser_download` refuses ambiguous or stale capabilities.
- `browser_tabs` works on the user's own logged-in Chrome profile. `list` returns windows, tabs with title and URL, and tab groups. `load` loads a tab Chrome restored or discarded without loading; page reads refuse such a tab until then. `group` can name and color a group. The error says when the extension is not connected.

## page (legacy)

The typed browser tools give exact targeting, endpoint ownership, and consent; the legacy mutation opt-in does not provide their exact binding or existing-profile grant guarantees. Restart the daemon after changing `CUA_DRIVER_ENABLE_LEGACY_PAGE_MUTATIONS`. Supports Chrome, Brave, Edge, Safari (AppleScript on macOS), Electron apps (CDP), Chromium and Firefox on Windows (UIA for reads; CDP for `execute_javascript` when `--remote-debugging-port` is set), and WKWebView, Tauri, and AT-SPI fallbacks.

Actions:

- `get_text`: visible page text. `query_dom`: elements matching `css_selector`, with the requested `attributes`.
- `execute_javascript`: run `javascript` and return the result.
- `click_element`: click the element matching `selector`, animating the agent cursor to its center first so the user sees it. Prefer it over `execute_javascript('el.click()')` when visible cursor feedback matters.
- `insert_text`: insert `text` at the DOM focus in one native operation (CDP `Input.insertText`). Rich-text editors treat it like an IME commit, so try it before `type_keystrokes` when a contenteditable discarded a script write. Focus the field first.
- `type_keystrokes`: real per-character key events at the DOM focus. Slower, but use it when `insert_text` is also discarded or the editor needs real keydown and keyup. Focus the field first.
- `enable_javascript_apple_events` (macOS): patch Chrome, Brave, or Edge preferences to allow JavaScript from Apple Events; needs `bundle_id`, user confirmation, and a browser restart.
- `cdp_port` is needed when the port was opened through the browser's own remote-debugging toggle, which may not answer discovery. `target_url_contains` exists because nothing links `window_id` to the tab a CDP call reaches.

## Recording tools

[RECORDING.md](RECORDING.md) owns the workflow and the turn-folder contents. Tool facts not repeated there:

- Recorded action tools include `click`, `right_click`, `scroll`, `type_text`, `press_key`, `hotkey`, and `set_value`. Turn numbering restarts at 1 each time recording starts, and `get_recording_state`'s counter increments on every recorded action.
- With `include_accessibility_tree:false`, state is classified `state_capture_disabled`.
- Video is H.264 at 30 fps for the life of the session and is torn down when the MCP client disconnects. On macOS it is a daemon-owned `SCStream` with `SCRecordingOutput` under the daemon's Screen Recording grant, with no ffmpeg; it captures screen only, no system audio. On macOS 26 the first direct capture can show a one-time consent to bypass the private window picker; choose Allow, or have a human run `cua-driver permissions grant` first.
- Windows and Linux need ffmpeg on PATH (`winget install Gyan.FFmpeg`, `apt install ffmpeg`, or `install_ffmpeg`). When ffmpeg is missing or fails to start, per-turn capture still runs and `last_error` carries the diagnostic. ffmpeg runs as a separate process, never linked into the driver.
- Recorder state lives for the life of the daemon; a restart resets it to disabled with nothing on disk.
- `stop_recording` terminates the video writer gracefully so the mp4 is finalized and playable. Ownership-scoped teardown (one client disconnecting cannot stop a recording a later client started) is the session-end hook's job, not this tool's.
- `replay_trajectory` parses each `turn-NNNNN/action.json` in lexical order and calls the tool through the same dispatch path as an MCP or CLI call. Failures are reported and stop the replay only when `stop_on_error` is true. Read-only tools such as `get_window_state` are not recorded, so replay does not refill the element cache. Recording a replay against a new build and diffing the two trajectories is the intended regression-test workflow.

## Sessions and clipboard

- `start_session` is optional because an ordinary action creates or reuses a named session on demand. `capture_scope` is deprecated compatibility input.
- `escalate_session` and `get_session_state` exist only for legacy capture-scope sessions; there is no `deescalate_session`.
- `list_sessions` is scoped to the caller's transport lease. `end_session` runs each cleanup hook exactly once.
- Clipboard content is never retained in telemetry.

## check_permissions

- Fields: `accessibility` and `screen_recording` (booleans from the TCC preflight APIs); `screen_recording_capturable` (a live ScreenCaptureKit probe when prompting, null on read-only calls); `direct_capture_status` (`ready`, `unavailable`, `timed_out`, `probe_failed`, `blocked_by_screen_recording`, or `not_checked`); `direct_capture_error` (a structured timeout or probe failure); `direct_capture_verification` (validated source, UTC time, and bundle identity from an explicit grant probe); and `source` (which identity the booleans reflect: the CuaDriver daemon or the launching terminal or IDE).
- macOS attributes grants to the responsible process, so a standalone call from a terminal reports the terminal's grants, not the driver's.
- The prompt-capable capture probe never runs when `prompt` is false. A trusted host setup route can pass `probe_direct_capture:false` with `prompt:true` to request only the two TCC grants before explaining the macOS 26 direct-capture consent separately.

## health_report

One stable call instead of stitching together `check_permissions`, `doctor`, version, bundle attribution, and platform capability status. On macOS the prompt-capable direct-capture probe is deliberately skipped; a human verifies it with `cua-driver permissions grant`.

Input, all optional: `include` (run only these checks) and `skip` (skip these). If both are given, `include` wins.

Check names:

- macOS: `binary_version`, `platform_supported`, `session_active`, `bundle_identity`, `tcc_accessibility`, `tcc_screen_recording`, `ax_capability`, `screen_capture_capability`
- Windows: `binary_version`, `platform_supported`, `session_active`, `ax_capability` (UIA), `screen_capture_capability` (DXGI)
- Linux: `binary_version`, `platform_supported`, `session_active`, `ax_capability` (AT-SPI), `screen_capture_capability` (X11)

Output, `schema_version: "1"`:

```json
{
  "schema_version": "1",
  "platform": "darwin | win32 | linux",
  "driver_version": "<semver>",
  "overall": "ok | degraded | failed",
  "checks": [
    {"name": "<check name>", "status": "pass | fail | skip",
     "message": "<one line, always present>", "hint": "<remediation when status is fail>",
     "data": {}}
  ]
}
```

`overall` is `ok` when every non-skipped check passes, `degraded` when a non-core check fails (the binary is still usable), and `failed` when a core check fails (`binary_version`, `platform_supported`, `session_active`). Breaking changes will bump `schema_version` to `"2"`; new check names under `"1"` are non-breaking, so tolerate unknown names.

## set_config and get_config

- The experimental picture-in-picture backend starts once at daemon startup, which is why its keys apply only after a restart.
- `capture_mode` is not a stored setting (it is a deprecated per-call parameter), and the old `capture_scope` config key is retired. Each action's target selects the capture modality.

## Other tools

- `kill_app` is `kill -9` on macOS and Linux and `taskkill /F` on Windows. Use it only after the cooperative close (Cmd+Q on macOS, the window's close button on Windows) failed to end the process.
- `bring_to_front` is for a focus-proxy surface that must stay frontmost across interactions. With `window_id`, success means the exact ordinary window was independently verified as the focused window and first in WindowServer layer-0 order; request acceptance alone is reported as partial, never as activation.
- `check_for_update` reports the current and selected channels. Pacman-owned Linux executables get package-manager guidance without a GitHub check. It mirrors `cua-driver check-update --json`.
- `install_extension` preview also names the artifact's source; `confirm:true` installs exactly the previewed, verified plan.
