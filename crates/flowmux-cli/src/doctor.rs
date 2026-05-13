// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux doctor` / `flowmux fix` — single entry point that audits
//! every flowmux ↔ host integration and (for `fix`) re-applies the
//! pieces the user is missing.
//!
//! Why a separate module: `agent::` covers the SKILL files and
//! `hook_install::` covers each agent's lifecycle hook config. They
//! were added incrementally and `flowmux agent doctor` / `flowmux
//! hooks doctor` only show one half each. After every flowmux upgrade
//! — and, just as often, after the user installs Claude / Codex /
//! OpenCode for the first time on a host that already had flowmux —
//! the user wants a single "is everything wired?" check plus a single
//! "wire it" command. That's what this module gives them.
//!
//! Doctor never writes; `fix` does. Both share the same in-memory
//! report shape so callers can render text or JSON.
//!
//! Browser-side checks live here too. They cover four things the
//! user asked for explicitly: WebKitGTK env-var defaults the GUI sets
//! at startup, the in-app browser data dir, host-browser detection
//! (for the cookie importer), and a daemon ping that confirms a
//! browser pane could be spawned at all.

use crate::{agent, hook_install};
use anyhow::Result;
use flowmux_ipc::{client::Client, protocol::Request, protocol::Response};
use serde_json::json;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Status code per doctor entry. Distinct from the per-domain enums in
/// `agent::DoctorStatus` and `hook_install::HookCheckStatus` so the
/// renderer can colourise without caring which subsystem produced the
/// row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Everything is wired correctly.
    Ok,
    /// Purely informational — not actionable, not a problem.
    Info,
    /// Soft warning. `flowmux fix` may help, but the host environment
    /// is the more likely culprit (e.g. the agent isn't installed).
    Warn,
    /// `flowmux fix` will install or repair this row.
    NeedsFix,
    /// Hit an I/O / parse error while inspecting. Surfaced verbatim so
    /// the user can fix permissions etc. by hand.
    Error,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Info => "info",
            Status::Warn => "warn",
            Status::NeedsFix => "fix",
            Status::Error => "error",
        }
    }

    /// ANSI SGR colour for the status badge. Returned as a numeric
    /// code (e.g. `32` for green) so the renderer can wrap it in the
    /// standard `\x1b[<code>m…\x1b[0m` envelope. `Info` gets no
    /// colour because every section starts with one and a tinted
    /// "info" everywhere would just become noise.
    fn ansi_code(&self) -> Option<&'static str> {
        match self {
            Status::Ok => Some("32"),       // green
            Status::NeedsFix => Some("31"), // red — actionable
            Status::Error => Some("31"),    // red
            Status::Warn => Some("33"),     // yellow
            Status::Info => None,
        }
    }
}

/// Wrap `s` in the SGR escape for `status` when colour is enabled.
/// `None` colour codes (Info) are returned as-is so the caller can
/// uniformly format every row without branching.
fn colorize(s: &str, status: &Status, use_color: bool) -> String {
    if !use_color {
        return s.to_string();
    }
    match status.ansi_code() {
        Some(code) => format!("\x1b[{code}m{s}\x1b[0m"),
        None => s.to_string(),
    }
}

/// Decide whether to emit ANSI colours. Honours the de-facto `NO_COLOR`
/// convention (https://no-color.org) and only colours when stdout is
/// an interactive terminal — piping `flowmux doctor | tee` stays
/// plain text so log files don't get unprintable bytes.
fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// e.g. "claude-code skill", "codex hooks", "host browsers".
    pub name: String,
    pub status: Status,
    /// Single-line summary shown next to the status. Path / count /
    /// short reason — anything that fits one row.
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub sections: Vec<Section>,
}

impl Report {
    /// `true` iff at least one entry is `NeedsFix` or `Error`. The CLI
    /// uses this to set its exit code.
    pub fn has_problems(&self) -> bool {
        self.sections
            .iter()
            .flat_map(|s| &s.entries)
            .any(|e| matches!(e.status, Status::NeedsFix | Status::Error))
    }
}

