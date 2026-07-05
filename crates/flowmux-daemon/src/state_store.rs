// SPDX-License-Identifier: GPL-3.0-or-later
//! In-memory state with debounced disk persistence.
//!
//! Every mutation goes through this store, which writes to
//! `$XDG_STATE_HOME/flowmux/state.json` after a short debounce so we
//! never block the event loop on fsync. State load is synchronous on
//! boot.

use flowmux_core::{
    detect_agent_idle_name_from_signals, detect_agent_name_from_signals,
    detect_agent_status_from_signals, terminal_tab_title_for_cwd, AgentPresence, AgentStatus,
    AgentStatusReport, CloseSurfaceOutcome, Pane, PaneContent, PaneId, PaneSurface, RemoveOutcome,
    SplitDirection, Surface, SurfaceId, SurfaceKind, Workspace, WorkspaceAgentBlock, WorkspaceId,
};
use flowmux_state::{State, WindowLayout};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{error, info};

/// flowmux-authored default sidebar palette. Vivid hues spaced around
/// the wheel so adjacent workspaces stay visually distinct against
/// the dark sidebar tint. Picked deterministically from the
/// workspace's UUID so the color stays the same across restarts.
const DEFAULT_PALETTE: &[&str] = &[
    "#7ab7e6", "#e69977", "#9ad57a", "#d188e0", "#e6d077", "#7adfd0", "#e07a9a", "#a797e0",
    "#79e0a3", "#e07a7a",
];

fn default_color_for(id: WorkspaceId) -> String {
    let idx = (id.0.as_u128() as usize) % DEFAULT_PALETTE.len();
    DEFAULT_PALETTE[idx].to_string()
}

#[derive(Debug, Clone, Copy)]
pub enum CloseOutcome {
    /// One leaf removed; the surface still exists.
    PaneRemoved { workspace: WorkspaceId },
    /// The leaf was the last in its surface; the surface was removed.
    SurfaceRemoved { workspace: WorkspaceId },
    /// That was the last surface; the entire workspace was removed.
    WorkspaceRemoved { workspace: WorkspaceId },
}

/// Result of relocating a tab via [`StateStore::move_surface_to_pane`] /
/// [`StateStore::move_surface_to_workspace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveSurfaceOutcome {
    /// Workspace the tab now lives in.
    pub dst_workspace: WorkspaceId,
    /// Workspace the tab came from.
    pub src_workspace: WorkspaceId,
    /// The source leaf emptied and was collapsed, so its pane no longer exists.
    pub src_pane_removed: bool,
    /// Collapsing the source removed its workspace entirely.
    pub src_workspace_removed: bool,
}

/// Result of [`StateStore::split_surface_into_pane`]: like
/// [`MoveSurfaceOutcome`] but also reports the freshly created sibling pane the
/// tab was placed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitMoveOutcome {
    /// The new sibling pane that now holds the moved tab.
    pub new_pane: PaneId,
    pub dst_workspace: WorkspaceId,
    pub src_workspace: WorkspaceId,
    pub src_pane_removed: bool,
    pub src_workspace_removed: bool,
}

/// Remove the leaf pane `target` from an already-locked [`State`], collapsing
/// the enclosing split / surface / workspace as needed. Mirrors the body of
/// [`StateStore::close_pane`] but operates on a held lock so it can be reused
/// by the tab-move path. Returns `None` if `target` is not found.
fn remove_pane_leaf_locked(s: &mut State, target: PaneId) -> Option<CloseOutcome> {
    for ws_idx in 0..s.workspaces.len() {
        let mut surface_to_drop = None;
        for surf_idx in 0..s.workspaces[ws_idx].surfaces.len() {
            let surface = &mut s.workspaces[ws_idx].surfaces[surf_idx];
            let root = std::mem::replace(
                &mut surface.root_pane,
                Pane::Leaf {
                    id: PaneId::new(),
                    content: PaneContent::tabbed_terminal("Terminal", None),
                },
            );
            match root.remove_leaf(target) {
                RemoveOutcome::EntirelyRemoved => {
                    surface_to_drop = Some(surf_idx);
                    break;
                }
                RemoveOutcome::Replaced(new_root) => {
                    surface.root_pane = new_root;
                    return Some(CloseOutcome::PaneRemoved {
                        workspace: s.workspaces[ws_idx].id,
                    });
                }
                RemoveOutcome::NotFound(unchanged) => {
                    surface.root_pane = unchanged;
                }
            }
        }
        if let Some(idx) = surface_to_drop {
            s.workspaces[ws_idx].surfaces.remove(idx);
            let ws_id = s.workspaces[ws_idx].id;
            if s.workspaces[ws_idx].surfaces.is_empty() {
                s.workspaces.remove(ws_idx);
                s.workspace_order.retain(|id| *id != ws_id);
                if s.active_workspace == Some(ws_id) {
                    s.active_workspace = s.workspace_order.first().copied();
                }
                return Some(CloseOutcome::WorkspaceRemoved { workspace: ws_id });
            }
            return Some(CloseOutcome::SurfaceRemoved { workspace: ws_id });
        }
    }
    None
}

#[derive(Clone)]
pub struct StateStore {
    inner: Arc<Mutex<State>>,
    cleared_agent_surfaces: Arc<Mutex<HashSet<SurfaceId>>>,
    dirty: Arc<Notify>,
    /// When false, all on-disk persistence is skipped — the store
    /// behaves as a pure in-memory cache. Used by additional flowmux
    /// windows that did not win the per-host `state.json` lock and so
    /// must not race the lock-owning instance.
    persist: Arc<AtomicBool>,
}

impl StateStore {
    /// Construct from inside a tokio runtime context. Spawns the
    /// persistence loop on the current runtime.
    pub fn new(initial: State) -> Self {
        let mut initial = initial;
        let normalized = normalize_state(&mut initial);
        let store = Self {
            inner: Arc::new(Mutex::new(initial)),
            cleared_agent_surfaces: Arc::new(Mutex::new(HashSet::new())),
            dirty: Arc::new(Notify::new()),
            persist: Arc::new(AtomicBool::new(true)),
        };
        let bg = store.clone();
        tokio::spawn(async move { bg.persist_loop().await });
        if normalized {
            store.mark_dirty();
        }
        store
    }

    /// Construct without entering a tokio context. Caller must spawn
    /// [`StateStore::persist_loop`] on the runtime themselves. Useful
    /// from the GTK main thread before the runtime is fully wired.
    pub fn new_lazy(initial: State) -> Self {
        let mut initial = initial;
        let normalized = normalize_state(&mut initial);
        let store = Self {
            inner: Arc::new(Mutex::new(initial)),
            cleared_agent_surfaces: Arc::new(Mutex::new(HashSet::new())),
            dirty: Arc::new(Notify::new()),
            persist: Arc::new(AtomicBool::new(true)),
        };
        if normalized {
            store.mark_dirty();
        }
        store
    }

    /// Same as [`new_lazy`], but the resulting store will never write
    /// to disk. Used by additional flowmux GUI windows that do not own
    /// the per-host `state.json` lock; their workspaces live and die
    /// with the window so they cannot stomp on the lock owner's file.
    pub fn new_lazy_ephemeral(initial: State) -> Self {
        let mut initial = initial;
        // Still normalize so any in-memory invariants the daemon
        // depends on hold, but do not flip the dirty bit — there is
        // nobody to flush to.
        let _ = normalize_state(&mut initial);
        Self {
            inner: Arc::new(Mutex::new(initial)),
            cleared_agent_surfaces: Arc::new(Mutex::new(HashSet::new())),
            dirty: Arc::new(Notify::new()),
            persist: Arc::new(AtomicBool::new(false)),
        }
    }

    /// True when this store is allowed to write to `state.json`.
    pub fn persist_enabled(&self) -> bool {
        self.persist.load(Ordering::Acquire)
    }

    /// Spawn the persist loop on `handle`. Pair with [`new_lazy`].
    pub fn spawn_persist(&self, handle: &tokio::runtime::Handle) {
        let bg = self.clone();
        handle.spawn(async move { bg.persist_loop().await });
    }

    pub async fn snapshot(&self) -> State {
        self.inner.lock().await.clone()
    }

    pub async fn list_workspaces(&self) -> Vec<WorkspaceId> {
        let s = self.inner.lock().await;
        s.workspaces.iter().map(|w| w.id).collect()
    }

