/// Bumped when parser.rs extraction logic changes (e.g. new capture names,
/// different dedup rules). Forces re-extraction of all cached symbols.
///
/// Version 4: Python class extraction was rewritten so that classes
/// containing decorated methods (e.g. `@property`) actually emit a Class
/// symbol. The previous SYMBOLS_QUERY had nested optional method captures
/// that overwrote the class capture in the parser's per-match loop. After
/// this change, all Python class symbols (including those that previously
/// went missing) are present, and methods carry the right `parent`. Cached
/// `parent: null` and missing-class entries from older runs would mask
/// this fix until the cache is invalidated.
pub const PARSER_VERSION: i64 = 4;

/// Encodes the set of tree-sitter grammar crate versions in use.
/// Bump when any `tree-sitter-*` dependency version changes in Cargo.toml.
pub const GRAMMAR_VERSION: &str = "rust0.24-py0.25-ts0.23-js0.25-go0.25-java0.23-scala0.24";

/// Bumped when the Symbol struct shape changes (fields added/removed/renamed).
/// Forces re-extraction so cached JSON stays deserializable.
pub const SYMBOL_SCHEMA_VERSION: i64 = 3;
