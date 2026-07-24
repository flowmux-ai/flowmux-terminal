// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    duration_label, FieldRefresh, Provider, ProviderRefresh, TokenTotals, UsageError,
    UsageErrorKind, UsageWindow,
};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, Offset, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// A locked login keychain makes `security` block on a GUI unlock/authorization
// prompt with no timeout of its own, which would wedge the whole usage refresh.
#[cfg(target_os = "macos")]
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn collect(home: PathBuf, client: reqwest::Client) -> ProviderRefresh {
    let collected_at = Utc::now();
    let local_now = Local::now();
    let day = local_now.date_naive();
    let offset = local_now.offset().fix();
    let transcript_root = home.join(".claude").join("projects");
    let credentials_path = home.join(".claude").join(".credentials.json");

    let tokens_task = tokio::task::spawn_blocking(move || {
        sum_transcript_tree(&transcript_root, day, offset).map(|today| TokenTotals {
            today: Some(today),
            lifetime: None,
        })
    });
    let limits_task = fetch_limits(&credentials_path, &client);
    let (tokens_result, limits_result) = tokio::join!(tokens_task, limits_task);

    let tokens = match tokens_result {
        Ok(Ok(value)) => FieldRefresh::Success(value),
        Ok(Err(error)) => FieldRefresh::Failure(error),
        Err(_) => FieldRefresh::Failure(UsageError::new(
            UsageErrorKind::Io,
            "Could not read the Claude token logs.",
        )),
    };
    let limits = match limits_result {
        Ok(value) => FieldRefresh::Success(value),
        Err(error) => FieldRefresh::Failure(error),
    };

    ProviderRefresh {
        provider: Provider::Claude,
        tokens,
        limits,
        collected_at,
    }
}

async fn fetch_limits(
    credentials_path: &Path,
    client: &reqwest::Client,
) -> Result<Vec<UsageWindow>, UsageError> {
    let raw = match tokio::fs::read_to_string(credentials_path).await {
        Ok(raw) => raw,
        // macOS stores the Claude login in the Keychain, not on disk, so fall
        // back to it before reporting the user as logged out.
        Err(_) => keychain_credentials().await?,
    };
    let access_token = access_token_from_credentials(&raw)?;
    drop(raw);

    let request = async {
        let response = client
            .get(USAGE_URL)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| UsageError::network())?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(UsageError::unauthorized());
        }
        if !response.status().is_success() {
            return Err(UsageError::network());
        }
        let value = response.json::<Value>().await.map_err(|_| {
            UsageError::new(
                UsageErrorKind::InvalidData,
                "Could not read the Claude usage response.",
            )
        })?;
        parse_usage_response(&value)
    };

    tokio::time::timeout(REQUEST_TIMEOUT, request)
        .await
        .map_err(|_| {
            UsageError::new(
                UsageErrorKind::Timeout,
                "The Claude usage request timed out.",
            )
        })?
}

fn not_logged_in() -> UsageError {
    UsageError::new(
        UsageErrorKind::NotLoggedIn,
        "The local Claude login was not found.",
    )
}

#[cfg(target_os = "macos")]
async fn keychain_credentials() -> Result<String, UsageError> {
    // `kill_on_drop` + an explicit timeout guarantee this returns: if the
    // keychain is locked, `security` waits on a modal prompt forever, so on
    // timeout we drop (and kill) the child and report the user as logged out.
    let child = tokio::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| not_logged_in())?;
    let output = match tokio::time::timeout(KEYCHAIN_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        _ => return Err(not_logged_in()),
    };
    if !output.status.success() {
        return Err(not_logged_in());
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| not_logged_in())?
        .trim()
        .to_string();
    if raw.is_empty() {
        return Err(not_logged_in());
    }
    Ok(raw)
}

#[cfg(not(target_os = "macos"))]
async fn keychain_credentials() -> Result<String, UsageError> {
    Err(not_logged_in())
}

#[derive(Deserialize)]
struct ClaudeCredentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<ClaudeOauthCredentials>,
}

#[derive(Deserialize)]
struct ClaudeOauthCredentials {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
}

fn access_token_from_credentials(raw: &str) -> Result<String, UsageError> {
    let credentials: ClaudeCredentials = serde_json::from_str(raw).map_err(|_| {
        UsageError::new(
            UsageErrorKind::InvalidData,
            "Could not read the Claude login information.",
        )
    })?;
    credentials
        .oauth
        .and_then(|oauth| oauth.access_token)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            UsageError::new(
                UsageErrorKind::NotLoggedIn,
                "The local Claude login was not found.",
            )
        })
}

