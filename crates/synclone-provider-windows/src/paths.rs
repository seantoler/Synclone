//! Mapping between paths on disk and (drive, path inside the drive), plus
//! the placeholder identity blob and time conversions.

use std::path::{Component, Path, PathBuf};

use cloud_filter::utility::FileTime;
use serde::{Deserialize, Serialize};
use synclone_core::{DriveId, LocalFingerprint, RelPath};

/// Where a path on disk is, relative to the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The sync root itself.
    Root,
    /// Directly inside the sync root: a drive folder (or a stray item).
    TopLevel(String),
    /// Inside a drive folder.
    Inside { folder: String, rel: RelPath },
    /// Not under the sync root.
    Outside,
}

fn normal_components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Prefix(px) => {
                // `\\?\C:` and `C:` name the same volume.
                let s = px.as_os_str().to_string_lossy().to_string();
                Some(s.trim_start_matches(r"\\?\").to_ascii_lowercase())
            }
            Component::RootDir | Component::CurDir => None,
            Component::ParentDir => Some("..".into()),
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
        })
        .collect()
}

/// Classify `path` against `root` (case-insensitively, as NTFS does).
pub fn place(root: &Path, path: &Path) -> Place {
    let r = normal_components(root);
    let p = normal_components(path);
    if p.len() < r.len() || !r.iter().zip(&p).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
        return Place::Outside;
    }
    let rest = &p[r.len()..];
    match rest.len() {
        0 => Place::Root,
        1 => Place::TopLevel(rest[0].clone()),
        _ => match RelPath::new(&rest[1..].join("/")) {
            Some(rel) => Place::Inside {
                folder: rest[0].clone(),
                rel,
            },
            None => Place::Outside,
        },
    }
}

/// `root\folder\a\b` for `a/b`.
pub fn join(root: &Path, folder: &str, rel: &RelPath) -> PathBuf {
    let mut p = root.join(folder);
    for seg in rel.as_str().split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

/// A key for maps of paths (NTFS is case-insensitive).
pub fn key(p: &Path) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").to_lowercase()
}

/// The identity stored in each placeholder (CF "file identity" blob). Kept
/// small: the engine's DB is the source of truth; this only records which
/// drive and which version (size, time) the placeholder stood for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub d: DriveId,
    #[serde(default)]
    pub s: i64,
    #[serde(default)]
    pub m: i64,
}

impl Blob {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// NTFS keeps times in 100 ns units since 1601.
pub fn file_time(mtime_ns: i64) -> FileTime {
    let secs = mtime_ns.div_euclid(1_000_000_000);
    let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
    FileTime::from_unix_time(secs, nanos).unwrap_or(FileTime::UNIX_EPOCH)
}

/// The same fingerprint the stub and the engine use: size and mtime in ns.
pub fn fingerprint(meta: &std::fs::Metadata) -> LocalFingerprint {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    LocalFingerprint {
        size: meta.len() as i64,
        mtime_ns,
    }
}

/// What NTFS will store for `mtime_ns` (rounded down to 100 ns).
pub fn ntfs_ns(mtime_ns: i64) -> i64 {
    mtime_ns - mtime_ns.rem_euclid(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places() {
        let root = Path::new(r"C:\Users\me\Synclone");
        assert_eq!(place(root, Path::new(r"C:\Users\me\Synclone")), Place::Root);
        assert_eq!(place(root, Path::new(r"\\?\C:\Users\me\synclone")), Place::Root);
        assert_eq!(
            place(root, Path::new(r"c:\users\ME\Synclone\Google Drive")),
            Place::TopLevel("Google Drive".into())
        );
        assert_eq!(
            place(root, Path::new(r"C:\Users\me\Synclone\Drive\a\b.txt")),
            Place::Inside {
                folder: "Drive".into(),
                rel: RelPath::new("a/b.txt").unwrap()
            }
        );
        assert_eq!(place(root, Path::new(r"C:\Users\me\Other\x")), Place::Outside);
        assert_eq!(place(root, Path::new(r"C:\Users\me\SyncloneX\x")), Place::Outside);
        assert_eq!(place(root, Path::new(r"D:\Users\me\Synclone\x")), Place::Outside);
        assert_eq!(
            join(root, "Drive", &RelPath::new("a/b.txt").unwrap()),
            PathBuf::from(r"C:\Users\me\Synclone\Drive\a\b.txt")
        );
    }

    #[test]
    fn times_round_trip_at_ntfs_precision() {
        let t = 1_700_000_000_123_456_789i64;
        let ft = file_time(t);
        let back = (u64::from(ft) as i64 - 116_444_736_000_000_000) * 100;
        assert_eq!(back, ntfs_ns(t));
    }
}
