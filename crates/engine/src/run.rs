//! The incremental run, one stage at a time.
//!
//! [`compute`] is the pipeline: external repositories, the bazelrc screen, the
//! flag resolve, the dirty set, the query, the splice, the toolchain check, the
//! direct set and the walk. Each stage is a method on [`Run`] with a typed
//! result, and the plan (`crate::plan`) runs the stages before the query
//! against git alone, so a dry run names the same dirty set and the same
//! pre-query global the run would. One verdict is reached after the query:
//! the toolchain check reads rule classes off the rows the query returned,
//! so a splice or cold rebuild the plan announced can still end as `//...`
//! under `CAMBI-W023`, and the plan says so of itself.

use std::collections::{BTreeMap, BTreeSet};

use crate::externals::LeafSeeds;
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
    /// `--registry` moved in a line the query reads: a lock-file question the
    /// externals stage answers ([`Run::registry_change`]).
    pub(crate) registry: Option<bazelrc::RegistryChange>,
    /// Changed files named like rc files that Bazel is told not to read (the
    /// root `.bazelrc` under `--noworkspace_rc`). They stay in `changes` as
    /// ordinary source changes, and the dirty stage must not fire the
    /// `.bazelrc` invalidator on them.
    pub(crate) inert: Vec<String>,
    /// Set when a bazelrc edit invalidates the whole run.
    pub(crate) global: Option<GlobalReason>,
}

