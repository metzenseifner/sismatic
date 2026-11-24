//! A declaratively configured, simulated Extron SMP.
//!
//! # Derived, not tabulated
//!
//! A hand-written table of request bytes → reply bytes would be a second copy
//! of the protocol, and it would rot silently: the failure mode is a command
//! timeout, not a compile error. So there is no table here.
//!
//! | Concern | Derived from |
//! |---|---|
//! | Which requests exist | [`Query::ALL`], [`Command::ALL`], [`Register::ALL`], [`Setting::ALL`] |
//! | Their exact bytes | each enum's own `instruction()` constructor |
//! | Which config keys are legal | the catalogs' `FromStr`, so aliases and case-insensitivity come free |
//! | Whether a value is well-formed | the *production* parser, run over the simulator's own output |
//!
//! [`Query::ALL`]: crate::protocol::instructions::query::Query::ALL
//! [`Command::ALL`]: crate::protocol::instructions::commands::Command::ALL
//! [`Register::ALL`]: crate::protocol::instructions::register::Register::ALL
//! [`Setting::ALL`]: crate::protocol::instructions::setting::Setting::ALL
//!
//! What no catalog can catch — a config that omits a field, misspells one, or
//! gives one a value the parser rejects — is [`DeviceState::check`]'s job. The
//! integration tests assert it is empty for the checked-in config, and
//! `--check-config` reports it.
//!
//! # Fields, algebraically
//!
//! The device is a partial map `Field → wire text`, where
//! `Field = names(Query) ∪ names(Register) ∪ names(Setting)` joined on the
//! canonical name. The catalogs are read and write *views* over that one name
//! space, so a write lands in the cell a later query reads, with no pairing
//! table to maintain. Writing `W = names(Register) ∪ names(Setting)`:
//!
//! - `names(Query) ∩ W` — read/write (`TITLE`, `TIMEZONE`, …).
//! - `names(Query) \ W` — read-only (`FIRMWARE`, `MAC_ADDRESS`, `DATE`, …).
//! - `W \ names(Query)` — write-only: the nine RTMP push fields, which SIS
//!   offers no read of. With no read path they can be checked for *presence*
//!   only, never decoded or observed, so
//!   `write_only_settings_are_exactly_the_attested_rtmp_hole` pins the set
//!   rather than letting it grow quietly.
//!
//! The two write views are disjoint deliberately — `Register` is recorder
//! metadata and carries a recording freeze, `Setting` is device configuration
//! and does not, and their wire framing differs — hence [`Request::Set`] and
//! [`Request::Configure`] rather than one merged write.
//!
//! The config is the map's *initial* value: writes overwrite a field, and
//! recording commands rewrite `RUNNING_STATE`.
//!
//! One field's reply is a *list* rather than a line — `RECORDING_EVENTS`, the
//! recorder's own table — which no YAML scalar can hold. That cell is written
//! as a [`RecordingEvents`] document instead, inline or from a file, and
//! rendered back to the device's framing by the protocol layer that parses it:
//!
//! ```yaml
//! fields:
//!   RECORDING_EVENTS:
//!     recording_events: []        # a recorder that has never recorded
//!   # ...or, for a table too long to paste:
//!   # RECORDING_EVENTS:
//!   #   file: ./recordings.json
//! ```
//!
//! [`RecordingEvents`]: crate::protocol::RecordingEvents
//!
//! # License, and why it is not a field
//!
//! An SMP sells its second and third stream as licensed options, and an
//! unlicensed field is *refused* with a SIS error code rather than answered
//! with a default. That is a third outcome beside a value and the silence an
//! unset field gets, and the client routes all three differently: a refusal is
//! a completed exchange that says no, so it feeds `auto_disable_after`, while
//! silence is indistinguishable from a broken connection and feeds
//! `exchange_timeout`.
//!
//! So the license is a veto ahead of the field lookup rather than an absent
//! field. It covers writes as much as reads, and holds whether or not the
//! config left a value in the cell.
//!
//! ```yaml
//! licensed_streams: [1]           # a base unit: no Dual Mode, no Confidence
//! unlicensed_fields: [SNMP_STATE] # anything else this unit refuses
//! refusal_code: 13                # answered as `E13`
//! ```
//!
//! Which stream a field belongs to is read out of its canonical name
//! ([`stream_index`]), so `licensed_streams: [1]` refuses fourteen fields
//! across streams 2 and 3 without any of them being listed, and a stream field
//! added to a catalog is classified the moment it is named.
//!
//! # Configuration, in three layers
//!
//! The built-in config, then a `--config` file, then individual flags, each
//! beating the one before it. The merge is [`DeviceState::from_yaml_str_with`],
//! so the precedence a test exercises is the precedence an operator gets:
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
