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
        let decorators: Vec<String> = Vec::new();

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
                "var.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Variable);
                }
                "var.def" => {
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
                "macro.name" => {
                    name = Some(text.to_string());
                    kind = Some(SymbolKind::Macro);
                }
                "macro.def" => {
                    def_node = Some(cap.node);
                }
                _ => {}
            }
        }

        if let (Some(name), Some(kind), Some(node)) = (name, kind, def_node) {
            // For decorated_definition nodes, extract decorators from the outer
            // node but use the inner definition child for identity metadata
            // (byte_range, line_range, signature). This ensures the symbol's
            // start line and signature point at the actual def/class line,
            // not the first decorator.
            let (identity_node, decorators) = if node.kind() == "decorated_definition" {
                let mut decs = Vec::new();
                let mut inner_node = node;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "decorator" {
                        if let Ok(dec_text) = child.utf8_text(source.as_bytes()) {
                            let trimmed = dec_text.trim().to_string();
                            if !trimmed.is_empty() {
                                decs.push(trimmed);
                            }
                        }
                    } else if child.kind() == "function_definition"
                        || child.kind() == "class_definition"
                    {
                        inner_node = child;
                    }
                }
                (inner_node, decs)
            } else {
                // Skip bare function_definition/class_definition nodes that
                // are already handled by more specific query patterns:
                // 1. Direct children of decorated_definition — those are
                //    handled by the decorated_definition patterns.
                // 2. Functions resolved as SymbolKind::Function that live
                //    inside a class_definition — those are methods, handled
                //    by the class-body method patterns.
                if matches!(kind, SymbolKind::Function | SymbolKind::Class) {
                    if let Some(parent_node) = node.parent() {
                        if parent_node.kind() == "decorated_definition" {
                            continue;
                        }
                    }
                }
                if kind == SymbolKind::Function {
                    // Walk ancestors to check if this function is inside a class.
                    // If so, skip — it will be emitted as a Method by the class-body pattern.
                    let mut ancestor = node.parent();
                    while let Some(anc) = ancestor {
                        if anc.kind() == "class_definition" {
                            break;
                        }
                        ancestor = anc.parent();
                    }
                    if ancestor.is_some() {
                        continue;
                    }
                }
                (node, decorators)
            };

            let start = identity_node.start_position();
            let end = identity_node.end_position();
            let byte_range = (identity_node.start_byte(), identity_node.end_byte());
            let line_range = (start.row + 1, end.row + 1); // 1-indexed

            // Extract signature (first line of the definition)
            let id_text = identity_node.utf8_text(source.as_bytes()).unwrap_or("");
            let signature = id_text.lines().next().unwrap_or("").to_string();

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
                decorators,
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
            .filter(|e| e.value().language.has_tree_sitter_support() && !e.value().oversized)
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
            Language::Go => "test.go",
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

        // Methods inside classes should NOT also appear as Function symbols
        let handle_func = symbols
            .iter()
            .find(|s| s.name == "handle" && s.kind == SymbolKind::Function);
        assert!(
            handle_func.is_none(),
            "Class method 'handle' should NOT also appear as a Function symbol"
        );
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
            "conditional_var",
            "loop_var",
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

    // ── Python decorator tests ──────────────────────────────────────────

    #[test]
    fn test_python_decorated_function_simple_decorators() {
        let source = r#"
@property
def name(self):
    return self._name

@staticmethod
def create():
    return MyClass()

@classmethod
def from_dict(cls, data):
    return cls(**data)
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Each decorated function should appear exactly once (no duplicates)
        let name_fns: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "name" && s.kind == SymbolKind::Function)
            .collect();
        assert_eq!(name_fns.len(), 1, "Expected exactly one symbol for 'name', got {}", name_fns.len());
        let name_fn = name_fns[0];
        assert!(
            name_fn.decorators.contains(&"@property".to_string()),
            "Expected @property decorator on 'name', got: {:?}",
            name_fn.decorators
        );
        // Signature should be the def line, not the decorator
        assert!(
            name_fn.signature.starts_with("def name"),
            "Signature should start with 'def name', got: '{}'",
            name_fn.signature
        );
        // line_range.0 should point at the def line (line 3), not the decorator (line 2)
        assert_eq!(
            name_fn.line_range.0, 3,
            "line_range.0 should be the def line, not the decorator line"
        );

        let create_fns: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "create" && s.kind == SymbolKind::Function)
            .collect();
        assert_eq!(create_fns.len(), 1, "Expected exactly one symbol for 'create'");
        let create_fn = create_fns[0];
        assert!(
            create_fn.decorators.contains(&"@staticmethod".to_string()),
            "Expected @staticmethod decorator on 'create', got: {:?}",
            create_fn.decorators
        );
        assert!(
            create_fn.signature.starts_with("def create"),
            "Signature should start with 'def create', got: '{}'",
            create_fn.signature
        );

        let from_dict_fns: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "from_dict" && s.kind == SymbolKind::Function)
            .collect();
        assert_eq!(from_dict_fns.len(), 1, "Expected exactly one symbol for 'from_dict'");
        let from_dict_fn = from_dict_fns[0];
        assert!(
            from_dict_fn.decorators.contains(&"@classmethod".to_string()),
            "Expected @classmethod decorator on 'from_dict', got: {:?}",
            from_dict_fn.decorators
        );
        assert!(
            from_dict_fn.signature.starts_with("def from_dict"),
            "Signature should start with 'def from_dict', got: '{}'",
            from_dict_fn.signature
        );
    }

    #[test]
    fn test_python_decorated_function_with_arguments() {
        let source = r#"
from flask import Flask
app = Flask(__name__)

@app.route("/api/health")
def health():
    return {"status": "ok"}

@app.route("/api/users", methods=["GET", "POST"])
def users():
    pass
"#;
        let symbols = extract_from_source(source, Language::Python);

        let health_fn = symbols
            .iter()
            .find(|s| s.name == "health" && s.kind == SymbolKind::Function);
        assert!(health_fn.is_some(), "Expected function 'health'");
        let health_fn = health_fn.unwrap();
        assert_eq!(
            health_fn.decorators.len(),
            1,
            "Expected exactly one decorator on 'health'"
        );
        assert!(
            health_fn.decorators[0].contains("@app.route"),
            "Expected @app.route decorator, got: {}",
            health_fn.decorators[0]
        );

        let users_fn = symbols
            .iter()
            .find(|s| s.name == "users" && s.kind == SymbolKind::Function);
        assert!(users_fn.is_some(), "Expected function 'users'");
        let users_fn = users_fn.unwrap();
        assert!(
            users_fn.decorators[0].contains("@app.route"),
            "Expected @app.route decorator on 'users', got: {:?}",
            users_fn.decorators
        );
    }

    #[test]
    fn test_python_decorated_method_in_class() {
        let source = r#"
class MyClass:
    @property
    def value(self):
        return self._value

    @value.setter
    def value(self, val):
        self._value = val

    @staticmethod
    def helper():
        pass

    def plain_method(self):
        pass
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Decorated methods should have their decorators
        let value_getters: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "value" && s.kind == SymbolKind::Method)
            .collect();
        assert!(
            !value_getters.is_empty(),
            "Expected at least one method named 'value'"
        );

        // Check that at least one 'value' method has @property
        let has_property = value_getters
            .iter()
            .any(|s| s.decorators.contains(&"@property".to_string()));
        assert!(
            has_property,
            "Expected at least one 'value' method with @property decorator"
        );

        let helper = symbols
            .iter()
            .find(|s| s.name == "helper" && s.kind == SymbolKind::Method);
        assert!(helper.is_some(), "Expected method 'helper'");
        let helper = helper.unwrap();
        assert!(
            helper.decorators.contains(&"@staticmethod".to_string()),
            "Expected @staticmethod on 'helper', got: {:?}",
            helper.decorators
        );

        // Plain method should have no decorators
        let plain = symbols
            .iter()
            .find(|s| s.name == "plain_method" && s.kind == SymbolKind::Method);
        assert!(plain.is_some(), "Expected method 'plain_method'");
        assert!(
            plain.unwrap().decorators.is_empty(),
            "Plain method should have no decorators"
        );
    }

    #[test]
    fn test_python_multiple_decorators_on_single_function() {
        let source = r#"
@login_required
@admin_only
@cache(timeout=300)
def admin_dashboard():
    pass
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Exactly one symbol for the function, even with 3 decorators
        let dashboards: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "admin_dashboard" && s.kind == SymbolKind::Function)
            .collect();
        assert_eq!(
            dashboards.len(), 1,
            "Expected exactly one symbol for 'admin_dashboard', got {}",
            dashboards.len()
        );
        let dashboard = dashboards[0];
        assert_eq!(
            dashboard.decorators.len(),
            3,
            "Expected 3 decorators on 'admin_dashboard', got: {:?}",
            dashboard.decorators
        );
        assert!(
            dashboard.decorators.contains(&"@login_required".to_string()),
            "Expected @login_required"
        );
        assert!(
            dashboard.decorators.contains(&"@admin_only".to_string()),
            "Expected @admin_only"
        );
        let has_cache = dashboard
            .decorators
            .iter()
            .any(|d| d.starts_with("@cache"));
        assert!(has_cache, "Expected @cache decorator");
        // Signature should be the def line (line 5), not any decorator line
        assert!(
            dashboard.signature.starts_with("def admin_dashboard"),
            "Signature should start with 'def admin_dashboard', got: '{}'",
            dashboard.signature
        );
        assert_eq!(
            dashboard.line_range.0, 5,
            "line_range.0 should be the def line (5), not the first decorator line"
        );
    }

    #[test]
    fn test_python_decorated_class() {
        let source = r#"
@dataclass
class Point:
    x: float
    y: float

@dataclass(frozen=True)
class Config:
    host: str
    port: int
"#;
        let symbols = extract_from_source(source, Language::Python);

        // Each decorated class should appear exactly once
        let points: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Point" && s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(points.len(), 1, "Expected exactly one symbol for 'Point'");
        let point = points[0];
        assert!(
            point.decorators.contains(&"@dataclass".to_string()),
            "Expected @dataclass on Point, got: {:?}",
            point.decorators
        );
        // Signature should be the class line, not the decorator
        assert!(
            point.signature.starts_with("class Point"),
            "Signature should start with 'class Point', got: '{}'",
            point.signature
        );
        // line_range.0 should point at the class line (line 3), not decorator (line 2)
        assert_eq!(
            point.line_range.0, 3,
            "line_range.0 should be the class line, not the decorator line"
        );

        let configs: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Config" && s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(configs.len(), 1, "Expected exactly one symbol for 'Config'");
        let config = configs[0];
        assert_eq!(
            config.decorators.len(),
            1,
            "Expected 1 decorator on Config"
        );
        assert!(
            config.decorators[0].starts_with("@dataclass"),
            "Expected @dataclass decorator on Config, got: {}",
            config.decorators[0]
        );
        assert!(
            config.signature.starts_with("class Config"),
            "Signature should start with 'class Config', got: '{}'",
            config.signature
        );
    }

    #[test]
    fn test_python_undecorated_symbols_have_empty_decorators() {
        let source = r#"
def plain_function():
    pass

class PlainClass:
    def plain_method(self):
        pass

MAX_VALUE = 100
"#;
        let symbols = extract_from_source(source, Language::Python);

        for sym in &symbols {
            assert!(
                sym.decorators.is_empty(),
                "Symbol '{}' (kind {:?}) should have no decorators but got: {:?}",
                sym.name,
                sym.kind,
                sym.decorators
            );
        }
    }

    #[test]
    fn test_python_decorators_do_not_appear_on_non_python_symbols() {
        // Rust functions should never have decorators
        let source = r#"
fn hello() -> String {
    "hello".to_string()
}

struct Foo {
    x: i32,
}

impl Foo {
    fn new(x: i32) -> Self {
        Foo { x }
    }
}
"#;
        let symbols = extract_from_source(source, Language::Rust);
        for sym in &symbols {
            assert!(
                sym.decorators.is_empty(),
                "Rust symbol '{}' should have no decorators",
                sym.name
            );
        }
    }

    #[test]
    fn test_python_decorator_serde_serialization() {
        // Verify that the decorators field serializes correctly
        let sym = Symbol {
            name: "my_route".to_string(),
            kind: SymbolKind::Function,
            file: "app.py".to_string(),
            byte_range: (0, 100),
            line_range: (1, 5),
            language: Language::Python,
            signature: "def my_route():".to_string(),
            definition: None,
            parent: None,
            decorators: vec!["@app.route(\"/api/test\")".to_string(), "@login_required".to_string()],
        };

        let json = serde_json::to_string(&sym).unwrap();
        assert!(
            json.contains("decorators"),
            "JSON should contain 'decorators' field"
        );
        assert!(
            json.contains("@app.route"),
            "JSON should contain the decorator text"
        );

        // Also verify that empty decorators are omitted (skip_serializing_if)
        let sym_no_dec = Symbol {
            name: "plain".to_string(),
            kind: SymbolKind::Function,
            file: "app.py".to_string(),
            byte_range: (0, 50),
            line_range: (1, 3),
            language: Language::Python,
            signature: "def plain():".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
        };

        let json_no_dec = serde_json::to_string(&sym_no_dec).unwrap();
        assert!(
            !json_no_dec.contains("decorators"),
            "JSON should NOT contain 'decorators' when empty (skip_serializing_if)"
        );
    }

    #[test]
    fn test_python_decorated_class_methods_not_dropped() {
        // Regression test: methods inside decorated classes must still be extracted.
        // The duplicate-skip logic for decorated_definition must not suppress
        // method symbols that happen to be inside a decorated class.
        let source = r#"
@dataclass
class User:
    name: str
    age: int

    def greet(self):
        return f"Hello, {self.name}"

    @property
    def is_adult(self):
        return self.age >= 18

    @staticmethod
    def default():
        return User("anonymous", 0)
"#;
        let symbols = extract_from_source(source, Language::Python);

        // The decorated class should appear exactly once
        let users: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "User" && s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(users.len(), 1, "Expected exactly one class 'User'");
        assert!(
            users[0].decorators.contains(&"@dataclass".to_string()),
            "Expected @dataclass on User"
        );
        assert!(
            users[0].signature.starts_with("class User"),
            "Signature should start with 'class User', got: '{}'",
            users[0].signature
        );

        // Plain method inside decorated class
        let greet = symbols
            .iter()
            .find(|s| s.name == "greet" && s.kind == SymbolKind::Method);
        assert!(
            greet.is_some(),
            "Method 'greet' inside decorated class should still be extracted"
        );
        assert!(
            greet.unwrap().decorators.is_empty(),
            "Plain method 'greet' should have no decorators"
        );

        // Methods inside classes should NOT also appear as Function symbols
        let greet_func = symbols
            .iter()
            .find(|s| s.name == "greet" && s.kind == SymbolKind::Function);
        assert!(
            greet_func.is_none(),
            "Class method 'greet' should NOT also appear as a Function symbol"
        );

        // Decorated method inside decorated class
        let is_adult = symbols
            .iter()
            .find(|s| s.name == "is_adult" && s.kind == SymbolKind::Method);
        assert!(
            is_adult.is_some(),
            "Decorated method 'is_adult' inside decorated class should still be extracted"
        );
        let is_adult = is_adult.unwrap();
        assert!(
            is_adult.decorators.contains(&"@property".to_string()),
            "Expected @property on 'is_adult', got: {:?}",
            is_adult.decorators
        );

        // Static method inside decorated class
        let default_fn = symbols
            .iter()
            .find(|s| s.name == "default" && s.kind == SymbolKind::Method);
        assert!(
            default_fn.is_some(),
            "Decorated method 'default' inside decorated class should still be extracted"
        );
        let default_fn = default_fn.unwrap();
        assert!(
            default_fn.decorators.contains(&"@staticmethod".to_string()),
            "Expected @staticmethod on 'default', got: {:?}",
            default_fn.decorators
        );
    }

    // ── Rust macro_rules! tests ────────────────────────────────────────

    #[test]
    fn test_rust_macro_rules_extracted_as_macro() {
        let source = r#"
macro_rules! my_vec {
    () => { Vec::new() };
    ($($x:expr),+ $(,)?) => {
        {
            let mut v = Vec::new();
            $(v.push($x);)+
            v
        }
    };
}
"#;
        let symbols = extract_from_source(source, Language::Rust);
        let macro_sym = symbols.iter().find(|s| s.name == "my_vec");
        assert!(
            macro_sym.is_some(),
            "Expected to find a symbol named 'my_vec' for the macro_rules! definition"
        );
        let macro_sym = macro_sym.unwrap();
        assert_eq!(
            macro_sym.kind,
            SymbolKind::Macro,
            "macro_rules! definition should be mapped to SymbolKind::Macro"
        );
        assert!(
            macro_sym.signature.contains("macro_rules!"),
            "Signature should contain 'macro_rules!', got: '{}'",
            macro_sym.signature
        );
    }

    #[test]
    fn test_rust_macro_alongside_functions_and_structs() {
        let source = r#"
macro_rules! log_debug {
    ($($arg:tt)*) => { println!($($arg)*) };
}

struct Config {
    name: String,
}

fn init() -> Config {
    Config { name: String::new() }
}

macro_rules! assert_config {
    ($c:expr) => { assert!(!$c.name.is_empty()) };
}
"#;
        let symbols = extract_from_source(source, Language::Rust);

        let log_macro = symbols
            .iter()
            .find(|s| s.name == "log_debug" && s.kind == SymbolKind::Macro);
        assert!(log_macro.is_some(), "Expected macro log_debug");

        let config_struct = symbols
            .iter()
            .find(|s| s.name == "Config" && s.kind == SymbolKind::Struct);
        assert!(config_struct.is_some(), "Expected struct Config");

        let init_fn = symbols
            .iter()
            .find(|s| s.name == "init" && s.kind == SymbolKind::Function);
        assert!(init_fn.is_some(), "Expected function init");

        let assert_macro = symbols
            .iter()
            .find(|s| s.name == "assert_config" && s.kind == SymbolKind::Macro);
        assert!(assert_macro.is_some(), "Expected macro assert_config");
    }

    #[test]
    fn test_rust_macro_line_range_and_signature() {
        let source = r#"
macro_rules! create_fn {
    ($name:ident) => {
        fn $name() {
            println!(stringify!($name));
        }
    };
}
"#;
        let symbols = extract_from_source(source, Language::Rust);
        let macro_sym = symbols
            .iter()
            .find(|s| s.name == "create_fn" && s.kind == SymbolKind::Macro)
            .expect("Expected macro create_fn");

        // Line range should start at line 2 (1-indexed) where macro_rules! begins
        assert_eq!(
            macro_sym.line_range.0, 2,
            "Macro should start at line 2, got {}",
            macro_sym.line_range.0
        );

        // Line range should end at line 8 where the closing brace is
        assert_eq!(
            macro_sym.line_range.1, 8,
            "Macro should end at line 8, got {}",
            macro_sym.line_range.1
        );
    }

    #[test]
    fn test_go_var_declarations_mapped_to_variable() {
        let source = r#"
package main

var GlobalConfig string
var MaxRetries int = 3

const MaxSize = 100
const AppName = "myapp"

func main() {}
"#;
        let symbols = extract_from_source(source, Language::Go);

        // var declarations should be SymbolKind::Variable
        let global_config = symbols
            .iter()
            .find(|s| s.name == "GlobalConfig")
            .expect("Expected to find symbol GlobalConfig");
        assert_eq!(
            global_config.kind,
            SymbolKind::Variable,
            "Go var declaration should be mapped to SymbolKind::Variable, got {:?}",
            global_config.kind
        );

        let max_retries = symbols
            .iter()
            .find(|s| s.name == "MaxRetries")
            .expect("Expected to find symbol MaxRetries");
        assert_eq!(
            max_retries.kind,
            SymbolKind::Variable,
            "Go var declaration should be mapped to SymbolKind::Variable, got {:?}",
            max_retries.kind
        );

        // const declarations should remain SymbolKind::Constant
        let max_size = symbols
            .iter()
            .find(|s| s.name == "MaxSize")
            .expect("Expected to find symbol MaxSize");
        assert_eq!(
            max_size.kind,
            SymbolKind::Constant,
            "Go const declaration should be mapped to SymbolKind::Constant, got {:?}",
            max_size.kind
        );

        let app_name = symbols
            .iter()
            .find(|s| s.name == "AppName")
            .expect("Expected to find symbol AppName");
        assert_eq!(
            app_name.kind,
            SymbolKind::Constant,
            "Go const declaration should be mapped to SymbolKind::Constant, got {:?}",
            app_name.kind
        );

        // main function should still be extracted
        let main_fn = symbols
            .iter()
            .find(|s| s.name == "main" && s.kind == SymbolKind::Function);
        assert!(main_fn.is_some(), "Expected to find function main");
    }

    #[test]
    fn test_go_var_block_declarations_mapped_to_variable() {
        let source = r#"
package main

var (
    Timeout  int = 30
    BasePath string
)

const (
    Version = "1.0"
    Debug   = false
)
"#;
        let symbols = extract_from_source(source, Language::Go);

        // var block entries should be SymbolKind::Variable
        let timeout = symbols
            .iter()
            .find(|s| s.name == "Timeout")
            .expect("Expected to find symbol Timeout");
        assert_eq!(
            timeout.kind,
            SymbolKind::Variable,
            "Go var block declaration should be mapped to SymbolKind::Variable, got {:?}",
            timeout.kind
        );

        let base_path = symbols
            .iter()
            .find(|s| s.name == "BasePath")
            .expect("Expected to find symbol BasePath");
        assert_eq!(
            base_path.kind,
            SymbolKind::Variable,
            "Go var block declaration should be mapped to SymbolKind::Variable, got {:?}",
            base_path.kind
        );

        // const block entries should remain SymbolKind::Constant
        let version = symbols
            .iter()
            .find(|s| s.name == "Version")
            .expect("Expected to find symbol Version");
        assert_eq!(
            version.kind,
            SymbolKind::Constant,
            "Go const block declaration should be mapped to SymbolKind::Constant, got {:?}",
            version.kind
        );

        let debug_sym = symbols
            .iter()
            .find(|s| s.name == "Debug")
            .expect("Expected to find symbol Debug");
        assert_eq!(
            debug_sym.kind,
            SymbolKind::Constant,
            "Go const block declaration should be mapped to SymbolKind::Constant, got {:?}",
            debug_sym.kind
        );
    }

    #[tokio::test]
    async fn test_extract_all_symbols_skips_oversized_files() {
        use crate::index::file_entry::FileEntry;
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();

        // Create a normal Rust file
        let normal_path = dir.path().join("normal.rs");
        std::fs::write(&normal_path, "fn hello() {}\nfn world() {}").unwrap();

        // Create another Rust file that we'll mark as oversized
        let oversized_path = dir.path().join("oversized.rs");
        std::fs::write(&oversized_path, "fn secret() {}").unwrap();

        let file_tree = Arc::new(FileTree::new());

        // Insert normal file
        let mut normal_entry = FileEntry::new("normal.rs".to_string(), 27, Utc::now());
        normal_entry.oversized = false;
        file_tree.insert(normal_entry);

        // Insert oversized file (flagged as oversized)
        let mut oversized_entry = FileEntry::new("oversized.rs".to_string(), 15, Utc::now());
        oversized_entry.oversized = true;
        file_tree.insert(oversized_entry);

        let symbol_table = Arc::new(SymbolTable::new());
        let count = extract_all_symbols(dir.path(), &file_tree, &symbol_table)
            .await
            .unwrap();

        // Should have extracted symbols only from normal.rs (hello, world)
        assert!(count >= 2, "Expected at least 2 symbols from normal.rs, got {}", count);

        // Symbols from oversized.rs should not exist
        let oversized_symbols = symbol_table.list_by_file("oversized.rs");
        assert!(
            oversized_symbols.is_empty(),
            "Oversized file should not have symbols extracted, but found: {:?}",
            oversized_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        // Symbols from normal.rs should exist
        let normal_symbols = symbol_table.list_by_file("normal.rs");
        assert!(
            !normal_symbols.is_empty(),
            "Normal file should have symbols extracted"
        );
    }
}
