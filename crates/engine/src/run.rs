//! The incremental run, one stage at a time.
//!
//! [`compute`] is the pipeline: external repositories, the bazelrc screen, the
//! flag resolve, the dirty set, the query, the splice, the toolchain check, the
//! direct set and the walk. Each stage is a method on [`Run`] with a typed result, and the plan
//! (`crate::plan`) runs the first stages of the same pipeline against git
//! alone, so a dry run cannot say one thing and the run then do another.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cambium_classify::{DirtySet, Matchers, PackageSet};
use cambium_diff::Change;
use cambium_index::{
    is_toolchain_rule_class, GraphView, IndexBuilder, IndexRef, SpliceReport, WalkOptions,
};
use rustc_hash::FxHashMap;

use crate::bazelrc;
use crate::bzl;
use crate::direct::{self, DirectInputs, LabelInputs};
use crate::engine::{AffectedRequest, Engine, RunOutcome};
use crate::error::Result;
use crate::externals::{Externals, WorkspaceText};
use crate::flags::FlagReach;
use crate::query::{PackageQuerier, Rows};
use crate::reason::{self, BazelrcCause, ExternalsCause, GlobalReason, RuleChange};
use crate::report::{AffectedReport, Stats, Stopwatch, Timings, SCHEMA_VERSION};

/// A diff with the bazelrc changes it can prove irrelevant removed.
pub(crate) struct ScreenedDiff<'a> {
    pub(crate) changes: Vec<&'a Change>,
    /// Bazelrc files whose edit was classified safe.
    pub(crate) safe: usize,
    /// Changed Starlark build settings, to be resolved against the index.
    pub(crate) label_flags: Vec<bazelrc::LabelFlag>,
    /// Set when a bazelrc edit invalidates the whole run.
    pub(crate) global: Option<GlobalReason>,
}

impl Engine {
    /// Reads every changed bazelrc and decides what its edit means.
    ///
    /// A bazelrc is an invalidator by default, and the generic rule can only
    /// say "this file changed, so everything changed". Here the two texts are
    /// available, so the run can ask the narrower question: did any flag change
    /// that loading or analysis could see? When the answer is no, the change is
    /// removed from the diff entirely — it dirties no package, because nothing
    /// in the graph depends on the number of jobs — and when it is yes, or when
    /// the file was added, removed, or is not text, the run is global exactly as
    /// before. The screen can only ever shrink an answer that would otherwise
    /// have been `//...`.
    ///
    /// Needs no index, which is why the plan can run it too.
    pub(crate) fn screen_bazelrc<'a>(
        &self,
        changes: &'a [Change],
        req: &AffectedRequest,
    ) -> ScreenedDiff<'a> {
        let mut kept: Vec<&Change> = Vec::with_capacity(changes.len());
        let mut safe = 0usize;
        let mut label_flags: Vec<bazelrc::LabelFlag> = Vec::new();
        // Which files are rc files is a property of the root rc's import
        // graph, at either end of the range: a file imported only at head is
        // a file that just became one. Anything else named `.bazelrc` is a
        // nested workspace's, and an ordinary source change.
        let rcs: std::collections::BTreeSet<String> = [&req.base, &req.head]
            .into_iter()
            .flat_map(|rev| {
                let read = |p: &str| {
                    self.repo
                        .read_at(rev, p)
                        .and_then(|b| String::from_utf8(b).ok())
                };
                bazelrc::reachable(self.workspace(), &self.cfg.bazel.startup_flags, &read)
            })
            .collect();
        for c in changes {
            if !rcs.contains(&c.path) {
                kept.push(c);
                continue;
            }
            match self.bazelrc_verdict(c, req) {
                Ok(s) => {
                    tracing::debug!(
                        path = %c.path, changed_flags = s.changed_flags,
                        label_flags = s.label_flags.len(),
                        "the bazelrc edit only touched flags that cannot affect loading, \
                         or Starlark build settings the index can resolve"
                    );
                    safe += 1;
                    label_flags.extend(s.label_flags);
                }
                Err(cause) => {
                    let reason = GlobalReason::Bazelrc {
                        path: c.path.clone(),
                        cause,
                    };
                    tracing::info!(path = %c.path, %reason, "bazelrc edit forces a global run");
                    self.diagnostics.emit(reason.diagnostic());
                    return ScreenedDiff {
                        changes: changes.iter().collect(),
                        safe,
                        label_flags: Vec::new(),
                        global: Some(reason),
                    };
                }
            }
        }
        ScreenedDiff {
            changes: kept,
            safe,
            label_flags,
            global: None,
        }
    }

    /// `Ok(screen)` if this one bazelrc change is irrelevant to loading except
    /// for Starlark build settings the caller will resolve, `Err(cause)` if it
    /// is not or cannot be read.
    fn bazelrc_verdict(
        &self,
        c: &Change,
        req: &AffectedRequest,
    ) -> std::result::Result<bazelrc::Screen, BazelrcCause> {
        use cambium_diff::Status;
        let path = &c.path;
        match c.status {
            Status::Modified => {}
            _ => return Err(BazelrcCause::AddedOrRemoved),
        }
        let text = |rev: &str| -> std::result::Result<String, BazelrcCause> {
            let bytes = self
                .repo
                .read_at(rev, path)
                .ok_or_else(|| BazelrcCause::Unreadable {
                    rev: rev.to_string(),
                })?;
            String::from_utf8(bytes).map_err(|_| BazelrcCause::NotUtf8 {
                rev: rev.to_string(),
            })
        };
        let base = text(&req.base)?;
        let head = text(&req.head)?;
        let screen = bazelrc::screen(&base, &head, &self.cfg.bazelrc.allowlist_extra);
        match screen.unknown {
            Some(why) => Err(BazelrcCause::Unknown(why)),
            None => Ok(screen),
        }
    }
}

/// One run's stages, over the state every stage reads and the counters they
/// fill in.
pub(crate) struct Run<'a> {
    engine: &'a Engine,
    base: IndexRef<'a>,
    querier: &'a dyn PackageQuerier,
    req: &'a AffectedRequest,
    stats: Stats,
    timings: Timings,
    sw: Stopwatch,
}

