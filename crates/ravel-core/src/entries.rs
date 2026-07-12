//! Generic entry-point discovery for projects and monorepos (not Nest-only).
//!
//! Sources (all automatic, no config):
//! 1. Path/name heuristics (Nest/Express/Next-style) — see `analysis::is_natural_entry_point`
//! 2. `package.json` `main` / `module` / `browser` / `exports` / `bin` fields
//! 3. Root `tsconfig.json` `files` array (if small)
//!
//! These files/symbols often have zero *in-repo* importers but are real process roots.

use rustc_hash::FxHashSet;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// Collect relative paths that package managers / bundlers treat as package entries.
pub fn collect_manifest_entry_paths(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // Prefer package roots; also inspect the root package.json
    let mut package_jsons = Vec::new();
    if root.join("package.json").is_file() {
        package_jsons.push(root.join("package.json"));
    }
    for base in ["apps", "libs", "packages", "services", "src", "team"] {
        let dir = root.join(base);
        if !dir.is_dir() {
            continue;
        }
        // shallow walk: apps/*/package.json, packages/*/*/package.json (depth 3)
        collect_package_jsons(&dir, 0, 3, &mut package_jsons);
    }
    for pj in package_jsons {
        let Ok(text) = fs::read_to_string(&pj) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let pkg_dir = pj.parent().unwrap_or(root);
        for field in ["main", "module", "browser", "types", "typings"] {
            if let Some(s) = json.get(field).and_then(|v| v.as_str()) {
                push_resolved(root, pkg_dir, s, &mut out);
            }
        }
        if let Some(bin) = json.get("bin") {
            match bin {
                Value::String(s) => push_resolved(root, pkg_dir, s, &mut out),
                Value::Object(map) => {
                    for v in map.values() {
                        if let Some(s) = v.as_str() {
                            push_resolved(root, pkg_dir, s, &mut out);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(exports) = json.get("exports") {
            walk_exports(root, pkg_dir, exports, &mut out);
        }
    }
    // tsconfig "files" (explicit entry list — rare but generic)
    for name in ["tsconfig.json", "tsconfig.build.json", "tsconfig.app.json"] {
        let p = root.join(name);
        if let Ok(text) = fs::read_to_string(&p) {
            // strip comments roughly for jsonc
            let cleaned: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(json) = serde_json::from_str::<Value>(&cleaned) {
                if let Some(files) = json.get("files").and_then(|v| v.as_array()) {
                    for f in files {
                        if let Some(s) = f.as_str() {
                            push_resolved(root, root, s, &mut out);
                        }
                    }
                }
            }
        }
    }
    out
}

fn collect_package_jsons(dir: &Path, depth: u32, max_depth: u32, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        // `to_string_lossy` borrows (no alloc) for valid UTF-8; `file_type` avoids a second stat.
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name == "node_modules" || name == "dist" || name == ".git" {
            continue;
        }
        let Ok(kind) = e.file_type() else {
            continue;
        };
        if kind.is_file() && name == "package.json" {
            out.push(e.path());
        } else if kind.is_dir() {
            collect_package_jsons(&e.path(), depth + 1, max_depth, out);
        }
    }
}

fn walk_exports(root: &Path, pkg_dir: &Path, exports: &Value, out: &mut BTreeSet<String>) {
    match exports {
        Value::String(s) => push_resolved(root, pkg_dir, s, out),
        Value::Array(arr) => {
            for v in arr {
                walk_exports(root, pkg_dir, v, out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                if k.starts_with('#') {
                    continue; // package imports internal
                }
                match v {
                    Value::String(s) => push_resolved(root, pkg_dir, s, out),
                    Value::Object(inner) => {
                        for field in ["import", "require", "default", "module", "node", "browser"] {
                            if let Some(Value::String(s)) = inner.get(field) {
                                push_resolved(root, pkg_dir, s, out);
                            }
                        }
                    }
                    _ => walk_exports(root, pkg_dir, v, out),
                }
            }
        }
        _ => {}
    }
}

fn push_resolved(root: &Path, pkg_dir: &Path, rel: &str, out: &mut BTreeSet<String>) {
    if rel.is_empty() || rel == "." {
        // package root → try index/main candidates
        for cand in [
            "index.ts",
            "index.js",
            "src/index.ts",
            "src/main.ts",
            "src/index.js",
        ] {
            push_resolved(root, pkg_dir, cand, out);
        }
        return;
    }
    if rel.contains('*') {
        return; // export globs — skip (not a single entry file)
    }
    let path = if rel.starts_with("./") || !rel.starts_with('/') {
        pkg_dir.join(rel.trim_start_matches("./"))
    } else {
        PathBuf::from(rel)
    };
    // Prefer existing file; also try with .ts/.js if extension missing
    let candidates = expand_source_candidates(&path);
    for c in candidates {
        if c.is_file() {
            if let Ok(rel) = c.strip_prefix(root) {
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn expand_source_candidates(path: &Path) -> Vec<PathBuf> {
    let mut v = vec![path.to_path_buf()];
    if path.extension().is_none() {
        for ext in ["ts", "tsx", "js", "mjs", "cjs", "jsx"] {
            v.push(path.with_extension(ext));
        }
        v.push(path.join("index.ts"));
        v.push(path.join("index.js"));
    }
    v
}

pub struct ManifestEntryIndex {
    paths: FxHashSet<String>,
    symbol_names: FxHashSet<String>,
}

impl ManifestEntryIndex {
    pub fn new(entry_files: &BTreeSet<String>) -> Self {
        let mut paths = FxHashSet::default();
        let mut symbol_names = FxHashSet::default();
        for entry in entry_files {
            let normalized = entry.replace('\\', "/").replace("/./", "/");
            if let Some(file) = normalized.rsplit('/').next() {
                let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
                symbol_names.insert(stem.to_owned());
            }
            paths.insert(normalized);
        }
        Self {
            paths,
            symbol_names,
        }
    }

    pub fn contains(&self, name_or_path: &str) -> bool {
        let normalized;
        let value = if name_or_path.contains('\\') || name_or_path.contains("/./") {
            normalized = name_or_path.replace('\\', "/").replace("/./", "/");
            normalized.as_str()
        } else {
            name_or_path
        };
        self.paths.contains(value) || (!value.contains('/') && self.symbol_names.contains(value))
    }
}

/// True if this graph node is an exact manifest path or declared entry symbol.
pub fn is_manifest_entry(name_or_path: &str, entry_files: &BTreeSet<String>) -> bool {
    ManifestEntryIndex::new(entry_files).contains(name_or_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn package_json_main_is_entry() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"main":"./dist/index.js","module":"./src/main.ts"}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.ts"), "export {}").unwrap();
        let entries = collect_manifest_entry_paths(dir.path());
        assert!(
            entries.iter().any(|e| e.ends_with("src/main.ts")),
            "{entries:?}"
        );
    }

    #[test]
    fn manifest_index_uses_exact_paths_not_substrings() {
        let entries = BTreeSet::from(["packages/api/src/worker.ts".to_owned()]);
        let index = ManifestEntryIndex::new(&entries);
        assert!(index.contains("packages/api/src/worker.ts"));
        assert!(index.contains("worker"));
        assert!(!index.contains("packages/rapidapi/src/worker.ts"));
        assert!(!index.contains("api"));
    }
}
