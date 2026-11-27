# Synclone — Build Plan

An open-source, odrive-style multi-cloud sync client for Windows and macOS, written in Rust on top of rclone.

This document is the complete brief for Claude Code. It is written so an agent can build and test the project with minimal human involvement. Items marked **VERIFY** are facts the planning session could not confirm and the agent must check before relying on them.

---

## 1. Product definition

### 1.1 What the user sees

| Behavior | Requirement |
|---|---|
| One app folder, one subfolder per cloud | A single app folder appears in Explorer / Finder (as odrive does). Each cloud storage is a top-level folder inside it. |
| Placeholders | Every remote file and folder is visible without taking disk space. Native OS placeholders (cloud-badge icons), not `.cloud` stub files. |
| Open to download | Double-clicking a placeholder downloads it and opens it. |
| Downloads stay | A downloaded file stays on the device until the user explicitly frees it. No automatic eviction by default. |
| Save to device | Right-click a file or folder → download it. On a folder this is recursive: all files, subfolders and their files. |
| Remove from device | Right-click → free the local copy. The cloud copy is untouched. Works recursively on folders. |
| Auto upload | Files and folders dropped, created, edited, renamed or moved locally sync to the cloud automatically. |
| Auto download of changes | Remote changes to files that are downloaded locally are applied, subject to the conflict rules in §5. |
| Conflict safety | A file that is open or has unsynced local edits is never overwritten by a remote change. |
| Queued deletes | Deleting locally does NOT delete from the cloud. It adds an entry to the app's trash queue. Cloud deletion happens only when the user chooses "Empty trash" in the tray / menu bar menu. Entries can be restored. |
| Unmount, not delete | Removing a cloud drive (or deleting its top-level folder) only disconnects it. It never deletes anything in the cloud. |
| Simple setup | "Add cloud storage" wizard with plain language. rclone terms ("remote", "backend", "mount") stay out of the default UI; they are allowed only in advanced settings and explanatory notes (§6.4). |
| Existing rclone remotes | Remotes already in the user's rclone config appear automatically as cloud drives. |

### 1.2 Priority providers

Google Drive, OneDrive, Drime. Everything else rclone supports is available through a generic "Other" path in the wizard.

- Google Drive and OneDrive use rclone's browser OAuth flow.
- Drime uses an API access token pasted by the user (created in the Drime web control panel). Drime support requires **rclone ≥ 1.73**.

### 1.3 Users

1. The owner (Windows 11 PC, MacBook Air M4).
2. One friend who uploads large audiobook files monthly. Large-file upload reliability matters.
3. Later: the public. The project will be free and open source.

### 1.4 Non-goals for v1

Encryption UI, share links, backup jobs, sync-to-arbitrary-folder, auto-unsync timers, Linux native integration, mobile, auto-update. These are in the backlog (§11).

### 1.5 Naming and legal

- Do not use the name "odrive", its icons, or any of its code. The app name is "Synclone"; keep the name in one constant so it can be renamed in a single commit.
- Do not put "rclone" in the product name: it is reported to be a trademark of rclone's author. Say "built on rclone" and "not affiliated with rclone" in the README.
- License: MIT OR Apache-2.0 (dual, Rust convention). rclone is MIT; bundle its license notice.

### 1.6 Prior art (checked November 2026)

No existing open-source project covers the whole goal (Windows + macOS, native placeholders, odrive-style delete queue, plain-language setup). Two cover half each and are useful references:

| Project | Platform | What it does | Gaps versus this plan | License |
|---|---|---|---|---|
| OpenCfDrive (`dkgs2000/OpenCfDrive`) | Windows | rclone remote as a Cloud Files API drive via `rclone rcd --rc-serve`; pin / free up space; two-way sync with keep-both conflicts; tray icon | One remote; remotes must be created with the rclone CLI; deletes go straight to the server; .NET; very new | MIT |
| Nimbus (`CasaVargas/Nimbus`) | macOS 15+ | rclone remotes in Finder via a File Provider extension; guided add-remote flow; menu bar panel | macOS only; no delete queue described; Swift | GPL-3.0-or-later |

Also relevant: Cascade (`ExaDev/cascade`, Rust, File Provider shim talking to the engine over a Unix socket), which supports the IPC design in §3.2.

