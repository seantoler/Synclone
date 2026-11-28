//! Drive management: import existing remotes, add new ones through the
//! sign-in flow, disconnect without touching remote data (PLAN.md §1.1, §6.5).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};
use synclone_rclone::config_flow::{self, Answer, FlowEvent, FlowSettings};
use synclone_rclone::ConfigQuestion;

use super::Engine;
use crate::types::*;

/// Friendly names for the backends people are most likely to connect.
pub fn service_name(kind: &str) -> &str {
    match kind {
        "drive" => "Google Drive",
        "onedrive" => "OneDrive",
        "drime" => "Drime",
        "dropbox" => "Dropbox",
        "box" => "Box",
        "pcloud" => "pCloud",
        "mega" => "MEGA",
        "protondrive" => "Proton Drive",
        "s3" => "S3",
        "sftp" => "SFTP",
        "webdav" => "WebDAV",
        other => other,
    }
}

/// How long a missing drive folder must stay missing to count as removed.
pub const ROOT_CHECK_DELAY_MS: i64 = 2_000;

/// What a disconnect would leave behind.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingWork {
    /// Files with local changes not yet uploaded.
    pub unsynced: Vec<RelPath>,
}

impl Engine {
    pub fn drives(&self) -> Result<Vec<Drive>> {
        Ok(self.db.drives()?)
    }

    pub fn drive(&self, id: DriveId) -> Result<Drive> {
        self.db.drive(id)?.ok_or_else(|| anyhow!("no cloud drive with id {id}"))
    }

    pub fn drive_by_name(&self, name: &str) -> Result<Drive> {
        let all = self.db.drives()?;
        all.iter()
            .find(|d| d.display_name.eq_ignore_ascii_case(name))
            .or_else(|| all.iter().find(|d| d.remote_name == name))
            .cloned()
            .ok_or_else(|| anyhow!("no cloud drive named \"{name}\""))
    }

    /// A display name not used by another drive: `base`, `base 2`, ...
    pub fn unique_display_name(&self, base: &str) -> Result<String> {
        let taken: Vec<String> = self
            .db
            .drives()?
            .into_iter()
            .map(|d| d.display_name.to_lowercase())
            .collect();
        let base = sanitize_folder_name(base);
        if !taken.contains(&base.to_lowercase()) {
            return Ok(base);
        }
        for n in 2.. {
            let c = format!("{base} {n}");
            if !taken.contains(&c.to_lowercase()) {
                return Ok(c);
            }
        }
        unreachable!()
    }

    /// A remote name not in rclone's config: `base`, `base2`, ...
    pub async fn unique_remote_name(&self, base: &str) -> Result<String> {
        let existing = self.api.list_remotes().await?;
        let base: String = base
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        // One-letter names read as drive letters on Windows ("C:").
        let base = match base.len() {
            0 => "cloud".to_string(),
            1 => format!("{base}-cloud"),
            _ => base,
        };
        if !existing.contains(&base) {
            return Ok(base);
        }
        for n in 2.. {
            let c = format!("{base}{n}");
            if !existing.contains(&c) {
                return Ok(c);
            }
        }
        unreachable!()
    }

    /// Ask the remote what it can do and store the comparison rules.
    pub async fn probe_drive(&self, drive: &mut Drive) -> Result<()> {
        let info = self.api.fsinfo(&drive.fs()).await?;
        drive.hash_type = info.preferred_hash();
        drive.keeps_mod_times = info.keeps_mod_times();
        drive.time_tolerance_ns = info.time_tolerance_ns();
        // OneDrive for Business / SharePoint may rewrite Office files on
        // upload, changing their hash: compare by size and time there.
        if drive.provider_type == "onedrive" {
            let cfg = self.api.config_get(&drive.remote_name).await.unwrap_or_default();
            let t = cfg.get("drive_type").and_then(Value::as_str).unwrap_or("");
            if t == "business" || t == "documentLibrary" {
                drive.hash_type = None;
            }
        }
        self.db.update_drive(drive)?;
        Ok(())
    }

    fn new_drive(&self, remote: &str, kind: &str, display: String, imported: bool) -> Drive {
        Drive {
            id: 0,
            remote_name: remote.to_string(),
            display_name: display,
            root_path: String::new(),
            provider_type: kind.to_string(),
            added_at: self.now(),
            paused: false,
            state: DriveState::Connected,
            imported,
            hash_type: None,
            time_tolerance_ns: 1_000_000_000,
            keeps_mod_times: true,
            poll_interval_secs: self.settings.default_poll_secs,
        }
    }

