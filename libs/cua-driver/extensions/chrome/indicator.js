// What the user sees while Cua works in a tab: a soft glow around the page, a
// "Cua is working in this tab" pill with Stop, and the Cua cursor drawn on the
// tab's favicon. All of it follows activity: it appears on the first command
// for a tab and disappears a few seconds after the last one.

export const CUA_BLUE = "#5EC0E8";
const IDLE_MS = 4000;

// The on-screen agent cursor, from cursor-overlay's build_default_theme.py
// (128-unit canvas).
const CURSOR_PATH =
  "M55,30 C48,28 42,33 43,41 C43,41 64,98 64,98 C67,106 73,106 77,99 C77,99 86,79 86,79 " +
  "C88,75 91,72 95,70 C95,70 108,63 108,63 C115,59 114,53 107,50 C107,50 55,30 55,30 Z";

const idleTimers = new Map();
// The color of the session last working in each tab (its cursor color).
const colors = new Map();
// Bumped on every show/hide, so a slow favicon render cannot redraw the
// indicator after a later hide (Stop, idle) already ran.
const generations = new Map();

/** Note activity in a tab; shows the indicator when it was idle or recolored. */
export function markActive(tabId, color) {
  if (typeof tabId !== "number" || tabId < 0) return;
  const recolored = color !== undefined && color !== colors.get(tabId);
  if (recolored) colors.set(tabId, color);
  const wasActive = idleTimers.has(tabId) && !recolored;
  clearTimeout(idleTimers.get(tabId));
  idleTimers.set(tabId, setTimeout(() => {
    idleTimers.delete(tabId);
    void show(tabId, false);
  }, IDLE_MS));
  if (!wasActive) void show(tabId, true);
}

/** Hide the indicator now (Stop, detach, or tab closing). */
export function clearActive(tabId) {
  clearTimeout(idleTimers.get(tabId));
  idleTimers.delete(tabId);
  void show(tabId, false);
}

export function isActive(tabId) {
  return idleTimers.has(tabId);
}

/** Re-draw after a navigation replaced the page mid-task. */
export function refresh(tabId) {
  if (idleTimers.has(tabId)) void show(tabId, true);
}

async function show(tabId, on) {
  const generation = (generations.get(tabId) ?? 0) + 1;
  generations.set(tabId, generation);
  const color = colors.get(tabId) ?? CUA_BLUE;
  const favicon = on ? await badgedFavicon(tabId, color).catch(() => null) : null;
  if (generations.get(tabId) !== generation) return;
  // Pages Chrome does not let extensions script (chrome://, the Web Store)
  // simply show nothing.
  await chrome.scripting
    .executeScript({ target: { tabId }, func: pageIndicator, args: [on, favicon, color] })
    .catch(() => {});
}

// The tab's own favicon with the Cua cursor over its lower right. The cursor
// covers most of the icon so it reads at the 16 px tab size.
async function badgedFavicon(tabId, color) {
  const tab = await chrome.tabs.get(tabId);
  const size = 64;
  const canvas = new OffscreenCanvas(size, size);
  const context = canvas.getContext("2d");
  if (tab.url) {
    try {
      const source = new URL(chrome.runtime.getURL("/_favicon/"));
      source.searchParams.set("pageUrl", tab.url);
      source.searchParams.set("size", String(size));
      const bitmap = await createImageBitmap(await (await fetch(source)).blob());
      context.drawImage(bitmap, 0, 0, size, size);
    } catch {
      // No favicon: the cursor alone still marks the tab.
    }
  }
  const scale = (size * 0.8) / 69;
  context.save();
  context.translate(size - 108 * scale - 1, size - 99 * scale - 1);
  context.scale(scale, scale);
  const cursor = new Path2D(CURSOR_PATH);
  context.shadowColor = "rgba(0, 0, 0, 0.45)";
  context.shadowBlur = 6;
  context.lineJoin = "round";
  context.lineWidth = 10;
  context.strokeStyle = "#ffffff";
  context.stroke(cursor);
  context.shadowColor = "transparent";
  context.fillStyle = color;
  context.fill(cursor);
  context.restore();
  const bytes = new Uint8Array(await (await canvas.convertToBlob({ type: "image/png" })).arrayBuffer());
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return `data:image/png;base64,${btoa(binary)}`;
}

