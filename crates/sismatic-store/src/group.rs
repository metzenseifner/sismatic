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
//! accepted — and it is deliberately **not** rolled back when a write fails,
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

use sismatic_api_types::{FieldName, GroupExpectation, GroupId, Intent, ReadValue, RecordingState};

use crate::ReadError;

pub type DynGroupState = Arc<dyn GroupState>;

/// The canonical name of the field the recording lifecycle is observed on.
///
/// The one string in this crate taken from `sismatic-core`'s query catalog, and
/// it is here rather than there because the port that needs it cannot see the
/// catalog — the same seam that makes [`Read::field`] a `String` in the
/// first place.
///
/// Held down from the other side: `sismatic-sync` can see both, already decides
/// which polled field to reconcile with `Query::RunningState.name()`, and
/// asserts that this constant is that name. A rename in the catalog therefore
/// fails a test at the seam rather than silently producing a group that is
/// permanently `unknown` because its expectation is filed under a field nobody
/// polls.
///
/// [`Read::field`]: sismatic_api_types::Read::field
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
/// see [`satisfies`] for why that text is compared against a decoded read
/// rather than being canonicalized here, which would require knowing each
/// setting's shape.
pub fn expects(intent: &Intent) -> (FieldName, ReadValue) {
    let state = |state| (RECORDING_STATE_FIELD.to_owned(), ReadValue::State(state));
    match intent {
        Intent::StartRecording => state(RecordingState::Started),
        Intent::StopRecording => state(RecordingState::Stopped),
        Intent::PauseRecording => state(RecordingState::Paused),
        Intent::SetMetadata { field, value } | Intent::SetSetting { field, value } => {
            (field.clone(), ReadValue::Text(value.clone()))
        }
    }
}

/// Whether `observed` — what a device reported — satisfies `expected`.
///
/// Not `==`, and the asymmetry is the whole content of this function.
///
/// An expectation minted from a lifecycle verb is already typed
/// ([`ReadValue::State`]) and matches a read exactly. One minted from a
/// write carries the caller's *text*, because that is what the [`Intent`] held:
/// `PUT /settings/HTTP_PORT {"value": "8080"}` expects `Text("8080")` while the
/// device reports `Port(8080)`, and `DHCP_MODE {"value": "true"}` expects
/// `Text("true")` while the device reports `Flag(true)`. Compared with `==`,
/// every flag and port setting in the system would read as permanently drifted.
///
/// So a `Text` expectation is *parsed in the shape the device answered in*. The
/// device's decode is the authority on what kind of value the field holds,
/// which is the correct direction: this crate does not know that `HTTP_PORT` is
/// a port, and does not have to — the read says so.
///
/// # What it gets wrong, and which way
///
/// The flag spellings below mirror `sismatic-core`'s setting encoder, which
/// accepts `1/true/on/yes` and their negations. A spelling core accepts and
/// this does not would read as `drifted` on a device that is in fact correct: a
/// false alarm rather than a missed one, which is the safe direction for a
/// signal whose whole job is to be noticed.
pub fn satisfies(expected: &ReadValue, observed: &ReadValue) -> bool {
    match expected {
        // The common case, and the only one for the three lifecycle verbs.
        _ if expected == observed => true,
        ReadValue::Text(want) => matches_text(want.trim(), observed),
        _ => false,
    }
}

