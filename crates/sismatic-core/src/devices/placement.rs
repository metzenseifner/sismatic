//! Where a device is actually reached, when that is not where the document
//! says.
//!
//! A devices file names the real recorders' addresses. Two programs read one,
//! and both sometimes need to be somewhere else: `simulated-fleet` *binds* a
//! unit per device, and `sismatic-server` *dials* one. Pointing both at a
//! production inventory and running them side by side on a laptop only works
//! if the two agree on where every device went — so the rule is written once,
//! here, as [`Placement::place`], and each program calls it rather than
//! re-stating it.
//!
//! # The rule
//!
//! Two independent overrides, each optional:
//!
//! - **host** replaces every device's host.
//! - **port base** replaces every device's port with `base + index`, where
//!   `index` is the device's position in the document (0-based).
//!
//! A base of `0` is the one exception: every device gets port `0`. For a
//! listener that means "kernel, choose", which is useful to the simulator and
//! meaningless to a client — so the server refuses `0` at its flag rather than
//! here, and this rule stays the simulator's verbatim.
//!
//! # Why by index, and why the index is pinned
//!
//! The index is what both sides can compute from the same file without talking
//! to each other. But the server's fleet changes while it runs — a device added
//! through the API, a group edited, an inventory reset — and an index read off
//! the *current* fleet would move every device after a removed one onto its
//! neighbor's port, which the simulator, still serving the startup document,
//! never did. So [`PlacedConnector`] pins each id to its slot the first time it
//! sees it: the startup document's devices in document order, and any id that
//! arrives later on the next slot after the highest one handed out. A slot is
//! never reused, so a device that leaves and comes back returns to the port it
//! had.
//!
//! # What is not touched
//!
//! Only the address the connector dials. The [`DeviceConfig`] the registry
//! holds — and therefore its UUID, the catalog the API serves, the persisted
//! runtime state and every export — keeps the document's own address. A test
//! run must not be able to leak `127.0.0.1` into a file that later goes back to
//! production.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::config::DeviceConfig;
use super::connector::{ConnectError, Connector};
use super::transport::Transport;

/// How to place a fleet: the two overrides, or neither.
///
/// [`Placement::default`] is the identity — every device stays where the
/// document put it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// Every device's host, instead of its own.
    pub host: Option<String>,
    /// Ports from here upward, in document order, instead of each device's own.
    pub port_base: Option<u16>,
}

impl Placement {
    /// Whether this placement leaves every device where it is.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.host.is_none() && self.port_base.is_none()
    }

    /// Where the `index`-th device of the document goes, given where it was
    /// declared.
    ///
    /// Ports count from `port_base` in document order, with the one exception
    /// the module documents: a base of `0` places every device on `0`, because
    /// port 0 means "kernel, choose" and incrementing from it would ask for real
    /// ports 1, 2, 3 — privileged, and not what anyone meant.
    ///
    /// Saturating rather than wrapping past `u16::MAX`: a wrapped port would be
    /// a plausible-looking address belonging to some other device, while a
    /// saturated one is a collision the simulator reports before binding.
    #[must_use]
    pub fn place(&self, host: &str, port: u16, index: usize) -> (String, u16) {
        let host = self.host.clone().unwrap_or_else(|| host.to_owned());
        let port = match self.port_base {
            None => port,
            Some(0) => 0,
            Some(base) => base.saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
        };
        (host, port)
    }
}

/// Which slot each device id holds — see the [module docs](self).
#[derive(Debug, Default)]
struct Slots {
    by_id: HashMap<String, usize>,
    next: usize,
}

impl Slots {
    fn slot_of(&mut self, id: &str) -> usize {
        if let Some(&slot) = self.by_id.get(id) {
            return slot;
        }
        let slot = self.next;
        self.next += 1;
        self.by_id.insert(id.to_owned(), slot);
        slot
    }
}

/// A [`Connector`] that dials each device where a [`Placement`] puts it, and
/// otherwise defers to the connector it wraps.
pub struct PlacedConnector<C> {
    inner: C,
    placement: Placement,
    slots: Mutex<Slots>,
}

impl<C> PlacedConnector<C> {
    /// Wrap `inner`, pinning `ids` — the startup document's devices, in
    /// document order — to slots `0, 1, 2, …`.
    ///
    /// The order is the whole contract with the simulator: it enumerates the
    /// same document the same way, so device `n` is listening on
    /// `port_base + n` and this dials exactly that.
    pub fn new<I, S>(inner: C, placement: Placement, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut slots = Slots::default();
        for id in ids {
            slots.slot_of(id.as_ref());
        }
        Self {
            inner,
            placement,
            slots: Mutex::new(slots),
        }
    }

