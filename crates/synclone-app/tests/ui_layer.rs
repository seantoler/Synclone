//! The UI layer end to end (PLAN.md §10 Phase 5): the exact calls the
//! windows make (`Client::ui_call`, `Client::add_drive`), against a real
//! daemon and rclone. Covers the wizard's data flow and the Trash window's
//! restore and empty.

mod common;

use common::*;
use serde_json::json;

/// What the wizard does for an "Other" service: read the form, suggest a
/// name, add the drive with the filled-in settings, rename it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_flow_adds_a_drive_from_a_generated_form() {
    let d = daemon().await;
    let c = &d.client;
    let services = c.ui_call("drive.services", json!({})).await.unwrap();
    let types: Vec<&str> = services
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["type"].as_str())
        .collect();
    assert!(types.contains(&"drive") && types.contains(&"dropbox"));
    // Wrappers and the local disk are advanced mode only (§6.4).
    for t in ["alias", "crypt", "union", "local"] {
        assert!(!types.contains(&t), "{t} offered by default");
    }
    let adv = c.ui_call("drive.services", json!({"advanced": true})).await.unwrap();
    assert!(adv.as_array().unwrap().iter().any(|s| s["type"] == "alias"));
    let form = c
        .ui_call("drive.service_options", json!({"type": "alias"}))
        .await
        .unwrap();
    assert_eq!(form["auth"], "form");
    let remote = form["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "remote")
        .expect("the alias form asks where the files are");
    assert_eq!(remote["required"], true);
    assert_eq!(remote["advanced"], false);
    // The three featured services sign in the right way.
    for (t, auth) in [("drive", "browser"), ("onedrive", "browser"), ("drime", "token")] {
        let f = c.ui_call("drive.service_options", json!({"type": t})).await.unwrap();
        assert_eq!(f["auth"], auth, "{t}");
        // Hard-delete options are never offered (D-008).
        assert!(!f["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["name"] == "hard_delete"));
    }
    let name = c.ui_call("drive.suggest_name", json!({"type": "alias"})).await.unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    let mut opened = Vec::new();
    let added = c
        .add_drive(
            json!({"type": "alias", "name": name, "params": {"remote": d.cloud().display().to_string()}}),
            &mut |u| opened.push(u),
        )
        .await
        .unwrap();
    assert!(opened.is_empty(), "no browser for a form service");
    let id = added["id"].as_i64().unwrap();
    let renamed = c
        .ui_call("drive.rename", json!({"drive_id": id, "new_name": "My Files"}))
        .await
        .unwrap();
    assert_eq!(renamed["name"], "My Files");
    let drives = c.ui_call("drive.list", json!({})).await.unwrap();
    let dr = drives
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == id)
        .unwrap()
        .clone();
    assert!(std::path::Path::new(dr["folder"].as_str().unwrap()).is_dir());
    // The next suggestion does not clash.
    let again = c.ui_call("drive.suggest_name", json!({"type": "alias"})).await.unwrap();
    assert_eq!(again["name"], name, "the first name is free again after the rename");
}

/// Trash window: a local delete waits in the trash with the cloud copy
/// intact; Restore brings it back; Empty deletes it from the cloud.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trash_restore_and_empty_from_the_ui_layer() {
    let d = daemon().await;
    let c = &d.client;
    std::fs::write(d.cloud().join("keep.txt"), b"keep").unwrap();
    std::fs::write(d.cloud().join("old.txt"), b"old").unwrap();
    let added = c
        .add_drive(
            json!({"type": "alias", "name": "Cloud", "params": {"remote": d.cloud().display().to_string()}}),
            &mut |_| {},
        )
        .await
        .unwrap();
    let folder = std::path::PathBuf::from(added["folder"].as_str().unwrap());
    c.ui_call("sync", json!({})).await.unwrap();
    let stub = |n: &str| folder.join(format!("{n}{}", synclone_provider_stub_ext()));
    assert!(stub("old.txt").exists(), "placeholder present");

    // Delete locally: into the trash, nothing sent.
    std::fs::remove_file(stub("old.txt")).unwrap();
    wait_for("trash entry", 20, || async {
        c.ui_call("trash.list", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == 1
    })
    .await;
    let list = c.ui_call("trash.list", json!({})).await.unwrap();
    assert_eq!(list[0]["path"], "old.txt");
    assert_eq!(list[0]["drive"], "Cloud");
    assert!(d.cloud().join("old.txt").exists());
    let st = c.ui_call("status", json!({})).await.unwrap();
    assert_eq!(st["trash"]["count"], 1);

    // Restore selected.
    let id = list[0]["id"].as_i64().unwrap();
    c.ui_call("trash.restore", json!({"id": id})).await.unwrap();
    wait_for("placeholder restored", 20, || async { stub("old.txt").exists() }).await;
    assert!(c
        .ui_call("trash.list", json!({}))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());

    // Delete again and empty the trash: gone from the cloud.
    std::fs::remove_file(stub("old.txt")).unwrap();
    wait_for("trash entry again", 20, || async {
        c.ui_call("trash.list", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == 1
    })
    .await;
    // Without confirmation nothing happens.
    assert!(c.ui_call("trash.empty", json!({})).await.is_err());
    assert!(d.cloud().join("old.txt").exists());
    let r = c.ui_call("trash.empty", json!({"confirm": true})).await.unwrap();
    assert_eq!(r["emptied"]["count"], 1);
    assert!(!d.cloud().join("old.txt").exists());
    assert!(d.cloud().join("keep.txt").exists());
}

/// Windows can reach only the methods they need: not `shutdown`, not
/// `ops`, not `drive.add` (which goes through its own command).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_cannot_call_methods_outside_the_allowlist() {
    let d = daemon().await;
    for m in ["shutdown", "ops", "drive.add", "download", "free"] {
        let e = d.client.ui_call(m, json!({})).await.unwrap_err();
        assert!(e.to_string().contains("not available"), "{m}: {e}");
    }
    assert!(synclone_app::daemon::is_running(&d.client).await);
}

/// Settings save and apply: bandwidth limit, poll interval, ignore list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_round_trip() {
    let d = daemon().await;
    let c = &d.client;
    let s = c.ui_call("settings.get", json!({})).await.unwrap();
    assert_eq!(s["bandwidth_limit"], "off");
    assert_eq!(s["poll_minutes"], 5);
    let s = c
        .ui_call(
            "settings.set",
            json!({"bandwidth_limit": "2M", "poll_minutes": 15, "ignore": ["*.bak", " ", "build/"]}),
        )
        .await
        .unwrap();
    assert_eq!(s["bandwidth_limit"], "2M");
    assert_eq!(s["poll_minutes"], 15);
    assert_eq!(s["ignore"], json!(["*.bak", "build/"]));
    let s = c.ui_call("settings.set", json!({"bandwidth_limit": ""})).await.unwrap();
    assert_eq!(s["bandwidth_limit"], "off");
}

