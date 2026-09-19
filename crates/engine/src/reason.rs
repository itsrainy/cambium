//! Why a run answered `//...`.
//!
//! A global run has exactly one cause, and three things are said about it: the
//! `global_reason` string in the report, the diagnostic that carries the code
//! and the `help:` line, and the log line. [`GlobalReason`] is the cause with
//! its data — the file, the flag, the `.bzl` paths — and the other three are
//! derived from it here, so they cannot disagree and nothing downstream has
//! to parse a sentence to learn which file it was about.
//!
//! **The `Display` wording is load-bearing.** The benchmark harness
//! histograms `global_reason` across runs and across releases, so every string
//! here is byte-for-byte what the run said before the enum existed; the test
//! at the bottom pins each one. Anything new about a cause goes in a new
//! field of the diagnostic, not in the sentence. A cause that changes *code*
//! keeps its sentence for the same reason: a toolchain registration is coded
//! with the rule-class escalation now, and says what it always said.

use std::fmt;

use crate::diag::{Code, Diagnostic};

/// Why a `.bazelrc` edit could not be shown irrelevant to loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BazelrcCause {
    /// A bazelrc that appears or vanishes changes which files Bazel reads at
    /// all, and the diff of a file against nothing is not a flag diff.
    AddedOrRemoved,
    /// `git show` had nothing for it at this revision.
    Unreadable {
        rev: String,
    },
    NotUtf8 {
        rev: String,
    },
    /// The screen read both texts and found something it cannot classify.
    Unknown(crate::bazelrc::Unknown),
}

impl fmt::Display for BazelrcCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BazelrcCause::AddedOrRemoved => f.write_str("the file was added or removed"),
            BazelrcCause::Unreadable { rev } => write!(f, "cannot be read at {rev}"),
            BazelrcCause::NotUtf8 { rev } => write!(f, "is not UTF-8 at {rev}"),
            BazelrcCause::Unknown(u) => write!(f, "{u}"),
        }
    }
}

/// Why a changed Starlark build setting could not be resolved against the
/// index (see [`crate::flags`] for the argument behind each).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagCause {
    /// The row source has no checkout, so no `.bzl` can be read.
    NoStarlarkSource,
    /// The base index was built without a load closure.
    NoLoadClosure,
    /// The flag's value is not a label cambium parses.
    Unparsed,
    /// A `.bzl` in the load closure names the setting: a transition, or some
    /// other Starlark read that is not an edge.
    ReadByBzl { path: String },
    /// The setting lives in a repository the index does not cover.
    External { repo: String },
}

impl fmt::Display for FlagCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlagCause::NoStarlarkSource => f.write_str(
                "the row source cannot read Starlark, so no `.bzl` can be checked for a \
                 transition that reads it",
            ),
            FlagCause::NoLoadClosure => f.write_str(
                "the base index carries no static .bzl load closure to check for a transition \
                 that reads it",
            ),
            FlagCause::Unparsed => f.write_str("is not a label cambium parses"),
            FlagCause::ReadByBzl { path } => write!(
                f,
                "and `{path}` in the load closure names it — a transition or other Starlark \
                 read of the setting is not an edge in the graph"
            ),
            FlagCause::External { repo } => write!(
                f,
                "and `@{repo}` is an external repository: the index does not cover its graph, \
                 so what inside it reads the setting cannot be seen"
            ),
        }
    }
}

