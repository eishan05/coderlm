use std::path::Path;
use std::sync::Arc;

use tree_sitter::StreamingIterator;

use crate::index::file_entry::Language;
use crate::index::file_tree::FileTree;
use crate::symbols::queries::{self, TestPattern};
use crate::symbols::symbol::{Symbol, SymbolKind};
use crate::symbols::SymbolTable;

pub fn list_symbols(
    symbol_table: &Arc<SymbolTable>,
    kind_filter: Option<SymbolKind>,
    file_filter: Option<&str>,
    limit: usize,
) -> Vec<Symbol> {
    let mut results: Vec<Symbol> = if let Some(file) = file_filter {
        symbol_table.list_by_file(file)
    } else {
        symbol_table.all_symbols()
    };

    if let Some(kind) = kind_filter {
        results.retain(|s| s.kind == kind);
    }

    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_range.0.cmp(&b.line_range.0)));
    results.truncate(limit);
    results
}

pub fn search_symbols(
    symbol_table: &Arc<SymbolTable>,
    query: &str,
    offset: usize,
    limit: usize,
) -> crate::symbols::SearchResult {
    symbol_table.search(query, offset, limit)
}

/// Result of a symbol implementation lookup, including optional ambiguity info.
#[derive(Debug, serde::Serialize)]
pub struct ImplResult {
    pub source: String,
    /// Warning when multiple same-named symbols exist and no line was specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Candidate symbols when ambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<ImplCandidate>>,
}

#[derive(Debug, serde::Serialize)]
pub struct ImplCandidate {
    pub line: usize,
    pub parent: Option<String>,
}

