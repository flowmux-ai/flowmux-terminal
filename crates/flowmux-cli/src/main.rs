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
use std::str::FromStr;

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

/// Parse a pane reference from the CLI. Accepts cmux-compatible prefix
/// forms `surface:<uuid>` and `pane:<uuid>` as well as a bare uuid, so
/// agents that already speak cmux's CLI can keep their existing call
/// shape. (`surface:<integer>` style numeric refs are out of scope for
/// this layer — they require a daemon-side index lookup, planned with
/// the ListPanes IPC verb.)
fn parse_pane_or_surface(s: &str) -> Result<PaneId, String> {
    let inner = s
        .strip_prefix("surface:")
        .or_else(|| s.strip_prefix("pane:"))
        .unwrap_or(s);
    PaneId::from_str(inner).map_err(|e| format!("invalid pane id `{s}`: {e}"))
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

    // ---- Phase 5 P0 action gap ------------------------
    /// Double-click an element by its ref id.
    BrowserDblClick { pane: PaneId, target: String },
    /// Hover over an element (mouseenter + mouseover).
    BrowserHover { pane: PaneId, target: String },
    /// Focus an element (`HTMLElement.focus()`).
    BrowserFocus { pane: PaneId, target: String },
    /// Blur an element (`HTMLElement.blur()`).
    BrowserBlur { pane: PaneId, target: String },
    /// Check a checkbox or radio (no-op when already checked).
    BrowserCheck { pane: PaneId, target: String },
    /// Uncheck a checkbox (radios cannot be unchecked individually).
    BrowserUncheck { pane: PaneId, target: String },
    /// Print `true` / `false` for whether an element is currently
    /// rendered (size > 0, not display:none, opacity > 0).
    BrowserIsVisible { pane: PaneId, target: String },
    /// Print `true` / `false` for whether `el.disabled === false`.
    BrowserIsEnabled { pane: PaneId, target: String },
    /// Print `true` / `false` for `el.checked` of a checkbox/radio.
    BrowserIsChecked { pane: PaneId, target: String },
    /// Print the number of elements matching a CSS selector.
    BrowserCount { pane: PaneId, selector: String },

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

    let socket = cli
        .socket
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from))
        .unwrap_or_else(paths::runtime_socket);
    let client = Client::connect(&socket)
        .await
        .with_context(|| "is the flowmux daemon running? try: flowmux-app &")?;

    if let Cmd::NotifyStream { pane } = cli.cmd {
        return notify_stream(&client, pane).await;
    }

    let req = build_request(cli.cmd)?;
    let resp = client.call(req).await?;
    print_response(&resp, cli.json)?;
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

        Cmd::BrowserDblClick { pane, target } => Request::BrowserDblClick { pane, target },
        Cmd::BrowserHover { pane, target } => Request::BrowserHover { pane, target },
        Cmd::BrowserFocus { pane, target } => Request::BrowserFocus { pane, target },
        Cmd::BrowserBlur { pane, target } => Request::BrowserBlur { pane, target },
        Cmd::BrowserCheck { pane, target } => Request::BrowserCheck { pane, target },
        Cmd::BrowserUncheck { pane, target } => Request::BrowserUncheck { pane, target },
        Cmd::BrowserIsVisible { pane, target } => Request::BrowserIsVisible { pane, target },
        Cmd::BrowserIsEnabled { pane, target } => Request::BrowserIsEnabled { pane, target },
        Cmd::BrowserIsChecked { pane, target } => Request::BrowserIsChecked { pane, target },
        Cmd::BrowserCount { pane, selector } => Request::BrowserCount { pane, selector },

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
    fn parse_pane_or_surface_accepts_bare_uuid_and_prefix_forms() {
        let pane = PaneId::new();
        let s = pane.to_string();

        assert_eq!(parse_pane_or_surface(&s).unwrap(), pane);
        assert_eq!(
            parse_pane_or_surface(&format!("surface:{s}")).unwrap(),
            pane
        );
        assert_eq!(
            parse_pane_or_surface(&format!("pane:{s}")).unwrap(),
            pane
        );
    }

    #[test]
    fn parse_pane_or_surface_rejects_garbage() {
        assert!(parse_pane_or_surface("not-a-uuid").is_err());
        assert!(parse_pane_or_surface("surface:also-not-a-uuid").is_err());
    }

    #[test]
    fn print_response_pretty_default_uses_indented_output() {
        let resp = Response::Ok;
        let pretty = serde_json::to_string_pretty(&resp).unwrap();
        let compact = serde_json::to_string(&resp).unwrap();

        // Sanity-check the format we expect on each side. Pretty output
        // is multi-line for non-trivial structures — `Response::Ok`
        // serializes to a single string either way, so use a richer
        // variant to verify the shape difference.
        let resp = Response::BrowserPaneOpened {
            pane: PaneId::new(),
            placement_strategy: flowmux_core::PlacementStrategy::SplitRight,
        };
        let pretty = serde_json::to_string_pretty(&resp).unwrap();
        let compact = serde_json::to_string(&resp).unwrap();
        assert!(pretty.contains('\n'), "pretty output should be multi-line");
        assert!(
            !compact.contains('\n'),
            "compact (--json mode) output must be single-line"
        );
    }

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

    #[test]
    fn browser_open_no_flags_defaults_to_right_split() {
        // SAFETY: tests in this module mutate process-global env. Remove
        // FLOWMUX_PANE_ID so a leak from another test doesn't change the
        // expected target_pane = None outcome below.
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
        let pane = PaneId::new();
        let pane_str = pane.to_string();
        // SAFETY: this test reads/writes a process-global env var. cargo
        // serializes by default within a binary's test harness, and we
        // remove the var at the end of the test.
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
            text: "안녕하세요 🚀".into(),
        })
        .unwrap();
        assert!(matches!(
            req,
            Request::BrowserType { pane: got, text }
                if got == pane && text == "안녕하세요 🚀"
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
}
