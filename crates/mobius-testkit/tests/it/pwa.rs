use dioxus::server::axum::body::{Body, to_bytes};
use dioxus::server::axum::http::{Request, StatusCode, header};
use dioxus::server::axum::{Extension, Router};
// Links the server functions of `mobius-api` into this test binary.
use mobius_api as _;
use mobius_engine::{Engine, auth};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::start;
use tempfile::TempDir;
use tower::ServiceExt;

async fn app() -> (Router, TempDir, FakeGitHub, Engine) {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let router = mobius_ui::router()
        .layer(Extension(engine.clone()))
        .layer(Extension(engine.store.clone()));
    (router, data_dir, github, engine)
}

async fn get(router: &Router, path: &str) -> dioxus::server::axum::response::Response {
    router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_shell_and_the_pwa_files_come_with_cache_control_no_cache() {
    let (router, _data_dir, _github, _engine) = app().await;

    for path in ["/", "/sw.js", "/manifest.webmanifest", "/workstreams"] {
        let response = get(&router, path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-cache",
            "{path}"
        );
    }

    // The immutable files keep the default caching of the server.
    let response = get(&router, "/icon.svg").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_ne!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .map(|value| value.to_str().unwrap()),
        Some("no-cache")
    );
}

#[tokio::test]
async fn the_shell_links_the_manifest_and_registers_the_service_worker() {
    let (router, _data_dir, _github, _engine) = app().await;

    let response = get(&router, "/").await;
    let html = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(html.to_vec()).unwrap();
    assert!(html.contains(r#"rel="manifest""#));
    assert!(html.contains(r#"href="/manifest.webmanifest""#));
    assert!(html.contains(r##"name="theme-color" content="#2d5f8b""##));
    assert!(html.contains(r#"name="apple-mobile-web-app-capable""#));
    assert!(html.contains(r#"name="apple-mobile-web-app-title" content="Mobius""#));
    assert!(html.contains("navigator.serviceWorker?.register('/sw.js')"));
}

#[tokio::test]
async fn the_manifest_describes_the_installable_app() {
    let (router, _data_dir, _github, _engine) = app().await;

    let response = get(&router, "/manifest.webmanifest").await;
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/manifest+json"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(manifest["name"], "Mobius");
    assert_eq!(manifest["short_name"], "Mobius");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["scope"], "/");
    assert_eq!(manifest["display"], "standalone");
    let icons = manifest["icons"].as_array().unwrap();
    for (src, purpose) in [
        ("/icon-192.png", "any"),
        ("/icon-512.png", "any"),
        ("/icon-maskable-512.png", "maskable"),
    ] {
        assert!(
            icons
                .iter()
                .any(|icon| icon["src"] == src && icon["purpose"] == purpose),
            "{src}"
        );
    }
    assert!(icons.iter().any(|icon| icon["src"] == "/icon.svg"));
}

#[tokio::test]
async fn the_ui_version_reports_the_build_of_the_server() {
    let (router, _data_dir, _github, engine) = app().await;

    let response = get(&router, "/ui-version").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let token = auth::login(&engine, "correct horse", "Firefox")
        .await
        .unwrap()
        .unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::get("/ui-version")
                .header(header::COOKIE, format!("mobius_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The app revalidates the build, so the answer must not be cached.
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let build = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(build, mobius_ui::BUILD);
}

#[tokio::test]
async fn the_service_worker_passes_each_request_to_the_network() {
    let (router, _data_dir, _github, _engine) = app().await;

    let response = get(&router, "/sw.js").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let script = String::from_utf8(body.to_vec()).unwrap();
    assert!(script.contains("skipWaiting()"));
    assert!(script.contains("clients.claim()"));
    assert!(script.contains("respondWith(fetch(event.request))"));
    // The service worker keeps no data in a cache.
    assert!(!script.contains("caches"));
}
