# CodeRLM: Potential Contributions

## Context

CodeRLM is a tree-sitter-powered code indexing server that gives AI coding agents (Claude Code, Codex, Cursor, etc.) a token-efficient way to explore codebases. Instead of reading full files into context, agents navigate through a coarse-to-fine funnel: `structure` -> `symbols/search` -> `impl` (single function) -> `peek` (line range).

This solves a real problem: when agents explore codebases, full file reads stay in conversation context permanently, wasting input tokens on every subsequent turn. CodeRLM already works and supports 12 platforms. But a deep code review revealed several gaps — from outright bugs to missing architectural pieces — that limit its effectiveness.

The contributions below are ordered by impact, prioritizing core index correctness over transport and ergonomics. Each one explains what's wrong, why it matters for token savings and agent quality, and what the fix looks like.

---

## P0: Critical Bugs

### 1. Symbol Table Key Collisions Cause Silent Data Loss

**The problem:** `SymbolTable` keys symbols as `"file::name"` (`symbols/mod.rs:29,33`), so same-named methods in the same file overwrite each other — regardless of class, impl block, or overload. In any file with method overloading or multiple classes defining methods with the same name, only the last-parsed symbol survives.

**Why it matters:** This is silent data corruption. Java classes with overloaded constructors, Rust impl blocks with common method names (`new`, `default`, `from`), TypeScript classes — all lose symbols. An agent querying `search` or `callers` gets incomplete results with no indication anything is missing. This undermines the core value proposition.

**The fix:** Change the primary key to include disambiguating context — e.g., `"file::parent::name"` or `"file::name::line"`. The `by_name` and `by_file` secondary indices (`mod.rs:15`) also need updating. This is a breaking change to the key format, so all insertion and lookup paths must be audited.

### 2. Java Records and Constructors Are Silently Dropped

**The problem:** Tree-sitter queries for Java records (`queries/java.rs:13-14`) and constructors (`queries/java.rs:19-20`) are defined but the parser never handles the capture names `record.name` and `constructor.name`. The match arms in `parser.rs:57-140` simply don't exist for these captures. Symbols are queried from the AST and then silently discarded.

**Why it matters:** Java records are increasingly common in modern Java (16+). Constructors are fundamental. An agent exploring a Java codebase will never find these symbols via `search` or `symbols`, forcing it to fall back to full file reads — defeating the entire purpose of CodeRLM.

**The fix:** Add match arms in `parser.rs` for `record.name` (-> `SymbolKind::Class` or a new `Record` kind) and `constructor.name` (-> `SymbolKind::Method`). Small change, maybe 10 lines.

### 3. Scala Objects Are Silently Dropped

**The problem:** Same bug pattern. `queries/scala.rs:7-8` defines a query for `object_definition` with capture `@object.name`, but `parser.rs` has no handler for it.

**Why it matters:** Scala objects (singletons, companion objects) are a core language feature. Missing them means the agent can't find entry points, factories, or companion logic.

**The fix:** Add a match arm for `object.name` -> `SymbolKind::Module` (or `SymbolKind::Class`). ~5 lines.

### 4. Symbol Extraction Race Condition (Worse Than It Appears)

**The problem:** When a project is created (`state.rs:105-115`), symbol extraction is spawned as a background async task. No endpoint checks readiness before returning results — `/symbols` (`routes.rs:322`) and `/symbols/search` (`routes.rs:347`) immediately read the live table. An agent that creates a session and immediately queries symbols may get empty or incomplete results.

**Compounding issue:** The annotation load is also racy. It runs after a hardcoded 500ms delay (`routes.rs:161-169`) that may not be enough. Worse, `annotations.rs:124` drops annotations for symbols not yet present in the table — this is **permanent annotation loss**, not just a timing issue. Once the symbols finish extracting, the annotations that referenced them are already gone.

**Why it matters:** This is the first thing an agent does after connecting — query for symbols. If the response is empty because extraction hasn't finished, the agent concludes there are no symbols and falls back to reading files directly. The entire indexing value is lost, and the agent doesn't know to retry.

