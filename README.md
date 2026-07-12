# Ravel

Ravel is a local code graph for TypeScript and JavaScript projects. It indexes
symbols, imports, and code relationships so coding agents can answer
navigation and impact questions without repeatedly searching the repository.

Ravel works through the CLI and MCP. It is local, requires no API key, and does
not modify source files.

It works with standalone projects and monorepos alike.

## Installation

macOS/Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/guigaoliveira/ravel/main/scripts/install.ps1 | iex
```

Install from source:

```bash
cargo install --path crates/ravel-cli --locked
```

## Connect a coding agent

Configure Ravel once. The installer detects Claude Code, Cursor, Codex CLI,
OpenCode, Gemini CLI, Windsurf, VS Code, and Grok.

```bash
ravel install --yes
```

The default location is global. To configure only the current project:

```bash
ravel install --target claude,cursor --location local --yes
```

Useful options:

```bash
ravel install --print-config codex  # print a config snippet without writing
ravel uninstall --yes                # remove Ravel from configured agents
```

Restart the agent after installing or changing its MCP configuration.

## Index a project

```bash
cd your-project
ravel index
ravel status
ravel context PaymentService
```

Indexing is per project. `ravel install` configures agents; `ravel index`
builds the project graph.

`ravel init` is optional. It creates `.ravel.toml` and `.ravelignore` for
projects that need custom extensions, ignore rules, or sync settings. Ravel
defaults to the TypeScript/JavaScript family, skips common dependency/build
directories, and honors `.gitignore` when Git is available.

## Keep the index current

The MCP server auto-syncs Git-dirty files before requests. For CLI and scripted
workflows:

```bash
ravel sync                 # sync Git-dirty source files
ravel sync src/services.ts # sync explicit paths
ravel watch                # keep syncing while files change
```

Without Git, use explicit paths with `sync` or run `watch`.

## Main commands

| Command | Purpose |
|---------|---------|
| `context SYMBOL` | Symbol, callers, callees, and related impact |
| `refactor SYMBOL` | Files and risk for a broad change |
| `search QUERY` | Search symbols (`exact`, `prefix`, `fuzzy`, or `regex`) |
| `query SYMBOL --reverse` | Dependencies and callers of a symbol |
| `impact SYMBOL --risk` | Blast radius with risk classification |
| `status` / `doctor` | Index status and diagnostics |
| `sync` / `watch` | Incremental or continuous updates |

`context` is also available as `explore`. Output is compact JSON by default;
use `--pretty` for human-readable output.

To use another project root, pass it explicitly:

```bash
ravel --root /path/to/project context PaymentService
```

## MCP

`ravel install` configures MCP automatically. For a manual stdio setup:

```json
{
  "mcpServers": {
    "ravel": {
      "type": "stdio",
      "command": "ravel",
      "args": ["--root", "/path/to/project", "serve", "--mcp"]
    }
  }
}
```

The default MCP surface has three primary tools: `explore`, `status`, and
`sync`. Set `RAVEL_MCP_TOOLS=all` to expose the full tool set.

See all CLI options with:

```bash
ravel --help
```

## License

Apache-2.0
