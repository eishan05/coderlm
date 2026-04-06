pub mod go;
pub mod java;
pub mod python;
pub mod rust;
pub mod scala;
pub mod typescript;

use crate::index::file_entry::Language;

/// Get the tree-sitter language and symbol query for a given language.
pub fn get_language_config(lang: Language) -> Option<LanguageConfig> {
    match lang {
        Language::Rust => Some(rust::config()),
        Language::Python => Some(python::config()),
        Language::TypeScript => Some(typescript::config()),
        Language::JavaScript => Some(typescript::js_config()),
        Language::Go => Some(go::config()),
        Language::Java => Some(java::config()),
        Language::Scala => Some(scala::config()),
        _ => None,
    }
}

pub struct LanguageConfig {
    pub language: tree_sitter::Language,
    pub symbols_query: &'static str,
    /// Tree-sitter query for call expressions. Captures `@callee` for the called name.
    pub callers_query: &'static str,
    /// Tree-sitter query for local variable bindings. Captures `@var.name`.
    pub variables_query: &'static str,
    /// Tree-sitter query for import statements. Captures `@import.source` for the
    /// module/package being imported (e.g., `os`, `std::collections`, `./utils`).
    pub imports_query: &'static str,
    pub test_patterns: Vec<TestPattern>,
}

#[derive(Debug)]
pub enum TestPattern {
    /// Match functions whose name starts with a prefix (e.g., "test_" in Python)
    FunctionPrefix(&'static str),
    /// Match functions with a specific attribute/decorator (e.g., #[test] in Rust, @Test in Java).
    /// Checks the symbol's `decorators` field and also scans source lines preceding the symbol.
    Attribute(&'static str),
    /// Match call expressions (e.g., it(), test(), describe() in JS/TS)
    CallExpression(&'static str),
    /// Match symbols whose file path contains the given substring (e.g., "/tests/", ".test.", "__tests__")
    FileContains(&'static str),
    /// Match symbols whose file path ends with the given suffix (e.g., "_test.go")
    FileEndsWith(&'static str),
}
