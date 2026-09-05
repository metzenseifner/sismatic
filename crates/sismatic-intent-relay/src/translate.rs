//! Mapper between Intent -> Instruction, the write-side counterpart of sismatic-sync::dto.
//! `Intent -> Instruction`, the write-side counterpart of `sismatic_sync::dto`.
//!
//! Exhaustive by construction. Do not add a catch-all arm: the wildcard is the
//! only thing that could let a new `Intent` variant reach a device as silence.

use std::fmt;
use std::str::FromStr;

use sismatic_api_types::Intent;
use sismatic_core::protocol::instructions::Instruction;
use sismatic_core::protocol::instructions::commands::Command;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::protocol::instructions::setting::{Setting, ValueError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    UnknownRegister(String),
    UnknownSetting(String),
    /// A metadata register submitted through the settings route. Refused,
    /// because the settings route carries no freeze and the metadata route
    /// does — accepting this would be a way around the rule.
    MetadataThroughSettings(String),
    Value(ValueError),
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranslateError::UnknownRegister(name) => {
                write!(f, "no metadata register named '{name}'")
            }
            TranslateError::UnknownSetting(name) => write!(f, "no device setting named '{name}'"),
            TranslateError::MetadataThroughSettings(name) => write!(
                f,
                "'{name}' is a metadata register; submit it to the metadata route, \
                 which enforces the recording freeze"
            ),
            TranslateError::Value(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TranslateError {}

pub fn to_instruction(intent: &Intent) -> Result<Instruction, TranslateError> {
    match intent {
        Intent::StartRecording => Ok(Command::Start.instruction()),
        Intent::StopRecording => Ok(Command::Stop.instruction()),
        Intent::PauseRecording => Ok(Command::Pause.instruction()),

        Intent::SetMetadata { field, value } => Register::from_str(field)
            .map(|register| register.instruction(value))
            .map_err(|_| TranslateError::UnknownRegister(field.clone())),

        Intent::SetSetting { field, value } => {
            // The catalog is the authority on which names are metadata, so the
            // check is a lookup rather than a second list to maintain.
            if Register::from_str(field).is_ok() {
                return Err(TranslateError::MetadataThroughSettings(field.clone()));
            }
            Setting::from_str(field)
                .map_err(|_| TranslateError::UnknownSetting(field.clone()))
                .and_then(|setting| setting.instruction(value).map_err(TranslateError::Value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_core::protocol::instructions::setting::Setting;

    #[test]
    fn every_metadata_register_translates() {
        for &register in Register::ALL {
            let intent = Intent::SetMetadata {
                field: register.name().to_owned(),
                value: "x".into(),
            };
            assert!(
                to_instruction(&intent).is_ok(),
                "{register} did not translate"
            );
        }
    }

    /// The settings half of the same pin: every name in the catalog is
    /// reachable, whatever its shape makes of the value.
    ///
    /// Split from the encoding assertion below because the two fail for
    /// different reasons and only one of them is about routing. A name that no
    /// longer resolves is a routing bug — `PUT /settings/<name>` 404s on a field
    /// the catalog still advertises. A value the shape refuses is not: it is the
    /// encoder doing its job, and `"1"` is not a plausible value for every kind
    /// of setting there is (it is not an RTMP URL, for one).
    #[test]
    fn every_device_setting_name_resolves() {
        for &setting in Setting::ALL {
            let intent = Intent::SetSetting {
                field: setting.name().to_owned(),
                value: "1".into(),
            };
            match to_instruction(&intent) {
                Ok(_) | Err(TranslateError::Value(_)) => {}
                Err(e) => panic!("{setting} did not resolve: {e}"),
            }
        }
    }

    /// ...and every setting can actually be encoded, given a value its own shape
    /// accepts.
    ///
    /// The candidates are tried rather than tabled per setting, because a table
    /// keyed by variant is a second catalog to keep in step with the first — the
    /// thing `instruction_catalog!` exists to avoid. What this pins is that no
    /// setting refuses *everything*, which is what a shape wired to the wrong
    /// field would do.
    #[test]
    fn every_device_setting_encodes_a_value_of_its_own_shape() {
        const CANDIDATES: &[&str] = &[
            "1",                               // flags, ports, plain text
            "rtmp://live.example.org/app/key", // RTMP publish targets
        ];
        for &setting in Setting::ALL {
            let encoded = CANDIDATES.iter().any(|value| {
                to_instruction(&Intent::SetSetting {
                    field: setting.name().to_owned(),
                    value: (*value).into(),
                })
                .is_ok()
            });
            assert!(
                encoded,
                "{setting} accepted none of {CANDIDATES:?}; is its shape wired to the right field?"
            );
        }
    }

    /// The hole this closes: without the guard, `PUT /settings/TITLE` would write a
    /// metadata register with no freeze applied.
    ///
    /// Asserted on the error rather than on the whole `Result`, because
    /// `Instruction` holds an `Arc<dyn Fn>` parser and so has no `PartialEq`.
    #[test]
    fn a_metadata_register_cannot_be_written_through_the_settings_intent() {
        for &register in Register::ALL {
            let intent = Intent::SetSetting {
                field: register.name().to_owned(),
                value: "x".into(),
            };
            assert_eq!(
                to_instruction(&intent).unwrap_err(),
                TranslateError::MetadataThroughSettings(register.name().to_owned())
            );
        }
    }

    /// A value the shape refuses never becomes a payload, and the reason
    /// survives the trip through `TranslateError` so the write log says why.
    #[test]
    fn a_bad_setting_value_is_refused_before_any_byte_is_sent() {
        let intent = Intent::SetSetting {
            field: "DHCP_MODE".to_owned(),
            value: "maybe".into(),
        };
        assert_eq!(
            to_instruction(&intent).unwrap_err(),
            TranslateError::Value(ValueError::NotAFlag("maybe".into()))
        );
    }

    #[test]
    fn an_unknown_name_is_refused_on_both_routes() {
        assert_eq!(
            to_instruction(&Intent::SetMetadata {
                field: "NOT_A_FIELD".to_owned(),
                value: "x".into(),
            })
            .unwrap_err(),
            TranslateError::UnknownRegister("NOT_A_FIELD".to_owned())
        );
        assert_eq!(
            to_instruction(&Intent::SetSetting {
                field: "NOT_A_FIELD".to_owned(),
                value: "x".into(),
            })
            .unwrap_err(),
            TranslateError::UnknownSetting("NOT_A_FIELD".to_owned())
        );
    }
}
