/// Bumped when parser.rs extraction logic changes (e.g. new capture names,
/// different dedup rules). Forces re-extraction of all cached symbols.
pub const PARSER_VERSION: i64 = 3;

/// Encodes the set of tree-sitter grammar crate versions in use.
/// Bump when any `tree-sitter-*` dependency version changes in Cargo.toml.
pub const GRAMMAR_VERSION: &str = "rust0.24-py0.25-ts0.23-js0.25-go0.25-java0.23-scala0.24";

/// Bumped when the Symbol struct shape changes (fields added/removed/renamed).
/// Forces re-extraction so cached JSON stays deserializable.
pub const SYMBOL_SCHEMA_VERSION: i64 = 3;
