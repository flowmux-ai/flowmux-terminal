// SPDX-License-Identifier: GPL-3.0-or-later
//! Unit tests for the CLI, split out of `main.rs` via #[path].

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
    fn ping_and_workspace_ls_parse_to_read_only_requests() {
        let ping = Cli::try_parse_from(["flowmuxctl", "ping"]).unwrap();
        assert!(matches!(build_request(ping.cmd).unwrap(), Request::Ping));

        let list = Cli::try_parse_from(["flowmuxctl", "workspace", "ls"]).unwrap();
        assert!(matches!(
            build_request(list.cmd).unwrap(),
            Request::WorkspaceList
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
    fn browser_wait_and_screenshot_map_to_ipc_requests() {
        let pane = PaneId::new();
        let pane_arg = format!("pane:{pane}");
        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "browser",
            "wait",
            &pane_arg,
            "--selector",
            ".ready",
            "--timeout-ms",
            "750",
            "--poll-ms",
            "25",
        ])
        .expect("browser wait should parse");
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::BrowserWait {
                pane: got,
                condition: BrowserWaitCondition::Selector(selector),
                timeout_ms: 750,
                poll_ms: 25,
            } if got == pane && selector == ".ready"
        ));

        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "browser",
            "screenshot",
            &pane_arg,
            "/tmp/flowmux-page.png",
        ])
        .expect("browser screenshot should parse");
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::BrowserScreenshot { pane: got, path }
                if got == pane && path.as_path() == std::path::Path::new("/tmp/flowmux-page.png")
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
    fn tmux_style_aliases_map_to_existing_ipc_requests() {
        let pane = PaneId::new();
        let pane_arg = format!("pane:{pane}");

        let cli = Cli::try_parse_from(["flowmuxctl", "capture-pane", &pane_arg]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::PaneReadScreen { pane: got } if got == pane
        ));

        let cli = Cli::try_parse_from(["flowmuxctl", "list-panes"]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::WorkspaceTree
        ));

        let cli = Cli::try_parse_from(["flowmuxctl", "select-pane", &pane_arg]).unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::PaneFocus { pane: got } if got == pane
        ));

        let cli = Cli::try_parse_from(["flowmuxctl", "resize-pane", &pane_arg, "--ratio", "0.6"])
            .unwrap();
        assert!(matches!(
            build_request(cli.cmd).unwrap(),
            Request::PaneResize { pane: got, ratio } if got == pane && (ratio - 0.6).abs() < f32::EPSILON
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
        use flowmux_core::{AgentActivity, AgentStatus};
        use flowmux_ipc::protocol::{TreeAgent, TreePane, TreeTab, TreeWorkspace};
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
                        agent: Some(TreeAgent {
                            name: "codex".into(),
                            status: AgentStatus::Blocked,
                            activity: AgentActivity::NeedsInput,
                            source: Some("flowmux:hook".into()),
                            seq: Some(7),
                            message: Some("approval needed".into()),
                            custom_status: None,
                            session_id: Some("session-1".into()),
                        }),
                    },
                    TreeTab {
                        id: t2,
                        title: "docs".into(),
                        kind: "browser".into(),
                        active: true,
                        agent: None,
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
        assert!(out.contains(&format!(
            "  [terminal] {t1} \"shell\" agent=codex status=blocked"
        )));
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
    fn local_only_commands_parse_to_local_variants() {
        let theme = Cli::try_parse_from(["flowmuxctl", "theme", "path"]).unwrap();
        assert!(matches!(theme.cmd, Cmd::Theme { op: ThemeOp::Path }));

        let theme_src = PathBuf::from("/tmp/flowmux-theme.toml");
        let import =
            Cli::try_parse_from(["flowmuxctl", "theme", "import", "/tmp/flowmux-theme.toml"])
                .unwrap();
        assert!(matches!(
            import.cmd,
            Cmd::Theme {
                op: ThemeOp::Import { src }
            } if src == theme_src
        ));

        assert!(matches!(
            Cli::try_parse_from(["flowmuxctl", "list-browsers"])
                .unwrap()
                .cmd,
            Cmd::ListBrowsers
        ));
        assert!(matches!(
            Cli::try_parse_from(["flowmuxctl", "doctor"]).unwrap().cmd,
            Cmd::Doctor
        ));
        assert!(matches!(
            Cli::try_parse_from(["flowmuxctl", "fix"]).unwrap().cmd,
            Cmd::Fix
        ));

        let agent = Cli::try_parse_from([
            "flowmuxctl",
            "agent",
            "install",
            "--agent",
            "codex",
            "--force",
        ])
        .unwrap();
        assert!(matches!(
            agent.cmd,
            Cmd::Agent {
                op: AgentOp::Install { agent, force }
            } if agent == vec!["codex"] && force
        ));

        let hooks = Cli::try_parse_from([
            "flowmuxctl",
            "hooks",
            "setup",
            "--agent",
            "claude",
            "--flowmux-bin",
            "/usr/bin/flowmux",
        ])
        .unwrap();
        assert!(matches!(
            hooks.cmd,
            Cmd::Hooks {
                op: HooksOp::Setup {
                    agent,
                    flowmux_bin: Some(bin),
                }
            } if agent == vec!["claude"] && bin == "/usr/bin/flowmux"
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

    #[test]
    fn hooks_cline_notification_uses_generic_agent_event_parser() {
        let surface = SurfaceId::new();
        let payload = r#"{"message":"needs approval"}"#;
        let cli = Cli::try_parse_from([
            "flowmuxctl",
            "hooks",
            "cline",
            "notification",
            "--surface",
            &surface.to_string(),
            payload,
        ])
        .expect("cline must accept the generic hook event shape");
        let Cmd::Hooks {
            op:
                HooksOp::Cline {
                    event:
                        AgentHookEvent::Notification {
                            pane,
                            surface: got_surface,
                            args,
                        },
                },
        } = cli.cmd
        else {
            panic!("expected hooks cline notification variant");
        };
        assert!(pane.is_none());
        assert_eq!(got_surface, Some(surface));
        assert_eq!(args, vec![payload.to_string()]);
    }

    #[test]
    fn notification_management_commands_map_to_ipc_requests() {
        let id = NotificationId::new();

        let list = build_request(Cmd::Notifications {
            op: NotificationOp::List { unread: true },
        })
        .unwrap();
        assert!(matches!(
            list,
            Request::NotificationsList { unread_only: true }
        ));

        let open = build_request(Cmd::Notifications {
            op: NotificationOp::Open { id },
        })
        .unwrap();
        assert!(matches!(open, Request::NotificationOpen { id: got } if got == id));

        let jump = build_request(Cmd::Notifications {
            op: NotificationOp::JumpToUnread,
        })
        .unwrap();
        assert!(matches!(jump, Request::NotificationJumpToUnread));

        let mark = build_request(Cmd::Notifications {
            op: NotificationOp::MarkRead { id },
        })
        .unwrap();
        assert!(matches!(mark, Request::NotificationMarkRead { id: got } if got == id));

        let clear = build_request(Cmd::Notifications {
            op: NotificationOp::Clear,
        })
        .unwrap();
        assert!(matches!(clear, Request::NotificationsClear));
    }
