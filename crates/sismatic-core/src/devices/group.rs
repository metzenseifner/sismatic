//! A set of devices driven as one.
//!
//! A [`DeviceGroup`] bundles several [`Device`]s behind a single id so a caller
//! can address a whole room as if it were one unit: sending an instruction to
//! the group sends it to *every* member. The motivating case is more than one
//! recorder in the same room that must start together — issuing `start` to the
//! group dispatches `start` to all of them.
//!
//! Fan-out is concurrent: each member's exchange is spawned before any is
//! awaited, so the members act in unison rather than one-after-another. Each
//! member still owns its own warm connection and its own command lock, exactly
//! as when addressed directly (the group holds the same `Arc<Device>` the
//! registry hands out), so nothing about grouping changes a device's own
//! self-healing or serialisation.
//!
//! A group run reports *every* member's outcome. When all members succeed it
//! returns their values tagged by device id, in group order; when any member
//! fails it returns a [`GroupError`] listing exactly which members failed and
//! why — the successful members have already run, so a partial failure is
//! surfaced, not hidden.

use std::fmt;
use std::sync::Arc;

use crate::protocol::Value;
use crate::protocol::instructions::Instruction;

use super::device::{Device, DeviceError};

/// One member's decoded reply, tagged with the device id it came from.
pub type MemberValue = (String, Value);

/// Several devices addressed as a single unit. Cloning-free to share: the
/// registry holds one `Arc<DeviceGroup>` and hands out clones of the `Arc`.
pub struct DeviceGroup {
    id: String,
    devices: Vec<Arc<Device>>,
}

/// A group run in which at least one member failed. The successful members (if
/// any) have already executed; this reports only the ones that did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupError {
    /// The group's id.
    pub id: String,
    /// Each failed member, as `(device id, why it failed)`, in group order.
    pub failures: Vec<(String, DeviceError)>,
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "group `{}`: ", self.id)?;
        for (i, (device, error)) in self.failures.iter().enumerate() {
            if i > 0 {
                write!(f, "; ")?;
            }
            write!(f, "`{device}`: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GroupError {}

impl DeviceGroup {
    /// Build a group of `devices` addressable by `id`.
    pub fn new(id: impl Into<String>, devices: Vec<Arc<Device>>) -> Self {
        Self {
            id: id.into(),
            devices,
        }
    }

    /// This group's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The ids of the member devices, in group order.
    pub fn member_ids(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.id().to_string()).collect()
    }

    /// How many devices are in the group.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether the group has no members.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Send `instruction` to every member at once and collect every reply.
    ///
    /// Each member's exchange is spawned before any is awaited, so the commands
    /// go out concurrently rather than in series. Returns each member's value
    /// tagged by device id (in group order) when all succeed, or a
    /// [`GroupError`] naming every member that failed.
    pub async fn run(&self, instruction: &Instruction) -> Result<Vec<MemberValue>, GroupError> {
        // Dispatch to all members first — spawning every exchange up front is
        // what makes the group act in unison instead of sequentially.
        let mut handles = Vec::with_capacity(self.devices.len());
        for device in &self.devices {
            let device = Arc::clone(device);
            let instruction = instruction.clone();
            handles.push(tokio::spawn(async move { device.run(&instruction).await }));
        }

        let mut values = Vec::with_capacity(self.devices.len());
        let mut failures = Vec::new();
        for (device, handle) in self.devices.iter().zip(handles) {
            let id = device.id().to_string();
            match handle.await.expect("member task should not panic") {
                Ok(value) => values.push((id, value)),
                Err(error) => failures.push((id, error)),
            }
        }

        if failures.is_empty() {
            Ok(values)
        } else {
            Err(GroupError {
                id: self.id.clone(),
                failures,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::devices::config::DeviceConfig;
    use crate::devices::connector::Connector;
    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::controller::ControllerError;
    use crate::devices::transport::fake::FakeTransport;
    use crate::protocol::instructions::commands::Command;
    use crate::protocol::instructions::query::Query;

    const PORT_REPLY: &str = "22023\r\n";
    const ACK_REPLY: &str = "RcdrY1\r\n";

    fn config(id: &str) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            host: "10.0.0.1".into(),
            port: 22023,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(500),
            eager: false,
            sis_keepalive: None,
            eager_retry: None,
        }
    }

    fn device(id: &str, connector: Arc<dyn Connector>) -> Arc<Device> {
        Arc::new(Device::new(config(id), connector))
    }

    fn connector(reply: &'static str) -> Arc<CountingConnector> {
        Arc::new(CountingConnector::new(move || {
            FakeTransport::with_reads([reply])
        }))
    }

    #[tokio::test]
    async fn command_reaches_every_member() {
        let group = DeviceGroup::new(
            "room-5",
            vec![
                device("front", connector(ACK_REPLY)),
                device("back", connector(ACK_REPLY)),
            ],
        );

        let results = group.run(&Command::Start.instruction()).await.unwrap();
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["front", "back"]);
        assert!(results.iter().all(|(_, v)| matches!(v, Value::Ack(_))));
    }

    #[tokio::test]
    async fn results_are_tagged_and_ordered_by_membership() {
        let group = DeviceGroup::new(
            "room-5",
            vec![
                device("front", connector(PORT_REPLY)),
                device("back", connector(PORT_REPLY)),
            ],
        );

        let results = group.run(&Query::SshPort.instruction()).await.unwrap();
        assert_eq!(
            results,
            vec![
                ("front".to_string(), Value::Port(22023)),
                ("back".to_string(), Value::Port(22023)),
            ]
        );
    }

    #[tokio::test]
    async fn a_failing_member_is_reported_without_hiding_the_group() {
        // `front` answers; `back` closes immediately, so its exchange fails.
        let group = DeviceGroup::new(
            "room-5",
            vec![
                device("front", connector(PORT_REPLY)),
                device("back", Arc::new(CountingConnector::new(FakeTransport::new))),
            ],
        );

        let err = group.run(&Query::SshPort.instruction()).await.unwrap_err();
        assert_eq!(err.id, "room-5");
        assert_eq!(err.failures.len(), 1);
        assert_eq!(err.failures[0].0, "back");
        assert!(matches!(
            err.failures[0].1,
            DeviceError::Command(ControllerError::ConnectionClosed { .. })
        ));
    }

    #[tokio::test]
    async fn empty_group_run_succeeds_with_no_results() {
        let group = DeviceGroup::new("empty", Vec::new());
        assert!(group.is_empty());
        assert_eq!(
            group.run(&Query::SshPort.instruction()).await.unwrap(),
            Vec::new()
        );
    }
}
