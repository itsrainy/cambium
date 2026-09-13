//! Path classification and the dirty set (spec 6.1, 6.2).
//!
//! This is where a list of changed paths becomes "these packages must be
//! re-queried, these files mark their consumers direct, or everything is
//! affected". It is pure: no Bazel, no git, no filesystem. That makes it cheap
//! to test exhaustively, which matters because a mistake here is an
//! under-approximation — the one class of bug the design does not tolerate.
//!
//! One thing it is not pure about, and cannot be: which directories of the
//! tree are Bazel repositories of their own. `bazel query //...` stops at a
//! subdirectory carrying a WORKSPACE or a MODULE.bazel, so the index has no
//! package under one, and a path there is not this repository's to classify.
//! The index records those directories at build time and
//! [`PackageSet::with_boundaries`] carries them in, which is what stops a
//! changed `third_party/xla/service/BUILD` from minting a package name Bazel
//! will never load. Such a path becomes [`Classification::Nested`] and lands
//! in [`DirtySet::nested`]; what to do about the repository it names is the
//! engine's question.

use std::borrow::Borrow;
use std::collections::BTreeSet;

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
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
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

    /// A root-level `BUILD` file is an invalidator by default (it can define
    /// package-wide settings), and so is anything matching a configured glob.
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

/// The set of packages that exist, with the diff's BUILD file additions and
/// deletions overlaid (spec 6.1).
pub struct PackageSet {
    packages: FxHashSet<String>,
    /// Directories that are Bazel repositories of their own, each with a
    /// trailing `/`. See [`PackageSet::boundary_of`].
    boundaries: Vec<String>,
}

impl PackageSet {
    pub fn new(packages: impl IntoIterator<Item = String>) -> Self {
        Self {
            packages: packages.into_iter().collect(),
            boundaries: Vec::new(),
        }
    }

    /// The directories below the root that are Bazel repositories of their
    /// own, as `cambium_index::repo_boundaries` found them: repo-relative and
    /// without a trailing slash.
    pub fn with_boundaries(mut self, dirs: impl IntoIterator<Item = String>) -> Self {
        let mut v: Vec<String> = dirs
            .into_iter()
            .filter(|d| !d.is_empty())
            .map(|d| format!("{}/", d.trim_end_matches('/')))
            .collect();
        v.sort();
        v.dedup();
        self.boundaries = v;
        self
    }

    /// The nested repository `path` sits in, if any: the *outermost* one,
    /// which is where `//...` stopped, returned without its trailing slash.
    ///
    /// A path under a boundary belongs to some other repository's build, not
    /// to this one. The main repository has no package there, can never gain
    /// one from a changed path, and owns none of the files: tensorflow's
    /// `third_party/xla` is the repository `@xla`, and reading
    /// `third_party/xla/service/BUILD` as a main-repo BUILD file mints four
    /// hundred packages Bazel will not load.
    pub fn boundary_of(&self, path: &str) -> Option<&str> {
        self.boundaries
            .iter()
            .find(|b| path.starts_with(b.as_str()) || path == &b[..b.len() - 1])
            .map(|b| &b[..b.len() - 1])
    }

    /// Live packages recorded in an index.
    pub fn from_index(index: IndexRef<'_>) -> Self {
        let mut packages = FxHashSet::default();
        for p in 0..index.num_packages() {
            if !index.package_tombstoned(p) {
                packages.insert(index.package_name(p).to_string());
            }
        }
        Self {
            packages,
            boundaries: Vec::new(),
        }
        .with_boundaries(index.repo_boundaries().map(str::to_string))
    }

