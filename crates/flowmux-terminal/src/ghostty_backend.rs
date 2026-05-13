// SPDX-License-Identifier: GPL-3.0-or-later
//! Libghostty-oriented terminal backend.
//!
//! flowmux's GUI owns the concrete GTK renderer while libghostty owns the
//! VT state. This backend models the terminal process contract shared with
//! the GUI path: spawn argv/cwd/env, raw input bytes, dimensions, and close
//! lifecycle.

use crate::{SpawnSpec, TerminalBackend, TerminalError};
use flowmux_core::PaneId;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct GhosttyBackend {
    panes: HashMap<PaneId, GhosttyPaneState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhosttyPaneState {
    argv: Vec<String>,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    rows: u16,
    cols: u16,
    pending_input: Vec<u8>,
}

impl GhosttyBackend {
    pub fn new() -> Self {
        Self {
            panes: HashMap::new(),
        }
    }

    pub fn register(&mut self, id: PaneId) {
        self.panes.insert(id, GhosttyPaneState::placeholder());
    }

    pub fn pane(&self, id: PaneId) -> Option<&GhosttyPaneState> {
        self.panes.get(&id)
    }

    pub fn take_pending_input(&mut self, id: PaneId) -> Result<Vec<u8>, TerminalError> {
        let pane = self.panes.get_mut(&id).ok_or(TerminalError::NotFound(id))?;
        Ok(std::mem::take(&mut pane.pending_input))
    }
}

impl GhosttyPaneState {
    fn from_spawn(spec: SpawnSpec<'_>) -> Self {
        Self {
            argv: spec.argv.iter().map(|s| (*s).to_string()).collect(),
            cwd: spec.cwd.map(PathBuf::from),
            env: spec
                .env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            rows: 24,
            cols: 80,
            pending_input: Vec::new(),
        }
    }

    fn placeholder() -> Self {
        Self {
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
            pending_input: Vec::new(),
        }
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }
}

impl Default for GhosttyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalBackend for GhosttyBackend {
    fn spawn(&mut self, spec: SpawnSpec<'_>) -> Result<PaneId, TerminalError> {
        let id = PaneId::new();
        self.panes.insert(id, GhosttyPaneState::from_spawn(spec));
        Ok(id)
    }

    fn send(&mut self, pane: PaneId, bytes: &[u8]) -> Result<(), TerminalError> {
        let state = self
            .panes
            .get_mut(&pane)
            .ok_or(TerminalError::NotFound(pane))?;
        state.pending_input.extend_from_slice(bytes);
        Ok(())
    }

    fn resize(&mut self, pane: PaneId, rows: u16, cols: u16) -> Result<(), TerminalError> {
        let state = self
            .panes
            .get_mut(&pane)
            .ok_or(TerminalError::NotFound(pane))?;
        state.rows = rows;
        state.cols = cols;
        Ok(())
    }

    fn close(&mut self, pane: PaneId) -> Result<(), TerminalError> {
        self.panes
            .remove(&pane)
            .ok_or(TerminalError::NotFound(pane))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_records_process_contract_and_default_size() {
        let mut backend = GhosttyBackend::new();
        let spec = SpawnSpec {
            argv: &["sh", "-l"],
            cwd: Some(std::path::Path::new("/tmp")),
            env: &[("FLOWMUX_PANE_ID", "pane")],
        };

        let pane = backend
            .spawn(spec)
            .expect("ghostty spawn should register pane");
        let state = backend.pane(pane).expect("spawned pane should exist");

        assert_eq!(state.argv(), &["sh".to_string(), "-l".to_string()]);
        assert_eq!(state.cwd(), Some(&PathBuf::from("/tmp")));
        assert_eq!(state.env(), &[("FLOWMUX_PANE_ID".into(), "pane".into())]);
        assert_eq!(state.size(), (24, 80));
    }

    #[test]
    fn send_buffers_raw_pty_input_until_taken() {
        let mut backend = GhosttyBackend::new();
        let pane = backend
            .spawn(SpawnSpec {
                argv: &["sh"],
                cwd: None,
                env: &[],
            })
            .unwrap();

        backend.send(pane, b"hello").unwrap();
        backend.send(pane, b"\r").unwrap();

        assert_eq!(backend.take_pending_input(pane).unwrap(), b"hello\r");
        assert!(backend.take_pending_input(pane).unwrap().is_empty());
    }

    #[test]
    fn resize_and_close_track_pane_lifecycle() {
        let mut backend = GhosttyBackend::new();
        let pane = backend
            .spawn(SpawnSpec {
                argv: &["sh"],
                cwd: None,
                env: &[],
            })
            .unwrap();

        backend.resize(pane, 40, 120).unwrap();
        assert_eq!(backend.pane(pane).unwrap().size(), (40, 120));

        backend.close(pane).unwrap();
        assert!(matches!(
            backend.resize(pane, 40, 120),
            Err(TerminalError::NotFound(id)) if id == pane
        ));
    }
}
