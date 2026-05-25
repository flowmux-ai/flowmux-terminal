// SPDX-License-Identifier: GPL-3.0-or-later
//! "Voice input" tab inside the options dialog.
//!
//! The widget tree built here mutates an [`AsrOptions`] in the
//! `RefCell` handed in by the dialog; the dialog reads it back when the
//! user clicks OK and persists it to `options.json`.
//!
//! Layout, top-to-bottom:
//!
//! * Enable switch + status line ("모델 미설치" / "준비됨").
//! * Model dropdown sourced from [`flowmux_asr::catalog`], paired with
//!   a "다운로드 / 삭제" button group and a `gtk::ProgressBar` that
//!   drives off [`flowmux_asr::DownloadEvent`].
//! * Language dropdown (`auto` + a curated list of Whisper languages).
//! * "Microphone permission" group with a button that runs
//!   [`flowmux_asr::audio::probe_microphone`] on a `gio::spawn_blocking`
//!   worker (the probe is synchronous so calling it from glib's main
//!   context directly would block the UI).
//! * Auto-Enter switch.
//! * PTT mode dropdown.

use adw::prelude::*;
use flowmux_asr::audio::{probe_microphone, MicProbeOutcome};
use flowmux_asr::catalog::{self, ModelEntry};
use flowmux_asr::download::{DownloadEvent, DownloadProgress};
use flowmux_asr::{ModelDownloader, ModelStore};
use flowmux_config::asr::{AsrLanguage, AsrOptions};
use gtk::gio;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

const SHORTCUT_HINT: &str = "단축키: Ctrl+Alt+Space (Keybindings 탭에서 변경)";

struct LanguageRow {
    code: &'static str,
    label: &'static str,
}

const LANGUAGES: &[LanguageRow] = &[
    LanguageRow {
        code: "auto",
        label: "자동 감지",
    },
    LanguageRow {
        code: "ko",
        label: "한국어",
    },
    LanguageRow {
        code: "en",
        label: "English",
    },
    LanguageRow {
        code: "ja",
        label: "日本語",
    },
    LanguageRow {
        code: "zh",
        label: "中文 (Mandarin)",
    },
    LanguageRow {
        code: "yue",
        label: "粵語 (Cantonese)",
    },
];

/// Build the Voice tab body.
///
/// `tokio_handle` is required for the model downloader; when it is
/// `None` (this should not happen in production) the download button
/// is shown but disabled with an explanatory label so the rest of the
/// tab still functions.
pub fn build(
    state: Rc<RefCell<AsrOptions>>,
    tokio_handle: Option<tokio::runtime::Handle>,
) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
    outer.set_margin_top(16);
    outer.set_margin_bottom(16);
    outer.set_margin_start(20);
    outer.set_margin_end(20);

    let intro = gtk::Label::new(Some(
        "마이크를 누르고 말하면 인식된 텍스트가 포커스된 터미널 pane에 입력됩니다. \
         모든 음성 처리는 디바이스 안에서 일어나며 외부 서버로 전송되지 않습니다.",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro.add_css_class("dim-label");
    outer.append(&intro);

    let enable_switch = build_enable_switch(state.clone());
    outer.append(&labelled("음성 입력 사용", &enable_switch));

    let shortcut_hint = gtk::Label::new(Some(SHORTCUT_HINT));
    shortcut_hint.set_wrap(true);
    shortcut_hint.set_xalign(0.0);
    shortcut_hint.add_css_class("dim-label");
    outer.append(&shortcut_hint);

    outer.append(&build_model_group(state.clone(), tokio_handle));

    let language_dropdown = build_language_dropdown(state.clone());
    outer.append(&labelled("언어", &language_dropdown));

    outer.append(&build_microphone_group(state.clone()));

    let device_dropdown = build_input_device_dropdown(state.clone());
    outer.append(&labelled("입력 장치", &device_dropdown));

    let gain_row = build_input_gain_row(state.clone());
    outer.append(&labelled("마이크 부스트", &gain_row));

    let auto_enter_switch = build_auto_enter_switch(state.clone());
    outer.append(&labelled("결과 끝에 Enter 자동 입력", &auto_enter_switch));

    outer
}

fn build_input_device_dropdown(state: Rc<RefCell<AsrOptions>>) -> gtk::DropDown {
    // First entry is "default" → input_device = None.
    let mut labels: Vec<String> = vec!["기본 (default)".into()];
    let devices = flowmux_asr::audio::enumerate_input_devices();
    labels.extend(devices.iter().cloned());
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let dropdown = gtk::DropDown::from_strings(&refs);
    let current = state.borrow().input_device.clone();
    let idx = match current {
        None => 0,
        Some(name) => devices
            .iter()
            .position(|d| d == &name)
            .map(|i| i + 1)
            .unwrap_or(0),
    };
    dropdown.set_selected(idx as u32);
    let devices_clone = devices.clone();
    dropdown.connect_selected_notify(move |d| {
        let i = d.selected() as usize;
        state.borrow_mut().input_device = if i == 0 {
            None
        } else {
            devices_clone.get(i - 1).cloned()
        };
    });
    dropdown
}

fn build_input_gain_row(state: Rc<RefCell<AsrOptions>>) -> gtk::Box {
    // Slider 1.0..=10.0 (1 decimal place) paired with a SpinButton.
    let adj = gtk::Adjustment::new(state.borrow().input_gain as f64, 1.0, 10.0, 0.1, 1.0, 0.0);
    let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adj));
    scale.set_hexpand(true);
    scale.set_draw_value(false);
    scale.set_round_digits(1);
    scale.add_mark(1.0, gtk::PositionType::Bottom, Some("1x"));
    scale.add_mark(3.0, gtk::PositionType::Bottom, Some("3x"));
    scale.add_mark(5.0, gtk::PositionType::Bottom, Some("5x"));
    let spin = gtk::SpinButton::new(Some(&adj), 0.1, 1);
    spin.set_numeric(true);
    spin.set_width_chars(5);
    {
        let state = state.clone();
        adj.connect_value_changed(move |a| {
            state.borrow_mut().input_gain = a.value() as f32;
        });
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_hexpand(true);
    row.append(&scale);
    row.append(&spin);
    row
}

