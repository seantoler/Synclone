//! In-memory `RcloneApi` for tests.
//!
//! Remotes live in memory; an absolute path as `fs` is the real local file
//! system (so uploads and downloads move real bytes). Every call is logged,
//! faults can be injected per call, and OAuth sign-in is simulated so the
//! drive wizard's state machine can be tested without a browser.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use serde_json::{json, Map, Value};

use crate::api::{join_remote, RcloneApi, REMOTE_WRITE_CALLS};
use crate::error::{ErrorKind, RcloneError, Result};
use crate::types::*;

#[derive(Debug, Clone)]
pub struct MockObject {
    pub data: Arc<Vec<u8>>,
    pub mod_time: DateTime<Utc>,
    pub id: String,
}

#[derive(Debug, Clone, Default)]
pub struct MockStore {
    /// Files by path.
    pub files: BTreeMap<String, MockObject>,
    /// Explicit directories by path (parents of files exist implicitly).
    pub dirs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthOutcome {
    Pending,
    Approved,
    Denied,
}

#[derive(Default)]
struct Inner {
    stores: HashMap<String, MockStore>,
    config: BTreeMap<String, Map<String, Value>>,
    calls: Vec<String>,
    faults: HashMap<String, VecDeque<RcloneError>>,
    next_id: u64,
    oauth: Option<(String, OAuthOutcome)>,
    bwlimit: String,
    clock_offset_secs: i64,
    latency_ms: u64,
}

/// See the module docs.
#[derive(Clone)]
pub struct MockRclone {
    inner: Arc<Mutex<Inner>>,
    /// Whether listings report provider IDs (like Drive / OneDrive).
    pub with_ids: bool,
    pub version: String,
}

impl Default for MockRclone {
    fn default() -> Self {
        Self::new()
    }
}

fn md5_hex(data: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn not_found(path: &str, what: &str) -> RcloneError {
    RcloneError {
        kind: ErrorKind::NotFound,
        path: path.into(),
        status: Some(404),
        message: what.into(),
    }
}

fn is_local(fs: &str) -> bool {
    let b = fs.as_bytes();
    fs.starts_with('/')
        || fs.starts_with("\\\\")
        // "C:\\..." or "C:/..." is a local Windows path; "r:" is a remote.
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

fn parent_of(p: &str) -> &str {
    p.rfind('/').map(|i| &p[..i]).unwrap_or("")
}

impl MockRclone {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                bwlimit: "off".into(),
                ..Default::default()
            })),
            with_ids: true,
            version: "v1.76.0".into(),
        }
    }

    pub fn without_ids(mut self) -> Self {
        self.with_ids = false;
        self
    }

    /// Add a remote of type `memory` to the mock config.
    pub fn add_remote(&self, name: &str) {
        let mut g = self.inner.lock().unwrap();
        let mut m = Map::new();
        m.insert("type".into(), Value::String("memory".into()));
        g.config.insert(name.to_string(), m);
        g.stores.entry(format!("{name}:")).or_default();
    }

    // ---- direct access to remote contents (the "other device") ----

    fn new_id(g: &mut Inner) -> String {
        g.next_id += 1;
        format!("id{}", g.next_id)
    }

    fn now(g: &Inner) -> DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(g.clock_offset_secs)
    }

    /// Write a file directly on the remote, as another device would.
    pub fn put(&self, fs: &str, path: &str, data: &[u8]) {
        let mut g = self.inner.lock().unwrap();
        let now = Self::now(&g);
        Self::put_locked(&mut g, fs, path, data, now);
    }

    pub fn put_with_time(&self, fs: &str, path: &str, data: &[u8], t: DateTime<Utc>) {
        let mut g = self.inner.lock().unwrap();
        Self::put_locked(&mut g, fs, path, data, t);
    }

    fn put_locked(g: &mut Inner, fs: &str, path: &str, data: &[u8], t: DateTime<Utc>) {
        let path = path.trim_matches('/').to_string();
        let existing_id = g.stores.get(fs).and_then(|s| s.files.get(&path)).map(|o| o.id.clone());
        let id = existing_id.unwrap_or_else(|| Self::new_id(g));
        // Make parent directories explicit, with IDs.
        let mut dir = parent_of(&path).to_string();
        let mut missing = Vec::new();
        while !dir.is_empty() {
            if !g.stores.get(fs).map(|s| s.dirs.contains_key(&dir)).unwrap_or(false) {
                missing.push(dir.clone());
            }
            dir = parent_of(&dir).to_string();
        }
        for d in missing {
            let did = Self::new_id(g);
            g.stores.entry(fs.into()).or_default().dirs.insert(d, did);
        }
        g.stores.entry(fs.into()).or_default().files.insert(
            path,
            MockObject {
                data: Arc::new(data.to_vec()),
                mod_time: t,
                id,
            },
        );
    }

    pub fn get(&self, fs: &str, path: &str) -> Option<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        g.stores
            .get(fs)?
            .files
            .get(path.trim_matches('/'))
            .map(|o| (*o.data).clone())
    }

    pub fn exists(&self, fs: &str, path: &str) -> bool {
        let g = self.inner.lock().unwrap();
        let p = path.trim_matches('/');
        g.stores
            .get(fs)
            .map(|s| s.files.contains_key(p) || s.dirs.contains_key(p))
            .unwrap_or(false)
    }

    /// Delete directly on the remote (file or directory tree).
    pub fn remove(&self, fs: &str, path: &str) {
        let mut g = self.inner.lock().unwrap();
        let p = path.trim_matches('/').to_string();
        if let Some(s) = g.stores.get_mut(fs) {
            s.files.remove(&p);
            let prefix = format!("{p}/");
            s.files.retain(|k, _| !k.starts_with(&prefix));
            s.dirs.remove(&p);
            s.dirs.retain(|k, _| !k.starts_with(&prefix));
        }
    }

    /// Rename directly on the remote, keeping IDs (a server-side move).
    pub fn rename(&self, fs: &str, from: &str, to: &str) {
        let mut g = self.inner.lock().unwrap();
        Self::rename_locked(&mut g, fs, from, to);
    }

    fn rename_locked(g: &mut Inner, fs: &str, from: &str, to: &str) {
        let from = from.trim_matches('/').to_string();
        let to = to.trim_matches('/').to_string();
        let Some(s) = g.stores.get_mut(fs) else { return };
        if let Some(o) = s.files.remove(&from) {
            s.files.insert(to.clone(), o);
        } else {
            let prefix = format!("{from}/");
            let moved: Vec<_> = s.files.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            for k in moved {
                let o = s.files.remove(&k).unwrap();
                s.files.insert(format!("{to}/{}", &k[prefix.len()..]), o);
            }
            let moved: Vec<_> = s
                .dirs
                .keys()
                .filter(|k| k.starts_with(&prefix) || **k == from)
                .cloned()
                .collect();
            for k in moved {
                let id = s.dirs.remove(&k).unwrap();
                let nk = if k == from {
                    to.clone()
                } else {
                    format!("{to}/{}", &k[prefix.len()..])
                };
                s.dirs.insert(nk, id);
            }
        }
        // Make sure the new parents exist.
        let mut dir = parent_of(&to).to_string();
        while !dir.is_empty() {
            if !g.stores.get(fs).unwrap().dirs.contains_key(&dir) {
                let id = Self::new_id(g);
                g.stores.get_mut(fs).unwrap().dirs.insert(dir.clone(), id);
            }
            dir = parent_of(&dir).to_string();
        }
    }

    pub fn list_remotes_sync(&self) -> Vec<String> {
        self.inner.lock().unwrap().config.keys().cloned().collect()
    }

    pub fn store(&self, fs: &str) -> MockStore {
        self.inner.lock().unwrap().stores.get(fs).cloned().unwrap_or_default()
    }

    /// Make the remote's clock run `secs` ahead (clock-skew tests).
    pub fn set_clock_offset(&self, secs: i64) {
        self.inner.lock().unwrap().clock_offset_secs = secs;
    }

    // ---- call log and faults ----

    pub fn calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.inner.lock().unwrap().calls.clear();
    }

    pub fn count(&self, method: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.as_str() == method)
            .count()
    }

    /// Number of calls that change remote data.
    pub fn remote_write_calls(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| REMOTE_WRITE_CALLS.contains(&c.as_str()))
            .count()
    }

    /// Make the next call to `method` fail with `kind`.
    pub fn fail_next(&self, method: &str, kind: ErrorKind, times: usize) {
        let mut g = self.inner.lock().unwrap();
        let q = g.faults.entry(method.to_string()).or_default();
        for _ in 0..times {
            let status = match kind {
                ErrorKind::RateLimited => 429,
                ErrorKind::NotFound => 404,
                ErrorKind::BadRequest => 400,
                _ => 500,
            };
            q.push_back(RcloneError {
                kind,
                path: method.into(),
                status: Some(status),
                message: format!("injected {kind:?}"),
            });
        }
    }

    /// Make every stat and transfer take `ms`, so concurrent operations
    /// really interleave (as they do against a real cloud).
    pub fn set_latency(&self, ms: u64) {
        self.inner.lock().unwrap().latency_ms = ms;
    }

    async fn pause(&self) {
        let ms = self.inner.lock().unwrap().latency_ms;
        if ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
    }

    fn enter(&self, method: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.calls.push(method.to_string());
        if let Some(q) = g.faults.get_mut(method) {
            if let Some(e) = q.pop_front() {
                return Err(e);
            }
        }
        Ok(())
    }

    // ---- simulated OAuth ----

    /// The pending browser sign-in, if one is waiting.
    pub fn oauth_pending(&self) -> Option<String> {
        let g = self.inner.lock().unwrap();
        g.oauth
            .as_ref()
            .filter(|(_, o)| *o == OAuthOutcome::Pending)
            .map(|(u, _)| u.clone())
    }

    /// Finish the pending browser sign-in as the user would.
    pub fn complete_oauth(&self, approve: bool) {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, o)) = g.oauth.as_mut() {
            *o = if approve {
                OAuthOutcome::Approved
            } else {
                OAuthOutcome::Denied
            };
        }
    }

    pub fn bwlimit_value(&self) -> String {
        self.inner.lock().unwrap().bwlimit.clone()
    }

    fn store_for(&self, fs: &str) -> Result<()> {
        let g = self.inner.lock().unwrap();
        let name = fs.trim_end_matches(':');
        if g.stores.contains_key(fs) || g.config.contains_key(name) {
            Ok(())
        } else {
            Err(RcloneError {
                kind: ErrorKind::BadRequest,
                path: String::new(),
                status: Some(400),
                message: format!("didn't find section in config file (\"{name}\")"),
            })
        }
    }

    fn entry_for(&self, path: &str, obj: Option<&MockObject>, dir_id: Option<&str>, opts: &ListOpts) -> RemoteEntry {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        match obj {
            Some(o) => RemoteEntry {
                path: path.to_string(),
                name,
                size: o.data.len() as i64,
                mod_time: if opts.no_mod_time { None } else { Some(o.mod_time) },
                is_dir: false,
                id: self.with_ids.then(|| o.id.clone()),
                hashes: opts.hash_types.iter().any(|h| h == "md5").then(|| {
                    let mut m = BTreeMap::new();
                    m.insert("md5".to_string(), md5_hex(&o.data));
                    m
                }),
                mime_type: None,
            },
            None => RemoteEntry {
                path: path.to_string(),
                name,
                size: -1,
                mod_time: None,
                is_dir: true,
                id: if self.with_ids {
                    dir_id.map(str::to_string)
                } else {
                    None
                },
                hashes: None,
                mime_type: None,
            },
        }
    }

    fn read_any(&self, fs: &str, remote: &str, method: &str) -> Result<(Vec<u8>, DateTime<Utc>)> {
        if is_local(fs) {
            let p = Path::new(fs).join(remote);
            let before = std::fs::metadata(&p).map_err(|_| not_found(method, "object not found"))?;
            let data = std::fs::read(&p).map_err(|e| RcloneError::new(ErrorKind::Other, method, e.to_string()))?;
            let after = std::fs::metadata(&p).map_err(|_| not_found(method, "object not found"))?;
            if before.len() != after.len()
                || before.modified().ok() != after.modified().ok()
                || data.len() as u64 != after.len()
            {
                return Err(RcloneError::new(
                    ErrorKind::SourceChanged,
                    method,
                    "can't copy - source file is being updated",
                ));
            }
            let t: DateTime<Utc> = after
                .modified()
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now());
            Ok((data, t))
        } else {
            self.store_for(fs)?;
            let g = self.inner.lock().unwrap();
            let o = g
                .stores
                .get(fs)
                .and_then(|s| s.files.get(remote.trim_matches('/')))
                .ok_or_else(|| not_found(method, "object not found"))?;
            Ok(((*o.data).clone(), o.mod_time))
        }
    }

    fn write_any(&self, fs: &str, remote: &str, data: &[u8], t: DateTime<Utc>, method: &str) -> Result<()> {
        if is_local(fs) {
            let p: PathBuf = Path::new(fs).join(remote);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| RcloneError::new(ErrorKind::Other, method, e.to_string()))?;
            }
            // Write to a temp name and rename, as rclone does (--inplace off).
            let tmp = p.with_extension("partial-mock");
            std::fs::write(&tmp, data).map_err(|e| RcloneError::new(ErrorKind::Other, method, e.to_string()))?;
            std::fs::rename(&tmp, &p).map_err(|e| RcloneError::new(ErrorKind::Other, method, e.to_string()))?;
            Ok(())
        } else {
            self.store_for(fs)?;
            let mut g = self.inner.lock().unwrap();
            Self::put_locked(&mut g, fs, remote, data, t);
            Ok(())
        }
    }
}

