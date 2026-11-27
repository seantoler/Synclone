//! Cloud Files callbacks: hydration, folder population, and the user's
//! deletes, renames and pin changes turned into engine events.
//!
//! Windows calls these on its own threads and blocks the program that
//! triggered them until they return, so they never wait on the engine for
//! long, and never on anything that could be waiting for them.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{info, ticket, Request, StateWatchEvent, SyncFilter};
use cloud_filter::placeholder_create_info::PlaceholderCreateInfo;
use cloud_filter::utility::WriteAt;
use synclone_core::provider::ProviderEvent;
use synclone_core::*;

use crate::paths::{self, Place};
use crate::provider::{PinMark, SharedInner};
use crate::win;

/// Bytes per transfer to Windows (a multiple of 4 KiB, as CfExecute needs).
const CHUNK: usize = 1 << 20;

pub(crate) struct Filter {
    pub inner: SharedInner,
}

impl Filter {
    /// Serve `size` bytes into the placeholder from `src`.
    fn transfer(
        &self,
        ticket: &ticket::FetchData,
        size: u64,
        cancel: &AtomicBool,
        mut next: impl FnMut() -> Result<Option<Vec<u8>>>,
    ) -> Result<()> {
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK * 2);
        let mut done: u64 = 0;
        let mut eof = false;
        while !eof {
            match next()? {
                Some(b) => buf.extend_from_slice(&b),
                None => eof = true,
            }
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            while buf.len() >= CHUNK || (eof && !buf.is_empty()) {
                let n = if buf.len() >= CHUNK { CHUNK } else { buf.len() };
                if done + n as u64 > size {
                    bail!("the cloud sent more than the expected {size} bytes");
                }
                ticket
                    .write_at(&buf[..n], done)
                    .map_err(|e| anyhow!("handing data to Windows: {e}"))?;
                done += n as u64;
                buf.drain(..n);
                // Explorer's progress bar; also keeps the request alive.
                let _ = ticket.report_progress(size, done);
            }
        }
        if done != size {
            bail!("got {done} of {size} bytes");
        }
        Ok(())
    }

    fn serve_staged(&self, ticket: &ticket::FetchData, staged: &Path, size: u64, cancel: &AtomicBool) -> Result<()> {
        let mut f = std::fs::File::open(staged)?;
        let mut chunk = vec![0u8; CHUNK];
        self.transfer(ticket, size, cancel, move || {
            let n = f.read(&mut chunk)?;
            Ok((n > 0).then(|| chunk[..n].to_vec()))
        })
    }

    fn serve_remote(
        &self,
        ticket: &ticket::FetchData,
        d: &Drive,
        rel: &RelPath,
        size: u64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let inner = &self.inner;
        let mut reader = inner
            .rt
            .block_on(inner.rc.open_read(&d.fs(), &d.remote_path(rel), 0))
            .map_err(|e| anyhow!("{e}"))?;
        if reader.total != size {
            // The cloud has another version: rule 1 updates the placeholder.
            inner.emit(ProviderEvent::LocalModified {
                drive: d.id,
                path: rel.clone(),
            });
            bail!(
                "{rel} changed in the cloud ({} bytes now, {size} expected); try again in a moment",
                reader.total
            );
        }
        let rt = inner.rt.clone();
        self.transfer(ticket, size, cancel, move || {
            rt.block_on(reader.chunk())
                .map(|c| c.map(|b| b.to_vec()))
                .map_err(|e| anyhow!("{e}"))
        })
    }

    /// After Windows filled a placeholder by itself, tell the engine once
    /// the file reads as hydrated.
    fn report_hydrated(&self, path: PathBuf, d: DriveId, rel: RelPath) {
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(10);
            while win::is_dehydrated(&path) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(25));
            }
            if let Ok(m) = std::fs::symlink_metadata(&path) {
                let fp = paths::fingerprint(&m);
                inner.emit(ProviderEvent::Hydrated {
                    drive: d,
                    path: rel,
                    size: fp.size,
                    mtime_ns: fp.mtime_ns,
                });
            }
        });
    }

    /// A drive folder may have gone: check after the OS has finished (a
    /// Recycle Bin delete is delete, undelete, delete) and report it.
    fn check_drive_folder_later(&self, path: PathBuf, d: DriveId) {
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            if !path.exists() && !inner.is_unregistering(d) {
                inner.emit(ProviderEvent::DriveRootRemoved { drive: d });
            }
        });
    }

    fn children_infos(&self, d: &Drive, rel: &RelPath, dir: &Path) -> Vec<PlaceholderCreateInfo> {
        let kids = self.inner.db.children(d.id, rel).unwrap_or_default();
        kids.into_iter()
            .filter(|c| c.local_state != LocalState::Trashed && c.unsyncable.is_none())
            .filter(|c| !dir.join(c.path.name()).exists())
            .map(|c| self.inner.create_info(d, &c.meta()))
            .collect()
    }
}

