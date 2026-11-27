//! The delete queue (PLAN.md §4.6). A local delete never reaches the
//! cloud until the user empties the trash; entries can be restored.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use synclone_rclone::ListOpts;

use super::sync::Outcome;
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
            snapshot: TrashSnapshot { items },
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

    /// Put a trashed item back (as placeholders). No remote calls.
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

    fn finish_trash_rows(&self, d: &Drive, t: &TrashEntry) -> Result<()> {
        // Rows stay only if the entry was restored.
        let st = self.db.trash_entry(t.id)?.map(|e| e.status).unwrap_or_default();
        if st == "emptied" || st == "gone" {
            self.db.delete_subtree(d.id, &t.path)?;
        }
        Ok(())
    }
}
