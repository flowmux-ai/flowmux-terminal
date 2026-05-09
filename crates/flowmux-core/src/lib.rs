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

/// Determine whether an OSC 0/2 title from a terminal surface is the shell's
/// PS1 window title, effectively an echo of cwd. If true, callers should ignore
/// the title and keep the cwd-based folder name.
///
/// Recognized patterns:
/// * Title equals the cwd absolute path itself (`/tmp/foo`).
/// * Title ends with `<prefix>:[ ]<cwd>`, with prefix ending in `:`, as in
///   default bash `\u@\h: \w` or debian_chroot variants. `<cwd>` may be an
///   absolute path or `$HOME` abbreviated to `~`.
///
/// Titles from external programs such as vi, codex, claude, or tmux, for
/// example `vi src/main.rs` or `tmux: 0:bash*`, do not match this structure and
/// pass through. Callers drop PS1 echoes but accept program titles.
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
    // Default bash PS1 puts a space between `:` and path (`\u@\h: \w`).
    // Older prompts or some zsh themes may omit the space (`\u@\h:\w`).
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
                /// Accepts a bare UUID (`<uuid>`) and the cmux-style
                /// prefixed forms (`surface:<uuid>`, `pane:<uuid>`,
                /// `workspace:<uuid>`, …). Anything before the first
                /// `:` is treated as a label and ignored, so an agent
                /// that learned the cmux CLI shape ports unchanged.
                ///
                /// (`surface:<integer>` numeric refs in cmux require a
                /// daemon-side index lookup; those are not parsed
                /// here — the CLI surfaces them as a separate IPC
                /// verb when implemented.)
                fn from_str(s: &str) -> Result<Self, Self::Err> {
                    let inner = s.split_once(':').map(|(_, rest)| rest).unwrap_or(s);
                    Ok(Self(inner.parse()?))
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
    /// Automatically determined workspace name. It starts as the last folder of
    /// root_dir when the workspace is created and may update from daemon-side
    /// automatic signals such as PTY OSC or cwd changes. This corresponds to
    /// cmux's `processTitle`: the latest system-observed value, not user intent.
    pub name: String,
    /// Name entered by the user through the right-click "Change tab name" menu.
    /// `None` means automatic mode, showing `name`. Saving an empty string resets
    /// to `None` and returns to automatic mode, matching cmux
    /// `customTitle: String?` semantics. [`Workspace::display_title`] computes
    /// the final name shown in the side panel.
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
    /// Final name shown in the side panel / window title. Returns user-provided
    /// [`Workspace::custom_title`] when present; otherwise the automatically
    /// determined [`Workspace::name`].
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

/// How `flowmux browser open` placed its new browser surface relative to
/// the requesting terminal pane. Mirrors cmux's `placement_strategy`
/// response field (`reuse_right_sibling` / `split_right` / `external`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlacementStrategy {
    /// A browser leaf already existed on the source pane's right side;
    /// the new URL was added to that pane as another surface tab.
    ReuseRightSibling,
    /// No suitable existing browser leaf — the source pane was split
    /// vertically and a fresh browser pane was put in the right sibling.
    SplitRight,
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

    /// Update a browser surface's stored URL. Called on webview navigation so
    /// the next launch can restore the same page. Applies only to matching
    /// browser surfaces. Returns false when the URL is unchanged so callers can
    /// skip dirty marking.
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
    /// terminal OSC, etc. Surfaces with title_locked = true were user-renamed and
    /// are skipped. Empty or identical titles return false.
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
                        // Drop OSC 0/2 when it is really a shell PS1 cwd echo.
                        // Otherwise every prompt's `user@host: /path` would
                        // overwrite folder labels and freeze tab/window titles
                        // in PS1 form. flowmux-app separately applies cwd folder
                        // names through terminal_tab_title_for_cwd via
                        // set_surface_cwd, while external program titles such
                        // as vi/codex/claude/tmux do not match the PS1 pattern
                        // and pass through.
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
                            // Update folder-based labels only when cwd actually
                            // changes. If cwd is the same, do not let polling
                            // overwrite external program titles from OSC 0/2 on
                            // each tick.
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

    /// Move the terminal or browser tab identified by `surface_id` within the
    /// same pane to `target_index`. `target_index` is the final index after the
    /// move and clamps to the last tab when too large. The active tab SurfaceId
    /// is preserved, so the same tab remains active after moving. Missing
    /// surfaces or same-position moves return `false` so callers can skip dirty
    /// marking and GTK widget moves.
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

    /// Return the `PaneId` of the Split node that directly owns `child`, whether
    /// a leaf or split. Recurses deeper when the adjacent split is not the owner.
    /// Returns `None` when `child` is the tree root or not in the tree.
    ///
    /// Used by the GTK side immediately after incremental split to find the
    /// newly created Split node through the new sibling.
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

    /// Update the ratio for the Split node identified by `target`. Clamp to
    /// [0.05, 0.95] to avoid exact 0/1 extremes and return `true` only for
    /// meaningful changes so callers can skip dirty marking.
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

    /// Return a clone of the [`PaneContent`] for the leaf identified by `target`.
    /// Returns `None` if no matching leaf exists in the tree or target is a split
    /// node. Used by the incremental split path to build GTK widgets for the new
    /// sibling pane's initial terminal or browser content.
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

    /// True if the leaf with `target` carries any browser surface (legacy
    /// `Browser` shape or a tab inside the new `Tabs` shape).
    pub fn pane_has_browser_surface(&self, target: PaneId) -> bool {
        match self {
            Pane::Leaf { id, content } if *id == target => content_has_browser(content),
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.pane_has_browser_surface(target)
                    || second.pane_has_browser_surface(target)
            }
        }
    }

    /// cmux-equivalent of `Workspace.preferredBrowserTargetPane`. Walks
    /// the split tree from the leaf with `from` upward, and for each
    /// vertical-split ancestor where `from` is in the `first` (left)
    /// subtree, searches the `second` (right) subtree for the nearest
    /// leaf that already hosts a browser surface. Returns `None` when
    /// no such right sibling exists, in which case callers should
    /// `split_leaf(from, Vertical, ...)` to create one.
    ///
    /// Geometry-based ordering (`y-center` then `x` distance, as in
    /// cmux) is approximated by tree distance: the closest ancestor
    /// wins, and within an ancestor's right subtree the depth-first
    /// leftmost browser leaf wins. For typical terminal-on-left /
    /// browser-on-right layouts the result matches cmux's behavior.
    pub fn find_right_sibling_browser_leaf(&self, from: PaneId) -> Option<PaneId> {
        match find_right_sibling_browser_leaf_inner(self, from) {
            RightSiblingSearch::Match(p) => Some(p),
            _ => None,
        }
    }
}

