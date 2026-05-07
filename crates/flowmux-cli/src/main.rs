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

#[derive(Parser)]
#[command(
    name = "flowmux",
    version,
    about = "Linux/GTK4 terminal for AI coding agents"
)]
struct Cli {
    /// Override the daemon socket path.
    #[arg(long, env = "FLOWMUX_SOCKET")]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
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
    Notify {
        #[arg(long)]
        pane: Option<PaneId>,
        #[arg(long, default_value = "Terminal")]
        title: String,
        #[arg(long, default_value = "info", value_parser = ["info", "attention", "error"])]
        level: String,
        body: String,
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

    /// Open URL in the in-app browser.
    Browser { url: String },

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

    // Local-only commands — handled before the daemon connect so they
    // work without a running flowmux-app.
    match &cli.cmd {
        Cmd::Theme { op } => return run_theme_op(op),
        Cmd::ListBrowsers => {
            for s in flowmux_cookies::discover_sources() {
                let detected = s.detect().is_some();
                println!("{:8}  detected={}", s.id().slug(), detected);
            }
            return Ok(());
        }
        _ => {}
    }

    let socket = cli.socket.unwrap_or_else(paths::runtime_socket);
    let client = Client::connect(&socket)
        .await
        .with_context(|| "is the flowmux daemon running? try: flowmux-app &")?;

    if let Cmd::NotifyStream { pane } = cli.cmd {
        return notify_stream(&client, pane).await;
    }

    let req = build_request(cli.cmd)?;
    let resp = client.call(req).await?;
    print_response(&resp)?;
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
            pane,
            title,
            body,
            level: parse_level(&level),
        },
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
        Cmd::Browser { url } => Request::BrowserOpen { url, surface: None },
        Cmd::Ssh { target } => Request::SshConnect { target },
        Cmd::NotifyStream { .. } => unreachable!("handled before request build"),
        Cmd::ClaudeTeams { count, root, args } => Request::ClaudeTeams {
            count,
            args,
            root: root.map(Ok).unwrap_or_else(std::env::current_dir)?,
        },
        Cmd::BrowserSnapshot { pane } => Request::BrowserSnapshot { pane },
        Cmd::BrowserEval { pane, source } => Request::BrowserEval { pane, source },
        Cmd::ImportCookies { from, domain } => Request::ImportCookies {
            source: from,
            domain,
        },
        Cmd::ListBrowsers => unreachable!("handled before request build"),
        Cmd::Theme { .. } => unreachable!("handled before request build"),
    })
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
            println!("relaunch flowmux-app to apply.");
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

fn print_response(r: &Response) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(r)?);
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
}
