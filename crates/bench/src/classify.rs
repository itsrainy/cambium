//! Why two tools disagreed.
//!
//! A raw symmetric difference between cambium's answer and bazel-diff's is not
//! actionable: on a real repository it is thousands of labels, almost all of
//! them explained by a handful of known, *expected* differences in what the
//! two tools model. The spec names seven classes; the real runs turned up four
//! more (`cambium_extra_bzl`, `cambium_extra_deleted`,
//! `cambium_extra_build_glob`, `bazeldiff_extra_external_content`). Nine of the
//! eleven are benign and two fail the run:
//!
//! | class | side | meaning |
//! |---|---|---|
//! | `cambium_extra_select` | cambium only | reached through a `select` branch that is not taken in the current configuration; cambium indexes the unconfigured graph, so it keeps every branch |
//! | `cambium_extra_implicit` | cambium only | reached only through an implicit (`$`-prefixed) attribute edge |
//! | `cambium_extra_external_repo` | cambium only | cambium reached it from an external repository whose contents moved — a `MODULE.bazel` edit, or a main-repo file a repository rule is built from; the hash-diffing tools resolve the repository and see its targets unchanged |
//! | `cambium_extra_bzl` | cambium only | the target's own rule is byte-identical at both revisions, but a `.bzl` its package loads changed, so cambium marked the whole loading package direct |
//! | `cambium_extra_build_glob` | cambium only | reached from a changed `BUILD` file that a `glob(["**"])` filegroup consumes; the file's *content* is an input, so the filegroup and everything embedding it really do change |
//! | `cambium_extra_deleted` | cambium only | reached from a target that existed at base and not at head; cambium marks every consumer of a deleted target direct, the hash-diffing tools cannot see a target that is no longer there |
//! | `cambium_extra_visibility` | cambium only | the target's rule changed only in `visibility`/`tags`, which cannot change its outputs but does move a hash |
//! | `bazeldiff_extra_content` | tool only | a changed file whose content bazel-diff hashes but which no target in the index consumes |
//! | `bazeldiff_extra_external` | tool only | an `@repo//...` label; cambium indexes `//...` only |
//! | `bazeldiff_extra_external_content` | tool only | a `//...` label whose rule, source inputs and `//...` dependencies are all unchanged, and which depends on something external: bazel-diff hashes the external graph, and how much of it loads varies between runs |
//! | `bazeldiff_extra_line_shift` | tool only | the rule is byte-identical at the two revisions and nothing it consumes changed, but its `BUILD` file did: only the rule's *line number* moved, which bazel-diff hashes (`generator_location`) and cambium deliberately does not |
//! | `bazeldiff_extra_workspace_path` | tool only | the rule is byte-identical at the two revisions once each side's own workspace root is normalised away: it embeds the absolute path of the checkout it was queried in, and the tool's base checkout is a different directory from head |
//! | `cambium_missing` | tool only | **fails**: the other tool reached something cambium did not, with no benign explanation |
//! | `unclassified` | cambium only | **fails**: an extra target nothing explains — either a real over-approximation bug or a missing predicate here |
//!
//! The classifier is a pure function of a [`Facts`] oracle so every class can
//! be tested without a Bazel; [`BazelFacts`] is the implementation that
//! actually asks one.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Which tool reported the label that the other did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// cambium reported it and the tool did not.
    Cambium,
    /// The tool reported it and cambium did not.
    Tool,
}

/// The fourteen disagreement classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    CambiumExtraSelect,
    /// An external repository this target names moved.
    CambiumExtraExternalRepo,
    CambiumExtraImplicit,
    CambiumExtraVisibility,
    /// A `.bzl` edit marked the whole loading package direct, but this
    /// target's own rule did not move.
    CambiumExtraBzl,
    /// A dependency of this target was deleted between base and head.
    CambiumExtraDeleted,
    /// Reached from a changed `BUILD` file consumed as a source by a
    /// `glob`bing filegroup.
    CambiumExtraBuildGlob,
    BazeldiffExtraContent,
    BazeldiffExtraExternal,
    /// A `//...` label the tool reported only because an external
    /// dependency of it hashed differently between the two runs.
    BazeldiffExtraExternalContent,
    /// A `//...` label whose rule differs between the tool's two checkouts
    /// only in the absolute workspace path it embeds.
    BazeldiffExtraWorkspacePath,
    /// The rule did not move, its inputs did not move, and nothing it depends
    /// on is affected — but the `BUILD` file it lives in changed, so its line
    /// number did.
    BazeldiffExtraLineShift,
    /// A failure: the other tool found something cambium missed.
    CambiumMissing,
    /// A failure: no predicate explained the disagreement.
    Unclassified,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::CambiumExtraSelect => "cambium_extra_select",
            Class::CambiumExtraExternalRepo => "cambium_extra_external_repo",
            Class::CambiumExtraImplicit => "cambium_extra_implicit",
            Class::CambiumExtraVisibility => "cambium_extra_visibility",
            Class::CambiumExtraBzl => "cambium_extra_bzl",
            Class::CambiumExtraDeleted => "cambium_extra_deleted",
            Class::CambiumExtraBuildGlob => "cambium_extra_build_glob",
            Class::BazeldiffExtraContent => "bazeldiff_extra_content",
            Class::BazeldiffExtraExternal => "bazeldiff_extra_external",
            Class::BazeldiffExtraExternalContent => "bazeldiff_extra_external_content",
            Class::BazeldiffExtraWorkspacePath => "bazeldiff_extra_workspace_path",
            Class::BazeldiffExtraLineShift => "bazeldiff_extra_line_shift",
            Class::CambiumMissing => "cambium_missing",
            Class::Unclassified => "unclassified",
        }
    }

    /// One line for the report's class table: what a label in this class
    /// means, in the same words as this module's own table.
    pub fn meaning(self) -> &'static str {
        match self {
            Class::CambiumExtraSelect => {
                "cambium keeps `select()` branches a configured Bazel query resolves away"
            }
            Class::CambiumExtraExternalRepo => {
                "cambium reached it from an external repository whose contents moved; the \
                 tool resolves the repository and sees its targets unchanged"
            }
            Class::CambiumExtraImplicit => {
                "reached only through an implicit (`$`-prefixed) attribute edge the tool \
                 does not walk"
            }
            Class::CambiumExtraVisibility => {
                "the two rules differ only in `visibility`/`tags`, which cambium's hash ignores"
            }
            Class::CambiumExtraBzl => {
                "the target's own rule is byte-identical, but a `.bzl` its package loads changed"
            }
            Class::CambiumExtraDeleted => {
                "a dependency of this target was deleted between base \
                                           and head"
            }
            Class::CambiumExtraBuildGlob => {
                "reached from a changed `BUILD` file that a `glob()`bing filegroup consumes as a \
                 source"
            }
            Class::BazeldiffExtraContent => {
                "a changed path with no consumer in cambium's head index (content the graph does \
                 not name)"
            }
            Class::BazeldiffExtraExternal => {
                "confined to `@external//...` labels, which cambium does not index and the tools \
                 canonicalise differently"
            }
            Class::BazeldiffExtraExternalContent => {
                "an unchanged `//...` rule that depends on something external; how much of the \
                 external graph loads varies between the tool's runs"
            }
            Class::BazeldiffExtraWorkspacePath => {
                "the rule is identical at both revisions once each checkout's own absolute \
                 workspace root is normalised away"
            }
            Class::BazeldiffExtraLineShift => {
                "an unchanged rule whose `BUILD` file changed around it: only its line number \
                 moved, and cambium's rule hash excludes `generator_location`"
            }
            Class::CambiumMissing => {
                "**fails**: the oracle or a tool reached something cambium did not — \
                 unsoundness"
            }
            Class::Unclassified => {
                "**fails**: no predicate explains the disagreement; needs a new class or a fix"
            }
        }
    }

    /// Whether a label in this class fails the change set.
    pub fn is_failure(self) -> bool {
        matches!(self, Class::CambiumMissing | Class::Unclassified)
    }

    pub const ALL: [Class; 14] = [
        Class::CambiumExtraSelect,
        Class::CambiumExtraExternalRepo,
        Class::CambiumExtraImplicit,
        Class::CambiumExtraVisibility,
        Class::CambiumExtraBzl,
        Class::CambiumExtraDeleted,
        Class::CambiumExtraBuildGlob,
        Class::BazeldiffExtraContent,
        Class::BazeldiffExtraExternal,
        Class::BazeldiffExtraExternalContent,
        Class::BazeldiffExtraWorkspacePath,
        Class::BazeldiffExtraLineShift,
        Class::CambiumMissing,
        Class::Unclassified,
    ];
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The questions the classifier asks about a label. Every one of them is
/// answerable from Bazel plus the head index; none of them needs the tool.
pub trait Facts {
    /// The label names a target outside the main repository (`@repo//...`, or
    /// the older `//external:...`).
    fn is_external(&self, label: &str) -> bool;

