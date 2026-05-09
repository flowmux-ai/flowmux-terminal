
<div align="center">
  
# FlowMux
![icon](resources/icons/flowmux-180.png)

**Agent Workflow Multiplexer Terminal** — *Go with the agents' flow.*

<img src=resources/screenshot/screenshot_1.png  width="850"/>

</div>

### A terminal for AI agent workflows, browser control, and task signals.

flowmux is a Linux/GTK4 terminal with tabs and notifications for AI coding agents.

> It is an unofficial GPL-3.0-or-later reimplementation inspired by [cmux](https://cmux.com/ko), a macOS/AppKit app, and is not affiliated with cmux.
  
## Control internal browser

flowmux ships a WebKitGTK 6.0 browser tab that lives next to terminal tabs in
the same pane tree. The clip below shows an AI agent driving the page through
flowmux's IPC socket — snapshot the DOM, click, type, read state back — with
no system Chromium and no separate driver process.

![video](resources/screenshot/video_control_browser.gif)

## AI Agent notification(Claude, Codex, OpenCode)

flowmux installs lifecycle hooks into Claude Code, Codex, and OpenCode so
events like *task complete*, *needs approval*, and *error* surface as native
desktop notifications. Each notification is routed to the workspace that
fired it, suppressed while its surface is focused, and isolated per flowmux
window so multiple sessions don't bleed into each other.

![video2](resources/screenshot/claude_notification.gif)

## Features

### Workspaces & panes
- Each side-panel item is a workspace with its own recursive pane tree —
  split a pane horizontally or vertically and mix terminal tabs with
  browser tabs in the same pane.
- `Alt` + arrow keys navigate between sibling panes, scoped to the active
  workspace; closing a pane preserves the keyboard focus and the other
  panes' PTY children.

### In-app browser
- WebKitGTK 6.0 surface that sits in the pane tree as a *browser tab*,
  not a popup window.
- Scriptable from the CLI or IPC: `browser snapshot` returns an a11y
  tree with stable `eN` ref tokens; `click` / `fill` / `select` / `type`
  / `press` / `scroll`, plus `is-visible` / `is-enabled` / `is-checked`
  / `count` / `text` / `value` / `attr` operate on those refs.
- `import-cookies --from {firefox,chrome,chromium,brave,edge,arc}` pulls
  the host browser's session into the in-app jar (libsecret + sqlite).
- An `AGENTS.md` ships at the repo root so AI agents that follow the
  AGENTS.md convention prefer the in-app browser over Playwright,
  Puppeteer, or a system Chromium.

### Notifications
- OSC 9 / OSC 99 / OSC 777 detection in any PTY, plus a streaming
  variant (`flowmux notify-stream`) for piping log files through.
- Desktop delivery via `org.freedesktop.Notifications` over zbus.
- Routing is surface-aware: a notification fired by the currently
  focused surface is suppressed, and clicking the bell popover jumps
  back to the source pane.
- Workspaces tint when they have unread attention and clear the tint
  when activated, whether by click or programmatically.

### AI agent integration
- `flowmux hooks setup` registers Claude Code, Codex, and OpenCode
  lifecycle hooks so `Stop` / `Notification` / `SessionStart` /
  `SessionEnd` / `PreToolUse` / `PromptSubmit` events flow into the
  flowmux notification system.
- `flowmux notify-complete --agent <Claude|Codex|OpenCode>` is a
  one-liner helper for hook scripts; it falls back to `FLOWMUX_PANE_ID`
  so it works without flags.
- Agent session ids are persisted with workspace state, so a workspace
  restored at startup still resolves its agent panes.
- `flowmux claude-teams --count N` opens a workspace pre-split into N
  panes, each running `claude`, mirroring cmux's `claude-teams`.
- `flowmux agent install` mirrors the bundled flowmux-browser SKILL
  into each agent's user-level skills directory; `flowmux agent doctor`
  reports drift.

### CLI surface
- `flowmuxctl` is the underlying CLI; the `flowmux` GUI binary forwards
  subcommands to it, so `flowmux browser open ...` and `flowmuxctl
  browser open ...` are equivalent.
- All target arguments accept a bare uuid, `pane:<uuid>`, or
  `surface:<uuid>`; `--json` emits machine-readable output for scripts
  and agents.
- Coverage: workspace, split, send-keys, browser (full automation
  surface), ssh, notify / notify-complete / notify-stream, theme,
  hooks, agent.

### Config & state
- `cmux.json` is read unchanged for custom commands.
- Workspace / pane / surface state lives under
  `$XDG_DATA_HOME/flowmux/` and survives restarts.
- Themes load from `$XDG_CONFIG_HOME/flowmux/theme`; `flowmux theme
  import <file>` copies one in.

## Layout

```
flowmux/
├── crates/
│   ├── flowmux-core/       Domain types: Workspace, Surface, Pane, Notification
│   ├── flowmux-config/     cmux.json + ~/.config/ghostty/config readers
│   ├── flowmux-state/      Persistent workspace/session state on disk
│   ├── flowmux-terminal/   Terminal backend trait + vte4 / libghostty backends
│   ├── flowmux-browser/    WebKitGTK 6.0 browser surface + scriptable refs
│   ├── flowmux-cookies/    Browser cookie/session import (libsecret + sqlite)
│   ├── flowmux-notify/     OSC 9/99/777 parser + libnotify D-Bus sender
│   ├── flowmux-ipc/        Unix-socket IPC (cmux socket-API compatible)
│   ├── flowmux-daemon/     Background daemon orchestrating IPC and panes
│   ├── flowmux-procmon/    PID-tree process / listening-port monitor
│   ├── flowmux-ssh/        SSH workspaces via russh
│   ├── flowmux-vcs/        Git/PR sidebar integration
│   ├── flowmux-cli/        `flowmuxctl` helper for CLI subcommands
│   └── flowmux/            GTK4 + libadwaita main app and public `flowmux` binary
├── packaging/{debian,flatpak}/  Distro packaging metadata
├── resources/             .desktop file, icons, screenshots, themes
├── LICENSE                GPL-3.0-or-later (verbatim from gnu.org)
├── THIRD_PARTY_LICENSES.md  Third-party dependency license inventory
└── NOTICE                 Copyright + attribution
```

## Build prerequisites (Ubuntu 24.04+)

```bash
sudo apt install \
    build-essential pkg-config \
    libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev \
    libwebkitgtk-6.0-dev libssl-dev \
    libssh2-1-dev libdbus-1-dev
# rustup (stable toolchain)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Recommended (optional) — full media playback in tab browser

WebKitGTK delegates media decoding to GStreamer. Without these
plugins the tab browser still loads pages, but YouTube / Twitch /
HTML5 `<video>` may stall, miss subtitles, or fail on
encrypted/DRM content. Install them if you plan to play video:

```bash
sudo apt install \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav
```

The exact symptoms when missing: log lines like
`GStreamer element fakevideosink not found` or
`WebKit wasn't able to find a WebVTT encoder. Subtitles handling
will be degraded unless gst-plugins-bad is installed.`

## Build

```bash
# release build of the GUI app and the CLI helper
cargo build --release -p flowmux -p flowmux-cli
```

The release profile produces two binaries under `target/release/`:

- `flowmux` — the GTK4 GUI; also forwards CLI subcommands to `flowmuxctl`.
- `flowmuxctl` — the CLI helper invoked by the GUI binary and by agent hooks.

For day-to-day development you can skip the install step:

```bash
cargo run -p flowmux           # debug GUI
cargo check --workspace        # type-check everything
```

## Install (user-local)

The recommended single-user layout puts `flowmux` on `PATH` and tucks the
CLI helper next to it where the GUI binary will find it:

```bash
# main binary on PATH
install -Dm755 target/release/flowmux ~/.local/bin/flowmux

# CLI helper — flowmux resolves it as <prefix>/lib/flowmux/flowmuxctl
install -Dm755 target/release/flowmuxctl ~/.local/lib/flowmux/flowmuxctl

# .desktop entry so GNOME / KDE menus see flowmux
install -Dm644 resources/desktop/com.flowmux.App.desktop \
    ~/.local/share/applications/com.flowmux.App.desktop
update-desktop-database ~/.local/share/applications/ 2>/dev/null || true

# wire AI agent lifecycle hooks (Claude Code, Codex, OpenCode)
flowmux hooks setup
flowmux hooks doctor      # verify per-agent registration
```

To uninstall, run `flowmux hooks uninstall` to clean up the agent configs,
then remove the four files installed above.

### Distro packaging

Stub manifests for downstream packagers live under
`packaging/debian/cargo-deb.toml` and
`packaging/flatpak/com.flowmux.App.yml`. They are not yet the primary
install path — prefer the user-local layout above.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions are accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
