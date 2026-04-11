use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ops::{annotations, content, history, imports, structure, symbol_ops};
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

fn record_history(
    state: &AppState,
    session_id: Option<&str>,
    method: &str,
    path: &str,
    preview: &str,
) {
    if let Some(id) = session_id {
        if let Some(mut session) = state.inner.sessions.get_mut(id) {
            session.record(method, path, preview);
        }
    }
}

/// Record a symbol lookup (search, list, callers, tests, variables) for telemetry.
fn record_symbol_lookup(state: &AppState, session_id: Option<&str>) {
    if let Some(id) = session_id {
        if let Some(session) = state.inner.sessions.get(id) {
            session.stats.record_symbol_lookup();
        }
    }
}

fn split_scope_param(value: Option<&str>) -> Option<Vec<String>> {
    let parts: Vec<String> = value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Record a peek operation for telemetry with the chars served vs full file chars.
fn record_peek_stats(
    state: &AppState,
    session_id: Option<&str>,
    chars_served: u64,
    full_file_chars: u64,
) {
    if let Some(id) = session_id {
        if let Some(session) = state.inner.sessions.get(id) {
            session.stats.record_peek(chars_served, full_file_chars);
        }
    }
}

/// Record an impl operation for telemetry with the chars served vs full file chars.
fn record_impl_stats(
    state: &AppState,
    session_id: Option<&str>,
    chars_served: u64,
    full_file_chars: u64,
) {
    if let Some(id) = session_id {
        if let Some(session) = state.inner.sessions.get(id) {
            session.stats.record_impl(chars_served, full_file_chars);
        }
    }
}

/// Record a grep operation for telemetry.
fn record_grep_stats(state: &AppState, session_id: Option<&str>) {
    if let Some(id) = session_id {
        if let Some(session) = state.inner.sessions.get(id) {
            session.stats.record_grep();
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
        .route(
            "/api/v1/symbols/implementations/batch",
            post(batch_implementations),
        )
        .route("/api/v1/symbols/tests", get(find_tests))
        .route("/api/v1/symbols/callers", get(find_callers))
        .route("/api/v1/symbols/callers/batch", post(batch_callers))
        .route("/api/v1/symbols/variables", get(list_variables))
        .route("/api/v1/symbols/outline", get(symbols_outline))
        // Imports
        .route("/api/v1/imports", get(get_file_imports))
        .route("/api/v1/dependents", get(get_file_dependents))
        // Content
        .route("/api/v1/peek", get(peek))
        .route("/api/v1/grep", get(grep_handler))
        .route("/api/v1/chunk_indices", get(chunk_indices))
        // History
        .route("/api/v1/history", get(get_history))
        // Stats
        .route("/api/v1/stats", get(get_stats))
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "GET",
        "/structure",
        &preview,
    );
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/structure/define",
        &body.file,
    );
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/structure/redefine",
        &body.file,
    );
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/structure/mark",
        &body.file,
    );
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "GET",
        "/symbols/ready",
        &format!("ready={}, {} symbols", ready, symbol_count),
    );
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
    // Validate file exists in index when a file filter is provided
    if let Some(ref file) = params.file {
        if project.file_tree.get(file).is_none() {
            return Err(AppError::NotFound(format!(
                "File '{}' not found in index",
                file
            )));
        }
    }
    let kind_filter = match params.kind.as_deref() {
        Some(k) => {
            let parsed = SymbolKind::from_str(k);
            if parsed.is_none() {
                return Err(AppError::BadRequest(format!(
                    "Invalid symbol kind '{}'. Valid kinds: function, method, class, struct, \
                     enum, trait, interface, constant, variable, type, module, macro, import",
                    k
                )));
            }
            parsed
        }
        None => None,
    };
    let limit = params.limit.unwrap_or(100);
    let results = symbol_ops::list_symbols(
        &project.symbol_table,
        kind_filter,
        params.file.as_deref(),
        limit,
    );
    let sid = session_id(&headers);
    let preview = format!("{} symbols", results.len());
    record_history(&state, sid.as_deref(), "GET", "/symbols", &preview);
    record_symbol_lookup(&state, sid.as_deref());
    Ok(Json(
        json!({ "symbols": results, "count": results.len(), "indexing_complete": indexing_complete }),
    ))
}

