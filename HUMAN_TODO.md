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

## 3. Sign in to the three priority services once (Phase 5 exit, needs a browser and your accounts)

Run the real app against your real accounts. This uses your normal rclone config (`%APPDATA%\rclone\rclone.conf`), as the plan intends (§2.5), and creates `%USERPROFILE%\Synclone`.

1. `cargo build -p synclone-app`, then start `target\debug\Synclone.exe`. The tray icon appears, and the "Add cloud storage" window opens if no drive is connected yet. Existing rclone remotes appear as drives automatically.
2. **Google Drive:** wizard, then Google Drive, then "Sign in with your browser". Approve in the browser, keep the name, then Finish. The folder opens in Explorer; browse it, and open one file.
3. **OneDrive:** the same steps. If you have a work or school account, try it too (D-019).
4. **Drime:** in the Drime web control panel, create an API access token. Wizard, then Drime, paste the token, then Connect, then Finish.
5. Go through `docs/MANUAL_CHECKLIST.md` items 6–13 with one of these drives, preferably on a test folder.
6. Tell the agent what worked and what did not (screenshots welcome). Remove a drive afterwards with its tray submenu, then Disconnect…; that never deletes anything in the cloud.

## 4. Remove leftovers of agent test runs

A test written during the Phase 5 session wrote its log to the real app-data folder before it was fixed. `%LOCALAPPDATA%\Synclone` was created then (2026-11-27 16:40) and holds only `logs\daemon.log` (159 bytes). The agent may not delete outside temp folders and the repo, so please delete `%LOCALAPPDATA%\Synclone` yourself. It is recreated properly the first time you run the app.

Two layer 3 test runs left stale sync-root registry keys behind. WinRT no longer knows them ("Element not found") and their folders are gone. If Explorer shows a dead "Synclone test ..." entry in its navigation pane, remove them in an **administrator** PowerShell:

```powershell
Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager' |
  Where-Object { $_.PSChildName -like 'Synclone!*!test-*' } |
  Remove-Item -Recurse
```

Only keys whose account starts with `test-` are touched; the real app uses the account `default`.

## 5. Live test remotes for layer 4 (when convenient; not blocking)

For each of Google Drive, OneDrive and Drime:

1. Create a folder named `_synclone-test` at the top of the account (web UI).
2. Create an rclone remote for the account if you don't have one (`rclone config`). Name it e.g. `gdrive`.
3. Create an **alias** remote pointing at the test folder, e.g. `rclone config create gdrive-test alias remote=gdrive:_synclone-test`.
4. Repeat for `onedrive-test` and `drime-test`. For Drime, first create an API access token in the Drime web control panel and use it for the `drime` remote (`rclone config create drime drime access_token=<token>`).
5. Tell the agent the alias names. It will run the smoke tests with `SYNCLONE_LIVE_TEST_REMOTES=gdrive-test,onedrive-test,drime-test`. The harness refuses any remote that is not an `alias` into `_synclone-test`.

These also settle the two open live checks: Google Docs as link files, and OneDrive for Business file mutation (DECISIONS.md D-019).
