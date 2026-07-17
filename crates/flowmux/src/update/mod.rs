// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-update: detect a newer release tag and choose an action that matches
//! the running installation. Source installs may build a managed checkout;
//! packaged and unknown installs open the release page instead.
//!
//! Split: [`check`] is the pure, unit-tested core (version parsing,
//! command plan); [`install`] executes that plan on the tokio runtime;
//! `ui::update_banner` renders the state in the side panel.

pub mod check;
pub mod install;
pub mod origin;

use check::Version;
use std::path::{Path, PathBuf};

const IGNORED_VERSION_FILE: &str = "ignored-update-version";

fn ignored_version_path() -> Option<PathBuf> {
    flowmux_config::paths::state_dir().map(|dir| dir.join(IGNORED_VERSION_FILE))
}

/// Release the user chose not to be prompted about. The marker is kept in
/// the state directory so the choice survives restarts without becoming a
/// permanent update preference: a strictly newer release is offered again.
pub fn ignored_version() -> Option<Version> {
    let ignored = read_ignored_version(ignored_version_path()?.as_path())?;
    check::update_available(env!("CARGO_PKG_VERSION"), ignored).then_some(ignored)
}

fn read_ignored_version(path: &Path) -> Option<Version> {
    let raw = std::fs::read_to_string(path).ok()?;
    Version::parse(raw.trim())
}

/// Persist that `version` should stay quiet until a newer release appears.
pub fn persist_ignored_version(version: Version) -> std::io::Result<()> {
    let path = ignored_version_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "state directory is unavailable",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{version}\n"))
}

/// Progress reported by the background install task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Bringing the managed clone to the release tag.
    Fetching,
    /// Running the platform install script (build + install).
    Installing,
}

/// Events flowing from the tokio side to the side-panel banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The running build is the newest release.
    Current,
    /// A newer release exists.
    Available(Version),
    /// Install progress.
    Stage(Stage),
    /// Install finished; takes effect on next launch.
    Done(Version),
    /// Install failed; message is a short summary for the banner.
    Failed(String),
    /// The user does not want another prompt for this release.
    Ignored(Version),
    /// An install was requested from either the banner or About popup.
    Start(Version),
}

/// Side-panel banner state. Pure so transitions stay unit-testable;
/// the GTK adapter only maps a state to widget properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BannerState {
    Hidden,
    /// A release check completed and the running build is current.
    Current,
    /// Suppress this version and older ones; a newer release reopens the offer.
    Ignored(Version),
    /// Update to `Version` can be started.
    Available(Version),
    /// Install running; keep the target version for retry/labels.
    Running(Stage, Version),
    Done(Version),
    /// Install failed; retry targets `Version`.
    Failed(String, Version),
}

impl BannerState {
    /// Fold an [`Event`] into the current state.
    pub fn apply(self, event: Event) -> BannerState {
        match (self, event) {
            // A running install owns the banner; periodic re-checks wait.
            (state @ BannerState::Running(..), Event::Available(_) | Event::Current) => state,
            // Keep the restart message after installation even if another
            // check reports no newer release.
            (state @ BannerState::Done(_), Event::Current) => state,
            // A skipped release stays quiet across periodic checks. Only a
            // strictly newer release is news again.
            (state @ BannerState::Ignored(ignored), Event::Available(v)) if v <= ignored => state,
            // The installed release being re-announced is not news.
            (BannerState::Done(installed), Event::Available(v)) if v <= installed => {
                BannerState::Done(installed)
            }
            (_, Event::Current) => BannerState::Current,
            (_, Event::Available(v)) => BannerState::Available(v),
            (_, Event::Ignored(v)) => BannerState::Ignored(v),
            // Move to Running synchronously so a second click cannot start a
            // duplicate installer before the async task reports its first stage.
            (state @ BannerState::Running(..), Event::Start(_)) => state,
            (BannerState::Done(installed), Event::Start(v)) if v <= installed => {
                BannerState::Done(installed)
            }
            (_, Event::Start(v)) => BannerState::Running(Stage::Fetching, v),
            (state, Event::Stage(stage)) => match state {
                BannerState::Available(v)
                | BannerState::Running(_, v)
                | BannerState::Failed(_, v)
                | BannerState::Done(v) => BannerState::Running(stage, v),
                BannerState::Hidden | BannerState::Current | BannerState::Ignored(_) => state,
            },
            (_, Event::Done(v)) => BannerState::Done(v),
            (state, Event::Failed(message)) => match state {
                BannerState::Available(v)
                | BannerState::Running(_, v)
                | BannerState::Failed(_, v)
                | BannerState::Done(v) => BannerState::Failed(message, v),
                BannerState::Hidden | BannerState::Current | BannerState::Ignored(_) => state,
            },
        }
    }

