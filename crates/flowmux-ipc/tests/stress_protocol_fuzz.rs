// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress: serialization round-trip + light fuzz over the IPC protocol.
//!
//! Marked `#[ignore]`. Run with:
//!     cargo test -p flowmux-ipc --release --test stress_protocol_fuzz -- --ignored --nocapture
//!
//! Two passes:
//!
//! 1. **Round-trip stability** -- generate envelopes covering every variant
//!    of [`Request`], [`Response`], [`Event`], and [`RpcError`]; serialize,
//!    deserialize, then serialize the deserialized form. The two JSON
//!    strings must be byte-equal so a refactor that drops a tag, breaks
//!    rename_all, or loses a field surfaces here.
//! 2. **Garbage-input safety** -- take valid serialized envelopes and
//!    apply byte-level mutations (flip / drop / insert random bytes).
//!    Deserialization must return `Err`, never panic.

use flowmux_core::{
    NotificationLevel, PaneId, PlacementStrategy, SplitDirection, SurfaceId, WorkspaceId,
};
use flowmux_ipc::protocol::{Envelope, Event, Payload, Request, Response, RpcError};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Xs(u64);
impl Xs {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn next_u8(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
    fn ascii_word(&mut self, max: usize) -> String {
        let len = (self.next_u64() as usize) % max.max(1);
        (0..len)
            .map(|_| {
                let c = (self.next_u64() % 26) as u8 + b'a';
                c as char
            })
            .collect()
    }
}

fn requests_for_round_trip(rng: &mut Xs) -> Vec<Request> {
    let pane = PaneId::new();
    let surface = SurfaceId::new();
    let ws = WorkspaceId::new();
    let dir = if rng.next_u64() & 1 == 0 {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    };
    vec![
        Request::Ping,
        Request::WorkspaceCreate {
            name: Some(rng.ascii_word(12)),
            root: PathBuf::from("/tmp/x"),
        },
        Request::WorkspaceCreate {
            name: None,
            root: PathBuf::from("/tmp/y"),
        },
        Request::WorkspaceList,
        Request::SurfaceCreate {
            workspace: ws,
            cwd: Some(PathBuf::from("/var/log")),
        },
        Request::SurfaceCreate {
            workspace: ws,
            cwd: None,
        },
        Request::PaneSplit {
            pane,
            direction: dir,
        },
        Request::PaneSendKeys {
            pane,
            keys: rng.ascii_word(20),
        },
        Request::Notify {
            pane: Some(pane),
            surface: None,
            title: rng.ascii_word(8),
            body: rng.ascii_word(40),
            level: NotificationLevel::Info,
        },
        Request::Notify {
            pane: None,
            surface: None,
            title: String::new(),
            body: String::new(),
            level: NotificationLevel::AttentionNeeded,
        },
        Request::SshConnect {
            target: "user@host:22".into(),
        },
        Request::BrowserOpen {
            url: "https://example.com".into(),
            target_pane: Some(pane),
            direction: dir,
        },
        Request::BrowserOpen {
            url: "https://example.com".into(),
            target_pane: None,
            direction: dir,
        },
        Request::BrowserNavigate {
            pane,
            url: "about:blank".into(),
        },
        Request::BrowserBack { pane },
        Request::BrowserForward { pane },
        Request::BrowserReload { pane },
        Request::BrowserUrl { pane },
        Request::BrowserTitle { pane },
        Request::BrowserClick {
            pane,
            target: "e1".into(),
        },
        Request::BrowserFill {
            pane,
            target: "e2".into(),
            value: rng.ascii_word(15),
        },
        Request::BrowserSelect {
            pane,
            target: "e3".into(),
            value: "opt-1".into(),
        },
        Request::BrowserScroll {
            pane,
            target: "body".into(),
            x: 0,
            y: 1024,
        },
        Request::BrowserType {
            pane,
            text: rng.ascii_word(20),
        },
        Request::BrowserPress {
            pane,
            key: "Enter".into(),
        },
        Request::BrowserText {
            pane,
            target: "h1".into(),
        },
        Request::BrowserValue {
            pane,
            target: "input".into(),
        },
        Request::BrowserAttr {
            pane,
            target: "a".into(),
            name: "href".into(),
        },
        Request::BrowserDblClick {
            pane,
            target: "e1".into(),
        },
        Request::BrowserHover {
            pane,
            target: "e1".into(),
        },
        Request::BrowserFocus {
            pane,
            target: "e1".into(),
        },
        Request::BrowserBlur {
            pane,
            target: "e1".into(),
        },
        Request::BrowserCheck {
            pane,
            target: "e1".into(),
        },
        Request::BrowserUncheck {
            pane,
            target: "e1".into(),
        },
        Request::BrowserIsVisible {
            pane,
            target: "e1".into(),
        },
        Request::BrowserIsEnabled {
            pane,
            target: "e1".into(),
        },
        Request::BrowserIsChecked {
            pane,
            target: "e1".into(),
        },
        Request::BrowserCount {
            pane,
            selector: ".row".into(),
        },
        Request::AgentSessionUpdate {
            agent: "claude".into(),
            surface,
            session_id: rng.ascii_word(20),
        },
        Request::AgentSessionGet {
            agent: "codex".into(),
            surface,
        },
        Request::AgentSessionForget {
            agent: "opencode".into(),
            surface,
        },
        Request::ClaudeTeams {
            count: 4,
            args: vec!["--continue".into(), rng.ascii_word(8)],
            root: PathBuf::from("/tmp/team"),
        },
        Request::BrowserSnapshot { pane },
        Request::BrowserEval {
            pane,
            source: "1+1".into(),
        },
        Request::ImportCookies {
            source: "firefox".into(),
            domain: Some("example.com".into()),
        },
        Request::ImportCookies {
            source: "chrome".into(),
            domain: None,
        },
    ]
}

fn responses_for_round_trip() -> Vec<Response> {
    vec![
        Response::Ok,
        Response::Pong,
        Response::WorkspaceCreated {
            id: WorkspaceId::new(),
        },
        Response::WorkspaceList {
            ids: (0..16).map(|_| WorkspaceId::new()).collect(),
        },
        Response::SurfaceCreated {
            id: SurfaceId::new(),
        },
        Response::PaneSplitDone {
            new_pane: PaneId::new(),
        },
        Response::BrowserResult {
            value: "result".into(),
        },
        Response::BrowserOk,
        Response::BrowserBoolResult { value: true },
        Response::BrowserBoolResult { value: false },
        Response::BrowserPaneOpened {
            pane: PaneId::new(),
            placement_strategy: PlacementStrategy::SplitRight,
        },
        Response::BrowserPaneOpened {
            pane: PaneId::new(),
            placement_strategy: PlacementStrategy::ReuseRightSibling,
        },
        Response::CookiesImported { count: 1234 },
        Response::AgentSession {
            session_id: Some("sess".into()),
        },
        Response::AgentSession { session_id: None },
        Response::Error(RpcError::Unimplemented("nope".into())),
        Response::Error(RpcError::NotFound("ws".into())),
        Response::Error(RpcError::InvalidArgument("bad".into())),
        Response::Error(RpcError::Io("io".into())),
        Response::Error(RpcError::Internal("oops".into())),
    ]
}

fn events_for_round_trip() -> Vec<Event> {
    vec![
        Event::NotificationRaised {
            workspace: WorkspaceId::new(),
            body: "hello".into(),
            level: NotificationLevel::Info,
        },
        Event::NotificationRaised {
            workspace: WorkspaceId::new(),
            body: String::new(),
            level: NotificationLevel::Error,
        },
        Event::PortListening {
            workspace: WorkspaceId::new(),
            port: 0,
        },
        Event::PortListening {
            workspace: WorkspaceId::new(),
            port: u16::MAX,
        },
    ]
}

fn assert_round_trip_stable(env: &Envelope) {
    let s1 = serde_json::to_string(env).expect("envelope must serialize");
    let parsed: Envelope = serde_json::from_str(&s1)
        .unwrap_or_else(|e| panic!("envelope must deserialize back: {e}; payload was: {s1}"));
    let s2 = serde_json::to_string(&parsed).expect("re-serialize must succeed");
    assert_eq!(
        s1, s2,
        "round-trip JSON not stable for envelope:\n  first:  {s1}\n  second: {s2}"
    );
}

#[test]
#[ignore = "stress: protocol round-trip across all variants"]
fn protocol_envelopes_survive_round_trip() {
    let mut rng = Xs::new(0xFE_ED_BE_EF_BA_AD_F0_0Du64);
    let start = Instant::now();
    let mut count = 0usize;

    for variant in requests_for_round_trip(&mut rng) {
        let env = Envelope {
            id: rng.next_u64(),
            payload: Payload::Request(variant),
        };
        assert_round_trip_stable(&env);
        count += 1;
    }
    for variant in responses_for_round_trip() {
        let env = Envelope {
            id: rng.next_u64(),
            payload: Payload::Response(variant),
        };
        assert_round_trip_stable(&env);
        count += 1;
    }
    for variant in events_for_round_trip() {
        let env = Envelope {
            id: rng.next_u64(),
            payload: Payload::Event(variant),
        };
        assert_round_trip_stable(&env);
        count += 1;
    }
    eprintln!(
        "round-trip: {count} envelopes verified in {:?}",
        start.elapsed()
    );
}

#[test]
#[ignore = "stress: protocol garbage-input safety"]
fn protocol_decoder_rejects_garbage_without_panic() {
    // Take a known-good serialized envelope and apply byte-level mutations.
    // None of these may panic; serde_json must always return Err for
    // syntactically broken or schema-violating input, never abort. (Valid
    // mutations that happen to remain parseable are also fine.)
    const ITER: usize = 5_000;
    const BUDGET: Duration = Duration::from_secs(20);

    let seed_envelopes = [
        Envelope {
            id: 1,
            payload: Payload::Request(Request::Ping),
        },
        Envelope {
            id: 42,
            payload: Payload::Request(Request::PaneSplit {
                pane: PaneId::new(),
                direction: SplitDirection::Vertical,
            }),
        },
        Envelope {
            id: 7,
            payload: Payload::Response(Response::Error(RpcError::NotFound("x".into()))),
        },
    ];
    let baselines: Vec<String> = seed_envelopes
        .iter()
        .map(|e| serde_json::to_string(e).expect("seed envelope serializes"))
        .collect();

    let mut rng = Xs::new(0xDEAD_BEEF_F00Du64);
    let start = Instant::now();
    let mut decoded_ok = 0u32;
    let mut decoded_err = 0u32;

    for i in 0..ITER {
        let base = &baselines[(rng.next_u64() as usize) % baselines.len()];
        let mut bytes = base.as_bytes().to_vec();
        // Mutation: flip / drop / insert N times.
        let muts = (rng.next_u64() % 6 + 1) as usize;
        for _ in 0..muts {
            if bytes.is_empty() {
                bytes.push(b'{');
                continue;
            }
            let pos = (rng.next_u64() as usize) % bytes.len();
            match rng.next_u64() % 3 {
                0 => bytes[pos] = rng.next_u8(),
                1 => {
                    if bytes.len() > 1 {
                        bytes.remove(pos);
                    }
                }
                _ => bytes.insert(pos, rng.next_u8()),
            }
        }
        // The use of `String::from_utf8_lossy` is intentional: serde_json
        // accepts invalid UTF-8 only inside string values, and on the wire
        // we only ever decode bytes that arrived as valid UTF-8 from
        // tokio's line codec. Lossy here matches that guarantee for the
        // fuzz target.
        let s = String::from_utf8_lossy(&bytes);
        match serde_json::from_str::<Envelope>(&s) {
            Ok(_) => decoded_ok += 1,
            Err(_) => decoded_err += 1,
        }

        assert!(
            start.elapsed() < BUDGET,
            "garbage-input fuzz exceeded {BUDGET:?} after {i} iters"
        );
    }
    eprintln!(
        "garbage-input fuzz: {ITER} mutations -> {decoded_ok} accepted, {decoded_err} rejected, no panic, {:?}",
        start.elapsed()
    );
}
