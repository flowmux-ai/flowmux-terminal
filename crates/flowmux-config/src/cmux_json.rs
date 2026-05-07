// SPDX-License-Identifier: GPL-3.0-or-later
//! `cmux.json` parser. Schema follows the public documentation at
//! cmux.com/docs/custom-commands.
//!
//! Only the fields flowmux actually uses are deserialized — additional
//! upstream keys are silently ignored so user files written for cmux
//! still load. Fields specific to macOS-only behavior (e.g. dock badge
//! tweaks) are accepted but no-op.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CmuxJson {
    /// Optional project name shown in the sidebar.
    #[serde(default)]
    pub name: Option<String>,

    /// Project-defined commands launchable from the command palette.
    #[serde(default)]
    pub commands: Vec<CustomCommand>,

    /// Project-level environment overlay applied to spawned terminals.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomCommand {
    pub id: String,
    pub label: String,
    /// Argv to spawn. The first element is the program; remaining are args.
    pub run: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Where the command's output should appear.
    #[serde(default)]
    pub target: CommandTarget,
    /// If true, prompt before running.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandTarget {
    /// Run in the currently focused pane.
    #[default]
    FocusedPane,
    /// Open a new horizontal split below the focused pane.
    SplitDown,
    /// Open a new vertical split to the right.
    SplitRight,
    /// Open in a new surface (tab) within the current workspace.
    NewSurface,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn load_from_dir(dir: &Path) -> Result<Option<CmuxJson>, LoadError> {
    let path = dir.join("cmux.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let cfg: CmuxJson = serde_json::from_str(&text)?;
    Ok(Some(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let raw = r#"{
            "name": "demo",
            "commands": [
                { "id": "test", "label": "Run tests", "run": ["pnpm", "test"] }
            ]
        }"#;
        let cfg: CmuxJson = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("demo"));
        assert_eq!(cfg.commands.len(), 1);
        assert_eq!(cfg.commands[0].target, CommandTarget::FocusedPane);
    }
}
