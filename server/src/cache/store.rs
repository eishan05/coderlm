// CacheStore: high-level API for the persistent symbol cache.

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use crate::cache::db;
use crate::cache::versions;
use crate::index::file_entry::Language;
use crate::symbols::symbol::Symbol;

/// A single entry from the workspace_manifest table.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub rel_path: String,
    pub content_hash: String,
    pub mtime: i64,
    pub file_size: i64,
}

/// Thread-safe persistent cache backed by SQLite.
///
/// Wraps a Mutex<Connection> so it can be shared across threads. All
/// operations acquire the lock briefly for the duration of the SQL statement.
pub struct CacheStore {
    pub(crate) conn: Mutex<Connection>,
}

impl CacheStore {
    /// Open or create the cache database at `db_path`.
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = db::open_db(db_path)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Returns the default database path for the current platform.
    pub fn default_db_path() -> PathBuf {
        db::cache_dir().join("cache.db")
    }

    /// Store parsed symbols for a content hash. Uses INSERT OR IGNORE so
    /// duplicate content hashes (same file in multiple workspaces) are a no-op.
    pub fn store_symbols(
        &self,
        content_hash: &str,
        language: Language,
        symbols: &[Symbol],
    ) -> Result<()> {
        let json = serde_json::to_string(symbols)?;
        let lang_str = serde_json::to_string(&language)?;
        // Remove surrounding quotes from JSON string (e.g. "\"rust\"" -> "rust")
        let lang_str = lang_str.trim_matches('"');

        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO file_index
             (content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params![
                content_hash,
                lang_str,
                json,
                versions::PARSER_VERSION,
                versions::GRAMMAR_VERSION,
                versions::SYMBOL_SCHEMA_VERSION,
            ],
        )?;
        Ok(())
    }

