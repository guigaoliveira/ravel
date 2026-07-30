# Changelog

All notable changes to Ravel are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.5.0] - 2026-07-30

**The index is rebuilt once on first use after upgrading.** Symbol metadata
changed on-disk layout, so `SCHEMA_VERSION` moves 15 -> 16 and auto-sync
rebuilds the index the first time a command runs. Nothing to do by hand.
Running an older binary against a 1.5.0 index also rebuilds, in its own
format — see the note under Changed for why that is deliberate.

### Changed
- Symbol lookups no longer decode a shard to reach one entry. Metadata shards
  are rkyv archives read in place, binary-searched in the mmap, and only the
  matching entry is owned. Validation is proportional to a record's size, so it
  runs once per shard per process; packs are immutable once published, and a new
  generation builds a new runtime. Isolated A/B over MCP on a 20.4k-file /
  744k-edge corpus: `context.candidates` 6.87ms -> 1.93ms (-72%), warm
  `explore` 17.25ms -> 7.97ms (-54%). Explore output is identical for 9 queries
  with each binary reading the format it writes.

  Two honest consequences. rkyv's relative pointers and alignment padding make
  these records slightly *larger*, not smaller — 182.3MB -> 188.2MB, +0.3% of
  the pack; the change earns its place on the read path, not on size. And the
  schema bump is not cosmetic: a binary predating this layout would otherwise
  reject only the shard index and then answer from an empty symbol-meta
  backend, which is indistinguishable from a genuine "nothing found". With the
  bump both directions report an unsupported schema and rebuild, so a downgrade
  is loud instead of silently empty.
- Full index is a further 13% faster on that corpus (9124ms in 1.4.1 ->
  7949-8071ms), snapshot id unchanged:
  - Resolution's two remaining sequential steps now fan out — building the
    resolution universe and ordering the resulting edges.
    `resolve.sort_edges` 281.4ms -> 54.7ms, `index.resolve_edges` 1240.0ms ->
    910.8ms.
  - Structural contributions are built per file in parallel: 475.8ms ->
    411.4ms. The small share confirms that stage is dominated by its
    sequential merge.
- `RAVEL_TIMING=1` reports resolution's phases: universe build,
  imports/exports, symbol refs, merge/dedup, and the edge sort.

### Notes for the next round
Two measured findings are recorded in the code rather than acted on, so they
are not rediscovered from scratch:
- `snapshot/edges` (293MB, 16% of the pack) is derivable from the graph's file
  section and is read only when hydrating a full snapshot, never by a query.
- `graph_from_edges` (1050ms) is dominated by ordered-map traversals over
  string keys, not by allocation — two attempts to cut its clones measured
  22% and 7% *slower*. Interning node ids is the structural fix, and it would
  also cut ~500MB from the pack.

## [1.4.1] - 2026-07-29

Released as 1.4.1: the 1.4.0 tag failed its own validation job (`cargo fmt
--check`) before publishing anything, and a tag that exists is not moved.
No 1.4.0 artifact was ever published, so this is the first release of the
work below.

### Added
- `this.<field>.<member>` calls resolve through the field's declared type.
  NestJS controllers and use-case classes are built almost entirely from
  constructor-injected services, so their call graph was largely invisible:
  the member lookup stopped at the owning class. Covers constructor
  injection, aliased imports, and interface-typed fields.
- Cursor pagination on the graph walk (`QueryLimits.cursor`, `ravel query
  --page-size N --cursor N`). Every page previously restarted at offset 0,
  which made the returned `next_cursor` unusable and hubs impossible to
  enumerate completely.
- Complete per-kind edge counts on explore (`incoming_by_kind` /
  `outgoing_by_kind`). Bounded by the number of edge kinds, so a hub reports
  its true shape even when the relation page truncates.
- File-level cycles (`ravel cycles --files`). Package-level SCCs collapse a
  monorepo whose top-level buckets all reach each other into one giant
  component, hiding the actionable cycles between files.
- `related-tests` also probes `src/` -> `test/` mirrors (NestJS/Jest layout).

### Changed
- `ImpactReport.exact` marks a budget-saturated `total_affected` as a lower
  bound instead of presenting it as the real total.
- Truncation warnings only promise a re-query limit the explore page can
  actually serve, and point at the paginated walk beyond that.
- `validate` returns a bounded findings page plus complete per-code counts
  (`--limit`); the raw list reaches megabytes on large monorepos.
