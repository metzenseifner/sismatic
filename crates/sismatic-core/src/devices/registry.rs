//! The set of known devices and groups, keyed by id.
//!
//! A [`Registry`] is built once from a `devices.toml` and a shared
//! [`Connector`], and hands out `Arc<Device>` (or `Arc<DeviceGroup>`) by id.
//! Because every lookup of the same id returns the same [`Device`], callers
//! transparently share that device's one warm connection — the registry is the
//! keep-warm cache, one entry per device.
//!
//! A [`DeviceGroup`] is a name over several of those same device handles, so a
//! caller can address a whole device group by one id; its members reuse the very
//! warm
//! connections the registry already holds. Device and group ids share one
//! namespace (the config layer guarantees they never collide), so [`target`]
//! resolves either kind from a single id.
//!
//! # A fleet that changes while it runs
//!
//! [`apply`] replaces the device and group set in place, and the whole of its
//! design is one comparison: a [`DeviceConfig`]'s
//! [`uuid`](DeviceConfig::uuid) is derived from its every field, so two configs
//! with the same UUID *are* the same device and there is nothing to do. That is
//! what makes a reload cheap — a file re-read that changed one recorder leaves
//! the other thirty-nine holding the SSH sessions they already had, rather than
//! every device in the fleet redialing to apply a change to one of them.
//!
//! A device is immutable, so a device whose configuration moved is not mutated
//! but *replaced*: a new [`Device`], a new connection, a new UUID. What crosses
//! from the old one to the new is the learned veto set (see [`AutoDisabled`]),
//! because that is evidence about the recorder at that address and not about the
//! configuration used to reach it — a device replaced for a changed
//! `connect_secs` is the same unit with the same missing license.
//!
//! Removal is cooperative, as cancellation is everywhere else here. Dropping a
//! device from the map does not reach into the callers already holding an
//! `Arc<Device>` — a poll loop mid-exchange finishes it against the device it
//! has. What removal guarantees is that no *new* lookup finds it.
//!
//! [`target`]: Registry::target
//! [`apply`]: Registry::apply
//! [`AutoDisabled`]: super::auto_disabled::AutoDisabled

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;

use super::config::{DeviceConfig, GroupConfig, Resolved};
use super::connector::Connector;
use super::device::Device;
use super::group::DeviceGroup;

/// What an id resolves to: a lone device or a group of them. Both answer the
/// same instructions, so a facade can run against either after one lookup.
pub enum Target {
    Device(Arc<Device>),
    Group(Arc<DeviceGroup>),
}

/// What one pass of [`Registry::apply`] did.
///
/// Ids rather than counts, because every consumer of this needs the names: the
/// sync supervisor restarts loops for them, the relay stops a task per removed
/// device, and the composition root cancels their queued writes. A count would
/// make each of those re-derive the diff this already computed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryChange {
    /// Ids that did not exist before, sorted.
    pub added: Vec<String>,
    /// Ids that existed with a different configuration, sorted. Each is a new
    /// [`Device`] with a new connection; the old one is dropped.
    pub replaced: Vec<String>,
    /// Ids that no longer appear in the applied config, sorted.
    pub removed: Vec<String>,
    /// How many devices were left exactly as they were — the number that
    /// *kept their warm connection*, which is the figure worth logging after a
    /// reload.
    pub unchanged: usize,
}

impl RegistryChange {
    /// Whether the applied config asked for nothing the registry was not
    /// already holding.
    #[must_use]
    pub fn is_nothing(&self) -> bool {
        self.added.is_empty() && self.replaced.is_empty() && self.removed.is_empty()
    }

    /// Every id whose `Arc<Device>` is no longer the one the registry hands
    /// out — the devices whose running tasks have to be stopped, and for the
    /// replaced ones restarted.
    #[must_use]
    pub fn invalidated(&self) -> Vec<String> {
        let mut ids = self.replaced.clone();
        ids.extend(self.removed.iter().cloned());
        ids.sort();
        ids
    }
}

