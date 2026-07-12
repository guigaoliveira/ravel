# Ravel — agent harness (Claude Code, Codex, Cursor, Grok, …)

Local **TS/JS** code graph for standalone projects and monorepos. Index once, query cheaply, **sync after edits**. Prefer ravel over blind multi-file grep for callers / impact / search.

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
| Mass rename / blast radius | **`ravel refactor Foo`** → `files[]` + risk |
| Full rebuild (rare) | `ravel index` |
| Live while coding | `ravel watch` (reindexes on change) |

## Prefer fewer tool calls (token budget)

1. **`context SYMBOL`** — search + callers + callees + impact (**one** call)  
2. **`refactor SYMBOL`** — `files` + risk for renames / blast radius  
3. Only if needed: `search`, `query --reverse`, `cycles`, `hubs`, `endpoints`  
4. **Edit with the agent’s own editor** — ravel does not replace ApplyPatch; it tells you *which* files matter  
5. JSON is **compact by default** (`--pretty` only for humans)  
6. MCP advertises **3 primary tools** only — `RAVEL_MCP_TOOLS=all` if you need CI/export/hubs via MCP

Avoid: multi-hop Grep/Glob/Read to rediscover imports.

CLI is often **cheaper in tokens** than MCP (no tool schema). MCP wins for **warm multi-query sessions**.

## Coverage (automatic)

- **Languages (auto):** `.ts` `.tsx` `.mts` `.cts` `.js` `.jsx` `.mjs` `.cjs` — no language list required.
- **Always ignored:** `node_modules`, `dist`, `build`, `coverage`, `.git`, `.next`, `.turbo`, `.ravel`, …  
- **Entry points (auto):** application entry files/controllers, `main.ts` / `bootstrap` — not reported as orphans.
- **Storage:** `.ravel/` sidecars — cold search/query without loading full snapshot.

## Debugging / refactors

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
