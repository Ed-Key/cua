// Cua Driver for Chrome: a thin bridge between the local cua-driver daemon and
// this Chrome profile. The daemon reaches this worker over native messaging
// (host "com.trycua.cua_driver") and sends JSON-RPC requests; the worker
// answers them with chrome.tabs, chrome.tabGroups, chrome.windows, and
// chrome.debugger calls, and forwards debugger events. It holds no agent logic.

import { clearActive, markActive, refresh } from "./indicator.js";

const HOST = "com.trycua.cua_driver";
const RECONNECT_ALARM = "cua-driver-reconnect";
const AGENT_GROUP = { title: "Cua", color: "cyan" };
const STOPPED_MESSAGE =
  "the user pressed Stop for Cua in this tab; ask them before continuing " +
  "(they can allow it again from the Cua Driver toolbar button)";

let port = null;
// Tabs this extension attached the debugger to, so detach only undoes its own.
const attached = new Set();
// Tabs where the user pressed Stop. Cua may not act in them again until the
// user clicks the Cua Driver toolbar button. Kept in session storage so a
// service-worker restart cannot quietly lift a Stop.
const stopped = new Set();
const stoppedLoaded = chrome.storage.session
  .get("stopped")
  .then(({ stopped: saved }) => (saved ?? []).forEach((tabId) => stopped.add(tabId)))
  .catch(() => {});
const saveStopped = () => chrome.storage.session.set({ stopped: [...stopped] }).catch(() => {});

// The debugger is released after this long without a command, which also
// clears Chrome's debugging banner; the next command attaches again.
const DEBUGGER_IDLE_MS = 20000;
const debuggerIdle = new Map();
// Tabs whose Page domain Cua enabled: replayed after an idle reattach so
// dialog events keep flowing to the daemon's existing session.
const pageEnabled = new Set();
// Tabs showing a JavaScript dialog. Chrome forgets a pending dialog when the
// debugger detaches, so an idle release waits until the dialog closes.
const dialogOpen = new Set();
// Attach and detach run one at a time per tab, so Stop cannot slip between
// an attach starting and a command being sent.
const tabQueues = new Map();

function serialized(tabId, work) {
  const run = (tabQueues.get(tabId) ?? Promise.resolve()).then(work, work);
  tabQueues.set(tabId, run.catch(() => {}));
  return run;
}

function touchDebugger(tabId) {
  clearTimeout(debuggerIdle.get(tabId));
  debuggerIdle.set(tabId, setTimeout(() => void releaseDebugger(tabId, "idle"), DEBUGGER_IDLE_MS));
}

// "idle" keeps the daemon's sessions (the next command reattaches);
// anything else ends them.
function releaseDebugger(tabId, reason) {
  return serialized(tabId, async () => {
    if (reason === "idle" && dialogOpen.has(tabId)) {
      touchDebugger(tabId);
      return;
    }
    clearTimeout(debuggerIdle.get(tabId));
    debuggerIdle.delete(tabId);
    // After a deliberate detach nothing is known about the tab's dialogs;
    // Chrome sends no event for detaches the extension makes itself.
    if (reason !== "idle") {
      pageEnabled.delete(tabId);
      dialogOpen.delete(tabId);
    }
    if (!attached.delete(tabId)) return;
    await chrome.debugger.detach({ tabId }).catch(() => {});
    // Chrome reports only detaches it caused; tell the daemon about this one.
    const method = reason === "idle" ? "debugger.released" : "debugger.detached";
    post({ jsonrpc: "2.0", method, params: { source: { tabId }, reason } });
  });
}

function refuseIfStopped(tabId) {
  if (stopped.has(tabId)) throw new Error(STOPPED_MESSAGE);
}

// A tab Chrome restored without loading, or discarded to save memory, has no
// page, and debugger commands to it hang. Reads never reload it (that reruns
// the page's scripts and requests); the caller loads it explicitly first.
const NOT_LOADED_MESSAGE =
  "this tab is not loaded (Chrome restored or discarded it); load it first with " +
  "browser_tabs action load, which reloads it in the background";
const LOAD_TIMEOUT_MS = 10000;

async function refuseIfNotLoaded(tabId) {
  const tab = await chrome.tabs.get(tabId);
  if (tab.status === "unloaded" || tab.discarded) throw new Error(NOT_LOADED_MESSAGE);
}

// One load in flight per tab (a second request joins it), and its Stop hook.
const loadsInFlight = new Map();
const loadCancels = new Map();

