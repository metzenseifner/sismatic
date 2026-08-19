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
pub mod commands;
pub mod devices;
pub mod error;
pub mod health_check;
pub mod readings;

pub use devices::{list_devices, list_groups, read_device, read_group};

pub use commands::{
    IdempotencyKey, ValueWrite, list_commands, pause_recording, read_command, read_phase,
    set_metadata, set_setting, start_recording, stop_recording,
};
pub use error::ApiFailure;
pub use health_check::health_check;
pub use readings::{field_history, list_fields, read_field};
