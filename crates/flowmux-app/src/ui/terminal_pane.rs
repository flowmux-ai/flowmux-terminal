// SPDX-License-Identifier: GPL-3.0-or-later
//! VTE-backed terminal pane.
//!
//! Spawns the user's shell in a PTY and surfaces:
//!
//! * `notification-received` (OSC 99 / Konsole) → forwarded as a
//!   structured notification to the app handler;
//! * `bell` (BEL) → optional attention signal;
//! * `child-exited` → caller decides whether to recycle the pane.
//!
//! For OSC 9 / 777 cmux supports, those are not fired by VTE as
//! distinct signals — agents wishing to use them should pipe through
//! `flowmux notify-stream` (which uses the same parser the GUI uses).
//! We will revisit when libghostty backend lands.

use flowmux_core::{PaneId, SurfaceId};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use vte::prelude::*;

#[derive(Clone)]
pub struct TerminalPane {
    pub id: PaneId,
    /// The VTE widget itself — apply_to_vte / feed call into this.
    pub widget: vte::Terminal,
    /// Widget that goes into a pane-local surface stack.
    pub root: gtk::Widget,
    /// PID of the spawned shell.
    pub pid: Rc<Cell<Option<i32>>>,
}

impl TerminalPane {
    /// Best-effort current working directory of the shell.
    ///
    /// Preference order:
    ///   1. VTE's `current-directory-uri` (OSC 7) — set by zsh / bash
    ///      / fish when the shell announces its cwd. Always reflects
    ///      `cd` exactly.
    ///   2. `/proc/<pid>/cwd` symlink target — works for any spawned
    ///      shell on Linux even without OSC 7 support.
    pub fn current_dir(&self) -> Option<PathBuf> {
        if let Some(uri) = self.widget.current_directory_uri() {
            let s: String = uri.into();
            if !s.is_empty() {
                if let Some(p) = uri_to_path(&s) {
                    return Some(p);
                }
            }
        }
        if let Some(pid) = self.pid.get() {
            if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                return Some(p);
            }
        }
        None
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    // file:///foo/bar  → /foo/bar
    // file://host/foo  → /foo  (host is dropped; flowmux is local)
    let rest = uri.strip_prefix("file://")?;
    let path_only = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => rest,
    };
    Some(PathBuf::from(percent_decode(path_only)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone)]
pub struct PaneCallbacks {
    pub on_notification: Rc<RefCell<dyn FnMut(PaneId, String, String)>>,
    pub on_bell: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_child_exited: Rc<RefCell<dyn FnMut(PaneId, i32)>>,
    pub on_focus: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Per-pane close button on the Overlay + 'Close Pane' menu item.
    pub on_close_pane: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Right-click menu 'Split Right'.
    pub on_split_right: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Right-click menu 'Split Down'.
    pub on_split_down: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local surface tab activation.
    pub on_activate_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local new terminal tab.
    pub on_new_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local new browser tab (탭브라우저 추가).
    pub on_new_browser_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local close tab.
    pub on_close_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local rename tab.
    pub on_rename_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// 같은 pane 내부에서 탭을 드래그 앤 드랍으로 reorder. 세 번째
    /// 인자는 이동 후의 최종 인덱스(0-based, 길이 초과 시 클램프).
    pub on_reorder_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, usize)>>,
    /// VTE reported that a terminal surface changed its cwd.
    pub on_terminal_cwd_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, PathBuf)>>,
    /// WebKit reported that a browser pane navigated to a new URL.
    pub on_browser_uri_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// WebKit reported that a browser pane's page title changed.
    pub on_browser_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// VTE가 OSC 0/2 window-title을 알렸다 (vi/claude/codex/tmux
    /// 같은 프로그램이 셸 안에서 보낼 때). 빈 문자열은 호출 측에서
    /// 무시한다.
    pub on_terminal_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// 현재 시점의 사용자 옵션을 가져온다 (새 BrowserPane 생성 시
    /// 엔진 선택 + 위젯 생성 직후 줌 적용에 사용). WindowController가
    /// 보유한 `Rc<RefCell<Options>>`에서 clone해 돌려주는 가벼운
    /// 함수. 다이얼로그가 옵션을 갱신하면 다음 호출부터 새 값이
    /// 보인다.
    pub read_options: Rc<dyn Fn() -> flowmux_config::options::Options>,
    /// 같은 pane 내부에서 surface의 현재 인덱스(0-based)를 돌려준다.
    /// 탭 DnD reorder가 source / target 상대 위치를 정확히 알고 final_index를
    /// 계산하기 위해 PaneRegistry::surface_tabs의 위치를 빌려 본다.
    pub position_of_surface_in_pane:
        Rc<dyn Fn(PaneId, SurfaceId) -> Option<usize>>,
    /// 터미널 안에서 Ctrl+클릭으로 URL이 선택되면 호출된다. 호출 측은
    /// 같은 pane에 새 탭브라우저를 url과 함께 연다(GtkCommand::OpenUrlInBrowserTab).
    /// url은 trailing punctuation이 정리된 상태로 전달된다.
    pub on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
}

