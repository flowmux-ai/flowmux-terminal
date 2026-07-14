// SPDX-License-Identifier: GPL-3.0-or-later

use chrono::{DateTime, Duration, Utc};
use std::future::Future;
use std::path::PathBuf;

mod claude;
mod codex;

pub(crate) async fn collect_all(home: PathBuf) -> [ProviderRefresh; 2] {
    join_provider_refreshes(
        claude::collect(home.clone(), reqwest::Client::new()),
        codex::collect(home),
    )
    .await
}

async fn join_provider_refreshes<C, D>(claude: C, codex: D) -> [ProviderRefresh; 2]
where
    C: Future<Output = ProviderRefresh>,
    D: Future<Output = ProviderRefresh>,
{
    let (claude, codex) = tokio::join!(claude, codex);
    [claude, codex]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TokenTotals {
    pub(crate) today: Option<u64>,
    pub(crate) lifetime: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UsageWindow {
    pub(crate) label: String,
    pub(crate) scope: Option<String>,
    pub(crate) used_percent: f64,
    pub(crate) duration_minutes: Option<u64>,
    pub(crate) resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageErrorKind {
    NotInstalled,
    NotLoggedIn,
    Unauthorized,
    Timeout,
    Network,
    InvalidData,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UsageError {
    pub(crate) kind: UsageErrorKind,
    pub(crate) message: String,
}

impl UsageError {
    pub(crate) fn new(kind: UsageErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn network() -> Self {
        Self::new(
            UsageErrorKind::Network,
            "Could not connect to the usage service.",
        )
    }

    pub(crate) fn unauthorized() -> Self {
        Self::new(
            UsageErrorKind::Unauthorized,
            "Run Claude once to refresh the local login.",
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) enum FieldRefresh<T> {
    Success(T),
    Failure(UsageError),
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderRefresh {
    pub(crate) provider: Provider,
    pub(crate) tokens: FieldRefresh<TokenTotals>,
    pub(crate) limits: FieldRefresh<Vec<UsageWindow>>,
    pub(crate) collected_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub(crate) struct Timestamped<T> {
    pub(crate) value: T,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderState {
    pub(crate) provider: Provider,
    pub(crate) tokens: Option<Timestamped<TokenTotals>>,
    pub(crate) limits: Option<Timestamped<Vec<UsageWindow>>>,
    pub(crate) token_error: Option<UsageError>,
    pub(crate) limits_error: Option<UsageError>,
}

impl ProviderState {
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            tokens: None,
            limits: None,
            token_error: None,
            limits_error: None,
        }
    }

    fn apply(&mut self, update: ProviderRefresh) {
        match update.tokens {
            FieldRefresh::Success(value) => {
                self.tokens = Some(Timestamped {
                    value,
                    updated_at: update.collected_at,
                });
                self.token_error = None;
            }
            FieldRefresh::Failure(error) => self.token_error = Some(error),
        }
        match update.limits {
            FieldRefresh::Success(value) => {
                self.limits = Some(Timestamped {
                    value,
                    updated_at: update.collected_at,
                });
                self.limits_error = None;
            }
            FieldRefresh::Failure(error) => self.limits_error = Some(error),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UsagePanelState {
    pub(crate) claude: ProviderState,
    pub(crate) codex: ProviderState,
    pub(crate) refreshing: bool,
    last_finished_at: Option<DateTime<Utc>>,
}

impl Default for UsagePanelState {
    fn default() -> Self {
        Self {
            claude: ProviderState::new(Provider::Claude),
            codex: ProviderState::new(Provider::Codex),
            refreshing: false,
            last_finished_at: None,
        }
    }
}

impl UsagePanelState {
    pub(crate) fn apply(&mut self, update: ProviderRefresh) {
        match update.provider {
            Provider::Claude => self.claude.apply(update),
            Provider::Codex => self.codex.apply(update),
        }
    }

    pub(crate) fn begin_refresh(&mut self, now: DateTime<Utc>) -> bool {
        if self.refreshing
            || self
                .last_finished_at
                .is_some_and(|last| now - last < Duration::seconds(60))
        {
            return false;
        }
        self.refreshing = true;
        true
    }

    pub(crate) fn finish_refresh(&mut self, now: DateTime<Utc>) {
        self.refreshing = false;
        self.last_finished_at = Some(now);
    }

    pub(crate) fn begin_forced_refresh(&mut self) -> bool {
        if self.refreshing {
            return false;
        }
        self.refreshing = true;
        true
    }
}

pub(crate) fn duration_label(minutes: Option<u64>) -> String {
    match minutes {
        Some(10_080) => "Weekly".into(),
        Some(1_440) => "1 day".into(),
        Some(60) => "1 hour".into(),
        Some(1) => "1 minute".into(),
        Some(0) => "0 minutes".into(),
        Some(value) if value % 1_440 == 0 => format!("{} days", value / 1_440),
        Some(value) if value % 60 == 0 => format!("{} hours", value / 60),
        Some(value) => format!("{value} minutes"),
        None => "Usage limit".into(),
    }
}

pub(crate) fn format_token_count(value: u64) -> String {
    if value >= 1_000_000 {
        format_compact(value, 1_000_000, "M")
    } else if value >= 1_000 {
        format_compact(value, 1_000, "K")
    } else {
        value.to_string()
    }
}

fn format_compact(value: u64, scale: u64, suffix: &str) -> String {
    let tenths = value.saturating_mul(10).saturating_add(scale / 2) / scale;
    format!("{}.{:01}{suffix}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn window(duration_minutes: u64, used_percent: f64) -> UsageWindow {
        UsageWindow {
            label: duration_label(Some(duration_minutes)),
            scope: None,
            used_percent,
            duration_minutes: Some(duration_minutes),
            resets_at: None,
        }
    }

    #[test]
    fn failed_field_refresh_preserves_last_success() {
        let now = Utc::now();
        let mut state = UsagePanelState::default();
        state.apply(ProviderRefresh {
            provider: Provider::Claude,
            tokens: FieldRefresh::Success(TokenTotals {
                today: Some(123),
                lifetime: None,
            }),
            limits: FieldRefresh::Success(vec![window(300, 25.0)]),
            collected_at: now,
        });
        state.apply(ProviderRefresh {
            provider: Provider::Claude,
            tokens: FieldRefresh::Failure(UsageError::network()),
            limits: FieldRefresh::Failure(UsageError::unauthorized()),
            collected_at: now + Duration::minutes(1),
        });

        assert_eq!(state.claude.tokens.as_ref().unwrap().value.today, Some(123));
        assert_eq!(
            state.claude.limits.as_ref().unwrap().value[0].used_percent,
            25.0
        );
        assert!(state.claude.token_error.is_some());
        assert!(state.claude.limits_error.is_some());
    }

    #[test]
    fn duration_labels_are_metadata_driven() {
        assert_eq!(duration_label(Some(300)), "5 hours");
        assert_eq!(duration_label(Some(10_080)), "Weekly");
        assert_eq!(duration_label(Some(4_320)), "3 days");
        assert_eq!(duration_label(Some(90)), "90 minutes");
        assert_eq!(duration_label(Some(1_440)), "1 day");
        assert_eq!(duration_label(Some(60)), "1 hour");
        assert_eq!(duration_label(Some(1)), "1 minute");
        assert_eq!(duration_label(Some(0)), "0 minutes");
        assert_eq!(duration_label(None), "Usage limit");
    }

    #[test]
    fn token_counts_use_compact_readable_units() {
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_250), "1.3K");
        assert_eq!(format_token_count(2_500_000), "2.5M");
    }

    #[test]
    fn refreshes_are_coalesced_and_cached_for_sixty_seconds() {
        let now = Utc::now();
        let mut state = UsagePanelState::default();
        assert!(state.begin_refresh(now));
        assert!(!state.begin_refresh(now + Duration::seconds(1)));
        state.finish_refresh(now + Duration::seconds(2));
        assert!(!state.begin_refresh(now + Duration::seconds(59)));
        assert!(state.begin_refresh(now + Duration::seconds(63)));
    }

    #[test]
    fn forced_refresh_bypasses_cache_but_not_an_active_refresh() {
        let now = Utc::now();
        let mut state = UsagePanelState::default();
        assert!(state.begin_refresh(now));
        state.finish_refresh(now + Duration::seconds(1));
        assert!(!state.begin_refresh(now + Duration::seconds(2)));
        assert!(state.begin_forced_refresh());
        assert!(!state.begin_forced_refresh());
    }

    #[tokio::test]
    async fn concurrent_refresh_results_keep_provider_order() {
        fn refresh(provider: Provider) -> ProviderRefresh {
            ProviderRefresh {
                provider,
                tokens: FieldRefresh::Success(TokenTotals::default()),
                limits: FieldRefresh::Success(Vec::new()),
                collected_at: Utc::now(),
            }
        }

        let results = join_provider_refreshes(async { refresh(Provider::Claude) }, async {
            refresh(Provider::Codex)
        })
        .await;

        assert_eq!(results[0].provider, Provider::Claude);
        assert_eq!(results[1].provider, Provider::Codex);
    }
}