fn kind(e: &anyhow::Error) -> CloudErrorKind {
    let s = format!("{e:#}");
    if s.contains("cancelled") {
        CloudErrorKind::RequestCancelled
    } else if s.contains("changed in the cloud") {
        CloudErrorKind::NotInSync
    } else if s.contains("not found") || s.contains("404") {
        CloudErrorKind::Unsuccessful
    } else {
        CloudErrorKind::NetworkUnavailable
    }
}

impl SyncFilter for Filter {
    fn fetch_data(&self, req: Request, ticket: ticket::FetchData, _info: info::FetchData) -> CResult<()> {
        let path = req.path();
        let size = req.file_size();
        let key = paths::key(&path);
        let staged = self.inner.staged.lock().unwrap().get(&key).cloned();
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner.cancels.lock().unwrap().insert(req.file_id(), cancel.clone());
        let located = self.inner.locate(&path);
        let r = match (&staged, &located) {
            (Some(s), _) => self.serve_staged(&ticket, s, size, &cancel),
            (None, Some((d, rel))) => self.serve_remote(&ticket, d, rel, size, &cancel),
            (None, None) => Err(anyhow!("{} is not in a connected cloud drive", path.display())),
        };
        self.inner.cancels.lock().unwrap().remove(&req.file_id());
        match r {
            Ok(()) => {
                if let (None, Some((d, rel))) = (staged, located) {
                    self.report_hydrated(path, d.id, rel);
                }
                Ok(())
            }
            Err(e) => {
                tracing::warn!("could not download {}: {e:#}", path.display());
                Err(kind(&e))
            }
        }
    }

    fn cancel_fetch_data(&self, req: Request, _info: info::CancelFetchData) {
        if let Some(f) = self.inner.cancels.lock().unwrap().get(&req.file_id()) {
            f.store(true, Ordering::Relaxed);
        }
    }

