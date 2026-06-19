// SPDX-License-Identifier: GPL-3.0-or-later
//! GUI-aware IPC handler.
//!
//! Wraps `flowmux_daemon::DaemonHandler` and intercepts the verbs that
//! need to mutate the GTK widget tree (workspace creation, pane split,
//! send-keys, browser open). Those verbs are forwarded across the
//! [`Bridge`] to the GTK main loop and the response is awaited via a
//! `oneshot` channel.

use crate::bridge::{Bridge, BrowserActionResult, BrowserOp, GtkCommand};
use flowmux_core::{AgentPresence, SplitDirection};
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
    let dir = flowmux_config::paths::data_dir()?.join("agent-sessions");
    Some(flowmux_state::AgentSessionStore::new(dir))
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
                        // `None` = built without the `vte-text` feature, so the
                        // VTE text API is unavailable. Report it explicitly
                        // rather than returning empty output.
                        Ok(Ok(None)) => Response::Error(RpcError::Unimplemented(
                            "read-screen requires building flowmux with --features vte-text".into(),
                        )),
                        Ok(Err(e)) => Response::Error(RpcError::NotFound(e)),
                        Err(_) => Response::Error(RpcError::Internal("bridge closed".into())),
                    }
                }
                Request::SurfaceFocus { pane, surface } => {
                    // Validate the pane against live state (reusing the
                    // tree flattener), then fire the same ActivateSurface
                    // the tab bar uses. Non-destructive, no dialog.
                    let tree = flowmux_ipc::protocol::describe_workspaces(
                        &self.inner.store().snapshot().await.workspaces,
                    );
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
                    let tree = flowmux_ipc::protocol::describe_workspaces(
                        &self.inner.store().snapshot().await.workspaces,
                    );
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
                Request::PaneClose { pane } => {
                    // Peek the pane count up front. Closing the workspace's
                    // last pane is what triggers CloseFocused's confirm
                    // dialog; refuse it here (with a clear error) so an
                    // agent's IPC call never blocks on user input.
                    match self.inner.store().workspace_pane_count_for(pane).await {
                        None => {
                            Response::Error(RpcError::NotFound(format!("pane not found: {pane}")))
                        }
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
                                Err(_) => {
                                    Response::Error(RpcError::Internal("bridge closed".into()))
                                }
                            }
                        }
                    }
                }
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
                Request::BrowserNavigate { pane, url } => {
                    browser_action(&self.bridge, pane, BrowserOp::Navigate { url }).await
                }
                Request::BrowserBack { pane } => {
                    browser_action(&self.bridge, pane, BrowserOp::Back).await
                }
                Request::BrowserForward { pane } => {
                    browser_action(&self.bridge, pane, BrowserOp::Forward).await
                }
                Request::BrowserReload { pane } => {
                    browser_action(&self.bridge, pane, BrowserOp::Reload).await
                }
                Request::BrowserUrl { pane } => {
                    browser_action(&self.bridge, pane, BrowserOp::Url).await
                }
                Request::BrowserTitle { pane } => {
                    browser_action(&self.bridge, pane, BrowserOp::Title).await
                }
                Request::BrowserClick { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Click { target }).await
                }
                Request::BrowserFill {
                    pane,
                    target,
                    value,
                } => browser_action(&self.bridge, pane, BrowserOp::Fill { target, value }).await,
                Request::BrowserSelect {
                    pane,
                    target,
                    value,
                } => browser_action(&self.bridge, pane, BrowserOp::Select { target, value }).await,
                Request::BrowserScroll { pane, target, x, y } => {
                    browser_action(&self.bridge, pane, BrowserOp::Scroll { target, x, y }).await
                }
                Request::BrowserType { pane, text } => {
                    browser_action(&self.bridge, pane, BrowserOp::Type { text }).await
                }
                Request::BrowserPress { pane, key } => {
                    browser_action(&self.bridge, pane, BrowserOp::Press { key }).await
                }
                Request::BrowserText { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Text { target }).await
                }
                Request::BrowserValue { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Value { target }).await
                }
                Request::BrowserAttr { pane, target, name } => {
                    browser_action(&self.bridge, pane, BrowserOp::Attr { target, name }).await
                }

                // ---- Phase 5 P0 action gap ------------------------
                Request::BrowserDblClick { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::DblClick { target }).await
                }
                Request::BrowserHover { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Hover { target }).await
                }
                Request::BrowserFocus { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Focus { target }).await
                }
                Request::BrowserBlur { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Blur { target }).await
                }
                Request::BrowserCheck { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Check { target }).await
                }
                Request::BrowserUncheck { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::Uncheck { target }).await
                }
                Request::BrowserIsVisible { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::IsVisible { target }).await
                }
                Request::BrowserIsEnabled { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::IsEnabled { target }).await
                }
                Request::BrowserIsChecked { pane, target } => {
                    browser_action(&self.bridge, pane, BrowserOp::IsChecked { target }).await
                }
                Request::BrowserCount { pane, selector } => {
                    browser_action(&self.bridge, pane, BrowserOp::Count { selector }).await
                }

                Request::ClaudeTeams { count, args, root } => {
                    let count = count.max(1).min(8);
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

                Request::BrowserSnapshot { pane } => {
                    // Routed through the shared browser_action path so the
                    // GTK side runs the non-mutating SNAPSHOT_JS and
                    // repopulates the pane's RefStore. The response shape
                    // (BrowserResult { value }) is unchanged.
                    browser_action(&self.bridge, pane, BrowserOp::Snapshot).await
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
                    activity,
                    pid,
                    ..
                } => match surface {
                    Some(surface) => {
                        let presence = activity.map(|act| AgentPresence {
                            name: agent,
                            activity: act,
                            pid,
                        });
                        if let Some(ws_id) = self
                            .inner
                            .store()
                            .set_agent_activity(surface, presence)
                            .await
                        {
                            let _ = self
                                .bridge
                                .tx
                                .send(GtkCommand::SetAgentActivity {
                                    workspace: ws_id,
                                    activity,
                                })
                                .await;
                        }
                        Response::Ok
                    }
                    None => Response::Ok,
                },

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

                // Everything else is fully GUI-independent: ping, list,
                // notify (delegated above), ssh — delegate.
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

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_core::{NotificationLevel, PaneId, SurfaceId, WorkspaceId};
    use flowmux_daemon::StateStore;
    use flowmux_state::State;

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
}
