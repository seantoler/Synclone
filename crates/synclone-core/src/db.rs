//! The state database (SQLite, WAL). Schema per PLAN.md §2.6, with the
//! additions listed in DECISIONS.md D-011.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::types::*;

pub type DbResult<T> = rusqlite::Result<T>;

/// Migrations, applied in order; `PRAGMA user_version` records how many ran.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE drives (
        id INTEGER PRIMARY KEY,
        remote_name TEXT NOT NULL UNIQUE,
        display_name TEXT NOT NULL,
        root_path TEXT NOT NULL DEFAULT '',
        provider_type TEXT NOT NULL DEFAULT '',
        added_at INTEGER NOT NULL,
        paused INTEGER NOT NULL DEFAULT 0,
        state TEXT NOT NULL DEFAULT 'connected',
        imported INTEGER NOT NULL DEFAULT 0,
        hash_type TEXT,
        time_tolerance_ns INTEGER NOT NULL DEFAULT 1000000000,
        keeps_mod_times INTEGER NOT NULL DEFAULT 1,
        poll_interval_secs INTEGER NOT NULL DEFAULT 300,
        last_poll_at INTEGER,
        extra_options_json TEXT NOT NULL DEFAULT '{}'
    );
    CREATE TABLE items (
        drive_id INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
        path TEXT NOT NULL,
        parent_path TEXT NOT NULL,
        is_dir INTEGER NOT NULL,
        remote_id TEXT,
        remote_size INTEGER,
        remote_mtime INTEGER,
        remote_hash TEXT,
        local_state TEXT NOT NULL DEFAULT 'placeholder',
        pinned INTEGER NOT NULL DEFAULT 0,
        synced_size INTEGER,
        synced_mtime INTEGER,
        synced_hash TEXT,
        local_size INTEGER,
        local_mtime INTEGER,
        dirty INTEGER NOT NULL DEFAULT 0,
        last_seen_remote INTEGER,
        unsyncable TEXT,
        listed_at INTEGER,
        PRIMARY KEY (drive_id, path)
    ) WITHOUT ROWID;
    CREATE INDEX items_parent ON items(drive_id, parent_path);
    CREATE INDEX items_remote_id ON items(drive_id, remote_id);
    CREATE TABLE trash (
        id INTEGER PRIMARY KEY,
        drive_id INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
        path TEXT NOT NULL,
        is_dir INTEGER NOT NULL,
        size INTEGER NOT NULL DEFAULT 0,
        deleted_at INTEGER NOT NULL,
        remote_snapshot_json TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        note TEXT
    );
    CREATE INDEX trash_pending ON trash(drive_id, path) WHERE status = 'pending';
    CREATE TABLE ops (
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL,
        drive_id INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
        path TEXT NOT NULL,
        payload_json TEXT NOT NULL DEFAULT '{}',
        state TEXT NOT NULL DEFAULT 'pending',
        attempts INTEGER NOT NULL DEFAULT 0,
        last_error TEXT,
        created_at INTEGER NOT NULL,
        not_before INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX ops_due ON ops(state, not_before);
    CREATE INDEX ops_path ON ops(drive_id, path);
    CREATE TABLE conflicts (
        id INTEGER PRIMARY KEY,
        drive_id INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
        path TEXT NOT NULL,
        conflict_copy_path TEXT NOT NULL,
        detected_at INTEGER NOT NULL,
        resolved INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE activity (
        id INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        drive_id INTEGER,
        path TEXT,
        kind TEXT NOT NULL,
        message TEXT NOT NULL,
        is_error INTEGER NOT NULL DEFAULT 0
    );
    "#,
];

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// One row of `items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub drive_id: DriveId,
    pub path: RelPath,
    pub is_dir: bool,
    /// Last seen on the remote.
    pub remote: Option<RemoteMeta>,
    pub local_state: LocalState,
    pub pinned: bool,
    /// Remote version at the last sync (the conflict baseline). `None`
    /// means the item has never been in sync with the remote.
    pub synced: Option<RemoteMeta>,
    /// Local (size, mtime) at the last sync.
    pub local_base: Option<LocalFingerprint>,
    pub dirty: bool,
    pub unsyncable: Option<String>,
}

