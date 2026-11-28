//! The sync engine. Split by concern:
//! - `drives`: adding, importing, disconnecting cloud drives;
//! - `index`: the remote crawler and polling differ (Phase 2);
//! - `sync`: reconciliation per PLAN.md §5, transfers, pin/free (Phase 3);
//! - `trash`: the delete queue (§4.6);
//! - `ops`: the durable op journal runner.

pub mod drives;
pub mod hot;
pub mod index;
pub mod ops;
pub mod sync;
pub mod trash;

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use synclone_rclone::RcloneApi;

use crate::db::Db;
use crate::ignore::IgnoreList;
use crate::provider::Provider;

/// Milliseconds since the epoch. Injected so tests control debounce and backoff.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::db::now_ms()
    }
}

/// A clock tests move by hand.
#[derive(Default)]
pub struct TestClock(AtomicI64);

impl TestClock {
    pub fn new(start: i64) -> Self {
        Self(AtomicI64::new(start))
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// Quiet time before a local change is uploaded (§4.4).
    pub debounce_ms: i64,
    /// Retry delay for a file that is open (§5 rule 3, §4.4).
    pub open_retry_ms: i64,
    /// Concurrent transfers (§4.5).
    pub concurrency: usize,
    /// Backoff after failures: base, doubling, capped.
    pub backoff_base_ms: i64,
    pub backoff_max_ms: i64,
    /// Give up (mark failed, keep in journal, report) after this many attempts.
    pub max_attempts: i64,
    /// Default poll interval for new drives (§4.3).
    pub default_poll_secs: i64,
    /// Re-list a folder on open if its listing is older than this (§4.3).
    pub relist_on_open_ms: i64,
    pub extra_ignore: Vec<String>,
    /// The Google client new Drive remotes use; the built-in one by
    /// default (D-030). `None`: rclone's shared client.
    pub google_client: Option<crate::oauth::OAuthClient>,
    /// Import only these rclone remotes as drives (`SYNCLONE_ONLY_REMOTES`);
    /// `None` imports every remote (PLAN.md §1.1).
    pub only_remotes: Option<Vec<String>>,
    /// Hot folders (D-031): seconds between listings of open or recently
    /// used folders. `None`: the service's default (20 s for Google Drive
    /// and OneDrive, 30 s otherwise); `Some(0)` turns it off.
    pub hot_poll_secs: Option<i64>,
    /// How many hot folders per drive; `None`: the service's default.
    pub hot_max_folders: Option<usize>,
    /// A folder stays hot this long after it was last browsed or used.
    pub hot_ttl_secs: i64,
    /// The longest the hot interval grows to while rate-limited.
    pub hot_max_backoff_secs: i64,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            debounce_ms: 3_000,
            open_retry_ms: 5_000,
            concurrency: 4,
            backoff_base_ms: 2_000,
            backoff_max_ms: 15 * 60_000,
            max_attempts: 50,
            default_poll_secs: 300,
            relist_on_open_ms: 30_000,
            extra_ignore: Vec::new(),
            google_client: crate::oauth::built_in_google(),
            only_remotes: None,
            hot_poll_secs: None,
            hot_max_folders: None,
            hot_ttl_secs: 600,
            hot_max_backoff_secs: 600,
        }
    }
}

/// Everything the engine needs, shared by the daemon's tasks.
pub struct Engine {
    pub db: Arc<Db>,
    pub api: Arc<dyn RcloneApi>,
    pub provider: Arc<dyn Provider>,
    pub clock: Arc<dyn Clock>,
    pub settings: EngineSettings,
    pub ignore: IgnoreList,
    /// Where downloads land before they are handed to the provider.
    pub staging_dir: PathBuf,
    /// Where files from a disconnected drive with unsynced edits are kept.
    pub recovered_dir: PathBuf,
    /// Device name for conflict copies.
    pub device: String,
    /// Paths with an op in flight (no two ops on one path at once).
    pub(crate) busy: std::sync::Mutex<std::collections::HashSet<(crate::types::DriveId, String)>>,
    /// Hot folders per drive (D-031).
    pub(crate) hot: std::sync::Mutex<std::collections::HashMap<crate::types::DriveId, hot::HotState>>,
}

impl Engine {
    pub fn new(
        db: Arc<Db>,
        api: Arc<dyn RcloneApi>,
        provider: Arc<dyn Provider>,
        clock: Arc<dyn Clock>,
        settings: EngineSettings,
        state_dir: PathBuf,
    ) -> Arc<Self> {
        let ignore = IgnoreList::new(&settings.extra_ignore);
        let staging_dir = state_dir.join("staging");
        let _ = std::fs::create_dir_all(&staging_dir);
        Arc::new(Self {
            db,
            api,
            provider,
            clock,
            settings,
            ignore,
            staging_dir,
            recovered_dir: state_dir.join("Recovered"),
            device: crate::conflict::device_name(),
            busy: Default::default(),
            hot: Default::default(),
        })
    }

    pub fn now(&self) -> i64 {
        self.clock.now_ms()
    }
}
