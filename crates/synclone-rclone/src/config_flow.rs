//! Creating a remote through rclone's non-interactive config protocol,
//! including the browser sign-in (OAuth) hand-off.
//!
//! `config/create` with `nonInteractive` returns one question at a time.
//! Synclone answers the ones it knows, asks the caller about the rest, and
//! continues with `config/update` + `continue`. Answering `config_is_local`
//! with `true` makes rclone start its local OAuth server and block until the
//! browser flow ends, so that call runs as an async rc job while
//! `config/oauthstatus` is polled for the URL to open (verified against
//! rclone v1.76.0; see DECISIONS.md).

use std::time::Duration;

use serde_json::{Map, Value};

use crate::api::RcloneApi;
use crate::error::{ErrorKind, RcloneError, Result};
use crate::types::{ConfigOpts, ConfigQuestion, ConfigStep};

/// Options that could make deleting on a connected drive permanent, or
/// otherwise bypass Synclone's delete queue. Requests that set them are
/// rejected (PLAN.md §6.4).
pub fn unsafe_parameter(kind: &str, key: &str, value: &Value) -> bool {
    let truthy = match value {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    };
    let falsy = match value {
        Value::Bool(b) => !*b,
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "false" | "0" | "no"),
        _ => false,
    };
    match key {
        "hard_delete" | "permanent_delete" | "delete_permanently" => truthy,
        "use_trash" if kind == "drive" => falsy,
        _ => false,
    }
}

pub fn check_parameters(kind: &str, params: &Map<String, Value>) -> Result<()> {
    for (k, v) in params {
        if unsafe_parameter(kind, k, v) {
            return Err(RcloneError::new(
                ErrorKind::BadRequest,
                "config/create",
                format!("the option \"{k}\" would delete files permanently and is not allowed"),
            ));
        }
    }
    Ok(())
}

/// What to say to a question rclone asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Value(String),
    Cancel,
}

/// Progress reported while a remote is being set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowEvent {
    /// The user must open this URL to sign in.
    OpenBrowser(String),
    /// rclone answered a question for itself or we used its default.
    AutoAnswered { question: String, answer: String },
}

/// Answers Synclone gives without asking anyone.
pub fn auto_answer(q: &ConfigQuestion) -> Option<String> {
    match q.name.as_str() {
        // Browser sign-in on this machine.
        "config_is_local" => Some("true".into()),
        // Google's shared client ID still works during 2026; users can set
        // their own client_id in the wizard's advanced options (see DECISIONS.md).
        "config_shared_client_id" => Some("true".into()),
        // Don't wander into shared drives / advanced config by default.
        "config_change_team_drive" | "config_fs_advanced" | "config_advanced" => Some("false".into()),
        _ => {
            // Use rclone's default where it has a meaningful one.
            match &q.default {
                Value::Bool(b) => Some(b.to_string()),
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => q
                    .examples
                    .as_ref()
                    .filter(|e| q.exclusive && e.len() == 1)
                    .map(|e| e[0].value.clone()),
            }
        }
    }
}

pub struct FlowSettings {
    /// How long to wait for the user to finish signing in.
    pub oauth_timeout: Duration,
    pub poll_interval: Duration,
    /// Safety limit on the number of questions.
    pub max_steps: usize,
}

impl Default for FlowSettings {
    fn default() -> Self {
        Self {
            oauth_timeout: Duration::from_secs(600),
            poll_interval: Duration::from_millis(100),
            max_steps: 50,
        }
    }
}

