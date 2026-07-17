// SPDX-License-Identifier: GPL-3.0-or-later
//! Side-panel banner for self-update. Renders [`BannerState`] into an
//! `adw::Banner` pinned above the side panel footer, spawns the
//! periodic release check, and starts the install when the user asks.

use crate::update::{self, BannerState, Event};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Widget properties for a banner state:
/// `(title, button_label, revealed, progress_visible, ignore_visible)`.
/// `None` for the button label hides the button. Pure so the mapping
/// stays unit-testable without GTK.
fn banner_props(state: &BannerState) -> (String, Option<&'static str>, bool, bool, bool) {
    use crate::update::Stage;
    match state {
        BannerState::Hidden | BannerState::Current | BannerState::Ignored(_) => {
            (String::new(), None, false, false, false)
        }
        BannerState::Available(v) => (
            format!("FlowMux {v} is available"),
            Some("Update"),
            true,
            false,
            true,
        ),
        BannerState::Running(Stage::Fetching, v) => (
            format!("Updating to {v} — downloading…"),
            None,
            true,
            true,
            false,
        ),
        BannerState::Running(Stage::Installing, v) => (
            format!("Updating to {v} — building & installing…"),
            None,
            true,
            true,
            false,
        ),
        BannerState::Done(v) => (
            format!("FlowMux {v} is installed. Restart FlowMux to use it."),
            Some("Dismiss"),
            true,
            false,
            false,
        ),
        BannerState::Failed(message, v) => (
            format!("Update to {v} failed: {message} (see ~/.cache/flowmux/update.log)"),
            Some("Retry"),
            true,
            false,
            false,
        ),
    }
}

#[derive(Clone)]
pub struct UpdateBanner {
    root: gtk::Box,
    banner: adw::Banner,
    ignore_button: gtk::Button,
    progress: gtk::ProgressBar,
    state: Rc<RefCell<BannerState>>,
    tx: async_channel::Sender<Event>,
    tokio_handle: Option<tokio::runtime::Handle>,
}

impl UpdateBanner {
    /// Build the (hidden) banner and start the release check on
    /// `tokio_handle`. Without a handle — tests, degraded startup —
    /// the banner stays permanently hidden.
    pub fn new(tokio_handle: Option<tokio::runtime::Handle>) -> Self {
        let banner = adw::Banner::new("");
        banner.set_revealed(false);
        banner.set_hexpand(true);
        banner.set_widget_name("flowmux-update-banner");
        let ignore_button = gtk::Button::with_label("Not now");
        ignore_button.add_css_class("flat");
        ignore_button.set_tooltip_text(Some(
            "Hide this release prompt. You can update later in Options.",
        ));
        ignore_button.set_widget_name("flowmux-update-ignore-button");
        ignore_button.set_visible(false);
        ignore_button.set_valign(gtk::Align::Center);
        ignore_button.set_margin_end(6);
        let progress = gtk::ProgressBar::new();
        progress.set_hexpand(true);
        progress.set_margin_start(12);
        progress.set_margin_end(12);
        progress.set_margin_bottom(6);
        progress.set_pulse_step(0.08);
        progress.set_visible(false);

        let banner_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        banner_row.append(&banner);
        banner_row.append(&ignore_button);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&banner_row);
        root.append(&progress);

        let (tx, rx) = async_channel::unbounded::<Event>();
        let this = Self {
            root,
            banner: banner.clone(),
            ignore_button: ignore_button.clone(),
            progress,
            state: Rc::new(RefCell::new(
                update::ignored_version()
                    .map(BannerState::Ignored)
                    .unwrap_or(BannerState::Hidden),
            )),
            tx,
            tokio_handle: tokio_handle.clone(),
        };

        if let Some(handle) = &tokio_handle {
            handle.spawn(update::install::check_loop(this.tx.clone()));
        }

