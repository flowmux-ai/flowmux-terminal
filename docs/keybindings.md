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

The terminal IME and scroll workarounds are intentionally fixed: Shift+Enter
flushes composed Hangul input, and PgUp/PgDn use smart scrollback behavior.
They are not editable keybindings.

See the [configuration reference](configuration.md) for the JSON shape.
