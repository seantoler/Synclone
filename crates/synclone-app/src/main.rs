//! `Synclone`: the tray / menu bar app (PLAN.md §6).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "macos"))]
mod gui;

fn main() {
    // The app starts its background service as `Synclone --service`.
    if std::env::args().any(|a| a == "--service") {
        synclone_daemon::init_logging();
        let rt = tokio::runtime::Runtime::new().expect("starting the service runtime");
        if let Err(e) = rt.block_on(synclone_daemon::run_from_env()) {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(any(windows, target_os = "macos"))]
    gui::run();
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        eprintln!(
            "The {} app runs on Windows and macOS. Use synclonectl here.",
            synclone_core::APP_NAME
        );
        std::process::exit(2);
    }
}
