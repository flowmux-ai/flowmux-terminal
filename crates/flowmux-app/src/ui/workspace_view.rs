// SPDX-License-Identifier: GPL-3.0-or-later
//! Render a workspace's pane tree as recursive GTK widgets.
//!
//! A split node becomes `gtk::Paned`; a leaf pane becomes a framed
//! cmux-style pane with a surface tab bar and a stack of terminal or
//! browser panels.

use crate::theme::ResolvedTheme;
use crate::ui::browser_pane::BrowserPane;
use crate::ui::terminal_pane::{PaneCallbacks, TerminalPane};
use flowmux_core::{
    terminal_tab_title_for_cwd, Pane, PaneContent, PaneId, PaneSurface, SplitDirection, Surface,
    SurfaceId, SurfaceKind, WorkspaceId,
};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vte::prelude::*;
use webkit6::prelude::*;

#[derive(Default)]
pub struct PaneRegistry {
    pub terminals: HashMap<SurfaceId, TerminalPane>,
    pub browsers: HashMap<SurfaceId, BrowserPane>,
    active_terminal_by_pane: HashMap<PaneId, SurfaceId>,
    active_browser_by_pane: HashMap<PaneId, SurfaceId>,
    pane_frames: HashMap<PaneId, gtk::Widget>,
    surface_stacks: HashMap<PaneId, gtk::Stack>,
    pub surface_tabs: HashMap<PaneId, Vec<(SurfaceId, gtk::Widget)>>,
    /// Tab-bar `gtk::Box` so incremental tab additions can `append`
    /// into the same row instead of rebuilding the whole pane.
    pane_tab_containers: HashMap<PaneId, gtk::Box>,
    surface_tab_labels: HashMap<SurfaceId, gtk::Label>,
    pane_workspace: HashMap<PaneId, WorkspaceId>,
    surface_workspace: HashMap<SurfaceId, WorkspaceId>,
    /// Pane::Split 노드의 PaneId → 그 split을 표현하는 `gtk::Paned`
    /// 위젯. 종료 시 paned.position()/너비/높이로부터 ratio를 계산해
    /// store에 기록하고, 다음 실행 시 같은 ratio로 복원.
    split_paneds: HashMap<PaneId, gtk::Paned>,
    split_workspace: HashMap<PaneId, WorkspaceId>,
}

impl PaneRegistry {
    pub fn active_terminal(&self, pane: PaneId) -> Option<&TerminalPane> {
        self.active_terminal_by_pane
            .get(&pane)
            .and_then(|surface| self.terminals.get(surface))
    }

    pub fn active_browser(&self, pane: PaneId) -> Option<&BrowserPane> {
        self.active_browser_by_pane
            .get(&pane)
            .and_then(|surface| self.browsers.get(surface))
    }

    pub fn pane_frame(&self, pane: PaneId) -> Option<gtk::Widget> {
        self.pane_frames.get(&pane).cloned()
    }

    pub fn pane_ids(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.pane_frames.keys().copied()
    }

