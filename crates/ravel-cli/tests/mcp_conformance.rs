//! MCP protocol conformance for `ravel mcp` (stdio).
//!
//! Every major coding agent (Claude Code, Codex, OpenCode, Cursor, Gemini, Windsurf, …) is an
//! MCP client, so if the server speaks the protocol correctly it works with all of them. This
//! test drives the real binary over stdio JSON-RPC and asserts the handshake, tool listing, and
//! a tool call — the agent-agnostic guarantee.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn mcp_stdio_speaks_protocol() {
    // Run the server in an empty dir — protocol conformance must not need a pre-built index.
    let tmp = std::env::temp_dir().join(format!("ravel-mcp-conf-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ravel"))
        .args(["--root", tmp.to_str().unwrap(), "mcp"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn `ravel mcp`");

    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"conformance","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
        stdin.flush().unwrap();
        // stdin dropped here → EOF, so a well-behaved server can also shut down on its own.
    }

    // Read stdout on a helper thread so a non-exiting server can't block the test forever.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut by_id: HashMap<u64, serde_json::Value> = HashMap::new();
    let mut seen_lines: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && by_id.len() < 3 {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                        by_id.insert(id, v);
                    }
                }
                seen_lines.push(line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    let _ = std::fs::remove_dir_all(&tmp);

    let dump = || seen_lines.join("\n");

    // 1. initialize → server advertises itself.
    let init = by_id
        .get(&1)
        .unwrap_or_else(|| panic!("no initialize response.\n--- stdout ---\n{}", dump()));
    assert!(
        init["result"]["serverInfo"].is_object(),
        "initialize missing result.serverInfo: {init}"
    );

    // 2. tools/list → the primary tool surface is present.
    let list = by_id
        .get(&2)
        .unwrap_or_else(|| panic!("no tools/list response.\n--- stdout ---\n{}", dump()));
    let tools: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools/list result.tools is an array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in ["explore", "status", "sync"] {
        assert!(
            tools.contains(&expected),
            "tools/list missing `{expected}` (got {tools:?})"
        );
    }
    let explore = list["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "explore"))
        .expect("primary explore tool is present");
    assert!(
        explore["inputSchema"]["properties"].get("kind").is_none(),
        "explore schema must not advertise the search-only kind field: {explore}"
    );

    // 3. tools/call → a real invocation returns a JSON-RPC result (not an error).
    let call = by_id
        .get(&3)
        .unwrap_or_else(|| panic!("no tools/call response.\n--- stdout ---\n{}", dump()));
    assert!(
        call.get("result").is_some() && call.get("error").is_none(),
        "tools/call `status` did not return a successful result: {call}"
    );
    let status_text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("status tool should return text content");
    assert!(
        status_text.contains(tmp.to_str().unwrap()),
        "MCP did not use the CLI --root default: {status_text}"
    );
}