    fn fetch_placeholders(
        &self,
        req: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        let inner = &self.inner;
        let path = req.path();
        match paths::place(&inner.root, &path) {
            Place::Root => {
                // The app folder: one folder per connected drive.
                let mut infos: Vec<PlaceholderCreateInfo> = inner
                    .db
                    .drives()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|d| d.state == DriveState::Connected && !inner.root.join(&d.display_name).exists())
                    .map(|d| {
                        let meta = ItemMeta {
                            path: RelPath::new(&d.display_name).unwrap_or_default(),
                            is_dir: true,
                            size: 0,
                            mtime_ns: Some(d.added_at * 1_000_000),
                        };
                        inner.create_info(&d, &meta)
                    })
                    .collect();
                if let Err(e) = ticket.pass_with_placeholders(&mut infos) {
                    tracing::warn!("listing the Synclone folder: {e}");
                }
                Ok(())
            }
            Place::TopLevel(_) | Place::Inside { .. } => {
                let Some((d, rel)) = inner.locate(&path) else {
                    return Ok(());
                };
                if inner.is_unregistering(d.id) {
                    return Ok(());
                }
                inner.emit(ProviderEvent::FolderEnumerated {
                    drive: d.id,
                    path: rel.clone(),
                });
                // Never indexed: wait for the engine to list it (§4.2).
                let until = Instant::now() + inner.list_wait;
                while inner.db.listed_at(d.id, &rel).ok().flatten().is_none() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let mut infos = self.children_infos(&d, &rel, &path);
                if let Err(e) = ticket.pass_with_placeholders(&mut infos) {
                    // Usually a name created meanwhile; the rest went in.
                    tracing::info!("listing {}: {e}", path.display());
                }
                // Anything the engine indexed while this ran.
                let me = Filter { inner: inner.clone() };
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(300));
                    for c in me.inner.db.children(d.id, &rel).unwrap_or_default() {
                        if c.local_state == LocalState::Trashed || c.unsyncable.is_some() {
                            continue;
                        }
                        if let Err(e) = me.inner.create_placeholder(&d, &path, &c.meta()) {
                            tracing::debug!("{e:#}");
                        }
                    }
                });
                Ok(())
            }
            Place::Outside => Ok(()),
        }
    }

    fn delete(&self, _req: Request, ticket: ticket::Delete, _info: info::Delete) -> CResult<()> {
        // Allowed locally; the engine puts synced items in the trash queue
        // and nothing is sent to the cloud (§4.6).
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn deleted(&self, req: Request, _info: info::Deleted) {
        let path = req.path();
        match paths::place(&self.inner.root, &path) {
            Place::TopLevel(f) => {
                if let Some(d) = self.inner.drive_by_folder(&f) {
                    self.check_drive_folder_later(path, d.id);
                }
            }
            Place::Inside { .. } => {
                if let Some((d, rel)) = self.inner.locate(&path) {
                    if !self.inner.is_unregistering(d.id) {
                        self.inner.emit(ProviderEvent::LocalDeleted { drive: d.id, path: rel });
                    }
                }
            }
            _ => {}
        }
    }

    fn rename(&self, req: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        let inner = &self.inner;
        let src = req.path();
        let dst = info.target_path();
        let ok = match (paths::place(&inner.root, &src), paths::place(&inner.root, &dst)) {
            (Place::TopLevel(f), Place::TopLevel(g)) => match inner.drive_by_folder(&f) {
                // Renaming a drive folder renames the drive (§3.1); the new
                // name must be usable and not another drive's.
                Some(d) => {
                    synclone_core::engine::drives::sanitize_folder_name(&g) == g
                        && inner.drive_by_folder(&g).map(|o| o.id == d.id).unwrap_or(true)
                }
                None => true,
            },
            // Moving a drive folder out is like deleting it: a disconnect.
            (Place::TopLevel(_), Place::Outside) => true,
            (Place::TopLevel(f), _) => inner.drive_by_folder(&f).is_none(),
            (Place::Inside { folder: a, .. }, Place::Inside { folder: b, .. }) if a.eq_ignore_ascii_case(&b) => true,
            // Leaving its drive: only a downloaded file can (its content is
            // here); a placeholder or a folder would arrive empty.
            (Place::Inside { .. }, _) => {
                std::fs::symlink_metadata(&src).map(|m| m.is_file()).unwrap_or(false) && !win::is_dehydrated(&src)
            }
            _ => true,
        };
        if !ok {
            tracing::info!("refused moving {} to {}", src.display(), dst.display());
            return Err(CloudErrorKind::InvalidRequest);
        }
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn renamed(&self, req: Request, info: info::Renamed) {
        let inner = &self.inner;
        let to = req.path();
        let from = info.source_path();
        match (paths::place(&inner.root, &from), paths::place(&inner.root, &to)) {
            (Place::TopLevel(f), Place::TopLevel(g)) => {
                if let Some(d) = inner.drive_by_folder(&f) {
                    inner.emit(ProviderEvent::DriveRootRenamed {
                        drive: d.id,
                        new_name: g,
                    });
                }
            }
            (Place::TopLevel(f), _) => {
                if let Some(d) = inner.drive_by_folder(&f) {
                    self.check_drive_folder_later(from, d.id);
                }
            }
            (Place::Inside { .. }, _) | (_, Place::Inside { .. }) => {
                let a = inner.locate(&from);
                let b = inner.locate(&to);
                match (a, b) {
                    (Some((da, ra)), Some((db, rb))) if da.id == db.id => inner.emit(ProviderEvent::LocalRenamed {
                        drive: da.id,
                        from: ra,
                        to: rb,
                    }),
                    (a, b) => {
                        if let Some((d, r)) = a {
                            inner.emit(ProviderEvent::LocalDeleted { drive: d.id, path: r });
                        }
                        if let Some((d, r)) = b {
                            inner.emit(ProviderEvent::LocalCreated { drive: d.id, path: r });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn dehydrate(&self, req: Request, ticket: ticket::Dehydrate, _info: info::Dehydrate) -> CResult<()> {
        // Downloads stay until the user frees them: only a file the user
        // marked "Free up space" may lose its content.
        let path = req.path();
        if win::attributes(&path).map(|a| a.unpinned()).unwrap_or(false) {
            ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
        } else {
            Err(CloudErrorKind::DehydrationDisallowed)
        }
    }

    fn dehydrated(&self, req: Request, _info: info::Dehydrated) {
        if let Some((d, rel)) = self.inner.locate(&req.path()) {
            self.inner.emit(ProviderEvent::LocalModified { drive: d.id, path: rel });
        }
    }

    fn state_changed(&self, event: StateWatchEvent) {
        let inner = &self.inner;
        let paths = match event {
            StateWatchEvent::Changed(p) => p,
            StateWatchEvent::Stopped(e) => {
                tracing::error!("watching the Synclone folder stopped: {e}");
                return;
            }
        };
        for path in paths {
            match paths::place(&inner.root, &path) {
                Place::TopLevel(f) => {
                    if let Some(d) = inner.drive_by_folder(&f) {
                        if !path.exists() {
                            self.check_drive_folder_later(path, d.id);
                        }
                    }
                }
                Place::Inside { .. } => {
                    let Some((d, rel)) = inner.locate(&path) else { continue };
                    if inner.is_unregistering(d.id) {
                        continue;
                    }
                    let modified = ProviderEvent::LocalModified {
                        drive: d.id,
                        path: rel.clone(),
                    };
                    let Ok(meta) = std::fs::symlink_metadata(&path) else {
                        inner.emit(modified);
                        continue;
                    };
                    let a = win::Attr(std::os::windows::fs::MetadataExt::file_attributes(&meta));
                    let mark = PinMark::of(a);
                    let prev = inner
                        .pins
                        .lock()
                        .unwrap()
                        .insert(paths::key(&path), mark)
                        .unwrap_or_default();
                    let recursive = meta.is_dir();
                    let ev = match (prev == mark, mark) {
                        // "Always keep on this device" on a folder, or a file not here yet.
                        (false, PinMark::Pinned) if recursive || a.dehydrated() => Some(ProviderEvent::PinRequested {
                            drive: d.id,
                            path: rel,
                            recursive,
                        }),
                        // "Free up space".
                        (false, PinMark::Unpinned) => Some(ProviderEvent::FreeRequested {
                            drive: d.id,
                            path: rel,
                            recursive,
                        }),
                        _ => None,
                    };
                    inner.emit(ev.unwrap_or(modified));
                }
                _ => {}
            }
        }
    }
}
