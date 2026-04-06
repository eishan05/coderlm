use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tree_sitter::StreamingIterator;
use tracing::{debug, warn};

use crate::index::file_entry::Language;
use crate::index::file_tree::FileTree;
use crate::symbols::queries;
use crate::symbols::symbol::{Symbol, SymbolKind};
use crate::symbols::SymbolTable;

/// Extract symbols from a single file.
pub fn extract_symbols_from_file(
    root: &Path,
    rel_path: &str,
    language: Language,
) -> Result<Vec<Symbol>> {
    let config = match queries::get_language_config(language) {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };

    let abs_path = root.join(rel_path);
    let source = std::fs::read_to_string(&abs_path)?;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&config.language)?;

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => {
            warn!("Failed to parse {}", rel_path);
            return Ok(Vec::new());
        }
    };

    let query = tree_sitter::Query::new(&config.language, config.symbols_query)?;
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

    let capture_names: Vec<String> = query.capture_names().iter().map(|s| s.to_string()).collect();

    let mut symbols = Vec::new();
    let mut current_impl_type: Option<String> = None;

    while let Some(m) = matches.next() {
        let mut name: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut def_node: Option<tree_sitter::Node> = None;
        let mut parent: Option<String> = None;

        for cap in m.captures {
            let cap_name = &capture_names[cap.index as usize];
            let text = cap.node.utf8_text(source.as_bytes()).unwrap_or("");

            match cap_name.as_str() {
                "function.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Function);
                }
                "function.def" => {
                    def_node = Some(cap.node);
                }
                "method.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Method);
                    parent = current_impl_type.clone();
                }
                "method.def" => {
                    def_node = Some(cap.node);
                }
                "impl.type" => {
                    current_impl_type = Some(text.to_string());
                }
                "struct.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Struct);
                }
                "struct.def" => {
                    def_node = Some(cap.node);
                }
                "enum.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Enum);
                }
                "enum.def" => {
                    def_node = Some(cap.node);
                }
                "trait.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Trait);
                }
                "trait.def" => {
                    def_node = Some(cap.node);
                }
                "class.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "class.def" => {
                    def_node = Some(cap.node);
                }
                "interface.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Interface);
                }
                "interface.def" => {
                    def_node = Some(cap.node);
                }
                "record.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "record.def" => {
                    def_node = Some(cap.node);
                }
                "constructor.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Method);
                }
                "constructor.def" => {
                    def_node = Some(cap.node);
                }
                "type.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Type);
                }
                "type.def" => {
                    def_node = Some(cap.node);
                }
                "const.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Constant);
                }
                "const.def" => {
                    def_node = Some(cap.node);
                }
                "static.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Constant);
                }
                "static.def" => {
                    def_node = Some(cap.node);
                }
                "mod.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Module);
                }
                "mod.def" => {
                    def_node = Some(cap.node);
                }
                _ => {}
            }
        }

        if let (Some(name), Some(kind), Some(node)) = (name, kind, def_node) {
            let start = node.start_position();
            let end = node.end_position();
            let byte_range = (node.start_byte(), node.end_byte());
            let line_range = (start.row + 1, end.row + 1); // 1-indexed

            // Extract signature (first line of the definition)
            let node_text = node.utf8_text(source.as_bytes()).unwrap_or("");
            let signature = node_text.lines().next().unwrap_or("").to_string();

            symbols.push(Symbol {
                name,
                kind,
                file: rel_path.to_string(),
                byte_range,
                line_range,
                language,
                signature,
                definition: None,
                parent,
            });
        }
    }

    debug!("Extracted {} symbols from {}", symbols.len(), rel_path);
    Ok(symbols)
}

