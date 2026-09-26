//! Path classification and the dirty set.
//!
//! This is where a list of changed paths becomes "this file marks its
//! consumers direct", "no package owns this", or "everything is affected". It
//! is pure: no Bazel, no git, no filesystem.
//!
//! # What this no longer decides
//!
//! *Which packages must be re-queried* used to be answered here too, from
//! three models of Bazel's loading phase: a new `BUILD` file carves a
//! sub-package out of its parent, so dirty both; an added or deleted file may
//! enter or leave a `glob`, so dirty its package; a changed `.bzl` dirties the
//! packages a statically parsed load closure says load it. All three are gone.
//! The index carries Bazel's own loading-phase graph
//! (`cambium_index::sky`), and the engine's dirty stage walks it: a changed
//! path becomes the Skyframe keys it moved, and every `PACKAGE` or
//! `PACKAGE_LOOKUP` node the reverse walk reaches is a dirty package. That is
//! the same question answered from the edges Bazel recorded rather than from a
//! restatement of the rules Bazel applies.
//!
//! What is left here is the part that is genuinely about paths: which package
//! owns a path at head (so a changed file can be resolved to a source label),
//! which paths invalidate everything, which are inside a nested repository,
//! and which are not in the graph at all.
//!
//! It stays pure by being handed what a path cannot tell it. One such fact is
//! which directories of the tree are Bazel repositories of their own. `bazel
//! query //...` stops at a subdirectory carrying a WORKSPACE or a
//! MODULE.bazel, so the index has no package under one, and a path there is
//! not this repository's to classify. The index records those directories when
//! it is built and [`PackageSet::from_index`] carries them in, which is what
//! stops a changed `third_party/xla/service/BUILD` from minting a package name
//! Bazel will never load. Such a path becomes [`Classification::Nested`] and
//! lands in [`DirtySet::nested`]; what to do about the repository it names is
//! the engine's question.
//!
//! Not every such directory stopped `//...`. Which markers stop it differs by
//! Bazel version, and xla's `third_party/tsl` is walked into anyway. A path
//! under a traversed boundary is both a main-repo file, classified like any
//! other, and a file of the repository the directory is, so [`dirty_set`]
//! also records it in [`DirtySet::nested`].

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

use cambium_diff::{Change, Status};
use cambium_index::{label, IndexRef};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rustc_hash::FxHashSet;

#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("invalid glob {pattern:?}: {source}")]
    BadGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

/// Path patterns that decide how a change is treated. Built from `cambium.toml`.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// Exact repo-relative paths that invalidate everything.
    pub invalidator_paths: Vec<String>,
    /// Globs that invalidate everything.
    pub invalidator_globs: Vec<String>,
    /// Globs whose changes are irrelevant to the build graph.
    pub ignore_globs: Vec<String>,
}

/// Compiled form of [`Rules`].
pub struct Matchers {
    exact: FxHashSet<String>,
    invalidators: GlobSet,
    ignores: GlobSet,
    exempt: FxHashSet<String>,
}

impl Matchers {
    pub fn compile(rules: &Rules) -> Result<Self, ClassifyError> {
        Ok(Self {
            exact: rules.invalidator_paths.iter().cloned().collect(),
            invalidators: build_globset(&rules.invalidator_globs)?,
            ignores: build_globset(&rules.ignore_globs)?,
            exempt: FxHashSet::default(),
        })
    }

    /// Paths the caller has already accounted for precisely, and which must
    /// therefore not fire an invalidator rule.
    ///
    /// The only user is the engine's external-repository stage: `MODULE.bazel`
    /// is an invalidator by default because a bzlmod edit can change anything,
    /// but once the stage has diffed it and named the repositories that moved,
    /// re-invalidating the world for the same file would throw that answer
    /// away. An exemption is a claim by the caller that it handled the file,
    /// not a claim that the file does not matter.
    pub fn exempt(&mut self, paths: impl IntoIterator<Item = String>) {
        self.exempt.extend(paths);
    }

    /// A path listed in `[invalidators].paths` is an invalidator, and so is
    /// anything matching a configured glob (no glob is on by default).
    ///
    /// The defaults are the files that can change anything and that the graph
    /// carries no edge to (`MODULE.bazel`, `.bazelrc`, `.bazelversion`,
    /// `REPO.bazel`, ...). A root-level `BUILD` file is deliberately not
    /// among them: its `package()` call applies to its own package and no
    /// other, and everything else in it is an ordinary target in the root
    /// package, whose consumers the graph already knows.
    pub fn is_invalidator(&self, path: &str) -> bool {
        !self.exempt.contains(path)
            && (self.exact.contains(path) || self.invalidators.is_match(path))
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        self.ignores.is_match(path)
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, ClassifyError> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).map_err(|source| ClassifyError::BadGlob {
            pattern: p.clone(),
            source,
        })?);
    }
    b.build().map_err(|source| ClassifyError::BadGlob {
        pattern: patterns.join(","),
        source,
    })
}

/// The main repository's package layout: which package owns a path, and
/// which nested repository a path is in.
///
/// Built from an index by [`PackageSet::from_index`], and answers for the
/// tree that index was built from. Given the base index, [`dirty_set`]
/// overlays the diff's BUILD file additions and deletions, so its answers
/// are the ones for head.
pub struct PackageSet {
    packages: FxHashSet<String>,
    /// Directories that are Bazel repositories of their own, each with a
    /// trailing `/`. See [`PackageSet::boundary_of`].
    boundaries: Vec<String>,
    /// The subset of `boundaries` that `//...` walked into anyway, each with
    /// a trailing `/`. See [`PackageSet::unwalked_boundary_of`].
    traversed: Vec<String>,
}

impl PackageSet {
    pub fn new(packages: impl IntoIterator<Item = String>) -> Self {
        Self {
            packages: packages.into_iter().collect(),
            boundaries: Vec::new(),
            traversed: Vec::new(),
        }
    }

    /// The directories below the root that are Bazel repositories of their
    /// own, as `cambium_index::repo_boundaries` found them: repo-relative and
    /// without a trailing slash.
    pub fn with_boundaries(mut self, dirs: impl IntoIterator<Item = String>) -> Self {
        self.boundaries = boundary_dirs(dirs);
        self
    }

    /// Of those directories, the ones the cold universe query returned a
    /// main-repo package under, as `cambium_index::Boundary::traversed`
    /// records them. Repo-relative and without a trailing slash.
    pub fn with_traversed(mut self, dirs: impl IntoIterator<Item = String>) -> Self {
        self.traversed = boundary_dirs(dirs);
        self
    }

