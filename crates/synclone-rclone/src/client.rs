//! HTTP client for a running `rclone rcd`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::api::RcloneApi;
use crate::error::{ErrorKind, RcloneError, Result};
use crate::types::*;

/// Where an rc server listens and the login it wants.
#[derive(Clone)]
pub struct Endpoint {
    /// e.g. `http://127.0.0.1:41234`
    pub url: String,
    pub user: String,
    pub pass: String,
}

impl std::fmt::Debug for Endpoint {
    // Never print the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Endpoint({} as {})", self.url, self.user)
    }
}

/// Typed rc client. Long-running calls are sent with `_async=true` and
/// followed through `job/status`, so no HTTP timeout ever cuts off a
/// multi-gigabyte transfer (see Nimbus issue #180 in PLAN.md §1.6).
pub struct RcClient {
    http: reqwest::Client,
    endpoint: Endpoint,
    group_seq: AtomicU64,
    /// Upper bound on how long one synchronous rc request may take.
    pub request_timeout: Duration,
}

impl RcClient {
    pub fn new(endpoint: Endpoint) -> Self {
        let http = reqwest::Client::builder()
            // The rc server is on 127.0.0.1: never route it through a proxy.
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("building an HTTP client cannot fail with these options");
        Self {
            http,
            endpoint,
            group_seq: AtomicU64::new(0),
            request_timeout: Duration::from_secs(120),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// One synchronous rc call.
    pub async fn call(&self, path: &str, params: Value) -> Result<Value> {
        let url = format!("{}/{}", self.endpoint.url.trim_end_matches('/'), path);
        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.endpoint.user, Some(&self.endpoint.pass))
            .timeout(self.request_timeout)
            .json(&params)
            .send()
            .await
            .map_err(|e| RcloneError::new(ErrorKind::Network, path, e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| RcloneError::new(ErrorKind::Network, path, e.to_string()))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or(text);
            return Err(RcloneError::from_response(path, status, &message));
        }
        if text.trim().is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        serde_json::from_str(&text).map_err(|e| RcloneError::new(ErrorKind::Other, path, format!("bad JSON: {e}")))
    }

    /// Start `path` as an async job and wait for it to finish.
    pub async fn call_async(&self, path: &str, mut params: Value) -> Result<Value> {
        let group = format!("synclone/{}", self.group_seq.fetch_add(1, Ordering::Relaxed));
        if let Value::Object(m) = &mut params {
            m.insert("_async".into(), Value::Bool(true));
            m.insert("_group".into(), Value::String(group));
        }
        let started = self.call(path, params).await?;
        let jobid = started
            .get("jobid")
            .and_then(|j| j.as_i64())
            .ok_or_else(|| RcloneError::new(ErrorKind::Other, path, "no jobid in async response"))?;
        let mut delay = Duration::from_millis(20);
        loop {
            let st = self.job_status(jobid).await?;
            if st.finished {
                if st.success {
                    return Ok(st.output);
                }
                // job/status carries no HTTP status; classify by message.
                let mut err = RcloneError::from_response(path, 500, &st.error);
                err.status = None;
                return Err(err);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    pub async fn job_status(&self, jobid: i64) -> Result<JobStatus> {
        let v = self.call("job/status", json!({ "jobid": jobid })).await?;
        serde_json::from_value(v).map_err(|e| RcloneError::new(ErrorKind::Other, "job/status", e.to_string()))
    }

    pub async fn job_stop(&self, jobid: i64) -> Result<()> {
        self.call("job/stop", json!({ "jobid": jobid })).await.map(|_| ())
    }

    /// Ask rclone to exit.
    pub async fn quit(&self) -> Result<()> {
        self.call("core/quit", json!({})).await.map(|_| ())
    }
}

fn decode<T: serde::de::DeserializeOwned>(path: &str, v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| RcloneError::new(ErrorKind::Other, path, format!("unexpected response: {e}")))
}

#[async_trait]
impl RcloneApi for RcClient {
    async fn version(&self) -> Result<VersionInfo> {
        decode("core/version", self.call("core/version", json!({})).await?)
    }

    async fn bwlimit(&self, rate: &str) -> Result<()> {
        self.call("core/bwlimit", json!({ "rate": rate })).await.map(|_| ())
    }

    async fn stats(&self, group: Option<&str>) -> Result<Stats> {
        let mut p = json!({ "short": true });
        if let Some(g) = group {
            p["group"] = Value::String(g.into());
        }
        decode("core/stats", self.call("core/stats", p).await?)
    }

    async fn list_remotes(&self) -> Result<Vec<String>> {
        let v = self.call("config/listremotes", json!({})).await?;
        Ok(v.get("remotes")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default())
    }

    async fn config_get(&self, name: &str) -> Result<Map<String, Value>> {
        match self.call("config/get", json!({ "name": name })).await? {
            Value::Object(m) => Ok(m),
            _ => Ok(Map::new()),
        }
    }

    async fn config_create(
        &self,
        name: &str,
        kind: &str,
        parameters: Map<String, Value>,
        opts: &ConfigOpts,
    ) -> Result<ConfigStep> {
        let p = json!({ "name": name, "type": kind, "parameters": parameters, "opt": opts.to_json() });
        // OAuth questions block until the browser flow finishes: run async.
        decode("config/create", self.call_async("config/create", p).await?)
    }

    async fn config_update(&self, name: &str, parameters: Map<String, Value>, opts: &ConfigOpts) -> Result<ConfigStep> {
        let p = json!({ "name": name, "parameters": parameters, "opt": opts.to_json() });
        decode("config/update", self.call_async("config/update", p).await?)
    }

    async fn config_delete(&self, name: &str) -> Result<()> {
        self.call("config/delete", json!({ "name": name })).await.map(|_| ())
    }

    async fn config_providers(&self) -> Result<Vec<ProviderInfo>> {
        let v = self.call("config/providers", json!({})).await?;
        decode(
            "config/providers",
            v.get("providers").cloned().unwrap_or(Value::Array(vec![])),
        )
    }

    async fn config_paths(&self) -> Result<ConfigPaths> {
        decode("config/paths", self.call("config/paths", json!({})).await?)
    }

    async fn oauth_status(&self) -> Result<OAuthStatus> {
        decode("config/oauthstatus", self.call("config/oauthstatus", json!({})).await?)
    }

    async fn oauth_stop(&self) -> Result<()> {
        self.call("config/oauthstop", json!({})).await.map(|_| ())
    }

    async fn list(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Vec<RemoteEntry>> {
        let p = json!({ "fs": fs, "remote": remote, "opt": opts.to_json() });
        let v = if opts.recurse {
            self.call_async("operations/list", p).await?
        } else {
            self.call("operations/list", p).await?
        };
        decode(
            "operations/list",
            v.get("list").cloned().unwrap_or(Value::Array(vec![])),
        )
    }

    async fn stat(&self, fs: &str, remote: &str, opts: &ListOpts) -> Result<Option<RemoteEntry>> {
        let p = json!({ "fs": fs, "remote": remote, "opt": opts.to_json() });
        match self.call("operations/stat", p).await {
            Ok(v) => match v.get("item") {
                Some(Value::Null) | None => Ok(None),
                Some(item) => decode("operations/stat", item.clone()).map(Some),
            },
            // stat of a path under a missing directory reports "directory not found".
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn fsinfo(&self, fs: &str) -> Result<FsInfo> {
        decode(
            "operations/fsinfo",
            self.call("operations/fsinfo", json!({ "fs": fs })).await?,
        )
    }

    async fn about(&self, fs: &str) -> Result<About> {
        decode(
            "operations/about",
            self.call("operations/about", json!({ "fs": fs })).await?,
        )
    }

    async fn hashsum_file(&self, fs: &str, remote: &str, hash_type: &str) -> Result<Option<String>> {
        let v = self
            .call_async(
                "operations/hashsumfile",
                json!({ "fs": fs, "remote": remote, "hashType": hash_type }),
            )
            .await?;
        Ok(v.get("hash")
            .and_then(|h| h.as_str())
            .filter(|h| !h.is_empty())
            .map(|h| h.to_ascii_lowercase()))
    }

    async fn copyfile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        let p = json!({ "srcFs": src_fs, "srcRemote": src_remote, "dstFs": dst_fs, "dstRemote": dst_remote });
        self.call_async("operations/copyfile", p).await.map(|_| ())
    }

    async fn movefile(&self, src_fs: &str, src_remote: &str, dst_fs: &str, dst_remote: &str) -> Result<()> {
        let p = json!({ "srcFs": src_fs, "srcRemote": src_remote, "dstFs": dst_fs, "dstRemote": dst_remote });
        self.call_async("operations/movefile", p).await.map(|_| ())
    }

    async fn deletefile(&self, fs: &str, remote: &str) -> Result<()> {
        self.call_async("operations/deletefile", json!({ "fs": fs, "remote": remote }))
            .await
            .map(|_| ())
    }

    async fn mkdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.call("operations/mkdir", json!({ "fs": fs, "remote": remote }))
            .await
            .map(|_| ())
    }

    async fn rmdir(&self, fs: &str, remote: &str) -> Result<()> {
        self.call("operations/rmdir", json!({ "fs": fs, "remote": remote }))
            .await
            .map(|_| ())
    }

    async fn purge(&self, fs: &str, remote: &str) -> Result<()> {
        self.call_async("operations/purge", json!({ "fs": fs, "remote": remote }))
            .await
            .map(|_| ())
    }
}
