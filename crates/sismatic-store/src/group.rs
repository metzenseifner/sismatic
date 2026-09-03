//! What a group was told to be — the write side's belief about a *device group*.
//!
//! [`outbox`](crate::outbox) holds one
//! [`DesiredRecordingState`](sismatic_api_types::DesiredRecordingState) per
//! device, which is the right grain for admission: a group start has to be
//! decided against every member's own state. This port holds the other half —
//! the value the group as a whole was last asked to hold, per field — and it
//! exists because the per-device desired states cannot answer the question a
//! device group actually raises.
//!
//! # The failure this closes
//!
//! Five recorders are told to start. The batch times out under
//! [`Barrier::FailBatch`](sismatic_api_types::Barrier::FailBatch), every row
//! fails, every desired state rolls back, and every member reports `stopped`.
//! Read member by member the fleet is perfectly consistent: five idle
//! recorders that agree with their own desired states and with each other.
//! Nothing anywhere records that a lecture was supposed to be recording.
//!
//! An expectation records it. It is written when the submission is *admitted*,
//! in the same critical section, so it exists exactly when the request was
//! accepted — and it is deliberately **not** rolled back when a writing fails,
//! unlike [`rollback`](crate::outbox::rollback). The desired state rolls back
//! because it gates admission and a stuck one would freeze metadata forever;
//! the expectation gates nothing and is read by nobody but a dashboard, so
//! leaving it standing costs nothing and is the only way the drift stays
//! visible.
//!
//! # Why it is keyed by field and not by desired recording state
//!
//! An expectation shaped like a `DesiredRecordingState` would cover the three
//! lifecycle verbs and nothing else. Keyed by field it also covers the two
//! writes, so a device group where four members took the new title and one did
//! not is the same kind of finding as a device group where four started —
//! reported through one shape, on the same routes, with no second concept for
//! a client to learn.
//!
//! The cost is [`RECORDING_STATE_FIELD`]: this crate has to name the field the
//! lifecycle verbs land on, and it cannot see the catalog that defines it. See
//! that constant for how the two are held together.

use std::sync::Arc;

use sismatic_api_types::{
    FieldName, GroupExpectation, GroupId, Intent, ReadingValue, RecordingState,
};

use crate::ReadError;

pub type DynGroupState = Arc<dyn GroupState>;

/// The canonical name of the field the recording lifecycle is observed on.
///
/// The one string in this crate taken from `sismatic-core`'s query catalog, and
/// it is here rather than there because the port that needs it cannot see the
/// catalog — the same seam that makes [`Reading::field`] a `String` in the
/// first place.
///
/// Held down from the other side: `sismatic-sync` can see both, already decides
/// which polled field to reconcile with `Query::RunningState.name()`, and
/// asserts that this constant is that name. A rename in the catalog therefore
/// fails a test at the seam rather than silently producing a group that is
/// permanently `unknown` because its expectation is filed under a field nobody
/// polls.
///
/// [`Reading::field`]: sismatic_api_types::Reading::field
pub const RECORDING_STATE_FIELD: &str = "RUNNING_STATE";

/// Reading what each group was told to be. The read half of the group state,
/// and the only half the HTTP surface is given.
///
/// Absence is never an error, matching [`ReadStore`](crate::ReadStore): a group
/// nothing was ever written to has no expectation, and that is an answer.
#[async_trait::async_trait]
pub trait GroupState: Send + Sync {
    /// What `group` was last told `field` should be, or `None` if it has never
    /// been told.
    async fn expected(
        &self,
        group: GroupId,
        field: FieldName,
    ) -> Result<Option<GroupExpectation>, ReadError>;

    /// Every field `group` has an expectation for, ordered by field name.
    ///
    /// Ordered for the same reason [`ReadStore::latest_all`] is, and a method of
    /// its own for the same reason: the index route needs the whole set, and an
    /// adapter answers it in one pass where a caller would need a field catalog
    /// it has no way to obtain.
    ///
    /// [`ReadStore::latest_all`]: crate::ReadStore::latest_all
    async fn expected_all(&self, group: GroupId) -> Result<Vec<GroupExpectation>, ReadError>;
}