    /// The nested repository `path` sits in whose files are not this
    /// repository's: a boundary `//...` really did stop at.
    ///
    /// The main repository has no package there, can never gain one from a
    /// changed path, and owns none of the files: tensorflow's
    /// `third_party/xla` is the repository `@xla`, and reading
    /// `third_party/xla/service/BUILD` as a main-repo BUILD file mints four
    /// hundred packages Bazel will not load.
    ///
    /// A boundary Bazel walked into anyway is not one of them. Which markers
    /// stop `//...` differs by Bazel version, and xla's `third_party/tsl`
    /// carries a `REPO.bazel` that Bazel 8.7 walks straight past: its
    /// packages are in the index, they are main-repo packages, and refusing
    /// to own a file there would drop every target that depends on it. So
    /// ownership is refused only where the universe query agrees that nothing
    /// of this repository is there.
    pub fn unwalked_boundary_of(&self, path: &str) -> Option<&str> {
        // Outermost first: that is where `//...` stopped, so an untraversed
        // boundary owns everything below it, inner boundaries included. Only
        // when a boundary was walked into does the question pass inwards, to
        // the repository vendored inside it that `//...` really did stop at.
        self.containing_boundaries(path)
            .find(|b| !self.is_traversed(b))
            .map(dir_of)
    }

    /// The boundaries `path` lies under, outermost first, each with its
    /// trailing `/`.
    fn containing_boundaries<'s, 'p>(
        &'s self,
        path: &'p str,
    ) -> impl Iterator<Item = &'s str> + use<'s, 'p> {
        // `boundaries` is sorted, and a prefix sorts before what it prefixes,
        // so this yields outer boundaries before the ones nested in them.
        self.boundaries
            .iter()
            .map(String::as_str)
            .filter(move |b| path.starts_with(b) || path == dir_of(b))
    }

    /// Whether `b` (with its trailing `/`) is a boundary the cold universe
    /// query returned a main-repo package under.
    fn is_traversed(&self, b: &str) -> bool {
        self.traversed
            .binary_search_by(|t| t.as_str().cmp(b))
            .is_ok()
    }

    /// The boundaries `path` lies under that `//...` walked into: the
    /// repositories a main-repo path is *also* a file of. See the third pass
    /// of [`dirty_set`].
    pub fn traversed_boundaries_of<'s, 'p>(
        &'s self,
        path: &'p str,
    ) -> impl Iterator<Item = &'s str> + use<'s, 'p> {
        self.containing_boundaries(path)
            .filter(|b| self.is_traversed(b))
            .map(dir_of)
    }

    /// The outermost boundary `path` lies under, traversed or not, without
    /// its trailing slash.
    ///
    /// This says whether a path is inside a nested repository's tree at all.
    /// It does not say whether the main repository owns the path: a boundary
    /// `//...` walked into anyway (xla's `third_party/tsl`) is answered here
    /// too, though the packages under it are this repository's. Ownership is
    /// [`PackageSet::unwalked_boundary_of`], which is what [`classify`] asks.
    pub fn boundary_of(&self, path: &str) -> Option<&str> {
        self.containing_boundaries(path).next().map(dir_of)
    }

    /// Whether `path` is a BUILD file of this repository: one that defines,
    /// adds or removes a main-repo package.
    ///
    /// Pass one of [`dirty_set`] applies these and pass two leaves them
    /// alone, and the engine names a package from one when its loading layer
    /// has no node for it. Both must answer alike, so both ask here.
    ///
    /// A BUILD file inside a nested repository defines a package of *that*
    /// repository: the main repository must not mint a package name from it,
    /// nor touch the parent package, which is in a different repository and
    /// cannot gain or lose files to it. A BUILD file under a boundary
    /// `//...` walked into anyway (xla's `third_party/tsl`) is this
    /// repository's, which is why the test is
    /// [`PackageSet::unwalked_boundary_of`] and not
    /// [`PackageSet::boundary_of`].
    pub fn is_main_repo_build_file(&self, path: &str) -> bool {
        is_build_file(path) && self.unwalked_boundary_of(path).is_none()
    }

    /// Live packages recorded in an index.
    pub fn from_index(index: IndexRef<'_>) -> Self {
        let packages = (0..index.num_packages())
            .filter(|&p| !index.package_tombstoned(p))
            .map(|p| index.package_name(p).to_string());
        let boundaries: Vec<cambium_index::Boundary> = index.boundaries().collect();
        Self::new(packages)
            .with_boundaries(boundaries.iter().map(|b| b.dir.clone()))
            .with_traversed(
                boundaries
                    .iter()
                    .filter(|b| b.traversed)
                    .map(|b| b.dir.clone()),
            )
    }

    pub fn contains(&self, pkg: &str) -> bool {
        self.packages.contains(pkg)
    }

    /// Records a package, unless the path is inside a nested repository: a
    /// directory `//...` never descended into has no main-repo package, and
    /// minting one from a changed path is what made the phantoms.
    pub fn insert(&mut self, pkg: String) {
        if self.unwalked_boundary_of(&pkg).is_some() {
            return;
        }
        self.packages.insert(pkg);
    }

    pub fn remove(&mut self, pkg: &str) {
        self.packages.remove(pkg);
    }

    /// Longest-prefix owner of `path`: walk the path's ancestors and return the
    /// first that is a package, in O(depth) hash lookups.
    pub fn owner_of(&self, path: &str) -> Option<&str> {
        if self.unwalked_boundary_of(path).is_some() {
            return None;
        }
        package_candidates(path)
            .find_map(|dir| self.packages.get(dir))
            .map(String::as_str)
    }
}