fn labelled(text: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Start);
    row.append(&label);
    row.append(widget);
    row
}

fn build_enable_switch(state: Rc<RefCell<AsrOptions>>) -> gtk::Switch {
    let sw = gtk::Switch::new();
    sw.set_active(state.borrow().enabled);
    sw.connect_active_notify(move |s| {
        state.borrow_mut().enabled = s.is_active();
    });
    sw
}

fn build_auto_enter_switch(state: Rc<RefCell<AsrOptions>>) -> gtk::Switch {
    let sw = gtk::Switch::new();
    sw.set_active(state.borrow().auto_enter);
    sw.connect_active_notify(move |s| {
        state.borrow_mut().auto_enter = s.is_active();
    });
    sw
}

/// Model selection + download/remove controls, grouped because they
/// all reference the same selected entry.
fn build_model_group(
    state: Rc<RefCell<AsrOptions>>,
    tokio_handle: Option<tokio::runtime::Handle>,
) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 6);

    let title = gtk::Label::new(Some("모델"));
    title.set_xalign(0.0);
    title.add_css_class("heading");
    group.append(&title);

    let entries = catalog::entries();
    let labels: Vec<String> = entries.iter().map(|e| e.display.clone()).collect();
    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let dropdown = gtk::DropDown::from_strings(&label_refs);
    let current_id = state.borrow().active_model_id.clone();
    let idx = entries
        .iter()
        .position(|e| e.id.as_str() == current_id)
        .unwrap_or(0);
    dropdown.set_selected(idx as u32);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.add_css_class("dim-label");
    status.set_wrap(true);

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_visible(false);

    let download_btn = gtk::Button::with_label("다운로드");
    download_btn.add_css_class("suggested-action");

    let remove_btn = gtk::Button::with_label("삭제");
    remove_btn.add_css_class("destructive-action");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&dropdown);
    row.append(&download_btn);
    row.append(&remove_btn);
    group.append(&row);
    group.append(&progress);
    group.append(&status);

    // Seed model id when nothing was persisted yet so the OK button
    // saves whatever the dropdown currently shows.
    if state.borrow().active_model_id.is_empty() {
        if let Some(entry) = entries.get(idx) {
            state.borrow_mut().active_model_id = entry.id.as_str().to_string();
        }
    }

    refresh_status_and_buttons(&status, &download_btn, &remove_btn, entries.get(idx));

    let entries_for_change = entries.clone();
    {
        let state = state.clone();
        let status = status.clone();
        let download_btn = download_btn.clone();
        let remove_btn = remove_btn.clone();
        dropdown.connect_selected_notify(move |d| {
            let i = d.selected() as usize;
            if let Some(entry) = entries_for_change.get(i) {
                state.borrow_mut().active_model_id = entry.id.as_str().to_string();
                refresh_status_and_buttons(&status, &download_btn, &remove_btn, Some(entry));
            }
        });
    }

    // Download click: spawn the downloader on the tokio handle and
    // forward DownloadEvents through an async_channel that the GTK
    // main loop drains.
    {
        let entries = entries.clone();
        let dropdown = dropdown.clone();
        let status = status.clone();
        let progress = progress.clone();
        let download_btn = download_btn.clone();
        let remove_btn = remove_btn.clone();
        let tokio_handle = tokio_handle.clone();
        download_btn.clone().connect_clicked(move |_| {
            let Some(handle) = tokio_handle.clone() else {
                status.set_text("tokio runtime이 사용 불가능합니다.");
                return;
            };
            let i = dropdown.selected() as usize;
            let Some(entry) = entries.get(i).cloned() else {
                status.set_text("선택된 모델을 찾을 수 없습니다.");
                return;
            };
            let Some(store) = ModelStore::xdg_default() else {
                status.set_text("XDG 데이터 디렉터리가 없습니다.");
                return;
            };
            if let Err(e) = store.ensure_dir() {
                status.set_text(&format!("모델 디렉터리 생성 실패: {e}"));
                return;
            }

            download_btn.set_sensitive(false);
            remove_btn.set_sensitive(false);
            progress.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_text(Some("준비 중…"));
            status.set_text(&format!("'{}' 다운로드를 시작합니다…", entry.display));

            let downloader = ModelDownloader::new(store.clone());
            let mut rx = downloader.start(&handle, entry.clone());

            let (ui_tx, ui_rx) = async_channel::unbounded::<DownloadEvent>();
            handle.spawn(async move {
                while let Some(event) = rx.recv().await {
                    if ui_tx.send(event).await.is_err() {
                        break;
                    }
                }
            });

            let entries = entries.clone();
            let dropdown = dropdown.clone();
            let status = status.clone();
            let progress = progress.clone();
            let download_btn = download_btn.clone();
            let remove_btn = remove_btn.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = ui_rx.recv().await {
                    match event {
                        DownloadEvent::Started { total } => {
                            progress.set_fraction(0.0);
                            let label = match total {
                                Some(t) => format!("0 / {} MB", t / 1_000_000),
                                None => "다운로드 중…".into(),
                            };
                            progress.set_text(Some(&label));
                            status.set_text("다운로드 중…");
                        }
                        DownloadEvent::Progress(DownloadProgress {
                            bytes_received,
                            total,
                        }) => {
                            let ratio = total.map(|t| {
                                (bytes_received as f64 / t.max(1) as f64).clamp(0.0, 1.0)
                            });
                            if let Some(r) = ratio {
                                progress.set_fraction(r);
                            } else {
                                progress.pulse();
                            }
                            let label = match total {
                                Some(t) => format!(
                                    "{} / {} MB",
                                    bytes_received / 1_000_000,
                                    t / 1_000_000
                                ),
                                None => format!("{} MB", bytes_received / 1_000_000),
                            };
                            progress.set_text(Some(&label));
                        }
                        DownloadEvent::Extracting => {
                            progress.set_fraction(0.0);
                            progress.set_text(Some("압축 해제 중…"));
                            status.set_text("아카이브 압축 해제 중…");
                        }
                        DownloadEvent::ExtractProgress {
                            ratio,
                            current_entry,
                        } => {
                            progress.set_fraction(ratio);
                            let pct = (ratio * 100.0).round() as u32;
                            let label = match current_entry {
                                Some(name) => format!("압축 해제 {pct}% — {name}"),
                                None => format!("압축 해제 {pct}%"),
                            };
                            progress.set_text(Some(&label));
                        }
                        DownloadEvent::Finished { directory } => {
                            progress.set_fraction(1.0);
                            progress.set_text(Some("완료"));
                            status.set_text(&format!("설치 완료: {}", directory.display()));
                            let i = dropdown.selected() as usize;
                            refresh_status_and_buttons(
                                &status,
                                &download_btn,
                                &remove_btn,
                                entries.get(i),
                            );
                            progress.set_visible(false);
                        }
                        DownloadEvent::Failed(err) => {
                            progress.set_visible(false);
                            status.set_text(&format!("다운로드 실패: {err}"));
                            download_btn.set_sensitive(true);
                            remove_btn.set_sensitive(true);
                        }
                    }
                }
            });
        });
    }

    // Remove click: blow the verified .bin away. Confirm with the
    // status label rather than a dialog because the file is small
    // and the action is reversible (re-download).
    {
        let entries = entries.clone();
        let dropdown = dropdown.clone();
        let status = status.clone();
        let download_btn = download_btn.clone();
        let remove_btn = remove_btn.clone();
        remove_btn.clone().connect_clicked(move |_| {
            let i = dropdown.selected() as usize;
            let Some(entry) = entries.get(i) else {
                return;
            };
            let Some(store) = ModelStore::xdg_default() else {
                status.set_text("XDG 데이터 디렉터리가 없습니다.");
                return;
            };
            match store.remove(entry) {
                Ok(_) => {
                    status.set_text(&format!("'{}' 모델을 삭제했습니다.", entry.display));
                    refresh_status_and_buttons(&status, &download_btn, &remove_btn, Some(entry));
                }
                Err(e) => {
                    status.set_text(&format!("삭제 실패: {e}"));
                }
            }
        });
    }

    group
}

