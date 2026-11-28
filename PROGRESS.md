# Progress

_Last updated: 2026-11-27 (Windows 11 PC session, branch `phase-4-windows`, not pushed yet: HUMAN_TODO.md item 2)._

## Current phase: 6 (macOS), not started. Phases 4 and 5 are done locally, with human follow-ups pending

Phases 0–3 were completed in the cloud (Linux + CI). Phases 4 and 5 were built and tested on the Windows 11 PC; all their automated exit criteria pass in a recorded local run (below). Still open: CI on the new branch (HUMAN_TODO item 2, which also settles the layer 3 VERIFY), and the live sign-in checklist with real accounts (HUMAN_TODO item 3).

## Phase exit criteria

### Phase 0: Bootstrap ✅
- [x] CI green on all three OSes; `synclonectl --version` runs; rclone v1.76.0 fetched and checksum-verified; `core/version` ≥ 1.73.

### Phase 1: rclone client and drive management ✅
- [x] Supervised `rclone rcd`, typed client and mock, OAuth state machine, `synclonectl drive list/add/remove`, rc unreachable without credentials, §2.5 VERIFY items (D-003).

### Phase 2: State DB and remote index ✅
- [x] 100,000 items with bounded memory; diff tests; folder listing under 50 ms.

### Phase 3: Sync engine with the stub provider ✅
- [x] Every §5 rule tested; property test 10,000 cases; kill-and-restart (Unix, and now Windows); every §1.1 action through `synclonectl`.

### Phase 4: Windows provider ✅ (local run; CI pending)
- [x] Spike: `cloud-filter` 0.1.0 chosen over raw `windows` bindings (D-021, with what Windows actually does).
- [x] Layer 3 Windows suite passes: `crates/synclone-provider-windows/tests/cloud_files.rs`, 10 tests plus the 5 GB test. Real sync root, real rclone, real engine; user actions run from a separate PowerShell process. It covers placeholders on browse, open to download (streamed, then kept and pinned), `attrib +P/+U` on folders, uploads and edits, renames and moves, deletes into the trash queue (restore, empty), cloud changes (rules 1 and 2), open-file detection, and drive folder delete (disconnect, cloud untouched) and rename.
- [x] A 5 GB file hydrates and uploads against a `local` (alias) remote: `five_gigabyte_file_hydrates_and_uploads` with `SYNCLONE_BIG_FILE_TEST=1`. 5 GiB hydrated in 46.1 s (including MD5 of it); 5 GiB written and uploaded in 19.9 s; MD5s match both ways.
- [x] Explorer status icons: `docs/evidence/windows-explorer-status.png` (regenerate with `scripts/evidence-windows.ps1`, which renders only the Explorer window of a sandboxed demo drive).
- [ ] VERIFY (§7): does the Cloud Files API work on GitHub's `windows-latest`? Needs the branch pushed (HUMAN_TODO item 2). The tests print SKIPPED where the API is missing.

### Phase 5: Tray app and wizard ✅ (local run; live sign-ins pending)
- [x] Tauri 2 app `Synclone.exe`: the tray menu of §6.1 (status line, per-drive Open folder, Refresh, Pause/Resume, Disconnect…, Add cloud storage…, Trash (N)…, Conflicts (N)…, Recent activity…, Pause all, Settings…, Quit), the wizard, and the Trash, Conflicts, Recent activity, Settings and Disconnect windows. Start at login; the daemon lifecycle (the app starts `Synclone --service` and Quit stops it). See D-026.
- [x] WebDriver tests pass for the wizard with a local provider (`crates/synclone-app/tests/webdriver.rs`, `SYNCLONE_WEBDRIVER=1`). They drive the real windows through tauri-driver 2.0.5 and msedgedriver 158.0.4370.45: wizard, then a local (alias) cloud, then the Trash window's Restore and Empty.
- [x] Trash empty and restore work end to end from the UI layer: `tests/ui_layer.rs` (`trash_restore_and_empty_from_the_ui_layer`) and the WebDriver test.
- [x] HUMAN_TODO.md has the live sign-in checklist for Google Drive, OneDrive and Drime (item 3), plus `docs/MANUAL_CHECKLIST.md`.

## Recorded local test run (2026-11-27, Windows 11 Pro 10.0.26300, rustc 1.100.1)

`cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` are clean. `cargo test --workspace` passes with no failures, plus these opt-in runs:

| Suite | Tests | Result |
|---|---|---|
| synclone-rclone unit / vs real rclone (`rcd.rs`) | 12 / 7 | pass (new: `open_read_streams_objects_with_ranges`) |
| synclone-core unit / index scale | 10 / 1 | pass |
| conflict rules | 14 | pass |
| engine behaviour | 28 | pass (new: OS hydration ×2, drive-folder-gone, rename race D-025, rename-scan race D-027) |
| property test, 10,000 cases | 1 | pass (277 s on this PC) |
| Windows provider unit / layer 3 (`cloud_files.rs`) | 2 / 11 | pass; repeated runs stable after D-025 and D-027 |
| 5 GB hydrate + upload (`SYNCLONE_BIG_FILE_TEST=1`) | 1 | pass |
| CLI end to end (incl. new Windows kill-and-restart) | 5 | pass |
| app UI layer / UI text | 5 / 4 | pass |
| app WebDriver (`SYNCLONE_WEBDRIVER=1`) | 1 | pass |

The Windows kill-and-restart test was also checked the other way round: with the job object assignment disabled, it fails ("rclone … was left running").

## Next
1. Human: push the branch and report CI (HUMAN_TODO 2); run the live sign-ins (HUMAN_TODO 3).
2. Phase 6 on the MacBook: spikes (App Group socket from a sandboxed extension; `.appex` in the Tauri bundle), then the File Provider shim. The tray app's code is cross-platform Tauri already. The daemon picks the stub provider on macOS until the native one exists (`SYNCLONE_PROVIDER`).

## Known gaps / failures
- CI on the PR (windows-latest) found a shrunk property-test case that lost "content-1". Cause: the test's model, not the engine. A poll between a placeholder rename and an overwrite of the new name made the model forget what the placeholder stood for. Fixed in the model; the case is kept as `regression_free_rename_poll_write`, and 10,000 fresh cases pass again.
- Layer 3 on CI is unverified (above).
- When a layer 3 test process dies before unregistering, its sync root can be left as a stale HKLM registry key that WinRT no longer lists, so the harness sweep cannot remove it (two exist now; HUMAN_TODO 4). A test-only issue; the app's own registration is stable.
- A file opened while its parent folder is renamed, before the engine applies the cloud rename, fails to download with "try again" (FETCH_DATA reads from the new path). It works once the rename syncs, about 3 s later (D-022).
- Desktop notifications for conflicts are not built; the tray shows "Attention needed" and Conflicts (N) (D-026).
- Settings: diagnostics export (Phase 8), hiding imported drives, and moving the Synclone folder are not built yet; ignore-list changes apply after a restart.
- The pending-upload list in the Disconnect window is parsed from the daemon's error text; a structured `drive.pending_work` RPC would be cleaner.
- macOS has no equivalent of `PR_SET_PDEATHSIG` or a job object yet (Phase 6).
- Google Docs handling (§4.7) still needs a live Drive account (D-019).
- Recursive fast-list for ListR backends is not implemented (D-016).
