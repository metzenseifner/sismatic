//! A single device: its resolved config plus one warm, self-healing connection.
//!
//! A [`Device`] keeps at most one open connection and reuses it across calls, so
//! the expensive SSH handshake is paid once. The connection lives behind an
//! async mutex: commands to the *same* device are serialised (the SIS channel is
//! a single command stream), while different devices run in parallel because
//! they hold different locks.
//!
//! The connection is self-healing. On any failed exchange the suspect
//! connection is dropped; if that connection had been cached (and so may have
//! been closed server-side while idle), the command is retried once on a fresh
//! connection. A failure on a freshly-opened connection is surfaced rather than
//! retried, so a genuinely unreachable device fails fast instead of looping.
//!
//! # The cold gate
//!
//! "Fails fast" is only true of the *first* caller. Because a device holds one
//! connection, every caller that wants a down device would otherwise pay its own
//! [`connect_timeout`] to learn what the previous one just learned — and they pay
//! it serially, behind this mutex. A fleet poller running one loop per
//! `(device, field)` turns a single unreachable SMP into dozens of full connect
//! timeouts per round, and rediscovers the same fact on every round forever.
//!
//! So a failed dial is *remembered*: it arms a gate for [`cold_backoff`], and
//! callers arriving inside that window get [`DeviceError::Cold`] without a dial
//! being attempted. One dial per window tests the device on everyone's behalf;
//! the rest fail in microseconds. The gate lives here, next to the connection it
//! describes, so no caller needs to know it exists — [`run`] keeps the signature
//! it always had, and the knowledge does not leak upward into schedulers.
//!
//! [`probe`] is the deliberate exception: it dials *through* the gate. That is
//! what [`SisKeepalive`] uses, which makes the division of labor exact — for an
//! eager device, the supervisor is the one component that re-dials a down device
//! (on its own `eager_retry` cadence), and the gate holds off everybody else.
//!
//! [`connect_timeout`]: super::config::DeviceConfig::connect_timeout
//! [`cold_backoff`]: super::config::DeviceConfig::cold_backoff
//! [`run`]: Device::run
//! [`probe`]: Device::probe
//! [`SisKeepalive`]: super::sis_keepalive::SisKeepalive

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::protocol::Value;
use crate::protocol::instructions::Instruction;

use super::config::DeviceConfig;
use super::connector::{ConnectError, Connector};
use super::controller::{Controller, ControllerError};

/// Why a command against a device failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// Opening a connection failed.
    Connect(ConnectError),
    /// The exchange failed on an established connection.
    Command(ControllerError),
    /// No connection was attempted at all: an earlier dial failed and the
    /// device's [`cold_backoff`] window has not closed yet. Carries the time
    /// left on it, so a caller can say how long it is choosing not to wait.
    ///
    /// This is the cheap error. It costs no network round-trip and no
    /// `connect_timeout`, and it says nothing new — the dial that armed the gate
    /// is the event worth reporting, and it has already been reported by whoever
    /// made it. Callers that log per-attempt should treat it accordingly.
    ///
    /// [`cold_backoff`]: super::config::DeviceConfig::cold_backoff
    Cold { retry_in: Duration },
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::Connect(e) => write!(f, "connect failed: {e}"),
            DeviceError::Command(e) => write!(f, "{e}"),
            DeviceError::Cold { retry_in } => {
                write!(f, "device is cold; dialing again possible in {retry_in:?}")
            }
        }
    }
}

impl std::error::Error for DeviceError {}

/// Everything the connection mutex guards, kept in one value so the cached
/// connection and what we believe about reaching it cannot be read or updated
/// out of step.
///
/// Invariant: `cold_until` is only ever `Some` while `conn` is `None`. A dial
/// that succeeds clears the gate, and nothing arms it while a connection is
/// held — so "we have a connection" and "we are refusing to make one" are never
/// both true.
#[derive(Default)]
struct Link {
    conn: Option<Controller>,
    cold_until: Option<Instant>,
}

