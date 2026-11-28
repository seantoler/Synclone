//! Hot folders (D-031) and restoring downloaded files from the trash
//! (D-032), against the mock and the stub provider.

mod common;

use common::*;
use synclone_core::*;
use synclone_rclone::ErrorKind;

fn list_calls(h: &H) -> usize {
    h.mock.count("operations/list")
}

/// Cloud downloads (copyfile from the cloud into staging). Uploads also
/// use copyfile, so tests check this where nothing uploads.
fn copyfiles(h: &H) -> usize {
    h.mock.count("operations/copyfile")
}

async fn browse(h: &H, dir: &str) {
    h.eng
        .handle_event(ProviderEvent::FolderEnumerated {
            drive: h.d.id,
            path: rp(dir),
        })
        .unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
}

// ---------------------------------------------------------------- hot folders

#[tokio::test]
async fn a_change_in_a_browsed_folder_shows_up_without_a_full_poll() {
    let h = harness().await;
    h.put("docs/a.txt", b"a");
    h.put("other/b.txt", b"b");
    h.sync().await;
    browse(&h, "docs").await;
    h.put("docs/new.txt", b"new");
    h.put("other/new.txt", b"new");
    h.mock.clear_calls();
    let r = h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(r.folders, 1);
    assert_eq!(list_calls(&h), 1, "one listing: the browsed folder only");
    assert!(h.is_placeholder("docs/new.txt"));
    assert_eq!(
        h.status("other/new.txt"),
        LocalStatus::Absent,
        "waits for the full poll"
    );
    // Not due again until the interval (30 s for this service) has passed.
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(list_calls(&h), 1);
    h.clock.advance(29_000);
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(list_calls(&h), 1);
    h.clock.advance(1_500);
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(list_calls(&h), 2);
    // The full poll still covers everything.
    h.sync().await;
    assert!(h.is_placeholder("other/new.txt"));
}

#[tokio::test]
async fn open_folders_stay_hot_and_others_cool_down() {
    let h = harness().await;
    h.put("open/a.txt", b"a");
    h.put("old/b.txt", b"b");
    h.sync().await;
    browse(&h, "old").await;
    h.stub.set_open_folders(vec![(h.d.id, rp("open"))]);
    h.eng.poll_hot(h.d.id).await.unwrap();
    let mut hot = h.eng.hot_folders(h.d.id);
    hot.sort();
    assert_eq!(hot, vec![rp("old"), rp("open")]);
    // Ten minutes later, "old" has cooled down; the open window keeps "open".
    h.clock.advance(601_000);
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(h.eng.hot_folders(h.d.id), vec![rp("open")]);
    h.stub.set_open_folders(vec![]);
    h.clock.advance(601_000);
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert!(h.eng.hot_folders(h.d.id).is_empty());
}

#[tokio::test]
async fn hot_folders_are_capped_per_drive() {
    let h = harness().await;
    for i in 0..9 {
        h.put(&format!("f{i}/x.txt"), b"x");
    }
    h.sync().await;
    for i in 0..9 {
        browse(&h, &format!("f{i}")).await;
        h.clock.advance(10);
    }
    h.mock.clear_calls();
    let r = h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(r.folders, 5, "this service's cap");
    assert_eq!(list_calls(&h), 5);
}

/// A single listing cannot tell a move from a delete: a downloaded file
/// that left the hot folder is not sent to the Recycle Bin; a full poll
/// sorts it out. A vanished placeholder is just removed.
#[tokio::test]
async fn a_downloaded_file_moved_out_of_a_hot_folder_is_not_treated_as_deleted() {
    let h = harness().await;
    h.put("docs/keep.txt", b"keep this");
    h.put("docs/ph.txt", b"placeholder");
    h.put("other/x.txt", b"x");
    h.sync().await;
    h.open("docs/keep.txt").await;
    browse(&h, "docs").await;
    h.mock.rename(FS, "docs/keep.txt", "other/keep.txt");
    h.mock.remove(FS, "docs/ph.txt");
    let r = h.eng.poll_hot(h.d.id).await.unwrap();
    assert!(r.full_poll_requested);
    assert_eq!(
        h.read_local("docs/keep.txt").unwrap(),
        b"keep this",
        "kept until the full poll"
    );
    assert!(h.recycled().is_empty(), "nothing sent to the Recycle Bin");
    assert_eq!(
        h.status("docs/ph.txt"),
        LocalStatus::Absent,
        "vanished placeholder removed"
    );
    assert_eq!(h.eng.db.last_poll(h.d.id).unwrap(), Some(0), "full poll due now");
    // The full poll sees the move.
    h.sync().await;
    assert_eq!(h.read_local("other/keep.txt").unwrap(), b"keep this");
    assert_eq!(h.status("docs/keep.txt"), LocalStatus::Absent);
    assert!(h.recycled().is_empty());
}

