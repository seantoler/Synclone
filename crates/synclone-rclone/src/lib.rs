//! rclone integration for Synclone: spawn and supervise `rclone rcd`, and
//! talk to its remote control API through the [`RcloneApi`] trait.
//!
//! [`RcClient`] is the real HTTP client; [`MockRclone`] is an in-memory
//! implementation for tests.

pub mod api;
pub mod client;
pub mod config_flow;
pub mod daemon;
pub mod error;
pub mod locate;
pub mod mock;
pub mod test_root;
pub mod types;

pub use api::{join_remote, split_remote, RcloneApi, REMOTE_WRITE_CALLS};
pub use client::{Endpoint, RcClient};
pub use daemon::{DaemonConfig, DaemonState, RcloneDaemon};
pub use error::{ErrorKind, RcloneError, Result};
pub use mock::MockRclone;
pub use test_root::TestRoot;
pub use types::*;
