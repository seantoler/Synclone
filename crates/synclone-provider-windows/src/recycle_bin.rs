//! The current user's Recycle Bin, read directly (D-032).
//!
//! Each deleted item is a pair in `<volume>\$Recycle.Bin\<user SID>\`:
//! `$I<id><ext>` records the original path, size and deletion time, and
//! `$R<id><ext>` is the item itself (a file or a folder). Restoring an
//! item, as Explorer's "Restore" does, is moving `$R…` back and removing
//! `$I…`. Only the current user's folder is read, and callers match
//! entries by their exact original path, so nothing else is ever touched.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinEntry {
    /// Where the item was when it was deleted.
    pub original: PathBuf,
    /// Its size as recorded at deletion.
    pub size: u64,
    /// Deletion time (FILETIME: 100 ns units since 1601).
    pub deleted: i64,
    /// The `$I` record.
    pub info: PathBuf,
    /// The `$R` content (file or folder).
    pub content: PathBuf,
}

/// Parse a `$I` record: version 1 (Vista–8.1, fixed 520-byte path) or
/// version 2 (Windows 10+, length-prefixed path).
pub fn parse_info(b: &[u8]) -> Option<(u64, i64, String)> {
    let u64_at = |o: usize| b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    let version = u64_at(0)?;
    let size = u64_at(8)?;
    let deleted = u64_at(16)? as i64;
    let utf16: Vec<u16> = match version {
        1 => b
            .get(24..24 + 520)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect(),
        2 => {
            let n = u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize;
            b.get(28..28 + n * 2)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect()
        }
        _ => return None,
    };
    let end = utf16.iter().position(|c| *c == 0).unwrap_or(utf16.len());
    Some((size, deleted, String::from_utf16(&utf16[..end]).ok()?))
}

/// The current user's Recycle Bin folder on the volume holding `path`.
pub fn bin_dir_for(path: &Path) -> Result<PathBuf> {
    let s = path.display().to_string();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let b = s.as_bytes();
    if b.len() < 2 || b[1] != b':' || !b[0].is_ascii_alphabetic() {
        bail!("{} is not on a drive with a letter", path.display());
    }
    Ok(PathBuf::from(format!(r"{}:\$Recycle.Bin", &s[..1])).join(current_user_sid()?))
}

/// Paths compare as Windows does: case-insensitive, either slash, no
/// `\\?\` prefix, no trailing separator.
pub fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

fn norm(p: &Path) -> String {
    let s = p.display().to_string().replace('/', "\\");
    let s = s
        .strip_prefix(r"\\?\")
        .unwrap_or(&s)
        .trim_end_matches('\\')
        .to_lowercase();
    s
}

/// Whether `p` is `prefix` or inside it.
pub fn is_under(p: &Path, prefix: &Path) -> bool {
    let (p, pre) = (norm(p), norm(prefix));
    p == pre || p.starts_with(&format!("{pre}\\"))
}

/// Every entry in the current user's bin on the volume of `near`.
pub fn entries_near(near: &Path) -> Result<Vec<BinEntry>> {
    let dir = bin_dir_for(near)?;
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        // No bin on this volume yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("$I") else { continue };
        let Ok(bytes) = std::fs::read(e.path()) else { continue };
        let Some((size, deleted, original)) = parse_info(&bytes) else {
            continue;
        };
        let content = dir.join(format!("$R{rest}"));
        if !content.exists() {
            continue;
        }
        out.push(BinEntry {
            original: PathBuf::from(original),
            size,
            deleted,
            info: e.path(),
            content,
        });
    }
    // Newest first.
    out.sort_by_key(|e| std::cmp::Reverse(e.deleted));
    Ok(out)
}

/// Entries whose original path is exactly `original`, newest first.
pub fn find(original: &Path) -> Result<Vec<BinEntry>> {
    Ok(entries_near(original)?
        .into_iter()
        .filter(|e| same_path(&e.original, original))
        .collect())
}