    async fn insert_and_register(&self, mut d: Drive) -> Result<Drive> {
        d.id = self.db.insert_drive(&d)?;
        self.db.ensure_root(d.id)?;
        if let Err(e) = self.probe_drive(&mut d).await {
            tracing::warn!("could not read capabilities of {}: {e}", d.remote_name);
        }
        if d.state == DriveState::Connected {
            self.provider.register_drive(&d)?;
        }
        self.db.log_activity(
            Some(d.id),
            None,
            "drive",
            &format!("Connected {}", d.display_name),
            false,
        );
        Ok(d)
    }

    /// Remotes in the rclone config that are not drives yet become drives
    /// (PLAN.md §1.1). Returns the new drives.
    pub async fn import_remotes(&self) -> Result<Vec<Drive>> {
        if self.db.setting("import_remotes")?.as_deref() == Some("false") {
            return Ok(Vec::new());
        }
        let remotes = self.api.list_remotes().await?;
        let hidden: Vec<String> = self
            .db
            .setting("hidden_remotes")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut added = Vec::new();
        for r in remotes {
            let left_out = self
                .settings
                .only_remotes
                .as_ref()
                .map(|only| !only.contains(&r))
                .unwrap_or(false);
            if left_out || hidden.contains(&r) || self.db.drive_by_remote(&r)?.is_some() {
                continue;
            }
            let cfg = self.api.config_get(&r).await.unwrap_or_default();
            let kind = cfg.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let display = self.unique_display_name(&r)?;
            let d = self.new_drive(&r, &kind, display, true);
            added.push(self.insert_and_register(d).await?);
        }
        Ok(added)
    }

