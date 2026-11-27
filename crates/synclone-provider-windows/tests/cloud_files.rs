//! Layer 3 Windows suite (PLAN.md §7): the Cloud Files provider with a real
//! sync root, real rclone and the real engine. User actions run in a
//! separate PowerShell process, the way Explorer and programs act.
#![cfg(windows)]

mod common;

use common::*;
use synclone_core::*;
use synclone_provider_windows::{attributes, is_dehydrated, is_pinned};

macro_rules! require_cloud_files {
    () => {
        if !cloud_files_supported() {
            return;
        }
    };
}

fn names(dir: &std::path::Path) -> String {
    ps(&format!(
        "((Get-ChildItem -LiteralPath {} -Force).Name | Sort-Object) -join ','",
        q(dir)
    ))
}

fn read(p: &std::path::Path) -> String {
    ps(&format!("[IO.File]::ReadAllText({})", q(p)))
}

/// Browsing shows every cloud file and folder as a placeholder that takes
/// no space; sizes and times are the cloud's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browsing_shows_placeholders_that_take_no_space() {
    require_cloud_files!();
    let h = harness_with(|c| {
        std::fs::create_dir_all(c.join("docs")).unwrap();
        std::fs::write(c.join("docs/a.txt"), b"alpha").unwrap();
        std::fs::write(c.join("big.bin"), vec![7u8; 1 << 20]).unwrap();
        std::fs::create_dir_all(c.join("music/album")).unwrap();
        std::fs::write(c.join("music/album/song.mp3"), b"la la").unwrap();
    })
    .await;
    // Not indexed yet: the first listing waits for the cloud (§4.2).
    assert_eq!(names(&h.root), "Cloud");
    assert_eq!(names(&h.local("")), "big.bin,docs,music");
    assert_eq!(names(&h.local("music/album")), "song.mp3");
    let big = h.local("big.bin");
    assert!(is_dehydrated(&big), "attributes {:?}", attributes(&big));
    assert_eq!(std::fs::metadata(&big).unwrap().len(), 1 << 20);
    let cloud_mtime = std::fs::metadata(h.cloud_path("big.bin")).unwrap().modified().unwrap();
    let local_mtime = std::fs::metadata(&big).unwrap().modified().unwrap();
    let diff = cloud_mtime
        .duration_since(local_mtime)
        .or_else(|_| local_mtime.duration_since(cloud_mtime))
        .unwrap();
    assert!(
        diff.as_millis() < 1000,
        "mtime {local_mtime:?} vs cloud {cloud_mtime:?}"
    );
    assert_eq!(h.status("big.bin"), LocalStatus::Placeholder);
    assert_eq!(h.status("docs"), LocalStatus::Dir);
}

/// Opening a placeholder downloads it (streamed from rclone), and the
/// download stays: pinned, recorded as downloaded, in sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_a_placeholder_downloads_it_and_it_stays() {
    require_cloud_files!();
    let body: String = (0..300_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    let b2 = body.clone();
    let h = harness_with(move |c| {
        std::fs::create_dir_all(c.join("docs")).unwrap();
        std::fs::write(c.join("docs/report.txt"), b2.as_bytes()).unwrap();
    })
    .await;
    names(&h.local("docs"));
    let p = h.local("docs/report.txt");
    assert!(is_dehydrated(&p));
    assert_eq!(read(&p), body);
    assert!(!is_dehydrated(&p));
    wait_for("recorded as downloaded", 10, || {
        h.item("docs/report.txt").map(|i| i.local_state) == Some(LocalState::Downloaded)
    })
    .await;
    wait_for("pinned so it stays", 10, || is_pinned(&p)).await;
    let it = h.item("docs/report.txt").unwrap();
    assert!(it.synced.is_some() && !it.dirty);
    // Nothing was uploaded back.
    assert_eq!(h.cloud_file("docs/report.txt").unwrap(), body.as_bytes());
}

