//! Engine behaviour beyond the conflict table: trash queue lifecycle,
//! pin/free recursion, drive lifecycle, debounce, backoff, ignore list,
//! unsyncable names, remote renames.

mod common;
use common::*;
use synclone_core::provider::NameRules;
use synclone_core::*;
use synclone_rclone::ErrorKind;

// ---------------------------------------------------------------- trash

#[tokio::test]
async fn local_delete_goes_to_trash_and_nothing_is_sent() {
    let h = harness().await;
    h.put("a.txt", b"hello");
    h.sync().await;
    h.open("a.txt").await;
    h.mock.clear_calls();
    std::fs::remove_file(h.local("a.txt")).unwrap();
    h.sync().await;
    assert_eq!(h.mock.remote_write_calls(), 0, "no remote write or delete");
    assert_eq!(h.remote("a.txt").unwrap(), b"hello");
    let t = h.eng.trash_list().unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].size, 5);
    assert_eq!(h.eng.trash_summary().unwrap().total_size, 5);
}

#[tokio::test]
async fn trash_restore_brings_back_placeholder() {
    let h = harness().await;
    h.put("a.txt", b"hello");
    h.sync().await;
    std::fs::remove_file(h.local("a.txt.sclstub")).unwrap();
    h.sync().await;
    let id = h.eng.trash_list().unwrap()[0].id;
    h.eng.restore_trash(id, None).unwrap();
    assert!(h.is_placeholder("a.txt"));
    assert!(h.eng.trash_list().unwrap().is_empty());
    h.sync().await;
    assert!(h.is_placeholder("a.txt"));
    assert_eq!(h.remote("a.txt").unwrap(), b"hello");
}

#[tokio::test]
async fn empty_trash_deletes_from_cloud() {
    let h = harness().await;
    h.put("a.txt", b"hello");
    h.put("b.txt", b"world");
    h.sync().await;
    std::fs::remove_file(h.local("a.txt.sclstub")).unwrap();
    h.sync().await;
    let s = h.eng.request_empty_trash().unwrap();
    assert_eq!((s.count, s.total_size), (1, 5));
    h.eng.run_until_idle(true, 10).await.unwrap();
    assert!(h.remote("a.txt").is_none());
    assert_eq!(h.remote("b.txt").unwrap(), b"world");
    assert!(h.item("a.txt").is_none());
    assert_eq!(h.eng.db.trash(None).unwrap()[0].status, "emptied");
    h.sync().await;
    assert_eq!(h.status("a.txt"), LocalStatus::Absent, "stays gone");
}

#[tokio::test]
async fn empty_trash_keeps_file_changed_after_delete() {
    let h = harness().await;
    h.put("a.txt", b"hello");
    h.sync().await;
    std::fs::remove_file(h.local("a.txt.sclstub")).unwrap();
    h.sync().await;
    // Changed remotely but not yet polled.
    h.put("a.txt", b"newer content");
    h.eng.request_empty_trash().unwrap();
    h.eng.run_until_idle(true, 10).await.unwrap();
    assert_eq!(h.remote("a.txt").unwrap(), b"newer content", "not deleted");
    assert!(h.is_placeholder("a.txt"), "restored");
    assert_eq!(h.mock.count("operations/deletefile"), 0);
}

#[tokio::test]
async fn folder_trash_purges_when_unchanged() {
    let h = harness().await;
    h.put("dir/a.txt", b"1");
    h.put("dir/sub/b.txt", b"2");
    h.put("keep.txt", b"3");
    h.sync().await;
    std::fs::remove_dir_all(h.local("dir")).unwrap();
    h.sync().await;
    let t = h.eng.trash_list().unwrap();
    assert_eq!(t.len(), 1, "one entry for the folder");
    assert!(t[0].is_dir);
    assert_eq!(h.mock.remote_write_calls(), 0);
    h.eng.request_empty_trash().unwrap();
    h.eng.run_until_idle(true, 10).await.unwrap();
    assert_eq!(h.mock.count("operations/purge"), 1);
    assert!(!h.mock.exists(FS, "dir"));
    assert_eq!(h.remote("keep.txt").unwrap(), b"3");
}

