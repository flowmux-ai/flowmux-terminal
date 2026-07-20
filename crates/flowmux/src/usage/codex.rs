// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    duration_label, FieldRefresh, Provider, ProviderRefresh, TokenTotals, UsageError,
    UsageErrorKind, UsageWindow,
};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FLOWMUX_AGENT_WRAPPER_MARKER: &[u8] = b"flowmux agent wrapper shim";

pub(crate) async fn collect(home: PathBuf) -> ProviderRefresh {
    let collected_at = Utc::now();
    let day = Local::now().date_naive();
    let path = std::env::var_os("PATH");
    let (tokens, limits) = match read_app_server(&home, path.as_deref()).await {
        Ok((limits_response, usage_response)) => (
            match parse_token_usage_response(&usage_response, day) {
                Ok(value) => FieldRefresh::Success(value),
                Err(error) => FieldRefresh::Failure(error),
            },
            match parse_rate_limits_response(&limits_response) {
                Ok(value) => FieldRefresh::Success(value),
                Err(error) => FieldRefresh::Failure(error),
            },
        ),
        Err(error) => (
            FieldRefresh::Failure(error.clone()),
            FieldRefresh::Failure(error),
        ),
    };
    ProviderRefresh {
        provider: Provider::Codex,
        tokens,
        limits,
        collected_at,
    }
}