/// Create remote `name` of type `kind`. `ask` is called for questions
/// Synclone cannot answer itself; `events` receives progress (the browser
/// URL above all). On failure or cancel the half-made remote is removed.
pub async fn create_remote(
    api: &dyn RcloneApi,
    name: &str,
    kind: &str,
    parameters: Map<String, Value>,
    ask: &mut (dyn FnMut(&ConfigQuestion) -> Answer + Send),
    events: &mut (dyn FnMut(FlowEvent) + Send),
    settings: &FlowSettings,
) -> Result<()> {
    check_parameters(kind, &parameters)?;
    if api.list_remotes().await?.iter().any(|r| r == name) {
        return Err(RcloneError::new(
            ErrorKind::BadRequest,
            "config/create",
            format!("\"{name}\" already exists"),
        ));
    }
    let opts = ConfigOpts {
        obscure: true,
        non_interactive: true,
        continue_with: None,
    };
    let result = async {
        let mut step = api.config_create(name, kind, parameters, &opts).await?;
        for _ in 0..settings.max_steps {
            if !step.error.is_empty() {
                return Err(RcloneError::new(
                    ErrorKind::BadRequest,
                    "config/create",
                    step.error.clone(),
                ));
            }
            let Some(q) = step.option.clone() else {
                if step.is_done() {
                    return Ok(());
                }
                return Err(RcloneError::new(
                    ErrorKind::Other,
                    "config/create",
                    "rclone stopped without a question",
                ));
            };
            let answer = match auto_answer(&q) {
                Some(a) => {
                    events(FlowEvent::AutoAnswered {
                        question: q.name.clone(),
                        answer: a.clone(),
                    });
                    a
                }
                None => match ask(&q) {
                    Answer::Value(v) => v,
                    Answer::Cancel => {
                        return Err(RcloneError::new(ErrorKind::Other, "config/create", "cancelled"));
                    }
                },
            };
            step = continue_step(api, name, &step, &q, answer, events, settings).await?;
        }
        Err(RcloneError::new(
            ErrorKind::Other,
            "config/create",
            "too many questions",
        ))
    }
    .await;
    if result.is_err() {
        let _ = api.config_delete(name).await;
    }
    result
}

