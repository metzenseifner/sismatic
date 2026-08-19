//! The storage port: the seam the two halves of the system meet across.
//!
//! `sismatic-sync` writes through [`WriteStore`], `sismatic-http-api` reads
//! through [`ReadStore`], and only the composition root knows the two traits are
//! one object. Neither side can name the other, which is what keeps the read
//! side free of a compile path to the device model.
//!
//! # Why every method is keyed by `(device, field)`
//!
//! A [`Reading`] is a statement about one *quantity* on one device — the value
//! of `SSH_PORT` on `atrium-101`, as of some instant — so "the latest reading"
//! is only a well-formed question once a field is named. Keying on the device
//! alone would make the store hold one reading per device, and since the sync
//! driver runs an independent poll loop per `(device, field)` pair (dozens of
//! them under a `'*'` wildcard schedule), each loop's write would evict the
//! last: `latest` would answer with whichever field happened to be polled most
//! recently, and no caller could ask for the field it actually wanted.
//!
//! The pair is therefore the store's primary key, and it is the write side's
//! grain as well — one poll loop, one key, no two loops contending for a slot.
//! [`latest_all`](ReadStore::latest_all) exists because "everything known about
//! this device" is a genuinely different question from a point read, and one an
//! adapter can answer far better than a caller looping over a field catalog it
//! would first have to obtain from somewhere.

use std::sync::Arc;

use sismatic_api_types::{DeviceId, FieldName, Reading, TimeSpan};

pub mod catalog;
pub mod error;
pub mod outbox;
pub mod status;

pub use catalog::{DeviceCatalog, DynDeviceCatalog};
pub use error::{ReadError, WriteError};
pub use status::{DeviceStatus, DynDeviceStatus};

/// A convenient object-safe handle.
pub type DynReadStore = Arc<dyn ReadStore>;
pub type DynWriteStore = Arc<dyn WriteStore>;

/// Reading persisted state. Absence is never an error — see [`error`].
#[async_trait::async_trait]
pub trait ReadStore: Send + Sync {
    /// The most recent reading of `field` on `dev`, or `None` if this pair has
    /// never been recorded.
    ///
    /// `None` deliberately conflates "no such device", "no such field" and
    /// "known pair, never yet polled": the store records what the sync side
    /// wrote and holds no catalog of what *could* be written, so it cannot tell
    /// the three apart. A caller that must distinguish them needs the device
    /// registry or the instruction catalog, neither of which is this port's job.
    async fn latest(&self, dev: DeviceId, field: FieldName) -> Result<Option<Reading>, ReadError>;

    /// The most recent reading of *every* field recorded for `dev`, ordered by
    /// field name.
    ///
    /// Ordered so the rendered JSON is deterministic — stable diffs, stable
    /// snapshot tests — rather than reflecting an adapter's hash iteration
    /// order. An unknown device yields an empty vector, for the same reason
    /// [`latest`](Self::latest) yields `None`.
    async fn latest_all(&self, dev: DeviceId) -> Result<Vec<Reading>, ReadError>;

    /// Every recorded reading of `field` on `dev` whose timestamp falls in
    /// `span`, oldest first.
    ///
    /// The field is required rather than optional because a history is a series
    /// of one quantity: interleaving `SSH_PORT` and `RUNNING_STATE` into one
    /// time-ordered list produces a sequence no caller can plot and every caller
    /// would immediately re-partition by field.
    async fn between(
        &self,
        dev: DeviceId,
        field: FieldName,
        span: TimeSpan,
    ) -> Result<Vec<Reading>, ReadError>;
}

/// Persisting what the poll loops read.
#[async_trait::async_trait]
pub trait WriteStore: Send + Sync {
    /// Record `reading` as the current value of its `(device, field)` pair, and
    /// append it to that pair's history.
    ///
    /// The key is carried *inside* the reading rather than passed alongside it,
    /// so a caller cannot file a reading under a pair it does not belong to.
    async fn upsert_latest(&self, reading: Reading) -> Result<(), WriteError>;
}
