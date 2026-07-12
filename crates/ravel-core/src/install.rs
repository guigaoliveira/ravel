//! Cross-agent install: detect coding agents and wire Ravel MCP + instruction snippets.
//! Agent setup UX: auto-detection, global/local scope, and printable config.

use serde::Serialize;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const MCP_SERVER_NAME: &str = "ravel";
pub const MARKER_BEGIN: &str = "<!-- ravel-agent-begin -->";
pub const MARKER_END: &str = "<!-- ravel-agent-end -->";

/// Supported agent harnesses (MCP + instruction files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Cursor,
    Codex,
    OpenCode,
    Gemini,
    Windsurf,
    VsCode,
    Grok,
}

impl AgentKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Windsurf => "windsurf",
            Self::VsCode => "vscode",
            Self::Grok => "grok",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Codex => "Codex CLI",
            Self::OpenCode => "OpenCode",
            Self::Gemini => "Gemini CLI",
            Self::Windsurf => "Windsurf",
            Self::VsCode => "VS Code / Copilot",
            Self::Grok => "Grok",
        }
    }

    pub fn all() -> &'static [AgentKind] {
        &[
            Self::Claude,
            Self::Cursor,
            Self::Codex,
            Self::OpenCode,
            Self::Gemini,
            Self::Windsurf,
            Self::VsCode,
            Self::Grok,
        ]
    }

    pub fn parse_csv(s: &str) -> Result<Vec<AgentKind>, String> {
        let mut out = Vec::new();
        for part in s.split(',') {
            let p = part.trim().to_ascii_lowercase();
            if p.is_empty() {
                continue;
            }
            let kind = match p.as_str() {
                "auto" => {
                    return Ok(detect_agents());
                }
                "all" => {
                    return Ok(Self::all().to_vec());
                }
                "claude" | "claude-code" => Self::Claude,
                "cursor" => Self::Cursor,
                "codex" => Self::Codex,
                "opencode" | "open-code" => Self::OpenCode,
                "gemini" | "gemini-cli" => Self::Gemini,
                "windsurf" | "cascade" => Self::Windsurf,
                "vscode" | "code" | "copilot" => Self::VsCode,
                "grok" | "grok-cli" => Self::Grok,
                other => return Err(format!("unknown agent target: {other}")),
            };
            if !out.contains(&kind) {
                out.push(kind);
            }
        }
        if out.is_empty() {
            return Err("no agents specified".into());
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallLocation {
    Global,
    Local,
}

impl InstallLocation {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "global" | "user" | "home" => Ok(Self::Global),
            "local" | "project" => Ok(Self::Local),
            other => Err(format!("unknown location: {other} (use global|local)")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub targets: Vec<AgentKind>,
    pub location: InstallLocation,
    /// Project root for local configs and instruction files.
    pub project_root: PathBuf,
    /// Absolute path to the ravel binary (preferred over bare `ravel` on PATH).
    pub ravel_bin: PathBuf,
    /// Also write AGENTS.md / CLAUDE.md / GEMINI.md snippets.
    pub write_instructions: bool,
    /// Claude Code: add mcp__ravel__* to allow list when possible.
    pub claude_permissions: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallAction {
    pub agent: String,
    pub path: String,
    pub action: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub ravel_bin: String,
    pub location: String,
    pub actions: Vec<InstallAction>,
    pub detected: Vec<String>,
    pub next_steps: Vec<String>,
}

/// Resolve this process's binary path for MCP command (stable across shells).
pub fn resolve_ravel_bin() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("ravel"))
}

/// Detect which agents look installed (binary on PATH and/or config dir present).
pub fn detect_agents() -> Vec<AgentKind> {
    let mut found = Vec::new();
    for kind in AgentKind::all() {
        if agent_looks_installed(*kind) {
            found.push(*kind);
        }
    }
    if found.is_empty() {
        // Fallback: still offer Claude + Cursor + Codex as common trio
        found.extend([AgentKind::Claude, AgentKind::Cursor, AgentKind::Codex]);
    }
    found
}

fn agent_looks_installed(kind: AgentKind) -> bool {
    let home = home_dir();
    match kind {
        AgentKind::Claude => {
            which_ok("claude")
                || home.join(".claude.json").exists()
                || home.join(".claude").is_dir()
        }
        AgentKind::Cursor => {
            which_ok("cursor")
                || home.join(".cursor").is_dir()
                || home.join(".cursor").join("mcp.json").exists()
        }
        AgentKind::Codex => which_ok("codex") || home.join(".codex").is_dir(),
        AgentKind::OpenCode => {
            which_ok("opencode")
                || home.join(".config").join("opencode").is_dir()
                || home.join(".opencode").is_dir()
        }
        AgentKind::Gemini => which_ok("gemini") || home.join(".gemini").is_dir(),
        AgentKind::Windsurf => home.join(".codeium").join("windsurf").is_dir(),
        AgentKind::VsCode => {
            which_ok("code")
                || home.join(".vscode").is_dir()
                || dirs_config().join("Code").join("User").is_dir()
        }
        AgentKind::Grok => which_ok("grok") || home.join(".grok").is_dir(),
    }
}

fn which_ok(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        || Command::new("where")
            .arg(bin)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn dirs_config() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir().join("Library").join("Application Support")
    } else if cfg!(windows) {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"))
    }
}

/// MCP entry shared by Claude / Cursor / Gemini / Windsurf / VS Code style JSON.
/// Uses `serve --mcp` for a persistent server with auto-sync.
pub fn mcp_stdio_entry(ravel_bin: &Path) -> Value {
    json!({
        "type": "stdio",
        "command": ravel_bin.to_string_lossy(),
        "args": ["serve", "--mcp"]
    })
}

/// Human-readable config snippet for one agent (no file writes).
pub fn print_config(kind: AgentKind, ravel_bin: &Path, location: InstallLocation) -> String {
    let cmd = ravel_bin.display();
    match kind {
        AgentKind::Claude => match location {
            InstallLocation::Global => format!(
                r#"# ~/.claude.json  (mcpServers key)
{{
  "mcpServers": {{
    "ravel": {{
      "type": "stdio",
      "command": "{cmd}",
      "args": ["serve", "--mcp"]
    }}
  }}
}}
"#
            ),
            InstallLocation::Local => format!(
                r#"# .mcp.json (project root)
{{
  "mcpServers": {{
    "ravel": {{
      "type": "stdio",
      "command": "{cmd}",
      "args": ["serve", "--mcp"]
    }}
  }}
}}
"#
            ),
        },
        AgentKind::Cursor => format!(
            r#"# {} 
{{
  "mcpServers": {{
    "ravel": {{
      "command": "{cmd}",
      "args": ["serve", "--mcp"]
    }}
  }}
}}
"#,
            if location == InstallLocation::Global {
                "~/.cursor/mcp.json"
            } else {
                ".cursor/mcp.json"
            }
        ),
        AgentKind::Codex => format!(
            r#"# {}
[mcp_servers.ravel]
command = "{cmd}"
args = ["serve", "--mcp"]
"#,
            if location == InstallLocation::Global {
                "~/.codex/config.toml"
            } else {
                ".codex/config.toml"
            }
        ),
        AgentKind::OpenCode => format!(
            r#"# {}
{{
  "mcp": {{
    "ravel": {{
      "type": "local",
      "command": ["{cmd}", "serve", "--mcp"],
      "enabled": true
    }}
  }}
}}
"#,
            if location == InstallLocation::Global {
                "~/.config/opencode/opencode.json"
            } else {
                "opencode.json"
            }
        ),
        AgentKind::Gemini => format!(
            r#"# {}
{{
  "mcpServers": {{
    "ravel": {{
      "command": "{cmd}",
      "args": ["serve", "--mcp"]
    }}
  }}
}}
"#,
            if location == InstallLocation::Global {
                "~/.gemini/settings.json"
            } else {
                ".gemini/settings.json"
            }
        ),
        AgentKind::Windsurf => format!(
            r#"# ~/.codeium/windsurf/mcp_config.json
{{
  "mcpServers": {{
    "ravel": {{
      "command": "{cmd}",
      "args": ["serve", "--mcp"]
    }}
  }}
}}
"#
        ),
        AgentKind::VsCode => format!(
            r#"# .vscode/mcp.json (project) or VS Code user mcp.json
{{
  "servers": {{
    "ravel": {{
      "type": "stdio",
      "command": "{cmd}",
      "args": ["serve", "--mcp"]
    }}
  }}
}}
"#
        ),
        AgentKind::Grok => format!(
            r#"# Grok: prefer CLI (no stable MCP path yet). Put in AGENTS.md / session:
#   {cmd} status
#   {cmd} context SYMBOL
"#
        ),
    }
}

fn agent_instruction_block() -> String {
    format!(
        r#"{MARKER_BEGIN}
## Ravel (code graph — prefer over grep/Read for structural queries)

Local TS/JS graph. Index once; query cheaply; sync after edits.

```bash
ravel status                   # session start
ravel explore SYMBOL           # ONE call → search + callers + callees + impact
ravel sync                     # after edits (auto on explore too)
ravel serve --mcp              # persistent MCP (stays fresh, auto-sync)
```

3 primary MCP tools (`explore`, `status`, `sync`) — low schema overhead.
Set `RAVEL_MCP_TOOLS=all` for full surface (search, impact, cycles, hubs, …).
Edit with the agent's editor — ravel does not write source files.
{MARKER_END}
"#
    )
}

/// Install Ravel into selected agents. Never clobbers unrelated MCP servers.
pub fn install_agents(opts: &InstallOptions) -> anyhow::Result<InstallReport> {
    let mut actions = Vec::new();
    let detected = detect_agents()
        .into_iter()
        .map(|a| a.id().to_string())
        .collect();

    for kind in &opts.targets {
        match install_one(*kind, opts, &mut actions) {
            Ok(()) => {}
            Err(e) => {
                actions.push(InstallAction {
                    agent: kind.id().into(),
                    path: String::new(),
                    action: "error".into(),
                    detail: e.to_string(),
                });
            }
        }
    }

    if opts.write_instructions {
        write_project_instructions(&opts.project_root, &mut actions)?;
    }

    let next_steps = vec![
        "Restart your agent(s) so MCP reloads.".into(),
        format!(
            "In each project: cd <repo> && {} index",
            opts.ravel_bin.display()
        ),
        format!(
            "Smoke test: {} --root <repo> context <Symbol>",
            opts.ravel_bin.display()
        ),
    ];

    Ok(InstallReport {
        ravel_bin: opts.ravel_bin.display().to_string(),
        location: match opts.location {
            InstallLocation::Global => "global".into(),
            InstallLocation::Local => "local".into(),
        },
        actions,
        detected,
        next_steps,
    })
}

/// Remove Ravel MCP entries + instruction markers from selected agents.
pub fn uninstall_agents(opts: &InstallOptions) -> anyhow::Result<InstallReport> {
    let mut actions = Vec::new();
    let detected = detect_agents()
        .into_iter()
        .map(|a| a.id().to_string())
        .collect();

    for kind in &opts.targets {
        match uninstall_one(*kind, opts, &mut actions) {
            Ok(()) => {}
            Err(e) => {
                actions.push(InstallAction {
                    agent: kind.id().into(),
                    path: String::new(),
                    action: "error".into(),
                    detail: e.to_string(),
                });
            }
        }
    }

    if opts.write_instructions {
        strip_project_instructions(&opts.project_root, &mut actions)?;
    }

    Ok(InstallReport {
        ravel_bin: opts.ravel_bin.display().to_string(),
        location: match opts.location {
            InstallLocation::Global => "global".into(),
            InstallLocation::Local => "local".into(),
        },
        actions,
        detected,
        next_steps: vec![
            "Restart agents to drop MCP server.".into(),
            "Project indexes (.ravel/) left intact; delete manually if desired.".into(),
        ],
    })
}

fn install_one(
    kind: AgentKind,
    opts: &InstallOptions,
    actions: &mut Vec<InstallAction>,
) -> anyhow::Result<()> {
    match kind {
        AgentKind::Claude => install_claude(opts, actions),
        AgentKind::Cursor => {
            install_json_mcp_servers(kind, &cursor_mcp_path(opts), opts, actions, "mcpServers")
        }
        AgentKind::Codex => install_codex(opts, actions),
        AgentKind::OpenCode => install_opencode(opts, actions),
        AgentKind::Gemini => install_json_mcp_servers(
            kind,
            &gemini_settings_path(opts),
            opts,
            actions,
            "mcpServers",
        ),
        AgentKind::Windsurf => {
            install_json_mcp_servers(kind, &windsurf_mcp_path(), opts, actions, "mcpServers")
        }
        AgentKind::VsCode => install_vscode(opts, actions),
        AgentKind::Grok => {
            // No stable MCP path — instructions only (handled in write_project_instructions).
            actions.push(InstallAction {
                agent: kind.id().into(),
                path: opts.project_root.join("AGENTS.md").display().to_string(),
                action: "instructions".into(),
                detail: "Grok uses CLI via AGENTS.md (no global MCP file)".into(),
            });
            Ok(())
        }
    }
}

fn uninstall_one(
    kind: AgentKind,
    opts: &InstallOptions,
    actions: &mut Vec<InstallAction>,
) -> anyhow::Result<()> {
    match kind {
        AgentKind::Claude => uninstall_claude(opts, actions),
        AgentKind::Cursor => {
            remove_json_mcp_key(kind, &cursor_mcp_path(opts), actions, "mcpServers")
        }
        AgentKind::Codex => uninstall_codex(opts, actions),
        AgentKind::OpenCode => uninstall_opencode(opts, actions),
        AgentKind::Gemini => {
            remove_json_mcp_key(kind, &gemini_settings_path(opts), actions, "mcpServers")
        }
        AgentKind::Windsurf => {
            remove_json_mcp_key(kind, &windsurf_mcp_path(), actions, "mcpServers")
        }
        AgentKind::VsCode => uninstall_vscode(opts, actions),
        AgentKind::Grok => {
            actions.push(InstallAction {
                agent: kind.id().into(),
                path: String::new(),
                action: "skip".into(),
                detail: "no MCP entry; strip AGENTS.md markers if present".into(),
            });
            Ok(())
        }
    }
}

fn cursor_mcp_path(opts: &InstallOptions) -> PathBuf {
    match opts.location {
        InstallLocation::Global => home_dir().join(".cursor").join("mcp.json"),
        InstallLocation::Local => opts.project_root.join(".cursor").join("mcp.json"),
    }
}

fn gemini_settings_path(opts: &InstallOptions) -> PathBuf {
    match opts.location {
        InstallLocation::Global => home_dir().join(".gemini").join("settings.json"),
        InstallLocation::Local => opts.project_root.join(".gemini").join("settings.json"),
    }
}

fn windsurf_mcp_path() -> PathBuf {
    home_dir()
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json")
}

fn claude_global_path() -> PathBuf {
    home_dir().join(".claude.json")
}

fn claude_local_path(opts: &InstallOptions) -> PathBuf {
    opts.project_root.join(".mcp.json")
}

fn install_claude(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Global => claude_global_path(),
        InstallLocation::Local => claude_local_path(opts),
    };
    upsert_json_mcp_servers(&path, &opts.ravel_bin, true)?;
    actions.push(InstallAction {
        agent: "claude".into(),
        path: path.display().to_string(),
        action: "wrote_mcp".into(),
        detail: "mcpServers.ravel".into(),
    });

    if opts.claude_permissions && opts.location == InstallLocation::Global {
        let settings = home_dir().join(".claude").join("settings.json");
        if let Err(e) = ensure_claude_allowlist(&settings) {
            actions.push(InstallAction {
                agent: "claude".into(),
                path: settings.display().to_string(),
                action: "warn".into(),
                detail: format!("permissions allowlist: {e}"),
            });
        } else if settings.exists() {
            actions.push(InstallAction {
                agent: "claude".into(),
                path: settings.display().to_string(),
                action: "permissions".into(),
                detail: "mcp__ravel__* allow".into(),
            });
        }
    }
    Ok(())
}

