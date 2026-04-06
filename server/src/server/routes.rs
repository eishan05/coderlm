use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ops::{annotations, content, history, structure, symbol_ops};
use crate::server::errors::AppError;
use crate::server::session::Session;
use crate::server::state::{AppState, Project};
use crate::symbols::symbol::SymbolKind;

// ---------------------------------------------------------------------------
// Helper: extract session ID from headers
// ---------------------------------------------------------------------------

fn session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn require_session(headers: &HeaderMap) -> Result<String, AppError> {
    session_id(headers).ok_or_else(|| AppError::BadRequest("Missing X-Session-Id header".into()))
}

/// Resolve session -> project. Touches last_active on both session and project.
fn require_project(state: &AppState, headers: &HeaderMap) -> Result<Arc<Project>, AppError> {
    let sid = require_session(headers)?;
    let project = state.get_project_for_session(&sid)?;
    state.touch_project(&project.root);
    // Update session last_active
    if let Some(mut session) = state.inner.sessions.get_mut(&sid) {
        session.last_active = chrono::Utc::now();
    }
    Ok(project)
}

/// Convert a symbol operation error into an AppError. When indexing hasn't
/// completed and the error looks like a lookup miss ("not found"), the message
/// is enriched so the client can distinguish "symbol missing" from "not yet
/// indexed". Non-lookup errors (e.g., file-read failures) are passed through
/// as-is to avoid mislabeling.
fn symbol_not_found_or_not_ready(err: String, indexing_complete: bool) -> AppError {
    let is_lookup_miss = err.contains("not found") || err.contains("ambiguous");
    if is_lookup_miss && !indexing_complete {
        AppError::BadRequest(format!(
            "{}. NOTE: Symbol indexing is still in progress (indexing_complete=false). \
             The symbol may not have been extracted yet. \
             Call GET /api/v1/symbols/ready?wait=true to wait for indexing to finish, \
             then retry.",
            err
        ))
    } else if is_lookup_miss {
        AppError::NotFound(err)
    } else {
        // Non-lookup errors (IO, parse failures, etc.) stay as internal errors
        AppError::Internal(err)
    }
}

fn record_history(state: &AppState, session_id: Option<&str>, method: &str, path: &str, preview: &str) {
    if let Some(id) = session_id {
        if let Some(mut session) = state.inner.sessions.get_mut(id) {
            session.record(method, path, preview);
        }
    }
}

// ---------------------------------------------------------------------------
// Router construction
// ---------------------------------------------------------------------------