    pub async fn create_workspace(
        &self,
        name: Option<String>,
        root: std::path::PathBuf,
    ) -> WorkspaceId {
        let id = WorkspaceId::new();
        let surface_id = SurfaceId::new();
        let pane_id = PaneId::new();
        let tab_title = terminal_tab_title_for_cwd(Some(&root));
        // Even if caller supplies a name, treat it as the automatic value (`name`).
        // cmux semantics: customTitle is filled only after an explicit user
        // rename, and new workspaces always start with None in automatic mode.
        let auto_name = name.unwrap_or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace")
                .to_string()
        });
        let ws = Workspace {
            id,
            name: auto_name,
            custom_title: None,
            root_dir: root.clone(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: surface_id,
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: Some(root.clone()),
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: pane_id,
                    content: PaneContent::tabbed_terminal(tab_title, Some(root)),
                },
            }],
            color: Some(default_color_for(id)),
        };
        let mut s = self.inner.lock().await;
        s.workspaces.push(ws);
        s.workspace_order.push(id);
        if s.active_workspace.is_none() {
            s.active_workspace = Some(id);
        }
        drop(s);
        self.mark_dirty();
        id
    }

    pub async fn replace_git_info(
        &self,
        workspace: WorkspaceId,
        info: Option<flowmux_core::GitInfo>,
    ) {
        let mut s = self.inner.lock().await;
        if let Some(w) = s.workspaces.iter_mut().find(|w| w.id == workspace) {
            w.git = info;
        }
        drop(s);
        self.mark_dirty();
    }

    pub async fn replace_listening_ports(&self, workspace: WorkspaceId, ports: Vec<u16>) {
        let mut s = self.inner.lock().await;
        if let Some(w) = s.workspaces.iter_mut().find(|w| w.id == workspace) {
            w.listening_ports = ports;
        }
        drop(s);
        self.mark_dirty();
    }

    /// Split a target leaf and replace the new sibling with a
    /// browser pane carrying `url`. Used by `flowmux browser open` to
    /// drop a webview next to a terminal without touching the
    /// terminal's content. The new pane uses the tabbed-browser
    /// content shape so it slots into the pane-local surface-tab
    /// bar like any other browser pane.
    pub async fn split_pane_with_browser(
        &self,
        target: PaneId,
        direction: SplitDirection,
        url: String,
    ) -> Option<(WorkspaceId, PaneId)> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if let Some(new_id) = surface.root_pane.split_leaf(
                    target,
                    direction,
                    0.5,
                    PaneContent::tabbed_browser("Browser", url.clone()),
                ) {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some((ws_id, new_id));
                }
            }
        }
        None
    }

    /// Find the pane in any workspace and split it. Returns the new
    /// pane's id and the workspace it lives in so the GUI can rebuild
    /// the affected widget tree.
    pub async fn split_pane(
        &self,
        target: PaneId,
        direction: SplitDirection,
    ) -> Option<(WorkspaceId, PaneId)> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                let cwd = surface
                    .root_pane
                    .terminal_surface_cwd(target)
                    .or_else(|| Some(ws.root_dir.clone()));
                let title = terminal_tab_title_for_cwd(cwd.as_deref());
                if let Some(new_id) = surface.root_pane.split_leaf(
                    target,
                    direction,
                    0.5,
                    PaneContent::tabbed_terminal(title, cwd),
                ) {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some((ws_id, new_id));
                }
            }
        }
        None
    }

    /// Remove the leaf pane and collapse its split. Returns the
    /// workspace it lived in. If the workspace's last surface becomes
    /// empty as a result, the surface is dropped; if the workspace's
    /// last surface is dropped, the workspace itself is removed too.
    /// Returns `None` if the pane wasn't found.
    pub async fn close_pane(&self, target: PaneId) -> Option<CloseOutcome> {
        let mut s = self.inner.lock().await;
        let outcome = remove_pane_leaf_locked(&mut s, target);
        drop(s);
        if outcome.is_some() {
            self.mark_dirty();
        }
        outcome
    }

    /// Relocate the tab `surface_id` out of `src_pane` and into a brand-new
    /// sibling pane created by splitting `dst_pane` in `direction`. The moved
    /// tab becomes the new pane's only tab. If the source leaf empties it is
    /// collapsed. Returns `None` (state unchanged) if the surface or
    /// destination pane cannot be found.
    pub async fn split_surface_into_pane(
        &self,
        src_pane: PaneId,
        surface_id: SurfaceId,
        dst_pane: PaneId,
        direction: SplitDirection,
    ) -> Option<SplitMoveOutcome> {
        let mut s = self.inner.lock().await;

        let dst_exists = s.workspaces.iter().any(|ws| {
            ws.surfaces
                .iter()
                .any(|sf| sf.root_pane.find_leaf_content(dst_pane).is_some())
        });
        if !dst_exists {
            return None;
        }

        // Take from the source.
        let mut taken = None;
        let mut src_workspace = None;
        let mut src_leaf_empty = false;
        'take: for ws in s.workspaces.iter_mut() {
            for sf in ws.surfaces.iter_mut() {
                if let Some((surface, empty)) =
                    sf.root_pane.take_surface_from_leaf(src_pane, surface_id)
                {
                    taken = Some(surface);
                    src_workspace = Some(ws.id);
                    src_leaf_empty = empty;
                    break 'take;
                }
            }
        }
        let taken = taken?;
        let src_workspace = src_workspace?;

        // Split the destination, placing the moved tab in the new sibling.
        let mut dst_workspace = None;
        let mut new_pane = None;
        let mut pending = Some(taken);
        'split: for ws in s.workspaces.iter_mut() {
            for sf in ws.surfaces.iter_mut() {
                if sf.root_pane.find_leaf_content(dst_pane).is_some() {
                    let surface = pending.take().expect("surface pending split");
                    let content = PaneContent::Tabs {
                        active: surface.id,
                        surfaces: vec![surface],
                    };
                    new_pane = sf.root_pane.split_leaf(dst_pane, direction, 0.5, content);
                    dst_workspace = Some(ws.id);
                    break 'split;
                }
            }
        }
        // split_leaf consumed `taken`; both lookups used the same `dst_exists`
        // check above, so this is unreachable, but guard rather than panic.
        let (Some(new_pane), Some(dst_workspace)) = (new_pane, dst_workspace) else {
            return None;
        };

        let mut src_pane_removed = false;
        let mut src_workspace_removed = false;
        if src_leaf_empty {
            match remove_pane_leaf_locked(&mut s, src_pane) {
                Some(CloseOutcome::WorkspaceRemoved { .. }) => {
                    src_pane_removed = true;
                    src_workspace_removed = true;
                }
                Some(_) => src_pane_removed = true,
                None => {}
            }
        }

        drop(s);
        self.mark_dirty();
        Some(SplitMoveOutcome {
            new_pane,
            dst_workspace,
            src_workspace,
            src_pane_removed,
            src_workspace_removed,
        })
    }

    /// Insert a surface imported from another window/process into `dst_pane`.
    /// The surface gets a fresh id in this store; terminal/browser live widget
    /// state is rebuilt by the GUI from the model.
    pub async fn import_surface_to_pane(
        &self,
        dst_pane: PaneId,
        mut surface: PaneSurface,
        target_index: usize,
    ) -> Option<(WorkspaceId, SurfaceId)> {
        surface.id = SurfaceId::new();
        surface.agent = None;
        let surface_id = surface.id;

        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for sf in ws.surfaces.iter_mut() {
                if sf.root_pane.find_leaf_content(dst_pane).is_some() {
                    sf.root_pane
                        .insert_surface_into_leaf(dst_pane, surface, target_index)?;
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some((ws_id, surface_id));
                }
            }
        }

        None
    }

    /// Split `dst_pane` and place a surface imported from another
    /// window/process into the new sibling pane.
    pub async fn split_imported_surface_into_pane(
        &self,
        dst_pane: PaneId,
        mut surface: PaneSurface,
        direction: SplitDirection,
    ) -> Option<(WorkspaceId, PaneId, SurfaceId)> {
        surface.id = SurfaceId::new();
        surface.agent = None;
        let surface_id = surface.id;
        let content = PaneContent::Tabs {
            active: surface_id,
            surfaces: vec![surface],
        };

        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for sf in ws.surfaces.iter_mut() {
                if sf.root_pane.find_leaf_content(dst_pane).is_some() {
                    let new_pane = sf.root_pane.split_leaf(dst_pane, direction, 0.5, content)?;
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some((ws_id, new_pane, surface_id));
                }
            }
        }

        None
    }

    /// Return the workspace that owns leaf pane `target`, if any.
    pub async fn workspace_of_pane(&self, target: PaneId) -> Option<WorkspaceId> {
        let s = self.inner.lock().await;
        s.workspaces
            .iter()
            .find(|ws| {
                ws.surfaces
                    .iter()
                    .any(|sf| sf.root_pane.find_leaf_content(target).is_some())
            })
            .map(|ws| ws.id)
    }

    /// Relocate the tab `surface_id` out of leaf `src_pane` and into leaf
    /// `dst_pane` at `target_index` (clamped to the end). Works whether the
    /// destination is in the same workspace or a different one. If the source
    /// leaf empties it is collapsed like a pane close. Returns `None` if the
    /// surface or destination pane cannot be found (the state is left
    /// unchanged in that case).
    pub async fn move_surface_to_pane(
        &self,
        src_pane: PaneId,
        surface_id: SurfaceId,
        dst_pane: PaneId,
        target_index: usize,
    ) -> Option<MoveSurfaceOutcome> {
        let mut s = self.inner.lock().await;

        // Destination must exist before we disturb the source, so a missing
        // target is a clean no-op rather than a lost tab.
        let dst_exists = s.workspaces.iter().any(|ws| {
            ws.surfaces
                .iter()
                .any(|sf| sf.root_pane.find_leaf_content(dst_pane).is_some())
        });
        if !dst_exists {
            return None;
        }

        // Take from the source.
        let mut taken = None;
        let mut src_workspace = None;
        let mut src_leaf_empty = false;
        'take: for ws in s.workspaces.iter_mut() {
            for sf in ws.surfaces.iter_mut() {
                if let Some((surface, empty)) =
                    sf.root_pane.take_surface_from_leaf(src_pane, surface_id)
                {
                    taken = Some(surface);
                    src_workspace = Some(ws.id);
                    src_leaf_empty = empty;
                    break 'take;
                }
            }
        }
        let taken = taken?;
        let src_workspace = src_workspace?;

        // Insert into the destination.
        let mut dst_workspace = None;
        let mut pending = Some(taken);
        'insert: for ws in s.workspaces.iter_mut() {
            for sf in ws.surfaces.iter_mut() {
                if sf.root_pane.find_leaf_content(dst_pane).is_some() {
                    let surface = pending.take().expect("surface pending insert");
                    sf.root_pane
                        .insert_surface_into_leaf(dst_pane, surface, target_index);
                    dst_workspace = Some(ws.id);
                    break 'insert;
                }
            }
        }
        let dst_workspace = dst_workspace.expect("destination existed before take");

        // Collapse the source leaf if it emptied.
        let mut src_pane_removed = false;
        let mut src_workspace_removed = false;
        if src_leaf_empty {
            match remove_pane_leaf_locked(&mut s, src_pane) {
                Some(CloseOutcome::WorkspaceRemoved { .. }) => {
                    src_pane_removed = true;
                    src_workspace_removed = true;
                }
                Some(_) => src_pane_removed = true,
                None => {}
            }
        }

        drop(s);
        self.mark_dirty();
        Some(MoveSurfaceOutcome {
            dst_workspace,
            src_workspace,
            src_pane_removed,
            src_workspace_removed,
        })
    }

    /// Convenience wrapper: move a tab to the **last** position of the first
    /// pane of `dst_workspace`. Used by the right-click "Move" menu and by a
    /// drop directly onto a workspace in the side panel.
    pub async fn move_surface_to_workspace(
        &self,
        src_pane: PaneId,
        surface_id: SurfaceId,
        dst_workspace: WorkspaceId,
    ) -> Option<MoveSurfaceOutcome> {
        let dst_pane = {
            let s = self.inner.lock().await;
            let ws = s.workspaces.iter().find(|w| w.id == dst_workspace)?;
            ws.surfaces.first()?.root_pane.first_leaf_id()?
        };
        self.move_surface_to_pane(src_pane, surface_id, dst_pane, usize::MAX)
            .await
    }

    /// Mark `id` as the focused workspace so the next launch starts
    /// there. No-op if the id isn't in the workspace list.
    pub async fn set_active_workspace(&self, id: Option<WorkspaceId>) {
        let mut s = self.inner.lock().await;
        let valid = id
            .map(|i| s.workspaces.iter().any(|w| w.id == i))
            .unwrap_or(true);
        let changed = valid && s.active_workspace != id;
        if changed {
            s.active_workspace = id;
        }
        if let Some(id) = id {
            if let Some(ws) = s.workspaces.iter_mut().find(|w| w.id == id) {
                Self::mark_active_agents_seen_locked(ws);
            }
        }
        drop(s);
        if changed {
            self.mark_dirty();
        }
    }

    /// Set a workspace's sidebar color. Returns true on success.
    pub async fn set_workspace_color(&self, id: WorkspaceId, color: String) -> bool {
        let mut s = self.inner.lock().await;
        let mut updated = false;
        if let Some(w) = s.workspaces.iter_mut().find(|w| w.id == id) {
            w.color = Some(color);
            updated = true;
        }
        drop(s);
        if updated {
            self.mark_dirty();
        }
        updated
    }

    /// Apply the value the user entered in the right-click "Change tab name"
    /// dialog to a workspace. Behavior matches cmux `setCustomTitle`:
    ///   * If trimming both ends yields an empty value, reset to
    ///     `custom_title = None` and return to automatic mode, showing `name`.
    ///   * Otherwise store `custom_title = Some(trimmed)`.
    ///
    /// The automatic value `name` is never modified here, so separate automatic
    /// signals such as folder rename or OSC can update it. Returns `false` when
    /// no workspace matches or nothing changes.
    pub async fn rename_workspace(&self, id: WorkspaceId, raw_input: String) -> bool {
        let trimmed = raw_input.trim();
        let new_custom = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        let mut s = self.inner.lock().await;
        let Some(w) = s.workspaces.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        if w.custom_title == new_custom {
            return false;
        }
        w.custom_title = new_custom;
        drop(s);
        self.mark_dirty();
        true
    }

    pub async fn surface_title(&self, pane: PaneId, surface_id: SurfaceId) -> Option<String> {
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            for surface in &ws.surfaces {
                if let Some(title) = surface.root_pane.surface_title(pane, surface_id) {
                    return Some(title.to_string());
                }
            }
        }
        None
    }

    pub async fn rename_surface(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
        title: String,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface
                    .root_pane
                    .rename_surface(pane, surface_id, title.clone())
                {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some(ws_id);
                }
            }
        }
        None
    }

    /// Set (or clear, with `None`) the live AI-agent presence on the tab
    /// surface `surface_id`. Returns the owning workspace id so the
    /// caller can route a sidebar update. Deliberately does **not**
    /// `mark_dirty`: agent presence is runtime-only (`#[serde(skip)]`),
    /// so there is nothing to persist and we avoid disk churn on every
    /// status flip.
    pub async fn set_agent_activity(
        &self,
        surface_id: SurfaceId,
        agent: Option<AgentPresence>,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        let mut found = None;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface
                    .root_pane
                    .set_surface_agent(surface_id, agent.clone())
                {
                    found = Some(ws.id);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        drop(s);
        if found.is_some() {
            if agent.is_some() {
                self.cleared_agent_surfaces.lock().await.remove(&surface_id);
            } else {
                self.cleared_agent_surfaces.lock().await.insert(surface_id);
            }
        }
        found
    }

    pub async fn clear_dead_agent_activity(&self, surface_id: SurfaceId) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        let mut found = None;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface.root_pane.set_surface_agent(surface_id, None) {
                    found = Some(ws.id);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        drop(s);
        if found.is_some() {
            self.cleared_agent_surfaces.lock().await.remove(&surface_id);
        }
        found
    }

    /// Merge a live agent status report into a tab surface. Returns the owning
    /// workspace and its rolled-up agent status when the report was accepted.
    /// Stale sequence numbers are ignored and return `None`.
    pub async fn report_agent_status(
        &self,
        surface_id: SurfaceId,
        mut report: AgentStatusReport,
    ) -> Option<(WorkspaceId, Option<AgentStatus>)> {
        let mut s = self.inner.lock().await;
        let active_workspace = s.active_workspace;
        let mut accepted = None;
        for ws in s.workspaces.iter_mut() {
            let workspace_visible = active_workspace == Some(ws.id);
            let mut found = false;
            let mut changed = false;
            for surface in ws.surfaces.iter_mut() {
                if let Some(existing) = surface.root_pane.agent_presence_for_surface(surface_id) {
                    preserve_live_agent_pid(&mut report, &existing);
                }
                if let Some(applied) = surface.root_pane.report_surface_agent(
                    surface_id,
                    report.clone(),
                    workspace_visible,
                ) {
                    found = true;
                    changed = applied;
                    break;
                }
            }
            if found {
                if changed {
                    accepted = Some((ws.id, ws.agent_status_rollup()));
                }
                break;
            }
        }
        drop(s);
        if accepted.is_some() {
            self.cleared_agent_surfaces.lock().await.remove(&surface_id);
        }
        accepted
    }

    pub async fn report_agent_screen_signals(
        &self,
        surface_id: SurfaceId,
        screen_text: Option<&str>,
        osc_title: Option<&str>,
    ) -> Option<(WorkspaceId, Option<AgentStatus>)> {
        let detected_status = detect_agent_status_from_signals(screen_text, osc_title);
        let idle_agent_name = if detected_status.is_none() {
            detect_agent_idle_name_from_signals(screen_text, osc_title)
        } else {
            None
        };
        let status = detected_status.or_else(|| idle_agent_name.map(|_| AgentStatus::Idle));
        let agent_name = if status.is_some() {
            detect_agent_name_from_signals(screen_text, osc_title).or(idle_agent_name)
        } else {
            None
        };
        if status.is_none() {
            return self.clear_screen_agent_signal(surface_id).await;
        }
        let status = status?;
        if self
            .cleared_agent_surfaces
            .lock()
            .await
            .contains(&surface_id)
        {
            return None;
        }
        let mut s = self.inner.lock().await;
        let active_workspace = s.active_workspace;
        for ws in s.workspaces.iter_mut() {
            let workspace_visible = active_workspace == Some(ws.id);
            let mut found = false;
            let mut changed = false;
            for surface in ws.surfaces.iter_mut() {
                if let Some(applied) = surface.root_pane.report_surface_agent_signal(
                    surface_id,
                    status,
                    "flowmux:screen",
                    agent_name,
                    workspace_visible,
                ) {
                    found = true;
                    changed = applied;
                    break;
                }
            }
            if found {
                if !changed {
                    return None;
                }
                return Some((ws.id, ws.agent_status_rollup()));
            }
        }
        None
    }

    async fn clear_screen_agent_signal(
        &self,
        surface_id: SurfaceId,
    ) -> Option<(WorkspaceId, Option<AgentStatus>)> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            let mut found = false;
            let mut changed = false;
            for surface in ws.surfaces.iter_mut() {
                if let Some(applied) = surface
                    .root_pane
                    .clear_surface_agent_from_source(surface_id, "flowmux:screen")
                {
                    found = true;
                    changed = applied;
                    break;
                }
            }
            if found {
                if !changed {
                    return None;
                }
                return Some((ws.id, ws.agent_status_rollup()));
            }
        }
        None
    }

    pub async fn workspace_agent_status(&self, workspace: WorkspaceId) -> Option<AgentStatus> {
        let s = self.inner.lock().await;
        s.workspaces
            .iter()
            .find(|ws| ws.id == workspace)
            .and_then(Workspace::agent_status_rollup)
    }

    pub async fn workspace_agent_blocks(
        &self,
        workspace: WorkspaceId,
        mru: &[PaneId],
    ) -> Vec<WorkspaceAgentBlock> {
        let s = self.inner.lock().await;
        s.workspaces
            .iter()
            .find(|ws| ws.id == workspace)
            .map(|ws| ws.collect_agent_blocks(mru))
            .unwrap_or_default()
    }

    fn mark_active_agents_seen_locked(ws: &mut Workspace) -> bool {
        let mut changed = false;
        for surface in ws.surfaces.iter_mut() {
            changed |= surface.root_pane.mark_active_agents_seen();
        }
        changed
    }

    /// Collect `(workspace, surface, pid)` for every tab surface that
    /// currently has an agent presence with a known PID. The daemon's
    /// liveness sweep walks this list and clears entries whose process
    /// has died (hard kill / closed terminal where `SessionEnd` never
    /// fired).
    pub async fn live_agent_presences(&self) -> Vec<(WorkspaceId, SurfaceId, u32)> {
        let s = self.inner.lock().await;
        let mut out = Vec::new();
        for ws in &s.workspaces {
            let mut found = Vec::new();
            for surface in &ws.surfaces {
                surface.root_pane.collect_agent_presences(&mut found);
            }
            for (sid, presence) in found {
                if let Some(pid) = presence.pid {
                    out.push((ws.id, sid, pid));
                }
            }
        }
        out
    }

    pub async fn update_surface_cwd(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
        cwd: std::path::PathBuf,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        let updated = update_surface_cwd_in_state(&mut s, pane, surface_id, cwd);
        drop(s);
        if updated.is_some() {
            self.mark_dirty();
        }
        updated
    }

    /// Store the last URL of a browser surface in state. Called in response to
    /// webview uri_notify so app exit/relaunch can restore the last viewed page.
    pub async fn update_browser_url(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
        url: String,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        let mut updated = None;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface
                    .root_pane
                    .set_surface_browser_url(pane, surface_id, url.clone())
                {
                    updated = Some(ws.id);
                    break;
                }
            }
            if updated.is_some() {
                break;
            }
        }
        drop(s);
        if updated.is_some() {
            self.mark_dirty();
        }
        updated
    }

    /// Apply an automatic title from external signals, such as browser page title
    /// or terminal OSC 0/2, to a surface. User-renamed surfaces (title_locked)
    /// are left untouched.
    ///
    /// Applies cmux's single-panel auto-sync rule in the same call: if the
    /// workspace has no split (single Leaf), the updated surface is that leaf's
    /// active tab, and there is no user-provided `custom_title`, the workspace's
    /// automatic value (`name`) follows the same title. This lets
    /// [`Workspace::display_title`] naturally reflect active tab OSC titles such
    /// as "Claude Code". Splits or locked custom titles block automatic workspace
    /// label changes.
    pub async fn update_surface_auto_title(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
        title: String,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        let mut updated = None;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface
                    .root_pane
                    .set_surface_title_auto(pane, surface_id, title.clone())
                {
                    updated = Some(ws.id);
                    break;
                }
            }
            if updated.is_some() {
                break;
            }
        }
        drop(s);
        if updated.is_some() {
            self.mark_dirty();
        }
        updated
    }

    /// Set the workspace's automatic value (`name`) directly. The GTK side knows
    /// the focused pane's active surface title, so the new design explicitly
    /// updates "current focused tab = workspace name" through this setter.
    /// `custom_title` is left untouched so user-locked labels remain. Returns
    /// `false` for no changes or missing workspaces.
    pub async fn set_workspace_name(&self, id: WorkspaceId, name: String) -> bool {
        let mut s = self.inner.lock().await;
        let Some(w) = s.workspaces.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        if w.name == name {
            return false;
        }
        w.name = name;
        drop(s);
        self.mark_dirty();
        true
    }

    pub fn update_surface_cwd_blocking(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
        cwd: std::path::PathBuf,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.blocking_lock();
        let updated = update_surface_cwd_in_state(&mut s, pane, surface_id, cwd);
        drop(s);
        if updated.is_some() {
            self.mark_dirty();
        }
        updated
    }

    /// Called when reordering terminal/browser tabs inside a pane by drag and
    /// drop. Moves `surface_id` within the same pane to `target_index`. The index
    /// is the final position after applying the move and clamps to the end when
    /// too large. Returns `None` for no changes or missing surfaces so callers
    /// leave GTK widgets untouched. Active SurfaceId is unaffected by reorder,
    /// so the same tab remains active after moving.
    pub async fn reorder_surface_in_pane(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
        target_index: usize,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface
                    .root_pane
                    .reorder_surface_in_leaf(pane, surface_id, target_index)
                {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some(ws_id);
                }
            }
        }
        None
    }

    /// Called when reordering workspaces in the side panel by drag and drop.
    /// Moves the workspace identified by `id` to `target_index` inside
    /// `workspace_order`. The index is the final position after applying the
    /// move and clamps to the end when too large. Same-position moves or missing
    /// workspaces return `false`.
    pub async fn reorder_workspace(&self, id: WorkspaceId, target_index: usize) -> bool {
        let mut s = self.inner.lock().await;
        let Some(current) = s.workspace_order.iter().position(|x| *x == id) else {
            return false;
        };
        let len = s.workspace_order.len();
        if len == 0 {
            return false;
        }
        let target = target_index.min(len - 1);
        if current == target {
            return false;
        }
        let removed = s.workspace_order.remove(current);
        s.workspace_order.insert(target, removed);
        drop(s);
        self.mark_dirty();
        true
    }

    /// Saved window size/maximized state. `None` on first launch.
    pub fn window_layout_blocking(&self) -> Option<WindowLayout> {
        self.inner.blocking_lock().window.clone()
    }

    /// Saved side-panel divider pixel position. `None` on first launch.
    pub fn sidebar_position_blocking(&self) -> Option<i32> {
        self.inner.blocking_lock().sidebar_position
    }

    /// Record window size/maximized state in state. This blocking variant is
    /// used because close handling calls it synchronously on the GTK main thread,
    /// where the async runtime is not guaranteed to still be alive.
    pub fn set_window_layout_blocking(&self, layout: WindowLayout) {
        let mut s = self.inner.blocking_lock();
        if s.window.as_ref() == Some(&layout) {
            return;
        }
        s.window = Some(layout);
        drop(s);
        self.mark_dirty();
    }

    /// Record the divider pixel position between side panel and content area.
    pub fn set_sidebar_position_blocking(&self, position: i32) {
        let mut s = self.inner.blocking_lock();
        if s.sidebar_position == Some(position) {
            return;
        }
        s.sidebar_position = Some(position);
        drop(s);
        self.mark_dirty();
    }

    /// Apply a pane split divider ratio to the model. `split_id` is the PaneId
    /// of a `Pane::Split` node in the tree. Returns `false` if no matching split
    /// exists or the ratio is unchanged so callers can skip dirty marking.
    pub fn set_pane_split_ratio_blocking(&self, split_id: PaneId, ratio: f32) -> bool {
        let mut s = self.inner.blocking_lock();
        let mut updated = false;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface.root_pane.set_split_ratio(split_id, ratio) {
                    updated = true;
                    break;
                }
            }
            if updated {
                break;
            }
        }
        drop(s);
        if updated {
            self.mark_dirty();
        }
        updated
    }

    pub async fn set_pane_split_ratio(&self, split_id: PaneId, ratio: f32) -> bool {
        let mut s = self.inner.lock().await;
        let mut updated = false;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                if surface.root_pane.set_split_ratio(split_id, ratio) {
                    updated = true;
                    break;
                }
            }
            if updated {
                break;
            }
        }
        drop(s);
        if updated {
            self.mark_dirty();
        }
        updated
    }

    pub async fn parent_split_for_pane(&self, pane: PaneId) -> Option<PaneId> {
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            for surface in &ws.surfaces {
                if let Some(split_id) = surface.root_pane.parent_split_id(pane) {
                    return Some(split_id);
                }
            }
        }
        None
    }

    /// Remove an entire workspace. Used by the sidebar's X close
    /// button. Returns true if a workspace with that id existed.
    pub async fn remove_workspace(&self, id: WorkspaceId) -> bool {
        let mut s = self.inner.lock().await;
        let before = s.workspaces.len();
        s.workspaces.retain(|w| w.id != id);
        s.workspace_order.retain(|x| *x != id);
        if s.active_workspace == Some(id) {
            s.active_workspace = s.workspace_order.first().copied();
        }
        let removed = s.workspaces.len() < before;
        drop(s);
        if removed {
            self.mark_dirty();
        }
        removed
    }

    /// Remove every workspace at once. Used by the sidebar context
    /// menu's "Close all tabs" item. Returns the ids that were removed
    /// (in their prior order) so the caller can tear down each
    /// workspace's GUI page. Clears the active-workspace pointer since
    /// nothing is left to activate.
    pub async fn remove_all_workspaces(&self) -> Vec<WorkspaceId> {
        let mut s = self.inner.lock().await;
        let removed: Vec<WorkspaceId> = s.workspace_order.clone();
        let removed = if removed.is_empty() {
            s.workspaces.iter().map(|w| w.id).collect()
        } else {
            removed
        };
        s.workspaces.clear();
        s.workspace_order.clear();
        s.active_workspace = None;
        drop(s);
        if !removed.is_empty() {
            self.mark_dirty();
        }
        removed
    }

    pub async fn workspace_for_pane(&self, pane: PaneId) -> Option<WorkspaceId> {
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            for surface in &ws.surfaces {
                if pane_tree_contains(&surface.root_pane, pane) {
                    return Some(ws.id);
                }
            }
        }
        None
    }

    /// Find a leaf pane whose currently-active tab title starts with
    /// `needle` (ASCII case-insensitive). Used by the Notify dispatcher
    /// as a fallback when the hook source couldn't pass pane/surface
    /// info — e.g. the Flatpak OpenCode plugin path, where `flatpak
    /// run` resets env before the in-sandbox CLI can read
    /// `FLOWMUX_PANE_ID`, so the hook-driven Notify arrives with
    /// `pane=None`. Without recovery the daemon stores the entry with
    /// no workspace, so the sidebar can't blink (`mark_attention`
    /// needs a workspace id) and the bell click can't navigate
    /// (`focus_pane` needs a pane id). With this lookup the daemon
    /// rebuilds the routing context from the pane title flowmux
    /// already tracks (e.g. the active tab in pane 86ff5134 has title
    /// "OpenCode" once the agent attaches its PTY, which matches the
    /// "OpenCode" prefix of the Notify's `title="OpenCode ready"`).
    ///
    /// Returns the first matching `(workspace, pane, surface)` tuple.
    /// First-match policy is intentional: when only one pane runs the
    /// agent the answer is unambiguous, and when several do, blinking
    /// one of them still beats blinking none. We never invent
    /// associations across workspaces — the candidate must actually
    /// own a leaf whose active surface title matches.
    pub async fn find_pane_by_active_title_prefix(
        &self,
        needle: &str,
    ) -> Option<(WorkspaceId, PaneId, SurfaceId)> {
        if needle.is_empty() {
            return None;
        }
        let needle_lower = needle.to_ascii_lowercase();
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            for surface in &ws.surfaces {
                if let Some((pane_id, surface_id)) =
                    find_active_title_prefix(&surface.root_pane, &needle_lower)
                {
                    return Some((ws.id, pane_id, surface_id));
                }
            }
        }
        None
    }

    pub async fn get_workspace(&self, id: WorkspaceId) -> Option<Workspace> {
        let s = self.inner.lock().await;
        s.workspaces.iter().find(|w| w.id == id).cloned()
    }

    /// Active workspace, falling back to the first one available.
    pub async fn active_or_first(&self) -> Option<WorkspaceId> {
        let s = self.inner.lock().await;
        s.active_workspace
            .or_else(|| s.workspaces.first().map(|w| w.id))
    }

    /// Add a fresh terminal surface to a workspace. Used by the
    /// "new surface" keyboard shortcut.
    pub async fn add_terminal_surface(
        &self,
        workspace: WorkspaceId,
        cwd: Option<std::path::PathBuf>,
    ) -> Option<SurfaceId> {
        let mut s = self.inner.lock().await;
        let w = s.workspaces.iter_mut().find(|w| w.id == workspace)?;
        let pane = w.surfaces.first()?.root_pane.first_leaf_id()?;
        let cwd = cwd
            .or_else(|| w.surfaces[0].root_pane.terminal_surface_cwd(pane))
            .or_else(|| Some(w.root_dir.clone()));
        let title = terminal_tab_title_for_cwd(cwd.as_deref());
        let surface = PaneSurface::terminal(title, cwd);
        let surface_id = w.surfaces[0].root_pane.add_surface_to_leaf(pane, surface)?;
        drop(s);
        self.mark_dirty();
        Some(surface_id)
    }

    pub async fn add_terminal_surface_to_pane(
        &self,
        pane: PaneId,
        cwd: Option<std::path::PathBuf>,
    ) -> Option<(WorkspaceId, SurfaceId)> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                let resolved_cwd = cwd
                    .clone()
                    .or_else(|| surface.root_pane.terminal_surface_cwd(pane))
                    .or_else(|| Some(ws.root_dir.clone()));
                let title = terminal_tab_title_for_cwd(resolved_cwd.as_deref());
                let tab = PaneSurface::terminal(title, resolved_cwd);
                if let Some(surface_id) = surface.root_pane.add_surface_to_leaf(pane, tab) {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some((ws_id, surface_id));
                }
            }
        }
        None
    }

    /// Add a browser surface to a workspace and return its id.
    pub async fn add_browser_surface(
        &self,
        workspace: WorkspaceId,
        url: String,
    ) -> Option<SurfaceId> {
        let mut s = self.inner.lock().await;
        let w = s.workspaces.iter_mut().find(|w| w.id == workspace)?;
        let pane = w.surfaces.first()?.root_pane.first_leaf_id()?;
        let tab = PaneSurface::browser("Browser", url);
        let surface_id = w.surfaces[0].root_pane.add_surface_to_leaf(pane, tab)?;
        drop(s);
        self.mark_dirty();
        Some(surface_id)
    }

    /// Walk every workspace's pane tree looking for a browser leaf
    /// that lives on the right side of `from`. cmux's
    /// `preferredBrowserTargetPane` policy: a `flowmux browser open`
    /// invoked from a terminal pane reuses an existing right-sibling
    /// browser pane instead of creating a new split. Returns the
    /// browser leaf's `PaneId` when found.
    pub async fn find_right_sibling_browser_leaf(&self, from: PaneId) -> Option<PaneId> {
        let s = self.inner.lock().await;
        for ws in s.workspaces.iter() {
            for surface in ws.surfaces.iter() {
                if let Some(p) = surface.root_pane.find_right_sibling_browser_leaf(from) {
                    return Some(p);
                }
            }
        }
        None
    }

    pub async fn add_browser_surface_to_pane(
        &self,
        pane: PaneId,
        url: String,
    ) -> Option<(WorkspaceId, SurfaceId)> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            for surface in ws.surfaces.iter_mut() {
                let tab = PaneSurface::browser("Browser", url.clone());
                if let Some(surface_id) = surface.root_pane.add_surface_to_leaf(pane, tab) {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some((ws_id, surface_id));
                }
            }
        }
        None
    }

    pub async fn set_active_surface(
        &self,
        pane: PaneId,
        surface_id: SurfaceId,
    ) -> Option<WorkspaceId> {
        let mut s = self.inner.lock().await;
        for ws in s.workspaces.iter_mut() {
            let mut hit = false;
            for surface in ws.surfaces.iter_mut() {
                if surface.root_pane.set_active_surface(pane, surface_id) {
                    surface.root_pane.mark_surface_agent_seen(surface_id);
                    hit = true;
                    break;
                }
            }
            if hit {
                let ws_id = ws.id;
                drop(s);
                self.mark_dirty();
                return Some(ws_id);
            }
        }
        None
    }

    /// Peek-only: how many leaf panes the workspace containing
    /// `target` has, plus the workspace id. Used by the GUI to decide
    /// whether closing `target` would also close the workspace, so it
    /// can put up a confirmation dialog before the mutation runs.
    pub async fn workspace_pane_count_for(&self, target: PaneId) -> Option<(WorkspaceId, usize)> {
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            let mut count = 0usize;
            let mut found = false;
            for surf in &ws.surfaces {
                surf.root_pane.for_each_leaf(|id| {
                    count += 1;
                    if id == target {
                        found = true;
                    }
                });
            }
            if found {
                return Some((ws.id, count));
            }
        }
        None
    }

    /// Peek-only: number of tab surfaces inside `pane`. `None` when
    /// the pane is unknown or it is a non-tabbed leaf. Used together
    /// with `workspace_pane_count_for` to decide whether closing a
    /// surface (tab) ends up closing the whole workspace.
    pub async fn tab_count_in_pane(&self, pane: PaneId) -> Option<usize> {
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            for surf in &ws.surfaces {
                if let Some(count) = pane_tab_count(&surf.root_pane, pane) {
                    return Some(count);
                }
            }
        }
        None
    }

    pub async fn close_surface(&self, pane: PaneId, surface_id: SurfaceId) -> Option<CloseOutcome> {
        let mut s = self.inner.lock().await;
        for ws_idx in 0..s.workspaces.len() {
            for surf_idx in 0..s.workspaces[ws_idx].surfaces.len() {
                let outcome = s.workspaces[ws_idx].surfaces[surf_idx]
                    .root_pane
                    .close_surface_in_leaf(pane, surface_id);
                match outcome {
                    CloseSurfaceOutcome::SurfaceRemoved => {
                        let ws_id = s.workspaces[ws_idx].id;
                        drop(s);
                        self.mark_dirty();
                        return Some(CloseOutcome::SurfaceRemoved { workspace: ws_id });
                    }
                    CloseSurfaceOutcome::LastSurfaceRemoved => {
                        drop(s);
                        return self.close_pane(pane).await;
                    }
                    CloseSurfaceOutcome::NotFound => {}
                }
            }
        }
        None
    }

    pub fn mark_dirty(&self) {
        self.dirty.notify_one();
    }

    pub async fn persist_loop(&self) {
        loop {
            self.dirty.notified().await;
            // Coalesce a flurry of mutations into a single write.
            tokio::time::sleep(Duration::from_millis(250)).await;
            // Ephemeral stores still observe the dirty bit so callers
            // do not need to special-case mutation paths, but they
            // never reach the disk.
            if !self.persist_enabled() {
                continue;
            }
            let snap = self.snapshot().await;
            match flowmux_state::save(&snap) {
                Ok(()) => info!(workspaces = snap.workspaces.len(), "state persisted"),
                Err(e) => error!(error = %e, "state save failed"),
            }
        }
    }

    pub async fn save_now(&self) -> Result<(), flowmux_state::StateError> {
        if !self.persist_enabled() {
            return Ok(());
        }
        let snap = self.snapshot().await;
        flowmux_state::save(&snap)
    }

    pub fn save_now_blocking(&self) -> Result<(), flowmux_state::StateError> {
        if !self.persist_enabled() {
            return Ok(());
        }
        let snap = self.inner.blocking_lock().clone();
        flowmux_state::save(&snap)
    }
}

