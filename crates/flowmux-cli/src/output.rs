// SPDX-License-Identifier: GPL-3.0-or-later
//! Response rendering: print_response and the tree view.
//!
//! Split out of `main.rs` (pure move; behavior unchanged).

use super::*;

pub(crate) fn print_response(r: &Response, json_mode: bool) -> anyhow::Result<()> {
    // `flowmux tree` gets a human-readable indented view in text mode;
    // --json still emits the structured payload for scripts.
    if !json_mode {
        if let Response::Tree { workspaces } = r {
            print!("{}", render_tree(workspaces));
            return Ok(());
        }
        if let Response::WorkspaceCurrent { id } = r {
            match id {
                Some(id) => println!("{id}"),
                None => println!("(none)"),
            }
            return Ok(());
        }
        if let Response::ScreenContents { text } = r {
            // Raw terminal text — print as-is (already newline-terminated
            // per row), no extra framing.
            print!("{text}");
            return Ok(());
        }
        if let Response::Notifications {
            entries,
            unread_count,
        } = r
        {
            println!("{} notification(s), {unread_count} unread", entries.len());
            for entry in entries {
                let state = if entry.read { "read" } else { "unread" };
                let pane = entry
                    .pane
                    .map(|pane| pane.to_string())
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{} [{}] {:?} pane={} {} - {}",
                    entry.id, state, entry.level, pane, entry.title, entry.body
                );
            }
            return Ok(());
        }
        if let Response::NotificationState { changed } = r {
            println!("{}", if *changed { "ok" } else { "not-found" });
            return Ok(());
        }
    }
    let s = if json_mode {
        // Single-line JSON — easier to parse from agent scripts
        // (`jq -r .pane` etc.). Mirrors cmux's `--json` shape.
        serde_json::to_string(r)?
    } else {
        serde_json::to_string_pretty(r)?
    };
    println!("{s}");
    Ok(())
}
/// Render `flowmux tree` as an indented workspace → leaf-pane → tab
/// view. The active tab in each pane is marked with `*`.
pub(crate) fn render_tree(workspaces: &[flowmux_ipc::protocol::TreeWorkspace]) -> String {
    use std::fmt::Write as _;
    if workspaces.is_empty() {
        return "(no workspaces)\n".to_string();
    }
    let mut out = String::new();
    for ws in workspaces {
        let _ = writeln!(
            out,
            "workspace {} \"{}\" ({})",
            ws.id,
            ws.name,
            ws.root.display()
        );
        for pane in &ws.panes {
            let _ = writeln!(out, "  pane {}", pane.id);
            for tab in &pane.tabs {
                let marker = if tab.active { '*' } else { ' ' };
                let agent = tab
                    .agent
                    .as_ref()
                    .map(|agent| format!(" agent={} status={}", agent.name, agent.status.as_str()))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "    {marker} [{}] {} \"{}\"{}",
                    tab.kind, tab.id, tab.title, agent
                );
            }
        }
    }
    out
}