#[tokio::test]
async fn folder_trash_keeps_new_and_changed_files() {
    let h = harness().await;
    h.put("dir/a.txt", b"1");
    h.put("dir/b.txt", b"2");
    h.sync().await;
    std::fs::remove_dir_all(h.local("dir")).unwrap();
    h.sync().await;
    h.put("dir/b.txt", b"changed");
    h.put("dir/new.txt", b"new");
    h.eng.request_empty_trash().unwrap();
    h.eng.run_until_idle(true, 10).await.unwrap();
    assert_eq!(h.mock.count("operations/purge"), 0, "no purge over unseen data");
    assert!(h.remote("dir/a.txt").is_none(), "unchanged file deleted");
    assert_eq!(h.remote("dir/b.txt").unwrap(), b"changed");
    assert_eq!(h.remote("dir/new.txt").unwrap(), b"new");
    h.sync().await;
    assert!(h.is_placeholder("dir/b.txt"));
    assert!(h.is_placeholder("dir/new.txt"));
    assert_eq!(h.status("dir/a.txt"), LocalStatus::Absent);
}

#[tokio::test]
async fn deleting_never_uploaded_file_is_plain_local_delete() {
    let h = harness().await;
    h.sync().await;
    h.write_local("draft.txt", b"x");
    // Deleted before the debounce let it upload.
    h.eng
        .handle_event(ProviderEvent::LocalCreated {
            drive: h.d.id,
            path: rp("draft.txt"),
        })
        .unwrap();
    std::fs::remove_file(h.local("draft.txt")).unwrap();
    h.sync().await;
    assert!(h.eng.trash_list().unwrap().is_empty());
    assert!(h.remote("draft.txt").is_none());
    assert!(h.item("draft.txt").is_none());
}

// ---------------------------------------------------------------- pin / free

#[tokio::test]
async fn pin_folder_downloads_recursively_and_new_children_follow() {
    let h = harness().await;
    h.put("music/a.mp3", b"a");
    h.put("music/album/b.mp3", b"bb");
    h.put("music/album/deep/c.mp3", b"ccc");
    h.put("other.txt", b"o");
    h.sync().await;
    h.eng
        .handle_event(ProviderEvent::PinRequested {
            drive: h.d.id,
            path: rp("music"),
            recursive: true,
        })
        .unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("music/a.mp3").unwrap(), b"a");
    assert_eq!(h.read_local("music/album/b.mp3").unwrap(), b"bb");
    assert_eq!(h.read_local("music/album/deep/c.mp3").unwrap(), b"ccc");
    assert!(h.is_placeholder("other.txt"));
    // New remote children of a pinned folder download automatically.
    h.put("music/album/new.mp3", b"new");
    h.put("music/newdir/x.mp3", b"x");
    h.sync().await;
    assert_eq!(h.read_local("music/album/new.mp3").unwrap(), b"new");
    assert_eq!(h.read_local("music/newdir/x.mp3").unwrap(), b"x");
}

#[tokio::test]
async fn free_folder_recursively_keeps_dirty_files() {
    let h = harness().await;
    h.put("d/a.txt", b"a");
    h.put("d/s/b.txt", b"b");
    h.sync().await;
    h.eng.pin(h.d.id, &rp("d"), true).await.unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("d/s/b.txt").unwrap(), b"b");
    h.write_local("d/a.txt", b"unsynced edit");
    let kept = h.eng.free(h.d.id, &rp("d"), true).unwrap();
    assert_eq!(kept, vec![rp("d/a.txt")]);
    assert!(h.is_placeholder("d/s/b.txt"));
    assert_eq!(h.read_local("d/a.txt").unwrap(), b"unsynced edit");
    assert_eq!(h.remote("d/s/b.txt").unwrap(), b"b", "cloud copy untouched");
    // Free does not unpin-and-redownload: a later sync leaves it freed.
    h.sync().await;
    assert!(h.is_placeholder("d/s/b.txt"));
    assert_eq!(h.remote("d/a.txt").unwrap(), b"unsynced edit", "the edit still uploads");
}

