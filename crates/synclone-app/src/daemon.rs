//! The background service's lifecycle as seen from the app: use the one
//! already running, or start it next to the app's own executable; stop it
//! when the user quits.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::json;

use crate::Client;

/// How to start the service: `SYNCLONE_DAEMON` (a daemon executable) if
/// set, else this app itself with `--service`.
pub fn service_command() -> Result<std::process::Command> {
    if let Some(p) = std::env::var_os("SYNCLONE_DAEMON").filter(|p| !p.is_empty()) {
        return Ok(std::process::Command::new(PathBuf::from(p)));
    }
    let mut c = std::process::Command::new(std::env::current_exe()?);
    c.arg("--service");
    Ok(c)
}

pub async fn is_running(c: &Client) -> bool {
    c.call("status", json!({})).await.is_ok()
}

/// Make sure the daemon answers, starting it if needed (up to 30 s).
/// Returns whether this call started it.
pub async fn ensure_running(c: &Client) -> Result<bool> {
    let logs = synclone_daemon::Config::from_env().state_dir.join("logs");
    ensure_running_with(c, service_command()?, &logs).await
}

/// [`ensure_running`] with an explicit command and log folder (tests: the
/// log goes where the command's own state dir is, never the user's).
pub async fn ensure_running_with(c: &Client, mut cmd: std::process::Command, logs: &std::path::Path) -> Result<bool> {
    if is_running(c).await {
        return Ok(false);
    }
    std::fs::create_dir_all(logs)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("daemon.log"))?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window; keeps running if the app closes.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    let what = format!("{:?}", cmd.get_program());
    cmd.spawn()
        .with_context(|| format!("starting the Synclone service ({what})"))?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(30) {
        if is_running(c).await {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Err(anyhow!(
        "the Synclone service did not start; see {}",
        logs.join("daemon.log").display()
    ))
}

/// Ask the daemon to stop (Quit in the tray menu).
pub async fn stop(c: &Client) {
    let _ = c.call("shutdown", json!({})).await;
}
