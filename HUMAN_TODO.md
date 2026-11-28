# Things that need a human

Each item has exact steps. Items are removed once done.

## 1. Get Synclone's Google OAuth client verified (before public release)

The client exists, is published, and is wired into builds through `SYNCLONE_GOOGLE_CLIENT_ID` / `SYNCLONE_GOOGLE_CLIENT_SECRET` (D-030). Until Google verifies it, sign-in shows "Google hasn't verified this app" (Advanced, then "Go to Synclone (unsafe)"), and the restricted Drive scope caps the app at 100 users in total.

1. In the Google Cloud console, OAuth consent screen: add the home page and privacy policy URLs (the GitHub Pages site in `docs/`), an app logo and a support email.
2. Submit the app for verification. The Drive scope is restricted, so expect a security assessment; this can take weeks.
3. When a release workflow is added (Phase 8), add the same two names as repository secrets (Settings, then Secrets and variables, then Actions).

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
