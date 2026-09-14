# macOS pixel keyboard focus

Coordinate forms of keyboard tools reuse ClickTool's exact-window hit test,
geometry, cursor feedback and guarded pointer worker. For an unfocused text
editor inside AXWebArea, keyboard preparation can request pointer delivery
instead of relying on an AXFocused write that the renderer may ignore.

The hit-tested element must belong to the requested window before this decision.
Focus is read from that element's AXFocused attribute or application focus
identity. This covers accessory Electron windows that omit the application-level
AXFocusedUIElement. If the exact hit-tested editor is
already focused, the existing non-destructive AX focus path preserves its
selection. Non-editor controls, native Cocoa controls and ordinary public
click focus actions retain their existing dispatch policy. There is no new
public input-route argument and no automatic foreground escalation.

Delivery verification remains separate. A web AX value is not independent
renderer proof; successful posting must not become a confirmed edit merely
because pointer preparation ran.

Focused live coverage uses the existing Electron fixture, journal and foreground
sentinel in electron_first_snapshot_macos_test.rs. The typing cases make a
signed disposable copy with a host that stays in the background and a textarea, then check
actual renderer text and replacement after the visible fixture control selects
all input text. Selection offsets are checked before replacement. Fixture setup does not
modify the installed shared test bundle.

```sh
cargo test --release -p cua-driver --test electron_first_snapshot_macos_test background_coordinate_typing -- --ignored --nocapture --test-threads=1
```

Run inside a logged-in, TCC-authorized macOS test environment with the installed
daemon and staged fixtures. This command is a focused diagnostic, not the full
canonical desktop certification.

This correction is in the macOS adapter. Windows UIA, Linux X11 and Wayland
input adapters are unchanged and have no new native evidence from this work.
The shared protocol is unchanged. Their ability to establish editor focus
without activating a window remains subject to their existing platform and
compositor constraints; macOS results do not establish parity for them.

A separate diagnostic found that background Cmd+A did not establish a selection
in this fixture. That hotkey issue is not resolved or counted as passing by the
pointer-focus change.
