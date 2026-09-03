//! The names this server accepts: what a reading can be asked for, and what a
//! write can address.
//!
//! Every other route in the API takes a field or a register name as a path
//! parameter and passes it through — which is what lets a name added to `core`'s
//! catalog be served with no code change anywhere (see
//! `sismatic_http_api::handlers::readings`), and is also why a misspelled name
//! and an unpolled one are one answer. These types close that gap from the other
//! side: rather than teaching the routes which names exist, the server publishes
//! the list, and a caller spells its next URL from something it read rather than
//! from memory.
//!
//! # Why a name, its aliases and its prose, and nothing else
//!
//! [`InstructionSummary`] is deliberately not a description of the *wire*: there
//! is no payload here, no parser, no value shape, no register index. Those are
//! `sismatic-core`'s and stay there — this crate may not name it (see the crate
//! docs), and a caller has no use for a SIS verb it will never send. What a
//! caller needs is the string to put in a URL, the other strings that mean the
//! same thing, and a line saying what it is.
//!
//! The aliases matter more than they look. `core` accepts a name
//! case-insensitively and reads `-` as `_`, so a listing of canonical spellings
//! alone would suggest three names where there is one; but it *also* carries
//! genuine synonyms — `STREAM_NAME_1` for `STREAM_1_NAME`, `START` for
//! `STARTRECORDING` — which no amount of normalization derives. Those are the
//! ones a caller cannot guess, and the ones an older client may still be
//! sending.

use serde::{Deserialize, Serialize};

use crate::FieldName;

/// One name the server answers to, as a caller meets it.
///
/// The wire projection of a `sismatic-core` instruction — a `Query`, a
/// `Command`, a `Register` or a `Setting` — reduced to what a caller can act on.
/// One type for all four, because from out here they differ only in which route
/// spells them: the shape of "a name, its synonyms and what it means" does not
/// change between reading `FIRMWARE` and writing `TITLE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InstructionSummary {
    /// The canonical spelling, and the one every response body uses.
    ///
    /// A stored [`Reading`](crate::Reading) carries this form whichever spelling
    /// was requested, so it is the string to match on rather than the one the
    /// caller happened to send.
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "RUNNING_STATE"))]
    pub name: FieldName,
    /// The other spellings accepted for this same instruction, canonical name
    /// excluded.
    ///
    /// Empty for most names. Case and `-`/`_` folding are *not* listed here —
    /// every name is accepted case-insensitively with `-` read as `_`, so
    /// listing those would be listing the same name several times. What is here
    /// is the synonyms that folding does not produce.
    pub aliases: Vec<String>,
    /// One line saying what the instruction is, from the catalog entry itself.
    pub description: String,
}

/// Every field a reading can be asked for — the body of `GET /v1/readings`.
///
/// Wrapped in an object for the same reason [`ReadingList`](crate::ReadingList)
/// is: a bare array cannot gain a sibling key later without becoming a different
/// media type.
///
/// A field being listed here says the server knows how to *ask* for it, not that
/// anything has. Whether it is polled at all is the sync schedule's business, and
/// an unscheduled field is a name this list carries and
/// `GET /v1/readings/devices/{id}/fields` never shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FieldCatalog {
    pub fields: Vec<InstructionSummary>,
}

/// Everything a write can name — the body of `GET /v1/commands`.
///
/// Three lists rather than one, because the three reach devices through three
/// different routes and are governed by different rules. Flattening them would
/// hand a caller a hundred names and no way to tell which of them
/// `PUT /v1/commands/devices/{id}/settings/{field}` will accept — which is the
/// question the list exists to answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CommandCatalog {
    /// The recording lifecycle instructions.
    ///
    /// The one list whose names are *not* spelled in a URL: each is invoked by
    /// its own route — `POST /v1/commands/devices/{id}/recording/start` and the
    /// two beside it — rather than by being passed as a parameter. They are
    /// reported because they are what those routes send, so a reader can tie a
    /// `succeeded` command's echo back to an instruction, and because "what can
    /// this server ask a recorder to do" is answerable in no other way.
    pub commands: Vec<InstructionSummary>,
    /// The metadata registers, whose names go in the `{field}` of
    /// `PUT /v1/commands/devices/{id}/metadata/{field}`.
    ///
    /// Writable only while nothing is recording — a write to one of these during
    /// a recording is refused with `metadata_frozen`.
    pub metadata: Vec<InstructionSummary>,
    /// The device settings, whose names go in the `{field}` of
    /// `PUT /v1/commands/devices/{id}/settings/{field}`.
    ///
    /// Writable in every phase. A name in this list is refused by the metadata
    /// route and vice versa: the split is what keeps the recording freeze from
    /// being bypassed by writing a register through the settings route.
    pub settings: Vec<InstructionSummary>,
}
