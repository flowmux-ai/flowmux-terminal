
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
- Side-panel workspaces keep several tasks side by side, and each one
  can be split into as many panes as you need.
- Terminal tabs and browser tabs share the same pane tree, and you can
  jump between panes from the keyboard.

### In-app browser
- A browser tab lives inside flowmux next to your terminals — no need
  to open a separate Chromium just to view or interact with a page.
- AI agents in a neighbouring pane can drive that browser directly:
  snapshot the page, click, type, scroll, and read state back.
- Import an existing session from Firefox, Chrome, Chromium, Brave,
  Edge, or Arc so you stay logged in to the sites you already use.

### Notifications
- "Task complete" and "needs attention" signals from a terminal turn
  into native desktop notifications.
- Each notification is routed to the workspace that fired it, stays
  quiet while you are already looking at that pane, and the sidebar
  highlights workspaces that need your attention.

### AI agent integration
- Claude Code, Codex, and OpenCode are wired up out of the box, so
  completion, approval, and error events surface as notifications you
  actually see.
- Agent sessions are remembered across restarts, so a resumed
  workspace lands back on the right pane.
- `claude-teams` opens a workspace pre-split into several panes, each
  running its own Claude instance.
- `flowmux doctor` shows whether each agent is wired correctly and
  `flowmux fix` re-installs the pieces that are missing — handy after
  installing an agent that wasn't on the host when flowmux was first
  set up.


## Layout

```
flowmux/
├── crates/
│   ├── flowmux-core/       Domain types: Workspace, Surface, Pane, Notification
│   ├── flowmux-config/     cmux.json + ~/.config/ghostty/config readers
│   ├── flowmux-state/      Persistent workspace/session state on disk
│   ├── flowmux-terminal/   libghostty-first terminal backend trait + VTE compatibility
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

### Ubuntu 22.04 (jammy) — install via Flatpak

> **Heads-up:** Ubuntu < 24.04 is **not recommended**. The Flatpak
> path below works as a fallback, but because the terminal pane runs
> the sandbox's own shell, host-installed tools (`git`, `tig`, `vim`,
> `htop`, …) are **not visible** from inside the pane — only what
> ships in the GNOME Platform runtime is reachable. Attempts to
> escape the sandbox via `flatpak-spawn --host` fail to inherit a
> controlling terminal cleanly (kernel rejects `TIOCSCTTY` on the
> forwarded PTY), so this is a fundamental Flatpak limitation rather
> than a configuration issue. If you need full host-tool access,
> upgrade to Ubuntu 24.04+ and use the native apt build above.

The native apt build above needs GTK 4.12+ and `libwebkitgtk-6.0`,
neither of which is in the 22.04 archive. On 22.04 the supported path
is Flatpak: the GNOME 48 runtime brings GTK 4.18, libadwaita 1.8,
libvte 0.78, and WebKitGTK 6.0 into the sandbox without touching the
host system, so the same flowmux build runs unchanged. The matching
`rust-stable//24.08` SDK extension ships a current Rust toolchain so
the workspace's crate-level edition requirements are met.

```bash
# 1. Install Flatpak, flatpak-builder, and the Flathub remote
sudo apt install flatpak flatpak-builder
flatpak remote-add --if-not-exists --user flathub \
    https://flathub.org/repo/flathub.flatpakrepo

# 2. Install the GNOME 48 runtime/SDK and the Rust SDK extension
flatpak install -y --user flathub \
    org.gnome.Platform//48 org.gnome.Sdk//48 \
    org.freedesktop.Sdk.Extension.rust-stable//24.08

# 3. Build and install flowmux from this repo (per-user, no sudo)
flatpak-builder --user --install --force-clean \
    build-flatpak packaging/flatpak/com.flowmux.App.yml

# 4. Launch
flatpak run com.flowmux.App
```

Notes for the Flatpak build:

- GStreamer plugins (see next section) are already bundled in the
  GNOME runtime, so you do not need to install them separately on the
  host for the Flatpak build to play media in the tab browser.
- The `flowmux` and `flowmuxctl` binaries inside the sandbox are
  reachable from a host terminal as `flatpak run --command=flowmux
  com.flowmux.App ...` and `flatpak run --command=flowmuxctl
  com.flowmux.App ...`. The in-app browser CLI flow described in
  [`AGENTS.md`](AGENTS.md) works the same way.
- After upgrading the repo, re-run step 3 to rebuild against the new
  source. The `--force-clean` flag wipes the previous build tree.

If browser tabs open but render as a blank page (WebKit's web process
aborts with `Could not create default EGL display: EGL_BAD_PARAMETER`),
the host's GL stack is too old for the newer Mesa inside the Flatpak
sandbox. Disable WebKit's GPU path with the `FLOWMUX_WEBKIT_HW_ACCEL`
environment variable — set it once and it sticks across launches:

```bash
flatpak override --user --env=FLOWMUX_WEBKIT_HW_ACCEL=never com.flowmux.App
flatpak run com.flowmux.App
```

Pages then render via CPU rasterisation. Hardware video decoding is
lost, but the browser pane works.

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
cargo build --release -p flowmux -p flowmux-cli -p flowmux-core -p flowmux-daemon
```

The release profile produces two binaries under `target/release/`:

- `flowmux` — the GTK4 GUI; also forwards CLI subcommands to `flowmuxctl`.
- `flowmuxctl` — the CLI helper invoked by the GUI binary and by agent hooks.

For day-to-day development you can skip the install step:

```bash
cargo run -p flowmux           # debug GUI
cargo check --workspace        # type-check everything
```

## Verify & repair

flowmux integrates with several things on your host — AI-agent SKILL
files, agent lifecycle hooks, the in-app browser data dir, host
browsers visible to the cookie importer, and the flowmux daemon
socket. Two commands keep the whole picture in one place:

```bash
flowmux doctor   # read-only audit; exits non-zero if anything needs fixing
flowmux fix      # re-install / refresh anything `doctor` flagged
```

`doctor` prints one row per check with a coloured status badge — green
`ok`, red `fix`, yellow `warn`, plain `info`. Pipe it (`flowmux
doctor | …`) or set `NO_COLOR=1` to disable colour for log files and
CI. Run it whenever you want to know "is everything wired?":

- **after `flowmux` install or upgrade** — the SKILL/hook payloads
  ship inside the binary, so a fresh build may need to re-sync the
  on-disk copies.
- **after installing Claude Code, Codex, or OpenCode for the first
  time** — agents installed *after* flowmux are detected on the next
  `doctor` run; `fix` then wires them up.
- **when the bell popover or desktop notifications stop arriving**
  from one agent — a row tagged `fix` (missing/drifted hook entry) is
  almost always the cause.

`fix` is idempotent: rows that are already `ok` are no-ops, agents
whose home directory is missing are skipped, and re-running it never
clobbers a hand-edited entry that doesn't carry the flowmux marker.
Add `--json` to either command for machine-readable output.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions are accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