/// The `(field, value)` an intent asks a device to end up holding.
///
/// Total and wildcard-free, so a new [`Intent`] variant stops this compiling
/// until someone says what it expects. That decision is not one a default
/// should make: a lifecycle verb that quietly recorded no expectation would
/// leave the device group it moved undetectably out of sync.
///
/// The two writes name their own field and carry the caller's text unchanged —
/// see [`satisfies`] for why that text is compared against a decoded reading
/// rather than being canonicalized here, which would require knowing each
/// setting's shape.
pub fn expects(intent: &Intent) -> (FieldName, ReadingValue) {
    let state = |state| (RECORDING_STATE_FIELD.to_owned(), ReadingValue::State(state));
    match intent {
        Intent::StartRecording => state(RecordingState::Started),
        Intent::StopRecording => state(RecordingState::Stopped),
        Intent::PauseRecording => state(RecordingState::Paused),
        Intent::SetMetadata { field, value } | Intent::SetSetting { field, value } => {
            (field.clone(), ReadingValue::Text(value.clone()))
        }
    }
}

/// Whether `observed` — what a device reported — satisfies `expected`.
///
/// Not `==`, and the asymmetry is the whole content of this function.
///
/// An expectation minted from a lifecycle verb is already typed
/// ([`ReadingValue::State`]) and matches a reading exactly. One minted from a
/// write carries the caller's *text*, because that is what the [`Intent`] held:
/// `PUT /settings/HTTP_PORT {"value": "8080"}` expects `Text("8080")` while the
/// device reports `Port(8080)`, and `DHCP_MODE {"value": "true"}` expects
/// `Text("true")` while the device reports `Flag(true)`. Compared with `==`,
/// every flag and port setting in the system would read as permanently drifted.
///
/// So a `Text` expectation is *parsed in the shape the device answered in*. The
/// device's decode is the authority on what kind of value the field holds,
/// which is the correct direction: this crate does not know that `HTTP_PORT` is
/// a port, and does not have to — the reading says so.
///
/// # What it gets wrong, and which way
///
/// The flag spellings below mirror `sismatic-core`'s setting encoder, which
/// accepts `1/true/on/yes` and their negations. A spelling core accepts and
/// this does not would read as `drifted` on a device that is in fact correct: a
/// false alarm rather than a missed one, which is the safe direction for a
/// signal whose whole job is to be noticed.
pub fn satisfies(expected: &ReadingValue, observed: &ReadingValue) -> bool {
    match expected {
        // The common case, and the only one for the three lifecycle verbs.
        _ if expected == observed => true,
        ReadingValue::Text(want) => matches_text(want.trim(), observed),
        _ => false,
    }
}

/// Read `want` as a value of whatever shape `observed` turned out to be.
fn matches_text(want: &str, observed: &ReadingValue) -> bool {
    match observed {
        // Reached only when the two strings differ, since `satisfies` tried
        // equality first — so these are `false` by the time they are evaluated,
        // and written out rather than wildcarded to keep the match total.
        ReadingValue::Text(got) | ReadingValue::Version(got) | ReadingValue::Ack(got) => {
            want == got
        }
        ReadingValue::Port(got) => want.parse::<u16>().is_ok_and(|p| p == *got),
        ReadingValue::Number(got) => want.parse::<u32>().is_ok_and(|n| n == *got),
        ReadingValue::Flag(got) => flag_of(want) == Some(*got),
        ReadingValue::Mac(got) => want.eq_ignore_ascii_case(&got.0),
        ReadingValue::State(got) => want.eq_ignore_ascii_case(state_name(*got)),
        // A list has no single text form, and no writable field reads back as
        // one — `ACTIVE_ALARMS` is a query with no setting behind it. Nothing
        // can legitimately expect this, so nothing satisfies it.
        ReadingValue::Alarms(_) => false,
    }
}