/// What a device's connection looks like *right now*, without dialing.
///
/// A snapshot for an operator, not a decision input. It is stale the instant it
/// is taken — nothing here reserves the connection it describes — so a caller
/// that wants to *use* a device still calls [`Device::run`] and handles the
/// error. What this is for is the status dot on a dashboard: the four states
/// are the four things an operator can usefully be told, and each one implies a
/// different next move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// A connection is open and idle. The next command reuses it.
    Warm,
    /// A command is in flight on this device.
    ///
    /// A distinct state rather than folded into [`Warm`](Connectivity::Warm),
    /// because it is what the observation can actually support: the connection
    /// lock is held, so this device either has a warm connection or is dialing
    /// one, and which of the two cannot be known without waiting for the answer
    /// — which is exactly what a status read must not do.
    Busy,
    /// No connection is open and nothing says one would fail. The ordinary
    /// resting state of a device that is not marked `eager`.
    Cold,
    /// No connection, and a recent dial failed: the cold gate is shut, so a
    /// command issued now fails without even dialing.
    ///
    /// The one state that says *the device is down* rather than merely idle,
    /// which is the distinction [`Cold`](Connectivity::Cold) cannot draw.
    Gated,
}

/// Whether a call is allowed to dial a device the cold gate is holding shut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dial {
    /// Honor the gate: fail with [`DeviceError::Cold`] rather than dial. What
    /// ordinary callers want — the whole point is that they stop paying for a
    /// dial someone else has already proven pointless.
    WhenWarm,
    /// Dial regardless. Reserved for the keep-warm supervisor, whose entire job
    /// is re-testing a device the gate is holding shut.
    Always,
}

/// One device and its cached connection.
pub struct Device {
    config: DeviceConfig,
    connector: Arc<dyn Connector>,
    link: Mutex<Link>,
}

impl Device {
    /// Create a device that will connect lazily on its first command.
    pub fn new(config: DeviceConfig, connector: Arc<dyn Connector>) -> Self {
        Self {
            config,
            connector,
            link: Mutex::new(Link::default()),
        }
    }

    /// This device's id.
    pub fn id(&self) -> &str {
        &self.config.id
    }

    /// This device's resolved config.
    pub fn config(&self) -> &DeviceConfig {
        &self.config
    }

    /// This device's connection state, without dialing and without waiting.
    ///
    /// # Why this reads the lock rather than a mirrored flag
    ///
    /// The obvious alternative is an `AtomicU8` updated alongside `link`. It
    /// would answer [`Busy`](Connectivity::Busy) devices more precisely, and it
    /// would be wrong sooner or later: `conn` and `cold_until` are written from
    /// five places between `exec` and `dial`, and a mirror is five chances for
    /// the two to disagree — on a value whose
    /// whole job is to be believed. Reading the real state has no such failure
    /// mode, because there is only one state.
    ///
    /// `try_lock` and not `lock().await`, because a status read must never
    /// queue behind an SSH exchange. A `GET` over a fleet mid-poll would
    /// otherwise take `command_timeout` per busy device, and an endpoint that
    /// reports on a slow device by *being* slow is the failure it exists to
    /// describe. A held lock is not a missing answer, either — it means a
    /// command is running, which is [`Busy`](Connectivity::Busy).
    ///
    /// Synchronous, so it composes into an `async` caller without an await
    /// point and cannot accidentally become a blocking one.
    pub fn connectivity(&self) -> Connectivity {
        let Ok(link) = self.link.try_lock() else {
            return Connectivity::Busy;
        };
        if link.conn.is_some() {
            return Connectivity::Warm;
        }
        // A gate armed in the past has expired; the next caller will dial
        // through it, so reporting the device as down would be a stale answer
        // rather than a current one.
        match link.cold_until {
            Some(until) if Instant::now() < until => Connectivity::Gated,
            _ => Connectivity::Cold,
        }
    }

    /// Run `instruction`, opening or reusing the warm connection as needed.
    ///
    /// Fails with [`DeviceError::Cold`], without dialing, if a recent dial
    /// failed and the device's [`cold_backoff`] window is still open.
    ///
    /// [`cold_backoff`]: super::config::DeviceConfig::cold_backoff
    pub async fn run(&self, instruction: &Instruction) -> Result<Value, DeviceError> {
        self.exec(instruction, Dial::WhenWarm).await
    }

