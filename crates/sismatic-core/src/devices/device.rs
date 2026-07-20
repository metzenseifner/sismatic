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

use std::fmt;
use std::sync::Arc;

use tokio::sync::Mutex;

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
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::Connect(e) => write!(f, "connect failed: {e}"),
            DeviceError::Command(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// One device and its cached connection.
pub struct Device {
    config: DeviceConfig,
    connector: Arc<dyn Connector>,
    conn: Mutex<Option<Controller>>,
}

impl Device {
    /// Create a device that will connect lazily on its first command.
    pub fn new(config: DeviceConfig, connector: Arc<dyn Connector>) -> Self {
        Self {
            config,
            connector,
            conn: Mutex::new(None),
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

    /// Run `instruction`, opening or reusing the warm connection as needed.
    pub async fn run(&self, instruction: &Instruction) -> Result<Value, DeviceError> {
        let mut guard = self.conn.lock().await;
        let mut reconnected = false;
        loop {
            let was_cached = guard.is_some();
            if guard.is_none() {
                *guard = Some(self.connect().await?);
            }
            let controller = guard.as_mut().expect("connection just ensured");

            match controller.run(instruction).await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    *guard = None; // the channel may be desynced; discard it
                    if was_cached && !reconnected {
                        // The cached connection may have been closed while idle;
                        // heal transparently by retrying once on a fresh one.
                        reconnected = true;
                        continue;
                    }
                    return Err(DeviceError::Command(err));
                }
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
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::transport::Transport;
    use crate::devices::transport::fake::FakeTransport;
    use crate::protocol::instructions::query::Query;

    const PORT_REPLY: &str = "22023\r\n";

    fn config(connect_ms: u64) -> DeviceConfig {
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
        let device = Device::new(config(500), Arc::new(FailingConnector));
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

    struct FailingConnector;

    #[async_trait]
    impl Connector for FailingConnector {
        async fn connect(
            &self,
            _config: &DeviceConfig,
        ) -> Result<Box<dyn Transport>, ConnectError> {
            Err(ConnectError::Failed("refused".into()))
        }
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
}
