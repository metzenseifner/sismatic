//! Fields a device has been *observed* to refuse, and when each may be tried
//! again.
//!
//! The runtime half of the field veto. [`DeviceConfig::disabled_fields`] is the
//! declared half — written down, immutable, part of a device's identity — and
//! this is what the system works out for itself when nobody has written
//! anything down.
//!
//! # Why this is not a field of `DeviceConfig`
//!
//! Because a `DeviceConfig` is immutable, and this is not. Editing a device's
//! configuration mints a new device with a new
//! [`uuid`](super::config::DeviceConfig::uuid), which is what lets the registry
//! keep a warm SSH session across a reload that did not touch that device. If an
//! inferred veto
//! lived in the config, the system would replace a device — and drop its
//! connection — every time it learned something, unprompted, in the middle of
//! ordinary operation.
//!
//! So this is held beside the device rather than inside its configuration, and
//! it is keyed by device *id*. That is also what it is evidence about: the
//! physical recorder at that address refused, not the configuration value used
//! to address it. A device replaced because its `connect_secs` moved is the same
//! unit with the same missing license, and it keeps what was learned about it; a
//! device removed from the fleet takes its learned set with it.
//!
//! # Who is allowed to count
//!
//! Only a caller that *repeats* can observe "repeated", so only the poll loops
//! in `sismatic-sync` call [`refused`](AutoDisabled::refused) and
//! [`answered`](AutoDisabled::answered). A refusal of a hand-issued write is a
//! refusal of that write and evidence of nothing — one operator pressing a
//! button must not take a field away from the fleet — and keeping the counting
//! out of [`Device::run`] is what makes that structurally true rather than a
//! rule someone has to remember.
//!
//! Enforcement is the opposite: [`Device::run`] checks the veto on every call,
//! so once a field is disabled *nothing* asks for it, whoever is asking.
//!
//! [`DeviceConfig::disabled_fields`]: super::config::DeviceConfig::disabled_fields
//! [`DeviceConfig::uuid`]: super::config::DeviceConfig::uuid
//! [`Device::run`]: super::device::Device::run

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Why a field is not being asked for.
///
/// Carried on [`DeviceError::Disabled`] so a caller can tell the two apart
/// without consulting the config, and reported on the wire for the same reason:
/// "you turned this off" and "we worked out it does not answer" call for very
/// different responses from an operator.
///
/// [`DeviceError::Disabled`]: super::device::DeviceError::Disabled
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VetoSource {
    /// Named in this device's `disabled_fields`.
    Declared,
    /// Inferred from consecutive refusals.
    Inferred,
}

impl std::fmt::Display for VetoSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VetoSource::Declared => f.write_str("disabled_fields"),
            VetoSource::Inferred => f.write_str("repeated refusals"),
        }
    }
}

/// One inferred veto, as an observer reads it.
///
/// A snapshot, taken under the lock and handed out by value: nothing here
/// reserves anything, and a caller rendering an inventory page must not be able
/// to hold the lock a poll loop needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDisabledField {
    /// Canonical field name.
    pub name: String,
    /// How many consecutive refusals have been seen. Keeps counting past the
    /// threshold so an operator can tell a field that just tripped from one
    /// that has refused a hundred times.
    pub refusals: u32,
    /// Whether the field is currently vetoed, as opposed to merely carrying a
    /// count that has not reached the threshold.
    pub disabled: bool,
    /// How long until the next attempt is allowed, or `None` when the device's
    /// `self_heal_secs` is zero and there will never be one.
    pub retry_in: Option<Duration>,
}

/// What one refusal did to the record.
///
/// Returned rather than logged here, because this type has no opinion about log
/// levels and the caller — a poll loop that already knows the device and the
/// field — is the one that can say it well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Counted, and the threshold is not reached yet.
    Counted { refusals: u32 },
    /// This refusal reached the threshold: the field is now vetoed.
    Disabled { refusals: u32 },
    /// The field was already vetoed. A heal attempt that was refused again, or
    /// a poll that raced the veto being armed.
    Still,
}

/// One field's record.
#[derive(Debug)]
struct Record {
    refusals: u32,
    /// `None` while the count is below the threshold — the field is being
    /// watched, not vetoed.
    ///
    /// `Some(None)` is a veto with no retry: `self_heal` is off, so the field
    /// stays disabled for the life of the process. `Some(Some(at))` is a veto
    /// that expires, which is the whole of self-healing — no timer, no task,
    /// just an instant a later poll compares itself against, exactly as the
    /// cold gate does for dialing.
    vetoed_until: Option<Option<Instant>>,
}

