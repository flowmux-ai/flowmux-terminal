// SPDX-License-Identifier: GPL-3.0-or-later
//! File browser and window chrome command dispatch on the GTK main thread.

use super::*;

impl WindowController {
    pub(super) async fn dispatch_window_chrome_command(&self, cmd: GtkCommand) {
        match cmd {
            GtkCommand::ShowOptionsDialog => {
                let current = self.options.borrow().clone();
                let options_cell = self.options.clone();
                let window = self.window.clone();
                let controller = self.clone();
                let preview_controller = self.clone();
                let theme = self.current_theme();
                let default_font_family = theme.font_family();
                let default_font_size = theme.font_size();
                let update_banner = self.workspace_presenter.sidebar.update_banner();
                let update_check_banner = update_banner.clone();
                let install_origin = update_banner.install_origin();
                let update_state = update_banner.state();
                crate::ui::options_dialog::present(
                    &self.window,
                    current,
                    default_font_family,
                    default_font_size,
                    move |opts| {
                        if let Err(e) = flowmux_config::options::save(&opts) {
                            tracing::warn!(error = %e, "options save failed");
                            return;
                        }
                        *options_cell.borrow_mut() = opts.clone();
                        // Re-resolves the theme (preset + overrides), repaints
                        // every terminal, reapplies the effective font, and
                        // reloads the CSS provider.
                        controller.apply_runtime_theme(&opts);
                        let registry = controller.pane_registry.borrow();
                        for terminal in registry.terminals.values() {
                            terminal.set_font_scale(opts.zoom_factor());
                            terminal
                                .set_cursor_blink(opts.cursor_blink, opts.cursor_blink_interval_ms);
                        }
                        for browser in registry.browsers.values() {
                            browser.set_zoom_level(opts.zoom_factor());
                        }
                        drop(registry);
                        // Re-install keybindings so the user does not have to
                        // restart for shortcut edits to take effect.
                        // set_accels_for_action overwrites the same keys so a
                        // second pass on the live ApplicationWindow's app is safe.
                        if let Some(app) = window
                            .application()
                            .and_then(|a| a.downcast::<adw::Application>().ok())
                        {
                            crate::keybindings::install_accels(&app, &opts);
                        } else {
                            tracing::warn!(
                            "options applied without keybinding re-install — window had no Application; restart to pick up shortcut changes"
                        );
                        }
                        tracing::info!(
                            zoom_percent = opts.zoom_percent,
                            engine = ?opts.default_browser_engine,
                            focus_border_color = %opts.focus_border_color,
                            focus_border_opacity = opts.focus_border_opacity,
                            agent_bar_enabled = opts.agent_bar_enabled,
                            keybindings_overrides = opts.keybindings.len(),
                            "options applied"
                        );
                        let controller = controller.clone();
                        glib::MainContext::default().spawn_local(async move {
                            controller.refresh_agent_bar().await;
                        });
                    },
                    // Live preview from the Theme tab; also called on Cancel /
                    // close with the original options to restore the look.
                    move |opts| {
                        preview_controller.apply_runtime_theme(opts);
                    },
                    install_origin,
                    update_state,
                    move |on_complete| update_check_banner.check_now(on_complete),
                    move |version| update_banner.start_install(version),
                );
            }
            GtkCommand::ShowCommandPalette => {
                self.show_command_palette().await;
            }

            GtkCommand::FileBrowserFocusOut { dir } => {
                self.focus_out_of_file_browser(dir);
            }
            GtkCommand::FileBrowserCloseAndRestoreFocus => {
                self.close_file_browser_and_restore_focus();
            }
            GtkCommand::ToggleWorktreePanel { pane } => {
                if self.worktrees.panel.widget().is_visible() {
                    self.close_worktree_panel_and_restore_focus();
                } else if let Some(pane) = pane.or_else(|| self.focused_pane.get()) {
                    self.show_worktrees_for_pane(pane).await;
                }
            }
            GtkCommand::RefreshWorktrees => {
                self.refresh_worktrees(true).await;
            }
            GtkCommand::WorktreesLoaded { generation, result } => {
                self.apply_worktrees_loaded(generation, result).await;
            }
            GtkCommand::OpenWorktree { path } => {
                self.open_worktree_workspace(path).await;
            }
            GtkCommand::ShowWorktreeInfo { path } => {
                self.show_worktree_info(path);
            }
            GtkCommand::RemoveWorktree { path } => {
                self.request_worktree_removal(path).await;
            }
            GtkCommand::WorktreeRemovalFinished {
                path,
                force,
                result,
            } => {
                self.finish_worktree_removal(path, force, result).await;
            }
            GtkCommand::WorktreePanelFocusOut { dir } => {
                self.focus_out_of_worktree_panel(dir);
            }
            GtkCommand::WorktreePanelCloseAndRestoreFocus => {
                self.close_worktree_panel_and_restore_focus();
            }
            GtkCommand::ToggleFileBrowser { pane } => {
                // `None` comes from the side-panel footer button / Ctrl+Alt+F,
                // which have no pane context — target the focused pane.
                // Already visible → close; otherwise open for that pane.
                if self.file_browser.panel.widget().is_visible() {
                    self.close_file_browser_and_restore_focus();
                } else if let Some(pane) = pane.or_else(|| self.focused_pane.get()) {
                    self.show_file_browser_for_pane(pane).await;
                }
            }
            GtkCommand::OpenImageViewer { pane, path } => {
                tracing::info!(%pane, path = %path.display(), "opening terminal image path");
                crate::ui::image_viewer::open_image_viewer(&self.window, path);
            }
            GtkCommand::OpenMarkdownViewer { pane, path } => {
                tracing::info!(%pane, path = %path.display(), "opening terminal markdown path");
                if let Err(err) = crate::ui::file_browser::launch_markdown_viewer(&path) {
                    tracing::warn!(error = %err, path = %path.display(), "failed to open Markdown viewer from terminal path");
                }
            }
            GtkCommand::ShowSurfaceFolder { pane, surface } => {
                let cwd = self
                    .pane_registry
                    .borrow()
                    .terminals
                    .get(&surface)
                    .and_then(|t| t.current_dir())
                    .or_else(|| {
                        self.pane_registry
                            .borrow()
                            .editors
                            .get(&surface)
                            .map(|editor| editor.workspace_root().to_path_buf())
                    });
                let workspace_for_pane = self.pane_registry.borrow().workspace_of_pane(pane);
                let stored = match workspace_for_pane {
                    Some(workspace) => self
                        .store
                        .get_workspace(workspace)
                        .await
                        .and_then(|ws| stored_terminal_cwd_from_workspace(&ws, pane, surface)),
                    None => None,
                };
                match cwd.or(stored) {
                    Some(p) => crate::ui::show_in_folder::open_directory(&p),
                    None => {
                        tracing::info!(%pane, %surface, "show-in-folder: surface has no resolvable cwd");
                    }
                }
            }
            GtkCommand::CopySurfaceText { pane, surface } => {
                let text = self.copyable_surface_text(pane, surface).await;
                match text {
                    Some(text) => {
                        self.window.clipboard().set_text(&text.value);
                        self.clipboard_toast
                            .show_with_message(&format!("Copied {}: {}", text.kind, text.value));
                    }
                    None => {
                        tracing::info!(%pane, %surface, "copy-surface-text: no path/url");
                        self.clipboard_toast.show_with_message("Nothing to copy");
                    }
                }
            }
            GtkCommand::CopyFocusedPaneText { workspace } => {
                flowmux_config::notify_debug!("gui/dispatch", "CopyFocusedPaneText ws={workspace}");
                let ws = self.store.get_workspace(workspace).await;
                let Some(ws) = ws else {
                    tracing::info!(%workspace, "copy-focused-pane-text: workspace not found");
                    return;
                };
                let focused = self.focused_pane.get().filter(|p| {
                    self.pane_registry.borrow().workspace_of_pane(*p) == Some(workspace)
                });
                let candidate_panes: Vec<PaneId> = focused
                    .into_iter()
                    .chain(
                        ws.surfaces
                            .first()
                            .and_then(|s| s.root_pane.first_leaf_id()),
                    )
                    .collect();
                let mut resolved = None;
                for pane in candidate_panes {
                    let surface = self.pane_registry.borrow().active_surface(pane);
                    let Some(surface) = surface else {
                        continue;
                    };
                    if let Some(text) = self.copyable_surface_text(pane, surface).await {
                        resolved = Some(text);
                        break;
                    }
                }
                let text = resolved.unwrap_or_else(|| CopyableText::stored_path(ws.root_dir));
                self.window.clipboard().set_text(&text.value);
                self.clipboard_toast
                    .show_with_message(&format!("Copied {}: {}", text.kind, text.value));
            }
            GtkCommand::ShowFocusedPaneFolder { workspace } => {
                flowmux_config::notify_debug!(
                    "gui/dispatch",
                    "ShowFocusedPaneFolder ws={workspace}"
                );
                // Resolution order:
                //   1. Globally focused pane, if it belongs to this workspace —
                //      its active terminal's cwd.
                //   2. Workspace's first leaf pane — its active terminal's cwd.
                //   3. Workspace's stored `root_dir` — guarantees we open
                //      *something* for a workspace whose panes are all browsers
                //      or whose terminals haven't reported a cwd yet.
                let ws = self.store.get_workspace(workspace).await;
                let Some(ws) = ws else {
                    tracing::info!(%workspace, "show-in-folder: workspace not found");
                    return;
                };
                let focused = self.focused_pane.get().filter(|p| {
                    self.pane_registry.borrow().workspace_of_pane(*p) == Some(workspace)
                });
                let candidate_panes: Vec<PaneId> = focused
                    .into_iter()
                    .chain(
                        ws.surfaces
                            .first()
                            .and_then(|s| s.root_pane.first_leaf_id()),
                    )
                    .collect();
                let path = {
                    let r = self.pane_registry.borrow();
                    candidate_panes.iter().find_map(|pane| {
                        let surface = r
                            .active_surface(*pane)
                            .or_else(|| active_surface_from_workspace(&ws, *pane))?;
                        r.active_terminal(*pane)
                            .and_then(|t| t.current_dir())
                            .or_else(|| stored_terminal_cwd_from_workspace(&ws, *pane, surface))
                    })
                }
                .unwrap_or_else(|| ws.root_dir.clone());
                crate::ui::show_in_folder::open_directory(&path);
            }
            other => unreachable!("non-chrome command routed to window dispatcher: {other:?}"),
        }
    }

    fn close_worktree_panel_and_restore_focus(&self) {
        self.worktrees
            .generation
            .set(self.worktrees.generation.get().wrapping_add(1));
        self.worktrees.loading.set(false);
        self.worktrees.active.set(false);
        self.worktrees.panel.hide();
        if let Some(pane) = self
            .worktrees
            .source_pane
            .get()
            .or_else(|| self.focused_pane.get())
        {
            self.focused_pane.set(Some(pane));
            self.focus_pane(pane);
        }
    }
}
