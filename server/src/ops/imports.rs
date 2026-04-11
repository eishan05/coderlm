use serde_json::{Value, json};

use crate::symbols::ImportTable;

/// Get the imports for a specific file.
///
/// If `file_tree` is provided, validates that the file exists in the index
/// before returning results. Pass `None` to skip validation (backward compat).
pub fn get_imports(
    import_table: &ImportTable,
    file: &str,
    file_tree: Option<&crate::index::file_tree::FileTree>,
) -> Result<Value, String> {
    if let Some(ft) = file_tree {
        if ft.get(file).is_none() {
            return Err(format!("File '{}' not found in index", file));
        }
    }
    let imports = import_table.get_imports(file);
    Ok(json!({
        "file": file,
        "imports": imports.iter().map(|i| json!({
            "source": i.source,
            "line": i.line,
        })).collect::<Vec<_>>(),
        "count": imports.len(),
    }))
}

/// Find files that depend on / import a given module.
///
/// In addition to the original substring match on the raw query, this also
/// converts file-like paths to language-specific module paths so that e.g.
/// `src/config.rs` matches the Rust import source `crate::config`, or
/// `src/utils/helpers.py` matches the Python import `utils.helpers`.
pub fn get_dependents(import_table: &ImportTable, file: &str) -> Result<Value, String> {
    if file.is_empty() {
        return Err("File path must not be empty".to_string());
    }
    // Start with the original substring-based lookup
    let mut result_set: std::collections::HashSet<String> =
        import_table.get_dependents(file).into_iter().collect();

    // Generate alternative module path forms from the file path and search those too
    let module_paths = file_to_module_paths(file);
    for module_path in &module_paths {
        for dep in import_table.get_dependents(module_path) {
            result_set.insert(dep);
        }
    }

    let mut dependents: Vec<String> = result_set.into_iter().collect();
    dependents.sort();

    Ok(json!({
        "query": file,
        "dependents": dependents,
        "count": dependents.len(),
    }))
}

/// Convert a file path to possible module path forms used in import statements.
///
/// Examples:
/// - `src/config.rs` -> `["crate::config", "config"]`
/// - `src/config/mod.rs` -> `["crate::config", "config"]`
/// - `src/server/state.rs` -> `["crate::server::state", "server::state", "server.state"]`
/// - `src/utils/helpers.py` -> `["utils.helpers", "utils/helpers"]`
/// - `src/utils/__init__.py` -> `["utils"]`
/// - `clients/clob` -> `["crate::clients::clob", "clients::clob", "clients.clob", "clients/clob"]`
fn file_to_module_paths(file: &str) -> Vec<String> {
    let mut paths = Vec::new();

    // Normalize path separators
    let file = file.replace('\\', "/");

    // Strip file extension and handle special files
    let module_base = if file.ends_with("/mod.rs") || file.ends_with("/__init__.py") {
        // mod.rs / __init__.py -> the directory is the module
        file.rsplitn(2, '/').nth(1).unwrap_or("").to_string()
    } else if let Some(without_ext) = file
        .strip_suffix(".rs")
        .or_else(|| file.strip_suffix(".py"))
        .or_else(|| file.strip_suffix(".ts"))
        .or_else(|| file.strip_suffix(".tsx"))
        .or_else(|| file.strip_suffix(".js"))
        .or_else(|| file.strip_suffix(".jsx"))
        .or_else(|| file.strip_suffix(".go"))
        .or_else(|| file.strip_suffix(".java"))
        .or_else(|| file.strip_suffix(".scala"))
    {
        let parts: Vec<&str> = without_ext.rsplitn(2, '/').collect();
        if parts.len() == 2 {
            format!("{}/{}", parts[1], parts[0])
        } else {
            parts[0].to_string()
        }
    } else if is_extensionless_path_query(&file) {
        file
    } else {
        return paths;
    };

    // Strip leading `src/` if present (common Rust convention)
    let mut module_path = module_base.strip_prefix("src/").unwrap_or(&module_base);
    while let Some(stripped) = module_path
        .strip_prefix("./")
        .or_else(|| module_path.strip_prefix("../"))
    {
        module_path = stripped;
    }

    // Rust-style: crate::module::submodule
    let rust_module = module_path.replace('/', "::");
    if !rust_module.is_empty() {
        paths.push(format!("crate::{}", rust_module));
        paths.push(rust_module.clone());
    }

    // Python-style: module.submodule
    let python_module = module_path.replace('/', ".");
    if !python_module.is_empty() && python_module != rust_module {
        paths.push(python_module);
    }

    // Also add the raw path-style without src/ (for JS/TS relative imports)
    if !module_path.is_empty() {
        paths.push(module_path.to_string());
    }

    paths
}

