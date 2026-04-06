use std::io::Write;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use coderlm_server::server::routes::build_routes;
use coderlm_server::server::state::AppState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temp project directory with a simple Rust file containing two
/// functions.  Returns the tempdir handle (must stay alive) and the
/// canonical path to the project root.
fn setup_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let src = dir.path().join("lib.rs");
    let mut f = std::fs::File::create(&src).expect("create lib.rs");
    writeln!(
        f,
        r#"
fn foo() {{
    println!("hello");
}}

fn bar() {{
    foo();
}}
"#
    )
    .expect("write lib.rs");

    let canonical = dir.path().canonicalize().expect("canonicalize");
    (dir, canonical)
}

/// Create a session by posting to /api/v1/sessions.  Returns the session id.
async fn create_session(app: &axum::Router, cwd: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(json!({"cwd": cwd}).to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = serde_json::from_slice(&body).unwrap();
    val["session_id"].as_str().unwrap().to_string()
}

/// Wait for symbol indexing to complete.
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

/// Parse response body as JSON.
async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Tests: POST /api/v1/symbols/implementations/batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_batch_implementations_success() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/implementations/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(
            json!({
                "symbols": [
                    {"symbol": "foo", "file": "lib.rs"},
                    {"symbol": "bar", "file": "lib.rs"},
                ]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let val = body_json(resp).await;
    assert_eq!(val["count"], 2);
    assert_eq!(val["successes"], 2);
    assert_eq!(val["errors"], 0);
    assert!(val["indexing_complete"].as_bool().unwrap());

    let results = val["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    // First result should be "foo"
    assert_eq!(results[0]["symbol"], "foo");
    assert!(results[0]["source"].as_str().unwrap().contains("println"));
    assert!(results[0].get("error").is_none());
    // Second result should be "bar"
    assert_eq!(results[1]["symbol"], "bar");
    assert!(results[1]["source"].as_str().unwrap().contains("foo()"));
}

#[tokio::test]
async fn test_batch_implementations_partial_failure() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/implementations/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(
            json!({
                "symbols": [
                    {"symbol": "foo", "file": "lib.rs"},
                    {"symbol": "nonexistent", "file": "lib.rs"},
                ]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let val = body_json(resp).await;
    assert_eq!(val["count"], 2);
    assert_eq!(val["successes"], 1);
    assert_eq!(val["errors"], 1);

    let results = val["results"].as_array().unwrap();
    // First should succeed
    assert_eq!(results[0]["symbol"], "foo");
    assert!(results[0].get("source").is_some());
    // Second should have an error
    assert_eq!(results[1]["symbol"], "nonexistent");
    assert!(results[1]["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn test_batch_implementations_empty_symbols_rejected() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/implementations/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(json!({"symbols": []}).to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let val = body_json(resp).await;
    assert!(val["error"].as_str().unwrap().contains("must not be empty"));
}

#[tokio::test]
async fn test_batch_implementations_no_session_header() {
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/implementations/batch")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "symbols": [{"symbol": "foo", "file": "lib.rs"}]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Tests: POST /api/v1/symbols/callers/batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_batch_callers_success() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/callers/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(
            json!({
                "symbols": [
                    {"symbol": "foo", "file": "lib.rs"},
                    {"symbol": "bar", "file": "lib.rs"},
                ]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let val = body_json(resp).await;
    assert_eq!(val["count"], 2);
    assert_eq!(val["successes"], 2);
    assert_eq!(val["errors"], 0);
    assert!(val["indexing_complete"].as_bool().unwrap());

    let results = val["results"].as_array().unwrap();
    // foo should have callers (called by bar)
    assert_eq!(results[0]["symbol"], "foo");
    let foo_callers = results[0]["callers"].as_array().unwrap();
    assert!(
        !foo_callers.is_empty(),
        "foo should have at least one caller (bar calls foo)"
    );
    // bar should have no callers
    assert_eq!(results[1]["symbol"], "bar");
}

#[tokio::test]
async fn test_batch_callers_partial_failure() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/callers/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(
            json!({
                "symbols": [
                    {"symbol": "foo", "file": "lib.rs"},
                    {"symbol": "missing_sym", "file": "lib.rs"},
                ]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let val = body_json(resp).await;
    assert_eq!(val["successes"], 1);
    assert_eq!(val["errors"], 1);

    let results = val["results"].as_array().unwrap();
    assert!(results[0].get("callers").is_some());
    assert!(results[1]["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn test_batch_callers_empty_symbols_rejected() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/callers/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(json!({"symbols": []}).to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_batch_callers_with_limit() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/callers/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(
            json!({
                "symbols": [{"symbol": "foo", "file": "lib.rs"}],
                "limit": 1
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let val = body_json(resp).await;
    assert_eq!(val["successes"], 1);
}

#[tokio::test]
async fn test_batch_implementations_with_line_disambiguation() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    // Use line number for disambiguation (foo is at line 2 in the test file)
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/symbols/implementations/batch")
        .header("content-type", "application/json")
        .header("x-session-id", &session_id)
        .body(Body::from(
            json!({
                "symbols": [
                    {"symbol": "foo", "file": "lib.rs", "line": 2},
                ]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let val = body_json(resp).await;
    assert_eq!(val["successes"], 1);
    assert_eq!(val["errors"], 0);
}
