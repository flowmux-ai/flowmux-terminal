// SPDX-License-Identifier: GPL-3.0-or-later
//! Skeleton daemon that accepts client connections and dispatches
//! requests via a user-supplied [`Handler`]. The GUI binary owns the
//! handler implementation; this crate only owns the wire protocol.

use crate::protocol::{Envelope, Payload, Request, Response, RpcError};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

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
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        let env: Envelope = match serde_json::from_str(buf.trim_end()) {
            Ok(e) => e,
            Err(e) => {
                error!(error = %e, raw = %buf, "malformed envelope");
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
