//! [`DeviceStatus`] over the live [`Registry`].
//!
//! The adapter lives in the composition root rather than in
//! `sismatic-store-memory`, where the other two adapters are, because it is the
//! one that cannot: it reads a `sismatic-core` type, and `store-memory` does
//! not depend on core — deliberately, since that is what lets the store's
//! adapters be swapped for a SQL backend without dragging the device model
//! along.
//!
//! Nor is it in `sismatic-sync` or `sismatic-intent-relay`, which do see both
//! sides. Those are drivers, with loops and shutdown semantics of their own,
//! and a projection with neither belongs where the two halves are already being
//! joined: here.

use std::collections::BTreeMap;
use std::sync::Arc;

use sismatic_api_types::{ConnectionStatus, DeviceId};
use sismatic_core::devices::device::Connectivity;
use sismatic_core::devices::registry::Registry;
use sismatic_store::status::DeviceStatus;

/// Reports what the registry's devices are doing, without dialing any of them.
pub struct RegistryStatus {
    registry: Arc<Registry>,
}

impl RegistryStatus {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl DeviceStatus for RegistryStatus {
    async fn status(&self, id: &str) -> ConnectionStatus {
        self.registry
            .device(id)
            .map_or(ConnectionStatus::Unknown, |device| {
                to_dto(device.connectivity())
            })
    }

    async fn all(&self) -> BTreeMap<DeviceId, ConnectionStatus> {
        self.registry
            .devices()
            .into_iter()
            .map(|device| (device.id().to_owned(), to_dto(device.connectivity())))
            .collect()
    }
}

/// Map core's connectivity onto the wire enum.
///
/// Wildcard-free, so a fifth [`Connectivity`] state is a build error here until
/// someone decides what a client should be told about it — the same drift
/// sentinel `sismatic_sync::dto` uses for reads. [`ConnectionStatus`] has one
/// variant this cannot produce, `Unknown`, which is reserved for the id the
/// registry does not hold at all.
const fn to_dto(connectivity: Connectivity) -> ConnectionStatus {
    match connectivity {
        Connectivity::Warm => ConnectionStatus::Warm,
        Connectivity::Busy => ConnectionStatus::Busy,
        Connectivity::Cold => ConnectionStatus::Cold,
        Connectivity::Gated => ConnectionStatus::Gated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use sismatic_core::devices::config::DeviceConfig;
    use sismatic_core::devices::connector::fake::CountingConnector;
    use sismatic_core::devices::transport::fake::FakeTransport;

    fn device_config(id: &str) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            host: "10.0.0.1".into(),
            port: 22023,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(500),
            exchange_timeout: Duration::from_millis(500),
            eager: false,
            sis_keepalive: None,
            eager_retry: None,
            cold_backoff: None,
        }
    }

    fn registry_status(ids: &[&str]) -> RegistryStatus {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["22023\r\n"])
        }));
        let registry =
            Registry::from_configs(ids.iter().map(|id| device_config(id)).collect(), connector);
        RegistryStatus::new(Arc::new(registry))
    }

    #[tokio::test]
    async fn an_untouched_fleet_reads_as_cold() {
        let status = registry_status(&["atrium", "annex"]);
        assert_eq!(
            status.all().await,
            BTreeMap::from([
                ("annex".to_owned(), ConnectionStatus::Cold),
                ("atrium".to_owned(), ConnectionStatus::Cold),
            ])
        );
    }

    /// The whole point: a device that has actually been used reads differently
    /// from one that has not. Before this port, both were `Unknown`.
    #[tokio::test]
    async fn a_device_that_has_been_used_reads_as_warm() {
        let status = registry_status(&["atrium"]);
        let device = status.registry.device("atrium").expect("the device");
        device
            .run(&sismatic_core::protocol::instructions::query::Query::SshPort.instruction())
            .await
            .expect("the write");

        assert_eq!(status.status("atrium").await, ConnectionStatus::Warm);
    }

    /// An id the registry does not hold is `Unknown`, not an error. The caller
    /// got the id from the catalog, so a disagreement between the two is this
    /// process's problem rather than the caller's.
    #[tokio::test]
    async fn an_unknown_id_is_unknown_rather_than_an_error() {
        let status = registry_status(&["atrium"]);
        assert_eq!(status.status("nobody").await, ConnectionStatus::Unknown);
    }
}
