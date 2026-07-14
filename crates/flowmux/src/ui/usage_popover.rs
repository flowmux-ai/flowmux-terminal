// SPDX-License-Identifier: GPL-3.0-or-later

use crate::usage::{
    collect_all, format_token_count, FieldRefresh, Provider, ProviderRefresh, ProviderState,
    UsageError, UsageErrorKind, UsagePanelState, UsageWindow,
};
use chrono::{DateTime, Local, Utc};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct UsagePopover {
    button: gtk::MenuButton,
}

impl UsagePopover {
    pub(crate) fn new(tokio_handle: Option<tokio::runtime::Handle>) -> Self {
        let button = gtk::MenuButton::new();
        button.set_icon_name("utilities-system-monitor-symbolic");
        button.add_css_class("flat");
        button.add_css_class("flowmux-sidebar-options");
        button.set_tooltip_text(Some("AI usage"));
        button.set_focus_on_click(false);
        button.set_widget_name("flowmux-usage-button");

        let popover = gtk::Popover::new();
        popover.set_size_request(360, -1);
        button.set_popover(Some(&popover));
        popover.set_position(gtk::PositionType::Top);

        let state = Rc::new(RefCell::new(UsagePanelState::default()));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_margin_top(10);
        root.set_margin_bottom(10);
        root.set_margin_start(10);
        root.set_margin_end(10);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let title = gtk::Label::new(Some("AI Usage"));
        title.add_css_class("heading");
        title.set_halign(gtk::Align::Start);
        title.set_hexpand(true);
        header.append(&title);

        let refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh_button.add_css_class("flat");
        refresh_button.set_tooltip_text(Some("Refresh usage"));
        refresh_button.set_focus_on_click(false);
        refresh_button.set_widget_name("flowmux-usage-refresh-button");
        header.append(&refresh_button);

        let spinner = gtk::Spinner::new();
        spinner.set_tooltip_text(Some("Refreshing usage"));
        spinner.set_widget_name("flowmux-usage-refresh-spinner");
        header.append(&spinner);
        root.append(&header);

        let cards = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_min_content_height(180);
        scroll.set_max_content_height(520);
        scroll.set_propagate_natural_height(true);
        scroll.set_child(Some(&cards));
        root.append(&scroll);
        popover.set_child(Some(&root));
        render_usage(&cards, &refresh_button, &spinner, &state.borrow());

        let (result_tx, result_rx) = async_channel::bounded(1);
        let state_for_results = state.clone();
        let popover_weak = popover.downgrade();
        let cards_weak = cards.downgrade();
        let refresh_weak = refresh_button.downgrade();
        let spinner_weak = spinner.downgrade();
        gtk::glib::MainContext::default().spawn_local(async move {
            while let Ok(refreshes) = result_rx.recv().await {
                {
                    let mut state = state_for_results.borrow_mut();
                    for refresh in refreshes {
                        state.apply(refresh);
                    }
                    state.finish_refresh(Utc::now());
                }
                let Some(popover) = popover_weak.upgrade() else {
                    break;
                };
                if popover.is_visible() {
                    let (Some(cards), Some(refresh), Some(spinner)) = (
                        cards_weak.upgrade(),
                        refresh_weak.upgrade(),
                        spinner_weak.upgrade(),
                    ) else {
                        break;
                    };
                    render_usage(&cards, &refresh, &spinner, &state_for_results.borrow());
                }
            }
        });

        let state_for_show = state.clone();
        let result_tx_for_show = result_tx.clone();
        let handle_for_show = tokio_handle.clone();
        let cards_for_show = cards.clone();
        let refresh_for_show = refresh_button.clone();
        let spinner_for_show = spinner.clone();
        popover.connect_show(move |_| {
            request_refresh(
                &state_for_show,
                false,
                &handle_for_show,
                &result_tx_for_show,
            );
            render_usage(
                &cards_for_show,
                &refresh_for_show,
                &spinner_for_show,
                &state_for_show.borrow(),
            );
        });

        let state_for_refresh = state.clone();
        let result_tx_for_refresh = result_tx.clone();
        let handle_for_refresh = tokio_handle.clone();
        let cards_for_refresh = cards.clone();
        let spinner_for_refresh = spinner.clone();
        refresh_button.connect_clicked(move |button| {
            request_refresh(
                &state_for_refresh,
                true,
                &handle_for_refresh,
                &result_tx_for_refresh,
            );
            render_usage(
                &cards_for_refresh,
                button,
                &spinner_for_refresh,
                &state_for_refresh.borrow(),
            );
        });

        Self { button }
    }

