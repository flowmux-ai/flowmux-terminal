<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Configuration reference

The main file is `$XDG_CONFIG_HOME/flowmux/options.json` (normally
`~/.config/flowmux/options.json`). All fields are optional; omitted values use
the built-in defaults.

## options.json

`zoom_percent`, `default_browser_engine`, `focus_border_color`,
`focus_border_opacity`, `persist_browser_session`, `auto_resume_agent_sessions`,
`restore_terminal_scrollback`, `scrollback_lines`, `default_shell`,
`system_notifications_enabled`, `agent_bar_enabled`, `cursor_blink`,
`cursor_blink_interval_ms`, `font_family`, `font_size`,
`agent_notification_target`, `theme`, `theme_overrides`, and `keybindings`.
`default_shell` selects the command for new tabs; a per-tab IPC `shell` takes
precedence, then `$SHELL` is used. Invalid commands fall back safely.

## Ghostty configuration

When present, `~/.config/ghostty/config` supplies `font-family`, `font-size`,
`theme`, `background`, `foreground`, `cursor-color`, `selection-background`,
`selection-foreground`, and `palette = N=#rrggbb`. Unknown keys are retained
for diagnostics but do not alter flowmux behavior.

## cmux.json

Project-local `cmux.json` supports `name`, `env`, and `commands`. Each command
has `id`, `label`, `run`, optional `cwd`, `target` (`focused_pane`,
`split_down`, `split_right`, or `new_surface`), and `confirm`.

## State and environment

`state.json` under `$XDG_STATE_HOME/flowmux` is managed by flowmux and is not a
user-editable configuration file. Runtime context variables include
`FLOWMUX_PANE_ID`, `FLOWMUX_SURFACE_ID`, `FLOWMUX_WORKSPACE_ID`,
`FLOWMUX_TAB_ID`, `FLOWMUX_SOCKET_PATH`, and optional
`FLOWMUX_BUNDLED_CLI_PATH`. `FLOWMUX_RUNTIME_DIR` can isolate a smoke run;
`FLOWMUX_LOG` selects a log file. `NO_COLOR=1` disables CLI colour.
