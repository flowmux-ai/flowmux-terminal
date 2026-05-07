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
    WorkspaceCreate { name: Option<String>, root: PathBuf },

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
    PaneSendKeys { pane: PaneId, keys: String },

    /// `flowmux notify --pane <id> --title ... --body ...`
    Notify {
        pane: Option<PaneId>,
        title: String,
        body: String,
        level: NotificationLevel,
    },

    /// `flowmux ssh user@host` — open a remote workspace.
    SshConnect { target: String },

    /// `flowmux browser open <url>` — open URL in the in-app browser pane.
    BrowserOpen {
        url: String,
        surface: Option<SurfaceId>,
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
    BrowserSnapshot { pane: PaneId },

    /// `flowmux browser eval --pane <id> <js>` — evaluate JS, return result.
    BrowserEval { pane: PaneId, source: String },

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
