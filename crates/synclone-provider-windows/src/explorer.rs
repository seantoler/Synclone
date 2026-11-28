//! Which folders are open in Explorer windows right now (D-031), through
//! the Shell's window list (`IShellWindows`), the same list Explorer
//! itself keeps. Read-only; a failure just means "none".

use std::path::PathBuf;

use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, PathCreateFromUrlW, ShellWindows};

/// File-system folders shown in open Explorer windows.
pub fn open_folders() -> Vec<PathBuf> {
    // COM is set up per thread; do it on a short-lived thread of our own so
    // the caller's threading model never matters.
    std::thread::spawn(|| unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let out = list().unwrap_or_default();
        if init.is_ok() {
            CoUninitialize();
        }
        out
    })
    .join()
    .unwrap_or_default()
}

unsafe fn list() -> windows::core::Result<Vec<PathBuf>> {
    let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
    let n = windows.Count()?;
    let mut out = Vec::new();
    for i in 0..n {
        let Ok(disp) = windows.Item(&VARIANT::from(i)) else {
            continue;
        };
        let Ok(browser) = disp.cast::<IWebBrowser2>() else {
            continue;
        };
        let Ok(url) = browser.LocationURL() else { continue };
        if let Some(p) = path_from_url(&url.to_string()) {
            out.push(p);
        }
    }
    Ok(out)
}

/// `file:///C:/a%20b` → `C:\a b` (the Shell's own decoding).
fn path_from_url(url: &str) -> Option<PathBuf> {
    if !url.to_ascii_lowercase().starts_with("file:") {
        return None;
    }
    let w: Vec<u16> = url.encode_utf16().chain([0]).collect();
    let mut buf = vec![0u16; 2048];
    let mut len = buf.len() as u32;
    unsafe {
        PathCreateFromUrlW(PCWSTR(w.as_ptr()), windows::core::PWSTR(buf.as_mut_ptr()), &mut len, 0).ok()?;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            path_from_url("file:///C:/Users/me/Synclone/My%20Drive/a"),
            Some(PathBuf::from(r"C:\Users\me\Synclone\My Drive\a"))
        );
        assert_eq!(path_from_url("https://example.com/"), None);
        assert_eq!(path_from_url(""), None);
    }

    #[test]
    fn listing_windows_never_fails() {
        // Whatever is open on this machine; must not panic or hang.
        let _ = open_folders();
    }
}
