use super::{LanguageConfig, TestPattern};

pub const SYMBOLS_QUERY: &str = r#"
(function_declaration
  name: (identifier) @function.name) @function.def

(class_declaration
  name: (type_identifier) @class.name) @class.def

(method_definition
  name: (property_identifier) @method.name) @method.def

(lexical_declaration
  (variable_declarator
    name: (identifier) @const.name
    value: (arrow_function))) @const.def

(interface_declaration
  name: (type_identifier) @interface.name) @interface.def

(type_alias_declaration
  name: (type_identifier) @type.name) @type.def

(enum_declaration
  name: (identifier) @enum.name) @enum.def
"#;

pub const CALLERS_QUERY: &str = r#"
(call_expression
  function: (identifier) @callee)

(call_expression
  function: (member_expression
    property: (property_identifier) @callee))

(new_expression
  constructor: (identifier) @callee)
"#;

pub const VARIABLES_QUERY: &str = r#"
(variable_declarator
  name: (identifier) @var.name)

(variable_declarator
  name: (object_pattern
    (shorthand_property_identifier_pattern) @var.name))

(variable_declarator
  name: (array_pattern
    (identifier) @var.name))

(for_in_statement
  left: (identifier) @var.name)

(for_in_statement
  left: (lexical_declaration
    (variable_declarator
      name: (identifier) @var.name)))

(required_parameter
  pattern: (identifier) @var.name)

(optional_parameter
  pattern: (identifier) @var.name)
"#;

pub const IMPORTS_QUERY: &str = r#"
(import_statement
  source: (string
    (string_fragment) @import.source))
"#;

pub fn config() -> LanguageConfig {
    LanguageConfig {
        language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        symbols_query: SYMBOLS_QUERY,
        callers_query: CALLERS_QUERY,
        variables_query: VARIABLES_QUERY,
        imports_query: IMPORTS_QUERY,
        test_patterns: vec![
            TestPattern::CallExpression("it"),
            TestPattern::CallExpression("test"),
            TestPattern::CallExpression("describe"),
            TestPattern::FileContains(".test."),
            TestPattern::FileContains(".spec."),
            TestPattern::FileContains("__tests__"),
        ],
    }
}

pub const JS_SYMBOLS_QUERY: &str = r#"
(function_declaration
  name: (identifier) @function.name) @function.def

(class_declaration
  name: (identifier) @class.name) @class.def

(method_definition
  name: (property_identifier) @method.name) @method.def

(lexical_declaration
  (variable_declarator
    name: (identifier) @const.name
    value: (arrow_function))) @const.def
"#;

pub const JS_CALLERS_QUERY: &str = r#"
(call_expression
  function: (identifier) @callee)

(call_expression
  function: (member_expression
    property: (property_identifier) @callee))

(new_expression
  constructor: (identifier) @callee)
"#;

pub const JS_VARIABLES_QUERY: &str = r#"
(variable_declarator
  name: (identifier) @var.name)

(variable_declarator
  name: (object_pattern
    (shorthand_property_identifier_pattern) @var.name))

(variable_declarator
  name: (array_pattern
    (identifier) @var.name))

(for_in_statement
  left: (identifier) @var.name)

(formal_parameters
  (identifier) @var.name)
"#;

pub const JS_IMPORTS_QUERY: &str = r#"
(import_statement
  source: (string
    (string_fragment) @import.source))
"#;

pub fn js_config() -> LanguageConfig {
    LanguageConfig {
        language: tree_sitter_javascript::LANGUAGE.into(),
        symbols_query: JS_SYMBOLS_QUERY,
        callers_query: JS_CALLERS_QUERY,
        variables_query: JS_VARIABLES_QUERY,
        imports_query: JS_IMPORTS_QUERY,
        test_patterns: vec![
            TestPattern::CallExpression("it"),
            TestPattern::CallExpression("test"),
            TestPattern::CallExpression("describe"),
            TestPattern::FileContains(".test."),
            TestPattern::FileContains(".spec."),
            TestPattern::FileContains("__tests__"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::StreamingIterator;

    /// Helper: run the callers query on source code and return all captured callee names.
    fn extract_callees(
        source: &str,
        language: tree_sitter::Language,
        query_str: &str,
    ) -> Vec<(String, usize)> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let query = tree_sitter::Query::new(&language, query_str).unwrap();

        let capture_names: Vec<String> = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let callee_idx = capture_names.iter().position(|n| n == "callee").unwrap();

        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut results = Vec::new();

        while let Some(m) = matches.next() {
            for cap in m.captures {
                if cap.index as usize == callee_idx {
                    let text = cap.node.utf8_text(source.as_bytes()).unwrap();
                    let line = cap.node.start_position().row + 1;
                    results.push((text.to_string(), line));
                }
            }
        }
        results
    }

    #[test]
    fn test_ts_callers_query_captures_new_expression() {
        let source = r#"
class Foo {
    constructor() {}
}

const a = new Foo();
const b = Foo.create();
doSomething(new Foo());
"#;
        let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let callees = extract_callees(source, lang, CALLERS_QUERY);
        let callee_names: Vec<&str> = callees.iter().map(|(name, _)| name.as_str()).collect();

        // `new Foo()` should be captured as a callee "Foo"
        assert!(
            callee_names.contains(&"Foo"),
            "Expected 'Foo' from `new Foo()` in callers, got: {:?}",
            callee_names
        );

        // Count how many times Foo appears — should be at least 2 (line 6 and line 8)
        let foo_count = callee_names.iter().filter(|&&n| n == "Foo").count();
        assert!(
            foo_count >= 2,
            "Expected at least 2 occurrences of 'Foo' (from `new Foo()` expressions), got {}",
            foo_count
        );

        // `doSomething` should also be captured as a regular call expression
        assert!(
            callee_names.contains(&"doSomething"),
            "Expected 'doSomething' from `doSomething(...)` in callers, got: {:?}",
            callee_names
        );

        // `create` should be captured from `Foo.create()`
        assert!(
            callee_names.contains(&"create"),
            "Expected 'create' from `Foo.create()` in callers, got: {:?}",
            callee_names
        );
    }

    #[test]
    fn test_ts_callers_query_new_with_member_expression() {
        // `new a.b.Foo()` — the constructor is a member_expression, not an identifier.
        // We don't capture these via the new_expression pattern (only simple `new Foo()`).
        // But `call_expression` patterns should still capture normal calls.
        let source = r#"
const x = new ns.Widget();
regularCall();
"#;
        let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let callees = extract_callees(source, lang, CALLERS_QUERY);
        let callee_names: Vec<&str> = callees.iter().map(|(name, _)| name.as_str()).collect();

        // regularCall should be captured
        assert!(
            callee_names.contains(&"regularCall"),
            "Expected 'regularCall' in callers, got: {:?}",
            callee_names
        );
    }

    #[test]
    fn test_js_callers_query_captures_new_expression() {
        let source = r#"
class Bar {}
const b = new Bar();
normalFunc();
"#;
        let lang: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
        let callees = extract_callees(source, lang, JS_CALLERS_QUERY);
        let callee_names: Vec<&str> = callees.iter().map(|(name, _)| name.as_str()).collect();

        // `new Bar()` should be captured
        assert!(
            callee_names.contains(&"Bar"),
            "Expected 'Bar' from `new Bar()` in JS callers, got: {:?}",
            callee_names
        );

        // `normalFunc` should also be captured
        assert!(
            callee_names.contains(&"normalFunc"),
            "Expected 'normalFunc' in JS callers, got: {:?}",
            callee_names
        );
    }
}