    pub(crate) fn button(&self) -> &gtk::MenuButton {
        &self.button
    }
}

fn request_refresh(
    state: &Rc<RefCell<UsagePanelState>>,
    forced: bool,
    tokio_handle: &Option<tokio::runtime::Handle>,
    result_tx: &async_channel::Sender<[ProviderRefresh; 2]>,
) {
    let started = if forced {
        state.borrow_mut().begin_forced_refresh()
    } else {
        state.borrow_mut().begin_refresh(Utc::now())
    };
    if !started {
        return;
    }

    let Some(handle) = tokio_handle.clone() else {
        apply_local_failure(
            state,
            UsageError::new(UsageErrorKind::Io, "Could not start the usage collector."),
        );
        return;
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        apply_local_failure(
            state,
            UsageError::new(
                UsageErrorKind::NotLoggedIn,
                "The local home directory was not found.",
            ),
        );
        return;
    };
    let result_tx = result_tx.clone();
    handle.spawn(async move {
        let _ = result_tx.send(collect_all(home).await).await;
    });
}

fn apply_local_failure(state: &Rc<RefCell<UsagePanelState>>, error: UsageError) {
    let now = Utc::now();
    let refresh = |provider| ProviderRefresh {
        provider,
        tokens: FieldRefresh::Failure(error.clone()),
        limits: FieldRefresh::Failure(error.clone()),
        collected_at: now,
    };
    let mut state = state.borrow_mut();
    state.apply(refresh(Provider::Claude));
    state.apply(refresh(Provider::Codex));
    state.finish_refresh(now);
}

fn render_usage(
    cards: &gtk::Box,
    refresh_button: &gtk::Button,
    spinner: &gtk::Spinner,
    state: &UsagePanelState,
) {
    set_refresh_controls(refresh_button, spinner, state.refreshing);
    while let Some(child) = cards.first_child() {
        cards.remove(&child);
    }
    cards.append(&provider_card(&state.claude, state.refreshing));
    cards.append(&provider_card(&state.codex, state.refreshing));
}

fn set_refresh_controls(refresh_button: &gtk::Button, spinner: &gtk::Spinner, refreshing: bool) {
    refresh_button.set_sensitive(!refreshing);
    spinner.set_visible(refreshing);
    if refreshing {
        spinner.start();
    } else {
        spinner.stop();
    }
}

fn provider_card(state: &ProviderState, refreshing: bool) -> gtk::Widget {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let provider_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = gtk::Label::new(Some(provider_name(state.provider)));
    name.add_css_class("heading");
    name.set_halign(gtk::Align::Start);
    name.set_hexpand(true);
    provider_header.append(&name);
    if let Some(updated_at) = latest_update(state) {
        let updated = gtk::Label::new(Some(&format_updated_at(updated_at)));
        updated.add_css_class("caption");
        updated.add_css_class("dim-label");
        provider_header.append(&updated);
    }
    content.append(&provider_header);

    let error_messages: HashSet<&str> = [state.token_error.as_ref(), state.limits_error.as_ref()]
        .into_iter()
        .flatten()
        .map(|error| error.message.as_str())
        .collect();
    let text_rows = provider_text_rows(state);
    for text in &text_rows {
        let label = gtk::Label::new(Some(text));
        label.set_halign(gtk::Align::Start);
        label.set_wrap(true);
        if error_messages.contains(text.as_str())
            || text.starts_with("Token data last updated ")
            || text.starts_with("Rate limits last updated ")
        {
            label.add_css_class("caption");
            label.add_css_class("dim-label");
        }
        content.append(&label);
    }

    if let Some(limits) = &state.limits {
        if limits.value.is_empty() {
            let empty = gtk::Label::new(Some("No rate limit data"));
            empty.set_halign(gtk::Align::Start);
            empty.add_css_class("caption");
            empty.add_css_class("dim-label");
            content.append(&empty);
        } else {
            for window in &limits.value {
                content.append(&limit_row(window));
            }
        }
    }

    if text_rows.is_empty() && state.limits.is_none() {
        let empty = gtk::Label::new(Some(if refreshing {
            "Loading usage…"
        } else {
            "No usage data"
        }));
        empty.set_halign(gtk::Align::Start);
        empty.add_css_class("dim-label");
        content.append(&empty);
    }

    row.set_child(Some(&content));
    list.append(&row);
    list.upcast()
}

