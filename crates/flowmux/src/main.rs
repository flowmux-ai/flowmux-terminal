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

/// Compile-time guard: the GApplication `application_id`, the FDO
/// `desktop-entry` hint, and the LauncherEntry `app_uri` must all key
/// off the same `.desktop` basename so the dock can correlate the
/// notification with our launcher icon. If they drift, GNOME Shell
/// closes the toast on a different app id than the badge tracker
/// watches and the dock dot survives every "mark read" sweep.
const _: () = {
    let lhs = APP_ID.as_bytes();
    let rhs = flowmux_notify::DESKTOP_FILE_BASENAME.as_bytes();
    assert!(
        lhs.len() == rhs.len(),
        "APP_ID and flowmux_notify::DESKTOP_FILE_BASENAME must match the installed .desktop basename"
    );
    let mut i = 0;
    while i < lhs.len() {
        assert!(
            lhs[i] == rhs[i],
            "APP_ID and flowmux_notify::DESKTOP_FILE_BASENAME must match the installed .desktop basename"
        );
        i += 1;
    }
};

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

    // GSK ships the new "ngl" renderer as its default on GTK 4.14+. On
    // older Mesa stacks (notably the 22.04 host that Flatpak builds
    // run against) ngl can fall back to software composition or spend
    // significant time uploading textures per frame, which shows up as
    // visible lag in TUIs that redraw a lot (tig, opencode, htop,
    // vim with `mouse=a`). The classic "gl" renderer has been around
    // since GTK 4.0 and runs smoothly on the same hosts. Pick it as a
    // default unless the user has already chosen a renderer.
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "gl");
    }

    // One socket per GUI process so several flowmux windows can run
    // side-by-side without their notifications crossing over. The
    // `FLOWMUX_SOCKET_PATH` env we inject into every PTY carries this
    // exact path, so terminal-side hooks (Claude/Codex/OpenCode) talk
    // back to the GUI that spawned them.
    let socket = paths::runtime_socket_for_pid(std::process::id());
    info!(?socket, "flowmux starting");
    flowmux_config::notify_debug!(
        "daemon/startup",
        "binding socket={socket:?} flatpak={} HOME={:?} XDG_RUNTIME_DIR={:?}",
        flowmux_config::paths::is_flatpak_sandbox(),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_RUNTIME_DIR")
    );
    // Make sure the parent directory exists. Inside Flatpak
    // `runtime_socket_for_pid` returns `$HOME/.cache/flowmux/…` which
    // is not auto-created by the runtime, so the IPC server's bind
    // would fail with ENOENT and the GUI would silently lose hooks.
    if let Some(parent) = socket.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, path = %parent.display(), "could not create socket parent dir");
            flowmux_config::notify_debug!(
                "daemon/startup",
                "could not create socket parent dir {parent:?}: {e}"
            );
        }
    }
    // Refresh the "current" socket pointer so a host-side process
    // (e.g. OpenCode plugin spawned with `flatpak run --command=
    // flowmuxctl …`) can find this daemon without knowing the PID.
    // Outside Flatpak `runtime_socket()` returns the legacy
    // `flowmux.sock` path and points at this same daemon for the
    // same reason — multi-window users keep using `FLOWMUX_SOCKET_PATH`
    // to scope to the right window.
    refresh_runtime_socket_pointer(&socket);

    // Tokio runtime hosts the IPC server, the state store, and any
    // async desktop-bus interactions.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Workspace ownership is per-window, not host-wide. The first GUI
    // on this host wins the `state.json` lock, restores the previous
    // session, and persists future mutations. Every additional window
    // started while the first one is alive starts from an empty
    // workspace list and runs purely in memory — so the two windows
    // never share, mutate, or overwrite each other's workspaces.
    let state_lock = match flowmux_state::try_acquire_state_lock() {
        Ok(lock) => lock,
        Err(e) => {
            tracing::warn!(error = %e, "state lock unavailable, running ephemeral");
            None
        }
    };
    let store = if state_lock.is_some() {
        let initial = match flowmux_state::load() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not load state, starting empty");
                flowmux_state::State::default()
            }
        };
        info!("state.json owner: this window restores the previous session");
        StateStore::new_lazy(initial)
    } else {
        info!(
            "another flowmux window owns state.json; this window starts with an empty workspace list and will not persist"
        );
        StateStore::new_lazy_ephemeral(flowmux_state::State::default())
    };
    store.spawn_persist(rt.handle());

    let (bridge, rx) = Bridge::new();
    let daemon_handler = DaemonHandler::new(store.clone());
    // Hand the GTK controller the same notifier cell the daemon
    // handler uses to send `AddNotification`. gnome-shell's
    // `org.gtk.Notifications` keys entries by `(sender, app_id)`, so a
    // withdraw issued through a second `Connection::session()` never
    // matches and the dock badge stays pinned after the user acks.
    let shared_notifier = daemon_handler.notifier_handle();
    let handler = Arc::new(GuiHandler::new(daemon_handler, bridge.clone()));

    let socket_clone = socket.clone();
    rt.spawn(async move {
        if let Err(e) = flowmux_ipc::server::run(&socket_clone, handler).await {
            tracing::error!(error = %e, "ipc server exited");
        }
    });

    // GTK runs on the main thread.
    let app = build_application();
    let store_for_activate = store.clone();
    let rx_for_activate = rx.clone();
    let bridge_for_activate = bridge.clone();
    // The GTK main thread is not a Tokio worker, but every D-Bus path
    // dispatched from there (FDO toast close, dock launcher badge
    // republish) goes through `zbus` with the `tokio` feature, which
    // needs an active runtime context for every `await`. Hand the
    // controller a clone of the runtime handle so it can `enter()` it
    // from inside `glib::spawn_local`. Without this, every D-Bus
    // `await` panics with "no reactor running", the panic is swallowed
    // by GLib's task wrapper, and the dock badge / notification close
    // path silently never runs.
    let tokio_handle_for_activate = rt.handle().clone();
    let shared_notifier_for_activate = shared_notifier;
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
        // Apply user keybinding overrides. install_accels validates each
        // accelerator through gtk::accelerator_parse, so it must run after
        // GTK has been initialised by the activate callback — calling it
        // before app.run() panics with "GTK has not been initialized".
        keybindings::install_accels(app, &initial_options);
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

        let mut controller = WindowController::new(
            app,
            store_for_activate.clone(),
            theme,
            bridge_for_activate.clone(),
            provider,
            Some(tokio_handle_for_activate.clone()),
        );
        controller.use_shared_notifier(shared_notifier_for_activate.clone());
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
    // Hold the per-host state lock until the very end so the next
    // flowmux launch only sees a free lock once we have actually
    // stopped writing.
    drop(state_lock);
    std::process::exit(exit_code.into());
}