#[derive(Deserialize)]
struct SymbolSearchQuery {
    q: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn search_symbols(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SymbolSearchQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(20);
    let result = symbol_ops::search_symbols(&project.symbol_table, &params.q, offset, limit);
    let sid = session_id(&headers);
    let preview = format!(
        "{} matches for '{}' (total {})",
        result.symbols.len(),
        params.q,
        result.total
    );
    record_history(&state, sid.as_deref(), "GET", "/symbols/search", &preview);
    record_symbol_lookup(&state, sid.as_deref());
    Ok(Json(json!({
        "symbols": result.symbols,
        "count": result.symbols.len(),
        "total": result.total,
        "offset": offset,
        "limit": limit,
        "indexing_complete": indexing_complete,
    })))
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/symbols/define",
        &body.symbol,
    );
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/symbols/redefine",
        &body.symbol,
    );
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
    let impl_result = symbol_ops::get_implementation(
        &project.root,
        &project.symbol_table,
        &params.symbol,
        &params.file,
        params.line,
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let sid = session_id(&headers);
    let preview = format!(
        "{}::{} ({} bytes)",
        params.file,
        params.symbol,
        impl_result.source.len()
    );
    record_history(
        &state,
        sid.as_deref(),
        "GET",
        "/symbols/implementation",
        &preview,
    );
    // Track token savings: chars served (impl snippet) vs full file
    let full_file_chars = project
        .file_tree
        .get(&params.file)
        .map(|e| e.size)
        .unwrap_or(impl_result.source.len() as u64);
    record_impl_stats(
        &state,
        sid.as_deref(),
        impl_result.source.len() as u64,
        full_file_chars,
    );
    let mut response = json!({
        "symbol": params.symbol,
        "file": params.file,
        "source": impl_result.source,
        "indexing_complete": indexing_complete,
    });
    if let Some(warning) = impl_result.warning {
        response["warning"] = json!(warning);
    }
    if let Some(candidates) = impl_result.candidates {
        response["candidates"] = json!(candidates);
    }
    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Batch implementations
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BatchImplementationRequest {
    symbols: Vec<BatchSymbolRef>,
}

#[derive(Deserialize)]
struct BatchSymbolRef {
    symbol: String,
    file: String,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn batch_implementations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BatchImplementationRequest>,
) -> Result<Json<Value>, AppError> {
    if body.symbols.is_empty() {
        return Err(AppError::BadRequest(
            "'symbols' array must not be empty".into(),
        ));
    }
    if body.symbols.len() > 50 {
        return Err(AppError::BadRequest(
            "Batch size exceeds maximum of 50 symbols".into(),
        ));
    }

    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();

    let mut results: Vec<Value> = Vec::with_capacity(body.symbols.len());
    let mut success_count = 0usize;
    let mut error_count = 0usize;
    let mut total_chars_served: u64 = 0;
    let mut total_chars_full_file: u64 = 0;

    for sym_ref in &body.symbols {
        match symbol_ops::get_implementation(
            &project.root,
            &project.symbol_table,
            &sym_ref.symbol,
            &sym_ref.file,
            sym_ref.line,
        ) {
            Ok(impl_result) => {
                success_count += 1;
                let full_file_chars = project
                    .file_tree
                    .get(&sym_ref.file)
                    .map(|e| e.size)
                    .unwrap_or(impl_result.source.len() as u64);
                total_chars_served += impl_result.source.len() as u64;
                total_chars_full_file += full_file_chars;
                let mut entry = json!({
                    "symbol": sym_ref.symbol,
                    "file": sym_ref.file,
                    "source": impl_result.source,
                });
                if let Some(warning) = impl_result.warning {
                    entry["warning"] = json!(warning);
                }
                if let Some(candidates) = impl_result.candidates {
                    entry["candidates"] = json!(candidates);
                }
                results.push(entry);
            }
            Err(err) => {
                error_count += 1;
                results.push(json!({
                    "symbol": sym_ref.symbol,
                    "file": sym_ref.file,
                    "error": err,
                }));
            }
        }
    }

    let sid = session_id(&headers);
    let preview = format!(
        "batch impl: {} requested, {} ok, {} errors",
        body.symbols.len(),
        success_count,
        error_count
    );
    record_history(
        &state,
        sid.as_deref(),
        "POST",
        "/symbols/implementations/batch",
        &preview,
    );
    // Track token savings: aggregate chars served vs full file for all successful impls
    if total_chars_served > 0 {
        if let Some(id) = sid.as_deref() {
            if let Some(session) = state.inner.sessions.get(id) {
                use std::sync::atomic::Ordering;
                session
                    .stats
                    .impl_reads
                    .fetch_add(success_count as u64, Ordering::Relaxed);
                session
                    .stats
                    .chars_served
                    .fetch_add(total_chars_served, Ordering::Relaxed);
                session
                    .stats
                    .chars_full_file
                    .fetch_add(total_chars_full_file, Ordering::Relaxed);
            }
        }
    }

    Ok(Json(json!({
        "results": results,
        "count": results.len(),
        "successes": success_count,
        "errors": error_count,
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
    let sid = session_id(&headers);
    let preview = format!("{} tests for {}", tests.len(), params.symbol);
    record_history(&state, sid.as_deref(), "GET", "/symbols/tests", &preview);
    record_symbol_lookup(&state, sid.as_deref());
    Ok(Json(
        json!({ "tests": tests, "count": tests.len(), "indexing_complete": indexing_complete }),
    ))
}

#[derive(Deserialize)]
struct CallersQuery {
    symbol: String,
    file: String,
    limit: Option<usize>,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
    /// Optional relative-path prefixes to include in caller search.
    #[serde(alias = "include_path")]
    include_paths: Option<String>,
    /// Optional relative-path prefixes to exclude from caller search.
    #[serde(alias = "exclude_path")]
    exclude_paths: Option<String>,
}

async fn find_callers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CallersQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    // Wait for initial indexing so we don't return false "not found" errors.
    project.wait_until_indexed().await;
    let indexing_complete = project.is_indexing_complete();
    let limit = params.limit.unwrap_or(50);
    let include_paths = split_scope_param(params.include_paths.as_deref());
    let exclude_paths = split_scope_param(params.exclude_paths.as_deref());
    let callers = symbol_ops::find_callers(
        &project.root,
        &project.file_tree,
        &project.symbol_table,
        &params.symbol,
        &params.file,
        limit,
        params.line,
        include_paths.as_deref(),
        exclude_paths.as_deref(),
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let sid = session_id(&headers);
    let preview = format!("{} callers of {}", callers.len(), params.symbol);
    record_history(&state, sid.as_deref(), "GET", "/symbols/callers", &preview);
    record_symbol_lookup(&state, sid.as_deref());
    Ok(Json(
        json!({ "callers": callers, "count": callers.len(), "indexing_complete": indexing_complete }),
    ))
}

// ---------------------------------------------------------------------------
// Batch callers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BatchCallersRequest {
    symbols: Vec<BatchCallersSymbolRef>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct BatchCallersSymbolRef {
    symbol: String,
    file: String,
    /// Optional line number to disambiguate same-named symbols in the same file.
    line: Option<usize>,
}

async fn batch_callers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BatchCallersRequest>,
) -> Result<Json<Value>, AppError> {
    if body.symbols.is_empty() {
        return Err(AppError::BadRequest(
            "'symbols' array must not be empty".into(),
        ));
    }
    if body.symbols.len() > 50 {
        return Err(AppError::BadRequest(
            "Batch size exceeds maximum of 50 symbols".into(),
        ));
    }

    let project = require_project(&state, &headers)?;
    // Wait for initial indexing so we don't return false "not found" errors.
    project.wait_until_indexed().await;
    let indexing_complete = project.is_indexing_complete();
    let limit = body.limit.unwrap_or(50);

    let mut results: Vec<Value> = Vec::with_capacity(body.symbols.len());
    let mut success_count = 0usize;
    let mut error_count = 0usize;

    for sym_ref in &body.symbols {
        match symbol_ops::find_callers(
            &project.root,
            &project.file_tree,
            &project.symbol_table,
            &sym_ref.symbol,
            &sym_ref.file,
            limit,
            sym_ref.line,
            None,
            None,
        ) {
            Ok(callers) => {
                success_count += 1;
                results.push(json!({
                    "symbol": sym_ref.symbol,
                    "file": sym_ref.file,
                    "callers": callers,
                    "count": callers.len(),
                }));
            }
            Err(err) => {
                error_count += 1;
                results.push(json!({
                    "symbol": sym_ref.symbol,
                    "file": sym_ref.file,
                    "error": err,
                }));
            }
        }
    }

    let sid = session_id(&headers);
    let preview = format!(
        "batch callers: {} requested, {} ok, {} errors",
        body.symbols.len(),
        success_count,
        error_count
    );
    record_history(
        &state,
        sid.as_deref(),
        "POST",
        "/symbols/callers/batch",
        &preview,
    );
    // Each successful caller lookup counts as a symbol lookup
    for _ in 0..success_count {
        record_symbol_lookup(&state, sid.as_deref());
    }

    Ok(Json(json!({
        "results": results,
        "count": results.len(),
        "successes": success_count,
        "errors": error_count,
        "indexing_complete": indexing_complete,
    })))
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
    let sid = session_id(&headers);
    let preview = format!("{} variables in {}", vars.len(), params.function);
    record_history(
        &state,
        sid.as_deref(),
        "GET",
        "/symbols/variables",
        &preview,
    );
    record_symbol_lookup(&state, sid.as_deref());
    Ok(Json(
        json!({ "variables": vars, "count": vars.len(), "indexing_complete": indexing_complete }),
    ))
}

// ---------------------------------------------------------------------------
// Outline (structural file summary)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OutlineQuery {
    file: String,
}

async fn symbols_outline(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<OutlineQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let indexing_complete = project.is_indexing_complete();
    let outline = symbol_ops::generate_outline(
        &project.root,
        &project.file_tree,
        &project.symbol_table,
        &params.file,
    )
    .map_err(|e| symbol_not_found_or_not_ready(e, indexing_complete))?;
    let sid = session_id(&headers);
    let preview = format!(
        "outline for {} ({} groups)",
        params.file,
        outline.groups.len()
    );
    record_history(&state, sid.as_deref(), "GET", "/symbols/outline", &preview);
    record_symbol_lookup(&state, sid.as_deref());
    Ok(Json(json!({
        "file": outline.file,
        "language": outline.language,
        "line_count": outline.line_count,
        "groups": outline.groups,
        "indexing_complete": indexing_complete,
    })))
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
    let result = content::peek(&project.root, &project.file_tree, &params.file, start, end)
        .map_err(AppError::NotFound)?;
    let sid = session_id(&headers);
    let preview = format!("{}:{}-{}", params.file, start, end);
    record_history(&state, sid.as_deref(), "GET", "/peek", &preview);
    // Track token savings: chars served (peek content) vs full file
    let full_file_chars = project
        .file_tree
        .get(&params.file)
        .map(|e| e.size)
        .unwrap_or(result.content.len() as u64);
    record_peek_stats(
        &state,
        sid.as_deref(),
        result.content.len() as u64,
        full_file_chars,
    );
    Ok(Json(serde_json::to_value(result).unwrap()))
}

#[derive(Deserialize)]
struct GrepQuery {
    pattern: String,
    #[serde(alias = "limit")]
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
        content::grep_with_scope(
            &root,
            &file_tree,
            &pattern,
            max_matches,
            context_lines,
            scope,
        )
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .map_err(AppError::BadRequest)?;

    let sid = session_id(&headers);
    let preview = format!("{} matches for '{}'", result.total_matches, params.pattern);
    record_history(&state, sid.as_deref(), "GET", "/grep", &preview);
    record_grep_stats(&state, sid.as_deref());
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
    let overlap = params.overlap.unwrap_or(0);
    let result = content::chunk_indices(
        &project.root,
        &project.file_tree,
        &params.file,
        size,
        overlap,
    )
    .map_err(AppError::BadRequest)?;
    let preview = format!("{} chunks for {}", result.chunks.len(), params.file);
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "GET",
        "/chunk_indices",
        &preview,
    );
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
            let entries = history::get_history(&state, &sid, limit).map_err(AppError::NotFound)?;
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
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/annotations/save",
        "saved",
    );
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
    let data =
        annotations::load_annotations(&project.root, &project.file_tree, &project.symbol_table)
            .map_err(AppError::Internal)?;
    let summary = json!({
        "file_definitions": data.file_definitions.len(),
        "file_marks": data.file_marks.len(),
        "symbol_definitions": data.symbol_definitions.len(),
    });
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "POST",
        "/annotations/load",
        "loaded",
    );
    Ok(Json(json!({ "ok": true, "loaded": summary })))
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImportsQuery {
    file: String,
}

async fn get_file_imports(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ImportsQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let result = imports::get_imports(
        &project.import_table,
        &params.file,
        Some(&project.file_tree),
    )
    .map_err(AppError::NotFound)?;
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "GET",
        "/imports",
        &format!("file={}", params.file),
    );
    Ok(Json(result))
}

#[derive(Deserialize)]
struct DependentsQuery {
    file: String,
}

async fn get_file_dependents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<DependentsQuery>,
) -> Result<Json<Value>, AppError> {
    let project = require_project(&state, &headers)?;
    let result = imports::get_dependents(&project.import_table, &params.file)
        .map_err(AppError::BadRequest)?;
    record_history(
        &state,
        session_id(&headers).as_deref(),
        "GET",
        "/dependents",
        &format!("file={}", params.file),
    );
    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Stats (token savings telemetry)
// ---------------------------------------------------------------------------

async fn get_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let sid = require_session(&headers)?;
    let session = state
        .inner
        .sessions
        .get(&sid)
        .ok_or_else(|| AppError::NotFound(format!("Session '{}' not found", sid)))?;
    let snapshot = session.stats.snapshot();
    Ok(Json(serde_json::to_value(snapshot).unwrap()))
}
