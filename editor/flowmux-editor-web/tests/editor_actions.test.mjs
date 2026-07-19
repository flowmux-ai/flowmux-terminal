// SPDX-License-Identifier: GPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import { EDITOR_ACTION_SPECS, monacoKeybinding } from "../.test-build/editor_actions.js";

function spec(id) {
  const found = EDITOR_ACTION_SPECS.find((candidate) => candidate.id === id);
  assert.notEqual(found, undefined, `missing editor action: ${id}`);
  return found;
}

test("find, replace, and save are bound to Ctrl+F, Ctrl+H, and Ctrl+S on Linux", () => {
  // `ctrlCmd` is Monaco's platform modifier: Ctrl on Linux/Windows, Cmd on macOS.
  assert.deepEqual(spec("flowmux.find").chord, { ctrlCmd: true, key: "KeyF" });
  assert.deepEqual(spec("flowmux.replace").chord, { ctrlCmd: true, key: "KeyH" });
  assert.deepEqual(spec("flowmux.save").chord, { ctrlCmd: true, key: "KeyS" });
  assert.equal(spec("flowmux.find").run, "find");
  assert.equal(spec("flowmux.replace").run, "replace");
  assert.equal(spec("flowmux.save").run, "save");
});

test("core editing shortcuts cover save as, save all, quick open, workspace search, and close", () => {
  assert.deepEqual(spec("flowmux.saveAs").chord, { ctrlCmd: true, shift: true, key: "KeyS" });
  assert.deepEqual(spec("flowmux.saveAll").chord, { ctrlCmd: true, alt: true, key: "KeyS" });
  assert.deepEqual(spec("flowmux.quickOpen").chord, { ctrlCmd: true, key: "KeyP" });
  assert.deepEqual(spec("flowmux.workspaceSearch").chord, {
    ctrlCmd: true,
    shift: true,
    key: "KeyF",
  });
  assert.deepEqual(spec("flowmux.closeDocument").chord, { ctrlCmd: true, key: "KeyW" });
});

test("action ids are unique and namespaced", () => {
  const ids = EDITOR_ACTION_SPECS.map((candidate) => candidate.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const id of ids) {
    assert.match(id, /^flowmux\./);
  }
});

test("no two actions claim the same chord", () => {
  const chords = EDITOR_ACTION_SPECS.filter((candidate) => candidate.chord !== null).map(
    (candidate) =>
      JSON.stringify([
        Boolean(candidate.chord.ctrlCmd),
        Boolean(candidate.chord.shift),
        Boolean(candidate.chord.alt),
        candidate.chord.key,
      ]),
  );
  assert.equal(new Set(chords).size, chords.length);
});

test("every runner name is one of the wired handlers", () => {
  const runners = new Set([
    "save",
    "saveAs",
    "saveAll",
    "find",
    "replace",
    "quickOpen",
    "workspaceSearch",
    "closeDocument",
    "toggleWordWrap",
    "toggleMinimap",
    "increaseFontSize",
    "decreaseFontSize",
    "resetFontSize",
  ]);
  for (const candidate of EDITOR_ACTION_SPECS) {
    assert.equal(runners.has(candidate.run), true, `unknown runner: ${candidate.run}`);
  }
});

test("no action shadows the native editing chords", () => {
  // Copy/cut/paste, select all, and undo/redo are handled natively by
  // Monaco and the WebView (Ctrl+C/X/V/A/Z/Y on Linux). A flowmux action on
  // any of these chords would override that built-in behavior.
  const nativeKeys = new Set(["KeyC", "KeyX", "KeyV", "KeyA", "KeyZ", "KeyY"]);
  for (const candidate of EDITOR_ACTION_SPECS) {
    if (candidate.chord === null) {
      continue;
    }
    const { ctrlCmd, shift, alt, key } = candidate.chord;
    const plainCtrlCmd = Boolean(ctrlCmd) && !shift && !alt;
    assert.equal(
      plainCtrlCmd && nativeKeys.has(key),
      false,
      `${candidate.id} would shadow a native editing shortcut`,
    );
  }
});

test("chords encode into Monaco keybinding numbers", () => {
  const keyMod = { CtrlCmd: 2048, Shift: 1024, Alt: 512 };
  const keyCode = { KeyF: 36, KeyS: 49, KeyH: 38 };
  assert.equal(
    monacoKeybinding({ ctrlCmd: true, key: "KeyF" }, keyMod, keyCode),
    2048 | 36,
  );
  assert.equal(
    monacoKeybinding({ ctrlCmd: true, shift: true, key: "KeyS" }, keyMod, keyCode),
    2048 | 1024 | 49,
  );
  assert.equal(
    monacoKeybinding({ alt: true, key: "KeyH" }, keyMod, keyCode),
    512 | 38,
  );
  assert.throws(() => monacoKeybinding({ ctrlCmd: true, key: "Nope" }, keyMod, keyCode));
});
