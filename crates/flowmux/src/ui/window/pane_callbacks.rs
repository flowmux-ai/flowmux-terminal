// SPDX-License-Identifier: GPL-3.0-or-later
//! Routes pane widget callbacks into GTK commands.

use super::*;

fn dispatch_detached(bridge: &Bridge, command: GtkCommand) {
    let bridge = bridge.clone();
    glib::MainContext::default().spawn_local(async move {
        let _ = bridge.tx.send(command).await;
    });
}

fn dispatch_with_ack<T: 'static>(
    bridge: &Bridge,
    build: impl FnOnce(oneshot::Sender<T>) -> GtkCommand + 'static,
) {
    let bridge = bridge.clone();
    glib::MainContext::default().spawn_local(async move {
        let (ack_tx, ack_rx) = oneshot::channel();
        if bridge.tx.send(build(ack_tx)).await.is_ok() {
            let result = ack_rx.await;
            if let Err(e) = result.as_ref().map(|_| ()).map_err(|_| "channel closed") {
                tracing::debug!(
                    target: "flowmux::dnd",
                    error = %e,
                    "dispatch_with_ack: ack channel closed before response"
                );
            }
        }
    });
}

/// Synchronous dispatch with an ack channel returned to the caller so the
/// drop handler can await the store + widget outcome before calling
/// `drop.finish()`. Returns `None` if the bridge channel is full or closed.
fn dispatch_with_ack_sync<T: 'static + std::fmt::Debug>(
    bridge: &Bridge,
    build: impl FnOnce(oneshot::Sender<T>) -> GtkCommand + 'static,
) -> Option<oneshot::Receiver<T>> {
    let (ack_tx, ack_rx) = oneshot::channel();
    match bridge.tx.try_send(build(ack_tx)) {
        Ok(_) => Some(ack_rx),
        Err(async_channel::TrySendError::Full(_)) => {
            tracing::warn!(
                target: "flowmux::dnd",
                "dispatch_with_ack_sync: bridge channel full, cannot dispatch drop"
            );
            None
        }
        Err(async_channel::TrySendError::Closed(_)) => {
            tracing::warn!(
                target: "flowmux::dnd",
                "dispatch_with_ack_sync: bridge channel closed, cannot dispatch drop"
            );
            None
        }
    }
}

/// Owns the dependencies captured by pane callbacks and builds the callback
/// table consumed by terminal/browser pane widgets.
pub(super) struct PaneCallbackRouter {
    focused: FocusedPane,
    bridge: Bridge,
    options: Rc<RefCell<flowmux_config::options::Options>>,
    pane_registry: Rc<RefCell<PaneRegistry>>,
    workspace_titles: Rc<RefCell<Vec<(WorkspaceId, String)>>>,
    drag_state: Rc<RefCell<DragState>>,
}

impl PaneCallbackRouter {
    pub(super) fn new(
        focused: FocusedPane,
        bridge: Bridge,
        options: Rc<RefCell<flowmux_config::options::Options>>,
        pane_registry: Rc<RefCell<PaneRegistry>>,
        workspace_titles: Rc<RefCell<Vec<(WorkspaceId, String)>>>,
        drag_state: Rc<RefCell<DragState>>,
    ) -> Self {
        Self {
            focused,
            bridge,
            options,
            pane_registry,
            workspace_titles,
            drag_state,
        }
    }

