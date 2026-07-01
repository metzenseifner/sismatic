//! The set of known devices, keyed by id.
//!
//! A [`Registry`] is built once from a `devices.toml` and a shared
//! [`Connector`], and hands out `Arc<Device>` by id. Because every lookup of the
//! same id returns the same [`Device`], callers transparently share that
//! device's one warm connection — the registry is the keep-warm cache, one
//! entry per device.

use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;

use super::config::{self, ConfigError, DeviceConfig};
use super::connector::Connector;
use super::device::Device;

/// A lookup table of devices, each owning its own cached connection.
pub struct Registry {
    devices: DashMap<String, Arc<Device>>,
}

impl Registry {
    /// Build a registry from already-resolved configs, all sharing `connector`.
    pub fn from_configs(configs: Vec<DeviceConfig>, connector: Arc<dyn Connector>) -> Self {
        let devices = DashMap::new();
        for config in configs {
            let id = config.id.clone();
            let device = Arc::new(Device::new(config, Arc::clone(&connector)));
            devices.insert(id, device);
        }
        Self { devices }
    }

    /// Read and resolve `path`, then build a registry over `connector`.
    pub fn load(
        path: impl AsRef<Path>,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, ConfigError> {
        Ok(Self::from_configs(config::load(path)?, connector))
    }

    /// The device with this id, or `None` if it is not in the registry.
    pub fn device(&self, id: &str) -> Option<Arc<Device>> {
        self.devices.get(id).map(|d| Arc::clone(d.value()))
    }

    /// The ids of every known device, in no particular order.
    pub fn ids(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.key().clone()).collect()
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

    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::transport::fake::FakeTransport;
    use crate::protocol::Value;
    use crate::protocol::instructions::query::Query;

    const PORT_REPLY: &str = "BPMAP\r\n22023\r\r";

    const EXAMPLE: &str = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3

[[device]]
id = "atrium-101"
host = "10.0.0.7"

[[device]]
id = "annex-far"
host = "10.9.40.12"
"#;

    fn registry_over(reply_count: usize) -> Registry {
        let connector = Arc::new(CountingConnector::new(move || {
            FakeTransport::with_reads(std::iter::repeat_n(PORT_REPLY, reply_count))
        }));
        Registry::from_configs(config::parse(EXAMPLE).unwrap(), connector)
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
        let registry = Registry::from_configs(config::parse(EXAMPLE).unwrap(), connector);

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

    #[test]
    fn load_builds_from_a_file() {
        let path =
            std::env::temp_dir().join(format!("opensis-registry-{}.toml", std::process::id()));
        std::fs::write(&path, EXAMPLE).unwrap();
        let connector = Arc::new(CountingConnector::new(FakeTransport::new));
        let registry = Registry::load(&path, connector).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(registry.len(), 2);
    }
}
