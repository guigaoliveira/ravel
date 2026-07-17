use crate::{
    config::{Config, ConfigError, discover_files},
    model::{
        Complexity, Diagnostic, EdgeKind, Export, ExportBinding, ExportBindingKind, FileArtifact,
        Import, ImportBinding, ImportBindingKind, Span, Symbol, SymbolRef,
        stable_symbol_id_for_kind,
    },
};
use rayon::prelude::*;
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use thiserror::Error;
use tree_sitter::{Node, Parser};

pub const EXTRACTOR_VERSION: &str = "ts-js-structural-v4";
pub const GRAMMAR_VERSION: &str = "tree-sitter-typescript-0.23+javascript-0.25";
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
            let mut artifact = match scan_file_with_path(path, &rel, max_bytes) {
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
    let logical_path = normalize_path(&path.to_string_lossy());
    scan_file_with_path(path, &logical_path, max_bytes)
}

fn scan_file_with_path(
    path: &Path,
    logical_path: &str,
    max_bytes: u64,
) -> std::io::Result<FileArtifact> {
    let metadata = fs::metadata(path)?;
    let path_string = normalize_path(logical_path);
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
    js: Parser,
    ts_ok: bool,
    tsx_ok: bool,
    js_ok: bool,
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
    let mut js = Parser::new();
    let js_ok = js
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .is_ok();
    PooledParsers {
        ts,
        tsx,
        js,
        ts_ok,
        tsx_ok,
        js_ok,
    }
}

thread_local! {
    // One TS + one TSX parser per (rayon worker) thread. `Parser::new()` + `set_language`
    // is paid once per thread instead of once per file — the dominant per-file scan cost.
    static PARSERS: RefCell<Option<PooledParsers>> = const { RefCell::new(None) };
}

