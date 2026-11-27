//! Files that are never uploaded (PLAN.md §4.4).

use globset::{Glob, GlobSet, GlobSetBuilder};

pub const DEFAULT_PATTERNS: &[&str] = &[
    "~$*",
    ".~lock.*",
    "*.tmp",
    "*.partial",
    "*.crdownload",
    ".DS_Store",
    "._*",
    "desktop.ini",
    "Thumbs.db",
    "Icon\r",
    // Synclone's own working files.
    "*.sclstub",
    ".synclone-*",
];

pub struct IgnoreList {
    set: GlobSet,
    patterns: Vec<String>,
}

impl IgnoreList {
    pub fn new(extra: &[String]) -> Self {
        let mut b = GlobSetBuilder::new();
        let mut patterns = Vec::new();
        for p in DEFAULT_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .chain(extra.iter().cloned())
        {
            // Patterns match the file name only, case-insensitively.
            if let Ok(g) = globset::GlobBuilder::new(&p)
                .case_insensitive(true)
                .literal_separator(true)
                .build()
            {
                b.add(g);
                patterns.push(p);
            } else if let Ok(g) = Glob::new(&globset::escape(&p)) {
                b.add(g);
                patterns.push(p);
            }
        }
        Self {
            set: b.build().unwrap_or_else(|_| GlobSet::empty()),
            patterns,
        }
    }

    pub fn is_ignored(&self, name: &str) -> bool {
        self.set.is_match(name)
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

impl Default for IgnoreList {
    fn default() -> Self {
        Self::new(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_list() {
        let l = IgnoreList::default();
        for n in [
            "~$report.docx",
            ".~lock.a.odt#",
            "x.TMP",
            "a.partial",
            "f.crdownload",
            ".DS_Store",
            "._x",
            "desktop.ini",
            "Thumbs.db",
            "Icon\r",
        ] {
            assert!(l.is_ignored(n), "{n:?}");
        }
        for n in ["report.docx", "tmp", "Icon", "photo.jpg"] {
            assert!(!l.is_ignored(n), "{n:?}");
        }
        let l = IgnoreList::new(&["*.bak".into()]);
        assert!(l.is_ignored("x.bak"));
    }
}
