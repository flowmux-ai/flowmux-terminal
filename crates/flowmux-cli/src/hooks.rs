// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmux hooks <agent> <event>` — handlers invoked by Claude Code,
//! OpenCode, and Codex CLI when an agent crosses a lifecycle boundary
//! (Stop / Notification / SessionStart / …).
//!
//! Each handler reads a small JSON payload from stdin (the agent's hook
//! input format), distills it into a one-line summary, and forwards it
//! to the daemon via `Request::Notify`. The daemon's GTK side then
//! shows the system toast and adds it to the bell popover with click
//! routing back to the originating pane.
//!
//! The handler stays fast: hook timeouts in agent settings are 5–10s,
//! so we resolve the workspace eagerly via the daemon (already done by
//! `Request::Notify`) and otherwise do minimal work.

use anyhow::Context;
use flowmux_core::{NotificationLevel, PaneId};
use flowmux_ipc::{
    client::Client,
    protocol::{Request, Response},
};
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;

/// Subset of an agent hook payload that flowmux cares about. Reused
/// across Claude/Codex/OpenCode because their JSON shapes overlap on
/// the fields we surface (event name, optional message, optional
/// last assistant text). Unknown fields are ignored so a new agent
/// release doesn't break us.
#[derive(Debug, Default, Deserialize)]
pub struct ClaudeHookInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    /// Set when `Notification` fires for permission/info popups.
    #[serde(default)]
    pub message: Option<String>,
    /// `Stop` payload sometimes carries the trailing assistant text
    /// (Claude). Codex's `notify` payload calls the same thing
    /// `last-assistant-message`. We accept both spellings.
    #[serde(default, alias = "last-assistant-message")]
    pub last_assistant_message: Option<String>,
}

/// Read up to 1 MiB of JSON from stdin and parse as a hook payload.
/// Empty stdin / parse failures degrade to a default payload so the
/// user still gets a generic toast even when the hook glue is broken.
pub fn read_claude_hook_input() -> ClaudeHookInput {
    let mut buf = String::new();
    let _ = std::io::stdin()
        .lock()
        .take(1024 * 1024)
        .read_to_string(&mut buf);
    if buf.trim().is_empty() {
        return ClaudeHookInput::default();
    }
    serde_json::from_str(&buf).unwrap_or_default()
}

/// Codex's `notify` config spawns the program with the JSON event
/// payload as the LAST positional argument (Claude's hook system uses
/// stdin instead). Try the positional path first; fall back to stdin
/// if `extra_args` is empty.
pub fn read_codex_hook_input(extra_args: &[String]) -> ClaudeHookInput {
    if let Some(payload) = extra_args.last() {
        if !payload.trim().is_empty() {
            if let Ok(parsed) = serde_json::from_str::<ClaudeHookInput>(payload) {
                return parsed;
            }
        }
    }
    read_claude_hook_input()
}

/// Resolve `FLOWMUX_PANE_ID` from the env (set by `flowmux-app` at PTY
/// spawn time). Mirrors `crate::pane_from_env` so the hooks module
/// stays self-contained.
pub fn pane_from_env() -> Option<PaneId> {
    std::env::var("FLOWMUX_PANE_ID")
        .ok()
        .as_deref()
        .and_then(|s| PaneId::from_str(s).ok())
}

/// Trim a body string down to a single notification-friendly line.
pub fn shorten_body(s: &str, max: usize) -> String {
    let one_line: String = s
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let truncated: String = one_line.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// Build a `Request::Notify` for an agent stop event.
pub fn build_stop_notify(agent: &str, body: Option<&str>, pane: Option<PaneId>) -> Request {
    let body_line = body
        .map(|s| shorten_body(s, 160))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "task complete".to_string());
    Request::Notify {
        pane,
        title: format!("{agent} ready"),
        body: body_line,
        level: NotificationLevel::AttentionNeeded,
    }
}

/// Build a `Request::Notify` for an agent permission / notification
/// event. Carries the agent message verbatim (truncated).
pub fn build_notification_notify(
    agent: &str,
    message: Option<&str>,
    pane: Option<PaneId>,
) -> Request {
    let body_line = message
        .map(|s| shorten_body(s, 160))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "needs your attention".to_string());
    Request::Notify {
        pane,
        title: format!("{agent} needs your input"),
        body: body_line,
        level: NotificationLevel::AttentionNeeded,
    }
}

