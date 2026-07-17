// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux hooks setup` — register flowmux's hook entries with each
//! supported agent so its lifecycle events (Stop / Notification / …)
//! call `flowmux hooks <agent> <event>` and surface as system + bell
//! popover notifications.
//!
//! Idempotent: every entry we own carries a `flowmux-hook` marker
//! string in its `command`, so a re-run prunes our previous insertions
//! before re-inserting them. Other entries the user added by hand are
//! preserved verbatim.
//!
//! Supported targets (mirroring cmux):
//! - **Claude Code** — `~/.claude/settings.json` `hooks.{Stop,Notification}`.
//! - **Codex CLI**   — top-level `notify` in `~/.codex/config.toml`;
//!   flowmux-owned entries in the legacy `hooks.json` are removed.
//! - **OpenCode**    — `~/.config/opencode/plugins/flowmux-session.mjs`
//!   plus `opencode.json` `plugin` entry.
//! - **Cline**       — executable lifecycle scripts under `~/.cline/hooks/`
//!   (plus its existing legacy global hooks directory when present).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Marker the hook installer drops into every command line we generate
/// so a re-run can identify (and prune) previous flowmux entries
/// without touching user-authored entries. Mirrors cmux's
/// `cmux-claude-hook-marker` convention.
pub const FLOWMUX_HOOK_MARKER: &str = "flowmux-hook";

/// Plugin source-marker for the OpenCode JS plugin file. Lets a re-run
/// detect that the file is owned by flowmux and may be overwritten.
pub const FLOWMUX_OPENCODE_PLUGIN_MARKER: &str = "flowmux-opencode-session-plugin v3";

/// One agent flowmux knows how to install hooks for. Same enum shape
/// as `agent::Target` so future merges can collapse them, but kept
/// separate today to keep the SKILL installer focused on text payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookTarget {
    Claude,
    Codex,
    OpenCode,
    Cline,
}

impl HookTarget {
    pub const ALL: &'static [HookTarget] = &[
        HookTarget::Claude,
        HookTarget::Codex,
        HookTarget::OpenCode,
        HookTarget::Cline,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            HookTarget::Claude => "claude",
            HookTarget::Codex => "codex",
            HookTarget::OpenCode => "opencode",
            HookTarget::Cline => "cline",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.slug() == s)
    }
}

/// Outcome of installing one target — exposed so a CLI doctor / setup
/// command can render a per-agent summary table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookInstallStatus {
    /// Wrote (or rewrote) the hook entries.
    Installed,
    /// The agent's home directory is not present — flowmux skipped this
    /// target rather than erroring.
    Skipped,
}

#[derive(Debug)]
pub struct HookInstallReport {
    pub target: HookTarget,
    pub status: HookInstallStatus,
    pub touched_paths: Vec<PathBuf>,
}

/// Read-only introspection result for `flowmux doctor`. Mirrors the
/// installer outcomes so the doctor view can report what `flowmux fix`
/// will (or won't) change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCheckStatus {
    /// Agent home dir is absent. `flowmux fix` will skip this target.
    NoAgentHome,
    /// Agent home is present but no flowmux hook entry was found.
    Missing,
    /// flowmux hook entries are present and look correct.
    Installed,
    /// flowmux hook entries are partially present or stale (e.g.
    /// a previous install left the marker on disk but the matching
    /// settings entry is gone). `flowmux fix` re-syncs.
    Drift,
    /// Could not read the agent's config (parse error, permission, …).
    Error(String),
}

#[derive(Debug, Clone)]
pub struct HookCheckEntry {
    pub target: HookTarget,
    pub status: HookCheckStatus,
    /// Files we inspected — useful for the doctor output and for
    /// telling the user where `flowmux fix` will write.
    pub paths: Vec<PathBuf>,
}

/// Inspect every supported target without touching any file. Safe to
/// call from `flowmux doctor`.
pub fn check_all() -> Vec<HookCheckEntry> {
    HookTarget::ALL.iter().map(|t| check(*t)).collect()
}

/// Inspect a single target.
pub fn check(target: HookTarget) -> HookCheckEntry {
    match target {
        HookTarget::Claude => check_claude(),
        HookTarget::Codex => check_codex(),
        HookTarget::OpenCode => check_opencode(),
        HookTarget::Cline => check_cline(),
    }
}

fn check_claude() -> HookCheckEntry {
    let path = match claude_settings_path() {
        Some(p) => p,
        None => return entry(HookTarget::Claude, HookCheckStatus::NoAgentHome, vec![]),
    };
    let agent_home_present = path.parent().map(|p| p.exists()).unwrap_or(false);
    if !agent_home_present {
        return entry(HookTarget::Claude, HookCheckStatus::NoAgentHome, vec![path]);
    }
    if !path.exists() {
        return entry(HookTarget::Claude, HookCheckStatus::Missing, vec![path]);
    }
    let root: Value = match read_json_or_empty_object(&path) {
        Ok(v) => v,
        Err(e) => {
            return entry(
                HookTarget::Claude,
                HookCheckStatus::Error(e.to_string()),
                vec![path],
            )
        }
    };
    let hooks = root.get("hooks").and_then(|h| h.as_object());
    let mut owned_events = 0usize;
    for event in CLAUDE_EVENTS {
        let arr = hooks
            .and_then(|h| h.get(event.name))
            .and_then(|v| v.as_array());
        if let Some(arr) = arr {
            if arr.iter().any(claude_entry_is_flowmux_owned) {
                owned_events += 1;
            }
        }
    }
    let status = match owned_events {
        0 => HookCheckStatus::Missing,
        n if n == CLAUDE_EVENTS.len() => HookCheckStatus::Installed,
        _ => HookCheckStatus::Drift,
    };
    entry(HookTarget::Claude, status, vec![path])
}

fn check_codex() -> HookCheckEntry {
    let home = match codex_home() {
        Some(h) => h,
        None => return entry(HookTarget::Codex, HookCheckStatus::NoAgentHome, vec![]),
    };
    if !home.exists() {
        return entry(
            HookTarget::Codex,
            HookCheckStatus::NoAgentHome,
            vec![home.join("config.toml")],
        );
    }
    let config_path = home.join("config.toml");
    if !config_path.exists() {
        return entry(
            HookTarget::Codex,
            HookCheckStatus::Missing,
            vec![config_path],
        );
    }
    let raw = match fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            return entry(
                HookTarget::Codex,
                HookCheckStatus::Error(e.to_string()),
                vec![config_path],
            )
        }
    };
    let doc: toml_edit::DocumentMut = match raw.parse() {
        Ok(d) => d,
        Err(e) => {
            return entry(
                HookTarget::Codex,
                HookCheckStatus::Error(e.to_string()),
                vec![config_path],
            )
        }
    };
    let notify = doc.get("notify").and_then(|v| v.as_array());
    let status = match notify {
        None => HookCheckStatus::Missing,
        Some(arr) => {
            let strs: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            // Canonical install: ["<bin>", "hooks", "codex", "stop"].
            // We only require that flowmux's verb chain is in the array
            // — the bin path may differ between installs.
            let has_flowmux = strs.iter().any(|s| s.contains("flowmux"));
            let has_verbs = strs.windows(3).any(|w| {
                w == ["hooks", "codex", "stop"] || w == ["hooks", "codex", "notification"]
            });
            if has_flowmux && has_verbs {
                HookCheckStatus::Installed
            } else if has_flowmux || has_verbs {
                HookCheckStatus::Drift
            } else {
                HookCheckStatus::Missing
            }
        }
    };
    entry(HookTarget::Codex, status, vec![config_path])
}