fn is_extensionless_path_query(file: &str) -> bool {
    let last_segment = file.rsplit('/').next().unwrap_or("");
    file.contains('/') && !last_segment.is_empty() && !last_segment.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::ImportEntry;

    #[test]
    fn test_get_imports_returns_file_imports() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/main.py",
            vec![
                ImportEntry {
                    source: "os".to_string(),
                    line: 1,
                },
                ImportEntry {
                    source: "sys".to_string(),
                    line: 2,
                },
            ],
        );

        let result = get_imports(&table, "src/main.py", None).unwrap();
        assert_eq!(result["file"], "src/main.py");
        assert_eq!(result["count"], 2);
        assert_eq!(result["imports"][0]["source"], "os");
        assert_eq!(result["imports"][1]["source"], "sys");
    }

    #[test]
    fn test_get_imports_empty_for_unknown_file() {
        let table = ImportTable::new();
        // Without file_tree validation, returns empty
        let result = get_imports(&table, "nonexistent.py", None).unwrap();
        assert_eq!(result["count"], 0);
        assert!(result["imports"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_get_dependents_finds_importing_files() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/a.py",
            vec![ImportEntry {
                source: "utils.helpers".to_string(),
                line: 1,
            }],
        );
        table.insert_file_imports(
            "src/b.py",
            vec![ImportEntry {
                source: "utils.helpers".to_string(),
                line: 1,
            }],
        );
        table.insert_file_imports(
            "src/c.py",
            vec![ImportEntry {
                source: "other".to_string(),
                line: 1,
            }],
        );

        let result = get_dependents(&table, "utils").unwrap();
        assert_eq!(result["count"], 2);
        let deps = result["dependents"].as_array().unwrap();
        let dep_strs: Vec<&str> = deps.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(dep_strs.contains(&"src/a.py"));
        assert!(dep_strs.contains(&"src/b.py"));
    }

    #[test]
    fn test_get_dependents_empty() {
        let table = ImportTable::new();
        let result = get_dependents(&table, "nonexistent").unwrap();
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn test_get_dependents_rejects_empty_string() {
        let table = ImportTable::new();
        let result = get_dependents(&table, "");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn test_get_dependents_rust_crate_import_matches_file_path() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/server/routes.rs",
            vec![ImportEntry {
                source: "crate::config".to_string(),
                line: 5,
            }],
        );
        table.insert_file_imports(
            "src/main.rs",
            vec![ImportEntry {
                source: "crate::config".to_string(),
                line: 3,
            }],
        );

        // Query by file path should find files importing crate::config
        let result = get_dependents(&table, "src/config.rs").unwrap();
        let count = result["count"].as_u64().unwrap();
        assert!(count >= 2, "Expected at least 2 dependents, got {}", count);
        let deps = result["dependents"].as_array().unwrap();
        let dep_strs: Vec<&str> = deps.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(dep_strs.contains(&"src/server/routes.rs"));
        assert!(dep_strs.contains(&"src/main.rs"));
    }

    #[test]
    fn test_get_dependents_rust_nested_module_path() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/ops/content.rs",
            vec![ImportEntry {
                source: "crate::server::state".to_string(),
                line: 2,
            }],
        );

        let result = get_dependents(&table, "src/server/state.rs").unwrap();
        assert_eq!(result["count"], 1);
        let deps = result["dependents"].as_array().unwrap();
        assert_eq!(deps[0].as_str().unwrap(), "src/ops/content.rs");
    }

    #[test]
    fn test_get_dependents_extensionless_slash_query_matches_dotted_import() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/market.py",
            vec![ImportEntry {
                source: "xanbot_polymarket.clients.clob".to_string(),
                line: 1,
            }],
        );

        let result = get_dependents(&table, "clients/clob").unwrap();
        assert_eq!(result["count"], 1);
        let deps = result["dependents"].as_array().unwrap();
        assert_eq!(deps[0].as_str().unwrap(), "src/market.py");
    }

    #[test]
    fn test_get_dependents_relative_slash_query_matches_normalized_import() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/main.py",
            vec![ImportEntry {
                source: "utils".to_string(),
                line: 1,
            }],
        );

        let result = get_dependents(&table, "./utils").unwrap();
        assert_eq!(result["count"], 1);
        let deps = result["dependents"].as_array().unwrap();
        assert_eq!(deps[0].as_str().unwrap(), "src/main.py");
    }

    #[test]
    fn test_file_to_module_paths_rust_src_file() {
        let paths = file_to_module_paths("src/config.rs");
        assert!(paths.contains(&"crate::config".to_string()));
        assert!(paths.contains(&"config".to_string()));
    }

    #[test]
    fn test_file_to_module_paths_rust_mod_rs() {
        let paths = file_to_module_paths("src/server/mod.rs");
        assert!(paths.contains(&"crate::server".to_string()));
    }

    #[test]
    fn test_file_to_module_paths_python() {
        let paths = file_to_module_paths("src/utils/helpers.py");
        assert!(paths.contains(&"utils.helpers".to_string()));
    }

    #[test]
    fn test_file_to_module_paths_python_init() {
        let paths = file_to_module_paths("src/utils/__init__.py");
        assert!(paths.contains(&"crate::utils".to_string()));
    }

    #[test]
    fn test_file_to_module_paths_unknown_extension() {
        let paths = file_to_module_paths("src/readme.txt");
        assert!(paths.is_empty());
    }
}
