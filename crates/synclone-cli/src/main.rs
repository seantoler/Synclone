//! `synclonectl`: every Synclone operation as a command (PLAN.md §2.3).
//! Paths are written the way they appear in the Synclone folder:
//! `"<cloud drive>/<path inside it>"`, e.g. `"Google Drive/Photos/2026"`.

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Map, Value};

#[derive(Parser)]
#[command(name = "synclonectl", version, about = "Control Synclone from the command line")]
struct Cli {
    /// Daemon socket (Unix) or pipe name (Windows). Default: SYNCLONE_SOCKET or the standard location.
    #[arg(long, global = true)]
    socket: Option<String>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the Synclone service in the foreground.
    Daemon,
    /// Stop the running service.
    Shutdown,
    /// Overall state: syncing, up to date, attention needed.
    Status,
    /// Cloud drives (connected cloud storage).
    #[command(subcommand)]
    Drive(DriveCmd),
    /// List a folder: `synclonectl ls "Google Drive/Photos"`.
    Ls { path: String },
    /// Download a file and keep it (what double-clicking a placeholder does).
    Open { path: String },
    /// Save to device: download a file, or a folder with everything in it.
    Download {
        path: String,
        /// Folders: only the files directly inside.
        #[arg(long)]
        no_recursive: bool,
    },
    /// Remove from device: free the local copy; the cloud copy stays.
    Free {
        path: String,
        #[arg(long)]
        no_recursive: bool,
    },
    /// Check for changes now and upload/download everything pending.
    Sync { drive: Option<String> },
    /// Pause all syncing.
    Pause,
    /// Resume all syncing.
    Resume,
    /// Synclone's trash: local deletes waiting before anything is deleted from the cloud.
    #[command(subcommand)]
    Trash(TrashCmd),
    /// Files changed in two places at once; both versions were kept.
    Conflicts {
        #[arg(long)]
        all: bool,
        /// Mark a conflict as dealt with.
        #[arg(long)]
        resolve: Option<i64>,
    },
    /// Recent activity (last 200 by default).
    Activity {
        #[arg(long, default_value_t = 200)]
        limit: u64,
    },
    /// Items whose names cannot exist on this computer.
    Unsyncable,
    /// The operations journal (advanced).
    Ops,
}

#[derive(Subcommand)]
enum DriveCmd {
    /// List cloud drives.
    List,
    /// Kinds of cloud storage that can be added.
    Services,
    /// Add cloud storage: `synclonectl drive add drive` (Google Drive),
    /// `onedrive`, `drime --param access_token=...`, or any other type.
    Add {
        /// Storage type (drive, onedrive, drime, ... see `drive services`).
        kind: String,
        /// Folder name (default: the service name).
        #[arg(long)]
        name: Option<String>,
        /// Settings as name=value (repeatable).
        #[arg(long = "param", value_name = "NAME=VALUE")]
        params: Vec<String>,
    },
    /// Connect an existing rclone remote by its name.
    Connect {
        remote: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Pick up remotes added to the rclone config since the last start.
    Import,
    /// Disconnect: remove the folder from this computer. Nothing is deleted in the cloud.
    Remove {
        drive: String,
        /// If some files have changes not uploaded yet, keep them in a local Recovered folder.
        #[arg(long)]
        recover: bool,
        /// Also forget the drive (it is not imported again).
        #[arg(long)]
        forget: bool,
        /// Also sign out (delete the rclone remote; cloud data is untouched).
        #[arg(long)]
        sign_out: bool,
    },
    Rename {
        drive: String,
        new_name: String,
    },
    Pause {
        drive: String,
    },
    Resume {
        drive: String,
    },
}

#[derive(Subcommand)]
enum TrashCmd {
    List,
    /// Restore one entry (by id) or all.
    Restore {
        id: Option<i64>,
        #[arg(long)]
        all: bool,
    },
    /// Empty the trash and delete those items from the cloud.
    Empty {
        /// Confirm the deletion.
        #[arg(long)]
        yes: bool,
    },
}

fn split_drive_path(s: &str) -> (String, String) {
    let s = s.trim_matches('/');
    match s.split_once('/') {
        Some((d, p)) => (d.to_string(), p.to_string()),
        None => (s.to_string(), String::new()),
    }
}

fn dp(s: &str) -> Value {
    let (d, p) = split_drive_path(s);
    json!({"drive": d, "path": p})
}

fn with(mut v: Value, k: &str, x: Value) -> Value {
    v[k] = x;
    v
}

fn open_in_browser(url: &str) {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if r.is_err() {
        eprintln!("(could not open a browser automatically)");
    }
}

fn human_size(n: i64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1000.0 && i < units.len() - 1 {
        f /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.1} {}", units[i])
    }
}

