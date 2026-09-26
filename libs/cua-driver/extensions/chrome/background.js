// Cua Driver for Chrome: a thin bridge between the local cua-driver daemon and
// this Chrome profile. The daemon reaches this worker over native messaging
// (host "com.trycua.cua_driver") and sends JSON-RPC requests; the worker
// answers them with chrome.tabs, chrome.tabGroups, chrome.windows, and
// chrome.debugger calls, and forwards debugger events. It holds no agent logic.

import { clearActive, markActive, refresh } from "./indicator.js";

const HOST = "com.trycua.cua_driver";
const RECONNECT_ALARM = "cua-driver-reconnect";
const AGENT_GROUP = { title: "Cua", color: "cyan" };

let port = null;
// Tabs this extension attached the debugger to, so detach only undoes its own.
const attached = new Set();
// Tabs where the user pressed Stop. Cua may not act in them again until the
// user clicks the Cua Driver toolbar button.
const stopped = new Set();

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
    if (!attached.has(tabId)) {
      await chrome.debugger.attach({ tabId }, "1.3");
      attached.add(tabId);
    }
    return { attached: true };
  },

  "debugger.detach": async ({ tabId }) => {
    if (attached.delete(tabId)) await chrome.debugger.detach({ tabId });
    return { detached: true };
  },

  "debugger.send": async ({ tabId, sessionId, method, params }) =>
    chrome.debugger.sendCommand(defined({ tabId, sessionId }), method, params ?? {}),

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
  clearActive(tabId);
  if (attached.delete(tabId)) await chrome.debugger.detach({ tabId }).catch(() => {});
  await chrome.action.setBadgeText({ text: "off" });
  await chrome.action.setTitle({ title: "Cua Driver: stopped in a tab. Click to allow it again." });
  post({ jsonrpc: "2.0", method: "user.stop", params: { tabId } });
}

async function handleMessage(message) {
  if (!message || message.id === undefined || typeof message.method !== "string") return;
  const tabs = tabsOf(message.method, message.params ?? {});
  if (tabs.some((tabId) => stopped.has(tabId))) {
    post({
      jsonrpc: "2.0",
      id: message.id,
      error: {
        code: -32001,
        message: "the user pressed Stop for Cua in this tab; ask them before continuing " +
          "(they can allow it again from the Cua Driver toolbar button)",
      },
    });
    return;
  }
  // Closing a tab is not work in it; everything else shows the indicator.
  if (message.method !== "tabs.remove") tabs.forEach(markActive);
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
  post({ jsonrpc: "2.0", method: "debugger.event", params: { source, method, params } });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  attached.delete(source.tabId);
  // "canceled_by_user": the user dismissed Chrome's debugging banner.
  if (reason === "canceled_by_user") void stopTab(source.tabId);
  post({ jsonrpc: "2.0", method: "debugger.detached", params: { source, reason } });
});

chrome.runtime.onMessage.addListener((message, sender) => {
  if (message?.type === "cua-stop" && sender.tab?.id !== undefined) void stopTab(sender.tab.id);
});

chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status === "complete") refresh(tabId);
});

chrome.tabs.onRemoved.addListener((tabId) => {
  stopped.delete(tabId);
  attached.delete(tabId);
  clearActive(tabId);
});

// The toolbar button lets Cua act again in tabs where the user pressed Stop.
chrome.action.onClicked.addListener(async () => {
  stopped.clear();
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
