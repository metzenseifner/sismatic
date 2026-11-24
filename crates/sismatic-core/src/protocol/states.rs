use std::fmt;

/// Recording state reported by the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    Stopped,
    Started,
    Paused,
    Unknown,
}

impl RecordingState {
    /// Map the SMP's integer state code to a variant.
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => RecordingState::Stopped,
            2 => RecordingState::Paused,
            1 => RecordingState::Started,
            _ => RecordingState::Unknown,
        }
    }
    pub fn is_recording(self) -> bool {
        matches!(self, RecordingState::Started | RecordingState::Paused)
    }
}

impl fmt::Display for RecordingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecordingState::Stopped => "stopped",
            RecordingState::Started => "started",
            RecordingState::Paused => "paused",
            RecordingState::Unknown => "unknown",
        })
    }
}

/// Where one `RECORDING_EVENTS` row is in its schedule's lifecycle, decoded
/// from the event's integer `state`.
///
/// Each variant is the device's own `SCHEDULE_*` name with the prefix removed,
/// so a code found in a firmware log can be searched for here as written.
///
/// # Codes nothing lists
///
/// The documented codes leave a gap at `7`, and a later firmware may add more.
/// An unlisted code becomes [`Unrecognized`](Self::Unrecognized) and keeps its
/// value, rather than failing the parse. One unknown row must not cost a caller
/// the other hundred, and keeping the code means nothing is lost when the list
/// is extended. The same reasoning keeps an `Alarm`'s level as free text.
///
/// Serialized as the bare integer, which is how the device writes it, so a
/// parsed event renders back to its own frame unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum ScheduleState {
    /// `0` — Recording Scheduled.
    Pending,
    /// `1` — Recording Now.
    Current,
    /// `2` — Recording Finalized.
    Processed,
    /// `3` — Ready to Upload.
    Finished,
    /// `4` — Uploading to Server.
    Archiving,
    /// `5` — Uploading to Server Complete.
    Archived,
    /// `6` — Uploading to Server Failed (retry pending).
    ArchiveFailed,
    /// `8` — Recording Deleted (to recover disk space).
    PackageDeleted,
    /// `9` — Recording Failed (package corrupted).
    RecordFailed,
    /// `10` — No Transfer Method Defined.
    ArchiveNomethod,
    /// `11` — Transfer Skipped.
    RecordSkipped,
    /// `200` — Transfer Rescheduled (one time).
    PackageResched,
    /// A code not listed above, kept as the device sent it.
    Unrecognized(u8),
}

impl ScheduleState {
    /// Every listed state, in code order. [`Unrecognized`](Self::Unrecognized)
    /// is not included because it is a placeholder, not a state.
    pub const KNOWN: [ScheduleState; 12] = [
        ScheduleState::Pending,
        ScheduleState::Current,
        ScheduleState::Processed,
        ScheduleState::Finished,
        ScheduleState::Archiving,
        ScheduleState::Archived,
        ScheduleState::ArchiveFailed,
        ScheduleState::PackageDeleted,
        ScheduleState::RecordFailed,
        ScheduleState::ArchiveNomethod,
        ScheduleState::RecordSkipped,
        ScheduleState::PackageResched,
    ];

