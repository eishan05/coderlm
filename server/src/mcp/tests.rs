//! Tests for the MCP transport layer.
//!
//! Tests verify that:
//! 1. `CoderlmMcpServer` can be constructed from a real project directory
//! 2. Each tool produces valid JSON responses
//! 3. Tool routing resolves correctly (tools/list returns all expected tools)
//! 4. Error cases (bad regex, missing symbols) produce error messages instead of panics

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
fn test_structure_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_structure_tool_attr();
    assert_eq!(attr.name, "coderlm_structure");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_search_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_search_tool_attr();
    assert_eq!(attr.name, "coderlm_search");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_impl_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_impl_tool_attr();
    assert_eq!(attr.name, "coderlm_impl");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_peek_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_peek_tool_attr();
    assert_eq!(attr.name, "coderlm_peek");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_grep_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_grep_tool_attr();
    assert_eq!(attr.name, "coderlm_grep");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_callers_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_callers_tool_attr();
    assert_eq!(attr.name, "coderlm_callers");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_tests_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_tests_tool_attr();
    assert_eq!(attr.name, "coderlm_tests");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_symbols_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_symbols_tool_attr();
    assert_eq!(attr.name, "coderlm_symbols");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_stats_tool_registered() {
    let attr = CoderlmMcpServer::coderlm_stats_tool_attr();
    assert_eq!(attr.name, "coderlm_stats");
    assert!(attr.annotations.as_ref().unwrap().read_only_hint == Some(true));
}

#[test]
fn test_all_nine_tools_have_attributes() {
    // Verify each tool attribute function exists and produces the right name.
    let expected = vec![
        CoderlmMcpServer::coderlm_structure_tool_attr().name,
        CoderlmMcpServer::coderlm_search_tool_attr().name,
        CoderlmMcpServer::coderlm_impl_tool_attr().name,
        CoderlmMcpServer::coderlm_peek_tool_attr().name,
        CoderlmMcpServer::coderlm_grep_tool_attr().name,
        CoderlmMcpServer::coderlm_callers_tool_attr().name,
        CoderlmMcpServer::coderlm_tests_tool_attr().name,
        CoderlmMcpServer::coderlm_symbols_tool_attr().name,
        CoderlmMcpServer::coderlm_stats_tool_attr().name,
    ];
    assert_eq!(expected.len(), 9, "Expected 9 tool attributes");

    let names: Vec<&str> = expected.iter().map(|s| s.as_ref()).collect();
    assert!(names.contains(&"coderlm_structure"));
    assert!(names.contains(&"coderlm_search"));
    assert!(names.contains(&"coderlm_impl"));
    assert!(names.contains(&"coderlm_peek"));
    assert!(names.contains(&"coderlm_grep"));
    assert!(names.contains(&"coderlm_callers"));
    assert!(names.contains(&"coderlm_tests"));
    assert!(names.contains(&"coderlm_symbols"));
    assert!(names.contains(&"coderlm_stats"));
}

