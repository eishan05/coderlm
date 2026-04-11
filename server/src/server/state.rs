use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::watch;
use tracing::info;

use crate::cache::CacheStore;
use crate::index::file_tree::FileTree;
use crate::index::{walker, watcher};
use crate::ops::annotations;
use crate::server::errors::AppError;
use crate::server::session::Session;
use crate::symbols::{ImportTable, SymbolTable, parser};

/// A single indexed project with its own file tree, symbol table, and watcher.
pub struct Project {
    pub root: PathBuf,
    pub file_tree: Arc<FileTree>,
    pub symbol_table: Arc<SymbolTable>,
    /// Import dependency graph extracted from source files.
    pub import_table: Arc<ImportTable>,
    // Held alive to keep the filesystem watcher running; dropped on eviction.
    #[allow(dead_code)]
    pub watcher: Option<watcher::WatcherHandle>,
    pub last_active: Mutex<DateTime<Utc>>,
    /// Tracks whether the initial full symbol extraction has completed.
    /// This is a one-shot flag: it starts `false` and flips to `true` once.
    /// Incremental reindexing triggered by the filesystem watcher does NOT
    /// reset this flag, since those updates are file-level and brief.
    pub indexing_complete_rx: watch::Receiver<bool>,
}

impl Project {
    /// Returns `true` if the initial symbol extraction has finished.
    /// Note: this does not track incremental watcher-driven reindexing.
    pub fn is_indexing_complete(&self) -> bool {
        *self.indexing_complete_rx.borrow()
    }

    /// Wait until symbol extraction completes (or the channel closes).
    pub async fn wait_until_indexed(&self) {
        let mut rx = self.indexing_complete_rx.clone();
        // If already complete, return immediately
        if *rx.borrow() {
            return;
        }
        // Wait for the value to change to true
        let _ = rx.wait_for(|v| *v).await;
    }
}

/// Shared application state, wrapped in Arc for axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<AppStateInner>,
}

pub struct AppStateInner {
    pub projects: DashMap<PathBuf, Arc<Project>>,
    pub sessions: DashMap<String, Session>,
    pub max_projects: usize,
    pub max_file_size: u64,
    pub cache: Option<Arc<CacheStore>>,
}

impl AppState {
    pub fn new(max_projects: usize, max_file_size: u64) -> Self {
        // Try to open the persistent cache; log and continue if it fails
        let cache = match CacheStore::open(&CacheStore::default_db_path()) {
            Ok(store) => {
                info!(
                    "Persistent cache opened at {}",
                    CacheStore::default_db_path().display()
                );
                Some(Arc::new(store))
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to open persistent cache: {}. Proceeding without cache.",
                    e
                );
                None
            }
        };

