# Changelog

All notable changes to Ravel are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/guigaoliveira/ravel/compare/v1.3.0...HEAD
[1.3.0]: https://github.com/guigaoliveira/ravel/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/guigaoliveira/ravel/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/guigaoliveira/ravel/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/guigaoliveira/ravel/releases/tag/v1.0.0
