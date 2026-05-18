// SPDX-License-Identifier: GPL-3.0-or-later
//! Production [`Handler`] for the flowmux IPC server.
//!
//! Owns the [`StateStore`] and dispatches verbs against it. Verbs that
//! depend on a GTK runtime (terminal spawn, browser open, pane split
//! materializing widgets) return [`RpcError::Unimplemented`] in the
//! headless context — the GUI binary wraps `DaemonHandler` to override
//! those verbs.

use crate::state_store::StateStore;
use flowmux_core::{Notification, NotificationId};
use flowmux_ipc::protocol::{Request, Response, RpcError};
use flowmux_ipc::server::Handler;
use flowmux_ssh::SshTarget;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{info, warn};

pub struct DaemonHandler {
    store: StateStore,
    notifier: Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>>,
}

impl DaemonHandler {
    pub fn new(store: StateStore) -> Self {
        Self {
            store,
            notifier: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    /// Hand out a clone of the lazily-instantiated notifier cell so the
    /// GUI side can issue `RemoveNotification` through the **same**
    /// `Connection::session()` that this handler used to send the
    /// `AddNotification`. gnome-shell's `org.gtk.Notifications` keys
    /// each entry by `(sender bus name, app_id)`, so a withdraw from a
    /// second connection silently fails to match and the dock badge /
    /// message-tray entry stays pinned. Sharing the cell makes both
    /// sides reuse the first connection that wins the lazy init race.
    pub fn notifier_handle(&self) -> Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>> {
        self.notifier.clone()
    }

    async fn ensure_notifier(
        &self,
    ) -> Option<tokio::sync::MutexGuard<'_, Option<flowmux_notify::DesktopNotifier>>> {
        let mut guard = self.notifier.lock().await;
        if guard.is_none() {
            match flowmux_notify::DesktopNotifier::connect().await {
                Ok(n) => *guard = Some(n),
                Err(e) => {
                    warn!(error = %e, "could not connect to org.gtk.Notifications");
                    return None;
                }
            }
        }
        Some(guard)
    }
}

impl Handler for DaemonHandler {
    fn handle<'a>(&'a self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
        Box::pin(async move {
            match req {
                Request::Ping => Response::Pong,

                Request::WorkspaceCreate { name, root } => {
                    let id = self.store.create_workspace(name, root.clone()).await;
                    info!(%id, root = %root.display(), "workspace created");
                    // Best-effort enrichment.
                    let store = self.store.clone();
                    tokio::spawn(async move {
                        if let Ok(Some(info)) = flowmux_vcs::inspect(&root).await {
                            store.replace_git_info(id, Some(info)).await;
                        }
                    });
                    Response::WorkspaceCreated { id }
                }

                Request::WorkspaceList => {
                    let ids = self.store.list_workspaces().await;
                    Response::WorkspaceList { ids }
                }

                Request::Notify {
                    pane,
                    surface: _,
                    title,
                    body,
                    level,
                } => {
                    flowmux_config::notify_debug!(
                        "daemon/notify",
                        "Notify reached daemon handler pane={pane:?} title={title:?} level={level:?}"
                    );
                    let n = Notification {
                        id: NotificationId::new(),
                        level,
                        title,
                        body,
                        source_pane: pane,
                        created_at: chrono::Utc::now(),
                        read: false,
                    };
                    let mut desktop_id: Option<String> = None;
                    if let Some(guard) = self.ensure_notifier().await {
                        if let Some(notifier) = guard.as_ref() {
                            match notifier.send(&n).await {
                                Ok(id) => {
                                    desktop_id = Some(id.clone());
                                    flowmux_config::notify_debug!(
                                        "daemon/notify",
                                        "desktop toast sent ok desktop_id={id}"
                                    );
                                }
                                Err(e) => {
                                    warn!(error = %e, "desktop notification failed");
                                    flowmux_config::notify_debug!(
                                        "daemon/notify",
                                        "desktop toast FAILED: {e}"
                                    );
                                }
                            }
                        } else {
                            flowmux_config::notify_debug!(
                                "daemon/notify",
                                "notifier guard present but inner None — D-Bus init never succeeded"
                            );
                        }
                    } else {
                        flowmux_config::notify_debug!(
                            "daemon/notify",
                            "ensure_notifier() returned None — no D-Bus session?"
                        );
                    }
                    Response::Notified { desktop_id }
                }

                Request::CloseDesktopNotification { desktop_id } => {
                    if let Some(guard) = self.ensure_notifier().await {
                        if let Some(notifier) = guard.as_ref() {
                            if let Err(e) = notifier.close(&desktop_id).await {
                                // Closing an unknown id is benign — the
                                // notifications daemon may already have
                                // dropped the entry, e.g. because the
                                // user cleared the message tray by hand.
                                warn!(error = %e, %desktop_id, "close notification failed");
                            }
                        }
                    }
                    Response::Ok
                }

                Request::SshConnect { target } => match SshTarget::parse(&target) {
                    Ok(_) => Response::Error(RpcError::Unimplemented(
                        "ssh authentication not yet wired".into(),
                    )),
                    Err(e) => Response::Error(RpcError::InvalidArgument(e.to_string())),
                },

                other => Response::Error(RpcError::Unimplemented(format!("{other:?}"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_state::State;

    #[tokio::test]
    async fn handles_ping_workspace_create_and_list() {
        let handler = DaemonHandler::new(StateStore::new_lazy(State::default()));

        assert!(matches!(
            handler.handle(Request::Ping).await,
            Response::Pong
        ));

        let root = std::path::PathBuf::from("/tmp/flowmux-handler-test");
        let response = handler
            .handle(Request::WorkspaceCreate {
                name: Some("demo".into()),
                root,
            })
            .await;
        let id = match response {
            Response::WorkspaceCreated { id } => id,
            other => panic!("expected workspace creation, got {other:?}"),
        };

        let response = handler.handle(Request::WorkspaceList).await;
        assert!(matches!(response, Response::WorkspaceList { ids } if ids == vec![id]));
        assert_eq!(
            handler.store().get_workspace(id).await.unwrap().name,
            "demo"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_ssh_target_before_unimplemented_transport() {
        let handler = DaemonHandler::new(StateStore::new_lazy(State::default()));

        let response = handler
            .handle(Request::SshConnect {
                target: "alice@".into(),
            })
            .await;

        assert!(matches!(
            response,
            Response::Error(RpcError::InvalidArgument(message))
                if message.contains("invalid ssh target")
        ));
    }
}
