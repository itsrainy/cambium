//! The reverse-dependency oracle.
//!
//! bazel-diff is a *comparison*, not a ground truth: where the two tools
//! disagree, neither is authoritative. Bazel itself is. So for every
//! incremental change set the suite takes **every** changed path that belongs
//! to a package at head, names it as a source label, and asks Bazel one
//! question:
//!
//! ```text
//! kind(rule, rdeps(//..., set(//pkg:a.cc //pkg:defs.bzl //other:BUILD.bazel …)))
//! ```
//!
//! Every rule in that answer must be in cambium's affected set. Extra targets
//! on cambium's side are over-approximation (allowed, and measured as
//! precision); a rule Bazel reaches that cambium does not is unsoundness, and
//! fails the change set.
//!
//! ## Why the whole change set and not a sample
//!
//! The oracle used to sample k members of cambium's *direct* set and ask for
//! their `rdeps`. That is a check of cambium's reverse walk given the seeds it
//! chose, and it is blind to the interesting half of the problem: a path
//! cambium never seeded at all. `tools/build_defs/repo/utils.bzl` is listed in
//! the `srcs` of fifteen filegroups in `bazel/bazel`; when a `.bzl` only ever
//! went to the load closure, none of those filegroups was direct, none was in
//! cambium's answer, and (because the roots were drawn from cambium's own
//! direct set) no query the oracle made could have named them. Seeding from
//! the *changed paths* instead makes the oracle independent of cambium's
//! seeding decisions, which is the only way it can catch one being wrong.
//!
//! ## Repository-rule inputs: the second root source
//!
//! A changed path's own label is not enough. A path that is an input of a
//! repository rule (a `patches` entry, a `build_file`, a lock file a rule
//! reads, a `local_repository` directory) is not a dependency of anything in
//! the query graph, so `rdeps` of it is empty and "recall 1.0" on such a step
//! proves nothing. Two real under-reports passed five box runs that way:
//! xla's `third_party/llvm/*.patch` (inputs of `@llvm-raw`, consumed only
//! through `@llvm-project`, generated from it by a module extension; run B
//! steps 13, 29 and 181 answered 0 of 0) and envoy's WORKSPACE-era
//! `bazel/pgv.patch` (issue #28).
//!
//! So the oracle has a second root source, and it does not come from
//! cambium's tables: Bazel's own repository marker files
//! (`<output_base>/external/@<canonical>.marker`, parsed in
//! [`super::repo_inputs`]). A marker lists every file, directory listing and
//! directory tree the repository rule read, and it is what Bazel itself
//! consults to decide a refetch, so it is the closest thing to ground truth
//! there is. A repository with a changed path among its recorded inputs is
//! dirty; so, to a fixpoint, is every repository whose marker records a file
//! of a dirty one. For each dirty repository `R` the roots gain
//! `@@R//...:*`: a refetch re-evaluates all of `R`, and a patch can change
//! any file of it, so what Bazel could rebuild is everything in `//...` that
//! reaches any target of `R`. `rdeps(//..., …)` already restricts that to
//! what the universe reaches. The question stays one question:
//!
//! ```text
//! kind(rule, rdeps(//..., set(<changed paths' labels> @@R1//...:* @@R2//...:* …)))
//! ```
//!
//! asked in two halves (the changed paths' labels, then the repository
//! patterns), whose union is the answer because `rdeps` distributes over
//! `set`. `roots` keeps its meaning (the changed paths' labels);
//! [`RdepsResult::repository_inputs`] records which repositories were dirty,
//! why, the patterns, and how many main-repository rules only they reached.
//! A pattern Bazel will not evaluate (a repository that is not defined at
//! head) is bisected out like any root and lands in `skipped_roots`, or in
//! `unresolved` when `--keep_going` absorbed it.
//!
//! **When the markers are read.** The oracle runs with the checkout at head,
//! after cambium's own run for the step. It reads the markers twice: before
//! its first query, and again after the changed paths' half, and a
//! repository's inputs are the union of the two reads. The record keeps each
//! read's count (`markers_first`, `markers_second`).
//!
//! - The first read sees what the last fetch or up-to-date check wrote. On a
//!   carried walk that is mostly the previous step's oracle, whose
//!   `rdeps(//...)` universe checked every repository `//...` reaches at this
//!   step's *base*; but cambium's own queries at head come between, and any
//!   repository they refetched already holds head's inputs. So the first
//!   read holds base's inputs only where nothing since has touched them,
//!   which is what can catch a patch deleted at head (the head marker no
//!   longer names it); it is not a guarantee.
//! - The second read follows a query whose universe is the transitive closure
//!   of `//...` at *head*, which makes Bazel check every repository that
//!   universe reaches against its marker and refetch (rewriting the marker)
//!   any that is out of date. So every repository the answer could come
//!   through has a marker current at head.
//! - When no changed path has a label (every one is outside every package),
//!   the changed paths' half runs no query, there is no second read, and the
//!   first read is all there is; a repository first fetched at head behind
//!   such a step is not seen.
//!
//! **Could a marker mislead?** A repository added at head has no marker
//! from the first read; the second read has it once the head universe
//! fetched it. A marker written for another revision and never checked
//! since can name a changed path the step's own definition does not read:
//!
//! - If the repository is not defined at head (a WORKSPACE-era repository on
//!   an output base that also serves bzlmod revisions, such as envoy's
//!   `@base_pip3`), its pattern fails to resolve and lands in `failed`; if
//!   `//...` does not reach it, its pattern reaches nothing. Either way it
//!   adds no rule, and the summary counts the step as idle, not rooted.
//! - If the repository is still defined and still reached at head, but its
//!   definition stopped reading the path some revisions ago and nothing
//!   checked it at this step's base, the union roots it wholesale although
//!   Bazel going from base to head would not refetch it: a miss the oracle
//!   invents. It needs a marker older than the base, so the first step of a
//!   window or a resume, a step after one `--oracle-sample` skipped, or an
//!   output base shared across revisions or agents. No instance is known in
//!   the current windows; a snapshot at base (below) would remove it.
//!
//! **What the markers do not see.** Three routes make Bazel refetch or
//! re-evaluate a repository and leave the step as vacuous as it was:
//!
//! - A repository never fetched on this output base has no marker at all.
//!   After the walk's cold build and one `rdeps(//...)` that is none the
//!   universe reaches.
//! - A module extension's file inputs are recorded with the extension's
//!   evaluation (the lockfile's `recordedFileInputs`), not in the markers of
//!   the repositories it generates. xla's `pip.parse(requirements_lock =
//!   "//:requirements_lock_3_11.txt")` reads the lock file, and no marker on
//!   `output/xla` names it; a step that edits only a requirements lock is
//!   0 of 0 still. At envoy's bzlmod pin `@rules_python++pip+base_pip3`'s
//!   marker names only rules_python templates, and the one marker naming
//!   `tools/base/requirements.txt` is the stale WORKSPACE-era `@base_pip3`
//!   above, which fails to resolve.
//! - A change to a repository's own definition changes its rule key (the
//!   marker's first line), not a recorded input. xla's `LLVM_COMMIT` bump in
//!   `third_party/llvm/workspace.bzl` refetches `@llvm-raw` and
//!   `@llvm-project`, and nothing here roots them; the changed `.bzl`'s own
//!   label reaches only rules that list it in `srcs`. An LLVM integrate that
//!   also edits a patch is rooted through the patch.
//!
//! And `rdeps` follows no load edges, so a main-repository package that only
//! `load`s a `.bzl` from a rooted repository is not reached from its
//! pattern; the changed paths' half has the same blind spot, and the load
//! oracle covers in-tree `.bzl` files only.
//!
//! The follow-up that closes the second and third routes without leaving
//! Bazel's own records: snapshot the markers at base, before cambium's run,
//! and root every repository whose rule key or recorded digests differ after
//! the head universe. That catches a definition change and a generated
//! repository whose attributes a module extension changed, and it replaces
//! the union, so the invented miss above goes too.
//!
//! ## Batching
//!
//! `rdeps(//..., set(a b c))` is the union of the individual answers, so the
//! labels go out in batches and the number of Bazel invocations is
//! `ceil(n / batch)` rather than `n`. A batch is capped both by label count
//! ([`MAX_BATCH_LABELS`]) and by the byte length of the expression
//! ([`MAX_EXPR_BYTES`]), because the expression travels on the command line and
//! a change set that renames a thousand files would otherwise overrun
//! `ARG_MAX`. When a batch fails outright it is bisected down to the single
//! label that Bazel would not evaluate, so one bad label costs its own answer
//! and not its neighbours'.
//!
//! ## Deleted files
//!
//! A file deleted between base and head cannot be named at head: `//pkg:gone.cc`
//! is not a target there and the query would fail. Those paths are taken from
//! the **base** index instead (its `file -> consumers` rows say which rules
//! read the file before it went away) and each such consumer must be in
//! cambium's answer, or its package must be (a rule deleted along with its
//! source is not in anyone's answer at head, and asking for the label itself
//! would fail a change set for being right). "Its package" is the affected
//! set's packages *or* cambium's `dirty_packages`, which is the only spelling
//! a wholly-deleted package has: the BUILD file is gone, every rule in it is
//! tombstoned, and no affected label carries the package name. That escape
//! hatch is offered only to a consumer that no longer exists at head; one
//! that survives in another package must still appear by label, however dirty
//! its package is. The base index is not always in hand; when it is not, the
//! deleted half is skipped and [`RdepsResult::deleted_checked`] says so.
//!
//! ## The escape hatch
//!
//! [`RdepsOracle::sample`] caps how many source labels are queried, seeded
//! and sorted so a failure reproduces from the run's seed. It exists for
//! subjects whose change sets are enormous; `0` is the full oracle and is what
//! every recorded number must come from. The walk always passes `0`
//! (`BazelChecks::oracle_k`); its `--oracle-sample` samples steps, not roots.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use cambium_bazel::{query_word, Bazel};