/// Synchronous slice of the doctor — runs every check that doesn't
/// require the daemon. Pure file-system inspection, safe to call in
/// tests with a fake `home`.
pub fn collect_offline(home: &Path, codex_home: Option<&Path>) -> Report {
    Report {
        sections: vec![section_agents(home, codex_home), section_browser_offline()],
    }
}

/// Full doctor — adds a daemon ping section if the socket is
/// reachable. Async because the IPC client is async.
pub async fn collect(home: &Path, codex_home: Option<&Path>, socket: Option<PathBuf>) -> Report {
    let mut report = collect_offline(home, codex_home);
    let daemon_section = section_daemon(socket).await;
    // Insert daemon section between agents and browser so the
    // browser-pane check at the bottom can reference it.
    if let Some(idx) = report.sections.iter().position(|s| s.title == "Browser") {
        report.sections.insert(idx, daemon_section);
    } else {
        report.sections.push(daemon_section);
    }
    report
}

// ---- AI agents ------------------------------------------------------

fn section_agents(home: &Path, codex_home: Option<&Path>) -> Section {
    let skill_report = agent::doctor_all(agent::Target::ALL, home, codex_home);
    let hook_report = hook_install::check_all();

    let mut entries = Vec::new();
    for target in agent::Target::ALL {
        let skill = skill_report
            .iter()
            .find(|e| e.target == *target)
            .expect("doctor_all returns one entry per target");
        let hook_target = hook_target_for(*target);
        let hook = hook_report
            .iter()
            .find(|e| e.target == hook_target)
            .expect("check_all returns one entry per target");
        let agent_present = agent_is_installed(*target, home, codex_home);

        let skill_entry = Entry {
            name: format!("{} skill", target.slug()),
            status: skill_status(&skill.status, agent_present),
            detail: skill_detail(&skill.status, &skill.path, agent_present),
        };
        let hook_entry = Entry {
            name: format!("{} hooks", target.slug()),
            status: hook_status(&hook.status),
            detail: hook_detail(&hook.status, &hook.paths),
        };
        entries.push(skill_entry);
        // Legacy Codex sibling file (`$CODEX_HOME/flowmux-browser.md`)
        // from before we moved to `$CODEX_HOME/skills/...`. The file is
        // harmless but no longer referenced; if any survived an upgrade,
        // surface a row so the user knows `flowmux fix` will clean it.
        if matches!(target, agent::Target::Codex) {
            if let Some(entry) = codex_legacy_entry(home, codex_home) {
                entries.push(entry);
            }
        }
        entries.push(hook_entry);
    }

    Section {
        title: "AI agents".into(),
        entries,
    }
}

/// Path of the pre-skill Codex sibling file flowmux used to write
/// before Codex CLI gained native skills. Kept here (rather than in
/// `agent::`) because it only exists for the migration story —
/// nothing in the install path ever writes here today.
fn codex_legacy_sibling_path(home: &Path, codex_home: Option<&Path>) -> PathBuf {
    codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex"))
        .join("flowmux-browser.md")
}

/// Build a doctor entry for the Codex legacy sibling file *iff* it
/// exists on disk. Returning `None` when it's absent keeps the
/// report quiet for fresh installs — we only want to surface this
/// row to users who upgraded from a pre-skills flowmux build.
fn codex_legacy_entry(home: &Path, codex_home: Option<&Path>) -> Option<Entry> {
    let path = codex_legacy_sibling_path(home, codex_home);
    if !path.exists() {
        return None;
    }
    Some(Entry {
        name: "codex legacy skill".into(),
        status: Status::NeedsFix,
        detail: format!("{} (pre-skills sibling — `flowmux fix` removes it)", path.display()),
    })
}

fn hook_target_for(t: agent::Target) -> hook_install::HookTarget {
    match t {
        agent::Target::ClaudeCode => hook_install::HookTarget::Claude,
        agent::Target::OpenCode => hook_install::HookTarget::OpenCode,
        agent::Target::Codex => hook_install::HookTarget::Codex,
    }
}

