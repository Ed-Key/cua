// Pure rules for how long the extension keeps a tab's debugger attached.
// background.js applies them; tests/lifecycle.test.mjs checks them with
// `node --test`.

// Backstop only. The normal release path is the driver's debugger.detach
// when the last Cua session holding a tab ends (and tab close, Stop, or the
// driver disconnecting). A tab with no debugger command from any holder for
// this long is detached anyway, so an ownership bug in the driver cannot
// leave Chrome's debugging banner up forever.
export const IDLE_BACKSTOP_MS = 10 * 60 * 1000;

// Tabs whose last debugger command is at least `limitMs` old.
export function idleTabs(lastCommandAt, now, limitMs = IDLE_BACKSTOP_MS) {
  return [...lastCommandAt].filter(([, at]) => now - at >= limitMs).map(([tabId]) => tabId);
}

// Tabs the backstop detaches: idle ones without an open JavaScript dialog.
// Chrome drops a pending dialog when the debugger detaches, which would leave
// browser_dialog nothing to accept or dismiss; such a tab waits until the
// dialog closes.
export function backstopTabs(lastCommandAt, dialogOpen, now, limitMs = IDLE_BACKSTOP_MS) {
  return idleTabs(lastCommandAt, now, limitMs).filter((tabId) => !dialogOpen.has(tabId));
}

// An attach that started under one native-messaging connection and finished
// under another (or none) belongs to no live driver: detach it at once.
export function attachOutlivedConnection(startedUnder, current) {
  return startedUnder !== current;
}

// A request is answered only for the connection it arrived on: one that
// arrived before a disconnect (or reconnect) belongs to no live driver and
// must not attach anything.
export function requestIsStale(arrivedUnder, current, connected) {
  return !connected || arrivedUnder !== current;
}
