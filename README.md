# flowmux

<img width="1518" height="837" alt="image" src="https://github.com/user-attachments/assets/ccc5992e-4b6c-44a5-b4b1-ea3cc4c7c42c" />

A Linux/GTK4 terminal with vertical tabs and notifications for AI coding agents.

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

Then:

```bash
cargo check --workspace
cargo run -p flowmux-app
```

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
Contributions are accepted under the same license; see
[`CONTRIBUTING.md`](CONTRIBUTING.md).
