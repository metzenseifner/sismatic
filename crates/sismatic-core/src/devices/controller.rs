//! Driving one SIS exchange over an open [`Transport`].
//!
//! A [`Controller`] is the glue between the typed [`Instruction`] catalog and a
//! byte channel: it writes an instruction's payload, then feeds the device's
//! reply to that instruction's streaming parser until a complete [`Value`] is
//! parsed. It owns the connection but no policy — reconnecting, caching, and
//! locking are the device layer's job. The only time limit it enforces is
//! `exchange_timeout`, the deadline for a single exchange.
//!
//! The reply is accumulated as raw bytes and only the valid-UTF-8 prefix is
//! handed to the parser each round, so a reply arriving in fragments — even one
//! that splits a multi-byte character across two reads — parses correctly.

use std::fmt;
use std::time::Duration;

use crate::protocol::SisError;
use crate::protocol::Step;
use crate::protocol::Value;
use crate::protocol::instructions::Instruction;

use super::transport::{Transport, TransportError};

/// Why a single exchange failed. The device layer reads these to decide whether
/// the cached connection is still usable (it is not, after a transport error or
/// an early close — but it is, after a [`Rejected`](ControllerError::Rejected)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerError {
    /// No complete reply arrived within `exchange_timeout`.
    Timeout {
        instruction: String,
        after: Duration,
    },
    /// The device answered, and the answer was a refusal.
    ///
    /// The odd one out among these variants, and the distinction the device
    /// layer turns on: this is a *complete* exchange whose outcome happens to be
    /// "no". The channel is in sync and the connection is fine, so unlike every
    /// other variant it is not evidence against the connection.
    Rejected {
        instruction: String,
        error: SisError,
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
            ControllerError::Rejected { instruction, error } => {
                write!(f, "device refused `{instruction}` with {error}")
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
    exchange_timeout: Duration,
}

impl Controller {
    /// Wrap an open transport. `exchange_timeout` bounds each [`run`](Self::run).
    pub fn new(transport: Box<dyn Transport>, exchange_timeout: Duration) -> Self {
        Self {
            transport,
            exchange_timeout,
        }
    }

    /// Send `instruction` and return the parsed reply, or fail if the exchange
    /// times out, the channel closes, or the transport errors.
    pub async fn run(&mut self, instruction: &Instruction) -> Result<Value, ControllerError> {
        match tokio::time::timeout(self.exchange_timeout, self.exchange(instruction)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(ControllerError::Timeout {
                instruction: instruction.name.clone(),
                after: self.exchange_timeout,
            }),
        }
    }

    /// The untimed write-then-read-until-complete loop. [`run`](Self::run) wraps
    /// this in the exchange timeout.
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
            let reply = valid_prefix(&acc);

            // Before the instruction's own parser, not after it, and the order
            // is the whole point. A refusal is a bare `E13\r\n`, which a parser
            // expecting a value either cannot match — and then reads until
            // `exchange_timeout` on a reply that already arrived — or, for the
            // free-text fields, matches all too well and stores `"E13"` as the
            // unit's name. Asking this first is what makes both impossible.
            if let Some(error) = SisError::in_reply(reply) {
                return Err(ControllerError::Rejected {
                    instruction: instruction.name.clone(),
                    error,
                });
            }

            if let Step::Done(value) = instruction.parse_step(reply) {
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
        let fake = FakeTransport::with_reads(["22023\r\n"]);
        let writes = fake.writes();
        let mut ctrl = controller(fake, 500);

        let value = ctrl.run(&instr).await.unwrap();
        assert_eq!(value, Value::Port(22023));
        assert_eq!(&*writes.lock().unwrap(), instr.payload.as_bytes());
    }

    #[tokio::test]
    async fn tolerates_a_reply_arriving_one_byte_at_a_time() {
        let instr = Query::MacAddress.instruction();
        let reply = "00-05-A6-1B-2C-3D\r\n";
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

    /// The regression this whole path exists for. `Exhausted::Stall` is the
    /// point: nothing follows the refusal, so if it were not recognised the
    /// only way out would be the timeout — which is exactly what a fleet of
    /// SMP 351s did on every poll of a field they do not implement.
    #[tokio::test]
    async fn a_refusal_ends_the_exchange_instead_of_waiting_out_the_timeout() {
        let instr = Query::RtmpStream2LiveState.instruction();
        let fake = FakeTransport::with_reads(["E13\r\n"]).on_exhausted(Exhausted::Stall);
        let mut ctrl = controller(fake, 500);

        assert_eq!(
            ctrl.run(&instr).await.unwrap_err(),
            ControllerError::Rejected {
                instruction: instr.name.clone(),
                error: SisError { code: 13 },
            }
        );
    }

    /// A refusal must win over the instruction's own parser, not merely be
    /// consulted when that parser has nothing to say. `plain_text` accepts any
    /// line, so left to itself it reads `E13` as the unit's *name* and stores
    /// it — a failure that never times out and never logs, which is worse than
    /// the one above.
    #[tokio::test]
    async fn a_refusal_is_not_mistaken_for_a_free_text_value() {
        let instr = Query::UnitName.instruction();
        let fake = FakeTransport::with_reads(["E13\r\n"]).on_exhausted(Exhausted::Stall);
        let mut ctrl = controller(fake, 500);

        assert_eq!(
            ctrl.run(&instr).await.unwrap_err(),
            ControllerError::Rejected {
                instruction: instr.name.clone(),
                error: SisError { code: 13 },
            }
        );
    }

    /// The other side of that coin: the anchor has to hold when the refusal is
    /// only a substring, or naming a room after a lecture hall breaks its poll.
    #[tokio::test]
    async fn a_value_containing_an_error_code_is_still_a_value() {
        let instr = Query::UnitName.instruction();
        let fake = FakeTransport::with_reads(["HALL E13\r\n"]).on_exhausted(Exhausted::Stall);
        let mut ctrl = controller(fake, 500);

        assert_eq!(
            ctrl.run(&instr).await.unwrap(),
            Value::Text("HALL E13".into())
        );
    }

    /// A refusal arrives in fragments like anything else, and the intermediate
    /// buffers (`E`, `E1`, `E13`) must not be mistaken for the finished token —
    /// nor for a reason to keep reading once it *is* finished.
    #[tokio::test]
    async fn recognises_a_refusal_that_arrives_one_byte_at_a_time() {
        let instr = Query::RtmpBackupStream3LiveState.instruction();
        let fake = FakeTransport::with_reads("E22\r\n".chars().map(|c| c.to_string()))
            .on_exhausted(Exhausted::Stall);
        let mut ctrl = controller(fake, 500);

        assert_eq!(
            ctrl.run(&instr).await.unwrap_err(),
            ControllerError::Rejected {
                instruction: instr.name.clone(),
                error: SisError { code: 22 },
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