/// `attrib +P` on a folder is what "Always keep on this device" sets:
/// everything beneath downloads. `attrib +U` ("Free up space") frees it
/// again; the cloud copy is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_and_unpin_attributes_save_and_free_folders() {
    require_cloud_files!();
    let h = harness_with(|c| {
        std::fs::create_dir_all(c.join("music/album")).unwrap();
        std::fs::write(c.join("music/a.mp3"), b"first").unwrap();
        std::fs::write(c.join("music/album/b.mp3"), b"second").unwrap();
        std::fs::write(c.join("other.txt"), b"other").unwrap();
    })
    .await;
    names(&h.local(""));
    ps(&format!("attrib +P {}", q(&h.local("music"))));
    wait_for("folder contents downloaded", 30, || {
        !is_dehydrated(&h.local("music/a.mp3"))
            && h.local("music/album/b.mp3").exists()
            && !is_dehydrated(&h.local("music/album/b.mp3"))
    })
    .await;
    assert_eq!(std::fs::read(h.local("music/album/b.mp3")).unwrap(), b"second");
    wait_for("downloads recorded", 10, || {
        h.item("music/album/b.mp3").map(|i| i.local_state) == Some(LocalState::Downloaded)
    })
    .await;
    wait_for("downloads pinned", 10, || {
        is_pinned(&h.local("music/a.mp3")) && is_pinned(&h.local("music/album/b.mp3"))
    })
    .await;
    assert!(is_dehydrated(&h.local("other.txt")), "outside the pinned folder");

    ps(&format!(
        "attrib -P +U {} /S /D; attrib -P +U {}",
        q(&h.local("music\\*")),
        q(&h.local("music"))
    ));
    wait_for("folder freed", 30, || {
        is_dehydrated(&h.local("music/a.mp3")) && is_dehydrated(&h.local("music/album/b.mp3"))
    })
    .await;
    wait_for("frees recorded", 10, || {
        h.item("music/a.mp3").map(|i| i.local_state) == Some(LocalState::Placeholder)
    })
    .await;
    assert_eq!(h.cloud_file("music/a.mp3").unwrap(), b"first");
    assert_eq!(h.cloud_file("music/album/b.mp3").unwrap(), b"second");
    // Opening a freed file downloads it again.
    assert_eq!(read(&h.local("music/a.mp3")), "first");
}

/// New files and folders upload; edits upload; uploaded files become
/// in-sync, pinned placeholders.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_and_edited_files_upload() {
    require_cloud_files!();
    let h = harness().await;
    names(&h.local(""));
    ps(&format!(
        "[IO.File]::WriteAllText({}, 'note one'); New-Item -ItemType Directory {} | Out-Null; [IO.File]::WriteAllText({}, 'deep')",
        q(&h.local("notes.txt")),
        q(&h.local("new folder")),
        q(&h.local("new folder/inner.txt"))
    ));
    wait_for("uploaded", 20, || {
        h.cloud_file("notes.txt").as_deref() == Some(b"note one".as_slice())
            && h.cloud_file("new folder/inner.txt").as_deref() == Some(b"deep".as_slice())
    })
    .await;
    wait_for("marked synced", 10, || is_pinned(&h.local("notes.txt"))).await;
    ps(&format!(
        "[IO.File]::AppendAllText({}, ' and two')",
        q(&h.local("notes.txt"))
    ));
    wait_for("edit uploaded", 20, || {
        h.cloud_file("notes.txt").as_deref() == Some(b"note one and two".as_slice())
    })
    .await;
    // Temp files are never uploaded (§4.4).
    ps(&format!(
        "[IO.File]::WriteAllText({}, 'x')",
        q(&h.local("~$draft.docx"))
    ));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(h.cloud_file("~$draft.docx").is_none());
}

/// Renames and moves inside a drive happen in the cloud too, for
/// placeholders and downloaded files alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renames_and_moves_apply_in_the_cloud() {
    require_cloud_files!();
    let h = harness_with(|c| {
        std::fs::create_dir_all(c.join("a")).unwrap();
        std::fs::create_dir_all(c.join("b")).unwrap();
        std::fs::write(c.join("a/one.txt"), b"one").unwrap();
        std::fs::write(c.join("a/two.txt"), b"two").unwrap();
    })
    .await;
    names(&h.local("a"));
    names(&h.local("b"));
    // A placeholder renamed.
    ps(&format!(
        "Rename-Item -LiteralPath {} -NewName 'uno.txt'",
        q(&h.local("a/one.txt"))
    ));
    wait_for("renamed in the cloud", 20, || {
        h.cloud_file("a/uno.txt").is_some() && h.cloud_file("a/one.txt").is_none()
    })
    .await;
    // A downloaded file moved to another folder.
    assert_eq!(read(&h.local("a/two.txt")), "two");
    ps(&format!(
        "Move-Item -LiteralPath {} -Destination {}",
        q(&h.local("a/two.txt")),
        q(&h.local("b/two.txt"))
    ));
    wait_for("moved in the cloud", 20, || {
        h.cloud_file("b/two.txt").as_deref() == Some(b"two".as_slice()) && h.cloud_file("a/two.txt").is_none()
    })
    .await;
    assert_eq!(h.cloud_file("a/uno.txt").unwrap(), b"one");
    let trash = h.eng.trash_list().unwrap();
    assert!(
        trash.is_empty(),
        "trash: {trash:?}
activity: {:?}",
        h.eng.db.activity(30).unwrap()
    );
}

