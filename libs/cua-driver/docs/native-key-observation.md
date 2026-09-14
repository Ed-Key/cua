# Native navigation-key observation

On macOS, a plain navigation key can finish its window observation with an
immediate check when native readback proves a selection-only change. The
addressed native text field or text area must retain the same readable value,
role, and element selection state, while its focused text range changes.
Web accessibility readback does not produce this evidence.

Eligible requests use `press_key` with an exact PID and window, background
delivery, and Left, Right, Up, Down, Home, or End. Optional Shift, Command, or
Option modifiers are supported. Requests that also address an element or pixel
retain the normal observation wait because they can perform a separate focus
action. Control and Fn combinations, dialog-opening shortcuts, other actions,
unconfirmed or failed input, and missing metadata retain the existing wait.

The immediate path still reads the window signature and current accessibility
roots. An existing transition signal receives the same bounded AX publication
catch-up and owner resolution as before. The input gate, focus suppression,
same-process mutation lease, and action verification remain in place.

This is adaptive, best-effort observation. A selection-only change proves the
editor processed the key; it does not prove an application cannot schedule a
later popup. The immediate path does not wait 400 ms for such a speculative
transition. A later window must be discovered by subsequent state reads and
fresh input admission. Absence of `window_change` is not proof that the window
set will remain unchanged. Workflows expecting a dialog should observe that
dialog explicitly rather than infer its absence from a key result.

The internal `TextSelectionReadback` evidence projects to the existing public
`value_readback` kind. No MCP input, output schema, or SDK binding changes.
Windows and Linux do not emit this internal evidence or use this macOS observer;
their timing behavior is unchanged and native qualification is deferred under
the current local macOS scope.
