//! Conflict copy names (PLAN.md §5 rule 4).

use chrono::{DateTime, Local};

use crate::types::RelPath;

/// The device name used in conflict copies: the host name.
pub fn device_name() -> String {
    let h = gethostname::gethostname().to_string_lossy().to_string();
    let h = h.split('.').next().unwrap_or("").trim().to_string();
    if h.is_empty() {
        "this device".into()
    } else {
        h
    }
}

/// Split `name` into stem and extension (`.ext`, or "" if none). A leading
/// dot (".bashrc") is part of the stem.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Truncate to at most `max` bytes without splitting a character.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `name (conflict - <device> <YYYY-MM-DD HH.MM>).ext`, with ` 2`, ` 3`...
/// added inside the parentheses until `taken` says the name is free. The
/// result always fits in 255 bytes: the stem is shortened if needed.
pub fn conflict_name(name: &str, device: &str, at: DateTime<Local>, taken: &dyn Fn(&str) -> bool) -> String {
    let (stem, ext) = split_ext(name);
    let stamp = at.format("%Y-%m-%d %H.%M").to_string();
    for n in 1.. {
        let suffix = if n == 1 {
            format!(" (conflict - {device} {stamp})")
        } else {
            format!(" (conflict - {device} {stamp} {n})")
        };
        let room = 255usize.saturating_sub(suffix.len() + ext.len());
        let candidate = format!("{}{}{}", truncate_bytes(stem, room), suffix, ext);
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

pub fn conflict_path(path: &RelPath, device: &str, at: DateTime<Local>, taken: &dyn Fn(&RelPath) -> bool) -> RelPath {
    let parent = path.parent().unwrap_or_default();
    let name = conflict_name(path.name(), device, at, &|n| taken(&parent.join(n)));
    parent.join(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn names_conflict_copies() {
        let at = Local.with_ymd_and_hms(2026, 11, 27, 9, 5, 0).unwrap();
        assert_eq!(
            conflict_name("report.docx", "laptop", at, &|_| false),
            "report (conflict - laptop 2026-11-27 09.05).docx"
        );
        assert_eq!(
            conflict_name("Makefile", "pc", at, &|_| false),
            "Makefile (conflict - pc 2026-11-27 09.05)"
        );
        assert_eq!(
            conflict_name(".bashrc", "pc", at, &|_| false),
            ".bashrc (conflict - pc 2026-11-27 09.05)"
        );
        let first = conflict_name("a.txt", "pc", at, &|_| false);
        let second = conflict_name("a.txt", "pc", at, &|n| n == first);
        assert_eq!(second, "a (conflict - pc 2026-11-27 09.05 2).txt");
        let long = "é".repeat(200) + ".txt";
        let c = conflict_name(&long, "pc", at, &|_| false);
        assert!(c.len() <= 255 && c.ends_with(").txt"));
    }
}