/// True when the pane tree has any leaf with id `target`. Walks the
/// tree with early-exit so a hit on the left subtree skips the right.
fn pane_tree_contains(tree: &Pane, target: PaneId) -> bool {
    match tree {
        Pane::Leaf { id, .. } => *id == target,
        Pane::Split { first, second, .. } => {
            pane_tree_contains(first, target) || pane_tree_contains(second, target)
        }
    }
}

/// Walk the pane tree and return the first leaf whose currently
/// active tab title (ASCII lowercased) starts with `needle_lower`.
/// Returns `(pane_id, active_surface_id)`. Used by
/// [`StateStore::find_pane_by_active_title_prefix`] as a fallback
/// route for Notify events that arrive with no pane info.
fn find_active_title_prefix(tree: &Pane, needle_lower: &str) -> Option<(PaneId, SurfaceId)> {
    match tree {
        Pane::Leaf { id, content } => match content {
            PaneContent::Tabs { active, surfaces } => surfaces
                .iter()
                .find(|s| s.id == *active)
                .filter(|s| s.title.to_ascii_lowercase().starts_with(needle_lower))
                .map(|s| (*id, s.id)),
            // Legacy leaf shapes carry no per-tab title; they should
            // have been normalised on load, but stay defensive.
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
        },
        Pane::Split { first, second, .. } => find_active_title_prefix(first, needle_lower)
            .or_else(|| find_active_title_prefix(second, needle_lower)),
    }
}