    /// Some attribute on the path from the change to this target is a
    /// `select`, so a configured build might not reach it.
    fn reachable_via_select(&self, label: &str) -> bool;

    /// The target is reached from the change only through implicit
    /// (rule-supplied, `$`-prefixed) attributes.
    fn reachable_only_via_implicit(&self, label: &str) -> bool;

    /// Between base and head this target's rule changed only in visibility (or
    /// tags): nothing that can change its outputs.
    fn visibility_only_change(&self, label: &str) -> bool;

    /// cambium reached this target because an external repository it names
    /// moved (`DirectReason::ExternalRepo`), directly or through the reverse
    /// walk.
    ///
    /// cambium's graph stops at the main repository's edge, so it cannot ask
    /// whether `@debian_java_deps//:asm` actually changed: the external stage
    /// says "this repository's *declaration* moved" and every main-repo target
    /// that names a label in it is direct. On `bazel/bazel`'s
    /// `5c47f233..bfd201d1` one `maven.install` version bump, plus the
    /// `//:MODULE.bazel` that `//:extensions.bzl` reads by label, put
    /// sixty-nine targets in cambium's answer that bazel-diff — which resolves
    /// the repositories and hashes their contents — reported as unchanged.
    ///
    /// Over-approximation, not unsoundness, and the price of not fetching
    /// every external repository on every run.
    fn reached_via_external_repo(&self, label: &str) -> bool;

    /// A `.bzl` file changed in this change set and this target's own rule proto is
    /// byte-identical at base and head.
    ///
    /// cambium marks *every* rule in a package whose `.bzl` load closure moved
    /// as direct, because a macro can change what a rule builds without moving
    /// a single byte of its proto (see `engine::direct_set`). The price is
    /// that the rules in the package which the macro does not touch — a
    /// hand-written `config_setting` next to a macro-generated genrule — are
    /// reported too. That is over-approximation, not unsoundness, and on a
    /// real repository it is the single largest source of cambium-side extras.
    fn bzl_closure_only(&self, label: &str) -> bool;

    /// cambium reached this target because something it depended on was
    /// deleted (`DirectReason::DeletedDep`), directly or through the reverse
    /// walk.
    ///
    /// This is the single largest cambium-side class on a real repository's
    /// deletion commits, and it is not an approximation: a target whose dep
    /// vanished genuinely may build differently. bazel-diff and Tango cannot
    /// report it in the same shape because at head the deleted target does not
    /// exist, so only the immediate parent's own hash moves; the rest of the
    /// reverse closure looks untouched to them.
    fn reached_via_deleted_dep(&self, label: &str) -> bool;

    /// cambium reached this target from a changed `BUILD` file that some rule
    /// consumes as a *source*.
    ///
    /// `filegroup(name = "srcs", srcs = glob(["**"]))` — the shape every
    /// package in `bazel/bazel` uses — has its own `BUILD` file among its
    /// inputs, so editing the `BUILD` file changes the filegroup's content
    /// even when no rule proto moves. In `bazel/bazel` that propagates a long
    /// way: `//third_party:srcs` is embedded by
    /// `//src/main/java/.../runtime/commands/license:license`, which the
    /// server binary depends on, so one `third_party/BUILD` edit legitimately
    /// reaches six hundred targets. The hash-diffing tools report the
    /// filegroup itself and stop short of that closure, so the difference is
    /// theirs, not cambium's — but it is expected, and counted rather than
    /// failed.
    fn reached_via_build_file(&self, label: &str) -> bool;

    /// This target's rule proto is identical at base and head once
    /// `visibility` and `tags` are removed: nothing the BUILD files say about
    /// it changed.
    ///
    /// Weaker than [`Facts::visibility_only_change`], which additionally
    /// requires the *hashed* rule to differ — here "nothing changed at all" is
    /// the case of interest.
    fn rule_unchanged_modulo_visibility(&self, label: &str) -> bool;

    /// None of the target's `rule_input` labels that name a file inside the
    /// workspace is in this change set's changed path set.
    fn no_changed_source_inputs(&self, label: &str) -> bool;

    /// None of the target's transitive `//...` dependencies, walked over the
    /// head index, is in cambium's affected set.
    fn no_affected_deps(&self, label: &str) -> bool;

    /// The target depends on at least one external (`@`-prefixed) label —
    /// through the head index, or through an implicit edge the index does not
    /// carry. The implicit half is not a detail: a target's whole toolchain
    /// graph is external and implicit, and it is the commonest thing for a
    /// tool's two runs to disagree about.
    fn has_external_input(&self, label: &str) -> bool;

    /// The label corresponds to a path that changed in this change set but that no
    /// target in the head index consumes — a file bazel-diff hashes and
    /// cambium correctly has no consumer for.
    fn changed_path_without_consumer(&self, label: &str) -> bool;

    /// The `BUILD` file of the target's own package is in this change set's changed
    /// path set. Says nothing on its own — a changed `BUILD` file usually means
    /// a changed rule — and is only ever asked after the rule has been shown to
    /// be byte-identical.
    fn build_file_changed(&self, label: &str) -> bool;

    /// The target's rule differs between base and head *only* in the absolute
    /// workspace path it embeds.
    ///
    /// The tools hash base in a second git worktree
    /// ([`crate::tools::BaseWorktree`]) while cambium carries its base
    /// index forward inside the one checkout, so any rule that bakes the
    /// workspace root into an attribute reads as changed to them and unchanged
    /// to cambium. envoy does this to eleven targets through
    /// `@envoy_repo//:path.bzl`:
    ///
    /// ```text
    /// sh_binary(name = "replace", srcs = ["replace.sh"], args = [PATH])
    /// #   args = ["/…/subjects/envoy"] at head, ["/…/scratch/base-worktree"] at base
    /// ```
    ///
    /// and the same constant reaches `//tools/code:check`'s `args`,
    /// `//tools/code:checked`'s `cmd`, `//tools/clang-tidy:collect_fixes`'s
    /// `--workspace_root=`, and the rest. It is a property of the *harness*,
    /// not of either tool: run both sides in the same directory and the
    /// difference is gone.
    fn workspace_path_only_change(&self, label: &str) -> bool;
}

