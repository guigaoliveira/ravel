use ravel_core::{config::Flags, engine::WorkspaceEngine};
use std::fs;
use tempfile::tempdir;

#[test]
fn refactor_plan_covers_duplicate_definitions_and_type_importers() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/definition.ts"),
        "export interface Shared { id: string }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/duplicate.ts"),
        "export interface Shared { code: string }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/use.ts"),
        "import type { Shared } from './definition';\nexport const use = (value: Shared) => value;\n",
    )
    .unwrap();

    let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
    engine.index().unwrap();
    let plan = engine.refactor_plan("Shared", 20).unwrap();
    let files: Vec<&str> = plan["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str())
        .collect();

    for expected in ["src/definition.ts", "src/duplicate.ts", "src/use.ts"] {
        assert!(
            files.contains(&expected),
            "refactor plan omitted {expected}: {plan}"
        );
    }
}
