//! Compact text for the answers an agent reads.
//!
//! Every answer is built as JSON and stays available as JSON (`--json`, `RAVEL_FORMAT=json`): that
//! is the contract for scripts. An agent, though, pays for each token it reads, and the JSON
//! spelling of a relation page repeats four key names and the whole path on every site — a symbol
//! referenced three times from one file prints that file's path three times, and a fourth time as
//! the `symbol` of its import. Measured on a 24.6k-file workspace, `callers-of` on a class with 47
//! references cost 2105 tokens as JSON, against about 2000 for `rg -n` over the same name; the same
//! answer rendered here costs about 700.
//!
//! The rules that buy that, in order of what they save:
//! - a path is written once per file, and a directory once when two or more of its files follow
//!   each other;
//! - key names are not repeated per row — position and a fixed word order carry them;
//! - what an answer derives from another field of itself is not printed again (`callers` beside
//!   `relations`, a site's `symbol` that is just its own path);
//! - bookkeeping that changes no decision is left to the JSON (snapshot ids, byte sizes, scores).
//!
//! Anything that qualifies an answer — truncation, an index gap, an unapplied scope, a failed
//! sync — is always printed, on a line starting with `!` when it is a warning.
//!
//! Rendering reads the JSON value rather than the engine's own types, so the daemon protocol is
//! untouched and an answer from an older daemon renders the same way. A value whose shape is not
//! recognised returns `None` and the caller prints it as JSON.

use rustc_hash::FxHashMap;
use serde_json::Value;
use std::fmt::Write as _;

/// How an answer is spelled on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

impl Format {
    /// `RAVEL_FORMAT=json` restores JSON everywhere a script cannot pass `--json` (an MCP server
    /// launched by a client, a wrapper that owns the argument list).
    pub fn from_env() -> Self {
        Self::parse(std::env::var("RAVEL_FORMAT").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value) if value == "json" => Self::Json,
            _ => Self::Text,
        }
    }
}

/// Which answer a value is. The shapes overlap too much to guess from the value alone: a bare
/// array of strings is a list of orphan names from one command and of test paths from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// `callers-of` / `calls-from`: a page of sites, a rollup, or an unresolved name.
    Relations,
    /// `explore` / `context`.
    Explore,
    Status,
    /// Index totals, as `index`, `sync` and `stats` report them.
    Stats,
    Search,
    /// `query`: one page of reachable nodes.
    Reachable,
    /// `impact --risk`.
    Impact,
    Hubs,
    /// A bare list of names (`orphans`).
    Names,
    Cycles,
    Cochanged,
    /// A bare list of paths (`related-tests`, the files of a package).
    Paths,
    Packages,
}

/// Whether a character could act on the reader instead of being read: redraw a terminal, or end
/// the line it is on and start one of its own.
///
/// The JSON spelling escaped these for free. Text does not, and every path, name and message
/// here comes from the repository being queried: a file named `a.ts\n! index is stale` would
/// otherwise print a warning this tool never issued, and one carrying an escape sequence would
/// reach the terminal of whoever ran the command. Only the source excerpt is allowed its own
/// newlines and tabs.
fn acts_on_reader(character: char, multiline: bool) -> bool {
    character.is_control() && !(multiline && matches!(character, '\n' | '\t'))
}

fn is_source_text(inside_source: bool, key: &str) -> bool {
    inside_source && key == "text"
}

fn carries_control(value: &Value, inside_source: bool, key: &str) -> bool {
    let unsafe_text = |text: &str, multiline: bool| {
        text.chars()
            .any(|character| acts_on_reader(character, multiline))
    };
    match value {
        Value::String(text) => unsafe_text(text, is_source_text(inside_source, key)),
        Value::Array(items) => items
            .iter()
            .any(|item| carries_control(item, inside_source, key)),
        Value::Object(map) => map.iter().any(|(name, item)| {
            unsafe_text(name, false)
                || carries_control(item, inside_source || key == "source", name)
        }),
        _ => false,
    }
}

fn spell_out(text: &str, multiline: bool) -> String {
    let mut spelled = String::with_capacity(text.len());
    for character in text.chars() {
        if acts_on_reader(character, multiline) {
            let _ = write!(spelled, "\\u{{{:x}}}", character as u32);
        } else {
            spelled.push(character);
        }
    }
    spelled
}

