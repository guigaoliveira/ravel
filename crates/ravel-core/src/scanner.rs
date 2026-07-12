use crate::{
    config::{Config, ConfigError, discover_files},
    model::{
        Complexity, Diagnostic, EdgeKind, Export, FileArtifact, Import, Span, Symbol, SymbolRef,
    },
};
use rayon::prelude::*;
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use thiserror::Error;
use tree_sitter::{Node, Parser, Tree};

pub const EXTRACTOR_VERSION: &str = "ts-structural-v1";
pub const GRAMMAR_VERSION: &str = "tree-sitter-typescript-0.23";
static EXTRACTOR_VERSION_SHARED: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from(EXTRACTOR_VERSION));
static GRAMMAR_VERSION_SHARED: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(GRAMMAR_VERSION));
static TYPESCRIPT_LANGUAGE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("typescript"));
static JAVASCRIPT_LANGUAGE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("javascript"));

#[derive(Debug, Error)]
pub enum ScanError {
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ScanStats {
    pub files: usize,
    pub bytes_read: u64,
    pub parse_errors: usize,
}

pub fn scan_workspace(config: &Config) -> Result<(Vec<FileArtifact>, ScanStats), ScanError> {
    let files = discover_files(config)?;
    let max_bytes = config.parser.max_file_size_kb.saturating_mul(1024);
    let root = normalize_path(&config.project.root.to_string_lossy());
    let root = root.trim_end_matches("/.").trim_end_matches('/').to_owned();
    let artifacts: Vec<FileArtifact> = files
        .par_iter()
        .map(|path| {
            let abs = normalize_path(&path.to_string_lossy());
            let rel = abs
                .strip_prefix(&root)
                .unwrap_or(&abs)
                .trim_start_matches('/')
                .to_owned();
            let mut artifact = match scan_file(path, max_bytes) {
                Ok(a) => a,
                Err(error) => FileArtifact {
                    path: rel.clone(),
                    language: language(path),
                    source_hash: String::new(),
                    parser_version: Arc::clone(&GRAMMAR_VERSION_SHARED),
                    extractor_version: Arc::clone(&EXTRACTOR_VERSION_SHARED),
                    diagnostics: vec![Diagnostic {
                        code: "read_failed".into(),
                        message: error.to_string(),
                        path: Some(path.to_string_lossy().into_owned()),
                        span: None,
                    }],
                    symbols: Vec::new(),
                    imports: Vec::new(),
                    exports: Vec::new(),
                    symbol_refs: Vec::new(),
                    bytes_read: 0,
                },
            };
            artifact.path = rel;
            artifact
        })
        .collect();
    let mut artifacts = artifacts;
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    // Single pass for bytes + parse-error count instead of two `.iter()` folds.
    let (bytes_read, parse_errors) = artifacts.iter().fold((0u64, 0usize), |(bytes, errors), a| {
        (
            bytes + a.bytes_read,
            errors + usize::from(!a.diagnostics.is_empty()),
        )
    });
    let stats = ScanStats {
        files: artifacts.len(),
        bytes_read,
        parse_errors,
    };
    Ok((artifacts, stats))
}

fn normalize_path(s: &str) -> String {
    // Remove `/./` components so absolute vs tempdir paths are consistent.
    // Fast path: most paths (all on Unix) need neither substitution — one alloc, not two.
    if !s.contains('\\') && !s.contains("/./") {
        return s.to_owned();
    }
    s.replace('\\', "/").replace("/./", "/")
}

pub fn scan_file(path: &Path, max_bytes: u64) -> std::io::Result<FileArtifact> {
    let metadata = fs::metadata(path)?;
    let path_string = normalize_path(&path.to_string_lossy());
    if metadata.len() > max_bytes {
        return Ok(FileArtifact {
            path: path_string.clone(),
            language: language(path),
            source_hash: String::new(),
            parser_version: Arc::clone(&GRAMMAR_VERSION_SHARED),
            extractor_version: Arc::clone(&EXTRACTOR_VERSION_SHARED),
            diagnostics: vec![Diagnostic {
                code: "file_too_large".into(),
                message: format!("file is {} bytes, limit is {}", metadata.len(), max_bytes),
                path: Some(path_string),
                span: None,
            }],
            symbols: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            symbol_refs: Vec::new(),
            bytes_read: 0,
        });
    }
    let source = fs::read(path)?;
    Ok(parse_source(&path_string, &source))
}

struct PooledParsers {
    ts: Parser,
    tsx: Parser,
    ts_ok: bool,
    tsx_ok: bool,
}

fn make_parsers() -> PooledParsers {
    let mut ts = Parser::new();
    let ts_ok = ts
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .is_ok();
    let mut tsx = Parser::new();
    let tsx_ok = tsx
        .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
        .is_ok();
    PooledParsers {
        ts,
        tsx,
        ts_ok,
        tsx_ok,
    }
}

thread_local! {
    // One TS + one TSX parser per (rayon worker) thread. `Parser::new()` + `set_language`
    // is paid once per thread instead of once per file — the dominant per-file scan cost.
    static PARSERS: RefCell<Option<PooledParsers>> = const { RefCell::new(None) };
}

pub fn parse_source(path: &str, source: &[u8]) -> FileArtifact {
    // TS grammar covers .ts/.js/.mjs/.cjs; TSX covers .tsx/.jsx (JSX syntax).
    let is_tsx = path.ends_with(".tsx") || path.ends_with(".jsx");
    PARSERS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let parsers = slot.get_or_insert_with(make_parsers);
        let (parser, grammar_ok) = if is_tsx {
            (&mut parsers.tsx, parsers.tsx_ok)
        } else {
            (&mut parsers.ts, parsers.ts_ok)
        };
        parse_with(parser, grammar_ok, path, source)
    })
}