fn check_opencode() -> HookCheckEntry {
    let homes: Vec<PathBuf> = opencode_homes()
        .into_iter()
        .filter(|h| h.exists())
        .collect();
    if homes.is_empty() {
        let stub = opencode_home()
            .map(|h| h.join("opencode.json"))
            .into_iter()
            .collect();
        return entry(HookTarget::OpenCode, HookCheckStatus::NoAgentHome, stub);
    }
    let mut all_paths: Vec<PathBuf> = Vec::with_capacity(homes.len() * 2);
    let mut every_installed = true;
    let mut every_missing = true;
    for home in &homes {
        let plugin_path = home.join("plugins").join("flowmux-session.mjs");
        let opencode_json = home.join("opencode.json");
        all_paths.push(plugin_path.clone());
        all_paths.push(opencode_json.clone());

        let plugin_ok = plugin_path
            .exists()
            .then(|| fs::read_to_string(&plugin_path).ok())
            .flatten()
            .map(|s| s.contains(FLOWMUX_OPENCODE_PLUGIN_MARKER))
            .unwrap_or(false);
        let registered = if opencode_json.exists() {
            match read_json_or_empty_object(&opencode_json) {
                Ok(v) => v
                    .get("plugin")
                    .and_then(|p| p.as_array())
                    .map(|arr| {
                        arr.iter().any(|p| {
                            p.as_str()
                                .map(|s| s.contains("flowmux-session"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false),
                Err(e) => {
                    return entry(
                        HookTarget::OpenCode,
                        HookCheckStatus::Error(e.to_string()),
                        all_paths,
                    )
                }
            }
        } else {
            false
        };
        let this_installed = plugin_ok && registered;
        let this_missing = !plugin_ok && !registered;
        every_installed &= this_installed;
        every_missing &= this_missing;
    }
    let status = if every_installed {
        HookCheckStatus::Installed
    } else if every_missing {
        HookCheckStatus::Missing
    } else {
        HookCheckStatus::Drift
    };
    entry(HookTarget::OpenCode, status, all_paths)
}

fn entry(target: HookTarget, status: HookCheckStatus, paths: Vec<PathBuf>) -> HookCheckEntry {
    HookCheckEntry {
        target,
        status,
        paths,
    }
}

/// Install hooks for a single target. Returns a report; errors are
/// reserved for genuine I/O / parse failures, not "agent isn't here"
/// (that maps to `Skipped`).
pub fn install(target: HookTarget, flowmux_bin: &str) -> Result<HookInstallReport> {
    match target {
        HookTarget::Claude => install_claude(flowmux_bin),
        HookTarget::Codex => install_codex(flowmux_bin),
        HookTarget::OpenCode => install_opencode(flowmux_bin),
        HookTarget::Cline => install_cline(flowmux_bin),
    }
}

/// Remove flowmux entries from a target. Mirrors `install` for users
/// who want to opt out without manually editing every file.
pub fn uninstall(target: HookTarget) -> Result<HookInstallReport> {
    match target {
        HookTarget::Claude => uninstall_claude(),
        HookTarget::Codex => uninstall_codex(),
        HookTarget::OpenCode => uninstall_opencode(),
        HookTarget::Cline => uninstall_cline(),
    }
}

// ---- Agent wrapper shims -------------------------------------------

/// Agents that get a PID-capturing wrapper shim. The GUI prepends the
/// shim dir to a PTY's `PATH`, so typing `claude` / `codex` resolves to
/// these scripts first. They export `FLOWMUX_AGENT_PID=$$` (read by the
/// hooks) and then `exec` the real binary, so they are otherwise fully
/// transparent.
const SHIM_AGENTS: &[&str] = &["claude", "codex", "opencode", "cline"];

/// Body of a wrapper shim for `agent`. Skips flowmux-managed shims when
/// resolving the real binary so it never re-execs itself or another copy. The shim
/// registers a best-effort SessionStart itself because not every agent
/// exposes a startup hook; the daemon still clears dead sessions through
/// the PID liveness sweep when the agent exits without SessionEnd.
fn shim_script(agent: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
# flowmux agent wrapper shim (managed by `flowmux fix`).
# Records the real {agent} PID, registers a best-effort flowmux presence,
# then transparently exec's the real binary. Safe to run outside flowmux.
if [ -n "${{FLOWMUX_SURFACE_ID:-}}" ]; then
  export FLOWMUX_AGENT_PID=$$
  if command -v flowmuxctl >/dev/null 2>&1; then
    flowmuxctl hooks {agent} session-start >/dev/null 2>&1 </dev/null &
  elif command -v flowmux >/dev/null 2>&1; then
    flowmux hooks {agent} session-start >/dev/null 2>&1 </dev/null &
  fi
fi
self_dir=$(cd "$(dirname "$0")" && pwd)
is_flowmux_shim() {{
  grep -q "flowmux agent wrapper shim" "$1" 2>/dev/null
}}
real=""
saved_ifs=$IFS
IFS=:
for d in $PATH; do
  [ "$d" = "$self_dir" ] && continue
  candidate="$d/{agent}"
  [ -x "$candidate" ] || continue
  is_flowmux_shim "$candidate" && continue
  real="$candidate"
  break
done
IFS=$saved_ifs
if [ -z "$real" ]; then
  echo "flowmux shim: {agent} not found on PATH" >&2
  exit 127
fi
exec "$real" "$@"
"#
    )
}

/// Write/refresh the agent wrapper shims into the shim dir and mark them
/// executable. Idempotent; returns the paths touched. `Ok(vec![])` when
/// no data dir is resolvable (e.g. `$HOME` unset).
pub fn install_agent_shims() -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = match flowmux_config::paths::agent_shim_dir() {
        Some(d) => d,
        None => return Ok(vec![]),
    };
    fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    for agent in SHIM_AGENTS {
        let path = dir.join(agent);
        let body = shim_script(agent);
        let up_to_date = fs::read_to_string(&path)
            .map(|c| c == body)
            .unwrap_or(false);
        if !up_to_date {
            fs::write(&path, &body)?;
            written.push(path.clone());
        }
        // Always assert the exec bit — cheap, and a non-executable shim
        // would silently break command resolution.
        let mut perms = fs::metadata(&path)?.permissions();
        if perms.mode() & 0o111 != 0o111 {
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)?;
            if !written.contains(&path) {
                written.push(path.clone());
            }
        }
        if let Some(local_bin) = user_local_bin_dir() {
            fs::create_dir_all(&local_bin)?;
            let local_path = local_bin.join(agent);
            if should_write_local_agent_shim(&local_path, &body) {
                fs::write(&local_path, &body)?;
                let mut perms = fs::metadata(&local_path)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&local_path, perms)?;
                written.push(local_path);
            }
        }
    }
    Ok(written)
}

/// Body of the `tmux` compatibility shim (Claude Code agent teams).
///
/// The GUI prepends the agent shim dir to every PTY's `PATH`, so a
/// Claude Code lead running inside a flowmux pane resolves `tmux` to
/// this script. Swarm-scoped invocations (the `claude-swarm` server
/// socket / session names, or a pane UUID handed out by tmux-compat)
/// are translated into flowmux workspaces and panes via
/// `flowmuxctl tmux-compat`; every other invocation falls through to
/// the real tmux, so humans using tmux inside flowmux are unaffected.
/// `FLOWMUX_TMUX_SHIM=0` disables interception entirely. Unlike the
/// agent wrapper shims this is deliberately NOT mirrored into
/// `~/.local/bin` — hijacking `tmux` outside flowmux panes would be
/// wrong.
pub fn tmux_shim_script() -> String {
    r#"#!/usr/bin/env bash
# flowmux tmux compat shim (managed by `flowmux fix`).
# Routes Claude Code agent-teams swarm calls into flowmux panes and
# passes everything else through to the real tmux.
self_dir=$(cd "$(dirname "$0")" && pwd)
find_real_tmux() {
  local saved_ifs=$IFS d candidate
  IFS=:
  for d in $PATH; do
    [ "$d" = "$self_dir" ] && continue
    candidate="$d/tmux"
    [ -x "$candidate" ] || continue
    grep -q "flowmux tmux compat shim" "$candidate" 2>/dev/null && continue
    printf '%s' "$candidate"
    IFS=$saved_ifs
    return 0
  done
  IFS=$saved_ifs
  return 1
}

swarm=0
if [ -n "${FLOWMUX_SOCKET_PATH:-}" ] && [ "${FLOWMUX_TMUX_SHIM:-1}" != "0" ]; then
  prev=""
  for a in "$@"; do
    case "$prev" in
      -L|-t|-s)
        case "$a" in
          claude-swarm|claude-swarm:*|claude-swarm-*) swarm=1 ;;
          # Pane UUIDs handed out by tmux-compat (legacy default-socket path).
          ????????-????-????-????-????????????) swarm=1 ;;
        esac
        ;;
    esac
    prev="$a"
  done
fi

if [ "$swarm" = "1" ]; then
  if command -v flowmuxctl >/dev/null 2>&1; then
    exec flowmuxctl tmux-compat "$@"
  elif command -v flowmux >/dev/null 2>&1; then
    exec flowmux tmux-compat "$@"
  fi
  echo "flowmux tmux shim: flowmuxctl not found on PATH" >&2
  exit 127
fi

if real=$(find_real_tmux); then
  exec "$real" "$@"
fi

# No real tmux installed. Inside a flowmux pane, still answer the
# availability probe so Claude Code's agent-teams detection succeeds.
if [ -n "${FLOWMUX_SOCKET_PATH:-}" ] && [ "$1" = "-V" ]; then
  if command -v flowmuxctl >/dev/null 2>&1; then
    exec flowmuxctl tmux-compat -V
  elif command -v flowmux >/dev/null 2>&1; then
    exec flowmux tmux-compat -V
  fi
fi
echo "flowmux tmux shim: tmux not found on PATH" >&2
exit 127
"#
    .to_string()
}

/// Write/refresh the `tmux` compat shim into the shim dir. Idempotent;
/// returns the paths touched. `Ok(vec![])` when no data dir resolves.
pub fn install_tmux_shim() -> Result<Vec<PathBuf>> {
    let dir = match flowmux_config::paths::agent_shim_dir() {
        Some(d) => d,
        None => return Ok(vec![]),
    };
    install_tmux_shim_into(&dir)
}

fn install_tmux_shim_into(dir: &Path) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir)?;
    let path = dir.join("tmux");
    let body = tmux_shim_script();
    let mut written = Vec::new();
    let up_to_date = fs::read_to_string(&path)
        .map(|c| c == body)
        .unwrap_or(false);
    if !up_to_date {
        fs::write(&path, &body)?;
        written.push(path.clone());
    }
    let mut perms = fs::metadata(&path)?.permissions();
    if perms.mode() & 0o111 != 0o111 {
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)?;
        if !written.contains(&path) {
            written.push(path);
        }
    }
    Ok(written)
}

fn user_local_bin_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("bin"))
}