    pub fn active_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.active_terminal_by_pane
            .get(&pane)
            .or_else(|| self.active_browser_by_pane.get(&pane))
            .copied()
    }

    pub fn next_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.adjacent_surface(pane, 1)
    }

    pub fn previous_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.adjacent_surface(pane, -1)
    }

    fn adjacent_surface(&self, pane: PaneId, offset: isize) -> Option<SurfaceId> {
        let tabs = self.surface_tabs.get(&pane)?;
        if tabs.len() < 2 {
            return None;
        }
        let active = self.active_surface(pane);
        let active_idx = active
            .and_then(|active| tabs.iter().position(|(id, _)| *id == active))
            .unwrap_or(0);
        let len = tabs.len() as isize;
        let next_idx = (active_idx as isize + offset).rem_euclid(len) as usize;
        Some(tabs[next_idx].0)
    }

    pub fn terminal_cwds(&self) -> Vec<(PaneId, SurfaceId, std::path::PathBuf)> {
        self.terminals
            .iter()
            .filter_map(|(surface, terminal)| {
                terminal
                    .current_dir()
                    .map(|cwd| (terminal.id, *surface, cwd))
            })
            .collect()
    }

    pub fn set_surface_title(&self, surface: SurfaceId, title: &str) {
        if let Some(label) = self.surface_tab_labels.get(&surface) {
            label.set_text(title);
            label.set_tooltip_text(Some(title));
        }
    }

    #[cfg(test)]
    pub fn surface_title_text(&self, surface: SurfaceId) -> Option<String> {
        self.surface_tab_labels
            .get(&surface)
            .map(|label| label.text().to_string())
    }

    pub fn clear_workspace(&mut self, workspace: WorkspaceId) {
        let panes: Vec<PaneId> = self
            .pane_workspace
            .iter()
            .filter_map(|(pane, owner)| (*owner == workspace).then_some(*pane))
            .collect();
        for pane in panes {
            self.active_terminal_by_pane.remove(&pane);
            self.active_browser_by_pane.remove(&pane);
            self.pane_frames.remove(&pane);
            self.surface_stacks.remove(&pane);
            self.surface_tabs.remove(&pane);
            self.pane_tab_containers.remove(&pane);
            self.pane_workspace.remove(&pane);
        }

        let split_ids: Vec<PaneId> = self
            .split_workspace
            .iter()
            .filter_map(|(split, owner)| (*owner == workspace).then_some(*split))
            .collect();
        for split in split_ids {
            self.split_paneds.remove(&split);
            self.split_workspace.remove(&split);
        }

        let surfaces: Vec<SurfaceId> = self
            .surface_workspace
            .iter()
            .filter_map(|(surface, owner)| (*owner == workspace).then_some(*surface))
            .collect();
        for surface in surfaces {
            self.terminals.remove(&surface);
            self.browsers.remove(&surface);
            self.surface_tab_labels.remove(&surface);
            self.surface_workspace.remove(&surface);
        }
    }

    /// 같은 pane 안에서 `surface`로 식별되는 탭을 `target_index` 위치로
    /// 옮긴다. store 쪽 reorder가 성공한 후에만 호출되며, 탭바의
    /// `gtk::Box`와 `surface_tabs` 벡터를 한 번에 동기화한다. `target_index`
    /// 가 길이를 넘거나 자기 자리이면 no-op.
    pub fn reorder_surface_widget(
        &mut self,
        pane: PaneId,
        surface: SurfaceId,
        target_index: usize,
    ) {
        let Some(tabs) = self.surface_tabs.get_mut(&pane) else {
            return;
        };
        let Some(current) = tabs.iter().position(|(id, _)| *id == surface) else {
            return;
        };
        let len = tabs.len();
        if len == 0 {
            return;
        }
        let new_index = target_index.min(len - 1);
        if current == new_index {
            return;
        }
        let entry = tabs.remove(current);
        tabs.insert(new_index, entry);
        let order: Vec<gtk::Widget> = tabs.iter().map(|(_, w)| w.clone()).collect();
        if let Some(container) = self.pane_tab_containers.get(&pane).cloned() {
            // GtkBox는 직접적인 reorder API가 없으므로, 새 순서 기준으로
            // 모든 자식을 떼었다가 다시 append 하는 게 가장 안전하다.
            // 위젯 자체는 유지되므로 핸들러/상태가 보존된다.
            let mut child = container.first_child();
            while let Some(c) = child {
                let next = c.next_sibling();
                container.remove(&c);
                child = next;
            }
            for w in &order {
                container.append(w);
            }
        }
    }

    /// 등록된 모든 split paned의 현재 (split_id, ratio) 쌍을 돌려준다.
    /// ratio는 paned.position() / 전체 길이로 계산. 아직 realize되지 않은
    /// paned나 너비/높이가 0인 paned는 건너뛴다 — 의미 있는 ratio가 아니다.
    pub fn split_ratios(&self) -> Vec<(PaneId, f32)> {
        let mut out = Vec::new();
        for (split_id, paned) in &self.split_paneds {
            let total = match paned.orientation() {
                gtk::Orientation::Horizontal => paned.width(),
                _ => paned.height(),
            };
            if total <= 0 {
                continue;
            }
            let pos = paned.position();
            if pos <= 0 {
                continue;
            }
            let ratio = (pos as f32) / (total as f32);
            out.push((*split_id, ratio));
        }
        out
    }

    /// 한 split paned 위젯을 등록한다. 이미 같은 split_id가 있으면 위젯만
    /// 갱신하고 workspace 매핑은 그대로 둔다.
    pub fn register_split(&mut self, split_id: PaneId, workspace: WorkspaceId, paned: gtk::Paned) {
        self.split_paneds.insert(split_id, paned);
        self.split_workspace.insert(split_id, workspace);
    }

    pub fn activate_surface(&mut self, pane: PaneId, surface: SurfaceId) {
        if let Some(stack) = self.surface_stacks.get(&pane) {
            stack.set_visible_child_name(&surface.to_string());
        }
        if self.terminals.contains_key(&surface) {
            self.active_terminal_by_pane.insert(pane, surface);
            self.active_browser_by_pane.remove(&pane);
        } else if self.browsers.contains_key(&surface) {
            self.active_browser_by_pane.insert(pane, surface);
            self.active_terminal_by_pane.remove(&pane);
        }
        if let Some(tabs) = self.surface_tabs.get(&pane) {
            for (id, tab) in tabs {
                if *id == surface {
                    tab.add_css_class("active");
                } else {
                    tab.remove_css_class("active");
                }
            }
        }
    }

    /// 같은 pane의 한 surface 탭/패널만 위젯 트리에서 떼어낸다. 같은
    /// 워크스페이스 안의 다른 pane은 전혀 건드리지 않으므로 그쪽 셸
    /// 세션 / 탭브라우저 navigate 상태가 그대로 보존된다. close_surface
    /// 가 `SurfaceRemoved`를 돌려준 케이스에서만 호출 가능 — pane이
    /// 통째로 제거된 경우엔 split 트리 변경이 필요하므로 별도 경로로.
    pub fn detach_surface_widget(&mut self, pane: PaneId, surface: SurfaceId) {
        // 탭바에서 해당 탭 위젯 unparent.
        if let Some(tabs) = self.surface_tabs.get_mut(&pane) {
            if let Some(idx) = tabs.iter().position(|(id, _)| *id == surface) {
                let (_, tab_widget) = tabs.remove(idx);
                if let Some(parent) = tab_widget.parent() {
                    if let Some(b) = parent.downcast_ref::<gtk::Box>() {
                        b.remove(&tab_widget);
                    } else {
                        tab_widget.unparent();
                    }
                }
            }
        }
        // 같은 pane의 stack에서 surface 패널 제거.
        if let Some(stack) = self.surface_stacks.get(&pane) {
            if let Some(child) = stack.child_by_name(&surface.to_string()) {
                stack.remove(&child);
            }
        }
        // PaneRegistry 내부 인덱스 정리.
        self.terminals.remove(&surface);
        self.browsers.remove(&surface);
        self.surface_tab_labels.remove(&surface);
        self.surface_workspace.remove(&surface);
        if self.active_terminal_by_pane.get(&pane) == Some(&surface) {
            self.active_terminal_by_pane.remove(&pane);
        }
        if self.active_browser_by_pane.get(&pane) == Some(&surface) {
            self.active_browser_by_pane.remove(&pane);
        }
    }
}

