//! Changed `.bzl` files: which packages they dirty, answered from the index's
//! static load closure, and the witness that asks the head checkout when the
//! closure has no node for one of them.
//!
//! The `rbuildfiles` call this replaces needed Sky Query, which loads every
//! external repository and therefore fails with exit 3 on any machine that
//! cannot fetch one of them — and a partial answer there is unusable, so the
//! run had to invalidate everything. The closure is computed at index time
//! from the text of the BUILD and `.bzl` files, so this path has no Bazel
//! process in it and no failure mode: one binary search and one CSR row per
//! changed file.

use std::collections::BTreeSet;

use cambium_classify::DirtySet;
use cambium_index::IndexRef;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::diag::{Code, Diagnostic, Diagnostics};
use crate::externals::Externals;
use crate::query::PackageQuerier;
use crate::reason::GlobalReason;
use crate::report::Stats;

/// What resolving the changed `.bzl` files concluded.
pub(crate) struct BzlResolution {
    /// Packages dirtied because a changed `.bzl` is in their load closure.
    /// They take the `BzlClosure` direct reason, which is what makes a rule
    /// implementation edit visible when no BUILD attribute moved.
    pub(crate) packages: BTreeSet<String>,
    /// Set when a changed `.bzl` cannot be localised; the run is global.
    pub(crate) global: Option<GlobalReason>,
}

/// The parts of a run the stage writes to.
pub(crate) struct BzlStage<'a> {
    pub(crate) base_view: IndexRef<'a>,
    pub(crate) querier: &'a dyn PackageQuerier,
    pub(crate) dirty: &'a mut DirtySet,
    pub(crate) ext: &'a mut Externals,
    pub(crate) stats: &'a mut Stats,
    pub(crate) diags: &'a Diagnostics,
    /// `[externals].enabled`: whether a workspace-closure `.bzl` may be
    /// answered with the repositories it defines rather than with `//...`.
    pub(crate) externals_enabled: bool,
}

/// Turns the changed `.bzl` files into dirty packages.
pub(crate) fn resolve_changed(s: BzlStage<'_>) -> BzlResolution {
    let mut out = BzlResolution {
        packages: BTreeSet::new(),
        global: None,
    };
    if s.dirty.global || s.dirty.changed_bzl.is_empty() {
        return out;
    }
    let paths: Vec<String> = s.dirty.changed_bzl.iter().cloned().collect();
    if !s.base_view.bzl_closure {
        // An index built by a row source that could not read the tree.
        // "No consumers" and "we never looked" are the same empty
        // table, and only one of them is safe to act on.
        tracing::warn!(
            count = paths.len(),
            "the base index carries no static .bzl load closure; treating the change as global"
        );
        let reason = GlobalReason::NoLoadClosure { paths };
        s.diags.emit(reason.diagnostic());
        out.global = Some(reason);
        return out;
    }
    let mut w = Witness {
        base_view: s.base_view,
        bzl_dirty: &mut out.packages,
        dirty: s.dirty,
        ext: s.ext,
        stats: s.stats,
        workspace_level: Vec::new(),
        diags: s.diags,
        externals_enabled: s.externals_enabled,
    };
    let mut unknown: Vec<String> = Vec::new();
    for path in &paths {
        // A `.bzl` the closure has no node for is usually loaded by
        // nothing: a dead file, or one added by this very commit (in which
        // case whatever BUILD file started loading it changed too, and
        // that dirties the package on its own). "Usually" is not good
        // enough to act on, so it is set aside for the witness, which
        // asks the head checkout instead of the table.
        match w.base_view.bzl_id(path) {
            Some(id) => w.dirty_known_bzl(id, path, false),
            None => unknown.push(path.clone()),
        }
    }
    if !unknown.is_empty() {
        w.witness_unknown_bzl(s.querier, &unknown);
    }
    let workspace_level = std::mem::take(&mut w.workspace_level);
    s.stats.bzl_closure_hits = out.packages.len();
    tracing::debug!(
        changed = paths.len(),
        packages = out.packages.len(),
        "resolved changed .bzl files from the static load closure"
    );
    if !workspace_level.is_empty() {
        tracing::warn!(
            files = ?workspace_level,
            "a changed .bzl is loaded outside any package; treating the change as global"
        );
        let reason = GlobalReason::WorkspaceLevelBzl {
            paths: workspace_level,
        };
        s.diags.emit(reason.diagnostic());
        out.global = Some(reason);
    }
    out
}