pub fn parse_source(path: &str, source: &[u8]) -> FileArtifact {
    let is_tsx = path.ends_with(".tsx");
    let is_js = path.ends_with(".js")
        || path.ends_with(".jsx")
        || path.ends_with(".mjs")
        || path.ends_with(".cjs");
    PARSERS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let parsers = slot.get_or_insert_with(make_parsers);
        let (parser, grammar_ok) = if is_js {
            (&mut parsers.js, parsers.js_ok)
        } else if is_tsx {
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
    // tree-sitter-typescript 0.23 predates TS 5.9 `import defer` and misparses type-only star
    // exports. Replacing only those contextual keywords with equal-width spaces lets the grammar
    // parse the otherwise identical module forms while every span still points at original text.
    let parser_source = if is_typescript_path(path) {
        sanitize_modern_module_keywords(source)
    } else {
        Cow::Borrowed(source)
    };
    let tree = parser.parse(parser_source.as_ref(), None);
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
        let file_owner = EnclosingSymbol {
            id: path.to_owned(),
            qualified_name: path.to_owned(),
            member_owner: None,
            lexical_bindings: BTreeSet::new(),
            active_scope: None,
            qualifies_children: false,
        };
        extract_node(
            tree.root_node(),
            path,
            source,
            &mut symbols,
            &mut imports,
            &mut exports,
            &mut symbol_refs,
            &mut None,
            Some(&file_owner),
            0,
            false,
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

fn is_typescript_path(path: &str) -> bool {
    path.ends_with(".ts")
        || path.ends_with(".tsx")
        || path.ends_with(".mts")
        || path.ends_with(".cts")
}

fn sanitize_modern_module_keywords(source: &[u8]) -> Cow<'_, [u8]> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TokenKind {
        Import,
        Defer,
        Export,
        Type,
        Star,
        Other,
    }
    #[derive(Clone, Copy)]
    struct Token {
        kind: TokenKind,
        start: usize,
        end: usize,
        statement_start: bool,
    }

    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut statement_start = true;
    let mut brace_depth = 0usize;
    while index < source.len() {
        let byte = source[index];
        if byte.is_ascii_whitespace() {
            if byte == b'\n' && brace_depth == 0 {
                statement_start = true;
            }
            index += 1;
            continue;
        }
        if source.get(index..index + 2) == Some(b"//") {
            index += 2;
            while index < source.len() && source[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if source.get(index..index + 2) == Some(b"/*") {
            index += 2;
            while index < source.len() {
                if source[index] == b'\n' && brace_depth == 0 {
                    statement_start = true;
                }
                if source.get(index..index + 2) == Some(b"*/") {
                    index += 2;
                    break;
                }
                index += 1;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            let quote = byte;
            index += 1;
            let mut escaped = false;
            while index < source.len() {
                let current = source[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if current == b'\\' {
                    escaped = true;
                } else if current == quote {
                    break;
                }
            }
            statement_start = false;
            continue;
        }
        let start = index;
        let kind = if byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') {
            index += 1;
            while source
                .get(index)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
            {
                index += 1;
            }
            match &source[start..index] {
                b"import" => TokenKind::Import,
                b"defer" => TokenKind::Defer,
                b"export" => TokenKind::Export,
                b"type" => TokenKind::Type,
                _ => TokenKind::Other,
            }
        } else {
            index += 1;
            match byte {
                b'*' => TokenKind::Star,
                b'{' => {
                    brace_depth += 1;
                    TokenKind::Other
                }
                b'}' => {
                    brace_depth = brace_depth.saturating_sub(1);
                    TokenKind::Other
                }
                b';' => {
                    statement_start = true;
                    TokenKind::Other
                }
                _ => TokenKind::Other,
            }
        };
        let token_starts_statement = statement_start;
        if byte != b';' {
            statement_start = false;
        }
        tokens.push(Token {
            kind,
            start,
            end: index,
            statement_start: token_starts_statement,
        });
    }

    let mut sanitized: Option<Vec<u8>> = None;
    for window in tokens.windows(3) {
        let replacement = match (window[0].kind, window[1].kind, window[2].kind) {
            (TokenKind::Import, TokenKind::Defer, TokenKind::Star) if window[0].statement_start => {
                Some(window[1])
            }
            (TokenKind::Export, TokenKind::Type, TokenKind::Star) if window[0].statement_start => {
                Some(window[1])
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            sanitized.get_or_insert_with(|| source.to_vec())[replacement.start..replacement.end]
                .fill(b' ');
        }
    }
    sanitized.map_or(Cow::Borrowed(source), Cow::Owned)
}

#[derive(Debug, Clone)]
struct EnclosingSymbol {
    id: String,
    qualified_name: String,
    /// Nearest class/interface/namespace used to qualify member declarations and `this.x`.
    member_owner: Option<String>,
    /// Bare parameter names shadow imports/workspace globals. They are intentionally not graph
    /// targets because calling a callback parameter is dynamic dispatch.
    lexical_bindings: BTreeSet<String>,
    /// Innermost lexical block; declarations use it for position-aware resolution.
    active_scope: Option<Span>,
    /// File/callback scopes own references but must not prefix top-level declaration names.
    qualifies_children: bool,
}

#[allow(clippy::too_many_arguments)]
fn extract_node(
    node: Node<'_>,
    path: &str,
    source: &[u8],
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    exports: &mut Vec<Export>,
    refs: &mut Vec<SymbolRef>,
    complexity: &mut Option<(u32, u32)>,
    enclosing: Option<&EnclosingSymbol>,
    nesting: u32,
    exported: bool,
) {
    let kind = node.kind();

    // Anonymous callbacks do not need searchable symbol nodes, but their parameters still form a
    // lexical scope. Keep references attributed to the nearest declaration/file while preventing
    // a callback parameter from resolving to an unrelated workspace symbol.
    if matches!(
        kind,
        "arrow_function" | "function_expression" | "generator_function"
    ) && enclosing.is_some()
    {
        let mut callback_owner = enclosing.cloned().expect("checked above");
        callback_owner
            .lexical_bindings
            .extend(collect_parameter_bindings(node, source));
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            extract_node(
                child,
                path,
                source,
                symbols,
                imports,
                exports,
                refs,
                complexity,
                Some(&callback_owner),
                nesting,
                false,
            );
        }
        return;
    }

    let scoped_owner = matches!(
        kind,
        "statement_block" | "catch_clause" | "for_statement" | "for_in_statement" | "switch_body"
    )
    .then(|| {
        enclosing.cloned().map(|mut owner| {
            owner.active_scope = Some(span(node));
            owner
                .lexical_bindings
                .extend(collect_scope_bindings(node, source));
            owner
        })
    })
    .flatten();
    let enclosing = scoped_owner.as_ref().or(enclosing);

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
        if let Some(import) = extract_import(node, source) {
            imports.push(import);
        }
        return;
    }

    if kind == "export_statement" {
        exports.push(extract_export(node, source));
        let declaration = node.child_by_field_name("declaration");
        let declaration_id = declaration.map(|n| n.id());
        let symbols_before_declaration = symbols.len();
        if let Some(declaration) = declaration {
            extract_node(
                declaration,
                path,
                source,
                symbols,
                imports,
                exports,
                refs,
                complexity,
                enclosing,
                next_nesting,
                true,
            );
        }
        let anonymous_default = declaration.is_none().then(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).find(|child| {
                matches!(
                    child.kind(),
                    "arrow_function"
                        | "function_expression"
                        | "generator_function"
                        | "class"
                        | "class_expression"
                )
            })
        });
        let anonymous_id = anonymous_default.flatten().map(|expression| {
            let symbol_kind = if matches!(expression.kind(), "class" | "class_expression") {
                "class"
            } else {
                "function"
            };
            let qualified_name = "default".to_owned();
            let id = stable_symbol_id_for_kind(path, &qualified_name, symbol_kind, None);
            let mut lexical_bindings = enclosing
                .map(|owner| owner.lexical_bindings.clone())
                .unwrap_or_default();
            lexical_bindings.extend(collect_parameter_bindings(expression, source));
            lexical_bindings.extend(collect_type_parameter_bindings(expression, source));
            let owner = EnclosingSymbol {
                id: id.clone(),
                qualified_name: qualified_name.clone(),
                member_owner: (symbol_kind == "class").then_some(qualified_name.clone()),
                lexical_bindings,
                active_scope: enclosing.and_then(|owner| owner.active_scope),
                qualifies_children: true,
            };
            let symbol_index = symbols.len();
            symbols.push(Symbol {
                id,
                name: "default".into(),
                qualified_name,
                kind: Arc::from(symbol_kind),
                span: span(expression),
                exported: true,
                complexity: None,
                scope: None,
            });
            let mut declaration_comp = (symbol_kind == "function").then_some((1, 0));
            let mut cursor = expression.walk();
            for child in expression.named_children(&mut cursor) {
                extract_node(
                    child,
                    path,
                    source,
                    symbols,
                    imports,
                    exports,
                    refs,
                    &mut declaration_comp,
                    Some(&owner),
                    next_nesting,
                    false,
                );
            }
            if let Some((cyclomatic, cognitive)) = declaration_comp {
                symbols[symbol_index].complexity = Some(Complexity {
                    cyclomatic,
                    cognitive: cognitive.max(1),
                });
            }
            expression.id()
        });
        let exported_owner =
            symbols
                .get(symbols_before_declaration)
                .map(|symbol| EnclosingSymbol {
                    id: symbol.id.clone(),
                    qualified_name: symbol.qualified_name.clone(),
                    member_owner: Some(symbol.qualified_name.clone()),
                    lexical_bindings: enclosing
                        .map(|owner| owner.lexical_bindings.clone())
                        .unwrap_or_default(),
                    active_scope: enclosing.and_then(|owner| owner.active_scope),
                    qualifies_children: true,
                });
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            // Source strings and export clauses contain names/bindings, not declarations.
            if declaration_id == Some(child.id())
                || anonymous_id == Some(child.id())
                || matches!(
                    child.kind(),
                    "string" | "export_clause" | "namespace_export"
                )
            {
                continue;
            }
            extract_node(
                child,
                path,
                source,
                symbols,
                imports,
                exports,
                refs,
                complexity,
                exported_owner.as_ref().or(enclosing),
                next_nesting,
                false,
            );
        }
        return;
    }

    if kind == "assignment_expression" {
        if node_text(node, source).trim_start().starts_with("using ") {
            let left = node.child_by_field_name("left");
            let right = node.child_by_field_name("right");
            let mut names = Vec::new();
            if let Some(left) = left {
                collect_binding_names(left, source, &mut names);
            }
            for (name, _) in names {
                let owner_name = enclosing
                    .filter(|owner| owner.qualifies_children)
                    .map(|owner| owner.qualified_name.as_str());
                let qualified_name = owner_name
                    .map(|owner| format!("{owner}.{name}"))
                    .unwrap_or_else(|| name.clone());
                let declaration_scope = enclosing.and_then(|owner| owner.active_scope);
                let id =
                    stable_symbol_id_for_kind(path, &qualified_name, "variable", declaration_scope);
                let owner = EnclosingSymbol {
                    id: id.clone(),
                    qualified_name: qualified_name.clone(),
                    member_owner: enclosing.and_then(|owner| owner.member_owner.clone()),
                    lexical_bindings: enclosing
                        .map(|owner| owner.lexical_bindings.clone())
                        .unwrap_or_default(),
                    active_scope: declaration_scope,
                    qualifies_children: true,
                };
                symbols.push(Symbol {
                    id,
                    name,
                    qualified_name,
                    kind: Arc::from("variable"),
                    span: span(node),
                    exported: false,
                    complexity: None,
                    scope: declaration_scope,
                });
                if let Some(right) = right {
                    extract_node(
                        right,
                        path,
                        source,
                        symbols,
                        imports,
                        exports,
                        refs,
                        complexity,
                        Some(&owner),
                        next_nesting,
                        false,
                    );
                }
            }
            return;
        }
        if let Some(export) = extract_commonjs_export(node, source) {
            exports.push(export);
        }
    }

    if kind == "call_expression" {
        if let Some(specifier) =
            require_specifier(node, source).or_else(|| dynamic_import_specifier(node, source))
        {
            imports.push(Import {
                specifier,
                type_only: false,
                span: span(node),
                bindings: Vec::new(),
            });
        }
    }

    if let Some(from) = enclosing {
        match kind {
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    if let Some(to) = expression_name(callee, source) {
                        push_ref(refs, from, to, EdgeKind::Calls, callee);
                    }
                }
            }
            "new_expression" => {
                if let Some(constructor) = node.child_by_field_name("constructor") {
                    if let Some(to) = expression_name(constructor, source) {
                        push_ref(refs, from, to, EdgeKind::Instantiates, constructor);
                    }
                }
            }
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(name) = node.child_by_field_name("name")
                    && let Some(to) = expression_name(name, source)
                    && to.chars().next().is_some_and(char::is_uppercase)
                {
                    push_ref(refs, from, to, EdgeKind::References, name);
                }
            }
            "decorator" => {
                if let Some(target) = first_expression_name(node, source) {
                    push_ref(refs, from, target, EdgeKind::Decorates, node);
                }
                // Do not misclassify the outer `@Decorator()` as an ordinary call, but calls in
                // decorator arguments (often validation callbacks) are real consumers.
                let mut cursor = node.walk();
                for expression in node.named_children(&mut cursor) {
                    if let Some(arguments) = expression.child_by_field_name("arguments") {
                        let mut arguments_cursor = arguments.walk();
                        for argument in arguments.named_children(&mut arguments_cursor) {
                            extract_node(
                                argument,
                                path,
                                source,
                                symbols,
                                imports,
                                exports,
                                refs,
                                complexity,
                                Some(from),
                                next_nesting,
                                false,
                            );
                        }
                    }
                }
                return;
            }
            "extends_clause" | "extends_type_clause" => {
                push_heritage(node, source, from, EdgeKind::Extends, refs);
                return;
            }
            "implements_clause" => {
                push_heritage(node, source, from, EdgeKind::Implements, refs);
                return;
            }
            "type_annotation" => {
                push_type_references(node, source, from, refs, imports);
                return;
            }
            "type_query" | "import_type" => {
                push_type_references(node, source, from, refs, imports);
                return;
            }
            "type_identifier" | "nested_type_identifier" => {
                push_ref(
                    refs,
                    from,
                    node_text(node, source).to_owned(),
                    EdgeKind::TypeOf,
                    node,
                );
                return;
            }
            _ => {}
        }
    }

    if matches!(
        kind,
        "lexical_declaration" | "variable_declaration" | "using_declaration"
    ) {
        extract_variable_declaration(
            node,
            path,
            source,
            symbols,
            imports,
            exports,
            refs,
            complexity,
            enclosing,
            next_nesting,
            exported,
        );
        return;
    }

    if kind == "enum_body" {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(child.kind(), "property_identifier" | "string" | "number") {
                if let (Some(owner), Some(name)) = (enclosing, static_name(child, source)) {
                    let qualified_name = format!("{}.{}", owner.qualified_name, name);
                    symbols.push(Symbol {
                        id: stable_symbol_id_for_kind(path, &qualified_name, "enum_member", None),
                        name,
                        qualified_name,
                        kind: Arc::from("enum_member"),
                        span: span(child),
                        exported: false,
                        complexity: None,
                        scope: None,
                    });
                }
            } else {
                extract_node(
                    child,
                    path,
                    source,
                    symbols,
                    imports,
                    exports,
                    refs,
                    complexity,
                    enclosing,
                    next_nesting,
                    false,
                );
            }
        }
        return;
    }

    if matches!(kind, "required_parameter" | "optional_parameter")
        && is_parameter_property(node, source)
        && enclosing
            .and_then(|owner| owner.member_owner.as_ref())
            .is_some()
    {
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("pattern"));
        if let (Some(owner), Some(name_node), Some(name)) = (
            enclosing,
            name_node,
            name_node.and_then(|node| static_name(node, source)),
        ) {
            let container = owner.member_owner.as_deref().expect("checked above");
            let qualified_name = format!("{container}.{name}");
            let property_owner = EnclosingSymbol {
                id: stable_symbol_id_for_kind(path, &qualified_name, "property", None),
                qualified_name: qualified_name.clone(),
                member_owner: Some(container.to_owned()),
                lexical_bindings: enclosing
                    .map(|owner| owner.lexical_bindings.clone())
                    .unwrap_or_default(),
                active_scope: enclosing.and_then(|owner| owner.active_scope),
                qualifies_children: true,
            };
            symbols.push(Symbol {
                id: property_owner.id.clone(),
                name,
                qualified_name,
                kind: Arc::from("property"),
                span: span(node),
                exported: false,
                complexity: None,
                scope: None,
            });
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.id() == name_node.id() {
                    continue;
                }
                extract_node(
                    child,
                    path,
                    source,
                    symbols,
                    imports,
                    exports,
                    refs,
                    complexity,
                    Some(&property_owner),
                    next_nesting,
                    false,
                );
            }
            return;
        }
    }

    let declaration_kind = matches!(
        kind,
        "class_declaration"
            | "abstract_class_declaration"
            | "function_declaration"
            | "function_signature"
            | "generator_function_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "internal_module"
    );
    let member_kind = matches!(
        kind,
        "method_definition"
            | "method_signature"
            | "abstract_method_signature"
            | "public_field_definition"
            | "field_definition"
            | "property_signature"
            | "enum_assignment"
    );
    if declaration_kind
        || (member_kind
            && enclosing
                .and_then(|owner| owner.member_owner.as_ref())
                .is_some())
    {
        let name_node = node.child_by_field_name("name");
        let static_name = name_node
            .and_then(|name| static_name(name, source))
            .or_else(|| exported.then(|| "default".to_owned()));
        if let Some(name) = static_name {
            let is_container = matches!(
                kind,
                "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "internal_module"
            );
            let owner_name = if member_kind {
                enclosing.and_then(|owner| owner.member_owner.as_deref())
            } else {
                enclosing
                    .filter(|owner| owner.qualifies_children)
                    .map(|owner| owner.qualified_name.as_str())
            };
            let qualified_name = owner_name
                .map(|owner| format!("{owner}.{name}"))
                .unwrap_or_else(|| name.clone());
            let declaration_scope = (!member_kind
                && enclosing.and_then(|owner| owner.active_scope).is_some()
                && matches!(
                    kind,
                    "class_declaration"
                        | "abstract_class_declaration"
                        | "function_declaration"
                        | "generator_function_declaration"
                ))
            .then(|| enclosing.and_then(|owner| owner.active_scope))
            .flatten();
            let symbol_kind = shared_symbol_kind(kind);
            let id = stable_symbol_id_for_kind(
                path,
                &qualified_name,
                symbol_kind.as_ref(),
                declaration_scope,
            );
            let member_owner = if is_container {
                Some(qualified_name.clone())
            } else {
                enclosing.and_then(|owner| owner.member_owner.clone())
            };
            let executable = matches!(
                kind,
                "class_declaration"
                    | "abstract_class_declaration"
                    | "function_declaration"
                    | "generator_function_declaration"
                    | "method_definition"
            );
            let mut lexical_bindings = enclosing
                .map(|owner| owner.lexical_bindings.clone())
                .unwrap_or_default();
            if executable {
                lexical_bindings.extend(collect_parameter_bindings(node, source));
            }
            lexical_bindings.extend(collect_type_parameter_bindings(node, source));
            let owner = EnclosingSymbol {
                id: id.clone(),
                qualified_name: qualified_name.clone(),
                member_owner,
                lexical_bindings,
                active_scope: enclosing.and_then(|owner| owner.active_scope),
                qualifies_children: true,
            };
            let idx = symbols.len();
            symbols.push(Symbol {
                id,
                name,
                qualified_name,
                kind: symbol_kind,
                span: span(node),
                exported,
                complexity: None,
                scope: declaration_scope,
            });

            let parent_comp = complexity.take();
            let mut declaration_comp = executable.then_some((1u32, 0u32));
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                // Declaration name is metadata, not a runtime/type reference.
                if name_node.is_some_and(|name_node| name_node.id() == child.id()) {
                    continue;
                }
                if kind == "type_alias_declaration" {
                    push_type_references(child, source, &owner, refs, imports);
                    continue;
                }
                extract_node(
                    child,
                    path,
                    source,
                    symbols,
                    imports,
                    exports,
                    refs,
                    &mut declaration_comp,
                    Some(&owner),
                    next_nesting,
                    false,
                );
            }
            if let Some((cyc, cog)) = declaration_comp {
                symbols[idx].complexity = Some(Complexity {
                    cyclomatic: cyc,
                    cognitive: if cog == 0 { 1 } else { cog },
                });
            }
            *complexity = parent_comp;
            return;
        }
        // Dynamic/computed members are intentionally not fabricated as searchable symbols, but
        // their bodies still belong to the surrounding declaration.
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract_node(
            child,
            path,
            source,
            symbols,
            imports,
            exports,
            refs,
            complexity,
            enclosing,
            next_nesting,
            false,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_variable_declaration(
    declaration: Node<'_>,
    path: &str,
    source: &[u8],
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<Import>,
    exports: &mut Vec<Export>,
    refs: &mut Vec<SymbolRef>,
    complexity: &mut Option<(u32, u32)>,
    enclosing: Option<&EnclosingSymbol>,
    nesting: u32,
    exported: bool,
) {
    let declaration_text = node_text(declaration, source).trim_start();
    let binding_kind = if declaration_text.starts_with("const ") {
        "constant"
    } else {
        "variable"
    };
    let mut cursor = declaration.walk();
    for declarator in declaration
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "variable_declarator")
    {
        let Some(pattern) = declarator.child_by_field_name("name") else {
            continue;
        };
        let mut names = Vec::new();
        collect_binding_names(pattern, source, &mut names);
        let value = declarator.child_by_field_name("value");
        let commonjs_specifier = value.and_then(|value| require_specifier(value, source));
        if let Some(specifier) = commonjs_specifier.as_ref() {
            imports.push(Import {
                specifier: specifier.clone(),
                type_only: false,
                span: span(declarator),
                bindings: commonjs_import_bindings(pattern, source),
            });
        } else if let Some((specifier, imported)) =
            value.and_then(|value| commonjs_member_import(value, source))
        {
            let bindings = names
                .first()
                .map(|(local, local_span)| ImportBinding {
                    imported,
                    local: local.clone(),
                    kind: ImportBindingKind::Named,
                    type_only: false,
                    span: *local_span,
                })
                .into_iter()
                .collect();
            imports.push(Import {
                specifier,
                type_only: false,
                span: span(declarator),
                bindings,
            });
        }
        let symbol_kind = match value.map(|node| node.kind()) {
            Some("arrow_function" | "function_expression" | "generator_function") => "function",
            Some("class" | "class_expression") => "class",
            _ => binding_kind,
        };
        for (index, (name, _name_span)) in names.into_iter().enumerate() {
            let owner_name = enclosing
                .filter(|owner| owner.qualifies_children)
                .map(|owner| owner.qualified_name.as_str());
            let qualified_name = owner_name
                .map(|owner| format!("{owner}.{name}"))
                .unwrap_or_else(|| name.clone());
            let declaration_scope = (declaration.kind() != "variable_declaration")
                .then(|| enclosing.and_then(|owner| owner.active_scope))
                .flatten();
            let id =
                stable_symbol_id_for_kind(path, &qualified_name, symbol_kind, declaration_scope);
            let mut lexical_bindings = enclosing
                .map(|owner| owner.lexical_bindings.clone())
                .unwrap_or_default();
            if symbol_kind == "function"
                && let Some(value) = value
            {
                lexical_bindings.extend(collect_parameter_bindings(value, source));
                lexical_bindings.extend(collect_type_parameter_bindings(value, source));
            }
            let owner = EnclosingSymbol {
                id: id.clone(),
                qualified_name: qualified_name.clone(),
                member_owner: if symbol_kind == "class" {
                    Some(qualified_name.clone())
                } else {
                    enclosing.and_then(|owner| owner.member_owner.clone())
                },
                lexical_bindings,
                active_scope: enclosing.and_then(|owner| owner.active_scope),
                qualifies_children: true,
            };
            let symbol_index = symbols.len();
            symbols.push(Symbol {
                id,
                name,
                qualified_name,
                kind: Arc::from(symbol_kind),
                span: span(declarator),
                exported,
                complexity: None,
                scope: declaration_scope,
            });
            // A destructuring initializer belongs to the declaration as a whole. Attribute it to
            // the first static binding only to avoid duplicate graph edges.
            if index == 0 {
                let parent_comp = complexity.take();
                let mut declaration_comp = (symbol_kind == "function").then_some((1u32, 0u32));
                let value_to_walk = commonjs_specifier.is_none().then_some(value).flatten();
                for child in [declarator.child_by_field_name("type"), value_to_walk]
                    .into_iter()
                    .flatten()
                {
                    extract_node(
                        child,
                        path,
                        source,
                        symbols,
                        imports,
                        exports,
                        refs,
                        &mut declaration_comp,
                        Some(&owner),
                        nesting,
                        false,
                    );
                }
                if let Some((cyc, cog)) = declaration_comp {
                    symbols[symbol_index].complexity = Some(Complexity {
                        cyclomatic: cyc,
                        cognitive: if cog == 0 { 1 } else { cog },
                    });
                }
                *complexity = parent_comp;
            }
        }
    }
}

