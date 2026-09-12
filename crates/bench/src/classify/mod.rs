//! Why two tools disagreed.
//!
//! A raw symmetric difference between cambium's answer and bazel-diff's is not
//! actionable: on a real repository it is thousands of labels, almost all of
//! them explained by a handful of known, *expected* differences in what the
//! two tools model. The spec named seven classes; the real runs turned up the
//! rest. Thirteen of the seventeen are benign and three fail the run — plus
//! `tool_missing`, which is a fact about the foil and gates nothing:
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
//! | `tool_extra_source_directory` | tool only | the target consumes a *directory* as a source file (`src_dir = ["."]`, rendered by Bazel as `//pkg:.`) and a changed path lies under it. Tango hashes a source directory's contents on disk; Bazel's own graph has no edge from a file to the directory above it, so neither `rdeps` nor cambium reaches the target. Bazel documents source directories as untracked, and the oracle is asked to make sure it did not reach the label anyway |
//! | `tool_missing` | cambium only | the tool missed a target the reverse-dependency oracle independently confirms is affected; cambium is right and the tool is wrong. Reported, not failed — it is not a defect of cambium |
//! | `cambium_missing_bzl_closure` | oracle only | **fails**: `rbuildfiles` names a loading package the static `.bzl` closure did not dirty. Not a per-label class — it is counted from the load oracle's `missing_packages` |
//! | `cambium_missing` | tool only | **fails**: the other tool reached something cambium did not, with no benign explanation |
//! | `unclassified` | cambium only | **fails**: an extra target nothing explains — either a real over-approximation bug or a missing predicate here |
//!
//! The classifier is a pure function of a [`Facts`] oracle so every class can
//! be tested without a Bazel; [`BazelFacts`] is the implementation that
//! actually asks one.

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

