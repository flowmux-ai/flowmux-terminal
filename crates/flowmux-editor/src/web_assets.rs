// SPDX-License-Identifier: GPL-3.0-or-later
//! Loopback-only HTTP server for the embedded Monaco assets.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

const MAX_REQUEST_HEADER_BYTES: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const SECURITY_HEADERS: &str = concat!(
    "Content-Security-Policy: default-src 'none'; script-src 'self'; ",
    "style-src 'self' 'unsafe-inline'; ",
    "worker-src 'self'; font-src 'self' data:; img-src data:; connect-src 'none'; ",
    "base-uri 'none'; form-action 'none'\r\n",
    "X-Content-Type-Options: nosniff\r\n",
    "Cross-Origin-Resource-Policy: same-origin\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "Cache-Control: no-store\r\n",
);

struct Asset {
    path: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        path: "index.html",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/index.html"),
    },
    Asset {
        path: "main.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/main.js"),
    },
    Asset {
        path: "main.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/main.css"),
    },
    Asset {
        path: "editor.worker.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/editor.worker.js"),
    },
    Asset {
        path: "json.worker.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/json.worker.js"),
    },
    Asset {
        path: "css.worker.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/css.worker.js"),
    },
    Asset {
        path: "html.worker.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/html.worker.js"),
    },
    Asset {
        path: "ts.worker.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/ts.worker.js"),
    },
    Asset {
        path: "THIRD_PARTY_NOTICES.md",
        content_type: "text/markdown; charset=utf-8",
        bytes: include_bytes!("../../../editor/flowmux-editor-web/dist/THIRD_PARTY_NOTICES.md"),
    },
];

