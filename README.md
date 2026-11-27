# Synclone

Multi-cloud sync client with native placeholder files for Windows and macOS, built on rclone.

Synclone puts one folder on your computer with a subfolder for each cloud storage you connect (Google Drive, OneDrive, Drime, and anything else rclone supports). Every file shows up without using disk space; open one and it downloads. Downloads stay until you remove them from the device. Changes you make upload automatically, and deleting a file locally never deletes it from the cloud until you empty Synclone's trash.

> **Status:** early development. The sync engine and command-line tool are being built and tested on Linux first; the Windows and macOS integrations come next. See [`PLAN.md`](PLAN.md) for the full design and [`PROGRESS.md`](PROGRESS.md) for where things stand.

## Building from source

Requirements: Rust (stable), `bash`, `curl` and `unzip`.

```bash
scripts/fetch-rclone
```

```bash
cargo build --workspace
```

```bash
cargo test --workspace
```

`scripts/fetch-rclone` downloads the pinned rclone release from GitHub and checks its SHA-256 against `scripts/rclone-checksums.txt`.

## Command-line tool

`synclonectl` exposes every operation (connecting cloud storage, downloading, freeing space, the trash queue, conflicts). Run `synclonectl --help`.

## Credits

- **[rclone](https://rclone.org)** does all the talking to cloud providers. Synclone is built on rclone and is **not affiliated with the rclone project**. rclone is MIT-licensed; see [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
- **[OpenCfDrive](https://github.com/dkgs2000/OpenCfDrive)** by dkgs2000 (MIT) showed how to drive `rclone rcd` safely and how to tell file versions apart across backends. Synclone adapts several of its designs; they are listed in [`DECISIONS.md`](DECISIONS.md) (D-006).
- Nimbus and Cascade were studied for design ideas only (no code used).

Synclone is not affiliated with odrive.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
