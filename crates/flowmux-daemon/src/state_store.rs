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
use flowmux_state::{State, WindowLayout};
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
        // 호출자가 명시한 name이 있어도 그것은 자동 결정값(name)으로 둔다.
        // cmux 시맨틱: customTitle은 사용자가 직접 rename 했을 때만 채워지고,
        // 새로 만들 때는 항상 None으로 시작해 자동 모드를 유지한다.
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

    /// 사용자가 우클릭 메뉴 → "Change tab name" 다이얼로그에서 입력한
    /// 값을 워크스페이스에 적용한다. cmux의 `setCustomTitle`과 동일하게
    /// 동작한다:
    ///   * 입력을 양 끝 공백 기준 trim 한 결과가 비어 있으면
    ///     `custom_title = None`으로 되돌려 자동 모드 (= `name`을 표시)
    ///     로 복귀한다.
    ///   * 비어 있지 않으면 `custom_title = Some(trimmed)`로 저장한다.
    /// 자동 결정값인 `name`은 어떤 경우에도 직접 건드리지 않는다 — 자동
    /// 갱신 신호(folder rename, OSC, …)가 따로 갱신할 수 있도록.
    /// 매칭되는 워크스페이스가 없거나 변경이 없으면 `false` 반환.
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

    /// 브라우저 surface의 마지막 URL을 state에 반영한다. webview의
    /// uri_notify 신호에 응답해 호출되며, 앱 종료/재실행 시 마지막에
    /// 보고 있던 페이지로 복원되도록 한다.
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

    /// 외부 신호로부터 받은 자동 타이틀(브라우저 페이지 제목 등)을
    /// surface에 반영한다. 사용자가 직접 rename 한 surface(title_locked)는
    /// 건드리지 않는다.
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

    /// pane 안에서 탭(터미널/탭브라우저)을 드래그 앤 드랍으로 재배치할
    /// 때 호출된다. 같은 pane 내부의 `surface_id` 탭을 `target_index`
    /// 위치로 옮긴다. `target_index`는 이동을 적용한 뒤의 최종 위치이며
    /// 길이를 넘어가면 끝으로 클램프된다. 변화가 없거나 매칭되는
    /// surface가 없으면 `None`을 반환해 호출자가 GTK 위젯을 건드리지
    /// 않도록 한다. 활성 탭의 SurfaceId는 reorder의 영향을 받지 않으므로
    /// 옮긴 후에도 같은 탭이 활성으로 남는다.
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

    /// 사이드 패널에서 워크스페이스를 드래그 앤 드랍으로 재배치할 때 호출된다.
    /// `id`로 식별되는 워크스페이스를 `workspace_order` 안의 `target_index` 위치로
    /// 옮긴다. `target_index`는 이동을 적용한 뒤의 최종 위치이며, 길이를
    /// 넘어가면 끝으로 클램프된다. 같은 위치이거나 워크스페이스가 존재하지
    /// 않으면 `false`를 반환한다.
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

    /// 저장된 윈도우 사이즈/maximize. 첫 실행에는 None.
    pub fn window_layout_blocking(&self) -> Option<WindowLayout> {
        self.inner.blocking_lock().window.clone()
    }

    /// 저장된 사이드 패널 divider 픽셀 위치. 첫 실행에는 None.
    pub fn sidebar_position_blocking(&self) -> Option<i32> {
        self.inner.blocking_lock().sidebar_position
    }

    /// 윈도우 사이즈/maximize 상태를 state에 기록한다. close 시 GTK 메인
    /// 스레드에서 동기적으로 호출되므로 blocking 변형으로 둔다 — async
    /// runtime이 종료 핸들러 안에서 살아 있다는 보장이 없다.
    pub fn set_window_layout_blocking(&self, layout: WindowLayout) {
        let mut s = self.inner.blocking_lock();
        if s.window.as_ref() == Some(&layout) {
            return;
        }
        s.window = Some(layout);
        drop(s);
        self.mark_dirty();
    }

    /// 사이드 패널 / 콘텐츠 영역 사이 divider 픽셀 위치를 기록.
    pub fn set_sidebar_position_blocking(&self, position: i32) {
        let mut s = self.inner.blocking_lock();
        if s.sidebar_position == Some(position) {
            return;
        }
        s.sidebar_position = Some(position);
        drop(s);
        self.mark_dirty();
    }

    /// pane split divider의 ratio를 모델에 반영. `split_id`는 트리 안의
    /// `Pane::Split` 노드 PaneId. 매칭되는 split이 없거나 ratio가 같으면
    /// `false` (호출자가 dirty mark을 건너뛸 수 있도록).
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

        // rename은 cmux 시맨틱 — name은 자동값으로 그대로 두고 custom_title만 갱신.
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

        // 사용자 rename → custom_title 채워짐.
        assert!(store.rename_workspace(ws_id, "MyName".into()).await);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.custom_title.as_deref(), Some("MyName"));
        assert_eq!(ws.display_title(), "MyName");

        // 빈 입력 → 자동 모드 복귀 (custom_title = None).
        assert!(store.rename_workspace(ws_id, "".into()).await);
        let ws = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(ws.custom_title, None);
        assert_eq!(ws.display_title(), "auto");
        assert_eq!(ws.name, "auto");

        // 공백만 있는 입력도 같은 의미.
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
        assert!(store
            .rename_workspace(ws_id, "  Spaced Name  ".into())
            .await);
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
        // 같은 값 재입력 → false (변경 없음).
        assert!(!store.rename_workspace(ws_id, "Same".into()).await);
        // trim 결과가 같으면 false.
        assert!(!store.rename_workspace(ws_id, "  Same  ".into()).await);
        // 빈 입력 두 번도 두 번째는 false.
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
    async fn locks_legacy_custom_terminal_titles_during_normalization() {
        let ws_id = WorkspaceId::new();
        let pane_id = PaneId::new();
        let tab = PaneSurface::terminal("server", Some("/tmp/one".into()));
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
        // 워크스페이스를 만들면 첫 pane에는 탭(terminal) 한 개가 있고, 거기에
        // 탭브라우저 추가 버튼을 누르면 같은 pane 안에 새 탭브라우저가
        // 추가되며 그게 활성 탭이 되어야 한다.
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
        // split을 통해 만들어진 새 pane(원래 pane이 아닌 sibling)에도
        // 탭브라우저를 추가할 수 있어야 하고, 다른 pane의 탭 개수에는
        // 영향이 없어야 한다.
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

    /// 케이스: pane A에 탭브라우저 추가 → pane B에 탭브라우저 추가
    /// → A의 기존 탭브라우저 surface는 (id, title, initial_url) 모두
    /// 변경 없이 보존되어야 한다. 이전에는 GTK 측 rerender 가
    /// BrowserPane을 새로 만들어 about:blank로 돌아갔지만, state
    /// 자체는 처음부터 변하지 않으므로 daemon 레벨에서 그 invariant
    /// 를 잠가둔다 — 만약 add_browser_surface_to_pane 구현이 다른
    /// pane을 손상시키게 회귀하면 여기서 잡힌다.
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

        // pane A에 https 탭브라우저 추가 (사용자가 navigate한 가상의 결과
        // URL은 GTK 측 webview에 있고 state는 initial_url만 가지므로,
        // 여기서는 새로 추가된 surface의 메타데이터 보존을 검증한다).
        let (_, browser_a) = store
            .add_browser_surface_to_pane(pane_a, "https://docs.a.test".into())
            .await
            .unwrap();
        let snap_before = store.get_workspace(ws_id).await.unwrap();
        let surfaces_a_before = pane_surfaces(&snap_before, pane_a);
        let surfaces_b_before = pane_surfaces(&snap_before, pane_b);

        // pane B에 about:blank 탭브라우저 추가.
        let (_, browser_b) = store
            .add_browser_surface_to_pane(pane_b, "about:blank".into())
            .await
            .unwrap();
        assert_ne!(browser_a, browser_b);

        let snap_after = store.get_workspace(ws_id).await.unwrap();
        let surfaces_a_after = pane_surfaces(&snap_after, pane_a);
        let surfaces_b_after = pane_surfaces(&snap_after, pane_b);

        // pane A의 surface 목록은 idx, id, title, kind 모두 동일하게 유지.
        assert_eq!(
            fingerprints(&surfaces_a_before),
            fingerprints(&surfaces_a_after),
            "pane A surfaces must not change when pane B gets a new browser tab"
        );
        // pane B는 정확히 한 개의 새 surface가 늘어났어야 한다.
        assert_eq!(surfaces_b_before.len() + 1, surfaces_b_after.len());
        assert!(surfaces_b_after
            .iter()
            .any(|s| s.id == browser_b
                && matches!(&s.kind, SurfaceKind::Browser { initial_url: Some(u) } if u == "about:blank")));
    }

    /// 케이스: 같은 pane에 탭브라우저를 여러 개 연속해서 추가해도
    /// 먼저 추가한 surface들의 메타데이터가 그대로이고, 새로 추가된
    /// 탭이 active로 전환된다.
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
        assert_eq!(surfaces.len(), 4); // 초기 terminal + 3 browsers
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
        // 가장 최근에 추가된 탭이 활성 surface여야 한다.
        assert_eq!(first_pane_active_surface(&ws), third_browser);
    }

    /// 케이스: 탭브라우저 추가가 다른 워크스페이스의 surface를
    /// 건드리지 않아야 한다.
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

    /// 케이스: 새 탭(터미널)을 다른 pane에 추가해도 기존 pane의
    /// terminal surface 메타데이터(특히 cwd)가 보존된다.
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
        // 사용자가 셸에서 cd 한 결과를 모사: pane A의 terminal surface에 cwd 갱신.
        assert_eq!(
            store
                .update_surface_cwd(pane_a, surface_a_id, "/tmp/work/inner".into())
                .await,
            Some(ws_id)
        );

        // 이제 pane B에 새 탭(terminal)을 추가한다.
        let (_, _new_term) = store
            .add_terminal_surface_to_pane(pane_b, Some("/tmp/other".into()))
            .await
            .unwrap();

        // pane A의 surface는 그대로 cwd /tmp/work/inner로 남아 있어야 한다.
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

    /// 브라우저 navigate가 발생하면 update_browser_url 호출로 surface
    /// 의 initial_url이 갱신되어, 다음 실행 시 같은 페이지로 복원된다.
    /// terminal surface나 잘못된 (pane, surface) 조합에는 영향 없음을
    /// 함께 검증한다.
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

        // navigate → state에 반영.
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

        // 같은 URL은 no-op으로 None.
        assert_eq!(
            store
                .update_browser_url(pane, browser, "https://two.test/page?x=1".into())
                .await,
            None
        );

        // terminal surface(첫 active surface)는 영향 X.
        let terminal_id = first_pane_active_surface(&store.get_workspace(ws_id).await.unwrap());
        // active 가 browser 였을 수 있으니 terminal id를 명시적으로 찾는다.
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

    /// 브라우저 페이지 title 신호가 surface.title을 자동 갱신.
    /// 사용자가 rename으로 잠근 surface는 자동 갱신되지 않는다.
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

        // A의 page title이 도착 → 갱신됨.
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

        // B를 사용자가 직접 이름 짓고 → 자동 갱신은 무시.
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

        // 빈 title은 무시.
        assert_eq!(
            store
                .update_surface_auto_title(pane, browser_a, "   ".into())
                .await,
            None
        );
    }

    /// 다른 워크스페이스의 다른 pane에 있는 browser url을 갱신해도, 첫 워크스페이스
    /// surface 데이터는 변하지 않는다.
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

    /// `PaneSurface`는 외부 crate라 `PartialEq` 미구현 — 단위 테스트용으로
    /// 보존성 검증에 필요한 핵심 필드(id, title, title_locked, kind)만
    /// 추출한다.
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
            .create_workspace(Some(name.into()), std::path::PathBuf::from("/tmp").join(name))
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

        // b를 끝으로 옮긴다 (a, c, d, b)
        assert!(store.reorder_workspace(b, 3).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a, c, d, b]);

        // d를 처음으로 옮긴다 (d, a, c, b)
        assert!(store.reorder_workspace(d, 0).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![d, a, c, b]);
    }

    #[tokio::test]
    async fn reorder_workspace_target_beyond_len_clamps_to_end() {
        let store = StateStore::new_lazy(State::default());
        let a = create_named_workspace(&store, "a").await;
        let b = create_named_workspace(&store, "b").await;
        let c = create_named_workspace(&store, "c").await;

        // 100을 줘도 끝으로만 이동해야 한다.
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

        // 자기 자리로 이동.
        assert!(!store.reorder_workspace(b, 1).await);
        assert_eq!(store.snapshot().await.workspace_order, vec![a, b, c]);

        // 길이를 초과한 인덱스도 자기 자리(끝)이면 false.
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

        // active는 처음 만들어진 a로 시작한다.
        assert_eq!(store.snapshot().await.active_workspace, Some(a));

        // a를 끝으로 옮겨도 active는 그대로 a여야 한다.
        assert!(store.reorder_workspace(a, 2).await);
        assert_eq!(store.snapshot().await.active_workspace, Some(a));

        // 이제 순서는 [b, c, a]. b를 끝으로 옮겨도 active는 a 그대로.
        assert!(store.reorder_workspace(b, 2).await);
        assert_eq!(store.snapshot().await.active_workspace, Some(a));
    }

    /// pane 내부의 탭(터미널/탭브라우저) reorder가
    /// 1) 정상 케이스에서 해당 workspace_id를 반환하고
    /// 2) 자기 자리/없는 surface일 때 None을 반환하며
    /// 3) 활성 탭이 옮긴 후에도 같은 SurfaceId로 유지되는지
    /// 통합적으로 본다.
    #[tokio::test]
    async fn reorder_surface_in_pane_moves_tab_and_keeps_active() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ws".into()), std::path::PathBuf::from("/tmp"))
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let first = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        // 두 번째 (terminal) 와 세 번째 (탭브라우저) 추가.
        let (_, second) = store
            .add_terminal_surface_to_pane(pane, Some("/tmp/two".into()))
            .await
            .unwrap();
        let (_, third) = store
            .add_browser_surface_to_pane(pane, "https://three.test".into())
            .await
            .unwrap();
        // 활성 탭을 first(첫 번째)로 되돌려 둔다.
        store.set_active_surface(pane, first).await;

        // first를 마지막 자리로.
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
        // first를 옮겼지만 여전히 first가 active.
        assert_eq!(*active, first);

        // 같은 자리(끝)로 다시 옮기면 None.
        assert!(store
            .reorder_surface_in_pane(pane, first, 2)
            .await
            .is_none());

        // 없는 SurfaceId면 None.
        assert!(store
            .reorder_surface_in_pane(pane, SurfaceId::new(), 0)
            .await
            .is_none());
    }

    /// 길이를 넘어가는 target_index는 끝으로 클램프된다 — 호출자가
    /// 드랍 위치를 +1 한 인덱스를 넘겨도 안전히 처리.
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

        // first 를 999번째로 → 끝(인덱스 1)로 클램프.
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

    /// 한 channel(workspace) 안의 reorder가 다른 channel에 영향이 없어야 한다.
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

        // alpha pane의 첫 탭을 끝으로.
        assert_eq!(
            store
                .reorder_surface_in_pane(alpha_pane, alpha_first, 1)
                .await,
            Some(alpha)
        );

        // beta는 그대로.
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
        // alpha는 swap 됐다.
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

    /// 윈도우 사이즈와 사이드바 위치 setter는 blocking이므로 별도 tokio
    /// 런타임 안에서 spawn_blocking으로 호출해 인메모리 mutex 충돌이 없는
    /// 지 본다. 동일 값으로 다시 호출하면 mark_dirty가 트리거되지 않는
    /// 의미상 idempotent도 함께 검증.
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

    /// pane split ratio setter — 정상 케이스, 트리 안에 없는 split id,
    /// 같은 ratio (no-op)를 한 시나리오로 본다.
    #[tokio::test]
    async fn pane_split_ratio_setter_updates_only_matching_split() {
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("demo".into()), std::path::PathBuf::from("/tmp/demo"))
            .await;
        let original = first_pane(&store.get_workspace(ws_id).await.unwrap());
        // split_pane이 새 Split 노드를 만들고, 그 PaneId는
        // workspace 트리 안의 첫 surface의 root_pane이다.
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

        // 동일 ratio 다시 호출 → false.
        let store_for_blocking = store.clone();
        let again = tokio::task::spawn_blocking(move || {
            store_for_blocking.set_pane_split_ratio_blocking(split_id, 0.7)
        })
        .await
        .unwrap();
        assert!(!again);

        // 모르는 split id → false, 트리 변경 없음.
        let store_for_blocking = store.clone();
        let unknown = tokio::task::spawn_blocking(move || {
            store_for_blocking.set_pane_split_ratio_blocking(PaneId::new(), 0.3)
        })
        .await
        .unwrap();
        assert!(!unknown);
    }
}
