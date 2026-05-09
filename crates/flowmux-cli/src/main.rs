// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux` — thin CLI client over the daemon IPC socket.
//!
//! Verb shape mirrors cmux's documented CLI so existing user automation
//! (scripts, Claude Code hooks, etc.) keeps working.

use anyhow::Context;
use clap::{Parser, Subcommand};
use flowmux_config::paths;
use flowmux_core::{NotificationLevel, PaneId, SplitDirection};
use flowmux_ipc::{client::Client, protocol::Request, protocol::Response};
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;

mod agent;
mod hook_install;
mod hooks;

/// Read `FLOWMUX_PANE_ID` (set by `flowmux-app` at PTY spawn time) and parse
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

#[derive(Parser)]
#[command(
    name = "flowmux",
    version,
    about = "Linux/GTK4 terminal for AI coding agents"
)]
struct Cli {
    /// Override the daemon socket path. Defaults to `FLOWMUX_SOCKET_PATH`
    /// (injected by `flowmux-app` into every PTY) and falls back to the
    /// XDG runtime path. `FLOWMUX_SOCKET` is accepted as a legacy alias.
    #[arg(long, env = "FLOWMUX_SOCKET_PATH")]
    socket: Option<PathBuf>,

    /// Print responses as a single-line JSON object instead of the
    /// default human-readable indented form. Mirrors cmux's `--json`
    /// flag — easier to parse from agent scripts.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon health probe.
    Ping,

    /// Workspace operations.
    Workspace {
        #[command(subcommand)]
        op: WorkspaceOp,
    },

    /// Send a desktop notification attached to a pane.
    ///
    /// When `--pane` is omitted the daemon picks up `FLOWMUX_PANE_ID`
    /// from the calling PTY (set by flowmux-app at spawn time), so
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