/// `value` with every control character in its strings and keys written out as `\u{..}`.
fn defused(value: &Value, inside_source: bool, key: &str) -> Value {
    match value {
        Value::String(text) => Value::String(spell_out(text, is_source_text(inside_source, key))),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| defused(item, inside_source, key))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, item)| {
                    (
                        spell_out(name, false),
                        defused(item, inside_source || key == "source", name),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The answer as text, or `None` when its shape is not the one `view` promises.
pub fn text(view: View, value: &Value) -> Option<String> {
    // Almost no answer carries a control character, so the copy is made only for one that does.
    let cleaned;
    let value = if carries_control(value, false, "") {
        cleaned = defused(value, false, "");
        &cleaned
    } else {
        value
    };
    let mut out = match view {
        View::Relations => relations(value)?,
        View::Explore => explore(value)?,
        View::Status => status(value)?,
        View::Stats => stats(value)?,
        View::Search => search(value)?,
        View::Reachable => reachable(value)?,
        View::Impact => impact(value)?,
        View::Hubs => hubs(value)?,
        View::Names => names(value)?,
        View::Cycles => cycles(value)?,
        View::Cochanged => cochanged(value)?,
        View::Paths => paths(value)?,
        View::Packages => packages(value)?,
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// `text`, or the compact JSON when the format asks for it or the shape is not recognised.
pub fn render(format: Format, view: Option<View>, value: &Value) -> String {
    let rendered = match (format, view) {
        (Format::Text, Some(view)) => text(view, value),
        _ => None,
    };
    rendered.unwrap_or_else(|| {
        let mut json = value.to_string();
        json.push('\n');
        json
    })
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn num(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `(Import 16, TypeOf 15)`, or nothing for an empty or absent count map.
fn kind_counts(counts: Option<&Value>) -> String {
    let Some(counts) = counts
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
    else {
        return String::new();
    };
    let parts: Vec<String> = counts
        .iter()
        .map(|(kind, count)| format!("{kind} {count}"))
        .collect();
    format!(" ({})", parts.join(", "))
}

/// A path and whatever is printed after it on its line.
type PathEntry<'a> = (&'a str, String);

fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    }
}

/// One line per entry: its path, then whatever follows it.
///
/// Neighbours in the same directory spell that directory once, on a line of its own ending in
/// `/`, and are then listed by file name behind one space. A path with no such neighbour is
/// written whole, so the common single-file case costs nothing extra and stays greppable.
fn write_paths(out: &mut String, entries: &[(&str, String)]) {
    let mut index = 0;
    while index < entries.len() {
        let dir = split_dir(entries[index].0).0;
        let run = entries[index..]
            .iter()
            .take_while(|(path, _)| split_dir(path).0 == dir)
            .count();
        let shared = run >= 2 && !dir.is_empty();
        if shared {
            out.push_str(dir);
            out.push('\n');
        }
        for (path, rest) in &entries[index..index + run] {
            let shown = if shared { split_dir(path).1 } else { path };
            let _ = writeln!(out, "{}{shown}{rest}", if shared { " " } else { "" });
        }
        index += run;
    }
}

/// What a site says besides its line: the edge kind, what qualifies it, and the symbol on the
/// other end.
fn site_label(site: &Value, path: &str) -> String {
    let kind = str_of(site, "kind").unwrap_or("?");
    let mut label = kind.to_owned();
    // Those two kinds are type positions by definition; the flag only says something on the rest.
    if flag(site, "type_only") && !matches!(kind, "TypeOf" | "Implements") {
        label.push_str("(type)");
    }
    if let Some(confidence) = str_of(site, "confidence") {
        let _ = write!(label, "[{confidence}]");
    }
    // An import is referred to by the file that holds it, which is the path already printed.
    if let Some(symbol) = str_of(site, "symbol").filter(|symbol| *symbol != path) {
        label.push(' ');
        label.push_str(symbol);
    }
    label
}

/// Sites grouped by file: `file: 39 Import; 54 TypeOf Owner.field; 71,90 Calls Owner.method`.
///
/// Lines that share a label — one function referring to the symbol several times — are joined
/// with commas in front of it instead of repeating it.
fn write_sites(out: &mut String, sites: &[Value]) {
    let mut slots: FxHashMap<&str, usize> = FxHashMap::default();
    let mut files: Vec<(&str, Vec<&Value>)> = Vec::new();
    for site in sites {
        let path = str_of(site, "path").unwrap_or("(unknown path)");
        let slot = *slots.entry(path).or_insert_with(|| {
            files.push((path, Vec::new()));
            files.len() - 1
        });
        files[slot].1.push(site);
    }
    let entries: Vec<(&str, String)> = files
        .into_iter()
        .map(|(path, mut sites)| {
            sites.sort_by_key(|site| site.get("line").and_then(Value::as_u64).unwrap_or(u64::MAX));
            let mut labels: Vec<(String, Vec<String>)> = Vec::new();
            for site in sites {
                let label = site_label(site, path);
                let line = site
                    .get("line")
                    .and_then(Value::as_u64)
                    .map_or_else(|| "?".to_owned(), |line| line.to_string());
                match labels.iter_mut().find(|(known, _)| *known == label) {
                    Some((_, lines)) => lines.push(line),
                    None => labels.push((label, vec![line])),
                }
            }
            let parts: Vec<String> = labels
                .into_iter()
                .map(|(label, lines)| format!("{} {label}", lines.join(",")))
                .collect();
            (path, format!(": {}", parts.join("; ")))
        })
        .collect();
    write_paths(out, &entries);
}

/// One definition per line, led by its id: the id is the one key every tool resolves exactly,
/// and it already spells the path and the qualified name, so neither is printed beside it.
fn write_candidates(out: &mut String, candidates: &[Value]) {
    for candidate in candidates {
        let kind = str_of(candidate, "kind").unwrap_or("?");
        let line = num(candidate, "line");
        match str_of(candidate, "id") {
            Some(id) => {
                let _ = writeln!(out, "{id} {kind}, line {line}");
            }
            None => {
                let _ = writeln!(
                    out,
                    "{}:{line} {kind} {}",
                    str_of(candidate, "path").unwrap_or("?"),
                    str_of(candidate, "qualified")
                        .or_else(|| str_of(candidate, "name"))
                        .unwrap_or("?"),
                );
            }
        }
    }
}

/// The index gaps that make a missing reference possible, or nothing when there are none.
fn write_degradation(out: &mut String, degraded: Option<&Value>) {
    let Some(degraded) = degraded.filter(|value| value.is_object()) else {
        return;
    };
    let mut gaps = Vec::new();
    for (key, what) in [
        ("unparsed_files", "unparsed file(s)"),
        ("unparsed_components", "unparsed component file(s)"),
        ("config_problems", "config problem(s)"),
    ] {
        match num(degraded, key) {
            0 => {}
            count => gaps.push(format!("{count} {what}")),
        }
    }
    if !gaps.is_empty() {
        let _ = writeln!(
            out,
            "! index gaps: {} — references there are not listed (see status)",
            gaps.join(", ")
        );
    }
}

fn write_scope(out: &mut String, value: &Value) {
    if let Some(scope) = str_of(value, "scope") {
        let _ = match value.get("scope_applied").and_then(Value::as_bool) {
            Some(false) => writeln!(
                out,
                "! scope `{scope}` NOT applied (the node is already an id, or the scope is blank)"
            ),
            _ => writeln!(out, "scope `{scope}`: applied"),
        };
    }
}

fn relations(value: &Value) -> Option<String> {
    let node = str_of(value, "node")?;
    let incoming = str_of(value, "direction") != Some("outgoing");
    let mut out = String::new();
    if value.get("resolved").and_then(Value::as_bool) == Some(false) {
        let candidates = list(value, "candidates");
        match str_of(value, "reason") {
            Some("ambiguous") => {
                let _ = writeln!(
                    out,
                    "`{node}` is ambiguous: {} definitions, {} shown. Pass scope (a path fragment) or one id.",
                    num(value, "definitions_total"),
                    candidates.len()
                );
            }
            Some("not_in_scope") => {
                let _ = writeln!(
                    out,
                    "`{node}` is not defined under scope `{}`; it is defined in ({} of {} shown):",
                    str_of(value, "scope").unwrap_or_default(),
                    candidates.len(),
                    num(value, "definitions_total"),
                );
            }
            _ => {
                let _ = writeln!(
                    out,
                    "no symbol named `{node}`: {}",
                    str_of(value, "hint").unwrap_or("check the spelling")
                );
            }
        }
        write_candidates(&mut out, candidates);
        if str_of(value, "reason") == Some("ambiguous") {
            write_scope(&mut out, value);
        }
        return Some(out);
    }
    let total = num(value, "total");
    let _ = write!(
        out,
        "{} {node}: {total}{}",
        if incoming { "callers of" } else { "calls from" },
        kind_counts(value.get("by_kind"))
    );
    if let Some(groups) = value.get("by_dir").and_then(Value::as_array) {
        let _ = writeln!(
            out,
            ", by {} directory:",
            if incoming {
                "referencing"
            } else {
                "referenced"
            }
        );
        for group in groups {
            let _ = write!(
                out,
                "{} in {} file(s): {}",
                num(group, "n"),
                num(group, "files"),
                str_of(group, "prefix").unwrap_or("?")
            );
            if let Some(folded) = group.get("groups").and_then(Value::as_u64) {
                let _ = write!(out, " — {folded} more directories");
            }
            out.push('\n');
        }
        if let Some(cursor) = value.get("cursor_ignored").and_then(Value::as_u64) {
            let _ = writeln!(out, "! cursor {cursor} ignored: a rollup has no pages");
        }
        if let Some(hint) = str_of(value, "hint") {
            let _ = writeln!(out, "! {hint}");
        }
    } else {
        let sites = list(value, "sites");
        let next = value.get("next_cursor").and_then(|cursor| {
            cursor
                .as_str()
                .map(str::to_owned)
                .or_else(|| cursor.as_u64().map(|n| n.to_string()))
        });
        let shown = sites.len() as u64;
        if shown == 0 && total > 0 {
            // A cursor past the end. A range computed from it would name sites that do not exist.
            out.push_str(", no sites at this cursor (it is past the end)");
        } else if shown < total {
            // The page does not say where it starts, but where the next one does and how long this
            // one is settle it; the last page ends at the total.
            let end = next
                .as_deref()
                .and_then(|next| next.parse::<u64>().ok())
                .unwrap_or(total);
            let _ = write!(out, ", sites {}-{end}", end.saturating_sub(shown) + 1);
            if let Some(next) = &next {
                let _ = write!(out, "; more: cursor {next}");
            }
        }
        out.push('\n');
        write_sites(&mut out, sites);
    }
    write_scope(&mut out, value);
    if let Some(warning) = str_of(value, "sync_warning") {
        let _ = writeln!(out, "! sync: {warning}");
    }
    write_degradation(&mut out, value.get("degraded_by"));
    Some(out)
}

fn explore(value: &Value) -> Option<String> {
    let query = str_of(value, "q")?;
    let relations = value.get("relations")?;
    let mut out = String::new();
    let detail = value.get("detail").filter(|detail| detail.is_object());
    let candidates = list(value, "candidates");
    let more_candidates = value
        .get("truncation")
        .is_some_and(|truncation| flag(truncation, "candidates"));
    let resolved_id = detail.and_then(|detail| str_of(detail, "id"));
    let others: Vec<Value> = candidates
        .iter()
        .filter(|candidate| str_of(candidate, "id") != resolved_id || resolved_id.is_none())
        .cloned()
        .collect();
    if let Some(detail) = detail {
        let qualified = str_of(detail, "qualified").unwrap_or(query);
        let _ = writeln!(
            out,
            "{qualified}: {}{}, {}:{}-{}",
            if flag(detail, "exported") {
                "exported "
            } else {
                ""
            },
            str_of(detail, "kind").unwrap_or("symbol"),
            str_of(detail, "path").unwrap_or("?"),
            num(detail, "line"),
            num(detail, "end_line"),
        );
        // The name is the re-query key unless another definition shares it; then only the id is.
        if others
            .iter()
            .any(|other| str_of(other, "qualified") == Some(qualified))
            && let Some(id) = resolved_id
        {
            let _ = writeln!(out, "id {id}");
        }
        if let Some(source) = value.get("source").filter(|source| source.is_object()) {
            if let Some(text) = str_of(source, "text") {
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
            if flag(source, "truncated") {
                let _ = writeln!(
                    out,
                    "! source cut at line {}; read the file for the rest",
                    num(source, "end_line")
                );
            }
        }
        for (direction, pager) in [("incoming", "callers-of"), ("outgoing", "calls-from")] {
            let sites = list(relations, direction);
            let total = num(relations, &format!("{direction}_total"));
            let _ = write!(
                out,
                "{direction} {total}{}",
                kind_counts(relations.get(format!("{direction}_by_kind")))
            );
            if (sites.len() as u64) < total {
                let _ = write!(out, ", first {} ({pager} pages the rest)", sites.len());
            }
            out.push_str(if sites.is_empty() { "\n" } else { ":\n" });
            write_sites(&mut out, sites);
        }
        let _ = writeln!(
            out,
            "affected within 2 hops: {}{}",
            num(value, "n_affected"),
            if flag(value, "n_affected_exact") {
                ""
            } else {
                "+"
            }
        );
        for item in list(value, "impact") {
            let _ = writeln!(
                out,
                " {} d{} {}",
                str_of(item, "risk").unwrap_or("?"),
                num(item, "d"),
                str_of(item, "s").unwrap_or("?")
            );
        }
    } else {
        let _ = writeln!(
            out,
            "`{query}` did not resolve to one definition{}",
            if flag(value, "ambiguous") {
                " (ambiguous)"
            } else {
                ""
            }
        );
    }
    // Beside an exact hit the remaining candidates are similar spellings, which the names below
    // already cover. They are the answer itself when nothing resolved, and real alternatives when
    // the definition was picked by ranking terms.
    let picked_exactly = detail.is_some()
        && candidates
            .iter()
            .find(|candidate| str_of(candidate, "id") == resolved_id)
            .and_then(|candidate| str_of(candidate, "why"))
            .is_some_and(|why| why.starts_with("exact"));
    if !others.is_empty() && !picked_exactly {
        let _ = writeln!(
            out,
            "{} ({} shown{}):",
            if detail.is_some() {
                "other candidates"
            } else {
                "candidates"
            },
            others.len(),
            if more_candidates { ", more exist" } else { "" }
        );
        write_candidates(&mut out, &others);
    }
    let primary = detail
        .and_then(|detail| str_of(detail, "name"))
        .or_else(|| str_of(value, "primary"));
    let similar: Vec<&str> = list(value, "matches")
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| Some(*name) != primary)
        .collect();
    // Beside an exact hit these are other spellings nobody asked about.
    if !similar.is_empty() && !picked_exactly {
        let _ = writeln!(
            out,
            "similar names ({} matched): {}",
            num(value, "matches_total"),
            similar.join(", ")
        );
    }
    for warning in list(value, "warnings").iter().filter_map(Value::as_str) {
        // The relation headers above already say how much of each direction is shown.
        if warning.starts_with("incoming edges truncated")
            || warning.starts_with("outgoing edges truncated")
        {
            continue;
        }
        let _ = writeln!(out, "! {warning}");
    }
    if let Some(warning) = str_of(value, "sync_warning") {
        let _ = writeln!(out, "! sync: {warning}");
    }
    write_degradation(
        &mut out,
        value
            .get("coverage")
            .and_then(|coverage| coverage.get("degraded_by")),
    );
    Some(out)
}

fn status(value: &Value) -> Option<String> {
    let indexed = value.get("indexed")?.as_bool()?;
    let mut out = String::new();
    let _ = write!(
        out,
        "ravel {} @ {}: ",
        str_of(value, "binary_version").unwrap_or("?"),
        str_of(value, "root").unwrap_or("?")
    );
    let stats = value.get("stats").filter(|stats| stats.is_object());
    match (indexed, stats) {
        (true, Some(stats)) => {
            let _ = write!(
                out,
                "indexed, {} files, {} edges",
                num(stats, "files"),
                num(stats, "edges")
            );
            // The total, which the diagnostics below only sample.
            match num(stats, "parse_errors") {
                0 => out.push('\n'),
                failed => {
                    let _ = writeln!(out, ", {failed} file(s) with parse errors");
                }
            }
        }
        (true, None) => out.push_str("indexed\n"),
        (false, _) => out.push_str("NOT indexed\n"),
    }
    if let Some(hint) = str_of(value, "hint") {
        let _ = writeln!(out, "{hint}");
    }
    let extensions: Vec<&str> = list(value, "extensions")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let _ = writeln!(
        out,
        "parses: {}; auto-sync {} (mode {}{}){}",
        extensions.join(" "),
        if flag(value, "auto_sync") {
            "on"
        } else {
            "off"
        },
        str_of(value, "sync_mode").unwrap_or("?"),
        if flag(value, "include_untracked") {
            ", untracked included"
        } else {
            ""
        },
        if flag(value, "git_repo") {
            ""
        } else {
            "; not a git repository"
        },
    );
    if let Some(coverage) = value.get("coverage") {
        let unsupported = num(coverage, "unsupported_source_files");
        if unsupported > 0 {
            let by_extension: Vec<String> = coverage
                .get("unsupported_by_extension")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .map(|(extension, count)| format!(".{extension} {count}"))
                        .collect()
                })
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "! not parsed: {unsupported}{} source file(s){} — {}",
                if flag(coverage, "walk_truncated") {
                    "+"
                } else {
                    ""
                },
                if flag(coverage, "walk_truncated") {
                    " (bounded walk, a floor)"
                } else {
                    ""
                },
                by_extension.join(", ")
            );
        }
    }
    // Nothing on disk yet is "not indexed", which the first line already says; only an index
    // written under another schema is a mismatch.
    if let Some(schema) = value.get("schema")
        && let Some(on_disk) = schema.get("on_disk").filter(|on_disk| !on_disk.is_null())
        && Some(on_disk) != schema.get("expected")
    {
        let _ = writeln!(
            out,
            "! schema on disk {on_disk}, this binary expects {}",
            schema.get("expected").unwrap_or(&Value::Null)
        );
    }
    if let Some(error) = str_of(value, "last_update_error") {
        let _ = writeln!(out, "! last update failed: {error}");
    }
    for problem in list(value, "config_problems") {
        let _ = writeln!(
            out,
            "! config {}: {}{}",
            str_of(problem, "code").unwrap_or("problem"),
            str_of(problem, "message").unwrap_or("?"),
            str_of(problem, "path").map_or_else(String::new, |path| format!(" ({path})"))
        );
    }
    // A diagnostic repeats one sentence per file; say it once and list the files under it.
    let mut groups: Vec<(String, Vec<(&str, String)>)> = Vec::new();
    for diagnostic in list(value, "diagnostics") {
        let title = format!(
            "{}: {}",
            str_of(diagnostic, "code").unwrap_or("diagnostic"),
            str_of(diagnostic, "message").unwrap_or("?")
        );
        let path = str_of(diagnostic, "path").unwrap_or("?");
        match groups.iter_mut().find(|(known, _)| *known == title) {
            Some((_, paths)) => paths.push((path, String::new())),
            None => groups.push((title, vec![(path, String::new())])),
        }
    }
    for (title, paths) in groups {
        let _ = writeln!(out, "! {title} — {} file(s):", paths.len());
        write_paths(&mut out, &paths);
    }
    Some(out)
}

