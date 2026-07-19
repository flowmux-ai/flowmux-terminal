// SPDX-License-Identifier: GPL-3.0-or-later

/**
 * Declarative table of the editor's local shortcuts, kept free of Monaco
 * imports so the chord assignments are testable in Node. `ctrlCmd` is
 * Monaco's platform modifier: Ctrl on Linux/Windows, Cmd on macOS — so
 * `ctrlCmd + KeyF` is Ubuntu's Ctrl+F.
 */
export interface EditorActionChord {
  ctrlCmd?: boolean;
  shift?: boolean;
  alt?: boolean;
  key: string;
}

export type EditorActionRunner =
  | "save"
  | "saveAs"
  | "saveAll"
  | "find"
  | "replace"
  | "quickOpen"
  | "workspaceSearch"
  | "closeDocument"
  | "toggleWordWrap"
  | "toggleMinimap"
  | "increaseFontSize"
  | "decreaseFontSize"
  | "resetFontSize";

export interface EditorActionSpec {
  id: string;
  label: string;
  chord: EditorActionChord | null;
  run: EditorActionRunner;
}

export const EDITOR_ACTION_SPECS: readonly EditorActionSpec[] = [
  { id: "flowmux.save", label: "Save", chord: { ctrlCmd: true, key: "KeyS" }, run: "save" },
  {
    id: "flowmux.saveAs",
    label: "Save As",
    chord: { ctrlCmd: true, shift: true, key: "KeyS" },
    run: "saveAs",
  },
  {
    id: "flowmux.saveAll",
    label: "Save All",
    chord: { ctrlCmd: true, alt: true, key: "KeyS" },
    run: "saveAll",
  },
  { id: "flowmux.find", label: "Find", chord: { ctrlCmd: true, key: "KeyF" }, run: "find" },
  {
    id: "flowmux.replace",
    label: "Replace",
    chord: { ctrlCmd: true, key: "KeyH" },
    run: "replace",
  },
  {
    id: "flowmux.quickOpen",
    label: "Quick Open",
    chord: { ctrlCmd: true, key: "KeyP" },
    run: "quickOpen",
  },
  {
    id: "flowmux.workspaceSearch",
    label: "Find in Workspace",
    chord: { ctrlCmd: true, shift: true, key: "KeyF" },
    run: "workspaceSearch",
  },
  {
    id: "flowmux.closeDocument",
    label: "Close Current Document",
    chord: { ctrlCmd: true, key: "KeyW" },
    run: "closeDocument",
  },
  {
    id: "flowmux.toggleWordWrap",
    label: "Toggle Word Wrap",
    chord: { alt: true, key: "KeyZ" },
    run: "toggleWordWrap",
  },
  {
    id: "flowmux.toggleMinimap",
    label: "Toggle Minimap",
    chord: null,
    run: "toggleMinimap",
  },
  {
    id: "flowmux.increaseFontSize",
    label: "Increase Editor Font Size",
    chord: { ctrlCmd: true, key: "Equal" },
    run: "increaseFontSize",
  },
  {
    id: "flowmux.decreaseFontSize",
    label: "Decrease Editor Font Size",
    chord: { ctrlCmd: true, key: "Minus" },
    run: "decreaseFontSize",
  },
  {
    id: "flowmux.resetFontSize",
    label: "Reset Editor Font Size",
    chord: { ctrlCmd: true, key: "Digit0" },
    run: "resetFontSize",
  },
];

export interface MonacoKeyMod {
  CtrlCmd: number;
  Shift: number;
  Alt: number;
}

/** Encode a chord as a Monaco keybinding number. */
export function monacoKeybinding(
  chord: EditorActionChord,
  keyMod: MonacoKeyMod,
  keyCode: Record<string, number>,
): number {
  const code = keyCode[chord.key];
  if (code === undefined) {
    throw new Error(`unknown Monaco key code: ${chord.key}`);
  }
  return (
    (chord.ctrlCmd ? keyMod.CtrlCmd : 0) |
    (chord.shift ? keyMod.Shift : 0) |
    (chord.alt ? keyMod.Alt : 0) |
    code
  );
}