#[tokio::test]
async fn rate_limits_back_the_hot_interval_off_and_it_recovers() {
    let h = harness().await;
    h.put("docs/a.txt", b"a");
    h.sync().await;
    browse(&h, "docs").await;
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 30_000);
    h.mock.fail_next("operations/list", ErrorKind::RateLimited, 2);
    h.clock.advance(30_000);
    assert!(h.eng.poll_hot(h.d.id).await.unwrap().rate_limited);
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 60_000);
    h.clock.advance(60_000);
    assert!(h.eng.poll_hot(h.d.id).await.unwrap().rate_limited);
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 120_000);
    // Nothing is asked while backing off.
    h.mock.clear_calls();
    h.clock.advance(60_000);
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(list_calls(&h), 0);
    // Clean rounds halve it back to normal.
    h.clock.advance(60_000);
    assert!(!h.eng.poll_hot(h.d.id).await.unwrap().rate_limited);
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 60_000);
    h.clock.advance(60_000);
    h.eng.poll_hot(h.d.id).await.unwrap();
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 30_000);
    // A throttled full poll slows hot polling too (same quota).
    h.eng.note_rate_limited(h.d.id);
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 60_000);
    // The cap.
    for _ in 0..10 {
        h.eng.note_rate_limited(h.d.id);
    }
    assert_eq!(h.eng.hot_interval_ms(h.d.id), 600_000);
}

#[tokio::test]
async fn hot_polling_can_be_turned_off() {
    let h = harness_with_settings(EngineSettings {
        hot_poll_secs: Some(0),
        ..default_settings()
    })
    .await;
    h.put("docs/a.txt", b"a");
    h.sync().await;
    browse(&h, "docs").await;
    h.mock.clear_calls();
    assert_eq!(h.eng.poll_hot(h.d.id).await.unwrap().folders, 0);
    assert_eq!(list_calls(&h), 0);
}

// ------------------------------------------------- restoring from the trash

async fn downloaded_then_recycled(h: &H, p: &str, data: &[u8]) -> i64 {
    h.put(p, data);
    h.sync().await;
    h.open(p).await;
    assert_eq!(h.read_local(p).unwrap(), data);
    h.stub.user_recycle(&h.d, &rp(p)).unwrap();
    h.sync().await;
    let t = h.eng.trash_list().unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].snapshot.downloaded.len(), 1, "remembered as downloaded");
    t[0].id
}

#[tokio::test]
async fn a_restored_download_comes_back_from_the_recycle_bin() {
    let h = harness().await;
    let id = downloaded_then_recycled(&h, "docs/report.txt", b"the report").await;
    assert_eq!(h.recycled().len(), 1);
    h.mock.clear_calls();
    h.eng.restore_trash(id, None).unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("docs/report.txt").unwrap(), b"the report");
    let it = h.item("docs/report.txt").unwrap();
    assert_eq!(it.local_state, LocalState::Downloaded);
    assert!(!it.dirty);
    assert_eq!(copyfiles(&h), 0, "not downloaded again");
    assert!(h.recycled().is_empty(), "taken out of the Recycle Bin");
    let act = h.eng.db.activity(10).unwrap();
    assert!(
        act.iter().any(|a| a.message.contains("from the Recycle Bin")),
        "{act:?}"
    );
    // Nothing to upload afterwards.
    h.sync().await;
    assert_eq!(copyfiles(&h), 0);
    assert_eq!(h.remote("docs/report.txt").unwrap(), b"the report");
}

#[tokio::test]
async fn a_restored_download_is_downloaded_again_if_the_cloud_changed() {
    let h = harness().await;
    let id = downloaded_then_recycled(&h, "a.txt", b"old version").await;
    h.put("a.txt", b"new version from elsewhere");
    h.eng.restore_trash(id, None).unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("a.txt").unwrap(), b"new version from elsewhere");
    assert_eq!(h.item("a.txt").unwrap().local_state, LocalState::Downloaded);
    assert_eq!(
        h.recycled(),
        vec![b"old version".to_vec()],
        "the old copy stays in the Recycle Bin"
    );
}

#[tokio::test]
async fn a_restored_download_is_downloaded_again_if_the_recycled_copy_differs() {
    let h = harness().await;
    let id = downloaded_then_recycled(&h, "a.txt", b"cloud copy").await;
    // Someone changed the copy in the Recycle Bin (its size and time differ).
    let copy = std::fs::read_dir(h.stub.recycle_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&copy, b"edited in the bin, longer").unwrap();
    h.eng.restore_trash(id, None).unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("a.txt").unwrap(), b"cloud copy");
    assert_eq!(h.item("a.txt").unwrap().local_state, LocalState::Downloaded);
    assert_eq!(h.recycled(), vec![b"edited in the bin, longer".to_vec()], "left alone");
}