#[tokio::test]
async fn downloads_stay_until_freed() {
    let h = harness().await;
    h.put("a.txt", b"x");
    h.sync().await;
    h.open("a.txt").await;
    for _ in 0..3 {
        h.sync().await;
    }
    assert_eq!(h.read_local("a.txt").unwrap(), b"x");
}

// ---------------------------------------------------------------- uploads

#[tokio::test]
async fn new_local_files_and_folders_upload() {
    let h = harness().await;
    h.sync().await;
    std::fs::create_dir_all(h.local("new/empty")).unwrap();
    h.write_local("new/file.txt", b"data");
    h.sync().await;
    assert_eq!(h.remote("new/file.txt").unwrap(), b"data");
    assert!(h.mock.exists(FS, "new/empty"), "empty folder created remotely");
    assert!(!h.item("new/file.txt").unwrap().dirty);
}

#[tokio::test]
async fn ignored_files_are_never_uploaded() {
    let h = harness().await;
    h.sync().await;
    for n in [
        "~$doc.docx",
        "x.tmp",
        ".DS_Store",
        "Thumbs.db",
        "dl.crdownload",
        "desktop.ini",
        ".~lock.a.odt#",
    ] {
        h.write_local(n, b"junk");
    }
    h.write_local("real.txt", b"ok");
    h.sync().await;
    let files: Vec<_> = h.mock.store(FS).files.keys().cloned().collect();
    assert_eq!(files, vec!["real.txt"]);
}

#[tokio::test]
async fn local_changes_are_debounced() {
    let h = harness().await;
    h.sync().await;
    h.write_local("a.txt", b"1");
    h.eng
        .handle_event(ProviderEvent::LocalCreated {
            drive: h.d.id,
            path: rp("a.txt"),
        })
        .unwrap();
    h.clock.advance(1_000);
    h.write_local("a.txt", b"12");
    h.eng
        .handle_event(ProviderEvent::LocalModified {
            drive: h.d.id,
            path: rp("a.txt"),
        })
        .unwrap();
    h.clock.advance(2_500);
    h.eng.run_due().await.unwrap();
    assert!(h.remote("a.txt").is_none(), "less than 3 s since the last change");
    h.clock.advance(600);
    h.eng.run_due().await.unwrap();
    assert_eq!(h.remote("a.txt").unwrap(), b"12");
    assert_eq!(h.mock.count("operations/copyfile"), 1, "one upload for two edits");
}

#[tokio::test]
async fn open_file_is_not_uploaded_until_closed() {
    let h = harness().await;
    h.sync().await;
    h.write_local("big.wav", b"partial");
    h.stub.set_open(h.d.id, &rp("big.wav"), true);
    h.sync().await;
    assert!(h.remote("big.wav").is_none());
    h.write_local("big.wav", b"complete file");
    h.stub.set_open(h.d.id, &rp("big.wav"), false);
    h.sync().await;
    assert_eq!(h.remote("big.wav").unwrap(), b"complete file");
}

#[tokio::test]
async fn failed_upload_is_retried_with_backoff_not_dropped() {
    let h = harness().await;
    h.sync().await;
    h.mock.fail_next("operations/copyfile", ErrorKind::RateLimited, 2);
    h.write_local("a.txt", b"x");
    h.eng.scan_drive(h.d.id).unwrap();
    h.clock.advance(3_100);
    h.eng.run_due().await.unwrap();
    let ops = h.eng.db.ops(Some("pending")).unwrap();
    assert_eq!(ops.len(), 1, "kept in the journal");
    let first_delay = ops[0].not_before - h.clock.now_ms();
    assert!(first_delay >= 8_000, "rate limits back off longer: {first_delay}");
    h.clock.advance(first_delay);
    h.eng.run_due().await.unwrap();
    let ops = h.eng.db.ops(Some("pending")).unwrap();
    let second_delay = ops[0].not_before - h.clock.now_ms();
    assert!(
        second_delay > first_delay,
        "exponential: {second_delay} > {first_delay}"
    );
    h.clock.advance(second_delay);
    h.eng.run_due().await.unwrap();
    assert_eq!(h.remote("a.txt").unwrap(), b"x");
    assert!(h.eng.db.ops(None).unwrap().is_empty());
}

