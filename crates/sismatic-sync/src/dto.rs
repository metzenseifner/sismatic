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

use sismatic_api_types::{Alarm, MacAddr, ReadValue, RecordingState};
use sismatic_core::protocol::{RecordingState as CoreState, Value};

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
