<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / run / test

This is a Rust workspace (edition 2021, rust-toolchain pinned to `stable`,
MSRV 1.93). All crates live under `crates/`. Most day-to-day commands run
from the repo root.

```bash
# Type-check the headless crates (bare `cargo check` uses the workspace
# `default-members`, which excludes the GTK GUI crate so this works before
# `libgtk-4-dev` is installed). NOTE: `default-members` includes
# `flowmux-terminal`, which builds libghostty-vt, so **Zig 0.15.x must be on
# `PATH`** even for this — see "Terminal backend" below.
cargo check

# Type-check the GUI crate too (requires GTK4 + libadwaita + WebKitGTK
# 6.0 dev packages — see README "Build prerequisites" — plus Zig on PATH).
cargo check -p flowmux

# Release build of all binaries (`flowmux`, `flowmuxctl`).
cargo build --release --workspace

# Debug GUI.
cargo run -p flowmux

# Full test suite. Several crates open GTK/D-Bus, so locally mirror CI:
xvfb-run -a dbus-run-session -- cargo test --workspace --locked -- --nocapture

# Headless crates (no GTK) run fine without xvfb/dbus:
cargo test -p flowmux-core
cargo test -p flowmux-ipc -- --nocapture

# Single test by name (substring match):
cargo test -p flowmux-core title_is_shell_cwd_echo

# Lint / format (toolchain ships rustfmt + clippy).
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

### Terminal backend: libghostty-vt

flowmux has a single terminal backend: a libghostty-vt core that flowmux
renders itself (Cairo/Pango). It links a static `libghostty-vt.a` built from a
pinned Ghostty revision by `scripts/build-ghostty-vt.sh` (`zig build
-Demit-lib-vt`), invoked automatically by `flowmux-terminal`'s `build.rs` on the
first build, so **Zig 0.15.x must be on `PATH`** for any build that includes
`flowmux-terminal`. There is no compile-time feature gate and no VTE path any
more, and `flowmux-terminal` is in `default-members`, so even a bare
`cargo check` / `cargo check --workspace` builds the native shim and needs Zig
(the `default-members` GTK exclusion only spares the GUI's system headers, not
the Zig requirement). `read-screen` reads the viewport straight from the
libghostty grid, so it needs no extra feature.

CJK note: the libghostty renderer keeps the configured font family unchanged.
Pango/fontconfig performs fallback for missing Hangul/CJK glyphs, and the
renderer centers wide glyphs inside their two-cell box without changing the
Latin font metrics.

The Flatpak build (Ubuntu 22.04 path) is described in `README.md`
under "Ubuntu 22.04 (jammy) support". `flowmux doctor` / `flowmux fix`
audit and repair the on-host pieces (agent hooks, SKILL files, socket,
browser data dir).

## Architecture

flowmux is split into focused crates so the GTK GUI and headless tools
can share core logic. Two binaries fall out of the workspace:

- `flowmux` (crate `flowmux/`) — the GTK4 + libadwaita GUI. Also acts
  as a thin shim: if invoked with a CLI subcommand, it `exec`s
  `flowmuxctl` so `flowmux browser open …` from inside a pane works
  without the user knowing two binaries exist.
- `flowmuxctl` (crate `flowmux-cli/`) — the daemon client. Speaks the
  IPC protocol over the Unix socket and is what AI-agent hooks invoke.

### Daemon-in-GUI model

There is no separate long-running daemon process. The GUI binary
embeds the daemon: on startup it spawns a tokio runtime and starts the
IPC server (`flowmux-ipc::server`) bound to a Unix socket at
`$XDG_RUNTIME_DIR/flowmux.sock`. The `flowmux-daemon` crate is the
shared handler — `flowmux-daemon::DaemonHandler` + `StateStore` are
embedded by the GUI binary; a headless `flowmux-daemon` binary exists
mostly for tests and logs verb traffic instead of touching widgets.

### tokio ↔ GTK bridge

GTK widgets are `!Send`, so anything that mutates the widget tree has
to run on the main thread. The IPC handler runs on tokio and the
window controller runs on GTK. They are connected via an
`async_channel` of `GtkCommand` values (`crates/flowmux/src/bridge/`):
the IPC handler sends commands and awaits a `oneshot::Sender` reply;
`spawn_dispatch_loop` reads them on the GTK side via
`glib::MainContext::spawn_local` and dispatches into the
`WindowController`. New IPC verbs that touch widgets follow that
pattern — add a `GtkCommand` variant, plumb a `oneshot` reply, and
handle it in the dispatch loop.

### Surface model

A `Workspace` (`flowmux-core`) owns a tree of `Pane`s; each `Pane`
holds one or more `PaneSurface`s of kind `Terminal` or `Browser`.
Terminal surfaces are stored as `PaneTerminal`
(`crates/flowmux/src/ui/pane_terminal.rs`), which is a type alias for the sole
backend `ui::ghostty_pane::GhosttyPane` (PTY + `flowmux-terminal::vt::Vt` + a
Cairo/Pango `DrawingArea` renderer); `pane_terminal.rs` also holds the shared
`PaneCallbacks` bundle. Browser surfaces are backed by a scriptable WebView —
WebKitGTK 6.0 on Linux, WKWebView on macOS (the platform split lives in
`flowmux/src/ui/browser_pane_{webkit,macos,stub}.rs`; `flowmux-browser` holds the
shared controller trait + snapshot types). The IPC protocol and the
GUI both refer to these by `WorkspaceId` / `PaneId` / `SurfaceId` (UUID newtypes
in `flowmux-core`).

The libghostty backend lives across two crates: `flowmux-terminal`
(headless — `vt::Vt` over a C shim `csrc/ghostty_shim.*` to libghostty-vt,
plus `pty::Pty`) and `flowmux/src/ui/ghostty_pane.rs` (the GTK widget:
render, input via libghostty's key/mouse encoders, IME, selection,
scrollback + scrollbar). New per-cell render data flows shim →
`Vt::read_grid` → the draw pass; new input goes through `Vt::encode_key`/
`encode_mouse` so terminal modes (DECCKM, keypad, Kitty, mouse) are honored.

### Crates at a glance

The crate tree under `crates/` matches the architectural seams above.
The README has a one-line description per crate; the cross-cutting
points worth knowing when navigating:

- `flowmux-core` — pure domain types (no GTK, no tokio). Shared by
  every other crate. Has its own unit tests for cwd/title logic.
- `flowmux-config` — XDG paths and parsers for the user's
  `cmux.json` / Ghostty config. The canonical entry point for "where
  on disk does X live".
- `flowmux-state` — persistent workspace/session JSON store. Reopened
  on app start so resumed workspaces land on the right pane.
- `flowmux-terminal` / `flowmux-browser` — surface backends. They do
  not depend on GTK widgets directly; the GUI crate adapts them.
  `flowmux-terminal` exposes `vt::Vt` (libghostty-vt via the C shim) and
  `pty::Pty`; its headless tests cover the VT/PTY contract
  (`cargo test -p flowmux-terminal` — always builds libghostty-vt, so Zig is
  required).
- `flowmux-ipc` — wire protocol (Unix socket, newline-delimited JSON).
  `protocol::Request`/`Response` are the source of truth for verb
  shape. The verb set mirrors cmux's socket API; unimplemented verbs
  return `RpcError::Unimplemented` so the CLI surface stays stable.
- `flowmux-daemon` — IPC handler + state store, embedded by both the
  GUI and the headless `flowmux-daemon` binary.
- `flowmux-notify` — OSC 9/99/777 parsing and libnotify D-Bus
  delivery. `DESKTOP_FILE_BASENAME` is compile-time matched against
  the GUI's `APP_ID`; do not change one without the other.
- `flowmux-cli` (`flowmuxctl`) — every CLI subcommand. Reads
  `FLOWMUX_SOCKET_PATH` / `FLOWMUX_PANE_ID` from env so hooks running
  inside a pane can omit pane arguments.
- `flowmux-cookies` — host browser session import (libsecret + sqlite).
- `flowmux-procmon` / `flowmux-ssh` / `flowmux-vcs` — auxiliary
  features (PID/port watcher, SSH workspaces via russh, Git/PR sidebar).

### Pane-aware env vars

Every PTY flowmux spawns gets `FLOWMUX_PANE_ID`, `FLOWMUX_SURFACE_ID`,
`FLOWMUX_WORKSPACE_ID`, `FLOWMUX_TAB_ID` (alias of workspace id),
`FLOWMUX_SOCKET_PATH`, and optionally `FLOWMUX_BUNDLED_CLI_PATH`. CLI
verbs and agent hooks look these up as fallbacks for `--pane` / socket
arguments — see `crates/flowmux-cli/src/main.rs` (`pane_from_env`).
When adding a new verb that targets a pane, accept an explicit
`--pane` flag *and* fall back to the env var so agent hooks stay
one-line invocations.

## Agent / browser integration

`AGENTS.md` is the contract for AI coding agents running *inside* a
flowmux PTY. Highlights worth remembering when editing browser /
agent code:

- The in-app browser is the preferred web automation surface for agents
  in a flowmux pane (over Playwright / Puppeteer / system Chromium).
  Snapshots return Markdown + an `eN` ref-token map; refs are
  invalidated by the next snapshot or any navigation.
- WebKitGTK 6.0 / WKWebView do **not** expose CDP, so CDP-only verbs
  (network mocking, viewport, screencast) are intentionally `not_supported`; do
  not stub them as no-ops. `wait` (DOM polling) and `screenshot` (native
  snapshot) do not need CDP and are supported.
- Agent hooks (Claude Code, Codex, OpenCode) are installed by
  `flowmux fix` and audited by `flowmux doctor`. Hook payloads ship
  *inside* the binary, so any change to the on-disk hook format must
  be paired with a `doctor`/`fix` revision.

## Project conventions

### Terminology (user-facing text)

Use the terms below in **all** human-readable text: UI labels,
notifications, errors, tooltips, commit messages, README text, and
user-facing discussion. Keep code identifiers in their existing
English form.

| Term | Meaning | Matching Code Identifier |
|---|---|---|
| side panel | The full left-side panel area | `Sidebar` |
| workspace | Each side-panel item and unit of work | `Workspace` |
| workspace name | The name shown for each side-panel item | `Workspace.name` |
| pane | A split window inside the main content area | `Pane` |
| tab | A terminal shown inside a pane | `PaneSurface` (`SurfaceKind::Terminal`) |
| browser tab | A browser shown inside a pane at the same level as a tab | `PaneSurface` (`SurfaceKind::Browser`) |
| tab name | The name shown in the pane tab bar | `PaneSurface.title` |

Rules:

- Use these terms consistently in user-facing text.
- Keep existing English code identifiers. Only rename identifiers
  after an explicit decision.
- In comments, use the terminology above for behavior descriptions and
  the exact identifier name when referring to a concrete type, field,
  or function.
- Do not mix multiple terms for the same concept in one document or view.

### Licensing

flowmux is GPL-3.0-or-later. Every source file starts with an
`SPDX-License-Identifier: GPL-3.0-or-later` line — preserve it on
edits and add it to any new file. Do not import code, assets, or
documentation from cmux or any other project unless it is
license-compatible and attribution is added; see `CONTRIBUTING.md`.