/// What the external-repository stage could not account for.
///
/// The `why` strings come from `cambium_index`'s `WORKSPACE`, `MODULE.bazel`
/// and lockfile diffs, which say in their own words which statement they
/// could not localise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalsCause {
    /// A legacy `WORKSPACE` file appeared or vanished.
    WorkspaceAddedOrRemoved { path: String },
    /// The `WORKSPACE` diff has a statement it cannot localise.
    Workspace { path: String, why: String },
    /// A `WORKSPACE` edit changed a call from a `.bzl` the index cannot
    /// attribute repositories to.
    WorkspaceBzlUnattributed { path: String, bzl: String },
    /// The `MODULE.bazel` diff has a change it cannot localise.
    Module { why: String },
    /// A `WORKSPACE` statement that registers toolchains or execution
    /// platforms moved. The graph's gap rather than the stage's, so it is
    /// coded with the rule-class escalation ([`Code::W023ToolchainMoved`]).
    WorkspaceRegistration { path: String, why: String },
    /// `MODULE.bazel`'s `register_toolchains` / `register_execution_platforms`
    /// calls moved. Coded like [`ExternalsCause::WorkspaceRegistration`].
    ModuleRegistration { why: String },
    /// A repository the stage dirtied is one a registration pattern at head
    /// offers toolchains or execution platforms from: its declarations are
    /// not rows of `//...`, so the rule-class check cannot see them move, and
    /// the route that dirtied it reaches only label consumers and loaders.
    /// `file` is where the pattern was read. Coded like
    /// [`ExternalsCause::WorkspaceRegistration`].
    RegisteredRepository { repo: String, file: String },
    /// `MODULE.bazel.lock` appeared or vanished.
    LockAddedOrRemoved,
    /// The lockfile diff has a change it cannot localise.
    Lock { why: String },
    /// A changed file sits inside a directory that is a Bazel repository of
    /// its own, and nothing says which external repository that directory is.
    /// The main repository's graph has no node for anything in there, so the
    /// only sound answer is everything.
    NestedRepositoryUnmapped { dir: String },
}

impl fmt::Display for ExternalsCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalsCause::WorkspaceAddedOrRemoved { path } => write!(
                f,
                "`{path}` was added or removed, and every repository it declares with it"
            ),
            ExternalsCause::Workspace { path, why }
            | ExternalsCause::WorkspaceRegistration { path, why } => write!(f, "`{path}`: {why}"),
            ExternalsCause::WorkspaceBzlUnattributed { path, bzl } => write!(
                f,
                "`{path}` changes a call from `{bzl}`, and cambium cannot say which \
                 repositories that file defines"
            ),
            ExternalsCause::Module { why }
            | ExternalsCause::Lock { why }
            | ExternalsCause::ModuleRegistration { why } => f.write_str(why),
            ExternalsCause::RegisteredRepository { repo, file } => write!(
                f,
                "`@{repo}` moved, and `{file}` registers toolchains or execution platforms from \
                 it; toolchain resolution has no edge in the query graph, so no reverse walk can \
                 find what it moves"
            ),
            ExternalsCause::NestedRepositoryUnmapped { dir } => write!(
                f,
                "`{dir}` is a Bazel repository of its own, so `//...` never loaded anything \
                 in it, and cambium cannot say which external repository it is"
            ),
            ExternalsCause::LockAddedOrRemoved => f.write_str(
                "`MODULE.bazel.lock` was added or removed, so every resolved repository in it \
                 is new or gone",
            ),
        }
    }
}

impl ExternalsCause {
    /// Whether the cause is a toolchain registration moving: the one thing
    /// the stage refuses to localise for the graph's sake rather than its
    /// own, and which is therefore coded with the rule-class escalation
    /// ([`Code::W023ToolchainMoved`]) instead of [`Code::W014ExternalsGlobal`].
    pub fn is_registration(&self) -> bool {
        matches!(
            self,
            ExternalsCause::WorkspaceRegistration { .. }
                | ExternalsCause::ModuleRegistration { .. }
                | ExternalsCause::RegisteredRepository { .. }
        )
    }
}

/// What the splice did to a rule, for a reason that names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleChange {
    /// Not in the base index, or not a rule there.
    Added,
    /// Its rule hash moved, or its class did.
    Changed,
    /// Tombstoned: gone from a re-queried package, or with its package.
    Removed,
}

impl fmt::Display for RuleChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RuleChange::Added => "added",
            RuleChange::Changed => "changed",
            RuleChange::Removed => "removed",
        })
    }
}

/// The one cause a global run has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobalReason {
    /// A `.bazelrc` edit the screen could not clear.
    Bazelrc { path: String, cause: BazelrcCause },
    /// A Starlark build setting a `.bazelrc` edit changed, unresolvable.
    Flag {
        /// The rc command the flag was set under (`build`, `test`).
        command: String,
        /// The flag name as written, without its dashes.
        name: String,
        cause: FlagCause,
    },
    /// A changed path matched an invalidator path or glob.
    Invalidator { path: String },
    /// The base index carries no Skyframe loading layer, so which packages a
    /// change moved cannot be answered at all.
    NoLoadingLayer,
    /// `.bzl` files changed that the module system loads rather than any
    /// package's BUILD file, and the index could not name the repositories
    /// they define.
    WorkspaceLevelBzl { paths: Vec<String> },
    /// The external-repository stage escalated.
    Externals(ExternalsCause),
    /// The splice added, changed or tombstoned a `toolchain`, `platform` or
    /// `toolchain_type` rule. Toolchain resolution has no edge in the query
    /// graph, so nothing reaches what it moves. One rule is named; the log
    /// line carries the count.
    ToolchainRule {
        label: String,
        class: String,
        change: RuleChange,
    },
}

