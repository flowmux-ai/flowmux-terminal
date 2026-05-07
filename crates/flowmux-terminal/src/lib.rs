// SPDX-License-Identifier: GPL-3.0-or-later
//! Terminal backend abstraction.
//!
//! flowmux renders panes through a [`TerminalBackend`] so we can swap
//! implementations without touching the application or IPC layers:
//!
//! * `vte` (default) — the VTE 2.91 GTK4 widget used by GNOME Terminal,
//!   Tilix, and Black Box. Mature, OSC sequences mostly handled.
//! * `ghostty` (planned) — libghostty embedded into a GTK widget. Same
//!   renderer cmux uses on macOS, for output parity.
//!
//! See `docs/upstream-mapping/terminal.md` for the parity matrix.

use flowmux_core::PaneId;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("pane not found: {0}")]
    NotFound(PaneId),
    #[cfg(feature = "vte")]
    #[error("glib: {0}")]
    Glib(String),
}

#[derive(Debug, Clone)]
pub struct SpawnSpec<'a> {
    pub argv: &'a [&'a str],
    pub cwd: Option<&'a Path>,
    pub env: &'a [(&'a str, &'a str)],
}

pub trait TerminalBackend {
    /// Spawn a process in a fresh pane and return its id.
    fn spawn(&mut self, spec: SpawnSpec<'_>) -> Result<PaneId, TerminalError>;
    /// Send keystrokes to a pane (raw bytes; caller handles escape).
    fn send(&mut self, pane: PaneId, bytes: &[u8]) -> Result<(), TerminalError>;
    /// Resize to (rows, cols).
    fn resize(&mut self, pane: PaneId, rows: u16, cols: u16) -> Result<(), TerminalError>;
    /// Close pane and reap child.
    fn close(&mut self, pane: PaneId) -> Result<(), TerminalError>;
}

#[cfg(feature = "vte")]
pub mod vte_backend;

#[cfg(feature = "ghostty")]
pub mod ghostty_backend;
