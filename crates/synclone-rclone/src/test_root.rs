//! `TestRoot`: the second barrier for live cloud tests (PLAN.md §7 layer 4).
//!
//! The first barrier is the alias remote: live tests only get remotes of
//! type `alias` that point into a `_synclone-test` folder, and rclone cannot
//! address anything above an alias remote's root. On top of that, the
//! harness makes a per-run alias (`<alias>:run-…`) and wraps the rc client
//! in a [`TestRoot`]. Every call that names a location must name that one
//! remote (by its root, the only form Synclone uses, D-006) or a local
//! folder inside one of the allowed temp directories. Anything else is
//! refused before it reaches rclone, and so are all config changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::api::RcloneApi;
use crate::error::{ErrorKind, RcloneError, Result};
use crate::types::*;

pub struct TestRoot {
    inner: Arc<dyn RcloneApi>,
    /// The per-run remote, e.g. `live-run:`.
    fs: String,
    local: Vec<PathBuf>,
}

impl TestRoot {
    /// `remote`: the per-run remote's name (no colon). `local`: folders the
    /// local side of a copy may be in (the test's temp directory).
    pub fn new(inner: Arc<dyn RcloneApi>, remote: &str, local: Vec<PathBuf>) -> Self {
        assert!(
            !remote.is_empty() && !remote.contains([':', ',', '/', '\\']),
            "TestRoot needs a plain remote name"
        );
        assert!(
            local.iter().all(|p| p.is_absolute()),
            "TestRoot local roots must be absolute"
        );
        Self {
            inner,
            fs: format!("{remote}:"),
            local,
        }
    }

    fn refuse(&self, call: &str, why: String) -> RcloneError {
        RcloneError::new(ErrorKind::BadRequest, call, format!("TestRoot refused: {why}"))
    }

    /// The remote side: exactly the run remote, with a relative path that
    /// cannot climb out of it.
    fn check_remote(&self, call: &str, fs: &str, remote: &str) -> Result<()> {
        if fs != self.fs {
            return Err(self.refuse(call, format!("{fs} is not the test remote {}", self.fs)));
        }
        check_rel(remote).map_err(|w| self.refuse(call, w))
    }

    /// Either side of a copy or move: the run remote, or a local folder
    /// inside an allowed directory.
    fn check_side(&self, call: &str, fs: &str, remote: &str) -> Result<()> {
        if fs.ends_with(':') {
            return self.check_remote(call, fs, remote);
        }
        let dir = Path::new(fs);
        let inside = dir.is_absolute()
            && !dir.components().any(|c| matches!(c, std::path::Component::ParentDir))
            && self.local.iter().any(|root| dir.starts_with(root));
        if !inside {
            return Err(self.refuse(call, format!("{fs} is outside the test's folders")));
        }
        check_rel(remote).map_err(|w| self.refuse(call, w))
    }
}

fn check_rel(remote: &str) -> std::result::Result<(), String> {
    if remote.starts_with('/') || remote.contains('\\') || remote.split('/').any(|s| s == "..") || remote.contains(':')
    {
        return Err(format!("path {remote:?} could leave the test root"));
    }
    Ok(())
}

#[async_trait]
impl RcloneApi for TestRoot {
    async fn version(&self) -> Result<VersionInfo> {
        self.inner.version().await
    }
    async fn bwlimit(&self, rate: &str) -> Result<()> {
        self.inner.bwlimit(rate).await
    }
    async fn stats(&self, group: Option<&str>) -> Result<Stats> {
        self.inner.stats(group).await
    }

    async fn list_remotes(&self) -> Result<Vec<String>> {
        // Only the run remote exists, as far as the code under test knows.
        Ok(self
            .inner
            .list_remotes()
            .await?
            .into_iter()
            .filter(|r| format!("{r}:") == self.fs)
            .collect())
    }
    async fn config_get(&self, name: &str) -> Result<Map<String, Value>> {
        if format!("{name}:") != self.fs {
            return Err(self.refuse("config/get", format!("{name} is not the test remote")));
        }
        self.inner.config_get(name).await
    }
    async fn config_create(&self, name: &str, _: &str, _: Map<String, Value>, _: &ConfigOpts) -> Result<ConfigStep> {
        Err(self.refuse("config/create", format!("no config changes ({name})")))
    }
    async fn config_update(&self, name: &str, _: Map<String, Value>, _: &ConfigOpts) -> Result<ConfigStep> {
        Err(self.refuse("config/update", format!("no config changes ({name})")))
    }
    async fn config_delete(&self, name: &str) -> Result<()> {
        Err(self.refuse("config/delete", format!("no config changes ({name})")))
    }
    async fn config_providers(&self) -> Result<Vec<ProviderInfo>> {
        self.inner.config_providers().await
    }
    async fn config_paths(&self) -> Result<ConfigPaths> {
        self.inner.config_paths().await
    }
    async fn oauth_status(&self) -> Result<OAuthStatus> {
        self.inner.oauth_status().await
    }
    async fn oauth_stop(&self) -> Result<()> {
        self.inner.oauth_stop().await
    }

