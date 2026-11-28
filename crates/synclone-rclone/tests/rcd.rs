//! Integration tests against a real `rclone rcd` (layer 2).
//! Requires `scripts/fetch-rclone` to have been run (or SYNCLONE_RCLONE set).

use std::time::Duration;

use serde_json::{Map, Value};
use synclone_rclone::config_flow::{create_remote, Answer, FlowSettings};
use synclone_rclone::locate::find_rclone;
use synclone_rclone::*;

fn rclone() -> std::path::PathBuf {
    find_rclone().expect("rclone not found: run scripts/fetch-rclone or set SYNCLONE_RCLONE")
}

async fn start(dir: &tempfile::TempDir) -> std::sync::Arc<RcloneDaemon> {
    let mut cfg = DaemonConfig::new(rclone());
    // Data safety: never touch the user's real rclone config.
    let conf = dir.path().join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    cfg.config_file = Some(conf);
    cfg.log_file = Some(dir.path().join("rclone.log"));
    RcloneDaemon::start(cfg).await.expect("rclone rcd starts")
}

#[tokio::test]
async fn version_is_at_least_1_73() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let v = d.client().version().await.unwrap();
    assert!(v.at_least(1, 73), "rclone {} is too old", v.version);
    d.shutdown().await;
}

#[tokio::test]
async fn rc_is_unreachable_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let ep = d.endpoint().clone();
    assert!(ep.url.starts_with("http://127.0.0.1:"));
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    // No login.
    let r = http
        .post(format!("{}/core/version", ep.url))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // Wrong password.
    let r = http
        .post(format!("{}/core/version", ep.url))
        .basic_auth(&ep.user, Some("wrong"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // core/command (which runs OS commands) is protected too.
    let r = http
        .post(format!("{}/core/command", ep.url))
        .json(&serde_json::json!({"command": "version"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // The object server (--rc-serve) too.
    let r = http.get(format!("{}/%5Bx:%5D/a", ep.url)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // And the right login works.
    let wrong = RcClient::new(Endpoint {
        pass: "nope".into(),
        ..ep.clone()
    });
    assert_eq!(wrong.version().await.unwrap_err().kind, ErrorKind::Unauthorized);
    d.shutdown().await;
}

#[tokio::test]
async fn restarts_after_crash_and_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let pid1 = d.pid().await.unwrap();
    d.kill_for_test().await;
    // The supervisor notices and restarts on the same port and login.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(d.wait_running(Duration::from_secs(20)).await, "restarted");
    let mut ok = false;
    for _ in 0..100 {
        if d.client().version().await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "rc answers after restart");
    let pid2 = d.pid().await.unwrap();
    assert_ne!(pid1, pid2);
    d.shutdown().await;
    assert_eq!(d.state(), DaemonState::Stopped);
    assert!(d.client().version().await.is_err(), "rc is gone after shutdown");
}

#[tokio::test]
async fn file_operations_on_local_and_memory_backends() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let c = d.client();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("sub")).unwrap();
    std::fs::write(data.join("sub/a.txt"), b"hello").unwrap();

    let mut p = Map::new();
    p.insert("remote".into(), Value::String(data.display().to_string()));
    c.config_create(
        "loc",
        "alias",
        p,
        &ConfigOpts {
            non_interactive: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    c.config_create(
        "mem",
        "memory",
        Map::new(),
        &ConfigOpts {
            non_interactive: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut remotes = c.list_remotes().await.unwrap();
    remotes.sort();
    assert_eq!(remotes, vec!["loc", "mem"]);

    let all = c
        .list(
            "loc:",
            "",
            &ListOpts {
                recurse: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut paths: Vec<_> = all.iter().map(|e| e.path.clone()).collect();
    paths.sort();
    assert_eq!(paths, vec!["sub", "sub/a.txt"]);

    // Upload from a local directory to memory, then read back.
    c.copyfile(&data.display().to_string(), "sub/a.txt", "mem:", "x/y/a.txt")
        .await
        .unwrap();
    let st = c
        .stat(
            "mem:",
            "x/y/a.txt",
            &ListOpts {
                hash_types: vec!["md5".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(st.size, 5);
    assert_eq!(st.hash("md5"), Some("5d41402abc4b2a76b9719d911017c592"));
    assert!(c.stat("mem:", "nope", &ListOpts::default()).await.unwrap().is_none());
    assert!(c
        .stat("mem:", "no/such/dir/file", &ListOpts::default())
        .await
        .unwrap()
        .is_none());

    // Download.
    let dl = dir.path().join("dl");
    std::fs::create_dir_all(&dl).unwrap();
    c.copyfile("mem:", "x/y/a.txt", &dl.display().to_string(), "got.txt")
        .await
        .unwrap();
    assert_eq!(std::fs::read(dl.join("got.txt")).unwrap(), b"hello");

    // Move, delete, errors.
    c.movefile("mem:", "x/y/a.txt", "mem:", "x/b.txt").await.unwrap();
    assert!(c
        .stat("mem:", "x/y/a.txt", &ListOpts::default())
        .await
        .unwrap()
        .is_none());
    c.deletefile("mem:", "x/b.txt").await.unwrap();
    let e = c.deletefile("mem:", "x/b.txt").await.unwrap_err();
    assert!(e.is_not_found(), "{e}");
    c.mkdir("loc:", "newdir").await.unwrap();
    assert!(data.join("newdir").is_dir());
    c.rmdir("loc:", "newdir").await.unwrap();
    assert!(!data.join("newdir").exists());

    let info = c.fsinfo("mem:").await.unwrap();
    assert!(info.hashes.iter().any(|h| h == "md5"));
    let info = c.fsinfo("loc:").await.unwrap();
    assert!(info.feature("SlowHash"));
    assert_eq!(info.preferred_hash(), None);

    let provs = c.config_providers().await.unwrap();
    assert!(provs.iter().any(|p| p.name == "drime"), "Drime needs rclone >= 1.73");
    assert!(provs.iter().any(|p| p.name == "drive"));
    assert!(provs.iter().any(|p| p.name == "onedrive"));

    c.config_delete("mem").await.unwrap();
    assert_eq!(c.list_remotes().await.unwrap(), vec!["loc"]);
    let conf = std::fs::read_to_string(dir.path().join("rclone.conf")).unwrap();
    assert!(conf.contains("[loc]") && !conf.contains("[mem]"));
    d.shutdown().await;
}

#[tokio::test]
async fn config_flow_against_real_rclone() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let c = d.client();
    let mut ask = |_: &ConfigQuestion| Answer::Cancel;
    let mut ev = |_e: config_flow::FlowEvent| {};
    create_remote(
        c.as_ref(),
        "m",
        "memory",
        Map::new(),
        &mut ask,
        &mut ev,
        &FlowSettings::default(),
    )
    .await
    .unwrap();
    assert_eq!(c.list_remotes().await.unwrap(), vec!["m"]);
    // Refused before rclone is asked: would delete permanently.
    let mut bad = Map::new();
    bad.insert("hard_delete".into(), Value::String("true".into()));
    bad.insert("access_token".into(), Value::String("x".into()));
    assert!(create_remote(
        c.as_ref(),
        "dr",
        "drime",
        bad,
        &mut ask,
        &mut ev,
        &FlowSettings::default()
    )
    .await
    .is_err());
    assert_eq!(c.list_remotes().await.unwrap(), vec!["m"]);
    d.shutdown().await;
}

#[tokio::test]
async fn oauth_flow_starts_browser_server_and_can_be_cancelled() {
    // Drive's config asks config_is_local; answering true starts rclone's
    // local OAuth server. We read the URL, then cancel as a user would.
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let c = d.client();
    let c2 = c.clone();
    let canceller = tokio::spawn(async move {
        for _ in 0..200 {
            if let Ok(st) = c2.oauth_status().await {
                if st.is_running() {
                    let _ = c2.oauth_stop().await;
                    return st.auth_url;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    });
    let mut ask = |_: &ConfigQuestion| Answer::Cancel;
    let mut urls = Vec::new();
    let mut ev = |e: config_flow::FlowEvent| {
        if let config_flow::FlowEvent::OpenBrowser(u) = e {
            urls.push(u);
        }
    };
    let settings = FlowSettings {
        poll_interval: Duration::from_millis(20),
        ..Default::default()
    };
    let res = create_remote(c.as_ref(), "gd", "drive", Map::new(), &mut ask, &mut ev, &settings).await;
    let url = canceller.await.unwrap().expect("OAuth server started");
    assert!(url.starts_with("http://127.0.0.1:53682/auth"), "{url}");
    assert!(res.is_err(), "cancelled sign-in fails");
    assert!(c.list_remotes().await.unwrap().is_empty(), "half-made remote removed");
    d.shutdown().await;
}

/// Hydration on Windows streams objects through `--rc-serve` with Range
/// requests (D-003, D-021): whole reads, reads from an offset, names that
/// need percent-encoding, and a clean error for a missing object.
#[tokio::test]
async fn open_read_streams_objects_with_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let c = d.client();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("a dir")).unwrap();
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(data.join("a dir/f #1 [x] ü%.bin"), &body).unwrap();
    let mut p = Map::new();
    p.insert("remote".into(), Value::String(data.display().to_string()));
    let opts = ConfigOpts {
        non_interactive: true,
        ..Default::default()
    };
    c.config_create("loc", "alias", p, &opts).await.unwrap();

    async fn read_all(mut r: client::ObjectReader) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(b) = r.chunk().await.unwrap() {
            out.extend_from_slice(&b);
        }
        out
    }
    let name = "a dir/f #1 [x] ü%.bin";
    let r = c.open_read("loc:", name, 0).await.unwrap();
    assert_eq!(r.total, body.len() as u64);
    assert!(r.last_modified.is_some());
    assert_eq!(read_all(r).await, body);

    let r = c.open_read("loc:", name, 123_457).await.unwrap();
    assert_eq!(r.total, body.len() as u64);
    assert_eq!(r.offset, 123_457);
    assert_eq!(read_all(r).await, &body[123_457..]);

    let e = c
        .open_read("loc:", "a dir/missing", 0)
        .await
        .err()
        .expect("missing object fails");
    assert_eq!(e.kind, ErrorKind::NotFound, "{e}");
    d.shutdown().await;
}
