// SPDX-License-Identifier: GPL-3.0-or-later
//! flowmux GUI entrypoint.
//!
//! Boots libadwaita, brings up the IPC server and the state store on a
//! tokio runtime, and wires GTK-affecting verbs to the GTK main loop
//! through an [`async_channel`] bridge.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

mod bridge;
mod builtin_icons;
mod ipc_handler;
mod keybindings;
mod notifications;
mod platform;
mod theme;
mod ui;
mod update;
mod usage;

use adw::prelude::*;
use bridge::Bridge;
use flowmux_config::paths;
use flowmux_daemon::{DaemonHandler, StateStore};
use ipc_handler::GuiHandler;
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
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
    flowmux_config::diagnostics::install_panic_hook();
    let default_filter = if cfg!(debug_assertions) {
        "info,flowmux=debug"
    } else {
        "error"
    };
    let log_guard = flowmux_config::diagnostics::init_logging("flowmux.log", default_filter)?;
    if std::env::var("FLOWMUX_DEBUG_PANIC").as_deref() == Ok("1") {
        panic!("FLOWMUX_DEBUG_PANIC requested");
    }

    if delegate_to_cli_if_needed()? {
        return Ok(());
    }

    let previous_crash = match flowmux_config::diagnostics::take_unreported_crash() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "could not inspect previous crash reports");
            None
        }
    };

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

    // CJK preedit — the "composing" text shown while a Hangul / Kana /
    // Pinyin syllable is still being assembled — is drawn by VTE's own
    // internal GTK IMContext. It only appears when GTK4 has loaded an
    // immodule that delivers `preedit-changed` events inline; in
    // practice that is the `ibus` immodule, which talks to ibus-daemon
    // directly. Several common setups never load it and so render
    // *nothing* until a syllable commits — exactly the "composing state
    // is invisible until the syllable is finished" symptom:
    //   * WSL2 + WSLg ships a Weston compositor that does not bridge the
    //     Wayland text-input protocol to a session IME, so the wayland
    //     immodule never receives the preedit channel.
    //   * The Flatpak sandbox has no route to the host IBus unless it is
    //     granted (see `--talk-name=org.freedesktop.portal.IBus` in
    //     `packaging/flatpak/com.flowmux.App.yml`); GTK4 otherwise falls
    //     back to the `simple` immodule.
    //   * Ubuntu 22.04's mutter 42 + IBus 1.5.26 bridges commits but not
    //     inline preedit over text-input-v3 on Wayland, and on X11 GTK4
    //     may pick the `simple` / `xim` immodule.
    //
    // Forcing `GTK_IM_MODULE=ibus` makes GTK4 load the ibus immodule,
    // which connects to ibus-daemon over its own socket / the IBus
    // portal and reports preedit inline on every one of those setups.
    // The override is gated so setups with another real IME such as
    // fcitx keep working, while known non-preedit modules (`wayland`,
    // `simple`, `xim`) are corrected when ibus is reachable. Inside
    // Flatpak we trust the manifest's portal grant; otherwise we require
    // an installed ibus-daemon binary.
    let ibus_reachable = flowmux_config::paths::is_flatpak_sandbox() || ibus_daemon_available();
    let gtk_im_module = std::env::var("GTK_IM_MODULE").ok();
    if should_force_ibus_im_module(gtk_im_module.as_deref(), ibus_reachable) {
        std::env::set_var("GTK_IM_MODULE", "ibus");
    }
    if gtk_im_module_is_ibus(std::env::var("GTK_IM_MODULE").ok().as_deref()) {
        if std::env::var_os("XMODIFIERS").is_none() {
            std::env::set_var("XMODIFIERS", "@im=ibus");
        }
        // The ibus immodule processes key events asynchronously by
        // default: a keypress is forwarded to ibus-daemon over D-Bus and
        // the resulting commit-text comes back on a later turn. When a
        // Hangul syllable is still in preedit and the user hits Enter,
        // VTE handles the (unfiltered) Enter and feeds `\r` to the PTY
        // *before* the async commit of the composed syllable arrives, so
        // the line breaks in front of the last character and that
        // character lands on the next line ("\n한" instead of "한\n").
        // `IBUS_ENABLE_SYNC_MODE=1` makes the immodule block for the
        // daemon's reply inside the key filter, so the commit is fed
        // ahead of the Enter and the ordering is correct. WSLg's IBus
        // portal path rejects the sync-mode PostProcessKeyEvent call on
        // Ubuntu 24.04, though, so leave it async there and rely on the
        // terminal-pane ordering/bypass workarounds below.
        if std::env::var_os("IBUS_ENABLE_SYNC_MODE").is_none() && !platform::running_under_wsl() {
            std::env::set_var("IBUS_ENABLE_SYNC_MODE", "1");
        }
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
    cleanup_stale_runtime_sockets(&socket);
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

    // Each process claims only workspaces left by exited owners. Every save
    // briefly locks state.json, reloads it, replaces this window's slice, and
    // preserves slices owned by other live windows.
    let owner = flowmux_state::WindowOwner::current();
    let store = match flowmux_state::claim_window(owner) {
        Ok(initial) => {
            info!(
                workspaces = initial.workspaces.len(),
                "claimed persisted workspaces for this window"
            );
            StateStore::new_lazy_window(initial, owner)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not claim persisted state, running ephemeral");
            StateStore::new_lazy_ephemeral(flowmux_state::State::default())
        }
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

    // Agent liveness sweep. SessionEnd clears a presence on a clean
    // exit, but Ctrl+C / hard kill / closed terminal may never fire it;
    // keep the check short so the Agent Bar drops dead sessions promptly.
    // Cheap `/proc` stat on Linux and kill(0) elsewhere.
    {
        let sweep_store = store.clone();
        let sweep_tx = bridge.tx.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(tokio::time::Duration::from_secs(2));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                for (workspace, surface, pid) in sweep_store.live_agent_presences().await {
                    if !flowmux_procmon::pid_alive(pid) {
                        sweep_store.clear_dead_agent_activity(surface).await;
                        let status = sweep_store.workspace_agent_status(workspace).await;
                        let _ = sweep_tx
                            .send(crate::bridge::GtkCommand::SetAgentStatus { workspace, status })
                            .await;
                    }
                }
            }
        });
    }

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
    let previous_crash_for_activate = previous_crash;
    let active_window_for_activate: Rc<
        RefCell<Option<gtk::glib::WeakRef<adw::ApplicationWindow>>>,
    > = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        if let Some(window) = active_window_for_activate
            .borrow()
            .as_ref()
            .and_then(|window| window.upgrade())
        {
            // macOS can emit another application activation while presenting
            // a secondary window. Do not raise the main window over an image
            // viewer (or another FlowMux window) that is already active.
            let active_window_is_active = app
                .active_window()
                .as_ref()
                .is_some_and(|window| window.is_active());
            if should_present_existing_main_window(active_window_is_active) {
                window.present();
            }
            return;
        }

        builtin_icons::install();
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
            controller.usage_button(),
        );
        spawn_dispatch_loop(rx_for_activate.clone(), controller.clone());
        let controller_for_init = controller.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            controller_for_init.restore_from_store().await;
            controller_for_init.show_status_when_empty();
        });
        *active_window_for_activate.borrow_mut() = Some(controller.window.downgrade());
        controller.window.present();
        if let Some(path) = previous_crash_for_activate.as_ref() {
            controller.clipboard_toast().show_with_message_for(
                &format!(
                    "Previous session ended unexpectedly — log: {}",
                    path.display()
                ),
                std::time::Duration::from_secs(8),
            );
        }
    });

    let exit_code = app.run();
    drop(rt);
    let _ = std::fs::remove_file(&socket);
    drop(log_guard);
    std::process::exit(exit_code.into());
}

