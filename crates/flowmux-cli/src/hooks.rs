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

use flowmux_core::{AgentActivity, NotificationLevel, PaneId, SurfaceId};
use flowmux_ipc::{
    client::Client,
    protocol::{Request, Response},
};
use serde::{de::DeserializeOwned, Deserialize};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HOOK_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const HOOK_NOTIFY_TIMEOUT: Duration = Duration::from_millis(750);

/// Subset of an agent hook payload that flowmux cares about. Reused
/// across Claude/Codex/OpenCode because their JSON shapes overlap on
/// the fields we surface (event name, optional message, optional
/// last assistant text). Unknown fields are ignored so a new agent
/// release doesn't break us.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub struct ClaudeHookInput {
    #[serde(
        default,
        alias = "thread-id",
        alias = "thread_id",
        alias = "sessionID",
        alias = "sessionId",
        alias = "taskId"
    )]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    /// Claude `SessionEnd` reason. Intentional exits such as Ctrl+C at the
    /// prompt use `prompt_input_exit`; the non-specific `other` reason remains
    /// resumable so an ambiguous teardown cannot discard recovery state.
    #[serde(default)]
    pub reason: Option<String>,
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
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return ClaudeHookInput::default();
    }
    read_hook_input(stdin.lock())
}

fn read_hook_input<R: Read>(reader: R) -> ClaudeHookInput {
    let mut buf = String::new();
    let _ = reader.take(1024 * 1024).read_to_string(&mut buf);
    parse_hook_payload(&buf).unwrap_or_default()
}

fn parse_hook_payload<T: DeserializeOwned>(payload: &str) -> Option<T> {
    if payload.trim().is_empty() {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Codex's `notify` config spawns the program with the JSON event
/// payload as the LAST positional argument. Do not fall back to stdin:
/// Codex can invoke `notify` with the terminal attached, and blocking
/// on that PTY makes Codex report "timeout waiting for child process to exit".
pub fn read_codex_hook_input(extra_args: &[String]) -> ClaudeHookInput {
    if let Some(payload) = extra_args.last() {
        if let Some(parsed) = parse_hook_payload(payload) {
            return parsed;
        }
    }
    ClaudeHookInput::default()
}

/// Resolve `FLOWMUX_PANE_ID` from the env (set by `flowmux` at PTY
/// spawn time). Mirrors `crate::pane_from_env` so the hooks module
/// stays self-contained.
pub fn pane_from_env() -> Option<PaneId> {
    std::env::var("FLOWMUX_PANE_ID")
        .ok()
        .as_deref()
        .and_then(|s| PaneId::from_str(s).ok())
}

/// Resolve `FLOWMUX_SURFACE_ID` (the specific tab inside the pane).
/// Lets the GUI tell whether the user is currently on that tab — so
/// it can suppress redundant toasts — and route a click to the right
/// tab when they are not.
pub fn surface_from_env() -> Option<SurfaceId> {
    std::env::var("FLOWMUX_SURFACE_ID")
        .ok()
        .as_deref()
        .and_then(|s| SurfaceId::from_str(s).ok())
}

/// Resolve the live agent PID exported by the wrapper shim. Follow-up
/// hook events use this only when the wrapper value is still present; they
/// must not replace a known-good agent PID with a short-lived hook helper PID.
pub fn pid_from_env() -> Option<u32> {
    pid_from_env_var()
}

/// Resolve a PID for initial session registration. Prefer the wrapper shim's
/// `FLOWMUX_AGENT_PID`; when shell startup bypassed the shim, fall back to this
/// hook process's parent PID. This fallback is only safe for SessionStart.
pub fn pid_from_env_or_parent() -> Option<u32> {
    pid_from_env_var().or_else(|| surface_from_env().and_then(|_| parent_process_pid()))
}

fn pid_from_env_var() -> Option<u32> {
    std::env::var("FLOWMUX_AGENT_PID")
        .ok()
        .as_deref()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&p| p != 0)
}

#[cfg(unix)]
fn parent_process_pid() -> Option<u32> {
    let pid = unsafe { libc::getppid() };
    u32::try_from(pid).ok().filter(|&p| p != 0)
}

#[cfg(not(unix))]
fn parent_process_pid() -> Option<u32> {
    None
}

fn hook_seq() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
}

