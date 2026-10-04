use dioxus::server::axum::body::{Body, to_bytes};
use dioxus::server::axum::http::{Request, StatusCode, header};
use dioxus::server::axum::{Extension, Router};
use dioxus::server::{DioxusRouterExt, FullstackState};
// Links the server functions of `mobius-api` into this test binary.
use mobius_api as _;
use mobius_domain::{CheckupView, LabelStatus, PermissionStatus, RepositoryCheckup};
use mobius_engine::labels::MOBIUS_LABELS;
use mobius_engine::{Engine, github, workstreams};
use mobius_testkit::fake_github::{APP_ID, APP_SLUG, FakeGitHub};
use mobius_testkit::{start, wait_for};
use tempfile::TempDir;
use tower::ServiceExt;

const REPOSITORY: &str = "owner/shop";

fn api(engine: &Engine) -> Router {
    Router::new()
        .register_server_functions()
        .with_state(FullstackState::headless())
        .layer(Extension(engine.clone()))
        .layer(Extension(engine.store.clone()))
}

async fn cookie(engine: &Engine) -> String {
    let response = api(engine)
        .oneshot(
            Request::post("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "Firefox")
                .body(Body::from(r#"{"password":"correct horse"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split_once("; ")
        .unwrap()
        .0
        .to_string()
}

async fn get_view(engine: &Engine, cookie: &str, organization: &str) -> CheckupView {
    let response = api(engine)
        .oneshot(
            Request::get(format!("/api/checkup?organization={organization}"))
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn get_checkup(engine: &Engine, cookie: &str, organization: &str) -> Vec<RepositoryCheckup> {
    get_view(engine, cookie, organization).await.repositories
}

async fn fix(engine: &Engine, cookie: &str, organization: &str) -> StatusCode {
    api(engine)
        .oneshot(
            Request::post("/api/checkup/fix")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!(
                    r#"{{"organization":"{organization}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn the_checkup_shows_the_label_status_and_the_button_fixes_the_fixable() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || {
        workstreams::organizations(&engine)
            .contains(&"owner".to_string())
            .then_some(())
    })
    .await;
    let cookie = cookie(&engine).await;

    // No repository of another organization is in the checkup.
    assert!(get_checkup(&engine, &cookie, "stranger").await.is_empty());

    // The first poll of the repository created each missing Mobius label, so
    // the test deletes them again to see the `Missing` status and the button.
    wait_for(async || {
        (github.repository_labels(REPOSITORY).len() == MOBIUS_LABELS.len()).then_some(())
    })
    .await;
    for label in MOBIUS_LABELS {
        github.delete_repository_label(REPOSITORY, label.name);
    }

    // All Mobius labels are missing, so the button is "Create labels".
    let checkup = get_checkup(&engine, &cookie, "owner").await;
    assert_eq!(checkup.len(), 1);
    assert_eq!(checkup[0].repository, REPOSITORY);
    assert_eq!(checkup[0].labels.len(), MOBIUS_LABELS.len());
    assert!(
        checkup[0]
            .labels
            .iter()
            .all(|label| label.status == LabelStatus::Missing)
    );
    assert_eq!(mobius_ui::fix_button(&checkup), Some("Create labels"));

    assert_eq!(fix(&engine, &cookie, "owner").await, StatusCode::OK);

    let labels = github.repository_labels(REPOSITORY);
    for expected in MOBIUS_LABELS {
        let label = labels
            .iter()
            .find(|label| label.name == expected.name)
            .unwrap();
        assert_eq!(label.color, expected.color);
        assert_eq!(label.description, expected.description);
    }
    // Creating labels needs no PATCH request.
    assert!(github.label_patches(REPOSITORY).is_empty());

    // `mobius:ready` goes missing and `mobius:working` gets a different color.
    github.delete_repository_label(REPOSITORY, "mobius:ready");
    github.add_repository_label(REPOSITORY, "mobius:working", "ededed", "Custom description");

    let checkup = get_checkup(&engine, &cookie, "owner").await;
    let status_of = |name: &str| {
        checkup[0]
            .labels
            .iter()
            .find(|label| label.name == name)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(status_of("mobius:ready"), LabelStatus::Missing);
    assert_eq!(
        status_of("mobius:working"),
        LabelStatus::WrongColor("ededed".to_string())
    );
    assert_eq!(mobius_ui::fix_button(&checkup), Some("Fix labels"));

    assert_eq!(fix(&engine, &cookie, "owner").await, StatusCode::OK);

    let labels = github.repository_labels(REPOSITORY);
    let ready = labels
        .iter()
        .find(|label| label.name == "mobius:ready")
        .unwrap();
    assert_eq!(ready.color, "0E8A16");
    let working = labels
        .iter()
        .find(|label| label.name == "mobius:working")
        .unwrap();
    // The color became the fixed color, the description stayed.
    assert_eq!(working.color, "FBCA04");
    assert_eq!(working.description, "Custom description");
    assert_eq!(github.label_patches(REPOSITORY), ["mobius:working"]);

    let checkup = get_checkup(&engine, &cookie, "owner").await;
    assert!(
        checkup[0]
            .labels
            .iter()
            .all(|label| label.status == LabelStatus::Present)
    );
    assert_eq!(mobius_ui::fix_button(&checkup), None);

    // A label in a different case shows its own status, and it alone shows no button.
    github.add_repository_label(REPOSITORY, "Mobius:Autopilot", "1D76DB", "Autopilot");
    let checkup = get_checkup(&engine, &cookie, "owner").await;
    let status_of = |name: &str| {
        checkup[0]
            .labels
            .iter()
            .find(|label| label.name == name)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(
        status_of("mobius:autopilot"),
        LabelStatus::WrongCase("Mobius:Autopilot".to_string())
    );
    assert_eq!(mobius_ui::fix_button(&checkup), None);

    // The fix skips the label in a different case.
    assert_eq!(fix(&engine, &cookie, "owner").await, StatusCode::OK);
    assert!(
        github
            .repository_labels(REPOSITORY)
            .iter()
            .any(|label| label.name == "Mobius:Autopilot")
    );
    assert_eq!(github.label_patches(REPOSITORY), ["mobius:working"]);
}

#[tokio::test]
async fn the_checkup_shows_the_status_of_each_app_permission_with_the_link_to_fix_it() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_account("owner", "Organization");
    github.add_repository(REPOSITORY);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || {
        workstreams::organizations(&engine)
            .contains(&"owner".to_string())
            .then_some(())
    })
    .await;
    let cookie = cookie(&engine).await;
    let status_of = async |name: &str| {
        get_view(&engine, &cookie, "owner")
            .await
            .permissions
            .unwrap()
            .into_iter()
            .find(|permission| permission.name == name)
            .unwrap()
            .status
    };

    let view = get_view(&engine, &cookie, "owner").await;
    let names: Vec<&str> = view
        .permissions
        .as_ref()
        .unwrap()
        .iter()
        .map(|permission| permission.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "issues",
            "pull_requests",
            "contents",
            "checks",
            "workflows",
            "actions",
            "metadata"
        ]
    );
    assert_eq!(status_of("issues").await, PermissionStatus::Present);

    // The App has no `workflows` permission.
    assert_eq!(
        status_of("workflows").await,
        PermissionStatus::Missing(format!(
            "{}/organizations/owner/settings/apps/{APP_SLUG}/permissions",
            github.url
        ))
    );

    // The App has the permission, and the installation does not.
    let mut permissions = vec![
        ("issues", "write"),
        ("pull_requests", "write"),
        ("contents", "write"),
        ("checks", "write"),
        ("metadata", "read"),
        ("workflows", "write"),
        ("actions", "read"),
    ];
    github.set_app_permissions(APP_ID, &permissions);
    assert_eq!(
        status_of("workflows").await,
        PermissionStatus::NotAccepted(format!(
            "{}/organizations/owner/settings/installations/1",
            github.url
        ))
    );

    // A lower level in the installation is not enough.
    github.set_installation_permissions(APP_ID, &[("workflows", "read")]);
    assert!(matches!(
        status_of("workflows").await,
        PermissionStatus::NotAccepted(_)
    ));
    assert!(matches!(
        status_of("issues").await,
        PermissionStatus::NotAccepted(_)
    ));

    // The installation has each permission, a higher level counts.
    permissions[5] = ("workflows", "admin");
    github.set_installation_permissions(APP_ID, &permissions);
    let view = get_view(&engine, &cookie, "owner").await;
    assert!(
        view.permissions
            .unwrap()
            .iter()
            .all(|permission| permission.status == PermissionStatus::Present)
    );
}

#[tokio::test]
async fn the_checkup_reports_the_missing_actions_read_permission() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_account("owner", "Organization");
    github.add_repository(REPOSITORY);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || {
        workstreams::organizations(&engine)
            .contains(&"owner".to_string())
            .then_some(())
    })
    .await;
    let cookie = cookie(&engine).await;
    let status_of = async |name: &str| {
        get_view(&engine, &cookie, "owner")
            .await
            .permissions
            .unwrap()
            .into_iter()
            .find(|permission| permission.name == name)
            .unwrap()
            .status
    };

    assert_eq!(
        status_of("actions").await,
        PermissionStatus::Missing(format!(
            "{}/organizations/owner/settings/apps/{APP_SLUG}/permissions",
            github.url
        ))
    );
}

#[tokio::test]
async fn the_checkup_shows_the_labels_when_the_check_of_the_app_permissions_fails() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_account("owner", "Organization");
    github.add_repository(REPOSITORY);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || {
        workstreams::organizations(&engine)
            .contains(&"owner".to_string())
            .then_some(())
    })
    .await;
    let cookie = cookie(&engine).await;
    github.fail_installations(APP_ID);

    let view = get_view(&engine, &cookie, "owner").await;
    assert!(view.permissions.is_err());
    assert_eq!(view.repositories.len(), 1);
    assert!(!view.repositories[0].labels.is_empty());
}