#[tokio::test]
async fn failed_listing_deletes_nothing() {
    let h = harness().await;
    h.put("a/x.txt", b"1");
    h.put("b/y.txt", b"2");
    h.sync().await;
    let before = h.eng.db.count_items(h.d.id).unwrap();
    h.mock.fail_next("operations/list", ErrorKind::Network, 1);
    assert!(h.eng.poll_drive(h.d.id).await.is_err());
    assert_eq!(h.eng.db.count_items(h.d.id).unwrap(), before);
    assert!(h.is_placeholder("a/x.txt"));
}

// ---------------------------------------------------------------- remote index

#[tokio::test]
async fn remote_rename_detected_by_id_keeps_downloaded_copy() {
    let h = harness().await;
    h.put("folder/song.mp3", b"audio");
    h.sync().await;
    h.open("folder/song.mp3").await;
    h.mock.clear_calls();
    h.mock.rename(FS, "folder", "renamed");
    h.sync().await;
    assert_eq!(h.read_local("renamed/song.mp3").unwrap(), b"audio");
    assert_eq!(h.status("folder"), LocalStatus::Absent);
    assert_eq!(h.mock.count("operations/copyfile"), 0, "no re-download");
    assert_eq!(h.item("renamed/song.mp3").unwrap().local_state, LocalState::Downloaded);
}

#[tokio::test]
async fn unsyncable_names_are_listed_not_synced() {
    let mock = synclone_rclone::MockRclone::new();
    let h = harness_with(mock, None).await;
    // Rebuild with Windows rules on the provider.
    drop(h);
    let tmp = tempfile::tempdir().unwrap();
    let mock = synclone_rclone::MockRclone::new();
    mock.add_remote("r");
    let stub = std::sync::Arc::new(
        synclone_provider_stub::StubProvider::new(tmp.path().join("root"), tmp.path().join("rec"))
            .with_rules(NameRules::WINDOWS),
    );
    let db = std::sync::Arc::new(synclone_core::db::Db::open_in_memory().unwrap());
    let eng = Engine::new(
        db,
        std::sync::Arc::new(mock.clone()),
        stub.clone(),
        std::sync::Arc::new(TestClock::new(0)),
        EngineSettings::default(),
        tmp.path().join("state"),
    );
    let d = eng.connect_remote("r", None).await.unwrap();
    mock.put(FS, "ok.txt", b"1");
    mock.put(FS, "bad:name.txt", b"2");
    mock.put(FS, "CON.txt", b"3");
    mock.put(FS, "Case.txt", b"4");
    mock.put(FS, "case.txt", b"5");
    eng.sync_drive_now(d.id).await.unwrap();
    let bad: Vec<String> = eng
        .db
        .unsyncable_items(d.id)
        .unwrap()
        .into_iter()
        .map(|i| i.path.as_str().to_string())
        .collect();
    assert_eq!(bad, vec!["CON.txt", "Case.txt", "bad:name.txt", "case.txt"]);
    assert_eq!(stub.local_status(&d, &rp("ok.txt")).unwrap(), LocalStatus::Placeholder);
    assert_eq!(stub.local_status(&d, &rp("Case.txt")).unwrap(), LocalStatus::Absent);
    assert_eq!(mock.store(FS).files.len(), 5, "remote files are never renamed");
}

// ---------------------------------------------------------------- drives