fn extract_import(node: Node<'_>, source: &[u8]) -> Option<Import> {
    let specifier = node
        .child_by_field_name("source")
        .map(|source_node| unquote(node_text(source_node, source)))
        .or_else(|| last_string(node, source))?;
    let statement_type_only = has_leading_keywords(node_text(node, source), &["import", "type"]);
    let mut bindings = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_clause" => {
                collect_import_clause(child, source, statement_type_only, &mut bindings)
            }
            "import_require_clause" => {
                if let Some(local) = first_direct_identifier(child, source) {
                    bindings.push(ImportBinding {
                        imported: "export=".into(),
                        local,
                        kind: ImportBindingKind::ImportEquals,
                        type_only: statement_type_only,
                        span: span(child),
                    });
                }
            }
            _ => {}
        }
    }
    Some(Import {
        specifier,
        type_only: statement_type_only,
        span: span(node),
        bindings,
    })
}

fn collect_import_clause(
    clause: Node<'_>,
    source: &[u8],
    statement_type_only: bool,
    bindings: &mut Vec<ImportBinding>,
) {
    let mut cursor = clause.walk();
    for child in clause.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => bindings.push(ImportBinding {
                imported: "default".into(),
                local: node_text(child, source).to_owned(),
                kind: ImportBindingKind::Default,
                type_only: statement_type_only,
                span: span(child),
            }),
            "namespace_import" => {
                if let Some(local) = first_direct_identifier(child, source) {
                    bindings.push(ImportBinding {
                        imported: "*".into(),
                        local,
                        kind: ImportBindingKind::Namespace,
                        type_only: statement_type_only,
                        span: span(child),
                    });
                }
            }
            "named_imports" => {
                let mut imports_cursor = child.walk();
                for specifier in child
                    .named_children(&mut imports_cursor)
                    .filter(|node| node.kind() == "import_specifier")
                {
                    let Some(imported_node) = specifier.child_by_field_name("name") else {
                        continue;
                    };
                    let imported = unquote(node_text(imported_node, source));
                    let local = specifier
                        .child_by_field_name("alias")
                        .map(|node| unquote(node_text(node, source)))
                        .unwrap_or_else(|| imported.clone());
                    let specifier_type_only = node_text(specifier, source)
                        .trim_start()
                        .starts_with("type ");
                    bindings.push(ImportBinding {
                        imported,
                        local,
                        kind: ImportBindingKind::Named,
                        type_only: statement_type_only || specifier_type_only,
                        span: span(specifier),
                    });
                }
            }
            _ => {}
        }
    }
}

