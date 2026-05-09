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
//! - **Codex CLI**   — `~/.codex/hooks.json` `Stop`, plus
//!                     `~/.codex/config.toml` `[features] codex_hooks = true`.
//! - **OpenCode**    — `~/.config/opencode/plugins/flowmux-session.mjs`
//!                     plus `opencode.json` `plugin` entry.

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
pub const FLOWMUX_OPENCODE_PLUGIN_MARKER: &str = "flowmux-opencode-session-plugin v1";

/// One agent flowmux knows how to install hooks for. Same enum shape
/// as `agent::Target` so future merges can collapse them, but kept
/// separate today to keep the SKILL installer focused on text payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookTarget {
    Claude,
    Codex,
    OpenCode,
}

impl HookTarget {
    pub const ALL: &'static [HookTarget] =
        &[HookTarget::Claude, HookTarget::Codex, HookTarget::OpenCode];

    pub fn slug(self) -> &'static str {
        match self {
            HookTarget::Claude => "claude",
            HookTarget::Codex => "codex",
            HookTarget::OpenCode => "opencode",
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

impl HookCheckStatus {
    pub fn label(&self) -> &'static str {
        match self {
            HookCheckStatus::NoAgentHome => "no-agent",
            HookCheckStatus::Missing => "missing",
            HookCheckStatus::Installed => "ok",
            HookCheckStatus::Drift => "drift",
            HookCheckStatus::Error(_) => "error",
        }
    }
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
        return entry(HookTarget::Codex, HookCheckStatus::Missing, vec![config_path]);
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
    let home = match opencode_home() {
        Some(h) => h,
        None => return entry(HookTarget::OpenCode, HookCheckStatus::NoAgentHome, vec![]),
    };
    if !home.exists() {
        return entry(
            HookTarget::OpenCode,
            HookCheckStatus::NoAgentHome,
            vec![home.join("opencode.json")],
        );
    }
    let plugin_path = home.join("plugins").join("flowmux-session.mjs");
    let opencode_json = home.join("opencode.json");
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
                    vec![plugin_path, opencode_json],
                )
            }
        }
    } else {
        false
    };
    let status = match (plugin_ok, registered) {
        (true, true) => HookCheckStatus::Installed,
        (false, false) => HookCheckStatus::Missing,
        _ => HookCheckStatus::Drift,
    };
    entry(
        HookTarget::OpenCode,
        status,
        vec![plugin_path, opencode_json],
    )
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
    }
}

/// Remove flowmux entries from a target. Mirrors `install` for users
/// who want to opt out without manually editing every file.
pub fn uninstall(target: HookTarget) -> Result<HookInstallReport> {
    match target {
        HookTarget::Claude => uninstall_claude(),
        HookTarget::Codex => uninstall_codex(),
        HookTarget::OpenCode => uninstall_opencode(),
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
];

fn claude_hook_entry(flowmux_bin: &str, event: ClaudeEvent) -> Value {
    let cmd = format!(
        // Marker `flowmux-hook` lets us identify our own entry on
        // re-install. Whitespace before/after is intentional.
        "{flowmux_bin} hooks claude {subcommand}  # {marker}",
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
    let mut arr = Array::new();
    arr.push(flowmux_bin);
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

// ---- OpenCode -------------------------------------------------------

fn opencode_home() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("opencode"))
}

fn install_opencode(flowmux_bin: &str) -> Result<HookInstallReport> {
    let home = match opencode_home() {
        Some(h) if h.exists() => h,
        _ => return Ok(skipped(HookTarget::OpenCode)),
    };
    let plugin_dir = home.join("plugins");
    fs::create_dir_all(&plugin_dir).with_context(|| format!("create {}", plugin_dir.display()))?;
    // Older flowmux installs wrote a CommonJS `.js` plugin; OpenCode
    // 1.14+ refuses to load it. Purge it so re-running setup is enough.
    let _ = fs::remove_file(plugin_dir.join("flowmux-session.js"));
    let plugin_path = plugin_dir.join("flowmux-session.mjs");
    let plugin_src = opencode_plugin_source(flowmux_bin);
    if !plugin_path.exists()
        || fs::read_to_string(&plugin_path).ok().as_deref() != Some(plugin_src.as_str())
    {
        write_atomic(&plugin_path, plugin_src.as_bytes())?;
    }

    let opencode_json = home.join("opencode.json");
    register_opencode_plugin(&opencode_json, "flowmux-session", &plugin_path)?;

    Ok(HookInstallReport {
        target: HookTarget::OpenCode,
        status: HookInstallStatus::Installed,
        touched_paths: vec![plugin_path, opencode_json],
    })
}

fn uninstall_opencode() -> Result<HookInstallReport> {
    let home = match opencode_home() {
        Some(h) if h.exists() => h,
        _ => return Ok(skipped(HookTarget::OpenCode)),
    };
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
    Ok(HookInstallReport {
        target: HookTarget::OpenCode,
        status: HookInstallStatus::Installed,
        touched_paths: vec![plugin_path, opencode_json],
    })
}

fn opencode_plugin_source(flowmux_bin: &str) -> String {
    // OpenCode 1.14+ plugins are ESM modules. Path-loaded plugins
    // (`file:///…`) must export an `id` so OpenCode can name them; npm
    // packages skip that because the package name is the id. The
    // `server` factory returns a `Hooks` object whose `event` callback
    // receives every lifecycle event — we spawn the matching
    // `flowmux hooks opencode <event>` for the ones we care about.
    format!(
        r#"// {marker}
// Auto-installed by `flowmux hooks setup`. Do not hand-edit; rerun the
// command instead. Removing this file is safe — flowmux just stops
// surfacing OpenCode lifecycle events to the bell popover.

import {{ spawn }} from "node:child_process";

const FLOWMUX_BIN = {bin_literal};

function fireFlowmuxHook(event) {{
  try {{
    spawn(FLOWMUX_BIN, ["hooks", "opencode", event], {{
      stdio: "ignore",
      detached: true,
    }}).unref();
  }} catch (_) {{
    // Hook failures must never crash OpenCode.
  }}
}}

export const id = "flowmux-session";

export const server = async () => ({{
  event: async ({{ event }}) => {{
    if (!event || typeof event.type !== "string") return;
    if (event.type === "session.idle") fireFlowmuxHook("stop");
    else if (event.type === "session.error") fireFlowmuxHook("notification");
  }},
}});

export default {{ id, server }};
"#,
        marker = FLOWMUX_OPENCODE_PLUGIN_MARKER,
        bin_literal = serde_json::to_string(flowmux_bin).unwrap_or_else(|_| "\"flowmux\"".into()),
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
    fn claude_install_creates_settings_with_two_event_entries() {
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
        // Must be an ESM module so OpenCode 1.14+ loads it.
        assert!(src.contains("import"));
        assert!(src.contains("export const server"));
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
