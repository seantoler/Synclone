//! A `Provider` backed by plain folders. A placeholder for `name` is an
//! empty file `name.sclstub`; a downloaded file is the real file. Used for
//! engine tests on Linux and CI (PLAN.md §2.3). Not shipped to users in v1.
//!
//! Layout: `<root>/<drive display name>/<path>`. Files removed "to the OS
//! trash" go to `<recycle>/<n>-<name>` so tests can check nothing is lost.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use synclone_core::provider::{LocalEntry, NameRules, Provider, ProviderEvent};
use synclone_core::*;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub const STUB_EXT: &str = ".sclstub";

pub struct StubProvider {
    root: PathBuf,
    recycle: PathBuf,
    rules: NameRules,
    open: Mutex<HashSet<(DriveId, String)>>,
    tx: UnboundedSender<ProviderEvent>,
    rx: Mutex<Option<UnboundedReceiver<ProviderEvent>>>,
    seq: AtomicU64,
}

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

impl StubProvider {
    pub fn new(root: impl Into<PathBuf>, recycle: impl Into<PathBuf>) -> Self {
        let (tx, rx) = unbounded_channel();
        let root = root.into();
        let recycle = recycle.into();
        let _ = std::fs::create_dir_all(&root);
        let _ = std::fs::create_dir_all(&recycle);
        Self {
            root,
            recycle,
            rules: NameRules::POSIX,
            open: Mutex::new(HashSet::new()),
            tx,
            rx: Mutex::new(Some(rx)),
            seq: AtomicU64::new(0),
        }
    }

    pub fn with_rules(mut self, rules: NameRules) -> Self {
        self.rules = rules;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn recycle_dir(&self) -> &Path {
        &self.recycle
    }

    pub fn drive_dir(&self, d: &Drive) -> PathBuf {
        self.root.join(&d.display_name)
    }

    pub fn path_of(&self, d: &Drive, p: &RelPath) -> PathBuf {
        let mut x = self.drive_dir(d);
        for seg in p.as_str().split('/').filter(|s| !s.is_empty()) {
            x.push(seg);
        }
        x
    }

    fn stub_of(&self, d: &Drive, p: &RelPath) -> PathBuf {
        let real = self.path_of(d, p);
        let mut s = real.into_os_string();
        s.push(STUB_EXT);
        PathBuf::from(s)
    }

    /// Mark a file as open by some program (tests).
    pub fn set_open(&self, d: DriveId, p: &RelPath, open: bool) {
        let mut g = self.open.lock().unwrap();
        if open {
            g.insert((d, p.as_str().to_string()));
        } else {
            g.remove(&(d, p.as_str().to_string()));
        }
    }

    /// Send an OS event, as Explorer / Finder would.
    pub fn emit(&self, ev: ProviderEvent) {
        let _ = self.tx.send(ev);
    }

    fn to_recycle(&self, path: &Path) -> Result<()> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let dest = self
            .recycle
            .join(format!("{}-{}", self.seq.fetch_add(1, Ordering::Relaxed), name));
        if std::fs::rename(path, &dest).is_err() {
            // Different file system: copy then remove.
            if path.is_dir() {
                copy_dir(path, &dest)?;
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::copy(path, &dest)?;
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn walk(&self, d: &Drive, dir: &Path, rel: &RelPath, out: &mut Vec<LocalEntry>) -> Result<()> {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for e in rd {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            let meta = match std::fs::symlink_metadata(e.path()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                let p = rel.join(&name);
                out.push(LocalEntry {
                    path: p.clone(),
                    status: LocalStatus::Dir,
                });
                self.walk(d, &e.path(), &p, out)?;
            } else if let Some(base) = name.strip_suffix(STUB_EXT) {
                let p = rel.join(base);
                // A real file next to a stub wins.
                if !self.path_of(d, &p).exists() {
                    out.push(LocalEntry {
                        path: p,
                        status: LocalStatus::Placeholder,
                    });
                }
            } else if meta.is_file() {
                out.push(LocalEntry {
                    path: rel.join(&name),
                    status: LocalStatus::File(fingerprint(&meta)),
                });
            }
        }
        Ok(())
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let t = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &t)?;
        } else {
            std::fs::copy(e.path(), t)?;
        }
    }
    Ok(())
}

impl Provider for StubProvider {
    fn register_drive(&self, drive: &Drive) -> Result<()> {
        std::fs::create_dir_all(self.drive_dir(drive))?;
        Ok(())
    }

