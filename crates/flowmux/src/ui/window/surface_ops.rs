// SPDX-License-Identifier: GPL-3.0-or-later
//! Surface lifecycle: split, move, tear-off, reattach, import.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

impl WindowController {
    /// Remove a pane from the live widget tree by re-parenting its
    /// surviving sibling into the slot the parent `gtk::Paned`
    /// occupied. Every other pane's widget instance — and therefore
    /// every running PTY shell + browser navigation state — is
    /// preserved.
    ///
    /// Falls back to [`Self::rerender_workspace`] when the GTK tree
    /// shape is not the simple "frame inside Paned" case (e.g. the
    /// removed pane was the workspace root). The fallback resets
    /// PTYs, but `close_pane` returning `WorkspaceRemoved` already
    /// goes down a different path, so the fallback is rarely hit.
    pub async fn apply_close_pane_incremental_or_rerender(
        &self,
        ws_id: WorkspaceId,
        removed: PaneId,
    ) {
        let frame = {
            let r = self.pane_registry.borrow();
            r.pane_frame(removed)
        };
        let Some(frame) = frame else {
            if let Some(ws) = self.store.get_workspace(ws_id).await {
                self.rerender_workspace(&ws);
            }
            return;
        };

        let Some(parent) = frame.parent() else {
            if let Some(ws) = self.store.get_workspace(ws_id).await {
                self.rerender_workspace(&ws);
            }
            return;
        };

        let Some(paned) = parent.downcast::<gtk::Paned>().ok() else {
            // Removed pane wasn't inside a `gtk::Paned` — nothing to
            // collapse incrementally. Defensive fallback.
            if let Some(ws) = self.store.get_workspace(ws_id).await {
                self.rerender_workspace(&ws);
            }
            return;
        };

        // Pick the sibling: the child of `paned` that isn't `frame`.
        let sibling = if paned.start_child().map(|w| w == frame).unwrap_or(false) {
            paned.end_child()
        } else {
            paned.start_child()
        };
        let Some(sibling) = sibling else {
            if let Some(ws) = self.store.get_workspace(ws_id).await {
                self.rerender_workspace(&ws);
            }
            return;
        };

        // Detach both children from `paned`. After this `paned` has
        // no children and `frame` / `sibling` have no parent; re-
        // parenting `sibling` lower preserves its widget instance.
        sibling.unparent();
        frame.unparent();

        // Re-parent `sibling` into the slot `paned` occupied. Each
        // grand-parent kind needs its own removal API: a plain
        // `paned.unparent()` works for `gtk::Paned`, but a
        // `gtk::Stack` keeps a `GtkStackPage` registered to the
        // child name. If we unparent the paned without going through
        // `Stack::remove`, the page entry survives, the subsequent
        // `add_named` with the same name silently no-ops, and the
        // workspace renders blank — which the user reported as "the
        // right X-close drops every pane".
        let Some(grand) = paned.parent() else {
            paned.unparent();
            if let Some(ws) = self.store.get_workspace(ws_id).await {
                self.rerender_workspace(&ws);
            }
            return;
        };

        if let Some(grand_paned) = grand.downcast_ref::<gtk::Paned>() {
            // Nested split: identify which slot of the outer paned holds
            // `paned` and replace it with `sibling`. Calling `set_*_child`
            // directly auto-unparents the previous occupant in one step,
            // so we do NOT pre-call `paned.unparent()` here. With manual
            // unparent followed by `is_none()` slot-pick, GTK4 can leave
            // the parent slot still pointing at `paned` until the next
            // event-loop flush, which triggered the rerender fallback —
            // and that fallback rebuilt every other pane's terminal, killing
            // any agent (claude/codex/shell) running in those panes.
            let paned_widget: gtk::Widget = paned.clone().upcast();
            if grand_paned.start_child().as_ref() == Some(&paned_widget) {
                grand_paned.set_start_child(Some(&sibling));
            } else if grand_paned.end_child().as_ref() == Some(&paned_widget) {
                grand_paned.set_end_child(Some(&sibling));
            } else {
                tracing::warn!(
                    "apply_close_pane_incremental: paned not found in grand_paned slots; falling back"
                );
                paned.unparent();
                if let Some(ws) = self.store.get_workspace(ws_id).await {
                    self.rerender_workspace(&ws);
                }
                return;
            }
        } else if let Some(stack) = grand.downcast_ref::<gtk::Stack>() {
            // Top-level workspace child of the GtkStack. Use
            // `Stack::remove` so the GtkStackPage for the old
            // paned-as-name is freed before we register `sibling`
            // under the same name.
            let name = ws_id.to_string();
            stack.remove(&paned);
            stack.add_named(&sibling, Some(&name));
            self.surfaces.borrow_mut().insert(ws_id, sibling.clone());
            stack.set_visible_child_name(&name);
        } else if let Some(b) = grand.downcast_ref::<gtk::Box>() {
            b.remove(&paned);
            b.append(&sibling);
        } else {
            tracing::warn!(
                kind = ?grand.type_(),
                "apply_close_pane_incremental: unexpected grand parent kind; falling back"
            );
            paned.unparent();
            if let Some(ws) = self.store.get_workspace(ws_id).await {
                self.rerender_workspace(&ws);
            }
            return;
        }

        // Drop registry entries for the removed pane only — every
        // other pane's TerminalPane / BrowserPane stays alive in the
        // registry and continues to render. The collapsed `paned` is also
        // gone now, so drop its split-index entries to avoid a slow leak
        // across repeated split/close cycles.
        {
            let mut reg = self.pane_registry.borrow_mut();
            reg.forget_pane(removed);
            reg.forget_split_paned(&paned);
        }
        if let Some(ws) = self.store.get_workspace(ws_id).await {
            self.refresh_workspace_solo(&ws);
        }
    }
    /// After a pane has been removed, hand keyboard focus to a sibling so the
    /// user can keep typing without clicking back into a terminal. Only acts
    /// when the removed pane *was* the focused one — closing an unfocused pane
    /// (e.g. clicking another pane's X-button while typing in this one) must
    /// leave focus alone.
    ///
    /// Successor selection, in order:
    /// 1. The most recently focused pane that still exists in this workspace
    ///    (the MRU head after removing `removed`). This matches what the user
    ///    most likely thinks of as "the previous pane".
    /// 2. The workspace's first leaf as a defensive fallback when MRU is empty
    ///    (e.g. the user closed a pane before any focus event was recorded).
    ///
    /// `grab_focus` is deferred to the next idle so the surviving sibling has
    /// a chance to be reparented and realized first; the existing
    /// `EventControllerFocus` then fires `on_focus`, which updates
    /// `focused_pane` and re-pushes the new front of MRU.
    pub(super) async fn focus_after_close(&self, ws_id: WorkspaceId, removed: PaneId) {
        let was_focused = self.focused_pane.get() == Some(removed);

        // Always evict the closed pane from MRU so a later focus_after_close
        // can't pick a dead PaneId.
        {
            let mut mru = self.focus_mru.borrow_mut();
            if let Some(q) = mru.get_mut(&ws_id) {
                q.retain(|p| *p != removed);
            }
        }

        if !was_focused {
            return;
        }

        // 1. MRU head, only if its frame is still registered.
        let mru_head = self
            .focus_mru
            .borrow()
            .get(&ws_id)
            .and_then(|q| q.front().copied());
        let target = match mru_head {
            Some(p) if self.pane_registry.borrow().pane_frame(p).is_some() => Some(p),
            _ => None,
        };

        // 2. Fall back to the workspace's first leaf in the daemon-side tree.
        let target = match target {
            Some(t) => Some(t),
            None => self.store.get_workspace(ws_id).await.and_then(|ws| {
                ws.surfaces
                    .first()
                    .and_then(|s| s.root_pane.first_leaf_id())
            }),
        };
        let Some(target) = target else { return };

        let registry = self.pane_registry.clone();
        glib::idle_add_local_once(move || {
            let r = registry.borrow();
            if let Some(term) = r.active_terminal(target) {
                term.grab_focus();
            } else if let Some(browser) = r.active_browser(target) {
                browser.grab_focus();
            }
        });
    }
    /// Update the GTK widget tree after the daemon-side split has completed.
    /// When possible, reuse `target_pane`'s existing `gtk::Frame` inside the new
    /// `gtk::Paned` so other panes in the same workspace, including shell
    /// sessions and browser navigation state, are not reset. If this fails,
    /// for example because the target is missing from the registry or the
    /// parent container is unexpected, safely fall back to [`Self::rerender_workspace`].
    pub(super) async fn apply_split_incremental_or_rerender(
        &self,
        ws_id: WorkspaceId,
        target_pane: PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
    ) {
        let Some(ws) = self.store.get_workspace(ws_id).await else {
            return;
        };

        // Find the new sibling pane's PaneContent / cwd in the post-split tree.
        // daemon::StateStore::split_pane created it with a 0.5 ratio and
        // tabbed_terminal, so mirror those values on the GTK side.
        let new_content = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.find_leaf_content(new_pane));
        let Some(new_content) = new_content else {
            self.rerender_workspace(&ws);
            return;
        };

