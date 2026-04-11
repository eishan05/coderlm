use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: DateTime<Utc>,
    pub method: String,
    pub path: String,
    pub response_preview: String,
}

/// Token-savings telemetry counters for a session.
///
/// Tracks how many operations were served and estimates how many characters
/// (and derived tokens at ~4 chars/token) were served vs what a full file
/// read would have cost.
pub struct SessionStats {
    /// Number of symbol lookup operations (search, list, callers, tests, variables).
    pub symbol_lookups: AtomicU64,
    /// Number of peek (partial file read) operations.
    pub peek_reads: AtomicU64,
    /// Number of implementation reads (targeted symbol source extraction).
    pub impl_reads: AtomicU64,
    /// Number of grep operations.
    pub grep_ops: AtomicU64,
    /// Characters actually served via impl/peek responses.
    pub chars_served: AtomicU64,
    /// Characters that a full file read would have cost for those same operations.
    /// For impl: the full file size. For peek: the full file size.
    pub chars_full_file: AtomicU64,
}

impl SessionStats {
    pub fn new() -> Self {
        Self {
            symbol_lookups: AtomicU64::new(0),
            peek_reads: AtomicU64::new(0),
            impl_reads: AtomicU64::new(0),
            grep_ops: AtomicU64::new(0),
            chars_served: AtomicU64::new(0),
            chars_full_file: AtomicU64::new(0),
        }
    }

