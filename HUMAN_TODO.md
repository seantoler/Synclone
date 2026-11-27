# Things that need a human

Each item has exact steps. Items are removed once done.

## 1. Create Synclone's own Google OAuth client (before public release)

rclone's shared Google Drive client ID is being retired during 2026 (DECISIONS.md D-007). Until Synclone has its own, Drive sign-in uses the shared one and may stop working.

1. Go to https://console.cloud.google.com/ and create a project named "Synclone".
2. APIs & Services → Library → enable **Google Drive API**.
3. APIs & Services → OAuth consent screen → External; app name "Synclone"; add the scope `https://www.googleapis.com/auth/drive`.
4. Credentials → Create credentials → OAuth client ID → **Desktop app**. Note the client ID and secret.
5. Do not commit them to the repo. Tell the agent they exist; it will wire them into the wizard as build-time configuration (desktop OAuth secrets are not confidential, but keep them out of git history until the approach is decided).
6. Submit the app for Google verification (the Drive scope is restricted). This can take weeks.

## 2. Start Phase 4 on the Windows 11 PC

Phases 0–3 are done in the cloud. Phase 4 needs real NTFS and Explorer (PLAN.md §8).

1. Install Rust (https://rustup.rs, MSVC toolchain), Git for Windows, and Claude Code, in native Windows (PowerShell), **not WSL**.
2. Clone the repo, then in Git Bash run `scripts/fetch-rclone`.
3. Run `cargo test --workspace` to confirm a green baseline on the PC.
4. Start Claude Code in the repo and ask it to continue with Phase 4 (it reads CLAUDE.md, PROGRESS.md and DECISIONS.md first).

## 3. Live test remotes for layer 4 (when convenient; not blocking)

For each of Google Drive, OneDrive and Drime:

1. Create a folder named `_synclone-test` at the top of the account (web UI).
2. Create an rclone remote for the account if you don't have one (`rclone config`). Name it e.g. `gdrive`.
3. Create an **alias** remote pointing at the test folder, e.g. `rclone config create gdrive-test alias remote=gdrive:_synclone-test`.
4. Repeat for `onedrive-test` and `drime-test`. For Drime, first create an API access token in the Drime web control panel and use it for the `drime` remote (`rclone config create drime drime access_token=<token>`).
5. Tell the agent the alias names. It will run the smoke tests with `SYNCLONE_LIVE_TEST_REMOTES=gdrive-test,onedrive-test,drime-test`. The harness refuses any remote that is not an `alias` into `_synclone-test`.

These also settle the two open live checks: Google Docs as link files, and OneDrive for Business file mutation (DECISIONS.md D-019).