/// Count the tab surfaces inside the leaf identified by `target` in
/// the given pane tree. Returns `None` when `target` is not a
/// `PaneContent::Tabs` leaf (Terminal/Browser leaves with no tabs).
fn pane_tab_count(tree: &Pane, target: PaneId) -> Option<usize> {
    match tree {
        Pane::Leaf { id, content } if *id == target => match content {
            PaneContent::Tabs { surfaces, .. } => Some(surfaces.len()),
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
        },
        Pane::Leaf { .. } => None,
        Pane::Split { first, second, .. } => {
            pane_tab_count(first, target).or_else(|| pane_tab_count(second, target))
        }
    }
}

fn update_surface_cwd_in_state(
    state: &mut State,
    pane: PaneId,
    surface_id: SurfaceId,
    cwd: std::path::PathBuf,
) -> Option<WorkspaceId> {
    for ws in state.workspaces.iter_mut() {
        for surface in ws.surfaces.iter_mut() {
            if surface
                .root_pane
                .set_surface_cwd(pane, surface_id, cwd.clone())
            {
                return Some(ws.id);
            }
        }
    }
    None
}

fn normalize_state(state: &mut State) -> bool {
    let mut changed = false;
    for ws in &mut state.workspaces {
        for surface in &mut ws.surfaces {
            let fallback_cwd = match &surface.kind {
                SurfaceKind::Terminal { cwd, .. } => cwd.clone(),
                SurfaceKind::Browser { .. } => None,
            };
            changed |= surface.root_pane.normalize_leaf_tabs(fallback_cwd);
        }
        if ws.surfaces.len() > 1 {
            changed |= migrate_top_level_surfaces_to_first_pane(ws);
        }
        for surface in &mut ws.surfaces {
            changed |= surface.root_pane.normalize_leaf_tabs(None);
        }
    }
    changed
}

fn migrate_top_level_surfaces_to_first_pane(ws: &mut Workspace) -> bool {
    let Some(target_pane) = ws.surfaces[0].root_pane.first_leaf_id() else {
        return false;
    };
    let extra_surfaces = ws.surfaces.split_off(1);
    let changed = !extra_surfaces.is_empty();
    for surface in extra_surfaces {
        let Some(mut tab) = first_active_pane_surface(&surface.root_pane) else {
            continue;
        };
        tab.id = surface.id;
        if !surface.title.is_empty() {
            tab.title = surface.title;
        }
        ws.surfaces[0]
            .root_pane
            .add_surface_to_leaf(target_pane, tab);
    }
    changed
}

fn first_active_pane_surface(pane: &Pane) -> Option<PaneSurface> {
    match pane {
        Pane::Leaf { content, .. } => content.active_surface().cloned(),
        Pane::Split { first, second, .. } => {
            first_active_pane_surface(first).or_else(|| first_active_pane_surface(second))
        }
    }
}

