//! End-to-end through `synclonectl` and a real daemon + rclone (layer 2):
//! - Phase 1 exit: `drive list/add/remove` on local and memory backends;
//! - Phase 3 exit: every user-facing action of PLAN.md §1.1 via the CLI;
//! - Phase 3 exit: kill the daemon mid-transfer, restart, everything resumes.
//!
//! "Local backend" here is an rclone `alias` remote pointing at a temp
//! folder (a bare `local` remote would expose the whole file system).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Env {
    tmp: tempfile::TempDir,
    socket: String,
    daemon: Option<Child>,
    extra_env: Vec<(String, String)>,
}

fn rclone() -> PathBuf {
    synclone_rclone::locate::find_rclone().expect("rclone not found: run scripts/fetch-rclone")
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("sc")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        std::fs::write(tmp.path().join("rclone.conf"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        let socket = if cfg!(windows) {
            format!(
                r"\\.\pipe\synclone-test-{}",
                std::process::id() as u64 * 1000 + rand_suffix()
            )
        } else {
            tmp.path().join("d.sock").display().to_string()
        };
        Env {
            tmp,
            socket,
            daemon: None,
            extra_env: Vec::new(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.tmp.path().join(p)
    }
    fn data(&self) -> PathBuf {
        self.path("data")
    }
    fn root(&self) -> PathBuf {
        self.path("root")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_synclonectl"));
        c.env("SYNCLONE_STATE_DIR", self.path("state"))
            .env("SYNCLONE_ROOT", self.root())
            .env("SYNCLONE_SOCKET", &self.socket)
            // Data safety: never the user's real rclone config.
            .env("SYNCLONE_RCLONE_CONFIG", self.path("rclone.conf"))
            .env("RCLONE_CONFIG", self.path("rclone.conf"))
            .env("SYNCLONE_RCLONE", rclone())
            .env("SYNCLONE_DEBOUNCE_MS", "200")
            .env("SYNCLONE_SCAN_SECS", "1")
            .env("SYNCLONE_NO_BROWSER", "1");
        for (k, v) in &self.extra_env {
            c.env(k, v);
        }
        c
    }

    fn start(&mut self) {
        let child = self
            .cmd()
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(self.path("daemon.log")).unwrap()))
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(30) {
            if self.try_ctl(&["status"]).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "daemon did not start: {}",
            std::fs::read_to_string(self.path("daemon.log")).unwrap_or_default()
        );
    }

    fn try_ctl(&self, args: &[&str]) -> Result<Value, String> {
        let out = self.cmd().arg("--json").args(args).output().unwrap();
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).to_string());
        }
        let s = String::from_utf8_lossy(&out.stdout);
        Ok(serde_json::from_str(&s).unwrap_or(Value::String(s.to_string())))
    }

    fn ctl(&self, args: &[&str]) -> Value {
        self.try_ctl(args)
            .unwrap_or_else(|e| panic!("synclonectl {args:?} failed: {e}"))
    }

    fn stop(&mut self) {
        let _ = self.try_ctl(&["shutdown"]);
        if let Some(mut c) = self.daemon.take() {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(15) {
                if let Ok(Some(_)) = c.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    #[cfg(unix)]
    fn kill(&mut self) {
        if let Some(mut c) = self.daemon.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    fn add_local_drive(&self, name: &str) {
        let remote = format!("remote={}", self.data().display());
        let v = self.ctl(&["drive", "add", "alias", "--name", name, "--param", &remote]);
        assert_eq!(v["name"], name);
    }

    fn wait_for(&self, what: &str, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(60) {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "timed out waiting for {what}\nops: {:?}\nactivity: {:?}\nlocal: {:?}\ndata: {:?}\nlog:\n{}",
            self.try_ctl(&["ops"]),
            self.try_ctl(&["activity", "--limit", "30"]),
            list_dir(&self.root()),
            list_dir(&self.data()),
            std::fs::read_to_string(self.path("daemon.log")).unwrap_or_default()
        );
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop();
    }
}

fn list_dir(p: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(format!("{} ({len})", path.strip_prefix(p).unwrap_or(&path).display()));
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
        % 1000
}

fn write(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn version_flag() {
    let out = Command::new(env!("CARGO_BIN_EXE_synclonectl"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("synclonectl"));
}

#[test]
fn drive_list_add_remove_on_local_and_memory_backends() {
    let mut e = Env::new();
    e.start();
    assert_eq!(e.ctl(&["drive", "list"]), Value::Array(vec![]));
    let v = e.ctl(&["drive", "add", "memory", "--name", "Scratch"]);
    assert_eq!(v["name"], "Scratch");
    e.add_local_drive("Local Files");
    let list = e.ctl(&["drive", "list"]);
    assert_eq!(names(&list), vec!["Scratch", "Local Files"]);
    assert!(e.root().join("Scratch").is_dir());
    assert!(e.root().join("Local Files").is_dir());
    // Remotes are in the (test) rclone config, usable by the rclone CLI too.
    let conf = std::fs::read_to_string(e.path("rclone.conf")).unwrap();
    assert!(conf.contains("[scratch]") && conf.contains("[local-files]"), "{conf}");
    // Refuses options that would delete permanently.
    assert!(e
        .try_ctl(&[
            "drive",
            "add",
            "drime",
            "--param",
            "access_token=x",
            "--param",
            "hard_delete=true"
        ])
        .is_err());

    // Disconnect: folder gone, sign-in kept, data untouched.
    write(&e.data().join("keep.txt"), b"precious");
    e.ctl(&["drive", "remove", "Local Files"]);
    assert!(!e.root().join("Local Files").exists());
    assert_eq!(std::fs::read(e.data().join("keep.txt")).unwrap(), b"precious");
    let list = e.ctl(&["drive", "list"]);
    assert_eq!(list[1]["state"], "disconnected");
    assert!(std::fs::read_to_string(e.path("rclone.conf"))
        .unwrap()
        .contains("[local-files]"));
    // Reconnect in one step.
    e.ctl(&["drive", "connect", "local-files"]);
    assert_eq!(e.ctl(&["drive", "list"])[1]["state"], "connected");
    // Forget + sign out removes the rclone remote (still no data deleted).
    e.ctl(&["drive", "remove", "Scratch", "--sign-out"]);
    assert_eq!(names(&e.ctl(&["drive", "list"])), vec!["Local Files"]);
    assert!(!std::fs::read_to_string(e.path("rclone.conf"))
        .unwrap()
        .contains("[scratch]"));
    assert_eq!(std::fs::read(e.data().join("keep.txt")).unwrap(), b"precious");
}

#[test]
fn existing_rclone_remotes_appear_as_drives() {
    let e0 = Env::new();
    std::fs::write(
        e0.path("rclone.conf"),
        format!("[myremote]\ntype = alias\nremote = {}\n", e0.data().display()),
    )
    .unwrap();
    let mut e = e0;
    e.start();
    assert_eq!(names(&e.ctl(&["drive", "list"])), vec!["myremote"]);
    assert_eq!(e.ctl(&["drive", "list"])[0]["imported"], true);
}

#[test]
fn every_user_action_through_the_cli() {
    let mut e = Env::new();
    e.start();
    e.add_local_drive("Cloud");
    let d = e.data();
    let local = e.root().join("Cloud");
    write(&d.join("docs/report.txt"), b"v1");
    write(&d.join("music/album/track1.mp3"), b"aaaa");
    write(&d.join("music/album/track2.mp3"), b"bbbb");
    e.ctl(&["sync"]);

    // Placeholders: visible, no content.
    let ls = e.ctl(&["ls", "Cloud/docs"]);
    assert_eq!(ls[0]["name"], "report.txt");
    assert_eq!(ls[0]["state"], "placeholder");
    assert!(local.join("docs/report.txt.sclstub").exists());
    assert!(!local.join("docs/report.txt").exists());

    // Open to download; downloads stay.
    e.ctl(&["open", "Cloud/docs/report.txt"]);
    assert_eq!(std::fs::read(local.join("docs/report.txt")).unwrap(), b"v1");
    e.ctl(&["sync"]);
    assert!(local.join("docs/report.txt").exists());

    // Save a folder to the device (recursive), then remove it from the device.
    e.ctl(&["download", "Cloud/music"]);
    assert_eq!(std::fs::read(local.join("music/album/track2.mp3")).unwrap(), b"bbbb");
    e.ctl(&["free", "Cloud/music"]);
    assert!(!local.join("music/album/track1.mp3").exists());
    assert!(local.join("music/album/track1.mp3.sclstub").exists());
    assert_eq!(
        std::fs::read(d.join("music/album/track1.mp3")).unwrap(),
        b"aaaa",
        "cloud copy untouched"
    );

    // Auto upload: new, edited and moved files.
    write(&local.join("new/notes.txt"), b"hello");
    std::fs::write(local.join("docs/report.txt"), b"v2 edited locally").unwrap();
    e.wait_for("uploads", || {
        std::fs::read(d.join("new/notes.txt")).ok().as_deref() == Some(b"hello".as_slice())
            && std::fs::read(d.join("docs/report.txt")).ok().as_deref() == Some(b"v2 edited locally".as_slice())
    });

    // Auto download of a remote change to a downloaded file.
    std::thread::sleep(Duration::from_millis(1100));
    write(&d.join("docs/report.txt"), b"v3 from elsewhere");
    e.ctl(&["sync"]);
    assert_eq!(
        std::fs::read(local.join("docs/report.txt")).unwrap(),
        b"v3 from elsewhere"
    );

    // Conflict: both sides change.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(local.join("docs/report.txt"), b"local v4").unwrap();
    write(&d.join("docs/report.txt"), b"remote v4!");
    e.ctl(&["sync"]);
    let c = e.ctl(&["conflicts"]);
    assert_eq!(c.as_array().unwrap().len(), 1, "{c}");
    assert_eq!(std::fs::read(local.join("docs/report.txt")).unwrap(), b"remote v4!");
    let copy = c[0]["conflict_copy_path"].as_str().unwrap();
    assert_eq!(std::fs::read(d.join(copy)).unwrap(), b"local v4");
    e.ctl(&["conflicts", "--resolve", &c[0]["id"].to_string()]);
    assert!(e.ctl(&["conflicts"]).as_array().unwrap().is_empty());

    // Queued delete: local delete does not touch the cloud.
    std::fs::remove_file(local.join("new/notes.txt")).unwrap();
    e.ctl(&["sync"]);
    let t = e.ctl(&["trash", "list"]);
    assert_eq!(t.as_array().unwrap().len(), 1);
    assert!(d.join("new/notes.txt").exists());
    // Restore.
    e.ctl(&["trash", "restore", &t[0]["id"].to_string()]);
    assert!(local.join("new/notes.txt.sclstub").exists());
    // Delete again and empty the trash (needs confirmation).
    std::fs::remove_file(local.join("new/notes.txt.sclstub")).unwrap();
    e.ctl(&["sync"]);
    assert!(e.try_ctl(&["trash", "empty"]).is_err(), "needs --yes");
    assert!(d.join("new/notes.txt").exists());
    e.ctl(&["trash", "empty", "--yes"]);
    assert!(!d.join("new/notes.txt").exists(), "deleted from the cloud only now");

    // Pause / resume.
    e.ctl(&["pause"]);
    assert_eq!(e.ctl(&["drive", "list"])[0]["paused"], true);
    e.ctl(&["resume"]);

    // Status and activity.
    let s = e.ctl(&["status"]);
    assert_eq!(s["app"], "Synclone");
    assert!(!e.ctl(&["activity"]).as_array().unwrap().is_empty());

    // Unmount, not delete.
    e.ctl(&["drive", "remove", "Cloud"]);
    assert!(!local.exists());
    assert_eq!(std::fs::read(d.join("music/album/track2.mp3")).unwrap(), b"bbbb");
    assert_eq!(std::fs::read(d.join("docs/report.txt")).unwrap(), b"remote v4!");
}

#[cfg(unix)]
#[test]
fn kill_and_restart_resumes_in_flight_operations() {
    let mut e = Env::new();
    // Slow transfers so the kill lands mid-upload.
    // --local-no-clone: on macOS rclone copies local to local with APFS
    // clonefile, which is instant and ignores the bandwidth limit.
    e.extra_env
        .push(("SYNCLONE_RCLONE_ARGS".into(), "--bwlimit 512k --local-no-clone".into()));
    e.start();
    e.add_local_drive("Cloud");
    e.ctl(&["sync"]);
    let local = e.root().join("Cloud");
    let mut files = Vec::new();
    for i in 0..3 {
        let data: Vec<u8> = (0..2_000_000u32)
            .map(|x| (x.wrapping_mul(31).wrapping_add(i)) as u8)
            .collect();
        write(&local.join(format!("big{i}.bin")), &data);
        files.push(data);
    }
    // Wait until an upload is running.
    e.wait_for("an upload in flight", || {
        e.try_ctl(&["ops"])
            .map(|v| {
                v.as_array()
                    .map(|a| a.iter().any(|o| o["state"] == "running"))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    let pid_file_before = rclone_pids();
    assert!(
        pid_file_before.iter().any(|p| is_our_rclone(*p, &e)),
        "found our rclone process"
    );
    let complete_before = (0..3)
        .filter(|i| std::fs::read(e.data().join(format!("big{i}.bin"))).ok().as_ref() == Some(&files[*i]))
        .count();
    assert!(complete_before < 3, "the kill must land before all uploads finished");
    e.kill();
    // rclone dies with the daemon (no orphan holding the port).
    std::thread::sleep(Duration::from_millis(500));
    let orphans: Vec<_> = rclone_pids()
        .into_iter()
        .filter(|p| pid_file_before.contains(p))
        .filter(|p| is_our_rclone(*p, &e))
        .collect();
    assert!(orphans.is_empty(), "rclone left running: {orphans:?}");

    e.extra_env.clear();
    e.start();
    e.wait_for("all uploads after restart", || {
        (0..3).all(|i| std::fs::read(e.data().join(format!("big{i}.bin"))).ok().as_ref() == Some(&files[i]))
    });
    let ops = e.ctl(&["ops"]);
    e.wait_for("journal drained", || {
        e.ctl(&["ops"]).as_array().map(|a| a.is_empty()).unwrap_or(false)
    });
    let _ = ops;
}

#[cfg(unix)]
fn rclone_pids() -> Vec<u32> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
                if let Ok(cmd) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) {
                    if cmd.contains("rcd") && cmd.contains("rclone") {
                        v.push(pid);
                    }
                }
            }
        }
    }
    v
}

#[cfg(unix)]
fn is_our_rclone(pid: u32, e: &Env) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|c| c.contains(&e.path("rclone.conf").display().to_string()))
        .unwrap_or(false)
}
