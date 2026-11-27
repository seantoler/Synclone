//! Typed views of rc requests and responses (as of rclone v1.76.0).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One entry of `operations/list` / `operations/stat` (the lsjson format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEntry {
    /// Path relative to the fs root that was listed.
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    /// -1 for objects of unknown size (e.g. Google Docs).
    #[serde(rename = "Size", default)]
    pub size: i64,
    #[serde(rename = "ModTime", default)]
    pub mod_time: Option<DateTime<Utc>>,
    #[serde(rename = "IsDir", default)]
    pub is_dir: bool,
    /// Provider ID, where the backend exposes one (Drive, OneDrive, Drime...).
    #[serde(rename = "ID", default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "Hashes", default, skip_serializing_if = "Option::is_none")]
    pub hashes: Option<std::collections::BTreeMap<String, String>>,
    #[serde(rename = "MimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl RemoteEntry {
    pub fn hash(&self, kind: &str) -> Option<&str> {
        self.hashes
            .as_ref()
            .and_then(|h| h.get(kind))
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// Options for `operations/list` and `operations/stat`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOpts {
    pub recurse: bool,
    /// Hash types to return. Always name them: `showHash` without
    /// `hashTypes` makes rclone compute every hash it knows, which on the
    /// local backend reads each file many times over.
    pub hash_types: Vec<String>,
    pub no_mod_time: bool,
    pub dirs_only: bool,
    pub files_only: bool,
}

impl ListOpts {
    pub fn to_json(&self) -> Value {
        let mut opt = Map::new();
        opt.insert("recurse".into(), Value::Bool(self.recurse));
        opt.insert("noMimeType".into(), Value::Bool(true));
        if self.no_mod_time {
            opt.insert("noModTime".into(), Value::Bool(true));
        }
        if !self.hash_types.is_empty() {
            opt.insert("showHash".into(), Value::Bool(true));
            opt.insert(
                "hashTypes".into(),
                Value::Array(self.hash_types.iter().cloned().map(Value::String).collect()),
            );
        }
        if self.dirs_only {
            opt.insert("dirsOnly".into(), Value::Bool(true));
        }
        if self.files_only {
            opt.insert("filesOnly".into(), Value::Bool(true));
        }
        Value::Object(opt)
    }
}

/// `operations/fsinfo`: what a remote can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsInfo {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Hashes", default)]
    pub hashes: Vec<String>,
    /// Modification time precision in nanoseconds; `MODTIME_NOT_SUPPORTED`
    /// when the backend cannot store times.
    #[serde(rename = "Precision", default = "one")]
    pub precision_ns: i64,
    #[serde(rename = "Features", default)]
    pub features: std::collections::BTreeMap<String, bool>,
}

fn one() -> i64 {
    1
}

/// rclone's `fs.ModTimeNotSupported` (100 years in ns).
pub const MODTIME_NOT_SUPPORTED: i64 = 3_153_600_000_000_000_000;

impl FsInfo {
    pub fn feature(&self, name: &str) -> bool {
        self.features.get(name).copied().unwrap_or(false)
    }

    /// The hash used to tell file versions apart: the remote's own, unless
    /// rclone would have to read the whole file to make one (local, SFTP).
    pub fn preferred_hash(&self) -> Option<String> {
        if self.feature("SlowHash") || self.hashes.is_empty() {
            return None;
        }
        for p in ["sha1", "md5", "sha256", "quickxor", "dropbox"] {
            if self.hashes.iter().any(|h| h.eq_ignore_ascii_case(p)) {
                return Some(p.to_string());
            }
        }
        Some(self.hashes[0].to_ascii_lowercase())
    }

    pub fn keeps_mod_times(&self) -> bool {
        self.precision_ns < MODTIME_NOT_SUPPORTED
    }

    /// Tolerance when comparing two times of the same file: the remote's
    /// precision, at least one second.
    pub fn time_tolerance_ns(&self) -> i64 {
        if self.keeps_mod_times() {
            self.precision_ns.max(1_000_000_000)
        } else {
            1_000_000_000
        }
    }
}

/// `core/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    #[serde(default)]
    pub decomposed: Vec<u32>,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(rename = "goVersion", default)]
    pub go_version: String,
}

impl VersionInfo {
    /// `(major, minor, patch)`, parsed from `decomposed` or the version string.
    pub fn triple(&self) -> (u32, u32, u32) {
        if self.decomposed.len() >= 3 {
            return (self.decomposed[0], self.decomposed[1], self.decomposed[2]);
        }
        parse_version(&self.version).unwrap_or((0, 0, 0))
    }

    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        let (a, b, _) = self.triple();
        (a, b) >= (major, minor)
    }
}

pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

/// The answer to `config/create` / `config/update` in non-interactive mode:
/// either done (`state` empty) or a question to answer with `continue`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConfigStep {
    #[serde(rename = "State", default)]
    pub state: String,
    #[serde(rename = "Option", default)]
    pub option: Option<ConfigQuestion>,
    #[serde(rename = "Error", default)]
    pub error: String,
    #[serde(rename = "Result", default)]
    pub result: String,
}

