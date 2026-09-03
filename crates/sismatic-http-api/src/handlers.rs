//! The handlers, one module per route.
//!
//! Handlers are plain `async fn`s that take extractors and return a response;
//! nothing here knows how the application is assembled or what it is served on.
//! That is [`crate::startup`]'s job, and the split is what lets a route be read
//! (and reasoned about) without reading the wiring.

// Public like `readings`, and for one narrow reason: the `#[utoipa::path]`
// attribute on a handler expands to a sibling item in the handler's own module,
// and `crate::openapi` has to name it. Re-exporting the function is not enough —
// the generated item is not the function.
pub mod devices;
pub mod error;
pub mod group_readings;
pub mod health_check;
pub mod instructions;
pub mod readings;
pub mod target;
pub mod writings;

pub use devices::{list_devices, list_groups, read_device, read_group};

pub use instructions::{field_catalog, writings_catalog};

pub use group_readings::{group_field_history, list_group_fields, read_group_field};

pub use error::ApiFailure;
pub use health_check::health_check;
pub use readings::{field_history, list_fields, read_field};
pub use writings::{
    IdempotencyKey, ValueWrite, list_group_writings, list_writings, pause_group_recording,
    pause_recording, read_group_phase, read_phase, read_writing, set_group_metadata,
    set_group_setting, set_metadata, set_setting, start_group_recording, start_recording,
    stop_group_recording, stop_recording,
};