pub fn build_routes(state: AppState) -> Router {
    Router::new()
        // Health
        .route("/api/v1/health", get(health))
        // Admin
        .route("/api/v1/roots", get(list_roots))
        // Sessions
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/{id}", get(get_session))
        .route("/api/v1/sessions/{id}", delete(delete_session))
        // Structure
        .route("/api/v1/structure", get(get_structure))
        .route("/api/v1/structure/define", post(define_file))
        .route("/api/v1/structure/redefine", post(redefine_file))
        .route("/api/v1/structure/mark", post(mark_file))
        // Symbols
        .route("/api/v1/symbols/ready", get(symbols_ready))
        .route("/api/v1/symbols", get(list_symbols))
        .route("/api/v1/symbols/search", get(search_symbols))
        .route("/api/v1/symbols/define", post(define_symbol))
        .route("/api/v1/symbols/redefine", post(redefine_symbol))
        .route("/api/v1/symbols/implementation", get(get_implementation))
        .route("/api/v1/symbols/tests", get(find_tests))
        .route("/api/v1/symbols/callers", get(find_callers))
        .route("/api/v1/symbols/variables", get(list_variables))
        // Content
        .route("/api/v1/peek", get(peek))
        .route("/api/v1/grep", get(grep_handler))
        .route("/api/v1/chunk_indices", get(chunk_indices))
        // History
        .route("/api/v1/history", get(get_history))
        // Annotations
        .route("/api/v1/annotations/save", post(save_annotations))
        .route("/api/v1/annotations/load", post(load_annotations))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Json<Value> {
    let project_count = state.inner.projects.len();
    let session_count = state.inner.sessions.len();

    Json(json!({
        "status": "ok",
        "projects": project_count,
        "active_sessions": session_count,
        "max_projects": state.inner.max_projects,
    }))
}

// ---------------------------------------------------------------------------
// Admin: list registered projects
// ---------------------------------------------------------------------------

async fn list_roots(State(state): State<AppState>) -> Json<Value> {
    let roots: Vec<Value> = state
        .inner
        .projects
        .iter()
        .map(|entry| {
            let project = entry.value();
            let session_count = state
                .inner
                .sessions
                .iter()
                .filter(|s| s.value().project_path == *entry.key())
                .count();
            json!({
                "path": project.root.display().to_string(),
                "file_count": project.file_tree.len(),
                "symbol_count": project.symbol_table.len(),
                "last_active": (*project.last_active.lock()).to_rfc3339(),
                "session_count": session_count,
                "indexing_complete": project.is_indexing_complete(),
            })
        })
        .collect();

    Json(json!({ "roots": roots, "count": roots.len() }))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSessionBody {
    cwd: String,
}

async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<Value>, AppError> {
    let cwd_path = PathBuf::from(&body.cwd);

    // Index the project (or return existing)
    let project = state.get_or_create_project(&cwd_path)?;

    let id = uuid::Uuid::new_v4().to_string();
    let session = Session::new(id.clone(), project.root.clone());
    let created_at = session.created_at;
    state.inner.sessions.insert(id.clone(), session);

    // For newly-created projects, annotations are loaded automatically after
    // symbol extraction completes (see state.rs). For already-resident projects
    // (indexing already done), reload annotations now to pick up any on-disk changes.
    if project.is_indexing_complete() {
        let ft = project.file_tree.clone();
        let st = project.symbol_table.clone();
        let root = project.root.clone();
        if let Err(e) = annotations::load_annotations(&root, &ft, &st) {
            tracing::warn!("Failed to reload annotations for {}: {}", root.display(), e);
        }
    }

    Ok(Json(json!({
        "session_id": id,
        "created_at": created_at.to_rfc3339(),
        "project": project.root.display().to_string(),
        "indexing_complete": project.is_indexing_complete(),
    })))
}

#[derive(Deserialize)]
struct SessionPath {
    id: String,
}

async fn get_session(
    State(state): State<AppState>,
    axum::extract::Path(params): axum::extract::Path<SessionPath>,
) -> Result<Json<Value>, AppError> {
    let session = state
        .inner
        .sessions
        .get(&params.id)
        .ok_or_else(|| AppError::NotFound(format!("Session '{}' not found", params.id)))?;

    // Check indexing status for this session's project
    let indexing_complete = state
        .inner
        .projects
        .get(&session.project_path)
        .map(|p| p.is_indexing_complete())
        .unwrap_or(false);

    Ok(Json(json!({
        "session_id": session.id,
        "project": session.project_path.display().to_string(),
        "created_at": session.created_at.to_rfc3339(),
        "last_active": session.last_active.to_rfc3339(),
        "history_count": session.history.len(),
        "indexing_complete": indexing_complete,
    })))
}

async fn delete_session(
    State(state): State<AppState>,
    axum::extract::Path(params): axum::extract::Path<SessionPath>,
) -> Result<Json<Value>, AppError> {
    state
        .inner
        .sessions
        .remove(&params.id)
        .ok_or_else(|| AppError::NotFound(format!("Session '{}' not found", params.id)))?;

    Ok(Json(json!({ "deleted": true })))
}

async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let mut sessions: Vec<Value> = state
        .inner
        .sessions
        .iter()
        .map(|entry| {
            let session = entry.value();
            json!({
                "session_id": session.id,
                "project": session.project_path.display().to_string(),
                "created_at": session.created_at.to_rfc3339(),
                "last_active": session.last_active.to_rfc3339(),
                "history_count": session.history.len(),
            })
        })
        .collect();

    sessions.sort_by(|a, b| {
        let a_time = a["last_active"].as_str().unwrap_or("");
        let b_time = b["last_active"].as_str().unwrap_or("");
        b_time.cmp(a_time)
    });

    Json(json!({ "sessions": sessions, "count": sessions.len() }))
}

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StructureQuery {
    depth: Option<usize>,
}

async fn get_structure(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<StructureQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let depth = params.depth.unwrap_or(0);
    let result = structure::get_structure(&project.file_tree, depth);
    let preview = format!("{} files", result.file_count);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/structure", &preview);
    Ok(Json(serde_json::to_value(result).unwrap()))
}

#[derive(Deserialize)]
struct DefineRequest {
    file: String,
    definition: String,
}

async fn define_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DefineRequest>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    structure::define_file(&project.file_tree, &body.file, &body.definition)
        .map_err(AppError::BadRequest)?;
    record_history(&state, session_id(&headers).as_deref(), "POST", "/structure/define", &body.file);
    Ok(Json(json!({ "ok": true })))
}

async fn redefine_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DefineRequest>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    structure::redefine_file(&project.file_tree, &body.file, &body.definition)
        .map_err(AppError::BadRequest)?;
    record_history(&state, session_id(&headers).as_deref(), "POST", "/structure/redefine", &body.file);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct MarkRequest {
    file: String,
    mark: String,
}

