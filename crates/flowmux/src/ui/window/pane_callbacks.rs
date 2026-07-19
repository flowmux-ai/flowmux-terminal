// SPDX-License-Identifier: GPL-3.0-or-later
//! Routes pane widget callbacks into GTK commands.

use super::*;
use crate::ui::pane_terminal::TabDropCommand;

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
            let _ = ack_rx.await;
        }
    });
}

fn dispatch_with_ack_result<T>(
    bridge: &Bridge,
    build: impl FnOnce(oneshot::Sender<T>) -> GtkCommand,
) -> Option<oneshot::Receiver<T>> {
    let (ack_tx, ack_rx) = oneshot::channel();
    bridge.tx.try_send(build(ack_tx)).ok()?;
    Some(ack_rx)
}

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
            on_editor_focus_direction: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, direction| {
                    let dir = match direction {
                        flowmux_editor::EditorFocusDirection::Left => FocusDir::Left,
                        flowmux_editor::EditorFocusDirection::Right => FocusDir::Right,
                        flowmux_editor::EditorFocusDirection::Up => FocusDir::Up,
                        flowmux_editor::EditorFocusDirection::Down => FocusDir::Down,
                    };
                    dispatch_detached(
                        &bridge,
                        GtkCommand::FocusDirection {
                            from: Some(pane),
                            dir,
                        },
                    );
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
            on_tab_drag_to_new_window: {
                let bridge = bridge.clone();
                Rc::new(RefCell::new(move |pane, surface| {
                    tracing::debug!(%pane, %surface, "tab drag requested tear-off window");
                    dispatch_detached(&bridge, GtkCommand::TearOffSurface { pane, surface });
                }))
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
            dispatch_tab_drop: {
                let bridge = bridge.clone();
                Rc::new(move |command| {
                    dispatch_with_ack_result(&bridge, |ack| match command {
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
            tab_drag_drop_seen,
            tab_drag_drop_committed,
            tab_drag_split_candidate: Rc::new(RefCell::new(None)),
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
            pane_at_root_point: {
                let registry = pane_registry.clone();
                Rc::new(move |root, x, y| registry.borrow().pane_at_root_point(root, x, y))
            },
            tab_at_root_point: {
                let registry = pane_registry.clone();
                Rc::new(move |root, x, y| registry.borrow().tab_at_root_point(root, x, y))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_os = "macos", test)]
    #[cfg_attr(not(target_os = "macos"), gtk::test)]
    fn editor_focus_direction_routes_to_existing_pane_command() {
        let (bridge, command_rx) = Bridge::new();
        let pane = PaneId::new();
        let callbacks = PaneCallbackRouter::new(
            Rc::new(Cell::new(Some(pane))),
            bridge,
            Rc::new(RefCell::new(flowmux_config::options::Options::default())),
            Rc::new(RefCell::new(PaneRegistry::default())),
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(Cell::new(false)),
            Rc::new(Cell::new(false)),
        )
        .into_callbacks();

        (callbacks.on_editor_focus_direction.borrow_mut())(
            pane,
            flowmux_editor::EditorFocusDirection::Up,
        );

        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
        let command = command_rx.try_recv();

        assert!(matches!(
            command,
            Ok(GtkCommand::FocusDirection {
                from: Some(actual_pane),
                dir: FocusDir::Up,
            }) if actual_pane == pane
        ));
    }

    #[test]
    fn tab_drop_dispatch_exposes_controller_rejection() {
        let (bridge, command_rx) = Bridge::new();
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let mut result_rx =
            dispatch_with_ack_result(&bridge, move |ack| GtkCommand::ReorderSurface {
                pane,
                surface,
                target_index: 0,
                ack,
            })
            .expect("drop command should enter the bridge");

        assert!(matches!(
            result_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let command = command_rx.try_recv().expect("queued drop command");
        let GtkCommand::ReorderSurface { ack, .. } = command else {
            panic!("expected reorder command");
        };
        ack.send(Err("move rejected".to_string()))
            .expect("drop receiver should still be alive");

        assert_eq!(
            result_rx.try_recv().expect("controller response"),
            Err("move rejected".to_string())
        );
    }
}