fn refresh_status_and_buttons(
    status: &gtk::Label,
    download_btn: &gtk::Button,
    remove_btn: &gtk::Button,
    entry: Option<&ModelEntry>,
) {
    let Some(entry) = entry else {
        status.set_text("");
        download_btn.set_sensitive(false);
        remove_btn.set_sensitive(false);
        return;
    };
    let mb = (entry.archive_size_bytes as f32 / 1_000_000.0).round();
    let installed = match ModelStore::xdg_default() {
        Some(store) => store.is_installed(entry),
        None => false,
    };
    let state = if installed { "설치됨" } else { "다운로드 필요" };
    status.set_text(&format!("크기: 약 {mb} MB · 상태: {state}"));
    download_btn.set_sensitive(!installed);
    remove_btn.set_sensitive(installed);
}

fn build_language_dropdown(state: Rc<RefCell<AsrOptions>>) -> gtk::DropDown {
    let labels: Vec<&str> = LANGUAGES.iter().map(|r| r.label).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    let current_code = state.borrow().language.as_code().to_string();
    let idx = LANGUAGES
        .iter()
        .position(|r| r.code == current_code)
        .unwrap_or(0);
    dropdown.set_selected(idx as u32);
    let state_clone = state.clone();
    dropdown.connect_selected_notify(move |d| {
        let i = d.selected() as usize;
        if let Some(row) = LANGUAGES.get(i) {
            let mut s = state_clone.borrow_mut();
            s.language = if row.code == "auto" {
                AsrLanguage::Auto
            } else {
                AsrLanguage::Code(row.code.into())
            };
        }
    });
    dropdown
}