async fn mark_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MarkRequest>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    structure::mark_file(&project.file_tree, &body.file, &body.mark)
        .map_err(AppError::BadRequest)?;
    record_history(&state, session_id(&headers).as_deref(), "POST", "/structure/mark", &body.file);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SymbolsReadyQuery {
    /// If true, block until indexing completes instead of returning immediately.
    wait: Option<bool>,
}

/// Check (or wait for) symbol extraction readiness.
///
/// `GET /api/v1/symbols/ready` — returns `{"ready": bool, "symbol_count": N}`
/// `GET /api/v1/symbols/ready?wait=true` — blocks until extraction completes
async fn symbols_ready(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SymbolsReadyQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;

    if params.wait.unwrap_or(false) {
        project.wait_until_indexed().await;
    }

    let ready = project.is_indexing_complete();
    let symbol_count = project.symbol_table.len();
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols/ready",
        &format!("ready={}, {} symbols", ready, symbol_count));
    Ok(Json(json!({
        "ready": ready,
        "symbol_count": symbol_count,
    })))
}

#[derive(Deserialize)]
struct SymbolListQuery {
    kind: Option<String>,
    file: Option<String>,
    limit: Option<usize>,
}

async fn list_symbols(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SymbolListQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let kind_filter = params.kind.as_deref().and_then(SymbolKind::from_str);
    let limit = params.limit.unwrap_or(100);
    let results = symbol_ops::list_symbols(
        &project.symbol_table,
        kind_filter,
        params.file.as_deref(),
        limit,
    );
    let preview = format!("{} symbols", results.len());
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols", &preview);
    Ok(Json(json!({ "symbols": results, "count": results.len(), "indexing_complete": indexing_complete })))
}

#[derive(Deserialize)]
struct SymbolSearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn search_symbols(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SymbolSearchQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let limit = params.limit.unwrap_or(20);
    let results = symbol_ops::search_symbols(&project.symbol_table, &params.q, limit);
    let preview = format!("{} matches for '{}'", results.len(), params.q);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols/search", &preview);
    Ok(Json(json!({ "symbols": results, "count": results.len(), "indexing_complete": indexing_complete })))
}