    pub fn contains(&self, pkg: &str) -> bool {
        self.packages.contains(pkg)
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Records a package, unless the path is inside a nested repository: a
    /// directory `//...` never descended into has no main-repo package, and
    /// minting one from a changed path is what made the phantoms.
    pub fn insert(&mut self, pkg: String) {
        if self.boundary_of(&pkg).is_some() {
            return;
        }
        self.packages.insert(pkg);
    }

    pub fn remove(&mut self, pkg: &str) {
        self.packages.remove(pkg);
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.packages.iter().map(|s| s.as_str())
    }

    /// Longest-prefix owner of `path`: walk the path's ancestors and return the
    /// first that is a package. O(depth) hash lookups — no binary search per
    /// component.
    pub fn owner_of(&self, path: &str) -> Option<&str> {
        if self.boundary_of(path).is_some() {
            return None;
        }
        let mut end = path.rfind('/');
        loop {
            // `None` is the root package, which is the last candidate rather
            // than the end of the walk.
            let dir = end.map_or("", |i| &path[..i]);
            if let Some(p) = self.packages.get(dir) {
                return Some(p.as_str());
            }
            let i = end?;
            end = path[..i].rfind('/');
        }
    }
}

/// True if the path is a package definition file.
pub fn is_build_file(path: &str) -> bool {
    matches!(basename(path), "BUILD" | "BUILD.bazel")
}

/// True if the path is a Starlark file whose contents can change rule
/// definitions. `.bzl` only: `.bazelrc` and friends are invalidators.
///
/// This is about *shape*, not about blast radius, and it is not the same
/// question the invalidator globs answer. A `.bzl` file is handled precisely:
/// the index's static load closure says which packages load it, and only those
/// are dirtied. The default globs then name a few directories —
/// `tools/toolchains/**`, `platforms/**` — where a `.bzl` edit is treated as
/// global anyway, because the files there define toolchains whose reach the
/// load closure does not model: a toolchain swap changes what every action
/// in the repository runs without any BUILD file loading the `.bzl` that did
/// it. So a path can be both, and the invalidator wins; that is a deliberate
/// over-approximation over a small, hand-named set of directories, not a
/// statement about `.bzl` files in general.
pub fn is_bzl_file(path: &str) -> bool {
    path.ends_with(".bzl")
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
    /// A `.bzl` file. Which packages it reaches is answered from the index's
    /// static load closure (`cambium_index::loads`), not from a path.
    Bzl(String),
    /// A file owned by the named package.
    Source(String),
    /// A file owned by the named package that also matches an ignore glob.
    /// Whether it really is out of the graph is a question for the index:
    /// see [`Consumed`].
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
    // Invalidators win over everything, including ignores (spec 9).
    if m.is_invalidator(path) {
        return Classification::Invalidator;
    }
    // Before anything reads the path as a package: a nested repository's files
    // are not this repository's, whatever they are called.
    if let Some(dir) = packages.boundary_of(path) {
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
/// An ignore glob is a claim about paths the graph does not name — "`**/*.md`
/// is documentation, not input". On a real repository that claim is often
/// wrong: `bazel/bazel` feeds its distribution tarball from
/// `filegroup(name = "srcs", srcs = glob(["**"]))` in every package, so
/// `CHANGELOG.md` is a genuine input of `//:changelog-file` and
/// `site/en/**/*.md` of `//site/en:srcs`. Taking the glob at its word there is
/// an under-approximation — the one class of bug this design does not tolerate
/// — so [`dirty_set`] asks the index instead and lets the glob decide only the
/// cases the graph does not contradict.
pub trait Consumed {
    /// Some rule consumes `path` (owned by `pkg`) as a source input.
    fn is_consumed(&self, pkg: &str, path: &str) -> bool;
}

/// A [`Consumed`] that never contradicts an ignore glob. For callers with no
/// index in hand; it restores the old path-only behaviour, so it is *not* the
/// right answer for a real run.
pub struct NoConsumers;

impl Consumed for NoConsumers {
    fn is_consumed(&self, _pkg: &str, _path: &str) -> bool {
        false
    }
}

/// The [`Consumed`] a real run uses: the base index's reverse row for the
/// path's source label.
pub struct IndexConsumers<'a>(pub IndexRef<'a>);

impl Consumed for IndexConsumers<'_> {
    fn is_consumed(&self, pkg: &str, path: &str) -> bool {
        let label = label::source_label(pkg, path);
        match self.0.id_of(&label) {
            Some(id) => !self.0.file_consumers(id).is_empty(),
            None => false,
        }
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
    pub dirty: BTreeSet<String>,
    /// Packages that no longer have a BUILD file.
    pub deleted_packages: BTreeSet<String>,
    /// Paths whose content changed but whose package structure did not.
    pub direct_files: BTreeSet<String>,
    /// `.bzl` paths that changed, to be resolved against the index's static
    /// load closure.
    pub changed_bzl: BTreeSet<String>,
    /// Paths that were ignored, for `--explain`.
    pub ignored: usize,
    /// Changed paths inside a nested Bazel repository, by the boundary
    /// directory they sit under. Nothing here dirties a main-repo package,
    /// because there is none; the engine routes each directory to the
    /// external repository it is, or gives up and says so.
    pub nested: std::collections::BTreeMap<String, Vec<String>>,
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

/// Applies spec 6.2 to a diff.
///
/// `packages` must be the package set from the base index; this function
/// overlays the diff's BUILD file additions and deletions itself, because a
/// path's owner depends on which BUILD files exist at `head`.
pub fn dirty_set(
    changes: &[impl Borrow<Change>],
    packages: &mut PackageSet,
    m: &Matchers,
    consumed: &dyn Consumed,
) -> DirtySet {
    // Pass one: apply BUILD file adds and deletes so ownership questions in
    // pass two see the package layout at head.
    let mut out = DirtySet::default();
    for c in changes {
        let c = c.borrow();
        if m.is_invalidator(&c.path) {
            return DirtySet::invalidated_by(&c.path);
        }
        if !is_build_file(&c.path) {
            continue;
        }
        // A BUILD file inside a nested repository defines a package of *that*
        // repository. Pass two records it; pass one must not mint a main-repo
        // package name from it, nor touch the parent package, which is in a
        // different repository and cannot gain or lose files to it.
        if packages.boundary_of(&c.path).is_some() {
            continue;
        }
        let pkg = dirname(&c.path).to_string();
        match c.status {
            Status::Added => {
                // A new package steals files from its parent, so both change.
                if !packages.contains(&pkg) {
                    if let Some(parent) = parent_package(&pkg, packages) {
                        out.dirty.insert(parent);
                    }
                }
                packages.insert(pkg.clone());
                out.dirty.insert(pkg);
            }
            Status::Deleted => {
                // Its files fall back to the parent package, so both change.
                packages.remove(&pkg);
                if let Some(parent) = parent_package(&pkg, packages) {
                    out.dirty.insert(parent);
                }
                out.deleted_packages.insert(pkg.clone());
                out.dirty.insert(pkg);
            }
            // A modified BUILD file dirties only its own package: the package
            // boundaries did not move.
            //
            // It is also a changed *source file*. That is not a technicality:
            // `filegroup(name = "srcs", srcs = glob(["**"]))` — the shape every
            // package in `bazel/bazel` uses to feed the distfile — has the
            // BUILD file itself among its inputs, and its rule proto does not
            // move when the BUILD file's text does. Re-querying the package
            // therefore finds nothing changed, and without this the filegroup
            // and everything above it (`//src:srcs`, `//:bazel-srcs`, the
            // distribution tarball) are missed. The first run against a real subject
            // bazel/bazel caught it: both bazel-diff and Tango reported
            // `//src/main/java/com/google/devtools/build/lib:srcs` and
            // `//src:srcs` on a BUILD-only commit and cambium did not.
            Status::Modified | Status::TypeChanged => {
                packages.insert(pkg.clone());
                out.dirty.insert(pkg.clone());
                out.direct_files.insert(c.path.clone());
            }
        }
    }

    // A BUILD -> BUILD.bazel rename arrives as a delete plus an add of the same
    // package, which would otherwise leave the package marked deleted.
    out.deleted_packages
        .retain(|p| !packages.contains(p.as_str()));

    // Pass two: everything else.
    for c in changes {
        let c = c.borrow();
        if is_build_file(&c.path) && packages.boundary_of(&c.path).is_none() {
            continue;
        }
        match classify(&c.path, packages, m) {
            Classification::Invalidator => return DirtySet::invalidated_by(&c.path),
            Classification::Build(_) => unreachable!("handled in pass one"),
            Classification::Bzl(p) => {
                out.changed_bzl.insert(p);
                // A `.bzl` is a load-closure input *and* an ordinary file in
                // its package. `filegroup(name = "srcs", srcs = glob(["**"]))`
                // and the hand-written `bzl_srcs` / `embedded_tools` lists
                // `bazel/bazel` keeps in `tools/build_defs/repo/BUILD` name the
                // `.bzl` as a source, so `//tools/build_defs/repo:utils.bzl` is
                // a genuine input of a dozen targets whose rule protos do not
                // move when the file's text does — exactly the case the
                // modified-BUILD arm above exists for.
                //
                // The load closure cannot stand in for this. It answers "which
                // packages *load* this file", and the packages that merely
                // *consume* it are a different, overlapping set; and even for a
                // package in both, being dirty only means "re-query me", which
                // finds the filegroup's rule byte-identical. So the file has to
                // reach its consumers the way every other changed source does.
                //
                // This is on top of, never instead of, the closure walk and
                // whatever the external-repository stage makes of the same
                // file: a `.bzl` in the `MODULE.bazel` workspace closure gets
                // both answers.
                //
                // A `.bzl` no package owns — one above every BUILD file — has
                // no source label, so the load closure is all there is.
                if let Some(pkg) = packages.owner_of(&c.path) {
                    match c.status {
                        Status::Modified | Status::TypeChanged => {
                            out.direct_files.insert(c.path.clone());
                        }
                        // An added or deleted `.bzl` can enter or leave a
                        // `glob`, so the package's edges may have moved.
                        Status::Added | Status::Deleted => {
                            out.dirty.insert(pkg.to_string());
                        }
                    }
                }
            }
            Classification::Source(pkg) => match c.status {
                // Content-only edits cannot change the graph, only mark the
                // file's consumers direct (spec section 2).
                Status::Modified | Status::TypeChanged => {
                    out.direct_files.insert(c.path.clone());
                }
                // An added or removed file may enter or leave a glob, so the
                // package's edges may have changed.
                Status::Added | Status::Deleted => {
                    out.dirty.insert(pkg);
                }
            },
            // A path an ignore glob claims is not input. The claim holds
            // only where the graph agrees.
            Classification::IgnoredSource(pkg) => match c.status {
                Status::Modified | Status::TypeChanged => {
                    if consumed.is_consumed(&pkg, &c.path) {
                        out.direct_files.insert(c.path.clone());
                    } else {
                        out.ignored += 1;
                    }
                }
                // The index cannot answer for a file that does not exist at
                // base, and a file entering or leaving a `glob(["**"])`
                // changes the package's edges either way, so an add or a
                // delete re-queries the package however the glob reads. That
                // costs one package query on a docs-only commit that adds a
                // file; the alternative is missing the filegroup.
                Status::Added | Status::Deleted => {
                    out.dirty.insert(pkg);
                }
            },
            Classification::Ignored => out.ignored += 1,
            Classification::Nested(dir) => out.nested.entry(dir).or_default().push(c.path.clone()),
        }
    }

    out
}

/// The package that owns `pkg`'s directory once `pkg` itself is not a package.
fn parent_package(pkg: &str, packages: &PackageSet) -> Option<String> {
    let mut cur = label::parent_package(pkg)?;
    loop {
        if packages.contains(cur) {
            return Some(cur.to_string());
        }
        cur = label::parent_package(cur)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matchers() -> Matchers {
        Matchers::compile(&Rules {
            invalidator_paths: vec![
                "MODULE.bazel".into(),
                "WORKSPACE".into(),
                ".bazelrc".into(),
                "BUILD".into(),
            ],
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
        // Ignore globs apply only to non-BUILD, non-.bzl paths (spec 9).
        let p = packages();
        let m = Matchers::compile(&Rules {
            ignore_globs: vec!["lib/**".into()],
            ..Default::default()
        })
        .unwrap();
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

    /// A [`Consumed`] over a hand-written list of consumed paths.
    struct Consumes(&'static [&'static str]);

    impl Consumed for Consumes {
        fn is_consumed(&self, _pkg: &str, path: &str) -> bool {
            self.0.contains(&path)
        }
    }

    fn run(changes: Vec<Change>) -> DirtySet {
        let mut p = packages();
        dirty_set(&changes, &mut p, &matchers(), &NoConsumers)
    }

    #[test]
    fn modified_source_marks_the_file_direct() {
        let d = run(vec![Change::new(Status::Modified, "lib/core/a.cc")]);
        assert!(d.dirty.is_empty());
        assert_eq!(d.direct_files.iter().next().unwrap(), "lib/core/a.cc");
    }

    #[test]
    fn added_source_dirties_the_package() {
        let d = run(vec![Change::new(Status::Added, "lib/core/new.cc")]);
        assert_eq!(d.dirty.iter().next().unwrap(), "lib/core");
        assert!(d.direct_files.is_empty());
    }

    #[test]
    fn modified_build_dirties_only_its_package() {
        let d = run(vec![Change::new(Status::Modified, "lib/core/BUILD.bazel")]);
        assert_eq!(
            d.dirty.iter().cloned().collect::<Vec<_>>(),
            vec!["lib/core"]
        );
        assert!(
            d.direct_files.contains("lib/core/BUILD.bazel"),
            "a BUILD file is also a source file: a glob(['**']) filegroup consumes it"
        );
    }

    #[test]
    fn modified_bzl_is_also_a_changed_source_file() {
        // The load closure gets it (`changed_bzl`) *and* the file->target map
        // gets it (`direct_files`), because a filegroup can list the `.bzl`
        // among its `srcs` without its own rule proto moving.
        let d = run(vec![Change::new(Status::Modified, "lib/core/rules.bzl")]);
        assert!(d.changed_bzl.contains("lib/core/rules.bzl"));
        assert!(
            d.direct_files.contains("lib/core/rules.bzl"),
            "a .bzl consumed as a source must reach its consumers"
        );
    }

    #[test]
    fn added_bzl_dirties_its_package() {
        let d = run(vec![Change::new(Status::Added, "lib/core/new.bzl")]);
        assert!(d.changed_bzl.contains("lib/core/new.bzl"));
        assert!(d.dirty.contains("lib/core"), "it may have entered a glob");
        assert!(d.direct_files.is_empty());
    }

    #[test]
    fn a_bzl_no_package_owns_is_left_to_the_load_closure() {
        let mut p = PackageSet::new(["lib".to_string()]);
        let d = dirty_set(
            &[Change::new(Status::Modified, "vendor/x.bzl")],
            &mut p,
            &matchers(),
            &NoConsumers,
        );
        assert!(d.changed_bzl.contains("vendor/x.bzl"));
        assert!(d.direct_files.is_empty());
        assert!(d.dirty.is_empty());
    }

    #[test]
    fn an_exempt_module_file_is_still_a_changed_source_file() {
        // `MODULE.bazel` is an invalidator by default; once the
        // external-repository stage claims it, it must fall through to the
        // ordinary source path so `filegroup(srcs = ["MODULE.bazel"])` and
        // `//:srcs` still see it.
        let mut m = matchers();
        m.exempt(["MODULE.bazel".to_string()]);
        let mut p = packages();
        let d = dirty_set(
            &[Change::new(Status::Modified, "MODULE.bazel")],
            &mut p,
            &m,
            &NoConsumers,
        );
        assert!(!d.global);
        assert!(d.direct_files.contains("MODULE.bazel"));
    }

    #[test]
    fn new_package_dirties_parent_and_itself() {
        let d = run(vec![
            Change::new(Status::Added, "lib/core/sub/BUILD.bazel"),
            Change::new(Status::Added, "lib/core/sub/x.cc"),
        ]);
        assert_eq!(
            d.dirty.iter().cloned().collect::<Vec<_>>(),
            vec!["lib/core", "lib/core/sub"]
        );
    }

    #[test]
    fn deleted_package_dirties_parent_and_itself() {
        let d = run(vec![Change::new(Status::Deleted, "lib/core/BUILD.bazel")]);
        assert_eq!(
            d.dirty.iter().cloned().collect::<Vec<_>>(),
            vec!["lib", "lib/core"]
        );
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
        assert!(d.dirty.contains("lib/core"));
    }

    #[test]
    fn file_moving_into_a_new_package_dirties_both() {
        // A rename arrives as delete + add.
        let d = run(vec![
            Change::new(Status::Deleted, "lib/core/x.cc"),
            Change::new(Status::Added, "app/x.cc"),
        ]);
        assert_eq!(
            d.dirty.iter().cloned().collect::<Vec<_>>(),
            vec!["app", "lib/core"]
        );
    }

    #[test]
    fn files_under_a_deleted_package_move_to_the_parent() {
        let d = run(vec![
            Change::new(Status::Deleted, "lib/core/BUILD.bazel"),
            Change::new(Status::Modified, "lib/core/a.cc"),
        ]);
        // a.cc now belongs to //lib, and only its content changed.
        assert!(d.direct_files.contains("lib/core/a.cc"));
        assert!(d.dirty.contains("lib"));
    }

    #[test]
    fn bzl_changes_are_collected_for_the_load_closure() {
        let d = run(vec![Change::new(Status::Modified, "lib/util.bzl")]);
        assert!(d.dirty.is_empty());
        assert_eq!(d.changed_bzl.iter().next().unwrap(), "lib/util.bzl");
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
        let mut p = packages();
        let d = dirty_set(&changes, &mut p, &matchers(), &NoConsumers);
        assert!(!d.global);
        assert_eq!(d.dirty.len(), 0, "sources do not re-query their package");
        assert_eq!(d.direct_files.len(), 10_000);
    }

    #[test]
    fn unowned_paths_are_ignored() {
        let mut p = PackageSet::new(["lib".to_string()]);
        let d = dirty_set(
            &[Change::new(Status::Modified, "README.md")],
            &mut p,
            &matchers(),
            &NoConsumers,
        );
        assert!(!d.global);
        assert!(d.dirty.is_empty());
        assert_eq!(d.ignored, 1);
    }

    /// The regression the benchmark found on `bazel/bazel`: the default ignore
    /// globs cover `**/*.md`, and `CHANGELOG.md` is a real input of
    /// `//:changelog-file`. Taking the glob at its word missed the genrule and
    /// everything above it; the graph has the final word.
    #[test]
    fn a_consumed_ignored_file_is_still_direct() {
        let m = Matchers::compile(&Rules {
            ignore_globs: vec!["**/*.md".into()],
            ..Default::default()
        })
        .unwrap();
        let mut p = PackageSet::new(["".to_string()]);
        let d = dirty_set(
            &[Change::new(Status::Modified, "CHANGELOG.md")],
            &mut p,
            &m,
            &Consumes(&["CHANGELOG.md"]),
        );
        assert!(d.direct_files.contains("CHANGELOG.md"));
        assert_eq!(d.ignored, 0);
        assert!(d.dirty.is_empty());
    }

    /// The case the ignore glob exists for: nothing consumes the file, so the
    /// commit does no Bazel work at all.
    #[test]
    fn an_unconsumed_ignored_file_stays_ignored() {
        let m = Matchers::compile(&Rules {
            ignore_globs: vec!["**/*.md".into()],
            ..Default::default()
        })
        .unwrap();
        let mut p = PackageSet::new(["".to_string()]);
        let d = dirty_set(
            &[Change::new(Status::Modified, "CHANGELOG.md")],
            &mut p,
            &m,
            &NoConsumers,
        );
        assert!(d.direct_files.is_empty());
        assert!(d.dirty.is_empty());
        assert_eq!(d.ignored, 1);
    }

    /// A file entering or leaving `glob(["**"])` moves the package's edges
    /// whatever the ignore glob says, and the base index cannot answer for a
    /// path that does not exist there yet.
    #[test]
    fn an_added_ignored_file_dirties_its_package() {
        let m = Matchers::compile(&Rules {
            ignore_globs: vec!["docs/**".into()],
            ..Default::default()
        })
        .unwrap();
        let mut p = PackageSet::new(["docs".to_string()]);
        let d = dirty_set(
            &[Change::new(Status::Added, "docs/new.md")],
            &mut p,
            &m,
            &NoConsumers,
        );
        assert_eq!(d.dirty.iter().next().unwrap(), "docs");
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
        let m = Matchers::compile(&Rules::default()).unwrap();
        let mut p = nested();
        let d = dirty_set(
            &[
                Change::new(Status::Added, "third_party/xla/service/BUILD"),
                Change::new(Status::Modified, "third_party/xla/BUILD"),
                Change::new(Status::Deleted, "third_party/xla/old/BUILD"),
                Change::new(Status::Modified, "third_party/xla/service/a.cc"),
            ],
            &mut p,
            &m,
            &NoConsumers,
        );
        assert!(d.dirty.is_empty(), "{:?}", d.dirty);
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
        b.set_repo_boundaries(vec![cambium_index::Boundary {
            dir: "third_party/xla".into(),
            bazelignored: false,
        }]);
        b.apply_packages(&[cambium_index::PackageUpdate {
            package: "lib".into(),
            build_hash: [1; 32],
            bzl_hash: [0; 32],
            targets: vec![cambium_index::TargetRow::rule(
                "//lib:a",
                "cc_library",
                vec![],
            )],
        }]);
        let d = b.finish("head");
        let p = PackageSet::from_index(d.view());
        assert!(p.contains("lib"));
        assert_eq!(
            p.boundary_of("third_party/xla/a.cc"),
            Some("third_party/xla")
        );
    }
}