fn limit_row(window: &UsageWindow) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 3);
    root.set_margin_top(2);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let mut title_text = window.label.clone();
    if let Some(scope) = &window.scope {
        title_text.push_str(" · ");
        title_text.push_str(scope);
    }
    let title = gtk::Label::new(Some(&title_text));
    title.set_halign(gtk::Align::Start);
    title.set_hexpand(true);
    header.append(&title);
    let percent = gtk::Label::new(Some(&percent_label(window.used_percent)));
    percent.add_css_class("numeric");
    header.append(&percent);
    root.append(&header);

    let progress = gtk::ProgressBar::new();
    progress.set_fraction(progress_fraction(window.used_percent));
    progress.set_hexpand(true);
    root.append(&progress);

    if let Some(resets_at) = window.resets_at {
        let reset = gtk::Label::new(Some(&format_reset_at(resets_at)));
        reset.set_halign(gtk::Align::Start);
        reset.add_css_class("caption");
        reset.add_css_class("dim-label");
        root.append(&reset);
    }
    root.upcast()
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}

fn provider_text_rows(state: &ProviderState) -> Vec<String> {
    let mut rows = Vec::new();
    if let Some(tokens) = &state.tokens {
        if let Some(today) = tokens.value.today {
            rows.push(format!("Tokens today {}", format_token_count(today)));
        }
        if let Some(lifetime) = tokens.value.lifetime {
            rows.push(format!("Lifetime tokens {}", format_token_count(lifetime)));
        }
        if state.token_error.is_some() {
            rows.push(format_field_updated_at("Token data", tokens.updated_at));
        }
    }
    if let Some(limits) = &state.limits {
        if state.limits_error.is_some() {
            rows.push(format_field_updated_at("Rate limits", limits.updated_at));
        }
    }
    let mut seen_errors = HashSet::new();
    for error in [state.token_error.as_ref(), state.limits_error.as_ref()]
        .into_iter()
        .flatten()
    {
        if seen_errors.insert(error.message.as_str()) {
            rows.push(error.message.clone());
        }
    }
    rows
}

fn format_field_updated_at(label: &str, value: DateTime<Utc>) -> String {
    let local: DateTime<Local> = value.into();
    format!("{label} last updated {}", local.format("%H:%M"))
}

fn latest_update(state: &ProviderState) -> Option<DateTime<Utc>> {
    state
        .tokens
        .as_ref()
        .map(|value| value.updated_at)
        .into_iter()
        .chain(state.limits.as_ref().map(|value| value.updated_at))
        .max()
}

fn format_updated_at(value: DateTime<Utc>) -> String {
    let local: DateTime<Local> = value.into();
    format!("Updated {}", local.format("%H:%M"))
}

fn format_reset_at(value: DateTime<Utc>) -> String {
    let local: DateTime<Local> = value.into();
    if local.date_naive() == Local::now().date_naive() {
        format!("Resets today at {}", local.format("%H:%M"))
    } else {
        format!("Resets at {}", local.format("%m/%d %H:%M"))
    }
}