impl Engine {
    /// Reads every changed bazelrc and decides what its edit means.
    ///
    /// A bazelrc is an invalidator by default, and the generic rule can only
    /// say "this file changed, so everything changed". Here the rc chain at
    /// both ends of the range is available, so the run can ask the narrower
    /// question: did any flag the index query reads change in a way loading
    /// could see? When the answer is no, the change is removed from the diff
    /// entirely (it dirties no package, because nothing in the graph depends
    /// on the number of jobs), and when it is yes, or when a file was added,
    /// removed, or is not text, the run is global exactly as before. The
    /// screen can only ever shrink an answer that would otherwise have been
    /// `//...`.
    ///
    /// Needs no index, which is why the plan can run it too.
    pub(crate) fn screen_bazelrc<'a>(
        &self,
        changes: &'a [Change],
        req: &AffectedRequest,
    ) -> ScreenedDiff<'a> {
        use cambium_diff::Status;
        // Which files are rc files is a property of the root rc's import
        // graph, at either end of the range: a file imported only at head is
        // a file that just became one. Anything else named `.bazelrc` is a
        // nested workspace's, and an ordinary source change.
        let chain_at = |rev: &str| {
            let read = |p: &str| {
                self.repo
                    .read_at(rev, p)
                    .and_then(|b| String::from_utf8(b).ok())
            };
            bazelrc::chain(self.workspace(), &self.cfg.bazel.startup_flags, &read)
        };
        let base_chain = chain_at(&req.base);
        let head_chain = chain_at(&req.head);
        let mut rcs: BTreeSet<&str> = base_chain.files.keys().map(String::as_str).collect();
        rcs.extend(head_chain.files.keys().map(String::as_str));
        let workspace_rc = bazelrc::reads_workspace_rc(&self.cfg.bazel.startup_flags);

        let global = |path: &str, cause: BazelrcCause| -> ScreenedDiff<'a> {
            let reason = GlobalReason::Bazelrc {
                path: path.to_string(),
                cause,
            };
            tracing::info!(path, %reason, "bazelrc edit forces a global run");
            self.diagnostics.emit(reason.diagnostic());
            ScreenedDiff {
                changes: changes.iter().collect(),
                safe: 0,
                label_flags: Vec::new(),
                registry: None,
                inert: Vec::new(),
                global: Some(reason),
            }
        };

        let mut kept: Vec<&Change> = Vec::with_capacity(changes.len());
        let mut inert: Vec<String> = Vec::new();
        let mut rc_changes = 0usize;
        for c in changes {
            if !rcs.contains(c.path.as_str()) {
                // The root rc is a file Bazel reads for this workspace unless
                // the startup flags say otherwise; then it is a source file
                // like any other, and the `.bazelrc` invalidator must not
                // undo that.
                if c.path == bazelrc::WORKSPACE_RC && !workspace_rc {
                    tracing::debug!(
                        path = %c.path,
                        "the startup flags say Bazel does not read the workspace rc; its edit \
                         is an ordinary source change"
                    );
                    inert.push(c.path.clone());
                }
                kept.push(c);
                continue;
            }
            if c.status != Status::Modified {
                return global(&c.path, BazelrcCause::AddedOrRemoved);
            }
            // The chain reads text; a file that is not text at an end is
            // simply absent from that end's chain, which would read as "it
            // contributes nothing there". Say so instead.
            for rev in [&req.base, &req.head] {
                match self.repo.read_at(rev, &c.path) {
                    None => return global(&c.path, BazelrcCause::Unreadable { rev: rev.clone() }),
                    Some(bytes) if std::str::from_utf8(&bytes).is_err() => {
                        return global(&c.path, BazelrcCause::NotUtf8 { rev: rev.clone() })
                    }
                    Some(_) => {}
                }
            }
            rc_changes += 1;
        }
        if rc_changes == 0 {
            return ScreenedDiff {
                changes: kept,
                safe: 0,
                label_flags: Vec::new(),
                registry: None,
                inert,
                global: None,
            };
        }

        let configs = self.query_configs();
        let screen = bazelrc::screen_chain(
            &base_chain,
            &head_chain,
            &bazelrc::Invocation {
                configs: &configs,
                extra_allowlist: &self.cfg.bazelrc.allowlist_extra,
            },
        );
        if let Some(b) = screen.unknown {
            return global(&b.path, BazelrcCause::Unknown(b.why));
        }
        tracing::debug!(
            rc_files = rc_changes,
            changed_flags = screen.changed_flags,
            label_flags = screen.label_flags.len(),
            registry = screen.registry.is_some(),
            "the bazelrc edit only touched flags the query does not read or that cannot affect \
             loading, Starlark build settings the index can resolve, or --registry"
        );
        ScreenedDiff {
            changes: kept,
            safe: rc_changes,
            label_flags: screen.label_flags,
            registry: screen.registry,
            inert,
            global: None,
        }
    }

    /// The `--config` names the index query runs with on its command line:
    /// `[bazelrc].configs`, plus any `--config` in `[bazel].extra_query_flags`.
    fn query_configs(&self) -> Vec<String> {
        let mut out = self.cfg.bazelrc.configs.clone();
        let flags = &self.cfg.bazel.extra_query_flags;
        for (i, f) in flags.iter().enumerate() {
            if let Some(c) = f.strip_prefix("--config=") {
                out.push(c.to_string());
            } else if f == "--config" {
                if let Some(c) = flags.get(i + 1) {
                    out.push(c.clone());
                }
            }
        }
        out
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
    /// The moved repositories `externals::reach` left alone because every
    /// path that moved them is a modified source file of a package this
    /// index holds (`settled_repos`).
    pub settled: BTreeSet<String>,
    /// The indexed repositories whose consumers' leaf maps may no longer
    /// be Bazel's answer at head ([`stale_leaf_maps`]), for the splice to
    /// ask again.
    pub stale_leaf_maps: BTreeSet<String>,
    /// The leaf questions this run asked at head for a consumer whose map
    /// was suspect ([`ask_at_head`]), for the splice to reuse.
    pub leaf_answers: crate::consumers::Asked,
    /// The round-trip edges at head, asked because a repository outside the
    /// index's universe moved (`crate::round_trip::at_head`); the splice
    /// writes them over the base's rows.
    pub round_trip: Option<crate::round_trip::Answer>,
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
    texts: &dyn WorkspaceText,
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
    run.dirty(&changes, &[], ext, texts)
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
    // dropped from the diff when every flag it changed is one the query does
    // not read or is known not to reach loading. Anything else about it (an
    // unknown flag, an import that cannot be resolved, the file appearing or
    // vanishing) invalidates the run, so this only ever turns a global into
    // a smaller answer. A `--registry` edit is handed to the externals stage,
    // which has just read the lockfile.
    let screened = engine.screen_bazelrc(changes, req);
    run.stats.bazelrc_safe_diffs = screened.safe;
    if let Some(reason) = screened.global {
        return Ok(run.global(reason));
    }
    if let Some(reg) = &screened.registry {
        if let Some(reason) = run.registry_change(reg, &mut ext, texts) {
            return Ok(run.global(reason));
        }
    }
    let flags = run.flags(&screened.label_flags);
    if let Some(reason) = flags.global {
        return Ok(run.global(reason));
    }

    let stage = run.dirty(&screened.changes, &screened.inert, &mut ext, texts)?;
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

    /// A `--registry` edit the screen reported: a lock-file question.
    ///
    /// The registries decide what module resolution reads, and the resolved
    /// state is `MODULE.bazel.lock`. When the lock moved in the same diff and
    /// the externals stage read its diff, that diff already names the modules
    /// whose registry metadata moved, and the rc line adds nothing. The lock
    /// moving beside a changed `MODULE.bazel` is not that: there the stage
    /// takes the module diff and never reads the lock, so the only
    /// repositories named are the ones whose *declaration* moved, while a
    /// re-pointed registry can move the resolved state of every other module
    /// in the graph. When bzlmod is off at base the line is inert.
    /// Otherwise no lock diff was read beside the registry and what the new
    /// registries serve cannot be read from the tree, so the stage escalates
    /// and the dirty stage reports it (`CAMBI-W014`). With the stage off there is nobody to read the
    /// lock, and the edit is the blunt `CAMBI-W004` it always was.
    pub(crate) fn registry_change(
        &mut self,
        reg: &bazelrc::RegistryChange,
        ext: &mut Externals,
        texts: &dyn WorkspaceText,
    ) -> Option<GlobalReason> {
        if !self.engine.config().externals.enabled {
            let reason = GlobalReason::Bazelrc {
                path: reg.path.clone(),
                cause: BazelrcCause::Unknown(bazelrc::Unknown::NotAllowlisted {
                    command: reg.key.command.clone(),
                    config: reg.key.config.clone(),
                    name: reg.key.name.clone(),
                }),
            };
            tracing::info!(path = %reg.path, %reason, "bazelrc edit forces a global run");
            self.engine.diagnostics().emit(reason.diagnostic());
            return Some(reason);
        }
        if ext.cause.is_some() {
            // Global already, for a reason of the stage's own that the dirty
            // stage will report.
            return None;
        }
        if ext.lock_read {
            tracing::debug!(
                path = %reg.path,
                "--registry moved with MODULE.bazel.lock, whose diff names the modules that moved"
            );
            return None;
        }
        let mode = match ext.mode {
            Some(m) => m,
            None => *ext
                .mode
                .insert(crate::externals::mode_at(self.base, texts, &self.req.base)),
        };
        if !mode.bzlmod_live() {
            tracing::info!(
                path = %reg.path,
                "--registry changed, but the base tree runs Bazel with bzlmod off: the edit is inert"
            );
            return None;
        }
        ext.escalate(ExternalsCause::RegistryWithoutLock {
            path: reg.path.clone(),
        });
        None
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
    ///
    /// `exempt` names changed files the bazelrc screen accounted for as
    /// ordinary source changes (the root rc under `--noworkspace_rc`), which
    /// the `.bazelrc` invalidator must therefore not fire on.
    pub(crate) fn dirty(
        &mut self,
        changes: &[&Change],
        exempt: &[String],
        ext: &mut Externals,
        texts: &dyn WorkspaceText,
    ) -> Result<DirtyStage> {
        let mut matchers = Matchers::compile(&self.engine.config().rules())?;
        matchers.exempt(ext.handled.clone());
        matchers.exempt(exempt.iter().cloned());
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
        // no soundness gained. Nor is a boundary the cold universe query
        // walked into a hole: its packages are this repository's, the
        // classifier owned the paths as main-repo paths on the way here, and
        // this loop has nothing left to say about them. What stays global is
        // a boundary Bazel reads, loads no main-repo package in, and no
        // repository maps: there the graph really does have a hole where the
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
                    // Phase 5 stage 3. When this index holds the
                    // repository's packages, the file is not only an input
                    // that moves the repository: it is a source file of one
                    // of its rules, under a label of its own
                    // (`@tsl//tsl/platform:x.cc`). The classifier could not
                    // name it -- `//...` stops at the boundary, so the main
                    // repository owns nothing here -- and it is named in the
                    // splice stage instead, where the builder's interner can
                    // resolve it (`external_labels`). Without this the
                    // repository's own rules are reached by nothing at all
                    // once rule (1) stops seeding every label of it.
                    if self
                        .base
                        .indexed_repos()
                        .iter()
                        .any(|r| r == self.base.repo_name(repo))
                    {
                        dirty
                            .direct_files
                            .extend(paths.iter().map(|p| p.to_string()));
                    }
                }
                // A boundary `//...` walked into anyway is already answered:
                // the classifier owned its paths as main-repo paths and they
                // moved the packages that hold them. There is no hole in the
                // graph to escalate over. The paths arrive here as well
                // because a traversed boundary is also the repository it
                // declares (xla's `third_party/tsl` is both), and the
                // repository half is the arm above; unmapped, it has nothing
                // left to say.
                None if self.base.boundary_is_traversed(dir) => {
                    tracing::debug!(
                        dir,
                        paths = paths.len(),
                        "a change inside a traversed boundary no repository maps stays a \
                         main-repo change"
                    );
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
        // A repository whose `.bzl` files may have moved moves every
        // repository defined through a load from it: xla's `@llvm-project`,
        // which a module extension defines by calling `llvm_configure` from
        // `@llvm-raw`. After every other route, so that a repository any of
        // them dirtied is followed; see `externals::defined_through_loads`.
        if !dirty.global && self.engine.config().externals.enabled {
            self.stats.repos_dirty_by_loader =
                crate::externals::defined_through_loads(self.base, ext, texts, &self.req.head);
        }

        // Every route that dirties a repository is in by now: the stage's
        // own, the nested checkouts above, and the workspace-level `.bzl`
        // files just read. A repository a registration pattern offers
        // toolchains from is the rule-class escalation seen from outside
        // `//...`, where the splice has no rows to read a class off, so it is
        // decided here, after the last route.
        //
        // Only while no cause has been put on the record yet, since a run
        // carries one. The missing layer and the unattributed workspace
        // `.bzl` raise their diagnostics as they decide, so this defers to
        // them; an invalidator's is raised below, once nothing has replaced
        // it, and this is one of the things that replaces it. Reads the
        // head's registrations only when there is a dirty repository to hold
        // them against.
        let reported = matches!(&global, Some(r) if !matches!(r, GlobalReason::Invalidator { .. }));
        if !reported {
            if let Some(cause) =
                crate::externals::registered_repository_moved(ext, texts, &self.req.head)
            {
                ext.escalate(cause);
            }
        }
        keep_surviving_packages(
            &mut dirty,
            self.querier,
            self.engine.workspace(),
            &confirmed_repos(self.base),
        );
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
        let mut settled = BTreeSet::new();
        let mut stale = StaleMaps::default();
        if !dirty.global {
            if !ext.repos.is_empty() {
                settled = settled_repos(self.base, ext, changes, &packages);
            }
            // Before `dirty_consumers` adds the consumers it dirties: a
            // consumer dirty because what it consumes moved has not had its
            // own rules move.
            stale = stale_leaf_maps(self.base, &ext.repos, &settled, &dirty.dirty);
        }
        let leaf_answers = ask_at_head(self.base, self.querier, &stale, &ext.repos);
        let round_trip = (!dirty.global && self.engine.config().externals.enabled)
            .then(|| crate::round_trip::at_head(self.base, self.querier, ext))
            .flatten();
        if !dirty.global && !ext.repos.is_empty() {
            self.stats.repos_settled = settled.len();
            let mut seeds = LeafSeeds::default();
            self.stats.repos_dirty_by_consumer = dirty_consumers(
                self.base,
                ext,
                &settled,
                &packages,
                &stale.suspect,
                &leaf_answers,
                &mut seeds,
            );
            let r = crate::externals::reach(
                self.base,
                &ext.repos,
                &ext.load_repos,
                &settled,
                &seeds,
            );
            self.stats.consumer_leaves_seeded = r.consumer_leaves;
            tracing::debug!(
                repos = ?ext.repos,
                load_repos = ?ext.load_repos,
                settled = ?settled,
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
            settled,
            stale_leaf_maps: stale.repos,
            leaf_answers,
            round_trip,
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
                    // This is the one route that runs after the stage filed
                    // its target-only repositories, and it contradicts that
                    // filing: the rule that writes the repository moved, so
                    // its `BUILD` files moved with it
                    // ([`Externals::target_only`]).
                    for r in &repos {
                        ext.target_only.remove(r);
                    }
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
            let mut rows = self.querier.query(&to_query)?;
            // Carrying a failed package's rows forward is right when
            // something else dirtied it: the rows are as old as the last
            // query that loaded it, and stale beats gone (issue #23). It is
            // the wrong direction when the load error is in the very `BUILD`
            // file this commit edited, because then the rows describe the
            // package before the edit and the targets the edit changed would
            // be reported unaffected. Those go back into the updates, where
            // the empty one tombstones them and their consumers go direct via
            // `DeletedDep` -- the wide answer, which is the safe one.
            let edited_here = build_files_edited(&dirty.direct_files);
            let (reapplied, carried): (Vec<_>, Vec<_>) = std::mem::take(&mut rows.unloaded)
                .into_iter()
                .partition(|u| edited_here.contains(&u.package));
            if !reapplied.is_empty() {
                tracing::warn!(
                    packages = reapplied.len(),
                    first = ?reapplied.iter().take(5).map(|u| &u.package).collect::<Vec<_>>(),
                    "these packages failed to load and their own BUILD file is what changed; \
                     tombstoning them rather than answering from rows that predate the edit"
                );
                self.engine.diagnostics.emit(
                    crate::diag::Diagnostic::warning(
                        crate::diag::Code::W005PackagesTombstoned,
                        format!(
                            "{} package(s) failed to load under --keep_going and their own \
                             BUILD file is what this change edited, so they were tombstoned \
                             rather than answered from rows that predate the edit; their \
                             consumers are reported as affected",
                            reapplied.len()
                        ),
                    )
                    .with_help(
                        "fix the load error bazel reported for those packages; \
                         run with --log warn to see its stderr",
                    ),
                );
            }
            rows.updates.extend(reapplied);
            rows.unloaded = carried;
            // A package that failed to load kept the rows the index already
            // had for it, so this run did not re-query it: it must not reach
            // the closure rescan or the loading-layer refresh as a package
            // whose answer moved, or they would drop what they hold for it
            // and undo the carry-forward one layer down.
            let to_query: Vec<String> = to_query
                .into_iter()
                .filter(|p| !rows.unloaded.iter().any(|u| &u.package == p))
                .collect();
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
    /// The repositories whose `Dir` claim was confirmed against Bazel when
    /// this lineage's index was built cold.
    ///
    /// The check is a `bazel query` per repository and belongs to the cold
    /// build, which runs it before the universe query (`crate::repos`). A
    /// splice carries the answer forward instead of re-running it: the
    /// declaration that makes the claim is a workspace-closure file, and a
    /// change to one of those is an invalidator, so a claim cannot move under
    /// a splice without the run being a cold build anyway.
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
        let mut boundaries = self
            .querier
            .workspace()
            .map(cambium_index::repo_boundaries)
            .unwrap_or_default();
        // Whether `//...` walks into a boundary is the universe query's
        // answer, and this run did not make one, so the base's answer is
        // carried forward for the directories it knows. A boundary this
        // commit added has no answer yet and keeps `false`, which is the safe
        // direction: its paths stay the repository's rather than becoming
        // main-repo paths of packages that may not exist.
        for b in &mut boundaries {
            b.traversed = self.base.boundary_is_traversed(&b.dir);
        }
        builder.set_repo_boundaries(boundaries.clone());
        // The consumer table Bazel answered on an overflow's re-query, and the
        // carried one's entries whose leaf maps went stale, to be given their
        // leaf maps once the splice has put the head's labels in.
        let mut rederived: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
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
                // The repositories the base index's universe query included.
                // A refresh has to ask for the same universe, or a reload
                // would materialise less than the cold build did.
                let indexed_repos = self.base.indexed_repos();
                match filter {
                    Some(filter) => {
                        layer = crate::sky::build_layer(
                            self.querier,
                            layer,
                            &filter,
                            Some(&requeried),
                            &indexed_repos,
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
                            &indexed_repos,
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
            // Who consumes each indexed repository is Bazel's answer, and
            // the base's copy is the answer at whatever commit last asked.
            // A run that re-queried `//...` is the one place to ask again
            // cheaply, on the server that just loaded the whole repository,
            // and the one place it is needed: a repository rename is a
            // workspace-closure change, which is exactly what overflows the
            // dirty limit. envoy's `com_github_grpc_grpc` became `grpc` at
            // one such step, every re-queried package then named its deps
            // `@grpc//...`, and the carried table kept seeding the leaves
            // of a repository no label spelled any more: one target whose
            // only route in was that proto went missing at the next
            // overflow, and a cold build at the same commit had it.
            // A repository Bazel cannot answer for keeps the carried entry:
            // a stale consumer over-reports, an absent one under-reports.
            //
            // The fresh answer is asked here and applied after the splice
            // below: its leaf map is keyed by the labels the index holds, and
            // after a rename those are the head's, which only exist once the
            // re-queried packages are in the builder. The closure rescan
            // carries the base's table meanwhile.
            let indexed = base.indexed_repos();
            let consumers = consumers_of(base);
            if queried.cold_reason.is_some() {
                rederived = self.querier.consumer_repos(&indexed);
                for (r, cs) in &rederived {
                    tracing::debug!(
                        repo = %r,
                        consumers = ?cs.keys().collect::<Vec<_>>(),
                        "the consumer table was re-derived on the re-queried server"
                    );
                }
            }
            // A leaf map something on its closure moved under
            // (`stale_leaf_maps`) is asked for again, over the carried
            // table's consumers and packages: the consumers and what they
            // pool are the overflow's to re-derive, the map is this one's.
            // Carried unasked, the map would stand as the answer for every
            // later run, and a quiche leaf given a path into `@envoy_api` by
            // a `BUILD` edit here would miss the edit there that follows.
            for r in &stage.stale_leaf_maps {
                if rederived.contains_key(r) {
                    continue;
                }
                if let Some(cs) = consumers.get(r) {
                    tracing::debug!(
                        repo = %r,
                        consumers = ?cs.keys().collect::<Vec<_>>(),
                        "the consumer leaf map is asked for again: something it was derived \
                         through moved"
                    );
                    rederived.insert(
                        r.clone(),
                        cs.iter().map(|(c, u)| (c.clone(), u.packages.clone())).collect(),
                    );
                }
            }
            if let Some(c) = bzl::rescan_closure(
                self.querier,
                builder.take_bzl_closure(),
                &scanned,
                &stage.changed_bzl,
                &gone,
                &crate::bzl::dirs(&boundaries),
                bzl::InTreeRepos {
                    confirmed: &confirmed_repos(base),
                    consumers: &consumers,
                    indexed: &indexed,
                },
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
        // The re-derived consumer table goes in over the carried one, with
        // each consumer's leaves asked for under the spelling the head's
        // labels have: envoy's rename to `grpc` is the case, where every
        // leaf the base held was `@com_github_grpc_grpc//...`. The same for
        // a stale leaf map, whose leaves a commit can have added to.
        if !rederived.is_empty() {
            if let Some(mut c) = builder.take_bzl_closure() {
                let fresh = crate::consumers::with_leaves(
                    self.querier,
                    std::mem::take(&mut rederived),
                    &crate::consumers::external_labels(&builder),
                    &c.inputs.canonical,
                    &stage.leaf_answers,
                );
                for (r, cs) in fresh {
                    c.inputs.consumers.insert(r, cs);
                }
                builder.set_bzl_closure(c);
            }
        }

        // The leaves' edges back into the universe: Bazel's answer at head
        // when a repository outside moved, and for an index this run writes,
        // the leaves an overflow's re-query or this splice's rows put in.
        // After the rows, so that the leaves are the head's.
        let want = match (self.req.materialize, queried.cold_reason.is_some()) {
            (false, _) => crate::round_trip::Want::Nothing,
            (true, true) => crate::round_trip::Want::All,
            (true, false) => crate::round_trip::Want::AddedSince(base.num_nodes()),
        };
        crate::round_trip::refresh(
            self.querier,
            &mut builder,
            &base.indexed_repos(),
            stage.round_trip.as_ref(),
            want,
        );

        // Resolve changed source files to node ids while the builder still has
        // its interner: rebuilding a label map over a finished index costs
        // O(labels), and at a few million labels that is the single most
        // expensive thing in an otherwise change-proportional run.
        let mut changed_file_ids: Vec<(u32, String)> = Vec::with_capacity(dirty.direct_files.len());
        // The directories of the in-tree repositories whose packages this
        // index holds. A file under one of them is a file of *two*
        // repositories at once and owes both labels; see `external_labels`.
        let indexed_dirs = indexed_repo_dirs(base);
        for path in &dirty.direct_files {
            let mut labels: Vec<String> = Vec::new();
            if let Some(pkg) = stage.packages.owner_of(path) {
                labels.push(cambium_index::label::source_label(pkg, path));
            }
            labels.extend(external_labels(&stage.packages, &indexed_dirs, path));
            if labels.is_empty() {
                continue;
            }
            for label in labels {
                match builder.label_id(&label) {
                    Some(id) => changed_file_ids.push((id, path.clone())),
                    // The most common "why wasn't my test run?" question: a
                    // file no rule references cannot affect anything.
                    None => tracing::debug!(%label, "changed file is not referenced by any rule"),
                }
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

    /// The toolchain check: did the splice add, change or tombstone a rule of
    /// one of the toolchain classes (`toolchain`, `platform`,
    /// `toolchain_type`, `constraint_setting`, `constraint_value`)?
    ///
    /// Toolchain resolution happens in analysis and has no edge in the query
    /// graph. A `toolchain()` that matches better than the one every
    /// `cc_library` resolved to yesterday changes what all of them build
    /// with, and nothing in the reverse walk reaches it: the rules that will
    /// use it do not name it, and the `register_toolchains` call that offers
    /// it is not an edge either. The two constraint classes are what a
    /// platform and a toolchain match through, and a setting's default or a
    /// value's parent moving changes that matching with no platform or
    /// toolchain proto moving. So one of these rules moving, in any package,
    /// is a whole-repository answer. The default invalidator globs
    /// (`tools/toolchains/**`, `platforms/**`) used to stand in for this, and
    /// were a naming convention rather than a rule: a `platform()` declared
    /// elsewhere never escalated, and a `filegroup` under `tools/toolchains/`
    /// always did.
    ///
    /// The five classes are native rules with no implementation of their
    /// own, so a `.bzl` edit can only move one through its proto (a macro
    /// that declares it), which the rule hash sees; a changed `.bzl` that
    /// leaves every hash alone moved no declaration and takes the ordinary
    /// route. What the hash cannot see is the implementation *behind* a
    /// declaration (a `cc_toolchain_config` whose `.bzl` changed): the walk
    /// reaches the `toolchain()` naming it through ordinary edges and stops
    /// there, which is the analysis-phase edge declined with the rest.
    ///
    /// Decided after the query, because it needs the head rows' classes: the
    /// one verdict a dry run (`crate::plan`) cannot reach. A declaration in
    /// an external repository is not a row of `//...` at all; that side is
    /// the dirty stage's, through the registration patterns
    /// (`crate::externals::registered_repository_moved`).
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
    ///
    /// A query that overflowed `max_dirty_packages` set `stats.cold_rebuild`
    /// on its way here, and that flag means "re-queried `//...` and spliced
    /// it, answer still exact": a global is neither, and its lineage is not
    /// kept. The flag is cleared so the report says one thing (`global`, with
    /// its cause) rather than two, and a reader counting cold rebuilds and
    /// globals counts this run once. The query's cost stays in
    /// `timings.query`, and the overflow diagnostic (`CAMBI-W012`) stays in
    /// the run's diagnostics, since it did happen.
    fn global_after_splice(mut self, reason: GlobalReason) -> RunOutcome {
        self.timings.splice = self.sw.lap();
        self.timings.total = self.timings.dirty + self.timings.query + self.timings.splice;
        self.stats.cold_rebuild = false;
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

/// The packages whose own `BUILD` file this change edited.
///
/// A modified BUILD file is a changed source path like any other, so the
/// classifier has it in `direct_files`; this reads the package names back out
/// of those paths. The root package is `""`, which is a real package name.
fn build_files_edited(direct_files: &BTreeSet<String>) -> BTreeSet<String> {
    direct_files
        .iter()
        .filter_map(|path| {
            let (dir, name) = match path.rsplit_once('/') {
                Some((dir, name)) => (dir, name),
                None => ("", path.as_str()),
            };
            matches!(name, "BUILD" | "BUILD.bazel").then(|| dir.to_string())
        })
        .collect()
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
fn keep_surviving_packages(
    dirty: &mut DirtySet,
    querier: &dyn PackageQuerier,
    fallback: &Path,
    confirmed: &std::collections::BTreeSet<(String, String)>,
) {
    if dirty.global || dirty.deleted_packages.is_empty() {
        return;
    }
    let ws = querier.workspace().unwrap_or(fallback);
    let survivors: Vec<String> = dirty
        .deleted_packages
        .iter()
        .filter(|p| {
            // An in-tree external repository's key is `@repo//pkg`, which is
            // not a path; its BUILD file is under the directory the
            // repository is. Joining the key itself would name nothing and
            // read every surviving external package as deleted.
            let dir = ws.join(crate::bzl::package_dir(p, confirmed));
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

/// The directories of the repositories this index holds the packages of, as
/// `(repo, dir)` with the directory's trailing `/`.
///
/// Decided by the rows, not by the flags. Only a confirmed `Dir` claim is
/// ever widened by, so a package key under `@repo//` is proof the probe
/// confirmed the directory at build time, whatever `RepoOrigin::confirmed`
/// and `indexed` came to say afterwards; and `externals::reach` rule (0)
/// already decides by the same rows. The two were keyed differently once:
/// every envoy base before the bzlmod migration held all 534 `@envoy_api`
/// packages and read both flags false (the confirmation had no candidate to
/// land on in Bazel's mode), so rule (0) re-queried the packages while the
/// join here found no directory, and a modified `.proto` under `api/`
/// reached nothing by its label. Rules that agree on their evidence cannot
/// come apart like that.
///
/// The directory is the origin's, which is the one the candidate carried
/// and the probe checked (`RepoInputs::confirm_dir`); an origin with no
/// directory has nothing to join a path under.
fn indexed_repo_dirs(base: IndexRef<'_>) -> Vec<(String, String)> {
    let mut with_rows: BTreeSet<&str> = BTreeSet::new();
    for p in 0..base.num_packages() {
        if base.package_tombstoned(p) {
            continue;
        }
        if let Some(repo) = cambium_index::label::key_repo(base.package_name(p)) {
            with_rows.insert(repo);
        }
    }
    let indexed = base.indexed_repos();
    (0..base.num_repos())
        .filter_map(|r| {
            let name = base.repo_name(r);
            let o = base.repo_origin(r);
            let dir = o.dir.trim_end_matches('/');
            let held = o.indexed || indexed.iter().any(|i| i == name) || with_rows.contains(name);
            (o.kind == cambium_index::externals::OriginKind::Dir && !dir.is_empty() && held)
                .then(|| (name.to_string(), format!("{dir}/")))
        })
        .collect()
}

/// The external-consumer table of an index, as `RepoInputs::consumers`
/// holds it, leaf maps and all, for the splice to carry forward.
fn consumers_of(
    base: IndexRef<'_>,
) -> BTreeMap<String, BTreeMap<String, cambium_index::externals::Consumer>> {
    (0..base.num_repos())
        .filter_map(|r| {
            let cs = base.repo_consumers(r);
            (!cs.is_empty()).then(|| (base.repo_name(r).to_string(), cs.into_iter().collect()))
        })
        .collect()
}

/// Dirties the external repositories that consume a dirty indexed one, or
/// the leaves of them the change can have moved, and says how many
/// consumers it reached.
///
/// The return path. `//app:test` links `@grpc//:grpc`; grpc's xDS client is
/// built from `@envoy_api` protos; a `.proto` under `api/` therefore moves
/// the test. The index holds the first edge as a leaf with no outgoing edges
/// and the second not at all, so the reverse walk from the proto ends at
/// `@envoy_api`. The table the cold build asked Bazel for
/// (`RepoInputs::consumers`) names `grpc` as a consumer of `envoy_api` and
/// the API packages it consumes. Box run C measured 510 labels missing on
/// one such step with recall 0.21; Bazel's own `rdeps` for the proto is
/// exactly the consumers of `@grpc//:grpc`.
///
/// Which packages of the indexed repository moved: for a settled repository
/// (`settled_repos`), the packages of the changed paths under its directory,
/// by the same join the splice uses; for one that is not settled (a `BUILD`
/// or `.bzl` moved, or a `MODULE.bazel` edit named it), every package, since
/// what its rules will look like is not known until they are re-queried.
///
/// What a consumer that moved gets depends on its leaf map
/// (`Consumer::leaves`):
///
/// - no map, or one [`stale_leaf_maps`] found suspect this run that Bazel
///   would not answer for at head ([`ask_at_head`]): the consumer is marked
///   dirty when the packages it consumes, pooled, meet what moved, and
///   `externals::reach` rule (1) seeds every leaf of it, the answer any
///   dirty repository nothing indexes gets;
/// - a map (for a suspect one, each leaf reaching what it reaches in the map
///   or at head): only the leaves whose recorded packages meet what moved go to
///   rule (1) as [`LeafSeeds`], with the leaves the map does not name
///   getting the pooled answer above. The consumer itself stays clean. For a
///   settled repository what moved is widened first by the repository's own
///   reverse edges in this index ([`reverse_within`]). For one that is not
///   settled, every leaf that reaches anything of it is seeded, and every
///   leaf at all when a path that unsettled it is Starlark or outside its
///   directory: a `.bzl` of the repository that the consumer's `BUILD` files
///   load can change the consumer's rules, including one that reached
///   nothing of the repository before.
fn dirty_consumers(
    base: IndexRef<'_>,
    ext: &mut Externals,
    settled: &BTreeSet<String>,
    packages: &PackageSet,
    suspect: &BTreeMap<String, BTreeSet<String>>,
    at_head: &crate::consumers::Asked,
    seeds: &mut LeafSeeds,
) -> usize {
    let indexed_dirs = indexed_repo_dirs(base);
    let mut added = 0;
    let mut narrowed: BTreeSet<String> = BTreeSet::new();
    for r in 0..base.num_repos() {
        let name = base.repo_name(r);
        if !ext.repos.contains(name) {
            continue;
        }
        let consumers = base.repo_consumers(r);
        if consumers.is_empty() {
            continue;
        }
        let paths: Vec<String> = ext.dirtied_by.get(name).cloned().unwrap_or_default();
        let one: Vec<(String, String)> = indexed_dirs
            .iter()
            .filter(|(rr, _)| rr == name)
            .cloned()
            .collect();
        // The labels the change touched inside the repository, and their
        // packages by their part inside it; `None` means every package.
        let touched_labels: Option<Vec<String>> = settled.contains(name).then(|| {
            paths
                .iter()
                .flat_map(|p| external_labels(packages, &one, p))
                .collect()
        });
        let touched: Option<BTreeSet<String>> = touched_labels.as_ref().map(|ls| {
            ls.iter()
                .filter_map(|l| {
                    cambium_index::label::Label::parse(l).map(|l| l.package().to_string())
                })
                .collect()
        });
        // Not settled, and not by `BUILD` files and sources under the
        // repository's own directory alone.
        let starlark = touched.is_none()
            && (paths.is_empty()
                || paths.iter().any(|p| {
                    cambium_classify::is_bzl_file(p)
                        || !one.iter().any(|(_, dir)| p.starts_with(dir.as_str()))
                }));
        let no_suspects = BTreeSet::new();
        let suspects = suspect.get(name).unwrap_or(&no_suspects);
        // Widened once per repository, and only if some consumer of it has a
        // map to hold it against.
        let mut widened: Option<Option<BTreeSet<String>>> = None;
        for (consumer, used) in consumers {
            let pooled = match &touched {
                Some(t) => t.iter().any(|p| used.packages.contains(p)),
                None => true,
            };
            // A suspect map is read against Bazel's answer at head, each
            // leaf reaching what it reached at either end: a package it
            // reaches only at head is one the change can have moved it
            // through, and one it reached only at base is one whose move it
            // can have been cut from. With no answer at head, no map.
            let merged: Option<BTreeMap<String, BTreeSet<String>>> = if suspects.contains(&consumer) {
                match (&used.leaves, at_head.get(name).and_then(|(_, a)| a.as_ref())) {
                    (Some(base_map), Some(head)) => {
                        let canonical = (0..base.num_repos())
                            .find(|&r| base.repo_name(r) == consumer)
                            .map_or("", |r| base.repo_canonical.get(r));
                        let mut m = base_map.clone();
                        for (leaf, reach) in head {
                            if crate::consumers::is_leaf_of(leaf, &consumer, canonical) {
                                m.entry(leaf.clone()).or_default().extend(reach.packages.iter().cloned());
                            }
                        }
                        Some(m)
                    }
                    _ => None,
                }
            } else {
                used.leaves.clone()
            };
            let Some(leaves) = merged.as_ref() else {
                if !pooled {
                    tracing::debug!(
                        repo = %name,
                        %consumer,
                        "a consumer of a settled repository depends on none of the packages that moved"
                    );
                    continue;
                }
                if ext.repos.insert(consumer.clone()) {
                    added += 1;
                    tracing::debug!(
                        repo = %name,
                        %consumer,
                        suspect = suspects.contains(&consumer),
                        "an external repository that consumes a moved indexed repository is dirty"
                    );
                }
                for p in &paths {
                    ext.name(p, std::iter::once(&consumer));
                }
                continue;
            };
            let moved: &Option<BTreeSet<String>> = widened.get_or_insert_with(|| {
                touched
                    .as_ref()
                    .zip(touched_labels.as_ref())
                    .map(|(t, ls)| reverse_within(base, name, t, ls))
            });
            let seeded: BTreeSet<String> = leaves
                .iter()
                .filter(|(_, reach)| match moved {
                    Some(m) => reach.iter().any(|p| m.contains(p)),
                    None => starlark || !reach.is_empty(),
                })
                .map(|(l, _)| l.clone())
                .collect();
            // A leaf the map does not know gets the pooled answer, and only
            // counts if there is one.
            let unknown = pooled && has_unknown_leaf(base, &consumer, leaves);
            tracing::debug!(
                repo = %name,
                %consumer,
                known = leaves.len(),
                seeded = seeded.len(),
                unknown,
                starlark,
                first = ?seeded.iter().take(5).collect::<Vec<_>>(),
                "a consumer of a moved indexed repository is seeded by its leaf map"
            );
            if !seeded.is_empty() || unknown {
                if !ext.repos.contains(&consumer) && narrowed.insert(consumer.clone()) {
                    added += 1;
                }
                for p in &paths {
                    ext.name(p, std::iter::once(&consumer));
                }
            }
            seeds
                .by_consumer
                .entry(consumer)
                .or_default()
                .push(crate::externals::MapSeeds {
                    known: leaves.keys().cloned().collect(),
                    seeded,
                    unknown,
                });
        }
    }
    added
}

/// Asks Bazel at head for the leaf maps of the indexed repositories that
/// moved in this run and have a consumer whose map [`stale_leaf_maps`] found
/// suspect: one question per repository, over every leaf of its consumers
/// the base holds.
///
/// Without it the suspect consumer is answered from its pooled packages,
/// every leaf seeded: sound, and on envoy the quiche consumer's 125 leaves
/// behind any `api/` edit that comes with a `BUILD` edit on the main-repo
/// closure quiche reaches the API through. The querier answers at head (it
/// is the one the dirty packages are re-queried with), and the splice reuses
/// the answer for its own re-derivation when the leaves are the same.
fn ask_at_head(
    base: IndexRef<'_>,
    q: &dyn PackageQuerier,
    stale: &StaleMaps,
    moved: &BTreeSet<String>,
) -> crate::consumers::Asked {
    let mut out = crate::consumers::Asked::new();
    for r in 0..base.num_repos() {
        let name = base.repo_name(r);
        if !moved.contains(name) || !stale.suspect.contains_key(name) {
            continue;
        }
        let consumers: Vec<(String, &str)> = base
            .repo_consumers(r)
            .into_iter()
            .map(|(c, _)| {
                let canonical = (0..base.num_repos())
                    .find(|&i| base.repo_name(i) == c)
                    .map_or("", |i| base.repo_canonical.get(i));
                (c, canonical)
            })
            .collect();
        let start = base
            .label_sorted
            .partition_point(|&id| base.label(id) < "@");
        let leaves: Vec<String> = base.label_sorted[start..]
            .iter()
            .filter(|&&id| !base.is_tombstone(id))
            .map(|&id| base.label(id))
            .take_while(|l| l.starts_with('@'))
            .filter(|l| {
                consumers
                    .iter()
                    .any(|(c, canonical)| crate::consumers::is_leaf_of(l, c, canonical))
            })
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let started = std::time::Instant::now();
        let answer = q.consumer_leaf_packages(name, &leaves);
        tracing::debug!(
            repo = %name,
            leaves = leaves.len(),
            answered = answer.as_ref().map(BTreeMap::len),
            ms = started.elapsed().as_millis() as u64,
            "a suspect consumer leaf map was asked for at head"
        );
        out.insert(name.to_string(), (leaves, answer));
    }
    out
}

/// Whether the index holds a label of `consumer` that its leaf map does not
/// name: one spelled after the map was derived, or one Bazel could not
/// resolve when it was.
fn has_unknown_leaf(
    base: IndexRef<'_>,
    consumer: &str,
    known: &BTreeMap<String, BTreeSet<String>>,
) -> bool {
    let canonical = (0..base.num_repos())
        .find(|&r| base.repo_name(r) == consumer)
        .map_or("", |r| base.repo_canonical.get(r));
    let start = base
        .label_sorted
        .partition_point(|&id| base.label(id) < "@");
    base.label_sorted[start..]
        .iter()
        .filter(|&&id| !base.is_tombstone(id))
        .map(|&id| base.label(id))
        .take_while(|l| l.starts_with('@'))
        .any(|l| crate::consumers::is_leaf_of(l, consumer, canonical) && !known.contains_key(l))
}

/// The leaf maps a run cannot trust, found before any consumer is dirtied.
#[derive(Default, Debug)]
struct StaleMaps {
    /// Indexed repositories whose consumers' maps the splice asks for again.
    repos: BTreeSet<String>,
    /// Indexed repository -> the consumers whose map this run reads as no
    /// map at all.
    suspect: BTreeMap<String, BTreeSet<String>>,
}

/// Which consumers' leaf maps may no longer be Bazel's answer at head.
///
/// A map is the closure of each leaf as Bazel answered it when it was
/// asked, and it stays right only while nothing on that closure moves. Two
/// things can move it without an overflow, which is the one place the whole
/// table is asked for again:
///
/// - something the map was derived through (`Consumer::via`): a main-repo
///   package in `main_dirty` (a `BUILD` edit, or a `.bzl` it loads), or
///   another repository in `moved`, the consumer's own definition included.
///   envoy's quiche leaves reach `@envoy_api` only through
///   `@envoy//source/common/quic/platform:*`; a `BUILD` edit on that closure
///   can give a leaf a path into any package of the API, and a later edit
///   there would then miss the leaf's consumers. The consumer is suspect:
///   the edit that moved the closure can come in the same run as one to
///   what it now reaches, so when the indexed repository moved too this run
///   asks Bazel at head ([`ask_at_head`]) rather than trust the map;
/// - the indexed repository itself, when it moved and is not settled: its
///   own rules changed, so a path through them may have, including one that
///   leaves it for the main repository and comes back. This run's seeds
///   stand on the old map ([`dirty_consumers`] says why that is enough for
///   the change that unsettled it), but the next run's would not.
///
/// Either way the repository is named for the splice to ask again.
fn stale_leaf_maps(
    base: IndexRef<'_>,
    moved: &BTreeSet<String>,
    settled: &BTreeSet<String>,
    main_dirty: &BTreeSet<String>,
) -> StaleMaps {
    let mut out = StaleMaps::default();
    let canonical: BTreeMap<&str, &str> = (0..base.num_repos())
        .map(|r| (base.repo_name(r), base.repo_canonical.get(r)))
        .collect();
    for r in 0..base.num_repos() {
        let name = base.repo_name(r);
        let consumers = base.repo_consumers(r);
        if consumers.iter().all(|(_, c)| c.leaves.is_none()) {
            continue;
        }
        if moved.contains(name) && !settled.contains(name) {
            out.repos.insert(name.to_string());
        }
        for (consumer, used) in consumers {
            if used.leaves.is_none() {
                continue;
            }
            let hit = used.via.iter().find(|v| match v.strip_prefix("//") {
                Some(pkg) => main_dirty.contains(pkg),
                None => v.strip_prefix('@').is_some_and(|token| {
                    moved.iter().any(|m| {
                        m != name
                            && crate::externals::repo_token_matches(
                                token,
                                m,
                                canonical.get(m.as_str()).copied().unwrap_or(""),
                            )
                    })
                }),
            });
            if let Some(v) = hit {
                tracing::debug!(
                    repo = %name,
                    %consumer,
                    through = %v,
                    "a consumer's leaf map was derived through something that moved; \
                     not trusted as it stands, and asked for again"
                );
                out.repos.insert(name.to_string());
                out.suspect.entry(name.to_string()).or_default().insert(consumer);
            }
        }
    }
    out
}

/// The packages of the indexed repository `repo` whose nodes reach one of
/// the changed files `labels` inside the repository, in this index's own
/// reverse edges, the files' packages `touched` included.
///
/// A leaf map Bazel answered is transitive, so a leaf that reaches a package
/// depending on a changed file reaches the file's package too, and this adds
/// nothing to it. It is there for a map that is not the closure the index
/// holds (an edge inside the repository the map was derived without, or a
/// closure a `--keep_going` load error cut short): the reverse rows are
/// re-queried whenever the repository's rules move, so they say what the
/// map may not. Only the repository's own nodes are walked: the walk is
/// about the part of the path inside it.
///
/// The walk starts from the changed files, not from every node of their
/// packages: on envoy an edit to `api/bazel/deps.yaml` started from every
/// rule of `@envoy_api//bazel`, which most of the API depends on, and seeded
/// 79 quiche leaves for a change whose oracle was 22 labels. A label the
/// index holds no node for starts from its whole package instead.
fn reverse_within(
    base: IndexRef<'_>,
    repo: &str,
    touched: &BTreeSet<String>,
    labels: &[String],
) -> BTreeSet<String> {
    let canonical = (0..base.num_repos())
        .find(|&r| base.repo_name(r) == repo)
        .map_or("", |r| base.repo_canonical.get(r));
    let inside = |key: &str| {
        cambium_index::label::key_repo(key)
            .is_some_and(|t| crate::externals::repo_token_matches(t, repo, canonical))
    };
    let mut out = touched.clone();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut stack: Vec<u32> = Vec::new();
    // Packages a changed file has no node in, walked from whole.
    let mut whole: BTreeSet<String> = BTreeSet::new();
    for l in labels {
        match base.id_of(l).filter(|&id| !base.is_tombstone(id)) {
            Some(id) => {
                if seen.insert(id) {
                    stack.push(id);
                }
            }
            None => {
                if let Some(p) = cambium_index::label::Label::parse(l) {
                    whole.insert(p.package().to_string());
                }
            }
        }
    }
    if !whole.is_empty() {
        for p in 0..base.num_packages() {
            if base.package_tombstoned(p) {
                continue;
            }
            let key = base.package_name(p);
            if inside(key) && whole.contains(cambium_index::label::key_package(key)) {
                for &n in base.package_nodes(p) {
                    if seen.insert(n) {
                        stack.push(n);
                    }
                }
            }
        }
    }
    while let Some(n) = stack.pop() {
        for &m in base.reverse.row(n) {
            if seen.contains(&m) || base.is_tombstone(m) {
                continue;
            }
            let Some(key) = base.package_of(m).filter(|k| inside(k)) else {
                continue;
            };
            seen.insert(m);
            out.insert(cambium_index::label::key_package(key).to_string());
            stack.push(m);
        }
    }
    out
}

/// The labels a changed path has inside the in-tree repositories whose
/// packages `base` holds: `external_labels` over `indexed_repo_dirs`, for a
/// caller outside the run (the benchmark's `rdeps` oracle roots a changed
/// path by every label it has, or it cannot see what a `.proto` under
/// envoy's `api/` reaches).
pub fn in_tree_labels(base: IndexRef<'_>, packages: &PackageSet, path: &str) -> Vec<String> {
    external_labels(packages, &indexed_repo_dirs(base), path)
}

/// The dirty repositories whose rules cannot have moved: every path that
/// dirtied them is a modified source file of a package this index holds.
///
/// A main-repo source edit re-queries nothing: the file's label is a node,
/// the rules built from it are its consumers, and the reverse walk finds them.
/// An in-tree repository whose packages the universe was widened by has the
/// same nodes and the same edges (`external_labels` joins the changed path to
/// its label there), so the same edit deserves the same answer. What it got
/// instead was `externals::reach` rule (0), every package of the repository
/// re-queried, which on envoy is `@envoy_api`'s 534 packages for one
/// `.proto` edit, over `limits.max_dirty_packages` and into a cold rebuild.
///
/// Settled is decided per repository and only from evidence that nothing
/// else could have moved:
///
/// - the repository is confirmed at a main-repo directory and indexed, so the
///   changed path has a label in it to reach anything by;
/// - every path the externals stage recorded against it lies under that
///   directory (a `MODULE.bazel` edit, or an input path a repository rule
///   reads from elsewhere, names the repository too, and keeps it wide);
/// - every path is `Modified` or `TypeChanged`: an added or deleted file can
///   enter or leave a `glob`, which is a rule change without a `BUILD` edit;
/// - no path is a `BUILD` file or a `.bzl`: those define rules, and rule (0)
///   is exactly how a changed definition is answered;
/// - every path resolves to a package this index holds, so the label the
///   splice joins it to exists. A file no rule references has no consumers,
///   which is the main-repo reading as well.
///
/// A `.bzl` inside the repository keeps the whole repository re-queried,
/// because the stored load closure stops at external labels
/// (`cambium_index::loads`) and cannot say which of its packages load it.
fn settled_repos(
    base: IndexRef<'_>,
    ext: &Externals,
    changes: &[&Change],
    packages: &PackageSet,
) -> BTreeSet<String> {
    let status: std::collections::HashMap<&str, cambium_diff::Status> = changes
        .iter()
        .map(|c| (c.path.as_str(), c.status))
        .collect();
    let content_edit = |path: &str| {
        matches!(
            status.get(path),
            Some(cambium_diff::Status::Modified | cambium_diff::Status::TypeChanged)
        ) && !cambium_classify::is_build_file(path)
            && !cambium_classify::is_bzl_file(path)
    };
    let indexed_dirs = indexed_repo_dirs(base);
    let mut out = BTreeSet::new();
    for repo in &ext.repos {
        let Some((_, dir)) = indexed_dirs.iter().find(|(r, _)| r == repo) else {
            continue;
        };
        let Some(paths) = ext.dirtied_by.get(repo) else {
            continue;
        };
        if paths.is_empty() {
            continue;
        }
        let one = [(repo.clone(), dir.clone())];
        let settled = paths.iter().all(|p| {
            p.starts_with(dir.as_str())
                && content_edit(p)
                && !external_labels(packages, &one, p).is_empty()
        });
        if settled {
            tracing::debug!(
                %repo,
                paths = paths.len(),
                "every change is a modified source file of an indexed package; \
                 the repository's rules stand and its files reach their consumers by label"
            );
            out.insert(repo.clone());
        }
    }
    out
}

/// The labels a changed path has inside the in-tree repositories it is also a
/// file of.
///
/// xla's `third_party/tsl/tsl/platform/x.cc` is a file of the main repository
/// *and* of `@tsl`, where it is `@tsl//tsl/platform:x.cc`, and the rules that
/// consume it there are reached by that label alone. Before the universe was
/// widened those rules were leaf nodes and the repository's own dirtiness
/// stood in for them; now they are real nodes with real edges, and the edge
/// is found the same way a main-repo source file's is.
///
/// The package is the longest prefix of the repo-relative path that this
/// index holds a package for, which is `PackageSet::owner_of`'s walk against
/// the external keys.
fn external_labels(
    packages: &PackageSet,
    indexed_dirs: &[(String, String)],
    path: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for (repo, dir) in indexed_dirs {
        let Some(rel) = path.strip_prefix(dir.as_str()) else {
            continue;
        };
        let mut end = rel.rfind('/');
        loop {
            let inner = end.map_or("", |i| &rel[..i]);
            let key = cambium_index::label::external_package_key(repo, inner);
            if packages.contains(&key) {
                // The name is what follows the package; +1 skips the '/'
                // separator, and the repository's root package takes the
                // whole repo-relative path.
                let name = if inner.is_empty() {
                    rel
                } else {
                    &rel[inner.len() + 1..]
                };
                out.push(cambium_index::label::key_label(&key, name));
                break;
            }
            let Some(i) = end else { break };
            end = rel[..i].rfind('/');
        }
    }
    out
}

fn confirmed_repos(base: IndexRef<'_>) -> std::collections::BTreeSet<(String, String)> {
    (0..base.num_repos())
        .filter_map(|r| {
            let o = base.repo_origin(r);
            (o.confirmed && o.kind == cambium_index::externals::OriginKind::Dir).then(|| {
                (
                    base.repo_name(r).to_string(),
                    o.dir.trim_end_matches('/').to_string(),
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod stale_tests {
    use super::stale_leaf_maps;
    use cambium_index::externals::Consumer;
    use cambium_index::{new_lineage, BzlClosure, IndexBuilder, IndexData};
    use std::collections::{BTreeMap, BTreeSet};

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    /// `envoy_api` consumed by `grpc`, whose map was derived through
    /// `@grpc` itself and `@abseil-cpp`, and by `quiche`, whose map was
    /// derived through a main-repo package and `@quiche`; and `tsl`,
    /// consumed by `xla` with no map at all.
    fn index() -> IndexData {
        let mut c = BzlClosure::default();
        c.inputs.repos = set(&["envoy_api", "tsl"]);
        let leaf = |l: &str| -> Option<BTreeMap<String, BTreeSet<String>>> {
            Some([(l.to_string(), set(&["envoy/type/v3"]))].into_iter().collect())
        };
        let grpc = Consumer {
            packages: set(&["envoy/type/v3"]),
            leaves: leaf("@grpc//:grpc++"),
            via: set(&["@grpc", "@abseil-cpp"]),
        };
        let quiche = Consumer {
            packages: set(&["envoy/type/v3"]),
            leaves: leaf("@quiche//:quic_core"),
            via: set(&["//source/common/quic/platform", "//", "@quiche"]),
        };
        c.inputs.consumers.insert(
            "envoy_api".into(),
            [("grpc".to_string(), grpc), ("quiche".to_string(), quiche)].into_iter().collect(),
        );
        c.inputs.consumers.insert(
            "tsl".into(),
            [("xla".to_string(), Consumer::of_packages(set(&["tsl/platform"])))]
                .into_iter()
                .collect(),
        );
        let mut b = IndexBuilder::new(new_lineage());
        b.set_bzl_closure(c);
        b.finish("c0")
    }

    #[test]
    fn a_main_repo_package_a_map_was_derived_through_makes_it_suspect() {
        let d = index();
        let s = stale_leaf_maps(d.view(), &set(&[]), &set(&[]), &set(&["source/common/quic/platform"]));
        assert_eq!(s.repos, set(&["envoy_api"]));
        assert_eq!(s.suspect["envoy_api"], set(&["quiche"]));
        // The root package is spelled `//`, and is the empty package.
        let s = stale_leaf_maps(d.view(), &set(&[]), &set(&[]), &set(&[""]));
        assert_eq!(s.suspect["envoy_api"], set(&["quiche"]));
        // A package no map was derived through moves none.
        let s = stale_leaf_maps(d.view(), &set(&[]), &set(&[]), &set(&["source/server"]));
        assert!(s.repos.is_empty() && s.suspect.is_empty(), "{s:?}");
    }

    /// The consumer's own definition moving (a version bump of `@grpc`) is
    /// a repository it was derived through, in either spelling Bazel printed.
    #[test]
    fn a_repository_a_map_was_derived_through_moving_makes_it_suspect() {
        let d = index();
        let s = stale_leaf_maps(d.view(), &set(&["grpc"]), &set(&[]), &set(&[]));
        assert_eq!(s.suspect["envoy_api"], set(&["grpc"]));
        let s = stale_leaf_maps(d.view(), &set(&["abseil-cpp"]), &set(&[]), &set(&[]));
        assert_eq!(s.suspect["envoy_api"], set(&["grpc"]));
    }

    /// The indexed repository moving and not settled names it for the splice
    /// without making any consumer suspect; settled, it names nothing. A
    /// repository whose consumers have no map is never named: there is
    /// nothing to ask again.
    #[test]
    fn an_unsettled_indexed_repository_is_asked_for_again() {
        let d = index();
        let s = stale_leaf_maps(d.view(), &set(&["envoy_api", "tsl"]), &set(&[]), &set(&[]));
        assert_eq!(s.repos, set(&["envoy_api"]));
        assert!(s.suspect.is_empty(), "{s:?}");
        let s = stale_leaf_maps(d.view(), &set(&["envoy_api"]), &set(&["envoy_api"]), &set(&[]));
        assert!(s.repos.is_empty(), "{s:?}");
    }
}
