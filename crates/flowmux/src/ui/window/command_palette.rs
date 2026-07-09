// SPDX-License-Identifier: GPL-3.0-or-later
//! Command palette + project command execution + config reload.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

impl WindowController {
    /// Connect (lazily) to the `org.gtk.Notifications` service and ask
    /// it to withdraw the given `desktop_id`s. Used by the bell popover
    /// sweep, the workspace-activation sweep, the OpenNotification
    /// click, and the late-arriving SetNotificationDesktopId race fix.
    /// On GNOME this destroys the `MessageTray.Source` entry, which is
    /// what actually shrinks Ubuntu Dock's per-app notification count —
    /// the legacy FDO `CloseNotification` path used to leave both the
    /// message-tray entry and the badge stuck.
    pub(super) fn show_command_palette(&self) {
        let dialog = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title("Command Palette")
            .default_width(360)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_spacing(6);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        dialog.set_child(Some(&content));

        for command in command_palette_commands() {
            let button = gtk::Button::with_label(command_palette_label(*command));
            button.set_hexpand(true);
            button.set_halign(gtk::Align::Fill);
            let controller = self.clone();
            let dialog_for_click = dialog.clone();
            let command = *command;
            button.connect_clicked(move |_| {
                dialog_for_click.close();
                let controller = controller.clone();
                glib::MainContext::default().spawn_local(async move {
                    controller.run_command_palette_command(command).await;
                });
            });
            content.append(&button);
        }

        if let Some((base_dir, config)) = self.command_palette_project_config() {
            for command in config.commands.clone() {
                let button = gtk::Button::with_label(&command.label);
                button.set_hexpand(true);
                button.set_halign(gtk::Align::Fill);
                let controller = self.clone();
                let dialog_for_click = dialog.clone();
                let env = config.env.clone();
                let base_dir = base_dir.clone();
                button.connect_clicked(move |_| {
                    dialog_for_click.close();
                    let controller = controller.clone();
                    let command = command.clone();
                    let env = env.clone();
                    let base_dir = base_dir.clone();
                    glib::MainContext::default().spawn_local(async move {
                        controller.run_project_command(base_dir, env, command).await;
                    });
                });
                content.append(&button);
            }
        }

        dialog.present();
    }
    pub(super) fn command_palette_project_config(&self) -> Option<(std::path::PathBuf, CmuxJson)> {
        let base_dir = self
            .focused_pane
            .get()
            .and_then(|pane| self.pane_registry.borrow().current_dir_for_pane(pane))
            .or_else(|| std::env::current_dir().ok())?;

        match flowmux_config::cmux_json::load_from_dir(&base_dir) {
            Ok(Some(config)) => Some((base_dir, config)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, dir = %base_dir.display(), "failed to load cmux.json");
                None
            }
        }
    }
    pub(super) async fn run_command_palette_command(&self, command: CommandPaletteCommand) {
        match command {
            CommandPaletteCommand::OpenBrowser => {
                if let Some(pane) = self.focused_pane.get() {
                    self.dispatch(GtkCommand::NewBrowserSurface { pane }).await;
                }
            }
            CommandPaletteCommand::RenameTab => {
                if let Some(pane) = self.focused_pane.get() {
                    let surface = self.pane_registry.borrow().active_surface(pane);
                    if let Some(surface) = surface {
                        self.dispatch(GtkCommand::ShowRenameSurfaceDialog { pane, surface })
                            .await;
                    }
                }
            }
            CommandPaletteCommand::ReloadConfig => {
                self.reload_runtime_config();
            }
            CommandPaletteCommand::OpenUnread => {
                let id = self
                    .notifications
                    .entries()
                    .into_iter()
                    .find(|entry| !entry.read)
                    .map(|entry| entry.id);
                if let Some(id) = id {
                    self.open_notification_id(id).await;
                }
            }
        }
    }
    pub(super) async fn run_project_command(
        &self,
        base_dir: std::path::PathBuf,
        env: std::collections::BTreeMap<String, String>,
        command: CustomCommand,
    ) {
        let Some(line) = custom_command_shell_line(&base_dir, &env, &command) else {
            tracing::warn!(id = %command.id, "project command has empty run argv");
            return;
        };

        if command.confirm && !self.confirm_project_command(&command).await {
            return;
        }

        let cwd = custom_command_cwd(&base_dir, &command);
        let Some(pane) = self
            .prepare_project_command_target(command.target, cwd)
            .await
        else {
            tracing::warn!(id = %command.id, "project command had no target pane");
            return;
        };

        let (ack_tx, ack_rx) = oneshot::channel();
        self.dispatch(GtkCommand::PaneSendKeys {
            pane,
            keys: format!("{line}\r"),
            ack: ack_tx,
        })
        .await;
        if let Ok(Err(e)) = ack_rx.await {
            tracing::warn!(error = %e, id = %command.id, "project command send failed");
        }
    }
    pub(super) async fn prepare_project_command_target(
        &self,
        target: CommandTarget,
        cwd: std::path::PathBuf,
    ) -> Option<PaneId> {
        let pane = self.focused_pane.get()?;
        match target {
            CommandTarget::FocusedPane => Some(pane),
            CommandTarget::NewSurface => {
                let (ws_id, surface_id) = self
                    .store
                    .add_terminal_surface_to_pane(pane, Some(cwd))
                    .await?;
                self.attach_or_rerender_surface(ws_id, pane, surface_id)
                    .await;
                Some(pane)
            }
            CommandTarget::SplitDown | CommandTarget::SplitRight => {
                let direction = match target {
                    CommandTarget::SplitDown => SplitDirection::Horizontal,
                    CommandTarget::SplitRight => SplitDirection::Vertical,
                    _ => unreachable!(),
                };
                let (ws_id, new_pane) = self.store.split_pane(pane, direction).await?;
                self.apply_split_incremental_or_rerender(ws_id, pane, new_pane, direction)
                    .await;
                Some(new_pane)
            }
        }
    }
    pub(super) async fn confirm_project_command(&self, command: &CustomCommand) -> bool {
        let dialog = adw::AlertDialog::new(
            Some("Run command?"),
            Some(&format!("{}: {}", command.label, command.run.join(" "))),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("run", "Run");
        dialog.set_default_response(Some("run"));
        dialog.set_close_response("cancel");

        let (tx, rx) = oneshot::channel();
        let tx = Rc::new(RefCell::new(Some(tx)));
        dialog.connect_response(None, move |dialog, response| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(response == "run");
            }
            dialog.close();
        });
        let _native_browser_suspend =
            crate::ui::browser_pane::suspend_native_browser_views_for_window(
                self.window.upcast_ref(),
            );
        dialog.present(Some(&self.window));
        rx.await.unwrap_or(false)
    }
    pub(super) fn reload_runtime_config(&self) {
        let opts = flowmux_config::options::load();
        *self.options.borrow_mut() = opts.clone();

        let font = self
            .theme
            .font_with_overrides(opts.font_family.as_deref(), opts.font_size);
        let registry = self.pane_registry.borrow();
        for terminal in registry.terminals.values() {
            terminal.set_font(&font);
            terminal.set_font_scale(opts.zoom_factor());
            terminal.set_cursor_blink(opts.cursor_blink, opts.cursor_blink_interval_ms);
        }
        for browser in registry.browsers.values() {
            browser.set_zoom_level(opts.zoom_factor());
        }
        drop(registry);

        self.css_provider.load_from_string(&self.theme.css(
            opts.focus_border_color_or_default(),
            opts.focus_border_alpha(),
        ));

        if let Some(app) = self
            .window
            .application()
            .and_then(|a| a.downcast::<adw::Application>().ok())
        {
            crate::keybindings::install_accels(&app, &opts);
        } else {
            tracing::warn!(
                "config reloaded without keybinding re-install — window had no Application"
            );
        }
    }
}