// Reload a sleeping tab in place and wait for its page. One settle path
// clears the timer, both listeners, and the Stop hook on success, failure,
// timeout, close, or Stop.
function loadTab(tabId) {
  const inFlight = loadsInFlight.get(tabId);
  if (inFlight) return inFlight;
  const load = new Promise((resolve, reject) => {
    let timer;
    const settle = (error) => {
      clearTimeout(timer);
      chrome.tabs.onUpdated.removeListener(onUpdated);
      chrome.tabs.onRemoved.removeListener(onRemoved);
      loadCancels.delete(tabId);
      loadsInFlight.delete(tabId);
      if (error) reject(error);
      else resolve();
    };
    loadCancels.set(tabId, () => settle(new Error(STOPPED_MESSAGE)));
    const onUpdated = (id, change) => {
      if (id === tabId && change.status === "complete") settle();
    };
    const onRemoved = (id) => {
      if (id === tabId) settle(new Error("the tab was closed while loading"));
    };
    chrome.tabs.onUpdated.addListener(onUpdated);
    chrome.tabs.onRemoved.addListener(onRemoved);
    timer = setTimeout(() => settle(new Error("the tab did not finish loading in time")), LOAD_TIMEOUT_MS);
    chrome.tabs.reload(tabId).catch(settle);
  });
  loadsInFlight.set(tabId, load);
  return load;
}

function ensureAttached(tabId) {
  return serialized(tabId, async () => {
    refuseIfStopped(tabId);
    if (!attached.has(tabId)) {
      await refuseIfNotLoaded(tabId);
      await chrome.debugger.attach({ tabId }, "1.3");
      attached.add(tabId);
      // Nothing runs in the tab after a Stop, not even the replay below.
      if (stopped.has(tabId)) {
        attached.delete(tabId);
        await chrome.debugger.detach({ tabId }).catch(() => {});
        throw new Error(STOPPED_MESSAGE);
      }
      if (pageEnabled.has(tabId)) await chrome.debugger.sendCommand({ tabId }, "Page.enable", {});
    }
    // Stop may have landed while the attach was in flight.
    if (stopped.has(tabId)) {
      attached.delete(tabId);
      await chrome.debugger.detach({ tabId }).catch(() => {});
      throw new Error(STOPPED_MESSAGE);
    }
    touchDebugger(tabId);
  });
}

function connect() {
  if (port) return;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch {
    port = null;
    return;
  }
  port.onMessage.addListener(handleMessage);
  port.onDisconnect.addListener(() => {
    // Reading lastError marks it handled; the alarm reconnects later.
    void chrome.runtime.lastError;
    port = null;
  });
  post({
    jsonrpc: "2.0",
    method: "hello",
    params: {
      extensionId: chrome.runtime.id,
      version: chrome.runtime.getManifest().version,
      userAgent: navigator.userAgent,
    },
  });
}

function post(message) {
  try {
    port?.postMessage(message);
  } catch {
    // The port closed between the check and the post; the alarm reconnects.
  }
}

// Drop undefined fields: Chrome's API schemas reject explicit undefined.
function defined(object) {
  return Object.fromEntries(Object.entries(object).filter(([, value]) => value !== undefined));
}

function tabInfo(tab) {
  return {
    tabId: tab.id,
    windowId: tab.windowId,
    index: tab.index,
    title: tab.title ?? "",
    url: tab.url ?? tab.pendingUrl ?? "",
    active: tab.active,
    pinned: tab.pinned,
    groupId: tab.groupId,
    audible: Boolean(tab.audible),
    muted: Boolean(tab.mutedInfo?.muted),
    status: tab.status,
    discarded: Boolean(tab.discarded),
  };
}

function groupInfo(group) {
  return {
    groupId: group.id,
    windowId: group.windowId,
    title: group.title ?? "",
    color: group.color,
    collapsed: group.collapsed,
  };
}

function windowInfo(window) {
  return {
    windowId: window.id,
    focused: window.focused,
    state: window.state,
    incognito: window.incognito,
    left: window.left,
    top: window.top,
    width: window.width,
    height: window.height,
  };
}

