// SPDX-License-Identifier: GPL-3.0-or-later
//! VTE 2.91 / GTK4 backend skeleton.
//!
//! The actual widgets live on the GTK main thread inside `flowmux-app`;
//! this module provides the type-level glue (so the trait surface is
//! stable) and the wiring it would need. We intentionally don't run any
//! VTE calls here — they require an active `gtk::init()`.

use crate::{SpawnSpec, TerminalBackend, TerminalError};
use flowmux_core::PaneId;
use std::collections::HashMap;

pub struct VteBackend {
    panes: HashMap<PaneId, PaneSlot>,
}

struct PaneSlot {
    /// `vte::Terminal` lives here once wired from the GTK side.
    /// Held as a weak ref through gtk's reference counting; we treat the
    /// backend as a registry, not as the widget owner.
    _placeholder: (),
}

impl VteBackend {
    pub fn new() -> Self {
        Self {
            panes: HashMap::new(),
        }
    }

    pub fn register(&mut self, id: PaneId) {
        self.panes.insert(id, PaneSlot { _placeholder: () });
    }
}

impl Default for VteBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalBackend for VteBackend {
    fn spawn(&mut self, _spec: SpawnSpec<'_>) -> Result<PaneId, TerminalError> {
        // Real impl will call `vte::Terminal::spawn_async` on the GTK
        // main loop and return the pane id once the child is reaped or
        // an error surfaces. The `flowmux-app` crate owns the GTK runtime
        // and provides a thin shim that calls into here.
        Err(TerminalError::Spawn(
            "vte backend not yet wired into GTK runtime".into(),
        ))
    }

    fn send(&mut self, pane: PaneId, _bytes: &[u8]) -> Result<(), TerminalError> {
        if !self.panes.contains_key(&pane) {
            return Err(TerminalError::NotFound(pane));
        }
        Ok(())
    }

    fn resize(&mut self, pane: PaneId, _rows: u16, _cols: u16) -> Result<(), TerminalError> {
        if !self.panes.contains_key(&pane) {
            return Err(TerminalError::NotFound(pane));
        }
        Ok(())
    }

    fn close(&mut self, pane: PaneId) -> Result<(), TerminalError> {
        self.panes
            .remove(&pane)
            .ok_or(TerminalError::NotFound(pane))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_panes_accept_send_resize_and_close() {
        let pane = PaneId::new();
        let mut backend = VteBackend::new();
        backend.register(pane);

        assert!(backend.send(pane, b"hello").is_ok());
        assert!(backend.resize(pane, 24, 80).is_ok());
        assert!(backend.close(pane).is_ok());
        assert!(matches!(backend.send(pane, b"x"), Err(TerminalError::NotFound(id)) if id == pane));
    }

    #[test]
    fn spawn_reports_runtime_not_wired() {
        let mut backend = VteBackend::new();
        let spec = SpawnSpec {
            argv: &["sh"],
            cwd: None,
            env: &[],
        };

        assert!(
            matches!(backend.spawn(spec), Err(TerminalError::Spawn(message)) if message.contains("not yet wired"))
        );
    }
}
