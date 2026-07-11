// SPDX-License-Identifier: GPL-3.0-or-later
//! Unit tests for the GUI IPC handler, split out of `ipc_handler.rs` via #[path].

use super::*;
use flowmux_core::{
    AgentActivity, AgentStatus, NotificationLevel, PaneId, PlacementStrategy, SurfaceId,
    WorkspaceId,
};
use flowmux_daemon::StateStore;
use flowmux_state::State;
use rusqlite::Connection;
use std::path::Path;

/// A GuiHandler over a store holding one workspace (one pane, one
/// tab). The bridge receiver is returned so the caller keeps it
/// alive; the safety branches under test all return *before* sending
/// a GtkCommand, so no GTK loop is needed to drive them.
async fn single_pane_handler() -> (
    GuiHandler,
    async_channel::Receiver<GtkCommand>,
    PaneId,
    SurfaceId,
) {
    let store = StateStore::new_lazy(State::default());
    store
        .create_workspace(
            Some("t".into()),
            std::path::PathBuf::from("/tmp/flowmux-ipc-test"),
        )
        .await;
    let ws = store
        .snapshot()
        .await
        .workspaces
        .into_iter()
        .next()
        .unwrap();
    let tree = flowmux_ipc::protocol::describe_workspaces(std::slice::from_ref(&ws));
    let pane = tree[0].panes[0].id;
    let tab = tree[0].panes[0].tabs[0].id;
    let (bridge, rx) = Bridge::new();
    let handler = GuiHandler::new(DaemonHandler::new(store), bridge);
    (handler, rx, pane, tab)
}

#[tokio::test]
async fn workspace_create_dispatches_workspace_created_and_waits_for_ack() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let root = std::path::PathBuf::from("/tmp/flowmux-ipc-create-workspace");
    let response = handler.handle(Request::WorkspaceCreate {
        name: Some("created".into()),
        root: root.clone(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("workspace create completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("workspace create should dispatch to GTK"),
    };
    let GtkCommand::WorkspaceCreated {
        id,
        name,
        root: command_root,
        ack,
    } = command
    else {
        panic!("expected WorkspaceCreated command");
    };
    assert_eq!(name, "created");
    assert_eq!(command_root, root);
    ack.send(()).unwrap();

    assert!(matches!(
        response.await,
        Response::WorkspaceCreated { id: response_id } if response_id == id
    ));
}

#[tokio::test]
async fn ssh_connect_creates_workspace_and_sends_ssh_command() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::SshConnect {
        target: "alice@example.com:2222".into(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("ssh completed before workspace ack: {response:?}"),
        command = rx.recv() => command.expect("ssh should create workspace"),
    };
    let GtkCommand::WorkspaceCreated { id, name, ack, .. } = command else {
        panic!("expected WorkspaceCreated command");
    };
    assert_eq!(name, "ssh alice@example.com");
    ack.send(()).unwrap();

    let command = tokio::select! {
        response = &mut response => panic!("ssh completed before send-keys ack: {response:?}"),
        command = rx.recv() => command.expect("ssh should send command to terminal"),
    };
    let GtkCommand::PaneSendKeys { keys, ack, .. } = command else {
        panic!("expected PaneSendKeys command");
    };
    assert_eq!(keys, "ssh -p 2222 alice@example.com\r");
    ack.send(Ok(())).unwrap();

    assert!(matches!(response.await, Response::WorkspaceCreated { id: got } if got == id));
}

#[tokio::test]
async fn read_only_requests_delegate_without_gtk_dispatch() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;

    assert!(matches!(
        handler.handle(Request::Ping).await,
        Response::Pong
    ));
    assert!(matches!(
        handler.handle(Request::WorkspaceList).await,
        Response::WorkspaceList { ids } if ids.len() == 1
    ));
    assert!(matches!(
        handler.handle(Request::WorkspaceCurrent).await,
        Response::WorkspaceCurrent { id: Some(_) }
    ));
    assert!(matches!(
        handler.handle(Request::WorkspaceTree).await,
        Response::Tree { workspaces } if workspaces.len() == 1
    ));

    assert!(
        rx.try_recv().is_err(),
        "read-only requests should not dispatch GTK commands"
    );
}

#[tokio::test]
async fn workspace_focus_dispatches_activate_workspace_for_known_workspace() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let workspace = handler
        .inner
        .store()
        .snapshot()
        .await
        .workspaces
        .first()
        .unwrap()
        .id;

    assert!(matches!(
        handler.handle(Request::WorkspaceFocus { workspace }).await,
        Response::Ok
    ));
    let command = rx.recv().await.expect("workspace focus should dispatch");
    assert!(matches!(
        command,
        GtkCommand::ActivateWorkspace { id } if id == workspace
    ));
}

