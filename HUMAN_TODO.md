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

