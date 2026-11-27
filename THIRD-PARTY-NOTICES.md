# Third-party notices

## rclone

Synclone bundles and runs [rclone](https://rclone.org) (pinned version in `scripts/rclone-version.txt`). rclone is distributed under the MIT license; its full notice is in [`licenses/rclone-COPYING.txt`](licenses/rclone-COPYING.txt) and must ship with every package that includes the rclone binary.

Synclone is built on rclone but is not affiliated with or endorsed by the rclone project.

## OpenCfDrive

Parts of Synclone's rclone integration are adapted from [OpenCfDrive](https://github.com/dkgs2000/OpenCfDrive) by dkgs2000, under the MIT license (full text in [`licenses/OpenCfDrive-LICENSE.txt`](licenses/OpenCfDrive-LICENSE.txt)). Adapted designs are listed in `DECISIONS.md` (D-006) and marked in source comments:

- `crates/synclone-rclone/src/daemon.rs`: rclone rcd supervision (random port and per-launch login passed through the environment, restart with growing delays).
- `crates/synclone-rclone/src/api.rs`, `types.rs`: fs/remote splitting rules, listing options and the remote-capabilities logic (`FsInfo::preferred_hash`, time tolerance).
- `crates/synclone-core`: the version comparison rules (size, then remote hash, then time within the remote's precision) and conflict-copy name truncation.

Copyright (c) 2026 dkgs2000