#[tokio::test]
async fn disconnect_issues_zero_remote_write_calls() {
    let h = harness().await;
    h.put("a.txt", b"1");
    h.put("d/b.txt", b"2");
    h.sync().await;
    h.open("a.txt").await;
    std::fs::remove_file(h.local("d/b.txt.sclstub")).unwrap();
    h.sync().await;
    assert_eq!(h.eng.trash_list().unwrap().len(), 1);
    h.mock.clear_calls();
    h.eng.disconnect(h.d.id, false).await.unwrap();
    assert_eq!(h.mock.remote_write_calls(), 0, "calls: {:?}", h.mock.calls());
    assert!(
        h.mock.calls().is_empty(),
        "no remote calls at all: {:?}",
        h.mock.calls()
    );
    assert!(!h.stub.drive_dir(&h.d).exists(), "local folder removed");
    assert_eq!(h.remote("a.txt").unwrap(), b"1");
    assert_eq!(h.remote("d/b.txt").unwrap(), b"2", "trash queue was not emptied");
    let d = h.eng.drive(h.d.id).unwrap();
    assert_eq!(d.state, DriveState::Disconnected);
    // Sign-in kept: reconnecting works in one step.
    let d = h.eng.connect_remote("r", None).await.unwrap();
    assert_eq!(d.state, DriveState::Connected);
    h.sync().await;
    assert!(h.is_placeholder("a.txt"));
}

#[tokio::test]
async fn disconnect_with_unsynced_edits_blocks_or_recovers() {
    let h = harness().await;
    h.put("a.txt", b"1");
    h.sync().await;
    h.open("a.txt").await;
    h.write_local("a.txt", b"not uploaded yet");
    h.write_local("new.txt", b"also new");
    h.eng.scan_drive(h.d.id).unwrap();
    let err = h.eng.disconnect(h.d.id, false).await.unwrap_err().to_string();
    assert!(err.contains("a.txt") && err.contains("new.txt"), "{err}");
    assert!(h.stub.drive_dir(&h.d).exists(), "nothing removed");
    h.mock.clear_calls();
    let dir = h.eng.disconnect(h.d.id, true).await.unwrap().unwrap();
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"not uploaded yet");
    assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), b"also new");
    assert_eq!(h.mock.remote_write_calls(), 0);
}

#[tokio::test]
async fn deleting_drive_folder_is_disconnect_not_delete() {
    let h = harness().await;
    h.put("a.txt", b"1");
    h.put("b/c.txt", b"2");
    h.sync().await;
    h.mock.clear_calls();
    std::fs::remove_dir_all(h.stub.drive_dir(&h.d)).unwrap();
    h.sync().await;
    assert_eq!(h.mock.remote_write_calls(), 0);
    assert!(h.eng.trash_list().unwrap().is_empty(), "nothing in the trash queue");
    assert_eq!(h.eng.drive(h.d.id).unwrap().state, DriveState::Disconnected);
    assert_eq!(h.mock.store(FS).files.len(), 2);
}

#[tokio::test]
async fn import_existing_remotes_and_dedupe_names() {
    let h = harness().await;
    h.mock.add_remote("gdrive");
    h.mock.add_remote("work");
    let added = h.eng.import_remotes().await.unwrap();
    let names: Vec<_> = added.iter().map(|d| d.remote_name.as_str()).collect();
    assert_eq!(names, vec!["gdrive", "work"]);
    assert!(added.iter().all(|d| d.imported));
    assert!(h.eng.import_remotes().await.unwrap().is_empty(), "idempotent");
    assert_eq!(h.eng.unique_display_name("gdrive").unwrap(), "gdrive 2");
    // A forgotten imported remote is not re-imported.
    let w = h.eng.drive_by_name("work").unwrap();
    h.eng.forget_drive(w.id, false).await.unwrap();
    assert!(h.eng.import_remotes().await.unwrap().is_empty());
    assert!(h.mock.list_remotes_sync().contains(&"work".to_string()), "sign-in kept");
}