fn should_write_local_agent_shim(path: &Path, body: &str) -> bool {
    match fs::read_to_string(path) {
        Ok(existing) => existing.contains("flowmux agent wrapper shim") && existing != body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

// ---- Claude Code ----------------------------------------------------

fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

fn install_claude(flowmux_bin: &str) -> Result<HookInstallReport> {
    let path = match claude_settings_path() {
        Some(p) => p,
        None => return Ok(skipped(HookTarget::Claude)),
    };
    if !path.parent().map(|p| p.exists()).unwrap_or(false) {
        return Ok(skipped(HookTarget::Claude));
    }

    let mut root: Value = read_json_or_empty_object(&path)?;
    let hooks = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks field is not a JSON object in {}", path.display()))?;

    for event in CLAUDE_EVENTS {
        let entry = hooks
            .entry(event.name.to_string())
            .or_insert_with(|| json!([]));
        if !entry.is_array() {
            *entry = json!([]);
        }
        let arr = entry.as_array_mut().unwrap();
        prune_flowmux_claude_entries(arr);
        arr.push(claude_hook_entry(flowmux_bin, *event));
    }

    write_json(&path, &root)?;
    Ok(HookInstallReport {
        target: HookTarget::Claude,
        status: HookInstallStatus::Installed,
        touched_paths: vec![path],
    })
}

fn uninstall_claude() -> Result<HookInstallReport> {
    let path = match claude_settings_path() {
        Some(p) if p.exists() => p,
        _ => return Ok(skipped(HookTarget::Claude)),
    };
    let mut root: Value = read_json_or_empty_object(&path)?;
    if let Some(hooks) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    {
        for event in CLAUDE_EVENTS {
            if let Some(arr) = hooks.get_mut(event.name).and_then(|v| v.as_array_mut()) {
                prune_flowmux_claude_entries(arr);
            }
        }
    }
    write_json(&path, &root)?;
    Ok(HookInstallReport {
        target: HookTarget::Claude,
        status: HookInstallStatus::Installed,
        touched_paths: vec![path],
    })
}

#[derive(Debug, Clone, Copy)]
struct ClaudeEvent {
    /// Event name as Claude Code spells it ("Stop", "Notification", …).
    name: &'static str,
    /// Subcommand fed to `flowmux hooks claude`.
    subcommand: &'static str,
    timeout_secs: u32,
}

const CLAUDE_EVENTS: &[ClaudeEvent] = &[
    ClaudeEvent {
        name: "Stop",
        subcommand: "stop",
        timeout_secs: 10,
    },
    ClaudeEvent {
        name: "Notification",
        subcommand: "notification",
        timeout_secs: 10,
    },
    // Live agent-activity tracking. SessionStart registers the agent's
    // presence/PID; UserPromptSubmit + PreToolUse mark it Running;
    // SessionEnd clears it. SessionEnd uses a tight timeout because it
    // fires on the exit path and must not stall the shell.
    ClaudeEvent {
        name: "SessionStart",
        subcommand: "session-start",
        timeout_secs: 5,
    },
    ClaudeEvent {
        name: "UserPromptSubmit",
        subcommand: "prompt-submit",
        timeout_secs: 5,
    },
    ClaudeEvent {
        name: "PreToolUse",
        subcommand: "pre-tool-use",
        timeout_secs: 5,
    },
    ClaudeEvent {
        name: "SessionEnd",
        subcommand: "session-end",
        timeout_secs: 1,
    },
];

fn claude_hook_entry(flowmux_bin: &str, event: ClaudeEvent) -> Value {
    let prefix = host_invocation_shell_command(flowmux_bin);
    let cmd = format!(
        // Marker `flowmux-hook` lets us identify our own entry on
        // re-install. Whitespace before/after is intentional.
        "{prefix} hooks claude {subcommand}  # {marker}",
        subcommand = event.subcommand,
        marker = FLOWMUX_HOOK_MARKER
    );
    json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": cmd,
            "timeout": event.timeout_secs,
        }]
    })
}

fn prune_flowmux_claude_entries(arr: &mut Vec<Value>) {
    arr.retain(|entry| !claude_entry_is_flowmux_owned(entry));
}

fn claude_entry_is_flowmux_owned(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|v| v.as_array())
        .map(|inner| {
            inner.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains(FLOWMUX_HOOK_MARKER))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ---- Codex CLI ------------------------------------------------------

fn codex_home() -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(env));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

fn install_codex(flowmux_bin: &str) -> Result<HookInstallReport> {
    let home = match codex_home() {
        Some(h) if h.exists() => h,
        _ => return Ok(skipped(HookTarget::Codex)),
    };
    let config_path = home.join("config.toml");

    // Codex 0.130's nested `hooks.json` schema is unstable across
    // releases; even when we wrote it the engine silently ignored
    // `Stop` entries. The legacy `notify` config in `config.toml` is
    // the documented and stable way to surface "agent-turn-complete"
    // events to an external process — cmux's parity path uses it.
    set_codex_notify(&config_path, flowmux_bin)?;

    // We no longer write hooks.json. Clean up any flowmux entry from a
    // previous setup so the file stays consistent with what we own.
    let hooks_path = home.join("hooks.json");
    if hooks_path.exists() {
        let mut root: Value = read_json_or_empty_object(&hooks_path)?;
        if let Some(stop) = root
            .as_object_mut()
            .and_then(|o| o.get_mut("Stop"))
            .and_then(|v| v.as_array_mut())
        {
            prune_flowmux_claude_entries(stop);
            // If this leaves the file empty, delete it; otherwise rewrite.
            if stop.is_empty() {
                if let Some(obj) = root.as_object_mut() {
                    obj.remove("Stop");
                }
            }
        }
        let is_empty = root.as_object().map(|o| o.is_empty()).unwrap_or(false);
        if is_empty {
            let _ = fs::remove_file(&hooks_path);
        } else {
            write_json(&hooks_path, &root)?;
        }
    }

    Ok(HookInstallReport {
        target: HookTarget::Codex,
        status: HookInstallStatus::Installed,
        touched_paths: vec![config_path],
    })
}

fn uninstall_codex() -> Result<HookInstallReport> {
    use toml_edit::DocumentMut;

    let home = match codex_home() {
        Some(h) if h.exists() => h,
        _ => return Ok(skipped(HookTarget::Codex)),
    };
    let config_path = home.join("config.toml");
    if config_path.exists() {
        let original = fs::read_to_string(&config_path)?;
        let mut doc: DocumentMut = original.parse()?;
        let was_flowmux = doc
            .get("notify")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .any(|v| v.as_str().map(|s| s.contains("flowmux")).unwrap_or(false))
            })
            .unwrap_or(false);
        if was_flowmux {
            doc.as_table_mut().remove("notify");
            let new_text = doc.to_string();
            if new_text != original {
                write_atomic(&config_path, new_text.as_bytes())?;
            }
        }
    }

    // Old hook artifacts from previous flowmux installs.
    let hooks_path = home.join("hooks.json");
    if hooks_path.exists() {
        let mut root: Value = read_json_or_empty_object(&hooks_path)?;
        if let Some(stop) = root
            .as_object_mut()
            .and_then(|o| o.get_mut("Stop"))
            .and_then(|v| v.as_array_mut())
        {
            prune_flowmux_claude_entries(stop);
        }
        write_json(&hooks_path, &root)?;
    }
    Ok(HookInstallReport {
        target: HookTarget::Codex,
        status: HookInstallStatus::Installed,
        touched_paths: vec![config_path],
    })
}

/// Set the top-level `notify = [...]` array in `~/.codex/config.toml`
/// so Codex spawns flowmux's hook handler whenever the agent finishes
/// a turn ("agent-turn-complete"). Preserves existing keys; removes
/// the no-longer-needed `[features].hooks` / `codex_hooks` entries
/// from earlier flowmux setups so a re-run quiets the warning.
fn set_codex_notify(config_path: &Path, flowmux_bin: &str) -> Result<()> {
    use toml_edit::{value, Array, DocumentMut};

    let original = match fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::from(e)).context(format!("read {}", config_path.display()))
        }
    };
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| format!("parse {}", config_path.display()))?;

    // notify = ["<flowmux-bin>", "hooks", "codex", "stop"]
    // Outside Flatpak the prefix is just [flowmux_bin]; inside Flatpak
    // it becomes ["flatpak", "run", "--command=flowmuxctl",
    // "com.flowmux.App"] so the host-side Codex process spawns
    // through the runtime instead of touching the sandbox-only binary
    // path directly.
    let prefix = host_invocation_argv(flowmux_bin);
    let mut arr = Array::new();
    for p in &prefix {
        arr.push(p.as_str());
    }
    arr.push("hooks");
    arr.push("codex");
    arr.push("stop");
    doc["notify"] = value(arr);

    // Roll back the old [features] flips — they're no-ops for Codex
    // 0.130+ and the deprecated key triggers a startup warning.
    if let Some(features) = doc
        .as_table_mut()
        .get_mut("features")
        .and_then(|i| i.as_table_mut())
    {
        features.remove("codex_hooks");
        features.remove("hooks");
        if features.is_empty() {
            doc.as_table_mut().remove("features");
        }
    }

    let new_text = doc.to_string();
    if new_text != original {
        write_atomic(config_path, new_text.as_bytes())?;
    }
    Ok(())
}

// ---- Cline ----------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ClineEvent {
    /// Extensionless executable filename used by Cline.
    name: &'static str,
    /// Generic flowmux hook event invoked by the script.
    subcommand: &'static str,
}

const CLINE_EVENTS: &[ClineEvent] = &[
    ClineEvent {
        name: "TaskStart",
        subcommand: "session-start",
    },
    ClineEvent {
        name: "TaskResume",
        subcommand: "session-start",
    },
    ClineEvent {
        name: "UserPromptSubmit",
        subcommand: "running",
    },
    ClineEvent {
        name: "TaskComplete",
        subcommand: "stop",
    },
];