fn uninstall_claude(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Global => claude_global_path(),
        InstallLocation::Local => claude_local_path(opts),
    };
    remove_json_mcp_key(AgentKind::Claude, &path, actions, "mcpServers")
}

fn ensure_claude_allowlist(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        // Don't create settings.json from scratch (user may not want defaults).
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let mut root: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
    // Guard every downcast: a settings.json that parses to a non-object (`[]`, scalar), or
    // whose `permissions`/`allow` are the wrong JSON type, must not panic — coerce instead.
    if !root.is_object() {
        root = json!({});
    }
    let permissions = root
        .as_object_mut()
        .unwrap()
        .entry("permissions")
        .or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }
    let allow = permissions
        .as_object_mut()
        .unwrap()
        .entry("allow")
        .or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let arr = allow.as_array_mut().unwrap();
    let entry = "mcp__ravel__*";
    if !arr.iter().any(|v| v.as_str() == Some(entry)) {
        arr.push(json!(entry));
        write_json_pretty(path, &root)?;
    }
    Ok(())
}

fn install_json_mcp_servers(
    kind: AgentKind,
    path: &Path,
    opts: &InstallOptions,
    actions: &mut Vec<InstallAction>,
    key: &str,
) -> anyhow::Result<()> {
    upsert_json_mcp_servers(path, &opts.ravel_bin, key == "mcpServers")?;
    actions.push(InstallAction {
        agent: kind.id().into(),
        path: path.display().to_string(),
        action: "wrote_mcp".into(),
        detail: format!("{key}.ravel"),
    });
    Ok(())
}