    /// Map the device's integer code to a variant. Total: an unlisted code is
    /// [`Unrecognized`](Self::Unrecognized).
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => ScheduleState::Pending,
            1 => ScheduleState::Current,
            2 => ScheduleState::Processed,
            3 => ScheduleState::Finished,
            4 => ScheduleState::Archiving,
            5 => ScheduleState::Archived,
            6 => ScheduleState::ArchiveFailed,
            8 => ScheduleState::PackageDeleted,
            9 => ScheduleState::RecordFailed,
            10 => ScheduleState::ArchiveNomethod,
            11 => ScheduleState::RecordSkipped,
            200 => ScheduleState::PackageResched,
            other => ScheduleState::Unrecognized(other),
        }
    }

    /// The device's integer code; the inverse of [`from_code`](Self::from_code).
    pub fn code(self) -> u8 {
        match self {
            ScheduleState::Pending => 0,
            ScheduleState::Current => 1,
            ScheduleState::Processed => 2,
            ScheduleState::Finished => 3,
            ScheduleState::Archiving => 4,
            ScheduleState::Archived => 5,
            ScheduleState::ArchiveFailed => 6,
            ScheduleState::PackageDeleted => 8,
            ScheduleState::RecordFailed => 9,
            ScheduleState::ArchiveNomethod => 10,
            ScheduleState::RecordSkipped => 11,
            ScheduleState::PackageResched => 200,
            ScheduleState::Unrecognized(code) => code,
        }
    }

    /// The device's documented name, e.g. `"SCHEDULE_ARCHIVED"`, or `None` for
    /// an unrecognized code.
    pub fn device_name(self) -> Option<&'static str> {
        Some(match self {
            ScheduleState::Pending => "SCHEDULE_PENDING",
            ScheduleState::Current => "SCHEDULE_CURRENT",
            ScheduleState::Processed => "SCHEDULE_PROCESSED",
            ScheduleState::Finished => "SCHEDULE_FINISHED",
            ScheduleState::Archiving => "SCHEDULE_ARCHIVING",
            ScheduleState::Archived => "SCHEDULE_ARCHIVED",
            ScheduleState::ArchiveFailed => "SCHEDULE_ARCHIVE_FAILED",
            ScheduleState::PackageDeleted => "SCHEDULE_PACKAGE_DELETED",
            ScheduleState::RecordFailed => "SCHEDULE_RECORD_FAILED",
            ScheduleState::ArchiveNomethod => "SCHEDULE_ARCHIVE_NOMETHOD",
            ScheduleState::RecordSkipped => "SCHEDULE_RECORD_SKIPPED",
            ScheduleState::PackageResched => "SCHEDULE_PACKAGE_RESCHED",
            ScheduleState::Unrecognized(_) => return None,
        })
    }

    /// The device's human-readable description, e.g. `"Ready to Upload"`, or
    /// `None` for an unrecognized code.
    pub fn description(self) -> Option<&'static str> {
        Some(match self {
            ScheduleState::Pending => "Recording Scheduled",
            ScheduleState::Current => "Recording Now",
            ScheduleState::Processed => "Recording Finalized",
            ScheduleState::Finished => "Ready to Upload",
            ScheduleState::Archiving => "Uploading to Server",
            ScheduleState::Archived => "Uploading to Server Complete",
            ScheduleState::ArchiveFailed => "Uploading to Server Failed (retry pending)",
            ScheduleState::PackageDeleted => "Recording Deleted (to recover disk space)",
            ScheduleState::RecordFailed => "Recording Failed (package corrupted)",
            ScheduleState::ArchiveNomethod => "No Transfer Method Defined",
            ScheduleState::RecordSkipped => "Transfer Skipped",
            ScheduleState::PackageResched => "Transfer Rescheduled (one time)",
            ScheduleState::Unrecognized(_) => return None,
        })
    }
}

impl From<u8> for ScheduleState {
    fn from(code: u8) -> Self {
        ScheduleState::from_code(code)
    }
}

impl From<ScheduleState> for u8 {
    fn from(state: ScheduleState) -> Self {
        state.code()
    }
}

impl fmt::Display for ScheduleState {
    /// The device name, or `SCHEDULE_<code>` for an unrecognized code.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.device_name() {
            Some(name) => f.write_str(name),
            None => write!(f, "SCHEDULE_{}", self.code()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_round_trips() {
        for code in 0..=u8::MAX {
            assert_eq!(ScheduleState::from_code(code).code(), code);
        }
    }

    #[test]
    fn known_states_are_named_and_distinct() {
        for state in ScheduleState::KNOWN {
            assert!(state.device_name().is_some(), "{state:?} has no name");
            assert!(
                state.description().is_some(),
                "{state:?} has no description"
            );
            assert_eq!(ScheduleState::from_code(state.code()), state);
        }
        let unrecognized = (0..=u8::MAX)
            .filter(|c| matches!(ScheduleState::from_code(*c), ScheduleState::Unrecognized(_)))
            .count();
        assert_eq!(unrecognized, 256 - ScheduleState::KNOWN.len());
    }

    /// `7` is the gap in the documented list.
    #[test]
    fn an_unlisted_code_is_kept() {
        let state = ScheduleState::from_code(7);
        assert_eq!(state, ScheduleState::Unrecognized(7));
        assert_eq!(state.device_name(), None);
        assert_eq!(state.to_string(), "SCHEDULE_7");
    }

    #[test]
    fn serializes_as_the_bare_code() {
        assert_eq!(
            serde_json::to_string(&ScheduleState::PackageResched).unwrap(),
            "200"
        );
        assert_eq!(
            serde_json::from_str::<ScheduleState>("5").unwrap(),
            ScheduleState::Archived
        );
        assert_eq!(
            serde_json::from_str::<ScheduleState>("7").unwrap(),
            ScheduleState::Unrecognized(7)
        );
    }
}