/// Cline's current global hook directory plus its legacy global directory when
/// that tree already exists. `CLINE_HOOKS_DIR` is the documented runtime
/// override and takes precedence when set.
fn cline_hook_dirs() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("CLINE_HOOKS_DIR") {
        if !path.is_empty() {
            return vec![PathBuf::from(path)];
        }
    }
    dirs::home_dir()
        .map(|home| cline_hook_dirs_for_home(&home))
        .unwrap_or_default()
}

fn cline_hook_dirs_for_home(home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let current_root = home.join(".cline");
    if current_root.exists() {
        dirs.push(current_root.join("hooks"));
    }
    let legacy_root = home.join("Documents").join("Cline");
    if legacy_root.exists() {
        dirs.push(legacy_root.join("Hooks"));
    }
    dirs
}

fn check_cline() -> HookCheckEntry {
    let dirs = cline_hook_dirs();
    if dirs.is_empty() {
        let paths = dirs::home_dir()
            .map(|home| home.join(".cline/hooks/TaskComplete"))
            .into_iter()
            .collect();
        return entry(HookTarget::Cline, HookCheckStatus::NoAgentHome, paths);
    }
    check_cline_in_dirs(&dirs)
}

fn check_cline_in_dirs(dirs: &[PathBuf]) -> HookCheckEntry {
    let mut paths = Vec::with_capacity(dirs.len() * CLINE_EVENTS.len());
    let mut installed = 0usize;
    let mut present = 0usize;
    for dir in dirs {
        for event in CLINE_EVENTS {
            let path = dir.join(event.name);
            paths.push(path.clone());
            let body = match fs::read(&path) {
                Ok(body) => {
                    present += 1;
                    body
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return entry(
                        HookTarget::Cline,
                        HookCheckStatus::Error(error.to_string()),
                        paths,
                    )
                }
            };
            let command = format!("hooks cline {}", event.subcommand);
            if bytes_contain(&body, FLOWMUX_HOOK_MARKER.as_bytes())
                && bytes_contain(&body, command.as_bytes())
                && is_executable(&path)
            {
                installed += 1;
            }
        }
    }
    let expected = dirs.len() * CLINE_EVENTS.len();
    let status = if installed == expected {
        HookCheckStatus::Installed
    } else if present == 0 {
        HookCheckStatus::Missing
    } else {
        HookCheckStatus::Drift
    };
    entry(HookTarget::Cline, status, paths)
}

fn install_cline(flowmux_bin: &str) -> Result<HookInstallReport> {
    let dirs = cline_hook_dirs();
    if dirs.is_empty() {
        return Ok(skipped(HookTarget::Cline));
    }
    install_cline_in_dirs(&dirs, flowmux_bin)
}

fn install_cline_in_dirs(dirs: &[PathBuf], flowmux_bin: &str) -> Result<HookInstallReport> {
    use std::os::unix::fs::PermissionsExt;

    let mut touched_paths = Vec::new();
    for dir in dirs {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        for event in CLINE_EVENTS {
            let path = dir.join(event.name);
            let desired = cline_hook_script(flowmux_bin, *event);
            let existing = match fs::read(&path) {
                Ok(body) => Some(body),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(anyhow::Error::from(error))
                        .with_context(|| format!("read {}", path.display()))
                }
            };
            if existing
                .as_deref()
                .is_some_and(|body| !bytes_contain(body, FLOWMUX_HOOK_MARKER.as_bytes()))
            {
                continue;
            }
            let needs_write = existing.as_deref() != Some(desired.as_bytes());
            if needs_write {
                write_atomic(&path, desired.as_bytes())?;
            }
            let mut permissions = fs::metadata(&path)?.permissions();
            let needs_exec = permissions.mode() & 0o111 != 0o111;
            if needs_exec {
                permissions.set_mode(0o755);
                fs::set_permissions(&path, permissions)?;
            }
            if needs_write || needs_exec {
                touched_paths.push(path);
            }
        }
    }
    Ok(HookInstallReport {
        target: HookTarget::Cline,
        status: HookInstallStatus::Installed,
        touched_paths,
    })
}

fn uninstall_cline() -> Result<HookInstallReport> {
    let dirs = cline_hook_dirs();
    if dirs.is_empty() {
        return Ok(skipped(HookTarget::Cline));
    }
    uninstall_cline_in_dirs(&dirs)
}

fn uninstall_cline_in_dirs(dirs: &[PathBuf]) -> Result<HookInstallReport> {
    let mut touched_paths = Vec::new();
    for dir in dirs {
        for event in CLINE_EVENTS {
            let path = dir.join(event.name);
            let body = match fs::read(&path) {
                Ok(body) => body,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(anyhow::Error::from(error))
                        .with_context(|| format!("read {}", path.display()))
                }
            };
            if bytes_contain(&body, FLOWMUX_HOOK_MARKER.as_bytes()) {
                fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
                touched_paths.push(path);
            }
        }
    }
    Ok(HookInstallReport {
        target: HookTarget::Cline,
        status: HookInstallStatus::Installed,
        touched_paths,
    })
}