pub fn get_implementation(
    root: &Path,
    symbol_table: &Arc<SymbolTable>,
    symbol_name: &str,
    file: &str,
    line: Option<usize>,
) -> Result<ImplResult, String> {
    let sym = symbol_table
        .get(file, symbol_name, line)
        .ok_or_else(|| format!("Symbol '{}' not found in '{}'", symbol_name, file))?;

    let abs_path = root.join(&sym.file);
    let source = std::fs::read_to_string(&abs_path)
        .map_err(|e| format!("Failed to read '{}': {}", sym.file, e))?;

    let start = sym.byte_range.0;
    let end = sym.byte_range.1.min(source.len());
    if start >= source.len() {
        return Err(format!(
            "Symbol '{}' source is stale (byte_range start {} >= file length {}). File has been modified.",
            symbol_name, start, source.len()
        ));
    }

    // Check for ambiguity when no line hint was provided
    let (warning, candidates) = if line.is_none() {
        let matches = symbol_table.find_by_file_and_name(file, symbol_name);
        if matches.len() > 1 {
            let cands: Vec<ImplCandidate> = matches.iter().map(|s| ImplCandidate {
                line: s.line_range.0,
                parent: s.parent.clone(),
            }).collect();
            (
                Some(format!(
                    "{} matches found, returning first. Use 'line' to disambiguate.",
                    matches.len()
                )),
                Some(cands),
            )
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    Ok(ImplResult {
        source: source[start..end].to_string(),
        warning,
        candidates,
    })
}

pub fn define_symbol(
    symbol_table: &Arc<SymbolTable>,
    symbol_name: &str,
    file: &str,
    definition: &str,
    line: Option<usize>,
) -> Result<(), String> {
    // Use get_unambiguous for mutating operations: require disambiguation
    // when multiple same-named symbols exist in the same file.
    let sym = symbol_table.get_unambiguous(file, symbol_name, line)?;

    let key = SymbolTable::make_key(file, symbol_name, sym.line_range.0);
    if let Some(mut entry) = symbol_table.symbols.get_mut(&key) {
        if entry.definition.is_some() {
            return Err(format!(
                "Symbol '{}' in '{}' already has a definition. Use redefine.",
                symbol_name, file
            ));
        }
        entry.definition = Some(definition.to_string());
        Ok(())
    } else {
        Err(format!("Symbol '{}' not found in '{}'", symbol_name, file))
    }
}

pub fn redefine_symbol(
    symbol_table: &Arc<SymbolTable>,
    symbol_name: &str,
    file: &str,
    definition: &str,
    line: Option<usize>,
) -> Result<(), String> {
    // Use get_unambiguous for mutating operations: require disambiguation
    // when multiple same-named symbols exist in the same file.
    let sym = symbol_table.get_unambiguous(file, symbol_name, line)?;

    let key = SymbolTable::make_key(file, symbol_name, sym.line_range.0);
    if let Some(mut entry) = symbol_table.symbols.get_mut(&key) {
        entry.definition = Some(definition.to_string());
        Ok(())
    } else {
        Err(format!("Symbol '{}' not found in '{}'", symbol_name, file))
    }
}

/// Find callers of a symbol using tree-sitter call-expression queries.
/// Falls back to regex for files without tree-sitter support.
pub fn find_callers(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
    symbol_name: &str,
    file: &str,
    limit: usize,
    line: Option<usize>,
) -> Result<Vec<CallerInfo>, String> {
    // Verify symbol exists
    let _sym = symbol_table
        .get(file, symbol_name, line)
        .ok_or_else(|| format!("Symbol '{}' not found in '{}'", symbol_name, file))?;

    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut callers = Vec::new();

    for entry in file_tree.files.iter() {
        let rel_path = entry.key().clone();
        let language = entry.value().language;

        // Skip documentation files from regex fallback — they produce
        // false positives (prose mentions, code examples in markdown).
        if !language.has_tree_sitter_support() && is_documentation_file(&rel_path) {
            continue;
        }

        let abs_path = root.join(&rel_path);

        let source = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let file_callers = if language.has_tree_sitter_support() {
            find_callers_ast(&source, &rel_path, language, symbol_name, file)
        } else {
            find_callers_regex(&source, &rel_path, symbol_name, file)
        };

        for caller in file_callers {
            callers.push(caller);
            if callers.len() >= limit {
                return Ok(callers);
            }
        }
    }

    Ok(callers)
}

/// AST-aware caller detection: parse the file, run the callers query,
/// and check if any call-expression callee matches the target symbol name.
fn find_callers_ast(
    source: &str,
    rel_path: &str,
    language: Language,
    symbol_name: &str,
    definition_file: &str,
) -> Vec<CallerInfo> {
    let config = match queries::get_language_config(language) {
        Some(c) => c,
        None => return find_callers_regex(source, rel_path, symbol_name, definition_file),
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&config.language).is_err() {
        return find_callers_regex(source, rel_path, symbol_name, definition_file);
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return find_callers_regex(source, rel_path, symbol_name, definition_file),
    };

    let query = match tree_sitter::Query::new(&config.language, config.callers_query) {
        Ok(q) => q,
        Err(_) => return find_callers_regex(source, rel_path, symbol_name, definition_file),
    };

    let capture_names: Vec<String> = query.capture_names().iter().map(|s| s.to_string()).collect();
    let callee_idx = capture_names.iter().position(|n| n == "callee");

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut callers = Vec::new();

    while let Some(m) = matches.next() {
        for cap in m.captures {
            if Some(cap.index as usize) == callee_idx {
                let text = cap.node.utf8_text(source.as_bytes()).unwrap_or("");
                if text == symbol_name {
                    let line_num = cap.node.start_position().row + 1;
                    // Skip the definition itself
                    if rel_path == definition_file {
                        let line_text = source
                            .lines()
                            .nth(line_num - 1)
                            .unwrap_or("");
                        if is_definition_line(line_text, symbol_name, language) {
                            continue;
                        }
                    }
                    let line_text = source
                        .lines()
                        .nth(line_num - 1)
                        .map(|l| l.trim().to_string())
                        .unwrap_or_default();
                    callers.push(CallerInfo {
                        file: rel_path.to_string(),
                        line: line_num,
                        text: line_text,
                    });
                }
            }
        }
    }

    callers
}

/// Regex fallback for files without tree-sitter support.
fn find_callers_regex(
    source: &str,
    rel_path: &str,
    symbol_name: &str,
    _definition_file: &str,
) -> Vec<CallerInfo> {
    // Use word boundaries to avoid matching substrings (e.g. "foo" inside "foobar")
    let pattern = match regex::Regex::new(&format!(r"\b{}\b", regex::escape(symbol_name))) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut callers = Vec::new();

    for (line_num, line) in source.lines().enumerate() {
        if pattern.is_match(line) {
            // Skip definitions in ANY file — not just the file where the symbol
            // was originally defined. Other files may contain copies or
            // re-definitions of the same name (e.g. a shell script embedding
            // Python code).
            if is_definition_line_regex(line, symbol_name) {
                continue;
            }

            callers.push(CallerInfo {
                file: rel_path.to_string(),
                line: line_num + 1,
                text: line.trim().to_string(),
            });
        }
    }

    callers
}

/// Language-agnostic check for whether a line looks like a definition of
/// the given symbol name. Used by the regex caller fallback to filter out
/// definitions across all files.
fn is_definition_line_regex(line: &str, name: &str) -> bool {
    let trimmed = line.trim();
    // Common definition keywords across languages
    trimmed.contains(&format!("fn {}", name))
        || trimmed.contains(&format!("def {}", name))
        || trimmed.contains(&format!("function {}", name))
        || trimmed.contains(&format!("func {}", name))
        || trimmed.contains(&format!("class {}", name))
        || trimmed.contains(&format!("interface {}", name))
        || trimmed.contains(&format!("object {}", name))
        || trimmed.contains(&format!("trait {}", name))
        || trimmed.contains(&format!("type {}", name))
        || trimmed.contains(&format!("enum {}", name))
        || trimmed.contains(&format!("struct {}", name))
        || trimmed.contains(&format!("const {}", name))
        || trimmed.contains(&format!("let {}", name))
        || trimmed.contains(&format!("var {}", name))
        || trimmed.contains(&format!("val {}", name))
}

fn is_definition_line(line: &str, name: &str, language: Language) -> bool {
    match language {
        Language::Rust => line.contains(&format!("fn {}", name)),
        Language::Python => line.contains(&format!("def {}", name)),
        Language::TypeScript | Language::JavaScript => {
            line.contains(&format!("function {}", name))
                || line.contains(&format!("{} =", name))
        }
        Language::Go => line.contains(&format!("func {}", name)),
        Language::Java => {
            line.contains(&format!("class {}", name))
                || line.contains(&format!("interface {}", name))
                || line.contains(&format!("enum {}", name))
                || (line.contains(name) && (line.contains("void ") || line.contains("int ")
                    || line.contains("String ") || line.contains("boolean ")
                    || line.contains("long ") || line.contains("double ")
                    || line.contains("float ") || line.contains("public ")
                    || line.contains("private ") || line.contains("protected ")))
        }
        Language::Scala => {
            line.contains(&format!("def {}", name))
                || line.contains(&format!("object {}", name))
                || line.contains(&format!("class {}", name))
                || line.contains(&format!("trait {}", name))
        }
        Language::Sql => {
            let lower = line.to_lowercase();
            let lower_name = name.to_lowercase();
            lower.contains("create") && lower.contains(&lower_name)
        }
        _ => false,
    }
}

/// Check if a file path refers to a documentation file (Markdown, text, etc.)
/// that should be excluded from regex-based caller detection.
fn is_documentation_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".rst")
        || lower.ends_with(".adoc")
        || lower.ends_with(".rdoc")
}

#[derive(Debug, serde::Serialize)]
pub struct CallerInfo {
    pub file: String,
    pub line: usize,
    pub text: String,
}