/// Whether the user actually has the agent's CLI / config tree on
/// disk. The skill installer happily creates `~/.claude/...` even if
/// the user has never run Claude Code, so we use the parent dir as
/// the "is the agent installed" signal.
fn agent_is_installed(t: agent::Target, home: &Path, codex_home: Option<&Path>) -> bool {
    match t {
        agent::Target::ClaudeCode => home.join(".claude").exists(),
        agent::Target::OpenCode => home.join(".config").join("opencode").exists(),
        agent::Target::Codex => codex_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".codex"))
            .exists(),
    }
}

fn skill_status(status: &agent::DoctorStatus, agent_present: bool) -> Status {
    match status {
        agent::DoctorStatus::Ok => Status::Ok,
        agent::DoctorStatus::Drift => Status::NeedsFix,
        agent::DoctorStatus::Missing if agent_present => Status::NeedsFix,
        agent::DoctorStatus::Missing => Status::Warn,
        agent::DoctorStatus::Error(_) => Status::Error,
    }
}

fn skill_detail(status: &agent::DoctorStatus, path: &Path, agent_present: bool) -> String {
    match status {
        agent::DoctorStatus::Ok => path.display().to_string(),
        agent::DoctorStatus::Drift => {
            format!("{} (drift — `flowmux fix` re-syncs)", path.display())
        }
        agent::DoctorStatus::Missing if agent_present => {
            format!("{} (missing — `flowmux fix` installs)", path.display())
        }
        agent::DoctorStatus::Missing => {
            format!("agent not installed; will install when you run the agent")
        }
        agent::DoctorStatus::Error(e) => format!("{}: {e}", path.display()),
    }
}

fn hook_status(status: &hook_install::HookCheckStatus) -> Status {
    use hook_install::HookCheckStatus as S;
    match status {
        S::Installed => Status::Ok,
        S::Missing => Status::NeedsFix,
        S::Drift => Status::NeedsFix,
        S::NoAgentHome => Status::Warn,
        S::Error(_) => Status::Error,
    }
}

fn hook_detail(status: &hook_install::HookCheckStatus, paths: &[PathBuf]) -> String {
    use hook_install::HookCheckStatus as S;
    let path_str = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match status {
        S::Installed => path_str,
        S::Missing => format!("{path_str} (missing — `flowmux fix` installs)"),
        S::Drift => format!("{path_str} (drift — `flowmux fix` re-syncs)"),
        S::NoAgentHome => "agent not installed".into(),
        S::Error(e) => format!("{path_str}: {e}"),
    }
}

// ---- Daemon --------------------------------------------------------

async fn section_daemon(socket: Option<PathBuf>) -> Section {
    let resolved = socket
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET_PATH").map(PathBuf::from))
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from))
        .unwrap_or_else(flowmux_config::paths::runtime_socket);

    let mut entries = vec![Entry {
        name: "socket".into(),
        status: if resolved.exists() {
            Status::Info
        } else {
            Status::Info
        },
        detail: resolved.display().to_string(),
    }];

    // Try a Ping with a tight timeout — if the GUI isn't running this
    // is the expected case, so we report it as Warn (browser pane
    // can't be opened without the GUI) instead of NeedsFix.
    let ping = match tokio::time::timeout(Duration::from_millis(250), Client::connect(&resolved))
        .await
    {
        Ok(Ok(c)) => {
            match tokio::time::timeout(Duration::from_millis(250), c.call(Request::Ping)).await {
                Ok(Ok(Response::Pong)) => Ok(()),
                Ok(Ok(other)) => Err(format!("unexpected response: {other:?}")),
                Ok(Err(e)) => Err(format!("call failed: {e}")),
                Err(_) => Err("ping timed out".into()),
            }
        }
        Ok(Err(e)) => Err(format!("connect failed: {e}")),
        Err(_) => Err("connect timed out".into()),
    };
    let (status, detail) = match ping {
        Ok(()) => (
            Status::Ok,
            "daemon responded to Ping (browser pane available)".into(),
        ),
        Err(reason) => (
            Status::Warn,
            format!("daemon not reachable — start `flowmux` to enable browser panes ({reason})"),
        ),
    };
    entries.push(Entry {
        name: "daemon".into(),
        status,
        detail,
    });

    Section {
        title: "Daemon".into(),
        entries,
    }
}