- `context` builds the index when no snapshot exists instead of failing and
  telling the agent to run `ravel index` by hand.
- Warm query latency is 5x lower. Measured over MCP against a live server on
  a 20.4k-file / 744k-edge corpus, 3 alternating rounds against the previous
  binary: mean warm `explore` 70.2ms -> 13.7ms. Four causes, all in the read
  path:
  - Term search evaluated the union of every query token's postings. Query
    "PixPaymentService" tokenizes to pix/payment/service, covering 190671
    postings over 156022 distinct definitions — all scored and ranked to
    return 160. The scorer's ceiling per coverage level is an analytic
    constant, so once the k-th best definition carrying the rarest token
    scores strictly above the ceiling for one-fewer-token coverage, no
    skipped definition could have entered the result. Term search now walks
    the lists as a merge join driven by the rarest one and falls back to the
    full union when that test fails. Candidates 156022 -> 29831;
    `search_terms` 20-70ms -> 1.3ms.
  - Packed archives were re-validated on every query. `rkyv::access` walks
    the whole archive, and the term record is ~117MB on that corpus.
    Validation now happens once per runtime; generation packs are immutable
    once published, and a new generation drops the runtime.
  - Candidates were owned before ranking: three heap allocations per
    candidate, of which all but `limit` were discarded. They are now ranked
    while still borrowing from the index.
  - Auto-sync rebuilt the hash sidecar (20k+ owned paths) on every call to
    check index freshness: 14-21ms per call, now cached per generation.

  Explore and term-search output are byte-identical before/after for 35
  queries, including bare "service" / "pix" / "payment", a multi-token
  phrase, and a token absent from the index.
- Syncing a batch of edits is ~15x faster. Loading previous artifacts,
  re-parsing new bytes, and reading/hashing the batch are per-file work that
  ran one file at a time, and the artifact index was decoded once per path
  probed. 30-file sync on the corpus above: `sync.prepare_paths` 481ms ->
  17ms, `sync.structural_delta` 1587ms -> 103ms, total 2066ms -> 141ms.
- Full index is 23% faster: 11838ms -> 9124ms, 3 alternating rounds vs 1.3.0
  (tmpfs, to isolate CPU from disk). `stage_snapshot` 4649ms -> 2173ms.
  Term index construction, symbol-meta staging, artifact encoding and edge
  digesting now fan out across cores; snapshot id is unchanged throughout, so
  the built index is identical.
- Reference resolution uses per-artifact lookups instead of scanning the
  artifact's symbols once per reference: O(refs x symbols) -> O(1) per
  reference. Performance-neutral on the corpus above (its files are small);
  it bounds the cost for generated files with thousands of symbols. The
  lookups only needed for `this.<field>.<member>` are built on first use,
  since most files never contain such a reference.
- `RAVEL_TIMING=1` breaks down what was previously opaque: `stage_snapshot`
  and `stage_graph` (together ~40% of a full index), the context workers,
  the auto-sync steps, dirty discovery, both halves of `prepare_paths`,
  artifact-delta publication and compaction, and what the candidate loop
  actually does. `context.prefix` in particular used to measure a whole
  parallel scope rather than search.

### Fixed
- `diff-impact` mapped nothing: git returns absolute paths while graph nodes
  are workspace-relative, so every changed file was skipped.
- An upgraded binary could route queries to a daemon from an older install
  (same wire protocol, older semantics). The runtime endpoint and singleton
  lock now embed the binary version.
- A daemon that is shutting down answers politely instead of dropping the
  connection; that reply surfaced "daemon is shutting down" to the agent
  rather than respawning.

## [1.3.0] - 2026-07-17

### Changed
- Search backend moved off `tantivy` to a custom `rkyv` + `memmap2` sectioned
  reverse index (definition-level inverted term index plus spelling dictionary),
  read zero-copy through the generation-pack mmap reader. On a 20.6k-file corpus:
  cold one-shot search 3–6× faster (exact/prefix 60→10ms, fuzzy 60→20ms, terms
  80→30ms), `.ravel` −10% (1.9→1.7GB), peak RSS −6%, release binary −18%
  (18.0→14.8MB), and the `tantivy` dependency tree removed (−601 Cargo.lock
  lines). Result value-sets are identical before/after for every search kind.
