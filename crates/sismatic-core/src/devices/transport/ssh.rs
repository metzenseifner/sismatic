//! The real SSH transport, backed by russh.
//!
//! [`RusshConnector`] dials a device, authenticates with a password, opens a
//! session channel, and requests an interactive shell — the SIS command
//! interface then flows over that channel as raw bytes. [`RusshTransport`]
//! reads that channel — merging both the normal (stdout) and extended-data
//! (stderr) streams, since Extron SMP devices reply on stderr — so the layers
//! above are unaware they are talking to real hardware rather than
//! [`super::fake::FakeTransport`].
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
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Config, Handle, KeyboardInteractiveAuthResponse, Msg};
use russh::keys::ssh_key::PublicKey;
use russh::{Channel, ChannelMsg};
use tracing::{debug, instrument};
use uuid::Uuid;

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
    channel: Channel<Msg>,
    /// Bytes from a channel message that did not fit the previous `read` buffer.
    pending: Vec<u8>,
    span: tracing::Span,
}

#[async_trait]
impl Transport for RusshTransport {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        debug!(parent: &self.span, data = %bytes.escape_ascii(), "TX");
        self.channel.data(bytes).await.map_err(io_error)
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // Refill from the channel only when the previous message has been fully
        // drained into earlier `read` calls.
        if self.pending.is_empty() {
            loop {
                match self.channel.wait().await {
                    // Extron SMP devices send their SIS replies on the
                    // extended-data (stderr) stream, so accept both Data and
                    // ExtendedData — see the `into_stream` note in `connect`.
                    Some(ChannelMsg::Data { data })
                    | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if data.is_empty() {
                            continue;
                        }
                        self.pending.extend_from_slice(&data);
                        break;
                    }
                    // The peer closed the channel: report EOF.
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => return Ok(0),
                    // Window adjusts, requests, successes, ...: keep waiting.
                    Some(_) => continue,
                }
            }
        }

        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        debug!(parent: &self.span, len = n, data = %buf[..n].escape_ascii(), "RX");
        Ok(n)
    }
}

/// Opens [`RusshTransport`]s with password authentication.
pub struct RusshConnector;

#[async_trait]
impl Connector for RusshConnector {
    #[instrument(
    name = "Connecting to get an SSH transport",
    skip_all,
    fields(
        connection_id = %Uuid::new_v4(),
        id = %config.id,
        host = %config.host,
        port = config.port
        ),
    )]
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

        let mut channel = session
            .channel_open_session()
            .await
            .map_err(connect_error)?;
        channel.request_shell(true).await.map_err(connect_error)?;

        // The SMP prints a two-line login banner (copyright + date) the moment
        // the shell opens, on the same stream it later uses for replies. In the
        // device's default verbose mode a query answer carries no tag (e.g.
        // unit-name reads back a bare `<name>\r\n`), so a banner line left in the
        // buffer is indistinguishable from an answer. Drain it here, before any
        // command is sent, so the first reply parses cleanly.
        drain_login_banner(&mut channel).await;

        Ok(Box::new(RusshTransport {
            _session: session,
            // Deliberately *not* channel.into_stream(): it builds its reader
            // with "ext: None", so it only reads ChannelMsg::Data, not
            // ExtendedData. Extron SMP devices answer SIS queries on the
            // extended-data (stderr) stream, so into_stream() dropped every
            // reply and each query timed out. Matching block that discards
            // ExtendedData while ext == None:
            // https://github.com/Eugeny/russh/blob/c4be19f1915c8682f4615c3fd50008512b474491/russh/src/channels/io/rx.rs#L79
            // Instead we drive channel.wait() in `read` and accept both streams.
            channel,
            pending: Vec::new(),
            span: tracing::Span::current(),
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
    let password = config.password.expose_secret();

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

/// Consume whatever the device volunteers right after the shell opens — its
/// login banner — by reading the channel until it stays quiet for `SETTLE`.
/// Runs before any command is written, so it can only swallow the banner, never
/// a reply. See the call site in [`connect`] for why this matters.
async fn drain_login_banner(channel: &mut Channel<Msg>) {
    const SETTLE: Duration = Duration::from_millis(500);
    while let Ok(msg) = tokio::time::timeout(SETTLE, channel.wait()).await {
        match msg {
            // Channel gone: nothing left to drain.
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            // Banner bytes, window adjusts, successes, ...: keep draining.
            Some(_) => continue,
        }
    }
}

fn io_error(e: impl std::fmt::Display) -> TransportError {
    TransportError::Io(e.to_string())
}

fn connect_error(e: impl std::fmt::Display) -> ConnectError {
    ConnectError::Failed(e.to_string())
}
