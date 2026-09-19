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
    /// Repository name -> the `build_file =` label of the repository-rule
    /// instantiation that declares it. The sources are fetched, the BUILD
    /// file is a file of this tree: envoy's `@quiche` before the bzlmod pin.
    /// A `build_file_content =` instead of a label records the empty string,
    /// which is the same shape (an overlay) with no tree file to read.
    pub build_files: BTreeMap<String, String>,
    /// Keyed by `bzl\0name`.
    pub extensions: BTreeMap<String, Extension>,
    /// Keyed by `bzl\0rule`.
    pub repo_rules: BTreeMap<String, RepoRule>,
    /// `register_toolchains` / `register_execution_platforms` arguments, in
    /// order. Toolchain resolution has no edge in the graph, so any movement
    /// here is global.
    pub registrations: Vec<String>,
    /// The repositories those registrations offer rules from, by the name
    /// written in the pattern (`register_toolchains("@x//:all")` names `x`).
    /// A main-repo pattern names none: its rules are rows of `//...`. The
    /// root module's own name can appear here (`@demo//tc:all`), and it is
    /// the reader's business to know it is the main repository.
    pub registration_repos: BTreeSet<String>,
    /// Main-repo paths of the `include()`d segments (Bazel 7.2 and later),
    /// in order. A segment is textually part of this file; whoever reads it
    /// follows these at the same revision and [`ModuleFacts::absorb`]s each
    /// one's facts, which also appends the segment's own includes here.
    pub includes: Vec<String>,
    /// Top-level statements the parser did not recognise, rendered. A run that
    /// sees one of these on either side of a diff goes global.
    pub unparsed: Vec<String>,
}

impl ModuleFacts {
    /// Merges an `include()`d segment's facts into the root's.
    ///
    /// Bazel reads an included file as if its text stood where the
    /// `include()` call is, except that variable bindings stay in the file
    /// they were made in; scanning each file on its own and merging the
    /// results is exactly that. An extension asked for from two files is one
    /// extension with both files' tags, as it is for Bazel. The `module()`
    /// declaration is the root's alone.
    pub fn absorb(&mut self, other: ModuleFacts) {
        self.deps.extend(other.deps);
        self.overrides.extend(other.overrides);
        self.local_paths.extend(other.local_paths);
        self.local_repo_paths.extend(other.local_repo_paths);
        for (key, e) in other.extensions {
            let mine = self.extensions.entry(key).or_default();
            if mine.bzl.is_empty() {
                mine.bzl = e.bzl;
                mine.name = e.name;
            }
            mine.repos.extend(e.repos);
            mine.renames.extend(e.renames);
            mine.tags.extend(e.tags);
            mine.labels.extend(e.labels);
            mine.overrides.extend(e.overrides);
        }
        for (key, r) in other.repo_rules {
            let mine = self.repo_rules.entry(key).or_default();
            if mine.bzl.is_empty() {
                mine.bzl = r.bzl;
                mine.rule = r.rule;
            }
            mine.repos.extend(r.repos);
            mine.calls.extend(r.calls);
            mine.labels.extend(r.labels);
        }
        self.registrations.extend(other.registrations);
        self.registration_repos.extend(other.registration_repos);
        self.includes.extend(other.includes);
        self.unparsed.extend(other.unparsed);
    }
}

/// Whether the Bazel reading this tree autoloads native rules from the
/// modules on [`crate::vocabulary::AUTOLOAD_PROVIDERS`].
///
/// Bazel 8 turned `--incompatible_autoload_externally` on by default, and
/// under it a bump of a provider module moves rules in packages that load
/// nothing from it. Bazel 7.4 introduced the flag with an empty default, and
/// Bazel 9 disabled autoloads altogether: the option is a deprecated no-op
/// there, its `exports.bzl` defines no rule to autoload (an unloaded
/// `cc_library` fails, asking for a `load()`), and so every rule a provider
/// gives a package arrives through a `load()` the package writes, which is an
/// edge. The version therefore says autoload for 8 and only 8, and the flag
/// says otherwise only where Bazel reads it ([`Autoload::reads_flag`]).
///
/// The diffs take this as a parameter rather than reading a version
/// themselves: which Bazel runs is the engine's knowledge (the index header,
/// `.bazelversion`, `.bazelrc`), and a version nothing states is read as
/// `On`, the side that over-reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autoload {
    /// A provider module that moved is global: Bazel 8 at its default, a
    /// Bazel that reads the flag with the flag naming a provider, or a Bazel
    /// nothing states.
    On,
    /// The native rules are Bazel's own or are loaded explicitly, and a
    /// provider module is an ordinary dependency (or a toolchain module, as
    /// before): Bazel 7 at its default, Bazel 9 and later whatever the flag
    /// says, or a Bazel that reads the flag with it set empty.
    Off,
}

impl Autoload {
    /// What a Bazel of this major version does at its default: 8 autoloads,
    /// every other known major does not, and an unknown one is read as 8.
    pub fn from_major(major: Option<u32>) -> Autoload {
        match major {
            Some(8) | None => Autoload::On,
            Some(_) => Autoload::Off,
        }
    }

    /// Whether a Bazel of this major version reads
    /// `--incompatible_autoload_externally` at all.
    ///
    /// 8 does (the flag is how an 8 tree opts out), and so does 7, where 7.4
    /// introduced it as the way to opt in early; an older 7 never sees the
    /// flag set, because a Bazel that does not know a flag will not start
    /// with it. Bazel 9 and later do not: autoloads are disabled there and
    /// the option is a deprecated no-op, so a line left over from an 8 tree
    /// says nothing about what that Bazel loads. An unknown version is read
    /// as 8 and reads the flag with it.
    pub fn reads_flag(major: Option<u32>) -> bool {
        matches!(major, Some(7) | Some(8) | None)
    }

    /// What an explicit `--incompatible_autoload_externally=<value>` says.
    ///
    /// The value is a comma-separated list of symbols and `@repository`
    /// entries. A bare entry autoloads (and fails if the repository is
    /// missing), a `+` entry autoloads when the repository is in the graph,
    /// and a `-` entry removes the symbol from Bazel's namespace so that only
    /// an explicit `load()` provides it, which is an edge. So the flag turns
    /// autoload on exactly when some entry is not a removal, and an empty
    /// value is the documented way to turn it off. Which repositories are
    /// named is not read: a flag naming any provider keeps every provider
    /// under autoload, the side that over-reports.
    pub fn from_flag(value: &str) -> Autoload {
        let autoloads = value
            .split(',')
            .map(str::trim)
            .any(|e| !e.is_empty() && !e.starts_with('-'));
        if autoloads {
            Autoload::On
        } else {
            Autoload::Off
        }
    }
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
    /// A module Bazel autoloads native rules from moved, under a Bazel that
    /// autoloads ([`Autoload::On`]): invalidate everything, for `why`, and
    /// say so under the autoload diagnostic rather than the stage's own.
    Autoload { module: String, why: String },
    /// Exactly these apparent repository names moved.
    Repos(BTreeSet<String>),
}

/// A reason the diff could not localise, as the readers write them.
impl From<String> for ModuleChange {
    fn from(why: String) -> Self {
        ModuleChange::Global(why)
    }
}

/// The apparent name the main repository sees a module under: the `repo_name`
/// of its `bazel_dep`, when it has one.
pub fn apparent_of(f: &ModuleFacts, module: &str) -> Option<String> {
    f.deps
        .values()
        .find(|d| d.module == module)
        .map(|d| d.repo_name.clone())
}
