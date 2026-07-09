//! Opening a fresh connection to a device.
//!
//! A [`Connector`] turns a [`DeviceConfig`] into an open [`Transport`]. It is
//! the injection seam for the network: production uses an SSH connector, tests
//! use [`fake::CountingConnector`], and the [`Device`](super::device::Device)
//! layer above is identical either way. Connecting is separated from running
//! commands so that the connect timeout and the command timeout stay distinct
//! concerns.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;

use super::config::DeviceConfig;
use super::transport::Transport;

/// Why opening a connection failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// Dialling the device failed (refused, DNS, auth, SSH handshake, ...).
    Failed(String),
    /// The connection did not establish within `connect_timeout`.
    Timeout(Duration),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Failed(e) => write!(f, "{e}"),
            ConnectError::Timeout(d) => write!(f, "connect timed out after {d:?}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Opens a fresh [`Transport`] to a device.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Dial the device described by `config` and return an open channel. The
    /// connect timeout is enforced by the caller, not the implementation.
    async fn connect(&self, config: &DeviceConfig) -> Result<Box<dyn Transport>, ConnectError>;
}

#[cfg(any(test, feature = "testing"))]
pub mod fake;