use super::repo_inputs::{self, Changes, DirtyRepository, Markers};
use crate::tools::bazel_query;
use cambium_classify::PackageSet;
use cambium_index::IndexRef;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// Most source labels in one `rdeps` query.
pub const MAX_BATCH_LABELS: usize = 500;

/// Most bytes of query expression in one invocation. macOS caps a process's
/// whole argument block at 256 KiB and Bazel's own flags need room, so this
/// leaves an order of magnitude of headroom rather than sailing close.
pub const MAX_EXPR_BYTES: usize = 96 * 1024;

/// What the oracle found.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RdepsResult {
    /// The source labels the query asked about, sorted.
    pub roots: Vec<String>,
    /// True when `--oracle-sample` capped the roots below the change set.
    #[serde(default)]
    pub sampled: bool,
    /// How many Bazel invocations the roots cost.
    #[serde(default)]
    pub batches: usize,
    /// The main-repository rules Bazel reached from the roots.
    ///
    /// Not serialised: on a large subject this is most of the repository, and
    /// what a reader needs from a stored result is the sizes and the failures,
    /// both of which are here.
    #[serde(default, skip_serializing)]
    pub oracle_set: BTreeSet<String>,
    /// `oracle_set.len()`, which survives serialisation.
    #[serde(default)]
    pub oracle_labels: usize,
    /// `|oracle_set ∩ cambium's affected set|`. The numerator of recall.
    #[serde(default)]
    pub matched: usize,
    /// Rules the oracle reached that cambium's answer does not contain, plus
    /// any unsatisfied consumer of a deleted file. Unsoundness; empty means
    /// pass.
    pub missing: Vec<String>,
    /// The `@repo//...` half of Bazel's answer.
    ///
    /// `rdeps(//..., …)` takes `//...` as the *universe expression*, and Bazel
    /// expands a universe to its transitive closure, so the answer contains
    /// external targets that depend on a root, and on envoy that is most of it
    /// (157 of the 173 labels on one change set, all `@quiche//...`). cambium
    /// indexes `//...` by construction and can never contain them, so counting
    /// them as unsoundness would measure the vocabulary difference the
    /// `bazeldiff_extra_external` class already names on the tool side. They
    /// are kept here so the number is visible rather than silently dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_external: Vec<String>,
    /// The subset of [`RdepsResult::missing`] that Bazel reaches only by
    /// leaving `//...` and coming back.
    ///
    /// This is a real cambium miss and it fails the change set; the field says
    /// *why*, so a failure that is the known external-route limitation is
    /// distinguishable from one that is a walk bug. envoy has two instances at
    /// the pin:
    ///
    /// ```text
    /// somepath(//distribution/dockerhub:readme_json, //changelogs:changelogs)
    ///   //distribution/dockerhub:readme_json
    ///   @envoy_repo//:project
    ///   //changelogs:changelogs
    /// ```
    ///
    /// and `//source/common/quic:*` -> `@quiche//:*` ->
    /// `@envoy//source/common/quic/platform:*` (envoy ships quiche's `BUILD`
    /// file and points its `label_flag`s back at main-repo platform impls).
    /// cambium stops at the first external node, so the reverse walk never
    /// climbs back.
    ///
    /// A miss that only the repository-input roots reached is here too,
    /// without a probe: it depends on a target of a repository whose
    /// recorded inputs changed and on no changed path's own label.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_route: Vec<String>,
    /// How many of [`RdepsResult::missing`] the route probe never asked Bazel
    /// about.
    ///
    /// The probe is capped at [`MAX_ROUTE_PROBES`] labels and skips any
    /// `somepath` Bazel would not evaluate, so `external_route` is a subset of
    /// the labels that were *probed*, not of `missing`. Without this count a
    /// reader subtracting one from the other reads a truncated probe as a set
    /// of unexplained misses, which is how the four xla steps of
    /// docs/XLA_MAIN_REPO_MISSES.md were filed: 48 missing, 25 probed, all 25
    /// external routes, and the 23 the probe never reached recorded nowhere.
    /// Unexplained is `missing - external_route - route_unprobed`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub route_unprobed: usize,
    /// Whether the deleted half of the change set could be checked at all: it
    /// needs the base index, which the caller does not always have.
    #[serde(default)]
    pub deleted_checked: bool,
    /// Deleted paths that had a package at base and a label the base index
    /// knew.
    #[serde(default)]
    pub deleted_paths: usize,
    /// Rules that consumed a deleted file at base. Each must be in cambium's
    /// answer, or its package must be.
    #[serde(default)]
    pub deleted_consumers: usize,
    /// The consumers that were neither. A subset of
    /// [`RdepsResult::missing`], repeated so a failure says which half it
    /// came from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_missing: Vec<String>,
    /// Changed paths that belong to no package at head, and so have no label
    /// to ask about. Not a failure: a file outside every package cannot be a
    /// rule's input.
    #[serde(default)]
    pub unowned_paths: usize,
    /// Labels whose batch failed even after bisection, and were skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_roots: Vec<String>,
    /// At least one batch came back from a `--keep_going` query that exited 3:
    /// Bazel could not load everything (an external repository that will not
    /// fetch on this machine, say) but answered for what did load. The oracle
    /// is a *lower bound* on what cambium must contain, so a partial answer is
    /// still sound to check against: it just proves less.
    #[serde(default)]
    pub partial: bool,
    /// How many batches came back partial.
    #[serde(default)]
    pub partial_batches: usize,
    /// What Bazel said it could not resolve while answering, sorted, the
    /// first [`MAX_UNRESOLVED`] of them: `no such package`, `no such
    /// target`, a repository whose fetch failed
    /// ([`bazel_query::unresolved_labels`]). This is what `partial` *is*:
    /// under `--keep_going` every target that transitively depends on one of
    /// these is dropped from the universe, and the answer is a lower bound
    /// by exactly that much. Empty when the answer was whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
    /// How many distinct unresolved labels there were, before the cap.
    #[serde(default)]
    pub unresolved_total: usize,
    /// Roots the oracle could not answer for because their package did not
    /// load: Bazel skipped the root for a reason other than "no such target"
    /// (the package failed to load, or a repository its loads reach would
    /// not fetch), or the root is unresolved and a `BUILD` file on disk at
    /// head sits between its package and the file, so the head index gave
    /// it the wrong package (the file's own package is missing from the
    /// index, as a tombstoned one is). For such a root the answer is not
    /// empty but unknown, and recall 1.0 over the rest proves nothing about
    /// it. A step with any is oracle-blind, which is not a pass
    /// ([`RdepsResult::blind`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blind_roots: Vec<String>,
    /// Every pattern a batch skipped, with Bazel's reason. Not serialised:
    /// what a reader needs is `blind_roots`, which is read from it.
    #[serde(default, skip)]
    pub skipped_patterns: BTreeMap<String, String>,
    /// The second root source: repositories with a changed path among their
    /// recorded inputs, from Bazel's marker files. `None` when no output base
    /// was given, so the markers were not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_inputs: Option<RepositoryRoots>,
    /// The oracle's whole wall for this step, queries and marker reads
    /// included.
    #[serde(default)]
    pub wall_ms: u64,
}

/// What the repository-input half of the oracle found and cost.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryRoots {
    /// Marker files parsed, over both reads.
    pub markers: usize,
    /// Marker files the first read parsed, before the oracle's first query.
    #[serde(default)]
    pub markers_first: usize,
    /// Marker files the second read parsed, after the changed paths' half;
    /// `None` when that half ran no query, so there was no second read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markers_second: Option<usize>,
    /// Repositories whose marker would not parse; their inputs are unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    /// The dirty repositories and why, sorted by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<DirtyRepository>,
    /// The `@@R//...:*` patterns queried, one per dirty repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    /// The patterns Bazel would not evaluate even alone (a repository not
    /// defined at head, such as a WORKSPACE-era marker left on an output
    /// base that now serves bzlmod revisions). A subset of `roots`, also
    /// in [`RdepsResult::skipped_roots`]; they rooted nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// Main-repository rules the patterns reached that the changed paths'
    /// labels did not: how much of `oracle_labels` this half contributed.
    #[serde(default)]
    pub labels_added: usize,
    /// Targets of the rooted repositories themselves in Bazel's answer and
    /// not in cambium's. They are roots, not reverse dependencies, so they
    /// are counted here and left out of `missing_external`.
    #[serde(default)]
    pub own_labels: usize,
    /// Both marker reads.
    #[serde(default)]
    pub read_ms: u64,
    /// The pattern queries.
    #[serde(default)]
    pub query_ms: u64,
}

impl RdepsResult {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }

    /// The oracle could not see what some changed path reaches
    /// ([`RdepsResult::blind_roots`]). A blind step found nothing missing
    /// only because it could not look, so it is neither sound nor a miss.
    pub fn blind(&self) -> bool {
        !self.blind_roots.is_empty()
    }

    /// Fraction of the oracle's answer cambium reproduces. 1.0 is required.
    ///
    /// Over the main repository only, for the reason
    /// [`RdepsResult::missing_external`] gives, and over the queried half
    /// only: a deleted file's consumers are satisfied by their package as well
    /// as by their label, which is a containment question and not a ratio. An
    /// unsatisfied one still fails the change set through
    /// [`RdepsResult::missing`].
    pub fn recall(&self) -> f64 {
        if self.oracle_labels == 0 {
            return 1.0;
        }
        self.matched as f64 / self.oracle_labels as f64
    }
}

/// The oracle's configuration.
#[derive(Clone, Debug)]
pub struct RdepsOracle {
    /// Cap on how many source labels are queried. 0 (the default) is the
    /// full change set, which is what the methodology specifies.
    pub sample: usize,
    pub seed: u64,
    /// Most labels in one Bazel invocation.
    pub max_labels: usize,
    /// Most bytes of query expression in one Bazel invocation.
    pub max_expr_bytes: usize,
}

impl Default for RdepsOracle {
    fn default() -> Self {
        Self {
            sample: 0,
            seed: 0,
            max_labels: MAX_BATCH_LABELS,
            max_expr_bytes: MAX_EXPR_BYTES,
        }
    }
}

