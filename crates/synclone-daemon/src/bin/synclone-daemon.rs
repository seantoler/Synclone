//! The Synclone background service. Usually started by the tray app or by
//! `synclonectl daemon`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    synclone_daemon::init_logging();
    synclone_daemon::run_from_env().await
}
