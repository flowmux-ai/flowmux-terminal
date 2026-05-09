
<div align="center">
  
# FlowMux
![icon](resources/icons/flowmux-180.png)

**Agent Workflow Multiplexer Terminal** — *Go with the agents' flow.*

<img src=resources/screenshot/screenshot_1.png  width="850"/>

</div>

### A cmux-inspired terminal for AI agent workflows, browser control, and task signals.

flowmux is a Linux/GTK4 terminal with vertical tabs and notifications for AI coding agents.

flowmux is an unofficial GPL-3.0-or-later Linux reimplementation inspired by
[cmux](https://github.com/manaflow-ai/cmux), a macOS/AppKit app. It is not
affiliated with or endorsed by Manaflow. The macOS-specific layers (AppKit,
Sparkle, WebKit, UserNotifications) are replaced with Linux desktop
counterparts (GTK4/libadwaita, libnotify/D-Bus, WebKitGTK, vte4 first with
libghostty planned).

> **Status**: pre-alpha. Workspaces, recursive pane splits, vte4 terminals,
> WebKitGTK browser tabs, OSC 9/99/777 notifications, the `flowmuxctl` CLI,
> and claude/codex/opencode lifecycle hooks are working end-to-end. SSH
> workspaces, libghostty rendering, and remaining cmux features land
> incrementally.

## Why a separate project

cmux is macOS-only and tightly coupled to AppKit, libghostty's macOS
embedding, Sparkle, and macOS UserNotifications. A single-codebase port
across these layers is not realistic, so flowmux ships as its own crate
workspace that mirrors cmux's domain model and IPC surface so that:

- existing `cmux.json` configs and `cmux <subcommand>` shell scripts
  largely work unchanged on Linux;
- each subsystem (terminal, notifications, IPC, browser, config) is an
  independent crate that can be re-implemented or swapped without
  touching the rest of the app.

## Control internal browser

![video](resources/screenshot/video_control_browser.gif)

## AI Agent notification(Claude, Codex, OpenCode)

![video2](resources/screenshot/claude_notification.gif)

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

Then:

```bash
cargo check --workspace
cargo run -p flowmux
```

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions are accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
