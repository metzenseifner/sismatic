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
pub use state::{Credentials, DeviceState, Issue, LoadError, catalog_fields};
