# Configuration (extensible)

Ravel is **zero-config by default** for TS/JS projects, including standalone projects and
monorepos. Everything below is optional and
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
| `watch` | **No** | OS events (best non-git freshness) |

**Default `sync.mode = "auto"`:** use git **only when** `.git` exists; otherwise behave like
`none` (no spawn, no error). Non-git users are first-class.

**Performance path for auto-sync:**

1. Skip if `auto = false` or no index (stats sidecar check only).
2. Skip if not a git repo (cached probe).
3. `git status` for dirty paths (cached ~750ms in warm MCP).
4. Compare against **`file_hashes` sidecar** (small) — **do not** load full snapshot if hashes match.
5. Only then reparse changed files + republish.

If you never want git:

```toml
[sync]
mode = "none"
auto = false
```

Then: `ravel sync path/a.ts` or `ravel watch`.

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
# [[sync.sibling_emit]]
# emit = "js"
# sources = ["ts", "tsx"]

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
| `RAVEL_MCP_TOOLS` | `primary` / `all` (MCP surface) |
