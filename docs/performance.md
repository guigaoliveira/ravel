# Performance notes

Ravel is designed for fast repeated queries against an existing `.ravel/`
index. Sidecars keep common reads small, while `sync` updates only changed
files when possible.

Full `index` runs are intentionally separate from the agent hot path and can
take minutes on a large project. A changed-file `sync` is expected to be much
cheaper than a full rebuild.

## Design choices

| Mechanism | Role |
|-----------|------|
| Sidecars (`stats`, `graph`, `symbols`, `hubs`, artifact locator) | Avoid loading the full snapshot for common reads and changed-path hash checks |
| `sync.include_untracked = false` (default) | No multi-thousand untracked walks |
| Hash sidecar no-op | Dirty paths with same content → no republish |
| `status` never spawns `git status` | Session start stays cheap |
| Git optional (`mode = auto`) | No git → zero discovery cost |
| Shared per-root daemon | MCP sessions reuse one watcher, engine cache, and serialized writer |
| Resident-only structural acceleration | Avoid global acceleration-pack hydration when a cold exact fallback is cheaper |

On the 21k-file stress corpus used during the 1.1.0 work, no-op and content-only
syncs stayed below 10ms. Structural add/delete/rename measured roughly
130–300ms depending on whether the daemon was warm. These numbers are a local
regression baseline, not a guarantee for other graphs or machines.

## Out of the hot path

Do not call these every agent turn: `index`, `validate`, `export`, or `ci` with
full policy output.

## Measurement

```bash
# Example:
for c in stats status "search Foo --kind prefix --limit 10" "context Foo --limit 5"; do
  /usr/bin/time -f "%e $c" ravel --root "$REPO" $c >/dev/null
done
```

Record results under `reports/perf-*.md` when changing performance-sensitive
code. Treat timings as machine- and project-dependent rather than universal
guarantees.

## Config knobs that affect latency

```toml
[sync]
mode = "auto"              # none | auto | git
auto = true
include_untracked = false  # set true only if you need new-file auto-sync without watch
```

Without git: MCP watches requested roots automatically. For CLI-only workflows,
use `ravel watch` or `ravel sync path/to/file.ts`.
