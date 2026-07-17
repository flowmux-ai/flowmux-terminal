// SPDX-License-Identifier: GPL-3.0-or-later
//! Side-panel banner for self-update. Renders [`BannerState`] into an
//! `adw::Banner` pinned above the side panel footer, spawns the
//! periodic release check, and starts the install when the user asks.

use crate::update::{self, BannerState, Event};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Widget properties for a banner state:
/// `(title, button_label, revealed, progress_visible)`.
/// `None` for the button label hides the button. Pure so the mapping
/// stays unit-testable without GTK.
fn banner_props(state: &BannerState) -> (String, Option<&'static str>, bool, bool) {
    use crate::update::Stage;
    match state {
        BannerState::Hidden => (String::new(), None, false, false),
        BannerState::Available(v) => (
            format!("FlowMux {v} is available"),
            Some("Update"),
            true,
            false,
        ),
        BannerState::Running(Stage::Fetching, v) => {
            (format!("Updating to {v} — downloading…"), None, true, true)
        }
        BannerState::Running(Stage::Installing, v) => (
            format!("Updating to {v} — building & installing…"),
            None,
            true,
            true,
        ),
        BannerState::Done(v) => (
            format!("FlowMux {v} is installed. Restart FlowMux to use it."),
            Some("Dismiss"),
            true,
            false,
        ),
        BannerState::Failed(message, v) => (
            format!("Update to {v} failed: {message} (see ~/.cache/flowmux/update.log)"),
            Some("Retry"),
            true,
            false,
        ),
    }
}

#[derive(Clone)]
pub struct UpdateBanner {
    root: gtk::Box,
    banner: adw::Banner,
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
        let progress = gtk::ProgressBar::new();
        progress.set_hexpand(true);
        progress.set_margin_start(12);
        progress.set_margin_end(12);
        progress.set_margin_bottom(6);
        progress.set_pulse_step(0.08);
        progress.set_visible(false);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&banner);
        root.append(&progress);

        let (tx, rx) = async_channel::unbounded::<Event>();
        let this = Self {
            root,
            banner: banner.clone(),
            progress,
            state: Rc::new(RefCell::new(BannerState::Hidden)),
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
                    if let Some(handle) = &for_click.tokio_handle {
                        handle.spawn(update::install::run_install(version, for_click.tx.clone()));
                    }
                }
                // Done state: the button is "Dismiss". Keep the state —
                // BannerState::Done ignores re-announcements of the
                // installed release, so the banner stays dormant until
                // a strictly newer tag appears.
                None => banner.set_revealed(false),
            }
        });

        this
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn dispatch(&self, event: Event) {
        let was_running = matches!(*self.state.borrow(), BannerState::Running(..));
        let next = self.state.borrow().clone().apply(event);
        let (title, button, revealed, progress_visible) = banner_props(&next);
        self.banner.set_title(&title);
        self.banner.set_button_label(button);
        self.banner.set_revealed(revealed);
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
            (String::new(), None, false, false)
        );
    }

    #[test]
    fn available_offers_the_update_button() {
        let (title, button, revealed, progress) = banner_props(&BannerState::Available(V));
        assert!(
            title.contains("0.8.0"),
            "title should name the release: {title}"
        );
        assert_eq!(button, Some("Update"));
        assert!(revealed);
        assert!(!progress);
    }

    #[test]
    fn running_shows_the_stage_and_no_button() {
        for (stage, needle) in [
            (Stage::Fetching, "downloading"),
            (Stage::Installing, "installing"),
        ] {
            let (title, button, revealed, progress) = banner_props(&BannerState::Running(stage, V));
            assert!(title.contains(needle), "{title} should mention {needle}");
            assert_eq!(button, None, "no button while running");
            assert!(revealed);
            assert!(progress, "progress should be visible while running");
        }
    }

    #[test]
    fn done_announces_next_launch_and_dismisses() {
        let (title, button, revealed, progress) = banner_props(&BannerState::Done(V));
        assert!(title.contains("Restart FlowMux"), "{title}");
        assert_eq!(button, Some("Dismiss"));
        assert!(revealed);
        assert!(!progress);
    }

    #[test]
    fn failed_offers_retry_and_points_at_the_log() {
        let (title, button, revealed, progress) =
            banner_props(&BannerState::Failed("git fetch exited with 128".into(), V));
        assert!(title.contains("git fetch exited with 128"), "{title}");
        assert!(
            title.contains("update.log"),
            "{title} should point at the log"
        );
        assert_eq!(button, Some("Retry"));
        assert!(revealed);
        assert!(!progress);
    }
}
