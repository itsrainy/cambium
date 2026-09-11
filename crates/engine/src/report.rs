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
/// - **6** adds `stats.repos_label_only`: repositories an input-path edit
///   dirtied where the derived path mapping showed the edit could not be one
///   of the `.bzl` files loaded from the repository, so only its label
///   consumers were reported. Purely additive.
/// - **7** adds the witness for a changed `.bzl` the base index's load closure
///   has no node for: `stats.bzl_witnessed_dead` (no `load()` in the head
///   checkout names it, so ignoring it was checked rather than assumed),
///   `stats.bzl_witnessed_loaded` (it *is* loaded at head, so the stored
///   closure was incomplete and its loaders were followed),
///   `stats.bzl_witness_packages` (packages the witness dirtied) and
///   `stats.bzl_witness_unavailable` (row sources with no checkout to witness
///   against, where the old ignore-it behaviour stands). Purely additive.
///   The same version adds `stats.bazelrc_flag_nodes` and the `bazelrc_flags` list: a
///   `.bazelrc` edit that changes a Starlark build setting
///   (`--@envoy//bazel:crypto=boringssl`) is no longer automatically global.
///   The setting is a target, so its node is seeded into the direct set and
///   the ordinary reverse walk reports what reads it; `--explain` can report a
///   new direct reason `{"kind": "starlark_flag", "label": "//bazel:crypto"}`.
///   `bazelrc_flags` is omitted when empty.
/// - **8** adds `stats.repos_dirty_by_target`: external repositories whose
///   *generated targets* depend on a main-repo rule target the reverse walk
///   reached — envoy's `@envoy_repo`, whose generated `genrule` lists
///   `@envoy//changelogs` in `tools`, when a changelog entry moves. Such a
///   repository is found after the walk rather than from the diff, so the
///   walk runs again from its label consumers, and `--explain` can report a
///   new direct reason `{"kind": "external_repo_target", "repo":
///   "envoy_repo", "via": "//changelogs:changelogs"}`. Purely additive; the
///   same edit used to be answered by a path prefix over the label's whole
///   package subtree (issue #8).
/// - **9** adds `diagnostics`: the actionable warnings a run raised, each
///   `{code, level, message, location?, help?}` with a stable `CAMBI-W`
///   code. Omitted when empty, so a run with nothing to say is byte-identical
///   to a version 8 document. See `docs/DIAGNOSTICS.md`.
pub const SCHEMA_VERSION: u32 = 9;

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
    /// A `.bazelrc` edit changed the value of a Starlark build setting this
    /// target's definition reads — through a `config_setting`'s `flag_values`,
    /// a `select()` on one, or an `attr.label` default.
    StarlarkFlag { label: String },
    /// It names a target in an external repository whose contents moved —
    /// because `MODULE.bazel` moved, or because a main-repo file the
    /// repository is built from moved.
    ExternalRepo { label: String },
    /// It names a target in an external repository whose *generated targets*
    /// depend on a main-repo rule target the change reached: the repository
    /// rule wrote a BUILD file naming `via`, so whatever moves `via` moves
    /// that repository's targets. Rule (1) only — the rule cannot read a
    /// target's output, so nothing it wrote can have changed.
    ExternalRepoTarget { repo: String, via: String },
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
            DirectReason::StarlarkFlag { label } => {
                format!("starlark build setting {label} changed")
            }
            DirectReason::ExternalRepo { label } => {
                format!("external dependency {label} changed")
            }
            DirectReason::ExternalRepoTarget { repo, via } => {
                format!("external repository @{repo} is built around {via}, which changed")
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
    /// Whether Bazel was asked which packages load a changed `.bzl`.
    ///
    /// **Permanently `false`, retained for schema stability.** Contract
    /// version 3 added it when the engine ran `rbuildfiles`; version 4
    /// replaced that call with the index's static load closure and nothing
    /// in the engine sets it since. It stays in the document because
    /// consumers and the benchmark's own history read the field by name, and
    /// removing it would be a contract break for no gain; the `rbuildfiles`
    /// call itself survives only as the comparison suite's oracle.
    #[serde(default)]
    pub rbuildfiles_called: bool,
    /// Packages dirtied because a changed `.bzl` is in their load closure.
    #[serde(default)]
    pub bzl_closure_hits: usize,
    /// Changed `.bzl` files the base index's load closure had no node for,
    /// where a scan of the head checkout confirmed that no `load()` names
    /// them: dead files, safely ignored.
    #[serde(default)]
    pub bzl_witnessed_dead: usize,
    /// Changed `.bzl` files the closure had no node for that *are* loaded at
    /// head. The stored closure was incomplete; the witness followed the
    /// loaders instead of dropping the file.
    #[serde(default)]
    pub bzl_witnessed_loaded: usize,
    /// Packages the witness dirtied, through a BUILD loader or through the
    /// consumers of a loading `.bzl`.
    #[serde(default)]
    pub bzl_witness_packages: usize,
    /// Changed `.bzl` files unknown to the closure that could not be
    /// witnessed, because the row source has no checkout to enumerate. The
    /// file is still ignored, as it always was; this says so out loud.
    #[serde(default)]
    pub bzl_witness_unavailable: usize,
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
    /// External repositories dirtied because the reverse walk reached a
    /// main-repo *rule target* their generated targets name — envoy's
    /// `@envoy_repo` when a changelog entry moves. Found after the walk, not
    /// from the diff, so the walk is run again from their label consumers.
    #[serde(default)]
    pub repos_dirty_by_target: usize,
    /// Of `repos_dirty_by_input`, how many were dirtied by an edit that
    /// provably cannot be one of the `.bzl` files loaded from the repository —
    /// the derived path mapping says so, or the path is named only by a target
    /// the repository rule writes: their label consumers are affected, their
    /// package closures are not re-queried.
    #[serde(default)]
    pub repos_label_only: usize,
    /// Targets in those repositories that the main repository names, and whose
    /// consumers were therefore made direct.
    #[serde(default)]
    pub external_direct_targets: usize,
    /// Starlark build settings a `.bazelrc` edit changed that resolved to a
    /// node in the index, and whose consumers were therefore made direct
    /// instead of the run going global. A setting that resolved to no node is
    /// counted in `bazelrc_flags` but not here: nothing depends on it.
    #[serde(default)]
    pub bazelrc_flag_nodes: usize,
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
    /// Starlark build settings a `.bazelrc` edit changed and the index
    /// resolved, as the rc file writes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bazelrc_flags: Vec<String>,
    pub direct: Vec<String>,
    /// `direct` plus its reverse closure, sorted by label.
    pub affected: Vec<AffectedTarget>,
    /// The labels of the affected targets that are tests, sorted.
    pub tests: Vec<String>,
    pub timings_ms: Timings,
    pub stats: Stats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanations: Option<Vec<Explanation>>,
    /// Everything the run wants someone to act on: an invalidator that fired,
    /// a store that did not answer, a closure that was incomplete. Drained
    /// from the engine's sink at the end of the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<crate::diag::Diagnostic>,
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
            bazelrc_flags: Vec::new(),
            direct: Vec::new(),
            affected: Vec::new(),
            tests: Vec::new(),
            timings_ms: Timings::default(),
            stats: Stats::default(),
            explanations: None,
            diagnostics: Vec::new(),
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

    /// The diagnostic that says why this run answered `//...`: the one
    /// [`crate::reason::GlobalReason`] raised, whose message carries
    /// `global_reason` verbatim and whose `help` names the fix. Absent on a
    /// run that was not global, and on a document replayed from before the
    /// codes existed.
    pub fn global_cause(&self) -> Option<&crate::diag::Diagnostic> {
        self.diagnostics
            .iter()
            .find(|d| crate::reason::GlobalReason::is_cause(d.code))
    }

    /// The diagnostic that says why this run re-indexed the whole repository
    /// at head, when `stats.cold_rebuild`.
    pub fn cold_rebuild_cause(&self) -> Option<&crate::diag::Diagnostic> {
        self.diagnostics
            .iter()
            .find(|d| d.code == crate::diag::Code::W012ColdRebuild)
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
