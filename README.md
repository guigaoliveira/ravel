<p align="center">
  <img src="assets/ravel-cover.png" alt="Ravel — local code graph for AI coding agents" width="100%">
</p>

<p align="center">
  <a href="https://github.com/guigaoliveira/ravel/actions/workflows/ci.yml"><img src="https://github.com/guigaoliveira/ravel/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://www.npmjs.com/package/@guigaoliveira/ravel-cli"><img src="https://img.shields.io/npm/v/%40guigaoliveira%2Fravel-cli?logo=npm&label=npm" alt="npm version"></a>
  <a href="https://github.com/guigaoliveira/ravel/releases"><img src="https://img.shields.io/github/v/release/guigaoliveira/ravel?logo=github" alt="GitHub release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/guigaoliveira/ravel" alt="Apache-2.0 license"></a>
</p>

<p align="center">
  <strong>Fast, local code context for AI coding agents.</strong>
</p>

Ravel indexes symbols and relationships so agents can find callers,
dependencies, and change impact without crawling the repository every time.
Use it through the CLI or any MCP client.

## Why Ravel?

Agents waste time and tokens rebuilding context with repeated searches and file
reads. Ravel indexes once and returns the relevant graph directly.

It is written in Rust and built for fast repeated queries: compact sidecars,
incremental sync, and a small MCP surface. Everything runs locally. No API key,
hosted service, external database, or source upload.

## Install

```bash
npm install -g @guigaoliveira/ravel-cli

# macOS / Linux, without npm
curl -fsSL https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.sh | sh

# Windows PowerShell, without npm
irm https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.ps1 | iex
```

Prebuilt binaries cover Linux (glibc and musl), macOS, and Windows on x64 and
ARM64. The npm installer requires Node.js 16 or newer.

Connect your agents and index a project:

```bash
ravel install
cd /path/to/project
ravel init
ravel context PaymentService
```

`ravel install` detects Claude Code, Cursor, Codex, OpenCode, Gemini, Windsurf,
VS Code, and Grok. Restart agents that were already running.

## Use

```bash
ravel context QUERY                   # exact/qualified symbol or natural terms + source/relations
ravel search QUERY --kind terms       # exact, prefix, fuzzy, regex, or lexical terms
ravel query SYMBOL --reverse          # reverse dependencies
ravel impact SYMBOL --risk            # blast radius and risk
ravel diff-impact HEAD~1              # impact of a Git diff
ravel sync path/to/file.ts            # fastest explicit sync after an edit
```

Output is compact JSON by default. Add `--pretty` for humans. Use
`--root /absolute/path` for another project. MCP watches each requested root
automatically unless `sync.mode = "none"`. In CLI-only workflows, `search`, `query`, and `context` auto-sync
Git changes; use `ravel sync <paths>` after known edits or `ravel watch` for
continuous updates.

## Benchmarks

| Measured | Corpus | Result |
| --- | --- | --- |
| Warm read latency | 21k files, 235k edges | search 14 ms · query 20 ms · impact 23 ms · context 53 ms |
| Changed-path sync | 21k files | no-op/content-only <10 ms · structural edits 130–300 ms |

Local tool latency on one machine, not an agent-workflow benchmark or an SLA.
Structural edits (add, delete, rename) scale with the affected graph. See the
[performance notes](docs/performance.md).

## Language support

| Language | Extensions |
| --- | --- |
| TypeScript | `.ts`, `.mts`, `.cts` |
| TSX | `.tsx` |
| JavaScript | `.js`, `.mjs`, `.cjs` |
| JSX | `.jsx` |

All are detected automatically. Static extraction covers ESM and CommonJS
imports/exports, declarations and types, calls and construction, decorators,
class elements, JSX component references, resource management, import
attributes, and TypeScript 5.9 `import defer`. Dynamic dispatch remains
conservatively unresolved rather than producing guessed edges.

## MCP

`ravel install` configures MCP automatically. Ravel exposes only
`explore`, `status`, and `sync` by default to reduce tool-schema overhead.
Multiple agents and processes may share the same indexed root: updates are
serialized and published atomically, so readers keep the last complete index.
MCP clients for the same root share one transient local daemon, watcher, warm
cache, and writer. The daemon exits after the last MCP session disconnects;
`ravel daemon start|status|stop` controls a persistent CLI daemon explicitly.

```bash
RAVEL_MCP_TOOLS=all ravel mcp  # expose every analysis tool
```

## Community

Found a bug or have an idea? [Open an issue](https://github.com/guigaoliveira/ravel/issues).
Contributions are welcome; read [CONTRIBUTING.md](CONTRIBUTING.md) before opening
a pull request.

## Documentation

- [Installation](docs/install.md)
- [Configuration](docs/config.md)
- [Performance](docs/performance.md)
- [Changelog](CHANGELOG.md)

## License

[Apache-2.0](LICENSE)
