//! Synclone core: domain types, state database, sync engine, conflict
//! rules, trash queue and the `Provider` trait. No OS-specific code.

pub mod compare;
pub mod conflict;
pub mod db;
pub mod engine;
pub mod ignore;
pub mod names;
pub mod provider;
pub mod types;

pub use engine::{Clock, Engine, EngineSettings, SystemClock, TestClock};
pub use provider::{LocalEntry, NameRules, Provider, ProviderEvent};
pub use types::*;

/// The product name. Change it here and nowhere else.
pub const APP_NAME: &str = "Synclone";
/// Lower-case identifier used for file, directory, socket and pipe names.
pub const APP_ID: &str = "synclone";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
