//! The delete queue (PLAN.md §4.6). A local delete never reaches the
//! cloud until the user empties the trash; entries can be restored.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use synclone_rclone::ListOpts;

use super::sync::{Outcome, Want};
use super::Engine;
use crate::compare::remote_meta;
use crate::db::{TrashEntry, TrashSnapshot};
use crate::types::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashSummary {
    pub count: usize,
    pub total_size: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyReport {
    /// Paths deleted from the cloud.
    pub deleted: Vec<String>,
    /// Paths that changed in the cloud after the local delete: kept and restored.
    pub restored: Vec<String>,
}

impl Engine {
    /// Rule: local delete of a synced item → trash row; nothing is sent.
    pub(crate) fn trash_local_delete(&self, d: &Drive, it: &crate::db::Item) -> Result<()> {
        if self.db.pending_trash_covering(d.id, &it.path)?.is_some() {
            return Ok(());
        }
        let mut items = Vec::new();
        let mut downloaded = Vec::new();
        let mut size = 0;
        let subtree = if it.is_dir {
            self.db.subtree(d.id, &it.path)?
        } else {
            vec![it.clone()]
        };
        let mut rows = subtree;
        if it.is_dir && !rows.iter().any(|r| r.path == it.path) {
            rows.push(it.clone());
        }
        for mut r in rows {
            if let Some(m) = r.remote.clone().or_else(|| r.synced.clone()) {
                if !m.is_dir {
                    size += m.size.max(0);
                }
                items.push((r.path.clone(), m));
            }
            if !r.is_dir && r.local_state == LocalState::Downloaded && r.synced.is_some() {
                if let Some(fp) = r.local_base {
                    downloaded.push((r.path.clone(), fp));
                }
            }
            r.local_state = LocalState::Trashed;
            self.db.upsert_item(&r)?;
        }
        // Any queued work for the deleted paths is moot.
        self.db.insert_trash(&TrashEntry {
            id: 0,
            drive_id: d.id,
            path: it.path.clone(),
            is_dir: it.is_dir,
            size,
            deleted_at: self.now(),
            snapshot: TrashSnapshot { items, downloaded },
            status: "pending".into(),
            note: None,
        })?;
        self.db.log_activity(
            Some(d.id),
            Some(&it.path),
            "trash",
            &format!(
                "{} moved to Synclone's trash (still in the cloud until you empty the trash)",
                it.path
            ),
            false,
        );
        Ok(())
    }

    /// Reconcile a path covered by a pending trash entry: rule 8.
    pub(crate) async fn reconcile_trashed(&self, d: &Drive, p: &RelPath, t: &TrashEntry) -> Result<Outcome> {
        let snap: HashMap<&RelPath, &RemoteMeta> = t.snapshot.items.iter().map(|(p, m)| (p, m)).collect();
        let Some(before) = snap.get(p) else {
            // A new remote item inside a trashed folder: the folder changed.
            if self.stat_remote(d, p).await?.is_some() {
                self.restore_trash(t.id, Some("new files appeared in the cloud after you deleted it"))?;
            }
            return Ok(Outcome::Done);
        };
        let now = self.stat_remote(d, p).await?;
        match now {
            None => {
                // Gone in the cloud too: nothing left to delete for this item.
                if &t.path == p {
                    self.db
                        .set_trash_status(t.id, "gone", Some("deleted in the cloud as well"))?;
                    self.db.delete_subtree(d.id, p)?;
                }
                Ok(Outcome::Done)
            }
            Some(r) if !self.rules(d).same(before, &r) => {
                self.restore_trash(t.id, Some("it changed in the cloud after you deleted it"))?;
                self.db.log_activity(
                    Some(d.id),
                    Some(p),
                    "trash",
                    &format!("{} was restored: it changed in the cloud after you deleted it", t.path),
                    true,
                );
                Ok(Outcome::Done)
            }
            Some(_) => Ok(Outcome::Done),
        }
    }

    pub fn trash_list(&self) -> Result<Vec<TrashEntry>> {
        Ok(self.db.trash(Some("pending"))?)
    }

    pub fn trash_summary(&self) -> Result<TrashSummary> {
        let t = self.trash_list()?;
        Ok(TrashSummary {
            count: t.len(),
            total_size: t.iter().map(|e| e.size).sum(),
        })
    }

    /// Put a trashed item back. No remote calls here: everything comes back
    /// as placeholders at once. When the user restores (`note` is None),
    /// files that were downloaded are queued to come back downloaded,
    /// from the OS's deleted copy when it still matches, else from the
    /// cloud (`recover` ops, D-032). Restores the engine makes itself (rule 8)
    /// leave them as placeholders.
    pub fn restore_trash(&self, id: i64, note: Option<&str>) -> Result<()> {
        let t = self.db.trash_entry(id)?.ok_or_else(|| anyhow!("no trash entry {id}"))?;
        if t.status != "pending" {
            return Ok(());
        }
        let d = self.drive(t.drive_id)?;
        let rows = self.db.subtree(d.id, &t.path)?;
        let mut rows: Vec<_> = rows;
        if let Some(it) = self.db.item(d.id, &t.path)? {
            if !rows.iter().any(|r| r.path == it.path) {
                rows.push(it);
            }
        }
        rows.sort_by_key(|r| r.path.depth());
        for mut r in rows {
            r.local_state = LocalState::Placeholder;
            r.local_base = None;
            self.db.upsert_item(&r)?;
            let parent = r.path.parent().unwrap_or_default();
            if self.provider.is_populated(&d, &parent) {
                self.provider.upsert_placeholders(&d, &parent, &[r.meta()])?;
            }
            if !r.is_dir && (r.pinned || self.pinned(d.id, &r.path)?) {
                self.db
                    .enqueue_op("hydrate", d.id, &r.path, &serde_json::json!({}), self.now())?;
            } else if note.is_none() {
                if let Some((_, fp)) = t.snapshot.downloaded.iter().find(|(p, _)| p == &r.path) {
                    self.db.enqueue_op(
                        "recover",
                        d.id,
                        &r.path,
                        &serde_json::json!({ "size": fp.size, "mtime_ns": fp.mtime_ns }),
                        self.now(),
                    )?;
                }
            }
        }
        self.db
            .set_trash_status(id, if note.is_some() { "cancelled" } else { "restored" }, note)?;
        Ok(())
    }

    pub fn restore_all(&self) -> Result<usize> {
        let all = self.trash_list()?;
        for t in &all {
            self.restore_trash(t.id, None)?;
        }
        Ok(all.len())
    }

    /// Queue every pending entry for deletion from the cloud. The ops are
    /// journaled so a crash mid-way resumes.
    pub fn request_empty_trash(&self) -> Result<TrashSummary> {
        let s = self.trash_summary()?;
        for t in self.trash_list()? {
            self.db.enqueue_op(
                "empty_trash",
                t.drive_id,
                &t.path,
                &serde_json::json!({ "trash_id": t.id }),
                self.now(),
            )?;
        }
        Ok(s)
    }

    /// Delete one trash entry from the cloud. Uses the provider's trash
    /// (no hard-delete flags). Anything that changed since the local delete
    /// is kept and restored instead.
    pub async fn empty_trash_entry(&self, id: i64) -> Result<EmptyReport> {
        let mut report = EmptyReport::default();
        let Some(t) = self.db.trash_entry(id)? else {
            return Ok(report);
        };
        if t.status != "pending" {
            return Ok(report);
        }
        let d = self.drive(t.drive_id)?;
        let rules = self.rules(&d);
        let snap: HashMap<RelPath, RemoteMeta> = t.snapshot.items.iter().cloned().collect();
        if !t.is_dir {
            let Some(before) = snap.get(&t.path) else {
                self.db.set_trash_status(id, "gone", None)?;
                return Ok(report);
            };
            match self.stat_remote(&d, &t.path).await? {
                None => {
                    self.db
                        .set_trash_status(id, "gone", Some("already gone from the cloud"))?;
                }
                Some(r) if rules.same(before, &r) => {
                    match self.api.deletefile(&d.fs(), &d.remote_path(&t.path)).await {
                        Ok(()) => {}
                        Err(e) if e.is_not_found() => {}
                        Err(e) => return Err(e.into()),
                    }
                    report.deleted.push(t.path.to_string());
                    self.db.set_trash_status(id, "emptied", None)?;
                }
                Some(_) => {
                    self.restore_trash(id, Some("it changed in the cloud after you deleted it"))?;
                    report.restored.push(t.path.to_string());
                }
            }
            self.finish_trash_rows(&d, &t)?;
            return Ok(report);
        }
        // A folder: compare everything under it with the snapshot.
        let listing = match self
            .api
            .list(
                &d.fs(),
                &d.remote_path(&t.path),
                &ListOpts {
                    recurse: true,
                    hash_types: d.hash_type.iter().cloned().collect(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(l) => l,
            Err(e) if e.is_not_found() => {
                self.db
                    .set_trash_status(id, "gone", Some("already gone from the cloud"))?;
                self.finish_trash_rows(&d, &t)?;
                return Ok(report);
            }
            Err(e) => return Err(e.into()),
        };
        let mut unchanged = Vec::new();
        let mut changed = Vec::new();
        for e in &listing {
            if e.is_dir {
                continue;
            }
            let root = d.root_path.trim_matches('/');
            let rel = if root.is_empty() {
                e.path.as_str()
            } else {
                e.path.strip_prefix(root).unwrap_or(&e.path).trim_start_matches('/')
            };
            let Some(rel) = RelPath::new(rel) else { continue };
            let now = remote_meta(e, d.hash_type.as_deref());
            match snap.get(&rel) {
                Some(b) if rules.same(b, &now) => unchanged.push(rel),
                _ => changed.push(rel),
            }
        }
        if changed.is_empty() {
            self.api.purge(&d.fs(), &d.remote_path(&t.path)).await?;
            report.deleted.push(t.path.to_string());
            self.db.set_trash_status(id, "emptied", None)?;
            self.finish_trash_rows(&d, &t)?;
            return Ok(report);
        }
        // Delete only what is exactly as it was; keep the rest.
        for p in &unchanged {
            match self.api.deletefile(&d.fs(), &d.remote_path(p)).await {
                Ok(()) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
            self.db.delete_item(d.id, p)?;
            report.deleted.push(p.to_string());
        }
        for p in &changed {
            report.restored.push(p.to_string());
        }
        self.restore_trash(
            id,
            Some("some files changed in the cloud after you deleted the folder; those were kept"),
        )?;
        // Restored rows for deleted files must not come back.
        for p in &unchanged {
            if self.provider.local_status(&d, p)? != LocalStatus::Absent {
                self.provider.remove_local(&d, p)?;
            }
            self.db.delete_item(d.id, p)?;
        }
        self.db.log_activity(
            Some(d.id),
            Some(&t.path),
            "trash",
            &format!(
                "Emptied part of {}; {} item(s) changed in the cloud and were kept",
                t.path,
                changed.len()
            ),
            true,
        );
        Ok(report)
    }

    /// A restored file that was downloaded when it was deleted (D-032):
    /// bring it back downloaded. If the cloud copy is unchanged since the
    /// last sync and the OS still holds the deleted copy (same size and
    /// time, and the same hash where the service has one), move that copy
    /// back; otherwise download it.
    pub(crate) async fn recover_restored(&self, d: &Drive, p: &RelPath, fp: LocalFingerprint) -> Result<Outcome> {
        let Some(it) = self.db.item(d.id, p)? else {
            return Ok(Outcome::Done);
        };
        if it.is_dir
            || it.local_state != LocalState::Placeholder
            || self.provider.local_status(d, p)? != LocalStatus::Placeholder
            || self.db.pending_trash_covering(d.id, p)?.is_some()
        {
            // Changed again since the restore: normal sync takes over.
            return Ok(Outcome::Done);
        }
        let Some(now) = self.stat_remote(d, p).await? else {
            return Ok(Outcome::Done);
        };
        let unchanged = it.synced.as_ref().map(|s| self.rules(d).same(s, &now)).unwrap_or(false);
        if unchanged {
            if let Some(copy) = self.provider.find_deleted_copy(d, p, &fp)? {
                if self.deleted_copy_matches(d, &copy.content, &now).await {
                    let (dir, name) = self.stage_path(d)?;
                    let staged = dir.join(name);
                    self.provider.take_deleted_copy(&copy, &staged)?;
                    let mut meta_item = it.clone();
                    meta_item.remote = Some(now.clone());
                    match self
                        .provider
                        .commit_download(d, p, &staged, &meta_item.meta(), LocalStatus::Placeholder)
                    {
                        Ok(new_fp) => {
                            let mut it = it;
                            it.remote = Some(now.clone());
                            it.synced = Some(now);
                            it.local_base = Some(new_fp);
                            it.local_state = LocalState::Downloaded;
                            it.dirty = false;
                            self.db.upsert_item(&it)?;
                            self.db.log_activity(
                                Some(d.id),
                                Some(p),
                                "trash",
                                &format!("Restored {p} from the Recycle Bin"),
                                false,
                            );
                            return Ok(Outcome::Done);
                        }
                        Err(e) => {
                            // Never lose the copy just taken back: keep it with
                            // the recovered files, then download as usual.
                            let keep = self.recovered_dir.join(&d.display_name).join(p.as_str());
                            if let Some(parent) = keep.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::rename(&staged, &keep).or_else(|_| std::fs::copy(&staged, &keep).map(|_| ()))?;
                            let _ = std::fs::remove_file(&staged);
                            self.db.log_activity(
                                Some(d.id),
                                Some(p),
                                "trash",
                                &format!(
                                    "Could not put {p} back from the Recycle Bin ({e}); kept it in {} and downloading it",
                                    keep.display()
                                ),
                                true,
                            );
                        }
                    }
                }
            }
        }
        self.reconcile(d, p, Want::Download).await
    }

    /// Whether a deleted copy is the cloud version `now`: same size, and
    /// the same hash when the drive compares hashes. Any doubt: no.
    async fn deleted_copy_matches(&self, d: &Drive, content: &std::path::Path, now: &RemoteMeta) -> bool {
        let Ok(m) = std::fs::metadata(content) else {
            return false;
        };
        if now.size >= 0 && m.len() as i64 != now.size {
            return false;
        }
        let (Some(ht), Some(want)) = (&d.hash_type, &now.hash) else {
            return true;
        };
        let (Some(dir), Some(name)) = (content.parent(), content.file_name()) else {
            return false;
        };
        match self
            .api
            .hashsum_file(&dir.display().to_string(), &name.to_string_lossy(), ht)
            .await
        {
            Ok(Some(h)) => h.eq_ignore_ascii_case(want),
            _ => false,
        }
    }

    fn finish_trash_rows(&self, d: &Drive, t: &TrashEntry) -> Result<()> {
        // Rows stay only if the entry was restored.
        let st = self.db.trash_entry(t.id)?.map(|e| e.status).unwrap_or_default();
        if st == "emptied" || st == "gone" {
            self.db.delete_subtree(d.id, &t.path)?;
        }
        Ok(())
    }
}
