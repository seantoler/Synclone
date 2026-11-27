//! A real daemon (in this process) with real rclone, the stub provider and
//! temp folders, plus an `alias` "cloud" folder for drives. The user's real
//! rclone config is never read.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use synclone_app::Client;
use synclone_core::EngineSettings;
use synclone_daemon::{Config, Daemon, ProviderKind};

pub struct D {
    pub tmp: tempfile::TempDir,
    pub daemon: Arc<Daemon>,
    pub client: Client,
    pub config: Config,
}

impl D {
    pub fn cloud(&self) -> PathBuf {
        self.tmp.path().join("cloud")
    }
    pub fn root(&self) -> PathBuf {
        self.tmp.path().join("root")
    }
}

impl Drop for D {
    fn drop(&mut self) {
        self.daemon.shutdown();
    }
}

pub async fn daemon() -> D {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("cloud")).unwrap();
    let conf = tmp.path().join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    let socket = if cfg!(windows) {
        format!(r"\\.\pipe\synclone-app-test-{}-{n}", std::process::id())
    } else {
        tmp.path().join("d.sock").display().to_string()
    };
    let config = Config {
        state_dir: tmp.path().join("state"),
        sync_root: tmp.path().join("root"),
        socket: socket.clone(),
        rclone_config: Some(conf),
        rclone: Some(synclone_rclone::locate::find_rclone().expect("rclone: run scripts/fetch-rclone")),
        rclone_args: Vec::new(),
        scan_interval: Duration::from_millis(500),
        provider: ProviderKind::Stub,
        settings: EngineSettings {
            debounce_ms: 200,
            ..Default::default()
        },
    };
    let daemon = Daemon::start(config.clone()).await.expect("daemon starts");
    tokio::spawn(daemon.clone().run());
    let client = Client::new(socket);
    let t = Instant::now();
    while !synclone_app::daemon::is_running(&client).await {
        assert!(t.elapsed() < Duration::from_secs(20), "daemon answers");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    D {
        tmp,
        daemon,
        client,
        config,
    }
}

pub async fn wait_for<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t = Instant::now();
    while !f().await {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