/// The dirty set with everything the query and the direct set need from it.
///
/// Public because it is also the answer [`dirty_stage`] hands an offline
/// caller: the benchmark's replay holds a recorded run against it, and the
/// fields are what that comparison reads.
pub struct DirtyStage {
    pub dirty: DirtySet,
    /// The package layout at head, for resolving changed files to labels.
    pub packages: PackageSet,
    /// Of the dirty packages, the ones a changed Starlark file reached
    /// through the loading layer's `BZL_LOAD` edges. Their rules are marked
    /// direct outright: a `.bzl` edit can change a rule's implementation
    /// without moving any rule proto, so every rule hash comes back unchanged.
    pub bzl_dirty: BTreeSet<String>,
    /// The diff's `.bzl` paths. Not a dirty-set input any more; the
    /// external-repository stage's closure rescan needs them to know which
    /// memoised parses to drop.
    pub changed_bzl: BTreeSet<String>,
    /// Leaf nodes of the external repositories that moved, as `(id, label)`.
    pub external_nodes: Vec<(u32, String)>,
    /// How many packages each moved repository contributed, for the overflow
    /// log.
    pub packages_by_repo: BTreeMap<String, usize>,
    /// Set when the answer is `//...`; the other fields are then irrelevant.
    pub global: Option<GlobalReason>,
}

/// The dirty stage alone, over inputs that need no Bazel and no checkout.
///
/// This is [`Run::dirty`] as [`compute`] runs it -- the classifier over the
/// diff, nested repositories, the changed `.bzl` files through the load
/// closure, the external repositories' reach -- with a querier that refuses
/// to be asked. The dirty stage never queries; `query()` is the stage that
/// does, and it is not run here. What the caller gets is therefore the
/// engine's own routing, not a copy of it: the benchmark's offline replay
/// used to reproduce this stage by hand and drifted from it twice in a day.
///
/// `ext` is the externals stage's answer over the same diff
/// ([`crate::externals::analyse`]); the bazelrc screen and the flag resolve
/// that precede this stage in [`compute`] both read the working tree, which
/// an offline caller does not have, so `changes` is handed over unscreened.
///
/// Diagnostics the stage emits land on `engine`'s own sink, exactly as in a
/// run.
pub fn dirty_stage(
    engine: &Engine,
    base: IndexRef<'_>,
    req: &AffectedRequest,
    changes: &[Change],
    ext: &mut Externals,
) -> Result<DirtyStage> {
    struct Refuses;
    impl PackageQuerier for Refuses {
        fn query(&self, packages: &[String]) -> Result<Rows> {
            Err(crate::error::EngineError::BazelQuery {
                what: format!("{} packages", packages.len()),
                code: None,
                stderr: "the offline dirty stage does not query".into(),
            })
        }
        fn universe(&self) -> Result<Rows> {
            Err(crate::error::EngineError::BazelQuery {
                what: "//...".into(),
                code: None,
                stderr: "the offline dirty stage does not query".into(),
            })
        }
    }
    let refuses = Refuses;
    let mut run = Run::new(engine, base, &refuses, req);
    run.stats.changed_files = changes.len();
    let changes: Vec<&Change> = changes.iter().collect();
    run.dirty(&changes, ext)
}

/// What the query stage read back.
pub(crate) struct Queried {
    /// The packages asked for: the dirty set, or every package at head.
    to_query: Vec<String>,
    rows: Rows,
    /// Set when the dirty set overflowed and `//...` was re-queried instead.
    cold_reason: Option<String>,
}

/// The splice, before the builder is finished one way or the other.
struct Spliced<'a> {
    builder: IndexBuilder<'a>,
    splice: SpliceReport,
    deleted_by_package: Vec<(String, Vec<u32>)>,
    changed_file_ids: Vec<(u32, String)>,
    label_inputs: Option<LabelInputs<'a>>,
}

/// The pipeline proper: dirty set, query, splice, direct set, walk.
pub(crate) fn compute(
    engine: &Engine,
    base: IndexRef<'_>,
    changes: &[Change],
    querier: &dyn PackageQuerier,
    req: &AffectedRequest,
    texts: &dyn WorkspaceText,
) -> Result<RunOutcome> {
    let mut run = Run::new(engine, base, querier, req);
    run.stats.changed_files = changes.len();

    let mut ext = run.externals(changes, texts);

    // A changed bazelrc is read before the generic invalidator check, and
    // dropped from the diff when every flag it changed is known not to
    // reach loading or analysis. Anything else about it — an unknown flag,
    // an import, the file appearing or vanishing — invalidates the run, so
    // this only ever turns a global into a smaller answer.
    let screened = engine.screen_bazelrc(changes, req);
    run.stats.bazelrc_safe_diffs = screened.safe;
    if let Some(reason) = screened.global {
        return Ok(run.global(reason));
    }
    let flags = run.flags(&screened.label_flags);
    if let Some(reason) = flags.global {
        return Ok(run.global(reason));
    }

    let stage = run.dirty(&screened.changes, &mut ext)?;
    if let Some(reason) = stage.global {
        return Ok(run.global(reason));
    }
    run.timings.dirty = run.sw.lap();

    let queried = run.query(&stage, &ext)?;
    let spliced = run.splice(&stage, &queried, &ext)?;
    // The one escalation the splice itself can raise: a toolchain
    // declaration moved, and the graph has no edge from it to what it
    // moves. Decided here, over the splice report and the base, rather than
    // inside the direct set, which only sees a view with no rule classes.
    if let Some(reason) = run.toolchain_rules(&queried, &spliced) {
        return Ok(run.global_after_splice(reason));
    }
    Ok(run.answer(spliced, &stage, &flags, queried.cold_reason))
}

impl<'a> Run<'a> {
    pub(crate) fn new(
        engine: &'a Engine,
        base: IndexRef<'a>,
        querier: &'a dyn PackageQuerier,
        req: &'a AffectedRequest,
    ) -> Self {
        Self {
            engine,
            base,
            querier,
            req,
            stats: Stats {
                base_nodes: base.num_nodes(),
                ..Default::default()
            },
            timings: Timings::default(),
            sw: Stopwatch::start(),
        }
    }

    /// The external-repository stage.
    ///
    /// Runs first because its answer decides whether `MODULE.bazel` still
    /// counts as an invalidator: a diff this stage mapped onto a set of
    /// repositories has been accounted for precisely, and letting the same
    /// file also fire the invalidator rule would throw that away.
    pub(crate) fn externals(&mut self, changes: &[Change], texts: &dyn WorkspaceText) -> Externals {
        let ext = if self.engine.config().externals.enabled {
            // Which world Bazel reads the base tree in is decided here, once
            // per run, and only if the diff makes the stage ask.
            let (base, base_rev) = (self.base, self.req.base.as_str());
            crate::externals::analyse(base, changes, base_rev, &self.req.head, texts, || {
                crate::externals::mode_at(base, texts, base_rev)
            })
        } else {
            Externals::default()
        };
        self.stats.repos_dirty_by_input = ext.by_input;
        self.stats.repos_dirty_by_module = ext.by_module;
        self.stats.repos_label_only = ext.label_only;
        ext
    }

