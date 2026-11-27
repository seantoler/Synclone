//! The remote index: a breadth-first crawler that lists one folder at a
//! time and diffs each listing against the state DB (PLAN.md §4.2, §4.3).
//!
//! Memory stays bounded by the largest single folder plus the queue of
//! folder paths still to list. Deletions are applied only after the whole
//! poll succeeded: a failed listing keeps the cache and deletes nothing.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use synclone_rclone::{ErrorKind, ListOpts, RcloneError, RemoteEntry};

use super::Engine;
use crate::compare::{remote_meta, VersionRules};
use crate::db::{Db, Item};
use crate::names::{case_collisions, unsyncable_reason};
use crate::types::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollReport {
    pub listed_dirs: usize,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub renamed: Vec<(RelPath, RelPath)>,
    pub unsyncable: usize,
}

impl PollReport {
    pub fn changes(&self) -> usize {
        self.added + self.modified + self.deleted + self.renamed.len()
    }
}

/// What happened to one item in a poll; the sync layer decides what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteChange {
    Added,
    Modified,
    Deleted,
    Renamed { from: RelPath },
}

struct DirTask {
    path: RelPath,
    /// The folder or one above it is pinned ("save to device").
    pinned: bool,
}

impl Engine {
    fn list_opts(&self, d: &Drive) -> ListOpts {
        ListOpts {
            hash_types: d.hash_type.iter().cloned().collect(),
            ..Default::default()
        }
    }

    /// Path relative to the drive for an rclone listing entry.
    fn rel_of(&self, d: &Drive, e: &RemoteEntry) -> Option<RelPath> {
        let root = d.root_path.trim_matches('/');
        let p = e.path.trim_matches('/');
        let rel = if root.is_empty() {
            p
        } else {
            p.strip_prefix(root)?.trim_start_matches('/')
        };
        RelPath::new(rel).filter(|r| !r.is_root())
    }

