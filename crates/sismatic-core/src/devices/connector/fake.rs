//! A [`Connector`] test double that counts how often it opened a connection.
//!
//! Each `connect` bumps a shared counter and builds a brand-new
//! [`FakeTransport`] from a caller-supplied factory, so a test can assert
//! whether the device layer reused its warm connection (counter stays put) or
//! reconnected (counter climbs).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::{ConnectError, Connector};
use crate::devices::config::DeviceConfig;
use crate::devices::transport::Transport;
use crate::devices::transport::fake::FakeTransport;

/// A connector that records its open count and yields a fresh transport each
/// time. The factory decides what each new connection replays.
pub struct CountingConnector {
    opens: Arc<AtomicUsize>,
    make: Arc<dyn Fn() -> FakeTransport + Send + Sync>,
}

impl CountingConnector {
    /// Build a connector whose every connection is `make()`.
    pub fn new(make: impl Fn() -> FakeTransport + Send + Sync + 'static) -> Self {
        Self {
            opens: Arc::new(AtomicUsize::new(0)),
            make: Arc::new(make),
        }
    }

    /// How many connections have been opened so far.
    pub fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    /// A shared handle to the open counter, usable after the connector is moved
    /// into a device.
    pub fn opens_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.opens)
    }
}

#[async_trait]
impl Connector for CountingConnector {
    async fn connect(&self, _config: &DeviceConfig) -> Result<Box<dyn Transport>, ConnectError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new((self.make)()))
    }
}
