# Manual checklist

What automation cannot check (PLAN.md §7 layer 5). Run before each release, on a clean user account where possible. Tick each item and note the OS build.

## Windows

1. **Install and first start.** Start `Synclone.exe`. The tray icon (blue square with a white cloud) appears. With no drives, the "Add cloud storage" window opens.
2. **Navigation pane.** Explorer shows a "Synclone" entry with the app icon in its navigation pane, pointing at `%USERPROFILE%\Synclone`.
3. **Google Drive sign-in.** Wizard, then Google Drive, then "Sign in with your browser". The browser opens Google's consent page; after you allow access, the wizard moves to "Name this cloud drive". "Open the sign-in page again" works if the browser tab was closed. "Cancel" while waiting returns to the form, and no half-made drive is left (`synclonectl drive list`).
4. **OneDrive sign-in.** The same as step 3 with a personal account, then a work account (OneDrive for Business: D-019 compares by size and time only).
5. **Drime token.** In the Drime web control panel, create an API access token, paste it, then Connect. "Where do I find this?" opens the page rclone's help names, if any.
6. **Status icons.** In each drive folder: cloud-only files show the cloud icon; opened files show a green check; files kept with "Always keep on this device" show the solid green check; a file being uploaded shows the sync arrows. Compare with `docs/evidence/windows-explorer-status.png`.
7. **Open to download.** Double-click a large cloud-only file. Explorer shows its download progress and the file opens.
8. **Context menu.** "Always keep on this device" on a folder downloads everything beneath it. "Free up space" frees it, and files with unsaved changes are kept.
9. **Delete and trash.** Delete a file. Tray, then Trash (1): Restore brings it back; delete again, then "Empty trash and delete from cloud", then confirm. The file is in the service's own web trash.
10. **Conflicts.** Edit a downloaded file in two places (here, and in the service's web editor) before Synclone syncs. Tray, then Conflicts (1): "Show in folder" selects the conflict copy.
11. **Disconnect.** Drive submenu, then Disconnect…, then confirm. The folder disappears and nothing changes in the cloud. "Connect again" restores it.
12. **Start at login.** Settings, then "Start Synclone when I sign in", then Save. Sign out and in: the tray icon is back, with no window opened.
13. **Quit.** Tray, then Quit: the tray icon goes, and no `Synclone.exe --service` or `rclone.exe` started by Synclone keeps running (Task Manager).
14. **SmartScreen.** On an unsigned build, SmartScreen warns once; "Run anyway" works (PLAN.md §12).
