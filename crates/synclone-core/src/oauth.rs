//! Synclone's own OAuth clients, when the build has them (see `build.rs`,
//! D-030). A desktop app's client secret is not confidential: Google says
//! so, and it ships inside every binary. It is kept out of git and logs
//! anyway, so it can be rotated without rewriting history.

use serde_json::{Map, Value};

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the values, even in a debug dump of the settings.
        f.write_str("OAuthClient(<redacted>)")
    }
}

/// The Google client baked into this build, if any.
pub fn built_in_google() -> Option<OAuthClient> {
    match (
        option_env!("SYNCLONE_BUILT_GOOGLE_CLIENT_ID"),
        option_env!("SYNCLONE_BUILT_GOOGLE_CLIENT_SECRET"),
    ) {
        (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => Some(OAuthClient {
            client_id: id.to_string(),
            client_secret: secret.to_string(),
        }),
        _ => None,
    }
}

/// Fill in Synclone's client for a new Google Drive remote, unless the user
/// gave their own `client_id` (the wizard's advanced options).
pub fn apply_google_client(kind: &str, params: &mut Map<String, Value>, client: Option<&OAuthClient>) {
    let Some(c) = client else { return };
    if kind != "drive" {
        return;
    }
    let own = params
        .get("client_id")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if own {
        return;
    }
    params.insert("client_id".into(), Value::String(c.client_id.clone()));
    params.insert("client_secret".into(), Value::String(c.client_secret.clone()));
}

/// Replace every occurrence of a client secret in `text` (error messages
/// shown to the user or written to a log).
pub fn redact(text: &str, client: Option<&OAuthClient>) -> String {
    match client {
        Some(c) if !c.client_secret.is_empty() => text.replace(&c.client_secret, "<redacted>"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c() -> OAuthClient {
        OAuthClient {
            client_id: "123-abc.apps.googleusercontent.com".into(),
            client_secret: "GOCSPX-test".into(),
        }
    }

    #[test]
    fn fills_in_drive_only_and_respects_the_users_own_client() {
        let mut p = Map::new();
        apply_google_client("drive", &mut p, Some(&c()));
        assert_eq!(p["client_id"], "123-abc.apps.googleusercontent.com");
        assert_eq!(p["client_secret"], "GOCSPX-test");

        let mut p = json!({"client_id": "mine", "client_secret": "my secret"})
            .as_object()
            .unwrap()
            .clone();
        apply_google_client("drive", &mut p, Some(&c()));
        assert_eq!(p["client_id"], "mine");
        assert_eq!(p["client_secret"], "my secret");

        let mut p = json!({"client_id": "  "}).as_object().unwrap().clone();
        apply_google_client("drive", &mut p, Some(&c()));
        assert_eq!(p["client_id"], "123-abc.apps.googleusercontent.com");

        let mut p = Map::new();
        apply_google_client("onedrive", &mut p, Some(&c()));
        assert!(p.is_empty());
        apply_google_client("drive", &mut p, None);
        assert!(p.is_empty());
    }

    #[test]
    fn debug_output_and_redaction_hide_the_secret() {
        assert!(!format!("{:?}", Some(c())).contains("GOCSPX-test"));
        assert!(!format!("{:?}", Some(c())).contains("123-abc"));
        assert_eq!(
            redact("bad secret GOCSPX-test here", Some(&c())),
            "bad secret <redacted> here"
        );
        assert_eq!(redact("x", None), "x");
    }
}
