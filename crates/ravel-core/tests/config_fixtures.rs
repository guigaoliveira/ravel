use ravel_core::config::{Config, Flags};
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;

#[test]
fn precedence_is_defaults_file_env_then_flags_and_diagnostics_are_secret_free() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join(".ravel.toml"),
        "[limits]\nmax_nodes = 10\n[parser]\nmax_file_size_kb = 2\n",
    )
    .unwrap();
    let env = BTreeMap::from([
        (String::from("RAVEL_MAX_NODES"), String::from("20")),
        (String::from("RAVEL_LOG_LEVEL"), String::from("debug")),
    ]);
    let flags = Flags {
        max_nodes: Some(30),
        ..Default::default()
    };
    let config = Config::load_with_env(root.path(), &flags, &env).unwrap();
    assert_eq!(config.limits.max_nodes, 30);
    assert_eq!(config.parser.max_file_size_kb, 2);
    assert_eq!(config.log_level, "debug");
    assert!(
        !config
            .effective_json()
            .to_string()
            .contains("RAVEL_MAX_NODES")
    );
}

#[test]
fn ravelignore_gitignore_and_ordering_are_respected() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::create_dir_all(root.path().join("generated")).unwrap();
    fs::write(root.path().join("src/a.ts"), "export const a = 1;").unwrap();
    fs::write(root.path().join("src/b.ts"), "export const b = 1;").unwrap();
    fs::write(root.path().join("generated/c.ts"), "export const c = 1;").unwrap();
    fs::write(
        root.path().join(".ravelignore"),
        "generated/\n!generated/c.ts\n",
    )
    .unwrap();
    let config = Config::load(root.path(), &Flags::default()).unwrap();
    let files = ravel_core::config::discover_files(&config).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files[0] < files[1]);
    assert!(files.iter().all(|path| !path.ends_with("generated/c.ts")));
}