/// What the oracle needs to know about one change set, beyond the Bazel handle.
pub struct OracleInputs<'a> {
    /// The index at head; its package list decides which changed paths have a
    /// label, and the workspace on disk decides which still exist.
    pub head: IndexRef<'a>,
    /// The index at base, for the deleted half. `None` skips it.
    pub base: Option<IndexRef<'a>>,
    /// Every path git reports changed between base and head.
    pub changed: &'a BTreeSet<String>,
    /// cambium's affected set: rule labels, the thing under test.
    pub cambium: &'a BTreeSet<String>,
    /// The packages cambium marked dirty for this change set.
    ///
    /// Only the deleted half reads it, and it is the *whole* answer for a
    /// package deleted outright: every rule in it is tombstoned, so cambium
    /// rightly emits no label carrying that package, and the package name
    /// survives nowhere else in the report. Without this the escape hatch in
    /// `deleted_consumers` ("the rule, or its package") is unsatisfiable
    /// by construction for exactly the case it was written for.
    pub dirty_packages: &'a BTreeSet<String>,
    /// The output base the queries run on, whose `external/@*.marker` files
    /// are the second root source. `None` skips it and leaves
    /// [`RdepsResult::repository_inputs`] unset.
    pub output_base: Option<&'a Path>,
    /// The changed paths that were added or deleted, for a marker's
    /// `DIRENTS` inputs (a directory listing changes only then). `None`
    /// matches a listing on any changed child.
    pub added_or_deleted: Option<&'a BTreeSet<String>>,
}

impl RdepsOracle {
    pub fn new(sample: usize, seed: u64) -> Self {
        Self {
            sample,
            seed,
            ..Self::default()
        }
    }

    /// Runs the oracle over one change set.
    pub fn run(&self, bazel: &Bazel, inputs: &OracleInputs<'_>) -> Result<RdepsResult> {
        let started = std::time::Instant::now();
        let mut result = RdepsResult::default();
        let ws = bazel.workspace();

        let (mut roots, deleted) = roots_of(inputs, ws, &mut result);
        if self.sample > 0 && roots.len() > self.sample {
            roots = sample(&roots, self.sample, self.seed);
            result.sampled = true;
        }
        result.roots = roots.clone();

        // The first marker read: before any query of ours has touched the
        // server, so base's inputs where the last check was at base.
        let first = inputs.output_base.map(|output_base| {
            let read_at = std::time::Instant::now();
            let markers = Markers::read(output_base, ws);
            FirstRead {
                output_base,
                markers,
                read_ms: read_at.elapsed().as_millis() as u64,
            }
        });

        let mut answer = BTreeSet::new();
        let mut unresolved: BTreeSet<String> = BTreeSet::new();
        for batch in batches(&roots, self.max_labels, self.max_expr_bytes) {
            self.run_batch(bazel, batch, &mut answer, &mut unresolved, &mut result);
        }

        let (own, repo_only) = match first {
            Some(first) => {
                let half = self.repository_half(
                    bazel,
                    inputs,
                    first,
                    &mut answer,
                    &mut unresolved,
                    &mut result,
                );
                result.repository_inputs = Some(half.roots);
                (half.own, half.repo_only)
            }
            None => (Vec::new(), BTreeSet::new()),
        };

        partition(answer, inputs.cambium, &mut result);
        // A rooted repository's own targets are roots, not targets that
        // depend on one: counted, and kept out of the external listing.
        if let Some(r) = result.repository_inputs.as_mut() {
            let before = result.missing_external.len();
            result
                .missing_external
                .retain(|l| !own.iter().any(|repo| repo_inputs::label_in_repo(l, repo)));
            r.own_labels = before - result.missing_external.len();
        }
        result.blind_roots = blind_roots(&result.roots, &result.skipped_patterns, &unresolved, ws);
        if !result.blind_roots.is_empty() {
            tracing::warn!(
                roots = result.blind_roots.len(),
                first = %result.blind_roots[0],
                "the oracle could not resolve changed paths whose package did not load; \
                 this step is oracle-blind"
            );
        }
        result.unresolved_total = unresolved.len();
        result.unresolved = unresolved.into_iter().take(MAX_UNRESOLVED).collect();
        // A miss only the repository patterns reached depends on a target of
        // a dirty repository and on no changed path's label, so every path
        // from it to a root runs through that repository: an external route
        // by construction, and no `somepath` is spent on it. Probing them
        // cost 4 s each on xla (the pattern is re-evaluated per probe), 200
        // probes on a step whose 9559 misses were all of this kind.
        let (via_repo, rest): (Vec<String>, Vec<String>) = result
            .missing
            .iter()
            .cloned()
            .partition(|l| repo_only.contains(l));
        let (routes, unprobed) = external_routes(bazel, &result.roots, &rest);
        result.external_route = via_repo.into_iter().chain(routes).collect();
        result.external_route.sort();
        result.route_unprobed = unprobed;

        if let Some(base) = inputs.base {
            result.deleted_checked = true;
            let deleted_missing = deleted_consumers(
                base,
                inputs.head,
                &deleted,
                inputs.cambium,
                inputs.dirty_packages,
                &mut result,
            );
            // They gate the change set exactly as a queried miss does.
            result.missing.extend(deleted_missing.iter().cloned());
            result.deleted_missing = deleted_missing;
            result.missing.sort();
            result.missing.dedup();
        }
        result.wall_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// The repository-input half: a second marker read, after a universe
    /// that checked every repository `//...` reaches at head, then one
    /// `rdeps` root per repository whose recorded inputs a changed path
    /// touched. What those roots reach joins `answer`.
    fn repository_half(
        &self,
        bazel: &Bazel,
        inputs: &OracleInputs<'_>,
        first: FirstRead<'_>,
        answer: &mut BTreeSet<String>,
        unresolved: &mut BTreeSet<String>,
        result: &mut RdepsResult,
    ) -> RepositoryHalf {
        let ws = bazel.workspace();
        let markers_first = first.markers.by_repo.len();
        let read_at = std::time::Instant::now();
        // With no changed path's label to ask about, no query has run yet,
        // the markers are as the first read found them, and a second read
        // would only repeat it.
        let (markers, markers_second) = match result.batches {
            0 => (first.markers, None),
            _ => {
                let after = Markers::read(first.output_base, ws);
                let n = after.by_repo.len();
                (first.markers.union(after), Some(n))
            }
        };
        let repos = repo_inputs::dirty(
            &markers,
            &Changes {
                changed: inputs.changed,
                added_or_deleted: inputs.added_or_deleted,
            },
            ws,
        );
        let read_ms = first.read_ms + read_at.elapsed().as_millis() as u64;
        let patterns: Vec<String> = repos
            .iter()
            .map(|r| repo_inputs::root_pattern(&r.name))
            .collect();
        let query_at = std::time::Instant::now();
        let mut from_repos = BTreeSet::new();
        for batch in batches(&patterns, self.max_labels, self.max_expr_bytes) {
            self.run_batch(bazel, batch, &mut from_repos, unresolved, result);
        }
        let query_ms = query_at.elapsed().as_millis() as u64;
        let failed: Vec<String> = patterns
            .iter()
            .filter(|p| result.skipped_roots.contains(p))
            .cloned()
            .collect();
        let repo_only: BTreeSet<String> = from_repos
            .iter()
            .filter(|l| !is_external(l) && !answer.contains(*l))
            .cloned()
            .collect();
        let own = repos.iter().map(|r| r.name.clone()).collect();
        answer.extend(from_repos);
        RepositoryHalf {
            roots: RepositoryRoots {
                markers: markers.by_repo.len(),
                markers_first,
                markers_second,
                unreadable: markers.unreadable.into_iter().collect(),
                repositories: repos,
                roots: patterns,
                failed,
                labels_added: repo_only.len(),
                own_labels: 0,
                read_ms,
                query_ms,
            },
            repo_only,
            own,
        }
    }

    fn run_batch(
        &self,
        bazel: &Bazel,
        batch: &[String],
        answer: &mut BTreeSet<String>,
        unresolved: &mut BTreeSet<String>,
        result: &mut RdepsResult,
    ) {
        result.batches += 1;
        match query_rdeps(bazel, batch) {
            Ok(Batch {
                labels,
                partial,
                unresolved: u,
                skipped,
            }) => {
                answer.extend(labels);
                if partial {
                    result.partial = true;
                    result.partial_batches += 1;
                    unresolved.extend(u);
                    result.skipped_patterns.extend(skipped);
                }
            }
            Err(e) if batch.len() > 1 => {
                // One label Bazel will not evaluate must not cost its
                // neighbours their answer, and finding it by bisection costs
                // log(n) extra invocations rather than n.
                tracing::warn!(
                    labels = batch.len(),
                    error = format!("{e:#}"),
                    "rdeps batch failed; bisecting"
                );
                let (a, b) = batch.split_at(batch.len() / 2);
                self.run_batch(bazel, a, answer, unresolved, result);
                self.run_batch(bazel, b, answer, unresolved, result);
            }
            Err(e) => {
                tracing::warn!(
                    root = ?batch.first(),
                    error = format!("{e:#}"),
                    "rdeps query failed for a single label; skipping it"
                );
                result.skipped_roots.extend(batch.iter().cloned());
            }
        }
    }
}

/// Does the path still exist in the head workspace?
///
/// git's diff says a path changed, not whether it survived; the head checkout
/// is on disk and is the cheapest authority. A path that is gone gets the
/// base-index treatment instead of a label Bazel would reject.
fn exists_at_head(workspace: &Path, path: &str) -> bool {
    workspace.join(path).exists()
}

/// `//pkg:rel`, the source-file target a workspace-relative path names.
pub(crate) fn source_label(package: &str, path: &str) -> String {
    let rel = match package.is_empty() {
        true => path,
        false => path
            .strip_prefix(package)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(path),
    };
    format!("//{package}:{rel}")
}

/// The package half of `//pkg:name`, for the deleted-file containment check.
pub(crate) fn package_of_label(label: &str) -> Option<&str> {
    label.strip_prefix("//")?.split_once(':').map(|(p, _)| p)
}

/// Consumers of the deleted paths that cambium's answer does not account for.
///
/// A rule that read a file which no longer exists must show up somewhere in
/// cambium's answer: either as itself, or, when the rule was deleted along
/// with its source and so exists at neither end, as its package. Requiring the
/// label alone would fail a change set for being right about a target that is
/// gone.
///
/// "Its package" means the affected set *or* `dirty_packages`. A package
/// deleted in its entirety only ever has the latter: cambium tombstones every
/// rule in it and correctly reports none of them, so reading packages off the
/// affected labels alone would fail every wholly-deleted package. That was the
/// whole of sourcegraph's and apollo's change-set failures on the 2026-09-08
/// box run.
///
/// The package spelling is an escape hatch for a rule that cannot be named at
/// head, so it is only offered to a consumer that is *gone* at head (absent
/// from the head index, or tombstoned in it). A consumer that survives in
/// another package must still appear by label, however dirty its package is:
/// otherwise a deletion anywhere would excuse a missed reverse edge
/// everywhere.
fn deleted_consumers(
    base: IndexRef<'_>,
    head: IndexRef<'_>,
    deleted: &[&String],
    cambium: &BTreeSet<String>,
    dirty_packages: &BTreeSet<String>,
    result: &mut RdepsResult,
) -> Vec<String> {
    let packages = PackageSet::from_index(base);
    // Both spellings of "cambium accounted for this package": a surviving
    // affected target inside it, or the package itself in the dirty set. A
    // package deleted outright has only the second: all its rules are
    // tombstoned and none can appear in the affected set.
    let cambium_packages: BTreeSet<&str> = cambium
        .iter()
        .filter_map(|l| package_of_label(l))
        .chain(dirty_packages.iter().map(String::as_str))
        .collect();
    let mut consumers = BTreeSet::new();
    for path in deleted {
        let Some(pkg) = packages.owner_of(path) else {
            continue;
        };
        let label = source_label(pkg, path);
        let Some(id) = base.id_of(&label) else {
            continue;
        };
        result.deleted_paths += 1;
        for &c in base.file_consumers(id) {
            if base.is_rule(c) && !base.is_tombstone(c) {
                consumers.insert(base.label(c).to_string());
            }
        }
    }
    result.deleted_consumers = consumers.len();
    consumers
        .into_iter()
        .filter(|l| {
            !cambium.contains(l)
                && !(gone_at_head(head, l)
                    && package_of_label(l).is_some_and(|p| cambium_packages.contains(p)))
        })
        .collect()
}

/// The oracle's roots for the changed paths, sorted and deduplicated, and
/// the paths that no longer exist at head (the deleted half's input).
///
/// A path's roots are every label it has: its main-repo one, and the one it
/// has inside an in-tree repository whose packages the index holds. A
/// `.proto` under envoy's `api/` has no main-repo owner (`//...` stops at the
/// boundary) and only the second, and an oracle rooted at main-repo labels
/// alone never asked Bazel what it reaches: recall read 1.0 on every such
/// step while the answer under test could have missed all of them. A path
/// with neither is counted in [`RdepsResult::unowned_paths`].
fn roots_of<'a>(
    inputs: &OracleInputs<'a>,
    ws: &Path,
    result: &mut RdepsResult,
) -> (Vec<String>, Vec<&'a String>) {
    let packages = PackageSet::from_index(inputs.head);
    let mut deleted: Vec<&String> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for path in inputs.changed {
        if !exists_at_head(ws, path) {
            deleted.push(path);
            continue;
        }
        let mut found = false;
        if let Some(pkg) = packages.owner_of(path) {
            roots.push(source_label(pkg, path));
            found = true;
        }
        for label in cambium_engine::in_tree_labels(inputs.head, &packages, path) {
            roots.push(label);
            found = true;
        }
        if !found {
            result.unowned_paths += 1;
        }
    }
    roots.sort();
    roots.dedup();
    (roots, deleted)
}

