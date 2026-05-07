// SPDX-License-Identifier: GPL-3.0-or-later
use flowmux_core::{NotificationLevel, PaneId, SplitDirection, SurfaceId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

    /// `flowmux notify --pane <id> --title ... --body ...`
    Notify {
        pane: Option<PaneId>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong,
    WorkspaceCreated { id: WorkspaceId },
    WorkspaceList { ids: Vec<WorkspaceId> },
    SurfaceCreated { id: SurfaceId },
    PaneSplitDone { new_pane: PaneId },
    BrowserResult { value: String },
    BrowserOk,
    BrowserBoolResult { value: bool },
    BrowserPaneOpened { pane: PaneId },
    CookiesImported { count: usize },
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
        let back: Envelope =
            serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
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
        let back: Envelope =
            serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
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
        let r = Response::BrowserPaneOpened { pane };
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::BrowserPaneOpened { pane: p } if p == pane));
    }

    #[test]
    fn browser_bool_result_serializes() {
        let s = serde_json::to_string(&Response::BrowserBoolResult { value: true }).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Response::BrowserBoolResult { value: true }
        ));
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
}
