use dashmap::DashMap;
use sismatic_api_types::{DeviceId, Reading, TimeSpan};
use sismatic_store::{ReadError, ReadStore, WriteError, WriteStore};
use std::sync::Arc;

#[derive(Default, Clone)]
pub struct MemoryStore {
    latest: Arc<DashMap<DeviceId, Reading>>,
    history: Arc<DashMap<DeviceId, Vec<Reading>>>, // optional per-device ring buffer for `between`
}

#[async_trait::async_trait]
impl ReadStore for MemoryStore {
    async fn latest(&self, dev: DeviceId) -> Result<Option<Reading>, ReadError> {
        Ok(self.latest.get(&dev).map(|r| r.clone()))
    }
    async fn between(&self, dev: DeviceId, span: TimeSpan) -> Result<Vec<Reading>, ReadError> {
        Ok(self
            .history
            .get(&dev)
            .map(|v| v.iter().filter(|r| span.within(&r.at)).cloned().collect())
            .unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl WriteStore for MemoryStore {
    async fn upsert_latest(&self, r: Reading) -> Result<(), WriteError> {
        self.latest.insert(r.device.clone(), r.clone());
        self.history.entry(r.device.clone()).or_default().push(r);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{ReadingValue, Timestamp};

    /// A `Reading` for `device`/`field` with a `Number` value stamped at `at`.
    /// Keeps each test to the one axis it cares about (device, time, or value).
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

    #[tokio::test]
    async fn latest_is_none_for_unknown_device() {
        let store = MemoryStore::default();
        assert_eq!(store.latest("nobody".into()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn upsert_then_latest_returns_the_reading() {
        let store = MemoryStore::default();
        let r = reading("dev-1", "RUNNING_STATE", 1, "2026-07-23T14:00:00Z");
        store.upsert_latest(r.clone()).await.unwrap();

        assert_eq!(store.latest("dev-1".into()).await.unwrap(), Some(r));
    }

    #[tokio::test]
    async fn latest_reflects_the_most_recent_upsert() {
        let store = MemoryStore::default();
        let first = reading("dev-1", "SSH_PORT", 22, "2026-07-23T14:00:00Z");
        let second = reading("dev-1", "SSH_PORT", 2222, "2026-07-23T14:05:00Z");
        store.upsert_latest(first).await.unwrap();
        store.upsert_latest(second.clone()).await.unwrap();

        // `latest` overwrites: only the second value is visible as "latest".
        assert_eq!(store.latest("dev-1".into()).await.unwrap(), Some(second));
    }

    #[tokio::test]
    async fn devices_are_isolated() {
        let store = MemoryStore::default();
        let a = reading("dev-a", "F", 1, "2026-07-23T14:00:00Z");
        let b = reading("dev-b", "F", 2, "2026-07-23T14:00:00Z");
        store.upsert_latest(a.clone()).await.unwrap();
        store.upsert_latest(b.clone()).await.unwrap();

        assert_eq!(store.latest("dev-a".into()).await.unwrap(), Some(a));
        assert_eq!(store.latest("dev-b".into()).await.unwrap(), Some(b));
    }

    #[tokio::test]
    async fn between_is_empty_for_unknown_device() {
        let store = MemoryStore::default();
        let got = store
            .between(
                "nobody".into(),
                span("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            )
            .await
            .unwrap();
        assert_eq!(got, Vec::<Reading>::new());
    }

    #[tokio::test]
    async fn between_keeps_every_upsert_in_insertion_order() {
        let store = MemoryStore::default();
        // Unlike `latest`, history accumulates every write — even repeats of the
        // same field — and preserves the order they arrived in.
        let r1 = reading("dev-1", "T", 10, "2026-07-23T14:00:00Z");
        let r2 = reading("dev-1", "T", 20, "2026-07-23T14:01:00Z");
        let r3 = reading("dev-1", "T", 30, "2026-07-23T14:02:00Z");
        store.upsert_latest(r1.clone()).await.unwrap();
        store.upsert_latest(r2.clone()).await.unwrap();
        store.upsert_latest(r3.clone()).await.unwrap();

        let got = store
            .between(
                "dev-1".into(),
                span("2026-07-23T00:00:00Z", "2026-07-24T00:00:00Z"),
            )
            .await
            .unwrap();
        assert_eq!(got, vec![r1, r2, r3]);
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

        assert_eq!(handle.latest("dev-1".into()).await.unwrap(), Some(r));
    }
}
