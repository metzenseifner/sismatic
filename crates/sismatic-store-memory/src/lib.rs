//! An in-memory [`ReadStore`] / [`WriteStore`] — the adapter the server runs on
//! today, and the one the tests run on always.
//!
//! # Shape
//!
//! Both maps are keyed by device, with a [`BTreeMap`] of fields inside:
//!
//! ```text
//! latest:  device -> field -> Reading
//! history: device -> field -> [Reading]      (append-only, insertion order)
//! ```
//!
//! The nesting is what makes the two questions the port asks both cheap. A point
//! read is two lookups; `latest_all` is one lookup and a clone of the inner map,
//! rather than a scan of every `(device, field)` pair in the process. A flat
//! `DashMap<(DeviceId, FieldName), _>` would invert that — point reads slightly
//! cheaper, `latest_all` linear in the whole fleet — and a fleet-wide scan to
//! answer a question about one device is the wrong trade for the route that
//! renders a device page.
//!
//! `BTreeMap` rather than `HashMap` for the inner map because the port promises
//! `latest_all` is ordered by field name, and a sorted map makes that the
//! natural iteration order rather than something re-established by a sort on
//! every read.
//!
//! Locking follows the outer key, so writers contend per *device*, not per
//! `(device, field)`. That is deliberate and cheap here: the poll loops for one
//! device tick seconds apart, so the window in which two of them want the same
//! device's entry is vanishingly small compared to the SSH exchange that
//! precedes each write.
//!
//! # What it is not
//!
//! `history` grows without bound — every poll of every field, for the life of
//! the process. That is fine for a test and for a development server, and it is
//! the main reason this adapter is not the deployment story: a real backend
//! bounds retention. Nothing in the port makes unbounded growth part of the
//! contract, so a bounded adapter is a drop-in.

use std::collections::BTreeMap;
use std::sync::Arc;

use dashmap::DashMap;
use sismatic_api_types::{DeviceId, FieldName, Reading, TimeSpan};
use sismatic_store::{ReadError, ReadStore, WriteError, WriteStore};

pub mod catalog;
pub mod outbox;

pub use catalog::MemoryCatalog;
pub use outbox::MemoryOutbox;

#[derive(Default, Clone)]
pub struct MemoryStore {
    latest: Arc<DashMap<DeviceId, BTreeMap<FieldName, Reading>>>,
    history: Arc<DashMap<DeviceId, BTreeMap<FieldName, Vec<Reading>>>>,
}

#[async_trait::async_trait]
impl ReadStore for MemoryStore {
    async fn latest(&self, dev: DeviceId, field: FieldName) -> Result<Option<Reading>, ReadError> {
        Ok(self
            .latest
            .get(&dev)
            .and_then(|fields| fields.get(&field).cloned()))
    }

