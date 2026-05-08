// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain types shared across flowmux crates.
//!
//! Types here are deliberately backend-agnostic: they describe the shape
//! of a workspace, a surface (terminal/browser pane), a notification, and
//! the IPC verbs — not how any of them are rendered or executed.
//!
//! Mapping to cmux concepts is documented in
//! `docs/upstream-mapping/domain-model.md`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const TERMINAL_TAB_TITLE_MAX_CHARS: usize = 15;
const FALLBACK_TERMINAL_TAB_TITLE: &str = "Terminal";

pub fn terminal_tab_title_for_cwd(cwd: Option<&Path>) -> String {
    let folder = cwd
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_TERMINAL_TAB_TITLE.to_string());
    truncate_tab_title(&folder)
}

/// 터미널 surface가 받은 OSC 0/2 타이틀이 셸의 PS1 윈도우 타이틀
/// (cwd를 그대로 에코한 형태)인지 판별한다. true이면 호출자는
/// 그 타이틀을 무시하고 cwd 기반 폴더 이름을 유지해야 한다.
///
/// 인식 패턴:
/// * 타이틀이 cwd 절대경로 자체와 동일 (`/tmp/foo`)
/// * 타이틀이 `<prefix>:[ ]<cwd>` 로 끝나고 prefix 끝이 `:`
///   (bash 기본 `\u@\h: \w`, debian_chroot 변형 등) — `<cwd>`는
///   절대경로 또는 `$HOME`을 `~`로 축약한 형태.
///
/// vi/codex/claude/tmux 같이 셸 외부 프로그램이 보내는 타이틀
/// (`vi src/main.rs`, `tmux: 0:bash*` 등)은 위 구조에 맞지 않으므로
/// 통과한다. 호출자는 PS1 에코는 버리고 프로그램 타이틀은 받는다.
pub fn title_is_shell_cwd_echo(title: &str, cwd: &Path, home: Option<&Path>) -> bool {
    let title = title.trim_end();
    if title.is_empty() {
        return false;
    }
    let cwd_str = cwd.to_string_lossy();
    if matches_trailing_path_after_colon(title, cwd_str.as_ref()) {
        return true;
    }
    if let Some(home) = home {
        let home_str = home.to_string_lossy();
        if let Some(rel) = cwd_str.strip_prefix(home_str.as_ref()) {
            let tilde_form = if rel.is_empty() {
                "~".to_string()
            } else if rel.starts_with('/') {
                format!("~{}", rel)
            } else {
                return false;
            };
            if matches_trailing_path_after_colon(title, &tilde_form) {
                return true;
            }
        }
    }
    false
}

fn matches_trailing_path_after_colon(title: &str, path: &str) -> bool {
    if title == path {
        return true;
    }
    let Some(prefix) = title.strip_suffix(path) else {
        return false;
    };
    // bash 기본 PS1은 `:`와 path 사이에 공백을 넣는다(`\u@\h: \w`).
    // 옛날식이나 zsh 일부 테마는 공백 없이 붙이기도 한다(`\u@\h:\w`).
    let prefix = prefix.trim_end_matches(' ');
    prefix.ends_with(':')
}

fn truncate_tab_title(title: &str) -> String {
    if title.chars().count() <= TERMINAL_TAB_TITLE_MAX_CHARS {
        return title.to_string();
    }
    let prefix: String = title.chars().take(TERMINAL_TAB_TITLE_MAX_CHARS).collect();
    format!("{prefix}...")
}

fn looks_like_legacy_terminal_title(title: &str) -> bool {
    let title = title.trim();
    if title.is_empty() || title == FALLBACK_TERMINAL_TAB_TITLE {
        return true;
    }
    title
        .strip_prefix("Terminal ")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
}

fn normalize_unlocked_terminal_title(surface: &mut PaneSurface) -> bool {
    if surface.title_locked {
        return false;
    }
    let SurfaceKind::Terminal { cwd, .. } = &surface.kind else {
        return false;
    };

    if looks_like_legacy_terminal_title(&surface.title) {
        let Some(cwd) = cwd.as_deref() else {
            return false;
        };
        let title = terminal_tab_title_for_cwd(Some(cwd));
        if surface.title == title {
            return false;
        }
        surface.title = title;
        return true;
    }

    let title_matches_cwd = cwd
        .as_deref()
        .map(|cwd| terminal_tab_title_for_cwd(Some(cwd)))
        .as_deref()
        == Some(surface.title.as_str());
    if title_matches_cwd {
        return false;
    }

    surface.title_locked = true;
    true
}

pub mod id {
    use super::*;

    macro_rules! id_newtype {
        ($name:ident) => {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
            #[serde(transparent)]
            pub struct $name(pub Uuid);

            impl $name {
                pub fn new() -> Self {
                    Self(Uuid::new_v4())
                }
            }

            impl Default for $name {
                fn default() -> Self {
                    Self::new()
                }
            }

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    self.0.fmt(f)
                }
            }

            impl std::str::FromStr for $name {
                type Err = uuid::Error;
                fn from_str(s: &str) -> Result<Self, Self::Err> {
                    Ok(Self(s.parse()?))
                }
            }
        };
    }

    id_newtype!(WorkspaceId);
    id_newtype!(SurfaceId);
    id_newtype!(PaneId);
    id_newtype!(NotificationId);
}

pub use id::{NotificationId, PaneId, SurfaceId, WorkspaceId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    /// 자동 결정되는 워크스페이스 네임. 워크스페이스 생성 시 root_dir의
    /// 마지막 폴더 이름으로 시작하고, daemon 측 자동 갱신 신호
    /// (예: PTY OSC, cwd 변경)로 갱신될 수 있다. cmux의 `processTitle`에
    /// 대응 — 사용자 의도가 아니라 시스템이 마지막으로 관찰한 값이다.
    pub name: String,
    /// 사용자가 우클릭 메뉴 → "Change tab name"으로 직접 입력한 이름.
    /// `None`이면 자동 모드(즉, `name`을 표시)이고, 빈 문자열로 다시 저장
    /// 요청하면 `None`으로 되돌아가 자동 모드로 복귀한다 (cmux의
    /// `customTitle: String?`과 동일 시맨틱). 사이드 패널에 표시되는 최종
    /// 이름은 [`Workspace::display_title`]가 계산.
    #[serde(default)]
    pub custom_title: Option<String>,
    pub root_dir: PathBuf,
    /// Resolved when the workspace's root_dir is a git checkout.
    pub git: Option<GitInfo>,
    /// Ports observed listening on localhost from any process descendant of
    /// the workspace's root pane (populated by the daemon, not stored).
    #[serde(default)]
    pub listening_ports: Vec<u16>,
    pub surfaces: Vec<Surface>,
    /// Hex color (`#RRGGBB`) used to tint the workspace's sidebar
    /// indicator. Optional so old `state.json` files load cleanly;
    /// the daemon assigns a default on creation.
    #[serde(default)]
    pub color: Option<String>,
}

