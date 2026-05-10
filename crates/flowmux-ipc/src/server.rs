// SPDX-License-Identifier: GPL-3.0-or-later
//! Skeleton daemon that accepts client connections and dispatches
//! requests via a user-supplied [`Handler`]. The GUI binary owns the
//! handler implementation; this crate only owns the wire protocol.

use crate::protocol::{Envelope, Payload, Request, Response, RpcError};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

/// Hard cap on a single envelope. Real envelopes are tiny (KB), so 1 MiB is
/// generous and still bounds memory if a peer streams without ever sending
/// `\n`. Without this cap, `read_line` would buffer indefinitely.
pub(crate) const MAX_LINE_BYTES: usize = 1024 * 1024;

pub trait Handler: Send + Sync + 'static {
    fn handle<'a>(&'a self, req: Request) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>>;
}

pub async fn run<H: Handler>(socket: &Path, handler: Arc<H>) -> anyhow::Result<()> {
    if socket.exists() {
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    info!(path = %socket.display(), "flowmux daemon listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let h = handler.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_one(stream, h).await {
                warn!(error = %e, "client disconnected with error");
            }
        });
    }
}

async fn serve_one<H: Handler>(stream: UnixStream, handler: Arc<H>) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut buf = String::new();
    loop {
        match read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Oversized or non-utf8 line. Closing is the safe response: the
                // peer has either misbehaved or is on a corrupt stream we can't
                // resync on.
                warn!(error = %e, "client sent malformed framing; dropping connection");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        let env: Envelope = match serde_json::from_str(buf.trim_end()) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, raw = %buf, "malformed envelope");
                continue;
            }
        };
        let response = match env.payload {
            Payload::Request(req) => handler.handle(req).await,
            Payload::Response(_) | Payload::Event(_) => Response::Error(RpcError::InvalidArgument(
                "client sent non-request payload".into(),
            )),
        };
        let out = Envelope {
            id: env.id,
            payload: Payload::Response(response),
        };
        let mut line = serde_json::to_string(&out)?;
        line.push('\n');
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
    }
}

