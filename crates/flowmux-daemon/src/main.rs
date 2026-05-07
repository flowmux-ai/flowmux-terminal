// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux-daemon` — IPC-only entrypoint.
//!
//! Boots the same handler that the GTK app uses, but without any
//! windowing. Useful in container/CI contexts and as a smoke test that
//! the IPC layer works end-to-end.

use clap::Parser;
use flowmux_config::paths;
use flowmux_daemon::{DaemonHandler, StateStore};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

#[derive(Parser)]
#[command(name = "flowmux-daemon", version)]
struct Cli {
    /// Override the unix socket path.
    #[arg(long, env = "FLOWMUX_SOCKET")]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOWMUX_LOG")
                .unwrap_or_else(|_| "info,flowmux=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(paths::runtime_socket);
    info!(?socket, "flowmux-daemon starting");

    let initial = match flowmux_state::load() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load state, starting empty");
            flowmux_state::State::default()
        }
    };
    let store = StateStore::new(initial);
    let handler = Arc::new(DaemonHandler::new(store));

    flowmux_ipc::server::run(&socket, handler).await
}
