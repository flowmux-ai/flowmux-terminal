// SPDX-License-Identifier: GPL-3.0-or-later
//! XDG paths for flowmux. cmux on macOS uses
//! `~/Library/Application Support/cmux`; on Linux we follow XDG.
//!
//! **Flatpak note:** when flowmux runs inside the
//! `com.flowmux.App` sandbox, the runtime/cache paths returned here
//! switch to `$HOME/.cache/flowmux/…`. The default XDG runtime dir
//! (`/run/user/UID/`) is sandbox-private — a Unix-socket bound there
//! from inside the sandbox is invisible to the host, so the OpenCode
//! plugin's host-side `flatpak run --command=flowmuxctl …` could
//! never reach the daemon. `$HOME` is bind-mounted from the host
//! (manifest carries `--filesystem=home`), so a socket placed under
//! `$HOME/.cache/flowmux/` is reachable from both sides.

use std::path::PathBuf;

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn base_config_dir() -> Option<PathBuf> {
    env_dir("XDG_CONFIG_HOME").or_else(dirs::config_dir)
}

fn base_data_dir() -> Option<PathBuf> {
    env_dir("XDG_DATA_HOME").or_else(dirs::data_dir)
}

fn base_state_dir() -> Option<PathBuf> {
    env_dir("XDG_STATE_HOME")
        .or_else(dirs::state_dir)
        .or_else(base_data_dir)
}

fn base_runtime_dir() -> Option<PathBuf> {
    env_dir("FLOWMUX_RUNTIME_DIR")
        .or_else(|| env_dir("XDG_RUNTIME_DIR"))
        .or_else(dirs::runtime_dir)
}

pub fn config_dir() -> Option<PathBuf> {
    base_config_dir().map(|d| d.join("flowmux"))
}

pub fn data_dir() -> Option<PathBuf> {
    base_data_dir().map(|d| d.join("flowmux"))
}

/// Directory holding the agent wrapper shims (`claude`, `codex`, …)
/// installed by `flowmux fix`. Each shim exports `FLOWMUX_AGENT_PID=$$`
/// before `exec`ing the real agent binary, so the daemon's liveness
/// sweep can detect a hard-killed agent. The GUI prepends this directory
/// to a PTY's `PATH`; the installer writes the scripts here.
pub fn agent_shim_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("shims"))
}

pub fn state_dir() -> Option<PathBuf> {
    base_state_dir().map(|d| d.join("flowmux"))
}

/// True when the current process runs inside a Flatpak sandbox.
///
/// Detection mirrors the helper in `flowmux::ui::terminal_pane` —
/// `FLATPAK_ID` is exported into every sandbox process and
/// `/.flatpak-info` is unconditionally present at the sandbox root.
pub fn is_flatpak_sandbox() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists()
}

/// `$HOME/.cache/flowmux/` — host-visible cache dir used as the
/// runtime root for Flatpak builds. Returns `None` only when `$HOME`
/// is unset, which on Linux desktops never happens in practice.
pub fn host_visible_cache_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("flowmux"))
}

