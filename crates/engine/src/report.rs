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
/// - **3** — `affected` is a list of `{label, depth, test}` objects rather than
///   labels: `depth` is the number of reverse-dependency hops from the change
///   at which the target was first reached (0 for the direct set). `stats`
///   gained `rbuildfiles_called`; `index stats` gained
///   `in_degree.top`. `direct` and `tests` stay lists of labels, and the `text`
///   and `query` outputs are unchanged.
/// - **4** — `stats` gained `bzl_closure_hits`, the number of packages dirtied
///   because a changed `.bzl` is in their static load closure.
///   `stats.rbuildfiles_called` stays in the document and is now always
///   `false`: the closure is read from the index instead of from Bazel.
/// - **5** — `stats` gained `bazelrc_safe_diffs` (changed bazelrc files
///   whose every changed flag was on the loading-irrelevant allowlist and
///   which therefore invalidated nothing) and `stats.cold_rebuild` with the
///   optional `cold_rebuild_reason`: a run whose dirty set exceeded
///   `limits.max_dirty_packages` re-indexed the whole repository at head
///   instead of splicing. That is *not* a global invalidation — `affected`
///   is still exact — so a consumer that treats `global` as "run everything"
///   needs no change. `limits.max_changed_files`, which used to make that
///   case global, is gone.
///   External repositories also became first-class: `stats` gained
///   `repos_dirty_by_input`, `repos_dirty_by_module`, `repos_dirty_by_bzl` and
///   `external_direct_targets`, and `--explain` can report a new direct reason
///   `{"kind": "external_repo", "label": "@@repo+//pkg:target"}`.
pub const SCHEMA_VERSION: u32 = 5;

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
    /// It names a target in an external repository whose contents moved —
    /// because `MODULE.bazel` moved, or because a main-repo file the
    /// repository is built from moved.
    ExternalRepo { label: String },
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
            DirectReason::ExternalRepo { label } => {
                format!("external dependency {label} changed")
            }
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
    /// A `.bzl` file changed, so Bazel was asked which packages load it.
    ///
    /// Always false since the static load closure replaced the `rbuildfiles`
    /// call; kept in the contract because consumers (and the benchmark's own
    /// history) read it, and because the method behind it still exists for the
    /// comparison suite's oracle.
    #[serde(default)]
    pub rbuildfiles_called: bool,
    /// Packages dirtied because a changed `.bzl` is in their load closure.
    #[serde(default)]
    pub bzl_closure_hits: usize,
    /// Changed bazelrc files read and found irrelevant to loading: every
    /// changed flag was on the allowlist, so the edit invalidated nothing
    /// instead of invalidating everything.
    #[serde(default)]
    pub bazelrc_safe_diffs: usize,
    /// The dirty set was larger than `limits.max_dirty_packages`, so the run
    /// re-queried `//...` at head rather than the dirty packages alone. The
    /// answer is exact either way; only the cost differs, and
    /// `dirty_packages` still reports the count that triggered it.
    #[serde(default)]
    pub cold_rebuild: bool,
    /// External repositories dirtied because a main-repo file they are built
    /// from changed (a `local_path_override` directory, or a file a repository
    /// rule reads by label).
    #[serde(default)]
    pub repos_dirty_by_input: usize,
    /// External repositories dirtied by the `MODULE.bazel` diff.
    #[serde(default)]
    pub repos_dirty_by_module: usize,
    /// External repositories dirtied because a `.bzl` that defines them
    /// changed.
    #[serde(default)]
    pub repos_dirty_by_bzl: usize,
    /// Targets in those repositories that the main repository names, and whose
    /// consumers were therefore made direct.
    #[serde(default)]
    pub external_direct_targets: usize,
}

/// One affected target, with how far from the change it sits.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AffectedTarget {
    pub label: String,
    /// Reverse-dependency hops from the nearest direct-set member: 0 means the
    /// target's own inputs changed. A hop through a tombstoned (deleted)
    /// dependency counts, so a consumer of a deleted target sits at depth 1.
    pub depth: u32,
    /// Whether the target is a test rule.
    pub test: bool,
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
    /// Why the run re-indexed the whole repository, when `stats.cold_rebuild`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold_rebuild_reason: Option<String>,
    pub dirty_packages: Vec<String>,
    pub direct: Vec<String>,
    /// `direct` plus its reverse closure, sorted by label.
    pub affected: Vec<AffectedTarget>,
    /// The labels of the affected targets that are tests, sorted.
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
            cold_rebuild_reason: None,
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
        let items: Vec<&str> = if tests_only {
            self.tests.iter().map(String::as_str).collect()
        } else {
            self.affected.iter().map(|t| t.label.as_str()).collect()
        };
        if items.is_empty() {
            // `set()` is a valid empty expression; bazel accepts it and builds
            // nothing, which is what callers want in a pipeline.
            return "set()".to_string();
        }
        format!("set({})", items.join(" "))
    }

    /// The affected labels, in the order they are reported (sorted).
    pub fn affected_labels(&self) -> Vec<String> {
        self.affected.iter().map(|t| t.label.clone()).collect()
    }

    /// Affected labels no deeper than `max_depth` hops from the change.
    pub fn affected_within(&self, max_depth: u32) -> impl Iterator<Item = &AffectedTarget> {
        self.affected.iter().filter(move |t| t.depth <= max_depth)
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
