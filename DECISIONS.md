# Decisions

Each entry: the decision, then one paragraph of reasoning. Newest last. `VERIFY` items from PLAN.md are resolved here with what was observed.

## D-001 Name: Synclone; crate and binary names

The app is called **Synclone** (owner's choice, replacing the "CloudFolders" placeholder in PLAN.md). The name lives in `synclone_core::APP_NAME` / `APP_ID`. The plan's `cf-*` crate prefix became `synclone-*`, `cfctl` became `synclonectl`, `.cfstub` became `.sclstub` and `CF_LIVE_TEST_REMOTES` became `SYNCLONE_LIVE_TEST_REMOTES`. The CLI is `synclonectl` rather than `synclone` because the tray app's executable will be `Synclone`/`Synclone.exe`, and on case-insensitive file systems (Windows, default macOS) both would collide in one install directory.

## D-002 rclone pinned at v1.76.0, checksums committed

`scripts/fetch-rclone` downloads `rclone-v1.76.0-<os>-<arch>.zip` from GitHub releases and verifies it against SHA-256 values committed in `scripts/rclone-checksums.txt` (copied from the release's `SHA256SUMS`, which is PGP-signed by the rclone team). The checksums are not fetched at run time, so a compromised release page cannot silently change the binary. v1.76.0 is the newest release on 2026-11-27 and satisfies the plan's ≥ 1.73 requirement (Drime). Tests find the binary via `SYNCLONE_RCLONE` or `vendor/rclone/<os>-<arch>/`. rclone on `PATH` is not used: the app ships a known build.

## D-003 VERIFY (§2.5): rc endpoints, auth, `--rc-serve` Range, async jobs

Checked against a running `rclone rcd` v1.76.0 with `rc/list` and direct calls:

- Every endpoint listed in §2.5 exists: `config/listremotes`, `config/get`, `config/create`, `config/update`, `config/delete`, `config/providers`, `operations/list`, `operations/stat`, `operations/copyfile`, `operations/movefile`, `operations/deletefile`, `operations/mkdir`, `operations/rmdir`, `operations/purge`, `operations/about`, `core/stats`, `core/version`, `job/status`, `job/stop`. Also used: `operations/fsinfo`, `operations/hashsumfile`, `core/bwlimit`, `core/quit`, `config/paths`, and the new `config/oauthstatus` / `config/oauthstop`.
- With `--rc-user/--rc-pass` set, every rc path (including `core/command`, which runs OS commands) and every `--rc-serve` object URL answers 401 without the login or with a wrong password. Covered by the test `rc_is_unreachable_without_credentials`.
- `--rc-serve` serves objects at `/[remote:]/path` with `Accept-Ranges: bytes`; a `Range: bytes=10-19` request returned `206 Partial Content` with the right 10 bytes and `Content-Range: bytes 10-19/100000`. So streaming, ranged hydration is possible for the Windows provider (Phase 4).
- `_async=true` returns `{"jobid": N}`; `job/status` reports `finished`, `success`, `error` and `output` (the synchronous result). All long calls (copy, move, delete, purge, recursive list, hashsum, config create/update) go through async jobs, so HTTP timeouts never cut off a transfer (Nimbus #180).
- `operations/stat` on a missing path returns `{"item": null}`; under a missing directory it can return a "directory not found" error, which the client also maps to `None`. `operations/deletefile` on a missing object returns HTTP 404 `object not found`.
- `operations/list` with `showHash` but no `hashTypes` computes every hash rclone knows (14 on the local backend). The client always names the hash types it wants.

## D-004 rclone launch details

`rclone rcd --rc-addr 127.0.0.1:<random free port> --rc-serve --ask-password=false`, with the login passed as `RCLONE_RC_USER` / `RCLONE_RC_PASS` environment variables (never on the command line, so it does not show in process lists) and `RCLONE_RC_NO_AUTH` removed from the inherited environment. The username and a 32-character password are random per launch. The supervisor restarts rclone on the same port and login after 1 s, 5 s, 15 s, then every 60 s. On Linux the child gets `PR_SET_PDEATHSIG` so a killed daemon does not leave rclone behind; Windows will use a job object in Phase 4, as OpenCfDrive does. The HTTP client never uses a proxy for 127.0.0.1.

## D-005 Sign-in (OAuth) uses rclone's non-interactive config protocol

`config/create` with `opt.nonInteractive=true` returns one question at a time (`State`, `Option`). Synclone answers known questions itself, uses rclone's default for the rest where there is one, and asks the user otherwise; it continues with `config/update` + `opt.continue/state/result`. Answering `config_is_local=true` makes rclone start its local OAuth server (127.0.0.1:53682) and block until the browser flow ends. That call runs as an async job while `config/oauthstatus` is polled for `authUrl`, which the UI opens in the browser. `config/oauthstop` cancels (the job then fails with "oauth authentication was cancelled"). On any failure, cancel or timeout (10 min) the half-made remote is deleted with `config/delete`. Verified live with the `drive` backend (up to the browser step) and with the mock for the full approve/deny/timeout state machine.

## D-006 What Synclone reuses from OpenCfDrive

OpenCfDrive (github.com/dkgs2000/OpenCfDrive, MIT, .NET, studied at commit 03f2d03) is credited in the README and in `THIRD-PARTY-NOTICES.md`. No code is copied verbatim (different language); these designs are adapted:

- **rclone process:** `rclone rcd --rc-serve` on a free 127.0.0.1 port with a random login in environment variables, strip `RCLONE_RC_NO_AUTH`, wait for `core/version`, restart on the same port and login with growing delays (`RcloneDaemon.cs`).
- **Addressing:** always pass a remote's root (`remote:`) as `fs` and the sub-path as `remote`, never a sub-path as the fs root. A sub-path root makes rclone probe it as a file first, which breaks on some WebDAV servers behind TLS proxies (their `docs/architecture.md`).
- **Listing:** `recurse` must go in `opt`, not top level; ask for no MIME types; name hash types explicitly.
- **Version comparison:** size first, then the remote's own hash when both sides have the same type, else modification time within the remote's precision (at least 1 s). Hash beats time. A remote that cannot set times (`Precision` = `ModTimeNotSupported`) never matches on time. Preferred hash order sha1, md5, sha256; no hash when `SlowHash` (local, SFTP). (`RemoteCapabilities.cs`, `FileVersions.cs`.)
- **Remote capabilities:** read `operations/fsinfo` once per drive (hashes, precision, `SlowHash`, `SlowModTime`, `CanHaveEmptyDirectories`).
- **Baselines:** store the server's post-upload `stat` as the remote baseline (servers re-stamp times), and keep a separate local baseline (size, mtime) taken before the upload starts, so a write during the upload still reads as an edit next time.
- **Safety checks:** stat the server right before every upload and every delete; re-check the local file right before replacing it. Hash the local file (`operations/hashsumfile`) only to rule out a false conflict. Never infer remote deletes from a missing local file at startup. If a listing fails, keep the cache and delete nothing. Treat "object not found" on delete as done (idempotent).
- **Conflicts:** server keeps the name; the local version becomes a conflict copy uploaded as a new file; names are truncated to fit 255 bytes without splitting a character; numbered suffixes on collision.
- **Folder deletes:** delete only what was seen (OpenCfDrive deletes known files and then removes the folder only if empty). Synclone applies the same rule when emptying a folder from the trash queue (D-015).
- **Echo suppression:** don't treat the engine's own writes as user edits.

Not adopted, because the plan or their own weaknesses argue against it:

- `operations/uploadfile` (multipart HTTP upload of the bytes). Synclone uses `operations/copyfile` from the local path, so rclone reads the file itself, with its chunking and retries for multi-GB audiobooks (§4.4).
- Dropping failed steps. OpenCfDrive marks a step complete even when it fails. Synclone's `ops` journal keeps failures with backoff (§2.6).
- Their missing temp-file ignore list.
- Path-only identity. Synclone detects remote renames by ID (§4.3).

## D-007 Google Drive's shared client ID is being retired during 2026

rclone v1.76.0's Drive config asks `config_shared_client_id` with the warning "rclone's shared Google Drive client_id is being retired and will stop working during 2026". For now the wizard answers "yes" so sign-in works out of the box, and the Drive form exposes `client_id` / `client_secret` under Advanced. Before the public release the project needs its own Google OAuth client (verified app, Drive scope). That is a human task, recorded in HUMAN_TODO.md.

## D-008 Unsafe options are rejected at creation

`hard_delete=true` (OneDrive, Drime and others) and `use_trash=false` (Drive) are refused by `config_flow::check_parameters` before rclone is called (§6.4 precedence rule). Emptying the trash then always uses the provider's own trash where it has one (§4.6).

## D-009 Async runtime and HTTP stack

The rclone client and engine are async (`tokio`), and the `RcloneApi` trait uses `async-trait` so it can be a trait object (`Arc<dyn RcloneApi>`), with the real client and the mock behind one type. HTTP is `reqwest` with no TLS features: the only server is rclone on 127.0.0.1.

## D-010 Engine shape: reconcile from current state, one path at a time

The engine has no in-memory sync state machine. Each `reconcile(path)` reads the last-synced baseline (DB), the local file (provider) and a fresh remote `stat`, then applies PLAN.md §5. Because every decision is re-derived, a reconcile is idempotent: repeating it after a crash is safe. Intent that can't be re-derived from state is journaled as its own op: `move` (local renames), `pin`, `hydrate` (user opened a placeholder), `empty_trash`, `list`. Finished ops are deleted, so the `ops` table holds only unfinished or failed work. Failures back off exponentially (×4 for rate limits) and end as `failed` rows shown to the user. A file that is open or still changing is a deferral, not a failure.

## D-011 Schema additions to PLAN.md §2.6

Additions to the plan's schema:

- `items`: `pinned`, a local baseline (`local_size`, `local_mtime`) separate from the remote baseline (`synced_*`), `unsyncable` (reason text), and `listed_at` for folders. A root row (`path = ''`) carries the root's pinned flag and listing time.
- `trash`: `size` and `note`.
- `ops`: `not_before` (debounce/backoff) and `updated_at`.
- `drives`: `state` (connected/disconnected), `imported`, the comparison rules from `fsinfo` (`hash_type`, `time_tolerance_ns`, `keeps_mod_times`), `poll_interval_secs`, `last_poll_at`, and `extra_options_json` for advanced mode later (§6.4).
- New table `activity` for "Recent activity".

Migrations are a list applied in order, tracked by `PRAGMA user_version`. The DB runs in WAL mode with a busy timeout.

## D-012 Transfers in Phases 0–3

Uploads use `operations/copyfile` from the file's local directory (D-006). Right before each upload the remote is re-stat'ed against the baseline. The local (size, mtime) is snapshotted before the transfer and becomes the baseline, so a write during the upload still counts as an edit. Downloads go to a staging folder via `copyfile`, are verified by size and (where the remote has one) hash, and are handed to `Provider::commit_download`. That call refuses if the local file changed since the decision. Streaming ranged hydration through `--rc-serve` (verified in D-003) is for the Windows provider in Phase 4, which needs `CfExecute` data transfer with progress.

## D-013 Conflict copies are copies, and reused after a crash

Rules 4 and 9:

1. Record a `conflicts` row.
2. Copy the local file to `name (conflict - <host> <YYYY-MM-DD HH.MM>).ext`.
3. Upload the copy as a new file.
4. Download the remote version over the original name (guarded by the local fingerprint).

Copying rather than renaming means a crash between steps leaves both versions on disk. On retry, an existing conflict copy with identical content is reused instead of making another. Open files are deferred; a file is never replaced while a program holds it.

## D-014 Emptying a trashed folder never deletes unseen data

A trashed folder stores a snapshot of every item beneath it. "Empty trash" lists the folder recursively first:

- If everything matches the snapshot, it uses `operations/purge` (the provider's trash where there is one).
- Otherwise it deletes only the files that are exactly as they were, keeps changed and new files, and restores them as placeholders with a note.

A trashed file that changed in the cloud is restored instead of deleted (rule 8 at empty time).

## D-015 Cases found by the property test

The property test found three real data-loss bugs, now fixed and covered by its regression seeds:

1. A local rename of a never-synced file onto a cloud path did a server-side `movefile` that overwrote the cloud file. Local moves now rename in the cloud only when the local item is that cloud object (synced, or still a placeholder) and the destination is free. Otherwise the cloud item reappears as a placeholder and the moved file is reconciled as new (keep both).
2. Following a cloud rename locally could overwrite a new local file at the destination. `relocate_local` never overwrites; it resets the destination's baseline so rule 9 keeps both. The stub provider's `rename_local` refuses to overwrite as a second barrier.

3. Found later by a macOS CI seed: a journaled local move whose source path, by the time it ran, held a different item's record (a cloud rename had moved a synced file's record there) treated the user's new file as that cloud file, moved it server-side and uploaded over it. The move op now records which item (ID, else synced version) was at the source when the user renamed it; if that changed, the cloud item stays where it is and the local file is reconciled as new. Named test: `local_move_whose_source_was_replaced_by_a_cloud_rename_keeps_both`.

Semantics settled on the way:

- Deleting a file and then saving a new file under the same name (how many editors save) is an edit of that file. It cancels the trash entry and uploads.
- Renaming a file onto a name that was just deleted replaces it.

## D-016 Indexing: breadth-first, one folder per call, deletions last

The crawler lists one folder at a time (`operations/list`, not recursive) and diffs it against the DB inside one transaction per folder. Memory is bounded by the largest folder plus the queue of folder paths. Measured with a counting allocator: peak extra Rust heap is under 256 KiB for both 10k and 100k items. SQLite's own allocations are outside that count; its page cache is capped by `cache_size`. Deletions are collected and applied only after the whole poll succeeds, so a failed listing deletes nothing.

Renames are matched by provider ID. An ID that shows up at a new path counts as a rename only if the old path is really gone, because Drive allows several parents. Recursive fast-list for ListR backends is postponed: it trades API calls for memory that grows with the tree, and the Phase 2 exit asks for bounded memory. It can be added per backend later.

The remote-ID lookup has to force its index (`INDEXED BY`). Without statistics SQLite chose a primary-key scan per new item, which made indexing quadratic.

## D-017 Test strategy for the engine

Layer 1–2 engine tests run against `MockRclone`, an in-memory remote with real local file I/O, call counting and fault injection, plus `StubProvider`. That covers one named test per §5 rule, 22 behaviour tests, and the 10,000-case property test (about 80 s on Linux). Real rclone is exercised end to end through the CLI tests (daemon + `rclone rcd` + `alias` remote onto a temp folder; a bare `local` remote would expose the whole disk), including SIGKILL of the daemon mid-upload. `PROPTEST_CASES` overrides the case count for quick local runs; CI runs the full 10,000.

## D-018 Daemon and IPC

The daemon is a library (`synclone-daemon`) with two entry points: the `synclone-daemon` binary and `synclonectl daemon`. IPC is JSON-RPC 2.0, one JSON object per line:

- Unix: a socket in the state directory (mode 0600). A stale socket is removed only if nothing answers on it.
- Windows: a named pipe `\\.\pipe\synclone-<user>` with remote clients rejected.

The rclone login never leaves the daemon. Every path is overridable by environment variables (`SYNCLONE_STATE_DIR`, `SYNCLONE_ROOT`, `SYNCLONE_SOCKET`, `SYNCLONE_RCLONE_CONFIG`, `SYNCLONE_RCLONE`, `SYNCLONE_RCLONE_ARGS`), which is how tests stay away from real files. Until Phases 4 and 6, the daemon uses the stub provider on every OS and scans for local changes every 5 s, because the stub has no watcher.

## D-019 VERIFY (§4.7) status

- **OneDrive for Business / SharePoint:** rclone documents that these may modify Office files on upload. Drives whose `drive_type` is `business` or `documentLibrary` therefore compare by size and time only (`hash_type = None`, set in `probe_drive`). Not yet confirmed against a live tenant; the live smoke test (HUMAN_TODO) covers it.
- **Google Docs:** not built on yet. The planned approach is Drive's `export_formats` link types (`url` on Windows, `webloc` on macOS) so Docs appear as read-only link files. rclone reports their size as -1, which the engine already treats as "unknown size". Confirming this needs a live Drive account (layer 4), so it stays open and nothing depends on it in Phases 0–3.

## D-020 Property-test model: what a placeholder stands for

The no-loss invariant exempts content the user overwrote on their own side. Writing over a placeholder overwrites the file that placeholder stands for. The model identifies that file by the version the engine last recorded at the path (matched by content hash), not by the model's own last view of the cloud. The difference showed up in a Windows CI seed (now kept in `property.proptest-regressions`), where the engine had uploaded and then freed a file without a poll in between. Neither this nor the earlier model fixes changed what counts as loss of data the user didn't overwrite.

## D-021 Phase 4 spike: `cloud-filter` 0.1.0, and what Windows actually does

The spike used `cloud-filter` 0.1.0 (MIT, Nov 2026, on `windows` 0.62) instead of raw `windows` bindings. It is no longer the stale 0.0.x the plan worried about. It wraps registration (WinRT and native), callbacks, tickets, placeholders, pin state, in-sync state and a `ReadDirectoryChangesW` watcher. The crate's MSRV is 1.93, so only `synclone-provider-windows` raises `rust-version`. A throwaway example on the Windows 11 PC observed:

- Registering a sync root under `target/` works. Windows refuses roots under `%TEMP%`, so layer 3 tests live in `target/tmp`.
- File attributes: a dehydrated file is `OFFLINE | RECALL_ON_DATA_ACCESS` (0x401000 among others), pinned is 0x80000 and unpinned is 0x100000. A partially populated folder also carries `RECALL_ON_DATA_ACCESS`. Hydration leaves the mtime as the placeholder's.
- Callbacks (fetch placeholders, delete, rename) fire only for other processes. The provider's own renames, deletes and enumerations raise none; the watcher still reports them. So the provider's own operations need no echo suppression on the callback side, and its scans never make Windows fill a folder.
- The sync root itself gets `FETCH_PLACEHOLDERS` first, so drive folders are served there as placeholders.
- A shell "send to Recycle Bin" of a placeholder folder arrives as DELETE, undelete, DELETE on the folder alone, with no per-child callbacks.
- `attrib +U` produces no dehydrate callback: the provider must free files itself.
- Handles: `CfConvertToPlaceholder` and metadata updates reject `CfOpenFileWithOplock` handles ("The handle is invalid"). An attribute-only Win32 handle works for convert, update and hydrate, but it also lets a dehydrate succeed while another program reads the file. Anything that drops content therefore uses an exclusive handle (read/write with no sharing, or an exclusive oplock handle); both fail while another program has the file open. Identity, metadata and in-sync changes use attribute-only handles, which never hydrate and never conflict.

## D-022 Windows layout and how user actions map to the engine

- **One sync root:** provider name `Synclone`, account `default`, display name `Synclone`, full hydration, full population, pinning allowed, no auto-dehydration. Explorer's navigation pane comes from the WinRT registration. A different account (`SYNCLONE_SYNC_ROOT_ACCOUNT`) gives tests their own root, so they never touch the user's.
- **Drive folders and subfolders:** each drive folder is an on-demand placeholder folder directly in the root. `FETCH_PLACEHOLDERS` is answered from the state DB. If the folder was never indexed, the callback emits `FolderEnumerated` and waits (up to 20 s) for the engine's listing; a second pass after completion adds anything indexed meanwhile. `is_populated` is "not a partial placeholder folder", so the engine creates placeholders only where Explorer has looked (§4.2).
- **Placeholder identity:** each placeholder's blob is `{d: drive, s: size, m: mtime}`. The DB stays the source of truth.
- **Deletes** are allowed locally. The `deleted` notification becomes `LocalDeleted`, and the engine queues the trash entry (§4.6). A drive folder deleted, or moved out of the root, is checked 1.5 s later (after the Recycle Bin's delete/undelete/delete); if it is gone, that is `DriveRootRemoved`.
- **Renames** inside a drive become `LocalRenamed`. A drive folder renamed to a usable name not used by another drive becomes `DriveRootRenamed`. Moving a placeholder or a folder out of its drive is refused, because it would arrive without content. A downloaded file may leave its drive: that is a delete there and a new file wherever it lands.
- **Pins:** a pin attribute change seen by the watcher (compared with the last one seen, and with the provider's own pin changes recorded first) becomes `PinRequested` (folders recursive, dehydrated files) or `FreeRequested`. Every other watcher path is `LocalModified`, and the engine re-derives what happened.
- **Dehydrate callbacks** (Storage Sense, other programs) are refused unless the file is marked unpinned by the user: downloads stay.

## D-023 Windows hydration: two paths into one placeholder

- **A program opens a placeholder:** `FETCH_DATA` streams the object from rclone's `--rc-serve` (`RcClient::open_read`) in 1 MiB chunks with `CfReportProviderProgress`, so Explorer shows native progress and the 60 s timeout never fires. If the cloud's size differs from the placeholder's, the open fails cleanly and the engine updates the placeholder (rule 1). Then a new `Hydrated` event tells the engine, which records a downloaded copy of the version on record without transferring again, and pins it. The provider has its own two-thread runtime and rc client, so hydration never waits for a free engine worker.
- **The engine downloads** (save to device, rule 2, conflicts): `copyfile` to staging as on every OS, with size and hash checks. `commit_download` then turns the target into a dehydrated placeholder with the new size and time. A downloaded copy is dropped only through an exclusive handle, after re-checking the fingerprint. The provider then calls `CfHydratePlaceholder`, and its own `FETCH_DATA` serves the staged file. Windows writes the content, and the file keeps its identity.
- **Marking in sync:** `set_state(Downloaded)` marks in sync only if the file still matches the engine's recorded fingerprint, so a write during an upload stays pending in Explorer as well. A file still open in another program is retried in the background.
- **Is it open:** a read open that shares only reading, with `FILE_FLAG_OPEN_NO_RECALL`. It fails with a sharing violation while anyone has the file open for writing. Placeholders are never opened.
- **Remote deletes (rule 6):** a downloaded copy is reverted to an ordinary file and sent to the Recycle Bin (`SHFileOperationW`, `FOF_ALLOWUNDO`). Placeholders are simply removed.

## D-024 Daemon provider selection

`SYNCLONE_PROVIDER` selects `native` (the Windows provider; the default on Windows) or `stub` (plain folders; the default elsewhere until Phase 6). The scan loop runs every 60 s with the native provider, as a safety net behind OS events, and every 5 s with the stub. The CLI end-to-end tests set `stub`: they assert the stub's plain folders and use `%TEMP%`, where Windows refuses sync roots. The Windows provider has its own layer 3 suite, and the Windows kill-and-restart test runs through the CLI.

## D-025 A rename runs alone on its paths

Found by the Windows layer 3 suite (about 1 run in 3). Windows reports a rename as a rename (the move op) and, through the watcher, as a change at the old and new names (reconcile ops). `run_due` runs one batch concurrently with only same-path deduplication. A reconcile of the old name could start while the move op was already `running`. `pending_move_involving` only counts `pending` moves, so the renamed item looked deleted and went into the trash queue, even though the cloud rename succeeded. Emptying the trash would then have found nothing to delete, but the entry was wrong. `run_due` now holds back any non-move op covered by a move in the same batch or one still running; it runs next round, once the DB says where the item is. The guard inside `reconcile` stays `pending`-only, because the move op reconciles its own destination. Regression test: `rename_reported_together_with_a_change_at_the_old_name_is_not_a_delete`. It needed `MockRclone::set_latency` so that operations really interleave, as they do against a real cloud.
