use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::gitignore::Gitignore;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config;
use crate::index::file_entry::FileEntry;
use crate::index::file_tree::FileTree;
use crate::symbols::parser::extract_symbols_from_file;
use crate::symbols::SymbolTable;

/// A file-change event sent from the watcher callback to the async processor.
#[derive(Debug, Clone)]
enum WatcherEvent {
    /// A file was created or modified.
    Changed {
        root: PathBuf,
        rel_path: String,
        abs_path: PathBuf,
    },
    /// A file was deleted.
    Deleted {
        rel_path: String,
    },
}

/// Start the filesystem watcher. Returns a handle that keeps the watcher alive.
/// Drop the handle to stop watching.
///
/// File change events are sent through an internal channel and processed
/// asynchronously on a Tokio task, so the watcher callback is never blocked
/// by slow symbol extraction.
pub fn start_watcher(
    root: &Path,
    file_tree: Arc<FileTree>,
    symbol_table: Arc<SymbolTable>,
    max_file_size: u64,
) -> Result<WatcherHandle> {
    let root_buf = root.to_path_buf();
    let root_for_handler = root_buf.clone();

    // Load .coderlmignore patterns (if present) so the watcher can skip
    // files that are excluded by project-specific ignore rules.
    let coderlm_ignore = Arc::new(config::load_coderlm_ignore(root));
    let ignore_for_handler = coderlm_ignore.clone();

    // Channel to decouple the watcher callback from async processing.
    // The bounded channel provides backpressure if the processor falls behind.
    let (tx, rx) = mpsc::channel::<Vec<WatcherEvent>>(64);

    // Spawn the async processor that drains the channel and handles events.
    let rt = tokio::runtime::Handle::current();
    rt.spawn(process_events(rx, file_tree.clone(), symbol_table.clone(), max_file_size, coderlm_ignore));

    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        move |result: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            match result {
                Ok(events) => {
                    let watcher_events = collect_events(
                        &root_for_handler,
                        max_file_size,
                        &ignore_for_handler,
                        events,
                    );
                    if !watcher_events.is_empty() {
                        // Non-blocking send; if the channel is full we drop this batch.
                        // This is acceptable: a subsequent change event will re-trigger.
                        if let Err(e) = tx.try_send(watcher_events) {
                            warn!("Watcher channel full or closed, dropping events: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Filesystem watcher error: {}", e);
                }
            }
        },
    )?;

    debouncer
        .watcher()
        .watch(&root_buf, notify::RecursiveMode::Recursive)?;

    info!("Filesystem watcher started for {}", root_buf.display());

    Ok(WatcherHandle {
        _debouncer: Some(debouncer),
    })
}

pub struct WatcherHandle {
    _debouncer: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
}

/// Collect raw debounced events into typed `WatcherEvent`s.
/// This runs in the watcher callback and does minimal work — no parsing,
/// no heavy I/O — just classification.
fn collect_events(
    root: &Path,
    _max_file_size: u64,
    coderlm_ignore: &Gitignore,
    events: Vec<notify_debouncer_mini::DebouncedEvent>,
) -> Vec<WatcherEvent> {
    let mut watcher_events = Vec::new();

    for event in events {
        let path = &event.path;

        // Get relative path
        let rel_path = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Skip ignored paths (hardcoded patterns)
        if should_skip(&rel_path) {
            continue;
        }

        // Skip paths matched by .coderlmignore
        if coderlm_ignore
            .matched_path_or_any_parents(&rel_path, path.is_dir())
            .is_ignore()
        {
            continue;
        }

        match event.kind {
            DebouncedEventKind::Any => {
                if path.is_file() {
                    watcher_events.push(WatcherEvent::Changed {
                        root: root.to_path_buf(),
                        rel_path,
                        abs_path: path.clone(),
                    });
                } else if !path.exists() {
                    watcher_events.push(WatcherEvent::Deleted { rel_path });
                }
            }
            DebouncedEventKind::AnyContinuous => {
                // Ignore continuous events (they'll be followed by a final Any)
            }
            _ => {}
        }
    }

    watcher_events
}