impl ConfigStep {
    pub fn is_done(&self) -> bool {
        self.state.is_empty() && self.option.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConfigQuestion {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Help", default)]
    pub help: String,
    #[serde(rename = "Default", default)]
    pub default: Value,
    #[serde(rename = "Examples", default)]
    pub examples: Option<Vec<ConfigExample>>,
    #[serde(rename = "Type", default)]
    pub kind: String,
    #[serde(rename = "Required", default)]
    pub required: bool,
    #[serde(rename = "IsPassword", default)]
    pub is_password: bool,
    #[serde(rename = "Exclusive", default)]
    pub exclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConfigExample {
    #[serde(rename = "Value", default)]
    pub value: String,
    #[serde(rename = "Help", default)]
    pub help: String,
}

/// Options for `config/create` and `config/update`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOpts {
    pub obscure: bool,
    pub non_interactive: bool,
    /// Continue with an answer to the question returned last time.
    pub continue_with: Option<(String, String)>,
}

impl ConfigOpts {
    pub fn to_json(&self) -> Value {
        let mut opt = Map::new();
        opt.insert("nonInteractive".into(), Value::Bool(self.non_interactive));
        if self.obscure {
            opt.insert("obscure".into(), Value::Bool(true));
        }
        if let Some((state, result)) = &self.continue_with {
            opt.insert("continue".into(), Value::Bool(true));
            opt.insert("state".into(), Value::String(state.clone()));
            opt.insert("result".into(), Value::String(result.clone()));
        }
        Value::Object(opt)
    }
}

/// One backend from `config/providers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Description", default)]
    pub description: String,
    #[serde(rename = "Prefix", default)]
    pub prefix: String,
    #[serde(rename = "Options", default)]
    pub options: Vec<ProviderOption>,
    #[serde(rename = "Hide", default)]
    pub hide: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderOption {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Help", default)]
    pub help: String,
    #[serde(rename = "Default", default)]
    pub default: Value,
    #[serde(rename = "Required", default)]
    pub required: bool,
    #[serde(rename = "Advanced", default)]
    pub advanced: bool,
    #[serde(rename = "IsPassword", default)]
    pub is_password: bool,
    #[serde(rename = "Sensitive", default)]
    pub sensitive: bool,
    #[serde(rename = "Hide", default)]
    pub hide: i64,
    #[serde(rename = "Type", default)]
    pub kind: String,
    #[serde(rename = "Examples", default)]
    pub examples: Option<Vec<ConfigExample>>,
}

/// `config/oauthstatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthStatus {
    pub status: String,
    #[serde(rename = "authUrl", default)]
    pub auth_url: Option<String>,
}

impl OAuthStatus {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
}

/// `job/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: i64,
    #[serde(default)]
    pub finished: bool,
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub group: String,
}

/// `operations/about`. Fields the provider does not report are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct About {
    #[serde(default)]
    pub total: Option<i64>,
    #[serde(default)]
    pub used: Option<i64>,
    #[serde(default)]
    pub free: Option<i64>,
    #[serde(default)]
    pub trashed: Option<i64>,
}

/// Transfer progress for a stats group (`core/stats`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Stats {
    #[serde(default)]
    pub bytes: i64,
    #[serde(rename = "totalBytes", default)]
    pub total_bytes: i64,
    #[serde(default)]
    pub speed: f64,
    #[serde(default)]
    pub errors: i64,
    #[serde(rename = "lastError", default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub transfers: i64,
}

/// `config/paths`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConfigPaths {
    #[serde(default)]
    pub config: String,
    #[serde(default)]
    pub cache: String,
    #[serde(default)]
    pub temp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("v1.76.0"), Some((1, 76, 0)));
        assert_eq!(parse_version("v1.73.0-beta.123"), Some((1, 73, 0)));
        assert_eq!(parse_version("1.80"), Some((1, 80, 0)));
        assert_eq!(parse_version("junk"), None);
    }

    #[test]
    fn fsinfo_hash_choice() {
        let mut info = FsInfo {
            name: "x".into(),
            hashes: vec!["md5".into(), "sha1".into()],
            precision_ns: 1,
            features: Default::default(),
        };
        assert_eq!(info.preferred_hash().as_deref(), Some("sha1"));
        info.features.insert("SlowHash".into(), true);
        assert_eq!(info.preferred_hash(), None);
        info.precision_ns = MODTIME_NOT_SUPPORTED;
        assert!(!info.keeps_mod_times());
    }

    #[test]
    fn list_opts_always_name_hash_types() {
        let o = ListOpts {
            hash_types: vec!["md5".into()],
            ..Default::default()
        };
        let j = o.to_json();
        assert_eq!(j["showHash"], Value::Bool(true));
        assert_eq!(j["hashTypes"][0], "md5");
        let none = ListOpts::default().to_json();
        assert!(none.get("showHash").is_none());
    }

    #[test]
    fn parses_entry_with_id() {
        let e: RemoteEntry = serde_json::from_str(
            r#"{"Path":"a/b.txt","Name":"b.txt","Size":3,"ModTime":"2026-11-27T18:28:07.273221878Z","IsDir":false,"ID":"abc","Hashes":{"md5":"x"}}"#,
        )
        .unwrap();
        assert_eq!(e.id.as_deref(), Some("abc"));
        assert_eq!(e.hash("md5"), Some("x"));
    }
}
