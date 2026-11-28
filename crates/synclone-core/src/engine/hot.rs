//! Hot folders (D-031): folders the user has open in Explorer, browsed,
//! or worked in recently are listed again every 20–30 s, so cloud changes
//! there show up quickly. Everything else waits for the drive's full poll.
//!
//! A single folder listing cannot tell a file moved to another folder from
//! a file deleted. So a hot listing applies additions, changes and renames
//! inside the folder at once, and removes vanished placeholders (they
//! reappear wherever they went). A vanished item with local content asks
//! for a full poll instead of being treated as a cloud delete.
//!
//! Rate limits: when a listing is throttled (after rclone's own retries),
//! the drive's hot interval doubles, up to `hot_max_backoff_secs`, and
//! halves back towards normal after each clean round.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use super::index::{is_rate_limited, DirTask, PollReport};
use super::Engine;
use crate::types::*;

/// How often and how widely to poll hot folders on one kind of service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotProfile {
    pub interval_secs: i64,
    pub max_folders: usize,
}

/// Defaults per rclone backend, from each service's published limits
/// (D-031). A folder listing costs 100 quota units on Google Drive and
/// 2 resource units on OneDrive; Drime publishes no numbers, so it gets
/// the cautious default.
pub fn hot_profile(backend: &str) -> HotProfile {
    match backend {
        "drive" | "onedrive" => HotProfile {
            interval_secs: 20,
            max_folders: 8,
        },
        _ => HotProfile {
            interval_secs: 30,
            max_folders: 5,
        },
    }
}

/// Wrapper backends whose `remote` option names the remote they sit on.
const WRAPPERS: &[&str] = &["alias", "crypt", "chunker", "compress", "hasher"];

#[derive(Debug, Default)]
pub(crate) struct HotState {
    /// Folder → when it was last touched (ms).
    folders: HashMap<RelPath, i64>,
    next_due: i64,
    /// Current interval, including any rate-limit backoff (0: not set yet).
    interval_ms: i64,
    /// The service under any alias/crypt wrappers, once looked up.
    backend: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotReport {
    pub polled: PollReport,
    pub folders: usize,
    /// A vanished item had local content: a full poll was requested.
    pub full_poll_requested: bool,
    pub rate_limited: bool,
}

impl Engine {
    /// The user is looking at or working in `dir` (a folder).
    pub fn touch_folder(&self, d: DriveId, dir: &RelPath) {
        let now = self.now();
        let mut g = self.hot.lock().unwrap();
        g.entry(d).or_default().folders.insert(dir.clone(), now);
    }

    /// The folder holding `p`.
    pub(crate) fn touch_parent(&self, d: DriveId, p: &RelPath) {
        self.touch_folder(d, &p.parent().unwrap_or_default());
    }

