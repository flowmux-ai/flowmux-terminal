// SPDX-License-Identifier: GPL-3.0-or-later
//! Planned libghostty backend.
//!
//! The feature exists so downstream packaging and parity work can
//! compile-gate the future backend explicitly. Until libghostty
//! bindings are wired, this backend reports a clear spawn error and
//! otherwise behaves as an empty pane registry.

use crate::{SpawnSpec, TerminalBackend, TerminalError};
use flowmux_core::PaneId;
use std::collections::HashSet;

pub struct GhosttyBackend {
    panes: HashSet<PaneId>,
}

impl GhosttyBackend {
    pub fn new() -> Self {
        Self {
            panes: HashSet::new(),
        }
    }

    pub fn register(&mut self, id: PaneId) {
        self.panes.insert(id);
    }
}

impl Default for GhosttyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalBackend for GhosttyBackend {
    fn spawn(&mut self, _spec: SpawnSpec<'_>) -> Result<PaneId, TerminalError> {
        Err(TerminalError::Spawn(
            "ghostty backend not yet wired into GTK runtime".into(),
        ))
    }

    fn send(&mut self, pane: PaneId, _bytes: &[u8]) -> Result<(), TerminalError> {
        if !self.panes.contains(&pane) {
            return Err(TerminalError::NotFound(pane));
        }
        Ok(())
    }

    fn resize(&mut self, pane: PaneId, _rows: u16, _cols: u16) -> Result<(), TerminalError> {
        if !self.panes.contains(&pane) {
            return Err(TerminalError::NotFound(pane));
        }
        Ok(())
    }

    fn close(&mut self, pane: PaneId) -> Result<(), TerminalError> {
        self.panes
            .remove(&pane)
            .then_some(())
            .ok_or(TerminalError::NotFound(pane))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_panes_accept_registry_operations() {
        let pane = PaneId::new();
        let mut backend = GhosttyBackend::new();
        backend.register(pane);

        assert!(backend.send(pane, b"hello").is_ok());
        assert!(backend.resize(pane, 24, 80).is_ok());
        assert!(backend.close(pane).is_ok());
        assert!(matches!(
            backend.close(pane),
            Err(TerminalError::NotFound(id)) if id == pane
        ));
    }

    #[test]
    fn spawn_reports_runtime_not_wired() {
        let mut backend = GhosttyBackend::new();
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
