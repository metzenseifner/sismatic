//! An in-memory [`Transport`] for exercising everything above the network seam.
//!
//! A `FakeTransport` replays a script of read chunks and records every byte
//! written. Splitting the reply into chunks lets a test control streaming
//! granularity — push it one byte at a time to prove the parser tolerates a
//! reply arriving in fragments. When the script runs out the channel either
//! closes (`Ok(0)`) or stalls forever, so timeout handling can be tested too.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{Transport, TransportError};

/// What a [`FakeTransport`] does once its scripted reads are exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhausted {
    /// Report end-of-stream (`read` returns `Ok(0)`), as if the peer hung up.
    Close,
    /// Never return from `read`, so a surrounding timeout has to fire.
    Stall,
}

/// A scripted, in-memory transport. Build one with [`FakeTransport::new`] and
/// the chaining setters, then hand it to the code under test.
pub struct FakeTransport {
    reads: VecDeque<Vec<u8>>,
    on_exhausted: Exhausted,
    written: Arc<Mutex<Vec<u8>>>,
}

impl FakeTransport {
    /// A transport with no scripted reads that closes once read.
    pub fn new() -> Self {
        Self {
            reads: VecDeque::new(),
            on_exhausted: Exhausted::Close,
            written: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build a transport whose reads replay `chunks` in order.
    pub fn with_reads<I, B>(chunks: I) -> Self
    where
        I: IntoIterator<Item = B>,
        B: Into<Vec<u8>>,
    {
        let mut t = Self::new();
        t.reads = chunks.into_iter().map(Into::into).collect();
        t
    }

    /// Append another chunk to the read script.
    pub fn push_read(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.reads.push_back(bytes.into());
        self
    }

    /// Set what happens after the scripted reads are exhausted.
    pub fn on_exhausted(mut self, behaviour: Exhausted) -> Self {
        self.on_exhausted = behaviour;
        self
    }

    /// A shared handle to the bytes written so far. Cloneable, so a test can
    /// keep it after moving the transport into the code under test.
    pub fn writes(&self) -> Arc<Mutex<Vec<u8>>> {
        Arc::clone(&self.written)
    }

    /// A snapshot copy of everything written so far.
    pub fn written_bytes(&self) -> Vec<u8> {
        self.written.lock().expect("written lock").clone()
    }
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.written
            .lock()
            .expect("written lock")
            .extend_from_slice(bytes);
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        while let Some(front) = self.reads.front_mut() {
            if front.is_empty() {
                self.reads.pop_front();
                continue;
            }
            let n = front.len().min(buf.len());
            buf[..n].copy_from_slice(&front[..n]);
            front.drain(..n);
            return Ok(n);
        }
        match self.on_exhausted {
            Exhausted::Close => Ok(0),
            Exhausted::Stall => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn read_to_string(t: &mut FakeTransport) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = t.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn records_writes_in_order() {
        let mut t = FakeTransport::new();
        t.write_all(b"Q").await.unwrap();
        t.write_all(b"2\r").await.unwrap();
        assert_eq!(t.written_bytes(), b"Q2\r");
    }

    #[tokio::test]
    async fn writes_handle_is_visible_after_move() {
        let t = FakeTransport::new();
        let handle = t.writes();
        let mut moved = t;
        moved.write_all(b"hi").await.unwrap();
        assert_eq!(&*handle.lock().unwrap(), b"hi");
    }

    #[tokio::test]
    async fn replays_read_chunks_in_order() {
        let mut t = FakeTransport::with_reads(["AB", "C"]);
        assert_eq!(read_to_string(&mut t).await, "ABC");
    }

    #[tokio::test]
    async fn read_never_exceeds_buffer_len() {
        let mut t = FakeTransport::with_reads(["ABCD"]);
        let mut buf = [0u8; 2];
        assert_eq!(t.read(&mut buf).await.unwrap(), 2);
        assert_eq!(&buf, b"AB");
        assert_eq!(t.read(&mut buf).await.unwrap(), 2);
        assert_eq!(&buf, b"CD");
        assert_eq!(t.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn closes_when_script_is_exhausted() {
        let mut t = FakeTransport::new();
        let mut buf = [0u8; 8];
        assert_eq!(t.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn stall_pends_until_a_timeout_fires() {
        let mut t = FakeTransport::new().on_exhausted(Exhausted::Stall);
        let mut buf = [0u8; 8];
        let timed_out = tokio::time::timeout(Duration::from_millis(20), t.read(&mut buf)).await;
        assert!(
            timed_out.is_err(),
            "stalled read should never resolve on its own"
        );
    }
}
