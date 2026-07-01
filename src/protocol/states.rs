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