/// Build the libadwaita application with the flags every other piece of the
/// multi-window story already relies on.
///
/// `NON_UNIQUE` is load-bearing: per-PID IPC socket (`runtime_socket_for_pid`),
/// per-host `state.lock` ownership, and the per-process `Bridge` channel all
/// assume each flowmux window is its own OS process. Without this flag,
/// GApplication's default singleton behavior makes a second `flowmux` launch
/// hand off to the existing process via D-Bus and exit, so the second
/// `connect_activate` opens a window inside the same process — sharing the
/// same `Bridge`. `async_channel` is MPMC, so a workspace click in window A
/// then races with window B's dispatcher and sometimes flips the wrong
/// window's active workspace.
fn build_application() -> adw::Application {
    adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build()
}

/// Refresh the stable "current daemon" pointer used by env-less CLI
/// invocations and (on Flatpak) by the host-side OpenCode plugin.
///
/// Uses a symlink so a process opening the path follows it to the
/// real per-PID socket atomically. Falling back to a regular text
/// file with the path inside would force every consumer to do a
/// two-step read-then-connect dance.
///
/// Last-writer wins: when two flowmux GUIs are alive at once the
/// pointer points at whichever started most recently. Per-PID
/// sockets keep working for terminals inside the earlier window
/// because the PTY env carries the explicit `FLOWMUX_SOCKET_PATH`.
/// The pointer is only consulted by callers that have no env (host
/// plugin process, manual `flowmuxctl notify` from outside any pane).
fn refresh_runtime_socket_pointer(target: &std::path::Path) {
    let pointer = paths::runtime_socket();
    if pointer == target {
        // Outside Flatpak with no XDG runtime dir this can collapse
        // onto the same path; nothing to do.
        return;
    }
    if let Some(parent) = pointer.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, path = %parent.display(), "could not create socket pointer parent");
            return;
        }
    }
    // Replace any prior pointer atomically. `remove_file` is fine for
    // both real files and symlinks; ignore NotFound.
    if let Err(e) = std::fs::remove_file(&pointer) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, path = %pointer.display(), "could not remove stale socket pointer");
        }
    }
    if let Err(e) = std::os::unix::fs::symlink(target, &pointer) {
        tracing::warn!(error = %e, from = %pointer.display(), to = %target.display(), "could not write socket pointer symlink");
    } else {
        tracing::info!(pointer = %pointer.display(), target = %target.display(), "current-daemon socket pointer refreshed");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gio::ApplicationFlags;

    /// Regression guard for cross-window workspace navigation: the bug was
    /// that two `flowmux` launches collapsed into one process under
    /// GApplication's default singleton behavior, then both windows
    /// shared a single MPMC `Bridge` and a click in window A activated
    /// window B's workspace. The fix is `NON_UNIQUE` on the application
    /// builder; if a future refactor drops it, this test fails before
    /// the user does.
    #[gtk::test]
    fn application_uses_non_unique_so_each_window_runs_in_its_own_process() {
        // libadwaita refuses to initialize without a display server.
        // Skip silently on headless CI; the assertion below is a pure
        // check against the configured GApplication flags and does not
        // need a live GTK loop to be meaningful.
        if adw::init().is_err() {
            return;
        }
        let app = build_application();
        assert!(
            app.flags().contains(ApplicationFlags::NON_UNIQUE),
            "build_application() must set NON_UNIQUE so two flowmux launches \
             produce two processes (per-PID socket, state.lock, Bridge all \
             assume this); got flags = {:?}",
            app.flags()
        );
    }
}
