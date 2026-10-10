//! `ravel install` / `uninstall` against real config files, in a sandboxed home.
//!
//! Every location the installer reads (HOME, the XDG and Windows config roots, CODEX_HOME,
//! CLAUDE_CONFIG_DIR, the runtime directory its locks live in) is pointed inside a temporary
//! directory, so a test can never touch the real user's agent configs — whatever the code under
//! test gets wrong.

use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, ExitStatus},
};
use tempfile::tempdir;

fn sandboxed(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ravel"));
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("CODEX_HOME", home.join(".codex"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .current_dir(home);
    command
}

fn run(mut command: Command, args: &[&str]) -> (ExitStatus, Value) {
    let out = command.args(args).output().expect("spawn ravel");
    let report = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "{args:?}: no JSON report ({error})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status, report)
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn codex_install_and_uninstall_keep_the_tables_after_the_ravel_entry() {
    let home = tempdir().unwrap();
    let config = home.path().join(".codex").join("config.toml");
    write(
        &config,
        "[mcp_servers.ravel]\ncommand = \"/old/ravel\"\n\n\
         [[hooks]]\nname = \"keep-me\"\n\n[[hooks]]\nname = \"keep-me-too\"\n\n\
         # the github server\n  [mcp_servers.github]\n  command = \"gh\"\n  env = { TOKEN = \"t\" }\n",
    );
    let kept = |text: &str| {
        for line in [
            "[[hooks]]\nname = \"keep-me\"\n",
            "[[hooks]]\nname = \"keep-me-too\"\n",
            "# the github server\n  [mcp_servers.github]\n  command = \"gh\"\n  env = { TOKEN = \"t\" }\n",
        ] {
            assert!(text.contains(line), "lost {line:?}:\n{text}");
        }
    };

    let (status, report) = run(
        sandboxed(home.path()),
        &["install", "--target", "codex", "--no-instructions"],
    );
    assert!(status.success(), "{report}");
    let text = fs::read_to_string(&config).unwrap();
    kept(&text);
    assert!(text.contains("[mcp_servers.ravel]"), "{text}");
    assert!(!text.contains("/old/ravel"), "{text}");

    let (status, report) = run(
        sandboxed(home.path()),
        &["uninstall", "--target", "codex", "--no-instructions"],
    );
    assert!(status.success(), "{report}");
    let text = fs::read_to_string(&config).unwrap();
    kept(&text);
    assert!(!text.contains("[mcp_servers.ravel]"), "{text}");
}

#[test]
fn a_local_install_never_touches_the_global_windsurf_config() {
    let home = tempdir().unwrap();
    let project = tempdir().unwrap();
    let windsurf = home
        .path()
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json");
    let original = r#"{"mcpServers":{"ravel":{"command":"/abs/ravel","args":["serve","--mcp"]}}}"#;
    write(&windsurf, original);
    let root = project.path().to_str().unwrap();

    for verb in ["install", "uninstall"] {
        let (status, report) = run(
            sandboxed(home.path()),
            &[
                "--root",
                root,
                verb,
                "--target",
                "windsurf",
                "--location",
                "local",
                "--no-instructions",
            ],
        );
        assert!(status.success(), "{verb}: {report}");
        assert_eq!(
            fs::read_to_string(&windsurf).unwrap(),
            original,
            "{verb} --location local rewrote the user's global Windsurf config"
        );
        assert!(
            report["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["agent"] == "windsurf" && action["action"] == "skip"),
            "{verb}: {report}"
        );
    }
}

#[test]
fn opencode_global_config_lives_under_xdg_config_home_on_every_platform() {
    let home = tempdir().unwrap();
    let xdg = home.path().join("xdg");
    let mut command = sandboxed(home.path());
    command.env("XDG_CONFIG_HOME", &xdg);
    let (status, report) = run(
        command,
        &["install", "--target", "opencode", "--no-instructions"],
    );
    assert!(status.success(), "{report}");
    let config = read_json(&xdg.join("opencode").join("opencode.json"));
    assert!(config["mcp"]["ravel"].is_object(), "{config}");

    // An empty XDG_CONFIG_HOME is unset (XDG Base Directory spec), not the current directory.
    let home = tempdir().unwrap();
    let mut command = sandboxed(home.path());
    command.env("XDG_CONFIG_HOME", "");
    let (status, report) = run(
        command,
        &["install", "--target", "opencode", "--no-instructions"],
    );
    assert!(status.success(), "{report}");
    let config = home.path().join(".config/opencode/opencode.json");
    assert!(config.is_file(), "{report}");
    assert!(!home.path().join("opencode").exists(), "{report}");
}