/// The parts of a run the witness may write to.
struct Witness<'a> {
    base_view: IndexRef<'a>,
    bzl_dirty: &'a mut BTreeSet<String>,
    dirty: &'a mut DirtySet,
    ext: &'a mut Externals,
    stats: &'a mut Stats,
    /// Changed `.bzl` files whose loader is `MODULE.bazel` or `WORKSPACE`
    /// and whose repositories the index could not name.
    workspace_level: Vec<String>,
    diags: &'a Diagnostics,
    externals_enabled: bool,
}

impl Witness<'_> {
    /// One known `.bzl`: its consumers, and its repositories when it is in
    /// the workspace closure. The same two moves whether the file was named
    /// by the diff or found by the witness, so the two answers cannot drift.
    /// `witnessed` says which, for the stats.
    fn dirty_known_bzl(&mut self, id: u32, path: &str, witnessed: bool) {
        // Reachable from MODULE.bazel/WORKSPACE: it can define repository
        // rules, whose output is not in the index at all. The index
        // records which repositories each such `.bzl` defines (format 5),
        // so the usual answer is "these repositories moved"; only a file
        // no extension and no repo rule reaches still escalates, as the
        // `rbuildfiles` path always did.
        if self.base_view.bzl_in_workspace(id) {
            match self
                .externals_enabled
                .then(|| crate::externals::repos_of_workspace_bzl(self.base_view, id))
                .flatten()
            {
                Some(repos) => {
                    tracing::debug!(%path, ?repos, "a workspace-level .bzl names its repositories");
                    self.ext.by_bzl += repos.len();
                    self.ext.name(path, repos.iter());
                    // The rule's implementation moved, so what it writes
                    // into the repository can be anything.
                    self.ext.load_repos.extend(repos.iter().cloned());
                    self.ext.repos.extend(repos);
                }
                None => self.workspace_level.push(path.to_string()),
            }
        }
        for &p in self.base_view.bzl_consumers(id) {
            if self.base_view.package_tombstoned(p) {
                continue;
            }
            let pkg = self.base_view.package_name(p).to_string();
            self.bzl_dirty.insert(pkg.clone());
            self.dirty.dirty.insert(pkg);
            if witnessed {
                self.stats.bzl_witness_packages += 1;
            }
        }
    }

    /// Decide, from the head checkout alone, what a changed `.bzl` the base
    /// index's load closure has no node for can affect.
    ///
    /// # Why the closure is not enough
    ///
    /// The "loaded by nothing" reading argues: a loader at head is either a
    /// file that changed in this diff — and then its own change dirties its
    /// package, or it is itself a changed `.bzl` — or a file unchanged since
    /// base, which therefore loaded it at base too, so the base closure would
    /// have a node for it. The second half assumes the base closure is
    /// *complete*, and there are known ways for it not to be: a load label
    /// the index-time walk could not resolve (`resolve_label` returning
    /// `None`, or an `@<own module name>//...` spelling read as external
    /// because the mapping had no main alias), a BUILD file the index-time
    /// checkout did not have, a package tombstoned as partial. The harness's
    /// `rbuildfiles` oracle used to catch that; on `repository_evaluation`
    /// change sets it no longer does.
    ///
    /// # What this does instead
    ///
    /// It re-derives the *inverse* of one hop of the load graph from the head
    /// checkout: every Starlark file, scanned once, gives the main-repo `.bzl`
    /// paths it loads, and that is inverted. A file no `load()` at head names
    /// is dead for real, and ignoring it is a statement about the tree rather
    /// than about a table that may be short. Anything else is followed:
    ///
    /// - a BUILD loader dirties its own package;
    /// - a `.bzl` loader the base closure *does* know is handed to exactly the
    ///   logic the known branch uses — its consumers, and its repositories if
    ///   it is in the workspace closure;
    /// - a `.bzl` loader the closure does not know either recurses, bounded
    ///   by the set of files that exist and cycle-safe through `seen`;
    /// - a WORKSPACE / MODULE.bazel loader means the file defines repository
    ///   rules, and there is no `.bzl` node to name the repositories from, so
    ///   it takes the workspace-level escalation.
    ///
    /// Labels are resolved with the main alias added to the repository
    /// mapping (`@envoy//pkg:f.bzl` inside envoy is a main-repo label), which
    /// is the very gap that can make the stored closure incomplete in the
    /// first place.
    fn witness_unknown_bzl(&mut self, querier: &dyn PackageQuerier, unknown: &[String]) {
        let (Some(files), Some(src)) = (querier.starlark_files(), querier.load_source()) else {
            // No tree to witness against: the old behaviour, and the report
            // says the check did not run rather than that it passed.
            self.stats.bzl_witness_unavailable += unknown.len();
            tracing::debug!(
                files = ?unknown,
                "a changed .bzl is unknown to the load closure and this row source has no checkout to witness against"
            );
            return;
        };
        let mapping = main_alias_mapping(querier.repo_mapping(), src);
        let loaders = invert_loads(&files, src, &mapping);

        for root in unknown {
            let mut found: Vec<String> = Vec::new();
            let mut seen: FxHashSet<&str> = FxHashSet::default();
            let mut stack: Vec<&str> = vec![root.as_str()];
            while let Some(path) = stack.pop() {
                if !seen.insert(path) {
                    continue;
                }
                let Some(list) = loaders.get(path) else {
                    continue;
                };
                for loader in list {
                    found.push(loader.clone());
                    let name = loader.rsplit('/').next().unwrap_or(loader.as_str());
                    if cambium_index::loads::WORKSPACE_FILES.contains(&name) {
                        // Reachable from the workspace files: repository
                        // rules, and no `.bzl` node to name the repositories
                        // from.
                        self.workspace_level.push(root.clone());
                    } else if name == "BUILD" || name == "BUILD.bazel" {
                        let pkg = cambium_index::loads::owning_dir(loader);
                        self.bzl_dirty.insert(pkg.clone());
                        self.dirty.dirty.insert(pkg);
                        self.stats.bzl_witness_packages += 1;
                    } else if let Some(id) = self.base_view.bzl_id(loader) {
                        self.dirty_known_bzl(id, loader, true);
                    } else {
                        stack.push(loader.as_str());
                    }
                }
            }
            if found.is_empty() {
                self.stats.bzl_witnessed_dead += 1;
                tracing::debug!(
                    path = %root,
                    scanned = files.len(),
                    "no load() in the head checkout names this .bzl; it is loaded by nothing"
                );
            } else {
                self.stats.bzl_witnessed_loaded += 1;
                found.sort();
                found.dedup();
                tracing::warn!(
                    path = %root,
                    loaders = ?found,
                    "a changed .bzl is loaded at head but has no node in the base index's load closure; \
                     the stored closure is incomplete and the witness dirtied its loaders instead"
                );
                self.diags.emit(
                    Diagnostic::warning(
                        Code::W010IncompleteClosure,
                        format!(
                            "`{root}` is loaded at head but has no node in the base index's load \
                             closure, so the stored closure is incomplete; the witness followed \
                             its {} loader(s) instead",
                            found.len()
                        ),
                    )
                    .at(root.to_string(), None)
                    .with_help(
                        "the answer is safe; rebuild the lineage with `cambi index build` to \
                         make it cheap again",
                    ),
                );
            }
        }
    }
}

