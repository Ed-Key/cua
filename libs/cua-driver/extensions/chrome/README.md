# Cua Driver for Chrome

A Chrome extension that lets the Cua Driver on this Mac work in your own Chrome
profile, the one you are signed in to. Chrome 136 and later refuse a
remote-debugging port on the default profile, so an extension is the way in,
the same approach Claude in Chrome and the Codex extension use.

## Install

1. Register the native messaging host (once per driver install):

   ```bash
   cua-driver chrome-extension install
   ```

2. In Chrome, open `chrome://extensions`, turn on **Developer mode**, click
   **Load unpacked**, and pick this folder. The id must be
   `pojfnghfciahibpbglblhhnhecmplejm`.

The extension connects to the driver's daemon whenever both are running.
After editing these files, press the extension's **Reload** button on
`chrome://extensions`; a Chrome restart alone can keep the old service worker.

## What you see

- Chrome's banner: "Cua Driver started debugging this browser", while Cua is
  attached to a tab. The debugger stays attached for the whole agent session,
  so the banner does not come and go between commands. It is released, and
  the banner cleared, when the session ends (the driver sends a detach once
  no session holds the tab), when the driver disconnects, when the tab closes,
  or when you press **Stop**.
- In the tab Cua is working in: a soft blue glow, a "Cua is working in this
  tab" pill with **Stop**, and the Cua cursor on the tab's favicon.
- Tabs Cua opens start in the background in a cyan "Cua" tab group.

**Stop** (or dismissing Chrome's banner) detaches the debugger and refuses any
further Cua work in that tab. Clicking the Cua Driver toolbar button allows Cua
again in every tab where you pressed Stop.

## Permissions

| Permission | Why |
| --- | --- |
| `debugger` | Read pages and send trusted input to the tab Cua works in |
| `tabs`, `tabGroups` | List and organize tabs (`browser_tabs`) |
| `nativeMessaging` | Talk to the local Cua Driver daemon |
| `scripting`, `<all_urls>` | Draw the "Cua is working" indicator in the page |
| `favicon` | Put the Cua cursor on the tab's own favicon |
| `storage` | Remember Stop across service-worker restarts |
| `alarms` | Reconnect to the daemon after it restarts |

## How it connects

The extension (`background.js`) answers JSON-RPC from the daemon with Chrome's
extension APIs and holds no agent logic. Chrome starts the native host (the
cua-driver binary itself), which pipes Chrome's frames to the daemon's owner-only
bridge socket. The daemon trusts a link only when the operating system shows its
peer is this driver's executable and its parent is the Chrome being bound. For
page work, the daemon's `extension_relay` presents a loopback DevTools endpoint
backed by `chrome.debugger`, so the existing browser engine runs unchanged; it
refuses web-page origins and allows only the existing-profile command set.

Icons are rendered from the Cua agent cursor by `icons/make_icons.py`.