/// Classifies one label of the symmetric difference.
///
/// Order matters only in that the cheap, certain predicates come first:
/// `external` before `content`, and on cambium's side `select` before
/// `implicit` before `visibility`, so a target explained by more than one is
/// attributed to the most specific reason. Anything left over is a failure —
/// deliberately, because a silent "other" bucket is how an unsoundness gets
/// shipped.
pub fn classify(label: &str, side: Side, facts: &dyn Facts) -> Class {
    match side {
        Side::Cambium => {
            // The external-repository reason comes first: when it holds, the
            // target is in cambium's answer *only* because a repository moved,
            // and a `select` or implicit edge further along the path is a
            // detail of how it got there rather than the reason it is there.
            if facts.reached_via_external_repo(label) {
                Class::CambiumExtraExternalRepo
            } else if facts.reachable_via_select(label) {
                Class::CambiumExtraSelect
            } else if facts.reachable_only_via_implicit(label) {
                Class::CambiumExtraImplicit
            } else if facts.visibility_only_change(label) {
                Class::CambiumExtraVisibility
            } else if facts.bzl_closure_only(label) {
                Class::CambiumExtraBzl
            } else if facts.reached_via_deleted_dep(label) {
                Class::CambiumExtraDeleted
            } else if facts.reached_via_build_file(label) {
                Class::CambiumExtraBuildGlob
            } else {
                Class::Unclassified
            }
        }
        Side::Tool => {
            if facts.is_external(label) {
                Class::BazeldiffExtraExternal
            } else if facts.changed_path_without_consumer(label) {
                Class::BazeldiffExtraContent
            } else if workspace_path_artefact(label, facts) {
                Class::BazeldiffExtraWorkspacePath
            } else if external_load_nondeterminism(label, facts) {
                Class::BazeldiffExtraExternalContent
            } else if line_shift_only(label, facts) {
                Class::BazeldiffExtraLineShift
            } else {
                Class::CambiumMissing
            }
        }
    }
}

/// The conjunction behind `bazeldiff_extra_workspace_path`.
///
/// Three facts, and all three are needed. The first is the explanation; the
/// other two are the same "nothing else could have impacted it" change set the
/// external-content class insists on, because a rule that embeds the workspace
/// path can *also* have a changed source or a changed dependency, and then the
/// tool is right and cambium is wrong for an unrelated reason.
///
/// 1. base and head agree once each side's own workspace root is normalised
///    away, and disagree before that — so the path is the whole difference;
/// 2. no source file it consumes changed;
/// 3. nothing it transitively depends on inside `//...` is in cambium's
///    affected set.
fn workspace_path_artefact(label: &str, facts: &dyn Facts) -> bool {
    let path_only = facts.workspace_path_only_change(label);
    let no_srcs = path_only && facts.no_changed_source_inputs(label);
    let no_deps = no_srcs && facts.no_affected_deps(label);
    tracing::debug!(
        label,
        path_only,
        no_srcs,
        no_deps,
        "workspace-path conditions"
    );
    no_deps
}

/// The conjunction behind `bazeldiff_extra_line_shift`.
///
/// Same three "nothing could have impacted it" facts the other tool-side
/// classes insist on, plus the explanation: the target's own `BUILD` file
/// changed. The rule proto is byte-identical, so whatever the edit did, it did
/// not do it to this rule — it moved it down the file. bazel-diff hashes
/// `generator_location`; cambium dropped it from the canonical rule
/// deliberately (see `docs/STORE.md` and `cambium_proto::RULE_HASH_VERSION`),
/// because a macro's BUILD line shifts whenever a line is inserted above it,
/// and Tango ignores it too.
///
/// Note the order: [`Facts::no_changed_source_inputs`] runs first, so the
/// `filegroup(srcs = glob(["**"]))` shape — where the `BUILD` file really *is*
/// an input and the target really did change — never reaches here.
///
/// Found on the `apollo` subject: one commit reordered
/// `modules/common/vehicle_model/BUILD`, and bazel-diff reported the two
/// `filegroup`s below the edit whose globbed data files had not moved at all.
fn line_shift_only(label: &str, facts: &dyn Facts) -> bool {
    let unchanged = facts.rule_unchanged_modulo_visibility(label);
    let no_srcs = unchanged && facts.no_changed_source_inputs(label);
    let no_deps = no_srcs && facts.no_affected_deps(label);
    let build_changed = no_deps && facts.build_file_changed(label);
    tracing::debug!(
        label,
        unchanged,
        no_srcs,
        no_deps,
        build_changed,
        "line-shift conditions"
    );
    build_changed
}

/// The conjunction behind `bazeldiff_extra_external_content`.
///
/// bazel-diff seeds a target's digest from its dependencies' digests, and its
/// dependency set includes the external graph. On `bazel/bazel` under
/// `--keep_going`, how much of that external graph loads is not stable between
/// invocations — a repository rule that fetched on one run and did not on the
/// next changes the digest of everything above it — so bazel-diff reports
/// labels like `//:bootstrap-jars` as impacted on change sets where nothing inside
/// `//...` that they depend on moved at all.
///
/// The predicate is deliberately a conjunction of four *independent* facts, and
/// all four must hold. Take away any one and the label stays `cambium_missing`,
/// because each of the first three is a way the label could genuinely have been
/// impacted and the fourth is the only thing that supplies the alternative
/// explanation:
///
/// 1. the target's own rule is identical at base and head (modulo
///    `visibility`/`tags`, which cannot change outputs) — so it is not a rule
///    edit;
/// 2. no source file it consumes changed — so it is not a content edit;
/// 3. nothing it transitively depends on inside `//...` is in cambium's
///    affected set — so it is not a propagation cambium dropped;
/// 4. it does depend on something external — so there *is* an external
///    explanation for the difference. "Depends on" here means Bazel's full
///    dependency set, implicit edges included: a target's toolchains are
///    external and reached only implicitly, and asking the
///    index answered "no external dependency" for every plain `java_binary`
///    in the repository when it was built with `--noimplicit_deps`.
///
/// Without (4) the class would be "cambium missed a label and we cannot see
/// why", which is exactly the failure `cambium_missing` exists to report.
fn external_load_nondeterminism(label: &str, facts: &dyn Facts) -> bool {
    // Evaluated one at a time and logged, because when a label lands in
    // `cambium_missing` the only thing worth knowing is *which* of the four
    // conditions said no: both times this classifier was wrong about a label,
    // it was a condition answering the wrong question rather than a missing
    // class.
    let unchanged = facts.rule_unchanged_modulo_visibility(label);
    let no_srcs = unchanged && facts.no_changed_source_inputs(label);
    let no_deps = no_srcs && facts.no_affected_deps(label);
    let external = no_deps && facts.has_external_input(label);
    tracing::debug!(
        label,
        unchanged,
        no_srcs,
        no_deps,
        external,
        "external-content conditions"
    );
    external
}

// ----------------------------------------------------------------- BazelFacts

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use cambium_bazel::Bazel;
use cambium_index::{IndexRef, WalkOptions};
use cambium_proto::blaze_query::{attribute::Discriminator, Rule, Target};

/// A [`Facts`] implementation backed by Bazel and the head index.
///
/// It caches every rule proto it fetches: a change set's symmetric difference asks
/// about the same handful of packages repeatedly, and each miss is a Bazel
/// query.
///
/// **On `cambium_extra_implicit`.** The index records no edge kind, so there is
/// no "implicit" edge to test directly.
/// The heuristic used instead: fetch the target's own rule and split its
/// declared dependency labels into explicit ones (attributes whose names do
/// not start with `$` or `:`) and the full `rule_input` set, which Bazel
/// populates from *all* attributes including the implicit ones. If none of the
/// explicit dependency labels is in the direct set but some other `rule_input`
/// is, the only way the change reaches this target is an implicit attribute.
/// That is an approximation in one direction — it only looks one hop back — so
/// it is conservative: a multi-hop implicit path lands in `unclassified` and
/// gets a human's attention rather than being waved through.
pub struct BazelFacts<'a> {
    head: &'a Bazel,
    /// A Bazel rooted at the base worktree, for the visibility comparison.
    /// `None` disables that class (it then reads as `unclassified`, which is
    /// the safe direction).
    base: Option<Bazel>,
    index: IndexRef<'a>,
    /// cambium's depth-0 set for this change set.
    direct: BTreeSet<String>,
    /// cambium's whole affected set for this change set (depth 0 included).
    affected: BTreeSet<String>,
    /// Workspace-relative paths `git diff --name-only base..head` reported.
    changed_paths: BTreeSet<String>,
    /// Affected labels whose `--explain` reason is `DeletedDep`.
    deleted_dep: BTreeSet<String>,
    /// Affected labels whose `--explain` reason is `ExternalRepo`.
    external_repo: BTreeSet<String>,
    /// Affected labels whose `--explain` reason is a changed `BUILD` file.
    build_file: BTreeSet<String>,
    /// For an affected label, the shortest path back to the direct-set member
    /// that reached it (`--explain`). Lets the `select` test look along the
    /// whole path instead of one hop.
    paths: HashMap<String, Vec<String>>,
    cache: RefCell<HashMap<String, Option<Rule>>>,
    base_cache: RefCell<HashMap<String, Option<Rule>>>,
    /// Rule protos fetched for [`Facts::has_external_input`]'s closure sweep,
    /// kept apart from `cache` because that one is populated by `prefetch`
    /// from the symmetric difference and this one from whole dependency
    /// closures.
    implicit_cache: RefCell<HashMap<String, Option<Rule>>>,
    /// One forward walk per label, memoised: `bazeldiff_extra_external_content`
    /// asks two questions of the same traversal.
    dep_cache: RefCell<HashMap<String, DepFacts>>,
}

