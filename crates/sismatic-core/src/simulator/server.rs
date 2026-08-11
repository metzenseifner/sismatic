//! A real russh server that behaves like an Extron SMP.
//!
//! It offers only `keyboard-interactive` authentication (never `password`),
//! prints the SMP login banner the instant the shell opens, and answers SIS
//! requests on either the normal data stream or — as real hardware does — the
//! extended-data (stderr) stream.
//!
//! This is the shared harness: `bin/simulated-device.rs` runs it as a
//! standalone device, and the integration tests bind it on a loopback port.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;

use crate::simulator::dispatch::classify;
use crate::simulator::state::DeviceState;

/// A throwaway ed25519 host key. It authenticates nothing real, so committing
/// it is harmless — the client accepts any host key anyway (see `ClientHandler`
/// in the ssh transport).
pub const DEFAULT_HOST_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAYfora4TPkBvMx2eF/FwcBKEaoVuEYk1F3JnzDZyAeGAAAAJgKyK15Csit
eQAAAAtzc2gtZWQyNTUxOQAAACAYfora4TPkBvMx2eF/FwcBKEaoVuEYk1F3JnzDZyAeGA
AAAECscSo1FYZdIrWMUboREni6VNWV929M6YrBMsb9x57WxBh+itrhM+QG8zHZ4X8XBwEo
RqhW4RiTUXcmfMNnIB4YAAAAFXNpbS1zbXAtdGVzdC1ob3N0LWtleQ==
-----END OPENSSH PRIVATE KEY-----
"#;

/// Which SSH stream the simulated device answers on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplyStream {
    /// Normal channel data (`SSH_MSG_CHANNEL_DATA`, stdout).
    Stdout,
    /// Extended data (`SSH_MSG_CHANNEL_EXTENDED_DATA`, stderr) — how a real
    /// Extron SMP answers, which regressed once when the transport read only
    /// stdout.
    #[default]
    Stderr,
}

/// A running simulated device. Aborts its accept loop on drop, so no listener
/// leaks between tests.
#[derive(Debug)]
pub struct SimulatedDevice {
    port: u16,
    state: Arc<DeviceState>,
    /// `None` only after [`serve_forever`](SimulatedDevice::serve_forever) has
    /// taken the handle, which is also what opts that caller out of the abort.
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SimulatedDevice {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl SimulatedDevice {
    /// The port the device is listening on. When bound to port 0 this is the
    /// kernel-assigned one.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The live field store, shared with every connection.
    pub fn state(&self) -> &Arc<DeviceState> {
        &self.state
    }

    /// Run until the accept loop ends (it does not, under normal operation).
    /// Takes the join handle out of `self`, so the drop-abort does not fire.
    pub async fn serve_forever(mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// Bind a simulated device on `addr` and start accepting. Returns once it is
/// listening, so a caller can connect immediately without racing the bind.
///
/// Pass port 0 to let the kernel pick — [`SimulatedDevice::port`] reports it.
pub async fn bind(
    addr: impl tokio::net::ToSocketAddrs,
    state: Arc<DeviceState>,
    reply: ReplyStream,
) -> std::io::Result<SimulatedDevice> {
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();

    // Advertise only keyboard-interactive, like an Extron SMP.
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::KeyboardInteractive);

    let config = Arc::new(server::Config {
        methods,
        // Default is a 1s constant-time rejection delay; shorten it so the
        // password -> keyboard-interactive fallback isn't paced by a full second.
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::from_millis(1)),
        keys: vec![PrivateKey::from_openssh(DEFAULT_HOST_KEY).expect("valid host key")],
        ..Default::default()
    });

    let mut smp = SmpServer {
        state: Arc::clone(&state),
        reply,
    };
    let task = tokio::spawn(async move {
        let _ = smp.run_on_socket(config, &listener).await;
    });

    Ok(SimulatedDevice {
        port,
        state,
        task: Some(task),
    })
}

/// The server side: one [`SmpHandler`] per connection, all sharing one
/// [`DeviceState`] so writes persist across sessions.
struct SmpServer {
    state: Arc<DeviceState>,
    reply: ReplyStream,
}

impl Server for SmpServer {
    type Handler = SmpHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> SmpHandler {
        SmpHandler {
            state: Arc::clone(&self.state),
            reply: self.reply,
        }
    }
}

/// Reproduces the SMP auth handshake: `password` is never accepted, and
/// `keyboard-interactive` sends one "Password:" prompt whose answer must match
/// the configured credential.
struct SmpHandler {
    state: Arc<DeviceState>,
    reply: ReplyStream,
}

impl Handler for SmpHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        // The device does not offer password auth at all.
        Ok(Auth::reject())
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        match response {
            // First round: challenge the client for its password.
            None => Ok(Auth::Partial {
                name: Cow::Borrowed("Extron SMP"),
                instructions: Cow::Borrowed(""),
                prompts: Cow::Owned(vec![(Cow::Borrowed("Password: "), false)]),
            }),
            // Second round: accept iff the single answer matches.
            Some(mut answers) => {
                let creds = self.state.credentials();
                let ok = user == creds.username
                    && answers.next().as_deref() == Some(creds.password.as_bytes());
                if ok {
                    Ok(Auth::Accept)
                } else {
                    Ok(Auth::reject())
                }
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        // The SMP prints its banner (copyright + date) the instant the shell
        // opens, on the extended-data stream like the real device. The client
        // must drain this before its first query, or a banner line would be
        // parsed as the (untagged) reply.
        session.extended_data(channel, 1, self.state.banner().into_bytes())?;
        Ok(())
    }

    /// The SIS command interface. Requests outside the catalog, and reads of
    /// fields the config left unset, are answered with silence — which surfaces
    /// as a command timeout on the client side, like real hardware.
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(request) = std::str::from_utf8(data) else {
            return Ok(());
        };
        let Some(reply) = classify(request).and_then(|r| self.state.handle(r)) else {
            tracing::debug!(
                request = %crate::protocol::control_chars::swap_non_printable(request),
                "simulated device has no answer for this request"
            );
            return Ok(());
        };
        match self.reply {
            ReplyStream::Stdout => session.data(channel, reply.into_bytes())?,
            // ext == 1 is stderr; this is the path a real Extron SMP uses.
            ReplyStream::Stderr => session.extended_data(channel, 1, reply.into_bytes())?,
        }
        Ok(())
    }
}
