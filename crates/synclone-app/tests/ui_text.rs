//! Plain language in the windows (PLAN.md §6.2, §6.4): no "remote",
//! "backend", "mount", "VFS" or "hydrate" in anything a user sees by
//! default. Allowed: elements with class `rclone-terms` (the "What does
//! this do?" note, drive details) and JS lines marked `rclone-terms`.

use std::path::{Path, PathBuf};

const BANNED: &[&str] = &["remote", "backend", "mount", "vfs", "hydrat"];

fn ui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui")
}

fn banned_in(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut hits = Vec::new();
    for w in BANNED {
        let mut start = 0;
        while let Some(i) = lower[start..].find(w) {
            let at = start + i;
            let before = lower[..at].chars().next_back();
            // Whole words only ("amount" is fine); "hydrat" matches hydrate/hydration.
            if !before.map(|c| c.is_alphanumeric()).unwrap_or(false) {
                let end = (at + 30).min(lower.len());
                hits.push(lower[at.saturating_sub(20)..end].to_string());
            }
            start = at + w.len();
        }
    }
    hits
}

/// Remove `<tag ... class="... rclone-terms ...">...</tag>` blocks (no nesting
/// of the same tag inside them in our pages).
fn strip_allowed(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find("rclone-terms") {
        let open = rest[..i].rfind('<').unwrap();
        let tag: String = rest[open + 1..].chars().take_while(|c| c.is_alphanumeric()).collect();
        let close = format!("</{tag}>");
        let end = rest[i..]
            .find(&close)
            .map(|j| i + j + close.len())
            .unwrap_or(rest.len());
        out.push_str(&rest[..open]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// What a user can read in an HTML page: text and visible attributes,
/// plus string literals in inline scripts.
fn visible_html(html: &str) -> String {
    let html = strip_allowed(html);
    let mut text = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in html.chars() {
        match (in_tag, ch) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                for attr in ["placeholder=\"", "title=\"", "aria-label=\""] {
                    if let Some(i) = tag.find(attr) {
                        let v = &tag[i + attr.len()..];
                        text.push_str(&v[..v.find('"').unwrap_or(v.len())]);
                        text.push('\n');
                    }
                }
            }
            (true, c) => tag.push(c),
            (false, c) => text.push(c),
        }
    }
    text
}

/// String literals of a script, skipping lines marked `rclone-terms`.
fn js_strings(js: &str) -> String {
    let mut out = String::new();
    for line in js.lines().filter(|l| !l.contains("rclone-terms")) {
        let mut q: Option<char> = None;
        for c in line.chars() {
            match (q, c) {
                (None, '\'' | '"' | '`') => q = Some(c),
                (Some(open), c) if c == open => {
                    q = None;
                    out.push('\n');
                }
                (Some(_), c) => out.push(c),
                _ => {}
            }
        }
    }
    out
}

#[test]
fn windows_use_plain_language() {
    let mut problems = Vec::new();
    for e in std::fs::read_dir(ui_dir()).unwrap() {
        let p = e.unwrap().path();
        let src = std::fs::read_to_string(&p).unwrap();
        let visible = match p.extension().and_then(|x| x.to_str()) {
            Some("html") => {
                // Inline scripts count as JS.
                let mut v = String::new();
                let mut rest = src.as_str();
                let mut html = String::new();
                while let Some(i) = rest.find("<script>") {
                    html.push_str(&rest[..i]);
                    let end = rest[i..].find("</script>").map(|j| i + j).unwrap_or(rest.len());
                    v.push_str(&js_strings(&rest[i + 8..end]));
                    rest = &rest[end..];
                }
                html.push_str(rest);
                v.push_str(&visible_html(&html));
                v
            }
            Some("js") => js_strings(&src),
            _ => continue,
        };
        for hit in banned_in(&visible) {
            problems.push(format!("{}: …{hit}…", p.file_name().unwrap().to_string_lossy()));
        }
    }
    assert!(
        problems.is_empty(),
        "rclone terms in default UI text:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_checker_catches_what_it_should() {
    assert_eq!(banned_in("Add a remote").len(), 1);
    assert!(banned_in("the amount you pay").is_empty());
    assert_eq!(banned_in("hydration").len(), 1);
    assert!(visible_html("<p class=\"note rclone-terms\">rclone remote x</p><p>fine</p>").contains("fine"));
    assert!(banned_in(&visible_html("<p class=\"note rclone-terms\">rclone remote x</p>")).is_empty());
    assert_eq!(banned_in(&visible_html("<input placeholder=\"Remote name\">")).len(), 1);
    assert!(banned_in(&js_strings("const x = d.remote; // code, not text")).is_empty());
    assert_eq!(banned_in(&js_strings("el('p', {text: 'Your remote'})")).len(), 1);
}

/// The product name lives in `APP_NAME`; the bundle config has to repeat
/// it, so this keeps the two in step (D-026).
#[test]
fn bundle_name_matches_app_name() {
    let conf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(conf["productName"], synclone_core::APP_NAME);
    assert_eq!(conf["version"], synclone_core::VERSION);
}

/// Every page loads the shared helpers and the stylesheet, and every
/// window the tray opens exists.
#[test]
fn every_window_page_exists() {
    for page in ["wizard", "trash", "conflicts", "activity", "settings", "disconnect"] {
        let src = std::fs::read_to_string(ui_dir().join(format!("{page}.html"))).unwrap();
        assert!(src.contains("common.js") && src.contains("style.css"), "{page}");
    }
}
