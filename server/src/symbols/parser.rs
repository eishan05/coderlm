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
                "object.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Module);
                }
                "object.def" => {
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
                "constant.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Constant);
                }
                "constant.def" => {
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
            Language::Scala => "Test.scala",
            Language::Python => "test.py",
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

    #[test]
    fn test_scala_object_extracted_as_module() {
        let source = r#"
object MyApp {
  def main(args: Array[String]): Unit = {
    println("Hello")
  }
}
"#;
        let symbols = extract_from_source(source, Language::Scala);
        let obj_sym = symbols.iter().find(|s| s.name == "MyApp");
        assert!(
            obj_sym.is_some(),
            "Expected to find a symbol named 'MyApp' for the Scala object"
        );
        let obj_sym = obj_sym.unwrap();
        assert_eq!(
            obj_sym.kind,
            SymbolKind::Module,
            "Scala object should be mapped to SymbolKind::Module"
        );
    }

    #[test]
    fn test_scala_object_alongside_class_and_trait() {
        let source = r#"
trait Greeter {
  def greet(name: String): String
}

class DefaultGreeter extends Greeter {
  def greet(name: String): String = s"Hello, $name"
}

object GreeterApp {
  def main(args: Array[String]): Unit = {
    val g = new DefaultGreeter()
    println(g.greet("World"))
  }
}
"#;
        let symbols = extract_from_source(source, Language::Scala);

        let trait_sym = symbols.iter().find(|s| s.name == "Greeter" && s.kind == SymbolKind::Trait);
        assert!(trait_sym.is_some(), "Expected trait Greeter");

        let class_sym = symbols
            .iter()
            .find(|s| s.name == "DefaultGreeter" && s.kind == SymbolKind::Class);
        assert!(class_sym.is_some(), "Expected class DefaultGreeter");

        let obj_sym = symbols
            .iter()
            .find(|s| s.name == "GreeterApp" && s.kind == SymbolKind::Module);
        assert!(obj_sym.is_some(), "Expected object GreeterApp as Module");

        // Functions inside the object should also be extracted
        let main_fn = symbols
            .iter()
            .find(|s| s.name == "main" && s.kind == SymbolKind::Function);
        assert!(main_fn.is_some(), "Expected function main inside object");
    }

    #[test]
    fn test_scala_companion_object_and_class() {
        // Scala companion objects share the same name as their class.
        // Both should be extractable with different SymbolKinds.
        let source = r#"
class Point(val x: Int, val y: Int)

object Point {
  def origin: Point = new Point(0, 0)
}
"#;
        let symbols = extract_from_source(source, Language::Scala);

        let classes: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Point" && s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(classes.len(), 1, "Expected exactly one class Point");

        let objects: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Point" && s.kind == SymbolKind::Module)
            .collect();
        assert_eq!(objects.len(), 1, "Expected exactly one object Point as Module");

        // They must have different line ranges
        assert_ne!(
            classes[0].line_range, objects[0].line_range,
            "Class and companion object should have different line ranges"
        );
    }

    #[test]
    fn test_python_module_level_constants_extracted() {
        let source = r#"
MAX_RETRIES = 3
DEFAULT_TIMEOUT = 30
API_BASE_URL = "https://api.example.com"

def do_something():
    pass
"#;
        let symbols = extract_from_source(source, Language::Python);

        let max_retries = symbols
            .iter()
            .find(|s| s.name == "MAX_RETRIES" && s.kind == SymbolKind::Constant);
        assert!(
            max_retries.is_some(),
            "Expected module-level constant MAX_RETRIES"
        );

        let default_timeout = symbols
            .iter()
            .find(|s| s.name == "DEFAULT_TIMEOUT" && s.kind == SymbolKind::Constant);
        assert!(
            default_timeout.is_some(),
            "Expected module-level constant DEFAULT_TIMEOUT"
        );

        let api_url = symbols
            .iter()
            .find(|s| s.name == "API_BASE_URL" && s.kind == SymbolKind::Constant);
        assert!(
            api_url.is_some(),
            "Expected module-level constant API_BASE_URL"
        );

        // Function should still be extracted
        let func = symbols
            .iter()
            .find(|s| s.name == "do_something" && s.kind == SymbolKind::Function);
        assert!(func.is_some(), "Expected function do_something");
    }

    #[test]
    fn test_python_function_local_assignments_not_extracted_as_constants() {
        let source = r#"
MODULE_CONST = "visible"

def my_function():
    local_var = 42
    another_local = "hidden"
    return local_var

class MyClass:
    class_attr = "also not a module constant"

    def method(self):
        method_local = 99
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Module-level constant should be extracted
        let module_const = symbols
            .iter()
            .find(|s| s.name == "MODULE_CONST" && s.kind == SymbolKind::Constant);
        assert!(
            module_const.is_some(),
            "Expected module-level constant MODULE_CONST"
        );

        // Function-local assignments should NOT be extracted as constants
        let local_var = symbols
            .iter()
            .find(|s| s.name == "local_var" && s.kind == SymbolKind::Constant);
        assert!(
            local_var.is_none(),
            "Function-local variable 'local_var' should NOT be a constant"
        );

        let another_local = symbols
            .iter()
            .find(|s| s.name == "another_local" && s.kind == SymbolKind::Constant);
        assert!(
            another_local.is_none(),
            "Function-local variable 'another_local' should NOT be a constant"
        );

        // Class body assignments should NOT be extracted as constants
        let class_attr = symbols
            .iter()
            .find(|s| s.name == "class_attr" && s.kind == SymbolKind::Constant);
        assert!(
            class_attr.is_none(),
            "Class attribute 'class_attr' should NOT be a constant"
        );

        // Method-local assignments should NOT be extracted as constants
        let method_local = symbols
            .iter()
            .find(|s| s.name == "method_local" && s.kind == SymbolKind::Constant);
        assert!(
            method_local.is_none(),
            "Method-local variable 'method_local' should NOT be a constant"
        );
    }

    #[test]
    fn test_python_constants_alongside_classes_and_functions() {
        let source = r#"
SENTINEL = object()
CONFIG = {"key": "value", "timeout": 30}

class Handler:
    def handle(self):
        pass

def process():
    pass

ANOTHER_CONST = True
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Constants
        let sentinel = symbols
            .iter()
            .find(|s| s.name == "SENTINEL" && s.kind == SymbolKind::Constant);
        assert!(sentinel.is_some(), "Expected constant SENTINEL");

        let config = symbols
            .iter()
            .find(|s| s.name == "CONFIG" && s.kind == SymbolKind::Constant);
        assert!(config.is_some(), "Expected constant CONFIG");

        let another = symbols
            .iter()
            .find(|s| s.name == "ANOTHER_CONST" && s.kind == SymbolKind::Constant);
        assert!(another.is_some(), "Expected constant ANOTHER_CONST");

        // Function should also be present
        let process_fn = symbols
            .iter()
            .find(|s| s.name == "process" && s.kind == SymbolKind::Function);
        assert!(process_fn.is_some(), "Expected function process");

        // The class method should be extracted (class with methods produces method symbols)
        let handle_method = symbols
            .iter()
            .find(|s| s.name == "handle" && s.kind == SymbolKind::Method);
        assert!(handle_method.is_some(), "Expected method handle from class Handler");
    }

    #[test]
    fn test_python_type_annotated_module_constants() {
        // Type-annotated assignments like `MAX_SIZE: int = 100` are also common
        let source = r#"
MAX_SIZE: int = 100
NAME: str = "coderlm"

def helper():
    x: int = 5
"#;
        let symbols = extract_from_source(source, Language::Python);

        let max_size = symbols
            .iter()
            .find(|s| s.name == "MAX_SIZE" && s.kind == SymbolKind::Constant);
        assert!(
            max_size.is_some(),
            "Expected type-annotated module constant MAX_SIZE"
        );

        let name_const = symbols
            .iter()
            .find(|s| s.name == "NAME" && s.kind == SymbolKind::Constant);
        assert!(
            name_const.is_some(),
            "Expected type-annotated module constant NAME"
        );

        // Function-local annotated assignment should NOT be a constant
        let local_x = symbols
            .iter()
            .find(|s| s.name == "x" && s.kind == SymbolKind::Constant);
        assert!(
            local_x.is_none(),
            "Function-local annotated variable 'x' should NOT be a constant"
        );
    }

    #[test]
    fn test_python_bare_annotation_at_module_level() {
        // Bare annotations like `X: int` (without assignment) are also captured
        // because tree-sitter-python parses them as `assignment` nodes.
        // This is acceptable: module-level annotations declare module globals.
        let source = r#"
X: int
Y: str = "hello"
"#;
        let symbols = extract_from_source(source, Language::Python);

        let x_sym = symbols
            .iter()
            .find(|s| s.name == "X" && s.kind == SymbolKind::Constant);
        assert!(
            x_sym.is_some(),
            "Module-level bare annotation 'X: int' should be captured as a constant"
        );

        let y_sym = symbols
            .iter()
            .find(|s| s.name == "Y" && s.kind == SymbolKind::Constant);
        assert!(
            y_sym.is_some(),
            "Module-level annotated assignment 'Y: str = ...' should be captured as a constant"
        );
    }

    #[test]
    fn test_python_nested_scope_assignments_excluded() {
        // Comprehensive test: assignments in various nested scopes
        // should NOT appear as module-level constants
        let source = r#"
TOP_LEVEL = "module constant"

def outer():
    outer_local = 1
    def inner():
        inner_local = 2

class Outer:
    class_var = "class level"
    class Nested:
        nested_var = "nested class level"

if True:
    conditional_var = "inside if"

for i in range(10):
    loop_var = "inside for"
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Module-level constant should be found
        let top = symbols
            .iter()
            .find(|s| s.name == "TOP_LEVEL" && s.kind == SymbolKind::Constant);
        assert!(top.is_some(), "Expected module-level constant TOP_LEVEL");

        // None of the nested assignments should be constants
        for excluded_name in &[
            "outer_local",
            "inner_local",
            "class_var",
            "nested_var",
        ] {
            let found = symbols
                .iter()
                .find(|s| s.name == *excluded_name && s.kind == SymbolKind::Constant);
            assert!(
                found.is_none(),
                "Nested variable '{}' should NOT be a module-level constant",
                excluded_name
            );
        }
    }
}