/// The roots the oracle could not answer for ([`RdepsResult::blind_roots`]),
/// sorted.
///
/// Box run F's xla walk is the case: at step 12 the index tombstoned the 17
/// packages that load `@llvm-project//mlir:tblgen.bzl`, and on steps 41,
/// 45, 193 and 195 the changed file's package was one of them, so
/// [`roots_of`] rooted it at the nearest surviving ancestor
/// (`//xla/python/ifrt:ir/ifrt_ir_loaded_executable_test_lib.cc` for a file
/// of `xla/python/ifrt/ir`). Bazel answered `no such target` for that label,
/// the oracle's answer was empty, and each step scored recall 1.0 while
/// cambium missed what the file reached.
pub(crate) fn blind_roots(
    roots: &[String],
    skipped: &BTreeMap<String, String>,
    unresolved: &BTreeSet<String>,
    ws: &Path,
) -> Vec<String> {
    let deleted = declared_deleted(ws);
    roots
        .iter()
        .filter(|r| {
            let why = skipped.get(*r);
            let package_failed = why.is_some_and(|w| !w.starts_with("no such target"));
            let unanswered = why.is_some() || unresolved.contains(*r);
            package_failed || (unanswered && crosses_a_package(ws, r, &deleted))
        })
        .cloned()
        .collect()
}

/// Whether a main-repository label `//pkg:a/b/c` names a file under a
/// directory of `pkg` that is a package of its own at head (it has a
/// `BUILD` or `BUILD.bazel` file on disk and is not in `deleted`, the
/// workspace's `--deleted_packages`), so the label spells the file with the
/// wrong package. An external label answers no.
pub(crate) fn crosses_a_package(ws: &Path, label: &str, deleted: &BTreeSet<String>) -> bool {
    let Some((pkg, rel)) = label.strip_prefix("//").and_then(|l| l.split_once(':')) else {
        return false;
    };
    let mut dir = ws.join(pkg);
    let mut name = pkg.to_string();
    let Some((dirs, _file)) = rel.rsplit_once('/') else {
        return false;
    };
    for seg in dirs.split('/') {
        dir.push(seg);
        if !name.is_empty() {
            name.push('/');
        }
        name.push_str(seg);
        if (dir.join("BUILD").is_file() || dir.join("BUILD.bazel").is_file())
            && !deleted.contains(&name)
        {
            return true;
        }
    }
    false
}

/// Every package a `--deleted_packages` flag names in the workspace's
/// `.bazelrc` or a `%workspace%/` file it imports, under any command or
/// config.
///
/// A directory so named has a `BUILD` file on disk and is not a package, so
/// a changed file under it resolves to the enclosing package and the oracle's
/// empty answer for it is the true one (envoy's generated `repo.bazelrc`
/// declares its `mobile/` test packages this way). Reading every command
/// and config over-counts, which can only make a root not blind, the
/// behaviour before blind roots existed.
pub(crate) fn declared_deleted(ws: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut todo = vec![ws.join(".bazelrc")];
    while let Some(path) = todo.pop() {
        if !seen.insert(path.clone()) || seen.len() > 32 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let mut words = line.split_whitespace().peekable();
            match words.peek().copied() {
                Some("import") | Some("try-import") => {
                    words.next();
                    if let Some(rel) = words.next().and_then(|w| w.strip_prefix("%workspace%/")) {
                        todo.push(ws.join(rel));
                    }
                    continue;
                }
                Some(w) if w.starts_with('#') => continue,
                _ => {}
            }
            while let Some(w) = words.next() {
                let value = match w.strip_prefix("--deleted_packages") {
                    Some(v) if v.starts_with('=') => Some(&v[1..]),
                    Some("") => words.next(),
                    _ => None,
                };
                for p in value.into_iter().flat_map(|v| v.split(',')) {
                    let p = p.trim_matches(['"', '\'']).trim_start_matches("//");
                    let p = p.strip_prefix("%workspace%/").unwrap_or(p);
                    if !p.is_empty() {
                        out.insert(p.trim_end_matches('/').to_string());
                    }
                }
            }
        }
    }
    out
}

/// The first marker read, taken before the oracle's own queries touch the
/// server.
struct FirstRead<'a> {
    output_base: &'a Path,
    markers: Markers,
    read_ms: u64,
}

/// What [`RdepsOracle::repository_half`] found: the record, the
/// main-repository rules only its roots reached, and the dirty repositories
/// by name.
struct RepositoryHalf {
    roots: RepositoryRoots,
    repo_only: BTreeSet<String>,
    own: Vec<String>,
}

/// Is this rule absent from head, or tombstoned there?
///
/// The condition on the package-spelling escape hatch: only a rule that no
/// longer exists may be credited to its package rather than to itself.
fn gone_at_head(head: IndexRef<'_>, label: &str) -> bool {
    match head.id_of(label) {
        Some(id) => head.is_tombstone(id),
        None => true,
    }
}

/// A label outside the main repository, in either spelling Bazel uses.
pub fn is_external(label: &str) -> bool {
    label.starts_with('@') || label.starts_with("//external:")
}

/// How many unresolved labels a result keeps by name. envoy at its pin has
/// eleven distinct ones; the cap is against a repository whose every
/// external dependency failed to fetch, where the count is the story.
pub const MAX_UNRESOLVED: usize = 32;

/// How many missing labels the external-route probe is willing to ask Bazel
/// about.
///
/// The cap was 25, on a reading of `somepath` cost that was the *first* query
/// against a cold server rather than the steady state. Measured on xla at
/// `1725bd8f8409` against a warm output base: the first probe 12.7 s, and
/// 0.25, 0.37, 0.43 s for the next three. At a third of a second a probe, the
/// 23 labels the old cap skipped on that step were worth about eight seconds,
/// on a step that had already failed.
///
/// So the cap is a guard against a pathological missing set, not a budget: it
/// is only reached when a change set is failing, and a run failing hundreds of
/// labels a step has a bigger problem than its diagnosis being slow. 200 is
/// about a minute in that case. Whatever it skips is counted in
/// [`RdepsResult::route_unprobed`], so a reader is never left guessing whether
/// a label went unexplained or unasked.
pub const MAX_ROUTE_PROBES: usize = 200;