/// Async task that processes watcher events from the channel.
/// Symbol extraction is offloaded to `spawn_blocking` so it doesn't
/// block the Tokio runtime.
async fn process_events(
    mut rx: mpsc::Receiver<Vec<WatcherEvent>>,
    file_tree: Arc<FileTree>,
    symbol_table: Arc<SymbolTable>,
    max_file_size: u64,
    coderlm_ignore: Arc<Gitignore>,
) {
    while let Some(events) = rx.recv().await {
        for event in events {
            match event {
                WatcherEvent::Changed { root, rel_path, abs_path } => {
                    handle_file_change(
                        &root,
                        &file_tree,
                        &symbol_table,
                        max_file_size,
                        &coderlm_ignore,
                        &rel_path,
                        &abs_path,
                    ).await;
                }
                WatcherEvent::Deleted { rel_path } => {
                    handle_file_delete(&file_tree, &symbol_table, &rel_path);
                }
            }
        }
    }
    debug!("Watcher event processor shutting down");
}

/// Handle a file change: update the file tree entry, then spawn symbol
/// re-extraction on a blocking thread so the event loop stays responsive.
async fn handle_file_change(
    root: &Path,
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
    max_file_size: u64,
    coderlm_ignore: &Gitignore,
    rel_path: &str,
    abs_path: &Path,
) {
    // Check extension-based ignoring
    if config::should_ignore_extension(rel_path) {
        return;
    }

    // Check .coderlmignore patterns
    if coderlm_ignore
        .matched_path_or_any_parents(rel_path, false)
        .is_ignore()
    {
        return;
    }

    let metadata = match std::fs::metadata(abs_path) {
        Ok(m) => m,
        Err(_) => return,
    };

    let size = metadata.len();

    let modified: DateTime<Utc> = metadata
        .modified()
        .map(DateTime::from)
        .unwrap_or_else(|_| Utc::now());

    // Update file tree — oversized files are listed but flagged
    let mut entry = FileEntry::new(rel_path.to_string(), size, modified);
    let language = entry.language;
    let is_oversized = size > max_file_size;
    entry.oversized = is_oversized;
    file_tree.insert(entry);

    // Remove old symbols for this file
    symbol_table.remove_file(rel_path);

    // Skip symbol extraction for oversized files
    if is_oversized {
        return;
    }

    // Re-extract symbols on a blocking thread so we don't block the
    // async event processor (tree-sitter parsing is CPU-bound).
    if language.has_tree_sitter_support() {
        let root = root.to_path_buf();
        let rel_path = rel_path.to_string();
        let file_tree = file_tree.clone();
        let symbol_table = symbol_table.clone();

        tokio::task::spawn_blocking(move || {
            match extract_symbols_from_file(&root, &rel_path, language) {
                Ok(symbols) => {
                    let count = symbols.len();
                    for sym in symbols {
                        symbol_table.insert(sym);
                    }
                    if let Some(mut entry) = file_tree.files.get_mut(&rel_path) {
                        entry.symbols_extracted = true;
                    }
                    debug!("Re-extracted {} symbols from {}", count, rel_path);
                }
                Err(e) => {
                    debug!("Failed to re-extract symbols from {}: {}", rel_path, e);
                }
            }
        }).await.unwrap_or_else(|e| {
            warn!("Symbol extraction task panicked: {}", e);
        });
    }
}