/// What every [`GlobalReason::ToolchainRule`] sentence ends with, after the
/// rule and what happened to it. Pinned by the tests below, and the shape
/// [`GlobalReason::decided_after_query`] reads back.
const TOOLCHAIN_RULE_TAIL: &str =
    "; toolchain resolution has no edge in the query graph, so no reverse walk can find what it \
     moves";

/// `` `one` `` for a single path, `3 changed `.bzl` files` for several.
fn bzl_files(paths: &[String]) -> String {
    match paths {
        [one] => format!("`{one}`"),
        many => format!("{} changed `.bzl` files", many.len()),
    }
}

impl fmt::Display for GlobalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlobalReason::Bazelrc { path, cause } => write!(f, "{path}: {cause}"),
            GlobalReason::Flag {
                command,
                name,
                cause,
            } => write!(f, "{command} --{name} changed {cause}"),
            GlobalReason::Invalidator { path } => write!(f, "{path} matches an invalidator"),
            GlobalReason::NoLoadingLayer => f.write_str(
                "the base index carries no skyframe loading layer, so which packages the \
                 change moved cannot be answered",
            ),
            // The wording is deliberately the one the `rbuildfiles` path used
            // before the static closure replaced it, so the benchmark's
            // reason histogram is comparable across both changes.
            GlobalReason::WorkspaceLevelBzl { paths } => write!(
                f,
                "{} is loaded by MODULE.bazel/WORKSPACE (repository rules), not by any \
                 package BUILD file; it is in the workspace load closure",
                bzl_files(paths)
            ),
            GlobalReason::Externals(cause) => write!(f, "{cause}"),
            GlobalReason::ToolchainRule {
                label,
                class,
                change,
            } => write!(f, "{label} ({class}) was {change}{TOOLCHAIN_RULE_TAIL}"),
        }
    }
}

impl GlobalReason {
    /// The diagnostic code this cause is reported under.
    pub fn code(&self) -> Code {
        match self {
            GlobalReason::Bazelrc { .. } => Code::W004BazelrcGlobal,
            GlobalReason::Flag { .. } => Code::W015FlagUnresolved,
            GlobalReason::Invalidator { .. } => Code::W013InvalidatorMatched,
            GlobalReason::NoLoadingLayer => Code::W008NoLoadingLayer,
            GlobalReason::WorkspaceLevelBzl { .. } => Code::W009WorkspaceLevelBzl,
            GlobalReason::Externals(c) if c.is_registration() => Code::W023ToolchainMoved,
            GlobalReason::Externals(_) => Code::W014ExternalsGlobal,
            GlobalReason::ToolchainRule { .. } => Code::W023ToolchainMoved,
        }
    }

    /// Whether a recorded reason sentence is one the run decides after the
    /// query, over the rows it read back: a toolchain-class rule the splice
    /// found moved ([`GlobalReason::ToolchainRule`]). Every other cause is
    /// decided by the stages before the query, which a dry run and an
    /// offline replay of the dirty stage reproduce; this one they cannot, so
    /// a reader holding a recorded global against those stages skips it.
    ///
    /// By the sentence rather than the enum because the recorded answer is
    /// the sentence: the benchmark harness stores `global_reason` and nothing
    /// structured beside it. The shape is `<label> (<class>) was <change>;
    /// <tail>`, with the class one of the toolchain classes.
    pub fn decided_after_query(reason: &str) -> bool {
        let Some(head) = reason.strip_suffix(TOOLCHAIN_RULE_TAIL) else {
            return false;
        };
        let Some((label_class, change)) = head.rsplit_once(" was ") else {
            return false;
        };
        let Some((label, class)) = label_class.rsplit_once(" (") else {
            return false;
        };
        let Some(class) = class.strip_suffix(')') else {
            return false;
        };
        !label.is_empty()
            && cambium_index::is_toolchain_rule_class(class)
            && ["added", "changed", "removed"].contains(&change)
    }