    /// A changed Starlark build setting is a changed *target*, not a changed
    /// flag: resolve it against the index and seed its node, or learn why
    /// that cannot be trusted. See `crate::flags`.
    fn flags(&mut self, label_flags: &[bazelrc::LabelFlag]) -> FlagReach {
        if label_flags.is_empty() {
            return FlagReach::default();
        }
        let reach = crate::flags::resolve(self.base, self.querier, label_flags);
        self.stats.bazelrc_flag_nodes = reach.nodes.len();
        if let Some(reason) = &reach.global {
            // W004 covers the bazelrc half; this is the other cause, and it
            // names a setting rather than a flag.
            self.engine.diagnostics().emit(reason.diagnostic());
        }
        reach
    }

    /// The dirty set: the loading layer's reverse walk over the screened diff,
    /// the classifier's path-shaped half, the packages a deleted BUILD file
    /// did not really delete, and the external repositories' reach into the
    /// graph.
    ///
    /// The walk is the whole of "which packages must be re-queried". Nothing
    /// here restates a rule of Bazel's loading phase: a changed path becomes
    /// the Skyframe keys it moved, and every `PACKAGE` or `PACKAGE_LOOKUP`
    /// node the reverse walk reaches is dirty. A sub-package carved out by a
    /// new `BUILD` file, a file entering a `glob`, a `.bzl` loaded only by
    /// other `.bzl` files: all three are edges Bazel recorded, and none of
    /// them is a case in this function.
    pub(crate) fn dirty(&mut self, changes: &[&Change], ext: &mut Externals) -> Result<DirtyStage> {
        let mut matchers = Matchers::compile(&self.engine.config().rules())?;
        matchers.exempt(ext.handled.clone());
        let mut packages = PackageSet::from_index(self.base);
        let mut dirty = cambium_classify::dirty_set(
            changes,
            &mut packages,
            &matchers,
            &cambium_classify::IndexConsumers(self.base),
        );

        let mut global: Option<GlobalReason> = None;
        if dirty.global {
            // The only thing that makes the classifier's own dirty set global
            // is an invalidator path or glob matching a changed file, and it
            // records which one; an empty path here would be a classifier
            // bug, not a run to fail. Its diagnostic is raised below, once
            // the external-repository stage has said whether it replaces it.
            global = Some(GlobalReason::Invalidator {
                path: dirty.invalidator.clone().unwrap_or_default(),
            });
        }

        // Changes inside a nested Bazel repository. `//...` stopped at its
        // directory, so the graph has no node for anything in there and no
        // package to re-query: the only route to the targets that depend on
        // it is the repository it is. Each boundary that received changes is
        // matched against the repositories the index knows are rooted at a
        // main-repo directory, and seeded exactly as an input-path edit to a
        // `local_path_override` directory is.
        //
        // A boundary nothing maps — and every boundary at all when the
        // externals stage is off, since nothing would then act on the
        // repository — is a global invalidation that says which directory did
        // it, *unless* `.bazelignore` names the directory. An ignored
        // directory is one Bazel never reads: it loads no package there, no
        // glob reaches into it, and the only way anything in the graph can
        // depend on it is through a repository rule naming it, which is the
        // mapping that was just looked for and not found. Nothing can depend
        // on it, so a change there moves nothing and counts as ignored. The
        // distinction is not academic: envoy's `docs/` and `mobile/` are both
        // `.bazelignore`d and carry a `MODULE.bazel` of their own, so without
        // it every documentation edit would be a whole-repository answer, for
        // no soundness gained. A boundary Bazel *does* read and nothing maps
        // stays global: there the graph really does have a hole where the
        // directory's targets should be.
        let nested = std::mem::take(&mut dirty.nested);
        for (dir, paths) in &nested {
            let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
            let mapped = self
                .engine
                .config()
                .externals
                .enabled
                .then(|| crate::externals::repo_rooted_at(self.base, dir))
                .flatten();
            match mapped {
                Some(repo) => {
                    tracing::debug!(
                        dir,
                        repo = self.base.repo_name(repo),
                        paths = paths.len(),
                        "a change inside a nested repository moved the repository it is"
                    );
                    ext.by_input += 1;
                    crate::externals::dirty_by_input(self.base, ext, repo, &paths);
                }
                None if self.base.boundary_is_bazelignored(dir) => {
                    tracing::debug!(
                        dir,
                        paths = paths.len(),
                        "a change inside an ignored nested repository nothing maps moves nothing"
                    );
                    dirty.ignored += paths.len();
                }
                None => ext.escalate(ExternalsCause::NestedRepositoryUnmapped { dir: dir.clone() }),
            }
        }
        self.stats.repos_dirty_by_input = ext.by_input;

        let walk = self.loading_layer_walk(changes, &mut dirty, &mut global);
        self.packages_the_layer_cannot_name(changes, &packages, &mut dirty);
        let changed_bzl: BTreeSet<String> = changes
            .iter()
            .filter(|c| cambium_classify::is_bzl_file(&c.path))
            .map(|c| c.path.clone())
            .collect();
        if !dirty.global {
            if let Some(reason) = self.workspace_bzl(&changed_bzl, ext) {
                dirty.global = true;
                global = Some(reason);
            }
        }
        keep_surviving_packages(&mut dirty, self.querier, self.engine.workspace());
        // A package the diff deleted is one this run acted on, so it belongs
        // in the dirty set even though the query stage will not ask for it.
        //
        // The classifier used to put it there as a side effect of its carving
        // rule. Nothing else does now, and `dirty_packages` is not only a
        // report field: it is how a deleted package is *accounted for*. Every
        // rule in it is tombstoned, so none can appear in the affected set,
        // and the rdeps oracle's deleted half accepts "a surviving affected
        // target in the package, or the package in the dirty set" and nothing
        // else. A 40-step walk on the `bazel` subject found it: the commit
        // that removed `third_party/py/{concurrent,dataclasses,mock}` left six
        // of their targets unaccounted for.
        let gone: Vec<String> = dirty.deleted_packages.iter().cloned().collect();
        dirty.dirty.extend(gone);
        self.stats.ignored_files = dirty.ignored;

        // The stage's own escalation replaces an invalidator's wording: both
        // say "everything", but only one of them says why in terms of the
        // module system. A run has one cause and one cause's diagnostic,
        // and `cambi explain` prints the first it finds, so the invalidator's
        // is raised only here, once nothing has replaced it: `MODULE.bazel`
        // and `WORKSPACE` are default invalidators, and a change the stage
        // escalated on used to be reported twice, under two codes.
        match ext.take_cause() {
            Some(cause) => {
                let reason = GlobalReason::Externals(cause);
                self.engine.diagnostics().emit(reason.diagnostic());
                dirty.global = true;
                global = Some(reason);
            }
            None => {
                if let Some(reason @ GlobalReason::Invalidator { .. }) = &global {
                    self.engine.diagnostics().emit(reason.diagnostic());
                }
            }
        }
        self.stats.repos_dirty_by_bzl = ext.by_bzl;

        // From dirty repositories to graph work.
        let mut external_nodes: Vec<(u32, String)> = Vec::new();
        let mut packages_by_repo: BTreeMap<String, usize> = BTreeMap::new();
        if !dirty.global && !ext.repos.is_empty() {
            let r = crate::externals::reach(self.base, &ext.repos, &ext.load_repos);
            tracing::debug!(
                repos = ?ext.repos,
                load_repos = ?ext.load_repos,
                nodes = r.nodes.len(),
                packages = r.packages.len(),
                "external repositories moved"
            );
            dirty.dirty.extend(r.packages);
            packages_by_repo = r.packages_by_repo;
            external_nodes = r
                .nodes
                .into_iter()
                .map(|id| (id, self.base.label(id).to_string()))
                .collect();
        }
        self.stats.external_direct_targets = external_nodes.len();

        Ok(DirtyStage {
            dirty,
            packages,
            bzl_dirty: walk.bzl_packages,
            changed_bzl,
            external_nodes,
            packages_by_repo,
            global,
        })
    }

