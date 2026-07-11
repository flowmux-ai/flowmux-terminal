// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux` — thin CLI client over the daemon IPC socket.
//!
//! Verb shape mirrors cmux's documented CLI so existing user automation
//! (scripts, Claude Code hooks, etc.) keeps working.

use anyhow::Context;
use clap::{Parser, Subcommand};
use flowmux_config::paths;
use flowmux_core::{
    NotificationId, NotificationLevel, PaneId, SplitDirection, SurfaceId, WorkspaceId,
};
use flowmux_ipc::{
    client::Client,
    protocol::{BrowserWaitCondition, Request, Response},
};
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;

mod agent;
mod cmd_hooks;
mod cmd_ops;
mod desktop_install;
mod doctor;
mod hook_install;
mod hooks;
mod keys;
mod output;
mod pty_tee;
mod request;
// Bring each command module's handlers into crate-root scope so both `main`'s
// dispatch and the `tests` module (via `use super::*`) reference them unqualified,
// exactly as when they lived in this file. Glob form avoids per-item churn.
use cmd_hooks::*;
use cmd_ops::*;
use keys::*;
use output::*;
use request::*;

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
    /// Codex). Fires a `TurnCompleted` toast titled with the agent
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

    /// Inspect and manage flowmux notification state.
    Notifications {
        #[command(subcommand)]
        op: NotificationOp,
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
    /// falls back to `$FLOWMUX_PANE_ID`).
    ReadScreen { pane: Option<PaneId> },
    /// tmux-compatible alias for `read-screen`.
    CapturePane { pane: Option<PaneId> },
    /// tmux-compatible alias for `tree`.
    ListPanes,
    /// tmux-compatible alias for `focus-pane`.
    SelectPane { pane: PaneId },
    /// Resize a pane's parent split to an absolute first-child ratio.
    ResizePane {
        pane: PaneId,
        #[arg(long)]
        ratio: f32,
    },

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
enum NotificationOp {
    /// List notifications, oldest first.
    List {
        /// Only show unread notifications.
        #[arg(long)]
        unread: bool,
    },

    /// Mark a notification read and focus its source pane when known.
    Open { id: NotificationId },

    /// Open the oldest unread notification.
    #[command(name = "jump-to-unread")]
    JumpToUnread,

    /// Mark one notification as read without focusing its source pane.
    #[command(name = "mark-read")]
    MarkRead { id: NotificationId },

    /// Clear all notifications.
    Clear,
}

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
    /// Poll until a page condition is true. Network-idle waits are not supported.
    #[command(group(
        clap::ArgGroup::new("condition")
            .required(true)
            .args(["selector", "text", "url", "ready_state", "js"])
    ))]
    Wait {
        pane: PaneId,
        /// CSS selector that must resolve to at least one element.
        #[arg(long)]
        selector: Option<String>,
        /// Text that must appear in document.body.innerText.
        #[arg(long)]
        text: Option<String>,
        /// Substring that must appear in location.href.
        #[arg(long)]
        url: Option<String>,
        /// Required document.readyState value (loading, interactive, complete).
        #[arg(long)]
        ready_state: Option<String>,
        /// JavaScript expression or function body that evaluates truthy.
        #[arg(long)]
        js: Option<String>,
        /// Maximum wait time in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 100)]
        poll_ms: u64,
    },
    /// Save the visible browser viewport to a PNG file.
    Screenshot { pane: PaneId, path: PathBuf },
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
    /// Cline lifecycle hook handler. Uses the same generic event
    /// shape as Codex/OpenCode when a caller wires Cline to flowmux.
    Cline {
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

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
