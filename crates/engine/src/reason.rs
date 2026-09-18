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
//! field of the diagnostic, not in the sentence.

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
    /// `MODULE.bazel.lock` appeared or vanished.
    LockAddedOrRemoved,
    /// The lockfile diff has a change it cannot localise.
    Lock { why: String },
    /// A changed file sits inside a directory that is a Bazel repository of
    /// its own, and nothing says which external repository that directory is.
    /// The main repository's graph has no node for anything in there, so the
    /// only sound answer is everything.
    NestedRepositoryUnmapped { dir: String },
    /// An rc file the query reads changed `--registry`, and `MODULE.bazel.lock`
    /// did not move with it, so which modules the new registries re-point
    /// cannot be read from the tree. See `crate::bazelrc`, "`--registry`".
    RegistryWithoutLock { path: String },
}

impl fmt::Display for ExternalsCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalsCause::WorkspaceAddedOrRemoved { path } => write!(
                f,
                "`{path}` was added or removed, and every repository it declares with it"
            ),
            ExternalsCause::Workspace { path, why } => write!(f, "`{path}`: {why}"),
            ExternalsCause::WorkspaceBzlUnattributed { path, bzl } => write!(
                f,
                "`{path}` changes a call from `{bzl}`, and cambium cannot say which \
                 repositories that file defines"
            ),
            ExternalsCause::Module { why } | ExternalsCause::Lock { why } => f.write_str(why),
            ExternalsCause::NestedRepositoryUnmapped { dir } => write!(
                f,
                "`{dir}` is a Bazel repository of its own, so `//...` never loaded anything \
                 in it, and cambium cannot say which external repository it is"
            ),
            ExternalsCause::LockAddedOrRemoved => f.write_str(
                "`MODULE.bazel.lock` was added or removed, so every resolved repository in it \
                 is new or gone",
            ),
            ExternalsCause::RegistryWithoutLock { path } => write!(
                f,
                "`{path}` changed --registry and `MODULE.bazel.lock` did not move with it, so \
                 which modules the new registries re-point cannot be read"
            ),
        }
    }
}

/// The one cause a global run has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobalReason {
    /// A `.bazelrc` edit the screen could not clear.
    Bazelrc { path: String, cause: BazelrcCause },
    /// A Starlark build setting a `.bazelrc` edit changed, unresolvable.
    Flag {
        /// The rc scope the flag was set under, as the file writes it
        /// (`common`, or `common:ci` under a config the query runs with).
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
}

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
            GlobalReason::Externals(_) => Code::W014ExternalsGlobal,
        }
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
                "add the flag to [bazelrc].allowlist_extra if it cannot change what `bazel \
                 query` loads; a flag under a --config the query does not run with is already \
                 ignored, and [bazelrc].configs names the ones it does run with",
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
                        command: "common".into(),
                        config: None,
                        name: "repo_env".into(),
                    }),
                },
                ".bazelrc: common --repo_env changed, not on the loading-irrelevant allowlist",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::Unknown(Unknown::NotAllowlisted {
                        command: "common".into(),
                        config: Some("ci".into()),
                        name: "repo_env".into(),
                    }),
                },
                ".bazelrc: common:ci --repo_env changed, not on the loading-irrelevant allowlist",
            ),
            (
                GlobalReason::Bazelrc {
                    path: ".bazelrc".into(),
                    cause: BazelrcCause::Unknown(Unknown::ConfigUndefined {
                        command: "common".into(),
                        config: "ci".into(),
                    }),
                },
                ".bazelrc: common --config=ci changed, and no rc file defines `ci` for any \
                 command",
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
            (
                GlobalReason::Externals(ExternalsCause::RegistryWithoutLock {
                    path: ".bazelrc".into(),
                }),
                "`.bazelrc` changed --registry and `MODULE.bazel.lock` did not move with it, \
                 so which modules the new registries re-point cannot be read",
            ),
        ]
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
