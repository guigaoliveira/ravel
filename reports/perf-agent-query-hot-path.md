# Agent query hot-path measurements

Measured on 2026-10-12 against base commit `63477b4b360e4fec49548bad9251919a59325b76`
(Ravel 1.19.0), using release binaries on Linux x86_64, Intel Core Ultra 7 265H
and approximately 30 GiB RAM.

## Method

The corpus was one private TypeScript monorepo snapshot containing 24,085 files,
889,932 edges and seven existing parser diagnostics. Each run copied the same
immutable generation to separate storage beneath the ignored `.ravel/` directory,
used its own `RAVEL_HOME` and `XDG_RUNTIME_DIR`, and started a foreground daemon
plus MCP stdio clients. The original snapshot was unchanged after all runs.

Each tool was warmed before collecting 30 sequential samples, spaced 80 ms apart
so the old 50 ms discovery cache could not turn the workload into an artificial
burst. Measurements include MCP transport and JSON decoding. Filesystem caches
were not flushed. The final run had no concurrent Cargo build or test workload.

The selected definition was a frequently used class; `calls_from` used one of its
methods. The held-lock scenario acquired the copied storage's `watch.lock` from
another process before daemon startup, reproducing an older daemon retaining
writer leadership without modifying or terminating that daemon.

Four-client measurements used 20 requests per client (80 total), cycling through
`callers_of`, `calls_from` and exact-ID `explore`, also spaced 80 ms apart. Memory
is the sum of Linux PSS for the daemon and measured MCP clients after the workload;
it is not peak memory or whole-machine memory.

## Warm daemon

| MCP call | Before p50 / p95 (ms) | After p50 / p95 (ms) |
|---|---:|---:|
| `status` | 27.97 / 31.42 | 3.14 / 4.28 |
| `callers_of` | 5.01 / 5.83 | 4.87 / 7.03 |
| `calls_from` | 3.69 / 5.35 | 4.58 / 5.49 |
| Exact-ID `explore` | 69.27 / 156.57 | 5.63 / 6.75 |
| Explicit no-op `sync` | 32.27 / 36.49 | 11.74 / 17.06 |

## Another process holds writer leadership

| MCP call | Before p50 / p95 (ms) | After p50 / p95 (ms) |
|---|---:|---:|
| `status` | 27.87 / 38.31 | 2.98 / 3.21 |
| `callers_of` | 136.46 / 199.62 | 4.81 / 6.41 |
| `calls_from` | 158.55 / 204.03 | 4.25 / 5.38 |
| Exact-ID `explore` | 207.45 / 250.75 | 5.28 / 6.91 |
| Explicit no-op `sync` | 33.67 / 37.20 | 7.67 / 11.63 |

## Concurrent clients and memory

| Measurement | Before | After |
|---|---:|---:|
| Four-client mixed workload p50 | 4.90 ms | 4.30 ms |
| Four-client mixed workload p95 | 215.74 ms | 6.29 ms |
| Daemon + one MCP client PSS | 293.4 MiB | 184.0 MiB |
| Daemon + four MCP clients PSS | 318.2 MiB | 211.8 MiB |

The memory reduction applies to sessions using exact IDs and relationship tools:
they no longer hydrate the lexical index. A later lexical search can still load
it. Relationship calls with an already-working observer remain in the same
few-millisecond range; the main relationship improvement restores the observer
fast path while another daemon holds writer leadership.

## Correctness and validation

Complete JSON responses matched for `callers_of`, `calls_from` and `sync`.
`status` matched after normalizing the intentionally separate storage directories.
For exact-ID `explore`, only candidates, matches, their count and candidate
truncation changed: the response now lists only the requested identity. Definition
detail, source excerpt, relations, relation totals, impact and warnings matched.
Bare-name and natural-language search keep the existing lexical path.

Regression tests cover exact IDs with same-name neighbors, missing IDs, diagnostic
cache reuse and external generation changes with equal error counts, explicit-sync
cache invalidation after content/staging/HEAD/permission changes, and follower
observers with committed deferred edits and bounded-queue overflow.

- `cargo test --workspace --all-features --locked -- --test-threads=1`:
  373 passed, four ignored.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
- `cargo fmt --all -- --check`.

The default parallel run exposed generation-GC lock flakes, also reproduced on the
unchanged base (`generation_churn_has_bounded_disk_entries` and two GC reader-lock
tests). The full local suite therefore used one test thread; no tests were skipped
beyond the repository's existing ignored tests. CI retains its existing parallel
configuration.

These are local observations on one graph, not latency guarantees. The measurements
do not claim faster lexical search, full rebuilds or real-edit publication, and
Linux measurements do not establish performance on macOS or Windows. Existing
parser diagnostics are preserved. No installed binary or original workspace
index was replaced by this benchmark.
