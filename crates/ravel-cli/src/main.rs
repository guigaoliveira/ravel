use clap::{Parser, Subcommand, ValueEnum};
use ravel_core::{
    config::Flags, engine::WorkspaceEngine, graph::QueryLimits, health, search::SearchKind,
};
use std::{path::PathBuf, time::Duration};

#[derive(Debug, Parser)]
#[command(
    name = "ravel",
    version,
    about = "Dependency indexer for large codebases (CLI-first for AI agents; MCP shares the same engine)"
)]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,
    /// Pretty-print JSON (default is compact — saves tokens for agents)
    #[arg(long, global = true)]
    pretty: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create .ravel.toml + .ravelignore
    Init,
    /// Full workspace index (slow; use after clone or large refactors)
    Index,
    /// Incremental update: re-parse git-dirty or listed files only (daily edits)
    Sync {
        /// Optional explicit paths (relative or absolute). Default: git dirty sources.
        paths: Vec<PathBuf>,
    },
    /// Index health + sidecar presence (agent session start)
    Status,
    /// One-shot agent context: search + callers + callees + impact (fewer tool hops)
    #[command(alias = "explore")]
    Context {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Refactor briefing: files_to_touch + risk counts (token-cheap mass rename/blast radius)
    Refactor {
        symbol: String,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Install agent harness files (AGENTS.md / CLAUDE.md snippet + MCP example)
    /// Prefer `ravel install` for multi-agent MCP wiring (CodeGraph-style).
    Setup {
        #[arg(long)]
        claude: bool,
        #[arg(long)]
        force: bool,
    },
    /// Wire Ravel MCP into coding agents (Claude, Cursor, Codex, OpenCode, Gemini, …)
    ///
    /// Examples:
    ///   ravel install --yes
    ///   ravel install --target claude,cursor --location global --yes
    ///   ravel install --print-config codex
    Install {
        /// Agents: auto | all | csv (claude,cursor,codex,opencode,gemini,windsurf,vscode,grok)
        #[arg(long, default_value = "auto")]
        target: String,
        /// global (user home) | local (project)
        #[arg(long, default_value = "global")]
        location: String,
        /// Non-interactive
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print MCP snippet for one agent and exit (no writes)
        #[arg(long, value_name = "AGENT")]
        print_config: Option<String>,
        /// Skip AGENTS.md / CLAUDE.md instruction markers
        #[arg(long)]
        no_instructions: bool,
        /// Skip Claude mcp__ravel__* allowlist tweak
        #[arg(long)]
        no_permissions: bool,
    },
    /// Remove Ravel MCP config from agents (indexes under .ravel/ kept)
    Uninstall {
        #[arg(long, default_value = "auto")]
        target: String,
        #[arg(long, default_value = "global")]
        location: String,
        #[arg(long, short = 'y')]
        yes: bool,
        #[arg(long)]
        no_instructions: bool,
    },
    /// Quick environment check (+ agent detection)
    Doctor,
    /// Guess related test files for a source path (Nest/Jest/Vitest)
    RelatedTests {
        path: String,
    },
    Query {
        node: String,
        #[arg(long)]
        reverse: bool,
        #[arg(long, default_value_t = 32)]
        depth: usize,
        #[arg(long, default_value_t = 10_000)]
        nodes: usize,
    },
    Search {
        query: String,
        #[arg(long, value_enum, default_value_t = SearchMode::Exact)]
        kind: SearchMode,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Impact {
        node: String,
        #[arg(long, default_value_t = 32)]
        depth: usize,
        /// Include high/medium/low risk scores (T016)
        #[arg(long)]
        risk: bool,
    },
    Cycles {
        #[arg(long)]
        package: Option<String>,
    },
    Hubs {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Filter by kind/path substring (e.g. class, controller, injectable)
        #[arg(long)]
        kind: Option<String>,
    },
    Orphans {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Packages,
    DiffImpact {
        /// Git ref to diff from (e.g. HEAD~1)
        from: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value_t = 16)]
        depth: usize,
    },
    Export {
        #[arg(long, default_value = "dot")]
        format: String,
        #[arg(long, default_value = "package")]
        scope: String,
    },
    Ci {
        #[arg(long)]
        strict: bool,
        #[arg(long, default_value_t = 2)]
        cycle_threshold: usize,
    },
    /// Files that tend to change in the same commits as `<file>` (git history)
    Cochanged {
        file: String,
        #[arg(long, default_value_t = 100)]
        commits: usize,
        #[arg(long, default_value_t = 2)]
        min_cooccurrence: u32,
    },
    Validate,
    /// Architecture boundary violations (ravel.boundaries.toml)
    Boundaries,
    /// Schema summary: counts by node/edge kind
    Schema,
    Stats,
    Watch,
    /// ~150-token agent map (session start; CodeGraph/CRG pattern)
    Cheatsheet,
    /// Long-lived MCP stdio server (reuse engine cache across calls).
    /// Default: primary tools only (explore, status, sync). Set RAVEL_MCP_TOOLS=all for full.
    Mcp,
    /// Persistent MCP server with auto-sync via file watcher (codegraph-style).
    /// Keeps graph in-memory, watches for changes, syncs automatically.
    /// Default: primary tools only. Set RAVEL_MCP_TOOLS=all for full surface.
    Serve {
        #[arg(long)]
        mcp: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SearchMode {
    Exact,
    Prefix,
    Fuzzy,
    Regex,
}
impl From<SearchMode> for SearchKind {
    fn from(value: SearchMode) -> Self {
        match value {
            SearchMode::Exact => Self::Exact,
            SearchMode::Prefix => Self::Prefix,
            SearchMode::Fuzzy => Self::Fuzzy,
            SearchMode::Regex => Self::Regex,
        }
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .try_init()
            .ok();
    }
    let cli = Cli::parse();
    let root = cli.root.canonicalize().unwrap_or(cli.root);
    let pretty = cli.pretty;
    match cli.command {
        Some(Command::Init) => {
            std::fs::create_dir_all(&root)?;
            // Config is optional: defaults auto-detect TS/JS and ignore noise dirs.
            let path = root.join(".ravel.toml");
            if !path.exists() {
                std::fs::write(
                    &path,
                    r#"# Optional — zero-config works; edit to extend.
# See README.md

[project]
root = "."

[parser]
max_file_size_kb = 1024
languages = ["auto"]
# Or fully custom (wins over languages):
# extensions = ["ts", "tsx", "js", "vue"]

[ignore]
# dirs = ["storybook-static", "generated"]
use_builtin_dirs = true
gitignore = true

[sync]
mode = "auto"              # auto | git | none
auto = true
include_untracked = false  # keep false for <200ms hot path
skip_sibling_emit = true
"#,
                )?;
            }
            let ignore = root.join(".ravelignore");
            if !ignore.exists() {
                std::fs::write(
                    ignore,
                    "# Extra gitignore-style patterns (optional)\n\
                     # Built-in dirs already skipped: node_modules, dist, build, …\n\
                     # *.generated.ts\n",
                )?;
            }
            println!(
                "initialized {} (defaults: TS/JS auto, builtin noise dirs, git sync if present)",
                root.display()
            );
        }
        Some(Command::Index) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let stats = engine.index()?;
            emit_json(&stats, pretty)?;
        }
        Some(Command::Sync { paths }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let abs: Vec<PathBuf> = paths
                .into_iter()
                .map(|p| if p.is_absolute() { p } else { root.join(p) })
                .collect();
            let stats = if abs.is_empty() {
                engine.sync(None)?
            } else {
                engine.sync(Some(&abs))?
            };
            emit_json(&stats, pretty)?;
        }
        Some(Command::Status) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.status()?, pretty)?;
        }
        Some(Command::Context { query, limit }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.context(&query, limit)?, pretty)?;
        }
        Some(Command::Refactor { symbol, limit }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.refactor_plan(&symbol, limit)?, pretty)?;
        }
        Some(Command::Setup { claude, force }) => {
            write_agent_setup(&root, claude, force)?;
            println!("agent setup written under {}", root.display());
            println!("tip: run `ravel install --yes` to wire MCP into Claude/Cursor/Codex/…");
        }
        Some(Command::Install {
            target,
            location,
            yes: _,
            print_config,
            no_instructions,
            no_permissions,
        }) => {
            let bin = ravel_core::install::resolve_ravel_bin();
            if let Some(agent) = print_config {
                let kind = ravel_core::install::AgentKind::parse_csv(&agent)
                    .map_err(anyhow::Error::msg)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("unknown agent for --print-config"))?;
                let loc = ravel_core::install::InstallLocation::parse(&location)
                    .map_err(anyhow::Error::msg)?;
                print!("{}", ravel_core::install::print_config(kind, &bin, loc));
            } else {
                let targets = ravel_core::install::AgentKind::parse_csv(&target)
                    .map_err(anyhow::Error::msg)?;
                let loc = ravel_core::install::InstallLocation::parse(&location)
                    .map_err(anyhow::Error::msg)?;
                let opts = ravel_core::install::InstallOptions {
                    targets,
                    location: loc,
                    project_root: root.clone(),
                    ravel_bin: bin,
                    write_instructions: !no_instructions,
                    claude_permissions: !no_permissions,
                };
                let report = ravel_core::install::install_agents(&opts)?;
                emit_json(&report, pretty)?;
            }
        }
        Some(Command::Uninstall {
            target,
            location,
            yes: _,
            no_instructions,
        }) => {
            let targets =
                ravel_core::install::AgentKind::parse_csv(&target).map_err(anyhow::Error::msg)?;
            let loc = ravel_core::install::InstallLocation::parse(&location)
                .map_err(anyhow::Error::msg)?;
            let opts = ravel_core::install::InstallOptions {
                targets,
                location: loc,
                project_root: root.clone(),
                ravel_bin: ravel_core::install::resolve_ravel_bin(),
                write_instructions: !no_instructions,
                claude_permissions: false,
            };
            let report = ravel_core::install::uninstall_agents(&opts)?;
            emit_json(&report, pretty)?;
        }
        Some(Command::Doctor) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let status = engine.status()?;
            let agents = ravel_core::install::doctor_agents(&root);
            emit_json(
                &serde_json::json!({
                    "index": status,
                    "agents": agents,
                    "binary": ravel_core::install::resolve_ravel_bin().display().to_string(),
                }),
                pretty,
            )?;
        }
        Some(Command::RelatedTests { path }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.related_tests(&path)?, pretty)?;
        }
        Some(Command::Query {
            node,
            reverse,
            depth,
            nodes,
        }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let limits = QueryLimits {
                depth,
                nodes,
                ..Default::default()
            };
            emit_json(&engine.query(&node, reverse, &limits, None)?, pretty)?;
        }
        Some(Command::Search { query, kind, limit }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.search(&query, kind.into(), limit)?, pretty)?;
        }
        Some(Command::Impact { node, depth, risk }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let limits = QueryLimits {
                depth,
                ..Default::default()
            };
            if risk {
                emit_json(&engine.impact_risk(&node, &limits)?, pretty)?;
            } else {
                emit_json(&engine.query(&node, false, &limits, None)?, pretty)?;
            }
        }
        Some(Command::Cycles { package }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.cycles(package.as_deref())?, pretty)?;
        }
        Some(Command::Hubs { limit, kind }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.hubs(limit, kind.as_deref())?, pretty)?;
        }
        Some(Command::Orphans { limit }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.orphans(limit)?, pretty)?;
        }
        Some(Command::Packages) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.list_packages()?, pretty)?;
        }
        Some(Command::DiffImpact { from, to, depth }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let limits = QueryLimits {
                depth,
                ..Default::default()
            };
            emit_json(&engine.diff_impact(&from, to.as_deref(), &limits)?, pretty)?;
        }
        Some(Command::Export { format, scope }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            if format != "dot" || scope != "package" {
                anyhow::bail!("only --format dot --scope package is supported currently");
            }
            print!("{}", engine.export_dot()?);
        }
        Some(Command::Ci {
            strict,
            cycle_threshold,
        }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let report = engine.ci(strict, cycle_threshold)?;
            emit_json(&report, pretty)?;
            if !report.passed {
                std::process::exit(1);
            }
        }
        Some(Command::Cochanged {
            file,
            commits,
            min_cooccurrence,
        }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.cochanged(&file, commits, min_cooccurrence)?, pretty)?;
        }
        Some(Command::Validate) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let findings = engine.validate()?;
            emit_json(&findings, pretty)?;
            if !findings.is_empty() {
                anyhow::bail!("index validation failed with {} finding(s)", findings.len());
            }
        }
        Some(Command::Boundaries) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.boundaries()?, pretty)?;
        }
        Some(Command::Schema) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.describe_schema()?, pretty)?;
        }
        Some(Command::Stats) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.stats()?, pretty)?;
        }
        Some(Command::Watch) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let extensions = ravel_core::config::effective_extensions(&engine.config);
            eprintln!(
                "watching {} (reindex on change; Ctrl-C to stop)",
                root.display()
            );
            loop {
                let batch = ravel_core::watch::watch_batch(
                    &root,
                    Duration::from_millis(200),
                    Duration::from_secs(3600),
                );
                let Ok(result) = batch else {
                    continue;
                };
                let cfg = &engine.config;
                let paths: Vec<_> = result
                    .paths
                    .into_iter()
                    .filter(|p| cfg.is_source_with_extensions(p, &extensions) && !cfg.is_noise(p))
                    .collect();
                if paths.is_empty() && !result.needs_reconcile {
                    continue;
                }
                let stats = if result.needs_reconcile || paths.is_empty() {
                    engine.index()?
                } else {
                    engine.sync(Some(&paths))?
                };
                println!("{}", serde_json::to_string(&stats)?);
            }
        }
        Some(Command::Cheatsheet) => {
            // ~150 tokens — inject once per agent session (CodeGraph/CRG pattern)
            print!("{}", ravel_cheatsheet());
        }
        Some(Command::Mcp) => serve_mcp()?,
        Some(Command::Serve { mcp }) => {
            if !mcp {
                anyhow::bail!("use `ravel serve --mcp` to start the MCP server");
            }
            // Persistent MCP server with auto-sync (codegraph serve --mcp style).
            // Each tool call already auto-syncs via engine.auto_sync_if_dirty().
            // Staleness info embedded in explore response via auto_synced field.
            // Primary tools: explore, status, sync. Set RAVEL_MCP_TOOLS=all for full.
            eprintln!("ravel serve --mcp (persistent, auto-sync on each call)");
            serve_mcp()?;
        }
        None => emit_json(&health(), pretty)?,
    }
    Ok(())
}