#[tokio::test]
async fn pane_focus_dispatches_focus_pane_and_waits_for_ack() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::PaneFocus { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("pane focus completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("pane focus should dispatch to GTK"),
    };
    let GtkCommand::FocusPane {
        pane: command_pane,
        ack,
    } = command
    else {
        panic!("expected FocusPane command");
    };
    assert_eq!(command_pane, pane);
    ack.send(Ok(())).unwrap();

    assert!(matches!(response.await, Response::Ok));
}

#[tokio::test]
async fn pane_resize_dispatches_resize_pane_and_waits_for_ack() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::PaneResize { pane, ratio: 0.6 });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("pane resize completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("pane resize should dispatch to GTK"),
    };

    let GtkCommand::ResizePane {
        pane: command_pane,
        ratio,
        ack,
    } = command
    else {
        panic!("expected ResizePane command");
    };
    assert_eq!(command_pane, pane);
    assert!((ratio - 0.6).abs() < f32::EPSILON);
    ack.send(Ok(())).unwrap();

    assert!(matches!(response.await, Response::Ok));
}

#[tokio::test]
async fn pane_send_keys_dispatches_terminal_command_and_waits_for_ack() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::PaneSendKeys {
        pane,
        keys: "abc".into(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("send-keys completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("send-keys should dispatch to GTK"),
    };
    let GtkCommand::PaneSendKeys {
        pane: command_pane,
        keys,
        ack,
    } = command
    else {
        panic!("expected PaneSendKeys command");
    };
    assert_eq!(command_pane, pane);
    assert_eq!(keys, "abc");
    ack.send(Ok(())).unwrap();

    assert!(matches!(response.await, Response::Ok));
}

#[tokio::test]
async fn pane_read_screen_dispatches_terminal_command_and_waits_for_ack() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::PaneReadScreen { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("read-screen completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("read-screen should dispatch to GTK"),
    };
    let GtkCommand::PaneReadScreen {
        pane: command_pane,
        ack,
    } = command
    else {
        panic!("expected PaneReadScreen command");
    };
    assert_eq!(command_pane, pane);
    ack.send(Ok(Some("screen".into()))).unwrap();

    assert!(matches!(
        response.await,
        Response::ScreenContents { text } if text == "screen"
    ));
}

#[tokio::test]
async fn notify_dispatches_add_notification_before_desktop_delivery() {
    let (handler, rx, pane, surface) = single_pane_handler().await;
    let expected_workspace = handler.inner.store().workspace_for_pane(pane).await;
    let response = handler.handle(Request::Notify {
        pane: Some(pane),
        surface: Some(surface),
        title: "Build".into(),
        body: "ready".into(),
        level: NotificationLevel::Info,
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("notify completed before GUI add-notification ack: {response:?}"),
        command = rx.recv() => command.expect("notify should dispatch AddNotification"),
    };
    let GtkCommand::AddNotification {
        pane: command_pane,
        surface: command_surface,
        workspace,
        title,
        body,
        level,
        ack,
    } = command
    else {
        panic!("expected AddNotification command");
    };
    assert_eq!(command_pane, Some(pane));
    assert_eq!(command_surface, Some(surface));
    assert_eq!(workspace, expected_workspace);
    assert_eq!(title, "Build");
    assert_eq!(body, "ready");
    assert_eq!(level, NotificationLevel::Info);

    ack.send(None).unwrap();
    assert!(matches!(response.await, Response::Ok));
    assert!(
        rx.try_recv().is_err(),
        "suppressed in-app notification must not request desktop follow-up"
    );
}

#[tokio::test]
async fn import_cookies_reports_unknown_source_without_dispatch() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;

    assert!(matches!(
        handler
            .handle(Request::ImportCookies {
                source: "unknown-browser".into(),
                domain: Some("example.com".into()),
            })
            .await,
        Response::Error(RpcError::InvalidArgument(_))
    ));
    assert!(
        rx.try_recv().is_err(),
        "invalid cookie source must not dispatch InjectCookies"
    );
}

fn firefox_cookie_fixture(path: &Path) {
    let conn = Connection::open(path).expect("open firefox cookie fixture");
    conn.execute_batch(
        "CREATE TABLE moz_cookies (
                host TEXT,
                name TEXT,
                value TEXT,
                path TEXT,
                expiry INTEGER,
                isSecure INTEGER,
                isHttpOnly INTEGER,
                sameSite INTEGER
            );",
    )
    .expect("create moz_cookies");
    conn.execute(
        "INSERT INTO moz_cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            ".example.com",
            "sid",
            "abc",
            "/",
            1_700_000_000_i64,
            1_i64,
            0_i64,
            1_i64,
        ),
    )
    .expect("insert matching cookie");
    conn.execute(
        "INSERT INTO moz_cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            ".other.test",
            "other",
            "zzz",
            "/",
            1_700_000_000_i64,
            0_i64,
            1_i64,
            2_i64,
        ),
    )
    .expect("insert filtered cookie");
}

