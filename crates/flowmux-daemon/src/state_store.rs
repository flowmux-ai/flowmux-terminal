// SPDX-License-Identifier: GPL-3.0-or-later
//! In-memory state with debounced disk persistence.
//!
//! Every mutation goes through this store, which writes to
//! `$XDG_STATE_HOME/flowmux/state.json` after a short debounce so we
//! never block the event loop on fsync. State load is synchronous on
//! boot.

use flowmux_core::{
    Pane, PaneContent, PaneId, RemoveOutcome, SplitDirection, Surface, SurfaceId, SurfaceKind,
    Workspace, WorkspaceId,
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
    "#7ab7e6", "#e69977", "#9ad57a", "#d188e0", "#e6d077",
    "#7adfd0", "#e07a9a", "#a797e0", "#79e0a3", "#e07a7a",
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
        let store = Self {
            inner: Arc::new(Mutex::new(initial)),
            dirty: Arc::new(Notify::new()),
        };
        let bg = store.clone();
        tokio::spawn(async move { bg.persist_loop().await });
        store
    }

    /// Construct without entering a tokio context. Caller must spawn
    /// [`StateStore::persist_loop`] on the runtime themselves. Useful
    /// from the GTK main thread before the runtime is fully wired.
    pub fn new_lazy(initial: State) -> Self {
        Self {
            inner: Arc::new(Mutex::new(initial)),
            dirty: Arc::new(Notify::new()),
        }
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
                    cwd: Some(root),
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: pane_id,
                    content: PaneContent::Terminal { pid: None },
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

    pub async fn replace_listening_ports(
        &self,
        workspace: WorkspaceId,
        ports: Vec<u16>,
    ) {
        let mut s = self.inner.lock().await;
        if let Some(w) = s.workspaces.iter_mut().find(|w| w.id == workspace) {
            w.listening_ports = ports;
        }
        drop(s);
        self.mark_dirty();
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
                if let Some(new_id) = surface.root_pane.split_leaf(
                    target,
                    direction,
                    0.5,
                    PaneContent::Terminal { pid: None },
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
                        content: PaneContent::Terminal { pid: None },
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
        s.active_workspace.or_else(|| s.workspaces.first().map(|w| w.id))
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
        let surface_id = SurfaceId::new();
        let pane_id = PaneId::new();
        w.surfaces.push(Surface {
            id: surface_id,
            kind: SurfaceKind::Terminal { shell: None, cwd },
            title: format!("surface-{}", w.surfaces.len() + 1),
            root_pane: Pane::Leaf {
                id: pane_id,
                content: PaneContent::Terminal { pid: None },
            },
        });
        drop(s);
        self.mark_dirty();
        Some(surface_id)
    }

    /// Add a browser surface to a workspace and return its id.
    pub async fn add_browser_surface(
        &self,
        workspace: WorkspaceId,
        url: String,
    ) -> Option<SurfaceId> {
        let mut s = self.inner.lock().await;
        let w = s.workspaces.iter_mut().find(|w| w.id == workspace)?;
        let surface_id = SurfaceId::new();
        let pane_id = PaneId::new();
        w.surfaces.push(Surface {
            id: surface_id,
            kind: SurfaceKind::Browser { initial_url: Some(url.clone()) },
            title: "Browser".into(),
            root_pane: Pane::Leaf {
                id: pane_id,
                content: PaneContent::Browser { url },
            },
        });
        drop(s);
        self.mark_dirty();
        Some(surface_id)
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
}