fn upsert_json_mcp_servers(
    path: &Path,
    ravel_bin: &Path,
    _mcp_servers_key: bool,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root: Value = if path.exists() {
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    let servers = root
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_NAME.into(), mcp_stdio_entry(ravel_bin));
    write_json_pretty(path, &root)?;
    Ok(())
}

fn remove_json_mcp_key(
    kind: AgentKind,
    path: &Path,
    actions: &mut Vec<InstallAction>,
    key: &str,
) -> anyhow::Result<()> {
    if !path.exists() {
        actions.push(InstallAction {
            agent: kind.id().into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "config missing".into(),
        });
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let mut root: Value = serde_json::from_str(&text)?;
    let mut removed = false;
    if let Some(obj) = root.as_object_mut() {
        if let Some(servers) = obj.get_mut(key).and_then(|v| v.as_object_mut()) {
            removed = servers.remove(MCP_SERVER_NAME).is_some();
        }
    }
    if removed {
        write_json_pretty(path, &root)?;
        actions.push(InstallAction {
            agent: kind.id().into(),
            path: path.display().to_string(),
            action: "removed_mcp".into(),
            detail: format!("{key}.ravel"),
        });
    } else {
        actions.push(InstallAction {
            agent: kind.id().into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "ravel not present".into(),
        });
    }
    Ok(())
}

fn install_codex(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Global => home_dir().join(".codex").join("config.toml"),
        InstallLocation::Local => opts.project_root.join(".codex").join("config.toml"),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let block = format!(
        "\n[mcp_servers.ravel]\ncommand = \"{}\"\nargs = [\"serve\", \"--mcp\"]\n",
        opts.ravel_bin.display()
    );
    if text.contains("[mcp_servers.ravel]") {
        // Replace existing block (simple line-based strip until next [section)
        text = replace_toml_table(&text, "mcp_servers.ravel", &block);
    } else {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(block.trim_start());
    }
    fs::write(&path, text)?;
    actions.push(InstallAction {
        agent: "codex".into(),
        path: path.display().to_string(),
        action: "wrote_mcp".into(),
        detail: "[mcp_servers.ravel]".into(),
    });
    Ok(())
}

fn uninstall_codex(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Global => home_dir().join(".codex").join("config.toml"),
        InstallLocation::Local => opts.project_root.join(".codex").join("config.toml"),
    };
    if !path.exists() {
        actions.push(InstallAction {
            agent: "codex".into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "config missing".into(),
        });
        return Ok(());
    }
    let text = fs::read_to_string(&path)?;
    if !text.contains("[mcp_servers.ravel]") {
        actions.push(InstallAction {
            agent: "codex".into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "ravel not present".into(),
        });
        return Ok(());
    }
    let new_text = replace_toml_table(&text, "mcp_servers.ravel", "");
    fs::write(&path, new_text)?;
    actions.push(InstallAction {
        agent: "codex".into(),
        path: path.display().to_string(),
        action: "removed_mcp".into(),
        detail: "[mcp_servers.ravel]".into(),
    });
    Ok(())
}