// ---- Browser -------------------------------------------------------

fn section_browser_offline() -> Section {
    let mut entries = vec![webkit_env_entry(), browser_data_dir_entry()];
    entries.extend(host_browser_entries());
    Section {
        title: "Browser".into(),
        entries,
    }
}

/// The GUI binary auto-sets `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS`
/// and `WEBKIT_DISABLE_DMABUF_RENDERER` at startup unless the user
/// pre-set them. The CLI process doesn't share those env vars, so the
/// most useful thing we can report is "the GUI applies safe defaults
/// — here's the override the user set, if any".
fn webkit_env_entry() -> Entry {
    let sandbox = std::env::var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").ok();
    let dmabuf = std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").ok();
    let detail = match (sandbox, dmabuf) {
        (None, None) => "no overrides; flowmux GUI applies safe defaults at startup".into(),
        (s, d) => format!(
            "user override: SANDBOX={} DMABUF={}",
            s.as_deref().unwrap_or("<unset>"),
            d.as_deref().unwrap_or("<unset>"),
        ),
    };
    Entry {
        name: "webkit env".into(),
        status: Status::Info,
        detail,
    }
}

/// `$XDG_DATA_HOME/flowmux/browser/default` is where the in-app
/// browser persists cookies / localStorage. Created on first browser
/// pane open. Doctor reports presence so the user can spot a broken
/// permissions layout (e.g. XDG_DATA_HOME unwritable).
fn browser_data_dir_entry() -> Entry {
    let base = match dirs::data_dir() {
        Some(b) => b,
        None => {
            return Entry {
                name: "browser data".into(),
                status: Status::Warn,
                detail: "XDG data dir unavailable (HOME unset?)".into(),
            }
        }
    };
    let dir = base.join("flowmux").join("browser").join("default");
    if dir.exists() {
        let writable = is_writable(&dir);
        if writable {
            Entry {
                name: "browser data".into(),
                status: Status::Ok,
                detail: dir.display().to_string(),
            }
        } else {
            Entry {
                name: "browser data".into(),
                status: Status::Warn,
                detail: format!("{} (not writable)", dir.display()),
            }
        }
    } else {
        Entry {
            name: "browser data".into(),
            status: Status::Info,
            detail: format!("{} (created on first browser pane open)", dir.display()),
        }
    }
}

fn is_writable(p: &Path) -> bool {
    use std::fs::OpenOptions;
    let probe = p.join(".flowmux-doctor-write-probe");
    let ok = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe)
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// One row per known host browser, summarising whether the cookie
/// importer would find it.
fn host_browser_entries() -> Vec<Entry> {
    let sources = flowmux_cookies::discover_sources();
    let detected: Vec<String> = sources
        .iter()
        .filter_map(|s| s.detect().map(|_| s.id().slug().to_string()))
        .collect();
    let missing: Vec<String> = sources
        .iter()
        .filter(|s| s.detect().is_none())
        .map(|s| s.id().slug().to_string())
        .collect();

    let detected_summary = if detected.is_empty() {
        "none — `flowmux import-cookies` will have nothing to read".into()
    } else {
        detected.join(", ")
    };
    let mut out = vec![Entry {
        name: "host browsers".into(),
        status: if detected.is_empty() {
            Status::Info
        } else {
            Status::Ok
        },
        detail: detected_summary,
    }];
    if !missing.is_empty() {
        out.push(Entry {
            name: "  not detected".into(),
            status: Status::Info,
            detail: missing.join(", "),
        });
    }
    out
}

// ---- Rendering -----------------------------------------------------

pub fn render_text(report: &Report) -> String {
    render_text_with_color(report, color_enabled())
}

