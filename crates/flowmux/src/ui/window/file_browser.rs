// SPDX-License-Identifier: GPL-3.0-or-later
//! In-pane file browser: show, focus, state save/restore.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

impl WindowController {
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
        let files_width = if self.file_browser.panel.widget().is_visible() {
            320
        } else {
            0
        };
        if files_width > 0 {
            self.file_browser
                .split
                .set_position((window_width - files_width).max(240));
        }
        if self.worktrees.panel.widget().is_visible() {
            let worktree_container_width = window_width - files_width;
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
            self.file_browser.active.set(true);
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
    pub(super) fn focus_direction_or_file_browser(&self, from: PaneId, dir: FocusDir) {
        let moved = self.focus_in_direction(from, dir).is_some();
        if dir == FocusDir::Right && self.file_browser.panel.widget().is_visible() && !moved {
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
