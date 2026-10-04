use std::fs;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mobius_domain::{Author, Session};
use mobius_engine::{Engine, chat, gh, github, workstreams};
use mobius_testkit::fake_github::{self, FakeGitHub};
use mobius_testkit::{install_fake_agent, start, wait_for};
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const SCRIPT: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
shell = "gh issue list"
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_user_code("user-code", "owner");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    install_fake_agent(data_dir.path(), FAKE_AGENT, SCRIPT);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    assert!(github::authorize_user(&engine, "user-code").await.unwrap());
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

async fn expire_user_token(engine: &Engine, refresh_token: &str) {
    engine
        .store
        .github_apps()
        .set_user_tokens(
            fake_github::APP_ID,
            "ghu_1",
            refresh_token,
            OffsetDateTime::now_utc() - Duration::minutes(1),
        )
        .await
        .unwrap();
}

async fn lead_reply(engine: &Engine) -> (Session, String) {
    chat::send(engine, "owner", REPOSITORY, 12, "List the issues")
        .await
        .unwrap();
    let session = wait_for(async || {
        engine
            .store
            .sessions()
            .list("owner", REPOSITORY, 12)
            .await
            .unwrap()
            .into_iter()
            .next()
            .filter(|session| session.ended_at.is_some())
    })
    .await;
    let reply = engine
        .store
        .chat_messages()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.author == Author::Lead)
        .unwrap()
        .text;
    (session, reply)
}

async fn get_token(engine: &Engine, path: &str) -> StatusCode {
    gh::router(engine.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn the_gh_token_route_serves_only_a_live_chat_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;

    let (_, reply) = lead_reply(&engine).await;

    assert_eq!(reply, "gh issue list with GH_TOKEN=ghu_1\nexit 0");
    let mcp_url = fs::read_to_string(data_dir.path().join("harnesses/mcp_url")).unwrap();
    let key = &mcp_url[mcp_url.find("/mcp/").unwrap() + "/mcp/".len()..];
    for path in [format!("/gh-token/{key}"), "/gh-token/0123".to_string()] {
        assert_eq!(
            get_token(&engine, &path).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn gh_refreshes_an_expired_user_token() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    expire_user_token(&engine, "ghr_1").await;

    let (_, reply) = lead_reply(&engine).await;

    assert_eq!(reply, "gh issue list with GH_TOKEN=ghu_2\nexit 0");
    let app = engine
        .store
        .github_apps()
        .get(fake_github::APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(app.user_token.as_deref(), Some("ghu_2"));
    assert_eq!(app.refresh_token.as_deref(), Some("ghr_2"));
    assert!(app.user_token_expires_at.unwrap() > OffsetDateTime::now_utc() + Duration::hours(7));
}

#[tokio::test]
async fn gh_prints_the_authorize_url_when_the_refresh_fails() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    expire_user_token(&engine, "ghr_wrong").await;

    let (_, reply) = lead_reply(&engine).await;

    // Before this text, `curl` prints its own line about the HTTP status.
    let error = format!(
        "\nThe refresh token passed is incorrect or expired. Tell the Owner to open {}/login/oauth/authorize?client_id=Iv23test and authorize the Mobius App.\nexit 1",
        github.url
    );
    assert!(reply.ends_with(&error), "{reply}");
}
