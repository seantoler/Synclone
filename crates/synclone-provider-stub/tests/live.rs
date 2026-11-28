//! Layer 4: live cloud smoke tests (PLAN.md §7, D-029). Opt-in:
//!
//! ```text
//! SYNCLONE_LIVE_TEST_REMOTES=gdrive-test1,onedrive-test
//! SYNCLONE_LIVE_TEST_CONFIG=<a config file holding only those remotes>
//! ```
//!
//! The test never reads the user's real rclone config. It copies
//! `SYNCLONE_LIVE_TEST_CONFIG` (made by `scripts/export-live-test-config.ps1`)
//! into a temp folder and runs rclone on that copy. Two barriers protect
//! the user's files:
//! 1. Every listed remote must be an `alias` whose target contains
//!    `_synclone-test`; anything else is refused before any call.
//! 2. All work happens in a per-run folder, `run-<time>-<pid>`, reached
//!    through its own alias and the [`TestRoot`] guard, which refuses
//!    every call outside it.
//!
//! One round trip per remote: upload, cloud change and list, download,
//! modify, conflict, trash, empty, cleanup. After cleanup the test checks
//! that the rest of `_synclone-test` looks exactly as it did before.
//!
//! `local_alias_round_trip` runs the same code on every CI run, against an
//! alias into a temp folder, so the harness itself stays tested.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use common::{bump_mtime, rp};
use serde_json::{json, Map, Value};
use synclone_core::db::Db;
use synclone_core::*;
use synclone_provider_stub::StubProvider;
use synclone_rclone::{ConfigOpts, DaemonConfig, ListOpts, RcClient, RcloneApi, RcloneDaemon, TestRoot};

const RUN_REMOTE: &str = "synclone-live-run";

/// Barrier 1: only aliases into a `_synclone-test` folder.
async fn check_alias(rc: &RcClient, name: &str) -> Result<String> {
    if !rc.list_remotes().await?.iter().any(|r| r == name) {
        bail!("{name}: no such remote in the live-test config");
    }
    let cfg = rc.config_get(name).await?;
    let kind = cfg.get("type").and_then(Value::as_str).unwrap_or("");
    let target = cfg.get("remote").and_then(Value::as_str).unwrap_or("");
    if kind != "alias" {
        bail!("{name}: refused, it is a {kind:?} remote, not an alias into a _synclone-test folder");
    }
    let parts: Vec<&str> = target.split(['/', '\\', ':']).collect();
    if !parts.contains(&"_synclone-test") || parts.contains(&"..") {
        bail!("{name}: refused, its target {target:?} is not a _synclone-test folder");
    }
    Ok(target.to_string())
}

struct Run {
    tmp: tempfile::TempDir,
    daemon: Arc<RcloneDaemon>,
    rc: Arc<RcClient>,
}

impl Run {
    /// rclone on a temp copy of `config` (never the user's own file).
    async fn start(config_text: &str) -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let conf = tmp.path().join("rclone.conf");
        std::fs::write(&conf, config_text)?;
        let mut cfg = DaemonConfig::new(synclone_rclone::locate::find_rclone().context("rclone not found")?);
        cfg.config_file = Some(conf);
        cfg.log_file = Some(tmp.path().join("rclone.log"));
        let daemon = RcloneDaemon::start(cfg).await?;
        let rc = daemon.client();
        Ok(Self { tmp, daemon, rc })
    }
}

/// What the test sees of the cloud, through the guard only.
struct Cloud {
    api: Arc<dyn RcloneApi>,
    fs: String,
    src: PathBuf,
    dl: PathBuf,
    n: std::sync::atomic::AtomicU32,
}

impl Cloud {
    async fn put(&self, p: &str, data: &[u8]) -> Result<()> {
        let n = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("up-{n}");
        std::fs::write(self.src.join(&name), data)?;
        self.api
            .copyfile(&self.src.display().to_string(), &name, &self.fs, p)
            .await?;
        Ok(())
    }
    async fn get(&self, p: &str) -> Result<Option<Vec<u8>>> {
        if self.api.stat(&self.fs, p, &ListOpts::default()).await?.is_none() {
            return Ok(None);
        }
        let n = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("dl-{n}");
        self.api
            .copyfile(&self.fs, p, &self.dl.display().to_string(), &name)
            .await?;
        Ok(Some(std::fs::read(self.dl.join(&name))?))
    }
    async fn names(&self, dir: &str) -> Result<Vec<String>> {
        Ok(self
            .api
            .list(&self.fs, dir, &ListOpts::default())
            .await?
            .into_iter()
            .map(|e| e.name)
            .collect())
    }
}