impl TerminalPane {
    /// Build a fresh terminal widget and spawn `argv` in `cwd`. If
    /// `argv` is empty we fall back to the user's `$SHELL`.
    pub fn spawn(
        id: PaneId,
        argv: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        callbacks: PaneCallbacks,
    ) -> Self {
        let term = vte::Terminal::new();
        term.set_hexpand(true);
        term.set_vexpand(true);
        term.set_scrollback_lines(10_000);
        term.set_audible_bell(false);

        // OSC 99 (Konsole-format) is not exposed as a signal on Ubuntu's
        // VTE 0.76 build — the `notification-received` signal is a
        // Konsole extension compiled out in upstream VTE. We capture
        // OSC notifications via the `flowmux notify-stream` CLI today,
        // and a PTY-tee path is planned in flowmux-terminal so the GUI
        // can subscribe directly without wrapping every command.
        let _unused_notification_cb = &callbacks.on_notification;

        // BEL — generic attention.
        {
            let cb = callbacks.on_bell.clone();
            let id = id;
            term.connect_bell(move |_term| {
                (cb.borrow_mut())(id);
            });
        }

        // URL 인식 (Ctrl+클릭으로 내부 탭브라우저에서 열기).
        // 터미널에 출력된 URL을 PCRE2 regex로 매치 등록 → hover 시
        // pointer 커서로 바뀌고, 사용자가 Ctrl을 누른 채 좌클릭하면
        // 같은 pane에 새 탭브라우저를 그 URL로 띄운다. 일반 클릭은
        // 평소처럼 VTE의 텍스트 선택으로 흘러간다.
        install_url_link_handling(&term, id, callbacks.on_open_url.clone());

        // Process exit.
        {
            let cb = callbacks.on_child_exited.clone();
            let id = id;
            term.connect_child_exited(move |_term, status| {
                (cb.borrow_mut())(id, status);
            });
        }

        // Focus tracking — keyboard shortcuts (split right/down, etc.)
        // need to know which pane is currently focused.
        {
            let cb = callbacks.on_focus.clone();
            let id = id;
            let focus_ctrl = gtk::EventControllerFocus::new();
            focus_ctrl.connect_enter(move |_| (cb.borrow_mut())(id));
            term.add_controller(focus_ctrl);
        }

        // Right-click — show a context menu with Split / Close.
        // We deliberately avoid PopoverMenu+win.* actions because the
        // action lookup chain can drop through PopoverMenu's internal
        // implementation in some GTK versions; instead we use a plain
        // Popover with Buttons whose connect_clicked closures fire
        // the per-pane callbacks directly through the bridge.
        {
            let on_focus = callbacks.on_focus.clone();
            let on_split_right = callbacks.on_split_right.clone();
            let on_split_down = callbacks.on_split_down.clone();
            let on_close_pane = callbacks.on_close_pane.clone();
            let id = id;
            let term_widget = term.clone();
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            click.connect_pressed(move |gesture, _n_press, x, y| {
                (on_focus.borrow_mut())(id);

                let popover = gtk::Popover::new();
                let v = gtk::Box::new(gtk::Orientation::Vertical, 0);
                v.set_margin_top(4);
                v.set_margin_bottom(4);

                let mk = |label: &str| -> gtk::Button {
                    let b = gtk::Button::with_label(label);
                    b.add_css_class("flat");
                    b.set_halign(gtk::Align::Fill);
                    b.set_hexpand(true);
                    if let Some(label) = b.child().and_downcast::<gtk::Label>() {
                        label.set_xalign(0.0);
                    }
                    b
                };

                let split_r = mk("Split Right");
                let pop = popover.clone();
                let cb = on_split_right.clone();
                split_r.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id);
                });
                v.append(&split_r);

                let split_d = mk("Split Down");
                let pop = popover.clone();
                let cb = on_split_down.clone();
                split_d.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id);
                });
                v.append(&split_d);

                v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

                let close_p = mk("Close Pane");
                let pop = popover.clone();
                let cb = on_close_pane.clone();
                close_p.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id);
                });
                v.append(&close_p);

                popover.set_child(Some(&v));
                popover.set_parent(&term_widget);
                popover.set_has_arrow(false);
                crate::ui::popover_pos::anchor_at_click(&popover, &term_widget, x, y);
                popover.connect_closed(|p| p.unparent());
                popover.popup();
                gesture.set_state(gtk::EventSequenceState::Claimed);
            });
            term.add_controller(click);
        }

        let argv: Vec<String> = if argv.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())]
        } else {
            argv
        };
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let cwd_str = cwd.as_ref().and_then(|p| p.to_str());

        let pid: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
        let pid_for_cb = pid.clone();
        term.spawn_async(
            vte::PtyFlags::DEFAULT,
            cwd_str,
            &argv_refs,
            &[], // envv: inherit
            glib::SpawnFlags::DEFAULT,
            || {}, // child setup (runs in child after fork)
            -1,    // no timeout
            gtk::gio::Cancellable::NONE,
            move |result| {
                match result {
                    Ok(pid_value) => {
                        // glib::Pid wraps libc::pid_t (i32 on Linux).
                        pid_for_cb.set(Some(pid_value.0 as i32));
                    }
                    Err(e) => tracing::warn!(error = %e, "vte spawn failed"),
                }
            },
        );

        Self {
            id,
            root: term.clone().upcast(),
            widget: term,
            pid,
        }
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.widget.feed_child(bytes);
    }
}

