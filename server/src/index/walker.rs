use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

use crate::config;
use crate::index::file_entry::FileEntry;
use crate::index::file_tree::FileTree;

/// Scan the codebase directory using the `ignore` crate (respects .gitignore)
/// plus our built-in ignore patterns. Returns the number of files indexed.
pub fn scan_directory(root: &Path, file_tree: &Arc<FileTree>, max_file_size: u64) -> Result<usize> {
    let walker = WalkBuilder::new(root)
        .hidden(true) // skip dotfiles by default
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(config::CODERLM_IGNORE_FILENAME)
        .build();

    let mut count = 0;

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip directories
        if entry.file_type().map_or(true, |ft| ft.is_dir()) {
            continue;
        }

        // Skip symlinks to prevent indexing files outside the project root
        if entry.path_is_symlink() {
            continue;
        }

        let path = entry.path();

        // Get the relative path
        let rel_path = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Apply our additional ignore rules
        if should_skip(&rel_path) {
            continue;
        }

        // Check extension-based ignoring
        if config::should_ignore_extension(&rel_path) {
            continue;
        }

        // Get file metadata
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let size = metadata.len();

        let modified: DateTime<Utc> = metadata
            .modified()
            .map(DateTime::from)
            .unwrap_or_else(|_| Utc::now());

        let mut file_entry = FileEntry::new(rel_path, size, modified);

        // Files over the size limit are still listed in the tree so agents
        // can see they exist, but they are flagged as oversized and will
        // not be parsed for symbols.
        if size > max_file_size {
            file_entry.oversized = true;
        }

        file_tree.insert(file_entry);
        count += 1;
    }

    info!("Scanned {} files from {}", count, root.display());
    Ok(count)
}

