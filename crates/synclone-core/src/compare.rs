//! Telling file versions apart (adapted from OpenCfDrive's FileVersions,
//! MIT; see DECISIONS.md D-006).

use chrono::{DateTime, Utc};
use synclone_rclone::RemoteEntry;

use crate::types::{Drive, RemoteMeta};

/// How a drive's remote tells versions apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRules {
    pub hash_type: Option<String>,
    pub tolerance_ns: i64,
    pub keeps_mod_times: bool,
    /// Compare by size and time only, never by hash (OneDrive for Business
    /// / SharePoint rewrite Office files on upload; PLAN.md §4.7).
    pub ignore_hash: bool,
}

impl VersionRules {
    pub fn for_drive(d: &Drive) -> Self {
        Self {
            hash_type: d.hash_type.clone(),
            tolerance_ns: d.time_tolerance_ns.max(1),
            keeps_mod_times: d.keeps_mod_times,
            ignore_hash: false,
        }
    }

    /// Whether `a` and `b` are the same version. Size first, then the
    /// remote's hash when both have one, else the time within tolerance.
    /// A remote that cannot keep times still compares its own upload time:
    /// that changes whenever the file is written.
    pub fn same(&self, a: &RemoteMeta, b: &RemoteMeta) -> bool {
        if a.is_dir != b.is_dir {
            return false;
        }
        if a.is_dir {
            return true;
        }
        if a.size != b.size {
            return false;
        }
        if !self.ignore_hash {
            if let (Some(x), Some(y)) = (&a.hash, &b.hash) {
                return x.eq_ignore_ascii_case(y);
            }
        }
        match (a.mtime_ns, b.mtime_ns) {
            (Some(x), Some(y)) => (x - y).abs() < self.tolerance_ns,
            // Nothing else to go on: same size and no times means unchanged.
            _ => true,
        }
    }
}

pub fn to_ns(t: &DateTime<Utc>) -> i64 {
    t.timestamp_nanos_opt().unwrap_or_else(|| t.timestamp() * 1_000_000_000)
}

/// Convert an rclone listing entry into `RemoteMeta`, keeping only the
/// drive's hash type.
pub fn remote_meta(e: &RemoteEntry, hash_type: Option<&str>) -> RemoteMeta {
    RemoteMeta {
        is_dir: e.is_dir,
        size: if e.is_dir { 0 } else { e.size },
        mtime_ns: e.mod_time.as_ref().map(to_ns),
        hash: hash_type.and_then(|h| e.hash(h)).map(|s| s.to_ascii_lowercase()),
        id: e.id.clone().filter(|s| !s.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(size: i64, t: i64, h: Option<&str>) -> RemoteMeta {
        RemoteMeta {
            is_dir: false,
            size,
            mtime_ns: Some(t),
            hash: h.map(str::to_string),
            id: None,
        }
    }

    #[test]
    fn hash_beats_time_and_size_comes_first() {
        let r = VersionRules {
            hash_type: Some("md5".into()),
            tolerance_ns: 1_000_000_000,
            keeps_mod_times: true,
            ignore_hash: false,
        };
        assert!(
            r.same(&m(1, 0, Some("a")), &m(1, 99_000_000_000, Some("A"))),
            "touch is not a change"
        );
        assert!(
            !r.same(&m(1, 0, Some("a")), &m(1, 0, Some("b"))),
            "same time, new content"
        );
        assert!(!r.same(&m(1, 0, Some("a")), &m(2, 0, Some("a"))));
        assert!(
            r.same(&m(1, 0, None), &m(1, 999_999_999, Some("a"))),
            "within precision"
        );
        assert!(!r.same(&m(1, 0, None), &m(1, 1_000_000_000, None)));
        let sp = VersionRules { ignore_hash: true, ..r };
        assert!(
            sp.same(&m(1, 0, Some("a")), &m(1, 0, Some("b"))),
            "SharePoint: hash ignored"
        );
    }
}
