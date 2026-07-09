// SPDX-License-Identifier: GPL-3.0-or-later
//! SSH workspace support for flowmux.
//!
//! What actually ships today:
//!
//! * [`SshTarget::parse`] turns a `[user@]host[:port]` spec into a target, and
//!   [`SshTarget::command_line`] builds the equivalent OpenSSH invocation.
//! * The GUI's `flowmux ssh user@host` creates a workspace and *types that
//!   OpenSSH command line into the pane as keystrokes*, so the remote shell runs
//!   through the host's system `ssh` client — see
//!   `flowmux::ipc_handler::GuiHandler::open_ssh_workspace`. This works.
//!
//! Runtime divergence: the headless `flowmux-daemon` handles the same
//! `SshConnect` verb by creating the workspace *only* — it does not inject the
//! `ssh` command line — so the verb's observable effect differs between the GUI
//! (workspace + remote shell) and the headless daemon (workspace only).
//!
//! What is **not** implemented (earlier revisions of this doc overstated it):
//! there is no in-crate SSH transport, no remote port forwarding, and no SFTP
//! upload. A russh-based native client (`SshClient`) exists only as scaffolding,
//! is gated behind the off-by-default `native-ssh` feature, and its `connect`
//! returns `SshError::Unimplemented`. Building the native transport (russh
//! handshake + `known_hosts` + ssh-agent auth) is future work, not a current
//! capability.

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

    pub fn workspace_name(&self) -> String {
        format!("ssh {}", self.address())
    }

    pub fn command_line(&self) -> String {
        let mut parts = vec!["ssh".to_string()];
        if self.port != 22 {
            parts.push("-p".into());
            parts.push(self.port.to_string());
        }
        parts.push(shell_quote(&self.address()));
        parts.join(" ")
    }

    fn address(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '@' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid ssh target: {0}")]
    ParseTarget(String),
    #[cfg(feature = "native-ssh")]
    #[error("ssh handshake failed: {0}")]
    Handshake(String),
    #[cfg(feature = "native-ssh")]
    #[error("auth failed for {0}")]
    AuthFailed(String),
    #[cfg(feature = "native-ssh")]
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "native-ssh")]
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}

/// russh-based native SSH transport scaffolding. Gated behind `native-ssh`
/// (off by default) and unused by the GUI/daemon, which drive SSH through the
/// host's system `ssh` client. Unimplemented: `connect` returns
/// `SshError::Unimplemented`.
#[cfg(feature = "native-ssh")]
mod native {
    use super::{SshError, SshTarget};
    use std::sync::Arc;
    use std::time::Duration;

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
        /// Connect and complete the SSH handshake. Authentication is future
        /// work; today this returns `SshError::Unimplemented` so the type
        /// exists but does not yet open a usable session.
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
}

#[cfg(feature = "native-ssh")]
pub use native::{ClientHandler, SshClient};

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
    fn builds_openssh_command_line() {
        let t = SshTarget::parse("alice@dev.example.com:2222").unwrap();
        assert_eq!(t.workspace_name(), "ssh alice@dev.example.com");
        assert_eq!(t.command_line(), "ssh -p 2222 alice@dev.example.com");
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