impl Workspace {
    /// 사이드 패널 / 윈도우 타이틀에 표시할 최종 이름. 사용자가 직접
    /// 지정한 [`Workspace::custom_title`]이 있으면 그 값, 없으면 자동
    /// 결정된 [`Workspace::name`]을 그대로 돌려준다.
    pub fn display_title(&self) -> &str {
        self.custom_title
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitInfo {
    pub branch: String,
    pub remote_url: Option<String>,
    pub linked_pr: Option<LinkedPr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPr {
    pub number: u64,
    pub state: PrState,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
    Draft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub id: SurfaceId,
    pub kind: SurfaceKind,
    pub title: String,
    pub root_pane: Pane,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SurfaceKind {
    Terminal {
        shell: Option<String>,
        cwd: Option<PathBuf>,
    },
    Browser {
        initial_url: Option<String>,
    },
}

/// A tab inside a leaf pane. cmux calls these surfaces: each pane can
/// host multiple terminal/browser surfaces, with exactly one active at
/// a time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSurface {
    pub id: SurfaceId,
    pub title: String,
    /// False while flowmux owns the title from cwd changes. Set true
    /// once the user explicitly renames the tab.
    #[serde(default)]
    pub title_locked: bool,
    pub kind: SurfaceKind,
}

/// A pane is either a leaf (rendered content) or a binary split. This
/// matches the recursive split model used by tmux, Ghostty, and cmux.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pane {
    Leaf {
        id: PaneId,
        content: PaneContent,
    },
    Split {
        id: PaneId,
        direction: SplitDirection,
        /// Ratio of the first child's size to the parent. 0.0 < ratio < 1.0.
        ratio: f32,
        first: Box<Pane>,
        second: Box<Pane>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

impl Pane {
    /// Find the leaf with `target` and replace it with a new split that
    /// keeps the original leaf as `first` and adds a fresh sibling as
    /// `second`. Returns the new sibling's [`PaneId`] on success, or
    /// `None` if `target` was not found.
    pub fn split_leaf(
        &mut self,
        target: PaneId,
        direction: SplitDirection,
        ratio: f32,
        new_content: PaneContent,
    ) -> Option<PaneId> {
        match self {
            Pane::Leaf { id, .. } if *id == target => {
                let original = std::mem::replace(
                    self,
                    Pane::Split {
                        id: PaneId::new(),
                        direction,
                        ratio,
                        first: Box::new(Pane::Leaf {
                            id: target,
                            content: PaneContent::tabbed_terminal("Terminal", None),
                        }),
                        second: Box::new(Pane::Leaf {
                            id: PaneId::new(),
                            content: new_content.clone(),
                        }),
                    },
                );
                if let (
                    Pane::Split { first, second, .. },
                    Pane::Leaf {
                        content: orig_content,
                        ..
                    },
                ) = (self, &original)
                {
                    *first = Box::new(Pane::Leaf {
                        id: target,
                        content: orig_content.clone(),
                    });
                    let new_id = if let Pane::Leaf { id, .. } = &**second {
                        *id
                    } else {
                        unreachable!()
                    };
                    return Some(new_id);
                }
                None
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .split_leaf(target, direction, ratio, new_content.clone())
                .or_else(|| second.split_leaf(target, direction, ratio, new_content)),
        }
    }

    /// Walk every leaf id in DFS order.
    pub fn for_each_leaf<F: FnMut(PaneId)>(&self, mut f: F) {
        fn rec<F: FnMut(PaneId)>(p: &Pane, f: &mut F) {
            match p {
                Pane::Leaf { id, .. } => f(*id),
                Pane::Split { first, second, .. } => {
                    rec(first, f);
                    rec(second, f);
                }
            }
        }
        rec(self, &mut f);
    }

    /// Normalize legacy leaf content into pane-local surface tabs.
    pub fn normalize_leaf_tabs(&mut self, fallback_cwd: Option<PathBuf>) -> bool {
        match self {
            Pane::Leaf { content, .. } => content.normalize_to_tabs(fallback_cwd),
            Pane::Split { first, second, .. } => {
                first.normalize_leaf_tabs(fallback_cwd.clone())
                    | second.normalize_leaf_tabs(fallback_cwd)
            }
        }
    }

    pub fn add_surface_to_leaf(
        &mut self,
        target: PaneId,
        surface: PaneSurface,
    ) -> Option<SurfaceId> {
        match self {
            Pane::Leaf { id, content } if *id == target => {
                content.normalize_to_tabs(None);
                match content {
                    PaneContent::Tabs { active, surfaces } => {
                        let id = surface.id;
                        *active = id;
                        surfaces.push(surface);
                        Some(id)
                    }
                    PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
                }
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .add_surface_to_leaf(target, surface.clone())
                .or_else(|| second.add_surface_to_leaf(target, surface)),
        }
    }

    pub fn set_active_surface(&mut self, target: PaneId, surface_id: SurfaceId) -> bool {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { active, surfaces } => {
                    if surfaces.iter().any(|surface| surface.id == surface_id) {
                        *active = surface_id;
                        true
                    } else {
                        false
                    }
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
            },
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.set_active_surface(target, surface_id)
                    || second.set_active_surface(target, surface_id)
            }
        }
    }

    pub fn surface_title(&self, target: PaneId, surface_id: SurfaceId) -> Option<&str> {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => surfaces
                    .iter()
                    .find(|surface| surface.id == surface_id)
                    .map(|surface| surface.title.as_str()),
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
            },
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .surface_title(target, surface_id)
                .or_else(|| second.surface_title(target, surface_id)),
        }
    }

    pub fn terminal_surface_cwd(&self, target: PaneId) -> Option<PathBuf> {
        match self {
            Pane::Leaf { id, content } if *id == target => {
                content
                    .active_surface()
                    .and_then(|surface| match &surface.kind {
                        SurfaceKind::Terminal { cwd, .. } => cwd.clone(),
                        SurfaceKind::Browser { .. } => None,
                    })
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .terminal_surface_cwd(target)
                .or_else(|| second.terminal_surface_cwd(target)),
        }
    }

    pub fn rename_surface(&mut self, target: PaneId, surface_id: SurfaceId, title: String) -> bool {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => {
                    if let Some(surface) =
                        surfaces.iter_mut().find(|surface| surface.id == surface_id)
                    {
                        surface.title = title;
                        surface.title_locked = true;
                        true
                    } else {
                        false
                    }
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
            },
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.rename_surface(target, surface_id, title.clone())
                    || second.rename_surface(target, surface_id, title)
            }
        }
    }

    /// Update a browser surface's stored URL — webview navigate 시
    /// 호출되어 다음 실행 때 같은 페이지로 복원되도록 한다. 매칭되는
    /// surface가 browser kind일 때만 적용. URL이 그대로면 false 반환
    /// (호출자는 dirty mark을 생략 가능).
    pub fn set_surface_browser_url(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        new_url: String,
    ) -> bool {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => {
                    if let Some(surface) =
                        surfaces.iter_mut().find(|surface| surface.id == surface_id)
                    {
                        if let SurfaceKind::Browser { initial_url } = &mut surface.kind {
                            if initial_url.as_deref() == Some(new_url.as_str()) {
                                return false;
                            }
                            *initial_url = Some(new_url);
                            return true;
                        }
                    }
                    false
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
            },
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.set_surface_browser_url(target, surface_id, new_url.clone())
                    || second.set_surface_browser_url(target, surface_id, new_url)
            }
        }
    }

    /// Auto-rename a surface from an external signal (browser page title,
    /// terminal OSC, …) — title_locked = true 인 surface는 사용자가 직접
    /// rename한 것이므로 건너뛴다. 빈 문자열이거나 동일 타이틀이면 false.
    pub fn set_surface_title_auto(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        new_title: String,
    ) -> bool {
        if new_title.trim().is_empty() {
            return false;
        }
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => {
                    if let Some(surface) =
                        surfaces.iter_mut().find(|surface| surface.id == surface_id)
                    {
                        if surface.title_locked || surface.title == new_title {
                            return false;
                        }
                        // OSC 0/2가 사실은 셸 PS1의 cwd 에코이면 버린다.
                        // 그렇지 않으면 cwd가 바뀔 때마다 셸이 매 프롬프트
                        // 마다 보내는 `user@host: /path` 가 폴더 이름을
                        // 덮어써 탭 라벨/윈도우 타이틀이 PS1 형태로 굳어
                        // 버린다. flowmux-app 측 터미널 cwd-notify가 별도로
                        // `terminal_tab_title_for_cwd`를 통해 폴더 이름을
                        // 다시 set_surface_cwd로 반영하므로 그 흐름만 살리면
                        // 충분하다. vi/codex/claude/tmux 같은 외부 프로그램
                        // 타이틀은 PS1 패턴에 안 걸리므로 그대로 통과.
                        if let SurfaceKind::Terminal { cwd: Some(cwd), .. } = &surface.kind {
                            let home = std::env::var_os("HOME").map(PathBuf::from);
                            if title_is_shell_cwd_echo(&new_title, cwd, home.as_deref()) {
                                return false;
                            }
                        }
                        surface.title = new_title;
                        return true;
                    }
                    false
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
            },
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.set_surface_title_auto(target, surface_id, new_title.clone())
                    || second.set_surface_title_auto(target, surface_id, new_title)
            }
        }
    }

    pub fn set_surface_cwd(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        new_cwd: PathBuf,
    ) -> bool {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => {
                    if let Some(surface) =
                        surfaces.iter_mut().find(|surface| surface.id == surface_id)
                    {
                        if let SurfaceKind::Terminal { cwd, .. } = &mut surface.kind {
                            // cwd가 실제로 바뀔 때만 폴더-기반 라벨로 갱신.
                            // cwd가 같다면 OSC 0/2로 들어와 있는 외부 프로그램
                            // 타이틀(예: "Claude Code", "vi …")을 polling이
                            // 매 tick마다 덮어 쓰지 않도록 surface.title을
                            // 건드리지 않는다.
                            if cwd.as_ref() == Some(&new_cwd) {
                                return false;
                            }
                            *cwd = Some(new_cwd);
                            if !surface.title_locked {
                                let next_title =
                                    terminal_tab_title_for_cwd(cwd.as_deref());
                                if surface.title != next_title {
                                    surface.title = next_title;
                                }
                            }
                            return true;
                        }
                    }
                    false
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
            },
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.set_surface_cwd(target, surface_id, new_cwd.clone())
                    || second.set_surface_cwd(target, surface_id, new_cwd)
            }
        }
    }

    /// 같은 pane 안에서 `surface_id`로 식별되는 탭(터미널 또는 탭브라우저)을
    /// `target_index` 위치로 옮긴다. `target_index`는 이동 후의 최종
    /// 인덱스이며 탭 수를 넘으면 마지막으로 클램프된다. 활성 탭의
    /// `SurfaceId`는 그대로 유지되므로 옮긴 뒤에도 같은 탭이 활성으로
    /// 남는다. 매칭되는 surface가 없거나 같은 자리이면 `false`를 반환해
    /// 호출자가 dirty mark / GTK 위젯 이동을 건너뛸 수 있도록 한다.
    pub fn reorder_surface_in_leaf(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        target_index: usize,
    ) -> bool {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { surfaces, .. } => {
                    let Some(current) = surfaces.iter().position(|s| s.id == surface_id) else {
                        return false;
                    };
                    let len = surfaces.len();
                    if len == 0 {
                        return false;
                    }
                    let new_index = target_index.min(len - 1);
                    if current == new_index {
                        return false;
                    }
                    let removed = surfaces.remove(current);
                    surfaces.insert(new_index, removed);
                    true
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
            },
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.reorder_surface_in_leaf(target, surface_id, target_index)
                    || second.reorder_surface_in_leaf(target, surface_id, target_index)
            }
        }
    }

    pub fn close_surface_in_leaf(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
    ) -> CloseSurfaceOutcome {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { active, surfaces } => {
                    let Some(idx) = surfaces.iter().position(|surface| surface.id == surface_id)
                    else {
                        return CloseSurfaceOutcome::NotFound;
                    };
                    surfaces.remove(idx);
                    if surfaces.is_empty() {
                        CloseSurfaceOutcome::LastSurfaceRemoved
                    } else {
                        if *active == surface_id
                            || !surfaces.iter().any(|surface| surface.id == *active)
                        {
                            *active = surfaces[idx.saturating_sub(1).min(surfaces.len() - 1)].id;
                        }
                        CloseSurfaceOutcome::SurfaceRemoved
                    }
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => {
                    CloseSurfaceOutcome::NotFound
                }
            },
            Pane::Leaf { .. } => CloseSurfaceOutcome::NotFound,
            Pane::Split { first, second, .. } => {
                let first_outcome = first.close_surface_in_leaf(target, surface_id);
                if matches!(first_outcome, CloseSurfaceOutcome::NotFound) {
                    second.close_surface_in_leaf(target, surface_id)
                } else {
                    first_outcome
                }
            }
        }
    }

    pub fn active_surface_id(&self, target: PaneId) -> Option<SurfaceId> {
        match self {
            Pane::Leaf { id, content } if *id == target => content.active_surface().map(|s| s.id),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .active_surface_id(target)
                .or_else(|| second.active_surface_id(target)),
        }
    }

    pub fn surface_count(&self, target: PaneId) -> Option<usize> {
        match self {
            Pane::Leaf { id, content } if *id == target => Some(content.surface_count()),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .surface_count(target)
                .or_else(|| second.surface_count(target)),
        }
    }

    /// Look up `(target_pane, target_surface)` and return a clone of the
    /// matching `PaneSurface`. Returns `None` if the pane or surface is
    /// not found.
    pub fn find_surface(&self, target_pane: PaneId, target: SurfaceId) -> Option<PaneSurface> {
        match self {
            Pane::Leaf { id, content } if *id == target_pane => match content {
                PaneContent::Tabs { surfaces, .. } => {
                    surfaces.iter().find(|s| s.id == target).cloned()
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
            },
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .find_surface(target_pane, target)
                .or_else(|| second.find_surface(target_pane, target)),
        }
    }

    /// `child`(leaf 또는 split)를 직접 자식으로 갖고 있는 Split 노드의
    /// `PaneId`를 반환. 인접 split이 아니면 더 깊이 재귀해서 찾는다.
    /// `child`가 트리의 루트이거나 트리 안에 없으면 `None`.
    ///
    /// incremental split이 끝난 직후 새 sibling을 통해 방금 만들어진
    /// Split 노드의 PaneId를 GTK 측에서 조회할 때 쓴다.
    pub fn parent_split_id(&self, child: PaneId) -> Option<PaneId> {
        if let Pane::Split {
            id, first, second, ..
        } = self
        {
            let first_id = match first.as_ref() {
                Pane::Leaf { id, .. } => *id,
                Pane::Split { id, .. } => *id,
            };
            let second_id = match second.as_ref() {
                Pane::Leaf { id, .. } => *id,
                Pane::Split { id, .. } => *id,
            };
            if first_id == child || second_id == child {
                return Some(*id);
            }
            return first
                .parent_split_id(child)
                .or_else(|| second.parent_split_id(child));
        }
        None
    }

    /// `target` 으로 식별되는 Split 노드의 ratio를 갱신. ratio가 0/1
    /// 양 끝으로 가는 걸 막기 위해 [0.05, 0.95]로 클램프하고, 의미
    /// 있는 변화가 있을 때만 `true` 반환 — 호출자가 dirty mark를
    /// 건너뛸 수 있도록.
    pub fn set_split_ratio(&mut self, target: PaneId, new_ratio: f32) -> bool {
        let clamped = new_ratio.clamp(0.05, 0.95);
        match self {
            Pane::Split {
                id,
                ratio,
                first,
                second,
                ..
            } => {
                if *id == target {
                    if (*ratio - clamped).abs() < f32::EPSILON {
                        return false;
                    }
                    *ratio = clamped;
                    return true;
                }
                first.set_split_ratio(target, clamped) || second.set_split_ratio(target, clamped)
            }
            Pane::Leaf { .. } => false,
        }
    }

    /// `target` 으로 식별되는 leaf의 [`PaneContent`] 클론을 반환한다.
    /// 같은 트리 안에 매칭되는 leaf가 없거나 target가 split 노드면
    /// `None`. incremental split 경로가 새로 생긴 sibling pane의 초기
    /// 컨텐츠(터미널 / 탭브라우저)를 GTK 위젯으로 빌드할 때 쓴다.
    pub fn find_leaf_content(&self, target: PaneId) -> Option<PaneContent> {
        match self {
            Pane::Leaf { id, content } if *id == target => Some(content.clone()),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .find_leaf_content(target)
                .or_else(|| second.find_leaf_content(target)),
        }
    }

    pub fn first_leaf_id(&self) -> Option<PaneId> {
        match self {
            Pane::Leaf { id, .. } => Some(*id),
            Pane::Split { first, .. } => first.first_leaf_id(),
        }
    }

    /// Remove the leaf with `target`. If found inside a split, the
    /// split collapses to the remaining sibling. Returns `RemoveOutcome`
    /// describing what happened so the caller can update its state.
    pub fn remove_leaf(self, target: PaneId) -> RemoveOutcome {
        match self {
            Pane::Leaf { id, .. } if id == target => RemoveOutcome::EntirelyRemoved,
            leaf @ Pane::Leaf { .. } => RemoveOutcome::NotFound(leaf),
            Pane::Split {
                id,
                direction,
                ratio,
                first,
                second,
            } => match first.remove_leaf(target) {
                RemoveOutcome::Replaced(new_first) => RemoveOutcome::Replaced(Pane::Split {
                    id,
                    direction,
                    ratio,
                    first: Box::new(new_first),
                    second,
                }),
                RemoveOutcome::EntirelyRemoved => RemoveOutcome::Replaced(*second),
                RemoveOutcome::NotFound(orig_first) => match second.remove_leaf(target) {
                    RemoveOutcome::Replaced(new_second) => RemoveOutcome::Replaced(Pane::Split {
                        id,
                        direction,
                        ratio,
                        first: Box::new(orig_first),
                        second: Box::new(new_second),
                    }),
                    RemoveOutcome::EntirelyRemoved => RemoveOutcome::Replaced(orig_first),
                    RemoveOutcome::NotFound(orig_second) => RemoveOutcome::NotFound(Pane::Split {
                        id,
                        direction,
                        ratio,
                        first: Box::new(orig_first),
                        second: Box::new(orig_second),
                    }),
                },
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseSurfaceOutcome {
    SurfaceRemoved,
    LastSurfaceRemoved,
    NotFound,
}

/// Outcome of [`Pane::remove_leaf`].
pub enum RemoveOutcome {
    /// Returned only at the root: the entire tree was a single leaf
    /// matching the target, so the surface is now empty.
    EntirelyRemoved,
    /// The tree mutated; this is the new root.
    Replaced(Pane),
    /// The target wasn't anywhere in this subtree; original returned.
    NotFound(Pane),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneContent {
    /// New cmux-compatible shape: pane-local tabs.
    Tabs {
        active: SurfaceId,
        surfaces: Vec<PaneSurface>,
    },
    /// Legacy state shape. New code normalizes these into
    /// [`PaneContent::Tabs`] on load/create.
    Terminal { pid: Option<u32> },
    /// Legacy state shape. New code normalizes these into
    /// [`PaneContent::Tabs`] on load/create.
    Browser { url: String },
}

impl PaneSurface {
    pub fn terminal(title: impl Into<String>, cwd: Option<PathBuf>) -> Self {
        Self {
            id: SurfaceId::new(),
            title: title.into(),
            title_locked: false,
            kind: SurfaceKind::Terminal { shell: None, cwd },
        }
    }

    pub fn browser(title: impl Into<String>, url: String) -> Self {
        Self {
            id: SurfaceId::new(),
            title: title.into(),
            title_locked: false,
            kind: SurfaceKind::Browser {
                initial_url: Some(url),
            },
        }
    }
}

impl PaneContent {
    pub fn tabbed_terminal(title: impl Into<String>, cwd: Option<PathBuf>) -> Self {
        let surface = PaneSurface::terminal(title, cwd);
        Self::Tabs {
            active: surface.id,
            surfaces: vec![surface],
        }
    }

    pub fn tabbed_browser(title: impl Into<String>, url: String) -> Self {
        let surface = PaneSurface::browser(title, url);
        Self::Tabs {
            active: surface.id,
            surfaces: vec![surface],
        }
    }

    pub fn active_surface(&self) -> Option<&PaneSurface> {
        match self {
            PaneContent::Tabs { active, surfaces } => surfaces
                .iter()
                .find(|surface| surface.id == *active)
                .or_else(|| surfaces.first()),
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
        }
    }

    pub fn active_surface_mut(&mut self) -> Option<&mut PaneSurface> {
        match self {
            PaneContent::Tabs { active, surfaces } => {
                let idx = surfaces
                    .iter()
                    .position(|surface| surface.id == *active)
                    .unwrap_or(0);
                surfaces.get_mut(idx)
            }
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
        }
    }

    pub fn surface_count(&self) -> usize {
        match self {
            PaneContent::Tabs { surfaces, .. } => surfaces.len(),
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => 1,
        }
    }

    pub fn normalize_to_tabs(&mut self, fallback_cwd: Option<PathBuf>) -> bool {
        let replacement = match self {
            PaneContent::Terminal { .. } => Some(PaneContent::tabbed_terminal(
                terminal_tab_title_for_cwd(fallback_cwd.as_deref()),
                fallback_cwd,
            )),
            PaneContent::Browser { url } => {
                Some(PaneContent::tabbed_browser("Browser", url.clone()))
            }
            PaneContent::Tabs { active, surfaces } => {
                if surfaces.is_empty() {
                    let surface = PaneSurface::terminal(
                        terminal_tab_title_for_cwd(fallback_cwd.as_deref()),
                        fallback_cwd,
                    );
                    *active = surface.id;
                    surfaces.push(surface);
                    return true;
                } else {
                    let mut changed = false;
                    for surface in surfaces.iter_mut() {
                        changed |= normalize_unlocked_terminal_title(surface);
                    }
                    if !surfaces.iter().any(|surface| surface.id == *active) {
                        *active = surfaces[0].id;
                        changed = true;
                    }
                    return changed;
                }
            }
        };
        if let Some(replacement) = replacement {
            *self = replacement;
            return true;
        }
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub level: NotificationLevel,
    pub title: String,
    pub body: String,
    pub source_pane: Option<PaneId>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub read: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    /// Generic info — no badge.
    Info,
    /// Agent is waiting for the user; pane gets the blue ring, tab badges,
    /// and the workspace bumps to the top of the unread list.
    AttentionNeeded,
    Error,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid pane id: {0}")]
    InvalidPaneId(PaneId),
    #[error("invalid split ratio: {0} (must be 0 < ratio < 1)")]
    InvalidSplitRatio(f32),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_leaf_replaces_target_with_a_split() {
        let leaf_id = PaneId::new();
        let mut p = Pane::Leaf {
            id: leaf_id,
            content: PaneContent::Terminal { pid: None },
        };
        let new_id = p
            .split_leaf(
                leaf_id,
                SplitDirection::Vertical,
                0.5,
                PaneContent::Terminal { pid: None },
            )
            .unwrap();
        match &p {
            Pane::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Vertical);
                assert!(matches!(**first, Pane::Leaf { id, .. } if id == leaf_id));
                assert!(matches!(**second, Pane::Leaf { id, .. } if id == new_id));
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn split_leaf_recurses_into_existing_split() {
        let l1 = PaneId::new();
        let l2 = PaneId::new();
        let mut p = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l1,
                content: PaneContent::Terminal { pid: None },
            }),
            second: Box::new(Pane::Leaf {
                id: l2,
                content: PaneContent::Terminal { pid: None },
            }),
        };
        let new_id = p
            .split_leaf(
                l2,
                SplitDirection::Vertical,
                0.5,
                PaneContent::Terminal { pid: None },
            )
            .unwrap();
        let mut leaves = vec![];
        p.for_each_leaf(|id| leaves.push(id));
        assert_eq!(leaves.len(), 3);
        assert!(leaves.contains(&l1));
        assert!(leaves.contains(&l2));
        assert!(leaves.contains(&new_id));
    }

    #[test]
    fn remove_leaf_collapses_split() {
        let l1 = PaneId::new();
        let l2 = PaneId::new();
        let p = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l1,
                content: PaneContent::Terminal { pid: None },
            }),
            second: Box::new(Pane::Leaf {
                id: l2,
                content: PaneContent::Terminal { pid: None },
            }),
        };
        match p.remove_leaf(l1) {
            RemoveOutcome::Replaced(Pane::Leaf { id, .. }) => assert_eq!(id, l2),
            _ => panic!("expected leaf l2 to remain after l1 removal"),
        }
    }

    #[test]
    fn remove_leaf_returns_entirely_removed_on_root_match() {
        let id = PaneId::new();
        let p = Pane::Leaf {
            id,
            content: PaneContent::Terminal { pid: None },
        };
        assert!(matches!(p.remove_leaf(id), RemoveOutcome::EntirelyRemoved));
    }

    #[test]
    fn remove_leaf_returns_not_found_when_id_missing() {
        let id = PaneId::new();
        let other = PaneId::new();
        let p = Pane::Leaf {
            id,
            content: PaneContent::Terminal { pid: None },
        };
        assert!(matches!(p.remove_leaf(other), RemoveOutcome::NotFound(_)));
    }

    #[test]
    fn pane_content_normalizes_legacy_terminal_to_surface_tab() {
        let cwd = PathBuf::from("/tmp/flowmux-core-test");
        let mut content = PaneContent::Terminal { pid: Some(123) };

        content.normalize_to_tabs(Some(cwd.clone()));

        let PaneContent::Tabs { active, surfaces } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].id, active);
        assert_eq!(surfaces[0].title, "flowmux-core-test");
        assert!(matches!(
            &surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(got), .. } if got == &cwd
        ));
    }

    #[test]
    fn terminal_tab_title_for_cwd_uses_folder_and_truncates() {
        assert_eq!(
            terminal_tab_title_for_cwd(Some(Path::new("/tmp/project"))),
            "project"
        );
        assert_eq!(
            terminal_tab_title_for_cwd(Some(Path::new("/tmp/1234567890123456"))),
            "123456789012345..."
        );
        assert_eq!(terminal_tab_title_for_cwd(Some(Path::new("/"))), "Terminal");
    }

    #[test]
    fn pane_content_normalizes_legacy_terminal_number_titles() {
        let mut first = PaneSurface::terminal("Terminal 3", Some("/tmp/project".into()));
        let first_id = first.id;
        let mut locked = PaneSurface::terminal("Terminal 4", Some("/tmp/locked".into()));
        locked.title_locked = true;
        first.title_locked = false;
        let mut content = PaneContent::Tabs {
            active: first_id,
            surfaces: vec![first, locked],
        };

        content.normalize_to_tabs(None);

        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "project");
        assert_eq!(surfaces[1].title, "Terminal 4");
    }

    #[test]
    fn pane_content_locks_non_legacy_terminal_titles_on_normalize() {
        let custom = PaneSurface::terminal("server", Some("/tmp/project".into()));
        let custom_id = custom.id;
        let mut content = PaneContent::Tabs {
            active: custom_id,
            surfaces: vec![custom],
        };

        assert!(content.normalize_to_tabs(None));

        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "server");
        assert!(surfaces[0].title_locked);
    }

    #[test]
    fn pane_content_keeps_cwd_title_unlocked_on_normalize() {
        let surface = PaneSurface::terminal("project", Some("/tmp/project".into()));
        let surface_id = surface.id;
        let mut content = PaneContent::Tabs {
            active: surface_id,
            surfaces: vec![surface],
        };

        assert!(!content.normalize_to_tabs(None));

        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "project");
        assert!(!surfaces[0].title_locked);
    }

    #[test]
    fn pane_surface_tabs_can_activate_and_close() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        let second = PaneSurface::terminal("two", None);
        let second_id = second.id;

        assert_eq!(pane.add_surface_to_leaf(pane_id, second), Some(second_id));
        assert_eq!(pane.active_surface_id(pane_id), Some(second_id));

        let first_id = match &pane {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces[0].id,
            _ => panic!("expected tabbed leaf"),
        };
        assert!(pane.set_active_surface(pane_id, first_id));
        assert_eq!(pane.active_surface_id(pane_id), Some(first_id));

        assert_eq!(
            pane.close_surface_in_leaf(pane_id, first_id),
            CloseSurfaceOutcome::SurfaceRemoved
        );
        assert_eq!(pane.active_surface_id(pane_id), Some(second_id));
        assert_eq!(
            pane.close_surface_in_leaf(pane_id, second_id),
            CloseSurfaceOutcome::LastSurfaceRemoved
        );
    }

    #[test]
    fn pane_surface_title_can_be_renamed() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        assert_eq!(pane.surface_title(pane_id, surface_id), Some("one"));
        assert!(pane.rename_surface(pane_id, surface_id, "renamed".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("renamed"));
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = pane
        else {
            panic!("expected tabbed leaf")
        };
        assert!(surfaces[0].title_locked);
    }

    #[test]
    fn terminal_surface_cwd_updates_unlocked_title_only() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", Some("/tmp/old".into())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        assert!(pane.set_surface_cwd(pane_id, surface_id, "/tmp/new".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("new"));
        assert!(!pane.set_surface_cwd(pane_id, surface_id, "/tmp/new".into()));
        assert!(pane.rename_surface(pane_id, surface_id, "fixed".into()));
        assert!(pane.set_surface_cwd(pane_id, surface_id, "/tmp/another".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("fixed"));
    }

    #[test]
    fn set_surface_cwd_preserves_program_title_when_cwd_unchanged() {
        // 회귀 방지: vi/claude 같은 외부 프로그램이 OSC 0/2로 set한 타이틀이
        // 1초 cwd polling에 의해 폴더명으로 되돌아가지 않아야 한다.
        // poll_terminal_cwds는 cwd가 그대로면 같은 cwd로 set_surface_cwd를
        // 호출하므로, 이 케이스에서 surface.title은 절대 건드리지 않아야 한다.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("os", Some("/tmp/work".into())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // 외부 프로그램 진입: set_surface_title_auto로 "Claude Code" 진입.
        assert!(pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "Claude Code".into(),
        ));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Claude Code"));

        // polling이 같은 cwd를 또 보고 — no-op이어야 하고, 타이틀은
        // "Claude Code" 그대로.
        assert!(!pane.set_surface_cwd(pane_id, surface_id, "/tmp/work".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Claude Code"));

        // 사용자가 cd로 cwd를 실제로 바꾸면 그제서야 폴더명 라벨로 복귀.
        // (외부 프로그램에서 빠져나온 자연스러운 흐름과 동치.)
        assert!(pane.set_surface_cwd(pane_id, surface_id, "/tmp/another".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("another"));

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = pane
        else {
            panic!("expected tabbed leaf")
        };
        assert!(matches!(
            &surfaces[0].kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &PathBuf::from("/tmp/another")
        ));
    }

    #[test]
    fn set_surface_browser_url_replaces_initial_url_only_for_browser_kind() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_browser("Docs", "https://one.test".into()),
        };
        let browser_id = pane.active_surface_id(pane_id).unwrap();
        assert!(pane.set_surface_browser_url(pane_id, browser_id, "https://two.test".into()));
        assert!(matches!(
            pane.find_surface(pane_id, browser_id).unwrap().kind,
            SurfaceKind::Browser { initial_url: Some(ref u) } if u == "https://two.test"
        ));
        // 같은 URL을 다시 set하면 false (no-op).
        assert!(!pane.set_surface_browser_url(pane_id, browser_id, "https://two.test".into()));

        // Terminal surface는 영향을 받지 않아야 한다.
        let term = PaneSurface::terminal("term", None);
        let term_id = term.id;
        pane.add_surface_to_leaf(pane_id, term).unwrap();
        assert!(!pane.set_surface_browser_url(pane_id, term_id, "https://x.test".into()));
    }

    #[test]
    fn set_surface_title_auto_skips_locked_titles() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_browser("Browser", "https://one.test".into()),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // 잠겨 있지 않은 surface는 자동 갱신.
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "Page Title".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Page Title"));

        // 동일 title은 no-op.
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "Page Title".into()));

        // 빈 / whitespace title은 no-op.
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "".into()));
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "   ".into()));

        // 사용자가 직접 rename → title_locked = true.
        assert!(pane.rename_surface(pane_id, surface_id, "MyName".into()));
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "Other Page".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("MyName"));
    }

    #[test]
    fn title_is_shell_cwd_echo_recognizes_bash_default_ps1() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // bash 기본 `\u@\h: \w` (절대경로 표시).
        assert!(title_is_shell_cwd_echo(
            "junsu@host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // 공백 없이 `\u@\h:\w`.
        assert!(title_is_shell_cwd_echo(
            "junsu@host:/tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // debian_chroot prefix 변형.
        assert!(title_is_shell_cwd_echo(
            "(jammy)junsu@host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // 호스트만 prefix.
        assert!(title_is_shell_cwd_echo(
            "host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // path 자체만 (PROMPT가 path만 emit하는 테마).
        assert!(title_is_shell_cwd_echo(
            "/tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
    }

    #[test]
    fn title_is_shell_cwd_echo_recognizes_tilde_form() {
        let home = Path::new("/home/junsu");
        let cwd = Path::new("/home/junsu/dev/os");
        // bash `\w`는 $HOME을 `~`로 축약.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: ~/dev/os",
            cwd,
            Some(home),
        ));
        // home 자체 (cwd == $HOME → ~).
        assert!(title_is_shell_cwd_echo(
            "junsu@host: ~",
            Path::new("/home/junsu"),
            Some(home),
        ));
        // home 정보가 없으면 tilde 매칭은 안 되지만 절대경로 매칭은 여전.
        assert!(!title_is_shell_cwd_echo(
            "junsu@host: ~/dev/os",
            cwd,
            None,
        ));
    }

    #[test]
    fn title_is_shell_cwd_echo_passes_program_titles() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // vi/codex/claude/tmux 같은 외부 프로그램이 보내는 타이틀은 PS1
        // 패턴(`prefix:[ ]<cwd>`)에 안 걸린다.
        assert!(!title_is_shell_cwd_echo(
            "vim src/main.rs",
            cwd,
            None,
        ));
        assert!(!title_is_shell_cwd_echo(
            "tmux: 0:bash*",
            cwd,
            None,
        ));
        assert!(!title_is_shell_cwd_echo(
            "claude — Anthropic",
            cwd,
            None,
        ));
        // cwd 안의 파일을 여는 vim도 통과해야 한다 — prefix가 `:`로
        // 끝나지 않으므로.
        assert!(!title_is_shell_cwd_echo(
            "vim /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // 빈 / whitespace는 echo로 오인하지 않는다 (호출자가 이미 검사
        // 하지만 helper 단독 호출 안전성 유지).
        assert!(!title_is_shell_cwd_echo("", cwd, None));
        assert!(!title_is_shell_cwd_echo("   ", cwd, None));
    }

    #[test]
    fn set_surface_title_auto_drops_shell_ps1_echo_on_terminal() {
        // 회귀 방지: 셸이 매 프롬프트마다 OSC 0/2로 보내는 PS1
        // 형태(`user@host: /path`)가 cwd 기반 폴더 이름 라벨을
        // 덮어 쓰지 않도록 한다. flowmux-app 측 cwd-notify 플로우가
        // set_surface_cwd로 폴더 이름을 별도로 반영하므로 OSC 0/2
        // 에코는 무시하는 것이 정답.
        let pane_id = PaneId::new();
        let cwd = PathBuf::from("/tmp/flowmux-shell-echo-test");
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("flowmux-shell-...", Some(cwd.clone())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // PS1 echo 들 — 모두 무시되어야 한다 (반환 false, title 그대로).
        assert!(!pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "junsu@host: /tmp/flowmux-shell-echo-test".into(),
        ));
        assert!(!pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "(jammy)junsu@host:/tmp/flowmux-shell-echo-test".into(),
        ));
        assert_eq!(
            pane.surface_title(pane_id, surface_id),
            Some("flowmux-shell-...")
        );

        // 외부 프로그램 타이틀(vi 등)은 정상적으로 반영된다.
        assert!(pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "vim src/main.rs".into(),
        ));
        assert_eq!(
            pane.surface_title(pane_id, surface_id),
            Some("vim src/main.rs")
        );
    }

    #[test]
    fn find_surface_returns_clone_of_matching_surface() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        let added = PaneSurface::browser("Docs", "https://docs.example.org".into());
        let added_id = added.id;
        assert_eq!(
            pane.add_surface_to_leaf(pane_id, added.clone()),
            Some(added_id)
        );

        let found = pane.find_surface(pane_id, added_id).expect("must find");
        assert_eq!(found.id, added_id);
        assert_eq!(found.title, "Docs");
        assert!(matches!(
            found.kind,
            SurfaceKind::Browser { initial_url: Some(ref u) } if u == "https://docs.example.org"
        ));
    }

    #[test]
    fn find_surface_returns_none_for_unknown_pane_or_surface() {
        let pane_id = PaneId::new();
        let pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("one", None),
        };
        assert!(pane.find_surface(PaneId::new(), SurfaceId::new()).is_none());
        assert!(pane.find_surface(pane_id, SurfaceId::new()).is_none());
    }

    #[test]
    fn parent_split_id_finds_immediate_owner() {
        let l = PaneId::new();
        let r = PaneId::new();
        let split_id = PaneId::new();
        let tree = Pane::Split {
            id: split_id,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: r,
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        assert_eq!(tree.parent_split_id(l), Some(split_id));
        assert_eq!(tree.parent_split_id(r), Some(split_id));
        // Split 자신은 누구의 자식도 아니므로 None — 루트 시점.
        assert_eq!(tree.parent_split_id(split_id), None);
        assert_eq!(tree.parent_split_id(PaneId::new()), None);
    }

    #[test]
    fn parent_split_id_walks_into_nested_tree() {
        let outer = PaneId::new();
        let inner = PaneId::new();
        let l = PaneId::new();
        let m = PaneId::new();
        let r = PaneId::new();
        let tree = Pane::Split {
            id: outer,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Split {
                id: inner,
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(Pane::Leaf {
                    id: m,
                    content: PaneContent::tabbed_terminal("M", None),
                }),
                second: Box::new(Pane::Leaf {
                    id: r,
                    content: PaneContent::tabbed_terminal("R", None),
                }),
            }),
        };
        assert_eq!(tree.parent_split_id(l), Some(outer));
        assert_eq!(tree.parent_split_id(inner), Some(outer));
        assert_eq!(tree.parent_split_id(m), Some(inner));
        assert_eq!(tree.parent_split_id(r), Some(inner));
    }

    #[test]
    fn set_split_ratio_updates_matching_node_only() {
        let outer = PaneId::new();
        let inner = PaneId::new();
        let l = PaneId::new();
        let m = PaneId::new();
        let r = PaneId::new();
        let mut tree = Pane::Split {
            id: outer,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Split {
                id: inner,
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(Pane::Leaf {
                    id: m,
                    content: PaneContent::tabbed_terminal("M", None),
                }),
                second: Box::new(Pane::Leaf {
                    id: r,
                    content: PaneContent::tabbed_terminal("R", None),
                }),
            }),
        };
        assert!(tree.set_split_ratio(outer, 0.7));
        assert!(tree.set_split_ratio(inner, 0.3));
        assert!(!tree.set_split_ratio(PaneId::new(), 0.5));
        assert!(!tree.set_split_ratio(l, 0.5));

        let Pane::Split {
            ratio: outer_r,
            second,
            ..
        } = &tree
        else {
            panic!("expected outer split")
        };
        assert!((outer_r - 0.7).abs() < 0.001);
        let Pane::Split { ratio: inner_r, .. } = second.as_ref() else {
            panic!("expected inner split")
        };
        assert!((inner_r - 0.3).abs() < 0.001);
    }

    #[test]
    fn set_split_ratio_clamps_extreme_values() {
        let split_id = PaneId::new();
        let mut tree = Pane::Split {
            id: split_id,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        assert!(tree.set_split_ratio(split_id, 1.0));
        let Pane::Split { ratio, .. } = &tree else {
            unreachable!()
        };
        assert!((ratio - 0.95).abs() < 0.001);

        assert!(tree.set_split_ratio(split_id, 0.0));
        let Pane::Split { ratio, .. } = &tree else {
            unreachable!()
        };
        assert!((ratio - 0.05).abs() < 0.001);
    }

    #[test]
    fn set_split_ratio_returns_false_when_unchanged() {
        let split_id = PaneId::new();
        let mut tree = Pane::Split {
            id: split_id,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        assert!(!tree.set_split_ratio(split_id, 0.5));
    }

    #[test]
    fn find_leaf_content_returns_clone_for_matching_leaf() {
        let leaf = PaneId::new();
        let tree = Pane::Leaf {
            id: leaf,
            content: PaneContent::tabbed_terminal("solo", None),
        };
        let content = tree.find_leaf_content(leaf).expect("leaf must match");
        let PaneContent::Tabs { surfaces, .. } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces[0].title, "solo");

        // 다른 PaneId는 None.
        assert!(tree.find_leaf_content(PaneId::new()).is_none());
    }

    #[test]
    fn find_leaf_content_walks_split_tree() {
        let l = PaneId::new();
        let r = PaneId::new();
        let tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("left", None),
            }),
            second: Box::new(Pane::Leaf {
                id: r,
                content: PaneContent::tabbed_browser("Docs", "https://r.test".into()),
            }),
        };
        let PaneContent::Tabs { surfaces, .. } = tree.find_leaf_content(l).unwrap() else {
            panic!("expected tabs")
        };
        assert_eq!(surfaces[0].title, "left");
        let PaneContent::Tabs { surfaces, .. } = tree.find_leaf_content(r).unwrap() else {
            panic!("expected tabs")
        };
        assert_eq!(surfaces[0].title, "Docs");

        // split 자체의 PaneId는 leaf가 아니므로 None.
        let split_id = match &tree {
            Pane::Split { id, .. } => *id,
            _ => unreachable!(),
        };
        assert!(tree.find_leaf_content(split_id).is_none());
    }

    #[test]
    fn split_leaf_preserves_target_pane_id_and_creates_fresh_sibling() {
        // incremental split의 핵심 가정 — 분할 후 target의 PaneId는
        // 그대로 유지되고, sibling은 새 PaneId를 받는다. 이 시나리오가
        // 깨지면 GTK 측 PaneRegistry::pane_frame(target_pane) 조회가
        // 빗나가 다른 pane을 통째로 rebuild하는 회귀가 발생한다.
        let target = PaneId::new();
        let mut tree = Pane::Leaf {
            id: target,
            content: PaneContent::tabbed_terminal("orig", Some("/tmp/orig".into())),
        };
        let new_pane = tree
            .split_leaf(
                target,
                SplitDirection::Vertical,
                0.5,
                PaneContent::tabbed_terminal("fresh", Some("/tmp/orig".into())),
            )
            .expect("split must succeed");
        assert_ne!(new_pane, target);

        let mut leaves = Vec::new();
        tree.for_each_leaf(|id| leaves.push(id));
        assert!(leaves.contains(&target));
        assert!(leaves.contains(&new_pane));

        // target의 컨텐츠는 원래대로, new_pane는 fresh 컨텐츠.
        let target_content = tree.find_leaf_content(target).unwrap();
        let new_content = tree.find_leaf_content(new_pane).unwrap();
        let (PaneContent::Tabs { surfaces: t_surfs, .. }, PaneContent::Tabs { surfaces: n_surfs, .. }) =
            (&target_content, &new_content)
        else {
            panic!("expected tabbed content for both")
        };
        assert_eq!(t_surfs[0].title, "orig");
        assert_eq!(n_surfs[0].title, "fresh");
    }

    #[test]
    fn split_leaf_inside_existing_split_preserves_neighbor_pane_id() {
        // 이미 split 트리 안에 있는 한 pane을 다시 split해도, 같은 split
        // 안의 다른 sibling pane의 PaneId는 그대로다. GTK 측에서 sibling의
        // gtk::Frame을 그대로 이어 갈 수 있음을 보장.
        let l = PaneId::new();
        let r = PaneId::new();
        let mut tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: l,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: r,
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };

        let new_under_l = tree
            .split_leaf(
                l,
                SplitDirection::Horizontal,
                0.5,
                PaneContent::tabbed_terminal("L2", None),
            )
            .unwrap();

        // r 은 그대로 leaf 로 유지.
        assert!(matches!(
            tree.find_leaf_content(r),
            Some(PaneContent::Tabs { .. })
        ));
        // l 도 새 split의 일원으로 살아남고, l 자체의 PaneId는 보존.
        assert!(matches!(
            tree.find_leaf_content(l),
            Some(PaneContent::Tabs { .. })
        ));
        // 새 sibling 등록.
        assert!(tree.find_leaf_content(new_under_l).is_some());
        assert_ne!(new_under_l, l);
        assert_ne!(new_under_l, r);
    }

    #[test]
    fn find_surface_walks_into_split_branches() {
        let left_id = PaneId::new();
        let right_id = PaneId::new();
        let mut tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: left_id,
                content: PaneContent::tabbed_terminal("L", None),
            }),
            second: Box::new(Pane::Leaf {
                id: right_id,
                content: PaneContent::tabbed_terminal("R", None),
            }),
        };
        let added = PaneSurface::browser("RBrowser", "https://r.test".into());
        let added_id = added.id;
        assert_eq!(tree.add_surface_to_leaf(right_id, added), Some(added_id));

        // 잘못된 (pane, surface) 매칭은 None — surface는 right pane에만 존재.
        assert!(tree.find_surface(left_id, added_id).is_none());
        let found = tree.find_surface(right_id, added_id).unwrap();
        assert_eq!(found.id, added_id);
        assert_eq!(found.title, "RBrowser");
    }

    /// pane 내부 탭 reorder 시나리오 모음. surface_id 기반으로
    /// active 탭이 보존되는지, terminal과 탭브라우저가 섞여 있어도
    /// 정상 이동하는지, 인덱스 클램프와 no-op 분기가 모두 동작하는지를
    /// 한 번에 본다.
    #[test]
    fn reorder_surface_moves_first_to_last_and_preserves_active() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        let b_id = b.id;
        let c = PaneSurface::browser("c", "https://c.test".into());
        let c_id = c.id;
        pane.add_surface_to_leaf(pane_id, b).unwrap();
        pane.add_surface_to_leaf(pane_id, c).unwrap();
        // a를 활성으로 되돌려 둔다 — c가 마지막으로 추가돼서 active.
        assert!(pane.set_active_surface(pane_id, a_id));

        assert!(pane.reorder_surface_in_leaf(pane_id, a_id, 2));

        let Pane::Leaf {
            content: PaneContent::Tabs { active, surfaces },
            ..
        } = &pane
        else {
            panic!("expected tabbed leaf")
        };
        let order: Vec<SurfaceId> = surfaces.iter().map(|s| s.id).collect();
        assert_eq!(order, vec![b_id, c_id, a_id]);
        // a를 옮겼지만 active는 여전히 a여야 한다.
        assert_eq!(*active, a_id);
    }

    #[test]
    fn reorder_surface_moves_last_to_first() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        let b_id = b.id;
        let c = PaneSurface::terminal("c", None);
        let c_id = c.id;
        pane.add_surface_to_leaf(pane_id, b).unwrap();
        pane.add_surface_to_leaf(pane_id, c).unwrap();

        assert!(pane.reorder_surface_in_leaf(pane_id, c_id, 0));

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![c_id, a_id, b_id]
        );
    }

    #[test]
    fn reorder_surface_clamps_target_beyond_len() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        let b_id = b.id;
        pane.add_surface_to_leaf(pane_id, b).unwrap();

        // target_index=999 → 끝으로 클램프 → b, a
        assert!(pane.reorder_surface_in_leaf(pane_id, a_id, 999));

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected tabbed leaf")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![b_id, a_id]
        );
    }

    #[test]
    fn reorder_surface_same_position_returns_false() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let a_id = pane.active_surface_id(pane_id).unwrap();
        let b = PaneSurface::terminal("b", None);
        pane.add_surface_to_leaf(pane_id, b).unwrap();

        // a가 이미 인덱스 0이므로 0으로 옮겨도 no-op.
        assert!(!pane.reorder_surface_in_leaf(pane_id, a_id, 0));
        // 길이를 넘어도 자기 자리(끝)로 클램프되면 마찬가지로 no-op.
        let last = pane
            .find_surface(
                pane_id,
                match &pane {
                    Pane::Leaf {
                        content: PaneContent::Tabs { surfaces, .. },
                        ..
                    } => surfaces.last().unwrap().id,
                    _ => unreachable!(),
                },
            )
            .unwrap()
            .id;
        assert!(!pane.reorder_surface_in_leaf(pane_id, last, 100));
    }

    #[test]
    fn reorder_surface_unknown_surface_returns_false() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", None),
        };
        let missing = SurfaceId::new();
        assert!(!pane.reorder_surface_in_leaf(pane_id, missing, 0));
    }

    #[test]
    fn reorder_surface_single_tab_is_noop() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("only", None),
        };
        let only = pane.active_surface_id(pane_id).unwrap();
        assert!(!pane.reorder_surface_in_leaf(pane_id, only, 0));
        assert!(!pane.reorder_surface_in_leaf(pane_id, only, 5));
    }

    /// terminal 두 개 + 탭브라우저 한 개를 만들고 가운데 탭브라우저를
    /// 양 끝으로 보내며 순서 + active 보존을 확인한다.
    #[test]
    fn reorder_surface_mixed_terminal_and_browser() {
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("term", None),
        };
        let term_id = pane.active_surface_id(pane_id).unwrap();
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        let term2 = PaneSurface::terminal("term2", None);
        let term2_id = term2.id;
        pane.add_surface_to_leaf(pane_id, browser).unwrap();
        pane.add_surface_to_leaf(pane_id, term2).unwrap();
        // 탭브라우저(중간)를 active로.
        assert!(pane.set_active_surface(pane_id, browser_id));

        // 가운데 → 처음
        assert!(pane.reorder_surface_in_leaf(pane_id, browser_id, 0));
        assert_active_order(&pane, pane_id, browser_id, &[browser_id, term_id, term2_id]);

        // 처음 → 마지막
        assert!(pane.reorder_surface_in_leaf(pane_id, browser_id, 2));
        assert_active_order(&pane, pane_id, browser_id, &[term_id, term2_id, browser_id]);
    }

    /// split 트리 안 깊숙한 leaf의 탭을 reorder. 다른 leaf는 영향 없어야.
    #[test]
    fn reorder_surface_walks_into_split_branches() {
        let left_id = PaneId::new();
        let right_id = PaneId::new();
        let mut tree = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(Pane::Leaf {
                id: left_id,
                content: PaneContent::tabbed_terminal("L0", None),
            }),
            second: Box::new(Pane::Leaf {
                id: right_id,
                content: PaneContent::tabbed_terminal("R0", None),
            }),
        };
        let r0 = tree.active_surface_id(right_id).unwrap();
        let r1 = PaneSurface::terminal("R1", None);
        let r1_id = r1.id;
        let r2 = PaneSurface::browser("R2", "https://r2.test".into());
        let r2_id = r2.id;
        tree.add_surface_to_leaf(right_id, r1).unwrap();
        tree.add_surface_to_leaf(right_id, r2).unwrap();
        let l0 = tree.active_surface_id(left_id).unwrap();

        // right pane의 R2(마지막)를 첫 번째로.
        assert!(tree.reorder_surface_in_leaf(right_id, r2_id, 0));
        assert_active_order(&tree, right_id, r2_id, &[r2_id, r0, r1_id]);

        // left pane은 그대로여야 한다.
        assert_active_order(&tree, left_id, l0, &[l0]);

        // 잘못된 (pane, surface) 매칭은 false.
        assert!(!tree.reorder_surface_in_leaf(left_id, r2_id, 0));
    }

    fn assert_active_order(
        pane: &Pane,
        target: PaneId,
        expected_active: SurfaceId,
        expected_order: &[SurfaceId],
    ) {
        fn find_tabs<'a>(p: &'a Pane, target: PaneId) -> Option<&'a PaneContent> {
            match p {
                Pane::Leaf { id, content } if *id == target => Some(content),
                Pane::Leaf { .. } => None,
                Pane::Split { first, second, .. } => {
                    find_tabs(first, target).or_else(|| find_tabs(second, target))
                }
            }
        }
        let Some(PaneContent::Tabs { active, surfaces }) = find_tabs(pane, target) else {
            panic!("target leaf {target} not found or not tabbed");
        };
        assert_eq!(*active, expected_active);
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            expected_order
        );
    }

    #[test]
    fn workspace_roundtrips_through_json() {
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "demo".into(),
            custom_title: None,
            root_dir: PathBuf::from("/tmp/demo"),
            git: None,
            listening_ports: vec![3000, 5173],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: PaneId::new(),
                    content: PaneContent::Terminal { pid: None },
                },
            }],
            color: None,
        };
        let s = serde_json::to_string(&ws).unwrap();
        let back: Workspace = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, ws.name);
        assert_eq!(back.custom_title, None);
    }

    #[test]
    fn display_title_falls_back_to_name_when_custom_unset() {
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: None,
            root_dir: PathBuf::from("/tmp/auto"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        };
        assert_eq!(ws.display_title(), "auto");
    }

    #[test]
    fn display_title_prefers_custom_title_when_set() {
        let mut ws = Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: Some("My Project".into()),
            root_dir: PathBuf::from("/tmp/auto"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        };
        assert_eq!(ws.display_title(), "My Project");

        // 자동 갱신으로 name이 바뀌어도 custom_title이 우선.
        ws.name = "updated-auto".into();
        assert_eq!(ws.display_title(), "My Project");
    }

    #[test]
    fn display_title_treats_empty_custom_as_unset() {
        // 방어: 어떤 경로로 빈 문자열이 저장되더라도 표시는 자동 모드.
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: Some("".into()),
            root_dir: PathBuf::from("/tmp/auto"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![],
            color: None,
        };
        assert_eq!(ws.display_title(), "auto");
    }

    #[test]
    fn workspace_loads_legacy_state_without_custom_title() {
        // 이전 버전 state.json은 custom_title 필드가 없다.
        // #[serde(default)] 덕에 None으로 로드되어야 한다.
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "old-project",
            "root_dir": "/tmp/old",
            "git": null,
            "surfaces": [],
            "color": null
        }"#;
        let ws: Workspace = serde_json::from_str(json).unwrap();
        assert_eq!(ws.name, "old-project");
        assert_eq!(ws.custom_title, None);
        assert_eq!(ws.display_title(), "old-project");
    }
}