// Runs in the page (isolated world). Only DOM and CSSOM calls: strict pages
// block injected <style> elements and innerHTML, but not these.
function pageIndicator(on, favicon, color) {
  const HOST_ID = "cua-driver-indicator";
  const ICON_ID = "cua-driver-favicon";
  const doc = document;
  const set = (element, styles) => Object.assign(element.style, styles);

  // Favicon: remember the page's own icons once, point them at the badge,
  // and put them back afterwards.
  const icons = [...doc.querySelectorAll('link[rel~="icon"]')].filter((link) => link.id !== ICON_ID);
  if (on && favicon) {
    for (const link of icons) {
      if (!("cuaHref" in link.dataset)) link.dataset.cuaHref = link.getAttribute("href") ?? "";
      link.setAttribute("href", favicon);
    }
    let mine = doc.getElementById(ICON_ID);
    if (!mine) {
      mine = doc.createElement("link");
      mine.id = ICON_ID;
      mine.rel = "icon";
      (doc.head ?? doc.documentElement).appendChild(mine);
    }
    mine.href = favicon;
  }
  if (!on) {
    doc.getElementById(ICON_ID)?.remove();
    for (const link of icons) {
      if ("cuaHref" in link.dataset) {
        link.setAttribute("href", link.dataset.cuaHref);
        delete link.dataset.cuaHref;
      }
    }
    doc.getElementById(HOST_ID)?.remove();
    return;
  }

  const existing = doc.getElementById(HOST_ID);
  if (existing?.dataset.color === color) return;
  existing?.remove();
  const host = doc.createElement("div");
  host.id = HOST_ID;
  host.dataset.color = color;
  set(host, { position: "fixed", inset: "0", pointerEvents: "none", zIndex: "2147483647" });
  // Closed: page scripts cannot restyle the indicator or press Stop.
  const root = host.attachShadow({ mode: "closed" });

  const glow = doc.createElement("div");
  set(glow, {
    position: "absolute",
    inset: "0",
    boxShadow: `inset 0 0 0 3px ${color}, inset 0 0 28px ${color}8c`,
  });

  const pill = doc.createElement("div");
  set(pill, {
    position: "absolute",
    left: "50%",
    bottom: "16px",
    transform: "translateX(-50%)",
    display: "flex",
    alignItems: "center",
    gap: "8px",
    padding: "6px 6px 6px 12px",
    borderRadius: "999px",
    background: "#0E1116",
    color: "#ffffff",
    font: "500 13px/1.2 -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
    boxShadow: "0 4px 18px rgba(0, 0, 0, 0.35)",
    pointerEvents: "auto",
    whiteSpace: "nowrap",
  });

  const svgNS = "http://www.w3.org/2000/svg";
  const icon = doc.createElementNS(svgNS, "svg");
  icon.setAttribute("width", "16");
  icon.setAttribute("height", "16");
  icon.setAttribute("viewBox", "30 20 88 88");
  const path = doc.createElementNS(svgNS, "path");
  path.setAttribute("d",
    "M55,30 C48,28 42,33 43,41 C43,41 64,98 64,98 C67,106 73,106 77,99 C77,99 86,79 86,79 " +
    "C88,75 91,72 95,70 C95,70 108,63 108,63 C115,59 114,53 107,50 C107,50 55,30 55,30 Z");
  path.setAttribute("fill", color);
  path.setAttribute("stroke", "#ffffff");
  path.setAttribute("stroke-width", "7");
  path.setAttribute("stroke-linejoin", "round");
  icon.appendChild(path);

  const label = doc.createElement("span");
  label.textContent = "Cua is working in this tab";

  const stop = doc.createElement("button");
  stop.textContent = "Stop";
  set(stop, {
    border: "0",
    borderRadius: "999px",
    padding: "4px 12px",
    background: "#ffffff",
    color: "#0E1116",
    font: "600 12px/1.2 -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
    cursor: "pointer",
  });
  stop.addEventListener("click", () => {
    stop.disabled = true;
    stop.textContent = "Stopping";
    chrome.runtime.sendMessage({ type: "cua-stop" });
  });

  pill.append(icon, label, stop);
  root.append(glow, pill);
  (doc.body ?? doc.documentElement).appendChild(host);
}
