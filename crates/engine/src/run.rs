//! The incremental run, one stage at a time.
//!
//! [`compute`] is the pipeline: external repositories, the bazelrc screen, the
//! flag resolve, the dirty set, the query, the splice, the direct set and the
//! walk. Each stage is a method on [`Run`] with a typed result, and the plan
//! (`crate::plan`) runs the first stages of the same pipeline against git
//! alone, so a dry run cannot say one thing and the run then do another.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cambium_classify::{DirtySet, Matchers, PackageSet};
use cambium_diff::Change;
use cambium_index::{GraphView, IndexBuilder, IndexRef, SpliceReport, WalkOptions};

use crate::bazelrc;
use crate::bzl;
use crate::direct::{self, DirectInputs, LabelInputs};
use crate::engine::{AffectedRequest, Engine, RunOutcome};
use crate::error::Result;
use crate::externals::{Externals, WorkspaceText};
use crate::flags::FlagReach;
use crate::query::{PackageQuerier, Rows};
use crate::reason::{self, BazelrcCause, ExternalsCause, GlobalReason};
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
pub(crate) struct DirtyStage {
    pub(crate) dirty: DirtySet,
    /// The package layout at head, for resolving changed files to labels.
    pub(crate) packages: PackageSet,
    /// Packages dirtied through a changed `.bzl` in their load closure.
    pub(crate) bzl_dirty: BTreeSet<String>,
    /// Leaf nodes of the external repositories that moved, as `(id, label)`.
    pub(crate) external_nodes: Vec<(u32, String)>,
    /// How many packages each moved repository contributed, for the overflow
    /// log.
    pub(crate) packages_by_repo: BTreeMap<String, usize>,
    /// Set when the answer is `//...`; the other fields are then irrelevant.
    pub(crate) global: Option<GlobalReason>,
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

    /// The dirty set: the classifier over the screened diff, the changed
    /// `.bzl` files resolved through the load closure, the packages a deleted
    /// BUILD file did not really delete, and the external repositories' reach
    /// into the graph.
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
            // bug, not a run to fail.
            let reason = GlobalReason::Invalidator {
                path: dirty.invalidator.clone().unwrap_or_default(),
            };
            self.engine.diagnostics().emit(reason.diagnostic());
            global = Some(reason);
        }

        // Changes inside a nested Bazel repository. `//...` stopped at its
        // directory, so the graph has no node for anything in there and no
        // package to re-query: the only route to the targets that depend on
        // it is the repository it is. Each boundary that received changes is
        // matched against the repositories the index knows are rooted at a
        // main-repo directory, and seeded exactly as an input-path edit to a
        // `local_path_override` directory is. A boundary nothing maps — and
        // every boundary at all when the externals stage is off, since
        // nothing would then act on the repository — is a global
        // invalidation that says which directory did it.
        for (dir, paths) in &dirty.nested {
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
                None => ext.escalate(ExternalsCause::NestedRepositoryUnmapped { dir: dir.clone() }),
            }
        }
        self.stats.repos_dirty_by_input = ext.by_input;

        let resolved = bzl::resolve_changed(bzl::BzlStage {
            base_view: self.base,
            querier: self.querier,
            dirty: &mut dirty,
            ext,
            stats: &mut self.stats,
            diags: self.engine.diagnostics(),
            externals_enabled: self.engine.config().externals.enabled,
        });
        if let Some(reason) = resolved.global {
            dirty.global = true;
            global = Some(reason);
        }
        keep_surviving_packages(&mut dirty, self.querier, self.engine.workspace());
        self.stats.ignored_files = dirty.ignored;

        // The stage's own escalation replaces an invalidator's wording: both
        // say "everything", but only one of them says why in terms of the
        // module system.
        if let Some(cause) = ext.take_cause() {
            let reason = GlobalReason::Externals(cause);
            self.engine.diagnostics().emit(reason.diagnostic());
            dirty.global = true;
            global = Some(reason);
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
            bzl_dirty: resolved.packages,
            external_nodes,
            packages_by_repo,
            global,
        })
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
            if let Some(c) = bzl::rescan_closure(
                self.querier,
                builder.take_bzl_closure(),
                &scanned,
                &dirty.changed_bzl,
                &gone,
                &boundaries,
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
