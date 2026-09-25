# Focus and text-selection observations

`get_window_state` can include `focused` and `text_selection` on element records.
The outline marks the focused element and shows `selection_utf16=location:length`.
An example for `A😀BC`, with the first two visible characters selected:

```json
{"focused":true,"text_selection":{"text":"A😀","range":{"location":0,"length":3}}}
```

Range offsets and lengths are UTF-16 code units, not bytes or visible characters.
A range with length zero identifies a caret. An empty selected text string means
the attribute was readable and the selection was empty. Missing fields mean
unknown, unsupported, excluded, or outside the captured tree. They do not mean
an empty selection or an unfocused app. Focus can be on a control outside a
window snapshot or on a node omitted by truncation or filtering.

On macOS, the reader matches `AXFocusedUIElement` against retained snapshot
nodes, reads selection only for the matched text control, and rechecks focus.
If focus changes while reading, it drops that app-level focus observation. Recognized secure
text controls do not publish selected text through this addition. Native
`press_key` verification also compares readable selection ranges on the same
focused control. Unchanged or unavailable ranges do not confirm a key action.

For background web content whose app-level focused-element lookup is unavailable,
the reader accepts a single text control reporting `AXFocused=true` and rechecks
that attribute after reading its selection. Multiple claimed focused editors
remain unknown. This fallback is restricted to web text observations.

For web content, these fields report the accessibility provider's state only.
`in_web_content:true` remains the trust marker. The key verifier continues to
exclude these targets: selection readback does not prove the renderer processed
input or that an edit committed. Use application or DOM evidence as appropriate.

The optional record types are shared in `cua-driver-contract`. This change only
populates them in the macOS adapter. Windows, X11, and Wayland population is not
implemented in this change; absence must not be interpreted as lack of OS support.
Their existing action behavior is unchanged. Native qualification for those
adapters remains future work under the current macOS-only scope.

The state is part of the existing snapshot, so `include_screenshot:false` and
existing queries can read it without a screenshot or a new tool. A query can
exclude the focused control, and each new snapshot retains the existing token
lifecycle. This work is related to issue #2243, not a claim that all of that
issue's native, Chromium, Electron, and lightweight-observation requirements
have been certified.

## Verifying a selection or caret

The existing `verify_state` tool accepts an exact UTF-16 range with optional
selected text. For example, verify the word `this` in `Keep this note.`:

```json
{"pid":42,"window_id":7,"expect":[{"element":{
  "selector":{"role":"AXTextField","label_contains":"Draft message"},
  "text_selection":{"location":5,"length":4,"text":"this"}
}}],"include_screenshot":false}
```

Use `{"location":9,"length":0}` to check a caret. The predicate's location
and length are required; `text` is optional. When supplied, its UTF-16 length
must equal `length`. Overflowing range ends and inconsistent requests are
rejected before observation, including when the predicate is in a later
`run_sequence` step.

The predicate requires one trusted matching control and readable focus and
range metadata. Matching range and requested text on a focused native control
is satisfied. A different range, different requested text, or explicitly false
focus is unsatisfied. Missing or inconsistent metadata is unknown with reason
`unsupported_predicate`; multiple matching native controls are `multi_match`.
Web targets remain `untrusted_source` even when their AX selection matches.
Windows, X11, and Wayland snapshots currently omit this metadata and therefore
cannot satisfy the new predicate; this does not claim an OS limitation.

The result uses the same polling, stability, and bounded evidence as other
predicates. Evidence includes focus and text-selection metadata. It reuses
the existing snapshot reads and does not introduce a new observer or retry
input. This verifies observed state, not which prior action caused it. The
existing `selected` predicate checks element selection, not selected text.