fn content_has_browser(content: &PaneContent) -> bool {
    match content {
        PaneContent::Browser { .. } => true,
        PaneContent::Tabs { surfaces, .. } => surfaces
            .iter()
            .any(|s| matches!(s.kind, SurfaceKind::Browser { .. })),
        PaneContent::Terminal { .. } => false,
    }
}

/// First leaf in DFS order that carries a browser surface, or `None`.
/// Used inside the right-sibling search to scan a sibling subtree.
fn first_browser_leaf_in(node: &Pane) -> Option<PaneId> {
    match node {
        Pane::Leaf { id, content } => {
            if content_has_browser(content) {
                Some(*id)
            } else {
                None
            }
        }
        Pane::Split { first, second, .. } => {
            first_browser_leaf_in(first).or_else(|| first_browser_leaf_in(second))
        }
    }
}

#[derive(Debug)]
enum RightSiblingSearch {
    /// `from` was located in this subtree, no browser-bearing right
    /// sibling has been found yet — caller should keep walking up.
    Found,
    /// A right-sibling browser leaf has been picked; bubble up.
    Match(PaneId),
    /// `from` is not in this subtree at all.
    NotInSubtree,
}

fn find_right_sibling_browser_leaf_inner(node: &Pane, from: PaneId) -> RightSiblingSearch {
    match node {
        Pane::Leaf { id, .. } => {
            if *id == from {
                RightSiblingSearch::Found
            } else {
                RightSiblingSearch::NotInSubtree
            }
        }
        Pane::Split {
            direction,
            first,
            second,
            ..
        } => match find_right_sibling_browser_leaf_inner(first, from) {
            RightSiblingSearch::Match(p) => RightSiblingSearch::Match(p),
            RightSiblingSearch::Found => {
                // `from` lives in our `first` subtree. If we're a
                // vertical split, our `second` subtree is the right
                // side of `from` — search it for a browser leaf.
                if matches!(direction, SplitDirection::Vertical) {
                    if let Some(p) = first_browser_leaf_in(second) {
                        return RightSiblingSearch::Match(p);
                    }
                }
                RightSiblingSearch::Found
            }
            RightSiblingSearch::NotInSubtree => {
                // Try the right subtree.
                find_right_sibling_browser_leaf_inner(second, from)
            }
        },
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
    use std::str::FromStr;

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
        // Folder name must fit within terminal_tab_title_for_cwd's truncation
        // budget so this test asserts migration semantics, not truncation.
        // The truncation contract itself is covered by
        // terminal_tab_title_for_cwd_uses_folder_and_truncates below.
        let cwd = PathBuf::from("/tmp/flowmux-core");
        let mut content = PaneContent::Terminal { pid: Some(123) };

        content.normalize_to_tabs(Some(cwd.clone()));

        let PaneContent::Tabs { active, surfaces } = content else {
            panic!("expected tabbed content")
        };
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].id, active);
        assert_eq!(surfaces[0].title, "flowmux-core");
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
        // Regression guard: titles set by external programs such as vi/claude
        // through OSC 0/2 must not be reverted to folder names by one-second cwd
        // polling. If cwd is unchanged, polling calls set_surface_cwd with the
        // same cwd, and surface.title must stay untouched.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("os", Some("/tmp/work".into())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // Enter an external program: set_surface_title_auto applies "Claude Code".
        assert!(pane.set_surface_title_auto(
            pane_id,
            surface_id,
            "Claude Code".into(),
        ));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Claude Code"));

        // Polling sees the same cwd again; it should be a no-op and keep the
        // title at "Claude Code".
        assert!(!pane.set_surface_cwd(pane_id, surface_id, "/tmp/work".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Claude Code"));

        // When the user actually changes cwd, then it returns to a folder label.
        // This matches the natural flow after leaving the external program.
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
        // Setting the same URL again returns false (no-op).
        assert!(!pane.set_surface_browser_url(pane_id, browser_id, "https://two.test".into()));

        // Terminal surfaces should not be affected.
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

        // Unlocked surfaces update automatically.
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "Page Title".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("Page Title"));

        // Identical title is a no-op.
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "Page Title".into()));

        // Empty / whitespace title is a no-op.
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "".into()));
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "   ".into()));

        // User rename -> title_locked = true.
        assert!(pane.rename_surface(pane_id, surface_id, "MyName".into()));
        assert!(!pane.set_surface_title_auto(pane_id, surface_id, "Other Page".into()));
        assert_eq!(pane.surface_title(pane_id, surface_id), Some("MyName"));
    }

    #[test]
    fn title_is_shell_cwd_echo_recognizes_bash_default_ps1() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // Default bash `\u@\h: \w` with an absolute path.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // `\u@\h:\w` without a space.
        assert!(title_is_shell_cwd_echo(
            "junsu@host:/tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // debian_chroot prefix variant.
        assert!(title_is_shell_cwd_echo(
            "(jammy)junsu@host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // Host-only prefix.
        assert!(title_is_shell_cwd_echo(
            "host: /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // Path only, for prompt themes that emit only the path.
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
        // bash `\w` abbreviates $HOME to `~`.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: ~/dev/os",
            cwd,
            Some(home),
        ));
        // Home itself, cwd == $HOME -> ~.
        assert!(title_is_shell_cwd_echo(
            "junsu@host: ~",
            Path::new("/home/junsu"),
            Some(home),
        ));
        // Without home information, tilde matching is unavailable but absolute
        // path matching still works.
        assert!(!title_is_shell_cwd_echo(
            "junsu@host: ~/dev/os",
            cwd,
            None,
        ));
    }

    #[test]
    fn title_is_shell_cwd_echo_passes_program_titles() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // Titles from external programs such as vi/codex/claude/tmux do not
        // match the PS1 pattern (`prefix:[ ]<cwd>`).
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
        // vim opening a file inside cwd should also pass through because its
        // prefix does not end with `:`.
        assert!(!title_is_shell_cwd_echo(
            "vim /tmp/flowmux-shell-echo-test",
            cwd,
            None,
        ));
        // Empty / whitespace values are not mistaken for echoes, even though
        // callers already check them, keeping the helper safe in isolation.
        assert!(!title_is_shell_cwd_echo("", cwd, None));
        assert!(!title_is_shell_cwd_echo("   ", cwd, None));
    }

    #[test]
    fn set_surface_title_auto_drops_shell_ps1_echo_on_terminal() {
        // Regression guard: shell PS1-shaped OSC 0/2 titles (`user@host: /path`)
        // emitted on every prompt must not overwrite cwd-based folder labels.
        // flowmux-app's cwd-notify flow applies folder names through
        // set_surface_cwd, so OSC 0/2 echoes should be ignored.
        let pane_id = PaneId::new();
        let cwd = PathBuf::from("/tmp/flowmux-shell-echo-test");
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("flowmux-shell-...", Some(cwd.clone())),
        };
        let surface_id = pane.active_surface_id(pane_id).unwrap();

        // PS1 echoes: all should be ignored, returning false and keeping title.
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

        // External program titles, such as vi, still apply normally.
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
        // A Split is not its own child, so root lookup returns None.
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

        // Other PaneIds return None.
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

        // A split PaneId is not a leaf, so return None.
        let split_id = match &tree {
            Pane::Split { id, .. } => *id,
            _ => unreachable!(),
        };
        assert!(tree.find_leaf_content(split_id).is_none());
    }

    #[test]
    fn split_leaf_preserves_target_pane_id_and_creates_fresh_sibling() {
        // Core assumption for incremental split: target keeps its PaneId after
        // splitting, and the sibling receives a new PaneId. If this breaks, GTK
        // PaneRegistry::pane_frame(target_pane) lookup misses and can rebuild
        // the wrong pane.
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

        // Target content remains original; new_pane has fresh content.
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
        // Splitting a pane already inside a split tree preserves the other
        // sibling pane's PaneId, so GTK can keep reusing that sibling's gtk::Frame.
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

        // r remains a leaf.
        assert!(matches!(
            tree.find_leaf_content(r),
            Some(PaneContent::Tabs { .. })
        ));
        // l survives as part of the new split and keeps its PaneId.
        assert!(matches!(
            tree.find_leaf_content(l),
            Some(PaneContent::Tabs { .. })
        ));
        // New sibling is registered.
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

        // Wrong (pane, surface) pairing returns None because the surface exists
        // only in the right pane.
        assert!(tree.find_surface(left_id, added_id).is_none());
        let found = tree.find_surface(right_id, added_id).unwrap();
        assert_eq!(found.id, added_id);
        assert_eq!(found.title, "RBrowser");
    }

    /// Pane-internal tab reorder scenarios. Covers preserving the active tab by
    /// surface_id, moving mixed terminal/browser tabs, index clamping, and no-op
    /// branches.
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
        // Restore active to a; c was added last and became active.
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
        // a moved, but active should still be a.
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

        // target_index=999 -> clamp to end -> b, a.
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

        // a is already at index 0, so moving to 0 is a no-op.
        assert!(!pane.reorder_surface_in_leaf(pane_id, a_id, 0));
        // Even out-of-range clamps to its current end position, so no-op.
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

    /// Create two terminals and one browser tab, move the middle browser tab to
    /// both ends, and verify order plus active preservation.
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
        // Make the middle browser tab active.
        assert!(pane.set_active_surface(pane_id, browser_id));

        // Middle -> first.
        assert!(pane.reorder_surface_in_leaf(pane_id, browser_id, 0));
        assert_active_order(&pane, pane_id, browser_id, &[browser_id, term_id, term2_id]);

        // First -> last.
        assert!(pane.reorder_surface_in_leaf(pane_id, browser_id, 2));
        assert_active_order(&pane, pane_id, browser_id, &[term_id, term2_id, browser_id]);
    }

    /// Reorder a tab in a deep leaf of a split tree. Other leaves must be unaffected.
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

        // Move R2, the last tab in the right pane, to first.
        assert!(tree.reorder_surface_in_leaf(right_id, r2_id, 0));
        assert_active_order(&tree, right_id, r2_id, &[r2_id, r0, r1_id]);

        // Left pane should stay unchanged.
        assert_active_order(&tree, left_id, l0, &[l0]);

        // Wrong (pane, surface) pairing returns false.
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

        // custom_title wins even when automatic updates change name.
        ws.name = "updated-auto".into();
        assert_eq!(ws.display_title(), "My Project");
    }

    #[test]
    fn display_title_treats_empty_custom_as_unset() {
        // Defensive: if any path stores an empty string, display returns to automatic mode.
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
        // Older state.json files lack custom_title.
        // #[serde(default)] should load it as None.
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

    // ----- right-sibling browser reuse (Phase 2) ----------------------

    fn term_leaf(id: PaneId) -> Pane {
        Pane::Leaf {
            id,
            content: PaneContent::tabbed_terminal("term", None),
        }
    }

    fn browser_leaf(id: PaneId) -> Pane {
        Pane::Leaf {
            id,
            content: PaneContent::tabbed_browser("Browser", "https://x".into()),
        }
    }

    fn vsplit(first: Pane, second: Pane) -> Pane {
        Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn hsplit(first: Pane, second: Pane) -> Pane {
        Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn pane_has_browser_surface_detects_tabs_and_legacy_shapes() {
        let leaf_id = PaneId::new();
        let pane = Pane::Leaf {
            id: leaf_id,
            content: PaneContent::tabbed_browser("Browser", "https://x".into()),
        };
        assert!(pane.pane_has_browser_surface(leaf_id));

        let term_id = PaneId::new();
        let term = term_leaf(term_id);
        assert!(!term.pane_has_browser_surface(term_id));

        let legacy_id = PaneId::new();
        let legacy = Pane::Leaf {
            id: legacy_id,
            content: PaneContent::Browser {
                url: "https://x".into(),
            },
        };
        assert!(legacy.pane_has_browser_surface(legacy_id));
    }

    #[test]
    fn right_sibling_returns_none_when_no_split_exists() {
        let term_id = PaneId::new();
        let pane = term_leaf(term_id);
        assert!(pane.find_right_sibling_browser_leaf(term_id).is_none());
    }

    #[test]
    fn right_sibling_finds_browser_directly_to_the_right() {
        let term_id = PaneId::new();
        let browser_id = PaneId::new();
        let pane = vsplit(term_leaf(term_id), browser_leaf(browser_id));
        assert_eq!(
            pane.find_right_sibling_browser_leaf(term_id),
            Some(browser_id),
            "vertical split with browser on right should reuse"
        );
    }

    #[test]
    fn right_sibling_skips_horizontal_split_below() {
        let term_id = PaneId::new();
        let browser_id = PaneId::new();
        let pane = hsplit(term_leaf(term_id), browser_leaf(browser_id));
        assert!(
            pane.find_right_sibling_browser_leaf(term_id).is_none(),
            "horizontal split is up/down, not right — must not reuse"
        );
    }

    #[test]
    fn right_sibling_does_not_reuse_when_caller_is_on_the_right() {
        // Browser is on the LEFT of the caller — reuse must not pick it.
        let term_id = PaneId::new();
        let browser_id = PaneId::new();
        let pane = vsplit(browser_leaf(browser_id), term_leaf(term_id));
        assert!(
            pane.find_right_sibling_browser_leaf(term_id).is_none(),
            "right-sibling search must not pick a left sibling"
        );
    }

    #[test]
    fn right_sibling_picks_nearest_ancestor_first() {
        // Tree:
        //
        //         vsplit
        //        /      \
        //   vsplit    browser_far  (far right)
        //   /     \
        // term  browser_near       (immediate right)
        //
        // We must pick browser_near, not browser_far — closest ancestor wins.
        let term_id = PaneId::new();
        let near_id = PaneId::new();
        let far_id = PaneId::new();
        let pane = vsplit(
            vsplit(term_leaf(term_id), browser_leaf(near_id)),
            browser_leaf(far_id),
        );
        assert_eq!(
            pane.find_right_sibling_browser_leaf(term_id),
            Some(near_id)
        );
    }

    #[test]
    fn right_sibling_falls_through_to_outer_ancestor_when_immediate_right_is_terminal() {
        // Tree:
        //
        //          vsplit
        //         /      \
        //     vsplit    browser_far
        //     /     \
        //  term   term_neighbor
        //
        // Immediate right (term_neighbor) is not a browser → walk up to
        // outer vsplit → second is browser_far → that's the result.
        let term_id = PaneId::new();
        let neighbor_id = PaneId::new();
        let far_id = PaneId::new();
        let pane = vsplit(
            vsplit(term_leaf(term_id), term_leaf(neighbor_id)),
            browser_leaf(far_id),
        );
        assert_eq!(
            pane.find_right_sibling_browser_leaf(term_id),
            Some(far_id)
        );
    }

    #[test]
    fn right_sibling_returns_first_browser_in_complex_subtree() {
        // Right subtree is itself split — pick the leftmost (DFS-first)
        // browser leaf inside it, which is the visually "closer" one.
        let term_id = PaneId::new();
        let browser_a = PaneId::new();
        let browser_b = PaneId::new();
        let pane = vsplit(
            term_leaf(term_id),
            hsplit(browser_leaf(browser_a), browser_leaf(browser_b)),
        );
        assert_eq!(
            pane.find_right_sibling_browser_leaf(term_id),
            Some(browser_a)
        );
    }

    #[test]
    fn right_sibling_returns_none_when_all_right_subtrees_are_terminals() {
        let term_id = PaneId::new();
        let other = PaneId::new();
        let pane = vsplit(term_leaf(term_id), term_leaf(other));
        assert!(pane.find_right_sibling_browser_leaf(term_id).is_none());
    }

    #[test]
    fn id_from_str_accepts_bare_uuid_and_cmux_prefixes() {
        let pane = PaneId::new();
        let s = pane.to_string();
        // bare UUID
        assert_eq!(PaneId::from_str(&s).unwrap(), pane);
        // surface: prefix (cmux-compatible)
        assert_eq!(PaneId::from_str(&format!("surface:{s}")).unwrap(), pane);
        // pane: prefix
        assert_eq!(PaneId::from_str(&format!("pane:{s}")).unwrap(), pane);
        // arbitrary label is ignored before ':' — keeps the rule simple
        // and forward-compatible with future label types.
        assert_eq!(PaneId::from_str(&format!("foo:{s}")).unwrap(), pane);

        let ws = WorkspaceId::new();
        let s = ws.to_string();
        assert_eq!(WorkspaceId::from_str(&format!("workspace:{s}")).unwrap(), ws);
    }

    #[test]
    fn id_from_str_rejects_non_uuid_after_prefix() {
        assert!(PaneId::from_str("surface:not-a-uuid").is_err());
        assert!(PaneId::from_str("not-a-uuid").is_err());
    }

    #[test]
    fn placement_strategy_serializes_as_snake_case() {
        let json = serde_json::to_string(&PlacementStrategy::ReuseRightSibling).unwrap();
        assert_eq!(json, r#""reuse_right_sibling""#);
        let json = serde_json::to_string(&PlacementStrategy::SplitRight).unwrap();
        assert_eq!(json, r#""split_right""#);
        // Round-trip both variants.
        let back: PlacementStrategy = serde_json::from_str(r#""reuse_right_sibling""#).unwrap();
        assert_eq!(back, PlacementStrategy::ReuseRightSibling);
    }
}
