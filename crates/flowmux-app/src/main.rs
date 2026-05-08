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
use std::sync::Arc;
use tracing::info;
use ui::{spawn_dispatch_loop, WindowController};

const APP_ID: &str = "com.flowmux.App";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOWMUX_LOG")
                .unwrap_or_else(|_| "info,flowmux=debug".into()),
        )
        .init();

    // WebKitGTK 6.0의 기본 sandbox는 bwrap + xdg-dbus-proxy를 띄우는데
    // Ubuntu 24.04의 unprivileged user-namespace AppArmor 제한 환경에서는
    // bwrap이 'setting up uid map: Permission denied'로 실패해 dbus-proxy
    // 가 종료 코드 1로 죽고, 곧이어 첫 탭브라우저 생성 시 앱이 강제 종료된다.
    // 사용자가 별도 AppArmor 프로파일을 설치하지 않아도 동작하도록, 첫
    // WebView가 만들어지기 전에 sandbox 비활성화 플래그를 세팅한다.
    // 이미 사용자가 명시적으로 값을 지정한 경우(0/1 무관)는 존중한다.
    if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
        std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
    }

    // WebKitGTK 6.0의 새 DMA-BUF renderer는 Mesa Wayland 환경에서 종료
    // 시 `eglDestroySync` 부재 + 후속 `corrupted size vs. prev_size`
    // glibc abort를 일으키는 race가 알려져 있다 (libepoxy가 EGL 1.5
    // 또는 EGL_KHR_fence_sync를 못 찾을 때 NULL deref). 사용자가 별도로
    // 값을 지정하지 않은 경우 DMA-BUF renderer를 끄는 게 가장 안전한
    // 기본값이다 — 그래픽 품질 손실 없이 종료 cleanup만 단순해진다.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    let socket = paths::runtime_socket();
    info!(?socket, "flowmux-app starting");

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

        // Install the global CSS for pane frames + sidebar tint. 옵션의
        // focus_border_color를 미리 읽어 포커스 테두리 색을 채워 둔다 —
        // 옵션 다이얼로그에서 변경되면 WindowController가 같은 provider
        // 인스턴스의 CSS를 다시 로드해 즉시 반영한다.
        let initial_options = flowmux_config::options::load();
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&theme.css(initial_options.focus_border_color_or_default()));
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