fn home_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

struct HomeEnvRestore(Option<std::ffi::OsString>);

impl Drop for HomeEnvRestore {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn import_cookies_dispatches_inject_for_firefox_fixture() {
    let _guard = home_env_lock();
    let _restore = HomeEnvRestore(std::env::var_os("HOME"));
    let home = tempfile::tempdir().expect("temp home");
    let profile = home.path().join(".mozilla/firefox/profile.default");
    std::fs::create_dir_all(&profile).expect("profile dir");
    firefox_cookie_fixture(&profile.join("cookies.sqlite"));
    unsafe {
        std::env::set_var("HOME", home.path());
    }

    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::ImportCookies {
        source: "firefox".into(),
        domain: Some("example.com".into()),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("import-cookies returned before bridge dispatch: {response:?}"),
        command = rx.recv() => command.expect("import-cookies should dispatch InjectCookies"),
    };
    let GtkCommand::InjectCookies { cookies, ack } = command else {
        panic!("expected InjectCookies command");
    };
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].host, ".example.com");
    assert_eq!(cookies[0].name, "sid");
    assert_eq!(cookies[0].value, "abc");

    ack.send(Ok(cookies.len())).unwrap();
    assert!(matches!(
        response.await,
        Response::CookiesImported { count: 1 }
    ));
}

#[tokio::test]
async fn agent_activity_update_refreshes_store_and_sidebar() {
    let (handler, rx, pane, surface) = single_pane_handler().await;
    let expected_workspace = handler
        .inner
        .store()
        .workspace_for_pane(pane)
        .await
        .unwrap();

    let response = handler.handle(Request::AgentActivityUpdate {
        pane: Some(pane),
        surface: Some(surface),
        agent: "codex".into(),
        status: None,
        activity: Some(AgentActivity::Running),
        pid: Some(1234),
        source: None,
        seq: Some(10),
        message: None,
        custom_status: None,
        session_id: None,
    });
    tokio::pin!(response);

    let visibility = tokio::select! {
        response = &mut response => panic!("activity update completed before visibility ack: {response:?}"),
        command = rx.recv() => command.expect("activity update should query GTK visibility"),
    };
    let GtkCommand::QueryAgentSurfaceVisible {
        surface: queried_surface,
        ack,
    } = visibility
    else {
        panic!("expected QueryAgentSurfaceVisible");
    };
    assert_eq!(queried_surface, surface);
    ack.send(false).unwrap();
    assert!(matches!(response.await, Response::Ok));

    let command = rx
        .recv()
        .await
        .expect("activity update should dispatch SetAgentStatus");
    assert!(matches!(
        command,
        GtkCommand::SetAgentStatus {
            workspace,
            status: Some(AgentStatus::Working),
        } if workspace == expected_workspace
    ));
    assert_eq!(
        handler.inner.store().live_agent_presences().await,
        vec![(expected_workspace, surface, 1234)]
    );

    assert!(matches!(
        handler
            .handle(Request::AgentActivityUpdate {
                pane: Some(pane),
                surface: Some(surface),
                agent: "codex".into(),
                status: None,
                activity: None,
                pid: Some(1234),
                source: None,
                seq: Some(11),
                message: None,
                custom_status: None,
                session_id: None,
            })
            .await,
        Response::Ok
    ));

    let command = rx
        .recv()
        .await
        .expect("activity clear should dispatch SetAgentStatus");
    assert!(matches!(
        command,
        GtkCommand::SetAgentStatus {
            workspace,
            status: None,
        } if workspace == expected_workspace
    ));
    assert!(
        handler
            .inner
            .store()
            .live_agent_presences()
            .await
            .is_empty(),
        "clearing activity must remove the runtime presence from the store"
    );
}

