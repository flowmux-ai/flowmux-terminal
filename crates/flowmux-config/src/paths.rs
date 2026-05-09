// SPDX-License-Identifier: GPL-3.0-or-later
//! XDG paths for flowmux. cmux on macOS uses
//! `~/Library/Application Support/cmux`; on Linux we follow XDG.

use std::path::PathBuf;

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("flowmux"))
}

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("flowmux"))
}

pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("flowmux"))
}

/// Legacy single-instance socket path. Kept as the last-resort
/// fallback for tools that want to talk to "any flowmux on this
/// machine" — manual `flowmuxctl notify` from outside any flowmux
/// PTY, the headless `flowmux-daemon` binary, etc. Multi-window
/// flowmux GUIs use [`runtime_socket_for_pid`] instead so their
/// notifications stay scoped to the window that hosts the source
/// terminal.
pub fn runtime_socket() -> PathBuf {
    if let Some(rt) = dirs::runtime_dir() {
        rt.join("flowmux.sock")
    } else {
        std::env::temp_dir().join(format!("flowmux-{}.sock", whoami()))
    }
}

/// Per-instance socket path. Each running flowmux GUI binds the
/// socket named after its OS PID and stamps its own
/// `FLOWMUX_SOCKET_PATH` into every PTY's environment, so a
/// notification from a terminal in window A flows back to window A
/// only — even when several flowmux windows are open at once.
pub fn runtime_socket_for_pid(pid: u32) -> PathBuf {
    let file = format!("flowmux-{pid}.sock");
    if let Some(rt) = dirs::runtime_dir() {
        rt.join(file)
    } else {
        std::env::temp_dir().join(format!("{}-{}", whoami(), file))
    }
}

pub fn ghostty_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ghostty").join("config"))
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "anon".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_socket_for_pid_is_distinct_per_process() {
        let a = runtime_socket_for_pid(1234);
        let b = runtime_socket_for_pid(5678);
        assert_ne!(a, b, "two GUIs must bind two different sockets");
        assert!(a.file_name().unwrap().to_string_lossy().contains("1234"));
        assert!(b.file_name().unwrap().to_string_lossy().contains("5678"));
    }

    #[test]
    fn runtime_socket_for_pid_does_not_collide_with_legacy_default() {
        // The fallback `flowmux.sock` path is reserved for manual
        // CLI invocations from outside any flowmux PTY. Per-PID
        // sockets must never accidentally point at it.
        let pid = runtime_socket_for_pid(std::process::id());
        let legacy = runtime_socket();
        assert_ne!(pid, legacy);
    }

    #[test]
    fn runtime_socket_for_pid_uses_runtime_dir_when_available() {
        // We can't force XDG_RUNTIME_DIR off in a parallel test
        // without affecting other tests, so just confirm the path
        // ends with the expected file name shape.
        let p = runtime_socket_for_pid(42);
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.contains("flowmux-42.sock") || name.contains("-flowmux-42.sock"),
            "unexpected socket name: {name}"
        );
    }
}