/// Build a `Request::AgentActivityUpdate`. `activity: None` clears the
/// presence (session end / teardown).
#[cfg(test)]
pub fn build_activity_update(
    agent: &str,
    activity: Option<AgentActivity>,
    pid: Option<u32>,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
) -> Request {
    build_activity_update_with_metadata(agent, activity, pid, pane, surface, None, None, None)
}

/// Register an agent whose process is known but whose turn activity has not
/// been reported by a reliable lifecycle event.
#[cfg(test)]
pub fn build_unknown_activity_update(
    agent: &str,
    pid: Option<u32>,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
) -> Request {
    build_unknown_activity_update_with_session(agent, pid, pane, surface, None)
}

pub fn build_unknown_activity_update_with_session(
    agent: &str,
    pid: Option<u32>,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
    session_id: Option<&str>,
) -> Request {
    Request::AgentActivityUpdate {
        pane,
        surface,
        agent: agent.to_ascii_lowercase(),
        status: Some(flowmux_core::AgentStatus::Unknown),
        activity: None,
        pid,
        source: Some("flowmux:hook".into()),
        seq: hook_seq(),
        message: None,
        custom_status: None,
        session_id: session_id.map(str::to_string),
    }
}

/// Build a live activity update with optional agent hook metadata.
#[allow(clippy::too_many_arguments)]
pub fn build_activity_update_with_metadata(
    agent: &str,
    activity: Option<AgentActivity>,
    pid: Option<u32>,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
    message: Option<&str>,
    custom_status: Option<&str>,
    session_id: Option<&str>,
) -> Request {
    Request::AgentActivityUpdate {
        pane,
        surface,
        agent: agent.to_ascii_lowercase(),
        status: activity.map(AgentActivity::status),
        activity,
        pid,
        source: Some("flowmux:hook".into()),
        seq: hook_seq(),
        message: message.map(str::to_string),
        custom_status: custom_status.map(str::to_string),
        session_id: session_id.map(str::to_string),
    }
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
pub fn build_stop_notify(
    agent: &str,
    body: Option<&str>,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
) -> Request {
    let body_line = body
        .map(|s| shorten_body(s, 160))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "task complete".to_string());
    Request::Notify {
        surface,
        pane,
        title: format!("{agent} ready"),
        body: body_line,
        level: NotificationLevel::TurnCompleted,
    }
}

/// Build a `Request::Notify` for an agent permission / notification
/// event. Carries the agent message verbatim (truncated).
pub fn build_notification_notify(
    agent: &str,
    message: Option<&str>,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
) -> Request {
    let body_line = message
        .map(|s| shorten_body(s, 160))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "needs your attention".to_string());
    Request::Notify {
        pane,
        surface,
        title: format!("{agent} needs your input"),
        body: body_line,
        level: NotificationLevel::NeedsInput,
    }
}

/// Send a request and ignore non-fatal errors. Hooks must not fail
/// loudly — Claude/OpenCode/Codex propagate non-zero exits to the
/// agent and surface them to the user as a hook error.
pub async fn send_best_effort(client: &Client, req: Request) {
    send_best_effort_with_timeout(client, req, HOOK_NOTIFY_TIMEOUT).await;
}

async fn send_best_effort_with_timeout(client: &Client, req: Request, timeout: Duration) {
    let summary = match &req {
        Request::Notify {
            pane,
            surface,
            title,
            level,
            ..
        } => {
            format!("Notify(title={title:?}, pane={pane:?}, surface={surface:?}, level={level:?})")
        }
        other => format!("{other:?}"),
    };
    flowmux_config::notify_debug!("cli/hook", "sending {summary}");
    match tokio::time::timeout(timeout, client.call(req)).await {
        Ok(Ok(Response::Error(e))) => {
            tracing::debug!(?e, "hook notify rejected by daemon");
            flowmux_config::notify_debug!("cli/hook", "daemon rejected: {e:?}");
        }
        Ok(Ok(resp)) => {
            flowmux_config::notify_debug!("cli/hook", "daemon replied: {resp:?}");
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "hook notify failed");
            flowmux_config::notify_debug!("cli/hook", "rpc transport error: {e}");
        }
        Err(_) => {
            tracing::debug!(?timeout, "hook notify timed out");
            flowmux_config::notify_debug!("cli/hook", "rpc timed out after {timeout:?}");
        }
    }
}

