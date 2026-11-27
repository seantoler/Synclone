//! Finding the rclone binary.

use std::path::{Path, PathBuf};

/// The rclone version Synclone is built and tested against.
pub const PINNED_VERSION: &str = "v1.76.0";
/// The oldest rclone that works (Drime support arrived in 1.73).
pub const MIN_VERSION: (u32, u32) = (1, 73);

pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "rclone.exe"
    } else {
        "rclone"
    }
}

/// `<os>-<arch>` as used by rclone's release file names and `vendor/rclone/`.
pub fn platform_dir() -> String {
    let os = match std::env::consts::OS {
        "macos" => "osx",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Where to find rclone, in order:
/// 1. `SYNCLONE_RCLONE` (an explicit path),
/// 2. next to the running executable (how installers bundle it),
/// 3. `vendor/rclone/<platform>/` above the current directory or the
///    executable (development checkouts, after `scripts/fetch-rclone`).
///
/// rclone on `PATH` is deliberately not used: the app ships a pinned,
/// checksum-verified build.
pub fn find_rclone() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYNCLONE_RCLONE") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let exe = std::env::current_exe().ok();
    if let Some(dir) = exe.as_deref().and_then(Path::parent) {
        let p = dir.join(exe_name());
        if p.is_file() {
            return Some(p);
        }
    }
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Some(dir) = exe.as_deref().and_then(Path::parent) {
        starts.push(dir.to_path_buf());
    }
    starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    for start in starts {
        for dir in start.ancestors() {
            let p = dir.join("vendor").join("rclone").join(platform_dir()).join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}
