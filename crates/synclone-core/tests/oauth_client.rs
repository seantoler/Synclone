//! Synclone's Google OAuth client (D-030): baked in from the environment
//! at build time, and never present in a file git tracks. Failures name
//! files only, never the values.

use std::path::Path;
use std::process::Command;

fn env(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The build picked up what the environment holds (when the test runs in
/// the same environment as the build, as `cargo test` does).
#[test]
fn the_build_uses_the_environments_client() {
    let built = synclone_core::oauth::built_in_google();
    match (env("SYNCLONE_GOOGLE_CLIENT_ID"), env("SYNCLONE_GOOGLE_CLIENT_SECRET")) {
        (Some(id), Some(secret)) => {
            let b = built.expect("both variables are set, but the build has no Google client");
            assert!(
                b.client_id == id,
                "built-in client_id differs from SYNCLONE_GOOGLE_CLIENT_ID"
            );
            assert!(
                b.client_secret == secret,
                "built-in secret differs from SYNCLONE_GOOGLE_CLIENT_SECRET"
            );
            assert!(
                b.client_id.ends_with(".apps.googleusercontent.com"),
                "client_id has an unexpected shape"
            );
        }
        _ => assert!(built.is_none(), "no variables, but the build has a Google client"),
    }
}

#[test]
fn no_tracked_file_contains_the_client() {
    let mut values: Vec<String> = Vec::new();
    if let Some(c) = synclone_core::oauth::built_in_google() {
        values.push(c.client_id);
        values.push(c.client_secret);
    }
    values.extend(env("SYNCLONE_GOOGLE_CLIENT_ID"));
    values.extend(env("SYNCLONE_GOOGLE_CLIENT_SECRET"));
    values.sort();
    values.dedup();
    if values.is_empty() {
        eprintln!("SKIPPED: no Google client in this build or environment");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z"])
        .output()
        .expect("git is needed for this check");
    assert!(out.status.success(), "git ls-files failed");
    let mut found = Vec::new();
    for f in out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        let name = String::from_utf8_lossy(f).to_string();
        let Ok(bytes) = std::fs::read(root.join(&name)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if values.iter().any(|v| text.contains(v.as_str())) {
            found.push(name);
        }
    }
    assert!(
        found.is_empty(),
        "the Google client appears in tracked files: {found:?}"
    );
}