    /// The one package a changed path names that the loading layer cannot.
    ///
    /// The layer is Bazel's graph *as it was when the index was built*, so it
    /// has a node for every directory Bazel looked at and for nothing else. A
    /// directory that did not exist at base is in neither category: Bazel
    /// never statted it, so there is no `PACKAGE_LOOKUP` to reach and no
    /// `PACKAGE` behind it. The walk still dirties the *parent* (the directory
    /// listing it reads moved, which is how the parent finds out it lost
    /// files) but the new package itself has to be named from the diff.
    ///
    /// This is a fact about a path, not a model of the loading phase: a
    /// `BUILD` file at `p/BUILD` defines the package `p`, and that is the same
    /// rule `PackageSet` applies to decide who owns a file. The case the
    /// carving rule existed for — the *parent* of the new package — is gone,
    /// and it is the half the layer answers better.
    ///
    /// It fires only where the layer is silent, and the layer itself says
    /// where that is: a package it holds a `PACKAGE` node for is one the walk
    /// speaks for. The diff's own status is not the test. A `BUILD` file that
    /// git reports as modified can still name a package that did not exist at
    /// base, because a range of commits that creates a file and then edits it
    /// can be folded either way, and reading the status as proof of novelty
    /// would miss exactly the package the change created.
    ///
    /// A synthetic cold-rebuild comparison is what found this: the incremental
    /// run was short of every target in a package the change created, because
    /// the layer built at base had never heard of the directory.
    fn packages_the_layer_cannot_name(
        &self,
        changes: &[&Change],
        packages: &PackageSet,
        dirty: &mut DirtySet,
    ) {
        if dirty.global {
            return;
        }
        for c in changes {
            if c.status == cambium_diff::Status::Deleted
                || !cambium_classify::is_build_file(&c.path)
                // A `BUILD` file inside a nested repository defines a package
                // of *that* repository, which this one can never query.
                || packages.boundary_of(&c.path).is_some()
            {
                continue;
            }
            let pkg = cambium_classify::dirname(&c.path);
            // The condition is the layer's own: a package it has a `PACKAGE`
            // node for is one the walk speaks for, and adding it here would be
            // a second opinion. One it has no node for is one Bazel had not
            // loaded when the index was built, and the only thing that knows
            // the name is the path.
            if self
                .base
                .sky
                .id_of(cambium_index::sky::Kind::Package, pkg)
                .is_some()
            {
                continue;
            }
            tracing::debug!(
                package = %pkg,
                path = %c.path,
                "a BUILD file names a package the layer has no node for; querying it"
            );
            dirty.dirty.insert(pkg.to_string());
        }
    }

    /// A changed `.bzl` the module system loads can define repository rules,
    /// whose output is not in the rule layer at all.
    ///
    /// This is the external-repository stage's business, not the dirty set's,
    /// and it is unchanged in what it decides: the index records which
    /// repositories each workspace-closure `.bzl` defines, so the usual answer
    /// is "these repositories moved", and only a file whose repositories
    /// cannot be named escalates.
    ///
    /// What changed is how "is this file in the workspace closure" is
    /// answered. It used to be a flag on the statically parsed closure, plus a
    /// witness that re-scanned the head checkout whenever the closure had no
    /// node for the file — because the closure could be short and had no way
    /// to know it. Now it is the `BZL_LOAD` key Bazel wrote
    /// (`KeyForBzlmod`, `KeyForWorkspace`) and the `BZL_LOAD -> BZL_LOAD`
    /// edges above it, which exist because Bazel followed them.
    fn workspace_bzl(
        &mut self,
        changed_bzl: &BTreeSet<String>,
        ext: &mut Externals,
    ) -> Option<GlobalReason> {
        if changed_bzl.is_empty()
            || !self.engine.config().externals.enabled
            || !self.base.sky.present
        {
            return None;
        }
        let mut unnamed: Vec<String> = Vec::new();
        for path in changed_bzl {
            if !self.base.sky.bzl_is_workspace_level(path) {
                continue;
            }
            let repos = self
                .base
                .bzl_id(path)
                .and_then(|id| crate::externals::repos_of_workspace_bzl(self.base, id));
            match repos {
                Some(repos) => {
                    tracing::debug!(%path, ?repos, "a workspace-level .bzl names its repositories");
                    ext.by_bzl += repos.len();
                    ext.name(path, repos.iter());
                    // The rule's implementation moved, so what it writes into
                    // the repository can be anything.
                    ext.load_repos.extend(repos.iter().cloned());
                    ext.repos.extend(repos);
                }
                None => unnamed.push(path.clone()),
            }
        }
        if unnamed.is_empty() {
            return None;
        }
        tracing::warn!(
            files = ?unnamed,
            "a changed .bzl is loaded by the module system and the index cannot name the \
             repositories it defines; treating the change as global"
        );
        let reason = GlobalReason::WorkspaceLevelBzl { paths: unnamed };
        self.engine.diagnostics().emit(reason.diagnostic());
        Some(reason)
    }