fn extract_export(node: Node<'_>, source: &[u8]) -> Export {
    let text = node_text(node, source).trim_start();
    let type_only = has_leading_keywords(text, &["export", "type"]);
    let specifier = node
        .child_by_field_name("source")
        .map(|source_node| unquote(node_text(source_node, source)))
        .or_else(|| last_string(node, source));
    let mut bindings = Vec::new();
    if let Some(declaration) = node.child_by_field_name("declaration") {
        let names = declared_names(declaration, source);
        let is_default = text.starts_with("export default");
        for (name, name_span) in names {
            bindings.push(ExportBinding {
                local: name.clone(),
                exported: if is_default { "default".into() } else { name },
                kind: if is_default {
                    ExportBindingKind::Default
                } else {
                    ExportBindingKind::Declaration
                },
                type_only,
                span: name_span,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "export_clause" => {
                let mut clause_cursor = child.walk();
                for export_specifier in child
                    .named_children(&mut clause_cursor)
                    .filter(|node| node.kind() == "export_specifier")
                {
                    let Some(local_node) = export_specifier.child_by_field_name("name") else {
                        continue;
                    };
                    let local = unquote(node_text(local_node, source));
                    let exported = export_specifier
                        .child_by_field_name("alias")
                        .map(|node| unquote(node_text(node, source)))
                        .unwrap_or_else(|| local.clone());
                    let binding_type_only = node_text(export_specifier, source)
                        .trim_start()
                        .starts_with("type ");
                    bindings.push(ExportBinding {
                        local,
                        exported,
                        kind: ExportBindingKind::Named,
                        type_only: type_only || binding_type_only,
                        span: span(export_specifier),
                    });
                }
            }
            "namespace_export" => {
                let exported = first_static_child_name(child, source).unwrap_or_else(|| "*".into());
                bindings.push(ExportBinding {
                    local: "*".into(),
                    exported,
                    kind: ExportBindingKind::Namespace,
                    type_only,
                    span: span(child),
                });
            }
            _ => {}
        }
    }
    if bindings.is_empty() && specifier.is_some() && text.starts_with("export *") {
        bindings.push(ExportBinding {
            local: "*".into(),
            exported: "*".into(),
            kind: ExportBindingKind::Star,
            type_only,
            span: span(node),
        });
    }
    if bindings.is_empty() && text.starts_with("export default") {
        let local = node
            .child_by_field_name("value")
            .and_then(|value| expression_name(value, source))
            .unwrap_or_else(|| "default".into());
        bindings.push(ExportBinding {
            local,
            exported: "default".into(),
            kind: ExportBindingKind::Default,
            type_only,
            span: span(node),
        });
    }
    Export {
        name: bindings.first().map(|binding| binding.exported.clone()),
        specifier,
        type_only,
        span: span(node),
        bindings,
    }
}

fn require_specifier(call: Node<'_>, source: &[u8]) -> Option<String> {
    if call.kind() != "call_expression"
        || call
            .child_by_field_name("function")
            .and_then(|function| expression_name(function, source))
            .as_deref()
            != Some("require")
    {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let mut named = arguments.named_children(&mut cursor);
    let first = named.next()?;
    if named.next().is_some() || first.kind() != "string" {
        return None;
    }
    Some(unquote(node_text(first, source)))
}

fn dynamic_import_specifier(call: Node<'_>, source: &[u8]) -> Option<String> {
    if call.kind() != "call_expression"
        || call
            .child_by_field_name("function")
            .is_none_or(|function| function.kind() != "import")
    {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let mut named = arguments.named_children(&mut cursor);
    let first = named.next()?;
    if named.next().is_some() || first.kind() != "string" {
        return None;
    }
    Some(unquote(node_text(first, source)))
}

fn commonjs_member_import(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "member_expression" && node.kind() != "subscript_expression" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    let property = node
        .child_by_field_name("property")
        .or_else(|| node.child_by_field_name("index"))?;
    Some((
        require_specifier(object, source)?,
        static_name(property, source)?,
    ))
}

fn commonjs_import_bindings(pattern: Node<'_>, source: &[u8]) -> Vec<ImportBinding> {
    match pattern.kind() {
        "identifier" => vec![ImportBinding {
            imported: "*".into(),
            local: node_text(pattern, source).to_owned(),
            kind: ImportBindingKind::Namespace,
            type_only: false,
            span: span(pattern),
        }],
        "object_pattern" => {
            let mut bindings = Vec::new();
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                match child.kind() {
                    "shorthand_property_identifier_pattern" => {
                        let name = node_text(child, source).to_owned();
                        bindings.push(ImportBinding {
                            imported: name.clone(),
                            local: name,
                            kind: ImportBindingKind::Named,
                            type_only: false,
                            span: span(child),
                        });
                    }
                    "pair_pattern" => {
                        let imported = child
                            .child_by_field_name("key")
                            .and_then(|key| static_name(key, source));
                        let local = child.child_by_field_name("value").and_then(|value| {
                            let mut names = Vec::new();
                            collect_binding_names(value, source, &mut names);
                            names.into_iter().next()
                        });
                        if let (Some(imported), Some((local, local_span))) = (imported, local) {
                            bindings.push(ImportBinding {
                                imported,
                                local,
                                kind: ImportBindingKind::Named,
                                type_only: false,
                                span: local_span,
                            });
                        }
                    }
                    _ => {}
                }
            }
            bindings
        }
        _ => Vec::new(),
    }
}

fn extract_commonjs_export(node: Node<'_>, source: &[u8]) -> Option<Export> {
    let left = node.child_by_field_name("left")?;
    let target = expression_name(left, source)?;
    let (exported, is_default) = if target == "module.exports" {
        ("default".to_owned(), true)
    } else if let Some(name) = target.strip_prefix("module.exports.") {
        (name.to_owned(), false)
    } else {
        let name = target.strip_prefix("exports.")?;
        (name.to_owned(), false)
    };
    let right = node.child_by_field_name("right");
    let mut bindings = Vec::new();
    if is_default && right.is_some_and(|value| value.kind() == "object") {
        let object = right.expect("checked above");
        let mut cursor = object.walk();
        for property in object.named_children(&mut cursor) {
            let pair = match property.kind() {
                "shorthand_property_identifier" => {
                    let name = node_text(property, source).to_owned();
                    Some((name.clone(), name, span(property)))
                }
                "pair" => {
                    let key = property
                        .child_by_field_name("key")
                        .and_then(|key| static_name(key, source));
                    let value = property
                        .child_by_field_name("value")
                        .and_then(|value| expression_name(value, source));
                    key.zip(value)
                        .map(|(key, value)| (key, value, span(property)))
                }
                _ => None,
            };
            if let Some((exported, local, binding_span)) = pair {
                bindings.push(ExportBinding {
                    local,
                    exported,
                    kind: ExportBindingKind::CommonJs,
                    type_only: false,
                    span: binding_span,
                });
            }
        }
    }
    let local = right
        .and_then(|right| expression_name(right, source))
        .unwrap_or_else(|| exported.clone());
    if bindings.is_empty() {
        bindings.push(ExportBinding {
            local,
            exported: exported.clone(),
            kind: if is_default {
                ExportBindingKind::Default
            } else {
                ExportBindingKind::CommonJs
            },
            type_only: false,
            span: span(left),
        });
    }
    Some(Export {
        name: bindings.first().map(|binding| binding.exported.clone()),
        specifier: None,
        type_only: false,
        span: span(node),
        bindings,
    })
}

fn is_parameter_property(node: Node<'_>, source: &[u8]) -> bool {
    let text = node_text(node, source).trim_start();
    text.starts_with("public ")
        || text.starts_with("private ")
        || text.starts_with("protected ")
        || text.starts_with("readonly ")
        || text.starts_with("override ")
}

fn declared_names(node: Node<'_>, source: &[u8]) -> Vec<(String, Span)> {
    if matches!(
        node.kind(),
        "lexical_declaration" | "variable_declaration" | "using_declaration"
    ) {
        let mut result = Vec::new();
        let mut cursor = node.walk();
        for declarator in node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
        {
            if let Some(name) = declarator.child_by_field_name("name") {
                collect_binding_names(name, source, &mut result);
            }
        }
        return result;
    }
    node.child_by_field_name("name")
        .and_then(|name| static_name(name, source).map(|text| (text, span(name))))
        .into_iter()
        .collect()
}

fn collect_binding_names(node: Node<'_>, source: &[u8], result: &mut Vec<(String, Span)>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            result.push((node_text(node, source).to_owned(), span(node)));
        }
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                collect_binding_names(value, source, result);
            }
        }
        "assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_binding_names(left, source, result);
            }
        }
        "rest_pattern" => {
            let mut cursor = node.walk();
            if let Some(child) = node.named_children(&mut cursor).next() {
                collect_binding_names(child, source, result);
            }
        }
        "object_pattern" | "array_pattern" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_binding_names(child, source, result);
            }
        }
        _ => {}
    }
}

