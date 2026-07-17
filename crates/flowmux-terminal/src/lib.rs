// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless terminal support for flowmux: the PTY layer, agent env
//! injection, terminal input-mode parsing, and a shared color type. The GTK
//! layer renders with VTE, so this crate no longer carries a VT core.

use flowmux_core::{PaneId, SurfaceId, WorkspaceId};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Env vars flowmux injects into every PTY spawn so terminal-side agents
/// (claude, codex, opencode, …) can discover their own pane and the
/// daemon socket without explicit flags. Mirrors cmux's
/// `GhosttyTerminalView` env injection — we only swap the `CMUX_` prefix
/// for `FLOWMUX_`.
///
/// Variables produced:
/// * `FLOWMUX_PANE_ID` — leaf pane (split-tree node). Multiple tab
///   surfaces inside a single pane share this id.
/// * `FLOWMUX_SURFACE_ID` — the specific tab surface that owns this
///   PTY. Distinct per tab so a notification can later route back to
///   the correct tab even when the pane has many.
/// * `FLOWMUX_WORKSPACE_ID` / `FLOWMUX_TAB_ID` — same value, the
///   workspace this pane lives in.
/// * `FLOWMUX_SOCKET_PATH` — the daemon's Unix socket path.
/// * `FLOWMUX_BUNDLED_CLI_PATH` — only when the caller knows where the
///   `flowmux` binary lives (e.g. derived from `current_exe()` in app).
/// * `TERM` / `TERM_PROGRAM` / `TERM_PROGRAM_VERSION` / `COLORTERM` — terminal identity
///   for TUIs that otherwise inherit the launcher terminal's identity or probe
///   VTE as a generic xterm.
/// * `CLAUDE_CODE_NATIVE_CURSOR` — asks Claude Code to leave the terminal cursor
///   available for VTE's IMContext. Without this, Claude's custom cursor hides
///   VTE's inline Hangul preedit on the GTK4/VTE path even though committed
///   syllables still arrive.
pub fn agent_pty_env(
    pane: PaneId,
    surface: SurfaceId,
    workspace: WorkspaceId,
    socket: &Path,
    bundled_cli: Option<&Path>,
) -> Vec<(String, String)> {
    let pane_s = pane.to_string();
    let surface_s = surface.to_string();
    let workspace_s = workspace.to_string();
    let mut out = Vec::with_capacity(11);
    out.push(("FLOWMUX_PANE_ID".to_string(), pane_s));
    out.push(("FLOWMUX_SURFACE_ID".to_string(), surface_s));
    out.push(("FLOWMUX_WORKSPACE_ID".to_string(), workspace_s.clone()));
    out.push(("FLOWMUX_TAB_ID".to_string(), workspace_s));
    out.push((
        "FLOWMUX_SOCKET_PATH".to_string(),
        socket.display().to_string(),
    ));
    if let Some(p) = bundled_cli {
        out.push((
            "FLOWMUX_BUNDLED_CLI_PATH".to_string(),
            p.display().to_string(),
        ));
    }
    out.push(("TERM".to_string(), "xterm-256color".to_string()));
    out.push(("TERM_PROGRAM".to_string(), "vte-based".to_string()));
    out.push((
        "TERM_PROGRAM_VERSION".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    out.push(("COLORTERM".to_string(), "truecolor".to_string()));
    out.push(("CLAUDE_CODE_NATIVE_CURSOR".to_string(), "1".to_string()));
    out
}

/// Convenience: collapse `[(k, v)]` env pairs into the `KEY=VALUE`
/// strings expected by terminal spawn APIs.
pub fn env_to_kv_strings(env: &[(String, String)]) -> Vec<String> {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Confirm that an explicit shell can be executed. Paths are checked directly;
/// bare command names are resolved through `PATH`, matching `execvp(3)`.
pub fn validate_shell_command(shell: &str) -> std::io::Result<()> {
    let shell = shell.trim();
    if shell.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell command is empty",
        ));
    }

    let path = Path::new(shell);
    let found = if path.components().count() > 1 {
        is_executable_file(path)
    } else {
        std::env::var_os("PATH")
            .map(|path_var| {
                std::env::split_paths(&path_var)
                    .map(|dir| dir.join(shell))
                    .any(|candidate| is_executable_file(&candidate))
            })
            .unwrap_or(false)
    };

    if found {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("shell is not executable: {shell}"),
        ))
    }
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Locate the `flowmuxctl` helper binary so the GUI can wrap a shell
/// spawn with `flowmuxctl pty-tee -- <argv>` (the OSC-notification
/// snooper). The lookup mirrors the priority in
/// `flowmux::delegate_to_cli_if_needed` so a packaged install (debian
/// /usr/bin/flowmux + /usr/lib/flowmux/flowmuxctl, or flatpak) and a
/// dev-tree `cargo run` install both Just Work:
///
/// 1. Sibling of `current_exe()` (e.g. `~/.local/bin/flowmux` next to
///    `~/.local/bin/flowmuxctl`, or a Cargo `target/debug/` build).
/// 2. `<prefix>/lib/flowmux/flowmuxctl` derived two levels above
///    `current_exe()` (debian / flatpak layout where only `flowmux` is
///    on `PATH`).
/// 3. `flowmuxctl` on `PATH` as a last resort.
///
/// Returns `None` only when none of the above resolve — callers (the
/// terminal pane in particular) should fall back to spawning the
/// shell directly so the absence of OSC alarms degrades gracefully
/// instead of breaking the terminal entirely.
pub fn find_flowmuxctl() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("flowmuxctl");
            if cand.is_file() {
                return Some(cand);
            }
        }
        if let Some(prefix) = exe.parent().and_then(|p| p.parent()) {
            let cand = prefix.join("lib").join("flowmux").join("flowmuxctl");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("flowmuxctl");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

pub mod color;
pub mod key_modes;

/// PTY layer for the terminal backend (spawn child, read/write master, resize).
pub mod pty;

pub use color::Rgb;
pub use key_modes::TerminalInputModes;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_validation_accepts_paths_and_path_commands() {
        validate_shell_command("/bin/sh").expect("/bin/sh should be executable");
        validate_shell_command("sh").expect("sh should resolve through PATH");
    }

    #[test]
    fn shell_validation_rejects_empty_and_missing_paths() {
        assert_eq!(
            validate_shell_command("  ").unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_shell_command("/bin/flowmux-shell-does-not-exist")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
    use std::path::PathBuf;

    fn collect<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn agent_pty_env_includes_pane_surface_workspace_socket() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let ws = WorkspaceId::new();
        let socket = PathBuf::from("/run/user/1000/flowmux.sock");

        let env = agent_pty_env(pane, surface, ws, &socket, None);

        assert_eq!(
            collect(&env, "FLOWMUX_PANE_ID"),
            Some(pane.to_string().as_str())
        );
        assert_eq!(
            collect(&env, "FLOWMUX_SURFACE_ID"),
            Some(surface.to_string().as_str())
        );
        assert_eq!(
            collect(&env, "FLOWMUX_WORKSPACE_ID"),
            Some(ws.to_string().as_str())
        );
        assert_eq!(
            collect(&env, "FLOWMUX_TAB_ID"),
            Some(ws.to_string().as_str())
        );
        assert_eq!(
            collect(&env, "FLOWMUX_SOCKET_PATH"),
            Some("/run/user/1000/flowmux.sock")
        );
        assert!(collect(&env, "FLOWMUX_BUNDLED_CLI_PATH").is_none());
        assert_eq!(collect(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(collect(&env, "TERM_PROGRAM"), Some("vte-based"));
        assert_eq!(
            collect(&env, "TERM_PROGRAM_VERSION"),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(collect(&env, "COLORTERM"), Some("truecolor"));
        assert_eq!(collect(&env, "CLAUDE_CODE_NATIVE_CURSOR"), Some("1"));
    }

    #[test]
    fn agent_pty_env_pane_and_surface_can_differ() {
        // The previous flowmux build aliased pane = surface; that
        // collapsed multi-tab routing because tab A and tab B in the
        // same pane shared one env. Now they differ on purpose.
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let env = agent_pty_env(pane, surface, WorkspaceId::new(), Path::new("/x"), None);
        assert_ne!(
            collect(&env, "FLOWMUX_PANE_ID"),
            collect(&env, "FLOWMUX_SURFACE_ID"),
            "pane and surface env vars must carry distinct ids"
        );
    }

    #[test]
    fn agent_pty_env_includes_bundled_cli_when_provided() {
        let env = agent_pty_env(
            PaneId::new(),
            SurfaceId::new(),
            WorkspaceId::new(),
            Path::new("/sock"),
            Some(Path::new("/usr/local/bin/flowmux")),
        );
        assert_eq!(
            collect(&env, "FLOWMUX_BUNDLED_CLI_PATH"),
            Some("/usr/local/bin/flowmux")
        );
    }

    #[test]
    fn env_to_kv_strings_joins_pairs_with_equals() {
        let env = vec![
            ("A".into(), "1".into()),
            ("FLOWMUX_PANE_ID".into(), "abc".into()),
        ];
        let kv = env_to_kv_strings(&env);
        assert_eq!(
            kv,
            vec!["A=1".to_string(), "FLOWMUX_PANE_ID=abc".to_string()]
        );
    }

    /// Scenario: building the env passed to terminal spawn APIs.
    /// Verifies the full pipeline (`agent_pty_env` → `env_to_kv_strings`)
    /// produces a valid envv array.
    #[test]
    fn scenario_full_envv_array_is_well_formed_for_terminal_spawn() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let ws = WorkspaceId::new();
        let env = agent_pty_env(
            pane,
            surface,
            ws,
            Path::new("/run/user/1000/flowmux.sock"),
            Some(Path::new("/usr/local/bin/flowmux")),
        );
        let kv = env_to_kv_strings(&env);

        assert_eq!(kv.len(), 11);
        for entry in &kv {
            let eq = entry.find('=').expect("envv entry must have '='");
            let key = &entry[..eq];
            let val = &entry[eq + 1..];
            assert!(!key.is_empty(), "envv key must be non-empty");
            assert!(!val.is_empty(), "envv value must be non-empty");
        }

        let pane_kv = format!("FLOWMUX_PANE_ID={pane}");
        let surface_kv = format!("FLOWMUX_SURFACE_ID={surface}");
        assert!(kv.iter().any(|e| e == &pane_kv));
        assert!(kv.iter().any(|e| e == &surface_kv));
        assert!(kv.iter().any(|e| e == "TERM=xterm-256color"));
        assert!(kv.iter().any(|e| e == "TERM_PROGRAM=vte-based"));
        assert!(kv.iter().any(|e| e == "COLORTERM=truecolor"));
        assert!(kv.iter().any(|e| e == "CLAUDE_CODE_NATIVE_CURSOR=1"));
    }
}
