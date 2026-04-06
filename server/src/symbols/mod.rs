pub mod parser;
pub mod queries;
pub mod symbol;

use dashmap::DashMap;
use std::collections::HashSet;

use symbol::Symbol;

/// Result of a symbol search with pagination metadata.
pub struct SearchResult {
    /// The page of matching symbols (after offset/limit applied).
    pub symbols: Vec<Symbol>,
    /// Total number of symbols matching the query (before pagination).
    pub total: usize,
}

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

    /// Look up a symbol, requiring unambiguous results for mutating operations.
    ///
    /// Like `get()`, but returns an error instead of silently picking the first
    /// match when multiple same-named symbols exist and `line` is not provided.
    /// Use this for operations that modify symbol state (define, redefine).
    pub fn get_unambiguous(
        &self,
        file: &str,
        name: &str,
        line: Option<usize>,
    ) -> Result<Symbol, String> {
        if let Some(line) = line {
            let key = Self::make_key(file, name, line);
            return self
                .symbols
                .get(&key)
                .map(|r| r.value().clone())
                .ok_or_else(|| format!("Symbol '{}' not found in '{}' at line {}", name, file, line));
        }

        let matches = self.find_by_file_and_name(file, name);
        match matches.len() {
            0 => Err(format!("Symbol '{}' not found in '{}'", name, file)),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => {
                let lines: Vec<String> = matches.iter().map(|s| s.line_range.0.to_string()).collect();
                Err(format!(
                    "Symbol '{}' is ambiguous in '{}' ({} matches at lines {}). \
                     Pass line= to disambiguate.",
                    name, file, n, lines.join(", ")
                ))
            }
        }
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

    pub fn search(&self, query: &str, offset: usize, limit: usize) -> SearchResult {
        let query_lower = query.to_lowercase();
        let mut matches: Vec<Symbol> = self
            .symbols
            .iter()
            .filter(|entry| entry.value().name.to_lowercase().contains(&query_lower))
            .map(|entry| entry.value().clone())
            .collect();

        // Deterministic ordering: sort by name, then file, then line number
        matches.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line_range.0.cmp(&b.line_range.0))
        });

        let total = matches.len();
        let symbols: Vec<Symbol> = matches.into_iter().skip(offset).take(limit).collect();

        SearchResult { symbols, total }
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
            decorators: Vec::new(),
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

        let result = table.search("new", 0, 10);
        assert_eq!(result.symbols.len(), 2);
        assert_eq!(result.total, 2);
    }

    #[test]
    fn test_search_returns_deterministic_order() {
        let table = SymbolTable::new();

        // Insert in deliberately non-alphabetical order
        let sym1 = make_symbol("create_widget", "src/z_file.rs", 10, None);
        let sym2 = make_symbol("create_thing", "src/a_file.rs", 20, None);
        let sym3 = make_symbol("create_bar", "src/m_file.rs", 30, None);
        let sym4 = make_symbol("create_bar", "src/a_file.rs", 5, None);

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);
        table.insert(sym4);

        let result = table.search("create", 0, 100);
        assert_eq!(result.total, 4);
        assert_eq!(result.symbols.len(), 4);

        // Should be sorted by name first, then file, then line
        assert_eq!(result.symbols[0].name, "create_bar");
        assert_eq!(result.symbols[0].file, "src/a_file.rs");
        assert_eq!(result.symbols[1].name, "create_bar");
        assert_eq!(result.symbols[1].file, "src/m_file.rs");
        assert_eq!(result.symbols[2].name, "create_thing");
        assert_eq!(result.symbols[2].file, "src/a_file.rs");
        assert_eq!(result.symbols[3].name, "create_widget");
        assert_eq!(result.symbols[3].file, "src/z_file.rs");

        // Running the same search again should produce identical results
        let result2 = table.search("create", 0, 100);
        for (a, b) in result.symbols.iter().zip(result2.symbols.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.file, b.file);
            assert_eq!(a.line_range, b.line_range);
        }
    }

    #[test]
    fn test_search_pagination_offset_and_limit() {
        let table = SymbolTable::new();

        // Insert 5 symbols matching "item"
        for i in 0..5 {
            let sym = make_symbol(
                &format!("item_{}", (b'a' + i) as char),
                "src/items.rs",
                (i as usize) * 10 + 10,
                None,
            );
            table.insert(sym);
        }

        // Full result
        let full = table.search("item", 0, 100);
        assert_eq!(full.total, 5);
        assert_eq!(full.symbols.len(), 5);

        // First page of 2
        let page1 = table.search("item", 0, 2);
        assert_eq!(page1.total, 5);
        assert_eq!(page1.symbols.len(), 2);
        assert_eq!(page1.symbols[0].name, "item_a");
        assert_eq!(page1.symbols[1].name, "item_b");

        // Second page of 2
        let page2 = table.search("item", 2, 2);
        assert_eq!(page2.total, 5);
        assert_eq!(page2.symbols.len(), 2);
        assert_eq!(page2.symbols[0].name, "item_c");
        assert_eq!(page2.symbols[1].name, "item_d");

        // Third page of 2 (only 1 remaining)
        let page3 = table.search("item", 4, 2);
        assert_eq!(page3.total, 5);
        assert_eq!(page3.symbols.len(), 1);
        assert_eq!(page3.symbols[0].name, "item_e");

        // Offset beyond total returns empty
        let empty = table.search("item", 10, 2);
        assert_eq!(empty.total, 5);
        assert_eq!(empty.symbols.len(), 0);
    }

    #[test]
    fn test_search_total_count_independent_of_limit() {
        let table = SymbolTable::new();

        for i in 0..10 {
            let sym = make_symbol(
                &format!("handler_{}", i),
                "src/handlers.rs",
                i * 10 + 10,
                None,
            );
            table.insert(sym);
        }

        // limit=3 but total should still reflect all 10 matches
        let result = table.search("handler", 0, 3);
        assert_eq!(result.total, 10);
        assert_eq!(result.symbols.len(), 3);
    }

    #[test]
    fn test_search_same_name_different_files_deterministic() {
        let table = SymbolTable::new();

        // Same name in different files — ordering by file should be stable
        let sym1 = make_symbol("init", "src/b.rs", 10, None);
        let sym2 = make_symbol("init", "src/a.rs", 10, None);
        let sym3 = make_symbol("init", "src/c.rs", 10, None);

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);

        let result = table.search("init", 0, 100);
        assert_eq!(result.symbols[0].file, "src/a.rs");
        assert_eq!(result.symbols[1].file, "src/b.rs");
        assert_eq!(result.symbols[2].file, "src/c.rs");
    }

    #[test]
    fn test_search_same_name_same_file_sorted_by_line() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("render", "src/view.rs", 100, None);
        let sym2 = make_symbol("render", "src/view.rs", 20, None);
        let sym3 = make_symbol("render", "src/view.rs", 50, None);

        table.insert(sym1);
        table.insert(sym2);
        table.insert(sym3);

        let result = table.search("render", 0, 100);
        assert_eq!(result.symbols[0].line_range.0, 20);
        assert_eq!(result.symbols[1].line_range.0, 50);
        assert_eq!(result.symbols[2].line_range.0, 100);
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let table = SymbolTable::new();

        assert!(table.get("src/foo.rs", "nonexistent", None).is_none());
        assert!(table.get("src/foo.rs", "nonexistent", Some(10)).is_none());
    }

    #[test]
    fn test_get_unambiguous_returns_single_match() {
        let table = SymbolTable::new();

        let sym = make_symbol("build", "src/foo.rs", 10, None);
        table.insert(sym);

        let result = table.get_unambiguous("src/foo.rs", "build", None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().line_range.0, 10);
    }

    #[test]
    fn test_get_unambiguous_rejects_ambiguous_without_line() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));

        table.insert(sym1);
        table.insert(sym2);

        let result = table.get_unambiguous("src/foo.rs", "new", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("10"));
        assert!(err.contains("50"));
    }

    #[test]
    fn test_get_unambiguous_succeeds_with_line() {
        let table = SymbolTable::new();

        let sym1 = make_symbol("new", "src/foo.rs", 10, Some("Foo"));
        let sym2 = make_symbol("new", "src/foo.rs", 50, Some("Bar"));

        table.insert(sym1);
        table.insert(sym2);

        let result = table.get_unambiguous("src/foo.rs", "new", Some(50));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().parent.as_deref(), Some("Bar"));
    }

    #[test]
    fn test_get_unambiguous_not_found() {
        let table = SymbolTable::new();

        let result = table.get_unambiguous("src/foo.rs", "nonexistent", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
