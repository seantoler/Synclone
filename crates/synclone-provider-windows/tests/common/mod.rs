//! Layer 3 harness (PLAN.md §7): a real sync root registered with
//! Windows, backed by real `rclone rcd` with an `alias` remote onto a temp
//! "cloud" folder, driven with ordinary file APIs from another process.
//!
//! Everything lives under Cargo's per-target temp dir inside the repo
//! (`target/tmp`): Windows refuses sync roots under %TEMP% (D-021). The
//! rclone config is a fresh file there; the user's real one is never read.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use synclone_core::db::Db;
use synclone_core::*;
use synclone_provider_windows::{WindowsProvider, WindowsProviderConfig};
use synclone_rclone::{ConfigOpts, DaemonConfig, RcloneApi, RcloneDaemon};

pub struct H {
    pub base: PathBuf,
    /// The fake cloud: what the `alias` remote points at.
    pub cloud: PathBuf,
    /// The Synclone folder (sync root).
    pub root: PathBuf,
    pub rclone: Arc<RcloneDaemon>,
    pub eng: Arc<Engine>,
    pub prov: Arc<WindowsProvider>,
    pub d: Drive,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

static N: AtomicU32 = AtomicU32::new(0);

pub fn rp(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

/// Skip (loudly) where Windows has no Cloud Files API, e.g. some CI images.
pub fn cloud_files_supported() -> bool {
    let ok = cloud_filter::root::is_supported().unwrap_or(false);
    if !ok {
        eprintln!("SKIPPED: the Cloud Files API is not available on this machine");
    }
    ok
}

pub async fn harness() -> H {
    harness_with(|_| {}).await
}

pub async fn harness_with(setup_cloud: impl FnOnce(&Path)) -> H {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
    let n = N.fetch_add(1, Ordering::Relaxed);
    let tag = format!("{}-{n}", std::process::id());
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("w{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    let cloud = base.join("cloud");
    let root = base.join("Synclone");
    let state = base.join("state");
    for p in [&cloud, &root, &state] {
        std::fs::create_dir_all(p).unwrap();
    }
    setup_cloud(&cloud);

    let conf = base.join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    let mut rc = DaemonConfig::new(synclone_rclone::locate::find_rclone().expect("rclone: run scripts/fetch-rclone"));
    rc.config_file = Some(conf);
    rc.log_file = Some(base.join("rclone.log"));
    let rclone = RcloneDaemon::start(rc).await.expect("rclone starts");
    let client = rclone.client();
    let mut p = Map::new();
    p.insert("remote".into(), Value::String(cloud.display().to_string()));
    client
        .config_create(
            "cloud",
            "alias",
            p,
            &ConfigOpts {
                non_interactive: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let db = Arc::new(Db::open(&state.join("state.db")).unwrap());
    let mut cfg = WindowsProviderConfig::new(&root);
    cfg.account = format!("test-{tag}");
    cfg.display_name = format!("Synclone test {tag}");
    cfg.list_wait = Duration::from_secs(15);
    let prov =
        Arc::new(WindowsProvider::start(cfg, db.clone(), rclone.endpoint().clone()).expect("sync root registers"));
    let settings = EngineSettings {
        debounce_ms: 300,
        open_retry_ms: 500,
        backoff_base_ms: 300,
        backoff_max_ms: 2_000,
        ..Default::default()
    };
    let eng = Engine::new(
        db,
        client as Arc<dyn RcloneApi>,
        prov.clone() as Arc<dyn synclone_core::Provider>,
        Arc::new(SystemClock),
        settings,
        state,
    );
    let d = eng.connect_remote("cloud", Some("Cloud")).await.unwrap();
    let mut tasks = Vec::new();
    let mut rx = prov.take_events().unwrap();
    let e = eng.clone();
    tasks.push(tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Err(err) = e.handle_event(ev.clone()) {
                eprintln!("event {ev:?}: {err:#}");
            }
        }
    }));
    let e = eng.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            match e.run_due().await {
                Ok(r) if r.executed > 0 => continue,
                Ok(_) => {}
                Err(err) => eprintln!("ops: {err:#}"),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }));
    H {
        base,
        cloud,
        root,
        rclone,
        eng,
        prov,
        d,
        tasks,
    }
}

impl Drop for H {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Err(e) = self.prov.unregister() {
            eprintln!("unregistering the test sync root: {e:#}");
        }
        // Inside target/tmp only.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

impl H {
    /// Path of `p` inside the drive folder.
    pub fn local(&self, p: &str) -> PathBuf {
        let mut x = self.root.join(&self.d.display_name);
        for s in p.split('/').filter(|s| !s.is_empty()) {
            x.push(s);
        }
        x
    }
    pub fn cloud_path(&self, p: &str) -> PathBuf {
        let mut x = self.cloud.clone();
        for s in p.split('/').filter(|s| !s.is_empty()) {
            x.push(s);
        }
        x
    }
    pub fn put_cloud(&self, p: &str, data: &[u8]) {
        let path = self.cloud_path(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
    }
    pub fn cloud_file(&self, p: &str) -> Option<Vec<u8>> {
        std::fs::read(self.cloud_path(p)).ok()
    }
    pub async fn poll(&self) {
        self.eng.poll_drive(self.d.id).await.unwrap();
    }
    pub fn item(&self, p: &str) -> Option<synclone_core::db::Item> {
        self.eng.db.item(self.d.id, &rp(p)).unwrap()
    }
    pub fn status(&self, p: &str) -> LocalStatus {
        self.prov.local_status(&self.d, &rp(p)).unwrap()
    }
    /// Every file in the fake cloud with its size and mtime.
    pub fn cloud_snapshot(&self) -> Vec<(String, u64, std::time::SystemTime)> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, u64, std::time::SystemTime)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let m = e.metadata().unwrap();
                let rel = e.path().strip_prefix(base).unwrap().display().to_string();
                if m.is_dir() {
                    out.push((rel + "/", 0, m.modified().unwrap()));
                    walk(&e.path(), base, out);
                } else {
                    out.push((rel, m.len(), m.modified().unwrap()));
                }
            }
        }
        let mut v = Vec::new();
        walk(&self.cloud, &self.cloud, &mut v);
        v.sort();
        v
    }
}

/// Run PowerShell in a separate process, as a user's program would act.
pub fn ps(script: &str) -> String {
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("powershell runs");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    assert!(
        out.status.success() && stderr.is_empty(),
        "powershell failed: {script}\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

/// A path quoted for PowerShell.
pub fn q(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

pub async fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !f() {
        if Instant::now() > until {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