impl Item {
    pub fn new_remote(drive_id: DriveId, path: RelPath, meta: RemoteMeta) -> Self {
        Self {
            drive_id,
            is_dir: meta.is_dir,
            path,
            remote: Some(meta),
            local_state: LocalState::Placeholder,
            pinned: false,
            synced: None,
            local_base: None,
            dirty: false,
            unsyncable: None,
        }
    }

    pub fn meta(&self) -> ItemMeta {
        let r = self.remote.as_ref().or(self.synced.as_ref());
        ItemMeta {
            path: self.path.clone(),
            is_dir: self.is_dir,
            size: r.map(|r| r.size).unwrap_or(0),
            mtime_ns: r.and_then(|r| r.mtime_ns),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashEntry {
    pub id: i64,
    pub drive_id: DriveId,
    pub path: RelPath,
    pub is_dir: bool,
    pub size: i64,
    pub deleted_at: i64,
    /// Remote metadata at the time of the local delete; for folders, every
    /// item beneath (path → meta).
    pub snapshot: TrashSnapshot,
    pub status: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrashSnapshot {
    pub items: Vec<(RelPath, RemoteMeta)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub id: i64,
    pub kind: String,
    pub drive_id: DriveId,
    pub path: RelPath,
    pub payload: serde_json::Value,
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub not_before: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: i64,
    pub drive_id: DriveId,
    pub path: RelPath,
    pub conflict_copy_path: RelPath,
    pub detected_at: i64,
    pub resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub id: i64,
    pub at: i64,
    pub drive_id: Option<DriveId>,
    pub path: Option<String>,
    pub kind: String,
    pub message: String,
    pub is_error: bool,
}

pub struct Db {
    conn: Mutex<Connection>,
}

fn remote_from(
    size: Option<i64>,
    mtime: Option<i64>,
    hash: Option<String>,
    id: Option<String>,
    is_dir: bool,
) -> Option<RemoteMeta> {
    size.map(|size| RemoteMeta {
        is_dir,
        size,
        mtime_ns: mtime,
        hash,
        id,
    })
}

const ITEM_COLS: &str = "drive_id, path, is_dir, remote_id, remote_size, remote_mtime, remote_hash, local_state, pinned, synced_size, synced_mtime, synced_hash, local_size, local_mtime, dirty, unsyncable";

pub(crate) fn item_row(r: &Row) -> DbResult<Item> {
    item_from_row(r)
}

fn item_from_row(r: &Row) -> DbResult<Item> {
    let is_dir: bool = r.get(2)?;
    let remote_id: Option<String> = r.get(3)?;
    let local_size: Option<i64> = r.get(12)?;
    let local_mtime: Option<i64> = r.get(13)?;
    Ok(Item {
        drive_id: r.get(0)?,
        path: RelPath::from_normalized(r.get(1)?),
        is_dir,
        remote: remote_from(r.get(4)?, r.get(5)?, r.get(6)?, remote_id.clone(), is_dir),
        local_state: LocalState::parse(&r.get::<_, String>(7)?),
        pinned: r.get(8)?,
        synced: remote_from(r.get(9)?, r.get(10)?, r.get(11)?, remote_id, is_dir),
        local_base: match (local_size, local_mtime) {
            (Some(size), Some(mtime_ns)) => Some(LocalFingerprint { size, mtime_ns }),
            _ => None,
        },
        dirty: r.get(14)?,
        unsyncable: r.get(15)?,
    })
}

const DRIVE_COLS: &str = "id, remote_name, display_name, root_path, provider_type, added_at, paused, state, imported, hash_type, time_tolerance_ns, keeps_mod_times, poll_interval_secs";

fn drive_from_row(r: &Row) -> DbResult<Drive> {
    Ok(Drive {
        id: r.get(0)?,
        remote_name: r.get(1)?,
        display_name: r.get(2)?,
        root_path: r.get(3)?,
        provider_type: r.get(4)?,
        added_at: r.get(5)?,
        paused: r.get(6)?,
        state: DriveState::parse(&r.get::<_, String>(7)?),
        imported: r.get(8)?,
        hash_type: r.get(9)?,
        time_tolerance_ns: r.get(10)?,
        keeps_mod_times: r.get(11)?,
        poll_interval_secs: r.get(12)?,
    })
}

const OP_COLS: &str = "id, kind, drive_id, path, payload_json, state, attempts, last_error, created_at, not_before";

fn op_from_row(r: &Row) -> DbResult<Op> {
    let payload: String = r.get(4)?;
    Ok(Op {
        id: r.get(0)?,
        kind: r.get(1)?,
        drive_id: r.get(2)?,
        path: RelPath::from_normalized(r.get(3)?),
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        state: r.get(5)?,
        attempts: r.get(6)?,
        last_error: r.get(7)?,
        created_at: r.get(8)?,
        not_before: r.get(9)?,
    })
}

fn trash_from_row(r: &Row) -> DbResult<TrashEntry> {
    let snap: String = r.get(6)?;
    Ok(TrashEntry {
        id: r.get(0)?,
        drive_id: r.get(1)?,
        path: RelPath::from_normalized(r.get(2)?),
        is_dir: r.get(3)?,
        size: r.get(4)?,
        deleted_at: r.get(5)?,
        snapshot: serde_json::from_str(&snap).unwrap_or_default(),
        status: r.get(7)?,
        note: r.get(8)?,
    })
}

/// SQL condition "path is `?n` or below it", without LIKE (names may contain `%` or `_`).
fn subtree_sql(col: &str, n: usize) -> String {
    format!("({col} = ?{n} OR ?{n} = '' OR substr({col}, 1, length(?{n}) + 1) = ?{n} || '/')")
}

impl Db {
    pub fn open(path: &Path) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> DbResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> DbResult<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> DbResult<()> {
        let mut c = self.lock();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(v as usize) {
            let tx = c.transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> i64 {
        self.lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---------------- settings ----------------

    pub fn setting(&self, key: &str) -> DbResult<Option<String>> {
        self.lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> DbResult<()> {
        self.lock().execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---------------- drives ----------------

    pub fn insert_drive(&self, d: &Drive) -> DbResult<DriveId> {
        let c = self.lock();
        c.execute(
            "INSERT INTO drives (remote_name, display_name, root_path, provider_type, added_at, paused, state, imported, hash_type, time_tolerance_ns, keeps_mod_times, poll_interval_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                d.remote_name,
                d.display_name,
                d.root_path,
                d.provider_type,
                d.added_at,
                d.paused,
                d.state.as_str(),
                d.imported,
                d.hash_type,
                d.time_tolerance_ns,
                d.keeps_mod_times,
                d.poll_interval_secs
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_drive(&self, d: &Drive) -> DbResult<()> {
        self.lock().execute(
            "UPDATE drives SET display_name=?2, root_path=?3, provider_type=?4, paused=?5, state=?6, imported=?7, hash_type=?8, time_tolerance_ns=?9, keeps_mod_times=?10, poll_interval_secs=?11 WHERE id=?1",
            params![
                d.id,
                d.display_name,
                d.root_path,
                d.provider_type,
                d.paused,
                d.state.as_str(),
                d.imported,
                d.hash_type,
                d.time_tolerance_ns,
                d.keeps_mod_times,
                d.poll_interval_secs
            ],
        )?;
        Ok(())
    }

    pub fn drives(&self) -> DbResult<Vec<Drive>> {
        let c = self.lock();
        let mut st = c.prepare(&format!("SELECT {DRIVE_COLS} FROM drives ORDER BY id"))?;
        let rows = st.query_map([], drive_from_row)?;
        rows.collect()
    }

    pub fn drive(&self, id: DriveId) -> DbResult<Option<Drive>> {
        self.lock()
            .query_row(
                &format!("SELECT {DRIVE_COLS} FROM drives WHERE id = ?1"),
                [id],
                drive_from_row,
            )
            .optional()
    }

    pub fn drive_by_remote(&self, remote: &str) -> DbResult<Option<Drive>> {
        self.lock()
            .query_row(
                &format!("SELECT {DRIVE_COLS} FROM drives WHERE remote_name = ?1"),
                [remote],
                drive_from_row,
            )
            .optional()
    }

    pub fn delete_drive(&self, id: DriveId) -> DbResult<()> {
        self.lock().execute("DELETE FROM drives WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Forget everything Synclone knew about a drive's files (on disconnect).
    pub fn clear_drive_state(&self, id: DriveId) -> DbResult<()> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        for t in ["items", "trash", "ops", "conflicts"] {
            tx.execute(&format!("DELETE FROM {t} WHERE drive_id = ?1"), [id])?;
        }
        tx.commit()
    }

    pub fn set_last_poll(&self, id: DriveId, at: i64) -> DbResult<()> {
        self.lock()
            .execute("UPDATE drives SET last_poll_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    pub fn last_poll(&self, id: DriveId) -> DbResult<Option<i64>> {
        self.lock()
            .query_row("SELECT last_poll_at FROM drives WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map(|o| o.flatten())
    }

    // ---------------- items ----------------

    pub fn item(&self, drive: DriveId, path: &RelPath) -> DbResult<Option<Item>> {
        self.lock()
            .query_row(
                &format!("SELECT {ITEM_COLS} FROM items WHERE drive_id = ?1 AND path = ?2"),
                params![drive, path.as_str()],
                item_from_row,
            )
            .optional()
    }

    pub fn upsert_item(&self, it: &Item) -> DbResult<()> {
        let c = self.lock();
        Self::upsert_item_on(&c, it)
    }

    pub fn upsert_item_on(c: &Connection, it: &Item) -> DbResult<()> {
        let parent = it.path.parent().map(|p| p.as_str().to_string()).unwrap_or_default();
        let r = it.remote.as_ref();
        let s = it.synced.as_ref();
        let id = r.and_then(|r| r.id.clone()).or_else(|| s.and_then(|s| s.id.clone()));
        c.execute(
            "INSERT INTO items (drive_id, path, parent_path, is_dir, remote_id, remote_size, remote_mtime, remote_hash, local_state, pinned, synced_size, synced_mtime, synced_hash, local_size, local_mtime, dirty, unsyncable, last_seen_remote)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(drive_id, path) DO UPDATE SET
               is_dir=excluded.is_dir, remote_id=excluded.remote_id, remote_size=excluded.remote_size, remote_mtime=excluded.remote_mtime,
               remote_hash=excluded.remote_hash, local_state=excluded.local_state, pinned=excluded.pinned, synced_size=excluded.synced_size,
               synced_mtime=excluded.synced_mtime, synced_hash=excluded.synced_hash, local_size=excluded.local_size, local_mtime=excluded.local_mtime,
               dirty=excluded.dirty, unsyncable=excluded.unsyncable, last_seen_remote=excluded.last_seen_remote",
            params![
                it.drive_id,
                it.path.as_str(),
                parent,
                it.is_dir,
                id,
                r.map(|r| r.size),
                r.and_then(|r| r.mtime_ns),
                r.and_then(|r| r.hash.clone()),
                it.local_state.as_str(),
                it.pinned,
                s.map(|s| s.size),
                s.and_then(|s| s.mtime_ns),
                s.and_then(|s| s.hash.clone()),
                it.local_base.map(|l| l.size),
                it.local_base.map(|l| l.mtime_ns),
                it.dirty,
                it.unsyncable,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn delete_item(&self, drive: DriveId, path: &RelPath) -> DbResult<()> {
        self.lock().execute(
            "DELETE FROM items WHERE drive_id = ?1 AND path = ?2",
            params![drive, path.as_str()],
        )?;
        Ok(())
    }

    /// Delete an item and everything beneath it.
    pub fn delete_subtree(&self, drive: DriveId, path: &RelPath) -> DbResult<usize> {
        let c = self.lock();
        Self::delete_subtree_on(&c, drive, path)
    }

    pub fn delete_subtree_on(c: &Connection, drive: DriveId, path: &RelPath) -> DbResult<usize> {
        c.execute(
            &format!("DELETE FROM items WHERE drive_id = ?1 AND {}", subtree_sql("path", 2)),
            params![drive, path.as_str()],
        )
    }

    /// Move an item and its subtree to a new path (a rename).
    pub fn move_subtree(&self, drive: DriveId, from: &RelPath, to: &RelPath) -> DbResult<usize> {
        let c = self.lock();
        Self::move_subtree_on(&c, drive, from, to)
    }

    pub fn move_subtree_on(c: &Connection, drive: DriveId, from: &RelPath, to: &RelPath) -> DbResult<usize> {
        // Anything already at the destination is replaced.
        c.execute(
            &format!("DELETE FROM items WHERE drive_id = ?1 AND {}", subtree_sql("path", 2)),
            params![drive, to.as_str()],
        )?;
        let n = c.execute(
            "UPDATE items SET path = ?3 || substr(path, length(?2) + 1),
                              parent_path = CASE WHEN path = ?2 THEN ?4 ELSE ?3 || substr(parent_path, length(?2) + 1) END
             WHERE drive_id = ?1 AND (path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')",
            params![
                drive,
                from.as_str(),
                to.as_str(),
                to.parent().map(|p| p.as_str().to_string()).unwrap_or_default()
            ],
        )?;
        Ok(n)
    }

    /// Children of a folder, ordered by name. This answers the OS's folder
    /// listing requests, so it must be fast (index on parent_path).
    pub fn children(&self, drive: DriveId, parent: &RelPath) -> DbResult<Vec<Item>> {
        let c = self.lock();
        Self::children_on(&c, drive, parent)
    }

    pub fn children_on(c: &Connection, drive: DriveId, parent: &RelPath) -> DbResult<Vec<Item>> {
        let mut st = c.prepare_cached(&format!(
            "SELECT {ITEM_COLS} FROM items WHERE drive_id = ?1 AND parent_path = ?2 AND path != '' ORDER BY path"
        ))?;
        let rows = st.query_map(params![drive, parent.as_str()], item_from_row)?;
        rows.collect()
    }

    /// Every item at or below `path`.
    pub fn subtree(&self, drive: DriveId, path: &RelPath) -> DbResult<Vec<Item>> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {ITEM_COLS} FROM items WHERE drive_id = ?1 AND {} AND path != '' ORDER BY path",
            subtree_sql("path", 2)
        ))?;
        let rows = st.query_map(params![drive, path.as_str()], item_from_row)?;
        rows.collect()
    }

    pub fn item_by_remote_id(&self, drive: DriveId, id: &str) -> DbResult<Option<Item>> {
        self.lock()
            .query_row(
                &format!("SELECT {ITEM_COLS} FROM items INDEXED BY items_remote_id WHERE drive_id = ?1 AND remote_id = ?2 LIMIT 1"),
                params![drive, id],
                item_from_row,
            )
            .optional()
    }

    /// When a folder was last listed from the remote (None: never).
    pub fn listed_at(&self, drive: DriveId, dir: &RelPath) -> DbResult<Option<i64>> {
        self.lock()
            .query_row(
                "SELECT listed_at FROM items WHERE drive_id = ?1 AND path = ?2",
                params![drive, dir.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map(|o| o.flatten())
    }

    pub fn set_listed_at_on(c: &Connection, drive: DriveId, dir: &RelPath, at: i64) -> DbResult<()> {
        c.execute(
            "UPDATE items SET listed_at = ?3 WHERE drive_id = ?1 AND path = ?2",
            params![drive, dir.as_str(), at],
        )?;
        Ok(())
    }

    /// Make sure the drive's root folder has a row (it carries `pinned` and `listed_at`).
    pub fn ensure_root(&self, drive: DriveId) -> DbResult<()> {
        self.lock().execute(
            "INSERT OR IGNORE INTO items (drive_id, path, parent_path, is_dir, local_state, remote_size) VALUES (?1, '', '', 1, 'downloaded', 0)",
            [drive],
        )?;
        Ok(())
    }

    pub fn count_items(&self, drive: DriveId) -> DbResult<i64> {
        self.lock()
            .query_row("SELECT count(*) FROM items WHERE drive_id = ?1", [drive], |r| r.get(0))
    }

    pub fn unsyncable_items(&self, drive: DriveId) -> DbResult<Vec<Item>> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {ITEM_COLS} FROM items WHERE drive_id = ?1 AND unsyncable IS NOT NULL ORDER BY path"
        ))?;
        let rows = st.query_map([drive], item_from_row)?;
        rows.collect()
    }

    /// Items with local changes not yet uploaded.
    pub fn dirty_items(&self, drive: DriveId) -> DbResult<Vec<Item>> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {ITEM_COLS} FROM items WHERE drive_id = ?1 AND dirty = 1 ORDER BY path"
        ))?;
        let rows = st.query_map([drive], item_from_row)?;
        rows.collect()
    }

    // ---------------- trash ----------------

    pub fn insert_trash(&self, t: &TrashEntry) -> DbResult<i64> {
        let c = self.lock();
        c.execute(
            "INSERT INTO trash (drive_id, path, is_dir, size, deleted_at, remote_snapshot_json, status, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                t.drive_id,
                t.path.as_str(),
                t.is_dir,
                t.size,
                t.deleted_at,
                serde_json::to_string(&t.snapshot).unwrap_or_default(),
                t.status,
                t.note
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn trash(&self, status: Option<&str>) -> DbResult<Vec<TrashEntry>> {
        let c = self.lock();
        let mut st = c.prepare(
            "SELECT id, drive_id, path, is_dir, size, deleted_at, remote_snapshot_json, status, note FROM trash WHERE (?1 IS NULL OR status = ?1) ORDER BY id",
        )?;
        let rows = st.query_map([status], trash_from_row)?;
        rows.collect()
    }

    pub fn trash_entry(&self, id: i64) -> DbResult<Option<TrashEntry>> {
        self.lock()
            .query_row(
                "SELECT id, drive_id, path, is_dir, size, deleted_at, remote_snapshot_json, status, note FROM trash WHERE id = ?1",
                [id],
                trash_from_row,
            )
            .optional()
    }

    /// The pending trash entry covering `path` (the path itself or a folder above it).
    pub fn pending_trash_covering(&self, drive: DriveId, path: &RelPath) -> DbResult<Option<TrashEntry>> {
        let c = self.lock();
        let mut st = c.prepare_cached(
            "SELECT id, drive_id, path, is_dir, size, deleted_at, remote_snapshot_json, status, note FROM trash
             WHERE drive_id = ?1 AND status = 'pending' AND (path = ?2 OR path = '' OR substr(?2, 1, length(path) + 1) = path || '/')
             ORDER BY length(path) LIMIT 1",
        )?;
        st.query_row(params![drive, path.as_str()], trash_from_row).optional()
    }

    pub fn set_trash_status(&self, id: i64, status: &str, note: Option<&str>) -> DbResult<()> {
        self.lock().execute(
            "UPDATE trash SET status = ?2, note = ?3 WHERE id = ?1",
            params![id, status, note],
        )?;
        Ok(())
    }

    // ---------------- ops journal ----------------

    /// Add an op. A pending op of the same kind for the same path is
    /// reused (its `not_before` moves to the later time: debounce).
    pub fn enqueue_op(
        &self,
        kind: &str,
        drive: DriveId,
        path: &RelPath,
        payload: &serde_json::Value,
        not_before: i64,
    ) -> DbResult<i64> {
        let c = self.lock();
        let p = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
        if let Some(id) = c
            .query_row(
                "SELECT id FROM ops WHERE kind = ?1 AND drive_id = ?2 AND path = ?3 AND state = 'pending' AND payload_json = ?4",
                params![kind, drive, path.as_str(), p],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            c.execute("UPDATE ops SET not_before = max(not_before, ?2), updated_at = ?3 WHERE id = ?1", params![id, not_before, now_ms()])?;
            return Ok(id);
        }
        c.execute(
            "INSERT INTO ops (kind, drive_id, path, payload_json, state, created_at, not_before, updated_at) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?5)",
            params![kind, drive, path.as_str(), p, now_ms(), not_before],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Pending ops whose time has come, oldest first.
    pub fn due_ops(&self, now: i64, limit: usize) -> DbResult<Vec<Op>> {
        let c = self.lock();
        let mut st = c.prepare_cached(&format!(
            "SELECT {OP_COLS} FROM ops WHERE state = 'pending' AND not_before <= ?1 ORDER BY id LIMIT ?2"
        ))?;
        let rows = st.query_map(params![now, limit as i64], op_from_row)?;
        rows.collect()
    }

    pub fn ops(&self, state: Option<&str>) -> DbResult<Vec<Op>> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {OP_COLS} FROM ops WHERE (?1 IS NULL OR state = ?1) ORDER BY id"
        ))?;
        let rows = st.query_map([state], op_from_row)?;
        rows.collect()
    }

    pub fn ops_for(&self, drive: DriveId) -> DbResult<Vec<Op>> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {OP_COLS} FROM ops WHERE drive_id = ?1 AND state IN ('pending','running') ORDER BY id"
        ))?;
        let rows = st.query_map([drive], op_from_row)?;
        rows.collect()
    }

    /// The earliest `not_before` of any pending op.
    pub fn next_op_time(&self) -> DbResult<Option<i64>> {
        self.lock()
            .query_row("SELECT min(not_before) FROM ops WHERE state = 'pending'", [], |r| {
                r.get(0)
            })
    }

    pub fn mark_op_running(&self, id: i64) -> DbResult<bool> {
        let n = self.lock().execute(
            "UPDATE ops SET state = 'running', attempts = attempts + 1, updated_at = ?2 WHERE id = ?1 AND state = 'pending'",
            params![id, now_ms()],
        )?;
        Ok(n == 1)
    }

    pub fn finish_op(&self, id: i64) -> DbResult<()> {
        // Finished ops are deleted: the journal holds only unfinished work.
        self.lock().execute("DELETE FROM ops WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn retry_op(&self, id: i64, error: &str, not_before: i64) -> DbResult<()> {
        self.lock().execute(
            "UPDATE ops SET state = 'pending', last_error = ?2, not_before = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, error, not_before, now_ms()],
        )?;
        Ok(())
    }

    pub fn fail_op(&self, id: i64, error: &str) -> DbResult<()> {
        self.lock().execute(
            "UPDATE ops SET state = 'failed', last_error = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, error, now_ms()],
        )?;
        Ok(())
    }

    /// After a crash: ops left `running` go back to `pending`.
    pub fn reset_running_ops(&self) -> DbResult<usize> {
        self.lock()
            .execute("UPDATE ops SET state = 'pending' WHERE state = 'running'", [])
    }

    pub fn cancel_ops(&self, drive: DriveId, kind: &str, path: &RelPath) -> DbResult<usize> {
        self.lock().execute(
            "DELETE FROM ops WHERE drive_id = ?1 AND kind = ?2 AND path = ?3 AND state = 'pending'",
            params![drive, kind, path.as_str()],
        )
    }

    // ---------------- conflicts ----------------

    pub fn insert_conflict(&self, drive: DriveId, path: &RelPath, copy: &RelPath) -> DbResult<i64> {
        let c = self.lock();
        c.execute(
            "INSERT INTO conflicts (drive_id, path, conflict_copy_path, detected_at) VALUES (?1, ?2, ?3, ?4)",
            params![drive, path.as_str(), copy.as_str(), now_ms()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn conflicts(&self, include_resolved: bool) -> DbResult<Vec<Conflict>> {
        let c = self.lock();
        let mut st = c.prepare(
            "SELECT id, drive_id, path, conflict_copy_path, detected_at, resolved FROM conflicts WHERE (?1 OR resolved = 0) ORDER BY id",
        )?;
        let rows = st.query_map([include_resolved], |r| {
            Ok(Conflict {
                id: r.get(0)?,
                drive_id: r.get(1)?,
                path: RelPath::from_normalized(r.get(2)?),
                conflict_copy_path: RelPath::from_normalized(r.get(3)?),
                detected_at: r.get(4)?,
                resolved: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn resolve_conflict(&self, id: i64) -> DbResult<()> {
        self.lock()
            .execute("UPDATE conflicts SET resolved = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---------------- activity ----------------

    pub fn log_activity(
        &self,
        drive: Option<DriveId>,
        path: Option<&RelPath>,
        kind: &str,
        message: &str,
        is_error: bool,
    ) {
        let c = self.lock();
        let _ = c.execute(
            "INSERT INTO activity (at, drive_id, path, kind, message, is_error) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now_ms(),
                drive,
                path.map(|p| p.as_str().to_string()),
                kind,
                message,
                is_error
            ],
        );
        // Keep the last 1000 rows; the UI shows 200.
        let _ = c.execute(
            "DELETE FROM activity WHERE id <= (SELECT max(id) FROM activity) - 1000",
            [],
        );
    }

    pub fn activity(&self, limit: usize) -> DbResult<Vec<Activity>> {
        let c = self.lock();
        let mut st = c.prepare(
            "SELECT id, at, drive_id, path, kind, message, is_error FROM activity ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(Activity {
                id: r.get(0)?,
                at: r.get(1)?,
                drive_id: r.get(2)?,
                path: r.get(3)?,
                kind: r.get(4)?,
                message: r.get(5)?,
                is_error: r.get(6)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn test_drive(db: &Db) -> DriveId {
        db.insert_drive(&Drive {
            id: 0,
            remote_name: "r".into(),
            display_name: "R".into(),
            root_path: String::new(),
            provider_type: "memory".into(),
            added_at: 0,
            paused: false,
            state: DriveState::Connected,
            imported: false,
            hash_type: Some("md5".into()),
            time_tolerance_ns: 1_000_000_000,
            keeps_mod_times: true,
            poll_interval_secs: 300,
        })
        .unwrap()
    }

    fn file(d: DriveId, p: &str) -> Item {
        Item::new_remote(
            d,
            RelPath::new(p).unwrap(),
            RemoteMeta {
                is_dir: false,
                size: 1,
                mtime_ns: Some(5),
                hash: None,
                id: Some(format!("id-{p}")),
            },
        )
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.db");
        let db = Db::open(&p).unwrap();
        assert_eq!(db.schema_version(), MIGRATIONS.len() as i64);
        drop(db);
        let db = Db::open(&p).unwrap();
        assert_eq!(db.schema_version(), MIGRATIONS.len() as i64);
        let mode: String = db.lock().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn items_children_and_subtree_moves() {
        let db = Db::open_in_memory().unwrap();
        let d = test_drive(&db);
        for p in ["a", "a/x%y", "a/x%y/f", "a_b", "a/b"] {
            db.upsert_item(&file(d, p)).unwrap();
        }
        let kids: Vec<_> = db
            .children(d, &RelPath::new("a").unwrap())
            .unwrap()
            .into_iter()
            .map(|i| i.path.as_str().to_string())
            .collect();
        assert_eq!(kids, vec!["a/b", "a/x%y"]);
        assert_eq!(db.subtree(d, &RelPath::new("a").unwrap()).unwrap().len(), 4);
        db.move_subtree(d, &RelPath::new("a").unwrap(), &RelPath::new("z/q").unwrap())
            .unwrap();
        assert!(db.item(d, &RelPath::new("z/q/x%y/f").unwrap()).unwrap().is_some());
        assert_eq!(db.children(d, &RelPath::new("z/q/x%y").unwrap()).unwrap().len(), 1);
        assert_eq!(db.children(d, &RelPath::new("z").unwrap()).unwrap().len(), 1);
        assert!(
            db.item(d, &RelPath::new("a_b").unwrap()).unwrap().is_some(),
            "LIKE wildcards are not used"
        );
        assert_eq!(db.delete_subtree(d, &RelPath::new("z/q").unwrap()).unwrap(), 4);
        assert_eq!(db.count_items(d).unwrap(), 1);
        assert!(db.item_by_remote_id(d, "id-a_b").unwrap().is_some());
    }

    #[test]
    fn ops_dedupe_and_crash_reset() {
        let db = Db::open_in_memory().unwrap();
        let d = test_drive(&db);
        let p = RelPath::new("f").unwrap();
        let a = db.enqueue_op("reconcile", d, &p, &serde_json::json!({}), 10).unwrap();
        let b = db.enqueue_op("reconcile", d, &p, &serde_json::json!({}), 20).unwrap();
        assert_eq!(a, b);
        assert!(db.due_ops(15, 10).unwrap().is_empty(), "debounced to 20");
        assert_eq!(db.due_ops(20, 10).unwrap().len(), 1);
        assert!(db.mark_op_running(a).unwrap());
        assert!(!db.mark_op_running(a).unwrap());
        assert_eq!(db.reset_running_ops().unwrap(), 1);
        assert_eq!(db.due_ops(20, 10).unwrap()[0].attempts, 1);
        db.finish_op(a).unwrap();
        assert!(db.ops(None).unwrap().is_empty());
    }

    #[test]
    fn trash_covering_lookup() {
        let db = Db::open_in_memory().unwrap();
        let d = test_drive(&db);
        db.insert_trash(&TrashEntry {
            id: 0,
            drive_id: d,
            path: RelPath::new("dir").unwrap(),
            is_dir: true,
            size: 0,
            deleted_at: 0,
            snapshot: TrashSnapshot::default(),
            status: "pending".into(),
            note: None,
        })
        .unwrap();
        assert!(db
            .pending_trash_covering(d, &RelPath::new("dir/x").unwrap())
            .unwrap()
            .is_some());
        assert!(db
            .pending_trash_covering(d, &RelPath::new("dir").unwrap())
            .unwrap()
            .is_some());
        assert!(db
            .pending_trash_covering(d, &RelPath::new("dirx").unwrap())
            .unwrap()
            .is_none());
    }
}
