//! Phase 2 exit: indexing 100,000 items completes and re-polls with bounded
//! memory; folder listings from the DB return in under 50 ms.
//!
//! Memory is measured with a counting global allocator (this test is alone
//! in its binary so nothing else allocates concurrently). "Bounded" is
//! checked by comparing the peak extra heap while indexing 10,000 vs
//! 100,000 items: a crawler that buffered the tree would grow ~10x.

mod null_provider;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use null_provider::NullProvider;
use synclone_core::db::Db;
use synclone_core::*;
use synclone_rclone::{MockRclone, RcloneApi};

struct Counting;
static CUR: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = CUR.fetch_add(l.size() as isize, Ordering::Relaxed) + l.size() as isize;
            PEAK.fetch_max(n, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size() as isize, Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

fn reset_peak() -> isize {
    let c = CUR.load(Ordering::Relaxed);
    PEAK.store(c, Ordering::Relaxed);
    c
}

/// Build a remote tree: `dirs` folders of `per_dir` files, two levels deep.
fn tree(mock: &MockRclone, fs: &str, dirs: usize, per_dir: usize) {
    for d in 0..dirs {
        for f in 0..per_dir {
            mock.put(fs, &format!("top{:03}/dir{d:05}/file{f:04}.bin", d % 100), b"x");
        }
    }
}

async fn engine(tmp: &tempfile::TempDir, mock: &MockRclone, name: &str) -> (Arc<Engine>, Drive) {
    mock.add_remote(name);
    let db = Arc::new(Db::open(&tmp.path().join(format!("{name}.db"))).unwrap());
    let eng = Engine::new(
        db,
        Arc::new(mock.clone()) as Arc<dyn RcloneApi>,
        Arc::new(NullProvider),
        Arc::new(SystemClock),
        EngineSettings::default(),
        tmp.path().join(format!("{name}-state")),
    );
    let d = eng.connect_remote(name, None).await.unwrap();
    (eng, d)
}

/// Peak extra heap (bytes) during one poll, and its duration.
async fn measured_poll(
    eng: &Engine,
    d: DriveId,
) -> (isize, std::time::Duration, synclone_core::engine::index::PollReport) {
    let base = reset_peak();
    let t = Instant::now();
    let r = eng.poll_drive(d).await.unwrap();
    (PEAK.load(Ordering::Relaxed) - base, t.elapsed(), r)
}

#[tokio::test(flavor = "current_thread")]
async fn index_100k_items_with_bounded_memory_and_fast_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockRclone::new();

    // 10,000 files (+ folders).
    let t0 = Instant::now();
    tree(&mock, "small:", 100, 100);
    println!("built 10k tree in {:?}", t0.elapsed());
    let (small, ds) = engine(&tmp, &mock, "small").await;
    let (small_peak, small_time, r) = measured_poll(&small, ds.id).await;
    assert_eq!(r.added, 10_000 + 100 + 100);

    // 100,000 files (+ folders).
    tree(&mock, "big:", 1_000, 100);
    let (big, db_) = engine(&tmp, &mock, "big").await;
    let (big_peak, big_time, r) = measured_poll(&big, db_.id).await;
    assert_eq!(r.added, 100_000 + 1_000 + 100);
    assert_eq!(big.db.count_items(db_.id).unwrap(), 100_000 + 1_000 + 100 + 1);
    println!(
        "index 10k: peak +{} KiB in {small_time:?}; 100k: peak +{} KiB in {big_time:?}",
        small_peak / 1024,
        big_peak / 1024
    );
    assert!(
        big_peak < small_peak * 2 + 16 * 1024 * 1024,
        "memory grows with the tree: 10k peak {small_peak} B, 100k peak {big_peak} B"
    );

    // Re-poll: nothing changed, memory still bounded.
    let (repoll_peak, repoll_time, r) = measured_poll(&big, db_.id).await;
    assert_eq!(r.changes(), 0);
    println!("re-poll 100k: peak +{} KiB in {repoll_time:?}", repoll_peak / 1024);
    assert!(
        repoll_peak < small_peak * 2 + 16 * 1024 * 1024,
        "re-poll peak {repoll_peak} B"
    );

    // A change deep in the tree is found.
    mock.put("big:", "top007/dir00507/file0001.bin", b"changed");
    mock.remove("big:", "top008/dir00508/file0002.bin");
    mock.put("big:", "top009/dir00509/new.bin", b"n");
    let r = big.poll_drive(db_.id).await.unwrap();
    assert_eq!((r.added, r.modified, r.deleted), (1, 1, 1));

    // Folder listing from the DB, as the OS asks for it: < 50 ms.
    for dir in ["", "top042", "top042/dir00542"] {
        let p = RelPath::new(dir).unwrap();
        let t = Instant::now();
        let kids = big.db.children(db_.id, &p).unwrap();
        let el = t.elapsed();
        println!("children({dir:?}): {} items in {el:?}", kids.len());
        assert!(!kids.is_empty());
        assert!(el.as_millis() < 50, "listing {dir:?} took {el:?}");
    }
}