**The fix:** Either block session creation until extraction completes (simple, adds latency) or have symbol endpoints return a `ready: false` / `indexing_progress` field so the agent (or CLI) can wait/retry. The annotation load must be deferred until after symbol extraction completes, not guarded by a fixed sleep.

---

## P1: Core Index Correctness

### Tree-sitter Coverage Gaps

These are real extraction gaps where symbols are entirely invisible to CodeRLM.

#### Python: Module-Level Constants

**Module-level constants not extracted.** Python modules commonly define constants (`MAX_RETRIES = 3`, `DEFAULT_TIMEOUT = 30`), configuration dicts, and sentinel objects at module level. These are invisible to CodeRLM (`queries/python.rs:3`), so the agent can't find them via `search` and must read the full file.

**Fix:** Add a query for `(expression_statement (assignment left: (identifier) @constant.name)) @constant.def` at module level. Map to `SymbolKind::Constant`.

#### Python: Decorators Not Captured

`@property`, `@staticmethod`, `@classmethod`, `@app.route("/path")` — these carry critical semantic information. A function decorated with `@app.route` is an HTTP endpoint, not a utility. Without this, the agent can't distinguish endpoints from helpers.

**Fix:** Extend the function/method query to capture decorator nodes. Store as metadata on the Symbol (e.g., a `decorators: Vec<String>` field) or include in the signature.

#### Rust: Macro Definitions Not Captured

`macro_rules!` definitions are absent from the Rust queries (`queries/rust.rs:3`). Macros are a major part of Rust codebases. Missing them means the agent can't find macro definitions via search.

**Fix:** Add `(macro_definition name: (identifier) @macro.name) @macro.def` to the Rust symbols query. Map to `SymbolKind::Function` or a new `Macro` kind.

#### TypeScript: `new` Expressions Not Captured as Callers

`new Foo()` doesn't show up in `/symbols/callers` for class `Foo` (`queries/typescript.rs:28`).

**Fix:** Add `(new_expression constructor: (identifier) @callee)` to the TypeScript callers query.

#### Go: `var` Declarations Mapped to `SymbolKind::Constant`

This is semantically wrong — vars are mutable. `queries/go.rs:28-30` captures `var_spec` as `@const.name`, which maps to `SymbolKind::Constant` in the parser.

**Fix:** Add a separate `var.name` capture and map to `SymbolKind::Variable`.

### Dead Test Discovery Configuration

Language-specific `TestPattern`s are defined in `queries/mod.rs:25`, but `find_tests` in `symbol_ops.rs:338` ignores them entirely and uses filename/name heuristics instead. Either the patterns should drive test discovery, or the dead config should be removed.

### Oversized File Behavior Mismatch

`config.rs:44` documents that oversized files are "still listed in the tree," but `walker.rs:61` drops them entirely. Either the code or the documentation is wrong — they need to agree.

### Note on Items Not Listed

Some gaps mentioned in earlier reviews turned out to already be addressed:
- **Python `async def`** and **Rust visibility modifiers** (`pub`, `pub(crate)`) are already visible in the stored `signature` field, since `parser.rs:149` stores the first source line of each symbol.
- **TypeScript constructors** are already captured as regular methods via the existing method query in `queries/typescript.rs:10`.

---

## P2: Operational Improvements

### Symbol Search: Nondeterministic and O(n)

**The problem:** `/symbols/search` does a case-insensitive substring scan across all symbols in the table (`symbols/mod.rs:70-82`). No index, no sorting. Results are bounded by a `limit` parameter (default 20, `routes.rs:353`), but because they come from DashMap iteration, the results are **nondeterministic** — the same query can return different symbols on different calls, depending on internal hash ordering.

**Why it matters:** Search is the first thing the agent calls. Nondeterministic results mean the agent may miss important symbols simply because the iteration happened to hit the limit before reaching them. This is worse than just being slow — it's unreliable.

**The fix:** Build a prefix trie or sorted index at extraction time. Add `offset`/`limit` pagination with deterministic ordering (e.g., alphabetical, then by file). Add a `total` count to responses so the agent knows there's more.

