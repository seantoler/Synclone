//! One named test per row of the conflict table (PLAN.md §5), plus the
//! trash queue, pin/free and drive lifecycle rules from §1.1 and §4.

mod common;
use common::*;
use synclone_core::*;

#[tokio::test]
async fn rule_01_remote_changed_placeholder_updates_metadata_only() {
    let h = harness().await;
    h.put("doc.txt", b"v1");
    h.sync().await;
    assert!(h.is_placeholder("doc.txt"));
    h.put("doc.txt", b"version two");
    h.mock.clear_calls();
    h.sync().await;
    assert!(h.is_placeholder("doc.txt"), "still not downloaded");
    assert_eq!(h.item("doc.txt").unwrap().remote.unwrap().size, 11);
    assert_eq!(h.mock.count("operations/copyfile"), 0, "nothing transferred");
}

#[tokio::test]
async fn rule_02_remote_changed_local_clean_is_replaced() {
    let h = harness().await;
    h.put("a.txt", b"one");
    h.sync().await;
    h.open("a.txt").await;
    assert_eq!(h.read_local("a.txt").unwrap(), b"one");
    h.put("a.txt", b"two!");
    h.sync().await;
    assert_eq!(h.read_local("a.txt").unwrap(), b"two!");
    assert!(h.conflict_copies("").is_empty());
}

#[tokio::test]
async fn rule_03_remote_changed_local_open_is_deferred_until_closed() {
    let h = harness().await;
    h.put("a.txt", b"one");
    h.sync().await;
    h.open("a.txt").await;
    h.stub.set_open(h.d.id, &rp("a.txt"), true);
    h.put("a.txt", b"two!");
    h.eng.poll_drive(h.d.id).await.unwrap();
    h.eng.run_until_idle(false, 5).await.unwrap();
    assert_eq!(h.read_local("a.txt").unwrap(), b"one", "open file not replaced");
    assert_eq!(
        h.eng.db.ops(Some("pending")).unwrap().len(),
        1,
        "retry kept in the journal"
    );
    h.stub.set_open(h.d.id, &rp("a.txt"), false);
    h.eng.run_until_idle(true, 10).await.unwrap();
    assert_eq!(h.read_local("a.txt").unwrap(), b"two!");
}

#[tokio::test]
async fn rule_03_then_04_open_file_edited_before_close_keeps_both() {
    let h = harness().await;
    h.put("a.txt", b"one");
    h.sync().await;
    h.open("a.txt").await;
    h.stub.set_open(h.d.id, &rp("a.txt"), true);
    h.put("a.txt", b"remote edit");
    h.sync().await;
    h.write_local("a.txt", b"local edit");
    h.stub.set_open(h.d.id, &rp("a.txt"), false);
    h.sync().await;
    assert_eq!(h.read_local("a.txt").unwrap(), b"remote edit");
    let copies = h.conflict_copies("");
    assert_eq!(copies.len(), 1);
    assert_eq!(h.read_local(&copies[0]).unwrap(), b"local edit");
}

#[tokio::test]
async fn rule_04_both_changed_keeps_both() {
    let h = harness().await;
    h.put("dir/report.docx", b"base");
    h.sync().await;
    h.open("dir/report.docx").await;
    h.put("dir/report.docx", b"remote version");
    h.write_local("dir/report.docx", b"local version");
    h.sync().await;
    // Canonical name holds the remote version, locally and remotely.
    assert_eq!(h.read_local("dir/report.docx").unwrap(), b"remote version");
    assert_eq!(h.remote("dir/report.docx").unwrap(), b"remote version");
    let copies = h.conflict_copies("dir");
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert!(copies[0].starts_with("report (conflict - ") && copies[0].ends_with(").docx"));
    assert_eq!(h.read_local(&format!("dir/{}", copies[0])).unwrap(), b"local version");
    assert_eq!(
        h.remote(&format!("dir/{}", copies[0])).unwrap(),
        b"local version",
        "conflict copy uploaded"
    );
    let c = h.eng.db.conflicts(false).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, rp("dir/report.docx"));
    // A second sync changes nothing.
    h.sync().await;
    assert_eq!(h.conflict_copies("dir").len(), 1);
}

#[tokio::test]
async fn rule_05_local_edit_remote_unchanged_uploads() {
    let h = harness().await;
    h.put("a.txt", b"one");
    h.sync().await;
    h.open("a.txt").await;
    h.write_local("a.txt", b"edited here");
    h.sync().await;
    assert_eq!(h.remote("a.txt").unwrap(), b"edited here");
    let it = h.item("a.txt").unwrap();
    assert!(!it.dirty);
    assert_eq!(it.synced.unwrap().size, 11);
}

#[tokio::test]
async fn rule_06_remote_deleted_local_clean_goes_to_os_trash() {
    let h = harness().await;
    h.put("a.txt", b"keep me safe");
    h.put("b.txt", b"placeholder only");
    h.sync().await;
    h.open("a.txt").await;
    h.mock.remove(FS, "a.txt");
    h.mock.remove(FS, "b.txt");
    h.sync().await;
    assert_eq!(h.status("a.txt"), LocalStatus::Absent);
    assert_eq!(h.status("b.txt"), LocalStatus::Absent);
    assert!(h.item("a.txt").is_none());
    assert_eq!(
        h.recycled(),
        vec![b"keep me safe".to_vec()],
        "downloaded copy is in the OS trash, not destroyed"
    );
}

