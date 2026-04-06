use serde_json::{json, Value};

use crate::symbols::ImportTable;

/// Get the imports for a specific file.
pub fn get_imports(import_table: &ImportTable, file: &str) -> Value {
    let imports = import_table.get_imports(file);
    json!({
        "file": file,
        "imports": imports.iter().map(|i| json!({
            "source": i.source,
            "line": i.line,
        })).collect::<Vec<_>>(),
        "count": imports.len(),
    })
}

/// Find files that depend on / import a given module (substring match).
pub fn get_dependents(import_table: &ImportTable, file: &str) -> Value {
    let dependents = import_table.get_dependents(file);
    json!({
        "query": file,
        "dependents": dependents,
        "count": dependents.len(),
    })
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
                ImportEntry { source: "os".to_string(), line: 1 },
                ImportEntry { source: "sys".to_string(), line: 2 },
            ],
        );

        let result = get_imports(&table, "src/main.py");
        assert_eq!(result["file"], "src/main.py");
        assert_eq!(result["count"], 2);
        assert_eq!(result["imports"][0]["source"], "os");
        assert_eq!(result["imports"][1]["source"], "sys");
    }

    #[test]
    fn test_get_imports_empty_for_unknown_file() {
        let table = ImportTable::new();
        let result = get_imports(&table, "nonexistent.py");
        assert_eq!(result["count"], 0);
        assert!(result["imports"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_get_dependents_finds_importing_files() {
        let table = ImportTable::new();
        table.insert_file_imports(
            "src/a.py",
            vec![ImportEntry { source: "utils.helpers".to_string(), line: 1 }],
        );
        table.insert_file_imports(
            "src/b.py",
            vec![ImportEntry { source: "utils.helpers".to_string(), line: 1 }],
        );
        table.insert_file_imports(
            "src/c.py",
            vec![ImportEntry { source: "other".to_string(), line: 1 }],
        );

        let result = get_dependents(&table, "utils");
        assert_eq!(result["count"], 2);
        let deps = result["dependents"].as_array().unwrap();
        let dep_strs: Vec<&str> = deps.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(dep_strs.contains(&"src/a.py"));
        assert!(dep_strs.contains(&"src/b.py"));
    }

    #[test]
    fn test_get_dependents_empty() {
        let table = ImportTable::new();
        let result = get_dependents(&table, "nonexistent");
        assert_eq!(result["count"], 0);
    }
}
