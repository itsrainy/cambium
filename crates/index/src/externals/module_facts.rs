//! The structured content of a `MODULE.bazel`, and what a change to it means.

use std::collections::{BTreeMap, BTreeSet};

/// One `bazel_dep`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dep {
    pub module: String,
    pub version: String,
    /// The apparent name the main repository sees it under.
    pub repo_name: String,
}

/// One `use_extension` plus the tags and `use_repo` calls against it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extension {
    /// The `.bzl` label the extension is defined in, as written.
    pub bzl: String,
    pub name: String,
    /// Apparent repository names this extension is asked for, from `use_repo`.
    pub repos: BTreeSet<String>,
    /// The renames in those `use_repo` calls: `use_repo(ext, apparent =
    /// "internal")` records `internal -> apparent`. The extension itself, and
    /// so `MODULE.bazel.lock`, only ever writes the internal name.
    pub renames: BTreeMap<String, String>,
    /// Canonical rendering of each tag call, for diffing.
    pub tags: Vec<String>,
    /// Main-repo labels named in the tag calls.
    pub labels: Vec<String>,
    /// Rendered `override_repo` / `inject_repo` calls against this extension's
    /// proxy, in source order.
    ///
    /// `override_repo(ext, "a", b = "c")` makes the name `a` inside the
    /// extension resolve to the main module's `a`, and `b` to the main
    /// module's `c`; `inject_repo(ext, "x")` makes `x` visible inside an
    /// extension that never asked for it. Either way the extension's
    /// repositories are built against a different set of repositories, so a
    /// change to one of these calls moves every repository it generates.
    pub overrides: Vec<String>,
}

impl Extension {
    /// The name the main repository sees a repository of this extension under,
    /// given the name the extension itself gave it. `None` when no `use_repo`
    /// of the main module mentions it at all.
    pub fn apparent_name(&self, internal: &str) -> Option<String> {
        if let Some(a) = self.renames.get(internal) {
            return Some(a.clone());
        }
        self.repos.contains(internal).then(|| internal.to_string())
    }
}

/// One `use_repo_rule` plus its instantiations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoRule {
    pub bzl: String,
    pub rule: String,
    /// The `name =` of each instantiation.
    pub repos: BTreeSet<String>,
    pub calls: Vec<String>,
    pub labels: Vec<String>,
}

/// The structured content of a `MODULE.bazel`.
///
/// Only what a diff needs: everything else is deliberately dropped, and
/// anything the parser did not recognise lands in [`ModuleFacts::unparsed`],
/// which forces a global invalidation rather than a guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleFacts {
    /// `module(name = ..., version = ...)`, rendered.
    pub module: String,
    /// The root module's own name, which is also an apparent name for the
    /// main repository.
    pub module_name: String,
    /// By apparent repository name.
    pub deps: BTreeMap<String, Dep>,
    /// Module name -> rendered override call.
    pub overrides: BTreeMap<String, String>,
    /// Module name -> the main-repo directory a `local_path_override` points
    /// at, with its trailing `/`.
    pub local_paths: BTreeMap<String, String>,
    /// Repository name -> the main-repo directory a `local_repository` or
    /// `new_local_repository` instantiation points at, with its trailing `/`.
    /// A `use_repo_rule` shape; the `.bzl` scanner finds the WORKSPACE one.
    pub local_repo_paths: BTreeMap<String, String>,
    /// Keyed by `bzl\0name`.
    pub extensions: BTreeMap<String, Extension>,
    /// Keyed by `bzl\0rule`.
    pub repo_rules: BTreeMap<String, RepoRule>,
    /// `register_toolchains` / `register_execution_platforms` arguments, in
    /// order. Toolchain resolution has no edge in the graph, so any movement
    /// here is global.
    pub registrations: Vec<String>,
    /// Top-level statements the parser did not recognise, rendered. A run that
    /// sees one of these on either side of a diff goes global.
    pub unparsed: Vec<String>,
}

/// What a `MODULE.bazel` edit means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleChange {
    /// Nothing a run can localise: invalidate everything, for this reason.
    Global(String),
    /// `register_toolchains` / `register_execution_platforms` moved. As global
    /// as [`ModuleChange::Global`], and kept apart so the run can report it
    /// under the toolchain code rather than the stage's own: the gap is the
    /// graph's (toolchain resolution has no edge), not the stage's.
    Registration(String),
    /// Exactly these apparent repository names moved.
    Repos(BTreeSet<String>),
}

/// The apparent name the main repository sees a module under: the `repo_name`
/// of its `bazel_dep`, when it has one.
pub fn apparent_of(f: &ModuleFacts, module: &str) -> Option<String> {
    f.deps
        .values()
        .find(|d| d.module == module)
        .map(|d| d.repo_name.clone())
}