### Synchronous Watcher Callback

**The problem:** When a file changes, the `notify` watcher callback re-parses it synchronously (`watcher.rs:98-150`). A large file change blocks all subsequent file change processing until the parse completes.

**Why it matters:** During active development (frequent saves, branch switches), the watcher falls behind. Symbol data becomes stale for files that changed while the watcher was busy parsing an earlier change.

**The fix:** Spawn re-extraction to `tokio::task::spawn_blocking()` instead of running it synchronously in the callback. Use a channel to decouple change detection from parsing.

### No Batch Operations

**The problem:** Every symbol lookup is a separate HTTP round-trip. If the agent wants to inspect 5 symbols, that's 5 requests, 5 responses in context.

**Why it matters:** Round-trips add latency and each response is a separate tool output in the agent's context. 5 small responses may cost more total context than 1 combined response.

**The fix:** Add batch variants: `/symbols/implementations?symbols=foo,bar,baz` returning multiple results in one response.

### No Token Savings Telemetry

**The problem:** There's no way to measure whether CodeRLM is actually saving tokens. Users install it and hope for the best.

**Why it matters:** Without measurement, users can't justify the setup overhead. And developers can't identify which tools are most/least effective.

**The fix:** Track per-session stats: number of symbol lookups vs full file reads, estimated tokens served via `impl`/`peek` vs what full file reads would have cost, and expose via `/stats` endpoint. A simple `coderlm stats` CLI command that shows "This session: 47 symbol lookups, 8 full file reads. Estimated savings: ~180k tokens."

---

## P3: Persistent Content-Hash Cache

### The Problem