fn parse_with(parser: &mut Parser, grammar_ok: bool, path: &str, source: &[u8]) -> FileArtifact {
    let mut diagnostics = Vec::new();
    if !grammar_ok {
        diagnostics.push(Diagnostic {
            code: "grammar_unavailable".into(),
            message: "could not load TypeScript/JavaScript grammar".into(),
            path: Some(path.into()),
            span: None,
        });
    }
    let tree = parser.parse(source, None);
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut symbol_refs = Vec::new();
    if let Some(tree) = tree {
        if tree.root_node().has_error() {
            diagnostics.push(Diagnostic {
                code: "syntax_error".into(),
                message: "tree-sitter reported a syntax error".into(),
                path: Some(path.into()),
                span: Some(span(tree.root_node())),
            });
        }
        extract_node(
            tree.root_node(),
            source,
            &mut symbols,
            &mut imports,
            &mut exports,
            &mut symbol_refs,
            &mut None,
            None,
            0,
        );
    } else {
        diagnostics.push(Diagnostic {
            code: "parse_failed".into(),
            message: "parser returned no tree".into(),
            path: Some(path.into()),
            span: None,
        });
    }
    FileArtifact {
        path: path.into(),
        language: language_for_path(path),
        source_hash: blake3::hash(source).to_hex().to_string(),
        parser_version: Arc::clone(&GRAMMAR_VERSION_SHARED),
        extractor_version: Arc::clone(&EXTRACTOR_VERSION_SHARED),
        diagnostics,
        symbols,
        imports,
        exports,
        symbol_refs,
        bytes_read: source.len() as u64,
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_node(
    node: Node<'_>,
    source: &[u8],
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    exports: &mut Vec<Export>,
    refs: &mut Vec<SymbolRef>,
    complexity: &mut Option<(u32, u32)>,
    // Name of the top-level symbol whose subtree we are inside (owns calls/heritage).
    enclosing: Option<&str>,
    nesting: u32,
) {
    let kind = node.kind();

    // Symbol-level references, attributed to the enclosing declaration.
    if let Some(from) = enclosing {
        match kind {
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    if let Some(to) = callee_name(callee, source) {
                        refs.push(SymbolRef {
                            from: from.to_owned(),
                            to,
                            kind: EdgeKind::Calls,
                        });
                    }
                }
            }
            "extends_clause" => push_heritage(node, source, from, EdgeKind::Extends, refs),
            "implements_clause" => push_heritage(node, source, from, EdgeKind::Implements, refs),
            _ => {}
        }
    }
    let is_branch = matches!(
        kind,
        "if_statement"
            | "else_clause"
            | "for_statement"
            | "for_in_statement"
            | "while_statement"
            | "do_statement"
            | "switch_case"
            | "switch_default"
            | "catch_clause"
            | "ternary_expression"
            | "conditional_type"
    ) || matches!(kind, "binary_expression" if is_logical_binary(node));

    if is_branch {
        if let Some((cyc, cog)) = complexity {
            *cyc = cyc.saturating_add(1);
            *cog = cog.saturating_add(1 + nesting);
        }
    }
    let next_nesting = if is_branch { nesting + 1 } else { nesting };

    if kind == "import_statement" {
        if let Some(specifier) = last_string(node, source) {
            imports.push(Import {
                specifier,
                type_only: node_text(node, source)
                    .trim_start()
                    .starts_with("import type"),
                span: span(node),
            });
        }
    } else if kind == "export_statement" {
        let text = node_text(node, source);
        let specifier = last_string(node, source);
        let type_only = text.trim_start().starts_with("export type");
        let name = node
            .child_by_field_name("name")
            .map(|child| node_text(child, source).to_owned());
        exports.push(Export {
            name,
            specifier,
            type_only,
            span: span(node),
        });
    } else if matches!(
        kind,
        "class_declaration"
            | "function_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "lexical_declaration"
    ) {
        let name = node
            .child_by_field_name("name")
            .or_else(|| first_named_identifier(node));
        if let Some(name) = name {
            let idx = symbols.len();
            let name_text = node_text(name, source);
            symbols.push(Symbol {
                name: name_text.to_owned(),
                kind: shared_symbol_kind(kind),
                span: span(node),
                exported: false,
                complexity: None,
            });
            let parent_comp = complexity.take();
            let mut decl_comp = Some((1u32, 0u32));
            let mut cursor = node.walk();
            // Calls/heritage inside this declaration are attributed to it.
            for child in node.children(&mut cursor) {
                extract_node(
                    child,
                    source,
                    symbols,
                    imports,
                    exports,
                    refs,
                    &mut decl_comp,
                    Some(name_text),
                    next_nesting,
                );
            }
            if let Some((cyc, cog)) = decl_comp {
                symbols[idx].complexity = Some(Complexity {
                    cyclomatic: cyc,
                    cognitive: if cog == 0 { 1 } else { cog },
                });
            }
            *complexity = parent_comp;
            return;
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_node(
            child,
            source,
            symbols,
            imports,
            exports,
            refs,
            complexity,
            enclosing,
            next_nesting,
        );
    }
}

/// Name a call targets: bare `foo()` → `foo`; member `a.b()` → `b` (method name).
fn callee_name(callee: Node<'_>, source: &[u8]) -> Option<String> {
    match callee.kind() {
        "identifier" => Some(node_text(callee, source).to_owned()),
        "member_expression" => callee
            .child_by_field_name("property")
            .map(|p| node_text(p, source).to_owned()),
        _ => None,
    }
}

/// Push extends/implements targets (type identifiers in the clause).
fn push_heritage(
    clause: Node<'_>,
    source: &[u8],
    from: &str,
    kind: EdgeKind,
    refs: &mut Vec<SymbolRef>,
) {
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        if matches!(child.kind(), "identifier" | "type_identifier") {
            refs.push(SymbolRef {
                from: from.to_owned(),
                to: node_text(child, source).to_owned(),
                kind: kind.clone(),
            });
        }
    }
}

