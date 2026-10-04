use std::time::{Duration, Instant};

use dioxus::server::axum::body::{Body, to_bytes};
use dioxus::server::axum::http::{Request, StatusCode, header};
use dioxus::server::axum::{Extension, Router};
use dioxus::server::{DioxusRouterExt, FullstackState, ServerFunction};
// Links the server functions of `mobius-api` into this test binary.
use mobius_api as _;
use mobius_domain::Devices;
use mobius_engine::{Engine, auth};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::start;
use tempfile::TempDir;
use tower::ServiceExt;

fn api(engine: &Engine) -> Router {
    Router::new()
        .register_server_functions()
        .with_state(FullstackState::headless())
        .layer(Extension(engine.clone()))
        .layer(Extension(engine.store.clone()))
}

#[tokio::test]
async fn login_with_the_access_password_gives_a_token_that_check_accepts() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;

    let token = auth::login(&engine, "correct horse", "Firefox")
        .await
        .unwrap()
        .unwrap();

    assert!(auth::check(&engine, &token).await.unwrap().is_some());
}

#[tokio::test]
async fn login_with_a_wrong_password_fails_after_one_second() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;

    let started = Instant::now();
    let token = auth::login(&engine, "battery staple", "Firefox")
        .await
        .unwrap();

    assert_eq!(token, None);
    assert!(started.elapsed() >= Duration::from_secs(1));
}

#[tokio::test]
async fn logout_makes_check_refuse_the_token() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let token = auth::login(&engine, "correct horse", "Firefox")
        .await
        .unwrap()
        .unwrap();
    let device = auth::check(&engine, &token).await.unwrap().unwrap();

    auth::logout(&engine, device).await.unwrap();

    assert_eq!(auth::check(&engine, &token).await.unwrap(), None);
}

#[tokio::test]
async fn start_keeps_the_logins_until_the_access_password_changes() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let token = auth::login(&engine, "correct horse", "Firefox")
        .await
        .unwrap()
        .unwrap();
    engine.store.pool.close().await;

    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    assert!(auth::check(&engine, &token).await.unwrap().is_some());
    engine.store.pool.close().await;

    let engine = start(data_dir.path(), "battery staple", &github.url).await;
    assert_eq!(auth::check(&engine, &token).await.unwrap(), None);
}

#[tokio::test]
async fn login_sets_the_session_cookie_that_opens_the_api() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;

    let response = api(&engine)
        .oneshot(
            Request::post("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "Firefox")
                .body(Body::from(r#"{"password":"correct horse"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    let (cookie, attributes) = set_cookie.split_once("; ").unwrap();
    assert!(cookie.starts_with("mobius_session="));
    assert_eq!(
        attributes,
        "HttpOnly; Path=/; SameSite=Lax; Max-Age=34560000"
    );

    let response = api(&engine)
        .oneshot(
            Request::get("/api/devices")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let devices: Devices =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(devices.logins.len(), 1);
    assert_eq!(devices.logins[0].user_agent, "Firefox");
    assert_eq!(devices.this_device, devices.logins[0].id);
}

#[tokio::test]
async fn each_api_path_except_login_needs_the_session_cookie() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let functions: Vec<_> = ServerFunction::collect()
        .into_iter()
        .filter(|function| function.path() != "/api/login")
        .collect();
    assert!(!functions.is_empty());

    for function in functions {
        let response = api(&engine)
            .oneshot(
                Request::builder()
                    .method(function.method())
                    .uri(function.path())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{} {}",
            function.method(),
            function.path()
        );
    }
}