    /// The loading layer's reverse walk, and what it means.
    ///
    /// Every package the walk reaches goes into the dirty set, whether it is a
    /// `PACKAGE` node (a package that was loaded) or a `PACKAGE_LOOKUP` one (a
    /// directory Bazel looked in, which this commit may have just made a
    /// package). The second kind is why a new `BUILD` file needs no rule: the
    /// index built at base has no `PACKAGE` node for a package that did not
    /// exist there, but it has the lookup that found nothing, and the lookup
    /// carries the name.
    ///
    /// A base index with no layer cannot be walked, and the run says so rather
    /// than reading an absent layer as "no package is dirty". That can only be
    /// an index built by a row source with no Bazel behind it; a real one is
    /// built by a cold build, which fails outright if the dump does.
    fn loading_layer_walk(
        &mut self,
        changes: &[&Change],
        dirty: &mut DirtySet,
        global: &mut Option<GlobalReason>,
    ) -> cambium_index::SkyWalk {
        if dirty.global {
            return Default::default();
        }
        if !self.base.sky.present {
            let reason = GlobalReason::NoLoadingLayer;
            tracing::warn!(
                "the base index carries no Skyframe loading layer; \
                 treating the change as global"
            );
            self.engine.diagnostics().emit(reason.diagnostic());
            dirty.global = true;
            *global = Some(reason);
            return Default::default();
        }
        let seeds = crate::sky::seeds_of(changes);
        let walk = self.base.sky.walk(&seeds);
        tracing::debug!(
            seeds = seeds.len(),
            unknown_seeds = walk.unknown_seeds,
            visited = walk.visited,
            packages = walk.packages.len(),
            lookups = walk.lookups.len(),
            bzl_packages = walk.bzl_packages.len(),
            external_packages = walk.external_packages,
            "walked the Skyframe loading layer"
        );
        for p in walk.dirty() {
            dirty.dirty.insert(p.to_string());
        }
        self.stats.bzl_closure_hits = walk.bzl_packages.len();
        self.stats.sky_seeds = seeds.len();
        self.stats.sky_unknown_seeds = walk.unknown_seeds;
        self.stats.sky_visited = walk.visited;
        self.stats.sky_new_packages = walk.lookups.len();
        walk
    }

    /// The query: normally the dirty packages, and only those.
    ///
    /// Past `limits.max_dirty_packages` that stops being the cheap way round:
    /// a per-package `bazel query` costs tens of milliseconds a package, and
    /// once enough of them are dirty, one `//...` query answers for the whole
    /// repository for less. So the run re-queries everything and splices all
    /// of it onto the base.
    ///
    /// That is a cost decision, not an accuracy one, and the distinction is
    /// the whole point of the change: the limit it replaces
    /// (`max_changed_files`) made a large diff *global*, discarding the
    /// answer. A cold rebuild keeps it. Every package the dirty set would
    /// have named is in the universe, so the splice sees at least the rows
    /// the incremental path would have seen; the packages it did not name
    /// come back byte-identical and contribute nothing to the direct set.
    /// The soundness sweep asserts exactly that, by running each synthetic
    /// revision both ways.
    fn query(&mut self, stage: &DirtyStage, ext: &Externals) -> Result<Queried> {
        let dirty = &stage.dirty;
        self.stats.dirty_packages = dirty.dirty.len();
        let limit = self.engine.config().limits.max_dirty_packages;
        let cold = limit > 0 && dirty.dirty.len() > limit;
        let out = if cold {
            let reason = reason::cold_rebuild_reason(dirty.dirty.len(), limit);
            self.engine
                .diagnostics
                .emit(reason::cold_rebuild_diagnostic(&reason));
            // Distinguishable from a genuine cold build, which says `running
            // full bazel query //... (cold build)`: this one re-queries the
            // same `//...` but keeps the lineage and splices onto the base, so
            // the answer stays exact and only the cost changed. `reason`'s
            // wording is load-bearing — the bench harness histograms it — so
            // everything new goes in the fields and the lines below it.
            tracing::info!(
                %reason,
                dirty_packages = dirty.dirty.len(),
                limit,
                "re-querying //... at head because the dirty set overflowed \
                 (lineage kept, answer still exact)"
            );
            log_overflow_contributors(ext, &stage.packages_by_repo);
            let rows = self.querier.universe()?;
            Queried {
                to_query: rows.updates.iter().map(|p| p.package.clone()).collect(),
                rows,
                cold_reason: Some(reason),
            }
        } else {
            let to_query: Vec<String> = dirty
                .dirty
                .iter()
                .filter(|p| !dirty.deleted_packages.contains(*p))
                .cloned()
                .collect();
            let rows = self.querier.query(&to_query)?;
            Queried {
                to_query,
                rows,
                cold_reason: None,
            }
        };
        self.stats.cold_rebuild = cold;
        self.stats.queried_targets = out.rows.updates.iter().map(|u| u.targets.len()).sum();
        self.timings.query = self.sw.lap();
        Ok(out)
    }