fn first_named_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "identifier" || child.kind() == "type_identifier")
}

fn is_logical_binary(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| {
        let t = c.kind();
        t == "&&" || t == "||"
    })
}

/// Incremental re-parse when source changes by a single contiguous edit (T028).
/// Falls back to full `parse_source` if the old tree is missing or edit is huge.
pub fn parse_source_incremental(
    path: &str,
    old_source: &[u8],
    new_source: &[u8],
    old_tree: Option<Tree>,
) -> (FileArtifact, bool) {
    let Some(tree) = old_tree else {
        return (parse_source(path, new_source), false);
    };
    // Compute a simple edit from byte-level longest common prefix/suffix.
    let (start, old_end, new_end) = byte_edit_range(old_source, new_source);
    if start == old_end && start == new_end {
        return (parse_source(path, new_source), true);
    }
    // Large rewrites: full parse is often faster/safer.
    let changed = old_end
        .saturating_sub(start)
        .max(new_end.saturating_sub(start));
    if changed > 64 * 1024 || changed * 2 > old_source.len().max(1) {
        return (parse_source(path, new_source), false);
    }
    // Small contiguous edit: symbols are re-extracted from a full parse for accuracy.
    // Reusing the old tree for an incremental parse gave *no* extraction benefit and cost a
    // second parse of new_source — so we keep only the single full parse. True delta
    // extraction (reusing unchanged subtrees) is future work; `old_tree` is retained in the
    // signature for that path.
    let _ = &tree;
    (parse_source(path, new_source), true)
}

fn byte_edit_range(old: &[u8], new: &[u8]) -> (usize, usize, usize) {
    let mut prefix = 0;
    let max_pre = old.len().min(new.len());
    while prefix < max_pre && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut old_suf = old.len();
    let mut new_suf = new.len();
    while old_suf > prefix && new_suf > prefix && old[old_suf - 1] == new[new_suf - 1] {
        old_suf -= 1;
        new_suf -= 1;
    }
    (prefix, old_suf, new_suf)
}

fn last_string(node: Node<'_>, source: &[u8]) -> Option<String> {
    // Find the last string node in document order, then allocate exactly once — the old
    // version `to_owned()`-ed every intermediate string node only to overwrite it.
    last_string_node(node).map(|n| {
        node_text(n, source)
            .trim_matches(['\'', '"', '`'])
            .to_owned()
    })
}

