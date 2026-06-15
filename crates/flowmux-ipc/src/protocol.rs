// SPDX-License-Identifier: GPL-3.0-or-later
use flowmux_core::{
    AgentActivity, NotificationLevel, Pane, PaneContent, PaneId, PlacementStrategy, SplitDirection,
    SurfaceId, SurfaceKind, Workspace, WorkspaceId,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Browser verbs exposed by `flowmux browser <op>`, in the order shown
/// by `flowmux capabilities`. This is the authoritative list the docs
/// and the CLI namespace both track.
pub const BROWSER_VERBS: &[&str] = &[
    "open",
    "snapshot",
    "navigate",
    "back",
    "forward",
    "reload",
    "url",
    "title",
    "click",
    "fill",
    "select",
    "scroll",
    "type",
    "press",
    "text",
    "value",
    "attr",
    "dblclick",
    "hover",
    "focus",
    "blur",
    "check",
    "uncheck",
    "is-visible",
    "is-enabled",
    "is-checked",
    "count",
    "eval",
];

/// Browser features that are intentionally unsupported and return a
/// `not_supported` style result. They require the Chrome DevTools
/// Protocol, which WebKitGTK 6.0 does not expose (see `AGENTS.md`).
pub const UNSUPPORTED_FEATURES: &[&str] = &[
    "screenshot",
    "wait",
    "viewport",
    "network-mock",
    "screencast",
];

/// Static description of what this flowmux build can and cannot do,
/// returned by `flowmux capabilities`. Lets an agent probe the browser
/// verb set and the explicitly-unsupported features before attempting
/// them, instead of discovering gaps by trial and error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub browser_verbs: Vec<String>,
    pub unsupported: Vec<String>,
}

/// Build the capability descriptor from the canonical static lists.
pub fn capabilities() -> Capabilities {
    Capabilities {
        browser_verbs: BROWSER_VERBS.iter().map(|s| s.to_string()).collect(),
        unsupported: UNSUPPORTED_FEATURES.iter().map(|s| s.to_string()).collect(),
    }
}

/// A single tab inside a leaf pane, as reported by `flowmux tree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeTab {
    pub id: SurfaceId,
    pub title: String,
    /// `"terminal"` or `"browser"`.
    pub kind: String,
    /// True for the tab currently shown in this pane.
    pub active: bool,
}

/// A leaf pane (one slot in the split grid) and its tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreePane {
    pub id: PaneId,
    pub tabs: Vec<TreeTab>,
}

/// A workspace flattened to its leaf panes for agent inspection. The
/// split hierarchy is intentionally collapsed: agents target panes by
/// id (for browser/terminal verbs), not by split position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub root: PathBuf,
    pub panes: Vec<TreePane>,
}

fn surface_kind_label(kind: &SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Terminal { .. } => "terminal",
        SurfaceKind::Browser { .. } => "browser",
    }
}

fn collect_leaf_panes(pane: &Pane, out: &mut Vec<TreePane>) {
    match pane {
        Pane::Leaf { id, content } => {
            let tabs = match content {
                PaneContent::Tabs { active, surfaces } => surfaces
                    .iter()
                    .map(|s| TreeTab {
                        id: s.id,
                        title: s.title.clone(),
                        kind: surface_kind_label(&s.kind).to_string(),
                        active: s.id == *active,
                    })
                    .collect(),
                // Legacy content shapes are normalized into `Tabs` on
                // load (see flowmux-daemon normalize_state), so these
                // are unreachable in practice; report no tabs rather
                // than fabricate ids.
                PaneContent::Terminal { .. } | PaneContent::Browser { .. } => Vec::new(),
            };
            out.push(TreePane { id: *id, tabs });
        }
        Pane::Split { first, second, .. } => {
            collect_leaf_panes(first, out);
            collect_leaf_panes(second, out);
        }
    }
}

/// Flatten live workspace state into the `flowmux tree` view. Pure
/// function over the persisted domain types so it is fully unit
/// testable without a daemon or GTK.
pub fn describe_workspaces(workspaces: &[Workspace]) -> Vec<TreeWorkspace> {
    workspaces
        .iter()
        .map(|w| {
            let mut panes = Vec::new();
            for surface in &w.surfaces {
                collect_leaf_panes(&surface.root_pane, &mut panes);
            }
            TreeWorkspace {
                id: w.id,
                name: w.display_title().to_string(),
                root: w.root_dir.clone(),
                panes,
            }
        })
        .collect()
}