const handlers = {
  ping: async () => ({ pong: true }),

  "windows.list": async () =>
    (await chrome.windows.getAll({ windowTypes: ["normal"] })).map(windowInfo),

  "tabs.list": async ({ windowId }) =>
    (await chrome.tabs.query(defined({ windowId }))).map(tabInfo),

  // New tabs open in the background unless the caller asks for focus, and
  // join the window's "Cua" group so the agent's tabs stay together.
  "tabs.create": async ({ url, windowId, index, active = false, group = true }) => {
    const tab = await chrome.tabs.create(defined({ url, windowId, index, active }));
    if (group) await addToAgentGroup(tab);
    return tabInfo(await chrome.tabs.get(tab.id));
  },

  "tabs.update": async ({ tabId, url, active, pinned, muted }) =>
    tabInfo(await chrome.tabs.update(tabId, defined({ url, active, pinned, muted }))),

  "tabs.move": async ({ tabIds, windowId, index = -1 }) => {
    const moved = await chrome.tabs.move(tabIds, defined({ windowId, index }));
    return (Array.isArray(moved) ? moved : [moved]).map(tabInfo);
  },

  "tabs.load": async ({ tabId }) => {
    const tab = await chrome.tabs.get(tabId);
    // A load already under way reports "loading": join it rather than
    // returning before the page is there.
    if (tab.status === "unloaded" || tab.discarded || loadsInFlight.has(tabId)) {
      refuseIfStopped(tabId);
      await loadTab(tabId);
    }
    return tabInfo(await chrome.tabs.get(tabId));
  },

  "tabs.remove": async ({ tabIds }) => {
    await chrome.tabs.remove(tabIds);
    return { removed: tabIds.length };
  },

  "tabs.group": async ({ tabIds, groupId, windowId, title, color, collapsed }) => {
    const createProperties = groupId === undefined && windowId !== undefined ? { windowId } : undefined;
    const id = await chrome.tabs.group(defined({ tabIds, groupId, createProperties }));
    const update = defined({ title, color, collapsed });
    const group = Object.keys(update).length
      ? await chrome.tabGroups.update(id, update)
      : await chrome.tabGroups.get(id);
    return groupInfo(group);
  },

  "tabs.ungroup": async ({ tabIds }) => {
    await chrome.tabs.ungroup(tabIds);
    return { ungrouped: tabIds.length };
  },

  "tabGroups.list": async ({ windowId }) =>
    (await chrome.tabGroups.query(defined({ windowId }))).map(groupInfo),

  "tabGroups.update": async ({ groupId, title, color, collapsed }) =>
    groupInfo(await chrome.tabGroups.update(groupId, defined({ title, color, collapsed }))),

  "debugger.attach": async ({ tabId }) => {
    await ensureAttached(tabId);
    return { attached: true };
  },

  "debugger.detach": async ({ tabId }) => {
    await releaseDebugger(tabId, "requested");
    return { detached: true };
  },

  "debugger.send": async ({ tabId, sessionId, method, params }) => {
    await ensureAttached(tabId);
    refuseIfStopped(tabId);
    if (method === "Page.enable" && !sessionId) pageEnabled.add(tabId);
    return chrome.debugger.sendCommand(defined({ tabId, sessionId }), method, params ?? {});
  },

  "debugger.targets": async () => chrome.debugger.getTargets(),
};

// The window's "Cua" group, reused while it exists.
async function addToAgentGroup(tab) {
  const groups = await chrome.tabGroups.query({ windowId: tab.windowId, title: AGENT_GROUP.title });
  const existing = groups.find((group) => group.color === AGENT_GROUP.color);
  const groupId = await chrome.tabs.group(
    existing ? { tabIds: [tab.id], groupId: existing.id } : { tabIds: [tab.id], createProperties: { windowId: tab.windowId } },
  );
  if (!existing) await chrome.tabGroups.update(groupId, AGENT_GROUP);
}

// The tabs a request acts on (reads such as tabs.list touch none).
function tabsOf(method, params) {
  if (method === "tabs.list" || method === "tabs.create" || method.startsWith("windows.") ||
      method.startsWith("tabGroups.") || method === "debugger.targets" || method === "ping") {
    return [];
  }
  return [params.tabId, ...(Array.isArray(params.tabIds) ? params.tabIds : [])].filter(
    (tabId) => typeof tabId === "number",
  );
}

