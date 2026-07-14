// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    duration_label, FieldRefresh, Provider, ProviderRefresh, TokenTotals, UsageError,
    UsageErrorKind, UsageWindow,
};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn collect() -> ProviderRefresh {
    let collected_at = Utc::now();
    let day = Local::now().date_naive();
    let (tokens, limits) = match read_app_server().await {
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

async fn read_app_server() -> Result<(Value, Value), UsageError> {
    let mut child = Command::new("codex")
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                UsageError::new(UsageErrorKind::NotInstalled, "Codex CLI를 찾지 못했습니다.")
            } else {
                UsageError::new(
                    UsageErrorKind::Io,
                    "Codex 사용량 수집기를 시작하지 못했습니다.",
                )
            }
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        UsageError::new(
            UsageErrorKind::Io,
            "Codex 사용량 수집기를 시작하지 못했습니다.",
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        UsageError::new(
            UsageErrorKind::Io,
            "Codex 사용량 수집기를 시작하지 못했습니다.",
        )
    })?;

    let exchange = async move {
        for request in app_server_requests() {
            let mut line = serde_json::to_vec(&request).map_err(|_| {
                UsageError::new(
                    UsageErrorKind::InvalidData,
                    "Codex 사용량 요청을 만들지 못했습니다.",
                )
            })?;
            line.push(b'\n');
            stdin.write_all(&line).await.map_err(|_| {
                UsageError::new(
                    UsageErrorKind::Io,
                    "Codex 사용량 요청을 전송하지 못했습니다.",
                )
            })?;
        }
        stdin.flush().await.map_err(|_| {
            UsageError::new(
                UsageErrorKind::Io,
                "Codex 사용량 요청을 전송하지 못했습니다.",
            )
        })?;

        let mut lines = BufReader::new(stdout).lines();
        let mut limits_response = None;
        let mut usage_response = None;
        while let Some(line) = lines.next_line().await.map_err(|_| {
            UsageError::new(UsageErrorKind::Io, "Codex 사용량 응답을 읽지 못했습니다.")
        })? {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match value.get("id").and_then(Value::as_u64) {
                Some(1) => limits_response = Some(value),
                Some(2) => usage_response = Some(value),
                _ => {}
            }
            if let (Some(limits), Some(usage)) = (limits_response.take(), usage_response.take()) {
                return Ok((limits, usage));
            }
        }
        Err(UsageError::new(
            UsageErrorKind::InvalidData,
            "Codex 사용량 응답이 완료되지 않았습니다.",
        ))
    };

    let result = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_| {
            UsageError::new(
                UsageErrorKind::Timeout,
                "Codex 사용량 요청 시간이 초과되었습니다.",
            )
        })?;
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
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
            "Codex 로컬 로그인을 확인해 주세요.",
        ));
    }
    value.get("result").ok_or_else(invalid_response)
}

fn invalid_response() -> UsageError {
    UsageError::new(
        UsageErrorKind::InvalidData,
        "Codex 사용량 응답 형식이 올바르지 않습니다.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::TokenTotals;
    use chrono::NaiveDate;

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

        assert_eq!(windows[0].label, "주간");
        assert_eq!(windows[1].label, "5시간");
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
