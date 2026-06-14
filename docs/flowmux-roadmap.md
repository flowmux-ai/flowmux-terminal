<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# flowmux roadmap

This document records the implementation plan for making flowmux a practical
Linux-first terminal, browser, and agent workflow multiplexer.

Status baseline: 2026-06-14. The plan is based on the current repository state.

## Goal

flowmux should focus on the workflows that matter most for Linux users running
AI coding agents:

- provide reliable terminal, pane, workspace, and browser control in one GTK
  application;
- make the agent-facing CLI predictable enough for scripts and local skills;
- support Ubuntu 24.04 natively, Ubuntu 22.04 through Flatpak, and WSL Ubuntu
  for terminal/CLI workflows where desktop integration is limited;
- avoid large product surfaces until the core local workflow is stable.

## Current product scope

| Area | Current state | Main gap |
|---|---|---|
| Platform | Linux/GTK4 app. Ubuntu 24.04 native path, Ubuntu 22.04 Flatpak path. | Needs a clear release validation matrix for Ubuntu and WSL. |
| Terminal panes | Workspaces, panes, terminal/browser surfaces, flowmux-owned PTYs. | Needs more complete CLI control over panes and surfaces. |
| Browser | WebKitGTK in-app browser, IPC-driven snapshot/actions, Firefox cookie import, partial Chromium-family cookie detection. | Needs complete core action/query CLI exposure and one consistent snapshot/ref implementation. |
| CLI shape | Several useful top-level commands exist, but documented browser namespace and actual parser are not fully aligned. | Highest priority is aligning CLI behavior with project docs and agent instructions. |
| Agent hooks | Claude Code, Codex, and OpenCode hook installation plus notification/activity updates. | Harden the supported agents before adding more integrations. |
| Session restore | Persistent workspace/window/sidebar state and agent session storage exist. | Needs reliable user-visible restore for core surfaces. |
| Notifications | Desktop notifications and in-app notification state exist. | Needs CLI notification management for scripted and keyboard-driven workflows. |
| SSH/remote | `flowmux ssh` command surface exists, but real SSH workspace handling is not wired through. | Start with a minimal remote terminal workflow before adding advanced remote features. |
| Config | Reads a useful project command/config subset. | Extend only the parts that unlock daily workflow. |
| Tmux-style compatibility | Not a primary surface today. | Minimal aliases are useful for agent scripts, but full terminal multiplexer replacement is not required. |
| Sidebar/feed surfaces | Notifications/activity surfaces exist; a full decision feed is not present. | Defer larger sidebar/feed features until core workflow is stable. |

## Priority labels

- Importance `S`: core requirement for the Linux workflow goal.
- Importance `A`: high user value once the core contract is stable.
- Importance `B`: useful compatibility, but not required for the first reliable
  release.
- Importance `C`: defer unless product direction changes.
- Necessity `required`: must be done for the stated goal.
- Necessity `near-required`: needed for strong day-to-day usability.
- Necessity `optional`: useful but can wait.

## P0: contract and agent usability

These items should be implemented first because they define whether users and
agents can drive flowmux reliably.

| Priority | Feature unit | Importance | Necessity | Implementation target |
|---|---|---:|---:|---|
| P0-1 | CLI namespace alignment | S | required | Implement the documented `flowmux browser <subcommand>` shape. Keep current top-level commands as compatibility aliases. |
| P0-2 | Browser action/query CLI completion | S | required | Expose already-supported IPC/GUI operations through CLI: `dblclick`, `hover`, `focus`, `blur`, `check`, `uncheck`, `is-visible`, `is-enabled`, `is-checked`, and `count`. |
| P0-3 | Snapshot implementation unification | S | required | Use the `flowmux-browser` RefStore-backed, non-DOM-mutating snapshot path everywhere. Remove or replace older code paths that stamp ref attributes into the page. |
| P0-4 | Identity and capability commands | S | required | Add `identify` and `capabilities` so agents can discover current window, workspace, pane, surface, socket path, supported browser verbs, and unsupported features. |
| P0-5 | Workspace/pane/surface CLI basics | S | required | Add or complete list/current/focus/close/new commands for workspaces, panes, and surfaces, plus a tree-style inspection command. |
| P0-6 | Terminal automation basics | S | required | Add `send`, `send-key`, and `read-screen` equivalents for agent scripts and terminal multiplexer style compatibility. |