/// Retry `f` (after a sync) until it holds, for up to a minute: listings
/// on some services lag a write by a few seconds.
async fn eventually<F, Fut>(what: &str, eng: &Engine, d: &Drive, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let t = Instant::now();
    loop {
        eng.sync_drive_now(d.id).await?;
        if f().await? {
            return Ok(());
        }
        if t.elapsed() > Duration::from_secs(60) {
            bail!("timed out: {what}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn write_local(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, data)?;
    bump_mtime(path);
    Ok(())
}

/// One full round trip against `alias`. Always cleans up its run folder.
async fn round_trip(run: &Run, alias: &str) -> Result<Vec<(String, Duration)>> {
    check_alias(&run.rc, alias).await?;
    // The run folder: new, and reached only through its own alias.
    let stamp = format!(
        "run-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        std::process::id()
    );
    let mut before = names_at(&run.rc, alias).await?;
    ensure!(!before.contains(&stamp), "{stamp} already exists");
    before.sort();
    let mut params = Map::new();
    params.insert("remote".into(), json!(format!("{alias}:{stamp}")));
    let _ = run.rc.config_delete(RUN_REMOTE).await;
    run.rc
        .config_create(RUN_REMOTE, "alias", params, &ConfigOpts::default())
        .await?;
    let guarded: Arc<dyn RcloneApi> = Arc::new(TestRoot::new(
        run.rc.clone() as Arc<dyn RcloneApi>,
        RUN_REMOTE,
        vec![run.tmp.path().to_path_buf()],
    ));
    guarded.mkdir(&format!("{RUN_REMOTE}:"), "").await?;

    let result = steps(run, guarded.clone()).await;

    // Cleanup, through the guard, whatever happened above.
    let cleanup = guarded.purge(&format!("{RUN_REMOTE}:"), "").await;
    let _ = run.rc.config_delete(RUN_REMOTE).await;
    let mut after = names_at(&run.rc, alias).await?;
    after.sort();
    let steps = result?;
    cleanup.context("removing the run folder")?;
    ensure!(
        after == before,
        "the rest of the test folder changed: before {before:?}, after {after:?}"
    );
    Ok(steps)
}

/// Top-level names in the alias's folder (read-only; nothing is listed
/// above it, since an alias cannot reach above its target).
async fn names_at(rc: &RcClient, alias: &str) -> Result<Vec<String>> {
    Ok(rc
        .list(&format!("{alias}:"), "", &ListOpts::default())
        .await?
        .into_iter()
        .map(|e| e.name)
        .collect())
}

async fn steps(run: &Run, api: Arc<dyn RcloneApi>) -> Result<Vec<(String, Duration)>> {
    let base = run.tmp.path();
    for d in ["src", "dl", "state"] {
        std::fs::create_dir_all(base.join(d))?;
    }
    let cloud = Cloud {
        api: api.clone(),
        fs: format!("{RUN_REMOTE}:"),
        src: base.join("src"),
        dl: base.join("dl"),
        n: Default::default(),
    };
    let stub = Arc::new(StubProvider::new(base.join("Synclone"), base.join("recycle")));
    let eng = Engine::new(
        Arc::new(Db::open(&base.join("state").join("state.db"))?),
        api.clone(),
        stub.clone(),
        Arc::new(TestClock::new(1_000_000)),
        EngineSettings {
            concurrency: 2,
            ..Default::default()
        },
        base.join("state"),
    );
    let d = eng.connect_remote(RUN_REMOTE, Some("Live")).await?;
    let local = |p: &str| stub.path_of(&d, &rp(p));
    let mut times = Vec::new();
    let mut t = Instant::now();
    let mut lap = |name: &str, t: &mut Instant| {
        times.push((name.to_string(), t.elapsed()));
        *t = Instant::now();
    };

    // Upload.
    write_local(&local("up/hello.txt"), b"hello from Synclone")?;
    eventually("upload", &eng, &d, || async {
        Ok(cloud.get("up/hello.txt").await?.as_deref() == Some(&b"hello from Synclone"[..]))
    })
    .await?;
    lap("upload", &mut t);

    // A file added in the cloud appears as a placeholder (list).
    cloud.put("down/cloud.txt", b"made in the cloud").await?;
    eventually("placeholder for a cloud file", &eng, &d, || async {
        Ok(stub.local_status(&d, &rp("down/cloud.txt"))? == LocalStatus::Placeholder)
    })
    .await?;
    lap("cloud change and list", &mut t);

    // Download on open.
    eng.handle_event(ProviderEvent::HydrationRequested {
        drive: d.id,
        path: rp("down/cloud.txt"),
    })?;
    eng.run_until_idle(true, 50).await?;
    ensure!(
        std::fs::read(local("down/cloud.txt")).ok().as_deref() == Some(&b"made in the cloud"[..]),
        "download"
    );
    lap("download", &mut t);

    // Modify locally.
    write_local(&local("up/hello.txt"), b"hello again, edited here")?;
    eventually("modified upload", &eng, &d, || async {
        Ok(cloud.get("up/hello.txt").await?.as_deref() == Some(&b"hello again, edited here"[..]))
    })
    .await?;
    lap("modify", &mut t);

    // Conflict: changed in the cloud and here before a sync. Both kept.
    cloud.put("up/hello.txt", b"cloud edit").await?;
    write_local(&local("up/hello.txt"), b"local edit, a longer one")?;
    eventually("conflict copy uploaded", &eng, &d, || async {
        Ok(cloud.names("up").await?.iter().any(|n| n.contains("(conflict - ")))
    })
    .await?;
    ensure!(
        std::fs::read(local("up/hello.txt"))? == b"cloud edit",
        "the cloud version keeps the name"
    );
    let copy = std::fs::read_dir(local("up"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.contains("(conflict - "))
        .context("a local conflict copy")?;
    ensure!(std::fs::read(local(&format!("up/{copy}")))? == b"local edit, a longer one");
    ensure!(
        cloud.get(&format!("up/{copy}")).await?.as_deref() == Some(&b"local edit, a longer one"[..]),
        "conflict copy in the cloud"
    );
    ensure!(eng.db.conflicts(false)?.len() == 1, "one conflict recorded");
    lap("conflict", &mut t);

    // Trash: a local delete waits; the cloud copy stays.
    std::fs::remove_file(local("down/cloud.txt"))?;
    eventually("trash entry", &eng, &d, || async { Ok(eng.trash_list()?.len() == 1) }).await?;
    ensure!(
        cloud.get("down/cloud.txt").await?.is_some(),
        "the cloud copy waits in the trash"
    );
    lap("trash", &mut t);

    // Empty: now it goes from the cloud.
    eng.request_empty_trash()?;
    eng.run_until_idle(true, 50).await?;
    eventually("emptied", &eng, &d, || async {
        Ok(cloud.get("down/cloud.txt").await?.is_none())
    })
    .await?;
    ensure!(eng.trash_list()?.is_empty());
    ensure!(cloud.get("up/hello.txt").await?.is_some(), "other files untouched");
    let failed = eng.db.ops(Some("failed"))?;
    ensure!(failed.is_empty(), "failed ops: {failed:?}");
    lap("empty trash", &mut t);
    Ok(times)
}

fn report(alias: &str, r: &Result<Vec<(String, Duration)>>) -> String {
    match r {
        Ok(steps) => {
            let s: Vec<String> = steps
                .iter()
                .map(|(n, t)| format!("{n} {:.1}s", t.as_secs_f64()))
                .collect();
            format!("{alias}: ok ({})", s.join(", "))
        }
        Err(e) => format!("{alias}: FAILED: {e:#}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_cloud_round_trip() {
    let Some(names) = std::env::var("SYNCLONE_LIVE_TEST_REMOTES")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!(
            "SKIPPED: set SYNCLONE_LIVE_TEST_REMOTES (and SYNCLONE_LIVE_TEST_CONFIG) to run the live cloud tests"
        );
        return;
    };
    let conf = std::env::var_os("SYNCLONE_LIVE_TEST_CONFIG")
        .expect("SYNCLONE_LIVE_TEST_CONFIG must name the file from scripts/export-live-test-config.ps1");
    let text = std::fs::read_to_string(&conf).expect("reading the live-test config");
    let mut lines = Vec::new();
    for alias in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let run = Run::start(&text).await.unwrap();
        let r = round_trip(&run, alias).await;
        lines.push(report(alias, &r));
        eprintln!("{}", lines.last().unwrap());
        run.daemon.shutdown().await;
    }
    assert!(
        lines.iter().all(|l| !l.contains("FAILED")),
        "live round trips:\n{}",
        lines.join("\n")
    );
}

/// The harness on every run: an alias into a temp `_synclone-test`
/// folder, plus the remotes barrier 1 must refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_alias_round_trip() {
    let cloud = tempfile::tempdir().unwrap();
    let test_dir = cloud.path().join("_synclone-test");
    let other = cloud.path().join("other");
    std::fs::create_dir_all(&test_dir).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(test_dir.join("existing.txt"), b"left alone").unwrap();
    std::fs::write(other.join("precious.txt"), b"never touched").unwrap();
    let conf = format!(
        "[fake-test]\ntype = alias\nremote = {}\n\n[wrong-folder]\ntype = alias\nremote = {}\n\n[plain]\ntype = local\n",
        test_dir.display(),
        other.display()
    );
    let run = Run::start(&conf).await.unwrap();
    for refused in ["plain", "wrong-folder", "missing"] {
        let e = round_trip(&run, refused).await.unwrap_err().to_string();
        assert!(e.contains("refused") || e.contains("no such remote"), "{refused}: {e}");
    }
    let r = round_trip(&run, "fake-test").await;
    eprintln!("{}", report("fake-test", &r));
    r.unwrap();
    // Only the run folder came and went.
    let left: Vec<String> = std::fs::read_dir(&test_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(left, vec!["existing.txt".to_string()]);
    assert_eq!(std::fs::read(test_dir.join("existing.txt")).unwrap(), b"left alone");
    assert_eq!(std::fs::read(other.join("precious.txt")).unwrap(), b"never touched");
    run.daemon.shutdown().await;
}
