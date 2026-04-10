use super::{LanguageConfig, TestPattern};

// Pattern design notes:
//
// Earlier versions of this query nested an OPTIONAL `(method...)` capture
// inside the `(class_definition ...)` patterns. Tree-sitter fired those
// patterns once per matching method, so a class with N methods produced N
// "class matches" — every one of which contained both `@class.name` AND
// `@method.name`. The parser's single-symbol-per-match loop then overwrote
// the class capture with the method, and the class itself was never
// emitted. Synthetic test classes with no decorated methods coincidentally
// produced a class-only match (the optional collapsed empty), which hid
// the bug. Real-world classes with `@property` methods always failed.
//
// The current design produces *standalone* matches:
//   - Functions and decorated functions match independently of where they
//     live. Whether each is a top-level function or a method is decided by
//     the parser via an ancestor walk over the syntax tree.
//   - Classes and decorated classes match independently of their bodies.
//
// Methods are not captured by name in this query. The parser uses the
// generic function patterns plus an ancestor walk to detect when a function
// is inside a `class_definition`, and rewrites its kind to Method with the
// containing class as parent.
pub const SYMBOLS_QUERY: &str = r#"
(decorated_definition
  definition: (function_definition
    name: (identifier) @function.name)) @function.def

(function_definition
  name: (identifier) @function.name) @function.def

(decorated_definition
  definition: (class_definition
    name: (identifier) @class.name)) @class.def

(class_definition
  name: (identifier) @class.name) @class.def

(module
  (expression_statement
    (assignment
      left: (identifier) @constant.name)) @constant.def)
"#;

pub const CALLERS_QUERY: &str = r#"
(call
  function: (identifier) @callee)

(call
  function: (attribute
    object: (_) @receiver
    attribute: (identifier) @callee))
"#;

pub const VARIABLES_QUERY: &str = r#"
(assignment
  left: (identifier) @var.name)

(assignment
  left: (pattern_list
    (identifier) @var.name))

(assignment
  left: (tuple_pattern
    (identifier) @var.name))

(for_statement
  left: (identifier) @var.name)

(for_statement
  left: (tuple_pattern
    (identifier) @var.name))

(with_item
  (as_pattern
    alias: (as_pattern_target
      (identifier) @var.name)))

(parameters
  (identifier) @var.name)

(parameters
  (default_parameter
    name: (identifier) @var.name))

(parameters
  (typed_parameter
    (identifier) @var.name))

(parameters
  (typed_default_parameter
    name: (identifier) @var.name))
"#;

pub const IMPORTS_QUERY: &str = r#"
(import_statement
  name: (dotted_name) @import.source)

(import_statement
  name: (aliased_import
    name: (dotted_name) @import.source))

(import_from_statement
  module_name: (dotted_name) @import.source)

(import_from_statement
  module_name: (relative_import
    (dotted_name) @import.source))
"#;

pub fn config() -> LanguageConfig {
    LanguageConfig {
        language: tree_sitter_python::LANGUAGE.into(),
        symbols_query: SYMBOLS_QUERY,
        callers_query: CALLERS_QUERY,
        variables_query: VARIABLES_QUERY,
        imports_query: IMPORTS_QUERY,
        test_patterns: vec![
            TestPattern::FunctionPrefix("test_"),
            TestPattern::FileContains("test_"),
            TestPattern::FileContains("_test."),
        ],
    }
}