fn cline_hook_script(flowmux_bin: &str, event: ClineEvent) -> String {
    let prefix = host_invocation_shell_command(flowmux_bin);
    format!(
        r#"#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# {marker} managed Cline {event_name} hook
payload=$(cat)
{prefix} hooks cline {subcommand} -- "$payload" >/dev/null 2>&1 </dev/null
printf '%s\n' '{{"cancel":false}}'
"#,
        marker = FLOWMUX_HOOK_MARKER,
        event_name = event.name,
        subcommand = event.subcommand,
    )
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ---- OpenCode -------------------------------------------------------

fn opencode_home() -> Option<PathBuf> {
    flowmux_config::paths::host_config_dir_for("opencode")
}

/// Every OpenCode config root flowmux should install the plugin
/// into. The primary `~/.config/opencode/` covers the upstream CLI
/// and any fork that honours the default XDG layout. The
/// `opencode-anycli` wrapper at https://github.com/JSUYA/opencode-anycli
/// re-launches opencode with `XDG_CONFIG_HOME=~/.config/opencode-anycli`
/// so its plugin loader only sees
/// `~/.config/opencode-anycli/opencode/plugins/`; without an entry
/// there the hook never reaches OpenCode and the in-app bell stays
/// silent.
///
/// Only existing roots are returned — we never create the
/// `opencode-anycli` tree on machines that don't have the wrapper
/// installed. The Flatpak build still installs into the anycli root
/// because the wrapper always runs on the host, and its
/// `$HOME/.config/opencode-anycli/` tree is bind-mounted into the
/// sandbox via the manifest's `--filesystem=home`, so the same write
/// path lands at the same on-disk bytes either way.
fn opencode_homes() -> Vec<PathBuf> {
    opencode_homes_for(opencode_home(), host_home_dir())
}

/// Pure-function core of [`opencode_homes`] — `primary` is the upstream
/// `~/.config/opencode/` (or `host_config_dir_for("opencode")` inside
/// Flatpak) and `host_home` is the host `$HOME` used to look up the
/// optional `opencode-anycli` tree. Split out so tests can exercise
/// the anycli-detection branch without touching real env vars.
fn opencode_homes_for(primary: Option<PathBuf>, host_home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = primary {
        out.push(p);
    }
    if let Some(home) = host_home {
        let anycli = home
            .join(".config")
            .join("opencode-anycli")
            .join("opencode");
        if anycli.exists() && !out.contains(&anycli) {
            out.push(anycli);
        }
    }
    out
}

/// `$HOME` as seen on the host filesystem. Inside the Flatpak sandbox
/// the manifest's `--filesystem=home` keeps `$HOME` pointing at the
/// host's user dir (not the sandbox-private `~/.var/app/...`), so a
/// plain `HOME` lookup is the right primitive for hook paths that
/// must agree with the host-side agent's view of disk.
fn host_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// Host-side spawn argv the OpenCode plugin should call. Outside a
/// Flatpak sandbox this is just `[FLOWMUX_BIN]` so spawn behaves
/// like before. Inside the sandbox the plugin is read by host
/// OpenCode, so we wrap the in-sandbox `flowmuxctl` with `flatpak
/// run --command=…` — the host has `flatpak` on PATH and the spawn
/// crosses back into the same sandbox the daemon lives in.
fn opencode_spawn_argv(flowmux_bin: &str) -> Vec<String> {
    host_invocation_argv(flowmux_bin)
}

/// Shell-command string the Claude / Codex hook entries write into
/// the agent's config file. Mirrors [`opencode_spawn_argv`] for the
/// agents that expect a single command string rather than an argv —
/// Claude's `settings.json` `hooks[*].command` and Codex's
/// `config.toml` `notify` are both shell strings.
fn host_invocation_shell_command(flowmux_bin: &str) -> String {
    let argv = host_invocation_argv(flowmux_bin);
    argv.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

fn host_invocation_argv(flowmux_bin: &str) -> Vec<String> {
    if flowmux_config::paths::is_flatpak_sandbox() {
        let app_id = std::env::var("FLATPAK_ID").unwrap_or_else(|_| "com.flowmux.App".to_string());
        // `--command` accepts a name resolved against /app/bin first
        // and an absolute path otherwise. We pass the bare name so the
        // /app/bin/flowmuxctl symlink (added by the manifest) keeps
        // the entry short and stable across path changes.
        let _ = flowmux_bin; // Sandbox builds resolve via FLATPAK_ID + app PATH.
        vec![
            "flatpak".to_string(),
            "run".to_string(),
            "--command=flowmuxctl".to_string(),
            app_id,
        ]
    } else {
        vec![flowmux_bin.to_string()]
    }
}

/// Conservative POSIX shell quoting for paths and app-ids — wraps
/// in single quotes and escapes embedded single quotes. Used only by
/// the hook installer so it stays close to the call site.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '=' | ':'))
    {
        return s.to_string();
    }
    let escaped = s.replace('\'', r"'\''");
    format!("'{escaped}'")
}

fn install_opencode(flowmux_bin: &str) -> Result<HookInstallReport> {
    let homes: Vec<PathBuf> = opencode_homes()
        .into_iter()
        .filter(|h| h.exists())
        .collect();
    if homes.is_empty() {
        return Ok(skipped(HookTarget::OpenCode));
    }
    let argv = opencode_spawn_argv(flowmux_bin);
    let plugin_src = opencode_plugin_source_with_argv(&argv);
    let mut touched: Vec<PathBuf> = Vec::with_capacity(homes.len() * 2);
    for home in &homes {
        let plugin_dir = home.join("plugins");
        fs::create_dir_all(&plugin_dir)
            .with_context(|| format!("create {}", plugin_dir.display()))?;
        // Older flowmux installs wrote a CommonJS `.js` plugin; OpenCode
        // 1.14+ refuses to load it. Purge it so re-running setup is enough.
        let _ = fs::remove_file(plugin_dir.join("flowmux-session.js"));
        let plugin_path = plugin_dir.join("flowmux-session.mjs");
        if !plugin_path.exists()
            || fs::read_to_string(&plugin_path).ok().as_deref() != Some(plugin_src.as_str())
        {
            write_atomic(&plugin_path, plugin_src.as_bytes())?;
        }

        let opencode_json = home.join("opencode.json");
        register_opencode_plugin(&opencode_json, "flowmux-session", &plugin_path)?;
        touched.push(plugin_path);
        touched.push(opencode_json);
    }
    Ok(HookInstallReport {
        target: HookTarget::OpenCode,
        status: HookInstallStatus::Installed,
        touched_paths: touched,
    })
}

fn uninstall_opencode() -> Result<HookInstallReport> {
    let homes: Vec<PathBuf> = opencode_homes()
        .into_iter()
        .filter(|h| h.exists())
        .collect();
    if homes.is_empty() {
        return Ok(skipped(HookTarget::OpenCode));
    }
    let mut touched: Vec<PathBuf> = Vec::with_capacity(homes.len() * 2);
    for home in &homes {
        let plugin_path = home.join("plugins").join("flowmux-session.mjs");
        let _ = fs::remove_file(&plugin_path);

        let opencode_json = home.join("opencode.json");
        if opencode_json.exists() {
            let mut root: Value = read_json_or_empty_object(&opencode_json)?;
            if let Some(plugins) = root
                .as_object_mut()
                .and_then(|o| o.get_mut("plugin"))
                .and_then(|v| v.as_array_mut())
            {
                plugins.retain(|p| {
                    p.as_str()
                        .map(|s| !s.contains("flowmux-session"))
                        .unwrap_or(true)
                });
            }
            write_json(&opencode_json, &root)?;
        }
        touched.push(plugin_path);
        touched.push(opencode_json);
    }
    Ok(HookInstallReport {
        target: HookTarget::OpenCode,
        status: HookInstallStatus::Installed,
        touched_paths: touched,
    })
}

/// Back-compat single-string entry point for older tests that pass a
/// bare binary path. New call sites prefer
/// [`opencode_plugin_source_with_argv`] so the spawn array can carry
/// the Flatpak `flatpak run …` prefix.
#[allow(dead_code)]
fn opencode_plugin_source(flowmux_bin: &str) -> String {
    opencode_plugin_source_with_argv(&[flowmux_bin.to_string()])
}

fn opencode_plugin_source_with_argv(argv: &[String]) -> String {
    let head = argv.first().map(|s| s.as_str()).unwrap_or("flowmux");
    let trailing: Vec<String> = argv.iter().skip(1).cloned().collect();
    let trailing_literal = serde_json::to_string(&trailing).unwrap_or_else(|_| "[]".into());
    // OpenCode 1.14+ plugins are ESM modules. Path-loaded plugins
    // (`file:///…`) must export an `id` so OpenCode can name them; npm
    // packages skip that because the package name is the id. The
    // `server` factory returns a `Hooks` object whose `event` callback
    // receives every lifecycle event — we spawn the matching
    // `flowmux hooks opencode <event>` for the ones we care about.
    //
    // Events we surface today:
    // - `session.status` busy/retry → `running` (agent started/resumed)
    // - `session.idle`              → `stop` (agent finished)
    // - `session.error`             → `notification` (agent errored)
    // - `permission.asked`          → `notification` (needs approval)
    // - `permission.replied`        → `running` (approval handled)
    // - `permission.updated` remains a legacy request-event fallback.
    //
    // The optional second positional arg is a JSON payload that the
    // matching Rust handler (`AgentHookEvent::Notification`) parses to
    // populate the toast body — keeps the alert informative instead of
    // a generic "needs your attention".
    format!(
        r#"// {marker}
// Auto-installed by `flowmux hooks setup`. Do not hand-edit; rerun the
// command instead. Removing this file is safe — flowmux just stops
// surfacing OpenCode lifecycle events to the bell popover.

import {{ spawn }} from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

// `FLOWMUX_BIN` is the executable invoked from the host. Outside
// Flatpak it is the absolute path to `flowmuxctl`. Inside Flatpak
// the hook installer rewrites it to `flatpak` and prepends the
// runtime args (`run --command=… com.flowmux.App`) so the spawn
// crosses back into the same sandbox the daemon lives in. Either
// way the trailing `["hooks", "opencode", <event>]` args land at the
// in-sandbox CLI unchanged.
const FLOWMUX_BIN = {bin_literal};
const FLOWMUX_ARGS_PREFIX = {trailing_literal};

// Mirror of the Rust-side debug log path. Writing from JS here puts a
// timestamped trace of every plugin invocation right next to the
// `cli/hook entry` line the in-sandbox CLI emits — so when the chain
// breaks we can see at a glance whether the plugin fired at all,
// what its process.env looked like, and what argv it sent across the
// `flatpak run` boundary. Append-only; both writers (JS plugin and
// Rust CLI) can hold the file simultaneously because they only append.
function debugLogPath() {{
  const home = process.env.HOME;
  if (!home) return null;
  return path.join(home, ".cache", "flowmux", "notify-debug.log");
}}

function logDebug(line) {{
  try {{
    const target = debugLogPath();
    if (!target) return;
    fs.mkdirSync(path.dirname(target), {{ recursive: true }});
    const stamp = new Date().toISOString();
    fs.appendFileSync(target, "[" + stamp + "] [opencode-plugin] " + line + "\n");
  }} catch (_) {{
    // Logging failures must never break the hook.
  }}
}}

// Build the final argv handed to spawn().
//
// Critical: `flatpak run` strips the host process's env to a minimal
// sandbox set before invoking the in-sandbox program, so values like
// FLOWMUX_PANE_ID never reach the in-sandbox flowmuxctl through env
// inheritance. The daemon then receives Notify{{pane:None}} and the
// sidebar can't blink / clicks can't navigate. We sidestep that by
// pushing the same values as explicit `--pane` / `--surface` flags
// at the end of the CLI invocation: argv survives the sandbox
// boundary, so the in-sandbox CLI sees the real ids regardless of
// what flatpak did to the environment.
function buildSpawnArgs(event, payload) {{
  const args = [...FLOWMUX_ARGS_PREFIX];
  args.push("hooks", "opencode", event);
  const pane = process.env.FLOWMUX_PANE_ID;
  const surface = process.env.FLOWMUX_SURFACE_ID;
  if (pane) args.push("--pane", pane);
  if (surface) args.push("--surface", surface);
  if (payload) args.push(payload);
  return args;
}}

function fireFlowmuxHook(event, payload) {{
  let args;
  try {{
    args = buildSpawnArgs(event, payload);
  }} catch (e) {{
    logDebug("buildSpawnArgs ERROR event=" + event + " err=" + String(e));
    return;
  }}
  logDebug(
    "fire event=" + event +
    " bin=" + FLOWMUX_BIN +
    " env.FLOWMUX_PANE_ID=" + (process.env.FLOWMUX_PANE_ID || "<unset>") +
    " env.FLOWMUX_SURFACE_ID=" + (process.env.FLOWMUX_SURFACE_ID || "<unset>") +
    " env.FLOWMUX_SOCKET_PATH=" + (process.env.FLOWMUX_SOCKET_PATH || "<unset>") +
    " argv=" + JSON.stringify(args)
  );
  try {{
    const child = spawn(FLOWMUX_BIN, args, {{
      stdio: "ignore",
      detached: true,
    }});
    child.on("error", (e) => logDebug("spawn error event=" + event + " err=" + String(e)));
    child.unref();
  }} catch (e) {{
    logDebug("spawn threw event=" + event + " err=" + String(e));
    // Hook failures must never crash OpenCode.
  }}
}}

export const id = "flowmux-session";

function sessionPayload(event) {{
  const properties = event && event.properties;
  if (!properties) return null;
  const session = properties.session;
  const sessionId = properties.sessionID || properties.sessionId || properties.session_id ||
    (session && (session.id || session.sessionID || session.sessionId));
  return sessionId ? JSON.stringify({{ session_id: String(sessionId) }}) : null;
}}

export const server = async () => ({{
  event: async ({{ event }}) => {{
    if (!event || typeof event.type !== "string") return;
    const t = event.type;
    const payload = sessionPayload(event);
    if (t === "session.created" || t === "session.updated") {{
      fireFlowmuxHook("session-start", payload);
    }} else if (t === "session.idle") fireFlowmuxHook("stop", payload);
    else if (t === "session.status") {{
      const status = event.properties && event.properties.status;
      if (status && (status.type === "busy" || status.type === "retry")) {{
        fireFlowmuxHook("running", payload);
      }}
    }} else if (t === "session.error") {{
      fireFlowmuxHook("notification", JSON.stringify({{ message: "OpenCode session error" }}));
    }} else if (t === "permission.asked" || t === "permission.updated") {{
      fireFlowmuxHook("notification", JSON.stringify({{ message: "OpenCode needs your input" }}));
    }} else if (t === "permission.replied") {{
      fireFlowmuxHook("running");
    }}
  }},
}});

export default {{ id, server }};
"#,
        marker = FLOWMUX_OPENCODE_PLUGIN_MARKER,
        bin_literal = serde_json::to_string(head).unwrap_or_else(|_| "\"flowmux\"".into()),
        trailing_literal = trailing_literal,
    )
}

fn register_opencode_plugin(path: &Path, plugin_name: &str, plugin_path: &Path) -> Result<()> {
    let mut root: Value = read_json_or_empty_object(path)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let plugins = obj.entry("plugin".to_string()).or_insert_with(|| json!([]));
    if !plugins.is_array() {
        *plugins = json!([]);
    }
    let arr = plugins.as_array_mut().unwrap();
    // OpenCode 1.14+ rejects `file://./...` (host = "." is invalid on
    // Linux) and `.js` (no longer ESM). Strip every previously-written
    // flowmux registration so we can replace it with the canonical
    // absolute-path `.mjs` URL.
    arr.retain(|v| v.as_str().map(|s| !s.contains(plugin_name)).unwrap_or(true));
    let canonical_uri = canonical_file_uri(plugin_path);
    arr.push(Value::String(canonical_uri));
    write_json(path, &root)?;
    Ok(())
}

/// Build a `file:///absolute/path` URL — the only form OpenCode 1.14+
/// accepts on Linux (`file://./relative` fails with "File URL host
/// must be 'localhost' or empty").
fn canonical_file_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let s = abs.to_string_lossy();
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

// ---- shared helpers -------------------------------------------------

fn skipped(target: HookTarget) -> HookInstallReport {
    HookInstallReport {
        target,
        status: HookInstallStatus::Skipped,
        touched_paths: vec![],
    }
}

fn read_json_or_empty_object(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parse JSON: {}", path.display()))
        }
        Ok(_) => Ok(json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(anyhow::Error::from(e)).context(format!("read {}", path.display())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(value)
        .with_context(|| format!("serialize {}", path.display()))?;
    write_atomic(path, body.as_bytes())
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.flowmux-tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("hook")
    ));
    fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A tiny fixture that overrides `dirs::home_dir` via env. We just
    /// use TempDir + explicit paths instead.
    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn cline_install_is_idempotent_and_doctor_reports_installed() {
        let dir = tmp();
        let hooks_dir = dir.path().join(".cline/hooks");
        let dirs = vec![hooks_dir.clone()];

        let first = install_cline_in_dirs(&dirs, "/usr/local/bin/flowmux").unwrap();
        assert_eq!(first.touched_paths.len(), CLINE_EVENTS.len());
        assert_eq!(
            check_cline_in_dirs(&dirs).status,
            HookCheckStatus::Installed
        );

        for event in CLINE_EVENTS {
            let path = hooks_dir.join(event.name);
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.starts_with("#!/usr/bin/env bash\n"));
            assert!(body.contains(FLOWMUX_HOOK_MARKER));
            assert!(body.contains(&format!("hooks cline {}", event.subcommand)));
            assert!(body.contains("{\"cancel\":false}"));
            assert!(is_executable(&path));
        }

        let second = install_cline_in_dirs(&dirs, "/usr/local/bin/flowmux").unwrap();
        assert!(second.touched_paths.is_empty());
    }

    #[test]
    fn cline_install_and_uninstall_preserve_unmarked_user_hook() {
        let dir = tmp();
        let hooks_dir = dir.path().join(".cline/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let manual = hooks_dir.join("TaskComplete");
        fs::write(&manual, "#!/bin/sh\nprintf manual\\n\n").unwrap();
        let dirs = vec![hooks_dir.clone()];

        install_cline_in_dirs(&dirs, "flowmux").unwrap();
        assert_eq!(
            fs::read_to_string(&manual).unwrap(),
            "#!/bin/sh\nprintf manual\\n\n"
        );
        assert_eq!(check_cline_in_dirs(&dirs).status, HookCheckStatus::Drift);

        let report = uninstall_cline_in_dirs(&dirs).unwrap();
        assert_eq!(report.touched_paths.len(), CLINE_EVENTS.len() - 1);
        assert!(manual.exists());
        assert_eq!(
            fs::read_to_string(&manual).unwrap(),
            "#!/bin/sh\nprintf manual\\n\n"
        );
    }

    #[test]
    fn cline_hook_dirs_include_current_and_existing_legacy_roots() {
        let dir = tmp();
        fs::create_dir_all(dir.path().join(".cline")).unwrap();
        fs::create_dir_all(dir.path().join("Documents/Cline")).unwrap();

        assert_eq!(
            cline_hook_dirs_for_home(dir.path()),
            vec![
                dir.path().join(".cline/hooks"),
                dir.path().join("Documents/Cline/Hooks"),
            ]
        );
    }

    #[test]
    fn claude_install_creates_settings_with_lifecycle_event_entries() {
        let dir = tmp();
        let claude_dir = dir.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let path = claude_dir.join("settings.json");
        // Fresh install path: empty file.
        let mut root = read_json_or_empty_object(&path).unwrap();
        let hooks = root
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .unwrap();
        for event in CLAUDE_EVENTS {
            let arr = hooks
                .entry(event.name.to_string())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            prune_flowmux_claude_entries(arr);
            arr.push(claude_hook_entry("flowmux", *event));
        }
        write_json(&path, &root).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let stop = &written["hooks"]["Stop"][0];
        assert_eq!(stop["matcher"], "");
        let cmd = stop["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("flowmux hooks claude stop"));
        assert!(cmd.contains(FLOWMUX_HOOK_MARKER));

        // The activity-tracking lifecycle events are installed alongside
        // Stop/Notification, each mapped to its kebab-case subcommand.
        for (name, subcommand) in [
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt-submit"),
            ("PreToolUse", "pre-tool-use"),
            ("SessionEnd", "session-end"),
        ] {
            let cmd = written["hooks"][name][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("missing hook entry for {name}"));
            assert!(
                cmd.contains(&format!("flowmux hooks claude {subcommand}")),
                "event {name} should invoke `{subcommand}`, got: {cmd}"
            );
        }
    }

    #[test]
    fn shim_script_exports_agent_pid_and_execs_real_binary() {
        let body = shim_script("claude");
        assert!(body.starts_with("#!/usr/bin/env bash"));
        assert!(body.contains("export FLOWMUX_AGENT_PID=$$"));
        // Only when inside flowmux, so it stays transparent elsewhere.
        assert!(body.contains("FLOWMUX_SURFACE_ID"));
        // Registers presence even for agents without their own startup hook.
        assert!(body.contains("flowmuxctl hooks claude session-start"));
        assert!(body.contains("flowmux hooks claude session-start"));
        // Skips its own dir, skips other flowmux shims, and exec's the resolved real binary.
        assert!(body.contains("[ \"$d\" = \"$self_dir\" ] && continue"));
        assert!(body.contains("is_flowmux_shim \"$candidate\" && continue"));
        assert!(body.contains("exec \"$real\" \"$@\""));
        // Agent name is substituted into the lookup.
        assert!(body.contains("$d/claude"));
    }

    #[test]
    fn local_agent_shim_write_policy_preserves_existing_real_binary() {
        let dir = tmp();
        let path = dir.path().join("codex");
        let body = shim_script("codex");

        assert!(should_write_local_agent_shim(&path, &body));
        fs::write(&path, "#!/bin/sh\necho real codex\n").unwrap();
        assert!(!should_write_local_agent_shim(&path, &body));
        fs::write(
            &path,
            shim_script("codex").replace("exec \"$real\"", "exec \"$real\" "),
        )
        .unwrap();
        assert!(should_write_local_agent_shim(&path, &body));
    }

    #[test]
    fn tmux_shim_intercepts_swarm_and_passes_through_everything_else() {
        let body = tmux_shim_script();
        // Marker so the shim recognizes (and skips) itself on PATH.
        assert!(body.contains("flowmux tmux compat shim"));
        // Swarm socket/session names and pane UUIDs are intercepted…
        assert!(body.contains("claude-swarm|claude-swarm:*|claude-swarm-*"));
        assert!(body.contains("????????-????-????-????-????????????"));
        // …and routed to the tmux-compat verb.
        assert!(body.contains("exec flowmuxctl tmux-compat \"$@\""));
        // Interception only happens inside a flowmux pane, with an
        // escape hatch.
        assert!(body.contains("FLOWMUX_SOCKET_PATH"));
        assert!(body.contains("FLOWMUX_TMUX_SHIM"));
        // Non-swarm usage execs the real tmux, skipping the shim dir.
        assert!(body.contains("[ \"$d\" = \"$self_dir\" ] && continue"));
        assert!(body.contains("exec \"$real\" \"$@\""));
    }

    #[test]
    fn tmux_shim_install_is_idempotent_and_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        let written = install_tmux_shim_into(dir.path()).unwrap();
        assert_eq!(written.len(), 1);
        let path = &written[0];
        assert_eq!(path.file_name().unwrap(), "tmux");
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "shim must be executable");

        // Second run: nothing to do.
        let written = install_tmux_shim_into(dir.path()).unwrap();
        assert!(written.is_empty());

        // Drift (edited body) is re-synced.
        fs::write(dir.path().join("tmux"), "#!/bin/sh\n").unwrap();
        let written = install_tmux_shim_into(dir.path()).unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("tmux")).unwrap(),
            tmux_shim_script()
        );
    }

    /// Run the installed shim under real bash with a controlled PATH:
    /// fake `flowmuxctl` and fake `tmux` record their argv, so the
    /// routing decision (intercept vs pass-through) is observable.
    #[test]
    fn tmux_shim_routes_correctly_under_bash() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = tmp();
        let shim_dir = dir.path().join("shims");
        install_tmux_shim_into(&shim_dir).unwrap();
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let log = dir.path().join("calls.log");

        let fake = |name: &str, label: &str| {
            let path = bin_dir.join(name);
            fs::write(
                &path,
                format!("#!/bin/sh\necho \"{label}: $*\" >> '{}'\n", log.display()),
            )
            .unwrap();
            let mut p = fs::metadata(&path).unwrap().permissions();
            p.set_mode(0o755);
            fs::set_permissions(&path, p).unwrap();
        };
        fake("flowmuxctl", "ctl");
        fake("tmux", "realtmux");

        // A coreutils-only dir (bash + the externals the shim calls) that
        // deliberately omits any system `tmux`. The "no real tmux installed"
        // cases run with this on PATH instead of /usr/bin, so they stay
        // reproducible on CI images that ship tmux in /usr/bin.
        let tools_dir = dir.path().join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        for tool in ["bash", "dirname", "grep"] {
            let src = ["/bin", "/usr/bin", "/usr/local/bin"]
                .iter()
                .map(|d| std::path::Path::new(d).join(tool))
                .find(|p| p.exists())
                .unwrap_or_else(|| panic!("required tool not found: {tool}"));
            std::os::unix::fs::symlink(src, tools_dir.join(tool)).unwrap();
        }

        let run = |args: &[&str], socket: Option<&str>, shim_env: Option<&str>| {
            let mut cmd = Command::new(shim_dir.join("tmux"));
            // Keep the system dirs so bash/dirname/grep resolve; our
            // dirs come first so the fakes win.
            cmd.args(args).env_clear().env(
                "PATH",
                format!("{}:{}:/usr/bin:/bin", shim_dir.display(), bin_dir.display()),
            );
            if let Some(s) = socket {
                cmd.env("FLOWMUX_SOCKET_PATH", s);
            }
            if let Some(v) = shim_env {
                cmd.env("FLOWMUX_TMUX_SHIM", v);
            }
            let status = cmd.status().unwrap();
            let calls = fs::read_to_string(&log).unwrap_or_default();
            fs::write(&log, "").unwrap();
            (status, calls)
        };

        // Swarm-shaped + inside flowmux pane → intercepted.
        let (status, calls) = run(
            &["-L", "claude-swarm-42", "has-session", "-t", "claude-swarm"],
            Some("/tmp/flowmux.sock"),
            None,
        );
        assert!(status.success());
        assert_eq!(
            calls.trim(),
            "ctl: tmux-compat -L claude-swarm-42 has-session -t claude-swarm"
        );

        // Legacy path: pane-UUID target without -L is intercepted too.
        let (_, calls) = run(
            &["kill-pane", "-t", "0b8e7f66-90bc-4f74-9e2e-7f3f4be2a111"],
            Some("/tmp/flowmux.sock"),
            None,
        );
        assert!(calls.starts_with("ctl: tmux-compat kill-pane"));

        // Ordinary tmux usage passes through to the real tmux.
        let (_, calls) = run(
            &["new-session", "-s", "mywork"],
            Some("/tmp/flowmux.sock"),
            None,
        );
        assert_eq!(calls.trim(), "realtmux: new-session -s mywork");

        // Outside a flowmux pane, even swarm shapes pass through.
        let (_, calls) = run(
            &["-L", "claude-swarm-42", "has-session", "-t", "claude-swarm"],
            None,
            None,
        );
        assert!(calls.starts_with("realtmux:"));

        // Escape hatch disables interception.
        let (_, calls) = run(
            &["-L", "claude-swarm-42", "has-session", "-t", "claude-swarm"],
            Some("/tmp/flowmux.sock"),
            Some("0"),
        );
        assert!(calls.starts_with("realtmux:"));

        // No real tmux installed: the availability probe still answers
        // through tmux-compat inside a flowmux pane. Runs on an isolated
        // PATH (coreutils only, no system tmux) so this holds on CI.
        fs::remove_file(bin_dir.join("tmux")).unwrap();
        let run_no_tmux = |args: &[&str], socket: Option<&str>| {
            let mut cmd = Command::new(shim_dir.join("tmux"));
            cmd.args(args).env_clear().env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    shim_dir.display(),
                    bin_dir.display(),
                    tools_dir.display()
                ),
            );
            if let Some(s) = socket {
                cmd.env("FLOWMUX_SOCKET_PATH", s);
            }
            let status = cmd.status().unwrap();
            let calls = fs::read_to_string(&log).unwrap_or_default();
            fs::write(&log, "").unwrap();
            (status, calls)
        };
        let (status, calls) = run_no_tmux(&["-V"], Some("/tmp/flowmux.sock"));
        assert!(status.success());
        assert_eq!(calls.trim(), "ctl: tmux-compat -V");

        // …but outside a pane it reports tmux as missing.
        let (status, calls) = run_no_tmux(&["-V"], None);
        assert_eq!(status.code(), Some(127));
        assert_eq!(calls.trim(), "");
    }

    #[test]
    fn claude_install_is_idempotent_and_preserves_user_entries() {
        let dir = tmp();
        let path = dir.path().join("settings.json");
        // Pre-existing user hook the user wants kept.
        let initial = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "/usr/local/bin/userscript.sh", "timeout": 30 }]
                }]
            }
        });
        write_json(&path, &initial).unwrap();

        // Install once.
        let mut root: Value = read_json_or_empty_object(&path).unwrap();
        upsert_claude_for_test(&mut root, "flowmux");
        write_json(&path, &root).unwrap();

        // Install twice.
        let mut root: Value = read_json_or_empty_object(&path).unwrap();
        upsert_claude_for_test(&mut root, "flowmux");
        write_json(&path, &root).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        // Exactly 2: the user's + flowmux's. No duplicate flowmux.
        assert_eq!(stop.len(), 2, "got: {stop:?}");
        // User entry is unchanged.
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "/usr/local/bin/userscript.sh"
        );
        // flowmux entry has the marker.
        let cmd = stop[1]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains(FLOWMUX_HOOK_MARKER));
    }

    fn upsert_claude_for_test(root: &mut Value, bin: &str) {
        let hooks = root
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .unwrap();
        for event in CLAUDE_EVENTS {
            let arr = hooks
                .entry(event.name.to_string())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            prune_flowmux_claude_entries(arr);
            arr.push(claude_hook_entry(bin, *event));
        }
    }

    #[test]
    fn claude_uninstall_removes_only_flowmux_entries() {
        let dir = tmp();
        let path = dir.path().join("settings.json");
        let initial = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "user_thing", "timeout": 5 }] },
                    { "matcher": "", "hooks": [{ "type": "command", "command": "/usr/bin/flowmux hooks claude stop  # flowmux-hook", "timeout": 10 }] }
                ]
            }
        });
        write_json(&path, &initial).unwrap();
        let mut root: Value = read_json_or_empty_object(&path).unwrap();
        if let Some(arr) = root["hooks"]["Stop"].as_array_mut() {
            prune_flowmux_claude_entries(arr);
        }
        write_json(&path, &root).unwrap();
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "user_thing");
    }

    #[test]
    fn codex_set_notify_writes_array_without_clobbering_other_keys() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"model = "gpt-x"