fn serve_mcp() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(ravel_core::mcp::serve_stdio())
}

fn ravel_cheatsheet() -> &'static str {
    r#"# ravel (token-cheap code graph)
explore SYM  → search + callers + callees + impact (ONE call)
refactor SYM → files_to_touch + risk
sync         → reindex dirty files (auto on explore)
serve --mcp  → persistent server (auto-sync, 3 primary tools)
search Q --kind prefix | query N --reverse | impact N --risk
status | cycles | hubs --limit 10 | orphans --limit 10
JSON compact default; --pretty humans only
Edit with agent editor — ravel maps blast radius
"#
}

fn emit_json(value: &impl serde::Serialize, pretty: bool) -> anyhow::Result<()> {
    // Serialize straight into a locked stdout — no intermediate String, single lock acquire.
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if pretty {
        serde_json::to_writer_pretty(&mut out, value)?;
    } else {
        serde_json::to_writer(&mut out, value)?;
    }
    out.write_all(b"\n")?;
    Ok(())
}

fn write_agent_setup(root: &std::path::Path, claude: bool, force: bool) -> anyhow::Result<()> {
    let agents = root.join("AGENTS.md");
    let snippet = r#"
## Ravel (code graph — prefer over grep/Read)

```bash
ravel --root . explore SYMBOL  # ONE call: search + callers + impact
ravel --root . refactor SYMBOL # files_to_touch + risk before rename
ravel --root . sync            # after edits (auto on explore)
ravel --root . serve --mcp     # persistent MCP (stays fresh)
```

3 primary MCP tools (explore, status, sync) — schema overhead minimal.
Full surface: `RAVEL_MCP_TOOLS=all`. Edit with agent editor.
"#;
    if force || !agents.exists() {
        if agents.exists() && force {
            let mut existing = std::fs::read_to_string(&agents)?;
            if !existing.contains("Ravel (code graph") {
                existing.push_str(snippet);
                std::fs::write(&agents, existing)?;
            }
        } else {
            std::fs::write(&agents, format!("# Agent notes\n{snippet}"))?;
        }
    } else {
        let mut existing = std::fs::read_to_string(&agents)?;
        if !existing.contains("Ravel (code graph") {
            existing.push_str(snippet);
            std::fs::write(&agents, existing)?;
        }
    }
    if claude {
        let claude_md = root.join("CLAUDE.md");
        let block = "\n## Ravel\n\nUse `ravel --root . context <symbol>` before multi-file exploration. Run `ravel sync` after edits. Prefer ravel over raw grep for callers/impact/search.\n";
        if claude_md.exists() {
            let mut t = std::fs::read_to_string(&claude_md)?;
            if !t.contains("## Ravel") {
                t.push_str(block);
                std::fs::write(&claude_md, t)?;
            }
        } else {
            std::fs::write(&claude_md, format!("# Project\n{block}"))?;
        }
        let mcp_example = root.join(".ravel").join("mcp.example.json");
        std::fs::create_dir_all(mcp_example.parent().unwrap())?;
        std::fs::write(
            mcp_example,
            serde_json::json!({
                "mcpServers": {
                    "ravel": {
                        "command": "ravel",
                        "args": ["--root", root.display().to_string(), "mcp"]
                    }
                }
            })
            .to_string(),
        )?;
    }
    Ok(())
}