/// Single stable socket path the host-side OpenCode plugin can
/// connect to without knowing the daemon PID. Daemon refreshes this
/// pointer (symlink) at startup; CLI invocations with no
/// `FLOWMUX_SOCKET_PATH` env fall back to this path.
///
/// `FLOWMUX_RUNTIME_DIR` overrides only flowmux's socket directory.
/// It intentionally does not require changing `XDG_RUNTIME_DIR`,
/// which would hide compositor sockets such as WSLg's Wayland socket.
///
/// Outside Flatpak the legacy `flowmux.sock` under `$XDG_RUNTIME_DIR`
/// stays in use so non-sandbox installs keep their existing wire-up.
pub fn runtime_socket() -> PathBuf {
    if is_flatpak_sandbox() {
        if let Some(cache) = host_visible_cache_dir() {
            return cache.join("current.sock");
        }
    }
    if let Some(rt) = base_runtime_dir() {
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
///
/// In Flatpak the per-PID file moves under
/// `$HOME/.cache/flowmux/` so a host process invoked via
/// `flatpak run` can reach the same socket the in-sandbox daemon
/// bound.
pub fn runtime_socket_for_pid(pid: u32) -> PathBuf {
    let file = format!("flowmux-{pid}.sock");
    if is_flatpak_sandbox() {
        if let Some(cache) = host_visible_cache_dir() {
            return cache.join(file);
        }
    }
    if let Some(rt) = base_runtime_dir() {
        rt.join(file)
    } else {
        std::env::temp_dir().join(format!("{}-{}", whoami(), file))
    }
}

/// `$HOME/.config/opencode/` (or analogous) bypassing the
/// flatpak-private `XDG_CONFIG_HOME` so the hook installer drops
/// agent plugin files where the host-side agent actually reads them.
/// Outside Flatpak this collapses back to the default XDG resolver.
pub fn host_config_dir_for(agent_subdir: &str) -> Option<PathBuf> {
    if is_flatpak_sandbox() {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join(agent_subdir))
    } else {
        base_config_dir().map(|d| d.join(agent_subdir))
    }
}

pub fn ghostty_config_path() -> Option<PathBuf> {
    base_config_dir().map(|d| d.join("ghostty").join("config"))
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

    #[test]
    fn flowmux_runtime_dir_overrides_xdg_runtime_dir_for_sockets() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let prev_flowmux = std::env::var_os("FLOWMUX_RUNTIME_DIR");
        let prev_xdg = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("FLOWMUX_RUNTIME_DIR", "/tmp/flowmux-runtime-test");
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/xdg-runtime-test");

        assert_eq!(
            runtime_socket(),
            PathBuf::from("/tmp/flowmux-runtime-test/flowmux.sock")
        );
        assert_eq!(
            runtime_socket_for_pid(4242),
            PathBuf::from("/tmp/flowmux-runtime-test/flowmux-4242.sock")
        );

        match prev_flowmux {
            Some(v) => std::env::set_var("FLOWMUX_RUNTIME_DIR", v),
            None => std::env::remove_var("FLOWMUX_RUNTIME_DIR"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn state_dir_is_available_on_non_xdg_platforms() {
        let dir = state_dir().expect("state dir should fall back to data dir when needed");
        assert!(dir.ends_with("flowmux"));
    }

    /// host_visible_cache_dir derives from $HOME directly so a Flatpak
    /// process sees the same on-disk bytes as the host. The literal
    /// segment ".cache/flowmux" is load-bearing: the manifest only
    /// passes --filesystem=home, so anywhere under $HOME stays
    /// visible from both sides, but XDG_CACHE_HOME (which dirs::cache_dir
    /// follows) points at ~/.var/app/<id>/cache/ inside the sandbox
    /// and would not match.
    #[test]
    fn host_visible_cache_dir_anchors_under_home() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", "/home/flowmuxtest");
        let dir = host_visible_cache_dir().unwrap();
        assert_eq!(dir, PathBuf::from("/home/flowmuxtest/.cache/flowmux"));
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn host_config_dir_for_returns_xdg_when_not_sandbox() {
        // FLATPAK_ID unset → fall back to the dirs crate (XDG_CONFIG_HOME
        // or ~/.config). We only assert the trailing segment because the
        // root depends on the runner's env.
        let _g = crate::test_env::env_lock().lock().unwrap();
        let prev_id = std::env::var_os("FLATPAK_ID");
        std::env::remove_var("FLATPAK_ID");
        let dir = host_config_dir_for("opencode").unwrap();
        assert!(dir.ends_with("opencode"));
        if let Some(v) = prev_id {
            std::env::set_var("FLATPAK_ID", v);
        }
    }

    #[test]
    fn host_config_dir_for_bypasses_xdg_when_sandbox() {
        // Force the sandbox branch and confirm we anchor under $HOME
        // rather than honour XDG_CONFIG_HOME (which would otherwise
        // route writes into the flatpak-private ~/.var/app/.../config/).
        let _g = crate::test_env::env_lock().lock().unwrap();
        let prev_id = std::env::var_os("FLATPAK_ID");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("FLATPAK_ID", "com.flowmux.App");
        std::env::set_var(
            "XDG_CONFIG_HOME",
            "/home/junsu/.var/app/com.flowmux.App/config",
        );
        std::env::set_var("HOME", "/home/junsu");
        let dir = host_config_dir_for("opencode").unwrap();
        assert_eq!(dir, PathBuf::from("/home/junsu/.config/opencode"));
        match prev_id {
            Some(v) => std::env::set_var("FLATPAK_ID", v),
            None => std::env::remove_var("FLATPAK_ID"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn runtime_socket_for_pid_moves_under_home_cache_in_sandbox() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let prev_id = std::env::var_os("FLATPAK_ID");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("FLATPAK_ID", "com.flowmux.App");
        std::env::set_var("HOME", "/home/junsu");
        let p = runtime_socket_for_pid(4242);
        assert_eq!(
            p,
            PathBuf::from("/home/junsu/.cache/flowmux/flowmux-4242.sock")
        );
        match prev_id {
            Some(v) => std::env::set_var("FLATPAK_ID", v),
            None => std::env::remove_var("FLATPAK_ID"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn runtime_socket_returns_stable_pointer_in_sandbox() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let prev_id = std::env::var_os("FLATPAK_ID");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("FLATPAK_ID", "com.flowmux.App");
        std::env::set_var("HOME", "/home/junsu");
        let p = runtime_socket();
        assert_eq!(p, PathBuf::from("/home/junsu/.cache/flowmux/current.sock"));
        match prev_id {
            Some(v) => std::env::set_var("FLATPAK_ID", v),
            None => std::env::remove_var("FLATPAK_ID"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