fn stats(value: &Value) -> Option<String> {
    value.get("files")?.as_u64()?;
    Some(format!(
        "index: {} files, {} edges, {} parse error(s)\n",
        num(value, "files"),
        num(value, "edges"),
        num(value, "parse_errors")
    ))
}

fn search(value: &Value) -> Option<String> {
    let hits = value.as_array()?;
    if hits.is_empty() {
        return Some("no matches\n".to_owned());
    }
    let mut out = String::new();
    for hit in hits {
        out.push_str(str_of(hit, "value")?);
        if let Some(reason) = str_of(hit, "reason") {
            let _ = write!(out, " ({reason})");
        }
        if let Some(id) = str_of(hit, "definition_id") {
            let _ = write!(out, " {id}");
        }
        out.push('\n');
    }
    Some(out)
}

fn write_walk_bounds(out: &mut String, value: &Value) {
    if flag(value, "truncated") {
        let _ = writeln!(
            out,
            "! walk stopped early{}: the list is partial",
            str_of(value, "reason").map_or_else(String::new, |reason| format!(" ({reason})"))
        );
    }
}

fn reachable(value: &Value) -> Option<String> {
    let items = value.get("items")?.as_array()?;
    let mut out = format!("{} node(s)", items.len());
    if let Some(next) = str_of(value, "next_cursor") {
        let _ = write!(out, "; more: cursor {next}");
    }
    out.push('\n');
    let entries: Vec<(&str, String)> = items
        .iter()
        .filter_map(Value::as_str)
        .map(|item| (item, String::new()))
        .collect();
    write_paths(&mut out, &entries);
    write_walk_bounds(&mut out, value);
    Some(out)
}

