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
        Ok(Err(e)) => Response::Error(RpcError::Internal(e)),
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
                                .send(GtkCommand::WorkspaceRerender { id: ws_id, ack: tx })
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
                    let known = flowmux_ipc::protocol::describe_workspaces(
                        &self.inner.store().snapshot().await.workspaces,
                    )
                    .iter()
                    .flat_map(|w| &w.panes)
                    .any(|p| p.id == pane);
                    if known {
                        let _ = self
                            .bridge
                            .tx
                            .send(GtkCommand::ActivateSurface { pane, surface })
                            .await;
                        Response::Ok
                    } else {
                        Response::Error(RpcError::NotFound(format!("pane not found: {pane}")))
                    }
                }
                Request::SurfaceClose { pane, surface } => {
                    // Refuse the last-tab-of-last-pane case up front so the
                    // agent never trips CloseSurface's confirm dialog.
                    let tabs = self.inner.store().tab_count_in_pane(pane).await;
                    let panes = self.inner.store().workspace_pane_count_for(pane).await;
                    if tabs == Some(1) && matches!(panes, Some((_, 1))) {
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
                        Ok(Err(e)) => Response::Error(RpcError::Internal(e)),
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
                    // Create a fresh workspace.
                    let ws_id = store
                        .create_workspace(Some("claude-teams".into()), root)
                        .await;
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
                            all_panes.push(new_pane);
                            current = new_pane;
                        }
                    }
                    // Re-render once.
                    let (tx, rx) = oneshot::channel();
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::WorkspaceRerender { id: ws_id, ack: tx })
                        .await;
                    let _ = rx.await;
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
                        Ok(Err(e)) => Response::Error(RpcError::Internal(e)),
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
