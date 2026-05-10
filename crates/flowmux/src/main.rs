// SPDX-License-Identifier: GPL-3.0-or-later
//! flowmux GUI entrypoint.
//!
//! Boots libadwaita, brings up the IPC server and the state store on a
//! tokio runtime, and wires GTK-affecting verbs to the GTK main loop
//! through an [`async_channel`] bridge.

mod bridge;
mod ipc_handler;
mod keybindings;
mod notifications;
mod theme;
mod ui;

use adw::prelude::*;
use bridge::Bridge;
use flowmux_config::paths;
use flowmux_daemon::{DaemonHandler, StateStore};
use ipc_handler::GuiHandler;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tracing::info;
use ui::{spawn_dispatch_loop, WindowController};

const APP_ID: &str = "com.flowmux.App";

fn main() -> anyhow::Result<()> {
    if delegate_to_cli_if_needed()? {
        return Ok(());
    }

    // Release builds stay quiet — only ERROR events surface so a
    // packaged binary doesn't spam the user's terminal/journald with
    // info/debug noise. `FLOWMUX_LOG` still overrides for users who
    // need to debug a production issue.
    let default_filter = if cfg!(debug_assertions) {
        "info,flowmux=debug"
    } else {
        "error"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOWMUX_LOG")
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    // WebKitGTK 6.0's default sandbox starts bwrap + xdg-dbus-proxy. On
    // Ubuntu 24.04 with unprivileged user namespaces restricted by AppArmor,
    // bwrap fails with "setting up uid map: Permission denied", the proxy
    // exits with code 1, and the first browser tab creation aborts the app.
    // Set the sandbox bypass before the first WebView unless the user has
    // already set an explicit value.
    if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
        std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
    }

    // WebKitGTK 6.0's DMA-BUF renderer has a known shutdown race on some
    // Mesa Wayland setups: missing `eglDestroySync` can lead to a later
    // `corrupted size vs. prev_size` glibc abort when libepoxy cannot find
    // EGL 1.5 or EGL_KHR_fence_sync. Unless the user has chosen otherwise,
    // disabling the DMA-BUF renderer is the safest default and only
    // simplifies shutdown cleanup.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // One socket per GUI process so several flowmux windows can run
    // side-by-side without their notifications crossing over. The
    // `FLOWMUX_SOCKET_PATH` env we inject into every PTY carries this
    // exact path, so terminal-side hooks (Claude/Codex/OpenCode) talk
    // back to the GUI that spawned them.
    let socket = paths::runtime_socket_for_pid(std::process::id());
    info!(?socket, "flowmux starting");

    // Tokio runtime hosts the IPC server, the state store, and any
    // async desktop-bus interactions.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let initial = match flowmux_state::load() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not load state, starting empty");
            flowmux_state::State::default()
        }
    };
    let store = StateStore::new_lazy(initial);
    store.spawn_persist(rt.handle());

    let (bridge, rx) = Bridge::new();
    let handler = Arc::new(GuiHandler::new(
        DaemonHandler::new(store.clone()),
        bridge.clone(),
    ));

    let socket_clone = socket.clone();
    rt.spawn(async move {
        if let Err(e) = flowmux_ipc::server::run(&socket_clone, handler).await {
            tracing::error!(error = %e, "ipc server exited");
        }
    });

    // GTK runs on the main thread.
    let app = adw::Application::builder().application_id(APP_ID).build();
    keybindings::install_accels(&app);
    let store_for_activate = store.clone();
    let rx_for_activate = rx.clone();
    let bridge_for_activate = bridge.clone();
    app.connect_activate(move |app| {
        gtk::Window::set_default_icon_name(APP_ID);

        // Resolve the visual theme once per activation so a config edit
        // picks up after the user re-launches.
        let theme = std::sync::Arc::new(theme::ResolvedTheme::load());

        // Force libadwaita's color scheme to match our background.
        let style = adw::StyleManager::default();
        if theme.is_dark() {
            style.set_color_scheme(adw::ColorScheme::ForceDark);
        } else {
            style.set_color_scheme(adw::ColorScheme::ForceLight);
        }

        // Install the global CSS for pane frames + sidebar tint. Seed it with
        // the current focus border options; the options dialog later reloads
        // this same provider so changes apply immediately.
        let initial_options = flowmux_config::options::load();
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&theme.css(
            initial_options.focus_border_color_or_default(),
            initial_options.focus_border_alpha(),
        ));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let controller = WindowController::new(
            app,
            store_for_activate.clone(),
            theme,
            bridge_for_activate.clone(),
            provider,
        );
        keybindings::install_actions(
            &controller.window,
            bridge_for_activate.clone(),
            controller.focused_pane.clone(),
            controller.pane_registry(),
            controller.clipboard_toast(),
        );
        spawn_dispatch_loop(rx_for_activate.clone(), controller.clone());
        let controller_for_init = controller.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            controller_for_init.restore_from_store().await;
            controller_for_init.show_status_when_empty();
        });
        controller.window.present();
    });

    let exit_code = app.run();
    drop(rt);
    let _ = std::fs::remove_file(&socket);
    std::process::exit(exit_code.into());
}

fn delegate_to_cli_if_needed() -> anyhow::Result<bool> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return Ok(false);
    }

    let current_exe = std::env::current_exe().ok();
    let sibling = current_exe
        .as_ref()
        .map(|p| p.with_file_name("flowmuxctl"))
        .filter(|p| p.is_file());
    let private_install = current_exe
        .as_ref()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|prefix| prefix.join("lib").join("flowmux").join("flowmuxctl"))
        .filter(|p| p.is_file());
    let program = sibling
        .or(private_install)
        .unwrap_or_else(|| PathBuf::from("flowmuxctl"));

    #[cfg(unix)]
    {
        use anyhow::Context;
        use std::os::unix::process::CommandExt;

        let err = Command::new(&program).args(args).exec();
        Err::<(), _>(err).with_context(|| format!("launch {}", program.display()))?;
    }

    #[cfg(not(unix))]
    {
        use anyhow::Context;

        let status = Command::new(&program)
            .args(args)
            .status()
            .with_context(|| format!("launch {}", program.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(true)
}