#[tokio::test]
async fn pane_split_dispatches_incremental_apply_command() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::PaneSplit {
        pane,
        direction: SplitDirection::Vertical,
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("pane split completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("pane split should dispatch to GTK"),
    };
    let GtkCommand::PaneSplitApplied {
        pane: command_pane,
        new_pane,
        direction,
        ack,
        ..
    } = command
    else {
        panic!("expected PaneSplitApplied command");
    };
    assert_eq!(command_pane, pane);
    assert_eq!(direction, SplitDirection::Vertical);
    ack.send(()).unwrap();

    assert!(matches!(
        response.await,
        Response::PaneSplitDone { new_pane: response_pane } if response_pane == new_pane
    ));
}

#[tokio::test]
async fn claude_teams_uses_incremental_splits_after_initial_workspace_render() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let root = std::path::PathBuf::from("/tmp/flowmux-claude-teams");
    let response = handler.handle(Request::ClaudeTeams {
        count: 3,
        args: vec!["--continue".into(), "task with spaces".into()],
        root: root.clone(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("claude-teams completed before initial render ack: {response:?}"),
        command = rx.recv() => command.expect("claude-teams should dispatch initial render"),
    };
    let GtkCommand::WorkspaceCreated {
        id: ws_id,
        name,
        root: command_root,
        ack,
    } = command
    else {
        panic!("expected WorkspaceCreated command");
    };
    assert_eq!(name, "claude-teams");
    assert_eq!(command_root, root);
    ack.send(()).unwrap();

    let mut panes = Vec::new();
    let mut expected_source = None;
    for expected_direction in [SplitDirection::Vertical, SplitDirection::Horizontal] {
        let command = tokio::select! {
            response = &mut response => panic!("claude-teams completed before split ack: {response:?}"),
            command = rx.recv() => command.expect("claude-teams should dispatch split apply"),
        };
        let GtkCommand::PaneSplitApplied {
            id,
            pane,
            new_pane,
            direction,
            ack,
        } = command
        else {
            panic!("expected PaneSplitApplied command");
        };
        assert_eq!(id, ws_id);
        assert_eq!(direction, expected_direction);
        if let Some(expected_source) = expected_source {
            assert_eq!(pane, expected_source);
        } else {
            panes.push(pane);
        }
        panes.push(new_pane);
        expected_source = Some(new_pane);
        ack.send(()).unwrap();
    }

    for expected_pane in panes {
        let command = tokio::select! {
            response = &mut response => panic!("claude-teams completed before send-keys ack: {response:?}"),
            command = rx.recv() => command.expect("claude-teams should send agent command"),
        };
        let GtkCommand::PaneSendKeys { pane, keys, ack } = command else {
            panic!("expected PaneSendKeys command");
        };
        assert_eq!(pane, expected_pane);
        assert_eq!(keys, "claude --continue 'task with spaces'\n");
        ack.send(Ok(())).unwrap();
    }

    assert!(matches!(
        response.await,
        Response::WorkspaceCreated { id } if id == ws_id
    ));
    assert!(
        rx.try_recv().is_err(),
        "claude-teams must not dispatch WorkspaceRerender after incremental splits"
    );
}

#[tokio::test]
async fn pane_split_reports_not_found_without_dispatch_for_unknown_pane() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::PaneSplit {
                pane: PaneId::new(),
                direction: SplitDirection::Vertical,
            })
            .await,
        Response::Error(RpcError::NotFound(_))
    ));
    assert!(
        rx.try_recv().is_err(),
        "invalid split must not dispatch a GTK command"
    );
}

