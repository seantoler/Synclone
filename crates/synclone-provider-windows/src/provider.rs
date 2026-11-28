//! The `Provider` implementation for the Cloud Files API.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder::{ConvertOptions, UpdateOptions};
use cloud_filter::placeholder::{PinOptions, PinState, Placeholder};
use cloud_filter::placeholder_create_info::PlaceholderCreateInfo;
use cloud_filter::root::{
    Connection, HydrationType, PopulationType, ProtectionMode, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
    SyncRootInfo,
};
use synclone_core::db::Db;
use synclone_core::provider::{DeletedCopy, LocalEntry, NameRules, Provider, ProviderEvent};
use synclone_core::*;
use synclone_rclone::{Endpoint, RcClient};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::filter::Filter;
use crate::paths::{self, Blob, Place};
use crate::win;

/// Settings for [`WindowsProvider::start`].
#[derive(Debug, Clone)]
pub struct WindowsProviderConfig {
    /// The app folder: one subfolder per cloud drive.
    pub sync_root: PathBuf,
    /// Distinguishes sync roots of one user (tests use a unique one each).
    pub account: String,
    /// Name in Explorer's navigation pane.
    pub display_name: String,
    /// `path,index` of the icon Explorer shows for the root.
    pub icon: String,
    /// How long a folder listing may wait for the cloud when the user opens
    /// a folder that was never indexed.
    pub list_wait: Duration,
}

impl WindowsProviderConfig {
    pub fn new(sync_root: impl Into<PathBuf>) -> Self {
        let icon = std::env::current_exe()
            .map(|p| format!("{},0", p.display()))
            .unwrap_or_else(|_| r"%SystemRoot%\system32\imageres.dll,197".into());
        Self {
            sync_root: sync_root.into(),
            account: "default".into(),
            display_name: APP_NAME.into(),
            icon,
            list_wait: Duration::from_secs(20),
        }
    }
}

/// Last pin attribute seen on a path, to report only changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PinMark {
    #[default]
    None,
    Pinned,
    Unpinned,
}

impl PinMark {
    pub fn of(a: win::Attr) -> Self {
        if a.pinned() {
            PinMark::Pinned
        } else if a.unpinned() {
            PinMark::Unpinned
        } else {
            PinMark::None
        }
    }
}

/// State shared by the provider and its callbacks.
pub(crate) struct Inner {
    pub root: PathBuf,
    pub db: Arc<Db>,
    /// A client and runtime of the provider's own, so hydration never
    /// waits for a free engine worker (the engine itself can block one in
    /// `CfHydratePlaceholder`).
    pub rc: RcClient,
    pub rt: tokio::runtime::Handle,
    pub rt_owner: Mutex<Option<tokio::runtime::Runtime>>,
    pub tx: UnboundedSender<ProviderEvent>,
    /// Engine downloads waiting to be handed to Windows: placeholder path
    /// key → staged file (served by the FETCH_DATA callback).
    pub staged: Mutex<HashMap<String, PathBuf>>,
    /// Drives being removed: their callbacks are answered without events.
    pub unregistering: Mutex<HashSet<DriveId>>,
    /// Hydrations in progress by file ID, set when Windows cancels one.
    pub cancels: Mutex<HashMap<i64, Arc<AtomicBool>>>,
    pub pins: Mutex<HashMap<String, PinMark>>,
    pub list_wait: Duration,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(rt) = self.rt_owner.lock().unwrap().take() {
            // Safe to call from async code, unlike dropping a runtime.
            rt.shutdown_background();
        }
    }
}

impl Inner {
    pub fn emit(&self, ev: ProviderEvent) {
        let _ = self.tx.send(ev);
    }

    /// The connected drive whose folder is called `name`.
    pub fn drive_by_folder(&self, name: &str) -> Option<Drive> {
        self.db
            .drives()
            .ok()?
            .into_iter()
            .find(|d| d.state == DriveState::Connected && d.display_name.eq_ignore_ascii_case(name))
    }