    pub fn record_symbol_lookup(&self) {
        self.symbol_lookups.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_peek(&self, chars_served: u64, full_file_chars: u64) {
        self.peek_reads.fetch_add(1, Ordering::Relaxed);
        self.chars_served.fetch_add(chars_served, Ordering::Relaxed);
        self.chars_full_file
            .fetch_add(full_file_chars, Ordering::Relaxed);
    }

    pub fn record_impl(&self, chars_served: u64, full_file_chars: u64) {
        self.impl_reads.fetch_add(1, Ordering::Relaxed);
        self.chars_served.fetch_add(chars_served, Ordering::Relaxed);
        self.chars_full_file
            .fetch_add(full_file_chars, Ordering::Relaxed);
    }

    pub fn record_grep(&self) {
        self.grep_ops.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the current stats into a serializable struct.
    pub fn snapshot(&self) -> SessionStatsSnapshot {
        let symbol_lookups = self.symbol_lookups.load(Ordering::Relaxed);
        let peek_reads = self.peek_reads.load(Ordering::Relaxed);
        let impl_reads = self.impl_reads.load(Ordering::Relaxed);
        let grep_ops = self.grep_ops.load(Ordering::Relaxed);
        let chars_served = self.chars_served.load(Ordering::Relaxed);
        let chars_full_file = self.chars_full_file.load(Ordering::Relaxed);
        let chars_saved = chars_full_file.saturating_sub(chars_served);
        let tokens_served = chars_served / 4;
        let tokens_full_file = chars_full_file / 4;
        let tokens_saved = chars_saved / 4;

        SessionStatsSnapshot {
            symbol_lookups,
            peek_reads,
            impl_reads,
            grep_ops,
            chars_served,
            chars_full_file,
            chars_saved,
            estimated_tokens_served: tokens_served,
            estimated_tokens_full_file: tokens_full_file,
            estimated_tokens_saved: tokens_saved,
        }
    }
}

impl Default for SessionStats {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SessionStats {
    fn clone(&self) -> Self {
        Self {
            symbol_lookups: AtomicU64::new(self.symbol_lookups.load(Ordering::Relaxed)),
            peek_reads: AtomicU64::new(self.peek_reads.load(Ordering::Relaxed)),
            impl_reads: AtomicU64::new(self.impl_reads.load(Ordering::Relaxed)),
            grep_ops: AtomicU64::new(self.grep_ops.load(Ordering::Relaxed)),
            chars_served: AtomicU64::new(self.chars_served.load(Ordering::Relaxed)),
            chars_full_file: AtomicU64::new(self.chars_full_file.load(Ordering::Relaxed)),
        }
    }
}

impl std::fmt::Debug for SessionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStats")
            .field(
                "symbol_lookups",
                &self.symbol_lookups.load(Ordering::Relaxed),
            )
            .field("peek_reads", &self.peek_reads.load(Ordering::Relaxed))
            .field("impl_reads", &self.impl_reads.load(Ordering::Relaxed))
            .field("grep_ops", &self.grep_ops.load(Ordering::Relaxed))
            .field("chars_served", &self.chars_served.load(Ordering::Relaxed))
            .field(
                "chars_full_file",
                &self.chars_full_file.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// A point-in-time snapshot of session stats, suitable for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatsSnapshot {
    pub symbol_lookups: u64,
    pub peek_reads: u64,
    pub impl_reads: u64,
    pub grep_ops: u64,
    pub chars_served: u64,
    pub chars_full_file: u64,
    pub chars_saved: u64,
    pub estimated_tokens_served: u64,
    pub estimated_tokens_full_file: u64,
    pub estimated_tokens_saved: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub history: Vec<HistoryEntry>,
    #[serde(skip)]
    pub stats: SessionStats,
}

impl Session {
    pub fn new(id: String, project_path: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id,
            project_path,
            created_at: now,
            last_active: now,
            history: Vec::new(),
            stats: SessionStats::new(),
        }
    }

    pub fn record(&mut self, method: &str, path: &str, response_preview: &str) {
        self.last_active = Utc::now();
        self.history.push(HistoryEntry {
            timestamp: Utc::now(),
            method: method.to_string(),
            path: path.to_string(),
            response_preview: if response_preview.len() > 200 {
                let truncate_at = response_preview.floor_char_boundary(200);
                format!("{}...", &response_preview[..truncate_at])
            } else {
                response_preview.to_string()
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_new_all_zero() {
        let stats = SessionStats::new();
        let snap = stats.snapshot();
        assert_eq!(snap.symbol_lookups, 0);
        assert_eq!(snap.peek_reads, 0);
        assert_eq!(snap.impl_reads, 0);
        assert_eq!(snap.grep_ops, 0);
        assert_eq!(snap.chars_served, 0);
        assert_eq!(snap.chars_full_file, 0);
        assert_eq!(snap.chars_saved, 0);
        assert_eq!(snap.estimated_tokens_served, 0);
        assert_eq!(snap.estimated_tokens_full_file, 0);
        assert_eq!(snap.estimated_tokens_saved, 0);
    }

    #[test]
    fn test_stats_record_symbol_lookup() {
        let stats = SessionStats::new();
        stats.record_symbol_lookup();
        stats.record_symbol_lookup();
        stats.record_symbol_lookup();
        let snap = stats.snapshot();
        assert_eq!(snap.symbol_lookups, 3);
    }

    #[test]
    fn test_stats_record_peek() {
        let stats = SessionStats::new();
        stats.record_peek(200, 1000);
        let snap = stats.snapshot();
        assert_eq!(snap.peek_reads, 1);
        assert_eq!(snap.chars_served, 200);
        assert_eq!(snap.chars_full_file, 1000);
        assert_eq!(snap.chars_saved, 800);
    }

    #[test]
    fn test_stats_record_impl() {
        let stats = SessionStats::new();
        stats.record_impl(100, 5000);
        let snap = stats.snapshot();
        assert_eq!(snap.impl_reads, 1);
        assert_eq!(snap.chars_served, 100);
        assert_eq!(snap.chars_full_file, 5000);
        assert_eq!(snap.chars_saved, 4900);
    }

    #[test]
    fn test_stats_record_grep() {
        let stats = SessionStats::new();
        stats.record_grep();
        stats.record_grep();
        let snap = stats.snapshot();
        assert_eq!(snap.grep_ops, 2);
    }

    #[test]
    fn test_stats_token_estimation_4_chars_per_token() {
        let stats = SessionStats::new();
        stats.record_impl(400, 4000);
        let snap = stats.snapshot();
        assert_eq!(snap.estimated_tokens_served, 100);
        assert_eq!(snap.estimated_tokens_full_file, 1000);
        assert_eq!(snap.estimated_tokens_saved, 900);
    }

    #[test]
    fn test_stats_accumulate_across_operations() {
        let stats = SessionStats::new();
        stats.record_impl(100, 1000);
        stats.record_peek(200, 2000);
        let snap = stats.snapshot();
        assert_eq!(snap.impl_reads, 1);
        assert_eq!(snap.peek_reads, 1);
        assert_eq!(snap.chars_served, 300);
        assert_eq!(snap.chars_full_file, 3000);
        assert_eq!(snap.chars_saved, 2700);
    }

    #[test]
    fn test_stats_clone() {
        let stats = SessionStats::new();
        stats.record_symbol_lookup();
        stats.record_impl(100, 500);
        let cloned = stats.clone();
        let snap = cloned.snapshot();
        assert_eq!(snap.symbol_lookups, 1);
        assert_eq!(snap.impl_reads, 1);
        assert_eq!(snap.chars_served, 100);
    }

    #[test]
    fn test_stats_default() {
        let stats = SessionStats::default();
        let snap = stats.snapshot();
        assert_eq!(snap.symbol_lookups, 0);
        assert_eq!(snap.chars_served, 0);
    }

    #[test]
    fn test_stats_saved_saturates_at_zero() {
        let stats = SessionStats::new();
        // Edge case: chars_served > chars_full_file shouldn't happen in practice,
        // but if it does, saturating_sub prevents underflow.
        stats.record_impl(1000, 500);
        let snap = stats.snapshot();
        assert_eq!(snap.chars_saved, 0);
    }
}
