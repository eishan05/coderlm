pub mod parser;
pub mod queries;
pub mod symbol;

use dashmap::DashMap;
use std::collections::HashSet;

use symbol::Symbol;

/// Thread-safe symbol table with secondary indices for fast lookup.
///
/// Primary key format: "file::name::line" where line is the 1-indexed start line.
/// This prevents collisions between same-named symbols in the same file
/// (e.g., methods on different impl blocks or overloaded functions).
pub struct SymbolTable {
    /// Primary store: keyed by "file::name::line"
    pub symbols: DashMap<String, Symbol>,
    /// Secondary index: symbol name -> set of primary keys
    pub by_name: DashMap<String, HashSet<String>>,
    /// Secondary index: file path -> set of primary keys
    pub by_file: DashMap<String, HashSet<String>>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self {
            symbols: DashMap::new(),
            by_name: DashMap::new(),
            by_file: DashMap::new(),
        }
    }

    /// Build the primary key: "file::name::line".
    pub fn make_key(file: &str, name: &str, line: usize) -> String {
        format!("{}::{}::{}", file, name, line)
    }

    /// Build a legacy-format key (file::name) for backward-compatible annotation matching.
    pub fn make_legacy_key(file: &str, name: &str) -> String {
        format!("{}::{}", file, name)
    }

    pub fn insert(&self, symbol: Symbol) {
        let key = Self::make_key(&symbol.file, &symbol.name, symbol.line_range.0);

        // Update secondary indices
        self.by_name
            .entry(symbol.name.clone())
            .or_insert_with(HashSet::new)
            .insert(key.clone());
        self.by_file
            .entry(symbol.file.clone())
            .or_insert_with(HashSet::new)
            .insert(key.clone());

        self.symbols.insert(key, symbol);
    }

    pub fn remove_file(&self, file: &str) {
        if let Some((_, keys)) = self.by_file.remove(file) {
            for key in &keys {
                if let Some((_, sym)) = self.symbols.remove(key) {
                    if let Some(mut name_set) = self.by_name.get_mut(&sym.name) {
                        name_set.remove(key);
                        if name_set.is_empty() {
                            drop(name_set);
                            self.by_name.remove(&sym.name);
                        }
                    }
                }
            }
        }
    }

    /// Look up a symbol by file, name, and optional line number.
    ///
    /// When `line` is `Some`, performs an exact primary-key lookup.
    /// When `line` is `None`, finds all symbols matching file+name and returns
    /// the first one (sorted by line number). This preserves backward compatibility
    /// for callers that don't know the line number.
    pub fn get(&self, file: &str, name: &str, line: Option<usize>) -> Option<Symbol> {
        if let Some(line) = line {
            // Exact lookup by primary key
            let key = Self::make_key(file, name, line);
            return self.symbols.get(&key).map(|r| r.value().clone());
        }

        // Fallback: find all symbols with this name in this file
        self.find_by_file_and_name(file, name).into_iter().next()
    }

    /// Find all symbols matching a file and name, sorted by line number.
    pub fn find_by_file_and_name(&self, file: &str, name: &str) -> Vec<Symbol> {
        let mut results = Vec::new();
        if let Some(keys) = self.by_name.get(name) {
            let prefix = format!("{}::{}::", file, name);
            for key in keys.iter() {
                if key.starts_with(&prefix) {
                    if let Some(sym) = self.symbols.get(key) {
                        results.push(sym.value().clone());
                    }
                }
            }
        }
        results.sort_by_key(|s| s.line_range.0);
        results
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<Symbol> {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        for entry in self.symbols.iter() {
            if entry.value().name.to_lowercase().contains(&query_lower) {
                results.push(entry.value().clone());
                if results.len() >= limit {
                    break;
                }
            }
        }
        results
    }

    pub fn list_by_file(&self, file: &str) -> Vec<Symbol> {
        if let Some(keys) = self.by_file.get(file) {
            keys.iter()
                .filter_map(|key| self.symbols.get(key).map(|r| r.value().clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn all_symbols(&self) -> Vec<Symbol> {
        self.symbols.iter().map(|r| r.value().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::file_entry::Language;
    use crate::symbols::symbol::SymbolKind;

    fn make_symbol(name: &str, file: &str, line: usize, parent: Option<&str>) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Method,
            file: file.to_string(),
            byte_range: (0, 100),
            line_range: (line, line + 5),
            language: Language::Rust,
            signature: format!("fn {}()", name),
            definition: None,
            parent: parent.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_make_key_includes_line_number() {
        let key = SymbolTable::make_key("src/foo.rs", "new", 42);
        assert_eq!(key, "src/foo.rs::new::42");
    }

    #[test]
    fn test_make_legacy_key() {
        let key = SymbolTable::make_legacy_key("src/foo.rs", "new");
        assert_eq!(key, "src/foo.rs::new");
    }

    #[test]
    fn test_same_named_symbols_do_not_collide() {
        let table = SymbolTable::new();

        // Two methods named "new" in the same file but different impl blocks
        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));

        table.insert(sym1);
        table.insert(sym2);

        // Both should exist
        assert_eq!(table.len(), 2);

        // Exact lookup by line
        let found1 = table.get("src/foo.rs", "new", Some(10));
        assert!(found1.is_some());
        assert_eq!(found1.unwrap().parent.as_deref(), Some("Foo"));

        let found2 = table.get("src/foo.rs", "new", Some(50));
        assert!(found2.is_some());
        assert_eq!(found2.unwrap().parent.as_deref(), Some("Bar"));
    }

    #[test]
    fn test_get_without_line_returns_first_by_line_order() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));
        let sym2 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));

        // Insert in reverse order to test sorting
        table.insert(sym1);
        table.insert(sym2);

        // Without line, should return the one with the lowest line number
        let found = table.get("src/foo.rs", "new", None);
        assert!(found.is_some());
        assert_eq!(found.unwrap().line_range.0, 10);
    }

    #[test]
    fn test_find_by_file_and_name_returns_all_matches() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));
        let sym3 = make_symbol("new", "src/bar.rs", 10, Some("Baz")); // different file

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);

        let matches = table.find_by_file_and_name("src/foo.rs", "new");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line_range.0, 10); // sorted by line
        assert_eq!(matches[1].line_range.0, 50);
    }

    #[test]
    fn test_secondary_indices_updated_correctly() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));

        table.insert(sym1);
        table.insert(sym2);

        // by_name should have both keys under "new"
        let name_keys = table.by_name.get("new").unwrap();
        assert_eq!(name_keys.len(), 2);

        // by_file should have both keys under "src/foo.rs"
        let file_keys = table.by_file.get("src/foo.rs").unwrap();
        assert_eq!(file_keys.len(), 2);
    }

    #[test]
    fn test_remove_file_cleans_up_all_indices() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));
        let sym3 = make_symbol("new", "src/bar.rs", 10, Some("Baz"));

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);

        assert_eq!(table.len(), 3);

        table.remove_file("src/foo.rs");

        assert_eq!(table.len(), 1);
        assert!(table.by_file.get("src/foo.rs").is_none());

        // by_name should still have the entry from bar.rs
        let name_keys = table.by_name.get("new").unwrap();
        assert_eq!(name_keys.len(), 1);
    }

    #[test]
    fn test_list_by_file_returns_all_symbols() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("build", "src/foo.rs", 50, None);
        let sym3 = make_symbol("other", "src/bar.rs", 10, None);

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);

        let foo_symbols = table.list_by_file("src/foo.rs");
        assert_eq!(foo_symbols.len(), 2);
    }

    #[test]
    fn test_search_finds_matching_symbols() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new_foo", "src/foo.rs", 10, None);
        let sym2 = make_symbol("new_bar", "src/foo.rs", 50, None);
        let sym3 = make_symbol("build", "src/foo.rs", 90, None);

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);

        let results = table.search("new", 10);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let table = SymbolTable::new();

        assert!(table.get("src/foo.rs", "nonexistent", None).is_none());
        assert!(table.get("src/foo.rs", "nonexistent", Some(10)).is_none());
    }
}
