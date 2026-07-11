// SPDX-License-Identifier: GPL-3.0-or-later
//! Routes pane widget callbacks into GTK commands.

use super::*;

/// Owns the dependencies captured by pane callbacks and builds the callback
/// table consumed by terminal/browser pane widgets.
pub(super) struct PaneCallbackRouter {
    focused: FocusedPane,
    bridge: Bridge,
    options: Rc<RefCell<flowmux_config::options::Options>>,
    pane_registry: Rc<RefCell<PaneRegistry>>,
    workspace_titles: Rc<RefCell<Vec<(WorkspaceId, String)>>>,
    tab_drag_drop_seen: Rc<Cell<bool>>,
    tab_drag_drop_committed: Rc<Cell<bool>>,
}

impl PaneCallbackRouter {
    pub(super) fn new(
        focused: FocusedPane,
        bridge: Bridge,
        options: Rc<RefCell<flowmux_config::options::Options>>,
        pane_registry: Rc<RefCell<PaneRegistry>>,
        workspace_titles: Rc<RefCell<Vec<(WorkspaceId, String)>>>,
        tab_drag_drop_seen: Rc<Cell<bool>>,
        tab_drag_drop_committed: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            focused,
            bridge,
            options,
            pane_registry,
            workspace_titles,
            tab_drag_drop_seen,
            tab_drag_drop_committed,
        }
    }

    pub(super) fn into_callbacks(self) -> PaneCallbacks {
        let Self {
            focused,
            bridge,
            options,
            pane_registry,
            workspace_titles,
            tab_drag_drop_seen,
            tab_drag_drop_committed,
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
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (tx, _rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::CloseFocused { pane, ack: tx })
                            .await;
                    });
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
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (tx, _rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::SplitFocused {
                                pane,
                                direction: flowmux_core::SplitDirection::Vertical,
                                ack: tx,
                            })
                            .await;
                    });
                }))
            },
            on_split_down: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (tx, _rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::SplitFocused {
                                pane,
                                direction: flowmux_core::SplitDirection::Horizontal,
                                ack: tx,
                            })
                            .await;
                    });
                }))
            },
            on_activate_surface: {
                let bridge = bridge.clone();
                let focused = focused.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    focused.set(Some(pane));
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::ActivateSurface { pane, surface })
                            .await;
                    });
                }))
            },
            on_new_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge.tx.send(GtkCommand::NewSurface { pane }).await;
                    });
                }))
            },
            on_new_browser_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge.tx.send(GtkCommand::NewBrowserSurface { pane }).await;
                    });
                }))
            },
            on_close_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (tx, _rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::CloseSurface {
                                pane,
                                surface,
                                ack: tx,
                            })
                            .await;
                    });
                }))
            },
            on_rename_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::ShowRenameSurfaceDialog { pane, surface })
                            .await;
                    });
                }))
            },
            on_show_surface_folder: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::ShowSurfaceFolder { pane, surface })
                            .await;
                    });
                }))
            },
            on_copy_surface_text: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::CopySurfaceText { pane, surface })
                            .await;
                    });
                }))
            },
            on_reorder_surface: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, target_index| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (tx, _rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::ReorderSurface {
                                pane,
                                surface,
                                target_index,
                                ack: tx,
                            })
                            .await;
                    });
                }))
            },
            on_tab_drag_to_new_window: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    tracing::debug!(%pane, %surface, "tab drag requested tear-off window");
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::TearOffSurface { pane, surface })
                            .await;
                    });
                }))
            },
            on_move_surface_to_pane: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(
                    move |src_pane, surface, surface_model, dst_pane, target_index| {
                        let bridge = bridge.clone();
                        glib::MainContext::default().spawn_local(async move {
                            let (ack_tx, ack_rx) = oneshot::channel();
                            let _ = bridge
                                .tx
                                .send(GtkCommand::MoveSurfaceToPane {
                                    src_pane,
                                    surface,
                                    surface_model,
                                    dst_pane,
                                    target_index,
                                    ack: ack_tx,
                                })
                                .await;
                            let _ = ack_rx.await;
                        });
                    },
                ))
            },
            on_move_surface_to_workspace: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |src_pane, surface, dst_workspace| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (ack_tx, ack_rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::MoveSurfaceToWorkspace {
                                src_pane,
                                surface,
                                dst_workspace,
                                ack: ack_tx,
                            })
                            .await;
                        let _ = ack_rx.await;
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
                        let bridge = bridge.clone();
                        glib::MainContext::default().spawn_local(async move {
                            let (ack_tx, ack_rx) = oneshot::channel();
                            let _ = bridge
                                .tx
                                .send(GtkCommand::SplitSurfaceIntoPane {
                                    src_pane,
                                    surface,
                                    surface_model,
                                    dst_pane,
                                    direction,
                                    ack: ack_tx,
                                })
                                .await;
                            let _ = ack_rx.await;
                        });
                    },
                ))
            },
            tab_drag_drop_seen,
            tab_drag_drop_committed,
            tab_drag_split_candidate: Rc::new(RefCell::new(None)),
            on_terminal_cwd_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, cwd| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::TerminalCwdChanged { pane, surface, cwd })
                            .await;
                    });
                }))
            },
            on_browser_uri_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, url| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::BrowserUriChanged { pane, surface, url })
                            .await;
                    });
                }))
            },
            on_browser_title_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, title| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::BrowserTitleChanged {
                                pane,
                                surface,
                                title,
                            })
                            .await;
                    });
                }))
            },
            on_terminal_title_changed: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface, title: String| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::TerminalTitleChanged {
                                pane,
                                surface,
                                title,
                            })
                            .await;
                    });
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
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::OpenUrlInBrowserTab { pane, url })
                            .await;
                    });
                }))
            },
            on_open_image: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, path| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::OpenImageViewer { pane, path })
                            .await;
                    });
                }))
            },
            on_open_markdown: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, path| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::OpenMarkdownViewer { pane, path })
                            .await;
                    });
                }))
            },
        }
    }
}
