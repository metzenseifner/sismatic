//! Integration test against a *real* SSH server that behaves like an Extron
//! SMP: it offers only `keyboard-interactive` authentication, never `password`.
//! This exercises [`RusshConnector`]'s auth fallback end to end over a genuine
//! russh handshake rather than a mock.
//!
//! The setup [`spawn_smp`] binds a russh server
//! on a random loopback port and returns its port plus the credentials it will
//! accept, so each test dials it exactly the way production dials a device. The
//! whole file is gated on the `ssh` feature, matching [`real_ssh`](super).
#![cfg(feature = "ssh")]

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;

use sismatic_core::devices::config::DeviceConfig;
use sismatic_core::devices::connector::{ConnectError, Connector};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::query::Query;
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

/// A throwaway ed25519 host key generated once for this test. It authenticates
/// nothing real, so committing it is harmless — the client accepts any host key
/// anyway (see `ClientHandler` in the ssh transport).
const HOST_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAYfora4TPkBvMx2eF/FwcBKEaoVuEYk1F3JnzDZyAeGAAAAJgKyK15Csit
eQAAAAtzc2gtZWQyNTUxOQAAACAYfora4TPkBvMx2eF/FwcBKEaoVuEYk1F3JnzDZyAeGA
AAAECscSo1FYZdIrWMUboREni6VNWV929M6YrBMsb9x57WxBh+itrhM+QG8zHZ4X8XBwEo
RqhW4RiTUXcmfMNnIB4YAAAAFXNpbS1zbXAtdGVzdC1ob3N0LWtleQ==
-----END OPENSSH PRIVATE KEY-----
"#;

const USERNAME: &str = "admin";
const PASSWORD: &str = "extron";
/// The unit name the simulated SMP reports for a `unit-name` query.
const UNIT_NAME: &str = "Main Hall SMP";

/// Which SSH stream the simulated SMP answers on. Real Extron devices reply on
/// the extended-data (stderr) stream; most other devices use normal data.
#[derive(Clone, Copy)]
enum ReplyStream {
    /// Normal channel data (`SSH_MSG_CHANNEL_DATA`, stdout).
    Stdout,
    /// Extended data (`SSH_MSG_CHANNEL_EXTENDED_DATA`, stderr) — how a real
    /// Extron SMP answers, which regressed when the transport read only stdout.
    Stderr,
}

/// A running simulated SMP. Holds the server task so it stays alive for the test
/// and aborts it on drop, so no accept loop leaks between tests.
struct SimulatedSmp {
    port: u16,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for SimulatedSmp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl SimulatedSmp {
    /// A device config pointed at this server, authenticating with `password`.
    fn device_config(&self, password: &str) -> DeviceConfig {
        DeviceConfig {
            id: "sim".into(),
            host: "127.0.0.1".into(),
            port: self.port,
            username: USERNAME.into(),
            password: password.into(),
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(5),
        }
    }
}

use std::sync::Once;
static INIT: Once = Once::new();
fn init_tracing() {
    INIT.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
        let formatting_layer = BunyanFormattingLayer::new(
            "sismatic-core".into(),
            //tracing_subscriber::fmt::TestWriter::default, // see gotcha #2
            std::io::stdout,
        );
        Registry::default()
            .with(env_filter)
            .with(JsonStorageLayer)
            .with(formatting_layer)
            .init();
    });
}

/// Bind a real SSH server that accepts only keyboard-interactive auth on a
/// random loopback port, answering queries on the given stream. Returns once it
/// is listening.
async fn spawn_smp(reply: ReplyStream) -> SimulatedSmp {
    init_tracing();
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();

    // Advertise only keyboard-interactive, like an Extron SMP.
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::KeyboardInteractive);

    let config = Arc::new(server::Config {
        methods,
        // Default is a 1s constant-time rejection delay; shorten it so the
        // password->keyboard-interactive fallback isn't paced by a full second.
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::from_millis(1)),
        keys: vec![PrivateKey::from_openssh(HOST_KEY).expect("valid host key")],
        ..Default::default()
    });

    let mut smp = SmpServer { reply };
    let server = tokio::spawn(async move {
        let _ = smp.run_on_socket(config, &listener).await;
    });

    SimulatedSmp { port, server }
}

