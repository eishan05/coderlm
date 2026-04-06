use super::{LanguageConfig, TestPattern};

pub const SYMBOLS_QUERY: &str = r#"
(function_declaration
  name: (identifier) @function.name) @function.def

(method_declaration
  name: (field_identifier) @method.name) @method.def

(type_declaration
  (type_spec
    name: (type_identifier) @struct.name
    type: (struct_type))) @struct.def

(type_declaration
  (type_spec
    name: (type_identifier) @interface.name
    type: (interface_type))) @interface.def

(type_declaration
  (type_spec
    name: (type_identifier) @type.name)) @type.def

(const_declaration
  (const_spec
    name: (identifier) @const.name)) @const.def

(var_declaration
  (var_spec
    name: (identifier) @var.name)) @var.def

(var_declaration
  (var_spec_list
    (var_spec
      name: (identifier) @var.name) @var.def))
"#;

pub const CALLERS_QUERY: &str = r#"
(call_expression
  function: (identifier) @callee)

(call_expression
  function: (selector_expression
    field: (field_identifier) @callee))
"#;

pub const VARIABLES_QUERY: &str = r#"
(short_var_declaration
  left: (expression_list
    (identifier) @var.name))

(var_declaration
  (var_spec
    name: (identifier) @var.name))

(range_clause
  left: (expression_list
    (identifier) @var.name))

(parameter_declaration
  name: (identifier) @var.name)
"#;

pub const IMPORTS_QUERY: &str = r#"
(import_declaration
  (import_spec
    path: (interpreted_string_literal) @import.source))

(import_declaration
  (import_spec_list
    (import_spec
      path: (interpreted_string_literal) @import.source)))
"#;

pub fn config() -> LanguageConfig {
    LanguageConfig {
        language: tree_sitter_go::LANGUAGE.into(),
        symbols_query: SYMBOLS_QUERY,
        callers_query: CALLERS_QUERY,
        variables_query: VARIABLES_QUERY,
        imports_query: IMPORTS_QUERY,
        test_patterns: vec![
            TestPattern::FunctionPrefix("Test"),
            TestPattern::FileEndsWith("_test.go"),
        ],
    }
}
