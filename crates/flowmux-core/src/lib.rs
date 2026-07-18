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
/// Maximum terminal scrollback persisted per tab. The cap keeps `state.json`
/// bounded even when VTE is configured with a much larger live scrollback.
pub const TERMINAL_SCROLLBACK_MAX_BYTES: usize = 256 * 1024;

/// Keep the newest complete UTF-8 suffix that fits the persisted scrollback
/// budget. Terminal history is useful from the tail; truncating from the front
/// also preserves the current prompt and most recent agent output.
pub fn bound_terminal_scrollback(text: &str) -> String {
    if text.len() <= TERMINAL_SCROLLBACK_MAX_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - TERMINAL_SCROLLBACK_MAX_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

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

    pub fn agent_attention_rollup(&self) -> Option<AgentStatus> {
        rollup_agent_statuses(
            self.surfaces
                .iter()
                .filter_map(|surface| surface.root_pane.agent_attention_rollup()),
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EditorSessionState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_files: Vec<EditorFileState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditorFileState {
    pub path: PathBuf,
    #[serde(default)]
    pub cursor_line: u32,
    #[serde(default)]
    pub cursor_column: u32,
    #[serde(default)]
    pub scroll_top: f64,
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
    Editor {
        workspace_root: PathBuf,
        #[serde(default)]
        session: EditorSessionState,
    },
}

/// A tab inside a leaf pane. cmux calls these surfaces: each pane can
/// host multiple terminal/browser/editor surfaces, with exactly one active at
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
    /// Best-effort plain-text terminal history replayed on the next launch.
    /// Bounded by [`TERMINAL_SCROLLBACK_MAX_BYTES`] before it reaches state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
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

    pub fn is_active_surface(&self, surface_id: SurfaceId) -> bool {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { active, .. },
                ..
            } => *active == surface_id,
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.is_active_surface(surface_id) || second.is_active_surface(surface_id)
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
        let Some(surface) = self.find_surface_mut(target, surface_id) else {
            return false;
        };
        surface.title = title;
        surface.title_locked = true;
        true
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

    /// Merge a live agent report into the matching tab surface. `surface_visible`
    /// is true only when the app window, containing pane, and surface are focused.
    pub fn report_surface_agent(
        &mut self,
        surface_id: SurfaceId,
        report: AgentStatusReport,
        surface_visible: bool,
    ) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { active, surfaces },
                ..
            } => {
                let visible = surface_visible && *active == surface_id;
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
                .report_surface_agent(surface_id, report.clone(), surface_visible)
                .or_else(|| second.report_surface_agent(surface_id, report, surface_visible)),
        }
    }

    pub fn report_surface_agent_signal(
        &mut self,
        surface_id: SurfaceId,
        status: AgentStatus,
        source: &'static str,
        agent_name: Option<&str>,
        surface_visible: bool,
    ) -> Option<bool> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { active, surfaces },
                ..
            } => {
                let visible = surface_visible && *active == surface_id;
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
                    surface_visible,
                )
                .or_else(|| {
                    second.report_surface_agent_signal(
                        surface_id,
                        status,
                        source,
                        agent_name,
                        surface_visible,
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
                Some(reconcile_surface_process_agent(
                    &mut surface.agent,
                    detected,
                ))
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
    pub fn settle_screen_idle(
        &mut self,
        surface_id: SurfaceId,
        surface_visible: bool,
    ) -> Option<bool> {
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
                        let prev = agent.status;
                        agent.status = AgentStatus::Idle;
                        agent.activity = AgentActivity::Idle;
                        agent.message = None;
                        agent.custom_status = None;
                        if AgentStatus::should_mark_unseen_on_idle(prev, AgentStatus::Idle) {
                            agent.seen = surface_visible;
                        } else if surface_visible {
                            agent.seen = true;
                        }
                        Some(true)
                    }
                    _ => Some(false),
                }
            }
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => first
                .settle_screen_idle(surface_id, surface_visible)
                .or_else(|| second.settle_screen_idle(surface_id, surface_visible)),
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

    /// Mark every agent in this pane tree as acknowledged. Workspace activation
    /// is the user's workspace-wide acknowledgement gesture, including agents
    /// living in inactive tabs.
    pub fn mark_all_agents_seen(&mut self) -> bool {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces
                .iter_mut()
                .filter_map(|surface| surface.agent.as_mut())
                .fold(false, |changed, agent| agent.mark_seen() | changed),
            Pane::Leaf { .. } => false,
            Pane::Split { first, second, .. } => {
                first.mark_all_agents_seen() | second.mark_all_agents_seen()
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

    pub fn agent_attention_rollup(&self) -> Option<AgentStatus> {
        match self {
            Pane::Leaf {
                content: PaneContent::Tabs { surfaces, .. },
                ..
            } => rollup_agent_statuses(surfaces.iter().filter_map(|surface| {
                let agent = surface.agent.as_ref()?;
                if agent.seen {
                    return None;
                }
                match agent.public_status() {
                    AgentStatus::Blocked | AgentStatus::Done => Some(agent.public_status()),
                    _ => None,
                }
            })),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => rollup_agent_statuses(
                first
                    .agent_attention_rollup()
                    .into_iter()
                    .chain(second.agent_attention_rollup()),
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
                    let cwd = match &surface.kind {
                        SurfaceKind::Terminal { cwd: Some(cwd), .. } => {
                            Some(cwd.display().to_string())
                        }
                        SurfaceKind::Terminal { cwd: None, .. }
                        | SurfaceKind::Browser { .. }
                        | SurfaceKind::Editor { .. } => None,
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
        let Some(surface) = self.find_surface_mut(target, surface_id) else {
            return false;
        };
        let SurfaceKind::Browser { initial_url } = &mut surface.kind else {
            return false;
        };
        if initial_url.as_deref() == Some(new_url.as_str()) {
            return false;
        }
        *initial_url = Some(new_url);
        true
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
        let Some(surface) = self.find_surface_mut(target, surface_id) else {
            return false;
        };
        if surface.title_locked || surface.title == new_title {
            return false;
        }
        // The `$HOME` lookup runs only after the surface is found and only for a
        // Terminal with cwd set, so calls that miss the leaf pay no env access.
        if let SurfaceKind::Terminal { cwd: Some(cwd), .. } = &surface.kind {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            if title_is_shell_cwd_echo(&new_title, cwd, home.as_deref()) {
                return false;
            }
        }
        surface.title = new_title;
        true
    }

    pub fn set_surface_cwd(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        new_cwd: PathBuf,
    ) -> bool {
        let Some(surface) = self.find_surface_mut(target, surface_id) else {
            return false;
        };
        let SurfaceKind::Terminal { cwd, .. } = &mut surface.kind else {
            return false;
        };
        if cwd.as_ref() == Some(&new_cwd) {
            return false;
        }
        *cwd = Some(new_cwd);
        if !surface.title_locked {
            let next_title = terminal_tab_title_for_cwd(cwd.as_deref());
            if surface.title != next_title {
                surface.title = next_title;
            }
        }
        true
    }

    /// Store bounded terminal history for a tab. Browser tabs reject the
    /// update, and identical snapshots are no-ops so periodic capture does not
    /// cause needless state writes.
    pub fn set_surface_scrollback(
        &mut self,
        target: PaneId,
        surface_id: SurfaceId,
        text: String,
    ) -> bool {
        let Some(surface) = self.find_surface_mut(target, surface_id) else {
            return false;
        };
        if !matches!(surface.kind, SurfaceKind::Terminal { .. }) {
            return false;
        }
        let bounded = bound_terminal_scrollback(&text);
        let next = (!bounded.is_empty()).then_some(bounded);
        if surface.scrollback == next {
            return false;
        }
        surface.scrollback = next;
        true
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

    /// Mutable sibling of [`Self::find_surface`]: locate the surface `target`
    /// (a tab of pane `target_pane`) for in-place mutation, walking into split
    /// branches. Returns `None` when the pane is absent, is not a `Tabs` leaf,
    /// or holds no surface with that id. Centralizing this descent lets the
    /// payload setters (rename / url / title / cwd) mutate the found surface
    /// directly instead of each re-implementing the tree walk.
    fn find_surface_mut(
        &mut self,
        target_pane: PaneId,
        target: SurfaceId,
    ) -> Option<&mut PaneSurface> {
        match self {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { surfaces, .. },
            } if *id == target_pane => surfaces.iter_mut().find(|s| s.id == target),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => {
                if let Some(found) = first.find_surface_mut(target_pane, target) {
                    return Some(found);
                }
                second.find_surface_mut(target_pane, target)
            }
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
            scrollback: None,
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
            scrollback: None,
            agent: None,
        }
    }

    pub fn editor(title: impl Into<String>, workspace_root: PathBuf) -> Self {
        Self {
            id: SurfaceId::new(),
            title: title.into(),
            title_locked: false,
            kind: SurfaceKind::Editor {
                workspace_root,
                session: EditorSessionState::default(),
            },
            scrollback: None,
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

    pub fn tabbed_editor(title: impl Into<String>, workspace_root: PathBuf) -> Self {
        let surface = PaneSurface::editor(title, workspace_root);
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
    /// cwd wins. If the active surface is a browser or editor, walks earlier tabs and
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
            SurfaceKind::Browser { .. } | SurfaceKind::Editor { .. } => surfaces[..active_idx]
                .iter()
                .rev()
                .find_map(|surface| match &surface.kind {
                    SurfaceKind::Terminal { cwd: Some(cwd), .. } => Some(cwd.clone()),
                    _ => None,
                }),
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
    /// An agent turn finished. The agent's `Done` state carries the
    /// acknowledgement affordance when the source was not visible.
    TurnCompleted,
    /// Agent is blocked waiting for user input or approval.
    NeedsInput,
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
        matches!(
            prev,
            AgentStatus::Unknown | AgentStatus::Working | AgentStatus::Blocked
        ) && next == AgentStatus::Idle
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
    Unknown,
    Working,
    Waiting,
    Done,
}

pub fn agent_bar_visual_status(status: AgentStatus) -> Option<AgentBarVisualStatus> {
    match status {
        AgentStatus::Working => Some(AgentBarVisualStatus::Working),
        AgentStatus::Idle | AgentStatus::Blocked => Some(AgentBarVisualStatus::Waiting),
        AgentStatus::Done => Some(AgentBarVisualStatus::Done),
        AgentStatus::Unknown => Some(AgentBarVisualStatus::Unknown),
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

/// flowmux workspace / agent-bar color palette. Harmonious hues at a fixed
/// saturation and lightness, spaced around the wheel and deliberately kept
/// clear of the pale-yellow band reserved for the focus border
/// (`FOCUS_BORDER_COLOR_DEFAULT`), so color-bar stripes read as distinct
/// against the dark sidebar tint while sharing one visual family. Entries are
/// lowercase 7-char hex; keep them perceptually spaced when editing.
pub const WORKSPACE_PALETTE: &[&str] = &[
    "#dc7b74", "#dc9a74", "#a5dc74", "#82dc74", "#74dc8e", "#74dcba", "#74d5dc", "#74b1dc",
    "#7489dc", "#8e74dc", "#c274dc", "#dc74c8", "#dc749a",
];

/// Choose a workspace color from [`WORKSPACE_PALETTE`]. Randomized by `seed`
/// (pass the workspace UUID's low bits) yet biased away from colors already in
/// `used`: while free palette slots remain every workspace gets a distinct
/// color, and once the palette is exhausted reuse spreads evenly instead of
/// clustering. Deterministic in `(used, seed)`, so the color chosen at creation
/// stays stable after it is persisted.
pub fn pick_workspace_color(used: &[String], seed: u128) -> String {
    let mut counts = vec![0usize; WORKSPACE_PALETTE.len()];
    for u in used {
        if let Some(i) = WORKSPACE_PALETTE
            .iter()
            .position(|c| c.eq_ignore_ascii_case(u))
        {
            counts[i] += 1;
        }
    }
    let min = counts.iter().copied().min().unwrap_or(0);
    // Least-used palette entries are the candidates; `seed` scatters the pick
    // among them so the choice stays random without ever landing on a color a
    // sibling workspace already wears (until every color is spoken for).
    let candidates: Vec<usize> = (0..WORKSPACE_PALETTE.len())
        .filter(|&i| counts[i] == min)
        .collect();
    let pick = candidates[(seed % candidates.len() as u128) as usize];
    WORKSPACE_PALETTE[pick].to_string()
}

/// Fallback stripe color for an agent-bar item whose workspace has no color
/// set. Hashes the surface id into [`WORKSPACE_PALETTE`] so it is deterministic
/// per surface and stays within the flowmux color family.
pub fn agent_bar_color_for_surface(surface: SurfaceId) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in surface.0.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let idx = (hash % WORKSPACE_PALETTE.len() as u64) as usize;
    WORKSPACE_PALETTE[idx].to_string()
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

    pub fn from_report(report: AgentStatusReport, visible: bool) -> Option<Self> {
        let status = report.effective_status()?;
        let mut presence = Self::new(report.name, status.to_activity(), report.pid);
        presence.status = status;
        presence.source = report.source;
        presence.seq = report.seq;
        presence.message = report.message;
        presence.custom_status = report.custom_status;
        presence.session_id = report.session_id;
        // Initial idle/unknown/working reports establish presence without an
        // alert. A blocking report is unacknowledged only when its source is not
        // actually visible.
        presence.seen = status != AgentStatus::Blocked || visible;
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
        let needs_acknowledgement = next == AgentStatus::Blocked
            || AgentStatus::should_mark_unseen_on_idle(prev, next)
            || (next == AgentStatus::Idle && prev == AgentStatus::Idle && !self.seen);
        self.seen = !needs_acknowledgement || visible;
        true
    }

    pub fn mark_seen(&mut self) -> bool {
        let changed = !self.seen;
        self.seen = true;
        changed
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
    if contains_ascii_token(text, "opencode") || contains_ascii_phrase(text, "open code") {
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
    if starts_with_agent_title_token(&lower, "opencode")
        || starts_with_agent_title_token(&lower, "open code")
        || lower.starts_with("oc |")
        || lower.starts_with("oc|")
    {
        Some("opencode")
    } else if starts_with_agent_title_token(&lower, "claude") {
        Some("claude")
    } else if starts_with_agent_title_token(&lower, "codex") {
        Some("codex")
    } else if starts_with_agent_title_token(&lower, "cline") {
        Some("cline")
    } else {
        None
    }
}

fn starts_with_agent_title_token(title: &str, token: &str) -> bool {
    let Some(rest) = title.strip_prefix(token) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some('|' | ':') => true,
        Some(ch) => ch.is_whitespace(),
    }
}

fn contains_ascii_token(haystack: &str, needle: &str) -> bool {
    contains_ascii_phrase(haystack, needle)
}

fn contains_ascii_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(idx, _)| {
        let before = haystack[..idx].chars().next_back();
        let after = haystack[idx + needle.len()..].chars().next();
        is_agent_name_boundary(before) && is_agent_name_boundary(after)
    })
}

fn is_agent_name_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
    }
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
#[path = "lib_tests.rs"]
mod tests;
