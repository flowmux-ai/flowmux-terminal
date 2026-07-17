// SPDX-License-Identifier: GPL-3.0-or-later
//! Git worktree panel loading and workspace coordination.

use super::*;
use crate::ui::worktree_panel::WorktreeRowView;
use chrono::{DateTime, Local, Utc};
use flowmux_vcs::worktree::{RemoveWorktreeError, WorktreeInfo, WorktreeList, WorktreeListError};
use std::path::Path;

/// Upper bound on one worktree listing (git worktree list + per-tree
/// status/commit reads). Generous for large checkouts; only a wedged
/// git or a bug should ever reach it.
const WORKTREE_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) fn same_existing_path(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = normalized_existing_path(path);
    let root = normalized_existing_path(root);
    path == root || path.starts_with(root)
}

fn normalized_existing_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_source_directory(previous: Option<&Path>, current: &Path) -> bool {
    previous.is_some_and(|previous| previous == current)
}

/// What an arriving worktree-list result does to the panel state.
///
/// The `loading` flag must reach `false` for every result of the
/// current generation — even when the panel was closed while the list
/// was in flight. Leaving it set makes every later same-directory
/// [`refresh_worktrees`](WindowController::refresh_worktrees) return
/// early, freezing the panel on "Loading worktrees…" until restart.
#[derive(Debug, PartialEq, Eq)]
enum WorktreesResultDisposition {
    /// A newer refresh owns the flag; drop this result untouched.
    IgnoreStale,
    /// Current generation, panel closed: end the load, render nothing.
    ClearLoadingOnly,
    /// Current generation, panel open: end the load and render.
    Apply,
}

fn worktrees_result_disposition(
    generation: u64,
    current_generation: u64,
    panel_open: bool,
) -> WorktreesResultDisposition {
    if generation != current_generation {
        WorktreesResultDisposition::IgnoreStale
    } else if !panel_open {
        WorktreesResultDisposition::ClearLoadingOnly
    } else {
        WorktreesResultDisposition::Apply
    }
}

fn repository_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn pane_uses_worktree(pane: &Pane, path: &Path) -> bool {
    match pane {
        Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } => surfaces.iter().any(|surface| {
            matches!(
                &surface.kind,
                SurfaceKind::Terminal { cwd: Some(cwd), .. } if path_is_within(cwd, path)
            )
        }),
        Pane::Leaf { .. } => false,
        Pane::Split { first, second, .. } => {
            pane_uses_worktree(first, path) || pane_uses_worktree(second, path)
        }
    }
}

fn workspace_uses_worktree(workspace: &Workspace, path: &Path) -> bool {
    path_is_within(&workspace.root_dir, path)
        || workspace.surfaces.iter().any(|surface| {
            matches!(
                &surface.kind,
                SurfaceKind::Terminal { cwd: Some(cwd), .. } if path_is_within(cwd, path)
            ) || pane_uses_worktree(&surface.root_pane, path)
        })
}

fn remove_block_reason(info: &WorktreeInfo, worktree_in_use: bool) -> Option<String> {
    if info.is_main {
        Some("Main worktree cannot be removed".into())
    } else if info.is_current {
        Some("Current worktree cannot be removed".into())
    } else if info.is_bare {
        Some("Bare repository cannot be removed as a worktree".into())
    } else if let Some(reason) = &info.lock_reason {
        Some(format!(
            "Locked worktree: {}",
            if reason.is_empty() {
                "no reason provided"
            } else {
                reason
            }
        ))
    } else if worktree_in_use {
        Some("Close tabs and workspaces using this worktree before removal".into())
    } else {
        None
    }
}

fn confirmation_receiver(
    dialog: &adw::AlertDialog,
    accepted_response: &'static str,
) -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    let tx = Rc::new(Cell::new(Some(tx)));
    dialog.connect_response(None, move |dialog, response| {
        if let Some(tx) = tx.take() {
            let _ = tx.send(response == accepted_response);
        }
        dialog.close();
    });
    rx
}