/// How many roots the probe's `somepath` target set carries. The full oracle's
/// root list can be thousands of labels; a probe is a hint about *why* a label
/// is missing, so it takes the first batch's worth and no more.
pub(crate) const MAX_ROUTE_ROOTS: usize = MAX_BATCH_LABELS;

/// Splits the oracle's answer into the halves the verdict is made of.
///
/// The main repository is what recall is over, so `oracle_set`, `matched` and
/// `missing` are all main-repo labels and `missing_external` is the rest.
/// That split does not move: `missing` is what fails a change set
/// (`RdepsResult::ok`), and `external_route` is a filter of `missing`, so it
/// stays a subset of it however wide cambium's own vocabulary grows. A
/// repository whose targets cambium does not reach shows up as a main-repo
/// rule in `missing` and fails the step; it never leaves the verdict through
/// `missing_external`.
fn partition(answer: BTreeSet<String>, cambium: &BTreeSet<String>, result: &mut RdepsResult) {
    let (main, external): (BTreeSet<String>, BTreeSet<String>) =
        answer.into_iter().partition(|l| !is_external(l));
    result.oracle_labels = main.len();
    result.matched = main.intersection(cambium).count();
    result.missing = main.difference(cambium).cloned().collect();
    result.missing_external = external
        .into_iter()
        .filter(|l| !cambium.contains(l))
        .collect();
    result.oracle_set = main;
}

/// Which of `missing` Bazel reaches only by leaving `//...`.
///
/// One `somepath(label, set(r1 r2 …))` per label: Bazel returns *a* shortest
/// path, and if that path contains an `@repo//...` node then the only way from
/// this target down to a root is through an external repository, which is
/// exactly the edge cambium's `//...`-only index does not carry. A path with
/// no external node means something else is wrong and the label really is
/// unexplained, the safe direction, since either way it fails the change set.
///
/// Returns the routes and the number of `missing` labels no probe was run for:
/// the ones past [`MAX_ROUTE_PROBES`], and the ones whose `somepath` Bazel
/// would not evaluate. A label counted there is not evidence of anything, and
/// keeping the count is what stops a truncated probe from reading as a set of
/// unexplained misses.
fn external_routes(bazel: &Bazel, roots: &[String], missing: &[String]) -> (Vec<String>, usize) {
    if roots.is_empty() || missing.is_empty() {
        return (Vec::new(), missing.len());
    }
    let head = &roots[..roots.len().min(MAX_ROUTE_ROOTS)];
    let union = label_set(head);
    let mut out = Vec::new();
    let mut unprobed = missing.len().saturating_sub(MAX_ROUTE_PROBES);
    for label in missing.iter().take(MAX_ROUTE_PROBES) {
        // A probe that will not run says nothing either way, so it is counted
        // as unprobed rather than left looking like an unexplained miss.
        let expr = format!("somepath({label}, {union})");
        let Ok((path, _partial)) = bazel_query::labels(bazel, &expr, "somepath") else {
            unprobed += 1;
            continue;
        };
        if path.iter().any(|l| is_external(l)) {
            out.push(label.clone());
        }
    }
    (out, unprobed)
}

/// `skip_serializing_if` for a count that is zero on every sound change set.
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A deterministic sample of at most `k` labels, sorted so the JSON is stable.
pub(crate) fn sample(labels: &[String], k: usize, seed: u64) -> Vec<String> {
    let mut all: Vec<String> = labels.to_vec();
    all.sort();
    all.dedup();
    if k == 0 {
        return Vec::new();
    }
    if all.len() <= k {
        return all;
    }
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut idx: Vec<usize> = (0..all.len()).collect();
    idx.shuffle(&mut rng);
    let mut chosen: Vec<usize> = idx.into_iter().take(k).collect();
    chosen.sort_unstable();
    chosen.into_iter().map(|i| all[i].clone()).collect()
}