#[derive(Deserialize)]
struct SymbolDefineRequest {
    symbol: String,
    file: String,
    definition: String,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn define_symbol(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SymbolDefineRequest>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    symbol_ops::define_symbol(
        &project.symbol_table,
        &body.symbol,
        &body.file,
        &body.definition,
        body.line,
    )
    .map_err(AppError::BadRequest)?;
    record_history(&state, session_id(&headers).as_deref(), "POST", "/symbols/define", &body.symbol);
    Ok(Json(json!({ "ok": true })))
}

async fn redefine_symbol(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SymbolDefineRequest>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    symbol_ops::redefine_symbol(
        &project.symbol_table,
        &body.symbol,
        &body.file,
        &body.definition,
        body.line,
    )
    .map_err(AppError::BadRequest)?;
    record_history(&state, session_id(&headers).as_deref(), "POST", "/symbols/redefine", &body.symbol);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ImplementationQuery {
    symbol: String,
    file: String,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn get_implementation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ImplementationQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let source = symbol_ops::get_implementation(
        &project.root,
        &project.symbol_table,
        &params.symbol,
        &params.file,
        params.line,
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let preview = format!("{}::{} ({} bytes)", params.file, params.symbol, source.len());
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols/implementation", &preview);
    Ok(Json(json!({
        "symbol": params.symbol,
        "file": params.file,
        "source": source,
        "indexing_complete": indexing_complete,
    })))
}

#[derive(Deserialize)]
struct TestsQuery {
    symbol: String,
    file: String,
    limit: Option<usize>,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn find_tests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<TestsQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let limit = params.limit.unwrap_or(20);
    let tests = symbol_ops::find_tests(
        &project.root,
        &project.file_tree,
        &project.symbol_table,
        &params.symbol,
        &params.file,
        limit,
        params.line,
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let preview = format!("{} tests for {}", tests.len(), params.symbol);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols/tests", &preview);
    Ok(Json(json!({ "tests": tests, "count": tests.len(), "indexing_complete": indexing_complete })))
}

#[derive(Deserialize)]
struct CallersQuery {
    symbol: String,
    file: String,
    limit: Option<usize>,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn find_callers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CallersQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let limit = params.limit.unwrap_or(50);
    let callers = symbol_ops::find_callers(
        &project.root,
        &project.file_tree,
        &project.symbol_table,
        &params.symbol,
        &params.file,
        limit,
        params.line,
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let preview = format!("{} callers of {}", callers.len(), params.symbol);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols/callers", &preview);
    Ok(Json(json!({ "callers": callers, "count": callers.len(), "indexing_complete": indexing_complete })))
}

#[derive(Deserialize)]
struct VariablesQuery {
    function: String,
    file: String,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn list_variables(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<VariablesQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let vars = symbol_ops::list_variables(
        &project.root,
        &project.symbol_table,
        &params.function,
        &params.file,
        params.line,
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let preview = format!("{} variables in {}", vars.len(), params.function);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/symbols/variables", &preview);
    Ok(Json(json!({ "variables": vars, "count": vars.len(), "indexing_complete": indexing_complete })))
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PeekQuery {
    file: String,
    start: Option<usize>,
    end: Option<usize>,
}

async fn peek(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<PeekQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let start = params.start.unwrap_or(0);
    let end = params.end.unwrap_or(100);
    let result = content::peek(
        &project.root,
        &project.file_tree,
        &params.file,
        start,
        end,
    )
    .map_err(AppError::NotFound)?;
    let preview = format!("{}:{}-{}", params.file, start, end);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/peek", &preview);
    Ok(Json(serde_json::to_value(result).unwrap()))
}

#[derive(Deserialize)]
struct GrepQuery {
    pattern: String,
    max_matches: Option<usize>,
    context_lines: Option<usize>,
    /// Optional scope filter: "all" (default) or "code" (skip comments/strings).
    scope: Option<String>,
}

async fn grep_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<GrepQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let max_matches = params.max_matches.unwrap_or(50);
    let context_lines = params.context_lines.unwrap_or(2);
    let scope = params
        .scope
        .as_deref()
        .map(|s| content::GrepScope::from_str(s))
        .flatten()
        .unwrap_or(content::GrepScope::All);

    // Run grep on a blocking thread since it reads many files
    let root = project.root.clone();
    let file_tree = project.file_tree.clone();
    let pattern = params.pattern.clone();

    let result = tokio::task::spawn_blocking(move || {
        content::grep_with_scope(&root, &file_tree, &pattern, max_matches, context_lines, scope)
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .map_err(AppError::BadRequest)?;

    let preview = format!("{} matches for '{}'", result.total_matches, params.pattern);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/grep", &preview);
    Ok(Json(serde_json::to_value(result).unwrap()))
}

#[derive(Deserialize)]
struct ChunkQuery {
    file: String,
    size: Option<usize>,
    overlap: Option<usize>,
}

async fn chunk_indices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ChunkQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let size = params.size.unwrap_or(5000);
    let overlap = params.overlap.unwrap_or(200);
    let result = content::chunk_indices(
        &project.root,
        &project.file_tree,
        &params.file,
        size,
        overlap,
    )
    .map_err(AppError::BadRequest)?;
    let preview = format!("{} chunks for {}", result.chunks.len(), params.file);
    record_history(&state, session_id(&headers).as_deref(), "GET", "/chunk_indices", &preview);
    Ok(Json(serde_json::to_value(result).unwrap()))
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

async fn get_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HistoryQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(50);

    // If no session header, return history from all active sessions (admin view)
    match session_id(&headers) {
        Some(sid) => {
            let _project = state.get_project_for_session(&sid)?;
            let entries =
                history::get_history(&state, &sid, limit).map_err(AppError::NotFound)?;
            Ok(Json(json!({ "history": entries, "count": entries.len() })))
        }
        None => {
            let blocks = history::get_all_history(&state, limit);
            let total: usize = blocks.iter().map(|b| b.entries.len()).sum();
            Ok(Json(json!({ "sessions": blocks, "total_entries": total })))
        }
    }
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

async fn save_annotations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    annotations::save_annotations(&project.root, &project.file_tree, &project.symbol_table)
        .map_err(AppError::Internal)?;
    record_history(&state, session_id(&headers).as_deref(), "POST", "/annotations/save", "saved");
    Ok(Json(json!({ "ok": true })))
}

async fn load_annotations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    if !indexing_complete {
        return Err(AppError::BadRequest(
            "Symbol extraction is still in progress. Wait for indexing to complete \
             before loading annotations, or call GET /api/v1/symbols/ready?wait=true first. \
             Loading annotations before symbols are ready will silently drop symbol annotations."
                .into(),
        ));
    }
    let data = annotations::load_annotations(
        &project.root,
        &project.file_tree,
        &project.symbol_table,
    )
    .map_err(AppError::Internal)?;
    let summary = json!({
        "file_definitions": data.file_definitions.len(),
        "file_marks": data.file_marks.len(),
        "symbol_definitions": data.symbol_definitions.len(),
    });
    record_history(&state, session_id(&headers).as_deref(), "POST", "/annotations/load", "loaded");
    Ok(Json(json!({ "ok": true, "loaded": summary })))
}
