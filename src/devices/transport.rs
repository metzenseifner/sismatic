//! The byte-level seam between protocol logic and the network.
//!
//! A [`Transport`] is one open, bidirectional channel to a device: write the
//! bytes of a request, read the bytes of the reply. It knows nothing about SIS
//! framing — the [`crate::recorder::protocol`] parsers turn those bytes into
//! typed values, and the controller (built on top) drives the write/read loop.
//!
//! Keeping the trait this small makes the test double trivial: real SSH is one
//! impl, [`fake::FakeTransport`] is the other, and everything above this seam
//! can be exercised with no network.

use std::fmt;

use async_trait::async_trait;

/// A failure on an open channel. Connecting is the connector's concern, so this
/// only covers errors that happen once bytes are flowing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The underlying channel failed (reset, broken pipe, SSH error, ...).
    Io(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "transport i/o error: {e}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// One open, bidirectional byte channel to a device.
#[async_trait]
pub trait Transport: Send {
    /// Write every byte of `bytes`, or fail.
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    /// Read whatever bytes are available into `buf`, returning how many were
    /// read. `Ok(0)` means the peer closed the channel.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
}

#[cfg(any(test, feature = "testing"))]
pub mod fake;
