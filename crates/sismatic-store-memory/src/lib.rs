//! An in-memory [`ReadStore`] / [`WriteStore`] / [`Lifecycle`] — the adapter the
//! server runs on today, and the one the tests run on always.
//!
//! # Shape
//!
//! Both maps are keyed by device, with a [`BTreeMap`] of fields inside:
//!
//! ```text
//! latest:  device -> field -> Read
//! history: device -> field -> [Read]      (append-only, chronological)
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
//! # Bounding it: the ledger
//!
//! `history` used to grow without bound — every poll of every field, for the
//! life of the process — which is what made this adapter a development story
//! rather than a deployment one. It is now bounded on two independent axes, and
//! the `Ledger` is what enforces both:
//!
//! - **In time**, by [`Lifecycle::prune`], which the composition root's sweeper
//!   calls on a schedule with a cutoff derived from the configured retention.
//! - **In bytes**, by a budget enforced on the *write* path. A cap that only
//!   held at sweep time would not be a cap: a burst arriving one second after a
//!   sweep has the whole interval to exhaust the machine. So every
//!   [`upsert_latest`](WriteStore::upsert_latest) that pushes the estimate over
//!   the budget evicts oldest-first until it is under again, before returning.
//!
//! The ledger holds one `Slot` per stored history entry, in the order it was
//! written. That queue *is* the eviction order, which is what makes both
//! operations amortised O(1) per entry rather than a scan for the oldest series
//! across the fleet. It costs a second copy of each entry's key, and that copy
//! is itself counted against the budget rather than quietly spent outside it.
//!
//! ## What the byte estimate counts
//!
//! `size_of` the stored structs, plus the bytes behind every `String` they own,
//! plus a flat per-entry surcharge standing in for what an estimate
//! cannot see — `BTreeMap` node slack, `VecDeque` spare capacity, the shard's
//! own bookkeeping. It is deliberately *not* an allocator reading: there is no
//! portable way to ask, and a number that moved with the allocator's mood would
//! make the budget untestable. Lengths rather than capacities, for the same
//! reason — a `String` grown and shrunk would otherwise change a figure the
//! tests pin.
//!
//! So the budget bounds the dominant term, not the process's RSS. An operator
//! sizing a machine should leave headroom above it; what the budget guarantees
//! is that the store's growth *stops*, which is the property unbounded history
//! did not have.
//!
//! ## What is never evicted
//!
//! `latest` is exempt from both axes. It holds one entry per `(device, field)`
//! actually polled, so it is bounded by the *configuration* — fleet size times
//! schedule width — and cannot grow with uptime the way history does. Expiring
//! it would also break the read side's basic promise for exactly the devices
//! worth worrying about: a recorder that went quiet three days ago would vanish
//! from the fleet page rather than showing the stale value that says so.
//!
//! It is still *counted*, so the budget is honest about the floor it cannot go
//! below. A budget under that floor leaves [`Usage::over_budget`] true after
//! every write; the sweeper is what says so out loud, since this crate has no
//! logger of its own.
//!
//! # Lock discipline
//!
//! The ledger's mutex is never held across a `latest`/`history` shard guard,
//! and no shard guard is held across taking it. Eviction has to reach an
//! arbitrary device's series, and a `DashMap` shard's `RwLock` is not
//! reentrant, so an evictor holding the guard of the device it was called for
//! would deadlock against itself on whichever ids happen to share a shard — the
//! same hash-dependent, test-resistant bug [`outbox`] avoids by not sharding at
//! all. Every mutating method therefore runs as: touch the maps, drop the
//! guards, settle the books, then apply whatever the books decided.
//!
//! That ordering makes the two structures briefly disagree — a slot can be
//! spoken for before its entry is gone. Nothing reads across the seam, so the
//! disagreement is unobservable: `between` answers from `history` alone, and
//! [`usage`](Lifecycle::usage) answers from the ledger alone.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use sismatic_api_types::{Alarm, DeviceId, FieldName, Read, ReadValue, TimeSpan, Timestamp};
use sismatic_store::lifecycle::{Lifecycle, Swept, Usage};
use sismatic_store::{ReadError, ReadStore, WriteError, WriteStore};

pub mod catalog;
pub mod outbox;

pub use catalog::MemoryCatalog;
pub use outbox::MemoryOutbox;

/// The flat per-entry surcharge the estimate adds for what it cannot see: a
/// `BTreeMap` node's links and slack, a `VecDeque`'s spare capacity, the
/// allocator's own per-allocation header.
///
/// A single constant rather than a per-container model on purpose. The figure
/// it stands in for varies with the allocator and the fill factor of maps this
/// crate does not control, so a more elaborate derivation would be more precise
/// about something still approximate. What it has to be is *not zero* — the
/// per-entry overheads dominate for the short values this store mostly holds
/// (a port number, a recording state), where the `Read` itself is a few dozen
/// bytes.
const ENTRY_OVERHEAD: u64 = 64;

