// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux agent install / doctor / uninstall` — make the
//! flowmux-browser SKILL discoverable to Claude Code, OpenCode, and
//! Codex CLI installed locally for the current user.
//!
//! Strategy: every supported agent has a documented user-level skill
//! / AGENTS file location under `$HOME`. We mirror our embedded
//! `SKILL.md` (and a small `flowmux-browser.md` snippet for Codex) into
//! those paths idempotently. `doctor` walks the same paths and
//! reports presence / content drift so the user can verify a fresh
//! install or update without leaving the terminal.
//!
//! Embedded payload comes straight from this repo via `include_str!`,
//! so a `cargo install --path crates/flowmux-cli --force` rebuild
//! always ships the latest workflow text — no separate package /
//! resource step needed.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// SKILL body embedded into the binary at compile time. Lives at
/// `<repo>/.claude/skills/flowmux-browser/SKILL.md`.
pub const SKILL_BODY: &str = include_str!("../../../.claude/skills/flowmux-browser/SKILL.md");

/// AGENTS.md body — used as the Codex-style instruction snippet.
pub const AGENTS_BODY: &str = include_str!("../../../AGENTS.md");

/// One agent we know how to wire up. The `Target` enum stays small —
/// adding a new agent means adding a variant + its
/// `resolved_install_path` arm + a doctor entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// `~/.claude/skills/flowmux-browser/SKILL.md`. Claude Code loads
    /// every directory under `~/.claude/skills/<name>/` automatically.
    ClaudeCode,
    /// `~/.config/opencode/skills/flowmux-browser/SKILL.md`. OpenCode
    /// follows the same skill convention as Claude Code.
    OpenCode,
    /// `~/.codex/flowmux-browser.md`. Codex CLI does not have a skills
    /// concept — it loads `AGENTS.md` files. We drop a sibling file
    /// the user can import (`@flowmux-browser`) into their own
    /// `~/.codex/AGENTS.md` and never overwrite the user's own file.
    Codex,
}

impl Target {
    pub const ALL: &'static [Target] = &[Target::ClaudeCode, Target::OpenCode, Target::Codex];

    pub fn slug(self) -> &'static str {
        match self {
            Target::ClaudeCode => "claude-code",
            Target::OpenCode => "opencode",
            Target::Codex => "codex",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.slug() == s)
    }

    /// Body that gets written to disk. Claude Code / OpenCode use the
    /// `SKILL.md` frontmatter+body shape; Codex receives the same
    /// content under a different filename (no skill concept).
    pub fn payload(self) -> &'static str {
        match self {
            Target::ClaudeCode | Target::OpenCode => SKILL_BODY,
            Target::Codex => SKILL_BODY,
        }
    }

    /// Path the install writes to. `home` is the resolved
    /// `$HOME` (callers usually pass `dirs::home_dir()`). For Codex
    /// we honour `$CODEX_HOME` if set, matching the upstream
    /// convention.
    pub fn resolved_install_path(self, home: &Path, codex_home: Option<&Path>) -> PathBuf {
        match self {
            Target::ClaudeCode => home
                .join(".claude")
                .join("skills")
                .join("flowmux-browser")
                .join("SKILL.md"),
            Target::OpenCode => home
                .join(".config")
                .join("opencode")
                .join("skills")
                .join("flowmux-browser")
                .join("SKILL.md"),
            Target::Codex => codex_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".codex"))
                .join("flowmux-browser.md"),
        }
    }
}

/// Per-target outcome of a `doctor` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorEntry {
    pub target: Target,
    pub path: PathBuf,
    pub status: DoctorStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorStatus {
    /// File is present and its content matches the embedded payload.
    Ok,
    /// File is present but its content drifted (older flowmux skill or
    /// hand-edited). `flowmux agent install --force` re-syncs it.
    Drift,
    /// File is missing. `flowmux agent install` creates it.
    Missing,
    /// Filesystem error while reading the file (permission etc.).
    Error(String),
}

impl DoctorStatus {
    pub fn label(&self) -> &'static str {
        match self {
            DoctorStatus::Ok => "ok",
            DoctorStatus::Drift => "drift",
            DoctorStatus::Missing => "missing",
            DoctorStatus::Error(_) => "error",
        }
    }
}