/// Replace or remove a TOML table `[name]` including nested keys until next top-level `[`.
fn replace_toml_table(text: &str, table: &str, replacement: &str) -> String {
    let header = format!("[{table}]");
    let Some(start) = text.find(&header) else {
        return format!(
            "{text}{}",
            if replacement.is_empty() {
                ""
            } else {
                replacement
            }
        );
    };
    // Find previous newline start of header line
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let after = &text[start + header.len()..];
    let mut end = text.len();
    for (i, line) in after.split_inclusive('\n').scan(0usize, |acc, l| {
        let at = *acc;
        *acc += l.len();
        Some((at, l))
    }) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            // only if it's a new table header at column 0-ish of original...
            // after is relative; check absolute
            let abs = start + header.len() + i;
            if text[abs..].starts_with('[') {
                // nested tables look like [mcp_servers.ravel.tools.x]
                let rest = &text[abs + 1..];
                let name_end = rest.find(']').unwrap_or(0);
                let name = &rest[..name_end];
                if !name.starts_with(&format!("{table}.")) && name != table {
                    end = abs;
                    break;
                }
            }
        }
        let _ = line;
    }
    let mut out = String::new();
    out.push_str(&text[..line_start]);
    if !replacement.is_empty() {
        out.push_str(replacement.trim_start_matches('\n'));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&text[end..]);
    out
}

