// SPDX-License-Identifier: GPL-3.0-or-later
//! In-pane file browser: show, focus, state save/restore.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

fn resolve_editor_target(
    source_pane: Option<PaneId>,
    pane_mru: &[PaneId],
    workspace_leaves: &[PaneId],
) -> Option<PaneId> {
    source_pane
        .filter(|pane| workspace_leaves.contains(pane))
        .or_else(|| {
            pane_mru
                .iter()
                .find(|pane| workspace_leaves.contains(pane))
                .copied()
        })
        .or_else(|| workspace_leaves.first().copied())
}

impl WindowController {
    pub(super) async fn open_file_in_editor(&self, path: PathBuf, source_pane: Option<PaneId>) {
        match crate::ui::file_browser::file_open_target(&path) {
            crate::ui::file_browser::FileOpenTarget::Editor => {}
            crate::ui::file_browser::FileOpenTarget::ImageViewer => {
                crate::ui::image_viewer::open_image_viewer(&self.window, path);
                return;
            }
            crate::ui::file_browser::FileOpenTarget::Binary => {
                crate::ui::file_browser::open_binary(&path);
                return;
            }
        }

        let mut workspace = {
            let registry = self.pane_registry.borrow();
            source_pane
                .and_then(|pane| registry.workspace_of_pane(pane))
                .or_else(|| {
                    self.focused_pane
                        .get()
                        .and_then(|pane| registry.workspace_of_pane(pane))
                })
        };
        if workspace.is_none() {
            workspace = self.store.active_or_first().await;
        }
        let Some(workspace) = workspace else {
            self.clipboard_toast
                .show_with_message("No workspace is available for the editor");
            return;
        };
        let Some(workspace_state) = self.store.get_workspace(workspace).await else {
            self.clipboard_toast
                .show_with_message("The editor workspace is no longer available");
            return;
        };

        let mut leaves = Vec::new();
        for surface in &workspace_state.surfaces {
            surface.root_pane.for_each_leaf(|pane| leaves.push(pane));
        }
        let pane_mru = self
            .focus_mru
            .borrow()
            .get(&workspace)
            .map(|queue| queue.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let Some(target_pane) = resolve_editor_target(source_pane, &pane_mru, &leaves) else {
            self.clipboard_toast
                .show_with_message("The workspace has no pane for the editor");
            return;
        };

        let existing_surface = self
            .pane_registry
            .borrow()
            .editor_surface_in_pane(target_pane);
        let editor_surface = if let Some(surface) = existing_surface {
            surface
        } else {
            let Some((workspace_id, surface)) =
                self.store.add_editor_surface_to_pane(target_pane).await
            else {
                self.clipboard_toast
                    .show_with_message("Could not create an editor tab");
                return;
            };
            self.attach_or_rerender_surface(workspace_id, target_pane, surface)
                .await;
            surface
        };

        self.store
            .set_active_surface(target_pane, editor_surface)
            .await;
        self.pane_registry
            .borrow_mut()
            .activate_surface(target_pane, editor_surface);
        let open_result = self
            .pane_registry
            .borrow()
            .editors
            .get(&editor_surface)
            .ok_or_else(|| "the editor view is not ready".to_string())
            .and_then(|editor| editor.open_file(&path));
        if let Err(error) = open_result {
            tracing::warn!(path = %path.display(), %error, "failed to open file in editor");
            self.clipboard_toast
                .show_with_message(&format!("Could not open file: {error}"));
        } else if let Some(title) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
        {
            if self
                .store
                .update_surface_auto_title(target_pane, editor_surface, title.to_string())
                .await
                .is_some()
            {
                if let Some(title) = self.store.surface_title(target_pane, editor_surface).await {
                    self.pane_registry
                        .borrow()
                        .set_surface_title(editor_surface, &title);
                }
            }
        }
        self.refresh_window_title().await;
        self.focus_pane(target_pane);
    }

    /// Single entry point for recomputing workspace label and subtitles.
    ///
    /// Design:
    ///   * Side-panel main label = active surface title from MRU[0], the most
    ///     recently focused pane. Use the original OSC title when present;
    ///     otherwise use the cwd folder name at full length, without truncation.
    ///   * Subtitles = active terminal cwd for MRU[0..3], shortened to the last
    ///     3 folders with a "..." prefix. Focus moves naturally update MRU and
    ///     therefore the 3 subtitle lines.
    ///   * If `custom_title` is locked, the user label takes display priority
    ///     and only ws.name, the automatic value, is updated in the background.
    ///
    /// Updates both store and side panel. The daemon setter is idempotent so
    /// repeated calls for the same ws_id, such as cwd polling, only mark disk
    /// dirty and rebuild GTK when values actually change.
    pub(super) async fn file_browser_root_for_pane(
        &self,
        pane: PaneId,
    ) -> Option<std::path::PathBuf> {
        if let Some(dir) = self.pane_registry.borrow().current_dir_for_pane(pane) {
            return Some(dir);
        }

        let ws_id = self.pane_registry.borrow().workspace_of_pane(pane)?;
        let active_surface = self.pane_registry.borrow().active_surface(pane);
        let ws = self.store.get_workspace(ws_id).await?;

        if let Some(surface) = active_surface {
            if let Some(surface) = ws
                .surfaces
                .iter()
                .find_map(|surface_root| surface_root.root_pane.find_surface(pane, surface))
            {
                if let SurfaceKind::Terminal { cwd: Some(cwd), .. } = surface.kind {
                    return Some(cwd);
                }
            }
        }

        Some(ws.root_dir)
    }
    pub(super) async fn show_file_browser_for_pane(&self, pane: PaneId) {
        let root = self
            .file_browser_root_for_pane(pane)
            .await
            .or_else(|| std::env::current_dir().ok());

        if let Some(root) = root {
            if self.file_browser.panel.is_open()
                && self.file_browser.source_pane.get() == Some(pane)
                && self.file_browser.panel.is_showing_root(&root)
            {
                // Opening / refreshing the panel does not move keyboard focus into
                // it — file_browser_active is driven by connect_focus_changed.
                //
                // The is_open() guard matters: close_file_browser_and_restore_focus
                // hides the panel but leaves file_browser_source_pane and the model
                // root intact, so without it a reopen for the same pane would match
                // this short-circuit and never call set_visible(true) — leaving the
                // toggle permanently stuck closed.
                return;
            }

            self.save_file_browser_state_for_source();
            self.file_browser.source_pane.set(Some(pane));
            let state = self.file_browser.pane_states.borrow().get(&pane).cloned();
            self.file_browser
                .panel
                .show_for_root_with_state(root, state);
            self.position_right_tool_splits();
        }
    }
    pub(super) fn position_right_tool_splits(&self) {
        let window_width = self.window.width().max(640);
        let content_width = (window_width - self.sidebar_split.position()).max(320);
        let files_width = if self.file_browser.panel.widget().is_visible() {
            320
        } else {
            0
        };
        if files_width > 0 {
            self.file_browser
                .split
                .set_position((content_width - files_width).max(240));
        }
        if self.worktrees.panel.widget().is_visible() {
            let worktree_container_width = content_width - files_width;
            self.worktrees
                .split
                .set_position((worktree_container_width - 340).max(240));
        }
    }
    pub(super) fn save_file_browser_state_for_source(&self) {
        let Some(pane) = self.file_browser.source_pane.get() else {
            return;
        };
        self.file_browser
            .pane_states
            .borrow_mut()
            .insert(pane, self.file_browser.panel.pane_state());
    }
    pub(super) fn focus_file_browser(&self) {
        if self.file_browser.panel.widget().is_visible() {
            if let Some(pane) = self.focused_pane.get() {
                self.file_browser.source_pane.set(Some(pane));
            }
            self.worktrees.active.set(false);
            self.file_browser.active.set(true);
            #[cfg(target_os = "macos")]
            if let Some(pane) = self.focused_pane.get() {
                if let Some(editor) = self.pane_registry.borrow().active_editor(pane) {
                    editor.resign_native_focus();
                }
            }
            self.file_browser.panel.grab_focus();
        }
    }
    pub(super) fn focus_out_of_file_browser(&self, dir: FocusDir) {
        self.save_file_browser_state_for_source();
        self.file_browser.active.set(false);
        let Some(from) =
            file_browser_return_pane(self.focused_pane.get(), self.file_browser.source_pane.get())
        else {
            return;
        };

        // The browser is docked immediately to the right of its source pane, so
        // Alt+Left should land on that adjacent pane — not skip past it to the
        // source's own left neighbour. (You always pass through the adjacent pane
        // to reach the browser, so the source pane is the one touching it.) Other
        // directions still move relative to the source pane.
        if dir == FocusDir::Left {
            if self.worktrees.panel.widget().is_visible() {
                self.worktrees.source_pane.set(Some(from));
                self.focus_worktree_panel();
                return;
            }
            self.focused_pane.set(Some(from));
            self.focus_pane(from);
            return;
        }

        let before = self.focused_pane.get();
        let moved = self.focus_in_direction(from, dir).is_some();
        if !moved || self.focused_pane.get() == before || self.focused_pane.get().is_none() {
            self.focused_pane.set(Some(from));
            self.focus_pane(from);
        }
    }
    pub(super) fn close_file_browser_and_restore_focus(&self) {
        self.save_file_browser_state_for_source();
        self.file_browser.active.set(false);
        self.file_browser.panel.hide();
        if let Some(pane) =
            file_browser_return_pane(self.focused_pane.get(), self.file_browser.source_pane.get())
        {
            self.focused_pane.set(Some(pane));
            self.focus_pane(pane);
        }
    }
    pub(super) fn focus_direction_or_right_tools(&self, from: PaneId, dir: FocusDir) {
        let moved = self.focus_in_direction(from, dir).is_some();
        if dir != FocusDir::Right || moved {
            return;
        }
        if self.worktrees.panel.widget().is_visible() {
            self.worktrees.source_pane.set(Some(from));
            self.focus_worktree_panel();
        } else if self.file_browser.panel.widget().is_visible() {
            self.file_browser.source_pane.set(Some(from));
            self.focus_file_browser();
        }
    }
    pub(super) async fn refresh_file_browser_from_focus(&self) {
        if !self.file_browser.panel.widget().is_visible() {
            return;
        }

        if let Some(pane) = self.focused_pane.get() {
            self.show_file_browser_for_pane(pane).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_target_prefers_live_source_then_mru_then_first_leaf() {
        let first = PaneId::new();
        let second = PaneId::new();
        let stale = PaneId::new();
        let leaves = [first, second];

        assert_eq!(
            resolve_editor_target(Some(second), &[first], &leaves),
            Some(second)
        );
        assert_eq!(
            resolve_editor_target(Some(stale), &[stale, second], &leaves),
            Some(second)
        );
        assert_eq!(
            resolve_editor_target(Some(stale), &[stale], &leaves),
            Some(first)
        );
        assert_eq!(resolve_editor_target(None, &[], &[]), None);
    }
}
