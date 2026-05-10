// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress: many clients hammering the IPC server in parallel.
//!
//! Marked `#[ignore]`. Run with:
//!     cargo test -p flowmux-ipc --release --test stress_server_concurrent -- --ignored --nocapture
//!
//! Two probes:
//!
//! 1. **Concurrent ping throughput** — N clients each issue M Pings against
//!    the same server task. Asserts every response carries the matching
//!    correlation id (no cross-talk between client tasks) and finishes
//!    within a generous wall-clock budget.
//! 2. **Oversize-line peer cannot OOM the server** — one peer streams a
//!    payload far larger than the documented per-line cap with no newline.
//!    The server must drop just that connection and keep serving healthy
//!    clients on other connections.

use flowmux_core::WorkspaceId;
use flowmux_ipc::protocol::{Envelope, Event, Payload, Request, Response, RpcError};
use flowmux_ipc::server::{run, Handler};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct CountingPing {
    handled: AtomicU64,
}

impl Handler for CountingPing {
    fn handle<'a>(&'a self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
        Box::pin(async move {
            self.handled.fetch_add(1, Ordering::Relaxed);
            match req {
                Request::Ping => Response::Pong,
                other => Response::Error(RpcError::Unimplemented(format!("{other:?}"))),
            }
        })
    }
}

async fn write_envelope(stream: &mut UnixStream, env: &Envelope) -> std::io::Result<()> {
    let mut line = serde_json::to_string(env).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await
}

async fn read_envelope<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Envelope> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "server closed",
        ));
    }
    Ok(serde_json::from_str(line.trim_end()).unwrap())
}

async fn spawn_server(handler: Arc<CountingPing>) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap().keep();
    let socket = dir.join("flowmux.sock");
    let socket_path = socket.clone();
    let task = tokio::spawn(async move {
        // run loops forever; the test drops the handle once it's done.
        let _ = run(&socket, handler).await;
    });
    // Wait for the socket to materialize before returning.
    let start = Instant::now();
    while !socket_path.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("server never bound socket");
        }
        tokio::task::yield_now().await;
    }
    (socket_path, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress: concurrent IPC server load"]
async fn many_clients_ping_concurrently_without_id_crosstalk() {
    const CLIENTS: usize = 32;
    const PER_CLIENT: u64 = 200;
    const BUDGET: Duration = Duration::from_secs(20);

    let handler = Arc::new(CountingPing {
        handled: AtomicU64::new(0),
    });
    let (socket, server) = spawn_server(handler.clone()).await;

    let start = Instant::now();
    let mut joins = Vec::new();
    for client_idx in 0..CLIENTS {
        let socket = socket.clone();
        joins.push(tokio::spawn(async move {
            let stream = UnixStream::connect(&socket).await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut reader = BufReader::new(r);
            for i in 0..PER_CLIENT {
                let id = (client_idx as u64) << 32 | i;
                let env = Envelope {
                    id,
                    payload: Payload::Request(Request::Ping),
                };
                let mut line = serde_json::to_string(&env).unwrap();
                line.push('\n');
                w.write_all(line.as_bytes()).await.unwrap();
                w.flush().await.unwrap();
                let mut buf = String::new();
                let n = reader.read_line(&mut buf).await.unwrap();
                assert!(n > 0, "server closed mid-burst on client {client_idx}");
                let resp: Envelope = serde_json::from_str(buf.trim_end()).unwrap();
                assert_eq!(resp.id, id, "id crosstalk on client {client_idx}, op {i}");
                assert!(matches!(resp.payload, Payload::Response(Response::Pong)));
            }
        }));
    }
    for j in joins {
        j.await.unwrap();
    }
    let elapsed = start.elapsed();
    server.abort();
    assert!(
        elapsed < BUDGET,
        "concurrent ping burst took {elapsed:?}, budget {BUDGET:?}"
    );
    assert_eq!(
        handler.handled.load(Ordering::Relaxed),
        (CLIENTS as u64) * PER_CLIENT
    );
    eprintln!(
        "concurrent ping: {} clients x {} ops in {:?}",
        CLIENTS, PER_CLIENT, elapsed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress: hostile peer cannot OOM the server"]
async fn oversize_line_does_not_starve_other_clients() {
    let handler = Arc::new(CountingPing {
        handled: AtomicU64::new(0),
    });
    let (socket, server) = spawn_server(handler.clone()).await;

    // Hostile client: stream way more bytes than the cap (1 MiB) without a
    // newline, then close. The server should drop that connection cleanly.
    let hostile_socket = socket.clone();
    let hostile = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&hostile_socket).await.unwrap();
        // 2 MiB of garbage, no newline.
        let payload = vec![b'A'; 2 * 1024 * 1024];
        // write_all may either succeed (server has buffered) or error
        // (server closed mid-write). Both are acceptable outcomes; the
        // important thing is the server keeps running.
        let _ = stream.write_all(&payload).await;
        let _ = stream.shutdown().await;
    });

    // Healthy client in parallel: should still get answered.
    let healthy_socket = socket.clone();
    let healthy = tokio::spawn(async move {
        let stream = UnixStream::connect(&healthy_socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        for i in 0..10u64 {
            let env = Envelope {
                id: 1000 + i,
                payload: Payload::Request(Request::Ping),
            };
            let mut line = serde_json::to_string(&env).unwrap();
            line.push('\n');
            w.write_all(line.as_bytes()).await.unwrap();
            w.flush().await.unwrap();
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).await.unwrap();
            assert!(n > 0, "healthy client got starved on op {i}");
            let resp: Envelope = serde_json::from_str(buf.trim_end()).unwrap();
            assert_eq!(resp.id, 1000 + i);
            assert!(matches!(resp.payload, Payload::Response(Response::Pong)));
        }
    });

    healthy.await.unwrap();
    hostile.await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "stress: malformed envelope does not break the connection"]
async fn malformed_envelope_skipped_then_next_request_succeeds() {
    let handler = Arc::new(CountingPing {
        handled: AtomicU64::new(0),
    });
    let (socket, server) = spawn_server(handler.clone()).await;

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    // Send malformed JSON, then a valid Ping. Server should log + skip the
    // first and answer the second.
    stream.write_all(b"{not json}\n").await.unwrap();
    let env = Envelope {
        id: 7,
        payload: Payload::Request(Request::Ping),
    };
    write_envelope(&mut stream, &env).await.unwrap();
    let (r, _w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let resp = read_envelope(&mut reader).await.unwrap();
    assert_eq!(resp.id, 7);
    assert!(matches!(resp.payload, Payload::Response(Response::Pong)));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "stress: client-sent events get a structured rejection"]
async fn client_sent_event_payload_is_rejected_with_invalid_argument() {
    // The server should refuse to process Event/Response payloads from
    // clients (they are server→client only) and surface the rejection as
    // a structured error rather than closing the socket.
    let handler = Arc::new(CountingPing {
        handled: AtomicU64::new(0),
    });
    let (socket, server) = spawn_server(handler.clone()).await;

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    let env = Envelope {
        id: 99,
        payload: Payload::Event(Event::NotificationRaised {
            workspace: WorkspaceId::new(),
            body: "x".into(),
            level: flowmux_core::NotificationLevel::Info,
        }),
    };
    write_envelope(&mut stream, &env).await.unwrap();
    let (r, _w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let resp = read_envelope(&mut reader).await.unwrap();
    assert_eq!(resp.id, 99);
    match resp.payload {
        Payload::Response(Response::Error(RpcError::InvalidArgument(_))) => {}
        other => panic!("unexpected response payload: {other:?}"),
    }
    server.abort();
}