fn collect_parameter_bindings(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let parameter_root = node
        .child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("parameter"));
    let Some(parameter_root) = parameter_root else {
        return BTreeSet::new();
    };
    let mut names = Vec::new();
    let mut cursor = parameter_root.walk();
    for parameter in parameter_root.named_children(&mut cursor) {
        let pattern = parameter
            .child_by_field_name("pattern")
            .or_else(|| parameter.child_by_field_name("name"))
            .unwrap_or(parameter);
        collect_binding_names(pattern, source, &mut names);
    }
    names.into_iter().map(|(name, _)| name).collect()
}

fn collect_type_parameter_bindings(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let Some(parameters) = node.child_by_field_name("type_parameters") else {
        return BTreeSet::new();
    };
    let mut result = BTreeSet::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if let Some(name) = parameter
            .child_by_field_name("name")
            .or_else(|| (parameter.kind() == "type_identifier").then_some(parameter))
        {
            result.insert(node_text(name, source).to_owned());
        }
    }
    result
}

fn collect_scope_bindings(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut add_pattern = |pattern: Node<'_>| {
        let mut names = Vec::new();
        collect_binding_names(pattern, source, &mut names);
        result.extend(names.into_iter().map(|(name, _)| name));
    };

    if node.kind() == "catch_clause"
        && let Some(parameter) = node.child_by_field_name("parameter")
    {
        add_pattern(parameter);
    }
    result
}

fn push_ref(
    refs: &mut Vec<SymbolRef>,
    from: &EnclosingSymbol,
    to: String,
    kind: EdgeKind,
    site: Node<'_>,
) {
    let head = to.split('.').next().unwrap_or(&to);
    if from.lexical_bindings.contains(head) {
        return;
    }
    refs.push(SymbolRef {
        from_id: from.id.clone(),
        to,
        kind,
        span: span(site),
    });
}

fn expression_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "this" | "super" | "private_property_identifier" => {
            Some(node_text(node, source).to_owned())
        }
        "member_expression" | "nested_identifier" | "nested_type_identifier" => {
            let object = node
                .child_by_field_name("object")
                .or_else(|| node.child_by_field_name("module"))
                .or_else(|| first_named_child(node));
            let property = node
                .child_by_field_name("property")
                .or_else(|| node.child_by_field_name("name"))
                .or_else(|| last_named_child(node));
            match (object, property) {
                (Some(object), Some(property)) if object.id() != property.id() => {
                    let left = expression_name(object, source)?;
                    let right = static_name(property, source)?;
                    Some(format!("{left}.{right}"))
                }
                _ => None,
            }
        }
        "parenthesized_expression"
        | "non_null_expression"
        | "as_expression"
        | "satisfies_expression"
        | "type_assertion" => {
            first_named_child(node).and_then(|child| expression_name(child, source))
        }
        "instantiation_expression" => node
            .child_by_field_name("function")
            .or_else(|| first_named_child(node))
            .and_then(|child| expression_name(child, source)),
        "subscript_expression" => {
            let object = node.child_by_field_name("object")?;
            let index = node.child_by_field_name("index")?;
            let left = expression_name(object, source)?;
            let right = static_name(index, source)?;
            Some(format!("{left}.{right}"))
        }
        _ => None,
    }
}

fn first_expression_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call_expression" {
            return child
                .child_by_field_name("function")
                .and_then(|callee| expression_name(callee, source));
        }
        if let Some(name) = expression_name(child, source) {
            return Some(name);
        }
    }
    None
}

fn push_heritage(
    clause: Node<'_>,
    source: &[u8],
    from: &EnclosingSymbol,
    kind: EdgeKind,
    refs: &mut Vec<SymbolRef>,
) {
    let mut cursor = clause.walk();
    for node in clause.named_children(&mut cursor) {
        let target_node = if node.kind() == "generic_type" {
            node.child_by_field_name("name")
        } else if node.kind() == "type_arguments" {
            None
        } else {
            Some(node)
        };
        let Some(target_node) = target_node else {
            continue;
        };
        let to = match target_node.kind() {
            "identifier" | "type_identifier" | "nested_identifier" | "nested_type_identifier" => {
                Some(node_text(target_node, source).to_owned())
            }
            _ => expression_name(target_node, source),
        };
        if let Some(to) = to {
            push_ref(refs, from, to, kind.clone(), target_node);
        }
    }
}

