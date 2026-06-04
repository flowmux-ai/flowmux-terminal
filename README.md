
<div align="center">
  
# FlowMux
![icon](resources/icons/flowmux-180.png)

**Agent Workflow Multiplexer Terminal** — *Go with the agents' flow.*

<img src=resources/screenshot/screenshot_1.png  width="850"/>

</div>

### A terminal for AI agent workflows, browser control, and task signals.

flowmux is a Linux/GTK4 terminal for AI coding agents. The terminal pane uses
the pure-Rust `alacritty_terminal` engine for VT state and scrollback,
flowmux-owned PTYs, and an application-owned GTK4 renderer (no VTE).

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
│   ├── flowmux-terminal/   pure-Rust terminal backend (alacritty_terminal) + PTY env helpers
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
├── packaging/debian/      Distro packaging metadata (cargo-deb)
├── resources/             .desktop file, icons, screenshots, themes
├── LICENSE                GPL-3.0-or-later (verbatim from gnu.org)
├── THIRD_PARTY_LICENSES.md  Third-party dependency license inventory
└── NOTICE                 Copyright + attribution
```

## Build prerequisites (Ubuntu 24.04 native)

```bash
sudo apt install \
    build-essential pkg-config \
    libgtk-4-dev libadwaita-1-dev \
    libwebkitgtk-6.0-dev libssl-dev \
    libssh2-1-dev libdbus-1-dev
# rustup (Rust 1.93+). The terminal is rendered by a pure-Rust backend
# (alacritty_terminal), so there is no VTE, no meson/ninja, and no Zig.
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

For development:

```bash
cargo run -p flowmux           # debug GUI
cargo check --workspace        # type-check everything
```

### Install to the host

```bash
scripts/install-host.sh        # release build → installs to ~/.local/bin + ~/.cargo/bin
```

The terminal is rendered by a pure-Rust backend (the `alacritty_terminal`
engine plus a flowmux-owned GTK4 renderer), so a plain
`cargo build --release --workspace` is a complete build — there is no VTE to
patch and no special link step. Text selection in agent TUIs (Codex, Claude
Code) survives output repaints because the selection lives in the terminal
model, independent of the cells being rewritten.

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

## Ubuntu 22.04 (jammy) — not supported

flowmux needs GTK 4.12+, libadwaita 1.5+, and WebKitGTK **6.0** (the GTK4
WebKit). 22.04 ships GTK 4.6 and has none of these in apt, and there is no
maintained jammy PPA that backports them (WebKitGTK 6.0 in particular is not
realistically available there). The pure-Rust terminal removed the libvte and
Zig requirements, but the GTK4 + WebKitGTK-6.0 requirement remains and that is
what jammy cannot meet.

flowmux previously shipped a Flatpak to cover 22.04; that has been removed, so
**22.04 is no longer a supported target.** Use **Ubuntu 24.04 or newer** (or any
distro whose repos carry GTK 4.12+ and WebKitGTK 6.0 — e.g. recent Fedora,
Arch). Running flowmux on 22.04 would require building GTK 4.12+, libadwaita,
and WebKitGTK 6.0 from source, which is out of scope here.

24.04 and 26.04 have everything in apt — see the prerequisites above; no PPA or
Flatpak is needed.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
