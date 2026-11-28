//! The `Provider` trait: the only way the engine touches the OS
//! (PLAN.md §2.4). Implementations: the stub (plain folders, tests and
//! Linux), Windows Cloud Files API (Phase 4), macOS File Provider (Phase 6).

use std::path::{Path, PathBuf};

use crate::types::*;

pub type ProviderResult<T> = anyhow::Result<T>;

/// Rules for which names the local file system can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameRules {
    /// Reject `<>:"/\|?*`, control characters, trailing dots/spaces and
    /// reserved device names (CON, NUL, COM1...).
    pub windows: bool,
    /// Names differing only in case collide.
    pub case_insensitive: bool,
}

impl NameRules {
    pub const POSIX: NameRules = NameRules {
        windows: false,
        case_insensitive: false,
    };
    pub const WINDOWS: NameRules = NameRules {
        windows: true,
        case_insensitive: true,
    };
    pub const MACOS: NameRules = NameRules {
        windows: false,
        case_insensitive: true,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The user opened a placeholder.
    HydrationRequested {
        drive: DriveId,
        path: RelPath,
    },
    /// The OS filled a placeholder itself (Windows streams the content when
    /// a program opens it). `size` / `mtime_ns` describe the version the
    /// placeholder stood for, as it is now on disk.
    Hydrated {
        drive: DriveId,
        path: RelPath,
        size: i64,
        mtime_ns: i64,
    },
    /// "Save to device" / "Always keep on this device".
    PinRequested {
        drive: DriveId,
        path: RelPath,
        recursive: bool,
    },
    /// "Remove from device" / "Free up space".
    FreeRequested {
        drive: DriveId,
        path: RelPath,
        recursive: bool,
    },
    LocalCreated {
        drive: DriveId,
        path: RelPath,
    },
    LocalModified {
        drive: DriveId,
        path: RelPath,
    },
    LocalRenamed {
        drive: DriveId,
        from: RelPath,
        to: RelPath,
    },
    LocalDeleted {
        drive: DriveId,
        path: RelPath,
    },
    /// The user browsed into a folder.
    FolderEnumerated {
        drive: DriveId,
        path: RelPath,
    },
    /// The drive's top-level folder was deleted: treat as disconnect.
    DriveRootRemoved {
        drive: DriveId,
    },
    /// The drive's top-level folder was renamed: new display name.
    DriveRootRenamed {
        drive: DriveId,
        new_name: String,
    },
}

/// One entry of a local scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub path: RelPath,
    pub status: LocalStatus,
}

pub trait Provider: Send + Sync {
    /// Create the drive's top-level folder.
    fn register_drive(&self, drive: &Drive) -> ProviderResult<()>;
    /// Remove the drive's local folder. Never touches remote data.
    fn unregister_drive(&self, drive: &Drive) -> ProviderResult<()>;
    /// Create or update placeholders for `items` (children of `parent`).
    /// Existing downloaded files are left as they are.
    fn upsert_placeholders(&self, drive: &Drive, parent: &RelPath, items: &[ItemMeta]) -> ProviderResult<()>;
    /// Remove the local copy or placeholder (downloaded content goes to the
    /// OS Recycle Bin / Trash).
    fn remove_local(&self, drive: &Drive, path: &RelPath) -> ProviderResult<()>;
    /// `Placeholder` frees the local content (dehydrate). `Downloaded` marks
    /// it to be kept (pinned against OS eviction).
    fn set_state(&self, drive: &Drive, path: &RelPath, state: LocalState) -> ProviderResult<()>;
    /// Whether a program has the file open (for writing, where the OS can tell).
    fn is_open(&self, drive: &Drive, path: &RelPath) -> ProviderResult<bool>;
    /// What is on disk at `path`.
    fn local_status(&self, drive: &Drive, path: &RelPath) -> ProviderResult<LocalStatus>;
    /// Where a downloaded file's content lives on disk (uploads read it).
    fn content_path(&self, drive: &Drive, path: &RelPath) -> PathBuf;
    /// Put downloaded content in place, replacing a placeholder or an older
    /// copy. Refuses (error) if the local file no longer matches `expected`
    /// (it changed since the engine decided). Returns the new fingerprint.
    fn commit_download(
        &self,
        drive: &Drive,
        path: &RelPath,
        staged: &Path,
        meta: &ItemMeta,
        expected: LocalStatus,
    ) -> ProviderResult<LocalFingerprint>;
    /// Rename a local item (remote renames). Must fail rather than
    /// overwrite anything at `to`.
    fn rename_local(&self, drive: &Drive, from: &RelPath, to: &RelPath) -> ProviderResult<()>;
    /// Copy a downloaded local file to a new local path (conflict copies).
    fn copy_local(&self, drive: &Drive, from: &RelPath, to: &RelPath) -> ProviderResult<()>;
    /// Create a local folder.
    fn create_dir(&self, drive: &Drive, path: &RelPath) -> ProviderResult<()>;
    /// Walk the drive's local folder (to find changes made while Synclone
    /// was not running, or where the OS reports none).
    fn scan(&self, drive: &Drive) -> ProviderResult<Vec<LocalEntry>>;
    /// Whether the OS has listed this folder, so its placeholders exist and
    /// must be kept up to date. Folders never browsed are filled on demand.
    fn is_populated(&self, drive: &Drive, dir: &RelPath) -> bool;
    fn name_rules(&self) -> NameRules;
    /// The OS event stream; taken once by the daemon.
    fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>>;
}
