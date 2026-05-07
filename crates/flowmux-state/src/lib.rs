// SPDX-License-Identifier: GPL-3.0-or-later
//! Atomic on-disk state for flowmux.
//!
//! Single source of truth lives at `$XDG_STATE_HOME/flowmux/state.json`.
//! Writes go through a tmp-file + rename so a crash mid-write never
//! leaves a half-serialized file.
//!
//! Schema is versioned (`schema_version`) so a future flowmux release can
//! migrate old state files from this format.

use flowmux_config::paths;
use flowmux_core::Workspace;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub schema_version: u32,
    pub workspaces: Vec<Workspace>,
    /// Workspace IDs in the order they appear in the sidebar.
    #[serde(default)]
    pub workspace_order: Vec<flowmux_core::WorkspaceId>,
    /// Most-recently-active workspace, used to focus on launch.
    #[serde(default)]
    pub active_workspace: Option<flowmux_core::WorkspaceId>,
    pub last_saved: chrono::DateTime<chrono::Utc>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            workspaces: vec![],
            workspace_order: vec![],
            active_workspace: None,
            last_saved: chrono::Utc::now(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Json(#[from] serde_json::Error),
    #[error("XDG state dir is unavailable")]
    NoStateDir,
    #[error("schema version {found} is newer than supported ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
}

pub fn default_path() -> Result<PathBuf, StateError> {
    paths::state_dir()
        .ok_or(StateError::NoStateDir)
        .map(|d| d.join("state.json"))
}

pub fn load() -> Result<State, StateError> {
    load_from(&default_path()?)
}

pub fn save(state: &State) -> Result<(), StateError> {
    save_to(&default_path()?, state)
}

pub fn load_from(path: &Path) -> Result<State, StateError> {
    if !path.exists() {
        return Ok(State::default());
    }
    let text = std::fs::read_to_string(path)?;
    let state: State = serde_json::from_str(&text)?;
    if state.schema_version > SCHEMA_VERSION {
        return Err(StateError::SchemaTooNew {
            found: state.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(state)
}

pub fn save_to(path: &Path, state: &State) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut s = state.clone();
    s.last_saved = chrono::Utc::now();
    let json = serde_json::to_vec_pretty(&s)?;

    // Atomic replace: write to <name>.tmp, fsync, then rename.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_core::*;
    use std::path::PathBuf;

    fn sample_workspace() -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            name: "demo".into(),
            root_dir: PathBuf::from("/tmp/demo"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        }
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = load_from(&path).unwrap();
        assert!(state.workspaces.is_empty());
        assert_eq!(state.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::default();
        let ws = sample_workspace();
        let id = ws.id;
        state.workspaces.push(ws);
        state.workspace_order.push(id);
        state.active_workspace = Some(id);
        save_to(&path, &state).unwrap();

        let back = load_from(&path).unwrap();
        assert_eq!(back.workspaces.len(), 1);
        assert_eq!(back.workspaces[0].name, "demo");
        assert_eq!(back.active_workspace, Some(id));
    }

    #[test]
    fn rejects_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version": 9999, "workspaces": [], "last_saved": "2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(matches!(err, StateError::SchemaTooNew { .. }));
    }
}