fn build_microphone_group(state: Rc<RefCell<AsrOptions>>) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
    group.set_margin_top(8);

    let title = gtk::Label::new(Some("마이크 권한"));
    title.set_xalign(0.0);
    title.add_css_class("heading");
    group.append(&title);

    let hint = gtk::Label::new(Some(
        "[권한 요청 및 테스트] 버튼을 누르면 짧게 마이크를 열어 \
         권한 상태를 확인합니다. Flatpak 환경에서는 시스템 다이얼로그가 표시될 수 있습니다.",
    ));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.add_css_class("dim-label");
    group.append(&hint);

    let button = gtk::Button::with_label("권한 요청 및 테스트");
    button.set_halign(gtk::Align::Start);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);

    {
        let state = state.clone();
        let button = button.clone();
        let status = status.clone();
        button.clone().connect_clicked(move |_| {
            button.set_sensitive(false);
            status.remove_css_class("success");
            status.remove_css_class("error");
            status.set_text("마이크 확인 중…");
            let state = state.clone();
            let button = button.clone();
            let status = status.clone();
            let device = state.borrow().input_device.clone();
            // The probe is synchronous (it joins the cpal capture
            // thread); offload to a worker so the GTK main loop is
            // not blocked while the 250 ms capture runs.
            glib::MainContext::default().spawn_local(async move {
                let outcome = gio::spawn_blocking(move || probe_microphone(device))
                    .await
                    .unwrap_or_else(|_| MicProbeOutcome::Failed {
                        detail: "워커 스레드 오류".into(),
                    });
                let (msg, css_class) = match outcome {
                    MicProbeOutcome::Ok {
                        sample_rate,
                        channels,
                        captured_samples,
                    } => {
                        state.borrow_mut().mic_permission_acknowledged = true;
                        (
                            format!(
                                "마이크 접근 OK ({sample_rate} Hz, {channels} ch, 샘플 {captured_samples}개)"
                            ),
                            "success",
                        )
                    }
                    MicProbeOutcome::NoDevice => (
                        "입력 장치를 찾을 수 없습니다. 마이크가 연결되어 있는지 확인하세요.".into(),
                        "error",
                    ),
                    MicProbeOutcome::PermissionDenied { detail } => {
                        (format!("권한이 거부되었습니다: {detail}"), "error")
                    }
                    MicProbeOutcome::Failed { detail } => {
                        (format!("마이크 열기에 실패했습니다: {detail}"), "error")
                    }
                };
                status.add_css_class(css_class);
                status.set_text(&msg);
                button.set_sensitive(true);
            });
        });
    }

    group.append(&button);
    group.append(&status);
    group
}
