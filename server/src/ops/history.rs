use crate::server::session::HistoryEntry;
use crate::server::state::AppState;
use serde::Serialize;

pub fn get_history(
    state: &AppState,
    session_id: &str,
    limit: usize,
) -> Result<Vec<HistoryEntry>, String> {
    let session = state
        .inner
        .sessions
        .get(session_id)
        .ok_or_else(|| format!("Session '{}' not found", session_id))?;

    let history = &session.history;
    let start = history.len().saturating_sub(limit);
    Ok(history[start..].to_vec())
}

#[derive(Debug, Serialize)]
pub struct SessionHistoryBlock {
    pub session_id: String,
    pub project: String,
    pub entries: Vec<HistoryEntry>,
}

/// Return history from all active sessions, ordered by timestamp.
pub fn get_all_history(state: &AppState, limit: usize) -> Vec<SessionHistoryBlock> {
    let mut blocks: Vec<SessionHistoryBlock> = state
        .inner
        .sessions
        .iter()
        .map(|entry| {
            let session = entry.value();
            let history = &session.history;
            let start = history.len().saturating_sub(limit);
            SessionHistoryBlock {
                session_id: session.id.clone(),
                project: session.project_path.display().to_string(),
                entries: history[start..].to_vec(),
            }
        })
        .collect();

    // Sort blocks by most recent activity (latest entry timestamp descending)
    blocks.sort_by(|a, b| {
        let a_latest = a.entries.last().map(|e| e.timestamp);
        let b_latest = b.entries.last().map(|e| e.timestamp);
        b_latest.cmp(&a_latest)
    });

    blocks
}
