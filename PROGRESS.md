# Progress

_Last updated: 2026-11-27 (cloud session)._

## Current phase: 4 (Windows provider), not started; it must run on the Windows PC

Phases 0–3 are complete (cloud session, Linux + CI on ubuntu/windows/macos). Phase 4 needs native Windows (PLAN.md §8); see HUMAN_TODO.md item 2 for how to start it.

## Phase exit criteria

### Phase 0: Bootstrap ✅
- [x] CI green on all three OSes (run 1 onwards: fmt, clippy `-D warnings`, tests, `--version`).
- [x] `synclonectl --version` runs.
- [x] rclone v1.76.0 fetched by `scripts/fetch-rclone` with SHA-256 verification; `core/version` ≥ 1.73 through `synclone-rclone` (test `version_is_at_least_1_73`).

### Phase 1: rclone client and drive management ✅
- [x] Supervised `rclone rcd`: random port, per-launch credentials, restart on crash, clean shutdown (`restarts_after_crash_and_shuts_down_cleanly`).
- [x] Typed client + mock; existing remotes imported; add/remove remotes; OAuth state machine (approve / deny / timeout / cancel against the mock, and the real rclone OAuth server start + cancel); token providers.
- [x] `synclonectl drive list/add/remove` against local (alias to temp folder) and memory backends (`drive_list_add_remove_on_local_and_memory_backends`).
- [x] rc unreachable without credentials (`rc_is_unreachable_without_credentials`).
- [x] All §2.5 VERIFY items resolved (DECISIONS.md D-003).

### Phase 2: State DB and remote index ✅
- [x] Schema and versioned migrations; BFS crawler; polling differ; rename detection by ID.
- [x] 100,000 items index and re-poll with bounded memory (`index_100k_items_with_bounded_memory_and_fast_listing`; 11 s debug build, peak extra heap under 256 KiB at both 10k and 100k).
- [x] Diff tests cover add/modify/delete/rename (same test, plus `remote_rename_detected_by_id_keeps_downloaded_copy`, `failed_listing_deletes_nothing`).
- [x] Folder listing from the DB under 50 ms (about 8 ms in a debug build).

### Phase 3: Sync engine with the stub provider ✅
- [x] Every §5 rule has a named passing test (`crates/synclone-provider-stub/tests/conflict_rules.rs`, 14 tests).
- [x] Property test passes 10,000 cases (`property.rs`, about 80 s on Linux, about 165 s on Windows CI). It found two real data-loss bugs, now fixed (D-015). Later CI seeds found only gaps in the test model (D-020), kept as regression seeds. Beyond the CI runs, 4 × 10,000 extra cases passed locally.
- [x] Kill-and-restart test resumes all in-flight operations (`kill_and_restart_resumes_in_flight_operations`: SIGKILL mid-upload, no orphan rclone, all files arrive after restart). Unix only.
- [x] `synclonectl` performs every action in §1.1 (`every_user_action_through_the_cli`).

## Test inventory (all passing locally, Linux)

| Suite | Tests |
|---|---|
| synclone-rclone unit | 12 |
| synclone-rclone vs real rclone (`tests/rcd.rs`) | 6 |
| synclone-core unit | 10 |
| index scale (`tests/index_scale.rs`) | 1 |
| conflict rules | 14 |
| engine behaviour | 22 |
| property (10,000 cases) | 1 |
| CLI end to end vs real rclone | 5 |

## Next
1. Phase 4 on the Windows PC: spike `cloud-filter` vs raw `windows` bindings, then implement `Provider` for the Cloud Files API in `crates/synclone-provider-windows`, using `--rc-serve` ranged reads for hydration with progress (D-003, D-012), and a job object for the rclone child (D-004).
2. Wire the Windows provider into `synclone-daemon` in place of the stub on Windows (D-018).

## Known gaps / failures
- Kill-and-restart test is Unix-only (uses `/proc` and SIGKILL); a Windows version belongs to Phase 4.
- macOS has no equivalent of `PR_SET_PDEATHSIG`: if the daemon is SIGKILLed there, rclone can be left running until its next start. To handle in Phase 6.
- Google Docs handling (§4.7) not built yet; needs a live Drive account (D-019).
- The stub provider detects local changes by scanning every 5 s (no watcher); fine for tests and Linux, replaced by real providers.
- Recursive fast-list for ListR backends not implemented (D-016).
