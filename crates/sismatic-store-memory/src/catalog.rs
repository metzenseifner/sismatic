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

use sismatic_api_types::{DeviceSummary, GroupSummary};
use sismatic_store::catalog::DeviceCatalog;

#[derive(Debug, Clone, Default)]
pub struct MemoryCatalog {
    devices: Vec<DeviceSummary>,
    groups: Vec<GroupSummary>,
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
        Self { devices, groups }
    }
}

#[async_trait::async_trait]
impl DeviceCatalog for MemoryCatalog {
    async fn devices(&self) -> Vec<DeviceSummary> {
        self.devices.clone()
    }

    async fn groups(&self) -> Vec<GroupSummary> {
        self.groups.clone()
    }

    // Linear rather than a map lookup, deliberately. A fleet is tens of
    // devices, not thousands, and a `Vec` that is already sorted for the two
    // list methods costs nothing to keep — a second `HashMap` beside it would
    // be a second thing to keep in step for a scan that is faster than the JSON
    // serialisation of its own result.
    async fn device(&self, id: &str) -> Option<DeviceSummary> {
        self.devices.iter().find(|d| d.id == id).cloned()
    }

    async fn group(&self, id: &str) -> Option<GroupSummary> {
        self.groups.iter().find(|g| g.id == id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::ConnectionStatus;

    fn device(id: &str) -> DeviceSummary {
        DeviceSummary {
            id: id.to_owned(),
            host: "10.0.0.1".to_owned(),
            port: 22023,
            eager: false,
            status: ConnectionStatus::Unknown,
        }
    }

    fn group(id: &str, members: &[&str]) -> GroupSummary {
        GroupSummary {
            id: id.to_owned(),
            members: members.iter().map(|m| (*m).to_owned()).collect(),
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
        // reordered it would address the room differently than it reads.
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