/// Connect to the daemon socket using the same fallback chain as
/// `Cli::socket`. Returns `None` (with a debug log) when the daemon is
/// unreachable so a hook on a host without flowmux running is a
/// silent no-op rather than a visible error.
pub async fn connect_daemon(socket: Option<PathBuf>) -> Option<Client> {
    connect_daemon_with_timeout(socket, HOOK_CONNECT_TIMEOUT).await
}

async fn connect_daemon_with_timeout(socket: Option<PathBuf>, timeout: Duration) -> Option<Client> {
    let env_socket = socket
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET_PATH").map(PathBuf::from))
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from));
    let env_source = env_socket
        .as_ref()
        .map(|_| "FLOWMUX_SOCKET_PATH/env")
        .unwrap_or("runtime_socket fallback");
    let primary = env_socket.unwrap_or_else(flowmux_config::paths::runtime_socket);
    flowmux_config::notify_debug!(
        "cli/hook",
        "connect_daemon: primary={primary:?} source={env_source} flatpak={} HOME={:?}",
        flowmux_config::paths::is_flatpak_sandbox(),
        std::env::var_os("HOME")
    );

    if let Some(client) = try_connect(&primary, timeout).await {
        return Some(client);
    }

    // Fallback: a stale `current.sock` symlink, a never-written
    // pointer, or an env that references a dead daemon all leave the
    // primary attempt with ENOENT/ECONNREFUSED. Scan
    // `$HOME/.cache/flowmux/flowmux-*.sock` for any per-PID socket
    // an active daemon may have bound and try each. Outside Flatpak
    // the same dir is still safe to scan (it is created on demand);
    // it is just empty by default so the fallback is a no-op.
    if let Some(candidates) = scan_pid_sockets() {
        flowmux_config::notify_debug!(
            "cli/hook",
            "primary unreachable; scanning {} per-PID candidates",
            candidates.len()
        );
        for path in candidates {
            if path == primary {
                continue;
            }
            if let Some(client) = try_connect(&path, timeout).await {
                flowmux_config::notify_debug!(
                    "cli/hook",
                    "fallback connected via per-PID socket {path:?}"
                );
                return Some(client);
            }
        }
    }
    None
}

async fn try_connect(socket: &PathBuf, timeout: Duration) -> Option<Client> {
    let exists = socket.exists();
    flowmux_config::notify_debug!("cli/hook", "try_connect path={socket:?} exists={exists}");
    match tokio::time::timeout(timeout, Client::connect(socket)).await {
        Ok(Ok(c)) => {
            flowmux_config::notify_debug!("cli/hook", "connected to {socket:?}");
            Some(c)
        }
        Ok(Err(e)) => {
            let e = e.context(format!("connect daemon at {}", socket.display()));
            tracing::debug!(error = %e, "hook: daemon not reachable, skipping notify");
            flowmux_config::notify_debug!("cli/hook", "connect error {socket:?}: {e}");
            None
        }
        Err(e) => {
            let e = anyhow::anyhow!("timed out after {:?}", e);
            tracing::debug!(error = %e, "hook: daemon not reachable, skipping notify");
            flowmux_config::notify_debug!("cli/hook", "connect timeout {socket:?}: {e}");
            None
        }
    }
}

