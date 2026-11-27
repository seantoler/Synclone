//! Which remote names can exist locally (PLAN.md §4.7).

use std::collections::HashMap;

use crate::provider::NameRules;

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹",
    "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²",
    "LPT³",
];

/// Why `name` cannot exist locally, or `None` if it can.
pub fn unsyncable_reason(name: &str, rules: NameRules) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("empty or reserved name".into());
    }
    if name.contains('/') || name.contains('\0') {
        return Some("name contains a path separator".into());
    }
    if name.len() > 255 {
        return Some("name is longer than 255 bytes".into());
    }
    if rules.windows {
        if let Some(c) = name
            .chars()
            .find(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || (*c as u32) < 32)
        {
            return Some(format!("Windows does not allow the character {c:?} in names"));
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Some("Windows does not allow names ending in a dot or space".into());
        }
        let stem = name.split('.').next().unwrap_or("").trim_end().to_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Some(format!("\"{stem}\" is a reserved name on Windows"));
        }
    }
    None
}

/// Names among `names` that collide case-insensitively with another one.
pub fn case_collisions<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut groups: HashMap<String, Vec<&str>> = HashMap::new();
    for n in names {
        groups.entry(n.to_lowercase()).or_default().push(n);
    }
    let mut out: Vec<String> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .flatten()
        .map(str::to_string)
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_rules() {
        let w = NameRules::WINDOWS;
        assert!(unsyncable_reason("a:b", w).is_some());
        assert!(unsyncable_reason("what?", w).is_some());
        assert!(unsyncable_reason("trailing.", w).is_some());
        assert!(unsyncable_reason("CON", w).is_some());
        assert!(unsyncable_reason("nul.txt", w).is_some());
        assert!(unsyncable_reason("console.txt", w).is_none());
        assert!(unsyncable_reason("ok name.txt", w).is_none());
        assert!(unsyncable_reason("a:b", NameRules::POSIX).is_none());
    }

    #[test]
    fn finds_case_collisions() {
        assert_eq!(case_collisions(["A.txt", "a.txt", "b"]), vec!["A.txt", "a.txt"]);
        assert!(case_collisions(["a", "b"]).is_empty());
    }
}