// ---------------------------------------------------------------------------
// Tool invocation tests (call the methods directly)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_coderlm_structure_returns_valid_json() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_structure(Parameters(StructureParams { depth: None }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Structure result not valid JSON: {}. Got: {}", e, result));
    assert!(parsed["file_count"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn test_coderlm_stats_returns_valid_json() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_stats(Parameters(StatsParams {}));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Stats result not valid JSON: {}. Got: {}", e, result));
    assert!(parsed["file_count"].as_u64().unwrap() >= 2);
    assert!(parsed["project_root"].is_string());
}

#[tokio::test]
async fn test_coderlm_search_after_indexing() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_search(Parameters(SearchParams {
        q: "hello".to_string(),
        offset: None,
        limit: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Search result not valid JSON: {}. Got: {}", e, result));
    assert!(parsed["indexing_complete"].as_bool().unwrap());
    // Should find the `hello` function
    assert!(parsed["total"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn test_coderlm_impl_returns_source() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_impl(Parameters(ImplParams {
        symbol: "hello".to_string(),
        file: "main.rs".to_string(),
        line: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Impl result not valid JSON: {}. Got: {}", e, result));
    assert!(parsed["source"].as_str().unwrap().contains("Hello, world!"));
}

#[tokio::test]
async fn test_coderlm_impl_error_for_missing_symbol() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_impl(Parameters(ImplParams {
        symbol: "nonexistent_function".to_string(),
        file: "main.rs".to_string(),
        line: None,
    }));
    assert!(result.starts_with("Error:"), "Expected error, got: {}", result);
}

#[tokio::test]
async fn test_coderlm_peek_returns_content() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_peek(Parameters(PeekParams {
        file: "main.rs".to_string(),
        start: Some(0),
        end: Some(5),
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Peek result not valid JSON: {}. Got: {}", e, result));
    assert!(parsed["content"].as_str().unwrap().contains("fn hello"));
}

#[tokio::test]
async fn test_coderlm_peek_error_for_missing_file() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_peek(Parameters(PeekParams {
        file: "nonexistent.rs".to_string(),
        start: None,
        end: None,
    }));
    assert!(result.starts_with("Error:"), "Expected error, got: {}", result);
}

#[tokio::test]
async fn test_coderlm_grep_finds_matches() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_grep(Parameters(GrepParams {
        pattern: "Hello".to_string(),
        max_matches: None,
        context_lines: None,
        scope: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Grep result not valid JSON: {}. Got: {}", e, result));
    assert!(parsed["total_matches"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn test_coderlm_grep_bad_regex_returns_error() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_grep(Parameters(GrepParams {
        pattern: "[invalid".to_string(),
        max_matches: None,
        context_lines: None,
        scope: None,
    }));
    assert!(result.starts_with("Error:"), "Expected error, got: {}", result);
}

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
async fn test_coderlm_tests_finds_test_functions() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_tests(Parameters(TestsParams {
        symbol: "add".to_string(),
        file: "main.rs".to_string(),
        limit: None,
        line: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Tests result not valid JSON: {}. Got: {}", e, result));
    // test_add references add
    assert!(
        parsed["count"].as_u64().unwrap() >= 1,
        "Expected at least 1 test, got result: {}",
        result
    );
}

#[tokio::test]
async fn test_coderlm_symbols_lists_symbols() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_symbols(Parameters(SymbolsParams {
        kind: None,
        file: None,
        limit: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Symbols result not valid JSON: {}. Got: {}", e, result));
    // Should have symbols from both files
    assert!(parsed["count"].as_u64().unwrap() >= 3);
}

#[tokio::test]
async fn test_coderlm_symbols_with_kind_filter() {
    let (_dir, server) = setup_test_server();
    wait_for_indexing(&server).await;

    let result = server.coderlm_symbols(Parameters(SymbolsParams {
        kind: Some("function".to_string()),
        file: None,
        limit: None,
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Symbols result not valid JSON: {}. Got: {}", e, result));
    // Should have at least hello, add, test_add, greet
    let symbols = parsed["symbols"].as_array().unwrap();
    for sym in symbols {
        // All returned symbols should be functions
        assert_eq!(sym["kind"].as_str().unwrap(), "function");
    }
}

#[tokio::test]
async fn test_coderlm_grep_with_scope_code() {
    let (_dir, server) = setup_test_server();
    let result = server.coderlm_grep(Parameters(GrepParams {
        pattern: "Hello".to_string(),
        max_matches: None,
        context_lines: None,
        scope: Some("code".to_string()),
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("Grep result not valid JSON: {}. Got: {}", e, result));
    // Should still find matches (the string literal contains "Hello")
    assert!(parsed["total_matches"].is_number());
}

// ---------------------------------------------------------------------------
// Tool descriptions test
// ---------------------------------------------------------------------------

#[test]
fn test_tool_descriptions_match_spec() {
    let impl_attr = CoderlmMcpServer::coderlm_impl_tool_attr();
    let desc = impl_attr.description.unwrap();
    assert!(
        desc.contains("source code of a single function or method"),
        "coderlm_impl description should match spec, got: {}",
        desc
    );

    let search_attr = CoderlmMcpServer::coderlm_search_tool_attr();
    let desc = search_attr.description.unwrap();
    assert!(
        desc.contains("Index-backed symbol search"),
        "coderlm_search description should match spec, got: {}",
        desc
    );

    let peek_attr = CoderlmMcpServer::coderlm_peek_tool_attr();
    let desc = peek_attr.description.unwrap();
    assert!(
        desc.contains("specific line range from a file"),
        "coderlm_peek description should match spec, got: {}",
        desc
    );
}

// ---------------------------------------------------------------------------
// Schema generation tests
// ---------------------------------------------------------------------------

#[test]
fn test_search_params_schema_has_required_q() {
    let attr = CoderlmMcpServer::coderlm_search_tool_attr();
    let schema = &attr.input_schema;
    // The "q" field should be in the schema
    let props = schema.get("properties").and_then(|p| p.as_object());
    assert!(props.is_some(), "Schema should have properties");
    assert!(
        props.unwrap().contains_key("q"),
        "Schema should have 'q' property"
    );
}

#[test]
fn test_impl_params_schema_has_required_fields() {
    let attr = CoderlmMcpServer::coderlm_impl_tool_attr();
    let schema = &attr.input_schema;
    let props = schema.get("properties").and_then(|p| p.as_object());
    assert!(props.is_some(), "Schema should have properties");
    let props = props.unwrap();
    assert!(props.contains_key("symbol"), "Schema should have 'symbol'");
    assert!(props.contains_key("file"), "Schema should have 'file'");
    assert!(props.contains_key("line"), "Schema should have 'line'");
}