fn push_type_references(
    annotation: Node<'_>,
    source: &[u8],
    from: &EnclosingSymbol,
    refs: &mut Vec<SymbolRef>,
    imports: &mut Vec<Import>,
) {
    let mut stack = vec![annotation];
    while let Some(node) = stack.pop() {
        if node.kind() == "type_query" {
            if let Some((name, target)) = widest_reference_name(node, source) {
                push_ref(refs, from, name, EdgeKind::References, target);
            }
            continue;
        }
        if node.kind() == "import_type" {
            if let Some(specifier) = last_string(node, source) {
                let target = widest_type_name(node, source);
                let bindings = target
                    .as_ref()
                    .map(|(name, target_node)| {
                        let imported = name.split('.').next().unwrap_or(name).to_owned();
                        vec![ImportBinding {
                            local: imported.clone(),
                            imported,
                            kind: ImportBindingKind::Named,
                            type_only: true,
                            span: span(*target_node),
                        }]
                    })
                    .unwrap_or_default();
                imports.push(Import {
                    specifier,
                    type_only: true,
                    span: span(node),
                    bindings,
                });
                if let Some((name, target_node)) = target {
                    push_ref(refs, from, name, EdgeKind::TypeOf, target_node);
                }
            }
            continue;
        }
        if node.kind() == "member_expression"
            && node_text(node, source).trim_start().starts_with("import(")
            && let Some(specifier) = last_string(node, source)
            && let Some(property) = node.child_by_field_name("property")
            && let Some(imported) = static_name(property, source)
        {
            imports.push(Import {
                specifier,
                type_only: true,
                span: span(node),
                bindings: vec![ImportBinding {
                    imported: imported.clone(),
                    local: imported.clone(),
                    kind: ImportBindingKind::Named,
                    type_only: true,
                    span: span(property),
                }],
            });
            push_ref(refs, from, imported, EdgeKind::TypeOf, property);
            continue;
        }
        if matches!(node.kind(), "type_identifier" | "nested_type_identifier") {
            let to = node_text(node, source).to_owned();
            push_ref(refs, from, to, EdgeKind::TypeOf, node);
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}

fn widest_reference_name<'tree>(root: Node<'tree>, source: &[u8]) -> Option<(String, Node<'tree>)> {
    let mut stack = vec![root];
    let mut best: Option<(String, Node<'tree>)> = None;
    while let Some(node) = stack.pop() {
        if let Some(name) = expression_name(node, source)
            && best
                .as_ref()
                .is_none_or(|(current, _)| name.len() > current.len())
        {
            best = Some((name, node));
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    best
}

fn widest_type_name<'tree>(root: Node<'tree>, source: &[u8]) -> Option<(String, Node<'tree>)> {
    let mut stack = vec![root];
    let mut best: Option<(String, Node<'tree>)> = None;
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "type_identifier" | "nested_type_identifier") {
            let name = node_text(node, source).to_owned();
            if best
                .as_ref()
                .is_none_or(|(current, _)| name.len() > current.len())
            {
                best = Some((name, node));
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    best
}

fn static_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier"
        | "type_identifier"
        | "property_identifier"
        | "private_property_identifier"
        | "shorthand_property_identifier_pattern"
        | "number" => Some(node_text(node, source).to_owned()),
        "string" => Some(unquote(node_text(node, source))),
        "computed_property_name" => first_named_child(node)
            .filter(|child| matches!(child.kind(), "string" | "number"))
            .and_then(|child| static_name(child, source)),
        _ => None,
    }
}

fn first_direct_identifier(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "identifier")
        .map(|child| node_text(child, source).to_owned())
}

fn first_static_child_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| static_name(child, source))
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn last_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).last()
}

fn unquote(text: &str) -> String {
    text.trim_matches(['\'', '"', '`']).to_owned()
}

fn has_leading_keywords(mut text: &str, expected: &[&str]) -> bool {
    for keyword in expected {
        loop {
            text = text.trim_start_matches(char::is_whitespace);
            if let Some(comment) = text.strip_prefix("//") {
                let Some(newline) = comment.find('\n') else {
                    return false;
                };
                text = &comment[newline + 1..];
                continue;
            }
            if let Some(comment) = text.strip_prefix("/*") {
                let Some(end) = comment.find("*/") else {
                    return false;
                };
                text = &comment[end + 2..];
                continue;
            }
            break;
        }
        let Some(rest) = text.strip_prefix(keyword) else {
            return false;
        };
        if rest
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
        {
            return false;
        }
        text = rest;
    }
    true
}

