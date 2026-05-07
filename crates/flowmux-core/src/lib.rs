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
use std::path::PathBuf;
use uuid::Uuid;

pub mod id {
    use super::*;

    macro_rules! id_newtype {
        ($name:ident) => {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
            #[serde(transparent)]
            pub struct $name(pub Uuid);

            impl $name {
                pub fn new() -> Self { Self(Uuid::new_v4()) }
            }

            impl Default for $name {
                fn default() -> Self { Self::new() }
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
pub enum PrState { Open, Closed, Merged, Draft }

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
    Terminal { shell: Option<String>, cwd: Option<PathBuf> },
    Browser { initial_url: Option<String> },
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
pub enum SplitDirection { Horizontal, Vertical }

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
                            content: PaneContent::Terminal { pid: None },
                        }),
                        second: Box::new(Pane::Leaf {
                            id: PaneId::new(),
                            content: new_content.clone(),
                        }),
                    },
                );
                if let (Pane::Split { first, second, .. }, Pane::Leaf { content: orig_content, .. }) =
                    (self, &original)
                {
                    *first = Box::new(Pane::Leaf { id: target, content: orig_content.clone() });
                    let new_id = if let Pane::Leaf { id, .. } = &**second { *id } else { unreachable!() };
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
            Pane::Split { id, direction, ratio, first, second } => {
                match first.remove_leaf(target) {
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
                }
            }
        }
    }
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
    Terminal { pid: Option<u32> },
    Browser { url: String },
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
        let mut p = Pane::Leaf { id: leaf_id, content: PaneContent::Terminal { pid: None } };
        let new_id = p
            .split_leaf(
                leaf_id,
                SplitDirection::Vertical,
                0.5,
                PaneContent::Terminal { pid: None },
            )
            .unwrap();
        match &p {
            Pane::Split { direction, first, second, .. } => {
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
            first: Box::new(Pane::Leaf { id: l1, content: PaneContent::Terminal { pid: None } }),
            second: Box::new(Pane::Leaf { id: l2, content: PaneContent::Terminal { pid: None } }),
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
            first: Box::new(Pane::Leaf { id: l1, content: PaneContent::Terminal { pid: None } }),
            second: Box::new(Pane::Leaf { id: l2, content: PaneContent::Terminal { pid: None } }),
        };
        match p.remove_leaf(l1) {
            RemoveOutcome::Replaced(Pane::Leaf { id, .. }) => assert_eq!(id, l2),
            _ => panic!("expected leaf l2 to remain after l1 removal"),
        }
    }

    #[test]
    fn remove_leaf_returns_entirely_removed_on_root_match() {
        let id = PaneId::new();
        let p = Pane::Leaf { id, content: PaneContent::Terminal { pid: None } };
        assert!(matches!(p.remove_leaf(id), RemoveOutcome::EntirelyRemoved));
    }

    #[test]
    fn remove_leaf_returns_not_found_when_id_missing() {
        let id = PaneId::new();
        let other = PaneId::new();
        let p = Pane::Leaf { id, content: PaneContent::Terminal { pid: None } };
        assert!(matches!(p.remove_leaf(other), RemoveOutcome::NotFound(_)));
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
                kind: SurfaceKind::Terminal { shell: None, cwd: None },
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