/// Boundary directories in the form [`PackageSet`] stores them: each with
/// one trailing `/`, sorted, without duplicates, and never the root.
///
/// Every boundary query leans on this shape. `is_traversed` binary-searches
/// it, and `containing_boundaries` relies on a prefix sorting before what it
/// prefixes to yield outer boundaries first.
fn boundary_dirs(dirs: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = dirs
        .into_iter()
        .filter_map(|d| {
            let dir = d.trim_end_matches('/');
            (!dir.is_empty()).then(|| format!("{dir}/"))
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// A stored boundary without its trailing `/`, as callers name it.
///
/// Boundaries are stored with the slash so that a prefix test for
/// `third_party/xla` cannot match a sibling such as `third_party/xla_other`.
fn dir_of(boundary: &str) -> &str {
    boundary.strip_suffix('/').unwrap_or(boundary)
}

/// True if the path is a package definition file.
pub fn is_build_file(path: &str) -> bool {
    matches!(basename(path), "BUILD" | "BUILD.bazel")
}

/// True if the path is a Starlark file whose contents can change rule
/// definitions. `.bzl` only: `.bazelrc` and friends are invalidators.
///
/// This is about *shape*, not about blast radius. What a changed `.bzl`
/// reaches is the loading layer's business (its `BZL_LOAD` node, and the
/// `BZL_LOAD -> BZL_LOAD` edges above it), and whether the rules it reached
/// include a toolchain declaration is the splice's: a `toolchain()`,
/// `platform()`, `toolchain_type()`, `constraint_setting()` or
/// `constraint_value()` moving is a whole-repository answer by rule class,
/// wherever it lives, because toolchain resolution is not a loading-phase
/// edge at all. No default invalidator glob names a directory
/// of `.bzl` files any more (`tools/toolchains/**` and `platforms/**` used
/// to, as a stand-in for that rule); a repository can configure one back,
/// and a path that matches an invalidator is an invalidator first, whatever
/// else it is.
pub fn is_bzl_file(path: &str) -> bool {
    path.ends_with(".bzl")
}

/// The directories that could be the package owning `path`, deepest first
/// and ending at the root package `""`.
///
/// This is the order [`PackageSet::owner_of`] tries them in.
fn package_candidates(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(dirname(path)), |dir| {
        (!dir.is_empty()).then(|| dirname(dir))
    })
}

pub fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

pub fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// What one changed path means for the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classification {
    /// Everything is affected.
    Invalidator,
    /// A BUILD file, defining the named package.
    Build(String),
    /// A `.bzl` file. Which packages it reaches is answered by the loading
    /// layer's reverse walk (`cambium_index::sky`), not from a path.
    Bzl(String),
    /// A file owned by the named package.
    Source(String),
    /// A file owned by the named package that also matches an ignore glob.
    /// Whether it really is out of the graph is a question for the index:
    /// see [`Consumers`].
    IgnoredSource(String),
    /// Not part of the build graph: no package owns it.
    Ignored,
    /// Inside the named nested repository, which is a Bazel repository of its
    /// own. No main-repo package owns it and none can be minted from it; what
    /// it moves is that repository, which is the engine's question, not this
    /// module's.
    Nested(String),
}

pub fn classify(path: &str, packages: &PackageSet, m: &Matchers) -> Classification {
    // Invalidators win over everything, including ignores.
    if m.is_invalidator(path) {
        return Classification::Invalidator;
    }
    // Before anything reads the path as a package: a nested repository's files
    // are not this repository's, whatever they are called.
    if let Some(dir) = packages.unwalked_boundary_of(path) {
        return Classification::Nested(dir.to_string());
    }
    if is_build_file(path) {
        return Classification::Build(dirname(path).to_string());
    }
    if is_bzl_file(path) {
        return Classification::Bzl(path.to_string());
    }
    // Ignore globs apply only to paths that are not BUILD/.bzl files, and
    // even then they do not settle the question on their own: a path the
    // build graph names as a source is an input whatever the glob says.
    match packages.owner_of(path) {
        Some(pkg) if m.is_ignored(path) => Classification::IgnoredSource(pkg.to_string()),
        Some(pkg) => Classification::Source(pkg.to_string()),
        // No BUILD file anywhere above: not part of any package.
        None => Classification::Ignored,
    }
}

/// Whether the build graph names a path as a source of some rule.
///
/// An ignore glob is a claim about paths the graph does not name ("`**/*.md`
/// is documentation, not input"). On a real repository that claim is often
/// wrong: `bazel/bazel` feeds its distribution tarball from
/// `filegroup(name = "srcs", srcs = glob(["**"]))` in every package, so
/// `CHANGELOG.md` is a genuine input of `//:changelog-file` and
/// `site/en/**/*.md` of `//site/en:srcs`. Taking the glob at its word there is
/// an under-approximation, the one class of bug this design does not
/// tolerate. So [`dirty_set`] asks the index instead and lets the glob decide
/// only the cases the graph does not contradict.
pub trait Consumers {
    /// Some rule consumes `path` (owned by `pkg`) as a source input.
    fn is_consumed(&self, pkg: &str, path: &str) -> bool;
}

/// The [`Consumers`] a real run uses: the base index's reverse row for the
/// path's source label.
pub struct IndexConsumers<'a>(pub IndexRef<'a>);

impl Consumers for IndexConsumers<'_> {
    fn is_consumed(&self, pkg: &str, path: &str) -> bool {
        let label = label::source_label(pkg, path);
        self.0
            .id_of(&label)
            .is_some_and(|id| !self.0.file_consumers(id).is_empty())
    }
}

/// The outcome of classifying a whole diff.
#[derive(Clone, Debug, Default)]
pub struct DirtySet {
    /// Everything is affected; the other fields are meaningless.
    pub global: bool,
    /// The changed path that matched an invalidator, when `global` is set.
    /// The only way this classifier's own answer goes global, so the reason,
    /// as data; the engine puts it into words.
    pub invalidator: Option<String>,
    /// Packages that must be re-queried.
    ///
    /// The classifier no longer puts anything here. The engine's dirty stage
    /// fills it from the loading layer's reverse walk and from the external
    /// repositories that moved; the field stays on this struct because it is
    /// what those stages accumulate into and what the query stage reads.
    pub dirty: BTreeSet<String>,
    /// Packages whose BUILD file the diff deleted.
    ///
    /// Still read from the diff, because it is a fact about a path and not
    /// about the graph: the loading layer answers "which packages moved", and
    /// a package that moved by ceasing to exist has to be tombstoned rather
    /// than re-queried. Whether it really ceased to exist is settled against
    /// the head checkout, not here (`run::keep_surviving_packages`).
    pub deleted_packages: BTreeSet<String>,
    /// Paths whose content changed, to be resolved to source labels and
    /// marked direct through the rule layer's file rows.
    pub direct_files: BTreeSet<String>,
    /// How many changed paths were ignored. The engine adds the ignored
    /// nested-repository paths and reports the total as the run's
    /// `ignored_files` stat.
    pub ignored: usize,
    /// Changed paths inside a nested Bazel repository, by the boundary
    /// directory they sit under. Nothing here dirties a main-repo package,
    /// because there is none; the engine routes each directory to the
    /// external repository it is, or gives up and says so.
    pub nested: BTreeMap<String, Vec<String>>,
}

impl DirtySet {
    /// Everything is affected because `path` matched an invalidator.
    fn invalidated_by(path: &str) -> Self {
        Self {
            global: true,
            invalidator: Some(path.to_string()),
            ..Default::default()
        }
    }
}

/// Classifies a diff: the path-shaped half of the dirty set.
///
/// `packages` must be the package set from the base index; this function
/// overlays the diff's BUILD file additions and deletions itself, because a
/// path's owner depends on which BUILD files exist at `head`, and the source
/// label a changed file resolves to depends on its owner.
///
/// It names no dirty package. Which packages a change moved is the loading
/// layer's answer, in `cambium_engine`'s dirty stage.
///
/// Three passes over the diff: the BUILD file changes first, so that every
/// other path is classified against the layout at head, then every other
/// path, then the paths under traversed boundaries a second time, for the
/// repository half of their answer.
pub fn dirty_set(
    changes: &[impl Borrow<Change>],
    packages: &mut PackageSet,
    m: &Matchers,
    consumers: &dyn Consumers,
) -> DirtySet {
    let mut out = DirtySet::default();
    if let Some(path) = apply_build_changes(changes, packages, m, &mut out) {
        return DirtySet::invalidated_by(path);
    }
    classify_changes(changes, packages, m, consumers, &mut out);
    route_traversed(changes, packages, &mut out);
    out
}

