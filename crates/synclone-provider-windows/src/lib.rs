//! Windows Cloud Files API provider (PLAN.md §3.1).
//!
//! One sync root for the whole app (default `%USERPROFILE%\Synclone`); each
//! cloud drive is a placeholder folder directly inside it. Folders are
//! populated on demand from the state DB, files are hydrated by streaming
//! from rclone's `--rc-serve` with Explorer's native progress, and the
//! built-in "Always keep on this device" / "Free up space" entries map to
//! save to device / remove from device. See DECISIONS.md D-021 to D-023.
//!
//! Built on the `cloud-filter` crate (D-021 records the spike).

#[cfg(windows)]
mod explorer;
#[cfg(windows)]
mod filter;
#[cfg(windows)]
mod paths;
#[cfg(windows)]
mod provider;
#[cfg(windows)]
pub mod recycle_bin;
#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use explorer::open_folders as open_explorer_folders;
#[cfg(windows)]
pub use provider::{WindowsProvider, WindowsProviderConfig};
#[cfg(windows)]
pub use win::{attributes, is_dehydrated, is_pinned, Attr};