/// Enumerate `$HOME/.cache/flowmux/flowmux-*.sock` entries. Returns
/// None when the dir does not exist; an empty list when the dir is
/// there but contains no per-PID sockets.
fn scan_pid_sockets() -> Option<Vec<PathBuf>> {
    let dir = flowmux_config::paths::host_visible_cache_dir()?;
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name_s = name.to_string_lossy();
        if name_s.starts_with("flowmux-") && name_s.ends_with(".sock") {
            out.push(e.path());
        }
    }
    Some(out)
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
    fn read_hook_input_parses_stdin_payloads_for_claude_style_hooks() {
        let parsed = read_hook_input(r#"{ "message": "approval needed" }"#.as_bytes());
        assert_eq!(parsed.message.as_deref(), Some("approval needed"));
    }

    #[test]
    fn build_activity_update_lowercases_agent_and_carries_fields() {
        let req = build_activity_update(
            "Claude",
            Some(AgentActivity::Running),
            Some(4321),
            None,
            None,
        );
        match req {
            Request::AgentActivityUpdate {
                agent,
                activity,
                pid,
                ..
            } => {
                assert_eq!(agent, "claude");
                assert_eq!(activity, Some(AgentActivity::Running));
                assert_eq!(pid, Some(4321));
            }
            other => panic!("expected AgentActivityUpdate, got {other:?}"),
        }
    }

    #[test]
    fn build_activity_update_with_metadata_carries_hook_fields() {
        let req = build_activity_update_with_metadata(
            "Claude",
            Some(AgentActivity::NeedsInput),
            None,
            None,
            None,
            Some("approval needed"),
            Some("waiting"),
            Some("session-1"),
        );
        match req {
            Request::AgentActivityUpdate {
                status,
                source,
                seq,
                message,
                custom_status,
                session_id,
                ..
            } => {
                assert_eq!(status, Some(flowmux_core::AgentStatus::Blocked));
                assert_eq!(source.as_deref(), Some("flowmux:hook"));
                assert!(seq.is_some());
                assert_eq!(message.as_deref(), Some("approval needed"));
                assert_eq!(custom_status.as_deref(), Some("waiting"));
                assert_eq!(session_id.as_deref(), Some("session-1"));
            }
            other => panic!("expected AgentActivityUpdate, got {other:?}"),
        }
    }

    #[test]
    fn build_activity_update_none_activity_clears() {
        let req = build_activity_update("codex", None, None, None, None);
        match req {
            Request::AgentActivityUpdate { activity, pid, .. } => {
                assert!(activity.is_none());
                assert!(pid.is_none());
            }
            other => panic!("expected AgentActivityUpdate, got {other:?}"),
        }
    }

    #[test]
    fn build_unknown_activity_update_keeps_presence_without_claiming_idle() {
        let req = build_unknown_activity_update("Codex", Some(42), None, None);
        match req {
            Request::AgentActivityUpdate {
                agent,
                status,
                activity,
                pid,
                ..
            } => {
                assert_eq!(agent, "codex");
                assert_eq!(status, Some(flowmux_core::AgentStatus::Unknown));
                assert_eq!(activity, None);
                assert_eq!(pid, Some(42));
            }
            other => panic!("expected AgentActivityUpdate, got {other:?}"),
        }
    }

    #[test]
    fn pid_from_env_parses_and_rejects_zero() {
        let _g = hook_env_lock();
        let prev = std::env::var_os("FLOWMUX_AGENT_PID");
        std::env::set_var("FLOWMUX_AGENT_PID", " 4321 ");
        assert_eq!(pid_from_env_var(), Some(4321));
        std::env::set_var("FLOWMUX_AGENT_PID", "0");
        assert_eq!(pid_from_env_var(), None);
        std::env::set_var("FLOWMUX_AGENT_PID", "notanumber");
        assert_eq!(pid_from_env_var(), None);
        match prev {
            Some(v) => std::env::set_var("FLOWMUX_AGENT_PID", v),
            None => std::env::remove_var("FLOWMUX_AGENT_PID"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn parent_process_pid_reports_live_parent() {
        assert!(parent_process_pid().is_some_and(|pid| pid > 0));
    }

    #[test]
    #[cfg(unix)]
    fn pid_from_env_does_not_fallback_to_parent_for_followup_events() {
        let _g = hook_env_lock();
        let prev_pid = std::env::var_os("FLOWMUX_AGENT_PID");
        let prev_surface = std::env::var_os("FLOWMUX_SURFACE_ID");
        std::env::remove_var("FLOWMUX_AGENT_PID");
        std::env::set_var(
            "FLOWMUX_SURFACE_ID",
            flowmux_core::SurfaceId::new().to_string(),
        );

        assert_eq!(pid_from_env(), None);
        assert!(pid_from_env_or_parent().is_some());

        match prev_pid {
            Some(v) => std::env::set_var("FLOWMUX_AGENT_PID", v),
            None => std::env::remove_var("FLOWMUX_AGENT_PID"),
        }
        match prev_surface {
            Some(v) => std::env::set_var("FLOWMUX_SURFACE_ID", v),
            None => std::env::remove_var("FLOWMUX_SURFACE_ID"),
        }
    }

    #[test]
    fn read_codex_hook_input_parses_last_positional_payload() {
        let args = vec![
            "--ignored".to_string(),
            r#"{ "last-assistant-message": "changed 2 files" }"#.to_string(),
        ];
        let parsed = read_codex_hook_input(&args);
        assert_eq!(
            parsed.last_assistant_message.as_deref(),
            Some("changed 2 files")
        );
    }

    #[test]
    fn read_codex_hook_input_defaults_without_stdin_fallback() {
        let parsed = read_codex_hook_input(&[]);
        assert!(parsed.session_id.is_none());
        assert!(parsed.message.is_none());
        assert!(parsed.last_assistant_message.is_none());
    }

    #[test]
    fn build_stop_notify_falls_back_to_default_body() {
        match build_stop_notify("Claude", None, None, None) {
            Request::Notify {
                title, body, level, ..
            } => {
                assert!(title.contains("Claude"));
                assert_eq!(body, "task complete");
                assert_eq!(level, NotificationLevel::TurnCompleted);
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }

    #[test]
    fn build_stop_notify_carries_pane_and_surface_for_click_routing() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        match build_stop_notify("Codex", Some("wrote 3 files"), Some(pane), Some(surface)) {
            Request::Notify {
                pane: got_pane,
                surface: got_surface,
                body,
                ..
            } => {
                assert_eq!(got_pane, Some(pane));
                assert_eq!(got_surface, Some(surface));
                assert_eq!(body, "wrote 3 files");
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }

    #[test]
    fn build_notification_notify_uses_attention_level() {
        let req = build_notification_notify("Claude", Some("permission to run rm"), None, None);
        if let Request::Notify {
            title, body, level, ..
        } = req
        {
            assert_eq!(level, NotificationLevel::NeedsInput);
            assert!(title.contains("Claude"));
            assert!(body.contains("permission"));
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn build_notification_notify_falls_back_when_message_missing() {
        let req = build_notification_notify("OpenCode", None, None, None);
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
    fn parse_claude_session_end_reason() {
        let parsed: ClaudeHookInput = serde_json::from_str(
            r#"{"session_id":"abc","hook_event_name":"SessionEnd","reason":"prompt_input_exit"}"#,
        )
        .unwrap();
        assert_eq!(parsed.reason.as_deref(), Some("prompt_input_exit"));
    }

    #[test]
    fn codex_and_opencode_session_id_aliases_are_accepted() {
        for raw in [
            r#"{"thread-id":"codex-thread"}"#,
            r#"{"sessionID":"opencode-session"}"#,
            r#"{"sessionId":"camel-session"}"#,
            r#"{"taskId":"cline-task"}"#,
        ] {
            let parsed: ClaudeHookInput = serde_json::from_str(raw).unwrap();
            assert!(parsed.session_id.is_some(), "payload was {raw}");
        }
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

    #[tokio::test]
    async fn send_best_effort_times_out_when_daemon_keeps_request_open() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("flowmux.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, _w) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(r);
            let mut line = String::new();
            let _ = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let client = Client::connect(&socket).await.unwrap();
        let start = std::time::Instant::now();
        send_best_effort_with_timeout(&client, Request::Ping, Duration::from_millis(25)).await;
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "hook notify should return promptly when the daemon does not answer"
        );
        server.abort();
    }

    fn hook_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}
