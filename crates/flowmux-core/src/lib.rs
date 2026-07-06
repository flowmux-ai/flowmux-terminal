// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain types shared across flowmux crates.
//!
//! Types here are deliberately backend-agnostic: they describe the shape
//! of a workspace, a surface (terminal/browser pane), a notification, and
//! the IPC verbs — not how any of them are rendered or executed.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const TERMINAL_TAB_TITLE_MAX_CHARS: usize = 17;
const FALLBACK_TERMINAL_TAB_TITLE: &str = "Terminal";
pub const AGENT_BAR_ITEM_MIN_WIDTH_PX: u16 = 84;
pub const AGENT_BAR_ITEM_MAX_WIDTH_PX: u16 = 168;

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

fn normalize_unlocked_terminal_title(surface: &mut PaneSurface) -> bool {
    if surface.title_locked {
        return false;
    }
    let SurfaceKind::Terminal { cwd, .. } = &surface.kind else {
        return false;
    };
    // At state-load time the running process that emitted the
    // last OSC 0/2 title is gone, so any unlocked title that
    // doesn't match the cwd-derived form is stale (e.g. "Claude
    // Code", "codex", "vim foo"). Reset it to the cwd-based title
    // — and never auto-promote to locked, because the title was
    // never the user's intent in the first place.
    let next_title = match cwd.as_deref() {
        Some(cwd) => terminal_tab_title_for_cwd(Some(cwd)),
        None => FALLBACK_TERMINAL_TAB_TITLE.to_string(),
    };
    if surface.title == next_title {
        return false;
    }
    surface.title = next_title;
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

    pub fn agent_status_rollup(&self) -> Option<AgentStatus> {
        rollup_agent_statuses(
            self.surfaces
                .iter()
                .filter_map(|surface| surface.root_pane.agent_status_rollup()),
        )
    }

    pub fn collect_agent_blocks(&self, mru: &[PaneId]) -> Vec<WorkspaceAgentBlock> {
        let mut blocks = Vec::new();
        for surface in &self.surfaces {
            surface.root_pane.collect_agent_blocks(&mut blocks);
        }
        blocks.sort_by(|a, b| {
            let a_mru = mru
                .iter()
                .position(|pane| *pane == a.pane)
                .unwrap_or(usize::MAX);
            let b_mru = mru
                .iter()
                .position(|pane| *pane == b.pane)
                .unwrap_or(usize::MAX);
            b.status
                .rollup_rank()
                .cmp(&a.status.rollup_rank())
                .then_with(|| a_mru.cmp(&b_mru))
                .then_with(|| a.agent_name.cmp(&b.agent_name))
                .then_with(|| a.surface.to_string().cmp(&b.surface.to_string()))
        });
        blocks
    }

    pub fn collect_agent_bar_items(&self) -> Vec<AgentBarItem> {
        let mut items = Vec::new();
        let color = self.color.as_deref();
        for surface in &self.surfaces {
            surface
                .root_pane
                .collect_agent_bar_items(self.id, color, &mut items);
        }
        items
    }
}