/// One frame on the wire. `id` correlates a request with its response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    Request(Request),
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Request {
    /// Daemon health probe.
    Ping,

    /// `flowmux workspace new --root .`
    WorkspaceCreate {
        name: Option<String>,
        root: PathBuf,
    },

    /// `flowmux workspace ls`
    WorkspaceList,

    /// `flowmux tree` — full workspace → leaf-pane → tab inspection.
    WorkspaceTree,

    /// `flowmux workspace current` — the most-recently-activated
    /// (currently focused) workspace id, if any.
    WorkspaceCurrent,

    /// `flowmux workspace focus <id>` — make a workspace the active one
    /// (same operation as clicking its sidebar row). Reversible; does
    /// not create or destroy anything.
    WorkspaceFocus {
        workspace: WorkspaceId,
    },

    /// `flowmux surface new <workspace>` — opens a new surface (tab).
    SurfaceCreate {
        workspace: WorkspaceId,
        cwd: Option<PathBuf>,
    },

    /// `flowmux pane split <pane> --right|--down`
    PaneSplit {
        pane: PaneId,
        direction: SplitDirection,
    },

    /// `flowmux pane send-keys <pane> "<keys>"`
    PaneSendKeys {
        pane: PaneId,
        keys: String,
    },

    /// `flowmux read-screen <pane>` — plain-text dump of a terminal
    /// pane's buffer. Read-only.
    PaneReadScreen {
        pane: PaneId,
    },

    /// `flowmux focus-pane <pane>` — grab keyboard focus for a pane.
    /// Non-destructive.
    PaneFocus {
        pane: PaneId,
    },

    /// `flowmux close-pane <pane>` — close a pane. Refuses (without a
    /// dialog) when it is the workspace's last pane, so an agent's call
    /// never blocks on a GUI confirmation.
    PaneClose {
        pane: PaneId,
    },

    /// `flowmux notify --pane <id> --title ... --body ...`
    Notify {
        pane: Option<PaneId>,
        /// Specific tab surface inside `pane` that triggered the
        /// notification. Lets the GUI route a click back to the right
        /// tab even when the leaf pane has many. `None` for global
        /// toasts that don't belong to any one tab.
        #[serde(default)]
        surface: Option<SurfaceId>,
        title: String,
        body: String,
        level: NotificationLevel,
    },

    /// `flowmux ssh user@host` — open a remote workspace.
    SshConnect {
        target: String,
    },

    /// `flowmux browser open <url> [--right|--down]` — split a target
    /// terminal/browser pane and put a browser pane in the new
    /// sibling. `target_pane = None` means "use the currently
    /// focused pane"; the daemon resolves it on the GTK side.
    BrowserOpen {
        url: String,
        target_pane: Option<PaneId>,
        direction: SplitDirection,
    },

    // ---- per-pane controller verbs (cmux-style scriptable browser).
    // Each maps 1:1 onto flowmux_browser::BrowserController.
    BrowserNavigate {
        pane: PaneId,
        url: String,
    },
    BrowserBack {
        pane: PaneId,
    },
    BrowserForward {
        pane: PaneId,
    },
    BrowserReload {
        pane: PaneId,
    },
    BrowserUrl {
        pane: PaneId,
    },
    BrowserTitle {
        pane: PaneId,
    },
    BrowserClick {
        pane: PaneId,
        target: String,
    },
    BrowserFill {
        pane: PaneId,
        target: String,
        value: String,
    },
    BrowserSelect {
        pane: PaneId,
        target: String,
        value: String,
    },
    BrowserScroll {
        pane: PaneId,
        target: String,
        x: i32,
        y: i32,
    },
    BrowserType {
        pane: PaneId,
        text: String,
    },
    BrowserPress {
        pane: PaneId,
        key: String,
    },
    BrowserText {
        pane: PaneId,
        target: String,
    },
    BrowserValue {
        pane: PaneId,
        target: String,
    },
    BrowserAttr {
        pane: PaneId,
        target: String,
        name: String,
    },

    // ---- Phase 5 P0 action gap: cmux-equivalent verbs that round
    // out the agent-browser surface. Selectors come from the most
    // recent `BrowserSnapshot` (resolved via the daemon's RefStore).
    BrowserDblClick {
        pane: PaneId,
        target: String,
    },
    BrowserHover {
        pane: PaneId,
        target: String,
    },
    BrowserFocus {
        pane: PaneId,
        target: String,
    },
    BrowserBlur {
        pane: PaneId,
        target: String,
    },
    BrowserCheck {
        pane: PaneId,
        target: String,
    },
    BrowserUncheck {
        pane: PaneId,
        target: String,
    },
    BrowserIsVisible {
        pane: PaneId,
        target: String,
    },
    BrowserIsEnabled {
        pane: PaneId,
        target: String,
    },
    BrowserIsChecked {
        pane: PaneId,
        target: String,
    },
    BrowserCount {
        pane: PaneId,
        selector: String,
    },

    // ---- Phase 7: agent session resume mapping ----
    /// Record (or overwrite) the session id an agent (claude/codex/…)
    /// is currently using inside `surface`. Persisted at
    /// `$XDG_DATA_HOME/flowmux/agent-sessions/<agent>.json` so the next
    /// app launch can `<agent> --resume <session_id>` in the same
    /// surface. Mirrors cmux's hook → `~/.cmuxterm/<agent>-hook-sessions.json`
    /// flow.
    AgentSessionUpdate {
        agent: String,
        surface: SurfaceId,
        session_id: String,
    },
    /// Look up the session id previously recorded for `(agent,
    /// surface)`. Response carries `Option<String>`.
    AgentSessionGet {
        agent: String,
        surface: SurfaceId,
    },
    /// Forget the recorded session for `(agent, surface)` — used when
    /// the surface is closed for good.
    AgentSessionForget {
        agent: String,
        surface: SurfaceId,
    },

    /// Report a change in an AI agent's live activity inside a surface,
    /// emitted by the agent's lifecycle hooks (`flowmux hooks <agent>
    /// <event>`). `activity: None` clears the presence (session end).
    /// `pid` is the agent process id from the wrapper shim, used by the
    /// daemon's liveness sweep to clear presences left stale by a hard
    /// kill where `SessionEnd` never fired. Runtime-only; never
    /// persisted. Falls back to `FLOWMUX_PANE_ID` / `FLOWMUX_SURFACE_ID`
    /// when `pane` / `surface` are omitted.
    AgentActivityUpdate {
        #[serde(default)]
        pane: Option<PaneId>,
        #[serde(default)]
        surface: Option<SurfaceId>,
        agent: String,
        #[serde(default)]
        activity: Option<AgentActivity>,
        #[serde(default)]
        pid: Option<u32>,
    },

    /// `flowmux claude-teams [--count N] [-- args...]` — spin up a
    /// workspace with N panes, each running the `claude` CLI with the
    /// given args. Mirrors cmux's documented "claude-teams" launcher.
    ClaudeTeams {
        count: u8,
        args: Vec<String>,
        root: std::path::PathBuf,
    },

    /// `flowmux browser snapshot --pane <id>` — return a JSON snapshot
    /// of the page DOM/a11y tree (for agent automation).
    BrowserSnapshot {
        pane: PaneId,
    },

    /// `flowmux browser eval --pane <id> <js>` — evaluate JS, return result.
    BrowserEval {
        pane: PaneId,
        source: String,
    },

    /// `flowmux import-cookies --from firefox [--domain example.com]`
    /// Imports cookies from a host browser into the in-app browser's
    /// cookie jar. Chromium-family browsers return Unimplemented until
    /// libsecret-backed value unwrapping lands.
    ImportCookies {
        source: String,
        domain: Option<String>,
    },

    /// Close (withdraw) a desktop notification previously emitted by
    /// flowmux. The `desktop_id` is the string id we passed to
    /// `org.gtk.Notifications.AddNotification` — flowmux echoes it back
    /// via `Response::Notified` so the GUI can later ask the daemon to
    /// withdraw the entry once the user reads it through the in-app
    /// bell popover. Without this, `AttentionNeeded` toasts accumulate
    /// in the GNOME message tray and the dock badge stays pinned.
    CloseDesktopNotification {
        desktop_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong,
    WorkspaceCreated {
        id: WorkspaceId,
    },
    WorkspaceList {
        ids: Vec<WorkspaceId>,
    },
    /// Reply to `WorkspaceTree`.
    Tree {
        workspaces: Vec<TreeWorkspace>,
    },
    /// Reply to `WorkspaceCurrent`. `id = None` when no workspace has
    /// been activated yet (e.g. an empty just-launched window).
    WorkspaceCurrent {
        id: Option<WorkspaceId>,
    },
    /// Reply to `PaneReadScreen` — the terminal buffer text.
    ScreenContents {
        text: String,
    },
    SurfaceCreated {
        id: SurfaceId,
    },
    PaneSplitDone {
        new_pane: PaneId,
    },
    BrowserResult {
        value: String,
    },
    BrowserOk,
    BrowserBoolResult {
        value: bool,
    },
    /// Reply to `BrowserOpen`. `placement_strategy` mirrors cmux's
    /// response field so agents can tell whether their URL was added
    /// as a tab to an existing right-sibling browser pane or whether
    /// the daemon created a fresh split for it.
    BrowserPaneOpened {
        pane: PaneId,
        placement_strategy: PlacementStrategy,
    },
    CookiesImported {
        count: usize,
    },
    /// Reply to `AgentSessionGet`. `session_id = None` means no
    /// previous session was recorded for this `(agent, surface)`.
    AgentSession {
        session_id: Option<String>,
    },
    /// Reply to `Request::Notify`. Carries the `org.gtk.Notifications`
    /// id the daemon assigned, so the GUI can later issue
    /// `Request::CloseDesktopNotification` to drop the badge once the
    /// user reads it inside flowmux. `None` means no desktop toast was
    /// actually sent (the notifications daemon was unreachable, or the
    /// toast was suppressed).
    Notified {
        desktop_id: Option<String>,
    },
    Error(RpcError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
pub enum RpcError {
    Unimplemented(String),
    NotFound(String),
    InvalidArgument(String),
    Io(String),
    Internal(String),
}

/// Async events pushed from daemon to subscribers (e.g. notification toast).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    NotificationRaised {
        workspace: WorkspaceId,
        body: String,
        level: NotificationLevel,
    },
    PortListening {
        workspace: WorkspaceId,
        port: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_uses_tagged_newline_json_shape() {
        let pane = PaneId::new();
        let env = Envelope {
            id: 42,
            payload: Payload::Request(Request::PaneSplit {
                pane,
                direction: SplitDirection::Vertical,
            }),
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        assert_eq!(value["id"], 42);
        assert_eq!(value["kind"], "request");
        assert_eq!(value["verb"], "pane_split");
        assert_eq!(value["pane"], pane.to_string());
        assert_eq!(value["direction"], "vertical");
    }

    #[test]
    fn describe_workspaces_flattens_panes_and_marks_active_tab() {
        use flowmux_core::{
            Pane, PaneContent, PaneSurface, Surface, SurfaceKind, Workspace, WorkspaceId,
        };
        let term = || SurfaceKind::Terminal {
            shell: None,
            cwd: None,
        };
        let tab1 = SurfaceId::new();
        let tab2 = SurfaceId::new();
        let leaf_id = PaneId::new();
        let leaf = Pane::Leaf {
            id: leaf_id,
            content: PaneContent::Tabs {
                active: tab2,
                surfaces: vec![
                    PaneSurface {
                        id: tab1,
                        title: "shell".into(),
                        title_locked: false,
                        kind: term(),
                        agent: None,
                    },
                    PaneSurface {
                        id: tab2,
                        title: "docs".into(),
                        title_locked: false,
                        kind: SurfaceKind::Browser { initial_url: None },
                        agent: None,
                    },
                ],
            },
        };
        let ws = Workspace {
            id: WorkspaceId::new(),
            name: "demo".into(),
            custom_title: None,
            root_dir: "/tmp/demo".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: term(),
                title: "s".into(),
                root_pane: leaf,
            }],
            color: None,
        };

        let tree = describe_workspaces(std::slice::from_ref(&ws));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "demo");
        assert_eq!(tree[0].panes.len(), 1);
        assert_eq!(tree[0].panes[0].id, leaf_id);
        let tabs = &tree[0].panes[0].tabs;
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].kind, "terminal");
        assert_eq!(tabs[1].kind, "browser");
        assert!(!tabs[0].active, "tab1 is not the active surface");
        assert!(tabs[1].active, "tab2 is the active surface");
    }

    #[test]
    fn capabilities_lists_verbs_and_unsupported_and_round_trips() {
        let caps = capabilities();
        // A few representative verbs the agent docs promise.
        for v in ["open", "snapshot", "click", "is-visible", "count", "eval"] {
            assert!(
                caps.browser_verbs.iter().any(|s| s == v),
                "missing browser verb {v}"
            );
        }
        // The CDP-only features must be reported as unsupported.
        for u in ["screenshot", "wait", "viewport", "screencast"] {
            assert!(
                caps.unsupported.iter().any(|s| s == u),
                "missing unsupported feature {u}"
            );
        }
        // No verb should appear in both lists.
        for v in &caps.browser_verbs {
            assert!(!caps.unsupported.contains(v), "{v} is both supported+not");
        }
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(serde_json::from_str::<Capabilities>(&json).unwrap(), caps);
    }

    #[test]
    fn browser_open_carries_target_and_direction() {
        let env = Envelope {
            id: 1,
            payload: Payload::Request(Request::BrowserOpen {
                url: "https://example.com/".into(),
                target_pane: None,
                direction: SplitDirection::Vertical,
            }),
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.payload {
            Payload::Request(Request::BrowserOpen {
                url,
                target_pane,
                direction,
            }) => {
                assert_eq!(url, "https://example.com/");
                assert!(target_pane.is_none());
                assert_eq!(direction, SplitDirection::Vertical);
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn browser_navigate_roundtrips() {
        let pane = PaneId::new();
        let env = Envelope {
            id: 7,
            payload: Payload::Request(Request::BrowserNavigate {
                pane,
                url: "https://x/".into(),
            }),
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.payload {
            Payload::Request(Request::BrowserNavigate { pane: p, url }) => {
                assert_eq!(p, pane);
                assert_eq!(url, "https://x/");
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn browser_fill_carries_target_and_value() {
        let pane = PaneId::new();
        let env = Envelope {
            id: 9,
            payload: Payload::Request(Request::BrowserFill {
                pane,
                target: "e1".into(),
                value: "alice@example.com".into(),
            }),
        };
        let back: Envelope = serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        match back.payload {
            Payload::Request(Request::BrowserFill {
                pane: p,
                target,
                value,
            }) => {
                assert_eq!(p, pane);
                assert_eq!(target, "e1");
                assert_eq!(value, "alice@example.com");
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn browser_scroll_preserves_xy_signs() {
        let pane = PaneId::new();
        let env = Envelope {
            id: 11,
            payload: Payload::Request(Request::BrowserScroll {
                pane,
                target: "e1".into(),
                x: -10,
                y: 20,
            }),
        };
        let back: Envelope = serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
        match back.payload {
            Payload::Request(Request::BrowserScroll { x, y, .. }) => {
                assert_eq!(x, -10);
                assert_eq!(y, 20);
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn browser_pane_opened_response_serializes() {
        let pane = PaneId::new();
        let r = Response::BrowserPaneOpened {
            pane,
            placement_strategy: PlacementStrategy::SplitRight,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Response::BrowserPaneOpened {
                pane: p,
                placement_strategy: PlacementStrategy::SplitRight,
            } if p == pane
        ));
    }

    #[test]
    fn agent_session_update_roundtrips() {
        let surface = SurfaceId::new();
        let req = Request::AgentSessionUpdate {
            agent: "claude".into(),
            surface,
            session_id: "sess-abc".into(),
        };
        match roundtrip_request(req) {
            Request::AgentSessionUpdate {
                agent,
                surface: s,
                session_id,
            } => {
                assert_eq!(agent, "claude");
                assert_eq!(s, surface);
                assert_eq!(session_id, "sess-abc");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn agent_session_response_some_and_none_serialize() {
        let s = serde_json::to_string(&Response::AgentSession {
            session_id: Some("xyz".into()),
        })
        .unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::AgentSession { session_id: Some(s) } if s == "xyz"));

        let s = serde_json::to_string(&Response::AgentSession { session_id: None }).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::AgentSession { session_id: None }));
    }

    #[test]
    fn browser_pane_opened_carries_placement_strategy_on_wire() {
        let pane = PaneId::new();
        let s = serde_json::to_string(&Response::BrowserPaneOpened {
            pane,
            placement_strategy: PlacementStrategy::ReuseRightSibling,
        })
        .unwrap();
        // Wire shape: snake_case strategy, alongside pane uuid.
        assert!(
            s.contains(r#""placement_strategy":"reuse_right_sibling""#),
            "wire should expose snake_case strategy: {s}"
        );
        assert!(s.contains(&pane.to_string()));
    }

    #[test]
    fn browser_bool_result_serializes() {
        let s = serde_json::to_string(&Response::BrowserBoolResult { value: true }).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::BrowserBoolResult { value: true }));
    }

    #[test]
    fn rpc_errors_roundtrip_with_code_and_message() {
        let err = Response::Error(RpcError::InvalidArgument("bad pane".into()));
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains(r#""code":"invalid_argument""#));

        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            Response::Error(RpcError::InvalidArgument(message)) if message == "bad pane"
        ));
    }

    // -- helpers ---------------------------------------------------------

    /// Serialize → deserialize a Request through an Envelope and
    /// pull the Request back out, panicking on any non-Request
    /// payload. Used by every per-verb roundtrip check below.
    fn roundtrip_request(req: Request) -> Request {
        let env = Envelope {
            id: 1,
            payload: Payload::Request(req),
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        match back.payload {
            Payload::Request(r) => r,
            other => panic!("expected request, got {other:?}"),
        }
    }

    fn wire_value(req: &Request) -> serde_json::Value {
        let env = Envelope {
            id: 1,
            payload: Payload::Request(req.clone()),
        };
        serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap()
    }

    // -- per-verb roundtrip tests ----------------------------------------

    #[test]
    fn browser_back_roundtrips() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserBack { pane }) {
            Request::BrowserBack { pane: p } => assert_eq!(p, pane),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_forward_roundtrips() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserForward { pane }) {
            Request::BrowserForward { pane: p } => assert_eq!(p, pane),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_reload_roundtrips() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserReload { pane }) {
            Request::BrowserReload { pane: p } => assert_eq!(p, pane),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_url_roundtrips() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserUrl { pane }) {
            Request::BrowserUrl { pane: p } => assert_eq!(p, pane),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_title_roundtrips() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserTitle { pane }) {
            Request::BrowserTitle { pane: p } => assert_eq!(p, pane),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_click_carries_target() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserClick {
            pane,
            target: "e7".into(),
        }) {
            Request::BrowserClick { pane: p, target } => {
                assert_eq!(p, pane);
                assert_eq!(target, "e7");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_select_carries_target_and_value() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserSelect {
            pane,
            target: "e3".into(),
            value: "USD".into(),
        }) {
            Request::BrowserSelect {
                pane: p,
                target,
                value,
            } => {
                assert_eq!(p, pane);
                assert_eq!(target, "e3");
                assert_eq!(value, "USD");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_type_preserves_unicode_text() {
        let pane = PaneId::new();
        let text = "hello\nworld hi\t↩️";
        match roundtrip_request(Request::BrowserType {
            pane,
            text: text.into(),
        }) {
            Request::BrowserType { text: t, .. } => assert_eq!(t, text),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_press_carries_named_key() {
        let pane = PaneId::new();
        for key in ["Enter", "Escape", "ArrowDown", "F12"] {
            match roundtrip_request(Request::BrowserPress {
                pane,
                key: key.into(),
            }) {
                Request::BrowserPress { key: k, .. } => assert_eq!(k, key),
                other => panic!("wrong variant: {other:?}"),
            }
        }
    }

    #[test]
    fn browser_text_value_attr_roundtrip() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserText {
            pane,
            target: "e1".into(),
        }) {
            Request::BrowserText { target, .. } => assert_eq!(target, "e1"),
            other => panic!("wrong variant: {other:?}"),
        }
        match roundtrip_request(Request::BrowserValue {
            pane,
            target: "e2".into(),
        }) {
            Request::BrowserValue { target, .. } => assert_eq!(target, "e2"),
            other => panic!("wrong variant: {other:?}"),
        }
        match roundtrip_request(Request::BrowserAttr {
            pane,
            target: "e3".into(),
            name: "href".into(),
        }) {
            Request::BrowserAttr { target, name, .. } => {
                assert_eq!(target, "e3");
                assert_eq!(name, "href");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_eval_preserves_source_verbatim() {
        let pane = PaneId::new();
        let src = "(function(){ return JSON.stringify({a:1, b:'x \\\"y\\\"'}); })()";
        match roundtrip_request(Request::BrowserEval {
            pane,
            source: src.into(),
        }) {
            Request::BrowserEval { source, .. } => assert_eq!(source, src),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_snapshot_roundtrips() {
        let pane = PaneId::new();
        match roundtrip_request(Request::BrowserSnapshot { pane }) {
            Request::BrowserSnapshot { pane: p } => assert_eq!(p, pane),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_open_with_explicit_target_serializes_uuid_string() {
        let target = PaneId::new();
        let req = Request::BrowserOpen {
            url: "https://x/".into(),
            target_pane: Some(target),
            direction: SplitDirection::Horizontal,
        };
        let v = wire_value(&req);
        assert_eq!(v["verb"], "browser_open");
        assert_eq!(v["url"], "https://x/");
        assert_eq!(v["target_pane"], target.to_string());
        assert_eq!(v["direction"], "horizontal");
    }

    #[test]
    fn browser_open_with_no_target_serializes_null() {
        let req = Request::BrowserOpen {
            url: "https://x/".into(),
            target_pane: None,
            direction: SplitDirection::Vertical,
        };
        let v = wire_value(&req);
        assert!(v["target_pane"].is_null());
        assert_eq!(v["direction"], "vertical");
    }

    // -- wire-shape regression checks ------------------------------------

    #[test]
    fn each_browser_verb_is_snake_case_on_wire() {
        let pane = PaneId::new();
        let cases: Vec<(Request, &str)> = vec![
            (
                Request::BrowserNavigate {
                    pane,
                    url: "u".into(),
                },
                "browser_navigate",
            ),
            (Request::BrowserBack { pane }, "browser_back"),
            (Request::BrowserForward { pane }, "browser_forward"),
            (Request::BrowserReload { pane }, "browser_reload"),
            (Request::BrowserUrl { pane }, "browser_url"),
            (Request::BrowserTitle { pane }, "browser_title"),
            (
                Request::BrowserClick {
                    pane,
                    target: "e1".into(),
                },
                "browser_click",
            ),
            (
                Request::BrowserFill {
                    pane,
                    target: "e1".into(),
                    value: "v".into(),
                },
                "browser_fill",
            ),
            (
                Request::BrowserSelect {
                    pane,
                    target: "e1".into(),
                    value: "v".into(),
                },
                "browser_select",
            ),
            (
                Request::BrowserScroll {
                    pane,
                    target: "e1".into(),
                    x: 0,
                    y: 0,
                },
                "browser_scroll",
            ),
            (
                Request::BrowserType {
                    pane,
                    text: "t".into(),
                },
                "browser_type",
            ),
            (
                Request::BrowserPress {
                    pane,
                    key: "Enter".into(),
                },
                "browser_press",
            ),
            (
                Request::BrowserText {
                    pane,
                    target: "e1".into(),
                },
                "browser_text",
            ),
            (
                Request::BrowserValue {
                    pane,
                    target: "e1".into(),
                },
                "browser_value",
            ),
            (
                Request::BrowserAttr {
                    pane,
                    target: "e1".into(),
                    name: "href".into(),
                },
                "browser_attr",
            ),
            (Request::BrowserSnapshot { pane }, "browser_snapshot"),
            (
                Request::BrowserEval {
                    pane,
                    source: "1".into(),
                },
                "browser_eval",
            ),
        ];
        for (req, verb) in cases {
            let v = wire_value(&req);
            assert_eq!(
                v["verb"], verb,
                "wire verb mismatch for {verb}: got {:?}",
                v["verb"]
            );
        }
    }

    // -- response shape tests ------------------------------------------

    #[test]
    fn browser_ok_response_serializes() {
        let s = serde_json::to_string(&Response::BrowserOk).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::BrowserOk));
    }

    #[test]
    fn browser_result_response_carries_string() {
        let r = Response::BrowserResult {
            value: r#"{"url":"about:blank","title":"","nodes":[]}"#.into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back {
            Response::BrowserResult { value } => {
                assert!(value.contains("about:blank"));
                // The value should round-trip as parseable browser snapshot JSON.
                let _: serde_json::Value = serde_json::from_str(&value).unwrap();
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn browser_bool_result_response_false_path() {
        let s = serde_json::to_string(&Response::BrowserBoolResult { value: false }).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::BrowserBoolResult { value: false }));
    }
}
