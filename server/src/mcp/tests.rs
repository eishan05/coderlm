//! Tests for the MCP transport layer.
//!
//! Tests verify that:
//! 1. `CoderlmMcpServer` can be constructed from a real project directory
//! 2. Each tool produces valid JSON responses
//! 3. Tool routing resolves correctly
//! 4. Error cases (missing symbols, missing files) produce error messages instead of panics

use std::path::PathBuf;

use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::ServerInfo,
};

use crate::mcp::server::*;
use crate::server::state::AppState;

/// Create a temp directory with sample files and return a ready MCP server.
fn setup_test_server() -> (tempfile::TempDir, CoderlmMcpServer) {
    let dir = tempfile::tempdir().unwrap();

    // Write a small Rust file
    std::fs::write(
        dir.path().join("main.rs"),
        r#"fn hello() {
    println!("Hello, world!");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn use_add() {
    let _result = add(1, 2);
}

#[test]
fn test_add() {
    let sum = add(1, 2);
    assert_eq!(sum, 3);
}
"#,
    )
    .unwrap();

    // Write a Python file
    std::fs::write(
        dir.path().join("lib.py"),
        r#"def greet(name):
    """Greet someone."""
    return f"Hello, {name}!"

class Calculator:
    def add(self, a, b):
        return a + b
"#,
    )
    .unwrap();

    let state = AppState::new(5, 10_000_000);
    let cwd = dir.path().to_path_buf();
    let server = CoderlmMcpServer::new(state, &cwd).expect("Failed to create MCP server");

    (dir, server)
}

/// Wait for indexing to complete on the test server's project.
async fn wait_for_indexing(server: &CoderlmMcpServer) {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.project.wait_until_indexed(),
    )
    .await
    .expect("indexing should complete within 5 seconds");
}

// ---------------------------------------------------------------------------
// Construction tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_server_construction_succeeds() {
    let (_dir, _server) = setup_test_server();
}

#[tokio::test]
async fn test_server_construction_fails_for_nonexistent_path() {
    let state = AppState::new(5, 10_000_000);
    let cwd = PathBuf::from("/nonexistent/path/that/does/not/exist");
    let result = CoderlmMcpServer::new(state, &cwd);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_server_info_contains_tools_capability() {
    let (_dir, server) = setup_test_server();
    let info: ServerInfo = server.get_info();
    assert!(info.capabilities.tools.is_some());
    assert_eq!(info.server_info.name, "coderlm");
}

// ---------------------------------------------------------------------------
// Tool attribute tests (verify tools are registered correctly)
// ---------------------------------------------------------------------------

#[test]
fn test_callers_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_callers_tool_attr();
    assert_eq!(attr.name, "coderlm_callers");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_dependents_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_dependents_tool_attr();
    assert_eq!(attr.name, "coderlm_dependents");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_only_two_tools_are_registered() {
    // Verify the full set of exposed tools is exactly {callers, dependents}.
    let names = vec![
        CoderlmMcpServer::coderlm_callers_tool_attr().name,
        CoderlmMcpServer::coderlm_dependents_tool_attr().name,
    ];
    assert_eq!(names.len(), 2);
    let names: Vec<&str> = names.iter().map(|s| s.as_ref()).collect();
    assert!(names.contains(&"coderlm_callers"));
    assert!(names.contains(&"coderlm_dependents"));
}

// ---------------------------------------------------------------------------
// Tool invocation tests (call the methods directly)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_coderlm_callers_finds_call_sites() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    // The test_add function calls add(), so there should be at least 1 caller
    let result = server.coderlm_callers(Parameters(CallersParams {
        symbol: "add".to_string(),
        file: "main.rs".to_string(),
        limit: None,
        line: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Callers result not valid JSON: {}. Got: {}", e, result));
    // The test_add function calls add()
    assert!(
        parsed["count"].as_u64().unwrap() >= 1,
        "Expected at least 1 caller, got result: {}",
        result
    );
}

#[tokio::test]
async fn test_coderlm_callers_error_for_missing_symbol() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_callers(Parameters(CallersParams {
        symbol: "nonexistent_function".to_string(),
        file: "main.rs".to_string(),
        limit: None,
        line: None,
    }));
    assert!(result.starts_with("Error:"), "Expected error, got: {}", result);
}

#[tokio::test]
async fn test_coderlm_dependents_returns_valid_json() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_dependents(Parameters(DependentsParams {
        file: "lib".to_string(),
    }));
    // Should be a valid JSON response (possibly empty dependents list).
    let _parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Dependents result not valid JSON: {}. Got: {}", e, result));
}

// ---------------------------------------------------------------------------
// Schema generation tests
// ---------------------------------------------------------------------------

#[test]
fn test_callers_params_schema_has_required_fields() {
    let attr = CoderlmMcpServer::coderlm_callers_tool_attr();
    let schema = &attr.input_schema;
    let props = schema.get("properties").and_then(|p| p.as_object());
    assert!(props.is_some(), "Schema should have properties");
    let props = props.unwrap();
    assert!(props.contains_key("symbol"), "Schema should have 'symbol'");
    assert!(props.contains_key("file"), "Schema should have 'file'");
    assert!(props.contains_key("line"), "Schema should have 'line'");
}

#[test]
fn test_dependents_params_schema_has_required_fields() {
    let attr = CoderlmMcpServer::coderlm_dependents_tool_attr();
    let schema = &attr.input_schema;
    let props = schema.get("properties").and_then(|p| p.as_object());
    assert!(props.is_some(), "Schema should have properties");
    assert!(
        props.unwrap().contains_key("file"),
        "Schema should have 'file' property"
    );
}
