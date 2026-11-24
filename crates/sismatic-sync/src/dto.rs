//! Translating a decoded [`Value`] into its [`ReadValue`] DTO.
//!
//! `api-types` deliberately re-declares the value model instead of depending on
//! `core` (so no frontend gains a compile path to the device library). The price
//! of that decoupling is that two enums describe the same values, and they could
//! drift. This module is where that risk is *paid down*.
//!
//! # The drift sentinel
//!
//! [`to_dto`] is a plain function, not a `From` impl: the orphan rule forbids
//! `impl From<core::Value> for api_types::ReadValue`, since both types are
//! foreign to this crate. That is fine — a free function is the right shape.
//!
//! What matters is that its `match` has **no `_ =>` wildcard arm**. Add a variant
//! to [`Value`] (or to [`sismatic_core::protocol::RecordingState`]) and this
//! function stops compiling until the new case is handled here and mirrored in
//! [`ReadValue`]. Drift therefore surfaces as a *build error at the seam*
//! rather than a silent wrong value on the wire. Do not add a catch-all arm; the
//! wildcard is the only thing that could hide a new variant.

use sismatic_api_types::{
    Alarm, MacAddr, ReadValue, RecordingEvent, RecordingState, ScheduleState,
};
use sismatic_core::protocol::{
    RecordingEvent as CoreRecordingEvent, RecordingState as CoreState,
    ScheduleState as CoreSchedule, Value,
};

/// Convert a decoded device value into its wire DTO.
///
/// Exhaustive by construction — see the module docs on why there is no wildcard.
pub fn to_dto(value: Value) -> ReadValue {
    match value {
        Value::Text(s) => ReadValue::Text(s),
        Value::Version(s) => ReadValue::Version(s),
        Value::Port(p) => ReadValue::Port(p),
        Value::Number(n) => ReadValue::Number(n),
        Value::Flag(b) => ReadValue::Flag(b),
        // core carries the MAC as `[u8; 6]`; the wire carries its canonical
        // hyphenated-hex rendering, which `Display` already produces.
        Value::Mac(m) => ReadValue::Mac(MacAddr(m.to_string())),
        Value::Ack(s) => ReadValue::Ack(s),
        Value::State(s) => ReadValue::State(state_to_dto(s)),
        // core models an alarm as an unnamed `(name, level)` pair; the DTO names
        // the fields for a self-describing JSON object.
        Value::Alarms(alarms) => ReadValue::Alarms(
            alarms
                .into_iter()
                .map(|(name, level)| Alarm { name, level })
                .collect(),
        ),
        Value::RecordingEvents(events) => {
            ReadValue::RecordingEvents(events.into_iter().map(event_to_dto).collect())
        }
    }
}

/// Convert one recording event into its wire DTO.
///
/// The one shape change is `extra`: core holds the unattested keys as real
/// `serde_json::Value`s, while the DTO holds each one's **JSON text**, so that
/// `api-types` can keep `serde` as its only dependency (see the field's docs
/// there). This crate sees both models and already has `serde_json`, so the
/// rendering lands here, which is where every other translation does.
///
/// A value that will not render is dropped rather than failing the whole read:
/// it is by definition a key nothing is attested to depend on, and losing one
/// speculative field beats losing a hundred recordings. `serde_json::Value` is
/// in fact infallible to serialize, so this is a defensive branch, not a
/// path with a known trigger.
fn event_to_dto(event: CoreRecordingEvent) -> RecordingEvent {
    RecordingEvent {
        db_id: event.db_id,
        start: event.start,
        identifier: event.identifier,
        creator: event.creator,
        title: event.title,
        state: event.state.map(schedule_to_dto),
        media_present: event.media_present,
        package: event.package,
        extra: event
            .extra
            .into_iter()
            .filter_map(|(k, v)| serde_json::to_string(&v).ok().map(|text| (k, text)))
            .collect(),
    }
}

/// Map core's schedule state onto the wire enum. Wildcard-free, like
/// [`state_to_dto`], so a state added in core is a compile error here.
pub fn schedule_to_dto(state: CoreSchedule) -> ScheduleState {
    match state {
        CoreSchedule::Pending => ScheduleState::Pending,
        CoreSchedule::Current => ScheduleState::Current,
        CoreSchedule::Processed => ScheduleState::Processed,
        CoreSchedule::Finished => ScheduleState::Finished,
        CoreSchedule::Archiving => ScheduleState::Archiving,
        CoreSchedule::Archived => ScheduleState::Archived,
        CoreSchedule::ArchiveFailed => ScheduleState::ArchiveFailed,
        CoreSchedule::PackageDeleted => ScheduleState::PackageDeleted,
        CoreSchedule::RecordFailed => ScheduleState::RecordFailed,
        CoreSchedule::ArchiveNomethod => ScheduleState::ArchiveNomethod,
        CoreSchedule::RecordSkipped => ScheduleState::RecordSkipped,
        CoreSchedule::PackageResched => ScheduleState::PackageResched,
        CoreSchedule::Unrecognized(code) => ScheduleState::Unrecognized(code),
    }
}

