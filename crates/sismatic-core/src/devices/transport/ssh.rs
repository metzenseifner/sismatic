//! The real SSH transport, backed by russh.
//!
//! [`RusshConnector`] dials a device, authenticates with a password, opens a
//! session channel, and requests an interactive shell — the SIS command
//! interface then flows over that channel as raw bytes. [`RusshTransport`]
//! wraps the channel's byte stream so the layers above are unaware they are
//! talking to real hardware rather than [`super::fake::FakeTransport`].
//!
//! Authentication tries plain `password` first, then falls back to
//! `keyboard-interactive` — Extron SIS devices offer only the latter, prompting
//! for the password over an info request rather than accepting it directly.
//!
//! Two deliberate simplifications, both candidates for hardening later:
//! - the server's host key is accepted unconditionally (no trust-on-first-use);
//! - an interactive shell is requested, which is how Extron SIS devices expose
//!   their command prompt over SSH.

use std::sync::Arc;

use async_trait::async_trait;
use russh::ChannelStream;
use russh::client::{self, Config, Handle, KeyboardInteractiveAuthResponse, Msg};
use russh::keys::ssh_key::PublicKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::devices::config::DeviceConfig;
use crate::devices::connector::{ConnectError, Connector};
use crate::devices::transport::{Transport, TransportError};

/// A russh client handler. It carries no state; its only job is to accept the
/// server's host key (the default implementation rejects it).
struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// An open SSH channel to a device. Holds the session handle so the connection
/// stays alive for as long as the transport does.
pub struct RusshTransport {
    _session: Handle<ClientHandler>,
    stream: ChannelStream<Msg>,
}

#[async_trait]
impl Transport for RusshTransport {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.stream.write_all(bytes).await.map_err(io_error)?;
        self.stream.flush().await.map_err(io_error)
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.stream.read(buf).await.map_err(io_error)
    }
}

/// Opens [`RusshTransport`]s with password authentication.
pub struct RusshConnector;

#[async_trait]
impl Connector for RusshConnector {
    async fn connect(&self, config: &DeviceConfig) -> Result<Box<dyn Transport>, ConnectError> {
        let ssh_config = Arc::new(Config::default());

        let mut session = client::connect(
            ssh_config,
            (config.host.as_str(), config.port),
            ClientHandler,
        )
        .await
        .map_err(connect_error)?;

        authenticate(&mut session, config).await?;

        let channel = session
            .channel_open_session()
            .await
            .map_err(connect_error)?;
        channel.request_shell(true).await.map_err(connect_error)?;

        Ok(Box::new(RusshTransport {
            _session: session,
            stream: channel.into_stream(),
        }))
    }
}

/// Authenticate the session with the device's password. Tries plain `password`
/// auth first; if the server does not accept it (Extron SIS devices offer only
/// `keyboard-interactive`), retries over keyboard-interactive, answering every
/// prompt the server sends with the same password.
async fn authenticate(
    session: &mut Handle<ClientHandler>,
    config: &DeviceConfig,
) -> Result<(), ConnectError> {
    let username = config.username.as_str();
    let password = config.password.as_str();

    if session
        .authenticate_password(username, password)
        .await
        .map_err(connect_error)?
        .success()
    {
        return Ok(());
    }

    let mut response = session
        .authenticate_keyboard_interactive_start(username, None)
        .await
        .map_err(connect_error)?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => {
                return Err(ConnectError::Failed(
                    "password authentication rejected".into(),
                ));
            }
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_owned()).collect();
                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(connect_error)?;
            }
        }
    }
}

fn io_error(e: std::io::Error) -> TransportError {
    TransportError::Io(e.to_string())
}

fn connect_error(e: impl std::fmt::Display) -> ConnectError {
    ConnectError::Failed(e.to_string())
}
