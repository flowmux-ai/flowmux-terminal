// SPDX-License-Identifier: GPL-3.0-or-later
//! Git worktree panel loading and workspace coordination.

use super::*;
use crate::ui::worktree_panel::WorktreeRowView;
use flowmux_vcs::worktree::{WorktreeInfo, WorktreeList, WorktreeListError};
use std::path::Path;

pub(super) fn same_existing_path(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn normalized_existing_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn repository_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn remove_block_reason(_info: &WorktreeInfo, workspace_open: bool) -> Option<String> {
    workspace_open.then(|| "Close the matching workspace before removal".into())
}

impl WindowController {
    pub(super) fn focus_worktree_panel(&self) {
        if self.worktrees.panel.widget().is_visible() {
            if let Some(pane) = self.focused_pane.get() {
                self.worktrees.source_pane.set(Some(pane));
            }
            self.file_browser.active.set(false);
            self.worktrees.active.set(true);
            self.worktrees.panel.grab_focus();
        }
    }

    pub(super) fn focus_out_of_worktree_panel(&self, dir: FocusDir) {
        self.worktrees.active.set(false);
        let Some(from) = self
            .worktrees
            .source_pane
            .get()
            .or_else(|| self.focused_pane.get())
        else {
            return;
        };

        if dir == FocusDir::Right && self.file_browser.panel.widget().is_visible() {
            self.file_browser.source_pane.set(Some(from));
            self.focus_file_browser();
            return;
        }

        if dir == FocusDir::Left {
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

    pub(super) async fn show_worktrees_for_pane(&self, pane: PaneId) {
        self.worktrees.source_pane.set(Some(pane));
        self.refresh_worktrees(true).await;
        self.position_right_tool_splits();
    }

    pub(super) async fn refresh_worktrees(&self, force: bool) {
        let Some(pane) = self
            .worktrees
            .source_pane
            .get()
            .or_else(|| self.focused_pane.get())
        else {
            return;
        };
        let Some(start) = self
            .file_browser_root_for_pane(pane)
            .await
            .or_else(|| std::env::current_dir().ok())
        else {
            self.worktrees
                .panel
                .show_error("Working directory is not available");
            return;
        };

        if !force {
            let start = normalized_existing_path(&start);
            let unchanged = self
                .worktrees
                .repository_root
                .borrow()
                .as_ref()
                .is_some_and(|current| start == *current || start.starts_with(current));
            if unchanged {
                return;
            }
        }

        let generation = self.worktrees.generation.get().wrapping_add(1);
        self.worktrees.generation.set(generation);
        self.worktrees.panel.show_loading();
        let bridge = self.bridge.clone();
        let Some(handle) = self.worktrees.tokio_handle.clone() else {
            self.worktrees
                .panel
                .show_error("Tokio runtime is not available");
            return;
        };
        handle.spawn(async move {
            let result = flowmux_vcs::worktree::list_worktrees(&start).await;
            let _ = bridge
                .tx
                .send(GtkCommand::WorktreesLoaded { generation, result })
                .await;
        });
    }

    pub(super) async fn apply_worktrees_loaded(
        &self,
        generation: u64,
        result: Result<WorktreeList, WorktreeListError>,
    ) {
        if !self.worktrees.panel.is_open() || generation != self.worktrees.generation.get() {
            return;
        }

        match result {
            Ok(list) => {
                let rows = self.annotate_worktree_rows(&list).await;
                let name = repository_name(&list.repository_root);
                *self.worktrees.repository_root.borrow_mut() =
                    Some(normalized_existing_path(&list.current_worktree));
                self.worktrees.panel.set_rows(&name, rows);
            }
            Err(WorktreeListError::NotRepository(_)) => {
                *self.worktrees.repository_root.borrow_mut() = None;
                self.worktrees.panel.show_not_repository();
            }
            Err(error) => {
                *self.worktrees.repository_root.borrow_mut() = None;
                self.worktrees.panel.show_error(&error.to_string());
            }
        }
    }

    async fn annotate_worktree_rows(&self, list: &WorktreeList) -> Vec<WorktreeRowView> {
        let snapshot = self.store.snapshot().await;
        list.items
            .iter()
            .cloned()
            .map(|info| {
                let matching = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| same_existing_path(&workspace.root_dir, &info.path));
                let workspace_open = matching.is_some();
                let workspace_active = matching
                    .is_some_and(|workspace| Some(workspace.id) == snapshot.active_workspace);
                let remove_block_reason = remove_block_reason(&info, workspace_open);
                WorktreeRowView {
                    info,
                    workspace_open,
                    workspace_active,
                    remove_block_reason,
                    operation_in_progress: false,
                }
            })
            .collect()
    }

    pub(super) async fn refresh_worktrees_from_focus(&self) {
        if !self.worktrees.panel.is_open() {
            return;
        }

        if let Some(pane) = self.focused_pane.get() {
            self.worktrees.source_pane.set(Some(pane));
            self.refresh_worktrees(false).await;
        }
    }

    pub(super) async fn open_worktree_workspace(&self, path: PathBuf) {
        if let Some(existing) = self
            .store
            .ordered_workspaces()
            .await
            .into_iter()
            .find(|workspace| same_existing_path(&workspace.root_dir, &path))
        {
            self.activate_workspace(existing.id).await;
        } else {
            let id = self.store.create_workspace(None, path.clone()).await;
            if let Some(workspace) = self.store.get_workspace(id).await {
                self.render_workspace_with_activation(&workspace, false);
            }
            self.activate_workspace(id).await;
            let store = self.store.clone();
            if let Some(handle) = self.worktrees.tokio_handle.clone() {
                handle.spawn(async move {
                    if let Ok(Some(info)) = flowmux_vcs::inspect(&path).await {
                        store.replace_git_info(id, Some(info)).await;
                    }
                });
            }
        }
        self.reannotate_visible_worktrees().await;
    }

    async fn reannotate_visible_worktrees(&self) {
        if self.worktrees.panel.is_open() {
            self.refresh_worktrees(true).await;
        }
    }
}