async function stopTab(tabId) {
  stopped.add(tabId);
  const cancelLoad = loadCancels.get(tabId);
  if (cancelLoad) {
    cancelLoad();
    // Put the page back to sleep; Chrome keeps the active tab loaded.
    await chrome.tabs.discard(tabId).catch(() => {});
  }
  await saveStopped();
  clearActive(tabId);
  await releaseDebugger(tabId, "stopped_by_user");
  await chrome.action.setBadgeText({ text: "off" });
  await chrome.action.setTitle({ title: "Cua Driver: stopped in a tab. Click to allow it again." });
  post({ jsonrpc: "2.0", method: "user.stop", params: { tabId } });
}

async function handleMessage(message) {
  if (!message || message.id === undefined || typeof message.method !== "string") return;
  await stoppedLoaded;
  const tabs = tabsOf(message.method, message.params ?? {});
  if (tabs.some((tabId) => stopped.has(tabId))) {
    post({ jsonrpc: "2.0", id: message.id, error: { code: -32001, message: STOPPED_MESSAGE } });
    return;
  }
  // Closing a tab is not work in it; everything else shows the indicator, in
  // the color of the session attaching to the tab when the daemon sends one.
  const color = /^#[0-9A-F]{6}$/i.test(message.params?.sessionColor)
    ? message.params.sessionColor
    : undefined;
  if (message.method !== "tabs.remove") tabs.forEach((tabId) => markActive(tabId, color));
  const handler = handlers[message.method];
  if (!handler) {
    post({ jsonrpc: "2.0", id: message.id, error: { code: -32601, message: `unknown method ${message.method}` } });
    return;
  }
  try {
    const result = await handler(message.params ?? {});
    post({ jsonrpc: "2.0", id: message.id, result: result ?? null });
  } catch (error) {
    post({ jsonrpc: "2.0", id: message.id, error: { code: -32000, message: String(error?.message ?? error) } });
  }
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (method === "Page.javascriptDialogOpening") dialogOpen.add(source.tabId);
  if (method === "Page.javascriptDialogClosed") dialogOpen.delete(source.tabId);
  post({ jsonrpc: "2.0", method: "debugger.event", params: { source, method, params } });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  attached.delete(source.tabId);
  pageEnabled.delete(source.tabId);
  dialogOpen.delete(source.tabId);
  clearTimeout(debuggerIdle.get(source.tabId));
  debuggerIdle.delete(source.tabId);
  // "canceled_by_user": the user dismissed Chrome's debugging banner.
  if (reason === "canceled_by_user") void stopTab(source.tabId);
  post({ jsonrpc: "2.0", method: "debugger.detached", params: { source, reason } });
});

// The daemon shows each session's cursor only over the tab it works in, so it
// hears when the selected tab changes, whether the user or Cua switched it.
// With every tab of the window, so the daemon finds the window even when no
// session works in the new tab (and hides the others' cursors).
chrome.tabs.onActivated.addListener(async ({ tabId, windowId }) => {
  // Page targets only, as the relay reports them: a tab can list other kinds.
  const [all, tabs] = await Promise.all([chrome.debugger.getTargets(), chrome.tabs.query({ windowId })]);
  const targets = all.filter((target) => target.type === "page");
  const inWindow = new Set(tabs.map((tab) => tab.id));
  const windowTargets = targets.filter((target) => inWindow.has(target.tabId)).map((target) => target.id);
  const selected = targets.find((target) => target.tabId === tabId)?.id ?? null;
  post({ jsonrpc: "2.0", method: "tabs.activated", params: { targetId: selected, windowId, windowTargets } });
});

chrome.runtime.onMessage.addListener((message, sender) => {
  if (message?.type === "cua-stop" && sender.tab?.id !== undefined) void stopTab(sender.tab.id);
});

chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status === "complete") refresh(tabId);
});

chrome.tabs.onRemoved.addListener((tabId) => {
  if (stopped.delete(tabId)) void saveStopped();
  attached.delete(tabId);
  pageEnabled.delete(tabId);
  dialogOpen.delete(tabId);
  tabQueues.delete(tabId);
  clearTimeout(debuggerIdle.get(tabId));
  debuggerIdle.delete(tabId);
  clearActive(tabId);
});

// The toolbar button lets Cua act again in tabs where the user pressed Stop.
chrome.action.onClicked.addListener(async () => {
  stopped.clear();
  await saveStopped();
  await chrome.action.setBadgeText({ text: "" });
  await chrome.action.setTitle({ title: "Cua Driver" });
});

// The worker can be stopped at any time; the alarm (30 s minimum) and the
// lifecycle events bring the native link back.
chrome.alarms.create(RECONNECT_ALARM, { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === RECONNECT_ALARM) connect();
});
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();
