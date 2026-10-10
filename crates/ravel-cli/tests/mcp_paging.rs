//! A relation page hands out its successor as `next_cursor`, and the tool says to pass it back as
//! `cursor`. Driven through `ravel mcp` over stdio, the way an agent pages.

use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Session {
    fn start(root: &Path) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ravel"))
            .arg("--root")
            .arg(root)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        let mut session = Session {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        session.request(
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}"#,
        );
        writeln!(
            session.stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        session
    }

    fn request(&mut self, method: &str, params: &str) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        )
        .unwrap();
        self.stdin.flush().unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect("`ravel mcp` did not answer");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    /// The tool's answer, or a panic with the error it reported.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let params = serde_json::json!({ "name": tool, "arguments": arguments }).to_string();
        let reply = self.request("tools/call", &params);
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"));
        assert_ne!(reply["result"]["isError"], true, "{tool} failed: {text}");
        serde_json::from_str(text).unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn callers_of_pages_on_with_the_cursor_it_handed_out() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/base.ts"),
        "export function pagedBase() { return 1; }\n",
    )
    .unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(
            root.join(format!("src/{name}.ts")),
            format!(
                "import {{ pagedBase }} from './base';\nexport const {name}Uses = pagedBase();\n"
            ),
        )
        .unwrap();
    }
    let index = Command::new(env!("CARGO_BIN_EXE_ravel"))
        .arg("--root")
        .arg(root)
        .arg("index")
        .output()
        .unwrap();
    assert!(index.status.success());

    let mut session = Session::start(root);
    let first = session.call(
        "callers_of",
        serde_json::json!({ "node": "pagedBase", "limit": 1 }),
    );
    let total = first["total"].as_u64().unwrap();
    assert!(total >= 3, "{first}");
    let mut cursor = first["next_cursor"].clone();
    let mut seen = first["sites"].as_array().unwrap().len();
    while !cursor.is_null() {
        assert!(cursor.is_string(), "next_cursor is a string: {cursor}");
        let page = session.call(
            "callers_of",
            serde_json::json!({ "node": "pagedBase", "limit": 1, "cursor": cursor }),
        );
        seen += page["sites"].as_array().unwrap().len();
        cursor = page["next_cursor"].clone();
    }
    assert_eq!(seen as u64, total, "paging did not reach every site");
}