/// What one forward walk over the head index found about a target.
#[derive(Clone, Copy, Debug, Default)]
struct DepFacts {
    /// Some transitive `//...` dependency is in cambium's affected set.
    touches_affected: bool,
    /// Some transitive dependency is an external (`@`-prefixed) label.
    has_external: bool,
    /// The head index knows the label at all. When it does not, nothing here
    /// was actually established and the caller must not conclude anything.
    known: bool,
}

impl<'a> BazelFacts<'a> {
    pub fn new(
        head: &'a Bazel,
        index: IndexRef<'a>,
        direct: impl IntoIterator<Item = String>,
        changed_paths: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            head,
            base: None,
            index,
            direct: direct.into_iter().collect(),
            affected: BTreeSet::new(),
            changed_paths: changed_paths.into_iter().collect(),
            deleted_dep: BTreeSet::new(),
            external_repo: BTreeSet::new(),
            build_file: BTreeSet::new(),
            paths: HashMap::new(),
            cache: RefCell::new(HashMap::new()),
            base_cache: RefCell::new(HashMap::new()),
            implicit_cache: RefCell::new(HashMap::new()),
            dep_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Records cambium's affected set, enabling
    /// `bazeldiff_extra_external_content`. Without it that class's third
    /// condition can never be shown to hold and the label stays
    /// `cambium_missing`, which is the safe direction.
    pub fn with_affected(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.affected = labels.into_iter().collect();
        self
    }

    /// Records the targets cambium reached because a dependency was deleted,
    /// enabling `cambium_extra_deleted`. Feed it from the report's
    /// `explanations` (the comparison asks for them).
    pub fn with_deleted_dep(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.deleted_dep = labels.into_iter().collect();
        self
    }

    /// Records the targets cambium reached because an external repository
    /// moved, enabling `cambium_extra_external_repo`. Feed it from the
    /// report's `explanations`.
    pub fn with_external_repo(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.external_repo = labels.into_iter().collect();
        self
    }

    /// Records the targets cambium reached from a changed `BUILD` file,
    /// enabling `cambium_extra_build_glob`.
    pub fn with_build_file(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.build_file = labels.into_iter().collect();
        self
    }

    /// Records `--explain` paths, so `cambium_extra_select` can look along the
    /// whole path from the change to the target rather than at its last hop.
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        self.paths = paths.into_iter().collect();
        self
    }

    /// Adds the base-side Bazel, enabling `cambium_extra_visibility`.
    pub fn with_base(mut self, base: Bazel) -> Self {
        self.base = Some(base);
        self
    }

