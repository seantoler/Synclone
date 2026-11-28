//! Layer 5 (PLAN.md §7): the real app's windows driven through WebDriver
//! (`tauri-driver` + `msedgedriver`), against a real daemon and rclone.
//!
//! Runs with SYNCLONE_WEBDRIVER=1 and the drivers in place:
//!   cargo install --locked --root target/tools tauri-driver@2
//!   msedgedriver matching the installed Edge in target/tools/edgedriver/
//! (paths overridable with TAURI_DRIVER / MSEDGEDRIVER). See
//! scripts/webdriver-setup.ps1.
#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use fantoccini::{Client as Wd, ClientBuilder, Locator};
use serde_json::json;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tool(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo().join(default))
}

/// tauri-driver, killed when dropped.
struct Driver(Child);
impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn enabled() -> bool {
    if std::env::var("SYNCLONE_WEBDRIVER").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set SYNCLONE_WEBDRIVER=1 to run the WebDriver tests");
        return false;
    }
    true
}

fn start_driver(d: &D) -> Driver {
    let td = tool("TAURI_DRIVER", "target/tools/bin/tauri-driver.exe");
    let ed = tool("MSEDGEDRIVER", "target/tools/edgedriver/msedgedriver.exe");
    assert!(td.exists(), "tauri-driver missing at {}", td.display());
    assert!(ed.exists(), "msedgedriver missing at {}", ed.display());
    let child = Command::new(td)
        .arg("--native-driver")
        .arg(ed)
        // The app finds this test's daemon, and never starts one of its own.
        .env("SYNCLONE_SOCKET", &d.config.socket)
        .env("SYNCLONE_STATE_DIR", &d.config.state_dir)
        .env("SYNCLONE_DAEMON", d.tmp.path().join("no-such-daemon.exe"))
        .env("SYNCLONE_NO_OPEN", "1")
        .env("SYNCLONE_NO_BROWSER", "1")
        // Offers `alias`, which the test uses as its local cloud.
        .env("SYNCLONE_ADVANCED", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("tauri-driver starts");
    let t = Instant::now();
    while std::net::TcpStream::connect("127.0.0.1:4444").is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "tauri-driver listens");
        std::thread::sleep(Duration::from_millis(100));
    }
    Driver(child)
}

async fn session(page: &str) -> Wd {
    let app = PathBuf::from(env!("CARGO_BIN_EXE_Synclone"));
    let mut caps = serde_json::Map::new();
    caps.insert("browserName".into(), json!("wry"));
    caps.insert(
        "tauri:options".into(),
        // WebDriver turns each argument into a `--switch`.
        json!({ "application": app, "args": [format!("open={page}")] }),
    );
    ClientBuilder::native()
        .capabilities(caps)
        .connect("http://127.0.0.1:4444")
        .await
        .expect("WebDriver session")
}

async fn visible(c: &Wd, css: &str) -> bool {
    match c.find(Locator::Css(css)).await {
        Ok(e) => e.is_displayed().await.unwrap_or(false),
        Err(_) => false,
    }
}

async fn wait_visible(c: &Wd, css: &str, secs: u64) {
    let t = Instant::now();
    while !visible(c, css).await {
        if t.elapsed() > Duration::from_secs(secs) {
            let err = match c.find(Locator::Css(".error:not(.hidden)")).await {
                Ok(e) => e.text().await.unwrap_or_default(),
                Err(_) => String::new(),
            };
            panic!("{css} did not appear; error shown: {err:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// For a person to look at (SYNCLONE_WEBDRIVER_SHOTS=<dir>); not asserted.
async fn shot(c: &Wd, name: &str) {
    if let Some(dir) = std::env::var_os("SYNCLONE_WEBDRIVER_SHOTS") {
        if let Ok(png) = c.screenshot().await {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(Path::new(&dir).join(format!("{name}.png")), png);
        }
    }
}

async fn click(c: &Wd, css: &str) {
    wait_visible(c, css, 15).await;
    c.find(Locator::Css(css)).await.unwrap().click().await.unwrap();
}

async fn type_into(c: &Wd, css: &str, text: &str) {
    wait_visible(c, css, 15).await;
    let e = c.find(Locator::Css(css)).await.unwrap();
    e.clear().await.unwrap();
    e.send_keys(text).await.unwrap();
    let got = e.prop("value").await.unwrap().unwrap_or_default();
    assert_eq!(got, text, "typing into {css}");
}

/// Both windows in one test: tauri-driver serves one port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wizard_and_trash_windows_through_webdriver() {
    if !enabled() {
        return;
    }
    let d = daemon().await;
    let _driver = start_driver(&d);

    // ---- Wizard: add a "local" cloud (an rclone alias onto a folder).
    std::fs::write(d.cloud().join("hello.txt"), b"hello").unwrap();
    std::fs::write(d.cloud().join("old.txt"), b"old").unwrap();
    let c = session("wizard").await;
    wait_visible(&c, "#other-tile", 15).await;
    shot(&c, "1-wizard-pick").await;
    click(&c, "#other-tile").await;
    type_into(&c, "#search", "alias").await;
    shot(&c, "2-wizard-search").await;
    click(&c, "button[data-type='alias']").await;
    wait_visible(&c, "#opt-remote", 15).await;
    shot(&c, "3-wizard-form").await;
    type_into(&c, "#opt-remote", &d.cloud().display().to_string()).await;
    click(&c, "#connect").await;
    wait_visible(&c, "#step-name", 30).await;
    type_into(&c, "#name", "Test Local").await;
    shot(&c, "4-wizard-name").await;
    click(&c, "#finish").await;
    wait_visible(&c, "#step-done", 20).await;
    shot(&c, "5-wizard-done").await;
    let done = c.find(Locator::Css("#done-text")).await.unwrap().text().await.unwrap();
    assert!(done.contains("Test Local"), "{done}");
    c.close().await.unwrap();

    let drives = d.client.ui_call("drive.list", json!({})).await.unwrap();
    let dr = drives
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "Test Local")
        .expect("the wizard added the drive")
        .clone();
    let folder = PathBuf::from(dr["folder"].as_str().unwrap());
    assert!(folder.is_dir());
    d.client.ui_call("sync", json!({})).await.unwrap();
    let stub = folder.join("old.txt.sclstub");
    assert!(stub.exists());

    // ---- Trash: restore, then empty.
    std::fs::remove_file(&stub).unwrap();
    wait_for("trash entry", 20, || async {
        d.client
            .ui_call("trash.list", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == 1
    })
    .await;
    let c = session("trash").await;
    click(&c, "#rows input[type=checkbox]").await;
    click(&c, "#restore").await;
    wait_for("restored", 20, || async { stub.exists() }).await;
    assert!(d.cloud().join("old.txt").exists());

    std::fs::remove_file(&stub).unwrap();
    wait_for("trash entry again", 20, || async {
        d.client
            .ui_call("trash.list", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == 1
    })
    .await;
    // The window refreshes itself every few seconds.
    wait_visible(&c, "#rows tr", 15).await;
    click(&c, "#empty-trash").await;
    wait_visible(&c, "#confirm", 5).await;
    shot(&c, "6-trash-confirm").await;
    let confirm = c
        .find(Locator::Css("#confirm-text"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(confirm.contains("1 item"), "{confirm}");
    click(&c, "#confirm-yes").await;
    wait_for("deleted from the cloud", 20, || async {
        !d.cloud().join("old.txt").exists()
    })
    .await;
    assert!(d.cloud().join("hello.txt").exists());
    wait_visible(&c, "#empty", 15).await;
    c.close().await.unwrap();
}