/// Splits the roots into query-sized batches: at most `max_labels` each, and
/// at most `max_bytes` of expression each. A single label longer than the byte
/// cap still gets its own batch: dropping it would silently weaken the
/// oracle, and Bazel will say if it is really too long.
pub(crate) fn batches(roots: &[String], max_labels: usize, max_bytes: usize) -> Vec<&[String]> {
    let max_labels = max_labels.max(1);
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for i in 0..roots.len() {
        let cost = roots[i].len() + 1;
        let full = i > start && (i - start >= max_labels || bytes + cost > max_bytes);
        if full {
            out.push(&roots[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < roots.len() {
        out.push(&roots[start..]);
    }
    out
}

/// `set(a b c)`: Bazel's literal set, which needs no operator precedence.
///
/// The individual labels still need [`cambium_bazel::query_word`]: `set` takes
/// query *words*, and the lexer ends a word at a `+`, so one unquoted
/// `//bazel:libc++` makes the whole batch a syntax error rather than one
/// missing label.
pub(crate) fn label_set<S: AsRef<str>>(labels: &[S]) -> String {
    format!(
        "set({})",
        labels
            .iter()
            .map(|l| query_word(l.as_ref()))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// `kind(rule, rdeps(//..., set(a b c)))`: every rule that reaches any of the
/// changed files.
///
/// The `kind(rule, ...)` wrapper is not cosmetic. Bazel's `rdeps` answer
/// includes *file* targets (the generated `//lib:lib.out` of a genrule, the
/// `//pkg:BUILD.bazel` of a package, and the roots themselves) while cambium's
/// walk emits rules only (`cambium_index::walk` filters on `NodeKind::Rule`).
/// Without the wrapper every changed file in the change set reads as a target
/// Bazel reached and cambium missed, which is the one thing this oracle is
/// supposed to detect, and the failure would be pure vocabulary. The same
/// restriction is applied to the other tools' answers
/// (`ToolRun::restrict_to_rules`), so all three are compared over one
/// universe.
pub(crate) fn rdeps_expression(roots: &[String]) -> String {
    format!("kind(rule, rdeps(//..., {}))", label_set(roots))
}

/// One batch's answer, and whether Bazel had to give up on part of the graph.
struct Batch {
    labels: BTreeSet<String>,
    partial: bool,
    unresolved: BTreeSet<String>,
    skipped: BTreeMap<String, String>,
}

/// Runs one batched `rdeps` query.
///
/// **Exit 3 is accepted.** Under `--keep_going` Bazel exits 3 when it finished
/// but hit errors: on a real repository that is routine: `bazel/bazel` at
/// these pins references external repositories that cannot be fetched on macOS
/// (a GraalVM distribution with no macos-x64 build, an RBE config that needs
/// Docker), so the transitive closure of `//...` cannot be completed. Bazel
/// still prints the reverse dependencies of everything that *did* load. Since
/// the oracle only ever asserts "cambium must contain all of this", a smaller
/// answer weakens the check but never makes it wrong, so the batch is kept and
/// flagged [`RdepsResult::partial`]. Any other non-zero exit is a real
/// failure and sends the batch to the bisection in
/// [`RdepsOracle::run_batch`].
fn query_rdeps(bazel: &Bazel, roots: &[String]) -> Result<Batch> {
    let expr = rdeps_expression(roots);
    let a = bazel_query::answer(bazel, &expr, &crate::tools::tail(&expr, 1))?;
    if a.partial {
        tracing::warn!(
            roots = roots.len(),
            labels = a.labels.len(),
            unresolved = a.unresolved.len(),
            first = a.unresolved.iter().next().map(String::as_str).unwrap_or(""),
            "the rdeps query exited 3 under --keep_going; the oracle answer for this batch is a lower bound"
        );
    }
    Ok(Batch {
        labels: a.labels,
        partial: a.partial,
        unresolved: a.unresolved,
        skipped: a.skipped,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cambium_index::{IndexBuilder, PackageUpdate, TargetRow};

    use super::*;

    fn labels(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("//p:t{i}")).collect()
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_verdict_is_the_main_repository_and_external_route_is_a_subset_of_it() {
        let answer = set(&[
            "//a:a",
            "//b:b",
            "//c:c",
            "@quiche//:quiche",
            "//external:thing",
        ]);
        let cambium = set(&["//a:a"]);
        let mut r = RdepsResult::default();
        partition(answer, &cambium, &mut r);

        assert_eq!(
            r.oracle_labels, 3,
            "recall is over the main repository only"
        );
        assert_eq!(r.matched, 1);
        assert_eq!(r.missing, vec!["//b:b".to_string(), "//c:c".to_string()]);
        assert_eq!(
            r.missing_external,
            vec![
                "//external:thing".to_string(),
                "@quiche//:quiche".to_string()
            ],
            "the other vocabulary is recorded, never in the verdict"
        );
        assert!(!r.ok(), "a main-repo miss fails the change set");

        // What `external_routes` returns is a filter of `missing`, so the
        // invariant the gate rests on is that every route is a label the step
        // already failed on. A repository cambium cannot reach must surface as
        // a main-repo rule here, not as a second verdict of its own.
        r.external_route = r.missing.iter().take(1).cloned().collect();
        assert!(
            r.external_route.iter().all(|l| r.missing.contains(l)),
            "external_route ⊆ missing"
        );
        assert!(
            r.external_route.iter().all(|l| !is_external(l)),
            "a route is the main-repo rule at its end, not the repository it went through"
        );

        // And the verdict is `missing` alone: neither half of the external
        // bookkeeping can pass or fail a step by itself.
        let mut clean = RdepsResult::default();
        partition(set(&["@quiche//:quiche"]), &BTreeSet::new(), &mut clean);
        assert!(
            clean.ok(),
            "an answer Bazel reached only outside //... is not a miss"
        );
        assert_eq!(clean.missing_external.len(), 1);
    }

    static EMPTY: BTreeSet<String> = BTreeSet::new();

    /// The oracle's inputs for a step with no base index, no dirty packages
    /// and no output base: what most of these tests vary is the changed
    /// paths and cambium's answer.
    fn inputs<'a>(
        head: IndexRef<'a>,
        changed: &'a BTreeSet<String>,
        cambium: &'a BTreeSet<String>,
    ) -> OracleInputs<'a> {
        OracleInputs {
            head,
            base: None,
            changed,
            cambium,
            dirty_packages: &EMPTY,
            output_base: None,
            added_or_deleted: None,
        }
    }

    /// A stub Bazel whose every `query` prints `stdout` and exits `code`.
    ///
    /// `bazel_query::labels` spawns the binary rather than going through
    /// [`cambium_bazel::BazelRunner`], so the seam a test can take here is the
    /// executable itself.
    fn bazel_answering(dir: &Path, code: i32, stdout: &str) -> Bazel {
        let binary = dir.join("stub-bazel");
        cambium_testkit::executable_script(
            &binary,
            &format!("printf '%s' {}\nexit {code}", shell_word(stdout)),
        );
        Bazel::at(dir.to_path_buf(), binary.to_str())
    }

    /// `'...'` with any embedded quote closed and reopened, so the stub script
    /// prints the text and nothing else.
    fn shell_word(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }

    #[test]
    fn a_truncated_route_probe_is_counted_and_not_left_looking_unexplained() {
        // More misses than the probe will ask about, all of them the same
        // external route. This is the shape the four xla steps of
        // docs/XLA_MAIN_REPO_MISSES.md had under the old cap of 25: 48
        // missing, 25 probed, and nothing said about the other 23.
        let missing = labels(MAX_ROUTE_PROBES + 23);
        let roots = vec!["//xla/tsl/platform:macros.h".to_string()];
        let dir = cambium_testkit::scratch();
        // The xla shape: `//xla/service:lockable` reaches
        // `//xla/tsl/platform:macros.h` only through `@tsl`, whose BUILD files
        // name `@xla//xla/...` back at the main repository.
        let bazel = bazel_answering(
            dir.path(),
            0,
            "//xla/service:lockable\n@tsl//tsl/profiler/lib:traceme\n//xla/tsl/platform:macros.h\n",
        );

        let (routes, unprobed) = external_routes(&bazel, &roots, &missing);

        assert_eq!(routes.len(), MAX_ROUTE_PROBES, "the probe is capped");
        assert_eq!(unprobed, 23, "and says how many it never asked about");
        assert_eq!(
            missing.len() - routes.len() - unprobed,
            0,
            "nothing is left unexplained: unexplained is missing - routes - unprobed"
        );
    }

    #[test]
    fn a_probe_bazel_will_not_run_is_unprobed_rather_than_unexplained() {
        let missing = labels(3);
        let roots = vec!["//a:a.cc".to_string()];

        let dir = cambium_testkit::scratch();
        let (routes, unprobed) =
            external_routes(&bazel_answering(dir.path(), 1, ""), &roots, &missing);
        assert!(routes.is_empty());
        assert_eq!(
            unprobed, 3,
            "a query that failed is not evidence either way"
        );

        // A path with no external node is the one case that really is
        // unexplained, and it is counted in neither column.
        let inside = cambium_testkit::scratch();
        let (routes, unprobed) = external_routes(
            &bazel_answering(inside.path(), 0, "//a:t\n//a:a.cc\n"),
            &roots,
            &missing,
        );
        assert!(routes.is_empty());
        assert_eq!(unprobed, 0);

        // No roots to walk down to is not a probe either.
        let none = cambium_testkit::scratch();
        let (routes, unprobed) =
            external_routes(&bazel_answering(none.path(), 0, ""), &[], &missing);
        assert!(routes.is_empty());
        assert_eq!(unprobed, 3);
    }

    #[test]
    fn the_sample_is_seeded_and_capped() {
        let all = labels(50);
        let a = sample(&all, 5, 7);
        assert_eq!(a.len(), 5);
        assert_eq!(a, sample(&all, 5, 7), "the same seed picks the same labels");
        assert_ne!(a, sample(&all, 5, 8));
        assert!(a.windows(2).all(|w| w[0] < w[1]), "sorted: {a:?}");
        assert_eq!(sample(&all, 100, 7).len(), 50, "k above the population");
        assert!(sample(&all, 0, 7).is_empty(), "k = 0 samples nothing");
    }

    #[test]
    fn a_path_becomes_the_source_label_of_its_package() {
        assert_eq!(source_label("lib", "lib/a.cc"), "//lib:a.cc");
        assert_eq!(source_label("lib", "lib/sub/a.cc"), "//lib:sub/a.cc");
        assert_eq!(source_label("", "a.cc"), "//:a.cc");
        assert_eq!(package_of_label("//lib:sub/a.cc"), Some("lib"));
        assert_eq!(package_of_label("//:a"), Some(""));
        assert_eq!(package_of_label("@ext//:a"), None);
    }

    #[test]
    fn roots_go_out_as_a_literal_set() {
        assert_eq!(
            rdeps_expression(&["//a:a.cc".into(), "//b:b.cc".into()]),
            "kind(rule, rdeps(//..., set(//a:a.cc //b:b.cc)))"
        );
    }

    #[test]
    fn a_root_the_lexer_would_split_is_quoted_inside_the_set() {
        // One `+` label used to make the whole batch a syntax error, which
        // reads as an oracle that reached nothing.
        assert_eq!(
            rdeps_expression(&["//bazel:libc++".into(), "//b:b.cc".into()]),
            "kind(rule, rdeps(//..., set(\"//bazel:libc++\" //b:b.cc)))"
        );
    }

    #[test]
    fn batching_respects_both_the_count_and_the_byte_cap() {
        let all = labels(10);
        assert_eq!(batches(&all, 100, 1 << 20).len(), 1, "one batch when small");
        let by_count = batches(&all, 3, 1 << 20);
        assert_eq!(by_count.len(), 4);
        assert_eq!(by_count[0].len(), 3);
        assert_eq!(by_count[3].len(), 1);
        // Every label is in exactly one batch, in order.
        let flat: Vec<String> = by_count.iter().flat_map(|b| b.to_vec()).collect();
        assert_eq!(flat, all);
        // "//p:t0" is 6 bytes, so 15 bytes holds two of them and not three.
        let by_bytes = batches(&all, 100, 15);
        assert!(by_bytes.iter().all(|b| b.len() == 2), "{by_bytes:?}");
        assert!(batches(&[], 10, 10).is_empty());
        // A label alone over the cap still gets asked about.
        assert_eq!(batches(&labels(1), 10, 1).len(), 1);
    }

    /// The three kinds of unanswered root, on a tree where `xla/python/ifrt`
    /// and `xla/python/ifrt/ir` are both packages. Box run F step 41's root
    /// spelled a file of `ir` under `ifrt` (the head index had tombstoned
    /// `ir`): blind. A root skipped because its package did not load: blind.
    /// A patch no rule names, `no such target`: its empty answer is the true
    /// one.
    #[test]
    fn a_root_whose_package_did_not_load_is_blind_and_an_undeclared_file_is_not() {
        let ws = tempfile::tempdir().unwrap();
        for d in ["xla/python/ifrt", "xla/python/ifrt/ir", "third_party/llvm"] {
            std::fs::create_dir_all(ws.path().join(d)).unwrap();
            std::fs::write(ws.path().join(d).join("BUILD"), "").unwrap();
        }
        let crossing = "//xla/python/ifrt:ir/ifrt_ir_loaded_executable_test_lib.cc".to_string();
        let failed = "//xla/mosaic:dialect/tpu/tpu_ops.cc".to_string();
        let patch = "//third_party/llvm:generated.patch".to_string();
        let fine = "//xla/python/ifrt:array.cc".to_string();
        let roots = vec![crossing.clone(), failed.clone(), patch.clone(), fine];
        let skipped: BTreeMap<String, String> = [
            (
                failed.clone(),
                "error loading package 'xla/mosaic': Encountered error while reading \
                 extension file 'mlir/tblgen.bzl': no such package \
                 '@@+llvm_extension+llvm-project//mlir': ..."
                    .to_string(),
            ),
            (
                patch.clone(),
                "no such target '//third_party/llvm:generated.patch': target \
                 'generated.patch' not declared in package 'third_party/llvm'"
                    .to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let unresolved: BTreeSet<String> = [crossing.clone(), patch].into_iter().collect();
        assert_eq!(
            blind_roots(&roots, &skipped, &unresolved, ws.path()),
            vec![crossing, failed]
        );
    }

    #[test]
    fn a_label_crosses_a_package_only_through_a_directory_with_a_build_file() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("a/b/c")).unwrap();
        std::fs::write(ws.path().join("a/BUILD"), "").unwrap();
        std::fs::write(ws.path().join("a/b/c/BUILD.bazel"), "").unwrap();
        let none = BTreeSet::new();
        assert!(crosses_a_package(ws.path(), "//a:b/c/x.cc", &none));
        assert!(!crosses_a_package(ws.path(), "//a:b/x.cc", &none));
        assert!(!crosses_a_package(ws.path(), "//a:x.cc", &none));
        assert!(!crosses_a_package(ws.path(), "@r//a:b/c/x.cc", &none));
        let deleted: BTreeSet<String> = ["a/b/c".to_string()].into_iter().collect();
        assert!(!crosses_a_package(ws.path(), "//a:b/c/x.cc", &deleted));
    }

    /// A directory the workspace declares deleted has a `BUILD` file and is
    /// not a package, so a root under it is not blind. envoy's shape: the
    /// flag lives in a generated file the root `.bazelrc` try-imports.
    #[test]
    fn a_declared_deleted_directory_is_not_a_package_to_cross() {
        let ws = tempfile::tempdir().unwrap();
        for d in [
            "mobile/test/common",
            "mobile/test/common/integration",
            "x/y",
        ] {
            std::fs::create_dir_all(ws.path().join(d)).unwrap();
            std::fs::write(ws.path().join(d).join("BUILD"), "").unwrap();
        }
        std::fs::write(
            ws.path().join(".bazelrc"),
            "# --deleted_packages=commented/out\n\
             build --jobs=4\n\
             try-import %workspace%/repo.bazelrc\n\
             try-import %workspace%/missing.bazelrc\n",
        )
        .unwrap();
        std::fs::write(
            ws.path().join("repo.bazelrc"),
            "common --deleted_packages=//mobile/test/common/integration,other/pkg\n\
             query:ci --deleted_packages x/y\n",
        )
        .unwrap();
        let deleted = declared_deleted(ws.path());
        assert_eq!(
            deleted.iter().map(String::as_str).collect::<Vec<_>>(),
            ["mobile/test/common/integration", "other/pkg", "x/y"]
        );
        let root = "//mobile/test/common:integration/client_test.cc".to_string();
        let unresolved: BTreeSet<String> = [root.clone()].into_iter().collect();
        assert!(blind_roots(&[root], &BTreeMap::new(), &unresolved, ws.path()).is_empty());
    }

    #[test]
    fn recall_is_one_when_the_oracle_is_empty_and_a_fraction_otherwise() {
        let r = RdepsResult {
            oracle_labels: 4,
            matched: 3,
            ..Default::default()
        };
        assert_eq!(r.recall(), 0.75);
        assert_eq!(RdepsResult::default().recall(), 1.0);
    }

    // ---- `run` end to end over a stub Bazel ----
    //
    // `Bazel::at` runs whatever binary it is given, so a script that ignores
    // its arguments and prints a fixed answer ([`bazel_answering`]) is a
    // complete Bazel as far as the oracle is concerned. That is enough to
    // drive `run` end to end: what is under test here is which labels are
    // asked about and how the answer is scored, not Bazel.

    /// The scenario of 11ae8e0: `defs/defs.bzl` is listed in the `srcs` of a
    /// filegroup in its own package. Bazel's `rdeps` names the filegroup; if
    /// cambium's answer has only the packages that *load* the file, the
    /// filegroup is missed.
    fn bzl_filegroup_index() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[
            PackageUpdate {
                package: "defs".into(),
                targets: vec![
                    TargetRow::rule(
                        "//defs:bzl_srcs",
                        "filegroup",
                        vec!["//defs:defs.bzl".into()],
                    ),
                    TargetRow::source_file("//defs:defs.bzl"),
                ],
                ..Default::default()
            },
            PackageUpdate {
                package: "app".into(),
                targets: vec![TargetRow::rule("//app:app", "genrule", vec![])],
                ..Default::default()
            },
        ]);
        b.finish("head")
    }

    #[cfg(unix)]
    #[test]
    fn a_filegroup_over_a_changed_bzl_is_missed_when_cambium_leaves_it_out() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("defs")).unwrap();
        std::fs::write(ws.join("defs/defs.bzl"), "def f():\n    pass\n").unwrap();
        // Bazel reaches the filegroup from the changed `.bzl`; the loading
        // package's rule is reached through the load, which `rdeps` over a
        // source label does not see, so the oracle names only the filegroup.
        let bazel = bazel_answering(ws, 0, "//defs:bzl_srcs\n");
        let idx = bzl_filegroup_index();
        let changed = set(&["defs/defs.bzl"]);

        // cambium answers with the loading package only: the miss.
        let loads_only = set(&["//app:app"]);
        let r = RdepsOracle::default()
            .run(&bazel, &inputs(idx.view(), &changed, &loads_only))
            .unwrap();
        assert_eq!(
            r.roots,
            vec!["//defs:defs.bzl"],
            "the changed path is the root"
        );
        assert_eq!(r.missing, vec!["//defs:bzl_srcs"]);
        assert_eq!(r.recall(), 0.0);
        assert!(
            !r.ok(),
            "a filegroup Bazel reaches and cambium does not fails"
        );

        // With the file's consumers in the answer, the same oracle passes.
        let with_consumers = set(&["//app:app", "//defs:bzl_srcs"]);
        let r = RdepsOracle::default()
            .run(&bazel, &inputs(idx.view(), &changed, &with_consumers))
            .unwrap();
        assert!(r.missing.is_empty());
        assert_eq!(r.recall(), 1.0);
        assert!(r.ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_in_no_package_is_counted_and_not_queried() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("docs")).unwrap();
        std::fs::write(ws.join("docs/README.md"), "hi\n").unwrap();
        let bazel = bazel_answering(ws, 0, "\n");
        let idx = bzl_filegroup_index();
        let changed = set(&["docs/README.md"]);
        let r = RdepsOracle::default()
            .run(&bazel, &inputs(idx.view(), &changed, &EMPTY))
            .unwrap();
        assert!(r.roots.is_empty());
        assert_eq!(r.unowned_paths, 1);
        assert_eq!(r.batches, 0, "no roots, no query");
        assert!(r.ok());
    }

    #[cfg(unix)]
    #[test]
    fn external_labels_are_split_off_and_left_out_of_recall() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("defs")).unwrap();
        std::fs::write(ws.join("defs/defs.bzl"), "x\n").unwrap();
        let bazel = bazel_answering(ws, 0, "//defs:bzl_srcs\n@quiche//:x\n//external:y\n");
        let idx = bzl_filegroup_index();
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &inputs(
                    idx.view(),
                    &set(&["defs/defs.bzl"]),
                    &set(&["//defs:bzl_srcs"]),
                ),
            )
            .unwrap();
        assert!(r.missing.is_empty(), "{:?}", r.missing);
        assert_eq!(r.missing_external, vec!["//external:y", "@quiche//:x"]);
        assert_eq!(r.recall(), 1.0, "the external half is not counted");
        assert!(r.ok());
    }

    /// The wholly-deleted package: `lib/BUILD` and `lib/gone.cc` go away
    /// together, `//lib:core` consumed the file from inside the package, and
    /// `//app:bin` consumed it from outside and survives.
    fn base_index_with_a_deleted_package() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[
            PackageUpdate {
                package: "lib".into(),
                targets: vec![
                    TargetRow::rule("//lib:core", "cc_library", vec!["//lib:gone.cc".into()]),
                    TargetRow::source_file("//lib:gone.cc"),
                ],
                ..Default::default()
            },
            PackageUpdate {
                package: "app".into(),
                targets: vec![TargetRow::rule(
                    "//app:bin",
                    "cc_binary",
                    vec!["//lib:gone.cc".into()],
                )],
                ..Default::default()
            },
        ]);
        b.finish("base")
    }

    /// Head after that deletion: `app` is still a package, `lib` is not.
    fn head_index_without_the_deleted_package() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[PackageUpdate {
            package: "app".into(),
            targets: vec![TargetRow::rule("//app:bin", "cc_binary", vec![])],
            ..Default::default()
        }]);
        b.finish("head")
    }

    /// A package deleted outright is accounted for by the dirty set alone,
    /// and only for the rules that went with it.
    ///
    /// This is the bazel `//third_party/py/{concurrent,dataclasses,mock}`
    /// shape and sourcegraph's `//internal/search/smartsearch`: the BUILD
    /// file is gone, every rule in the package is tombstoned, and cambium's
    /// answer carries the package name nowhere but `dirty_packages`. The
    /// consumer that lives in another package and survives at head is *not*
    /// excused by its package being dirty: a deletion anywhere must not
    /// excuse a missed reverse edge everywhere.
    #[cfg(unix)]
    #[test]
    fn a_wholly_deleted_package_is_satisfied_by_the_dirty_set_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // Neither `lib/BUILD` nor `lib/gone.cc` is on disk at head.
        let bazel = bazel_answering(ws, 0, "\n");
        let head = head_index_without_the_deleted_package();
        let base = base_index_with_a_deleted_package();
        let changed = set(&["lib/BUILD", "lib/gone.cc"]);
        let run = |cambium: &BTreeSet<String>, dirty: &BTreeSet<String>| {
            RdepsOracle::default()
                .run(
                    &bazel,
                    &OracleInputs {
                        head: head.view(),
                        base: Some(base.view()),
                        changed: &changed,
                        cambium,
                        dirty_packages: dirty,
                        output_base: None,
                        added_or_deleted: None,
                    },
                )
                .unwrap()
        };
        let none = BTreeSet::new();

        // Both consumers are unaccounted for with nothing in hand.
        let r = run(&none, &none);
        assert!(r.deleted_checked);
        assert_eq!(r.deleted_consumers, 2);
        assert_eq!(r.deleted_missing, vec!["//app:bin", "//lib:core"]);

        // The dirty package accounts for the tombstoned rule inside it, and
        // for nothing else: `//app:bin` still has to appear by label.
        let r = run(&none, &set(&["lib"]));
        assert_eq!(r.deleted_missing, vec!["//app:bin"]);
        assert!(!r.ok());

        // Nor does `app` being dirty excuse it: the rule is alive at head, so
        // the package spelling is not its escape hatch.
        let r = run(&none, &set(&["lib", "app"]));
        assert_eq!(r.deleted_missing, vec!["//app:bin"]);
        assert!(!r.ok());

        // The label does, and then the change set is sound.
        let r = run(&set(&["//app:bin"]), &set(&["lib"]));
        assert!(r.deleted_missing.is_empty());
        assert!(r.missing.is_empty(), "{:?}", r.missing);
        assert!(r.ok());
        assert_eq!(r.recall(), 1.0);
    }

    fn base_index_with_a_consumed_source() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[PackageUpdate {
            package: "lib".into(),
            targets: vec![
                TargetRow::rule("//lib:core", "cc_library", vec!["//lib:gone.cc".into()]),
                TargetRow::source_file("//lib:gone.cc"),
            ],
            ..Default::default()
        }]);
        b.finish("base")
    }

    #[cfg(unix)]
    #[test]
    fn a_deleted_files_consumers_come_from_the_base_index() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // `lib/gone.cc` is not on disk: that is what makes it deleted.
        let bazel = bazel_answering(ws, 0, "\n");
        let head = bzl_filegroup_index();
        let base = base_index_with_a_consumed_source();
        let changed = set(&["lib/gone.cc"]);
        let run = |cambium: &BTreeSet<String>, dirty: &BTreeSet<String>| {
            RdepsOracle::default()
                .run(
                    &bazel,
                    &OracleInputs {
                        head: head.view(),
                        base: Some(base.view()),
                        changed: &changed,
                        cambium,
                        dirty_packages: dirty,
                        output_base: None,
                        added_or_deleted: None,
                    },
                )
                .unwrap()
        };
        let none = BTreeSet::new();

        let r = run(&none, &none);
        assert!(r.roots.is_empty(), "a deleted path cannot be named at head");
        assert!(r.deleted_checked);
        assert_eq!(r.deleted_paths, 1);
        assert_eq!(r.deleted_consumers, 1);
        assert_eq!(r.deleted_missing, vec!["//lib:core"]);
        assert_eq!(r.missing, vec!["//lib:core"]);
        assert!(!r.ok());

        // The label itself satisfies it.
        assert!(run(&set(&["//lib:core"]), &none).ok());
        // So does anything else in its package: the rule may have been deleted
        // with its source, and then it is in nobody's answer at head.
        assert!(run(&set(&["//lib:other"]), &none).ok());
        // And so does the dirty package alone, with an empty affected set.
        //
        // This is the wholly-deleted package: every rule in `lib` is
        // tombstoned, so cambium rightly reports no label carrying the
        // package and `dirty_packages` is the only place its name survives.
        // Before the dirty set was consulted this case could not be satisfied
        // at all, and it failed every such change set on the 2026-09-08
        // box run: `//internal/search/smartsearch` on sourcegraph,
        // `//modules/drivers/lidar/vanjeelidar` on apollo.
        assert!(run(&none, &set(&["lib"])).ok());
        // A different package does not satisfy it.
        assert!(!run(&none, &set(&["other"])).ok());
        // And with no base index the half is skipped rather than failed.
        let r = RdepsOracle::default()
            .run(&bazel, &inputs(head.view(), &changed, &EMPTY))
            .unwrap();
        assert!(!r.deleted_checked);
        assert!(r.ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_sample_escape_hatch_caps_the_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("defs")).unwrap();
        let mut changed = BTreeSet::new();
        for i in 0..10 {
            std::fs::write(ws.join(format!("defs/f{i}.bzl")), "x\n").unwrap();
            changed.insert(format!("defs/f{i}.bzl"));
        }
        let bazel = bazel_answering(ws, 0, "\n");
        let idx = bzl_filegroup_index();
        let step = inputs(idx.view(), &changed, &changed);
        let full = RdepsOracle::default().run(&bazel, &step).unwrap();
        assert_eq!(full.roots.len(), 10);
        assert!(!full.sampled);
        let capped = RdepsOracle::new(3, 7).run(&bazel, &step).unwrap();
        assert_eq!(capped.roots.len(), 3);
        assert!(capped.sampled);
    }

    /// xla at step 29 of run B, reduced: `third_party/llvm/build.patch` is
    /// the only changed path, it is a source file of `//third_party/llvm`
    /// that no rule consumes, and it is a `patches` entry of `@llvm-raw`,
    /// from which `@llvm-project` is generated.
    fn patch_step_index() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[
            PackageUpdate {
                package: "third_party/llvm".into(),
                targets: vec![TargetRow::source_file("//third_party/llvm:build.patch")],
                ..Default::default()
            },
            PackageUpdate {
                package: "xla/service".into(),
                targets: vec![TargetRow::rule("//xla/service:cpu", "cc_library", vec![])],
                ..Default::default()
            },
        ]);
        b.finish("head")
    }

    /// A Bazel that reaches nothing from a changed path's label and, from a
    /// repository pattern, a main-repo rule and a target of the repository
    /// itself: what `rdeps` says on that step.
    #[cfg(unix)]
    fn patch_step_bazel(dir: &Path) -> Bazel {
        let bin = dir.join("stub-bazel");
        cambium_testkit::executable_script(
            &bin,
            "case \"$*\" in\n\
             *'//...:*'*) printf '%s\\n' //xla/service:cpu @llvm-project//llvm:Support ;;\n\
             *) ;;\n\
             esac",
        );
        Bazel::at(dir.to_path_buf(), Some(bin.to_str().unwrap()))
    }

    #[cfg(unix)]
    #[test]
    fn a_patch_only_step_is_rooted_at_the_repositories_its_marker_names() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("third_party/llvm")).unwrap();
        std::fs::write(ws.path().join("third_party/llvm/build.patch"), "x").unwrap();
        let ob = tempfile::tempdir().unwrap();
        let ext = ob.path().join("external");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(
            ext.join("@+third_party_ext+llvm-raw.marker"),
            "e2ec\nFILE:@@//third_party/llvm/build.patch 9d52\nFILE:@@//third_party/llvm/toolchains.patch dba1\n",
        )
        .unwrap();
        std::fs::write(
            ext.join("@+llvm_extension+llvm-project.marker"),
            "9abd\nDIRENTS:@@+third_party_ext+llvm-raw// f1a3\nFILE:@@+third_party_ext+llvm-raw//llvm/CMakeLists.txt c1e3\nREPO_MAPPING:+third_party_ext+llvm-raw,llvm-raw +third_party_ext+llvm-raw\n",
        )
        .unwrap();
        std::fs::write(ext.join("@unrelated.marker"), "k\nFILE:@@//other.patch 1\n").unwrap();

        let bazel = patch_step_bazel(ws.path());
        let idx = patch_step_index();
        let changed = set(&["third_party/llvm/build.patch"]);
        let run = |cambium: &BTreeSet<String>, ob: Option<&Path>| {
            RdepsOracle::default()
                .run(
                    &bazel,
                    &OracleInputs {
                        head: idx.view(),
                        base: None,
                        changed: &changed,
                        cambium,
                        dirty_packages: &BTreeSet::new(),
                        output_base: ob,
                        added_or_deleted: None,
                    },
                )
                .unwrap()
        };

        // Without the markers: the vacuous answer every box run recorded.
        let blind = run(&BTreeSet::new(), None);
        assert_eq!(blind.roots, vec!["//third_party/llvm:build.patch"]);
        assert_eq!(blind.oracle_labels, 0);
        assert!(blind.ok(), "0 of 0 passes, and proves nothing");
        assert!(blind.repository_inputs.is_none());

        // With them: both repositories, the patterns, and the miss.
        let r = run(&BTreeSet::new(), Some(ob.path()));
        assert_eq!(
            r.roots,
            vec!["//third_party/llvm:build.patch"],
            "`roots` keeps its meaning: the changed paths' labels"
        );
        let ri = r.repository_inputs.as_ref().expect("the markers were read");
        assert_eq!(ri.markers, 3);
        assert_eq!(ri.markers_first, 3);
        assert_eq!(ri.markers_second, Some(3), "the stub fetches nothing new");
        assert!(ri.failed.is_empty());
        assert_eq!(
            ri.repositories
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            vec!["+llvm_extension+llvm-project", "+third_party_ext+llvm-raw"]
        );
        assert_eq!(
            ri.roots,
            vec![
                "@@+llvm_extension+llvm-project//...:*",
                "@@+third_party_ext+llvm-raw//...:*"
            ]
        );
        assert_eq!(ri.labels_added, 1);
        assert_eq!(ri.own_labels, 1, "@llvm-project//llvm:Support is a root");
        assert!(r.missing_external.is_empty(), "{:?}", r.missing_external);
        assert_eq!(r.batches, 2, "the changed paths' half, then the patterns");
        assert_eq!(r.oracle_labels, 1);
        assert_eq!(r.missing, vec!["//xla/service:cpu"]);
        assert!(!r.ok(), "the engine that missed the route fails");
        assert_eq!(
            r.external_route,
            vec!["//xla/service:cpu"],
            "reached only through the repository: a route, with no probe spent"
        );
        assert_eq!(r.route_unprobed, 0);

        // And the engine that covers it passes.
        let covered = run(&set(&["//xla/service:cpu"]), Some(ob.path()));
        assert!(covered.ok());
        assert_eq!(covered.recall(), 1.0);

        // A step that touches no recorded input roots no repository.
        let quiet_changed = set(&["xla/service/a.cc"]);
        let quiet = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    output_base: Some(ob.path()),
                    ..inputs(idx.view(), &quiet_changed, &EMPTY)
                },
            )
            .unwrap();
        let qi = quiet.repository_inputs.as_ref().unwrap();
        assert!(qi.repositories.is_empty() && qi.roots.is_empty());
    }

    /// A marker left on the output base by a revision that defined the
    /// repository differently (envoy's WORKSPACE-era `@base_pip3` on a base
    /// that now serves bzlmod revisions) names a changed path, but its
    /// pattern does not resolve at head: it is recorded as `failed` and
    /// roots nothing.
    #[cfg(unix)]
    #[test]
    fn a_pattern_for_a_repository_not_defined_at_head_is_recorded_as_failed() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("third_party/llvm")).unwrap();
        std::fs::write(ws.path().join("third_party/llvm/build.patch"), "x").unwrap();
        let ob = tempfile::tempdir().unwrap();
        let ext = ob.path().join("external");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(
            ext.join("@base_pip3.marker"),
            "k\nFILE:@@//third_party/llvm/build.patch 1\n",
        )
        .unwrap();
        let bin = ws.path().join("stub-bazel");
        cambium_testkit::executable_script(
            &bin,
            "case \"$*\" in\n\
             *'@@base_pip3//...:*'*) echo \"ERROR: no such repository '@@base_pip3'\" >&2; exit 7 ;;\n\
             *) ;;\n\
             esac",
        );
        let bazel = Bazel::at(ws.path().to_path_buf(), Some(bin.to_str().unwrap()));
        let idx = patch_step_index();
        let changed = set(&["third_party/llvm/build.patch"]);
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    output_base: Some(ob.path()),
                    ..inputs(idx.view(), &changed, &EMPTY)
                },
            )
            .unwrap();
        let ri = r.repository_inputs.as_ref().unwrap();
        assert_eq!(ri.roots, vec!["@@base_pip3//...:*"]);
        assert_eq!(ri.failed, vec!["@@base_pip3//...:*"], "{r:#?}");
        assert_eq!(ri.labels_added, 0);
        assert!(r.skipped_roots.contains(&"@@base_pip3//...:*".to_string()));
    }
}
