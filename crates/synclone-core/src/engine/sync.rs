//! Reconciliation: for one path, compare the last-synced baseline, the
//! local file and the remote object, and act per the conflict table in
//! PLAN.md §5. Every decision is re-derived from current state, so a
//! reconcile is idempotent and safe to repeat after a crash.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use synclone_rclone::{ListOpts, RcloneError};

use super::index::RemoteChange;
use super::Engine;
use crate::compare::{remote_meta, VersionRules};
use crate::conflict::conflict_path;
use crate::db::Item;
use crate::types::*;

/// What a step decided besides success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Try again later without counting it as a failure (file open, ...).
    Defer {
        ms: i64,
        why: String,
    },
}

static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Why a reconcile was asked for: decides whether a placeholder is downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Keep the current local state (download only if pinned).
    Sync,
    /// The user opened the placeholder or asked to save it.
    Download,
}

impl Engine {
    pub(crate) fn rules(&self, d: &Drive) -> VersionRules {
        VersionRules::for_drive(d)
    }

    pub(crate) async fn stat_remote(&self, d: &Drive, p: &RelPath) -> Result<Option<RemoteMeta>> {
        let opts = ListOpts {
            hash_types: d.hash_type.iter().cloned().collect(),
            ..Default::default()
        };
        Ok(self
            .api
            .stat(&d.fs(), &d.remote_path(p), &opts)
            .await?
            .map(|e| remote_meta(&e, d.hash_type.as_deref())))
    }