/// Deleting goes to Synclone's trash queue; the cloud keeps the file until
/// "Empty trash". Restoring brings the placeholder back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deletes_go_to_the_trash_queue_not_the_cloud() {
    require_cloud_files!();
    let h = harness_with(|c| {
        std::fs::write(c.join("keep.txt"), b"keep").unwrap();
        std::fs::write(c.join("gone.txt"), b"gone").unwrap();
        std::fs::write(c.join("recycled.txt"), b"recycled").unwrap();
    })
    .await;
    names(&h.local(""));
    assert_eq!(read(&h.local("recycled.txt")), "recycled");
    ps(&format!("Remove-Item -LiteralPath {}", q(&h.local("gone.txt"))));
    // Explorer's Delete key: to the Recycle Bin.
    ps(&format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile({}, 'OnlyErrorDialogs', 'SendToRecycleBin')",
        q(&h.local("recycled.txt"))
    ));
    wait_for("both in the trash queue", 20, || h.eng.trash_list().unwrap().len() == 2).await;
    assert_eq!(h.cloud_file("gone.txt").unwrap(), b"gone");
    assert_eq!(h.cloud_file("recycled.txt").unwrap(), b"recycled");
    // Polling does not bring them back while they wait in the trash.
    h.poll().await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert!(!h.local("gone.txt").exists());
    // Restore one, empty the other.
    let t = h
        .eng
        .trash_list()
        .unwrap()
        .into_iter()
        .find(|t| t.path.as_str() == "recycled.txt")
        .unwrap();
    h.eng.restore_trash(t.id, None).unwrap();
    wait_for("restored placeholder", 10, || h.local("recycled.txt").exists()).await;
    h.eng.request_empty_trash().unwrap();
    wait_for("deleted from the cloud", 20, || h.cloud_file("gone.txt").is_none()).await;
    assert_eq!(h.cloud_file("keep.txt").unwrap(), b"keep");
    assert_eq!(h.cloud_file("recycled.txt").unwrap(), b"recycled");
}

/// Rule 1: a cloud change to a placeholder updates its size and time.
/// Rule 2: a cloud change to a clean downloaded file replaces it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_changes_update_placeholders_and_downloads() {
    require_cloud_files!();
    let h = harness_with(|c| {
        std::fs::write(c.join("ph.txt"), b"small").unwrap();
        std::fs::write(c.join("dl.txt"), b"version one").unwrap();
    })
    .await;
    names(&h.local(""));
    assert_eq!(read(&h.local("dl.txt")), "version one");
    wait_for("recorded as downloaded", 10, || {
        h.item("dl.txt").map(|i| i.local_state) == Some(LocalState::Downloaded)
    })
    .await;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    h.put_cloud("ph.txt", b"a good deal larger now");
    h.put_cloud("dl.txt", b"version two, longer");
    h.poll().await;
    wait_for("placeholder updated", 20, || {
        std::fs::metadata(h.local("ph.txt")).map(|m| m.len()).unwrap_or(0) == 22
    })
    .await;
    assert!(is_dehydrated(&h.local("ph.txt")));
    wait_for("download replaced", 20, || {
        std::fs::read(h.local("dl.txt")).ok().as_deref() == Some(b"version two, longer".as_slice())
    })
    .await;
    assert!(is_pinned(&h.local("dl.txt")));
    assert_eq!(read(&h.local("ph.txt")), "a good deal larger now");
}

/// A file a program holds open for writing is reported as open, so it is
/// neither uploaded half-written nor replaced (rule 3, §4.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_open_for_writing_are_detected() {
    require_cloud_files!();
    let h = harness_with(|c| std::fs::write(c.join("f.txt"), b"f").unwrap()).await;
    names(&h.local(""));
    assert_eq!(read(&h.local("f.txt")), "f");
    assert!(!h.prov.is_open(&h.d, &rp("f.txt")).unwrap());
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(1 | 2) // read | write, like most editors
        .open(h.local("f.txt"))
        .unwrap();
    assert!(h.prov.is_open(&h.d, &rp("f.txt")).unwrap());
    drop(held);
    assert!(!h.prov.is_open(&h.d, &rp("f.txt")).unwrap());
    assert!(!h.prov.is_open(&h.d, &rp("missing.txt")).unwrap());
}

