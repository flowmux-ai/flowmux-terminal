// SPDX-License-Identifier: GPL-3.0-or-later
//! Push-to-talk controller — Hold-mode only.
//!
//! The controller decouples capture from engine availability so the
//! user can press Ctrl+Alt+Space immediately at app start without
//! waiting for SenseVoice to finish loading. Concretely:
//!
//! * The streaming engine is stored behind
//!   `Arc<Mutex<Option<Arc<SenseVoiceEngine>>>>` and populated
//!   asynchronously by `schedule_preload`.
//! * `start()` opens the cpal capture immediately — no engine
//!   dependency.
//! * The streaming-partial pump and the finish-time transcription
//!   poll the cell, picking up the engine as soon as the background
//!   preload completes. Audio captured during the wait is fed in
//!   without loss.
//!
//! The Toggle mode the crate previously shipped was removed at the
//! user's request; the EventControllerKey installed here drives a
//! single press-to-record / release-to-transcribe loop.

use crate::keybindings::{FocusedPane, TerminalRegistry};
use crate::ui::window::ClipboardToast;
use adw::prelude::*;
use flowmux_asr::catalog::{self, ModelEntry};
use flowmux_asr::engine::load_engine;
use flowmux_asr::session::{clean_asr_artifacts, sanitize_for_pty, PttSession, SessionConfig};
use flowmux_asr::{ModelStore, SenseVoiceEngine};
use flowmux_config::asr::AsrOptions;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Convenience alias for the engine slot shared between the
/// controller, the preload worker, and the per-session pump / finish
/// workers.
type EngineCell = Arc<Mutex<Option<Arc<SenseVoiceEngine>>>>;

#[derive(Clone)]
pub enum AsrUiEvent {
    Recording,
    Partial { delta: String },
    Transcribing,
    Done { delta: String },
    DroppedTooShort { seconds: f32 },
    Failed(String),
    Cancelled,
    EnginePreloaded,
}

pub struct AsrController {
    options: AsrOptions,
    runtime: tokio::runtime::Handle,
    store: ModelStore,
    /// Shared engine slot. Populated by `schedule_preload`; consumed
    /// by pump / finish workers via poll.
    engine_cell: EngineCell,
    /// Signature of the engine currently loaded into `engine_cell`.
    /// Used to detect when a settings change requires a reload.
    engine_signature: Arc<Mutex<Option<String>>>,
    /// True once a preload spawn_blocking is in flight; prevents
    /// duplicate loads.
    preload_in_flight: Arc<AtomicBool>,
    session: Option<PttSession>,
    started_at: Option<std::time::Instant>,
    pump_cancel: Option<Arc<AtomicBool>>,
    finish_cancel: Option<Arc<AtomicBool>>,
    last_injection: Arc<Mutex<String>>,
    event_tx: async_channel::Sender<AsrUiEvent>,
    event_rx: Option<async_channel::Receiver<AsrUiEvent>>,
    focused: FocusedPane,
    registry: TerminalRegistry,
    clipboard_toast: ClipboardToast,
}

pub type AsrControllerHandle = Rc<RefCell<AsrController>>;

impl AsrController {
    pub fn new(
        options: AsrOptions,
        runtime: tokio::runtime::Handle,
        focused: FocusedPane,
        registry: TerminalRegistry,
        clipboard_toast: ClipboardToast,
    ) -> AsrControllerHandle {
        let (event_tx, event_rx) = async_channel::unbounded::<AsrUiEvent>();
        let store = ModelStore::xdg_default().unwrap_or_else(|| {
            ModelStore::new(std::env::temp_dir().join("flowmux-asr-models"))
        });
        let _ = store.ensure_dir();
        Rc::new(RefCell::new(Self {
            options,
            runtime,
            store,
            engine_cell: Arc::new(Mutex::new(None)),
            engine_signature: Arc::new(Mutex::new(None)),
            preload_in_flight: Arc::new(AtomicBool::new(false)),
            session: None,
            started_at: None,
            pump_cancel: None,
            finish_cancel: None,
            last_injection: Arc::new(Mutex::new(String::new())),
            event_tx,
            event_rx: Some(event_rx),
            focused,
            registry,
            clipboard_toast,
        }))
    }