/// Send a request and ignore non-fatal errors. Hooks must not fail
/// loudly — Claude/OpenCode/Codex propagate non-zero exits to the
/// agent and surface them to the user as a hook error.
pub async fn send_best_effort(client: &Client, req: Request) {
    if let Ok(Response::Error(e)) = client.call(req).await {
        tracing::debug!(?e, "hook notify rejected by daemon");
    }
}

/// Connect to the daemon socket using the same fallback chain as
/// `Cli::socket`. Returns `None` (with a debug log) when the daemon is
/// unreachable so a hook on a host without flowmux-app running is a
/// silent no-op rather than a visible error.
pub async fn connect_daemon(socket: Option<PathBuf>) -> Option<Client> {
    let socket = socket
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET_PATH").map(PathBuf::from))
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from))
        .unwrap_or_else(flowmux_config::paths::runtime_socket);
    match Client::connect(&socket)
        .await
        .with_context(|| format!("connect daemon at {}", socket.display()))
    {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::debug!(error = %e, "hook: daemon not reachable, skipping notify");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_collapses_whitespace_and_caps_length() {
        let s = "line1\nline2  line3\t\twith\nlots of whitespace";
        let out = shorten_body(s, 1000);
        assert_eq!(out, "line1 line2 line3 with lots of whitespace");
    }

    #[test]
    fn shorten_appends_ellipsis_when_truncated() {
        let s = "abcdefghij".repeat(20);
        let out = shorten_body(&s, 30);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 31); // 30 + ellipsis
    }

    #[test]
    fn shorten_empty_input_yields_empty_string() {
        assert_eq!(shorten_body("", 100), "");
        assert_eq!(shorten_body("   \n\n  \t  ", 100), "");
    }

    #[test]
    fn build_stop_notify_falls_back_to_default_body() {
        match build_stop_notify("Claude", None, None) {
            Request::Notify {
                title, body, level, ..
            } => {
                assert!(title.contains("Claude"));
                assert_eq!(body, "task complete");
                assert_eq!(level, NotificationLevel::AttentionNeeded);
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }

    #[test]
    fn build_stop_notify_carries_pane_and_uses_supplied_body() {
        let pane = PaneId::new();
        match build_stop_notify("Codex", Some("wrote 3 files"), Some(pane)) {
            Request::Notify {
                pane: got_pane,
                body,
                ..
            } => {
                assert_eq!(got_pane, Some(pane));
                assert_eq!(body, "wrote 3 files");
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }

    #[test]
    fn build_notification_notify_uses_attention_level() {
        let req = build_notification_notify("Claude", Some("permission to run rm"), None);
        if let Request::Notify {
            title, body, level, ..
        } = req
        {
            assert_eq!(level, NotificationLevel::AttentionNeeded);
            assert!(title.contains("Claude"));
            assert!(body.contains("permission"));
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn build_notification_notify_falls_back_when_message_missing() {
        let req = build_notification_notify("OpenCode", None, None);
        if let Request::Notify { body, .. } = req {
            assert_eq!(body, "needs your attention");
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn parse_claude_hook_payload_extracts_stop_event_fields() {
        let raw = r#"{
            "session_id": "abc",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "Stop"
        }"#;
        let parsed: ClaudeHookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.session_id.as_deref(), Some("abc"));
        assert_eq!(parsed.hook_event_name.as_deref(), Some("Stop"));
        assert!(parsed.transcript_path.is_some());
    }

    #[test]
    fn parse_claude_hook_payload_tolerates_unknown_fields() {
        // Future Claude versions may add fields; we must not error.
        let raw = r#"{ "future_field": 42, "hook_event_name": "Notification", "message": "hi" }"#;
        let parsed: ClaudeHookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.hook_event_name.as_deref(), Some("Notification"));
        assert_eq!(parsed.message.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_claude_hook_payload_handles_empty_object() {
        let parsed: ClaudeHookInput = serde_json::from_str("{}").unwrap();
        assert!(parsed.hook_event_name.is_none());
        assert!(parsed.session_id.is_none());
    }
}