    /// The "Add cloud storage" flow: create the rclone remote (browser
    /// sign-in, token or form) and connect it as a drive.
    pub async fn add_drive(
        &self,
        kind: &str,
        display_name: Option<&str>,
        parameters: Map<String, Value>,
        ask: &mut (dyn FnMut(&ConfigQuestion) -> Answer + Send),
        events: &mut (dyn FnMut(FlowEvent) + Send),
    ) -> Result<Drive> {
        let display = self.unique_display_name(display_name.unwrap_or(service_name(kind)))?;
        let remote = self.unique_remote_name(&display).await?;
        let mut parameters = parameters;
        let client = self.settings.google_client.as_ref();
        crate::oauth::apply_google_client(kind, &mut parameters, client);
        config_flow::create_remote(
            self.api.as_ref(),
            &remote,
            kind,
            parameters,
            ask,
            events,
            &FlowSettings::default(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", crate::oauth::redact(&e.to_string(), client)))
        .with_context(|| format!("could not set up {}", service_name(kind)))?;
        let d = self.new_drive(&remote, kind, display, false);
        self.insert_and_register(d).await
    }

    /// Connect an existing remote by name (CLI / tests).
    pub async fn connect_remote(&self, remote: &str, display_name: Option<&str>) -> Result<Drive> {
        if let Some(mut d) = self.db.drive_by_remote(remote)? {
            if d.state == DriveState::Disconnected {
                d.state = DriveState::Connected;
                self.db.update_drive(&d)?;
                self.db.ensure_root(d.id)?;
                self.provider.register_drive(&d)?;
            }
            return Ok(d);
        }
        if !self.api.list_remotes().await?.iter().any(|r| r == remote) {
            bail!("rclone has no remote named \"{remote}\"");
        }
        let cfg = self.api.config_get(remote).await.unwrap_or_default();
        let kind = cfg.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let display = self.unique_display_name(display_name.unwrap_or(remote))?;
        let d = self.new_drive(remote, &kind, display, false);
        self.insert_and_register(d).await
    }

    /// Local work that would be lost by disconnecting now.
    pub fn pending_work(&self, id: DriveId) -> Result<PendingWork> {
        let mut unsynced: Vec<RelPath> = self.db.dirty_items(id)?.into_iter().map(|i| i.path).collect();
        for op in self.db.ops_for(id)? {
            if op.kind == "reconcile" || op.kind == "move" {
                if let Ok(LocalStatus::File(_)) = self.provider.local_status(&self.drive(id)?, &op.path) {
                    if !unsynced.contains(&op.path) && self.local_is_dirty(id, &op.path).unwrap_or(true) {
                        unsynced.push(op.path.clone());
                    }
                }
            }
        }
        unsynced.sort();
        Ok(PendingWork { unsynced })
    }

    fn local_is_dirty(&self, id: DriveId, path: &RelPath) -> Result<bool> {
        let d = self.drive(id)?;
        let st = self.provider.local_status(&d, path)?;
        let it = self.db.item(id, path)?;
        Ok(match (st, it) {
            (LocalStatus::File(fp), Some(it)) => it.local_base != Some(fp) || it.synced.is_none(),
            (LocalStatus::File(_), None) => true,
            _ => false,
        })
    }

    /// Disconnect: remove the local folder, keep the sign-in, issue zero
    /// remote calls (PLAN.md §6.5, §9 rule 8). With unsynced local edits
    /// this refuses unless `recover` is set, in which case those files are
    /// first copied to the Recovered folder.
    pub async fn disconnect(&self, id: DriveId, recover: bool) -> Result<Option<PathBuf>> {
        let mut d = self.drive(id)?;
        let pending = self.pending_work(id)?;
        let mut recovered = None;
        if !pending.unsynced.is_empty() {
            if !recover {
                bail!(
                    "{} has {} file(s) with changes not uploaded yet: {}. Wait for them to upload, or disconnect with recovery to keep them in a local Recovered folder.",
                    d.display_name,
                    pending.unsynced.len(),
                    pending.unsynced.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
                );
            }
            let dest = self.recovered_dir.join(sanitize_folder_name(&d.display_name));
            for p in &pending.unsynced {
                let src = self.provider.content_path(&d, p);
                let dst = dest.join(p.as_str());
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src, &dst).with_context(|| format!("copying {} to the Recovered folder", p))?;
            }
            recovered = Some(dest);
        }
        self.provider.unregister_drive(&d)?;
        self.db.clear_drive_state(id)?;
        d.state = DriveState::Disconnected;
        self.db.update_drive(&d)?;
        self.db.log_activity(
            Some(id),
            None,
            "drive",
            &format!("Disconnected {}", d.display_name),
            false,
        );
        Ok(recovered)
    }

    /// The drive's folder looks gone. Check again in a moment, by the
    /// drive's name at that time, before treating it as a disconnect: a
    /// rename of the folder (in Explorer, or from the app, which updates
    /// the DB and the folder one after the other) looks the same for an
    /// instant (D-027).
    pub fn drive_folder_missing(&self, id: DriveId) -> Result<()> {
        self.db.enqueue_op(
            "rootcheck",
            id,
            &RelPath::root(),
            &serde_json::json!({}),
            self.now() + ROOT_CHECK_DELAY_MS,
        )?;
        Ok(())
    }

    /// The drive's folder was deleted by the user: disconnect, no remote
    /// calls, nothing into the trash queue (PLAN.md §3.1).
    pub fn drive_root_removed(&self, id: DriveId) -> Result<()> {
        let mut d = self.drive(id)?;
        self.db.clear_drive_state(id)?;
        let _ = self.provider.unregister_drive(&d);
        d.state = DriveState::Disconnected;
        self.db.update_drive(&d)?;
        self.db.log_activity(
            Some(id),
            None,
            "drive",
            &format!("{} folder was removed; disconnected", d.display_name),
            false,
        );
        Ok(())
    }

    /// Rename a drive's folder (display name only).
    pub fn rename_drive(&self, id: DriveId, new_name: &str) -> Result<Drive> {
        let mut d = self.drive(id)?;
        let name = sanitize_folder_name(new_name);
        if self
            .db
            .drives()?
            .iter()
            .any(|o| o.id != id && o.display_name.eq_ignore_ascii_case(&name))
        {
            bail!("another cloud drive is already called \"{name}\"");
        }
        d.display_name = name;
        self.db.update_drive(&d)?;
        Ok(d)
    }

    /// Disconnect and forget a drive. With `sign_out` also delete its rclone
    /// remote (a config change only: nothing in the cloud is touched).
    pub async fn forget_drive(&self, id: DriveId, sign_out: bool) -> Result<()> {
        let d = self.drive(id)?;
        if d.state == DriveState::Connected {
            self.disconnect(id, false).await?;
        }
        self.db.delete_drive(id)?;
        if sign_out {
            self.api.config_delete(&d.remote_name).await?;
        } else if d.imported {
            // Keep it from being imported again at the next start.
            let mut hidden: Vec<String> = self
                .db
                .setting("hidden_remotes")?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            hidden.push(d.remote_name.clone());
            self.db
                .set_setting("hidden_remotes", &serde_json::to_string(&hidden)?)?;
        }
        Ok(())
    }

    pub fn set_paused(&self, id: DriveId, paused: bool) -> Result<()> {
        let mut d = self.drive(id)?;
        d.paused = paused;
        self.db.update_drive(&d)?;
        Ok(())
    }

    pub fn arc(self: &Arc<Self>) -> Arc<Self> {
        self.clone()
    }
}

/// Make a string safe as a folder name on Windows and macOS.
pub fn sanitize_folder_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32 {
                '-'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim().trim_end_matches('.').trim().to_string();
    if t.is_empty() {
        "Cloud".into()
    } else {
        t
    }
}
