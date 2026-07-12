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
| Sidecars (`stats`, `graph`, `symbols`, `hubs`, `file_hashes`) | Avoid 84MB full snapshot |
| `sync.include_untracked = false` (default) | No multi-thousand untracked walks |
| Hash sidecar no-op | Dirty paths with same content → no republish |
| `status` never spawns `git status` | Session start stays cheap |
| Git optional (`mode = auto`) | No git → zero discovery cost |
| MCP engine cache | Warm multi-query sessions avoid repeated startup work |

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

Without git: use `ravel watch` or `ravel sync path/to/file.ts`.