/// close-pane on the workspace's only pane must be refused up front —
/// this is the guard that stops an agent's IPC call from tripping the
/// confirm dialog (and destroying the workspace).
#[tokio::test]
async fn close_pane_refuses_the_last_pane_without_a_dialog() {
    let (handler, _rx, pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler.handle(Request::PaneClose { pane }).await,
        Response::Error(RpcError::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn close_pane_dispatches_close_focused_and_waits_for_ack() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    handler
        .inner
        .store()
        .split_pane(pane, SplitDirection::Vertical)
        .await
        .expect("second pane should be created");
    let response = handler.handle(Request::PaneClose { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("close-pane completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("close-pane should dispatch to GTK"),
    };
    let GtkCommand::CloseFocused {
        pane: command_pane,
        ack,
    } = command
    else {
        panic!("expected CloseFocused command");
    };
    assert_eq!(command_pane, pane);
    ack.send(Ok(())).unwrap();

    assert!(matches!(response.await, Response::Ok));
}

#[tokio::test]
async fn close_pane_reports_not_found_for_unknown_pane() {
    let (handler, _rx, _pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::PaneClose {
                pane: PaneId::new()
            })
            .await,
        Response::Error(RpcError::NotFound(_))
    ));
}

/// close-tab on the last tab of the last pane would also destroy the
/// workspace — it must be refused without a dialog.
#[tokio::test]
async fn close_tab_refuses_last_tab_of_last_pane() {
    let (handler, _rx, pane, tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::SurfaceClose { pane, surface: tab })
            .await,
        Response::Error(RpcError::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn close_tab_dispatches_close_surface_and_waits_for_ack() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let (_, surface) = handler
        .inner
        .store()
        .add_terminal_surface_to_pane(pane, None)
        .await
        .expect("second tab should be created");
    let response = handler.handle(Request::SurfaceClose { pane, surface });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("close-tab completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("close-tab should dispatch to GTK"),
    };
    let GtkCommand::CloseSurface {
        pane: command_pane,
        surface: command_surface,
        ack,
    } = command
    else {
        panic!("expected CloseSurface command");
    };
    assert_eq!(command_pane, pane);
    assert_eq!(command_surface, surface);
    ack.send(Ok(())).unwrap();

    assert!(matches!(response.await, Response::Ok));
}

#[tokio::test]
async fn close_tab_reports_not_found_for_unknown_surface_in_known_pane() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::SurfaceClose {
                pane,
                surface: SurfaceId::new(),
            })
            .await,
        Response::Error(RpcError::NotFound(_))
    ));
    assert!(
        rx.try_recv().is_err(),
        "invalid close-tab must not dispatch CloseSurface"
    );
}

#[tokio::test]
async fn workspace_focus_reports_not_found_for_unknown_id() {
    let (handler, _rx, _pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::WorkspaceFocus {
                workspace: WorkspaceId::new()
            })
            .await,
        Response::Error(RpcError::NotFound(_))
    ));
}

#[tokio::test]
async fn surface_focus_reports_not_found_for_unknown_pane() {
    let (handler, _rx, _pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::SurfaceFocus {
                pane: PaneId::new(),
                surface: SurfaceId::new(),
            })
            .await,
        Response::Error(RpcError::NotFound(_))
    ));
}

#[tokio::test]
async fn surface_focus_reports_not_found_for_unknown_surface_in_known_pane() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    assert!(matches!(
        handler
            .handle(Request::SurfaceFocus {
                pane,
                surface: SurfaceId::new(),
            })
            .await,
        Response::Error(RpcError::NotFound(_))
    ));
    assert!(
        rx.try_recv().is_err(),
        "invalid focus-tab must not dispatch ActivateSurface"
    );
}

#[tokio::test]
async fn browser_open_dispatches_split_and_maps_success_outcome() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let opened = PaneId::new();
    let response = handler.handle(Request::BrowserOpen {
        url: "https://example.com".into(),
        target_pane: Some(pane),
        direction: SplitDirection::Vertical,
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser open returned before bridge dispatch: {response:?}"),
        command = rx.recv() => command.expect("browser open should dispatch to GTK"),
    };
    let GtkCommand::BrowserOpenSplit {
        target_pane,
        url,
        direction,
        ack,
    } = command
    else {
        panic!("expected BrowserOpenSplit command");
    };
    assert_eq!(target_pane, Some(pane));
    assert_eq!(url, "https://example.com");
    assert_eq!(direction, SplitDirection::Vertical);

    ack.send(Ok(crate::bridge::BrowserOpenOutcome {
        pane: opened,
        placement_strategy: PlacementStrategy::SplitRight,
    }))
    .unwrap();

    assert!(matches!(
        response.await,
        Response::BrowserPaneOpened {
            pane: got,
            placement_strategy: PlacementStrategy::SplitRight,
        } if got == opened
    ));
}