    /// Run `instruction`, dialing even if the cold gate is shut.
    ///
    /// The supervisor's entry point. [`SisKeepalive`] exists to keep re-dialing
    /// a device that is down, so gating it would be self-defeating: it would
    /// stall behind a window it is itself responsible for reopening, and an
    /// eager device's real retry cadence would become the larger of
    /// [`eager_retry`] and [`cold_backoff`] rather than the one the operator set.
    ///
    /// The result is a clean split for an eager device — one component dials a
    /// cold device, on a cadence named for that purpose, and every other caller
    /// rides on the verdict it publishes into the gate.
    ///
    /// [`SisKeepalive`]: super::sis_keepalive::SisKeepalive
    /// [`eager_retry`]: super::config::DeviceConfig::eager_retry
    /// [`cold_backoff`]: super::config::DeviceConfig::cold_backoff
    pub async fn probe(&self, instruction: &Instruction) -> Result<Value, DeviceError> {
        self.exec(instruction, Dial::Always).await
    }

    /// The shared body of [`run`](Self::run) and [`probe`](Self::probe): ensure a
    /// connection (subject to `dial`), then exchange on it, healing once through
    /// a connection that had been cached.
    async fn exec(&self, instruction: &Instruction, dial: Dial) -> Result<Value, DeviceError> {
        let mut link = self.link.lock().await;
        let mut reconnected = false;
        loop {
            let was_cached = link.conn.is_some();
            if link.conn.is_none() {
                // Split in two statements, not `link.conn = Some(self.dial(&mut
                // link, ..).await?)`: the borrow for the argument has to end
                // before the one for the assignment begins.
                let controller = self.dial(&mut link, dial).await?;
                link.conn = Some(controller);
            }
            let controller = link.conn.as_mut().expect("connection just ensured");

            match controller.run(instruction).await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    link.conn = None; // the channel may be desynced; discard it
                    if was_cached && !reconnected {
                        // The cached connection may have been closed while idle;
                        // heal transparently by retrying once on a fresh one.
                        reconnected = true;
                        continue;
                    }
                    // Deliberately does *not* arm the cold gate. The dial
                    // succeeded, so the device is reachable; what failed was one
                    // exchange, and attributing that to the device would let a
                    // single misbehaving instruction gate every other caller —
                    // including the fields that are answering perfectly well.
                    return Err(DeviceError::Command(err));
                }
            }
        }
    }

    /// Open a fresh connection, consulting and then updating the cold gate.
    ///
    /// This is the only place `cold_until` is written, which is what keeps its
    /// invariant local: it is armed exactly when a dial fails and cleared
    /// exactly when one succeeds.
    async fn dial(&self, link: &mut Link, dial: Dial) -> Result<Controller, DeviceError> {
        if let (Dial::WhenWarm, Some(until)) = (dial, link.cold_until) {
            let now = Instant::now();
            if now < until {
                return Err(DeviceError::Cold {
                    retry_in: until - now,
                });
            }
        }

        match self.connect().await {
            Ok(controller) => {
                link.cold_until = None;
                Ok(controller)
            }
            Err(err) => {
                // `None` means the operator turned the gate off, so a failure
                // teaches the next caller nothing and everyone keeps dialing.
                link.cold_until = self
                    .config
                    .cold_backoff
                    .map(|backoff| Instant::now() + backoff);
                Err(err)
            }
        }
    }

    /// Open a fresh connection, enforcing the device's connect timeout.
    async fn connect(&self) -> Result<Controller, DeviceError> {
        let dial = self.connector.connect(&self.config);
        let transport = match tokio::time::timeout(self.config.connect_timeout, dial).await {
            Ok(Ok(transport)) => transport,
            Ok(Err(e)) => return Err(DeviceError::Connect(e)),
            Err(_elapsed) => {
                return Err(DeviceError::Connect(ConnectError::Timeout(
                    self.config.connect_timeout,
                )));
            }
        };
        Ok(Controller::new(transport, self.config.command_timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::transport::Transport;
    use crate::devices::transport::fake::FakeTransport;
    use crate::protocol::instructions::query::Query;

    const PORT_REPLY: &str = "22023\r\n";

    fn config(connect_ms: u64) -> DeviceConfig {
        gated_config(connect_ms, None)
    }

    /// `config`, plus a cold gate. `None` leaves the gate off, which is how the
    /// device behaved before it existed — the tests that predate the gate use it
    /// so they keep asserting on dialing alone.
    fn gated_config(connect_ms: u64, cold_backoff: Option<Duration>) -> DeviceConfig {
        DeviceConfig {
            id: "test".into(),
            host: "10.0.0.1".into(),
            port: 22023,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(connect_ms),
            command_timeout: Duration::from_millis(500),
            eager: false,
            sis_keepalive: None,
            eager_retry: None,
            cold_backoff,
        }
    }

    fn port_query() -> Instruction {
        Query::SshPort.instruction()
    }

    #[tokio::test]
    async fn opens_once_and_reuses_the_warm_connection() {
        // One connection that can answer two queries.
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY, PORT_REPLY])
        }));
        let opens = connector.opens_handle();
        let device = Device::new(config(500), connector);

        assert_eq!(device.run(&port_query()).await.unwrap(), Value::Port(22023));
        assert_eq!(device.run(&port_query()).await.unwrap(), Value::Port(22023));
        assert_eq!(opens.load(Ordering::SeqCst), 1, "second call must reuse");
    }

    #[tokio::test]
    async fn reconnects_transparently_after_a_stale_connection_fails() {
        // Each connection answers exactly once, then closes.
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let opens = connector.opens_handle();
        let device = Device::new(config(500), connector);

        assert_eq!(device.run(&port_query()).await.unwrap(), Value::Port(22023));
        // The cached connection is now exhausted; the device should heal.
        assert_eq!(device.run(&port_query()).await.unwrap(), Value::Port(22023));
        assert_eq!(
            opens.load(Ordering::SeqCst),
            2,
            "stale connection must reconnect"
        );
    }

    #[tokio::test]
    async fn surfaces_error_when_a_fresh_connection_fails() {
        // A connection that closes immediately with no reply.
        let connector = Arc::new(CountingConnector::new(FakeTransport::new));
        let opens = connector.opens_handle();
        let device = Device::new(config(500), connector);

        let err = device.run(&port_query()).await.unwrap_err();
        assert!(matches!(
            err,
            DeviceError::Command(ControllerError::ConnectionClosed { .. })
        ));
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1,
            "must not loop on a fresh failure"
        );
    }

    #[tokio::test]
    async fn surfaces_a_connect_error() {
        let device = Device::new(config(500), Arc::new(FailingConnector::new()));
        assert_eq!(
            device.run(&port_query()).await.unwrap_err(),
            DeviceError::Connect(ConnectError::Failed("refused".into()))
        );
    }

    #[tokio::test]
    async fn connect_that_never_completes_times_out() {
        let device = Device::new(config(20), Arc::new(StallingConnector));
        assert_eq!(
            device.run(&port_query()).await.unwrap_err(),
            DeviceError::Connect(ConnectError::Timeout(Duration::from_millis(20)))
        );
    }

    #[tokio::test]
    async fn concurrent_commands_share_one_connection() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY, PORT_REPLY])
        }));
        let opens = connector.opens_handle();
        let device = Arc::new(Device::new(config(500), connector));

        let a = Arc::clone(&device);
        let b = Arc::clone(&device);
        let (q1, q2) = (port_query(), port_query());
        let (ra, rb) = tokio::join!(a.run(&q1), b.run(&q2));

        assert_eq!(ra.unwrap(), Value::Port(22023));
        assert_eq!(rb.unwrap(), Value::Port(22023));
        assert_eq!(opens.load(Ordering::SeqCst), 1, "one connection for both");
    }

    /// A connector that refuses every dial, counting the attempts. The counter is
    /// what the cold-gate tests assert on: the gate's whole purpose is that a
    /// call *does not reach* the connector.
    struct FailingConnector {
        attempts: Arc<AtomicUsize>,
    }

    impl FailingConnector {
        fn new() -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn attempts_handle(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.attempts)
        }
    }

    #[async_trait]
    impl Connector for FailingConnector {
        async fn connect(
            &self,
            _config: &DeviceConfig,
        ) -> Result<Box<dyn Transport>, ConnectError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(ConnectError::Failed("refused".into()))
        }
    }

    /// A connector that refuses its first `failures` dials and then succeeds,
    /// answering one port query per connection.
    struct FlakyConnector {
        attempts: Arc<AtomicUsize>,
        failures: usize,
    }

    impl FlakyConnector {
        fn new(failures: usize) -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
                failures,
            }
        }

        fn attempts_handle(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.attempts)
        }
    }

    #[async_trait]
    impl Connector for FlakyConnector {
        async fn connect(
            &self,
            _config: &DeviceConfig,
        ) -> Result<Box<dyn Transport>, ConnectError> {
            let prior = self.attempts.fetch_add(1, Ordering::SeqCst);
            if prior < self.failures {
                Err(ConnectError::Failed("down".into()))
            } else {
                Ok(Box::new(FakeTransport::with_reads([PORT_REPLY])))
            }
        }
    }

    // ---- the cold gate ---------------------------------------------------

    /// A backoff long enough that nothing in a test can outlive it, for the cases
    /// that assert the gate *stays* shut.
    const HELD_SHUT: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn a_failed_dial_gates_the_next_caller() {
        let connector = Arc::new(FailingConnector::new());
        let attempts = connector.attempts_handle();
        let device = Device::new(gated_config(500, Some(HELD_SHUT)), connector);

        // The first caller pays for the discovery...
        assert_eq!(
            device.run(&port_query()).await.unwrap_err(),
            DeviceError::Connect(ConnectError::Failed("refused".into()))
        );
        // ...and every caller behind it rides on the answer instead of redialing.
        for _ in 0..5 {
            assert!(matches!(
                device.run(&port_query()).await.unwrap_err(),
                DeviceError::Cold { .. }
            ));
        }
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "the gate must collapse six callers into one dial"
        );
    }

    #[tokio::test]
    async fn the_gate_reopens_once_the_backoff_elapses() {
        let connector = Arc::new(FailingConnector::new());
        let attempts = connector.attempts_handle();
        let device = Device::new(
            gated_config(500, Some(Duration::from_millis(50))),
            connector,
        );

        device.run(&port_query()).await.unwrap_err();
        assert!(matches!(
            device.run(&port_query()).await.unwrap_err(),
            DeviceError::Cold { .. }
        ));

        tokio::time::sleep(Duration::from_millis(80)).await;

        // The window has closed, so this call re-tests the device for everyone.
        assert_eq!(
            device.run(&port_query()).await.unwrap_err(),
            DeviceError::Connect(ConnectError::Failed("refused".into()))
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_successful_dial_clears_the_gate() {
        // Refuses once, then answers — so the recovery has to survive a gate that
        // was armed by the refusal.
        let connector = Arc::new(FlakyConnector::new(1));
        let attempts = connector.attempts_handle();
        let device = Device::new(
            gated_config(500, Some(Duration::from_millis(50))),
            connector,
        );

        device.run(&port_query()).await.unwrap_err();
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(device.run(&port_query()).await.unwrap(), Value::Port(22023));
        // Cleared, not merely expired: the connection is cached and reused, which
        // a still-armed gate would have prevented from being established.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn probe_dials_through_a_shut_gate() {
        let connector = Arc::new(FailingConnector::new());
        let attempts = connector.attempts_handle();
        let device = Device::new(gated_config(500, Some(HELD_SHUT)), connector);

        device.run(&port_query()).await.unwrap_err();
        assert!(matches!(
            device.run(&port_query()).await.unwrap_err(),
            DeviceError::Cold { .. }
        ));

        // The supervisor's path ignores the window a `run` would have honored —
        // otherwise an eager device's retry cadence would be the gate's, not
        // `eager_retry`'s.
        assert_eq!(
            device.probe(&port_query()).await.unwrap_err(),
            DeviceError::Connect(ConnectError::Failed("refused".into()))
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_exchange_failure_does_not_gate_the_device() {
        // The dial succeeds and the exchange does not, which says nothing about
        // whether the device is reachable — gating on it would let one sick
        // instruction silence every other caller of a healthy device.
        let connector = Arc::new(CountingConnector::new(FakeTransport::new));
        let opens = connector.opens_handle();
        let device = Device::new(gated_config(500, Some(HELD_SHUT)), connector);

        for _ in 0..3 {
            assert!(matches!(
                device.run(&port_query()).await.unwrap_err(),
                DeviceError::Command(ControllerError::ConnectionClosed { .. })
            ));
        }
        assert_eq!(opens.load(Ordering::SeqCst), 3, "each call must still dial");
    }

    #[tokio::test]
    async fn an_unset_backoff_leaves_every_call_dialing() {
        // `cold_backoff_secs = 0` is the opt-out, and must restore exactly the
        // behavior from before the gate existed.
        let connector = Arc::new(FailingConnector::new());
        let attempts = connector.attempts_handle();
        let device = Device::new(gated_config(500, None), connector);

        for _ in 0..3 {
            assert_eq!(
                device.run(&port_query()).await.unwrap_err(),
                DeviceError::Connect(ConnectError::Failed("refused".into()))
            );
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    struct StallingConnector;

    #[async_trait]
    impl Connector for StallingConnector {
        async fn connect(
            &self,
            _config: &DeviceConfig,
        ) -> Result<Box<dyn Transport>, ConnectError> {
            std::future::pending().await
        }
    }
    // ---- connectivity ----------------------------------------------------
    //
    // The four states, driven through the real state machine rather than
    // asserted against a mirrored flag.

    #[tokio::test]
    async fn a_device_that_has_never_been_used_is_cold() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let device = Device::new(config(500), connector);
        assert_eq!(device.connectivity(), Connectivity::Cold);
    }

    #[tokio::test]
    async fn a_device_that_ran_a_command_is_warm() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let device = Device::new(config(500), connector);
        device.run(&port_query()).await.expect("the command");
        // The connection is cached for reuse, which is the whole reason a
        // device holds one.
        assert_eq!(device.connectivity(), Connectivity::Warm);
    }

    /// The state that says *down* rather than merely idle. A failed dial arms
    /// the cold gate, and until it expires a command fails without dialing —
    /// which is what an operator wants a red dot for.
    #[tokio::test]
    async fn a_device_whose_dial_failed_is_gated() {
        let device = Device::new(
            gated_config(500, Some(Duration::from_secs(3600))),
            Arc::new(FailingConnector::new()),
        );

        device.run(&port_query()).await.expect_err("the dial fails");

        assert_eq!(device.connectivity(), Connectivity::Gated);
    }

    /// ...and an *expired* gate is not `Gated`: the next caller will dial
    /// through it, so reporting the device as down would be a stale answer
    /// rather than a current one.
    #[tokio::test]
    async fn an_expired_gate_reads_as_cold_again() {
        let device = Device::new(
            gated_config(500, Some(Duration::from_millis(50))),
            Arc::new(FailingConnector::new()),
        );
        device.run(&port_query()).await.expect_err("the dial fails");
        assert_eq!(device.connectivity(), Connectivity::Gated);

        // A real sleep, as the neighbouring gate test uses: this crate's tokio
        // has no `test-util`, so there is no clock to pause.
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(device.connectivity(), Connectivity::Cold);
    }

    /// A device with no gate configured never reads as `Gated`, however many
    /// dials fail — the operator turned the gate off, so nothing is holding
    /// anyone back and "down" is not a state this device has.
    #[tokio::test]
    async fn a_device_with_no_backoff_is_never_gated() {
        let device = Device::new(config(500), Arc::new(FailingConnector::new()));
        device.run(&port_query()).await.expect_err("the dial fails");
        assert_eq!(device.connectivity(), Connectivity::Cold);
    }

    /// The property the whole design turns on: a status read never waits for a
    /// command. Were this `lock().await`, a `GET` over a fleet mid-poll would
    /// take `command_timeout` per busy device — an endpoint reporting on a slow
    /// device by *being* slow.
    #[tokio::test]
    async fn a_status_read_does_not_wait_for_an_in_flight_command() {
        // A connector that never completes, so the dial holds the lock for as
        // long as the test cares to look.
        let device = Arc::new(Device::new(config(60_000), Arc::new(StallingConnector)));

        let busy = tokio::spawn({
            let device = Arc::clone(&device);
            async move { device.run(&port_query()).await }
        });

        // Each of these reads returns immediately; the loop is waiting for the
        // spawned task to take the lock, not for the read to answer.
        for _ in 0..1000 {
            if device.connectivity() == Connectivity::Busy {
                busy.abort();
                return;
            }
            tokio::task::yield_now().await;
        }
        busy.abort();
        panic!("the command never took the lock");
    }
}
