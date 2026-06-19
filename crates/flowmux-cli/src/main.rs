// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux` — thin CLI client over the daemon IPC socket.
//!
//! Verb shape mirrors cmux's documented CLI so existing user automation
//! (scripts, Claude Code hooks, etc.) keeps working.

use anyhow::Context;
use clap::{Parser, Subcommand};
use flowmux_config::paths;
use flowmux_core::{NotificationLevel, PaneId, SplitDirection, SurfaceId, WorkspaceId};
use flowmux_ipc::{client::Client, protocol::Request, protocol::Response};
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;

mod agent;
mod desktop_install;
mod doctor;
mod hook_install;
mod hooks;
mod pty_tee;

/// Read `FLOWMUX_PANE_ID` (set by `flowmux` at PTY spawn time) and parse
/// it as a `PaneId`. Returns `None` if the env var is missing or invalid.
/// Used as a fallback when the user does not pass an explicit `--pane`/
/// positional pane id, so terminal-side agents can call e.g.
/// `flowmux browser open https://...` without knowing their own pane.
fn pane_from_env() -> Option<PaneId> {
    std::env::var("FLOWMUX_PANE_ID")
        .ok()
        .as_deref()
        .and_then(|s| PaneId::from_str(s).ok())
}

/// Resolve a pane argument: the explicit value if given, else the
/// `FLOWMUX_PANE_ID` of the calling pane. Errors when neither is set.
fn resolve_pane(pane: Option<PaneId>) -> anyhow::Result<PaneId> {
    pane.or_else(pane_from_env)
        .ok_or_else(|| anyhow::anyhow!("no pane: pass pane:<uuid> or set FLOWMUX_PANE_ID"))
}

/// Translate a named terminal key (`Enter`, `Tab`, `ArrowUp`, …) into
/// the byte sequence a PTY expects. A single character passes through as
/// itself. Used by `flowmux send-key` for tmux-style key-name input;
/// raw byte/escape input still goes through `send-keys`.
fn named_key_to_bytes(key: &str) -> anyhow::Result<String> {
    let seq = match key {
        "Enter" | "Return" | "CR" => "\r",
        "Tab" => "\t",
        "Escape" | "Esc" => "\x1b",
        "Backspace" | "BSpace" => "\x7f",
        "Delete" | "DC" => "\x1b[3~",
        "Space" => " ",
        "Up" | "ArrowUp" => "\x1b[A",
        "Down" | "ArrowDown" => "\x1b[B",
        "Right" | "ArrowRight" => "\x1b[C",
        "Left" | "ArrowLeft" => "\x1b[D",
        "Home" => "\x1b[H",
        "End" => "\x1b[F",
        "PageUp" | "PPage" => "\x1b[5~",
        "PageDown" | "NPage" => "\x1b[6~",
        // A bare single character (e.g. `a`, `:`) is sent verbatim.
        other if other.chars().count() == 1 => return Ok(other.to_string()),
        other => {
            anyhow::bail!("unknown key name {other:?}; use a named key (Enter, Tab, ArrowUp, …), a single character, or `send-keys` for raw input")
        }
    };
    Ok(seq.to_string())
}

#[derive(Parser)]
#[command(
    name = "flowmux",
    version,
    about = "Linux/GTK4 terminal for AI coding agents"
)]
struct Cli {
    /// Override the daemon socket path. Defaults to `FLOWMUX_SOCKET_PATH`
    /// (injected by `flowmux` into every PTY) and falls back to the
    /// XDG runtime path. `FLOWMUX_SOCKET` is accepted as a legacy alias.
    #[arg(long, env = "FLOWMUX_SOCKET_PATH")]
    socket: Option<PathBuf>,

    /// Print responses as a single-line JSON object instead of the
    /// default human-readable indented form. Mirrors cmux's `--json`
    /// flag — easier to parse from agent scripts.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon health probe.
    Ping,

    /// Print the calling agent's flowmux context (pane, surface,
    /// workspace, socket) resolved from the `FLOWMUX_*` env vars that
    /// flowmux injects into every PTY. One command for an agent to
    /// discover where it is running.
    Identify,

    /// Print what this flowmux build supports: the `browser` verb set
    /// and the explicitly-unsupported (CDP-only) features.
    Capabilities,

    /// Inspect the live workspace → pane → tab structure. Prints an
    /// indented tree (or `--json` for scripts).
    Tree,

    /// Workspace operations.
    Workspace {
        #[command(subcommand)]
        op: WorkspaceOp,
    },

    /// Send a desktop notification attached to a pane.
    ///
    /// When `--pane` is omitted the daemon picks up `FLOWMUX_PANE_ID`
    /// from the calling PTY (set by flowmux at spawn time), so
    /// hooks running inside a flowmux pane can omit the flag and still
    /// have the click-through routed to the right pane.
    Notify {
        #[arg(long)]
        pane: Option<PaneId>,
        #[arg(long, default_value = "Terminal")]
        title: String,
        #[arg(long, default_value = "info", value_parser = ["info", "attention", "error"])]
        level: String,
        body: String,
    },

    /// Friendly helper for AI-agent hooks (Claude Code, OpenCode,
    /// Codex). Fires an `AttentionNeeded` toast titled with the agent
    /// name so flowmux's bell popover and the OS notification both say
    /// "Claude is ready" without the caller having to spell every flag.
    /// Like `Notify`, falls back to `FLOWMUX_PANE_ID` when `--pane` is
    /// omitted, so it works as a one-liner from a hook script.
    NotifyComplete {
        /// Agent name. Used as the title prefix and exposed to the
        /// router as a category, e.g. "Claude", "Codex", "OpenCode".
        #[arg(long)]
        agent: String,
        /// Optional message — defaults to "task complete".
        #[arg(long)]
        message: Option<String>,
        /// Override the source pane (otherwise FLOWMUX_PANE_ID).
        #[arg(long)]
        pane: Option<PaneId>,
    },

    /// Split a pane.
    Split {
        pane: PaneId,
        #[arg(long, conflicts_with = "down")]
        right: bool,
        #[arg(long)]
        down: bool,
    },

    /// Send keystrokes to a pane (escape sequences accepted).
    SendKeys { pane: PaneId, keys: String },

    /// Send a single named key to a pane (`Enter`, `Tab`, `ArrowUp`, …,
    /// or one character). `--pane` falls back to `$FLOWMUX_PANE_ID`. For
    /// raw or multi-byte input use `send-keys`.
    SendKey {
        key: String,
        #[arg(long)]
        pane: Option<PaneId>,
    },

    /// Print a terminal pane's buffer text (`pane:<uuid>` or bare uuid;
    /// falls back to `$FLOWMUX_PANE_ID`). Requires a flowmux built with
    /// the `vte-text` feature.
    ReadScreen { pane: Option<PaneId> },

    /// Grab keyboard focus for a pane (falls back to `$FLOWMUX_PANE_ID`).
    FocusPane { pane: Option<PaneId> },

    /// Close a pane (falls back to `$FLOWMUX_PANE_ID`). Refuses to close
    /// a workspace's last pane.
    ClosePane { pane: Option<PaneId> },

    /// Make a tab the active one in its pane. `--pane` falls back to
    /// `$FLOWMUX_PANE_ID`.
    FocusTab {
        surface: SurfaceId,
        #[arg(long)]
        pane: Option<PaneId>,
    },

    /// Close a tab. `--pane` falls back to `$FLOWMUX_PANE_ID`. Refuses
    /// to close the last tab of a workspace's last pane.
    CloseTab {
        surface: SurfaceId,
        #[arg(long)]
        pane: Option<PaneId>,
    },

    /// In-app browser automation: `flowmux browser <op> …`. This is the
    /// documented agent-facing surface (see `AGENTS.md`). The older
    /// hyphenated `browser-*` commands remain as hidden compatibility
    /// aliases.
    Browser {
        #[command(subcommand)]
        op: BrowserOp,
    },

    /// Open a remote workspace over SSH.
    Ssh { target: String },

    /// Pipe a byte stream through the OSC parser and forward each
    /// notification to the daemon. Useful for wiring agent hooks:
    ///
    ///   tail -f ~/.claude/log | flowmux notify-stream
    NotifyStream {
        /// Optional pane id to attribute notifications to.
        #[arg(long)]
        pane: Option<PaneId>,
    },

    /// Internal: PTY-tee proxy used by the GUI as a transparent
    /// wrapper around the user's shell.
    ///
    /// Forks the child argv on an inner PTY, pumps bytes between the
    /// outer terminal pane and the inner shell, and snoops every
    /// inner→outer byte through the OSC parser so OSC 9 / 99 / 777
    /// notifications emitted by agents like Claude Code or Codex
    /// reach the daemon's `Request::Notify` path without depending on
    /// a GUI terminal signal.
    ///
    /// Hidden because end users should never invoke it directly —
    /// `terminal_pane::spawn` wraps the shell with it automatically.
    #[command(name = "pty-tee", hide = true)]
    PtyTee {
        /// Pane id this terminal belongs to. Forwarded as the
        /// notification's `pane` so the bell-popover click router can
        /// focus the right pane.
        #[arg(long)]
        pane: Option<PaneId>,
        /// Surface (tab) id inside `pane`. Lets the router switch
        /// tabs when the user is currently looking at a sibling.
        #[arg(long)]
        surface: Option<SurfaceId>,
        /// Argv of the user's shell (everything after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        argv: Vec<OsString>,
    },