#[derive(Clone)]
pub struct MemoryStore {
    latest: Arc<DashMap<DeviceId, BTreeMap<FieldName, Read>>>,
    history: Arc<DashMap<DeviceId, BTreeMap<FieldName, VecDeque<Read>>>>,
    ledger: Arc<Ledger>,
}

impl Default for MemoryStore {
    /// An unbounded store — the shape every test wants, and the shape this
    /// adapter had before it could be bounded at all.
    ///
    /// Unbounded rather than defaulting to some cap, because a default cap here
    /// would be a policy invented by an adapter: the number belongs to the
    /// deployment, and the composition root is what has it. A test that means
    /// to exercise the budget names one with [`MemoryStore::with_budget`].
    fn default() -> Self {
        Self::with_budget(None)
    }
}

impl MemoryStore {
    /// A store that evicts oldest-first to stay within `budget` estimated bytes,
    /// or an unbounded one for `None`.
    ///
    /// `Option<u64>` rather than `u64` with a zero sentinel: "no cap" and "a cap
    /// of zero bytes" are genuinely different requests, and the second one — a
    /// store that evicts everything it is given — is a config error worth being
    /// able to express and then complain about, rather than one silently read as
    /// the first.
    #[must_use]
    pub fn with_budget(budget: Option<u64>) -> Self {
        Self {
            latest: Arc::new(DashMap::new()),
            history: Arc::new(DashMap::new()),
            ledger: Arc::new(Ledger {
                budget,
                books: Mutex::new(Books::default()),
            }),
        }
    }

    /// Remove the entries `slots` names, front-first, from the series they
    /// belong to.
    ///
    /// Called only with slots already taken out of the ledger's queue, so each
    /// one is spoken for by exactly this caller. Two evictors targeting one
    /// series therefore hold two *different* slots and pop the two oldest
    /// entries between them, in whichever order they get the guard — the pair
    /// that leaves is the same either way.
    ///
    /// A series that is already empty is skipped rather than treated as a bug.
    /// The books have been settled by the time this runs, so the alternative
    /// would be a panic that unwinds a poll loop over an accounting discrepancy
    /// of a few dozen bytes.
    fn drop_front(&self, slots: &[Slot]) {
        for slot in slots {
            let Some(mut fields) = self.history.get_mut(&slot.device) else {
                continue;
            };

            let emptied = match fields.get_mut(&slot.field) {
                Some(series) => {
                    series.pop_front();
                    series.is_empty()
                }
                None => false,
            };
            if emptied {
                fields.remove(&slot.field);
            }

            // Drop the device's own entry once nothing is filed under it. A
            // fleet that shrinks — or a field that stops being polled — would
            // otherwise leak one map key per name that ever appeared, which is
            // unbounded growth of exactly the kind this ledger exists to stop.
            //
            // The guard goes first, and the re-check inside `remove_if` is why:
            // between dropping it and taking the shard's write lock, a poll loop
            // may have filed something new under this device.
            let vacated = fields.is_empty();
            drop(fields);
            if vacated {
                self.history.remove_if(&slot.device, |_, f| f.is_empty());
            }
        }
    }
}

/// The budget and the books it is enforced against.
///
/// Split from [`Books`] so `budget` — which never changes after construction —
/// is readable without taking the lock, and so the lock covers exactly the
/// mutable state.
struct Ledger {
    /// Estimated bytes the store may hold, or `None` for unbounded.
    budget: Option<u64>,
    books: Mutex<Books>,
}

/// Every stored history entry, in write order, plus the running totals.
#[derive(Default)]
struct Books {
    /// One slot per live history entry, oldest first. The eviction order, and
    /// — since a read is stamped immediately before it is written — very nearly
    /// the chronological one; see [`Books::take_expired`] for where the
    /// difference shows and why it does not matter.
    queue: VecDeque<Slot>,
    /// Estimated bytes held by `history` and this queue together.
    history_bytes: u64,
    /// Estimated bytes held by `latest`, which nothing evicts.
    latest_bytes: u64,
    /// Entries the budget has dropped since the process started.
    evicted: u64,
}

/// The ledger's record of one stored history entry.
///
/// Carries the key rather than a pointer to the entry because the entry lives
/// behind a shard lock this must not hold. The duplicated strings are the
/// price, and they are counted in [`Slot::bytes`] — so the budget is enforced
/// against the store's true footprint rather than against the part of it the
/// ledger finds convenient to admit.
struct Slot {
    device: DeviceId,
    field: FieldName,
    /// The entry's own timestamp, so an expiry sweep never has to reach into a
    /// shard to ask how old something is.
    at: Timestamp,
    /// What removing this entry reclaims: the `Read`, this slot, and one
    /// [`ENTRY_OVERHEAD`].
    bytes: u64,
}

