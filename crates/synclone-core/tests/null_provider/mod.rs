#![allow(dead_code)]
//! A provider that holds nothing locally and has populated no folders, the
//! way Windows looks before the user browses anywhere. Used to test the
//! index on its own.

use std::path::{Path, PathBuf};

use synclone_core::provider::{LocalEntry, NameRules, Provider, ProviderEvent, ProviderResult};
use synclone_core::*;

pub struct NullProvider;

impl Provider for NullProvider {
    fn register_drive(&self, _: &Drive) -> ProviderResult<()> {
        Ok(())
    }
    fn unregister_drive(&self, _: &Drive) -> ProviderResult<()> {
        Ok(())
    }
    fn upsert_placeholders(&self, _: &Drive, _: &RelPath, _: &[ItemMeta]) -> ProviderResult<()> {
        Ok(())
    }
    fn remove_local(&self, _: &Drive, _: &RelPath) -> ProviderResult<()> {
        Ok(())
    }
    fn set_state(&self, _: &Drive, _: &RelPath, _: LocalState) -> ProviderResult<()> {
        Ok(())
    }
    fn is_open(&self, _: &Drive, _: &RelPath) -> ProviderResult<bool> {
        Ok(false)
    }
    fn local_status(&self, _: &Drive, p: &RelPath) -> ProviderResult<LocalStatus> {
        Ok(if p.is_root() {
            LocalStatus::Dir
        } else {
            LocalStatus::Absent
        })
    }
    fn content_path(&self, _: &Drive, _: &RelPath) -> PathBuf {
        PathBuf::from("/nonexistent")
    }
    fn commit_download(
        &self,
        _: &Drive,
        _: &RelPath,
        _: &Path,
        _: &ItemMeta,
        _: LocalStatus,
    ) -> ProviderResult<LocalFingerprint> {
        anyhow::bail!("null provider stores nothing")
    }
    fn rename_local(&self, _: &Drive, _: &RelPath, _: &RelPath) -> ProviderResult<()> {
        Ok(())
    }
    fn copy_local(&self, _: &Drive, _: &RelPath, _: &RelPath) -> ProviderResult<()> {
        Ok(())
    }
    fn create_dir(&self, _: &Drive, _: &RelPath) -> ProviderResult<()> {
        Ok(())
    }
    fn scan(&self, _: &Drive) -> ProviderResult<Vec<LocalEntry>> {
        Ok(Vec::new())
    }
    fn is_populated(&self, _: &Drive, _: &RelPath) -> bool {
        false
    }
    fn name_rules(&self) -> NameRules {
        NameRules::POSIX
    }
    fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>> {
        None
    }
}