    fn unregister_drive(&self, drive: &Drive) -> Result<()> {
        let dir = self.drive_dir(drive);
        if dir.exists() {
            if !dir.starts_with(&self.root) {
                bail!("refusing to remove {} outside the sync root", dir.display());
            }
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    fn upsert_placeholders(&self, drive: &Drive, parent: &RelPath, items: &[ItemMeta]) -> Result<()> {
        let pdir = self.path_of(drive, parent);
        std::fs::create_dir_all(&pdir)?;
        for it in items {
            if it.is_dir {
                std::fs::create_dir_all(self.path_of(drive, &it.path))?;
            } else if !self.path_of(drive, &it.path).exists() {
                let stub = self.stub_of(drive, &it.path);
                if !stub.exists() {
                    std::fs::write(&stub, b"")?;
                }
            }
        }
        Ok(())
    }

    fn remove_local(&self, drive: &Drive, path: &RelPath) -> Result<()> {
        let real = self.path_of(drive, path);
        if real.exists() {
            self.to_recycle(&real)?;
        }
        let stub = self.stub_of(drive, path);
        if stub.exists() {
            std::fs::remove_file(stub)?;
        }
        Ok(())
    }

    fn set_state(&self, drive: &Drive, path: &RelPath, state: LocalState) -> Result<()> {
        if state == LocalState::Placeholder {
            let real = self.path_of(drive, path);
            if real.is_file() {
                std::fs::remove_file(&real)?;
                std::fs::write(self.stub_of(drive, path), b"")?;
            }
        }
        Ok(())
    }

    fn is_open(&self, drive: &Drive, path: &RelPath) -> Result<bool> {
        Ok(self
            .open
            .lock()
            .unwrap()
            .contains(&(drive.id, path.as_str().to_string())))
    }

    fn local_status(&self, drive: &Drive, path: &RelPath) -> Result<LocalStatus> {
        let real = self.path_of(drive, path);
        match std::fs::symlink_metadata(&real) {
            Ok(m) if m.is_dir() => return Ok(LocalStatus::Dir),
            Ok(m) if m.is_file() => return Ok(LocalStatus::File(fingerprint(&m))),
            _ => {}
        }
        if !path.is_root() && self.stub_of(drive, path).is_file() {
            return Ok(LocalStatus::Placeholder);
        }
        Ok(LocalStatus::Absent)
    }

    fn content_path(&self, drive: &Drive, path: &RelPath) -> PathBuf {
        self.path_of(drive, path)
    }

    fn commit_download(
        &self,
        drive: &Drive,
        path: &RelPath,
        staged: &Path,
        _meta: &ItemMeta,
        expected: LocalStatus,
    ) -> Result<LocalFingerprint> {
        let now = self.local_status(drive, path)?;
        let ok = match (expected, now) {
            (LocalStatus::File(a), LocalStatus::File(b)) => a == b,
            (LocalStatus::Placeholder | LocalStatus::Absent, LocalStatus::Placeholder | LocalStatus::Absent) => true,
            _ => false,
        };
        if !ok {
            return Err(anyhow!("{path} changed locally ({now:?}, expected {expected:?})"));
        }
        let real = self.path_of(drive, path);
        if let Some(p) = real.parent() {
            std::fs::create_dir_all(p)?;
        }
        if std::fs::rename(staged, &real).is_err() {
            std::fs::copy(staged, &real).with_context(|| format!("placing {}", real.display()))?;
        }
        let stub = self.stub_of(drive, path);
        if stub.exists() {
            std::fs::remove_file(stub)?;
        }
        Ok(fingerprint(&std::fs::metadata(&real)?))
    }

    fn rename_local(&self, drive: &Drive, from: &RelPath, to: &RelPath) -> Result<()> {
        let (src, dst) = if self.path_of(drive, from).exists() {
            (self.path_of(drive, from), self.path_of(drive, to))
        } else if self.stub_of(drive, from).exists() {
            (self.stub_of(drive, from), self.stub_of(drive, to))
        } else {
            bail!("{from} does not exist locally");
        };
        if dst.exists() || self.path_of(drive, to).exists() || self.stub_of(drive, to).exists() {
            bail!("{to} already exists locally; not overwriting it");
        }
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(src, dst)?;
        Ok(())
    }

    fn copy_local(&self, drive: &Drive, from: &RelPath, to: &RelPath) -> Result<()> {
        let dst = self.path_of(drive, to);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(self.path_of(drive, from), dst)?;
        Ok(())
    }

    fn create_dir(&self, drive: &Drive, path: &RelPath) -> Result<()> {
        std::fs::create_dir_all(self.path_of(drive, path))?;
        Ok(())
    }

    fn scan(&self, drive: &Drive) -> Result<Vec<LocalEntry>> {
        let mut out = Vec::new();
        self.walk(drive, &self.drive_dir(drive), &RelPath::root(), &mut out)?;
        Ok(out)
    }

    fn is_populated(&self, _drive: &Drive, _dir: &RelPath) -> bool {
        true
    }

    fn name_rules(&self) -> NameRules {
        self.rules
    }

    fn take_events(&self) -> Option<UnboundedReceiver<ProviderEvent>> {
        self.rx.lock().unwrap().take()
    }
}
