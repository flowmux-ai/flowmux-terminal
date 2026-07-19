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
| toggle-pane-zoom | Ctrl+Shift+Z | Ctrl+Shift+Z |
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

See the [configuration reference](configuration.md) for the JSON shape.