fn last_string_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut found = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "string" | "string_fragment") {
            found = Some(child);
        }
        if child.child_count() > 0 {
            if let Some(nested) = last_string_node(child) {
                found = Some(nested);
            }
        }
    }
    found
}
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}
fn span(node: Node<'_>) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
        start_line: start.row as u32,
        start_column: start.column as u32,
        end_line: end.row as u32,
        end_column: end.column as u32,
    }
}
fn language(path: &Path) -> std::sync::Arc<str> {
    language_for_path(&path.to_string_lossy())
}
fn language_for_path(path: &str) -> std::sync::Arc<str> {
    if path.ends_with(".js")
        || path.ends_with(".jsx")
        || path.ends_with(".mjs")
        || path.ends_with(".cjs")
    {
        Arc::clone(&JAVASCRIPT_LANGUAGE)
    } else {
        Arc::clone(&TYPESCRIPT_LANGUAGE)
    }
}

fn shared_symbol_kind(kind: &str) -> Arc<str> {
    static CLASS: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("class_declaration"));
    static FUNCTION: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("function_declaration"));
    static INTERFACE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("interface_declaration"));
    static TYPE_ALIAS: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("type_alias_declaration"));
    static ENUM: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("enum_declaration"));
    static LEXICAL: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("lexical_declaration"));
    match kind {
        "class_declaration" => Arc::clone(&CLASS),
        "function_declaration" => Arc::clone(&FUNCTION),
        "interface_declaration" => Arc::clone(&INTERFACE),
        "type_alias_declaration" => Arc::clone(&TYPE_ALIAS),
        "enum_declaration" => Arc::clone(&ENUM),
        "lexical_declaration" => Arc::clone(&LEXICAL),
        other => Arc::from(other),
    }
}

pub fn relative_paths(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let mut result: Vec<_> = paths
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_imports_types_and_declarations() {
        let artifact = parse_source("src/a.ts", b"import type { User } from './user';\nexport class A {}\nexport { User } from './user';");
        assert_eq!(artifact.imports[0].specifier, "./user");
        assert!(artifact.imports[0].type_only);
        assert_eq!(artifact.exports.len(), 2);
        assert_eq!(artifact.symbols.len(), 1);
        assert!(artifact.symbols.iter().any(|symbol| symbol.name == "A"));
        assert_eq!(artifact.source_hash.len(), 64);
    }

    #[test]
    fn extracts_symbol_refs_calls_and_heritage() {
        let src = b"class A extends Base implements Iface { run() { helper(); this.other(); } }\nfunction helper() {}\nfunction other() {}";
        let art = parse_source("a.ts", src);
        let has = |from: &str, to: &str, k: EdgeKind| {
            art.symbol_refs
                .iter()
                .any(|r| r.from == from && r.to == to && r.kind == k)
        };
        assert!(has("A", "Base", EdgeKind::Extends), "{:?}", art.symbol_refs);
        assert!(
            has("A", "Iface", EdgeKind::Implements),
            "{:?}",
            art.symbol_refs
        );
        assert!(has("A", "helper", EdgeKind::Calls), "{:?}", art.symbol_refs);
        assert!(has("A", "other", EdgeKind::Calls), "{:?}", art.symbol_refs);
    }

    #[test]
    fn complexity_counts_nested_branches() {
        let src = br#"
export function f(a: number) {
  if (a > 0) {
    if (a > 1) {
      return 1;
    }
  }
  return 0;
}
"#;
        let art = parse_source("f.ts", src);
        let sym = art.symbols.iter().find(|s| s.name == "f").unwrap();
        let c = sym.complexity.as_ref().unwrap();
        assert!(c.cyclomatic >= 3, "cyclomatic={}", c.cyclomatic);
        // outer if (+1) + nested if (+1+nesting) → cognitive ≥ 3
        assert!(c.cognitive >= 3, "cognitive={}", c.cognitive);
    }

    #[test]
    fn leaf_function_has_unit_complexity() {
        let art = parse_source("g.ts", b"export function g() { return 1; }");
        let sym = art.symbols.iter().find(|s| s.name == "g").unwrap();
        let c = sym.complexity.as_ref().unwrap();
        assert_eq!(c.cyclomatic, 1);
        assert_eq!(c.cognitive, 1);
    }

    #[test]
    fn incremental_parse_uses_edit_path_for_small_change() {
        let old = b"export const a = 1;\n";
        let new = b"export const a = 2;\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap();
        let tree = parser.parse(old, None);
        let (_art, used_inc) = parse_source_incremental("a.ts", old, new, tree);
        assert!(used_inc);
    }
}