pub fn rollup_agent_statuses(
    statuses: impl IntoIterator<Item = AgentStatus>,
) -> Option<AgentStatus> {
    statuses
        .into_iter()
        .max_by_key(|status| status.rollup_rank())
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
    /// Live AI-agent activity, set from agent lifecycle hooks. Runtime
    /// state only — never persisted, so resumed workspaces start with no
    /// agent presence until the next hook fires.
    #[serde(skip)]
    pub agent: Option<AgentPresence>,
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
        // Pass new_content by reference until we reach the matching leaf,
        // then take ownership for the single placement. Saves a chain of
        // PaneContent::clone calls (each carrying owned tab Vec<PaneSurface>)
        // on the way down.
        let mut new_content = Some(new_content);
        split_leaf_descend(self, target, direction, ratio, &mut new_content)
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
        // PaneSurface contains owned strings (title, kind data); cloning
        // it on every Split node along the path is the expensive part —
        // moving once via Option::take avoids that.
        let mut surface = Some(surface);
        add_surface_to_leaf_descend(self, target, &mut surface)
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

    /// Pick a cwd to seed a newly spawned terminal next to `target`.
    ///
    /// Used by split and add-tab so the new terminal opens where the user is
    /// looking. Resolution:
    ///   1. If the active surface is a terminal with a tracked cwd, use it.
    ///   2. If the active surface is a browser, walk earlier tabs in this
    ///      pane and use the most recent terminal's cwd. Browsers do not
    ///      have a cwd of their own, so this preserves the user's prior
    ///      location instead of dropping to the workspace root.
    pub fn terminal_surface_cwd(&self, target: PaneId) -> Option<PathBuf> {
        match self {
            Pane::Leaf { id, content } if *id == target => content.cwd_for_new_terminal(),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .terminal_surface_cwd(target)
                .or_else(|| second.terminal_surface_cwd(target)),
        }
    }

    pub fn rename_surface(&mut self, target: PaneId, surface_id: SurfaceId, title: String) -> bool {
        // Carry the title as Option<String> through the descent so it is
        // moved into the matching leaf rather than cloned at every Split.
        // For deep pane trees this turns an O(depth) chain of String clones
        // into a single move.
        let mut title = Some(title);
        rename_surface_descend(self, target, surface_id, &mut title)
    }

    /// Set (or clear, with `None`) the live AI-agent presence on the tab
    /// surface identified by `surface_id`, wherever it sits in this pane
    /// tree. Returns true when a matching surface was found. Runtime-only
    /// state — callers must not schedule persistence for this change.
    pub fn set_surface_agent(
        &mut self,
        surface_id: SurfaceId,
        agent: Option<AgentPresence>,
    ) -> bool {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => {
                if let Some(surface) = surfaces.iter_mut().find(|s| s.id == surface_id) {
                    surface.agent = agent;
                    true
                } else {
                    false
                }
            }
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.set_surface_agent(surface_id, agent.clone())
                    || second.set_surface_agent(surface_id, agent)
            }
        }
    }

    /// Merge a live agent report into the matching tab surface. `workspace_visible`
    /// tells the done/seen logic whether this workspace is currently foregrounded;
    /// the pane-local active tab is checked here.
    pub fn report_surface_agent(
        &mut self,
        surface_id: SurfaceId,
        report: AgentStatusReport,
        workspace_visible: bool,
    ) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { active, surfaces },
                ..
            } => {
                let visible = workspace_visible && *active == surface_id;
                let surface = surfaces.iter_mut().find(|s| s.id == surface_id)?;
                let mut report = report;
                normalize_agent_report_name_for_surface_title(&mut report, &surface.title);
                match surface.agent.as_mut() {
                    Some(agent) => Some(agent.apply_report(report, visible)),
                    None => {
                        surface.agent = AgentPresence::from_report(report, visible);
                        Some(surface.agent.is_some())
                    }
                }
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .report_surface_agent(surface_id, report.clone(), workspace_visible)
                .or_else(|| second.report_surface_agent(surface_id, report, workspace_visible)),
        }
    }

    pub fn report_surface_agent_signal(
        &mut self,
        surface_id: SurfaceId,
        status: AgentStatus,
        source: &'static str,
        agent_name: Option<&str>,
        workspace_visible: bool,
    ) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { active, surfaces },
                ..
            } => {
                let visible = workspace_visible && *active == surface_id;
                let surface = surfaces.iter_mut().find(|s| s.id == surface_id)?;
                let title_agent_name = detect_agent_name_from_surface_title(&surface.title);
                let incoming_name = title_agent_name.or(agent_name).map(str::to_string);
                let existing_agent = surface.agent.as_ref();
                let incoming_is_different_agent = incoming_name
                    .as_deref()
                    .is_some_and(|name| existing_agent.is_some_and(|agent| agent.name != name));
                if source == "flowmux:screen"
                    && !incoming_is_different_agent
                    && surface.agent.as_ref().is_some_and(|agent| {
                        agent.name == "claude" && agent.source.as_deref() == Some("flowmux:hook")
                    })
                {
                    return Some(false);
                }
                if source == "flowmux:screen"
                    && !incoming_is_different_agent
                    && status == AgentStatus::Idle
                    && surface.agent.as_ref().is_some_and(|agent| {
                        matches!(agent.status, AgentStatus::Working | AgentStatus::Blocked)
                            && agent.source.as_deref() == Some("flowmux:hook")
                    })
                {
                    return Some(false);
                }
                if source == "flowmux:screen"
                    && incoming_name.is_none()
                    && surface
                        .agent
                        .as_ref()
                        .is_some_and(|agent| agent.source.as_deref() == Some("flowmux:screen"))
                {
                    surface.agent = None;
                    return Some(true);
                }

                let name = incoming_name
                    .or_else(|| surface.agent.as_ref().map(|agent| agent.name.clone()))?;
                let report = AgentStatusReport {
                    name,
                    status: Some(status),
                    activity: Some(status.to_activity()),
                    pid: None,
                    source: Some(source.into()),
                    seq: None,
                    message: None,
                    custom_status: None,
                    session_id: None,
                };
                match surface.agent.as_mut() {
                    Some(agent) => Some(agent.apply_report(report, visible)),
                    None => {
                        surface.agent = AgentPresence::from_report(report, visible);
                        Some(surface.agent.is_some())
                    }
                }
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .report_surface_agent_signal(
                    surface_id,
                    status,
                    source,
                    agent_name,
                    workspace_visible,
                )
                .or_else(|| {
                    second.report_surface_agent_signal(
                        surface_id,
                        status,
                        source,
                        agent_name,
                        workspace_visible,
                    )
                }),
        }
    }

    pub fn clear_surface_agent_from_source(
        &mut self,
        surface_id: SurfaceId,
        source: &str,
    ) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => {
                let surface = surfaces.iter_mut().find(|s| s.id == surface_id)?;
                let should_clear = surface
                    .agent
                    .as_ref()
                    .is_some_and(|agent| agent.source.as_deref() == Some(source));
                if should_clear {
                    surface.agent = None;
                    Some(true)
                } else {
                    Some(false)
                }
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .clear_surface_agent_from_source(surface_id, source)
                .or_else(|| second.clear_surface_agent_from_source(surface_id, source)),
        }
    }

    /// Reconcile a surface's agent presence against process-tree truth.
    /// `detected` is the canonical agent name found running in the pane's
    /// process subtree, or `None` when no agent process is present. This is the
    /// authoritative *existence* signal: it creates an idle, process-owned
    /// presence the moment an agent process appears (independent of TUI text,
    /// OSC title, or hooks) and drops it when the process exits back to a plain
    /// shell. Hook/screen-owned presences are left to their own lifecycles.
    /// Returns `Some(true)` when the presence changed, `Some(false)` when
    /// unchanged, `None` when the surface is not in this pane tree.
    pub fn reconcile_process_agent(
        &mut self,
        surface_id: SurfaceId,
        detected: Option<&str>,
    ) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => {
                let surface = surfaces.iter_mut().find(|s| s.id == surface_id)?;
                Some(reconcile_surface_process_agent(&mut surface.agent, detected))
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .reconcile_process_agent(surface_id, detected)
                .or_else(|| second.reconcile_process_agent(surface_id, detected)),
        }
    }

    /// Screen scan saw the surface but detected no active status. Clear a
    /// screen-owned presence (screen is the sole owner of what it created), but
    /// for a process-owned presence only settle it back to Idle — the agent is
    /// still running, it simply finished its turn. Hook-owned presences are
    /// authoritative and left untouched.
    pub fn settle_screen_idle(&mut self, surface_id: SurfaceId) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => {
                let surface = surfaces.iter_mut().find(|s| s.id == surface_id)?;
                let Some(agent) = surface.agent.as_mut() else {
                    return Some(false);
                };
                match agent.source.as_deref() {
                    Some("flowmux:screen") => {
                        surface.agent = None;
                        Some(true)
                    }
                    Some(AGENT_SOURCE_PROC) if agent.status != AgentStatus::Idle => {
                        agent.status = AgentStatus::Idle;
                        agent.activity = AgentActivity::Idle;
                        agent.message = None;
                        agent.custom_status = None;
                        Some(true)
                    }
                    _ => Some(false),
                }
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .settle_screen_idle(surface_id)
                .or_else(|| second.settle_screen_idle(surface_id)),
        }
    }

    /// Mark the matching surface as seen. Used when a user activates a tab that
    /// was showing the derived `done` status.
    pub fn mark_surface_agent_seen(&mut self, surface_id: SurfaceId) -> bool {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces
                .iter_mut()
                .find(|surface| surface.id == surface_id)
                .and_then(|surface| surface.agent.as_mut())
                .map(AgentPresence::mark_seen)
                .unwrap_or(false),
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.mark_surface_agent_seen(surface_id)
                    || second.mark_surface_agent_seen(surface_id)
            }
        }
    }

    /// Mark every active tab in this pane tree as seen. Used when a workspace is
    /// brought to the foreground.
    pub fn mark_active_agents_seen(&mut self) -> bool {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { active, surfaces },
                ..
            } => surfaces
                .iter_mut()
                .find(|surface| surface.id == *active)
                .and_then(|surface| surface.agent.as_mut())
                .map(AgentPresence::mark_seen)
                .unwrap_or(false),
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.mark_active_agents_seen() | second.mark_active_agents_seen()
            }
        }
    }

    pub fn agent_status_rollup(&self) -> Option<AgentStatus> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces
                .iter()
                .filter_map(|surface| surface.agent.as_ref().map(AgentPresence::public_status))
                .max_by_key(|status| status.rollup_rank()),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => rollup_agent_statuses(
                first
                    .agent_status_rollup()
                    .into_iter()
                    .chain(second.agent_status_rollup()),
            ),
        }
    }

    /// Append `(surface_id, presence)` for every tab surface in this tree
    /// that currently carries an agent presence. Used by the daemon's
    /// PID liveness sweep.
    pub fn collect_agent_presences(&self, out: &mut Vec<(SurfaceId, AgentPresence)>) {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => {
                for surface in surfaces {
                    if let Some(agent) = &surface.agent {
                        out.push((surface.id, agent.clone()));
                    }
                }
            }
            Pane::Leaf { .. } => {}
            Pane::Split { first, second, .. } => {
                first.collect_agent_presences(out);
                second.collect_agent_presences(out);
            }
        }
    }

    pub fn collect_agent_blocks(&self, out: &mut Vec<WorkspaceAgentBlock>) {
        match self {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { surfaces, .. },
            } => {
                for surface in surfaces {
                    let Some(agent) = &surface.agent else {
                        continue;
                    };
                    let status = agent.public_status();
                    if status == AgentStatus::Unknown {
                        continue;
                    }
                    let cwd = match &surface.kind {
                        SurfaceKind::Terminal { cwd: Some(cwd), .. } => {
                            Some(cwd.display().to_string())
                        }
                        SurfaceKind::Terminal { cwd: None, .. } | SurfaceKind::Browser { .. } => {
                            None
                        }
                    };
                    out.push(WorkspaceAgentBlock {
                        pane: *id,
                        surface: surface.id,
                        agent_name: agent.name.clone(),
                        status,
                        seen: agent.seen,
                        status_text: agent.status_text().map(str::to_string),
                        cwd,
                    });
                }
            }
            Pane::Leaf { .. } => {}
            Pane::Split { first, second, .. } => {
                first.collect_agent_blocks(out);
                second.collect_agent_blocks(out);
            }
        }
    }

    pub fn agent_presence_for_surface(&self, surface_id: SurfaceId) -> Option<AgentPresence> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces
                .iter()
                .find(|surface| surface.id == surface_id)
                .and_then(|surface| surface.agent.clone()),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .agent_presence_for_surface(surface_id)
                .or_else(|| second.agent_presence_for_surface(surface_id)),
        }
    }

    pub fn collect_agent_bar_items(
        &self,
        workspace: WorkspaceId,
        workspace_color: Option<&str>,
        out: &mut Vec<AgentBarItem>,
    ) {
        match self {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { surfaces, .. },
            } => {
                for surface in surfaces {
                    let Some(agent) = &surface.agent else {
                        continue;
                    };
                    if let Some(item) = AgentBarItem::from_presence(
                        workspace,
                        *id,
                        surface.id,
                        agent,
                        workspace_color,
                    ) {
                        out.push(item);
                    }
                }
            }
            Pane::Leaf { .. } => {}
            Pane::Split { first, second, .. } => {
                first.collect_agent_bar_items(workspace, workspace_color, out);
                second.collect_agent_bar_items(workspace, workspace_color, out);
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
        let mut new_url = Some(new_url);
        set_surface_browser_url_descend(self, target, surface_id, &mut new_url)
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
        let mut new_title = Some(new_title);
        // `home_cache` defers the `$HOME` env lookup until the leaf actually
        // needs it (Terminal surface with cwd set). state_store walks every
        // workspace looking for `target`, so calls that miss the leaf must
        // not pay for an env access.
        let mut home_cache: Option<Option<PathBuf>> = None;
        set_surface_title_auto_descend(self, target, surface_id, &mut new_title, &mut home_cache)
    }

    pub fn set_surface_cwd(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        new_cwd: PathBuf,
    ) -> bool {
        let mut new_cwd = Some(new_cwd);
        set_surface_cwd_descend(self, target, surface_id, &mut new_cwd)
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

    /// Remove `surface_id` from leaf `target` and return the removed
    /// [`PaneSurface`] together with a flag that is `true` when the leaf has no
    /// tabs left afterwards. Mirrors [`Self::close_surface_in_leaf`]'s
    /// active-id fix-up but hands the surface back instead of dropping it — used
    /// by the tab-move feature to relocate a tab while preserving its state.
    /// Returns `None` if the pane or surface is not found.
    pub fn take_surface_from_leaf(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
    ) -> Option<(PaneSurface, bool)> {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { active, surfaces } => {
                    let idx = surfaces.iter().position(|s| s.id == surface_id)?;
                    let taken = surfaces.remove(idx);
                    let empty = surfaces.is_empty();
                    if !empty
                        && (*active == surface_id || !surfaces.iter().any(|s| s.id == *active))
                    {
                        *active = surfaces[idx.saturating_sub(1).min(surfaces.len() - 1)].id;
                    }
                    Some((taken, empty))
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
            },
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .take_surface_from_leaf(target, surface_id)
                .or_else(|| second.take_surface_from_leaf(target, surface_id)),
        }
    }

    /// Insert `surface` into leaf `target` at `index` (clamped to the tab
    /// count) and make it the active tab. Returns the inserted [`SurfaceId`],
    /// or `None` if no matching tabs leaf exists. Companion to
    /// [`Self::take_surface_from_leaf`] for relocating a tab.
    pub fn insert_surface_into_leaf(
        &mut self,
        target: PaneId,
        surface: PaneSurface,
        index: usize,
    ) -> Option<SurfaceId> {
        self.insert_surface_into_leaf_inner(target, surface, index)
            .ok()
    }

    // The `Err` payload hands ownership of the not-yet-inserted surface back up
    // the recursion so it can be tried on the sibling subtree; boxing it keeps
    // the `Result` small.
    fn insert_surface_into_leaf_inner(
        &mut self,
        target: PaneId,
        surface: PaneSurface,
        index: usize,
    ) -> Result<SurfaceId, Box<PaneSurface>> {
        match self {
            Pane::Leaf { id, content } if *id == target => match content {
                PaneContent::Tabs { active, surfaces } => {
                    let sid = surface.id;
                    let at = index.min(surfaces.len());
                    surfaces.insert(at, surface);
                    *active = sid;
                    Ok(sid)
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => {
                    Err(Box::new(surface))
                }
            },
            Pane::Leaf { .. } => Err(Box::new(surface)),
            Pane::Split { first, second, .. } => {
                match first.insert_surface_into_leaf_inner(target, surface, index) {
                    Ok(id) => Ok(id),
                    Err(surface) => second.insert_surface_into_leaf_inner(target, *surface, index),
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
                first.pane_has_browser_surface(target) || second.pane_has_browser_surface(target)
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

/// Helpers used by `Pane::set_surface_*`-family methods. They take the
/// owned payload as `&mut Option<T>` so the value moves into the matching
/// leaf via `Option::take` instead of being cloned at every Split node
/// during the descent. The public methods are thin wrappers that wrap
/// the owned value into `Some` and call here.
fn rename_surface_descend(
    node: &mut Pane,
    target: PaneId,
    surface_id: SurfaceId,
    title: &mut Option<String>,
) -> bool {
    match node {
        Pane::Leaf { id, content } if *id == target => match content {
            PaneContent::Tabs { surfaces, .. } => {
                let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == surface_id)
                else {
                    return false;
                };
                surface.title = title
                    .take()
                    .expect("title is Some until consumed at the matching leaf");
                surface.title_locked = true;
                true
            }
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
        },
        Pane::Leaf { .. } => false,
        Pane::Split { first, second, .. } => {
            rename_surface_descend(first, target, surface_id, title)
                || rename_surface_descend(second, target, surface_id, title)
        }
    }
}

fn set_surface_browser_url_descend(
    node: &mut Pane,
    target: PaneId,
    surface_id: SurfaceId,
    new_url: &mut Option<String>,
) -> bool {
    match node {
        Pane::Leaf { id, content } if *id == target => match content {
            PaneContent::Tabs { surfaces, .. } => {
                let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == surface_id)
                else {
                    return false;
                };
                let SurfaceKind::Browser { initial_url } = &mut surface.kind else {
                    return false;
                };
                let candidate = new_url
                    .as_ref()
                    .expect("new_url is Some until taken at match");
                if initial_url.as_deref() == Some(candidate.as_str()) {
                    return false;
                }
                *initial_url = Some(new_url.take().unwrap());
                true
            }
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
        },
        Pane::Leaf { .. } => false,
        Pane::Split { first, second, .. } => {
            set_surface_browser_url_descend(first, target, surface_id, new_url)
                || set_surface_browser_url_descend(second, target, surface_id, new_url)
        }
    }
}

fn set_surface_title_auto_descend(
    node: &mut Pane,
    target: PaneId,
    surface_id: SurfaceId,
    new_title: &mut Option<String>,
    home_cache: &mut Option<Option<PathBuf>>,
) -> bool {
    match node {
        Pane::Leaf { id, content } if *id == target => match content {
            PaneContent::Tabs { surfaces, .. } => {
                let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == surface_id)
                else {
                    return false;
                };
                let candidate = new_title
                    .as_ref()
                    .expect("new_title is Some until taken at match");
                if surface.title_locked || surface.title == *candidate {
                    return false;
                }
                if let SurfaceKind::Terminal { cwd: Some(cwd), .. } = &surface.kind {
                    let home = home_cache
                        .get_or_insert_with(|| std::env::var_os("HOME").map(PathBuf::from))
                        .as_deref();
                    if title_is_shell_cwd_echo(candidate, cwd, home) {
                        return false;
                    }
                }
                surface.title = new_title.take().unwrap();
                true
            }
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
        },
        Pane::Leaf { .. } => false,
        Pane::Split { first, second, .. } => {
            set_surface_title_auto_descend(first, target, surface_id, new_title, home_cache)
                || set_surface_title_auto_descend(second, target, surface_id, new_title, home_cache)
        }
    }
}

fn set_surface_cwd_descend(
    node: &mut Pane,
    target: PaneId,
    surface_id: SurfaceId,
    new_cwd: &mut Option<PathBuf>,
) -> bool {
    match node {
        Pane::Leaf { id, content } if *id == target => match content {
            PaneContent::Tabs { surfaces, .. } => {
                let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == surface_id)
                else {
                    return false;
                };
                let SurfaceKind::Terminal { cwd, .. } = &mut surface.kind else {
                    return false;
                };
                let candidate = new_cwd
                    .as_ref()
                    .expect("new_cwd is Some until taken at match");
                if cwd.as_ref() == Some(candidate) {
                    return false;
                }
                *cwd = Some(new_cwd.take().unwrap());
                if !surface.title_locked {
                    let next_title = terminal_tab_title_for_cwd(cwd.as_deref());
                    if surface.title != next_title {
                        surface.title = next_title;
                    }
                }
                true
            }
            PaneContent::Terminal { .. } | PaneContent::Browser { .. } => false,
        },
        Pane::Leaf { .. } => false,
        Pane::Split { first, second, .. } => {
            set_surface_cwd_descend(first, target, surface_id, new_cwd)
                || set_surface_cwd_descend(second, target, surface_id, new_cwd)
        }
    }
}

fn add_surface_to_leaf_descend(
    node: &mut Pane,
    target: PaneId,
    surface: &mut Option<PaneSurface>,
) -> Option<SurfaceId> {
    match node {
        Pane::Leaf { id, content } if *id == target => {
            content.normalize_to_tabs(None);
            match content {
                PaneContent::Tabs { active, surfaces } => {
                    let s = surface
                        .take()
                        .expect("surface is Some until taken at match");
                    let id = s.id;
                    *active = id;
                    surfaces.push(s);
                    Some(id)
                }
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => None,
            }
        }
        Pane::Leaf { .. } => None,
        Pane::Split { first, second, .. } => add_surface_to_leaf_descend(first, target, surface)
            .or_else(|| add_surface_to_leaf_descend(second, target, surface)),
    }
}

fn split_leaf_descend(
    node: &mut Pane,
    target: PaneId,
    direction: SplitDirection,
    ratio: f32,
    new_content: &mut Option<PaneContent>,
) -> Option<PaneId> {
    match node {
        Pane::Leaf { id, .. } if *id == target => {
            let original = std::mem::replace(
                node,
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
                        content: new_content
                            .take()
                            .expect("new_content is Some until taken at match"),
                    }),
                },
            );
            if let (
                Pane::Split { first, second, .. },
                Pane::Leaf {
                    content: orig_content,
                    ..
                },
            ) = (node, original)
            {
                **first = Pane::Leaf {
                    id: target,
                    content: orig_content,
                };
                let new_id = match &**second {
                    Pane::Leaf { id, .. } => *id,
                    Pane::Split { .. } => unreachable!(),
                };
                return Some(new_id);
            }
            None
        }
        Pane::Leaf { .. } => None,
        Pane::Split { first, second, .. } => {
            split_leaf_descend(first, target, direction, ratio, new_content)
                .or_else(|| split_leaf_descend(second, target, direction, ratio, new_content))
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
            agent: None,
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
            agent: None,
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

    /// cwd to seed a new terminal spawned inside this pane. Active terminal's
    /// cwd wins. If the active surface is a browser, walks earlier tabs and
    /// returns the most recent terminal's cwd so the new terminal opens in
    /// the user's last directory rather than the workspace root.
    pub fn cwd_for_new_terminal(&self) -> Option<PathBuf> {
        let PaneContent::Tabs { active, surfaces } = self else {
            return None;
        };
        let active_idx = surfaces
            .iter()
            .position(|surface| surface.id == *active)
            .unwrap_or(0);
        let active_surface = surfaces.get(active_idx)?;
        match &active_surface.kind {
            SurfaceKind::Terminal { cwd, .. } => cwd.clone(),
            SurfaceKind::Browser { .. } => {
                surfaces[..active_idx]
                    .iter()
                    .rev()
                    .find_map(|surface| match &surface.kind {
                        SurfaceKind::Terminal { cwd: Some(cwd), .. } => Some(cwd.clone()),
                        _ => None,
                    })
            }
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentNotificationTarget {
    #[default]
    AgentBar,
    Workspace,
    Both,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentNotificationVisualFlags {
    pub agent_bar: bool,
    pub workspace: bool,
    pub desktop_toast: bool,
}

impl AgentNotificationVisualFlags {
    pub fn for_unread(target: AgentNotificationTarget, desktop_toast: bool) -> Self {
        Self {
            agent_bar: matches!(
                target,
                AgentNotificationTarget::AgentBar | AgentNotificationTarget::Both
            ),
            workspace: matches!(
                target,
                AgentNotificationTarget::Workspace | AgentNotificationTarget::Both
            ),
            desktop_toast,
        }
    }

    pub fn clear(self) -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentNotificationClearTrigger {
    WorkspaceClick,
    PaneFocus,
    AgentBarItemClick,
}

pub fn clear_agent_notification_visuals(
    _trigger: AgentNotificationClearTrigger,
    flags: AgentNotificationVisualFlags,
) -> AgentNotificationVisualFlags {
    flags.clear()
}

/// Public/live status of an AI coding agent inside a surface. `Done` is a
/// derived user-visible state: the underlying agent is idle, but it finished
/// while the user was not looking at that surface.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Unknown,
    Idle,
    Working,
    Done,
    Blocked,
}

impl AgentStatus {
    pub fn from_activity(activity: AgentActivity) -> Self {
        match activity {
            AgentActivity::Running => AgentStatus::Working,
            AgentActivity::NeedsInput => AgentStatus::Blocked,
            AgentActivity::Idle => AgentStatus::Idle,
        }
    }

    pub fn to_activity(self) -> AgentActivity {
        match self {
            AgentStatus::Working => AgentActivity::Running,
            AgentStatus::Blocked => AgentActivity::NeedsInput,
            AgentStatus::Unknown | AgentStatus::Idle | AgentStatus::Done => AgentActivity::Idle,
        }
    }

    /// Rollup priority for workspace/sidebar status. Higher wins.
    pub fn rollup_rank(self) -> u8 {
        match self {
            AgentStatus::Blocked => 5,
            AgentStatus::Done => 4,
            AgentStatus::Working => 3,
            AgentStatus::Idle => 2,
            AgentStatus::Unknown => 1,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Unknown => "unknown",
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Done => "done",
            AgentStatus::Blocked => "blocked",
        }
    }

    fn should_mark_unseen_on_idle(prev: AgentStatus, next: AgentStatus) -> bool {
        matches!(prev, AgentStatus::Working | AgentStatus::Blocked) && next == AgentStatus::Idle
    }
}

/// Live activity state of an AI coding agent (Claude Code, Codex,
/// OpenCode) running inside a surface. Driven by the agent's lifecycle
/// hooks. Runtime-only — never persisted (see [`PaneSurface::agent`]).
///
/// State machine mirrors cmux: `UserPromptSubmit` → [`Running`], `Stop`
/// → [`Idle`], `Notification` → [`NeedsInput`]; `SessionEnd` or the
/// daemon's PID liveness sweep clears the presence entirely.
///
/// [`Running`]: AgentActivity::Running
/// [`Idle`]: AgentActivity::Idle
/// [`NeedsInput`]: AgentActivity::NeedsInput
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivity {
    /// Agent is actively working a turn (prompt submitted, tools running).
    Running,
    /// Agent is blocked waiting for the user (permission/input prompt).
    NeedsInput,
    /// Agent finished a turn and is present but idle.
    Idle,
}

impl AgentActivity {
    pub fn status(self) -> AgentStatus {
        AgentStatus::from_activity(self)
    }
}

impl From<AgentActivity> for AgentStatus {
    fn from(value: AgentActivity) -> Self {
        value.status()
    }
}

/// Normalized agent status report before it is merged into a surface's
/// runtime-only [`AgentPresence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatusReport {
    pub name: String,
    pub status: Option<AgentStatus>,
    pub activity: Option<AgentActivity>,
    pub pid: Option<u32>,
    pub source: Option<String>,
    pub seq: Option<u64>,
    pub message: Option<String>,
    pub custom_status: Option<String>,
    pub session_id: Option<String>,
}

impl AgentStatusReport {
    pub fn from_activity(
        name: impl Into<String>,
        activity: Option<AgentActivity>,
        pid: Option<u32>,
    ) -> Self {
        Self {
            name: name.into(),
            status: activity.map(AgentStatus::from_activity),
            activity,
            pid,
            source: None,
            seq: None,
            message: None,
            custom_status: None,
            session_id: None,
        }
    }

    pub fn effective_status(&self) -> Option<AgentStatus> {
        self.status.or_else(|| self.activity.map(AgentStatus::from))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAgentBlock {
    pub pane: PaneId,
    pub surface: SurfaceId,
    pub agent_name: String,
    pub status: AgentStatus,
    pub seen: bool,
    pub status_text: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentBarVisualStatus {
    Working,
    Waiting,
    Done,
}

pub fn agent_bar_visual_status(status: AgentStatus) -> Option<AgentBarVisualStatus> {
    match status {
        AgentStatus::Working => Some(AgentBarVisualStatus::Working),
        AgentStatus::Idle | AgentStatus::Blocked => Some(AgentBarVisualStatus::Waiting),
        AgentStatus::Done => Some(AgentBarVisualStatus::Done),
        AgentStatus::Unknown => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentBarItemWidth {
    pub width_px: u16,
    pub ellipsize: bool,
}

pub fn clamp_agent_bar_item_width(preferred_px: u16) -> AgentBarItemWidth {
    AgentBarItemWidth {
        width_px: preferred_px.clamp(AGENT_BAR_ITEM_MIN_WIDTH_PX, AGENT_BAR_ITEM_MAX_WIDTH_PX),
        ellipsize: preferred_px > AGENT_BAR_ITEM_MAX_WIDTH_PX,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBarItem {
    pub workspace: WorkspaceId,
    pub pane: PaneId,
    pub surface: SurfaceId,
    pub agent_name: String,
    pub status: AgentStatus,
    pub visual_status: AgentBarVisualStatus,
    pub seen: bool,
    pub status_text: String,
    pub color: String,
}

impl AgentBarItem {
    fn from_presence(
        workspace: WorkspaceId,
        pane: PaneId,
        surface: SurfaceId,
        agent: &AgentPresence,
        workspace_color: Option<&str>,
    ) -> Option<Self> {
        let status = agent.public_status();
        let visual_status = agent_bar_visual_status(status)?;
        Some(Self {
            workspace,
            pane,
            surface,
            agent_name: agent.name.clone(),
            status,
            visual_status,
            seen: agent.seen,
            status_text: agent
                .status_text()
                .map(str::to_string)
                .unwrap_or_else(|| status.as_str().to_string()),
            color: workspace_color
                .map(str::to_string)
                .unwrap_or_else(|| agent_bar_color_for_surface(surface)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBarModel {
    pub visible: bool,
    pub items: Vec<AgentBarItem>,
}

pub fn collect_agent_bar_model<'a>(
    workspaces: impl IntoIterator<Item = &'a Workspace>,
) -> AgentBarModel {
    let mut items = Vec::new();
    for workspace in workspaces {
        items.extend(workspace.collect_agent_bar_items());
    }
    AgentBarModel {
        visible: !items.is_empty(),
        items,
    }
}

pub fn agent_bar_color_for_surface(surface: SurfaceId) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in surface.0.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let r = 64 + ((hash & 0x9f) as u8);
    let g = 64 + (((hash >> 16) & 0x9f) as u8);
    let b = 64 + (((hash >> 32) & 0x9f) as u8);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Presence source tag for agents discovered by the process-tree sweep
/// (`flowmux_procmon::agent_name_in_tree`), as opposed to `flowmux:hook`
/// (agent hook reports) or `flowmux:screen` (terminal text heuristics).
pub const AGENT_SOURCE_PROC: &str = "flowmux:proc";

/// Apply process-truth to one surface's agent slot. See
/// [`Pane::reconcile_process_agent`]. Split out as a free function so it can be
/// unit-tested against a bare `Option<AgentPresence>`.
fn reconcile_surface_process_agent(
    slot: &mut Option<AgentPresence>,
    detected: Option<&str>,
) -> bool {
    match (slot.as_mut(), detected) {
        (None, Some(name)) => {
            let mut presence = AgentPresence::new(name, AgentActivity::Idle, None);
            presence.source = Some(AGENT_SOURCE_PROC.to_string());
            *slot = Some(presence);
            true
        }
        (Some(existing), None) => {
            if existing.source.as_deref() == Some(AGENT_SOURCE_PROC) {
                *slot = None;
                true
            } else {
                false
            }
        }
        (Some(existing), Some(name)) => {
            // Process truth is authoritative on *identity*. Correct a proc-owned
            // presence whose agent swapped, and reclaim one a screen-text scan
            // mislabeled — terminal scrollback that merely *mentions* another
            // agent must not leave the pane renamed. Hook-owned presences are
            // left to the hook's own lifecycle.
            let reclaimable = matches!(
                existing.source.as_deref(),
                Some(AGENT_SOURCE_PROC) | Some("flowmux:screen")
            );
            if reclaimable && existing.name != name {
                existing.name = name.to_string();
                existing.source = Some(AGENT_SOURCE_PROC.to_string());
                true
            } else {
                false
            }
        }
        (None, None) => false,
    }
}

/// An AI agent currently occupying a surface, with its live activity and
/// (when known) the agent process PID used for liveness sweeps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPresence {
    /// Agent identity as reported by its hook (`claude`, `codex`,
    /// `opencode`). Lowercase CLI name.
    pub name: String,
    pub activity: AgentActivity,
    pub status: AgentStatus,
    /// PID of the agent process (from the wrapper shim's
    /// `FLOWMUX_AGENT_PID`). `None` for agents without a wrapper; such
    /// presences are cleared by hooks only, not the PID sweep.
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default = "agent_presence_seen_default")]
    pub seen: bool,
}

fn agent_presence_seen_default() -> bool {
    true
}

impl AgentPresence {
    pub fn new(name: impl Into<String>, activity: AgentActivity, pid: Option<u32>) -> Self {
        Self {
            name: name.into(),
            activity,
            status: activity.status(),
            pid,
            source: None,
            seq: None,
            message: None,
            custom_status: None,
            session_id: None,
            seen: true,
        }
    }

    pub fn from_report(report: AgentStatusReport, _visible: bool) -> Option<Self> {
        let status = report.effective_status()?;
        let mut presence = Self::new(report.name, status.to_activity(), report.pid);
        presence.status = status;
        presence.source = report.source;
        presence.seq = report.seq;
        presence.message = report.message;
        presence.custom_status = report.custom_status;
        presence.session_id = report.session_id;
        // A freshly reported idle agent is an active session waiting for input,
        // not an unseen completed turn. Only transitions from a prior live state
        // to hidden Idle become public Done until the user sees them.
        presence.seen = true;
        Some(presence)
    }

    pub fn public_status(&self) -> AgentStatus {
        if self.status == AgentStatus::Idle && !self.seen {
            AgentStatus::Done
        } else {
            self.status
        }
    }

    pub fn status_text(&self) -> Option<&str> {
        self.custom_status
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                self.message
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
            })
    }

    /// Merge a hook/screen report into this presence. Returns `false` when the
    /// report was stale and should not be published.
    pub fn apply_report(&mut self, report: AgentStatusReport, visible: bool) -> bool {
        if let (Some(current), Some(incoming)) = (self.seq, report.seq) {
            if incoming <= current {
                return false;
            }
        }
        let next = report.effective_status().unwrap_or(self.status);
        let prev = self.status;
        let same_agent = self.name == report.name;
        // Process-tree truth owns *identity*: a screen-text scan must not rename
        // (or re-own) a proc-owned presence. Terminal scrollback routinely
        // *mentions* other agents — a log line, a file listing, or an AI chat
        // about agents — so the name heuristic would otherwise relabel a running
        // `claude` pane as `cline`. The 2s proc sweep, not the screen, corrects a
        // pane whose agent genuinely swapped. Hook-owned presences stay
        // screen-replaceable so a stale hook can be superseded (see
        // `screen_fallback_replaces_stale_claude_name_when_agent_signal_differs`).
        let screen_defers_to_proc = report.source.as_deref() == Some("flowmux:screen")
            && self.source.as_deref() == Some(AGENT_SOURCE_PROC);
        if !screen_defers_to_proc {
            self.name = report.name;
        }
        self.status = next;
        self.activity = next.to_activity();
        if report.pid.is_some() {
            self.pid = report.pid;
        }
        if let Some(source) = report.source {
            // A screen scan may refine the *status* of a presence owned by a
            // stronger source (a hook, or the process-tree sweep) but must not
            // steal *ownership*: otherwise the screen-cleared path would later
            // drop a presence whose existence is guaranteed by a live process or
            // an active hook session. `screen_defers_to_proc` also covers the
            // case where the scan named a *different* agent than a proc-owned
            // presence (the scrollback false-positive above).
            let keep_ownership = screen_defers_to_proc
                || (same_agent
                    && matches!(
                        self.source.as_deref(),
                        Some("flowmux:hook") | Some(AGENT_SOURCE_PROC)
                    )
                    && source == "flowmux:screen");
            if !keep_ownership {
                self.source = Some(source);
            }
        }
        if report.seq.is_some() {
            self.seq = report.seq;
        }
        self.message = report.message;
        self.custom_status = report.custom_status;
        if report.session_id.is_some() {
            self.session_id = report.session_id;
        }
        let should_remain_unseen = !visible
            && (AgentStatus::should_mark_unseen_on_idle(prev, next)
                || (prev == AgentStatus::Idle && !self.seen));
        self.seen = next != AgentStatus::Idle || !should_remain_unseen;
        true
    }

    pub fn mark_seen(&mut self) -> bool {
        let was_done = self.public_status() == AgentStatus::Done;
        self.seen = true;
        was_done
    }
}

pub fn detect_agent_status_from_signals(
    screen_text: Option<&str>,
    osc_title: Option<&str>,
) -> Option<AgentStatus> {
    let title = osc_title.unwrap_or_default().trim();
    let title_lower = title.to_ascii_lowercase();
    if title_lower.contains("action required")
        || title_lower.contains("needs input")
        || title_lower.contains("permission required")
        || title_lower.contains("needs permission")
    {
        return Some(AgentStatus::Blocked);
    }
    if title.chars().any(is_braille_spinner)
        || title_lower.contains("working")
        || title_lower.contains("thinking")
        || title_lower.contains("running")
    {
        return Some(AgentStatus::Working);
    }

    let text = screen_text.unwrap_or_default();
    let recent = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    if recent.contains("do you want to")
        || recent.contains("approve this")
        || recent.contains("approve command")
        || recent.contains("requires approval")
        || recent.contains("waiting for approval")
        || recent.contains("awaiting approval")
        || recent.contains("needs approval")
        || recent.contains("allow this")
        || recent.contains("permission to")
        || recent.contains("permission prompt")
        || recent.contains("requires permission")
        || recent.contains("continue?")
        || recent.contains("proceed?")
        || recent.contains("action required")
    {
        return Some(AgentStatus::Blocked);
    }
    if recent.contains("working")
        || recent.contains("thinking")
        || recent.contains("running tool")
        || recent.contains("executing")
    {
        return Some(AgentStatus::Working);
    }
    if title_lower.contains("idle") {
        return Some(AgentStatus::Idle);
    }
    None
}

pub fn detect_agent_name_from_signals(
    screen_text: Option<&str>,
    osc_title: Option<&str>,
) -> Option<&'static str> {
    if let Some(name) = osc_title.and_then(detect_agent_name_from_surface_title) {
        return Some(name);
    }
    if let Some(text) = screen_text {
        for line in text.lines().rev().take(80) {
            if let Some(name) = detect_agent_name_from_screen_status_line(line) {
                return Some(name);
            }
        }
    }
    None
}

fn detect_agent_name_from_screen_status_line(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if !line_has_agent_status_cue(&lower) {
        return None;
    }
    agent_name_in_text(&lower)
}

fn line_has_agent_status_cue(line: &str) -> bool {
    line.contains("action required")
        || line.contains("needs input")
        || line.contains("permission required")
        || line.contains("needs permission")
        || line.contains("approve this")
        || line.contains("approve command")
        || line.contains("requires approval")
        || line.contains("waiting for approval")
        || line.contains("awaiting approval")
        || line.contains("needs approval")
        || line.contains("permission prompt")
        || line.contains("requires permission")
        || line.contains("working")
        || line.contains("thinking")
        || line.contains("running tool")
        || line.contains("executing")
        || line.contains("idle")
}

fn agent_name_in_text(text: &str) -> Option<&'static str> {
    if text.contains("opencode") || text.contains("open code") {
        Some("opencode")
    } else if contains_ascii_token(text, "claude") {
        Some("claude")
    } else if contains_ascii_token(text, "codex") {
        Some("codex")
    } else if contains_ascii_token(text, "cline") {
        Some("cline")
    } else {
        None
    }
}

pub fn detect_agent_idle_name_from_signals(
    screen_text: Option<&str>,
    osc_title: Option<&str>,
) -> Option<&'static str> {
    let title_agent_name = osc_title.and_then(detect_agent_name_from_surface_title);
    let text = screen_text.unwrap_or_default();
    let recent = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let looks_like_agent_prompt = recent.lines().any(is_agent_idle_prompt_line);
    if !looks_like_agent_prompt {
        return (screen_text.is_none() || text.trim().is_empty())
            .then_some(title_agent_name)
            .flatten();
    }
    title_agent_name
        .or_else(|| agent_name_in_text(&recent))
        .or_else(|| {
            (recent.contains("ask anything") && recent.contains("ctrl+p commands"))
                .then_some("opencode")
        })
}

fn is_agent_idle_prompt_line(line: &str) -> bool {
    let line = line.trim();
    let is_shell_command = line.contains("\\n")
        || line.starts_with("printf ")
        || line.starts_with("echo ")
        || line.starts_with("clear;")
        || line.contains("; echo ");
    !is_shell_command
        && (line.contains("press / for commands")
            || line.contains("press / to")
            || line.contains("type /")
            || line.contains("ask anything")
            || line.contains("ask me anything"))
}

fn normalize_agent_report_name_for_surface_title(report: &mut AgentStatusReport, title: &str) {
    if let Some(name) = detect_agent_name_from_surface_title(title) {
        report.name = name.to_string();
    }
}

fn detect_agent_name_from_surface_title(title: &str) -> Option<&'static str> {
    let lower = title.trim_start().to_ascii_lowercase();
    let first = lower
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '|'))
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    if first == "opencode"
        || lower.starts_with("open code")
        || lower.starts_with("oc |")
        || lower.starts_with("oc|")
    {
        Some("opencode")
    } else if first == "claude" {
        Some("claude")
    } else if first == "codex" {
        Some("codex")
    } else if first == "cline" {
        Some("cline")
    } else {
        None
    }
}

fn contains_ascii_token(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == needle)
}

fn is_braille_spinner(ch: char) -> bool {
    matches!(ch as u32, 0x2800..=0x28ff)
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
            terminal_tab_title_for_cwd(Some(Path::new("/tmp/1234567890123456789"))),
            "12345678901234567..."
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
    fn pane_content_resets_stale_terminal_titles_on_normalize() {
        // Previously this test asserted the opposite — that an unlocked
        // terminal whose title didn't match the cwd was AUTO-LOCKED.
        // That kept stale OSC 0/2 titles ("Claude Code", "codex foo")
        // alive across app restarts. The current behavior resets the
        // title back to the cwd-derived form and stays unlocked, so the
        // next process inside the tab can paint a fresh title.
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
        assert_eq!(
            surfaces[0].title,
            terminal_tab_title_for_cwd(Some(std::path::Path::new("/tmp/project")))
        );
        assert!(!surfaces[0].title_locked);
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
    fn terminal_surface_cwd_uses_active_tab_cwd() {
        // A pane with three terminal tabs at /tmp, /home, /bin. While viewing
        // /home, splitting should seed the new terminal at /home.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("tmp", Some("/tmp".into())),
        };
        let home = PaneSurface::terminal("home", Some("/home".into()));
        let home_id = home.id;
        let bin = PaneSurface::terminal("bin", Some("/bin".into()));
        pane.add_surface_to_leaf(pane_id, home).unwrap();
        pane.add_surface_to_leaf(pane_id, bin).unwrap();
        assert!(pane.set_active_surface(pane_id, home_id));

        assert_eq!(
            pane.terminal_surface_cwd(pane_id),
            Some(std::path::PathBuf::from("/home"))
        );
    }

    #[test]
    fn terminal_surface_cwd_falls_back_to_prior_terminal_when_browser_active() {
        // Tabs in order: terminal(/tmp), browser, terminal(/home). With the
        // browser active, the new terminal should inherit /tmp - the most
        // recent terminal that comes *before* the browser in tab order.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("tmp", Some("/tmp".into())),
        };
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        let home = PaneSurface::terminal("home", Some("/home".into()));
        pane.add_surface_to_leaf(pane_id, browser).unwrap();
        pane.add_surface_to_leaf(pane_id, home).unwrap();
        assert!(pane.set_active_surface(pane_id, browser_id));

        assert_eq!(
            pane.terminal_surface_cwd(pane_id),
            Some(std::path::PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn terminal_surface_cwd_picks_most_recent_terminal_before_browser() {
        // Tabs in order: terminal(/a), terminal(/b), browser. With the browser
        // active, the closest terminal before it (/b) wins.
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::tabbed_terminal("a", Some("/a".into())),
        };
        let b = PaneSurface::terminal("b", Some("/b".into()));
        pane.add_surface_to_leaf(pane_id, b).unwrap();
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        pane.add_surface_to_leaf(pane_id, browser).unwrap();
        assert!(pane.set_active_surface(pane_id, browser_id));

        assert_eq!(
            pane.terminal_surface_cwd(pane_id),
            Some(std::path::PathBuf::from("/b"))
        );
    }

    #[test]
    fn terminal_surface_cwd_returns_none_when_browser_has_no_prior_terminal() {
        // Tabs in order: browser, terminal(/home). With the browser active,
        // there is no terminal *before* it - resolution returns None so the
        // caller falls back to the workspace root.
        let pane_id = PaneId::new();
        let pane_id_inner = pane_id;
        let mut pane = Pane::Leaf {
            id: pane_id_inner,
            content: PaneContent::Tabs {
                active: SurfaceId::new(),
                surfaces: vec![],
            },
        };
        let browser = PaneSurface::browser("docs", "https://docs.test".into());
        let browser_id = browser.id;
        let term = PaneSurface::terminal("home", Some("/home".into()));
        // Manually set the surfaces vec because tabbed_terminal would create
        // a terminal first.
        if let Pane::Leaf {
            content: PaneContent::Tabs { active, surfaces },
            ..
        } = &mut pane
        {
            surfaces.push(browser);
            surfaces.push(term);
            *active = browser_id;
        }

        assert_eq!(pane.terminal_surface_cwd(pane_id), None);
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
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "Claude Code".into(),));
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
        assert!(!title_is_shell_cwd_echo("junsu@host: ~/dev/os", cwd, None,));
    }

    #[test]
    fn title_is_shell_cwd_echo_passes_program_titles() {
        let cwd = Path::new("/tmp/flowmux-shell-echo-test");
        // Titles from external programs such as vi/codex/claude/tmux do not
        // match the PS1 pattern (`prefix:[ ]<cwd>`).
        assert!(!title_is_shell_cwd_echo("vim src/main.rs", cwd, None,));
        assert!(!title_is_shell_cwd_echo("tmux: 0:bash*", cwd, None,));
        assert!(!title_is_shell_cwd_echo("claude — Anthropic", cwd, None,));
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
        // flowmux's cwd-notify flow applies folder names through
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
        assert!(pane.set_surface_title_auto(pane_id, surface_id, "vim src/main.rs".into(),));
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
        let (
            PaneContent::Tabs {
                surfaces: t_surfs, ..
            },
            PaneContent::Tabs {
                surfaces: n_surfs, ..
            },
        ) = (&target_content, &new_content)
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
        fn find_tabs(p: &Pane, target: PaneId) -> Option<&PaneContent> {
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
        assert_eq!(pane.find_right_sibling_browser_leaf(term_id), Some(near_id));
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
        assert_eq!(pane.find_right_sibling_browser_leaf(term_id), Some(far_id));
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
        assert_eq!(
            WorkspaceId::from_str(&format!("workspace:{s}")).unwrap(),
            ws
        );
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

    /// Stale OSC 0/2 titles ("Claude Code", "codex", "vim foo") that
    /// were captured into the persisted state must NOT survive into
    /// the next launch — the process that emitted them is gone, so
    /// the tab should restart with a cwd-derived title.
    #[test]
    fn normalize_resets_unlocked_title_to_cwd_after_relaunch() {
        let cwd = std::path::PathBuf::from("/home/u/dev/os/flowmux");
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "Claude Code".into(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: Some(cwd.clone()),
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(changed, "stale OSC title should be reset");
        assert_eq!(surface.title, terminal_tab_title_for_cwd(Some(&cwd)));
        assert!(
            !surface.title_locked,
            "must not auto-lock — the title was never the user's intent"
        );
    }

    #[test]
    fn agent_presence_is_never_persisted() {
        let surface = PaneSurface {
            id: SurfaceId::new(),
            title: "Claude Code".into(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: None,
            },
            agent: Some(AgentPresence::new(
                "claude",
                AgentActivity::Running,
                Some(4321),
            )),
        };
        let json = serde_json::to_string(&surface).unwrap();
        assert!(
            !json.contains("agent") && !json.contains("claude"),
            "agent presence must be skipped from serialization, got: {json}"
        );
        // Round-trips back with no agent (runtime-only field); the rest
        // of the surface survives.
        let back: PaneSurface = serde_json::from_str(&json).unwrap();
        assert!(back.agent.is_none());
        assert_eq!(back.id, surface.id);
        assert_eq!(back.title, surface.title);
    }

    #[test]
    fn agent_activity_maps_to_public_status() {
        assert_eq!(AgentActivity::Running.status(), AgentStatus::Working);
        assert_eq!(AgentActivity::NeedsInput.status(), AgentStatus::Blocked);
        assert_eq!(AgentActivity::Idle.status(), AgentStatus::Idle);
    }

    #[test]
    fn agent_presence_ignores_stale_seq() {
        let mut presence = AgentPresence::new("codex", AgentActivity::Running, Some(7));
        presence.seq = Some(20);
        let applied = presence.apply_report(
            AgentStatusReport {
                name: "codex".into(),
                status: Some(AgentStatus::Idle),
                activity: Some(AgentActivity::Idle),
                pid: Some(7),
                source: Some("flowmux:hook".into()),
                seq: Some(19),
                message: None,
                custom_status: None,
                session_id: None,
            },
            true,
        );
        assert!(!applied);
        assert_eq!(presence.public_status(), AgentStatus::Working);
    }

    #[test]
    fn apply_report_screen_scan_keeps_proc_owned_identity() {
        // Regression: a `claude` pane whose scrollback mentions another agent
        // (e.g. an AI chat *about* `cline`) must not be relabeled. Process truth
        // owns the identity; a screen scan may only refine the status.
        let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
        presence.source = Some(AGENT_SOURCE_PROC.to_string());
        let applied = presence.apply_report(
            AgentStatusReport {
                name: "cline".into(),
                status: Some(AgentStatus::Working),
                activity: Some(AgentActivity::Running),
                pid: None,
                source: Some("flowmux:screen".into()),
                seq: None,
                message: None,
                custom_status: None,
                session_id: None,
            },
            true,
        );
        assert!(applied);
        assert_eq!(presence.name, "claude", "screen must not rename a proc-owned presence");
        assert_eq!(presence.source.as_deref(), Some(AGENT_SOURCE_PROC));
        assert_eq!(presence.status, AgentStatus::Working, "status still refines");
    }

    #[test]
    fn working_to_idle_in_hidden_surface_becomes_done_until_seen() {
        let mut presence = AgentPresence::new("claude", AgentActivity::Running, None);
        presence.seq = Some(1);
        let applied = presence.apply_report(
            AgentStatusReport {
                name: "claude".into(),
                status: Some(AgentStatus::Idle),
                activity: Some(AgentActivity::Idle),
                pid: None,
                source: Some("flowmux:hook".into()),
                seq: Some(2),
                message: None,
                custom_status: None,
                session_id: None,
            },
            false,
        );
        assert!(applied);
        assert_eq!(presence.status, AgentStatus::Idle);
        assert_eq!(presence.public_status(), AgentStatus::Done);
        assert!(presence.mark_seen());
        assert_eq!(presence.public_status(), AgentStatus::Idle);
    }

    #[test]
    fn initial_idle_agent_report_in_hidden_surface_stays_idle() {
        let presence = AgentPresence::from_report(
            AgentStatusReport {
                name: "cline".into(),
                status: Some(AgentStatus::Idle),
                activity: Some(AgentActivity::Idle),
                pid: Some(42),
                source: Some("flowmux:hook".into()),
                seq: Some(1),
                message: None,
                custom_status: None,
                session_id: None,
            },
            false,
        )
        .expect("idle report should create presence");

        assert_eq!(presence.status, AgentStatus::Idle);
        assert_eq!(presence.public_status(), AgentStatus::Idle);
        assert!(presence.seen);
    }

    #[test]
    fn agent_presence_replaces_transient_message_metadata() {
        let mut presence = AgentPresence::new("claude", AgentActivity::NeedsInput, None);
        presence.message = Some("approval needed".into());
        presence.custom_status = Some("waiting".into());
        presence.session_id = Some("session-1".into());
        presence.seq = Some(1);

        let applied = presence.apply_report(
            AgentStatusReport {
                name: "claude".into(),
                status: Some(AgentStatus::Working),
                activity: Some(AgentActivity::Running),
                pid: None,
                source: Some("flowmux:hook".into()),
                seq: Some(2),
                message: None,
                custom_status: None,
                session_id: None,
            },
            true,
        );

        assert!(applied);
        assert_eq!(presence.public_status(), AgentStatus::Working);
        assert_eq!(presence.message, None);
        assert_eq!(presence.custom_status, None);
        assert_eq!(presence.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn hook_report_uses_opencode_name_when_surface_title_has_oc_prefix() {
        let surface = PaneSurface::terminal("OC | greeting", None);
        let surface_id = surface.id;
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent(
                surface_id,
                AgentStatusReport {
                    name: "claude".into(),
                    status: Some(AgentStatus::Idle),
                    activity: Some(AgentActivity::Idle),
                    pid: None,
                    source: Some("flowmux:hook".into()),
                    seq: Some(1),
                    message: None,
                    custom_status: None,
                    session_id: Some("ses-opencode".into()),
                },
                true,
            ),
            Some(true)
        );
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected leaf pane");
        };
        let agent = surfaces[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
    }

    #[test]
    fn agent_presence_status_text_prefers_custom_status_then_message() {
        let mut presence = AgentPresence::new("codex", AgentActivity::Running, None);
        presence.message = Some("running tests".into());
        presence.custom_status = Some("reviewing patch".into());
        assert_eq!(presence.status_text(), Some("reviewing patch"));

        presence.custom_status = Some("   ".into());
        assert_eq!(presence.status_text(), Some("running tests"));

        presence.message = Some(" ".into());
        assert_eq!(presence.status_text(), None);
    }

    fn terminal_surface_with_agent(
        title: &str,
        cwd: &str,
        agent_name: &str,
        status: AgentStatus,
    ) -> PaneSurface {
        let mut surface = PaneSurface::terminal(title, Some(std::path::PathBuf::from(cwd)));
        let mut presence = AgentPresence::new(agent_name, status.to_activity(), None);
        presence.status = status;
        presence.custom_status = Some(format!("{agent_name} status"));
        surface.agent = Some(presence);
        surface
    }

    fn workspace_with_agent_leaves(leaves: Vec<(PaneId, PaneSurface)>) -> Workspace {
        fn leaf(id: PaneId, surface: PaneSurface) -> Pane {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs {
                    active: surface.id,
                    surfaces: vec![surface],
                },
            }
        }

        let root = leaves
            .into_iter()
            .map(|(id, surface)| leaf(id, surface))
            .reduce(|first, second| Pane::Split {
                id: PaneId::new(),
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            })
            .expect("workspace needs at least one leaf");

        Workspace {
            id: WorkspaceId::new(),
            name: "agents".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: Vec::new(),
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: root,
            }],
            color: None,
        }
    }

    fn workspace_with_tab_leaves(leaves: Vec<(PaneId, Vec<PaneSurface>)>) -> Workspace {
        fn leaf(id: PaneId, surfaces: Vec<PaneSurface>) -> Pane {
            let active = surfaces
                .first()
                .map(|surface| surface.id)
                .expect("test leaf needs at least one surface");
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { active, surfaces },
            }
        }

        let root = leaves
            .into_iter()
            .map(|(id, surfaces)| leaf(id, surfaces))
            .reduce(|first, second| Pane::Split {
                id: PaneId::new(),
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            })
            .expect("workspace needs at least one leaf");

        Workspace {
            id: WorkspaceId::new(),
            name: "agents".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: Vec::new(),
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: root,
            }],
            color: None,
        }
    }

    #[test]
    fn workspace_agent_blocks_sort_by_status_then_recent_focus() {
        let working_old = PaneId::new();
        let blocked = PaneId::new();
        let working_recent = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![
            (
                working_old,
                terminal_surface_with_agent("old", "/tmp/old", "codex", AgentStatus::Working),
            ),
            (
                blocked,
                terminal_surface_with_agent(
                    "blocked",
                    "/tmp/blocked",
                    "claude",
                    AgentStatus::Blocked,
                ),
            ),
            (
                working_recent,
                terminal_surface_with_agent(
                    "recent",
                    "/tmp/recent",
                    "opencode",
                    AgentStatus::Working,
                ),
            ),
        ]);

        let blocks = ws.collect_agent_blocks(&[working_recent, working_old, blocked]);
        assert_eq!(
            blocks.iter().map(|block| block.pane).collect::<Vec<_>>(),
            vec![blocked, working_recent, working_old]
        );
        assert_eq!(blocks[0].status, AgentStatus::Blocked);
        assert_eq!(blocks[0].status_text.as_deref(), Some("claude status"));
        assert_eq!(blocks[0].cwd.as_deref(), Some("/tmp/blocked"));
    }

    #[test]
    fn workspace_agent_blocks_exclude_unknown_status() {
        let unknown = PaneId::new();
        let idle = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![
            (
                unknown,
                terminal_surface_with_agent(
                    "unknown",
                    "/tmp/unknown",
                    "codex",
                    AgentStatus::Unknown,
                ),
            ),
            (
                idle,
                terminal_surface_with_agent("idle", "/tmp/idle", "claude", AgentStatus::Idle),
            ),
        ]);

        let blocks = ws.collect_agent_blocks(&[unknown, idle]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].pane, idle);
        assert_eq!(blocks[0].status, AgentStatus::Idle);
    }

    #[test]
    fn agent_bar_model_hidden_when_no_agents_exist() {
        let pane = PaneId::new();
        let ws = workspace_with_tab_leaves(vec![(
            pane,
            vec![PaneSurface::terminal("shell", Some("/tmp/shell".into()))],
        )]);

        let model = collect_agent_bar_model([&ws]);

        assert!(!model.visible);
        assert!(model.items.is_empty());
    }

    #[test]
    fn agent_bar_model_collects_one_agent_with_name_and_status_text() {
        let pane = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![(
            pane,
            terminal_surface_with_agent("codex", "/tmp/codex", "codex", AgentStatus::Working),
        )]);

        let model = collect_agent_bar_model([&ws]);

        assert!(model.visible);
        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].workspace, ws.id);
        assert_eq!(model.items[0].pane, pane);
        assert_eq!(model.items[0].agent_name, "codex");
        assert_eq!(model.items[0].status_text, "codex status");
        assert_eq!(model.items[0].visual_status, AgentBarVisualStatus::Working);
    }

    #[test]
    fn agent_bar_model_uses_workspace_color_for_item_stripe() {
        let pane = PaneId::new();
        let mut ws = workspace_with_tab_leaves(vec![(
            pane,
            vec![
                terminal_surface_with_agent("codex", "/tmp/codex", "codex", AgentStatus::Working),
                terminal_surface_with_agent("claude", "/tmp/claude", "claude", AgentStatus::Idle),
            ],
        )]);
        ws.color = Some("#112233".into());

        let model = collect_agent_bar_model([&ws]);
        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.color.as_str())
                .collect::<Vec<_>>(),
            vec!["#112233", "#112233"]
        );

        ws.color = Some("#445566".into());
        let model = collect_agent_bar_model([&ws]);
        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.color.as_str())
                .collect::<Vec<_>>(),
            vec!["#445566", "#445566"]
        );
    }

    #[test]
    fn agent_bar_model_keeps_workspace_pane_tab_discovery_order() {
        let first_pane = PaneId::new();
        let second_pane = PaneId::new();
        let third_pane = PaneId::new();
        let ws_a = workspace_with_tab_leaves(vec![
            (
                first_pane,
                vec![
                    terminal_surface_with_agent("a1", "/tmp/a1", "codex", AgentStatus::Working),
                    terminal_surface_with_agent("a2", "/tmp/a2", "claude", AgentStatus::Idle),
                ],
            ),
            (
                second_pane,
                vec![terminal_surface_with_agent(
                    "a3",
                    "/tmp/a3",
                    "opencode",
                    AgentStatus::Blocked,
                )],
            ),
        ]);
        let ws_b = workspace_with_agent_leaves(vec![(
            third_pane,
            terminal_surface_with_agent("b1", "/tmp/b1", "cline", AgentStatus::Done),
        )]);

        let model = collect_agent_bar_model([&ws_a, &ws_b]);

        assert_eq!(
            model
                .items
                .iter()
                .map(|item| item.agent_name.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "claude", "opencode", "cline"]
        );
        assert_eq!(
            model.items.iter().map(|item| item.pane).collect::<Vec<_>>(),
            vec![first_pane, first_pane, second_pane, third_pane]
        );
    }

    #[test]
    fn agent_bar_visual_status_maps_public_statuses() {
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Working),
            Some(AgentBarVisualStatus::Working)
        );
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Idle),
            Some(AgentBarVisualStatus::Waiting)
        );
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Blocked),
            Some(AgentBarVisualStatus::Waiting)
        );
        assert_eq!(
            agent_bar_visual_status(AgentStatus::Done),
            Some(AgentBarVisualStatus::Done)
        );
        assert_eq!(agent_bar_visual_status(AgentStatus::Unknown), None);
    }

    #[test]
    fn agent_bar_model_excludes_unknown_status() {
        let pane = PaneId::new();
        let ws = workspace_with_agent_leaves(vec![(
            pane,
            terminal_surface_with_agent("unknown", "/tmp/unknown", "codex", AgentStatus::Unknown),
        )]);

        let model = collect_agent_bar_model([&ws]);

        assert!(!model.visible);
        assert!(model.items.is_empty());
    }

    #[test]
    fn agent_bar_text_updates_without_reordering_or_recoloring() {
        let pane = PaneId::new();
        let mut ws = workspace_with_agent_leaves(vec![(
            pane,
            terminal_surface_with_agent("codex", "/tmp/codex", "codex", AgentStatus::Working),
        )]);
        let before = collect_agent_bar_model([&ws]).items[0].clone();

        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &mut ws.surfaces[0].root_pane
        else {
            panic!("expected single leaf test workspace");
        };
        surfaces[0].agent.as_mut().unwrap().custom_status = Some("running tests".into());
        let after = collect_agent_bar_model([&ws]).items[0].clone();

        assert_eq!(after.workspace, before.workspace);
        assert_eq!(after.pane, before.pane);
        assert_eq!(after.surface, before.surface);
        assert_eq!(after.agent_name, before.agent_name);
        assert_eq!(after.color, before.color);
        assert_eq!(after.status_text, "running tests");
    }

    #[test]
    fn agent_bar_surface_color_is_deterministic() {
        let surface_a = SurfaceId(uuid::Uuid::from_u128(0x11111111111111111111111111111111));
        let surface_b = SurfaceId(uuid::Uuid::from_u128(0x22222222222222222222222222222222));

        assert_eq!(
            agent_bar_color_for_surface(surface_a),
            agent_bar_color_for_surface(surface_a)
        );
        assert_ne!(
            agent_bar_color_for_surface(surface_a),
            agent_bar_color_for_surface(surface_b)
        );
        assert!(agent_bar_color_for_surface(surface_a).starts_with('#'));
        assert_eq!(agent_bar_color_for_surface(surface_a).len(), 7);
    }

    #[test]
    fn agent_bar_width_clamps_and_reports_ellipsize() {
        assert_eq!(
            clamp_agent_bar_item_width(80),
            AgentBarItemWidth {
                width_px: AGENT_BAR_ITEM_MIN_WIDTH_PX,
                ellipsize: false
            }
        );
        assert_eq!(
            clamp_agent_bar_item_width(126),
            AgentBarItemWidth {
                width_px: 126,
                ellipsize: false
            }
        );
        assert_eq!(
            clamp_agent_bar_item_width(320),
            AgentBarItemWidth {
                width_px: AGENT_BAR_ITEM_MAX_WIDTH_PX,
                ellipsize: true
            }
        );
    }

    #[test]
    fn agent_bar_model_reflects_agent_tab_pane_and_workspace_removal() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        let surface_a = terminal_surface_with_agent("a", "/tmp/a", "codex", AgentStatus::Working);
        let mut ended_surface =
            terminal_surface_with_agent("ended", "/tmp/ended", "claude", AgentStatus::Idle);
        ended_surface.agent = None;
        let surface_b =
            terminal_surface_with_agent("b", "/tmp/b", "opencode", AgentStatus::Blocked);
        let full = workspace_with_tab_leaves(vec![
            (pane_a, vec![surface_a.clone(), ended_surface]),
            (pane_b, vec![surface_b.clone()]),
        ]);
        assert_eq!(collect_agent_bar_model([&full]).items.len(), 2);

        let tab_closed = workspace_with_tab_leaves(vec![
            (pane_a, vec![surface_a.clone()]),
            (pane_b, vec![surface_b.clone()]),
        ]);
        assert_eq!(
            collect_agent_bar_model([&tab_closed])
                .items
                .iter()
                .map(|item| item.agent_name.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "opencode"]
        );

        let pane_closed = workspace_with_tab_leaves(vec![(pane_a, vec![surface_a])]);
        assert_eq!(
            collect_agent_bar_model([&pane_closed])
                .items
                .iter()
                .map(|item| item.agent_name.as_str())
                .collect::<Vec<_>>(),
            vec!["codex"]
        );

        let workspace_closed = collect_agent_bar_model(std::iter::empty::<&Workspace>());
        assert!(!workspace_closed.visible);
        assert!(workspace_closed.items.is_empty());
    }

    #[test]
    fn agent_notification_target_controls_blink_flags() {
        assert_eq!(
            AgentNotificationVisualFlags::for_unread(AgentNotificationTarget::AgentBar, true),
            AgentNotificationVisualFlags {
                agent_bar: true,
                workspace: false,
                desktop_toast: true
            }
        );
        assert_eq!(
            AgentNotificationVisualFlags::for_unread(AgentNotificationTarget::Workspace, true),
            AgentNotificationVisualFlags {
                agent_bar: false,
                workspace: true,
                desktop_toast: true
            }
        );
        assert_eq!(
            AgentNotificationVisualFlags::for_unread(AgentNotificationTarget::Both, false),
            AgentNotificationVisualFlags {
                agent_bar: true,
                workspace: true,
                desktop_toast: false
            }
        );
    }

    #[test]
    fn agent_notification_clear_triggers_clear_all_visual_flags() {
        let flags = AgentNotificationVisualFlags {
            agent_bar: true,
            workspace: true,
            desktop_toast: true,
        };

        for trigger in [
            AgentNotificationClearTrigger::WorkspaceClick,
            AgentNotificationClearTrigger::PaneFocus,
            AgentNotificationClearTrigger::AgentBarItemClick,
        ] {
            assert_eq!(
                clear_agent_notification_visuals(trigger, flags),
                AgentNotificationVisualFlags::default()
            );
        }
    }

    #[test]
    fn agent_notification_target_default_is_agent_bar() {
        assert_eq!(
            AgentNotificationTarget::default(),
            AgentNotificationTarget::AgentBar
        );
        assert_eq!(
            serde_json::to_string(&AgentNotificationTarget::default()).unwrap(),
            "\"agent_bar\""
        );
    }

    #[test]
    fn reconcile_creates_idle_proc_presence_when_agent_process_appears() {
        let mut slot = None;
        assert!(reconcile_surface_process_agent(&mut slot, Some("codex")));
        let p = slot.as_ref().unwrap();
        assert_eq!(p.name, "codex");
        assert_eq!(p.status, AgentStatus::Idle);
        assert_eq!(p.source.as_deref(), Some(AGENT_SOURCE_PROC));
        // Idempotent: a second identical reconcile reports no change.
        assert!(!reconcile_surface_process_agent(&mut slot, Some("codex")));
    }

    #[test]
    fn reconcile_drops_proc_presence_when_agent_process_exits() {
        let mut p = AgentPresence::new("codex", AgentActivity::Idle, None);
        p.source = Some(AGENT_SOURCE_PROC.to_string());
        let mut slot = Some(p);
        assert!(reconcile_surface_process_agent(&mut slot, None));
        assert!(slot.is_none());
    }

    #[test]
    fn reconcile_leaves_hook_owned_presence_when_process_absent() {
        // A hook-owned presence must not be dropped by the process sweep; the
        // hook (and the pid liveness sweep) own its lifecycle.
        let mut p = AgentPresence::new("claude", AgentActivity::Running, Some(42));
        p.source = Some("flowmux:hook".into());
        let mut slot = Some(p);
        assert!(!reconcile_surface_process_agent(&mut slot, None));
        assert!(slot.is_some());
    }

    #[test]
    fn reconcile_follows_agent_swap_for_proc_owned_presence() {
        let mut p = AgentPresence::new("codex", AgentActivity::Idle, None);
        p.source = Some(AGENT_SOURCE_PROC.to_string());
        let mut slot = Some(p);
        assert!(reconcile_surface_process_agent(&mut slot, Some("claude")));
        assert_eq!(slot.as_ref().unwrap().name, "claude");
    }

    #[test]
    fn reconcile_reclaims_screen_owned_presence_mislabeled_by_scrollback() {
        // A screen scan mislabeled a pane because its scrollback *mentioned*
        // another agent; the process sweep reclaims the true identity.
        let mut p = AgentPresence::new("cline", AgentActivity::Idle, None);
        p.source = Some("flowmux:screen".into());
        let mut slot = Some(p);
        assert!(reconcile_surface_process_agent(&mut slot, Some("claude")));
        let agent = slot.as_ref().unwrap();
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.source.as_deref(), Some(AGENT_SOURCE_PROC));
    }

    #[test]
    fn reconcile_is_noop_for_running_screen_owned_presence() {
        let mut p = AgentPresence::new("codex", AgentActivity::Running, None);
        p.source = Some("flowmux:screen".into());
        let mut slot = Some(p);
        assert!(!reconcile_surface_process_agent(&mut slot, Some("codex")));
        assert_eq!(slot.as_ref().unwrap().name, "codex");
    }

    #[test]
    fn screen_working_keeps_proc_ownership_then_settles_idle_without_clearing() {
        // Core regression fix: a screen scan may raise a proc-owned presence to
        // Working, but must not steal ownership — otherwise the screen-idle path
        // would later drop a still-running agent (Codex's exact failure).
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, None);
        presence.source = Some(AGENT_SOURCE_PROC.to_string());
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };
        pane.report_surface_agent_signal(
            surface_id,
            AgentStatus::Working,
            "flowmux:screen",
            Some("codex"),
            true,
        );
        // Working turn ends: proc presence settles to Idle, not cleared.
        assert_eq!(pane.settle_screen_idle(surface_id), Some(true));
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
        // Still present — a second settle is a no-op.
        assert_eq!(pane.settle_screen_idle(surface_id), Some(false));
    }

    #[test]
    fn settle_screen_idle_clears_screen_owned_presence() {
        let mut surface = PaneSurface::terminal("a", None);
        let sid = surface.id;
        let mut p = AgentPresence::new("codex", AgentActivity::Idle, None);
        p.source = Some("flowmux:screen".into());
        surface.agent = Some(p);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: sid,
                surfaces: vec![surface],
            },
        };
        assert_eq!(pane.settle_screen_idle(sid), Some(true));
        assert_eq!(pane.agent_status_rollup(), None);
    }

    #[test]
    fn pane_mark_surface_seen_clears_done() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, None);
        presence.seen = false;
        surface.agent = Some(presence);
        let pane_id = PaneId::new();
        let mut pane = Pane::Leaf {
            id: pane_id,
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Done));
        assert!(pane.mark_surface_agent_seen(surface_id));
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
    }

    #[test]
    fn screen_fallback_does_not_override_claude_hook_presence() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
        presence.source = Some("flowmux:hook".into());
        presence.seq = Some(1);
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent_signal(
                surface_id,
                AgentStatus::Blocked,
                "flowmux:screen",
                Some("claude"),
                true,
            ),
            Some(false)
        );
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
    }

    #[test]
    fn screen_fallback_does_not_take_ownership_from_matching_hook_presence() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, Some(42));
        presence.source = Some("flowmux:hook".into());
        presence.seq = Some(1);
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent_signal(
                surface_id,
                AgentStatus::Working,
                "flowmux:screen",
                Some("codex"),
                true,
            ),
            Some(true)
        );
        assert_eq!(
            pane.clear_surface_agent_from_source(surface_id, "flowmux:screen"),
            Some(false)
        );
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected leaf pane");
        };
        let agent = surfaces[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.status, AgentStatus::Working);
        assert_eq!(agent.source.as_deref(), Some("flowmux:hook"));
        assert_eq!(agent.pid, Some(42));
    }

    #[test]
    fn screen_fallback_replaces_stale_claude_name_when_agent_signal_differs() {
        for detected_agent in ["codex", "opencode", "cline"] {
            let mut surface = PaneSurface::terminal("agent", None);
            let surface_id = surface.id;
            let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
            presence.source = Some("flowmux:hook".into());
            presence.seq = Some(1);
            surface.agent = Some(presence);
            let mut pane = Pane::Leaf {
                id: PaneId::new(),
                content: PaneContent::Tabs {
                    active: surface_id,
                    surfaces: vec![surface],
                },
            };

            assert_eq!(
                pane.report_surface_agent_signal(
                    surface_id,
                    AgentStatus::Blocked,
                    "flowmux:screen",
                    Some(detected_agent),
                    true,
                ),
                Some(true),
                "{detected_agent} should replace stale claude presence"
            );
            let Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } = &pane
            else {
                panic!("expected leaf pane");
            };
            let agent = surfaces[0].agent.as_ref().unwrap();
            assert_eq!(agent.name, detected_agent);
            assert_eq!(agent.status, AgentStatus::Blocked);
            assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
        }
    }

    #[test]
    fn screen_fallback_uses_opencode_title_before_claude_screen_text() {
        let mut surface = PaneSurface::terminal("OC | greeting", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("claude", AgentActivity::Idle, None);
        presence.source = Some("flowmux:hook".into());
        presence.seq = Some(1);
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.report_surface_agent_signal(
                surface_id,
                AgentStatus::Blocked,
                "flowmux:screen",
                Some("claude"),
                true,
            ),
            Some(true)
        );
        let Pane::Leaf {
            content: PaneContent::Tabs { surfaces, .. },
            ..
        } = &pane
        else {
            panic!("expected leaf pane");
        };
        let agent = surfaces[0].agent.as_ref().unwrap();
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.status, AgentStatus::Blocked);
        assert_eq!(agent.source.as_deref(), Some("flowmux:screen"));
    }

    #[test]
    fn clear_surface_agent_from_source_only_clears_matching_source() {
        let mut surface = PaneSurface::terminal("agent", None);
        let surface_id = surface.id;
        let mut presence = AgentPresence::new("codex", AgentActivity::Idle, None);
        presence.source = Some("flowmux:screen".into());
        surface.agent = Some(presence);
        let mut pane = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::Tabs {
                active: surface_id,
                surfaces: vec![surface],
            },
        };

        assert_eq!(
            pane.clear_surface_agent_from_source(surface_id, "flowmux:hook"),
            Some(false)
        );
        assert_eq!(pane.agent_status_rollup(), Some(AgentStatus::Idle));
        assert_eq!(
            pane.clear_surface_agent_from_source(surface_id, "flowmux:screen"),
            Some(true)
        );
        assert_eq!(pane.agent_status_rollup(), None);
    }

    #[test]
    fn agent_status_rollup_uses_blocked_done_working_idle_unknown_order() {
        assert_eq!(
            rollup_agent_statuses([
                AgentStatus::Unknown,
                AgentStatus::Idle,
                AgentStatus::Working,
                AgentStatus::Done,
                AgentStatus::Blocked,
            ]),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            rollup_agent_statuses([AgentStatus::Working, AgentStatus::Done]),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn detector_reads_strong_osc_and_screen_signals() {
        assert_eq!(
            detect_agent_status_from_signals(None, Some("Codex Action Required")),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            detect_agent_status_from_signals(None, Some("Codex ⠋ working")),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            detect_agent_status_from_signals(Some("Do you want to approve this command?"), None),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            detect_agent_status_from_signals(Some("bypass permissions on"), None),
            None
        );
        assert_eq!(
            detect_agent_status_from_signals(Some("Auto-approve all enabled (Shift+Tab)"), None),
            None
        );
    }

    #[test]
    fn detector_reads_agent_name_from_osc_and_screen_signals() {
        assert_eq!(
            detect_agent_name_from_signals(None, Some("OpenCode Action Required")),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Claude is thinking"), Some("OC | greeting")),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_name_from_signals(None, Some("Claude")),
            Some("claude")
        );
        assert_eq!(
            detect_agent_name_from_signals(None, Some("Codex")),
            Some("codex")
        );
        assert_eq!(
            detect_agent_name_from_signals(None, Some("Cline")),
            Some("cline")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Claude is thinking"), None),
            Some("claude")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Codex working"), None),
            Some("codex")
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("Cline needs approval"), None),
            Some("cline")
        );
        assert_eq!(detect_agent_name_from_signals(Some("decline"), None), None);
        assert_eq!(
            detect_agent_name_from_signals(
                Some(
                    "Which agents should CodeGraph configure?\n\
                     Claude Code (detected), Codex CLI (detected), opencode (detected)\n\
                     Do you want to continue?"
                ),
                None
            ),
            None
        );
        assert_eq!(
            detect_agent_name_from_signals(Some("OpenCode needs input"), None),
            Some("opencode")
        );
    }

    #[test]
    fn detector_reads_idle_agent_prompt_without_trusting_stale_scrollback() {
        assert_eq!(
            detect_agent_idle_name_from_signals(
                Some("Codex\npress / for commands\n\n\n\n\n\n\n\n\n\n\n\n"),
                None
            ),
            Some("codex")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(
                Some(
                    "Ask anything... \"Fix broken tests\"\n\
                     Sisyphus - Ultraworker · GPT-5.5 OpenAI · medium\n\
                     tab agents  ctrl+p commands"
                ),
                None
            ),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("$ echo shell ready"), Some("OpenCode")),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("   \n\n"), Some("Claude")),
            Some("claude")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("$ echo shell ready"), Some("Claude")),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(None, Some("OpenCode")),
            Some("opencode")
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(Some("codex exited\n$ echo done"), None),
            None
        );
        assert_eq!(
            detect_agent_idle_name_from_signals(
                Some(r#"printf "Codex\\npress / for commands\\n""#),
                None
            ),
            None
        );
    }

    #[test]
    fn normalize_keeps_user_renamed_titles() {
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "my pinned shell".into(),
            title_locked: true,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: Some("/tmp".into()),
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(!changed);
        assert_eq!(surface.title, "my pinned shell");
        assert!(surface.title_locked);
    }

    #[test]
    fn normalize_keeps_already_cwd_matching_titles() {
        let cwd = std::path::PathBuf::from("/home/u/dev/os/flowmux");
        let derived = terminal_tab_title_for_cwd(Some(&cwd));
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: derived.clone(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: Some(cwd),
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(!changed, "no-op when title already matches cwd");
        assert!(!surface.title_locked);
    }

    #[test]
    fn normalize_skips_browser_surfaces() {
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "Page Title".into(),
            title_locked: false,
            kind: SurfaceKind::Browser { initial_url: None },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(!changed);
        assert_eq!(surface.title, "Page Title");
    }

    #[test]
    fn normalize_falls_back_to_default_when_cwd_is_missing() {
        let mut surface = PaneSurface {
            id: SurfaceId::new(),
            title: "claude".into(),
            title_locked: false,
            kind: SurfaceKind::Terminal {
                shell: None,
                cwd: None,
            },
            agent: None,
        };
        let changed = normalize_unlocked_terminal_title(&mut surface);
        assert!(changed);
        assert_eq!(surface.title, FALLBACK_TERMINAL_TAB_TITLE);
    }

    // ---- tab move (take / insert) ----

    fn leaf_with_tabs(pane_id: PaneId, titles: &[&str]) -> (Pane, Vec<SurfaceId>) {
        let surfaces: Vec<PaneSurface> = titles
            .iter()
            .map(|t| PaneSurface::terminal(*t, None))
            .collect();
        let ids: Vec<SurfaceId> = surfaces.iter().map(|s| s.id).collect();
        let active = ids[0];
        (
            Pane::Leaf {
                id: pane_id,
                content: PaneContent::Tabs { active, surfaces },
            },
            ids,
        )
    }

    fn leaf_tab_ids(pane: &Pane, target: PaneId) -> Vec<SurfaceId> {
        match pane.find_leaf_content(target) {
            Some(PaneContent::Tabs { surfaces, .. }) => surfaces.iter().map(|s| s.id).collect(),
            _ => panic!("expected tabs leaf"),
        }
    }

    #[test]
    fn take_surface_removes_middle_tab_and_keeps_leaf() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b", "c"]);
        let (taken, empty) = p.take_surface_from_leaf(pane, ids[1]).expect("taken");
        assert_eq!(taken.id, ids[1]);
        assert!(!empty);
        assert_eq!(leaf_tab_ids(&p, pane), vec![ids[0], ids[2]]);
    }

    #[test]
    fn take_surface_reports_empty_when_last_tab_removed() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["only"]);
        let (taken, empty) = p.take_surface_from_leaf(pane, ids[0]).expect("taken");
        assert_eq!(taken.id, ids[0]);
        assert!(empty);
    }

    #[test]
    fn take_surface_reactivates_neighbor_when_active_removed() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b", "c"]);
        // active is ids[0]; remove it
        let (_taken, empty) = p.take_surface_from_leaf(pane, ids[0]).expect("taken");
        assert!(!empty);
        match p.find_leaf_content(pane) {
            Some(PaneContent::Tabs { active, surfaces }) => {
                assert!(surfaces.iter().any(|s| s.id == active));
                assert_ne!(active, ids[0]);
            }
            _ => panic!("expected tabs"),
        }
    }

    #[test]
    fn take_surface_not_found_returns_none() {
        let pane = PaneId::new();
        let (mut p, _ids) = leaf_with_tabs(pane, &["a"]);
        assert!(p.take_surface_from_leaf(pane, SurfaceId::new()).is_none());
    }

    #[test]
    fn insert_surface_at_index_sets_active() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b", "c"]);
        let moved = PaneSurface::terminal("moved", None);
        let moved_id = moved.id;
        let got = p
            .insert_surface_into_leaf(pane, moved, 1)
            .expect("inserted");
        assert_eq!(got, moved_id);
        assert_eq!(
            leaf_tab_ids(&p, pane),
            vec![ids[0], moved_id, ids[1], ids[2]]
        );
        match p.find_leaf_content(pane) {
            Some(PaneContent::Tabs { active, .. }) => assert_eq!(active, moved_id),
            _ => panic!("expected tabs"),
        }
    }

    #[test]
    fn insert_surface_clamps_index_to_end() {
        let pane = PaneId::new();
        let (mut p, ids) = leaf_with_tabs(pane, &["a", "b"]);
        let moved = PaneSurface::terminal("moved", None);
        let moved_id = moved.id;
        p.insert_surface_into_leaf(pane, moved, 999)
            .expect("inserted");
        assert_eq!(leaf_tab_ids(&p, pane), vec![ids[0], ids[1], moved_id]);
    }

    #[test]
    fn insert_surface_into_missing_leaf_returns_none() {
        let pane = PaneId::new();
        let (mut p, _ids) = leaf_with_tabs(pane, &["a"]);
        let moved = PaneSurface::terminal("moved", None);
        assert!(p
            .insert_surface_into_leaf(PaneId::new(), moved, 0)
            .is_none());
    }

    #[test]
    fn take_then_insert_moves_between_leaves_in_split() {
        let l1 = PaneId::new();
        let l2 = PaneId::new();
        let (left, left_ids) = leaf_with_tabs(l1, &["a", "b"]);
        let (right, _right_ids) = leaf_with_tabs(l2, &["x"]);
        let mut p = Pane::Split {
            id: PaneId::new(),
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(left),
            second: Box::new(right),
        };
        let (taken, empty) = p.take_surface_from_leaf(l1, left_ids[1]).expect("taken");
        assert!(!empty);
        let moved_id = taken.id;
        p.insert_surface_into_leaf(l2, taken, usize::MAX)
            .expect("inserted");
        assert_eq!(leaf_tab_ids(&p, l1), vec![left_ids[0]]);
        let right_ids = leaf_tab_ids(&p, l2);
        assert_eq!(right_ids.last().copied(), Some(moved_id));
        assert_eq!(right_ids.len(), 2);
    }
}