fn parse_usage_response(value: &Value) -> Result<Vec<UsageWindow>, UsageError> {
    let object = value.as_object().ok_or_else(|| {
        UsageError::new(
            UsageErrorKind::InvalidData,
            "The Claude usage response was invalid.",
        )
    })?;
    let mut windows = Vec::new();
    if let Some(window) = parse_named_window(object.get("five_hour"), Some(300), None)? {
        windows.push(window);
    }
    if let Some(window) = parse_named_window(object.get("seven_day"), Some(10_080), None)? {
        windows.push(window);
    }
    // Model- or feature-scoped windows (e.g. "seven_day_opus", "fable") arrive
    // as extra top-level objects; keep them all instead of the fixed two.
    for (key, entry) in object {
        if matches!(key.as_str(), "five_hour" | "seven_day" | "limits") || !entry.is_object() {
            continue;
        }
        let (duration_minutes, scope) = window_metadata(key);
        if let Some(window) = parse_named_window(Some(entry), duration_minutes, scope)? {
            windows.push(window);
        }
    }
    // Same for the optional "limits" array, which carries kind/group metadata.
    if let Some(limits) = object.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let duration_minutes = limit
                .get("kind")
                .and_then(Value::as_str)
                .and_then(kind_duration_minutes);
            let scope = limit.get("group").and_then(Value::as_str).map(scope_label);
            if let Some(window) = parse_named_window(Some(limit), duration_minutes, scope)? {
                windows.push(window);
            }
        }
    }
    deduplicate_windows(&mut windows);
    Ok(windows)
}

fn window_metadata(key: &str) -> (Option<u64>, Option<String>) {
    if let Some(rest) = key.strip_prefix("five_hour_") {
        (Some(300), Some(scope_label(rest)))
    } else if let Some(rest) = key.strip_prefix("seven_day_") {
        (Some(10_080), Some(scope_label(rest)))
    } else {
        (None, Some(scope_label(key)))
    }
}

fn kind_duration_minutes(kind: &str) -> Option<u64> {
    match kind {
        "five_hour" => Some(300),
        "daily" => Some(1_440),
        "weekly" | "seven_day" => Some(10_080),
        _ => None,
    }
}

fn scope_label(raw: &str) -> String {
    let label = raw.replace('_', " ");
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => label,
    }
}

fn parse_named_window(
    value: Option<&Value>,
    duration_minutes: Option<u64>,
    scope: Option<String>,
) -> Result<Option<UsageWindow>, UsageError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(object) = value.as_object() else {
        return Err(UsageError::new(
            UsageErrorKind::InvalidData,
            "The Claude rate limit response was invalid.",
        ));
    };
    let Some(used_percent) = object
        .get("utilization")
        .or_else(|| object.get("percent"))
        .and_then(Value::as_f64)
    else {
        return Ok(None);
    };
    let resets_at = object
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|value| value.with_timezone(&Utc));
    Ok(Some(UsageWindow {
        label: duration_label(duration_minutes),
        scope,
        used_percent,
        duration_minutes,
        resets_at,
    }))
}

fn deduplicate_windows(windows: &mut Vec<UsageWindow>) {
    let mut seen = HashSet::new();
    windows.retain(|window| {
        seen.insert((
            window.duration_minutes,
            window.resets_at,
            window.used_percent.to_bits(),
            window.scope.clone(),
        ))
    });
}

#[derive(Deserialize)]
struct TranscriptEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    uuid: Option<String>,
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    id: Option<String>,
    usage: Option<TranscriptUsage>,
}

#[derive(Default, Deserialize)]
struct TranscriptUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl TranscriptUsage {
    fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

fn sum_transcript_tree(
    root: &Path,
    target_day: NaiveDate,
    offset: FixedOffset,
) -> Result<u64, UsageError> {
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(UsageError::new(
                UsageErrorKind::NotLoggedIn,
                "The local Claude usage history was not found.",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(UsageError::new(
                UsageErrorKind::NotLoggedIn,
                "The local Claude usage history was not found.",
            ));
        }
        Err(_) => return Err(transcript_io_error()),
    }
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(directory).map_err(|_| transcript_io_error())?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(transcript_io_error()),
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(transcript_io_error()),
            };
            if file_type.is_dir() {
                directories.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                files.push(path);
            }
        }
    }

    let mut seen = HashSet::new();
    let mut total = 0_u64;
    for path in files {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(transcript_io_error()),
        };
        let file_total = sum_transcript_reader(BufReader::new(file), target_day, offset, &mut seen)
            .map_err(|_| transcript_io_error())?;
        total = total.saturating_add(file_total);
    }
    Ok(total)
}

fn transcript_io_error() -> UsageError {
    UsageError::new(
        UsageErrorKind::Io,
        "Could not read the local Claude usage history.",
    )
}