/// Idempotent install. Writes `payload` to `path` (creating parent
/// dirs). If `path` already exists with the same content, this is a
/// no-op. If it exists with different content, `force = true`
/// overwrites; `force = false` returns an error.
pub fn install_one(path: &Path, payload: &str, force: bool) -> Result<InstallOutcome> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        if existing == payload {
            return Ok(InstallOutcome::AlreadyUpToDate);
        }
        if !force {
            return Err(anyhow!(
                "{} exists with different content (run `flowmux agent install --force` to overwrite)",
                path.display()
            ));
        }
    }
    fs::write(path, payload).with_context(|| format!("writing {}", path.display()))?;
    Ok(InstallOutcome::Written)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Written,
    AlreadyUpToDate,
}

/// Compare embedded payload against on-disk file.
pub fn doctor_one(path: &Path, payload: &str) -> DoctorStatus {
    if !path.exists() {
        return DoctorStatus::Missing;
    }
    match fs::read_to_string(path) {
        Ok(s) if s == payload => DoctorStatus::Ok,
        Ok(_) => DoctorStatus::Drift,
        Err(e) => DoctorStatus::Error(e.to_string()),
    }
}

/// Resolve the home directory + Codex home for a real run. Tests pass
/// fakes through the lower-level helpers above.
pub fn resolved_home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("HOME is not set; cannot locate user-level dirs"))
}

pub fn resolved_codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from)
}

/// Install for every requested target. Returns one outcome per
/// target; the first error short-circuits.
pub fn install_all(
    targets: &[Target],
    home: &Path,
    codex_home: Option<&Path>,
    force: bool,
) -> Result<Vec<(Target, PathBuf, InstallOutcome)>> {
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        let path = t.resolved_install_path(home, codex_home);
        let outcome = install_one(&path, t.payload(), force)?;
        out.push((*t, path, outcome));
    }
    Ok(out)
}

/// Doctor report for every requested target.
pub fn doctor_all(
    targets: &[Target],
    home: &Path,
    codex_home: Option<&Path>,
) -> Vec<DoctorEntry> {
    targets
        .iter()
        .map(|t| {
            let path = t.resolved_install_path(home, codex_home);
            let status = doctor_one(&path, t.payload());
            DoctorEntry {
                target: *t,
                path,
                status,
            }
        })
        .collect()
}

