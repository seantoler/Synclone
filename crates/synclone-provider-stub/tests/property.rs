//! Property test (PLAN.md §7 layer 2): random interleaved operations on the
//! local and remote sides, then run the engine to quiescence and check:
//!
//! **No content is lost.** Every file content a user wrote is still present
//! locally (including the OS trash), remotely, or as a conflict copy —
//! unless that same user later overwrote or deleted it on their own side,
//! or it was removed by an explicitly emptied trash entry.
//!
//! Plus convergence: after quiescence every local file has the same content
//! as the remote file at the same path, and no op is left failed.
//!
//! Case count: `PROPTEST_CASES` (default 10 000, as the Phase 3 exit asks).

mod common;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use common::{bump_mtime, rp, FS};
use proptest::prelude::*;
use synclone_core::db::Db;
use synclone_core::*;
use synclone_provider_stub::StubProvider;
use synclone_rclone::{MockRclone, RcloneApi};

const PATHS: &[&str] = &["a.txt", "b.txt", "d/c.txt", "d/e/f.txt"];

#[derive(Debug, Clone)]
enum Act {
    LocalWrite(usize),
    LocalDelete(usize),
    LocalRename(usize, usize),
    RemoteWrite(usize),
    RemoteDelete(usize),
    RemoteRename(usize, usize),
    Open(usize),
    Free(usize),
    Sync,
    PollOnly,
    ScanOnly,
    EmptyTrash,
    RestoreTrash,
}

fn act() -> impl Strategy<Value = Act> {
    let i = 0..PATHS.len();
    prop_oneof![
        3 => i.clone().prop_map(Act::LocalWrite),
        1 => i.clone().prop_map(Act::LocalDelete),
        1 => (i.clone(), i.clone()).prop_map(|(a, b)| Act::LocalRename(a, b)),
        3 => i.clone().prop_map(Act::RemoteWrite),
        1 => i.clone().prop_map(Act::RemoteDelete),
        1 => (i.clone(), i.clone()).prop_map(|(a, b)| Act::RemoteRename(a, b)),
        2 => i.clone().prop_map(Act::Open),
        1 => i.clone().prop_map(Act::Free),
        3 => Just(Act::Sync),
        1 => Just(Act::PollOnly),
        1 => Just(Act::ScanOnly),
        1 => Just(Act::EmptyTrash),
        1 => Just(Act::RestoreTrash),
    ]
}

struct World {
    _tmp: tempfile::TempDir,
    mock: MockRclone,
    stub: Arc<StubProvider>,
    eng: Arc<Engine>,
    d: Drive,
    next: u64,
    must_exist: HashSet<Vec<u8>>,
    /// What the engine last saw in the cloud (after a poll), per path.
    seen_remote: BTreeMap<String, Vec<u8>>,
    /// Content the user deleted locally at a path (a later write there replaces it).
    deleted_at: BTreeMap<String, Vec<u8>>,
}