async fn continue_step(
    api: &dyn RcloneApi,
    name: &str,
    step: &ConfigStep,
    q: &ConfigQuestion,
    answer: String,
    events: &mut (dyn FnMut(FlowEvent) + Send),
    settings: &FlowSettings,
) -> Result<ConfigStep> {
    let opts = ConfigOpts {
        obscure: true,
        non_interactive: true,
        continue_with: Some((step.state.clone(), answer.clone())),
    };
    let starts_oauth = q.name == "config_is_local" && answer == "true";
    let update = api.config_update(name, Map::new(), &opts);
    if !starts_oauth {
        return update.await;
    }
    // Run the blocking update while watching for the sign-in URL.
    tokio::pin!(update);
    let deadline = tokio::time::Instant::now() + settings.oauth_timeout;
    let mut announced = false;
    loop {
        tokio::select! {
            r = &mut update => return r,
            _ = tokio::time::sleep(settings.poll_interval) => {
                if tokio::time::Instant::now() > deadline {
                    let _ = api.oauth_stop().await;
                    // Let the update finish with its cancellation error.
                    let _ = (&mut update).await;
                    return Err(RcloneError::new(ErrorKind::Other, "config/update", "sign-in timed out"));
                }
                if !announced {
                    if let Ok(st) = api.oauth_status().await {
                        if let (true, Some(url)) = (st.is_running(), st.auth_url) {
                            events(FlowEvent::OpenBrowser(url));
                            announced = true;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockRclone;
    use std::sync::{Arc, Mutex};

    #[test]
    fn rejects_hard_delete() {
        assert!(unsafe_parameter("drime", "hard_delete", &Value::String("true".into())));
        assert!(unsafe_parameter("drive", "use_trash", &Value::Bool(false)));
        assert!(!unsafe_parameter("drive", "use_trash", &Value::Bool(true)));
        assert!(!unsafe_parameter(
            "onedrive",
            "hard_delete",
            &Value::String("false".into())
        ));
    }

    #[tokio::test]
    async fn oauth_flow_reports_url_and_completes() {
        let mock = MockRclone::new();
        let m2 = mock.clone();
        let urls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let u2 = urls.clone();
        let u3 = urls.clone();
        let approver = tokio::spawn(async move {
            loop {
                // Approve only once the user has been shown the URL.
                if m2.oauth_pending().is_some() && !u3.lock().unwrap().is_empty() {
                    m2.complete_oauth(true);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut ask = |_: &ConfigQuestion| Answer::Cancel;
        let mut ev = move |e: FlowEvent| {
            if let FlowEvent::OpenBrowser(u) = e {
                u2.lock().unwrap().push(u);
            }
        };
        let settings = FlowSettings {
            poll_interval: Duration::from_millis(5),
            ..Default::default()
        };
        create_remote(&mock, "gd", "drive", Map::new(), &mut ask, &mut ev, &settings)
            .await
            .unwrap();
        approver.await.unwrap();
        assert!(urls.lock().unwrap()[0].contains("/auth?state="));
        assert!(mock.list_remotes().await.unwrap().contains(&"gd".to_string()));
        assert!(mock.config_get("gd").await.unwrap().contains_key("token"));
    }

    #[tokio::test]
    async fn denied_oauth_removes_half_made_remote() {
        let mock = MockRclone::new();
        let m2 = mock.clone();
        tokio::spawn(async move {
            loop {
                if m2.oauth_pending().is_some() {
                    m2.complete_oauth(false);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut ask = |_: &ConfigQuestion| Answer::Cancel;
        let mut ev = |_e: FlowEvent| {};
        let settings = FlowSettings {
            poll_interval: Duration::from_millis(5),
            ..Default::default()
        };
        let err = create_remote(&mock, "od", "onedrive", Map::new(), &mut ask, &mut ev, &settings)
            .await
            .unwrap_err();
        assert!(err.message.contains("cancelled"), "{err}");
        assert!(!mock.list_remotes().await.unwrap().contains(&"od".to_string()));
    }

    #[tokio::test]
    async fn oauth_timeout_stops_server_and_cleans_up() {
        let mock = MockRclone::new();
        let mut ask = |_: &ConfigQuestion| Answer::Cancel;
        let mut ev = |_e: FlowEvent| {};
        let settings = FlowSettings {
            oauth_timeout: Duration::from_millis(50),
            poll_interval: Duration::from_millis(5),
            max_steps: 10,
        };
        let err = create_remote(&mock, "gd", "drive", Map::new(), &mut ask, &mut ev, &settings)
            .await
            .unwrap_err();
        assert!(err.message.contains("timed out"));
        assert_eq!(mock.count("config/oauthstop"), 1);
        assert!(mock.list_remotes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_provider_needs_no_browser() {
        let mock = MockRclone::new();
        let mut p = Map::new();
        p.insert("access_token".into(), Value::String("secret".into()));
        let mut ask = |_: &ConfigQuestion| Answer::Cancel;
        let mut ev = |_e: FlowEvent| {};
        create_remote(&mock, "drime", "drime", p, &mut ask, &mut ev, &FlowSettings::default())
            .await
            .unwrap();
        assert_eq!(mock.count("config/oauthstatus"), 0);
        let mut bad = Map::new();
        bad.insert("hard_delete".into(), Value::String("true".into()));
        assert!(
            create_remote(&mock, "d2", "drime", bad, &mut ask, &mut ev, &FlowSettings::default())
                .await
                .is_err()
        );
        assert_eq!(mock.count("config/create"), 1);
    }

    #[tokio::test]
    async fn duplicate_name_is_refused_without_touching_existing() {
        let mock = MockRclone::new();
        mock.add_remote("x");
        let mut ask = |_: &ConfigQuestion| Answer::Cancel;
        let mut ev = |_e: FlowEvent| {};
        assert!(create_remote(
            &mock,
            "x",
            "memory",
            Map::new(),
            &mut ask,
            &mut ev,
            &FlowSettings::default()
        )
        .await
        .is_err());
        assert!(mock.list_remotes().await.unwrap().contains(&"x".to_string()));
        assert_eq!(mock.count("config/delete"), 0);
    }
}