/// Pass one: applies the diff's main-repo BUILD file adds and deletes to
/// `packages`, so that ownership questions in pass two see the package
/// layout at head, and records deleted packages and edited BUILD files.
///
/// Returns the first changed path that matches an invalidator. The whole
/// answer is then global, so the rest of the diff is not read.
fn apply_build_changes<'c>(
    changes: &'c [impl Borrow<Change>],
    packages: &mut PackageSet,
    m: &Matchers,
    out: &mut DirtySet,
) -> Option<&'c str> {
    for c in changes {
        let c = c.borrow();
        if m.is_invalidator(&c.path) {
            return Some(&c.path);
        }
        if !packages.is_main_repo_build_file(&c.path) {
            continue;
        }
        let pkg = dirname(&c.path).to_string();
        match c.status {
            // The package layout at head. That a new `BUILD` file carves a
            // sub-package out of its parent, and a deleted one hands the files
            // back, is not stated here any more: the layer has
            // `PACKAGE:parent -> PACKAGE_LOOKUP:parent/sub` for exactly that
            // reason, and the reverse walk follows it. What is still needed is
            // *ownership*, because a changed file's source label depends on
            // which package owns it at head.
            Status::Added => packages.insert(pkg),
            Status::Deleted => {
                packages.remove(&pkg);
                out.deleted_packages.insert(pkg);
            }
            // A modified BUILD file is also a changed *source file*. That is
            // not a technicality: `filegroup(name = "srcs", srcs =
            // glob(["**"]))` (the shape every package in `bazel/bazel` uses to
            // feed the distfile) has the BUILD file itself among its inputs,
            // and its rule proto does not move when the BUILD file's text
            // does. Re-querying the package therefore finds nothing changed,
            // and without this the filegroup and everything above it
            // (`//src:srcs`, `//:bazel-srcs`, the distribution tarball) are
            // missed. The first run against a real subject caught it: both
            // bazel-diff and Tango reported
            // `//src/main/java/com/google/devtools/build/lib:srcs` and
            // `//src:srcs` on a BUILD-only commit and cambium did not.
            Status::Modified | Status::TypeChanged => {
                packages.insert(pkg);
                out.direct_files.insert(c.path.clone());
            }
        }
    }

    // A BUILD -> BUILD.bazel rename arrives as a delete plus an add of the same
    // package, which would otherwise leave the package marked deleted.
    out.deleted_packages
        .retain(|p| !packages.contains(p.as_str()));
    None
}

/// Pass two: classifies every path pass one did not take, against the
/// package layout at head, and records what each one means.
fn classify_changes(
    changes: &[impl Borrow<Change>],
    packages: &PackageSet,
    m: &Matchers,
    consumers: &dyn Consumers,
    out: &mut DirtySet,
) {
    for c in changes {
        let c = c.borrow();
        if packages.is_main_repo_build_file(&c.path) {
            continue;
        }
        match classify(&c.path, packages, m) {
            // Pass one returned on the first invalidator, with the same
            // matchers over the same paths, and took every main-repo BUILD
            // file.
            Classification::Invalidator | Classification::Build(_) => {
                unreachable!("handled in pass one")
            }
            // A `.bzl` is a Starlark input *and* an ordinary file in its
            // package. `filegroup(name = "srcs", srcs = glob(["**"]))` and the
            // hand-written `bzl_srcs` / `embedded_tools` lists `bazel/bazel`
            // keeps in `tools/build_defs/repo/BUILD` name the `.bzl` as a
            // source, so `//tools/build_defs/repo:utils.bzl` is a genuine input
            // of a dozen targets whose rule protos do not move when the file's
            // text does. That is exactly the case the modified-BUILD arm of
            // pass one exists for.
            //
            // The loading layer does not stand in for this and is not asked
            // to. It answers "which packages did this file's `BZL_LOAD` node
            // reach", and the packages that merely *consume* the file as a
            // source are a different, overlapping set; even for a package in
            // both, being dirty means "re-query me", which finds the
            // filegroup's rule byte-identical. So the file reaches its
            // consumers the way every other changed source does, on top of
            // whatever the layer and the external-repository stage make of it.
            Classification::Bzl(_) => {
                if packages.owner_of(&c.path).is_some() && is_content_edit(c.status) {
                    out.direct_files.insert(c.path.clone());
                }
            }
            Classification::Source(_) => {
                if is_content_edit(c.status) {
                    out.direct_files.insert(c.path.clone());
                }
            }
            // A path an ignore glob claims is not input. The claim holds only
            // where the graph agrees: `bazel/bazel` feeds its distribution
            // tarball from `glob(["**"])`, so `CHANGELOG.md` really is an
            // input of `//:changelog-file`.
            Classification::IgnoredSource(pkg) => {
                if is_content_edit(c.status) {
                    if consumers.is_consumed(&pkg, &c.path) {
                        out.direct_files.insert(c.path.clone());
                    } else {
                        out.ignored += 1;
                    }
                }
            }
            Classification::Ignored => out.ignored += 1,
            Classification::Nested(dir) => out.nested.entry(dir).or_default().push(c.path.clone()),
        }
    }
}

/// Pass three: records every path under a traversed boundary in
/// [`DirtySet::nested`] too.
///
/// A traversed boundary is two things at once, and both answers are owed.
/// xla's `third_party/tsl` is a main-repo directory whose packages Bazel
/// loaded (classified in pass two, as any other path is) *and* the
/// repository `@tsl`, whose targets consume those same files. Classification
/// is single-owner, so the repository half is recorded here rather than as a
/// classification: the paths go into `nested` beside the untraversed
/// boundaries' paths, and the engine routes them to the repository the
/// directory is, if it knows one.
fn route_traversed(changes: &[impl Borrow<Change>], packages: &PackageSet, out: &mut DirtySet) {
    for c in changes {
        let c = c.borrow();
        if packages.unwalked_boundary_of(&c.path).is_some() {
            continue;
        }
        for dir in packages.traversed_boundaries_of(&c.path) {
            out.nested
                .entry(dir.to_string())
                .or_default()
                .push(c.path.clone());
        }
    }
}