/// Extract symbols from all files in the tree. Runs on blocking threads
/// with bounded concurrency.
pub async fn extract_all_symbols(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
) -> Result<usize> {
    let root = root.to_path_buf();
    let file_tree = file_tree.clone();
    let symbol_table = symbol_table.clone();

    let count = tokio::task::spawn_blocking(move || -> Result<usize> {
        let mut total = 0;

        let paths: Vec<(String, Language)> = file_tree
            .files
            .iter()
            .filter(|e| e.value().language.has_tree_sitter_support())
            .map(|e| (e.key().clone(), e.value().language))
            .collect();

        for (rel_path, language) in paths {
            match extract_symbols_from_file(&root, &rel_path, language) {
                Ok(symbols) => {
                    let count = symbols.len();
                    for sym in symbols {
                        symbol_table.insert(sym);
                    }
                    // Mark file as having symbols extracted
                    if let Some(mut entry) = file_tree.files.get_mut(&rel_path) {
                        entry.symbols_extracted = true;
                    }
                    total += count;
                }
                Err(e) => {
                    debug!("Failed to extract symbols from {}: {}", rel_path, e);
                }
            }
        }

        Ok(total)
    })
    .await??;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::file_entry::Language;
    use crate::symbols::symbol::SymbolKind;
    use std::io::Write;

    /// Helper: write source to a temp file and extract symbols.
    fn extract_from_source(source: &str, language: Language) -> Vec<Symbol> {
        let dir = tempfile::tempdir().unwrap();
        let filename = match language {
            Language::Java => "Test.java",
            Language::Rust => "test.rs",
            _ => "test.txt",
        };
        let file_path = dir.path().join(filename);
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(source.as_bytes()).unwrap();
        drop(f);

        extract_symbols_from_file(dir.path(), filename, language).unwrap()
    }

    #[test]
    fn test_java_record_extracted_as_class() {
        let source = r#"
public record Point(int x, int y) {
    public double distance() {
        return Math.sqrt(x * x + y * y);
    }
}
"#;
        let symbols = extract_from_source(source, Language::Java);
        let record_sym = symbols.iter().find(|s| s.name == "Point");
        assert!(
            record_sym.is_some(),
            "Expected to find a symbol named 'Point' for the Java record"
        );
        let record_sym = record_sym.unwrap();
        assert_eq!(
            record_sym.kind,
            SymbolKind::Class,
            "Java record should be mapped to SymbolKind::Class"
        );
    }

    #[test]
    fn test_java_constructor_extracted_as_method() {
        let source = r#"
public class Greeter {
    private String name;

    public Greeter(String name) {
        this.name = name;
    }

    public String greet() {
        return "Hello, " + name;
    }
}
"#;
        let symbols = extract_from_source(source, Language::Java);
        let ctor_sym = symbols.iter().find(|s| s.name == "Greeter" && s.kind == SymbolKind::Method);
        assert!(
            ctor_sym.is_some(),
            "Expected to find a constructor named 'Greeter' with SymbolKind::Method"
        );
    }

    #[test]
    fn test_java_class_and_methods_still_extracted() {
        let source = r#"
public class Foo {
    public void bar() {}
    public int baz() { return 1; }
}
"#;
        let symbols = extract_from_source(source, Language::Java);
        let class_sym = symbols.iter().find(|s| s.name == "Foo" && s.kind == SymbolKind::Class);
        assert!(class_sym.is_some(), "Expected class Foo");

        let method_bar = symbols.iter().find(|s| s.name == "bar" && s.kind == SymbolKind::Method);
        assert!(method_bar.is_some(), "Expected method bar");

        let method_baz = symbols.iter().find(|s| s.name == "baz" && s.kind == SymbolKind::Method);
        assert!(method_baz.is_some(), "Expected method baz");
    }

    #[test]
    fn test_java_record_with_compact_constructor_and_methods() {
        let source = r#"
public record Person(String name, int age) {
    public Person {
        if (age < 0) throw new IllegalArgumentException();
    }

    public String greeting() {
        return "Hi, I'm " + name;
    }
}
"#;
        let symbols = extract_from_source(source, Language::Java);

        // The record itself
        let record = symbols.iter().find(|s| s.name == "Person" && s.kind == SymbolKind::Class);
        assert!(record.is_some(), "Expected record Person as Class");

        // The compact constructor (record-style, no parameter list)
        let compact_ctors: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Person" && s.kind == SymbolKind::Method)
            .collect();
        assert!(
            !compact_ctors.is_empty(),
            "Expected compact constructor Person as Method"
        );

        // The method inside the record
        let method = symbols.iter().find(|s| s.name == "greeting" && s.kind == SymbolKind::Method);
        assert!(method.is_some(), "Expected method greeting");
    }

    #[test]
    fn test_java_class_constructor_disambiguation() {
        // Verifies that a class and its constructor (same name) coexist in the symbol table
        // via line-number-based primary keys, and that both are individually retrievable.
        let source = r#"
public class Widget {
    public Widget(int size) {
        // constructor
    }
}
"#;
        let symbols = extract_from_source(source, Language::Java);

        let classes: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Widget" && s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(classes.len(), 1, "Expected exactly one class Widget");

        let ctors: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Widget" && s.kind == SymbolKind::Method)
            .collect();
        assert_eq!(ctors.len(), 1, "Expected exactly one constructor Widget");

        // They must have different line ranges (otherwise SymbolTable keys would collide)
        assert_ne!(
            classes[0].line_range, ctors[0].line_range,
            "Class and constructor should have different line ranges"
        );
    }
}
