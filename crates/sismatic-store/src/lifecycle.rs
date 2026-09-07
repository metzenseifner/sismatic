//! The retention port: bounding what the store holds, in time and in bytes.
//!
//! [`ReadStore`](crate::ReadStore) and [`WriteStore`](crate::WriteStore) say
//! what is *recorded*; this port says what stops being recorded. It is a third
//! port rather than a pair of methods on the write side because the two are
//! written by different callers on different clocks: the sync driver writes a
//! read the instant a device answers, and the sweeper deletes on a schedule
//! nobody polls. A `WriteStore` that also pruned would hand every poll loop a
//! method it must never call.
//!
//! # Why the port exists at all, given a database would not need it
//!
//! A durable backend bounds retention with a partition drop or a `DELETE`
//! scheduled outside this process, so its [`prune`](Lifecycle::prune) is a
//! no-op that reports zero. The in-memory adapter has no such outside, which is
//! what [the store's module docs](crate) call the reason it "is not the
//! deployment story". This port is that reason answered: retention is stated
//! once, as configuration, and each adapter implements it with whatever it has.
//!
//! # What a cutoff is, and what it is not
//!
//! [`prune`](Lifecycle::prune) takes an *instant*, not a policy. Deciding
//! whether that instant is "thirty days before now" or a fixed floor an operator
//! wrote down is the caller's job, and keeping it there is what makes this
//! method testable without a clock: the same call with the same argument does
//! the same thing forever. The composition root owns the policy — see
//! `sismatic_server::lifecycle::Retention`.
//!
//! # The byte budget is deliberately absent
//!
//! There is no `prune_to_bytes`. A memory cap has to hold *between* sweeps —
//! a burst of writes that lands one second after a sweep must not be able to
//! exhaust the machine before the next one — so it is enforced by the adapter
//! on the write path, where every entry passes, rather than by a caller on a
//! timer. What crosses this port is the resulting [`Usage`], so an operator can
//! see the budget working without the sweeper having to know how.

use std::sync::Arc;

use sismatic_api_types::Timestamp;

use crate::WriteError;

/// A convenient object-safe handle, as [`DynReadStore`](crate::DynReadStore) is.
pub type DynLifecycle = Arc<dyn Lifecycle>;

/// Bounding the store's contents.
#[async_trait::async_trait]
pub trait Lifecycle: Send + Sync {
    /// Drop every recorded read older than `before`, and report what went.
    ///
    /// "Older" is by the read's own [`at`](sismatic_api_types::Read::at), not by
    /// when it was written, so a backfilled read expires on the schedule its
    /// timestamp implies rather than the one its arrival did.
    ///
    /// The bound is exclusive: an entry stamped exactly `before` is kept. That
    /// makes a cutoff the caller derived as `now - retain` mean "the last
    /// `retain` of history, inclusive", which is what an operator writing
    /// `retain: 30d` is asking for.
    ///
    /// Idempotent, and calling it more often than necessary is only wasted work
    /// — which is what lets a sweeper tick on a fixed interval without first
    /// asking whether anything is due.
    async fn prune(&self, before: Timestamp) -> Result<Swept, WriteError>;

    /// What the store currently holds.
    ///
    /// Separate from [`prune`](Self::prune)'s return because it answers a
    /// different question — "how full is it" rather than "what did that call
    /// remove" — and because it is meaningful on a tick that pruned nothing,
    /// which is the tick an operator watching a budget cares about most.
    async fn usage(&self) -> Result<Usage, WriteError>;
}

/// What one [`prune`](Lifecycle::prune) removed.
///
/// Both counts, because either alone misleads: entries alone cannot say whether
/// a sweep reclaimed anything worth the wake-up, and bytes alone cannot say
/// whether the retention window is doing anything at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    /// How many recorded reads were dropped.
    pub entries: u64,
    /// The store's own estimate of the bytes they held.
    pub bytes: u64,
}

/// How much the store is holding, and against what limit.
///
/// `bytes` is an *estimate* wherever the adapter cannot ask its allocator — see
/// `sismatic_store_memory` for what its own estimate counts and what it cannot
/// see. It is reported rather than hidden precisely because an operator tuning
/// a budget needs the same number the budget is enforced against, not a better
/// one measured differently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Estimated bytes currently held.
    pub bytes: u64,
    /// How many recorded reads are held, across every `(device, field)` series.
    pub entries: u64,
    /// The configured cap, or `None` when the adapter is unbounded.
    pub budget: Option<u64>,
    /// How many entries the budget has evicted since the process started.
    ///
    /// A cumulative counter rather than a rate, so a caller can difference two
    /// readings and get whichever window it wanted. Non-zero means the budget —
    /// not the retention window — is what is deciding how much history exists,
    /// which is the one condition under which `retain` is silently not being
    /// honoured.
    pub evicted: u64,
}

impl Usage {
    /// Whether the store is over the cap it was given.
    ///
    /// Possible even for an adapter that evicts on every write: nothing a
    /// budget can evict brings it under one smaller than the store's
    /// never-evicted floor — see `sismatic_store_memory`'s note on `latest`.
    /// A caller that finds this true has a misconfiguration to report, not a
    /// transient to retry.
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.budget.is_some_and(|cap| self.bytes > cap)
    }
}