fn render_text_with_color(report: &Report, use_color: bool) -> String {
    let mut out = String::new();
    for section in &report.sections {
        out.push_str(&format!("{}\n", section.title));
        for entry in &section.entries {
            out.push_str(&format_status_row(
                &entry.status,
                &entry.name,
                &entry.detail,
                use_color,
            ));
        }
        out.push('\n');
    }
    if report.has_problems() {
        let hint = "Run `flowmux fix` to repair the rows tagged `fix`.\n";
        if use_color {
            // Bold red attention line so the prompt is visible at a
            // glance even on a long report.
            out.push_str(&format!("\x1b[1;31m{hint}\x1b[0m"));
        } else {
            out.push_str(hint);
        }
    }
    out
}

/// Format one row with a colour-aware, fixed-visual-width status badge.
/// Padding is computed against the *visible* label length so the
/// colour escape codes don't push the following columns out of
/// alignment.
fn format_status_row(status: &Status, name: &str, detail: &str, use_color: bool) -> String {
    const STATUS_WIDTH: usize = 8;
    const NAME_WIDTH: usize = 24;
    let label = status.label();
    let pad = " ".repeat(STATUS_WIDTH.saturating_sub(label.len()));
    let badge = colorize(label, status, use_color);
    format!("  {badge}{pad}  {name:NAME_WIDTH$}  {detail}\n")
}

