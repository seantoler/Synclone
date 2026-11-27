# Progress

## Current phase: 0 (Bootstrap)

### Done
- Plan renamed to Synclone; CLAUDE.md, DECISIONS.md, HUMAN_TODO.md created.
- Cargo workspace with all eight crates (`synclone-core`, `-rclone`, `-daemon`, `-cli`, `-provider-stub`, `-provider-windows`, `-provider-macos`, `-app`).
- `scripts/fetch-rclone` (rclone v1.76.0, SHA-256 verified against committed checksums).
- `synclone-rclone`: supervised `rclone rcd`, typed async client, mock, sign-in flow.
- CI workflow for ubuntu/windows/macos (fmt, clippy -D warnings, tests).

### Next
- Confirm CI is green on all three OSes.
- Phase 1.

### Known failures
- None yet.
