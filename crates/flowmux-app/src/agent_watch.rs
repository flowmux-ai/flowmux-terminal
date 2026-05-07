// SPDX-License-Identifier: GPL-3.0-or-later
//! Detect when AI-coding-agent commands finish in any terminal pane.
//!
//! Mirrors cmux's signature attention behavior: when claude / codex /
//! opencode finishes a turn the user wants to be told. flowmux polls
//! the descendant tree of each terminal pane's shell every couple of
//! seconds and emits a one-shot `AgentCompleted` event whenever an
//! agent process that *was* there a tick ago is no longer there.
//!
//! Comparison is by `comm` (the basename of the executable from
//! `/proc/<pid>/comm`) so it doesn't matter which directory the
//! agent was invoked from. We deliberately avoid diffing against
//! exit-status, since agents typically self-restart between turns
//! while keeping the same parent shell — what we want to capture is
//! "agent isn't running right now", not "agent crashed".

use crate::bridge::{Bridge, GtkCommand};
use crate::ui::workspace_view::PaneRegistry;
use flowmux_core::PaneId;
use gtk::glib;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

/// Process-name prefixes we treat as agent commands.
///
/// Match is on the basename of any argv element (not just `comm`)
/// because claude / codex / opencode-anycli are Node-based CLIs:
/// their `/proc/<pid>/comm` is just `node`, while the full argv
/// contains the entry script path that ends in `claude`, `codex`,
/// `opencode`, etc.
const AGENT_PREFIXES: &[&str] = &["claude", "codex", "opencode"];

/// Returns the matching agent name when `pid` looks like one of the
/// tracked agents (or one of their Node/Python wrappers).
fn agent_name_for(pid: u32) -> Option<String> {
    if let Some(comm) = flowmux_procmon::comm_of(pid) {
        for prefix in AGENT_PREFIXES {
            if comm.starts_with(prefix) {
                return Some(comm);
            }
        }
    }
    // Fall through to argv: handles `node /path/.../claude/cli.js …`
    // and similar Python/Ruby wrappers.
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    for chunk in cmdline.split(|&b| b == 0) {
        let arg = match std::str::from_utf8(chunk) {
            Ok(s) if !s.is_empty() => s,
            _ => continue,
        };
        let basename = std::path::Path::new(arg)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(arg);
        for prefix in AGENT_PREFIXES {
            if basename.starts_with(prefix) {
                return Some(basename.to_string());
            }
        }
    }
    None
}

#[derive(Default)]
struct AgentWatcher {
    /// Per-pane set of agent comm strings observed last tick.
    state: HashMap<PaneId, HashSet<String>>,
}

impl AgentWatcher {
    fn poll(&mut self, registry: &PaneRegistry) -> Vec<(PaneId, String)> {
        let mut events = Vec::new();
        let mut now: HashMap<PaneId, HashSet<String>> = HashMap::new();
        for (pane_id, term) in registry.terminals.iter() {
            let agents = collect_agents(term);
            if let Some(prev) = self.state.get(pane_id) {
                for gone in prev.difference(&agents) {
                    events.push((*pane_id, gone.clone()));
                }
            }
            now.insert(*pane_id, agents);
        }
        // Forget panes that no longer exist (closed tabs etc).
        self.state = now;
        events
    }
}

fn collect_agents(term: &crate::ui::terminal_pane::TerminalPane) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(shell_pid) = term.pid.get() else {
        return out;
    };
    let Ok(descendants) = flowmux_procmon::descendants(shell_pid as u32) else {
        return out;
    };
    for pid in descendants {
        if pid as i32 == shell_pid {
            continue; // skip the shell itself
        }
        if let Some(name) = agent_name_for(pid) {
            out.insert(name);
        }
    }
    out
}

/// Install a glib timeout that polls the registry every second and
/// forwards any `AgentCompleted` events through the bridge.
///
/// 1 s is short enough to catch a typical agent turn (which lasts
/// many seconds at minimum) yet not so frequent that scanning
/// `/proc/<pid>/{comm,cmdline}` shows up in profiles.
pub fn install(pane_registry: Rc<RefCell<PaneRegistry>>, bridge: Bridge) {
    let watcher: Rc<RefCell<AgentWatcher>> = Rc::new(RefCell::new(AgentWatcher::default()));
    glib::timeout_add_local(Duration::from_secs(1), move || {
        let registry = pane_registry.borrow();
        let events = watcher.borrow_mut().poll(&registry);
        drop(registry);
        for (pane, name) in events {
            tracing::info!(%pane, %name, "agent disappeared from pane");
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::AgentCompleted { pane, name })
                    .await;
            });
        }
        glib::ControlFlow::Continue
    });
}