Rules for the agent: read these for design ideas and known pitfalls (their issue trackers are valuable, e.g. Nimbus issue #180 on synchronous rc calls being cut off by timeouts: always run long rc operations as async jobs). MIT code may be adapted with attribution. Do **not** copy GPL code into this MIT/Apache project.

---

## 2. Architecture

### 2.1 Overview

```
┌────────────────────┐   ┌─────────────────────────┐
│ Tray / menu bar UI │   │ Explorer / Finder        │
│ (Tauri 2)          │   │                          │
└─────────┬──────────┘   └────────────┬────────────┘
          │ IPC (JSON-RPC)            │ OS callbacks
          │                 ┌─────────┴───────────────┐
          │                 │ Platform provider        │
          │                 │  Windows: Cloud Files API│
          │                 │  macOS: File Provider ext│
          │                 └─────────┬───────────────┘
          ▼                           ▼
┌──────────────────────────────────────────────────┐
│ Daemon (Rust)                                     │
│  core: sync engine, state DB, conflict logic,     │
│        trash queue, drive manager                 │
└──────────────────────┬───────────────────────────┘
                       │ HTTP on 127.0.0.1 (authenticated)
              ┌────────┴────────┐
              │ rclone rcd      │  bundled binary, supervised child process
              └─────────────────┘
```

### 2.2 Why not `rclone mount`

`rclone mount` has no concept of "keep this file until I say otherwise", no delete queue, and no native placeholder icons. The app uses rclone only as a storage API (`rclone rcd`) and implements the placeholder layer itself with the native OS sync-provider APIs.

### 2.3 Cargo workspace

| Crate | Purpose | Platforms |
|---|---|---|
| `synclone-core` | Domain types, state DB (SQLite via `rusqlite`), sync engine, conflict resolution, trash queue, `Provider` trait. No OS-specific code. | all |
| `synclone-rclone` | Spawn and supervise `rclone rcd`; typed async client for the rc API; `RcloneApi` trait with a mock implementation. | all |
| `synclone-daemon` | Long-running process. Hosts core, owns the DB, exposes JSON-RPC over a named pipe (Windows) or Unix socket (macOS/Linux). | all |
| `synclone-cli` | `synclonectl`: every user-facing operation as a command (`drive add/list/remove`, `download`, `free`, `trash list/restore/empty`, `status`, `conflicts`). Exists mainly so the agent can test everything headlessly. | all |
| `synclone-provider-stub` | A `Provider` using plain folders and zero-byte `.sclstub` files. Used for engine tests on Linux and CI. Not shipped to users in v1. | all |
| `synclone-provider-windows` | Cloud Files API sync root provider. | Windows |
| `synclone-provider-macos` | Rust side of the File Provider integration, plus a Swift app-extension shim in `macos/`. | macOS |
| `synclone-app` | Tauri 2 tray / menu bar app and windows (wizard, trash, activity, settings). | Windows, macOS |

### 2.4 The `Provider` trait (the key abstraction)

The sync engine talks to the OS only through this trait, so 90% of the logic is testable on Linux.

```rust
trait Provider {
    fn register_drive(&self, drive: &Drive) -> Result<()>;
    fn unregister_drive(&self, drive: &DriveId) -> Result<()>;   // never touches remote data
    fn upsert_placeholders(&self, drive: &DriveId, parent: &RelPath, items: &[ItemMeta]) -> Result<()>;
    fn remove_local(&self, drive: &DriveId, path: &RelPath) -> Result<()>;
    fn set_state(&self, drive: &DriveId, path: &RelPath, state: LocalState) -> Result<()>; // Placeholder | Downloaded | Pinned
    fn is_open(&self, drive: &DriveId, path: &RelPath) -> Result<bool>;
    fn events(&self) -> Receiver<ProviderEvent>;
}

enum ProviderEvent {
    HydrationRequested { .. },   // user opened a placeholder
    PinRequested { recursive },  // "save to device"
    FreeRequested { recursive }, // "remove from device"
    LocalCreated, LocalModified, LocalRenamed, LocalDeleted,
    FolderEnumerated { .. },     // user browsed into a folder
    DriveRootRemoved,            // top-level folder deleted → treat as unmount
}
```

### 2.5 rclone integration

- Bundle a pinned rclone binary (≥ 1.73). A `scripts/fetch-rclone` script downloads it from GitHub releases and verifies the SHA-256.
- Start `rclone rcd` bound to `127.0.0.1` on a random free port with a random per-launch username and password (`--rc-user`, `--rc-pass`). Authentication is mandatory: the rc API can execute OS commands, so it must never be reachable unauthenticated.
- Use the user's existing default rclone config file in place (find it with `rclone config file`). Remotes added in the app are visible to the rclone CLI and vice versa. Tests always override this with `RCLONE_CONFIG=<tempfile>`.
- rc endpoints expected to be needed (**VERIFY** each against `rclone rc --help` output of the pinned version):
  `config/listremotes`, `config/get`, `config/create`, `config/update`, `config/delete`, `config/providers`, `operations/list`, `operations/stat`, `operations/copyfile`, `operations/movefile`, `operations/deletefile`, `operations/mkdir`, `operations/rmdir`, `operations/purge`, `operations/about`, `core/stats`, `core/version`, `job/status`, `job/stop`.
- Long operations run with `_async=true` and are tracked through `job/status` and `core/stats` for progress.
- Streaming downloads: **VERIFY** that `--rc-serve` exposes remote objects over HTTP with Range support. If it does, use it for chunked hydration with progress on Windows. Otherwise download with `operations/copyfile` into a staging directory, then hand the data to the OS.
- Remote change detection is by polling. The rc API does not expose push change notifications. See §4.3.

### 2.6 State database (SQLite)

Location: per-user app data directory. One DB, WAL mode.

- `drives(id, remote_name, display_name, root_path, provider_type, added_at, paused)`
- `items(drive_id, path, parent_path, is_dir, remote_id, remote_size, remote_mtime, remote_hash, local_state, synced_size, synced_mtime, synced_hash, dirty, last_seen_remote)`
- `trash(id, drive_id, path, is_dir, deleted_at, remote_snapshot_json, status)`
- `ops(id, kind, drive_id, path, payload_json, state, attempts, last_error, created_at)`: durable job queue; every upload, download, move and delete is journaled so a crash resumes cleanly.
- `conflicts(id, drive_id, path, conflict_copy_path, detected_at, resolved)`
- `settings(key, value)`

`synced_*` columns hold the last state known to be identical on both sides. They are the basis of all conflict decisions.

---

## 3. Platform integration

### 3.1 Windows: Cloud Files API

The same API OneDrive, Google Drive and Dropbox use.

- One sync root for the whole app at `%USERPROFILE%\Synclone`, registered so it shows once in the Explorer navigation pane with the app icon. Each cloud drive is a top-level folder inside it (`Synclone\Google Drive`, `Synclone\OneDrive`, …). The engine maps the first path segment to a drive.
- Files cannot live directly in the app root; only cloud-drive folders. A file dropped there is left unsynced and flagged in the tray menu.
- Rust bindings: start with the `cloud-filter` crate (a wrapper over the Cloud Filter API). It is at version 0.0.x and may be incomplete or stale. **Spike it first.** Fallback: call `CfRegisterSyncRoot`, `CfConnectSyncRoot`, `CfCreatePlaceholders`, `CfExecute` etc. directly through the `windows` crate.
- Hydration: `FETCH_DATA` callback → stream bytes from rclone → `CfExecute(TRANSFER_DATA)`, with progress reported so Explorer shows its native progress UI.
- Folder population: on-demand (`FETCH_PLACEHOLDERS`), answered instantly from the state DB. See §4.2.
- Context menu: use the built-in Explorer entries **"Always keep on this device"** and **"Free up space"**. They need no shell extension. They set the pinned / unpinned file attributes; the provider watches for attribute changes and reacts:
  - pinned on a folder → download everything beneath it, recursively, and mark new children pinned as they appear;
  - unpinned → dehydrate (recursively for folders), unless the file has unsynced changes.
- "Downloads stay": mark every hydrated file as pinned so Windows Storage Sense never dehydrates it automatically. (Setting: allow Storage Sense to manage space, off by default.)
- Local change detection: the Cloud Files API does not report content edits. Run a `ReadDirectoryChangesW` watcher on each sync root (the `notify` crate) and reconcile with the in-sync state.
- Local delete: handle the delete notification by allowing the local delete, writing a `trash` row, and suppressing re-creation of the placeholder while the row is pending.
- A cloud drive's top-level folder deleted by the user → `DriveRootRemoved` → treat as disconnect: remove the local folder, mark the drive disconnected, keep its sign-in so it can be re-added in one click, write nothing to the trash queue, issue zero remote calls. Renaming a top-level folder renames the drive's display name only.
- Status icons: use the native in-sync / syncing / error states. No custom icon overlays.
- A custom top-level right-click entry is not needed for v1, which avoids the Windows 11 packaged-app requirement.

### 3.2 macOS: File Provider (`NSFileProviderReplicatedExtension`)

The same API OneDrive, Google Drive and Dropbox use on modern macOS.

- One `NSFileProviderDomain` for the whole app. macOS decides its location (`~/Library/CloudStorage/Synclone`) and shows it once in the Finder sidebar under Locations. Each cloud drive is a top-level folder inside the domain, exactly as on Windows. Item identifiers are `<drive-id>/<remote-id-or-path>`.
- Deleting a top-level drive folder in Finder is handled as disconnect (same rules as Windows), never as a remote delete.
- Structure: a host `.app` containing a File Provider app extension (`.appex`) written as a thin Swift shim. All logic lives in Rust.
  - Preferred: the shim forwards each callback (`item`, `enumerator`, `fetchContents`, `createItem`, `modifyItem`, `deleteItem`) to the daemon over a Unix socket in the shared App Group container. **VERIFY** early that a sandboxed extension can connect to that socket.
  - Fallback: link `synclone-core` into the extension as a static library through a C ABI (`extern "C"`), with the DB in the App Group container.
- File hand-off: `fetchContents` must return a file URL on the same volume. Download into a staging directory inside the App Group container and return that URL. For uploads the system provides a temporary URL; copy or clone it into staging before the callback returns, then upload from staging.
- Context menu: Finder supplies **"Download Now"** and **"Remove Download"** for File Provider items, including on folders (recursive). Map these to save-to-device and remove-from-device.
- "Downloads stay": **VERIFY** which content policy keeps materialized files from being evicted under disk pressure on the target macOS versions (look at `NSFileProviderContentPolicy` and "Keep Downloaded"). Apply it to everything the user downloads.
- Local changes arrive as extension callbacks; no file watcher is needed.
- Local delete: `deleteItem` → write a `trash` row and report success. Do not advertise Trash-container support, so Finder deletes go straight to this path.
- Removing a drive: remove its top-level folder from the domain's enumeration. Before doing so, offer to copy downloaded files to a local "Recovered" folder. Never call rclone delete.
- Requirements: paid Apple Developer account, App Group, File Provider entitlements, provisioning profiles for app and extension, Developer ID signing and notarization for distribution. Development builds can use the File Provider testing-mode entitlement.
- Host app: try bundling the `.appex` into the Tauri app bundle's `Contents/PlugIns` with a post-build step and re-signing. If that fights the toolchain, use a small Swift host app with an `NSStatusItem` menu that talks to the same daemon. Decide in the Phase 6 spike and record it in `DECISIONS.md`.

### 3.3 Minimum OS versions (assumption)

Windows 10 22H2 and Windows 11. macOS 14 or later. Apple Silicon and Intel (universal binary).

---

## 4. Sync engine

### 4.1 Item states

`Placeholder` → `Downloading` → `Downloaded` (kept until freed) ⇄ `Dirty` (local changes waiting to upload) → `Uploading` → `Downloaded`.
A folder is `Pinned` when the user chose save-to-device on it; new remote children of a pinned folder download automatically.

### 4.2 Folder listing strategy

The requirement: the whole folder tree should feel instantly available, without slow loads or wasted space.

- The OS asks for a folder's children only when the user browses into it. Those requests are answered from the state DB, so they are instant once indexed.
- A background crawler indexes each drive breadth-first after it is added (recursive `operations/list`, using fast-list where the provider supports it), then keeps it fresh by polling.
- If the user opens a folder the crawler has not reached, list it live and jump it to the front of the queue.
- Cost: a few hundred bytes per item in SQLite, and roughly 1 KB per placeholder on disk only for folders actually browsed. A drive with 100,000 files needs tens of MB of index.

### 4.3 Remote change detection

- Per-drive poll interval, default 5 minutes, configurable; manual "Refresh" in the tray menu and on folder open (if the cached listing is older than 30 seconds).
- Each poll diffs the remote listing against `items` using provider ID where available, otherwise path + size + mtime (+ hash where cheap).
- Detect renames/moves by remote ID when the provider exposes one, to avoid delete + re-download.
- Back off on rate-limit errors. Respect per-provider API limits.

### 4.4 Uploads

- Debounce local changes (default 3 s of quiet).
- Do not upload a file that is open for writing. Retry when it closes. (Windows: attempt an open that denies write sharing. macOS: the system generally calls `modifyItem` after the writer finishes.)
- Before every upload, re-stat the remote object and compare with `synced_*`. If the remote changed since the last sync, go to conflict handling instead of overwriting.
- Upload via `operations/copyfile` from the local path. rclone handles chunking and retries for multi-GB files. A failed upload stays in `ops` and is retried with backoff; it is never dropped silently.
- Ignore list (never uploaded): `~$*`, `.~lock.*`, `*.tmp`, `*.partial`, `*.crdownload`, `.DS_Store`, `._*`, `desktop.ini`, `Thumbs.db`, `Icon\r`. User-extendable.

### 4.5 Downloads

- Triggered by opening a placeholder, by save-to-device, or by a remote change to a file that is `Downloaded`.
- Concurrency limit (default 4), bandwidth limit setting passed through to rclone (`core/bwlimit`).
- Verify size (and hash where available) before marking `Downloaded`.

### 4.6 Trash queue

- Local delete of a synced item → `trash` row with a snapshot of the remote metadata. Nothing is sent to the cloud.
- While a row is pending, the item is hidden locally (its placeholder is not recreated by polling).
- Tray menu shows "Trash (N)". Trash window: list, Restore selected, Restore all, **Empty trash and delete from cloud** (with confirmation showing the count and total size).
- Emptying uses `operations/deletefile` / `operations/purge`, which sends items to the provider's own trash where the provider has one. Do not pass any hard-delete flag.
- If the remote copy changed after the local delete, do not delete it. Restore its placeholder and report it.
- Deleting a local file that was never uploaded is a plain local delete (it goes to the OS Recycle Bin / Trash as usual).

### 4.7 Special cases

- Google Docs/Sheets/Slides have no real file content. v1: show them as link files that open in the browser, read-only. (**VERIFY** the best rclone Drive option for this, e.g. export formats set to link files.)
- OneDrive for Business / SharePoint can alter Office files on upload, changing size and hash. For those remotes compare by mtime + size with tolerance, not hash. (**VERIFY** current rclone guidance.)
- Names illegal on Windows (`:`, `?`, trailing dots, reserved names) and case-only collisions: do not sync the item; surface it in an "Unsyncable items" list. Never rename remote files automatically.
- Wrapped rclone remotes (crypt, alias, union, combine) appear as drives like any other.

---

## 5. Conflict rules

Principle: **no user data is ever overwritten or lost without an explicit user action.** When in doubt, keep both copies.

| # | Situation | Action |
|---|---|---|
| 1 | Remote changed; local copy is a placeholder | Update placeholder metadata. |
| 2 | Remote changed; local downloaded, clean, not open | Replace local with the new remote version. |
| 3 | Remote changed; local downloaded, clean, but open | Defer. Retry when closed. Then re-evaluate (it may now be dirty → rule 4). |
| 4 | Remote changed AND local has unsynced edits | Keep both. Upload the local version as `name (conflict - <device> <YYYY-MM-DD HH.MM>).ext`, then make the remote version the canonical `name.ext`. Record in `conflicts`, notify the user. |
| 5 | Local edit; remote unchanged | Upload. |
| 6 | Remote deleted; local clean | Remove the local copy (downloaded files go to the OS Recycle Bin / Trash, not permanent delete). |
| 7 | Remote deleted; local dirty | Keep local and upload it again as a new file. |
| 8 | Local deleted (in trash queue); remote changed afterwards | Cancel the trash entry, restore the placeholder, notify. |
| 9 | Same new filename created on both sides with different content | Treat as rule 4. |
| 10 | Local rename/move; remote changed content | Apply the rename remotely (server-side move), then rule 2. |
| 11 | Both sides renamed the same item differently | Remote name wins for the original; no data is affected; notify. |

Device name in conflict copies comes from the hostname. Conflicts are listed in the tray menu ("Conflicts (N)") with "Show in folder".

---

## 6. User interface

### 6.1 Tray / menu bar menu

```
Synclone — Up to date            (or: Syncing 3 items… / Paused / Attention needed)
────────────────────────────────
Google Drive            ▸  Open folder · Refresh · Pause · Disconnect…
OneDrive                ▸
Drime                   ▸
────────────────────────────────
Add cloud storage…
────────────────────────────────
Trash (4)…
Conflicts (1)…
Recent activity…
────────────────────────────────
Pause all syncing
Settings…
Quit
```

### 6.2 Add cloud storage wizard

1. Pick a service: large tiles for Google Drive, OneDrive, Drime; then a searchable list of the rest (names and descriptions from `config/providers`).
2. Sign in:
   - OAuth services: "Sign in with your browser" → rclone's built-in browser flow.
   - Token services (Drime): a paste field with a "Where do I find this?" link to the provider's token page.
   - Other: a form generated from the provider's required options only; an "Advanced" expander for the rest.
3. Name it (default: the service name; deduplicate with " 2").
4. Done → the folder opens in Explorer / Finder.

Vocabulary for the default UI: "cloud storage", "cloud drive", "cloud", "sign in", "disconnect", "save to device", "remove from device". Do not use "remote", "backend", "mount", "VFS" or "hydrate" in anything a non-technical user sees by default. See §6.4 for where rclone terms are allowed.

### 6.3 Other windows

Trash, Conflicts, Recent activity (last 200 operations with errors expandable), Settings (start at login, poll interval, bandwidth limits, ignore list, where drives live on Windows, show/hide imported drives, diagnostics export).

### 6.4 rclone terminology and advanced mode

The plain-language rule protects non-technical users; it is not a ban.

- **Allowed in v1:** rclone terms in clearly secondary places where they clarify what the app is doing: a "What does this do?" note in the wizard (e.g. "This creates an rclone remote named `gdrive`"), the drive's details panel (showing the underlying remote name and type), logs, diagnostics, docs and the CLI.
- **Advanced mode (post-v1, not built initially):** an opt-in switch in Settings that exposes the full rclone feature set: all provider options in the wizard, per-drive rclone flags, wrapped remotes (crypt, union, alias, combine) creation, and raw config editing. rclone vocabulary is used freely there.
- **Precedence rule, applies always:** the app's own placeholder, sync, conflict and delete-queue behavior takes precedence over rclone's equivalents. Advanced mode never exposes `rclone mount`, VFS cache settings, `sync`/`bisync` jobs or anything else that would bypass or conflict with the sync engine on a connected drive. Flags that could cause data loss on a connected drive (e.g. hard-delete options) are rejected.
- **Design for it now:** keep the wizard's form generator driven by `config/providers` metadata, and store per-drive extra options in the DB, so advanced mode is a UI addition later rather than a rework.

### 6.5 Disconnect

Confirmation text: "This removes the <name> folder from this computer. Nothing is deleted from <service>." If uploads are pending, list them and block until done or the user chooses to keep those files in a local "Recovered" folder.

---

## 7. Testing strategy

The goal is that the agent can prove correctness without a human clicking through the app.

### Layer 1: unit tests (all platforms, including Linux cloud sessions)

Core logic against the mock `RcloneApi` and an in-memory provider. Every row of the conflict table in §5 has at least one named test.

### Layer 2: engine integration tests (all platforms)

Real `rclone rcd` against rclone's `local` backend pointed at temp directories, with `synclone-provider-stub`. Covers: add/remove drive, index, download, pin/free recursion, upload, rename/move, trash queue lifecycle, crash-and-resume from the `ops` journal, rate-limit backoff (injected faults).

Property test (`proptest`): apply random interleaved operations to the "local" and "remote" sides, run the engine to quiescence, and assert the invariant: **every distinct file content that ever existed is still present locally, remotely, or as a conflict copy, unless it was removed by an explicitly emptied trash entry.**

### Layer 3: platform end-to-end tests

- **Windows:** a test harness registers a real sync root backed by a `local` rclone remote and drives it with ordinary file APIs: read a placeholder (forces hydration), `attrib +P` / `attrib +U` (the pin and unpin attributes Explorer's menu sets), write, rename, delete. Assertions read file attributes and placeholder state. **VERIFY** whether the Cloud Files API works on GitHub's `windows-latest` runners; if not, these run only on the developer PC.
- **macOS:** drive the extension with `fileproviderctl` and `NSFileProviderManager` calls (materialize, evict, enumerate) from a test helper. Requires a signed build, so runs on the developer Mac only.

### Layer 4: live cloud smoke tests (opt-in)

- Run only when `SYNCLONE_LIVE_TEST_REMOTES` is set.
- Two independent barriers protect the owner's real files:
  1. Live tests never use the real remote directly. For each provider the owner creates a folder `_synclone-test` and an rclone **alias** remote pointing at it (e.g. `gdrive-test` → `gdrive:_synclone-test`). rclone cannot address anything above an alias remote's root, so nothing outside that folder is reachable. `SYNCLONE_LIVE_TEST_REMOTES` may only list remotes whose type is `alias` and whose target path contains `_synclone-test`; the harness refuses anything else.
  2. Every path also passes through a `TestRoot` guard in code that refuses operations outside a per-run subfolder.
- One round trip per provider: upload, list, download, modify, conflict, trash, empty, cleanup.

### Layer 5: UI

Windows are thin views over daemon RPC. Test the RPC layer thoroughly; cover the wizard with `tauri-driver` WebDriver tests on Windows. A short manual checklist (`docs/MANUAL_CHECKLIST.md`) covers what automation cannot: tray icon appearance, Explorer/Finder icons, the OAuth browser hand-off.

### CI

GitHub Actions on `ubuntu-latest`, `windows-latest`, `macos-latest`: fmt, clippy with warnings denied, layers 1–2 everywhere, layer 3 where it works unsigned.

---

## 8. Where and how to build

| Phase | Where | Why |
|---|---|---|
| 0–3 (core engine, CLI) | Claude Code **cloud session** | Platform-neutral Rust; everything is testable on Linux. Fully hands-off. |
| 4–5 (Windows provider, tray app) | Claude Code **locally on the Windows 11 PC**, in native Windows (PowerShell), **not WSL** | The Cloud Files API needs real NTFS and Explorer. Cloud sessions run Linux. |
| 6 (macOS) | Claude Code **locally on the MacBook** with Xcode | File Provider extensions need Xcode, signing and a real Finder. |
| 7–8 | Either local machine, CI for release builds | |

One GitHub repository ties the three environments together. Each session starts by reading `CLAUDE.md`, `PROGRESS.md` and `DECISIONS.md`.

Cloud-session network note: the sandbox may only reach package registries and GitHub. Fetch rclone from GitHub releases, not rclone.org.

---

## 9. Agent operating rules (put these in `CLAUDE.md`)

1. Work through the phases in §10 in order. A phase is complete only when all its exit criteria pass in CI or in a recorded local test run.
2. Do not stop to ask for approval of design choices. Make the choice, record it in `DECISIONS.md` with one paragraph of reasoning, and continue.
3. Keep `PROGRESS.md` current: phase, what is done, what is next, known failures. Update it at the end of every working session.
4. When something needs a human (list below), append it to `HUMAN_TODO.md` with exact steps, then continue with any work that is not blocked.
5. Small commits with descriptive messages. Never commit secrets, tokens, or rclone config files.
6. Never weaken, skip or delete a test to make a build pass. Fix the code, or record the failure honestly in `PROGRESS.md`.
7. Resolve every **VERIFY** item in this plan before building on it; record the finding in `DECISIONS.md`.
8. **Data safety, absolute rules:**
   - Tests always set `RCLONE_CONFIG` to a temp file. Never read, modify or print the user's real rclone config in tests.
   - Never run delete, purge, move or sync against a real cloud remote except through the `TestRoot` guard (§7 layer 4).
   - Never delete local files outside temp directories and the repository.
   - Unregistering a drive must issue zero remote write or delete calls. There is a test that asserts this by counting calls on the mock.
9. No telemetry. Logs stay local. A "diagnostics export" bundles logs with paths and tokens redacted.

**Needs a human** (cannot be automated):
- Creating the GitHub repository and connecting it to Claude Code.
- Apple Developer enrollment, certificates, App Group, provisioning profiles.
- Signing in to cloud accounts in a browser; creating a Drime API token.
- Approving OS prompts (enabling the File Provider extension, Windows SmartScreen on unsigned builds).
- Supplying live test remotes for layer 4.
- The manual checklist before each release.

---

## 10. Phases and exit criteria

### Phase 0: Bootstrap (cloud)
Workspace with all crates stubbed, `CLAUDE.md`, `PROGRESS.md`, `DECISIONS.md`, `HUMAN_TODO.md`, licenses, CI on three OSes, `scripts/fetch-rclone` with checksum verification.
**Exit:** CI green on all three; `synclonectl --version` runs; rclone binary fetched and `core/version` returns ≥ 1.73 through `synclone-rclone`.

### Phase 1: rclone client and drive management (cloud)
Supervised `rclone rcd` (random port, credentials, restart on crash, clean shutdown). Typed client and mock. Import existing remotes. Add/remove remotes, including the OAuth flow (state machine tested against the mock) and token providers.
**Exit:** `synclonectl drive list/add/remove` works against `local` and `memory` backends; rc is unreachable without credentials (test); all **VERIFY** items in §2.5 resolved.

### Phase 2: State DB and remote index (cloud)
Schema and migrations, background crawler, polling differ, rename detection by ID.
**Exit:** indexing a generated tree of 100,000 items completes and re-polls with bounded memory; diff tests cover add/modify/delete/rename; folder listing from the DB returns in under 50 ms.

### Phase 3: Sync engine with the stub provider (cloud)
Downloads, pin/free recursion, uploads with debounce and open-file deferral, the full conflict table, trash queue, durable `ops` journal, ignore list.
**Exit:** every §5 rule has a passing test; property test passes 10,000 cases; kill-and-restart test resumes all in-flight operations; `synclonectl` can perform every action in §1.1.

### Phase 4: Windows provider (Windows PC)
Spike `cloud-filter` vs. raw `windows` bindings first (one day's worth of effort at most, then decide). Sync root registration, placeholders, streaming hydration with progress, pin/unpin handling, watcher, delete interception, root-removal-as-unmount.
**Exit:** layer 3 Windows suite passes; a 5 GB file hydrates and uploads successfully against a `local` remote; Explorer shows correct status icons (screenshot saved to `docs/evidence/`).

### Phase 5: Tray app and wizard (Windows PC)
Tauri 2 app, tray menu, wizard, Trash/Conflicts/Activity/Settings windows, start at login, daemon lifecycle.
**Exit:** WebDriver tests pass for the wizard with a `local` provider; trash empty/restore works end to end from the UI layer; `HUMAN_TODO.md` contains the live sign-in checklist for Google Drive, OneDrive and Drime.

### Phase 6: macOS (MacBook)
Spikes first: (a) sandboxed extension ↔ daemon socket in the App Group container; (b) embedding the `.appex` in the Tauri bundle. Then the Swift shim, domain management, fetch/create/modify/delete, keep-downloaded policy, menu bar app.
**Exit:** layer 3 macOS suite passes locally on a development-signed build; Finder shows placeholders and "Download Now" / "Remove Download" work on files and folders; deleting in Finder creates a trash-queue entry and leaves the remote untouched.

### Phase 7: Packaging
Windows installer (Tauri NSIS/MSI) bundling rclone; macOS universal, Developer ID signed, notarized DMG; README with screenshots; release workflow in GitHub Actions.
**Exit:** clean-machine install on both OSes following only the README (manual checklist).

### Phase 8: Hardening and friend beta
Large-file soak (multi-GB audiobook uploads over a throttled connection), network drop and sleep/wake recovery, token expiry and re-sign-in flow, diagnostics export, error messages reviewed for plain language.
**Exit:** 72-hour soak with injected faults shows zero invariant violations; second user completes install and first upload unaided.

---

## 11. Backlog after v1

Advanced mode exposing full rclone options (§6.4); auto-free files not opened in N days; encrypted folders (rclone `crypt`) in the wizard; share links (`operations/publiclink`); sync any local folder to a cloud folder; backup jobs; per-folder selective pinning rules; auto-update; Linux desktop integration; localization.

---

## 12. Risks

| Risk | Mitigation |
|---|---|
| `cloud-filter` crate is pre-1.0 and may lag | Phase 4 spike; fall back to raw `windows` crate bindings. |
| macOS extension sandbox blocks the IPC design | Phase 6 spike (a); fall back to linking core into the extension. |
| Tauri bundle cannot host an `.appex` cleanly | Phase 6 spike (b); fall back to a small Swift host app. |
| Polling means remote changes are seen with delay | Refresh on folder open; configurable interval; document it. |
| Provider quirks (Google Docs, SharePoint file mutation, rate limits) | §4.7 handling; live smoke tests per provider. |
| A bug deletes cloud data | Deletes only via the explicit trash-empty path; call-count tests; `TestRoot` guard; provider-side trash used where available. |
| Unsigned Windows build triggers SmartScreen | Acceptable for v1; document it; consider code signing later. |

---

## 13. Open questions (defaults are assumed until answered)

Answered by the owner:
- The second user is on Windows. Windows ships first.
- The repository is public from day one.
- Remote deletes remove the local copy (rule 6), as odrive does.
- Live smoke tests run in a dedicated test folder in the real accounts, isolated as in §7 layer 4.
- All cloud drives live as folders inside one app folder, as odrive does.
- Final app name: **Synclone**.

Still open: none.