/// Find test functions that reference a given symbol.
pub fn find_tests(
    root: &Path,
    _file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
    symbol_name: &str,
    file: &str,
    limit: usize,
    line: Option<usize>,
) -> Result<Vec<TestInfo>, String> {
    let _sym = symbol_table
        .get(file, symbol_name, line)
        .ok_or_else(|| format!("Symbol '{}' not found in '{}'", symbol_name, file))?;

    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut tests = Vec::new();

    // Look through all symbols for test functions
    for entry in symbol_table.symbols.iter() {
        let sym = entry.value();

        // Read the source for attribute checking and body-reference checking
        let abs_path = root.join(&sym.file);
        let source = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !is_test_symbol(sym, Some(&source)) {
            continue;
        }

        // Check if the test function body references the target symbol
        let start = sym.byte_range.0;
        let end = sym.byte_range.1.min(source.len());
        // Guard against stale byte ranges after file modification
        if start >= source.len() {
            continue;
        }
        let body = &source[start..end];

        if contains_word(body, symbol_name) {
            tests.push(TestInfo {
                name: sym.name.clone(),
                file: sym.file.clone(),
                line: sym.line_range.0,
                signature: sym.signature.clone(),
            });

            if tests.len() >= limit {
                break;
            }
        }
    }

    Ok(tests)
}

/// Check if a single `TestPattern` matches a symbol.
///
/// `source` is optionally provided for `Attribute` pattern matching,
/// which needs to inspect lines preceding the symbol definition.
fn matches_test_pattern(pattern: &TestPattern, sym: &Symbol, source: Option<&str>) -> bool {
    match pattern {
        TestPattern::FunctionPrefix(prefix) => sym.name.starts_with(prefix),

        TestPattern::Attribute(attr) => {
            // First check the symbol's decorator list (populated for Python)
            if sym.decorators.iter().any(|d| d.contains(attr)) {
                return true;
            }
            // For languages like Rust (#[test]) and Java (@Test), scan
            // the source lines immediately preceding the symbol's start byte.
            if let Some(src) = source {
                let start = sym.byte_range.0.min(src.len());
                // Look at up to 512 bytes before the symbol start for attributes
                let scan_start = start.saturating_sub(512);
                let prefix_text = &src[scan_start..start];
                // Check for Rust-style #[attr] or #[something::attr]
                if prefix_text.contains(&format!("#[{}]", attr))
                    || prefix_text.contains(&format!("#[{}(", attr))
                    || prefix_text.contains(&format!("::{}", attr))
                {
                    return true;
                }
                // Check for Java/Scala-style @Attr
                if prefix_text.contains(&format!("@{}", attr)) {
                    return true;
                }
            }
            false
        }

        TestPattern::CallExpression(call_name) => {
            // For JS/TS test frameworks: if the symbol name matches the call
            // expression name (e.g., it(), test(), describe()), it's a test symbol.
            // Also match symbols within files that contain these call expressions.
            sym.name == *call_name
        }

        TestPattern::FileContains(substr) => sym.file.contains(substr),

        TestPattern::FileEndsWith(suffix) => sym.file.ends_with(suffix),
    }
}

/// Determine whether a symbol is a test symbol by consulting the language's
/// `TestPattern` configuration from `queries::get_language_config()`.
///
/// Falls back to basic heuristics for languages without tree-sitter support.
fn is_test_symbol(sym: &Symbol, source: Option<&str>) -> bool {
    if let Some(config) = queries::get_language_config(sym.language) {
        config
            .test_patterns
            .iter()
            .any(|p| matches_test_pattern(p, sym, source))
    } else {
        // Fallback for languages without a LanguageConfig (e.g., SQL, C, etc.)
        false
    }
}

/// Check if `haystack` contains `needle` as a whole word (not as a substring
/// of a larger identifier). A word boundary is defined as: start/end of string,
/// or any character that is not alphanumeric or underscore.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let haystack_bytes = haystack.as_bytes();
    let needle_len = needle.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs_pos = start + pos;
        let before_ok = abs_pos == 0 || {
            let c = haystack_bytes[abs_pos - 1];
            !c.is_ascii_alphanumeric() && c != b'_'
        };
        let after_pos = abs_pos + needle_len;
        let after_ok = after_pos >= haystack.len() || {
            let c = haystack_bytes[after_pos];
            !c.is_ascii_alphanumeric() && c != b'_'
        };
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

#[derive(Debug, serde::Serialize)]
pub struct TestInfo {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub signature: String,
}

/// List local variables within a function using tree-sitter queries.
/// Falls back to regex for languages without tree-sitter support.
pub fn list_variables(
    root: &Path,
    symbol_table: &Arc<SymbolTable>,
    function_name: &str,
    file: &str,
    line: Option<usize>,
) -> Result<Vec<VariableInfo>, String> {
    let sym = symbol_table
        .get(file, function_name, line)
        .ok_or_else(|| format!("Symbol '{}' not found in '{}'", function_name, file))?;

    let abs_path = root.join(&sym.file);
    let source = std::fs::read_to_string(&abs_path)
        .map_err(|e| format!("Failed to read '{}': {}", sym.file, e))?;

    let start = sym.byte_range.0;
    let end = sym.byte_range.1.min(source.len());

    if start >= source.len() {
        return Err(format!(
            "Symbol '{}' source is stale (byte_range start {} >= file length {}). File has been modified.",
            function_name, start, source.len()
        ));
    }

    let variables = if sym.language.has_tree_sitter_support() {
        list_variables_ast(&source, sym.language, start, end, function_name)
    } else {
        list_variables_regex(&source[start..end], sym.language, function_name)
    };

    Ok(variables)
}

/// AST-aware variable extraction: parse the function body slice, run the
/// variables query, and collect all @var.name captures within the byte range.
fn list_variables_ast(
    source: &str,
    language: Language,
    fn_start: usize,
    fn_end: usize,
    function_name: &str,
) -> Vec<VariableInfo> {
    let config = match queries::get_language_config(language) {
        Some(c) => c,
        None => return list_variables_regex(&source[fn_start..fn_end], language, function_name),
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&config.language).is_err() {
        return list_variables_regex(&source[fn_start..fn_end], language, function_name);
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return list_variables_regex(&source[fn_start..fn_end], language, function_name),
    };

    let query = match tree_sitter::Query::new(&config.language, config.variables_query) {
        Ok(q) => q,
        Err(_) => return list_variables_regex(&source[fn_start..fn_end], language, function_name),
    };

    let capture_names: Vec<String> = query.capture_names().iter().map(|s| s.to_string()).collect();
    let var_name_idx = capture_names.iter().position(|n| n == "var.name");

    let mut cursor = tree_sitter::QueryCursor::new();
    // Restrict matches to the function's byte range
    cursor.set_byte_range(fn_start..fn_end);
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut variables = Vec::new();
    let mut seen = std::collections::HashSet::new();

    while let Some(m) = matches.next() {
        for cap in m.captures {
            if Some(cap.index as usize) == var_name_idx {
                let text = cap.node.utf8_text(source.as_bytes()).unwrap_or("");
                if !text.is_empty() && text != "self" && text != "_" && seen.insert(text.to_string()) {
                    variables.push(VariableInfo {
                        name: text.to_string(),
                        function: function_name.to_string(),
                    });
                }
            }
        }
    }

    variables
}

