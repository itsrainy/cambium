//! Result types shared by the engine and the CLI.

use serde::{Deserialize, Serialize};

/// Version of cambium's JSON contract.
///
/// Every JSON document any subcommand emits carries this as its `schema` field.
/// The contract covers the field names and shapes of those documents *and* the
/// CLI's exit codes: callers parse both, and both are documented together in
/// the README. Bump it whenever either changes in a way a consumer could
/// notice — a renamed or removed field, a changed type, a repurposed exit code.
/// Adding an optional field is still a bump, because a consumer validating the
/// document against a known shape will see it.
///
/// `crates/engine/tests/json_contract.rs` pins the affected report's shape, so
/// a change that forgets to bump this fails there with instructions.
///
/// History:
///
/// - **1** — the first published contract.
/// - **2** — `index stats` gained `bytes.label_sorted`, `bytes.label_total_share`
///   and `bytes.label_packing`. The affected report is unchanged between the
///   two, but the version is shared across every document, so a consumer
///   pinning `schema == 1` sees the bump.
pub const SCHEMA_VERSION: u32 = 2;

/// What a document with no `schema` field is: version 1 predates the field, so
/// reading one back must not claim it was written against today's contract.
pub(crate) const fn schema_version_of_a_document_without_one() -> u32 {
    1
}

/// Per-stage wall time, in milliseconds (spec 10, JSON schema).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Timings {
    pub load: u64,
    pub dirty: u64,
    pub query: u64,
    pub splice: u64,
    pub direct: u64,
    pub walk: u64,
    pub total: u64,
}

/// Why a target ended up in the direct set.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DirectReason {
    /// The target did not exist at base.
    New,
    /// Its own definition changed.
    RuleHash,
    /// Its package's `.bzl` load closure changed, so the rule implementation may have.
    BzlClosure { package: String },
    /// A source file it consumes changed.
    File { path: String },
    /// A target it depended on was deleted.
    DeletedDep { label: String },
}

impl DirectReason {
    pub fn describe(&self) -> String {
        match self {
            DirectReason::New => "target is new".into(),
            DirectReason::RuleHash => "rule definition changed".into(),
            DirectReason::BzlClosure { package } => {
                format!("bzl load closure of //{package} changed")
            }
            DirectReason::File { path } => format!("source file {path} changed"),
            DirectReason::DeletedDep { label } => format!("dependency {label} was deleted"),
        }
    }
}

/// One `--explain` entry: how an affected target connects to the change.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Explanation {
    pub target: String,
    /// Shortest path from `target` back to the direct-set member, inclusive.
    pub path: Vec<String>,
    pub direct: String,
    pub reason: DirectReason,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub base_nodes: u32,
    pub head_nodes: u32,
    pub head_edges: u64,
    pub dirty_packages: usize,
    pub queried_targets: usize,
    pub direct_targets: usize,
    pub affected_targets: usize,
    pub changed_files: usize,
    pub ignored_files: usize,
}

/// The answer, in the shape the CLI serialises (spec 10).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AffectedReport {
    /// JSON contract version; see [`SCHEMA_VERSION`].
    #[serde(default = "schema_version_of_a_document_without_one")]
    pub schema: u32,
    pub base: String,
    pub head: String,
    /// Everything is affected; `affected` and `tests` are empty and callers
    /// should treat the result as `//...`.
    pub global: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    pub dirty_packages: Vec<String>,
    pub direct: Vec<String>,
    pub affected: Vec<String>,
    pub tests: Vec<String>,
    pub timings_ms: Timings,
    pub stats: Stats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanations: Option<Vec<Explanation>>,
}

impl Default for AffectedReport {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            base: String::new(),
            head: String::new(),
            global: false,
            global_reason: None,
            dirty_packages: Vec::new(),
            direct: Vec::new(),
            affected: Vec::new(),
            tests: Vec::new(),
            timings_ms: Timings::default(),
            stats: Stats::default(),
            explanations: None,
        }
    }
}

impl AffectedReport {
    /// A `set(...)` expression for `bazel test` / `bazel build`.
    pub fn as_query_expression(&self, tests_only: bool) -> String {
        if self.global {
            return "//...".to_string();
        }
        let items = if tests_only {
            &self.tests
        } else {
            &self.affected
        };
        if items.is_empty() {
            // `set()` is a valid empty expression; bazel accepts it and builds
            // nothing, which is what callers want in a pipeline.
            return "set()".to_string();
        }
        format!("set({})", items.join(" "))
    }
}

/// Simple stopwatch for stage timings.
pub struct Stopwatch(std::time::Instant);

impl Stopwatch {
    pub fn start() -> Self {
        Self(std::time::Instant::now())
    }

    pub fn lap(&mut self) -> u64 {
        let now = std::time::Instant::now();
        let ms = now.duration_since(self.0).as_millis() as u64;
        self.0 = now;
        ms
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}
