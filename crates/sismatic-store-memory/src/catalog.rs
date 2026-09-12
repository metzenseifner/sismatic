//! A [`DeviceCatalog`] over a fixed set of summaries.
//!
//! Unlike the other two adapters here, this one is not "the in-memory version
//! of something a database would do better". The catalog *is* static — the
//! device set comes from a file read before the server binds — so holding it in
//! two sorted vectors is the whole implementation, and a SQL adapter would
//! exist only to serve a catalog that came from somewhere else.
//!
//! Sorted at construction rather than on every read: the port promises ordered
//! output, the input is written once, and sorting once at startup is the
//! cheapest place to keep that promise.

use std::sync::{Arc, RwLock};

use sismatic_api_types::{DeviceSummary, GroupSummary};
use sismatic_store::catalog::DeviceCatalog;

/// The configured set, behind locks so it can be replaced while the server runs.
///
/// It was two plain `Vec`s until the fleet could change — the module docs above
/// still say why that was the whole implementation, and the reasoning holds for
/// everything except *when* the set is written. A `RwLock` rather than a
/// `DashMap`: the set is read on every inventory request and written when an
/// operator adds or removes a recorder, so readers should not contend with each
/// other and a writer taking the whole thing is free at that rate.
///
/// `Arc` inside, so the clones the composition root hands out are handles on one
/// catalog. Without it a `replace` through one clone would leave every other
/// clone serving the old fleet.
#[derive(Debug, Clone, Default)]
pub struct MemoryCatalog {
    devices: Arc<RwLock<Vec<DeviceSummary>>>,
    groups: Arc<RwLock<Vec<GroupSummary>>>,
}

impl MemoryCatalog {
    /// Build a catalog over `devices` and `groups`.
    ///
    /// Both are sorted by id here, so [`DeviceCatalog::devices`] and
    /// [`DeviceCatalog::groups`] can promise an order without re-sorting per
    /// request. Member lists are left in configured order: a group's members
    /// are written deliberately, and `[atrium, annex]` is the operator's
    /// sequence rather than an arbitrary one to normalise away.
    pub fn new(mut devices: Vec<DeviceSummary>, mut groups: Vec<GroupSummary>) -> Self {
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        groups.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            devices: Arc::new(RwLock::new(devices)),
            groups: Arc::new(RwLock::new(groups)),
        }
    }

    /// Adopt a new device and group set wholesale.
    ///
    /// For a fleet that changes while the server runs. Wholesale rather than
    /// per-device because the caller is holding the freshly resolved set anyway
    /// — and because a device *replaced* is a different device, so an
    /// incremental API would need the same three verbs the inventory port
    /// already has, duplicated here with nothing new to say.
    ///
    /// Takes `&self` and not `&mut self`: this is called through the shared
    /// handle the composition root keeps, which is what makes it reachable from
    /// a request at all. Sorting happens here for the same reason
    /// [`new`](Self::new) does it — the port promises an order, and promising it
    /// once at the point of construction is what keeps every reader from
    /// re-establishing it.
    pub fn replace(&self, mut devices: Vec<DeviceSummary>, mut groups: Vec<GroupSummary>) {
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        groups.sort_by(|a, b| a.id.cmp(&b.id));
        *self.devices.write().expect("the catalog is poisoned") = devices;
        *self.groups.write().expect("the catalog is poisoned") = groups;
    }
}

#[async_trait::async_trait]
impl DeviceCatalog for MemoryCatalog {
    async fn devices(&self) -> Vec<DeviceSummary> {
        self.devices
            .read()
            .expect("the catalog is poisoned")
            .clone()
    }

    async fn groups(&self) -> Vec<GroupSummary> {
        self.groups.read().expect("the catalog is poisoned").clone()
    }

    // Linear rather than a map lookup, deliberately. A fleet is tens of
    // devices, not thousands, and a `Vec` that is already sorted for the two
    // list methods costs nothing to keep — a second `HashMap` beside it would
    // be a second thing to keep in step for a scan that is faster than the JSON
    // serialisation of its own result.
    async fn device(&self, id: &str) -> Option<DeviceSummary> {
        self.devices
            .read()
            .expect("the catalog is poisoned")
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    async fn group(&self, id: &str) -> Option<GroupSummary> {
        self.groups
            .read()
            .expect("the catalog is poisoned")
            .iter()
            .find(|g| g.id == id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{Barrier, ConnectionStatus};

    fn device(id: &str) -> DeviceSummary {
        DeviceSummary {
            id: id.to_owned(),
            uuid: "00000000-0000-0000-0000-000000000000".to_owned(),
            host: "10.0.0.1".to_owned(),
            port: 22023,
            eager: false,
            status: ConnectionStatus::Unknown,
            disabled_fields: Vec::new(),
            auto_disabled_fields: Vec::new(),
        }
    }

    fn group(id: &str, members: &[&str]) -> GroupSummary {
        GroupSummary {
            id: id.to_owned(),
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            barrier_timeout_secs: 15,
            barrier: Barrier::FailBatch,
        }
    }

    fn catalog() -> MemoryCatalog {
        MemoryCatalog::new(
            vec![device("atrium"), device("annex")],
            vec![group("room", &["atrium", "annex"])],
        )
    }

    #[tokio::test]
    async fn listing_is_ordered_by_id_whatever_order_it_was_built_in() {
        let ids: Vec<String> = catalog()
            .devices()
            .await
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, ["annex", "atrium"]);
    }

    #[tokio::test]
    async fn a_groups_members_stay_in_configured_order() {
        // Not sorted: the operator wrote the sequence, and a fan-out that
        // reordered it would address the device group differently than it
        // reads.
        let groups = catalog().groups().await;
        assert_eq!(groups[0].members, ["atrium", "annex"]);
    }

    #[tokio::test]
    async fn a_device_and_a_group_both_resolve_but_never_as_each_other() {
        let catalog = catalog();
        assert!(catalog.device("atrium").await.is_some());
        assert!(catalog.group("atrium").await.is_none());
        assert!(catalog.group("room").await.is_some());
        assert!(catalog.device("room").await.is_none());
    }

    /// The check the write routes use to turn an unknown target into a `404`
    /// instead of a `202` for something that can never happen.
    #[tokio::test]
    async fn contains_covers_both_kinds_and_nothing_else() {
        let catalog = catalog();
        assert!(catalog.contains("atrium").await);
        assert!(catalog.contains("room").await);
        assert!(!catalog.contains("typo").await);
    }

    /// The shape a group fan-out needs: one call answers "which devices does
    /// this id address", whichever kind of id it is.
    #[tokio::test]
    async fn members_expands_a_group_and_wraps_a_device() {
        let catalog = catalog();
        assert_eq!(
            catalog.members("room").await,
            Some(vec!["atrium".to_owned(), "annex".to_owned()])
        );
        assert_eq!(
            catalog.members("atrium").await,
            Some(vec!["atrium".to_owned()])
        );
        assert_eq!(catalog.members("typo").await, None);
    }

    #[tokio::test]
    async fn an_empty_catalog_contains_nothing() {
        let catalog = MemoryCatalog::default();
        assert!(catalog.devices().await.is_empty());
        assert!(catalog.groups().await.is_empty());
        assert!(!catalog.contains("anything").await);
    }
}
