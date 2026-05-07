// SPDX-License-Identifier: GPL-3.0-or-later
//! GUI-aware IPC handler.
//!
//! Wraps `flowmux_daemon::DaemonHandler` and intercepts the verbs that
//! need to mutate the GTK widget tree (workspace creation, pane split,
//! send-keys, browser open). Those verbs are forwarded across the
//! [`Bridge`] to the GTK main loop and the response is awaited via a
//! `oneshot` channel.

use crate::bridge::{Bridge, GtkCommand};
use flowmux_core::SplitDirection;
use flowmux_daemon::DaemonHandler;
use flowmux_ipc::protocol::{Request, Response, RpcError};
use flowmux_ipc::server::Handler;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::oneshot;
use tracing::warn;

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
                Request::BrowserOpen { url, surface: _ } => {
                    let store = self.inner.store();
                    let ws_id = match store.active_or_first().await {
                        Some(id) => id,
                        None => {
                            return Response::Error(RpcError::InvalidArgument(
                                "no active workspace; create one first".into(),
                            ))
                        }
                    };
                    if store.add_browser_surface(ws_id, url).await.is_none() {
                        return Response::Error(RpcError::NotFound(ws_id.to_string()));
                    }
                    let (tx, rx) = oneshot::channel();
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::WorkspaceRerender { id: ws_id, ack: tx })
                        .await;
                    let _ = rx.await;
                    Response::Ok
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
                    let (tx, rx) = oneshot::channel();
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::BrowserEval {
                            pane,
                            source: BROWSER_SNAPSHOT_JS.to_string(),
                            ack: tx,
                        })
                        .await;
                    match rx.await {
                        Ok(Ok(value)) => Response::BrowserResult { value },
                        Ok(Err(e)) => Response::Error(RpcError::Internal(e)),
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

                Request::Notify {
                    pane,
                    ref title,
                    ref body,
                    level,
                } => {
                    // Tee to the GUI's in-process notification log so
                    // the sidebar bell popover sees it. The desktop
                    // toast still goes out through DaemonHandler.
                    let _ = self
                        .bridge
                        .tx
                        .send(GtkCommand::AddNotification {
                            title: title.clone(),
                            body: body.clone(),
                            level,
                        })
                        .await;
                    self.inner
                        .handle(Request::Notify {
                            pane,
                            title: title.clone(),
                            body: body.clone(),
                            level,
                        })
                        .await
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

/// JS executed inside the browser pane to produce an a11y/DOM
/// snapshot. Returns a JSON string. Each node has:
///   - `ref`: a stable per-snapshot id ("e1", "e2", ...) that the
///     caller can pass back to click/fill verbs (those land next).
///   - `tag`, `role`, `name`, `text` (truncated), and a flat `bbox`.
/// Hidden / off-screen / very small elements are skipped to keep the
/// payload tractable for agent consumption.
const BROWSER_SNAPSHOT_JS: &str = r#"
(function () {
  const out = [];
  let counter = 0;
  function visible(el) {
    const r = el.getBoundingClientRect();
    if (r.width < 4 || r.height < 4) return false;
    const cs = window.getComputedStyle(el);
    if (cs.visibility === 'hidden' || cs.display === 'none') return false;
    if (Number(cs.opacity) === 0) return false;
    return true;
  }
  function name(el) {
    return (
      el.getAttribute('aria-label') ||
      el.getAttribute('alt') ||
      el.getAttribute('title') ||
      el.getAttribute('placeholder') ||
      (el.innerText || '').trim().slice(0, 120)
    );
  }
  document.querySelectorAll(
    'a,button,input,textarea,select,[role],h1,h2,h3,label,summary'
  ).forEach((el) => {
    if (!visible(el)) return;
    const r = el.getBoundingClientRect();
    counter += 1;
    const ref = 'e' + counter;
    el.setAttribute('data-flowmux-ref', ref);
    out.push({
      ref,
      tag: el.tagName.toLowerCase(),
      role: el.getAttribute('role') || el.tagName.toLowerCase(),
      name: name(el),
      bbox: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)],
    });
  });
  return JSON.stringify({ url: location.href, title: document.title, nodes: out });
})()
"#;

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
