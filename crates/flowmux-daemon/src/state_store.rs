// SPDX-License-Identifier: GPL-3.0-or-later
//! In-memory state with debounced disk persistence.
//!
//! Every mutation goes through this store, which writes to
//! `$XDG_STATE_HOME/flowmux/state.json` after a short debounce so we
//! never block the event loop on fsync. State load is synchronous on
//! boot.

use flowmux_core::{
    terminal_tab_title_for_cwd, CloseSurfaceOutcome, Pane, PaneContent, PaneId, PaneSurface,
    RemoveOutcome, SplitDirection, Surface, SurfaceId, SurfaceKind, Workspace, WorkspaceId,
};
use flowmux_state::State;
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

#[derive(Clone)]
pub struct StateStore {
    inner: Arc<Mutex<State>>,
    dirty: Arc<Notify>,
}

impl StateStore {
    /// Construct from inside a tokio runtime context. Spawns the
    /// persistence loop on the current runtime.
    pub fn new(initial: State) -> Self {
        let mut initial = initial;
        let normalized = normalize_state(&mut initial);
        let store = Self {
            inner: Arc::new(Mutex::new(initial)),
            dirty: Arc::new(Notify::new()),
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
            dirty: Arc::new(Notify::new()),
        };
        if normalized {
            store.mark_dirty();
        }
        store
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
        let ws = Workspace {
            id,
            name: name.unwrap_or_else(|| {
                root.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("workspace")
                    .to_string()
            }),
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
                        let ws_id = s.workspaces[ws_idx].id;
                        drop(s);
                        self.mark_dirty();
                        return Some(CloseOutcome::PaneRemoved { workspace: ws_id });
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
                    drop(s);
                    self.mark_dirty();
                    return Some(CloseOutcome::WorkspaceRemoved { workspace: ws_id });
                }
                drop(s);
                self.mark_dirty();
                return Some(CloseOutcome::SurfaceRemoved { workspace: ws_id });
            }
        }
        None
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

    /// Rename a workspace. Returns true on success.
    pub async fn rename_workspace(&self, id: WorkspaceId, name: String) -> bool {
        let mut s = self.inner.lock().await;
        let mut renamed = false;
        if let Some(w) = s.workspaces.iter_mut().find(|w| w.id == id) {
            w.name = name;
            renamed = true;
        }
        drop(s);
        if renamed {
            self.mark_dirty();
        }
        renamed
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

    pub async fn workspace_for_pane(&self, pane: PaneId) -> Option<WorkspaceId> {
        let s = self.inner.lock().await;
        for ws in &s.workspaces {
            for surface in &ws.surfaces {
                let mut found = None;
                surface.root_pane.for_each_leaf(|id| {
                    if id == pane {
                        found = Some(ws.id);
                    }
                });
                if found.is_some() {
                    return found;
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
            for surface in ws.surfaces.iter_mut() {
                if surface.root_pane.set_active_surface(pane, surface_id) {
                    let ws_id = ws.id;
                    drop(s);
                    self.mark_dirty();
                    return Some(ws_id);
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
            let snap = self.snapshot().await;
            match flowmux_state::save(&snap) {
                Ok(()) => info!(workspaces = snap.workspaces.len(), "state persisted"),
                Err(e) => error!(error = %e, "state save failed"),
            }
        }
    }

    pub async fn save_now(&self) -> Result<(), flowmux_state::StateError> {
        let snap = self.snapshot().await;
        flowmux_state::save(&snap)
    }

    pub fn save_now_blocking(&self) -> Result<(), flowmux_state::StateError> {
        let snap = self.inner.blocking_lock().clone();
        flowmux_state::save(&snap)
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

        assert!(store.rename_workspace(ws_id, "new".into()).await);
        assert!(store.set_workspace_color(ws_id, "#112233".into()).await);
        assert!(!store.rename_workspace(missing, "missing".into()).await);
        assert!(!store.set_workspace_color(missing, "#445566".into()).await);
        store.set_active_workspace(Some(missing)).await;

        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.name, "new");
        assert_eq!(ws.color.as_deref(), Some("#112233"));
        assert_eq!(store.snapshot().await.active_workspace, Some(ws_id));
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
    async fn locks_legacy_custom_terminal_titles_during_normalization() {
        let ws_id = WorkspaceId::new();
        let pane_id = PaneId::new();
        let tab = PaneSurface::terminal("server", Some("/tmp/one".into()));
        let tab_id = tab.id;
        let mut state = State::default();
        state.workspaces.push(Workspace {
            id: ws_id,
            name: "legacy".into(),
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
        assert_eq!(
            store
                .update_surface_cwd(pane_id, tab_id, "/tmp/two".into())
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
        assert!(matches!(
            &surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &std::path::PathBuf::from("/tmp/two")
        ));
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
            .add_terminal_surface_to_pane(pane, Some("/tmp/1234567890123456".into()))
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
        assert_eq!(active.title, "123456789012345...");
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
}