fn preserve_live_agent_pid(report: &mut AgentStatusReport, existing: &AgentPresence) {
    let (Some(existing_pid), Some(incoming_pid)) = (existing.pid, report.pid) else {
        return;
    };
    if existing_pid == incoming_pid {
        return;
    }
    if existing.name != report.name {
        return;
    }
    if existing.source.as_deref() != Some("flowmux:hook")
        || report.source.as_deref() != Some("flowmux:hook")
    {
        return;
    }
    if flowmux_procmon::pid_alive(existing_pid) {
        report.pid = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_pane(ws: &Workspace) -> PaneId {
        ws.surfaces[0].root_pane.first_leaf_id().unwrap()
    }

    fn first_pane_tab_count(ws: &Workspace) -> usize {
        let Pane::Leaf { content, .. } = &ws.surfaces[0].root_pane else {
            panic!("expected single leaf")
        };
        match content {
            PaneContent::Tabs { surfaces, .. } => surfaces.len(),
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => 1,
        }
    }

    fn first_pane_active_surface(ws: &Workspace) -> SurfaceId {
        let pane = first_pane(ws);
        ws.surfaces[0]
            .root_pane
            .active_surface_id(pane)
            .expect("expected active surface")
    }

    #[tokio::test]
    async fn create_workspace_sets_order_active_surface_and_color() {
        let store = StateStore::new_lazy(State::default());
        let root = std::path::PathBuf::from("/tmp/demo");
        let id = store.create_workspace(None, root.clone()).await;

        let state = store.snapshot().await;
        assert_eq!(state.workspace_order, vec![id]);
        assert_eq!(state.active_workspace, Some(id));
        assert_eq!(state.workspaces.len(), 1);
        let ws = &state.workspaces[0];
        assert_eq!(ws.name, "demo");
        assert!(ws.color.as_deref().is_some_and(|c| c.starts_with('#')));
        assert_eq!(ws.surfaces.len(), 1);
        assert!(matches!(
            &ws.surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &root
        ));
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(surfaces[0].title, "demo");
        assert!(!surfaces[0].title_locked);
    }

    #[tokio::test]
    async fn report_agent_status_surfaces_in_workspace_tree() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        let result = store
            .report_agent_status(
                surface,
                AgentStatusReport {
                    name: "claude".into(),
                    status: Some(AgentStatus::Working),
                    activity: Some(flowmux_core::AgentActivity::Running),
                    pid: Some(42),
                    source: Some("flowmux:hook".into()),
                    seq: Some(1),
                    message: None,
                    custom_status: None,
                    session_id: None,
                },
            )
            .await;
        assert_eq!(result, Some((ws_id, Some(AgentStatus::Working))));

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.status, AgentStatus::Working);
    }

    #[tokio::test]
    async fn report_agent_status_keeps_opencode_name_for_oc_titled_surface() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);
        let surface = first_pane_active_surface(&ws);
        assert_eq!(
            store
                .rename_surface(pane, surface, "OC | greeting".into())
                .await,
            Some(ws_id)
        );

        let result = store
            .report_agent_status(
                surface,
                AgentStatusReport {
                    name: "claude".into(),
                    status: Some(AgentStatus::Idle),
                    activity: Some(flowmux_core::AgentActivity::Idle),
                    pid: None,
                    source: Some("flowmux:hook".into()),
                    seq: Some(1),
                    message: None,
                    custom_status: None,
                    session_id: Some("ses-opencode".into()),
                },
            )
            .await;
        assert_eq!(result, Some((ws_id, Some(AgentStatus::Idle))));

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.status, AgentStatus::Idle);
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
    }

    #[tokio::test]
    async fn report_agent_screen_signals_can_create_fallback_presence() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        let result = store
            .report_agent_screen_signals(surface, None, Some("Codex Action Required"))
            .await;
        assert_eq!(result, Some((ws_id, Some(AgentStatus::Blocked))));

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.status, AgentStatus::Blocked);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[tokio::test]
    async fn report_agent_screen_signals_restores_idle_presence_from_agent_name() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        let result = store
            .report_agent_screen_signals(surface, Some("Codex\npress / for commands"), None)
            .await;
        assert_eq!(result, Some((ws_id, Some(AgentStatus::Idle))));

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.status, AgentStatus::Idle);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[tokio::test]
    async fn report_agent_screen_signals_restores_idle_presence_from_blank_screen_title() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        let result = store
            .report_agent_screen_signals(surface, Some("  \n\n"), Some("Claude"))
            .await;
        assert_eq!(result, Some((ws_id, Some(AgentStatus::Idle))));

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.status, AgentStatus::Idle);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[tokio::test]
    async fn report_agent_screen_signals_clears_screen_presence_when_signal_disappears() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_screen_signals(surface, Some("Codex\npress / for commands"), None)
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );
        assert_eq!(
            store
                .report_agent_screen_signals(surface, Some("$ echo shell ready"), Some("demo"))
                .await,
            Some((ws_id, None))
        );

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        assert!(tree[0].panes[0].tabs[0].agent.is_none());
    }

    #[tokio::test]
    async fn screen_clear_does_not_remove_matching_hook_presence() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "codex".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(42),
                        source: Some("flowmux:hook".into()),
                        seq: Some(1),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );
        assert_eq!(
            store
                .report_agent_screen_signals(surface, None, Some("Codex Working"))
                .await,
            Some((ws_id, Some(AgentStatus::Working)))
        );
        assert_eq!(
            store
                .report_agent_screen_signals(surface, Some("$ echo shell ready"), Some("demo"))
                .await,
            None
        );

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.status, AgentStatus::Working);
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
        assert_eq!(
            store.live_agent_presences().await,
            vec![(ws_id, surface, 42)]
        );
    }

    #[tokio::test]
    async fn auto_approve_banner_does_not_block_hook_presence() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "cline".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(42),
                        source: Some("flowmux:hook".into()),
                        seq: Some(1),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );
        assert_eq!(
            store
                .report_agent_screen_signals(
                    surface,
                    Some("Ask anything...\nGPT-5.4  Plan  Act\nAuto-approve all enabled"),
                    Some("> hello"),
                )
                .await,
            None
        );

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "cline");
        assert_eq!(agent.status, AgentStatus::Idle);
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
    }

    #[tokio::test]
    async fn duplicate_session_start_does_not_replace_live_hook_pid() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);
        let live_pid = std::process::id();
        let other_pid = live_pid.saturating_add(100_000);

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "opencode".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(live_pid),
                        source: Some("flowmux:hook".into()),
                        seq: Some(1),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );
        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "opencode".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(other_pid),
                        source: Some("flowmux:hook".into()),
                        seq: Some(2),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );

        assert_eq!(
            store.live_agent_presences().await,
            vec![(ws_id, surface, live_pid)]
        );
    }

    #[tokio::test]
    async fn dead_pid_clear_does_not_block_live_screen_fallback_restore() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "opencode".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(42),
                        source: Some("flowmux:hook".into()),
                        seq: Some(1),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );
        assert_eq!(store.clear_dead_agent_activity(surface).await, Some(ws_id));
        assert_eq!(
            store
                .report_agent_screen_signals(
                    surface,
                    Some(
                        "Ask anything... \"Fix broken tests\"\n\
                         Sisyphus - Ultraworker · GPT-5.5 OpenAI · medium\n\
                         tab agents  ctrl+p commands"
                    ),
                    Some("OpenCode"),
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.status, AgentStatus::Idle);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[tokio::test]
    async fn stale_agent_name_scrollback_does_not_recreate_screen_presence() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_screen_signals(surface, Some("codex exited\n$ echo done"), None)
                .await,
            None
        );

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        assert!(tree[0].panes[0].tabs[0].agent.is_none());
    }

    #[tokio::test]
    async fn cleared_agent_presence_blocks_stale_screen_recreation_until_hook_report() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "codex".into(),
                        status: Some(AgentStatus::Working),
                        activity: Some(flowmux_core::AgentActivity::Running),
                        pid: Some(42),
                        source: Some("flowmux:hook".into()),
                        seq: Some(1),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Working)))
        );

        assert_eq!(store.set_agent_activity(surface, None).await, Some(ws_id));
        assert_eq!(
            store
                .report_agent_screen_signals(surface, Some("Codex\npress / for commands"), None)
                .await,
            None
        );
        assert_eq!(
            store
                .report_agent_screen_signals(surface, None, Some("Codex Action Required"))
                .await,
            None
        );
        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        assert!(tree[0].panes[0].tabs[0].agent.is_none());

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "codex".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(43),
                        source: Some("flowmux:hook".into()),
                        seq: Some(2),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );
        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "codex");
        assert_eq!(
            store.live_agent_presences().await,
            vec![(ws_id, surface, 43)]
        );
    }

    #[tokio::test]
    async fn report_agent_screen_signals_can_replace_stale_claude_presence_name() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .report_agent_status(
                    surface,
                    AgentStatusReport {
                        name: "claude".into(),
                        status: Some(AgentStatus::Idle),
                        activity: Some(flowmux_core::AgentActivity::Idle),
                        pid: Some(42),
                        source: Some("flowmux:hook".into()),
                        seq: Some(1),
                        message: None,
                        custom_status: None,
                        session_id: None,
                    },
                )
                .await,
            Some((ws_id, Some(AgentStatus::Idle)))
        );

        let result = store
            .report_agent_screen_signals(surface, None, Some("Cline Action Required"))
            .await;
        assert_eq!(result, Some((ws_id, Some(AgentStatus::Blocked))));

        let state = store.snapshot().await;
        let tree = flowmux_ipc::protocol::describe_workspaces(&state.workspaces);
        let agent = tree[0].panes[0].tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "cline");
        assert_eq!(agent.status, AgentStatus::Blocked);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[tokio::test]
    async fn split_and_close_pane_updates_tree_and_removes_empty_workspace() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (split_ws, new_pane) = store
            .split_pane(original, SplitDirection::Vertical)
            .await
            .unwrap();
        assert_eq!(split_ws, ws_id);
        assert_eq!(store.workspace_for_pane(new_pane).await, Some(ws_id));
        let ws = store.get_workspace(ws_id).await.unwrap();
        let new_surface = ws.surfaces[0]
            .root_pane
            .active_surface_id(new_pane)
            .expect("expected active surface in new pane");
        assert_eq!(
            ws.surfaces[0]
                .root_pane
                .surface_title(new_pane, new_surface),
            Some("demo")
        );

        let outcome = store.close_pane(new_pane).await.unwrap();
        assert!(matches!(outcome, CloseOutcome::PaneRemoved { workspace } if workspace == ws_id));
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.surfaces[0].root_pane.first_leaf_id(), Some(original));

        let outcome = store.close_pane(original).await.unwrap();
        assert!(matches!(
            outcome,
            CloseOutcome::WorkspaceRemoved { workspace } if workspace == ws_id
        ));
        let state = store.snapshot().await;
        assert!(state.workspaces.is_empty());
        assert!(state.workspace_order.is_empty());
        assert_eq!(state.active_workspace, None);
    }

    #[tokio::test]
    async fn workspace_mutators_only_mark_successful_changes() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("old".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let missing = WorkspaceId::new();

        // Rename follows cmux semantics: keep `name` as the automatic value and
        // update only custom_title.
        assert!(store.rename_workspace(ws_id, "new".into()).await);
        assert!(store.set_workspace_color(ws_id, "#112233".into()).await);
        assert!(!store.rename_workspace(missing, "missing".into()).await);
        assert!(!store.set_workspace_color(missing, "#445566".into()).await);
        store.set_active_workspace(Some(missing)).await;

        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.name, "old");
        assert_eq!(ws.custom_title.as_deref(), Some("new"));
        assert_eq!(ws.display_title(), "new");
        assert_eq!(ws.color.as_deref(), Some("#112233"));
        assert_eq!(store.snapshot().await.active_workspace, Some(ws_id));
    }

    #[tokio::test]
    async fn rename_workspace_clears_custom_title_on_empty_input() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("auto".into()), std::path::PathBuf::from("/tmp/auto"))
            .await;

        // User rename -> custom_title is filled.
        assert!(store.rename_workspace(ws_id, "MyName".into()).await);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.custom_title.as_deref(), Some("MyName"));
        assert_eq!(ws.display_title(), "MyName");

        // Empty input -> return to automatic mode (custom_title = None).
        assert!(store.rename_workspace(ws_id, "".into()).await);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.custom_title, None);
        assert_eq!(ws.display_title(), "auto");
        assert_eq!(ws.name, "auto");

        // Whitespace-only input has the same meaning.
        assert!(store.rename_workspace(ws_id, "Custom Again".into()).await);
        assert!(store.rename_workspace(ws_id, "   \t\n".into()).await);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.custom_title, None);
    }

    #[tokio::test]
    async fn rename_workspace_trims_whitespace_around_input() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("auto".into()), std::path::PathBuf::from("/tmp/auto"))
            .await;
        assert!(
            store
                .rename_workspace(ws_id, "  Spaced Name  ".into())
                .await
        );
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.custom_title.as_deref(), Some("Spaced Name"));
    }

    #[tokio::test]
    async fn rename_workspace_idempotent_for_same_value() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("auto".into()), std::path::PathBuf::from("/tmp/auto"))
            .await;
        assert!(store.rename_workspace(ws_id, "Same".into()).await);
        // Re-entering the same value -> false (no change).
        assert!(!store.rename_workspace(ws_id, "Same".into()).await);
        // Same trimmed result returns false.
        assert!(!store.rename_workspace(ws_id, "  Same  ".into()).await);
        // Empty input twice returns false on the second call.
        assert!(store.rename_workspace(ws_id, "".into()).await);
        assert!(!store.rename_workspace(ws_id, "".into()).await);
    }

    #[tokio::test]
    async fn normalizes_legacy_top_level_surfaces_into_first_pane_tabs() {
        let ws_id = WorkspaceId::new();
        let first_surface = SurfaceId::new();
        let second_surface = SurfaceId::new();
        let mut state = State::default();
        state.workspaces.push(Workspace {
            id: ws_id,
            name: "legacy".into(),
            custom_title: None,
            root_dir: "/tmp/legacy".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![
                Surface {
                    id: first_surface,
                    kind: SurfaceKind::Terminal {
                        shell: None,
                        cwd: Some("/tmp/legacy".into()),
                    },
                    title: "main".into(),
                    root_pane: Pane::Leaf {
                        id: PaneId::new(),
                        content: PaneContent::Terminal { pid: None },
                    },
                },
                Surface {
                    id: second_surface,
                    kind: SurfaceKind::Browser {
                        initial_url: Some("https://example.com".into()),
                    },
                    title: "Browser".into(),
                    root_pane: Pane::Leaf {
                        id: PaneId::new(),
                        content: PaneContent::Browser {
                            url: "https://example.com".into(),
                        },
                    },
                },
            ],
            color: None,
        });

        let store = StateStore::new_lazy(state);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.surfaces.len(), 1);
        assert_eq!(first_pane_tab_count(&ws), 2);

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, active },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected migrated tabbed root")
        };
        assert_eq!(*active, second_surface);
        assert!(surfaces.iter().any(|surface| surface.id == second_surface
            && matches!(&surface.kind, SurfaceKind::Browser { .. })));
    }

    #[tokio::test]
    async fn normalizes_legacy_terminal_number_titles_to_cwd_folder() {
        let ws_id = WorkspaceId::new();
        let pane_id = PaneId::new();
        let tab = PaneSurface::terminal("Terminal 3", Some("/tmp/project".into()));
        let tab_id = tab.id;
        let mut state = State::default();
        state.workspaces.push(Workspace {
            id: ws_id,
            name: "legacy".into(),
            custom_title: None,
            root_dir: "/tmp/legacy".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: Some("/tmp/legacy".into()),
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: pane_id,
                    content: PaneContent::Tabs {
                        active: tab_id,
                        surfaces: vec![tab],
                    },
                },
            }],
            color: None,
        });

        let store = StateStore::new_lazy(state);
        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(surfaces[0].title, "project");
        assert!(!surfaces[0].title_locked);
    }

    #[tokio::test]
    async fn resets_stale_terminal_titles_during_normalization_on_load() {
        // The persisted state captures whatever OSC 0/2 the program
        // running inside the tab last set ("Claude Code", "codex …",
        // "vim foo"). On the next launch that program is gone, so the
        // tab title must reset to the cwd-derived form. This test
        // pins the new behavior; a previous version of flowmux
        // auto-locked the stale title and it survived restarts.
        let ws_id = WorkspaceId::new();
        let pane_id = PaneId::new();
        let tab = PaneSurface::terminal("Claude Code", Some("/tmp/one".into()));
        let tab_id = tab.id;
        let mut state = State::default();
        state.workspaces.push(Workspace {
            id: ws_id,
            name: "legacy".into(),
            custom_title: None,
            root_dir: "/tmp/legacy".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: Some("/tmp/legacy".into()),
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: pane_id,
                    content: PaneContent::Tabs {
                        active: tab_id,
                        surfaces: vec![tab],
                    },
                },
            }],
            color: None,
        });

        let store = StateStore::new_lazy(state);
        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(
            surfaces[0].title,
            terminal_tab_title_for_cwd(Some(std::path::Path::new("/tmp/one")))
        );
        assert!(
            !surfaces[0].title_locked,
            "must not auto-lock; the title was never the user's intent"
        );
    }

    #[tokio::test]
    async fn add_surfaces_and_remove_workspace_keep_order_consistent() {
        let store = StateStore::new_lazy(State::default());
        let first = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let second = store
            .create_workspace(Some("two".into()), std::path::PathBuf::from("/tmp/two"))
            .await;

        let terminal = store
            .add_terminal_surface(first, Some("/tmp/one".into()))
            .await;
        let browser = store
            .add_browser_surface(first, "https://example.com".into())
            .await;
        assert!(terminal.is_some());
        assert!(browser.is_some());
        assert_eq!(store.get_workspace(first).await.unwrap().surfaces.len(), 1);
        assert_eq!(
            first_pane_tab_count(&store.get_workspace(first).await.unwrap()),
            3
        );

        assert!(store.remove_workspace(first).await);
        let state = store.snapshot().await;
        assert_eq!(state.workspace_order, vec![second]);
        assert_eq!(state.active_workspace, Some(second));
        assert!(!store.remove_workspace(first).await);
    }

    #[tokio::test]
    async fn remove_all_workspaces_clears_everything() {
        let store = StateStore::new_lazy(State::default());
        let first = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let second = store
            .create_workspace(Some("two".into()), std::path::PathBuf::from("/tmp/two"))
            .await;
        let third = store
            .create_workspace(Some("three".into()), std::path::PathBuf::from("/tmp/three"))
            .await;

        let removed = store.remove_all_workspaces().await;
        assert_eq!(removed, vec![first, second, third]);

        let state = store.snapshot().await;
        assert!(state.workspaces.is_empty());
        assert!(state.workspace_order.is_empty());
        assert_eq!(state.active_workspace, None);

        // Idempotent: a second call on an empty store removes nothing.
        assert!(store.remove_all_workspaces().await.is_empty());
    }

    #[tokio::test]
    async fn close_surface_removes_tab_then_last_pane() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (tab_ws, second_surface) = store
            .add_terminal_surface_to_pane(pane, Some("/tmp/one".into()))
            .await
            .unwrap();
        assert_eq!(tab_ws, ws_id);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(first_pane_tab_count(&ws), 2);
        assert_eq!(first_pane_active_surface(&ws), second_surface);

        let outcome = store.close_surface(pane, second_surface).await.unwrap();
        assert!(matches!(
            outcome,
            CloseOutcome::SurfaceRemoved { workspace } if workspace == ws_id
        ));
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(first_pane_tab_count(&ws), 1);

        let last_surface = first_pane_active_surface(&ws);
        let outcome = store.close_surface(pane, last_surface).await.unwrap();
        assert!(matches!(
            outcome,
            CloseOutcome::WorkspaceRemoved { workspace } if workspace == ws_id
        ));
        assert!(store.snapshot().await.workspaces.is_empty());
    }

    #[tokio::test]
    async fn rename_surface_updates_pane_tab_title() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("one")
        );
        assert_eq!(
            store.rename_surface(pane, surface, "server".into()).await,
            Some(ws_id)
        );
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("server")
        );
    }

    #[tokio::test]
    async fn update_surface_cwd_persists_terminal_tab_cwd() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store
                .update_surface_cwd(pane, surface, "/tmp/two".into())
                .await,
            Some(ws_id)
        );

        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        assert!(matches!(
            &surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &std::path::PathBuf::from("/tmp/two")
        ));
        assert_eq!(surfaces[0].title, "two");
        assert!(!surfaces[0].title_locked);
    }

    #[tokio::test]
    async fn update_surface_cwd_keeps_manually_renamed_tab_title() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);
        let surface = first_pane_active_surface(&ws);

        assert_eq!(
            store.rename_surface(pane, surface, "server".into()).await,
            Some(ws_id)
        );
        assert_eq!(
            store
                .update_surface_cwd(pane, surface, "/tmp/two".into())
                .await,
            Some(ws_id)
        );

        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(surfaces[0].title, "server");
        assert!(surfaces[0].title_locked);
    }

    #[tokio::test]
    async fn add_terminal_surface_defaults_title_to_truncated_cwd_folder() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);

        store
            .add_terminal_surface_to_pane(pane, Some("/tmp/1234567890123456789".into()))
            .await
            .unwrap();

        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, active },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        let active = surfaces
            .iter()
            .find(|surface| surface.id == *active)
            .expect("expected active surface");
        assert_eq!(active.title, "12345678901234567...");
        assert!(!active.title_locked);
    }

    #[tokio::test]
    async fn add_terminal_surface_without_cwd_uses_pane_cwd() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);

        store
            .add_terminal_surface_to_pane(pane, None)
            .await
            .unwrap();

        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, active },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf")
        };
        let active = surfaces
            .iter()
            .find(|surface| surface.id == *active)
            .expect("expected active surface");
        assert_eq!(active.title, "one");
        assert!(matches!(
            &active.kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &std::path::PathBuf::from("/tmp/one")
        ));
    }

    fn collect_leaves(p: &Pane) -> Vec<PaneId> {
        let mut v = Vec::new();
        p.for_each_leaf(|id| v.push(id));
        v
    }

    fn pane_split_direction(p: &Pane) -> Option<SplitDirection> {
        match p {
            Pane::Split { direction, .. } => Some(*direction),
            Pane::Leaf { .. } => None,
        }
    }

    fn find_browser_pane_url(p: &Pane, target: PaneId) -> Option<String> {
        match p {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => surfaces.iter().find_map(|s| match &s.kind {
                    SurfaceKind::Browser { initial_url } => initial_url.clone(),
                    _ => None,
                }),
                PaneContent::Browser { url } => Some(url.clone()),
                PaneContent::Terminal { .. } => None,
            },
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => find_browser_pane_url(first, target)
                .or_else(|| find_browser_pane_url(second, target)),
        }
    }

    #[tokio::test]
    async fn split_pane_with_browser_creates_browser_sibling() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (split_ws, new_pane) = store
            .split_pane_with_browser(
                original,
                SplitDirection::Vertical,
                "https://example.com".into(),
            )
            .await
            .expect("split should succeed for valid target");
        assert_eq!(split_ws, ws_id);
        assert_ne!(new_pane, original);

        let ws = store.get_workspace(ws_id).await.unwrap();
        let leaves = collect_leaves(&ws.surfaces[0].root_pane);
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&original));
        assert!(leaves.contains(&new_pane));

        let url = find_browser_pane_url(&ws.surfaces[0].root_pane, new_pane);
        assert_eq!(url.as_deref(), Some("https://example.com"));
    }

    #[tokio::test]
    async fn split_pane_with_browser_returns_none_for_unknown_target() {
        let store = StateStore::new_lazy(State::default());
        let _ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let bogus = PaneId::new();

        let result = store
            .split_pane_with_browser(bogus, SplitDirection::Vertical, "https://x.test".into())
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn split_pane_with_browser_returns_none_when_no_workspaces() {
        let store = StateStore::new_lazy(State::default());
        let bogus = PaneId::new();
        let result = store
            .split_pane_with_browser(bogus, SplitDirection::Horizontal, "https://x.test".into())
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn split_pane_with_browser_honors_vertical_direction() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        store
            .split_pane_with_browser(
                original,
                SplitDirection::Vertical,
                "https://example.com".into(),
            )
            .await
            .unwrap();

        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(
            pane_split_direction(&ws.surfaces[0].root_pane),
            Some(SplitDirection::Vertical)
        );
    }

    #[tokio::test]
    async fn split_pane_with_browser_honors_horizontal_direction() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        store
            .split_pane_with_browser(
                original,
                SplitDirection::Horizontal,
                "https://example.com".into(),
            )
            .await
            .unwrap();

        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(
            pane_split_direction(&ws.surfaces[0].root_pane),
            Some(SplitDirection::Horizontal)
        );
    }

    #[tokio::test]
    async fn split_pane_with_browser_finds_correct_workspace_among_many() {
        let store = StateStore::new_lazy(State::default());
        let _first = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let middle = store
            .create_workspace(Some("two".into()), std::path::PathBuf::from("/tmp/two"))
            .await;
        let _last = store
            .create_workspace(Some("three".into()), std::path::PathBuf::from("/tmp/three"))
            .await;

        let target = first_pane(&store.get_workspace(middle).await.unwrap());
        let (ws_id, new_pane) = store
            .split_pane_with_browser(
                target,
                SplitDirection::Vertical,
                "https://middle.test".into(),
            )
            .await
            .expect("split should succeed");
        assert_eq!(ws_id, middle);

        // The other workspaces stayed single-leaf.
        for id in [_first, _last] {
            let ws = store.get_workspace(id).await.unwrap();
            assert_eq!(collect_leaves(&ws.surfaces[0].root_pane).len(), 1);
        }

        let middle_ws = store.get_workspace(middle).await.unwrap();
        assert_eq!(collect_leaves(&middle_ws.surfaces[0].root_pane).len(), 2);
        assert_eq!(
            find_browser_pane_url(&middle_ws.surfaces[0].root_pane, new_pane).as_deref(),
            Some("https://middle.test")
        );
    }

    #[tokio::test]
    async fn split_pane_with_browser_preserves_target_leaf_id() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        store
            .split_pane_with_browser(
                original,
                SplitDirection::Vertical,
                "https://example.com".into(),
            )
            .await
            .unwrap();

        // After splitting, the original PaneId is still resolvable via
        // workspace_for_pane — it must still be a leaf in the tree.
        assert_eq!(store.workspace_for_pane(original).await, Some(ws_id));
    }

    #[tokio::test]
    async fn split_pane_with_browser_assigns_unique_pane_ids() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (_, first_new) = store
            .split_pane_with_browser(original, SplitDirection::Vertical, "https://a.test".into())
            .await
            .unwrap();
        let (_, second_new) = store
            .split_pane_with_browser(
                first_new,
                SplitDirection::Horizontal,
                "https://b.test".into(),
            )
            .await
            .unwrap();

        assert_ne!(first_new, original);
        assert_ne!(second_new, original);
        assert_ne!(first_new, second_new);

        let ws = store.get_workspace(ws_id).await.unwrap();
        let leaves = collect_leaves(&ws.surfaces[0].root_pane);
        assert_eq!(leaves.len(), 3);
        assert!(leaves.contains(&original));
        assert!(leaves.contains(&first_new));
        assert!(leaves.contains(&second_new));
    }

    #[tokio::test]
    async fn split_pane_with_browser_browser_pane_uses_initial_url() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let url = "https://docs.example.org/path?q=1#frag";
        let (_, new_pane) = store
            .split_pane_with_browser(original, SplitDirection::Vertical, url.into())
            .await
            .unwrap();

        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Split { second, .. } = &ws.surfaces[0].root_pane else {
            panic!("expected split root after split_pane_with_browser")
        };
        let Pane::Leaf { id, content } = second.as_ref() else {
            panic!("expected new sibling to be a leaf")
        };
        assert_eq!(*id, new_pane);
        let PaneContent::Tabs { surfaces, active } = content else {
            panic!("browser pane content must be tabbed")
        };
        let active_surface = surfaces
            .iter()
            .find(|s| s.id == *active)
            .expect("active surface must exist");
        assert!(matches!(
            &active_surface.kind,
            SurfaceKind::Browser { initial_url: Some(u) } if u == url
        ));
    }

    #[tokio::test]
    async fn add_browser_surface_to_pane_appends_browser_tab_and_activates() {
        // Creating a workspace creates one terminal tab in the first pane. Pressing
        // the browser-tab add button should add a new browser tab to the same
        // pane and make it active.
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());
        assert_eq!(
            first_pane_tab_count(&store.get_workspace(ws_id).await.unwrap()),
            1
        );

        let (returned_ws, browser_surface) = store
            .add_browser_surface_to_pane(pane, "about:blank".into())
            .await
            .expect("browser tab should be added to existing pane");
        assert_eq!(returned_ws, ws_id);

        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(first_pane_tab_count(&ws), 2);
        assert_eq!(first_pane_active_surface(&ws), browser_surface);

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &ws.surfaces[0].root_pane
        else {
            panic!("expected tabbed leaf after add_browser_surface_to_pane")
        };
        let added = surfaces
            .iter()
            .find(|s| s.id == browser_surface)
            .expect("new browser surface must be present");
        assert!(matches!(
            &added.kind,
            SurfaceKind::Browser { initial_url: Some(u) } if u == "about:blank"
        ));
    }

    #[tokio::test]
    async fn add_browser_surface_to_pane_returns_none_for_unknown_pane() {
        let store = StateStore::new_lazy(State::default());
        let _ = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let bogus = PaneId::new();

        let result = store
            .add_browser_surface_to_pane(bogus, "about:blank".into())
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn add_browser_surface_to_pane_targets_correct_pane_after_split() {
        // A newly split sibling pane, not the original pane, should also accept
        // browser tabs without affecting tab counts in other panes.
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (_, sibling) = store
            .split_pane(original, SplitDirection::Vertical)
            .await
            .unwrap();

        let (_, browser_surface) = store
            .add_browser_surface_to_pane(sibling, "https://example.com".into())
            .await
            .expect("browser tab should be added to sibling pane");

        let ws = store.get_workspace(ws_id).await.unwrap();
        let url = find_browser_pane_url(&ws.surfaces[0].root_pane, sibling);
        assert!(
            matches!(url.as_deref(), Some("https://example.com")),
            "sibling should contain the added browser tab"
        );

        let Pane::Split { first, second, .. } = &ws.surfaces[0].root_pane else {
            panic!("expected split after split_pane")
        };
        let leaf_for = |pane_id: PaneId| -> &Pane {
            for candidate in [first.as_ref(), second.as_ref()] {
                if let Pane::Leaf { id, .. } = candidate {
                    if *id == pane_id {
                        return candidate;
                    }
                }
            }
            panic!("pane {pane_id} not found in split tree")
        };
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, active },
            ..
        } = leaf_for(sibling)
        else {
            panic!("sibling pane should be tabbed leaf")
        };
        assert_eq!(*active, browser_surface);
        assert_eq!(surfaces.len(), 2);

        let Pane::Leaf {
            content:
                PaneContent::Tabs {
                    surfaces: orig_surfaces,
                    ..
                },
            ..
        } = leaf_for(original)
        else {
            panic!("original pane should be tabbed leaf")
        };
        assert_eq!(orig_surfaces.len(), 1, "original pane untouched");
    }

    /// Case: add a browser tab to pane A, then add a browser tab to pane B. A's
    /// existing browser surface must preserve id, title, and initial_url. GTK
    /// rerender previously recreated BrowserPane and returned to about:blank,
    /// but daemon state itself should never change, so lock that invariant here.
    /// If add_browser_surface_to_pane regresses and damages another pane, this
    /// catches it.
    #[tokio::test]
    async fn add_browser_to_one_pane_keeps_other_pane_browser_intact() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane_a = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .unwrap();

        // Add an https browser tab to pane A. A hypothetical user-navigated URL
        // lives in the GTK webview while state keeps only initial_url, so verify
        // the newly added surface metadata is preserved.
        let (_, browser_a) = store
            .add_browser_surface_to_pane(pane_a, "https://docs.a.test".into())
            .await
            .unwrap();
        let snap_before = store.get_workspace(ws_id).await.unwrap();
        let surfaces_a_before = pane_surfaces(&snap_before, pane_a);
        let surfaces_b_before = pane_surfaces(&snap_before, pane_b);

        // Add an about:blank browser tab to pane B.
        let (_, browser_b) = store
            .add_browser_surface_to_pane(pane_b, "about:blank".into())
            .await
            .unwrap();
        assert_ne!(browser_a, browser_b);

        let snap_after = store.get_workspace(ws_id).await.unwrap();
        let surfaces_a_after = pane_surfaces(&snap_after, pane_a);
        let surfaces_b_after = pane_surfaces(&snap_after, pane_b);

        // Pane A's surface list keeps the same idx, id, title, and kind.
        assert_eq!(
            fingerprints(&surfaces_a_before),
            fingerprints(&surfaces_a_after),
            "pane A surfaces must not change when pane B gets a new browser tab"
        );
        // Pane B should have exactly one new surface.
        assert_eq!(surfaces_b_before.len() + 1, surfaces_b_after.len());
        assert!(surfaces_b_after
            .iter()
            .any(|s| s.id == browser_b
                && matches!(&s.kind, SurfaceKind::Browser { initial_url: Some(u) } if u == "about:blank")));
    }

    /// Case: adding multiple browser tabs to the same pane preserves metadata
    /// for earlier surfaces and activates the newly added tab.
    #[tokio::test]
    async fn appending_browser_tabs_preserves_earlier_tabs_in_same_pane() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (_, first_browser) = store
            .add_browser_surface_to_pane(pane, "https://one.test".into())
            .await
            .unwrap();
        let (_, second_browser) = store
            .add_browser_surface_to_pane(pane, "https://two.test".into())
            .await
            .unwrap();
        let (_, third_browser) = store
            .add_browser_surface_to_pane(pane, "https://three.test".into())
            .await
            .unwrap();

        let ws = store.get_workspace(ws_id).await.unwrap();
        let surfaces = pane_surfaces(&ws, pane);
        assert_eq!(surfaces.len(), 4); // initial terminal + 3 browsers
        let by_id: std::collections::HashMap<_, _> =
            surfaces.iter().map(|s| (s.id, s.clone())).collect();
        for (id, expected_url) in [
            (first_browser, "https://one.test"),
            (second_browser, "https://two.test"),
            (third_browser, "https://three.test"),
        ] {
            let s = by_id.get(&id).expect("browser surface must still exist");
            assert!(matches!(
                &s.kind,
                SurfaceKind::Browser { initial_url: Some(u) } if u == expected_url
            ));
        }
        // The most recently added tab should be the active surface.
        assert_eq!(first_pane_active_surface(&ws), third_browser);
    }

    /// Case: adding a browser tab must not touch surfaces in another workspace.
    #[tokio::test]
    async fn adding_browser_in_one_workspace_does_not_touch_other_workspaces() {
        let store = StateStore::new_lazy(State::default());
        let alpha = store
            .create_workspace(Some("alpha".into()), std::path::PathBuf::from("/tmp/alpha"))
            .await;
        let beta = store
            .create_workspace(Some("beta".into()), std::path::PathBuf::from("/tmp/beta"))
            .await;
        let pane_alpha = first_pane(&store.get_workspace(alpha).await.unwrap());
        let pane_beta = first_pane(&store.get_workspace(beta).await.unwrap());

        let beta_before = pane_surfaces(&store.get_workspace(beta).await.unwrap(), pane_beta);
        let _ = store
            .add_browser_surface_to_pane(pane_alpha, "https://alpha-only.test".into())
            .await
            .unwrap();
        let beta_after = pane_surfaces(&store.get_workspace(beta).await.unwrap(), pane_beta);
        assert_eq!(fingerprints(&beta_before), fingerprints(&beta_after));
    }

    /// Case: adding a new terminal tab to another pane preserves terminal surface
    /// metadata, especially cwd, in the existing pane.
    #[tokio::test]
    async fn adding_terminal_tab_to_other_pane_keeps_existing_terminal_cwd() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane_a = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .unwrap();

        let surface_a_id = first_pane_active_surface(&store.get_workspace(ws_id).await.unwrap());
        // Simulate the user running cd in the shell by updating pane A terminal cwd.
        assert_eq!(
            store
                .update_surface_cwd(pane_a, surface_a_id, "/tmp/work/inner".into())
                .await,
            Some(ws_id)
        );

        // Now add a new terminal tab to pane B.
        let (_, _new_term) = store
            .add_terminal_surface_to_pane(pane_b, Some("/tmp/other".into()))
            .await
            .unwrap();

        // Pane A's surface should keep cwd /tmp/work/inner.
        let ws = store.get_workspace(ws_id).await.unwrap();
        let surfaces_a = pane_surfaces(&ws, pane_a);
        let s_a = surfaces_a
            .iter()
            .find(|s| s.id == surface_a_id)
            .expect("pane A's terminal surface must still exist");
        assert!(matches!(
            &s_a.kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. }
                if cwd == &std::path::PathBuf::from("/tmp/work/inner")
        ));
    }

    fn pane_surfaces(ws: &Workspace, pane: PaneId) -> Vec<PaneSurface> {
        fn walk(p: &Pane, target: PaneId) -> Option<Vec<PaneSurface>> {
            match p {
                Pane::Leaf { id, content } if *id == target => match content {
                    PaneContent::Tabs { surfaces, .. } => Some(surfaces.clone()),
                    PaneContent::Terminal { .. } | PaneContent::Browser { .. } => Some(vec![]),
                },
                Pane::Leaf { .. } => None,
                Pane::Split { first, second, .. } => {
                    walk(first, target).or_else(|| walk(second, target))
                }
            }
        }
        ws.surfaces
            .iter()
            .find_map(|s| walk(&s.root_pane, pane))
            .unwrap_or_default()
    }

    /// Browser navigation updates a surface's initial_url via update_browser_url,
    /// allowing the next launch to restore the same page. Also verify terminal
    /// surfaces and wrong (pane, surface) pairs are unaffected.
    #[tokio::test]
    async fn update_browser_url_persists_last_navigation_only_for_browser() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let (_, browser) = store
            .add_browser_surface_to_pane(pane, "https://one.test".into())
            .await
            .unwrap();

        // navigate -> reflected in state.
        assert_eq!(
            store
                .update_browser_url(pane, browser, "https://two.test/page?x=1".into())
                .await,
            Some(ws_id)
        );
        let ws = store.get_workspace(ws_id).await.unwrap();
        let updated = ws.surfaces[0]
            .root_pane
            .find_surface(pane, browser)
            .unwrap();
        assert!(matches!(
            &updated.kind,
            SurfaceKind::Browser { initial_url: Some(u) } if u == "https://two.test/page?x=1"
        ));

        // Same URL returns None as a no-op.
        assert_eq!(
            store
                .update_browser_url(pane, browser, "https://two.test/page?x=1".into())
                .await,
            None
        );

        // Terminal surface, the first active surface, is unaffected.
        let terminal_id = first_pane_active_surface(&store.get_workspace(ws_id).await.unwrap());
        // Active may be browser, so find the terminal id explicitly.
        let ws = store.get_workspace(ws_id).await.unwrap();
        let terminal_id = match &ws.surfaces[0].root_pane {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces
                .iter()
                .find(|s| matches!(s.kind, SurfaceKind::Terminal { .. }))
                .map(|s| s.id)
                .unwrap(),
            _ => terminal_id,
        };
        assert_eq!(
            store
                .update_browser_url(pane, terminal_id, "https://nope.test".into())
                .await,
            None
        );
    }

    /// Browser page title signals automatically update surface.title. Surfaces
    /// locked by user rename do not update automatically.
    #[tokio::test]
    async fn update_surface_auto_title_respects_user_rename() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let (_, browser_a) = store
            .add_browser_surface_to_pane(pane, "https://a.test".into())
            .await
            .unwrap();
        let (_, browser_b) = store
            .add_browser_surface_to_pane(pane, "https://b.test".into())
            .await
            .unwrap();

        // A's page title arrives -> updated.
        assert_eq!(
            store
                .update_surface_auto_title(pane, browser_a, "Example A — Home".into())
                .await,
            Some(ws_id)
        );
        assert_eq!(
            store.surface_title(pane, browser_a).await.as_deref(),
            Some("Example A — Home")
        );

        // User names B directly -> automatic updates are ignored.
        store
            .rename_surface(pane, browser_b, "Pinned".into())
            .await
            .unwrap();
        assert_eq!(
            store
                .update_surface_auto_title(pane, browser_b, "B Page".into())
                .await,
            None
        );
        assert_eq!(
            store.surface_title(pane, browser_b).await.as_deref(),
            Some("Pinned")
        );

        // Empty title is ignored.
        assert_eq!(
            store
                .update_surface_auto_title(pane, browser_a, "   ".into())
                .await,
            None
        );
    }

    /// Updating a browser URL in another pane of another workspace must not
    /// change the first workspace's surface data.
    #[tokio::test]
    async fn update_browser_url_in_one_workspace_does_not_touch_others() {
        let store = StateStore::new_lazy(State::default());
        let alpha = store
            .create_workspace(Some("alpha".into()), std::path::PathBuf::from("/tmp/alpha"))
            .await;
        let beta = store
            .create_workspace(Some("beta".into()), std::path::PathBuf::from("/tmp/beta"))
            .await;
        let pane_alpha = first_pane(&store.get_workspace(alpha).await.unwrap());
        let pane_beta = first_pane(&store.get_workspace(beta).await.unwrap());

        let (_, alpha_browser) = store
            .add_browser_surface_to_pane(pane_alpha, "https://alpha.test".into())
            .await
            .unwrap();
        let (_, beta_browser) = store
            .add_browser_surface_to_pane(pane_beta, "https://beta.test".into())
            .await
            .unwrap();

        let _ = store
            .update_browser_url(pane_alpha, alpha_browser, "https://alpha.test/2".into())
            .await;

        let beta_surfaces = pane_surfaces(&store.get_workspace(beta).await.unwrap(), pane_beta);
        let beta_b = beta_surfaces.iter().find(|s| s.id == beta_browser).unwrap();
        assert!(matches!(
            &beta_b.kind,
            SurfaceKind::Browser { initial_url: Some(u) } if u == "https://beta.test"
        ));
    }

    /// `PaneSurface` comes from another crate and does not implement PartialEq.
    /// Extract only the key fields needed for unit-test preservation checks.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SurfaceFingerprint {
        id: SurfaceId,
        title: String,
        title_locked: bool,
        kind: SurfaceKindFingerprint,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum SurfaceKindFingerprint {
        Terminal {
            shell: Option<String>,
            cwd: Option<std::path::PathBuf>,
        },
        Browser {
            initial_url: Option<String>,
        },
    }

    fn fingerprint(s: &PaneSurface) -> SurfaceFingerprint {
        let kind = match &s.kind {
            SurfaceKind::Terminal { shell, cwd } => SurfaceKindFingerprint::Terminal {
                shell: shell.clone(),
                cwd: cwd.clone(),
            },
            SurfaceKind::Browser { initial_url } => SurfaceKindFingerprint::Browser {
                initial_url: initial_url.clone(),
            },
        };
        SurfaceFingerprint {
            id: s.id,
            title: s.title.clone(),
            title_locked: s.title_locked,
            kind,
        }
    }

    fn fingerprints(surfaces: &[PaneSurface]) -> Vec<SurfaceFingerprint> {
        surfaces.iter().map(fingerprint).collect()
    }

    async fn create_named_workspace(store: &StateStore, name: &str) -> WorkspaceId {
        store
            .create_workspace(
                Some(name.into()),
                std::path::PathBuf::from("/tmp").join(name),
            )
            .await
    }

    #[tokio::test]
    async fn reorder_workspace_moves_first_to_last() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let c = create_named_workspace(&store, "c").await;

        assert!(store.reorder_workspace(a, 2).await);

        let order = store.snapshot().await.workspace_order;
        assert_eq!(order, vec![b, c, a]);
    }

    #[tokio::test]
    async fn reorder_workspace_moves_last_to_first() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let c = create_named_workspace(&store, "c").await;

        assert!(store.reorder_workspace(c, 0).await);

        let order = store.snapshot().await.workspace_order;
        assert_eq!(order, vec![c, a, b]);
    }

    #[tokio::test]
    async fn reorder_workspace_moves_middle_within_range() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let c = create_named_workspace(&store, "c").await;
        let d = create_named_workspace(&store, "d").await;

        // Move b to the end (a, c, d, b).
        assert!(store.reorder_workspace(b, 3).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a, c, d, b]);

        // Move d to the front (d, a, c, b).
        assert!(store.reorder_workspace(d, 0).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![d, a, c, b]);
    }

    #[tokio::test]
    async fn reorder_workspace_target_beyond_len_clamps_to_end() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let c = create_named_workspace(&store, "c").await;

        // Even 100 should only move to the end.
        assert!(store.reorder_workspace(a, 100).await);

        let order = store.snapshot().await.workspace_order;
        assert_eq!(order, vec![b, c, a]);
    }

    #[tokio::test]
    async fn reorder_workspace_no_change_returns_false() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let c = create_named_workspace(&store, "c").await;

        // Move to its own position.
        assert!(!store.reorder_workspace(b, 1).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a, b, c]);

        // An out-of-range index that clamps to its own end position returns false.
        assert!(!store.reorder_workspace(c, 100).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a, b, c]);
    }

    #[tokio::test]
    async fn reorder_workspace_unknown_id_returns_false() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;

        let missing = WorkspaceId::new();
        assert!(!store.reorder_workspace(missing, 0).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a, b]);
    }

    #[tokio::test]
    async fn reorder_workspace_single_channel_is_noop() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;

        assert!(!store.reorder_workspace(a, 0).await);
        assert!(!store.reorder_workspace(a, 5).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a]);
    }

    #[tokio::test]
    async fn reorder_workspace_empty_state_returns_false() {
        let store = StateStore::new_lazy(State::default());
        let any = WorkspaceId::new();
        assert!(!store.reorder_workspace(any, 0).await);
    }

    #[tokio::test]
    async fn reorder_workspace_does_not_change_active_workspace() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let _c = create_named_workspace(&store, "c").await;

        // Active starts as the first-created a.
        assert_eq!(store.snapshot().await.active_workspace, Some(a));

        // Moving a to the end should leave active as a.
        assert!(store.reorder_workspace(a, 2).await);
        assert_eq!(store.snapshot().await.active_workspace, Some(a));

        // Order is now [b, c, a]. Moving b to the end still leaves active as a.
        assert!(store.reorder_workspace(b, 2).await);
        assert_eq!(store.snapshot().await.active_workspace, Some(a));
    }

    /// Integrated test for pane-internal terminal/browser tab reorder:
    /// 1. returns the workspace_id in the normal case,
    /// 2. returns None for same-position or missing surfaces,
    /// 3. keeps the active tab on the same SurfaceId after moving.
    #[tokio::test]
    async fn reorder_surface_in_pane_moves_tab_and_keeps_active() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ws".into()), std::path::PathBuf::from("/tmp"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let first = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        // Add the second terminal and third browser tab.
        let (_, second) = store
            .add_terminal_surface_to_pane(pane, Some("/tmp/two".into()))
            .await
            .unwrap();
        let (_, third) = store
            .add_browser_surface_to_pane(pane, "https://three.test".into())
            .await
            .unwrap();
        // Restore the active tab to first.
        store.set_active_surface(pane, first).await;

        // Move first to the last position.
        assert_eq!(
            store.reorder_surface_in_pane(pane, first, 2).await,
            Some(ws_id)
        );
        let snap = store.get_workspace(ws_id).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { active, surfaces },
            ..
        } = &snap.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second, third, first]
        );
        // first moved but remains active.
        assert_eq!(*active, first);

        // Moving to the same end position again returns None.
        assert!(store
            .reorder_surface_in_pane(pane, first, 2)
            .await
            .is_none());

        // Missing SurfaceId returns None.
        assert!(store
            .reorder_surface_in_pane(pane, SurfaceId::new(), 0)
            .await
            .is_none());
    }

    /// target_index beyond length clamps to the end, safely handling callers
    /// that pass drop-position + 1 indexes.
    #[tokio::test]
    async fn reorder_surface_in_pane_clamps_target_index_beyond_len() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ws".into()), std::path::PathBuf::from("/tmp"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let first = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        let (_, second) = store
            .add_terminal_surface_to_pane(pane, None)
            .await
            .unwrap();

        // Move first to 999 -> clamp to the end, index 1.
        assert_eq!(
            store.reorder_surface_in_pane(pane, first, 999).await,
            Some(ws_id)
        );
        let snap = store.get_workspace(ws_id).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { surfaces, .. },
            ..
        } = &snap.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second, first]
        );
    }

    /// Reorder inside one channel/workspace must not affect another channel.
    #[tokio::test]
    async fn reorder_surface_in_pane_does_not_touch_other_workspaces() {
        let store = StateStore::new_lazy(State::default());
        let alpha = store
            .create_workspace(Some("alpha".into()), std::path::PathBuf::from("/tmp/alpha"))
            .await;
        let beta = store
            .create_workspace(Some("beta".into()), std::path::PathBuf::from("/tmp/beta"))
            .await;

        let ws_alpha = store.get_workspace(alpha).await.unwrap();
        let alpha_pane = ws_alpha.surfaces[0].root_pane.first_leaf_id().unwrap();
        let alpha_first = ws_alpha.surfaces[0]
            .root_pane
            .active_surface_id(alpha_pane)
            .unwrap();
        let (_, alpha_second) = store
            .add_terminal_surface_to_pane(alpha_pane, None)
            .await
            .unwrap();

        let ws_beta = store.get_workspace(beta).await.unwrap();
        let beta_pane = ws_beta.surfaces[0].root_pane.first_leaf_id().unwrap();
        let beta_first = ws_beta.surfaces[0]
            .root_pane
            .active_surface_id(beta_pane)
            .unwrap();
        let (_, beta_second) = store
            .add_terminal_surface_to_pane(beta_pane, None)
            .await
            .unwrap();

        // Move alpha pane's first tab to the end.
        assert_eq!(
            store
                .reorder_surface_in_pane(alpha_pane, alpha_first, 1)
                .await,
            Some(alpha)
        );

        // beta stays unchanged.
        let snap_beta = store.get_workspace(beta).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { surfaces, .. },
            ..
        } = &snap_beta.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![beta_first, beta_second]
        );
        // alpha was swapped.
        let snap_alpha = store.get_workspace(alpha).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { surfaces, .. },
            ..
        } = &snap_alpha.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![alpha_second, alpha_first]
        );
    }

    /// Window size and sidebar position setters are blocking, so call them with
    /// spawn_blocking inside a separate tokio runtime and verify there is no
    /// in-memory mutex conflict. Also verify semantic idempotence: repeating the
    /// same value does not trigger mark_dirty.
    #[tokio::test]
    async fn window_layout_setter_persists_value() {
        let store = StateStore::new_lazy(State::default());
        let store_for_blocking = store.clone();
        tokio::task::spawn_blocking(move || {
            store_for_blocking.set_window_layout_blocking(WindowLayout {
                width: 1440,
                height: 900,
                maximized: false,
            });
        })
        .await
        .unwrap();

        let snap = store.snapshot().await;
        assert_eq!(
            snap.window,
            Some(WindowLayout {
                width: 1440,
                height: 900,
                maximized: false,
            })
        );
    }

    #[tokio::test]
    async fn sidebar_position_setter_persists_value() {
        let store = StateStore::new_lazy(State::default());
        let store_for_blocking = store.clone();
        tokio::task::spawn_blocking(move || {
            store_for_blocking.set_sidebar_position_blocking(280);
        })
        .await
        .unwrap();
        assert_eq!(store.snapshot().await.sidebar_position, Some(280));
    }

    /// Pane split ratio setter scenario: normal case, split id missing from the
    /// tree, and same-ratio no-op.
    #[tokio::test]
    async fn pane_split_ratio_setter_updates_only_matching_split() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());
        // split_pane creates a new Split node whose PaneId is the root_pane of
        // the first surface in the workspace tree.
        let _ = store
            .split_pane(original, SplitDirection::Vertical)
            .await
            .unwrap();
        let ws = store.get_workspace(ws_id).await.unwrap();
        let split_id = match &ws.surfaces[0].root_pane {
            Pane::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };

        let store_for_blocking = store.clone();
        let updated = tokio::task::spawn_blocking(move || {
            store_for_blocking.set_pane_split_ratio_blocking(split_id, 0.7)
        })
        .await
        .unwrap();
        assert!(updated);

        let ws = store.get_workspace(ws_id).await.unwrap();
        let Pane::Split { ratio, .. } = &ws.surfaces[0].root_pane else {
            unreachable!()
        };
        assert!((ratio - 0.7).abs() < 0.001);

        // Calling the same ratio again -> false.
        let store_for_blocking = store.clone();
        let again = tokio::task::spawn_blocking(move || {
            store_for_blocking.set_pane_split_ratio_blocking(split_id, 0.7)
        })
        .await
        .unwrap();
        assert!(!again);

        // Unknown split id -> false, tree unchanged.
        let store_for_blocking = store.clone();
        let unknown = tokio::task::spawn_blocking(move || {
            store_for_blocking.set_pane_split_ratio_blocking(PaneId::new(), 0.3)
        })
        .await
        .unwrap();
        assert!(!unknown);
    }

    /// set_workspace_name is the setter the GTK side uses to write the focused
    /// pane's active surface title explicitly into ws.name. Repeating the same
    /// value returns false (no-op).
    /// false (no-op).
    #[tokio::test]
    async fn set_workspace_name_updates_only_on_change() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("auto".into()), std::path::PathBuf::from("/tmp/auto"))
            .await;

        // Different value returns true.
        assert!(store.set_workspace_name(ws_id, "Claude Code".into()).await);
        assert_eq!(
            store.get_workspace(ws_id).await.unwrap().name,
            "Claude Code"
        );

        // Same value returns false.
        assert!(!store.set_workspace_name(ws_id, "Claude Code".into()).await);

        // Unknown workspace returns false.
        assert!(
            !store
                .set_workspace_name(WorkspaceId::new(), "ignored".into())
                .await
        );

        // Even with custom_title locked, set_workspace_name updates only ws.name.
        // custom_title stays as-is, and display_title gives custom priority.
        store.rename_workspace(ws_id, "MyName".into()).await;
        store.set_workspace_name(ws_id, "Updated Auto".into()).await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.name, "Updated Auto");
        assert_eq!(ws.custom_title.as_deref(), Some("MyName"));
        assert_eq!(ws.display_title(), "MyName");
    }

    /// Automatic synchronization is not the daemon's responsibility: surface
    /// updates touch only the surface, and ws.name changes only when GTK knows
    /// focus information and calls set_workspace_name. Regression guard.
    #[tokio::test]
    async fn surface_auto_title_does_not_touch_workspace_name() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("auto".into()), std::path::PathBuf::from("/tmp/auto"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);
        let active = first_pane_active_surface(&ws);

        store
            .update_surface_auto_title(pane, active, "Claude Code".into())
            .await;

        let ws = store.get_workspace(ws_id).await.unwrap();
        // Surface label updates, but ws.name stays unchanged until GTK calls
        // set_workspace_name.
        assert_eq!(ws.name, "auto");
    }

    /// Likewise, cwd changes do not let the daemon mutate ws.name by itself.
    #[tokio::test]
    async fn surface_cwd_does_not_touch_workspace_name() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(
                Some("origin".into()),
                std::path::PathBuf::from("/tmp/origin"),
            )
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = first_pane(&ws);
        let active = first_pane_active_surface(&ws);

        store
            .update_surface_cwd(pane, active, std::path::PathBuf::from("/tmp/elsewhere"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.name, "origin");
    }

    // ----- right-sibling browser reuse (Phase 2) ----------------------

    /// Workspace with a single terminal pane → no right sibling exists,
    /// so `find_right_sibling_browser_leaf` must return `None`. This is
    /// the "first call" leg of the reuse-vs-split decision.
    #[tokio::test]
    async fn right_sibling_lookup_returns_none_for_unsplit_workspace() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let term = first_pane(&store.get_workspace(ws_id).await.unwrap());

        assert_eq!(store.find_right_sibling_browser_leaf(term).await, None);
    }

    /// After `flowmux browser open` once, the workspace looks like
    /// `term | browser`. The next call from the *terminal* pane must
    /// detect the browser as its right sibling.
    #[tokio::test]
    async fn right_sibling_lookup_finds_existing_browser_pane() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let term = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let (_, browser_pane) = store
            .split_pane_with_browser(term, SplitDirection::Vertical, "https://x".into())
            .await
            .expect("split should succeed");

        let found = store.find_right_sibling_browser_leaf(term).await;
        assert_eq!(found, Some(browser_pane));
    }

    /// Two-call scenario: first call hits split path, second call hits
    /// reuse path and adds a tab to the existing browser leaf.
    #[tokio::test]
    async fn two_browser_open_calls_first_splits_then_reuses_right_sibling() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let term = first_pane(&store.get_workspace(ws_id).await.unwrap());

        // First call: no right sibling → split path.
        assert!(store.find_right_sibling_browser_leaf(term).await.is_none());
        let (_, browser_pane) = store
            .split_pane_with_browser(term, SplitDirection::Vertical, "https://a".into())
            .await
            .unwrap();

        // Second call: reuse path. Daemon would call
        // add_browser_surface_to_pane on the right-sibling leaf.
        let reuse = store
            .find_right_sibling_browser_leaf(term)
            .await
            .expect("right sibling must exist after first split");
        assert_eq!(reuse, browser_pane);

        let added = store
            .add_browser_surface_to_pane(reuse, "https://b".into())
            .await;
        assert!(added.is_some(), "second URL should append a tab");

        // The browser pane now hosts two surface tabs (initial + new).
        let ws = store.get_workspace(ws_id).await.unwrap();
        let leaf_tabs = ws.surfaces[0]
            .root_pane
            .find_right_sibling_browser_leaf(term)
            .and_then(|p| {
                fn count_tabs(node: &Pane, target: PaneId) -> Option<usize> {
                    match node {
                        Pane::Leaf { id, content } if *id == target => match content {
                            PaneContent::Tabs { surfaces, .. } => Some(surfaces.len()),
                            _ => None,
                        },
                        Pane::Leaf { .. } => None,
                        Pane::Split { first, second, .. } => {
                            count_tabs(first, target).or_else(|| count_tabs(second, target))
                        }
                    }
                }
                count_tabs(&ws.surfaces[0].root_pane, p)
            });
        assert_eq!(leaf_tabs, Some(2));
    }

    #[tokio::test]
    async fn ephemeral_store_reports_persistence_disabled() {
        let store = StateStore::new_lazy_ephemeral(State::default());
        assert!(
            !store.persist_enabled(),
            "ephemeral stores must not persist to disk"
        );
        // save_now is a no-op on ephemeral stores: it returns Ok
        // without touching the on-disk state.json shared by the
        // lock-owning instance.
        assert!(store.save_now().await.is_ok());
        assert!(store.save_now_blocking().is_ok());

        let normal = StateStore::new_lazy(State::default());
        assert!(
            normal.persist_enabled(),
            "default constructor should persist"
        );
    }

    /// An ephemeral store still accepts mutations and lets them flow
    /// through `mark_dirty` so the rest of the daemon code path stays
    /// uniform — only the disk write is suppressed.
    #[tokio::test]
    async fn ephemeral_store_accepts_mutations_in_memory() {
        let store = StateStore::new_lazy_ephemeral(State::default());
        let id = store
            .create_workspace(Some("ghost".into()), std::path::PathBuf::from("/tmp/ghost"))
            .await;
        let snap = store.snapshot().await;
        assert_eq!(snap.workspaces.len(), 1);
        assert_eq!(snap.workspaces[0].id, id);
    }

    // --- Title-prefix fallback resolver -----------------------------
    //
    // Pins the Flatpak hook recovery path: when a Notify arrives with
    // `pane=None surface=None` because the host->sandbox transition
    // stripped FLOWMUX_PANE_ID, the daemon must rebuild the routing
    // context by matching the notification title against the pane's
    // active tab title (which flowmux flips to the agent name as
    // soon as the agent attaches its PTY).

    #[tokio::test]
    async fn title_prefix_resolver_finds_pane_after_rename() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let surface = first_pane_active_surface(&store.get_workspace(ws_id).await.unwrap());
        // workspace_view::terminal_title_notify renames the active
        // surface to the agent name. Re-create that pre-condition here.
        assert_eq!(
            store.rename_surface(pane, surface, "OpenCode".into()).await,
            Some(ws_id)
        );

        let hit = store
            .find_pane_by_active_title_prefix("OpenCode")
            .await
            .expect("rename must make the pane discoverable by title");
        assert_eq!(hit, (ws_id, pane, surface));
    }

    #[tokio::test]
    async fn title_prefix_resolver_is_case_insensitive() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let pane = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let surface = first_pane_active_surface(&store.get_workspace(ws_id).await.unwrap());
        store.rename_surface(pane, surface, "OPENCODE".into()).await;

        assert!(store
            .find_pane_by_active_title_prefix("opencode")
            .await
            .is_some());
        assert!(store
            .find_pane_by_active_title_prefix("OPENcode")
            .await
            .is_some());
    }

    #[tokio::test]
    async fn title_prefix_resolver_rejects_empty_needle() {
        let store = StateStore::new_lazy(State::default());
        let _ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        // A blank `title.split_whitespace().next()` must not collapse
        // into an everything-matches starts_with on every leaf, or the
        // daemon would attribute random Notifications to whatever pane
        // happens to be first in the workspace list.
        assert!(store.find_pane_by_active_title_prefix("").await.is_none());
    }

    #[tokio::test]
    async fn title_prefix_resolver_returns_none_when_no_pane_matches() {
        let store = StateStore::new_lazy(State::default());
        let _ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        // Title is the default "demo" — Notify("OpenCode ready")
        // must not match it, even after the lowercasing.
        assert!(store
            .find_pane_by_active_title_prefix("OpenCode")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn title_prefix_resolver_walks_split_pane_trees() {
        // Split the workspace into two leaves, only one of which has
        // the agent attached. The resolver must walk into the split
        // tree (both halves) and pick the leaf whose active tab
        // matches.
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());
        let (_, sibling) = store
            .split_pane(original, SplitDirection::Vertical)
            .await
            .expect("split must succeed");

        let original_surface = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(original)
            .unwrap();
        let sibling_surface = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(sibling)
            .unwrap();

        // Rename only the sibling pane's active tab. The resolver must
        // skip `original` and land on `sibling`.
        store
            .rename_surface(sibling, sibling_surface, "OpenCode".into())
            .await;
        let hit = store
            .find_pane_by_active_title_prefix("OpenCode")
            .await
            .unwrap();
        assert_eq!(hit, (ws_id, sibling, sibling_surface));

        // Sanity: `original` is still a valid pane and its active
        // surface title is unchanged.
        let original_ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(
            original_ws.surfaces[0]
                .root_pane
                .surface_title(original, original_surface),
            Some("demo")
        );
    }

    /// Horizontal split (split_down) does not place the browser to the
    /// right — so even though we created one, reuse must not pick it.
    #[tokio::test]
    async fn right_sibling_lookup_ignores_horizontally_split_browser() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let term = first_pane(&store.get_workspace(ws_id).await.unwrap());

        let _ = store
            .split_pane_with_browser(term, SplitDirection::Horizontal, "https://x".into())
            .await
            .unwrap();

        assert!(store.find_right_sibling_browser_leaf(term).await.is_none());
    }

    // ---- move_surface_to_pane / move_surface_to_workspace ----

    async fn pane_tab_ids(store: &StateStore, ws: WorkspaceId, pane: PaneId) -> Vec<SurfaceId> {
        let w = store.get_workspace(ws).await.unwrap();
        for surface in &w.surfaces {
            if let Some(PaneContent::Tabs { surfaces, .. }) =
                surface.root_pane.find_leaf_content(pane)
            {
                return surfaces.iter().map(|s| s.id).collect();
            }
        }
        Vec::new()
    }

    #[tokio::test]
    async fn move_surface_to_pane_appends_to_other_pane_in_same_workspace() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let src = first_pane(&store.get_workspace(ws).await.unwrap());
        let (_, dst) = store
            .split_pane(src, SplitDirection::Vertical)
            .await
            .unwrap();
        // src now has 2 tabs.
        let (_, moved) = store.add_terminal_surface_to_pane(src, None).await.unwrap();

        let out = store
            .move_surface_to_pane(src, moved, dst, usize::MAX)
            .await
            .unwrap();
        assert_eq!(out.dst_workspace, ws);
        assert_eq!(out.src_workspace, ws);
        assert!(!out.src_pane_removed);
        assert!(!out.src_workspace_removed);

        let src_tabs = pane_tab_ids(&store, ws, src).await;
        let dst_tabs = pane_tab_ids(&store, ws, dst).await;
        assert!(!src_tabs.contains(&moved));
        assert_eq!(dst_tabs.last().copied(), Some(moved));
        assert_eq!(dst_tabs.len(), 2);
    }

    #[tokio::test]
    async fn move_surface_to_pane_inserts_at_index() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let src = first_pane(&store.get_workspace(ws).await.unwrap());
        let (_, dst) = store
            .split_pane(src, SplitDirection::Vertical)
            .await
            .unwrap();
        // dst gets a second tab so it has [d0, d1].
        store.add_terminal_surface_to_pane(dst, None).await.unwrap();
        let dst_before = pane_tab_ids(&store, ws, dst).await;
        let (_, moved) = store.add_terminal_surface_to_pane(src, None).await.unwrap();

        store
            .move_surface_to_pane(src, moved, dst, 1)
            .await
            .unwrap();

        let dst_after = pane_tab_ids(&store, ws, dst).await;
        assert_eq!(dst_after, vec![dst_before[0], moved, dst_before[1]]);
    }

    #[tokio::test]
    async fn move_surface_collapses_emptied_source_pane_but_keeps_workspace() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let keep = first_pane(&store.get_workspace(ws).await.unwrap());
        let (_, src) = store
            .split_pane(keep, SplitDirection::Vertical)
            .await
            .unwrap();
        let only = store.get_workspace(ws).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(src)
            .unwrap();

        let out = store
            .move_surface_to_pane(src, only, keep, usize::MAX)
            .await
            .unwrap();
        assert!(out.src_pane_removed);
        assert!(!out.src_workspace_removed);

        let ws_after = store.get_workspace(ws).await.unwrap();
        // src pane is gone; keep pane absorbed the surface (2 tabs).
        assert_eq!(pane_tab_ids(&store, ws, keep).await.len(), 2);
        assert_eq!(ws_after.surfaces[0].root_pane.first_leaf_id(), Some(keep));
    }

    #[tokio::test]
    async fn move_surface_to_workspace_appends_and_removes_empty_source_workspace() {
        let store = StateStore::new_lazy(State::default());
        let ws1 = store
            .create_workspace(Some("one".into()), std::path::PathBuf::from("/tmp/one"))
            .await;
        let ws2 = store
            .create_workspace(Some("two".into()), std::path::PathBuf::from("/tmp/two"))
            .await;
        let src = first_pane(&store.get_workspace(ws1).await.unwrap());
        let moved = store.get_workspace(ws1).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(src)
            .unwrap();
        let dst = first_pane(&store.get_workspace(ws2).await.unwrap());

        let out = store
            .move_surface_to_workspace(src, moved, ws2)
            .await
            .unwrap();
        assert_eq!(out.dst_workspace, ws2);
        assert_eq!(out.src_workspace, ws1);
        assert!(out.src_workspace_removed);

        assert!(store.get_workspace(ws1).await.is_none());
        let dst_tabs = pane_tab_ids(&store, ws2, dst).await;
        assert_eq!(dst_tabs.len(), 2);
        assert_eq!(dst_tabs.last().copied(), Some(moved));
    }

    #[tokio::test]
    async fn move_surface_missing_surface_returns_none() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let src = first_pane(&store.get_workspace(ws).await.unwrap());
        let (_, dst) = store
            .split_pane(src, SplitDirection::Vertical)
            .await
            .unwrap();
        assert!(store
            .move_surface_to_pane(src, SurfaceId::new(), dst, 0)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn move_surface_to_missing_dest_keeps_surface() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let src = first_pane(&store.get_workspace(ws).await.unwrap());
        let moved = store.get_workspace(ws).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(src)
            .unwrap();
        assert!(store
            .move_surface_to_pane(src, moved, PaneId::new(), 0)
            .await
            .is_none());
        // Surface still present in the source pane.
        assert!(pane_tab_ids(&store, ws, src).await.contains(&moved));
    }

    // ---- split_surface_into_pane ----

    #[tokio::test]
    async fn split_surface_into_pane_creates_sibling_with_moved_tab() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let dst = first_pane(&store.get_workspace(ws).await.unwrap());
        // Give the source its own pane (so dst is untouched) with a tab to move.
        let (_, src) = store
            .split_pane(dst, SplitDirection::Vertical)
            .await
            .unwrap();
        let (_, moved) = store.add_terminal_surface_to_pane(src, None).await.unwrap();

        let out = store
            .split_surface_into_pane(src, moved, dst, SplitDirection::Horizontal)
            .await
            .unwrap();
        assert_eq!(out.dst_workspace, ws);
        assert!(!out.src_pane_removed);

        // The new sibling pane holds exactly the moved tab.
        assert_eq!(pane_tab_ids(&store, ws, out.new_pane).await, vec![moved]);
        assert!(!pane_tab_ids(&store, ws, src).await.contains(&moved));
        // dst keeps its original tab; the new split sits next to dst.
        let w = store.get_workspace(ws).await.unwrap();
        assert!(w.surfaces[0]
            .root_pane
            .parent_split_id(out.new_pane)
            .is_some());
    }

    #[tokio::test]
    async fn import_surface_to_pane_assigns_new_id_and_activates() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("ws".into()), std::path::PathBuf::from("/tmp/ws"))
            .await;
        let pane = first_pane(&store.get_workspace(ws).await.unwrap());
        let imported = PaneSurface::browser("Remote", "https://remote.test".into());
        let old_id = imported.id;

        let (dst_ws, new_id) = store
            .import_surface_to_pane(pane, imported, usize::MAX)
            .await
            .unwrap();

        assert_eq!(dst_ws, ws);
        assert_ne!(new_id, old_id);
        assert_eq!(
            store.get_workspace(ws).await.unwrap().surfaces[0]
                .root_pane
                .active_surface_id(pane),
            Some(new_id)
        );
        assert_eq!(
            store.surface_title(pane, new_id).await.as_deref(),
            Some("Remote")
        );
    }

    #[tokio::test]
    async fn split_imported_surface_into_pane_creates_sibling() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("ws".into()), std::path::PathBuf::from("/tmp/ws"))
            .await;
        let dst = first_pane(&store.get_workspace(ws).await.unwrap());
        let imported = PaneSurface::terminal("Remote shell", Some("/tmp/remote".into()));

        let (dst_ws, new_pane, new_surface) = store
            .split_imported_surface_into_pane(dst, imported, SplitDirection::Horizontal)
            .await
            .unwrap();

        assert_eq!(dst_ws, ws);
        assert_eq!(pane_tab_ids(&store, ws, new_pane).await, vec![new_surface]);
        let w = store.get_workspace(ws).await.unwrap();
        assert!(w.surfaces[0].root_pane.parent_split_id(new_pane).is_some());
    }

    #[tokio::test]
    async fn split_surface_into_pane_collapses_emptied_source() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let dst = first_pane(&store.get_workspace(ws).await.unwrap());
        let (_, src) = store
            .split_pane(dst, SplitDirection::Vertical)
            .await
            .unwrap();
        let only = store.get_workspace(ws).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(src)
            .unwrap();

        let out = store
            .split_surface_into_pane(src, only, dst, SplitDirection::Horizontal)
            .await
            .unwrap();
        assert!(out.src_pane_removed);
        assert!(!out.src_workspace_removed);
        assert_eq!(pane_tab_ids(&store, ws, out.new_pane).await, vec![only]);
    }

    #[tokio::test]
    async fn split_surface_into_missing_dest_keeps_surface() {
        let store = StateStore::new_lazy(State::default());
        let ws = store
            .create_workspace(Some("w".into()), std::path::PathBuf::from("/tmp/w"))
            .await;
        let src = first_pane(&store.get_workspace(ws).await.unwrap());
        let moved = store.get_workspace(ws).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(src)
            .unwrap();
        assert!(store
            .split_surface_into_pane(src, moved, PaneId::new(), SplitDirection::Vertical)
            .await
            .is_none());
        assert!(pane_tab_ids(&store, ws, src).await.contains(&moved));
    }
}