fn app_server_requests() -> Vec<Value> {
    vec![
        json!({
            "method": "initialize",
            "id": 0,
            "params": {
                "clientInfo": {
                    "name": "flowmux",
                    "title": "flowmux",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
        json!({"method": "initialized", "params": {}}),
        json!({"method": "account/rateLimits/read", "id": 1}),
        json!({"method": "account/usage/read", "id": 2}),
    ]
}

async fn read_app_server(home: &Path, path: Option<&OsStr>) -> Result<(Value, Value), UsageError> {
    read_app_server_with_timeout(home, path, REQUEST_TIMEOUT).await
}

async fn read_app_server_with_timeout(
    home: &Path,
    path: Option<&OsStr>,
    timeout: Duration,
) -> Result<(Value, Value), UsageError> {
    let executable = resolve_codex_executable(home, path)
        .ok_or_else(|| UsageError::new(UsageErrorKind::NotInstalled, "Codex CLI was not found."))?;
    let mut command = Command::new(executable.program);
    if let Some(script) = executable.script {
        command.arg(script);
    }
    let mut child = command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                UsageError::new(UsageErrorKind::NotInstalled, "Codex CLI was not found.")
            } else {
                UsageError::new(
                    UsageErrorKind::Io,
                    "Could not start the Codex usage collector.",
                )
            }
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        UsageError::new(
            UsageErrorKind::Io,
            "Could not start the Codex usage collector.",
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        UsageError::new(
            UsageErrorKind::Io,
            "Could not start the Codex usage collector.",
        )
    })?;

    let exchange = async move {
        for request in app_server_requests() {
            let mut line = serde_json::to_vec(&request).map_err(|_| {
                UsageError::new(
                    UsageErrorKind::InvalidData,
                    "Could not create the Codex usage request.",
                )
            })?;
            line.push(b'\n');
            stdin.write_all(&line).await.map_err(|_| {
                UsageError::new(
                    UsageErrorKind::Io,
                    "Could not send the Codex usage request.",
                )
            })?;
        }
        stdin.flush().await.map_err(|_| {
            UsageError::new(
                UsageErrorKind::Io,
                "Could not send the Codex usage request.",
            )
        })?;

        let mut lines = BufReader::new(stdout).lines();
        let mut limits_response = None;
        let mut usage_response = None;
        while let Some(line) = lines.next_line().await.map_err(|_| {
            UsageError::new(
                UsageErrorKind::Io,
                "Could not read the Codex usage response.",
            )
        })? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(responses) =
                record_app_server_response(value, &mut limits_response, &mut usage_response)
            {
                return Ok(responses);
            }
        }
        Err(UsageError::new(
            UsageErrorKind::InvalidData,
            "The Codex usage response was incomplete.",
        ))
    };

    let result = match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => Err(UsageError::new(
            UsageErrorKind::Timeout,
            "The Codex usage request timed out.",
        )),
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

fn record_app_server_response(
    value: Value,
    limits_response: &mut Option<Value>,
    usage_response: &mut Option<Value>,
) -> Option<(Value, Value)> {
    match value.get("id").and_then(Value::as_u64) {
        Some(1) => {
            if let Some(usage) = usage_response.take() {
                Some((value, usage))
            } else {
                *limits_response = Some(value);
                None
            }
        }
        Some(2) => {
            if let Some(limits) = limits_response.take() {
                Some((limits, value))
            } else {
                *usage_response = Some(value);
                None
            }
        }
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CodexExecutable {
    program: PathBuf,
    script: Option<PathBuf>,
}

fn resolve_codex_executable(home: &Path, path: Option<&OsStr>) -> Option<CodexExecutable> {
    let executable_name = format!("codex{}", std::env::consts::EXE_SUFFIX);
    if let Some(candidate) = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(&executable_name))
        .find(|candidate| is_executable(candidate) && !is_flowmux_agent_wrapper(candidate))
    {
        return Some(CodexExecutable {
            program: candidate,
            script: None,
        });
    }

    let versions = home.join(".nvm/versions/node");
    fs::read_dir(versions)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = parse_node_version(&entry.file_name())?;
            let candidate = entry.path().join("bin").join(&executable_name);
            let node = entry
                .path()
                .join("bin")
                .join(format!("node{}", std::env::consts::EXE_SUFFIX));
            (is_executable(&candidate) && is_executable(&node)).then_some((
                version,
                CodexExecutable {
                    program: node,
                    script: Some(candidate),
                },
            ))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, candidate)| candidate)
}

fn parse_node_version(value: &OsStr) -> Option<(u64, u64, u64)> {
    let mut parts = value.to_str()?.strip_prefix('v')?.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_flowmux_agent_wrapper(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut prefix = Vec::new();
    if file.take(4_096).read_to_end(&mut prefix).is_err() {
        return false;
    }
    prefix
        .windows(FLOWMUX_AGENT_WRAPPER_MARKER.len())
        .any(|window| window == FLOWMUX_AGENT_WRAPPER_MARKER)
}

fn parse_rate_limits_response(value: &Value) -> Result<Vec<UsageWindow>, UsageError> {
    let result = response_result(value)?;
    let mut windows = Vec::new();
    let root_limit_id = result
        .get("rateLimits")
        .and_then(|snapshot| snapshot.get("limitId"))
        .and_then(Value::as_str);
    if let Some(snapshot) = result.get("rateLimits") {
        append_snapshot_windows(snapshot, None, &mut windows)?;
    }
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (limit_id, snapshot) in by_id {
            let limit_id = snapshot
                .get("limitId")
                .and_then(Value::as_str)
                .unwrap_or(limit_id);
            if Some(limit_id) == root_limit_id {
                continue;
            }
            let scope = snapshot
                .get("limitName")
                .and_then(Value::as_str)
                .unwrap_or(limit_id)
                .to_owned();
            append_snapshot_windows(snapshot, Some(scope), &mut windows)?;
        }
    }
    let mut seen = HashSet::new();
    windows.retain(|window| {
        seen.insert((
            window.duration_minutes,
            window.resets_at,
            window.used_percent.to_bits(),
            window.scope.clone(),
        ))
    });
    Ok(windows)
}

fn append_snapshot_windows(
    snapshot: &Value,
    fallback_scope: Option<String>,
    windows: &mut Vec<UsageWindow>,
) -> Result<(), UsageError> {
    let object = snapshot.as_object().ok_or_else(invalid_response)?;
    let scope = object
        .get("limitName")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(fallback_scope);
    for key in ["primary", "secondary"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let window = value.as_object().ok_or_else(invalid_response)?;
        let Some(used_percent) = window.get("usedPercent").and_then(Value::as_f64) else {
            continue;
        };
        let duration_minutes = window.get("windowDurationMins").and_then(Value::as_u64);
        let resets_at = window
            .get("resetsAt")
            .and_then(Value::as_i64)
            .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0));
        windows.push(UsageWindow {
            label: duration_label(duration_minutes),
            scope: scope.clone(),
            used_percent,
            duration_minutes,
            resets_at,
        });
    }
    Ok(())
}

fn parse_token_usage_response(value: &Value, day: NaiveDate) -> Result<TokenTotals, UsageError> {
    let result = response_result(value)?;
    let lifetime = match result.get("summary") {
        None | Some(Value::Null) => None,
        Some(Value::Object(summary)) => match summary.get("lifetimeTokens") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(invalid_response)?),
        },
        Some(_) => return Err(invalid_response()),
    };
    let today = match result.get("dailyUsageBuckets") {
        None | Some(Value::Null) => None,
        Some(Value::Array(buckets)) => {
            let target = day.format("%Y-%m-%d").to_string();
            let mut today = None;
            for bucket in buckets {
                let bucket = bucket.as_object().ok_or_else(invalid_response)?;
                let start_date = bucket
                    .get("startDate")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_response)?;
                let tokens = bucket
                    .get("tokens")
                    .and_then(Value::as_u64)
                    .ok_or_else(invalid_response)?;
                if start_date == target {
                    today = Some(tokens);
                    break;
                }
            }
            today
        }
        Some(_) => return Err(invalid_response()),
    };
    Ok(TokenTotals { today, lifetime })
}

