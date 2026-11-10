use std::process::Command;

#[test]
fn binary_help_and_version_are_available() {
    let binary = env!("CARGO_BIN_EXE_ravel");
    let help = Command::new(binary)
        .arg("--help")
        .output()
        .expect("binary should execute");
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("Local TypeScript/JavaScript code graph")
    );
    let version = Command::new(binary)
        .arg("--version")
        .output()
        .expect("binary should execute");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("ravel {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn init_builds_a_standalone_project_index_by_default() {
    let binary = env!("CARGO_BIN_EXE_ravel");
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ravel-cli-init-{}-{suffix}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.ts"), "export const answer = 42;\n").unwrap();

    let init = Command::new(binary)
        .args(["--root", root.to_str().unwrap(), "init"])
        .output()
        .expect("init should execute");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(String::from_utf8_lossy(&init.stdout).contains("indexed"));
    assert!(root.join(".ravel.toml").is_file());
    assert!(root.join(".ravelignore").is_file());

    let status = Command::new(binary)
        .args(["--root", root.to_str().unwrap(), "status", "--json"])
        .output()
        .expect("status should execute");
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["indexed"], true);
    assert_eq!(status["stats"]["files"], 1);

    std::fs::remove_dir_all(root).unwrap();
}

/// Queries answer in the compact text an agent reads; a script asks for the JSON, by flag or by
/// environment, and gets the same answer in the shape it always had.
#[test]
fn queries_answer_in_text_unless_json_is_asked_for() {
    let binary = env!("CARGO_BIN_EXE_ravel");
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/api")).unwrap();
    std::fs::write(
        root.path().join("src/charge.ts"),
        "export function charge() {}\n",
    )
    .unwrap();
    for name in ["a", "b"] {
        std::fs::write(
            root.path().join(format!("src/api/{name}.ts")),
            "import { charge } from '../charge';\nexport function run() {\n  charge();\n  charge();\n}\n",
        )
        .unwrap();
    }
    let run = |args: &[&str], format: Option<&str>| {
        let mut command = Command::new(binary);
        command.arg("--root").arg(root.path()).args(args);
        match format {
            Some(format) => command.env("RAVEL_FORMAT", format),
            None => command.env_remove("RAVEL_FORMAT"),
        };
        let output = command.output().expect("ravel should execute");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let indexed = run(&["index"], None);
    assert!(
        indexed.starts_with("index: 3 files, ") && indexed.ends_with(" edges, 0 parse error(s)\n"),
        "{indexed}"
    );

    // One line per file, the shared directory once, and repeated calls folded onto one label.
    assert_eq!(
        run(&["callers-of", "charge"], None),
        "callers of symbol://src/charge.ts#value:charge: 6 (Calls 4, Import 2)\n\
         src/api/\n \
         a.ts: 1 Import; 3,4 Calls run\n \
         b.ts: 1 Import; 3,4 Calls run\n"
    );
    let status = run(&["status"], None);
    assert!(status.contains(": indexed, 3 files, "), "{status}");
    let explore = run(&["explore", "charge"], None);
    assert!(
        explore.starts_with("charge: exported function_declaration, src/charge.ts:1-1\n"),
        "{explore}"
    );

    for json in [
        run(&["callers-of", "charge", "--json"], None),
        run(&["--json", "callers-of", "charge"], None),
        run(&["callers-of", "charge", "--pretty"], None),
        run(&["callers-of", "charge"], Some("json")),
    ] {
        let page: serde_json::Value = serde_json::from_str(&json).expect("a JSON page");
        assert_eq!(page["total"], 6, "{json}");
        assert_eq!(page["sites"].as_array().map(Vec::len), Some(6), "{json}");
    }
    // The flag wins over an environment that asks for text, and an unknown value means text.
    assert!(run(&["status", "--json"], Some("text")).starts_with('{'));
    assert!(run(&["status"], Some("yaml")).starts_with("ravel "));
}