/// Read `want` as a value of whatever shape `observed` turned out to be.
fn matches_text(want: &str, observed: &ReadValue) -> bool {
    match observed {
        // Reached only when the two strings differ, since `satisfies` tried
        // equality first — so these are `false` by the time they are evaluated,
        // and written out rather than wildcarded to keep the match total.
        ReadValue::Text(got) | ReadValue::Version(got) | ReadValue::Ack(got) => want == got,
        ReadValue::Port(got) => want.parse::<u16>().is_ok_and(|p| p == *got),
        ReadValue::Number(got) => want.parse::<u32>().is_ok_and(|n| n == *got),
        ReadValue::Flag(got) => flag_of(want) == Some(*got),
        ReadValue::Mac(got) => want.eq_ignore_ascii_case(&got.0),
        ReadValue::State(got) => want.eq_ignore_ascii_case(state_name(*got)),
        // A list has no single text form, and no writable field reads back as
        // one — `ACTIVE_ALARMS` is a query with no setting behind it. Nothing
        // can legitimately expect this, so nothing satisfies it.
        ReadValue::Alarms(_) => false,
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
                (RECORDING_STATE_FIELD.to_owned(), ReadValue::State(state))
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
            ("TITLE".to_owned(), ReadValue::Text("Week 4".into()))
        );
        assert_eq!(
            expects(&Intent::SetSetting {
                field: "TIMEZONE".into(),
                value: "UTC".into(),
            }),
            ("TIMEZONE".to_owned(), ReadValue::Text("UTC".into()))
        );
    }

    /// The comparison a start is judged by, and the one place `==` is enough.
    #[test]
    fn a_typed_expectation_is_satisfied_by_the_same_value_and_nothing_else() {
        let started = ReadValue::State(RecordingState::Started);
        assert!(satisfies(
            &started,
            &ReadValue::State(RecordingState::Started)
        ));
        for other in [
            ReadValue::State(RecordingState::Stopped),
            ReadValue::State(RecordingState::Paused),
            ReadValue::State(RecordingState::Unknown),
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
            ("8080", ReadValue::Port(8080)),
            ("300", ReadValue::Number(300)),
            ("1", ReadValue::Flag(true)),
            ("true", ReadValue::Flag(true)),
            ("on", ReadValue::Flag(true)),
            ("yes", ReadValue::Flag(true)),
            ("0", ReadValue::Flag(false)),
            ("false", ReadValue::Flag(false)),
            ("off", ReadValue::Flag(false)),
            ("no", ReadValue::Flag(false)),
            ("started", ReadValue::State(RecordingState::Started)),
            ("STARTED", ReadValue::State(RecordingState::Started)),
            ("2.11", ReadValue::Version("2.11".into())),
            ("RcdrY1", ReadValue::Ack("RcdrY1".into())),
            (
                "00-05-A6-1B-2C-3D",
                ReadValue::Mac(MacAddr("00-05-A6-1B-2C-3D".into())),
            ),
        ];
        for (text, observed) in cases {
            assert!(
                satisfies(&ReadValue::Text(text.into()), &observed),
                "'{text}' should have satisfied {observed:?}"
            );
        }
    }

    /// Surrounding whitespace is the caller's, not the device's: a value pasted
    /// into a form arrives with a trailing space and names the same setting.
    #[test]
    fn a_text_expectation_ignores_the_callers_surrounding_whitespace() {
        assert!(satisfies(
            &ReadValue::Text("  8080 ".into()),
            &ReadValue::Port(8080)
        ));
    }

    #[test]
    fn a_text_expectation_is_not_satisfied_by_a_different_value() {
        for observed in [
            ReadValue::Port(9090),
            ReadValue::Number(9090),
            ReadValue::Flag(false),
            ReadValue::Text("8081".into()),
            ReadValue::State(RecordingState::Stopped),
            ReadValue::Alarms(vec![Alarm {
                name: "video_loss".into(),
                level: "critical".into(),
            }]),
        ] {
            assert!(
                !satisfies(&ReadValue::Text("8080".into()), &observed),
                "'8080' should not have satisfied {observed:?}"
            );
        }
    }

    /// A spelling neither side accepts is drift, not a crash and not a pass.
    #[test]
    fn an_uninterpretable_text_expectation_is_drift() {
        assert!(!satisfies(
            &ReadValue::Text("perhaps".into()),
            &ReadValue::Flag(true)
        ));
    }
}