/// `gtk::Paned`의 ratio를 첫 allocation 직후에 적용한다.
///
/// `connect_realize` 시점에는 widget이 아직 allocate되지 않아
/// `paned.width() / height()`가 0이고 그 상태로 `set_position`을 부르면
/// 의미 없는 위치가 잡혀 다음 실행 때 사이즈가 살아나지 않는다. 그래서
/// realize 직후 `idle_add_local`로 한 프레임 미루고, total이 0이면 다음
/// idle에서 다시 시도한다. 최대 60번(약 1초) 재시도한 뒤 포기 — 비활성
/// 워크스페이스라 끝까지 mapping 되지 않는 경우 무한 루프를 막는다.
fn apply_ratio_when_sized(paned: &gtk::Paned, ratio: f32) {
    let weak = paned.downgrade();
    let mut attempts: u32 = 0;
    gtk::glib::idle_add_local(move || {
        let Some(p) = weak.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        let total = match p.orientation() {
            gtk::Orientation::Horizontal => p.width(),
            _ => p.height(),
        };
        if total > 0 {
            p.set_position((total as f32 * ratio) as i32);
            return gtk::glib::ControlFlow::Break;
        }
        attempts += 1;
        if attempts > 60 {
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
}

/// [`split_pane_incremental`]의 결과.
pub enum IncrementalSplitOutcome {
    /// 성공. target이 이미 다른 split 안에 있었으므로 stack 자식은 그대로다.
    /// 호출자가 surfaces map을 갱신할 필요 없음.
    SucceededNested,
    /// 성공. target이 워크스페이스 stack의 직속 자식이었으므로 stack
    /// 자식이 새 `gtk::Paned`로 교체됐다. 호출자는 surfaces map을
    /// 이 새 widget으로 갱신해야 다음 rerender / drop_workspace 경로가
    /// 정상 동작한다.
    SucceededRoot { new_root: gtk::Widget },
    /// incremental 경로 실패. 호출자가 안전하게 rerender_workspace로 폴백
    /// 해야 한다 (registry에 target이 없거나 부모 컨테이너가 비정상).
    Failed,
}

/// 같은 워크스페이스 안의 다른 pane을 그대로 유지한 채 `target_pane`만
/// 새 split으로 감싼다. flowmux-core::Pane::split_leaf와 동일한 의미 —
/// `target_pane`의 PaneId는 보존되어 새 split의 첫 번째 자식으로 남고,
/// `new_pane_id`로 식별되는 새 sibling이 두 번째 자식으로 추가된다.
///
/// 이 incremental 경로의 핵심은 target pane의 `gtk::Frame`을 그대로
/// 재사용한다는 것 — 다른 pane의 VTE 셸 세션과 탭브라우저 navigate
/// 상태가 rerender 없이 살아남는다. 호출 전에 daemon쪽 split_pane이
/// 이미 실행돼 트리 모양은 결정돼 있어야 한다.
///
/// `parent_stack_name`은 target frame이 stack 직속 자식일 때 같은
/// 이름으로 다시 add_named 하기 위해 호출자가 알려준다 (워크스페이스 id).
pub fn split_pane_incremental(
    workspace: WorkspaceId,
    target_pane: PaneId,
    new_pane_id: PaneId,
    new_split_id: PaneId,
    direction: SplitDirection,
    ratio: f32,
    new_content: PaneContent,
    new_cwd: Option<std::path::PathBuf>,
    parent_stack_name: &str,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> IncrementalSplitOutcome {
    let Some(target_frame) = registry.borrow().pane_frame(target_pane) else {
        return IncrementalSplitOutcome::Failed;
    };
    let Some(parent) = target_frame.parent() else {
        return IncrementalSplitOutcome::Failed;
    };

    // 부모 컨테이너 종류 + target가 어느 슬롯에 있었는지를 detach 전에 기록.
    enum Slot {
        PanedStart(gtk::Paned),
        PanedEnd(gtk::Paned),
        Stack(gtk::Stack),
    }
    let slot = if let Some(p) = parent.downcast_ref::<gtk::Paned>() {
        if p.start_child().as_ref() == Some(&target_frame) {
            Slot::PanedStart(p.clone())
        } else if p.end_child().as_ref() == Some(&target_frame) {
            Slot::PanedEnd(p.clone())
        } else {
            return IncrementalSplitOutcome::Failed;
        }
    } else if let Some(s) = parent.downcast_ref::<gtk::Stack>() {
        Slot::Stack(s.clone())
    } else {
        return IncrementalSplitOutcome::Failed;
    };

    // target frame을 부모에서 떼어 낸다. set_*_child(None)은 이전 자식의
    // unparent를 자동으로 처리한다.
    match &slot {
        Slot::PanedStart(p) => p.set_start_child(gtk::Widget::NONE),
        Slot::PanedEnd(p) => p.set_end_child(gtk::Widget::NONE),
        Slot::Stack(s) => s.remove(&target_frame),
    }

    // 새 sibling pane 위젯 빌드. cwd / argv는 새 sibling용 — target은
    // 이미 build 끝나 있는 frame을 그대로 쓰므로 영향 없다.
    let new_sibling = build_leaf_pane(
        workspace,
        new_pane_id,
        &new_content,
        Vec::new(),
        new_cwd,
        callbacks,
        registry.clone(),
        theme.clone(),
    );

    let orient = match direction {
        SplitDirection::Horizontal => gtk::Orientation::Vertical,
        SplitDirection::Vertical => gtk::Orientation::Horizontal,
    };
    let paned = gtk::Paned::new(orient);
    paned.set_hexpand(true);
    paned.set_vexpand(true);
    paned.set_start_child(Some(&target_frame));
    paned.set_end_child(Some(&new_sibling));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    {
        let p = paned.clone();
        paned.connect_realize(move |_| apply_ratio_when_sized(&p, ratio));
    }

    registry
        .borrow_mut()
        .register_split(new_split_id, workspace, paned.clone());

    let paned_widget: gtk::Widget = paned.upcast();

    // 빈 슬롯에 새 Paned를 다시 끼워 넣는다.
    match slot {
        Slot::PanedStart(p) => {
            p.set_start_child(Some(&paned_widget));
            IncrementalSplitOutcome::SucceededNested
        }
        Slot::PanedEnd(p) => {
            p.set_end_child(Some(&paned_widget));
            IncrementalSplitOutcome::SucceededNested
        }
        Slot::Stack(s) => {
            s.add_named(&paned_widget, Some(parent_stack_name));
            s.set_visible_child_name(parent_stack_name);
            IncrementalSplitOutcome::SucceededRoot {
                new_root: paned_widget,
            }
        }
    }
}

pub fn build_surface(
    workspace: WorkspaceId,
    surface: &Surface,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> gtk::Widget {
    let (argv, cwd) = match &surface.kind {
        SurfaceKind::Terminal { cwd, shell } => (
            shell.clone().map(|s| vec![s]).unwrap_or_default(),
            cwd.clone(),
        ),
        SurfaceKind::Browser { .. } => (Vec::new(), None),
    };
    build_pane(
        workspace,
        &surface.root_pane,
        argv,
        cwd,
        callbacks,
        registry,
        theme,
    )
}

fn build_pane(
    workspace: WorkspaceId,
    pane: &Pane,
    argv: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> gtk::Widget {
    match pane {
        Pane::Leaf { id, content } => build_leaf_pane(
            workspace, *id, content, argv, cwd, callbacks, registry, theme,
        ),
        Pane::Split {
            id: split_id,
            direction,
            ratio,
            first,
            second,
        } => {
            let orient = match direction {
                SplitDirection::Horizontal => gtk::Orientation::Vertical,
                SplitDirection::Vertical => gtk::Orientation::Horizontal,
            };
            let paned = gtk::Paned::new(orient);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            let left = build_pane(
                workspace,
                first,
                argv.clone(),
                cwd.clone(),
                callbacks,
                registry.clone(),
                theme.clone(),
            );
            let right = build_pane(workspace, second, argv, cwd, callbacks, registry.clone(), theme);
            paned.set_start_child(Some(&left));
            paned.set_end_child(Some(&right));
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            let r = *ratio;
            {
                let p = paned.clone();
                paned.connect_realize(move |_| apply_ratio_when_sized(&p, r));
            }
            registry
                .borrow_mut()
                .register_split(*split_id, workspace, paned.clone());
            paned.upcast()
        }
    }
}

fn build_leaf_pane(
    workspace: WorkspaceId,
    pane_id: PaneId,
    content: &PaneContent,
    argv: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> gtk::Widget {
    let surfaces = materialize_surfaces(content, cwd);
    let active = match content {
        PaneContent::Tabs { active, .. }
            if surfaces.iter().any(|surface| surface.id == *active) =>
        {
            *active
        }
        _ => surfaces[0].id,
    };

    let frame = gtk::Frame::new(None);
    frame.add_css_class("flowmux-pane");
    frame.set_hexpand(true);
    frame.set_vexpand(true);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_hexpand(true);
    root.set_vexpand(true);

    let tabbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tabbar.add_css_class("flowmux-pane-tabbar");

    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tabs.add_css_class("flowmux-pane-tabs");
    tabs.set_hexpand(false);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 1);
    tools.add_css_class("flowmux-pane-tools");

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);

    let mut tab_widgets = Vec::new();
    for surface in &surfaces {
        let (tab, label) = build_surface_tab_widget(pane_id, surface, surface.id == active, callbacks);
        tabs.append(&tab);
        tab_widgets.push((surface.id, tab.clone().upcast::<gtk::Widget>()));
        registry
            .borrow_mut()
            .surface_tab_labels
            .insert(surface.id, label);

        let widget = build_panel(
            pane_id,
            workspace,
            surface,
            argv.clone(),
            callbacks,
            registry.clone(),
            theme.clone(),
            frame.clone(),
        );
        stack.add_named(&widget, Some(&surface.id.to_string()));
    }

    let split_right = pane_tool_button("go-next-symbolic", "Split right");
    {
        let cb = callbacks.on_split_right.clone();
        let pane_id = pane_id;
        split_right.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&split_right);

    let split_down = pane_tool_button("go-down-symbolic", "Split down");
    {
        let cb = callbacks.on_split_down.clone();
        let pane_id = pane_id;
        split_down.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&split_down);

    let add = pane_tool_button("tab-new-symbolic", "탭 추가");
    {
        let cb = callbacks.on_new_surface.clone();
        let pane_id = pane_id;
        add.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&add);

    let add_browser = pane_tool_button("web-browser-symbolic", "탭브라우저 추가");
    {
        let cb = callbacks.on_new_browser_surface.clone();
        let pane_id = pane_id;
        add_browser.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&add_browser);

    stack.set_visible_child_name(&active.to_string());
    tabbar.append(&tabs);
    tabbar.append(&spacer);
    tabbar.append(&tools);
    root.append(&tabbar);
    root.append(&stack);
    frame.set_child(Some(&root));

    {
        let frame_widget = frame.clone().upcast::<gtk::Widget>();
        let mut r = registry.borrow_mut();
        r.pane_frames.insert(pane_id, frame_widget);
        r.surface_stacks.insert(pane_id, stack);
        r.surface_tabs.insert(pane_id, tab_widgets);
        r.pane_tab_containers.insert(pane_id, tabs);
        r.pane_workspace.insert(pane_id, workspace);
        r.activate_surface(pane_id, active);
    }

    frame.upcast()
}

fn materialize_surfaces(
    content: &PaneContent,
    fallback_cwd: Option<std::path::PathBuf>,
) -> Vec<PaneSurface> {
    match content {
        PaneContent::Tabs { surfaces, .. } if !surfaces.is_empty() => surfaces.clone(),
        PaneContent::Terminal { .. } => vec![PaneSurface::terminal(
            terminal_tab_title_for_cwd(fallback_cwd.as_deref()),
            fallback_cwd,
        )],
        PaneContent::Browser { url } => vec![PaneSurface::browser("Browser", url.clone())],
        PaneContent::Tabs { .. } => vec![PaneSurface::terminal(
            terminal_tab_title_for_cwd(fallback_cwd.as_deref()),
            fallback_cwd,
        )],
    }
}

/// Build a single surface tab + wire its click / double-click (rename)
/// / close handlers. Shared between the initial pane render and the
/// incremental [`attach_surface_to_pane`] path so a click on either
/// behaves identically.
fn build_surface_tab_widget(
    pane_id: PaneId,
    surface: &PaneSurface,
    active: bool,
    callbacks: &PaneCallbacks,
) -> (gtk::Box, gtk::Label) {
    let (tab, label) = surface_tab(surface, active);
    let button = tab
        .first_child()
        .and_downcast::<gtk::Button>()
        .expect("surface tab starts with button");
    {
        let activate_cb = callbacks.on_activate_surface.clone();
        let rename_cb = callbacks.on_rename_surface.clone();
        let last_click = Rc::new(Cell::new(None::<Instant>));
        let surface_id = surface.id;
        button.connect_clicked(move |_| {
            let now = Instant::now();
            let double_clicked = last_click
                .get()
                .is_some_and(|last| now.duration_since(last) <= Duration::from_millis(500));
            if double_clicked {
                last_click.set(None);
                (rename_cb.borrow_mut())(pane_id, surface_id);
            } else {
                last_click.set(Some(now));
                (activate_cb.borrow_mut())(pane_id, surface_id);
            }
        });
    }
    let close = tab
        .last_child()
        .and_downcast::<gtk::Button>()
        .expect("surface tab ends with close button");
    {
        let cb = callbacks.on_close_surface.clone();
        let surface_id = surface.id;
        close.connect_clicked(move |_| (cb.borrow_mut())(pane_id, surface_id));
    }
    attach_tab_dnd_handlers(&tab, pane_id, surface.id, callbacks);
    (tab, label)
}

/// 같은 pane 안에서 탭(터미널/탭브라우저)을 좌우로 드래그 앤 드랍 reorder
/// 하기 위한 컨트롤러를 탭 위젯에 붙인다.
///
/// - `DragSource`: 탭을 잡으면 (PaneId, SurfaceId) 페어를 UTF-8로 직렬화한
///   바이트를 ContentProvider에 담는다. 다른 pane 사이의 이동은 의도적으로
///   막기 위해 PaneId를 함께 실어 DropTarget이 비교할 수 있도록 한다.
/// - `DropTarget`: 같은 pane의 다른 탭 위에 드롭하면 드롭 위치 x로 좌/우를
///   결정해 reorder 콜백을 호출한다. 다른 pane으로의 드롭은 거부한다.
fn attach_tab_dnd_handlers(
    tab: &gtk::Box,
    pane_id: PaneId,
    surface_id: SurfaceId,
    callbacks: &PaneCallbacks,
) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    drag_source.connect_prepare(move |_, _, _| {
        tracing::debug!(%pane_id, %surface_id, "tab drag prepare");
        // ContentProvider::for_value(String) + DropTarget::new(STRING) 조합으로
        // 매칭한다. for_bytes(mime, bytes)는 mime-specific이라 generic
        // Bytes type filter와 매칭되지 않아 motion/drop 시그널이 호출되지
        // 않았다. PaneId와 SurfaceId를 '|' 구분자로 묶어 한 String에 담는다.
        let payload = format!("{pane_id}|{surface_id}");
        Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
    });
    let tab_for_begin = tab.clone();
    drag_source.connect_drag_begin(move |_, _| {
        tab_for_begin.set_opacity(0.4);
        tab_for_begin.add_css_class("flowmux-pane-tab-dragging");
    });
    let tab_for_end = tab.clone();
    drag_source.connect_drag_end(move |_, _, _| {
        tab_for_end.set_opacity(1.0);
        tab_for_end.remove_css_class("flowmux-pane-tab-dragging");
    });
    let tab_for_cancel = tab.clone();
    drag_source.connect_drag_cancel(move |_, _, _| {
        tab_for_cancel.set_opacity(1.0);
        tab_for_cancel.remove_css_class("flowmux-pane-tab-dragging");
        false
    });
    tab.add_controller(drag_source);

    let drop_target =
        gtk::DropTarget::new(gtk::glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
    // motion 시그널의 x로 탭 좌/우 절반을 판정해 인디케이터 위치를 정한다.
    // 드롭 로직도 같은 x 기준으로 final_index를 계산하므로, 사용자가 보는
    // 파란 라인이 곧 드롭이 일어나는 위치다. 첫 탭의 왼쪽 절반에 호버하면
    // 인디케이터가 첫 탭 왼쪽에 떠서 "맨 앞으로 이동" 시그널이 된다.
    let tab_for_motion = tab.clone();
    drop_target.connect_motion(move |_, x, _y| {
        let width = tab_for_motion.width();
        let after = if width > 0 {
            x > (width as f64) / 2.0
        } else {
            false
        };
        if after {
            tab_for_motion.remove_css_class("flowmux-pane-tab-drop-before");
            tab_for_motion.add_css_class("flowmux-pane-tab-drop-after");
        } else {
            tab_for_motion.remove_css_class("flowmux-pane-tab-drop-after");
            tab_for_motion.add_css_class("flowmux-pane-tab-drop-before");
        }
        gtk::gdk::DragAction::MOVE
    });
    let tab_for_leave = tab.clone();
    drop_target.connect_leave(move |_| {
        tab_for_leave.remove_css_class("flowmux-pane-tab-drop-before");
        tab_for_leave.remove_css_class("flowmux-pane-tab-drop-after");
    });
    let target_pane = pane_id;
    let target_surface = surface_id;
    let tab_for_drop = tab.clone();
    let reorder_cb = callbacks.on_reorder_surface.clone();
    let position_of_surface_cb = callbacks.position_of_surface_in_pane.clone();
    drop_target.connect_drop(move |_, value, x, _y| {
        tracing::debug!(%target_pane, %target_surface, "tab drop fired");
        tab_for_drop.remove_css_class("flowmux-pane-tab-drop-before");
        tab_for_drop.remove_css_class("flowmux-pane-tab-drop-after");
        let Ok(payload) = value.get::<String>() else {
            tracing::warn!(value = ?value, "tab drop: payload was not String — DropTarget type mismatch");
            return false;
        };
        let Some((src_pane_str, src_surface_str)) = payload.split_once('|') else {
            tracing::warn!(payload = %payload, "tab drop: payload missing '|' separator");
            return false;
        };
        let Ok(src_pane) = src_pane_str.parse::<PaneId>() else {
            tracing::warn!(s = %src_pane_str, "tab drop: payload pane id invalid");
            return false;
        };
        let Ok(src_surface) = src_surface_str.parse::<SurfaceId>() else {
            tracing::warn!(s = %src_surface_str, "tab drop: payload surface id invalid");
            return false;
        };
        // pane 간 이동은 지원하지 않는다 — 같은 pane의 다른 탭 위에서만 reorder.
        if src_pane != target_pane {
            tracing::debug!(%src_pane, %target_pane, "tab drop: cross-pane drop ignored");
            return false;
        }
        if src_surface == target_surface {
            tracing::debug!(%src_surface, "tab drop: dropped onto self, ignoring");
            return false;
        }

        // 드롭 x가 탭 폭의 절반보다 왼쪽이면 target 앞, 오른쪽이면 뒤로.
        // target_index는 *최종* 인덱스이므로, 탭바 내부에서 target tab의
        // 현재 인덱스를 알아야 한다. 부모 GtkBox에서 형제 위치를 센다.
        let Some(parent) = tab_for_drop.parent() else {
            return false;
        };
        let mut target_index: usize = 0;
        let mut child = parent.first_child();
        while let Some(c) = child {
            if c == tab_for_drop.clone().upcast::<gtk::Widget>() {
                break;
            }
            target_index += 1;
            child = c.next_sibling();
        }

        let tab_width = tab_for_drop.width();
        let after = if tab_width > 0 {
            x > (tab_width as f64) / 2.0
        } else {
            false
        };

        // 같은 박스 안에서:
        // - 소스가 타깃 *왼쪽*에 있을 때 (src_idx < target_index)
        //     "타깃 앞"이면 target_index-1, "타깃 뒤"면 target_index.
        // - 소스가 타깃 *오른쪽*에 있을 때 (src_idx > target_index)
        //     "타깃 앞"이면 target_index, "타깃 뒤"면 target_index+1.
        // 소스 인덱스를 모르기 때문에 +1 보정은 daemon의 클램프(min(len-1))
        // 에 맡긴다. 결과가 자기 자리면 reorder_surface_in_pane이 None을
        // 반환하므로 GTK 위젯 이동도 건너뛴다.
        // 정확한 final_index 계산 — source remove 후 target 옆에 insert.
        // PaneRegistry의 surface_tabs에서 src_surface 위치를 직접 빌려 본다
        // (callback으로 노출).
        let src_index = (position_of_surface_cb)(target_pane, src_surface);
        let final_index = match (after, src_index) {
            (false, Some(s)) if s < target_index => target_index.saturating_sub(1),
            (false, _) => target_index,
            (true, Some(s)) if s < target_index => target_index,
            (true, _) => target_index.saturating_add(1),
        };

        tracing::info!(
            %target_pane,
            %src_surface,
            %target_surface,
            target_index,
            src_index = ?src_index,
            final_index,
            after,
            "tab drop: dispatching reorder callback"
        );
        (reorder_cb.borrow_mut())(target_pane, src_surface, final_index);
        true
    });
    tab.add_controller(drop_target);
}

/// Attach a single new surface to an already-rendered pane: appends a
/// tab to its tab bar, mounts the panel widget into the pane's stack,
/// records it in `registry`, and activates it. Returns `false` if the
/// pane has not been rendered yet (e.g. workspace not visible) — the
/// caller can fall back to a full re-render.
///
/// 이 경로는 기존 탭/탭브라우저의 GTK 위젯을 손대지 않으므로 다른
/// pane에 띄워둔 탭브라우저의 navigate 상태와 터미널 셸 세션이
/// 사라지지 않는다는 점이 핵심이다.
pub fn attach_surface_to_pane(
    pane_id: PaneId,
    workspace: WorkspaceId,
    surface: &PaneSurface,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> bool {
    let (tabs, stack, frame) = {
        let r = registry.borrow();
        let Some(tabs) = r.pane_tab_containers.get(&pane_id).cloned() else {
            return false;
        };
        let Some(stack) = r.surface_stacks.get(&pane_id).cloned() else {
            return false;
        };
        let Some(frame) = r
            .pane_frames
            .get(&pane_id)
            .and_then(|w| w.downcast_ref::<gtk::Frame>().cloned())
        else {
            return false;
        };
        (tabs, stack, frame)
    };

    let (tab, label) = build_surface_tab_widget(pane_id, surface, true, callbacks);
    tabs.append(&tab);

    let widget = build_panel(
        pane_id,
        workspace,
        surface,
        Vec::new(),
        callbacks,
        registry.clone(),
        theme,
        frame,
    );
    stack.add_named(&widget, Some(&surface.id.to_string()));

    {
        let mut r = registry.borrow_mut();
        r.surface_tab_labels.insert(surface.id, label);
        r.surface_tabs
            .entry(pane_id)
            .or_default()
            .push((surface.id, tab.upcast::<gtk::Widget>()));
        r.activate_surface(pane_id, surface.id);
    }
    true
}

fn surface_tab(surface: &PaneSurface, active: bool) -> (gtk::Box, gtk::Label) {
    let tab = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab.add_css_class("flowmux-pane-tab");
    if active {
        tab.add_css_class("active");
    }

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("flowmux-pane-tab-main");
    button.set_tooltip_text(Some(&surface.title));
    button.set_focus_on_click(false);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let icon_name = match surface.kind {
        SurfaceKind::Terminal { .. } => "utilities-terminal-symbolic",
        SurfaceKind::Browser { .. } => "web-browser-symbolic",
    };
    row.append(&gtk::Image::from_icon_name(icon_name));
    let label = gtk::Label::new(Some(&surface.title));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(18);
    label.set_tooltip_text(Some(&surface.title));
    row.append(&label);
    button.set_child(Some(&row));
    tab.append(&button);

    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.add_css_class("flowmux-pane-tab-close");
    close.set_tooltip_text(Some("Close tab"));
    close.set_focus_on_click(false);
    tab.append(&close);

    (tab, label)
}

fn pane_tool_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    button.add_css_class("flat");
    button.add_css_class("flowmux-pane-tool");
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

fn build_panel(
    pane_id: PaneId,
    workspace: WorkspaceId,
    surface: &PaneSurface,
    argv: Vec<String>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
    frame: gtk::Frame,
) -> gtk::Widget {
    match &surface.kind {
        SurfaceKind::Terminal { cwd, shell } => {
            let argv = shell.clone().map(|s| vec![s]).unwrap_or(argv);
            let pane = TerminalPane::spawn(pane_id, argv, cwd.clone(), callbacks.clone());
            theme.apply_to_vte(&pane.widget);
            // 새 터미널 위젯도 현재 옵션의 줌 배율로 시작한다.
            pane.widget
                .set_font_scale((callbacks.read_options)().zoom_factor());

            {
                let cb = callbacks.on_terminal_cwd_changed.clone();
                let pane_for_cwd = pane.clone();
                let surface_id = surface.id;
                pane.widget.connect_current_directory_uri_notify(move |_| {
                    if let Some(cwd) = pane_for_cwd.current_dir() {
                        (cb.borrow_mut())(pane_id, surface_id, cwd);
                    }
                });
            }

            // OSC 0/2로 들어온 윈도우 타이틀(vi/claude/codex/tmux 등이
            // 발행)을 탭 라벨 + 윈도우 타이틀에 반영. VTE가 빈 문자열
            // 로 reset 보낼 수도 있으므로 dispatch 측에서 trim한 결과
            // 가 비면 무시한다.
            {
                let cb = callbacks.on_terminal_title_changed.clone();
                let surface_id = surface.id;
                let widget_for_title = pane.widget.clone();
                widget_for_title.clone().connect_window_title_notify(move |_| {
                    let title = widget_for_title
                        .window_title()
                        .map(|t| t.to_string())
                        .unwrap_or_default();
                    tracing::debug!(
                        %pane_id,
                        %surface_id,
                        title = %title,
                        "VTE window-title notify"
                    );
                    (cb.borrow_mut())(pane_id, surface_id, title);
                });
            }

            // 포커스 enter/leave에 맞춰 frame에 .focused class를
            // 토글한다. 포커스된 pane은 옵션의 focus_border_color로
            // 1px 테두리를 그리도록 theme.rs CSS가 처리한다.
            let frame_in = frame.clone();
            let frame_out = frame.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                if !frame_in.has_css_class("focused") {
                    frame_in.add_css_class("focused");
                }
            });
            focus.connect_leave(move |_| {
                frame_out.remove_css_class("focused");
            });
            pane.widget.add_controller(focus);

            let widget = pane.root.clone();
            let mut r = registry.borrow_mut();
            r.terminals.insert(surface.id, pane);
            r.surface_workspace.insert(surface.id, workspace);
            widget
        }
        SurfaceKind::Browser { initial_url } => {
            let opts = (callbacks.read_options)();
            let pane = BrowserPane::new(
                pane_id,
                surface.id,
                initial_url.as_deref(),
                callbacks.clone(),
                opts.default_browser_engine.clone(),
            );
            // 다이얼로그에서 적용된 줌 배율을 새 탭브라우저에 즉시
            // 반영 — apply_zoom 호출 전에 만들어진 위젯도 옵션과
            // 동기화된 상태에서 시작한다.
            pane.web_view.set_zoom_level(opts.zoom_factor());

            // 탭브라우저도 포커스 표시 + on_focus 콜백 동일하게 처리.
            // on_focus를 호출해야 WindowController.focused_pane이 갱신
            // 되고 RefreshWindowTitle이 새 active surface 라벨로
            // 윈도우 타이틀을 다시 계산한다 (브라우저 탭 클릭 시
            // 윈도우 제목이 안 바뀌던 회귀 수정).
            //
            // 컨트롤러는 web_view가 아니라 BrowserPane.root에 단다.
            // BrowserPane은 [chrome row(주소창/back/forward/reload) +
            // web_view]로 구성되는데, web_view에만 컨트롤러가 있으면
            // 사용자가 주소창을 클릭한 순간 web_view가 leave를 받아
            // frame의 .focused 테두리가 사라지고, 주소창 쪽에는 컨트롤러
            // 가 없어 on_focus도 호출되지 않아 focused_pane이 갱신되지
            // 않는 문제가 있었다 (Alt+화살표가 갑자기 동작하지 않는 증상).
            // root에 달면 GTK4 EventControllerFocus가 widget+descendants
            // 단위로 enter/leave를 emit하므로 chrome row와 web_view 사이
            // 에서 포커스가 오가는 것은 무시되고, pane 바깥으로 나갈 때
            // 만 leave가 발생한다.
            let frame_in = frame.clone();
            let frame_out = frame.clone();
            let on_focus = callbacks.on_focus.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                tracing::debug!(%pane_id, "browser pane focus enter");
                if !frame_in.has_css_class("focused") {
                    frame_in.add_css_class("focused");
                }
                (on_focus.borrow_mut())(pane_id);
            });
            focus.connect_leave(move |_| {
                frame_out.remove_css_class("focused");
            });
            pane.root.add_controller(focus);

            let widget = pane.root.clone().upcast::<gtk::Widget>();
            let mut r = registry.borrow_mut();
            r.browsers.insert(surface.id, pane);
            r.surface_workspace.insert(surface.id, workspace);
            widget
        }
    }
}