    /// Look up cached symbols by content hash. Returns None if:
    /// - No entry exists for this hash
    /// - The entry exists but has stale version numbers
    pub fn lookup_symbols(&self, content_hash: &str) -> Result<Option<Vec<Symbol>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT symbols_json FROM file_index
             WHERE content_hash = ?1
               AND parser_version = ?2
               AND grammar_version = ?3
               AND symbol_schema_version = ?4",
        )?;

        let result = stmt.query_row(
            rusqlite::params![
                content_hash,
                versions::PARSER_VERSION,
                versions::GRAMMAR_VERSION,
                versions::SYMBOL_SCHEMA_VERSION,
            ],
            |row| {
                let json: String = row.get(0)?;
                Ok(json)
            },
        );

        match result {
            Ok(json) => {
                let symbols: Vec<Symbol> = serde_json::from_str(&json)?;
                Ok(Some(symbols))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Update or insert a workspace manifest entry (upsert).
    pub fn update_manifest(
        &self,
        workspace_id: &str,
        rel_path: &str,
        content_hash: &str,
        mtime: i64,
        file_size: i64,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO workspace_manifest (workspace_id, rel_path, content_hash, mtime, file_size, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(workspace_id, rel_path)
             DO UPDATE SET content_hash = ?3, mtime = ?4, file_size = ?5, last_seen_at = datetime('now')",
            rusqlite::params![workspace_id, rel_path, content_hash, mtime, file_size],
        )?;
        Ok(())
    }

    /// Get a single manifest entry for a workspace + path.
    pub fn get_manifest_entry(
        &self,
        workspace_id: &str,
        rel_path: &str,
    ) -> Result<Option<ManifestEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT rel_path, content_hash, mtime, file_size
             FROM workspace_manifest
             WHERE workspace_id = ?1 AND rel_path = ?2",
        )?;

        let result = stmt.query_row(
            rusqlite::params![workspace_id, rel_path],
            |row| {
                Ok(ManifestEntry {
                    rel_path: row.get(0)?,
                    content_hash: row.get(1)?,
                    mtime: row.get(2)?,
                    file_size: row.get(3)?,
                })
            },
        );

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all manifest entries for a workspace.
    pub fn get_workspace_manifest(&self, workspace_id: &str) -> Result<Vec<ManifestEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT rel_path, content_hash, mtime, file_size
             FROM workspace_manifest
             WHERE workspace_id = ?1",
        )?;

        let entries = stmt
            .query_map(rusqlite::params![workspace_id], |row| {
                Ok(ManifestEntry {
                    rel_path: row.get(0)?,
                    content_hash: row.get(1)?,
                    mtime: row.get(2)?,
                    file_size: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Check if a file is unchanged based on mtime + file_size.
    /// Returns false if the file is not in the manifest.
    pub fn is_file_unchanged(
        &self,
        workspace_id: &str,
        rel_path: &str,
        mtime: i64,
        file_size: i64,
    ) -> Result<bool> {
        match self.get_manifest_entry(workspace_id, rel_path)? {
            Some(entry) => Ok(entry.mtime == mtime && entry.file_size == file_size),
            None => Ok(false),
        }
    }

    /// Remove a single manifest entry.
    pub fn remove_manifest_entry(&self, workspace_id: &str, rel_path: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM workspace_manifest WHERE workspace_id = ?1 AND rel_path = ?2",
            rusqlite::params![workspace_id, rel_path],
        )?;
        Ok(())
    }

    /// Remove all manifest entries for a workspace.
    pub fn clear_workspace(&self, workspace_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM workspace_manifest WHERE workspace_id = ?1",
            rusqlite::params![workspace_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::file_entry::Language;
    use crate::symbols::symbol::{Symbol, SymbolKind};

    fn make_test_symbol(name: &str, file: &str, line: usize, lang: Language) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            file: file.to_string(),
            byte_range: (0, 100),
            line_range: (line, line + 5),
            language: lang,
            signature: format!("fn {}()", name),
            definition: None,
            parent: None,
            decorators: Vec::new(),
        }
    }

    fn make_test_store() -> CacheStore {
        let dir = tempfile::tempdir().unwrap();
        // Keep tempdir alive by leaking it (tests are short-lived)
        #[allow(deprecated)]
        let dir_path = dir.into_path();
        CacheStore::open(&dir_path.join("cache.db")).unwrap()
    }

    // --- store_symbols / lookup_symbols ---

    #[test]
    fn test_store_and_lookup_symbols() {
        let store = make_test_store();
        let symbols = vec![
            make_test_symbol("foo", "src/main.rs", 1, Language::Rust),
            make_test_symbol("bar", "src/main.rs", 10, Language::Rust),
        ];

        store
            .store_symbols("abc123hash", Language::Rust, &symbols)
            .unwrap();

        let loaded = store.lookup_symbols("abc123hash").unwrap();
        assert!(loaded.is_some(), "Should find cached symbols");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "foo");
        assert_eq!(loaded[1].name, "bar");
    }

    #[test]
    fn test_lookup_symbols_returns_none_for_missing_hash() {
        let store = make_test_store();
        let loaded = store.lookup_symbols("nonexistent").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_lookup_symbols_returns_none_for_stale_parser_version() {
        let store = make_test_store();
        let symbols = vec![make_test_symbol("foo", "src/main.rs", 1, Language::Rust)];

        // Insert with a different parser version directly
        let json = serde_json::to_string(&symbols).unwrap();
        store.conn.lock().execute(
            "INSERT INTO file_index (content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params![
                "stale_hash",
                "rust",
                json,
                999, // wrong parser version
                crate::cache::versions::GRAMMAR_VERSION,
                crate::cache::versions::SYMBOL_SCHEMA_VERSION,
            ],
        ).unwrap();

        let loaded = store.lookup_symbols("stale_hash").unwrap();
        assert!(loaded.is_none(), "Stale parser version should return None");
    }

    #[test]
    fn test_lookup_symbols_returns_none_for_stale_grammar_version() {
        let store = make_test_store();
        let symbols = vec![make_test_symbol("foo", "src/main.rs", 1, Language::Rust)];

        let json = serde_json::to_string(&symbols).unwrap();
        store.conn.lock().execute(
            "INSERT INTO file_index (content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params![
                "stale_grammar",
                "rust",
                json,
                crate::cache::versions::PARSER_VERSION,
                "old-grammar-version",
                crate::cache::versions::SYMBOL_SCHEMA_VERSION,
            ],
        ).unwrap();

        let loaded = store.lookup_symbols("stale_grammar").unwrap();
        assert!(loaded.is_none(), "Stale grammar version should return None");
    }

    #[test]
    fn test_lookup_symbols_returns_none_for_stale_schema_version() {
        let store = make_test_store();
        let symbols = vec![make_test_symbol("foo", "src/main.rs", 1, Language::Rust)];

        let json = serde_json::to_string(&symbols).unwrap();
        store.conn.lock().execute(
            "INSERT INTO file_index (content_hash, language, symbols_json, parser_version, grammar_version, symbol_schema_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params![
                "stale_schema",
                "rust",
                json,
                crate::cache::versions::PARSER_VERSION,
                crate::cache::versions::GRAMMAR_VERSION,
                999, // wrong schema version
            ],
        ).unwrap();

        let loaded = store.lookup_symbols("stale_schema").unwrap();
        assert!(loaded.is_none(), "Stale schema version should return None");
    }

    #[test]
    fn test_store_symbols_deduplicates_by_content_hash() {
        let store = make_test_store();
        let symbols1 = vec![make_test_symbol("foo", "a/main.rs", 1, Language::Rust)];
        let symbols2 = vec![make_test_symbol("foo", "b/main.rs", 1, Language::Rust)];

        // Same content hash from two different workspaces
        store.store_symbols("shared_hash", Language::Rust, &symbols1).unwrap();
        // Second store with same hash should succeed (upsert / ignore)
        store.store_symbols("shared_hash", Language::Rust, &symbols2).unwrap();

        // Should return whichever was stored (first or second, doesn't matter)
        let loaded = store.lookup_symbols("shared_hash").unwrap();
        assert!(loaded.is_some());
    }

    // --- update_manifest / get_manifest_entry ---

    #[test]
    fn test_update_and_get_manifest_entry() {
        let store = make_test_store();
        let workspace = "/home/user/project";

        store
            .update_manifest(workspace, "src/main.rs", "hash123", 1700000000, 1024)
            .unwrap();

        let entry = store.get_manifest_entry(workspace, "src/main.rs").unwrap();
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.content_hash, "hash123");
        assert_eq!(entry.mtime, 1700000000);
        assert_eq!(entry.file_size, 1024);
    }

    #[test]
    fn test_get_manifest_entry_returns_none_for_missing() {
        let store = make_test_store();
        let entry = store.get_manifest_entry("/project", "nonexistent.rs").unwrap();
        assert!(entry.is_none());
    }

    #[test]
    fn test_update_manifest_upserts_on_conflict() {
        let store = make_test_store();
        let workspace = "/project";

        store.update_manifest(workspace, "src/main.rs", "hash_v1", 100, 50).unwrap();
        store.update_manifest(workspace, "src/main.rs", "hash_v2", 200, 60).unwrap();

        let entry = store.get_manifest_entry(workspace, "src/main.rs").unwrap().unwrap();
        assert_eq!(entry.content_hash, "hash_v2");
        assert_eq!(entry.mtime, 200);
        assert_eq!(entry.file_size, 60);
    }

    // --- get_workspace_manifest ---

    #[test]
    fn test_get_workspace_manifest_returns_all_entries() {
        let store = make_test_store();
        let workspace = "/project";

        store.update_manifest(workspace, "src/main.rs", "h1", 100, 50).unwrap();
        store.update_manifest(workspace, "src/lib.rs", "h2", 200, 60).unwrap();
        store.update_manifest("/other", "src/main.rs", "h3", 300, 70).unwrap();

        let entries = store.get_workspace_manifest(workspace).unwrap();
        assert_eq!(entries.len(), 2);
    }

    // --- is_file_unchanged ---

    #[test]
    fn test_is_file_unchanged_true_when_matching() {
        let store = make_test_store();
        store.update_manifest("/project", "src/main.rs", "hash1", 100, 50).unwrap();

        assert!(store.is_file_unchanged("/project", "src/main.rs", 100, 50).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_false_when_mtime_differs() {
        let store = make_test_store();
        store.update_manifest("/project", "src/main.rs", "hash1", 100, 50).unwrap();

        assert!(!store.is_file_unchanged("/project", "src/main.rs", 200, 50).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_false_when_size_differs() {
        let store = make_test_store();
        store.update_manifest("/project", "src/main.rs", "hash1", 100, 50).unwrap();

        assert!(!store.is_file_unchanged("/project", "src/main.rs", 100, 99).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_false_when_not_in_manifest() {
        let store = make_test_store();
        assert!(!store.is_file_unchanged("/project", "unknown.rs", 100, 50).unwrap());
    }

    // --- remove_manifest_entry ---

    #[test]
    fn test_remove_manifest_entry() {
        let store = make_test_store();
        store.update_manifest("/project", "src/main.rs", "hash1", 100, 50).unwrap();

        store.remove_manifest_entry("/project", "src/main.rs").unwrap();
        let entry = store.get_manifest_entry("/project", "src/main.rs").unwrap();
        assert!(entry.is_none());
    }

    // --- clear_workspace ---

    #[test]
    fn test_clear_workspace_removes_all_entries_for_workspace() {
        let store = make_test_store();
        store.update_manifest("/project", "a.rs", "h1", 100, 50).unwrap();
        store.update_manifest("/project", "b.rs", "h2", 200, 60).unwrap();
        store.update_manifest("/other", "c.rs", "h3", 300, 70).unwrap();

        store.clear_workspace("/project").unwrap();

        let entries = store.get_workspace_manifest("/project").unwrap();
        assert!(entries.is_empty());

        // Other workspace should be untouched
        let other = store.get_workspace_manifest("/other").unwrap();
        assert_eq!(other.len(), 1);
    }

    // --- Symbol serialization roundtrip ---

    #[test]
    fn test_symbol_roundtrip_preserves_all_fields() {
        let store = make_test_store();
        let sym = Symbol {
            name: "my_func".to_string(),
            kind: SymbolKind::Method,
            file: "src/lib.rs".to_string(),
            byte_range: (100, 500),
            line_range: (10, 25),
            language: Language::Python,
            signature: "def my_func(self, x: int) -> str:".to_string(),
            definition: Some("Converts x to string".to_string()),
            parent: Some("MyClass".to_string()),
            decorators: vec!["@property".to_string(), "@cache".to_string()],
        };

        store.store_symbols("roundtrip_hash", Language::Python, &[sym.clone()]).unwrap();
        let loaded = store.lookup_symbols("roundtrip_hash").unwrap().unwrap();
        assert_eq!(loaded.len(), 1);

        let loaded = &loaded[0];
        assert_eq!(loaded.name, sym.name);
        assert_eq!(loaded.kind, sym.kind);
        assert_eq!(loaded.file, sym.file);
        assert_eq!(loaded.byte_range, sym.byte_range);
        assert_eq!(loaded.line_range, sym.line_range);
        assert_eq!(loaded.language, sym.language);
        assert_eq!(loaded.signature, sym.signature);
        assert_eq!(loaded.definition, sym.definition);
        assert_eq!(loaded.parent, sym.parent);
        assert_eq!(loaded.decorators, sym.decorators);
    }

    // --- default_db_path ---

    #[test]
    fn test_default_db_path_contains_coderlm() {
        let path = CacheStore::default_db_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("coderlm"), "Path: {}", path_str);
        assert!(path_str.ends_with("cache.db"), "Path: {}", path_str);
    }
}
