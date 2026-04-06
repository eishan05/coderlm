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
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::ops::{content, structure, symbol_ops};
use crate::server::state::{AppState, Project};
use crate::symbols::symbol::SymbolKind;

// ---------------------------------------------------------------------------
// Tool parameter types (derive JsonSchema for MCP schema generation)
// ---------------------------------------------------------------------------

/// Parameters for `coderlm_structure` — show the file tree of the indexed project.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct StructureParams {
    /// Maximum directory depth to display. 0 = full tree.
    #[serde(default)]
    pub depth: Option<usize>,
}

/// Parameters for `coderlm_search` — index-backed symbol search.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchParams {
    /// Search query (substring match on symbol names).
    pub q: String,
    /// Number of results to skip (for pagination).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of results to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for `coderlm_impl` — get the source code of a symbol.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ImplParams {
    /// Symbol name (function, method, class, etc.).
    pub symbol: String,
    /// Relative path to the file containing the symbol.
    pub file: String,
    /// Optional line number to disambiguate same-named symbols in the same file.
    #[serde(default)]
    pub line: Option<usize>,
}

/// Parameters for `coderlm_peek` — read specific lines from a file.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PeekParams {
    /// Relative path to the file.
    pub file: String,
    /// Start line (0-based index). Defaults to 0.
    #[serde(default)]
    pub start: Option<usize>,
    /// End line (exclusive, 0-based index). Defaults to 100.
    #[serde(default)]
    pub end: Option<usize>,
}

/// Parameters for `coderlm_grep` — regex search across all indexed files.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GrepParams {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Maximum number of matches to return. Defaults to 50.
    #[serde(default)]
    pub max_matches: Option<usize>,
    /// Number of context lines before/after each match. Defaults to 2.
    #[serde(default)]
    pub context_lines: Option<usize>,
    /// Scope filter: "all" (default) or "code" (skip comments/strings).
    #[serde(default)]
    pub scope: Option<String>,
}

/// Parameters for `coderlm_callers` — find callers of a symbol.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CallersParams {
    /// Symbol name to find callers of.
    pub symbol: String,
    /// Relative path to the file containing the symbol definition.
    pub file: String,
    /// Maximum number of callers to return. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional line number to disambiguate same-named symbols in the same file.
    #[serde(default)]
    pub line: Option<usize>,
}

/// Parameters for `coderlm_tests` — find tests that reference a symbol.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TestsParams {
    /// Symbol name to find tests for.
    pub symbol: String,
    /// Relative path to the file containing the symbol definition.
    pub file: String,
    /// Maximum number of tests to return. Defaults to 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional line number to disambiguate same-named symbols in the same file.
    #[serde(default)]
    pub line: Option<usize>,
}

/// Parameters for `coderlm_outline` — get a structured file outline.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct OutlineParams {
    /// Relative path to the file to outline.
    pub file: String,
}