/// Whether a change edited a file's content in place, which is the only
/// change that marks the file's consumers direct.
///
/// An add or a delete moves no file row that exists at base; what it moves
/// is the package's own edges, and the loading layer is what says whether it
/// did.
fn is_content_edit(status: Status) -> bool {
    matches!(status, Status::Modified | Status::TypeChanged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rules shaped like a repository's configuration. The paths are among
    /// the defaults; the globs are ones a repository configures itself (no
    /// glob is on by default), `tools/toolchains/**` being the directory of
    /// `.bzl` files `is_bzl_file`'s doc says one can configure back.
    fn matchers() -> Matchers {
        Matchers::compile(&Rules {
            invalidator_paths: vec!["MODULE.bazel".into(), "WORKSPACE".into(), ".bazelrc".into()],
            invalidator_globs: vec!["tools/toolchains/**".into(), "third_party/**/*.bzl".into()],
            ignore_globs: vec!["**/*.md".into(), "docs/**".into()],
        })
        .unwrap()
    }

    fn packages() -> PackageSet {
        PackageSet::new(["", "lib", "lib/core", "app"].into_iter().map(String::from))
    }

    #[test]
    fn finds_the_longest_matching_package() {
        let p = packages();
        assert_eq!(p.owner_of("lib/core/sub/x.cc"), Some("lib/core"));
        assert_eq!(p.owner_of("lib/other/x.cc"), Some("lib"));
        assert_eq!(p.owner_of("README"), Some(""));
        let q = PackageSet::new(["lib".to_string()]);
        assert_eq!(q.owner_of("README"), None);
    }

    #[test]
    fn package_candidates_run_deepest_first_down_to_the_root() {
        assert_eq!(
            package_candidates("lib/core/a.cc").collect::<Vec<_>>(),
            vec!["lib/core", "lib", ""]
        );
        assert_eq!(package_candidates("README").collect::<Vec<_>>(), vec![""]);
    }

    #[test]
    fn classifies_paths() {
        let p = packages();
        let m = matchers();
        assert_eq!(
            classify("MODULE.bazel", &p, &m),
            Classification::Invalidator
        );
        assert_eq!(
            classify("tools/toolchains/cc.bzl", &p, &m),
            Classification::Invalidator
        );
        assert_eq!(
            classify("lib/core/BUILD.bazel", &p, &m),
            Classification::Build("lib/core".into())
        );
        assert_eq!(
            classify("lib/util.bzl", &p, &m),
            Classification::Bzl("lib/util.bzl".into())
        );
        assert_eq!(
            classify("lib/core/a.cc", &p, &m),
            Classification::Source("lib/core".into())
        );
        assert_eq!(
            classify("docs/x.txt", &p, &m),
            Classification::IgnoredSource("".into())
        );
        assert_eq!(
            classify("lib/core/x.md", &p, &m),
            Classification::IgnoredSource("lib/core".into())
        );
    }

    #[test]
    fn build_files_are_never_ignored() {
        // Ignore globs apply only to non-BUILD, non-.bzl paths.
        let p = packages();
        let m = ignoring(&["lib/**"]);
        assert_eq!(
            classify("lib/core/BUILD", &p, &m),
            Classification::Build("lib/core".into())
        );
        assert_eq!(
            classify("lib/core/x.bzl", &p, &m),
            Classification::Bzl("lib/core/x.bzl".into())
        );
        assert_eq!(
            classify("lib/core/x.cc", &p, &m),
            Classification::IgnoredSource("lib/core".into())
        );
    }

    /// A [`Consumers`] that never contradicts an ignore glob: the path-only
    /// reading, which is not the right answer for a real run.
    struct NoConsumers;

    impl Consumers for NoConsumers {
        fn is_consumed(&self, _pkg: &str, _path: &str) -> bool {
            false
        }
    }

    /// A [`Consumers`] over a hand-written list of consumed paths.
    struct Consumes(&'static [&'static str]);

    impl Consumers for Consumes {
        fn is_consumed(&self, _pkg: &str, path: &str) -> bool {
            self.0.contains(&path)
        }
    }

    /// Rules with nothing but these ignore globs.
    fn ignoring(globs: &[&str]) -> Matchers {
        Matchers::compile(&Rules {
            ignore_globs: globs.iter().map(|g| g.to_string()).collect(),
            ..Default::default()
        })
        .unwrap()
    }

    /// No rules at all: nothing invalidates and nothing is ignored.
    fn no_rules() -> Matchers {
        Matchers::compile(&Rules::default()).unwrap()
    }

    /// [`dirty_set`] over `changes`, with the package layout it leaves at
    /// head.
    fn run_with(
        mut packages: PackageSet,
        m: &Matchers,
        consumers: &dyn Consumers,
        changes: &[Change],
    ) -> (DirtySet, PackageSet) {
        let d = dirty_set(changes, &mut packages, m, consumers);
        (d, packages)
    }

    /// [`run_with`] over the shared packages and rules, for tests that read
    /// only the answer.
    fn run(changes: Vec<Change>) -> DirtySet {
        run_with(packages(), &matchers(), &NoConsumers, &changes).0
    }

    /// The contract `DirtySet::dirty`'s doc states: the classifier names no
    /// dirty package, whatever the diff. Which packages moved is the loading
    /// layer's answer.
    #[test]
    fn dirty_set_names_no_dirty_package() {
        let p = PackageSet::new(
            ["", "lib", "lib/core", "third_party/tsl/tsl/platform"].map(String::from),
        )
        .with_boundaries(["third_party/tsl".to_string(), "third_party/xla".to_string()])
        .with_traversed(["third_party/tsl".to_string()]);
        let (d, _) = run_with(
            p,
            &matchers(),
            &NoConsumers,
            &[
                Change::new(Status::Added, "lib/core/sub/BUILD"),
                Change::new(Status::Added, "lib/core/sub/x.cc"),
                Change::new(Status::Deleted, "lib/BUILD"),
                Change::new(Status::Modified, "lib/core/BUILD.bazel"),
                Change::new(Status::Modified, "lib/core/a.cc"),
                Change::new(Status::Modified, "lib/core/rules.bzl"),
                Change::new(Status::Modified, "docs/notes.md"),
                Change::new(Status::Modified, "third_party/xla/a.cc"),
                Change::new(Status::Modified, "third_party/tsl/tsl/platform/x.cc"),
            ],
        );
        assert!(!d.global);
        // Every other field heard from this diff, so the empty one is the
        // classifier's choice rather than a diff that reached nothing.
        assert!(
            !d.deleted_packages.is_empty()
                && !d.direct_files.is_empty()
                && !d.nested.is_empty()
                && d.ignored > 0,
            "{d:?}"
        );
        assert!(d.dirty.is_empty(), "{:?}", d.dirty);
    }

    #[test]
    fn modified_source_marks_the_file_direct() {
        let d = run(vec![Change::new(Status::Modified, "lib/core/a.cc")]);
        assert_eq!(d.direct_files.iter().next().unwrap(), "lib/core/a.cc");
    }

    /// An added file names no package here any more. Whether it entered a
    /// `glob` is a question about the directory listing Bazel read, and the
    /// loading layer is what holds that edge.
    #[test]
    fn an_added_source_names_no_package_and_no_file() {
        let d = run(vec![Change::new(Status::Added, "lib/core/new.cc")]);
        assert!(d.direct_files.is_empty());
    }

    #[test]
    fn a_modified_build_file_is_also_a_changed_source_file() {
        let d = run(vec![Change::new(Status::Modified, "lib/core/BUILD.bazel")]);
        assert!(
            d.direct_files.contains("lib/core/BUILD.bazel"),
            "a BUILD file is also a source file: a glob(['**']) filegroup consumes it"
        );
    }

    #[test]
    fn modified_bzl_is_also_a_changed_source_file() {
        // The file->target map gets it, because a filegroup can list the
        // `.bzl` among its `srcs` without its own rule proto moving. What the
        // file *loads* is the layer's answer, not this function's.
        let d = run(vec![Change::new(Status::Modified, "lib/core/rules.bzl")]);
        assert!(
            d.direct_files.contains("lib/core/rules.bzl"),
            "a .bzl consumed as a source must reach its consumers"
        );
    }

    #[test]
    fn an_added_bzl_names_no_package() {
        let d = run(vec![Change::new(Status::Added, "lib/core/new.bzl")]);
        assert!(d.direct_files.is_empty());
    }

    /// A `.bzl` above every BUILD file has no source label, so nothing here
    /// has anything to say about it at all.
    #[test]
    fn a_bzl_no_package_owns_is_left_to_the_layer() {
        let (d, _) = run_with(
            PackageSet::new(["lib".to_string()]),
            &matchers(),
            &NoConsumers,
            &[Change::new(Status::Modified, "vendor/x.bzl")],
        );
        assert!(d.direct_files.is_empty());
    }

    #[test]
    fn an_exempt_module_file_is_still_a_changed_source_file() {
        // `MODULE.bazel` is an invalidator by default; once the
        // external-repository stage claims it, it must fall through to the
        // ordinary source path so `filegroup(srcs = ["MODULE.bazel"])` and
        // `//:srcs` still see it.
        let mut m = matchers();
        m.exempt(["MODULE.bazel".to_string()]);
        let (d, _) = run_with(
            packages(),
            &m,
            &NoConsumers,
            &[Change::new(Status::Modified, "MODULE.bazel")],
        );
        assert!(!d.global);
        assert!(d.direct_files.contains("MODULE.bazel"));
    }

    /// A new `BUILD` file enters the package layout so later paths resolve to
    /// the right owner. It dirties nothing here: the layer's
    /// `PACKAGE:lib/core -> PACKAGE_LOOKUP:lib/core/sub` edge is what says the
    /// parent moved, and the engine walks it.
    #[test]
    fn a_new_package_enters_the_layout() {
        let (_, p) = run_with(
            packages(),
            &matchers(),
            &NoConsumers,
            &[
                Change::new(Status::Added, "lib/core/sub/BUILD.bazel"),
                Change::new(Status::Added, "lib/core/sub/x.cc"),
            ],
        );
        assert!(p.contains("lib/core/sub"));
        assert_eq!(p.owner_of("lib/core/sub/x.cc"), Some("lib/core/sub"));
    }

    /// A deleted `BUILD` file is still named here, because a package that
    /// moved by ceasing to exist has to be tombstoned rather than re-queried,
    /// and which package that is, is a fact about the path.
    #[test]
    fn a_deleted_build_file_names_the_package_it_deleted() {
        let d = run(vec![Change::new(Status::Deleted, "lib/core/BUILD.bazel")]);
        assert_eq!(
            d.deleted_packages.iter().cloned().collect::<Vec<_>>(),
            vec!["lib/core"]
        );
    }

    #[test]
    fn build_to_build_bazel_rename_keeps_the_package() {
        let d = run(vec![
            Change::new(Status::Deleted, "lib/core/BUILD"),
            Change::new(Status::Added, "lib/core/BUILD.bazel"),
        ]);
        assert!(d.deleted_packages.is_empty());
    }

    /// What the package layout is still for: once `lib/core` is not a package,
    /// `lib/core/a.cc` is `//lib`'s file, and that is the label its content
    /// change has to resolve to.
    #[test]
    fn files_under_a_deleted_package_move_to_the_parent() {
        let (d, p) = run_with(
            packages(),
            &matchers(),
            &NoConsumers,
            &[
                Change::new(Status::Deleted, "lib/core/BUILD.bazel"),
                Change::new(Status::Modified, "lib/core/a.cc"),
            ],
        );
        assert!(d.direct_files.contains("lib/core/a.cc"));
        assert_eq!(p.owner_of("lib/core/a.cc"), Some("lib"));
    }

    #[test]
    fn invalidators_win_over_everything() {
        let d = run(vec![
            Change::new(Status::Modified, "lib/core/a.cc"),
            Change::new(Status::Modified, "MODULE.bazel"),
        ]);
        assert!(d.global);
        assert_eq!(d.invalidator.as_deref(), Some("MODULE.bazel"));
    }

    /// A diff of any size is classified on its merits. The old
    /// `max_changed_files` cut-off went global here on file count alone,
    /// which fired on large no-op commits (a 10k-file licence-header sweep in
    /// `apollo`) whose dirty set was tiny. What a run cannot afford is
    /// re-querying too many *packages*, and the engine bounds that instead.
    #[test]
    fn a_large_diff_within_one_package_is_not_global() {
        let changes: Vec<Change> = (0..10_000)
            .map(|i| Change::new(Status::Modified, format!("lib/core/f{i}.cc")))
            .collect();
        let d = run(changes);
        assert!(!d.global);
        assert_eq!(d.direct_files.len(), 10_000);
    }

    #[test]
    fn unowned_paths_are_ignored() {
        let (d, _) = run_with(
            PackageSet::new(["lib".to_string()]),
            &matchers(),
            &NoConsumers,
            &[Change::new(Status::Modified, "README.md")],
        );
        assert!(!d.global);
        assert_eq!(d.ignored, 1);
    }

    /// The regression the benchmark found on `bazel/bazel`: the default ignore
    /// globs cover `**/*.md`, and `CHANGELOG.md` is a real input of
    /// `//:changelog-file`. Taking the glob at its word missed the genrule and
    /// everything above it; the graph has the final word.
    #[test]
    fn a_consumed_ignored_file_is_still_direct() {
        let (d, _) = run_with(
            PackageSet::new(["".to_string()]),
            &ignoring(&["**/*.md"]),
            &Consumes(&["CHANGELOG.md"]),
            &[Change::new(Status::Modified, "CHANGELOG.md")],
        );
        assert!(d.direct_files.contains("CHANGELOG.md"));
        assert_eq!(d.ignored, 0);
    }

    /// The case the ignore glob exists for: nothing consumes the file, so the
    /// commit does no Bazel work at all.
    #[test]
    fn an_unconsumed_ignored_file_stays_ignored() {
        let (d, _) = run_with(
            PackageSet::new(["".to_string()]),
            &ignoring(&["**/*.md"]),
            &NoConsumers,
            &[Change::new(Status::Modified, "CHANGELOG.md")],
        );
        assert!(d.direct_files.is_empty());
        assert_eq!(d.ignored, 1);
    }

    /// A file entering or leaving `glob(["**"])` moves the package's edges
    /// whatever the ignore glob says, and the base index cannot answer for a
    /// path that does not exist there yet.
    #[test]
    fn an_added_ignored_file_names_no_package() {
        let (d, _) = run_with(
            PackageSet::new(["docs".to_string()]),
            &ignoring(&["docs/**"]),
            &NoConsumers,
            &[Change::new(Status::Added, "docs/new.md")],
        );
        assert!(d.direct_files.is_empty());
    }
    /// The xla shape: a `REPO.bazel` directory Bazel 8.7 walked into anyway.
    fn traversed() -> PackageSet {
        PackageSet::new(
            ["", "lib", "third_party/tsl/tsl/platform"]
                .into_iter()
                .map(String::from),
        )
        .with_boundaries(["third_party/tsl".to_string()])
        .with_traversed(["third_party/tsl".to_string()])
    }

    #[test]
    fn a_traversed_boundary_keeps_main_repo_ownership() {
        let p = traversed();
        // Still a boundary, so the repository half can still be routed.
        assert_eq!(
            p.boundary_of("third_party/tsl/tsl/platform/dso_loader.cc"),
            Some("third_party/tsl")
        );
        // But not one `//...` stopped at, so the packages under it are this
        // repository's and own their files.
        assert_eq!(
            p.unwalked_boundary_of("third_party/tsl/tsl/platform/x.cc"),
            None
        );
        assert_eq!(
            p.owner_of("third_party/tsl/tsl/platform/dso_loader.cc"),
            Some("third_party/tsl/tsl/platform")
        );
        let m = matchers();
        assert_eq!(
            classify("third_party/tsl/tsl/platform/dso_loader.cc", &p, &m),
            Classification::Source("third_party/tsl/tsl/platform".to_string())
        );
    }

    #[test]
    fn a_change_under_a_traversed_boundary_is_also_the_repositorys() {
        let (d, _) = run_with(
            traversed(),
            &no_rules(),
            &NoConsumers,
            &[Change::new(
                Status::Modified,
                "third_party/tsl/tsl/platform/dso_loader.cc",
            )],
        );
        // The main-repo half.
        assert!(d
            .direct_files
            .contains("third_party/tsl/tsl/platform/dso_loader.cc"));
        // And the repository half, for the engine to route.
        assert_eq!(
            d.nested.get("third_party/tsl").map(Vec::len),
            Some(1),
            "{:?}",
            d.nested
        );
    }

    /// A repository vendored inside a traversed one: `third_party/tsl` is
    /// walked into, and the `WORKSPACE` directory under it is not.
    fn nested_in_traversed() -> PackageSet {
        PackageSet::new(
            ["", "lib", "third_party/tsl/tsl/platform"]
                .into_iter()
                .map(String::from),
        )
        .with_boundaries([
            "third_party/tsl".to_string(),
            "third_party/tsl/third_party/gemm".to_string(),
        ])
        .with_traversed(["third_party/tsl".to_string()])
    }

    #[test]
    fn a_repository_inside_a_traversed_boundary_is_still_nested() {
        let p = nested_in_traversed();
        let path = "third_party/tsl/third_party/gemm/lib/matmul.cc";
        assert_eq!(
            p.unwalked_boundary_of(path),
            Some("third_party/tsl/third_party/gemm")
        );
        // So no main-repo package owns it and none can be minted from it.
        assert_eq!(p.owner_of(path), None);
        assert_eq!(
            classify(path, &p, &matchers()),
            Classification::Nested("third_party/tsl/third_party/gemm".to_string())
        );
        let mut q = nested_in_traversed();
        q.insert("third_party/tsl/third_party/gemm/lib".to_string());
        assert!(!q.contains("third_party/tsl/third_party/gemm/lib"));
        // The traversed half is still owed for paths outside the inner one.
        assert_eq!(
            p.traversed_boundaries_of("third_party/tsl/tsl/platform/x.cc")
                .collect::<Vec<_>>(),
            vec!["third_party/tsl"]
        );
    }

    #[test]
    fn the_inner_repository_is_routed_not_the_outer_one() {
        let (d, _) = run_with(
            nested_in_traversed(),
            &no_rules(),
            &NoConsumers,
            &[Change::new(
                Status::Modified,
                "third_party/tsl/third_party/gemm/lib/matmul.cc",
            )],
        );
        assert_eq!(
            d.nested.keys().collect::<Vec<_>>(),
            vec!["third_party/tsl/third_party/gemm"]
        );
        assert!(d.direct_files.is_empty());
    }

    fn nested() -> PackageSet {
        PackageSet::new(["", "lib", "third_party"].into_iter().map(String::from))
            .with_boundaries(["third_party/xla".to_string()])
    }

    #[test]
    fn a_nested_repository_owns_its_own_paths() {
        let p = nested();
        assert_eq!(
            p.boundary_of("third_party/xla/BUILD"),
            Some("third_party/xla")
        );
        assert_eq!(p.boundary_of("third_party/xla"), Some("third_party/xla"));
        assert_eq!(p.boundary_of("third_party/xla_other/a.cc"), None);
        // No main-repo package owns anything in there, not even the parent's.
        assert_eq!(p.owner_of("third_party/xla/service/a.cc"), None);
        assert_eq!(p.owner_of("third_party/other/a.cc"), Some("third_party"));
    }

    /// The root is the main repository itself, never a boundary inside it,
    /// however it is spelled.
    #[test]
    fn the_root_is_never_a_boundary() {
        let p = PackageSet::new(["".to_string()])
            .with_boundaries(["".to_string(), "/".to_string()])
            .with_traversed(["/".to_string()]);
        assert_eq!(p.boundary_of(""), None);
        assert_eq!(p.boundary_of("a.cc"), None);
        assert_eq!(p.owner_of("a.cc"), Some(""));
    }

    #[test]
    fn classifies_paths_inside_a_nested_repository() {
        let p = nested();
        let m = matchers();
        for path in [
            "third_party/xla/service/BUILD",
            "third_party/xla/service/a.cc",
            "third_party/xla/service/notes.md",
        ] {
            assert_eq!(
                classify(path, &p, &m),
                Classification::Nested("third_party/xla".into()),
                "{path}"
            );
        }
        // An invalidator still wins, as it does over every other reading: the
        // configured globs are a claim about this repository's build that
        // nothing below can take back. `matchers` makes any
        // `third_party/**/*.bzl` one.
        assert_eq!(
            classify("third_party/xla/xla.bzl", &p, &m),
            Classification::Invalidator
        );
    }

    #[test]
    fn a_nested_build_file_mints_no_package() {
        let (d, p) = run_with(
            nested(),
            &no_rules(),
            &NoConsumers,
            &[
                Change::new(Status::Added, "third_party/xla/service/BUILD"),
                Change::new(Status::Modified, "third_party/xla/BUILD"),
                Change::new(Status::Deleted, "third_party/xla/old/BUILD"),
                Change::new(Status::Modified, "third_party/xla/service/a.cc"),
            ],
        );
        assert!(d.deleted_packages.is_empty());
        assert!(d.direct_files.is_empty());
        assert!(!p.contains("third_party/xla/service"));
        assert_eq!(
            d.nested.get("third_party/xla").map(Vec::len),
            Some(4),
            "{:?}",
            d.nested
        );
    }

    #[test]
    fn the_boundary_set_comes_off_the_index() {
        let mut b = cambium_index::IndexBuilder::new(cambium_index::new_lineage());
        b.set_repo_boundaries(vec![
            cambium_index::Boundary {
                dir: "third_party/xla".into(),
                bazelignored: false,
                traversed: false,
            },
            cambium_index::Boundary {
                dir: "third_party/tsl".into(),
                bazelignored: false,
                traversed: true,
            },
        ]);
        let package = |name: &str| cambium_index::PackageUpdate {
            package: name.into(),
            build_hash: [1; 32],
            bzl_hash: [0; 32],
            targets: vec![cambium_index::TargetRow::rule(
                format!("//{name}:a"),
                "cc_library",
                vec![],
            )],
        };
        b.apply_packages(&[package("lib"), package("third_party/tsl/tsl")]);
        let d = b.finish("head");
        let p = PackageSet::from_index(d.view());
        assert!(p.contains("lib"));
        assert_eq!(
            p.boundary_of("third_party/xla/a.cc"),
            Some("third_party/xla")
        );
        assert_eq!(
            p.unwalked_boundary_of("third_party/xla/a.cc"),
            Some("third_party/xla")
        );
        // The traversed flag comes off the index too: the boundary is still
        // one, and the main repository still owns the packages under it.
        let tsl = "third_party/tsl/tsl/a.cc";
        assert_eq!(p.boundary_of(tsl), Some("third_party/tsl"));
        assert_eq!(p.unwalked_boundary_of(tsl), None);
        assert_eq!(p.owner_of(tsl), Some("third_party/tsl/tsl"));
    }

    #[test]
    fn a_bad_glob_names_the_pattern_it_came_from() {
        for rules in [
            Rules {
                invalidator_globs: vec!["tools/**".into(), "a[".into()],
                ..Default::default()
            },
            Rules {
                ignore_globs: vec!["docs/**".into(), "a[".into()],
                ..Default::default()
            },
        ] {
            let Err(ClassifyError::BadGlob { pattern, .. }) = Matchers::compile(&rules) else {
                panic!("`a[` compiled: {rules:?}");
            };
            assert_eq!(pattern, "a[");
        }
    }

    /// An exemption is the caller's claim to have handled a file, so it holds
    /// against a configured glob as it does against an exact path.
    #[test]
    fn an_exemption_beats_a_glob_invalidator() {
        let mut m = matchers();
        assert!(m.is_invalidator("tools/toolchains/cc.bzl"));
        m.exempt(["tools/toolchains/cc.bzl".to_string()]);
        assert!(!m.is_invalidator("tools/toolchains/cc.bzl"));
        assert!(m.is_invalidator("tools/toolchains/other.bzl"));
    }

    /// Under a traversed boundary a BUILD file is this repository's, so it
    /// moves the package layout like any other, and the repository half of
    /// the answer is owed for it as well.
    #[test]
    fn a_build_file_under_a_traversed_boundary_moves_the_layout() {
        let (d, p) = run_with(
            traversed(),
            &no_rules(),
            &NoConsumers,
            &[
                Change::new(Status::Added, "third_party/tsl/tsl/newpkg/BUILD"),
                Change::new(Status::Deleted, "third_party/tsl/tsl/platform/BUILD"),
            ],
        );
        assert!(p.contains("third_party/tsl/tsl/newpkg"));
        assert!(!p.contains("third_party/tsl/tsl/platform"));
        assert_eq!(
            d.deleted_packages.iter().collect::<Vec<_>>(),
            vec!["third_party/tsl/tsl/platform"]
        );
        assert_eq!(
            d.nested.get("third_party/tsl").map(Vec::len),
            Some(2),
            "{:?}",
            d.nested
        );
    }

    /// The engine names a package its loading layer has no node for by asking
    /// [`PackageSet::is_main_repo_build_file`], and pass one of [`dirty_set`]
    /// moves the layout by the same question. This pins that the two agree
    /// path by path, over every kind of boundary: none, one `//...` walked
    /// into, one it stopped at, and one it stopped at inside one it walked
    /// into. `boundary_of` is the question that does not agree, and asking
    /// it is how the engine once dropped xla's new `third_party/tsl`
    /// packages.
    #[test]
    fn the_engine_and_the_layout_agree_on_which_build_files_are_this_repositorys() {
        let layout = || {
            PackageSet::new(["", "lib"].into_iter().map(String::from))
                .with_boundaries([
                    "third_party/tsl".to_string(),
                    "third_party/tsl/third_party/gemm".to_string(),
                    "third_party/xla".to_string(),
                ])
                .with_traversed(["third_party/tsl".to_string()])
        };
        let cases = [
            ("lib/sub/BUILD", true),
            ("third_party/tsl/tsl/newpkg/BUILD", true),
            ("third_party/tsl/tsl/newpkg/BUILD.bazel", true),
            ("third_party/tsl/third_party/gemm/lib/BUILD", false),
            ("third_party/xla/service/BUILD", false),
            ("third_party/tsl/tsl/newpkg/x.cc", false),
        ];
        let p = layout();
        for (path, main) in cases {
            assert_eq!(p.is_main_repo_build_file(path), main, "{path}");
            let (_, head) = run_with(
                layout(),
                &no_rules(),
                &NoConsumers,
                &[Change::new(Status::Added, path)],
            );
            assert_eq!(head.contains(dirname(path)), main, "{path}");
        }
        // The traversed case is the one the two questions split on.
        assert_eq!(
            p.boundary_of("third_party/tsl/tsl/newpkg/BUILD"),
            Some("third_party/tsl")
        );
    }
}