impl World {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mock = MockRclone::new();
        mock.add_remote("r");
        let stub = Arc::new(StubProvider::new(tmp.path().join("root"), tmp.path().join("recycle")));
        let eng = Engine::new(
            Arc::new(Db::open_in_memory().unwrap()),
            Arc::new(mock.clone()) as Arc<dyn RcloneApi>,
            stub.clone(),
            Arc::new(TestClock::new(1_000_000)),
            EngineSettings {
                concurrency: 1,
                ..Default::default()
            },
            tmp.path().join("state"),
        );
        let d = eng.connect_remote("r", Some("Cloud")).await.unwrap();
        World {
            _tmp: tmp,
            mock,
            stub,
            eng,
            d,
            next: 0,
            must_exist: HashSet::new(),
            seen_remote: BTreeMap::new(),
            deleted_at: BTreeMap::new(),
        }
    }

    fn content(&mut self) -> Vec<u8> {
        self.next += 1;
        format!("content-{}", self.next).into_bytes()
    }

    fn local_path(&self, p: &str) -> std::path::PathBuf {
        self.stub.path_of(&self.d, &rp(p))
    }

    fn local_content(&self, p: &str) -> Option<Vec<u8>> {
        let path = self.local_path(p);
        if path.is_file() {
            std::fs::read(path).ok()
        } else {
            None
        }
    }

    fn snapshot_remote(&mut self) {
        self.seen_remote = self
            .mock
            .store(FS)
            .files
            .iter()
            .map(|(k, v)| (k.clone(), (*v.data).clone()))
            .collect();
    }

    /// The content a placeholder at `p` stands for, if the cloud still has it.
    fn placeholder_content(&self, p: &str) -> Option<Vec<u8>> {
        // Still in the cloud unchanged (possibly renamed there since).
        let seen = self.seen_remote.get(p)?;
        self.remote_contents().contains(seen).then(|| seen.clone())
    }

    fn is_placeholder(&self, p: &str) -> bool {
        std::path::Path::new(&format!("{}.sclstub", self.local_path(p).display())).is_file()
            && !self.local_path(p).exists()
    }

    fn remote_contents(&self) -> HashSet<Vec<u8>> {
        self.mock.store(FS).files.values().map(|o| (*o.data).clone()).collect()
    }

    fn all_present(&self) -> HashSet<Vec<u8>> {
        let mut s = self.remote_contents();
        let mut stack = vec![self.stub.root().to_path_buf(), self.stub.recycle_dir().to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if !p.to_string_lossy().ends_with(".sclstub") {
                    if let Ok(c) = std::fs::read(&p) {
                        s.insert(c);
                    }
                }
            }
        }
        s
    }

    async fn apply(&mut self, a: &Act) {
        let id = self.d.id;
        match a {
            Act::LocalWrite(i) => {
                let p = PATHS[*i];
                let path = self.local_path(p);
                if path.is_dir() || path.parent().map(|x| x.is_file()).unwrap_or(false) {
                    return;
                }
                // The user replaces what they see at p: the local file, the
                // file a placeholder stands for, or a file they just deleted.
                let prev = self
                    .local_content(p)
                    .or_else(|| {
                        if self.is_placeholder(p) {
                            self.placeholder_content(p)
                        } else {
                            None
                        }
                    })
                    .or_else(|| self.deleted_at.remove(p));
                if let Some(prev) = prev {
                    self.must_exist.remove(&prev);
                }
                let c = self.content();
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &c).unwrap();
                bump_mtime(&path);
                self.must_exist.insert(c);
                let _ = self
                    .eng
                    .handle_event(ProviderEvent::LocalModified { drive: id, path: rp(p) });
            }
            Act::LocalDelete(i) => {
                let p = PATHS[*i];
                let path = self.local_path(p);
                if let Some(c) = self.local_content(p) {
                    // Only content that exists nowhere else is gone by the user's choice.
                    if !self.remote_contents().contains(&c) {
                        self.must_exist.remove(&c);
                    }
                    self.deleted_at.insert(p.to_string(), c);
                    std::fs::remove_file(&path).unwrap();
                } else {
                    let stub = format!("{}.sclstub", path.display());
                    if let Some(c) = self.placeholder_content(p) {
                        self.deleted_at.insert(p.to_string(), c);
                    }
                    if std::fs::remove_file(&stub).is_err() {
                        return;
                    }
                }
                let _ = self
                    .eng
                    .handle_event(ProviderEvent::LocalDeleted { drive: id, path: rp(p) });
            }
            Act::LocalRename(a, b) => {
                let (from, to) = (PATHS[*a], PATHS[*b]);
                if from == to {
                    return;
                }
                let src = self.local_path(from);
                let dst = self.local_path(to);
                let src_stub = std::path::PathBuf::from(format!("{}.sclstub", src.display()));
                let dst_stub = std::path::PathBuf::from(format!("{}.sclstub", dst.display()));
                if dst.exists() || dst_stub.exists() || dst.parent().map(|x| x.is_file()).unwrap_or(false) {
                    return;
                }
                std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
                if src.is_file() || src_stub.is_file() {
                    // Renaming onto a name the user just deleted replaces that file.
                    if let Some(prev) = self.deleted_at.remove(to) {
                        self.must_exist.remove(&prev);
                    }
                }
                if src.is_file() {
                    std::fs::rename(&src, &dst).unwrap();
                } else if src_stub.is_file() {
                    std::fs::rename(&src_stub, &dst_stub).unwrap();
                    // The placeholder still stands for the same cloud file.
                    if let Some(c) = self.seen_remote.remove(from) {
                        self.seen_remote.insert(to.to_string(), c);
                    }
                } else {
                    return;
                }
                let _ = self.eng.handle_event(ProviderEvent::LocalRenamed {
                    drive: id,
                    from: rp(from),
                    to: rp(to),
                });
            }
            Act::RemoteWrite(i) => {
                let p = PATHS[*i];
                if self.mock.store(FS).dirs.contains_key(p) {
                    return;
                }
                let parent_is_file = rp(p)
                    .ancestors()
                    .iter()
                    .any(|a| !a.is_root() && self.mock.get(FS, a.as_str()).is_some());
                if parent_is_file {
                    return;
                }
                if let Some(prev) = self.mock.get(FS, p) {
                    self.must_exist.remove(&prev);
                }
                let c = self.content();
                self.mock.put(FS, p, &c);
                self.must_exist.insert(c);
            }
            Act::RemoteDelete(i) => {
                let p = PATHS[*i];
                if let Some(prev) = self.mock.get(FS, p) {
                    self.must_exist.remove(&prev);
                    self.mock.remove(FS, p);
                }
            }
            Act::RemoteRename(a, b) => {
                let (from, to) = (PATHS[*a], PATHS[*b]);
                if from == to || self.mock.get(FS, from).is_none() || self.mock.exists(FS, to) {
                    return;
                }
                let parent_is_file = rp(to)
                    .ancestors()
                    .iter()
                    .any(|x| !x.is_root() && self.mock.get(FS, x.as_str()).is_some());
                if parent_is_file {
                    return;
                }
                self.mock.rename(FS, from, to);
            }
            Act::Open(i) => {
                let _ = self.eng.handle_event(ProviderEvent::HydrationRequested {
                    drive: id,
                    path: rp(PATHS[*i]),
                });
                self.eng.run_until_idle(true, 20).await.unwrap();
                self.snapshot_remote();
            }
            Act::Free(i) => {
                let _ = self.eng.free(id, &rp(PATHS[*i]), true);
            }
            Act::Sync => {
                self.eng.sync_drive_now(id).await.unwrap();
                self.snapshot_remote();
            }
            Act::PollOnly => {
                self.eng.poll_drive(id).await.unwrap();
                self.snapshot_remote();
            }
            Act::ScanOnly => {
                self.eng.scan_drive(id).unwrap();
            }
            Act::EmptyTrash => {
                // Run pending work first so the trash reflects it.
                self.eng.sync_drive_now(id).await.unwrap();
                let before = self.remote_contents();
                let trashed: HashSet<Vec<u8>> = self
                    .eng
                    .trash_list()
                    .unwrap()
                    .iter()
                    .flat_map(|t| t.snapshot.items.clone())
                    .filter_map(|(p, _)| self.mock.get(FS, p.as_str()))
                    .collect();
                self.eng.request_empty_trash().unwrap();
                self.eng.run_until_idle(true, 20).await.unwrap();
                let after = self.remote_contents();
                for gone in before.difference(&after) {
                    assert!(
                        trashed.contains(gone),
                        "emptying the trash deleted something not in the trash"
                    );
                    self.must_exist.remove(gone);
                }
                self.snapshot_remote();
            }
            Act::RestoreTrash => {
                self.eng.restore_all().unwrap();
                // Only deletes the engine had queued come back.
                let restored: Vec<String> = self
                    .deleted_at
                    .keys()
                    .filter(|p| self.is_placeholder(p) || self.local_path(p).exists())
                    .cloned()
                    .collect();
                for p in restored {
                    self.deleted_at.remove(&p);
                }
            }
        }
    }

    async fn quiesce(&mut self) {
        for _ in 0..4 {
            self.eng.sync_drive_now(self.d.id).await.unwrap();
        }
    }

    fn check(&self, acts: &[Act]) {
        let present = self.all_present();
        for c in &self.must_exist {
            assert!(
                present.contains(c),
                "lost {:?}\nacts: {acts:?}\nremote: {:?}",
                String::from_utf8_lossy(c),
                self.mock.store(FS).files.keys().collect::<Vec<_>>()
            );
        }
        let failed = self.eng.db.ops(Some("failed")).unwrap();
        assert!(failed.is_empty(), "failed ops {failed:?}\nacts: {acts:?}");
        // Convergence: local files equal the remote file at the same path.
        let trashed: Vec<RelPath> = self.eng.trash_list().unwrap().into_iter().map(|t| t.path).collect();
        let remote: BTreeMap<String, Vec<u8>> = self
            .mock
            .store(FS)
            .files
            .iter()
            .map(|(k, v)| (k.clone(), (*v.data).clone()))
            .collect();
        for e in self.stub.scan(&self.d).unwrap() {
            if let LocalStatus::File(_) = e.status {
                let local = self.local_content(e.path.as_str()).unwrap();
                assert_eq!(
                    remote.get(e.path.as_str()),
                    Some(&local),
                    "{} differs from the cloud after quiescence\nacts: {acts:?}",
                    e.path
                );
            }
        }
        for p in remote.keys() {
            let rel = rp(p);
            if trashed.iter().any(|t| rel.starts_with(t)) {
                continue;
            }
            let st = self.stub.local_status(&self.d, &rel).unwrap();
            assert!(
                matches!(st, LocalStatus::File(_) | LocalStatus::Placeholder),
                "{p} in the cloud but {st:?} locally\nacts: {acts:?}"
            );
        }
    }
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), max_shrink_iters: 2000, ..ProptestConfig::default() })]

    #[test]
    fn no_content_is_ever_lost(acts in proptest::collection::vec(act(), 1..24)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut w = World::new().await;
            for a in &acts {
                w.apply(a).await;
            }
            w.quiesce().await;
            w.check(&acts);
        });
    }
}
