# MCP merge review against 1.19.0 — 2026-10-10

The merges improve correctness, but do not produce a uniform performance improvement. The measured follow-up optimization halves cold `context` latency on the 20,040-file corpus: **139.17 → 69.62 ms**, with **245.71 → 148.27 CPU ms** and **224.53 → 218.48 MiB peak RSS**. That cold query is also slightly below the 1.19.0 reference (74.27 ms, 151.87 CPU ms, 226.60 MiB).

Resident MCP costs remain mixed. On the large corpus, optimized exact `explore` takes **2.01 ms / 1.87 CPU ms**, versus **1.71 ms / 1.50 CPU ms** in 1.19.0. Resident RSS is **208.58 MiB versus 196.85 MiB (+6.0%)**. This change should not be described as a general memory reduction or a universal MCP speedup.

## Revisions and scope

| Label | Source |
|---|---|
| `v1.19.0` | `cedd98e64380d1b16540502c14ff04c253023410` — downloaded exact source archive and built locally; no published 1.19.0 GitHub release/tag was available when checked |
| `merged` | `775200b9a41ddb20bff76a857d7a86b21b544c53` — merges of `claude/inspiring-gauss-6vbnof` and `claude/parallel-subagents-ai-optimization-3r3xvn`, with conflicts resolved |
| `optimized` | `b8a889237023df5ebfc28a5a8c058edca093df10` — coverage optimization and test/hook fixes |

`origin/main` subsequently advanced to `63477b4` by merging the second requested branch. History was reconciled in `7bec523`; its tree is identical to `b8a8892`. Later report/harness commits do not change the measured Rust implementation. All three binaries print `1.19.0`; the commits and SHA-256 values in [manifest.json](benchmarks/mcp-merge-2026-10-10/manifest.json) identify the actual builds.

The AST inventory found **1,061 production function definitions and 503 test functions/helpers** under `crates/*/src/**/*.rs` at the merged revision. It covers authored definitions, including methods and nested functions, and does not expand macros. This is an inventory, not evidence that each function was individually exercised or audited. Profiling prioritized executed paths across configuration/ignore handling, engine orchestration, storage, graph reconstruction and search.

## Method

- Linux 7.0.0-30, Intel Core Ultra 7 265H; Rust 1.99.0, release builds with the lockfile. Measured processes and the client were pinned to CPUs 0–3. The workstation was not otherwise isolated.
- Deterministic TypeScript corpora from `scripts/gen_corpus.py`: 4 × 500 and 40 × 500 files plus barrels, yielding **2,004 / 20,040 indexed files**. Each variant has its own copy, index, runtime directory and transient daemon. The source tree is committed, with `.ravel/` ignored.
- Real stdio JSON-RPC MCP: initialize, tools/list, status, explore (exact, ambiguous and terms), callers_of, calls_from, sync and pagination. Each of the two main runs has three sessions per variant and 100 calls per operation per session: **10,800 timed burst requests**, plus **360 paced requests** with 75 ms between calls.
- Version order alternates between rounds and between operations. Warm latency is the pooled median/p95 of 300 calls per operation/version/size; CPU is the mean of the three batch totals divided by call count. p95 uses the nearest-rank definition.
- MCP CPU sums user/system time for the stdio server and daemon, including their waited-for children (such as git), from `/proc`. Tick resolution is 10 ms; dividing over 100 calls improves precision, but small CPU differences remain noisy. Client CPU is excluded.
- MCP RSS/PSS sums the two Ravel processes. Resident values below are session medians after warmup. Raw batches include 20 ms RSS samples and per-process high-water marks; sampling can miss brief peaks, and shared RSS pages can be counted twice. PSS apportions shared pages. Short-lived git RSS is not included in the server sum.
- Cold CLI rows use five fresh processes per operation/version, with warm filesystem caches. CPU and peak RSS come from `wait4`; peak RSS is the process/child high-water metric, not a simultaneous tree sum. Small commands show a roughly 41 MiB floor inherited from the Python launcher's pre-exec process image, so those rows cannot establish small RSS gains. Latency includes process startup and JSON transport.
- The separate first-explore run restarts each MCP daemon three times and sends explore before status. Its one-call warm batches are retained as diagnostics and are not used for CPU conclusions.
- Callgrind profiles count user-space instructions for one cold large-corpus context call. They include neither kernel work nor git child instructions. Hardware `perf` access was denied (`perf_event_paranoid=4`); no cycles/cache-miss claim is made. Instrumented runs were separate from latency/CPU-time measurements.

## Cold CLI results

Each cell is **wall ms / CPU ms / peak RSS MiB**, median of five runs.


