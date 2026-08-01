//! `validate` must not answer "no violations" when it could not read the policy it was given.
//!
//! A silent zero is the worst answer this command can give: it is indistinguishable from a clean
//! repository, so a broken policy file protects nothing while looking like it protects everything.
use ravel_core::config::Flags;
use ravel_core::engine::WorkspaceEngine;
use std::{fs, path::Path};
use tempfile::tempdir;

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Two packages with one import between them, and deliberately in *different* top-level
/// directories: an `apps/ -> libs/` edge is what the removed prefix rule reported, so a fixture
/// confined to one directory would pass whether or not the rule is gone.
fn workspace() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    write_file(
        dir.path(),
        "apps/service-a/src/x.ts",
        "import { y } from '../../../libs/money/src/y';\nexport const x = y;\n",
    );
    write_file(
        dir.path(),
        "libs/money/src/y.ts",
        "export const y = true;\n",
    );
    dir
}

#[test]
fn an_unparsable_policy_file_fails_loudly_instead_of_reporting_zero() {
    let workspace = workspace();
    write_file(
        workspace.path(),
        "ravel.boundaries.toml",
        "[[layers]]\nname = \"service\"\npackages = [\"service-a\"   # unterminated\n",
    );

    let engine = WorkspaceEngine::load(workspace.path(), &Flags::default()).unwrap();
    engine.index().unwrap();

    let error = engine
        .validate()
        .expect_err("a policy file that cannot be parsed must not be reported as zero violations");
    let message = error.to_string();
    assert!(
        message.contains("ravel.boundaries.toml") && message.contains("could not be applied"),
        "the error must name the file and say it was not applied, got: {message}"
    );
}

#[test]
fn a_workspace_with_no_policy_file_still_validates() {
    let workspace = workspace();
    let engine = WorkspaceEngine::load(workspace.path(), &Flags::default()).unwrap();
    engine.index().unwrap();

    // The file is optional: absent means "no declared policy", not "unreadable policy".
    let findings = engine
        .validate()
        .expect("absent policy file is not an error");
    assert!(
        findings.is_empty(),
        "an import between two directories is not a violation without a declared policy, got {findings:?}"
    );
}

#[test]
fn a_parsable_policy_file_is_applied() {
    let workspace = workspace();
    write_file(
        workspace.path(),
        "ravel.boundaries.toml",
        r#"
[[layers]]
name = "service"
packages = ["service-a"]
forbidden_deps = ["shared"]

[[layers]]
name = "shared"
packages = ["money"]
"#,
    );

    let engine = WorkspaceEngine::load(workspace.path(), &Flags::default()).unwrap();
    engine.index().unwrap();

    let findings = engine
        .validate()
        .expect("a valid policy file is not an error");
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "layer_bypass"),
        "the declared forbidden dependency must be reported, got {findings:?}"
    );
}