fn build_remove_confirmation(_path: &Path) -> (adw::AlertDialog, oneshot::Receiver<bool>) {
    let dialog = adw::AlertDialog::new(
        Some("Remove worktree?"),
        Some("The checkout directory will be removed. Its Git branch will be kept."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Remove");
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    let rx = confirmation_receiver(&dialog, "remove");
    (dialog, rx)
}

fn build_force_remove_confirmation(
    _path: &Path,
    reason: &str,
) -> (adw::AlertDialog, oneshot::Receiver<bool>) {
    let reason: String = reason.chars().take(2_000).collect();
    let body = format!(
        "{reason}\n\nTracked and untracked changes in this checkout will be lost. The branch will be kept."
    );
    let dialog = adw::AlertDialog::new(Some("Force remove dirty worktree?"), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("force", "Force Remove");
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("force", adw::ResponseAppearance::Destructive);
    let rx = confirmation_receiver(&dialog, "force");
    (dialog, rx)
}

fn worktree_info_heading(info: &WorktreeInfo) -> String {
    info.branch.clone().unwrap_or_else(|| {
        let short_head: String = info.head.chars().take(10).collect();
        format!("Detached at {short_head}")
    })
}

fn worktree_info_body(row: &WorktreeRowView) -> String {
    let head = if row.info.head.is_empty() {
        "Unavailable"
    } else {
        row.info.head.as_str()
    };
    let commit = row.info.commit_subject.as_deref().unwrap_or("Unavailable");
    let committed = row
        .info
        .commit_time
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|| "Unavailable".into());
    let changes = match &row.info.changes {
        Some(changes) if changes.is_clean() => "Clean".into(),
        Some(changes) => format!(
            "staged {}, unstaged {}, untracked {}",
            changes.staged, changes.unstaged, changes.untracked
        ),
        None => "Unavailable".into(),
    };
    let locked = row.info.lock_reason.as_deref().unwrap_or("No");
    let prunable = row.info.prunable_reason.as_deref().unwrap_or("No");
    let workspace = if row.workspace_active {
        "Active"
    } else if row.workspace_open {
        "Open"
    } else {
        "Not open"
    };

    format!(
        "Path: {}\nHEAD: {head}\nCommit: {commit}\nCommitted: {committed}\nChanges: {changes}\nLocked: {locked}\nPrunable: {prunable}\nWorkspace: {workspace}",
        row.info.path.display()
    )
}

impl WindowController {
    fn worktree_in_use(&self, path: &Path, workspaces: &[Workspace]) -> bool {
        workspaces
            .iter()
            .any(|workspace| workspace_uses_worktree(workspace, path))
            || self
                .pane_registry
                .borrow()
                .terminals
                .values()
                .filter_map(|terminal| terminal.current_dir())
                .any(|cwd| path_is_within(&cwd, path))
    }

    fn show_worktree_alert(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("close", "Close");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));
    }

    pub(super) fn show_worktree_info(&self, path: PathBuf) {
        let path = normalized_existing_path(&path);
        let Some(row) = self.worktrees.panel.row_for_path(&path) else {
            self.show_worktree_alert(
                "Unable to show worktree information",
                "The worktree is no longer in the current list.",
            );
            return;
        };
        let dialog = adw::AlertDialog::new(
            Some(&worktree_info_heading(&row.info)),
            Some(&worktree_info_body(&row)),
        );
        dialog.add_response("close", "Close");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));
    }

    async fn removable_worktree_row(&self, path: &Path) -> Result<WorktreeRowView, String> {
        let path = normalized_existing_path(path);
        let row = self
            .worktrees
            .panel
            .row_for_path(&path)
            .ok_or_else(|| "The worktree is no longer in the current list.".to_string())?;
        let snapshot = self.store.snapshot().await;
        if let Some(reason) = remove_block_reason(
            &row.info,
            self.worktree_in_use(&row.info.path, &snapshot.workspaces),
        ) {
            return Err(reason);
        }
        Ok(row)
    }

    async fn present_worktree_confirmation(
        &self,
        dialog: adw::AlertDialog,
        response: oneshot::Receiver<bool>,
    ) -> bool {
        let _native_browser_suspend =
            crate::ui::browser_pane::suspend_native_browser_views_for_window(
                self.window.upcast_ref(),
            );
        dialog.present(Some(&self.window));
        response.await.unwrap_or(false)
    }

    pub(super) async fn request_worktree_removal(&self, path: PathBuf) {
        let path = normalized_existing_path(&path);
        let row = match self.removable_worktree_row(&path).await {
            Ok(row) => row,
            Err(reason) => {
                self.show_worktree_alert("Cannot remove worktree", &reason);
                return;
            }
        };
        let (dialog, response) = build_remove_confirmation(&row.info.path);
        if self.present_worktree_confirmation(dialog, response).await {
            self.start_worktree_removal(row.info.path, false);
        }
    }

    fn start_worktree_removal(&self, path: PathBuf, force: bool) {
        let Some(repository_root) = self.worktrees.repository_root.borrow().clone() else {
            self.show_worktree_alert(
                "Unable to remove worktree",
                "The repository is no longer available.",
            );
            return;
        };
        let Some(handle) = self.worktrees.tokio_handle.clone() else {
            self.show_worktree_alert(
                "Unable to remove worktree",
                "Tokio runtime is not available.",
            );
            return;
        };
        if !self.worktrees.panel.set_operation_in_progress(&path, true) {
            self.show_worktree_alert(
                "Unable to remove worktree",
                "The worktree is no longer in the current list.",
            );
            return;
        }
        self.worktrees
            .removals_in_progress
            .borrow_mut()
            .insert(path.clone());

        let bridge = self.bridge.clone();
        handle.spawn(async move {
            let result =
                flowmux_vcs::worktree::remove_worktree(&repository_root, &path, force).await;
            let _ = bridge
                .tx
                .send(GtkCommand::WorktreeRemovalFinished {
                    path,
                    force,
                    result,
                })
                .await;
        });
    }

    pub(super) async fn finish_worktree_removal(
        &self,
        path: PathBuf,
        force: bool,
        result: Result<(), RemoveWorktreeError>,
    ) {
        self.worktrees
            .removals_in_progress
            .borrow_mut()
            .remove(&normalized_existing_path(&path));
        let still_represented = self.worktrees.panel.is_open()
            && self.worktrees.panel.is_showing_rows()
            && self.worktrees.panel.row_for_path(&path).is_some();
        self.worktrees.panel.set_operation_in_progress(&path, false);

        match result {
            Ok(()) => {
                if still_represented {
                    self.refresh_worktrees(true).await;
                }
            }
            Err(RemoveWorktreeError::RequiresForce(reason)) if !force && still_represented => {
                let (dialog, response) = build_force_remove_confirmation(&path, &reason);
                if self.present_worktree_confirmation(dialog, response).await {
                    match self.removable_worktree_row(&path).await {
                        Ok(row) => self.start_worktree_removal(row.info.path, true),
                        Err(reason) => self.show_worktree_alert("Cannot remove worktree", &reason),
                    }
                }
            }
            Err(error) if still_represented => {
                self.show_worktree_alert("Unable to remove worktree", &error.to_string());
            }
            Err(_) => {}
        }
    }

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

        let start = normalized_existing_path(&start);
        let same_source =
            same_source_directory(self.worktrees.source_directory.borrow().as_deref(), &start);
        tracing::debug!(
            start = %start.display(),
            same_source,
            loading = self.worktrees.loading.get(),
            force,
            "worktrees: refresh requested"
        );
        if same_source && (self.worktrees.loading.get() || !force) {
            tracing::debug!("worktrees: refresh skipped (same source, loading or not forced)");
            return;
        }
        *self.worktrees.source_directory.borrow_mut() = Some(start.clone());
        self.worktrees.loading.set(true);

        let generation = self.worktrees.generation.get().wrapping_add(1);
        self.worktrees.generation.set(generation);
        self.worktrees.panel.show_loading();
        let bridge = self.bridge.clone();
        let Some(handle) = self.worktrees.tokio_handle.clone() else {
            self.worktrees.loading.set(false);
            self.worktrees
                .panel
                .show_error("Tokio runtime is not available");
            return;
        };
        handle.spawn(async move {
            // Backstop, mirroring the usage popover fix: a result must
            // reach the panel no matter what happens to the listing
            // (wedged git, panic), or `loading` never clears and the
            // panel freezes on "Loading worktrees…" until restart.
            let mut list =
                tokio::spawn(async move { flowmux_vcs::worktree::list_worktrees(&start).await });
            let result = match tokio::time::timeout(WORKTREE_LIST_TIMEOUT, &mut list).await {
                Ok(Ok(result)) => result,
                Ok(Err(join_error)) => Err(WorktreeListError::CommandFailed(format!(
                    "worktree listing crashed: {join_error}"
                ))),
                Err(_) => {
                    list.abort();
                    Err(WorktreeListError::CommandFailed(format!(
                        "worktree listing timed out after {}s",
                        WORKTREE_LIST_TIMEOUT.as_secs()
                    )))
                }
            };
            tracing::debug!(
                generation,
                ok = result.is_ok(),
                "worktrees: listing finished, delivering"
            );
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
        let disposition = worktrees_result_disposition(
            generation,
            self.worktrees.generation.get(),
            self.worktrees.panel.is_open(),
        );
        tracing::debug!(generation, ?disposition, "worktrees: result arrived");
        match disposition {
            WorktreesResultDisposition::IgnoreStale => return,
            WorktreesResultDisposition::ClearLoadingOnly => {
                self.worktrees.loading.set(false);
                return;
            }
            WorktreesResultDisposition::Apply => {}
        }
        self.worktrees.loading.set(false);

        match result {
            Ok(list) => {
                let rows = self.annotate_worktree_rows(&list).await;
                let name = repository_name(&list.repository_root);
                tracing::debug!(rows = rows.len(), repository = %name, "worktrees: rendering rows");
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
        let removals_in_progress = self.worktrees.removals_in_progress.borrow().clone();
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
                let remove_block_reason = remove_block_reason(
                    &info,
                    self.worktree_in_use(&info.path, &snapshot.workspaces),
                );
                let operation_in_progress =
                    removals_in_progress.contains(&normalized_existing_path(&info.path));
                WorktreeRowView {
                    info,
                    workspace_open,
                    workspace_active,
                    remove_block_reason,
                    operation_in_progress,
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
            let bridge = self.bridge.clone();
            if let Some(handle) = self.worktrees.tokio_handle.clone() {
                handle.spawn(async move {
                    if let Ok(Some(info)) = flowmux_vcs::inspect(&path).await {
                        let _ = bridge
                            .tx
                            .send(GtkCommand::WorkspaceGitInfoLoaded {
                                workspace: id,
                                info,
                            })
                            .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_vcs::worktree::WorktreeChanges;

    fn sample_worktree_info(path: &str) -> WorktreeInfo {
        WorktreeInfo {
            path: path.into(),
            branch: Some("feature".into()),
            head: "1234567890abcdef".into(),
            commit_subject: Some("sample commit".into()),
            commit_time: Some(1_700_000_000),
            changes: Some(WorktreeChanges::default()),
            is_main: false,
            is_current: false,
            is_bare: false,
            lock_reason: None,
            prunable_reason: None,
        }
    }

    #[test]
    fn removal_gate_explains_every_blocked_state() {
        let mut item = sample_worktree_info("/repo/main");
        item.is_current = true;
        assert_eq!(
            remove_block_reason(&item, false).as_deref(),
            Some("Current worktree cannot be removed")
        );

        item.is_current = false;
        item.is_main = true;
        assert_eq!(
            remove_block_reason(&item, false).as_deref(),
            Some("Main worktree cannot be removed")
        );

        item.is_main = false;
        item.is_bare = true;
        assert_eq!(
            remove_block_reason(&item, false).as_deref(),
            Some("Bare repository cannot be removed as a worktree")
        );

        item.is_bare = false;
        item.lock_reason = Some("in use".into());
        assert_eq!(
            remove_block_reason(&item, false).as_deref(),
            Some("Locked worktree: in use")
        );

        item.lock_reason = None;
        assert_eq!(
            remove_block_reason(&item, true).as_deref(),
            Some("Close tabs and workspaces using this worktree before removal")
        );
    }

    #[test]
    fn worktree_usage_includes_workspace_roots_and_terminal_cwds_below_it() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("project/subdir");
        std::fs::create_dir_all(&nested).unwrap();
        let workspace = Workspace {
            id: WorkspaceId::new(),
            name: "nested".into(),
            custom_title: None,
            root_dir: nested.clone(),
            git: None,
            listening_ports: Vec::new(),
            surfaces: Vec::new(),
            color: None,
        };
        let pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::tabbed_terminal("shell", Some(nested)),
        };

        assert!(workspace_uses_worktree(&workspace, root.path()));
        assert!(pane_uses_worktree(&pane, root.path()));
    }

    #[test]
    fn stale_result_leaves_loading_to_the_newer_refresh() {
        assert_eq!(
            worktrees_result_disposition(3, 4, true),
            WorktreesResultDisposition::IgnoreStale
        );
        assert_eq!(
            worktrees_result_disposition(3, 4, false),
            WorktreesResultDisposition::IgnoreStale
        );
    }

    #[test]
    fn result_arriving_while_panel_closed_still_ends_the_load() {
        // Regression: this case used to drop the result without
        // clearing `loading`, freezing the panel on "Loading
        // worktrees…" for the rest of the session.
        assert_eq!(
            worktrees_result_disposition(4, 4, false),
            WorktreesResultDisposition::ClearLoadingOnly
        );
    }

    #[test]
    fn current_result_with_open_panel_is_applied() {
        assert_eq!(
            worktrees_result_disposition(4, 4, true),
            WorktreesResultDisposition::Apply
        );
    }

    #[test]
    fn nested_worktree_is_not_the_same_source_directory() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join(".worktrees/feature");
        std::fs::create_dir_all(&nested).unwrap();

        assert!(same_source_directory(Some(root.path()), root.path()));
        assert!(!same_source_directory(Some(root.path()), &nested));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn safe_remove_defaults_to_cancel_and_force_is_destructive() {
        let (safe, safe_rx) = build_remove_confirmation(Path::new("/repo/feature"));
        assert_eq!(safe.default_response().as_deref(), Some("cancel"));
        assert_eq!(safe.close_response().as_str(), "cancel");
        assert_eq!(
            safe.response_appearance("remove"),
            adw::ResponseAppearance::Destructive
        );
        safe.emit_by_name::<()>("response", &[&"cancel"]);
        assert!(!safe_rx.await.unwrap());

        let (force, force_rx) =
            build_force_remove_confirmation(Path::new("/repo/feature"), "dirty");
        assert_eq!(force.default_response().as_deref(), Some("cancel"));
        assert_eq!(force.close_response().as_str(), "cancel");
        assert_eq!(
            force.response_appearance("force"),
            adw::ResponseAppearance::Destructive
        );
        force.emit_by_name::<()>("response", &[&"force"]);
        assert!(force_rx.await.unwrap());
    }

    #[test]
    fn worktree_info_fields_are_ordered_and_complete() {
        let mut info = sample_worktree_info("/repo/feature");
        info.branch = None;
        info.changes = Some(WorktreeChanges {
            staged: 1,
            unstaged: 2,
            untracked: 3,
        });
        let row = WorktreeRowView {
            info,
            workspace_open: true,
            workspace_active: false,
            remove_block_reason: None,
            operation_in_progress: false,
        };

        assert_eq!(worktree_info_heading(&row.info), "Detached at 1234567890");
        let body = worktree_info_body(&row);
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines[0], "Path: /repo/feature");
        assert_eq!(lines[1], "HEAD: 1234567890abcdef");
        assert_eq!(lines[2], "Commit: sample commit");
        assert!(lines[3].starts_with("Committed: "));
        assert_eq!(lines[4], "Changes: staged 1, unstaged 2, untracked 3");
        assert_eq!(lines[5], "Locked: No");
        assert_eq!(lines[6], "Prunable: No");
        assert_eq!(lines[7], "Workspace: Open");
    }
}