/// Build the libadwaita application with the flags every other piece of the
/// multi-window story already relies on.
///
/// `NON_UNIQUE` is load-bearing: per-PID IPC socket (`runtime_socket_for_pid`),
/// per-window state ownership, and the per-process `Bridge` channel all assume
/// each flowmux window is its own OS process. Without this flag,
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

fn should_present_existing_main_window(active_window_is_active: bool) -> bool {
    !active_window_is_active
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

fn cleanup_stale_runtime_sockets(current: &std::path::Path) {
    let Some(dir) = current.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(pid) = pid_from_runtime_socket_name(name) else {
            continue;
        };
        if process_exists(pid) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, path = %path.display(), "could not remove stale runtime socket");
            }
        } else {
            tracing::debug!(path = %path.display(), pid, "removed stale runtime socket");
        }
    }
}

fn pid_from_runtime_socket_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".sock")?;
    let pid = stem
        .strip_prefix("flowmux-")
        .or_else(|| stem.rsplit_once("-flowmux-").map(|(_, pid)| pid))?;
    pid.parse().ok()
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
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

/// True when an `ibus-daemon` binary is reachable on the standard
/// system paths. We do not probe `$PATH` because the GUI launcher's
/// environment may differ from a login shell's — the file check is
/// authoritative and matches what the GTK4 ibus immodule itself looks
/// for. If the daemon is not installed, forcing `GTK_IM_MODULE=ibus`
/// would only swap one broken state for another, so the caller skips
/// the override.
fn ibus_daemon_available() -> bool {
    const CANDIDATES: &[&str] = &[
        "/usr/bin/ibus-daemon",
        "/usr/local/bin/ibus-daemon",
        "/bin/ibus-daemon",
    ];
    CANDIDATES.iter().any(|p| std::path::Path::new(p).exists())
}