fn progress_fraction(percent: f64) -> f64 {
    if percent.is_finite() {
        (percent / 100.0).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn percent_label(percent: f64) -> String {
    if !percent.is_finite() {
        return "—".into();
    }
    if (percent - percent.round()).abs() < 0.05 {
        format!("{percent:.0}%")
    } else {
        format!("{percent:.1}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{
        Provider, ProviderState, Timestamped, TokenTotals, UsageError, UsageErrorKind,
    };
    use chrono::Utc;

    #[test]
    fn progress_fraction_clamps_without_changing_numeric_label() {
        assert_eq!(progress_fraction(-5.0), 0.0);
        assert_eq!(progress_fraction(42.0), 0.42);
        assert_eq!(progress_fraction(120.0), 1.0);
        assert_eq!(percent_label(120.0), "120%");
        assert_eq!(percent_label(42.5), "42.5%");
    }

    #[test]
    fn stale_error_keeps_value_and_explains_refresh_failure() {
        let state = ProviderState {
            provider: Provider::Claude,
            tokens: Some(Timestamped {
                value: TokenTotals {
                    today: Some(12_500),
                    lifetime: None,
                },
                updated_at: Utc::now(),
            }),
            limits: None,
            token_error: Some(UsageError::new(
                UsageErrorKind::Unauthorized,
                "Run Claude once to refresh the local login.",
            )),
            limits_error: None,
        };

        let rows = provider_text_rows(&state);

        assert!(rows.iter().any(|row| row.contains("Tokens today 12.5K")));
        assert!(rows.iter().any(|row| row.contains("Run Claude once")));
    }

    #[test]
    fn partial_failure_marks_only_the_stale_field_timestamp() {
        let old = Utc::now() - chrono::Duration::minutes(5);
        let fresh = Utc::now();
        let mut panel = UsagePanelState::default();
        panel.apply(ProviderRefresh {
            provider: Provider::Claude,
            tokens: FieldRefresh::Success(TokenTotals {
                today: Some(12_500),
                lifetime: None,
            }),
            limits: FieldRefresh::Success(Vec::new()),
            collected_at: old,
        });
        panel.apply(ProviderRefresh {
            provider: Provider::Claude,
            tokens: FieldRefresh::Failure(UsageError::network()),
            limits: FieldRefresh::Success(Vec::new()),
            collected_at: fresh,
        });

        let rows = provider_text_rows(&panel.claude);

        assert!(rows
            .iter()
            .any(|row| row.starts_with("Token data last updated ")));
        assert!(!rows
            .iter()
            .any(|row| row.starts_with("Rate limits last updated ")));
    }

    #[gtk::test]
    fn menu_button_owns_a_wide_upward_popover() {
        if gtk::init().is_err() {
            return;
        }

        let usage = UsagePopover::new(None);
        let popover = usage
            .button()
            .popover()
            .unwrap()
            .downcast::<gtk::Popover>()
            .unwrap();

        assert_eq!(
            usage.button().icon_name().as_deref(),
            Some("utilities-system-monitor-symbolic")
        );
        assert_eq!(popover.position(), gtk::PositionType::Top);
        assert_eq!(popover.width_request(), 360);

        let root = popover.child().unwrap();
        let refresh = find_named_widget(&root, "flowmux-usage-refresh-button")
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        let spinner = find_named_widget(&root, "flowmux-usage-refresh-spinner")
            .unwrap()
            .downcast::<gtk::Spinner>()
            .unwrap();
        assert_eq!(
            refresh.icon_name().as_deref(),
            Some("view-refresh-symbolic")
        );
        assert_eq!(refresh.tooltip_text().as_deref(), Some("Refresh usage"));
        set_refresh_controls(&refresh, &spinner, true);
        assert!(!refresh.is_sensitive());
        assert!(spinner.is_spinning());
    }

    #[gtk::test]
    fn dropping_usage_popover_releases_refresh_widgets() {
        if gtk::init().is_err() {
            return;
        }

        let usage = UsagePopover::new(None);
        let refresh_weak = {
            let popover = usage.button().popover().unwrap();
            let root = popover.child().unwrap();
            find_named_widget(&root, "flowmux-usage-refresh-button")
                .unwrap()
                .downgrade()
        };

        drop(usage);
        let context = gtk::glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }

        assert!(
            refresh_weak.upgrade().is_none(),
            "the refresh receiver must not retain the widget graph"
        );
    }

    fn find_named_widget(root: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
        if root.widget_name() == name {
            return Some(root.clone());
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(found) = find_named_widget(&widget, name) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }
}