    /// Open a workspace with N panes, each running `claude`.
    /// Mirrors cmux's `claude-teams` launcher.
    ClaudeTeams {
        /// Number of teammate panes (1..=8).
        #[arg(long, default_value_t = 4)]
        count: u8,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Arguments forwarded to `claude` in each pane.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    // ---- Compatibility aliases ----
    // The hyphenated `browser-*` commands below predate the documented
    // `flowmux browser <op>` namespace (see `BrowserOp`). They are kept
    // working but hidden from `--help` so existing scripts/hooks that
    // call them do not break.
    /// Take a JSON snapshot of the page in a browser pane.
    #[command(hide = true)]
    BrowserSnapshot { pane: PaneId },

    /// Run JS in a browser pane and print the result.
    #[command(hide = true)]
    BrowserEval { pane: PaneId, source: String },

    /// Navigate a browser pane to a new URL.
    #[command(hide = true)]
    BrowserNavigate { pane: PaneId, url: String },
    /// Move a browser pane backward in session history.
    #[command(hide = true)]
    BrowserBack { pane: PaneId },
    /// Move a browser pane forward in session history.
    #[command(hide = true)]
    BrowserForward { pane: PaneId },
    /// Reload the current page in a browser pane.
    #[command(hide = true)]
    BrowserReload { pane: PaneId },
    /// Print the current URL of a browser pane.
    #[command(hide = true)]
    BrowserUrl { pane: PaneId },
    /// Print the current page title of a browser pane.
    #[command(hide = true)]
    BrowserTitle { pane: PaneId },
    /// Click an element by its ref id (from a snapshot).
    #[command(hide = true)]
    BrowserClick { pane: PaneId, target: String },
    /// Fill an input/textarea by ref id with `value`.
    #[command(hide = true)]
    BrowserFill {
        pane: PaneId,
        target: String,
        value: String,
    },
    /// Select a `<select>` option by value or visible text.
    #[command(hide = true)]
    BrowserSelect {
        pane: PaneId,
        target: String,
        value: String,
    },
    /// Scroll an element into view, then offset the viewport by (x, y).
    #[command(hide = true)]
    BrowserScroll {
        pane: PaneId,
        target: String,
        x: i32,
        y: i32,
    },
    /// Type literal text into the active element of a browser pane.
    #[command(hide = true)]
    BrowserType { pane: PaneId, text: String },
    /// Press a single named key (Enter, Tab, ArrowDown, …).
    #[command(hide = true)]
    BrowserPress { pane: PaneId, key: String },
    /// Read innerText of an element.
    #[command(hide = true)]
    BrowserText { pane: PaneId, target: String },
    /// Read .value of an input/textarea/select.
    #[command(hide = true)]
    BrowserValue { pane: PaneId, target: String },
    /// Read an attribute (`href`, `id`, `class`, …) of an element.
    #[command(hide = true)]
    BrowserAttr {
        pane: PaneId,
        target: String,
        name: String,
    },

    /// Import cookies from a host browser into the in-app browser jar.
    ImportCookies {
        /// Browser slug: firefox, chrome, chromium, brave, edge, arc.
        #[arg(long)]
        from: String,
        /// Optional domain substring filter.
        #[arg(long)]
        domain: Option<String>,
    },

    /// List browsers we can import from (and whether we detect them).
    ListBrowsers,

    /// Theme management.
    Theme {
        #[command(subcommand)]
        op: ThemeOp,
    },

    /// Make the flowmux-browser SKILL discoverable to local agents
    /// (Claude Code, OpenCode, Codex CLI). Run after every `flowmux`
    /// install / upgrade to re-sync the on-disk skill files.
    Agent {
        #[command(subcommand)]
        op: AgentOp,
    },

    /// Hook glue between agent CLIs and flowmux's notification system.
    /// `flowmux hooks setup` registers entries with each supported
    /// agent so its lifecycle events route into flowmux. The other
    /// subcommands are invoked by the agents themselves at runtime
    /// (Claude Code's `Stop` hook, Codex's `hooks.json` `Stop`, etc).
    Hooks {
        #[command(subcommand)]
        op: HooksOp,
    },

    /// Audit every flowmux ↔ host integration in one place: AI-agent
    /// SKILL files, AI-agent lifecycle hooks (Claude / Codex /
    /// OpenCode), the in-app browser data dir, host browsers visible
    /// to the cookie importer, and the daemon socket. Read-only — use
    /// `flowmux fix` to repair the rows tagged `fix`.
    Doctor,

    /// Re-install / refresh every flowmux-managed integration the
    /// `doctor` would flag. Idempotent: a row that's already correct
    /// is a no-op. Skips agents whose home directory is missing, so
    /// it's safe to re-run after installing Claude / Codex / OpenCode
    /// for the first time.
    Fix,
}

/// `flowmux browser <op>` — the documented agent-facing browser surface.
///
/// Every pane argument accepts `pane:<uuid>`, `surface:<uuid>`, or a bare
/// `<uuid>` (handled by `PaneId`'s `FromStr`). Refs (`eN`) come from the
/// most recent `browser snapshot` of the same pane and are resolved
/// server-side via the daemon's `RefStore`.
#[derive(Subcommand)]
enum BrowserOp {
    /// Open URL in a new in-app browser pane (splits next to the
    /// currently focused pane). Default split direction is right.
    Open {
        url: String,
        #[arg(long, conflicts_with = "down")]
        right: bool,
        #[arg(long)]
        down: bool,
    },
    /// Take a JSON snapshot of the page. Markdown tree + refs map; the
    /// live DOM is never modified.
    Snapshot { pane: PaneId },
    /// Run JS in a browser pane and print the result.
    Eval { pane: PaneId, source: String },
    /// Navigate a browser pane to a new URL.
    Navigate { pane: PaneId, url: String },
    /// Move a browser pane backward in session history.
    Back { pane: PaneId },
    /// Move a browser pane forward in session history.
    Forward { pane: PaneId },
    /// Reload the current page in a browser pane.
    Reload { pane: PaneId },
    /// Print the current URL of a browser pane.
    Url { pane: PaneId },
    /// Print the current page title of a browser pane.
    Title { pane: PaneId },
    /// Click an element by ref id (from a snapshot).
    Click { pane: PaneId, target: String },
    /// Fill an input/textarea by ref id with `value`.
    Fill {
        pane: PaneId,
        target: String,
        value: String,
    },
    /// Select a `<select>` option by value or visible text.
    Select {
        pane: PaneId,
        target: String,
        value: String,
    },
    /// Scroll an element into view, then offset the viewport by (x, y).
    Scroll {
        pane: PaneId,
        target: String,
        x: i32,
        y: i32,
    },
    /// Type literal text into the active element of a browser pane.
    Type { pane: PaneId, text: String },
    /// Press a single named key (Enter, Tab, ArrowDown, …).
    Press { pane: PaneId, key: String },
    /// Read innerText of an element.
    Text { pane: PaneId, target: String },
    /// Read .value of an input/textarea/select.
    Value { pane: PaneId, target: String },
    /// Read an attribute (`href`, `id`, `class`, …) of an element.
    Attr {
        pane: PaneId,
        target: String,
        name: String,
    },
    /// Double-click an element by ref id.
    #[command(name = "dblclick")]
    DblClick { pane: PaneId, target: String },
    /// Hover the pointer over an element by ref id.
    Hover { pane: PaneId, target: String },
    /// Focus an element by ref id.
    Focus { pane: PaneId, target: String },
    /// Blur (unfocus) an element by ref id.
    Blur { pane: PaneId, target: String },
    /// Check a checkbox/radio by ref id.
    Check { pane: PaneId, target: String },
    /// Uncheck a checkbox by ref id.
    Uncheck { pane: PaneId, target: String },
    /// Print `true`/`false` for an element's visibility.
    IsVisible { pane: PaneId, target: String },
    /// Print `true`/`false` for whether an element is enabled.
    IsEnabled { pane: PaneId, target: String },
    /// Print `true`/`false` for whether a checkbox/radio is checked.
    IsChecked { pane: PaneId, target: String },
    /// Count elements matching a CSS selector.
    Count { pane: PaneId, selector: String },
}

#[derive(Subcommand)]
enum AgentOp {
    /// Mirror the embedded SKILL.md into each supported agent's
    /// user-level location. Idempotent: a no-op when the file is
    /// already up to date. Fails on existing-but-different files
    /// unless `--force` is set.
    Install {
        /// Limit installation to one agent. Repeat the flag to pick
        /// multiple. Omit to install for all known agents.
        #[arg(long, value_parser = ["claude-code", "opencode", "codex"])]
        agent: Vec<String>,
        /// Overwrite drifted on-disk files instead of erroring.
        #[arg(long)]
        force: bool,
    },
    /// Report whether each agent's expected file is present and
    /// matches the bundled SKILL. Exit code is 0 only when every
    /// checked target is `ok`.
    Doctor {
        #[arg(long, value_parser = ["claude-code", "opencode", "codex"])]
        agent: Vec<String>,
    },
    /// Remove the flowmux-browser SKILL files from each agent's
    /// user-level location. The agent's top-level dir is left
    /// untouched.
    Uninstall {
        #[arg(long, value_parser = ["claude-code", "opencode", "codex"])]
        agent: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ThemeOp {
    /// Show where flowmux looks for its theme file and whether it exists.
    Path,
    /// Copy a theme file into `$XDG_CONFIG_HOME/flowmux/theme`. Accepts
    /// any file in flowmux's `key = value` theme format (the same shape
    /// as `resources/themes/example.theme`).
    Import {
        /// Source file path.
        src: PathBuf,
    },
}

#[derive(Subcommand)]
enum HooksOp {
    /// Install / refresh hook entries for every supported agent. Run
    /// once after `flowmux` is installed; safe to re-run after agent
    /// upgrades. Skips agents whose home directory is missing.
    Setup {
        /// Limit installation to specific agents. Omit to do all.
        #[arg(long, value_parser = ["claude", "codex", "opencode"])]
        agent: Vec<String>,
        /// Path of the `flowmux` binary that the installed hook
        /// commands should invoke. Defaults to the current `flowmux`
        /// binary on PATH (resolved at install time).
        #[arg(long)]
        flowmux_bin: Option<String>,
    },
    /// Remove flowmux's hook entries from every supported agent.
    Uninstall {
        #[arg(long, value_parser = ["claude", "codex", "opencode"])]
        agent: Vec<String>,
    },
    /// Print which agent config files flowmux currently owns hook
    /// entries in.
    Doctor,

    /// Claude Code lifecycle hook handler. Invoked by Claude Code via
    /// `~/.claude/settings.json` `hooks.<event>` after `flowmux hooks
    /// setup`. Reads stdin JSON, fires a desktop notification.
    Claude {
        #[command(subcommand)]
        event: ClaudeHookEvent,
    },
    /// Codex CLI lifecycle hook handler. Invoked by Codex via the
    /// `notify = [...]` config in `~/.codex/config.toml`. Codex
    /// passes the JSON event payload as the LAST positional argument,
    /// so we accept trailing args via `--` after the event name.
    Codex {
        #[command(subcommand)]
        event: AgentHookEvent,
    },
    /// OpenCode lifecycle hook handler. Invoked by the OpenCode
    /// `flowmux-session` plugin after `flowmux hooks setup`.
    Opencode {
        #[command(subcommand)]
        event: AgentHookEvent,
    },
}

#[derive(Subcommand)]
enum ClaudeHookEvent {
    /// Claude has finished a turn / its agent loop is idle.
    Stop,
    /// Claude needs the user (permission prompt, plan summary, …).
    Notification,
    /// New session started — registers the agent's presence (and PID
    /// from the wrapper shim) so its activity can be tracked.
    SessionStart,
    /// Session ended — clears the agent presence for this surface.
    SessionEnd,
    /// Claude is about to call a tool — marks the agent Running.
    PreToolUse,
    /// User submitted a prompt — marks the agent Running.
    PromptSubmit,
}

#[derive(Subcommand, Debug)]
enum AgentHookEvent {
    /// Agent finished a turn. Trailing args carry an optional JSON
    /// payload — Codex's `notify` config delivers the event JSON this
    /// way; Claude/OpenCode use stdin and leave args empty.
    Stop {
        /// Source pane id. The Flatpak OpenCode plugin passes this
        /// explicitly because `flatpak run` resets env to a minimal
        /// sandbox set, dropping FLOWMUX_PANE_ID before it reaches the
        /// in-sandbox CLI. Falls back to FLOWMUX_PANE_ID when omitted
        /// so non-sandboxed callers still work without changes.
        #[arg(long)]
        pane: Option<PaneId>,
        /// Source surface (tab) id. Same flatpak-boundary motivation
        /// as `--pane`; falls back to FLOWMUX_SURFACE_ID.
        #[arg(long)]
        surface: Option<SurfaceId>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Agent needs attention (permission prompt, error). Trailing args
    /// follow the same positional-or-stdin convention.
    Notification {
        #[arg(long)]
        pane: Option<PaneId>,
        #[arg(long)]
        surface: Option<SurfaceId>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Session started; flowmux currently no-ops on this event.
    SessionStart {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum WorkspaceOp {
    /// Create a new workspace rooted at `--root` (defaults to cwd).
    New {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List active workspace IDs.
    Ls,
    /// Print the currently-focused workspace id (or `(none)`).
    Current,
    /// Make a workspace the active one (like clicking its sidebar row).
    Focus { workspace: WorkspaceId },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Release builds stay quiet — only ERROR events surface so the
    // CLI never spams agent hooks (Claude/Codex/OpenCode) with info
    // chatter on stderr. `FLOWMUX_LOG` still overrides for debugging.
    let default_filter = if cfg!(debug_assertions) {
        "warn,flowmux=info"
    } else {
        "error"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOWMUX_LOG")
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    let cli = Cli::parse();
    let cmd = cli.cmd;

    // Local-only commands — handled before the daemon connect so they
    // work without a running flowmux GUI.
    match &cmd {
        Cmd::Theme { op } => return run_theme_op(op),
        Cmd::ListBrowsers => {
            for s in flowmux_cookies::discover_sources() {
                let detected = s.detect().is_some();
                println!("{:8}  detected={}", s.id().slug(), detected);
            }
            return Ok(());
        }
        Cmd::Agent { op } => return run_agent_op(op, cli.json),
        // Context/capability probes resolve from env + static data;
        // no daemon round-trip needed, so they work even before the GUI
        // is up and from inside `flatpak run` sandboxes.
        Cmd::Identify => return run_identify(cli.json),
        Cmd::Capabilities => return run_capabilities(cli.json),
        // `Hooks` runtime handlers (Claude / Codex / Opencode events)
        // talk to the daemon themselves; `Hooks::Setup`, `Uninstall`,
        // and `Doctor` are pure file edits with no daemon round-trip.
        Cmd::Hooks { op } => return run_hooks_op(op, cli.socket.clone()).await,
        // `Doctor` and `Fix` are top-level convenience commands that
        // wrap the per-subsystem doctor/install paths. `Doctor` may
        // ping the daemon; `Fix` never does.
        Cmd::Doctor => return run_doctor(cli.socket.clone(), cli.json).await,
        Cmd::Fix => return run_fix(cli.json),
        Cmd::PtyTee { .. } => {} // handled below, after the connect block
        _ => {}
    }

    // pty-tee owns its own (synchronous) PTY pump and a worker thread
    // for IPC; running it under the outer tokio runtime would prevent
    // the blocking poll() loop from ever yielding. Dispatch it here,
    // outside the daemon-connect path, so `Cmd::PtyTee` works even if
    // the daemon hasn't come up yet — the worker reconnects on its
    // own with backoff. The `matches!` gate is two lines so we can
    // destructure-by-value below without borrowing-then-moving `cmd`.
    if matches!(cmd, Cmd::PtyTee { .. }) {
        let Cmd::PtyTee {
            pane,
            surface,
            argv,
        } = cmd
        else {
            unreachable!("matches! just confirmed the variant")
        };
        // Escape the outer tokio runtime into a fresh OS thread so the
        // blocking poll() inside pty_tee::run does not starve runtime
        // workers; the IPC half lives on its own current-thread tokio
        // runtime spawned inside pty_tee::ipc_worker.
        let socket = cli.socket.clone();
        let handle = std::thread::spawn(move || pty_tee::run(pane, surface, socket, argv));
        let exit_code = handle
            .join()
            .map_err(|_| anyhow::anyhow!("pty-tee thread panicked"))??;
        std::process::exit(exit_code);
    }

    let socket = cli
        .socket
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from))
        .unwrap_or_else(paths::runtime_socket);
    let client = Client::connect(&socket)
        .await
        .with_context(|| "is the flowmux daemon running? try: flowmux")?;

    if let Cmd::NotifyStream { pane } = cmd {
        return notify_stream(&client, pane).await;
    }

    let json_mode = cli.json;
    let req = build_request(cmd)?;
    let resp = client.call(req).await?;
    print_response(&resp, json_mode)?;
    Ok(())
}

async fn notify_stream(client: &Client, pane: Option<PaneId>) -> anyhow::Result<()> {
    use flowmux_notify::{osc::parse_osc, OscExtractor};
    use std::sync::{Arc, Mutex};

    let pending: Arc<Mutex<Vec<(String, String, NotificationLevel)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let cell = pending.clone();
    let mut extractor = OscExtractor::new(move |payload| {
        if let Some(n) = parse_osc(payload) {
            cell.lock().unwrap().push((n.title, n.body, n.level));
        }
    });

    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut std::io::stdout(), &buf[..n])?;
        extractor.feed(&buf[..n]);
        let drained: Vec<_> = pending.lock().unwrap().drain(..).collect();
        for (title, body, level) in drained {
            let resp = client
                .call(Request::Notify {
                    pane,
                    surface: hooks::surface_from_env(),
                    title,
                    body,
                    level,
                })
                .await?;
            if let Response::Error(e) = resp {
                tracing::warn!(?e, "notify failed");
            }
        }
    }
    Ok(())
}

/// Map a `flowmux browser <op>` invocation to its IPC request. Every
/// arm maps 1:1 to an existing `Request::Browser*` variant, so the new
/// namespace and the hidden hyphenated aliases share one handler path.
fn browser_op_to_request(op: BrowserOp) -> Request {
    match op {
        BrowserOp::Open {
            url,
            right: _,
            down,
        } => {
            // `--down` splits horizontally; everything else (including the
            // default and `--right`) splits vertically, matching the prior
            // `flowmux browser <url>` behavior.
            let direction = if down {
                SplitDirection::Horizontal
            } else {
                SplitDirection::Vertical
            };
            // When invoked from a terminal that flowmux spawned, the PTY's
            // `FLOWMUX_PANE_ID` lets the daemon resolve "next to me" without
            // the caller passing a pane id explicitly.
            Request::BrowserOpen {
                url,
                target_pane: pane_from_env(),
                direction,
            }
        }
        BrowserOp::Snapshot { pane } => Request::BrowserSnapshot { pane },
        BrowserOp::Eval { pane, source } => Request::BrowserEval { pane, source },
        BrowserOp::Navigate { pane, url } => Request::BrowserNavigate { pane, url },
        BrowserOp::Back { pane } => Request::BrowserBack { pane },
        BrowserOp::Forward { pane } => Request::BrowserForward { pane },
        BrowserOp::Reload { pane } => Request::BrowserReload { pane },
        BrowserOp::Url { pane } => Request::BrowserUrl { pane },
        BrowserOp::Title { pane } => Request::BrowserTitle { pane },
        BrowserOp::Click { pane, target } => Request::BrowserClick { pane, target },
        BrowserOp::Fill {
            pane,
            target,
            value,
        } => Request::BrowserFill {
            pane,
            target,
            value,
        },
        BrowserOp::Select {
            pane,
            target,
            value,
        } => Request::BrowserSelect {
            pane,
            target,
            value,
        },
        BrowserOp::Scroll { pane, target, x, y } => Request::BrowserScroll { pane, target, x, y },
        BrowserOp::Type { pane, text } => Request::BrowserType { pane, text },
        BrowserOp::Press { pane, key } => Request::BrowserPress { pane, key },
        BrowserOp::Text { pane, target } => Request::BrowserText { pane, target },
        BrowserOp::Value { pane, target } => Request::BrowserValue { pane, target },
        BrowserOp::Attr { pane, target, name } => Request::BrowserAttr { pane, target, name },
        BrowserOp::DblClick { pane, target } => Request::BrowserDblClick { pane, target },
        BrowserOp::Hover { pane, target } => Request::BrowserHover { pane, target },
        BrowserOp::Focus { pane, target } => Request::BrowserFocus { pane, target },
        BrowserOp::Blur { pane, target } => Request::BrowserBlur { pane, target },
        BrowserOp::Check { pane, target } => Request::BrowserCheck { pane, target },
        BrowserOp::Uncheck { pane, target } => Request::BrowserUncheck { pane, target },
        BrowserOp::IsVisible { pane, target } => Request::BrowserIsVisible { pane, target },
        BrowserOp::IsEnabled { pane, target } => Request::BrowserIsEnabled { pane, target },
        BrowserOp::IsChecked { pane, target } => Request::BrowserIsChecked { pane, target },
        BrowserOp::Count { pane, selector } => Request::BrowserCount { pane, selector },
    }
}

/// The calling agent's flowmux context, resolved from the `FLOWMUX_*`
/// env vars that flowmux injects into every PTY. Empty/unset vars
/// become `None` (e.g. when run outside a flowmux pane).
#[derive(Debug, PartialEq, Eq)]
struct Identity {
    pane: Option<String>,
    surface: Option<String>,
    workspace: Option<String>,
    socket: Option<String>,
}

impl Identity {
    fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Identity {
            pane: get("FLOWMUX_PANE_ID"),
            surface: get("FLOWMUX_SURFACE_ID"),
            workspace: get("FLOWMUX_WORKSPACE_ID"),
            socket: get("FLOWMUX_SOCKET_PATH"),
        }
    }
}

fn run_identify(json: bool) -> anyhow::Result<()> {
    let id = Identity::from_env();
    if json {
        let v = serde_json::json!({
            "pane": id.pane,
            "surface": id.surface,
            "workspace": id.workspace,
            "socket": id.socket,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
        println!("pane:      {}", show(&id.pane));
        println!("surface:   {}", show(&id.surface));
        println!("workspace: {}", show(&id.workspace));
        println!("socket:    {}", show(&id.socket));
    }
    Ok(())
}

fn run_capabilities(json: bool) -> anyhow::Result<()> {
    let caps = flowmux_ipc::protocol::capabilities();
    if json {
        println!("{}", serde_json::to_string_pretty(&caps)?);
    } else {
        println!("browser verbs:");
        for v in &caps.browser_verbs {
            println!("  {v}");
        }
        println!("unsupported (CDP-only, return not_supported):");
        for u in &caps.unsupported {
            println!("  {u}");
        }
    }
    Ok(())
}

fn build_request(cmd: Cmd) -> anyhow::Result<Request> {
    Ok(match cmd {
        Cmd::Ping => Request::Ping,
        Cmd::Tree => Request::WorkspaceTree,
        Cmd::Workspace {
            op: WorkspaceOp::New { name, root },
        } => Request::WorkspaceCreate {
            name,
            root: root.map(Ok).unwrap_or_else(std::env::current_dir)?,
        },
        Cmd::Workspace {
            op: WorkspaceOp::Ls,
        } => Request::WorkspaceList,
        Cmd::Workspace {
            op: WorkspaceOp::Current,
        } => Request::WorkspaceCurrent,
        Cmd::Workspace {
            op: WorkspaceOp::Focus { workspace },
        } => Request::WorkspaceFocus { workspace },
        Cmd::Notify {
            pane,
            title,
            level,
            body,
        } => Request::Notify {
            pane: pane.or_else(pane_from_env),
            surface: hooks::surface_from_env(),
            title,
            body,
            level: parse_level(&level),
        },
        Cmd::NotifyComplete {
            agent,
            message,
            pane,
        } => {
            let body = message.unwrap_or_else(|| "task complete".to_string());
            Request::Notify {
                pane: pane.or_else(pane_from_env),
                surface: hooks::surface_from_env(),
                title: format!("{agent} ready"),
                body,
                level: NotificationLevel::AttentionNeeded,
            }
        }
        Cmd::Split { pane, right, down } => {
            let direction = if down {
                SplitDirection::Horizontal
            } else if right {
                SplitDirection::Vertical
            } else {
                SplitDirection::Vertical
            };
            Request::PaneSplit { pane, direction }
        }
        Cmd::SendKeys { pane, keys } => Request::PaneSendKeys { pane, keys },
        Cmd::SendKey { pane, key } => Request::PaneSendKeys {
            pane: resolve_pane(pane)?,
            keys: named_key_to_bytes(&key)?,
        },
        Cmd::ReadScreen { pane } => Request::PaneReadScreen {
            pane: resolve_pane(pane)?,
        },
        Cmd::FocusPane { pane } => Request::PaneFocus {
            pane: resolve_pane(pane)?,
        },
        Cmd::ClosePane { pane } => Request::PaneClose {
            pane: resolve_pane(pane)?,
        },
        Cmd::FocusTab { surface, pane } => Request::SurfaceFocus {
            pane: resolve_pane(pane)?,
            surface,
        },
        Cmd::CloseTab { surface, pane } => Request::SurfaceClose {
            pane: resolve_pane(pane)?,
            surface,
        },
        Cmd::Browser { op } => browser_op_to_request(op),
        Cmd::Ssh { target } => Request::SshConnect { target },
        Cmd::NotifyStream { .. } => unreachable!("handled before request build"),
        Cmd::ClaudeTeams { count, root, args } => Request::ClaudeTeams {
            count,
            args,
            root: root.map(Ok).unwrap_or_else(std::env::current_dir)?,
        },
        Cmd::BrowserSnapshot { pane } => Request::BrowserSnapshot { pane },
        Cmd::BrowserEval { pane, source } => Request::BrowserEval { pane, source },
        Cmd::BrowserNavigate { pane, url } => Request::BrowserNavigate { pane, url },
        Cmd::BrowserBack { pane } => Request::BrowserBack { pane },
        Cmd::BrowserForward { pane } => Request::BrowserForward { pane },
        Cmd::BrowserReload { pane } => Request::BrowserReload { pane },
        Cmd::BrowserUrl { pane } => Request::BrowserUrl { pane },
        Cmd::BrowserTitle { pane } => Request::BrowserTitle { pane },
        Cmd::BrowserClick { pane, target } => Request::BrowserClick { pane, target },
        Cmd::BrowserFill {
            pane,
            target,
            value,
        } => Request::BrowserFill {
            pane,
            target,
            value,
        },
        Cmd::BrowserSelect {
            pane,
            target,
            value,
        } => Request::BrowserSelect {
            pane,
            target,
            value,
        },
        Cmd::BrowserScroll { pane, target, x, y } => Request::BrowserScroll { pane, target, x, y },
        Cmd::BrowserType { pane, text } => Request::BrowserType { pane, text },
        Cmd::BrowserPress { pane, key } => Request::BrowserPress { pane, key },
        Cmd::BrowserText { pane, target } => Request::BrowserText { pane, target },
        Cmd::BrowserValue { pane, target } => Request::BrowserValue { pane, target },
        Cmd::BrowserAttr { pane, target, name } => Request::BrowserAttr { pane, target, name },
        Cmd::ImportCookies { from, domain } => Request::ImportCookies {
            source: from,
            domain,
        },
        Cmd::ListBrowsers => unreachable!("handled before request build"),
        Cmd::Theme { .. } => unreachable!("handled before request build"),
        Cmd::Agent { .. } => unreachable!("handled before request build"),
        Cmd::Hooks { .. } => unreachable!("handled before request build"),
        Cmd::Doctor => unreachable!("handled before request build"),
        Cmd::Fix => unreachable!("handled before request build"),
        Cmd::PtyTee { .. } => unreachable!("handled before request build"),
        Cmd::Identify => unreachable!("handled before request build"),
        Cmd::Capabilities => unreachable!("handled before request build"),
    })
}

/// Dispatch every `flowmux hooks <op>` invocation. Setup/Doctor/Uninstall
/// only touch user config files and never need the daemon. The runtime
/// hook events (Claude/Codex/Opencode) talk to the daemon themselves.
async fn run_hooks_op(op: &HooksOp, socket: Option<PathBuf>) -> anyhow::Result<()> {
    use hook_install::HookInstallStatus;
    match op {
        HooksOp::Setup { agent, flowmux_bin } => {
            let bin = flowmux_bin
                .clone()
                .or_else(resolve_self_bin)
                .unwrap_or_else(|| "flowmux".to_string());
            let targets = parse_hook_targets(agent)?;
            for t in targets {
                match hook_install::install(t, &bin) {
                    Ok(report) => print_hook_report(&report),
                    Err(e) => println!("{:8}  error: {e:#}", t.slug()),
                }
            }
            Ok(())
        }
        HooksOp::Uninstall { agent } => {
            let targets = parse_hook_targets(agent)?;
            for t in targets {
                match hook_install::uninstall(t) {
                    Ok(report) => print_hook_report(&report),
                    Err(e) => println!("{:8}  error: {e:#}", t.slug()),
                }
            }
            Ok(())
        }
        HooksOp::Doctor => {
            run_hooks_doctor(socket.clone()).await;
            // The `let _` pin is intentional: it forces the compiler
            // to keep the `HookInstallStatus` variants reachable so a
            // future refactor cannot silently drop them.
            let _ = HookInstallStatus::Installed;
            Ok(())
        }
        HooksOp::Claude { event } => run_claude_hook_event(event, socket).await,
        HooksOp::Codex { event } => run_generic_agent_hook_event("Codex", event, socket).await,
        HooksOp::Opencode { event } => {
            run_generic_agent_hook_event("OpenCode", event, socket).await
        }
    }
}

/// Full diagnostic dump that one command captures: sandbox state,
/// resolved socket + connect outcome, per-agent install status, hook
/// plugin checksums, and the tail of `notify-debug.log`. The single
/// goal is "run this once on the failing host and paste the output."
async fn run_hooks_doctor(socket: Option<PathBuf>) {
    use hook_install::HookTarget;

    println!("=== flowmux hooks doctor ===");

    // 1. Sandbox + env
    let sandbox = flowmux_config::paths::is_flatpak_sandbox();
    println!(
        "sandbox          : {} (FLATPAK_ID={:?})",
        sandbox,
        std::env::var_os("FLATPAK_ID")
    );
    println!("HOME             : {:?}", std::env::var_os("HOME"));
    println!(
        "XDG_RUNTIME_DIR  : {:?}",
        std::env::var_os("XDG_RUNTIME_DIR")
    );
    println!(
        "XDG_CONFIG_HOME  : {:?}",
        std::env::var_os("XDG_CONFIG_HOME")
    );

    // 2. Socket resolution + reachability
    let env_socket = socket
        .clone()
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET_PATH").map(PathBuf::from))
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from));
    let resolved = env_socket
        .clone()
        .unwrap_or_else(flowmux_config::paths::runtime_socket);
    println!(
        "socket primary   : {resolved:?} (source={})",
        if env_socket.is_some() {
            "env"
        } else {
            "fallback"
        }
    );
    println!(
        "  exists?        : {} symlink_target?={:?}",
        resolved.exists(),
        std::fs::read_link(&resolved).ok()
    );

    if let Some(cache) = flowmux_config::paths::host_visible_cache_dir() {
        println!("cache dir        : {cache:?} exists={}", cache.exists());
        if let Ok(entries) = std::fs::read_dir(&cache) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name_s = name.to_string_lossy();
                if name_s.starts_with("flowmux-") && name_s.ends_with(".sock") {
                    println!("  per-pid sock   : {:?}", e.path());
                }
            }
        }
    }

    // Live connect probe through the same path the OpenCode plugin
    // would take (envless, fallback resolver, scan included).
    println!("daemon ping      : ...");
    match hooks::connect_daemon(socket).await {
        Some(client) => match client.call(flowmux_ipc::protocol::Request::Ping).await {
            Ok(resp) => println!("  -> ok ({resp:?})"),
            Err(e) => println!("  -> connected but rpc failed: {e}"),
        },
        None => println!("  -> UNREACHABLE (see notify-debug.log tail)"),
    }

    // 3. Per-agent install state
    println!();
    println!("--- agents ---");
    for t in HookTarget::ALL {
        let label = match t {
            HookTarget::Claude => "claude",
            HookTarget::Codex => "codex",
            HookTarget::OpenCode => "opencode",
        };
        let entry = hook_install::check(*t);
        println!("{label:8}  status={:?}", entry.status);
        for p in &entry.paths {
            let info = if p.exists() {
                let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                format!("exists len={len}B")
            } else {
                "missing".into()
            };
            println!("           {p:?} ({info})");
        }
    }

    // 4. Tail the unified debug log
    println!();
    println!("--- notify-debug.log (last 60 lines) ---");
    if let Some(log_path) = flowmux_config::debug_log::log_path() {
        println!("path: {log_path:?}");
        match std::fs::read_to_string(&log_path) {
            Ok(body) => {
                let lines: Vec<&str> = body.lines().collect();
                let start = lines.len().saturating_sub(60);
                for line in &lines[start..] {
                    println!("  {line}");
                }
            }
            Err(e) => println!("  (could not read: {e})"),
        }
    } else {
        println!("  (no HOME — debug log disabled)");
    }
}

fn parse_hook_targets(agents: &[String]) -> anyhow::Result<Vec<hook_install::HookTarget>> {
    if agents.is_empty() {
        return Ok(hook_install::HookTarget::ALL.to_vec());
    }
    agents
        .iter()
        .map(|s| {
            hook_install::HookTarget::from_slug(s)
                .ok_or_else(|| anyhow::anyhow!("unknown hook target: {s}"))
        })
        .collect()
}

fn print_hook_report(report: &hook_install::HookInstallReport) {
    let label = report.target.slug();
    match &report.status {
        hook_install::HookInstallStatus::Installed if report.touched_paths.is_empty() => {
            println!("{label:8}  ok");
        }
        hook_install::HookInstallStatus::Installed => {
            for p in &report.touched_paths {
                println!("{label:8}  wrote  {}", p.display());
            }
        }
        hook_install::HookInstallStatus::Skipped => {
            println!("{label:8}  skipped (agent not installed)");
        }
    }
}

/// Best-effort discovery of the running `flowmux` binary path so the
/// command lines we drop into `~/.claude/settings.json` etc. survive
/// when the user has multiple `flowmux` builds on PATH.
fn resolve_self_bin() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

async fn run_claude_hook_event(
    event: &ClaudeHookEvent,
    socket: Option<PathBuf>,
) -> anyhow::Result<()> {
    use flowmux_core::AgentActivity::{Idle, NeedsInput, Running};
    use hooks::*;
    let input = read_claude_hook_input();
    let pane = pane_from_env();
    let surface = surface_from_env();
    let pid = pid_from_env();
    // Most events carry exactly one request; Stop/Notification carry two
    // (the user-facing toast *and* the activity flip) so the existing
    // "ready" notification keeps firing alongside the new live-status
    // tracking.
    let mut reqs: Vec<_> = Vec::new();
    match event {
        ClaudeHookEvent::Stop => {
            let body = input.last_assistant_message.as_deref();
            reqs.push(build_stop_notify("Claude", body, pane, surface));
            reqs.push(build_activity_update(
                "claude",
                Some(Idle),
                pid,
                pane,
                surface,
            ));
        }
        ClaudeHookEvent::Notification => {
            let msg = input.message.as_deref();
            reqs.push(build_notification_notify("Claude", msg, pane, surface));
            reqs.push(build_activity_update(
                "claude",
                Some(NeedsInput),
                pid,
                pane,
                surface,
            ));
        }
        // SessionStart registers the agent's presence (and PID, for the
        // liveness sweep) without claiming it is working yet.
        ClaudeHookEvent::SessionStart => {
            reqs.push(build_activity_update(
                "claude",
                Some(Idle),
                pid,
                pane,
                surface,
            ));
        }
        // A new prompt or an imminent tool call means the agent is
        // actively working this turn — and clears any "needs input".
        ClaudeHookEvent::PromptSubmit | ClaudeHookEvent::PreToolUse => {
            reqs.push(build_activity_update(
                "claude",
                Some(Running),
                pid,
                pane,
                surface,
            ));
        }
        // Real teardown (covers Ctrl+C, where Stop never fires). The
        // daemon PID sweep is the backstop for a hard kill that skips
        // SessionEnd too.
        ClaudeHookEvent::SessionEnd => {
            reqs.push(build_activity_update("claude", None, pid, pane, surface));
        }
    };
    if let Some(client) = hooks::connect_daemon(socket).await {
        for req in reqs {
            hooks::send_best_effort(&client, req).await;
        }
    }
    Ok(())
}

async fn run_generic_agent_hook_event(
    agent: &str,
    event: &AgentHookEvent,
    socket: Option<PathBuf>,
) -> anyhow::Result<()> {
    use hooks::*;
    let env_pane = pane_from_env();
    let env_surface = surface_from_env();
    let (cli_pane, cli_surface) = match event {
        AgentHookEvent::Stop { pane, surface, .. } => (*pane, *surface),
        AgentHookEvent::Notification { pane, surface, .. } => (*pane, *surface),
        AgentHookEvent::SessionStart { .. } => (None, None),
    };
    // CLI flags win over env so the OpenCode Flatpak plugin (which
    // passes them explicitly across the sandbox boundary) is the
    // single source of truth for pane/surface attribution. Non-flatpak
    // callers leave the flags unset and we recover the values from
    // env, preserving the legacy code path.
    let pane = cli_pane.or(env_pane);
    let surface = cli_surface.or(env_surface);
    flowmux_config::notify_debug!(
        "cli/hook",
        "entry agent={agent:?} event={event:?} cli_pane={cli_pane:?} cli_surface={cli_surface:?} env_pane={env_pane:?} env_surface={env_surface:?} resolved_pane={pane:?} resolved_surface={surface:?} socket_arg={socket:?}"
    );
    use flowmux_core::AgentActivity::{Idle, NeedsInput};
    let pid = hooks::pid_from_env();
    let mut reqs: Vec<_> = Vec::new();
    match event {
        AgentHookEvent::Stop { args, .. } => {
            let input = read_codex_hook_input(args);
            let body = input.last_assistant_message.as_deref();
            reqs.push(build_stop_notify(agent, body, pane, surface));
            reqs.push(build_activity_update(agent, Some(Idle), pid, pane, surface));
        }
        AgentHookEvent::Notification { args, .. } => {
            let input = read_codex_hook_input(args);
            let msg = input.message.as_deref();
            reqs.push(build_notification_notify(agent, msg, pane, surface));
            reqs.push(build_activity_update(
                agent,
                Some(NeedsInput),
                pid,
                pane,
                surface,
            ));
        }
        // Codex / OpenCode register presence on session start (no
        // wrapper PID for these, so the daemon clears them via Stop→Idle
        // plus the liveness sweep rather than a SessionEnd hook).
        AgentHookEvent::SessionStart { .. } => {
            reqs.push(build_activity_update(agent, Some(Idle), pid, pane, surface));
        }
    };
    match hooks::connect_daemon(socket).await {
        Some(client) => {
            for req in reqs {
                hooks::send_best_effort(&client, req).await;
            }
        }
        None => {
            flowmux_config::notify_debug!("cli/hook", "daemon not reachable — request dropped");
        }
    }
    Ok(())
}

fn run_agent_op(op: &AgentOp, json: bool) -> anyhow::Result<()> {
    let home = agent::resolved_home()?;
    let codex_home = agent::resolved_codex_home();

    let parse_targets = |slugs: &[String]| -> anyhow::Result<Vec<agent::Target>> {
        if slugs.is_empty() {
            Ok(agent::Target::ALL.to_vec())
        } else {
            slugs
                .iter()
                .map(|s| {
                    agent::Target::from_slug(s).ok_or_else(|| anyhow::anyhow!("unknown agent: {s}"))
                })
                .collect()
        }
    };

    match op {
        AgentOp::Install {
            agent: slugs,
            force,
        } => {
            let targets = parse_targets(slugs)?;
            let outcomes = agent::install_all(&targets, &home, codex_home.as_deref(), *force)?;
            if json {
                let body = outcomes
                    .iter()
                    .map(|(t, p, o)| {
                        serde_json::json!({
                            "agent": t.slug(),
                            "path": p.display().to_string(),
                            "outcome": match o {
                                agent::InstallOutcome::Written => "written",
                                agent::InstallOutcome::AlreadyUpToDate => "already_up_to_date",
                            },
                        })
                    })
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string(&body)?);
            } else {
                for (t, p, o) in &outcomes {
                    let label = match o {
                        agent::InstallOutcome::Written => "wrote   ",
                        agent::InstallOutcome::AlreadyUpToDate => "up-to-date",
                    };
                    println!("{label}  {:12}  {}", t.slug(), p.display());
                }
            }
            Ok(())
        }
        AgentOp::Doctor { agent: slugs } => {
            let targets = parse_targets(slugs)?;
            let report = agent::doctor_all(&targets, &home, codex_home.as_deref());
            let any_bad = report
                .iter()
                .any(|e| !matches!(e.status, agent::DoctorStatus::Ok));
            if json {
                let body = report
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "agent": e.target.slug(),
                            "path": e.path.display().to_string(),
                            "status": e.status.label(),
                        })
                    })
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string(&body)?);
            } else {
                for entry in &report {
                    println!(
                        "{:9}  {:12}  {}",
                        entry.status.label(),
                        entry.target.slug(),
                        entry.path.display()
                    );
                }
            }
            if any_bad {
                std::process::exit(1);
            }
            Ok(())
        }
        AgentOp::Uninstall { agent: slugs } => {
            let targets = parse_targets(slugs)?;
            for t in targets {
                let path = t.resolved_install_path(&home, codex_home.as_deref());
                let outcome = agent::uninstall_one(&path)?;
                let label = match outcome {
                    agent::UninstallOutcome::Removed => "removed",
                    agent::UninstallOutcome::AlreadyAbsent => "absent ",
                };
                println!("{label}  {:12}  {}", t.slug(), path.display());
            }
            Ok(())
        }
    }
}

