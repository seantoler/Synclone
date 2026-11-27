//! The durable op journal runner (PLAN.md §2.6 `ops`). Every upload,
//! download, move and delete is a row before it runs; a crash leaves it
//! `running`, and `recover()` puts it back to `pending`. Failures stay in
//! the journal with exponential backoff: nothing is dropped silently.

use anyhow::Result;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use super::sync::{is_retryable, Outcome, Want};
use super::Engine;
use crate::db::Op;
use crate::types::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    pub executed: usize,
    pub failed: usize,
    pub deferred: usize,
}

impl Engine {
    /// Call once at startup: ops interrupted by a crash run again.
    pub fn recover(&self) -> Result<usize> {
        Ok(self.db.reset_running_ops()?)
    }

    /// Run every op that is due now (up to `concurrency` at a time, never
    /// two on the same path). Returns what happened.
    pub async fn run_due(&self) -> Result<RunReport> {
        let now = self.now();
        let mut due = self.db.due_ops(now, 256)?;
        // Skip paused drives and duplicate paths in this batch.
        let mut seen = std::collections::HashSet::new();
        let paused: std::collections::HashSet<DriveId> = self
            .db
            .drives()?
            .into_iter()
            .filter(|d| d.paused || d.state != DriveState::Connected)
            .map(|d| d.id)
            .collect();
        due.retain(|op| !paused.contains(&op.drive_id) && seen.insert((op.drive_id, op.path.clone())));
        let mut report = RunReport::default();
        let results: Vec<Result<OpResult>> = stream::iter(due.into_iter().map(|op| self.run_op(op)))
            .buffer_unordered(self.settings.concurrency.max(1))
            .collect()
            .await;
        for r in results {
            match r? {
                OpResult::Done => report.executed += 1,
                OpResult::Deferred => report.deferred += 1,
                OpResult::Failed => report.failed += 1,
                OpResult::Skipped => {}
            }
        }
        Ok(report)
    }

    /// Run ops until nothing is due. With `include_future`, ops waiting for
    /// debounce or backoff run as well (tests and "sync now").
    pub async fn run_until_idle(&self, include_future: bool, max_rounds: usize) -> Result<RunReport> {
        let mut total = RunReport::default();
        for _ in 0..max_rounds {
            if include_future {
                let c = self.db.lock();
                c.execute("UPDATE ops SET not_before = 0 WHERE state = 'pending'", [])?;
            }
            let r = self.run_due().await?;
            total.executed += r.executed;
            total.failed += r.failed;
            total.deferred += r.deferred;
            if r.executed == 0 && r.deferred == 0 {
                break;
            }
            if r.executed == 0 && !include_future {
                break;
            }
        }
        Ok(total)
    }

    async fn run_op(&self, op: Op) -> Result<OpResult> {
        let key = (op.drive_id, op.path.as_str().to_string());
        {
            let mut busy = self.busy.lock().unwrap();
            if !busy.insert(key.clone()) {
                return Ok(OpResult::Skipped);
            }
        }
        let result = self.run_op_inner(&op).await;
        self.busy.lock().unwrap().remove(&key);
        result
    }

    async fn run_op_inner(&self, op: &Op) -> Result<OpResult> {
        if !self.db.mark_op_running(op.id)? {
            return Ok(OpResult::Skipped);
        }
        let outcome = self.execute(op).await;
        match outcome {
            Ok(Outcome::Done) => {
                self.db.finish_op(op.id)?;
                Ok(OpResult::Done)
            }
            Ok(Outcome::Defer { ms, why }) => {
                tracing::debug!("op {} {} on {} deferred: {why}", op.id, op.kind, op.path);
                // A deferral is not a failure: don't count the attempt, but
                // keep retrying after a delay; ms = 0 means "re-run at once".
                {
                    let c = self.db.lock();
                    c.execute("UPDATE ops SET attempts = max(attempts - 1, 0) WHERE id = ?1", [op.id])?;
                }
                if op.attempts > self.settings.max_attempts * 4 {
                    self.db.fail_op(op.id, &format!("still waiting: {why}"))?;
                    return Ok(OpResult::Failed);
                }
                self.db.retry_op(op.id, &why, self.now() + ms)?;
                Ok(OpResult::Deferred)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let attempts = op.attempts + 1;
                if is_retryable(&e) && attempts < self.settings.max_attempts {
                    let mut delay = self
                        .settings
                        .backoff_base_ms
                        .saturating_mul(1i64 << (attempts - 1).min(20));
                    if super::index::is_rate_limited(&e) {
                        delay = delay.saturating_mul(4);
                    }
                    let delay = delay.min(self.settings.backoff_max_ms);
                    self.db.retry_op(op.id, &msg, self.now() + delay)?;
                    self.db.log_activity(
                        Some(op.drive_id),
                        Some(&op.path),
                        &op.kind,
                        &format!("Will retry: {msg}"),
                        true,
                    );
                    Ok(OpResult::Deferred)
                } else {
                    // Kept in the journal as failed and shown to the user.
                    self.db.fail_op(op.id, &msg)?;
                    self.db.log_activity(
                        Some(op.drive_id),
                        Some(&op.path),
                        &op.kind,
                        &format!("Failed: {msg}"),
                        true,
                    );
                    Ok(OpResult::Failed)
                }
            }
        }
    }

    async fn execute(&self, op: &Op) -> Result<Outcome> {
        let d = self.drive(op.drive_id)?;
        match op.kind.as_str() {
            "reconcile" => self.reconcile(&d, &op.path, Want::Sync).await,
            "hydrate" => self.reconcile(&d, &op.path, Want::Download).await,
            "pin" => {
                let recursive = op.payload.get("recursive").and_then(|v| v.as_bool()).unwrap_or(true);
                self.pin(d.id, &op.path, recursive).await
            }
            "move" => {
                let from = op
                    .payload
                    .get("from")
                    .and_then(|v| v.as_str())
                    .and_then(RelPath::new)
                    .ok_or_else(|| anyhow::anyhow!("move op without a source"))?;
                self.apply_local_move(&d, &from, &op.path).await
            }
            "list" => {
                self.index_dir(&d, &op.path).await?;
                Ok(Outcome::Done)
            }
            "empty_trash" => {
                let id = op.payload.get("trash_id").and_then(|v| v.as_i64()).unwrap_or(-1);
                self.empty_trash_entry(id).await?;
                Ok(Outcome::Done)
            }
            other => {
                tracing::warn!("unknown op kind {other}; dropping");
                Ok(Outcome::Done)
            }
        }
    }

    /// One full sync pass for a drive: local scan, remote poll, run ops.
    pub async fn sync_drive_now(&self, drive_id: DriveId) -> Result<RunReport> {
        self.scan_drive(drive_id)?;
        self.poll_drive(drive_id).await?;
        self.run_until_idle(true, 100).await
    }
}

enum OpResult {
    Done,
    Deferred,
    Failed,
    Skipped,
}