#[tokio::test]
async fn browser_open_reports_not_found_for_missing_target_pane() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let missing = PaneId::new();
    let response = handler.handle(Request::BrowserOpen {
        url: "https://example.com".into(),
        target_pane: Some(missing),
        direction: SplitDirection::Vertical,
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser open completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser open should dispatch to GTK"),
    };
    let GtkCommand::BrowserOpenSplit {
        target_pane, ack, ..
    } = command
    else {
        panic!("expected BrowserOpenSplit command");
    };
    assert_eq!(target_pane, Some(missing));
    ack.send(Err(format!("pane not found: {missing}"))).unwrap();

    assert!(matches!(
        response.await,
        Response::Error(RpcError::NotFound(_))
    ));
}

#[tokio::test]
async fn browser_open_reports_not_found_when_default_target_cannot_resolve() {
    let (handler, rx, _pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserOpen {
        url: "https://example.com".into(),
        target_pane: None,
        direction: SplitDirection::Vertical,
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser open completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser open should dispatch to GTK"),
    };
    let GtkCommand::BrowserOpenSplit {
        target_pane, ack, ..
    } = command
    else {
        panic!("expected BrowserOpenSplit command");
    };
    assert_eq!(target_pane, None);
    ack.send(Err("no target pane focused".into())).unwrap();

    assert!(matches!(
        response.await,
        Response::Error(RpcError::NotFound(_))
    ));
}

async fn assert_browser_action_dispatches(
    handler: &GuiHandler,
    rx: &async_channel::Receiver<GtkCommand>,
    pane: PaneId,
    request: Request,
    assert_op: impl FnOnce(BrowserOp),
) {
    let response = handler.handle(request);
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser action completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser action should dispatch to GTK"),
    };
    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };

    assert_eq!(command_pane, pane);
    assert_op(op);
    ack.send(Ok(BrowserActionResult::Ok)).unwrap();
    assert!(matches!(response.await, Response::BrowserOk));
}