fn sum_transcript_reader<R: BufRead>(
    reader: R,
    target_day: NaiveDate,
    offset: FixedOffset,
    seen: &mut HashSet<String>,
) -> io::Result<u64> {
    let mut total = 0_u64;
    for line in reader.lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<TranscriptEntry>(&line) else {
            continue;
        };
        if entry.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(timestamp) = entry
            .timestamp
            .as_deref()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        else {
            continue;
        };
        if timestamp.with_timezone(&offset).date_naive() != target_day {
            continue;
        }
        let Some(message) = entry.message else {
            continue;
        };
        let Some(id) = message.id.or(entry.uuid) else {
            continue;
        };
        let Some(usage) = message.usage else {
            continue;
        };
        if seen.insert(id) {
            total = total.saturating_add(usage.total());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};
    use std::collections::HashSet;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn usage_response_keeps_every_reported_window() {
        let value = serde_json::json!({
            "five_hour": {
                "utilization": 7.0,
                "resets_at": "2026-07-14T10:00:00Z"
            },
            "seven_day": {
                "utilization": 42.0,
                "resets_at": "2026-07-20T00:00:00Z"
            },
            "seven_day_opus": {
                "utilization": 12.0,
                "resets_at": "2026-07-20T00:00:00Z"
            },
            "extra_window": {
                "utilization": 3.0
            },
            "not_a_window": "ignored",
            "limits": [{
                "kind": "weekly",
                "group": "fable",
                "percent": 78.0,
                "resets_at": "2026-07-19T00:00:00Z"
            }]
        });

        let windows = parse_usage_response(&value).unwrap();

        assert_eq!(windows[0].duration_minutes, Some(300));
        assert_eq!(windows[0].scope, None);
        assert_eq!(windows[1].duration_minutes, Some(10_080));
        assert_eq!(windows[1].scope, None);
        assert!(windows.iter().any(|window| {
            window.scope.as_deref() == Some("Opus")
                && window.duration_minutes == Some(10_080)
                && window.used_percent == 12.0
        }));
        assert!(windows.iter().any(|window| {
            window.scope.as_deref() == Some("Extra window") && window.duration_minutes.is_none()
        }));
        assert!(windows.iter().any(|window| {
            window.scope.as_deref() == Some("Fable")
                && window.duration_minutes == Some(10_080)
                && window.used_percent == 78.0
        }));
    }

    #[test]
    fn usage_response_allows_missing_optional_sections() {
        assert!(parse_usage_response(&serde_json::json!({}))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn transcript_sum_filters_local_day_and_deduplicates_message_id() {
        let fixture = concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-07-13T15:30:00Z\",\"uuid\":\"u1\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":7}}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-07-13T15:30:01Z\",\"uuid\":\"u2\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":7}}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-07-13T14:59:59Z\",\"uuid\":\"old\",\"message\":{\"id\":\"old\",\"usage\":{\"input_tokens\":100,\"output_tokens\":100}}}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-07-13T16:00:00Z\",\"message\":{}}\n",
            "not-json\n"
        );
        let mut seen = HashSet::new();
        let offset = FixedOffset::east_opt(9 * 60 * 60).unwrap();

        let total = sum_transcript_reader(
            fixture.as_bytes(),
            NaiveDate::from_ymd_opt(2026, 7, 14).unwrap(),
            offset,
            &mut seen,
        )
        .unwrap();

        assert_eq!(total, 17);
        assert_eq!(seen.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn transcript_scan_reports_non_not_found_file_errors() {
        let temp = tempfile::tempdir().unwrap();
        let transcript = temp.path().join("blocked.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        let mut permissions = std::fs::metadata(&transcript).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&transcript, permissions).unwrap();

        let result = sum_transcript_tree(
            temp.path(),
            NaiveDate::from_ymd_opt(2026, 7, 14).unwrap(),
            FixedOffset::east_opt(9 * 60 * 60).unwrap(),
        );

        let mut permissions = std::fs::metadata(&transcript).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&transcript, permissions).unwrap();
        assert_eq!(result.unwrap_err().kind, UsageErrorKind::Io);
    }

    #[cfg(unix)]
    #[test]
    fn transcript_scan_reports_unreadable_root_as_io() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("blocked");
        let root = parent.join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let mut permissions = std::fs::metadata(&parent).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&parent, permissions).unwrap();

        let result = sum_transcript_tree(
            &root,
            NaiveDate::from_ymd_opt(2026, 7, 14).unwrap(),
            FixedOffset::east_opt(9 * 60 * 60).unwrap(),
        );

        let mut permissions = std::fs::metadata(&parent).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&parent, permissions).unwrap();
        assert_eq!(result.unwrap_err().kind, UsageErrorKind::Io);
    }

    #[test]
    fn credential_parser_extracts_only_access_token() {
        let raw = r#"{
            "claudeAiOauth": {
                "accessToken": "access-secret",
                "refreshToken": "refresh-secret"
            },
            "unrelated": "ignored"
        }"#;

        assert_eq!(access_token_from_credentials(raw).unwrap(), "access-secret");
    }
}