    pub fn take_event_receiver(&mut self) -> Option<async_channel::Receiver<AsrUiEvent>> {
        self.event_rx.take()
    }

    pub fn options(&self) -> &AsrOptions {
        &self.options
    }

    pub fn replace_options(&mut self, options: AsrOptions) {
        let model_changed = options.active_model_id != self.options.active_model_id;
        let language_changed = options.language.as_code() != self.options.language.as_code();
        if model_changed || language_changed {
            *self.engine_cell.lock().unwrap() = None;
            *self.engine_signature.lock().unwrap() = None;
        }
        self.options = options;
    }

    pub fn is_recording(&self) -> bool {
        self.session.is_some()
    }

    /// Trigger the background engine load. No-op when an engine is
    /// already in the cell with the right signature, or when a
    /// preload is already in flight.
    pub fn schedule_preload(&self) {
        // Resolve the active entry via the fallback first — a
        // persisted `active_model_id` from the whisper era would
        // make `is_ready_to_record` return false even though
        // SenseVoice was downloaded and `start()` would still pick
        // it up through `active_entry`.
        if !self.options.enabled {
            return;
        }
        let Some(entry) = self.active_entry() else {
            return;
        };
        if !self.store.is_installed(&entry) {
            return;
        }
        // Honour the user's literal language setting — `Auto` means
        // "let SenseVoice ID the language" and the picked code wins
        // whenever the user explicitly chose one.
        let lang_code = self.options.language.as_code().to_string();
        let target_signature = format!("{}::{}", entry.id.as_str(), lang_code);
        if self.engine_signature.lock().unwrap().as_deref() == Some(target_signature.as_str()) {
            return;
        }
        if self.preload_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        let store = self.store.clone();
        let cell = self.engine_cell.clone();
        let sig_cell = self.engine_signature.clone();
        let in_flight = self.preload_in_flight.clone();
        let tx = self.event_tx.clone();
        let signature = target_signature;
        self.runtime.spawn_blocking(move || {
            eprintln!(
                "[flowmux-asr] preloading engine for '{}' (lang={})",
                entry.id.as_str(),
                lang_code
            );
            let started = std::time::Instant::now();
            let mut effective = entry.clone();
            effective.language = lang_code;
            match load_engine(&effective, &store) {
                Ok(engine) => {
                    eprintln!(
                        "[flowmux-asr] engine preloaded in {:.2}s",
                        started.elapsed().as_secs_f32()
                    );
                    *cell.lock().unwrap() = Some(engine);
                    *sig_cell.lock().unwrap() = Some(signature);
                    let _ = tx.send_blocking(AsrUiEvent::EnginePreloaded);
                }
                Err(e) => {
                    eprintln!("[flowmux-asr] engine preload failed: {e}");
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    pub fn is_ready_to_record(&self) -> bool {
        if !self.options.enabled {
            return false;
        }
        let Some(entry) = self.active_entry() else {
            return false;
        };
        self.store.is_installed(&entry)
    }

    /// Hold-mode press: start capture and pump.
    pub fn start(&mut self) -> Result<(), ()> {
        if self.is_recording() {
            return Ok(());
        }
        if let Some(cancel) = self.finish_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(cancel) = self.pump_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if !self.options.is_ready() {
            self.emit(AsrUiEvent::Failed(
                "음성 입력이 비활성화되어 있거나 모델이 선택되지 않았습니다.".into(),
            ));
            return Err(());
        }
        let Some(entry) = self.active_entry() else {
            self.emit(AsrUiEvent::Failed("선택된 모델을 찾을 수 없습니다.".into()));
            return Err(());
        };
        if !self.store.is_installed(&entry) {
            self.emit(AsrUiEvent::Failed(
                "모델이 디스크에 설치되지 않았습니다. 설정에서 다운로드를 진행하세요.".into(),
            ));
            return Err(());
        }
        // Kick the engine load if it has not started yet. Capture
        // begins regardless — the recogniser is only required at
        // finish time and pump-tick time.
        self.schedule_preload();
        let session_config = SessionConfig {
            device_name: self.options.input_device.clone(),
            max_duration: Duration::from_secs(self.options.max_seconds as u64),
            auto_enter: self.options.auto_enter,
            min_duration: Duration::from_millis(150),
            input_gain: self.options.input_gain,
        };
        let mut session = PttSession::new(session_config);
        if let Err(e) = session.start() {
            self.emit(AsrUiEvent::Failed(format!("녹음 시작 실패: {e}")));
            return Err(());
        }
        {
            let mut last = self.last_injection.lock().unwrap();
            last.clear();
        }
        if let Some(buffer_arc) = session.buffer_arc() {
            let cancel = Arc::new(AtomicBool::new(false));
            self.pump_cancel = Some(cancel.clone());
            self.spawn_partial_pump(buffer_arc, cancel);
        }
        self.session = Some(session);
        self.started_at = Some(std::time::Instant::now());
        self.emit(AsrUiEvent::Recording);
        Ok(())
    }

    fn spawn_partial_pump(
        &self,
        buffer_arc: Arc<Mutex<flowmux_asr::audio::capture::PcmBuffer>>,
        cancel: Arc<AtomicBool>,
    ) {
        let tx = self.event_tx.clone();
        let last_injection = self.last_injection.clone();
        let engine_cell = self.engine_cell.clone();
        let gain = self.options.input_gain.clamp(1.0, 30.0);
        const TICK: Duration = Duration::from_millis(1500);
        const MIN_SECONDS: f32 = 0.6;
        self.runtime.spawn(async move {
            eprintln!("[flowmux-asr] pump: started");
            tokio::time::sleep(TICK).await;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    eprintln!("[flowmux-asr] pump: cancelled");
                    break;
                }
                let engine_snapshot = engine_cell.lock().unwrap().clone();
                let engine = match engine_snapshot {
                    Some(e) => e,
                    None => {
                        eprintln!("[flowmux-asr] pump: engine not loaded yet, skip");
                        tokio::time::sleep(TICK).await;
                        continue;
                    }
                };
                let (pcm, dur) = {
                    let buf = buffer_arc.lock().unwrap();
                    let d = buf.duration_seconds();
                    if buf.sample_rate == 0
                        || buf.channels == 0
                        || d < MIN_SECONDS
                    {
                        (None, d)
                    } else {
                        let mono = flowmux_asr::audio::resample::resample_to_16k_mono(
                            &buf.interleaved,
                            buf.sample_rate,
                            buf.channels,
                        )
                        .ok();
                        let mono = mono.map(|mut m| {
                            if gain > 1.001 {
                                for s in m.iter_mut() {
                                    *s = (*s * gain).clamp(-1.0, 1.0);
                                }
                            }
                            m
                        });
                        (mono, d)
                    }
                };
                let Some(pcm) = pcm else {
                    eprintln!("[flowmux-asr] pump: only {dur:.2}s buffered, skip");
                    tokio::time::sleep(TICK).await;
                    continue;
                };
                eprintln!("[flowmux-asr] pump tick: {dur:.2}s buffered, transcribing...");
                let result = tokio::task::spawn_blocking(move || {
                    engine.transcribe(16_000, &pcm)
                })
                .await;
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(text) = result {
                    eprintln!("[flowmux-asr] pump partial raw: {text:?}");
                    let cleaned = clean_asr_artifacts(&text);
                    if cleaned.is_empty() {
                        eprintln!("[flowmux-asr] pump: cleaned empty, skip");
                        tokio::time::sleep(TICK).await;
                        continue;
                    }
                    let sanitized = sanitize_for_pty(&cleaned);
                    let payload = {
                        let mut last = last_injection.lock().unwrap();
                        if *last == sanitized {
                            String::new()
                        } else {
                            let p = build_replace_payload(&last, &sanitized);
                            *last = sanitized.clone();
                            p
                        }
                    };
                    eprintln!("[flowmux-asr] pump emit len={}", payload.len());
                    if !payload.is_empty() {
                        if tx.send(AsrUiEvent::Partial { delta: payload }).await.is_err() {
                            eprintln!("[flowmux-asr] pump: send failed");
                            break;
                        }
                    }
                }
                tokio::time::sleep(TICK).await;
            }
            eprintln!("[flowmux-asr] pump: exit");
        });
    }

    /// Hold-mode release: stop capture, transcribe, inject.
    pub fn release(&mut self) {
        if self.is_recording() {
            self.finish();
        }
    }

    pub fn cancel(&mut self) {
        if let Some(cancel) = self.pump_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(cancel) = self.finish_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(mut session) = self.session.take() {
            session.cancel();
        }
        self.started_at = None;
        self.emit(AsrUiEvent::Cancelled);
    }

    fn finish(&mut self) {
        if let Some(cancel) = self.pump_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        let duration = self
            .started_at
            .take()
            .map(|t| t.elapsed())
            .unwrap_or_default();
        if duration < Duration::from_millis(150) {
            session.cancel();
            self.emit(AsrUiEvent::DroppedTooShort {
                seconds: duration.as_secs_f32(),
            });
            return;
        }
        self.emit(AsrUiEvent::Transcribing);
        let tx = self.event_tx.clone();
        let last_injection = self.last_injection.clone();
        let engine_cell = self.engine_cell.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.finish_cancel = Some(cancel.clone());
        self.runtime.spawn_blocking(move || {
            // Wait (with cancel) for the engine to finish loading.
            // The capture has already stopped, so any delay here only
            // affects how long the final text takes to appear — the
            // user's audio was preserved.
            let engine = match wait_for_engine(&engine_cell, &cancel) {
                Some(e) => e,
                None => return, // cancelled
            };
            let result = session.finish(&engine);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let event = match result {
                Ok(text) => {
                    let cleaned = clean_asr_artifacts(&text);
                    let sanitized = sanitize_for_pty(&cleaned);
                    let mut last = last_injection.lock().unwrap();
                    let payload = if *last == sanitized {
                        String::new()
                    } else {
                        let p = build_replace_payload(&last, &sanitized);
                        *last = sanitized.clone();
                        p
                    };
                    AsrUiEvent::Done { delta: payload }
                }
                Err(e) => AsrUiEvent::Failed(format!("finalize failed: {e}")),
            };
            let _ = tx.send_blocking(event);
        });
    }

    fn active_entry(&self) -> Option<ModelEntry> {
        let id = self.options.active_model_id.clone();
        catalog::entries()
            .into_iter()
            .find(|e| e.id.as_str() == id)
            .or_else(|| Some(catalog::recommended_default()))
    }

    fn signature(&self) -> String {
        let id = self
            .active_entry()
            .map(|e| e.id.as_str().to_string())
            .unwrap_or_default();
        format!("{}::{}", id, self.options.language.as_code())
    }

    fn emit(&self, event: AsrUiEvent) {
        let _ = self.event_tx.send_blocking(event);
    }
}

/// Block (with a 100 ms poll) until the engine cell is populated or
/// the cancel flag is set. Returns `None` only when cancelled.
fn wait_for_engine(cell: &EngineCell, cancel: &AtomicBool) -> Option<Arc<SenseVoiceEngine>> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(engine) = cell.lock().unwrap().clone() {
            return Some(engine);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn build_replace_payload(previous: &str, candidate: &str) -> String {
    let prev_chars: Vec<char> = previous.chars().collect();
    let cand_chars: Vec<char> = candidate.chars().collect();
    let mut common = 0;
    while common < prev_chars.len()
        && common < cand_chars.len()
        && prev_chars[common] == cand_chars[common]
    {
        common += 1;
    }
    let deletions = prev_chars.len() - common;
    let additions: String = cand_chars[common..].iter().collect();
    let mut payload = String::with_capacity(deletions + additions.len());
    for _ in 0..deletions {
        payload.push('\x7f');
    }
    payload.push_str(&additions);
    payload
}

pub fn handle_ui_event(
    event: AsrUiEvent,
    focused: &FocusedPane,
    registry: &TerminalRegistry,
    clipboard_toast: &ClipboardToast,
    indicator: &dyn AsrIndicator,
    auto_enter: bool,
) {
    let _ = clipboard_toast;
    match event {
        AsrUiEvent::EnginePreloaded => {}
        AsrUiEvent::Recording => indicator.set_recording(true),
        AsrUiEvent::Partial { delta } => {
            if !delta.is_empty() {
                inject_text(focused, registry, &delta);
            }
        }
        AsrUiEvent::Transcribing => indicator.set_busy(true),
        AsrUiEvent::Done { delta } => {
            indicator.set_recording(false);
            indicator.set_busy(false);
            if !delta.trim().is_empty() {
                inject_text(focused, registry, &delta);
            }
            if auto_enter {
                inject_text(focused, registry, "\r");
            }
        }
        AsrUiEvent::DroppedTooShort { seconds } => {
            indicator.set_recording(false);
            indicator.set_busy(false);
            clipboard_toast.show_with_message(&format!(
                "음성이 너무 짧습니다 ({:.2}초)",
                seconds
            ));
        }
        AsrUiEvent::Failed(msg) => {
            indicator.set_recording(false);
            indicator.set_busy(false);
            clipboard_toast.show_with_message(&format!("음성 입력 실패: {msg}"));
        }
        AsrUiEvent::Cancelled => {
            indicator.set_recording(false);
            indicator.set_busy(false);
        }
    }
}

fn inject_text(focused: &FocusedPane, registry: &TerminalRegistry, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(pane_id) = focused.get() else {
        return;
    };
    let registry = registry.borrow();
    let Some(terminal) = registry.active_terminal(pane_id) else {
        return;
    };
    terminal.feed_text(text);
}

pub trait AsrIndicator {
    fn set_recording(&self, on: bool);
    fn set_busy(&self, on: bool);
}

pub struct DotIndicator {
    pub widget: gtk::Box,
}

impl AsrIndicator for DotIndicator {
    fn set_recording(&self, on: bool) {
        self.widget.set_visible(on);
    }
    fn set_busy(&self, _on: bool) {}
}

/// Window-level Hold-mode key controller. Press of the configured
/// accelerator (default Ctrl+Alt+Space) opens the capture; release
/// of the trailing key (Space) closes it and queues the
/// transcription. Toggle mode and the Enter / Esc helper bindings
/// were removed at the user's request.
pub fn install_ptt_event_controller(
    window: &adw::ApplicationWindow,
    asr_controller: AsrControllerHandle,
) {
    use gtk::gdk;
    use gtk::glib::Propagation;

    let ctrl = gtk::EventControllerKey::new();
    ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pressed = Rc::new(Cell::new(false));

    let target_keyval = gdk::Key::space;
    let target_mods = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;
    let relevant_mask = gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::META_MASK;

    {
        let asr = asr_controller.clone();
        let pressed = pressed.clone();
        ctrl.connect_key_pressed(move |_, keyval, _keycode, modifier| {
            if keyval != target_keyval {
                return Propagation::Proceed;
            }
            let modifier_subset = modifier & relevant_mask;
            if modifier_subset != target_mods {
                return Propagation::Proceed;
            }
            eprintln!(
                "[flowmux-asr] hold press: latch_was={}",
                pressed.get()
            );
            if pressed.get() {
                return Propagation::Stop;
            }
            pressed.set(true);
            match asr.borrow_mut().start() {
                Ok(()) => eprintln!("[flowmux-asr] hold press: start OK"),
                Err(()) => eprintln!("[flowmux-asr] hold press: start FAILED"),
            }
            Propagation::Stop
        });
    }

    {
        let asr = asr_controller.clone();
        let pressed = pressed.clone();
        ctrl.connect_key_released(move |_, keyval, _keycode, _modifier| {
            if keyval != target_keyval {
                return;
            }
            let was = pressed.get();
            eprintln!("[flowmux-asr] hold release: latch_was={was}");
            if !was {
                return;
            }
            pressed.set(false);
            asr.borrow_mut().release();
        });
    }

    window.add_controller(ctrl);
}
