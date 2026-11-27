//! The Synclone daemon: supervises rclone, hosts the engine, runs the
//! background loops (ops, polling, local scans) and serves JSON-RPC to the
//! CLI and, later, the tray app (PLAN.md §2.1).

pub mod rpc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use synclone_core::db::Db;
use synclone_core::provider::Provider;
use synclone_core::*;
use synclone_provider_stub::StubProvider;
use synclone_rclone::config_flow::{Answer, FlowEvent};
use synclone_rclone::{DaemonConfig, RcloneApi, RcloneDaemon};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Notify};

/// Where everything lives. Every field can be overridden by environment
/// variables, which is how tests keep away from the user's real files.
#[derive(Debug, Clone)]
pub struct Config {
    /// `SYNCLONE_STATE_DIR`: DB, logs, staging.
    pub state_dir: PathBuf,
    /// `SYNCLONE_ROOT`: the folder holding one subfolder per cloud drive.
    pub sync_root: PathBuf,
    /// `SYNCLONE_SOCKET`: socket path (Unix) or pipe name (Windows).
    pub socket: String,
    /// `SYNCLONE_RCLONE_CONFIG` (or `RCLONE_CONFIG`): rclone config file.
    /// None: rclone's default, i.e. the user's existing config (§2.5).
    pub rclone_config: Option<PathBuf>,
    /// `SYNCLONE_RCLONE`: rclone binary.
    pub rclone: Option<PathBuf>,
    /// `SYNCLONE_RCLONE_ARGS`: extra rclone flags (space separated), e.g. `--bwlimit 1M`.
    pub rclone_args: Vec<String>,
    /// `SYNCLONE_SCAN_SECS`: how often to compare the drive folders with
    /// the DB. The stub has no watcher and relies on it (default 5 s); the
    /// Windows provider gets OS events and scans only as a safety net (60 s).
    pub scan_interval: Duration,
    /// `SYNCLONE_PROVIDER`: `native` (Windows Cloud Files; the default on
    /// Windows) or `stub` (plain folders; the default elsewhere until the
    /// macOS provider exists, and what the CLI tests use).
    pub provider: ProviderKind,
    pub settings: EngineSettings,
}