/// The spellings `sismatic-core`'s `Setting` encoder folds onto `1` and `0`.
fn flag_of(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// The wire spelling of a recording state — the same `snake_case` its
/// `Serialize` impl produces, so a caller who writes what it read back agrees
/// with itself.
const fn state_name(state: RecordingState) -> &'static str {
    match state {
        RecordingState::Stopped => "stopped",
        RecordingState::Started => "started",
        RecordingState::Paused => "paused",
        RecordingState::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{Alarm, MacAddr};

    #[test]
    fn the_lifecycle_verbs_expect_a_recording_state() {
        for (intent, state) in [
            (Intent::StartRecording, RecordingState::Started),
            (Intent::StopRecording, RecordingState::Stopped),
            (Intent::PauseRecording, RecordingState::Paused),
        ] {
            assert_eq!(
                expects(&intent),
                (RECORDING_STATE_FIELD.to_owned(), ReadingValue::State(state))
            );
        }
    }

    #[test]
    fn a_write_expects_its_own_field_to_hold_the_text_it_carried() {
        assert_eq!(
            expects(&Intent::SetMetadata {
                field: "TITLE".into(),
                value: "Week 4".into(),
            }),
            ("TITLE".to_owned(), ReadingValue::Text("Week 4".into()))
        );
        assert_eq!(
            expects(&Intent::SetSetting {
                field: "TIMEZONE".into(),
                value: "UTC".into(),
            }),
            ("TIMEZONE".to_owned(), ReadingValue::Text("UTC".into()))
        );
    }

    /// The comparison a start is judged by, and the one place `==` is enough.
    #[test]
    fn a_typed_expectation_is_satisfied_by_the_same_value_and_nothing_else() {
        let started = ReadingValue::State(RecordingState::Started);
        assert!(satisfies(
            &started,
            &ReadingValue::State(RecordingState::Started)
        ));
        for other in [
            ReadingValue::State(RecordingState::Stopped),
            ReadingValue::State(RecordingState::Paused),
            ReadingValue::State(RecordingState::Unknown),
        ] {
            assert!(!satisfies(&started, &other), "{other:?} satisfied started");
        }
    }

    /// The failure the whole function exists for: a setting write carries text,
    /// the device answers a decoded value, and `==` would call every one of
    /// these drifted.
    #[test]
    fn a_text_expectation_is_read_in_the_shape_the_device_answered_in() {
        let cases = [
            ("8080", ReadingValue::Port(8080)),
            ("300", ReadingValue::Number(300)),
            ("1", ReadingValue::Flag(true)),
            ("true", ReadingValue::Flag(true)),
            ("on", ReadingValue::Flag(true)),
            ("yes", ReadingValue::Flag(true)),
            ("0", ReadingValue::Flag(false)),
            ("false", ReadingValue::Flag(false)),
            ("off", ReadingValue::Flag(false)),
            ("no", ReadingValue::Flag(false)),
            ("started", ReadingValue::State(RecordingState::Started)),
            ("STARTED", ReadingValue::State(RecordingState::Started)),
            ("2.11", ReadingValue::Version("2.11".into())),
            ("RcdrY1", ReadingValue::Ack("RcdrY1".into())),
            (
                "00-05-A6-1B-2C-3D",
                ReadingValue::Mac(MacAddr("00-05-A6-1B-2C-3D".into())),
            ),
        ];
        for (text, observed) in cases {
            assert!(
                satisfies(&ReadingValue::Text(text.into()), &observed),
                "'{text}' should have satisfied {observed:?}"
            );
        }
    }

    /// Surrounding whitespace is the caller's, not the device's: a value pasted
    /// into a form arrives with a trailing space and names the same setting.
    #[test]
    fn a_text_expectation_ignores_the_callers_surrounding_whitespace() {
        assert!(satisfies(
            &ReadingValue::Text("  8080 ".into()),
            &ReadingValue::Port(8080)
        ));
    }

    #[test]
    fn a_text_expectation_is_not_satisfied_by_a_different_value() {
        for observed in [
            ReadingValue::Port(9090),
            ReadingValue::Number(9090),
            ReadingValue::Flag(false),
            ReadingValue::Text("8081".into()),
            ReadingValue::State(RecordingState::Stopped),
            ReadingValue::Alarms(vec![Alarm {
                name: "video_loss".into(),
                level: "critical".into(),
            }]),
        ] {
            assert!(
                !satisfies(&ReadingValue::Text("8080".into()), &observed),
                "'8080' should not have satisfied {observed:?}"
            );
        }
    }

    /// A spelling neither side accepts is drift, not a crash and not a pass.
    #[test]
    fn an_uninterpretable_text_expectation_is_drift() {
        assert!(!satisfies(
            &ReadingValue::Text("perhaps".into()),
            &ReadingValue::Flag(true)
        ));
    }
}
