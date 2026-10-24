//! A declaratively configured, simulated Extron SMP.
//!
//! # The drift problem
//!
//! A simulator is only useful if it speaks the protocol the client parses. The
//! naive version — a hand-written table of "request bytes → reply bytes" —
//! is a second, silent copy of the protocol that rots the moment a payload or a
//! parser changes, and the failure mode is a mysterious command timeout rather
//! than a compile error.
//!
//! This module has no such table. Everything is derived from the instruction
//! catalogs:
//!
//! | Concern | Derived from |
//! |---|---|
//! | Which requests exist | [`Query::ALL`], [`Command::ALL`], [`Register::ALL`], [`Setting::ALL`] |
//! | Their exact bytes | each enum's own `instruction()` constructor |
//! | Which config keys are legal | the catalogs' `FromStr` (aliases and case-insensitivity for free) |
//! | Whether a value is well-formed | the *production* parser, run over the simulator's own output |
//!
//! [`Query::ALL`]: crate::protocol::instructions::query::Query::ALL
//! [`Command::ALL`]: crate::protocol::instructions::commands::Command::ALL
//! [`Register::ALL`]: crate::protocol::instructions::register::Register::ALL
//! [`Setting::ALL`]: crate::protocol::instructions::setting::Setting::ALL
//!
//! The remaining gap — a config that omits a field, names one that does not
//! exist, or gives one a value the parser rejects — is closed by
//! [`DeviceState::check`], which the integration tests assert is empty for the
//! checked-in config and which `simulated-device --check-config` reports.
//!
//! # Fields, algebraically
//!
//! The device is a partial map `Field → wire text`, where
//! `Field = names(Query) ∪ names(Register) ∪ names(Setting)` joined on the
//! canonical name. The catalogs are read and write *views* over that one name
//! space, so, writing `W = names(Register) ∪ names(Setting)`:
//!
//! - `names(Query) ∩ W` — read/write fields (`TITLE`, `TIMEZONE`, …). One
//!   storage cell, so a write is visible to a later query with no pairing table
//!   to maintain.
//! - `names(Query) \ W` — read-only (`FIRMWARE`, `MAC_ADDRESS`, `DATE`,
//!   `IDENTIFIER`, …).
//! - `W \ names(Query)` — write-only. Currently empty, and
//!   `every_settable_register_has_a_read_path` and `every_setting_has_a_read_path`
//!   keep it that way: a field with no read path is one the drift net can only
//!   check for *presence*, never decode or observe.
//!
//! The two write views are disjoint and stay that way deliberately: `Register`
//! is metadata addressed to the recorder subsystem and carries a recording
//! freeze, `Setting` is device configuration that does not. The simulator keeps
//! them apart as [`Request::Set`] and [`Request::Configure`] rather than
//! merging them into one write, because the wire framing differs too.
//!
//! The config supplies the map's *initial* value. It is mutable at runtime:
//! register and setting writes overwrite a field and recording commands rewrite
//! `RUNNING_STATE`.
//!
//! # License, and why it is not a field
//!
//! An Extron SMP sells its second and third stream as licensed options. A unit
//! without the license does not answer those fields with a default — it
//! *refuses* them, with a SIS error code, every time. That is a third outcome
//! alongside "here is the value" and the silence an unset field gets, and the
//! client distinguishes all three: a refusal is a completed exchange that
//! happens to say no (so it feeds `auto_disable_after`), while silence is
//! indistinguishable from a broken connection (so it feeds `exchange_timeout`).
//!
//! So the license is modelled as a veto that runs *ahead* of the field lookup,
//! not as an absent field:
//!
//! ```yaml
//! licensed_streams: [1]          # a base unit: no Dual Mode, no Confidence
//! unlicensed_fields: [SNMP_STATE] # anything else this unit refuses
//! refusal_code: 13               # answered as `E13`
//! ```
//!
//! Which fields a stream owns is read out of their canonical names
//! ([`stream_index`]) rather than tabulated, so the module's one rule holds
//! here too: a stream field added to a catalog is classified the moment it is
//! named. `licensed_streams: [1]` refuses fourteen fields across streams 2 and
//! 3 — the names, the enable states, the push URLs, the push enables and the
//! live states — without any of them being listed anywhere.
//!
//! # Configuration, in three layers
//!
//! The built-in config, then a `--config` file, then individual flags, each
//! beating the one before it. The merge is [`DeviceState::from_yaml_str_with`]
//! and [`Overrides`], so the precedence a test exercises is the precedence an
//! operator gets:
//!
//! ```text
//! simulated-device --licensed-streams 1 --field unit-name='Hall 2' --print-config
//! ```
//!
//! # Example
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use sismatic_core::simulator::{DeviceState, ReplyStream, bind};
//!
//! let state = Arc::new(DeviceState::from_yaml_str(&std::fs::read_to_string("device.yaml")?)?);
//! let device = bind(("127.0.0.1", 0), state, ReplyStream::Stderr).await?;
//! println!("listening on {}", device.port());
//! # Ok(())
//! # }
//! ```

pub mod dispatch;
pub mod server;
pub mod state;

pub use dispatch::{Request, classify};
pub use server::{DEFAULT_HOST_KEY, ReplyStream, SimulatedDevice, bind};
pub use state::{
    Credentials, DEFAULT_REFUSAL_CODE, DeviceState, Issue, License, LoadError, Overrides,
    canonical_name, catalog_fields, catalog_streams, stream_index,
};
