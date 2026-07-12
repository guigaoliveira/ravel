use ravel_core::config::Flags;
use ravel_core::engine::WorkspaceEngine;
use std::{fs, path::Path};
use tempfile::tempdir;

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn orphan_detection_respects_manifest_entries_and_natural_entry_points() {
    let workspace = tempdir().unwrap();

    write_file(
        workspace.path(),
        "team/runtime/package.json",
        r#"{
            "main": "./src/start.ts",
            "bin": { "runtime-cli": "./src/worker.mjs" },
            "exports": { "./runtime": { "import": "./src/exported.js" } }
        }"#,
    );
    write_file(
        workspace.path(),
        "team/runtime/tsconfig.json",
        r#"{
            "files": ["./src/config-root.ts"],
            "include": ["./src/**/*.ts"]
        }"#,
    );

    // Manifest-declared roots and their reachable deps → not orphans.
    write_file(
        workspace.path(),
        "team/runtime/src/start.ts",
        "import { used } from './used'; export { used };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/used.ts",
        "export const used = true;\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/worker.mjs",
        "import { worker } from './worker-dep.mjs'; export { worker };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/worker-dep.mjs",
        "export const worker = true;\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/exported.js",
        "import { exported } from './exported-dep.js'; export { exported };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/exported-dep.js",
        "export const exported = true;\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/config-root.ts",
        "import { configured } from './config-dep'; export { configured };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/config-dep.ts",
        "export const configured = true;\n",
    );

    // Natural entry points by naming convention → not orphans.
    write_file(
        workspace.path(),
        "team/runtime/src/index.ts",
        "import { indexDep } from './index-dep'; export { indexDep };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/index-dep.ts",
        "export const indexDep = true;\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/main.ts",
        "import { mainDep } from './main-dep'; export { mainDep };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/main-dep.ts",
        "export const mainDep = true;\n",
    );

    // Application-style file that exports but is not a natural entry → orphan.
    write_file(
        workspace.path(),
        "team/runtime/src/users.controller.ts",
        "import { handler } from './users-handler'; export { handler };\n",
    );
    write_file(
        workspace.path(),
        "team/runtime/src/users-handler.ts",
        "export const handler = true;\n",
    );

    // Separate project scope with its own natural entry.
    write_file(
        workspace.path(),
        "other/project/tsconfig.json",
        r#"{
            "include": ["./src/**/*.ts"]
        }"#,
    );
    write_file(
        workspace.path(),
        "other/project/src/main.ts",
        "import { dep } from './dep'; export { dep };\n",
    );
    write_file(
        workspace.path(),
        "other/project/src/dep.ts",
        "export const dep = true;\n",
    );

    let engine = WorkspaceEngine::load(workspace.path(), &Flags::default()).unwrap();
    engine.index().unwrap();

    let orphans = engine.orphans(100).unwrap();

    // Natural entry points are excluded.
    assert!(
        !orphans.iter().any(|p| p.contains("index.ts")),
        "index.ts should not be orphan: {orphans:?}"
    );
    assert!(
        !orphans.iter().any(|p| p.contains("main.ts")),
        "main.ts should not be orphan: {orphans:?}"
    );

    // users.controller.ts has no caller and is not a natural entry → orphan.
    assert!(
        orphans.iter().any(|p| p.contains("users.controller")),
        "users.controller.ts should be orphan: {orphans:?}"
    );

    // Manifest-backed files are excluded.
    for reachable in [
        "start.ts",
        "used.ts",
        "worker.mjs",
        "worker-dep.mjs",
        "exported.js",
        "exported-dep.js",
    ] {
        assert!(
            !orphans.iter().any(|p| p.contains(reachable)),
            "{reachable} should not be orphan: {orphans:?}"
        );
    }

    // other/project's natural entry (main.ts) is excluded.
    assert!(
        !orphans.iter().any(|p| p == "other/project/src/main.ts"),
        "other/project/src/main.ts should not be orphan: {orphans:?}"
    );
}

#[test]
fn workspace_without_declared_roots_is_conservative() {
    let workspace = tempdir().unwrap();
    write_file(
        workspace.path(),
        "arbitrary/main.ts",
        "import { dep } from './dep'; export { dep };\n",
    );
    write_file(
        workspace.path(),
        "arbitrary/dep.ts",
        "export const dep = true;\n",
    );

    let engine = WorkspaceEngine::load(workspace.path(), &Flags::default()).unwrap();
    engine.index().unwrap();
    assert!(engine.orphans(100).unwrap().is_empty());
}
