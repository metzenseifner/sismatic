//! The real SSH transport, backed by russh.
//!
//! [`RusshConnector`] dials a device, authenticates with a password, opens a
//! session channel, and requests an interactive shell — the SIS command
//! interface then flows over that channel as raw bytes. [`RusshTransport`]
//! wraps the channel's byte stream so the layers above are unaware they are
//! talking to real hardware rather than [`super::fake::FakeTransport`].
//!
//! Two deliberate simplifications, both candidates for hardening later:
//! - the server's host key is accepted unconditionally (no trust-on-first-use);
//! - an interactive shell is requested, which is how Extron SIS devices expose
//!   their command prompt over SSH.

use std::sync::Arc;

use async_trait::async_trait;
use russh::ChannelStream;
use russh::client::{self, Config, Handle, Msg};
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

        let auth = session
            .authenticate_password(config.username.as_str(), config.password.as_str())
            .await
            .map_err(connect_error)?;
        if !auth.success() {
            return Err(ConnectError::Failed(
                "password authentication rejected".into(),
            ));
        }

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

fn io_error(e: std::io::Error) -> TransportError {
    TransportError::Io(e.to_string())
}

fn connect_error(e: impl std::fmt::Display) -> ConnectError {
    ConnectError::Failed(e.to_string())
}