/// Check if any path component matches our built-in ignore directories.
fn should_skip(rel_path: &str) -> bool {
    for component in rel_path.split('/') {
        if config::should_ignore_dir(component) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_oversized_file_listed_in_tree() {
        let dir = tempfile::tempdir().unwrap();
        // Create a small file (under limit)
        let small_path = dir.path().join("small.rs");
        std::fs::write(&small_path, "fn main() {}").unwrap();

        // Create an oversized file (over limit)
        let big_path = dir.path().join("big.rs");
        let mut f = std::fs::File::create(&big_path).unwrap();
        // Write 101 bytes to exceed a 100-byte limit
        f.write_all(&vec![b'x'; 101]).unwrap();

        let file_tree = Arc::new(FileTree::new());
        let count = scan_directory(dir.path(), &file_tree, 100).unwrap();

        // Both files should be in the tree
        assert!(file_tree.get("small.rs").is_some(), "small file should be in tree");
        assert!(file_tree.get("big.rs").is_some(), "oversized file should be in tree");

        // Count should include both files
        assert_eq!(count, 2);
    }

    #[test]
    fn test_oversized_file_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let small_path = dir.path().join("small.rs");
        std::fs::write(&small_path, "fn main() {}").unwrap();

        let big_path = dir.path().join("big.rs");
        let mut f = std::fs::File::create(&big_path).unwrap();
        f.write_all(&vec![b'x'; 101]).unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 100).unwrap();

        let small_entry = file_tree.get("small.rs").unwrap();
        assert!(!small_entry.oversized, "small file should not be flagged oversized");

        let big_entry = file_tree.get("big.rs").unwrap();
        assert!(big_entry.oversized, "oversized file should be flagged");
    }

    #[test]
    fn test_normal_files_not_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("normal.py");
        std::fs::write(&path, "print('hello')").unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        let entry = file_tree.get("normal.py").unwrap();
        assert!(!entry.oversized);
    }

    #[test]
    fn test_file_exactly_at_limit_not_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact.rs");
        // Write exactly 100 bytes
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&vec![b'a'; 100]).unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 100).unwrap();

        let entry = file_tree.get("exact.rs").unwrap();
        assert!(!entry.oversized, "file exactly at limit should not be flagged oversized");
    }

    // ---- Tests for .coderlmignore support ----

    #[test]
    fn test_coderlmignore_excludes_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        // Create a .coderlmignore that excludes the "generated/" directory
        std::fs::write(
            dir.path().join(config::CODERLM_IGNORE_FILENAME),
            "generated/\n",
        ).unwrap();

        // Create files: one inside generated/, one outside
        let gen_dir = dir.path().join("generated");
        std::fs::create_dir_all(&gen_dir).unwrap();
        std::fs::write(gen_dir.join("proto.rs"), "// generated").unwrap();
        std::fs::write(dir.path().join("src.rs"), "fn main() {}").unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        assert!(file_tree.get("src.rs").is_some(), "Non-ignored file should be indexed");
        assert!(file_tree.get("generated/proto.rs").is_none(),
            "File in .coderlmignore'd directory should not be indexed");
    }

    #[test]
    fn test_coderlmignore_glob_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(config::CODERLM_IGNORE_FILENAME),
            "*.pb.go\n",
        ).unwrap();

        std::fs::write(dir.path().join("service.pb.go"), "package api").unwrap();
        std::fs::write(dir.path().join("service.go"), "package api").unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        assert!(file_tree.get("service.go").is_some(), "Non-matching file should be indexed");
        assert!(file_tree.get("service.pb.go").is_none(),
            "File matching .coderlmignore glob should not be indexed");
    }

    #[test]
    fn test_coderlmignore_negation_pattern() {
        let dir = tempfile::tempdir().unwrap();
        // Ignore all .snap files except important.snap
        std::fs::write(
            dir.path().join(config::CODERLM_IGNORE_FILENAME),
            "*.snap\n!important.snap\n",
        ).unwrap();

        std::fs::write(dir.path().join("important.snap"), "keep this").unwrap();
        std::fs::write(dir.path().join("junk.snap"), "discard this").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn lib() {}").unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        assert!(file_tree.get("important.snap").is_some(),
            "Negated file should be indexed");
        assert!(file_tree.get("junk.snap").is_none(),
            "Non-negated file matching ignore glob should not be indexed");
        assert!(file_tree.get("lib.rs").is_some(),
            "Unrelated file should be indexed");
    }

    #[test]
    fn test_no_coderlmignore_indexes_everything() {
        let dir = tempfile::tempdir().unwrap();
        // No .coderlmignore file
        std::fs::write(dir.path().join("src.rs"), "fn main() {}").unwrap();

        let generated_dir = dir.path().join("mydir");
        std::fs::create_dir_all(&generated_dir).unwrap();
        std::fs::write(generated_dir.join("file.rs"), "fn f() {}").unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        assert!(file_tree.get("src.rs").is_some());
        assert!(file_tree.get("mydir/file.rs").is_some(),
            "Without .coderlmignore all non-gitignored files should be indexed");
    }

    #[test]
    fn test_coderlmignore_multiple_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(config::CODERLM_IGNORE_FILENAME),
            "# Comment line\nfixtures/\n*.snap\n",
        ).unwrap();

        let fixtures_dir = dir.path().join("fixtures");
        std::fs::create_dir_all(&fixtures_dir).unwrap();
        std::fs::write(fixtures_dir.join("data.json"), "{}").unwrap();
        std::fs::write(dir.path().join("output.snap"), "snapshot").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn lib() {}").unwrap();

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        assert!(file_tree.get("lib.rs").is_some(), "Normal file should be indexed");
        assert!(file_tree.get("fixtures/data.json").is_none(),
            "File in ignored directory should not be indexed");
        assert!(file_tree.get("output.snap").is_none(),
            "File matching ignored glob should not be indexed");
    }

    #[test]
    fn test_symlinks_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // Create a normal file
        std::fs::write(dir.path().join("real.rs"), "fn main() {}").unwrap();

        // Create a symlink pointing to a file outside the project
        // (we'll just point to the real file to test skipping; the key
        //  is that the entry IS a symlink)
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                dir.path().join("real.rs"),
                dir.path().join("link.rs"),
            )
            .unwrap();
        }

        let file_tree = Arc::new(FileTree::new());
        scan_directory(dir.path(), &file_tree, 1_000_000).unwrap();

        assert!(
            file_tree.get("real.rs").is_some(),
            "Regular file should be indexed"
        );

        #[cfg(unix)]
        assert!(
            file_tree.get("link.rs").is_none(),
            "Symlink should NOT be indexed"
        );
    }
}