impl Books {
    /// Estimated bytes held in total.
    fn used(&self) -> u64 {
        self.latest_bytes + self.history_bytes
    }

    /// File a newly written history entry.
    fn record(&mut self, slot: Slot) {
        self.history_bytes += slot.bytes;
        self.queue.push_back(slot);
    }

    /// Account for `latest` gaining `added` and losing `removed`.
    ///
    /// Saturating on the way down, and every subtraction in this file is. The
    /// figures are estimates, `removed` is measured against what a shard held a
    /// moment before the lock was taken, and the alternative to saturating is a
    /// debug-build overflow panic that unwinds a poll loop over an accounting
    /// discrepancy of a few dozen bytes. Bounding the store is worth more than
    /// bookkeeping that is exact or nothing.
    fn restate_latest(&mut self, added: u64, removed: u64) {
        self.latest_bytes = (self.latest_bytes + added).saturating_sub(removed);
    }

    /// Claim the oldest entries until the estimate is within `budget`.
    ///
    /// Stops when the queue empties, which is what a budget below the
    /// never-evicted floor of `latest` looks like from in here: everything
    /// evictable has been evicted and the store is still over. The caller
    /// learns that from [`Usage::over_budget`] rather than from an error,
    /// because there is nothing a *write* could have done differently.
    fn take_over_budget(&mut self, budget: u64) -> Vec<Slot> {
        let mut taken = Vec::new();
        while self.used() > budget {
            let Some(slot) = self.queue.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(slot.bytes);
            self.evicted += 1;
            taken.push(slot);
        }
        taken
    }

    /// Claim every entry stamped before `before`.
    ///
    /// A scan of the whole queue, not a pop of its expired prefix, and the
    /// difference is not an optimisation left on the table. The queue is in
    /// *write* order; the cutoff is in *timestamp* order. Those agree within one
    /// series — a series has exactly one poll loop appending to it, so its
    /// stamps only ever increase — but they do not agree across the store,
    /// because two devices' series interleave in whatever order their loops
    /// happen to tick. A prefix pop would therefore stop at the first entry of
    /// the first device that is still current and leave every older entry
    /// behind it standing, which is retention silently not being enforced on
    /// most of the fleet.
    ///
    /// The scan costs one pass over the live entries per sweep, against an
    /// interval measured in minutes. That is the cheaper half of the trade by
    /// several orders of magnitude.
    ///
    /// What survives from the write-order property is the part [`drop_front`]
    /// needs: because stamps increase within a series, the entries this claims
    /// from any one series are exactly that series' oldest, in order — so they
    /// can be applied by popping fronts rather than by searching each deque.
    ///
    /// [`drop_front`]: MemoryStore::drop_front
    fn take_expired(&mut self, before: &Timestamp) -> Vec<Slot> {
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(self.queue.len());

        // Order-preserving on both sides: `taken` stays in per-series order for
        // the caller to apply, and `kept` stays the eviction order the budget
        // reads.
        for slot in std::mem::take(&mut self.queue) {
            // Timestamps are RFC 3339 in UTC, which sorts lexicographically in
            // chronological order — the same property `TimeSpan::within` relies
            // on, and the reason this comparison needs no date library.
            if slot.at.0 < before.0 {
                self.history_bytes = self.history_bytes.saturating_sub(slot.bytes);
                taken.push(slot);
            } else {
                kept.push_back(slot);
            }
        }

        self.queue = kept;
        taken
    }
}

impl Ledger {
    /// Settle one write's whole effect on the books, and report what the budget
    /// wants gone as a result.
    ///
    /// The `latest` delta and the new history slot travel together under one
    /// lock because they are one write's effect: settling them apart would let
    /// the budget check run against half of it, and evict history to make room
    /// for a `latest` entry that has already been replaced.
    fn admit(&self, added: u64, displaced: u64, slot: Slot) -> Vec<Slot> {
        let mut books = self.books.lock().expect("ledger poisoned");
        books.restate_latest(added, displaced);
        books.record(slot);
        match self.budget {
            Some(budget) => books.take_over_budget(budget),
            None => Vec::new(),
        }
    }

    /// Claim everything stamped before `before`, settling the books for it.
    fn expire(&self, before: &Timestamp) -> Vec<Slot> {
        let mut books = self.books.lock().expect("ledger poisoned");
        books.take_expired(before)
    }
}

/// Estimated heap bytes one [`Read`] occupies, the struct itself included.
fn read_bytes(read: &Read) -> u64 {
    (size_of::<Read>() + read.device.len() + read.field.len() + read.at.0.len()) as u64
        + value_bytes(&read.value)
}