#[tokio::test]
async fn browser_actions_dispatch_form_read_and_phase5_ops() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserFill {
            pane,
            target: "name".into(),
            value: "flowmux".into(),
        },
        |op| match op {
            BrowserOp::Fill { target, value } => {
                assert_eq!(target, "name");
                assert_eq!(value, "flowmux");
            }
            other => panic!("expected BrowserOp::Fill, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserScroll {
            pane,
            target: "root".into(),
            x: -10,
            y: 250,
        },
        |op| match op {
            BrowserOp::Scroll { target, x, y } => {
                assert_eq!(target, "root");
                assert_eq!(x, -10);
                assert_eq!(y, 250);
            }
            other => panic!("expected BrowserOp::Scroll, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserAttr {
            pane,
            target: "link".into(),
            name: "href".into(),
        },
        |op| match op {
            BrowserOp::Attr { target, name } => {
                assert_eq!(target, "link");
                assert_eq!(name, "href");
            }
            other => panic!("expected BrowserOp::Attr, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserFocus {
            pane,
            target: "field".into(),
        },
        |op| match op {
            BrowserOp::Focus { target } => assert_eq!(target, "field"),
            other => panic!("expected BrowserOp::Focus, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserUncheck {
            pane,
            target: "box".into(),
        },
        |op| match op {
            BrowserOp::Uncheck { target } => assert_eq!(target, "box"),
            other => panic!("expected BrowserOp::Uncheck, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserCount {
            pane,
            selector: ".row".into(),
        },
        |op| match op {
            BrowserOp::Count { selector } => assert_eq!(selector, ".row"),
            other => panic!("expected BrowserOp::Count, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserWait {
            pane,
            condition: flowmux_ipc::protocol::BrowserWaitCondition::Text("ready".into()),
            timeout_ms: 500,
            poll_ms: 25,
        },
        |op| match op {
            BrowserOp::Wait {
                condition,
                timeout_ms,
                poll_ms,
            } => {
                assert_eq!(
                    condition,
                    flowmux_ipc::protocol::BrowserWaitCondition::Text("ready".into())
                );
                assert_eq!(timeout_ms, 500);
                assert_eq!(poll_ms, 25);
            }
            other => panic!("expected BrowserOp::Wait, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserScreenshot {
            pane,
            path: std::path::PathBuf::from("/tmp/page.png"),
        },
        |op| match op {
            BrowserOp::Screenshot { path } => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/page.png"));
            }
            other => panic!("expected BrowserOp::Screenshot, got {other:?}"),
        },
    )
    .await;
}

#[tokio::test]
async fn browser_actions_dispatch_remaining_navigation_ref_ops() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;

    assert_browser_action_dispatches(&handler, &rx, pane, Request::BrowserBack { pane }, |op| {
        assert!(matches!(op, BrowserOp::Back));
    })
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserForward { pane },
        |op| assert!(matches!(op, BrowserOp::Forward)),
    )
    .await;

    assert_browser_action_dispatches(&handler, &rx, pane, Request::BrowserReload { pane }, |op| {
        assert!(matches!(op, BrowserOp::Reload))
    })
    .await;

    assert_browser_action_dispatches(&handler, &rx, pane, Request::BrowserUrl { pane }, |op| {
        assert!(matches!(op, BrowserOp::Url))
    })
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserClick {
            pane,
            target: "button".into(),
        },
        |op| match op {
            BrowserOp::Click { target } => assert_eq!(target, "button"),
            other => panic!("expected BrowserOp::Click, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserSelect {
            pane,
            target: "choice".into(),
            value: "A".into(),
        },
        |op| match op {
            BrowserOp::Select { target, value } => {
                assert_eq!(target, "choice");
                assert_eq!(value, "A");
            }
            other => panic!("expected BrowserOp::Select, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserType {
            pane,
            text: "hello".into(),
        },
        |op| match op {
            BrowserOp::Type { text } => assert_eq!(text, "hello"),
            other => panic!("expected BrowserOp::Type, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserPress {
            pane,
            key: "Enter".into(),
        },
        |op| match op {
            BrowserOp::Press { key } => assert_eq!(key, "Enter"),
            other => panic!("expected BrowserOp::Press, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserText {
            pane,
            target: "label".into(),
        },
        |op| match op {
            BrowserOp::Text { target } => assert_eq!(target, "label"),
            other => panic!("expected BrowserOp::Text, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserValue {
            pane,
            target: "input".into(),
        },
        |op| match op {
            BrowserOp::Value { target } => assert_eq!(target, "input"),
            other => panic!("expected BrowserOp::Value, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserDblClick {
            pane,
            target: "row".into(),
        },
        |op| match op {
            BrowserOp::DblClick { target } => assert_eq!(target, "row"),
            other => panic!("expected BrowserOp::DblClick, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserHover {
            pane,
            target: "menu".into(),
        },
        |op| match op {
            BrowserOp::Hover { target } => assert_eq!(target, "menu"),
            other => panic!("expected BrowserOp::Hover, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserBlur {
            pane,
            target: "field".into(),
        },
        |op| match op {
            BrowserOp::Blur { target } => assert_eq!(target, "field"),
            other => panic!("expected BrowserOp::Blur, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserCheck {
            pane,
            target: "box".into(),
        },
        |op| match op {
            BrowserOp::Check { target } => assert_eq!(target, "box"),
            other => panic!("expected BrowserOp::Check, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserIsEnabled {
            pane,
            target: "submit".into(),
        },
        |op| match op {
            BrowserOp::IsEnabled { target } => assert_eq!(target, "submit"),
            other => panic!("expected BrowserOp::IsEnabled, got {other:?}"),
        },
    )
    .await;

    assert_browser_action_dispatches(
        &handler,
        &rx,
        pane,
        Request::BrowserIsChecked {
            pane,
            target: "box".into(),
        },
        |op| match op {
            BrowserOp::IsChecked { target } => assert_eq!(target, "box"),
            other => panic!("expected BrowserOp::IsChecked, got {other:?}"),
        },
    )
    .await;
}

#[tokio::test]
async fn browser_navigate_dispatches_action_and_maps_ok_result() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserNavigate {
        pane,
        url: "https://example.com".into(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser navigate completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser navigate should dispatch to GTK"),
    };
    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };
    assert_eq!(command_pane, pane);
    match op {
        BrowserOp::Navigate { url } => assert_eq!(url, "https://example.com"),
        _ => panic!("expected BrowserOp::Navigate"),
    }
    ack.send(Ok(BrowserActionResult::Ok)).unwrap();

    assert!(matches!(response.await, Response::BrowserOk));
}

#[tokio::test]
async fn browser_title_dispatches_action_and_maps_string_result() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserTitle { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser title completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser title should dispatch to GTK"),
    };
    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };
    assert_eq!(command_pane, pane);
    assert!(matches!(op, BrowserOp::Title));
    ack.send(Ok(BrowserActionResult::String("Example".into())))
        .unwrap();

    assert!(matches!(
        response.await,
        Response::BrowserResult { value } if value == "Example"
    ));
}

#[tokio::test]
async fn browser_is_visible_dispatches_action_and_maps_bool_result() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserIsVisible {
        pane,
        target: "e3".into(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser is-visible completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser is-visible should dispatch to GTK"),
    };
    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };
    assert_eq!(command_pane, pane);
    match op {
        BrowserOp::IsVisible { target } => assert_eq!(target, "e3"),
        _ => panic!("expected BrowserOp::IsVisible"),
    }
    ack.send(Ok(BrowserActionResult::Bool(true))).unwrap();

    assert!(matches!(
        response.await,
        Response::BrowserBoolResult { value: true }
    ));
}

#[tokio::test]
async fn browser_action_reports_not_found_for_missing_browser_pane() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserUrl { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser action completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser action should dispatch to GTK"),
    };
    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };
    assert_eq!(command_pane, pane);
    assert!(matches!(op, BrowserOp::Url));
    ack.send(Err(format!("browser pane not found: {pane}")))
        .unwrap();

    assert!(matches!(
        response.await,
        Response::Error(RpcError::NotFound(_))
    ));
}

#[tokio::test]
async fn browser_snapshot_dispatches_snapshot_and_maps_json_result() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserSnapshot { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser snapshot completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser snapshot should dispatch to GTK"),
    };

    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };

    assert_eq!(command_pane, pane);
    assert!(matches!(op, BrowserOp::Snapshot));
    ack.send(Ok(BrowserActionResult::String(
        r#"{"tree":[],"refs":{}}"#.into(),
    )))
    .unwrap();

    assert!(matches!(
        response.await,
        Response::BrowserResult { value } if value == r#"{"tree":[],"refs":{}}"#
    ));
}

#[tokio::test]
async fn browser_snapshot_reports_not_found_for_missing_browser_pane() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserSnapshot { pane });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser snapshot completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser snapshot should dispatch to GTK"),
    };
    let GtkCommand::BrowserAction {
        pane: command_pane,
        op,
        ack,
    } = command
    else {
        panic!("expected BrowserAction command");
    };
    assert_eq!(command_pane, pane);
    assert!(matches!(op, BrowserOp::Snapshot));
    ack.send(Err(format!("browser pane not found: {pane}")))
        .unwrap();

    assert!(matches!(
        response.await,
        Response::Error(RpcError::NotFound(_))
    ));
}

