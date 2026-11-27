//! Small Win32 helpers: file attributes, the open-for-writing probe, moves
//! that never overwrite, and the Recycle Bin.

use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SHARING_VIOLATION, HANDLE};
use windows::Win32::Storage::CloudFilters::{CfRevertPlaceholder, CF_REVERT_FLAG_NONE};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
    FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_UNPINNED, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_NO_RECALL, FILE_SHARE_READ, MOVE_FILE_FLAGS,
};
use windows::Win32::UI::Shell::{
    SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
};

/// File attribute bits that matter here, as read from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr(pub u32);

impl Attr {
    pub fn pinned(self) -> bool {
        self.0 & FILE_ATTRIBUTE_PINNED.0 != 0
    }
    pub fn unpinned(self) -> bool {
        self.0 & FILE_ATTRIBUTE_UNPINNED.0 != 0
    }
    /// Content (or, for folders, children) not on disk yet.
    pub fn dehydrated(self) -> bool {
        self.0 & (FILE_ATTRIBUTE_OFFLINE.0 | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0 | FILE_ATTRIBUTE_RECALL_ON_OPEN.0)
            != 0
    }
    /// Cloud Files placeholders are reparse points.
    pub fn reparse(self) -> bool {
        self.0 & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }
}

/// Attributes without opening the file (no hydration).
pub fn attributes(path: &Path) -> Option<Attr> {
    std::fs::symlink_metadata(path).ok().map(|m| Attr(m.file_attributes()))
}

pub fn is_dehydrated(path: &Path) -> bool {
    attributes(path).map(Attr::dehydrated).unwrap_or(false)
}

pub fn is_pinned(path: &Path) -> bool {
    attributes(path).map(Attr::pinned).unwrap_or(false)
}

/// Whether another program has the file open for writing: an open that
/// only shares reading fails with a sharing violation then (PLAN.md §4.4).
/// Never recalls (hydrates) the file.
pub fn is_open_for_writing(path: &Path) -> Result<bool> {
    if is_dehydrated(path) {
        return Ok(false);
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_NO_RECALL.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
    {
        Ok(_) => Ok(false),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking whether {} is open", path.display())),
    }
}

pub fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Rename that fails instead of replacing anything at `to`.
pub fn move_no_replace(from: &Path, to: &Path) -> Result<()> {
    let (f, t) = (wide(from), wide(to));
    unsafe { MoveFileExW(PCWSTR(f.as_ptr()), PCWSTR(t.as_ptr()), MOVE_FILE_FLAGS(0)) }
        .with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

/// Turn a (fully hydrated) placeholder back into an ordinary file, so it
/// can leave the sync root, e.g. for the Recycle Bin.
pub fn revert_placeholder(path: &Path) -> Result<()> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    unsafe { CfRevertPlaceholder(HANDLE(f.as_raw_handle()), CF_REVERT_FLAG_NONE, None) }
        .with_context(|| format!("converting {} back to an ordinary file", path.display()))
}

/// Move a file or folder to the Recycle Bin, silently (§5 rule 6).
pub fn recycle(path: &Path) -> Result<()> {
    // SHFileOperation takes a list ending with two NULs.
    let mut from = wide(path);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        bail!("moving {} to the Recycle Bin failed (code {rc:#x})", path.display());
    }
    if op.fAnyOperationsAborted.as_bool() {
        bail!("moving {} to the Recycle Bin was cancelled", path.display());
    }
    Ok(())
}