    async fn list(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Vec<RemoteEntry>> {
        self.check_remote("operations/list", fs, remote)?;
        self.inner.list(fs, remote, opts).await
    }
    async fn stat(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Option<RemoteEntry>> {
        self.check_remote("operations/stat", fs, remote)?;
        self.inner.stat(fs, remote, opts).await
    }
    async fn fsinfo(&self, fs: &str) -> Result<FsInfo> {
        self.check_remote("operations/fsinfo", fs, "")?;
        self.inner.fsinfo(fs).await
    }
    async fn about(&self, fs: &str) -> Result<About> {
        self.check_remote("operations/about", fs, "")?;
        self.inner.about(fs).await
    }
    async fn hashsum_file(&self, fs: &str, remote: &str, hash_type: &str) -> Result<Option<String>> {
        self.check_side("operations/hashsumfile", fs, remote)?;
        self.inner.hashsum_file(fs, remote, hash_type).await
    }

    async fn copyfile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.check_side("operations/copyfile", src_fs, src_remote)?;
        self.check_side("operations/copyfile", dst_fs, dst_remote)?;
        self.inner.copyfile(src_fs, src_remote, dst_fs, dst_remote).await
    }
    async fn movefile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.check_side("operations/movefile", src_fs, src_remote)?;
        self.check_side("operations/movefile", dst_fs, dst_remote)?;
        self.inner.movefile(src_fs, src_remote, dst_fs, dst_remote).await
    }
    async fn deletefile(&self, fs: &str, remote: &str) -> Result<()> {
        self.check_remote("operations/deletefile", fs, remote)?;
        self.inner.deletefile(fs, remote).await
    }
    async fn mkdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.check_remote("operations/mkdir", fs, remote)?;
        self.inner.mkdir(fs, remote).await
    }
    async fn rmdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.check_remote("operations/rmdir", fs, remote)?;
        self.inner.rmdir(fs, remote).await
    }
    async fn purge(&self, fs: &str, remote: &str) -> Result<()> {
        self.check_remote("operations/purge", fs, remote)?;
        self.inner.purge(fs, remote).await
    }
    async fn movedir(&self, fs: &str, src: &str, dst: &str) -> Result<()> {
        self.check_remote("sync/move", fs, src)?;
        self.check_remote("sync/move", fs, dst)?;
        self.inner.movedir(fs, src, dst).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockRclone;

    fn guard(mock: &MockRclone, tmp: &Path) -> TestRoot {
        mock.add_remote("run");
        mock.add_remote("real");
        TestRoot::new(Arc::new(mock.clone()), "run", vec![tmp.to_path_buf()])
    }

    #[tokio::test]
    async fn allows_the_run_remote_and_the_test_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let mock = MockRclone::new();
        let g = guard(&mock, tmp.path());
        mock.put("run:", "a/x.txt", b"x");
        assert!(g.stat("run:", "a/x.txt", &ListOpts::default()).await.unwrap().is_some());
        let local = tmp.path().join("dl");
        std::fs::create_dir_all(&local).unwrap();
        g.copyfile("run:", "a/x.txt", &local.display().to_string(), "x.txt")
            .await
            .unwrap();
        g.deletefile("run:", "a/x.txt").await.unwrap();
        mock.put("run:", "b/y.txt", b"y");
        g.purge("run:", "b").await.unwrap();
        assert!(mock.get("run:", "b/y.txt").is_none());
        assert_eq!(g.list_remotes().await.unwrap(), vec!["run".to_string()]);
    }

    #[tokio::test]
    async fn refuses_everything_else_before_it_reaches_rclone() {
        let tmp = tempfile::tempdir().unwrap();
        let mock = MockRclone::new();
        let g = guard(&mock, tmp.path());
        mock.put("real:", "keep.txt", b"keep");
        let before = mock.calls().len();
        let o = ListOpts::default();
        let outside = std::env::temp_dir().join("not-this-test");
        let outside = outside.display().to_string();
        let inside = tmp.path().display().to_string();
        let climb = tmp.path().join("..").join("x").display().to_string();
        let refused = [
            g.deletefile("real:", "keep.txt").await.err(),
            g.purge("real:", "").await.err(),
            g.purge("run,opt=1:", "").await.err(),
            g.deletefile("run:", "../keep.txt").await.err(),
            g.deletefile("run:", "/keep.txt").await.err(),
            g.deletefile("run:", "real:keep.txt").await.err(),
            g.copyfile(&inside, "x", "real:", "x").await.err(),
            g.copyfile("real:", "keep.txt", &inside, "k").await.err(),
            g.copyfile("run:", "x", &outside, "x").await.err(),
            g.copyfile("run:", "x", &climb, "x").await.err(),
            g.movedir("run:", "a", "../b").await.err(),
            g.list("real:", "", &o).await.err(),
            g.config_delete("real").await.err(),
            g.config_create("x", "alias", Map::new(), &ConfigOpts::default())
                .await
                .err(),
            g.config_get("real").await.err(),
        ];
        for (i, e) in refused.iter().enumerate() {
            let e = e.as_ref().unwrap_or_else(|| panic!("call {i} was allowed"));
            assert!(e.message.starts_with("TestRoot refused"), "call {i}: {e}");
        }
        assert_eq!(mock.calls().len(), before, "nothing reached rclone");
        assert_eq!(mock.get("real:", "keep.txt").unwrap(), b"keep");
    }
}