fn impact(value: &Value) -> Option<String> {
    let affected = value.get("affected")?.as_array()?;
    let total = num(value, "total_affected");
    let mut out = format!(
        "impact of {}: {total}{} affected",
        str_of(value, "root").unwrap_or("?"),
        if flag(value, "exact") { "" } else { "+" }
    );
    if (affected.len() as u64) < total {
        let _ = write!(out, ", first {}", affected.len());
    }
    out.push_str(" (in = direct dependents)\n");
    // The list is ranked by risk and then by score, so depths interleave inside one risk. Each
    // (risk, depth) pair is gathered under one heading, in the order the pairs first appear and
    // with its members in ranked order, instead of repeating a heading every time depth flips.
    let mut groups: Vec<((&str, u64), Vec<PathEntry<'_>>)> = Vec::new();
    for item in affected {
        let key = (str_of(item, "risk").unwrap_or("?"), num(item, "depth"));
        let entry = (
            str_of(item, "symbol").unwrap_or("?"),
            format!(" in {}", num(item, "in_degree")),
        );
        match groups.iter_mut().find(|(known, _)| *known == key) {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((key, vec![entry])),
        }
    }
    for ((risk, depth), entries) in groups {
        let _ = writeln!(out, "{risk} risk, depth {depth}:");
        write_paths(&mut out, &entries);
    }
    write_walk_bounds(&mut out, value);
    Some(out)
}

fn hubs(value: &Value) -> Option<String> {
    let hubs = value.as_array()?;
    if hubs.is_empty() {
        return Some("no hubs\n".to_owned());
    }
    let mut out = String::from("in out name (kind) path\n");
    for hub in hubs {
        let _ = write!(
            out,
            "{} {} {}",
            num(hub, "in_degree"),
            num(hub, "out_degree"),
            str_of(hub, "name")?
        );
        if let Some(kind) = str_of(hub, "kind") {
            let _ = write!(out, " ({kind})");
        }
        if let Some(path) = str_of(hub, "path") {
            let _ = write!(out, " {path}");
        }
        out.push('\n');
    }
    Some(out)
}

fn names(value: &Value) -> Option<String> {
    let names = value.as_array()?;
    if names.is_empty() {
        return Some("none\n".to_owned());
    }
    let names: Option<Vec<&str>> = names.iter().map(Value::as_str).collect();
    Some(names?.join("\n"))
}

fn cycles(value: &Value) -> Option<String> {
    let cycles = value.as_array()?;
    if cycles.is_empty() {
        return Some("no cycles\n".to_owned());
    }
    let mut out = String::new();
    for cycle in cycles {
        let members: Vec<&str> = cycle
            .get("members")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let _ = writeln!(out, "cycle of {}: {}", members.len(), members.join(", "));
    }
    Some(out)
}

fn cochanged(value: &Value) -> Option<String> {
    // A daemon from before the answer was paged sends the bare list.
    let (entries, page) = match value.as_array() {
        Some(entries) => (entries.as_slice(), None),
        None => (value.get("entries")?.as_array()?.as_slice(), Some(value)),
    };
    let mut out = String::new();
    if let Some(page) = page {
        let _ = write!(
            out,
            "{} file(s) changed with it, over {} commit(s)",
            num(page, "total"),
            num(page, "commits_considered")
        );
        // Otherwise an empty answer reads as "nothing changes with this file" when every commit
        // that touched it was a mass edit.
        match num(page, "commits_skipped_as_bulk") {
            0 => {}
            skipped => {
                let _ = write!(out, " ({skipped} bulk commit(s) skipped)");
            }
        }
        if (entries.len() as u64) < num(page, "total") {
            let _ = write!(out, ", {} shown", entries.len());
        }
        if let Some(next) = page.get("next_cursor").and_then(Value::as_u64) {
            let _ = write!(out, "; more: cursor {next}");
        }
        out.push_str(if entries.is_empty() {
            "\n"
        } else {
            " — commits together, share of them:\n"
        });
    } else if entries.is_empty() {
        return Some("no co-changed files\n".to_owned());
    }
    // Ranked by confidence, so the order is kept and each row carries its own numbers.
    let rows: Option<Vec<(&str, String)>> = entries
        .iter()
        .map(|entry| {
            let mut numbers = format!(" {}", num(entry, "cooccurrence_count"));
            if let Some(confidence) = entry.get("confidence_micros").and_then(Value::as_u64) {
                let _ = write!(numbers, " {}%", (confidence + 5_000) / 10_000);
            }
            Some((str_of(entry, "file")?, numbers))
        })
        .collect();
    write_paths(&mut out, &rows?);
    Some(out)
}

/// `name files`, with the commonest language list said once in the heading and repeated only on
/// the rows that differ — in a TypeScript monorepo that is one word saved on almost every row.
fn packages(value: &Value) -> Option<String> {
    let packages = value.as_array()?;
    if packages.is_empty() {
        return Some("no packages\n".to_owned());
    }
    let languages: Vec<String> = packages
        .iter()
        .map(|package| {
            let names: Vec<&str> = list(package, "languages")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            names.join(",")
        })
        .collect();
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for spelled in &languages {
        match counts.iter_mut().find(|(known, _)| *known == spelled) {
            Some((_, count)) => *count += 1,
            None => counts.push((spelled, 1)),
        }
    }
    // The first seen wins a tie, so the heading does not depend on how the map iterates.
    let mut usual = counts[0];
    for candidate in &counts[1..] {
        if candidate.1 > usual.1 {
            usual = *candidate;
        }
    }
    let usual = usual.0;
    let mut out = format!("{} package(s), name and file count", packages.len());
    if !usual.is_empty() {
        let _ = write!(
            out,
            " ({usual}{})",
            if counts.len() == 1 {
                " in all"
            } else {
                " unless noted"
            }
        );
    }
    out.push_str(":\n");
    for (package, spelled) in packages.iter().zip(&languages) {
        let _ = write!(
            out,
            "{} {}",
            str_of(package, "name")?,
            num(package, "files")
        );
        if spelled != usual {
            let _ = write!(
                out,
                " {}",
                if spelled.is_empty() {
                    "(no language)"
                } else {
                    spelled
                }
            );
        }
        out.push('\n');
    }
    Some(out)
}

fn paths(value: &Value) -> Option<String> {
    let paths = value.as_array()?;
    if paths.is_empty() {
        return Some("none\n".to_owned());
    }
    let entries: Option<Vec<(&str, String)>> = paths
        .iter()
        .map(|path| Some((path.as_str()?, String::new())))
        .collect();
    let mut out = String::new();
    write_paths(&mut out, &entries?);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn site(path: &str, line: u64, kind: &str, symbol: &str) -> Value {
        json!({ "path": path, "line": line, "symbol": symbol, "kind": kind })
    }

    #[test]
    fn a_relation_page_spells_each_path_and_shared_directory_once() {
        let page = json!({
            "node": "symbol://src/pay/charge.ts#value:Charge",
            "direction": "incoming",
            "total": 6,
            "by_kind": { "Calls": 2, "Import": 3, "Instantiates": 1 },
            "next_cursor": null,
            "sites": [
                site("src/api/a.ts", 30, "Calls", "A.run"),
                site("src/api/a.ts", 12, "Calls", "A.run"),
                site("src/api/a.ts", 2, "Import", "src/api/a.ts"),
                site("src/api/b.ts", 9, "Instantiates", "B.constructor"),
                site("src/api/b.ts", 1, "Import", "src/api/b.ts"),
                site("test/charge.spec.ts", 3, "Import", "test/charge.spec.ts"),
            ],
        });
        assert_eq!(
            text(View::Relations, &page).unwrap(),
            "callers of symbol://src/pay/charge.ts#value:Charge: 6 (Calls 2, Import 3, Instantiates 1)\n\
             src/api/\n \
             a.ts: 2 Import; 12,30 Calls A.run\n \
             b.ts: 1 Import; 9 Instantiates B.constructor\n\
             test/charge.spec.ts: 3 Import\n"
        );
    }

    #[test]
    fn a_partial_page_says_which_sites_it_holds_and_where_the_next_starts() {
        let mut page = json!({
            "node": "n", "direction": "outgoing", "total": 5, "by_kind": {},
            "next_cursor": "4",
            "sites": [site("a.ts", 1, "Calls", "x"), site("a.ts", 2, "Calls", "y")],
        });
        let middle = text(View::Relations, &page).unwrap();
        assert!(
            middle.starts_with("calls from n: 5, sites 3-4; more: cursor 4\n"),
            "{middle}"
        );
        page["next_cursor"] = Value::Null;
        let last = text(View::Relations, &page).unwrap();
        assert!(last.starts_with("calls from n: 5, sites 4-5\n"), "{last}");
        page["sites"] = json!([]);
        assert_eq!(
            text(View::Relations, &page).unwrap(),
            "calls from n: 5, no sites at this cursor (it is past the end)\n"
        );
    }

    #[test]
    fn what_qualifies_a_site_or_a_page_is_never_dropped() {
        let page = json!({
            "node": "n", "direction": "incoming", "total": 3, "by_kind": {}, "next_cursor": null,
            "sites": [
                { "path": "a.ts", "line": 4, "symbol": "a.ts", "kind": "Import", "type_only": true },
                { "path": "a.ts", "line": 9, "symbol": "f", "kind": "Calls", "confidence": "inferred" },
                { "path": "a.ts", "line": 7, "symbol": "C.x", "kind": "TypeOf", "type_only": true },
            ],
            "scope": "apps/pay", "scope_applied": false,
            "sync_warning": "watcher died",
            "degraded_by": { "config_problems": 1, "unparsed_components": 0, "unparsed_files": 7 },
        });
        assert_eq!(
            text(View::Relations, &page).unwrap(),
            "callers of n: 3\n\
             a.ts: 4 Import(type); 7 TypeOf C.x; 9 Calls[inferred] f\n\
             ! scope `apps/pay` NOT applied (the node is already an id, or the scope is blank)\n\
             ! sync: watcher died\n\
             ! index gaps: 7 unparsed file(s), 1 config problem(s) — references there are not listed (see status)\n"
        );
    }

    #[test]
    fn an_unresolved_name_lists_each_definition_by_the_id_that_picks_it() {
        let answer = json!({
            "node": "execute", "direction": "incoming", "resolved": false, "reason": "ambiguous",
            "definitions_total": 40, "showing": 3, "scope": null, "scope_applied": false,
            "candidates": [
                { "id": "symbol://a.ts#value:execute@scope:1:2", "kind": "constant", "line": 4, "path": "a.ts", "qualified": "execute" },
                { "id": "symbol://a.ts#value:execute@scope:5:9", "kind": "constant", "line": 8, "path": "a.ts", "qualified": "execute" },
                { "id": "symbol://b.ts#value:Job.execute", "kind": "method", "line": 2, "path": "b.ts", "qualified": "Job.execute" },
            ],
        });
        assert_eq!(
            text(View::Relations, &answer).unwrap(),
            "`execute` is ambiguous: 40 definitions, 3 shown. Pass scope (a path fragment) or one id.\n\
             symbol://a.ts#value:execute@scope:1:2 constant, line 4\n\
             symbol://a.ts#value:execute@scope:5:9 constant, line 8\n\
             symbol://b.ts#value:Job.execute method, line 2\n"
        );
        let missing = json!({
            "node": "Nope", "direction": "incoming", "resolved": false,
            "reason": "no_symbol_named", "hint": "check the spelling",
        });
        assert_eq!(
            text(View::Relations, &missing).unwrap(),
            "no symbol named `Nope`: check the spelling\n"
        );
    }

    #[test]
    fn a_rollup_keeps_both_counts_and_its_own_caveats() {
        let rollup = json!({
            "node": "n", "direction": "incoming", "total": 9000, "by_kind": { "Calls": 9000 },
            "by_dir": [
                { "prefix": "apps/users/src", "n": 45, "files": 15 },
                { "prefix": "(other)", "n": 2, "files": 1, "groups": 4 },
            ],
            "grouped_sites": 47, "prefix_depth": 3, "grouped_by": "referencing_file",
            "cursor_ignored": 10, "rollup_truncated": true, "hint": "counts cover the first 47",
        });
        assert_eq!(
            text(View::Relations, &rollup).unwrap(),
            "callers of n: 9000 (Calls 9000), by referencing directory:\n\
             45 in 15 file(s): apps/users/src\n\
             2 in 1 file(s): (other) — 4 more directories\n\
             ! cursor 10 ignored: a rollup has no pages\n\
             ! counts cover the first 47\n"
        );
    }

    fn explored() -> Value {
        json!({
            "q": "Charge", "primary": "Charge", "matches": ["Charge", "charge", "Charger"],
            "matches_total": 3, "ambiguous": false,
            "candidates": [
                { "id": "symbol://src/charge.ts#value:Charge", "name": "Charge", "qualified": "Charge", "kind": "class_declaration", "path": "src/charge.ts", "line": 3, "end_line": 5, "score": 1, "why": "exact-qualified" },
                { "id": "symbol://src/a.ts#value:A.charge", "name": "charge", "qualified": "A.charge", "kind": "property", "path": "src/a.ts", "line": 7, "end_line": 7, "score": 1, "why": "prefix" },
            ],
            "detail": { "id": "symbol://src/charge.ts#value:Charge", "name": "Charge", "qualified": "Charge", "kind": "class_declaration", "exported": true, "path": "src/charge.ts", "line": 3, "end_line": 5 },
            "source": { "path": "src/charge.ts", "start_line": 3, "end_line": 5, "truncated": false, "text": "3: class Charge {\n4:   run() {}\n5: }" },
            "callers": ["A.run"], "callees": [],
            "relations": {
                "incoming": [site("src/a.ts", 9, "Calls", "A.run")],
                "outgoing": [],
                "incoming_total": 4, "outgoing_total": 0,
                "incoming_by_kind": { "Calls": 3, "Import": 1 }, "outgoing_by_kind": {},
            },
            "impact": [], "n_callers": 1, "n_affected": 12, "n_affected_exact": false,
            "auto_synced": false, "sync_warning": null,
            "warnings": ["incoming edges truncated: showing 1 of 4; re-query with limit=4 for the full list"],
            "truncation": { "candidates": true, "incoming": true, "outgoing": false },
            "coverage": { "degraded_by": { "config_problems": 0, "unparsed_components": 0, "unparsed_files": 0 }, "authoritative_zero": true },
            "sid": "abc",
        })
    }

    #[test]
    fn explore_leads_with_the_definition_and_drops_what_it_already_said() {
        assert_eq!(
            text(View::Explore, &explored()).unwrap(),
            "Charge: exported class_declaration, src/charge.ts:3-5\n\
             3: class Charge {\n\
             4:   run() {}\n\
             5: }\n\
             incoming 4 (Calls 3, Import 1), first 1 (callers-of pages the rest):\n\
             src/a.ts: 9 Calls A.run\n\
             outgoing 0\n\
             affected within 2 hops: 12+\n"
        );
    }

    #[test]
    fn explore_lists_candidates_when_they_are_the_answer_or_a_ranked_alternative() {
        let mut ranked = explored();
        ranked["candidates"][0]["why"] = json!("term-coverage");
        let ranked = text(View::Explore, &ranked).unwrap();
        assert!(
            ranked.contains(
                "other candidates (1 shown, more exist):\nsymbol://src/a.ts#value:A.charge property, line 7\n"
            ),
            "{ranked}"
        );

        let mut unresolved = explored();
        unresolved["detail"] = Value::Null;
        unresolved["source"] = Value::Null;
        unresolved["ambiguous"] = json!(true);
        unresolved["warnings"] =
            json!(["ambiguous symbol name; select a candidate id or qualified name"]);
        assert_eq!(
            text(View::Explore, &unresolved).unwrap(),
            "`Charge` did not resolve to one definition (ambiguous)\n\
             candidates (2 shown, more exist):\n\
             symbol://src/charge.ts#value:Charge class_declaration, line 3\n\
             symbol://src/a.ts#value:A.charge property, line 7\n\
             similar names (3 matched): charge, Charger\n\
             ! ambiguous symbol name; select a candidate id or qualified name\n"
        );
    }

    #[test]
    fn explore_prints_the_id_when_the_name_alone_would_not_resolve_again() {
        let mut twin = explored();
        twin["candidates"][1]["qualified"] = json!("Charge");
        let twin = text(View::Explore, &twin).unwrap();
        assert!(
            twin.contains("\nid symbol://src/charge.ts#value:Charge\n"),
            "{twin}"
        );
    }

    #[test]
    fn status_says_each_diagnostic_once_over_the_files_it_names() {
        let status = json!({
            "root": "/repo", "indexed": true, "binary_version": "1.2.3",
            "stats": { "files": 10, "edges": 20, "bytes": 1, "parse_errors": 2, "snapshot_id": "s" },
            "hint": "Index ready.", "extensions": ["ts", "tsx"], "auto_sync": true,
            "sync_mode": "auto", "include_untracked": true, "git_repo": true,
            "coverage": { "indexed_files": 10, "unsupported_source_files": 3, "unsupported_by_extension": { "vue": 3 }, "walk_truncated": true },
            "schema": { "on_disk": 16, "expected": 17 }, "last_update_error": null,
            "config_problems": [],
            "diagnostics": [
                { "code": "syntax_error", "message": "recovered", "path": "libs/x/a.ts" },
                { "code": "syntax_error", "message": "recovered", "path": "libs/x/b.ts" },
            ],
        });
        assert_eq!(
            text(View::Status, &status).unwrap(),
            "ravel 1.2.3 @ /repo: indexed, 10 files, 20 edges, 2 file(s) with parse errors\n\
             Index ready.\n\
             parses: ts tsx; auto-sync on (mode auto, untracked included)\n\
             ! not parsed: 3+ source file(s) (bounded walk, a floor) — .vue 3\n\
             ! schema on disk 16, this binary expects 17\n\
             ! syntax_error: recovered — 2 file(s):\n\
             libs/x/\n a.ts\n b.ts\n"
        );
    }

    #[test]
    fn a_workspace_with_no_index_is_not_reported_as_a_schema_mismatch() {
        let status = json!({
            "root": "/repo", "indexed": false, "binary_version": "1.2.3", "stats": null,
            "hint": "Run `ravel index` first.", "extensions": ["ts"], "auto_sync": false,
            "sync_mode": "none", "include_untracked": false, "git_repo": false,
            "coverage": { "indexed_files": 0, "unsupported_source_files": 0, "unsupported_by_extension": {}, "walk_truncated": false },
            "schema": { "on_disk": null, "expected": 17 }, "last_update_error": null,
            "config_problems": [], "diagnostics": [],
        });
        assert_eq!(
            text(View::Status, &status).unwrap(),
            "ravel 1.2.3 @ /repo: NOT indexed\n\
             Run `ravel index` first.\n\
             parses: ts; auto-sync off (mode none); not a git repository\n"
        );
    }

    #[test]
    fn the_smaller_answers_keep_their_bounds() {
        let page = json!({
            "items": ["src/a.ts", "src/b.ts", "lib/c.ts"], "next_cursor": "3",
            "truncated": true, "reason": "node limit", "snapshot_id": "s",
            "visited_nodes": 3, "visited_edges": 9,
        });
        assert_eq!(
            text(View::Reachable, &page).unwrap(),
            "3 node(s); more: cursor 3\nsrc/\n a.ts\n b.ts\nlib/c.ts\n\
             ! walk stopped early (node limit): the list is partial\n"
        );
        let report = json!({
            "root": "n", "total_affected": 5, "exact": false, "truncated": false, "reason": null,
            "affected": [
                { "symbol": "src/a.ts", "risk": "high", "depth": 1, "in_degree": 3, "score": 80 },
                { "symbol": "src/b.ts", "risk": "high", "depth": 1, "in_degree": 2, "score": 70 },
                { "symbol": "lib/c.ts", "risk": "low", "depth": 2, "in_degree": 0, "score": 5 },
                { "symbol": "src/d.ts", "risk": "high", "depth": 1, "in_degree": 1, "score": 4 },
            ],
        });
        assert_eq!(
            text(View::Impact, &report).unwrap(),
            "impact of n: 5+ affected, first 4 (in = direct dependents)\n\
             high risk, depth 1:\nsrc/\n a.ts in 3\n b.ts in 2\n d.ts in 1\n\
             low risk, depth 2:\nlib/c.ts in 0\n"
        );
        assert_eq!(
            text(
                View::Cochanged,
                &json!([
                    { "file": "a/x.ts", "cooccurrence_count": 3 },
                    { "file": "a/y.ts", "cooccurrence_count": 3 },
                    { "file": "b.ts", "cooccurrence_count": 2 },
                ])
            )
            .unwrap(),
            "a/\n x.ts 3\n y.ts 3\nb.ts 2\n"
        );
        assert_eq!(
            text(View::Search, &json!([
                { "value": "isCnpj", "reason": "exact-case", "score_micros": 1 },
                { "value": "Cnpj", "score_micros": 1, "definition_id": "symbol://a.ts#value:Cnpj" },
            ]))
            .unwrap(),
            "isCnpj (exact-case)\nCnpj symbol://a.ts#value:Cnpj\n"
        );
        assert_eq!(
            text(View::Hubs, &json!([
                { "name": "On", "in_degree": 9, "out_degree": 1, "kind": "function_declaration", "path": "a.ts" },
                { "name": "winston", "in_degree": 4, "out_degree": 0 },
            ]))
            .unwrap(),
            "in out name (kind) path\n9 1 On (function_declaration) a.ts\n4 0 winston\n"
        );
        assert_eq!(
            text(View::Cycles, &json!([{ "members": ["a", "b"], "size": 2 }])).unwrap(),
            "cycle of 2: a, b\n"
        );
        let page = json!({
            "entries": [
                { "file": "a/x.ts", "cooccurrence_count": 3, "confidence_micros": 600_000 },
                { "file": "a/y.ts", "cooccurrence_count": 2, "confidence_micros": 333_333 },
            ],
            "total": 9, "truncated": true, "next_cursor": 2,
            "commits_considered": 5, "commits_skipped_as_bulk": 1,
        });
        assert_eq!(
            text(View::Cochanged, &page).unwrap(),
            "9 file(s) changed with it, over 5 commit(s) (1 bulk commit(s) skipped), 2 shown; \
             more: cursor 2 — commits together, share of them:\na/\n x.ts 3 60%\n y.ts 2 33%\n"
        );
        let empty = json!({
            "entries": [], "total": 0, "truncated": false, "next_cursor": null,
            "commits_considered": 0, "commits_skipped_as_bulk": 4,
        });
        assert_eq!(
            text(View::Cochanged, &empty).unwrap(),
            "0 file(s) changed with it, over 0 commit(s) (4 bulk commit(s) skipped)\n"
        );
        assert_eq!(text(View::Names, &json!(["A", "B"])).unwrap(), "A\nB\n");
        let api = json!({ "name": "api", "files": 9, "languages": ["typescript"] });
        let web = json!({ "name": "web", "files": 2, "languages": ["typescript"] });
        assert_eq!(
            text(View::Packages, &json!([api, web])).unwrap(),
            "2 package(s), name and file count (typescript in all):\napi 9\nweb 2\n"
        );
        let mixed = json!({ "name": "web", "files": 2, "languages": ["javascript", "typescript"] });
        assert_eq!(
            text(View::Packages, &json!([api, mixed])).unwrap(),
            "2 package(s), name and file count (typescript unless noted):\napi 9\nweb 2 javascript,typescript\n"
        );
        assert_eq!(
            text(View::Paths, &json!(["t/a.spec.ts"])).unwrap(),
            "t/a.spec.ts\n"
        );
        assert_eq!(
            text(View::Stats, &json!({ "files": 2, "edges": 3, "bytes": 9, "parse_errors": 0, "snapshot_id": "s" })).unwrap(),
            "index: 2 files, 3 edges, 0 parse error(s)\n"
        );
    }

    #[test]
    fn an_unrecognised_shape_falls_back_to_json_rather_than_printing_a_guess() {
        let odd = json!({ "unexpected": true });
        for view in [
            View::Relations,
            View::Explore,
            View::Status,
            View::Stats,
            View::Search,
            View::Reachable,
            View::Impact,
            View::Hubs,
            View::Names,
            View::Cycles,
            View::Cochanged,
            View::Paths,
            View::Packages,
        ] {
            assert_eq!(text(view, &odd), None, "{view:?}");
            assert_eq!(
                render(Format::Text, Some(view), &odd),
                "{\"unexpected\":true}\n"
            );
        }
        let stats = json!({ "files": 1, "edges": 1, "parse_errors": 0 });
        assert_eq!(
            render(Format::Json, Some(View::Stats), &stats),
            format!("{stats}\n")
        );
        assert_eq!(render(Format::Text, None, &stats), format!("{stats}\n"));
    }

    #[test]
    fn text_from_the_repository_cannot_forge_a_line_or_reach_the_terminal() {
        let page = json!({
            "node": "n", "direction": "incoming", "total": 1, "by_kind": {}, "next_cursor": null,
            "sites": [{
                "path": "a.ts\n! index gaps: none\u{1b}[2J", "line": 1,
                "symbol": "f\r\u{9b}31m", "kind": "Calls",
            }],
        });
        assert_eq!(
            text(View::Relations, &page).unwrap(),
            "callers of n: 1\na.ts\\u{a}! index gaps: none\\u{1b}[2J: 1 Calls f\\u{d}\\u{9b}31m\n"
        );

        // The source excerpt keeps its own lines and indentation, and nothing else that moves.
        let mut answer = explored();
        answer["source"]["text"] =
            json!("3: class Charge {\n4:\trun() {} \u{1b}]0;title\u{7}\n5: }");
        let rendered = text(View::Explore, &answer).unwrap();
        assert!(
            rendered.contains("3: class Charge {\n4:\trun() {} \\u{1b}]0;title\\u{7}\n5: }\n"),
            "{rendered}"
        );
        assert!(!rendered.contains(['\u{1b}', '\u{7}']), "{rendered}");

        // An extension is a key, and it comes from a file name too.
        let status = json!({
            "root": "/repo", "indexed": false,
            "coverage": { "unsupported_source_files": 1, "unsupported_by_extension": { "v\u{1b}ue": 1 } },
        });
        let rendered = text(View::Status, &status).unwrap();
        assert!(rendered.contains(".v\\u{1b}ue 1"), "{rendered}");
    }

    #[test]
    fn only_an_explicit_json_request_leaves_text() {
        assert_eq!(Format::parse(None), Format::Text);
        assert_eq!(Format::parse(Some("text")), Format::Text);
        assert_eq!(Format::parse(Some(" JSON ")), Format::Json);
        assert_eq!(Format::parse(Some("yaml")), Format::Text);
    }
}
