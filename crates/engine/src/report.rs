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
/// - **5** — `stats` gained `bazelrc_safe_diffs` (changed bazelrc files the
///   screen read and cleared: none of the triples the index query reads
///   changed in a way it could not account for, so the file itself
///   invalidated nothing) and `stats.cold_rebuild` with the
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
///   to a version 8 document. See `docs/DIAGNOSTICS.md`. The same version adds
///   `stats.spliced_rows`: the target rows the splice wrote into the base
///   index, new or rehashed plus the ones it tombstoned. Zero means the
///   commit changed nothing the graph models, which is what `cambi profile`
///   reports per commit as `index`. Purely additive.
/// - **10** adds `depth_histogram`: the number of affected targets at each
///   reverse-dependency depth, index 0 first, so a consumer gets the shape of
///   the answer without counting `affected[].depth` itself. Its length is one
///   more than the deepest target reached, and it is omitted (as an empty
///   list) on a global run, where nothing was walked. Purely additive; the
///   per-target `depth` it summarises has been in the document since
///   version 3.
/// - **11** adds `affected[].package_depth` and `package_depth_histogram`:
///   the same walk measured in package boundaries crossed instead of hops,
///   an edge inside one package being free. A target's `package_depth` never
///   exceeds its `depth`. The histogram has the shape of `depth_histogram`
///   and is empty in the same cases. Purely additive.
/// - **12** retires the witness fields (`stats.bzl_witnessed_dead`,
///   `bzl_witnessed_loaded`, `bzl_witness_packages`,
///   `bzl_witness_unavailable`) with the witness itself, and adds
///   `stats.sky_seeds`, `sky_unknown_seeds`, `sky_visited` and
///   `sky_new_packages`. Which packages a change moved is answered by a
///   reverse walk over the Skyframe loading layer the index now carries
///   (`cambium_index::sky`) rather than by a statically parsed `.bzl` load
///   closure and three modelled rules, so there is no stored closure left for
///   a witness to find incomplete. `stats.bzl_closure_hits` keeps its name and
///   its meaning: dirty packages a changed Starlark file reached.
/// - **13** adds `stats.repos_settled`: dirty in-tree repositories whose
///   packages the index holds and whose every dirtying path was a modified
///   source file of one of them, so no package of theirs was re-queried and
///   no label of theirs seeded; the changed files reach their consumers by
///   label. Purely additive.
/// - **14** adds `stats.repos_dirty_by_consumer`: external repositories
///   dirtied because an indexed repository their rules depend on moved,
///   read from the index's consumer table (format 13). Purely additive.
/// - **15** adds `stats.repos_dirty_by_loader`: external repositories
///   dirtied because a workspace-closure `.bzl` (or `MODULE.bazel`) that
///   defines them loads from a repository whose `.bzl` files may have moved.
///   Purely additive.
/// - **16** changes `inspect verify`: both sides are compared as
///   main-repository rule targets, so `theirs_total` no longer counts the
///   source and generated files or external targets Bazel's `rdeps` names
///   (now `ignored_files` and `ignored_external`). `precision_pct` is
///   redefined as `agreed_total / ours_total` (it was `theirs_total /
///   ours_total`, which could pass 100). `failures[].missing` is sorted and
///   gained `round_trip` and `via` (misses Bazel connects only through an
///   external repository, and the external targets on those paths).
///   `escalations` lists toolchain-class samples whose misses a run covers
///   by answering `//...` under `CAMBI-W023`; they no longer count as
///   unsound.
/// - **17** is `cambi profile`'s. A commit that fails no longer ends the
///   profile: its step
///   carries `failed` (the error as a diagnostic) and `index: {"kind":
///   "failed"}`, the next one is indexed cold with `cold_after_failure`, the
///   walk stops after three failures in a row with `stopped` set, and
///   `from_failed` says when the cold build moved off the range's first
///   commit. `cold` gains `targets` and `tests`; each step gains `subject`
///   and `global_cause`; `summary.global_reasons[]` gains `code` and `help`;
///   `summary` gains `global_rebuild` and `requery` (the two halves of
///   `rebuild`), `incremental_affected` and `incremental_tests` (mean, p50,
///   p90), `affected_share`, `tests_share`, `global_share`,
///   `index_spliced`, `index_unchanged`, `failed` and
///   `rebuilt_after_failure`; the document gains `repository`. The jsonl
///   `row` event of `profile` gains `step`, the raw step, beside the display
///   strings it already had. Existing fields keep their meaning: a global
///   step is still `affected: 0` with `global: true`, and `mean_affected`
///   still averages it in. Purely additive.
/// - **18** adds `stats.consumer_leaves_seeded`: leaves of external
///   repositories seeded because the consumer table's leaf map (index
///   format 14) says their transitive deps reach a package of an indexed
///   repository that moved, or does not know the leaf. A consumer seeded
///   this way is counted in `repos_dirty_by_consumer` as before, but only
///   its reaching leaves are seeded rather than all of them. Purely
///   additive.
/// - **19** adds `stats.closure_leaves_seeded`: leaves seeded because their
///   transitive deps closure runs through a repository that moved (index
///   format 16), and for no other reason. `external_direct_targets` counts
///   the rules above them as before. Purely additive.
pub const SCHEMA_VERSION: u32 = 19;

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
    /// A changed Starlark file reached its package through the loading
    /// layer's `BZL_LOAD` edges, so the rule implementation may have moved
    /// without the rule proto moving. (The name is the one this reason has
    /// carried since a static load closure answered the same question.)
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
    /// Of `direct_targets`, the rules `affected` leaves out because a tag in
    /// `output.exclude_tags` marks them (`manual`, by default) and the run
    /// did not pass `--include-manual`: why `direct_targets` can exceed
    /// `affected_targets`. Said by `cambi explain`; not part of the JSON
    /// document, whose contract this would change.
    #[serde(skip)]
    pub direct_excluded_by_tag: usize,
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
    /// Dirty packages a changed Starlark file reached, through the loading
    /// layer's `BZL_LOAD` edges. Their rules are direct whatever their hashes
    /// say. Named as it was when a static load closure answered the same
    /// question, because the benchmark's own history reads the field by name.
    #[serde(default)]
    pub bzl_closure_hits: usize,
    /// Skyframe keys the changed paths moved: one `FILE_STATE` per path, plus
    /// a `DIRECTORY_LISTING_STATE` per ancestor directory of an add or a
    /// delete.
    #[serde(default)]
    pub sky_seeds: usize,
    /// Of those, the keys the stored layer has no node for. Normal, and the
    /// usual reason a changed path costs nothing: Bazel's loading phase never
    /// looked at it.
    #[serde(default)]
    pub sky_unknown_seeds: usize,
    /// Nodes the reverse walk over the layer visited.
    #[serde(default)]
    pub sky_visited: usize,
    /// Dirty directories the walk reached through a `PACKAGE_LOOKUP` with no
    /// `PACKAGE` node behind it: a package this change may have created, or a
    /// directory that holds none. Re-queried either way; the second kind
    /// comes back empty and tombstones nothing.
    #[serde(default)]
    pub sky_new_packages: usize,
    /// Changed bazelrc files the screen read and cleared, so the file itself
    /// invalidated nothing instead of invalidating everything.
    ///
    /// Cleared means every triple it changed that the index query reads was
    /// accounted for: on the loading-irrelevant allowlist, a Starlark build
    /// setting the flag resolver took, or `--registry`, which is routed to
    /// the externals stage. A triple under a command or a `--config` the
    /// query never reads is not read at all and does not keep the file out
    /// of this count. Counted per file before the routed causes are
    /// answered, so a run can report a cleared rc file and still be global:
    /// a `--registry` the externals stage then escalates (`CAMBI-W014`) is
    /// the case to expect. It is a count of files, not of allowlist hits.
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
    /// Of the dirty repositories whose packages this index holds, how many
    /// were dirtied only by modified source files of those packages, so that
    /// nothing was re-queried or seeded for them: their rules stand, and the
    /// changed files reach their consumers by label, as a main-repo source
    /// edit does.
    #[serde(default)]
    pub repos_settled: usize,
    /// External repositories dirtied because an indexed repository they
    /// consume moved (`RepoInputs::consumers`): envoy's `@grpc` on a change
    /// under `api/`. Their leaves seed the walk, as any dirty repository's
    /// do.
    #[serde(default)]
    pub repos_dirty_by_consumer: usize,
    /// Leaves seeded through a consumer's leaf map
    /// (`Consumer::leaves`): the leaves of a consumer whose transitive deps
    /// reach a package of a moved indexed repository, plus any leaf the map
    /// does not know. The consumer's other leaves are not seeded.
    #[serde(default)]
    pub consumer_leaves_seeded: usize,
    /// Leaves seeded because their closure runs through a repository that
    /// moved (index format 16, `leaf_closure`), and for no other reason:
    /// envoy's `@grpc//:grpc++` when an input of `@c-ares` moves.
    #[serde(default)]
    pub closure_leaves_seeded: usize,
    /// External repositories dirtied because the definition that makes them
    /// loads a `.bzl` from a repository whose `.bzl` files may have moved:
    /// xla's `@llvm-project`, which a module extension defines by calling
    /// `llvm_configure` from `@llvm-raw`, when an input of `@llvm-raw` moves.
    #[serde(default)]
    pub repos_dirty_by_loader: usize,
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
    /// Target rows the splice wrote into the base index: the ones that were
    /// new or whose rule hash changed, plus the ones it tombstoned because
    /// they left their package or their package was deleted. Zero means the
    /// commit changed nothing the graph models and the index at head is the
    /// base, byte for byte.
    #[serde(default)]
    pub spliced_rows: usize,
}

