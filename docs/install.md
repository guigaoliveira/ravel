# Install & agent setup

Ravel uses a simple three-step workflow:

1. **Install the binary** (no Rust required when releases exist)
2. **`ravel install`** — wire MCP into every agent you use
3. **`ravel init`** per project (or `ravel index` when configuration is already present)

## 1. Install the CLI

### One-liner (recommended)

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.ps1 | iex
```

If a GitHub Release asset for your OS/arch is missing, the script falls back to `cargo install` from source.

### npm wrapper

```bash
npm install -g @guigaoliveira/ravel-cli
```

The package downloads the matching native binary from GitHub Releases during installation.

The package is published to npm as `@guigaoliveira/ravel-cli`. Release tags also
publish it to GitHub Packages. npm is the recommended registry for general use;
GitHub Packages requires registry authentication and is mainly useful for
GitHub-based workflows.

To install from GitHub Packages, authenticate with a GitHub personal access
token (classic) and route the scope in `.npmrc`:

```ini
@guigaoliveira:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

Env knobs:

| Variable | Meaning |
|----------|---------|
| `RAVEL_GITHUB_REPO` | `owner/repo` (default `guigaoliveira/ravel`) |
| `RAVEL_VERSION` | `latest` or a release such as `1.1.0` |
| `RAVEL_INSTALL_DIR` | binary destination |
| `RAVEL_FROM_SOURCE=1` | skip prebuilt; force cargo |

### From source (Rust)

```bash
cargo install --path crates/ravel-cli --locked
# or
cargo build -p ravel-cli --release
# → target/release/ravel
```

There is no crates.io dependency for the public installation path. Use the
GitHub release installer or build from source when a platform asset is absent.

## 2. Wire agents (global, once)

```bash
# Auto-detect Claude Code, Cursor, Codex, OpenCode, Gemini, Windsurf, VS Code, Grok
ravel install

# Explicit
ravel install --target claude,cursor,codex --location global

# Project-local MCP only
ravel install --target claude --location local

# Preview without writing
ravel install --print-config cursor
ravel install --print-config codex
```

What it writes:

| Agent | Global config | Local config | Instructions |
|-------|---------------|--------------|--------------|
| Claude Code | `~/.claude.json` `mcpServers` (`$CLAUDE_CONFIG_DIR/.claude.json` when set) | `.mcp.json` | `CLAUDE.md` / `AGENTS.md`; skill in `~/.claude/skills/ravel/` or `$CLAUDE_CONFIG_DIR/skills/ravel/` (local: `.claude/skills/ravel/`) |
| Cursor | `~/.cursor/mcp.json` | `.cursor/mcp.json` | `.cursor/rules/ravel.mdc` if the project has `.cursor/` |
| Codex | `$CODEX_HOME/config.toml` (default `~/.codex`) | `.codex/config.toml` (trusted projects) | `AGENTS.md`; skill in `~/.agents/skills/ravel/` (local: `.agents/skills/ravel/`) |
| OpenCode | `~/.config/opencode/opencode.json` (`$XDG_CONFIG_HOME/opencode/` when set; same on macOS and Windows) | `opencode.json` | `AGENTS.md` |
| Gemini CLI | `~/.gemini/settings.json` | `.gemini/settings.json` | `GEMINI.md` if present |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | — | — |
| VS Code | user `mcp.json` | `.vscode/mcp.json` | — |
| Grok | — (CLI via `AGENTS.md`) | — | `AGENTS.md` |

MCP always launches:

```text
<absolute-path-to-ravel> serve --mcp
```

so agents don’t depend on PATH quirks. Project root is the agent’s cwd (`--root` optional).

Project configs (`--location local`) are meant to be committed, and an absolute path
from one machine breaks on every other, so they launch plain `ravel` whenever it
is on your PATH (on Windows, `ravel.exe`). When it is not, the absolute path is
used and the install report says so — put `ravel` on PATH and re-run before
committing.

Re-running `ravel install` (after an upgrade or a move) refreshes the command and
arguments and keeps everything else you added to the `ravel` entry: `env`,
timeouts, tool allow-lists, per-tool approvals.

