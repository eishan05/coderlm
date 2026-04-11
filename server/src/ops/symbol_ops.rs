use std::path::Path;
use std::sync::Arc;

use tree_sitter::StreamingIterator;

use crate::index::file_entry::Language;
use crate::index::file_tree::FileTree;
use crate::symbols::SymbolTable;
use crate::symbols::queries::{self, TestPattern};
use crate::symbols::symbol::{Symbol, SymbolKind};

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

    results.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line_range.0.cmp(&b.line_range.0))
    });
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
            symbol_name,
            start,
            source.len()
        ));
    }

    // Check for ambiguity when no line hint was provided
    let (warning, candidates) = if line.is_none() {
        let matches = symbol_table.find_by_file_and_name(file, symbol_name);
        if matches.len() > 1 {
            let cands: Vec<ImplCandidate> = matches
                .iter()
                .map(|s| ImplCandidate {
                    line: s.line_range.0,
                    parent: s.parent.clone(),
                })
                .collect();
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
///
/// `symbol_name` may be a bare name (`"method"`) or a qualified name
/// (`"ClassName.method"`). When qualified, the method is looked up by its
/// bare name and then filtered to the symbol whose `parent` matches.
///
/// `include_paths` / `exclude_paths` restrict which indexed files are
/// scanned. Each entry is matched as a prefix against the relative path.
pub fn find_callers(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
    symbol_name: &str,
    file: &str,
    limit: usize,
    line: Option<usize>,
    include_paths: Option<&[String]>,
    exclude_paths: Option<&[String]>,
) -> Result<Vec<CallerInfo>, String> {
    // Handle qualified names: "ClassName.method" → look up "method" with
    // parent == "ClassName".
    let (bare_name, qualified_parent) = if let Some(dot_pos) = symbol_name.rfind('.') {
        let class = &symbol_name[..dot_pos];
        let method = &symbol_name[dot_pos + 1..];
        if class.is_empty() || method.is_empty() {
            (symbol_name, None)
        } else {
            (method, Some(class.to_string()))
        }
    } else {
        (symbol_name, None)
    };

    // Verify symbol exists and capture its identity (kind, language, parent
    // class) so caller scanning can scope matches to the right definition
    // rather than just matching by name.
    let mut target = if let Some(ref qp) = qualified_parent {
        // Qualified lookup: find by bare name, filter by parent class.
        // If a line hint is also present, use it for exact disambiguation.
        if let Some(ln) = line {
            symbol_table.get(file, bare_name, Some(ln)).ok_or_else(|| {
                format!(
                    "Symbol '{}.{}' not found in '{}' at line {}",
                    qp, bare_name, file, ln
                )
            })?
        } else {
            let candidates = symbol_table.find_by_file_and_name(file, bare_name);
            let matching: Vec<_> = candidates
                .into_iter()
                .filter(|s| s.parent.as_deref() == Some(qp.as_str()))
                .collect();
            match matching.len() {
                0 => {
                    return Err(format!(
                        "Symbol '{}.{}' not found in '{}'",
                        qp, bare_name, file
                    ));
                }
                1 => matching.into_iter().next().unwrap(),
                n => {
                    let lines: Vec<String> = matching
                        .iter()
                        .map(|s| s.line_range.0.to_string())
                        .collect();
                    return Err(format!(
                        "Symbol '{}.{}' is ambiguous in '{}' ({} matches at lines {}). \
                         Pass line= to disambiguate.",
                        qp,
                        bare_name,
                        file,
                        n,
                        lines.join(", ")
                    ));
                }
            }
        }
    } else {
        symbol_table
            .get(file, bare_name, line)
            .ok_or_else(|| format!("Symbol '{}' not found in '{}'", bare_name, file))?
    };

    // If the caller did not provide a line hint and the file has multiple
    // same-named symbols, `get()` silently picks the first one by line
    // order. Running definition-aware receiver resolution against that
    // arbitrary target would then drop callers belonging to the *other*
    // same-named definitions — a silent regression vs the old name-only
    // behavior. Detect this ambiguity and fall back to name-only matching
    // by clearing `parent` on the working target copy.
    if line.is_none() && qualified_parent.is_none() {
        let same_named = symbol_table.find_by_file_and_name(file, bare_name);
        if same_named.len() > 1 {
            target.parent = None;
        }
    }

    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut callers = Vec::new();

    for entry in file_tree.files.iter() {
        let rel_path = entry.key().clone();
        let language = entry.value().language;

        // Only search code files — data/config/markup files (JSON, YAML,
        // TOML, HTML, CSS, Markdown, etc.) produce false-positive callers.
        if !language.is_code() {
            continue;
        }

        // Apply path filters before reading the file.
        if let Some(includes) = include_paths {
            if !includes.is_empty() && !includes.iter().any(|p| rel_path.starts_with(p.as_str())) {
                continue;
            }
        }
        if let Some(excludes) = exclude_paths {
            if excludes.iter().any(|p| rel_path.starts_with(p.as_str())) {
                continue;
            }
        }

        let abs_path = root.join(&rel_path);

        let source = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let file_callers = if language.has_tree_sitter_support() {
            find_callers_ast(&source, &rel_path, language, &target, symbol_table)
        } else {
            find_callers_regex(&source, &rel_path, bare_name, file)
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

/// Classification of a single method call site relative to a target method.
#[derive(Debug, PartialEq, Eq)]
enum ReceiverResolution {
    /// The receiver was resolved to the target's parent class.
    Exact,
    /// The receiver was resolved to a *different* unambiguously-known class
    /// — drop.
    NoMatch,
    /// The receiver could not be resolved to any specific class. Keep the
    /// match but flag it so the agent knows this is a best-effort result.
    Ambiguous(String),
}

/// Result of classifying a bare identifier name against the project's
/// symbol table for the purpose of "is this a class constructor?".
#[derive(Debug, PartialEq, Eq)]
enum NameClassification {
    /// Exactly one class symbol with this name, and no non-class symbol
    /// shares it. Safe to treat `Name(...)` as a constructor.
    UniqueClass,
    /// Either multiple classes share this name, or a class and a
    /// function/variable share it. `Name(...)` could be any of them, so we
    /// refuse to drop the call site.
    Ambiguous,
    /// No class symbol with this name (e.g. a factory function, local
    /// variable, or something we haven't indexed).
    NotAClass,
}

/// Classify a bare identifier against the project's symbol table.
fn classify_name(symbol_table: &SymbolTable, name: &str) -> NameClassification {
    let Some(keys) = symbol_table.by_name.get(name) else {
        return NameClassification::NotAClass;
    };

    let mut class_count = 0usize;
    let mut has_non_class = false;
    for key in keys.iter() {
        if let Some(sym) = symbol_table.symbols.get(key) {
            let kind = sym.value().kind;
            if matches!(
                kind,
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface
            ) {
                class_count += 1;
            } else {
                has_non_class = true;
            }
        }
    }

    if class_count == 0 {
        NameClassification::NotAClass
    } else if class_count == 1 && !has_non_class {
        NameClassification::UniqueClass
    } else {
        // Multiple classes with the same name, or a class plus a
        // function/variable of the same name — can't attribute `Name(...)`
        // to a single definition from the AST alone.
        NameClassification::Ambiguous
    }
}

/// Resolve a Python call-site receiver to a class name (if possible) and
/// classify the call against a target method's parent class.
///
/// This implements the subset of Python receiver analysis that is visible at
/// the AST level without type inference:
///
/// 1. `ClassName(...).method()` — the receiver is a direct constructor call.
///    Exact match iff `ClassName == target_parent`, no-match iff it is an
///    unambiguously different known class, ambiguous if the identifier is
///    not uniquely a class.
/// 2. `x = ClassName(...); x.method()` — a local variable whose *latest*
///    binding prior to the call is a constructor call. The walk respects
///    the enclosing function's lexical scope and does not descend into
///    nested `def` / `lambda` / comprehensions. Reassignments are tracked
///    so `x = A(); x = make_b(); x.f()` correctly becomes ambiguous.
/// 3. `self`, `cls`, `self.attr`, unknown identifiers, factory calls, or
///    other expressions — classified as `Ambiguous` with a short reason.
///    Callers are returned so that true positives are not silently dropped.
///
/// `symbol_table` is consulted to decide whether an identifier used as a
/// constructor is a unique class in the project — this is what lets us
/// treat `A()` differently from `make_foo()`, and also lets us fall back
/// to ambiguous when a name is shadowed.
fn resolve_python_receiver(
    receiver_node: tree_sitter::Node,
    callee_node: tree_sitter::Node,
    source: &str,
    target_parent: &str,
    symbol_table: &SymbolTable,
) -> ReceiverResolution {
    // Case 1: receiver is a direct call like `ClassName(...)`.
    if receiver_node.kind() == "call" {
        if let Some(fn_node) = receiver_node.child_by_field_name("function") {
            if fn_node.kind() == "identifier" {
                let ident = fn_node.utf8_text(source.as_bytes()).unwrap_or("");
                return match classify_name(symbol_table, ident) {
                    NameClassification::UniqueClass => {
                        if ident == target_parent {
                            ReceiverResolution::Exact
                        } else {
                            ReceiverResolution::NoMatch
                        }
                    }
                    NameClassification::Ambiguous => ReceiverResolution::Ambiguous(format!(
                        "receiver constructor '{}' is ambiguous (multiple classes or \
                         class/non-class name collision)",
                        ident
                    )),
                    NameClassification::NotAClass => ReceiverResolution::Ambiguous(format!(
                        "receiver is a call to '{}' which is not a known class",
                        ident
                    )),
                };
            }
        }
        return ReceiverResolution::Ambiguous(
            "receiver is a call expression with an unresolved callable".to_string(),
        );
    }

    // Case 2: receiver is a bare identifier like `x`.
    if receiver_node.kind() == "identifier" {
        let ident = receiver_node.utf8_text(source.as_bytes()).unwrap_or("");
        if ident == "self" || ident == "cls" {
            return ReceiverResolution::Ambiguous(
                "receiver is self/cls; class-attribute tracking is not implemented".to_string(),
            );
        }
        // Find the latest binding of `ident` in the enclosing function
        // body, occurring lexically before the call site.
        match find_local_binding(callee_node, ident, source, symbol_table) {
            Some(LocalBinding::UniqueClass(class_name)) => {
                if class_name == target_parent {
                    ReceiverResolution::Exact
                } else {
                    ReceiverResolution::NoMatch
                }
            }
            Some(LocalBinding::AmbiguousClass(class_name)) => {
                ReceiverResolution::Ambiguous(format!(
                    "local '{}' was assigned from '{}' but that name is ambiguous \
                     (multiple classes or class/non-class name collision)",
                    ident, class_name
                ))
            }
            Some(LocalBinding::Other) => ReceiverResolution::Ambiguous(format!(
                "local '{}' was reassigned from a non-constructor expression before \
                 this call",
                ident
            )),
            None => ReceiverResolution::Ambiguous(format!(
                "receiver '{}' has no visible local assignment to a known class",
                ident
            )),
        }
    } else if receiver_node.kind() == "attribute" {
        // Case 3: attribute access like `self.attr`, `self.broker.method`.
        ReceiverResolution::Ambiguous(
            "receiver is an attribute access; requires data-flow analysis".to_string(),
        )
    } else {
        // Anything else (subscript, parenthesized expression, etc.) — ambiguous.
        ReceiverResolution::Ambiguous(format!(
            "receiver kind '{}' is not handled",
            receiver_node.kind()
        ))
    }
}

/// Possible outcomes of looking up the latest local binding of an
/// identifier before a call site.
#[derive(Debug, PartialEq, Eq)]
enum LocalBinding {
    /// Latest binding is a constructor call to an unambiguously-unique class.
    UniqueClass(String),
    /// Latest binding is a constructor call, but the class name is
    /// ambiguous (multiple classes or class/non-class name collision).
    AmbiguousClass(String),
    /// Latest binding is not a constructor call (factory, variable,
    /// literal, etc.) — classification unknown.
    Other,
}

/// Walk up from `from_node` until we find the enclosing `function_definition`
/// or `lambda`. Returns that function node, or `None` if the call is at
/// module scope.
fn enclosing_function_node(from_node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cur = from_node.parent();
    while let Some(n) = cur {
        if n.kind() == "function_definition" || n.kind() == "lambda" {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// Find the latest local binding of `ident` in the enclosing function of
/// `call_node`, occurring strictly before `call_node`. Respects lexical
/// scope: does not descend into nested functions, classes, lambdas, or
/// comprehensions.
fn find_local_binding(
    call_node: tree_sitter::Node,
    ident: &str,
    source: &str,
    symbol_table: &SymbolTable,
) -> Option<LocalBinding> {
    // Scope the search to the enclosing function body. If the call is at
    // module scope, fall back to scanning the module root.
    let scope_root = enclosing_function_node(call_node)
        .and_then(|f| f.child_by_field_name("body"))
        .or_else(|| {
            let mut cur = call_node.parent();
            while let Some(n) = cur {
                if n.kind() == "module" {
                    return Some(n);
                }
                cur = n.parent();
            }
            None
        })?;

    let call_start = call_node.start_byte();
    // Track the assignment with the largest start_byte (lexically latest)
    // that binds `ident` and is fully before the call site.
    let mut latest: Option<(usize, LocalBinding)> = None;
    walk_assignments(
        scope_root,
        /*is_scope_root=*/ true,
        ident,
        call_start,
        source,
        symbol_table,
        &mut latest,
    );
    latest.map(|(_, binding)| binding)
}

/// Recursively walk `node`'s descendants looking for assignments that bind
/// `ident`, occurring fully before `cutoff_byte`. Updates `latest` with the
/// lexically latest such assignment.
///
/// Lexical-scope safety: when we descend into a subtree that introduces a
/// new Python scope (nested function, lambda, class body, comprehension),
/// we stop — assignments there bind a *different* `ident` in a different
/// scope and must not affect the caller's scope. The scope-root node itself
/// is always allowed (the function body we started at).
fn walk_assignments(
    node: tree_sitter::Node,
    is_scope_root: bool,
    ident: &str,
    cutoff_byte: usize,
    source: &str,
    symbol_table: &SymbolTable,
    latest: &mut Option<(usize, LocalBinding)>,
) {
    // Skip nested scopes entirely — they introduce their own binding of
    // `ident` which must not leak out into the enclosing function.
    if !is_scope_root
        && matches!(
            node.kind(),
            "function_definition"
                | "class_definition"
                | "lambda"
                | "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression"
        )
    {
        return;
    }

    // If the entire subtree starts at or after the call, nothing in it can
    // precede the call.
    if node.start_byte() >= cutoff_byte {
        return;
    }

    if node.kind() == "assignment" {
        // Only count assignments that have *completed* before the call.
        // `x = A(x.f())` has an assignment that starts before the callee
        // but has not finished when the callee runs — it must not be
        // treated as a prior binding.
        if node.end_byte() <= cutoff_byte {
            let left = node.child_by_field_name("left");
            let right = node.child_by_field_name("right");
            if let (Some(left), Some(right)) = (left, right) {
                let matches_ident = left.kind() == "identifier"
                    && left.utf8_text(source.as_bytes()).unwrap_or("") == ident;
                if matches_ident {
                    let binding = classify_assignment_rhs(right, source, symbol_table);
                    let start = node.start_byte();
                    match latest.as_ref() {
                        Some((prev_start, _)) if *prev_start >= start => {}
                        _ => {
                            *latest = Some((start, binding));
                        }
                    }
                }
            }
        }
    }

    // Recurse into children. We still recurse even when the current node
    // is an assignment whose end_byte > cutoff, because inner statements
    // might legitimately contain completed prior assignments (e.g. within
    // an inner list comprehension's generator, though those would be
    // blocked by the scope check above anyway).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_assignments(
            child,
            /*is_scope_root=*/ false,
            ident,
            cutoff_byte,
            source,
            symbol_table,
            latest,
        );
    }
}

/// Classify the right-hand side of an assignment: is it a known-class
/// constructor call, an ambiguous constructor-like call, or something else?
fn classify_assignment_rhs(
    rhs: tree_sitter::Node,
    source: &str,
    symbol_table: &SymbolTable,
) -> LocalBinding {
    if rhs.kind() == "call" {
        if let Some(fn_node) = rhs.child_by_field_name("function") {
            if fn_node.kind() == "identifier" {
                let cls = fn_node
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .to_string();
                return match classify_name(symbol_table, &cls) {
                    NameClassification::UniqueClass => LocalBinding::UniqueClass(cls),
                    NameClassification::Ambiguous => LocalBinding::AmbiguousClass(cls),
                    NameClassification::NotAClass => LocalBinding::Other,
                };
            }
        }
    }
    LocalBinding::Other
}

/// Extract 2 lines before + the call line + 2 lines after as context.
fn extract_context(source: &str, line_num: usize) -> (String, usize) {
    let lines: Vec<&str> = source.lines().collect();
    let start = if line_num > 2 { line_num - 2 } else { 1 };
    let end = (line_num + 2).min(lines.len());
    let snippet: Vec<&str> = lines[(start - 1)..end].to_vec();
    (snippet.join("\n"), start)
}

/// Walk from `node` up the tree to find the enclosing function and class.
/// Returns `(function_name, class_name)`. Language-agnostic: checks for
/// common tree-sitter node kinds across Python, Rust, TS, Go, Java, Scala.
fn enclosing_function_and_class(
    node: tree_sitter::Node,
    source: &str,
) -> (Option<String>, Option<String>) {
    let mut func_name: Option<String> = None;
    let mut class_name: Option<String> = None;

    let func_kinds = [
        "function_definition",  // Python, Rust
        "function_declaration", // TS/JS, Go
        "method_declaration",   // Java
        "function_item",        // Rust (top-level fn)
        "lambda",               // Python lambda
        "function_definition",  // Scala
    ];
    let class_kinds = [
        "class_definition",  // Python, Scala
        "class_declaration", // Java, TS/JS
        "impl_item",         // Rust
        "struct_item",       // Rust
    ];

    let mut cur = node.parent();
    while let Some(n) = cur {
        let kind = n.kind();
        if func_name.is_none() && func_kinds.contains(&kind) {
            if let Some(name_node) = n.child_by_field_name("name") {
                func_name = name_node
                    .utf8_text(source.as_bytes())
                    .ok()
                    .map(|s| s.to_string());
            }
        }
        if class_name.is_none() && class_kinds.contains(&kind) {
            if let Some(name_node) = n.child_by_field_name("name") {
                class_name = name_node
                    .utf8_text(source.as_bytes())
                    .ok()
                    .map(|s| s.to_string());
            } else if kind == "impl_item" {
                // Rust impl blocks use "type" field instead of "name"
                if let Some(type_node) = n.child_by_field_name("type") {
                    class_name = type_node
                        .utf8_text(source.as_bytes())
                        .ok()
                        .map(|s| s.to_string());
                }
            }
        }
        if func_name.is_some() && class_name.is_some() {
            break;
        }
        cur = n.parent();
    }
    (func_name, class_name)
}

/// AST-aware caller detection: parse the file, run the callers query,
/// and check if any call-expression callee matches the target symbol name.
///
/// For Python methods with a resolved parent class, receivers are analyzed
/// and the match is classified (exact / ambiguous / no-match). For other
/// languages, parentless Python targets, or Python free functions the old
/// name-only behavior is preserved.
fn find_callers_ast(
    source: &str,
    rel_path: &str,
    language: Language,
    target: &Symbol,
    symbol_table: &SymbolTable,
) -> Vec<CallerInfo> {
    let symbol_name = target.name.as_str();
    let definition_file = target.file.as_str();

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

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let callee_idx = capture_names.iter().position(|n| n == "callee");
    let receiver_idx = capture_names.iter().position(|n| n == "receiver");

    // Enable definition-aware resolution only when the target is a Python
    // method with a known parent class.
    let python_method_target = target.language == Language::Python
        && target.kind == SymbolKind::Method
        && target.parent.is_some();

    // Determine call_form for non-method targets: free functions get
    // "function_call", everything else gets None.
    let is_free_function = target.kind == SymbolKind::Function;

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut callers = Vec::new();

    while let Some(m) = matches.next() {
        // Find the callee capture in this match, and the receiver capture
        // if present.
        let mut callee_cap: Option<&tree_sitter::QueryCapture> = None;
        let mut receiver_cap: Option<&tree_sitter::QueryCapture> = None;
        for cap in m.captures {
            if Some(cap.index as usize) == callee_idx {
                callee_cap = Some(cap);
            } else if Some(cap.index as usize) == receiver_idx {
                receiver_cap = Some(cap);
            }
        }

        let cap = match callee_cap {
            Some(c) => c,
            None => continue,
        };

        let text = cap.node.utf8_text(source.as_bytes()).unwrap_or("");
        if text != symbol_name {
            continue;
        }

        let line_num = cap.node.start_position().row + 1;

        // Skip the definition itself (the `def foo` line still contains the
        // identifier `foo`, which can otherwise be captured as a callee).
        if rel_path == definition_file {
            let line_text = source.lines().nth(line_num - 1).unwrap_or("");
            if is_definition_line(line_text, symbol_name, language) {
                continue;
            }
        }

        let line_text = source
            .lines()
            .nth(line_num - 1)
            .map(|l| l.trim().to_string())
            .unwrap_or_default();

        // Decide what resolution metadata, if any, to attach.
        let (resolution, reason, call_form) = if python_method_target {
            let target_parent = target.parent.as_deref().unwrap_or("");
            match receiver_cap {
                Some(rcap) => {
                    match resolve_python_receiver(
                        rcap.node,
                        cap.node,
                        source,
                        target_parent,
                        symbol_table,
                    ) {
                        ReceiverResolution::Exact => (
                            Some("exact".to_string()),
                            None,
                            Some("method_call".to_string()),
                        ),
                        ReceiverResolution::NoMatch => continue,
                        ReceiverResolution::Ambiguous(reason) => (
                            Some("ambiguous".to_string()),
                            Some(reason),
                            Some("method_call".to_string()),
                        ),
                    }
                }
                None => {
                    // A method call should almost always have a receiver. The
                    // only way to reach this branch is a bare `symbol_name()`
                    // call matching the identifier-only pattern. Treat it as
                    // ambiguous rather than silently including it as exact.
                    (
                        Some("ambiguous".to_string()),
                        Some("bare call has no receiver — cannot tie to any class".to_string()),
                        Some("bare_call".to_string()),
                    )
                }
            }
        } else if is_free_function {
            (None, None, Some("function_call".to_string()))
        } else {
            (None, None, None)
        };

        // Enclosing function/class for the call site.
        let (calling_function, calling_class) = enclosing_function_and_class(cap.node, source);

        // Context: a few lines around the call.
        let (context, context_start_line) = extract_context(source, line_num);

        callers.push(CallerInfo {
            file: rel_path.to_string(),
            line: line_num,
            text: line_text,
            resolution,
            reason,
            calling_function,
            calling_class,
            call_form,
            context: Some(context),
            context_start_line: Some(context_start_line),
        });
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

            let one_based = line_num + 1;
            let (context, context_start_line) = extract_context(source, one_based);
            callers.push(CallerInfo {
                file: rel_path.to_string(),
                line: one_based,
                text: line.trim().to_string(),
                resolution: None,
                reason: None,
                calling_function: None,
                calling_class: None,
                call_form: None,
                context: Some(context),
                context_start_line: Some(context_start_line),
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
            line.contains(&format!("function {}", name)) || line.contains(&format!("{} =", name))
        }
        Language::Go => line.contains(&format!("func {}", name)),
        Language::Java => {
            line.contains(&format!("class {}", name))
                || line.contains(&format!("interface {}", name))
                || line.contains(&format!("enum {}", name))
                || (line.contains(name)
                    && (line.contains("void ")
                        || line.contains("int ")
                        || line.contains("String ")
                        || line.contains("boolean ")
                        || line.contains("long ")
                        || line.contains("double ")
                        || line.contains("float ")
                        || line.contains("public ")
                        || line.contains("private ")
                        || line.contains("protected ")))
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallerInfo {
    pub file: String,
    pub line: usize,
    pub text: String,
    /// Caller-resolution classification. Currently only populated for Python
    /// method callers where the receiver expression can (or cannot) be tied
    /// back to the target method's containing class.
    ///
    /// - `"exact"`: the receiver was resolved to the target's parent class.
    /// - `"ambiguous"`: the receiver could not be resolved (e.g. `self.attr`,
    ///   an unknown variable, or a factory call), but the call *might* target
    ///   this method. Returned so callers are not silently dropped.
    ///
    /// `None` means resolution is not applicable (non-Python, free function,
    /// or the target symbol has no recorded parent class).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Short human-readable explanation of why the resolution was ambiguous.
    /// `None` for exact matches and when resolution is not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Name of the function/method enclosing this call site.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calling_function: Option<String>,
    /// Name of the class enclosing this call site (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calling_class: Option<String>,
    /// Shape of the call expression:
    /// - `"method_call"`: receiver-qualified call (`obj.method()`)
    /// - `"bare_call"`: unqualified call to a method-named symbol (`method()`)
    /// - `"function_call"`: call to a free function target
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_form: Option<String>,
    /// A few lines of source around the call site for immediate context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// The 1-indexed line number where `context` starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_start_line: Option<usize>,
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
            function_name,
            start,
            source.len()
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

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
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
                if !text.is_empty()
                    && text != "self"
                    && text != "_"
                    && seen.insert(text.to_string())
                {
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
fn list_variables_regex(body: &str, language: Language, function_name: &str) -> Vec<VariableInfo> {
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

        let result = generate_outline(dir.path(), &file_tree, &symbol_table, "nonexistent.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_generate_outline_basic() {
        use crate::index::file_entry::FileEntry;
        use crate::index::file_tree::FileTree;
        use crate::symbols::SymbolTable;

        let dir = tempfile::tempdir().unwrap();
        let source =
            "fn hello() {\n    println!(\"Hello\");\n}\n\nstruct Foo {\n    bar: i32,\n}\n";
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

        let outline = generate_outline(dir.path(), &file_tree, &symbol_table, "main.rs").unwrap();

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

        let outline = generate_outline(dir.path(), &file_tree, &symbol_table, "empty.rs").unwrap();

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

        let outline = generate_outline(dir.path(), &file_tree, &symbol_table, "lib.rs").unwrap();

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

    // ── Python method caller resolution (end-to-end) ──────────────────────
    //
    // These tests index a small temp project on disk, then exercise
    // `find_callers` through the same path used by the HTTP API. This
    // verifies that:
    // - Python methods are indexed with a `parent` class.
    // - `find_callers` uses the resolved symbol's identity (file + line) to
    //   distinguish same-named methods on different classes.
    // - Receiver-based resolution correctly includes/excludes call sites.
    // - Ambiguous receivers are surfaced with a `resolution: "ambiguous"`
    //   flag instead of being silently presented as exact matches.

    use crate::index::file_tree::FileTree;
    use crate::symbols::SymbolTable;
    use crate::symbols::parser;
    use tempfile::TempDir;

    /// Index a temp directory containing Python files and return the
    /// root path, file tree, and symbol table ready for `find_callers`.
    fn index_python_project(files: &[(&str, &str)]) -> (TempDir, Arc<FileTree>, Arc<SymbolTable>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let file_path = dir.path().join(name);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&file_path, content).unwrap();
        }

        let file_tree = Arc::new(FileTree::new());
        crate::index::walker::scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        let symbol_table = Arc::new(SymbolTable::new());
        let paths: Vec<(String, Language)> = file_tree
            .files
            .iter()
            .filter(|e| e.value().language.has_tree_sitter_support())
            .map(|e| (e.key().clone(), e.value().language))
            .collect();
        for (rel_path, language) in paths {
            if let Ok(symbols) = parser::extract_symbols_from_file(dir.path(), &rel_path, language)
            {
                for sym in symbols {
                    symbol_table.insert(sym);
                }
            }
        }

        (dir, file_tree, symbol_table)
    }

    /// Helper: find a Python method by name + parent class name so tests
    /// can unambiguously reference `A.f` vs `B.f`.
    fn find_method(symbol_table: &SymbolTable, file: &str, name: &str, parent: &str) -> Symbol {
        symbol_table
            .find_by_file_and_name(file, name)
            .into_iter()
            .find(|s| s.parent.as_deref() == Some(parent))
            .unwrap_or_else(|| panic!("Method {}.{} not found in {}", parent, name, file))
    }

    #[test]
    fn test_find_callers_distinguishes_a_f_and_b_f() {
        let source = r#"
class A:
    def f(self):
        pass

class B:
    def f(self):
        pass

A().f()
B().f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let b_f = find_method(&symbol_table, "main.py", "f", "B");

        // Callers of A.f should only include A().f(), not B().f().
        let a_callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            a_callers.len(),
            1,
            "Expected 1 caller of A.f, got {:?}",
            a_callers
        );
        assert!(
            a_callers[0].text.contains("A().f()"),
            "Expected A().f() call site, got '{}'",
            a_callers[0].text
        );
        assert_eq!(
            a_callers[0].resolution.as_deref(),
            Some("exact"),
            "A().f() should be an exact match"
        );

        // Callers of B.f should only include B().f().
        let b_callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &b_f.name,
            &b_f.file,
            50,
            Some(b_f.line_range.0),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            b_callers.len(),
            1,
            "Expected 1 caller of B.f, got {:?}",
            b_callers
        );
        assert!(
            b_callers[0].text.contains("B().f()"),
            "Expected B().f() call site, got '{}'",
            b_callers[0].text
        );
        assert_eq!(b_callers[0].resolution.as_deref(), Some("exact"));
    }

    #[test]
    fn test_find_callers_resolves_local_variable_constructor() {
        let source = r#"
class A:
    def f(self):
        pass

class B:
    def f(self):
        pass

def use_a():
    a = A()
    a.f()

def use_b():
    b = B()
    b.f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();

        // Only `a.f()` from use_a should match; `b.f()` in use_b must be
        // resolved to B and dropped.
        let exact: Vec<&CallerInfo> = callers
            .iter()
            .filter(|c| c.resolution.as_deref() == Some("exact"))
            .collect();
        assert_eq!(
            exact.len(),
            1,
            "Expected 1 exact caller for A.f, got {:?}",
            callers
        );
        assert!(
            exact[0].text.contains("a.f()"),
            "Expected 'a.f()' as exact caller, got '{}'",
            exact[0].text
        );

        // And none of the returned callers should reference `b.f()`.
        for c in &callers {
            assert!(
                !c.text.contains("b.f()"),
                "A.f callers should not include b.f() from use_b, got {:?}",
                c
            );
        }
    }

    #[test]
    fn test_find_callers_unknown_receiver_marked_ambiguous() {
        let source = r#"
class A:
    def f(self):
        pass

def handle(x):
    x.f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();

        // x.f() can't be tied to A or any other class from parameter info
        // alone. It must be returned with resolution=ambiguous rather than
        // silently dropped or promoted to exact.
        assert_eq!(callers.len(), 1, "Expected 1 caller, got {:?}", callers);
        assert!(
            callers[0].text.contains("x.f()"),
            "Expected 'x.f()' call site, got '{}'",
            callers[0].text
        );
        assert_eq!(
            callers[0].resolution.as_deref(),
            Some("ambiguous"),
            "Unknown receiver should be marked ambiguous"
        );
        assert!(
            callers[0].reason.is_some(),
            "Ambiguous callers should include a reason"
        );
    }

    #[test]
    fn test_find_callers_self_receiver_marked_ambiguous() {
        // A regression-style fixture modeled after the xanbot broker case:
        // `self._broker.place_order(...)` cannot be resolved without
        // data-flow / type analysis, so it must be returned as ambiguous
        // for all same-named methods (one per broker implementation).
        let source = r#"
class LiveBroker:
    def place_order(self, ticker):
        pass

class DryRunBroker:
    def place_order(self, ticker):
        pass

class Strategy:
    def __init__(self, broker):
        self._broker = broker

    def run(self, ticker):
        self._broker.place_order(ticker)
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let live = find_method(&symbol_table, "main.py", "place_order", "LiveBroker");
        let dry = find_method(&symbol_table, "main.py", "place_order", "DryRunBroker");

        // Both broker.place_order targets should surface the same ambiguous
        // self._broker.place_order site — they are honestly ambiguous.
        for target in &[live, dry] {
            let callers = find_callers(
                dir.path(),
                &file_tree,
                &symbol_table,
                &target.name,
                &target.file,
                50,
                Some(target.line_range.0),
                None,
                None,
            )
            .unwrap();
            assert_eq!(
                callers.len(),
                1,
                "Expected 1 ambiguous caller for {}.place_order, got {:?}",
                target.parent.as_deref().unwrap_or(""),
                callers
            );
            assert_eq!(
                callers[0].resolution.as_deref(),
                Some("ambiguous"),
                "self._broker.place_order must be classified as ambiguous, not exact"
            );
        }
    }

    #[test]
    fn test_find_callers_no_line_ambiguous_falls_back_to_name_only() {
        // Regression for codex finding #1: when the caller omits `line`
        // and the file has multiple same-named methods, `get()` silently
        // picks the first one. Running definition-aware filtering against
        // that arbitrary target would drop the *other* same-named methods'
        // callers. Instead, find_callers must detect the ambiguity and
        // fall back to name-only matching so both `A().f()` and `B().f()`
        // are returned.
        let source = r#"
class A:
    def f(self):
        pass

class B:
    def f(self):
        pass

A().f()
B().f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        // No line hint — the caller doesn't know which f to pick.
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            "f",
            "main.py",
            50,
            None,
            None,
            None,
        )
        .unwrap();

        // Both A().f() and B().f() must be returned.
        assert_eq!(
            callers.len(),
            2,
            "Expected 2 callers in name-only fallback, got {:?}",
            callers
        );
        assert!(
            callers.iter().any(|c| c.text.contains("A().f()")),
            "Expected A().f() in results"
        );
        assert!(
            callers.iter().any(|c| c.text.contains("B().f()")),
            "Expected B().f() in results"
        );
        // Fallback mode: no resolution metadata should be attached.
        for c in &callers {
            assert!(
                c.resolution.is_none(),
                "Fallback mode should not emit resolution metadata: {:?}",
                c
            );
        }
    }

    #[test]
    fn test_find_callers_reassignment_downgrades_to_ambiguous() {
        // Regression for codex finding #2: the walker must track the
        // *latest* binding to `x`, not just the latest known-class
        // binding. `x = A(); x = make_b(); x.f()` must not be treated as
        // an exact A.f call.
        let source = r#"
class A:
    def f(self):
        pass

def make_b():
    return None

def use():
    x = A()
    x = make_b()
    x.f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();

        // The single x.f() call site must be included, but classified as
        // ambiguous — never as exact — because the latest binding is
        // unknown.
        assert_eq!(callers.len(), 1, "Expected 1 caller, got {:?}", callers);
        assert_eq!(
            callers[0].resolution.as_deref(),
            Some("ambiguous"),
            "Reassigned variable should be ambiguous, not exact"
        );
    }

    #[test]
    fn test_find_callers_nested_scope_does_not_leak_bindings() {
        // Regression for codex finding #3: the walker must not descend
        // into nested `def` / `lambda` / `class`. If an outer `x = A()`
        // sits lexically next to a `def inner(): x = B()`, then a later
        // `x.f()` at outer scope should still resolve to A, not B.
        let source = r#"
class A:
    def f(self):
        pass

class B:
    def f(self):
        pass

def use():
    x = A()
    def inner():
        x = B()
        return x
    x.f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();

        // The outer x.f() must be an exact A.f match.
        let exact: Vec<&CallerInfo> = callers
            .iter()
            .filter(|c| c.resolution.as_deref() == Some("exact"))
            .collect();
        assert_eq!(
            exact.len(),
            1,
            "Expected 1 exact caller for A.f, got {:?}",
            callers
        );
        assert!(
            exact[0].text.contains("x.f()"),
            "Expected outer x.f() to be exact, got '{}'",
            exact[0].text
        );

        // And B.f must NOT pick up the outer x.f() via the inner `x = B()`.
        let b_f = find_method(&symbol_table, "main.py", "f", "B");
        let b_callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &b_f.name,
            &b_f.file,
            50,
            Some(b_f.line_range.0),
            None,
            None,
        )
        .unwrap();
        let b_exact: Vec<&CallerInfo> = b_callers
            .iter()
            .filter(|c| c.resolution.as_deref() == Some("exact"))
            .collect();
        assert!(
            b_exact.is_empty(),
            "Nested `x = B()` must not leak to outer x.f(); got exact callers {:?}",
            b_exact
        );
    }

    #[test]
    fn test_find_callers_in_progress_assignment_rhs() {
        // Regression for codex finding #4: `x = A(x.f())` — the outer
        // assignment starts before the inner `x.f()` callee but has not
        // completed. `x` at the moment of the inner call is whatever `x`
        // was *before* this statement (None here, since nothing set it).
        // The walker must not count the incomplete `x = A(...)` as a
        // prior binding.
        let source = r#"
class A:
    def f(self):
        pass

def use(x):
    x = A(x.f())
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();

        // The inner x.f() must NOT be classified exact — no completed
        // prior binding exists.
        assert_eq!(callers.len(), 1, "Expected 1 caller, got {:?}", callers);
        assert_eq!(
            callers[0].resolution.as_deref(),
            Some("ambiguous"),
            "In-progress assignment should not count as a prior binding"
        );
    }

    #[test]
    fn test_find_callers_shared_class_function_name_is_ambiguous() {
        // Regression for codex finding #5: if a class and a function
        // share a name, `Name(...)` could refer to either. The resolver
        // must not drop `Name().f()` as a NoMatch for other classes.
        let source = r#"
class A:
    def f(self):
        pass

class Foo:
    def f(self):
        pass

def Foo():
    return None

Foo().f()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        // Target: A.f. We should not NoMatch Foo().f() based on the
        // name shadow — it must be surfaced as ambiguous.
        let a_f = find_method(&symbol_table, "main.py", "f", "A");
        let a_callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &a_f.name,
            &a_f.file,
            50,
            Some(a_f.line_range.0),
            None,
            None,
        )
        .unwrap();

        let ambiguous: Vec<&CallerInfo> = a_callers
            .iter()
            .filter(|c| c.resolution.as_deref() == Some("ambiguous"))
            .collect();
        assert!(
            !ambiguous.is_empty(),
            "Expected Foo().f() to be surfaced as ambiguous for A.f target, got {:?}",
            a_callers
        );
        assert!(
            ambiguous.iter().any(|c| c.text.contains("Foo().f()")),
            "Expected Foo().f() among ambiguous callers: {:?}",
            ambiguous
        );
    }

    #[test]
    fn test_find_callers_duplicate_class_name_is_ambiguous() {
        // Regression for codex finding #5: two modules define a class
        // named `Inner`. Both `Outer1.Inner.f` and `Outer2.Inner.f` end
        // up with parent="Inner" in the current representation. An
        // `Inner().f()` call site cannot be attributed to either, so it
        // must be returned as ambiguous — not as a false exact for one
        // of them.
        let file_a = r#"
class Outer1:
    class Inner:
        def f(self):
            pass
"#;
        let file_b = r#"
class Outer2:
    class Inner:
        def f(self):
            pass
"#;
        let call_site = r#"
from a import Outer1
from b import Outer2

Outer1.Inner().f()
"#;
        let (dir, file_tree, symbol_table) =
            index_python_project(&[("a.py", file_a), ("b.py", file_b), ("main.py", call_site)]);

        let inner_a = find_method(&symbol_table, "a.py", "f", "Inner");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &inner_a.name,
            &inner_a.file,
            50,
            Some(inner_a.line_range.0),
            None,
            None,
        )
        .unwrap();

        // The `Outer1.Inner().f()` call is an attribute-receiver
        // (`Outer1.Inner()` → attribute access that's called) — it is
        // correctly ambiguous at the AST level. We only assert that the
        // receiver scanner did not silently promote it to exact or drop
        // it as no-match.
        for c in &callers {
            assert_ne!(
                c.resolution.as_deref(),
                Some("exact"),
                "Must not claim exact match for ambiguous nested-class name: {:?}",
                c
            );
        }
    }

    #[test]
    fn test_find_callers_xanbot_broker_distinguishes_implementations() {
        // Regression for the original xanbot benchmark: three brokers
        // each with their own `place_order`, plus a usage site that
        // constructs each one and calls its method. Before the parser
        // class-symbol fix, none of these classes were emitted as Class
        // symbols (because they had `@property` methods), so the
        // constructor receiver `LiveBroker()` did not match any known
        // class and the resolver downgraded everything to ambiguous.
        //
        // After the fix:
        //  - LiveBroker.place_order should resolve only `live.place_order()`
        //    as exact.
        //  - DryRunBroker.place_order should resolve only `dry.place_order()`
        //    as exact.
        //  - PaperBroker.place_order should resolve only `paper.place_order()`
        //    as exact.
        //  - The three definitions must NOT all return the same caller list.
        let source = r#"
class LiveBroker:
    @property
    def order_count(self) -> int:
        return 0

    def place_order(self, ticker):
        pass

class DryRunBroker:
    @property
    def order_count(self) -> int:
        return 0

    def place_order(self, ticker):
        pass

class PaperBroker:
    @property
    def order_count(self) -> int:
        return 0

    def place_order(self, ticker):
        pass

def use_all():
    live = LiveBroker()
    dry = DryRunBroker()
    paper = PaperBroker()
    live.place_order("BTC")
    dry.place_order("BTC")
    paper.place_order("BTC")
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("brokers.py", source)]);

        // Sanity: all three classes must have been indexed.
        for cls in &["LiveBroker", "DryRunBroker", "PaperBroker"] {
            let found = symbol_table
                .all_symbols()
                .iter()
                .any(|s| s.kind == SymbolKind::Class && s.name == *cls);
            assert!(found, "Expected Class symbol '{}'", cls);
        }

        let live_po = find_method(&symbol_table, "brokers.py", "place_order", "LiveBroker");
        let dry_po = find_method(&symbol_table, "brokers.py", "place_order", "DryRunBroker");
        let paper_po = find_method(&symbol_table, "brokers.py", "place_order", "PaperBroker");

        let lookup = |target: &Symbol| {
            find_callers(
                dir.path(),
                &file_tree,
                &symbol_table,
                &target.name,
                &target.file,
                50,
                Some(target.line_range.0),
                None,
                None,
            )
            .unwrap()
        };

        let live_callers = lookup(&live_po);
        let dry_callers = lookup(&dry_po);
        let paper_callers = lookup(&paper_po);

        // Each must have exactly 1 exact caller, pointing at its own
        // local-variable invocation.
        let exact = |callers: &[CallerInfo]| -> Vec<CallerInfo> {
            callers
                .iter()
                .filter(|c| c.resolution.as_deref() == Some("exact"))
                .cloned()
                .collect()
        };
        let live_exact = exact(&live_callers);
        let dry_exact = exact(&dry_callers);
        let paper_exact = exact(&paper_callers);

        assert_eq!(
            live_exact.len(),
            1,
            "LiveBroker.place_order should have 1 exact caller, got {:?}",
            live_callers
        );
        assert!(
            live_exact[0].text.contains("live.place_order"),
            "LiveBroker exact caller should be live.place_order, got '{}'",
            live_exact[0].text
        );

        assert_eq!(
            dry_exact.len(),
            1,
            "DryRunBroker.place_order should have 1 exact caller, got {:?}",
            dry_callers
        );
        assert!(
            dry_exact[0].text.contains("dry.place_order"),
            "DryRunBroker exact caller should be dry.place_order, got '{}'",
            dry_exact[0].text
        );

        assert_eq!(
            paper_exact.len(),
            1,
            "PaperBroker.place_order should have 1 exact caller, got {:?}",
            paper_callers
        );
        assert!(
            paper_exact[0].text.contains("paper.place_order"),
            "PaperBroker exact caller should be paper.place_order, got '{}'",
            paper_exact[0].text
        );

        // The three definitions must NOT return identical caller lists.
        // Compare just the exact-match texts.
        let live_text = live_exact[0].text.clone();
        let dry_text = dry_exact[0].text.clone();
        let paper_text = paper_exact[0].text.clone();
        assert_ne!(
            live_text, dry_text,
            "Live and Dry must return different exact callers"
        );
        assert_ne!(
            dry_text, paper_text,
            "Dry and Paper must return different exact callers"
        );
        assert_ne!(
            live_text, paper_text,
            "Live and Paper must return different exact callers"
        );
    }

    #[test]
    fn test_find_callers_broker_local_constructor_resolves_exactly() {
        // Minimal version of the xanbot user-reported case:
        //   broker = DryRunBroker()
        //   oid = broker.place_order(...)
        // This must be an exact caller of DryRunBroker.place_order.
        let source = r#"
class LiveBroker:
    @property
    def name(self) -> str:
        return "live"

    def place_order(self, ticker):
        pass

class DryRunBroker:
    @property
    def name(self) -> str:
        return "dry"

    def place_order(self, ticker):
        pass

def test_dry_run_e2e():
    broker = DryRunBroker()
    oid = broker.place_order("token-123")
    return oid
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("test_dry_run.py", source)]);

        let dry_po = find_method(
            &symbol_table,
            "test_dry_run.py",
            "place_order",
            "DryRunBroker",
        );
        let dry_callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &dry_po.name,
            &dry_po.file,
            50,
            Some(dry_po.line_range.0),
            None,
            None,
        )
        .unwrap();

        let exact: Vec<&CallerInfo> = dry_callers
            .iter()
            .filter(|c| c.resolution.as_deref() == Some("exact"))
            .collect();
        assert_eq!(
            exact.len(),
            1,
            "Expected 1 exact caller for DryRunBroker.place_order, got {:?}",
            dry_callers
        );
        assert!(
            exact[0].text.contains("broker.place_order"),
            "Expected broker.place_order to be exact, got '{}'",
            exact[0].text
        );

        // LiveBroker.place_order should NOT pick this up — `broker` is
        // unambiguously bound from `DryRunBroker()`.
        let live_po = find_method(
            &symbol_table,
            "test_dry_run.py",
            "place_order",
            "LiveBroker",
        );
        let live_callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &live_po.name,
            &live_po.file,
            50,
            Some(live_po.line_range.0),
            None,
            None,
        )
        .unwrap();
        let live_exact: Vec<&CallerInfo> = live_callers
            .iter()
            .filter(|c| c.resolution.as_deref() == Some("exact"))
            .collect();
        assert!(
            live_exact.is_empty(),
            "LiveBroker.place_order should have NO exact callers in this fixture, got {:?}",
            live_exact
        );
    }

    #[test]
    fn test_find_callers_non_method_target_unchanged() {
        // Python free functions should not grow resolution metadata — the
        // old name-only behavior must still apply so non-method callers
        // are unaffected.
        let source = r#"
def helper(x):
    return x

def main():
    helper(1)
    helper(2)
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let helper = symbol_table
            .find_by_file_and_name("main.py", "helper")
            .into_iter()
            .next()
            .expect("Expected helper function");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &helper.name,
            &helper.file,
            50,
            Some(helper.line_range.0),
            None,
            None,
        )
        .unwrap();

        assert_eq!(callers.len(), 2, "Expected 2 callers of helper");
        for c in &callers {
            assert!(
                c.resolution.is_none(),
                "Free-function callers should not carry resolution metadata: {:?}",
                c
            );
        }
    }

    // ── Qualified name support ──────────────────────────────────────────

    #[test]
    fn test_find_callers_qualified_name() {
        let source = r#"
class Foo:
    def run(self):
        pass

class Bar:
    def run(self):
        pass

Foo().run()
Bar().run()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        // Using "Foo.run" should find only Foo().run()
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            "Foo.run",
            "main.py",
            50,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            callers.len(),
            1,
            "Expected 1 caller for Foo.run, got {:?}",
            callers
        );
        assert!(callers[0].text.contains("Foo().run()"));
        assert_eq!(callers[0].resolution.as_deref(), Some("exact"));

        // Using "Bar.run" should find only Bar().run()
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            "Bar.run",
            "main.py",
            50,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            callers.len(),
            1,
            "Expected 1 caller for Bar.run, got {:?}",
            callers
        );
        assert!(callers[0].text.contains("Bar().run()"));
    }

    #[test]
    fn test_find_callers_qualified_name_not_found() {
        let source = r#"
class Foo:
    def run(self):
        pass
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let result = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            "NoSuchClass.run",
            "main.py",
            50,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_find_callers_qualified_name_with_line_hint() {
        let source = r#"
class Foo:
    def run(self):
        pass

class Bar:
    def run(self):
        pass

Foo().run()
Bar().run()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let foo_run = find_method(&symbol_table, "main.py", "run", "Foo");

        // Qualified name + line hint should also work
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            "Foo.run",
            "main.py",
            50,
            Some(foo_run.line_range.0),
            None,
            None,
        )
        .unwrap();
        assert_eq!(callers.len(), 1);
        assert!(callers[0].text.contains("Foo().run()"));
    }

    // ── Path filter support ─────────────────────────────────────────────

    #[test]
    fn test_find_callers_include_paths() {
        let source_lib = r#"
class Widget:
    def draw(self):
        pass
"#;
        let source_a = r#"
from lib import Widget
Widget().draw()
"#;
        let source_b = r#"
from lib import Widget
Widget().draw()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[
            ("lib.py", source_lib),
            ("src/a.py", source_a),
            ("tests/b.py", source_b),
        ]);

        let draw = find_method(&symbol_table, "lib.py", "draw", "Widget");

        // Only include src/ — should exclude tests/b.py
        let includes = vec!["src/".to_string()];
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &draw.name,
            &draw.file,
            50,
            Some(draw.line_range.0),
            Some(&includes),
            None,
        )
        .unwrap();
        assert!(
            callers.iter().all(|c| c.file.starts_with("src/")),
            "All callers should be in src/, got {:?}",
            callers.iter().map(|c| &c.file).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_find_callers_exclude_paths() {
        let source_lib = r#"
class Widget:
    def draw(self):
        pass
"#;
        let source_a = r#"
from lib import Widget
Widget().draw()
"#;
        let source_b = r#"
from lib import Widget
Widget().draw()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[
            ("lib.py", source_lib),
            ("src/a.py", source_a),
            ("tests/b.py", source_b),
        ]);

        let draw = find_method(&symbol_table, "lib.py", "draw", "Widget");

        // Exclude tests/ — should only find callers outside tests/
        let excludes = vec!["tests/".to_string()];
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &draw.name,
            &draw.file,
            50,
            Some(draw.line_range.0),
            None,
            Some(&excludes),
        )
        .unwrap();
        assert!(
            callers.iter().all(|c| !c.file.starts_with("tests/")),
            "No callers should be in tests/, got {:?}",
            callers.iter().map(|c| &c.file).collect::<Vec<_>>()
        );
    }

    // ── Enclosing caller (calling_function / calling_class) ─────────────

    #[test]
    fn test_find_callers_calling_function_and_class() {
        let source = r#"
class Target:
    def do_work(self):
        pass

class Caller:
    def invoke(self):
        Target().do_work()

def standalone():
    Target().do_work()

Target().do_work()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let do_work = find_method(&symbol_table, "main.py", "do_work", "Target");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &do_work.name,
            &do_work.file,
            50,
            Some(do_work.line_range.0),
            None,
            None,
        )
        .unwrap();

        assert!(
            callers.len() >= 2,
            "Expected at least 2 callers, got {:?}",
            callers
        );

        // The caller inside Caller.invoke should have both calling_function and calling_class
        let method_caller = callers
            .iter()
            .find(|c| c.calling_function.as_deref() == Some("invoke"))
            .expect("Expected a caller from Caller.invoke");
        assert_eq!(method_caller.calling_class.as_deref(), Some("Caller"));

        // The caller inside standalone() should have calling_function but no calling_class
        let fn_caller = callers
            .iter()
            .find(|c| c.calling_function.as_deref() == Some("standalone"))
            .expect("Expected a caller from standalone()");
        assert!(fn_caller.calling_class.is_none());
    }

    // ── Call form classification ─────────────────────────────────────────

    #[test]
    fn test_find_callers_call_form_method_vs_bare() {
        let source = r#"
class Service:
    def process(self):
        pass

def process():
    """A free function with the same name."""
    pass

Service().process()
process()
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let method = find_method(&symbol_table, "main.py", "process", "Service");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &method.name,
            &method.file,
            50,
            Some(method.line_range.0),
            None,
            None,
        )
        .unwrap();

        // Service().process() should be method_call
        let method_call = callers
            .iter()
            .find(|c| c.text.contains("Service().process()"));
        assert!(method_call.is_some(), "Expected method call site");
        assert_eq!(
            method_call.unwrap().call_form.as_deref(),
            Some("method_call")
        );

        // process() (bare) should be bare_call
        let bare_call = callers
            .iter()
            .find(|c| c.call_form.as_deref() == Some("bare_call"));
        assert!(
            bare_call.is_some(),
            "Expected bare call site, got {:?}",
            callers
        );
    }

    #[test]
    fn test_find_callers_call_form_function_call() {
        let source = r#"
def helper(x):
    return x

def main():
    helper(1)
    helper(2)
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let helper = symbol_table
            .find_by_file_and_name("main.py", "helper")
            .into_iter()
            .next()
            .expect("Expected helper function");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &helper.name,
            &helper.file,
            50,
            Some(helper.line_range.0),
            None,
            None,
        )
        .unwrap();

        assert_eq!(callers.len(), 2);
        for c in &callers {
            assert_eq!(
                c.call_form.as_deref(),
                Some("function_call"),
                "Free function callers should have call_form='function_call': {:?}",
                c
            );
        }
    }

    // ── Context lines ───────────────────────────────────────────────────

    #[test]
    fn test_find_callers_context_populated() {
        let source = r#"
class Svc:
    def act(self):
        pass

def caller():
    s = Svc()
    s.act()
    print("done")
"#;
        let (dir, file_tree, symbol_table) = index_python_project(&[("main.py", source)]);

        let act = find_method(&symbol_table, "main.py", "act", "Svc");
        let callers = find_callers(
            dir.path(),
            &file_tree,
            &symbol_table,
            &act.name,
            &act.file,
            50,
            Some(act.line_range.0),
            None,
            None,
        )
        .unwrap();

        assert!(!callers.is_empty(), "Expected at least 1 caller");
        let c = &callers[0];
        assert!(c.context.is_some(), "context should be populated");
        assert!(
            c.context_start_line.is_some(),
            "context_start_line should be populated"
        );
        let ctx = c.context.as_ref().unwrap();
        // Context should include the call line
        assert!(
            ctx.contains("s.act()"),
            "Context should contain the call: {}",
            ctx
        );
        // Context should be multi-line (at least 3 lines)
        assert!(
            ctx.lines().count() >= 3,
            "Context should be at least 3 lines, got: {}",
            ctx
        );
    }

    #[test]
    fn test_extract_context_boundaries() {
        // Line 1 — context should not go below line 1
        let source = "first\nsecond\nthird\nfourth\nfifth";
        let (ctx, start) = extract_context(source, 1);
        assert_eq!(start, 1);
        assert!(ctx.contains("first"));
        assert!(ctx.contains("third")); // 2 lines after

        // Last line — context should not go past end
        let (ctx, start) = extract_context(source, 5);
        assert_eq!(start, 3); // 2 lines before line 5
        assert!(ctx.contains("fifth"));
        assert!(ctx.contains("third"));
    }
}
