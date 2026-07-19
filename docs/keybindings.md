<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Keyboard shortcuts

Shortcuts are the built-in defaults from `flowmux-config` and can be changed
under Options → Keybindings. Action names are the keys used in
`options.json`. Linux and macOS use the platform-specific accelerators below.

| ActionId | Linux | macOS |
|---|---|---|
| split-right / split-down | Ctrl+Shift+PageUp / PageDown | Cmd+Shift+PageUp / PageDown |
| focus-left/right/up/down | Alt+Arrow | Alt+Arrow |
| close-surface / quit-app | Alt+W / Ctrl+Shift+W | Alt+W / Cmd+Shift+W |
| next/prev-surface | Ctrl+Shift+Right / Left | Cmd+Shift+Right / Left |
| next/prev-workspace | Ctrl+Tab / Ctrl+Shift+Tab | same |
| workspace-1 … workspace-8 | Alt+1 … Alt+8 | same |
| copy / paste | Ctrl+Shift+C / Ctrl+Shift+V | Cmd+C / Cmd+V |
| new-surface / new-browser-surface | Ctrl+Shift+T / B | Cmd+Shift+T / B |
| new-workspace / new-window | Ctrl+N / Ctrl+Shift+N | Cmd+N / Cmd+Shift+N |
| command-palette | Ctrl+Shift+P | Cmd+Shift+P |
| terminal-search | Ctrl+Shift+F | Ctrl+Shift+F |
| toggle-pane-zoom | Ctrl+Alt+Z | Ctrl+Alt+Z |
| copy-pane-path | Ctrl+Shift+K | Cmd+Shift+K |
| toggle-worktree-panel / toggle-file-browser | Ctrl+Alt+W / Ctrl+Alt+F | Cmd+Alt+W / Cmd+Alt+F |
| toggle-usage-popover | Ctrl+Alt+U | Cmd+Alt+U |
| open-tig | Ctrl+Alt+G | Cmd+Alt+G |

The terminal IME and scroll workarounds are intentionally fixed: Shift+Enter
flushes composed Hangul input, and PgUp/PgDn use smart scrollback behavior.
They are not editable keybindings.

When the embedded editor has focus, it also provides its local editing
shortcuts: Ctrl/Cmd+S saves, Ctrl/Cmd+Shift+S opens Save As,
Ctrl/Cmd+Alt+S saves all, Ctrl/Cmd+F and Ctrl/Cmd+H find and replace,
Ctrl/Cmd+P opens a file, Ctrl/Cmd+Shift+F searches the workspace, and
Ctrl/Cmd+W closes the current document. Standard editing chords —
Ctrl/Cmd+C/X/V copy/cut/paste, Ctrl/Cmd+A select all, Ctrl/Cmd+Z undo —
work natively inside the editor, and the terminal-style Ctrl+Shift+C/V
copy/paste also routes to the focused editor. Flowmux surface shortcuts
such as Alt+W remain global.

## Context behavior

| Input | Terminal, browser, or Files focused | Embedded editor focused |
|---|---|---|
| Flowmux layout, tab, workspace, panel, window, zoom, palette, and tig shortcuts listed above | Runs the flowmux action | Runs the same flowmux action |
| copy / paste | Targets the focused terminal or file view | Targets the editor selection or cursor |
| terminal-search | Searches the focused terminal or browser | Searches the editor workspace |
| Alt+Arrow | Moves focus between panes | Moves focus between panes |
| Arrow, PageUp/PageDown, Home/End | Remains local to the focused surface | Moves the editor cursor or viewport; Shift extends the selection |
| Text input, Enter, Tab, Backspace, Delete, and IME composition | Remains local to the focused surface | Remains entirely inside the editor |

The editor only overrides local editing input. Modifier chords assigned to a
flowmux action are handled at the window level in both contexts, so focusing an
editor does not disable pane, tab, workspace, or panel control.

See the [configuration reference](configuration.md) for the JSON shape.