fn install_opencode(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Global => dirs_config().join("opencode").join("opencode.json"),
        InstallLocation::Local => opts.project_root.join("opencode.json"),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root: Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?)?
    } else {
        json!({ "$schema": "https://opencode.ai/config.json" })
    };
    if !root.is_object() {
        root = json!({});
    }
    let mcp = root
        .as_object_mut()
        .unwrap()
        .entry("mcp")
        .or_insert_with(|| json!({}));
    if !mcp.is_object() {
        *mcp = json!({});
    }
    mcp.as_object_mut().unwrap().insert(
        MCP_SERVER_NAME.into(),
        json!({
            "type": "local",
            "command": [opts.ravel_bin.to_string_lossy(), "serve", "--mcp"],
            "enabled": true
        }),
    );
    write_json_pretty(&path, &root)?;
    actions.push(InstallAction {
        agent: "opencode".into(),
        path: path.display().to_string(),
        action: "wrote_mcp".into(),
        detail: "mcp.ravel".into(),
    });
    Ok(())
}

fn uninstall_opencode(
    opts: &InstallOptions,
    actions: &mut Vec<InstallAction>,
) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Global => dirs_config().join("opencode").join("opencode.json"),
        InstallLocation::Local => opts.project_root.join("opencode.json"),
    };
    if !path.exists() {
        actions.push(InstallAction {
            agent: "opencode".into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "config missing".into(),
        });
        return Ok(());
    }
    let mut root: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut removed = false;
    if let Some(mcp) = root.get_mut("mcp").and_then(|v| v.as_object_mut()) {
        removed = mcp.remove(MCP_SERVER_NAME).is_some();
    }
    if removed {
        write_json_pretty(&path, &root)?;
        actions.push(InstallAction {
            agent: "opencode".into(),
            path: path.display().to_string(),
            action: "removed_mcp".into(),
            detail: "mcp.ravel".into(),
        });
    } else {
        actions.push(InstallAction {
            agent: "opencode".into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "ravel not present".into(),
        });
    }
    Ok(())
}

