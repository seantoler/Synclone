//! The `RcloneApi` trait: every rc call the rest of Synclone makes.
//!
//! Paths follow rclone's convention: `fs` names a remote root (`"gdrive:"`)
//! or a local directory (an absolute path), `remote` is a path inside it.
//! Following OpenCfDrive's experience, callers pass only a remote's root as
//! `fs` and put sub-paths in `remote`: opening a sub-path as an fs root makes
//! rclone probe it as a file first, which breaks on some WebDAV servers.

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::error::Result;
use crate::types::*;

/// Calls that change remote data. The mock counts these so tests can prove
/// an operation (e.g. disconnecting a drive) made none of them.
pub const REMOTE_WRITE_CALLS: &[&str] = &[
    "operations/copyfile",
    "operations/movefile",
    "operations/deletefile",
    "operations/mkdir",
    "operations/rmdir",
    "operations/purge",
];

#[async_trait]
pub trait RcloneApi: Send + Sync {
    // ---- core ----
    async fn version(&self) -> Result<VersionInfo>;
    /// Set the bandwidth limit, e.g. `"off"`, `"10M"`, `"1M:2M"`.
    async fn bwlimit(&self, rate: &str) -> Result<()>;
    async fn stats(&self, group: Option<&str>) -> Result<Stats>;

    // ---- config ----
    async fn list_remotes(&self) -> Result<Vec<String>>;
    /// A remote's settings. They contain secrets: never log them.
    async fn config_get(&self, name: &str) -> Result<Map<String, Value>>;
    async fn config_create(
        &self,
        name: &str,
        kind: &str,
        parameters: Map<String, Value>,
        opts: &ConfigOpts,
    ) -> Result<ConfigStep>;
    async fn config_update(&self, name: &str, parameters: Map<String, Value>, opts: &ConfigOpts) -> Result<ConfigStep>;
    async fn config_delete(&self, name: &str) -> Result<()>;
    async fn config_providers(&self) -> Result<Vec<ProviderInfo>>;
    async fn config_paths(&self) -> Result<ConfigPaths>;
    async fn oauth_status(&self) -> Result<OAuthStatus>;
    async fn oauth_stop(&self) -> Result<()>;

    // ---- reading ----
    async fn list(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Vec<RemoteEntry>>;
    /// `Ok(None)` when nothing exists at the path.
    async fn stat(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Option<RemoteEntry>>;
    async fn fsinfo(&self, fs: &str) -> Result<FsInfo>;
    async fn about(&self, fs: &str) -> Result<About>;
    async fn hashsum_file(&self, fs: &str, remote: &str, hash_type: &str) -> Result<Option<String>>;

    // ---- writing (counted by the mock; see REMOTE_WRITE_CALLS) ----
    /// Copy one file. Either side may be local (an absolute directory as `fs`).
    async fn copyfile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()>;
    async fn movefile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()>;
    async fn deletefile(&self, fs: &str, remote: &str) -> Result<()>;
    async fn mkdir(&self, fs: &str, remote: &str) -> Result<()>;
    async fn rmdir(&self, fs: &str, remote: &str) -> Result<()>;
    /// Remove a directory and everything in it (to the provider's trash
    /// where it has one; Synclone never sets hard-delete options).
    async fn purge(&self, fs: &str, remote: &str) -> Result<()>;
}

/// Split `"remote:path/inside"` into `("remote:", "path/inside")`.
/// A string without a colon (a local path) is returned whole as the fs.
pub fn split_remote(spec: &str) -> (String, String) {
    let s = spec.trim();
    // A Windows drive letter ("C:\...") is a local path, not a remote.
    let bytes = s.as_bytes();
    let is_drive_letter = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'\\' || bytes[2] == b'/');
    if is_drive_letter || s.starts_with('/') || s.starts_with("\\\\") {
        return (s.to_string(), String::new());
    }
    match s.find(':') {
        Some(i) => (s[..=i].to_string(), s[i + 1..].trim_matches('/').to_string()),
        None => (s.to_string(), String::new()),
    }
}

/// Join path segments with `/`, skipping empty ones.
pub fn join_remote(a: &str, b: &str) -> String {
    let a = a.trim_matches('/');
    let b = b.trim_matches('/');
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_remote_specs() {
        assert_eq!(split_remote("gdrive:"), ("gdrive:".into(), "".into()));
        assert_eq!(split_remote("gdrive:a/b/"), ("gdrive:".into(), "a/b".into()));
        assert_eq!(split_remote("/tmp/x"), ("/tmp/x".into(), "".into()));
        assert_eq!(split_remote("C:\\Users\\me"), ("C:\\Users\\me".into(), "".into()));
        assert_eq!(join_remote("", "a"), "a");
        assert_eq!(join_remote("root/", "/a/b"), "root/a/b");
    }
}