    /// Hot folders of a drive right now, most recently touched first (tests).
    pub fn hot_folders(&self, d: DriveId) -> Vec<RelPath> {
        let ttl = self.settings.hot_ttl_secs * 1000;
        let now = self.now();
        let g = self.hot.lock().unwrap();
        let mut v: Vec<(RelPath, i64)> = g
            .get(&d)
            .map(|s| {
                s.folders
                    .iter()
                    .filter(|(_, t)| now - **t <= ttl)
                    .map(|(p, t)| (p.clone(), *t))
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().map(|(p, _)| p).collect()
    }

    /// The drive's current hot interval in ms (with backoff), for tests and status.
    pub fn hot_interval_ms(&self, d: DriveId) -> i64 {
        self.hot.lock().unwrap().get(&d).map(|s| s.interval_ms).unwrap_or(0)
    }

    /// A full poll of `d` was throttled: slow the hot polling down too
    /// (both draw on the same quota).
    pub fn note_rate_limited(&self, d: DriveId) {
        let max = self.settings.hot_max_backoff_secs * 1000;
        let now = self.now();
        let mut g = self.hot.lock().unwrap();
        let s = g.entry(d).or_default();
        if s.interval_ms > 0 {
            s.interval_ms = (s.interval_ms * 2).min(max);
            s.next_due = now + s.interval_ms;
        }
    }

    /// The service a drive really talks to, under alias/crypt wrappers.
    async fn base_backend(&self, d: &Drive) -> String {
        if let Some(b) = self.hot.lock().unwrap().get(&d.id).and_then(|s| s.backend.clone()) {
            return b;
        }
        let mut kind = d.provider_type.clone();
        let mut name = d.remote_name.clone();
        for _ in 0..4 {
            if !WRAPPERS.contains(&kind.as_str()) {
                break;
            }
            let Ok(cfg) = self.api.config_get(&name).await else {
                break;
            };
            let target = cfg.get("remote").and_then(Value::as_str).unwrap_or("");
            let Some((next, _)) = target.split_once(':') else { break };
            // A local path (C:\...) is not a remote.
            if next.is_empty() || next.len() == 1 || next.contains(['/', '\\', ',']) {
                break;
            }
            let Ok(next_cfg) = self.api.config_get(next).await else {
                break;
            };
            name = next.to_string();
            kind = next_cfg.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        }
        self.hot.lock().unwrap().entry(d.id).or_default().backend = Some(kind.clone());
        kind
    }

    /// List the drive's hot folders if they are due. Cheap to call often.
    pub async fn poll_hot(&self, drive_id: DriveId) -> Result<HotReport> {
        let mut report = HotReport::default();
        let d = self.drive(drive_id)?;
        if d.state != DriveState::Connected || d.paused {
            return Ok(report);
        }
        let backend = self.base_backend(&d).await;
        let profile = hot_profile(&backend);
        let base_ms = self.settings.hot_poll_secs.unwrap_or(profile.interval_secs) * 1000;
        if base_ms <= 0 {
            return Ok(report);
        }
        let now = self.now();
        let open = self.provider.open_folders(&d);
        let folders = {
            let mut g = self.hot.lock().unwrap();
            let s = g.entry(d.id).or_default();
            if s.interval_ms == 0 {
                s.interval_ms = base_ms;
            }
            if now < s.next_due {
                return Ok(report);
            }
            for f in open {
                s.folders.insert(f, now);
            }
            let ttl = self.settings.hot_ttl_secs * 1000;
            s.folders.retain(|_, t| now - *t <= ttl);
            let mut v: Vec<(RelPath, i64)> = s.folders.iter().map(|(p, t)| (p.clone(), *t)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            v.truncate(self.settings.hot_max_folders.unwrap_or(profile.max_folders));
            v.into_iter().map(|(p, _)| p).collect::<Vec<_>>()
        };
        for f in &folders {
            let known = f.is_root() || self.db.item(d.id, f)?.map(|i| i.is_dir).unwrap_or(false);
            if !known {
                self.forget_hot(d.id, f);
                continue;
            }
            match self.hot_list(&d, f, &mut report).await {
                Ok(()) => report.folders += 1,
                Err(e) if is_rate_limited(&e) => {
                    report.rate_limited = true;
                    let max = self.settings.hot_max_backoff_secs * 1000;
                    let mut g = self.hot.lock().unwrap();
                    let s = g.entry(d.id).or_default();
                    s.interval_ms = (s.interval_ms * 2).min(max).max(base_ms);
                    s.next_due = now + s.interval_ms;
                    tracing::info!(
                        "{}: the service is limiting requests; checking open folders every {} s for now",
                        d.display_name,
                        s.interval_ms / 1000
                    );
                    return Ok(report);
                }
                Err(e) => {
                    self.schedule_hot(d.id, now, base_ms);
                    return Err(e);
                }
            }
        }
        // A clean round: recover from any backoff.
        {
            let mut g = self.hot.lock().unwrap();
            let s = g.entry(d.id).or_default();
            s.interval_ms = (s.interval_ms / 2).max(base_ms);
            s.next_due = now + s.interval_ms;
        }
        if report.polled.changes() > 0 {
            self.db.log_activity(
                Some(d.id),
                None,
                "poll",
                &format!(
                    "{}: {} new, {} changed, {} removed, {} renamed in the cloud (open folders)",
                    d.display_name,
                    report.polled.added,
                    report.polled.modified,
                    report.polled.deleted,
                    report.polled.renamed.len()
                ),
                false,
            );
        }
        Ok(report)
    }

    fn schedule_hot(&self, d: DriveId, now: i64, base_ms: i64) {
        let mut g = self.hot.lock().unwrap();
        let s = g.entry(d).or_default();
        s.interval_ms = s.interval_ms.max(base_ms);
        s.next_due = now + s.interval_ms;
    }

    fn forget_hot(&self, d: DriveId, dir: &RelPath) {
        if let Some(s) = self.hot.lock().unwrap().get_mut(&d) {
            s.folders.remove(dir);
        }
    }

    /// One hot folder: list and diff it, then apply only what one listing
    /// can be sure of (see the module docs).
    async fn hot_list(&self, d: &Drive, dir: &RelPath, report: &mut HotReport) -> Result<()> {
        let pinned = self.is_pinned_path(d.id, dir)?;
        let mut missing = Vec::new();
        let listed_before = report.polled.listed_dirs;
        self.list_and_diff(
            d,
            &DirTask {
                path: dir.clone(),
                pinned,
            },
            &mut report.polled,
            &mut missing,
        )
        .await?;
        if report.polled.listed_dirs == listed_before {
            // The folder itself is gone from the cloud: the full poll decides.
            self.forget_hot(d.id, dir);
            self.request_full_poll(d.id)?;
            report.full_poll_requested = true;
            return Ok(());
        }
        let (safe, unsure): (Vec<_>, Vec<_>) = missing
            .into_iter()
            .partition(|i| !i.is_dir && i.local_state == LocalState::Placeholder && !i.dirty && !i.pinned);
        self.apply_missing(d, safe, &mut report.polled).await?;
        if !unsure.is_empty() {
            self.request_full_poll(d.id)?;
            report.full_poll_requested = true;
        }
        Ok(())
    }

    /// Make the drive's next full poll due now.
    pub fn request_full_poll(&self, d: DriveId) -> Result<()> {
        self.db.set_last_poll(d, 0)?;
        Ok(())
    }
}