CodeRLM loses all index state on server restart. While it correctly reuses in-memory projects across sessions pointing to the same canonical path (`state.rs:64`), stopping and restarting the server forces a full re-index of every project. For large codebases, this means seconds of startup latency while tree-sitter parses every file — and exacerbates the symbol extraction race (P0 #4).

### What to build

A SQLite-backed persistent cache keyed by content hash:

```
file_index(
  content_hash TEXT PRIMARY KEY,
  language TEXT,
  symbols_json TEXT,
  parser_version INTEGER,
  grammar_version TEXT,
  symbol_schema_version INTEGER,
  created_at TIMESTAMP
)

workspace_manifest(
  workspace_id TEXT,    -- canonical cwd (matching state.rs key)
  rel_path TEXT,
  content_hash TEXT,
  mtime INTEGER,
  file_size INTEGER,
  last_seen_at TIMESTAMP,
  PRIMARY KEY (workspace_id, rel_path)
)
```

**Key design decisions:**
- **Content hash, not mtime for symbol lookup**: Handles branch switches, rebases, and cherry-picks correctly. If you switch to a branch where `foo.rs` has the same content, no re-parse needed.
- **mtime + size in workspace manifest for fast startup**: On startup, stat each file. If mtime + size match the manifest, skip hashing entirely. Only hash (and potentially re-parse) files that changed. This is what makes "near-instant startup" achievable — most files don't change between restarts.
- **Deduplication across workspaces**: Same file content in different worktrees shares one cache entry.
- **Cache location**: `~/Library/Caches/coderlm/` (macOS), `~/.cache/coderlm/` (Linux). Not in the repo.
- **Versioned invalidation**: Cache entries are invalidated when `parser_version`, `grammar_version`, or `symbol_schema_version` change — not just when content changes. This prevents stale symbols after CodeRLM upgrades.
- **Hydration plan**: On startup, populate `FileTree` and `SymbolTable` directly from cache hits before spawning background extraction for cache misses. This ensures the race condition (P0 #4) is minimized — most symbols are available immediately.
- **Non-git fallback**: `workspace_id` uses the canonical cwd path (matching the existing `state.rs:52` key), not the git root. This works for non-git projects too.

### Impact

- Near-instant startup for previously-indexed projects (stat + manifest lookup, no hashing or parsing for unchanged files)
- Branch switching only re-parses changed files
- Server restarts don't lose index state
- Annotations can be stored alongside the cache (keyed by workspace + path)

---

## P4: MCP Transport Layer

### Why This Matters

CodeRLM currently uses a plain HTTP REST API. Agent integration requires:
- A Python CLI wrapper that translates subcommands to HTTP calls
- Platform-specific config generators (`generate.py`) for 12 different agents
- Hook-based prompt injection to steer agents toward CodeRLM tools
- Per-platform instruction templates, rules files, and state directories

MCP (Model Context Protocol) is supported natively by Claude Code, Codex CLI, OpenCode, Cursor, Windsurf, and others. A single MCP server replaces all 12 config generators. The agent discovers tools automatically via the protocol. Tool descriptions steer behavior without prompt injection hacks.

**Note:** MCP is deliberately prioritized below P0-P3. Shipping MCP on top of lossy symbol data (key collisions, dropped captures, race conditions) just lets more agents get wrong answers faster. Fix the core index first.

### What to build

Add an MCP transport alongside (not replacing) the existing HTTP API. The MCP server should call the shared `ops/*` service functions and `AppState` directly — **not** proxy through the HTTP/Axum handlers, which are coupled to headers, JSON extraction, and HTTP status codes.

The MCP layer would:
- Expose operations as MCP tools (`coderlm_structure`, `coderlm_search`, `coderlm_impl`, `coderlm_peek`, `coderlm_grep`, `coderlm_callers`, `coderlm_tests`, etc.)
- Call `ops::*` functions directly, sharing the same `AppState`
- Use MCP tool annotations (`readOnlyHint: true`) for safety
- Support both stdio and Streamable HTTP transports
- Handle session/workspace semantics (MCP doesn't have `X-Session-Id` headers — needs a different approach)

### Agent steering via tool descriptions

The current hook-based steering ("ALWAYS invoke Skill(coderlm) as your FIRST action") is fragile — it depends on the agent respecting injected prompts. With MCP, tool descriptions do the steering:

- `coderlm_impl`: "Returns the source code of a single function or method. Use instead of reading the entire file."
- `coderlm_search`: "Index-backed symbol search. Faster and more precise than grep for finding functions, classes, and types."
- `coderlm_peek`: "Returns a specific line range from a file. Use for targeted reading instead of loading full files."

### Installation UX improvement

With MCP, installation becomes:
```bash
# Instead of running generate.py for each platform:
claude mcp add coderlm -- coderlm-server --mcp
# or
codex mcp add coderlm --url http://127.0.0.1:3000/mcp
```

One command, any client. No generated config files committed to the repo.

---

## P5: Nice-to-Haves

### Structural File Summaries as a Default Response

Currently, `symbols --file path` returns a flat list of symbols. A more useful default would be a structured outline showing the file's shape:

```
src/server/routes.rs (Rust, 450 lines)
  Imports: axum, serde, tokio
  Functions:
    pub async fn health_check(State) -> Json<Value>       [L12]
    pub async fn create_session(State, Json) -> Json<Value> [L45]
    ...
  Structs:
    pub struct CreateSessionRequest { cwd: String }        [L8]
```

This gives the agent a file "outline" — the equivalent of collapsing all function bodies in an IDE. Much cheaper than reading the full file, and more structured than a flat symbol list.

### `.coderlmignore`

The current ignore logic uses hardcoded patterns in `config.rs:4-33` plus `.gitignore`. A `.coderlmignore` file would let users exclude project-specific noise (generated protobuf code, vendored dependencies, test fixtures) without modifying `.gitignore`.

### Doc-Comment Extraction

Tree-sitter can capture doc comments (`///` in Rust, `"""` in Python, `/** */` in Java/TS) attached to symbols. Surfacing these in symbol metadata would give agents even more context without reading the full file — often the doc comment alone is enough to determine relevance.

### Import/Export Graph

Currently, imports aren't extracted as queryable data. Adding import extraction would enable "what does this file depend on?" and "what depends on this file?" queries — powerful for understanding architecture without reading code.