/// The seventeen disagreement classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// cambium reports a label because it keeps `select()` branches a
    /// *configured* Bazel query resolves away.
    ///
    /// An over-approximation, and the expected one: all three tools work on
    /// the unconfigured target graph, but the configured query the other two
    /// can fall back to resolves a `select()` to one branch. Nothing to do.
    CambiumExtraSelect,
    /// cambium reached the label from an external repository whose
    /// *declaration* moved.
    ///
    /// A `MODULE.bazel` edit, or a main-repo file a repository rule is built
    /// from; every main-repo target naming a label in that repository is then
    /// direct. An over-approximation: cambium's graph stops at the main
    /// repository's edge, so it cannot ask whether the repository's *contents*
    /// actually changed, while the hash-diffing tools fetch and hash it and
    /// see the targets unchanged. On `bazel/bazel`'s `5c47f233..bfd201d1` one
    /// `maven.install` version bump plus the `//:MODULE.bazel` that
    /// `//:extensions.bzl` reads through `ctx.path(Label(...))` put 69 such
    /// targets in cambium's answer. Ranked ahead of [`Class::CambiumExtraSelect`]:
    /// when it holds, the repository is *why* the target is in the answer.
    CambiumExtraExternalRepo,
    /// cambium reports a label reachable only through an implicit
    /// (`$`-prefixed, rule-supplied) attribute edge the other tool does not
    /// walk. An over-approximation.
    CambiumExtraImplicit,
    /// The two tools' canonical rules differ only in `visibility` or `tags`,
    /// which cambium's hash ignores on purpose. An over-approximation.
    CambiumExtraVisibility,
    /// The target's own rule proto is byte-identical at both revisions, but a
    /// `.bzl` its package loads changed, so cambium marked the whole loading
    /// package direct.
    ///
    /// An over-approximation, and on a real subject the largest cambium-side
    /// class.
    CambiumExtraBzl,
    /// A dependency of this target was deleted between base and head.
    ///
    /// Not an approximation: a target whose dep vanished really may build
    /// differently.
    CambiumExtraDeleted,
    /// Reached from a changed `BUILD` file consumed as a *source* by a
    /// `glob`bing filegroup.
    ///
    /// Not an approximation either: the file's content genuinely is an input
    /// to that filegroup.
    CambiumExtraBuildGlob,
    /// bazel-diff reports a changed path with no consumer in cambium's head
    /// index — a file outside any package, or a hash-only artifact. It hashes
    /// source content cambium's graph does not name.
    BazeldiffExtraContent,
    /// The disagreement is confined to `@external//...` labels, which the
    /// tools canonicalise differently (`@ext//:x` against `@@ext~//:x`).
    /// Canonicalisation only, not a graph difference.
    BazeldiffExtraExternal,
    /// A `//...` label the tool reported only because an external
    /// dependency of it hashed differently between the two runs.
    ///
    /// Its own rule (modulo `visibility`/`tags`), its source inputs and its
    /// transitive `//...` dependencies are all unchanged, and it depends on
    /// something external. bazel-diff's digests include the external graph,
    /// and on `bazel/bazel` under `--keep_going` how much of that graph loads
    /// varies between invocations. All four conditions must hold; drop any one
    /// and the label stays [`Class::CambiumMissing`].
    BazeldiffExtraExternalContent,
    /// A `//...` label whose rule differs between the tool's two checkouts
    /// only in the absolute workspace path it embeds.
    ///
    /// A harness artifact: the other tools hash base in a second git worktree
    /// while cambium carries its base index forward inside one checkout, so a
    /// rule that bakes the workspace root into `args`/`cmd`/`env` reads as
    /// changed to them and unchanged to cambium. All three conditions — rule
    /// identical once normalised, no changed source input, no affected
    /// dependency — must hold.
    BazeldiffExtraWorkspacePath,
    /// The rule did not move, its inputs did not move, and nothing it depends
    /// on is affected — but the `BUILD` file it lives in changed, so its line
    /// number did.
    ///
    /// bazel-diff hashes `generator_location`; cambium drops it from the
    /// canonical rule on purpose (`cambium_proto::RULE_HASH_VERSION`, see
    /// `docs/STORE.md`), and Tango ignores it too. Seen on `apollo`, where a
    /// `BUILD` reshuffle pushed two unrelated `filegroup`s down the file. The
    /// source-input condition is checked first, so the `glob(["**"])`
    /// filegroup that really does eat its own `BUILD` file never lands here.
    BazeldiffExtraLineShift,
    /// The target consumes a *directory* as a source file, and a changed path
    /// lies under that directory.
    ///
    /// `attr.label_list(allow_files = True)` given `["."]` is what `apollo`'s
    /// `install_src_files` rule does in a hundred packages
    /// (`tools/install/install.bzl`): Bazel renders the label as the source
    /// file `//modules/perception:.`, and the install script copies the
    /// directory at run time. Bazel's own graph has no edge from a file to the
    /// directory above it — source directories are documented as untracked
    /// (`label_directory`), and `rdeps(//..., //modules/perception/x:y.cc)`
    /// stops at the rules naming `y.cc` — so neither cambium nor the
    /// reverse-dependency oracle reaches the target. Tango hashes a source
    /// directory by walking it on disk (`targethasher.hashDir`), so to Tango
    /// every file under `modules/perception/` is an input of
    /// `//modules/perception:install_perception_src`, and it reports the
    /// target, its `install_src` parent, and the parent's parent up to
    /// `//:install_src`.
    ///
    /// Tango is describing the filesystem truthfully and Bazel is not, but
    /// the benchmark's definition of "affected" is Bazel's graph, which is
    /// what the oracle measures and what cambium indexes. The class needs
    /// two things: the mechanism — a directory source label on the target or
    /// in its dependency closure with a changed path strictly under it — and
    /// the oracle's silence. When `rdeps` *does* reach the label, the
    /// directory is not the explanation and the label stays
    /// [`Class::CambiumMissing`]; when the oracle did not run there is no
    /// witness either way and it stays there too. Reported, not failed: it is
    /// a limitation of Bazel's source model, not of cambium's walk, and the
    /// one thing cambium could do about it — hash source directories itself
    /// — would mean reading the tree on every run.
    ///
    /// Ranked ahead of the three "nothing could have impacted it"
    /// conjunctions on the tool side, because when it holds it is the more
    /// specific reason: the `install_src` parents would otherwise land in
    /// `bazeldiff_extra_external_content` (they have an external toolchain
    /// input) or `bazeldiff_extra_line_shift` (a `BUILD` edit *is* a changed
    /// path under the package directory) and the mechanism would be lost.
    ToolExtraSourceDirectory,
    /// The *tool* missed it, and the reverse-dependency oracle says cambium
    /// is right.
    ///
    /// The mirror image of [`Class::CambiumMissing`]: no benign predicate
    /// explains why cambium reported the label, and an independent
    /// `rdeps(//..., <changed files>)` reached it too. Two witnesses against
    /// one make this the tool's miss, not cambium's over-approximation.
    ///
    /// Deliberately ranked last on cambium's side, after every benign class.
    /// The oracle is an *unconfigured* query, so it contains the `select()`
    /// branches and macro-loaded packages that `cambium_extra_select` and
    /// `cambium_extra_bzl` already name; asking it first would relabel most of
    /// those as tool misses and say nothing about mechanism. Only a label
    /// nothing else explains reaches here.
    ///
    /// Not a failure. It is a fact about the foil, and gating cambium's run on
    /// another tool's defect would make the suite unrunnable for a reason that
    /// is not cambium's to fix. First seen on `bazel/bazel`, where Tango
    /// dropped `//docs:docs`, `//docs:srcs` and
    /// `//scripts/docs:gen_mdx_release_docs` — three plain consumers of a
    /// changed `docs/docs/user-manual.mdx` that both cambium and bazel-diff
    /// reported and the oracle confirmed.
    ToolMissing,
    /// A failure: a tool or an oracle found something cambium missed.
    ///
    /// This is unsoundness, and it is the one thing the benchmark exists to
    /// catch. There is no version of it that is acceptable.
    CambiumMissing,
    /// A failure: Bazel's `rbuildfiles` named a loading package that
    /// cambium's static `.bzl` load closure did not dirty.
    ///
    /// The one class no *label* is ever filed under: it is counted from the
    /// load oracle's `missing_packages` (`oracles::load`), one per package,
    /// not from a symmetric difference. It is the same defect as
    /// [`Class::CambiumMissing`] one level up — a package cambium should have
    /// marked direct and did not — caught by an oracle rather than a tool,
    /// and it fails the run for the same reason.
    CambiumMissingBzlClosure,
    /// A failure: no predicate explained the disagreement.
    ///
    /// It needs a new class or a real bug fix before it can be waved through.
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
            Class::ToolExtraSourceDirectory => "tool_extra_source_directory",
            Class::ToolMissing => "tool_missing",
            Class::CambiumMissing => "cambium_missing",
            Class::CambiumMissingBzlClosure => "cambium_missing_bzl_closure",
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
            Class::ToolExtraSourceDirectory => {
                "the target consumes a directory as a source file (`//pkg:.`) and a changed \
                 path lies under it; the tool hashes the directory's contents, Bazel's graph \
                 has no such edge and the reverse-dependency oracle did not reach the label"
            }
            Class::ToolMissing => {
                "the tool missed a target the reverse-dependency oracle confirms is \
                 affected: the tool's miss, not cambium's extra"
            }
            Class::CambiumMissing => {
                "**fails**: the oracle or a tool reached something cambium did not: \
                 unsoundness"
            }
            Class::CambiumMissingBzlClosure => {
                "**fails**: `rbuildfiles` names a loading package the static `.bzl` closure \
                 did not dirty"
            }
            Class::Unclassified => {
                "**fails**: no predicate explains the disagreement; needs a new class or a fix"
            }
        }
    }

    /// Whether a label in this class fails the change set.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            Class::CambiumMissing | Class::CambiumMissingBzlClosure | Class::Unclassified
        )
    }

    pub const ALL: [Class; 17] = [
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
        Class::ToolExtraSourceDirectory,
        Class::ToolMissing,
        Class::CambiumMissing,
        Class::CambiumMissingBzlClosure,
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

    /// The reverse-dependency oracle's answer contains this label.
    ///
    /// The oracle is `rdeps(//..., <every changed file's label>)` run against
    /// Bazel itself, so a `true` here is an independent witness that the
    /// target really is affected. Only ever asked once every benign
    /// cambium-side class has been ruled out; see [`Class::ToolMissing`].
    ///
    /// Defaults to `false`, which keeps the label in `unclassified`: without
    /// an oracle to appeal to there is no evidence either way, and the safe
    /// direction is a human's attention.
    fn confirmed_by_oracle(&self, _label: &str) -> bool {
        false
    }

    /// The reverse-dependency oracle ran on this change set, so
    /// [`Facts::confirmed_by_oracle`] answering `false` means Bazel did not
    /// reach the label rather than that nobody asked.
    ///
    /// Defaults to `false`: the classes that rest on the oracle's *silence*
    /// (`tool_extra_source_directory`) then never fire, which is the safe
    /// direction.
    fn oracle_ran(&self) -> bool {
        false
    }

    /// The target, or something in its dependency closure over the head
    /// index, names a directory as a source file (`//pkg:.`, `//pkg:testdata`)
    /// and some path in this change set lies strictly under that directory.
    ///
    /// Strictly under: a changed path *equal* to the label's path is a plain
    /// source edit, which cambium's walk sees on its own, and explaining it
    /// away here would hide a real miss.
    fn consumes_source_directory_under_change(&self, _label: &str) -> bool {
        false
    }
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
            } else if facts.confirmed_by_oracle(label) {
                // Nothing benign explains the extra, but Bazel's own
                // `rdeps` reached it as well: the tool is the one that is
                // short, so it is not cambium's failure to answer for.
                Class::ToolMissing
            } else {
                Class::Unclassified
            }
        }
        Side::Tool => {
            if facts.is_external(label) {
                Class::BazeldiffExtraExternal
            } else if facts.changed_path_without_consumer(label) {
                Class::BazeldiffExtraContent
            } else if source_directory_untracked(label, facts) {
                Class::ToolExtraSourceDirectory
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

/// The conjunction behind `tool_extra_source_directory`.
///
/// Two facts, and the order is deliberate: the mechanism first, because it
/// is a cheap index walk and names *why* the tool reported the label; then
/// the oracle, which must have run and must not have reached the label. A
/// label Bazel's own `rdeps` reaches is affected by the benchmark's
/// definition whatever else is true of it, so an affirmative oracle sends it
/// straight to `cambium_missing`. An oracle that did not run leaves the
/// label there too: the directory alone is a story, not a witness.
fn source_directory_untracked(label: &str, facts: &dyn Facts) -> bool {
    let directory = facts.consumes_source_directory_under_change(label);
    let asked = directory && facts.oracle_ran();
    let silent = asked && !facts.confirmed_by_oracle(label);
    tracing::debug!(
        label,
        directory,
        asked,
        silent,
        "source-directory conditions"
    );
    silent
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

mod bazel_facts;
pub use bazel_facts::{
    directory_contains_changed_path, explicit_dep_labels, label_to_path, names_external_input,
    rule_without_visibility, substitute_paths, BazelFacts, PREFETCH_CHUNK,
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

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
        oracle: BTreeSet<String>,
        /// The oracle ran (and `oracle` is its whole answer).
        oracle_ran: bool,
        source_dir: BTreeSet<String>,
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
        fn confirmed_by_oracle(&self, l: &str) -> bool {
            self.oracle.contains(l)
        }
        fn oracle_ran(&self) -> bool {
            self.oracle_ran
        }
        fn consumes_source_directory_under_change(&self, l: &str) -> bool {
            self.source_dir.contains(l)
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
    fn an_extra_the_oracle_confirms_is_the_tools_miss_and_does_not_fail() {
        // Nothing benign explains cambium reporting it, but `rdeps` reached
        // it too: two witnesses against the tool's one. `bazel/bazel`'s
        // `//docs:docs` on the 2026-09-08 run.
        let h = Hand::with(|h| {
            h.oracle.insert("//docs:docs".into());
        });
        let c = classify("//docs:docs", Side::Cambium, &h);
        assert_eq!(c, Class::ToolMissing);
        assert!(!c.is_failure(), "another tool's defect is not cambium's");
    }

    #[test]
    fn the_oracle_is_asked_last_so_benign_classes_keep_their_labels() {
        // The oracle is an *unconfigured* query, so it contains the `select`
        // branches and macro-loaded packages the benign classes name. If it
        // were consulted first every one of those would become a tool miss
        // and the taxonomy would stop explaining anything.
        for (setup, want) in [
            (
                Hand::with(|h| {
                    h.select.insert("//a:b".into());
                    h.oracle.insert("//a:b".into());
                }),
                Class::CambiumExtraSelect,
            ),
            (
                Hand::with(|h| {
                    h.bzl.insert("//a:b".into());
                    h.oracle.insert("//a:b".into());
                }),
                Class::CambiumExtraBzl,
            ),
            (
                Hand::with(|h| {
                    h.external_repo.insert("//a:b".into());
                    h.oracle.insert("//a:b".into());
                }),
                Class::CambiumExtraExternalRepo,
            ),
        ] {
            assert_eq!(classify("//a:b", Side::Cambium, &setup), want);
        }
    }

    #[test]
    fn without_an_oracle_an_unexplained_extra_stays_unclassified() {
        // The default `confirmed_by_oracle` is `false`, so a run with the
        // oracle switched off gets a human's attention rather than a silent
        // "the tool must be wrong".
        let c = classify("//a:b", Side::Cambium, &Hand::default());
        assert_eq!(c, Class::Unclassified);
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
        // Two kinds of class do not fail: an over-approximation by cambium
        // (every one of them named `*_extra_*`), and `tool_missing`, which is
        // a defect of the foil rather than of cambium. Nothing else may be
        // waved through — a silent "other" bucket is how an unsoundness ships.
        for c in Class::ALL.iter().filter(|c| !c.is_failure()) {
            assert!(
                c.name().contains("extra") || *c == Class::ToolMissing,
                "{c}"
            );
        }
        assert_eq!(Class::ALL.len(), 17);
        assert_eq!(Class::CambiumMissing.to_string(), "cambium_missing");
        assert_eq!(Class::ToolMissing.to_string(), "tool_missing");
    }

    /// docs/METHODOLOGY.md's taxonomy table is the published contract, and a
    /// class that exists only in one of the two is the failure mode: a run
    /// reports a name no reader can look up, or the docs promise a class the
    /// binary cannot produce.
    #[test]
    fn the_methodology_table_lists_exactly_the_classes_this_binary_knows() {
        let doc =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/METHODOLOGY.md");
        let text = std::fs::read_to_string(&doc).expect("docs/METHODOLOGY.md");
        let documented: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|l| l.split_once("` |"))
            .map(|(name, _)| name)
            // The file has more than one table; a class name is the only cell
            // that matches one of ours, and unknown names are caught below.
            .filter(|n| !n.contains(' '))
            .collect();
        assert!(
            documented.len() >= Class::ALL.len(),
            "the table did not parse: {documented:?}"
        );
        for c in Class::ALL {
            assert!(
                documented.contains(&c.name()),
                "{c} is missing from docs/METHODOLOGY.md's taxonomy table"
            );
        }
        for name in documented {
            if name.contains("extra") || name.contains("missing") || name == "unclassified" {
                assert!(
                    Class::ALL.iter().any(|c| c.name() == name),
                    "docs/METHODOLOGY.md documents `{name}`, which is not a Class"
                );
            }
        }
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

    /// The apollo shape: `install_src_files(src_dir = ["."])` in
    /// `//modules/perception`, a `.cc` three directories down changed, the
    /// oracle ran and did not reach the target.
    fn source_directory_facts() -> Hand {
        Hand::with(|h| {
            h.source_dir
                .insert("//modules/perception:install_perception_src".into());
            h.oracle_ran = true;
        })
    }

    #[test]
    fn a_source_directory_the_oracle_cannot_see_is_the_tools_extra_not_cambiums_miss() {
        let c = classify(
            "//modules/perception:install_perception_src",
            Side::Tool,
            &source_directory_facts(),
        );
        assert_eq!(c, Class::ToolExtraSourceDirectory);
        assert!(!c.is_failure());
    }

    #[test]
    fn a_source_directory_the_oracle_does_reach_is_still_a_cambium_miss() {
        // Bazel's own `rdeps` reached it: then it is affected by the
        // benchmark's definition, the directory is a coincidence, and cambium
        // has to answer for the label.
        let h = Hand::with(|h| {
            h.source_dir.insert("//a:b".into());
            h.oracle_ran = true;
            h.oracle.insert("//a:b".into());
        });
        assert_eq!(classify("//a:b", Side::Tool, &h), Class::CambiumMissing);
    }

    #[test]
    fn without_an_oracle_a_source_directory_is_not_evidence() {
        // `--oracle` off: nobody asked Bazel, so the directory is a story and
        // the label keeps the failing class until someone does.
        let h = Hand::with(|h| {
            h.source_dir.insert("//a:b".into());
        });
        assert_eq!(classify("//a:b", Side::Tool, &h), Class::CambiumMissing);
    }

    #[test]
    fn the_source_directory_outranks_the_nothing_could_have_impacted_it_classes() {
        // The `install_src` parents satisfy the external-content conjunction
        // (a toolchain input) and the line-shift one (a `BUILD` edit *is* a
        // changed path under the package directory). Both are true and
        // neither is why Tango reported the label; the directory is, so it
        // is asked first. Pinned so a reorder has to argue with this.
        let mut h = external_content_facts();
        h.build_changed.insert("//a:b".into());
        assert_eq!(
            classify("//a:b", Side::Tool, &h),
            Class::BazeldiffExtraExternalContent,
            "without the directory the older explanation stands"
        );
        h.source_dir.insert("//a:b".into());
        h.oracle_ran = true;
        assert_eq!(
            classify("//a:b", Side::Tool, &h),
            Class::ToolExtraSourceDirectory
        );
        // But never ahead of the two cheap certainties on the tool side: an
        // external label is a canonicalisation difference whatever it
        // consumes, and a changed path with no consumer is content.
        h.external.insert("//a:b".into());
        assert_eq!(
            classify("//a:b", Side::Tool, &h),
            Class::BazeldiffExtraExternal
        );
    }
}