fn is_logical_binary(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| {
        let t = c.kind();
        t == "&&" || t == "||"
    })
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
        "class_declaration" | "abstract_class_declaration" => Arc::clone(&CLASS),
        "function_declaration" | "function_signature" | "generator_function_declaration" => {
            Arc::clone(&FUNCTION)
        }
        "interface_declaration" => Arc::clone(&INTERFACE),
        "type_alias_declaration" => Arc::clone(&TYPE_ALIAS),
        "enum_declaration" => Arc::clone(&ENUM),
        "lexical_declaration" => Arc::clone(&LEXICAL),
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            Arc::from("method")
        }
        "public_field_definition" | "field_definition" | "property_signature" => {
            Arc::from("property")
        }
        "enum_assignment" => Arc::from("enum_member"),
        "internal_module" => Arc::from("namespace"),
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

    fn has_symbol(artifact: &FileArtifact, qualified_name: &str, kind: &str) -> bool {
        artifact
            .symbols
            .iter()
            .any(|symbol| symbol.qualified_name == qualified_name && symbol.kind.as_ref() == kind)
    }

    fn reference_owner<'a>(artifact: &'a FileArtifact, reference: &'a SymbolRef) -> &'a str {
        artifact
            .symbols
            .iter()
            .find(|symbol| symbol.id == reference.from_id)
            .map_or(reference.from_id.as_str(), |symbol| {
                symbol.qualified_name.as_str()
            })
    }

    #[test]
    fn extracts_imports_types_and_declarations() {
        let artifact = parse_source(
            "src/a.ts",
            br#"
import DefaultThing, { type User, Other as Alias } from './user';
import type { OnlyType } from './types';
import * as NS from './namespace';
import Legacy = require('./legacy');
import './side-effect';
export class A {}
export { User as PublicUser } from './user';
export type { OnlyType as PublicType } from './types';
export * from './all';
export * as everything from './all';
"#,
        );
        assert_eq!(artifact.imports[0].specifier, "./user");
        assert!(!artifact.imports[0].type_only);
        assert_eq!(artifact.imports[0].bindings.len(), 3);
        assert!(artifact.imports[0].bindings.iter().any(|binding| {
            binding.imported == "default"
                && binding.local == "DefaultThing"
                && binding.kind == ImportBindingKind::Default
        }));
        assert!(artifact.imports[0].bindings.iter().any(|binding| {
            binding.imported == "User" && binding.local == "User" && binding.type_only
        }));
        assert!(
            artifact.imports[0]
                .bindings
                .iter()
                .any(|binding| binding.imported == "Other" && binding.local == "Alias")
        );
        assert!(artifact.imports[1].type_only);
        assert_eq!(
            artifact.imports[2].bindings[0].kind,
            ImportBindingKind::Namespace
        );
        assert_eq!(
            artifact.imports[3].bindings[0].kind,
            ImportBindingKind::ImportEquals
        );
        assert!(artifact.imports[4].bindings.is_empty());
        assert_eq!(artifact.exports.len(), 5);
        assert_eq!(artifact.symbols.len(), 1);
        assert!(artifact.symbols.iter().any(|symbol| {
            symbol.name == "A" && symbol.exported && symbol.qualified_name == "A"
        }));
        assert!(
            artifact
                .exports
                .iter()
                .flat_map(|export| &export.bindings)
                .any(|binding| binding.local == "User" && binding.exported == "PublicUser")
        );
        assert!(
            artifact
                .exports
                .iter()
                .flat_map(|export| &export.bindings)
                .any(|binding| binding.kind == ExportBindingKind::Star)
        );
        assert_eq!(artifact.source_hash.len(), 64);
    }

    #[test]
    fn extracts_symbol_refs_calls_and_heritage() {
        let src = b"class A extends Base implements Iface { run() { helper(); this.other(); } }\nfunction helper() {}\nfunction other() {}";
        let art = parse_source("a.ts", src);
        let has = |from: &str, to: &str, k: EdgeKind| {
            art.symbol_refs
                .iter()
                .any(|r| reference_owner(&art, r) == from && r.to == to && r.kind == k)
        };
        assert!(has("A", "Base", EdgeKind::Extends), "{:?}", art.symbol_refs);
        assert!(
            has("A", "Iface", EdgeKind::Implements),
            "{:?}",
            art.symbol_refs
        );
        assert!(
            has("A.run", "helper", EdgeKind::Calls),
            "{:?}",
            art.symbol_refs
        );
        assert!(
            has("A.run", "this.other", EdgeKind::Calls),
            "{:?}",
            art.symbol_refs
        );
        assert!(has_symbol(&art, "A.run", "method"));
    }

    #[test]
    fn captures_decorator_callback_and_top_level_calls_without_parameter_shadowing() {
        let art = parse_source(
            "callbacks.ts",
            br#"
import { helper } from './helper';
class Request {
  @ValidateIf(() => helper())
  value!: string;
}
describe('consumer', () => helper());
test((helper) => helper());
"#,
        );
        let helper_calls: Vec<_> = art
            .symbol_refs
            .iter()
            .filter(|reference| reference.to == "helper" && reference.kind == EdgeKind::Calls)
            .collect();
        assert!(
            helper_calls
                .iter()
                .any(|reference| reference_owner(&art, reference) == "Request.value"),
            "{:?}",
            art.symbol_refs
        );
        assert_eq!(
            helper_calls
                .iter()
                .filter(|reference| reference_owner(&art, reference) == "callbacks.ts")
                .count(),
            1,
            "{:?}",
            art.symbol_refs
        );
        assert!(art.symbol_refs.iter().any(|reference| {
            reference_owner(&art, reference) == "Request.value"
                && reference.to == "ValidateIf"
                && reference.kind == EdgeKind::Decorates
        }));
    }

    #[test]
    fn extracts_typescript_symbol_matrix_without_duplicate_export_symbols() {
        let src = br#"
@Controller()
export abstract class Service extends Base implements Contract {
  @Inject() private readonly repo: Repository;
  static count = 0;
  #secret = 1;
  constructor(private dep: Dependency) {}
  get value(): ResultType { return this.repo.read(); }
  async execute(input: Input): Promise<Output> {
    const local = () => helper(input);
    return new Worker(local);
  }
  ['literal']() {}
  [dynamicName()]() {}
}
export const isCnpj = (value: string) => validate(value);
export let counter = 0;
export var legacy = 1;
export function regular(): void {}
export async function asyncFn() {}
export function* generated() { yield 1; }
export interface Api extends Parent {
  run(input: Input): Output;
  value: Value;
}
export type Identifier = string | number;
export enum State { Ready, Busy = 2 }
export namespace Tools { export function work() {} }
export default Service;
"#;
        let artifact = parse_source("matrix.ts", src);
        assert!(
            artifact.diagnostics.is_empty(),
            "{:?}",
            artifact.diagnostics
        );
        for (name, kind) in [
            ("Service", "class_declaration"),
            ("Service.repo", "property"),
            ("Service.count", "property"),
            ("Service.#secret", "property"),
            ("Service.constructor", "method"),
            ("Service.dep", "property"),
            ("Service.value", "method"),
            ("Service.execute", "method"),
            ("Service.execute.local", "function"),
            ("Service.literal", "method"),
            ("isCnpj", "function"),
            ("counter", "variable"),
            ("legacy", "variable"),
            ("regular", "function_declaration"),
            ("asyncFn", "function_declaration"),
            ("generated", "function_declaration"),
            ("Api", "interface_declaration"),
            ("Api.run", "method"),
            ("Api.value", "property"),
            ("Identifier", "type_alias_declaration"),
            ("State", "enum_declaration"),
            ("State.Ready", "enum_member"),
            ("State.Busy", "enum_member"),
            ("Tools", "namespace"),
            ("Tools.work", "function_declaration"),
        ] {
            assert!(
                has_symbol(&artifact, name, kind),
                "missing {name}:{kind}; symbols={:?}",
                artifact.symbols
            );
        }
        assert!(
            !artifact
                .symbols
                .iter()
                .any(|symbol| symbol.qualified_name.contains("dynamicName"))
        );
        let unique_ids: std::collections::BTreeSet<_> =
            artifact.symbols.iter().map(|symbol| &symbol.id).collect();
        assert_eq!(unique_ids.len(), artifact.symbols.len());
        assert_eq!(
            artifact
                .symbols
                .iter()
                .filter(|symbol| symbol.qualified_name == "Service")
                .count(),
            1
        );
        let has_ref = |from: &str, to: &str, kind: EdgeKind| {
            artifact.symbol_refs.iter().any(|reference| {
                reference_owner(&artifact, reference) == from
                    && reference.to == to
                    && reference.kind == kind
            })
        };
        assert!(
            has_ref("Service", "Controller", EdgeKind::Decorates),
            "{:?}",
            artifact.symbol_refs
        );
        assert!(has_ref("Service.repo", "Inject", EdgeKind::Decorates));
        assert!(has_ref("Service.repo", "Repository", EdgeKind::TypeOf));
        assert!(has_ref("Service.dep", "Dependency", EdgeKind::TypeOf));
        assert!(has_ref("Service.execute", "Worker", EdgeKind::Instantiates));
        assert!(has_ref("Service.execute.local", "helper", EdgeKind::Calls));
    }

    #[test]
    fn javascript_uses_javascript_grammar_and_rejects_typescript_only_declarations() {
        let valid = br#"
import DefaultThing, { thing as alias } from './dep.js';
export const handler = async (value) => alias(value);
export function* generated() { yield 1; }
export class Service {
  field = 1;
  execute() { return new DefaultThing(); }
}
export default Service;
"#;
        for path in ["module.js", "module.mjs", "module.cjs"] {
            let artifact = parse_source(path, valid);
            assert_eq!(artifact.language.as_ref(), "javascript");
            assert!(
                artifact.diagnostics.is_empty(),
                "{path}: {:?}",
                artifact.diagnostics
            );
            assert!(has_symbol(&artifact, "handler", "function"));
            assert!(has_symbol(&artifact, "Service.execute", "method"));
        }

        let invalid = parse_source(
            "invalid.js",
            b"interface Shape { value: string }\ntype Id = string;\nenum State { Ready }",
        );
        assert!(!invalid.diagnostics.is_empty());
        assert!(
            !invalid
                .symbols
                .iter()
                .any(|symbol| { matches!(symbol.name.as_str(), "Shape" | "Id" | "State") })
        );
    }

    #[test]
    fn extracts_commonjs_imports_and_exports_without_dynamic_require_false_positives() {
        let source = br#"
const Package = require('./package');
const { Service, helper: alias } = require('./services');
require('./side-effect');
const ignored = require(dynamicPath);
module.exports = Package;
exports.Service = Service;
module.exports.alias = alias;
"#;
        let artifact = parse_source("legacy.cjs", source);
        assert!(
            artifact.diagnostics.is_empty(),
            "{:?}",
            artifact.diagnostics
        );
        assert_eq!(artifact.imports.len(), 3, "{:?}", artifact.imports);
        assert_eq!(artifact.imports[0].specifier, "./package");
        assert_eq!(artifact.imports[0].bindings[0].local, "Package");
        assert_eq!(
            artifact.imports[0].bindings[0].kind,
            ImportBindingKind::Namespace
        );
        assert!(
            artifact.imports[1]
                .bindings
                .iter()
                .any(|binding| binding.imported == "Service" && binding.local == "Service")
        );
        assert!(
            artifact.imports[1]
                .bindings
                .iter()
                .any(|binding| binding.imported == "helper" && binding.local == "alias")
        );
        assert_eq!(artifact.imports[2].specifier, "./side-effect");
        assert!(artifact.imports[2].bindings.is_empty());
        assert!(
            !artifact
                .imports
                .iter()
                .any(|import| import.specifier == "dynamicPath")
        );
        assert_eq!(artifact.exports.len(), 3);
        assert!(
            artifact
                .exports
                .iter()
                .flat_map(|export| &export.bindings)
                .any(|binding| binding.exported == "default" && binding.local == "Package")
        );
        assert!(
            artifact
                .exports
                .iter()
                .flat_map(|export| &export.bindings)
                .any(|binding| binding.exported == "Service" && binding.local == "Service")
        );
        assert!(
            artifact
                .exports
                .iter()
                .flat_map(|export| &export.bindings)
                .any(|binding| binding.exported == "alias" && binding.local == "alias")
        );
    }

    #[test]
    fn module_extensions_select_the_expected_language() {
        for path in ["a.ts", "a.tsx", "a.mts", "a.cts"] {
            assert_eq!(
                parse_source(path, b"export const value = 1;")
                    .language
                    .as_ref(),
                "typescript"
            );
        }
        for path in ["a.js", "a.jsx", "a.mjs", "a.cjs"] {
            assert_eq!(
                parse_source(path, b"export const value = 1;")
                    .language
                    .as_ref(),
                "javascript"
            );
        }
    }

    #[test]
    fn tsx_and_jsx_use_their_language_grammar() {
        let tsx = parse_source(
            "component.tsx",
            b"export const View = (props: Props) => <Panel value={props.value} />;",
        );
        assert!(tsx.diagnostics.is_empty(), "{:?}", tsx.diagnostics);
        assert!(has_symbol(&tsx, "View", "function"));
        assert!(tsx.symbol_refs.iter().any(|reference| {
            reference_owner(&tsx, reference) == "View"
                && reference.to == "Props"
                && reference.kind == EdgeKind::TypeOf
        }));

        let jsx = parse_source(
            "component.jsx",
            b"export const View = (props) => <Panel value={props.value} />;",
        );
        assert!(jsx.diagnostics.is_empty(), "{:?}", jsx.diagnostics);
        assert_eq!(jsx.language.as_ref(), "javascript");
        assert!(has_symbol(&jsx, "View", "function"));
        for artifact in [&tsx, &jsx] {
            assert!(artifact.symbol_refs.iter().any(|reference| {
                reference_owner(artifact, reference) == "View"
                    && reference.to == "Panel"
                    && reference.kind == EdgeKind::References
            }));
        }
    }

    #[test]
    fn covers_modern_typescript_and_javascript_syntax_supported_by_current_grammars() {
        let cases: &[(&str, &[u8])] = &[
            (
                "attributes.mts",
                b"import data from './data.json' with { type: 'json' }; export { data };",
            ),
            (
                "types.mts",
                b"export type * from './types.js'; const config = { mode: 'strict' } satisfies Config;",
            ),
            (
                "defer.mts",
                b"import defer * as feature from './feature.js'; export function use() { return feature.value; }",
            ),
            (
                "resource.mts",
                b"export async function work() { await using resource = openResource(); }",
            ),
            (
                "class.mts",
                b"export class Box<const T> { static { register(Box); } accessor value!: T; #privateMethod() { return this.value; } }",
            ),
            (
                "dynamic.mts",
                b"export async function load() { const module = await import('./lazy.js'); return module?.default?.(); }",
            ),
            (
                "attributes.mjs",
                b"import data from './data.json' with { type: 'json' }; export { data };",
            ),
            (
                "resource.mjs",
                b"export async function work() { await using resource = openResource(); }",
            ),
            (
                "dynamic.mjs",
                b"export async function load() { const module = await import('./lazy.js'); return module?.default?.(); }",
            ),
        ];
        for (path, source) in cases {
            let artifact = parse_source(path, source);
            assert!(
                artifact.diagnostics.is_empty(),
                "{path}: {:?}",
                artifact.diagnostics
            );
            if path.starts_with("attributes") {
                assert!(
                    artifact
                        .imports
                        .iter()
                        .any(|import| import.specifier == "./data.json")
                );
            }
            if path.starts_with("dynamic") {
                assert!(
                    artifact
                        .imports
                        .iter()
                        .any(|import| import.specifier == "./lazy.js")
                );
            }
            if path.starts_with("defer") {
                assert!(artifact.imports.iter().any(|import| {
                    import.specifier == "./feature.js"
                        && import.bindings.iter().any(|binding| {
                            binding.local == "feature"
                                && binding.kind == ImportBindingKind::Namespace
                        })
                }));
            }
            if path.starts_with("resource") {
                assert!(
                    has_symbol(&artifact, "work.resource", "variable"),
                    "{:?}",
                    artifact.symbols
                );
            }
            if path.starts_with("class") {
                assert!(has_symbol(&artifact, "Box.value", "property"));
                assert!(has_symbol(&artifact, "Box.#privateMethod", "method"));
            }
        }
    }

    #[test]
    fn modern_module_keyword_sanitizer_is_token_aware_across_comments_and_lines() {
        let artifact = parse_source(
            "modern.mts",
            br#"
const text = "import defer * export type *";
import /* capability */ defer
  * as feature from './feature.js';
export /* public surface */ type
  * from './types.js';
"#,
        );
        assert!(
            artifact.diagnostics.is_empty(),
            "{:?}",
            artifact.diagnostics
        );
        assert!(artifact.imports.iter().any(|import| {
            import.specifier == "./feature.js"
                && import
                    .bindings
                    .iter()
                    .any(|binding| binding.local == "feature")
        }));
        assert!(artifact.exports.iter().any(|export| {
            export.specifier.as_deref() == Some("./types.js") && export.type_only
        }));
    }

    #[test]
    fn class_expressions_anonymous_defaults_type_queries_and_override_properties_are_extracted() {
        let artifact = parse_source(
            "expressions.ts",
            br#"
const token = 1;
export const C = class { method() { return token; } };
export default () => token;
class Base {}
class Child extends Base { constructor(override value: Base) {} }
type Token = typeof token;
type Remote = import('./remote').Thing;
"#,
        );
        assert!(
            artifact.diagnostics.is_empty(),
            "{:?}",
            artifact.diagnostics
        );
        assert!(has_symbol(&artifact, "C", "class"));
        assert!(has_symbol(&artifact, "C.method", "method"));
        assert!(has_symbol(&artifact, "default", "function"));
        assert!(has_symbol(&artifact, "Child.value", "property"));
        assert!(
            artifact.symbol_refs.iter().any(|reference| {
                reference.to == "token" && reference.kind == EdgeKind::References
            }),
            "{:?}",
            artifact.symbol_refs
        );
        assert!(
            artifact
                .symbol_refs
                .iter()
                .any(|reference| { reference.to == "Thing" && reference.kind == EdgeKind::TypeOf })
        );
        assert!(
            artifact
                .imports
                .iter()
                .any(|import| { import.specifier == "./remote" && import.type_only })
        );
    }

    #[test]
    fn generic_heritage_only_emits_the_direct_base_not_type_arguments() {
        let artifact = parse_source(
            "heritage.ts",
            b"class Child extends NS.Base<Dependency> implements Contract<Options> {}",
        );
        let relations: Vec<_> = artifact
            .symbol_refs
            .iter()
            .filter(|reference| matches!(reference.kind, EdgeKind::Extends | EdgeKind::Implements))
            .map(|reference| (reference.to.as_str(), reference.kind.clone()))
            .collect();
        assert!(
            relations.contains(&("NS.Base", EdgeKind::Extends)),
            "{relations:?}"
        );
        assert!(
            relations.contains(&("Contract", EdgeKind::Implements)),
            "{relations:?}"
        );
        assert!(
            !relations
                .iter()
                .any(|(name, _)| matches!(*name, "Dependency" | "Options"))
        );
    }

    #[test]
    fn lexical_and_type_parameter_shadowing_suppress_false_global_references() {
        let artifact = parse_source(
            "shadow.ts",
            br#"
import { helper } from './helper';
function run<T>() {
  const helper = () => 1;
  helper();
  const value: T = {} as T;
}
function outer() {
  function helper() {}
  helper();
}
"#,
        );
        assert!(
            !artifact
                .symbol_refs
                .iter()
                .any(|reference| reference.to == "T"),
            "{:?}",
            artifact.symbol_refs
        );
        assert!(artifact.symbol_refs.iter().any(|reference| {
            reference_owner(&artifact, reference) == "outer" && reference.to == "helper"
        }));
    }

    #[test]
    fn stable_ids_survive_non_semantic_prefix_edits_and_merge_overloads() {
        let before = parse_source(
            "stable.ts",
            b"export function parse(value: string): string;\nexport function parse(value: string) { return value; }",
        );
        let after = parse_source(
            "stable.ts",
            b"// comment\nexport function parse(value: string): string;\nexport function parse(value: string) { return value; }",
        );
        let before_ids: BTreeSet<_> = before
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "parse")
            .map(|symbol| symbol.id.as_str())
            .collect();
        let after_ids: BTreeSet<_> = after
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "parse")
            .map(|symbol| symbol.id.as_str())
            .collect();
        assert_eq!(before_ids.len(), 1);
        assert_eq!(before_ids, after_ids);
    }

    #[test]
    fn commonjs_object_exports_member_require_and_computed_exports_are_static() {
        let artifact = parse_source(
            "common.cjs",
            br#"
const Service = require('./services').Service;
const helper = require('./services')['helper'];
module.exports = { Service, alias: helper };
exports['computed'] = helper;
"#,
        );
        let bindings: Vec<_> = artifact
            .imports
            .iter()
            .flat_map(|import| &import.bindings)
            .map(|binding| (binding.imported.as_str(), binding.local.as_str()))
            .collect();
        assert!(bindings.contains(&("Service", "Service")), "{bindings:?}");
        assert!(bindings.contains(&("helper", "helper")), "{bindings:?}");
        let exports: BTreeSet<_> = artifact
            .exports
            .iter()
            .flat_map(|export| &export.bindings)
            .map(|binding| binding.exported.as_str())
            .collect();
        assert_eq!(
            exports,
            ["Service", "alias", "computed"].into_iter().collect()
        );
    }

    #[test]
    fn destructuring_emits_only_bound_identifiers_and_all_spans_are_valid() {
        let source = br#"
const { key: local, shorthand, nested: { value }, ...rest } = source;
const [first, , third = fallback] = values;
"#;
        let artifact = parse_source("bindings.ts", source);
        let names: std::collections::BTreeSet<_> = artifact
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["first", "local", "rest", "shorthand", "third", "value"]
                .into_iter()
                .collect()
        );
        assert!(!names.contains("key"));
        assert!(!names.contains("nested"));
        for symbol in &artifact.symbols {
            assert!(!symbol.id.is_empty());
            assert!(!symbol.qualified_name.is_empty());
            assert!((symbol.span.end_byte as usize) <= source.len());
            assert!(symbol.span.start_byte <= symbol.span.end_byte);
        }
        let ids: std::collections::BTreeSet<_> = artifact
            .symbols
            .iter()
            .map(|symbol| symbol.id.as_str())
            .collect();
        for reference in &artifact.symbol_refs {
            assert!(ids.contains(reference.from_id.as_str()));
            assert!((reference.span.end_byte as usize) <= source.len());
        }
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
}