fn should_force_ibus_im_module(current: Option<&str>, ibus_reachable: bool) -> bool {
    if !ibus_reachable {
        return false;
    }
    match current.map(str::trim).filter(|s| !s.is_empty()) {
        None => true,
        Some("wayland" | "simple" | "xim") => true,
        Some(_) => false,
    }
}

fn gtk_im_module_is_ibus(current: Option<&str>) -> bool {
    matches!(current.map(str::trim), Some("ibus"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_os = "macos"))]
    use gtk::gio::ApplicationFlags;

    /// Regression guard for cross-window workspace navigation: the bug was
    /// that two `flowmux` launches collapsed into one process under
    /// GApplication's default singleton behavior, then both windows
    /// shared a single MPMC `Bridge` and a click in window A activated
    /// window B's workspace. The fix is `NON_UNIQUE` on the application
    /// builder; if a future refactor drops it, this test fails before
    /// the user does.
    #[cfg(not(target_os = "macos"))]
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

    #[test]
    fn ibus_im_module_override_targets_only_missing_or_non_preedit_modules() {
        assert!(should_force_ibus_im_module(None, true));
        assert!(should_force_ibus_im_module(Some(""), true));
        assert!(should_force_ibus_im_module(Some("wayland"), true));
        assert!(should_force_ibus_im_module(Some("simple"), true));
        assert!(should_force_ibus_im_module(Some("xim"), true));

        assert!(!should_force_ibus_im_module(Some("ibus"), true));
        assert!(!should_force_ibus_im_module(Some("fcitx"), true));
        assert!(!should_force_ibus_im_module(Some("fcitx5"), true));
        assert!(!should_force_ibus_im_module(None, false));
        assert!(!should_force_ibus_im_module(Some("wayland"), false));
    }

    #[test]
    fn activation_does_not_raise_main_over_an_active_viewer() {
        assert!(!should_present_existing_main_window(true));
        assert!(should_present_existing_main_window(false));
    }

    #[test]
    fn ibus_sync_mode_applies_when_gtk_im_module_is_ibus() {
        assert!(gtk_im_module_is_ibus(Some("ibus")));
        assert!(gtk_im_module_is_ibus(Some(" ibus ")));
        assert!(!gtk_im_module_is_ibus(None));
        assert!(!gtk_im_module_is_ibus(Some("wayland")));
    }

    #[test]
    fn runtime_socket_pid_parser_accepts_linux_and_temp_fallback_names() {
        assert_eq!(
            pid_from_runtime_socket_name("flowmux-1234.sock"),
            Some(1234)
        );
        assert_eq!(
            pid_from_runtime_socket_name("junsu-flowmux-5678.sock"),
            Some(5678)
        );
        assert_eq!(pid_from_runtime_socket_name("notflowmux-1234.sock"), None);
        assert_eq!(pid_from_runtime_socket_name("flowmux.sock"), None);
        assert_eq!(pid_from_runtime_socket_name("flowmux-current.sock"), None);
        assert_eq!(pid_from_runtime_socket_name("flowmux-abc.sock"), None);
    }
}
