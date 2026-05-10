// SPDX-License-Identifier: GPL-3.0-or-later
//! SSH workspaces for flowmux.
//!
//! Mirrors cmux's documented behavior:
//!
//! * `flowmux ssh user@host` opens a workspace whose terminal is a remote
//!   shell over an SSH channel.
//! * Browser panes inside an SSH workspace route their localhost
//!   requests through a remote port forward, so dev servers on the
//!   remote box "just work" from the in-app browser.
//! * Dragging an image into the terminal pane uploads it to the remote
//!   over SFTP and pastes the remote path.
//!
//! This crate is the transport layer. Authentication, channel I/O,
//! port forwarding, and SFTP are landed in follow-on commits — the
//! current code provides the target parser and the connection
//! scaffolding so the IPC verbs can already be wired through to it.
//!
//! TODO(SSH): replace [`SshClient::connect`] with a real russh handshake
//! once the host-key store and ssh-agent flow are designed.

use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: String,
    pub host: String,
    pub port: u16,
}

impl SshTarget {
    /// Parse `[user@]host[:port]`.
    pub fn parse(spec: &str) -> Result<Self, SshError> {
        let (user, hostport) = match spec.split_once('@') {
            Some((u, hp)) => (u.to_string(), hp),
            None => (
                std::env::var("USER").unwrap_or_else(|_| "root".into()),
                spec,
            ),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| SshError::ParseTarget(spec.into()))?,
            ),
            None => (hostport.to_string(), 22),
        };
        if host.is_empty() {
            return Err(SshError::ParseTarget(spec.into()));
        }
        Ok(SshTarget { user, host, port })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid ssh target: {0}")]
    ParseTarget(String),
    #[error("ssh handshake failed: {0}")]
    Handshake(String),
    #[error("auth failed for {0}")]
    AuthFailed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}

pub struct ClientHandler;

#[async_trait::async_trait]
impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO(SSH): consult ~/.ssh/known_hosts and prompt user on
        // unknown / changed keys. Skeleton accepts everything so the
        // GUI flow can be exercised end-to-end against test boxes.
        Ok(true)
    }
}

pub struct SshClient {
    pub session: russh::client::Handle<ClientHandler>,
    pub target: SshTarget,
}

impl SshClient {
    /// Connect and complete the SSH handshake. Authentication is left
    /// to a follow-on commit; today this returns [`SshError::Unimplemented`]
    /// so the verb is wired through the IPC surface but does not yet
    /// open a usable session.
    pub async fn connect(target: SshTarget) -> Result<Self, SshError> {
        let cfg = Arc::new(russh::client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        });
        let session =
            russh::client::connect(cfg, (target.host.as_str(), target.port), ClientHandler)
                .await
                .map_err(|e| SshError::Handshake(e.to_string()))?;

        // Authentication path lands once we have a host-key store + agent
        // integration. Holding the open session here is intentional so
        // we surface a clean Unimplemented error rather than leaking a
        // half-authenticated channel.
        drop(session);
        Err(SshError::Unimplemented("ssh authentication"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_user_host_port() {
        let t = SshTarget::parse("alice@dev.example.com:2222").unwrap();
        assert_eq!(t.user, "alice");
        assert_eq!(t.host, "dev.example.com");
        assert_eq!(t.port, 2222);
    }
    #[test]
    fn parse_host_only() {
        let t = SshTarget::parse("host").unwrap();
        assert_eq!(t.host, "host");
        assert_eq!(t.port, 22);
    }
    #[test]
    fn rejects_empty_host() {
        assert!(SshTarget::parse("user@").is_err());
    }

    #[test]
    fn rejects_non_numeric_port() {
        assert!(SshTarget::parse("user@host:ssh").is_err());
    }

    #[test]
    fn parse_user_at_host_without_port_defaults_to_22() {
        let t = SshTarget::parse("alice@dev").unwrap();
        assert_eq!(t.user, "alice");
        assert_eq!(t.host, "dev");
        assert_eq!(t.port, 22);
    }

    #[test]
    fn parse_host_with_port_uses_default_user_from_env() {
        // Without a `user@` prefix, the parser falls back to $USER (or
        // "root" if unset). Using a unique username so the assertion does
        // not collide with real CI environments.
        std::env::set_var("USER", "flowmux-test-user");
        let t = SshTarget::parse("dev:2222").unwrap();
        assert_eq!(t.user, "flowmux-test-user");
        assert_eq!(t.host, "dev");
        assert_eq!(t.port, 2222);
    }

    #[test]
    fn rejects_port_overflow() {
        // Above u16::MAX must be rejected; we don't silently truncate.
        assert!(SshTarget::parse("user@host:99999").is_err());
    }

    #[test]
    fn ipv6_literal_with_explicit_port_is_split_at_last_colon() {
        // Today the parser only supports a single colon (host:port) and
        // does not understand `[::1]:22`. Documenting the limitation as a
        // regression guard so that future bracket support can flip this
        // assertion intentionally.
        let result = SshTarget::parse("user@::1:22");
        // The current behavior: rsplit_once(':') produces ("::1", "22") so
        // a literal IPv6 happens to parse, but only because there are no
        // brackets in the input.
        let t = result.unwrap();
        assert_eq!(t.user, "user");
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 22);
    }
}