    fn is_pinned_path(&self, d: DriveId, p: &RelPath) -> Result<bool> {
        if let Some(it) = self.db.item(d, p)? {
            if it.pinned {
                return Ok(true);
            }
        }
        for a in p.ancestors() {
            if let Some(it) = self.db.item(d, &a)? {
                if it.pinned {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// List one folder live and update the index (used for folders the
    /// user opens before the crawler gets there). Returns child folders.
    pub async fn index_dir(&self, drive: &Drive, dir: &RelPath) -> Result<PollReport> {
        let mut report = PollReport::default();
        let mut missing = Vec::new();
        let pinned = self.is_pinned_path(drive.id, dir)?;
        self.list_and_diff(
            drive,
            &DirTask {
                path: dir.clone(),
                pinned,
            },
            &mut report,
            &mut missing,
        )
        .await?;
        self.apply_missing(drive, missing, &mut report).await?;
        Ok(report)
    }

    /// Crawl the whole drive and apply every difference (one poll).
    pub async fn poll_drive(&self, drive_id: DriveId) -> Result<PollReport> {
        let drive = self.drive(drive_id)?;
        if drive.state != DriveState::Connected {
            return Ok(PollReport::default());
        }
        self.db.ensure_root(drive.id)?;
        let mut report = PollReport::default();
        let mut missing: Vec<Item> = Vec::new();
        let mut queue = VecDeque::new();
        let root_pinned = self.is_pinned_path(drive.id, &RelPath::root())?;
        queue.push_back(DirTask {
            path: RelPath::root(),
            pinned: root_pinned,
        });
        while let Some(task) = queue.pop_front() {
            let subdirs = self.list_and_diff(&drive, &task, &mut report, &mut missing).await?;
            for (p, pinned) in subdirs {
                queue.push_back(DirTask { path: p, pinned });
            }
        }
        self.apply_missing(&drive, missing, &mut report).await?;
        self.db.set_last_poll(drive.id, self.now())?;
        if report.changes() > 0 {
            self.db.log_activity(
                Some(drive.id),
                None,
                "poll",
                &format!(
                    "{}: {} new, {} changed, {} removed, {} renamed in the cloud",
                    drive.display_name,
                    report.added,
                    report.modified,
                    report.deleted,
                    report.renamed.len()
                ),
                false,
            );
        }
        Ok(report)
    }

    /// List `task.path`, diff it against the DB, write the changes, and
    /// return its subfolders (with inherited pin state). Items missing from
    /// the listing are appended to `missing` (applied at the end).
    async fn list_and_diff(
        &self,
        drive: &Drive,
        task: &DirTask,
        report: &mut PollReport,
        missing: &mut Vec<Item>,
    ) -> Result<Vec<(RelPath, bool)>> {
        let fs = drive.fs();
        let remote = drive.remote_path(&task.path);
        let entries = match self.api.list(&fs, &remote, &self.list_opts(drive)).await {
            Ok(e) => e,
            // The folder vanished since its parent was listed: the parent's
            // next listing reports it.
            Err(e) if e.kind == ErrorKind::NotFound && !task.path.is_root() => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        report.listed_dirs += 1;
        let rules = VersionRules::for_drive(drive);
        let name_rules = self.provider.name_rules();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        let collisions: HashSet<String> = if name_rules.case_insensitive {
            case_collisions(names.iter().copied()).into_iter().collect()
        } else {
            HashSet::new()
        };
        let listed_names: HashSet<String> = entries.iter().map(|e| e.name.clone()).collect();

        let mut subdirs = Vec::new();
        let mut placeholders: Vec<ItemMeta> = Vec::new();
        let mut changed: Vec<(RelPath, RemoteChange)> = Vec::new();
        let mut rename_candidates: Vec<(RelPath, crate::types::RemoteMeta, Item)> = Vec::new();
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            let known: HashMap<String, Item> = Db::children_on(&tx, drive.id, &task.path)?
                .into_iter()
                .map(|i| (i.path.name().to_string(), i))
                .collect();
            let mut seen: HashSet<String> = HashSet::new();
            for e in &entries {
                let Some(rel) = self.rel_of(drive, e) else { continue };
                if rel.parent().as_ref() != Some(&task.path) {
                    continue;
                }
                let meta = remote_meta(e, drive.hash_type.as_deref());
                let reason = unsyncable_reason(&e.name, name_rules).or_else(|| {
                    collisions
                        .contains(&e.name)
                        .then(|| "another item differs from this name only in letter case".to_string())
                });
                seen.insert(e.name.clone());
                match known.get(&e.name) {
                    Some(k) if k.is_dir == meta.is_dir => {
                        let mut it = k.clone();
                        let was_absent = k.remote.is_none();
                        let differs = match &k.remote {
                            Some(r) => !rules.same(r, &meta) || r.id != meta.id,
                            None => true,
                        };
                        if differs || it.unsyncable != reason {
                            it.remote = Some(meta.clone());
                            it.unsyncable = reason.clone();
                            Db::upsert_item_on(&tx, &it)?;
                            if differs && !meta.is_dir {
                                report.modified += 1;
                                changed.push((
                                    rel.clone(),
                                    if was_absent {
                                        RemoteChange::Added
                                    } else {
                                        RemoteChange::Modified
                                    },
                                ));
                                if it.local_state == LocalState::Placeholder && it.unsyncable.is_none() {
                                    placeholders.push(it.meta());
                                }
                            }
                        }
                        if meta.is_dir && it.unsyncable.is_none() && it.local_state != LocalState::Trashed {
                            subdirs.push((rel.clone(), task.pinned || it.pinned));
                        }
                    }
                    other => {
                        if let Some(k) = other {
                            // File became folder or the other way round.
                            missing.push(k.clone());
                        }
                        // A new path. Maybe a known item moved here (same ID)?
                        if let Some(id) = meta.id.as_deref() {
                            if let Some(old) = Db::item_by_remote_id_on(&tx, drive.id, id)? {
                                if old.path != rel && old.is_dir == meta.is_dir {
                                    rename_candidates.push((rel.clone(), meta.clone(), old));
                                    continue;
                                }
                            }
                        }
                        let mut it = Item::new_remote(drive.id, rel.clone(), meta.clone());
                        it.unsyncable = reason.clone();
                        Db::upsert_item_on(&tx, &it)?;
                        report.added += 1;
                        if reason.is_some() {
                            report.unsyncable += 1;
                        } else {
                            placeholders.push(it.meta());
                            changed.push((rel.clone(), RemoteChange::Added));
                            if meta.is_dir {
                                subdirs.push((rel.clone(), task.pinned));
                            }
                        }
                    }
                }
            }
            for (name, k) in &known {
                if !seen.contains(name) && k.remote.is_some() {
                    missing.push(k.clone());
                }
            }
            Db::set_listed_at_on(&tx, drive.id, &task.path, self.now())?;
            tx.commit()?;
        }

        // Renames by ID: only if the old path is really gone remotely
        // (Drive lets one file have several parents).
        for (new_path, meta, old) in rename_candidates {
            let old_parent = old.path.parent().unwrap_or_default();
            let old_gone = if old_parent == task.path {
                !listed_names.contains(old.path.name())
            } else {
                match self
                    .api
                    .stat(&drive.fs(), &drive.remote_path(&old.path), &ListOpts::default())
                    .await
                {
                    Ok(None) => true,
                    Ok(Some(e)) => e.id.as_deref() != meta.id.as_deref(),
                    Err(e) => return Err(e.into()),
                }
            };
            if old_gone {
                self.db.move_subtree(drive.id, &old.path, &new_path)?;
                let mut it = self.db.item(drive.id, &new_path)?.unwrap_or(old.clone());
                let content_changed = it.remote.as_ref().map(|r| !rules.same(r, &meta)).unwrap_or(true);
                it.remote = Some(meta.clone());
                self.db.upsert_item(&it)?;
                // The old path may already be in `missing` (its folder was listed first).
                missing.retain(|m| {
                    !(m.path == old.path && m.remote.as_ref().and_then(|r| r.id.as_deref()) == meta.id.as_deref())
                });
                report.renamed.push((old.path.clone(), new_path.clone()));
                changed.push((new_path.clone(), RemoteChange::Renamed { from: old.path.clone() }));
                if content_changed && !meta.is_dir {
                    changed.push((new_path.clone(), RemoteChange::Modified));
                }
                if meta.is_dir {
                    subdirs.push((new_path, task.pinned || it.pinned));
                }
            } else {
                let it = Item::new_remote(drive.id, new_path.clone(), meta.clone());
                self.db.upsert_item(&it)?;
                report.added += 1;
                placeholders.push(it.meta());
                changed.push((new_path.clone(), RemoteChange::Added));
                if meta.is_dir {
                    subdirs.push((new_path, task.pinned));
                }
            }
        }

        if !placeholders.is_empty() && self.provider.is_populated(drive, &task.path) {
            if let Err(e) = self.provider.upsert_placeholders(drive, &task.path, &placeholders) {
                tracing::warn!("placeholders in {}: {e}", task.path);
            }
        }
        for (p, c) in changed {
            self.on_remote_change(drive, &p, &c, task.pinned)?;
        }
        Ok(subdirs)
    }

    /// Items missing from their folder's listing: deleted remotely.
    async fn apply_missing(&self, drive: &Drive, missing: Vec<Item>, report: &mut PollReport) -> Result<()> {
        for m in missing {
            // Skip if it was renamed away or re-created meanwhile.
            let Some(cur) = self.db.item(drive.id, &m.path)? else {
                continue;
            };
            if cur.remote.is_none()
                || cur.remote.as_ref().and_then(|r| r.id.clone()) != m.remote.as_ref().and_then(|r| r.id.clone())
            {
                continue;
            }
            if cur.remote.as_ref().map(|r| r.is_dir) != m.remote.as_ref().map(|r| r.is_dir) {
                continue;
            }
            report.deleted += 1;
            self.on_remote_deleted(drive, &cur)?;
        }
        Ok(())
    }
}

impl Db {
    pub fn item_by_remote_id_on(c: &rusqlite::Connection, drive: DriveId, id: &str) -> rusqlite::Result<Option<Item>> {
        use rusqlite::OptionalExtension;
        let mut st = c.prepare_cached(
            "SELECT drive_id, path, is_dir, remote_id, remote_size, remote_mtime, remote_hash, local_state, pinned, synced_size, synced_mtime, synced_hash, local_size, local_mtime, dirty, unsyncable FROM items INDEXED BY items_remote_id WHERE drive_id = ?1 AND remote_id = ?2 AND remote_size IS NOT NULL LIMIT 1",
        )?;
        st.query_row(rusqlite::params![drive, id], crate::db::item_row)
            .optional()
    }
}

/// Classify a listing error for callers.
pub fn is_rate_limited(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RcloneError>()
        .map(|r| r.kind == ErrorKind::RateLimited)
        .unwrap_or(false)
}