        Self {
            inner: Arc::new(AppStateInner {
                projects: DashMap::new(),
                sessions: DashMap::new(),
                max_projects,
                max_file_size,
                cache,
            }),
        }
    }

    /// Create AppState with an explicit cache (for testing).
    pub fn new_with_cache(
        max_projects: usize,
        max_file_size: u64,
        cache: Option<Arc<CacheStore>>,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                projects: DashMap::new(),
                sessions: DashMap::new(),
                max_projects,
                max_file_size,
                cache,
            }),
        }
    }

    /// Look up an existing project or index a new one. Evicts LRU if at capacity.
    pub fn get_or_create_project(&self, cwd: &Path) -> Result<Arc<Project>, AppError> {
        let canonical = cwd
            .canonicalize()
            .map_err(|e| AppError::BadRequest(format!("Path not accessible: {}", e)))?;

        if !canonical.is_dir() {
            return Err(AppError::BadRequest(format!(
                "'{}' is not a directory",
                canonical.display()
            )));
        }

        // Return existing project if found
        if let Some(project) = self.inner.projects.get(&canonical) {
            *project.last_active.lock() = Utc::now();
            return Ok(project.clone());
        }

        // Check capacity, evict if needed
        if self.inner.projects.len() >= self.inner.max_projects {
            self.evict_lru()?;
        }

        // Scan directory
        let file_tree = Arc::new(FileTree::new());
        let symbol_table = Arc::new(SymbolTable::new());
        let import_table = Arc::new(ImportTable::new());
        let max_file_size = self.inner.max_file_size;

        info!("Indexing new project: {}", canonical.display());
        let file_count = walker::scan_directory(&canonical, &file_tree, max_file_size)
            .map_err(|e| AppError::Internal(e.to_string()))?;
        info!("Indexed {} files for {}", file_count, canonical.display());

        // Start watcher
        let watcher_handle = watcher::start_watcher(
            &canonical,
            file_tree.clone(),
            symbol_table.clone(),
            import_table.clone(),
            max_file_size,
        )
        .ok();

        // Channel to signal when symbol extraction is complete
        let (indexing_tx, indexing_rx) = watch::channel(false);

        let project = Arc::new(Project {
            root: canonical.clone(),
            file_tree: file_tree.clone(),
            symbol_table: symbol_table.clone(),
            import_table: import_table.clone(),
            watcher: watcher_handle,
            last_active: Mutex::new(Utc::now()),
            indexing_complete_rx: indexing_rx,
        });

        self.inner.projects.insert(canonical, project.clone());

        // Spawn symbol extraction in background. When it completes, load
        // annotations (which depend on symbols being present) and then
        // signal readiness.
        let ft = file_tree;
        let st = symbol_table;
        let it = import_table;
        let root = project.root.clone();
        let cache = self.inner.cache.clone();
        tokio::spawn(async move {
            info!("Starting symbol extraction for {}...", root.display());
            match parser::extract_all_symbols_cached(&root, &ft, &st, &it, cache.as_ref()).await {
                Ok(count) => {
                    info!("Extracted {} symbols for {}", count, root.display());
                    // Load annotations now that symbols are available.
                    // This replaces the old racy 500ms-delayed spawn.
                    match annotations::load_annotations(&root, &ft, &st) {
                        Ok(_) => info!("Loaded annotations for {}", root.display()),
                        Err(e) => tracing::warn!(
                            "Failed to load annotations for {}: {}",
                            root.display(),
                            e
                        ),
                    }
                }
                Err(e) => tracing::error!("Symbol extraction failed for {}: {}", root.display(), e),
            }
            // Signal readiness regardless of success/failure so waiters don't hang
            let _ = indexing_tx.send(true);
        });

        Ok(project)
    }

    /// Look up the project for a given session. Returns a descriptive error if
    /// the project has been evicted.
    pub fn get_project_for_session(&self, session_id: &str) -> Result<Arc<Project>, AppError> {
        let session = self
            .inner
            .sessions
            .get(session_id)
            .ok_or_else(|| AppError::NotFound(format!("Session '{}' not found", session_id)))?;

        let project_path = &session.project_path;

        let project = self.inner.projects.get(project_path).ok_or_else(|| {
            AppError::Gone(format!(
                "Project at '{}' was evicted due to capacity limits. \
                     Start a new session to re-index, or increase --max-projects.",
                project_path.display()
            ))
        })?;

        Ok(project.clone())
    }

    /// Update the last-active timestamp on a project.
    pub fn touch_project(&self, project_path: &Path) {
        if let Some(project) = self.inner.projects.get(project_path) {
            *project.last_active.lock() = Utc::now();
        }
    }

    /// Evict the least recently used project. Removes all sessions pointing to it.
    fn evict_lru(&self) -> Result<(), AppError> {
        // Find the project with the oldest last_active
        let oldest = self
            .inner
            .projects
            .iter()
            .min_by_key(|entry| *entry.value().last_active.lock())
            .map(|entry| entry.key().clone());

        let path = oldest.ok_or_else(|| AppError::Internal("No projects to evict".into()))?;

        info!("Evicting project: {}", path.display());

        // Clear cached manifest entries for this workspace
        if let Some(ref cache) = self.inner.cache {
            let workspace_id = path.to_string_lossy().to_string();
            if let Err(e) = cache.clear_workspace(&workspace_id) {
                tracing::warn!(
                    "Failed to clear cache manifest for {}: {}",
                    path.display(),
                    e
                );
            }
        }

        // Remove the project (drops watcher)
        self.inner.projects.remove(&path);

        // Remove all sessions attached to this project
        self.inner
            .sessions
            .retain(|_, session| session.project_path != path);

        Ok(())
    }

    /// Returns a reference to the cache store, if available.
    pub fn cache(&self) -> Option<&std::sync::Arc<CacheStore>> {
        self.inner.cache.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::file_tree::FileTree;
    use crate::symbols::SymbolTable;

    /// Helper: create a Project with a pre-set indexing_complete state.
    fn make_test_project(ready: bool) -> Project {
        let (tx, rx) = watch::channel(ready);
        // Keep tx alive if ready is true; drop it otherwise to test channel behavior
        if !ready {
            // We'll need the tx to send later, but for this test we just
            // want an un-signalled receiver
            std::mem::forget(tx);
        }
        Project {
            root: PathBuf::from("/tmp/test-project"),
            file_tree: Arc::new(FileTree::new()),
            symbol_table: Arc::new(SymbolTable::new()),
            import_table: Arc::new(ImportTable::new()),
            watcher: None,
            last_active: Mutex::new(Utc::now()),
            indexing_complete_rx: rx,
        }
    }

    #[test]
    fn test_project_initially_not_ready() {
        let project = make_test_project(false);
        assert!(!project.is_indexing_complete());
    }

    #[test]
    fn test_project_reports_ready_when_signalled() {
        let project = make_test_project(true);
        assert!(project.is_indexing_complete());
    }

    #[tokio::test]
    async fn test_watch_channel_signals_readiness() {
        let (tx, rx) = watch::channel(false);

        let project = Project {
            root: PathBuf::from("/tmp/test-project"),
            file_tree: Arc::new(FileTree::new()),
            symbol_table: Arc::new(SymbolTable::new()),
            import_table: Arc::new(ImportTable::new()),
            watcher: None,
            last_active: Mutex::new(Utc::now()),
            indexing_complete_rx: rx,
        };

        assert!(!project.is_indexing_complete());

        // Simulate symbol extraction completing
        tx.send(true).unwrap();

        // Now it should be ready
        assert!(project.is_indexing_complete());
    }

    #[tokio::test]
    async fn test_wait_until_indexed_returns_immediately_when_ready() {
        let (_tx, rx) = watch::channel(true);

        let project = Project {
            root: PathBuf::from("/tmp/test-project"),
            file_tree: Arc::new(FileTree::new()),
            symbol_table: Arc::new(SymbolTable::new()),
            import_table: Arc::new(ImportTable::new()),
            watcher: None,
            last_active: Mutex::new(Utc::now()),
            indexing_complete_rx: rx,
        };

        // Should return immediately since already signalled
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            project.wait_until_indexed(),
        )
        .await
        .expect("wait_until_indexed should return immediately when already ready");
    }

    #[tokio::test]
    async fn test_wait_until_indexed_blocks_then_resolves() {
        let (tx, rx) = watch::channel(false);

        let project = Arc::new(Project {
            root: PathBuf::from("/tmp/test-project"),
            file_tree: Arc::new(FileTree::new()),
            symbol_table: Arc::new(SymbolTable::new()),
            import_table: Arc::new(ImportTable::new()),
            watcher: None,
            last_active: Mutex::new(Utc::now()),
            indexing_complete_rx: rx,
        });

        let project_clone = project.clone();

        // Spawn a task that waits for indexing
        let wait_handle = tokio::spawn(async move {
            project_clone.wait_until_indexed().await;
            true
        });

        // Give the wait task time to start blocking
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Signal completion
        tx.send(true).unwrap();

        // The wait task should resolve now
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), wait_handle)
            .await
            .expect("wait_handle should resolve after signalling")
            .expect("join error");

        assert!(result);
    }

    #[tokio::test]
    async fn test_get_or_create_project_with_real_dir() {
        // Create a real temp directory to test the full flow
        let dir = tempfile::tempdir().unwrap();
        // Write a small file so there's something to index
        std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let state = AppState::new(5, 10_000_000);
        let project = state.get_or_create_project(dir.path()).unwrap();

        // Project should exist but indexing may not be complete yet
        // (it's async). The key test is that the field exists and is false
        // immediately after creation (since extraction just spawned).
        assert!(
            project.file_tree.len() > 0,
            "File tree should have been populated synchronously"
        );
        // indexing_complete_rx should be accessible
        let _ready = project.is_indexing_complete();
    }

    #[tokio::test]
    async fn test_get_or_create_project_signals_ready_after_extraction() {
        // Create a temp directory with a Rust file so symbol extraction runs
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn hello() { println!(\"Hello, world!\"); }\n",
        )
        .unwrap();

        let state = AppState::new(5, 10_000_000);
        let project = state.get_or_create_project(dir.path()).unwrap();

        // Wait for indexing to complete
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            project.wait_until_indexed(),
        )
        .await
        .expect("indexing should complete within 5 seconds");

        assert!(project.is_indexing_complete());
        // The Rust file should have had symbols extracted
        assert!(
            project.symbol_table.len() > 0,
            "Symbol table should have symbols after extraction"
        );
    }
}
