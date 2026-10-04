use mobius_engine::github;
use mobius_testkit::fake_github::{self, FakeGitHub};
use mobius_testkit::{start, wait_for};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

#[tokio::test]
async fn manifest_form_posts_the_manifest_to_the_settings_of_an_organization() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_account("acme", "Organization");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;

    let form = github::manifest_form(
        &engine,
        "acme",
        " Mobius acme ",
        "https://mobius.example.ts.net",
    )
    .await
    .unwrap();

    assert_eq!(
        form.url,
        format!("{}/organizations/acme/settings/apps/new", github.url)
    );
    let manifest: Value = serde_json::from_str(&form.manifest).unwrap();
    assert_eq!(
        manifest,
        json!({
            "name": "Mobius acme",
            "url": "https://github.com/Mobius-Toolkit/Mobius",
            "redirect_url": "https://mobius.example.ts.net/api/github/manifest-callback",
            "callback_urls": ["https://mobius.example.ts.net/api/github/user-callback"],
            "request_oauth_on_install": true,
            "public": false,
            "default_permissions": {
                "issues": "write",
                "pull_requests": "write",
                "contents": "write",
                "checks": "write",
                "workflows": "write",
                "actions": "read",
                "metadata": "read"
            }
        })
    );
}

#[tokio::test]
async fn manifest_form_posts_the_manifest_to_the_personal_settings_of_a_user() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_account("owner", "User");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;

    let form = github::manifest_form(&engine, "owner", "Mobius owner", "http://127.0.0.1:8080")
        .await
        .unwrap();

    assert_eq!(form.url, format!("{}/settings/apps/new", github.url));
}

#[tokio::test]
async fn manifest_callback_stores_the_app() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;

    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();

    let app = engine
        .store
        .github_apps()
        .get(fake_github::APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(app.app_id, fake_github::APP_ID);
    assert_eq!(app.slug, fake_github::APP_SLUG);
    assert_eq!(app.private_key, fake_github::APP_PRIVATE_KEY);
    assert_eq!(app.client_id, fake_github::APP_CLIENT_ID);
    assert_eq!(app.client_secret, fake_github::APP_CLIENT_SECRET);
    assert_eq!(app.user_token, None);
    assert_eq!(app.refresh_token, None);
    assert_eq!(app.user_token_expires_at, None);
}

#[tokio::test]
async fn user_callback_stores_the_tokens_and_a_new_authorization_replaces_them() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_user_code("first-code", "owner");
    github.add_user_code("second-code", "owner");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();

    assert!(github::authorize_user(&engine, "first-code").await.unwrap());
    let app = engine
        .store
        .github_apps()
        .get(fake_github::APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(app.user_token.as_deref(), Some("ghu_1"));
    assert_eq!(app.refresh_token.as_deref(), Some("ghr_1"));
    assert!(app.user_token_expires_at.unwrap() > OffsetDateTime::now_utc() + Duration::hours(7));

    assert!(
        github::authorize_user(&engine, "second-code")
            .await
            .unwrap()
    );
    let app = engine
        .store
        .github_apps()
        .get(fake_github::APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(app.user_token.as_deref(), Some("ghu_2"));
    assert_eq!(app.refresh_token.as_deref(), Some("ghr_2"));
}

#[tokio::test]
async fn user_callback_refuses_a_login_that_is_not_trusted() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_user_code("mallory-code", "mallory");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();

    assert!(
        !github::authorize_user(&engine, "mallory-code")
            .await
            .unwrap()
    );

    let app = engine
        .store
        .github_apps()
        .get(fake_github::APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(app.user_token, None);
    assert_eq!(app.refresh_token, None);
}

#[tokio::test]
async fn manifest_callback_adds_a_second_app() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("first-code");
    github.add_manifest_code("second-code");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "first-code")
        .await
        .unwrap();

    github::convert_manifest(&engine, "second-code")
        .await
        .unwrap();

    let slugs: Vec<String> = engine
        .store
        .github_apps()
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|app| app.slug)
        .collect();
    assert_eq!(slugs, [fake_github::APP_SLUG, fake_github::SECOND_APP_SLUG]);
}

#[tokio::test]
async fn user_callback_stores_the_tokens_on_the_app_of_the_code() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("first-code");
    github.add_manifest_code("second-code");
    github.add_second_app_user_code("user-code", "owner");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "first-code")
        .await
        .unwrap();
    github::convert_manifest(&engine, "second-code")
        .await
        .unwrap();

    assert!(github::authorize_user(&engine, "user-code").await.unwrap());

    let apps = engine.store.github_apps();
    let first = apps.get(fake_github::APP_ID).await.unwrap().unwrap();
    let second = apps.get(fake_github::SECOND_APP_ID).await.unwrap().unwrap();
    assert_eq!(first.user_token, None);
    assert_eq!(second.user_token.as_deref(), Some("ghu_1"));
}

#[tokio::test]
async fn user_callback_compares_the_login_without_letter_case() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_user_code("user-code", "Owner");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();

    assert!(github::authorize_user(&engine, "user-code").await.unwrap());
}

#[tokio::test]
async fn many_release_reads_make_one_call_to_github() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository("owner/shop");
    github.set_release("v9.9.9", &[]);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (github.latest_release_calls() == 1).then_some(())).await;

    for _ in 0..20 {
        github::new_release(&engine);
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_eq!(github.latest_release_calls(), 1);
    // The App does not exist before `convert_manifest`, so no token exists for these calls.
    assert_eq!(
        github.unauthenticated_requests(),
        ["POST /app-manifests/manifest-code/conversions"]
    );
}