    /// `git diff --name-only base..head` in `repo`.
    pub fn changed_paths(repo: &Path, base: &str, head: &str) -> Result<BTreeSet<String>> {
        let out = crate::tools::git(repo, &["diff", "--name-only", &format!("{base}..{head}")])?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn rule(
        &self,
        bazel: &Bazel,
        cache: &RefCell<HashMap<String, Option<Rule>>>,
        label: &str,
    ) -> Option<Rule> {
        if let Some(hit) = cache.borrow().get(label) {
            return hit.clone();
        }
        let got = query_rule(bazel, label);
        cache.borrow_mut().insert(label.to_string(), got.clone());
        got
    }

    fn head_rule(&self, label: &str) -> Option<Rule> {
        self.rule(self.head, &self.cache, label)
    }

    /// Everything the head index can say about a target's dependency cone.
    ///
    /// One breadth-first walk over the forward edges answers both "does it
    /// reach anything cambium called affected" and "does it reach anything
    /// external". External labels *are* in the index: `rule_input` is interned
    /// unfiltered, so an `@repo//...` dependency of an indexed rule exists as
    /// an [`cambium_index::NodeKind::Unknown`] node with no package and no
    /// rule class — enough to see it and to walk past it, which is all this
    /// needs. (Its own dependencies are not there: cambium queries `//...`, so
    /// the external graph stops at its first node.)
    fn dep_facts(&self, label: &str) -> DepFacts {
        if let Some(hit) = self.dep_cache.borrow().get(label) {
            return *hit;
        }
        let facts = self.compute_dep_facts(label);
        self.dep_cache.borrow_mut().insert(label.to_string(), facts);
        facts
    }

    fn compute_dep_facts(&self, label: &str) -> DepFacts {
        let Some(id) = self.index.id_of(label) else {
            return DepFacts::default();
        };
        let mut out = DepFacts {
            known: true,
            ..Default::default()
        };
        let walk = cambium_index::walk_deps(self.index, &[id], &WalkOptions::default());
        for v in walk.visited.ones() {
            let v = v as u32;
            if v == id {
                continue;
            }
            let l = self.index.label(v);
            if l.starts_with('@') || l.starts_with("//external:") {
                out.has_external = true;
            } else if self.affected.contains(l) {
                out.touches_affected = true;
            }
        }
        // A dependency named in the rule but absent from the index still
        // counts: the index is `//...`, so an external label the walk cannot
        // reach is exactly the case this looks for. The toolchain graph —
        // `@bazel_tools//tools/jdk:jdk`, `@local_jdk//:jdk`, the C++
        // toolchain — is exactly what a tool's two runs disagree about
        // most often, and it is reached through implicit attributes; the index
        // now carries those (`cambium_bazel::QUERY_FLAGS`), so the walk above
        // usually settles it and this is the fallback for a label the index
        // does not know.
        if !out.has_external {
            if let Some(rule) = self.head_rule(label) {
                out.has_external = names_external_input(&rule);
            }
        }
        // And when the label's own inputs say nothing either, ask the same
        // question of every rule in the closure the walk just enumerated.
        // `//:srcs` reaches `@async_profiler//file:file` only several
        // filegroups down, through an edge on a target inside its closure;
        // bazel-diff reported it on a change set whose only changed files were
        // `.github/workflows/*.yml`, which no rule consumes at all.
        if !out.has_external {
            let mut closure: Vec<String> = walk
                .visited
                .ones()
                .map(|v| v as u32)
                // Only rules have a `rule_input`; the file nodes in a
                // filegroup's closure would be a Bazel query each for nothing.
                .filter(|&v| self.index.is_rule(v))
                .map(|v| self.index.label(v).to_string())
                .filter(|l| !self.is_external(l))
                .collect();
            closure.sort();
            closure.dedup();
            out.has_external = self.any_implicit_external(&closure);
        }
        out
    }

    /// Does any rule in `labels` name an external label among its inputs?
    ///
    /// Batched and memoised: the closures overlap heavily between the handful
    /// of labels one change set asks about, and one `query 'set(...)'` answers five
    /// hundred of them.
    fn any_implicit_external(&self, labels: &[String]) -> bool {
        for chunk in labels.chunks(PREFETCH_CHUNK) {
            let todo: Vec<&String> = {
                let c = self.implicit_cache.borrow();
                chunk
                    .iter()
                    .filter(|l| !c.contains_key(l.as_str()))
                    .collect()
            };
            if !todo.is_empty() {
                let expr = format!(
                    "set({})",
                    todo.iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if let Some(rules) = query_rules(self.head, &expr) {
                    let mut c = self.implicit_cache.borrow_mut();
                    for r in rules {
                        c.insert(r.name.clone(), Some(r));
                    }
                    for l in todo {
                        c.entry(l.clone()).or_insert(None);
                    }
                }
            }
            let c = self.implicit_cache.borrow();
            if chunk
                .iter()
                .filter_map(|l| c.get(l.as_str()))
                .flatten()
                .any(names_external_input)
            {
                return true;
            }
        }
        false
    }

    /// Fetches the rule protos for `labels` in bulk, at both revisions.
    ///
    /// Without this the classifier is one `bazel query <label>` per label, and
    /// the first real run on `bazel/bazel` produced symmetric differences of
    /// tens of thousands of labels: at roughly a second per invocation that is
    /// hours per change set, and it is *all* process startup — one batched
    /// `query 'set(a b c ...)'` answers five hundred labels in the same time.
    /// Labels the batch does not return are cached as `None`, so a target
    /// Bazel will not talk about is not retried one at a time either.
    ///
    /// Best-effort: a batch that fails leaves its labels uncached, and they
    /// fall back to the per-label path.
    pub fn prefetch(&self, labels: impl IntoIterator<Item = String>) {
        let wanted: Vec<String> = labels
            .into_iter()
            .filter(|l| !self.is_external(l))
            .collect();
        if wanted.is_empty() {
            return;
        }
        prefetch_into(self.head, &self.cache, &wanted);
        if let Some(base) = self.base.as_ref() {
            prefetch_into(base, &self.base_cache, &wanted);
        }
    }
}

/// How many labels go into one batched `bazel query 'set(...)'`. Large enough
/// that startup stops dominating, small enough that the command line and the
/// decoded proto stay manageable.
pub const PREFETCH_CHUNK: usize = 500;

fn prefetch_into(bazel: &Bazel, cache: &RefCell<HashMap<String, Option<Rule>>>, labels: &[String]) {
    for chunk in labels.chunks(PREFETCH_CHUNK) {
        let todo: Vec<&String> = {
            let c = cache.borrow();
            chunk
                .iter()
                .filter(|l| !c.contains_key(l.as_str()))
                .collect()
        };
        if todo.is_empty() {
            continue;
        }
        let expr = format!(
            "set({})",
            todo.iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let Some(rules) = query_rules(bazel, &expr) else {
            continue;
        };
        let mut c = cache.borrow_mut();
        for r in rules {
            c.insert(r.name.clone(), Some(r));
        }
        for l in todo {
            c.entry(l.clone()).or_insert(None);
        }
    }
}

/// Every rule proto a query expression yields, or `None` if the query could
/// not be run at all. Exit 3 (`--keep_going` finished with errors) is kept:
/// the labels Bazel could resolve are on stdout, and the ones it could not are
/// cached as `None` by the caller.
fn query_rules(bazel: &Bazel, expr: &str) -> Option<Vec<Rule>> {
    let out = bazel
        .command("query")
        .args([
            expr,
            "--output=streamed_proto",
            "--proto:flatten_selects=false",
            "--proto:locations=false",
            // No `--noimplicit_deps`, matching `cambium_bazel::QUERY_FLAGS`:
            // the index carries implicit edges, so a proto compared against it
            // has to carry them too.
            "--order_output=no",
            "--keep_going",
            "--noshow_progress",
            "--ui_event_filters=-info",
        ])
        .output()
        .ok()?;
    if !out.status.success() && out.status.code() != Some(3) {
        tracing::debug!(status = %out.status, "the batched rule query failed");
        return None;
    }
    let targets: Vec<Target> = cambium_proto::decode_all(out.stdout.as_slice()).ok()?;
    Some(targets.into_iter().filter_map(|t| t.rule).collect())
}

/// Whether a rule names a label outside the main repository among its
/// inputs. Split out from the walk so the predicate can be tested on a
/// hand-built proto without a Bazel.
pub fn names_external_input(rule: &Rule) -> bool {
    rule.rule_input
        .iter()
        .any(|l| l.starts_with('@') || l.starts_with("//external:"))
}

/// One rule's proto at a revision, or `None` when Bazel cannot produce it.
///
/// `--proto:flatten_selects=false` is the point of the call: flattened, an
/// attribute's `select` branches are merged into one list and the
/// `selector_list` field is gone, so there is no way to tell a `select`ed dep
/// from an unconditional one.
fn query_rule(bazel: &Bazel, label: &str) -> Option<Rule> {
    let out = bazel
        .command("query")
        .args([
            label,
            "--output=streamed_proto",
            "--proto:flatten_selects=false",
            "--proto:locations=false",
            // See `query_rules`.
            "--order_output=no",
            "--keep_going",
            "--noshow_progress",
            "--ui_event_filters=-info",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!(label, "bazel query for the rule proto failed");
        return None;
    }
    let targets: Vec<Target> = cambium_proto::decode_all(out.stdout.as_slice()).ok()?;
    targets.into_iter().find_map(|t| t.rule)
}

/// The labels a rule names in attributes the BUILD file wrote, and whether any
/// of those attributes is a `select`.
///
/// Both selector branches and plain values count as declared labels: cambium's
/// edges are the union over branches, so the comparison has to look at the
/// same union.
pub fn explicit_dep_labels(rule: &Rule) -> (BTreeSet<String>, bool) {
    let mut labels = BTreeSet::new();
    let mut any_select = false;
    for a in &rule.attribute {
        if a.name.starts_with('$') || a.name.starts_with(':') {
            continue; // implicit, rule-supplied
        }
        // Under a selector list the attribute's own discriminator is
        // SELECTOR_LIST; the type it evaluates to lives on the list.
        let discriminator = match &a.selector_list {
            Some(sel) => sel.r#type.unwrap_or(a.r#type),
            None => a.r#type,
        };
        let is_label = matches!(
            Discriminator::try_from(discriminator),
            Ok(Discriminator::Label)
                | Ok(Discriminator::LabelList)
                | Ok(Discriminator::LabelKeyedStringDict)
        );
        if !is_label {
            continue;
        }
        if let Some(sel) = &a.selector_list {
            any_select = true;
            for s in &sel.elements {
                for e in &s.entries {
                    labels.extend(e.string_list_value.iter().cloned());
                    labels.extend(e.string_value.iter().cloned());
                }
            }
            continue;
        }
        labels.extend(a.string_list_value.iter().cloned());
        labels.extend(a.string_value.iter().cloned());
    }
    (labels, any_select)
}

/// A rule stripped of everything that cannot change its outputs, for the
/// visibility comparison: the canonical form used for hashing, minus
/// `visibility` and `tags`.
pub fn rule_without_visibility(rule: &Rule) -> Rule {
    let mut r = cambium_proto::canonical_rule(rule);
    r.attribute
        .retain(|a| a.name != "visibility" && a.name != "tags");
    r
}

/// A rule with every occurrence of `from` in its string-valued attributes
/// replaced by `to`.
///
/// Only the fields that can carry a workspace path matter: `string_value`
/// (a `genrule`'s `cmd`), `string_list_value` (an `args` list), the values of
/// a `string_dict` (a test's `env`), and the same three inside every branch of
/// a `selector_list`. Anything the substitution does not reach simply keeps
/// its bytes, and the comparison this feeds then reports "not equal", which is
/// the safe direction: the label stays `cambium_missing`.
pub fn substitute_paths(rule: &Rule, from: &str, to: &str) -> Rule {
    fn fix(v: &mut String, from: &str, to: &str) {
        if v.contains(from) {
            *v = v.replace(from, to);
        }
    }
    let mut r = rule.clone();
    for a in &mut r.attribute {
        if let Some(v) = a.string_value.as_mut() {
            fix(v, from, to);
        }
        for v in &mut a.string_list_value {
            fix(v, from, to);
        }
        for e in &mut a.string_dict_value {
            fix(&mut e.value, from, to);
        }
        if let Some(sel) = a.selector_list.as_mut() {
            for s in &mut sel.elements {
                for e in &mut s.entries {
                    if let Some(v) = e.string_value.as_mut() {
                        fix(v, from, to);
                    }
                    for v in &mut e.string_list_value {
                        fix(v, from, to);
                    }
                    for d in &mut e.string_dict_value {
                        fix(&mut d.value, from, to);
                    }
                }
            }
        }
    }
    r
}

/// `//pkg:name` -> `pkg/name`, the workspace-relative path of a source label.
/// `None` for anything that is not a main-repository label.
pub fn label_to_path(label: &str) -> Option<String> {
    let rest = label.strip_prefix("//")?;
    let (pkg, name) = rest.split_once(':').unwrap_or((rest, ""));
    if name.is_empty() {
        return None;
    }
    Some(if pkg.is_empty() {
        name.to_string()
    } else {
        format!("{pkg}/{name}")
    })
}

impl Facts for BazelFacts<'_> {
    fn is_external(&self, label: &str) -> bool {
        label.starts_with('@') || label.starts_with("//external:")
    }

    fn reachable_via_select(&self, label: &str) -> bool {
        // Any rule on the path from the change to this target declares a
        // `select`ed dependency attribute, so a configured build need not take
        // this route at all.
        //
        // Looking only at the target's own rule (which is all this could do
        // before the comparison carried `--explain` paths) misses the common
        // shape by a mile: on `bazel/bazel` the extra targets are reached
        // through a `select` several hops down — a `cc_library` picking a
        // platform-specific dep, with plain `java_library` rules stacked above
        // it — and every one of those landed in `unclassified`, which fails
        // the run for a difference that is exactly what cambium promises to
        // over-approximate.
        let path = self.paths.get(label);
        let candidates: Vec<&str> = match path {
            Some(p) => p.iter().map(String::as_str).collect(),
            None => vec![label],
        };
        candidates.iter().any(|l| match self.head_rule(l) {
            Some(rule) => explicit_dep_labels(&rule).1,
            None => false,
        })
    }

    fn reachable_only_via_implicit(&self, label: &str) -> bool {
        let Some(rule) = self.head_rule(label) else {
            return false;
        };
        let (explicit, _) = explicit_dep_labels(&rule);
        let touches_direct = |set: &BTreeSet<String>| set.iter().any(|l| self.direct.contains(l));
        let all: BTreeSet<String> = rule.rule_input.iter().cloned().collect();
        !touches_direct(&explicit) && touches_direct(&all)
    }

    fn visibility_only_change(&self, label: &str) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            return false;
        };
        use prost::Message as _;
        let (hb, bb) = (
            rule_without_visibility(&h).encode_to_vec(),
            rule_without_visibility(&b).encode_to_vec(),
        );
        // Equal once visibility and tags are gone, but not equal with them:
        // that is exactly a visibility-only edit.
        hb == bb
            && cambium_proto::canonical_rule(&h).encode_to_vec()
                != cambium_proto::canonical_rule(&b).encode_to_vec()
    }

    fn bzl_closure_only(&self, label: &str) -> bool {
        if !self.changed_paths.iter().any(|p| p.ends_with(".bzl")) {
            return false;
        }
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            return false;
        };
        use prost::Message as _;
        // Identical proto at both ends: nothing about this rule changed, so
        // the only reason cambium could have reported it is its package's
        // load closure.
        cambium_proto::canonical_rule(&h).encode_to_vec()
            == cambium_proto::canonical_rule(&b).encode_to_vec()
    }