    /// The drive and path inside it for a path on disk.
    pub fn locate(&self, path: &Path) -> Option<(Drive, RelPath)> {
        match paths::place(&self.root, path) {
            Place::TopLevel(f) => Some((self.drive_by_folder(&f)?, RelPath::root())),
            Place::Inside { folder, rel } => Some((self.drive_by_folder(&folder)?, rel)),
            _ => None,
        }
    }

    pub fn path_of(&self, d: &Drive, rel: &RelPath) -> PathBuf {
        paths::join(&self.root, &d.display_name, rel)
    }

    pub fn is_unregistering(&self, d: DriveId) -> bool {
        self.unregistering.lock().unwrap().contains(&d)
    }

    pub fn blob(&self, d: &Drive, size: i64, mtime_ns: i64) -> Vec<u8> {
        Blob {
            d: d.id,
            s: size,
            m: mtime_ns,
        }
        .to_bytes()
    }

    /// The create-info for one placeholder (file or on-demand folder).
    pub fn create_info(&self, d: &Drive, meta: &ItemMeta) -> PlaceholderCreateInfo {
        let mtime = meta.mtime_ns.unwrap_or_else(|| db::now_ms() * 1_000_000);
        let t = paths::file_time(mtime);
        let md = if meta.is_dir {
            Metadata::directory()
        } else {
            Metadata::file().size(meta.size.max(0) as u64)
        }
        .created(t)
        .written(t)
        .changed(t)
        .accessed(t);
        PlaceholderCreateInfo::new(meta.path.name())
            .metadata(md)
            .mark_in_sync()
            .blob(self.blob(d, meta.size, mtime))
    }

    /// Create one placeholder inside the existing folder `parent`.
    /// Something already there is left alone.
    pub fn create_placeholder(&self, d: &Drive, parent: &Path, meta: &ItemMeta) -> Result<()> {
        if parent.join(meta.path.name()).exists() {
            return Ok(());
        }
        match self.create_info(d, meta).create(parent) {
            Ok(_) => Ok(()),
            // Raced with Windows filling the folder: fine.
            Err(_) if parent.join(meta.path.name()).exists() => Ok(()),
            Err(e) => Err(anyhow!("creating the placeholder for {}: {e}", meta.path)),
        }
    }

    /// Make sure the folder `rel` exists on disk, creating any missing
    /// folders on the way as on-demand placeholders.
    pub fn ensure_dir(&self, d: &Drive, rel: &RelPath) -> Result<()> {
        let mut chain = rel.ancestors();
        chain.reverse();
        chain.push(rel.clone());
        for p in chain {
            let path = self.path_of(d, &p);
            if path.is_dir() {
                continue;
            }
            if p.is_root() {
                self.ensure_drive_folder(d)?;
                continue;
            }
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
            let mtime = self.db.item(d.id, &p)?.and_then(|i| i.remote.and_then(|r| r.mtime_ns));
            self.create_placeholder(
                d,
                parent,
                &ItemMeta {
                    path: p.clone(),
                    is_dir: true,
                    size: 0,
                    mtime_ns: mtime,
                },
            )?;
        }
        Ok(())
    }

    /// The drive's top-level folder, as an on-demand placeholder folder so
    /// Windows asks for its contents and reports its deletion (D-022).
    pub fn ensure_drive_folder(&self, d: &Drive) -> Result<()> {
        let path = self.path_of(d, &RelPath::root());
        if path.is_dir() {
            if !win::attributes(&path).map(win::Attr::reparse).unwrap_or(false) {
                // An ordinary folder (e.g. made before the provider ran): adopt it.
                let mut ph = win32_placeholder(&path, false)?;
                ph.convert_to_placeholder(ConvertOptions::default().mark_in_sync().blob(self.blob(d, 0, 0)), None)?;
            }
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)?;
        self.create_placeholder(
            d,
            &self.root,
            &ItemMeta {
                path: RelPath::new(&d.display_name).unwrap_or_default(),
                is_dir: true,
                size: 0,
                mtime_ns: Some(d.added_at * 1_000_000),
            },
        )
    }