/// Recomputes the static `.bzl` load closure for the packages a run touched.
///
/// `prior` is the base index's closure, carried through the splice: every
/// package that was not re-queried keeps the row it had, because nothing that
/// could have moved it is in this diff. What does get recomputed:
///
/// - every changed `.bzl` is invalidated, so it is re-read rather than
///   answered from the memoised parse;
/// - every re-queried package has its BUILD file re-scanned and its closure
///   rebuilt (a package is re-queried precisely when its BUILD file, or the
///   files it owns, moved — which is when its loads can have changed);
/// - a deleted package's closure goes with it;
/// - the workspace closure is rebuilt, since a `.bzl` can move into or out of
///   it without `MODULE.bazel` changing (`MODULE.bazel` itself is an
///   invalidator, so that case never reaches here).
///
/// Returns `None` when the row source cannot read the tree, which leaves the
/// index without a closure and makes the next `.bzl` change global.
pub(crate) fn rescan_closure(
    querier: &dyn PackageQuerier,
    prior: Option<cambium_index::BzlClosure>,
    packages: &[String],
    changed_bzl: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
) -> Option<cambium_index::BzlClosure> {
    if packages.is_empty() && changed_bzl.is_empty() && deleted.is_empty() {
        return prior;
    }
    let src = querier.load_source()?;
    // With the main repository's own apparent names: `@envoy//...` in a
    // repository rule's `.bzl` is a main-repo input under either name source.
    let mapping = main_alias_mapping(querier.repo_mapping(), src);
    let mut c = prior.unwrap_or_default();
    for p in changed_bzl {
        c.graph.invalidate(p);
    }
    for p in deleted {
        c.packages.remove(p);
        c.package_external.remove(p);
    }
    for p in packages {
        let pc = c.graph.package_closure(p, src, &mapping);
        c.packages.insert(p.clone(), pc.bzl);
        if pc.external.is_empty() {
            c.package_external.remove(p);
        } else {
            c.package_external.insert(p.clone(), pc.external);
        }
    }
    c.workspace = c.graph.workspace_closure(src, &mapping);
    // The workspace closure just moved, so what the main repository feeds its
    // external repositories may have moved with it. Which world Bazel reads
    // `MODULE.bazel` in is decided for the tree `src` reads — the head — not
    // taken from the externals stage, whose answer is about the base: the
    // two trees can disagree, and this closure describes the head.
    let mode = cambium_index::Mode::detect(src, &mapping);
    let facts = cambium_index::read_module_facts_in(src, mode);
    c.inputs = cambium_index::build_repo_inputs(src, &mapping, &c.graph, &c.workspace, &facts);
    Some(c)
}