#[tokio::test]
async fn a_restored_download_deleted_for_good_is_downloaded_again() {
    let h = harness().await;
    h.put("a.txt", b"content");
    h.sync().await;
    h.open("a.txt").await;
    std::fs::remove_file(h.local("a.txt")).unwrap();
    h.sync().await;
    let id = h.eng.trash_list().unwrap()[0].id;
    h.eng.restore_trash(id, None).unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("a.txt").unwrap(), b"content");
    assert_eq!(h.item("a.txt").unwrap().local_state, LocalState::Downloaded);
}

#[tokio::test]
async fn a_restored_placeholder_stays_a_placeholder() {
    let h = harness().await;
    h.put("a.txt", b"never opened");
    h.sync().await;
    assert!(h.is_placeholder("a.txt"));
    std::fs::remove_file(format!("{}.sclstub", h.local("a.txt").display())).unwrap();
    h.sync().await;
    let t = h.eng.trash_list().unwrap();
    assert!(t[0].snapshot.downloaded.is_empty());
    h.mock.clear_calls();
    h.eng.restore_trash(t[0].id, None).unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert!(h.is_placeholder("a.txt"));
    assert_eq!(copyfiles(&h), 0);
}

/// Rule 8: when the cloud copy changes after a delete, the engine restores
/// it by itself. That brings back a placeholder, not a download.
#[tokio::test]
async fn an_automatic_restore_brings_back_a_placeholder() {
    let h = harness().await;
    let _ = downloaded_then_recycled(&h, "a.txt", b"v1").await;
    h.put("a.txt", b"v2, changed in the cloud");
    h.sync().await;
    assert!(h.eng.trash_list().unwrap().is_empty(), "restored by rule 8");
    assert!(h.is_placeholder("a.txt"));
    assert_eq!(h.recycled(), vec![b"v1".to_vec()]);
}

/// Restoring a folder brings each file back as it was: downloaded ones
/// downloaded, placeholders as placeholders.
#[tokio::test]
async fn a_restored_folder_brings_back_its_downloads() {
    let h = harness().await;
    h.put("dir/dl.txt", b"downloaded");
    h.put("dir/ph.txt", b"placeholder");
    h.sync().await;
    h.open("dir/dl.txt").await;
    h.stub.user_recycle(&h.d, &rp("dir")).unwrap();
    h.sync().await;
    let t = h.eng.trash_list().unwrap();
    assert_eq!(t.len(), 1);
    assert!(t[0].is_dir);
    // The folder went to the bin as a whole, so the file is not found
    // there by itself: it is downloaded again.
    h.eng.restore_trash(t[0].id, None).unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("dir/dl.txt").unwrap(), b"downloaded");
    assert_eq!(h.item("dir/dl.txt").unwrap().local_state, LocalState::Downloaded);
    assert!(h.is_placeholder("dir/ph.txt"));
}

/// Found by the CLI test: a placeholder the user deletes while its download
/// is in flight must not come back when the download lands.
#[tokio::test]
async fn a_download_does_not_bring_back_a_placeholder_deleted_meanwhile() {
    let h = harness().await;
    h.put("a.txt", b"content");
    h.sync().await;
    assert!(h.is_placeholder("a.txt"));
    let staged = h.tmp.path().join("staged");
    std::fs::write(&staged, b"content").unwrap();
    std::fs::remove_file(format!("{}.sclstub", h.local("a.txt").display())).unwrap();
    let meta = h.item("a.txt").unwrap().meta();
    let r = h
        .stub
        .commit_download(&h.d, &rp("a.txt"), &staged, &meta, LocalStatus::Placeholder);
    assert!(r.is_err(), "refused");
    assert_eq!(h.status("a.txt"), LocalStatus::Absent);
    // Expecting nothing (a folder never browsed) still works.
    let r = h
        .stub
        .commit_download(&h.d, &rp("a.txt"), &staged, &meta, LocalStatus::Absent);
    assert!(r.is_ok());
}

#[tokio::test]
async fn deleting_again_before_the_restore_finishes_is_a_delete() {
    let h = harness().await;
    h.put("a.txt", b"content");
    h.sync().await;
    h.open("a.txt").await;
    std::fs::remove_file(h.local("a.txt")).unwrap();
    h.sync().await;
    let id = h.eng.trash_list().unwrap()[0].id;
    h.eng.restore_trash(id, None).unwrap();
    // The placeholder is back; delete it before the recover op runs.
    assert!(h.is_placeholder("a.txt"));
    std::fs::remove_file(format!("{}.sclstub", h.local("a.txt").display())).unwrap();
    h.sync().await;
    assert_eq!(h.status("a.txt"), LocalStatus::Absent, "not brought back");
    assert_eq!(h.eng.trash_list().unwrap().len(), 1, "in the trash again");
    h.eng.request_empty_trash().unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert!(h.remote("a.txt").is_none());
}
