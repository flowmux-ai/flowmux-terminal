// SPDX-License-Identifier: GPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { adjustedFontSize, movedTabIndex } from "../.test-build/editor_state.js";

test("document tab navigation wraps and handles edge positions", () => {
  assert.equal(movedTabIndex(0, 3, "previous"), 2);
  assert.equal(movedTabIndex(2, 3, "next"), 0);
  assert.equal(movedTabIndex(1, 3, "first"), 0);
  assert.equal(movedTabIndex(1, 3, "last"), 2);
  assert.equal(movedTabIndex(0, 0, "next"), null);
});

test("font zoom stays inside a readable supported range", () => {
  assert.equal(adjustedFontSize(13, 1), 14);
  assert.equal(adjustedFontSize(32, 1), 32);
  assert.equal(adjustedFontSize(10, -1), 10);
});