    /// The splice: the rows onto the base, the deleted packages out of it,
    /// and — while the builder still has its interner — the changed source
    /// files resolved to node ids.
    fn splice(
        &mut self,
        stage: &DirtyStage,
        queried: &Queried,
        ext: &Externals,
    ) -> Result<Spliced<'a>> {
        let base = self.base;
        let dirty = &stage.dirty;
        let mut builder = IndexBuilder::from_view(base);
        builder.set_config_hash(self.engine.config().hash());
        // A query that exited 3 under `--keep_going` loaded some of what it
        // was asked for, and the index has to say so — a package that failed
        // to load was tombstoned as if deleted, and only this flag tells a
        // reader that its targets are missing rather than gone.
        if let Some(e) = &queried.rows.partial {
            builder.set_partial(e.count, e.packages.clone());
        }
        // A nested repository can appear or vanish in any commit, so the
        // boundary list is re-walked from the checkout these rows came from
        // rather than carried forward from the base.
        let boundaries = self
            .querier
            .workspace()
            .map(cambium_index::repo_boundaries)
            .unwrap_or_default();
        builder.set_repo_boundaries(boundaries.clone());
        // Only the materialised path: an overlay is never published, and the
        // run that produced it read the closure it needed out of the base.
        if self.req.materialize {
            // Only the packages Bazel loaded keep a closure. A dirty package
            // that came back empty — it failed to load under `--keep_going`,
            // or it has no targets — is tombstoned above, and a full build at
            // the same commit never sees it in `//...` and so never scans its
            // BUILD file; the rebuild oracle compared the two and found the
            // closure of a package neither index has any target for (apollo's
            // packages that load `@local_config_cuda`). Its BUILD is read
            // again the moment it loads, since that re-queries it.
            let loaded: BTreeSet<&str> = queried
                .rows
                .updates
                .iter()
                .filter(|u| !u.targets.is_empty())
                .map(|u| u.package.as_str())
                .collect();
            let (scanned, unloaded): (Vec<String>, Vec<String>) = queried
                .to_query
                .iter()
                .cloned()
                .partition(|p| loaded.contains(p.as_str()));
            let mut gone = dirty.deleted_packages.clone();
            gone.extend(unloaded);
            // The loading layer goes first, before anything else is asked
            // of this server. A Bazel command whose option set differs from
            // the last one's re-evaluates, and the closure rescan below runs
            // two such commands (`mod dump_repo_mapping` and
            // `query //external:* --output=build`), each of which costs the
            // dump packages it would otherwise have named.
            // The loading layer, refreshed from the same server the splice
            // query just ran on, and only for what that query can have moved.
            //
            // The filter is not an optimisation. A Bazel server holds
            // everything it has ever loaded, so on the server a cold build
            // filled -- which is every `cambi profile` step, every bench walk
            // step, and any CI worker that built its own index -- an
            // unfiltered dump walks and prints the whole repository on every
            // commit. A sourcegraph profile spent 15 to 20 seconds a step on
            // exactly that, including steps that re-queried nothing at all.
            let mut layer = builder.take_sky_layer();
            if layer.is_some() {
                // Nothing re-queried and nothing deleted means no
                // loading-phase node moved, by construction: the dirty set is
                // what the layer's own walk produced. The cheapest dump is the
                // one that does not run.
                let filter = if queried.cold_reason.is_some() {
                    // `//...` was re-queried, so everything is fair game and
                    // the package list would be the whole repository anyway.
                    Some(cambium_index::sky::SKYKEY_FILTER.to_string())
                } else {
                    cambium_index::sky::refresh_filter(&queried.to_query)
                };
                // Without the packages this run found gone: a deleted package
                // has no `PACKAGE` block in any dump, and the refresh check
                // would read its absence as a dump that lost it. They are
                // dropped from the layer below instead.
                let requeried: BTreeSet<String> = queried
                    .to_query
                    .iter()
                    .filter(|p| !gone.contains(*p))
                    .cloned()
                    .collect();
                tracing::debug!(
                    packages = requeried.len(),
                    cold = queried.cold_reason.is_some(),
                    first = ?requeried.iter().take(5).collect::<Vec<_>>(),
                    "refreshing the loading layer for the packages this run re-queried"
                );
                match filter {
                    Some(filter) => {
                        layer = crate::sky::build_layer(
                            self.querier,
                            layer,
                            &filter,
                            Some(&requeried),
                        )?;
                    }
                    None if queried.to_query.is_empty() => {
                        tracing::debug!(
                            "no package was re-queried, so no loading-phase node moved and \
                             the layer needs no dump"
                        );
                    }
                    // More packages than a filter is worth writing: ask for
                    // the whole loading phase rather than build a megabyte of
                    // regex to save less than the dump costs.
                    None => {
                        layer = crate::sky::build_layer(
                            self.querier,
                            layer,
                            cambium_index::sky::SKYKEY_FILTER,
                            Some(&requeried),
                        )?;
                    }
                }
                if let Some(l) = layer.as_mut() {
                    // A package whose BUILD file is gone has no `PACKAGE` node
                    // to refresh, and the dump cannot say so by omission: a
                    // package that merely was not re-queried is omitted too.
                    //
                    // Only the packages the *diff* deleted, never `gone`, which
                    // also carries the packages this run re-queried and got no
                    // targets for. Those two are not the same thing and the
                    // difference is unrecoverable: retiring a `PACKAGE` node
                    // clears its dependencies, so it leaves every reverse row,
                    // and from then on a changed source file in that package
                    // reaches its glob node and stops. Only a `BUILD` edit
                    // could ever re-query it again (its `PACKAGE_LOOKUP`
                    // survives), so its targets -- already tombstoned by the
                    // empty update -- stay missing from the index for good. A
                    // package Bazel could not load under `--keep_going` comes
                    // back with no targets and is exactly that case, which is
                    // why a subject with load errors bled whole packages:
                    // //xla/python/ifrt/ir and its seventy targets, missing
                    // from every later step of a 300-step walk.
                    for p in &dirty.deleted_packages {
                        l.delete_package(p);
                    }
                }
            }
            if let Some(l) = layer {
                builder.set_sky_layer(l);
            }
            if let Some(c) = bzl::rescan_closure(
                self.querier,
                builder.take_bzl_closure(),
                &scanned,
                &stage.changed_bzl,
                &gone,
                &crate::bzl::dirs(&boundaries),
            ) {
                builder.set_bzl_closure(c);
            }
        }
        if !base.bazel_version.is_empty() {
            builder.set_bazel_version(base.bazel_version.to_string());
        }
        let mut deleted_by_package: Vec<(String, Vec<u32>)> = Vec::new();
        for p in &dirty.deleted_packages {
            let ids = builder.delete_package(p);
            deleted_by_package.push((p.clone(), ids));
        }
        let splice = builder.apply_packages(&queried.rows.updates);

        // Resolve changed source files to node ids while the builder still has
        // its interner: rebuilding a label map over a finished index costs
        // O(labels), and at a few million labels that is the single most
        // expensive thing in an otherwise change-proportional run.
        let mut changed_file_ids: Vec<(u32, String)> = Vec::with_capacity(dirty.direct_files.len());
        for path in &dirty.direct_files {
            let Some(pkg) = stage.packages.owner_of(path) else {
                continue;
            };
            let label = cambium_index::label::source_label(pkg, path);
            match builder.label_id(&label) {
                Some(id) => changed_file_ids.push((id, path.clone())),
                // The most common "why wasn't my test run?" question: a file
                // no rule references cannot affect anything.
                None => tracing::debug!(%label, "changed file is not referenced by any rule"),
            }
        }

        // Which repositories are built around a main-repo *target*, and which
        // node each of those targets is. Resolved against the base index, whose
        // ids the splice preserves.
        let label_inputs = self
            .engine
            .cfg
            .externals
            .enabled
            .then(|| direct::label_inputs(base, ext.repos.clone()))
            .flatten();

