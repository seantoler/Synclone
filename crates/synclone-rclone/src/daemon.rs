//! Spawning and supervising `rclone rcd`.
//!
//! Design adapted from OpenCfDrive's `RcloneDaemon` (MIT, see
//! THIRD-PARTY-NOTICES.md): listen on a free 127.0.0.1 port with a random
//! per-launch login passed through the environment (so it never shows in a
//! process list), strip any inherited `RCLONE_RC_NO_AUTH`, wait for
//! `core/version` to answer, and restart on the same port and login with
//! growing delays when rclone exits on its own.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rand::distributions::Alphanumeric;
use rand::Rng;
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex};

use crate::api::RcloneApi;
use crate::client::{Endpoint, RcClient};
use crate::error::{ErrorKind, RcloneError, Result};
use crate::locate::MIN_VERSION;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const RESTART_DELAYS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub rclone: PathBuf,
    /// rclone config file. `None` means rclone's default; Synclone uses the
    /// user's existing config in place (PLAN.md §2.5). Tests always set it.
    pub config_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    /// Extra flags, e.g. `--bwlimit`.
    pub extra_args: Vec<String>,
    /// Restart rclone when it exits on its own.
    pub restart: bool,
}

impl DaemonConfig {
    pub fn new(rclone: impl Into<PathBuf>) -> Self {
        Self {
            rclone: rclone.into(),
            config_file: None,
            log_file: None,
            extra_args: Vec::new(),
            restart: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Starting,
    Running { pid: u32, version: String },
    Restarting { reason: String },
    Stopped,
}

/// A supervised `rclone rcd`. Dropping it (or calling `shutdown`) stops rclone.
pub struct RcloneDaemon {
    cfg: DaemonConfig,
    endpoint: Endpoint,
    client: Arc<RcClient>,
    child: Arc<Mutex<Option<Child>>>,
    state_tx: watch::Sender<DaemonState>,
    stop_tx: watch::Sender<bool>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn free_port() -> std::io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn random_secret(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

impl RcloneDaemon {
    /// Start rclone and wait until it answers.
    pub async fn start(cfg: DaemonConfig) -> Result<Arc<Self>> {
        let port = free_port().map_err(|e| RcloneError::new(ErrorKind::Other, "rcd", e.to_string()))?;
        let endpoint = Endpoint {
            url: format!("http://127.0.0.1:{port}"),
            user: format!("synclone-{}", random_secret(8)),
            pass: random_secret(32),
        };
        let client = Arc::new(RcClient::new(endpoint.clone()));
        let (state_tx, _) = watch::channel(DaemonState::Starting);
        let (stop_tx, _) = watch::channel(false);
        let daemon = Arc::new(Self {
            cfg,
            endpoint,
            client,
            child: Arc::new(Mutex::new(None)),
            state_tx,
            stop_tx,
            supervisor: Mutex::new(None),
        });
        daemon.launch().await?;
        let weak = Arc::downgrade(&daemon);
        let handle = tokio::spawn(async move { supervise(weak).await });
        *daemon.supervisor.lock().await = Some(handle);
        Ok(daemon)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The rc client for this daemon.
    pub fn client(&self) -> Arc<RcClient> {
        self.client.clone()
    }

    pub fn state(&self) -> DaemonState {
        self.state_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<DaemonState> {
        self.state_tx.subscribe()
    }

    pub async fn pid(&self) -> Option<u32> {
        self.child.lock().await.as_ref().and_then(|c| c.id())
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.cfg.rclone);
        let addr = self.endpoint.url.trim_start_matches("http://").to_string();
        cmd.arg("rcd")
            .arg("--rc-addr")
            .arg(addr)
            // Expose objects over HTTP with Range support (verified on v1.76.0).
            .arg("--rc-serve")
            // No one can type a config password: fail instead of waiting.
            .arg("--ask-password=false")
            .arg("--log-level")
            .arg("INFO");
        if let Some(cfg) = &self.cfg.config_file {
            cmd.arg("--config").arg(cfg);
        }
        if let Some(log) = &self.cfg.log_file {
            if let Some(parent) = log.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            cmd.arg("--log-file").arg(log);
        }
        cmd.args(&self.cfg.extra_args);
        // rclone reads RCLONE_<FLAG> variables as flags: the login goes in
        // this way so it never appears on the command line.
        cmd.env("RCLONE_RC_USER", &self.endpoint.user)
            .env("RCLONE_RC_PASS", &self.endpoint.pass)
            .env_remove("RCLONE_RC_NO_AUTH")
            .env_remove("RCLONE_RC_ADDR")
            .env_remove("RCLONE_RC_HTPASSWD");
        if self.cfg.config_file.is_some() {
            cmd.env_remove("RCLONE_CONFIG");
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        cmd.kill_on_drop(true);
        #[cfg(target_os = "linux")]
        unsafe {
            // Die with the daemon even if it is killed with SIGKILL.
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        cmd
    }

    async fn launch(&self) -> Result<()> {
        self.state_tx.send_replace(DaemonState::Starting);
        let mut child = self.command().spawn().map_err(|e| {
            RcloneError::new(
                ErrorKind::Other,
                "rcd",
                format!("cannot start {}: {e}", self.cfg.rclone.display()),
            )
        })?;
        let pid = child.id().unwrap_or(0);
        #[cfg(windows)]
        if let Some(h) = child.raw_handle() {
            // Die with the daemon even if it is killed (D-004).
            if let Err(e) = kill_on_close_job::assign(h) {
                tracing::warn!("could not tie rclone to Synclone's lifetime: {e}");
            }
        }
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(RcloneError::new(
                    ErrorKind::Other,
                    "rcd",
                    format!("rclone exited right away ({status}); see its log"),
                ));
            }
            match self.client.version().await {
                Ok(v) => {
                    if !v.at_least(MIN_VERSION.0, MIN_VERSION.1) {
                        let _ = child.kill().await;
                        return Err(RcloneError::new(
                            ErrorKind::Other,
                            "rcd",
                            format!(
                                "rclone {} is too old; {}.{} or later is required",
                                v.version, MIN_VERSION.0, MIN_VERSION.1
                            ),
                        ));
                    }
                    self.state_tx.send_replace(DaemonState::Running {
                        pid,
                        version: v.version,
                    });
                    *self.child.lock().await = Some(child);
                    return Ok(());
                }
                Err(e) if e.kind == ErrorKind::Network => {}
                Err(e) => {
                    let _ = child.kill().await;
                    return Err(e);
                }
            }
            if tokio::time::Instant::now() > deadline {
                let _ = child.kill().await;
                return Err(RcloneError::new(
                    ErrorKind::Other,
                    "rcd",
                    "rclone did not answer within 30 s",
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop rclone: ask it to quit, then kill it if it lingers.
    pub async fn shutdown(&self) {
        self.stop_tx.send_replace(true);
        if let Some(h) = self.supervisor.lock().await.take() {
            h.abort();
        }
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.client.quit()).await;
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        self.state_tx.send_replace(DaemonState::Stopped);
    }

    /// Kill rclone without telling the supervisor (for crash tests).
    pub async fn kill_for_test(&self) {
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = child.kill().await;
        }
    }

    /// Wait until rclone is running (e.g. after a restart).
    pub async fn wait_running(&self, timeout: Duration) -> bool {
        let mut rx = self.subscribe();
        let fut = async {
            loop {
                if matches!(*rx.borrow(), DaemonState::Running { .. }) {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        };
        tokio::time::timeout(timeout, fut).await.unwrap_or(false)
    }

    pub fn config_file(&self) -> Option<&Path> {
        self.cfg.config_file.as_deref()
    }
}

async fn supervise(weak: std::sync::Weak<RcloneDaemon>) {
    let mut failures = 0usize;
    loop {
        let Some(d) = weak.upgrade() else { return };
        let mut stop_rx = d.stop_tx.subscribe();
        if *stop_rx.borrow() {
            return;
        }
        // Wait for the child to exit (poll so we never hold the lock across a wait).
        let exited = loop {
            {
                let mut guard = d.child.lock().await;
                match guard.as_mut() {
                    Some(c) => {
                        if let Ok(Some(status)) = c.try_wait() {
                            guard.take();
                            break Some(status);
                        }
                    }
                    None => break None,
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                _ = stop_rx.changed() => return,
            }
        };
        if *stop_rx.borrow() || !d.cfg.restart {
            d.state_tx.send_replace(DaemonState::Stopped);
            return;
        }
        let reason = match exited {
            Some(s) => format!("rclone stopped ({s})"),
            None => "rclone is not running".to_string(),
        };
        let delay = RESTART_DELAYS[failures.min(RESTART_DELAYS.len() - 1)];
        tracing::warn!("{reason}; starting it again in {delay:?}");
        d.state_tx.send_replace(DaemonState::Restarting { reason });
        drop(d);
        tokio::time::sleep(delay).await;
        let Some(d) = weak.upgrade() else { return };
        if *d.stop_tx.borrow() {
            return;
        }
        match d.launch().await {
            Ok(()) => failures = 0,
            Err(e) => {
                tracing::warn!("restarting rclone failed: {e}");
                failures += 1;
            }
        }
    }
}

impl Drop for RcloneDaemon {
    fn drop(&mut self) {
        // kill_on_drop on the child handles the process itself.
        self.stop_tx.send_replace(true);
    }
}

/// Windows: a job object, created once per process, that kills every
/// process in it when its last handle closes, i.e. when this process exits
/// for any reason (the Windows counterpart of `PR_SET_PDEATHSIG`, D-004).
#[cfg(windows)]
mod kill_on_close_job {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct Job(HANDLE);
    // A job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    static JOB: OnceLock<Result<Job, String>> = OnceLock::new();

    fn job() -> Result<&'static Job, String> {
        JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(None, None).map_err(|e| e.to_string())?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| e.to_string())?;
            // The handle is deliberately never closed: closing it is what kills rclone.
            Ok(Job(h))
        })
        .as_ref()
        .map_err(|e| e.clone())
    }

    pub fn assign(process: std::os::windows::io::RawHandle) -> Result<(), String> {
        let j = job()?;
        unsafe { AssignProcessToJobObject(j.0, HANDLE(process)) }.map_err(|e| e.to_string())
    }
}
