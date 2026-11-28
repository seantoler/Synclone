//! Setting up drives (D-030): Synclone's own Google client goes into new
//! Drive remotes, the user's own client wins, nothing leaks the secret;
//! and `only_remotes` limits which existing remotes become drives.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use synclone_core::db::Db;
use synclone_core::oauth::OAuthClient;
use synclone_core::*;
use synclone_provider_stub::StubProvider;
use synclone_rclone::config_flow::Answer;
use synclone_rclone::{MockRclone, RcloneApi};

fn engine(mock: &MockRclone, tmp: &tempfile::TempDir, settings: EngineSettings) -> Arc<Engine> {
    let stub = Arc::new(StubProvider::new(
        tmp.path().join("Synclone"),
        tmp.path().join("recycle"),
    ));
    Engine::new(
        Arc::new(Db::open(&tmp.path().join("state.db")).unwrap()),
        Arc::new(mock.clone()) as Arc<dyn RcloneApi>,
        stub,
        Arc::new(TestClock::new(1_000_000)),
        settings,
        tmp.path().join("state"),
    )
}

fn fake_client() -> OAuthClient {
    OAuthClient {
        client_id: "111-fake.apps.googleusercontent.com".into(),
        client_secret: "GOCSPX-fake-secret".into(),
    }
}

/// Add a Drive drive, approving the mock's browser sign-in; returns the
/// stored config of the new remote.
async fn add_drive(mock: &MockRclone, eng: &Engine, params: Map<String, Value>) -> Map<String, Value> {
    let approver = {
        let mock = mock.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                if mock.oauth_pending().is_some() {
                    mock.complete_oauth(true);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no sign-in was started");
        })
    };
    let d = eng
        .add_drive(
            "drive",
            Some("Google Drive"),
            params,
            &mut |_| Answer::Cancel,
            &mut |_| {},
        )
        .await
        .unwrap();
    approver.await.unwrap();
    mock.config_get(&d.remote_name).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_google_drive_remotes_use_synclones_client() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockRclone::new();
    let eng = engine(
        &mock,
        &tmp,
        EngineSettings {
            google_client: Some(fake_client()),
            ..Default::default()
        },
    );
    let cfg = add_drive(&mock, &eng, Map::new()).await;
    assert_eq!(cfg["client_id"], "111-fake.apps.googleusercontent.com");
    assert_eq!(cfg["client_secret"], "GOCSPX-fake-secret");
    // The user's own client (wizard, advanced options) wins.
    let own = json!({"client_id": "my-own-id", "client_secret": "my-own-secret"});
    let cfg = add_drive(&mock, &eng, own.as_object().unwrap().clone()).await;
    assert_eq!(cfg["client_id"], "my-own-id");
    assert_eq!(cfg["client_secret"], "my-own-secret");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_built_in_client_nothing_is_added() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockRclone::new();
    let eng = engine(
        &mock,
        &tmp,
        EngineSettings {
            google_client: None,
            ..Default::default()
        },
    );
    let cfg = add_drive(&mock, &eng, Map::new()).await;
    assert!(
        cfg.get("client_id").is_none() && cfg.get("client_secret").is_none(),
        "{cfg:?}"
    );
}

#[tokio::test]
async fn a_failed_setup_does_not_show_the_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockRclone::new();
    let eng = engine(
        &mock,
        &tmp,
        EngineSettings {
            google_client: Some(fake_client()),
            ..Default::default()
        },
    );
    // rclone echoing the parameters back in an error.
    mock.fail_next_with(
        "config/create",
        synclone_rclone::ErrorKind::BadRequest,
        "invalid client: client_secret GOCSPX-fake-secret rejected",
    );
    let e = eng
        .add_drive("drive", None, Map::new(), &mut |_| Answer::Cancel, &mut |_| {})
        .await
        .unwrap_err();
    let text = format!("{e:#} {e:?}");
    assert!(!text.contains("GOCSPX-fake-secret"), "{text}");
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!format!("{:?}", eng.settings).contains("GOCSPX-fake-secret"));
}

#[tokio::test]
async fn only_listed_remotes_are_imported() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockRclone::new();
    for r in ["gdrive-personal", "gdrive-test1", "onedrive", "onedrive-test"] {
        mock.add_remote(r);
    }
    let eng = engine(
        &mock,
        &tmp,
        EngineSettings {
            only_remotes: Some(vec!["gdrive-test1".into(), "onedrive-test".into()]),
            ..Default::default()
        },
    );
    let added = eng.import_remotes().await.unwrap();
    let mut names: Vec<_> = added.iter().map(|d| d.remote_name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["gdrive-test1", "onedrive-test"]);
    assert!(eng.import_remotes().await.unwrap().is_empty(), "idempotent");
    assert_eq!(mock.remote_write_calls(), 0);
    // The others stay untouched in the rclone config, and are not hidden:
    // without the filter they would be imported as usual.
    assert_eq!(mock.list_remotes_sync().len(), 4);
    let tmp2 = tempfile::tempdir().unwrap();
    let all = engine(&mock, &tmp2, EngineSettings::default());
    assert_eq!(all.import_remotes().await.unwrap().len(), 4);
}