/// Parameters for `coderlm_symbols` — list symbols in the index.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SymbolsParams {
    /// Filter by symbol kind: "function", "class", "method", "struct",
    /// "trait", "enum", "interface", "module", "constant", "variable",
    /// "type_alias", "macro".
    #[serde(default)]
    pub kind: Option<String>,
    /// Filter by file path (relative).
    #[serde(default)]
    pub file: Option<String>,
    /// Maximum number of symbols to return. Defaults to 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// (Empty) Parameters for `coderlm_stats`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct StatsParams {}

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
    /// Returns the file tree structure of the indexed project, showing files,
    /// directories, and their annotations. Use to understand project layout
    /// before diving into specific files.
    #[tool(
        name = "coderlm_structure",
        annotations(read_only_hint = true, title = "Project Structure")
    )]
    pub fn coderlm_structure(&self, params: Parameters<StructureParams>) -> String {
        let depth = params.0.depth.unwrap_or(0);
        let result = structure::get_structure(&self.project().file_tree, depth);
        serde_json::to_string_pretty(&result).unwrap_or_else(|e| format!("Error: {}", e))
    }

    /// Index-backed symbol search. Faster and more precise than grep for
    /// finding functions, classes, and types. Returns matching symbols with
    /// their file locations, kinds, and signatures.
    #[tool(
        name = "coderlm_search",
        annotations(read_only_hint = true, title = "Symbol Search")
    )]
    pub fn coderlm_search(&self, params: Parameters<SearchParams>) -> String {
        let offset = params.0.offset.unwrap_or(0);
        let limit = params.0.limit.unwrap_or(20);
        let result = symbol_ops::search_symbols(&self.project().symbol_table, &params.0.q, offset, limit);
        serde_json::to_string_pretty(&serde_json::json!({
            "symbols": result.symbols,
            "total": result.total,
            "offset": offset,
            "limit": limit,
            "indexing_complete": self.project().is_indexing_complete(),
        }))
        .unwrap_or_else(|e| format!("Error: {}", e))
    }

    /// Returns the source code of a single function or method. Use instead of
    /// reading the entire file. Extracts just the symbol's implementation from
    /// the AST, saving context window tokens.
    #[tool(
        name = "coderlm_impl",
        annotations(read_only_hint = true, title = "Symbol Implementation")
    )]
    pub fn coderlm_impl(&self, params: Parameters<ImplParams>) -> String {
        let p = &params.0;
        match symbol_ops::get_implementation(
            &self.project().root,
            &self.project().symbol_table,
            &p.symbol,
            &p.file,
            p.line,
        ) {
            Ok(source) => serde_json::to_string_pretty(&serde_json::json!({
                "symbol": p.symbol,
                "file": p.file,
                "source": source,
                "indexing_complete": self.project().is_indexing_complete(),
            }))
            .unwrap_or_else(|e| format!("Error: {}", e)),
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Returns a specific line range from a file. Use for targeted reading
    /// instead of loading full files. Lines are 0-indexed; the result includes
    /// line numbers for reference.
    #[tool(
        name = "coderlm_peek",
        annotations(read_only_hint = true, title = "Peek at File")
    )]
    pub fn coderlm_peek(&self, params: Parameters<PeekParams>) -> String {
        let p = &params.0;
        let start = p.start.unwrap_or(0);
        let end = p.end.unwrap_or(100);
        match content::peek(
            &self.project().root,
            &self.project().file_tree,
            &p.file,
            start,
            end,
        ) {
            Ok(result) => serde_json::to_string_pretty(&result)
                .unwrap_or_else(|e| format!("Error: {}", e)),
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Regex search across all indexed files. Supports optional scope
    /// filtering to search only in code (skipping comments and strings).
    #[tool(
        name = "coderlm_grep",
        annotations(read_only_hint = true, title = "Grep Search")
    )]
    pub fn coderlm_grep(&self, params: Parameters<GrepParams>) -> String {
        let p = &params.0;
        let max_matches = p.max_matches.unwrap_or(50);
        let context_lines = p.context_lines.unwrap_or(2);
        let scope = p
            .scope
            .as_deref()
            .and_then(content::GrepScope::from_str)
            .unwrap_or(content::GrepScope::All);

        match content::grep_with_scope(
            &self.project().root,
            &self.project().file_tree,
            &p.pattern,
            max_matches,
            context_lines,
            scope,
        ) {
            Ok(result) => serde_json::to_string_pretty(&result)
                .unwrap_or_else(|e| format!("Error: {}", e)),
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Find all call sites of a symbol across the indexed codebase. Uses
    /// AST-aware analysis for supported languages, with regex fallback.
    #[tool(
        name = "coderlm_callers",
        annotations(read_only_hint = true, title = "Find Callers")
    )]
    pub fn coderlm_callers(&self, params: Parameters<CallersParams>) -> String {
        let p = &params.0;
        let limit = p.limit.unwrap_or(50);
        match symbol_ops::find_callers(
            &self.project().root,
            &self.project().file_tree,
            &self.project().symbol_table,
            &p.symbol,
            &p.file,
            limit,
            p.line,
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

    /// Find test functions that reference a given symbol. Searches across
    /// the entire index using language-specific test detection heuristics.
    #[tool(
        name = "coderlm_tests",
        annotations(read_only_hint = true, title = "Find Tests")
    )]
    pub fn coderlm_tests(&self, params: Parameters<TestsParams>) -> String {
        let p = &params.0;
        let limit = p.limit.unwrap_or(20);
        match symbol_ops::find_tests(
            &self.project().root,
            &self.project().file_tree,
            &self.project().symbol_table,
            &p.symbol,
            &p.file,
            limit,
            p.line,
        ) {
            Ok(tests) => serde_json::to_string_pretty(&serde_json::json!({
                "tests": tests,
                "count": tests.len(),
                "indexing_complete": self.project().is_indexing_complete(),
            }))
            .unwrap_or_else(|e| format!("Error: {}", e)),
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Returns a structured outline of a file, grouping symbols by kind
    /// (Functions, Structs, Methods, etc.) with signatures and line numbers.
    /// Use to quickly understand a file's shape without reading the full source.
    #[tool(
        name = "coderlm_outline",
        annotations(read_only_hint = true, title = "File Outline")
    )]
    pub fn coderlm_outline(&self, params: Parameters<OutlineParams>) -> String {
        let p = &params.0;
        match symbol_ops::generate_outline(
            &self.project().root,
            &self.project().file_tree,
            &self.project().symbol_table,
            &p.file,
        ) {
            Ok(outline) => serde_json::to_string_pretty(&serde_json::json!({
                "file": outline.file,
                "language": outline.language,
                "line_count": outline.line_count,
                "groups": outline.groups,
                "indexing_complete": self.project().is_indexing_complete(),
            }))
            .unwrap_or_else(|e| format!("Error: {}", e)),
            Err(e) => format!("Error: {}", e),
        }
    }

    /// List symbols in the index. Optionally filter by kind and/or file.
    /// Returns symbol names, files, lines, kinds, and signatures.
    #[tool(
        name = "coderlm_symbols",
        annotations(read_only_hint = true, title = "List Symbols")
    )]
    pub fn coderlm_symbols(&self, params: Parameters<SymbolsParams>) -> String {
        let p = &params.0;
        let kind_filter = p.kind.as_deref().and_then(SymbolKind::from_str);
        let limit = p.limit.unwrap_or(100);
        let results = symbol_ops::list_symbols(
            &self.project().symbol_table,
            kind_filter,
            p.file.as_deref(),
            limit,
        );
        serde_json::to_string_pretty(&serde_json::json!({
            "symbols": results,
            "count": results.len(),
            "indexing_complete": self.project().is_indexing_complete(),
        }))
        .unwrap_or_else(|e| format!("Error: {}", e))
    }

    /// Returns indexing stats: project root, file count, symbol count, and
    /// whether indexing is complete.
    #[tool(
        name = "coderlm_stats",
        annotations(read_only_hint = true, title = "Index Stats")
    )]
    pub fn coderlm_stats(&self, _params: Parameters<StatsParams>) -> String {
        let project = self.project();
        serde_json::to_string_pretty(&serde_json::json!({
            "project_root": project.root.display().to_string(),
            "file_count": project.file_tree.len(),
            "symbol_count": project.symbol_table.len(),
            "indexing_complete": project.is_indexing_complete(),
        }))
        .unwrap_or_else(|e| format!("Error: {}", e))
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
                "CodeRLM is a code-aware index server. Use the tools to explore \
                 the project structure, search for symbols, read implementations, \
                 find callers, and grep across the codebase. Start with \
                 coderlm_stats or coderlm_structure, then drill into specifics \
                 with coderlm_search, coderlm_impl, coderlm_peek, etc.",
            )
    }
}
