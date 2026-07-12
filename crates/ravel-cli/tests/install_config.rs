//! Per-agent MCP install-config schema round-trip.
//!
//! Every agent reads its MCP config from a different file in a different shape. `ravel install`
//! must emit a snippet that matches each agent's *current* schema — otherwise the wiring is
//! silently broken even though the MCP server itself is fine. This drives
//! `ravel install --print-config <agent>` (no writes) and asserts the emitted snippet parses and
//! carries the `ravel` server entry. If an agent changes its config format, this test fails.

use std::process::Command;

/// Strip the leading `# path` comment lines the CLI prints before the actual snippet body.
fn snippet_body(agent: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ravel"))
        .args(["install", "--print-config", agent])
        .output()
        .unwrap_or_else(|e| panic!("spawn install --print-config {agent}: {e}"));
    assert!(
        out.status.success(),
        "install --print-config {agent} exited non-zero"
    );
    let text = String::from_utf8(out.stdout).expect("utf8");
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON agents: body parses as JSON and `<container>.ravel` is an object.
fn assert_json_agent(agent: &str, container_key: &str) {
    let body = snippet_body(agent);
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("{agent} snippet is not valid JSON: {e}\n{body}"));
    let ravel = &v[container_key]["ravel"];
    assert!(
        ravel.is_object(),
        "{agent}: expected `{container_key}.ravel` object, got:\n{body}"
    );
    // command is either a string (most) or an array (opencode) — either way must mention ravel.
    assert!(
        body.contains("ravel"),
        "{agent}: snippet does not reference the ravel binary:\n{body}"
    );
}

#[test]
fn claude_mcp_config_schema() {
    assert_json_agent("claude", "mcpServers");
}

#[test]
fn cursor_mcp_config_schema() {
    assert_json_agent("cursor", "mcpServers");
}

#[test]
fn gemini_mcp_config_schema() {
    assert_json_agent("gemini", "mcpServers");
}

#[test]
fn windsurf_mcp_config_schema() {
    assert_json_agent("windsurf", "mcpServers");
}

#[test]
fn vscode_mcp_config_schema() {
    assert_json_agent("vscode", "servers");
}

#[test]
fn opencode_mcp_config_schema() {
    assert_json_agent("opencode", "mcp");
}

#[test]
fn codex_mcp_config_schema() {
    // Codex uses TOML (`~/.codex/config.toml`). Assert the table + command key are present.
    let body = snippet_body("codex");
    assert!(
        body.contains("[mcp_servers.ravel]"),
        "codex snippet missing [mcp_servers.ravel] table:\n{body}"
    );
    assert!(
        body.contains("command"),
        "codex snippet missing command key:\n{body}"
    );
}
