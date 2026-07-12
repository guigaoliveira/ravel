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

Env knobs:

| Variable | Meaning |
|----------|---------|
| `RAVEL_GITHUB_REPO` | `owner/repo` (default `guigaoliveira/ravel`) |
| `RAVEL_VERSION` | `latest` or `1.0.0` |
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
ravel install --yes

# Explicit
ravel install --target claude,cursor,codex --location global --yes

# Project-local MCP only
ravel install --target claude --location local --yes

# Preview without writing
ravel install --print-config cursor
ravel install --print-config codex
```

What it writes:

| Agent | Global config | Local config | Instructions |
|-------|---------------|--------------|--------------|
| Claude Code | `~/.claude.json` `mcpServers` | `.mcp.json` | `CLAUDE.md` / `AGENTS.md` |
| Cursor | `~/.cursor/mcp.json` | `.cursor/mcp.json` | `.cursor/rules/ravel.mdc` if `.cursor/` exists |
| Codex | `~/.codex/config.toml` | `.codex/config.toml` | `AGENTS.md` |
| OpenCode | `~/.config/opencode/opencode.json` | `opencode.json` | `AGENTS.md` |
| Gemini CLI | `~/.gemini/settings.json` | `.gemini/settings.json` | `GEMINI.md` if present |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | — | — |
| VS Code | user `mcp.json` | `.vscode/mcp.json` | — |
| Grok | — (CLI via `AGENTS.md`) | — | `AGENTS.md` |

MCP always launches:

```text
<absolute-path-to-ravel> mcp
```

so agents don’t depend on PATH quirks. Project root is the agent’s cwd (`--root` optional).

### Uninstall agents

```bash
ravel uninstall --yes
ravel uninstall --target cursor --location global --yes
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

- `context` / `search` / `query` auto-sync git-dirty sources
- or `ravel sync` / `ravel watch`

## Doctor

```bash
ravel doctor
# → index health + detected agents + binary path
```

## MCP primary tools (token tax)

Default MCP exposes **3 tools** (`explore`, `status`, `sync`). Full set:

```bash
RAVEL_MCP_TOOLS=all ravel mcp
```

Or set that env in the agent’s MCP config `env` block.

## Multi-OS notes

| OS | Binary asset name | Notes |
|----|-------------------|-------|
| Linux x64 | `ravel-x86_64-unknown-linux-gnu.tar.gz` | glibc |
| Linux arm64 | `ravel-aarch64-unknown-linux-gnu.tar.gz` | glibc |
| macOS Intel | `ravel-x86_64-apple-darwin.tar.gz` | |
| macOS Apple Silicon | `ravel-aarch64-apple-darwin.tar.gz` | |
| Windows x64 | `ravel-x86_64-pc-windows-msvc.zip` | |

Release binaries are uploaded manually to GitHub Releases. If a prebuilt asset is
not available for your platform, the installers fall back to building from source.