### 2,004 files

| Operation | 1.19.0 | Merged | Optimized |
|---|---:|---:|---:|
| status | 18.18 / 17.77 / 41.07 | 22.77 / 22.36 / 41.07 | 20.64 / 20.24 / 41.07 |
| context exact | 19.93 / 43.71 / 48.51 | 23.13 / 44.47 / 49.07 | 17.08 / 35.36 / 48.90 |
| context ambiguous | 18.36 / 40.90 / 50.41 | 20.86 / 41.38 / 53.14 | 16.06 / 32.02 / 46.63 |
| callers-of | 30.34 / 41.12 / 41.07 | 37.35 / 47.98 / 41.07 | 25.41 / 36.36 / 41.07 |
| hubs | 9.13 / 8.60 / 41.11 | 9.30 / 8.88 / 41.11 | 9.33 / 8.88 / 41.11 |

### 20,040 files

| Operation | 1.19.0 | Merged | Optimized |
|---|---:|---:|---:|
| status | 23.73 / 22.86 / 40.93 | 142.74 / 142.61 / 40.93 | 108.56 / 108.69 / 40.93 |
| context exact | 74.27 / 151.87 / 226.60 | 139.17 / 245.71 / 224.53 | 69.62 / 148.27 / 218.48 |
| context ambiguous | 67.85 / 147.97 / 228.38 | 141.73 / 249.03 / 238.66 | 63.95 / 144.10 / 222.27 |
| callers-of | 90.70 / 143.88 / 173.56 | 216.10 / 272.73 / 167.45 | 107.59 / 156.65 / 167.45 |
| hubs | 14.94 / 14.28 / 40.93 | 14.06 / 13.43 / 40.93 | 17.79 / 16.94 / 40.93 |

The optimization reduces large-corpus context CPU by **39.7%**, latency by **50.0%**, and peak RSS by **2.7%** relative to the merge. Against 1.19.0 the differences are −2.4%, −6.3% and −3.6% respectively; these smaller margins are local observations, not a cross-machine guarantee.

Large-corpus status still costs 108.56 ms versus 23.73 ms in 1.19.0. The old probe stopped at a file-count budget; the merged probe reports the whole Git worktree and applies the complete ignore chain. That correctness change makes the work different. The hash-cache change reduces the new status cost by about 24%, but does not recover the old bounded-probe latency. Cold callers-of remains slower than 1.19.0 as well.

## Resident MCP results

Each cell is **p50 ms / p95 ms / CPU ms per call** (300 calls).


### 2,004 files

| Operation | 1.19.0 | Merged | Optimized |
|---|---:|---:|---:|
| status | 1.06 / 1.38 / 0.50 | 1.04 / 1.38 / 0.53 | 0.98 / 1.34 / 0.53 |
| explore exact | 1.63 / 2.27 / 1.33 | 1.60 / 2.16 / 1.27 | 1.63 / 2.39 / 1.37 |
| explore ambiguous | 2.20 / 3.41 / 2.03 | 1.89 / 2.81 / 1.70 | 1.84 / 2.95 / 1.60 |
| explore terms | 1.79 / 2.52 / 1.53 | 2.02 / 2.86 / 1.73 | 1.90 / 2.78 / 1.63 |
| callers_of | 0.91 / 1.48 / 0.70 | 1.05 / 1.49 / 0.70 | 0.96 / 1.46 / 0.73 |
| calls_from | 0.77 / 1.17 / 0.60 | 0.79 / 1.32 / 0.67 | 0.67 / 1.39 / 0.60 |

### 20,040 files

| Operation | 1.19.0 | Merged | Optimized |
|---|---:|---:|---:|
| status | 0.92 / 1.36 / 0.50 | 1.02 / 1.42 / 0.53 | 1.00 / 1.41 / 0.57 |
| explore exact | 1.71 / 2.41 / 1.50 | 1.90 / 2.61 / 1.60 | 2.01 / 2.85 / 1.87 |
| explore ambiguous | 8.70 / 11.11 / 8.57 | 8.23 / 10.54 / 8.13 | 7.69 / 10.16 / 7.57 |
| explore terms | 7.69 / 10.33 / 7.77 | 7.50 / 10.36 / 7.47 | 7.94 / 10.46 / 7.93 |
| callers_of | 1.02 / 1.56 / 0.70 | 0.99 / 1.58 / 0.77 | 0.94 / 1.43 / 0.67 |
| calls_from | 0.74 / 1.22 / 0.60 | 0.76 / 1.27 / 0.67 | 0.82 / 1.27 / 0.73 |