/// The server side: one [`SmpHandler`] per connection.
struct SmpServer {
    reply: ReplyStream,
}

impl Server for SmpServer {
    type Handler = SmpHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> SmpHandler {
        SmpHandler { reply: self.reply }
    }
}

/// Reproduces the SMP auth handshake: `password` is never accepted, and
/// `keyboard-interactive` sends one "Password:" prompt whose answer must match
/// the configured credential.
struct SmpHandler {
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
                let ok = user == USERNAME && answers.next().as_deref() == Some(PASSWORD.as_bytes());
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
        Ok(())
    }

    /// The SIS command interface: each request the client writes is answered
    /// with the device's framed reply. Requests the simulator does not model are
    /// ignored, which surfaces as a command timeout on the client side.
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(reply) = self.reply_to(data) {
            match self.reply {
                ReplyStream::Stdout => session.data(channel, reply.into_bytes())?,
                // ext == 1 is stderr; this is the path a real Extron SMP uses.
                ReplyStream::Stderr => session.extended_data(channel, 1, reply.into_bytes())?,
            }
        }
        Ok(())
    }
}

impl SmpHandler {
    /// Map a SIS request payload to the SMP's framed reply, deriving the request
    /// bytes from the real instruction so the simulator can't drift from the
    /// protocol. Extend this match to model more queries.
    ///
    /// Assumes each request arrives as a single write, which holds over loopback.
    fn reply_to(&self, request: &[u8]) -> Option<String> {
        // `unit-name`: ESC "CN" CR  ->  "CN" CR LF <name> CR CR
        if request == Query::UnitName.instruction().payload.as_bytes() {
            return Some(format!("CN\r\n{UNIT_NAME}\r\r"));
        }
        None
    }
}

#[tokio::test]
async fn keyboard_interactive_auth_succeeds() {
    let smp = spawn_smp(ReplyStream::Stdout).await;

    RusshConnector
        .connect(&smp.device_config(PASSWORD))
        .await
        .expect("keyboard-interactive auth should succeed");
}

#[tokio::test]
async fn unit_name_query_round_trips() {
    let smp = spawn_smp(ReplyStream::Stdout).await;
    // Drive the query through the same Device stack production uses, so the whole
    // path — RusshConnector, Controller, framed_text parser — runs over real SSH.
    let device = Device::new(smp.device_config(PASSWORD), Arc::new(RusshConnector));

    let value = device
        .run(&Query::UnitName.instruction())
        .await
        .expect("unit-name query should succeed");

    assert_eq!(value, Value::Text(UNIT_NAME.into()));
}

#[tokio::test]
async fn unit_name_query_over_stderr_round_trips() {
    // Regression: real Extron SMP devices reply on the SSH extended-data
    // (stderr) stream. The transport must read stderr as well as stdout, or the
    // query times out (see the `into_stream` note in the ssh transport).
    let smp = spawn_smp(ReplyStream::Stderr).await;
    let device = Device::new(smp.device_config(PASSWORD), Arc::new(RusshConnector));

    let value = device
        .run(&Query::UnitName.instruction())
        .await
        .expect("unit-name query should succeed when the device replies on stderr");

    assert_eq!(value, Value::Text(UNIT_NAME.into()));
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let smp = spawn_smp(ReplyStream::Stdout).await;

    match RusshConnector
        .connect(&smp.device_config("not-the-password"))
        .await
    {
        Err(ConnectError::Failed(_)) => {}
        Err(other) => panic!("expected an auth failure, got {other:?}"),
        Ok(_) => panic!("expected an auth failure, but the connection succeeded"),
    }
}
