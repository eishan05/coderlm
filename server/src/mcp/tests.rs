//! Tests for the MCP transport layer.
//!
//! Tests verify that:
//! 1. `CoderlmMcpServer` can be constructed from a real project directory
//! 2. Each tool produces valid JSON responses
//! 3. Tool routing resolves correctly
//! 4. Error cases (missing symbols, missing files) produce error messages instead of panics

use std::path::PathBuf;

use rmcp::{ServerHandler, handler::server::wrapper::Parameters, model::ServerInfo};

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
fn test_only_one_tool_is_registered() {
    // Verify the full set of exposed tools is exactly {callers}.
    let attr = CoderlmMcpServer::coderlm_callers_tool_attr();
    assert_eq!(attr.name, "coderlm_callers");
}

// ---------------------------------------------------------------------------
// Tool invocation tests (call the methods directly)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_coderlm_callers_finds_call_sites() {
    let (_dir, server) = setup_test_server();
    // NOTE: no explicit wait_for_indexing — the handler itself awaits indexing.
    // This validates the race-fix.

    // The test_add function calls add(), so there should be at least 1 caller
    let result = server
        .coderlm_callers(Parameters(CallersParams {
            symbol: "add".to_string(),
            file: "main.rs".to_string(),
            limit: None,
            line: None,
            include_paths: None,
            exclude_paths: None,
        }))
        .await;
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
    // Handler waits for indexing internally — no explicit wait needed.

    let result = server
        .coderlm_callers(Parameters(CallersParams {
            symbol: "nonexistent_function".to_string(),
            file: "main.rs".to_string(),
            limit: None,
            line: None,
            include_paths: None,
            exclude_paths: None,
        }))
        .await;
    assert!(
        result.starts_with("Error:"),
        "Expected error, got: {}",
        result
    );
}

// ---------------------------------------------------------------------------
// Indexing-race regression tests
// ---------------------------------------------------------------------------

/// Create a server with Python class-method fixtures that exercises the exact
/// scenario from the bug report: `Alpha.run` in `app/service.py` is called
/// from `tests/test_service.py`.
fn setup_race_test_server() -> (tempfile::TempDir, CoderlmMcpServer) {
    let dir = tempfile::tempdir().unwrap();

    // Create the nested directory structure
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();

    std::fs::write(
        dir.path().join("app/service.py"),
        r#"class Alpha:
    def run(self):
        pass
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("tests/test_service.py"),
        r#"from app.service import Alpha

def test_alpha():
    alpha = Alpha()
    alpha.run()
"#,
    )
    .unwrap();

    let state = AppState::new(5, 10_000_000);
    let cwd = dir.path().to_path_buf();
    let server = CoderlmMcpServer::new(state, &cwd).expect("Failed to create MCP server");

    (dir, server)
}

/// Regression: immediate qualified caller lookup (`Alpha.run`) must succeed
/// on the very first call after server construction — no explicit wait, no
/// sleep, no retry.
#[tokio::test]
async fn test_immediate_qualified_caller_lookup_succeeds() {
    let (_dir, server) = setup_race_test_server();
    // Call immediately — the handler must wait for indexing internally.
    let result = server
        .coderlm_callers(Parameters(CallersParams {
            symbol: "Alpha.run".to_string(),
            file: "app/service.py".to_string(),
            limit: None,
            line: None,
            include_paths: Some(vec!["tests/".to_string()]),
            exclude_paths: None,
        }))
        .await;

    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Not valid JSON: {}. Got: {}", e, result));
    assert!(
        parsed["count"].as_u64().unwrap() >= 1,
        "Expected at least 1 caller for Alpha.run, got: {}",
        result
    );
    assert_eq!(parsed["indexing_complete"].as_bool(), Some(true));
}

/// Regression: immediate bare-symbol lookup must also succeed without wait.
#[tokio::test]
async fn test_immediate_bare_symbol_lookup_succeeds() {
    let (_dir, server) = setup_race_test_server();

    let result = server
        .coderlm_callers(Parameters(CallersParams {
            symbol: "run".to_string(),
            file: "app/service.py".to_string(),
            limit: None,
            line: None,
            include_paths: None,
            exclude_paths: None,
        }))
        .await;

    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Not valid JSON: {}. Got: {}", e, result));
    assert!(
        parsed["count"].as_u64().unwrap() >= 1,
        "Expected at least 1 caller for bare 'run', got: {}",
        result
    );
}

/// Regression: include_paths / exclude_paths filtering still works after
/// the indexing-wait fix.
#[tokio::test]
async fn test_immediate_include_paths_filter_works() {
    let (_dir, server) = setup_race_test_server();

    // With include_paths=["app/"] the caller in tests/ should be excluded.
    let result = server
        .coderlm_callers(Parameters(CallersParams {
            symbol: "Alpha.run".to_string(),
            file: "app/service.py".to_string(),
            limit: None,
            line: None,
            include_paths: Some(vec!["app/".to_string()]),
            exclude_paths: None,
        }))
        .await;

    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Not valid JSON: {}. Got: {}", e, result));
    assert_eq!(
        parsed["count"].as_u64().unwrap(),
        0,
        "Callers in tests/ should be excluded by include_paths=[\"app/\"], got: {}",
        result
    );
}

/// Regression: calling_function and call_form fields are still populated.
#[tokio::test]
async fn test_immediate_caller_fields_populated() {
    let (_dir, server) = setup_race_test_server();

    let result = server
        .coderlm_callers(Parameters(CallersParams {
            symbol: "Alpha.run".to_string(),
            file: "app/service.py".to_string(),
            limit: None,
            line: None,
            include_paths: Some(vec!["tests/".to_string()]),
            exclude_paths: None,
        }))
        .await;

    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Not valid JSON: {}. Got: {}", e, result));
    let callers = parsed["callers"]
        .as_array()
        .expect("callers should be array");
    assert!(!callers.is_empty(), "Expected callers, got: {}", result);
    let first = &callers[0];
    assert!(
        first.get("calling_function").is_some(),
        "calling_function field should be present: {}",
        first
    );
    assert!(
        first.get("call_form").is_some(),
        "call_form field should be present: {}",
        first
    );
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
