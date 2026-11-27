#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use synclone_core::db::Db;
use synclone_core::*;
use synclone_provider_stub::StubProvider;
use synclone_rclone::{MockRclone, RcloneApi};

pub struct H {
    pub tmp: tempfile::TempDir,
    pub mock: MockRclone,
    pub stub: Arc<StubProvider>,
    pub eng: Arc<Engine>,
    pub clock: Arc<TestClock>,
    pub d: Drive,
}

pub const FS: &str = "r:";

pub fn rp(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

pub async fn harness() -> H {
    harness_with(MockRclone::new(), None).await
}

pub async fn harness_with(mock: MockRclone, db_path: Option<PathBuf>) -> H {
    let tmp = tempfile::tempdir().unwrap();
    mock.add_remote("r");
    let stub = Arc::new(StubProvider::new(
        tmp.path().join("Synclone"),
        tmp.path().join("recycle"),
    ));
    let db = Arc::new(match db_path {
        Some(p) => Db::open(&p).unwrap(),
        None => Db::open(&tmp.path().join("state.db")).unwrap(),
    });
    let clock = Arc::new(TestClock::new(1_000_000));
    let settings = EngineSettings {
        concurrency: 2,
        ..Default::default()
    };
    let eng = Engine::new(
        db,
        Arc::new(mock.clone()) as Arc<dyn RcloneApi>,
        stub.clone(),
        clock.clone(),
        settings,
        tmp.path().join("state"),
    );
    let d = eng.connect_remote("r", Some("Cloud")).await.unwrap();
    H {
        tmp,
        mock,
        stub,
        eng,
        clock,
        d,
    }
}

impl H {
    pub fn local(&self, p: &str) -> PathBuf {
        self.stub.path_of(&self.d, &rp(p))
    }
    pub fn write_local(&self, p: &str, data: &[u8]) {
        let path = self.local(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
        // Make sure the mtime moves even on coarse clocks.
        bump_mtime(&path);
    }
    pub fn read_local(&self, p: &str) -> Option<Vec<u8>> {
        std::fs::read(self.local(p)).ok()
    }
    pub fn is_placeholder(&self, p: &str) -> bool {
        self.stub.local_status(&self.d, &rp(p)).unwrap() == LocalStatus::Placeholder
    }
    pub fn status(&self, p: &str) -> LocalStatus {
        self.stub.local_status(&self.d, &rp(p)).unwrap()
    }
    pub fn put(&self, p: &str, data: &[u8]) {
        self.mock.put(FS, p, data);
    }
    pub fn remote(&self, p: &str) -> Option<Vec<u8>> {
        self.mock.get(FS, p)
    }
    pub async fn sync(&self) {
        self.eng.sync_drive_now(self.d.id).await.unwrap();
    }
    pub fn item(&self, p: &str) -> Option<synclone_core::db::Item> {
        self.eng.db.item(self.d.id, &rp(p)).unwrap()
    }
    pub fn recycled(&self) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(self.stub.recycle_dir()).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                v.push(std::fs::read(p).unwrap());
            }
        }
        v
    }
    /// Download a file the way "open" does.
    pub async fn open(&self, p: &str) {
        self.eng
            .handle_event(ProviderEvent::HydrationRequested {
                drive: self.d.id,
                path: rp(p),
            })
            .unwrap();
        self.eng.run_until_idle(true, 50).await.unwrap();
    }
    pub fn conflict_copies(&self, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.local(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("(conflict - "))
            .collect();
        v.sort();
        v
    }
    pub fn remote_conflict_copies(&self) -> Vec<String> {
        self.mock
            .store(FS)
            .files
            .keys()
            .filter(|k| k.contains("(conflict - "))
            .cloned()
            .collect()
    }
}

/// Push a file's mtime forward by a second so every write is visible.
pub fn bump_mtime(path: &std::path::Path) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now() + std::time::Duration::from_secs(n);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(t).unwrap();
}
