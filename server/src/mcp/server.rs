//! MCP server implementation for CodeRLM.
//!
//! Implements the `ServerHandler` trait from `rmcp`, exposing CodeRLM's ops
//! as MCP tools. The server initialises a project from the working directory
//! (or an explicit `cwd` parameter) and calls `ops::*` functions directly,
//! sharing the same `AppState` as the HTTP layer.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::ops::symbol_ops;
use crate::server::state::{AppState, Project};

// ---------------------------------------------------------------------------
// Tool parameter types (derive JsonSchema for MCP schema generation)
// ---------------------------------------------------------------------------

/// Parameters for `coderlm_callers` — find callers of a symbol.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CallersParams {
    /// Symbol name to find callers of. May be a bare name (`"method"`) or a
    /// qualified name (`"ClassName.method"`) to target a specific class method.
    pub symbol: String,
    /// Relative path to the file containing the symbol definition.
    pub file: String,
    /// Maximum number of callers to return. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional line number to disambiguate same-named symbols in the same file.
    #[serde(default)]
    pub line: Option<usize>,
    /// Optional list of path prefixes to restrict the search to. Only files
    /// whose relative path starts with one of these prefixes will be scanned.
    #[serde(default)]
    pub include_paths: Option<Vec<String>>,
    /// Optional list of path prefixes to exclude from the search. Files whose
    /// relative path starts with any of these prefixes will be skipped.
    #[serde(default)]
    pub exclude_paths: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// MCP Server struct
// ---------------------------------------------------------------------------

/// The CodeRLM MCP server. Holds a reference to the shared `AppState` and the
/// project that was initialised from the working directory.
#[derive(Clone)]
pub struct CoderlmMcpServer {
    #[allow(dead_code)]
    state: AppState,
    /// The indexed project. Public so tests can await indexing completion.
    pub project: Arc<Project>,
    tool_router: ToolRouter<Self>,
}

// Manual Debug impl because AppState and Project don't derive Debug.
impl std::fmt::Debug for CoderlmMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoderlmMcpServer")
            .field("project_root", &self.project.root)
            .finish()
    }
}

impl CoderlmMcpServer {
    /// Create a new MCP server for the given working directory.
    ///
    /// Indexes the project (or re-uses an existing one) via `AppState`.
    pub fn new(state: AppState, cwd: &PathBuf) -> Result<Self, String> {
        let project = state
            .get_or_create_project(cwd)
            .map_err(|e| format!("Failed to index project: {}", e))?;

        info!(
            "MCP server initialised for project: {}",
            project.root.display()
        );

        Ok(Self {
            state,
            project,
            tool_router: Self::tool_router(),
        })
    }

    /// Get a reference to the project for convenience.
    fn project(&self) -> &Arc<Project> {
        &self.project
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router]
impl CoderlmMcpServer {
    /// Find all call sites of a symbol across the indexed codebase. Uses
    /// AST-aware analysis for supported languages, with regex fallback.
    #[tool(
        name = "coderlm_callers",
        annotations(read_only_hint = true, title = "Find Callers")
    )]
    pub async fn coderlm_callers(&self, params: Parameters<CallersParams>) -> String {
        // Wait for initial indexing to complete so we don't return false
        // "not found" errors when the symbol table hasn't been populated yet.
        self.project.wait_until_indexed().await;

        let p = &params.0;
        let limit = p.limit.unwrap_or(50);
        let include_refs = p.include_paths.as_deref();
        let exclude_refs = p.exclude_paths.as_deref();
        match symbol_ops::find_callers(
            &self.project().root,
            &self.project().file_tree,
            &self.project().symbol_table,
            &p.symbol,
            &p.file,
            limit,
            p.line,
            include_refs,
            exclude_refs,
        ) {
            Ok(callers) => serde_json::to_string_pretty(&serde_json::json!({
                "callers": callers,
                "count": callers.len(),
                "indexing_complete": self.project().is_indexing_complete(),
            }))
            .unwrap_or_else(|e| format!("Error: {}", e)),
            Err(e) => format!("Error: {}", e),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CoderlmMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "coderlm",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "CodeRLM is a code-aware index server. Use coderlm_callers to \
                 find call sites of a symbol.",
            )
    }
}