/// One affected target, with how far from the change it sits.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AffectedTarget {
    pub label: String,
    /// Reverse-dependency hops from the nearest direct-set member: 0 means the
    /// target's own inputs changed. A hop through a tombstoned (deleted)
    /// dependency counts, so a consumer of a deleted target sits at depth 1.
    pub depth: u32,
    /// Package boundaries crossed on the way from the nearest direct-set
    /// member: 0 means the change and the target share a package. A hop
    /// inside one package is free, so this is at most `depth`.
    #[serde(default)]
    pub package_depth: u32,
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
    /// How many affected targets sit at each depth, `depth_histogram[d]` for
    /// depth `d`, starting at 0. A depth inside the range that nothing
    /// reached is a zero rather than a gap, so the list can be drawn as a
    /// histogram straight through. Empty when nothing was affected, and on a
    /// global run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depth_histogram: Vec<u32>,
    /// `depth_histogram` for `package_depth`: how many affected targets sit
    /// at each count of package boundaries crossed, index 0 first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package_depth_histogram: Vec<u32>,
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
            depth_histogram: Vec::new(),
            package_depth_histogram: Vec::new(),
            timings_ms: Timings::default(),
            stats: Stats::default(),
            explanations: None,
            diagnostics: Vec::new(),
        }
    }
}

impl AffectedReport {
    /// A `set(...)` expression for `bazel query` / `bazel cquery`.
    ///
    /// Not a target pattern: `bazel build` and `bazel test` reject it (`invalid
    /// package name 'set(//a'`), and read `cambi affected --format labels`
    /// through `--target_pattern_file` instead.
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
            // `set()` is a valid empty query expression: `bazel query 'set()'`
            // answers with nothing. (`bazel build 'set()'` is an error, like
            // every other `set(...)` handed to a build.)
            return "set()".to_string();
        }
        format!("set({})", items.join(" "))
    }

    /// The affected labels, in the order they are reported (sorted).
    pub fn affected_labels(&self) -> Vec<String> {
        self.affected.iter().map(|t| t.label.clone()).collect()
    }

    /// The deepest an affected target sits, or `None` when none were.
    pub fn max_depth(&self) -> Option<u32> {
        (self.depth_histogram.len() as u32).checked_sub(1)
    }

    /// The most package boundaries any affected target sits behind; `None`
    /// when nothing was walked.
    pub fn max_package_depth(&self) -> Option<u32> {
        (self.package_depth_histogram.len() as u32).checked_sub(1)
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
