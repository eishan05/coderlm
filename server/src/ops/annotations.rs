use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::index::file_entry::FileMark;
use crate::index::file_tree::FileTree;
use crate::symbols::SymbolTable;

const ANNOTATIONS_FILE: &str = ".coderlm/annotations.json";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct AnnotationData {
    /// File definitions: rel_path -> definition string
    #[serde(default)]
    pub file_definitions: HashMap<String, String>,
    /// File marks: rel_path -> list of mark strings
    #[serde(default)]
    pub file_marks: HashMap<String, Vec<String>>,
    /// Symbol definitions: "file::name::line" -> definition string
    #[serde(default)]
    pub symbol_definitions: HashMap<String, String>,
}

/// Save all annotations (file definitions, marks, symbol definitions)
/// to `.coderlm/annotations.json` in the project root.
pub fn save_annotations(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
) -> Result<(), String> {
    let mut data = AnnotationData::default();

    // Collect file definitions and marks
    for entry in file_tree.files.iter() {
        let fe = entry.value();
        if let Some(def) = &fe.definition {
            data.file_definitions
                .insert(fe.rel_path.clone(), def.clone());
        }
        if !fe.marks.is_empty() {
            let mark_strs: Vec<String> = fe
                .marks
                .iter()
                .map(|m| format!("{:?}", m).to_lowercase())
                .collect();
            data.file_marks.insert(fe.rel_path.clone(), mark_strs);
        }
    }

    // Collect symbol definitions using new key format (file::name::line)
    for entry in symbol_table.symbols.iter() {
        let sym = entry.value();
        if let Some(def) = &sym.definition {
            let key = SymbolTable::make_key(&sym.file, &sym.name, sym.line_range.0);
            data.symbol_definitions.insert(key, def.clone());
        }
    }

    let annotations_path = root.join(ANNOTATIONS_FILE);
    if let Some(parent) = annotations_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create annotations dir: {}", e))?;
    }

    let json = serde_json::to_string_pretty(&data)
        .map_err(|e| format!("Failed to serialize annotations: {}", e))?;
    std::fs::write(&annotations_path, json)
        .map_err(|e| format!("Failed to write annotations: {}", e))?;

    debug!(
        "Saved annotations: {} file defs, {} file marks, {} symbol defs",
        data.file_definitions.len(),
        data.file_marks.len(),
        data.symbol_definitions.len()
    );

    Ok(())
}

/// Load annotations from `.coderlm/annotations.json` and apply them
/// to the file tree and symbol table.
///
/// Supports both new key format ("file::name::line") and legacy format
/// ("file::name") for backward compatibility with existing annotation files.
pub fn load_annotations(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
) -> Result<AnnotationData, String> {
    let annotations_path = root.join(ANNOTATIONS_FILE);
    if !annotations_path.exists() {
        return Ok(AnnotationData::default());
    }

    let json = std::fs::read_to_string(&annotations_path)
        .map_err(|e| format!("Failed to read annotations: {}", e))?;
    let data: AnnotationData = serde_json::from_str(&json)
        .map_err(|e| format!("Failed to parse annotations: {}", e))?;

    // Apply file definitions
    for (path, def) in &data.file_definitions {
        if let Some(mut entry) = file_tree.files.get_mut(path.as_str()) {
            entry.definition = Some(def.clone());
        } else {
            debug!("Annotation for missing file: {}", path);
        }
    }

    // Apply file marks
    for (path, marks) in &data.file_marks {
        if let Some(mut entry) = file_tree.files.get_mut(path.as_str()) {
            for mark_str in marks {
                if let Some(mark) = FileMark::from_str(mark_str) {
                    if !entry.marks.contains(&mark) {
                        entry.marks.push(mark);
                    }
                } else {
                    warn!("Unknown mark '{}' for file '{}'", mark_str, path);
                }
            }
        }
    }

    // Apply symbol definitions — supports both new (file::name::line) and
    // legacy (file::name) key formats. When a new-format key misses (e.g.,
    // because the symbol moved lines), falls back to file+name matching.
    for (key, def) in &data.symbol_definitions {
        if let Some(mut sym) = symbol_table.symbols.get_mut(key) {
            // Exact match on new-format key
            sym.definition = Some(def.clone());
        } else {
            // Key didn't match exactly. Try to recover by extracting file and
            // name, then searching for matching symbols.
            let file_and_name = parse_legacy_key(key).or_else(|| parse_new_format_key(key));
            if let Some((file, name)) = file_and_name {
                let matches = symbol_table.find_by_file_and_name(file, name);
                if matches.len() == 1 {
                    // Unambiguous match — apply annotation
                    let new_key = SymbolTable::make_key(
                        &matches[0].file,
                        &matches[0].name,
                        matches[0].line_range.0,
                    );
                    if let Some(mut sym) = symbol_table.symbols.get_mut(&new_key) {
                        sym.definition = Some(def.clone());
                        debug!(
                            "Annotation key '{}' relocated to line {}",
                            key, matches[0].line_range.0
                        );
                    }
                } else if matches.is_empty() {
                    debug!("Annotation for missing symbol: {}", key);
                } else {
                    debug!(
                        "Annotation key '{}' is ambiguous ({} matches), skipping. \
                         Re-save annotations to update keys.",
                        key,
                        matches.len()
                    );
                }
            } else {
                debug!("Annotation for missing symbol: {}", key);
            }
        }
    }

    debug!(
        "Loaded annotations: {} file defs, {} file marks, {} symbol defs",
        data.file_definitions.len(),
        data.file_marks.len(),
        data.symbol_definitions.len()
    );

    Ok(data)
}

/// Parse a legacy key ("file::name") into (file, name).
/// Returns None if the key doesn't contain "::".
///
/// Note: This splits on the first "::" occurrence, so file paths
/// cannot contain "::" (which is not valid in typical file paths anyway).
fn parse_legacy_key(key: &str) -> Option<(&str, &str)> {
    // A new-format key has the pattern "file::name::line" where line is a number.
    // We need to distinguish new-format from legacy.
    // Strategy: split on "::" and check if the last part is a valid line number.
    let parts: Vec<&str> = key.splitn(3, "::").collect();
    match parts.len() {
        2 => {
            // Legacy format: "file::name"
            Some((parts[0], parts[1]))
        }
        3 => {
            // Could be new format "file::name::line" or legacy with "::" in file path.
            // If the last part is a number, it's new format — don't treat as legacy.
            if parts[2].parse::<usize>().is_ok() {
                None // New format key, not legacy
            } else {
                // Unusual case — treat first part as file, rest as name
                Some((parts[0], parts[1]))
            }
        }
        _ => None,
    }
}

/// Parse a new-format key ("file::name::line") into (file, name).
/// Returns None if the key is not in new format (i.e., the last part is not a number).
fn parse_new_format_key(key: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = key.splitn(3, "::").collect();
    if parts.len() == 3 && parts[2].parse::<usize>().is_ok() {
        Some((parts[0], parts[1]))
    } else {
        None
    }
}
