//! Path classification and the dirty set (spec 6.1, 6.2).
//!
//! This is where a list of changed paths becomes "these packages must be
//! re-queried, these files mark their consumers direct, or everything is
//! affected". It is pure: no Bazel, no git, no filesystem. That makes it cheap
//! to test exhaustively, which matters because a mistake here is an
//! under-approximation — the one class of bug the design does not tolerate.

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
}

impl Matchers {
    pub fn compile(rules: &Rules) -> Result<Self, ClassifyError> {
        Ok(Self {
            exact: rules.invalidator_paths.iter().cloned().collect(),
            invalidators: build_globset(&rules.invalidator_globs)?,
            ignores: build_globset(&rules.ignore_globs)?,
        })
    }

    /// A root-level `BUILD` file is an invalidator by default (it can define
    /// package-wide settings), and so is anything matching a configured glob.
    pub fn is_invalidator(&self, path: &str) -> bool {
        self.exact.contains(path) || self.invalidators.is_match(path)
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
}

impl PackageSet {
    pub fn new(packages: impl IntoIterator<Item = String>) -> Self {
        Self {
            packages: packages.into_iter().collect(),
        }
    }

    /// Live packages recorded in an index.
    pub fn from_index(index: IndexRef<'_>) -> Self {
        let mut packages = FxHashSet::default();
        for p in 0..index.num_packages() {
            if !index.package_tombstoned(p) {
                packages.insert(index.package_name(p).to_string());
            }
        }
        Self { packages }
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

    pub fn insert(&mut self, pkg: String) {
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
}

pub fn classify(path: &str, packages: &PackageSet, m: &Matchers) -> Classification {
    // Invalidators win over everything, including ignores (spec 9).
    if m.is_invalidator(path) {
        return Classification::Invalidator;
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
    /// Why, when `global` is set.
    pub global_reason: Option<String>,
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
}

impl DirtySet {
    fn global(reason: impl Into<String>) -> Self {
        Self {
            global: true,
            global_reason: Some(reason.into()),
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
    changes: &[Change],
    packages: &mut PackageSet,
    m: &Matchers,
    max_changed_files: usize,
    consumed: &dyn Consumed,
) -> DirtySet {
    if max_changed_files > 0 && changes.len() > max_changed_files {
        return DirtySet::global(format!(
            "{} changed files exceeds max_changed_files = {}",
            changes.len(),
            max_changed_files
        ));
    }

    // Pass one: apply BUILD file adds and deletes so ownership questions in
    // pass two see the package layout at head.
    let mut out = DirtySet::default();
    for c in changes {
        if m.is_invalidator(&c.path) {
            return DirtySet::global(format!("{} matches an invalidator", c.path));
        }
        if !is_build_file(&c.path) {
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
            // distribution tarball) are missed. The first corpus run against
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
        if is_build_file(&c.path) {
            continue;
        }
        match classify(&c.path, packages, m) {
            Classification::Invalidator => {
                return DirtySet::global(format!("{} matches an invalidator", c.path))
            }
            Classification::Build(_) => unreachable!("handled in pass one"),
            Classification::Bzl(p) => {
                out.changed_bzl.insert(p);
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
        dirty_set(&changes, &mut p, &matchers(), 5000, &NoConsumers)
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
        assert!(d.global_reason.unwrap().contains("MODULE.bazel"));
    }

    #[test]
    fn oversized_diffs_fall_back_to_global() {
        let changes: Vec<Change> = (0..10)
            .map(|i| Change::new(Status::Modified, format!("lib/core/f{i}.cc")))
            .collect();
        let mut p = packages();
        let d = dirty_set(&changes, &mut p, &matchers(), 5, &NoConsumers);
        assert!(d.global);
        assert!(d.global_reason.unwrap().contains("max_changed_files"));
    }

    #[test]
    fn unowned_paths_are_ignored() {
        let mut p = PackageSet::new(["lib".to_string()]);
        let d = dirty_set(
            &[Change::new(Status::Modified, "README.md")],
            &mut p,
            &matchers(),
            0,
            &NoConsumers,
        );
        assert!(!d.global);
        assert!(d.dirty.is_empty());
        assert_eq!(d.ignored, 1);
    }

    /// The regression the corpus found on `bazel/bazel`: the default ignore
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
            0,
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
            0,
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
            0,
            &NoConsumers,
        );
        assert_eq!(d.dirty.iter().next().unwrap(), "docs");
        assert!(d.direct_files.is_empty());
    }
}
