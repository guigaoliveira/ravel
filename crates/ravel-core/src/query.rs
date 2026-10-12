use crate::{analysis, engine::WorkspaceEngine, graph::QueryLimits, search::SearchKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceQuery {
    Search {
        query: String,
        kind: SearchKind,
        limit: usize,
    },
    Reachable {
        node: String,
        reverse: bool,
        limits: QueryLimits,
    },
    Impact {
        node: String,
        limits: QueryLimits,
    },
    Stats,
    Packages,
    NodeDetail {
        symbol: String,
    },
    FilesInPackage {
        name: String,
        limit: usize,
    },
    Cycles {
        package: Option<String>,
        files: bool,
    },
    Hubs {
        limit: usize,
        kind: Option<String>,
    },
    Orphans {
        limit: usize,
    },
    DiffImpact {
        from: String,
        to: Option<String>,
        limits: QueryLimits,
    },
    Ci {
        strict: bool,
        cycle_threshold: usize,
    },
    ExportDot,
    Validate {
        limit: usize,
    },
    Cochanged {
        file: String,
        commits: usize,
        min_cooccurrence: u32,
    },
    Boundaries,
    Schema,
    RelatedTests {
        path: String,
    },
}

impl WorkspaceQuery {
    pub fn execute(&self, engine: &WorkspaceEngine) -> Result<Value, String> {
        fn encode<T: Serialize, E: std::fmt::Display>(
            result: Result<T, E>,
        ) -> Result<Value, String> {
            result
                .map_err(|error| error.to_string())
                .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))
        }
        if matches!(
            self,
            Self::Search { .. }
                | Self::Impact { .. }
                | Self::Cycles { .. }
                | Self::Hubs { .. }
                | Self::Orphans { .. }
                | Self::DiffImpact { .. }
                | Self::NodeDetail { .. }
                | Self::ExportDot
        ) {
            engine
                .auto_sync_if_dirty()
                .map_err(|error| error.to_string())?;
        }
        match self {
            Self::Search { query, kind, limit } => encode(engine.search_raw(query, *kind, *limit)),
            Self::Reachable {
                node,
                reverse,
                limits,
            } => encode(engine.query(node, *reverse, limits, None)),
            Self::Impact { node, limits } => encode(engine.impact_risk(node, limits)),
            Self::Stats => encode(engine.stats()),
            Self::Packages => match engine
                .storage()
                .open_file_list()
                .map_err(|error| error.to_string())?
            {
                Some(files) => serde_json::to_value(analysis::list_packages_from_paths(
                    files.paths.iter().map(String::as_str),
                ))
                .map_err(|error| error.to_string()),
                None => encode(engine.list_packages()),
            },
            Self::NodeDetail { symbol } => match engine
                .node_detail(symbol)
                .map_err(|error| error.to_string())?
            {
                Some(value) => serde_json::to_value(value).map_err(|error| error.to_string()),
                None => Err(format!("symbol '{symbol}' not found")),
            },
            Self::FilesInPackage { name, limit } => {
                let files = match engine
                    .storage()
                    .open_file_list()
                    .map_err(|error| error.to_string())?
                {
                    Some(files) => files.in_package_limit(name, *limit),
                    None => engine
                        .files_in_package(name)
                        .map_err(|error| error.to_string())?
                        .into_iter()
                        .take(*limit)
                        .collect(),
                };
                serde_json::to_value(files).map_err(|error| error.to_string())
            }
            Self::Cycles { package, files } => {
                if *files {
                    encode(engine.file_cycles(package.as_deref()))
                } else {
                    encode(engine.cycles(package.as_deref()))
                }
            }
            Self::Hubs { limit, kind } => encode(engine.hubs(*limit, kind.as_deref())),
            Self::Orphans { limit } => encode(engine.orphans(*limit)),
            Self::DiffImpact { from, to, limits } => {
                encode(engine.diff_impact(from, to.as_deref(), limits))
            }
            Self::Ci {
                strict,
                cycle_threshold,
            } => encode(engine.ci(*strict, *cycle_threshold)),
            Self::ExportDot => engine
                .export_dot()
                .map(|dot| serde_json::json!({"format":"dot", "content":dot}))
                .map_err(|error| error.to_string()),
            Self::Validate { limit } => engine
                .validate()
                .map_err(|error| error.to_string())
                .and_then(|findings| {
                    serde_json::to_value(analysis::policy_report(findings, *limit))
                        .map_err(|error| error.to_string())
                }),
            Self::Cochanged {
                file,
                commits,
                min_cooccurrence,
            } => encode(engine.cochanged(file, *commits, *min_cooccurrence)),
            Self::Boundaries => encode(engine.boundaries()),
            Self::Schema => encode(engine.describe_schema()),
            Self::RelatedTests { path } => encode(engine.related_tests(path)),
        }
    }
}