    /// True when clicking the banner button should start an install
    /// (initial attempt or retry) targeting the returned version.
    pub fn actionable_version(&self) -> Option<Version> {
        match self {
            BannerState::Available(v) | BannerState::Failed(_, v) => Some(*v),
            BannerState::Hidden
            | BannerState::Current
            | BannerState::Ignored(_)
            | BannerState::Running(..)
            | BannerState::Done(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: Version = Version(0, 7, 1);
    const V2: Version = Version(0, 8, 0);

    #[test]
    fn available_shows_the_offer() {
        assert_eq!(
            BannerState::Hidden.apply(Event::Available(V1)),
            BannerState::Available(V1)
        );
    }

    #[test]
    fn current_check_records_a_hidden_up_to_date_state() {
        assert_eq!(
            BannerState::Hidden.apply(Event::Current),
            BannerState::Current
        );
        assert_eq!(
            BannerState::Ignored(V1).apply(Event::Current),
            BannerState::Current
        );
    }

    #[test]
    fn a_newer_release_during_the_offer_updates_the_offer() {
        assert_eq!(
            BannerState::Available(V1).apply(Event::Available(V2)),
            BannerState::Available(V2)
        );
    }

    #[test]
    fn ignored_release_stays_hidden_until_a_newer_release() {
        let ignored = BannerState::Available(V1).apply(Event::Ignored(V1));
        assert_eq!(ignored, BannerState::Ignored(V1));
        assert_eq!(
            ignored.clone().apply(Event::Available(V1)),
            BannerState::Ignored(V1)
        );
        assert_eq!(
            ignored.apply(Event::Available(V2)),
            BannerState::Available(V2)
        );
    }

    #[test]
    fn start_moves_offer_to_running_immediately_and_is_idempotent() {
        let running = BannerState::Available(V1).apply(Event::Start(V1));
        assert_eq!(running, BannerState::Running(Stage::Fetching, V1));
        assert_eq!(running.clone().apply(Event::Start(V1)), running);
    }

    #[test]
    fn periodic_check_does_not_disturb_a_running_install() {
        let running = BannerState::Running(Stage::Installing, V1);
        assert_eq!(running.clone().apply(Event::Available(V2)), running);
    }

    #[test]
    fn stages_progress_while_running() {
        assert_eq!(
            BannerState::Available(V1).apply(Event::Stage(Stage::Fetching)),
            BannerState::Running(Stage::Fetching, V1)
        );
        assert_eq!(
            BannerState::Running(Stage::Fetching, V1).apply(Event::Stage(Stage::Installing)),
            BannerState::Running(Stage::Installing, V1)
        );
    }

    #[test]
    fn done_and_failed_terminate_a_run() {
        assert_eq!(
            BannerState::Running(Stage::Installing, V1).apply(Event::Done(V1)),
            BannerState::Done(V1)
        );
        assert_eq!(
            BannerState::Running(Stage::Fetching, V1).apply(Event::Failed("boom".into())),
            BannerState::Failed("boom".into(), V1)
        );
    }

    #[test]
    fn done_ignores_re_announcement_of_the_installed_release() {
        assert_eq!(
            BannerState::Done(V1).apply(Event::Available(V1)),
            BannerState::Done(V1)
        );
        // …but a strictly newer one re-opens the offer.
        assert_eq!(
            BannerState::Done(V1).apply(Event::Available(V2)),
            BannerState::Available(V2)
        );
    }

    #[test]
    fn only_available_and_failed_are_actionable() {
        assert_eq!(BannerState::Available(V1).actionable_version(), Some(V1));
        assert_eq!(
            BannerState::Failed("e".into(), V1).actionable_version(),
            Some(V1)
        );
        assert_eq!(BannerState::Hidden.actionable_version(), None);
        assert_eq!(BannerState::Current.actionable_version(), None);
        assert_eq!(BannerState::Ignored(V1).actionable_version(), None);
        assert_eq!(
            BannerState::Running(Stage::Fetching, V1).actionable_version(),
            None
        );
        assert_eq!(BannerState::Done(V1).actionable_version(), None);
    }

    #[test]
    fn ignored_version_file_round_trips_and_rejects_invalid_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(IGNORED_VERSION_FILE);
        std::fs::write(&path, "v0.8.0\n").unwrap();
        assert_eq!(read_ignored_version(&path), Some(V2));

        std::fs::write(&path, "not-a-release\n").unwrap();
        assert_eq!(read_ignored_version(&path), None);
    }
}