    async fn latest_all(&self, dev: DeviceId) -> Result<Vec<Reading>, ReadError> {
        // `BTreeMap`'s iteration order *is* the field ordering the port
        // promises, so there is nothing to sort here.
        Ok(self
            .latest
            .get(&dev)
            .map(|fields| fields.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn between(
        &self,
        dev: DeviceId,
        field: FieldName,
        span: TimeSpan,
    ) -> Result<Vec<Reading>, ReadError> {
        Ok(self
            .history
            .get(&dev)
            .and_then(|fields| {
                fields
                    .get(&field)
                    // Insertion order is chronological because the writer is a
                    // poll loop appending as it goes, so filtering preserves the
                    // "oldest first" the port promises without a sort.
                    .map(|series| {
                        series
                            .iter()
                            .filter(|r| span.within(&r.at))
                            .cloned()
                            .collect()
                    })
            })
            .unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl WriteStore for MemoryStore {
    async fn upsert_latest(&self, r: Reading) -> Result<(), WriteError> {
        // Keyed off the reading's own `(device, field)`, so two poll loops on
        // one device write to two slots and neither evicts the other.
        self.latest
            .entry(r.device.clone())
            .or_default()
            .insert(r.field.clone(), r.clone());
        self.history
            .entry(r.device.clone())
            .or_default()
            .entry(r.field.clone())
            .or_default()
            .push(r);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{ReadingValue, Timestamp};

    /// A `Reading` for `device`/`field` with a `Number` value stamped at `at`.
    /// Keeps each test to the one axis it cares about (device, field, time, or
    /// value).
    fn reading(device: &str, field: &str, value: u32, at: &str) -> Reading {
        Reading {
            device: device.into(),
            field: field.into(),
            value: ReadingValue::Number(value),
            at: Timestamp(at.into()),
        }
    }

    /// A closed span `[start, end]` from two RFC 3339 strings.
    fn span(start: &str, end: &str) -> TimeSpan {
        TimeSpan {
            start: Timestamp(start.into()),
            end: Timestamp(end.into()),
        }
    }

    /// The whole of `dev`'s history for `field`, over a span wide enough to
    /// exclude nothing — for the tests that are about storage, not filtering.
    async fn all_history(store: &MemoryStore, dev: &str, field: &str) -> Vec<Reading> {
        store
            .between(
                dev.into(),
                field.into(),
                span("0000-01-01T00:00:00Z", "9999-12-31T23:59:59Z"),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn latest_is_none_for_unknown_device() {
        let store = MemoryStore::default();
        assert_eq!(
            store
                .latest("nobody".into(), "FIRMWARE".into())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn latest_is_none_for_a_field_never_polled_on_a_known_device() {
        let store = MemoryStore::default();
        store
            .upsert_latest(reading("dev-1", "FIRMWARE", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        // The device is known; this field of it is not. Absence, not an error.
        assert_eq!(
            store
                .latest("dev-1".into(), "SSH_PORT".into())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn upsert_then_latest_returns_the_reading() {
        let store = MemoryStore::default();
        let r = reading("dev-1", "RUNNING_STATE", 1, "2026-07-23T14:00:00Z");
        store.upsert_latest(r.clone()).await.unwrap();

        assert_eq!(
            store
                .latest("dev-1".into(), "RUNNING_STATE".into())
                .await
                .unwrap(),
            Some(r)
        );
    }

    #[tokio::test]
    async fn latest_reflects_the_most_recent_upsert_of_that_field() {
        let store = MemoryStore::default();
        let first = reading("dev-1", "SSH_PORT", 22, "2026-07-23T14:00:00Z");
        let second = reading("dev-1", "SSH_PORT", 2222, "2026-07-23T14:05:00Z");
        store.upsert_latest(first).await.unwrap();
        store.upsert_latest(second.clone()).await.unwrap();

        // Same key, so the second write overwrites: only it is "latest".
        assert_eq!(
            store
                .latest("dev-1".into(), "SSH_PORT".into())
                .await
                .unwrap(),
            Some(second)
        );
    }

    #[tokio::test]
    async fn fields_on_one_device_do_not_evict_each_other() {
        // The property the `(device, field)` key exists for. Under a `'*'`
        // schedule the sync driver runs one poll loop per field on a device, all
        // writing through this method; with a device-only key the last writer
        // would win and every other field would be unreadable.
        let store = MemoryStore::default();
        let firmware = reading("dev-1", "FIRMWARE", 211, "2026-07-23T14:00:00Z");
        let ssh_port = reading("dev-1", "SSH_PORT", 22023, "2026-07-23T14:00:01Z");
        let state = reading("dev-1", "RUNNING_STATE", 1, "2026-07-23T14:00:02Z");
        for r in [&firmware, &ssh_port, &state] {
            store.upsert_latest(r.clone()).await.unwrap();
        }

        assert_eq!(
            store
                .latest("dev-1".into(), "FIRMWARE".into())
                .await
                .unwrap(),
            Some(firmware)
        );
        assert_eq!(
            store
                .latest("dev-1".into(), "SSH_PORT".into())
                .await
                .unwrap(),
            Some(ssh_port)
        );
        assert_eq!(
            store
                .latest("dev-1".into(), "RUNNING_STATE".into())
                .await
                .unwrap(),
            Some(state)
        );
    }

    #[tokio::test]
    async fn devices_are_isolated() {
        let store = MemoryStore::default();
        let a = reading("dev-a", "F", 1, "2026-07-23T14:00:00Z");
        let b = reading("dev-b", "F", 2, "2026-07-23T14:00:00Z");
        store.upsert_latest(a.clone()).await.unwrap();
        store.upsert_latest(b.clone()).await.unwrap();

        assert_eq!(
            store.latest("dev-a".into(), "F".into()).await.unwrap(),
            Some(a)
        );
        assert_eq!(
            store.latest("dev-b".into(), "F".into()).await.unwrap(),
            Some(b)
        );
    }

    #[tokio::test]
    async fn latest_all_is_empty_for_unknown_device() {
        let store = MemoryStore::default();
        assert_eq!(
            store.latest_all("nobody".into()).await.unwrap(),
            Vec::<Reading>::new()
        );
    }

    #[tokio::test]
    async fn latest_all_returns_one_reading_per_field_sorted_by_field_name() {
        let store = MemoryStore::default();
        // Written in an order that is neither sorted nor reverse-sorted, so a
        // passing assertion cannot be an accident of insertion order.
        let ssh_port = reading("dev-1", "SSH_PORT", 22023, "2026-07-23T14:00:00Z");
        let firmware = reading("dev-1", "FIRMWARE", 211, "2026-07-23T14:00:01Z");
        let timezone = reading("dev-1", "TIMEZONE", 1, "2026-07-23T14:00:02Z");
        for r in [&ssh_port, &firmware, &timezone] {
            store.upsert_latest(r.clone()).await.unwrap();
        }
        // A repeat write must not add a second entry for the field.
        let firmware_again = reading("dev-1", "FIRMWARE", 212, "2026-07-23T14:05:00Z");
        store.upsert_latest(firmware_again.clone()).await.unwrap();

        assert_eq!(
            store.latest_all("dev-1".into()).await.unwrap(),
            vec![firmware_again, ssh_port, timezone]
        );
    }

    #[tokio::test]
    async fn latest_all_is_scoped_to_one_device() {
        let store = MemoryStore::default();
        let mine = reading("dev-a", "F", 1, "2026-07-23T14:00:00Z");
        store.upsert_latest(mine.clone()).await.unwrap();
        store
            .upsert_latest(reading("dev-b", "G", 2, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        assert_eq!(store.latest_all("dev-a".into()).await.unwrap(), vec![mine]);
    }

    #[tokio::test]
    async fn between_is_empty_for_unknown_device() {
        let store = MemoryStore::default();
        assert_eq!(
            all_history(&store, "nobody", "T").await,
            Vec::<Reading>::new()
        );
    }

    #[tokio::test]
    async fn between_is_empty_for_a_field_with_no_history() {
        let store = MemoryStore::default();
        store
            .upsert_latest(reading("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        assert_eq!(
            all_history(&store, "dev-1", "OTHER").await,
            Vec::<Reading>::new()
        );
    }

    #[tokio::test]
    async fn between_keeps_every_upsert_in_insertion_order() {
        let store = MemoryStore::default();
        // Unlike `latest`, history accumulates every write — even repeats of the
        // same field — and preserves the order they arrived in.
        let r1 = reading("dev-1", "T", 10, "2026-07-23T14:00:00Z");
        let r2 = reading("dev-1", "T", 20, "2026-07-23T14:01:00Z");
        let r3 = reading("dev-1", "T", 30, "2026-07-23T14:02:00Z");
        for r in [&r1, &r2, &r3] {
            store.upsert_latest(r.clone()).await.unwrap();
        }

        assert_eq!(all_history(&store, "dev-1", "T").await, vec![r1, r2, r3]);
    }

    #[tokio::test]
    async fn between_returns_only_the_requested_field() {
        // The read-side complement of `fields_on_one_device_do_not_evict_each_other`:
        // a history is a series of one quantity, so a second field polled on the
        // same device must not appear interleaved in it.
        let store = MemoryStore::default();
        let t1 = reading("dev-1", "T", 10, "2026-07-23T14:00:00Z");
        let other = reading("dev-1", "OTHER", 99, "2026-07-23T14:00:30Z");
        let t2 = reading("dev-1", "T", 20, "2026-07-23T14:01:00Z");
        for r in [&t1, &other, &t2] {
            store.upsert_latest(r.clone()).await.unwrap();
        }

        assert_eq!(all_history(&store, "dev-1", "T").await, vec![t1, t2]);
        assert_eq!(all_history(&store, "dev-1", "OTHER").await, vec![other]);
    }

    #[tokio::test]
    async fn between_filters_to_the_span_inclusive_of_bounds() {
        let store = MemoryStore::default();
        let before = reading("dev-1", "T", 1, "2026-07-23T13:59:59Z");
        let on_start = reading("dev-1", "T", 2, "2026-07-23T14:00:00Z");
        let inside = reading("dev-1", "T", 3, "2026-07-23T14:30:00Z");
        let on_end = reading("dev-1", "T", 4, "2026-07-23T15:00:00Z");
        let after = reading("dev-1", "T", 5, "2026-07-23T15:00:01Z");
        for r in [&before, &on_start, &inside, &on_end, &after] {
            store.upsert_latest(r.clone()).await.unwrap();
        }

        let got = store
            .between(
                "dev-1".into(),
                "T".into(),
                span("2026-07-23T14:00:00Z", "2026-07-23T15:00:00Z"),
            )
            .await
            .unwrap();
        // Both bounds are inclusive; the two straddling readings are excluded.
        assert_eq!(got, vec![on_start, inside, on_end]);
    }

    #[tokio::test]
    async fn between_can_return_empty_when_nothing_falls_in_span() {
        let store = MemoryStore::default();
        store
            .upsert_latest(reading("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        let got = store
            .between(
                "dev-1".into(),
                "T".into(),
                span("2020-01-01T00:00:00Z", "2020-12-31T23:59:59Z"),
            )
            .await
            .unwrap();
        assert_eq!(got, Vec::<Reading>::new());
    }

    #[tokio::test]
    async fn clone_shares_the_same_backing_store() {
        // `MemoryStore` holds `Arc<DashMap>`, so a clone is a handle to the same
        // data — a write through one is visible through the other.
        let store = MemoryStore::default();
        let handle = store.clone();
        let r = reading("dev-1", "F", 7, "2026-07-23T14:00:00Z");
        store.upsert_latest(r.clone()).await.unwrap();

        assert_eq!(
            handle.latest("dev-1".into(), "F".into()).await.unwrap(),
            Some(r)
        );
    }
}