#[tokio::test]
async fn browser_eval_dispatches_eval_and_maps_string_result() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserEval {
        pane,
        source: "document.title".into(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser eval completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser eval should dispatch to GTK"),
    };
    let GtkCommand::BrowserEval {
        pane: command_pane,
        source,
        ack,
    } = command
    else {
        panic!("expected BrowserEval command");
    };

    assert_eq!(command_pane, pane);
    assert_eq!(source, "document.title");
    ack.send(Ok("Example".into())).unwrap();

    assert!(matches!(
        response.await,
        Response::BrowserResult { value } if value == "Example"
    ));
}

#[tokio::test]
async fn browser_eval_reports_not_found_for_missing_browser_pane() {
    let (handler, rx, pane, _tab) = single_pane_handler().await;
    let response = handler.handle(Request::BrowserEval {
        pane,
        source: "document.title".into(),
    });
    tokio::pin!(response);

    let command = tokio::select! {
        response = &mut response => panic!("browser eval completed before bridge ack: {response:?}"),
        command = rx.recv() => command.expect("browser eval should dispatch to GTK"),
    };
    let GtkCommand::BrowserEval {
        pane: command_pane,
        source,
        ack,
    } = command
    else {
        panic!("expected BrowserEval command");
    };
    assert_eq!(command_pane, pane);
    assert_eq!(source, "document.title");
    ack.send(Err(format!("browser pane not found: {pane}")))
        .unwrap();

    assert!(matches!(
        response.await,
        Response::Error(RpcError::NotFound(_))
    ));
}