/// A lookup table of devices and the groups layered over them.
pub struct Registry {
    devices: DashMap<String, Arc<Device>>,
    groups: DashMap<String, Arc<DeviceGroup>>,
    /// Kept so [`apply`](Registry::apply) can build a device without being
    /// handed one. Every device in a registry shares it, which is what makes
    /// the connector a property of the fleet rather than of a device.
    connector: Arc<dyn Connector>,
    /// Serializes reconciliation against itself.
    ///
    /// The `DashMap`s make each individual insert and remove safe; this makes
    /// the *diff* safe, which is a different claim. Two concurrent `apply`s
    /// reading the same "before" state would each decide against a fleet that
    /// no longer exists by the time they write, and the loser's decisions would
    /// be silently wrong rather than merely late.
    ///
    /// A `std::sync::Mutex` because nothing under it awaits: building a
    /// `Device` is a struct literal, and no I/O happens until something runs an
    /// instruction on it.
    reconcile: Mutex<()>,
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

        let registry = Self {
            devices,
            groups: DashMap::new(),
            connector,
            reconcile: Mutex::new(()),
        };
        registry.rebuild_groups(group_configs);
        registry
    }

    /// Make the registry match `resolved`, and report what moved.
    ///
    /// The diff is by [`DeviceConfig::uuid`], which is derived from every field
    /// of a device's configuration — so this compares two `u128`s per id rather
    /// than walking eleven fields, and it cannot forget one. A device whose UUID
    /// is unchanged is left strictly alone: the same `Arc<Device>`, the same
    /// connection, not even a lock taken.
    ///
    /// A device whose UUID moved is replaced rather than mutated, because a
    /// `DeviceConfig` is immutable. Its learned veto set is carried into the
    /// replacement (see [`Device::with_auto_disabled`]), so a config edit does
    /// not make the fleet re-discover, at `auto_disable_after` refused
    /// exchanges per field, everything it already knew about that recorder.
    ///
    /// Groups are rebuilt wholesale rather than diffed. A group holds
    /// `Arc<Device>` handles, so any replacement invalidates every group that
    /// contains it, and a group is an id and a vector of `Arc` clones — cheaper
    /// to rebuild than to work out which ones needed it.
    ///
    /// The `resolved` argument is a whole [`Resolved`] rather than the two
    /// vectors, because its invariants are what make this safe to apply: ids are
    /// unique across devices *and* groups, and every group member names a
    /// device present in the same value. Taking the pieces separately would let
    /// a caller assemble a pair that `resolve_config` would have refused.
    pub fn apply(&self, resolved: Resolved) -> RegistryChange {
        let _guard = self
            .reconcile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let Resolved { devices, groups } = resolved;
        let mut change = RegistryChange::default();
        let desired: BTreeMap<String, DeviceConfig> = devices
            .into_iter()
            .map(|config| (config.id.clone(), config))
            .collect();

        // Taken before the map is consumed below; this is what says which of
        // the *currently* registered devices the applied config no longer
        // mentions.
        let desired_ids: std::collections::BTreeSet<String> = desired.keys().cloned().collect();

        for (id, config) in desired {
            match self.devices.get(&id).map(|d| Arc::clone(d.value())) {
                // Same configuration, so same device. Nothing is touched — this
                // is the case a reload is almost entirely made of, and the
                // reason it does not cost the fleet its connections.
                Some(existing) if existing.config().uuid == config.uuid => {
                    change.unchanged += 1;
                }
                Some(existing) => {
                    let replacement = Device::with_auto_disabled(
                        config,
                        Arc::clone(&self.connector),
                        Arc::clone(existing.auto_disabled()),
                    );
                    self.devices.insert(id.clone(), Arc::new(replacement));
                    change.replaced.push(id);
                }
                None => {
                    let device = Device::new(config, Arc::clone(&self.connector));
                    self.devices.insert(id.clone(), Arc::new(device));
                    change.added.push(id);
                }
            }
        }

        // Collected before removing, rather than removing while iterating: a
        // `DashMap` iterator holds shard locks, and `remove` inside one is the
        // shape that deadlocks.
        let stale: Vec<String> = self
            .devices
            .iter()
            .filter(|entry| !desired_ids.contains(entry.key()))
            .map(|entry| entry.key().clone())
            .collect();
        for id in stale {
            self.devices.remove(&id);
            change.removed.push(id);
        }

        change.added.sort();
        change.replaced.sort();
        change.removed.sort();

        self.rebuild_groups(groups);
        change
    }

    /// Replace every group with one built over the devices now registered.
    ///
    /// A member that does not resolve is skipped rather than panicking, the
    /// same contract [`build`](Self::build) has and for the same reason: a
    /// [`Resolved`] guarantees every member names a device, so a miss is
    /// impossible and defending against it is cheaper than proving it.
    fn rebuild_groups(&self, group_configs: Vec<GroupConfig>) {
        self.groups.clear();
        for group in group_configs {
            let members = group
                .device_ids
                .iter()
                .filter_map(|id| self.devices.get(id).map(|d| Arc::clone(d.value())))
                .collect();
            self.groups.insert(
                group.id.clone(),
                Arc::new(DeviceGroup::new(group.id, members)),
            );
        }
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
    use std::collections::BTreeSet;

    use crate::devices::config::Uuid;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::devices::config::Barrier;
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
                exchange_timeout: Duration::from_secs(3),
                eager: false,
                sis_keepalive: None,
                eager_retry: None,
                cold_backoff: None,
                uuid: Uuid::nil(),
                disabled_fields: BTreeSet::new(),
                auto_disable_after: 0,
                self_heal: None,
            })
            .map(DeviceConfig::derive_uuid)
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
            // The registry does not read either — the barrier lives in the
            // outbox — so these are whatever `resolve_config` would default to.
            barrier_timeout: Duration::from_secs(8),
            barrier: Barrier::FailBatch,
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

    // ---- applying a new fleet ---------------------------------------------

    fn resolved(devices: Vec<DeviceConfig>, groups: Vec<GroupConfig>) -> Resolved {
        Resolved { devices, groups }
    }

    /// `example_configs`, with `id`'s `connect_timeout` moved — the smallest
    /// edit that mints a different device.
    fn with_retimed(id: &str) -> Vec<DeviceConfig> {
        example_configs()
            .into_iter()
            .map(|config| {
                if config.id == id {
                    DeviceConfig {
                        connect_timeout: Duration::from_secs(30),
                        ..config
                    }
                    .derive_uuid()
                } else {
                    config
                }
            })
            .collect()
    }

    /// The property a reload rests on: re-applying the same configuration
    /// touches nothing, so the fleet keeps every SSH session it holds.
    #[test]
    fn re_applying_the_same_config_changes_nothing() {
        let registry = registry_over(1);
        let before = registry.device("atrium-101").expect("configured");

        let change = registry.apply(resolved(example_configs(), vec![]));

        assert!(change.is_nothing(), "{change:?}");
        assert_eq!(change.unchanged, 2);
        assert!(
            Arc::ptr_eq(&before, &registry.device("atrium-101").unwrap()),
            "an unchanged device must be the very same handle, warm connection and all"
        );
    }

    #[test]
    fn a_changed_device_is_replaced_and_the_others_are_left_alone() {
        let registry = registry_over(1);
        let untouched = registry.device("annex-far").expect("configured");
        let before = registry.device("atrium-101").expect("configured");

        let change = registry.apply(resolved(with_retimed("atrium-101"), vec![]));

        assert_eq!(change.replaced, vec!["atrium-101"]);
        assert_eq!(change.unchanged, 1, "the edit was to one device only");
        assert!(change.added.is_empty() && change.removed.is_empty());

        let after = registry.device("atrium-101").unwrap();
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a changed device is replaced"
        );
        assert_eq!(after.config().connect_timeout, Duration::from_secs(30));
        assert!(
            Arc::ptr_eq(&untouched, &registry.device("annex-far").unwrap()),
            "one device's edit must not cost another its connection"
        );
    }

    /// The reason a replacement is not simply a fresh device: what was learned
    /// is evidence about the recorder at that address, and re-learning it costs
    /// `auto_disable_after` refused exchanges per field.
    #[test]
    fn a_replaced_device_keeps_what_was_learned_about_it() {
        let registry = registry_over(1);
        let before = registry.device("atrium-101").expect("configured");
        // Two refusals at a threshold of two: the field is now vetoed.
        before.auto_disabled().refused("STREAM_2_NAME", 2, None);
        before.auto_disabled().refused("STREAM_2_NAME", 2, None);
        assert!(before.auto_disabled().veto("STREAM_2_NAME").is_some());

        registry.apply(resolved(with_retimed("atrium-101"), vec![]));

        let after = registry.device("atrium-101").unwrap();
        assert!(
            after.auto_disabled().veto("STREAM_2_NAME").is_some(),
            "the replacement must inherit the learned veto"
        );
        assert!(
            Arc::ptr_eq(before.auto_disabled(), after.auto_disabled()),
            "and share the very set, so what it learns next is not forked"
        );
    }

    #[test]
    fn devices_are_added_and_removed() {
        let registry = registry_over(1);

        let mut fleet = example_configs();
        fleet.retain(|config| config.id != "annex-far");
        fleet.push(
            DeviceConfig {
                id: "new-wing".into(),
                ..example_configs().remove(0)
            }
            .derive_uuid(),
        );

        let change = registry.apply(resolved(fleet, vec![]));

        assert_eq!(change.added, vec!["new-wing"]);
        assert_eq!(change.removed, vec!["annex-far"]);
        assert_eq!(change.unchanged, 1);
        assert!(
            registry.device("annex-far").is_none(),
            "removed from lookup"
        );
        assert!(registry.device("new-wing").is_some());
        assert_eq!(registry.len(), 2);
    }

    /// A removed device takes its learned set with it: the id is gone, and what
    /// was recorded was about that id.
    #[test]
    fn a_removed_and_re_added_device_starts_over() {
        let registry = registry_over(1);
        let before = registry.device("annex-far").expect("configured");
        before.auto_disabled().refused("STREAM_2_NAME", 1, None);
        assert!(before.auto_disabled().veto("STREAM_2_NAME").is_some());
        drop(before);

        let mut fleet = example_configs();
        fleet.retain(|config| config.id != "annex-far");
        registry.apply(resolved(fleet, vec![]));
        registry.apply(resolved(example_configs(), vec![]));

        let readded = registry.device("annex-far").expect("back again");
        assert_eq!(
            readded.auto_disabled().veto("STREAM_2_NAME"),
            None,
            "a device removed from the fleet leaves nothing behind"
        );
    }

    /// A group holds `Arc<Device>` handles, so a replacement it contains would
    /// leave it addressing a device the registry no longer hands out.
    #[test]
    fn groups_are_rebuilt_over_the_devices_that_replaced_their_members() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([PORT_REPLY])
        }));
        let registry = Registry::build(example_configs(), group_config(), connector);

        registry.apply(resolved(with_retimed("atrium-101"), group_config()));

        let group = registry.group("everywhere").expect("still configured");
        let current = registry.device("atrium-101").unwrap();
        assert!(
            group
                .members()
                .iter()
                .any(|member| Arc::ptr_eq(member, &current)),
            "the group must address the device the registry now hands out"
        );
    }

    #[test]
    fn applying_a_config_with_no_devices_empties_the_registry() {
        let registry = registry_over(1);
        let change = registry.apply(resolved(vec![], vec![]));

        assert_eq!(change.removed, vec!["annex-far", "atrium-101"]);
        assert_eq!(change.unchanged, 0);
        assert!(registry.is_empty());
        assert!(registry.group_ids().is_empty());
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
