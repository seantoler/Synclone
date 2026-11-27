//! The Synclone tray / menu bar app (PLAN.md §6). The windows are thin
//! views over the daemon's JSON-RPC; this library is the part that does not
//! need a GUI, so tests can drive exactly what the windows call:
//!
//! - [`Client`]: the daemon connection, limited to the methods the UI uses;
//! - [`daemon`]: start the background service when the app starts, stop it
//!   on Quit.

pub mod daemon;

use anyhow::{bail, Result};
use serde_json::Value;

/// The RPC methods windows may call. Anything else (e.g. `shutdown`, which
/// only the app's Quit uses) is refused, so the web view cannot reach it.
pub const UI_METHODS: &[&str] = &[
    "status",
    "drive.list",
    "drive.services",
    "drive.service_options",
    "drive.suggest_name",
    "drive.cancel_signin",
    "drive.remove",
    "drive.pause",
    "drive.resume",
    "drive.rename",
    "pause_all",
    "resume_all",
    "sync",
    "trash.list",
    "trash.restore",
    "trash.empty",
    "conflicts.list",
    "conflicts.resolve",
    "activity",
    "unsyncable",
    "settings.get",
    "settings.set",
];

/// A connection to the daemon (socket path or pipe name).
#[derive(Debug, Clone)]
pub struct Client {
    pub socket: String,
}

impl Client {
    pub fn new(socket: impl Into<String>) -> Self {
        Self { socket: socket.into() }
    }

    /// The daemon's address from the same environment it reads.
    pub fn from_env() -> Self {
        Self::new(synclone_daemon::Config::from_env().socket)
    }

    /// Any method (the app itself).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        synclone_daemon::rpc::call(&self.socket, method, params, &mut |_| {}).await
    }

    /// A method a window asked for.
    pub async fn ui_call(&self, method: &str, params: Value) -> Result<Value> {
        if !UI_METHODS.contains(&method) {
            bail!("{method} is not available to windows");
        }
        self.call(method, params).await
    }

    /// Add a cloud drive. `on_browser` gets the sign-in page to open for
    /// services that sign in in the browser.
    pub async fn add_drive(&self, params: Value, on_browser: &mut (dyn FnMut(String) + Send)) -> Result<Value> {
        synclone_daemon::rpc::call(&self.socket, "drive.add", params, &mut |ev| {
            if let Some(url) = ev.get("open_browser").and_then(Value::as_str) {
                on_browser(url.to_string());
            }
        })
        .await
    }
}