P0 completion criteria:

- The examples in `AGENTS.md` work without translation.
- A browser loop works end to end: open, snapshot, click/fill/type/press,
  state query, text/value/attr/url/title.
- Refs are invalidated on the next snapshot/navigation and are never written
  into the live DOM.
- An agent can discover its current flowmux context with one command.
- A script can inspect panes, focus a target, read terminal output, and send
  input.

## P1: daily usability

These items should follow P0. They make flowmux dependable for real
agent-heavy work.

| Priority | Feature unit | Importance | Necessity | Implementation target |
|---|---|---:|---:|---|
| P1-1 | Notification CLI management | A | near-required | Add list/open/jump-to-unread/mark-read/clear commands for notification state. |
| P1-2 | Reliable session restore | A | near-required | Restore workspace layout, focused surfaces, terminal cwd, browser URLs/history where practical, and Claude/Codex/OpenCode session IDs. |
| P1-3 | Chromium-family cookie import | A | near-required | Implement libsecret-backed Chromium cookie unwrapping for Chrome, Chromium, Brave, Edge, and Arc where Linux data stores are available. |
| P1-4 | Command palette | A | near-required | Provide keyboard-driven commands for new workspace, split, open browser, rename, reload config, clear notifications, and jump to unread. |
| P1-5 | Rename/reorder/close UX | A | near-required | Make workspace and surface organization fast from keyboard and context menus. |
| P1-6 | Project config compatibility | A | near-required | Support project-local config, JSONC where needed, custom command targets, confirm prompts, and reload-config behavior. |

P1 completion criteria:

- A user can restart flowmux and continue the same core workflow with minimal
  manual reconstruction.
- Common operations are available by keyboard without remembering long CLI
  calls.
- Browser login reuse works for Firefox and common Chromium-family browsers on
  supported Linux desktops.
- Project config can launch common agent commands into predictable targets.

## P2: compatibility maturity

These items improve compatibility, but they should not delay P0/P1.

| Priority | Feature unit | Importance | Necessity | Implementation target |
|---|---|---:|---:|---|
| P2-1 | Minimal `flowmux ssh` | A | near-required | Start with OpenSSH-based remote terminal workspaces, cwd/title propagation, and notification routing. Defer remote daemon/proxy architecture. |
| P2-2 | Browser `wait` | A | near-required | Add selector/text/url/readyState/JS-predicate polling. Do not promise network-idle waits. |
| P2-3 | Browser screenshot | B | optional | Add viewport screenshot only if WebKitGTK support is stable enough. Full-page capture can wait. |
| P2-4 | Minimal terminal multiplexer aliases | B | optional | Support aliases most likely used by agent scripts: capture/read-screen, send-keys, list/select pane, resize-pane where feasible. |
| P2-5 | Compact right sidebar | B | optional | Show notifications, active sessions, and recent logs. Do not implement a larger decision-feed system yet. |

## Deferred scope

These are intentionally not required for the current Linux workflow goal:

- cloud VM orchestration;
- mobile or iOS surfaces;
- full decision-feed product surface;
- workspace groups;
- browser network mocking, tracing, screencast, raw input, or accurate device
  emulation;
- a broad agent integration matrix;
- macOS-specific UX or AppKit behavior.

## Testing strategy

Implementing on macOS is useful for editing, static checks, CLI parser work,
protocol tests, and pure Rust unit tests. It is not sufficient as the release
test environment for flowmux.

The required support targets are Linux targets:

| Target | Required support level | Why it matters |
|---|---|---|
| Ubuntu 24.04 native | Full support | Main native build target for GTK4, libadwaita, WebKitGTK 6.0, libnotify, PTY behavior, XDG paths, and host browser integration. |
| Ubuntu 22.04 Flatpak | Full support through Flatpak | 22.04 does not provide the required native GTK/WebKit stack, so the Flatpak path must be treated as a first-class target. |
| WSL Ubuntu | Terminal/CLI support, limited desktop support | WSL can cover CLI, IPC, PTY, hooks, config, and many daemon flows. Browser/desktop notification behavior depends on WSLg and should be tested separately from native Linux. |
| macOS development host | Development aid only | macOS cannot validate GTK/WebKitGTK runtime behavior, Linux desktop integration, Flatpak packaging, libsecret, D-Bus notifications, or WSL-specific behavior. |

### What macOS can validate

- CLI argument parsing and command namespace compatibility.
- IPC protocol serialization and request/response mapping.
- Config parsing.
- Agent hook file generation logic where paths can be redirected to temp dirs.
- Pure Rust unit tests for state, notification parsing, ref resolution, and
  browser command construction.
- Documentation and examples.

### What macOS cannot validate sufficiently

- GTK4/libadwaita window, pane, focus, shortcut, and rendering behavior.
- WebKitGTK browser behavior, snapshot execution timing, cookies, media, and
  Web Inspector behavior.
- Linux PTY edge cases, process trees, shell startup, and environment injection.
- libnotify and D-Bus notification routing.
- libsecret-backed Chromium cookie decryption.
- Flatpak sandbox integration, host command bridging, WebKit GPU behavior, and
  bundled runtime dependencies.
- WSLg rendering, D-Bus availability, Windows filesystem paths, and shell
  interop.

### Required validation matrix

Use macOS for fast development, then gate user-facing changes with this matrix:

| Check | macOS | Ubuntu 24.04 native | Ubuntu 22.04 Flatpak | WSL Ubuntu |
|---|---:|---:|---:|---:|
| `cargo fmt --check` | yes | yes | yes | yes |
| `cargo check --workspace` | partial | yes | yes, via Flatpak build context | yes for non-GUI-safe subsets |
| CLI parser tests | yes | yes | yes | yes |
| IPC protocol tests | yes | yes | yes | yes |
| GTK window/pane smoke test | no | yes | yes | WSLg only |
| Browser open/snapshot/action loop | no | yes | yes | WSLg only |
| Notification routing | no | yes | yes | best effort |
| Cookie import | no | yes | yes | browser-dependent |
| Agent hook install/repair | partial | yes | yes | yes |
| Session restore | no | yes | yes | yes for terminal/CLI state |
| Packaging/install | no | native install scripts | Flatpak build/install | install script or documented WSL path |

### Recommended test automation stages

1. Unit and parser tests on every platform.
2. Ubuntu 24.04 CI for native `cargo check`, unit tests, CLI integration tests,
   and headless protocol tests.
3. Ubuntu 24.04 graphical smoke tests under a virtual display for GTK startup,
   pane creation, browser open, snapshot, click/fill/type, and notification
   state.
4. Ubuntu 22.04 Flatpak build test with a smoke run of the installed app.
5. Manual or scheduled WSL Ubuntu checks for install, CLI, PTY env injection,
   agent hooks, and WSLg browser behavior.

## Suggested execution order

1. Fix the CLI/API contract: namespace alignment, browser command exposure,
   identify/capabilities, pane/surface inspection, and terminal send/read.
2. Stabilize browser automation: one snapshot/ref implementation, core action
   loop tests, and explicit unsupported responses for deferred browser features.
3. Harden notifications and session restore for Claude, Codex, and OpenCode.
4. Improve browser login reuse with Chromium-family cookie import.
5. Add compatibility conveniences: project config, command palette,
   rename/reorder UX, and minimal terminal multiplexer aliases.
6. Implement minimal SSH after local workflow stability is proven.

## Practical release gate

A feature should not be considered complete just because it works from a macOS
development checkout. For flowmux, completion means:

- it has focused unit or integration tests where practical;
- the documented CLI form works on Ubuntu 24.04;
- the Ubuntu 22.04 Flatpak path still builds and launches;
- WSL impact is known and documented;
- unsupported features return explicit unsupported/not-supported responses
  instead of failing ambiguously.
