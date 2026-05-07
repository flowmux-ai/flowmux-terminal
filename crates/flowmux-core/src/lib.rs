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
    pub name: String,
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
                            let cwd_changed = cwd.as_ref() != Some(&new_cwd);
                            let next_title = (!surface.title_locked)
                                .then(|| terminal_tab_title_for_cwd(Some(&new_cwd)));
                            let title_changed = next_title
                                .as_ref()
                                .is_some_and(|title| surface.title != *title);
                            if cwd_changed {
                                *cwd = Some(new_cwd);
                            }
                            if let Some(title) = next_title {
                                if title_changed {
                                    surface.title = title;
                                }
                            }
                            return cwd_changed || title_changed;
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
    fn workspace_roundtrips_through_json() {
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "demo".into(),
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
    }
}
