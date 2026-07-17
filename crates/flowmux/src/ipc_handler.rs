// SPDX-License-Identifier: GPL-3.0-or-later
//! GUI-aware IPC handler.
//!
//! Wraps `flowmux_daemon::DaemonHandler` and intercepts the verbs that
//! need to mutate the GTK widget tree (workspace creation, pane split,
//! send-keys, browser open). Those verbs are forwarded across the
//! [`Bridge`] to the GTK main loop and the response is awaited via a
//! `oneshot` channel.

use crate::bridge::{Bridge, BrowserActionResult, BrowserOp, GtkCommand};
use flowmux_core::{AgentStatusReport, SplitDirection};
use flowmux_daemon::DaemonHandler;
use flowmux_ipc::protocol::{Request, Response, RpcError};
use flowmux_ipc::server::Handler;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::oneshot;
use tracing::warn;

/// Resolve the agent-session store rooted at
/// `$XDG_DATA_HOME/flowmux/agent-sessions/`. Returns `None` when the
/// XDG data dir is unavailable (very rare on Linux, but possible in
/// minimal containers without HOME / XDG_DATA_HOME).
fn agent_session_store() -> Option<flowmux_state::AgentSessionStore> {
    flowmux_state::default_agent_session_store()
}

fn browser_error_response(error: String) -> Response {
    if error.starts_with("browser pane not found:")
        || error.starts_with("pane not found:")
        || error == "no target pane focused"
    {
        Response::Error(RpcError::NotFound(error))
    } else {
        Response::Error(RpcError::Internal(error))
    }
}

async fn browser_action(bridge: &Bridge, pane: flowmux_core::PaneId, op: BrowserOp) -> Response {
    tracing::debug!(
        %pane,
        verb = op.capability_name(),
        query = op.is_query(),
        "dispatching browser action"
    );
    let (tx, rx) = oneshot::channel();
    let _ = bridge
        .tx
        .send(GtkCommand::BrowserAction { pane, op, ack: tx })
        .await;
    match rx.await {
        Ok(Ok(BrowserActionResult::Ok)) => Response::BrowserOk,
        Ok(Ok(BrowserActionResult::Bool(value))) => Response::BrowserBoolResult { value },
        Ok(Ok(BrowserActionResult::String(value))) => Response::BrowserResult { value },
        Ok(Err(e)) => browser_error_response(e),
        Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
    }
}

/// Browser requests that map directly to the shared GTK browser action path.
/// Open, cookie import, and raw eval keep their dedicated response contracts.
struct BrowserActionRequest {
    pane: flowmux_core::PaneId,
    op: BrowserOp,
}

impl TryFrom<Request> for BrowserActionRequest {
    type Error = Request;

    fn try_from(request: Request) -> Result<Self, Self::Error> {
        let (pane, op) = match request {
            Request::BrowserNavigate { pane, url } => (pane, BrowserOp::Navigate { url }),
            Request::BrowserBack { pane } => (pane, BrowserOp::Back),
            Request::BrowserForward { pane } => (pane, BrowserOp::Forward),
            Request::BrowserReload { pane } => (pane, BrowserOp::Reload),
            Request::BrowserUrl { pane } => (pane, BrowserOp::Url),
            Request::BrowserTitle { pane } => (pane, BrowserOp::Title),
            Request::BrowserClick { pane, target } => (pane, BrowserOp::Click { target }),
            Request::BrowserFill {
                pane,
                target,
                value,
            } => (pane, BrowserOp::Fill { target, value }),
            Request::BrowserSelect {
                pane,
                target,
                value,
            } => (pane, BrowserOp::Select { target, value }),
            Request::BrowserScroll { pane, target, x, y } => {
                (pane, BrowserOp::Scroll { target, x, y })
            }
            Request::BrowserType { pane, text } => (pane, BrowserOp::Type { text }),
            Request::BrowserPress { pane, key } => (pane, BrowserOp::Press { key }),
            Request::BrowserText { pane, target } => (pane, BrowserOp::Text { target }),
            Request::BrowserValue { pane, target } => (pane, BrowserOp::Value { target }),
            Request::BrowserAttr { pane, target, name } => (pane, BrowserOp::Attr { target, name }),
            Request::BrowserWait {
                pane,
                condition,
                timeout_ms,
                poll_ms,
            } => (
                pane,
                BrowserOp::Wait {
                    condition,
                    timeout_ms,
                    poll_ms,
                },
            ),
            Request::BrowserScreenshot { pane, path } => (pane, BrowserOp::Screenshot { path }),
            Request::BrowserDblClick { pane, target } => (pane, BrowserOp::DblClick { target }),
            Request::BrowserHover { pane, target } => (pane, BrowserOp::Hover { target }),
            Request::BrowserFocus { pane, target } => (pane, BrowserOp::Focus { target }),
            Request::BrowserBlur { pane, target } => (pane, BrowserOp::Blur { target }),
            Request::BrowserCheck { pane, target } => (pane, BrowserOp::Check { target }),
            Request::BrowserUncheck { pane, target } => (pane, BrowserOp::Uncheck { target }),
            Request::BrowserIsVisible { pane, target } => (pane, BrowserOp::IsVisible { target }),
            Request::BrowserIsEnabled { pane, target } => (pane, BrowserOp::IsEnabled { target }),
            Request::BrowserIsChecked { pane, target } => (pane, BrowserOp::IsChecked { target }),
            Request::BrowserCount { pane, selector } => (pane, BrowserOp::Count { selector }),
            Request::BrowserSnapshot { pane } => (pane, BrowserOp::Snapshot),
            other => return Err(other),
        };
        Ok(Self { pane, op })
    }
}