/// A device's inferred vetoes.
///
/// A `std::sync::Mutex` and not an async one, deliberately: every method here is
/// a map lookup and an integer compare, nothing awaits under the lock, and an
/// async mutex would buy the ability to hold it across an await that no caller
/// wants to perform. It is on the poll path, so it is also the cheap thing on a
/// path whose other half is an SSH round trip.
#[derive(Debug, Default)]
pub struct AutoDisabled {
    fields: Mutex<BTreeMap<String, Record>>,
}

impl AutoDisabled {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `field` is currently vetoed, and if so for how much longer.
    ///
    /// `Some(None)` means vetoed with no scheduled retry. A veto whose instant
    /// has passed reports `None` — the next caller is the heal attempt, and it
    /// goes through — rather than being cleared here, because clearing is what
    /// [`answered`](Self::answered) does and doing it from a read would make a
    /// status query change what it observes.
    pub fn veto(&self, field: &str) -> Option<Option<Duration>> {
        let fields = self.locked();
        let record = fields.get(field)?;
        match record.vetoed_until? {
            None => Some(None),
            Some(until) => {
                let now = Instant::now();
                (now < until).then(|| Some(until - now))
            }
        }
    }

    /// Record that `field` was refused, and say what that did.
    ///
    /// `threshold` of zero switches the inference off: the refusal is not even
    /// counted, so a deployment that wants only declared vetoes pays nothing
    /// for this and leaves no state behind.
    ///
    /// A refusal *at or past* the threshold re-arms the retry window rather
    /// than letting a healed-then-refused field retry on every tick — the heal
    /// attempt is itself evidence, and the answer it got was the same "no".
    pub fn refused(&self, field: &str, threshold: u32, self_heal: Option<Duration>) -> Refusal {
        if threshold == 0 {
            return Refusal::Still;
        }

        let mut fields = self.locked();
        let record = fields.entry(field.to_owned()).or_insert(Record {
            refusals: 0,
            vetoed_until: None,
        });
        let already = record.vetoed_until.is_some();
        record.refusals = record.refusals.saturating_add(1);

        if record.refusals < threshold {
            return Refusal::Counted {
                refusals: record.refusals,
            };
        }

        record.vetoed_until = Some(self_heal.map(|wait| Instant::now() + wait));
        if already {
            Refusal::Still
        } else {
            Refusal::Disabled {
                refusals: record.refusals,
            }
        }
    }

    /// Record that `field` answered, clearing anything held against it.
    ///
    /// The reset is what makes the count *consecutive* rather than cumulative.
    /// Without it a field that fails one poll in a hundred would eventually
    /// reach any threshold and be disabled on the strength of evidence spread
    /// over a week.
    ///
    /// Returns whether this cleared an actual veto, so a caller can announce a
    /// heal — the news is that a field came back, and only the poll that found
    /// it can say so.
    pub fn answered(&self, field: &str) -> bool {
        let mut fields = self.locked();
        match fields.remove(field) {
            Some(record) => record.vetoed_until.is_some(),
            None => false,
        }
    }

    /// Every field with something recorded against it, ordered by name.
    ///
    /// Includes fields being *watched* (counting, below the threshold) as well
    /// as vetoed ones, distinguished by
    /// [`disabled`](AutoDisabledField::disabled). An operator chasing a flaky
    /// recorder wants the near-misses too, and a snapshot that hid them would
    /// make a field appear to be disabled out of nowhere.
    pub fn snapshot(&self) -> Vec<AutoDisabledField> {
        let now = Instant::now();
        self.locked()
            .iter()
            .map(|(name, record)| AutoDisabledField {
                name: name.clone(),
                refusals: record.refusals,
                disabled: record.vetoed_until.is_some(),
                retry_in: record
                    .vetoed_until
                    .flatten()
                    .and_then(|until| until.checked_duration_since(now)),
            })
            .collect()
    }

