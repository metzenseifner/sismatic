//! Reading a whole group: what it was told to be, and what its members report.
//!
//! The device readings routes answer "what did this device last say". A group
//! is not a device and has nothing to say, so the group routes answer a
//! different question — one that only exists once there is more than one
//! device in a device group:
//!
//! > Are they all in the state they were put in?
//!
//! Two comparisons answer it, and they are deliberately separate fields rather
//! than one verdict, because they fail independently and call for different
//! actions.
//!
//! # Against the expectation
//!
//! [`GroupExpectation`] is what the group was last *told* to be — recorded when
//! a group-addressed write is admitted, and read back here beside the members'
//! readings. It is what makes "the device group is not recording, and it was
//! asked to be" a statement the API can make. Without it, five members that all
//! failed to start agree perfectly with each other and look fine.
//!
//! [`SyncState`] is that comparison, per member and rolled up for the group. It
//! is [`Unknown`](SyncState::Unknown), not `InSync`, when there is nothing to
//! compare: a group that was never commanded has no expectation, and reporting
//! it as in sync would be a claim nothing supports.
//!
//! # Against each other
//!
//! [`GroupFieldState::uniform`] is the other comparison: whether every member
//! that has reported holds the same value. It needs no expectation, so it
//! catches drift on fields nobody commands — a device group where one member is
//! on last year's firmware, or in a different timezone — which is exactly the
//! class of problem an expectation can never see.
//!
//! It is a plain `bool` rather than a third [`SyncState`], because unlike the
//! comparison against an expectation it is *always* decidable: with fewer than
//! two members reporting it is vacuously true, and vacuity is a fact about the
//! answer rather than an absence of one.

use serde::{Deserialize, Serialize};

use crate::command::{CommandRecord, Phase};
use crate::reading::{Reading, Timestamp};
use crate::value::ReadingValue;
use crate::{DeviceId, FieldName, GroupId};

/// What a group was last told one of its fields should hold.
///
/// Recorded at *submission*, not at success: this is the value the device group
/// was asked for, and a command that never reached a device is precisely the
/// case drift detection exists to surface. An expectation that rolled back on
/// failure would make the one situation worth an alarm — five members told to
/// start, none of which did — indistinguishable from a device group that is
/// quietly idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupExpectation {
    /// The field the expectation is about, by canonical name.
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "RUNNING_STATE"))]
    pub field: FieldName,
    /// The value the members are expected to hold.
    ///
    /// A metadata or setting write carries the caller's text
    /// ([`ReadingValue::Text`]) because that is what the intent held, while a
    /// reading carries the device's decode — so `"1"` is expected and
    /// `{"type":"flag","value":true}` is observed, and the two agree. The
    /// comparison that reconciles them is the server's; a client should read
    /// [`SyncState`] rather than re-derive it.
    pub value: ReadingValue,
    /// When the group was told. The submission's instant, so every member of
    /// one request shares it.
    pub since: Timestamp,
}

/// Whether an observation agrees with what the group was told.
///
/// Three states rather than a `bool`, because "we cannot tell" is a real and
/// common answer — a group that has never been commanded, or a member that has
/// never reported the field — and folding it into either `true` or `false`
/// would put a wrong claim on a dashboard. `Unknown` is the resting state of a
/// system nobody has asked for anything yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// The observed value satisfies the expectation.
    InSync,
    /// The observed value does not. Something happened to this member that did
    /// not happen to the request — or something happened to the device that the
    /// request never asked for.
    Drifted,
    /// Nothing to compare: no expectation is recorded for the field, or the
    /// member has never reported it.
    Unknown,
}

/// One member's side of a group field: what it last reported, and whether that
/// is what was asked of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemberState {
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    /// The member's most recent reading of the field, or `null` if it has never
    /// reported one.
    ///
    /// `null` rather than omitting the member, because *which* member is silent
    /// is the answer: a device group where one member has stopped reporting
    /// looks identical to a one-member device group if the silent one is
    /// dropped.
    pub reading: Option<Reading>,
    pub sync: SyncState,
}