pub struct GuiHandler {
    inner: DaemonHandler,
    bridge: Bridge,
}

impl GuiHandler {
    pub fn new(inner: DaemonHandler, bridge: Bridge) -> Self {
        Self { inner, bridge }
    }
}

impl Handler for GuiHandler {
    fn handle<'a>(&'a self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
        Box::pin(async move {
            match req {
                Request::WorkspaceCreate { .. }
                | Request::WorkspaceFocus { .. }
                | Request::SurfaceCreate { .. } => self.handle_workspace_verb(req).await,
                Request::PaneSplit { .. }
                | Request::PaneSendKeys { .. }
                | Request::PaneReadScreen { .. }
                | Request::SurfaceFocus { .. }
                | Request::SurfaceClose { .. }
                | Request::PaneFocus { .. }
                | Request::PaneResize { .. }
                | Request::PaneClose { .. } => self.handle_pane_verb(req).await,
                Request::BrowserOpen { .. }
                | Request::BrowserNavigate { .. }
                | Request::BrowserBack { .. }
                | Request::BrowserForward { .. }
                | Request::BrowserReload { .. }
                | Request::BrowserUrl { .. }
                | Request::BrowserTitle { .. }
                | Request::BrowserClick { .. }
                | Request::BrowserFill { .. }
                | Request::BrowserSelect { .. }
                | Request::BrowserScroll { .. }
                | Request::BrowserType { .. }
                | Request::BrowserPress { .. }
                | Request::BrowserText { .. }
                | Request::BrowserValue { .. }
                | Request::BrowserAttr { .. }
                | Request::BrowserWait { .. }
                | Request::BrowserScreenshot { .. }
                | Request::BrowserDblClick { .. }
                | Request::BrowserHover { .. }
                | Request::BrowserFocus { .. }
                | Request::BrowserBlur { .. }
                | Request::BrowserCheck { .. }
                | Request::BrowserUncheck { .. }
                | Request::BrowserIsVisible { .. }
                | Request::BrowserIsEnabled { .. }
                | Request::BrowserIsChecked { .. }
                | Request::BrowserCount { .. }
                | Request::BrowserSnapshot { .. }
                | Request::ImportCookies { .. }
                | Request::BrowserEval { .. } => self.handle_browser_verb(req).await,
                Request::ClaudeTeams { .. }
                | Request::TmuxCompat { .. }
                | Request::AgentSessionUpdate { .. }
                | Request::AgentSessionGet { .. }
                | Request::AgentSessionForget { .. }
                | Request::AgentActivityUpdate { .. } => self.handle_agent_verb(req).await,
                Request::Notify { .. }
                | Request::NotificationsList { .. }
                | Request::NotificationOpen { .. }
                | Request::NotificationJumpToUnread
                | Request::NotificationMarkRead { .. }
                | Request::NotificationsClear => self.handle_notification_verb(req).await,
                _ => self.inner.handle(req).await,
            }
        })
    }
}