/// Deleting a drive's folder disconnects the drive: nothing goes into the
/// trash queue and nothing in the cloud changes (§3.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_the_drive_folder_disconnects_without_touching_the_cloud() {
    require_cloud_files!();
    let h = harness_with(|c| {
        std::fs::create_dir_all(c.join("d")).unwrap();
        std::fs::write(c.join("d/x.txt"), b"x").unwrap();
        std::fs::write(c.join("y.txt"), b"y").unwrap();
    })
    .await;
    names(&h.local(""));
    names(&h.local("d"));
    assert_eq!(read(&h.local("y.txt")), "y");
    let before = h.cloud_snapshot();
    ps(&format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory({}, 'OnlyErrorDialogs', 'SendToRecycleBin')",
        q(&h.local(""))
    ));
    wait_for("drive disconnected", 20, || {
        h.eng.drive(h.d.id).unwrap().state == DriveState::Disconnected
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(h.eng.trash_list().unwrap().is_empty());
    assert_eq!(h.cloud_snapshot(), before, "the cloud is untouched");
}

/// Renaming a drive's folder renames the drive; the files keep working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_the_drive_folder_renames_the_drive() {
    require_cloud_files!();
    let h = harness_with(|c| std::fs::write(c.join("z.txt"), b"zed").unwrap()).await;
    names(&h.local(""));
    let before = h.cloud_snapshot();
    ps(&format!(
        "Rename-Item -LiteralPath {} -NewName 'My Cloud'",
        q(&h.local(""))
    ));
    wait_for("drive renamed", 20, || {
        h.eng.drive(h.d.id).unwrap().display_name == "My Cloud"
    })
    .await;
    assert_eq!(read(&h.root.join("My Cloud").join("z.txt")), "zed");
    assert_eq!(h.eng.drive(h.d.id).unwrap().state, DriveState::Connected);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(h.cloud_snapshot(), before);
}

/// Write `gib` GiB of non-repeating blocks to `path`.
fn write_big(path: &std::path::Path, gib: u64) {
    use std::io::Write;
    let mut f = std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(path).unwrap());
    let mut block = vec![0u8; 1 << 20];
    for (i, b) in block.iter_mut().enumerate() {
        *b = (i.wrapping_mul(2654435761) >> 13) as u8;
    }
    for n in 0..gib * 1024 {
        block[..8].copy_from_slice(&n.to_le_bytes());
        f.write_all(&block).unwrap();
    }
    f.flush().unwrap();
}

fn md5_of(path: &std::path::Path) -> String {
    ps(&format!("(Get-FileHash -Algorithm MD5 -LiteralPath {}).Hash", q(path)))
}

/// Phase 4 exit: a 5 GB file hydrates (opened by another program, streamed
/// from rclone) and a 5 GB local file uploads, both intact. Writes about
/// 20 GB, so it runs only with SYNCLONE_BIG_FILE_TEST=1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_gigabyte_file_hydrates_and_uploads() {
    require_cloud_files!();
    if std::env::var("SYNCLONE_BIG_FILE_TEST").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set SYNCLONE_BIG_FILE_TEST=1 to run the 5 GB test (needs about 20 GB free)");
        return;
    }
    const GIB: u64 = 5;
    let h = harness_with(|c| write_big(&c.join("audiobook.m4b"), GIB)).await;
    names(&h.local(""));
    let local = h.local("audiobook.m4b");
    assert!(is_dehydrated(&local));
    assert_eq!(std::fs::metadata(&local).unwrap().len(), GIB << 30);

    let t = std::time::Instant::now();
    let local_md5 = md5_of(&local); // reading it hydrates it
    let hydrate_secs = t.elapsed().as_secs_f64();
    assert_eq!(
        local_md5,
        md5_of(&h.cloud_path("audiobook.m4b")),
        "hydrated content matches"
    );
    assert!(!is_dehydrated(&local));
    wait_for("recorded as downloaded", 30, || {
        h.item("audiobook.m4b").map(|i| i.local_state) == Some(LocalState::Downloaded)
    })
    .await;

    let t = std::time::Instant::now();
    write_big(&h.local("recording.wav"), GIB);
    let src_md5 = md5_of(&h.local("recording.wav"));
    wait_for("5 GB upload", 900, || {
        h.item("recording.wav")
            .map(|i| i.synced.is_some() && !i.dirty)
            .unwrap_or(false)
    })
    .await;
    let upload_secs = t.elapsed().as_secs_f64();
    assert_eq!(
        md5_of(&h.cloud_path("recording.wav")),
        src_md5,
        "uploaded content matches"
    );
    eprintln!(
        "RESULT: 5 GiB hydrated in {hydrate_secs:.1} s (incl. hashing), written + uploaded in {upload_secs:.1} s"
    );
    drop(h);
}
