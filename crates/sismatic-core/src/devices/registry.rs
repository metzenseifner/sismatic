//! The set of known devices and groups, keyed by id.
//!
//! A [`Registry`] is built once from a `devices.toml` and a shared
//! [`Connector`], and hands out `Arc<Device>` (or `Arc<DeviceGroup>`) by id.
//! Because every lookup of the same id returns the same [`Device`], callers
//! transparently share that device's one warm connection — the registry is the
//! keep-warm cache, one entry per device.
//!
//! A [`DeviceGroup`] is a name over several of those same device handles, so a
//! caller can address a whole room by one id; its members reuse the very warm
//! connections the registry already holds. Device and group ids share one
//! namespace (the config layer guarantees they never collide), so [`target`]
//! resolves either kind from a single id.
//!
//! [`target`]: Registry::target

use std::sync::Arc;

use dashmap::DashMap;

use super::config::{DeviceConfig, GroupConfig};
use super::connector::Connector;
use super::device::Device;
use super::group::DeviceGroup;

/// What an id resolves to: a lone device or a group of them. Both answer the
/// same instructions, so a facade can run against either after one lookup.
pub enum Target {
    Device(Arc<Device>),
    Group(Arc<DeviceGroup>),
}

/// A lookup table of devices and the groups layered over them.
pub struct Registry {
    devices: DashMap<String, Arc<Device>>,
    groups: DashMap<String, Arc<DeviceGroup>>,
}

impl Registry {
    /// Build a registry of devices only (no groups), all sharing `connector`.
    pub fn from_configs(configs: Vec<DeviceConfig>, connector: Arc<dyn Connector>) -> Self {
        Self::build(configs, Vec::new(), connector)
    }

    /// Build a registry from resolved device and group configs, all sharing
    /// `connector`. The `group_configs` are assumed valid — every member id
    /// naming a device present in `device_configs` — which the config layer's
    /// [`resolve_config`] guarantees; any member that somehow does not resolve
    /// is skipped rather than panicking.
    ///
    /// [`resolve_config`]: super::config::resolve_config
    pub fn build(
        device_configs: Vec<DeviceConfig>,
        group_configs: Vec<GroupConfig>,
        connector: Arc<dyn Connector>,
    ) -> Self {
        let devices = DashMap::new();
        for config in device_configs {
            let id = config.id.clone();
            let device = Arc::new(Device::new(config, Arc::clone(&connector)));
            devices.insert(id, device);
        }

        let groups = DashMap::new();
        for group in group_configs {
            let members = group
                .device_ids
                .iter()
                .filter_map(|id| devices.get(id).map(|d| Arc::clone(d.value())))
                .collect();
            groups.insert(
                group.id.clone(),
                Arc::new(DeviceGroup::new(group.id, members)),
            );
        }

        Self { devices, groups }
    }

    /// The device with this id, or `None` if no device has it. This looks up
    /// devices only; use [`target`](Self::target) to resolve a group id too.
    pub fn device(&self, id: &str) -> Option<Arc<Device>> {
        self.devices.get(id).map(|d| Arc::clone(d.value()))
    }

    /// The group with this id, or `None` if no group has it.
    pub fn group(&self, id: &str) -> Option<Arc<DeviceGroup>> {
        self.groups.get(id).map(|g| Arc::clone(g.value()))
    }

    /// Resolve `id` to a device or a group, whichever owns it, or `None`. Since
    /// the two share one id namespace, at most one kind can match.
    pub fn target(&self, id: &str) -> Option<Target> {
        if let Some(device) = self.device(id) {
            Some(Target::Device(device))
        } else {
            self.group(id).map(Target::Group)
        }
    }

