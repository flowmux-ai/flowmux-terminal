// SPDX-License-Identifier: GPL-3.0-or-later
//! Unit tests for `flowmux-core`, split out of `lib.rs` via #[path].

    use super::*;
    use std::str::FromStr;

    #[test]
    fn split_leaf_replaces_target_with_a_split() {
        let leaf_id = PaneId::new();
        let mut p = Pane::Leaf {
            id: leaf_id,
            content: PaneContent::Terminal { pid: None },
        };
        let new_id = p
            .split_leaf(
                leaf_id,
                SplitDirection::Vertical,
                0.5,
                PaneContent::Terminal { pid: None },
            )
            .unwrap();
        match &p {
            Pane::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Vertical);
                assert!(matches!(**first, Pane::Leaf { id, .. } if id == leaf_id));
                assert!(matches!(**second, Pane::Leaf { id, .. } if id == new_id));
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn split_leaf_recurses_into_existing_split() {
        let l1 = PaneId::new();
        let l2 = PaneId::new();
        let mut p = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l1,
                content: PaneContent::Terminal { pid: None },
            }),
            second: Box::new(Pane::Leaf {
                id: l2,
                content: PaneContent::Terminal { pid: None },
            }),
        };
        let new_id = p
            .split_leaf(
                l2,
                SplitDirection::Vertical,
                0.5,
                PaneContent::Terminal { pid: None },
            )
            .unwrap();
        let mut leaves = vec![];
        p.for_each_leaf(|id| leaves.push(id));
        assert_eq!(leaves.len(), 3);
        assert!(leaves.contains(&l1));
        assert!(leaves.contains(&l2));
        assert!(leaves.contains(&new_id));
    }

    #[test]
    fn remove_leaf_collapses_split() {
        let l1 = PaneId::new();
        let l2 = PaneId::new();
        let p = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l1,
                content: PaneContent::Terminal { pid: None },
            }),
            second: Box::new(Pane::Leaf {
                id: l2,
                content: PaneContent::Terminal { pid: None },
            }),
        };
        match p.remove_leaf(l1) {
            RemoveOutcome::Replaced(Pane::Leaf { id, .. }) => assert_eq!(id, l2),
            _ => panic!("expected leaf l2 to remain after l1 removal"),
        }
    }

    #[test]
    fn remove_leaf_returns_entirely_removed_on_root_match() {
        let id = PaneId::new();
        let p = Pane::Leaf {
            id,
            content: PaneContent::Terminal { pid: None },
        };
        assert!(matches!(p.remove_leaf(id), RemoveOutcome::EntirelyRemoved));
    }

    #[test]
    fn remove_leaf_returns_not_found_when_id_missing() {
        let id = PaneId::new();
        let other = PaneId::new();
        let p = Pane::Leaf {
            id,
            content: PaneContent::Terminal { pid: None },
        };
        assert!(matches!(p.remove_leaf(other), RemoveOutcome::NotFound(_)));
    }

    #[test]
    fn pane_content_normalizes_legacy_terminal_to_surface_tab() {
        // Folder name must fit within terminal_tab_title_for_cwd's truncation
        // budget so this test asserts migration semantics, not truncation.
        // The truncation contract itself is covered by
        // terminal_tab_title_for_cwd_uses_folder_and_truncates below.
        let cwd = PathBuf::from("/tmp/flowmux-core");
        let mut content = PaneContent::Terminal { pid: Some(123) };

        content.normalize_to_tabs(Some(cwd.clone()));

        let PaneContent::Tabs { active, surfaces } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].id, active);
        assert_eq!(surfaces[0].title, "flowmux-core");
        assert!(matches!(
            &surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(got), .. } if got == &cwd
        ));
    }

    #[test]
    fn terminal_tab_title_for_cwd_uses_folder_and_truncates() {
        assert_eq!(
            terminal_tab_title_for_cwd(Some(Path::new("/tmp/project"))),
            "project"
        );
        assert_eq!(
            terminal_tab_title_for_cwd(Some(Path::new("/tmp/1234567890123456789"))),
            "12345678901234567..."
        );
        assert_eq!(terminal_tab_title_for_cwd(Some(Path::new("/"))), "Terminal");
    }

    #[test]
    fn pane_content_normalizes_legacy_terminal_number_titles() {
        let mut first = PaneSurface::terminal("Terminal 3", Some("/tmp/project".into()));
        let first_id = first.id;
        let mut locked = PaneSurface::terminal("Terminal 4", Some("/tmp/locked".into()));
        locked.title_locked = true;
        first.title_locked = false;
        let mut content = PaneContent::Tabs {
            active: first_id,
            surfaces: vec![first, locked],
        };

        content.normalize_to_tabs(None);

        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "project");
        assert_eq!(surfaces[1].title, "Terminal 4");
    }

    #[test]
    fn pane_content_resets_stale_terminal_titles_on_normalize() {
        // Previously this test asserted the opposite — that an unlocked
        // terminal whose title didn't match the cwd was AUTO-LOCKED.
        // That kept stale OSC 0/2 titles ("Claude Code", "codex foo")
        // alive across app restarts. The current behavior resets the
        // title back to the cwd-derived form and stays unlocked, so the
        // next process inside the tab can paint a fresh title.
        let custom = PaneSurface::terminal("server", Some("/tmp/project".into()));
        let custom_id = custom.id;
        let mut content = PaneContent::Tabs {
            active: custom_id,
            surfaces: vec![custom],
        };

        assert!(content.normalize_to_tabs(None));

        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(
            surfaces[0].title,
            terminal_tab_title_for_cwd(Some(std::path::Path::new("/tmp/project")))
        );
        assert!(!surfaces[0].title_locked);
    }

    #[test]
    fn pane_content_keeps_cwd_title_unlocked_on_normalize() {
        let surface = PaneSurface::terminal("project", Some("/tmp/project".into()));
        let surface_id = surface.id;
        let mut content = PaneContent::Tabs {
            active: surface_id,
            surfaces: vec![surface],
        };

        assert!(!content.normalize_to_tabs(None));

        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "project");
        assert!(!surfaces[0].title_locked);
    }

    #[test]
    fn pane_surface_tabs_can_activate_and_close() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        let second = PaneSurface::terminal("two", None);
        let second_id = second.id;

        assert_eq!(pane.add_surface_to_leaf(pane_id, second), Some(second_id));
        assert_eq!(pane.active_surface_id(pane_id), Some(second_id));

        let first_id = match &pane {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces[0].id,
            _ => panic!("expected tabbed leaf"),
        };
        assert!(pane.set_active_surface(pane_id, first_id));
        assert_eq!(pane.active_surface_id(pane_id), Some(first_id));

        assert_eq!(
            pane.close_surface_in_leaf(pane_id, first_id),
            CloseSurfaceOutcome::SurfaceRemoved
        );
        assert_eq!(pane.active_surface_id(pane_id), Some(second_id));
        assert_eq!(
            pane.close_surface_in_leaf(pane_id, second_id),
            CloseSurfaceOutcome::LastSurfaceRemoved
        );
    }

    #[test]
    fn pane_surface_title_can_be_renamed() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        assert_eq!(pane.surface_title(pane_id, surface_id), Some("one"));
        assert!(pane.rename_surface(pane_id, surface_id, "renamed".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("renamed"));
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = pane
        else {
            panic!("expected tabbed leaf")
        };
        assert!(surfaces[0].title_locked);
    }

    #[test]
    fn terminal_surface_cwd_uses_active_tab_cwd() {
        // A pane with three terminal tabs at /tmp, /home, /bin. While viewing
        // /home, splitting should seed the new terminal at /home.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("tmp", Some("/tmp".into())),
        };
        let home = PaneSurface::terminal("home", Some("/home".into()));
        let home_id = home.id;
        let bin = PaneSurface::terminal("bin", Some("/bin".into()));
        pane.add_surface_to_leaf(pane_id, home).unwrap();
        pane.add_surface_to_leaf(pane_id, bin).unwrap();
        assert!(pane.set_active_surface(pane_id, home_id));

        assert_eq!(
            pane.terminal_surface_cwd(pane_id),
            Some(std::path::PathBuf::from("/home"))
        );
    }

    #[test]
    fn terminal_surface_cwd_falls_back_to_prior_terminal_when_browser_active() {
        // Tabs in order: terminal(/tmp), browser, terminal(/home). With the
        // browser active, the new terminal should inherit /tmp - the most
        // recent terminal that comes *before* the browser in tab order.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("tmp", Some("/tmp".into())),
        };
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        let home = PaneSurface::terminal("home", Some("/home".into()));
        pane.add_surface_to_leaf(pane_id, browser).unwrap();
        pane.add_surface_to_leaf(pane_id, home).unwrap();
        assert!(pane.set_active_surface(pane_id, browser_id));

        assert_eq!(
            pane.terminal_surface_cwd(pane_id),
            Some(std::path::PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn terminal_surface_cwd_picks_most_recent_terminal_before_browser() {
        // Tabs in order: terminal(/a), terminal(/b), browser. With the browser
        // active, the closest terminal before it (/b) wins.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", Some("/a".into())),
        };
        let b = PaneSurface::terminal("b", Some("/b".into()));
        pane.add_surface_to_leaf(pane_id, b).unwrap();
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        pane.add_surface_to_leaf(pane_id, browser).unwrap();
        assert!(pane.set_active_surface(pane_id, browser_id));

        assert_eq!(
            pane.terminal_surface_cwd(pane_id),
            Some(std::path::PathBuf::from("/b"))
        );
    }

    #[test]
    fn terminal_surface_cwd_returns_none_when_browser_has_no_prior_terminal() {
        // Tabs in order: browser, terminal(/home). With the browser active,
        // there is no terminal *before* it - resolution returns None so the
        // caller falls back to the workspace root.
        let pane_id = PaneId::new();
        let pane_id_inner = pane_id;
        let mut pane = Pane::Leaf {
            id: pane_id_inner,
            content: PaneContent::Tabs {
                active: SurfaceId::new(),
                surfaces: vec![],
            },
        };
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        let term = PaneSurface::terminal("home", Some("/home".into()));
        // Manually set the surfaces vec because tabbed_terminal would create
        // a terminal first.
        if let Pane::Leaf {
            content: PaneContent::Tabs { active, surfaces },
            ..
        } = &mut pane
        {
            surfaces.push(browser);
            surfaces.push(term);
            *active = browser_id;
        }

        assert_eq!(pane.terminal_surface_cwd(pane_id), None);
    }

    #[test]
    fn terminal_surface_cwd_updates_unlocked_title_only() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", Some("/tmp/old".into())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        assert!(pane.set_surface_cwd(pane_id, surface_id, "/tmp/new".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("new"));
        assert!(!pane.set_surface_cwd(pane_id, surface_id, "/tmp/new".into()));
        assert!(pane.rename_surface(pane_id, surface_id, "fixed".into()));
        assert!(pane.set_surface_cwd(pane_id, surface_id, "/tmp/another".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("fixed"));
    }

    #[test]
    fn set_surface_cwd_preserves_program_title_when_cwd_unchanged() {
        // Regression guard: titles set by external programs such as vi/claude
        // through OSC 0/2 must not be reverted to folder names by one-second cwd
        // polling. If cwd is unchanged, polling calls set_surface_cwd with the
        // same cwd, and surface.title must stay untouched.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("os", Some("/tmp/work".into())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // Enter an external program: set_surface_title_auto applies "Claude Code".
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "Claude Code".into(),));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Claude Code"));

        // Polling sees the same cwd again; it should be a no-op and keep the
        // title at "Claude Code".
        assert!(!pane.set_surface_cwd(pane_id, surface_id, "/tmp/work".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Claude Code"));

        // When the user actually changes cwd, then it returns to a folder label.
        // This matches the natural flow after leaving the external program.
        assert!(pane.set_surface_cwd(pane_id, surface_id, "/tmp/another".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("another"));

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = pane
        else {
            panic!("expected tabbed leaf")
        };
        assert!(matches!(
            &surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &PathBuf::from("/tmp/another")
        ));
    }

    #[test]
    fn set_surface_browser_url_replaces_initial_url_only_for_browser_kind() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_browser("Docs", "https://one.test".into()),
        };
        let browser_id = pane.active_surface_id(pane_id).unwrap();
        assert!(pane.set_surface_browser_url(pane_id, browser_id, "https://two.test".into()));
        assert!(matches!(
            pane.find_surface(pane_id, browser_id).unwrap().kind,
            SurfaceKind::Browser { initial_url: Some(ref u) } if u == "https://two.test"
        ));
        // Setting the same URL again returns false (no-op).
        assert!(!pane.set_surface_browser_url(pane_id, browser_id, "https://two.test".into()));

        // Terminal surfaces should not be affected.
        let term = PaneSurface::terminal("term", None);
        let term_id = term.id;
        pane.add_surface_to_leaf(pane_id, term).unwrap();
        assert!(!pane.set_surface_browser_url(pane_id, term_id, "https://x.test".into()));
    }

    #[test]
    fn set_surface_title_auto_skips_locked_titles() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_browser("Browser", "https://one.test".into()),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // Unlocked surfaces update automatically.
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "Page Title".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Page Title"));

        // Identical title is a no-op.
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "Page Title".into()));

        // Empty / whitespace title is a no-op.
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "".into()));
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "   ".into()));

        // User rename -> title_locked = true.
        assert!(pane.rename_surface(pane_id, surface_id, "MyName".into()));
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "Other Page".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("MyName"));
    }

    #[test]
    fn title_is_shell_cwd_echo_recognizes_bash_default_ps1() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // Default bash `\u@\h: \w` with an absolute path.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // `\u@\h:\w` without a space.
        assert!(title_is_shell_cwd_echo(
            "junsu@host:/tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // debian_chroot prefix variant.
        assert!(title_is_shell_cwd_echo(
            "(jammy)junsu@host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // Host-only prefix.
        assert!(title_is_shell_cwd_echo(
            "host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // Path only, for prompt themes that emit only the path.
        assert!(title_is_shell_cwd_echo(
            "/tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
    }

    #[test]
    fn title_is_shell_cwd_echo_recognizes_tilde_form() {
        let home = Path::new("/home/junsu");
        let cwd = Path::new("/home/junsu/dev/os");
        // bash `\w` abbreviates $HOME to `~`.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: ~/dev/os",
            cwd,
            Some(home),
        ));
        // Home itself, cwd == $HOME -> ~.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: ~",
            Path::new("/home/junsu"),
            Some(home),
        ));
        // Without home information, tilde matching is unavailable but absolute
        // path matching still works.
        assert!(!title_is_shell_cwd_echo("junsu@host: ~/dev/os", cwd, None,));
    }

    #[test]
    fn title_is_shell_cwd_echo_passes_program_titles() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // Titles from external programs such as vi/codex/claude/tmux do not
        // match the PS1 pattern (`prefix:[ ]<cwd>`).
        assert!(!title_is_shell_cwd_echo("vim src/main.rs", cwd, None,));
        assert!(!title_is_shell_cwd_echo("tmux: 0:bash*", cwd, None,));
        assert!(!title_is_shell_cwd_echo("claude — Anthropic", cwd, None,));
        // vim opening a file inside cwd should also pass through because its
        // prefix does not end with `:`.
        assert!(!title_is_shell_cwd_echo(
            "vim /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // Empty / whitespace values are not mistaken for echoes, even though
        // callers already check them, keeping the helper safe in isolation.
        assert!(!title_is_shell_cwd_echo("", cwd, None));
        assert!(!title_is_shell_cwd_echo("   ", cwd, None));
    }

    #[test]
    fn set_surface_title_auto_drops_shell_ps1_echo_on_terminal() {
        // Regression guard: shell PS1-shaped OSC 0/2 titles (`user@host: /path`)
        // emitted on every prompt must not overwrite cwd-based folder labels.
        // flowmux's cwd-notify flow applies folder names through
        // set_surface_cwd, so OSC 0/2 echoes should be ignored.
        let pane_id = PaneId::new();
        let cwd = PathBuf::from("/tmp/flowmux-shell-echo-test");
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("flowmux-shell-...", Some(cwd.clone())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // PS1 echoes: all should be ignored, returning false and keeping title.
        assert!(!pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "junsu@host: /tmp/flowmux-shell-echo-test".into(),
        ));
        assert!(!pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "(jammy)junsu@host:/tmp/flowmux-shell-echo-test".into(),
        ));
        assert_eq!(
            pane.surface_title(pane_id, surface_id),
            Some("flowmux-shell-...")
        );

        // External program titles, such as vi, still apply normally.
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "vim src/main.rs".into(),));
        assert_eq!(
            pane.surface_title(pane_id, surface_id),
            Some("vim src/main.rs")
        );
    }

    #[test]
    fn find_surface_returns_clone_of_matching_surface() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        let added = PaneSurface::browser("Docs", "https://docs.example.org".into());
        let added_id = added.id;
        assert_eq!(
            pane.add_surface_to_leaf(pane_id, added.clone()),
            Some(added_id)
        );

        let found = pane.find_surface(pane_id, added_id).expect("must find");
        assert_eq!(found.id, added_id);
        assert_eq!(found.title, "Docs");
        assert!(matches!(
            found.kind,
            SurfaceKind::Browser { initial_url: Some(ref u) } if u == "https://docs.example.org"
        ));
    }

    #[test]
    fn find_surface_returns_none_for_unknown_pane_or_surface() {
        let pane_id = PaneId::new();
        let pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        assert!(pane.find_surface(PaneId::new(), SurfaceId::new()).is_none());
        assert!(pane.find_surface(pane_id, SurfaceId::new()).is_none());
    }

    #[test]
    fn parent_split_id_finds_immediate_owner() {
        let l = PaneId::new();
        let r = PaneId::new();
        let split_id = PaneId::new();
        let tree = Pane::Split {
            id: split_id,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: r,
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        assert_eq!(tree.parent_split_id(l), Some(split_id));
        assert_eq!(tree.parent_split_id(r), Some(split_id));
        // A Split is not its own child, so root lookup returns None.
        assert_eq!(tree.parent_split_id(split_id), None);
        assert_eq!(tree.parent_split_id(PaneId::new()), None);
    }

    #[test]
    fn parent_split_id_walks_into_nested_tree() {
        let outer = PaneId::new();
        let inner = PaneId::new();
        let l = PaneId::new();
        let m = PaneId::new();
        let r = PaneId::new();
        let tree = Pane::Split {
            id: outer,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Split {
                id: inner,
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(Pane::Leaf {
                    id: m,
                    content: PaneContent::tabbed_terminal("M", None),
                }),
                second: Box::new(Pane::Leaf {
                    id: r,
                    content: PaneContent::tabbed_terminal("R", None),
                }),
            }),
        };
        assert_eq!(tree.parent_split_id(l), Some(outer));
        assert_eq!(tree.parent_split_id(inner), Some(outer));
        assert_eq!(tree.parent_split_id(m), Some(inner));
        assert_eq!(tree.parent_split_id(r), Some(inner));
    }

    #[test]
    fn set_split_ratio_updates_matching_node_only() {
        let outer = PaneId::new();
        let inner = PaneId::new();
        let l = PaneId::new();
        let m = PaneId::new();
        let r = PaneId::new();
        let mut tree = Pane::Split {
            id: outer,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Split {
                id: inner,
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(Pane::Leaf {
                    id: m,
                    content: PaneContent::tabbed_terminal("M", None),
                }),
                second: Box::new(Pane::Leaf {
                    id: r,
                    content: PaneContent::tabbed_terminal("R", None),
                }),
            }),
        };
        assert!(tree.set_split_ratio(outer, 0.7));
        assert!(tree.set_split_ratio(inner, 0.3));
        assert!(!tree.set_split_ratio(PaneId::new(), 0.5));
        assert!(!tree.set_split_ratio(l, 0.5));

        let Pane::Split {
            ratio: outer_r,
            second,
            ..
        } = &tree
        else {
            panic!("expected outer split")
        };
        assert!((outer_r - 0.7).abs() < 0.001);
        let Pane::Split { ratio: inner_r, .. } = second.as_ref() else {
            panic!("expected inner split")
        };
        assert!((inner_r - 0.3).abs() < 0.001);
    }

    #[test]
    fn set_split_ratio_clamps_extreme_values() {
        let split_id = PaneId::new();
        let mut tree = Pane::Split {
            id: split_id,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        assert!(tree.set_split_ratio(split_id, 1.0));
        let Pane::Split { ratio, .. } = &tree else {
            unreachable!()
        };
        assert!((ratio - 0.95).abs() < 0.001);

        assert!(tree.set_split_ratio(split_id, 0.0));
        let Pane::Split { ratio, .. } = &tree else {
            unreachable!()
        };
        assert!((ratio - 0.05).abs() < 0.001);
    }

    #[test]
    fn set_split_ratio_returns_false_when_unchanged() {
        let split_id = PaneId::new();
        let mut tree = Pane::Split {
            id: split_id,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        assert!(!tree.set_split_ratio(split_id, 0.5));
    }

    #[test]
    fn find_leaf_content_returns_clone_for_matching_leaf() {
        let leaf = PaneId::new();
        let tree = Pane::Leaf {
            id: leaf,
            content: PaneContent::tabbed_terminal("solo", None),
        };
        let content = tree.find_leaf_content(leaf).expect("leaf must match");
        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "solo");

        // Other PaneIds return None.
        assert!(tree.find_leaf_content(PaneId::new()).is_none());
    }

    #[test]
    fn find_leaf_content_walks_split_tree() {
        let l = PaneId::new();
        let r = PaneId::new();
        let tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("left", None),
            }),
            second: Box::new(Pane::Leaf {
                id: r,
                content: PaneContent::tabbed_browser("Docs", "https://r.test".into()),
            }),
        };
        let PaneContent::Tabs { surfaces, .. } = tree.find_leaf_content(l).unwrap() else {
            panic!("expected tabs")
        };
        assert_eq!(surfaces[0].title, "left");
        let PaneContent::Tabs { surfaces, .. } = tree.find_leaf_content(r).unwrap() else {
            panic!("expected tabs")
        };
        assert_eq!(surfaces[0].title, "Docs");

        // A split PaneId is not a leaf, so return None.
        let split_id = match &tree {
            Pane::Split { id, .. } => *id,
            _ => unreachable!(),
        };
        assert!(tree.find_leaf_content(split_id).is_none());
    }

    #[test]
    fn split_leaf_preserves_target_pane_id_and_creates_fresh_sibling() {
        // Core assumption for incremental split: target keeps its PaneId after
        // splitting, and the sibling receives a new PaneId. If this breaks, GTK
        // PaneRegistry::pane_frame(target_pane) lookup misses and can rebuild
        // the wrong pane.
        let target = PaneId::new();
        let mut tree = Pane::Leaf {
            id: target,
            content: PaneContent::tabbed_terminal("orig", Some("/tmp/orig".into())),
        };
        let new_pane = tree
            .split_leaf(
                target,
                SplitDirection::Vertical,
                0.5,
                PaneContent::tabbed_terminal("fresh", Some("/tmp/orig".into())),
            )
            .expect("split must succeed");
        assert_ne!(new_pane, target);

        let mut leaves = Vec::new();
        tree.for_each_leaf(|id| leaves.push(id));
        assert!(leaves.contains(&target));
        assert!(leaves.contains(&new_pane));

        // Target content remains original; new_pane has fresh content.
        let target_content = tree.find_leaf_content(target).unwrap();
        let new_content = tree.find_leaf_content(new_pane).unwrap();
        let (
            PaneContent::Tabs {
                surfaces: t_surfs, ..
            },
            PaneContent::Tabs {
                surfaces: n_surfs, ..
            },
        ) = (&target_content, &new_content)
        else {
            panic!("expected tabbed content for both")
        };
        assert_eq!(t_surfs[0].title, "orig");
        assert_eq!(n_surfs[0].title, "fresh");
    }

    #[test]
    fn split_leaf_inside_existing_split_preserves_neighbor_pane_id() {
        // Splitting a pane already inside a split tree preserves the other
        // sibling pane's PaneId, so GTK can keep reusing that sibling's gtk::Frame.
        let l = PaneId::new();
        let r = PaneId::new();
        let mut tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: r,
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };

        let new_under_l = tree
            .split_leaf(
                l,
                SplitDirection::Horizontal,
                0.5,
                PaneContent::tabbed_terminal("L2", None),
            )
            .unwrap();

        // r remains a leaf.
        assert!(matches!(
            tree.find_leaf_content(r),
            Some(PaneContent::Tabs { .. })
        ));
        // l survives as part of the new split and keeps its PaneId.
        assert!(matches!(
            tree.find_leaf_content(l),
            Some(PaneContent::Tabs { .. })
        ));
        // New sibling is registered.
        assert!(tree.find_leaf_content(new_under_l).is_some());
        assert_ne!(new_under_l, l);
        assert_ne!(new_under_l, r);
    }

    #[test]
    fn find_surface_walks_into_split_branches() {
        let left_id = PaneId::new();
        let right_id = PaneId::new();
        let mut tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: left_id,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: right_id,
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        let added = PaneSurface::browser("RBrowser", "https://r.test".into());
        let added_id = added.id;
        assert_eq!(tree.add_surface_to_leaf(right_id, added), Some(added_id));

        // Wrong (pane, surface) pairing returns None because the surface exists
        // only in the right pane.
        assert!(tree.find_surface(left_id, added_id).is_none());
        let found = tree.find_surface(right_id, added_id).unwrap();
        assert_eq!(found.id, added_id);
        assert_eq!(found.title, "RBrowser");
    }

    /// Pane-internal tab reorder scenarios. Covers preserving the active tab by
    /// surface_id, moving mixed terminal/browser tabs, index clamping, and no-op
    /// branches.
    #[test]
    fn reorder_surface_moves_first_to_last_and_preserves_active() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        let b_id = b.id;
        let c = PaneSurface::browser("c", "https://c.test".into());
        let c_id = c.id;
        pane.add_surface_to_leaf(pane_id, b).unwrap();
        pane.add_surface_to_leaf(pane_id, c).unwrap();
        // Restore active to a; c was added last and became active.
        assert!(pane.set_active_surface(pane_id, a_id));

        assert!(pane.reorder_surface_in_leaf(pane_id, a_id, 2));

        let Pane::Leaf {
            content: PaneContent::Tabs { active, surfaces },
            ..
        } = &pane
        else {
            panic!("expected tabbed leaf")
        };
        let order: Vec<SurfaceId> = surfaces.iter().map(|s| s.id).collect();
        assert_eq!(order, vec![b_id, c_id, a_id]);
        // a moved, but active should still be a.
        assert_eq!(*active, a_id);
    }

    #[test]
    fn reorder_surface_moves_last_to_first() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        let b_id = b.id;
        let c = PaneSurface::terminal("c", None);
        let c_id = c.id;
        pane.add_surface_to_leaf(pane_id, b).unwrap();
        pane.add_surface_to_leaf(pane_id, c).unwrap();

        assert!(pane.reorder_surface_in_leaf(pane_id, c_id, 0));

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![c_id, a_id, b_id]
        );
    }

    #[test]
    fn reorder_surface_clamps_target_beyond_len() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        let b_id = b.id;
        pane.add_surface_to_leaf(pane_id, b).unwrap();

        // target_index=999 -> clamp to end -> b, a.
        assert!(pane.reorder_surface_in_leaf(pane_id, a_id, 999));

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![b_id, a_id]
        );
    }

    #[test]
    fn reorder_surface_same_position_returns_false() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        pane.add_surface_to_leaf(pane_id, b).unwrap();

        // a is already at index 0, so moving to 0 is a no-op.
        assert!(!pane.reorder_surface_in_leaf(pane_id, a_id, 0));
        // Even out-of-range clamps to its current end position, so no-op.
        let last = pane
            .find_surface(
                pane_id,
                match &pane {
                    Pane::Leaf {
                        content: PaneContent::Tabs { surfaces, .. },
                        ..
                    } => surfaces.last().unwrap().id,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .id;
        assert!(!pane.reorder_surface_in_leaf(pane_id, last, 100));
    }

    #[test]
    fn reorder_surface_unknown_surface_returns_false() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let missing = SurfaceId::new();
        assert!(!pane.reorder_surface_in_leaf(pane_id, missing, 0));
    }

    #[test]
    fn reorder_surface_single_tab_is_noop() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("only", None),
        };
        let only = pane.active_surface_id(pane_id).unwrap();
        assert!(!pane.reorder_surface_in_leaf(pane_id, only, 0));
        assert!(!pane.reorder_surface_in_leaf(pane_id, only, 5));
    }

    /// Create two terminals and one browser tab, move the middle browser tab to
    /// both ends, and verify order plus active preservation.
    #[test]
    fn reorder_surface_mixed_terminal_and_browser() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("term", None),
        };
        let term_id = pane.active_surface_id(pane_id).unwrap();
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        let term2 = PaneSurface::terminal("term2", None);
        let term2_id = term2.id;
        pane.add_surface_to_leaf(pane_id, browser).unwrap();
        pane.add_surface_to_leaf(pane_id, term2).unwrap();
        // Make the middle browser tab active.
        assert!(pane.set_active_surface(pane_id, browser_id));

        // Middle -> first.
        assert!(pane.reorder_surface_in_leaf(pane_id, browser_id, 0));
        assert_active_order(&pane, pane_id, browser_id, &[browser_id, term_id, term2_id]);

        // First -> last.
        assert!(pane.reorder_surface_in_leaf(pane_id, browser_id, 2));
        assert_active_order(&pane, pane_id, browser_id, &[term_id, term2_id, browser_id]);
    }

    /// Reorder a tab in a deep leaf of a split tree. Other leaves must be unaffected.
    #[test]
    fn reorder_surface_walks_into_split_branches() {
        let left_id = PaneId::new();
        let right_id = PaneId::new();
        let mut tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: left_id,
                content: PaneContent::tabbed_terminal("L0", None),
            }),
            second: Box::new(Pane::Leaf {
                id: right_id,
                content: PaneContent::tabbed_terminal("R0", None),
            }),
        };
        let r0 = tree.active_surface_id(right_id).unwrap();
        let r1 = PaneSurface::terminal("R1", None);
        let r1_id = r1.id;
        let r2 = PaneSurface::browser("R2", "https://r2.test".into());
        let r2_id = r2.id;
        tree.add_surface_to_leaf(right_id, r1).unwrap();
        tree.add_surface_to_leaf(right_id, r2).unwrap();
        let l0 = tree.active_surface_id(left_id).unwrap();

        // Move R2, the last tab in the right pane, to first.
        assert!(tree.reorder_surface_in_leaf(right_id, r2_id, 0));
        assert_active_order(&tree, right_id, r2_id, &[r2_id, r0, r1_id]);

        // Left pane should stay unchanged.
        assert_active_order(&tree, left_id, l0, &[l0]);

        // Wrong (pane, surface) pairing returns false.
        assert!(!tree.reorder_surface_in_leaf(left_id, r2_id, 0));
    }

    fn assert_active_order(
        pane: &Pane,
        target: PaneId,
        expected_active: SurfaceId,
        expected_order: &[SurfaceId],
    ) {
        fn find_tabs(p: &Pane, target: PaneId) -> Option<&PaneContent> {
            match p {
                Pane::Leaf { id, content } if *id == target => Some(content),
                Pane::Leaf { .. } => None,
                Pane::Split { first, second, .. } => {
                    find_tabs(first, target).or_else(|| find_tabs(second, target))
                }
            }
        }
        let Some(PaneContent::Tabs { active, surfaces }) = find_tabs(pane, target) else {
            panic!("target leaf {target} not found or not tabbed");
        };
        assert_eq!(*active, expected_active);
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            expected_order
        );
    }

    #[test]
    fn workspace_roundtrips_through_json() {
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "demo".into(),
            custom_title: None,
            root_dir: PathBuf::from("/tmp/demo"),
            git: None,
            listening_ports: vec![3000, 5173],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: PaneId::new(),
                    content: PaneContent::Terminal { pid: None },
                },
            }],
            color: None,
        };
        let s = serde_json::to_string(&ws).unwrap();
        let back: Workspace = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, ws.name);
        assert_eq!(back.custom_title, None);
    }

    #[test]
    fn display_title_falls_back_to_name_when_custom_unset() {
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: None,
            root_dir: PathBuf::from("/tmp/auto"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        };
        assert_eq!(ws.display_title(), "auto");
    }

    #[test]
    fn display_title_prefers_custom_title_when_set() {
        let mut ws = Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: Some("My Project".into()),
            root_dir: PathBuf::from("/tmp/auto"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        };
        assert_eq!(ws.display_title(), "My Project");

        // custom_title wins even when automatic updates change name.
        ws.name = "updated-auto".into();
        assert_eq!(ws.display_title(), "My Project");
    }

    #[test]
    fn display_title_treats_empty_custom_as_unset() {
        // Defensive: if any path stores an empty string, display returns to automatic mode.
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: Some("".into()),
            root_dir: PathBuf::from("/tmp/auto"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        };
        assert_eq!(ws.display_title(), "auto");
    }

    #[test]
    fn workspace_loads_legacy_state_without_custom_title() {
        // Older state.json files lack custom_title.
        // #[serde(default)] should load it as None.
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "old-project",
            "root_dir": "/tmp/old",
            "git": null,
            "surfaces": [],
            "color": null
        }"#;
        let ws: Workspace = serde_json::from_str(json).unwrap();
        assert_eq!(ws.name, "old-project");
        assert_eq!(ws.custom_title, None);
        assert_eq!(ws.display_title(), "old-project");
    }

    // ----- right-sibling browser reuse (Phase 2) ----------------------

    fn term_leaf(id: PaneId) -> Pane {
        Pane::Leaf {
            id,
            content: PaneContent::tabbed_terminal("term", None),
        }
    }

    fn browser_leaf(id: PaneId) -> Pane {
        Pane::Leaf {
            id,
            content: PaneContent::tabbed_browser("Browser", "https://x".into()),
        }
    }

    fn vsplit(first: Pane, second: Pane) -> Pane {
        Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn hsplit(first: Pane, second: Pane) -> Pane {
        Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn pane_has_browser_surface_detects_tabs_and_legacy_shapes() {
        let leaf_id = PaneId::new();
        let pane = Pane::Leaf {
            id: leaf_id,
            content: PaneContent::tabbed_browser("Browser", "https://x".into()),
        };
        assert!(pane.pane_has_browser_surface(leaf_id));

        let term_id = PaneId::new();
        let term = term_leaf(term_id);
        assert!(!term.pane_has_browser_surface(term_id));

        let legacy_id = PaneId::new();
        let legacy = Pane::Leaf {
            id: legacy_id,
            content: PaneContent::Browser {
                url: "https://x".into(),
            },
        };
        assert!(legacy.pane_has_browser_surface(legacy_id));
    }

    #[test]
    fn right_sibling_returns_none_when_no_split_exists() {
        let term_id = PaneId::new();
        let pane = term_leaf(term_id);
        assert!(pane.find_right_sibling_browser_leaf(term_id).is_none());
    }

    #[test]
    fn right_sibling_finds_browser_directly_to_the_right() {
        let term_id = PaneId::new();
        let browser_id = PaneId::new();
        let pane = vsplit(term_leaf(term_id), browser_leaf(browser_id));
        assert_eq!(
            pane.find_right_sibling_browser_leaf(term_id),
            Some(browser_id),
            "vertical split with browser on right should reuse"
        );
    }

    #[test]
    fn right_sibling_skips_horizontal_split_below() {
        let term_id = PaneId::new();
        let browser_id = PaneId::new();
        let pane = hsplit(term_leaf(term_id), browser_leaf(browser_id));
        assert!(
            pane.find_right_sibling_browser_leaf(term_id).is_none(),
            "horizontal split is up/down, not right — must not reuse"
        );
    }

    #[test]
    fn right_sibling_does_not_reuse_when_caller_is_on_the_right() {
        // Browser is on the LEFT of the caller — reuse must not pick it.
        let term_id = PaneId::new();
        let browser_id = PaneId::new();
        let pane = vsplit(browser_leaf(browser_id), term_leaf(term_id));
        assert!(
            pane.find_right_sibling_browser_leaf(term_id).is_none(),
            "right-sibling search must not pick a left sibling"
        );
    }

    #[test]
    fn right_sibling_picks_nearest_ancestor_first() {
        // Tree:
        //
        //         vsplit
        //        /      \
        //   vsplit    browser_far  (far right)
        //   /     \
        // term  browser_near       (immediate right)
        //
        // We must pick browser_near, not browser_far — closest ancestor wins.
        let term_id = PaneId::new();
        let near_id = PaneId::new();
        let far_id = PaneId::new();
        let pane = vsplit(
            vsplit(term_leaf(term_id), browser_leaf(near_id)),
            browser_leaf(far_id),
        );
        assert_eq!(pane.find_right_sibling_browser_leaf(term_id), Some(near_id));
    }

    #[test]
    fn right_sibling_falls_through_to_outer_ancestor_when_immediate_right_is_terminal() {
        // Tree:
        //
        //          vsplit
        //         /      \
        //     vsplit    browser_far
        //     /     \
        //  term   term_neighbor
        //
        // Immediate right (term_neighbor) is not a browser → walk up to
        // outer vsplit → second is browser_far → that's the result.
        let term_id = PaneId::new();
        let neighbor_id = PaneId::new();
        let far_id = PaneId::new();
        let pane = vsplit(
            vsplit(term_leaf(term_id), term_leaf(neighbor_id)),
            browser_leaf(far_id),
        );
        assert_eq!(pane.find_right_sibling_browser_leaf(term_id), Some(far_id));
    }

    #[test]
    fn right_sibling_returns_first_browser_in_complex_subtree() {
        // Right subtree is itself split — pick the leftmost (DFS-first)
        // browser leaf inside it, which is the visually "closer" one.
        let term_id = PaneId::new();
        let browser_a = PaneId::new();
        let browser_b = PaneId::new();
        let pane = vsplit(
            term_leaf(term_id),
            hsplit(browser_leaf(browser_a), browser_leaf(browser_b)),
        );
        assert_eq!(
            pane.find_right_sibling_browser_leaf(term_id),
            Some(browser_a)
        );
    }

    #[test]
    fn right_sibling_returns_none_when_all_right_subtrees_are_terminals() {
        let term_id = PaneId::new();
        let other = PaneId::new();
        let pane = vsplit(term_leaf(term_id), term_leaf(other));
        assert!(pane.find_right_sibling_browser_leaf(term_id).is_none());
    }

    #[test]
    fn id_from_str_accepts_bare_uuid_and_cmux_prefixes() {
        let pane = PaneId::new();
        let s = pane.to_string();
        // bare UUID
        assert_eq!(PaneId::from_str(&s).unwrap(), pane);
        // surface: prefix (cmux-compatible)
        assert_eq!(PaneId::from_str(&format!("surface:{s}")).unwrap(), pane);
        // pane: prefix
        assert_eq!(PaneId::from_str(&format!("pane:{s}")).unwrap(), pane);
        // arbitrary label is ignored before ':' — keeps the rule simple
        // and forward-compatible with future label types.
        assert_eq!(PaneId::from_str(&format!("foo:{s}")).unwrap(), pane);

        let ws = WorkspaceId::new();
        let s = ws.to_string();
        assert_eq!(
            WorkspaceId::from_str(&format!("workspace:{s}")).unwrap(),
            ws
        );
    }

    #[test]
    fn id_from_str_rejects_non_uuid_after_prefix() {
        assert!(PaneId::from_str("surface:not-a-uuid").is_err());
        assert!(PaneId::from_str("not-a-uuid").is_err());
    }

    #[test]
    fn placement_strategy_serializes_as_snake_case() {
        let json = serde_json::to_string(&PlacementStrategy::ReuseRightSibling).unwrap();
        assert_eq!(json, r#""reuse_right_sibling""#);
        let json = serde_json::to_string(&PlacementStrategy::SplitRight).unwrap();
        assert_eq!(json, r#""split_right""#);
        // Round-trip both variants.
        let back: PlacementStrategy = serde_json::from_str(r#""reuse_right_sibling""#).unwrap();
        assert_eq!(back, PlacementStrategy::ReuseRightSibling);
    }

    /// Stale OSC 0/2 titles ("Claude Code", "codex", "vim foo") that
    /// were captured into the persisted state must NOT survive into
    /// the next launch — the process that emitted them is gone, so
    /// the tab should restart with a cwd-derived title.
    #[test]
    fn normalize_resets_unlocked_title_to_cwd_after_relaunch() {
        let cwd = std::path::PathBuf::from("/home/u/dev/os/flowmux");
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "Claude Code".into(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: Some(cwd.clone()),
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(changed, "stale OSC title should be reset");
        assert_eq!(surface.title, terminal_tab_title_for_cwd(Some(&cwd)));
        assert!(
            !surface.title_locked,
            "must not auto-lock — the title was never the user's intent"
        );
    }

    #[test]
    fn agent_presence_is_never_persisted() {
        let surface = PaneSurface {
            id: SurfaceId::new(),
            title: "Claude Code".into(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: None,
            },
            agent: Some(AgentPresence::new(
                "claude",
                AgentActivity::Running,
                Some(4321),
            )),
        };
        let json = serde_json::to_string(&surface).unwrap();
        assert!(
            !json.contains("agent") && !json.contains("claude"),
            "agent presence must be skipped from serialization, got: {json}"
        );
        // Round-trips back with no agent (runtime-only field); the rest
        // of the surface survives.
        let back: PaneSurface = serde_json::from_str(&json).unwrap();
        assert!(back.agent.is_none());
        assert_eq!(back.id, surface.id);
        assert_eq!(back.title, surface.title);
    }

    #[test]
    fn agent_activity_maps_to_public_status() {
        assert_eq!(AgentActivity::Running.status(), AgentStatus::Working);
        assert_eq!(AgentActivity::NeedsInput.status(), AgentStatus::Blocked);
        assert_eq!(AgentActivity::Idle.status(), AgentStatus::Idle);
    }

    #[test]
    fn agent_presence_ignores_stale_seq() {
        let mut presence = AgentPresence::new("codex", AgentActivity::Running, Some(7));
        presence.seq = Some(20);
        let applied = presence.apply_report(
            AgentStatusReport {
                name: "codex".into(),
                status: Some(AgentStatus::Idle),
                activity: Some(AgentActivity::Idle),
                pid: Some(7),
                source: Some("flowmux:hook".into()),
                seq: Some(19),
                message: None,
                custom_status: None,
                session_id: None,
            },
            true,
        );
        assert!(!applied);
        assert_eq!(presence.public_status(), AgentStatus::Working);
    }

    #[test]
    fn apply_report_screen_scan_keeps_proc_owned_identity() {
        // Regression: a `claude` pane whose scrollback mentions another agent
        // (e.g. an AI chat *about* `cline`) must not be relabeled. Process truth
        // owns the identity; a screen scan may only refine the status.
        let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
        presence.source = Some(AGENT_SOURCE_PROC.to_string());
        let applied = presence.apply_report(
            AgentStatusReport {
                name: "cline".into(),
                status: Some(AgentStatus::Working),
                activity: Some(AgentActivity::Running),
                pid: None,
                source: Some("flowmux:screen".into()),
                seq: None,
                message: None,
                custom_status: None,
                session_id: None,
            },
            true,
        );
        assert!(applied);
        assert_eq!(
            presence.name, "claude",
            "screen must not rename a proc-owned presence"
        );
        assert_eq!(presence.source.as_deref(), Some(AGENT_SOURCE_PROC));
        assert_eq!(
            presence.status,
            AgentStatus::Working,
            "status still refines"
        );
    }

    #[test]
    fn working_to_idle_in_hidden_surface_becomes_done_until_seen() {
        let mut presence = AgentPresence::new("claude", AgentActivity::Running, None);
        presence.seq = Some(1);
        let applied = presence.apply_report(
            AgentStatusReport {
                name: "claude".into(),
                status: Some(AgentStatus::Idle),
                activity: Some(AgentActivity::Idle),
                pid: None,
                source: Some("flowmux:hook".into()),
                seq: Some(2),
                message: None,
                custom_status: None,
                session_id: None,
            },
            false,
        );
        assert!(applied);
        assert_eq!(presence.status, AgentStatus::Idle);
        assert_eq!(presence.public_status(), AgentStatus::Done);
        assert!(presence.mark_seen());
        assert_eq!(presence.public_status(), AgentStatus::Idle);
    }

    #[test]
    fn initial_idle_agent_report_in_hidden_surface_stays_idle() {
        let presence = AgentPresence::from_report(
            AgentStatusReport {
                name: "cline".into(),
                status: Some(AgentStatus::Idle),
                activity: Some(AgentActivity::Idle),
                pid: Some(42),
                source: Some("flowmux:hook".into()),
                seq: Some(1),
                message: None,
                custom_status: None,
                session_id: None,
            },
            false,
        )
        .expect("idle report should create presence");

        assert_eq!(presence.status, AgentStatus::Idle);
        assert_eq!(presence.public_status(), AgentStatus::Idle);
        assert!(presence.seen);
    }

    #[test]
    fn agent_presence_replaces_transient_message_metadata() {
        let mut presence = AgentPresence::new("claude", AgentActivity::NeedsInput, None);
        presence.message = Some("approval needed".into());
        presence.custom_status = Some("waiting".into());
        presence.session_id = Some("session-1".into());
        presence.seq = Some(1);

        let applied = presence.apply_report(
            AgentStatusReport {
                name: "claude".into(),
                status: Some(AgentStatus::Working),
                activity: Some(AgentActivity::Running),
                pid: None,
                source: Some("flowmux:hook".into()),
                seq: Some(2),
                message: None,
                custom_status: None,
                session_id: None,
            },
            true,
        );

        assert!(applied);
        assert_eq!(presence.public_status(), AgentStatus::Working);
        assert_eq!(presence.message, None);
        assert_eq!(presence.custom_status, None);
        assert_eq!(presence.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn hook_report_uses_opencode_name_when_surface_title_has_oc_prefix() {
        let surface = PaneSurface::terminal("OC | greeting", None);
        let surface_id = surface.id;
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent(
                surface_id,
                AgentStatusReport {
                    name: "claude".into(),
                    status: Some(AgentStatus::Idle),
                    activity: Some(AgentActivity::Idle),
                    pid: None,
                    source: Some("flowmux:hook".into()),
                    seq: Some(1),
                    message: None,
                    custom_status: None,
                    session_id: Some("ses-opencode".into()),
                },
                true,
            ),
            Some(true)
        );
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected leaf pane");
        };
        let agent = surfaces[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
    }

    #[test]
    fn agent_presence_status_text_prefers_custom_status_then_message() {
        let mut presence = AgentPresence::new("codex", AgentActivity::Running, None);
        presence.message = Some("running tests".into());
        presence.custom_status = Some("reviewing patch".into());
        assert_eq!(presence.status_text(), Some("reviewing patch"));

        presence.custom_status = Some("   ".into());
        assert_eq!(presence.status_text(), Some("running tests"));

        presence.message = Some(" ".into());
        assert_eq!(presence.status_text(), None);
    }

    fn terminal_surface_with_agent(
        title: &str,
        cwd: &str,
        agent_name: &str,
        status: AgentStatus,
    ) -> PaneSurface {
        let mut surface = PaneSurface::terminal(title, Some(std::path::PathBuf::from(cwd)));
        let mut presence = AgentPresence::new(agent_name, status.to_activity(), None);
        presence.status = status;
        presence.custom_status = Some(format!("{agent_name} status"));
        surface.agent = Some(presence);
        surface
    }

    fn workspace_with_agent_leaves(leaves: Vec<(PaneId, PaneSurface)>) -> Workspace {
        fn leaf(id: PaneId, surface: PaneSurface) -> Pane {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs {
                    active: surface.id,
                    surfaces: vec![surface],
                },
            }
        }

        let root = leaves
            .into_iter()
            .map(|(id, surface)| leaf(id, surface))
            .reduce(|first, second| Pane::Split {
                id: PaneId::new(),
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            })
            .expect("workspace needs at least one leaf");

        Workspace {
            id: WorkspaceId::new(),
            name: "agents".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: Vec::new(),
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: root,
            }],
            color: None,
        }
    }

    fn workspace_with_tab_leaves(leaves: Vec<(PaneId, Vec<PaneSurface>)>) -> Workspace {
        fn leaf(id: PaneId, surfaces: Vec<PaneSurface>) -> Pane {
            let active = surfaces
                .first()
                .map(|surface| surface.id)
                .expect("test leaf needs at least one surface");
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { active, surfaces },
            }
        }

        let root = leaves
            .into_iter()
            .map(|(id, surfaces)| leaf(id, surfaces))
            .reduce(|first, second| Pane::Split {
                id: PaneId::new(),
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            })
            .expect("workspace needs at least one leaf");

        Workspace {
            id: WorkspaceId::new(),
            name: "agents".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: Vec::new(),
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: root,
            }],
            color: None,
        }
    }

    #[test]
    fn workspace_agent_blocks_sort_by_status_then_recent_focus() {
        let working_old = PaneId::new();
        let blocked = PaneId::new();
        let working_recent = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![
            (
                working_old,
                terminal_surface_with_agent("old", "/tmp/old", "codex", AgentStatus::Working),
            ),
            (
                blocked,
                terminal_surface_with_agent(
                    "blocked",
                    "/tmp/blocked",
                    "claude",
                    AgentStatus::Blocked,
                ),
            ),
            (
                working_recent,
                terminal_surface_with_agent(
                    "recent",
                    "/tmp/recent",
                    "opencode",
                    AgentStatus::Working,
                ),
            ),
        ]);

        let blocks = ws.collect_agent_blocks(&[working_recent, working_old, blocked]);
        assert_eq!(
            blocks.iter().map(|block| block.pane).collect::<Vec<_>>(),
            vec![blocked, working_recent, working_old]
        );
        assert_eq!(blocks[0].status, AgentStatus::Blocked);
        assert_eq!(blocks[0].status_text.as_deref(), Some("claude status"));
        assert_eq!(blocks[0].cwd.as_deref(), Some("/tmp/blocked"));
    }

    #[test]
    fn workspace_agent_blocks_exclude_unknown_status() {
        let unknown = PaneId::new();
        let idle = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![
            (
                unknown,
                terminal_surface_with_agent(
                    "unknown",
                    "/tmp/unknown",
                    "codex",
                    AgentStatus::Unknown,
                ),
            ),
            (
                idle,
                terminal_surface_with_agent("idle", "/tmp/idle", "claude", AgentStatus::Idle),
            ),
        ]);

        let blocks = ws.collect_agent_blocks(&[unknown, idle]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].pane, idle);
        assert_eq!(blocks[0].status, AgentStatus::Idle);
    }

    #[test]
    fn agent_bar_model_hidden_when_no_agents_exist() {
        let pane = PaneId::new();
        let ws = workspace_with_tab_leaves(vec![(
            pane,
            vec![PaneSurface::terminal("shell", Some("/tmp/shell".into()))],
        )]);

        let model = collect_agent_bar_model([&ws]);

        assert!(!model.visible);
        assert!(model.items.is_empty());
    }

    #[test]
    fn agent_bar_model_collects_one_agent_with_name_and_status_text() {
        let pane = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![(
            pane,
            terminal_surface_with_agent("codex", "/tmp/codex", "codex", AgentStatus::Working),
        )]);

        let model = collect_agent_bar_model([&ws]);

        assert!(model.visible);
        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].workspace, ws.id);
        assert_eq!(model.items[0].pane, pane);
        assert_eq!(model.items[0].agent_name, "codex");
        assert_eq!(model.items[0].status_text, "codex status");
        assert_eq!(model.items[0].visual_status, AgentBarVisualStatus::Working);
    }

    #[test]
    fn agent_bar_model_uses_workspace_color_for_item_stripe() {
        let pane = PaneId::new();
        let mut ws = workspace_with_tab_leaves(vec![(
            pane,
            vec![
                terminal_surface_with_agent("codex", "/tmp/codex", "codex", AgentStatus::Working),
                terminal_surface_with_agent("claude", "/tmp/claude", "claude", AgentStatus::Idle),
            ],
        )]);
        ws.color = Some("#112233".into());

        let model = collect_agent_bar_model([&ws]);
        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.color.as_str())
                .collect::<Vec<_>>(),
            vec!["#112233", "#112233"]
        );

        ws.color = Some("#445566".into());
        let model = collect_agent_bar_model([&ws]);
        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.color.as_str())
                .collect::<Vec<_>>(),
            vec!["#445566", "#445566"]
        );
    }

    #[test]
    fn agent_bar_model_keeps_workspace_pane_tab_discovery_order() {
        let first_pane = PaneId::new();
        let second_pane = PaneId::new();
        let third_pane = PaneId::new();
        let ws_a = workspace_with_tab_leaves(vec![
            (
                first_pane,
                vec![
                    terminal_surface_with_agent("a1", "/tmp/a1", "codex", AgentStatus::Working),
                    terminal_surface_with_agent("a2", "/tmp/a2", "claude", AgentStatus::Idle),
                ],
            ),
            (
                second_pane,
                vec![terminal_surface_with_agent(
                    "a3",
                    "/tmp/a3",
                    "opencode",
                    AgentStatus::Blocked,
                )],
            ),
        ]);
        let ws_b = workspace_with_agent_leaves(vec![(
            third_pane,
            terminal_surface_with_agent("b1", "/tmp/b1", "cline", AgentStatus::Done),
        )]);

        let model = collect_agent_bar_model([&ws_a, &ws_b]);

        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.agent_name.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "claude", "opencode", "cline"]
        );
        assert_eq!(
            model.items.iter().map(|item| item.pane).collect::<Vec<_>>(),
            vec![first_pane, first_pane, second_pane, third_pane]
        );
    }

    #[test]
    fn agent_bar_visual_status_maps_public_statuses() {
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Working),
            Some(AgentBarVisualStatus::Working)
        );
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Idle),
            Some(AgentBarVisualStatus::Waiting)
        );
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Blocked),
            Some(AgentBarVisualStatus::Waiting)
        );
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Done),
            Some(AgentBarVisualStatus::Done)
        );
        assert_eq!(agent_bar_visual_status(AgentStatus::Unknown), None);
    }

    #[test]
    fn agent_bar_model_excludes_unknown_status() {
        let pane = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![(
            pane,
            terminal_surface_with_agent("unknown", "/tmp/unknown", "codex", AgentStatus::Unknown),
        )]);

        let model = collect_agent_bar_model([&ws]);

        assert!(!model.visible);
        assert!(model.items.is_empty());
    }

    #[test]
    fn agent_bar_text_updates_without_reordering_or_recoloring() {
        let pane = PaneId::new();
        let mut ws = workspace_with_agent_leaves(vec![(
            pane,
            terminal_surface_with_agent("codex", "/tmp/codex", "codex", AgentStatus::Working),
        )]);
        let before = collect_agent_bar_model([&ws]).items[0].clone();

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &mut ws.surfaces[0].root_pane
        else {
            panic!("expected single leaf test workspace");
        };
        surfaces[0].agent.as_mut().unwrap().custom_status = Some("running tests".into());
        let after = collect_agent_bar_model([&ws]).items[0].clone();

        assert_eq!(after.workspace, before.workspace);
        assert_eq!(after.pane, before.pane);
        assert_eq!(after.surface, before.surface);
        assert_eq!(after.agent_name, before.agent_name);
        assert_eq!(after.color, before.color);
        assert_eq!(after.status_text, "running tests");
    }

    #[test]
    fn agent_bar_surface_color_is_deterministic() {
        let surface_a = SurfaceId(uuid::Uuid::from_u128(0x11111111111111111111111111111111));
        let surface_b = SurfaceId(uuid::Uuid::from_u128(0x22222222222222222222222222222222));

        assert_eq!(
            agent_bar_color_for_surface(surface_a),
            agent_bar_color_for_surface(surface_a)
        );
        assert_ne!(
            agent_bar_color_for_surface(surface_a),
            agent_bar_color_for_surface(surface_b)
        );
        assert!(agent_bar_color_for_surface(surface_a).starts_with('#'));
        assert_eq!(agent_bar_color_for_surface(surface_a).len(), 7);
        assert!(WORKSPACE_PALETTE.contains(&agent_bar_color_for_surface(surface_a).as_str()));
    }

    #[test]
    fn pick_workspace_color_from_palette_avoids_used_until_exhausted() {
        // Every color the picker returns is a palette member.
        for seed in 0..40u128 {
            assert!(WORKSPACE_PALETTE.contains(&pick_workspace_color(&[], seed).as_str()));
        }
        // Filling slots one at a time never repeats a color while free slots
        // remain, so the first PALETTE.len() workspaces are all distinct.
        let mut used: Vec<String> = Vec::new();
        for i in 0..WORKSPACE_PALETTE.len() {
            let c = pick_workspace_color(&used, (i as u128) * 7 + 3);
            assert!(!used.contains(&c), "repeated {c} before palette exhausted");
            used.push(c);
        }
        let mut sorted = used.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            WORKSPACE_PALETTE.len(),
            "all colors used once"
        );
        // Seed varies the choice: two empty-set picks with different seeds can
        // differ (spot-check that not every seed yields the same color).
        let distinct: std::collections::HashSet<_> = (0..WORKSPACE_PALETTE.len() as u128)
            .map(|s| pick_workspace_color(&[], s))
            .collect();
        assert!(distinct.len() > 1, "seed should scatter the pick");
    }

    #[test]
    fn agent_bar_width_clamps_and_reports_ellipsize() {
        assert_eq!(
            clamp_agent_bar_item_width(80),
            AgentBarItemWidth {
                width_px: AGENT_BAR_ITEM_MIN_WIDTH_PX,
                ellipsize: false
            }
        );
        assert_eq!(
            clamp_agent_bar_item_width(126),
            AgentBarItemWidth {
                width_px: 126,
                ellipsize: false
            }
        );
        assert_eq!(
            clamp_agent_bar_item_width(320),
            AgentBarItemWidth {
                width_px: AGENT_BAR_ITEM_MAX_WIDTH_PX,
                ellipsize: true
            }
        );
    }

    #[test]
    fn agent_bar_model_reflects_agent_tab_pane_and_workspace_removal() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        let surface_a = terminal_surface_with_agent("a", "/tmp/a", "codex", AgentStatus::Working);
        let mut ended_surface =
            terminal_surface_with_agent("ended", "/tmp/ended", "claude", AgentStatus::Idle);
        ended_surface.agent = None;
        let surface_b =
            terminal_surface_with_agent("b", "/tmp/b", "opencode", AgentStatus::Blocked);
        let full = workspace_with_tab_leaves(vec![
            (pane_a, vec![surface_a.clone(), ended_surface]),
            (pane_b, vec![surface_b.clone()]),
        ]);
        assert_eq!(collect_agent_bar_model([&full]).items.len(), 2);

        let tab_closed = workspace_with_tab_leaves(vec![
            (pane_a, vec![surface_a.clone()]),
            (pane_b, vec![surface_b.clone()]),
        ]);
        assert_eq!(
            collect_agent_bar_model([&tab_closed])
                .items
                .iter()
                .map(|item| item.agent_name.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "opencode"]
        );

        let pane_closed = workspace_with_tab_leaves(vec![(pane_a, vec![surface_a])]);
        assert_eq!(
            collect_agent_bar_model([&pane_closed])
                .items
                .iter()
                .map(|item| item.agent_name.as_str())
                .collect::<Vec<_>>(),
            vec!["codex"]
        );

        let workspace_closed = collect_agent_bar_model(std::iter::empty::<&Workspace>());
        assert!(!workspace_closed.visible);
        assert!(workspace_closed.items.is_empty());
    }

    #[test]
    fn agent_notification_target_controls_blink_flags() {
        assert_eq!(
            AgentNotificationVisualFlags::for_unread(AgentNotificationTarget::AgentBar, true),
            AgentNotificationVisualFlags {
                agent_bar: true,
                workspace: false,
                desktop_toast: true
            }
        );
        assert_eq!(
            AgentNotificationVisualFlags::for_unread(AgentNotificationTarget::Workspace, true),
            AgentNotificationVisualFlags {
                agent_bar: false,
                workspace: true,
                desktop_toast: true
            }
        );
        assert_eq!(
            AgentNotificationVisualFlags::for_unread(AgentNotificationTarget::Both, false),
            AgentNotificationVisualFlags {
                agent_bar: true,
                workspace: true,
                desktop_toast: false
            }
        );
    }

    #[test]
    fn agent_notification_clear_triggers_clear_all_visual_flags() {
        let flags = AgentNotificationVisualFlags {
            agent_bar: true,
            workspace: true,
            desktop_toast: true,
        };

        for trigger in [
            AgentNotificationClearTrigger::WorkspaceClick,
            AgentNotificationClearTrigger::PaneFocus,
            AgentNotificationClearTrigger::AgentBarItemClick,
        ] {
            assert_eq!(
                clear_agent_notification_visuals(trigger, flags),
                AgentNotificationVisualFlags::default()
            );
        }
    }

    #[test]
    fn agent_notification_target_default_is_agent_bar() {
        assert_eq!(
            AgentNotificationTarget::default(),
            AgentNotificationTarget::AgentBar
        );
        assert_eq!(
            serde_json::to_string(&AgentNotificationTarget::default()).unwrap(),
            "\"agent_bar\""
        );
    }

    #[test]
    fn reconcile_creates_idle_proc_presence_when_agent_process_appears() {
        let mut slot = None;
        assert!(reconcile_surface_process_agent(&mut slot, Some("codex")));
        let p = slot.as_ref().unwrap();
        assert_eq!(p.name, "codex");
        assert_eq!(p.status, AgentStatus::Idle);
        assert_eq!(p.source.as_deref(), Some(AGENT_SOURCE_PROC));
        // Idempotent: a second identical reconcile reports no change.
        assert!(!reconcile_surface_process_agent(&mut slot, Some("codex")));
    }

    #[test]
    fn reconcile_drops_proc_presence_when_agent_process_exits() {
        let mut p = AgentPresence::new("codex", AgentActivity::Idle, None);
        p.source = Some(AGENT_SOURCE_PROC.to_string());
        let mut slot = Some(p);
        assert!(reconcile_surface_process_agent(&mut slot, None));
        assert!(slot.is_none());
    }

    #[test]
    fn reconcile_leaves_hook_owned_presence_when_process_absent() {
        // A hook-owned presence must not be dropped by the process sweep; the
        // hook (and the pid liveness sweep) own its lifecycle.
        let mut p = AgentPresence::new("claude", AgentActivity::Running, Some(42));
        p.source = Some("flowmux:hook".into());
        let mut slot = Some(p);
        assert!(!reconcile_surface_process_agent(&mut slot, None));
        assert!(slot.is_some());
    }

    #[test]
    fn reconcile_follows_agent_swap_for_proc_owned_presence() {
        let mut p = AgentPresence::new("codex", AgentActivity::Idle, None);
        p.source = Some(AGENT_SOURCE_PROC.to_string());
        let mut slot = Some(p);
        assert!(reconcile_surface_process_agent(&mut slot, Some("claude")));
        assert_eq!(slot.as_ref().unwrap().name, "claude");
    }

    #[test]
    fn reconcile_reclaims_screen_owned_presence_mislabeled_by_scrollback() {
        // A screen scan mislabeled a pane because its scrollback *mentioned*
        // another agent; the process sweep reclaims the true identity.
        let mut p = AgentPresence::new("cline", AgentActivity::Idle, None);
        p.source = Some("flowmux:screen".into());
        let mut slot = Some(p);
        assert!(reconcile_surface_process_agent(&mut slot, Some("claude")));
        let agent = slot.as_ref().unwrap();
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.source.as_deref(), Some(AGENT_SOURCE_PROC));
    }

    #[test]
    fn reconcile_is_noop_for_running_screen_owned_presence() {
        let mut p = AgentPresence::new("codex", AgentActivity::Running, None);
        p.source = Some("flowmux:screen".into());
        let mut slot = Some(p);
        assert!(!reconcile_surface_process_agent(&mut slot, Some("codex")));
        assert_eq!(slot.as_ref().unwrap().name, "codex");
    }

    #[test]
    fn screen_working_keeps_proc_ownership_then_settles_idle_without_clearing() {
        // Core regression fix: a screen scan may raise a proc-owned presence to
        // Working, but must not steal ownership — otherwise the screen-idle path
        // would later drop a still-running agent (Codex's exact failure).
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, None);
        presence.source = Some(AGENT_SOURCE_PROC.to_string());
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };
        pane.report_surface_agent_signal(
            surface_id,
            AgentStatus::Working,
            "flowmux:screen",
            Some("codex"),
            true,
        );
        // Working turn ends: proc presence settles to Idle, not cleared.
        assert_eq!(pane.settle_screen_idle(surface_id), Some(true));
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
        // Still present — a second settle is a no-op.
        assert_eq!(pane.settle_screen_idle(surface_id), Some(false));
    }

    #[test]
    fn settle_screen_idle_clears_screen_owned_presence() {
        let mut surface = PaneSurface::terminal("a", None);
        let sid = surface.id;
        let mut p = AgentPresence::new("codex", AgentActivity::Idle, None);
        p.source = Some("flowmux:screen".into());
        surface.agent = Some(p);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: sid,
                surfaces: vec![surface],
            },
        };
        assert_eq!(pane.settle_screen_idle(sid), Some(true));
        assert_eq!(pane.agent_status_rollup(), None);
    }

    #[test]
    fn pane_mark_surface_seen_clears_done() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, None);
        presence.seen = false;
        surface.agent = Some(presence);
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Done));
        assert!(pane.mark_surface_agent_seen(surface_id));
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
    }

    #[test]
    fn screen_fallback_does_not_override_claude_hook_presence() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
        presence.source = Some("flowmux:hook".into());
        presence.seq = Some(1);
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent_signal(
                surface_id,
                AgentStatus::Blocked,
                "flowmux:screen",
                Some("claude"),
                true,
            ),
            Some(false)
        );
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
    }

    #[test]
    fn screen_fallback_does_not_take_ownership_from_matching_hook_presence() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, Some(42));
        presence.source = Some("flowmux:hook".into());
        presence.seq = Some(1);
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent_signal(
                surface_id,
                AgentStatus::Working,
                "flowmux:screen",
                Some("codex"),
                true,
            ),
            Some(true)
        );
        assert_eq!(
            pane.clear_surface_agent_from_source(surface_id, "flowmux:screen"),
            Some(false)
        );
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected leaf pane");
        };
        let agent = surfaces[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.status, AgentStatus::Working);
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
        assert_eq!(agent.pid, Some(42));
    }

    #[test]
    fn screen_fallback_replaces_stale_claude_name_when_agent_signal_differs() {
        for detected_agent in ["codex", "opencode", "cline"] {
            let mut surface = PaneSurface::terminal("agent", None);
            let surface_id = surface.id;
            let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
            presence.source = Some("flowmux:hook".into());
            presence.seq = Some(1);
            surface.agent = Some(presence);
            let mut pane = Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::Tabs {
                    active: surface_id,
                    surfaces: vec![surface],
                },
            };

            assert_eq!(
                pane.report_surface_agent_signal(
                    surface_id,
                    AgentStatus::Blocked,
                    "flowmux:screen",
                    Some(detected_agent),
                    true,
                ),
                Some(true),
                "{detected_agent} should replace stale claude presence"
            );
            let Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } = &pane
            else {
                panic!("expected leaf pane");
            };
            let agent = surfaces[0].agent.as_ref().unwrap();
            assert_eq!(agent.name, detected_agent);
            assert_eq!(agent.status, AgentStatus::Blocked);
            assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
        }
    }

    #[test]
    fn screen_fallback_uses_opencode_title_before_claude_screen_text() {
        let mut surface = PaneSurface::terminal("OC | greeting", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
        presence.source = Some("flowmux:hook".into());
        presence.seq = Some(1);
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent_signal(
                surface_id,
                AgentStatus::Blocked,
                "flowmux:screen",
                Some("claude"),
                true,
            ),
            Some(true)
        );
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected leaf pane");
        };
        let agent = surfaces[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.status, AgentStatus::Blocked);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[test]
    fn clear_surface_agent_from_source_only_clears_matching_source() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, None);
        presence.source = Some("flowmux:screen".into());
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.clear_surface_agent_from_source(surface_id, "flowmux:hook"),
            Some(false)
        );
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
        assert_eq!(
            pane.clear_surface_agent_from_source(surface_id, "flowmux:screen"),
            Some(true)
        );
        assert_eq!(pane.agent_status_rollup(), None);
    }

    #[test]
    fn agent_status_rollup_uses_blocked_done_working_idle_unknown_order() {
        assert_eq!(
            rollup_agent_statuses([
                AgentStatus::Unknown,
                AgentStatus::Idle,
                AgentStatus::Working,
                AgentStatus::Done,
                AgentStatus::Blocked,
            ]),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            rollup_agent_statuses([AgentStatus::Working, AgentStatus::Done]),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn detector_reads_strong_osc_and_screen_signals() {
        assert_eq!(
            detect_agent_status_from_signals(None, Some("Codex Action Required")),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            detect_agent_status_from_signals(None, Some("Codex ⠋ working")),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            detect_agent_status_from_signals(Some("Do you want to approve this command?"), None),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            detect_agent_status_from_signals(Some("bypass permissions on"), None),
            None
        );
        assert_eq!(
            detect_agent_status_from_signals(Some("Auto-approve all enabled (Shift+Tab)"), None),
            None
        );
    }

    #[test]
    fn detector_reads_agent_name_from_osc_and_screen_signals() {
        assert_eq!(
            detect_agent_name_from_signals(None, Some("OpenCode Action Required")),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Claude is thinking"), Some("OC | greeting")),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_name_from_signals(None, Some("Claude")),
            Some("claude")
        );
        assert_eq!(
            detect_agent_name_from_signals(None, Some("Codex")),
            Some("codex")
        );
        assert_eq!(
            detect_agent_name_from_signals(None, Some("Cline")),
            Some("cline")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Claude is thinking"), None),
            Some("claude")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Codex working"), None),
            Some("codex")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Cline needs approval"), None),
            Some("cline")
        );
        assert_eq!(detect_agent_name_from_signals(Some("decline"), None), None);
        assert_eq!(
            detect_agent_name_from_signals(
                Some(
                    "Which agents should CodeGraph configure?\n\
                     Claude Code (detected), Codex CLI (detected), opencode (detected)\n\
                     Do you want to continue?"
                ),
                None
            ),
            None
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("OpenCode needs input"), None),
            Some("opencode")
        );
    }

    #[test]
    fn detector_reads_idle_agent_prompt_without_trusting_stale_scrollback() {
        assert_eq!(
            detect_agent_idle_name_from_signals(
                Some("Codex\npress / for commands\n\n\n\n\n\n\n\n\n\n\n\n"),
                None
            ),
            Some("codex")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(
                Some(
                    "Ask anything... \"Fix broken tests\"\n\
                     Sisyphus - Ultraworker · GPT-5.5 OpenAI · medium\n\
                     tab agents  ctrl+p commands"
                ),
                None
            ),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("$ echo shell ready"), Some("OpenCode")),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("   \n\n"), Some("Claude")),
            Some("claude")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("$ echo shell ready"), Some("Claude")),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(None, Some("OpenCode")),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("codex exited\n$ echo done"), None),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(
                Some(r#"printf "Codex\\npress / for commands\\n""#),
                None
            ),
            None
        );
    }

    #[test]
    fn detector_rejects_hyphenated_agent_name_prefixes() {
        assert_eq!(
            detect_agent_name_from_signals(None, Some("opencode-anycli")),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(None, Some("opencode-anycli")),
            None
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("idle /tmp/opencode-anycli"), None),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("Ask anything\n/tmp/opencode-anycli"), None),
            None
        );
    }

    #[test]
    fn normalize_keeps_user_renamed_titles() {
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "my pinned shell".into(),
            title_locked: true,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: Some("/tmp".into()),
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(!changed);
        assert_eq!(surface.title, "my pinned shell");
        assert!(surface.title_locked);
    }

    #[test]
    fn normalize_keeps_already_cwd_matching_titles() {
        let cwd = std::path::PathBuf::from("/home/u/dev/os/flowmux");
        let derived = terminal_tab_title_for_cwd(Some(&cwd));
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: derived.clone(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: Some(cwd),
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(!changed, "no-op when title already matches cwd");
        assert!(!surface.title_locked);
    }

    #[test]
    fn normalize_skips_browser_surfaces() {
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "Page Title".into(),
            title_locked: false,
            kind: SurfaceKind::Browser { initial_url: None },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(!changed);
        assert_eq!(surface.title, "Page Title");
    }

    #[test]
    fn normalize_falls_back_to_default_when_cwd_is_missing() {
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "claude".into(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: None,
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(changed);
        assert_eq!(surface.title, FALLBACK_TERMINAL_TAB_TITLE);
    }

    // ---- tab move (take / insert) ----

    fn leaf_with_tabs(pane_id: PaneId, titles: &[&str]) -> (Pane, Vec<SurfaceId>) {
        let surfaces: Vec<PaneSurface> = titles
            .iter()
            .map(|t| PaneSurface::terminal(*t, None))
            .collect();
        let ids: Vec<SurfaceId> = surfaces.iter().map(|s| s.id).collect();
        let active = ids[0];
        (
            Pane::Leaf {
                id: pane_id,
                content: PaneContent::Tabs { active, surfaces },
            },
            ids,
        )
    }

    fn leaf_tab_ids(pane: &Pane, target: PaneId) -> Vec<SurfaceId> {
        match pane.find_leaf_content(target) {
            Some(PaneContent::Tabs { surfaces, .. }) => surfaces.iter().map(|s| s.id).collect(),
            _ => panic!("expected tabs leaf"),
        }
    }

    #[test]
    fn take_surface_removes_middle_tab_and_keeps_leaf() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b", "c"]);
        let (taken, empty) = p.take_surface_from_leaf(pane, ids[1]).expect("taken");
        assert_eq!(taken.id, ids[1]);
        assert!(!empty);
        assert_eq!(leaf_tab_ids(&p, pane), vec![ids[0], ids[2]]);
    }

    #[test]
    fn take_surface_reports_empty_when_last_tab_removed() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["only"]);
        let (taken, empty) = p.take_surface_from_leaf(pane, ids[0]).expect("taken");
        assert_eq!(taken.id, ids[0]);
        assert!(empty);
    }

    #[test]
    fn take_surface_reactivates_neighbor_when_active_removed() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b", "c"]);
        // active is ids[0]; remove it
        let (_taken, empty) = p.take_surface_from_leaf(pane, ids[0]).expect("taken");
        assert!(!empty);
        match p.find_leaf_content(pane) {
            Some(PaneContent::Tabs { active, surfaces }) => {
                assert!(surfaces.iter().any(|s| s.id == active));
                assert_ne!(active, ids[0]);
            }
            _ => panic!("expected tabs"),
        }
    }

    #[test]
    fn take_surface_not_found_returns_none() {
        let pane = PaneId::new();
        let (mut p, _ids) = leaf_with_tabs(pane, &["a"]);
        assert!(p.take_surface_from_leaf(pane, SurfaceId::new()).is_none());
    }

    #[test]
    fn insert_surface_at_index_sets_active() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b", "c"]);
        let moved = PaneSurface::terminal("moved", None);
        let moved_id = moved.id;
        let got = p
            .insert_surface_into_leaf(pane, moved, 1)
            .expect("inserted");
        assert_eq!(got, moved_id);
        assert_eq!(
            leaf_tab_ids(&p, pane),
            vec![ids[0], moved_id, ids[1], ids[2]]
        );
        match p.find_leaf_content(pane) {
            Some(PaneContent::Tabs { active, .. }) => assert_eq!(active, moved_id),
            _ => panic!("expected tabs"),
        }
    }

    #[test]
    fn insert_surface_clamps_index_to_end() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b"]);
        let moved = PaneSurface::terminal("moved", None);
        let moved_id = moved.id;
        p.insert_surface_into_leaf(pane, moved, 999)
            .expect("inserted");
        assert_eq!(leaf_tab_ids(&p, pane), vec![ids[0], ids[1], moved_id]);
    }

    #[test]
    fn insert_surface_into_missing_leaf_returns_none() {
        let pane = PaneId::new();
        let (mut p, _ids) = leaf_with_tabs(pane, &["a"]);
        let moved = PaneSurface::terminal("moved", None);
        assert!(p
            .insert_surface_into_leaf(PaneId::new(), moved, 0)
            .is_none());
    }

    #[test]
    fn take_then_insert_moves_between_leaves_in_split() {
        let l1 = PaneId::new();
        let l2 = PaneId::new();
        let (left, left_ids) = leaf_with_tabs(l1, &["a", "b"]);
        let (right, _right_ids) = leaf_with_tabs(l2, &["x"]);
        let mut p = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(left),
            second: Box::new(right),
        };
        let (taken, empty) = p.take_surface_from_leaf(l1, left_ids[1]).expect("taken");
        assert!(!empty);
        let moved_id = taken.id;
        p.insert_surface_into_leaf(l2, taken, usize::MAX)
            .expect("inserted");
        assert_eq!(leaf_tab_ids(&p, l1), vec![left_ids[0]]);
        let right_ids = leaf_tab_ids(&p, l2);
        assert_eq!(right_ids.last().copied(), Some(moved_id));
        assert_eq!(right_ids.len(), 2);
    }
