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
        .args(["--root", root.to_str().unwrap(), "status"])
        .output()
        .expect("status should execute");
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["indexed"], true);
    assert_eq!(status["stats"]["files"], 1);

    std::fs::remove_dir_all(root).unwrap();
}
