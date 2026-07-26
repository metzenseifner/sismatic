use std::sync::Arc;

use sismatic_api_types::{DeviceId, Reading, TimeSpan};

pub mod error;

pub use error::{ReadError, WriteError};

/// A convenient object-safe handle.
pub type DynReadStore = Arc<dyn ReadStore>;
pub type DynWriteStore = Arc<dyn WriteStore>;

#[async_trait::async_trait]
pub trait ReadStore: Send + Sync {
    async fn latest(&self, dev: DeviceId) -> Result<Option<Reading>, ReadError>;
    async fn between(&self, dev: DeviceId, span: TimeSpan) -> Result<Vec<Reading>, ReadError>;
}

#[async_trait::async_trait]
pub trait WriteStore: Send + Sync {
    async fn upsert_latest(&self, reading: Reading) -> Result<(), WriteError>;
}