The large-corpus ambiguous query improves, but exact explore, terms and calls_from do not all improve together. Differences between individual sessions and the earlier exploratory pass are visible in the raw data. The optimization targets cold coverage work; it does not establish a steady-state speedup after coverage has been cached.

### Resident server memory

Each cell is **RSS MiB / PSS MiB** after warmup, median of three sessions.

| Corpus | 1.19.0 | Merged | Optimized |
|---|---:|---:|---:|
| 2,004 files | 64.64 / 54.15 | 68.21 / 57.85 | 65.02 / 54.92 |
| 20,040 files | 196.85 / 186.33 | 207.64 / 197.08 | 208.58 / 197.85 |

There is no demonstrated resident-memory improvement on the large corpus. The optimized build is about 0.94 MiB above the merge and 11.73 MiB above 1.19.0 in RSS. On the small corpus its RSS is lower than the merge, but approximately equal to 1.19.0. Cold CLI peak memory and resident MCP memory describe different lifetimes and must not be substituted for one another.

### MCP first request and paced requests

| Large-corpus scenario | 1.19.0 | Merged | Optimized |
|---|---:|---:|---:|
| First status, median ms | 24.15 | 136.31 | 103.98 |
| First explore after restart, median ms | 74.83 | 135.05 | 71.16 |
| First explore CPU, median ms | 150.00 | 270.00 | 170.00 |
| Paced exact explore, median ms | 3.51 | 3.53 | 3.58 |

First-explore samples include a substantially slower first-ever session for every variant (1.19.0: 380.56 ms; merged: 389.33 ms; optimized: 367.88 ms). Later restarts were 69–75, 128–135 and 66–71 ms respectively. The median must not be presented as the initial first-use time. Its CPU numbers have 10 ms resolution and only three samples. With a 75 ms pause between requests, exact-query latency remains around 3.5 ms for every variant.

## Function-level evidence and changes

The [merged profile](benchmarks/mcp-merge-2026-10-10/merged-profile.txt) attributes **743.53 million instructions (66.72%)** to `component_source_probe`, including **445.61 million (39.99% of the whole program)** in `IgnoreChain::rules_for`. Inclusive costs overlap and must not be added. The chain stored directory rules in a BTreeMap, repeatedly comparing shared path prefixes for every file.

The implementation changes two internal details in `config.rs`:

1. Apply the component-extension predicate before metadata/ignore checks on Git's complete enumeration. A `.ts` file cannot affect the `.vue/.svelte/.astro` count. The non-Git walk still charges every file against its existing budget; filtering before that budget would change answer-health semantics.
2. Use a HashMap for the directory-rule cache. This cache is only looked up, inserted into, or cleared; iteration order is not observable. Counts and public response ordering retain their existing ordered structures.

The [optimized profile](benchmarks/mcp-merge-2026-10-10/optimized-profile.txt) reduces the component probe to **20.39 million instructions (4.94%)**. Whole-program instructions fall from **1,114.33 to 412.85 million (−63.0%)**. Uninstrumented CPU/RSS results above are the evidence for actual resource effects.

After this change, `FileSnapshotStorage::open_graph` accounts for **203.20 million instructions (49.22%)**, and `WorkspaceEngine::context_searches` for **154.95 million (37.53%)**. These are measured remaining costs, not quantified savings from a proposed rewrite. A graph-loading or search change needs its own CPU and memory A/B; no such gain is claimed here. The resident-memory increase and full status census remain open optimization targets.

The largest function groups in the merged inventory are:

| Module | Production definitions |
|---|---:|
| storage | 167 |
| engine | 134 |
| graph | 88 |
| install | 76 |
| resolver | 76 |
| mcp | 65 |
| daemon | 59 |
| config | 51 |
| scanner | 51 |
| search | 50 |
| Remaining modules | 244 |
| Total | 1,061 |

## Functional checks and test findings