// ---- URL link handling --------------------------------------------------

/// PCRE2 패턴: http(s) / ftp / file URL을 공백/꺽쇠/따옴표/백틱 전까지
/// 매치한다. (?i)로 schema는 대소문자 무관. 매치된 문자열에 끝쪽
/// 구두점이 붙을 수 있어 dispatch 직전에 trim_url_trailing()로 정리한다.
const URL_REGEX_PATTERN: &str = r#"(?i)(?:https?|ftp|file)://[^\s<>"'`]+"#;

/// PCRE2 컴파일 플래그.
///   * PCRE2_MULTILINE (0x400): 줄을 넘기는 wrap된 터미널 출력에서도
///     매치가 깨지지 않도록 한다.
///   * PCRE2_UTF (0x80000): VTE는 매치 엔진에 UTF-8 텍스트를 넘기므로
///     이 플래그가 빠지면 PCRE2가 입력을 raw byte로 다뤄 hover 감지 /
///     커서 변경이 동작하지 않는다 (gnome-terminal과 동일 조합).
///   * PCRE2_NO_UTF_CHECK (0x4000_0000): VTE 내부에서 이미 UTF-8을
///     검증하므로 PCRE2의 추가 검증을 끄고 매치마다의 오버헤드를 줄인다.
const URL_REGEX_COMPILE_FLAGS: u32 = 0x0000_0400 | 0x0008_0000 | 0x4000_0000;

/// Trailing punctuation을 잘라낸 URL을 돌려준다. `.`, `,`, `;`, `:`,
/// `!`, `?`, 닫는 괄호류, 따옴표 — 문장 끝에 자연스럽게 붙기 쉬운
/// 문자들. URL path/query에 의도적으로 들어 있을 수도 있지만, 사용자
/// 시나리오에서 "마지막에 `.`이 붙은 URL을 그대로 열어 404"보다는
/// "마지막 `.`을 떼서 정상 URL을 여는" 쪽이 거의 항상 옳다.
fn trim_url_trailing(s: &str) -> String {
    s.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | ')'
                | ']'
                | '}'
                | '\''
                | '"'
                | '`'
        )
    })
    .to_string()
}

