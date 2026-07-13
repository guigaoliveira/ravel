# Changelog

All notable changes to Ravel are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/guigaoliveira/ravel/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/guigaoliveira/ravel/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/guigaoliveira/ravel/releases/tag/v1.0.0
