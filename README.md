# FlowMux

**Agent Workflow Multiplexer** — *Go with the agents' flow.*

<img width="1426" height="926" alt="Screenshot from 2026-05-09 12-06-58" src="https://github.com/user-attachments/assets/5f452957-32c7-4439-aada-06a0e8c6d5c7" />



### A cmux-inspired terminal for AI agent workflows, browser control, and task signals.

flowmux is a Linux/GTK4 terminal with vertical tabs and notifications for AI coding agents.

flowmux is an unofficial GPL-3.0-or-later Linux reimplementation inspired by
[cmux](https://github.com/manaflow-ai/cmux), a macOS/AppKit app. It is not
affiliated with or endorsed by Manaflow. The macOS-specific layers (AppKit,
Sparkle, WebKit, UserNotifications) are replaced with Linux desktop
counterparts (GTK4/libadwaita, libnotify/D-Bus, WebKitGTK, vte4 first with
libghostty planned).

> **Status**: skeleton. The workspace, IPC contract, OSC notification parser,
> and CLI surface are scaffolded. Terminal rendering, browser pane, SSH
> workspaces, and Claude Code Teams integration are tracked in
> [`docs/upstream-mapping/`](docs/upstream-mapping/) and land incrementally.

## Why a separate project

cmux is macOS-only and tightly coupled to AppKit, libghostty's macOS
embedding, Sparkle, and macOS UserNotifications. A single-codebase port
across these layers is not realistic, so flowmux ships as its own crate
workspace that mirrors cmux's domain model and IPC surface so that:

- existing `cmux.json` configs and `cmux <subcommand>` shell scripts
  largely work unchanged on Linux;
- new cmux features are picked up via a documented upstream-tracking
  process (see [`docs/UPSTREAM.md`](docs/UPSTREAM.md));
- each subsystem (terminal, notifications, IPC, browser, config) is an
  independent crate that can be re-implemented or swapped without
  touching the rest of the app.

## Layout

```
flowmux/
├── crates/
│   ├── flowmux-core/       Domain types: Workspace, Surface, Pane, Notification
│   ├── flowmux-config/     cmux.json + ~/.config/ghostty/config readers
│   ├── flowmux-terminal/   Terminal backend trait + vte4 backend (libghostty later)
│   ├── flowmux-notify/     OSC 9/99/777 parser + libnotify D-Bus sender
│   ├── flowmux-ipc/        Unix-socket IPC (cmux socket-API compatible)
│   ├── flowmux-cli/        `flowmux` binary (workspaces, panes, notify, ssh)
│   └── flowmux-app/        GTK4 + libadwaita main app
├── docs/
│   ├── UPSTREAM.md       How we track cmux upstream
│   └── upstream-mapping/ Per-feature spec (cmux behavior → flowmux impl)
├── scripts/sync-upstream.sh
├── packaging/debian/     .deb metadata stub
├── resources/desktop/    .desktop file, icons
├── LICENSE               GPL-3.0-or-later (verbatim from gnu.org)
├── THIRD_PARTY_LICENSES.md  Third-party dependency license inventory
└── NOTICE                Copyright + attribution
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
cargo run -p flowmux-app
```

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions are accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
