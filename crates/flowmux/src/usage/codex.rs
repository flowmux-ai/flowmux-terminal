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
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    let executable = resolve_codex_executable(home, path)
        .ok_or_else(|| UsageError::new(UsageErrorKind::NotInstalled, "Codex CLI was not found."))?;
    let mut child = Command::new(executable)
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

    let result = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_| {
            UsageError::new(
                UsageErrorKind::Timeout,
                "The Codex usage request timed out.",
            )
        })?;
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

fn resolve_codex_executable(home: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    let executable_name = format!("codex{}", std::env::consts::EXE_SUFFIX);
    if let Some(candidate) = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(&executable_name))
        .find(|candidate| is_executable(candidate))
    {
        return Some(candidate);
    }

    let versions = home.join(".nvm/versions/node");
    fs::read_dir(versions)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = parse_node_version(&entry.file_name())?;
            let candidate = entry.path().join("bin").join(&executable_name);
            is_executable(&candidate).then_some((version, candidate))
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

fn parse_rate_limits_response(value: &Value) -> Result<Vec<UsageWindow>, UsageError> {
    let result = response_result(value)?;
    let mut windows = Vec::new();
    if let Some(snapshot) = result.get("rateLimits") {
        append_snapshot_windows(snapshot, None, &mut windows)?;
    }
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (limit_id, snapshot) in by_id {
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
    let lifetime = result
        .get("summary")
        .and_then(|summary| summary.get("lifetimeTokens"))
        .and_then(Value::as_u64);
    let today = result
        .get("dailyUsageBuckets")
        .and_then(Value::as_array)
        .map(|buckets| {
            buckets
                .iter()
                .find(|bucket| {
                    bucket.get("startDate").and_then(Value::as_str)
                        == Some(day.format("%Y-%m-%d").to_string().as_str())
                })
                .and_then(|bucket| bucket.get("tokens").and_then(Value::as_u64))
                .unwrap_or(0)
        });
    Ok(TokenTotals { today, lifetime })
}

fn response_result(value: &Value) -> Result<&Value, UsageError> {
    if value.get("error").is_some() {
        return Err(UsageError::new(
            UsageErrorKind::NotLoggedIn,
            "Check the local Codex login.",
        ));
    }
    value.get("result").ok_or_else(invalid_response)
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
            Some(path_codex)
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_uses_newest_nvm_version_as_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let older = temp.path().join(".nvm/versions/node/v20.18.0/bin/codex");
        let newer = temp.path().join(".nvm/versions/node/v22.22.2/bin/codex");
        make_executable(&older);
        make_executable(&newer);

        assert_eq!(resolve_codex_executable(temp.path(), None), Some(newer));
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
}