    /// Set the pin state, remembering it so the change Windows reports back
    /// is not taken for the user's.
    pub fn set_pin(&self, path: &Path, state: PinState) -> Result<()> {
        let mark = match state {
            PinState::Pinned => PinMark::Pinned,
            PinState::Unpinned => PinMark::Unpinned,
            _ => PinMark::None,
        };
        self.pins.lock().unwrap().insert(paths::key(path), mark);
        let mut ph = Placeholder::open(path).with_context(|| format!("opening {}", path.display()))?;
        ph.mark_pin(state, PinOptions::default())
            .with_context(|| format!("setting the pin state of {}", path.display()))?;
        Ok(())
    }

    pub fn is_populated_path(&self, path: &Path) -> bool {
        match win::attributes(path) {
            None => false,
            Some(a) if !a.reparse() => true,
            Some(_) => match Placeholder::open(path).and_then(|p| p.state()) {
                Ok(Some(st)) => !st.partial(),
                Ok(None) => true,
                Err(_) => false,
            },
        }
    }
}

/// The Cloud Files API provider. Dropping it disconnects from Windows
/// (Explorer then shows the folder as offline); the registration stays.
pub struct WindowsProvider {
    pub(crate) inner: Arc<Inner>,
    rx: Mutex<Option<UnboundedReceiver<ProviderEvent>>>,
    conn: Mutex<Option<Connection<Filter>>>,
    id: SyncRootId,
}

fn root_id(account: &str) -> Result<SyncRootId> {
    Ok(SyncRootIdBuilder::new(APP_NAME)
        .user_security_id(SecurityId::current_user()?)
        .account_name(account)
        .build())
}

impl WindowsProvider {
    /// Register the sync root (once per user and account) and connect.
    /// `rclone` is the running `rclone rcd` (hydration streams from its
    /// `--rc-serve`).
    pub fn start(cfg: WindowsProviderConfig, db: Arc<Db>, rclone: Endpoint) -> Result<Self> {
        if !cloud_filter::root::is_supported().unwrap_or(false) {
            bail!("this version of Windows does not support cloud files (the Cloud Files API)");
        }
        std::fs::create_dir_all(&cfg.sync_root)?;
        let root = std::path::absolute(&cfg.sync_root)?;
        let id = root_id(&cfg.account)?;
        if id.is_registered()? {
            let at = id.info().map(|i| i.path()).ok();
            if at.as_deref().map(paths::key) != Some(paths::key(&root)) {
                id.unregister().context("moving the Synclone folder")?;
            }
        }
        // Registration goes through WinRT, which can fail transiently
        // (seen with several registrations at once): retry briefly.
        let mut attempt = 0;
        loop {
            let info = SyncRootInfo::default()
                .with_display_name(&cfg.display_name)
                .with_icon(&cfg.icon)
                .with_version(VERSION)
                .with_hydration_type(HydrationType::Full)
                .with_population_type(PopulationType::Full)
                .with_protection_mode(ProtectionMode::Personal)
                .with_allow_pinning(true)
                .with_show_siblings_as_group(false)
                .with_path(&root)?;
            match id.register(info) {
                Ok(()) => break,
                Err(e) if attempt < 4 => {
                    attempt += 1;
                    tracing::info!("registering the Synclone folder (attempt {attempt}): {e}");
                    std::thread::sleep(Duration::from_millis(300 * attempt));
                }
                Err(e) => return Err(anyhow!(e).context("registering the Synclone folder with Windows")),
            }
        }
        let (tx, rx) = unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("synclone-hydrate")
            .enable_all()
            .build()?;
        let inner = Arc::new(Inner {
            root: root.clone(),
            db,
            rc: RcClient::new(rclone),
            rt: runtime.handle().clone(),
            rt_owner: Mutex::new(Some(runtime)),
            tx,
            staged: Default::default(),
            unregistering: Default::default(),
            cancels: Default::default(),
            pins: Default::default(),
            list_wait: cfg.list_wait,
        });
        let conn = Session::new()
            .connect(&root, Filter { inner: inner.clone() })
            .context("connecting to the Synclone folder")?;
        Ok(Self {
            inner,
            rx: Mutex::new(Some(rx)),
            conn: Mutex::new(Some(conn)),
            id,
        })
    }

