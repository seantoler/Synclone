//! Domain types shared by the engine, the database and providers.

use serde::{Deserialize, Serialize};
use std::fmt;

pub type DriveId = i64;

/// A path inside a drive: `/`-separated, no leading or trailing slash, no
/// `.` or `..` segments. The empty path is the drive's root folder.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    pub fn root() -> Self {
        RelPath(String::new())
    }

    /// Normalise `s` (accepts `\` too). Returns `None` for paths that try to
    /// leave the drive (`..`) or contain NUL.
    pub fn new(s: &str) -> Option<Self> {
        let mut parts = Vec::new();
        for seg in s.split(['/', '\\']) {
            match seg {
                "" | "." => continue,
                ".." => return None,
                s if s.contains('\0') => return None,
                s => parts.push(s),
            }
        }
        Some(RelPath(parts.join("/")))
    }

    /// Build from a string already known to be normal (DB rows).
    pub fn from_normalized(s: String) -> Self {
        RelPath(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    pub fn parent(&self) -> Option<RelPath> {
        if self.0.is_empty() {
            return None;
        }
        Some(match self.0.rfind('/') {
            Some(i) => RelPath(self.0[..i].to_string()),
            None => RelPath::root(),
        })
    }

    pub fn join(&self, name: &str) -> RelPath {
        if self.0.is_empty() {
            RelPath::new(name).unwrap_or_default()
        } else {
            RelPath::new(&format!("{}/{}", self.0, name)).unwrap_or_else(|| self.clone())
        }
    }

    /// True when `self` is `other` or inside it.
    pub fn starts_with(&self, other: &RelPath) -> bool {
        other.0.is_empty()
            || self.0 == other.0
            || (self.0.starts_with(&other.0) && self.0.as_bytes().get(other.0.len()) == Some(&b'/'))
    }

    /// Re-root: `a/b/c` with prefix `a` → new prefix `x` gives `x/b/c`.
    pub fn rebase(&self, from: &RelPath, to: &RelPath) -> Option<RelPath> {
        if !self.starts_with(from) {
            return None;
        }
        let rest = &self.0[from.0.len()..].trim_start_matches('/');
        Some(if rest.is_empty() { to.clone() } else { to.join(rest) })
    }

    /// Ancestors from the parent up to the root (root last).
    pub fn ancestors(&self) -> Vec<RelPath> {
        let mut out = Vec::new();
        let mut p = self.parent();
        while let Some(x) = p {
            p = x.parent();
            out.push(x);
        }
        out
    }

    pub fn depth(&self) -> usize {
        if self.0.is_empty() {
            0
        } else {
            self.0.matches('/').count() + 1
        }
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("/")
        } else {
            f.write_str(&self.0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveState {
    Connected,
    /// Disconnected by the user (or by deleting its folder). Its sign-in is
    /// kept so it can be connected again in one step.
    Disconnected,
}

impl DriveState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DriveState::Connected => "connected",
            DriveState::Disconnected => "disconnected",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s == "disconnected" {
            DriveState::Disconnected
        } else {
            DriveState::Connected
        }
    }
}

/// A connected cloud storage: an rclone remote (plus an optional folder
/// inside it) shown as one top-level folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    pub id: DriveId,
    /// rclone remote name, without the colon.
    pub remote_name: String,
    /// Folder name shown to the user.
    pub display_name: String,
    /// Folder inside the remote that the drive shows ("" for its root).
    pub root_path: String,
    /// rclone backend type (drive, onedrive, drime, ...).
    pub provider_type: String,
    pub added_at: i64,
    pub paused: bool,
    pub state: DriveState,
    /// Came from the user's existing rclone config rather than the wizard.
    pub imported: bool,
    /// Hash type used to compare versions (None: size + time only).
    pub hash_type: Option<String>,
    /// Modification-time tolerance in ns (see `FsInfo::time_tolerance_ns`).
    pub time_tolerance_ns: i64,
    /// Whether the remote can store modification times.
    pub keeps_mod_times: bool,
    pub poll_interval_secs: i64,
}

impl Drive {
    /// The rclone `fs` string (only the remote's root, see D-006).
    pub fn fs(&self) -> String {
        format!("{}:", self.remote_name)
    }

    /// The path inside the remote for `rel`.
    pub fn remote_path(&self, rel: &RelPath) -> String {
        synclone_rclone::join_remote(&self.root_path, rel.as_str())
    }
}

/// What the remote says about one item (from a listing or a stat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteMeta {
    pub is_dir: bool,
    pub size: i64,
    /// Nanoseconds since the epoch, when known.
    pub mtime_ns: Option<i64>,
    pub hash: Option<String>,
    pub id: Option<String>,
}

/// Item metadata handed to providers to create placeholders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMeta {
    pub path: RelPath,
    pub is_dir: bool,
    pub size: i64,
    pub mtime_ns: Option<i64>,
}

/// What the engine records about the local copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalState {
    /// Visible but not on the device.
    Placeholder,
    /// Content on the device; kept until the user frees it.
    Downloaded,
    /// Deleted locally; waiting in Synclone's trash queue.
    Trashed,
}

impl LocalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LocalState::Placeholder => "placeholder",
            LocalState::Downloaded => "downloaded",
            LocalState::Trashed => "trashed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "downloaded" => LocalState::Downloaded,
            "trashed" => LocalState::Trashed,
            _ => LocalState::Placeholder,
        }
    }
}

/// What is on disk at a path right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalStatus {
    Absent,
    Placeholder,
    Dir,
    File(LocalFingerprint),
}

/// Size and modification time of a local file: how local edits are told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFingerprint {
    pub size: i64,
    pub mtime_ns: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_normalises() {
        assert_eq!(RelPath::new("/a//b/./c/").unwrap().as_str(), "a/b/c");
        assert_eq!(RelPath::new("a\\b").unwrap().as_str(), "a/b");
        assert!(RelPath::new("a/../b").is_none());
        assert!(RelPath::new("").unwrap().is_root());
        let p = RelPath::new("a/b/c.txt").unwrap();
        assert_eq!(p.name(), "c.txt");
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        assert_eq!(RelPath::new("a").unwrap().parent().unwrap(), RelPath::root());
        assert!(p.starts_with(&RelPath::new("a/b").unwrap()));
        assert!(!RelPath::new("a/bc").unwrap().starts_with(&RelPath::new("a/b").unwrap()));
        assert_eq!(
            p.rebase(&RelPath::new("a").unwrap(), &RelPath::new("x/y").unwrap())
                .unwrap()
                .as_str(),
            "x/y/b/c.txt"
        );
        assert_eq!(p.ancestors().len(), 3);
        assert_eq!(p.depth(), 3);
    }
}
