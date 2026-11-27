# Synclone — agent instructions

Synclone is an open-source, odrive-style multi-cloud sync client for Windows and macOS, written in Rust on top of rclone. The full brief is `PLAN.md`. Every session starts by reading this file, `PROGRESS.md` and `DECISIONS.md`.

## Operating rules (PLAN.md §9)

1. Work through the phases in PLAN.md §10 in order. A phase is complete only when all its exit criteria pass in CI or in a recorded local test run.
2. Do not stop to ask for approval of design choices. Make the choice, record it in `DECISIONS.md` with one paragraph of reasoning, and continue.
3. Keep `PROGRESS.md` current: phase, what is done, what is next, known failures. Update it at the end of every working session.
4. When something needs a human (list below), append it to `HUMAN_TODO.md` with exact steps, then continue with any work that is not blocked.
5. Small commits with descriptive messages. Never commit secrets, tokens, or rclone config files.
6. Never weaken, skip or delete a test to make a build pass. Fix the code, or record the failure honestly in `PROGRESS.md`.
7. Resolve every **VERIFY** item in the plan before building on it; record the finding in `DECISIONS.md`.
8. **Data safety, absolute rules:**
   - Tests always set `RCLONE_CONFIG` to a temp file. Never read, modify or print the user's real rclone config in tests.
   - Never run delete, purge, move or sync against a real cloud remote except through the `TestRoot` guard (PLAN.md §7 layer 4).
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

## Project conventions

- The product name lives in one constant: `synclone_core::APP_NAME` (and `APP_ID` for file and socket names). Never hard-code it elsewhere.
- Crates are named `synclone-*`; the CLI binary is `synclonectl` (a different name from the future `Synclone` app binary, which matters on case-insensitive file systems).
- Default UI vocabulary follows PLAN.md §6.2 and §6.4: no "remote", "backend", "mount", "VFS" or "hydrate" in user-facing default text. The CLI and logs may use rclone terms.
- Fetch rclone with `scripts/fetch-rclone` (pinned version and SHA-256 in `scripts/`). Tests locate it through `SYNCLONE_RCLONE` or `vendor/rclone/<os>-<arch>/`.
- Before pushing: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