/// One field across a whole group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupFieldState {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub group: GroupId,
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "RUNNING_STATE"))]
    pub field: FieldName,
    /// What the group was last told this field should be, if anything.
    pub expected: Option<GroupExpectation>,
    /// The members' agreement with `expected`, rolled up: `drifted` if any
    /// member drifted, `in_sync` if at least one agrees and none drifted, and
    /// `unknown` when there is nothing to compare against.
    ///
    /// Drift wins over agreement deliberately. A roll-up is read as a status
    /// light, and a device group where four members started and one did not is
    /// one that needs attention, not one that is four-fifths fine.
    pub sync: SyncState,
    /// Whether every member that has reported holds the same value.
    ///
    /// Vacuously `true` when fewer than two members have reported. Independent
    /// of `sync`: members can agree with each other and all disagree with what
    /// was asked (nothing happened), or each agree with the expectation and
    /// differ from each other only in a field nobody commanded.
    pub uniform: bool,
    /// One entry per member, in the order the group configures them.
    pub members: Vec<MemberState>,
}

/// Every field a group knows about, for the group index view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupFieldStateList {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub group: GroupId,
    /// Ordered by field name, for the same reason the store's `latest_all` is:
    /// a rendered page should diff cleanly between requests rather than
    /// reflecting an adapter's iteration order.
    pub fields: Vec<GroupFieldState>,
}

/// One member's series over the requested span, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemberHistory {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    pub readings: Vec<Reading>,
}

/// A group's history of one field: one series per member.
///
/// Partitioned by member rather than interleaved into one time-ordered list,
/// for the reason the store's `between` takes a field at all: a series is a
/// series of one quantity on one device, and a merged list is one every caller
/// would immediately re-partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupHistory {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub group: GroupId,
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "RUNNING_STATE"))]
    pub field: FieldName,
    /// The current expectation, so a plot of the series can be drawn against
    /// the line it was supposed to hold. It describes *now*, not the span —
    /// `since` says when it was set, which may be after the window ends.
    pub expected: Option<GroupExpectation>,
    /// One series per member, in configured order.
    pub members: Vec<MemberHistory>,
}

/// One member's write-side state: the phase the outbox has accepted for it, and
/// the epoch that phase belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemberPhase {
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    pub phase: Phase,
    pub epoch: u64,
}

/// A device group's write-side state, as `GET /v1/commands/groups/{id}/recording`
/// reports it.
///
/// The write-side counterpart of [`GroupFieldState`] over `RUNNING_STATE`, and
/// deliberately a different answer: this is what the outbox *accepted*, which
/// moves the moment a start is admitted and before any device has been
/// contacted, while the reading is what a member last reported. A group whose
/// members are all `recording` here and all `stopped` there is a group whose
/// commands have not landed yet — or have failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupPhase {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub group: GroupId,
    /// The phase every member is in, or `null` when they are not all in the
    /// same one.
    ///
    /// A device group has no phase of its own — the outbox admits per member,
    /// because a start has to be decided against each member's own state — so
    /// the only honest group-level answer is the one the members agree on.
    /// `null` is therefore a finding rather than a missing value: it says the
    /// members have diverged, and `members` says how.
    ///
    /// `null` for a group with no members, which agree on nothing because there
    /// is nothing to agree.
    pub phase: Option<Phase>,
    /// One entry per member, in the order the group configures them.
    ///
    /// Epochs are reported per member and never rolled up: two members can
    /// share a phase and be on different takes, and a single group epoch would
    /// have to pick one of them.
    pub members: Vec<MemberPhase>,
}

/// One member's command history, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemberCommands {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    pub commands: Vec<CommandRecord>,
}

/// Everything a device group's members have been asked to do.
///
/// Partitioned by member rather than merged into one time-ordered list, for the
/// reason [`GroupHistory`] is: two submissions can share an instant, so a
/// merged list would have no total order to present them in, and every caller
/// would re-partition it to find out what one recorder was told.
///
/// A row's `batch` is what ties one group-addressed request back together
/// across the members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupCommandList {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub group: GroupId,
    pub members: Vec<MemberCommands>,
}