pub fn render_json(report: &Report) -> Result<String> {
    let body = json!({
        "sections": report
            .sections
            .iter()
            .map(|s| json!({
                "title": s.title,
                "entries": s.entries.iter().map(|e| json!({
                    "name": e.name,
                    "status": e.status.label(),
                    "detail": e.detail,
                })).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "has_problems": report.has_problems(),
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

// ---- Fix -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FixOutcome {
    pub area: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct FixReport {
    pub outcomes: Vec<FixOutcome>,
}

impl FixReport {
    pub fn has_problems(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o.status, Status::Error))
    }
}

/// Re-install every flowmux-managed config the doctor would flag. We
/// always pass `force = true` for skills so a drifted file resyncs;
/// `hook_install::install` is already idempotent and silently skips
/// agents whose home dir is missing.
pub fn run_fix(home: &Path, codex_home: Option<&Path>, flowmux_bin: &str) -> FixReport {
    let mut outcomes = Vec::new();

    // Skills — only attempt agents whose home dir actually exists, so
    // a fresh box that hasn't run Claude / Codex yet doesn't get a
    // mystery `~/.claude/skills/...` tree it never asked for.
    for target in agent::Target::ALL {
        if !agent_is_installed(*target, home, codex_home) {
            outcomes.push(FixOutcome {
                area: format!("{} skill", target.slug()),
                status: Status::Warn,
                detail: "agent not installed; skipping".into(),
            });
            continue;
        }
        let path = target.resolved_install_path(home, codex_home);
        match agent::install_one(&path, target.payload(), true) {
            Ok(agent::InstallOutcome::Written) => outcomes.push(FixOutcome {
                area: format!("{} skill", target.slug()),
                status: Status::Ok,
                detail: format!("wrote {}", path.display()),
            }),
            Ok(agent::InstallOutcome::AlreadyUpToDate) => outcomes.push(FixOutcome {
                area: format!("{} skill", target.slug()),
                status: Status::Ok,
                detail: format!("already up-to-date: {}", path.display()),
            }),
            Err(e) => outcomes.push(FixOutcome {
                area: format!("{} skill", target.slug()),
                status: Status::Error,
                detail: e.to_string(),
            }),
        }
    }

    // Codex legacy sibling cleanup. Only emits a row when the legacy
    // file actually exists, so the fix output stays empty for fresh
    // installs. We never touch `$CODEX_HOME` itself or anything else.
    if let Some(outcome) = run_codex_legacy_cleanup(home, codex_home) {
        outcomes.push(outcome);
    }

    // Hooks — install_install handles the "agent home missing → skip"
    // case itself, so we surface its Skipped status verbatim.
    for target in hook_install::HookTarget::ALL {
        match hook_install::install(*target, flowmux_bin) {
            Ok(report) => {
                let (status, detail) = match report.status {
                    hook_install::HookInstallStatus::Installed
                        if report.touched_paths.is_empty() =>
                    {
                        (Status::Ok, "no changes".into())
                    }
                    hook_install::HookInstallStatus::Installed => (
                        Status::Ok,
                        report
                            .touched_paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    hook_install::HookInstallStatus::Skipped => {
                        (Status::Warn, "agent not installed; skipping".into())
                    }
                };
                outcomes.push(FixOutcome {
                    area: format!("{} hooks", target.slug()),
                    status,
                    detail,
                });
            }
            Err(e) => outcomes.push(FixOutcome {
                area: format!("{} hooks", target.slug()),
                status: Status::Error,
                detail: e.to_string(),
            }),
        }
    }

    FixReport { outcomes }
}

/// Remove the pre-skills Codex sibling file if it exists. Returns
/// `None` when there's nothing to clean up so callers can keep the
/// fix output quiet on fresh installs.
fn run_codex_legacy_cleanup(home: &Path, codex_home: Option<&Path>) -> Option<FixOutcome> {
    let path = codex_legacy_sibling_path(home, codex_home);
    if !path.exists() {
        return None;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Some(FixOutcome {
            area: "codex legacy skill".into(),
            status: Status::Ok,
            detail: format!("removed {}", path.display()),
        }),
        Err(e) => Some(FixOutcome {
            area: "codex legacy skill".into(),
            status: Status::Error,
            detail: format!("removing {}: {e}", path.display()),
        }),
    }
}

pub fn render_fix_text(report: &FixReport) -> String {
    render_fix_text_with_color(report, color_enabled())
}

fn render_fix_text_with_color(report: &FixReport, use_color: bool) -> String {
    let mut out = String::new();
    for o in &report.outcomes {
        out.push_str(&format_status_row(&o.status, &o.area, &o.detail, use_color));
    }
    out
}

pub fn render_fix_json(report: &FixReport) -> Result<String> {
    let body = json!({
        "outcomes": report
            .outcomes
            .iter()
            .map(|o| json!({
                "area": o.area,
                "status": o.status.label(),
                "detail": o.detail,
            }))
            .collect::<Vec<_>>(),
        "has_problems": report.has_problems(),
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fake_home() -> TempDir {
        TempDir::new().unwrap()
    }

    /// Doctor on a totally empty fake HOME: every skill row is Warn
    /// ("agent not installed") and every hook row is Warn (NoAgentHome).
    /// has_problems() must be false because nothing is actionable —
    /// the user simply hasn't installed the agents yet.
    #[test]
    fn doctor_on_empty_home_reports_warn_not_needsfix() {
        let home = fake_home();
        let report = collect_offline(home.path(), None);
        let agents = report
            .sections
            .iter()
            .find(|s| s.title == "AI agents")
            .unwrap();
        for entry in &agents.entries {
            assert!(
                matches!(entry.status, Status::Warn | Status::Info | Status::Ok),
                "{:?} {}: status={:?}",
                entry.name,
                entry.detail,
                entry.status
            );
        }
        assert!(!report.has_problems(), "empty HOME should not need fix");
    }

    /// Doctor when the user has Claude installed but no SKILL/hooks
    /// yet: rows flip from Warn to NeedsFix — that is what should
    /// drive `flowmux fix`.
    #[test]
    fn doctor_with_present_claude_home_marks_skill_and_hooks_as_needsfix() {
        let _lock = home_env_lock();
        let home = fake_home();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        // hook_install resolves the agent root via dirs::home_dir(),
        // which honours $HOME. Without this override the hook check
        // would inspect the real user's ~/.claude and pollute the
        // assertion.
        let _h = HomeOverride::set(home.path());
        let report = collect_offline(home.path(), None);
        let agents = report
            .sections
            .iter()
            .find(|s| s.title == "AI agents")
            .unwrap();
        let skill = agents
            .entries
            .iter()
            .find(|e| e.name == "claude-code skill")
            .unwrap();
        assert_eq!(skill.status, Status::NeedsFix, "{}", skill.detail);
        let hooks = agents
            .entries
            .iter()
            .find(|e| e.name == "claude-code hooks")
            .unwrap();
        assert_eq!(hooks.status, Status::NeedsFix, "{}", hooks.detail);
        assert!(report.has_problems());
    }

    /// `run_fix` after the doctor flagged Claude as NeedsFix should
    /// install both pieces and a follow-up doctor run should report
    /// the skill row as Ok. (Hooks rely on real `~/.claude` paths, so
    /// the assertion here is just that fix didn't error.)
    #[test]
    fn fix_then_doctor_clears_skill_row_for_present_agent() {
        let _lock = home_env_lock();
        let home = fake_home();
        fs::create_dir_all(home.path().join(".claude")).unwrap();

        // Point HOME at our temp dir for the duration of the test so
        // hook_install (which uses dirs::home_dir()) writes inside the
        // sandbox instead of the real user home.
        let _h = HomeOverride::set(home.path());

        let fix = run_fix(home.path(), None, "flowmux");
        let skill_outcomes: Vec<&FixOutcome> = fix
            .outcomes
            .iter()
            .filter(|o| o.area == "claude-code skill")
            .collect();
        assert_eq!(skill_outcomes.len(), 1);
        assert_eq!(skill_outcomes[0].status, Status::Ok);

        let report = collect_offline(home.path(), None);
        let skill = report
            .sections
            .iter()
            .find(|s| s.title == "AI agents")
            .unwrap()
            .entries
            .iter()
            .find(|e| e.name == "claude-code skill")
            .unwrap();
        assert_eq!(skill.status, Status::Ok, "{}", skill.detail);
    }

    #[test]
    fn render_text_lists_every_section_and_entry() {
        let home = fake_home();
        let report = collect_offline(home.path(), None);
        let txt = render_text(&report);
        assert!(txt.contains("AI agents"));
        assert!(txt.contains("Browser"));
        assert!(txt.contains("host browsers"));
    }

    #[test]
    fn render_text_without_color_emits_no_ansi_escapes() {
        let home = fake_home();
        let report = collect_offline(home.path(), None);
        let txt = render_text_with_color(&report, false);
        // Plain output must never contain ESC bytes; otherwise a user
        // who pipes `flowmux doctor` into a log file gets garbage.
        assert!(!txt.contains('\x1b'), "found escape in: {txt:?}");
    }

    #[test]
    fn render_text_with_color_wraps_status_badges_in_sgr() {
        let report = Report {
            sections: vec![Section {
                title: "AI agents".into(),
                entries: vec![
                    Entry {
                        name: "claude-code skill".into(),
                        status: Status::Ok,
                        detail: "installed".into(),
                    },
                    Entry {
                        name: "claude-code hooks".into(),
                        status: Status::NeedsFix,
                        detail: "missing".into(),
                    },
                ],
            }],
        };
        let txt = render_text_with_color(&report, true);
        // Green ok = 32, red fix = 31.
        assert!(txt.contains("\x1b[32mok\x1b[0m"), "missing green ok: {txt}");
        assert!(txt.contains("\x1b[31mfix\x1b[0m"), "missing red fix: {txt}");
    }

    #[test]
    fn status_row_padding_keeps_columns_aligned_regardless_of_color() {
        // Visible width must stay 8 chars wide for the badge and 24
        // for the name. We compare lengths without ANSI escapes.
        let plain = format_status_row(&Status::Ok, "host browsers", "chrome", false);
        let coloured = format_status_row(&Status::Ok, "host browsers", "chrome", true);
        let strip = |s: &str| -> String {
            // Tiny ad-hoc SGR stripper — enough for `\x1b[<digits>m`.
            let mut out = String::new();
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\x1b' && chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert_eq!(plain, strip(&coloured));
    }

    #[test]
    fn render_json_round_trips() {
        let home = fake_home();
        let report = collect_offline(home.path(), None);
        let s = render_json(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["sections"].is_array());
        assert!(v["has_problems"].is_boolean());
    }

    /// Guard against an installer that flips a row to Ok just because
    /// `flowmux fix` wrote *something*. Drift means content differs —
    /// fix must re-sync, not paper over.
    #[test]
    fn fix_resyncs_drifted_skill_files_with_force() {
        let _lock = home_env_lock();
        let home = fake_home();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        let _h = HomeOverride::set(home.path());

        // Pre-seed a stale SKILL.md.
        let path = agent::Target::ClaudeCode.resolved_install_path(home.path(), None);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old payload").unwrap();

        let pre = collect_offline(home.path(), None);
        let row = pre
            .sections
            .iter()
            .find(|s| s.title == "AI agents")
            .unwrap()
            .entries
            .iter()
            .find(|e| e.name == "claude-code skill")
            .unwrap();
        assert_eq!(row.status, Status::NeedsFix, "{}", row.detail);

        let _ = run_fix(home.path(), None, "flowmux");
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, agent::Target::ClaudeCode.payload());
    }

    /// A user upgrading from a pre-skills flowmux build still has a
    /// `~/.codex/flowmux-browser.md` sibling file. Doctor surfaces it
    /// as NeedsFix so the user knows `flowmux fix` will tidy up.
    #[test]
    fn doctor_flags_codex_legacy_sibling_when_present() {
        let home = fake_home();
        let codex_home = home.path().join(".codex");
        fs::create_dir_all(&codex_home).unwrap();
        let legacy = codex_home.join("flowmux-browser.md");
        fs::write(&legacy, "legacy payload").unwrap();

        let report = collect_offline(home.path(), None);
        let row = report
            .sections
            .iter()
            .find(|s| s.title == "AI agents")
            .unwrap()
            .entries
            .iter()
            .find(|e| e.name == "codex legacy skill");
        let row = row.expect("legacy row should be present when the sibling file exists");
        assert_eq!(row.status, Status::NeedsFix, "{}", row.detail);
        assert!(row.detail.contains("flowmux-browser.md"));
    }

    /// Fresh installs (no legacy file) must not emit the legacy row —
    /// otherwise the report grows a permanent zombie entry for every
    /// new user.
    #[test]
    fn doctor_omits_legacy_row_when_sibling_absent() {
        let home = fake_home();
        // Make the codex home exist but without the legacy file, so
        // the codex skill row itself stays a real assertion target.
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        let report = collect_offline(home.path(), None);
        let any_legacy = report
            .sections
            .iter()
            .flat_map(|s| &s.entries)
            .any(|e| e.name == "codex legacy skill");
        assert!(!any_legacy, "legacy row leaked into a clean report");
    }

    /// `flowmux fix` must remove the legacy sibling file when it
    /// exists. A second fix run must stay silent (no zombie row).
    #[test]
    fn fix_removes_codex_legacy_sibling_then_is_idempotent() {
        let _lock = home_env_lock();
        let home = fake_home();
        let _h = HomeOverride::set(home.path());
        let codex_home = home.path().join(".codex");
        fs::create_dir_all(&codex_home).unwrap();
        let legacy = codex_home.join("flowmux-browser.md");
        fs::write(&legacy, "legacy payload").unwrap();

        let fix = run_fix(home.path(), None, "flowmux");
        let row = fix
            .outcomes
            .iter()
            .find(|o| o.area == "codex legacy skill")
            .expect("legacy cleanup row should be in the fix report");
        assert_eq!(row.status, Status::Ok, "{}", row.detail);
        assert!(!legacy.exists(), "legacy file should be gone after fix");

        // Re-run: now that the file is absent, no legacy row should
        // appear in either doctor or fix.
        let report = collect_offline(home.path(), None);
        assert!(!report
            .sections
            .iter()
            .flat_map(|s| &s.entries)
            .any(|e| e.name == "codex legacy skill"));
        let fix2 = run_fix(home.path(), None, "flowmux");
        assert!(!fix2
            .outcomes
            .iter()
            .any(|o| o.area == "codex legacy skill"));
    }

    /// Process-wide guard for tests that mutate $HOME. Restored on drop.
    struct HomeOverride {
        prev: Option<std::ffi::OsString>,
    }
    impl HomeOverride {
        fn set(p: &Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", p);
            Self { prev }
        }
    }
    impl Drop for HomeOverride {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Cargo runs tests in parallel within a single binary, and they
    /// share process-global env. Without this lock, two tests racing
    /// `set_var("HOME", …)` would clobber each other and produce
    /// flaky failures (one test reads the other's tempdir before its
    /// own override took hold).
    fn home_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}