#[async_trait]
impl RcloneApi for MockRclone {
    async fn version(&self) -> Result<VersionInfo> {
        self.enter("core/version")?;
        let (a, b, c) = parse_version(&self.version).unwrap_or((0, 0, 0));
        Ok(VersionInfo {
            version: self.version.clone(),
            decomposed: vec![a, b, c],
            os: "mock".into(),
            arch: "mock".into(),
            go_version: String::new(),
        })
    }

    async fn bwlimit(&self, rate: &str) -> Result<()> {
        self.enter("core/bwlimit")?;
        self.inner.lock().unwrap().bwlimit = rate.to_string();
        Ok(())
    }

    async fn stats(&self, _group: Option<&str>) -> Result<Stats> {
        self.enter("core/stats")?;
        Ok(Stats::default())
    }

    async fn list_remotes(&self) -> Result<Vec<String>> {
        self.enter("config/listremotes")?;
        Ok(self.inner.lock().unwrap().config.keys().cloned().collect())
    }

    async fn config_get(&self, name: &str) -> Result<Map<String, Value>> {
        self.enter("config/get")?;
        Ok(self.inner.lock().unwrap().config.get(name).cloned().unwrap_or_default())
    }

    async fn config_create(
        &self,
        name: &str,
        kind: &str,
        parameters: Map<String, Value>,
        opts: &ConfigOpts,
    ) -> Result<ConfigStep> {
        self.enter("config/create")?;
        {
            let mut g = self.inner.lock().unwrap();
            let mut m = parameters.clone();
            m.insert("type".into(), Value::String(kind.into()));
            g.config.insert(name.to_string(), m);
            g.stores.entry(format!("{name}:")).or_default();
        }
        if matches!(kind, "drive" | "onedrive") && opts.non_interactive {
            return Ok(ConfigStep {
                state: "*oauth-islocal,oauth,".into(),
                option: Some(ConfigQuestion {
                    name: "config_is_local".into(),
                    help: "Use web browser to automatically authenticate rclone with remote?".into(),
                    default: Value::Bool(true),
                    kind: "bool".into(),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        Ok(ConfigStep::default())
    }

    async fn config_update(&self, name: &str, parameters: Map<String, Value>, opts: &ConfigOpts) -> Result<ConfigStep> {
        self.enter("config/update")?;
        {
            let mut g = self.inner.lock().unwrap();
            let Some(m) = g.config.get_mut(name) else {
                return Err(RcloneError::new(
                    ErrorKind::BadRequest,
                    "config/update",
                    "remote not found",
                ));
            };
            for (k, v) in parameters {
                m.insert(k, v);
            }
        }
        if let Some((state, result)) = &opts.continue_with {
            if state.starts_with("*oauth-islocal") && result == "true" {
                // Start the "browser" flow and block until the test completes it,
                // as rclone's config/update does.
                {
                    let mut g = self.inner.lock().unwrap();
                    g.oauth = Some((
                        format!("http://127.0.0.1:53682/auth?state=mock-{name}"),
                        OAuthOutcome::Pending,
                    ));
                }
                loop {
                    let outcome = self.inner.lock().unwrap().oauth.as_ref().map(|(_, o)| *o);
                    match outcome {
                        Some(OAuthOutcome::Pending) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                        Some(OAuthOutcome::Approved) => {
                            let mut g = self.inner.lock().unwrap();
                            g.oauth = None;
                            if let Some(m) = g.config.get_mut(name) {
                                m.insert("token".into(), json!("{\"access_token\":\"mock\"}"));
                            }
                            return Ok(ConfigStep::default());
                        }
                        Some(OAuthOutcome::Denied) | None => {
                            self.inner.lock().unwrap().oauth = None;
                            return Err(RcloneError::new(
                                ErrorKind::Other,
                                "config/update",
                                "config failed to refresh token: oauth authentication was cancelled",
                            ));
                        }
                    }
                }
            }
        }
        Ok(ConfigStep::default())
    }

    async fn config_delete(&self, name: &str) -> Result<()> {
        self.enter("config/delete")?;
        let mut g = self.inner.lock().unwrap();
        g.config.remove(name);
        Ok(())
    }

    async fn config_providers(&self) -> Result<Vec<ProviderInfo>> {
        self.enter("config/providers")?;
        let opt = |name: &str, required: bool, advanced: bool, sensitive: bool| ProviderOption {
            name: name.into(),
            help: format!("{name} option"),
            default: Value::String(String::new()),
            required,
            advanced,
            is_password: false,
            sensitive,
            hide: 0,
            kind: "string".into(),
            examples: None,
        };
        let p = |name: &str, desc: &str, options: Vec<ProviderOption>| ProviderInfo {
            name: name.into(),
            description: desc.into(),
            prefix: name.into(),
            options,
            hide: false,
        };
        Ok(vec![
            p(
                "drive",
                "Google Drive",
                vec![
                    opt("client_id", false, false, true),
                    opt("use_trash", false, true, false),
                ],
            ),
            p(
                "onedrive",
                "Microsoft OneDrive",
                vec![
                    opt("client_id", false, false, true),
                    opt("hard_delete", false, true, false),
                ],
            ),
            p(
                "drime",
                "Drime",
                vec![
                    opt("access_token", false, false, true),
                    opt("hard_delete", false, true, false),
                ],
            ),
            p("local", "Local Disk", vec![]),
            p("memory", "In memory object storage system.", vec![]),
            p(
                "alias",
                "Alias for an existing remote",
                vec![opt("remote", true, false, false)],
            ),
            p(
                "sftp",
                "SSH/SFTP",
                vec![
                    opt("host", true, false, false),
                    opt("user", false, false, false),
                    opt("port", false, true, false),
                ],
            ),
        ])
    }

    async fn config_paths(&self) -> Result<ConfigPaths> {
        self.enter("config/paths")?;
        Ok(ConfigPaths {
            config: "/mock/rclone.conf".into(),
            ..Default::default()
        })
    }

    async fn oauth_status(&self) -> Result<OAuthStatus> {
        self.enter("config/oauthstatus")?;
        let g = self.inner.lock().unwrap();
        Ok(match g.oauth.as_ref().filter(|(_, o)| *o == OAuthOutcome::Pending) {
            Some((u, _)) => OAuthStatus {
                status: "running".into(),
                auth_url: Some(u.clone()),
            },
            None => OAuthStatus {
                status: "stopped".into(),
                auth_url: None,
            },
        })
    }

    async fn oauth_stop(&self) -> Result<()> {
        self.enter("config/oauthstop")?;
        let mut g = self.inner.lock().unwrap();
        match g.oauth.as_mut() {
            Some((_, o)) if *o == OAuthOutcome::Pending => {
                *o = OAuthOutcome::Denied;
                Ok(())
            }
            _ => Err(RcloneError::new(
                ErrorKind::Other,
                "config/oauthstop",
                "no OAuth authentication in progress",
            )),
        }
    }

    async fn list(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Vec<RemoteEntry>> {
        self.enter("operations/list")?;
        if is_local(fs) {
            return Err(RcloneError::new(
                ErrorKind::BadRequest,
                "operations/list",
                "mock lists remotes only",
            ));
        }
        self.store_for(fs)?;
        let g = self.inner.lock().unwrap();
        let empty = MockStore::default();
        let s = g.stores.get(fs).unwrap_or(&empty);
        let base = remote.trim_matches('/');
        let prefix = if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        };
        // Range queries over the sorted maps: listing one folder costs its
        // size, not the whole store (the 100k-item index test relies on it).
        // Parents are always explicit (put/mkdir/rename create them).
        fn under<V>(m: &BTreeMap<String, V>, prefix: &str) -> bool {
            m.range(prefix.to_string()..)
                .next()
                .map(|(k, _)| k.starts_with(prefix))
                .unwrap_or(false)
        }
        if !base.is_empty() && !s.dirs.contains_key(base) && !under(&s.files, &prefix) && !under(&s.dirs, &prefix) {
            return Err(not_found("operations/list", "directory not found"));
        }
        let direct = |p: &str| -> bool { p.len() > prefix.len() && (opts.recurse || !p[prefix.len()..].contains('/')) };
        let mut dir_entries = Vec::new();
        let mut file_entries = Vec::new();
        if !opts.files_only {
            for (d, id) in s
                .dirs
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
            {
                if direct(d) {
                    dir_entries.push((d.clone(), id.clone()));
                }
            }
        }
        if !opts.dirs_only {
            for (k, o) in s
                .files
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
            {
                if direct(k) {
                    file_entries.push((k.clone(), o.clone()));
                }
            }
        }
        drop(g);
        let mut out = Vec::with_capacity(dir_entries.len() + file_entries.len());
        for (d, id) in dir_entries {
            out.push(self.entry_for(&d, None, Some(&id), opts));
        }
        for (k, o) in file_entries {
            out.push(self.entry_for(&k, Some(&o), None, opts));
        }
        Ok(out)
    }

    async fn stat(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Option<RemoteEntry>> {
        self.enter("operations/stat")?;
        self.pause().await;
        if is_local(fs) {
            let p = Path::new(fs).join(remote);
            return Ok(std::fs::metadata(&p).ok().map(|m| RemoteEntry {
                path: remote.to_string(),
                name: remote.rsplit('/').next().unwrap_or(remote).to_string(),
                size: if m.is_dir() { -1 } else { m.len() as i64 },
                mod_time: m.modified().ok().map(DateTime::<Utc>::from),
                is_dir: m.is_dir(),
                id: None,
                hashes: None,
                mime_type: None,
            }));
        }
        self.store_for(fs)?;
        let g = self.inner.lock().unwrap();
        let p = remote.trim_matches('/');
        let Some(s) = g.stores.get(fs) else { return Ok(None) };
        if let Some(o) = s.files.get(p) {
            let o = o.clone();
            drop(g);
            return Ok(Some(self.entry_for(p, Some(&o), None, opts)));
        }
        let pre = format!("{p}/");
        let implicit = s
            .files
            .range(pre.clone()..)
            .next()
            .map(|(k, _)| k.starts_with(&pre))
            .unwrap_or(false);
        if let Some(id) = s.dirs.get(p).cloned() {
            drop(g);
            return Ok(Some(self.entry_for(p, None, Some(&id), opts)));
        }
        if implicit {
            drop(g);
            return Ok(Some(self.entry_for(p, None, None, opts)));
        }
        Ok(None)
    }

    async fn fsinfo(&self, fs: &str) -> Result<FsInfo> {
        self.enter("operations/fsinfo")?;
        let mut features = BTreeMap::new();
        features.insert("CanHaveEmptyDirectories".to_string(), true);
        features.insert("Move".to_string(), true);
        features.insert("DirMove".to_string(), true);
        features.insert("Purge".to_string(), true);
        Ok(FsInfo {
            name: fs.trim_end_matches(':').to_string(),
            hashes: vec!["md5".into()],
            precision_ns: 1,
            features,
        })
    }

    async fn about(&self, fs: &str) -> Result<About> {
        self.enter("operations/about")?;
        let used: i64 = self.store(fs).files.values().map(|o| o.data.len() as i64).sum();
        Ok(About {
            total: Some(1 << 40),
            used: Some(used),
            free: Some((1 << 40) - used),
            trashed: None,
        })
    }

    async fn hashsum_file(&self, fs: &str, remote: &str, hash_type: &str) -> Result<Option<String>> {
        self.enter("operations/hashsumfile")?;
        if hash_type != "md5" {
            return Ok(None);
        }
        let (data, _) = self.read_any(fs, remote, "operations/hashsumfile")?;
        Ok(Some(md5_hex(&data)))
    }

    async fn copyfile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.enter("operations/copyfile")?;
        self.pause().await;
        let (data, t) = self.read_any(src_fs, src_remote, "operations/copyfile")?;
        self.write_any(dst_fs, dst_remote, &data, t, "operations/copyfile")
    }

    async fn movefile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        self.enter("operations/movefile")?;
        self.pause().await;
        if src_fs == dst_fs && !is_local(src_fs) {
            if !self.exists(src_fs, src_remote) {
                return Err(not_found("operations/movefile", "object not found"));
            }
            let mut g = self.inner.lock().unwrap();
            Self::rename_locked(&mut g, src_fs, src_remote, dst_remote);
            return Ok(());
        }
        let (data, t) = self.read_any(src_fs, src_remote, "operations/movefile")?;
        self.write_any(dst_fs, dst_remote, &data, t, "operations/movefile")?;
        if is_local(src_fs) {
            let _ = std::fs::remove_file(Path::new(src_fs).join(src_remote));
        } else {
            self.remove(src_fs, src_remote);
        }
        Ok(())
    }

    async fn deletefile(&self, fs: &str, remote: &str) -> Result<()> {
        self.enter("operations/deletefile")?;
        self.pause().await;
        self.store_for(fs)?;
        let mut g = self.inner.lock().unwrap();
        let removed = g
            .stores
            .get_mut(fs)
            .and_then(|s| s.files.remove(remote.trim_matches('/')));
        match removed {
            Some(_) => Ok(()),
            None => Err(not_found("operations/deletefile", "object not found")),
        }
    }

    async fn mkdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.enter("operations/mkdir")?;
        self.store_for(fs)?;
        let mut g = self.inner.lock().unwrap();
        let mut d = remote.trim_matches('/').to_string();
        while !d.is_empty() {
            if !g.stores.get(fs).map(|s| s.dirs.contains_key(&d)).unwrap_or(false) {
                let id = Self::new_id(&mut g);
                g.stores.entry(fs.into()).or_default().dirs.insert(d.clone(), id);
            }
            d = parent_of(&d).to_string();
        }
        Ok(())
    }

    async fn rmdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.enter("operations/rmdir")?;
        let p = remote.trim_matches('/').to_string();
        let mut g = self.inner.lock().unwrap();
        let Some(s) = g.stores.get_mut(fs) else {
            return Err(not_found("operations/rmdir", "directory not found"));
        };
        let prefix = format!("{p}/");
        if s.files.keys().any(|k| k.starts_with(&prefix)) || s.dirs.keys().any(|k| k.starts_with(&prefix)) {
            return Err(RcloneError::new(
                ErrorKind::Other,
                "operations/rmdir",
                "directory not empty",
            ));
        }
        match s.dirs.remove(&p) {
            Some(_) => Ok(()),
            None => Err(not_found("operations/rmdir", "directory not found")),
        }
    }

    async fn purge(&self, fs: &str, remote: &str) -> Result<()> {
        self.enter("operations/purge")?;
        if !self.exists(fs, remote)
            && !self
                .store(fs)
                .files
                .keys()
                .any(|k| k.starts_with(&format!("{}/", remote.trim_matches('/'))))
        {
            return Err(not_found("operations/purge", "directory not found"));
        }
        self.remove(fs, remote);
        Ok(())
    }

    async fn movedir(&self, fs: &str, src: &str, dst: &str) -> Result<()> {
        self.enter("sync/move")?;
        let s = self.store(fs);
        let prefix = format!("{}/", src.trim_matches('/'));
        if !s.dirs.contains_key(src.trim_matches('/')) && !s.files.keys().any(|k| k.starts_with(&prefix)) {
            return Err(not_found("sync/move", "directory not found"));
        }
        let mut g = self.inner.lock().unwrap();
        Self::rename_locked(&mut g, fs, src, dst);
        Ok(())
    }
}

/// Convenience: join a drive root and a relative path the way the engine does.
pub fn remote_path(root: &str, rel: &str) -> String {
    join_remote(root, rel)
}