    fn reached_via_deleted_dep(&self, label: &str) -> bool {
        self.deleted_dep.contains(label)
    }

    fn reached_via_external_repo(&self, label: &str) -> bool {
        self.external_repo.contains(label)
    }

    fn reached_via_build_file(&self, label: &str) -> bool {
        self.build_file.contains(label)
    }

    fn rule_unchanged_modulo_visibility(&self, label: &str) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            // Without both protos nothing is established. Saying "unchanged"
            // here would let a target Bazel refuses to talk about into a
            // benign class.
            return false;
        };
        use prost::Message as _;
        rule_without_visibility(&h).encode_to_vec() == rule_without_visibility(&b).encode_to_vec()
    }

    fn no_changed_source_inputs(&self, label: &str) -> bool {
        let Some(rule) = self.head_rule(label) else {
            return false;
        };
        !rule
            .rule_input
            .iter()
            .filter_map(|l| label_to_path(l))
            .any(|p| self.changed_paths.contains(&p))
    }

    fn no_affected_deps(&self, label: &str) -> bool {
        let facts = self.dep_facts(label);
        facts.known && !facts.touches_affected
    }

    fn has_external_input(&self, label: &str) -> bool {
        self.dep_facts(label).has_external
    }

    fn workspace_path_only_change(&self, label: &str) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        // Both spellings of each root. Bazel prints the path it resolved the
        // workspace to, which on macOS is `/private/var/...` where the
        // worktree was created under `/var/...`; substituting the unresolved
        // spelling would then find nothing and the label would stay
        // `cambium_missing` for a reason that is pure path formatting.
        let roots = |b: &Bazel| {
            let p = b.workspace().to_path_buf();
            let mut v = vec![p.to_string_lossy().into_owned()];
            if let Ok(c) = p.canonicalize() {
                let c = c.to_string_lossy().into_owned();
                if !v.contains(&c) {
                    v.push(c);
                }
            }
            v
        };
        let (base_roots, head_roots) = (roots(base), roots(self.head));
        // Same directory on both sides: there is no artefact to explain, and
        // the substitution would be the identity.
        if base_roots == head_roots {
            return false;
        }
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            return false;
        };
        use prost::Message as _;
        let (h, b) = (rule_without_visibility(&h), rule_without_visibility(&b));
        let (hb, bb) = (h.encode_to_vec(), b.encode_to_vec());
        // Equal already: this is not what happened, and the external-content
        // class is the one that should speak for it.
        if hb == bb {
            return false;
        }
        base_roots.iter().any(|from| {
            head_roots
                .iter()
                .any(|to| substitute_paths(&b, from, to).encode_to_vec() == hb)
        })
    }

    fn build_file_changed(&self, label: &str) -> bool {
        // `//pkg:name` -> `pkg/BUILD`, either spelling. The root package's
        // BUILD file has no directory prefix.
        let Some(rest) = label.strip_prefix("//") else {
            return false;
        };
        let pkg = rest.split_once(':').map(|(p, _)| p).unwrap_or(rest);
        ["BUILD.bazel", "BUILD"].iter().any(|name| {
            let path = if pkg.is_empty() {
                (*name).to_string()
            } else {
                format!("{pkg}/{name}")
            };
            self.changed_paths.contains(&path)
        })
    }

    fn changed_path_without_consumer(&self, label: &str) -> bool {
        let Some(path) = label_to_path(label) else {
            return false;
        };
        if !self.changed_paths.contains(&path) {
            return false;
        }
        match self.index.id_of(label) {
            Some(id) => self.index.file_consumers(id).is_empty(),
            // The index does not know the file at all, so nothing consumes it.
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fourth condition of `bazeldiff_extra_external_content` reads the
    /// rule's inputs; a toolchain edge only shows up there when Bazel is
    /// asked *with* implicit deps, which is what
    /// `query_rule_with_implicit` is for.
    #[test]
    fn a_toolchain_input_counts_as_external() {
        let mut rule = Rule {
            name: "//examples:hello-world".into(),
            rule_input: vec!["//examples:Greeter.java".into()],
            ..Default::default()
        };
        assert!(
            !names_external_input(&rule),
            "a proto naming only main-repo inputs names nothing external"
        );
        rule.rule_input.push("@bazel_tools//tools/jdk:jdk".into());
        assert!(names_external_input(&rule));
    }

    #[test]
    fn the_legacy_external_spelling_counts_too() {
        let rule = Rule {
            name: "//a:b".into(),
            rule_input: vec!["//external:jdk".into()],
            ..Default::default()
        };
        assert!(names_external_input(&rule));
    }

    /// A hand-built oracle: every predicate is a membership test, so a test
    /// says exactly which fact it is exercising.
    #[derive(Default)]
    struct Hand {
        external: BTreeSet<String>,
        select: BTreeSet<String>,
        implicit: BTreeSet<String>,
        visibility: BTreeSet<String>,
        bzl: BTreeSet<String>,
        deleted: BTreeSet<String>,
        external_repo: BTreeSet<String>,
        build_glob: BTreeSet<String>,
        content: BTreeSet<String>,
        rule_unchanged: BTreeSet<String>,
        no_changed_srcs: BTreeSet<String>,
        no_affected_deps: BTreeSet<String>,
        external_input: BTreeSet<String>,
        workspace_path: BTreeSet<String>,
        build_changed: BTreeSet<String>,
    }

    impl Hand {
        fn with(mut f: impl FnMut(&mut Hand)) -> Hand {
            let mut h = Hand::default();
            f(&mut h);
            h
        }
    }

    impl Facts for Hand {
        fn is_external(&self, l: &str) -> bool {
            self.external.contains(l)
        }
        fn reachable_via_select(&self, l: &str) -> bool {
            self.select.contains(l)
        }
        fn reachable_only_via_implicit(&self, l: &str) -> bool {
            self.implicit.contains(l)
        }
        fn visibility_only_change(&self, l: &str) -> bool {
            self.visibility.contains(l)
        }
        fn bzl_closure_only(&self, l: &str) -> bool {
            self.bzl.contains(l)
        }
        fn reached_via_deleted_dep(&self, l: &str) -> bool {
            self.deleted.contains(l)
        }
        fn reached_via_external_repo(&self, l: &str) -> bool {
            self.external_repo.contains(l)
        }
        fn reached_via_build_file(&self, l: &str) -> bool {
            self.build_glob.contains(l)
        }
        fn changed_path_without_consumer(&self, l: &str) -> bool {
            self.content.contains(l)
        }
        fn rule_unchanged_modulo_visibility(&self, l: &str) -> bool {
            self.rule_unchanged.contains(l)
        }
        fn no_changed_source_inputs(&self, l: &str) -> bool {
            self.no_changed_srcs.contains(l)
        }
        fn no_affected_deps(&self, l: &str) -> bool {
            self.no_affected_deps.contains(l)
        }
        fn has_external_input(&self, l: &str) -> bool {
            self.external_input.contains(l)
        }
        fn workspace_path_only_change(&self, l: &str) -> bool {
            self.workspace_path.contains(l)
        }
        fn build_file_changed(&self, l: &str) -> bool {
            self.build_changed.contains(l)
        }
    }

    /// All four conditions of `bazeldiff_extra_external_content`, so a test can
    /// take exactly one away.
    fn external_content_facts() -> Hand {
        Hand::with(|h| {
            h.rule_unchanged.insert("//a:b".into());
            h.no_changed_srcs.insert("//a:b".into());
            h.no_affected_deps.insert("//a:b".into());
            h.external_input.insert("//a:b".into());
        })
    }

    /// The three "nothing could have impacted it" facts plus the changed
    /// `BUILD` file, so a test can take exactly one away.
    fn line_shift_facts() -> Hand {
        Hand::with(|h| {
            h.rule_unchanged.insert("//a:b".into());
            h.no_changed_srcs.insert("//a:b".into());
            h.no_affected_deps.insert("//a:b".into());
            h.build_changed.insert("//a:b".into());
        })
    }

    #[test]
    fn a_rule_that_only_moved_down_its_build_file_is_not_a_cambium_miss() {
        assert_eq!(
            classify("//a:b", Side::Tool, &line_shift_facts()),
            Class::BazeldiffExtraLineShift
        );
        assert!(!Class::BazeldiffExtraLineShift.is_failure());
    }

    #[test]
    fn a_glob_filegroup_that_eats_its_own_build_file_still_fails() {
        // The `BUILD` file is a source input, so the second condition is
        // false and the label is a genuine miss, not a line shift.
        let mut h = line_shift_facts();
        h.no_changed_srcs.clear();
        assert_eq!(classify("//a:b", Side::Tool, &h), Class::CambiumMissing);
    }

    #[test]
    fn a_moved_rule_with_an_affected_dependency_still_fails() {
        let mut h = line_shift_facts();
        h.no_affected_deps.clear();
        assert_eq!(classify("//a:b", Side::Tool, &h), Class::CambiumMissing);
    }

    fn one(field: fn(&mut Hand) -> &mut BTreeSet<String>, side: Side) -> Class {
        let h = Hand::with(|h| {
            field(h).insert("//a:b".to_string());
        });
        classify("//a:b", side, &h)
    }

    #[test]
    fn a_selected_dependency_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.select, Side::Cambium),
            Class::CambiumExtraSelect
        );
    }

    #[test]
    fn a_moved_external_repository_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.external_repo, Side::Cambium),
            Class::CambiumExtraExternalRepo
        );
    }

    /// The external-repository reason outranks `select`: a target cambium only
    /// holds because a repository moved is explained by the repository, not by
    /// the branch of the path it happened to arrive along.
    #[test]
    fn the_external_repository_reason_outranks_select() {
        let h = Hand::with(|h| {
            h.external_repo.insert("//a:b".into());
            h.select.insert("//a:b".into());
        });
        assert_eq!(
            classify("//a:b", Side::Cambium, &h),
            Class::CambiumExtraExternalRepo
        );
    }

    #[test]
    fn an_implicit_only_path_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.implicit, Side::Cambium),
            Class::CambiumExtraImplicit
        );
    }

    #[test]
    fn a_deleted_dependency_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.deleted, Side::Cambium),
            Class::CambiumExtraDeleted
        );
    }

    #[test]
    fn a_changed_build_file_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.build_glob, Side::Cambium),
            Class::CambiumExtraBuildGlob
        );
    }

    #[test]
    fn a_visibility_only_edit_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.visibility, Side::Cambium),
            Class::CambiumExtraVisibility
        );
    }

    #[test]
    fn an_unmoved_rule_in_a_bzl_dirty_package_explains_a_cambium_extra() {
        assert_eq!(one(|h| &mut h.bzl, Side::Cambium), Class::CambiumExtraBzl);
        assert!(!Class::CambiumExtraBzl.is_failure());
    }

    #[test]
    fn an_external_label_explains_a_tool_extra() {
        assert_eq!(
            one(|h| &mut h.external, Side::Tool),
            Class::BazeldiffExtraExternal
        );
    }

    #[test]
    fn an_unconsumed_changed_file_explains_a_tool_extra() {
        assert_eq!(
            one(|h| &mut h.content, Side::Tool),
            Class::BazeldiffExtraContent
        );
    }

    /// All three conditions of `bazeldiff_extra_workspace_path`.
    fn workspace_path_facts() -> Hand {
        Hand::with(|h| {
            h.workspace_path.insert("//a:b".into());
            h.no_changed_srcs.insert("//a:b".into());
            h.no_affected_deps.insert("//a:b".into());
        })
    }

    #[test]
    fn an_embedded_workspace_path_explains_a_tool_extra() {
        let c = classify("//a:b", Side::Tool, &workspace_path_facts());
        assert_eq!(c, Class::BazeldiffExtraWorkspacePath);
        assert!(!c.is_failure());
    }

    #[test]
    fn every_condition_of_the_workspace_path_class_is_load_bearing() {
        for drop in [
            (|h: &mut Hand| h.workspace_path.clear()) as fn(&mut Hand),
            |h: &mut Hand| h.no_changed_srcs.clear(),
            |h: &mut Hand| h.no_affected_deps.clear(),
        ] {
            let mut h = workspace_path_facts();
            drop(&mut h);
            assert_eq!(
                classify("//a:b", Side::Tool, &h),
                Class::CambiumMissing,
                "one condition short must not be waved through"
            );
        }
    }

    /// The substitution reaches an `args` list, a `cmd` string and a `select`
    /// branch — the three shapes envoy's `@envoy_repo//:path.bzl` constant
    /// lands in.
    #[test]
    fn substitution_normalises_every_string_shape() {
        use cambium_proto::blaze_query::attribute::{
            Discriminator as D, Selector, SelectorEntry, SelectorList,
        };
        use cambium_proto::blaze_query::Attribute;
        let rule = Rule {
            name: "//tools/sha:replace".into(),
            attribute: vec![
                Attribute {
                    name: "args".into(),
                    r#type: D::StringList as i32,
                    string_list_value: vec!["/base/wt".into()],
                    ..Default::default()
                },
                Attribute {
                    name: "cmd".into(),
                    r#type: D::String as i32,
                    string_value: Some("git -C /base/wt ls-files".into()),
                    ..Default::default()
                },
                Attribute {
                    name: "copts".into(),
                    r#type: D::SelectorList as i32,
                    selector_list: Some(SelectorList {
                        r#type: Some(D::StringList as i32),
                        elements: vec![Selector {
                            entries: vec![SelectorEntry {
                                label: Some("//conditions:default".into()),
                                string_list_value: vec!["-I/base/wt/include".into()],
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let got = substitute_paths(&rule, "/base/wt", "/head/wt");
        assert_eq!(got.attribute[0].string_list_value, vec!["/head/wt"]);
        assert_eq!(
            got.attribute[1].string_value.as_deref(),
            Some("git -C /head/wt ls-files")
        );
        assert_eq!(
            got.attribute[2].selector_list.as_ref().unwrap().elements[0].entries[0]
                .string_list_value,
            vec!["-I/head/wt/include"]
        );
        // A rule with nothing to substitute comes back untouched.
        use prost::Message as _;
        assert_eq!(
            substitute_paths(&rule, "/elsewhere", "/x").encode_to_vec(),
            rule.encode_to_vec()
        );
    }

    #[test]
    fn external_load_nondeterminism_explains_a_tool_extra() {
        let c = classify("//a:b", Side::Tool, &external_content_facts());
        assert_eq!(c, Class::BazeldiffExtraExternalContent);
        assert!(!c.is_failure());
    }

    #[test]
    fn every_condition_of_the_external_content_class_is_load_bearing() {
        // Take away one fact at a time: each removal is a way the label could
        // genuinely have been impacted (or, for the fourth, the loss of the
        // only alternative explanation), so each must fall back to the
        // failing class rather than being waved through.
        type Drop = (&'static str, fn(&mut Hand) -> &mut BTreeSet<String>);
        let drops: [Drop; 4] = [
            ("the rule itself changed", |h| &mut h.rule_unchanged),
            ("a source input changed", |h| &mut h.no_changed_srcs),
            ("a dependency is affected", |h| &mut h.no_affected_deps),
            ("nothing external is involved", |h| &mut h.external_input),
        ];
        for (why, field) in drops {
            let mut h = external_content_facts();
            field(&mut h).clear();
            assert_eq!(
                classify("//a:b", Side::Tool, &h),
                Class::CambiumMissing,
                "{why}: without this fact the label must still fail the change_set"
            );
        }
    }

    #[test]
    fn a_genuinely_external_label_is_not_the_content_class() {
        // `@repo//...` is out of cambium's universe entirely; the cheaper,
        // certain class wins.
        let mut h = external_content_facts();
        h.external.insert("//a:b".into());
        assert_eq!(
            classify("//a:b", Side::Tool, &h),
            Class::BazeldiffExtraExternal
        );
    }

    #[test]
    fn an_unexplained_tool_extra_is_unsoundness() {
        let c = classify("//a:b", Side::Tool, &Hand::default());
        assert_eq!(c, Class::CambiumMissing);
        assert!(c.is_failure());
    }

    #[test]
    fn an_unexplained_cambium_extra_is_unclassified_and_fails() {
        let c = classify("//a:b", Side::Cambium, &Hand::default());
        assert_eq!(c, Class::Unclassified);
        assert!(c.is_failure());
    }

    #[test]
    fn the_benign_classes_do_not_fail_a_change_set() {
        for c in Class::ALL.iter().filter(|c| !c.is_failure()) {
            assert!(c.name().contains("extra"), "{c}");
        }
        assert_eq!(Class::ALL.len(), 14);
        assert_eq!(Class::CambiumMissing.to_string(), "cambium_missing");
    }

    #[test]
    fn side_decides_which_predicates_apply() {
        // A label that is external *and* selected: the side settles it, so a
        // tool-only external label is never filed as a cambium extra.
        let h = Hand::with(|h| {
            h.external.insert("//a:b".into());
            h.select.insert("//a:b".into());
        });
        assert_eq!(
            classify("//a:b", Side::Tool, &h),
            Class::BazeldiffExtraExternal
        );
        assert_eq!(
            classify("//a:b", Side::Cambium, &h),
            Class::CambiumExtraSelect
        );
    }

    #[test]
    fn labels_map_to_workspace_paths() {
        assert_eq!(
            label_to_path("//lib/core:a.cc").as_deref(),
            Some("lib/core/a.cc")
        );
        assert_eq!(label_to_path("//:top.txt").as_deref(), Some("top.txt"));
        assert_eq!(label_to_path("@ext//a:b"), None);
        assert_eq!(label_to_path("//lib/core"), None, "a package is not a file");
    }

    fn attr(name: &str, values: &[&str]) -> cambium_proto::blaze_query::Attribute {
        cambium_proto::blaze_query::Attribute {
            name: name.into(),
            r#type: Discriminator::LabelList as i32,
            string_list_value: values.iter().map(|s| s.to_string()).collect(),
            explicitly_specified: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn selected_dep_attributes_are_seen_unflattened() {
        use cambium_proto::blaze_query::{
            attribute::Selector, attribute::SelectorEntry, attribute::SelectorList,
        };
        let mut deps = attr("deps", &[]);
        deps.r#type = Discriminator::SelectorList as i32;
        deps.selector_list = Some(SelectorList {
            r#type: Some(Discriminator::LabelList as i32),
            elements: vec![Selector {
                entries: vec![SelectorEntry {
                    label: Some("//conditions:default".into()),
                    string_list_value: vec!["//lib:a".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        });
        let rule = Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            attribute: vec![deps, attr("srcs", &["//p:s.cc"])],
            ..Default::default()
        };
        let (labels, any_select) = explicit_dep_labels(&rule);
        assert!(
            any_select,
            "the select survives --proto:flatten_selects=false"
        );
        assert!(labels.contains("//lib:a") && labels.contains("//p:s.cc"));
    }

    #[test]
    fn implicit_attributes_are_not_explicit_dependencies() {
        let rule = Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            attribute: vec![
                attr("$cc_toolchain", &["//tools:cc"]),
                attr("deps", &["//lib:a"]),
            ],
            rule_input: vec!["//tools:cc".into(), "//lib:a".into()],
            ..Default::default()
        };
        let (labels, _) = explicit_dep_labels(&rule);
        assert!(labels.contains("//lib:a"));
        assert!(
            !labels.contains("//tools:cc"),
            "$-prefixed attrs are implicit"
        );
    }

    #[test]
    fn visibility_and_tags_drop_out_of_the_comparable_rule() {
        let a = Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            attribute: vec![
                attr("deps", &["//lib:a"]),
                attr("visibility", &["//x:__pkg__"]),
            ],
            ..Default::default()
        };
        let mut b = a.clone();
        b.attribute[1] = attr("visibility", &["//visibility:public"]);
        use prost::Message as _;
        assert_eq!(
            rule_without_visibility(&a).encode_to_vec(),
            rule_without_visibility(&b).encode_to_vec()
        );
        assert_ne!(
            cambium_proto::canonical_rule(&a).encode_to_vec(),
            cambium_proto::canonical_rule(&b).encode_to_vec(),
            "the hash does move, which is why cambium reports the target"
        );
    }
}