    pub fn sync_root(&self) -> &Path {
        &self.inner.root
    }

    /// Disconnect and remove the registration (uninstall, tests). Files on
    /// disk are left as they are.
    pub fn unregister(&self) -> Result<()> {
        self.conn.lock().unwrap().take();
        self.id.unregister().context("unregistering the Synclone folder")?;
        Ok(())
    }

    /// Remove a sync root registration left behind (tests, uninstall).
    pub fn unregister_account(account: &str) -> Result<()> {
        let id = root_id(account)?;
        if id.is_registered()? {
            id.unregister()?;
        }
        Ok(())
    }

    fn path_of(&self, d: &Drive, p: &RelPath) -> PathBuf {
        self.inner.path_of(d, p)
    }

    /// Recycle a downloaded file, delete a placeholder; folders: both,
    /// throughout (§5 rule 6: downloaded content goes to the Recycle Bin).
    fn remove_tree(&self, path: &Path) -> Result<()> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(path) {
                for e in rd.flatten() {
                    self.remove_tree(&e.path())?;
                }
            }
            std::fs::remove_dir(path).with_context(|| format!("removing {}", path.display()))?;
            return Ok(());
        }
        if win::is_dehydrated(path) {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            return Ok(());
        }
        if win::attributes(path).map(win::Attr::reparse).unwrap_or(false) {
            win::revert_placeholder(path)?;
        }
        win::recycle(path)
    }
}

impl Drop for WindowsProvider {
    fn drop(&mut self) {
        self.conn.lock().unwrap().take();
    }
}

impl Provider for WindowsProvider {
    fn register_drive(&self, drive: &Drive) -> Result<()> {
        self.inner.unregistering.lock().unwrap().remove(&drive.id);
        self.inner.ensure_drive_folder(drive)
    }