fn synclone_provider_stub_ext() -> &'static str {
    ".sclstub"
}

/// Daemon lifecycle: the app starts its service (`Synclone --service`)
/// when none answers, and Quit stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_starts_its_service_and_quit_stops_it() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("rclone.conf"), "").unwrap();
    let socket = if cfg!(windows) {
        format!(r"\\.\pipe\synclone-app-lifecycle-{}", std::process::id())
    } else {
        tmp.path().join("d.sock").display().to_string()
    };
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_Synclone"));
    cmd.arg("--service")
        .env("SYNCLONE_SOCKET", &socket)
        .env("SYNCLONE_STATE_DIR", tmp.path().join("state"))
        .env("SYNCLONE_ROOT", tmp.path().join("root"))
        .env("SYNCLONE_RCLONE_CONFIG", tmp.path().join("rclone.conf"))
        .env("RCLONE_CONFIG", tmp.path().join("rclone.conf"))
        .env("SYNCLONE_RCLONE", synclone_rclone::locate::find_rclone().unwrap())
        .env("SYNCLONE_PROVIDER", "stub");
    let c = synclone_app::Client::new(socket);
    assert!(!synclone_app::daemon::is_running(&c).await);
    let logs = tmp.path().join("state").join("logs");
    let started = synclone_app::daemon::ensure_running_with(&c, cmd, &logs).await;
    assert!(
        started.as_ref().map(|s| *s).unwrap_or(false),
        "started it: {started:?}
{}",
        std::fs::read_to_string(logs.join("daemon.log")).unwrap_or_default()
    );
    let st = c.call("status", json!({})).await.unwrap();
    assert_eq!(st["app"], synclone_core::APP_NAME);
    // Already running: used as is.
    let again = std::process::Command::new("does-not-exist");
    assert!(!synclone_app::daemon::ensure_running_with(&c, again, &logs)
        .await
        .unwrap());
    synclone_app::daemon::stop(&c).await;
    wait_for("service stopped", 20, || async {
        !synclone_app::daemon::is_running(&c).await
    })
    .await;
}
