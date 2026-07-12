# Performance SLAs

**Hard product target:** agent-facing commands that read an **existing index** complete in
**&lt; 200 ms wall-clock** on a warm machine (cold process start included), for large indexed
projects in the ~20k-file, ~200k-edge class.

These are **requirements**, not aspirations. Regressions above budget need a fix or an
explicit budget exception in this file.

## Budget table (indexed repo)

| Command | Budget (CLI cold process) | Warm MCP (cached engine) | Notes |
|---------|--------------------------:|-------------------------:|-------|
| `stats` | **50 ms** | **10 ms** | stats sidecar only |
| `cheatsheet` | **20 ms** | n/a | static text |
| `hubs` | **50 ms** | **15 ms** | hubs sidecar |
| `impact` | **100 ms** | **30 ms** | compact graph + BFS |
| `search` (exact/prefix) | **100 ms** | **30 ms** | symbol dict / hybrid |
| `query` | **100 ms** | **30 ms** | compact graph walk |
| `context` | **200 ms** | **50 ms** | search + callers + impact |
| `refactor` | **200 ms** | **50 ms** | impact + files list |
| `status` | **100 ms** | **20 ms** | **no** git status spawn |
| `doctor` | **150 ms** | **40 ms** | status + agent detect |
| `sync` (no content change) | **100 ms** | **40 ms** | tracked dirty + hash sidecar |
| `sync` (few files changed) | **2 s** | **1 s** | reparse + resolve dirty set |
| `index` (full) | **no hot-path SLA** | n/a | minutes are acceptable on large projects |
| `validate` / `ci` | **2 s** | **1 s** | not agent hot path |
| `export` | **500 ms** | **200 ms** | package DOT |

## What “cold” means

- New OS process (`ravel …` from shell / agent CLI).
- Index already built (sidecars present).
- Page cache warm or cold is noted in reports; **budget is wall clock** on a developer laptop.

## How we stay under budget

| Mechanism | Role |
|-----------|------|
| Sidecars (`stats`, `graph`, `symbols`, `hubs`, `file_hashes`) | Avoid 84MB full snapshot |
| `sync.include_untracked = false` (default) | No multi-thousand untracked walks |
| Hash sidecar no-op | Dirty paths with same content → no republish |
| `status` never spawns `git status` | Session start stays cheap |
| Git optional (`mode = auto`) | No git → zero discovery cost |
| MCP engine cache | Warm multi-query ≪ cold CLI |

## Explicitly out of hot path

Do **not** call these every agent turn: `index`, `validate`, `export` (large), `ci` with full policy dumps.

## Measurement

```bash
# Example (large-project scale):
for c in stats status "search Foo --kind prefix --limit 10" "context Foo --limit 5"; do
  /usr/bin/time -f "%e $c" ravel --root "$REPO" $c >/dev/null
done
```

Record results under `reports/perf-*.md` when changing the hot path.

## Config knobs that affect latency

```toml
[sync]
mode = "auto"              # none | auto | git
auto = true
include_untracked = false  # set true only if you need new-file auto-sync without watch
```

Without git: use `ravel watch` or `ravel sync path/to/file.ts`.