/// Entries that came from `prefix` or anything inside it.
pub fn entries_under(prefix: &Path) -> Result<Vec<BinEntry>> {
    Ok(entries_near(prefix)?
        .into_iter()
        .filter(|e| is_under(&e.original, prefix))
        .collect())
}

/// Move an entry's content to `to` and drop its record (a restore to a
/// place of our choosing).
pub fn take(e: &BinEntry, to: &Path) -> Result<()> {
    if std::fs::rename(&e.content, to).is_err() {
        // Another volume: copy, then remove.
        if e.content.is_dir() {
            bail!("cannot move the folder {} to another drive", e.content.display());
        }
        std::fs::copy(&e.content, to).with_context(|| format!("copying {}", e.content.display()))?;
        std::fs::remove_file(&e.content)?;
    }
    let _ = std::fs::remove_file(&e.info);
    refresh_explorer(&e.info);
    Ok(())
}

/// Delete an entry for good (test cleanup of the tests' own entries).
pub fn purge(e: &BinEntry) -> Result<()> {
    if e.content.is_dir() {
        std::fs::remove_dir_all(&e.content)
    } else {
        std::fs::remove_file(&e.content)
    }
    .with_context(|| format!("removing {}", e.content.display()))?;
    let _ = std::fs::remove_file(&e.info);
    refresh_explorer(&e.info);
    Ok(())
}

/// Tell an open Recycle Bin window that its contents changed.
fn refresh_explorer(info: &Path) {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_UPDATEDIR, SHCNF_PATHW};
    if let Some(dir) = info.parent() {
        let w = crate::win::wide(dir);
        unsafe { SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW, Some(w.as_ptr().cast()), None) };
    }
}

/// The current user's SID as a string (`S-1-5-21-…`).
pub fn current_user_sid() -> Result<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).context("opening the process token")?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let r = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len);
        let _ = CloseHandle(token);
        r.context("reading the token's user")?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).context("formatting the SID")?;
        let out = s.to_string().context("SID text")?;
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(size: u64, t: i64, path: &str) -> Vec<u8> {
        let w: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut b = Vec::new();
        b.extend(2u64.to_le_bytes());
        b.extend(size.to_le_bytes());
        b.extend(t.to_le_bytes());
        b.extend((w.len() as u32).to_le_bytes());
        for c in w {
            b.extend(c.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_both_record_versions() {
        let b = v2(1234, 99, r"C:\Users\me\Synclone\Cloud\a.txt");
        assert_eq!(
            parse_info(&b),
            Some((1234, 99, r"C:\Users\me\Synclone\Cloud\a.txt".into()))
        );
        let mut v1 = Vec::new();
        v1.extend(1u64.to_le_bytes());
        v1.extend(5u64.to_le_bytes());
        v1.extend(7i64.to_le_bytes());
        let mut path: Vec<u8> = r"D:\x\y.doc".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        path.resize(520, 0);
        v1.extend(path);
        assert_eq!(parse_info(&v1), Some((5, 7, r"D:\x\y.doc".into())));
        assert_eq!(parse_info(b"junk"), None);
        assert_eq!(parse_info(&3u64.to_le_bytes().repeat(5)), None);
    }

    #[test]
    fn paths_compare_like_windows() {
        assert!(same_path(Path::new(r"C:\A\b.TXT"), Path::new(r"c:\a\B.txt")));
        assert!(same_path(Path::new(r"\\?\C:\a\"), Path::new("C:/a")));
        assert!(is_under(Path::new(r"C:\t\w1\Cloud\x.txt"), Path::new(r"c:\t\w1")));
        assert!(!is_under(Path::new(r"C:\t\w10\x.txt"), Path::new(r"C:\t\w1")));
    }

    #[test]
    fn finds_the_current_users_bin() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        let dir = bin_dir_for(Path::new(r"C:\Users")).unwrap();
        assert!(dir.ends_with(&sid));
        // Reading it works (it may be empty or missing).
        entries_near(Path::new(r"C:\")).unwrap();
    }
}