[projects."/a"]
trust_level = "trusted"
"#,
        )
        .unwrap();
        set_codex_notify(&path, "/usr/local/bin/flowmux").unwrap();
        let new = fs::read_to_string(&path).unwrap();
        // Original keys preserved.
        assert!(new.contains("model = \"gpt-x\""));
        assert!(new.contains("trust_level = \"trusted\""));
        // notify array present with our argv.
        assert!(
            new.contains(r#"notify = ["/usr/local/bin/flowmux", "hooks", "codex", "stop"]"#)
                || new.contains("notify = [\"/usr/local/bin/flowmux\",")
        );
        assert!(new.contains("\"hooks\""));
        assert!(new.contains("\"codex\""));
        assert!(new.contains("\"stop\""));
    }

    #[test]
    fn codex_set_notify_strips_deprecated_features_keys() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"[features]
codex_hooks = true
hooks = true
"#,
        )
        .unwrap();
        set_codex_notify(&path, "flowmux").unwrap();
        let new = fs::read_to_string(&path).unwrap();
        assert!(!new.contains("codex_hooks"), "stale key remained: {new}");
        assert!(!new.contains("hooks = true"), "stale flag remained: {new}");
        assert!(new.contains("notify = "));
    }

    #[test]
    fn codex_set_notify_idempotent_no_op_on_second_call() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        fs::write(&path, "").unwrap();
        set_codex_notify(&path, "flowmux").unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        set_codex_notify(&path, "flowmux").unwrap();
        let after_second = fs::read_to_string(&path).unwrap();
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn opencode_register_plugin_appends_unique_entry() {
        let dir = tmp();
        let path = dir.path().join("opencode.json");
        let plugin_path = dir.path().join("plugins/flowmux-session.mjs");
        fs::create_dir_all(plugin_path.parent().unwrap()).unwrap();
        fs::write(&plugin_path, "// stub").unwrap();
        register_opencode_plugin(&path, "flowmux-session", &plugin_path).unwrap();
        register_opencode_plugin(&path, "flowmux-session", &plugin_path).unwrap(); // idempotent
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let plugins = v["plugin"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        let entry = plugins[0].as_str().unwrap();
        assert!(entry.contains("flowmux-session"));
        assert!(entry.ends_with(".mjs"), "must use ESM extension: {entry}");
        // OpenCode 1.14+ requires file:// with empty host (`file:///abs`).
        assert!(entry.starts_with("file:///"), "must be absolute: {entry}");
    }

    #[test]
    fn opencode_register_plugin_replaces_stale_relative_or_js_entries() {
        let dir = tmp();
        let path = dir.path().join("opencode.json");
        let plugin_path = dir.path().join("plugins/flowmux-session.mjs");
        fs::create_dir_all(plugin_path.parent().unwrap()).unwrap();
        fs::write(&plugin_path, "// stub").unwrap();
        // Simulate a previous flowmux install which used `.js` and
        // the invalid relative `file://./` URL.
        let initial = json!({
            "plugin": [
                "file://./plugins/flowmux-session.js",
                "file://./plugins/flowmux-session.mjs",
                "@user/unrelated",
            ]
        });
        write_json(&path, &initial).unwrap();
        register_opencode_plugin(&path, "flowmux-session", &plugin_path).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let plugins = v["plugin"].as_array().unwrap();
        // Stale flowmux-session entries replaced; user's unrelated plugin kept.
        assert!(plugins
            .iter()
            .any(|p| p.as_str().unwrap() == "@user/unrelated"));
        let flowmux_entries: Vec<&str> = plugins
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| s.contains("flowmux-session"))
            .collect();
        assert_eq!(flowmux_entries.len(), 1);
        assert!(flowmux_entries[0].starts_with("file:///"));
        assert!(flowmux_entries[0].ends_with(".mjs"));
    }

    #[test]
    fn opencode_register_preserves_existing_unrelated_plugins() {
        let dir = tmp();
        let path = dir.path().join("opencode.json");
        let plugin_path = dir.path().join("plugins/flowmux-session.mjs");
        fs::create_dir_all(plugin_path.parent().unwrap()).unwrap();
        fs::write(&plugin_path, "// stub").unwrap();
        let initial = json!({ "plugin": ["@user/foo", "@user/bar"] });
        write_json(&path, &initial).unwrap();
        register_opencode_plugin(&path, "flowmux-session", &plugin_path).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let plugins = v["plugin"].as_array().unwrap();
        assert_eq!(plugins.len(), 3);
        assert_eq!(plugins[0], "@user/foo");
    }

    #[test]
    fn opencode_plugin_source_carries_marker_and_bin_path() {
        let src = opencode_plugin_source("/usr/local/bin/flowmux");
        assert!(src.contains(FLOWMUX_OPENCODE_PLUGIN_MARKER));
        assert!(src.contains("/usr/local/bin/flowmux"));
        assert!(src.contains("session.idle"));
        assert!(src.contains("session.created"));
        assert!(src.contains("session.updated"));
        assert!(src.contains("sessionPayload"));
        assert!(src.contains("session_id"));
        // Must be an ESM module so OpenCode 1.14+ loads it.
        assert!(src.contains("import"));
        assert!(src.contains("export const server"));
    }

    #[test]
    fn opencode_plugin_source_passes_pane_and_surface_as_cli_args() {
        // `flatpak run` resets env to a minimal sandbox set, so the
        // in-sandbox flowmuxctl could not recover FLOWMUX_PANE_ID /
        // FLOWMUX_SURFACE_ID from inherited env. Solution: push the
        // values onto the CLI argv as explicit `--pane` / `--surface`
        // flags — argv survives the sandbox boundary intact. Pin the
        // JS-side argv shape so a future refactor cannot quietly
        // regress the sidebar / click-navigation path.
        let src = opencode_plugin_source_with_argv(&[
            "flatpak".to_string(),
            "run".to_string(),
            "--command=flowmuxctl".to_string(),
            "com.flowmux.App".to_string(),
        ]);
        // CLI argv path
        assert!(src.contains("FLOWMUX_PANE_ID"));
        assert!(src.contains("FLOWMUX_SURFACE_ID"));
        assert!(src.contains("\"--pane\""));
        assert!(src.contains("\"--surface\""));
        // Diagnostic logging path so future failures are self-evident
        // in notify-debug.log without an iterative reproduction loop.
        assert!(src.contains("notify-debug.log"));
        assert!(src.contains("logDebug"));
        assert!(src.contains("appendFileSync"));
        // The legacy `--env=` forwarding is removed; the argv path is
        // the single source of truth across the flatpak boundary.
        assert!(!src.contains("--env="));
    }

    #[test]
    fn opencode_plugin_source_omits_pane_flag_outside_flatpak() {
        // Outside Flatpak the spawn inherits env directly, so the
        // legacy env-var path still resolves pane/surface. The plugin
        // can stay symmetrical (always push `--pane` when the env var
        // is set) — but it must NOT push the flag with an empty string,
        // which would make clap fail to parse Option<PaneId>. This
        // pins the `if (pane) args.push(...)` guard.
        let src = opencode_plugin_source("/usr/local/bin/flowmux");
        // The guard is what keeps empty-value pushes out.
        assert!(src.contains("if (pane) args.push(\"--pane\""));
        assert!(src.contains("if (surface) args.push(\"--surface\""));
    }

    #[test]
    fn opencode_plugin_source_routes_permission_events_to_notification() {
        let src = opencode_plugin_source("flowmux");
        assert!(src.contains("permission.asked"));
        assert!(src.contains("permission.updated"));
        assert!(src.contains("permission.replied"));
        assert!(src.contains("session.status"));
        assert!(src.contains("status.type === \"busy\""));
        assert!(src.contains("status.type === \"retry\""));
        assert!(src.contains("fireFlowmuxHook(\"running\")"));
        // Errors and permission requests both go through the
        // `notification` subcommand with a JSON payload so the toast
        // body is informative.
        assert!(src.contains("OpenCode needs your input"));
        assert!(src.contains("OpenCode session error"));
    }

    #[test]
    fn opencode_homes_includes_anycli_tree_when_present() {
        // The opencode-anycli wrapper sets
        // XDG_CONFIG_HOME=~/.config/opencode-anycli, so its plugin
        // loader only sees ~/.config/opencode-anycli/opencode/plugins/.
        // The Flatpak build must still install there: the wrapper
        // always runs on the host, and the tree is bind-mounted into
        // the sandbox via --filesystem=home. Before this assertion the
        // sandbox branch dropped the anycli root entirely and OpenCode
        // never saw the flowmux plugin.
        let dir = tmp();
        let host_home = dir.path().to_path_buf();
        let anycli_tree = host_home
            .join(".config")
            .join("opencode-anycli")
            .join("opencode");
        fs::create_dir_all(&anycli_tree).unwrap();
        let primary = host_home.join(".config").join("opencode");
        let homes = opencode_homes_for(Some(primary.clone()), Some(host_home));
        assert_eq!(homes, vec![primary, anycli_tree]);
    }

    #[test]
    fn opencode_homes_skips_anycli_tree_when_absent() {
        // Machines without opencode-anycli should not have the
        // wrapper's plugin tree fabricated on disk — only the primary
        // root is returned.
        let dir = tmp();
        let host_home = dir.path().to_path_buf();
        let primary = host_home.join(".config").join("opencode");
        let homes = opencode_homes_for(Some(primary.clone()), Some(host_home));
        assert_eq!(homes, vec![primary]);
    }

    #[test]
    fn write_atomic_replaces_target_via_rename() {
        let dir = tmp();
        let path = dir.path().join("a/b/c.txt");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn read_json_or_empty_object_handles_missing_and_empty_files() {
        let dir = tmp();
        let missing = dir.path().join("missing.json");
        assert_eq!(read_json_or_empty_object(&missing).unwrap(), json!({}));
        let empty = dir.path().join("empty.json");
        fs::write(&empty, "").unwrap();
        assert_eq!(read_json_or_empty_object(&empty).unwrap(), json!({}));
        let blank = dir.path().join("blank.json");
        fs::write(&blank, "   \n  \t").unwrap();
        assert_eq!(read_json_or_empty_object(&blank).unwrap(), json!({}));
    }

    #[test]
    fn read_json_or_empty_object_errors_on_invalid_json() {
        let dir = tmp();
        let bad = dir.path().join("bad.json");
        fs::write(&bad, "{ not valid").unwrap();
        assert!(read_json_or_empty_object(&bad).is_err());
    }
}

#[cfg(test)]
mod render_dump {
    use super::*;

    #[test]
    #[ignore] // run manually with `cargo test -p flowmux-cli -- --ignored render_dump::dump_plugin_source`
    fn dump_plugin_source() {
        let src = opencode_plugin_source_with_argv(&[
            "flatpak".into(),
            "run".into(),
            "--command=flowmuxctl".into(),
            "com.flowmux.App".into(),
        ]);
        eprintln!(
            "\n----- BEGIN PLUGIN SOURCE -----\n{}\n----- END PLUGIN SOURCE -----\n",
            src
        );
    }
}