        // PaneId of the newly created Split node. The split containing the new
        // sibling as a child is the split we need. Ratio save/restore keys on
        // PaneId, so register the GTK widget with the same id.
        let new_split_id = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.parent_split_id(new_pane));
        let Some(new_split_id) = new_split_id else {
            self.rerender_workspace(&ws);
            return;
        };

        // Fallback cwd for the new terminal. Surface content cwd wins; this
        // value is only for legacy or empty-state fallback, so workspace root_dir
        // is enough.
        let new_cwd = Some(ws.root_dir.clone());

        let stack_name = ws.id.to_string();
        let outcome = split_pane_incremental(
            ws.id,
            target_pane,
            new_pane,
            new_split_id,
            direction,
            0.5,
            new_content,
            new_cwd,
            &stack_name,
            &self.callbacks,
            self.pane_registry.clone(),
            self.theme.clone(),
            false,
        );

        match outcome {
            IncrementalSplitOutcome::SucceededRoot { new_root } => {
                // If target was the stack root, update the surfaces tracking map
                // to the new widget so later drop_workspace / rerender paths do
                // not look for the old widget in the stack.
                self.surfaces.borrow_mut().insert(ws.id, new_root);
                self.refresh_workspace_solo(&ws);
                self.refresh_window_title().await;
            }
            IncrementalSplitOutcome::SucceededNested => {
                self.refresh_workspace_solo(&ws);
                self.refresh_window_title().await;
            }
            IncrementalSplitOutcome::Failed => {
                self.rerender_workspace(&ws);
                self.refresh_window_title().await;
            }
        }
    }
    /// Attach a newly created surface incrementally whenever possible.
    ///
    /// Old behavior: call rerender_workspace, rebuild the entire workspace
    /// widget, and lose browser navigation state plus terminal shell sessions in
    /// other panes.
    ///
    /// New behavior: append only to the target pane's tab bar / stack. If the
    /// pane is not rendered yet, for example because another workspace is
    /// visible, or the registry cannot find handles, safely fall back to a full
    /// rerender.
    pub(super) async fn attach_or_rerender_surface(
        &self,
        ws_id: WorkspaceId,
        pane: PaneId,
        surface_id: SurfaceId,
    ) {
        let Some(ws) = self.store.get_workspace(ws_id).await else {
            return;
        };
        let surface = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.find_surface(pane, surface_id));
        if let Some(surface) = surface {
            let attached = attach_surface_to_pane(
                pane,
                ws.id,
                &surface,
                &self.callbacks,
                self.pane_registry.clone(),
                self.theme.clone(),
            );
            if attached {
                self.refresh_workspace_solo(&ws);
                self.refresh_window_title().await;
                // Move keyboard focus to the newly added terminal/browser tab.
                // attach_surface_to_pane adds the widget and switches the visible
                // child but does not grab focus, which previously left focus on
                // the now-hidden old widget after Ctrl+Shift+T / Ctrl+Shift+B.
                // Defer to idle like ActivateSurface so focus happens after realize.
                let registry = self.pane_registry.clone();
                glib::idle_add_local_once(move || {
                    let r = registry.borrow();
                    if let Some(term) = r.terminals.get(&surface_id) {
                        term.grab_focus();
                    } else if let Some(browser) = r.browsers.get(&surface_id) {
                        browser.grab_focus();
                    }
                });
                return;
            }
        }
        self.rerender_workspace(&ws);
        self.refresh_window_title().await;
    }
    pub(super) fn build_torn_off_pane(
        torn: TornOffSurface,
        title: &str,
        window_ref: Rc<RefCell<Option<glib::WeakRef<adw::ApplicationWindow>>>>,
    ) -> gtk::Widget {
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

        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tab.add_css_class("flowmux-pane-tab");
        tab.add_css_class("active");

        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("flowmux-pane-tab-main");
        button.set_tooltip_text(Some(title));
        button.set_focus_on_click(false);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let icon_name = match torn.kind {
            SurfaceKind::Terminal { .. } => "utilities-terminal-symbolic",
            SurfaceKind::Browser { .. } => "web-browser-symbolic",
        };
        row.append(&gtk::Image::from_icon_name(icon_name));
        let label = gtk::Label::new(Some(title));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(18);
        label.set_tooltip_text(Some(title));
        row.append(&label);
        button.set_child(Some(&row));

        let focus_for_tab = torn.focus.clone();
        button.connect_clicked(move |_| {
            focus_for_tab.grab_focus();
        });
        tab.append(&button);

        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("flowmux-pane-tab-close");
        close.set_tooltip_text(Some("Close tab"));
        close.set_focus_on_click(false);
        let window_ref_for_close = window_ref.clone();
        close.connect_clicked(move |_| {
            if let Some(window) = window_ref_for_close
                .borrow()
                .as_ref()
                .and_then(|window| window.upgrade())
            {
                window.close();
            }
        });
        tab.append(&close);
        tabs.append(&tab);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);

        let tools = gtk::Box::new(gtk::Orientation::Horizontal, 1);
        tools.add_css_class("flowmux-pane-tools");
        for (icon, tooltip) in [
            ("go-next-symbolic", "Split right"),
            ("go-down-symbolic", "Split down"),
            ("tab-new-symbolic", "Add tab"),
            ("web-browser-symbolic", "Add browser tab"),
        ] {
            let button = gtk::Button::from_icon_name(icon);
            button.add_css_class("flat");
            button.add_css_class("flowmux-pane-tool");
            button.set_tooltip_text(Some(tooltip));
            button.set_focus_on_click(false);
            button.set_sensitive(false);
            tools.append(&button);
        }

        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.add_named(&torn.content, Some(&torn.surface.to_string()));
        stack.set_visible_child_name(&torn.surface.to_string());

        tabbar.append(&tabs);
        tabbar.append(&spacer);
        tabbar.append(&tools);
        root.append(&tabbar);
        root.append(&stack);
        frame.set_child(Some(&root));
        frame.upcast()
    }
    pub(super) fn present_torn_off_surface(&self, app: &gtk::Application, torn: TornOffSurface) {
        let title = match torn.title.trim() {
            "" => "flowmux".to_string(),
            title => title.to_string(),
        };
        let window_title = format!("flowmux - {title}");

        let workspace_id = WorkspaceId::new();
        let pane_surface = PaneSurface {
            id: torn.surface,
            title: title.clone(),
            title_locked: false,
            kind: torn.kind.clone(),
            agent: None,
        };
        let workspace = Workspace {
            id: workspace_id,
            name: title.clone(),
            custom_title: None,
            root_dir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
            git: None,
            listening_ports: Vec::new(),
            surfaces: vec![Surface {
                id: torn.surface,
                kind: torn.kind.clone(),
                title: title.clone(),
                root_pane: Pane::Leaf {
                    id: torn.pane,
                    content: PaneContent::Tabs {
                        active: torn.surface,
                        surfaces: vec![pane_surface],
                    },
                },
            }],
            color: None,
        };

        let (sidebar_bridge, _sidebar_rx) = Bridge::new();
        let window_ref: Rc<RefCell<Option<glib::WeakRef<adw::ApplicationWindow>>>> =
            Rc::new(RefCell::new(None));
        let window_ref_for_sidebar = window_ref.clone();
        let sidebar = Sidebar::new(
            |_| {},
            move |_| {
                if let Some(window) = window_ref_for_sidebar
                    .borrow()
                    .as_ref()
                    .and_then(|window| window.upgrade())
                {
                    window.close();
                }
            },
            sidebar_bridge,
            NotificationStore::new(),
        );
        sidebar.root.set_size_request(160, -1);
        sidebar.upsert(&workspace);
        sidebar.select_workspace(workspace_id);

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        let focus = torn.focus.clone();
        let pane_id = torn.pane;
        let surface_id = torn.surface;
        let pane = Self::build_torn_off_pane(torn, &title, window_ref.clone());
        stack.add_named(&pane, Some(&workspace_id.to_string()));
        stack.set_visible_child_name(&workspace_id.to_string());

        let sidebar_pos = match self.sidebar_split.position() {
            pos if pos > 0 => pos,
            _ => 260,
        };
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar.root)
            .end_child(&stack)
            .resize_start_child(false)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(sidebar_pos)
            .build();

        let content_overlay = gtk::Overlay::new();
        content_overlay.set_child(Some(&split));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content_overlay));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(1280)
            .default_height(800)
            .icon_name(crate::APP_ID)
            .title(&window_title)
            .build();
        set_window_content(&window, &toolbar);
        *window_ref.borrow_mut() = Some(window.downgrade());
        window.present();

        glib::idle_add_local_once(move || {
            focus.grab_focus();
        });
        tracing::info!(
            pane = %pane_id,
            surface = %surface_id,
            title = %title,
            "tore tab off into flowmux window"
        );
    }
    pub(super) async fn tear_off_surface(&self, pane: PaneId, surface: SurfaceId) {
        let Some(app) = self.window.application() else {
            tracing::warn!(%pane, %surface, "tab tear-off skipped: window has no application");
            return;
        };
        let Some(title) = self.store.surface_title(pane, surface).await else {
            tracing::warn!(%pane, %surface, "tab tear-off skipped: surface not found in store");
            return;
        };
        let Some(torn) = self
            .pane_registry
            .borrow_mut()
            .take_surface_for_tearoff(pane, surface, &title)
        else {
            tracing::warn!(%pane, %surface, "tab tear-off skipped: surface widget not found");
            return;
        };

        self.present_torn_off_surface(&app, torn);
        match self.store.close_surface(pane, surface).await {
            None => {
                tracing::warn!(
                    %pane,
                    %surface,
                    "tab tear-off moved widget but store no longer had the surface"
                );
            }
            Some(flowmux_daemon::CloseOutcome::WorkspaceRemoved { workspace }) => {
                self.drop_workspace(workspace);
                self.activate_active_or_show_empty().await;
            }
            Some(flowmux_daemon::CloseOutcome::SurfaceRemoved { workspace }) => {
                if let Some(ws) = self.store.get_workspace(workspace).await {
                    if let Some(active) = ws
                        .surfaces
                        .iter()
                        .find_map(|s| s.root_pane.active_surface_id(pane))
                    {
                        self.pane_registry
                            .borrow_mut()
                            .activate_surface(pane, active);
                    }
                    self.refresh_workspace_solo(&ws);
                }
                self.refresh_window_title().await;
                self.sync_workspace_agent_status_from_store(workspace).await;
            }
            Some(flowmux_daemon::CloseOutcome::PaneRemoved { workspace }) => {
                self.apply_close_pane_incremental_or_rerender(workspace, pane)
                    .await;
                self.focus_after_close(workspace, pane).await;
                self.sync_workspace_agent_status_from_store(workspace).await;
            }
        }
    }
    /// Move a live surface tab into another pane (`dst_pane`), preserving its
    /// terminal/browser state by re-parenting the existing widget rather than
    /// rebuilding it. `target_index` is the final position in the destination
    /// pane (clamped to the end). Backs cross-pane and cross-workspace
    /// drag-and-drop.
    pub(super) async fn move_surface(
        &self,
        src_pane: PaneId,
        surface: SurfaceId,
        surface_model: Option<flowmux_core::PaneSurface>,
        dst_pane: PaneId,
        target_index: usize,
    ) -> Result<(), String> {
        if src_pane == dst_pane {
            // A drop onto the tab's own pane (e.g. pane-body drop) is just a
            // reorder; do that instead of a no-op move.
            if self
                .store
                .reorder_surface_in_pane(src_pane, surface, target_index)
                .await
                .is_some()
            {
                self.pane_registry.borrow_mut().reorder_surface_widget(
                    src_pane,
                    surface,
                    target_index,
                );
            }
            return Ok(());
        }

        if !self.pane_registry.borrow().has_pane(dst_pane) {
            return Err("destination pane is not rendered".to_string());
        }

        let moving = self
            .pane_registry
            .borrow_mut()
            .detach_surface_for_move(src_pane, surface);
        let Some(moving) = moving else {
            if let Some(surface_model) = surface_model {
                return self
                    .import_surface_to_pane(surface_model, dst_pane, target_index)
                    .await;
            }
            return Err("source surface is not rendered".to_string());
        };

        let outcome = match self
            .store
            .move_surface_to_pane(src_pane, surface, dst_pane, target_index)
            .await
        {
            Some(outcome) => outcome,
            None => {
                // Model rejected the move; restore the widget to its source.
                self.reattach_surface(src_pane, surface, moving).await;
                return Err("destination pane no longer exists".to_string());
            }
        };

        let mounted = self
            .mount_moved_surface(
                dst_pane,
                outcome.dst_workspace,
                surface,
                moving,
                target_index,
            )
            .await;
        if !mounted {
            return Err("failed to mount moved surface".to_string());
        }

        if outcome.src_workspace_removed {
            self.drop_workspace(outcome.src_workspace);
            self.activate_active_or_show_empty().await;
        } else if outcome.src_pane_removed {
            self.apply_close_pane_incremental_or_rerender(outcome.src_workspace, src_pane)
                .await;
        } else {
            self.sync_pane_active_from_store(outcome.src_workspace, src_pane)
                .await;
        }

        if let Some(ws) = self.store.get_workspace(outcome.dst_workspace).await {
            self.refresh_workspace_solo(&ws);
        }
        self.sync_workspace_agent_status_from_store(outcome.dst_workspace)
            .await;
        if outcome.src_workspace != outcome.dst_workspace && !outcome.src_workspace_removed {
            self.sync_workspace_agent_status_from_store(outcome.src_workspace)
                .await;
        }
        self.refresh_window_title().await;
        self.focus_pane(dst_pane);
        Ok(())
    }
    /// Move a live surface tab to the last position of the first pane of
    /// `dst_workspace`, then bring that workspace to the front. Backs the
    /// right-click "Move" submenu and a drop directly onto a side-panel row.
    pub(super) async fn move_surface_to_workspace(
        &self,
        src_pane: PaneId,
        surface: SurfaceId,
        dst_workspace: WorkspaceId,
    ) -> Result<(), String> {
        let dst_pane = self
            .store
            .get_workspace(dst_workspace)
            .await
            .and_then(|ws| {
                ws.surfaces
                    .first()
                    .and_then(|s| s.root_pane.first_leaf_id())
            })
            .ok_or_else(|| "destination workspace has no pane".to_string())?;
        let res = self
            .move_surface(src_pane, surface, None, dst_pane, usize::MAX)
            .await;
        if res.is_ok() {
            self.activate_workspace(dst_workspace).await;
        }
        res
    }
    /// Build a tab widget for `surface` in `dst_pane` and mount the detached
    /// live widget there, aligning the tab order with the model index.
    pub(super) async fn mount_moved_surface(
        &self,
        dst_pane: PaneId,
        dst_workspace: WorkspaceId,
        surface: SurfaceId,
        moving: MovingSurface,
        target_index: usize,
    ) -> bool {
        let surface_model = self
            .store
            .get_workspace(dst_workspace)
            .await
            .and_then(|ws| {
                ws.surfaces
                    .iter()
                    .find_map(|s| s.root_pane.find_surface(dst_pane, surface))
            });
        let Some(surface_model) = surface_model else {
            return false;
        };
        let (tab, label) =
            build_surface_tab_widget(dst_pane, &surface_model, true, &self.callbacks);
        let ok = self.pane_registry.borrow_mut().attach_moved_surface(
            dst_pane,
            dst_workspace,
            moving,
            tab.upcast(),
            label,
        );
        if ok {
            self.pane_registry
                .borrow_mut()
                .reorder_surface_widget(dst_pane, surface, target_index);
        }
        ok
    }
    /// Best-effort restore of a detached surface back into its source pane when
    /// a move is rejected by the store.
    pub(super) async fn reattach_surface(&self, src_pane: PaneId, surface: SurfaceId, moving: MovingSurface) {
        let Some(ws_id) = self.store.workspace_of_pane(src_pane).await else {
            return;
        };
        let _ = self
            .mount_moved_surface(src_pane, ws_id, surface, moving, usize::MAX)
            .await;
    }
    pub(super) async fn sync_pane_active_from_store(&self, workspace: WorkspaceId, pane: PaneId) {
        let Some(ws) = self.store.get_workspace(workspace).await else {
            return;
        };
        let Some(active) = active_surface_from_workspace(&ws, pane) else {
            return;
        };
        self.pane_registry
            .borrow_mut()
            .activate_surface(pane, active);
    }
    pub(super) async fn import_surface_to_pane(
        &self,
        surface_model: flowmux_core::PaneSurface,
        dst_pane: PaneId,
        target_index: usize,
    ) -> Result<(), String> {
        if !self.pane_registry.borrow().has_pane(dst_pane) {
            return Err("destination pane is not rendered".to_string());
        }

        let Some((ws_id, surface_id)) = self
            .store
            .import_surface_to_pane(dst_pane, surface_model, target_index)
            .await
        else {
            return Err("destination pane no longer exists".to_string());
        };

        self.attach_or_rerender_surface(ws_id, dst_pane, surface_id)
            .await;
        if let Some(ws) = self.store.get_workspace(ws_id).await {
            self.refresh_workspace_solo(&ws);
        }
        self.sync_workspace_agent_status_from_store(ws_id).await;
        self.refresh_window_title().await;
        self.focus_pane(dst_pane);
        Ok(())
    }
    pub(super) async fn split_imported_surface_into_pane(
        &self,
        surface_model: flowmux_core::PaneSurface,
        dst_pane: PaneId,
        direction: SplitDirection,
    ) -> Result<(), String> {
        if !self.pane_registry.borrow().has_pane(dst_pane) {
            return Err("destination pane is not rendered".to_string());
        }

        let Some((ws_id, new_pane, _surface_id)) = self
            .store
            .split_imported_surface_into_pane(dst_pane, surface_model, direction)
            .await
        else {
            return Err("destination pane no longer exists".to_string());
        };
        let Some(ws) = self.store.get_workspace(ws_id).await else {
            return Err("destination workspace vanished".to_string());
        };
        let Some(new_split_id) = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.parent_split_id(new_pane))
        else {
            return Err("could not locate the new split node".to_string());
        };
        let Some(content) = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.find_leaf_content(new_pane))
        else {
            return Err("could not locate imported surface content".to_string());
        };

        let stack_name = ws.id.to_string();
        match split_pane_incremental(
            ws.id,
            dst_pane,
            new_pane,
            new_split_id,
            direction,
            0.5,
            content,
            Some(ws.root_dir.clone()),
            &stack_name,
            &self.callbacks,
            self.pane_registry.clone(),
            self.theme.clone(),
            false,
        ) {
            IncrementalSplitOutcome::SucceededRoot { new_root } => {
                self.surfaces.borrow_mut().insert(ws.id, new_root);
            }
            IncrementalSplitOutcome::SucceededNested => {}
            IncrementalSplitOutcome::Failed => {
                return Err("incremental split failed".to_string());
            }
        }

        self.refresh_workspace_solo(&ws);
        self.sync_workspace_agent_status_from_store(ws.id).await;
        self.refresh_window_title().await;
        self.focus_pane(new_pane);
        Ok(())
    }
    /// Split `dst_pane` and move the dragged tab (with its live state) into the
    /// new sibling. Backs dropping a tab on the right / bottom region of a pane.
    pub(super) async fn split_surface_into_pane(
        &self,
        src_pane: PaneId,
        surface: SurfaceId,
        surface_model: Option<flowmux_core::PaneSurface>,
        dst_pane: PaneId,
        direction: SplitDirection,
    ) -> Result<(), String> {
        if !self.pane_registry.borrow().has_pane(dst_pane) {
            return Err("destination pane is not rendered".to_string());
        }

        let moving = self
            .pane_registry
            .borrow_mut()
            .detach_surface_for_move(src_pane, surface);
        let Some(moving) = moving else {
            if let Some(surface_model) = surface_model {
                return self
                    .split_imported_surface_into_pane(surface_model, dst_pane, direction)
                    .await;
            }
            return Err("source surface is not rendered".to_string());
        };

        let outcome = match self
            .store
            .split_surface_into_pane(src_pane, surface, dst_pane, direction)
            .await
        {
            Some(outcome) => outcome,
            None => {
                self.reattach_surface(src_pane, surface, moving).await;
                return Err("destination pane no longer exists".to_string());
            }
        };
        let dst_ws = outcome.dst_workspace;
        let new_pane = outcome.new_pane;

        // Build the new sibling pane empty, then mount the live tab into it.
        let Some(ws) = self.store.get_workspace(dst_ws).await else {
            return Err("destination workspace vanished".to_string());
        };
        let new_split_id = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.parent_split_id(new_pane));
        let Some(new_split_id) = new_split_id else {
            // Could not locate the split node; fall back to a plain tab move so
            // the live widget is not lost.
            self.mount_moved_surface(dst_pane, dst_ws, surface, moving, usize::MAX)
                .await;
            return Err("could not locate the new split node".to_string());
        };

        let empty = flowmux_core::PaneContent::Tabs {
            active: surface,
            surfaces: Vec::new(),
        };
        let stack_name = ws.id.to_string();
        let split_outcome = split_pane_incremental(
            ws.id,
            dst_pane,
            new_pane,
            new_split_id,
            direction,
            0.5,
            empty,
            Some(ws.root_dir.clone()),
            &stack_name,
            &self.callbacks,
            self.pane_registry.clone(),
            self.theme.clone(),
            true,
        );
        match split_outcome {
            IncrementalSplitOutcome::SucceededRoot { new_root } => {
                self.surfaces.borrow_mut().insert(ws.id, new_root);
            }
            IncrementalSplitOutcome::SucceededNested => {}
            IncrementalSplitOutcome::Failed => {
                // Sibling not built; keep the tab by re-homing it into dst.
                self.mount_moved_surface(dst_pane, dst_ws, surface, moving, usize::MAX)
                    .await;
                return Err("incremental split failed".to_string());
            }
        }

        let mounted = self
            .mount_moved_surface(new_pane, dst_ws, surface, moving, 0)
            .await;
        if !mounted {
            return Err("failed to mount moved surface".to_string());
        }

        if outcome.src_workspace_removed {
            self.drop_workspace(outcome.src_workspace);
            self.activate_active_or_show_empty().await;
        } else if outcome.src_pane_removed {
            self.apply_close_pane_incremental_or_rerender(outcome.src_workspace, src_pane)
                .await;
        } else {
            self.sync_pane_active_from_store(outcome.src_workspace, src_pane)
                .await;
        }

        if let Some(ws) = self.store.get_workspace(dst_ws).await {
            self.refresh_workspace_solo(&ws);
        }
        self.sync_workspace_agent_status_from_store(dst_ws).await;
        if outcome.src_workspace != dst_ws && !outcome.src_workspace_removed {
            self.sync_workspace_agent_status_from_store(outcome.src_workspace)
                .await;
        }
        self.refresh_window_title().await;
        self.focus_pane(new_pane);
        Ok(())
    }
}
