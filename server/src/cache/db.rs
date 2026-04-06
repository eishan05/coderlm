// SQLite database setup and schema management for persistent cache.

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Returns the platform-appropriate cache directory for coderlm.
/// macOS: ~/Library/Caches/coderlm/
/// Linux: ~/.cache/coderlm/
pub fn cache_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs_cache_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs_cache_linux()
    }
}

fn dirs_cache_macos() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Library").join("Caches").join("coderlm")
}

#[allow(dead_code)]
fn dirs_cache_linux() -> PathBuf {
    let cache_home = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{}/.cache", home)
    });
    PathBuf::from(cache_home).join("coderlm")
}

/// Open (or create) the SQLite cache database at the given path.
/// Creates parent directories if needed. Sets WAL mode for concurrent reads.
pub fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;

    // Enable WAL mode for concurrent reads during server operation
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

    // Create tables if they don't exist
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_index (
            content_hash TEXT NOT NULL,
            language TEXT NOT NULL,
            symbols_json TEXT NOT NULL,
            parser_version INTEGER NOT NULL,
            grammar_version TEXT NOT NULL,
            symbol_schema_version INTEGER NOT NULL,
            created_at TIMESTAMP NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (content_hash, language)
        );

        CREATE TABLE IF NOT EXISTS workspace_manifest (
            workspace_id TEXT NOT NULL,
            rel_path TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            mtime INTEGER NOT NULL,
            file_size INTEGER NOT NULL,
            last_seen_at TIMESTAMP NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (workspace_id, rel_path)
        );"
    )?;

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_db_creates_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let conn = open_db(&db_path).unwrap();

        // Verify file_index table exists by querying it
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_index", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify workspace_manifest table exists
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM workspace_manifest", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_open_db_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");

        // Open twice - second open should not fail or drop tables
        let conn1 = open_db(&db_path).unwrap();
        conn1.execute(
            "INSERT INTO file_index (content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params!["abc123", "rust", "[]", 1, "v1", 1],
        ).unwrap();
        drop(conn1);

        let conn2 = open_db(&db_path).unwrap();
        let count: i64 = conn2
            .query_row("SELECT COUNT(*) FROM file_index", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "Data should persist across re-opens");
    }

    #[test]
    fn test_open_db_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("deep").join("cache.db");
        let conn = open_db(&db_path).unwrap();

        // Should have created the nested directory structure
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_index", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_file_index_schema_has_correct_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let conn = open_db(&db_path).unwrap();

        // Insert a full row to verify all columns exist
        conn.execute(
            "INSERT INTO file_index (content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params!["deadbeef", "python", "[{\"name\":\"foo\"}]", 1, "py0.25", 1],
        ).unwrap();

        let (hash, lang, json, pv, gv, sv): (String, String, String, i64, String, i64) = conn
            .query_row(
                "SELECT content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version FROM file_index WHERE content_hash = ?1",
                ["deadbeef"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();

        assert_eq!(hash, "deadbeef");
        assert_eq!(lang, "python");
        assert_eq!(json, "[{\"name\":\"foo\"}]");
        assert_eq!(pv, 1);
        assert_eq!(gv, "py0.25");
        assert_eq!(sv, 1);
    }

    #[test]
    fn test_workspace_manifest_schema_has_correct_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let conn = open_db(&db_path).unwrap();

        conn.execute(
            "INSERT INTO workspace_manifest (workspace_id, rel_path, content_hash, mtime, file_size, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params!["/home/user/project", "src/main.rs", "abc123", 1700000000_i64, 1024_i64],
        ).unwrap();

        let (ws, rp, ch, mt, fs): (String, String, String, i64, i64) = conn
            .query_row(
                "SELECT workspace_id, rel_path, content_hash, mtime, file_size FROM workspace_manifest WHERE workspace_id = ?1 AND rel_path = ?2",
                rusqlite::params!["/home/user/project", "src/main.rs"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();

        assert_eq!(ws, "/home/user/project");
        assert_eq!(rp, "src/main.rs");
        assert_eq!(ch, "abc123");
        assert_eq!(mt, 1700000000);
        assert_eq!(fs, 1024);
    }

    #[test]
    fn test_workspace_manifest_composite_primary_key() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let conn = open_db(&db_path).unwrap();

        // Same rel_path in different workspaces should both succeed
        conn.execute(
            "INSERT INTO workspace_manifest (workspace_id, rel_path, content_hash, mtime, file_size, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params!["/project-a", "src/main.rs", "hash1", 100_i64, 50_i64],
        ).unwrap();

        conn.execute(
            "INSERT INTO workspace_manifest (workspace_id, rel_path, content_hash, mtime, file_size, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params!["/project-b", "src/main.rs", "hash2", 200_i64, 60_i64],
        ).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM workspace_manifest", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_workspace_manifest_duplicate_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let conn = open_db(&db_path).unwrap();

        conn.execute(
            "INSERT INTO workspace_manifest (workspace_id, rel_path, content_hash, mtime, file_size, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params!["/project", "src/main.rs", "hash1", 100_i64, 50_i64],
        ).unwrap();

        let result = conn.execute(
            "INSERT INTO workspace_manifest (workspace_id, rel_path, content_hash, mtime, file_size, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params!["/project", "src/main.rs", "hash2", 200_i64, 60_i64],
        );
        assert!(result.is_err(), "Duplicate workspace_id + rel_path should fail");
    }

    #[test]
    fn test_cache_dir_returns_platform_appropriate_path() {
        let path = cache_dir();
        // On macOS: ~/Library/Caches/coderlm/
        // On Linux: ~/.cache/coderlm/
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("coderlm"),
            "Cache dir should contain 'coderlm', got: {}",
            path_str
        );
    }

    #[test]
    fn test_wal_mode_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let conn = open_db(&db_path).unwrap();

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "WAL mode should be enabled for concurrent reads");
    }
}