/// Idempotent uninstall. Removes the file (and the empty
/// `flowmux-browser` parent directory for Claude/OpenCode skill
/// layouts, but never the agent's top-level dir).
pub fn uninstall_one(path: &Path) -> Result<UninstallOutcome> {
    if !path.exists() {
        return Ok(UninstallOutcome::AlreadyAbsent);
    }
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    if let Some(parent) = path.parent() {
        if parent.file_name().and_then(|s| s.to_str()) == Some("flowmux-browser") {
            // Empty the dir if we just removed the only file inside.
            if fs::read_dir(parent).map(|mut d| d.next().is_none()).unwrap_or(false) {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    Ok(UninstallOutcome::Removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    Removed,
    AlreadyAbsent,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn install_writes_when_missing() {
        let home = fake_home();
        let path = Target::ClaudeCode.resolved_install_path(home.path(), None);
        let outcome = install_one(&path, "hello", false).unwrap();
        assert_eq!(outcome, InstallOutcome::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn install_is_noop_when_content_matches() {
        let home = fake_home();
        let path = Target::ClaudeCode.resolved_install_path(home.path(), None);
        install_one(&path, "hello", false).unwrap();
        let outcome = install_one(&path, "hello", false).unwrap();
        assert_eq!(outcome, InstallOutcome::AlreadyUpToDate);
    }

    #[test]
    fn install_refuses_overwrite_without_force() {
        let home = fake_home();
        let path = Target::ClaudeCode.resolved_install_path(home.path(), None);
        install_one(&path, "v1", false).unwrap();
        let err = install_one(&path, "v2", false).unwrap_err();
        assert!(err.to_string().contains("--force"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1");
    }

    #[test]
    fn install_overwrites_with_force() {
        let home = fake_home();
        let path = Target::ClaudeCode.resolved_install_path(home.path(), None);
        install_one(&path, "v1", false).unwrap();
        let outcome = install_one(&path, "v2", true).unwrap();
        assert_eq!(outcome, InstallOutcome::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
    }

    #[test]
    fn doctor_reports_missing_then_ok_then_drift() {
        let home = fake_home();
        let path = Target::ClaudeCode.resolved_install_path(home.path(), None);

        assert_eq!(doctor_one(&path, "v1"), DoctorStatus::Missing);

        install_one(&path, "v1", false).unwrap();
        assert_eq!(doctor_one(&path, "v1"), DoctorStatus::Ok);

        // Hand-edit on disk → drift.
        fs::write(&path, "edited").unwrap();
        assert_eq!(doctor_one(&path, "v1"), DoctorStatus::Drift);
    }

    #[test]
    fn target_path_layout_per_agent() {
        let home = fake_home();
        let claude = Target::ClaudeCode.resolved_install_path(home.path(), None);
        let opencode = Target::OpenCode.resolved_install_path(home.path(), None);
        let codex = Target::Codex.resolved_install_path(home.path(), None);

        assert!(claude.ends_with(".claude/skills/flowmux-browser/SKILL.md"));
        assert!(opencode.ends_with(".config/opencode/skills/flowmux-browser/SKILL.md"));
        assert!(codex.ends_with(".codex/flowmux-browser.md"));
    }

    #[test]
    fn codex_home_env_overrides_default_dir() {
        let home = fake_home();
        let codex_home = home.path().join("custom-codex");
        let path = Target::Codex.resolved_install_path(home.path(), Some(&codex_home));
        assert_eq!(path, codex_home.join("flowmux-browser.md"));
    }

    #[test]
    fn target_from_slug_round_trip() {
        for t in Target::ALL {
            assert_eq!(Target::from_slug(t.slug()), Some(*t));
        }
        assert_eq!(Target::from_slug("nonexistent"), None);
    }

    #[test]
    fn install_all_handles_every_target() {
        let home = fake_home();
        let outcomes = install_all(Target::ALL, home.path(), None, false).unwrap();
        assert_eq!(outcomes.len(), Target::ALL.len());
        for (_t, path, outcome) in &outcomes {
            assert_eq!(*outcome, InstallOutcome::Written);
            assert!(path.exists());
        }
    }

    #[test]
    fn doctor_all_reports_one_entry_per_target_after_partial_install() {
        let home = fake_home();
        // Install Claude only.
        install_one(
            &Target::ClaudeCode.resolved_install_path(home.path(), None),
            Target::ClaudeCode.payload(),
            false,
        )
        .unwrap();

        let report = doctor_all(Target::ALL, home.path(), None);
        assert_eq!(report.len(), Target::ALL.len());

        let by_target: std::collections::HashMap<_, _> =
            report.iter().map(|e| (e.target, &e.status)).collect();
        assert_eq!(by_target[&Target::ClaudeCode], &DoctorStatus::Ok);
        assert_eq!(by_target[&Target::OpenCode], &DoctorStatus::Missing);
        assert_eq!(by_target[&Target::Codex], &DoctorStatus::Missing);
    }

    #[test]
    fn uninstall_removes_then_reports_absent() {
        let home = fake_home();
        let path = Target::ClaudeCode.resolved_install_path(home.path(), None);
        install_one(&path, "v1", false).unwrap();
        assert_eq!(uninstall_one(&path).unwrap(), UninstallOutcome::Removed);
        assert!(!path.exists());
        // The empty `flowmux-browser/` parent should be cleaned up too.
        assert!(!path.parent().unwrap().exists());

        // Second uninstall is idempotent.
        assert_eq!(uninstall_one(&path).unwrap(), UninstallOutcome::AlreadyAbsent);
    }

    #[test]
    fn embedded_payload_is_not_empty() {
        // Sanity: include_str! resolved to actual SKILL/AGENTS bodies.
        assert!(SKILL_BODY.contains("flowmux"));
        assert!(SKILL_BODY.contains("snapshot"));
        assert!(AGENTS_BODY.contains("AGENTS.md"));
    }

    /// Scenario: full install → doctor reports OK → simulate a flowmux
    /// upgrade that ships an updated SKILL → doctor reports Drift on
    /// every target → `--force` install brings them all back to OK.
    #[test]
    fn scenario_upgrade_drift_then_reinstall_with_force() {
        let home = fake_home();

        // 1. Initial install with embedded SKILL.
        install_all(Target::ALL, home.path(), None, false).unwrap();
        let report = doctor_all(Target::ALL, home.path(), None);
        assert!(report.iter().all(|e| e.status == DoctorStatus::Ok));

        // 2. Pretend the embedded SKILL changed by writing an older
        //    body to every install path.
        for t in Target::ALL {
            let p = t.resolved_install_path(home.path(), None);
            fs::write(p, "old payload").unwrap();
        }
        let report = doctor_all(Target::ALL, home.path(), None);
        assert!(report.iter().all(|e| e.status == DoctorStatus::Drift));

        // 3. Re-install with --force restores parity.
        install_all(Target::ALL, home.path(), None, true).unwrap();
        let report = doctor_all(Target::ALL, home.path(), None);
        assert!(report.iter().all(|e| e.status == DoctorStatus::Ok));
    }
}