fn install_vscode(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    // Prefer project-local .vscode/mcp.json (works in VS Code Copilot).
    let path = match opts.location {
        InstallLocation::Local => opts.project_root.join(".vscode").join("mcp.json"),
        InstallLocation::Global => {
            // User-level path varies; still write project if root is given, else user Code config.
            if cfg!(target_os = "macos") {
                home_dir()
                    .join("Library")
                    .join("Application Support")
                    .join("Code")
                    .join("User")
                    .join("mcp.json")
            } else {
                // Linux + Windows user MCP path via config dir.
                dirs_config().join("Code").join("User").join("mcp.json")
            }
        }
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root: Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?)?
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    // VS Code 1.99+ uses "servers"; some forks still use mcpServers.
    let key = if root.get("servers").is_some() {
        "servers"
    } else if root.get("mcpServers").is_some() {
        "mcpServers"
    } else {
        "servers"
    };
    let servers = root
        .as_object_mut()
        .unwrap()
        .entry(key)
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert(MCP_SERVER_NAME.into(), mcp_stdio_entry(&opts.ravel_bin));
    write_json_pretty(&path, &root)?;
    actions.push(InstallAction {
        agent: "vscode".into(),
        path: path.display().to_string(),
        action: "wrote_mcp".into(),
        detail: format!("{key}.ravel"),
    });
    Ok(())
}

