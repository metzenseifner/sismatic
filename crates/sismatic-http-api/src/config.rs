//! The settings port: reading what the server is running under, and changing it.
//!
//! Every other port this crate takes belongs to `sismatic-store` — they are ways
//! of asking about *stored* things, and the store crate is where a second
//! adapter for one of them would land. This one is different in kind, and so is
//! its home. What is behind it is the composition root itself: the object that
//! holds the resolved config, the channels the poll loops and the relay read
//! their schedules from, and the file the whole document was loaded from. None
//! of that is storage, and no crate but the root can supply it — so the port is
//! declared by its *consumer*, the same arrangement [`Stamp`](crate::Stamp)
//! already uses for the id-and-clock the write routes need.
//!
//! # One trait, two verbs, and why they are not split
//!
//! The rest of this crate narrows hard: the write routes hold a `WriteSubmit`
//! and cannot drain, the reads routes hold a `ReadStore` and cannot write. The
//! narrowing is worth its ceremony wherever *another* holder needs the smaller
//! capability — and here there is none. Reading the settings and changing them
//! are both the config scope's, they are wanted by the same three handlers, and
//! a second consumer of a read-only half does not exist. Two traits over one
//! object would be ceremony around a struct.
//!
//! What the port does narrow is the shape of a change. There is no
//! `set(document)`: a caller states a [`ConfigPatch`], the implementation folds
//! it onto what is running, and every setting the patch does not name keeps the
//! value it has. A whole-document setter would make "leave this alone"
//! unsayable, which is the one thing a config API is asked for most.
//!
//! # What a failure here is
//!
//! [`ConfigRefusal`] has three cases and they are three different people's
//! problems, which is why they are not one string. [`Malformed`] is the
//! caller's: a duration that does not parse. [`Fixed`] is nobody's mistake
//! exactly — the setting is real and the value is readable, and applying it
//! needs a restart rather than a request. [`Source`] is the deployment's: the
//! config file on disk cannot be read, which a reload discovers and a patch
//! never can.
//!
//! [`Malformed`]: ConfigRefusal::Malformed
//! [`Fixed`]: ConfigRefusal::Fixed
//! [`Source`]: ConfigRefusal::Source

use std::sync::Arc;

use sismatic_api_types::{ConfigDocument, ConfigPatch};

/// A convenient object-safe handle, as `DynReadStore` is.
pub type DynLiveConfig = Arc<dyn LiveConfig>;

/// The server's settings, read and changed while it runs.
#[async_trait::async_trait]
pub trait LiveConfig: Send + Sync {
    /// Every setting as it stands right now.
    async fn current(&self) -> ConfigDocument;

    /// Fold `patch` onto what is running, and report what the settings became.
    ///
    /// Whole or not at all. Every value is read and every fixed setting checked
    /// before anything takes effect, so a patch that names one bad duration
    /// among five good ones changes none of the five.
    async fn apply(&self, patch: ConfigPatch) -> Result<ConfigDocument, ConfigRefusal>;

    /// Read the config file again — environment and command line included — and
    /// apply what it says.
    ///
    /// The whole of what the process did at startup, repeated, which is what
    /// makes this the route a mounted ConfigMap is picked up by: the file stays
    /// the source of truth, and nothing has to describe the change twice.
    ///
    /// Subject to the same rules [`apply`](Self::apply) is: a file whose fixed
    /// settings have moved is refused whole, naming them, because the honest
    /// answer to a changed listen port is a restart and not a partial reload.
    async fn reload(&self) -> Result<ConfigDocument, ConfigRefusal>;
}

/// Why a change to the settings was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRefusal {
    /// A value the server could not read. Carries a message naming the text and
    /// the spellings that would have worked, because this is one line an
    /// operator gets and there is no manual beside it.
    Malformed(String),
    /// A setting that cannot change while the process runs, named, with what it
    /// is and what was asked for.
    Fixed(String),
    /// The config file could not be read or parsed. Only a reload can produce
    /// this — a patch names its own values and never opens a file.
    Source(String),
}

impl std::fmt::Display for ConfigRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigRefusal::Malformed(msg)
            | ConfigRefusal::Fixed(msg)
            | ConfigRefusal::Source(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ConfigRefusal {}
