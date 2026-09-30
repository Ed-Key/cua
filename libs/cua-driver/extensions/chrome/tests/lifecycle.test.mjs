// Run with: node --test libs/cua-driver/extensions/chrome/tests/lifecycle.test.mjs
import assert from "node:assert/strict";
import test from "node:test";
import { IDLE_BACKSTOP_MS, attachOutlivedConnection, idleTabs } from "../lifecycle.js";

test("the backstop is ten minutes, far above a normal pause between commands", () => {
  assert.equal(IDLE_BACKSTOP_MS, 600_000);
  // chrome_banner.py keeps the banner through 33 s of idle.
  assert.deepEqual(idleTabs(new Map([[1, 0]]), 33_000), []);
});

test("only tabs idle for the whole backstop are released", () => {
  const now = 1_000_000;
  const lastCommandAt = new Map([
    [1, now - IDLE_BACKSTOP_MS],      // exactly at the limit
    [2, now - IDLE_BACKSTOP_MS + 1],  // one millisecond short
    [3, now - 2 * IDLE_BACKSTOP_MS],
    [4, now],
  ]);
  assert.deepEqual(idleTabs(lastCommandAt, now), [1, 3]);
  assert.deepEqual(idleTabs(new Map(), now), []);
});

test("an attach that finishes under a different connection is released", () => {
  assert.equal(attachOutlivedConnection(3, 3), false);
  assert.equal(attachOutlivedConnection(3, 4), true);
});