// Cargo `Request::Clone` is used above; ensure the type implements Clone.
// This trait bound is satisfied because the protocol enum derives Clone.
const _: fn() = || {
    fn assert_clone<T: Clone>() {}
    assert_clone::<Request>();
};

/// Quote a single argv element for safe `sh` re-parsing.
fn shell_escape(arg: String) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'='))
    {
        return arg;
    }
    let escaped = arg.replace('\'', "'\\''");
    format!("'{escaped}'")
}

impl GuiHandler {
    /// Dispatch for the workspace verb group (split out of the `handle` match).
    async fn handle_workspace_verb(&self, req: Request) -> Response {
        match req {
            Request::WorkspaceCreate { ref name, ref root } => {
                // Persist via the headless handler first so state.json is consistent...
                let resp = self.inner.handle(req.clone()).await;
                let id = match &resp {
                    Response::WorkspaceCreated { id } => *id,
                    _ => return resp,
                };
                // ...then ask the GTK side to materialize widgets.
                let (tx, rx) = oneshot::channel();
                if let Err(e) = self
                    .bridge
                    .tx
                    .send(GtkCommand::WorkspaceCreated {
                        id,
                        name: name.clone().unwrap_or_else(|| {
                            root.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("workspace")
                                .into()
                        }),
                        root: root.clone(),
                        ack: tx,
                    })
                    .await
                {
                    warn!(error = %e, "bridge closed");
                }
                let _ = rx.await;
                Response::WorkspaceCreated { id }
            }
            Request::WorkspaceFocus { workspace } => {
                // Validate against live state so a bad id returns a
                // clean NotFound instead of a silent no-op. The focus
                // itself reuses ActivateWorkspace — the exact operation
                // a sidebar row click performs: no dialog, reversible,
                // creates/destroys nothing.
                let exists = self
                    .inner
                    .store()
                    .snapshot()
                    .await
                    .workspaces
                    .iter()
                    .any(|w| w.id == workspace);
                if exists {
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::ActivateWorkspace { id: workspace })
                        .await;
                    Response::Ok
                } else {
                    Response::Error(RpcError::NotFound(workspace.to_string()))
                }
            }
            Request::SurfaceCreate { workspace, cwd } => {
                if self.inner.store().get_workspace(workspace).await.is_none() {
                    return Response::Error(RpcError::NotFound(workspace.to_string()));
                }
                let (tx, rx) = oneshot::channel();
                if let Err(error) = self
                    .bridge
                    .tx
                    .send(GtkCommand::CreateSurface {
                        workspace,
                        cwd,
                        ack: tx,
                    })
                    .await
                {
                    return Response::Error(RpcError::Internal(error.to_string()));
                }
                match rx.await {
                    Ok(Ok((pane, id))) => Response::SurfaceCreated { id, pane },
                    Ok(Err(error)) => Response::Error(RpcError::Internal(error)),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            other => unreachable!("workspace router got a non-workspace verb: {other:?}"),
        }
    }

    /// Dispatch for the pane verb group (split out of the `handle` match).
    async fn handle_pane_verb(&self, req: Request) -> Response {
        match req {
            Request::PaneSplit { pane, direction } => {
                match self.inner.store().split_pane(pane, direction).await {
                    None => Response::Error(RpcError::NotFound(pane.to_string())),
                    Some((ws_id, new_pane)) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = self
                            .bridge
                            .tx
                            .send(GtkCommand::PaneSplitApplied {
                                id: ws_id,
                                pane,
                                new_pane,
                                direction,
                                ack: tx,
                            })
                            .await;
                        let _ = rx.await;
                        Response::PaneSplitDone { new_pane }
                    }
                }
            }
            Request::PaneSendKeys { pane, keys } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::PaneSendKeys {
                        pane,
                        keys,
                        ack: tx,
                    })
                    .await;
                match rx.await {
                    Ok(Ok(())) => Response::Ok,
                    Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::PaneReadScreen { pane } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::PaneReadScreen { pane, ack: tx })
                    .await;
                match rx.await {
                    Ok(Ok(Some(text))) => Response::ScreenContents { text },
                    // `None` = the pane has no readable terminal surface
                    // (e.g. a browser tab). Terminal panes expose screen text through VTE,
                    // so this is not feature-gated.
                    Ok(Ok(None)) => Response::Error(RpcError::Unimplemented(
                        "read-screen: this pane has no readable terminal surface".into(),
                    )),
                    Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::SurfaceFocus { pane, surface } => {
                // Validate the pane against live state (reusing the
                // tree flattener), then fire the same ActivateSurface
                // the tab bar uses. Non-destructive, no dialog.
                let workspaces = self.inner.store().ordered_workspaces().await;
                let tree = flowmux_ipc::protocol::describe_workspaces(&workspaces);
                let pane_found = tree.iter().flat_map(|w| &w.panes).any(|p| p.id == pane);
                let surface_found = tree
                    .iter()
                    .flat_map(|w| &w.panes)
                    .find(|p| p.id == pane)
                    .is_some_and(|p| p.tabs.iter().any(|tab| tab.id == surface));
                if surface_found {
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::ActivateSurface { pane, surface })
                        .await;
                    Response::Ok
                } else if pane_found {
                    Response::Error(RpcError::NotFound(format!(
                        "surface not found in pane {pane}: {surface}"
                    )))
                } else {
                    Response::Error(RpcError::NotFound(format!("pane not found: {pane}")))
                }
            }
            Request::SurfaceClose { pane, surface } => {
                // Refuse the last-tab-of-last-pane case up front so the
                // agent never trips CloseSurface's confirm dialog.
                let workspaces = self.inner.store().ordered_workspaces().await;
                let tree = flowmux_ipc::protocol::describe_workspaces(&workspaces);
                let pane_found = tree.iter().flat_map(|w| &w.panes).any(|p| p.id == pane);
                let surface_found = tree
                    .iter()
                    .flat_map(|w| &w.panes)
                    .find(|p| p.id == pane)
                    .is_some_and(|p| p.tabs.iter().any(|tab| tab.id == surface));
                let tabs = self.inner.store().tab_count_in_pane(pane).await;
                let panes = self.inner.store().workspace_pane_count_for(pane).await;
                if !pane_found {
                    Response::Error(RpcError::NotFound(format!("pane not found: {pane}")))
                } else if !surface_found {
                    Response::Error(RpcError::NotFound(format!(
                        "surface not found in pane {pane}: {surface}"
                    )))
                } else if tabs == Some(1) && matches!(panes, Some((_, 1))) {
                    Response::Error(RpcError::InvalidArgument(
                        "refusing to close the last tab of the workspace's last pane".into(),
                    ))
                } else {
                    let (tx, rx) = oneshot::channel();
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::CloseSurface {
                            pane,
                            surface,
                            ack: tx,
                        })
                        .await;
                    match rx.await {
                        Ok(Ok(())) => Response::Ok,
                        Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                        Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                    }
                }
            }
            Request::PaneFocus { pane } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::FocusPane { pane, ack: tx })
                    .await;
                match rx.await {
                    Ok(Ok(())) => Response::Ok,
                    Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::PaneResize { pane, ratio } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::ResizePane {
                        pane,
                        ratio,
                        ack: tx,
                    })
                    .await;
                match rx.await {
                    Ok(Ok(())) => Response::Ok,
                    Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::PaneClose { pane } => {
                // Peek the pane count up front. Closing the workspace's
                // last pane is what triggers CloseFocused's confirm
                // dialog; refuse it here (with a clear error) so an
                // agent's IPC call never blocks on user input.
                match self.inner.store().workspace_pane_count_for(pane).await {
                    None => Response::Error(RpcError::NotFound(format!("pane not found: {pane}"))),
                    Some((_, 1)) => Response::Error(RpcError::InvalidArgument(
                        "refusing to close the workspace's last pane; close the workspace \
                             instead"
                            .into(),
                    )),
                    Some((_, _)) => {
                        // >1 pane: CloseFocused takes the no-dialog path.
                        let (tx, rx) = oneshot::channel();
                        let _ = self
                            .bridge
                            .tx
                            .send(GtkCommand::CloseFocused { pane, ack: tx })
                            .await;
                        match rx.await {
                            Ok(Ok(())) => Response::Ok,
                            Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                            Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                        }
                    }
                }
            }
            other => unreachable!("pane router got a non-pane verb: {other:?}"),
        }
    }

    /// Dispatch for the browser verb group (split out of the `handle` match).
    async fn handle_browser_verb(&self, req: Request) -> Response {
        let req = match BrowserActionRequest::try_from(req) {
            Ok(action) => return browser_action(&self.bridge, action.pane, action.op).await,
            Err(req) => req,
        };
        match req {
            Request::BrowserOpen {
                url,
                target_pane,
                direction,
            } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::BrowserOpenSplit {
                        target_pane,
                        url,
                        direction,
                        ack: tx,
                    })
                    .await;
                match rx.await {
                    Ok(Ok(outcome)) => Response::BrowserPaneOpened {
                        pane: outcome.pane,
                        placement_strategy: outcome.placement_strategy,
                    },
                    Ok(Err(e)) => browser_error_response(e),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::ImportCookies { source, domain } => {
                // Read cookies inside an inner scope so the !Send
                // `Box<dyn Source>` is dropped before any `.await`.
                let cookies = {
                    let sources = flowmux_cookies::discover_sources();
                    let s = match sources.into_iter().find(|s| s.id().slug() == source) {
                        Some(s) => s,
                        None => {
                            return Response::Error(RpcError::InvalidArgument(format!(
                                "unknown browser source: {source}"
                            )))
                        }
                    };
                    match s.list_cookies(domain.as_deref()) {
                        Ok(c) => c,
                        Err(e @ flowmux_cookies::source::Error::EncryptedValuesUnsupported) => {
                            return Response::Error(RpcError::Unimplemented(e.to_string()))
                        }
                        Err(e) => return Response::Error(RpcError::Internal(e.to_string())),
                    }
                };
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::InjectCookies { cookies, ack: tx })
                    .await;
                match rx.await {
                    Ok(Ok(count)) => Response::CookiesImported { count },
                    Ok(Err(e)) => Response::Error(RpcError::Internal(e)),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::BrowserEval { pane, source } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::BrowserEval {
                        pane,
                        source,
                        ack: tx,
                    })
                    .await;
                match rx.await {
                    Ok(Ok(value)) => Response::BrowserResult { value },
                    Ok(Err(e)) => browser_error_response(e),
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }

            // ---- Phase 7: agent session resume mapping --------
            other => unreachable!("browser router got a non-browser verb: {other:?}"),
        }
    }

    /// Dispatch for the agent verb group (split out of the `handle` match).
    async fn handle_agent_verb(&self, req: Request) -> Response {
        match req {
            // One tmux CLI invocation forwarded by the `tmux` shim —
            // Claude Code agent teams driving flowmux panes natively.
            // State mutations run in the shared orchestrator; widget
            // work goes through the bridge-backed UI below.
            Request::TmuxCompat { args, cwd } => {
                let ui = GuiTmuxUi {
                    bridge: &self.bridge,
                };
                let out =
                    flowmux_daemon::tmux_compat::execute(self.inner.store(), &ui, &args, &cwd)
                        .await;
                Response::TmuxCompatResult(out)
            }
            Request::ClaudeTeams { count, args, root } => {
                let count = count.clamp(1, 8);
                let store = self.inner.store();
                let root_for_ui = root.clone();
                // Create a fresh workspace.
                let ws_id = store
                    .create_workspace(Some("claude-teams".into()), root)
                    .await;

                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::WorkspaceCreated {
                        id: ws_id,
                        name: "claude-teams".into(),
                        root: root_for_ui,
                        ack: tx,
                    })
                    .await;
                let _ = rx.await;

                // Split the root pane (count - 1) times to get `count` panes.
                let ws = match store.get_workspace(ws_id).await {
                    Some(w) => w,
                    None => {
                        return Response::Error(RpcError::Internal("workspace vanished".into()))
                    }
                };
                // First leaf id from the root pane:
                let mut leaves = vec![];
                if let Some(s) = ws.surfaces.first() {
                    s.root_pane.for_each_leaf(|id| leaves.push(id));
                }
                let mut current = match leaves.first().copied() {
                    Some(id) => id,
                    None => {
                        return Response::Error(RpcError::Internal(
                            "workspace had no leaves".into(),
                        ))
                    }
                };
                let mut all_panes = vec![current];
                for i in 1..count {
                    let dir = if i % 2 == 1 {
                        SplitDirection::Vertical
                    } else {
                        SplitDirection::Horizontal
                    };
                    if let Some((_, new_pane)) = store.split_pane(current, dir).await {
                        let source_pane = current;
                        all_panes.push(new_pane);
                        current = new_pane;

                        let (tx, rx) = oneshot::channel();
                        let _ = self
                            .bridge
                            .tx
                            .send(GtkCommand::PaneSplitApplied {
                                id: ws_id,
                                pane: source_pane,
                                new_pane,
                                direction: dir,
                                ack: tx,
                            })
                            .await;
                        let _ = rx.await;
                    }
                }
                // Feed the `claude` invocation into each pane.
                let cmd_line = std::iter::once("claude".to_string())
                    .chain(args.iter().cloned())
                    .map(shell_escape)
                    .collect::<Vec<_>>()
                    .join(" ");
                let cmd_line = format!("{cmd_line}\n");
                for pane in &all_panes {
                    let (tx, rx) = oneshot::channel();
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::PaneSendKeys {
                            pane: *pane,
                            keys: cmd_line.clone(),
                            ack: tx,
                        })
                        .await;
                    let _ = rx.await;
                }
                Response::WorkspaceCreated { id: ws_id }
            }
            Request::AgentSessionUpdate {
                agent,
                surface,
                session_id,
            } => match agent_session_store() {
                Some(store) => match store.record(&agent, surface, &session_id) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error(RpcError::Io(e.to_string())),
                },
                None => Response::Error(RpcError::Internal(
                    "XDG data dir unavailable; cannot persist agent session".into(),
                )),
            },
            Request::AgentSessionGet { agent, surface } => match agent_session_store() {
                Some(store) => Response::AgentSession {
                    session_id: store.lookup(&agent, surface),
                },
                None => Response::AgentSession { session_id: None },
            },
            Request::AgentSessionForget { agent, surface } => match agent_session_store() {
                Some(store) => match store.forget(&agent, surface) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error(RpcError::Io(e.to_string())),
                },
                None => Response::Ok,
            },

            // ---- Live agent activity (Running / NeedsInput / Idle).
            // Hooks pass FLOWMUX_SURFACE_ID, so a surface is expected;
            // without one we can't route the presence to a tab.
            Request::AgentActivityUpdate {
                surface,
                agent,
                status,
                activity,
                pid,
                source,
                seq,
                message,
                custom_status,
                session_id,
                ..
            } => match surface {
                Some(surface) => {
                    // Start/activity hooks refresh the native binding. An
                    // intentional Claude SessionEnd follows this update with
                    // AgentSessionForget; app teardown leaves it intact so a
                    // relaunch can resume the still-active session.
                    if let Some(session_id) = session_id.as_deref() {
                        if let Some(store) = agent_session_store() {
                            if let Err(error) = store.record(&agent, surface, session_id) {
                                warn!(%surface, %agent, %error, "failed to persist agent session");
                            }
                        }
                    }
                    if status.is_none() && activity.is_none() {
                        if let Some(ws_id) =
                            self.inner.store().set_agent_activity(surface, None).await
                        {
                            let rollup = self.inner.store().workspace_agent_status(ws_id).await;
                            let _ = self
                                .bridge
                                .tx
                                .send(GtkCommand::SetAgentStatus {
                                    workspace: ws_id,
                                    status: rollup,
                                })
                                .await;
                        }
                    } else {
                        let (visibility_tx, visibility_rx) = oneshot::channel();
                        let _ = self
                            .bridge
                            .tx
                            .send(GtkCommand::QueryAgentSurfaceVisible {
                                surface,
                                ack: visibility_tx,
                            })
                            .await;
                        let surface_visible = visibility_rx.await.unwrap_or(false);
                        let report = AgentStatusReport {
                            name: agent,
                            status,
                            activity,
                            pid,
                            source,
                            seq,
                            message,
                            custom_status,
                            session_id,
                        };
                        if let Some((ws_id, rollup)) = self
                            .inner
                            .store()
                            .report_agent_status_with_visibility(surface, report, surface_visible)
                            .await
                        {
                            let _ = self
                                .bridge
                                .tx
                                .send(GtkCommand::SetAgentStatus {
                                    workspace: ws_id,
                                    status: rollup,
                                })
                                .await;
                        }
                    }
                    Response::Ok
                }
                None => Response::Ok,
            },
            other => unreachable!("agent router got a non-agent verb: {other:?}"),
        }
    }

    /// Dispatch for the notification verb group (split out of the `handle` match).
    async fn handle_notification_verb(&self, req: Request) -> Response {
        match req {
            Request::Notify {
                pane,
                surface,
                ref title,
                ref body,
                level,
            } => {
                // Try to recover pane/surface from the pane title
                // when the hook source could not pass them. The
                // Flatpak OpenCode plugin path is the trigger: the
                // host->sandbox transition strips env, so the
                // in-sandbox CLI cannot read FLOWMUX_PANE_ID and
                // the request lands with `pane=None surface=None`.
                // The pane that actually ran the agent already has
                // its tab title flipped to the agent name by
                // workspace_view's terminal title hook, so we can
                // resolve back to it by matching on the first
                // whitespace-delimited token of `title` (e.g.
                // "OpenCode" out of "OpenCode ready"). When both
                // pane and surface arrive as None, this fallback
                // is the only path that lets `mark_attention`
                // (sidebar blink) and `focus_pane` (bell-click
                // navigation) work for the Flatpak hook.
                let (resolved_pane, resolved_surface, fallback_used) =
                    if pane.is_none() && surface.is_none() {
                        let needle = title.split_whitespace().next().unwrap_or("");
                        match self
                            .inner
                            .store()
                            .find_pane_by_active_title_prefix(needle)
                            .await
                        {
                            Some((_ws_id, p, s)) => (Some(p), Some(s), true),
                            None => (pane, surface, false),
                        }
                    } else {
                        (pane, surface, false)
                    };
                // Pre-resolve the workspace here so the GTK side
                // can route the click without a second store
                // lookup (the dispatcher still falls back to a
                // late lookup if it sees `workspace = None`).
                let workspace = match resolved_pane {
                    Some(p) => self.inner.store().workspace_for_pane(p).await,
                    None => None,
                };
                tracing::info!(
                    ?pane,
                    ?surface,
                    resolved_pane = ?resolved_pane,
                    resolved_surface = ?resolved_surface,
                    fallback_used,
                    ?workspace,
                    title = %title,
                    ?level,
                    "Notify request received — routing to GTK"
                );
                flowmux_config::notify_debug!(
                        "gui/ipc",
                        "Notify received pane_in={pane:?} surface_in={surface:?} resolved_pane={resolved_pane:?} resolved_surface={resolved_surface:?} fallback_used={fallback_used} workspace={workspace:?} title={title:?} level={level:?}"
                    );
                let pane = resolved_pane;
                let surface = resolved_surface;
                // Ask the GTK side to record the entry. The ack
                // returns `None` when the source pane+surface is
                // already focused — in that case we also skip the
                // desktop toast so flowmux stays out of the way.
                // `Some(entry_id)` is the in-process popover id we
                // need so we can later attach the gtk notifications
                // id returned by the daemon.
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::AddNotification {
                        pane,
                        surface,
                        workspace,
                        title: title.clone(),
                        body: body.clone(),
                        level,
                        ack: tx,
                    })
                    .await;
                let entry_id = rx.await.unwrap_or(None);
                if let Some(entry_id) = entry_id {
                    let resp = self
                        .inner
                        .handle(Request::Notify {
                            pane,
                            surface,
                            title: title.clone(),
                            body: body.clone(),
                            level,
                        })
                        .await;
                    // Forward the desktop id (when present) to the
                    // GUI store so the bell popover's "mark all
                    // read" sweep can later ask the FDO daemon to
                    // close the toast and shrink the dock badge.
                    if let Response::Notified {
                        desktop_id: Some(ref desktop_id),
                    } = resp
                    {
                        let _ = self
                            .bridge
                            .tx
                            .send(GtkCommand::SetNotificationDesktopId {
                                id: entry_id,
                                desktop_id: desktop_id.clone(),
                            })
                            .await;
                    }
                    // Hooks and other CLI callers expect a benign
                    // success — collapse Notified into Ok so the
                    // wire shape stays stable for them.
                    match resp {
                        Response::Notified { .. } => Response::Ok,
                        other => other,
                    }
                } else {
                    Response::Ok
                }
            }
            Request::NotificationsList { unread_only } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::ListNotifications {
                        unread_only,
                        ack: tx,
                    })
                    .await;
                match rx.await {
                    Ok((entries, unread_count)) => Response::Notifications {
                        entries,
                        unread_count,
                    },
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::NotificationOpen { id } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::OpenNotificationWithAck { id, ack: tx })
                    .await;
                match rx.await {
                    Ok(changed) => Response::NotificationState { changed },
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::NotificationJumpToUnread => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::OpenOldestUnreadNotification { ack: tx })
                    .await;
                match rx.await {
                    Ok(changed) => Response::NotificationState { changed },
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::NotificationMarkRead { id } => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::MarkNotificationRead { id, ack: tx })
                    .await;
                match rx.await {
                    Ok(changed) => Response::NotificationState { changed },
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }
            Request::NotificationsClear => {
                let (tx, rx) = oneshot::channel();
                let _ = self
                    .bridge
                    .tx
                    .send(GtkCommand::ClearNotifications { ack: tx })
                    .await;
                match rx.await {
                    Ok(changed) => Response::NotificationState { changed },
                    Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                }
            }

            // Everything else is fully GUI-independent: ping, list, and
            // notification operations delegated above.
            other => unreachable!("notification router got a non-notification verb: {other:?}"),
        }
    }
}

