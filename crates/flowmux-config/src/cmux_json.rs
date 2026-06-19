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
    let cfg = parse_str(&text)?;
    Ok(Some(cfg))
}

pub fn parse_str(text: &str) -> Result<CmuxJson, LoadError> {
    Ok(serde_json::from_str(&strip_jsonc_comments(text))?)
}

fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                        }
                        if prev == '*' && next == '/' {
                            break;
                        }
                        prev = next;
                    }
                }
                _ => out.push(ch),
            }
            continue;
        }

        out.push(ch);
    }

    out
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
        let cfg = parse_str(raw).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("demo"));
        assert_eq!(cfg.commands.len(), 1);
        assert_eq!(cfg.commands[0].target, CommandTarget::FocusedPane);
    }

    #[test]
    fn parses_jsonc_comments_without_touching_strings() {
        let raw = r#"
        {
          // project commands
          "commands": [
            {
              "id": "echo",
              "label": "Echo // literal",
              "run": ["printf", "/* not a comment */"],
              "target": "new_surface"
            }
          ],
          /* shared environment */
          "env": { "A": "B" }
        }
        "#;

        let cfg = parse_str(raw).unwrap();
        assert_eq!(cfg.commands.len(), 1);
        assert_eq!(cfg.commands[0].label, "Echo // literal");
        assert_eq!(cfg.commands[0].run[1], "/* not a comment */");
        assert_eq!(cfg.commands[0].target, CommandTarget::NewSurface);
        assert_eq!(cfg.env.get("A").map(String::as_str), Some("B"));
    }
}