/// Map core's recording state onto the wire enum. Also wildcard-free, so a new
/// state variant in core is a compile error here.
pub fn state_to_dto(state: CoreState) -> RecordingState {
    match state {
        CoreState::Stopped => RecordingState::Stopped,
        CoreState::Started => RecordingState::Started,
        CoreState::Paused => RecordingState::Paused,
        CoreState::Unknown => RecordingState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_core::protocol::MacAddr as CoreMac;

    #[test]
    fn maps_primitive_variants() {
        assert_eq!(to_dto(Value::Port(22023)), ReadValue::Port(22023));
        assert_eq!(to_dto(Value::Number(300)), ReadValue::Number(300));
        assert_eq!(to_dto(Value::Flag(true)), ReadValue::Flag(true));
        assert_eq!(
            to_dto(Value::Text("Lecture 1".into())),
            ReadValue::Text("Lecture 1".into())
        );
        assert_eq!(
            to_dto(Value::Ack("RcdrY1".into())),
            ReadValue::Ack("RcdrY1".into())
        );
    }

    #[test]
    fn renders_mac_as_hyphenated_hex() {
        let dto = to_dto(Value::Mac(CoreMac([0x00, 0x05, 0xA6, 0x1B, 0x2C, 0x3D])));
        assert_eq!(dto, ReadValue::Mac(MacAddr("00-05-A6-1B-2C-3D".into())));
    }

    #[test]
    fn maps_every_recording_state() {
        // Each state is exercised so the mapping's *correctness* is covered; the
        // wildcard-free match already covers its *totality* at compile time.
        for (core, wire) in [
            (CoreState::Stopped, RecordingState::Stopped),
            (CoreState::Started, RecordingState::Started),
            (CoreState::Paused, RecordingState::Paused),
            (CoreState::Unknown, RecordingState::Unknown),
        ] {
            assert_eq!(to_dto(Value::State(core)), ReadValue::State(wire));
        }
    }

    /// Core and the DTO each spell out the codes, so this pins the two lists
    /// to each other: same codes, and each wire name is the device name with
    /// its prefix removed.
    #[test]
    fn maps_every_schedule_state() {
        assert_eq!(CoreSchedule::KNOWN.len(), ScheduleState::KNOWN.len());
        for (wire, code, name) in ScheduleState::KNOWN {
            let core = CoreSchedule::from_code(code);
            assert_eq!(schedule_to_dto(core), wire, "code {code}");
            assert_eq!(
                core.device_name(),
                Some(format!("SCHEDULE_{}", name.to_uppercase()).as_str()),
                "code {code}"
            );
        }
        assert_eq!(
            schedule_to_dto(CoreSchedule::Unrecognized(7)),
            ScheduleState::Unrecognized(7)
        );
    }

    /// The `extra` map changes representation across this seam — core holds
    /// JSON values, the DTO holds their text — so it is asserted directly
    /// rather than inferred from the struct copy around it.
    #[test]
    fn renders_unattested_event_keys_as_json_text() {
        let event = CoreRecordingEvent {
            db_id: 58,
            start: Some(1767879511),
            identifier: "A_1".into(),
            creator: "c900011".into(),
            title: "Mechanics I".into(),
            state: Some(CoreSchedule::RecordSkipped),
            media_present: Some(true),
            package: "/var/internal/A_1".into(),
            extra: [
                ("duration".to_string(), serde_json::json!(1200)),
                ("channel".to_string(), serde_json::json!("B")),
                ("tags".to_string(), serde_json::json!(["a", "b"])),
            ]
            .into_iter()
            .collect(),
        };

        let ReadValue::RecordingEvents(events) = to_dto(Value::RecordingEvents(vec![event])) else {
            panic!("expected a recording-event list");
        };
        let got = &events[0];

        assert_eq!(got.db_id, 58);
        assert_eq!(got.start, Some(1767879511));
        assert_eq!(got.state, Some(ScheduleState::RecordSkipped));
        assert_eq!(got.media_present, Some(true));
        assert_eq!(got.title, "Mechanics I");
        // Each value is the JSON *text* of what core held, so a client gets it
        // back with one `JSON.parse` and nothing is lost on the way.
        assert_eq!(got.extra["duration"], "1200");
        assert_eq!(got.extra["channel"], r#""B""#);
        assert_eq!(got.extra["tags"], r#"["a","b"]"#);
    }

    /// Absence survives the crossing. A firmware that omits a field must not
    /// have `0`/`false` invented for it on the way to the wire.
    #[test]
    fn a_sparse_event_crosses_the_seam_still_sparse() {
        let event = CoreRecordingEvent {
            db_id: 42,
            start: None,
            identifier: String::new(),
            creator: String::new(),
            title: String::new(),
            state: None,
            media_present: None,
            package: String::new(),
            extra: Default::default(),
        };

        let ReadValue::RecordingEvents(events) = to_dto(Value::RecordingEvents(vec![event])) else {
            panic!("expected a recording-event list");
        };
        assert_eq!(events[0].start, None);
        assert_eq!(events[0].state, None);
        assert_eq!(events[0].media_present, None);
        assert!(events[0].extra.is_empty());
    }

    #[test]
    fn names_alarm_pair_fields() {
        let dto = to_dto(Value::Alarms(vec![
            ("video_loss".into(), "critical".into()),
            ("publish_failure".into(), "warning".into()),
        ]));
        assert_eq!(
            dto,
            ReadValue::Alarms(vec![
                Alarm {
                    name: "video_loss".into(),
                    level: "critical".into(),
                },
                Alarm {
                    name: "publish_failure".into(),
                    level: "warning".into(),
                },
            ])
        );
    }
}