/// Regex fallback for variable extraction.
fn list_variables_regex(
    body: &str,
    language: Language,
    function_name: &str,
) -> Vec<VariableInfo> {
    let mut variables = Vec::new();

    match language {
        Language::Rust => {
            let let_re = regex::Regex::new(r"let\s+(mut\s+)?(\w+)").unwrap();
            for cap in let_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[2].to_string(),
                    function: function_name.to_string(),
                });
            }
        }
        Language::Python => {
            let assign_re = regex::Regex::new(r"^\s+(\w+)\s*=").unwrap();
            for cap in assign_re.captures_iter(body) {
                let name = cap[1].to_string();
                if name != "self" && !name.starts_with('_') {
                    variables.push(VariableInfo {
                        name,
                        function: function_name.to_string(),
                    });
                }
            }
        }
        Language::TypeScript | Language::JavaScript => {
            let var_re = regex::Regex::new(r"(?:let|const|var)\s+(\w+)").unwrap();
            for cap in var_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
        }
        Language::Go => {
            let short_re = regex::Regex::new(r"(\w+)\s*:=").unwrap();
            for cap in short_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
            let var_re = regex::Regex::new(r"var\s+(\w+)").unwrap();
            for cap in var_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
        }
        Language::Java => {
            let var_re = regex::Regex::new(r"\b(?:int|long|float|double|boolean|char|byte|short|String|var|final\s+\w+)\s+(\w+)\s*[=;,)]").unwrap();
            for cap in var_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
        }
        Language::Scala => {
            let val_re = regex::Regex::new(r"\b(?:val|var)\s+(\w+)").unwrap();
            for cap in val_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
        }
        Language::Sql => {
            let declare_re = regex::Regex::new(r"(?i)DECLARE\s+@?(\w+)").unwrap();
            for cap in declare_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
            let plsql_re = regex::Regex::new(r"(\w+)\s+\w+\s*:=").unwrap();
            for cap in plsql_re.captures_iter(body) {
                variables.push(VariableInfo {
                    name: cap[1].to_string(),
                    function: function_name.to_string(),
                });
            }
        }
        _ => {}
    }

    // Deduplicate
    variables.sort_by(|a, b| a.name.cmp(&b.name));
    variables.dedup_by(|a, b| a.name == b.name);

    variables
}

#[derive(Debug, serde::Serialize)]
pub struct VariableInfo {
    pub name: String,
    pub function: String,
}

// ---------------------------------------------------------------------------
// File outline (structural summary)
// ---------------------------------------------------------------------------

/// A structured outline of a single file, grouping symbols by kind.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileOutline {
    /// Relative path of the file.
    pub file: String,
    /// Detected language.
    pub language: Language,
    /// Total number of lines in the file.
    pub line_count: usize,
    /// Symbol groups, keyed by a human-readable kind label (e.g. "Functions",
    /// "Structs"), each containing a list of symbol summaries sorted by line.
    pub groups: Vec<SymbolGroup>,
}

/// A group of symbols sharing the same kind.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolGroup {
    /// Human-readable group label, e.g. "Functions", "Structs".
    pub kind: String,
    /// Symbols within this group, sorted by start line.
    pub symbols: Vec<SymbolOutlineEntry>,
}

/// A single symbol in the outline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolOutlineEntry {
    pub name: String,
    pub signature: String,
    pub line: usize,
    /// Parent symbol (e.g. struct for a method).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// Human-readable plural label for a `SymbolKind`.
fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "Functions",
        SymbolKind::Method => "Methods",
        SymbolKind::Class => "Classes",
        SymbolKind::Struct => "Structs",
        SymbolKind::Enum => "Enums",
        SymbolKind::Trait => "Traits",
        SymbolKind::Interface => "Interfaces",
        SymbolKind::Constant => "Constants",
        SymbolKind::Variable => "Variables",
        SymbolKind::Type => "Types",
        SymbolKind::Module => "Modules",
        SymbolKind::Macro => "Macros",
        SymbolKind::Import => "Imports",
        SymbolKind::Other => "Other",
    }
}

/// Ordering key so that groups appear in a conventional, stable order
/// (imports first, then types/structs/classes, then functions/methods, etc.).
fn kind_order(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Import => 0,
        SymbolKind::Module => 1,
        SymbolKind::Constant => 2,
        SymbolKind::Type => 3,
        SymbolKind::Struct => 4,
        SymbolKind::Class => 5,
        SymbolKind::Enum => 6,
        SymbolKind::Trait => 7,
        SymbolKind::Interface => 8,
        SymbolKind::Function => 9,
        SymbolKind::Method => 10,
        SymbolKind::Macro => 11,
        SymbolKind::Variable => 12,
        SymbolKind::Other => 13,
    }
}