    /// Whether a diagnostic with this code is the one that explains a global
    /// run — the inverse of [`GlobalReason::code`], for a reader of a report
    /// that has the diagnostics but not the enum.
    pub fn is_cause(code: Code) -> bool {
        matches!(
            code,
            Code::W004BazelrcGlobal
                | Code::W008NoLoadingLayer
                | Code::W009WorkspaceLevelBzl
                | Code::W013InvalidatorMatched
                | Code::W014ExternalsGlobal
                | Code::W015FlagUnresolved
                | Code::W023ToolchainMoved
        )
    }

    /// The actionable diagnostic: the code, a message that ends with the
    /// reason itself, the file when there is one, and what to change.
    ///
    /// The message carrying the reason verbatim is what lets `cambi explain`
    /// print the diagnostic alone and never a second copy of the reason.
    pub fn diagnostic(&self) -> Diagnostic {
        let reason = self.to_string();
        match self {
            GlobalReason::Bazelrc { path, .. } => Diagnostic::warning(
                self.code(),
                format!("{path} changed and the run went global: {reason}"),
            )
            // The screen works on flag triples rather than on lines, so it
            // knows the file but not where in it.
            .at(path.clone(), None)
            .with_help(
                "add the flag to [bazelrc].allowlist_extra if it cannot affect loading or \
                 analysis",
            ),
            GlobalReason::Flag { .. } => Diagnostic::warning(
                self.code(),
                format!(
                    "a Starlark build setting a .bazelrc edit changed could not be resolved \
                     against the index, so everything is affected: {reason}"
                ),
            )
            .with_help(
                "move the setting into the main repository, or add the flag to \
                 [bazelrc].allowlist_extra if it cannot affect analysis",
            ),
            GlobalReason::Invalidator { path } => Diagnostic::warning(
                self.code(),
                format!(
                    "a changed path matches an invalidator, so everything is affected: {reason}"
                ),
            )
            .at(path.clone(), None)
            .with_help(
                "if the path cannot move the build graph, take it out of [invalidators].paths \
                 / globs, or add it to [ignore].globs",
            ),
            GlobalReason::NoLoadingLayer => Diagnostic::warning(
                self.code(),
                format!(
                    "nothing can be told apart in this change, so everything is affected: {reason}"
                ),
            )
            .with_help(
                "rebuild the lineage with `cambi index build` against a bazel that can run \
                 `dump --skyframe=deps`",
            ),
            GlobalReason::WorkspaceLevelBzl { paths } => {
                let d = Diagnostic::warning(
                    self.code(),
                    format!(
                        "a changed .bzl is loaded outside any package, so everything is \
                         affected: {reason}"
                    ),
                )
                .with_help(
                    "expected for a workspace-level .bzl; if the file is not really in the \
                     workspace closure, rebuild the index so its closure is current",
                );
                match paths.first() {
                    Some(p) => d.at(p.clone(), None),
                    None => d,
                }
            }
            GlobalReason::Externals(c) if c.is_registration() => {
                let d = Diagnostic::warning(
                    self.code(),
                    format!("a toolchain registration moved, so everything is affected: {reason}"),
                )
                .with_help(
                    "expected: a changed registration can resolve a different toolchain for \
                     every target, and resolution has no query-graph edge; nothing to configure",
                );
                match c {
                    ExternalsCause::WorkspaceRegistration { path, .. } => d.at(path.clone(), None),
                    ExternalsCause::RegisteredRepository { file, .. } => d.at(file.clone(), None),
                    _ => d.at("MODULE.bazel", None),
                }
            }
            GlobalReason::Externals(_) => Diagnostic::warning(
                self.code(),
                format!(
                    "the external-repository stage could not account for a change, so \
                     everything is affected: {reason}"
                ),
            )
            .with_help(
                "the message names the file; [externals].enabled = false turns the stage off \
                 if its precision is not wanted here",
            ),
            GlobalReason::ToolchainRule { .. } => Diagnostic::warning(
                self.code(),
                format!("a toolchain declaration moved, so everything is affected: {reason}"),
            )
            .with_help(
                "expected: a toolchain, platform or toolchain_type rule is resolved in analysis, \
                 not depended on, so no reverse walk can bound what it moves; nothing to \
                 configure",
            ),
        }
    }
}