    /// The ids of every known device, in no particular order.
    pub fn ids(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.key().clone()).collect()
    }

    /// The ids of every known group, in no particular order.
    pub fn group_ids(&self) -> Vec<String> {
        self.groups.iter().map(|g| g.key().clone()).collect()
    }

    /// A handle to every device, in no particular order. Used to drive
    /// cross-device work such as the eager-connect [`SisKeepalive`] supervisor.
    ///
    /// [`SisKeepalive`]: super::sis_keepalive::SisKeepalive
    pub fn devices(&self) -> Vec<Arc<Device>> {
        self.devices.iter().map(|d| Arc::clone(d.value())).collect()
    }

    /// How many devices are registered.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether the registry holds no devices.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::transport::fake::FakeTransport;
    use crate::protocol::Value;
    use crate::protocol::instructions::query::Query;

    const PORT_REPLY: &str = "22023\r\n";

    /// The two-device pool used across these tests, built as domain values so the
    /// registry tests stay coupled to the registry alone, not to any file format.
    fn example_configs() -> Vec<DeviceConfig> {
        [("atrium-101", "10.0.0.7"), ("annex-far", "10.9.40.12")]
            .into_iter()
            .map(|(id, host)| DeviceConfig {
                id: id.into(),
                host: host.into(),
                port: 22023,
                username: "admin".into(),
                password: "extron".into(),
                connect_timeout: Duration::from_secs(5),
                command_timeout: Duration::from_secs(3),
                eager: false,
                sis_keepalive: None,
                eager_retry: None,
            })
            .collect()
    }

    fn registry_over(reply_count: usize) -> Registry {
        let connector = Arc::new(CountingConnector::new(move || {
            FakeTransport::with_reads(std::iter::repeat_n(PORT_REPLY, reply_count))
        }));
        Registry::from_configs(example_configs(), connector)
    }

    #[test]
    fn builds_an_entry_per_device() {
        let registry = registry_over(1);
        assert_eq!(registry.len(), 2);
        let mut ids = registry.ids();
        ids.sort();
        assert_eq!(ids, vec!["annex-far", "atrium-101"]);
    }

    #[test]
    fn lookup_hits_and_misses() {
        let registry = registry_over(1);
        assert!(registry.device("atrium-101").is_some());
        assert!(registry.device("nope").is_none());
    }

    #[tokio::test]
    async fn a_looked_up_device_runs_commands() {
        let registry = registry_over(1);
        let device = registry.device("atrium-101").unwrap();
        assert_eq!(
            device.run(&Query::SshPort.instruction()).await.unwrap(),
            Value::Port(22023)
        );
    }

    #[tokio::test]
    async fn repeated_lookups_share_one_warm_connection() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY, PORT_REPLY])
        }));
        let opens = connector.opens_handle();
        let registry = Registry::from_configs(example_configs(), connector);

        // Two independent lookups of the same id...
        registry
            .device("atrium-101")
            .unwrap()
            .run(&Query::SshPort.instruction())
            .await
            .unwrap();
        registry
            .device("atrium-101")
            .unwrap()
            .run(&Query::SshPort.instruction())
            .await
            .unwrap();

        // ...reuse the same device, and therefore the same connection.
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    fn group_config() -> Vec<GroupConfig> {
        vec![GroupConfig {
            id: "everywhere".into(),
            device_ids: vec!["atrium-101".into(), "annex-far".into()],
        }]
    }

    #[test]
    fn a_group_resolves_alongside_its_devices() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let registry = Registry::build(example_configs(), group_config(), connector);

        assert_eq!(registry.group_ids(), vec!["everywhere"]);
        let group = registry.group("everywhere").unwrap();
        let mut members = group.member_ids();
        members.sort();
        assert_eq!(members, vec!["annex-far", "atrium-101"]);
    }

    #[tokio::test]
    async fn target_resolves_a_device_or_a_group_from_one_id() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let registry = Registry::build(example_configs(), group_config(), connector);

        assert!(matches!(
            registry.target("atrium-101"),
            Some(Target::Device(_))
        ));
        assert!(matches!(
            registry.target("everywhere"),
            Some(Target::Group(_))
        ));
        assert!(registry.target("nope").is_none());
    }

    #[tokio::test]
    async fn a_group_command_reaches_every_member() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let registry = Registry::build(example_configs(), group_config(), connector);

        let group = registry.group("everywhere").unwrap();
        let results = group.run(&Query::SshPort.instruction()).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, v)| *v == Value::Port(22023)));
    }
}