impl Config {
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let state_dir = env("SYNCLONE_STATE_DIR")
            .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join(APP_NAME));
        let sync_root =
            env("SYNCLONE_ROOT").unwrap_or_else(|| dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(APP_NAME));
        let socket = std::env::var("SYNCLONE_SOCKET")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_socket(&state_dir));
        let provider = match std::env::var("SYNCLONE_PROVIDER").as_deref() {
            Ok("stub") => ProviderKind::Stub,
            Ok("native") => ProviderKind::Native,
            _ if cfg!(windows) => ProviderKind::Native,
            _ => ProviderKind::Stub,
        };
        let mut settings = EngineSettings::default();
        if let Some(ms) = std::env::var("SYNCLONE_DEBOUNCE_MS").ok().and_then(|s| s.parse().ok()) {
            settings.debounce_ms = ms;
        }
        Self {
            rclone_config: env("SYNCLONE_RCLONE_CONFIG").or_else(|| env("RCLONE_CONFIG")),
            rclone: env("SYNCLONE_RCLONE"),
            rclone_args: std::env::var("SYNCLONE_RCLONE_ARGS")
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            scan_interval: Duration::from_secs(
                std::env::var("SYNCLONE_SCAN_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(if provider == ProviderKind::Native { 60 } else { 5 }),
            ),
            provider,
            state_dir,
            sync_root,
            socket,
            settings,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Stub,
    Native,
}

pub fn default_socket(state_dir: &Path) -> String {
    if cfg!(windows) {
        rpc::default_pipe_name()
    } else {
        state_dir.join("daemon.sock").display().to_string()
    }
}

pub struct Daemon {
    pub engine: Arc<Engine>,
    pub rclone: Arc<RcloneDaemon>,
    pub config: Config,
    wake: Notify,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Daemon {
    /// Start rclone and the engine (no background loops yet).
    pub async fn start(config: Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&config.state_dir)?;
        let rclone_bin = config
            .rclone
            .clone()
            .or_else(synclone_rclone::locate::find_rclone)
            .ok_or_else(|| anyhow!("rclone was not found (set SYNCLONE_RCLONE or run scripts/fetch-rclone)"))?;
        let mut rc = DaemonConfig::new(rclone_bin);
        rc.config_file = config.rclone_config.clone();
        rc.log_file = Some(config.state_dir.join("logs").join("rclone.log"));
        rc.extra_args = config.rclone_args.clone();
        let rclone = RcloneDaemon::start(rc).await.context("starting rclone")?;
        let db = Arc::new(Db::open(&config.state_dir.join("state.db")).context("opening the state database")?);
        let provider: Arc<dyn Provider> = match config.provider {
            ProviderKind::Stub => Arc::new(StubProvider::new(&config.sync_root, config.state_dir.join("recycle"))),
            #[cfg(windows)]
            ProviderKind::Native => Arc::new(
                synclone_provider_windows::WindowsProvider::start(
                    {
                        let mut c = synclone_provider_windows::WindowsProviderConfig::new(&config.sync_root);
                        // `SYNCLONE_SYNC_ROOT_ACCOUNT`: tests register their own sync
                        // root and never touch the user's ("default").
                        if let Ok(a) = std::env::var("SYNCLONE_SYNC_ROOT_ACCOUNT") {
                            if !a.is_empty() {
                                c.display_name = format!("{APP_NAME} ({a})");
                                c.account = a;
                            }
                        }
                        c
                    },
                    db.clone(),
                    rclone.endpoint().clone(),
                )
                .context("connecting the Synclone folder to Windows")?,
            ),
            // macOS arrives in Phase 6.
            #[cfg(not(windows))]
            ProviderKind::Native => {
                bail!("there is no native file provider for this OS yet; set SYNCLONE_PROVIDER=stub")
            }
        };
        let api: Arc<dyn RcloneApi> = rclone.client();
        let engine = Engine::new(
            db,
            api,
            provider,
            Arc::new(SystemClock),
            config.settings.clone(),
            config.state_dir.clone(),
        );
        let n = engine.recover()?;
        if n > 0 {
            tracing::info!("resuming {n} operation(s) interrupted by a restart");
        }
        let (stop, _) = tokio::sync::watch::channel(false);
        Ok(Arc::new(Self {
            engine,
            rclone,
            config,
            wake: Notify::new(),
            stop,
        }))
    }

    /// Run until `shutdown`: background loops plus the RPC server.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        if let Err(e) = self.engine.import_remotes().await {
            tracing::warn!("could not read the rclone config: {e:#}");
        }
        for d in self.engine.drives()? {
            if d.state == DriveState::Connected {
                let _ = self.engine.provider.register_drive(&d);
                // Changes made while Synclone was not running.
                let _ = self.engine.scan_drive(d.id);
            }
        }
        let mut tasks = Vec::new();
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.ops_loop().await }));
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.poll_loop().await }));
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.scan_loop().await }));
        if let Some(rx) = self.engine.provider.take_events() {
            let me = self.clone();
            tasks.push(tokio::spawn(async move { me.event_loop(rx).await }));
        }
        let server = self.clone().serve();
        let mut stop = self.stop.subscribe();
        tokio::select! {
            r = server => { r?; }
            _ = stop.changed() => {}
        }
        for t in tasks {
            t.abort();
        }
        self.rclone.shutdown().await;
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.config.socket);
        Ok(())
    }

    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    async fn ops_loop(&self) {
        loop {
            match self.engine.run_due().await {
                Ok(r) if r.executed > 0 => continue,
                Ok(_) => {}
                Err(e) => tracing::warn!("running operations: {e:#}"),
            }
            let wait = match self.engine.db.next_op_time() {
                Ok(Some(t)) => (t - self.engine.now()).clamp(50, 5_000) as u64,
                _ => 5_000,
            };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn poll_loop(&self) {
        loop {
            if let Ok(drives) = self.engine.drives() {
                for d in drives {
                    if d.state != DriveState::Connected || d.paused {
                        continue;
                    }
                    let last = self.engine.db.last_poll(d.id).ok().flatten().unwrap_or(0);
                    if self.engine.now() - last >= d.poll_interval_secs * 1000 {
                        match self.engine.poll_drive(d.id).await {
                            Ok(r) if r.changes() > 0 => self.wake.notify_one(),
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("checking {} for changes: {e:#}", d.display_name);
                                let _ = self.engine.db.set_last_poll(d.id, self.engine.now());
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Compare the drive folders with the DB: the stub's only change
    /// detection, and a safety net for missed OS events otherwise.
    async fn scan_loop(&self) {
        loop {
            tokio::time::sleep(self.config.scan_interval).await;
            if let Ok(drives) = self.engine.drives() {
                for d in drives {
                    if d.state == DriveState::Connected {
                        if let Ok(n) = self.engine.scan_drive(d.id) {
                            if n > 0 {
                                self.wake.notify_one();
                            }
                        }
                    }
                }
            }
        }
    }

    async fn event_loop(&self, mut rx: mpsc::UnboundedReceiver<ProviderEvent>) {
        while let Some(ev) = rx.recv().await {
            if let Err(e) = self.engine.handle_event(ev) {
                tracing::warn!("handling a file event: {e:#}");
            }
            self.wake.notify_one();
        }
    }

    async fn serve(self: Arc<Self>) -> Result<()> {
        #[cfg(unix)]
        {
            let path = PathBuf::from(&self.config.socket);
            if path.exists() {
                // A stale socket from a crash, unless another daemon answers.
                if tokio::net::UnixStream::connect(&path).await.is_ok() {
                    bail!("another Synclone service is already running ({})", path.display());
                }
                std::fs::remove_file(&path)?;
            }
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            let listener = tokio::net::UnixListener::bind(&path)?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            tracing::info!("listening on {}", path.display());
            loop {
                let (s, _) = listener.accept().await?;
                let me = self.clone();
                tokio::spawn(async move { me.handle_conn(s).await });
            }
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let name = self.config.socket.clone();
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            loop {
                server.connect().await?;
                let s = server;
                server = ServerOptions::new().reject_remote_clients(true).create(&name)?;
                let me = self.clone();
                tokio::spawn(async move { me.handle_conn(s).await });
            }
        }
    }

    async fn handle_conn<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(self: Arc<Self>, s: S) {
        let (r, w) = tokio::io::split(s);
        let w = Arc::new(tokio::sync::Mutex::new(w));
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let req: rpc::Request = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    let _ = write_line(
                        &w,
                        &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}}),
                    )
                    .await;
                    continue;
                }
            };
            let (etx, mut erx) = mpsc::unbounded_channel::<Value>();
            let w2 = w.clone();
            let fwd = tokio::spawn(async move {
                while let Some(ev) = erx.recv().await {
                    let _ = write_line(&w2, &json!({"jsonrpc":"2.0","method":"event","params":ev})).await;
                }
            });
            let result = self.dispatch(&req.method, req.params.clone(), etx).await;
            let _ = fwd.await;
            let resp = match result {
                Ok(v) => json!({"jsonrpc":"2.0","id":req.id,"result":v}),
                Err(e) => json!({"jsonrpc":"2.0","id":req.id,"error":{"code":1,"message":format!("{e:#}")}}),
            };
            if write_line(&w, &resp).await.is_err() {
                return;
            }
            self.wake.notify_one();
        }
    }

    /// A drive's folder on disk.
    pub fn folder(&self, d: &Drive) -> PathBuf {
        self.engine.provider.content_path(d, &RelPath::root())
    }

    fn drive_arg(&self, p: &Value) -> Result<Drive> {
        if let Some(id) = p.get("drive_id").and_then(Value::as_i64) {
            return self.engine.drive(id);
        }
        let name = p
            .get("drive")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("which cloud drive?"))?;
        self.engine.drive_by_name(name)
    }

    fn path_arg(p: &Value) -> Result<RelPath> {
        let s = p.get("path").and_then(Value::as_str).unwrap_or("");
        RelPath::new(s).ok_or_else(|| anyhow!("invalid path {s:?}"))
    }

    /// One RPC method. `events` receives notifications for the caller.
    pub async fn dispatch(&self, method: &str, p: Value, events: mpsc::UnboundedSender<Value>) -> Result<Value> {
        let e = &self.engine;
        Ok(match method {
            "status" => {
                let drives = e.drives()?;
                let pending = e.db.ops(Some("pending"))?.len();
                let failed = e.db.ops(Some("failed"))?.len();
                let conflicts = e.db.conflicts(false)?.len();
                let mut unsyncable = 0;
                for d in &drives {
                    unsyncable += e.db.unsyncable_items(d.id)?.len();
                }
                let all_paused = !drives.is_empty() && drives.iter().all(|d| d.paused);
                let state = if failed > 0 || conflicts > 0 || unsyncable > 0 {
                    "Attention needed"
                } else if all_paused {
                    "Paused"
                } else if pending > 0 {
                    "Syncing"
                } else {
                    "Up to date"
                };
                json!({
                    "app": APP_NAME,
                    "version": VERSION,
                    "state": state,
                    "pending": pending,
                    "failed": failed,
                    "drives": drives.len(),
                    "trash": e.trash_summary()?,
                    "conflicts": conflicts,
                    "unsyncable": unsyncable,
                    "rclone": format!("{:?}", self.rclone.state()),
                    "sync_root": self.config.sync_root,
                })
            }
            "drive.list" => {
                let mut out = Vec::new();
                for d in e.drives()? {
                    out.push(json!({
                        "id": d.id,
                        "name": d.display_name,
                        "service": synclone_core::engine::drives::service_name(&d.provider_type),
                        "remote": d.remote_name,
                        "type": d.provider_type,
                        "state": d.state,
                        "paused": d.paused,
                        "imported": d.imported,
                        "folder": self.folder(&d),
                    }));
                }
                Value::Array(out)
            }
            "drive.services" => {
                let provs = e.api.config_providers().await?;
                Value::Array(
                    provs
                        .into_iter()
                        .filter(|p| !p.hide)
                        .map(|p| json!({"type": p.name, "name": synclone_core::engine::drives::service_name(&p.name), "description": p.description}))
                        .collect(),
                )
            }
            "drive.add" => {
                let kind = p.get("type").and_then(Value::as_str).ok_or_else(|| anyhow!("which kind of cloud storage?"))?;
                let name = p.get("name").and_then(Value::as_str);
                let params: Map<String, Value> = p.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
                let mut unanswered = Vec::new();
                let mut ask = |q: &synclone_rclone::ConfigQuestion| {
                    unanswered.push(q.name.clone());
                    Answer::Cancel
                };
                let ev = events.clone();
                let mut on_event = move |fe: FlowEvent| {
                    if let FlowEvent::OpenBrowser(url) = fe {
                        let _ = ev.send(json!({"open_browser": url}));
                    }
                };
                let r = e.add_drive(kind, name, params, &mut ask, &mut on_event).await;
                match r {
                    Ok(d) => json!({"id": d.id, "name": d.display_name, "folder": self.folder(&d)}),
                    Err(err) if !unanswered.is_empty() => {
                        bail!("{err:#}. Provide these settings with --param name=value: {}", unanswered.join(", "))
                    }
                    Err(err) => return Err(err),
                }
            }
            "drive.connect" => {
                let remote = p.get("remote").and_then(Value::as_str).ok_or_else(|| anyhow!("which remote?"))?;
                let d = e.connect_remote(remote, p.get("name").and_then(Value::as_str)).await?;
                json!({"id": d.id, "name": d.display_name, "folder": self.folder(&d)})
            }
            "drive.import" => {
                let added = e.import_remotes().await?;
                Value::Array(added.into_iter().map(|d| json!({"id": d.id, "name": d.display_name})).collect())
            }
            "drive.remove" => {
                let d = self.drive_arg(&p)?;
                let recover = p.get("recover").and_then(Value::as_bool).unwrap_or(false);
                let forget = p.get("forget").and_then(Value::as_bool).unwrap_or(false);
                let sign_out = p.get("sign_out").and_then(Value::as_bool).unwrap_or(false);
                let recovered = if d.state == DriveState::Connected { e.disconnect(d.id, recover).await? } else { None };
                if forget || sign_out {
                    e.forget_drive(d.id, sign_out).await?;
                }
                json!({"name": d.display_name, "recovered": recovered})
            }
            "drive.rename" => {
                let d = self.drive_arg(&p)?;
                let new = p.get("new_name").and_then(Value::as_str).ok_or_else(|| anyhow!("new name?"))?;
                let old_dir = self.folder(&d);
                let nd = e.rename_drive(d.id, new)?;
                if old_dir.exists() {
                    std::fs::rename(&old_dir, self.folder(&nd))?;
                }
                json!({"name": nd.display_name})
            }
            "drive.pause" | "drive.resume" => {
                let d = self.drive_arg(&p)?;
                e.set_paused(d.id, method == "drive.pause")?;
                json!({"name": d.display_name, "paused": method == "drive.pause"})
            }
            "pause_all" | "resume_all" => {
                for d in e.drives()? {
                    e.set_paused(d.id, method == "pause_all")?;
                }
                json!({"paused": method == "pause_all"})
            }
            "ls" => {
                let d = self.drive_arg(&p)?;
                let path = Self::path_arg(&p)?;
                if e.db.listed_at(d.id, &path)?.is_none() {
                    e.index_dir(&d, &path).await?;
                }
                let kids = e.db.children(d.id, &path)?;
                Value::Array(
                    kids.into_iter()
                        .map(|i| {
                            json!({
                                "name": i.path.name(),
                                "is_dir": i.is_dir,
                                "size": i.remote.as_ref().map(|r| r.size),
                                "state": i.local_state,
                                "pinned": i.pinned,
                                "unsyncable": i.unsyncable,
                            })
                        })
                        .collect(),
                )
            }
            "open" => {
                let d = self.drive_arg(&p)?;
                let path = Self::path_arg(&p)?;
                e.handle_event(ProviderEvent::HydrationRequested { drive: d.id, path: path.clone() })?;
                e.run_until_idle(false, 50).await?;
                json!({"path": self.engine.provider.content_path(&d, &path)})
            }
            "download" => {
                let d = self.drive_arg(&p)?;
                let path = Self::path_arg(&p)?;
                let recursive = p.get("recursive").and_then(Value::as_bool).unwrap_or(true);
                e.pin(d.id, &path, recursive).await?;
                if p.get("wait").and_then(Value::as_bool).unwrap_or(true) {
                    e.run_until_idle(false, 10_000).await?;
                }
                json!({"path": self.engine.provider.content_path(&d, &path)})
            }
            "free" => {
                let d = self.drive_arg(&p)?;
                let path = Self::path_arg(&p)?;
                let recursive = p.get("recursive").and_then(Value::as_bool).unwrap_or(true);
                let kept = e.free(d.id, &path, recursive)?;
                json!({"kept": kept})
            }
            "sync" => {
                let ids: Vec<DriveId> = match self.drive_arg(&p) {
                    Ok(d) => vec![d.id],
                    Err(_) => e.drives()?.into_iter().filter(|d| d.state == DriveState::Connected).map(|d| d.id).collect(),
                };
                let mut reports = Vec::new();
                for id in ids {
                    e.scan_drive(id)?;
                    let poll = e.poll_drive(id).await?;
                    reports.push(json!({"drive_id": id, "poll": poll}));
                }
                let run = e.run_until_idle(true, 10_000).await?;
                json!({"drives": reports, "ops": run})
            }
            "trash.list" => serde_json::to_value(
                e.trash_list()?
                    .into_iter()
                    .map(|t| json!({"id": t.id, "drive_id": t.drive_id, "path": t.path, "is_dir": t.is_dir, "size": t.size, "deleted_at": t.deleted_at}))
                    .collect::<Vec<_>>(),
            )?,
            "trash.restore" => {
                if p.get("all").and_then(Value::as_bool).unwrap_or(false) {
                    json!({"restored": e.restore_all()?})
                } else {
                    let id = p.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("which trash entry?"))?;
                    e.restore_trash(id, None)?;
                    json!({"restored": 1})
                }
            }
            "trash.empty" => {
                let summary = e.trash_summary()?;
                if !p.get("confirm").and_then(Value::as_bool).unwrap_or(false) {
                    bail!(
                        "this deletes {} item(s), {} bytes, from the cloud (to the service's own trash where it has one). Confirm with --yes",
                        summary.count,
                        summary.total_size
                    );
                }
                e.request_empty_trash()?;
                e.run_until_idle(false, 10_000).await?;
                json!({"emptied": summary, "left": e.trash_summary()?})
            }
            "conflicts.list" => {
                let all = p.get("all").and_then(Value::as_bool).unwrap_or(false);
                serde_json::to_value(e.db.conflicts(all)?)?
            }
            "conflicts.resolve" => {
                let id = p.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("which conflict?"))?;
                e.db.resolve_conflict(id)?;
                json!({"resolved": id})
            }
            "activity" => {
                let n = p.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
                serde_json::to_value(e.db.activity(n)?)?
            }
            "unsyncable" => {
                let mut out = Vec::new();
                for d in e.drives()? {
                    for i in e.db.unsyncable_items(d.id)? {
                        out.push(json!({"drive": d.display_name, "path": i.path, "reason": i.unsyncable}));
                    }
                }
                Value::Array(out)
            }
            "ops" => serde_json::to_value(e.db.ops(None)?)?,
            "shutdown" => {
                self.shutdown();
                json!({"stopping": true})
            }
            other => bail!("unknown method {other}"),
        })
    }
}

async fn write_line<W: AsyncWrite + Unpin>(w: &tokio::sync::Mutex<W>, v: &Value) -> std::io::Result<()> {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    let mut g = w.lock().await;
    g.write_all(s.as_bytes()).await?;
    g.flush().await
}

/// Log to stderr (RUST_LOG controls the level; default info).
pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .try_init();
}

/// Entry point used by both `synclone-daemon` and `synclonectl daemon`.
pub async fn run_from_env() -> Result<()> {
    let config = Config::from_env();
    let logs = config.state_dir.join("logs");
    std::fs::create_dir_all(&logs)?;
    let d = Daemon::start(config).await?;
    let d2 = d.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        d2.shutdown();
    });
    d.run().await
}