fn uninstall_vscode(opts: &InstallOptions, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let path = match opts.location {
        InstallLocation::Local => opts.project_root.join(".vscode").join("mcp.json"),
        InstallLocation::Global => dirs_config().join("Code").join("User").join("mcp.json"),
    };
    if !path.exists() {
        actions.push(InstallAction {
            agent: "vscode".into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "config missing".into(),
        });
        return Ok(());
    }
    let mut root: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut removed = false;
    for key in ["servers", "mcpServers"] {
        if let Some(servers) = root.get_mut(key).and_then(|v| v.as_object_mut()) {
            if servers.remove(MCP_SERVER_NAME).is_some() {
                removed = true;
            }
        }
    }
    if removed {
        write_json_pretty(&path, &root)?;
        actions.push(InstallAction {
            agent: "vscode".into(),
            path: path.display().to_string(),
            action: "removed_mcp".into(),
            detail: "ravel".into(),
        });
    } else {
        actions.push(InstallAction {
            agent: "vscode".into(),
            path: path.display().to_string(),
            action: "skip".into(),
            detail: "ravel not present".into(),
        });
    }
    Ok(())
}

fn write_project_instructions(root: &Path, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    let block = agent_instruction_block();
    for name in ["AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
        let path = root.join(name);
        // Only create AGENTS.md automatically; append to others only if they exist.
        if name != "AGENTS.md" && !path.exists() {
            continue;
        }
        let mut text = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            "# Agent notes\n".to_string()
        };
        if text.contains(MARKER_BEGIN) {
            // Refresh block
            text = replace_marked_section(&text, &block);
        } else {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&block);
        }
        fs::write(&path, text)?;
        actions.push(InstallAction {
            agent: "instructions".into(),
            path: path.display().to_string(),
            action: "wrote_instructions".into(),
            detail: name.into(),
        });
    }
    // Cursor project rule only if the project already uses .cursor/
    if root.join(".cursor").is_dir() {
        let cursor_rule = root.join(".cursor").join("rules").join("ravel.mdc");
        if let Some(parent) = cursor_rule.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = format!(
            "---\ndescription: Prefer Ravel code graph over multi-file grep\nglobs:\nalwaysApply: true\n---\n\n{}\n",
            agent_instruction_block()
        );
        fs::write(&cursor_rule, content)?;
        actions.push(InstallAction {
            agent: "cursor".into(),
            path: cursor_rule.display().to_string(),
            action: "wrote_rule".into(),
            detail: "ravel.mdc".into(),
        });
    }
    Ok(())
}