        Ok(Spliced {
            builder,
            splice,
            deleted_by_package,
            changed_file_ids,
            label_inputs,
        })
    }

    /// The toolchain check: did the splice add, change or tombstone a
    /// `toolchain`, `platform` or `toolchain_type` rule?
    ///
    /// Toolchain resolution happens in analysis and has no edge in the query
    /// graph. A `toolchain()` that matches better than the one every
    /// `cc_library` resolved to yesterday changes what all of them build
    /// with, and nothing in the reverse walk reaches it: the rules that will
    /// use it do not name it, and the `register_toolchains` call that offers
    /// it is not an edge either. So one of these rules moving, in any
    /// package, is a whole-repository answer. The default invalidator globs
    /// (`tools/toolchains/**`, `platforms/**`) used to stand in for this, and
    /// were a naming convention rather than a rule: a `platform()` declared
    /// elsewhere never escalated, and a `filegroup` under `tools/toolchains/`
    /// always did.
    ///
    /// The three classes are native rules with no implementation of their
    /// own, so a `.bzl` edit can only move one through its proto (a macro
    /// that declares it), which the rule hash sees; a changed `.bzl` that
    /// leaves every hash alone moved no declaration and takes the ordinary
    /// route. What the hash cannot see is the implementation *behind* a
    /// declaration (a `cc_toolchain_config` whose `.bzl` changed): the walk
    /// reaches the `toolchain()` naming it through ordinary edges and stops
    /// there, which is the analysis-phase edge declined with the rest.
    ///
    /// The head class comes from the rows this run queried and the base class
    /// from the base index, whose ids the splice preserves; a tombstoned node
    /// has no class left in the builder, so the base is the only place a
    /// removed rule's class can be read.
    fn toolchain_rules(
        &mut self,
        queried: &Queried,
        spliced: &Spliced<'_>,
    ) -> Option<GlobalReason> {
        let base = self.base;
        // Only the rows of the three classes: in the usual run there are
        // none, and the map is empty.
        let head_class: FxHashMap<&str, &str> = queried
            .rows
            .updates
            .iter()
            .flat_map(|u| u.targets.iter())
            .filter_map(|t| {
                let c = t.rule_class.as_deref()?;
                is_toolchain_rule_class(c).then_some((t.label.as_str(), c))
            })
            .collect();
        let in_base = |id: u32| id < base.num_nodes();
        let base_class = |id: u32| {
            in_base(id)
                .then(|| base.rule_class(id))
                .flatten()
                .filter(|c| is_toolchain_rule_class(c))
        };
        let mut moved: Vec<(String, String, RuleChange)> = Vec::new();
        for pd in &spliced.splice.packages {
            for &id in &pd.changed {
                let label = spliced.builder.label(id);
                match (base_class(id), head_class.get(label).copied()) {
                    (None, None) => {}
                    // Declared with one of the classes now and not before:
                    // new to the graph, or a rule of another class until now.
                    (None, Some(c)) => {
                        let change = if in_base(id) && base.is_rule(id) {
                            RuleChange::Changed
                        } else {
                            RuleChange::Added
                        };
                        moved.push((label.to_string(), c.to_string(), change));
                    }
                    (Some(_), Some(c)) => {
                        moved.push((label.to_string(), c.to_string(), RuleChange::Changed));
                    }
                    // Declared with one of the classes before, and as
                    // something else now: the declaration is gone.
                    (Some(c), None) => {
                        moved.push((label.to_string(), c.to_string(), RuleChange::Removed));
                    }
                }
            }
            for &id in &pd.deleted {
                if let Some(c) = base_class(id) {
                    moved.push((
                        base.label(id).to_string(),
                        c.to_string(),
                        RuleChange::Removed,
                    ));
                }
            }
        }
        for (_, ids) in &spliced.deleted_by_package {
            for &id in ids {
                if let Some(c) = base_class(id) {
                    moved.push((
                        base.label(id).to_string(),
                        c.to_string(),
                        RuleChange::Removed,
                    ));
                }
            }
        }
        if moved.is_empty() {
            return None;
        }
        // One rule names the cause, the first by label so the report does
        // not depend on query order; the count goes in the log line, which
        // is where anything beyond the sentence belongs.
        moved.sort_by(|a, b| a.0.cmp(&b.0));
        let count = moved.len();
        let (label, class, change) = moved.swap_remove(0);
        let reason = GlobalReason::ToolchainRule {
            label,
            class,
            change,
        };
        tracing::info!(
            %reason,
            moved = count,
            "a toolchain declaration moved; treating the change as global"
        );
        self.engine.diagnostics().emit(reason.diagnostic());
        Some(reason)
    }

    /// Materialise a whole new index, or take only the delta; then the direct
    /// set, the walk and the report over whichever it was.
    ///
    /// Both answer the same question over the same graph — one implements it
    /// as a complete index, the other as the base plus corrections — and the
    /// property tests compute every synthetic revision both ways and require
    /// the answers to be identical.
    fn answer(
        mut self,
        spliced: Spliced<'_>,
        stage: &DirtyStage,
        flags: &FlagReach,
        cold_reason: Option<String>,
    ) -> RunOutcome {
        let Spliced {
            builder,
            splice,
            deleted_by_package,
            changed_file_ids,
            label_inputs,
        } = spliced;
        let inputs = DirectInputs {
            splice: &splice,
            deleted_by_package: &deleted_by_package,
            changed_file_ids: &changed_file_ids,
            bzl_dirty: &stage.bzl_dirty,
            external_nodes: &stage.external_nodes,
            flag_nodes: &flags.nodes,
            label_inputs: label_inputs.as_ref(),
        };
        if self.req.materialize {
            let head_index = builder.finish(&self.req.head);
            let hv = head_index.view();
            self.stats.head_nodes = hv.num_nodes();
            self.stats.head_edges = hv.num_edges() as u64;
            let report = self.report(&hv, &inputs, &stage.dirty, cold_reason, &flags.flags);
            RunOutcome {
                report,
                index: Some(head_index),
                base_index: None,
            }
        } else {
            let overlay = builder.finish_overlay();
            let delta = overlay.delta_size();
            self.stats.head_nodes = overlay.num_nodes();
            // The overlay knows the base's edge count and every edge it added
            // or removed, which is the same number the materialised index
            // would report.
            self.stats.head_edges = self.base.num_edges() as u64 + delta.edges_added as u64
                - delta.edges_removed as u64;
            tracing::debug!(delta = ?delta, "walking a delta overlay");
            let report = self.report(&overlay, &inputs, &stage.dirty, cold_reason, &flags.flags);
            RunOutcome {
                report,
                index: None,
                base_index: None,
            }
        }
    }

    /// Direct set, walk, report — over a materialised head index or an
    /// overlay.
    fn report<V: GraphView + ?Sized>(
        &mut self,
        view: &V,
        inputs: &DirectInputs<'_>,
        dirty: &DirtySet,
        cold_reason: Option<String>,
        bazelrc_flags: &[String],
    ) -> AffectedReport {
        self.timings.splice = self.sw.lap();
        // What the splice actually wrote: rows that are new or whose rule
        // hash changed, rows it tombstoned out of a re-queried package, and
        // rows it tombstoned because the package itself was deleted.
        self.stats.spliced_rows = inputs.splice.changed_ids().count()
            + inputs.splice.deleted_ids().count()
            + inputs
                .deleted_by_package
                .iter()
                .map(|(_, ids)| ids.len())
                .sum::<usize>();
        let wopts = WalkOptions {
            include_manual: self.req.include_manual,
            explain: self.req.explain,
            max_depth: self.req.max_depth,
        };
        let (mut direct, mut reasons) = direct::derive_direct(view, inputs);
        self.timings.direct = self.sw.lap();

        let mut result = cambium_index::walk::walk_view(view, &direct, &wopts);
        if let Some(li) = inputs.label_inputs {
            self.stats.repos_dirty_by_target = direct::close_over_repository_targets(
                view,
                li,
                &mut direct,
                &mut reasons,
                &mut result,
                &wopts,
            );
        }
        self.stats.direct_targets = direct.len();
        self.stats.affected_targets = result.affected.len();
        self.timings.walk = self.sw.lap();
        let affected = direct::affected_targets(view, &result);
        let depths = histogram(&affected, |t| t.depth);
        let package_depths = histogram(&affected, |t| t.package_depth);

        AffectedReport {
            schema: SCHEMA_VERSION,
            base: self.req.base.clone(),
            head: self.req.head.clone(),
            global: false,
            global_reason: None,
            cold_rebuild_reason: cold_reason,
            dirty_packages: dirty.dirty.iter().cloned().collect(),
            bazelrc_flags: bazelrc_flags.to_vec(),
            direct: cambium_index::view::labels_sorted(view, &direct),
            affected,
            tests: cambium_index::view::labels_sorted(view, &result.tests),
            depth_histogram: depths,
            package_depth_histogram: package_depths,
            timings_ms: std::mem::take(&mut self.timings),
            stats: std::mem::take(&mut self.stats),
            explanations: self
                .req
                .explain
                .then(|| direct::explain(view, &result, &reasons)),
            // Filled by `Engine::affected` from the sink; a caller that drives
            // `compute` directly fills it from `engine.diagnostics()` itself.
            diagnostics: Vec::new(),
        }
    }

    /// The run is over before a splice happened: everything is affected.
    fn global(mut self, reason: GlobalReason) -> RunOutcome {
        self.timings.dirty = self.sw.lap();
        self.timings.total = self.timings.dirty;
        self.global_outcome(reason)
    }

    /// The run is over after the splice: the query and the splice were paid
    /// for, and everything is affected anyway.
    ///
    /// The spliced index goes with the answer, as it does for every global
    /// (`RunOutcome::index` is `None`), so `cambi index update` rebuilds a
    /// lineage at head. Keeping it would be sound, since the graph was spliced
    /// exactly as on any other run and toolchain resolution is not in it
    /// either way; it is the first thing to change if a subject's history
    /// shows declarations moving often enough for the cold build to matter.
    fn global_after_splice(mut self, reason: GlobalReason) -> RunOutcome {
        self.timings.splice = self.sw.lap();
        self.timings.total = self.timings.dirty + self.timings.query + self.timings.splice;
        self.global_outcome(reason)
    }

    fn global_outcome(self, reason: GlobalReason) -> RunOutcome {
        RunOutcome {
            report: AffectedReport {
                schema: SCHEMA_VERSION,
                base: self.req.base.clone(),
                head: self.req.head.clone(),
                global: true,
                global_reason: Some(reason.to_string()),
                timings_ms: self.timings,
                stats: self.stats,
                ..AffectedReport::default()
            },
            index: None,
            base_index: None,
        }
    }
}