    /// The address `config` is dialed at, pinning a slot for its id if it has
    /// none yet.
    pub fn address_of(&self, config: &DeviceConfig) -> (String, u16) {
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .slot_of(&config.id);
        self.placement.place(&config.host, config.port, slot)
    }
}

#[async_trait]
impl<C: Connector> Connector for PlacedConnector<C> {
    async fn connect(&self, config: &DeviceConfig) -> Result<Box<dyn Transport>, ConnectError> {
        if self.placement.is_identity() {
            return self.inner.connect(config).await;
        }
        let (host, port) = self.address_of(config);
        let placed = DeviceConfig {
            host,
            port,
            ..config.clone()
        };
        self.inner.connect(&placed).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::time::Duration;

    use uuid::Uuid;

    /// A connector that never connects, and says where it was asked to.
    struct Echo;

    #[async_trait]
    impl Connector for Echo {
        async fn connect(&self, config: &DeviceConfig) -> Result<Box<dyn Transport>, ConnectError> {
            Err(ConnectError::Failed(format!(
                "{}:{}",
                config.host, config.port
            )))
        }
    }

    fn device(id: &str, host: &str, port: u16) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            host: host.into(),
            port,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(500),
            exchange_timeout: Duration::from_millis(500),
            eager: false,
            sis_keepalive: None,
            eager_retry: None,
            cold_backoff: None,
            uuid: Uuid::nil(),
            disabled_fields: BTreeSet::new(),
            auto_disable_after: 0,
            self_heal: None,
        }
        .derive_uuid()
    }

    async fn dialed(connector: &impl Connector, config: &DeviceConfig) -> String {
        match connector.connect(config).await {
            Err(ConnectError::Failed(addr)) => addr,
            other => panic!("Echo only ever fails, got {:?}", other.map(|_| ())),
        }
    }

    fn local(base: u16) -> Placement {
        Placement {
            host: Some("127.0.0.1".into()),
            port_base: Some(base),
        }
    }

    #[test]
    fn the_default_placement_is_the_identity() {
        let placement = Placement::default();
        assert!(placement.is_identity());
        assert_eq!(
            placement.place("10.0.0.1", 22023, 7),
            ("10.0.0.1".into(), 22023)
        );
    }

    #[test]
    fn the_two_overrides_are_independent() {
        let host_only = Placement {
            host: Some("127.0.0.1".into()),
            port_base: None,
        };
        assert_eq!(
            host_only.place("10.0.0.1", 22023, 3),
            ("127.0.0.1".into(), 22023)
        );
        let ports_only = Placement {
            host: None,
            port_base: Some(2301),
        };
        assert_eq!(
            ports_only.place("10.0.0.1", 22023, 3),
            ("10.0.0.1".into(), 2304)
        );
    }

    #[test]
    fn a_base_of_zero_is_zero_for_every_index() {
        assert_eq!(local(0).place("10.0.0.1", 22023, 5).1, 0);
    }

    #[test]
    fn ports_saturate_rather_than_wrap() {
        assert_eq!(local(65_534).place("h", 1, 5).1, u16::MAX);
    }

    #[tokio::test]
    async fn the_startup_document_is_dialed_in_document_order() {
        let connector = PlacedConnector::new(Echo, local(2301), ["a", "b", "c"]);
        // Asked out of order: the slot is the document's, not the dial order's.
        assert_eq!(
            dialed(&connector, &device("c", "10.0.0.3", 22023)).await,
            "127.0.0.1:2303"
        );
        assert_eq!(
            dialed(&connector, &device("a", "10.0.0.1", 22023)).await,
            "127.0.0.1:2301"
        );
        assert_eq!(
            dialed(&connector, &device("b", "10.0.0.2", 22023)).await,
            "127.0.0.1:2302"
        );
    }

    #[tokio::test]
    async fn a_device_added_later_takes_the_next_slot_and_keeps_it() {
        let connector = PlacedConnector::new(Echo, local(2301), ["a", "b"]);
        assert_eq!(
            dialed(&connector, &device("new", "10.0.0.9", 22023)).await,
            "127.0.0.1:2303"
        );
        // Dialed again — a reconnect, or the same id back after a removal.
        assert_eq!(
            dialed(&connector, &device("new", "10.0.0.9", 22023)).await,
            "127.0.0.1:2303"
        );
        assert_eq!(
            dialed(&connector, &device("b", "10.0.0.2", 22023)).await,
            "127.0.0.1:2302"
        );
    }

    #[tokio::test]
    async fn the_identity_placement_dials_the_document_unchanged() {
        let connector = PlacedConnector::new(Echo, Placement::default(), ["a"]);
        assert_eq!(
            dialed(&connector, &device("a", "10.0.0.1", 22023)).await,
            "10.0.0.1:22023"
        );
    }
}
