
<div align="center">
  
# FlowMux
![icon](resources/icons/flowmux-180.png)

**Agent Workflow Multiplexer Terminal** — *Go with the agents' flow.*

<img src=resources/screenshot/screenshot_1.png  width="850"/>

</div>

### A terminal for AI agent workflows, browser control, and task signals.

flowmux is a Linux/GTK4 terminal for AI coding agents. The terminal pane uses
the system VTE widget for terminal emulation, flowmux-owned PTYs, and GTK integration.

> Unofficial GPL-3.0-or-later reimplementation inspired by [cmux](https://cmux.com/ko), a macOS/AppKit app. Not affiliated with cmux.
  
## Control internal browser

A WebKitGTK 6.0 browser tab lives next to terminal tabs in the same pane tree.
The clip shows an AI agent driving the page over flowmux's IPC socket —
snapshot the DOM, click, type, read state back — with no system Chromium and
no separate driver.

![video](resources/screenshot/video_control_browser.gif)

## AI Agent notification (Claude, Codex, OpenCode)

flowmux installs lifecycle hooks into Claude Code, Codex, and OpenCode so
*task complete*, *needs approval*, and *error* events surface as native
desktop notifications — routed to the workspace that fired them, suppressed
while that surface is focused, and isolated per window.

![video2](resources/screenshot/claude_notification.gif)

## Features

- **Workspaces & panes** — side-panel workspaces hold tasks side by side, each
  split into multiple keyboard-navigable panes mixing terminal and browser
  tabs. `Ctrl+Shift+K` copies the focused cwd; right-click for Copy path / URL.
- **In-app browser** — a WebKitGTK tab next to your terminals, drivable by
  agents in a neighbouring pane (snapshot, click, type, read state). Import a
  session from Firefox / Chrome / Chromium / Brave / Edge / Arc; **Web
  Inspector** opens WebKit dev tools.
- **Notifications** — terminal "task complete" / "needs attention" signals
  become desktop notifications, routed to the firing workspace and quiet while
  focused. Bell popover **All Clear** clears all entries and toasts at once.
- **AI agent integration** — Claude Code, Codex, OpenCode work out of the box;
  sessions persist across restarts. `claude-teams` opens a workspace pre-split
  into per-Claude panes. `flowmux doctor` / `fix` audit and repair wiring.
- **Agent CLI** — scripts and agents drive flowmux over its socket:
  `flowmux browser <op>` (snapshot / click / fill / type / press /
  is-visible / count / …), `flowmux identify` and `capabilities` for context
  discovery, `flowmux tree` to inspect the workspace → pane → tab structure,
  `workspace current|focus`, `focus-pane|close-pane`, `focus-tab|close-tab`,
  `send-keys`, and `read-screen` (terminal buffer dump). Pane args accept
  `pane:<uuid>` or fall back to
  `$FLOWMUX_PANE_ID`; `--json` everywhere. Full contract in
  [`AGENTS.md`](AGENTS.md).
- **Customizable keybindings** — Options → **Keybindings** rebinds any shortcut
  (applies on OK, no restart), saved to
  `$XDG_CONFIG_HOME/flowmux/options.json`. IME/scroll terminal shortcuts
  (Shift+Enter Hangul flush, smart PgUp/PgDn) are fixed and not editable.

## Layout

```
flowmux/
├── crates/
│   ├── flowmux-core/       Domain types: Workspace, Surface, Pane, Notification
│   ├── flowmux-config/     cmux.json + ~/.config/ghostty/config readers
│   ├── flowmux-state/      Persistent workspace/session state on disk
│   ├── flowmux-terminal/   VTE terminal integration + PTY env helpers
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

## Build prerequisites (Ubuntu 24.04 native)

```bash
sudo apt install \
    build-essential pkg-config git \
    libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev \
    libwebkitgtk-6.0-dev libssl-dev \
    libssh2-1-dev libdbus-1-dev libsecret-1-dev
# rustup (Rust 1.93+) required.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Optional — full media playback in tab browser

WebKitGTK decodes media via GStreamer. Without these plugins pages still load,
but YouTube / Twitch / `<video>` may stall, miss subtitles, or fail on DRM:

```bash
sudo apt install \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav
```

## Build

```bash
cargo build --release --workspace
```

Produces two binaries under `target/release/`:

- `flowmux` — GTK4 GUI; also forwards CLI subcommands to `flowmuxctl`.
- `flowmuxctl` — CLI helper invoked by the GUI and by agent hooks.

`flowmux read-screen` (terminal buffer dump) reads the viewport straight from
the VTE terminal buffer, so it works in every build.

For development:

```bash
cargo run -p flowmux           # debug GUI
cargo check --workspace        # type-check everything
scripts/check-ubuntu-compat.sh # Docker smoke check for 22.04/24.04/26.04
```

## macOS local install

The macOS build uses Homebrew GTK / libadwaita and the system
WebKit.framework for the browser pane. It installs a
regular app bundle plus CLI binaries:

```bash
brew install pkg-config gtk4 libadwaita
scripts/install-macos.sh --check
scripts/install-macos.sh
open "$HOME/Applications/FlowMux.app"
```

The script installs `FlowMux.app` under `~/Applications` and copies `flowmux`,
`flowmuxctl`, and `flowmux-md-viewer` to `~/.local/bin`.

### Install to the host

```bash
scripts/install-host.sh        # release-builds flowmux → installs the binaries
```

This installs `flowmux`, `flowmuxctl`, and `flowmux-md-viewer` binaries to
`~/.local/bin` and `~/.cargo/bin`. It is a plain `cargo build --release` using
the system VTE library; no Zig toolchain or vendored terminal backend is
required.

## Verify & repair

flowmux wires into host pieces: agent SKILL files, agent hooks, the browser
data dir, host browsers for the cookie importer, and the daemon socket.

```bash
flowmux doctor   # read-only audit; non-zero exit if anything needs fixing
flowmux fix      # re-install / refresh what doctor flagged
```

`doctor` prints one row per check with a status badge (`ok` / `fix` / `warn` /
`info`); `NO_COLOR=1` or piping disables colour. Run it after a flowmux
install/upgrade and after installing a new agent. `fix` is idempotent and
never clobbers hand-edited entries lacking the flowmux marker. Add `--json` to
either for machine-readable output.

## Ubuntu 22.04 (jammy)

22.04 lacks the GTK/WebKit versions for a native build, so ship via Flatpak
(GNOME 48 runtime). Host tools stay visible through `flatpak-spawn --host`,
and GStreamer plugins are bundled, so no extra host packages are needed.

```bash
sudo apt install flatpak flatpak-builder
flatpak remote-add --if-not-exists --user flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install -y --user flathub org.gnome.Platform//48 org.gnome.Sdk//48
flatpak-builder --user --install --force-clean build-flatpak packaging/flatpak/com.flowmux.App.yml
flatpak run com.flowmux.App
```

Blank browser tabs (`EGL_BAD_PARAMETER`) mean the host GL stack is too old for
the sandbox Mesa — disable WebKit's GPU path:
`flatpak override --user --env=FLOWMUX_WEBKIT_HW_ACCEL=never com.flowmux.App`.

## WSL / WSLg

Ubuntu 24.04 and 26.04 on WSLg follow the native host path above:
install the build prerequisites, run `scripts/install-host.sh`, then start
`flowmux` from the Linux side so GTK connects to WSLg's Wayland display. The
runtime detects WSL and enables the terminal key/resize workarounds that differ
from a regular GNOME session. Ubuntu 22.04 on WSL should use the Flatpak path
from the Jammy section because the native GTK/libadwaita floor is too old.

To verify a real WSLg session end to end on 24.04 or 26.04:

```bash
scripts/check-wslg-runtime.sh
```

That script installs the native host build if needed, launches the GUI through
WSLg, creates a workspace, and verifies `send-keys` / `read-screen` against the
live terminal pane. It uses a temporary `FLOWMUX_RUNTIME_DIR` for the smoke
run while leaving WSLg's `XDG_RUNTIME_DIR` alone, so it will not steal the
socket from an existing flowmux session or hide the Wayland socket. Use
`--no-install` to test an already-installed binary, or `--keep-open` to leave
the smoke GUI running and print the matching env for manual CLI checks.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
