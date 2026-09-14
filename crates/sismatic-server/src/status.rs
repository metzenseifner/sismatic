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

use sismatic_api_types::{AutoDisabledField, ConnectionStatus, DeviceId};
use sismatic_core::devices::auto_disabled::AutoDisabledField as AutoDisabled;
use sismatic_core::devices::device::{Connectivity, Device};
use sismatic_core::devices::registry::Registry;
use sismatic_store::status::{DeviceStatus, Observation};

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
    async fn observe(&self, id: &str) -> Observation {
        self.registry
            .device(id)
            .map_or_else(Observation::default, |device| observe(&device))
    }

    async fn all(&self) -> BTreeMap<DeviceId, Observation> {
        self.registry
            .devices()
            .into_iter()
            .map(|device| (device.id().to_owned(), observe(&device)))
            .collect()
    }
}

/// Read one device's live state: its connectivity and what it has been observed
/// to refuse.
///
/// Both come off the same handle, which is the whole reason they are one port.
/// Neither dials and neither waits — `connectivity` uses `try_lock` and the
/// learned set is a map behind a sync mutex — so this stays true of a fleet
/// mid-poll, which is exactly when an operator looks at it.
fn observe(device: &Device) -> Observation {
    Observation {
        connection: to_dto(device.connectivity()),
        auto_disabled: device
            .auto_disabled()
            .snapshot()
            .into_iter()
            .map(to_field_dto)
            .collect(),
    }
}

/// Map core's learned-veto entry onto the wire shape.
///
/// The one lossy step is deliberate: a `Duration` becomes whole seconds,
/// because the wire spells every other delay that way (`interval_secs`,
/// `retry_in_secs`) and a self-heal window is configured in seconds to begin
/// with. A sub-second remainder would be reporting precision the setting that
/// produced it never had.
fn to_field_dto(field: AutoDisabled) -> AutoDisabledField {
    AutoDisabledField {
        name: field.name,
        refusals: field.refusals,
        disabled: field.disabled,
        retry_in_secs: field.retry_in.map(|wait| wait.as_secs()),
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
    use std::collections::BTreeSet;
    use std::time::Duration;

    use sismatic_core::devices::config::{DeviceConfig, Uuid};
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
            uuid: Uuid::nil(),
            disabled_fields: BTreeSet::new(),
            auto_disable_after: 0,
            self_heal: None,
        }
        .derive_uuid()
    }

    fn registry_status(ids: &[&str]) -> RegistryStatus {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["22023\r\n"])
        }));
        let registry =
            Registry::from_configs(ids.iter().map(|id| device_config(id)).collect(), connector);
        RegistryStatus::new(Arc::new(registry))
    }

    /// Every connection state, with nothing inferred against any device.
    fn connections(
        observed: &BTreeMap<DeviceId, Observation>,
    ) -> BTreeMap<DeviceId, ConnectionStatus> {
        observed
            .iter()
            .map(|(id, observation)| (id.clone(), observation.connection))
            .collect()
    }

    #[tokio::test]
    async fn an_untouched_fleet_reads_as_cold() {
        let status = registry_status(&["atrium", "annex"]);
        assert_eq!(
            connections(&status.all().await),
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

        assert_eq!(
            status.observe("atrium").await.connection,
            ConnectionStatus::Warm
        );
    }

    /// An id the registry does not hold is `Unknown`, not an error. The caller
    /// got the id from the catalog, so a disagreement between the two is this
    /// process's problem rather than the caller's.
    #[tokio::test]
    async fn an_unknown_id_is_unknown_rather_than_an_error() {
        let status = registry_status(&["atrium"]);
        let observed = status.observe("nobody").await;
        assert_eq!(observed.connection, ConnectionStatus::Unknown);
        assert!(
            observed.auto_disabled.is_empty(),
            "and nothing may be claimed about a device that is not there"
        );
    }

    /// The inferred veto crosses this seam alongside connectivity, which is the
    /// reason the two are one port: both come off the same handle, and the fleet
    /// index reads them in a single walk.
    #[tokio::test]
    async fn the_inferred_veto_is_reported_with_the_connection_state() {
        let status = registry_status(&["atrium"]);
        let device = status.registry.device("atrium").expect("the device");
        // Two refusals at a threshold of two: vetoed, with no self-heal.
        device.auto_disabled().refused("STREAM_2_NAME", 2, None);
        device.auto_disabled().refused("STREAM_2_NAME", 2, None);
        // One below the threshold: watched, not yet vetoed.
        device.auto_disabled().refused("STREAM_3_NAME", 2, None);

        let observed = status.observe("atrium").await;

        assert_eq!(observed.auto_disabled.len(), 2, "{observed:?}");
        let vetoed = &observed.auto_disabled[0];
        assert_eq!(vetoed.name, "STREAM_2_NAME");
        assert!(vetoed.disabled);
        assert_eq!(vetoed.refusals, 2);
        assert_eq!(
            vetoed.retry_in_secs, None,
            "self-heal is off, so there is no next attempt to report"
        );

        let watched = &observed.auto_disabled[1];
        assert_eq!(watched.name, "STREAM_3_NAME");
        assert!(
            !watched.disabled,
            "a near-miss is reported, so a field never goes dark out of nowhere"
        );
        assert_eq!(watched.refusals, 1);
    }

    /// A self-healing veto reports when it will next be tried, which is the one
    /// thing an operator wants from it that the flag alone cannot say.
    #[tokio::test]
    async fn a_self_healing_veto_reports_its_retry_window() {
        let status = registry_status(&["atrium"]);
        let device = status.registry.device("atrium").expect("the device");
        let heal = Some(Duration::from_secs(600));
        device.auto_disabled().refused("STREAM_2_NAME", 1, heal);

        let observed = status.observe("atrium").await;
        let field = &observed.auto_disabled[0];

        assert!(field.disabled);
        // Whole seconds, and the window has only just been armed — so this is
        // 600 or a hair under, never above.
        let retry = field.retry_in_secs.expect("a self-healing veto retries");
        assert!((599..=600).contains(&retry), "{retry}");
    }
}
