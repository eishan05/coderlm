use std::io::Write;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use coderlm_server::server::routes::build_routes;
use coderlm_server::server::state::AppState;

fn setup_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create tempdir");

    std::fs::create_dir_all(dir.path().join("src")).expect("create src");
    std::fs::create_dir_all(dir.path().join("app")).expect("create app");
    std::fs::create_dir_all(dir.path().join("tests")).expect("create tests");

    let mut def = std::fs::File::create(dir.path().join("src").join("foo.rs")).expect("create foo");
    writeln!(
        def,
        r#"
pub fn foo() {{
    println!("hello");
}}
"#
    )
    .expect("write foo");

    let mut app = std::fs::File::create(dir.path().join("app").join("main.rs")).expect("create app main");
    writeln!(
        app,
        r#"
fn run() {{
    foo();
}}
"#
    )
    .expect("write app main");

    let mut test_file =
        std::fs::File::create(dir.path().join("tests").join("test_foo.rs")).expect("create test");
    writeln!(
        test_file,
        r#"
#[test]
fn test_foo() {{
    foo();
}}
"#
    )
    .expect("write test");

    let canonical = dir.path().canonicalize().expect("canonicalize");
    (dir, canonical)
}

async fn create_session(app: &axum::Router, cwd: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"cwd":"{}"}}"#, cwd)))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = serde_json::from_slice(&body).unwrap();
    val["session_id"].as_str().unwrap().to_string()
}

async fn wait_for_indexing(app: &axum::Router, session_id: &str) {
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/ready?wait=true")
        .header("x-session-id", session_id)
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn test_get_callers_respects_include_paths() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/callers?symbol=foo&file=src/foo.rs&include_path=app/")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let val = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "unexpected response body: {}", val);

    let callers = val["callers"].as_array().expect("callers should be array");
    assert_eq!(callers.len(), 1, "Expected only app/ caller, got: {}", val);
    assert_eq!(callers[0]["file"].as_str(), Some("app/main.rs"));
    assert_eq!(callers[0]["calling_function"].as_str(), Some("run"));
}
