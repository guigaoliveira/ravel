# Configuration (extensible)

Ravel is **zero-config by default** for TypeScript/JavaScript projects. Everything below is optional and
**user-owned** — defaults are product defaults, not a lock-in.

File: `.ravel.toml` at project root (+ optional `.ravelignore`).

## Layers of “what do we index?”

| Layer | Default | Override |
|-------|---------|----------|
| **Extensions** | `ts,tsx,mts,cts,js,jsx,mjs,cjs` via `languages = ["auto"]` | `parser.extensions = [...]` **wins** |
| **Builtin noise dirs** | `node_modules`, `dist`, `build`, `.git`, `.ravel`, … | `ignore.use_builtin_dirs = false` and/or `ignore.dirs` |
| **User ignore dirs** | empty | `ignore.dirs = ["storybook-static", "generated"]` |
| **gitignore** | on | `ignore.gitignore = false` |
| **.ravelignore** | if file exists | edit the file (gitignore syntax) |

## Git: optional, performance-first

| Operation | Needs git? | Role |
|-----------|------------|------|
| `ravel index` | **No** | Walk filesystem |
| `search` / `query` / `context` (read) | **No** | Sidecars |
| `sync` with paths | **No** | Explicit list |
| `sync` no args / auto-sync | **Only if** repo has `.git` and `mode` allows | Dirty discovery |
| CLI `watch` | **No** | OS events for a long-lived CLI-only session |
| MCP server | **No** | Watches every indexed root automatically |

**Default `sync.mode = "auto"`:** use git **only when** `.git` exists; otherwise behave like
`none` (no spawn, no error). Non-git users are first-class.

**Performance path for auto-sync:**

1. Skip if `auto = false` or no index (stats sidecar check only).
2. Skip if not a git repo (cached probe).
3. `git status` for dirty paths (cached for `sync.discovery_cache_ms`, 50ms by default).
4. Point-read hashes for only the dirty paths from the artifact locator — **do not** materialize the full path/hash index.
5. Only then reparse changed files + republish.

If you never want git:

```toml
[sync]
mode = "none"
auto = false
```

Then: `ravel sync path/a.ts`; for CLI-only continuous updates, use `ravel watch`.

MCP sessions share one transient daemon, engine, watcher, and writer per
canonical root. It starts automatically and exits after the final MCP lease
disconnects and pending writes drain. `sync.mode = "none"` disables daemon
auto-watch; explicit `sync(paths)` and standalone CLI `ravel watch` remain
available. Different roots and worktrees use independent daemons, indexes, and
locks.

## Full example

```toml
[project]
root = "."

[parser]
max_file_size_kb = 1024
# High-level tokens (auto | typescript | javascript | or raw ext names like "vue")
languages = ["auto"]
# Explicit list wins completely — any extensions you care about:
# extensions = ["ts", "tsx", "js", "jsx", "mts", "cts"]

[ignore]
# Extra path-segment names to skip everywhere
dirs = ["storybook-static", "coverage-html"]
use_builtin_dirs = true
gitignore = true

[sync]
mode = "auto"              # auto | git | none
auto = true
include_untracked = false  # default fast path (tracked only); true = slower
skip_sibling_emit = true
discovery_cache_ms = 50    # reuse near-simultaneous warm MCP discovery
coalesce_ms = 0            # compatibility only; batching is contention-driven, without sleep
queue_max_ticket_bytes = 1048576
queue_max_tickets = 1024
queue_max_paths = 4096
queue_cleanup_limit = 64
queue_stale_seconds = 3600
# [[sync.sibling_emit]]
# emit = "js"
# sources = ["ts", "tsx"]

[watch]
debounce_ms = 150
# Bounded event buffer. Overflow falls back to a safe but potentially expensive full index.
queue_capacity = 4096
# Bound exact incremental batches even if the filesystem never becomes quiet.
max_batch_paths = 4096
max_batch_ms = 1000
# Environment overrides: RAVEL_WATCH_QUEUE_CAPACITY,
# RAVEL_WATCH_MAX_BATCH_PATHS, RAVEL_WATCH_MAX_BATCH_MS.

[storage]
retention = 3
# Compact the append-only artifact store based on measured physical/live byte amplification,
# not an edit-count threshold. Lower values trade more rewrite I/O for less disk usage.
artifact_store_max_amplification = 4

[analysis]
# entry_points = ["MyCustomBootstrap"]  # extras on top of heuristics
hubs_top_k = 1000
```

## Extensibility notes

- **New extensions in the index set**: set `parser.extensions`. Discovery will include them.
- **Parsers today**: tree-sitter TS/TSX grammars cover the default JS/TS family. Extra
  extensions are still listed/walked; rich symbol extract for non-TS languages is a future
  plug-in path — config already accepts them so you are not blocked on product defaults.
- **Sibling emit** is not a hardcode: it is a **configurable rule table** with a
  sensible default for tsc-style `*.js` leftovers next to `*.ts`. Disable with `skip_sibling_emit = false` or
  replace `sibling_emit`.

## Env overrides (selected)

| Env | Maps to |
|-----|---------|
| `RAVEL_HOME` | storage home dir name |
| `RAVEL_LOG_LEVEL` | log level |
| `RAVEL_DAEMON_MAX_CONNECTIONS` | hard cap for all daemon connections; defaults to `max(8, CPUs * 4)` |
| `RAVEL_DAEMON_MAX_LEASES` | hard cap for persistent MCP leases; defaults to the connection cap minus one request slot |
| `RAVEL_DAEMON_REQUEST_TIMEOUT_MS` | handshake/request read timeout; established leases are not timed out |
| `RAVEL_MCP_MAX_CACHED_ROOTS` | maximum cached workspace roots per MCP process; defaults to `8`; least-recently-used inactive roots are evicted |
| `RAVEL_MCP_TOOLS` | `primary` / `all` (MCP surface) |