/// The part of a [`ReadValue`] that lives outside the enum.
///
/// Wildcard-free so a variant added to the wire model is a build error here
/// rather than a value silently estimated at zero — the same drift sentinel
/// `sismatic_sync::dto` uses at the other end of the same enum.
fn value_bytes(value: &ReadValue) -> u64 {
    let heap = match value {
        ReadValue::Text(s) | ReadValue::Version(s) | ReadValue::Ack(s) => s.len(),
        ReadValue::Mac(mac) => mac.0.len(),
        ReadValue::Alarms(alarms) => alarms
            .iter()
            .map(|a| size_of::<Alarm>() + a.name.len() + a.level.len())
            .sum(),
        // Stored inline in the enum, so they are already in `size_of::<Read>()`.
        ReadValue::Port(_) | ReadValue::Number(_) | ReadValue::Flag(_) | ReadValue::State(_) => 0,
    };
    heap as u64
}

/// What one `latest` entry costs: the read, the key it is filed under, and the
/// map node holding it.
fn latest_bytes(read: &Read) -> u64 {
    read_bytes(read) + read.field.len() as u64 + ENTRY_OVERHEAD
}

/// What one history entry costs: the read, the ledger slot that tracks it, and
/// the container node holding it.
fn history_bytes(read: &Read) -> u64 {
    read_bytes(read)
        + (size_of::<Slot>() + read.device.len() + read.field.len() + read.at.0.len()) as u64
        + ENTRY_OVERHEAD
}

#[async_trait::async_trait]
impl ReadStore for MemoryStore {
    async fn latest(&self, dev: DeviceId, field: FieldName) -> Result<Option<Read>, ReadError> {
        Ok(self
            .latest
            .get(&dev)
            .and_then(|fields| fields.get(&field).cloned()))
    }