    pub(crate) fn pinned(&self, d: DriveId, p: &RelPath) -> Result<bool> {
        if self.db.item(d, p)?.map(|i| i.pinned).unwrap_or(false) {
            return Ok(true);
        }
        for a in p.ancestors() {
            if self.db.item(d, &a)?.map(|i| i.pinned).unwrap_or(false) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn pending_move_involving(&self, d: DriveId, p: &RelPath) -> Result<bool> {
        for op in self.db.ops_for(d)? {
            if op.kind != "move" || op.state != "pending" {
                continue;
            }
            let from = op.payload.get("from").and_then(|f| f.as_str()).and_then(RelPath::new);
            if p.starts_with(&op.path) || from.map(|f| p.starts_with(&f)).unwrap_or(false) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ask for a reconcile of `p`, `delay_ms` from now (debounce).
    pub fn enqueue_reconcile(&self, d: DriveId, p: &RelPath, delay_ms: i64) -> Result<()> {
        self.db
            .enqueue_op("reconcile", d, p, &serde_json::json!({}), self.now() + delay_ms)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Hooks from the indexer
    // ------------------------------------------------------------------

    pub(crate) fn on_remote_change(
        &self,
        d: &Drive,
        p: &RelPath,
        change: &RemoteChange,
        parent_pinned: bool,
    ) -> Result<()> {
        let Some(item) = self.db.item(d.id, p)? else {
            return Ok(());
        };
        if let RemoteChange::Renamed { from } = change {
            // Rule 11: the user renamed it here too; the remote name wins.
            if let Some(op) = self
                .db
                .ops_for(d.id)?
                .into_iter()
                .find(|o| o.kind == "move" && o.payload.get("from").and_then(|f| f.as_str()) == Some(from.as_str()))
            {
                let local_to = op.path.clone();
                self.db.finish_op(op.id)?;
                self.relocate_local(d, &local_to, p)?;
                self.db.log_activity(
                    Some(d.id),
                    Some(p),
                    "rename",
                    &format!("{from} was renamed to {local_to} here and to {p} in the cloud; kept the cloud name"),
                    false,
                );
            } else {
                self.relocate_local(d, from, p)?;
            }
            if !item.is_dir {
                self.enqueue_reconcile(d.id, p, 0)?;
            } else {
                for c in self.db.subtree(d.id, p)? {
                    if !c.is_dir && c.local_state != LocalState::Placeholder {
                        self.enqueue_reconcile(d.id, &c.path, 0)?;
                    }
                }
            }
            return Ok(());
        }
        if item.is_dir {
            if matches!(change, RemoteChange::Added) && parent_pinned {
                let mut it = item;
                it.pinned = true;
                self.db.upsert_item(&it)?;
            }
            return Ok(());
        }
        let needs_engine = item.local_state != LocalState::Placeholder || parent_pinned || item.pinned || item.dirty;
        if needs_engine {
            self.enqueue_reconcile(d.id, p, 0)?;
        }
        Ok(())
    }

    pub(crate) fn on_remote_deleted(&self, d: &Drive, item: &Item) -> Result<()> {
        let subtree: Vec<Item> = if item.is_dir {
            let mut v = self.db.subtree(d.id, &item.path)?;
            if !v.iter().any(|i| i.path == item.path) {
                v.push(item.clone());
            }
            v
        } else {
            vec![item.clone()]
        };
        let has_local = subtree
            .iter()
            .any(|i| i.local_state != LocalState::Placeholder || i.dirty);
        if !has_local && self.db.pending_trash_covering(d.id, &item.path)?.is_none() {
            // Placeholders only (rule 6 for placeholders): drop them.
            if self.provider.local_status(d, &item.path)? != LocalStatus::Absent {
                self.provider.remove_local(d, &item.path)?;
            }
            self.db.delete_subtree(d.id, &item.path)?;
            return Ok(());
        }
        for mut i in subtree {
            i.remote = None;
            self.db.upsert_item(&i)?;
            if !i.is_dir || i.path == item.path {
                self.enqueue_reconcile(d.id, &i.path, 0)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Local events
    // ------------------------------------------------------------------

    pub fn handle_event(&self, ev: crate::provider::ProviderEvent) -> Result<()> {
        use crate::provider::ProviderEvent as E;
        let deb = self.settings.debounce_ms;
        match ev {
            E::HydrationRequested { drive, path } => {
                self.db
                    .enqueue_op("hydrate", drive, &path, &serde_json::json!({}), self.now())?;
            }
            E::Hydrated {
                drive,
                path,
                size,
                mtime_ns,
            } => self.on_hydrated(drive, &path, LocalFingerprint { size, mtime_ns })?,
            E::PinRequested { drive, path, recursive } => {
                self.db.enqueue_op(
                    "pin",
                    drive,
                    &path,
                    &serde_json::json!({ "recursive": recursive }),
                    self.now(),
                )?;
            }
            E::FreeRequested { drive, path, recursive } => {
                self.free(drive, &path, recursive)?;
            }
            E::LocalCreated { drive, path } | E::LocalModified { drive, path } | E::LocalDeleted { drive, path } => {
                self.enqueue_reconcile(drive, &path, deb)?;
            }
            E::LocalRenamed { drive, from, to } => {
                // Remember which cloud object (if any) was at `from` when the
                // user renamed it: by the time the move runs, a cloud rename
                // may have put a different item's record there.
                let was = self
                    .db
                    .item(drive, &from)?
                    .map(|i| (i.synced, i.remote.and_then(|r| r.id)));
                self.db.enqueue_op(
                    "move",
                    drive,
                    &to,
                    &serde_json::json!({ "from": from.as_str(), "was": was }),
                    self.now() + deb,
                )?;
            }
            E::FolderEnumerated { drive, path } => {
                let stale = match self.db.listed_at(drive, &path)? {
                    Some(t) => self.now() - t > self.settings.relist_on_open_ms,
                    None => true,
                };
                if stale {
                    self.db
                        .enqueue_op("list", drive, &path, &serde_json::json!({}), self.now())?;
                }
            }
            E::DriveRootRemoved { drive } => self.drive_folder_missing(drive)?,
            E::DriveRootRenamed { drive, new_name } => {
                self.rename_drive(drive, &new_name)?;
            }
        }
        Ok(())
    }

    /// The OS hydrated a placeholder on its own (D-021). If the file on disk
    /// is still exactly what the placeholder described, and that is the
    /// cloud version on record, it is now a downloaded copy of that version.
    /// Anything else is left to a normal reconcile, which re-derives it.
    fn on_hydrated(&self, drive: DriveId, p: &RelPath, served: LocalFingerprint) -> Result<()> {
        let d = self.drive(drive)?;
        let local = self.provider.local_status(&d, p)?;
        let item = self.db.item(drive, p)?;
        let matches = match (&item, local) {
            (Some(it), LocalStatus::File(fp)) => {
                fp == served
                    && it.local_state == LocalState::Placeholder
                    && it.remote.as_ref().map(|r| r.size == fp.size).unwrap_or(false)
            }
            _ => false,
        };
        if !matches {
            return self.enqueue_reconcile(drive, p, 0);
        }
        let mut it = item.expect("checked above");
        it.synced = it.remote.clone();
        it.local_base = Some(served);
        it.local_state = LocalState::Downloaded;
        it.dirty = false;
        self.db.upsert_item(&it)?;
        // Downloads stay: keep it from being freed automatically.
        self.provider.set_state(&d, p, LocalState::Downloaded)?;
        // A "hydrate" op queued for the same open is now done.
        self.db.cancel_ops(drive, "hydrate", p)?;
        self.db
            .log_activity(Some(drive), Some(p), "download", &format!("Downloaded {p}"), false);
        Ok(())
    }

    /// Compare the local folder with the DB and queue a reconcile for every
    /// difference (startup, and the stub provider's change detection).
    pub fn scan_drive(&self, drive_id: DriveId) -> Result<usize> {
        let d = self.drive(drive_id)?;
        if d.state != DriveState::Connected {
            return Ok(0);
        }
        if self.provider.local_status(&d, &RelPath::root())? == LocalStatus::Absent {
            // The whole folder is gone: that is a disconnect, never a mass delete.
            self.drive_folder_missing(drive_id)?;
            return Ok(0);
        }
        let entries = self.provider.scan(&d)?;
        let mut seen = std::collections::HashSet::new();
        let mut queued = 0;
        for e in &entries {
            seen.insert(e.path.clone());
            if e.path.is_root() || self.ignore.is_ignored(e.path.name()) {
                continue;
            }
            let item = self.db.item(d.id, &e.path)?;
            let differs = match (&item, e.status) {
                (None, LocalStatus::File(_) | LocalStatus::Dir) => true,
                (None, _) => false,
                (Some(i), _) if i.local_state == LocalState::Trashed => false,
                (Some(i), LocalStatus::File(fp)) => {
                    i.local_base != Some(fp) || i.local_state == LocalState::Placeholder
                }
                (Some(i), LocalStatus::Placeholder) => i.local_state == LocalState::Downloaded,
                (Some(i), LocalStatus::Dir) => !i.is_dir,
                (Some(_), LocalStatus::Absent) => false,
            };
            if differs {
                self.enqueue_reconcile(d.id, &e.path, self.settings.debounce_ms)?;
                queued += 1;
            }
        }
        // Known items that vanished locally (only in folders the OS populated).
        let mut stack = vec![RelPath::root()];
        while let Some(dir) = stack.pop() {
            if !self.provider.is_populated(&d, &dir) {
                continue;
            }
            for c in self.db.children(d.id, &dir)? {
                if c.local_state == LocalState::Trashed || c.unsyncable.is_some() {
                    continue;
                }
                if !seen.contains(&c.path) {
                    if c.remote.is_some() || c.synced.is_some() {
                        self.enqueue_reconcile(d.id, &c.path, self.settings.debounce_ms)?;
                        queued += 1;
                    }
                    continue;
                }
                if c.is_dir {
                    stack.push(c.path.clone());
                }
            }
        }
        Ok(queued)
    }

    // ------------------------------------------------------------------
    // Reconcile
    // ------------------------------------------------------------------

    pub async fn reconcile(&self, d: &Drive, p: &RelPath, want: Want) -> Result<Outcome> {
        if p.is_root() {
            return Ok(Outcome::Done);
        }
        // A pending local rename covers this path: the move op handles it
        // (otherwise the old name would look deleted and the new one new).
        if self.pending_move_involving(d.id, p)? {
            return Ok(Outcome::Done);
        }
        if self.ignore.is_ignored(p.name()) {
            return Ok(Outcome::Done);
        }
        if p.ancestors()
            .iter()
            .any(|a| !a.is_root() && self.ignore.is_ignored(a.name()))
        {
            return Ok(Outcome::Done);
        }
        // The drive's whole folder is gone: that is a disconnect, never a
        // pile of deletes (the OS may report each child before the folder).
        if self.provider.local_status(d, &RelPath::root())? == LocalStatus::Absent {
            self.drive_folder_missing(d.id)?;
            // Wait for that check: either the drive is disconnected (and this
            // op dropped) or the folder is back under its current name.
            return Ok(Outcome::Defer {
                ms: crate::engine::drives::ROOT_CHECK_DELAY_MS + 500,
                why: "the drive's folder is missing".into(),
            });
        }
        let item = self.db.item(d.id, p)?;
        if item.as_ref().map(|i| i.unsyncable.is_some()).unwrap_or(false) {
            return Ok(Outcome::Done);
        }
        let local = self.provider.local_status(d, p)?;
        if let Some(t) = self.db.pending_trash_covering(d.id, p)? {
            if &t.path == p && !t.is_dir && matches!(local, LocalStatus::File(_)) {
                // Deleted, then a file was saved under the same name again
                // (editors save this way): it is an edit, not a delete.
                self.db
                    .set_trash_status(t.id, "replaced", Some("a new file was saved with the same name"))?;
                if let Some(mut it) = item.clone() {
                    it.local_state = LocalState::Downloaded;
                    self.db.upsert_item(&it)?;
                }
            } else {
                return self.reconcile_trashed(d, p, &t).await;
            }
        }
        let item = self.db.item(d.id, p)?;
        let remote = self.stat_remote(d, p).await?;
        let is_dir = matches!(local, LocalStatus::Dir)
            || remote.as_ref().map(|r| r.is_dir).unwrap_or(false)
            || item.as_ref().map(|i| i.is_dir).unwrap_or(false);
        if is_dir {
            return self.reconcile_dir(d, p, item, local, remote).await;
        }
        self.reconcile_file(d, p, item, local, remote, want).await
    }

    async fn reconcile_file(
        &self,
        d: &Drive,
        p: &RelPath,
        item: Option<Item>,
        local: LocalStatus,
        remote: Option<RemoteMeta>,
        want: Want,
    ) -> Result<Outcome> {
        let rules = self.rules(d);
        let parent_populated = self.provider.is_populated(d, &p.parent().unwrap_or_default());
        // On platforms that create placeholders on demand, "absent" in an
        // unbrowsed folder just means "not created yet".
        let local = if local == LocalStatus::Absent && !parent_populated && item.is_some() {
            LocalStatus::Placeholder
        } else {
            local
        };
        match (item, local) {
            // ---------- not known yet ----------
            (None, LocalStatus::File(fp)) => match remote {
                None => self.upload(d, p, fp, None).await,
                Some(r) if r.is_dir => Ok(self.unsyncable(d, p, "a folder in the cloud has the same name")?),
                Some(r) => {
                    // Rule 9: created on both sides.
                    if self.same_content(d, p, fp, &r).await? {
                        self.adopt(d, p, fp, r)
                    } else {
                        self.keep_both(d, p, None, fp, r).await
                    }
                }
            },
            (None, LocalStatus::Absent | LocalStatus::Placeholder) => {
                if let Some(r) = remote {
                    let mut it = Item::new_remote(d.id, p.clone(), r);
                    if let Some(reason) = crate::names::unsyncable_reason(p.name(), self.provider.name_rules()) {
                        it.unsyncable = Some(reason);
                        self.db.upsert_item(&it)?;
                        return Ok(Outcome::Done);
                    }
                    self.db.upsert_item(&it)?;
                    if parent_populated {
                        self.provider
                            .upsert_placeholders(d, &p.parent().unwrap_or_default(), &[it.meta()])?;
                    }
                    if want == Want::Download || self.pinned(d.id, p)? {
                        return self.download(d, p, &it, LocalStatus::Placeholder, want).await;
                    }
                }
                Ok(Outcome::Done)
            }
            (None, LocalStatus::Dir) => Ok(Outcome::Done),

            // ---------- trashed rows without a trash entry (restored) ----------
            (Some(mut it), _) if it.local_state == LocalState::Trashed => {
                it.local_state = LocalState::Placeholder;
                self.db.upsert_item(&it)?;
                Ok(Outcome::Done)
            }

            // ---------- local copy gone ----------
            (Some(it), LocalStatus::Absent) => {
                match (&remote, &it.synced) {
                    // Never uploaded and now deleted: a plain local delete.
                    (None, None) => {
                        self.db.delete_item(d.id, p)?;
                        Ok(Outcome::Done)
                    }
                    // Gone on both sides.
                    (None, Some(_)) => {
                        self.db.delete_item(d.id, p)?;
                        Ok(Outcome::Done)
                    }
                    // Deleted here: into the trash queue, nothing sent (§4.6).
                    (Some(r), _) => {
                        let mut it = it;
                        it.remote = Some(r.clone());
                        self.db.upsert_item(&it)?;
                        self.trash_local_delete(d, &it)?;
                        Ok(Outcome::Done)
                    }
                }
            }

            // ---------- placeholder ----------
            (Some(mut it), LocalStatus::Placeholder) => match remote {
                None => {
                    // Rule 6 (placeholder): remove it.
                    self.provider.remove_local(d, p)?;
                    self.db.delete_item(d.id, p)?;
                    Ok(Outcome::Done)
                }
                Some(r) => {
                    let changed = it.remote.as_ref().map(|x| !rules.same(x, &r)).unwrap_or(true);
                    if it.local_state == LocalState::Downloaded {
                        // Freed by the OS behind our back: record it.
                        it.local_state = LocalState::Placeholder;
                    }
                    it.remote = Some(r.clone());
                    self.db.upsert_item(&it)?;
                    if changed && parent_populated {
                        // Rule 1: update placeholder metadata.
                        self.provider
                            .upsert_placeholders(d, &p.parent().unwrap_or_default(), &[it.meta()])?;
                    }
                    if want == Want::Download || it.pinned || self.pinned(d.id, p)? {
                        return self.download(d, p, &it, LocalStatus::Placeholder, want).await;
                    }
                    Ok(Outcome::Done)
                }
            },

            // ---------- local file ----------
            (Some(it), LocalStatus::File(fp)) => {
                let local_changed = it.synced.is_none() || it.local_base != Some(fp);
                let Some(r) = remote else {
                    if it.synced.is_none() || local_changed {
                        // Never uploaded, or rule 7: remote deleted, local dirty → upload as new.
                        let mut it2 = it.clone();
                        it2.synced = None;
                        it2.remote = None;
                        it2.dirty = true;
                        self.db.upsert_item(&it2)?;
                        return self.upload(d, p, fp, None).await;
                    }
                    // Rule 6: remote deleted, local clean → remove the local copy (to the OS trash).
                    if self.provider.is_open(d, p)? {
                        return Ok(self.defer("open"));
                    }
                    self.provider.remove_local(d, p)?;
                    self.db.delete_item(d.id, p)?;
                    self.db.log_activity(
                        Some(d.id),
                        Some(p),
                        "remove",
                        &format!("{p} was deleted in the cloud; moved the local copy to the trash"),
                        false,
                    );
                    return Ok(Outcome::Done);
                };
                if r.is_dir {
                    return self.unsyncable(d, p, "a folder in the cloud has the same name");
                }
                let Some(base) = it.synced.clone() else {
                    // Local file never synced, remote file exists: rule 9.
                    if self.same_content(d, p, fp, &r).await? {
                        return self.adopt(d, p, fp, r);
                    }
                    return self.keep_both(d, p, Some(it), fp, r).await;
                };
                let remote_changed = !rules.same(&base, &r);
                match (remote_changed, local_changed) {
                    (false, false) => {
                        let mut it = it;
                        if it.local_state != LocalState::Downloaded || it.dirty || it.remote.as_ref() != Some(&r) {
                            it.local_state = LocalState::Downloaded;
                            it.dirty = false;
                            it.remote = Some(r);
                            self.db.upsert_item(&it)?;
                        }
                        Ok(Outcome::Done)
                    }
                    // Rule 5.
                    (false, true) => {
                        let mut it = it;
                        it.dirty = true;
                        self.db.upsert_item(&it)?;
                        self.upload(d, p, fp, Some(&base)).await
                    }
                    // Rule 2 (or 3 when open).
                    (true, false) => {
                        if self.provider.is_open(d, p)? {
                            return Ok(self.defer("open"));
                        }
                        let mut it = it;
                        it.remote = Some(r);
                        self.db.upsert_item(&it)?;
                        self.download(d, p, &it, LocalStatus::File(fp), Want::Sync).await
                    }
                    // Rule 4.
                    (true, true) => {
                        if self.same_content(d, p, fp, &r).await? {
                            return self.adopt(d, p, fp, r);
                        }
                        self.keep_both(d, p, Some(it), fp, r).await
                    }
                }
            }
            (Some(it), LocalStatus::Dir) => {
                if !it.is_dir {
                    return self.unsyncable(d, p, "a local folder has the same name as a cloud file");
                }
                Ok(Outcome::Done)
            }
        }
    }

    async fn reconcile_dir(
        &self,
        d: &Drive,
        p: &RelPath,
        item: Option<Item>,
        local: LocalStatus,
        remote: Option<RemoteMeta>,
    ) -> Result<Outcome> {
        let parent_populated = self.provider.is_populated(d, &p.parent().unwrap_or_default());
        match (item, local, remote) {
            (None, LocalStatus::Dir, None) => {
                // New local folder.
                self.api.mkdir(&d.fs(), &d.remote_path(p)).await?;
                let r = self.stat_remote(d, p).await?.unwrap_or(RemoteMeta {
                    is_dir: true,
                    size: 0,
                    mtime_ns: None,
                    hash: None,
                    id: None,
                });
                let mut it = Item::new_remote(d.id, p.clone(), r.clone());
                it.synced = Some(r);
                it.local_state = LocalState::Downloaded;
                self.db.upsert_item(&it)?;
                // Files already inside were queued by their own events/scan.
                Ok(Outcome::Done)
            }
            (None, LocalStatus::Dir, Some(r)) => {
                if !r.is_dir {
                    return self.unsyncable(d, p, "a cloud file has the same name as this folder");
                }
                let mut it = Item::new_remote(d.id, p.clone(), r.clone());
                it.synced = Some(r);
                it.local_state = LocalState::Downloaded;
                self.db.upsert_item(&it)?;
                Ok(Outcome::Done)
            }
            (None, _, Some(r)) => {
                if !r.is_dir {
                    return Ok(Outcome::Done);
                }
                let it = Item::new_remote(d.id, p.clone(), r);
                self.db.upsert_item(&it)?;
                if parent_populated {
                    self.provider
                        .upsert_placeholders(d, &p.parent().unwrap_or_default(), &[it.meta()])?;
                }
                Ok(Outcome::Done)
            }
            (None, _, None) => Ok(Outcome::Done),
            (Some(it), LocalStatus::Absent, remote) if parent_populated => match remote {
                Some(r) => {
                    let mut it = it;
                    it.remote = Some(r);
                    self.db.upsert_item(&it)?;
                    self.trash_local_delete(d, &it)?;
                    Ok(Outcome::Done)
                }
                None => {
                    self.db.delete_subtree(d.id, p)?;
                    Ok(Outcome::Done)
                }
            },
            (Some(_), LocalStatus::Absent, _) => Ok(Outcome::Done),
            (Some(it), LocalStatus::Dir | LocalStatus::Placeholder, None) => {
                // Deleted in the cloud. Children with local edits were kept
                // (rule 7) and are uploaded again; the folder goes only when
                // nothing is left in it.
                let children = self.db.children(d.id, p)?;
                let local_children: Vec<_> = self
                    .provider
                    .scan(d)?
                    .into_iter()
                    .filter(|e| e.path.parent().as_ref() == Some(p) && !self.ignore.is_ignored(e.path.name()))
                    .collect();
                if children.is_empty() && local_children.is_empty() {
                    self.provider.remove_local(d, p)?;
                    self.db.delete_subtree(d.id, p)?;
                    return Ok(Outcome::Done);
                }
                if children.iter().any(|c| c.remote.is_none() && c.synced.is_some()) {
                    // Children still being handled; look again shortly.
                    return Ok(self.defer("children pending"));
                }
                self.api.mkdir(&d.fs(), &d.remote_path(p)).await?;
                let r = self.stat_remote(d, p).await?;
                let mut it = it;
                it.remote = r.clone();
                it.synced = r;
                self.db.upsert_item(&it)?;
                Ok(Outcome::Done)
            }
            (Some(mut it), _, Some(r)) => {
                if !r.is_dir {
                    return self.unsyncable(d, p, "a cloud file has the same name as this folder");
                }
                if it.synced.is_none() || it.remote.as_ref() != Some(&r) {
                    it.remote = Some(r.clone());
                    it.synced = Some(r);
                    self.db.upsert_item(&it)?;
                }
                Ok(Outcome::Done)
            }
            (Some(_), LocalStatus::File(_), None) => Ok(Outcome::Done),
        }
    }

    fn defer(&self, why: &str) -> Outcome {
        Outcome::Defer {
            ms: self.settings.open_retry_ms,
            why: why.into(),
        }
    }

    fn unsyncable(&self, d: &Drive, p: &RelPath, why: &str) -> Result<Outcome> {
        let mut it = self.db.item(d.id, p)?.unwrap_or(Item {
            drive_id: d.id,
            path: p.clone(),
            is_dir: false,
            remote: None,
            local_state: LocalState::Downloaded,
            pinned: false,
            synced: None,
            local_base: None,
            dirty: false,
            unsyncable: None,
        });
        it.unsyncable = Some(why.into());
        self.db.upsert_item(&it)?;
        self.db.log_activity(
            Some(d.id),
            Some(p),
            "unsyncable",
            &format!("{p} is not synced: {why}"),
            true,
        );
        Ok(Outcome::Done)
    }

    /// Record a local file and remote object as already in sync.
    fn adopt(&self, d: &Drive, p: &RelPath, fp: LocalFingerprint, r: RemoteMeta) -> Result<Outcome> {
        let mut it = self
            .db
            .item(d.id, p)?
            .unwrap_or_else(|| Item::new_remote(d.id, p.clone(), r.clone()));
        it.remote = Some(r.clone());
        it.synced = Some(r);
        it.local_base = Some(fp);
        it.local_state = LocalState::Downloaded;
        it.dirty = false;
        self.db.upsert_item(&it)?;
        Ok(Outcome::Done)
    }

    /// Whether the local file has the same content as the remote object.
    /// Uses the remote's hash where there is one (computed locally by
    /// rclone), else size + time.
    async fn same_content(&self, d: &Drive, p: &RelPath, fp: LocalFingerprint, r: &RemoteMeta) -> Result<bool> {
        if fp.size != r.size {
            return Ok(false);
        }
        if let (Some(ht), Some(rh)) = (&d.hash_type, &r.hash) {
            let path = self.provider.content_path(d, p);
            let (dir, name) = split_local(&path)?;
            if let Some(lh) = self.api.hashsum_file(&dir, &name, ht).await? {
                return Ok(lh.eq_ignore_ascii_case(rh));
            }
        }
        Ok(match r.mtime_ns {
            Some(t) if d.keeps_mod_times => (t - fp.mtime_ns).abs() < d.time_tolerance_ns,
            _ => false,
        })
    }

    // ------------------------------------------------------------------
    // Transfers
    // ------------------------------------------------------------------

    /// Upload the local file at `p`. `base` is the synced remote version
    /// the upload is allowed to replace (None: the remote must not exist).
    async fn upload(&self, d: &Drive, p: &RelPath, fp: LocalFingerprint, base: Option<&RemoteMeta>) -> Result<Outcome> {
        if d.paused {
            return Ok(self.defer("paused"));
        }
        if self.provider.is_open(d, p)? {
            return Ok(self.defer("open"));
        }
        // Re-stat right before writing: if the remote moved on since the
        // decision, start over rather than overwrite (§4.4).
        let now_remote = self.stat_remote(d, p).await?;
        let rules = self.rules(d);
        match (base, &now_remote) {
            (None, Some(_)) => {
                return Ok(Outcome::Defer {
                    ms: 0,
                    why: "remote appeared".into(),
                })
            }
            (Some(b), Some(r)) if !rules.same(b, r) => {
                return Ok(Outcome::Defer {
                    ms: 0,
                    why: "remote changed".into(),
                })
            }
            (Some(_), None) => {
                return Ok(Outcome::Defer {
                    ms: 0,
                    why: "remote deleted".into(),
                })
            }
            _ => {}
        }
        let path = self.provider.content_path(d, p);
        let (dir, name) = split_local(&path)?;
        self.api
            .copyfile(&dir, &name, &d.fs(), &d.remote_path(p))
            .await
            .with_context(|| format!("uploading {p}"))?;
        let r = self
            .stat_remote(d, p)
            .await?
            .ok_or_else(|| anyhow!("{p} is not in the cloud after uploading it"))?;
        let after = self.provider.local_status(d, p)?;
        let mut it = self
            .db
            .item(d.id, p)?
            .unwrap_or_else(|| Item::new_remote(d.id, p.clone(), r.clone()));
        it.is_dir = false;
        it.remote = Some(r.clone());
        // The server's own stat is the baseline (servers re-stamp times).
        it.synced = Some(r);
        // The local baseline is the snapshot from before the upload, so a
        // write during the upload still reads as an edit next time.
        it.local_base = Some(fp);
        it.local_state = LocalState::Downloaded;
        it.dirty = after != LocalStatus::File(fp);
        self.db.upsert_item(&it)?;
        self.provider.set_state(d, p, LocalState::Downloaded)?;
        self.db
            .log_activity(Some(d.id), Some(p), "upload", &format!("Uploaded {p}"), false);
        if it.dirty {
            self.enqueue_reconcile(d.id, p, self.settings.debounce_ms)?;
        }
        Ok(Outcome::Done)
    }

    /// Download the remote version of `p` and put it in place, provided the
    /// local file still matches `expected`.
    async fn download(&self, d: &Drive, p: &RelPath, it: &Item, expected: LocalStatus, _want: Want) -> Result<Outcome> {
        if d.paused {
            return Ok(self.defer("paused"));
        }
        let r = it.remote.clone().ok_or_else(|| anyhow!("{p} is not in the cloud"))?;
        let stage_dir = self.staging_dir.join(d.id.to_string());
        std::fs::create_dir_all(&stage_dir)?;
        let stage_name = format!(
            "{}-{}.partial",
            std::process::id(),
            STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let staged = stage_dir.join(&stage_name);
        let result = async {
            self.api
                .copyfile(
                    &d.fs(),
                    &d.remote_path(p),
                    &stage_dir.display().to_string(),
                    &stage_name,
                )
                .await
                .with_context(|| format!("downloading {p}"))?;
            let meta = std::fs::metadata(&staged).with_context(|| format!("downloaded {p} is missing"))?;
            if r.size >= 0 && meta.len() as i64 != r.size {
                // The remote changed under us; the next reconcile sees the new version.
                return Ok(Outcome::Defer {
                    ms: 0,
                    why: "size changed while downloading".into(),
                });
            }
            if let (Some(ht), Some(rh)) = (&d.hash_type, &r.hash) {
                if let Some(h) = self
                    .api
                    .hashsum_file(&stage_dir.display().to_string(), &stage_name, ht)
                    .await?
                {
                    if !h.eq_ignore_ascii_case(rh) {
                        return Ok(Outcome::Defer {
                            ms: 0,
                            why: "content changed while downloading".into(),
                        });
                    }
                }
            }
            let fp = match self.provider.commit_download(d, p, &staged, &it.meta(), expected) {
                Ok(fp) => fp,
                Err(e) => {
                    tracing::info!("not replacing {p}: {e}");
                    return Ok(Outcome::Defer {
                        ms: 0,
                        why: "local file changed".into(),
                    });
                }
            };
            let mut it = self.db.item(d.id, p)?.unwrap_or_else(|| it.clone());
            it.remote = Some(r.clone());
            it.synced = Some(r.clone());
            it.local_base = Some(fp);
            it.local_state = LocalState::Downloaded;
            it.dirty = false;
            self.db.upsert_item(&it)?;
            self.db
                .log_activity(Some(d.id), Some(p), "download", &format!("Downloaded {p}"), false);
            Ok(Outcome::Done)
        }
        .await;
        let _ = std::fs::remove_file(&staged);
        result
    }

    /// Rules 4 and 9: keep both. The local version is copied to a conflict
    /// name and uploaded as a new file; the remote version takes the
    /// original name.
    async fn keep_both(
        &self,
        d: &Drive,
        p: &RelPath,
        it: Option<Item>,
        fp: LocalFingerprint,
        r: RemoteMeta,
    ) -> Result<Outcome> {
        if self.provider.is_open(d, p)? {
            return Ok(self.defer("open"));
        }
        // After a crash mid-way, reuse the copy already made.
        let existing = self
            .db
            .conflicts(false)?
            .into_iter()
            .filter(|c| c.drive_id == d.id && &c.path == p)
            .find(|c| {
                files_equal(
                    &self.provider.content_path(d, p),
                    &self.provider.content_path(d, &c.conflict_copy_path),
                )
            });
        let copy = match existing {
            Some(c) => c.conflict_copy_path,
            None => {
                let copy = conflict_path(p, &self.device, chrono::Local::now(), &|cand| {
                    self.provider
                        .local_status(d, cand)
                        .map(|s| s != LocalStatus::Absent)
                        .unwrap_or(true)
                        || self.db.item(d.id, cand).ok().flatten().is_some()
                });
                self.db.insert_conflict(d.id, p, &copy)?;
                self.provider.copy_local(d, p, &copy)?;
                copy
            }
        };
        // Upload the conflict copy as a new file.
        if let LocalStatus::File(cfp) = self.provider.local_status(d, &copy)? {
            if self.stat_remote(d, &copy).await?.is_none() {
                self.upload(d, &copy, cfp, None).await?;
            }
        }
        // The remote version becomes the canonical name.
        let mut item = it.unwrap_or_else(|| Item::new_remote(d.id, p.clone(), r.clone()));
        item.remote = Some(r);
        self.db.upsert_item(&item)?;
        let out = self.download(d, p, &item, LocalStatus::File(fp), Want::Sync).await?;
        self.db.log_activity(
            Some(d.id),
            Some(p),
            "conflict",
            &format!("{p} was changed here and in the cloud; your version was saved as {copy}"),
            true,
        );
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Pin / free / hydrate / move
    // ------------------------------------------------------------------

    /// Save to device. Folders: everything beneath, and new children later.
    pub async fn pin(&self, drive_id: DriveId, p: &RelPath, recursive: bool) -> Result<Outcome> {
        let d = self.drive(drive_id)?;
        let mut it = match self.db.item(d.id, p)? {
            Some(i) => i,
            None if p.is_root() => {
                self.db.ensure_root(d.id)?;
                self.db.item(d.id, p)?.unwrap()
            }
            None => {
                // Maybe not indexed yet.
                self.index_dir(&d, &p.parent().unwrap_or_default()).await?;
                self.db.item(d.id, p)?.ok_or_else(|| anyhow!("{p} was not found"))?
            }
        };
        it.pinned = true;
        self.db.upsert_item(&it)?;
        if !it.is_dir {
            return self.reconcile(&d, p, Want::Download).await;
        }
        // Index the folder fully, then queue every file beneath.
        let mut queue = vec![p.clone()];
        while let Some(dir) = queue.pop() {
            self.index_dir(&d, &dir).await?;
            for c in self.db.children(d.id, &dir)? {
                if c.unsyncable.is_some() || c.local_state == LocalState::Trashed {
                    continue;
                }
                if c.is_dir {
                    if recursive {
                        queue.push(c.path.clone());
                    }
                } else if c.local_state == LocalState::Placeholder {
                    self.db
                        .enqueue_op("hydrate", d.id, &c.path, &serde_json::json!({}), self.now())?;
                }
            }
        }
        self.db
            .log_activity(Some(d.id), Some(p), "pin", &format!("Saving {p} to this device"), false);
        Ok(Outcome::Done)
    }

    /// Remove from device. Files with unsynced edits or open files are
    /// kept; their paths are returned.
    pub fn free(&self, drive_id: DriveId, p: &RelPath, recursive: bool) -> Result<Vec<RelPath>> {
        let d = self.drive(drive_id)?;
        let targets: Vec<Item> = match self.db.item(d.id, p)? {
            Some(i) if !i.is_dir => vec![i],
            Some(_) | None if p.is_root() || self.db.item(d.id, p)?.is_some() => {
                if recursive {
                    self.db.subtree(d.id, p)?
                } else {
                    self.db.children(d.id, p)?
                }
            }
            _ => return Err(anyhow!("{p} was not found")),
        };
        if let Some(mut it) = self.db.item(d.id, p)? {
            it.pinned = false;
            self.db.upsert_item(&it)?;
        }
        let mut kept = Vec::new();
        for mut t in targets {
            if t.is_dir {
                if t.pinned {
                    t.pinned = false;
                    self.db.upsert_item(&t)?;
                }
                continue;
            }
            t.pinned = false;
            if t.local_state != LocalState::Downloaded {
                self.db.upsert_item(&t)?;
                continue;
            }
            let st = self.provider.local_status(&d, &t.path)?;
            let dirty = match st {
                LocalStatus::File(fp) => t.local_base != Some(fp) || t.synced.is_none(),
                _ => false,
            };
            if dirty || self.provider.is_open(&d, &t.path)? {
                kept.push(t.path.clone());
                self.db.upsert_item(&t)?;
                continue;
            }
            self.provider.set_state(&d, &t.path, LocalState::Placeholder)?;
            t.local_state = LocalState::Placeholder;
            t.local_base = None;
            self.db.upsert_item(&t)?;
        }
        self.db.log_activity(
            Some(d.id),
            Some(p),
            "free",
            &format!("Removed {p} from this device"),
            false,
        );
        Ok(kept)
    }

    /// A local rename (`from` → `to`), journaled as a "move" op.
    pub async fn apply_local_move(&self, d: &Drive, from: &RelPath, to: &RelPath) -> Result<Outcome> {
        self.apply_local_move_checked(d, from, to, None).await
    }

    /// `was`: the record at `from` when the rename happened (see
    /// `handle_event`). If the record there now is a different item, the
    /// renamed local file is not it (found by the property test on macOS CI).
    pub async fn apply_local_move_checked(
        &self,
        d: &Drive,
        from: &RelPath,
        to: &RelPath,
        was: Option<Option<(Option<RemoteMeta>, Option<String>)>>,
    ) -> Result<Outcome> {
        let Some(it) = self.db.item(d.id, from)? else {
            // Never synced: the new name is just a new local item.
            return self.reconcile(d, to, Want::Sync).await;
        };
        if let Some(was) = was {
            let now = (it.synced.clone(), it.remote.as_ref().and_then(|r| r.id.clone()));
            let same = match was {
                None => false,
                Some((synced, id)) => id == now.1 && (id.is_some() || synced == now.0),
            };
            if !same {
                self.restore_placeholder_at(d, &it)?;
                return self.reconcile(d, to, Want::Sync).await;
            }
        }
        if it.synced.is_none() && it.remote.is_none() {
            self.db.delete_subtree(d.id, from)?;
            return self.reconcile(d, to, Want::Sync).await;
        }
        // Only rename in the cloud what the local item really is: a synced
        // file or folder, or a placeholder that stayed a placeholder. A new
        // local file that happens to sit where a cloud file appeared is not
        // that cloud file (found by the property test).
        let to_status = self.provider.local_status(d, to)?;
        let corresponds = if it.is_dir {
            it.synced.is_some() || it.remote.as_ref().map(|r| r.is_dir).unwrap_or(false)
        } else {
            it.synced.is_some() || (it.local_state == LocalState::Placeholder && to_status == LocalStatus::Placeholder)
        };
        if !corresponds {
            self.restore_placeholder_at(d, &it)?;
            return self.reconcile(d, to, Want::Sync).await;
        }
        // Never move onto something that exists in the cloud.
        if self.stat_remote(d, to).await?.is_some() {
            self.restore_placeholder_at(d, &it)?;
            return self.reconcile(d, to, Want::Sync).await;
        }
        let r = self.stat_remote(d, from).await?;
        let expected_id = it.remote.as_ref().or(it.synced.as_ref()).and_then(|m| m.id.clone());
        let same_object = match (&r, &expected_id) {
            (Some(r), Some(id)) => r.id.as_deref() == Some(id.as_str()),
            (Some(r), None) => r.is_dir == it.is_dir,
            (None, _) => false,
        };
        if !same_object {
            // Moved or deleted remotely too. A listing tells which (rule 11).
            self.index_dir(d, &from.parent().unwrap_or_default()).await?;
            if let Some(id) = &expected_id {
                if let Some(moved) = self.db.item_by_remote_id(d.id, id)? {
                    if &moved.path != from && &moved.path != to {
                        // Remote name wins.
                        self.relocate_local(d, to, &moved.path)?;
                        self.db.log_activity(
                            Some(d.id),
                            Some(&moved.path),
                            "rename",
                            &format!(
                                "{from} was renamed to {to} here and to {} in the cloud; kept the cloud name",
                                moved.path
                            ),
                            false,
                        );
                        return self.reconcile(d, &moved.path, Want::Sync).await;
                    }
                }
            }
            // Deleted remotely: keep the local item and upload it as new.
            self.db.delete_subtree(d.id, from)?;
            return self.reconcile(d, to, Want::Sync).await;
        }
        if it.is_dir {
            self.api
                .movedir(&d.fs(), &d.remote_path(from), &d.remote_path(to))
                .await?;
        } else {
            self.api
                .movefile(&d.fs(), &d.remote_path(from), &d.fs(), &d.remote_path(to))
                .await
                .with_context(|| format!("renaming {from} to {to}"))?;
        }
        self.db.move_subtree(d.id, from, to)?;
        // Remote metadata at the new path (IDs usually survive a move).
        if let Some(nr) = self.stat_remote(d, to).await? {
            if let Some(mut moved) = self.db.item(d.id, to)? {
                moved.remote = Some(nr);
                self.db.upsert_item(&moved)?;
            }
        }
        self.db.log_activity(
            Some(d.id),
            Some(to),
            "rename",
            &format!("Renamed {from} to {to}"),
            false,
        );
        // Rule 10: the content may have changed remotely too.
        if it.is_dir {
            for c in self.db.subtree(d.id, to)? {
                if !c.is_dir && c.local_state != LocalState::Placeholder {
                    self.enqueue_reconcile(d.id, &c.path, 0)?;
                }
            }
            Ok(Outcome::Done)
        } else {
            self.reconcile(d, to, Want::Sync).await
        }
    }
}

impl Engine {
    /// Follow a cloud rename locally: move `from` to `to`, but never over
    /// something that exists at `to`. In that case the local item at `from`
    /// stays (and is uploaded as new) and the rows now at `to` lose their
    /// baseline, so the next reconcile keeps both versions (rule 9).
    pub(crate) fn relocate_local(&self, d: &Drive, from: &RelPath, to: &RelPath) -> Result<()> {
        let src = self.provider.local_status(d, from)?;
        let dst = self.provider.local_status(d, to)?;
        if src != LocalStatus::Absent && dst == LocalStatus::Absent {
            self.provider.rename_local(d, from, to)?;
            return Ok(());
        }
        let mut rows = self.db.subtree(d.id, to)?;
        if let Some(it) = self.db.item(d.id, to)? {
            if !rows.iter().any(|r| r.path == it.path) {
                rows.push(it);
            }
        }
        rows.sort_by_key(|r| r.path.depth());
        for mut r in rows {
            let st = self.provider.local_status(d, &r.path)?;
            if !r.is_dir {
                r.synced = None;
                r.local_base = None;
                r.dirty = false;
                r.local_state = if matches!(st, LocalStatus::File(_)) {
                    LocalState::Downloaded
                } else {
                    LocalState::Placeholder
                };
            }
            self.db.upsert_item(&r)?;
            if st == LocalStatus::Absent {
                let parent = r.path.parent().unwrap_or_default();
                if self.provider.is_populated(d, &parent) {
                    self.provider.upsert_placeholders(d, &parent, &[r.meta()])?;
                }
            }
        }
        Ok(())
    }

    /// The cloud item at `it.path` is still there but the local one moved
    /// away (or never was it): show it as a placeholder again.
    fn restore_placeholder_at(&self, d: &Drive, it: &Item) -> Result<()> {
        if it.remote.is_none() {
            self.db.delete_subtree(d.id, &it.path)?;
            return Ok(());
        }
        let mut it = it.clone();
        if !it.is_dir {
            it.local_state = LocalState::Placeholder;
            it.local_base = None;
            it.synced = None;
            it.dirty = false;
        }
        self.db.upsert_item(&it)?;
        let parent = it.path.parent().unwrap_or_default();
        if self.provider.is_populated(d, &parent) && self.provider.local_status(d, &it.path)? == LocalStatus::Absent {
            self.provider.upsert_placeholders(d, &parent, &[it.meta()])?;
        }
        Ok(())
    }
}

/// Split a local file path into (directory as rclone fs, file name).
pub(crate) fn split_local(path: &Path) -> Result<(String, String)> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent folder", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("{} has no usable name", path.display()))?;
    Ok((dir.display().to_string(), name.to_string()))
}

fn files_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Whether an error is worth retrying (rate limits, network, changing sources).
pub fn is_retryable(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(r) = cause.downcast_ref::<RcloneError>() {
            return r.is_retryable();
        }
    }
    true
}