    /// The guard, or the panic that says a previous holder died mid-update.
    ///
    /// `expect` rather than recovering the inner value, for the reason
    /// `LiveSettings` gives for the same choice: everything under this lock is
    /// infallible, so a poisoned mutex means a panic that had nothing to do
    /// with the data, and a half-updated veto is not a state to go on serving.
    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Record>> {
        self.fields
            .lock()
            .expect("the auto-disabled set is poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELD: &str = "STREAM_2_NAME";

    #[test]
    fn a_single_refusal_below_the_threshold_does_not_veto() {
        let learned = AutoDisabled::new();
        assert_eq!(
            learned.refused(FIELD, 2, None),
            Refusal::Counted { refusals: 1 }
        );
        assert_eq!(
            learned.veto(FIELD),
            None,
            "one refusal is not yet a pattern"
        );
    }

    #[test]
    fn the_threshold_refusal_vetoes_the_field() {
        let learned = AutoDisabled::new();
        learned.refused(FIELD, 2, None);
        assert_eq!(
            learned.refused(FIELD, 2, None),
            Refusal::Disabled { refusals: 2 }
        );
        assert_eq!(
            learned.veto(FIELD),
            Some(None),
            "with self-heal off the veto has no retry"
        );
    }

    /// The property that keeps the count meaningful: it is consecutive, so a
    /// field that mostly works never accumulates its way to a veto.
    #[test]
    fn an_answer_resets_the_count() {
        let learned = AutoDisabled::new();
        learned.refused(FIELD, 3, None);
        learned.refused(FIELD, 3, None);
        assert!(!learned.answered(FIELD), "nothing was vetoed yet");
        assert_eq!(
            learned.refused(FIELD, 3, None),
            Refusal::Counted { refusals: 1 },
            "the count restarted"
        );
    }

    #[test]
    fn a_threshold_of_zero_switches_the_inference_off() {
        let learned = AutoDisabled::new();
        for _ in 0..10 {
            assert_eq!(learned.refused(FIELD, 0, None), Refusal::Still);
        }
        assert_eq!(learned.veto(FIELD), None);
        assert!(
            learned.snapshot().is_empty(),
            "a disabled inference should leave no state behind"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_self_healing_veto_expires_on_its_own() {
        let learned = AutoDisabled::new();
        let heal = Some(Duration::from_secs(600));
        learned.refused(FIELD, 2, heal);
        learned.refused(FIELD, 2, heal);
        assert!(learned.veto(FIELD).is_some(), "vetoed immediately after");

        tokio::time::advance(Duration::from_secs(599)).await;
        assert!(learned.veto(FIELD).is_some(), "still inside the window");

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            learned.veto(FIELD),
            None,
            "the window closed, so the next poll is the heal attempt"
        );
    }

    /// A heal attempt that is refused again re-arms the window. Without this the
    /// veto would expire once and then let every tick through, which is the
    /// per-tick exchange the veto exists to stop.
    #[tokio::test(start_paused = true)]
    async fn a_refused_heal_attempt_re_arms_the_window() {
        let learned = AutoDisabled::new();
        let heal = Some(Duration::from_secs(600));
        learned.refused(FIELD, 2, heal);
        learned.refused(FIELD, 2, heal);

        tokio::time::advance(Duration::from_secs(601)).await;
        assert_eq!(learned.veto(FIELD), None);

        assert_eq!(
            learned.refused(FIELD, 2, heal),
            Refusal::Still,
            "already disabled; this is not news"
        );
        assert!(
            learned.veto(FIELD).is_some(),
            "the window should be armed again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_healed_field_clears_and_says_so() {
        let learned = AutoDisabled::new();
        let heal = Some(Duration::from_secs(60));
        learned.refused(FIELD, 2, heal);
        learned.refused(FIELD, 2, heal);

        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(learned.answered(FIELD), "clearing a veto is news");
        assert_eq!(learned.veto(FIELD), None);
        assert!(learned.snapshot().is_empty());
    }

    /// A snapshot shows near-misses too, so a field never appears to be
    /// disabled out of nowhere.
    #[test]
    fn a_snapshot_reports_watched_fields_as_well_as_vetoed_ones() {
        let learned = AutoDisabled::new();
        learned.refused("WATCHED", 3, None);
        learned.refused("VETOED", 1, None);

        let snapshot = learned.snapshot();
        assert_eq!(snapshot.len(), 2);
        // Ordered by name, which is what the `BTreeMap` is for.
        assert_eq!(snapshot[0].name, "VETOED");
        assert!(snapshot[0].disabled);
        assert_eq!(snapshot[1].name, "WATCHED");
        assert!(!snapshot[1].disabled, "counting is not yet disabling");
        assert_eq!(snapshot[1].refusals, 1);
    }
}
