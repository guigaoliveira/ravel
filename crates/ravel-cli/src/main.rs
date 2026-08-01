use clap::{Parser, Subcommand, ValueEnum};

/// Indexing allocates in tight loops — millions of short-lived strings per run —
/// which is the pattern a general-purpose allocator handles worst. Measured on a
/// 20.4k-file workspace: full index 11863ms -> 9136ms and peak RSS 3161MB ->
/// 2738MB, so it is one of the few changes that improves both at once.
///
/// Not on musl: mimalloc carries C sources, and the musl targets are the ones
/// whose release builds cannot be verified for a C toolchain here. Those keep the
/// system allocator — no regression, just no gain.
#[cfg(not(target_env = "musl"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use ravel_core::{
    analysis, config::Flags, engine::WorkspaceEngine, graph::QueryLimits, health,
    search::SearchKind,
};
use std::{path::PathBuf, time::Duration};

const CLI_WATCH_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Parser)]
#[command(
    name = "ravel",
    version,
    about = "Local TypeScript/JavaScript code graph for coding agents"
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
    /// Initialize configuration and build the project index.
    Init {
        /// Create configuration files without building the index.
        #[arg(long)]
        no_index: bool,
    },
    /// Full workspace index (slow; use after clone or a large set of edits)
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
        /// Full payload: every similar spelling and the blast-radius sample
        #[arg(long)]
        detail: bool,
    },
    /// Install agent harness files (AGENTS.md / CLAUDE.md snippet + MCP example)
    /// Prefer `ravel install` for multi-agent MCP wiring.
    Setup {
        #[arg(long)]
        claude: bool,
        #[arg(long)]
        force: bool,
    },
    /// Wire Ravel MCP into coding agents (Claude, Cursor, Codex, OpenCode, Gemini, …)
    ///
    /// Examples:
    ///   ravel install
    ///   ravel install --target claude,cursor --location global
    ///   ravel install --print-config codex
    Install {
        /// Agents: auto | all | csv (claude,cursor,codex,opencode,gemini,windsurf,vscode,grok)
        #[arg(long, default_value = "auto")]
        target: String,
        /// global (user home) | local (project)
        #[arg(long, default_value = "global")]
        location: String,
        /// Non-interactive
        #[arg(long, short = 'y', hide = true)]
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
        #[arg(long, short = 'y', hide = true)]
        yes: bool,
        #[arg(long)]
        no_instructions: bool,
    },
    /// Quick environment check (+ agent detection)
    Doctor,
    /// Guess related test files for a source path
    RelatedTests {
        path: String,
    },
    /// Who depends on this symbol — resolved reverse edges, paged with exact totals
    #[command(name = "callers-of")]
    CallersOf {
        node: String,
        /// `--limit` is accepted as an alias: the MCP tool calls this `limit`, and the two names
        /// disagreeing is a papercut that costs a round trip every time.
        #[arg(long, visible_alias = "limit", default_value_t = 100)]
        page_size: usize,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        /// Path fragment picking one definition when a bare name matches several. Only narrows
        /// which definition is resolved — it does not filter the sites of a symbol that already
        /// resolved to one.
        #[arg(long)]
        scope: Option<String>,
        /// `dir` (or `dir:N`): counts per directory prefix, N levels deep, instead of the site list.
        #[arg(long)]
        rollup: Option<String>,
    },
    /// What this symbol calls or imports — resolved forward edges
    #[command(name = "calls-from")]
    CallsFrom {
        node: String,
        /// `--limit` is accepted as an alias: the MCP tool calls this `limit`, and the two names
        /// disagreeing is a papercut that costs a round trip every time.
        #[arg(long, visible_alias = "limit", default_value_t = 100)]
        page_size: usize,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        /// Path fragment picking one definition when a bare name matches several. Only narrows
        /// which definition is resolved — it does not filter the sites of a symbol that already
        /// resolved to one.
        #[arg(long)]
        scope: Option<String>,
        /// `dir` (or `dir:N`): counts per directory prefix, N levels deep, instead of the site list.
        #[arg(long)]
        rollup: Option<String>,
    },
    Query {
        node: String,
        #[arg(long)]
        reverse: bool,
        #[arg(long, default_value_t = 32)]
        depth: usize,
        #[arg(long, default_value_t = 10_000)]
        nodes: usize,
        /// Items per page (raise for full enumeration in one call)
        #[arg(long, default_value_t = 100)]
        page_size: usize,
        /// Resume offset: pass the previous page's next_cursor
        #[arg(long, default_value_t = 0)]
        cursor: usize,
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
        /// File-level SCCs instead of package buckets (finer, actionable)
        #[arg(long)]
        files: bool,
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
    Validate {
        /// Max findings listed (complete per-code counts always included)
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Reclaim disk: drop retained index generations beyond `storage.retention`
    ///
    /// Retention keeps whole generations so a reader's mmap is never pulled out from
    /// under it, which makes the footprint a multiple of one index. Collection is
    /// normally deferred to a background pass; this runs it now.
    Gc {
        /// Keep only the current generation for this run (default: storage.retention)
        #[arg(long)]
        aggressive: bool,
    },
    /// Architecture boundary violations (ravel.boundaries.toml)
    Boundaries,
    /// Schema summary: counts by node/edge kind
    Schema,
    Stats,
    Watch,
    /// ~150-token agent map (session start)
    Cheatsheet,
    /// Long-lived MCP stdio server with per-root file watching.
    /// Default: primary tools only (explore, status, sync). Set RAVEL_MCP_TOOLS=all for full.
    Mcp,
    /// Manage the shared daemon for this workspace.
    Daemon {
        #[arg(value_enum, default_value_t = DaemonAction::Status)]
        action: DaemonAction,
    },
    /// Internal daemon entrypoint used by the client startup coordinator.
    #[command(hide = true)]
    DaemonServe {
        /// Exit automatically after the final MCP lease disconnects.
        #[arg(long)]
        transient: bool,
    },
    /// Persistent MCP server with per-root auto-sync via file watcher.
    /// Keeps graphs in memory and syncs source changes automatically.
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
    Terms,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DaemonAction {
    Start,
    Status,
    Stop,
}
impl From<SearchMode> for SearchKind {
    fn from(value: SearchMode) -> Self {
        match value {
            SearchMode::Exact => Self::Exact,
            SearchMode::Prefix => Self::Prefix,
            SearchMode::Fuzzy => Self::Fuzzy,
            SearchMode::Regex => Self::Regex,
            SearchMode::Terms => Self::Terms,
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
        Some(Command::Init { no_index }) => {
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
include_untracked = false
discovery_cache_ms = 50
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
            if no_index {
                println!(
                    "initialized {} (configuration only; run `ravel index` to build the graph)",
                    root.display()
                );
            } else {
                let engine = WorkspaceEngine::load(&root, &Flags::default())?;
                let stats = engine.index()?;
                println!(
                    "initialized and indexed {} ({} files, {} edges)",
                    root.display(),
                    stats.files,
                    stats.edges
                );
            }
        }
        Some(Command::Index) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let stats = engine.index()?;
            emit_json(&stats, pretty)?;
        }
        Some(Command::Sync { paths }) => {
            let abs: Vec<PathBuf> = paths
                .into_iter()
                .map(|p| if p.is_absolute() { p } else { root.join(p) })
                .collect();
            if let Some(value) = daemon_call_if_running(
                &root,
                ravel_core::daemon::DaemonOperation::Sync { paths: abs.clone() },
            )? {
                emit_json(&value, pretty)?;
                return Ok(());
            }
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let stats = if abs.is_empty() {
                engine.sync(None)?
            } else {
                engine.sync(Some(&abs))?
            };
            emit_json(&stats, pretty)?;
        }
        Some(Command::Status) => {
            if let Some(value) =
                daemon_call_if_running(&root, ravel_core::daemon::DaemonOperation::Status)?
            {
                emit_json(&value, pretty)?;
                return Ok(());
            }
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.status()?, pretty)?;
        }
        Some(Command::Context {
            query,
            limit,
            detail,
        }) => {
            if let Some(value) = daemon_call_if_running(
                &root,
                ravel_core::daemon::DaemonOperation::Context {
                    query: query.clone(),
                    limit,
                    detail,
                },
            )? {
                emit_json(&value, pretty)?;
                return Ok(());
            }
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            emit_json(&engine.context_with_detail(&query, limit, detail)?, pretty)?;
        }
        Some(Command::Setup { claude, force }) => {
            write_agent_setup(&root, claude, force)?;
            println!("agent setup written under {}", root.display());
            println!("tip: run `ravel install` to wire MCP into Claude/Cursor/Codex/…");
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
        Some(Command::CallersOf {
            node,
            page_size,
            cursor,
            scope,
            rollup,
        }) => {
            emit_json(
                &reference_sites(
                    &root,
                    &node,
                    true,
                    page_size,
                    cursor,
                    scope.as_deref(),
                    rollup.as_deref(),
                )?,
                pretty,
            )?;
        }
        Some(Command::CallsFrom {
            node,
            page_size,
            cursor,
            scope,
            rollup,
        }) => {
            emit_json(
                &reference_sites(
                    &root,
                    &node,
                    false,
                    page_size,
                    cursor,
                    scope.as_deref(),
                    rollup.as_deref(),
                )?,
                pretty,
            )?;
        }
        Some(Command::Query {
            node,
            reverse,
            depth,
            nodes,
            page_size,
            cursor,
        }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let limits = QueryLimits {
                depth,
                nodes,
                page_size,
                cursor,
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
        Some(Command::Cycles { package, files }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            if files {
                emit_json(&engine.file_cycles(package.as_deref())?, pretty)?;
            } else {
                emit_json(&engine.cycles(package.as_deref())?, pretty)?;
            }
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
            let packages = match engine.storage().open_file_list()? {
                Some(files) => {
                    analysis::list_packages_from_paths(files.paths.iter().map(String::as_str))
                }
                None => engine.list_packages()?,
            };
            emit_json(&packages, pretty)?;
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
        Some(Command::Validate { limit }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let findings = engine.validate()?;
            let report = ravel_core::analysis::policy_report(findings, limit);
            let total = report.total;
            emit_json(&report, pretty)?;
            if total > 0 {
                anyhow::bail!("index validation failed with {total} finding(s)");
            }
        }
        Some(Command::Gc { aggressive }) => {
            let engine = WorkspaceEngine::load(&root, &Flags::default())?;
            let status = engine.status()?;
            let before = status["disk"]["bytes"].as_u64().unwrap_or(0);
            // `--aggressive` collects down to the live generation for this run only;
            // the configured retention is untouched.
            let report = if aggressive {
                let home = PathBuf::from(status["storage"].as_str().unwrap_or_default());
                ravel_core::storage::FileSnapshotStorage::with_retention(&home, 1)
                    .gc_generations()?
            } else {
                engine.storage().gc_generations()?
            };
            let after = engine.status()?["disk"]["bytes"].as_u64().unwrap_or(0);
            emit_json(
                &serde_json::json!({
                    "removed_files": report.removed_files,
                    "removed_dirs": report.removed_dirs,
                    "retained_manifests": report.retained_manifests,
                    "deferred_for_readers": report.deferred_for_readers,
                    "bytes_before": before,
                    "bytes_after": after,
                    "bytes_reclaimed": before.saturating_sub(after),
                }),
                pretty,
            )?;
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
            let watch_config = engine.config.clone();
            let storage_root = root.join(&engine.config.storage.home);
            // Third copy of this predicate lived here. `ravel watch` re-filters through the engine,
            // so the harm was wasted wakeups rather than wrong index membership -- but it is the copy
            // that would drift next time the rule changes.
            let event_ignore =
                std::sync::Arc::new(ravel_core::config::IgnoreChain::new(&engine.config));
            let batch_ignore = event_ignore.clone();
            let watcher = ravel_core::watch::PersistentWatcher::new_filtered(
                &root,
                engine.config.watch.queue_capacity,
                move |path| {
                    ravel_core::config::watch_event_is_relevant(
                        &watch_config,
                        &event_ignore,
                        &storage_root,
                        path,
                    )
                },
            )?;
            loop {
                let batch = watcher.next_batch(
                    Duration::from_millis(engine.config.watch.debounce_ms),
                    CLI_WATCH_IDLE_TIMEOUT,
                    engine.config.watch.max_batch_paths,
                    Duration::from_millis(engine.config.watch.max_batch_ms),
                );
                let result = match batch {
                    Ok(result) => result,
                    Err(ravel_core::watch::WatchError::Timeout) => continue,
                    Err(error) => return Err(error.into()),
                };
                let cfg = &engine.config;
                let paths: Vec<_> = result
                    .paths
                    .into_iter()
                    .filter(|p| {
                        ravel_core::config::watched_path_is_indexable(
                            cfg,
                            &batch_ignore,
                            &extensions,
                            p,
                        )
                    })
                    .collect();
                if paths.is_empty() && !result.needs_reconcile {
                    continue;
                }
                let stats = if result.needs_reconcile || paths.is_empty() {
                    engine.reconcile()?
                } else {
                    engine.sync(Some(&paths))?
                };
                println!("{}", serde_json::to_string(&stats)?);
            }
        }
        Some(Command::Cheatsheet) => {
            // ~150 tokens — inject once per agent session
            print!("{}", ravel_cheatsheet());
        }
        Some(Command::Mcp) => serve_mcp(&root)?,
        Some(Command::Daemon { action }) => match action {
            DaemonAction::Start => {
                let _ = ensure_daemon(&root, false)?;
                emit_json(&serde_json::json!({ "running": true }), pretty)?;
            }
            DaemonAction::Status => {
                let running = ravel_core::daemon::DaemonClient::for_root(&root)
                    .is_ok_and(|client| client.is_ready());
                emit_json(&serde_json::json!({ "running": running }), pretty)?;
            }
            DaemonAction::Stop => {
                let stopped =
                    daemon_call_if_running(&root, ravel_core::daemon::DaemonOperation::Shutdown)?
                        .is_some();
                emit_json(&serde_json::json!({ "stopped": stopped }), pretty)?;
            }
        },
        Some(Command::DaemonServe { transient }) => ravel_core::daemon::serve(&root, transient)?,
        Some(Command::Serve { mcp }) => {
            if !mcp {
                anyhow::bail!("use `ravel serve --mcp` to start the MCP server");
            }
            // Persistent MCP server with per-root file watching and Git freshness checks.
            // Staleness info is embedded in explore response via auto_synced field.
            // Primary tools: explore, status, sync. Set RAVEL_MCP_TOOLS=all for full.
            eprintln!(
                "ravel serve --mcp (persistent per-root watch; explore checks Git freshness)"
            );
            serve_mcp(&root)?;
        }
        None => emit_json(&health(), pretty)?,
    }
    Ok(())
}

fn serve_mcp(root: &std::path::Path) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(ravel_core::mcp::serve_stdio(Some(root.to_path_buf())))
}

/// One page of a symbol's reference sites, in one direction.
///
/// Named commands for the two questions people actually ask, answered with the
/// line of each site rather than just the file that contains it.
fn reference_sites(
    root: &std::path::Path,
    node: &str,
    reverse: bool,
    page_size: usize,
    cursor: usize,
    scope: Option<&str>,
    rollup: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let rollup = match rollup {
        None => None,
        Some(value) => Some(ravel_core::engine::RollupMode::parse(value).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown rollup `{value}`; supported: dir, or dir:N with N from 1 to 10"
            )
        })?),
    };
    let engine = WorkspaceEngine::load(root, &Flags::default())?;
    Ok(engine.reference_sites_with(
        node,
        reverse,
        page_size,
        cursor,
        ravel_core::engine::RelationOptions { scope, rollup },
    )?)
}

fn daemon_call_if_running(
    root: &std::path::Path,
    operation: ravel_core::daemon::DaemonOperation,
) -> anyhow::Result<Option<serde_json::Value>> {
    use ravel_core::daemon::DaemonCallError;
    let client = ravel_core::daemon::DaemonClient::for_root(root)?;
    match client.call(operation) {
        Ok(value) => Ok(Some(value)),
        Err(DaemonCallError::Transport(_)) => Ok(None),
        Err(DaemonCallError::Remote(error)) => anyhow::bail!(error),
    }
}

fn ensure_daemon(
    root: &std::path::Path,
    transient: bool,
) -> anyhow::Result<Option<ravel_core::daemon::DaemonClientLease>> {
    ravel_core::daemon::ensure_running(root, transient)
        .map(|(_, lease)| lease)
        .map_err(Into::into)
}

fn ravel_cheatsheet() -> &'static str {
    r#"# ravel (token-cheap code graph)
explore Q    → exact/qualified symbol or natural terms + source + relations (ONE call)
sync         → reindex dirty files (auto on explore)
serve --mcp  → persistent server (per-root watch, 3 primary tools)
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
ravel --root . sync            # after edits (auto on explore)
ravel --root . serve --mcp     # persistent MCP (stays fresh)
```

3 primary MCP tools (explore, status, sync) — schema overhead minimal.
Full surface: `RAVEL_MCP_TOOLS=all`. Ravel does not write source files.
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