fn handle_file_delete(
    file_tree: &Arc<FileTree>,
    symbol_table: &Arc<SymbolTable>,
    rel_path: &str,
) {
    if file_tree.remove(rel_path).is_some() {
        symbol_table.remove_file(rel_path);
        debug!("Removed {} from index", rel_path);
    }
}

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
    use crate::index::file_entry::Language;
    use crate::symbols::SymbolTable;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper: create a temp directory with a Rust file for testing.
    fn setup_test_dir() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn create_rust_file(root: &Path, rel_path: &str, content: &str) {
        let abs = root.join(rel_path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&abs).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    /// Return an empty Gitignore matcher for tests that don't need custom ignore rules.
    fn empty_ignore() -> Gitignore {
        Gitignore::empty()
    }

    // ---- Tests for collect_events ----

    #[test]
    fn test_collect_events_classifies_existing_file_as_changed() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "src/main.rs", "fn main() {}");

        let abs_path = root.join("src/main.rs");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path.clone(),
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &empty_ignore(), events);
        assert_eq!(result.len(), 1);
        match &result[0] {
            WatcherEvent::Changed { rel_path, .. } => {
                assert_eq!(rel_path, "src/main.rs");
            }
            _ => panic!("Expected Changed event"),
        }
    }

    #[test]
    fn test_collect_events_classifies_nonexistent_file_as_deleted() {
        let (_dir, root) = setup_test_dir();
        // File does not exist on disk
        let abs_path = root.join("src/gone.rs");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path.clone(),
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &empty_ignore(), events);
        assert_eq!(result.len(), 1);
        match &result[0] {
            WatcherEvent::Deleted { rel_path } => {
                assert_eq!(rel_path, "src/gone.rs");
            }
            _ => panic!("Expected Deleted event"),
        }
    }

    #[test]
    fn test_collect_events_skips_ignored_dirs() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "node_modules/foo.js", "var x = 1;");

        let abs_path = root.join("node_modules/foo.js");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path,
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &empty_ignore(), events);
        assert!(result.is_empty(), "Events in ignored dirs should be skipped");
    }

    #[test]
    fn test_collect_events_ignores_any_continuous() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "lib.rs", "fn foo() {}");

        let abs_path = root.join("lib.rs");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path,
            kind: DebouncedEventKind::AnyContinuous,
        }];

        let result = collect_events(&root, 1024 * 1024, &empty_ignore(), events);
        assert!(result.is_empty(), "AnyContinuous events should be ignored");
    }

    #[test]
    fn test_collect_events_skips_paths_outside_root() {
        let (_dir, root) = setup_test_dir();
        let outside = PathBuf::from("/some/other/path/file.rs");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: outside,
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &empty_ignore(), events);
        assert!(result.is_empty());
    }

    // ---- Async tests for process_events / handle_file_change ----

    #[tokio::test]
    async fn test_handle_file_change_updates_file_tree() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "src/lib.rs", "pub fn hello() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &empty_ignore(),
            "src/lib.rs",
            &root.join("src/lib.rs"),
        ).await;

        // File tree should have the entry
        let entry = file_tree.get("src/lib.rs").expect("File should be in tree");
        assert_eq!(entry.language, Language::Rust);
        assert!(!entry.oversized);
    }

    #[tokio::test]
    async fn test_handle_file_change_extracts_symbols() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "src/lib.rs", "pub fn hello_world() {}\npub fn goodbye() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &empty_ignore(),
            "src/lib.rs",
            &root.join("src/lib.rs"),
        ).await;

        // Symbols should have been extracted
        let entry = file_tree.get("src/lib.rs").unwrap();
        assert!(entry.symbols_extracted, "Symbols should be marked as extracted");

        let syms = symbol_table.list_by_file("src/lib.rs");
        assert!(syms.len() >= 2, "Expected at least 2 symbols, got {}", syms.len());
    }

    #[tokio::test]
    async fn test_handle_file_change_skips_oversized_files() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "big.rs", "fn big() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        // Use a tiny max_file_size so the file is "oversized"
        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1, // 1 byte max
            &empty_ignore(),
            "big.rs",
            &root.join("big.rs"),
        ).await;

        let entry = file_tree.get("big.rs").unwrap();
        assert!(entry.oversized, "File should be marked oversized");
        assert!(!entry.symbols_extracted, "Oversized file should not have symbols extracted");
        assert_eq!(symbol_table.list_by_file("big.rs").len(), 0);
    }

    #[tokio::test]
    async fn test_handle_file_change_replaces_old_symbols() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "src/lib.rs", "pub fn alpha() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        // First change
        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &empty_ignore(),
            "src/lib.rs",
            &root.join("src/lib.rs"),
        ).await;

        let syms = symbol_table.list_by_file("src/lib.rs");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "alpha");

        // Overwrite the file with different content
        create_rust_file(&root, "src/lib.rs", "pub fn beta() {}\npub fn gamma() {}");

        // Second change
        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &empty_ignore(),
            "src/lib.rs",
            &root.join("src/lib.rs"),
        ).await;

        let syms = symbol_table.list_by_file("src/lib.rs");
        assert_eq!(syms.len(), 2, "Old symbol 'alpha' should have been removed");
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"gamma"));
        assert!(!names.contains(&"alpha"));
    }

    #[tokio::test]
    async fn test_handle_file_delete_removes_from_tree_and_symbols() {
        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        // Manually insert an entry so we can test deletion
        use crate::symbols::symbol::{Symbol, SymbolKind};
        let entry = FileEntry::new("src/gone.rs".to_string(), 100, Utc::now());
        file_tree.insert(entry);
        symbol_table.insert(Symbol {
            name: "gone_fn".to_string(),
            kind: SymbolKind::Function,
            file: "src/gone.rs".to_string(),
            byte_range: (0, 20),
            line_range: (1, 3),
            language: Language::Rust,
            signature: "fn gone_fn()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
        });

        assert!(file_tree.get("src/gone.rs").is_some());
        assert_eq!(symbol_table.list_by_file("src/gone.rs").len(), 1);

        handle_file_delete(&file_tree, &symbol_table, "src/gone.rs");

        assert!(file_tree.get("src/gone.rs").is_none());
        assert_eq!(symbol_table.list_by_file("src/gone.rs").len(), 0);
    }

    #[tokio::test]
    async fn test_process_events_handles_mixed_events() {
        let (_dir, root) = setup_test_dir();
        create_rust_file(&root, "src/a.rs", "pub fn a_func() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        // Pre-insert an entry for the file we'll "delete"
        use crate::symbols::symbol::{Symbol, SymbolKind};
        let entry = FileEntry::new("src/b.rs".to_string(), 50, Utc::now());
        file_tree.insert(entry);
        symbol_table.insert(Symbol {
            name: "b_func".to_string(),
            kind: SymbolKind::Function,
            file: "src/b.rs".to_string(),
            byte_range: (0, 15),
            line_range: (1, 1),
            language: Language::Rust,
            signature: "fn b_func()".to_string(),
            definition: None,
            parent: None,
            decorators: Vec::new(),
        });

        let (tx, rx) = mpsc::channel(64);
        let ft = file_tree.clone();
        let st = symbol_table.clone();
        let handle = tokio::spawn(process_events(rx, ft, st, 1024 * 1024, Arc::new(empty_ignore())));

        // Send a batch with one Changed and one Deleted event
        tx.send(vec![
            WatcherEvent::Changed {
                root: root.clone(),
                rel_path: "src/a.rs".to_string(),
                abs_path: root.join("src/a.rs"),
            },
            WatcherEvent::Deleted {
                rel_path: "src/b.rs".to_string(),
            },
        ]).await.unwrap();

        // Drop sender to signal the processor to exit
        drop(tx);
        handle.await.unwrap();

        // a.rs should be indexed with symbols
        assert!(file_tree.get("src/a.rs").is_some());
        let syms_a = symbol_table.list_by_file("src/a.rs");
        assert!(syms_a.len() >= 1, "a.rs should have symbols");

        // b.rs should be gone
        assert!(file_tree.get("src/b.rs").is_none());
        assert_eq!(symbol_table.list_by_file("src/b.rs").len(), 0);
    }

    #[tokio::test]
    async fn test_channel_decoupling_watcher_does_not_block() {
        // Verify that sending events through the channel returns immediately
        // even when the receiver hasn't processed them yet.
        let (tx, _rx) = mpsc::channel::<Vec<WatcherEvent>>(64);

        let start = std::time::Instant::now();

        for _ in 0..10 {
            tx.try_send(vec![WatcherEvent::Deleted {
                rel_path: "foo.rs".to_string(),
            }]).unwrap();
        }

        let elapsed = start.elapsed();
        // Sending 10 events through the channel should be nearly instant
        assert!(elapsed < Duration::from_millis(10),
            "Channel send should be non-blocking, took {:?}", elapsed);
    }

    #[test]
    fn test_should_skip_ignored_dirs() {
        assert!(should_skip("node_modules/foo.js"));
        assert!(should_skip(".git/objects/abc123"));
        assert!(should_skip("target/debug/build/foo.rs"));
        assert!(!should_skip("src/main.rs"));
        assert!(!should_skip("lib/utils.rs"));
    }

    #[tokio::test]
    async fn test_handle_file_change_skips_ignored_extension() {
        let (_dir, root) = setup_test_dir();
        // Create a .png file (should be ignored by extension)
        let abs = root.join("image.png");
        std::fs::write(&abs, b"fake png data").unwrap();

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &empty_ignore(),
            "image.png",
            &abs,
        ).await;

        // Should not be added to the file tree since it's extension-ignored
        assert!(file_tree.get("image.png").is_none(),
            "Extension-ignored files should not be added to file tree");
    }

    // ---- Tests for .coderlmignore support ----

    /// Helper: create a `.coderlmignore` file in the project root and return
    /// the loaded Gitignore matcher.
    fn create_coderlmignore(root: &Path, content: &str) -> Gitignore {
        let ignore_path = root.join(config::CODERLM_IGNORE_FILENAME);
        std::fs::write(&ignore_path, content).unwrap();
        config::load_coderlm_ignore(root)
    }

    #[test]
    fn test_collect_events_skips_coderlmignored_file() {
        let (_dir, root) = setup_test_dir();
        let gi = create_coderlmignore(&root, "generated/\n");
        create_rust_file(&root, "generated/proto.rs", "// generated code");

        let abs_path = root.join("generated/proto.rs");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path,
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &gi, events);
        assert!(result.is_empty(), "Files matching .coderlmignore should be skipped");
    }

    #[test]
    fn test_collect_events_allows_non_coderlmignored_file() {
        let (_dir, root) = setup_test_dir();
        let gi = create_coderlmignore(&root, "generated/\n");
        create_rust_file(&root, "src/main.rs", "fn main() {}");

        let abs_path = root.join("src/main.rs");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path,
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &gi, events);
        assert_eq!(result.len(), 1, "Non-ignored files should pass through");
    }

    #[test]
    fn test_collect_events_coderlmignore_glob_pattern() {
        let (_dir, root) = setup_test_dir();
        let gi = create_coderlmignore(&root, "*.pb.go\n");
        create_rust_file(&root, "api/service.pb.go", "package api");

        let abs_path = root.join("api/service.pb.go");
        let events = vec![notify_debouncer_mini::DebouncedEvent {
            path: abs_path,
            kind: DebouncedEventKind::Any,
        }];

        let result = collect_events(&root, 1024 * 1024, &gi, events);
        assert!(result.is_empty(), "Glob patterns in .coderlmignore should work");
    }

    #[test]
    fn test_collect_events_coderlmignore_negation() {
        let (_dir, root) = setup_test_dir();
        // Ignore all .snap files except important.snap
        let gi = create_coderlmignore(&root, "*.snap\n!important.snap\n");
        create_rust_file(&root, "important.snap", "keep this");
        create_rust_file(&root, "junk.snap", "discard this");

        let events = vec![
            notify_debouncer_mini::DebouncedEvent {
                path: root.join("important.snap"),
                kind: DebouncedEventKind::Any,
            },
            notify_debouncer_mini::DebouncedEvent {
                path: root.join("junk.snap"),
                kind: DebouncedEventKind::Any,
            },
        ];

        let result = collect_events(&root, 1024 * 1024, &gi, events);
        // important.snap should be whitelisted, junk.snap should be ignored
        assert_eq!(result.len(), 1, "Negation patterns should whitelist files");
        match &result[0] {
            WatcherEvent::Changed { rel_path, .. } => {
                assert_eq!(rel_path, "important.snap");
            }
            _ => panic!("Expected Changed event for important.snap"),
        }
    }

    #[tokio::test]
    async fn test_handle_file_change_skips_coderlmignored_file() {
        let (_dir, root) = setup_test_dir();
        let gi = create_coderlmignore(&root, "generated/\n");
        create_rust_file(&root, "generated/proto.rs", "pub fn generated() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &gi,
            "generated/proto.rs",
            &root.join("generated/proto.rs"),
        ).await;

        assert!(file_tree.get("generated/proto.rs").is_none(),
            "Files matching .coderlmignore should not be added to file tree");
        assert_eq!(symbol_table.list_by_file("generated/proto.rs").len(), 0,
            "Symbols should not be extracted for .coderlmignore'd files");
    }

    #[tokio::test]
    async fn test_handle_file_change_allows_non_coderlmignored_file() {
        let (_dir, root) = setup_test_dir();
        let gi = create_coderlmignore(&root, "generated/\n");
        create_rust_file(&root, "src/lib.rs", "pub fn real() {}");

        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());

        handle_file_change(
            &root,
            &file_tree,
            &symbol_table,
            1024 * 1024,
            &gi,
            "src/lib.rs",
            &root.join("src/lib.rs"),
        ).await;

        assert!(file_tree.get("src/lib.rs").is_some(),
            "Non-ignored files should still be indexed");
    }

    #[test]
    fn test_load_coderlm_ignore_missing_file_returns_empty() {
        let (_dir, root) = setup_test_dir();
        // No .coderlmignore file created
        let gi = config::load_coderlm_ignore(&root);
        assert!(gi.is_empty(), "Missing .coderlmignore should produce empty matcher");
    }

    #[test]
    fn test_load_coderlm_ignore_with_file() {
        let (_dir, root) = setup_test_dir();
        let gi = create_coderlmignore(&root, "vendor/\n*.pb.go\n");
        assert!(!gi.is_empty(), "Non-empty .coderlmignore should produce non-empty matcher");
    }
}