fn print(cmd: &str, v: &Value, raw: bool) {
    if raw {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    match cmd {
        "status" => {
            println!(
                "{} — {}",
                v["app"].as_str().unwrap_or("Synclone"),
                v["state"].as_str().unwrap_or("")
            );
            println!("  cloud drives: {}", v["drives"]);
            println!("  pending: {}   failed: {}", v["pending"], v["failed"]);
            println!(
                "  trash: {} item(s), {}",
                v["trash"]["count"],
                human_size(v["trash"]["total_size"].as_i64().unwrap_or(0))
            );
            println!("  conflicts: {}", v["conflicts"]);
        }
        "drive.list" => {
            let a = v.as_array().cloned().unwrap_or_default();
            if a.is_empty() {
                println!("No cloud drives yet. Add one with `synclonectl drive add <type>`.");
            }
            for d in a {
                let mut flags = Vec::new();
                if d["state"] == "disconnected" {
                    flags.push("disconnected");
                }
                if d["paused"] == true {
                    flags.push("paused");
                }
                println!(
                    "{}\t{}\t(rclone remote \"{}\"){}",
                    d["name"].as_str().unwrap_or(""),
                    d["service"].as_str().unwrap_or(""),
                    d["remote"].as_str().unwrap_or(""),
                    if flags.is_empty() {
                        String::new()
                    } else {
                        format!("\t[{}]", flags.join(", "))
                    }
                );
            }
        }
        "ls" => {
            for i in v.as_array().cloned().unwrap_or_default() {
                let name = i["name"].as_str().unwrap_or("");
                let state = match (
                    i["is_dir"].as_bool().unwrap_or(false),
                    i["state"].as_str().unwrap_or(""),
                ) {
                    (true, _) => "folder".to_string(),
                    (false, "downloaded") => "on device".to_string(),
                    (false, "trashed") => "in trash".to_string(),
                    _ => "in cloud".to_string(),
                };
                let size = i["size"]
                    .as_i64()
                    .filter(|_| i["is_dir"] != true)
                    .map(human_size)
                    .unwrap_or_default();
                let extra = if i["unsyncable"].is_string() {
                    "  (not synced: name not allowed here)"
                } else {
                    ""
                };
                println!(
                    "{state:<10} {size:>10}  {name}{}{extra}",
                    if i["is_dir"] == true { "/" } else { "" }
                );
            }
        }
        "trash.list" => {
            let a = v.as_array().cloned().unwrap_or_default();
            if a.is_empty() {
                println!("The trash is empty.");
            }
            for t in a {
                println!(
                    "{}\t{}\t{}{}",
                    t["id"],
                    human_size(t["size"].as_i64().unwrap_or(0)),
                    t["path"].as_str().unwrap_or(""),
                    if t["is_dir"] == true { "/" } else { "" }
                );
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(v).unwrap_or_default()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Daemon = cli.cmd {
        tracing_init();
        return synclone_daemon::run_from_env().await;
    }
    let cfg = synclone_daemon::Config::from_env();
    let socket = cli.socket.clone().unwrap_or(cfg.socket);
    let (method, params): (&str, Value) = match &cli.cmd {
        Cmd::Daemon => unreachable!(),
        Cmd::Shutdown => ("shutdown", json!({})),
        Cmd::Status => ("status", json!({})),
        Cmd::Drive(d) => match d {
            DriveCmd::List => ("drive.list", json!({})),
            DriveCmd::Services => ("drive.services", json!({})),
            DriveCmd::Add { kind, name, params } => {
                let mut m = Map::new();
                for kv in params {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| anyhow!("--param needs NAME=VALUE, got {kv:?}"))?;
                    m.insert(k.to_string(), Value::String(v.to_string()));
                }
                ("drive.add", json!({"type": kind, "name": name, "params": m}))
            }
            DriveCmd::Connect { remote, name } => ("drive.connect", json!({"remote": remote, "name": name})),
            DriveCmd::Import => ("drive.import", json!({})),
            DriveCmd::Remove {
                drive,
                recover,
                forget,
                sign_out,
            } => (
                "drive.remove",
                json!({"drive": drive, "recover": recover, "forget": forget, "sign_out": sign_out}),
            ),
            DriveCmd::Rename { drive, new_name } => ("drive.rename", json!({"drive": drive, "new_name": new_name})),
            DriveCmd::Pause { drive } => ("drive.pause", json!({"drive": drive})),
            DriveCmd::Resume { drive } => ("drive.resume", json!({"drive": drive})),
        },
        Cmd::Ls { path } => ("ls", dp(path)),
        Cmd::Open { path } => ("open", dp(path)),
        Cmd::Download { path, no_recursive } => ("download", with(dp(path), "recursive", json!(!no_recursive))),
        Cmd::Free { path, no_recursive } => ("free", with(dp(path), "recursive", json!(!no_recursive))),
        Cmd::Sync { drive } => ("sync", json!({"drive": drive})),
        Cmd::Pause => ("pause_all", json!({})),
        Cmd::Resume => ("resume_all", json!({})),
        Cmd::Trash(t) => match t {
            TrashCmd::List => ("trash.list", json!({})),
            TrashCmd::Restore { id, all } => ("trash.restore", json!({"id": id, "all": all})),
            TrashCmd::Empty { yes } => ("trash.empty", json!({"confirm": yes})),
        },
        Cmd::Conflicts { all, resolve } => match resolve {
            Some(id) => ("conflicts.resolve", json!({"id": id})),
            None => ("conflicts.list", json!({"all": all})),
        },
        Cmd::Activity { limit } => ("activity", json!({"limit": limit})),
        Cmd::Unsyncable => ("unsyncable", json!({})),
        Cmd::Ops => ("ops", json!({})),
    };
    let mut on_event = |ev: &Value| {
        if let Some(url) = ev.get("open_browser").and_then(Value::as_str) {
            eprintln!("Sign in with your browser: {url}");
            if std::env::var_os("SYNCLONE_NO_BROWSER").is_none() {
                open_in_browser(url);
            }
        }
    };
    match synclone_daemon::rpc::call(&socket, method, params, &mut on_event).await {
        Ok(v) => {
            print(method, &v, cli.json);
            Ok(())
        }
        Err(e) => {
            eprintln!("synclonectl: {e:#}");
            std::process::exit(1);
        }
    }
}

fn tracing_init() {
    synclone_daemon::init_logging();
}
