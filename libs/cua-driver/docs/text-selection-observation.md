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
If focus changes while reading, it drops the focus/selection observation. Secure
text controls do not publish selected text through this addition. Native
`press_key` verification also compares readable selection ranges on the same
focused control. Unchanged or unavailable ranges do not confirm a key action.

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