/// `flowmux doctor` — render the unified report and exit non-zero
/// if any row needs the user to do something.
async fn run_doctor(socket: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    let home = agent::resolved_home()?;
    let codex_home = agent::resolved_codex_home();
    let report = doctor::collect(&home, codex_home.as_deref(), socket).await;
    if json {
        println!("{}", doctor::render_json(&report)?);
    } else {
        print!("{}", doctor::render_text(&report));
    }
    if report.has_problems() {
        std::process::exit(1);
    }
    Ok(())
}

/// `flowmux fix` — re-install everything the doctor would flag.
fn run_fix(json: bool) -> anyhow::Result<()> {
    let home = agent::resolved_home()?;
    let codex_home = agent::resolved_codex_home();
    let bin = resolve_self_bin().unwrap_or_else(|| "flowmux".to_string());
    let report = doctor::run_fix(&home, codex_home.as_deref(), &bin);
    if json {
        println!("{}", doctor::render_fix_json(&report)?);
    } else {
        print!("{}", doctor::render_fix_text(&report));
    }
    if report.has_problems() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_theme_op(op: &ThemeOp) -> anyhow::Result<()> {
    match op {
        ThemeOp::Path => {
            match flowmux_config::theme::user_theme_path() {
                Some(p) => {
                    let exists = p.is_file();
                    println!("{}  exists={exists}", p.display());
                }
                None => println!("(XDG config dir unavailable)"),
            }
            Ok(())
        }
        ThemeOp::Import { src } => {
            let dest = flowmux_config::theme::import_from(src)
                .with_context(|| format!("importing {}", src.display()))?;
            println!("imported  {} → {}", src.display(), dest.display());
            println!("relaunch flowmux to apply.");
            Ok(())
        }
    }
}

fn parse_level(s: &str) -> NotificationLevel {
    match s {
        "attention" => NotificationLevel::AttentionNeeded,
        "error" => NotificationLevel::Error,
        _ => NotificationLevel::Info,
    }
}

fn print_response(r: &Response, json_mode: bool) -> anyhow::Result<()> {
    // `flowmux tree` gets a human-readable indented view in text mode;
    // --json still emits the structured payload for scripts.
    if !json_mode {
        if let Response::Tree { workspaces } = r {
            print!("{}", render_tree(workspaces));
            return Ok(());
        }
        if let Response::WorkspaceCurrent { id } = r {
            match id {
                Some(id) => println!("{id}"),
                None => println!("(none)"),
            }
            return Ok(());
        }
        if let Response::ScreenContents { text } = r {
            // Raw terminal text — print as-is (already newline-terminated
            // per row), no extra framing.
            print!("{text}");
            return Ok(());
        }
    }
    let s = if json_mode {
        // Single-line JSON — easier to parse from agent scripts
        // (`jq -r .pane` etc.). Mirrors cmux's `--json` shape.
        serde_json::to_string(r)?
    } else {
        serde_json::to_string_pretty(r)?
    };
    println!("{s}");
    Ok(())
}

/// Render `flowmux tree` as an indented workspace → leaf-pane → tab
/// view. The active tab in each pane is marked with `*`.
fn render_tree(workspaces: &[flowmux_ipc::protocol::TreeWorkspace]) -> String {
    use std::fmt::Write as _;
    if workspaces.is_empty() {
        return "(no workspaces)\n".to_string();
    }
    let mut out = String::new();
    for ws in workspaces {
        let _ = writeln!(
            out,
            "workspace {} \"{}\" ({})",
            ws.id,
            ws.name,
            ws.root.display()
        );
        for pane in &ws.panes {
            let _ = writeln!(out, "  pane {}", pane.id);
            for tab in &pane.tabs {
                let marker = if tab.active { '*' } else { ' ' };
                let _ = writeln!(
                    out,
                    "    {marker} [{}] {} \"{}\"",
                    tab.kind, tab.id, tab.title
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_notify_level_strings_to_core_levels() {
        assert_eq!(parse_level("info"), NotificationLevel::Info);
        assert_eq!(parse_level("attention"), NotificationLevel::AttentionNeeded);
        assert_eq!(parse_level("error"), NotificationLevel::Error);
        assert_eq!(parse_level("unknown"), NotificationLevel::Info);
    }

    #[test]
    fn notify_parses_to_gui_routed_request_with_surface_env() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        unsafe {
            std::env::set_var("FLOWMUX_SURFACE_ID", surface.to_string());
        }

        let req = build_request(Cmd::Notify {
            pane: Some(pane),
            title: "Build".into(),
            level: "error".into(),
            body: "failed".into(),
        });

        unsafe {
            std::env::remove_var("FLOWMUX_SURFACE_ID");
        }

        assert!(matches!(
            req.unwrap(),
            Request::Notify {
                pane: got_pane,
                surface: got_surface,
                title,
                body,
                level,
            } if got_pane == Some(pane)
                && got_surface == Some(surface)
                && title == "Build"
                && body == "failed"
                && level == NotificationLevel::Error
        ));
    }

    #[test]
    fn notify_complete_uses_attention_ready_payload_and_env_pane() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
            std::env::remove_var("FLOWMUX_SURFACE_ID");
        }

        let req = build_request(Cmd::NotifyComplete {
            agent: "Codex".into(),
            message: Some("done".into()),
            pane: None,
        });

        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }

        assert!(matches!(
            req.unwrap(),
            Request::Notify {
                pane: got_pane,
                surface: None,
                title,
                body,
                level,
            } if got_pane == Some(pane)
                && title == "Codex ready"
                && body == "done"
                && level == NotificationLevel::AttentionNeeded
        ));
    }

    #[test]
    fn split_defaults_to_right_when_no_direction_flag_is_set() {
        let pane = PaneId::new();
        let req = build_request(Cmd::Split {
            pane,
            right: false,
            down: false,
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::PaneSplit { pane: got, direction: SplitDirection::Vertical }
                if got == pane
        ));
    }

    #[test]
    fn split_down_maps_to_horizontal_direction() {
        let pane = PaneId::new();
        let req = build_request(Cmd::Split {
            pane,
            right: false,
            down: true,
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::PaneSplit { pane: got, direction: SplitDirection::Horizontal }
                if got == pane
        ));
    }

    #[test]
    fn workspace_create_uses_explicit_root_and_name() {
        let root = PathBuf::from("/tmp/flowmux-cli-test");
        let req = build_request(Cmd::Workspace {
            op: WorkspaceOp::New {
                name: Some("demo".into()),
                root: Some(root.clone()),
            },
        })
        .unwrap();

        assert!(matches!(
            req,
            Request::WorkspaceCreate { name, root: got_root }
                if name.as_deref() == Some("demo") && got_root == root
        ));
    }

    #[test]
    fn browser_and_cookie_commands_map_to_ipc_requests() {
        let pane = PaneId::new();
        let snapshot = build_request(Cmd::BrowserSnapshot { pane }).unwrap();
        assert!(matches!(snapshot, Request::BrowserSnapshot { pane: got } if got == pane));

        let eval = build_request(Cmd::BrowserEval {
            pane,
            source: "document.title".into(),
        })
        .unwrap();
        assert!(matches!(
            eval,
            Request::BrowserEval { pane: got, source } if got == pane && source == "document.title"
        ));

        let import = build_request(Cmd::ImportCookies {
            from: "firefox".into(),
            domain: Some("example.com".into()),
        })
        .unwrap();
        assert!(matches!(
            import,
            Request::ImportCookies { source, domain }
                if source == "firefox" && domain.as_deref() == Some("example.com")
        ));
    }

    /// The `flowmux browser <op> pane:<uuid> …` namespace is the
    /// documented agent contract (`AGENTS.md`). Parse the literal argv
    /// the docs show — including the `pane:` prefix — and confirm it
    /// reaches the right IPC request without translation.
    #[test]
    fn browser_namespace_parses_documented_examples() {
        let pane = PaneId::new();
        let pane_arg = format!("pane:{pane}");

        let cli = Cli::try_parse_from(["flowmuxctl", "browser", "snapshot", &pane_arg])
            .expect("`browser snapshot pane:<uuid>` must parse");
        let req = build_request(cli.cmd).unwrap();
        assert!(matches!(req, Request::BrowserSnapshot { pane: got } if got == pane));

        let cli = Cli::try_parse_from(["flowmuxctl", "browser", "click", &pane_arg, "e3"])
            .expect("`browser click pane:<uuid> e3` must parse");
        let req = build_request(cli.cmd).unwrap();
        assert!(
            matches!(req, Request::BrowserClick { pane: got, target } if got == pane && target == "e3")
        );

        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "browser",
            "fill",
            &pane_arg,
            "e1",
            "user@example.com",
        ])
        .expect("`browser fill pane:<uuid> e1 <value>` must parse");
        let req = build_request(cli.cmd).unwrap();
        assert!(matches!(
            req,
            Request::BrowserFill { pane: got, target, value }
                if got == pane && target == "e1" && value == "user@example.com"
        ));
    }

    /// The `open` verb keeps the env-based "next to me" fallback the old
    /// bare `flowmux browser <url>` form had.
    #[test]
    fn browser_open_namespace_uses_pane_env_fallback() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
        }
        let cli =
            Cli::try_parse_from(["flowmuxctl", "browser", "open", "https://example.com"]).unwrap();
        let req = build_request(cli.cmd).unwrap();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            req,
            Request::BrowserOpen { url, target_pane, direction: _ }
                if url == "https://example.com" && target_pane == Some(pane)
        ));
    }

    /// Every Phase-5 verb that previously existed only in IPC must now be
    /// reachable from the CLI namespace and map to its request 1:1.
    #[test]
    fn browser_namespace_exposes_phase5_verbs() {
        let pane = PaneId::new();
        let pane_arg = format!("pane:{pane}");
        // (argv verb, then assert the resulting Request variant)
        macro_rules! parse_build {
            ($($arg:expr),+ $(,)?) => {{
                let cli = Cli::try_parse_from(["flowmuxctl", "browser", $($arg),+])
                    .expect("verb must parse");
                build_request(cli.cmd).unwrap()
            }};
        }

        assert!(matches!(
            parse_build!("dblclick", &pane_arg, "e3"),
            Request::BrowserDblClick { target, .. } if target == "e3"
        ));
        assert!(matches!(
            parse_build!("hover", &pane_arg, "e3"),
            Request::BrowserHover { .. }
        ));
        assert!(matches!(
            parse_build!("focus", &pane_arg, "e3"),
            Request::BrowserFocus { .. }
        ));
        assert!(matches!(
            parse_build!("blur", &pane_arg, "e3"),
            Request::BrowserBlur { .. }
        ));
        assert!(matches!(
            parse_build!("check", &pane_arg, "e3"),
            Request::BrowserCheck { .. }
        ));
        assert!(matches!(
            parse_build!("uncheck", &pane_arg, "e3"),
            Request::BrowserUncheck { .. }
        ));
        assert!(matches!(
            parse_build!("is-visible", &pane_arg, "e3"),
            Request::BrowserIsVisible { .. }
        ));
        assert!(matches!(
            parse_build!("is-enabled", &pane_arg, "e3"),
            Request::BrowserIsEnabled { .. }
        ));
        assert!(matches!(
            parse_build!("is-checked", &pane_arg, "e7"),
            Request::BrowserIsChecked { target, .. } if target == "e7"
        ));
        assert!(matches!(
            parse_build!("count", &pane_arg, ".result-row"),
            Request::BrowserCount { selector, .. } if selector == ".result-row"
        ));
    }

    #[test]
    fn named_key_to_bytes_maps_keys_and_passthrough() {
        assert_eq!(named_key_to_bytes("Enter").unwrap(), "\r");
        assert_eq!(named_key_to_bytes("Tab").unwrap(), "\t");
        assert_eq!(named_key_to_bytes("Escape").unwrap(), "\x1b");
        assert_eq!(named_key_to_bytes("ArrowUp").unwrap(), "\x1b[A");
        assert_eq!(named_key_to_bytes("PageDown").unwrap(), "\x1b[6~");
        // single char passes through
        assert_eq!(named_key_to_bytes("q").unwrap(), "q");
        assert_eq!(named_key_to_bytes(":").unwrap(), ":");
        // unknown multi-char name errors rather than guessing
        assert!(named_key_to_bytes("Wat").is_err());
    }

    #[test]
    fn send_key_parses_named_key_and_maps_to_send_keys() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
        }
        let built = build_request(
            Cli::try_parse_from(["flowmuxctl", "send-key", "Enter"])
                .unwrap()
                .cmd,
        );
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            built.unwrap(),
            Request::PaneSendKeys { pane: got, keys } if got == pane && keys == "\r"
        ));
    }

    #[test]
    fn read_screen_parses_pane_arg_and_env_fallback() {
        let pane = PaneId::new();
        // Explicit pane: arg.
        let cli =
            Cli::try_parse_from(["flowmuxctl", "read-screen", &format!("pane:{pane}")]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::PaneReadScreen { pane: got } if got == pane
        ));

        // Omitted pane falls back to FLOWMUX_PANE_ID.
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
        }
        let cli = Cli::try_parse_from(["flowmuxctl", "read-screen"]).unwrap();
        let built = build_request(cli.cmd);
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            built.unwrap(),
            Request::PaneReadScreen { pane: got } if got == pane
        ));
    }

    #[test]
    fn focus_tab_and_close_tab_parse_and_map() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
        }
        let focus = build_request(
            Cli::try_parse_from(["flowmuxctl", "focus-tab", &surface.to_string()])
                .unwrap()
                .cmd,
        );
        let close = build_request(
            Cli::try_parse_from([
                "flowmuxctl",
                "close-tab",
                &surface.to_string(),
                "--pane",
                &format!("pane:{pane}"),
            ])
            .unwrap()
            .cmd,
        );
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            focus.unwrap(),
            Request::SurfaceFocus { pane: gp, surface: gs } if gp == pane && gs == surface
        ));
        assert!(matches!(
            close.unwrap(),
            Request::SurfaceClose { pane: gp, surface: gs } if gp == pane && gs == surface
        ));
    }

    #[test]
    fn focus_pane_and_close_pane_parse_and_map() {
        let pane = PaneId::new();
        let cli =
            Cli::try_parse_from(["flowmuxctl", "focus-pane", &format!("pane:{pane}")]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::PaneFocus { pane: got } if got == pane
        ));
        let cli = Cli::try_parse_from(["flowmuxctl", "close-pane", &pane.to_string()]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::PaneClose { pane: got } if got == pane
        ));
    }

    #[test]
    fn workspace_current_parses_and_maps_to_request() {
        let cli = Cli::try_parse_from(["flowmuxctl", "workspace", "current"]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::WorkspaceCurrent
        ));
    }

    #[test]
    fn workspace_focus_parses_and_maps_to_request() {
        let ws = flowmux_core::WorkspaceId::new();
        let cli =
            Cli::try_parse_from(["flowmuxctl", "workspace", "focus", &ws.to_string()]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::WorkspaceFocus { workspace } if workspace == ws
        ));
    }

    #[test]
    fn ssh_parses_and_maps_to_connect_request() {
        let cli = Cli::try_parse_from(["flowmuxctl", "ssh", "alice@example.com"]).unwrap();

        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::SshConnect { target } if target == "alice@example.com"
        ));
    }

    #[test]
    fn tree_parses_and_maps_to_workspace_tree_request() {
        let cli = Cli::try_parse_from(["flowmuxctl", "tree"]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::WorkspaceTree
        ));
    }

    #[test]
    fn render_tree_marks_active_tab_and_indents() {
        use flowmux_ipc::protocol::{TreePane, TreeTab, TreeWorkspace};
        let pane = PaneId::new();
        let t1 = SurfaceId::new();
        let t2 = SurfaceId::new();
        let ws = TreeWorkspace {
            id: flowmux_core::WorkspaceId::new(),
            name: "demo".into(),
            root: "/tmp/demo".into(),
            panes: vec![TreePane {
                id: pane,
                tabs: vec![
                    TreeTab {
                        id: t1,
                        title: "shell".into(),
                        kind: "terminal".into(),
                        active: false,
                    },
                    TreeTab {
                        id: t2,
                        title: "docs".into(),
                        kind: "browser".into(),
                        active: true,
                    },
                ],
            }],
        };
        let out = render_tree(std::slice::from_ref(&ws));
        assert!(out.contains("workspace "));
        assert!(out.contains("\"demo\""));
        assert!(out.contains(&format!("pane {pane}")));
        // Active tab marked with '*', inactive with a space.
        assert!(out.contains(&format!("* [browser] {t2} \"docs\"")));
        assert!(out.contains(&format!("  [terminal] {t1} \"shell\"")));
        assert_eq!(render_tree(&[]), "(no workspaces)\n");
    }

    #[test]
    fn identify_and_capabilities_parse_as_local_commands() {
        assert!(matches!(
            Cli::try_parse_from(["flowmuxctl", "identify"]).unwrap().cmd,
            Cmd::Identify
        ));
        assert!(matches!(
            Cli::try_parse_from(["flowmuxctl", "capabilities"])
                .unwrap()
                .cmd,
            Cmd::Capabilities
        ));
    }

    #[test]
    fn identity_from_env_resolves_flowmux_context() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
            std::env::set_var("FLOWMUX_WORKSPACE_ID", "ws-1");
            std::env::set_var("FLOWMUX_SOCKET_PATH", "/run/flowmux.sock");
            // An empty var must read as None, not Some("").
            std::env::set_var("FLOWMUX_SURFACE_ID", "");
        }
        let id = Identity::from_env();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
            std::env::remove_var("FLOWMUX_WORKSPACE_ID");
            std::env::remove_var("FLOWMUX_SOCKET_PATH");
            std::env::remove_var("FLOWMUX_SURFACE_ID");
        }
        assert_eq!(id.pane.as_deref(), Some(pane.to_string().as_str()));
        assert_eq!(id.workspace.as_deref(), Some("ws-1"));
        assert_eq!(id.socket.as_deref(), Some("/run/flowmux.sock"));
        assert_eq!(id.surface, None);
    }

    /// The hidden hyphenated aliases must keep mapping to the same
    /// requests so pre-namespace scripts/hooks do not break.
    #[test]
    fn browser_hyphenated_aliases_still_work() {
        let pane = PaneId::new();
        let cli =
            Cli::try_parse_from(["flowmuxctl", "browser-click", &pane.to_string(), "e3"]).unwrap();
        let req = build_request(cli.cmd).unwrap();
        assert!(matches!(req, Request::BrowserClick { target, .. } if target == "e3"));
    }

    /// Serialize every test that reads/writes FLOWMUX_PANE_ID — cargo
    /// runs tests in parallel within a single binary, and they share
    /// process-global env. Without this lock, a `set_var` from one
    /// test races a `remove_var` from another and one of them sees
    /// the wrong value.
    fn flowmux_pane_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn browser_open_no_flags_defaults_to_right_split() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }

        let req = build_request(Cmd::Browser {
            op: BrowserOp::Open {
                url: "https://example.com".into(),
                right: false,
                down: false,
            },
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserOpen {
                url,
                target_pane: None,
                direction: SplitDirection::Vertical,
            } if url == "https://example.com"
        ));
    }

    #[test]
    fn browser_open_picks_target_pane_from_flowmux_pane_id_env() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        let pane_str = pane.to_string();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", &pane_str);
        }

        let req = build_request(Cmd::Browser {
            op: BrowserOp::Open {
                url: "https://example.com".into(),
                right: false,
                down: false,
            },
        })
        .unwrap();

        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }

        assert!(matches!(
            req,
            Request::BrowserOpen { target_pane: Some(got), .. } if got == pane
        ));
    }

    #[test]
    fn browser_open_ignores_invalid_flowmux_pane_id_env() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", "not-a-uuid");
        }

        let req = build_request(Cmd::Browser {
            op: BrowserOp::Open {
                url: "https://example.com".into(),
                right: false,
                down: false,
            },
        })
        .unwrap();

        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }

        assert!(matches!(
            req,
            Request::BrowserOpen {
                target_pane: None,
                ..
            }
        ));
    }

    #[test]
    fn browser_open_with_right_is_vertical_split() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }

        let req = build_request(Cmd::Browser {
            op: BrowserOp::Open {
                url: "https://a.test".into(),
                right: true,
                down: false,
            },
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserOpen {
                target_pane: None,
                direction: SplitDirection::Vertical,
                ..
            }
        ));
    }

    #[test]
    fn browser_open_with_down_is_horizontal_split() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }

        let req = build_request(Cmd::Browser {
            op: BrowserOp::Open {
                url: "https://a.test".into(),
                right: false,
                down: true,
            },
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserOpen {
                target_pane: None,
                direction: SplitDirection::Horizontal,
                ..
            }
        ));
    }

    #[test]
    fn browser_navigate_maps_pane_and_url() {
        let pane = PaneId::new();
        let req = build_request(Cmd::BrowserNavigate {
            pane,
            url: "https://example.com/x?y=1".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserNavigate { pane: got, url }
                if got == pane && url == "https://example.com/x?y=1"
        ));
    }

    #[test]
    fn browser_history_verbs_map_pane_only() {
        let pane = PaneId::new();
        assert!(matches!(
            build_request(Cmd::BrowserBack { pane }).unwrap(),
            Request::BrowserBack { pane: got } if got == pane
        ));
        assert!(matches!(
            build_request(Cmd::BrowserForward { pane }).unwrap(),
            Request::BrowserForward { pane: got } if got == pane
        ));
        assert!(matches!(
            build_request(Cmd::BrowserReload { pane }).unwrap(),
            Request::BrowserReload { pane: got } if got == pane
        ));
    }

    #[test]
    fn browser_url_and_title_verbs_map_pane_only() {
        let pane = PaneId::new();
        assert!(matches!(
            build_request(Cmd::BrowserUrl { pane }).unwrap(),
            Request::BrowserUrl { pane: got } if got == pane
        ));
        assert!(matches!(
            build_request(Cmd::BrowserTitle { pane }).unwrap(),
            Request::BrowserTitle { pane: got } if got == pane
        ));
    }

    #[test]
    fn browser_click_maps_pane_and_target() {
        let pane = PaneId::new();
        let req = build_request(Cmd::BrowserClick {
            pane,
            target: "e7".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserClick { pane: got, target } if got == pane && target == "e7"
        ));
    }

    #[test]
    fn browser_fill_maps_pane_target_and_value() {
        let pane = PaneId::new();
        let req = build_request(Cmd::BrowserFill {
            pane,
            target: "e3".into(),
            value: "hello world".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserFill { pane: got, target, value }
                if got == pane && target == "e3" && value == "hello world"
        ));
    }

    #[test]
    fn browser_select_maps_pane_target_and_value() {
        let pane = PaneId::new();
        let req = build_request(Cmd::BrowserSelect {
            pane,
            target: "e9".into(),
            value: "OptionA".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserSelect { pane: got, target, value }
                if got == pane && target == "e9" && value == "OptionA"
        ));
    }

    #[test]
    fn browser_scroll_preserves_negative_offsets() {
        let pane = PaneId::new();
        let req = build_request(Cmd::BrowserScroll {
            pane,
            target: "root".into(),
            x: -10,
            y: 250,
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserScroll { pane: got, target, x: -10, y: 250 }
                if got == pane && target == "root"
        ));
    }

    #[test]
    fn browser_type_preserves_unicode_text() {
        let pane = PaneId::new();
        let req = build_request(Cmd::BrowserType {
            pane,
            text: "hello there 🚀".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserType { pane: got, text }
                if got == pane && text == "hello there 🚀"
        ));
    }

    #[test]
    fn browser_press_maps_named_keys() {
        let pane = PaneId::new();
        for key in ["Enter", "Tab", "ArrowDown", "Escape", "F1"] {
            let req = build_request(Cmd::BrowserPress {
                pane,
                key: key.into(),
            })
            .unwrap();
            assert!(matches!(
                req,
                Request::BrowserPress { pane: got, key: got_key }
                    if got == pane && got_key == key
            ));
        }
    }

    #[test]
    fn browser_text_value_attr_each_carry_their_fields() {
        let pane = PaneId::new();
        assert!(matches!(
            build_request(Cmd::BrowserText {
                pane,
                target: "e1".into()
            })
            .unwrap(),
            Request::BrowserText { pane: got, target } if got == pane && target == "e1"
        ));
        assert!(matches!(
            build_request(Cmd::BrowserValue {
                pane,
                target: "e2".into()
            })
            .unwrap(),
            Request::BrowserValue { pane: got, target } if got == pane && target == "e2"
        ));
        let req = build_request(Cmd::BrowserAttr {
            pane,
            target: "link".into(),
            name: "href".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserAttr { pane: got, target, name }
                if got == pane && target == "link" && name == "href"
        ));
    }

    // -- Notification CLI surface --------------------------------------
    //
    // 5 variants per feature, each provoking one realistic mistake the
    // user might make from a hook script.

    #[test]
    fn notify_with_explicit_pane_passes_it_through_even_when_env_set() {
        let _g = flowmux_pane_env_lock();
        let env_pane = PaneId::new();
        let arg_pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", env_pane.to_string());
        }
        let req = build_request(Cmd::Notify {
            pane: Some(arg_pane),
            title: "Build".into(),
            level: "info".into(),
            body: "ok".into(),
        })
        .unwrap();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        // Explicit --pane wins. Env is only a fallback.
        assert!(matches!(
            req,
            Request::Notify { pane: Some(got), .. } if got == arg_pane
        ));
    }

    #[test]
    fn notify_falls_back_to_flowmux_pane_id_env() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
        }
        let req = build_request(Cmd::Notify {
            pane: None,
            title: "Build".into(),
            level: "attention".into(),
            body: "ready".into(),
        })
        .unwrap();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            req,
            Request::Notify { pane: Some(got), level: NotificationLevel::AttentionNeeded, .. }
                if got == pane
        ));
    }

    #[test]
    fn notify_with_no_pane_and_no_env_yields_global_notification() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        let req = build_request(Cmd::Notify {
            pane: None,
            title: "Build".into(),
            level: "info".into(),
            body: "ok".into(),
        })
        .unwrap();
        assert!(matches!(req, Request::Notify { pane: None, .. }));
    }

    #[test]
    fn notify_ignores_invalid_flowmux_pane_id_env_instead_of_panicking() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", "not-a-uuid");
        }
        let req = build_request(Cmd::Notify {
            pane: None,
            title: "Build".into(),
            level: "info".into(),
            body: "ok".into(),
        })
        .unwrap();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        // Bad env should not crash; fall back to None and let the
        // daemon fire a global toast.
        assert!(matches!(req, Request::Notify { pane: None, .. }));
    }

    #[test]
    fn notify_unknown_level_string_falls_back_to_info_not_panic() {
        // parse_level is documented to default unknown strings to Info.
        // A clap value_parser already rejects unknown strings at the
        // CLI boundary, but the inner parse_level should still be
        // defensive — if a future caller passes "warn" they get Info.
        assert_eq!(parse_level("warn"), NotificationLevel::Info);
        assert_eq!(parse_level(""), NotificationLevel::Info);
        assert_eq!(parse_level("ATTENTION"), NotificationLevel::Info); // case-sensitive on purpose
    }

    // -- NotifyComplete (claude / opencode / codex hook helper) ---------

    #[test]
    fn notify_complete_default_message_uses_attention_level() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        let req = build_request(Cmd::NotifyComplete {
            agent: "Claude".into(),
            message: None,
            pane: None,
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::Notify {
                pane: None,
                level: NotificationLevel::AttentionNeeded,
                ..
            }
        ));
        if let Request::Notify { title, body, .. } = req {
            assert!(title.contains("Claude"), "title carries agent: {title}");
            assert_eq!(body, "task complete");
        }
    }

    #[test]
    fn notify_complete_passes_explicit_message_verbatim() {
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        let req = build_request(Cmd::NotifyComplete {
            agent: "Codex".into(),
            message: Some("waiting for approval".into()),
            pane: None,
        })
        .unwrap();
        if let Request::Notify { body, .. } = req {
            assert_eq!(body, "waiting for approval");
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn notify_complete_picks_pane_from_env_for_focus_routing() {
        let _g = flowmux_pane_env_lock();
        let pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", pane.to_string());
        }
        let req = build_request(Cmd::NotifyComplete {
            agent: "OpenCode".into(),
            message: None,
            pane: None,
        })
        .unwrap();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            req,
            Request::Notify { pane: Some(got), .. } if got == pane
        ));
    }

    #[test]
    fn notify_complete_explicit_pane_overrides_env() {
        let _g = flowmux_pane_env_lock();
        let env_pane = PaneId::new();
        let arg_pane = PaneId::new();
        unsafe {
            std::env::set_var("FLOWMUX_PANE_ID", env_pane.to_string());
        }
        let req = build_request(Cmd::NotifyComplete {
            agent: "claude".into(),
            message: Some("hi".into()),
            pane: Some(arg_pane),
        })
        .unwrap();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        assert!(matches!(
            req,
            Request::Notify { pane: Some(got), .. } if got == arg_pane
        ));
    }

    #[test]
    fn notify_complete_handles_empty_agent_string_without_panic() {
        // A buggy hook might forget to substitute the agent name. The
        // CLI should still produce a Notify (the resulting title is
        // useless but the toast is harmless), not crash mid-pipeline.
        let _g = flowmux_pane_env_lock();
        unsafe {
            std::env::remove_var("FLOWMUX_PANE_ID");
        }
        let req = build_request(Cmd::NotifyComplete {
            agent: String::new(),
            message: None,
            pane: None,
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::Notify {
                level: NotificationLevel::AttentionNeeded,
                ..
            }
        ));
    }

    // -- Agent hook event parsing -------------------------------------
    //
    // The OpenCode Flatpak plugin passes pane/surface as explicit
    // `--pane` / `--surface` flags because `flatpak run` resets env
    // before the in-sandbox CLI is reached, so the legacy
    // FLOWMUX_PANE_ID env-var path returns None across the boundary.
    // These tests pin the clap surface that path depends on.

    #[test]
    fn hooks_opencode_stop_accepts_pane_and_surface_flags() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "hooks",
            "opencode",
            "stop",
            "--pane",
            &pane.to_string(),
            "--surface",
            &surface.to_string(),
        ])
        .expect("clap must parse the OpenCode plugin's argv shape");
        let Cmd::Hooks {
            op:
                HooksOp::Opencode {
                    event:
                        AgentHookEvent::Stop {
                            pane: got_pane,
                            surface: got_surface,
                            args,
                        },
                },
        } = cli.cmd
        else {
            panic!("expected hooks opencode stop variant");
        };
        assert_eq!(got_pane, Some(pane));
        assert_eq!(got_surface, Some(surface));
        assert!(args.is_empty(), "no trailing payload was provided");
    }

    #[test]
    fn hooks_opencode_stop_keeps_trailing_payload_after_flags() {
        // The plugin always emits flags before the optional JSON
        // payload (Codex-compat) so clap can split them cleanly. Make
        // sure that ordering still parses with the payload intact.
        let pane = PaneId::new();
        let payload = r#"{"message":"all done"}"#;
        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "hooks",
            "opencode",
            "notification",
            "--pane",
            &pane.to_string(),
            payload,
        ])
        .expect("flags-before-payload must parse");
        let Cmd::Hooks {
            op:
                HooksOp::Opencode {
                    event:
                        AgentHookEvent::Notification {
                            pane: got_pane,
                            surface,
                            args,
                        },
                },
        } = cli.cmd
        else {
            panic!("expected hooks opencode notification variant");
        };
        assert_eq!(got_pane, Some(pane));
        assert!(surface.is_none());
        assert_eq!(args, vec![payload.to_string()]);
    }

    #[test]
    fn hooks_opencode_stop_with_no_flags_parses_empty() {
        // Backwards-compat: when no flags are present (legacy
        // installs that never emit them) the CLI must still parse so
        // `pane_from_env` / `surface_from_env` can resolve the values.
        let cli = Cli::try_parse_from(["flowmuxctl", "hooks", "opencode", "stop"])
            .expect("flag-less stop must still parse");
        let Cmd::Hooks {
            op:
                HooksOp::Opencode {
                    event:
                        AgentHookEvent::Stop {
                            pane,
                            surface,
                            args,
                        },
                },
        } = cli.cmd
        else {
            panic!("expected hooks opencode stop variant");
        };
        assert!(pane.is_none());
        assert!(surface.is_none());
        assert!(args.is_empty());
    }

    #[test]
    fn hooks_codex_stop_inherits_the_same_pane_flag_surface() {
        // AgentHookEvent is shared with Codex's `notify` config path;
        // the parser surface must be symmetric so a future Codex-side
        // sandbox forwarding patch can reuse the same flag.
        let pane = PaneId::new();
        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "hooks",
            "codex",
            "stop",
            "--pane",
            &pane.to_string(),
        ])
        .expect("codex must accept the same flag");
        let Cmd::Hooks {
            op:
                HooksOp::Codex {
                    event: AgentHookEvent::Stop { pane: got_pane, .. },
                },
        } = cli.cmd
        else {
            panic!("expected hooks codex stop variant");
        };
        assert_eq!(got_pane, Some(pane));
    }
}
