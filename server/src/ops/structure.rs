use crate::index::file_entry::FileMark;
use crate::index::file_tree::FileTree;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct StructureResponse {
    pub tree: String,
    pub file_count: usize,
    pub language_breakdown: Vec<LanguageCount>,
}

#[derive(Debug, Serialize)]
pub struct LanguageCount {
    pub language: String,
    pub count: usize,
}

pub fn get_structure(file_tree: &Arc<FileTree>, depth: usize) -> StructureResponse {
    let tree = file_tree.render_tree(depth);
    let file_count = file_tree.len();
    let breakdown = file_tree
        .language_breakdown()
        .into_iter()
        .map(|b| LanguageCount {
            language: format!("{:?}", b.language).to_lowercase(),
            count: b.count,
        })
        .collect();

    StructureResponse {
        tree,
        file_count,
        language_breakdown: breakdown,
    }
}

pub fn define_file(file_tree: &Arc<FileTree>, file: &str, definition: &str) -> Result<(), String> {
    if let Some(mut entry) = file_tree.files.get_mut(file) {
        if entry.definition.is_some() {
            return Err(format!(
                "File '{}' already has a definition. Use redefine to update it.",
                file
            ));
        }
        entry.definition = Some(definition.to_string());
        Ok(())
    } else {
        Err(format!("File '{}' not found in index", file))
    }
}

pub fn redefine_file(
    file_tree: &Arc<FileTree>,
    file: &str,
    definition: &str,
) -> Result<(), String> {
    if let Some(mut entry) = file_tree.files.get_mut(file) {
        entry.definition = Some(definition.to_string());
        Ok(())
    } else {
        Err(format!("File '{}' not found in index", file))
    }
}

pub fn mark_file(file_tree: &Arc<FileTree>, file: &str, mark_str: &str) -> Result<(), String> {
    let mark = FileMark::from_str(mark_str)
        .ok_or_else(|| format!("Unknown mark type: '{}'. Valid: documentation, ignore, test, config, generated, custom", mark_str))?;

    if let Some(mut entry) = file_tree.files.get_mut(file) {
        if !entry.marks.contains(&mark) {
            entry.marks.push(mark);
        }
        Ok(())
    } else {
        Err(format!("File '{}' not found in index", file))
    }
}