fn install_url_link_handling(
    term: &vte::Terminal,
    pane_id: PaneId,
    on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
) {
    // 1) Regex 컴파일 + 등록. 실패하면(매우 드묾 — 보통 PCRE2 빌드
    //    이슈) 조용히 fall through — 링크 인식만 비활성화되고 터미널
    //    자체는 정상 동작한다.
    let regex = match vte::Regex::for_match(URL_REGEX_PATTERN, URL_REGEX_COMPILE_FLAGS) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to compile URL regex; link clicking disabled");
            return;
        }
    };
    let tag = term.match_add_regex(&regex, 0);
    // hover 시 pointer 커서로 바뀌게 함. Ctrl 없이도 pointer가 보이지만
    // 실제 클릭은 Ctrl이 눌린 경우에만 동작 — gnome-terminal과 동일한
    // UX 패턴이다 (시각적 hint는 항상, action은 modifier로 제한).
    term.match_set_cursor_name(tag, "pointer");
    tracing::debug!(%pane_id, tag, "URL match registered on terminal");

    // 2) 좌클릭 gesture. Capture phase에서 button-press를 먼저 보고
    //    Ctrl 여부를 판단한다.
    //
    //    여기서 핵심 함정: GtkGestureSingle은 button-press 시점에
    //    자기 sequence를 자동으로 Claimed로 가져가 버린다. 즉 우리가
    //    아무 것도 안 해도 같은 button-press 이벤트는 다른 controller
    //    (VTE의 selection drag)로 전달되지 않아 텍스트 선택이 막힌다.
    //    "선택도 안 된다"의 직접 원인이 이거다.
    //
    //    해결: Ctrl이 안 눌렸으면 명시적으로 set_state(Denied)로
    //    sequence를 우리 손에서 풀어 준다 — VTE의 selection gesture가
    //    그 sequence를 잡을 수 있다. Ctrl이 눌린 경우에만 Claimed로
    //    유지해 selection이 시작되지 않게 한다.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);

    let term_widget = term.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
        let modifiers = gesture
            .current_event()
            .map(|e| e.modifier_state())
            .unwrap_or_else(gtk::gdk::ModifierType::empty);
        if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
            // VTE의 selection drag가 같은 button-press를 처리할 수 있도록
            // 우리 sequence를 풀어 준다. 이거 없으면 GestureSingle의 자동
            // Claim 때문에 selection이 영구히 막힌다.
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }

        // 우선 OSC 8 hyperlink (escape sequence로 URL 첨부된 텍스트)부터
        // 보고, 없으면 regex 매치로 폴백한다. OSC 8을 지원하는 ls / git
        // / 일부 빌드 도구가 만드는 링크가 우선순위가 높다.
        let url_raw: Option<String> = term_widget
            .check_hyperlink_at(x, y)
            .map(|g| g.to_string())
            .or_else(|| {
                let (m, _tag) = term_widget.check_match_at(x, y);
                m.map(|g| g.to_string())
            });

        let Some(raw) = url_raw else {
            // Ctrl을 누른 채로 클릭했지만 URL 위가 아니다. selection 시도로
            // 보고 sequence를 풀어 준다 — Ctrl+드래그로 block selection
            // 같은 VTE 자체 기능을 막지 않기 위해.
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        let url = trim_url_trailing(&raw);
        if url.is_empty() {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        tracing::info!(%pane_id, %url, "Ctrl+click on terminal URL → open in browser tab");
        (on_open_url.borrow_mut())(pane_id, url);
        // URL을 처리했으니 sequence를 가져가서 VTE의 selection이
        // 시작되지 않게 한다.
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    term.add_controller(click);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_url_trailing_strips_common_sentence_punctuation() {
        assert_eq!(
            trim_url_trailing("https://example.com/page."),
            "https://example.com/page"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/page),"),
            "https://example.com/page"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/path?q=1!"),
            "https://example.com/path?q=1"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/'\"`"),
            "https://example.com/"
        );
    }

    #[test]
    fn trim_url_trailing_preserves_internal_punctuation() {
        // path 내부의 `.`이나 `,`은 보존해야 한다 — trim은 끝부터만 한다.
        assert_eq!(
            trim_url_trailing("https://example.com/a.b/c"),
            "https://example.com/a.b/c"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/path?a=1,2,3"),
            "https://example.com/path?a=1,2,3"
        );
    }

    #[test]
    fn trim_url_trailing_handles_clean_url() {
        assert_eq!(
            trim_url_trailing("https://example.com/"),
            "https://example.com/"
        );
    }

    #[test]
    fn trim_url_trailing_handles_empty() {
        assert_eq!(trim_url_trailing(""), "");
        assert_eq!(trim_url_trailing("...,,;"), "");
    }
}
