//! Driving one SIS exchange over an open [`Transport`].
//!
//! A [`Controller`] is the glue between the typed [`Instruction`] catalog and a
//! byte channel: it writes an instruction's payload, then feeds the device's
//! reply to that instruction's streaming parser until a complete [`Value`] is
//! parsed. It owns the connection but no policy — reconnecting, caching, and
//! locking are the device layer's job. The only time limit it enforces is
//! `command_timeout`, the deadline for a single exchange.
//!
//! The reply is accumulated as raw bytes and only the valid-UTF-8 prefix is
//! handed to the parser each round, so a reply arriving in fragments — even one
//! that splits a multi-byte character across two reads — parses correctly.

use std::fmt;
use std::time::Duration;

use crate::protocol::Step;
use crate::protocol::Value;
use crate::protocol::instructions::Instruction;

use super::transport::{Transport, TransportError};

/// Why a single exchange failed. The device layer reads these to decide whether
/// the cached connection is still usable (it is not, after a transport error or
/// an early close).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerError {
    /// No complete reply arrived within `command_timeout`.
    Timeout {
        instruction: String,
        after: Duration,
    },
    /// The channel closed before a complete reply was parsed.
    ConnectionClosed { instruction: String },
    /// The underlying transport failed mid-exchange.
    Transport {
        instruction: String,
        source: TransportError,
    },
}

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControllerError::Timeout { instruction, after } => {
                write!(f, "`{instruction}` timed out after {after:?}")
            }
            ControllerError::ConnectionClosed { instruction } => {
                write!(f, "channel closed during `{instruction}`")
            }
            ControllerError::Transport {
                instruction,
                source,
            } => {
                write!(f, "`{instruction}`: {source}")
            }
        }
    }
}

impl std::error::Error for ControllerError {}

/// Owns one open connection and runs instructions over it.
pub struct Controller {
    transport: Box<dyn Transport>,
    command_timeout: Duration,
}

impl Controller {
    /// Wrap an open transport. `command_timeout` bounds each [`run`](Self::run).
    pub fn new(transport: Box<dyn Transport>, command_timeout: Duration) -> Self {
        Self {
            transport,
            command_timeout,
        }
    }

    /// Send `instruction` and return the parsed reply, or fail if the exchange
    /// times out, the channel closes, or the transport errors.
    pub async fn run(&mut self, instruction: &Instruction) -> Result<Value, ControllerError> {
        match tokio::time::timeout(self.command_timeout, self.exchange(instruction)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(ControllerError::Timeout {
                instruction: instruction.name.clone(),
                after: self.command_timeout,
            }),
        }
    }

    /// The untimed write-then-read-until-complete loop. [`run`](Self::run) wraps
    /// this in the command timeout.
    async fn exchange(&mut self, instruction: &Instruction) -> Result<Value, ControllerError> {
        self.transport
            .write_all(instruction.payload.as_bytes())
            .await
            .map_err(|source| ControllerError::Transport {
                instruction: instruction.name.clone(),
                source,
            })?;

        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = self.transport.read(&mut buf).await.map_err(|source| {
                ControllerError::Transport {
                    instruction: instruction.name.clone(),
                    source,
                }
            })?;
            if n == 0 {
                return Err(ControllerError::ConnectionClosed {
                    instruction: instruction.name.clone(),
                });
            }
            acc.extend_from_slice(&buf[..n]);

            if let Step::Done(value) = instruction.parse_step(valid_prefix(&acc)) {
                return Ok(value);
            }
        }
    }
}

/// The longest UTF-8-valid prefix of `bytes`. Trailing bytes of an incomplete
/// multi-byte character are withheld until the next read completes them.
fn valid_prefix(bytes: &[u8]) -> &str {
    let end = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => e.valid_up_to(),
    };
    std::str::from_utf8(&bytes[..end]).expect("prefix is valid up to valid_up_to()")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::transport::fake::{Exhausted, FakeTransport};
    use crate::protocol::MacAddr;
    use crate::protocol::instructions::query::Query;

    fn controller(transport: FakeTransport, timeout_ms: u64) -> Controller {
        Controller::new(Box::new(transport), Duration::from_millis(timeout_ms))
    }

    #[tokio::test]
    async fn runs_instruction_and_writes_payload() {
        let instr = Query::SshPort.instruction();
        let fake = FakeTransport::with_reads(["BPMAP\r\n22023\r\r"]);
        let writes = fake.writes();
        let mut ctrl = controller(fake, 500);

        let value = ctrl.run(&instr).await.unwrap();
        assert_eq!(value, Value::Port(22023));
        assert_eq!(&*writes.lock().unwrap(), instr.payload.as_bytes());
    }

    #[tokio::test]
    async fn tolerates_a_reply_arriving_one_byte_at_a_time() {
        let instr = Query::MacAddress.instruction();
        let reply = "CH\r\n00-05-A6-1B-2C-3D\r\r";
        let fake = FakeTransport::with_reads(reply.chars().map(|c| c.to_string()));
        let mut ctrl = controller(fake, 500);

        assert_eq!(
            ctrl.run(&instr).await.unwrap(),
            Value::Mac(MacAddr([0x00, 0x05, 0xA6, 0x1B, 0x2C, 0x3D]))
        );
    }

    #[tokio::test]
    async fn times_out_when_no_complete_reply_arrives() {
        let instr = Query::SshPort.instruction();
        // A partial reply, then the channel stalls forever.
        let fake = FakeTransport::with_reads(["BPMAP\r\n220"]).on_exhausted(Exhausted::Stall);
        let mut ctrl = controller(fake, 20);

        assert_eq!(
            ctrl.run(&instr).await.unwrap_err(),
            ControllerError::Timeout {
                instruction: instr.name.clone(),
                after: Duration::from_millis(20),
            }
        );
    }

    #[tokio::test]
    async fn errors_when_channel_closes_before_completion() {
        let instr = Query::SshPort.instruction();
        // Partial reply, then close (FakeTransport's default on exhaustion).
        let fake = FakeTransport::with_reads(["BPMAP\r\n220"]);
        let mut ctrl = controller(fake, 500);

        assert_eq!(
            ctrl.run(&instr).await.unwrap_err(),
            ControllerError::ConnectionClosed {
                instruction: instr.name.clone(),
            }
        );
    }
}