    /// Open URL in a new in-app browser pane (splits next to the
    /// currently focused pane). Default split direction is right.
    Browser {
        url: String,
        #[arg(long, conflicts_with = "down")]
        right: bool,
        #[arg(long)]
        down: bool,
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

    /// Take a JSON snapshot of the page in a browser pane.
    BrowserSnapshot { pane: PaneId },

    /// Run JS in a browser pane and print the result.
    BrowserEval { pane: PaneId, source: String },

    /// Navigate a browser pane to a new URL.
    BrowserNavigate { pane: PaneId, url: String },
    /// Move a browser pane backward in session history.
    BrowserBack { pane: PaneId },
    /// Move a browser pane forward in session history.
    BrowserForward { pane: PaneId },
    /// Reload the current page in a browser pane.
    BrowserReload { pane: PaneId },
    /// Print the current URL of a browser pane.
    BrowserUrl { pane: PaneId },
    /// Print the current page title of a browser pane.
    BrowserTitle { pane: PaneId },
    /// Click an element by its `data-flowmux-ref` id (from a snapshot).
    BrowserClick { pane: PaneId, target: String },
    /// Fill an input/textarea by ref id with `value`.
    BrowserFill {
        pane: PaneId,
        target: String,
        value: String,
    },
    /// Select a `<select>` option by value or visible text.
    BrowserSelect {
        pane: PaneId,
        target: String,
        value: String,
    },
    /// Scroll an element into view, then offset the viewport by (x, y).
    BrowserScroll {
        pane: PaneId,
        target: String,
        x: i32,
        y: i32,
    },
    /// Type literal text into the active element of a browser pane.
    BrowserType { pane: PaneId, text: String },
    /// Press a single named key (Enter, Tab, ArrowDown, …).
    BrowserPress { pane: PaneId, key: String },
    /// Read innerText of an element.
    BrowserText { pane: PaneId, target: String },
    /// Read .value of an input/textarea/select.
    BrowserValue { pane: PaneId, target: String },
    /// Read an attribute (`href`, `id`, `class`, …) of an element.
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
    /// New session started — used to associate a session id with the
    /// current PTY so future events can resolve back to a pane.
    SessionStart,
    /// Session ended — clear the cached session→pane association.
    SessionEnd,
    /// Claude is about to call a tool; flowmux currently no-ops.
    PreToolUse,
    /// User submitted a prompt; flowmux currently no-ops.
    PromptSubmit,
}

#[derive(Subcommand)]
enum AgentHookEvent {
    /// Agent finished a turn. Trailing args carry an optional JSON
    /// payload — Codex's `notify` config delivers the event JSON this
    /// way; Claude/OpenCode use stdin and leave args empty.
    Stop {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Agent needs attention (permission prompt, error). Trailing args
    /// follow the same positional-or-stdin convention.
    Notification {
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOWMUX_LOG")
                .unwrap_or_else(|_| "warn,flowmux=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let Some(cmd) = cli.cmd else {
        return exec_flowmux_app();
    };

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
        // `Hooks` runtime handlers (Claude / Codex / Opencode events)
        // talk to the daemon themselves; `Hooks::Setup`, `Uninstall`,
        // and `Doctor` are pure file edits with no daemon round-trip.
        Cmd::Hooks { op } => return run_hooks_op(op, cli.socket.clone()).await,
        _ => {}
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

fn exec_flowmux_app() -> anyhow::Result<()> {
    let sibling = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("flowmux-app"))
        .filter(|p| p.is_file());
    let program = sibling.unwrap_or_else(|| PathBuf::from("flowmux-app"));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(&program).exec();
        Err::<(), _>(err).with_context(|| format!("launch {}", program.display()))
    }

    #[cfg(not(unix))]
    {
        let status = Command::new(&program)
            .status()
            .with_context(|| format!("launch {}", program.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
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

fn build_request(cmd: Cmd) -> anyhow::Result<Request> {
    Ok(match cmd {
        Cmd::Ping => Request::Ping,
        Cmd::Workspace {
            op: WorkspaceOp::New { name, root },
        } => Request::WorkspaceCreate {
            name,
            root: root.map(Ok).unwrap_or_else(std::env::current_dir)?,
        },
        Cmd::Workspace {
            op: WorkspaceOp::Ls,
        } => Request::WorkspaceList,
        Cmd::Notify {
            pane,
            title,
            level,
            body,
        } => Request::Notify {
            pane: pane.or_else(pane_from_env),
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
        Cmd::Browser { url, right, down } => {
            let direction = if down {
                SplitDirection::Horizontal
            } else if right {
                SplitDirection::Vertical
            } else {
                SplitDirection::Vertical
            };
            // When invoked from a terminal that flowmux-app spawned, the
            // PTY's `FLOWMUX_PANE_ID` lets the daemon resolve "next to me"
            // without the caller passing a pane id explicitly. cmux's
            // CLI uses the same fallback (`CMUX_SURFACE_ID`).
            Request::BrowserOpen {
                url,
                target_pane: pane_from_env(),
                direction,
            }
        }
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
    })
}

/// Dispatch every `flowmux hooks <op>` invocation. Setup/Doctor/Uninstall
/// only touch user config files and never need the daemon. The runtime
/// hook events (Claude/Codex/Opencode) talk to the daemon themselves.
async fn run_hooks_op(op: &HooksOp, socket: Option<PathBuf>) -> anyhow::Result<()> {
    use hook_install::{HookInstallStatus, HookTarget};
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
            for t in HookTarget::ALL {
                let label = match t {
                    HookTarget::Claude => "claude",
                    HookTarget::Codex => "codex",
                    HookTarget::OpenCode => "opencode",
                };
                println!("{label:8}  (run `flowmux hooks setup` to install)");
            }
            // Doctor is intentionally minimal today; cmux's full doctor
            // walks each marker and reports drift, which we can add
            // once we have real-world drift to debug.
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
    use hooks::*;
    let input = read_claude_hook_input();
    let pane = pane_from_env();
    let req = match event {
        ClaudeHookEvent::Stop => {
            let body = input.last_assistant_message.as_deref();
            build_stop_notify("Claude", body, pane)
        }
        ClaudeHookEvent::Notification => {
            let msg = input.message.as_deref();
            build_notification_notify("Claude", msg, pane)
        }
        // The remaining events are tracked by cmux as stateful
        // (session→workspace mapping, status flips). flowmux's first
        // notification cut keeps them as no-ops so the user gets the
        // same "ready" toast regardless of which Claude version they
        // run, without growing a stateful session store yet.
        ClaudeHookEvent::SessionStart
        | ClaudeHookEvent::SessionEnd
        | ClaudeHookEvent::PreToolUse
        | ClaudeHookEvent::PromptSubmit => return Ok(()),
    };
    if let Some(client) = hooks::connect_daemon(socket).await {
        hooks::send_best_effort(&client, req).await;
    }
    Ok(())
}

async fn run_generic_agent_hook_event(
    agent: &str,
    event: &AgentHookEvent,
    socket: Option<PathBuf>,
) -> anyhow::Result<()> {
    use hooks::*;
    let pane = pane_from_env();
    let req = match event {
        AgentHookEvent::Stop { args } => {
            let input = read_codex_hook_input(args);
            let body = input.last_assistant_message.as_deref();
            build_stop_notify(agent, body, pane)
        }
        AgentHookEvent::Notification { args } => {
            let input = read_codex_hook_input(args);
            let msg = input.message.as_deref();
            build_notification_notify(agent, msg, pane)
        }
        AgentHookEvent::SessionStart { .. } => return Ok(()),
    };
    if let Some(client) = hooks::connect_daemon(socket).await {
        hooks::send_best_effort(&client, req).await;
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
            url: "https://example.com".into(),
            right: false,
            down: false,
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
            url: "https://example.com".into(),
            right: false,
            down: false,
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
            url: "https://example.com".into(),
            right: false,
            down: false,
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
        let req = build_request(Cmd::Browser {
            url: "https://a.test".into(),
            right: true,
            down: false,
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
        let req = build_request(Cmd::Browser {
            url: "https://a.test".into(),
            right: false,
            down: true,
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
}
