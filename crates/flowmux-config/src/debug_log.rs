// SPDX-License-Identifier: GPL-3.0-or-later
//! Append-only debug log for the notification pipeline.
//!
//! Writes to `$HOME/.cache/flowmux/notify-debug.log`. The path is
//! deliberately under `$HOME` so:
//!
//! * Flatpak builds (with `--filesystem=home` in finish-args) see the
//!   same on-disk file from both sides of the sandbox boundary —
//!   the in-sandbox daemon, the in-sandbox `flowmuxctl` and a
//!   host-spawned `flatpak run --command=flowmuxctl …` all append
//!   to a single timeline, so a single OpenCode completion attempt
//!   captures the full host-hook → daemon → GTK dispatch chain.
//! * Output survives backgrounded daemons, killed terminals, and
//!   `journalctl --user -t com.flowmux.App` tag mismatches.
//!
//! Each line is `RFC3339 timestamp + pid + process-name + message`.
//! The file is opened append-only on every call so concurrent
//! writers from different processes (daemon + ephemeral CLI) do not
//! truncate each other. Log writes never fail loudly — a notify hook
//! must not error just because the cache dir is read-only.
//!
//! Always-on by default so a single repro reveals all state. Set
//! `FLOWMUX_NOTIFY_DEBUG=0` to silence it on hosts where the I/O
//! cost is unwanted.

use std::io::Write;
use std::path::PathBuf;

/// Hard upper bound on the log file size (bytes). When reached we
/// truncate and start over so a long-running daemon does not fill
/// the cache with old chatter. 2 MiB ≈ tens of thousands of lines —
/// plenty for one debug session.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn log_path() -> Option<PathBuf> {
    crate::paths::host_visible_cache_dir().map(|d| d.join("notify-debug.log"))
}

fn enabled() -> bool {
    !matches!(
        std::env::var("FLOWMUX_NOTIFY_DEBUG").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Append one structured line. Never panics; failures are silent.
///
/// `component` is a short tag like `"cli/hook"`, `"daemon/notify"`,
/// `"gui/addnotification"` so the multi-process timeline reads
/// cleanly without parsing arg lists.
pub fn append(component: &str, message: &str) {
    if !enabled() {
        return;
    }
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = std::fs::write(&path, b"");
        }
    }
    let pid = std::process::id();
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".into());
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let line = format!("{ts}  pid={pid}  bin={bin:<14}  [{component}] {message}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Helper macro so call sites read as `notify_debug!("cli/hook", "connected {socket:?}")`.
#[macro_export]
macro_rules! notify_debug {
    ($component:expr, $($arg:tt)*) => {{
        $crate::debug_log::append($component, &format!($($arg)*));
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_file_under_home_cache() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_flag = std::env::var_os("FLOWMUX_NOTIFY_DEBUG");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("FLOWMUX_NOTIFY_DEBUG");

        append("test", "hello world");

        let path = log_path().unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("hello world"));
        assert!(body.contains("[test]"));

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Some(v) = prev_flag {
            std::env::set_var("FLOWMUX_NOTIFY_DEBUG", v);
        }
    }

    #[test]
    fn append_is_silent_when_disabled() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_flag = std::env::var_os("FLOWMUX_NOTIFY_DEBUG");
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("FLOWMUX_NOTIFY_DEBUG", "0");

        append("test", "should not appear");
        let path = log_path().unwrap();
        assert!(
            !path.exists(),
            "disabling FLOWMUX_NOTIFY_DEBUG must skip the write entirely"
        );

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_flag {
            Some(v) => std::env::set_var("FLOWMUX_NOTIFY_DEBUG", v),
            None => std::env::remove_var("FLOWMUX_NOTIFY_DEBUG"),
        }
    }

    #[test]
    fn append_truncates_when_file_exceeds_max() {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_flag = std::env::var_os("FLOWMUX_NOTIFY_DEBUG");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("FLOWMUX_NOTIFY_DEBUG");

        let path = log_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let big = vec![b'x'; (MAX_BYTES + 1024) as usize];
        std::fs::write(&path, &big).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > MAX_BYTES);

        append("test", "rolled");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("rolled"));
        assert!(
            (body.len() as u64) < MAX_BYTES,
            "post-truncation file must be small"
        );

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Some(v) = prev_flag {
            std::env::set_var("FLOWMUX_NOTIFY_DEBUG", v);
        }
    }
}