    fn unregister_drive(&self, drive: &Drive) -> Result<()> {
        let dir = self.path_of(drive, &RelPath::root());
        if paths::place(&self.inner.root, &dir) != Place::TopLevel(drive.display_name.clone()) {
            bail!("refusing to remove {} outside the Synclone folder", dir.display());
        }
        self.inner.unregistering.lock().unwrap().insert(drive.id);
        // Our own deletes raise no Cloud Files callbacks (spike, D-021), and
        // unregistering marks the drive so any stray one is ignored. Nothing
        // here can reach rclone.
        let r = if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
        } else {
            Ok(())
        };
        r
    }

    fn upsert_placeholders(&self, drive: &Drive, parent: &RelPath, items: &[ItemMeta]) -> Result<()> {
        self.inner.ensure_dir(drive, parent)?;
        let pdir = self.path_of(drive, parent);
        let mut first_err = None;
        for it in items {
            let path = self.path_of(drive, &it.path);
            let r = match win::attributes(&path) {
                None => self.inner.create_placeholder(drive, &pdir, it),
                // Rule 1: a placeholder takes the cloud's new size and time.
                Some(a) if !it.is_dir && a.dehydrated() && a.reparse() => {
                    update_dehydrated(&self.inner, drive, &path, it)
                }
                Some(_) => Ok(()),
            };
            if let Err(e) = r {
                tracing::warn!("{e:#}");
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn remove_local(&self, drive: &Drive, path: &RelPath) -> Result<()> {
        if path.is_root() {
            bail!("refusing to remove a whole drive folder this way");
        }
        self.remove_tree(&self.path_of(drive, path))
    }

    fn set_state(&self, drive: &Drive, path: &RelPath, state: LocalState) -> Result<()> {
        let full = self.path_of(drive, path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            return Ok(());
        };
        if meta.is_dir() {
            return Ok(());
        }
        let attr = win::Attr(std::os::windows::fs::MetadataExt::file_attributes(&meta));
        let fp = paths::fingerprint(&meta);
        match state {
            LocalState::Placeholder => {
                if attr.dehydrated() {
                    return Ok(());
                }
                if !attr.reparse() {
                    let mut ph = win32_placeholder(&full, true)?;
                    ph.convert_to_placeholder(
                        ConvertOptions::default()
                            .mark_in_sync()
                            .dehydrate()
                            .blob(self.inner.blob(drive, fp.size, fp.mtime_ns)),
                        None,
                    )
                    .with_context(|| format!("freeing {path}"))?;
                    return Ok(());
                }
                self.inner.set_pin(&full, PinState::Unspecified)?;
                let mut ph = Placeholder::options().write_access().exclusive().open(&full)?;
                // Only content Windows also considers in sync is dropped: a
                // second barrier behind the engine's own dirty check.
                ph.update(UpdateOptions::default().update_if_in_sync().dehydrate(), None)
                    .with_context(|| format!("freeing {path}"))?;
                Ok(())
            }
            LocalState::Downloaded => {
                // Downloads stay until the user frees them (§1.1, §3.1).
                // Pinning needs no write access, so it works while the
                // program that opened the file still has it.
                if attr.reparse() && !attr.pinned() {
                    self.inner.set_pin(&full, PinState::Pinned)?;
                }
                // Mark in sync only if the file is still the version the
                // engine recorded (a write during an upload stays pending).
                let clean = self
                    .inner
                    .db
                    .item(drive.id, path)?
                    .map(|i| i.local_base == Some(fp) && i.synced.is_some())
                    .unwrap_or(false);
                let in_sync = attr.reparse()
                    && Placeholder::open(&full)
                        .ok()
                        .and_then(|p| p.info().ok().flatten())
                        .map(|i| i.is_in_sync())
                        .unwrap_or(false);
                if in_sync || (!clean && attr.reparse()) {
                    return Ok(());
                }
                tracing::debug!(
                    "set_state downloaded {path}: clean={clean} in_sync={in_sync} attr={:#x}",
                    attr.0
                );
                let inner = self.inner.clone();
                let (d, p) = (drive.clone(), path.clone());
                let attempt = move || mark_synced(&inner, &d, &p, &full, fp, clean, !attr.reparse());
                if let Err(e) = attempt() {
                    if !is_sharing_violation(&e) {
                        return Err(e);
                    }
                    tracing::debug!("{path} is busy, retrying: {e:#}");
                    // Still open in another program: try again shortly.
                    std::thread::spawn(move || {
                        for _ in 0..60 {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            match attempt() {
                                Err(e) if is_sharing_violation(&e) => continue,
                                Err(e) => {
                                    tracing::warn!("{e:#}");
                                    return;
                                }
                                Ok(()) => return,
                            }
                        }
                    });
                }
                Ok(())
            }
            LocalState::Trashed => Ok(()),
        }
    }

    fn is_open(&self, drive: &Drive, path: &RelPath) -> Result<bool> {
        win::is_open_for_writing(&self.path_of(drive, path))
    }

    fn local_status(&self, drive: &Drive, path: &RelPath) -> Result<LocalStatus> {
        let full = self.path_of(drive, path);
        Ok(match std::fs::symlink_metadata(&full) {
            Ok(m) if m.is_dir() => LocalStatus::Dir,
            Ok(m) => {
                let a = win::Attr(std::os::windows::fs::MetadataExt::file_attributes(&m));
                if a.dehydrated() {
                    LocalStatus::Placeholder
                } else {
                    LocalStatus::File(paths::fingerprint(&m))
                }
            }
            Err(_) => LocalStatus::Absent,
        })
    }

    fn content_path(&self, drive: &Drive, path: &RelPath) -> PathBuf {
        self.path_of(drive, path)
    }

    fn commit_download(
        &self,
        drive: &Drive,
        path: &RelPath,
        staged: &Path,
        meta: &ItemMeta,
        expected: LocalStatus,
    ) -> Result<LocalFingerprint> {
        let check = |what: &str| -> Result<()> {
            let now = self.local_status(drive, path)?;
            let ok = match (expected, now) {
                (LocalStatus::File(a), LocalStatus::File(b)) => a == b,
                // A placeholder that vanished meanwhile was deleted by the
                // user: the download must not bring it back.
                (LocalStatus::Placeholder, LocalStatus::Placeholder) => true,
                (LocalStatus::Absent, LocalStatus::Placeholder | LocalStatus::Absent) => true,
                _ => false,
            };
            if !ok {
                bail!("{path} changed locally {what} ({now:?}, expected {expected:?})");
            }
            Ok(())
        };
        check("")?;
        let size = std::fs::metadata(staged)?.len();
        let mtime = meta.mtime_ns.unwrap_or_else(|| db::now_ms() * 1_000_000);
        let t = paths::file_time(mtime);
        let md = Metadata::file().size(size).created(t).written(t).changed(t).accessed(t);
        let blob = self.inner.blob(drive, size as i64, mtime);
        let full = self.path_of(drive, path);
        let parent = path.parent().unwrap_or_default();
        self.inner.ensure_dir(drive, &parent)?;

        // Step 1: a dehydrated placeholder describing the new version.
        match win::attributes(&full) {
            None => {
                self.inner.create_placeholder(
                    drive,
                    &self.path_of(drive, &parent),
                    &ItemMeta {
                        path: path.clone(),
                        is_dir: false,
                        size: size as i64,
                        mtime_ns: Some(mtime),
                    },
                )?;
            }
            Some(a) if a.dehydrated() => {
                let mut ph = win32_placeholder(&full, false)?;
                ph.update(UpdateOptions::default().metadata(md).blob(&blob).mark_in_sync(), None)?;
            }
            Some(a) => {
                // A downloaded copy (clean, or already saved as a conflict
                // copy by the engine) is replaced: drop its content.
                if a.pinned() || a.unpinned() {
                    self.inner.set_pin(&full, PinState::Unspecified)?;
                }
                check("before replacing it")?;
                // Exclusive: fails, rather than dropping content, while any
                // other program has the file open.
                let mut ph = win32_placeholder(&full, true)?;
                if !a.reparse() {
                    ph.convert_to_placeholder(ConvertOptions::default().mark_in_sync().dehydrate(), None)?;
                }
                ph.update(
                    UpdateOptions::default()
                        .metadata(md)
                        .blob(&blob)
                        .mark_in_sync()
                        .dehydrate(),
                    None,
                )
                .with_context(|| format!("replacing {path}"))?;
            }
        }

        // Step 2: fill it from the staged file (our FETCH_DATA callback
        // serves it), so Windows does the write.
        let key = paths::key(&full);
        self.inner
            .staged
            .lock()
            .unwrap()
            .insert(key.clone(), staged.to_path_buf());
        let r = Placeholder::open(&full).and_then(|mut ph| ph.hydrate(..));
        self.inner.staged.lock().unwrap().remove(&key);
        r.with_context(|| format!("writing the downloaded {path}"))?;
        self.inner.set_pin(&full, PinState::Pinned)?;
        let m = std::fs::symlink_metadata(&full)?;
        Ok(paths::fingerprint(&m))
    }

    fn rename_local(&self, drive: &Drive, from: &RelPath, to: &RelPath) -> Result<()> {
        let (src, dst) = (self.path_of(drive, from), self.path_of(drive, to));
        if std::fs::symlink_metadata(&src).is_err() {
            bail!("{from} does not exist locally");
        }
        if std::fs::symlink_metadata(&dst).is_ok() && paths::key(&src) != paths::key(&dst) {
            bail!("{to} already exists locally; not overwriting it");
        }
        self.inner.ensure_dir(drive, &to.parent().unwrap_or_default())?;
        win::move_no_replace(&src, &dst)
    }

    fn copy_local(&self, drive: &Drive, from: &RelPath, to: &RelPath) -> Result<()> {
        let dst = self.path_of(drive, to);
        self.inner.ensure_dir(drive, &to.parent().unwrap_or_default())?;
        if dst.exists() {
            bail!("{to} already exists locally");
        }
        std::fs::copy(self.path_of(drive, from), dst)?;
        Ok(())
    }

    fn create_dir(&self, drive: &Drive, path: &RelPath) -> Result<()> {
        self.inner.ensure_dir(drive, &path.parent().unwrap_or_default())?;
        std::fs::create_dir_all(self.path_of(drive, path))?;
        Ok(())
    }

    fn scan(&self, drive: &Drive) -> Result<Vec<LocalEntry>> {
        // Enumerating from this process never makes Windows fill a folder
        // (spike, D-021): only what is on disk is listed.
        fn walk(p: &WindowsProvider, d: &Drive, dir: &Path, rel: &RelPath, out: &mut Vec<LocalEntry>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let rp = rel.join(&name);
                let st = p.local_status(d, &rp).unwrap_or(LocalStatus::Absent);
                out.push(LocalEntry {
                    path: rp.clone(),
                    status: st,
                });
                if st == LocalStatus::Dir {
                    walk(p, d, &e.path(), &rp, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(
            self,
            drive,
            &self.path_of(drive, &RelPath::root()),
            &RelPath::root(),
            &mut out,
        );
        Ok(out)
    }

    fn is_populated(&self, drive: &Drive, dir: &RelPath) -> bool {
        self.inner.is_populated_path(&self.path_of(drive, dir))
    }

    fn name_rules(&self) -> NameRules {
        NameRules::WINDOWS
    }

    fn take_events(&self) -> Option<UnboundedReceiver<ProviderEvent>> {
        self.rx.lock().unwrap().take()
    }

    fn open_folders(&self, drive: &Drive) -> Vec<RelPath> {
        let top = self.path_of(drive, &RelPath::root());
        crate::explorer::open_folders()
            .into_iter()
            .filter(|p| crate::recycle_bin::is_under(p, &top))
            .filter_map(|p| {
                let s = p.display().to_string();
                let rel = s
                    .get(top.display().to_string().len()..)?
                    .trim_start_matches('\\')
                    .replace('\\', "/");
                if rel.is_empty() {
                    Some(RelPath::root())
                } else {
                    RelPath::new(&rel)
                }
            })
            .collect()
    }

    fn find_deleted_copy(&self, d: &Drive, p: &RelPath, fp: &LocalFingerprint) -> Result<Option<DeletedCopy>> {
        let original = self.path_of(d, p);
        for e in crate::recycle_bin::find(&original)? {
            let Ok(m) = std::fs::metadata(&e.content) else { continue };
            if !m.is_file() {
                continue;
            }
            let here = paths::fingerprint(&m);
            // NTFS keeps 100 ns; the fingerprint on record came from NTFS too.
            if here.size == fp.size && paths::ntfs_ns(here.mtime_ns) == paths::ntfs_ns(fp.mtime_ns) {
                return Ok(Some(DeletedCopy {
                    content: e.content,
                    record: Some(e.info),
                }));
            }
        }
        Ok(None)
    }

    fn take_deleted_copy(&self, copy: &DeletedCopy, to: &Path) -> Result<()> {
        let e = crate::recycle_bin::BinEntry {
            original: PathBuf::new(),
            size: 0,
            deleted: 0,
            info: copy.record.clone().ok_or_else(|| anyhow!("no Recycle Bin record"))?,
            content: copy.content.clone(),
        };
        crate::recycle_bin::take(&e, to)
    }
}

/// A Win32 handle for placeholder calls (D-021). `CfConvertToPlaceholder`
/// and metadata updates reject the opaque handles `CfOpenFileWithOplock`
/// returns, so:
/// - `exclusive`: read/write with no sharing, for anything that drops the
///   content; it fails while any other program has the file open;
/// - otherwise attribute-only, for identity, metadata and in-sync changes;
///   it never hydrates and never conflicts with other programs.
fn win32_placeholder(path: &Path, exclusive: bool) -> Result<Placeholder> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL};
    let mut o = std::fs::OpenOptions::new();
    if exclusive {
        o.read(true).write(true).share_mode(0);
    } else {
        o.access_mode(0).share_mode(7);
    }
    let f = o
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_NO_RECALL.0)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(Placeholder::from(f))
}

fn is_sharing_violation(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<windows::core::Error>()
            .map(|w| w.code() == windows::Win32::Foundation::ERROR_SHARING_VIOLATION.to_hresult())
            .unwrap_or(false)
            || c.downcast_ref::<std::io::Error>()
                .and_then(|io| io.raw_os_error())
                .map(|c| c == windows::Win32::Foundation::ERROR_SHARING_VIOLATION.0 as i32)
                .unwrap_or(false)
    })
}