/// Why a run gave up on splicing the dirty set and re-indexed instead.
///
/// The wording is fixed: the benchmark harness histograms this string, so it
/// must stay comparable across releases. Anything new about the overflow goes
/// in the log fields beside it.
pub fn cold_rebuild_reason(dirty: usize, limit: usize) -> String {
    format!("dirty packages {dirty} exceed limits.max_dirty_packages = {limit}")
}

/// The diagnostic a cold rebuild raises; like a global cause, its message
/// carries the reason verbatim.
pub fn cold_rebuild_diagnostic(reason: &str) -> Diagnostic {
    Diagnostic::warning(
        Code::W012ColdRebuild,
        format!(
            "the run re-indexed the whole repository at head rather than splicing: {reason}. \
             The answer is still exact"
        ),
    )
    .with_help("raise limits.max_dirty_packages, or set it to 0 to never cold-rebuild")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bazelrc::Unknown;

    /// One of every variant, with the exact string each produced before the
    /// enum existed. The benchmark histograms these; a changed byte is a
    /// changed bucket.
    fn pinned() -> Vec<(GlobalReason, &'static str)> {
        vec![
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::AddedOrRemoved,
                },
                ".bazelrc: the file was added or removed",
            ),
            (
                GlobalReason::Bazelrc {
                    path: "ci.bazelrc".into(),
                    cause: BazelrcCause::Unreadable { rev: "abc".into() },
                },
                "ci.bazelrc: cannot be read at abc",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::NotUtf8 { rev: "abc".into() },
                },
                ".bazelrc: is not UTF-8 at abc",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::Unknown(Unknown::NotAllowlisted {
                        command: "build".into(),
                        name: "define".into(),
                    }),
                },
                ".bazelrc: build --define changed, not on the loading-irrelevant allowlist",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::Unknown(Unknown::ImportChanged {
                        import: "import %workspace%/ci.bazelrc".into(),
                    }),
                },
                ".bazelrc: `import %workspace%/ci.bazelrc` changed; an imported rc file can \
                 carry anything",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::Unknown(Unknown::StartupFlag {
                        name: "host_jvm_args".into(),
                    }),
                },
                ".bazelrc: startup --host_jvm_args changed, and startup options are never \
                 classified",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::Unknown(Unknown::ConfigScoped {
                        command: "build".into(),
                        config: "ci".into(),
                        name: "@envoy//bazel:crypto".into(),
                    }),
                },
                ".bazelrc: build:ci --@envoy//bazel:crypto changed, and a flag set under a \
                 --config applies only to the runs that name it, which cambium does not model",
            ),
            (
                GlobalReason::Flag {
                    command: "build".into(),
                    name: "@envoy//bazel:crypto".into(),
                    cause: FlagCause::NoStarlarkSource,
                },
                "build --@envoy//bazel:crypto changed the row source cannot read Starlark, so \
                 no `.bzl` can be checked for a transition that reads it",
            ),
            (
                GlobalReason::Flag {
                    command: "build".into(),
                    name: "@envoy//bazel:crypto".into(),
                    cause: FlagCause::NoLoadClosure,
                },
                "build --@envoy//bazel:crypto changed the base index carries no static .bzl \
                 load closure to check for a transition that reads it",
            ),
            (
                GlobalReason::Flag {
                    command: "build".into(),
                    name: "define".into(),
                    cause: FlagCause::Unparsed,
                },
                "build --define changed is not a label cambium parses",
            ),
            (
                GlobalReason::Flag {
                    command: "build".into(),
                    name: "@envoy//bazel:crypto".into(),
                    cause: FlagCause::ReadByBzl {
                        path: "bazel/defs.bzl".into(),
                    },
                },
                "build --@envoy//bazel:crypto changed and `bazel/defs.bzl` in the load closure \
                 names it — a transition or other Starlark read of the setting is not an edge \
                 in the graph",
            ),
            (
                GlobalReason::Flag {
                    command: "build".into(),
                    name: "@quiche//:ssl_lib".into(),
                    cause: FlagCause::External {
                        repo: "quiche".into(),
                    },
                },
                "build --@quiche//:ssl_lib changed and `@quiche` is an external repository: \
                 the index does not cover its graph, so what inside it reads the setting \
                 cannot be seen",
            ),
            (
                GlobalReason::Invalidator {
                    path: "MODULE.bazel".into(),
                },
                "MODULE.bazel matches an invalidator",
            ),
            (
                GlobalReason::NoLoadingLayer,
                "the base index carries no skyframe loading layer, so which packages the \
                 change moved cannot be answered",
            ),
            (
                GlobalReason::WorkspaceLevelBzl {
                    paths: vec!["bazel/ext.bzl".into()],
                },
                "`bazel/ext.bzl` is loaded by MODULE.bazel/WORKSPACE (repository rules), not \
                 by any package BUILD file; it is in the workspace load closure",
            ),
            (
                GlobalReason::Externals(ExternalsCause::WorkspaceAddedOrRemoved {
                    path: "WORKSPACE".into(),
                }),
                "`WORKSPACE` was added or removed, and every repository it declares with it",
            ),
            (
                GlobalReason::Externals(ExternalsCause::Workspace {
                    path: "WORKSPACE".into(),
                    why: "a statement cambium cannot classify".into(),
                }),
                "`WORKSPACE`: a statement cambium cannot classify",
            ),
            (
                GlobalReason::Externals(ExternalsCause::WorkspaceBzlUnattributed {
                    path: "WORKSPACE".into(),
                    bzl: "bazel/repos.bzl".into(),
                }),
                "`WORKSPACE` changes a call from `bazel/repos.bzl`, and cambium cannot say \
                 which repositories that file defines",
            ),
            (
                GlobalReason::Externals(ExternalsCause::Module {
                    why: "a toolchain module moved".into(),
                }),
                "a toolchain module moved",
            ),
            (
                GlobalReason::Externals(ExternalsCause::LockAddedOrRemoved),
                "`MODULE.bazel.lock` was added or removed, so every resolved repository in it \
                 is new or gone",
            ),
            (
                GlobalReason::Externals(ExternalsCause::Lock {
                    why: "a novel section".into(),
                }),
                "a novel section",
            ),
            // The two registration causes say exactly what they said under
            // the stage's own code; only the code moved.
            (
                GlobalReason::Externals(ExternalsCause::ModuleRegistration {
                    why: "`register_toolchains` / `register_execution_platforms` changed; \
                          toolchain resolution has no edge in the query graph, so no reverse \
                          walk can find what it moves"
                        .into(),
                }),
                "`register_toolchains` / `register_execution_platforms` changed; toolchain \
                 resolution has no edge in the query graph, so no reverse walk can find what it \
                 moves",
            ),
            (
                GlobalReason::Externals(ExternalsCause::WorkspaceRegistration {
                    path: "WORKSPACE".into(),
                    why: "`go_register_toolchains(version = \"2\")` registers toolchains, and \
                          toolchain resolution has no edge in the query graph"
                        .into(),
                }),
                "`WORKSPACE`: `go_register_toolchains(version = \"2\")` registers toolchains, \
                 and toolchain resolution has no edge in the query graph",
            ),
            (
                GlobalReason::ToolchainRule {
                    label: "//tools/cpp:linux_toolchain".into(),
                    class: "toolchain".into(),
                    change: RuleChange::Added,
                },
                "//tools/cpp:linux_toolchain (toolchain) was added; toolchain resolution has no \
                 edge in the query graph, so no reverse walk can find what it moves",
            ),
            (
                GlobalReason::ToolchainRule {
                    label: "//platforms:linux_arm64".into(),
                    class: "platform".into(),
                    change: RuleChange::Removed,
                },
                "//platforms:linux_arm64 (platform) was removed; toolchain resolution has no \
                 edge in the query graph, so no reverse walk can find what it moves",
            ),
            (
                GlobalReason::ToolchainRule {
                    label: "//constraints:libc".into(),
                    class: "constraint_setting".into(),
                    change: RuleChange::Changed,
                },
                "//constraints:libc (constraint_setting) was changed; toolchain resolution has \
                 no edge in the query graph, so no reverse walk can find what it moves",
            ),
            (
                GlobalReason::Externals(ExternalsCause::RegisteredRepository {
                    repo: "llvm_toolchain".into(),
                    file: "MODULE.bazel".into(),
                }),
                "`@llvm_toolchain` moved, and `MODULE.bazel` registers toolchains or execution \
                 platforms from it; toolchain resolution has no edge in the query graph, so no \
                 reverse walk can find what it moves",
            ),
        ]
    }

    /// The one verdict reached after the query is the rule-class one, and it
    /// is told apart by its sentence alone, since that is all a recorded
    /// answer keeps: every other pinned sentence, the two registration causes
    /// with the same tail included, is decided before the query.
    #[test]
    fn only_the_rule_class_sentence_is_decided_after_the_query() {
        for (reason, text) in pinned() {
            let after = matches!(reason, GlobalReason::ToolchainRule { .. });
            assert_eq!(GlobalReason::decided_after_query(text), after, "{reason:?}");
        }
        assert!(!GlobalReason::decided_after_query(""));
        assert!(!GlobalReason::decided_after_query(
            "//a:b (filegroup) was changed; toolchain resolution has no edge in the query graph, \
             so no reverse walk can find what it moves"
        ));
    }

    /// A registration is the graph's gap, not the stage's: it is coded with
    /// the rule-class escalation and located at the file it was read from,
    /// while every other externals cause keeps the stage's code.
    #[test]
    fn a_registration_is_coded_with_the_toolchain_rule_and_the_rest_of_the_stage_is_not() {
        let module =
            GlobalReason::Externals(ExternalsCause::ModuleRegistration { why: "x".into() });
        assert_eq!(module.code(), Code::W023ToolchainMoved);
        assert_eq!(
            module.diagnostic().location.map(|l| l.path),
            Some("MODULE.bazel".to_string())
        );
        let ws = GlobalReason::Externals(ExternalsCause::WorkspaceRegistration {
            path: "WORKSPACE.bazel".into(),
            why: "x".into(),
        });
        assert_eq!(ws.code(), Code::W023ToolchainMoved);
        assert_eq!(
            ws.diagnostic().location.map(|l| l.path),
            Some("WORKSPACE.bazel".to_string())
        );
        let registered = GlobalReason::Externals(ExternalsCause::RegisteredRepository {
            repo: "x".into(),
            file: "WORKSPACE".into(),
        });
        assert_eq!(registered.code(), Code::W023ToolchainMoved);
        assert_eq!(
            registered.diagnostic().location.map(|l| l.path),
            Some("WORKSPACE".to_string())
        );
        for other in [
            ExternalsCause::Module { why: "x".into() },
            ExternalsCause::Lock { why: "x".into() },
            ExternalsCause::Workspace {
                path: "WORKSPACE".into(),
                why: "x".into(),
            },
            ExternalsCause::LockAddedOrRemoved,
        ] {
            assert!(!other.is_registration(), "{other:?}");
            assert_eq!(
                GlobalReason::Externals(other).code(),
                Code::W014ExternalsGlobal
            );
        }
        let rule = GlobalReason::ToolchainRule {
            label: "//a:b".into(),
            class: "toolchain_type".into(),
            change: RuleChange::Changed,
        };
        assert_eq!(rule.code(), Code::W023ToolchainMoved);
    }

    #[test]
    fn every_reason_displays_as_the_string_the_harness_histograms() {
        for (reason, text) in pinned() {
            assert_eq!(reason.to_string(), text, "{reason:?}");
        }
    }

    #[test]
    fn every_diagnostic_carries_its_reason_verbatim_and_a_help_line() {
        for (reason, text) in pinned() {
            let d = reason.diagnostic();
            assert_eq!(d.code, reason.code());
            assert!(GlobalReason::is_cause(d.code), "{:?}", d.code);
            assert!(
                d.message.ends_with(text),
                "the message must end with the reason so `explain` never prints it twice: {}",
                d.message
            );
            assert!(d.help.is_some(), "{d:?}");
        }
    }

    #[test]
    fn a_cause_that_names_a_file_locates_the_diagnostic_there() {
        let d = GlobalReason::Invalidator {
            path: "tools/x".into(),
        }
        .diagnostic();
        assert_eq!(
            d.location.as_ref().map(|l| l.path.as_str()),
            Some("tools/x")
        );
        let d = GlobalReason::WorkspaceLevelBzl {
            paths: vec!["a.bzl".into(), "b.bzl".into()],
        }
        .diagnostic();
        assert_eq!(d.location.as_ref().map(|l| l.path.as_str()), Some("a.bzl"));
    }

    #[test]
    fn the_cold_rebuild_reason_is_pinned_too() {
        assert_eq!(
            cold_rebuild_reason(9001, 500),
            "dirty packages 9001 exceed limits.max_dirty_packages = 500"
        );
        let d = cold_rebuild_diagnostic(&cold_rebuild_reason(9001, 500));
        assert_eq!(d.code, Code::W012ColdRebuild);
        assert!(d.message.contains("exceed limits.max_dirty_packages = 500"));
    }
}