fn response_result(value: &Value) -> Result<&Value, UsageError> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let error = error.as_object().ok_or_else(invalid_response)?;
        error
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(invalid_response)?;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(invalid_response)?;
        if is_authentication_error(message) {
            return Err(UsageError::new(
                UsageErrorKind::NotLoggedIn,
                "Check the local Codex login.",
            ));
        }
        return Err(UsageError::new(
            UsageErrorKind::InvalidData,
            "The Codex usage request failed.",
        ));
    }
    value.get("result").ok_or_else(invalid_response)
}

fn is_authentication_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "not authenticated",
        "authentication required",
        "login required",
        "not logged in",
        "unauthorized",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn invalid_response() -> UsageError {
    UsageError::new(
        UsageErrorKind::InvalidData,
        "The Codex usage response was invalid.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::TokenTotals;
    use chrono::NaiveDate;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn app_server_requests_use_expected_sequence() {
        let requests = app_server_requests();

        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[0]["id"], 0);
        assert_eq!(
            requests[0]["params"]["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(requests[1]["method"], "initialized");
        assert_eq!(requests[2]["method"], "account/rateLimits/read");
        assert_eq!(requests[2]["id"], 1);
        assert_eq!(requests[3]["method"], "account/usage/read");
        assert_eq!(requests[3]["id"], 2);
    }

    #[test]
    fn out_of_order_responses_are_retained_until_pair_is_complete() {
        let mut limits = None;
        let mut usage = None;

        assert!(record_app_server_response(
            serde_json::json!({"id": 2, "result": {"summary": {}}}),
            &mut limits,
            &mut usage,
        )
        .is_none());
        assert!(limits.is_none());
        assert!(usage.is_some());

        let (limits, usage) = record_app_server_response(
            serde_json::json!({"id": 1, "result": {"rateLimits": {}}}),
            &mut limits,
            &mut usage,
        )
        .expect("both responses should now be available");
        assert_eq!(limits["id"], 1);
        assert_eq!(usage["id"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_prefers_path_over_nvm() {
        let temp = tempfile::tempdir().unwrap();
        let path_dir = temp.path().join("path-bin");
        let path_codex = path_dir.join("codex");
        make_executable(&path_codex);
        let nvm_codex = temp.path().join(".nvm/versions/node/v99.0.0/bin/codex");
        make_executable(&nvm_codex);

        assert_eq!(
            resolve_codex_executable(temp.path(), Some(path_dir.as_os_str())),
            Some(CodexExecutable {
                program: path_codex,
                script: None,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_uses_newest_nvm_version_as_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let older = temp.path().join(".nvm/versions/node/v20.18.0/bin/codex");
        let older_node = temp.path().join(".nvm/versions/node/v20.18.0/bin/node");
        let newer = temp.path().join(".nvm/versions/node/v22.22.2/bin/codex");
        let newer_node = temp.path().join(".nvm/versions/node/v22.22.2/bin/node");
        make_executable(&older);
        make_executable(&older_node);
        make_executable(&newer);
        make_executable(&newer_node);

        let resolved = resolve_codex_executable(temp.path(), None).unwrap();

        assert_eq!(resolved.program, newer_node);
        assert_eq!(resolved.script, Some(newer));
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_skips_flowmux_wrapper_for_nvm_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let path_dir = temp.path().join("path-bin");
        let path_codex = path_dir.join("codex");
        make_executable(&path_codex);
        fs::write(
            &path_codex,
            "#!/usr/bin/env bash\n# flowmux agent wrapper shim\nexit 127\n",
        )
        .unwrap();

        let nvm_codex = temp.path().join(".nvm/versions/node/v24.13.0/bin/codex");
        let nvm_node = temp.path().join(".nvm/versions/node/v24.13.0/bin/node");
        make_executable(&nvm_codex);
        make_executable(&nvm_node);

        assert_eq!(
            resolve_codex_executable(temp.path(), Some(path_dir.as_os_str())),
            Some(CodexExecutable {
                program: nvm_node,
                script: Some(nvm_codex),
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_app_server_is_reaped_before_returning() {
        let temp = tempfile::tempdir().unwrap();
        let path_dir = temp.path().join("bin");
        let executable = path_dir.join("codex");
        let pid_file = temp.path().join("pid");
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nwhile :; do :; done\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();

        let error = read_app_server_with_timeout(
            temp.path(),
            Some(path_dir.as_os_str()),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind, UsageErrorKind::Timeout);
        let pid = fs::read_to_string(pid_file).unwrap();
        let still_running = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!still_running, "timed-out child must already be reaped");
    }

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn rate_limit_parser_uses_duration_and_keeps_scoped_windows() {
        let value = serde_json::json!({"result": {
            "rateLimits": {
                "primary": {
                    "usedPercent": 8,
                    "windowDurationMins": 10080,
                    "resetsAt": 1784505600
                },
                "secondary": {
                    "usedPercent": 20,
                    "windowDurationMins": 300,
                    "resetsAt": 1784000000
                }
            },
            "rateLimitsByLimitId": {
                "fable": {
                    "limitName": "Fable",
                    "primary": {
                        "usedPercent": 55,
                        "windowDurationMins": 10080,
                        "resetsAt": 1784505600
                    }
                }
            }
        }});

        let windows = parse_rate_limits_response(&value).unwrap();

        assert_eq!(windows[0].label, "Weekly");
        assert_eq!(windows[1].label, "5 hours");
        assert!(windows
            .iter()
            .any(|window| window.scope.as_deref() == Some("Fable")));
    }

    #[test]
    fn rate_limit_parser_deduplicates_mirrored_bucket() {
        let value = serde_json::json!({"result": {
            "rateLimits": {
                "limitName": "Codex",
                "primary": {"usedPercent": 8, "windowDurationMins": 300, "resetsAt": 1784000000}
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitName": "Codex",
                    "primary": {"usedPercent": 8, "windowDurationMins": 300, "resetsAt": 1784000000}
                }
            }
        }});

        assert_eq!(parse_rate_limits_response(&value).unwrap().len(), 1);
    }

    #[test]
    fn rate_limit_parser_deduplicates_null_named_root_by_limit_id() {
        let value = serde_json::json!({"result": {
            "rateLimits": {
                "limitId": "codex",
                "limitName": null,
                "primary": {"usedPercent": 33, "windowDurationMins": 10080, "resetsAt": 1784971874}
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": {"usedPercent": 33, "windowDurationMins": 10080, "resetsAt": 1784971874}
                }
            }
        }});

        let windows = parse_rate_limits_response(&value).unwrap();

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used_percent, 33.0);
        assert_eq!(windows[0].scope, None);
    }

    #[test]
    fn token_usage_parser_selects_local_date_bucket() {
        let value = serde_json::json!({"result": {
            "summary": {"lifetimeTokens": 123456},
            "dailyUsageBuckets": [
                {"startDate": "2026-07-13", "tokens": 10},
                {"startDate": "2026-07-14", "tokens": 789}
            ]
        }});

        assert_eq!(
            parse_token_usage_response(&value, NaiveDate::from_ymd_opt(2026, 7, 14).unwrap())
                .unwrap(),
            TokenTotals {
                today: Some(789),
                lifetime: Some(123456)
            }
        );
    }

    #[test]
    fn token_usage_parser_rejects_malformed_today_bucket() {
        let value = serde_json::json!({"result": {
            "summary": {"lifetimeTokens": 123456},
            "dailyUsageBuckets": [
                {"startDate": "2026-07-14", "tokens": "unknown"}
            ]
        }});

        let error =
            parse_token_usage_response(&value, NaiveDate::from_ymd_opt(2026, 7, 14).unwrap())
                .unwrap_err();

        assert_eq!(error.kind, UsageErrorKind::InvalidData);
    }

    #[test]
    fn token_usage_parser_does_not_invent_zero_for_missing_day() {
        let value = serde_json::json!({"result": {
            "summary": {"lifetimeTokens": 123456},
            "dailyUsageBuckets": [
                {"startDate": "2026-07-19", "tokens": 789}
            ]
        }});

        assert_eq!(
            parse_token_usage_response(&value, NaiveDate::from_ymd_opt(2026, 7, 20).unwrap())
                .unwrap(),
            TokenTotals {
                today: None,
                lifetime: Some(123456)
            }
        );
    }

    #[test]
    fn non_auth_rpc_error_is_not_reported_as_login_problem() {
        let value = serde_json::json!({
            "id": 1,
            "error": {"code": -32601, "message": "Method not found"}
        });

        let error = response_result(&value).unwrap_err();

        assert_eq!(error.kind, UsageErrorKind::InvalidData);
        assert_eq!(error.message, "The Codex usage request failed.");
    }

    #[test]
    fn auth_rpc_error_is_reported_as_missing_local_login() {
        let value = serde_json::json!({
            "id": 1,
            "error": {"code": -32600, "message": "Not authenticated"}
        });

        let error = response_result(&value).unwrap_err();

        assert_eq!(error.kind, UsageErrorKind::NotLoggedIn);
        assert_eq!(error.message, "Check the local Codex login.");
    }
}