    async fn latest_all(&self, dev: DeviceId) -> Result<Vec<Read>, ReadError> {
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
    ) -> Result<Vec<Read>, ReadError> {
        Ok(self
            .history
            .get(&dev)
            .and_then(|fields| {
                fields
                    .get(&field)
                    // Insertion order is chronological because the writer is a
                    // poll loop appending as it goes, and neither eviction nor
                    // expiry removes anything but a prefix — so filtering
                    // preserves the "oldest first" the port promises without a
                    // sort.
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
    async fn upsert_latest(&self, r: Read) -> Result<(), WriteError> {
        // Costed before the read is moved into the maps below, so the ledger
        // statements are computed from the value rather than from a clone kept
        // alive to be measured later.
        let slot = Slot {
            device: r.device.clone(),
            field: r.field.clone(),
            at: r.at.clone(),
            bytes: history_bytes(&r),
        };
        let added = latest_bytes(&r);

        // Keyed off the read's own `(device, field)`, so two poll loops on
        // one device write to two slots and neither evicts the other.
        //
        // The guard is scoped so it is gone before the ledger lock is taken —
        // see the module's lock discipline. What comes back out is the cost of
        // whatever this write displaced, which is the only thing the books need.
        let displaced = {
            let mut fields = self.latest.entry(r.device.clone()).or_default();
            fields
                .insert(r.field.clone(), r.clone())
                .map_or(0, |old| latest_bytes(&old))
        };

        {
            let mut fields = self.history.entry(r.device.clone()).or_default();
            fields.entry(r.field.clone()).or_default().push_back(r);
        }

        let evicted = self.ledger.admit(added, displaced, slot);
        self.drop_front(&evicted);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Lifecycle for MemoryStore {
    async fn prune(&self, before: Timestamp) -> Result<Swept, WriteError> {
        let expired = self.ledger.expire(&before);

        let swept = Swept {
            entries: expired.len() as u64,
            bytes: expired.iter().map(|slot| slot.bytes).sum(),
        };
        self.drop_front(&expired);
        Ok(swept)
    }

    async fn usage(&self) -> Result<Usage, WriteError> {
        let books = self.ledger.books.lock().expect("ledger poisoned");
        Ok(Usage {
            bytes: books.used(),
            entries: books.queue.len() as u64,
            budget: self.ledger.budget,
            evicted: books.evicted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{ReadValue, Timestamp};

    /// A `Read` for `device`/`field` with a `Number` value stamped at `at`.
    /// Keeps each test to the one axis it cares about (device, field, time, or
    /// value).
    fn read(device: &str, field: &str, value: u32, at: &str) -> Read {
        Read {
            device: device.into(),
            field: field.into(),
            value: ReadValue::Number(value),
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
    async fn all_history(store: &MemoryStore, dev: &str, field: &str) -> Vec<Read> {
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
            .upsert_latest(read("dev-1", "FIRMWARE", 1, "2026-07-23T14:00:00Z"))
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
    async fn upsert_then_latest_returns_the_read() {
        let store = MemoryStore::default();
        let r = read("dev-1", "RUNNING_STATE", 1, "2026-07-23T14:00:00Z");
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
        let first = read("dev-1", "SSH_PORT", 22, "2026-07-23T14:00:00Z");
        let second = read("dev-1", "SSH_PORT", 2222, "2026-07-23T14:05:00Z");
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
        let firmware = read("dev-1", "FIRMWARE", 211, "2026-07-23T14:00:00Z");
        let ssh_port = read("dev-1", "SSH_PORT", 22023, "2026-07-23T14:00:01Z");
        let state = read("dev-1", "RUNNING_STATE", 1, "2026-07-23T14:00:02Z");
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
        let a = read("dev-a", "F", 1, "2026-07-23T14:00:00Z");
        let b = read("dev-b", "F", 2, "2026-07-23T14:00:00Z");
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
            Vec::<Read>::new()
        );
    }

    #[tokio::test]
    async fn latest_all_returns_one_read_per_field_sorted_by_field_name() {
        let store = MemoryStore::default();
        // Written in an order that is neither sorted nor reverse-sorted, so a
        // passing assertion cannot be an accident of insertion order.
        let ssh_port = read("dev-1", "SSH_PORT", 22023, "2026-07-23T14:00:00Z");
        let firmware = read("dev-1", "FIRMWARE", 211, "2026-07-23T14:00:01Z");
        let timezone = read("dev-1", "TIMEZONE", 1, "2026-07-23T14:00:02Z");
        for r in [&ssh_port, &firmware, &timezone] {
            store.upsert_latest(r.clone()).await.unwrap();
        }
        // A repeat write must not add a second entry for the field.
        let firmware_again = read("dev-1", "FIRMWARE", 212, "2026-07-23T14:05:00Z");
        store.upsert_latest(firmware_again.clone()).await.unwrap();

        assert_eq!(
            store.latest_all("dev-1".into()).await.unwrap(),
            vec![firmware_again, ssh_port, timezone]
        );
    }

    #[tokio::test]
    async fn latest_all_is_scoped_to_one_device() {
        let store = MemoryStore::default();
        let mine = read("dev-a", "F", 1, "2026-07-23T14:00:00Z");
        store.upsert_latest(mine.clone()).await.unwrap();
        store
            .upsert_latest(read("dev-b", "G", 2, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        assert_eq!(store.latest_all("dev-a".into()).await.unwrap(), vec![mine]);
    }

    #[tokio::test]
    async fn between_is_empty_for_unknown_device() {
        let store = MemoryStore::default();
        assert_eq!(all_history(&store, "nobody", "T").await, Vec::<Read>::new());
    }

    #[tokio::test]
    async fn between_is_empty_for_a_field_with_no_history() {
        let store = MemoryStore::default();
        store
            .upsert_latest(read("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        assert_eq!(
            all_history(&store, "dev-1", "OTHER").await,
            Vec::<Read>::new()
        );
    }

    #[tokio::test]
    async fn between_keeps_every_upsert_in_insertion_order() {
        let store = MemoryStore::default();
        // Unlike `latest`, history accumulates every write — even repeats of the
        // same field — and preserves the order they arrived in.
        let r1 = read("dev-1", "T", 10, "2026-07-23T14:00:00Z");
        let r2 = read("dev-1", "T", 20, "2026-07-23T14:01:00Z");
        let r3 = read("dev-1", "T", 30, "2026-07-23T14:02:00Z");
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
        let t1 = read("dev-1", "T", 10, "2026-07-23T14:00:00Z");
        let other = read("dev-1", "OTHER", 99, "2026-07-23T14:00:30Z");
        let t2 = read("dev-1", "T", 20, "2026-07-23T14:01:00Z");
        for r in [&t1, &other, &t2] {
            store.upsert_latest(r.clone()).await.unwrap();
        }

        assert_eq!(all_history(&store, "dev-1", "T").await, vec![t1, t2]);
        assert_eq!(all_history(&store, "dev-1", "OTHER").await, vec![other]);
    }

    #[tokio::test]
    async fn between_filters_to_the_span_inclusive_of_bounds() {
        let store = MemoryStore::default();
        let before = read("dev-1", "T", 1, "2026-07-23T13:59:59Z");
        let on_start = read("dev-1", "T", 2, "2026-07-23T14:00:00Z");
        let inside = read("dev-1", "T", 3, "2026-07-23T14:30:00Z");
        let on_end = read("dev-1", "T", 4, "2026-07-23T15:00:00Z");
        let after = read("dev-1", "T", 5, "2026-07-23T15:00:01Z");
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
        // Both bounds are inclusive; the two straddling reads are excluded.
        assert_eq!(got, vec![on_start, inside, on_end]);
    }

    #[tokio::test]
    async fn between_can_return_empty_when_nothing_falls_in_span() {
        let store = MemoryStore::default();
        store
            .upsert_latest(read("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
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
        assert_eq!(got, Vec::<Read>::new());
    }

    #[tokio::test]
    async fn clone_shares_the_same_backing_store() {
        // `MemoryStore` holds `Arc<DashMap>`, so a clone is a handle to the same
        // data — a write through one is visible through the other.
        let store = MemoryStore::default();
        let handle = store.clone();
        let r = read("dev-1", "F", 7, "2026-07-23T14:00:00Z");
        store.upsert_latest(r.clone()).await.unwrap();

        assert_eq!(
            handle.latest("dev-1".into(), "F".into()).await.unwrap(),
            Some(r)
        );
    }

    // ---- the ledger ------------------------------------------------------

    /// A budget of exactly `entries` history entries of the shape `read`
    /// produces, plus the `latest` floor those same writes establish.
    ///
    /// Derived from the estimator rather than written as a literal, because the
    /// estimate is explicitly not a promised number — a test that hard-coded
    /// one would fail on a `Read` gaining a field, which is the one change that
    /// should *not* invalidate a claim about eviction order.
    fn budget_for(sample: &Read, entries: u64, series: u64) -> u64 {
        history_bytes(sample) * entries + latest_bytes(sample) * series
    }

    #[tokio::test]
    async fn an_empty_store_holds_nothing() {
        let store = MemoryStore::default();
        assert_eq!(
            store.usage().await.unwrap(),
            Usage {
                bytes: 0,
                entries: 0,
                budget: None,
                evicted: 0,
            }
        );
    }

    #[tokio::test]
    async fn usage_counts_every_write() {
        let store = MemoryStore::default();
        let r = read("dev-1", "T", 1, "2026-07-23T14:00:00Z");
        store.upsert_latest(r.clone()).await.unwrap();
        store
            .upsert_latest(read("dev-1", "T", 2, "2026-07-23T14:00:01Z"))
            .await
            .unwrap();

        let usage = store.usage().await.unwrap();
        assert_eq!(usage.entries, 2, "history counts both writes");
        // Two history entries, and one `latest` slot the second write replaced
        // in place rather than added to.
        assert_eq!(usage.bytes, budget_for(&r, 2, 1));
        assert!(!usage.over_budget(), "an unbounded store is never over");
    }

    #[tokio::test]
    async fn an_unbounded_store_evicts_nothing() {
        // The property every existing caller depends on: `default()` behaves
        // exactly as it did before there was a ledger at all.
        let store = MemoryStore::default();
        for n in 0..50 {
            store
                .upsert_latest(read("dev-1", "T", n, &format!("2026-07-23T14:00:{n:02}Z")))
                .await
                .unwrap();
        }

        assert_eq!(all_history(&store, "dev-1", "T").await.len(), 50);
        assert_eq!(store.usage().await.unwrap().evicted, 0);
    }

    #[tokio::test]
    async fn a_budget_evicts_the_oldest_entry_first() {
        let sample = read("dev-1", "T", 0, "2026-07-23T14:00:00Z");
        // Room for three history entries and the one `latest` slot they share.
        let store = MemoryStore::with_budget(Some(budget_for(&sample, 3, 1)));

        for n in 0..5 {
            store
                .upsert_latest(read("dev-1", "T", n, &format!("2026-07-23T14:00:{n:02}Z")))
                .await
                .unwrap();
        }

        // The two oldest went, in order, and the survivors are still ordered.
        let kept: Vec<u32> = all_history(&store, "dev-1", "T")
            .await
            .iter()
            .map(|r| match r.value {
                ReadValue::Number(n) => n,
                _ => unreachable!("the fixture writes numbers"),
            })
            .collect();
        assert_eq!(kept, [2, 3, 4]);
        assert_eq!(store.usage().await.unwrap().evicted, 2);
    }

    #[tokio::test]
    async fn eviction_keeps_the_store_within_its_budget() {
        // The property the budget exists for, stated over the estimate the
        // budget is actually enforced against: however many writes arrive, the
        // figure never ends a write above the cap.
        let sample = read("dev-1", "T", 0, "2026-07-23T14:00:00Z");
        let budget = budget_for(&sample, 4, 1);
        let store = MemoryStore::with_budget(Some(budget));

        for n in 0..200u32 {
            store
                .upsert_latest(read(
                    "dev-1",
                    "T",
                    n,
                    &format!("2026-07-23T14:{:02}:{:02}Z", n / 60, n % 60),
                ))
                .await
                .unwrap();
            let usage = store.usage().await.unwrap();
            assert!(
                !usage.over_budget(),
                "over budget after write {n}: {} > {budget}",
                usage.bytes
            );
        }
    }

    #[tokio::test]
    async fn eviction_crosses_devices_in_write_order() {
        // The eviction order is global, not per series: the oldest entry in the
        // *store* goes first, whichever device wrote it. A per-series bound
        // would let one chatty device keep its whole window while a quiet one
        // lost history it had barely any of.
        let sample = read("dev-a", "T", 0, "2026-07-23T14:00:00Z");
        // Two history entries, across two devices with a `latest` slot each.
        let store = MemoryStore::with_budget(Some(budget_for(&sample, 2, 2)));

        store
            .upsert_latest(read("dev-a", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();
        store
            .upsert_latest(read("dev-b", "T", 2, "2026-07-23T14:00:01Z"))
            .await
            .unwrap();
        store
            .upsert_latest(read("dev-b", "T", 3, "2026-07-23T14:00:02Z"))
            .await
            .unwrap();

        // `dev-a`'s entry was the oldest in the store, so it is what went.
        assert_eq!(all_history(&store, "dev-a", "T").await, Vec::<Read>::new());
        assert_eq!(all_history(&store, "dev-b", "T").await.len(), 2);
    }

    #[tokio::test]
    async fn eviction_never_touches_latest() {
        // A device whose history has been evicted entirely still answers a point
        // read, which is what keeps a fleet page from losing a recorder to a
        // budget rather than to an outage.
        let sample = read("dev-1", "T", 0, "2026-07-23T14:00:00Z");
        let store = MemoryStore::with_budget(Some(budget_for(&sample, 1, 1)));

        let first = read("dev-1", "T", 1, "2026-07-23T14:00:00Z");
        let second = read("dev-1", "T", 2, "2026-07-23T14:00:01Z");
        store.upsert_latest(first).await.unwrap();
        store.upsert_latest(second.clone()).await.unwrap();

        assert_eq!(
            store.latest("dev-1".into(), "T".into()).await.unwrap(),
            Some(second)
        );
    }

    #[tokio::test]
    async fn a_budget_below_the_latest_floor_reports_itself() {
        // The one condition eviction cannot fix. It is not an error — the write
        // succeeded and the data an operator needs is there — but it is a
        // misconfiguration, and `over_budget` is how the sweeper learns to say
        // so.
        let store = MemoryStore::with_budget(Some(1));
        store
            .upsert_latest(read("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        let usage = store.usage().await.unwrap();
        assert_eq!(usage.entries, 0, "everything evictable was evicted");
        assert!(usage.over_budget(), "and it is still over");
        // ...and the store did not spin trying: one entry in, one evicted.
        assert_eq!(usage.evicted, 1);
    }

    // ---- expiry ----------------------------------------------------------

    #[tokio::test]
    async fn prune_drops_entries_older_than_the_cutoff() {
        let store = MemoryStore::default();
        let old = read("dev-1", "T", 1, "2026-07-23T14:00:00Z");
        let cutoff = read("dev-1", "T", 2, "2026-07-23T15:00:00Z");
        let new = read("dev-1", "T", 3, "2026-07-23T16:00:00Z");
        for r in [&old, &cutoff, &new] {
            store.upsert_latest(r.clone()).await.unwrap();
        }

        let swept = store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();

        // The bound is exclusive, so the entry stamped exactly at it survives.
        assert_eq!(swept.entries, 1);
        assert_eq!(swept.bytes, history_bytes(&old));
        assert_eq!(all_history(&store, "dev-1", "T").await, vec![cutoff, new]);
    }

    #[tokio::test]
    async fn prune_reaches_every_device() {
        let store = MemoryStore::default();
        for dev in ["dev-a", "dev-b", "dev-c"] {
            store
                .upsert_latest(read(dev, "T", 1, "2026-07-23T14:00:00Z"))
                .await
                .unwrap();
            store
                .upsert_latest(read(dev, "T", 2, "2026-07-23T16:00:00Z"))
                .await
                .unwrap();
        }

        let swept = store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();

        assert_eq!(swept.entries, 3, "the old entry of each device");
        for dev in ["dev-a", "dev-b", "dev-c"] {
            assert_eq!(all_history(&store, dev, "T").await.len(), 1);
        }
    }

    #[tokio::test]
    async fn prune_reaches_an_expired_entry_written_after_a_current_one() {
        // The case a prefix pop gets wrong. Two devices' series interleave in
        // whatever order their poll loops tick, so an expired entry can sit
        // behind a current one in write order — and a sweep that stopped at the
        // first current entry would leave it, and everything after it, standing.
        let store = MemoryStore::default();
        let current = read("dev-a", "T", 1, "2026-07-23T16:00:00Z");
        let expired = read("dev-b", "T", 2, "2026-07-23T14:00:00Z");
        store.upsert_latest(current.clone()).await.unwrap();
        store.upsert_latest(expired).await.unwrap();

        let swept = store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();

        assert_eq!(swept.entries, 1);
        assert_eq!(all_history(&store, "dev-a", "T").await, vec![current]);
        assert_eq!(all_history(&store, "dev-b", "T").await, Vec::<Read>::new());
    }

    #[tokio::test]
    async fn prune_leaves_the_survivors_in_eviction_order() {
        // A sweep rebuilds the ledger's queue, so it has to put it back in the
        // order the budget reads: whatever the sweep kept must still evict
        // oldest-first afterwards.
        let sample = read("dev-1", "T", 0, "2026-07-23T14:00:00Z");
        let store = MemoryStore::with_budget(Some(budget_for(&sample, 2, 1)));

        for hour in [14, 16, 17] {
            store
                .upsert_latest(read(
                    "dev-1",
                    "T",
                    hour,
                    &format!("2026-07-23T{hour}:00:00Z"),
                ))
                .await
                .unwrap();
        }
        // The 14:00 entry expires; 16:00 and 17:00 remain, filling the budget.
        store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();
        // One more write has to displace 16:00 — the oldest survivor — rather
        // than whatever the rebuild happened to leave at the front.
        store
            .upsert_latest(read("dev-1", "T", 18, "2026-07-23T18:00:00Z"))
            .await
            .unwrap();

        let kept: Vec<u32> = all_history(&store, "dev-1", "T")
            .await
            .iter()
            .map(|r| match r.value {
                ReadValue::Number(n) => n,
                _ => unreachable!("the fixture writes numbers"),
            })
            .collect();
        assert_eq!(kept, [17, 18]);
    }

    #[tokio::test]
    async fn prune_leaves_latest_alone() {
        // The retention window bounds *history*. A device that stopped
        // reporting before the cutoff keeps its last known value, because a
        // stale reading is the finding — dropping it would present a configured
        // recorder as one that never answered.
        let store = MemoryStore::default();
        let ancient = read("dev-1", "T", 1, "2020-01-01T00:00:00Z");
        store.upsert_latest(ancient.clone()).await.unwrap();

        store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();

        assert_eq!(all_history(&store, "dev-1", "T").await, Vec::<Read>::new());
        assert_eq!(
            store.latest("dev-1".into(), "T".into()).await.unwrap(),
            Some(ancient)
        );
    }

    #[tokio::test]
    async fn prune_is_idempotent() {
        // What lets the sweeper tick on a fixed interval without first asking
        // whether anything is due.
        let store = MemoryStore::default();
        store
            .upsert_latest(read("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        let cutoff = Timestamp("2026-07-23T15:00:00Z".into());
        let first = store.prune(cutoff.clone()).await.unwrap();
        let second = store.prune(cutoff).await.unwrap();

        assert_eq!(first.entries, 1);
        assert_eq!(second, Swept::default(), "nothing left to take");
    }

    #[tokio::test]
    async fn prune_returns_the_bytes_to_the_ledger() {
        // The accounting property the budget depends on: expiry and eviction
        // settle the same books, so a sweep genuinely makes room rather than
        // only deleting.
        let store = MemoryStore::default();
        let r = read("dev-1", "T", 1, "2026-07-23T14:00:00Z");
        store.upsert_latest(r.clone()).await.unwrap();

        store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();

        let usage = store.usage().await.unwrap();
        assert_eq!(usage.entries, 0);
        // Down to the `latest` floor, which expiry does not touch.
        assert_eq!(usage.bytes, latest_bytes(&r));
    }

    #[tokio::test]
    async fn expiry_makes_room_the_budget_can_use() {
        // The two axes composed: a sweep that reclaims bytes lets subsequent
        // writes land without evicting, which is the whole reason a deployment
        // sets both.
        let sample = read("dev-1", "T", 0, "2026-07-23T14:00:00Z");
        let store = MemoryStore::with_budget(Some(budget_for(&sample, 2, 1)));

        store
            .upsert_latest(read("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();
        store
            .upsert_latest(read("dev-1", "T", 2, "2026-07-23T14:00:01Z"))
            .await
            .unwrap();
        store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();
        store
            .upsert_latest(read("dev-1", "T", 3, "2026-07-23T16:00:00Z"))
            .await
            .unwrap();

        // The third write found an empty store rather than a full one, so
        // nothing was ever evicted.
        assert_eq!(store.usage().await.unwrap().evicted, 0);
        assert_eq!(all_history(&store, "dev-1", "T").await.len(), 1);
    }

    #[tokio::test]
    async fn an_emptied_series_leaves_no_key_behind() {
        // Otherwise a fleet that shrinks — or a field that stops being polled —
        // leaks one map key per name that ever appeared, which is unbounded
        // growth of exactly the kind the ledger exists to stop.
        let store = MemoryStore::default();
        store
            .upsert_latest(read("dev-1", "T", 1, "2026-07-23T14:00:00Z"))
            .await
            .unwrap();

        store
            .prune(Timestamp("2026-07-23T15:00:00Z".into()))
            .await
            .unwrap();

        assert!(store.history.is_empty(), "the device key went with it");
    }

    #[tokio::test]
    async fn value_size_is_counted() {
        // A long metadata string has to cost more than a port number, or the
        // budget would be blind to the one field that can actually be large.
        let short = Read {
            value: ReadValue::Port(22),
            ..read("dev-1", "T", 0, "2026-07-23T14:00:00Z")
        };
        let long = Read {
            value: ReadValue::Text("x".repeat(4096)),
            ..read("dev-1", "T", 0, "2026-07-23T14:00:00Z")
        };
        assert!(history_bytes(&long) > history_bytes(&short) + 4000);
    }
}
