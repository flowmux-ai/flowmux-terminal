// SPDX-License-Identifier: GPL-3.0-or-later
//! Command palette + project command execution + config reload.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

fn fuzzy_matches_prepared(query: &str, candidate: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    query.split_whitespace().all(|token| {
        let mut chars = candidate.chars();
        token.chars().all(|needle| chars.any(|ch| ch == needle))
    })
}

#[cfg(test)]
fn fuzzy_matches(query: &str, candidate: &str) -> bool {
    fuzzy_matches_prepared(&query.to_lowercase(), &candidate.to_lowercase())
}

type PaletteEntry = (String, String, gtk::Button);

fn palette_entry(search_text: String, button: gtk::Button) -> PaletteEntry {
    let normalized = search_text.to_lowercase();
    (search_text, normalized, button)
}

fn palette_button(label: &str, shortcut: Option<&str>) -> gtk::Button {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    row.append(&title);
    if let Some(shortcut) = shortcut {
        let shortcut = gtk::Label::new(Some(shortcut));
        shortcut.add_css_class("dim-label");
        row.append(&shortcut);
    }
    let button = gtk::Button::new();
    button.set_child(Some(&row));
    button.set_hexpand(true);
    button.set_halign(gtk::Align::Fill);
    button
}

fn accelerator_label(accelerator: &str) -> Option<String> {
    gtk::accelerator_parse(accelerator)
        .map(|(key, modifiers)| gtk::accelerator_get_label(key, modifiers).to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceTemplate {
    id: String,
    label: String,
    panes: Vec<TemplatePane>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TemplatePane {
    id: String,
    target: Option<String>,
    split: TemplateSplit,
    content: TemplatePaneContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TemplateSplit {
    Right,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TemplatePaneContent {
    Terminal {
        run: Vec<String>,
        cwd: Option<String>,
    },
    Browser {
        url: String,
    },
}

fn record_palette_mru(mru: &mut std::collections::VecDeque<String>, key: &str) {
    if let Some(index) = mru.iter().position(|entry| entry == key) {
        mru.remove(index);
    }
    mru.push_front(key.to_string());
    mru.truncate(20);
}

fn quick_open_files(root: &std::path::Path, limit: usize) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut directories = std::collections::VecDeque::from([root.to_path_buf()]);
    while let Some(directory) = directories.pop_front() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                let name = entry.file_name();
                if !matches!(name.to_str(), Some(".git" | "target" | "node_modules")) {
                    directories.push_back(path);
                }
            } else if file_type.is_file() {
                files.push(path);
                if files.len() >= limit {
                    files.sort_by_key(|path| (path.components().count(), path.clone()));
                    return files;
                }
            }
        }
    }
    files.sort_by_key(|path| (path.components().count(), path.clone()));
    files
}

pub(super) fn development_workspace_template() -> WorkspaceTemplate {
    WorkspaceTemplate {
        id: "agent-tests-browser".into(),
        label: "Agent + tests + browser".into(),
        panes: vec![
            TemplatePane {
                id: "agent".into(),
                target: None,
                split: TemplateSplit::Right,
                content: TemplatePaneContent::Terminal {
                    run: Vec::new(),
                    cwd: None,
                },
            },
            TemplatePane {
                id: "browser".into(),
                target: Some("agent".into()),
                split: TemplateSplit::Right,
                content: TemplatePaneContent::Browser {
                    url: "about:blank".into(),
                },
            },
            TemplatePane {
                id: "tests".into(),
                target: Some("agent".into()),
                split: TemplateSplit::Down,
                content: TemplatePaneContent::Terminal {
                    run: Vec::new(),
                    cwd: None,
                },
            },
        ],
    }
}

pub(super) fn workspace_template_preview(template: &WorkspaceTemplate) -> String {
    template
        .panes
        .iter()
        .map(|pane| {
            let detail = match &pane.content {
                TemplatePaneContent::Terminal { run, .. } if run.is_empty() => "terminal".into(),
                TemplatePaneContent::Terminal { run, .. } => {
                    format!("terminal — {}", run.join(" "))
                }
                TemplatePaneContent::Browser { url } => format!("browser — {url}"),
            };
            format!("• {}: {detail}", pane.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn template_terminal_shell_line(
    base_dir: &std::path::Path,
    env: &std::collections::BTreeMap<String, String>,
    run: &[String],
    cwd: Option<&str>,
) -> Option<String> {
    if run.is_empty() && cwd.is_none() {
        return None;
    }
    let command = CustomCommand {
        id: "workspace-template".into(),
        label: "Workspace template".into(),
        run: run.to_vec(),
        cwd: cwd.map(str::to_string),
        target: CommandTarget::FocusedPane,
        confirm: false,
    };
    if run.is_empty() {
        Some(format!(
            "cd {}",
            shell_quote(&custom_command_cwd(base_dir, &command).to_string_lossy())
        ))
    } else {
        custom_command_shell_line(base_dir, env, &command)
    }
}

#[derive(Debug)]
pub(super) struct MaterializedWorkspaceTemplate {
    pub(super) workspace: WorkspaceId,
    pub(super) terminal_commands: Vec<(PaneId, String)>,
}

pub(super) async fn materialize_workspace_template(
    store: &StateStore,
    base_dir: &std::path::Path,
    env: &std::collections::BTreeMap<String, String>,
    template: &WorkspaceTemplate,
) -> Result<MaterializedWorkspaceTemplate, String> {
    let workspace = store
        .create_workspace(Some(template.label.clone()), base_dir.to_path_buf())
        .await;
    store
        .rename_workspace(workspace, template.label.clone())
        .await;
    let result = async {
        let model = store
            .get_workspace(workspace)
            .await
            .ok_or_else(|| "new workspace disappeared".to_string())?;
        let first_pane = model
            .surfaces
            .first()
            .and_then(|surface| surface.root_pane.first_leaf_id())
            .ok_or_else(|| "new workspace has no pane".to_string())?;
        let first_id = template
            .panes
            .first()
            .map(|pane| pane.id.clone())
            .ok_or_else(|| "workspace template has no panes".to_string())?;
        let mut panes = std::collections::HashMap::from([(first_id.clone(), first_pane)]);
        let mut terminal_commands = Vec::new();

        for (index, definition) in template.panes.iter().enumerate() {
            let pane = if index == 0 {
                first_pane
            } else {
                let target_name = definition.target.as_deref().unwrap_or(&first_id);
                let target = panes
                    .get(target_name)
                    .copied()
                    .ok_or_else(|| format!("unknown template target '{target_name}'"))?;
                let direction = match definition.split {
                    TemplateSplit::Right => SplitDirection::Vertical,
                    TemplateSplit::Down => SplitDirection::Horizontal,
                };
                let created = match &definition.content {
                    TemplatePaneContent::Terminal { .. } => {
                        store.split_pane(target, direction).await
                    }
                    TemplatePaneContent::Browser { url } => {
                        store
                            .split_pane_with_browser(target, direction, url.clone())
                            .await
                    }
                };
                let (created_workspace, pane) =
                    created.ok_or_else(|| format!("failed to create pane '{}'", definition.id))?;
                if created_workspace != workspace {
                    return Err("template split targeted another workspace".into());
                }
                panes.insert(definition.id.clone(), pane);
                pane
            };

            let surface = if index == 0 {
                match &definition.content {
                    TemplatePaneContent::Terminal { .. } => active_surface_from_workspace(
                        &store
                            .get_workspace(workspace)
                            .await
                            .ok_or_else(|| "new workspace disappeared".to_string())?,
                        pane,
                    )
                    .ok_or_else(|| "first terminal pane has no surface".to_string())?,
                    TemplatePaneContent::Browser { url } => {
                        let model = store
                            .get_workspace(workspace)
                            .await
                            .ok_or_else(|| "new workspace disappeared".to_string())?;
                        let terminal = active_surface_from_workspace(&model, pane)
                            .ok_or_else(|| "first pane has no surface".to_string())?;
                        let (_, browser) = store
                            .add_browser_surface_to_pane(pane, url.clone())
                            .await
                            .ok_or_else(|| "failed to create first browser pane".to_string())?;
                        store
                            .close_surface(pane, terminal)
                            .await
                            .ok_or_else(|| "failed to remove placeholder terminal".to_string())?;
                        browser
                    }
                }
            } else {
                let model = store
                    .get_workspace(workspace)
                    .await
                    .ok_or_else(|| "new workspace disappeared".to_string())?;
                active_surface_from_workspace(&model, pane)
                    .ok_or_else(|| format!("pane '{}' has no surface", definition.id))?
            };
            store
                .rename_surface(pane, surface, definition.id.clone())
                .await
                .ok_or_else(|| format!("failed to label pane '{}'", definition.id))?;

            if let TemplatePaneContent::Terminal { run, cwd } = &definition.content {
                if let Some(line) = template_terminal_shell_line(base_dir, env, run, cwd.as_deref())
                {
                    terminal_commands.push((pane, line));
                }
            }
        }

        Ok(MaterializedWorkspaceTemplate {
            workspace,
            terminal_commands,
        })
    }
    .await;

    if result.is_err() {
        store.remove_workspace(workspace).await;
    }
    result
}

impl WindowController {
    /// Connect (lazily) to the `org.gtk.Notifications` service and ask
    /// it to withdraw the given `desktop_id`s. Used by the bell popover
    /// sweep, the workspace-activation sweep, the OpenNotification
    /// click, and the late-arriving SetNotificationDesktopId race fix.
    /// On GNOME this destroys the `MessageTray.Source` entry, which is
    /// what actually shrinks Ubuntu Dock's per-app notification count —
    /// the legacy FDO `CloseNotification` path used to leave both the
    /// message-tray entry and the badge stuck.
    pub(super) async fn show_command_palette(&self) {
        let workspaces = self.store.ordered_workspaces().await;
        let quick_open_root = self.store.active_or_first().await.and_then(|active| {
            workspaces
                .iter()
                .find(|workspace| workspace.id == active)
                .map(|workspace| workspace.root_dir.clone())
        });
        let project_config = self.command_palette_project_config();
        let dialog = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title("Command Palette")
            .default_width(480)
            .default_height(420)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_spacing(6);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        dialog.set_child(Some(&content));

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search commands…")
            .build();
        content.append(&search);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        content.append(&scroll);
        let no_results = gtk::Label::new(Some("No matching commands"));
        no_results.add_css_class("dim-label");
        no_results.set_visible(false);
        content.append(&no_results);

        let mut entries = Vec::new();

        for command in command_palette_commands() {
            let label = command_palette_label(*command);
            let shortcut = match command {
                CommandPaletteCommand::OpenBrowser => Some("Ctrl+Shift+B"),
                _ => None,
            };
            let button = palette_button(label, shortcut);
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
            list.append(&button);
            entries.push(palette_entry(label.to_string(), button));
        }

        let builtin_template = development_workspace_template();
        let builtin_base_dir = self
            .focused_pane
            .get()
            .and_then(|pane| self.pane_registry.borrow().current_dir_for_pane(pane))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let label = format!("Workspace template: {}", builtin_template.label);
        let button = palette_button(&label, None);
        let controller = self.clone();
        let dialog_for_click = dialog.clone();
        button.connect_clicked(move |_| {
            dialog_for_click.close();
            let controller = controller.clone();
            let template = builtin_template.clone();
            let base_dir = builtin_base_dir.clone();
            glib::MainContext::default().spawn_local(async move {
                controller
                    .run_workspace_template(base_dir, std::collections::BTreeMap::new(), template)
                    .await;
            });
        });
        list.append(&button);
        entries.push(palette_entry(label, button));
        // Keep the asynchronous file section at its original position even if
        // MRU ordering moves the template button before the scan completes.
        let quick_open_anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
        quick_open_anchor.set_visible(false);
        list.append(&quick_open_anchor);

        for workspace in &workspaces {
            let workspace_name = workspace.custom_title.as_deref().unwrap_or(&workspace.name);
            let label = format!("Workspace: {workspace_name}");
            let button = palette_button(&label, None);
            let controller = self.clone();
            let dialog_for_click = dialog.clone();
            let command = CommandPaletteCommand::ActivateWorkspace(workspace.id);
            button.connect_clicked(move |_| {
                dialog_for_click.close();
                let controller = controller.clone();
                glib::MainContext::default().spawn_local(async move {
                    controller.run_command_palette_command(command).await;
                });
            });
            list.append(&button);
            entries.push(palette_entry(label, button));

            let mut pane_ids = Vec::new();
            for surface_root in &workspace.surfaces {
                surface_root
                    .root_pane
                    .for_each_leaf(|pane| pane_ids.push(pane));
            }
            for pane in pane_ids {
                let active_surface = workspace
                    .surfaces
                    .iter()
                    .find_map(|root| root.root_pane.active_surface_id(pane));
                let pane_title = active_surface
                    .and_then(|surface| {
                        workspace
                            .surfaces
                            .iter()
                            .find_map(|root| root.root_pane.surface_title(pane, surface))
                    })
                    .unwrap_or("Pane");
                let label = format!("Pane: {workspace_name} / {pane_title}");
                let button = palette_button(&label, None);
                let controller = self.clone();
                let dialog_for_click = dialog.clone();
                let command = CommandPaletteCommand::FocusPane {
                    workspace: workspace.id,
                    pane,
                };
                button.connect_clicked(move |_| {
                    dialog_for_click.close();
                    let controller = controller.clone();
                    glib::MainContext::default().spawn_local(async move {
                        controller.run_command_palette_command(command).await;
                    });
                });
                list.append(&button);
                entries.push(palette_entry(label, button));

                for root in &workspace.surfaces {
                    let Some(PaneContent::Tabs { surfaces, .. }) =
                        root.root_pane.find_leaf_content(pane)
                    else {
                        continue;
                    };
                    for surface in surfaces {
                        let label = format!(
                            "Tab: {} / {} / {}",
                            workspace_name, pane_title, surface.title
                        );
                        let button = palette_button(&label, None);
                        let controller = self.clone();
                        let dialog_for_click = dialog.clone();
                        let command = CommandPaletteCommand::ActivateSurface {
                            workspace: workspace.id,
                            pane,
                            surface: surface.id,
                        };
                        button.connect_clicked(move |_| {
                            dialog_for_click.close();
                            let controller = controller.clone();
                            glib::MainContext::default().spawn_local(async move {
                                controller.run_command_palette_command(command).await;
                            });
                        });
                        list.append(&button);
                        entries.push(palette_entry(label, button));
                    }
                }
            }
        }

        for (action, accelerators) in self.options.borrow().keybindings.resolve() {
            if action == flowmux_config::keybindings::ActionId::CommandPalette {
                continue;
            }
            let label = action.label();
            let shortcut = accelerators
                .first()
                .and_then(|accelerator| accelerator_label(accelerator));
            let button = palette_button(label, shortcut.as_deref());
            let controller = self.clone();
            let dialog_for_click = dialog.clone();
            button.connect_clicked(move |_| {
                dialog_for_click.close();
                let controller = controller.clone();
                glib::MainContext::default().spawn_local(async move {
                    controller
                        .run_command_palette_command(CommandPaletteCommand::Keybinding(action))
                        .await;
                });
            });
            list.append(&button);
            entries.push(palette_entry(
                format!("{} {}", label, action.as_str()),
                button,
            ));
        }

        if let Some((base_dir, config)) = project_config {
            for command in config.commands.clone() {
                let button = palette_button(&command.label, None);
                let search_text = format!("{} {}", command.label, command.id);
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
                list.append(&button);
                entries.push(palette_entry(search_text, button));
            }
        }

        for (search_text, _, button) in &entries {
            let key = search_text.clone();
            let mru = self.palette_mru.clone();
            button.connect_clicked(move |_| record_palette_mru(&mut mru.borrow_mut(), &key));
        }
        for key in self.palette_mru.borrow().iter().rev() {
            if let Some((_, _, button)) = entries
                .iter()
                .find(|(search_text, _, _)| search_text == key)
            {
                list.reorder_child_after(button, None::<&gtk::Widget>);
            }
        }

        let entries = Rc::new(RefCell::new(entries));
        let entries_for_search = entries.clone();
        let no_results_for_search = no_results.clone();
        search.connect_search_changed(move |entry| {
            let query = entry.text().to_lowercase();
            let mut any_visible = false;
            for (_, normalized, button) in entries_for_search.borrow().iter() {
                let visible = fuzzy_matches_prepared(&query, normalized);
                button.set_visible(visible);
                any_visible |= visible;
            }
            no_results_for_search.set_visible(!any_visible);
        });
        let entries_for_activate = entries.clone();
        search.connect_activate(move |_| {
            if let Some((_, _, button)) = entries_for_activate
                .borrow()
                .iter()
                .find(|(_, _, button)| button.is_visible())
            {
                button.emit_clicked();
            }
        });
        let dialog_for_stop = dialog.clone();
        search.connect_stop_search(move |_| dialog_for_stop.close());

        dialog.present();
        search.grab_focus();

        if let Some(root) = quick_open_root {
            let dialog = dialog.clone();
            let list = list.clone();
            let search = search.clone();
            let entries = entries.clone();
            let no_results = no_results.clone();
            let palette_mru = self.palette_mru.clone();
            let bridge = self.bridge.clone();
            let source_pane = self.focused_pane.get();
            glib::MainContext::default().spawn_local(async move {
                let worker_root = root.clone();
                let quick_open =
                    gtk::gio::spawn_blocking(move || quick_open_files(&worker_root, 2_000))
                        .await
                        .unwrap_or_default();
                let mut anchor = quick_open_anchor.upcast::<gtk::Widget>();
                let mut completed = true;

                for batch in quick_open.chunks(100) {
                    if !dialog.is_visible() {
                        completed = false;
                        break;
                    }
                    let query = search.text().to_lowercase();
                    for path in batch {
                        let relative = path.strip_prefix(&root).unwrap_or(path);
                        let label = format!("File: {}", relative.display());
                        let button = palette_button(&label, None);
                        button.set_visible(fuzzy_matches_prepared(&query, &label.to_lowercase()));
                        let dialog_for_click = dialog.clone();
                        let path = path.clone();
                        let bridge = bridge.clone();
                        button.connect_clicked(move |_| {
                            dialog_for_click.close();
                            let bridge = bridge.clone();
                            let path = path.clone();
                            glib::MainContext::default().spawn_local(async move {
                                let _ = bridge
                                    .tx
                                    .send(GtkCommand::OpenFileInEditor { path, source_pane })
                                    .await;
                            });
                        });
                        let key = label.clone();
                        let mru = palette_mru.clone();
                        button.connect_clicked(move |_| {
                            record_palette_mru(&mut mru.borrow_mut(), &key)
                        });
                        list.insert_child_after(&button, Some(&anchor));
                        anchor = button.clone().upcast();
                        entries.borrow_mut().push(palette_entry(label, button));
                    }
                    let any_visible = entries
                        .borrow()
                        .iter()
                        .any(|(_, _, button)| button.is_visible());
                    no_results.set_visible(!any_visible);
                    glib::timeout_future(std::time::Duration::from_millis(1)).await;
                }

                if completed && dialog.is_visible() {
                    for key in palette_mru.borrow().iter().rev() {
                        if let Some((_, _, button)) = entries
                            .borrow()
                            .iter()
                            .find(|(search_text, _, _)| search_text == key)
                        {
                            list.reorder_child_after(button, None::<&gtk::Widget>);
                        }
                    }
                }
            });
        }
    }
    async fn run_workspace_template(
        &self,
        base_dir: std::path::PathBuf,
        env: std::collections::BTreeMap<String, String>,
        template: WorkspaceTemplate,
    ) {
        if !self.confirm_workspace_template(&template).await {
            return;
        }
        let materialized =
            match materialize_workspace_template(&self.store, &base_dir, &env, &template).await {
                Ok(materialized) => materialized,
                Err(error) => {
                    tracing::warn!(template = %template.id, %error, "workspace template failed");
                    return;
                }
            };

        let Some(workspace) = self.store.get_workspace(materialized.workspace).await else {
            tracing::warn!(template = %template.id, "materialized workspace disappeared");
            return;
        };
        self.render_workspace(&workspace);
        self.activate_workspace(materialized.workspace).await;

        for (pane, line) in materialized.terminal_commands {
            let (ack_tx, ack_rx) = oneshot::channel();
            self.dispatch(GtkCommand::PaneSendKeys {
                pane,
                keys: format!("{line}\r"),
                ack: ack_tx,
            })
            .await;
            match ack_rx.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(template = %template.id, %pane, %error, "template command failed")
                }
                Err(error) => {
                    tracing::warn!(template = %template.id, %pane, %error, "template command ack dropped")
                }
            }
        }
    }

    async fn confirm_workspace_template(&self, template: &WorkspaceTemplate) -> bool {
        let dialog = adw::AlertDialog::new(
            Some("Create workspace from template?"),
            Some(&workspace_template_preview(template)),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("create", "Create");
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");

        let (tx, rx) = oneshot::channel();
        let tx = Rc::new(RefCell::new(Some(tx)));
        dialog.connect_response(None, move |dialog, response| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(response == "create");
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
            CommandPaletteCommand::Keybinding(action) => {
                let detailed_action = format!("win.{}", action.as_str());
                if let Err(error) =
                    gtk::prelude::WidgetExt::activate_action(&self.window, &detailed_action, None)
                {
                    tracing::warn!(
                        action = action.as_str(),
                        error = %error,
                        "command palette action failed"
                    );
                }
            }
            CommandPaletteCommand::ActivateWorkspace(workspace) => {
                self.activate_workspace(workspace).await;
            }
            CommandPaletteCommand::FocusPane { workspace, pane } => {
                self.activate_workspace(workspace).await;
                self.focus_pane(pane);
                self.on_pane_focused(pane).await;
            }
            CommandPaletteCommand::ActivateSurface {
                workspace,
                pane,
                surface,
            } => {
                self.activate_workspace(workspace).await;
                self.dispatch(GtkCommand::ActivateSurface { pane, surface })
                    .await;
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

        // Re-resolves the theme (preset + overrides), repaints terminals,
        // reapplies the font, and reloads the CSS provider.
        self.apply_runtime_theme(&opts);

        let registry = self.pane_registry.borrow();
        for terminal in registry.terminals.values() {
            terminal.set_font_scale(opts.zoom_factor());
            terminal.set_cursor_blink(opts.cursor_blink, opts.cursor_blink_interval_ms);
        }
        for browser in registry.browsers.values() {
            browser.set_zoom_level(opts.zoom_factor());
        }
        drop(registry);

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

#[cfg(test)]
mod tests {
    use super::{fuzzy_matches, quick_open_files, record_palette_mru};

    #[test]
    fn fuzzy_match_supports_subsequences_and_multiple_terms() {
        assert!(fuzzy_matches("op br", "Open browser"));
        assert!(fuzzy_matches("rn tab", "Rename tab"));
        assert!(fuzzy_matches("RLCFG", "Reload config"));
        assert!(!fuzzy_matches("browser rename", "Open browser"));
    }

    #[test]
    fn empty_fuzzy_query_matches_every_command() {
        assert!(fuzzy_matches("", "Open browser"));
        assert!(fuzzy_matches("   ", "Project test"));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn accelerator_labels_are_human_readable() {
        let label = super::accelerator_label("<Ctrl><Shift>b").unwrap();
        assert!(label.contains("Ctrl"));
        assert!(label.to_lowercase().contains('b'));
    }

    #[test]
    fn palette_mru_moves_existing_entries_and_caps_history() {
        let mut mru = std::collections::VecDeque::new();
        for index in 0..25 {
            record_palette_mru(&mut mru, &format!("command-{index}"));
        }
        record_palette_mru(&mut mru, "command-10");

        assert_eq!(mru.front().map(String::as_str), Some("command-10"));
        assert_eq!(mru.len(), 20);
        assert_eq!(
            mru.iter()
                .filter(|entry| entry.as_str() == "command-10")
                .count(),
            1
        );
    }

    #[test]
    fn quick_open_skips_build_trees_and_respects_limit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("one.rs"), "one").unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/two.rs"), "two").unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/generated.rs"), "generated").unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("outside.rs"), "outside").unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("external-link")).unwrap();

        let files = quick_open_files(root.path(), 2);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], root.path().join("one.rs"));
        assert!(files
            .iter()
            .all(|path| !path.starts_with(root.path().join("target"))));
        assert!(files
            .iter()
            .all(|path| !path.starts_with(root.path().join("external-link"))));
    }

    #[test]
    fn quick_open_limit_is_deterministic() {
        let root = tempfile::tempdir().unwrap();
        for name in ["z.rs", "b.rs", "a.rs"] {
            std::fs::write(root.path().join(name), name).unwrap();
        }

        assert_eq!(
            quick_open_files(root.path(), 2),
            vec![root.path().join("a.rs"), root.path().join("b.rs")]
        );
    }
}
