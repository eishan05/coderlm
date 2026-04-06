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

async fn get_stats(app: &axum::Router, session_id: &str) -> Value {
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/stats")
        .header("x-session-id", session_id)
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stats_returns_zeros_initially() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;

    let stats = get_stats(&app, &session_id).await;

    assert_eq!(stats["symbol_lookups"], 0);
    assert_eq!(stats["peek_reads"], 0);
    assert_eq!(stats["impl_reads"], 0);
    assert_eq!(stats["grep_ops"], 0);
    assert_eq!(stats["chars_served"], 0);
    assert_eq!(stats["chars_full_file"], 0);
    assert_eq!(stats["chars_saved"], 0);
    assert_eq!(stats["estimated_tokens_served"], 0);
    assert_eq!(stats["estimated_tokens_full_file"], 0);
    assert_eq!(stats["estimated_tokens_saved"], 0);
}

#[tokio::test]
async fn test_stats_requires_session() {
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/stats")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_stats_tracks_symbol_lookups() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    // Perform a symbol search
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/search?q=foo")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Perform a symbol list
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["symbol_lookups"], 2);
}

#[tokio::test]
async fn test_stats_tracks_impl_reads_with_savings() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    // Get implementation of foo
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/implementation?symbol=foo&file=lib.rs")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["impl_reads"], 1);
    assert!(stats["chars_served"].as_u64().unwrap() > 0);
    assert!(stats["chars_full_file"].as_u64().unwrap() > 0);
    // The impl snippet should be smaller than the full file
    assert!(stats["chars_served"].as_u64().unwrap() < stats["chars_full_file"].as_u64().unwrap());
    assert!(stats["chars_saved"].as_u64().unwrap() > 0);
    assert!(stats["estimated_tokens_saved"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_stats_tracks_peek_reads() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;

    // Peek a subset of the file
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/peek?file=lib.rs&start=0&end=3")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["peek_reads"], 1);
    assert!(stats["chars_served"].as_u64().unwrap() > 0);
    assert!(stats["chars_full_file"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_stats_tracks_grep_ops() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;

    // Grep for "foo"
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/grep?pattern=foo")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["grep_ops"], 1);
}

#[tokio::test]
async fn test_stats_tracks_batch_impl() {
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

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["impl_reads"], 2);
    assert!(stats["chars_served"].as_u64().unwrap() > 0);
    assert!(stats["chars_full_file"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_stats_tracks_callers_as_symbol_lookup() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/callers?symbol=foo&file=lib.rs")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["symbol_lookups"], 1);
}

#[tokio::test]
async fn test_stats_token_estimation_uses_4_chars_per_token() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    // Get implementation of foo
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/implementation?symbol=foo&file=lib.rs")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stats = get_stats(&app, &session_id).await;
    let chars_served = stats["chars_served"].as_u64().unwrap();
    let tokens_served = stats["estimated_tokens_served"].as_u64().unwrap();
    assert_eq!(tokens_served, chars_served / 4);
}

#[tokio::test]
async fn test_stats_accumulates_across_multiple_operations() {
    let (_dir, canonical) = setup_project();
    let state = AppState::new(4, 1_000_000);
    let app = build_routes(state);

    let session_id = create_session(&app, canonical.to_str().unwrap()).await;
    wait_for_indexing(&app, &session_id).await;

    // Perform a symbol search
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/search?q=foo")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // Get implementation
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/symbols/implementation?symbol=foo&file=lib.rs")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // Peek
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/peek?file=lib.rs&start=0&end=3")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // Grep
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/grep?pattern=foo")
        .header("x-session-id", &session_id)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    let stats = get_stats(&app, &session_id).await;
    assert_eq!(stats["symbol_lookups"], 1);
    assert_eq!(stats["impl_reads"], 1);
    assert_eq!(stats["peek_reads"], 1);
    assert_eq!(stats["grep_ops"], 1);
    assert!(stats["chars_served"].as_u64().unwrap() > 0);
    assert!(stats["chars_full_file"].as_u64().unwrap() > 0);
}