- The 1.19.0 MCP returns `next_cursor: "1"` but rejects that same string when sent back as `cursor` (`invalid type: string "1", expected usize`). The merge and optimized builds accept it and return the next page. All six baseline paging checks reproduce the failure; all twelve merged/optimized checks pass.
- Across the main runs, **18 edit sequences / 72 explicit sync steps** cover unchanged content, a body/comment edit, adding a function, and restoring the original file. Queries observe the added function only during the structural-edit step.
- The harness asserts equal tool schemas, coverage/stats/schema/diagnostics, initial query responses, warm query-response hashes, and pagination between merged and optimized builds for every round. Snapshot IDs/root paths and the volatile `auto_synced` flag are normalized; disk byte/generation totals are excluded from status equivalence. No cross-version equivalence is claimed for 1.19.0, whose parser/resolver/schema differ. For example, the large corpus has 697,728 versus 637,728 edges; this is not an equal-graph algorithm benchmark.
- The existing differential coverage test now checks tracked, deleted and untracked components against the full probe even with a zero Git walk budget. Its non-Git budget/case/noise checks remain in place.
- The first parallel library runs exposed tests assuming immediate maintenance after dropping a reader. Serial execution passed all 404 enabled library tests. A separate flock/fork probe reproduced how an inherited child handle can keep the barrier held after the parent closes its handle. Tests now retry only expected-success GC/compaction with a five-second deadline. Live-reader deferral checks still inspect the first attempt. Production GC behavior was not changed.
- A real pre-commit attempt exposed Git hook environment leakage: fixture `git init/config/commit` operations were redirected by inherited repository variables into the PR worktree, creating fixture commits and changing local Git configuration. Those effects were restored before continuing. `scripts/with-clean-git-env.sh` clears Git's own `--local-env-vars` list before unit/workspace tests in both hooks. An injected-environment fixture probe verified a separate `.git` and unchanged parent HEAD; the corrected commit hook then passed.

Validation on the implementation: **486 tests passed, 4 ignored across 22 suites**; rustfmt and Clippy with all targets/features and `-D warnings` passed. Final push/CI status is recorded in the PR. Results here are from synthetic corpora on one Linux workstation; there is no production-repository or macOS/Windows performance claim.

## Reproduce and inspect

The [raw evidence directory](benchmarks/mcp-merge-2026-10-10/) contains per-call timings, process CPU counters, memory samples, tool schemas, edit results, binary hashes, the function inventory and Callgrind files. `final-*.json.gz` are the final three-variant runs; `full-*.json.gz` preserve the earlier two-variant exploratory pass. `summary-*.json` are derived with the aggregation rules above. Local scratch prefixes are replaced with `<BENCH>`/`<CHECKOUT>`.

From a checkout containing the PR history, choose an unused scratch directory:

```sh
RAVEL_BENCH=/tmp/ravel-mcp-merge-review
rtk git worktree add --detach "$RAVEL_BENCH/before" cedd98e64380d1b16540502c14ff04c253023410
rtk git worktree add --detach "$RAVEL_BENCH/merged" 775200b9a41ddb20bff76a857d7a86b21b544c53
rtk git worktree add --detach "$RAVEL_BENCH/optimized" b8a889237023df5ebfc28a5a8c058edca093df10
for ravel_variant in before merged optimized; do
  rtk cargo build --release --locked --bin ravel \
    --manifest-path "$RAVEL_BENCH/$ravel_variant/Cargo.toml" \
    --target-dir "$RAVEL_BENCH/target-$ravel_variant"
done
rtk proxy python3 scripts/gen_corpus.py "$RAVEL_BENCH/corpus" 40 500
rtk proxy python3 -c 'from pathlib import Path; import sys; (Path(sys.argv[1])/".gitignore").write_text(".ravel/\n")' "$RAVEL_BENCH/corpus"
rtk git -C "$RAVEL_BENCH/corpus" init -q
rtk git -C "$RAVEL_BENCH/corpus" add .
rtk git -C "$RAVEL_BENCH/corpus" -c user.name=Benchmark -c user.email=benchmark@example.invalid commit -qm corpus
rtk proxy taskset -c 0-3 python3 scripts/mcp_merge_bench.py \
  --binary "v1.19.0=$RAVEL_BENCH/target-before/release/ravel" \
  --binary "merged=$RAVEL_BENCH/target-merged/release/ravel" \
  --binary "optimized=$RAVEL_BENCH/target-optimized/release/ravel" \
  --corpus "$RAVEL_BENCH/corpus" --work "$RAVEL_BENCH/run" \
  --output "$RAVEL_BENCH/result.json" \
  --rounds 3 --calls 100 --cold-runs 5 --paced-calls 20 \
  --assert-equivalent merged=optimized
```

Use `4 500` for the small corpus. Use a fresh `--work` plus `--skip-cold --skip-edits --explore-first --calls 1 --paced-calls 0` for the first-explore experiment. Every work directory is private to the benchmark; it refuses to overwrite an existing variant root.

The function inventory is reproducible with `scripts/rust_function_inventory.py`; its docstring pins Python/tree-sitter versions. Callgrind profiles can be decompressed with Python/gzip and opened in KCachegrind or `callgrind_annotate --inclusive=yes --auto=no`. Inclusive instruction counts should be compared using the binary hashes in the manifest, not the common CLI version string.