fn strip_project_instructions(root: &Path, actions: &mut Vec<InstallAction>) -> anyhow::Result<()> {
    for name in ["AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
        let path = root.join(name);
        if !path.exists() {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        if !text.contains(MARKER_BEGIN) {
            continue;
        }
        let new_text = replace_marked_section(&text, "");
        fs::write(&path, new_text)?;
        actions.push(InstallAction {
            agent: "instructions".into(),
            path: path.display().to_string(),
            action: "stripped_instructions".into(),
            detail: name.into(),
        });
    }
    let cursor_rule = root.join(".cursor").join("rules").join("ravel.mdc");
    if cursor_rule.exists() {
        fs::remove_file(&cursor_rule)?;
        actions.push(InstallAction {
            agent: "cursor".into(),
            path: cursor_rule.display().to_string(),
            action: "removed_rule".into(),
            detail: "ravel.mdc".into(),
        });
    }
    Ok(())
}

fn replace_marked_section(text: &str, replacement: &str) -> String {
    let Some(start) = text.find(MARKER_BEGIN) else {
        return if replacement.is_empty() {
            text.to_string()
        } else {
            format!("{text}\n{replacement}")
        };
    };
    let after_begin = start + MARKER_BEGIN.len();
    let end = text[after_begin..]
        .find(MARKER_END)
        .map(|i| after_begin + i + MARKER_END.len())
        .unwrap_or(text.len());
    let mut out = String::new();
    out.push_str(&text[..start]);
    out.push_str(replacement);
    out.push_str(&text[end..]);
    out
}

fn write_json_pretty(path: &Path, value: &Value) -> anyhow::Result<()> {
    let pretty = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{pretty}\n"))?;
    Ok(())
}

/// List detection + suggested install without writing.
pub fn doctor_agents(project_root: &Path) -> Value {
    let detected: Vec<_> = detect_agents().iter().map(|a| a.id()).collect();
    let bin = resolve_ravel_bin();
    json!({
        "ravel_bin": bin.display().to_string(),
        "project": project_root.display().to_string(),
        "detected_agents": detected,
        "supported": AgentKind::all().iter().map(|a| {
            json!({
                "id": a.id(),
                "label": a.label(),
                "detected": agent_looks_installed(*a),
            })
        }).collect::<Vec<_>>(),
        "hint": "ravel install --yes   # wire MCP into detected agents",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_targets_auto_and_csv() {
        let all = AgentKind::parse_csv("all").unwrap();
        assert_eq!(all.len(), AgentKind::all().len());
        let few = AgentKind::parse_csv("claude,cursor").unwrap();
        assert_eq!(few, vec![AgentKind::Claude, AgentKind::Cursor]);
    }

    #[test]
    fn upsert_and_remove_json_mcp() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        fs::write(&path, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        upsert_json_mcp_servers(&path, Path::new("/usr/bin/ravel"), true).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            v["mcpServers"]["ravel"]["command"]
                .as_str()
                .unwrap()
                .contains("ravel")
        );
        assert_eq!(v["mcpServers"]["other"]["command"], "x");

        let mut actions = Vec::new();
        remove_json_mcp_key(AgentKind::Cursor, &path, &mut actions, "mcpServers").unwrap();
        let v2: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v2["mcpServers"].get("ravel").is_none());
        assert!(v2["mcpServers"].get("other").is_some());
    }

    #[test]
    fn replace_toml_table_roundtrip() {
        let text = r#"
[foo]
a = 1

[mcp_servers.other]
command = "x"

[mcp_servers.ravel]
command = "old"
args = ["mcp"]

[bar]
b = 2
"#;
        let next = replace_toml_table(
            text,
            "mcp_servers.ravel",
            "\n[mcp_servers.ravel]\ncommand = \"new\"\nargs = [\"mcp\"]\n",
        );
        assert!(next.contains("command = \"new\""));
        assert!(next.contains("[mcp_servers.other]"));
        assert!(next.contains("[bar]"));
        let gone = replace_toml_table(&next, "mcp_servers.ravel", "");
        assert!(!gone.contains("[mcp_servers.ravel]"));
        assert!(gone.contains("[mcp_servers.other]"));
    }

    #[test]
    fn marked_instructions_refresh() {
        let text = format!("# x\n{MARKER_BEGIN}\nold\n{MARKER_END}\n# y\n");
        let next = replace_marked_section(&text, &agent_instruction_block());
        assert!(next.contains("explore SYMBOL"));
        assert!(next.contains("# y"));
        assert_eq!(next.matches(MARKER_BEGIN).count(), 1);
    }

    #[test]
    fn local_install_claude_mcp_json() {
        let dir = tempdir().unwrap();
        let opts = InstallOptions {
            targets: vec![AgentKind::Claude],
            location: InstallLocation::Local,
            project_root: dir.path().to_path_buf(),
            ravel_bin: PathBuf::from("/opt/ravel"),
            write_instructions: true,
            claude_permissions: false,
        };
        let report = install_agents(&opts).unwrap();
        assert!(report.actions.iter().any(|a| a.action == "wrote_mcp"));
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["ravel"]["command"], "/opt/ravel");
        assert!(dir.path().join("AGENTS.md").exists());
    }

    #[test]
    fn print_config_nonempty() {
        for kind in AgentKind::all() {
            let s = print_config(*kind, Path::new("/bin/ravel"), InstallLocation::Global);
            assert!(!s.is_empty(), "{}", kind.id());
        }
    }
}
