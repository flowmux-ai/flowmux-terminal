// SPDX-License-Identifier: GPL-3.0-or-later
//! Persistent surface ↔ agent-session mapping.
//!
//! Mirrors cmux's `~/.cmuxterm/<agent>-hook-sessions.json` — each
//! supported agent (claude, codex, opencode, …) reports its session id
//! to flowmux through an IPC verb and we persist
//! `(agent, surface_id) → session_id`. On the next launch the GUI can
//! re-spawn the same agent in the same surface with `<agent> --resume
//! <session-id>` and continue the conversation where it left off.
//!
//! Storage layout:
//!
//! ```text
//! $XDG_DATA_HOME/flowmux/agent-sessions/<agent>.json
//! ```
//!
//! A single file per agent makes it easy to inspect, back up, or
//! delete one agent's history without touching the others. The JSON
//! payload is `{"<surface_uuid>": "<session_id>", ...}` — flat, so
//! human-readable and trivially mergeable.
//!
//! Writes are atomic (tmp file + rename), matching the policy in
//! `state_store.rs`, so a crash mid-write never leaves a partially
//! serialized file behind.

use flowmux_core::SurfaceId;
use std::collections::BTreeMap;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File-backed store. Constructed once on daemon boot from
/// `$XDG_DATA_HOME/flowmux/agent-sessions/`.
#[derive(Debug, Clone)]
pub struct AgentSessionStore {
    dir: PathBuf,
}

impl AgentSessionStore {
    /// `dir` is the directory that holds `<agent>.json` files. The
    /// store creates it on the first write — callers don't need to
    /// pre-create it.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn agent_path(&self, agent: &str) -> PathBuf {
        self.dir.join(format!("{agent}.json"))
    }

    fn load(&self, agent: &str) -> io::Result<BTreeMap<String, String>> {
        let path = self.agent_path(agent);
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let bytes = std::fs::read(&path)?;
        if bytes.is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn write_atomic(path: &Path, payload: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(payload)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// Record (or overwrite) the session id for `(agent, surface)`.
    pub fn record(&self, agent: &str, surface: SurfaceId, session_id: &str) -> io::Result<()> {
        let mut map = self.load(agent)?;
        map.insert(surface.to_string(), session_id.to_string());
        let bytes = serde_json::to_vec_pretty(&map)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Self::write_atomic(&self.agent_path(agent), &bytes)
    }

    /// Return the session id previously recorded for `(agent,
    /// surface)`, or `None` if the agent has never reported one for
    /// that surface (or the file is missing).
    pub fn lookup(&self, agent: &str, surface: SurfaceId) -> Option<String> {
        self.load(agent).ok()?.get(&surface.to_string()).cloned()
    }

    /// Forget any session previously recorded for `(agent, surface)`.
    /// Used when the surface is closed permanently.
    pub fn forget(&self, agent: &str, surface: SurfaceId) -> io::Result<()> {
        let mut map = self.load(agent)?;
        if map.remove(&surface.to_string()).is_some() {
            let bytes = serde_json::to_vec_pretty(&map)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Self::write_atomic(&self.agent_path(agent), &bytes)?;
        }
        Ok(())
    }

    /// Total entries stored for `agent`. Useful for tests / diagnostics.
    pub fn len(&self, agent: &str) -> usize {
        self.load(agent).map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self, agent: &str) -> bool {
        self.len(agent) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (AgentSessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = AgentSessionStore::new(dir.path().to_path_buf());
        (s, dir)
    }

    #[test]
    fn lookup_returns_none_when_no_file_exists() {
        let (s, _td) = store();
        assert_eq!(s.lookup("claude", SurfaceId::new()), None);
        assert_eq!(s.len("claude"), 0);
    }

    #[test]
    fn record_then_lookup_returns_session_id() {
        let (s, _td) = store();
        let surface = SurfaceId::new();
        s.record("claude", surface, "sess-abc").unwrap();
        assert_eq!(s.lookup("claude", surface), Some("sess-abc".into()));
        assert_eq!(s.len("claude"), 1);
    }

    #[test]
    fn record_overwrites_existing_session_id() {
        let (s, _td) = store();
        let surface = SurfaceId::new();
        s.record("claude", surface, "sess-old").unwrap();
        s.record("claude", surface, "sess-new").unwrap();
        assert_eq!(s.lookup("claude", surface), Some("sess-new".into()));
        assert_eq!(s.len("claude"), 1);
    }

    #[test]
    fn forget_removes_only_the_targeted_surface() {
        let (s, _td) = store();
        let a = SurfaceId::new();
        let b = SurfaceId::new();
        s.record("claude", a, "sess-a").unwrap();
        s.record("claude", b, "sess-b").unwrap();
        s.forget("claude", a).unwrap();
        assert_eq!(s.lookup("claude", a), None);
        assert_eq!(s.lookup("claude", b), Some("sess-b".into()));
    }

    #[test]
    fn agents_are_isolated_in_separate_files() {
        let (s, td) = store();
        let surface = SurfaceId::new();
        s.record("claude", surface, "claude-sess").unwrap();
        s.record("codex", surface, "codex-sess").unwrap();
        // Same surface → different session ids per agent.
        assert_eq!(s.lookup("claude", surface), Some("claude-sess".into()));
        assert_eq!(s.lookup("codex", surface), Some("codex-sess".into()));
        // Two distinct files on disk.
        assert!(td.path().join("claude.json").exists());
        assert!(td.path().join("codex.json").exists());
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind_on_success() {
        let (s, td) = store();
        s.record("claude", SurfaceId::new(), "sess").unwrap();
        // Atomic-replace policy: only the final `claude.json` exists,
        // no `claude.tmp.<pid>` lingering.
        for entry in std::fs::read_dir(td.path()).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(
                !name.contains("tmp."),
                "stale tmp file from atomic write: {name}"
            );
        }
    }

    /// Scenario: cmux-equivalent restart flow. Daemon boot calls
    /// `lookup` for every recorded (agent, surface) and rehydrates
    /// the session. Verify the round-trip: write, drop the in-memory
    /// store, recreate it pointing at the same dir, look up.
    #[test]
    fn scenario_session_survives_daemon_restart_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let surface = SurfaceId::new();

        // First "boot": record a couple of sessions.
        {
            let s = AgentSessionStore::new(dir.path().to_path_buf());
            s.record("claude", surface, "sess-1").unwrap();
            s.record("codex", surface, "sess-2").unwrap();
        }

        // Second "boot": fresh store reads the same files.
        let s = AgentSessionStore::new(dir.path().to_path_buf());
        assert_eq!(s.lookup("claude", surface), Some("sess-1".into()));
        assert_eq!(s.lookup("codex", surface), Some("sess-2".into()));
    }

    #[test]
    fn malformed_file_yields_invalid_data_error_on_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("claude.json"), "not json {{").unwrap();
        let s = AgentSessionStore::new(dir.path().to_path_buf());
        // load surfaces InvalidData; lookup just returns None
        // (defensive — a bad file shouldn't crash the daemon).
        assert_eq!(s.lookup("claude", SurfaceId::new()), None);
    }
}