/// Make the file a placeholder (if it is not yet) that Windows shows as in
/// sync, and pin it, if it still is the version `fp` the engine recorded.
fn mark_synced(
    inner: &Inner,
    d: &Drive,
    path: &RelPath,
    full: &Path,
    fp: LocalFingerprint,
    clean: bool,
    convert: bool,
) -> Result<()> {
    let now = std::fs::symlink_metadata(full).map(|m| paths::fingerprint(&m)).ok();
    if now != Some(fp) {
        tracing::debug!("{path} changed before it could be marked synced ({now:?} vs {fp:?})");
        return Ok(()); // Changed meanwhile: the engine will see it.
    }
    let blob = inner.blob(d, fp.size, fp.mtime_ns);
    let mut ph = win32_placeholder(full, false)?;
    if convert {
        let mut opts = ConvertOptions::default().blob(blob);
        if clean {
            opts = opts.mark_in_sync();
        }
        ph.convert_to_placeholder(opts, None)
            .with_context(|| format!("marking {path} as synced"))?;
        drop(ph);
        inner.set_pin(full, PinState::Pinned)?;
        tracing::debug!("{path} converted and pinned: {:?}", win::attributes(full));
    } else {
        ph.update(UpdateOptions::default().blob(&blob).mark_in_sync(), None)
            .with_context(|| format!("marking {path} as synced"))?;
    }
    Ok(())
}

/// Rule 1 for a dehydrated placeholder: new size and time from the cloud.
fn update_dehydrated(inner: &Inner, d: &Drive, path: &Path, it: &ItemMeta) -> Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    let fp = paths::fingerprint(&m);
    let mtime = it.mtime_ns.unwrap_or(fp.mtime_ns);
    if fp.size == it.size.max(0) && fp.mtime_ns == paths::ntfs_ns(mtime) {
        return Ok(());
    }
    let t = paths::file_time(mtime);
    let md = Metadata::file()
        .size(it.size.max(0) as u64)
        .created(t)
        .written(t)
        .changed(t)
        .accessed(t);
    let blob = inner.blob(d, it.size, mtime);
    let mut ph = win32_placeholder(path, false)?;
    ph.update(UpdateOptions::default().metadata(md).blob(&blob).mark_in_sync(), None)
        .with_context(|| format!("updating the placeholder for {}", it.path))?;
    Ok(())
}

/// Shared with the callbacks.
pub(crate) type SharedInner = Arc<Inner>;
