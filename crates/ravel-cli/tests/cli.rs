use std::process::Command;

#[test]
fn binary_help_and_version_are_available() {
    let binary = env!("CARGO_BIN_EXE_ravel");
    let help = Command::new(binary)
        .arg("--help")
        .output()
        .expect("binary should execute");
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Dependency indexer"));
    let version = Command::new(binary)
        .arg("--version")
        .output()
        .expect("binary should execute");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("ravel 1.0.0"));
}