#[derive(Debug, Error)]
pub enum EditorAssetServerError {
    #[error("failed to bind editor asset server: {0}")]
    Bind(#[source] io::Error),
    #[error("failed to configure editor asset server: {0}")]
    Configure(#[source] io::Error),
    #[error("failed to start editor asset server: {0}")]
    Start(#[source] io::Error),
    #[error("invalid editor surface ID")]
    InvalidSurfaceId,
}

pub struct EditorAssetServer {
    address: SocketAddr,
    token: String,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl EditorAssetServer {
    pub fn start() -> Result<Self, EditorAssetServerError> {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(EditorAssetServerError::Bind)?;
        let address = listener
            .local_addr()
            .map_err(EditorAssetServerError::Configure)?;
        let token = Uuid::new_v4().simple().to_string();
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = stopping.clone();
        let thread_token = token.clone();
        let worker = thread::Builder::new()
            .name("flowmux-editor-assets".into())
            .spawn(move || serve(listener, thread_token, thread_stopping))
            .map_err(EditorAssetServerError::Start)?;

        Ok(Self {
            address,
            token,
            stopping,
            worker: Some(worker),
        })
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn editor_url(&self, surface_id: &str) -> Result<String, EditorAssetServerError> {
        if surface_id.is_empty()
            || surface_id.len() > 128
            || !surface_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(EditorAssetServerError::InvalidSurfaceId);
        }
        Ok(format!(
            "{}/{}/index.html?surface={surface_id}",
            self.origin(),
            self.token
        ))
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for EditorAssetServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(listener: TcpListener, token: String, stopping: Arc<AtomicBool>) {
    // Blocking accept: `Drop` sets `stopping` and then connects once to wake
    // this thread so it can observe the flag and exit.
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let request_token = token.clone();
                let _ = thread::Builder::new()
                    .name("flowmux-editor-asset-request".into())
                    .spawn(move || {
                        let _ = handle_connection(stream, &request_token);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn handle_connection(mut stream: TcpStream, token: &str) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let request = read_request_headers(&mut stream)?;
    let Some(request_line) = request.lines().next() else {
        return write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"bad request",
            false,
        );
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"bad request",
            false,
        );
    };
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"bad request",
            false,
        );
    }
    if method != "GET" && method != "HEAD" {
        return write_method_not_allowed(&mut stream);
    }

    let path = target.split('?').next().unwrap_or(target);
    let prefix = format!("/{token}/");
    let Some(asset_path) = path.strip_prefix(&prefix) else {
        return write_response(
            &mut stream,
            "404 Not Found",
            "text/plain",
            b"not found",
            method == "HEAD",
        );
    };
    let Some(asset) = ASSETS.iter().find(|asset| asset.path == asset_path) else {
        return write_response(
            &mut stream,
            "404 Not Found",
            "text/plain",
            b"not found",
            method == "HEAD",
        );
    };
    write_response(
        &mut stream,
        "200 OK",
        asset.content_type,
        asset.bytes,
        method == "HEAD",
    )
}

fn read_request_headers(stream: &mut TcpStream) -> io::Result<String> {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 1024];
    while request.len() < MAX_REQUEST_HEADER_BYTES {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    if request.len() >= MAX_REQUEST_HEADER_BYTES
        || !request.windows(4).any(|window| window == b"\r\n\r\n")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "editor asset request headers are incomplete or too large",
        ));
    }
    String::from_utf8(request)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request headers are not UTF-8"))
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\n{SECURITY_HEADERS}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn write_method_not_allowed(stream: &mut TcpStream) -> io::Result<()> {
    let body = b"method not allowed";
    write!(
        stream,
        "HTTP/1.1 405 Method Not Allowed\r\n{SECURITY_HEADERS}Allow: GET, HEAD\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_editor_and_workers_from_tokenized_loopback_origin() {
        let server = EditorAssetServer::start().unwrap();
        assert_eq!(
            server.address().ip(),
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        let url = server.editor_url("surface-1").unwrap();
        assert!(url.starts_with(&format!("{}/", server.origin())));

        let index = request(&server, "GET", "index.html");
        assert!(index.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(index.contains("Content-Type: text/html; charset=utf-8"));
        assert!(index.contains("Content-Security-Policy: default-src 'none'"));
        assert!(index.contains("style-src 'self' 'unsafe-inline'"));
        assert!(index.contains("font-src 'self' data:"));
        assert!(!index.contains("script-src 'self' 'unsafe-inline'"));
        assert!(index.contains("Cross-Origin-Resource-Policy: same-origin"));
        assert!(index.contains("Flowmux editor"));

        let worker = request(&server, "HEAD", "editor.worker.js");
        assert!(worker.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(worker.contains("Content-Type: text/javascript; charset=utf-8"));
        assert!(worker.ends_with("\r\n\r\n"));

        let main = request(&server, "GET", "main.js");
        let body = main
            .split_once("\r\n\r\n")
            .expect("asset response must contain a header separator")
            .1;
        let expected = ASSETS
            .iter()
            .find(|asset| asset.path == "main.js")
            .unwrap()
            .bytes
            .len();
        assert_eq!(body.len(), expected);
    }

    #[test]
    fn refuses_wrong_tokens_traversal_and_mutating_methods() {
        let server = EditorAssetServer::start().unwrap();
        let wrong_token = raw_request(
            server.address(),
            "GET /wrong/index.html HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(wrong_token.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let traversal = request(&server, "GET", "../Cargo.toml");
        assert!(traversal.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let post = request(&server, "POST", "index.html");
        assert!(post.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        assert!(post.contains("Allow: GET, HEAD"));
    }

    #[test]
    fn rejects_unsafe_surface_ids_and_stops_on_drop() {
        let server = EditorAssetServer::start().unwrap();
        let address = server.address();
        assert!(matches!(
            server.editor_url("../surface"),
            Err(EditorAssetServerError::InvalidSurfaceId)
        ));
        drop(server);

        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
    }

    fn request(server: &EditorAssetServer, method: &str, path: &str) -> String {
        raw_request(
            server.address(),
            &format!(
                "{method} /{}/{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                server.token
            ),
        )
    }

    fn raw_request(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}