/// Read one `\n`-terminated line into `out`, refusing to grow past `max`
/// bytes. Mirrors `BufRead::read_line` in shape but caps the buffer so a
/// peer that never sends a newline cannot drive memory growth.
///
/// Returns `Ok(0)` on clean EOF, `Ok(n)` on a complete line of length `n`
/// (newline included). On overflow returns `InvalidData`; the caller is
/// expected to drop the connection.
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut String,
    max: usize,
) -> std::io::Result<usize> {
    out.clear();
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let (consumed, found_newline) = {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                if bytes.is_empty() {
                    return Ok(0);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "stream ended without newline",
                ));
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    let take = pos + 1;
                    if bytes.len() + take > max {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("line exceeded {max} bytes"),
                        ));
                    }
                    bytes.extend_from_slice(&chunk[..take]);
                    (take, true)
                }
                None => {
                    if bytes.len() + chunk.len() > max {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("line exceeded {max} bytes"),
                        ));
                    }
                    let n = chunk.len();
                    bytes.extend_from_slice(chunk);
                    (n, false)
                }
            }
        };
        reader.consume(consumed);
        if found_newline {
            break;
        }
    }
    match String::from_utf8(bytes) {
        Ok(s) => {
            let n = s.len();
            *out = s;
            Ok(n)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "non-utf8 line",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Event;
    use flowmux_core::{NotificationLevel, WorkspaceId};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct PingHandler;

    impl Handler for PingHandler {
        fn handle<'a>(
            &'a self,
            req: Request,
        ) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
            Box::pin(async move {
                match req {
                    Request::Ping => Response::Pong,
                    other => Response::Error(RpcError::Unimplemented(format!("{other:?}"))),
                }
            })
        }
    }

    async fn write_envelope(stream: &mut UnixStream, env: Envelope) {
        let mut line = serde_json::to_string(&env).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).await.unwrap();
    }

    async fn read_envelope(reader: &mut BufReader<UnixStream>) -> Envelope {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim_end()).unwrap()
    }

    #[tokio::test]
    async fn serves_request_envelopes_with_matching_response_ids() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve_one(server, Arc::new(PingHandler)));

        write_envelope(
            &mut client,
            Envelope {
                id: 7,
                payload: Payload::Request(Request::Ping),
            },
        )
        .await;
        let mut reader = BufReader::new(client);
        let env = read_envelope(&mut reader).await;

        assert_eq!(env.id, 7);
        assert!(matches!(env.payload, Payload::Response(Response::Pong)));
        drop(reader);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn drops_connection_on_oversized_line_without_oom() {
        // A peer that streams forever without `\n` must not be able to drive
        // unbounded memory growth: the bounded reader caps the buffer at
        // MAX_LINE_BYTES and the connection closes.
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve_one(server, Arc::new(PingHandler)));

        // 2x the cap, no newline, then close the writer.
        let payload = vec![b'A'; MAX_LINE_BYTES + 1];
        client.write_all(&payload).await.unwrap();
        drop(client); // trigger EOF on the server side after the overflow

        // serve_one must return Ok (connection closed cleanly) rather than
        // panicking or running out of memory.
        let result = task.await.unwrap();
        assert!(
            result.is_ok(),
            "expected clean shutdown on oversized line, got {result:?}"
        );
    }

    #[tokio::test]
    async fn read_line_bounded_returns_invalid_data_when_line_exceeds_max() {
        use tokio::io::BufReader as TokioBufReader;
        // Feed bytes from one half of a unix pair while the other half
        // exercises read_line_bounded directly so we can assert the exact
        // error kind without going through serve_one.
        let (a, mut b) = UnixStream::pair().unwrap();
        let writer_task = tokio::spawn(async move {
            let payload = vec![b'X'; 32];
            b.write_all(&payload).await.unwrap();
            // No newline, no close yet — but the read should fail before
            // we hit EOF because the cap is hit first.
            b.shutdown().await.unwrap();
        });

        let mut reader = TokioBufReader::new(a);
        let mut buf = String::new();
        let err = read_line_bounded(&mut reader, &mut buf, 16)
            .await
            .expect_err("expected overflow error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_line_bounded_returns_ok_for_complete_line_within_cap() {
        use tokio::io::BufReader as TokioBufReader;
        let (a, mut b) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            b.write_all(b"hello world\n").await.unwrap();
        });

        let mut reader = TokioBufReader::new(a);
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf, 1024)
            .await
            .unwrap();
        assert_eq!(buf, "hello world\n");
        assert_eq!(n, "hello world\n".len());
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn read_line_bounded_returns_zero_on_clean_eof_before_first_byte() {
        use tokio::io::BufReader as TokioBufReader;
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);

        let mut reader = TokioBufReader::new(a);
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf, 1024)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn read_line_bounded_errors_on_eof_mid_line() {
        // A peer that wrote data but never sent `\n` then closed should be
        // surfaced as UnexpectedEof, not silently treated as success.
        use tokio::io::BufReader as TokioBufReader;
        let (a, mut b) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            b.write_all(b"partial").await.unwrap();
            b.shutdown().await.unwrap();
        });

        let mut reader = TokioBufReader::new(a);
        let mut buf = String::new();
        let err = read_line_bounded(&mut reader, &mut buf, 1024)
            .await
            .expect_err("expected UnexpectedEof");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_client_non_request_payloads() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve_one(server, Arc::new(PingHandler)));

        write_envelope(
            &mut client,
            Envelope {
                id: 9,
                payload: Payload::Event(Event::NotificationRaised {
                    workspace: WorkspaceId::new(),
                    body: "body".into(),
                    level: NotificationLevel::Info,
                }),
            },
        )
        .await;
        let mut reader = BufReader::new(client);
        let env = read_envelope(&mut reader).await;

        assert_eq!(env.id, 9);
        assert!(matches!(
            env.payload,
            Payload::Response(Response::Error(RpcError::InvalidArgument(_)))
        ));
        drop(reader);
        task.await.unwrap().unwrap();
    }
}