/// The querier's mapping plus the main repository's own apparent name, taken
/// from `module(name = ...)` and, in WORKSPACE mode, `workspace(name = ...)`.
pub fn main_alias_mapping(
    m: cambium_index::RepoMapping,
    src: &dyn cambium_index::FileText,
) -> cambium_index::RepoMapping {
    let mut m = m.with_main_alias(&cambium_index::externals::read_module_facts(src).module_name);
    for name in ["WORKSPACE", "WORKSPACE.bazel"] {
        if let Some(n) = src
            .text(name)
            .as_deref()
            .and_then(cambium_index::loads::workspace_name)
        {
            m = m.with_main_alias(&n);
        }
    }
    m
}

/// Loaded main-repo `.bzl` path -> the files that `load()` it, from the head
/// checkout. Each file is read and scanned once.
fn invert_loads(
    files: &[String],
    src: &dyn cambium_index::FileText,
    m: &cambium_index::RepoMapping,
) -> FxHashMap<String, Vec<String>> {
    let mut out: FxHashMap<String, Vec<String>> = FxHashMap::default();
    for f in files {
        let Some(text) = src.text(f) else { continue };
        let name = f.rsplit('/').next().unwrap_or(f.as_str());
        let labels = if cambium_index::loads::WORKSPACE_FILES.contains(&name) {
            cambium_index::loads::scan_workspace_loads(&text)
        } else {
            cambium_index::loads::scan_loads(&text)
        };
        let from_pkg = cambium_index::loads::owning_dir(f);
        for l in labels {
            if let Some(cambium_index::Load::Main(p)) =
                cambium_index::loads::resolve_label(&l, &from_pkg, m)
            {
                out.entry(p).or_default().push(f.clone());
            }
        }
    }
    out
}