/// Bridge-backed [`TmuxCompatUi`]: applies each tmux-compat side effect
/// on the GTK main thread, mirroring how the plain pane verbs dispatch.
///
/// [`TmuxCompatUi`]: flowmux_daemon::tmux_compat::TmuxCompatUi
struct GuiTmuxUi<'a> {
    bridge: &'a Bridge,
}

impl flowmux_daemon::tmux_compat::TmuxCompatUi for GuiTmuxUi<'_> {
    async fn workspace_created(
        &self,
        id: flowmux_core::WorkspaceId,
        name: &str,
        root: &std::path::Path,
    ) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::WorkspaceCreated {
                id,
                name: name.to_string(),
                root: root.to_path_buf(),
                ack: tx,
            })
            .await;
        let _ = rx.await;
    }

    async fn pane_split_applied(
        &self,
        workspace: flowmux_core::WorkspaceId,
        pane: flowmux_core::PaneId,
        new_pane: flowmux_core::PaneId,
        direction: SplitDirection,
    ) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::PaneSplitApplied {
                id: workspace,
                pane,
                new_pane,
                direction,
                ack: tx,
            })
            .await;
        let _ = rx.await;
    }

    async fn send_keys(&self, pane: flowmux_core::PaneId, keys: &str) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::PaneSendKeys {
                pane,
                keys: keys.to_string(),
                ack: tx,
            })
            .await;
        match rx.await {
            Ok(result) => result,
            Err(_) => Err("bridge closed".into()),
        }
    }

    async fn rename_surface(
        &self,
        pane: flowmux_core::PaneId,
        surface: flowmux_core::SurfaceId,
        title: &str,
    ) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::RenameSurface {
                pane,
                surface,
                title: title.to_string(),
                ack: tx,
            })
            .await;
        match rx.await {
            Ok(result) => result,
            Err(_) => Err("bridge closed".into()),
        }
    }

    async fn close_pane(&self, pane: flowmux_core::PaneId) -> Result<(), String> {
        // The orchestrator only closes non-last panes, so this stays on
        // CloseFocused's no-dialog path.
        let (tx, rx) = oneshot::channel();
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::CloseFocused { pane, ack: tx })
            .await;
        match rx.await {
            Ok(result) => result,
            Err(_) => Err("bridge closed".into()),
        }
    }

    async fn remove_workspace(&self, id: flowmux_core::WorkspaceId) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::RemoveWorkspace {
                id,
                // Agent-driven teardown must never block on a modal.
                confirm: false,
                ack: tx,
            })
            .await;
        let _ = rx.await;
    }

    async fn workspace_activated(&self, id: flowmux_core::WorkspaceId) {
        let _ = self
            .bridge
            .tx
            .send(GtkCommand::ActivateWorkspace { id })
            .await;
    }
}

#[cfg(test)]
#[path = "ipc_handler_tests.rs"]
mod tests;