/// Generate a structured outline for a file.
///
/// The outline groups the file's symbols by kind, shows each symbol's
/// signature and line number, and includes the file's language and line count.
pub fn generate_outline(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
    file: &str,
) -> Result<FileOutline, String> {
    // Verify the file exists in the tree
    let entry = file_tree
        .get(file)
        .ok_or_else(|| format!("File '{}' not found in index", file))?;

    // Count lines by reading the file
    let abs_path = root.join(file);
    let source = std::fs::read_to_string(&abs_path)
        .map_err(|e| format!("Failed to read '{}': {}", file, e))?;
    let line_count = source.lines().count();

    // Collect symbols for this file
    let mut symbols = symbol_table.list_by_file(file);
    symbols.sort_by_key(|s| (kind_order(s.kind), s.line_range.0));

    // Group by kind
    let mut groups_map: std::collections::BTreeMap<u8, (SymbolKind, Vec<SymbolOutlineEntry>)> =
        std::collections::BTreeMap::new();

    for sym in &symbols {
        let order = kind_order(sym.kind);
        let group = groups_map
            .entry(order)
            .or_insert_with(|| (sym.kind, Vec::new()));
        group.1.push(SymbolOutlineEntry {
            name: sym.name.clone(),
            signature: sym.signature.clone(),
            line: sym.line_range.0,
            parent: sym.parent.clone(),
        });
    }

    let groups: Vec<SymbolGroup> = groups_map
        .into_values()
        .map(|(kind, entries)| SymbolGroup {
            kind: kind_label(kind).to_string(),
            symbols: entries,
        })
        .collect();

    Ok(FileOutline {
        file: file.to_string(),
        language: entry.language,
        line_count,
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::file_entry::Language;
    use crate::symbols::symbol::{Symbol, SymbolKind};

    /// Helper to create a minimal Symbol for testing `is_test_symbol`.
    fn make_symbol(name: &str, file: &str, language: Language) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            file: file.to_string(),
            byte_range: (0, 100),
            line_range: (1, 5),
            language,
            signature: format!("fn {}()", name),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        }
    }

    /// Helper to create a Symbol with decorators.
    fn make_symbol_with_decorators(
        name: &str,
        file: &str,
        language: Language,
        decorators: Vec<String>,
    ) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            file: file.to_string(),
            byte_range: (0, 100),
            line_range: (1, 5),
            language,
            signature: format!("fn {}()", name),
            definition: None,
            parent: None,
            decorators,
            doc_comment: None,
        }
    }

    // ---- Rust tests ----

    #[test]
    fn test_rust_function_prefix_matches_test_symbol() {
        let sym = make_symbol("test_something", "src/lib.rs", Language::Rust);
        assert!(
            is_test_symbol(&sym, None),
            "Rust symbol with 'test' prefix should be identified as test"
        );
    }

    #[test]
    fn test_rust_file_in_tests_dir_matches() {
        let sym = make_symbol("helper_setup", "src/tests/helpers.rs", Language::Rust);
        assert!(
            is_test_symbol(&sym, None),
            "Rust symbol in /tests/ directory should be identified as test"
        );
    }

    #[test]
    fn test_rust_attribute_test_matches() {
        // Simulate source where #[test] attribute precedes the function
        let source = "use std::io;\n\n#[test]\nfn my_function() {\n    assert!(true);\n}\n";
        let sym = Symbol {
            name: "my_function".to_string(),
            kind: SymbolKind::Function,
            file: "src/lib.rs".to_string(),
            byte_range: (24, 58), // starts at "fn my_function..."
            line_range: (4, 6),
            language: Language::Rust,
            signature: "fn my_function()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        };
        assert!(
            is_test_symbol(&sym, Some(source)),
            "Rust symbol preceded by #[test] should be identified as test"
        );
    }

    #[test]
    fn test_rust_non_test_symbol_not_matched() {
        let sym = make_symbol("process_data", "src/main.rs", Language::Rust);
        assert!(
            !is_test_symbol(&sym, None),
            "Regular Rust symbol should NOT be identified as test"
        );
    }

    // ---- Python tests ----

    #[test]
    fn test_python_function_prefix_matches() {
        let sym = make_symbol("test_create_user", "tests/test_user.py", Language::Python);
        assert!(
            is_test_symbol(&sym, None),
            "Python symbol with 'test_' prefix should be identified as test"
        );
    }

    #[test]
    fn test_python_file_with_test_prefix_matches() {
        let sym = make_symbol("helper", "test_utils.py", Language::Python);
        assert!(
            is_test_symbol(&sym, None),
            "Python symbol in test_ file should be identified as test"
        );
    }

    #[test]
    fn test_python_file_with_test_suffix_matches() {
        let sym = make_symbol("helper", "utils_test.py", Language::Python);
        assert!(
            is_test_symbol(&sym, None),
            "Python symbol in _test. file should be identified as test"
        );
    }

    #[test]
    fn test_python_non_test_symbol_not_matched() {
        let sym = make_symbol("process", "src/main.py", Language::Python);
        assert!(
            !is_test_symbol(&sym, None),
            "Regular Python symbol should NOT be identified as test"
        );
    }

    // ---- TypeScript/JavaScript tests ----

    #[test]
    fn test_ts_file_with_test_dot_matches() {
        let sym = make_symbol("someHelper", "src/utils.test.ts", Language::TypeScript);
        assert!(
            is_test_symbol(&sym, None),
            "TypeScript symbol in .test. file should be identified as test"
        );
    }

    #[test]
    fn test_ts_file_with_spec_dot_matches() {
        let sym = make_symbol("someHelper", "src/utils.spec.ts", Language::TypeScript);
        assert!(
            is_test_symbol(&sym, None),
            "TypeScript symbol in .spec. file should be identified as test"
        );
    }

    #[test]
    fn test_ts_file_in_tests_dir_matches() {
        let sym = make_symbol("someHelper", "src/__tests__/utils.ts", Language::TypeScript);
        assert!(
            is_test_symbol(&sym, None),
            "TypeScript symbol in __tests__ directory should be identified as test"
        );
    }

    #[test]
    fn test_js_file_with_test_dot_matches() {
        let sym = make_symbol("someHelper", "src/utils.test.js", Language::JavaScript);
        assert!(
            is_test_symbol(&sym, None),
            "JavaScript symbol in .test. file should be identified as test"
        );
    }

    #[test]
    fn test_ts_call_expression_test_matches() {
        let sym = make_symbol("test", "src/app.test.ts", Language::TypeScript);
        assert!(
            is_test_symbol(&sym, None),
            "TypeScript 'test' call expression symbol should be identified as test"
        );
    }

    #[test]
    fn test_ts_non_test_symbol_not_matched() {
        let sym = make_symbol("render", "src/App.tsx", Language::TypeScript);
        assert!(
            !is_test_symbol(&sym, None),
            "Regular TypeScript symbol should NOT be identified as test"
        );
    }

    // ---- Go tests ----

    #[test]
    fn test_go_function_prefix_matches() {
        let sym = make_symbol("TestCreateUser", "user_test.go", Language::Go);
        assert!(
            is_test_symbol(&sym, None),
            "Go symbol with 'Test' prefix should be identified as test"
        );
    }

    #[test]
    fn test_go_file_suffix_matches() {
        let sym = make_symbol("helperSetup", "user_test.go", Language::Go);
        assert!(
            is_test_symbol(&sym, None),
            "Go symbol in _test.go file should be identified as test"
        );
    }

    #[test]
    fn test_go_non_test_symbol_not_matched() {
        let sym = make_symbol("ProcessData", "main.go", Language::Go);
        assert!(
            !is_test_symbol(&sym, None),
            "Regular Go symbol should NOT be identified as test"
        );
    }

    // ---- Java tests ----

    #[test]
    fn test_java_file_contains_test_matches() {
        let sym = make_symbol("setUp", "src/test/java/UserTest.java", Language::Java);
        assert!(
            is_test_symbol(&sym, None),
            "Java symbol in Test file should be identified as test"
        );
    }

    #[test]
    fn test_java_file_in_test_dir_matches() {
        let sym = make_symbol("setUp", "src/test/java/Helper.java", Language::Java);
        assert!(
            is_test_symbol(&sym, None),
            "Java symbol in /test/ directory should be identified as test"
        );
    }

    #[test]
    fn test_java_attribute_test_matches() {
        // Simulate Java source with @Test annotation
        let source = "import org.junit.Test;\n\n@Test\npublic void myMethod() {\n}\n";
        let sym = Symbol {
            name: "myMethod".to_string(),
            kind: SymbolKind::Method,
            file: "src/main/java/App.java".to_string(),
            byte_range: (30, 56),
            line_range: (4, 5),
            language: Language::Java,
            signature: "public void myMethod()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        };
        assert!(
            is_test_symbol(&sym, Some(source)),
            "Java symbol preceded by @Test should be identified as test"
        );
    }

    #[test]
    fn test_java_non_test_symbol_not_matched() {
        let sym = make_symbol("processOrder", "src/main/java/Order.java", Language::Java);
        assert!(
            !is_test_symbol(&sym, None),
            "Regular Java symbol should NOT be identified as test"
        );
    }

    // ---- Scala tests ----

    #[test]
    fn test_scala_file_contains_spec_matches() {
        let sym = make_symbol("helper", "src/test/scala/UserSpec.scala", Language::Scala);
        assert!(
            is_test_symbol(&sym, None),
            "Scala symbol in Spec file should be identified as test"
        );
    }

    #[test]
    fn test_scala_function_prefix_matches() {
        let sym = make_symbol("testSomething", "src/main/scala/App.scala", Language::Scala);
        assert!(
            is_test_symbol(&sym, None),
            "Scala symbol with 'test' prefix should be identified as test"
        );
    }

    #[test]
    fn test_scala_non_test_symbol_not_matched() {
        let sym = make_symbol("processData", "src/main/scala/App.scala", Language::Scala);
        assert!(
            !is_test_symbol(&sym, None),
            "Regular Scala symbol should NOT be identified as test"
        );
    }

    // ---- Languages without configs ----

    #[test]
    fn test_sql_always_returns_false() {
        let sym = make_symbol("test_query", "migrations/test.sql", Language::Sql);
        assert!(
            !is_test_symbol(&sym, None),
            "SQL symbols should never be identified as test (no config)"
        );
    }

    #[test]
    fn test_other_language_always_returns_false() {
        let sym = make_symbol("test_something", "test.txt", Language::Other);
        assert!(
            !is_test_symbol(&sym, None),
            "Other language symbols should never be identified as test (no config)"
        );
    }

    // ---- Python decorator-based matching ----

    #[test]
    fn test_python_decorator_pytest_mark() {
        // Python with @pytest.mark.parametrize should not match unless
        // there's a TestPattern::Attribute pattern for it. Python uses
        // FunctionPrefix("test_"), so this should only match by name.
        let sym = make_symbol_with_decorators(
            "test_parametrized",
            "src/main.py",
            Language::Python,
            vec!["@pytest.mark.parametrize".to_string()],
        );
        assert!(
            is_test_symbol(&sym, None),
            "Python function with test_ prefix should match even without attribute pattern"
        );
    }

    // ---- Attribute matching edge cases ----

    #[test]
    fn test_rust_tokio_test_attribute_matches() {
        let source = "#[tokio::test]\nasync fn my_async_test() {\n}\n";
        let sym = Symbol {
            name: "my_async_test".to_string(),
            kind: SymbolKind::Function,
            file: "src/lib.rs".to_string(),
            byte_range: (15, 46),
            line_range: (2, 3),
            language: Language::Rust,
            signature: "async fn my_async_test()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        };
        assert!(
            is_test_symbol(&sym, Some(source)),
            "Rust symbol preceded by #[tokio::test] should match via ::test pattern"
        );
    }

    #[test]
    fn test_matches_test_pattern_function_prefix() {
        let sym = make_symbol("test_foo", "src/lib.rs", Language::Rust);
        assert!(matches_test_pattern(
            &TestPattern::FunctionPrefix("test"),
            &sym,
            None
        ));
        assert!(!matches_test_pattern(
            &TestPattern::FunctionPrefix("spec_"),
            &sym,
            None
        ));
    }

    #[test]
    fn test_matches_test_pattern_file_contains() {
        let sym = make_symbol("helper", "src/__tests__/foo.ts", Language::TypeScript);
        assert!(matches_test_pattern(
            &TestPattern::FileContains("__tests__"),
            &sym,
            None
        ));
        assert!(!matches_test_pattern(
            &TestPattern::FileContains(".spec."),
            &sym,
            None
        ));
    }

    #[test]
    fn test_matches_test_pattern_file_ends_with() {
        let sym = make_symbol("TestFoo", "pkg/foo_test.go", Language::Go);
        assert!(matches_test_pattern(
            &TestPattern::FileEndsWith("_test.go"),
            &sym,
            None
        ));
        assert!(!matches_test_pattern(
            &TestPattern::FileEndsWith("_test.rs"),
            &sym,
            None
        ));
    }

    #[test]
    fn test_matches_test_pattern_call_expression() {
        let sym = make_symbol("test", "src/app.spec.ts", Language::TypeScript);
        assert!(matches_test_pattern(
            &TestPattern::CallExpression("test"),
            &sym,
            None
        ));
        assert!(!matches_test_pattern(
            &TestPattern::CallExpression("describe"),
            &sym,
            None
        ));
    }

    #[test]
    fn test_matches_test_pattern_attribute_via_decorators() {
        let sym = make_symbol_with_decorators(
            "my_method",
            "src/test.py",
            Language::Python,
            vec!["@pytest.fixture".to_string()],
        );
        assert!(matches_test_pattern(
            &TestPattern::Attribute("pytest.fixture"),
            &sym,
            None
        ));
        assert!(!matches_test_pattern(
            &TestPattern::Attribute("Test"),
            &sym,
            None
        ));
    }

    #[test]
    fn test_matches_test_pattern_attribute_via_source_scan() {
        let source = "    @Test\n    public void foo() {}\n";
        let sym = Symbol {
            name: "foo".to_string(),
            kind: SymbolKind::Method,
            file: "Foo.java".to_string(),
            byte_range: (10, 34),
            line_range: (2, 2),
            language: Language::Java,
            signature: "public void foo()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        };
        assert!(matches_test_pattern(
            &TestPattern::Attribute("Test"),
            &sym,
            Some(source)
        ));
    }

    // ---- Outline tests ----

    #[test]
    fn test_kind_label_returns_correct_labels() {
        assert_eq!(kind_label(SymbolKind::Function), "Functions");
        assert_eq!(kind_label(SymbolKind::Struct), "Structs");
        assert_eq!(kind_label(SymbolKind::Method), "Methods");
        assert_eq!(kind_label(SymbolKind::Class), "Classes");
        assert_eq!(kind_label(SymbolKind::Enum), "Enums");
        assert_eq!(kind_label(SymbolKind::Trait), "Traits");
        assert_eq!(kind_label(SymbolKind::Interface), "Interfaces");
        assert_eq!(kind_label(SymbolKind::Constant), "Constants");
        assert_eq!(kind_label(SymbolKind::Variable), "Variables");
        assert_eq!(kind_label(SymbolKind::Type), "Types");
        assert_eq!(kind_label(SymbolKind::Module), "Modules");
        assert_eq!(kind_label(SymbolKind::Macro), "Macros");
        assert_eq!(kind_label(SymbolKind::Import), "Imports");
        assert_eq!(kind_label(SymbolKind::Other), "Other");
    }

    #[test]
    fn test_kind_order_imports_before_functions() {
        assert!(kind_order(SymbolKind::Import) < kind_order(SymbolKind::Function));
        assert!(kind_order(SymbolKind::Struct) < kind_order(SymbolKind::Function));
        assert!(kind_order(SymbolKind::Function) < kind_order(SymbolKind::Method));
    }

    #[test]
    fn test_generate_outline_file_not_in_index() {
        use crate::index::file_tree::FileTree;
        use crate::symbols::SymbolTable;

        let dir = tempfile::tempdir().unwrap();
        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        let result = generate_outline(
            dir.path(),
            &file_tree,
            &symbol_table,
            "nonexistent.rs",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_generate_outline_basic() {
        use crate::index::file_entry::FileEntry;
        use crate::index::file_tree::FileTree;
        use crate::symbols::SymbolTable;

        let dir = tempfile::tempdir().unwrap();
        let source = "fn hello() {\n    println!(\"Hello\");\n}\n\nstruct Foo {\n    bar: i32,\n}\n";
        std::fs::write(dir.path().join("main.rs"), source).unwrap();

        let file_tree = Arc::new(FileTree::new());
        file_tree.insert(FileEntry::new(
            "main.rs".to_string(),
            source.len() as u64,
            chrono::Utc::now(),
        ));

        let symbol_table = Arc::new(SymbolTable::new());
        symbol_table.insert(Symbol {
            name: "hello".to_string(),
            kind: SymbolKind::Function,
            file: "main.rs".to_string(),
            byte_range: (0, 35),
            line_range: (1, 3),
            language: Language::Rust,
            signature: "fn hello()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        });
        symbol_table.insert(Symbol {
            name: "Foo".to_string(),
            kind: SymbolKind::Struct,
            file: "main.rs".to_string(),
            byte_range: (37, 60),
            line_range: (5, 7),
            language: Language::Rust,
            signature: "struct Foo".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        });

        let outline = generate_outline(
            dir.path(),
            &file_tree,
            &symbol_table,
            "main.rs",
        )
        .unwrap();

        assert_eq!(outline.file, "main.rs");
        assert_eq!(outline.language, Language::Rust);
        assert_eq!(outline.line_count, 7);
        // Should have two groups: Structs and Functions (in that order)
        assert_eq!(outline.groups.len(), 2);
        assert_eq!(outline.groups[0].kind, "Structs");
        assert_eq!(outline.groups[0].symbols.len(), 1);
        assert_eq!(outline.groups[0].symbols[0].name, "Foo");
        assert_eq!(outline.groups[0].symbols[0].line, 5);
        assert_eq!(outline.groups[1].kind, "Functions");
        assert_eq!(outline.groups[1].symbols.len(), 1);
        assert_eq!(outline.groups[1].symbols[0].name, "hello");
        assert_eq!(outline.groups[1].symbols[0].line, 1);
    }

    #[test]
    fn test_generate_outline_empty_file() {
        use crate::index::file_entry::FileEntry;
        use crate::index::file_tree::FileTree;
        use crate::symbols::SymbolTable;

        let dir = tempfile::tempdir().unwrap();
        let source = "// empty file\n";
        std::fs::write(dir.path().join("empty.rs"), source).unwrap();

        let file_tree = Arc::new(FileTree::new());
        file_tree.insert(FileEntry::new(
            "empty.rs".to_string(),
            source.len() as u64,
            chrono::Utc::now(),
        ));

        let symbol_table = Arc::new(SymbolTable::new());

        let outline = generate_outline(
            dir.path(),
            &file_tree,
            &symbol_table,
            "empty.rs",
        )
        .unwrap();

        assert_eq!(outline.file, "empty.rs");
        assert_eq!(outline.line_count, 1);
        assert!(outline.groups.is_empty());
    }

    #[test]
    fn test_generate_outline_preserves_parent() {
        use crate::index::file_entry::FileEntry;
        use crate::index::file_tree::FileTree;
        use crate::symbols::SymbolTable;

        let dir = tempfile::tempdir().unwrap();
        let source = "struct Foo;\nimpl Foo {\n    fn bar(&self) {}\n}\n";
        std::fs::write(dir.path().join("lib.rs"), source).unwrap();

        let file_tree = Arc::new(FileTree::new());
        file_tree.insert(FileEntry::new(
            "lib.rs".to_string(),
            source.len() as u64,
            chrono::Utc::now(),
        ));

        let symbol_table = Arc::new(SymbolTable::new());
        symbol_table.insert(Symbol {
            name: "Foo".to_string(),
            kind: SymbolKind::Struct,
            file: "lib.rs".to_string(),
            byte_range: (0, 11),
            line_range: (1, 1),
            language: Language::Rust,
            signature: "struct Foo".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
            doc_comment: None,
        });
        symbol_table.insert(Symbol {
            name: "bar".to_string(),
            kind: SymbolKind::Method,
            file: "lib.rs".to_string(),
            byte_range: (27, 43),
            line_range: (3, 3),
            language: Language::Rust,
            signature: "fn bar(&self)".to_string(),
            definition: None,
            parent: Some("Foo".to_string()),
            decorators: Vec::new(),
            doc_comment: None,
        });

        let outline = generate_outline(
            dir.path(),
            &file_tree,
            &symbol_table,
            "lib.rs",
        )
        .unwrap();

        // Structs before Methods
        assert_eq!(outline.groups.len(), 2);
        assert_eq!(outline.groups[0].kind, "Structs");
        assert_eq!(outline.groups[1].kind, "Methods");
        assert_eq!(outline.groups[1].symbols[0].parent.as_deref(), Some("Foo"));
    }

    // -----------------------------------------------------------------------
    // contains_word tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_contains_word_exact_match() {
        assert!(contains_word("health", "health"));
    }

    #[test]
    fn test_contains_word_in_function_call() {
        assert!(contains_word("    health_check()", "health_check"));
    }

    #[test]
    fn test_contains_word_not_substring() {
        // "health" should NOT match inside "health_check"
        assert!(!contains_word("health_check()", "health"));
    }

    #[test]
    fn test_contains_word_preceded_by_non_alnum() {
        assert!(contains_word("call(health)", "health"));
        assert!(contains_word("self.health", "health"));
    }

    #[test]
    fn test_contains_word_not_part_of_longer_word() {
        assert!(!contains_word("unhealthy", "health"));
    }

    #[test]
    fn test_contains_word_empty_needle() {
        assert!(!contains_word("anything", ""));
    }

    #[test]
    fn test_contains_word_at_start_of_string() {
        assert!(contains_word("health()", "health"));
    }

    #[test]
    fn test_contains_word_at_end_of_string() {
        assert!(contains_word("call health", "health"));
    }

    #[test]
    fn test_contains_word_multiple_occurrences() {
        // First occurrence is part of longer word, second is standalone
        assert!(contains_word("health_check and health", "health"));
    }

    #[test]
    fn test_find_callers_regex_skips_definition_in_other_file() {
        // Reproduces the false-positive bug: a shell script embeds Python code
        // containing `def diff_drive_command(...)`. The regex fallback should
        // NOT report this definition as a caller.
        let shell_source = r#"#!/bin/bash
# some shell code
cat << 'EOF' > /tmp/drive.py

def diff_drive_command(linear_x, angular_z, wheel_separation=0.24, wheel_radius=0.04):
    """Convert (linear_x m/s, angular_z rad/s) to wheel velocities."""
    left_vel = (linear_x - angular_z * wheel_separation / 2) / wheel_radius
    right_vel = (linear_x + angular_z * wheel_separation / 2) / wheel_radius
    return left_vel, right_vel

result = diff_drive_command(0.5, 0.1)
EOF
"#;
        let callers = find_callers_regex(
            shell_source,
            "setup-runpod.sh",
            "diff_drive_command",
            "src/drive_robot.py", // definition is in a DIFFERENT file
        );

        // Should find the call site but NOT the definition
        assert_eq!(callers.len(), 1, "Expected 1 caller, got: {:?}", callers);
        assert!(
            callers[0].text.contains("result = diff_drive_command"),
            "Expected call site, got: {}",
            callers[0].text
        );
    }

    #[test]
    fn test_is_definition_line_regex_matches_common_patterns() {
        assert!(is_definition_line_regex("def foo(x, y):", "foo"));
        assert!(is_definition_line_regex("  def foo(self):", "foo"));
        assert!(is_definition_line_regex("fn foo() -> i32 {", "foo"));
        assert!(is_definition_line_regex("function foo() {", "foo"));
        assert!(is_definition_line_regex("func foo(a int) {", "foo"));
        assert!(is_definition_line_regex("class foo:", "foo"));
        assert!(is_definition_line_regex("struct foo {", "foo"));
        assert!(is_definition_line_regex("const foo = 42;", "foo"));
        // Should NOT match call sites
        assert!(!is_definition_line_regex("result = foo(1, 2)", "foo"));
        assert!(!is_definition_line_regex("let x = foo();", "foo"));
        assert!(!is_definition_line_regex("print(foo(bar))", "foo"));
    }
}
