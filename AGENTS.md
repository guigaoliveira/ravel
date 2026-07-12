# Ravel — agent harness (Claude Code, Codex, Cursor, Grok, …)

Local **TS/JS** code graph for projects. Index once, query cheaply, **sync after edits**. Prefer ravel over blind multi-file grep for callers / impact / search.

## Install

```bash
# Binary (macOS/Linux) — or see README.md for Windows / cargo
curl -fsSL https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.sh | sh

ravel install --yes                 # wire MCP: Claude, Cursor, Codex, OpenCode, Gemini, …
cd /path/to/repo && ravel index
```

Or from source: `cargo install --path crates/ravel-cli --locked` then `ravel install --yes`.

## Daily loop

| When | Command |
|------|---------|
| Session start | `ravel cheatsheet` then `status` |
| After save/edit/delete | Auto on `query`/`search`/`context` (git dirty); or `sync` / `watch` |
| Understand a symbol | **`ravel context PaymentService`** (one call) |
| Full rebuild (rare) | `ravel index` |
| Live while coding | `ravel watch` (reindexes on change) |

## Prefer fewer tool calls (token budget)

1. **`context SYMBOL`** — search + callers + callees + impact (**one** call)
2. Only if needed: `search`, `query --reverse`, `impact`, `cycles`, `hubs`, `endpoints`
3. **Edit with the agent’s own editor** — ravel does not write source files
4. JSON is **compact by default** (`--pretty` only for humans)
5. MCP advertises **3 primary tools** only — `RAVEL_MCP_TOOLS=all` if you need CI/export/hubs via MCP

Avoid: multi-hop Grep/Glob/Read to rediscover imports.

CLI is often **cheaper in tokens** than MCP (no tool schema). MCP wins for **warm multi-query sessions**.

## Coverage (automatic)

- **Languages (auto):** `.ts` `.tsx` `.mts` `.cts` `.js` `.jsx` `.mjs` `.cjs` — no language list required.
- **Always ignored:** `node_modules`, `dist`, `build`, `coverage`, `.git`, `.next`, `.turbo`, `.ravel`, …  
- **Entry points (auto):** application entry files/controllers, `main.ts` / `bootstrap` — not reported as orphans.
- **Storage:** `.ravel/` sidecars — cold search/query without loading full snapshot.

## Debugging

```bash
ravel search Foo --kind prefix --limit 20
ravel query Foo --reverse          # who depends on me
ravel impact Foo --risk            # blast radius + high/medium/low
ravel diff-impact HEAD~1           # impact of local git diff
ravel cochanged path/to/file.ts
ravel boundaries                   # optional ravel.boundaries.toml
ravel ci --cycle-threshold 5
```

## Multi-project

Always pass `--root /abs/path`. MCP engines are cached **per root**. Index each project once under its own `.ravel/`.
