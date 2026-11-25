use std::{fmt, sync::Arc};

use crate::protocol::control_chars::swap_non_printable;
use crate::protocol::{ParseFn, Step, Value};

pub mod commands;
pub mod query;
pub mod register;
pub mod setting;
//
//
// ---- Instruction ----------------------------------------------------------

/// What an exchange does to the session it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Session {
    /// The channel is reusable afterwards, and a failure on a cached connection
    /// may be replayed once on a fresh one. Every read and every write but one.
    #[default]
    Preserved,
    /// Success ends the session (`REBOOT`). Never replayed: a lost reply most
    /// likely means the device acted, and a resend would act twice. The
    /// connection is discarded after the exchange whatever its outcome.
    Ends,
}

/// A single protocol exchange: what to send and how to interpret the reply.
#[derive(Clone)]
pub struct Instruction {
    /// Human-readable identifier, surfaced in responses and errors.
    pub name: String,
    /// Raw characters written to the channel (control characters included).
    pub payload: String,
    pub(crate) session: Session,
    parser: ParseFn,
}

impl fmt::Debug for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instruction")
            .field("name", &self.name)
            .field("payload", &swap_non_printable(&self.payload))
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Instruction {
    /// Build a user-defined instruction from a custom streaming parser. This is
    /// the extension point for instructions the catalog does not cover.
    pub fn custom(
        name: impl Into<String>,
        payload: impl Into<String>,
        parser: impl Fn(&str) -> Step<Value> + Send + Sync + 'static,
    ) -> Self {
        Instruction {
            name: name.into(),
            payload: payload.into(),
            session: Session::Preserved,
            parser: Arc::new(parser),
        }
    }

    /// Build an instruction that simply reads until `terminator` appears, then
    /// returns the whole buffer as [`Value::Text`]. Convenient for ad-hoc/raw
    /// commands where the reply shape is not modelled.
    pub fn raw_until(
        name: impl Into<String>,
        payload: impl Into<String>,
        terminator: String,
    ) -> Self {
        Instruction::custom(name, payload, move |buf| {
            if terminator.is_empty() || buf.contains(&terminator) {
                Step::Done(Value::Text(buf.to_string()))
            } else {
                Step::NeedMore
            }
        })
    }

    /// What this exchange does to the session it runs on.
    pub fn session(&self) -> Session {
        self.session
    }

    /// Feed the accumulated buffer to this instruction's parser.
    pub fn parse_step(&self, buffer: &str) -> Step<Value> {
        (self.parser)(buffer)
    }
}

/// Returned by the `FromStr` impls when a name is not in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownInstruction(pub String);

impl fmt::Display for UnknownInstruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no built-in instruction named '{}'", self.0)
    }
}

impl std::error::Error for UnknownInstruction {}
