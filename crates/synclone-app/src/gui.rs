//! Tray menu, windows and the commands they call. Everything that talks to
//! the daemon goes through `synclone_app::Client`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use synclone_app::{daemon, Client};
use synclone_core::APP_NAME;
use tauri::image::Image;
use tauri::menu::{Menu, MenuBuilder, MenuItem, SubmenuBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt;

struct AppState {
    client: Client,
    quitting: AtomicBool,
    /// The last menu shown, to rebuild it only when something changed.
    menu_key: std::sync::Mutex<String>,
}

const TRAY: &str = "main";

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .try_init();
    let open_at_start = page_arg(std::env::args());
    let state = Arc::new(AppState {
        client: Client::from_env(),
        quitting: AtomicBool::new(false),
        menu_key: Default::default(),
    });
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // A second start (e.g. from the Start menu) shows a window instead.
            let page = page_arg(args.into_iter()).unwrap_or_else(|| "settings".into());
            open_page(app, &page, None);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            rpc,
            add_drive,
            open_path,
            reveal_path,
            open_url,
            autostart_get,
            autostart_set,
            open_window,
            close_self,
            advanced_mode
        ])
        .setup(move |app| {
            let menu = MenuBuilder::new(app)
                .item(&MenuItem::with_id(
                    app,
                    "status",
                    format!("{APP_NAME} — starting…"),
                    false,
                    None::<&str>,
                )?)
                .separator()
                .text("quit", "Quit")
                .build()?;
            TrayIconBuilder::with_id(TRAY)
                .icon(Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
                .tooltip(APP_NAME)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, ev| on_menu(app, ev.id().as_ref()))
                .build(app)?;
            let handle = app.handle().clone();
            let st = state.clone();
            tauri::async_runtime::spawn(async move {
                let started = daemon::ensure_running(&st.client).await;
                if let Err(e) = &started {
                    tracing::error!("{e:#}");
                }
                let first_run = st
                    .client
                    .call("drive.list", json!({}))
                    .await
                    .map(|v| v.as_array().map(|a| a.is_empty()).unwrap_or(false))
                    .unwrap_or(false);
                match open_at_start {
                    Some(p) => open_page(&handle, &p, None),
                    None if first_run => open_page(&handle, "wizard", None),
                    None => {}
                }
                loop {
                    refresh_menu(&handle, &st).await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("building the app");
    app.run(move |app, ev| {
        if let RunEvent::ExitRequested { api, code, .. } = ev {
            // Closing the last window keeps the tray; only Quit exits.
            let st = app.state::<Arc<AppState>>();
            if code.is_none() && !st.quitting.load(Ordering::SeqCst) {
                api.prevent_exit();
            }
        }
    });
}

/// `--open=<page>` or `--open <page>` (tests, a second start). WebDriver
/// passes arguments as switches, so `--open --wizard` works too.
fn page_arg(args: impl Iterator<Item = String>) -> Option<String> {
    let args: Vec<String> = args.collect();
    for (i, a) in args.iter().enumerate() {
        if let Some(p) = a.strip_prefix("--open=") {
            return Some(p.to_string());
        }
        if a == "--open" {
            return args.get(i + 1).map(|p| p.trim_start_matches('-').to_string());
        }
    }
    None
}

/// Show one window per page, reusing it if it is open.
fn open_page(app: &AppHandle, page: &str, query: Option<&str>) {
    let (title, w, h) = match page {
        "wizard" => ("Add cloud storage", 640.0, 600.0),
        "trash" => ("Trash", 760.0, 520.0),
        "conflicts" => ("Conflicts", 760.0, 480.0),
        "activity" => ("Recent activity", 760.0, 560.0),
        "settings" => ("Settings", 620.0, 600.0),
        "disconnect" => ("Disconnect", 520.0, 360.0),
        _ => return,
    };
    let label = match query {
        Some(q) => format!("{page}-{}", q.replace(|c: char| !c.is_ascii_alphanumeric(), "")),
        None => page.to_string(),
    };
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let url = match query {
        Some(q) => format!("{page}.html?{q}"),
        None => format!("{page}.html"),
    };
    if let Err(e) = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
        .title(format!("{title} — {APP_NAME}"))
        .inner_size(w, h)
        .min_inner_size(420.0, 300.0)
        .center()
        .build()
    {
        tracing::warn!("opening {page}: {e}");
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = app.state::<Arc<AppState>>().inner().clone();
    let app2 = app.clone();
    let id = id.to_string();
    match id.as_str() {
        "add" => open_page(app, "wizard", None),
        "trash" => open_page(app, "trash", None),
        "conflicts" => open_page(app, "conflicts", None),
        "activity" => open_page(app, "activity", None),
        "settings" => open_page(app, "settings", None),
        "quit" => {
            st.quitting.store(true, Ordering::SeqCst);
            tauri::async_runtime::spawn(async move {
                daemon::stop(&st.client).await;
                app2.exit(0);
            });
        }
        _ => {
            tauri::async_runtime::spawn(async move {
                let r = match id.as_str() {
                    "pause_all" | "resume_all" => st.client.call(&id, json!({})).await,
                    other => drive_action(&app2, &st, other).await,
                };
                if let Err(e) = r {
                    tracing::warn!("{id}: {e:#}");
                }
                refresh_menu(&app2, &st).await;
            });
        }
    }
}

/// `drive:<id>:<action>` from a drive's submenu.
async fn drive_action(app: &AppHandle, st: &AppState, id: &str) -> anyhow::Result<Value> {
    let mut parts = id.splitn(3, ':');
    let (Some("drive"), Some(n), Some(action)) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(Value::Null);
    };
    let drive_id: i64 = n.parse()?;
    let p = json!({ "drive_id": drive_id });
    match action {
        "open" => {
            let drives = st.client.call("drive.list", json!({})).await?;
            if let Some(f) = drives
                .as_array()
                .and_then(|a| a.iter().find(|d| d["id"] == drive_id))
                .and_then(|d| d["folder"].as_str())
            {
                app.opener().open_path(f, None::<&str>)?;
            }
            Ok(Value::Null)
        }
        "refresh" => st.client.call("sync", p).await,
        "pause" => st.client.call("drive.pause", p).await,
        "resume" => st.client.call("drive.resume", p).await,
        "connect" => {
            let drives = st.client.call("drive.list", json!({})).await?;
            let remote = drives
                .as_array()
                .and_then(|a| a.iter().find(|d| d["id"] == drive_id))
                .and_then(|d| d["remote"].as_str())
                .unwrap_or_default()
                .to_string();
            st.client.call("drive.connect", json!({ "remote": remote })).await
        }
        "disconnect" => {
            open_page(app, "disconnect", Some(&format!("id={drive_id}")));
            Ok(Value::Null)
        }
        _ => Ok(Value::Null),
    }
}

/// Rebuild the tray menu from the daemon's status (PLAN.md §6.1).
async fn refresh_menu(app: &AppHandle, st: &AppState) {
    let status = st.client.call("status", json!({})).await;
    let drives = st.client.call("drive.list", json!({})).await.unwrap_or(json!([]));
    let (headline, trash, conflicts, all_paused) = match &status {
        Ok(s) => {
            let state = s["state"].as_str().unwrap_or("");
            let pending = s["pending"].as_i64().unwrap_or(0);
            let head = match state {
                "Syncing" => format!(
                    "{APP_NAME} — Syncing {pending} item{}…",
                    if pending == 1 { "" } else { "s" }
                ),
                other => format!("{APP_NAME} — {other}"),
            };
            (
                head,
                s["trash"]["count"].as_i64().unwrap_or(0),
                s["conflicts"].as_i64().unwrap_or(0),
                state == "Paused",
            )
        }
        Err(_) => (format!("{APP_NAME} — not running"), 0, 0, false),
    };
    let key = format!("{headline}|{trash}|{conflicts}|{all_paused}|{drives}");
    {
        let mut last = st.menu_key.lock().unwrap();
        if *last == key {
            return;
        }
        *last = key;
    }
    match build_menu(app, &headline, &drives, trash, conflicts, all_paused) {
        Ok(menu) => {
            if let Some(t) = app.tray_by_id(TRAY) {
                let _ = t.set_menu(Some(menu));
                let _ = t.set_tooltip(Some(&headline));
            }
        }
        Err(e) => tracing::warn!("tray menu: {e}"),
    }
}

fn build_menu(
    app: &AppHandle,
    headline: &str,
    drives: &Value,
    trash: i64,
    conflicts: i64,
    all_paused: bool,
) -> tauri::Result<Menu<tauri::Wry>> {
    let mut m = MenuBuilder::new(app)
        .item(&MenuItem::with_id(app, "status", headline, false, None::<&str>)?)
        .separator();
    for d in drives.as_array().into_iter().flatten() {
        let id = d["id"].as_i64().unwrap_or(0);
        let name = d["name"].as_str().unwrap_or("?");
        let connected = d["state"] == "connected";
        let paused = d["paused"].as_bool().unwrap_or(false);
        let label = match (connected, paused) {
            (false, _) => format!("{name} (disconnected)"),
            (true, true) => format!("{name} (paused)"),
            _ => name.to_string(),
        };
        let mut sub = SubmenuBuilder::new(app, label);
        if connected {
            sub = sub
                .text(format!("drive:{id}:open"), "Open folder")
                .text(format!("drive:{id}:refresh"), "Refresh");
            sub = if paused {
                sub.text(format!("drive:{id}:resume"), "Resume")
            } else {
                sub.text(format!("drive:{id}:pause"), "Pause")
            };
            sub = sub.separator().text(format!("drive:{id}:disconnect"), "Disconnect…");
        } else {
            sub = sub.text(format!("drive:{id}:connect"), "Connect again");
        }
        m = m.item(&sub.build()?);
    }
    if drives.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        m = m.separator();
    }
    m = m
        .text("add", "Add cloud storage…")
        .separator()
        .text("trash", format!("Trash ({trash})…"))
        .text("conflicts", format!("Conflicts ({conflicts})…"))
        .text("activity", "Recent activity…")
        .separator();
    m = if all_paused {
        m.text("resume_all", "Resume all syncing")
    } else {
        m.text("pause_all", "Pause all syncing")
    };
    m.text("settings", "Settings…").text("quit", "Quit").build()
}

// ---------------------------------------------------------------- commands

#[tauri::command]
async fn rpc(state: State<'_, Arc<AppState>>, method: String, params: Option<Value>) -> Result<Value, String> {
    state
        .client
        .ui_call(&method, params.unwrap_or(json!({})))
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Add a drive; the sign-in page, if any, opens in the browser and is also
/// sent to the window ("signin-url") so it can offer a link.
#[tauri::command]
async fn add_drive(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, Arc<AppState>>,
    params: Value,
) -> Result<Value, String> {
    let no_browser = std::env::var_os("SYNCLONE_NO_BROWSER").is_some();
    let mut on_browser = move |url: String| {
        let _ = window.emit("signin-url", url.clone());
        if !no_browser {
            let _ = app.opener().open_url(url, None::<&str>);
        }
    };
    state
        .client
        .add_drive(params, &mut on_browser)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn open_path(app: AppHandle, path: String) -> Result<(), String> {
    // Tests drive the windows without Explorer popping up.
    if std::env::var_os("SYNCLONE_NO_OPEN").is_some() {
        return Ok(());
    }
    app.opener().open_path(path, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    app.opener().reveal_item_in_dir(path).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https links open from here".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn autostart_get(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn autostart_set(app: AppHandle, on: bool) -> Result<bool, String> {
    let al = app.autolaunch();
    if on { al.enable() } else { al.disable() }.map_err(|e| e.to_string())?;
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn open_window(app: AppHandle, page: String) {
    open_page(&app, &page, None);
}

#[tauri::command]
fn close_self(window: WebviewWindow) {
    let _ = window.close();
}

/// SYNCLONE_ADVANCED=1 also offers rclone's wrapper services (alias, crypt,
/// union...) in the wizard: the start of advanced mode (§6.4).
#[tauri::command]
fn advanced_mode() -> bool {
    std::env::var("SYNCLONE_ADVANCED").as_deref() == Ok("1")
}
