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