- Trade-off: full workspace index is ~21% slower (~10.0→12.1s); it is a rare,
  one-time operation off the agent hot path.

### Fixed
- Term-search paths bound-check `document_index` against the document count
  (`.get()` + skip) rather than indexing directly, so a corrupt or
  version-skewed on-disk pack degrades gracefully instead of panicking — rkyv's
  structural validation does not enforce that semantic invariant.

## [1.2.0] - 2026-07-17

### Changed
- Structural sync cold path rebuilt around a sectioned graph base pack:
  independent `graph/file`, `graph/edge`, `graph/adj` key-spaces so a single-file
  sync decodes only the section it touches; edge refcounts keyed by blake3-128 of
  the edge bytes. Cold one-shot structural sync 12.5s → ~0.76s, graph section
  read 785MB → ~94MB, peak sync RSS 3.9GB → ~0.69GB.
- Warm daemon structural sync 11.5–19s → ~0.26s via partial delta state,
  resident readers carried across generations, adaptive delta overlays
  (63MB → 3.9MB per sync), and parallel shard decode.
- Watcher reconcile runs incremental sync instead of a full reindex on backend
  rescan; full reindex only on non-git roots.
- Full index parallelised (parallel per-artifact resolution, parallel publish).

## [1.1.0] - 2026-07-12

### Added
- Shared per-workspace daemon with transient MCP leases, persistent CLI control,
  per-root caching, bounded connections, and watcher leadership failover.
- Incremental generation packs, artifact deltas, structural reverse indexes,
  atomic failpoint coverage, and bounded generation garbage collection.
- Configurable bounds for watcher storms, sync tickets, daemon connections,
  retained generations, and artifact-store amplification.

### Changed
- Explicit sync batching is contention-driven; a single writer no longer waits
  for a fixed coalescing window.
- Changed-path no-op checks use point lookups instead of materializing all file
  hashes. Worktree identity and large sidecars are cached per engine.
- Structural acceleration is resident-only on sync, avoiding a cold path that
  used more time and memory than the exact fallback.
- Common CLI/MCP reads use compact sidecars and bounded/LRU caches; packages,
  fuzzy search, graph limits, impact counts, and byte limits were tightened.
- Installers and release artifacts verify SHA-256 checksums; release workflows
  pin actions and validate packages before publication.

### Fixed
- Cross-process writer races, stale-generation cache reads, watcher event storms,
  daemon lease exhaustion, blocked daemon startup, and GC/reader deadlocks.
- Add/delete/rename equivalence, idempotent A-to-B-to-A publication, stale hubs,
  Git path handling, co-change history, boundary matching, and fuzzy ranking.
- Concurrent agent configuration writes and archive extraction validation.

## [1.0.0] - 2026-07-12

Initial public release.

### Added
- Local code graph for TypeScript / JavaScript codebases — index once, query cheaply.
- Sub-200ms agent hot path over `.ravel/` sidecars (cold CLI, existing index).
- CLI commands: `index`, `sync`, `watch`, `status`, `context`,
  `search`, `query`, `impact`, `hubs`, `cheatsheet`, `doctor`, `install`, `uninstall`.
- MCP server (`ravel mcp`) with 3 primary tools by default; `RAVEL_MCP_TOOLS=all` for the full set.
- One-shot agent wiring (`ravel install`) for Claude Code, Cursor, Codex, OpenCode,
  Gemini, Windsurf, VS Code, Grok, and more.
- Compact JSON output by default (`--pretty` for humans).
- Zero-config defaults with optional `.ravel.toml` overrides (extensions, ignore, sync).
- Git-optional operation: auto-sync when `.git` is present, `watch`/`sync <paths>` otherwise.
- Automatic entry-point detection for application entry files/controllers and `main.ts` / `bootstrap`.
- Install scripts (curl / PowerShell), npm distribution, and `cargo install` from source.

[Unreleased]: https://github.com/guigaoliveira/ravel/compare/v1.5.0...HEAD
[1.5.0]: https://github.com/guigaoliveira/ravel/compare/v1.4.1...v1.5.0
[1.4.1]: https://github.com/guigaoliveira/ravel/compare/v1.3.0...v1.4.1
[1.3.0]: https://github.com/guigaoliveira/ravel/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/guigaoliveira/ravel/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/guigaoliveira/ravel/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/guigaoliveira/ravel/releases/tag/v1.0.0