A config Ravel cannot parse — JSON with comments or trailing commas, for
example — is left untouched and reported as an `error` action naming the file,
and the command exits non-zero once the report is printed. An empty config file
counts as an empty object.

The `AGENTS.md` block is written into the directory you run the installer from
only when it is a project (it has `.git`, `package.json`, `tsconfig.json` or
`jsconfig.json`) or when you pass `--location local`; a global install run from
your home directory no longer leaves an `AGENTS.md` there. The skill covers every
other repository. Files that already carry the block are refreshed. A skill
directory you wrote yourself under the name `ravel` is never overwritten or
removed. `--no-instructions` skips both.

`ravel install --print-config claude` and `--print-config codex` also print the
equivalent `claude mcp add` / `codex mcp add` one-liner.

Claude Code specifics:

- A global install adds `mcp__ravel__*` to the `permissions.allow` list in
  `~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR/settings.json` when that is
  set), creating the file when Claude Code has not written
  one yet, so Ravel's tools run without a prompt per call. `--no-permissions`
  skips it. Project installs do not touch permissions: a
  `.claude/settings.local.json` created by anything other than Claude Code is
  not kept out of git automatically.
- Claude Code reads both the user config and the project `.mcp.json`. When both
  name a `ravel` server with different commands — a global install followed by a
  project one does exactly that — `claude mcp list` reports a scope conflict and
  the project entry wins. The install report warns about it and names the
  `claude mcp remove` that resolves it; keep one scope per machine.

### Uninstall agents

```bash
ravel uninstall
ravel uninstall --target cursor --location global
```

`.ravel/` indexes are **not** deleted.

## 3. Initialize each project

```bash
cd your-project
ravel init           # creates config and builds the initial index
# ravel init --no-index  # configuration only
ravel status
ravel context PaymentService
```

Daily:

- MCP watches indexed roots automatically; use `sync(paths)` when the edited paths are known
- CLI queries auto-sync git-dirty sources; `ravel sync <paths>` is the fastest explicit path
- `ravel watch` is for a long-lived CLI-only session

For multiple projects, pass the MCP tool's absolute `root`; relying on the MCP
process working directory is host-dependent. Multiple MCP clients for one root
share a transient local daemon, watcher, cache, and writer automatically.
For a persistent CLI-only daemon, use `ravel daemon start|status|stop`.

## Doctor

```bash
ravel doctor
# → index health + detected agents + what is wired, per agent
```

For each agent, `wired` says whether the global and project MCP configs carry a
`ravel` entry and whether the skill is present — the question to ask first when
an agent never reaches for the graph.

## MCP primary tools (token tax)

Default MCP exposes **5 tools** (`explore`, `callers_of`, `calls_from`,
`status`, `sync`). Full set:

```bash
RAVEL_MCP_TOOLS=all ravel serve --mcp
```

Or set that env in the agent’s MCP config `env` block. Codex starts MCP servers
with a cleaned environment, so a `RAVEL_*` variable exported in your shell does
not reach Ravel there; put it in the config instead:

```toml
[mcp_servers.ravel.env]
RAVEL_MCP_TOOLS = "all"
```

Tools carry MCP annotations: `readOnlyHint: true` on every query and
`destructiveHint: false` on `sync`, all with `openWorldHint: false`. Codex's
default `auto` approval mode asks before any tool without those hints. Failed
calls set `isError`, so the model is told the call failed instead of reading an
error body as an answer.

## Multi-OS notes

| OS | Binary asset name | Notes |
|----|-------------------|-------|
| Linux x64 | `ravel-x86_64-unknown-linux-gnu.tar.gz` | glibc |
| Linux arm64 | `ravel-aarch64-unknown-linux-gnu.tar.gz` | glibc |
| macOS Intel | `ravel-x86_64-apple-darwin.tar.gz` | |
| macOS Apple Silicon | `ravel-aarch64-apple-darwin.tar.gz` | |
| Windows x64 | `ravel-x86_64-pc-windows-msvc.zip` | |

Release binaries are published by GitHub Actions. If a prebuilt asset is not
available for your platform, the installers fall back to building from source.