/// How many affected targets sit at each distance, densely, index 0 first,
/// for whichever distance `of` reads off a target.
///
/// The walk already labelled every target with the layer it was first
/// reached in, so this is a count rather than a second traversal; a distance
/// nothing reached is a zero and not a gap, so the list draws as a histogram
/// with no holes in it.
fn histogram(
    affected: &[crate::report::AffectedTarget],
    of: impl Fn(&crate::report::AffectedTarget) -> u32,
) -> Vec<u32> {
    let Some(max) = affected.iter().map(&of).max() else {
        return Vec::new();
    };
    let mut out = vec![0u32; max as usize + 1];
    for t in affected {
        out[of(t) as usize] += 1;
    }
    out
}

/// A deleted `BUILD` does not always delete the package.
///
/// `bazel/bazel` spent several commits carrying both `docs/BUILD` and
/// `docs/BUILD.bazel`, and the commit that removed the old one reads, in the
/// diff alone, exactly like a package deletion — after which the package is
/// dropped from the index and every target in it becomes invisible to every
/// later revision. (The change-set comparison found this: `//docs:docs` and
/// `//docs:srcs` were reported by both bazel-diff and Tango, on pairs where
/// cambium's index no longer had a `docs` package at all.) The diff cannot
/// tell the two cases apart, but the head checkout can: if either BUILD file
/// is still on disk, the package lives and is merely dirty.
///
/// The check runs in the querier's own checkout, which for a revision the
/// working tree is not on is a worktree at that revision — asking the working
/// tree whether a BUILD file survived would answer for the wrong revision.
/// `fallback` is for a querier with no checkout at all (the synthetic one).
fn keep_surviving_packages(dirty: &mut DirtySet, querier: &dyn PackageQuerier, fallback: &Path) {
    if dirty.global || dirty.deleted_packages.is_empty() {
        return;
    }
    let ws = querier.workspace().unwrap_or(fallback);
    let survivors: Vec<String> = dirty
        .deleted_packages
        .iter()
        .filter(|p| {
            let dir = ws.join(p.as_str());
            dir.join("BUILD").exists() || dir.join("BUILD.bazel").exists()
        })
        .cloned()
        .collect();
    for p in survivors {
        tracing::debug!(package = %p, "a BUILD file went away but the package did not");
        dirty.deleted_packages.remove(&p);
        dirty.dirty.insert(p);
    }
}

/// What actually filled the dirty set, when it overflowed.
///
/// The repository set alone is hundreds of names and thousands of characters
/// and says nothing about *why*; what diagnoses the overflow is which changed
/// file dirtied which repository — an input path, a workspace `.bzl`,
/// `MODULE.bazel` or its lock — and how many distinct packages each
/// repository's rule (2) reach then contributed. Ten lines at most, the
/// largest contributors first, at `info` — a run that just decided to
/// re-query the whole repository can afford them.
fn log_overflow_contributors(ext: &Externals, packages_by_repo: &BTreeMap<String, usize>) {
    const MAX_LINES: usize = 10;
    let mut rows: Vec<(&str, usize, &[String])> = ext
        .dirtied_by
        .iter()
        .map(|(repo, files)| {
            (
                repo.as_str(),
                packages_by_repo.get(repo).copied().unwrap_or(0),
                files.as_slice(),
            )
        })
        .collect();
    // Every route that names a repository records what named it; a row with
    // nothing here is a repository the reach found that no route claimed,
    // which is worth seeing rather than hiding.
    for (repo, packages) in packages_by_repo {
        if !ext.dirtied_by.contains_key(repo) {
            rows.push((repo.as_str(), *packages, &[]));
        }
    }
    if rows.is_empty() {
        return;
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    tracing::info!(
        repositories = rows.len(),
        shown = rows.len().min(MAX_LINES),
        "top contributors to the dirty set"
    );
    for (repo, packages, paths) in rows.iter().take(MAX_LINES) {
        tracing::info!(
            repo = %repo,
            packages = packages,
            dirtied_by = ?paths,
            "  contributor"
        );
    }
}
