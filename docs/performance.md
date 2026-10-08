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
| `sync.include_untracked` | Untracked files are listed (default): +20 ms per discovery on 20k files; off if a tree carries thousands of un-ignored build outputs |
| Hash sidecar no-op | Dirty paths with same content → no republish |
| `status` never spawns `git status` | Session start stays cheap |
| Git optional (`mode = auto`) | No git → zero discovery cost |
| Shared per-root daemon | MCP sessions reuse one watcher, engine cache, and serialized writer |
| Watcher-gated freshness check | A daemon skips the whole-worktree `git status` while its watcher has seen nothing change since the last check found the index current |
| Resident-only structural acceleration | Avoid global acceleration-pack hydration when a cold exact fallback is cheaper |

On the 21k-file stress corpus used during the 1.1.0 work, no-op and content-only
syncs stayed below 10ms. Structural add/delete/rename measured roughly
130–300ms depending on whether the daemon was warm. These numbers are a local
regression baseline, not a guarantee for other graphs or machines.

## Freshness without `git status` on every query

Every query first asks git what changed (about 6 ms of CPU on 2k files, 36 ms on 20k), because the
index is only as current as the last thing that told it about an edit. A daemon already runs a
recursive file watcher, so it can often answer that question itself. Before looking, a query
makes the watcher prove it is caught up: it creates a marker file inside `.ravel/` and waits for
the watcher to report it. Events arrive in the order the changes happened, so once the marker is
back, every change that finished before the query began has been counted. If the count has not
moved since a full check last found the index consistent with the tree (and the index is still
the same generation), the full check is skipped.

The skip is withheld, and the query runs the full check it always ran, whenever the watcher cannot
vouch for the tree: it reported an error or lost events, a marker did not come back in 100 ms (three
misses in a row pause the shortcut for 30 s), a directory was created or moved in the last 2 s (its
watch may not exist yet), an ignore-rules file (`.gitignore`, `.ravelignore`, `.git/info/exclude`)
changed, the filesystem, or anything mounted inside the tree, is not a local one (network, FUSE and
VM shares are not trusted), or the last full check is more than 60 s old. Any change to a file the watcher does not filter out -- source or
not -- ends the quiet, so a log file that grows inside the workspace keeps every query on the full
check. `RAVEL_WATCH_FASTPATH=0` turns the shortcut off. CLI commands that do not go through a daemon
are unaffected.

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
guarantees. `scripts/gen_corpus.py` builds a reproducible synthetic monorepo and
`scripts/perf_bench.py` measures wall time, CPU and peak RSS per command.
`scripts/mcp_session_bench.py` drives `ravel mcp` the way an agent does (status,
explore, callers_of, edits with sync) and samples the RSS of the stdio server and
the shared daemon after each step — the processes that live for a whole session.

## Config knobs that affect latency

```toml
[sync]
mode = "auto"              # none | auto | git
auto = true
include_untracked = true   # set false on trees with thousands of un-ignored build outputs
```

Without git: MCP watches requested roots automatically. For CLI-only workflows,
use `ravel watch` or `ravel sync path/to/file.ts`.