#[tokio::test]
async fn rule_07_remote_deleted_local_dirty_is_uploaded_again() {
    let h = harness().await;
    h.put("a.txt", b"one");
    h.sync().await;
    h.open("a.txt").await;
    h.write_local("a.txt", b"my edits");
    h.mock.remove(FS, "a.txt");
    h.sync().await;
    assert_eq!(h.read_local("a.txt").unwrap(), b"my edits");
    assert_eq!(h.remote("a.txt").unwrap(), b"my edits");
}

#[tokio::test]
async fn rule_08_trashed_then_remote_changed_cancels_trash() {
    let h = harness().await;
    h.put("a.txt", b"one");
    h.sync().await;
    std::fs::remove_file(h.local("a.txt.sclstub")).unwrap();
    h.sync().await;
    assert_eq!(h.eng.trash_list().unwrap().len(), 1);
    assert_eq!(h.status("a.txt"), LocalStatus::Absent, "hidden while in the trash");
    // Polling does not bring it back while it is in the trash.
    h.sync().await;
    assert_eq!(h.status("a.txt"), LocalStatus::Absent);
    h.put("a.txt", b"changed elsewhere");
    h.sync().await;
    assert!(h.eng.trash_list().unwrap().is_empty(), "trash entry cancelled");
    assert!(h.is_placeholder("a.txt"), "placeholder restored");
    let all = h.eng.db.trash(None).unwrap();
    assert_eq!(all[0].status, "cancelled");
    assert_eq!(h.remote("a.txt").unwrap(), b"changed elsewhere");
}

#[tokio::test]
async fn rule_09_same_new_name_both_sides_keeps_both() {
    let h = harness().await;
    h.sync().await;
    h.write_local("new.txt", b"made here");
    h.put("new.txt", b"made there!");
    h.sync().await;
    assert_eq!(h.read_local("new.txt").unwrap(), b"made there!");
    assert_eq!(h.remote("new.txt").unwrap(), b"made there!");
    let copies = h.conflict_copies("");
    assert_eq!(copies.len(), 1);
    assert_eq!(h.remote(&copies[0]).unwrap(), b"made here");
}

#[tokio::test]
async fn rule_09_same_new_name_same_content_is_not_a_conflict() {
    let h = harness().await;
    h.sync().await;
    h.write_local("same.txt", b"identical");
    h.put("same.txt", b"identical");
    h.sync().await;
    assert!(h.conflict_copies("").is_empty());
    assert!(!h.item("same.txt").unwrap().dirty);
}

#[tokio::test]
async fn rule_10_local_rename_remote_content_changed() {
    let h = harness().await;
    h.put("old.txt", b"one");
    h.sync().await;
    h.open("old.txt").await;
    // Rename locally, while the content changes remotely.
    std::fs::rename(h.local("old.txt"), h.local("new.txt")).unwrap();
    h.put("old.txt", b"two, from the cloud");
    h.eng
        .handle_event(ProviderEvent::LocalRenamed {
            drive: h.d.id,
            from: rp("old.txt"),
            to: rp("new.txt"),
        })
        .unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert!(h.remote("old.txt").is_none(), "renamed server-side");
    assert_eq!(h.remote("new.txt").unwrap(), b"two, from the cloud");
    assert_eq!(h.read_local("new.txt").unwrap(), b"two, from the cloud", "then rule 2");
    assert!(h.conflict_copies("").is_empty());
    assert_eq!(h.mock.count("operations/movefile"), 1);
}

#[tokio::test]
async fn rule_11_both_renamed_remote_name_wins() {
    let h = harness().await;
    h.put("a.txt", b"content");
    h.sync().await;
    h.open("a.txt").await;
    std::fs::rename(h.local("a.txt"), h.local("local-name.txt")).unwrap();
    h.mock.rename(FS, "a.txt", "cloud-name.txt");
    h.eng
        .handle_event(ProviderEvent::LocalRenamed {
            drive: h.d.id,
            from: rp("a.txt"),
            to: rp("local-name.txt"),
        })
        .unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    h.sync().await;
    assert_eq!(h.read_local("cloud-name.txt").unwrap(), b"content");
    assert_eq!(h.status("local-name.txt"), LocalStatus::Absent);
    assert_eq!(h.remote("cloud-name.txt").unwrap(), b"content");
    assert!(h.remote("local-name.txt").is_none());
    assert_eq!(h.mock.store(FS).files.len(), 1, "no data duplicated or lost");
}

#[tokio::test]
async fn rule_11_poll_first_also_keeps_remote_name() {
    let h = harness().await;
    h.put("a.txt", b"content");
    h.sync().await;
    h.open("a.txt").await;
    std::fs::rename(h.local("a.txt"), h.local("local-name.txt")).unwrap();
    h.eng
        .handle_event(ProviderEvent::LocalRenamed {
            drive: h.d.id,
            from: rp("a.txt"),
            to: rp("local-name.txt"),
        })
        .unwrap();
    h.mock.rename(FS, "a.txt", "cloud-name.txt");
    // The poll sees the remote rename while the local move is still debounced.
    h.eng.poll_drive(h.d.id).await.unwrap();
    h.eng.run_until_idle(true, 20).await.unwrap();
    assert_eq!(h.read_local("cloud-name.txt").unwrap(), b"content");
    assert_eq!(h.status("local-name.txt"), LocalStatus::Absent);
    assert_eq!(h.mock.store(FS).files.len(), 1);
}
