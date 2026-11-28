//! Bake Synclone's own Google OAuth client into the build when the builder
//! has one (D-030). The values come from the environment only, never from
//! a file in the repo:
//!
//! - `SYNCLONE_GOOGLE_CLIENT_ID`
//! - `SYNCLONE_GOOGLE_CLIENT_SECRET`
//!
//! Without both, Drive sign-in uses rclone's shared client, as before.
//! Values are trimmed (a pasted trailing space is common). Nothing is
//! printed: build script output is not shown unless cargo runs with -vv,
//! and even then only as `rustc-env` lines.

fn main() {
    let get = |k: &str| {
        println!("cargo:rerun-if-env-changed={k}");
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let id = get("SYNCLONE_GOOGLE_CLIENT_ID");
    let secret = get("SYNCLONE_GOOGLE_CLIENT_SECRET");
    match (id, secret) {
        (Some(id), Some(secret)) => {
            println!("cargo:rustc-env=SYNCLONE_BUILT_GOOGLE_CLIENT_ID={id}");
            println!("cargo:rustc-env=SYNCLONE_BUILT_GOOGLE_CLIENT_SECRET={secret}");
        }
        (None, None) => {}
        _ => println!(
            "cargo:warning=only one of SYNCLONE_GOOGLE_CLIENT_ID and SYNCLONE_GOOGLE_CLIENT_SECRET is set; using rclone's shared Google client"
        ),
    }
}