        let for_events = this.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = rx.recv().await {
                for_events.dispatch(event);
            }
        });

        let for_click = this.clone();
        banner.connect_button_clicked(move |banner| {
            let actionable = for_click.state.borrow().actionable_version();
            match actionable {
                Some(version) => {
                    for_click.start_install(version);
                }
                // Done state: the button is "Dismiss". Keep the state —
                // BannerState::Done ignores re-announcements of the
                // installed release, so the banner stays dormant until
                // a strictly newer tag appears.
                None => banner.set_revealed(false),
            }
        });

        let for_ignore = this.clone();
        ignore_button.connect_clicked(move |_| {
            let version = match *for_ignore.state.borrow() {
                BannerState::Available(version) => version,
                _ => return,
            };
            if let Err(error) = update::persist_ignored_version(version) {
                tracing::warn!(%error, %version, "failed to persist ignored update version");
            }
            for_ignore.dispatch(Event::Ignored(version));
        });

        this
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub(crate) fn state(&self) -> BannerState {
        self.state.borrow().clone()
    }

    /// Run a release check immediately and report the resulting shared state
    /// back on the GTK main context. This is used by Options > Update while the
    /// periodic startup check continues to use the banner's event channel.
    pub(crate) fn check_now(
        &self,
        on_complete: Box<dyn FnOnce(Result<BannerState, String>)>,
    ) -> bool {
        let Some(handle) = &self.tokio_handle else {
            return false;
        };
        let (result_tx, result_rx) = async_channel::bounded(1);
        handle.spawn(async move {
            let result = update::install::check_once()
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = result_tx.send(result).await;
        });

        let this = self.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let result = match result_rx.recv().await {
                Ok(Ok(Some(version))) => {
                    this.dispatch(Event::Available(version));
                    Ok(this.state())
                }
                Ok(Ok(None)) => {
                    this.dispatch(Event::Current);
                    Ok(this.state())
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(format!("release check stopped: {error}")),
            };
            on_complete(result);
        });
        true
    }

    /// Start `version` through the same progress channel used by the banner.
    /// Returns `false` when no runtime is available or that release is already
    /// installing/installed, allowing other UI entry points to avoid duplicates.
    pub(crate) fn start_install(&self, version: update::check::Version) -> bool {
        let Some(handle) = &self.tokio_handle else {
            return false;
        };
        let next = self.state.borrow().clone().apply(Event::Start(version));
        if next == *self.state.borrow() {
            return false;
        }
        self.render(next);
        handle.spawn(update::install::run_install(version, self.tx.clone()));
        true
    }

    fn dispatch(&self, event: Event) {
        let was_running = matches!(*self.state.borrow(), BannerState::Running(..));
        let next = self.state.borrow().clone().apply(event);
        self.render_with_previous_running(next, was_running);
    }

    fn render(&self, next: BannerState) {
        let was_running = matches!(*self.state.borrow(), BannerState::Running(..));
        self.render_with_previous_running(next, was_running);
    }

    fn render_with_previous_running(&self, next: BannerState, was_running: bool) {
        let (title, button, revealed, progress_visible, ignore_visible) = banner_props(&next);
        self.banner.set_title(&title);
        self.banner.set_button_label(button);
        self.banner.set_revealed(revealed);
        self.ignore_button.set_visible(ignore_visible);
        self.progress.set_visible(progress_visible);
        *self.state.borrow_mut() = next;
        if progress_visible && !was_running {
            self.start_progress_pulse();
        }
    }

    fn start_progress_pulse(&self) {
        self.progress.set_fraction(0.0);
        self.progress.pulse();
        let progress = self.progress.downgrade();
        let state = Rc::clone(&self.state);
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
            if !matches!(*state.borrow(), BannerState::Running(..)) {
                return gtk::glib::ControlFlow::Break;
            }
            let Some(progress) = progress.upgrade() else {
                return gtk::glib::ControlFlow::Break;
            };
            progress.pulse();
            gtk::glib::ControlFlow::Continue
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{check::Version, Stage};

    const V: Version = Version(0, 8, 0);

    #[test]
    fn hidden_state_reveals_nothing() {
        assert_eq!(
            banner_props(&BannerState::Hidden),
            (String::new(), None, false, false, false)
        );
    }

    #[test]
    fn current_state_reveals_nothing() {
        assert_eq!(
            banner_props(&BannerState::Current),
            (String::new(), None, false, false, false)
        );
    }

    #[test]
    fn ignored_state_reveals_nothing() {
        assert_eq!(
            banner_props(&BannerState::Ignored(V)),
            (String::new(), None, false, false, false)
        );
    }

    #[test]
    fn available_offers_the_update_button() {
        let (title, button, revealed, progress, ignore) = banner_props(&BannerState::Available(V));
        assert!(
            title.contains("0.8.0"),
            "title should name the release: {title}"
        );
        assert_eq!(button, Some("Update"));
        assert!(revealed);
        assert!(!progress);
        assert!(ignore);
    }

    #[test]
    fn running_shows_the_stage_and_no_button() {
        for (stage, needle) in [
            (Stage::Fetching, "downloading"),
            (Stage::Installing, "installing"),
        ] {
            let (title, button, revealed, progress, ignore) =
                banner_props(&BannerState::Running(stage, V));
            assert!(title.contains(needle), "{title} should mention {needle}");
            assert_eq!(button, None, "no button while running");
            assert!(revealed);
            assert!(progress, "progress should be visible while running");
            assert!(!ignore);
        }
    }

    #[test]
    fn done_announces_next_launch_and_dismisses() {
        let (title, button, revealed, progress, ignore) = banner_props(&BannerState::Done(V));
        assert!(title.contains("Restart FlowMux"), "{title}");
        assert_eq!(button, Some("Dismiss"));
        assert!(revealed);
        assert!(!progress);
        assert!(!ignore);
    }

    #[test]
    fn failed_offers_retry_and_points_at_the_log() {
        let (title, button, revealed, progress, ignore) =
            banner_props(&BannerState::Failed("git fetch exited with 128".into(), V));
        assert!(title.contains("git fetch exited with 128"), "{title}");
        assert!(
            title.contains("update.log"),
            "{title} should point at the log"
        );
        assert_eq!(button, Some("Retry"));
        assert!(revealed);
        assert!(!progress);
        assert!(!ignore);
    }
}