    pub(super) fn into_callbacks(self) -> PaneCallbacks {
        let Self {
            focused,
            bridge,
            options,
            pane_registry,
            workspace_titles,
            drag_state,
        } = self;
        use std::cell::RefCell;
        use std::rc::Rc;
        PaneCallbacks {
            on_child_exited: Rc::new(RefCell::new(|pane, status| {
                tracing::info!(%pane, status, "child exited");
            })),
            on_close_pane: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    dispatch_with_ack(&bridge, move |ack| GtkCommand::CloseFocused { pane, ack });
                }))
            },
            on_focus: {
                let bridge = bridge.clone();
                let focused = focused.clone();
                Rc::new(RefCell::new(move |pane| {
                    tracing::debug!(%pane, "pane focused");
                    focused.set(Some(pane));
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge.tx.send(GtkCommand::PaneFocused { pane }).await;
                        let _ = bridge.tx.send(GtkCommand::RefreshWindowTitle).await;
                    });
                }))
            },
            on_split_right: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    dispatch_with_ack(&bridge, move |ack| GtkCommand::SplitFocused {
                        pane,
                        direction: flowmux_core::SplitDirection::Vertical,
                        ack,
                    });
                }))
            },
            on_split_down: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    dispatch_with_ack(&bridge, move |ack| GtkCommand::SplitFocused {
                        pane,
                        direction: flowmux_core::SplitDirection::Horizontal,
                        ack,
                    });
                }))
            },
            on_activate_surface: {
                let bridge = bridge.clone();
                let focused = focused.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    focused.set(Some(pane));
                    dispatch_detached(&bridge, GtkCommand::ActivateSurface { pane, surface });
                }))
            },
            on_new_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    dispatch_detached(&bridge, GtkCommand::NewSurface { pane });
                }))
            },
            on_new_browser_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    dispatch_detached(&bridge, GtkCommand::NewBrowserSurface { pane });
                }))
            },
            on_close_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    dispatch_with_ack(&bridge, move |ack| GtkCommand::CloseSurface {
                        pane,
                        surface,
                        ack,
                    });
                }))
            },
            on_rename_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    dispatch_detached(
                        &bridge,
                        GtkCommand::ShowRenameSurfaceDialog { pane, surface },
                    );
                }))
            },
            on_show_surface_folder: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    dispatch_detached(&bridge, GtkCommand::ShowSurfaceFolder { pane, surface });
                }))
            },
            on_copy_surface_text: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    dispatch_detached(&bridge, GtkCommand::CopySurfaceText { pane, surface });
                }))
            },
            on_reorder_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, target_index| {
                    dispatch_with_ack(&bridge, move |ack| GtkCommand::ReorderSurface {
                        pane,
                        surface,
                        target_index,
                        ack,
                    });
                }))
            },
            on_tab_drag_to_new_window: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    tracing::debug!(%pane, %surface, "tab drag requested tear-off window");
                    dispatch_detached(&bridge, GtkCommand::TearOffSurface { pane, surface });
                }))
            },
            on_move_surface_to_pane: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(
                    move |src_pane, surface, surface_model, dst_pane, target_index| {
                        dispatch_with_ack(&bridge, move |ack| GtkCommand::MoveSurfaceToPane {
                            src_pane,
                            surface,
                            surface_model,
                            dst_pane,
                            target_index,
                            ack,
                        });
                    },
                ))
            },
            on_move_surface_to_workspace: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |src_pane, surface, dst_workspace| {
                    dispatch_with_ack(&bridge, move |ack| GtkCommand::MoveSurfaceToWorkspace {
                        src_pane,
                        surface,
                        dst_workspace,
                        ack,
                    });
                }))
            },
            list_workspaces: {
                let workspace_titles = workspace_titles.clone();
                Rc::new(move || workspace_titles.borrow().clone())
            },
            workspace_of_pane: {
                let pane_registry = pane_registry.clone();
                Rc::new(move |pane| pane_registry.borrow().workspace_of_pane(pane))
            },
            on_split_surface_into_pane: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(
                    move |src_pane, surface, surface_model, dst_pane, direction| {
                        dispatch_with_ack(&bridge, move |ack| GtkCommand::SplitSurfaceIntoPane {
                            src_pane,
                            surface,
                            surface_model,
                            dst_pane,
                            direction,
                            ack,
                        });
                    },
                ))
            },
            drag_state: drag_state.clone(),
            tab_drag_split_candidate: Rc::new(RefCell::new(None)),
            dispatch_tab_drop: {
                let bridge = bridge.clone();
                Rc::new(move |cmd| {
                    use crate::ui::pane_terminal::TabDropCommand;
                    dispatch_with_ack_sync(&bridge, |ack| match cmd {
                        TabDropCommand::MoveToPane {
                            src_pane,
                            surface,
                            surface_model,
                            dst_pane,
                            target_index,
                        } => GtkCommand::MoveSurfaceToPane {
                            src_pane,
                            surface,
                            surface_model,
                            dst_pane,
                            target_index,
                            ack,
                        },
                        TabDropCommand::SplitIntoPane {
                            src_pane,
                            surface,
                            surface_model,
                            dst_pane,
                            direction,
                        } => GtkCommand::SplitSurfaceIntoPane {
                            src_pane,
                            surface,
                            surface_model,
                            dst_pane,
                            direction,
                            ack,
                        },
                        TabDropCommand::Reorder {
                            pane,
                            surface,
                            target_index,
                        } => GtkCommand::ReorderSurface {
                            pane,
                            surface,
                            target_index,
                            ack,
                        },
                    })
                })
            },
            drag_in_flight: Rc::new(Cell::new(None)),
            on_terminal_cwd_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, cwd| {
                    dispatch_detached(
                        &bridge,
                        GtkCommand::TerminalCwdChanged { pane, surface, cwd },
                    );
                }))
            },
            on_browser_uri_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, url| {
                    dispatch_detached(
                        &bridge,
                        GtkCommand::BrowserUriChanged { pane, surface, url },
                    );
                }))
            },
            on_browser_title_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, title| {
                    dispatch_detached(
                        &bridge,
                        GtkCommand::BrowserTitleChanged {
                            pane,
                            surface,
                            title,
                        },
                    );
                }))
            },
            on_terminal_title_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, title: String| {
                    dispatch_detached(
                        &bridge,
                        GtkCommand::TerminalTitleChanged {
                            pane,
                            surface,
                            title,
                        },
                    );
                }))
            },
            read_options: {
                let options = options.clone();
                Rc::new(move || options.borrow().clone())
            },
            position_of_surface_in_pane: {
                let registry = pane_registry.clone();
                Rc::new(move |pane, surface| {
                    let r = registry.borrow();
                    r.surface_tabs
                        .get(&pane)?
                        .iter()
                        .position(|(id, _)| *id == surface)
                })
            },
            on_open_url: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, url| {
                    dispatch_detached(&bridge, GtkCommand::OpenUrlInBrowserTab { pane, url });
                }))
            },
            on_open_image: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, path| {
                    dispatch_detached(&bridge, GtkCommand::OpenImageViewer { pane, path });
                }))
            },
            on_open_markdown: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, path| {
                    dispatch_detached(&bridge, GtkCommand::OpenMarkdownViewer { pane, path });
                }))
            },
        }
    }
}
