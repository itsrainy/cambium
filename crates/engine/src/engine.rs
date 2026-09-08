//! The run: load a baseline, work out what is dirty, re-query it, splice, walk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cambium_bazel::Bazel;
use cambium_classify::{DirtySet, Matchers, PackageSet};
use cambium_diff::{Change, Repo};
use cambium_index::{
    walk, GraphView, IndexBuilder, IndexData, IndexFile, IndexRef, SpliceReport, WalkOptions,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::baseline::BaselineCache;
use crate::bazelrc;
use crate::config::Config;
use crate::error::{EngineError, Result};
use crate::externals::Externals;
use crate::query::{BazelQuerier, FileSource, PackageQuerier};
use crate::report::{AffectedReport, DirectReason, Explanation, Stats, Stopwatch, Timings};
use crate::revision::{worktrees_dir, RevisionQuerier, RevisionWorkspace};
use crate::store::{self, ArtifactStore};

/// What the caller asked for.
#[derive(Clone, Debug)]
pub struct AffectedRequest {
    pub base: String,
    pub head: String,
    pub include_manual: bool,
    pub explain: bool,
    /// Refuse to fall back to a cold full build when no baseline is found.
    pub no_full_build: bool,
    /// Produce a complete head index rather than a delta overlay.
    ///
    /// `affected` does not need one — it answers a question and throws the
    /// graph away — and materialising is the most expensive step in a run, so
    /// this is off by default. `baseline update` sets it, because its output is
    /// what gets published.
    pub materialize: bool,
    /// Publish the *base* index when this run had to compute it — because the
    /// store had no index for `base` and one was replayed forward from an
    /// ancestor, or built cold. The head-side index is never published from
    /// `affected`; only `baseline update` publishes that.
    pub publish_base: bool,
    /// Let `publish_base` publish an index Bazel could not load all of.
    ///
    /// A partial index answers `affected` correctly about the packages it has
    /// and knows nothing about the rest, and once it is in the store every
    /// later run splices onto it. So a run publishes one only when told to,
    /// here or by `baseline.publish_partial` in the config; otherwise the
    /// index is kept locally and the publish is skipped with a warning.
    pub allow_partial: bool,
    /// Stop the reverse walk after this many hops from the direct set.
    /// `None` reports the whole closure.
    pub max_depth: Option<u32>,
}

impl AffectedRequest {
    pub fn new(base: impl Into<String>, head: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            head: head.into(),
            include_manual: false,
            explain: false,
            no_full_build: false,
            materialize: false,
            publish_base: false,
            allow_partial: false,
            max_depth: None,
        }
    }
}

/// A base index, however it was obtained.
///
/// A fetched index stays mmapped rather than being deserialised into owned
/// memory: the `affected` path only ever reads it, and copying a few hundred
/// megabytes to read it would undo what the delta overlay bought.
pub enum Baseline {
    Mapped(IndexFile),
    Owned(Box<IndexData>),
}

impl Baseline {
    pub fn view(&self) -> IndexRef<'_> {
        match self {
            Baseline::Mapped(f) => f.view(),
            Baseline::Owned(d) => d.view(),
        }
    }

    /// The owned index, deserialising the mapping only if that is what the
    /// caller actually needs.
    pub fn into_owned(self) -> Result<IndexData> {
        match self {
            Baseline::Mapped(f) => Ok(f.deserialize()?),
            Baseline::Owned(d) => Ok(*d),
        }
    }
}

impl std::fmt::Debug for Baseline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.view();
        f.debug_struct("Baseline")
            .field("commit", &v.commit)
            .field("nodes", &v.num_nodes())
            .finish()
    }
}

/// How a run came by its base index. Only a computed one is worth publishing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaselineOrigin {
    /// Read from the artifact store; already published by definition.
    Fetched,
    /// No index for `base`, so an ancestor's was replayed forward to it.
    Replayed { from: String },
    /// No usable ancestor either, so it was built from scratch.
    ColdBuild,
}

/// A diff with the bazelrc changes it can prove irrelevant removed.
struct ScreenedDiff<'a> {
    changes: Vec<&'a Change>,
    /// Bazelrc files whose edit was classified safe.
    safe: usize,
    /// Set when a bazelrc edit invalidates the whole run.
    global_reason: Option<String>,
}

/// A completed run: the answer, plus the head index it produced.
pub struct RunOutcome {
    pub report: AffectedReport,
    /// Index at head. Present unless the run was globally invalidated before a
    /// splice happened.
    pub index: Option<IndexData>,
}

pub struct Engine {
    pub cfg: Config,
    pub repo: Repo,
    pub bazel: Bazel,
    pub store: Option<Box<dyn ArtifactStore>>,
    cache_dir: PathBuf,
}

/// Where fetched indexes, revision worktrees and publish scratch files go
/// when neither the config nor the caller says.
pub fn default_cache_dir() -> PathBuf {
    std::env::temp_dir().join("cambium-cache")
}

impl Engine {
    /// Opens an engine rooted at the Bazel workspace containing `start`.
    pub fn open(
        start: &Path,
        config_path: Option<&Path>,
        bazel_binary: Option<&str>,
    ) -> Result<Self> {
        let mut bazel = Bazel::discover(start, bazel_binary)?;
        let workspace = bazel.workspace().to_path_buf();
        let cfg = match config_path {
            Some(p) => Config::load_from(p)?,
            None => Config::load(&workspace)?,
        };
        if bazel_binary.is_none() && !cfg.bazel.binary.is_empty() {
            bazel = Bazel::at(workspace.clone(), Some(&cfg.bazel.binary));
        }
        let bazel = bazel
            .with_extra_query_flags(cfg.bazel.extra_query_flags.clone())
            .with_startup_flags(cfg.bazel.startup_flags.clone());
        let repo = Repo::discover(&workspace).unwrap_or_else(|_| Repo::new(workspace.clone()));
        Ok(Self::with_parts(cfg, repo, bazel))
    }

    /// Engine over an already-known workspace.
    pub fn with_parts(cfg: Config, repo: Repo, bazel: Bazel) -> Self {
        let store = store::from_config(&cfg.baseline.store);
        let cache_dir = cfg
            .baseline
            .cache_dir
            .clone()
            .unwrap_or_else(default_cache_dir);
        Self {
            cfg,
            repo,
            bazel,
            store,
            cache_dir,
        }
    }

    /// Overrides the cache directory (`--cache-dir` on the command line).
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    pub fn workspace(&self) -> &Path {
        self.bazel.workspace()
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Reads BUILD files from `root`, falling back to `git show` at `rev` for
    /// anything the tree does not have (a `.gitignore`d generated BUILD file,
    /// or a shallow checkout).
    fn file_source(&self, root: &Path, rev: &str) -> FileSource {
        FileSource::worktree(root).with_fallback_rev(Some(rev.to_string()))
    }

    /// Whether Bazel has to be run somewhere other than the working tree to
    /// see `rev`.
    ///
    /// Trees, not commit shas: a rebase or a squash-merge that produced the
    /// same content is the same checkout as far as `bazel query` is concerned,
    /// and the store already keys indexes by tree for the same reason. A
    /// revision git cannot resolve (a synthetic id in the tests, a sha this
    /// clone does not have) is left to the working tree, which is the only
    /// thing there is.
    ///
    /// A *dirty* working tree at HEAD stays on the fast path deliberately:
    /// `cambi affected` on a developer's checkout is meant to index the edits
    /// in front of it, not the last commit.
    pub fn needs_revision_workspace(&self, rev: &str) -> bool {
        let (Ok(want), Ok(have)) = (self.repo.tree_of(rev), self.repo.tree_of("HEAD")) else {
            return false;
        };
        want != have
    }

    // ------------------------------------------------------------- cold build

    /// The querier a normal run uses.
    ///
    /// When the working tree is not at `rev`, this is where the second
    /// checkout appears: a detached worktree at `rev` with its own Bazel (see
    /// [`crate::revision`]). It is removed when the returned querier drops.
    pub fn bazel_querier(&self, rev: &str) -> Result<RevisionQuerier> {
        if !self.needs_revision_workspace(rev) {
            return Ok(RevisionQuerier::new(
                BazelQuerier {
                    bazel: self.bazel.clone(),
                    opts: self.cfg.convert_options(),
                    files: self.file_source(self.workspace(), rev),
                },
                None,
            ));
        }
        let sha = self.repo.rev_parse(rev)?;
        let ws = RevisionWorkspace::create(&self.repo, &sha, &worktrees_dir(&self.cache_dir))?;
        let bazel = ws.bazel(&self.bazel, &sha);
        let files = self.file_source(ws.path(), &sha);
        Ok(RevisionQuerier::new(
            BazelQuerier {
                bazel,
                opts: self.cfg.convert_options(),
                files,
            },
            Some(ws),
        ))
    }

    /// Full build from `bazel query //...` (spec 6.9). Mints a new lineage.
    pub fn full_build(&self, commit: &str) -> Result<IndexData> {
        tracing::info!("running full bazel query //... (cold build)");
        self.full_build_with(commit, &self.bazel_querier(commit)?)
    }

    /// Full build from an arbitrary row source.
    pub fn full_build_with(&self, commit: &str, q: &dyn PackageQuerier) -> Result<IndexData> {
        let u = q.universe()?;
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.set_bazel_version(q.version());
        b.set_config_hash(self.cfg.hash());
        if let Some(e) = &u.partial {
            b.set_partial(e.count, e.packages.clone());
        }
        b.apply_packages(&u.updates);
        let packages: Vec<String> = u.updates.iter().map(|p| p.package.clone()).collect();
        if let Some(c) = rescan_closure(q, None, &packages, &BTreeSet::new(), &BTreeSet::new()) {
            b.set_bzl_closure(c);
        }
        Ok(b.finish(commit))
    }

    // ------------------------------------------------------------- baselines

    /// The git tree a revision points at, or the revision string itself when
    /// git cannot resolve it (synthetic revision ids in tests, a sha the local
    /// clone does not have).
    pub fn tree_of(&self, rev: &str) -> String {
        self.repo.tree_of(rev).unwrap_or_else(|_| rev.to_string())
    }

    /// The store, with the cache directory and config hash a fetch needs.
    fn baselines(&self) -> Option<BaselineCache<'_>> {
        self.store
            .as_deref()
            .map(|s| BaselineCache::new(s, &self.cache_dir, self.cfg.hash()))
    }

    /// Finds an index for `commit` in the artifact store and caches it
    /// locally. See [`BaselineCache::fetch`] for how a commit becomes a key.
    pub fn fetch_baseline(&self, commit: &str) -> Result<Option<PathBuf>> {
        match self.baselines() {
            Some(c) => c.fetch(commit, &self.tree_of(commit)),
            None => Ok(None),
        }
    }

    /// Publishes the index file at `path` under every key the store layout
    /// wants for `commit` (see [`BaselineCache::publish`]), and returns them.
    ///
    /// A partial index — one whose query finished with load errors — is
    /// refused unless `baseline.publish_partial` is set: publishing it would
    /// hand every later run a base that silently lacks packages.
    pub fn publish_index(
        &self,
        path: &Path,
        lineage: &[u8; 16],
        commit: &str,
    ) -> Result<Vec<String>> {
        self.publish_index_allowing_partial(path, lineage, commit, false)
    }

    fn publish_index_allowing_partial(
        &self,
        path: &Path,
        lineage: &[u8; 16],
        commit: &str,
        allow_partial: bool,
    ) -> Result<Vec<String>> {
        let Some(cache) = self.baselines() else {
            return Err(EngineError::NoStore);
        };
        if !(allow_partial || self.cfg.baseline.publish_partial) {
            let f = IndexFile::open(path)?;
            let load = crate::inspect::PartialLoad::from(f.view());
            if load.partial {
                return Err(EngineError::PartialIndex {
                    commit: commit.to_string(),
                    packages_in_error: load.packages_in_error,
                });
            }
        }
        cache.publish(path, lineage, commit, &self.tree_of(commit))
    }

    /// Locates a usable baseline for `base` (spec 6.5): the store, then
    /// first-parent ancestors brought forward, then a cold build.
    fn resolve_baseline(
        &self,
        base: &str,
        req: &AffectedRequest,
    ) -> Result<(Baseline, BaselineOrigin)> {
        if let Some(found) = self.fetch_mapped(base)? {
            return Ok(found);
        }
        // The replay's head is `base`, so its querier is the one built for
        // `base` — not the one the outer run uses for its own head. On a
        // checkout that is at head (the usual `affected` shape) that querier
        // runs in a worktree at `base`; without it the "base" index would be
        // built from head's BUILD files, and a target added at head would sit
        // in both indexes with the same hash and never be reported.
        //
        // The querier is built only once the store has come up empty for
        // `base` itself: on a checkout at head, building it means a detached
        // worktree and a second Bazel server, which is seconds of work that a
        // fetched baseline never needs.
        self.replay_or_build(base, req, &self.bazel_querier(base)?)
    }

    /// As [`Engine::resolve_baseline`], with the row source for the replay step
    /// supplied. Taking the querier is what lets the ancestor walk be tested
    /// without Bazel.
    pub fn resolve_baseline_with(
        &self,
        base: &str,
        req: &AffectedRequest,
        querier: &dyn PackageQuerier,
    ) -> Result<(Baseline, BaselineOrigin)> {
        if let Some(found) = self.fetch_mapped(base)? {
            return Ok(found);
        }
        self.replay_or_build(base, req, querier)
    }

    /// The store's index for `base`, mapped and checked against the config.
    fn fetch_mapped(&self, base: &str) -> Result<Option<(Baseline, BaselineOrigin)>> {
        let Some(path) = self.fetch_baseline(base)? else {
            return Ok(None);
        };
        let f = IndexFile::open(&path)?;
        self.check_compatible(f.view(), &path)?;
        tracing::info!(%base, path = %path.display(), "loaded baseline index");
        Ok(Some((Baseline::Mapped(f), BaselineOrigin::Fetched)))
    }

    /// The store had nothing for `base`: bring the nearest indexed ancestor
    /// forward, or build from scratch.
    fn replay_or_build(
        &self,
        base: &str,
        req: &AffectedRequest,
        querier: &dyn PackageQuerier,
    ) -> Result<(Baseline, BaselineOrigin)> {
        let max = self.cfg.baseline.max_ancestor_walk;
        for anc in self.repo.first_parent_ancestors(base, max)? {
            let Some(path) = self.fetch_baseline(&anc)? else {
                continue;
            };
            let f = IndexFile::open(&path)?;
            self.check_compatible(f.view(), &path)?;
            tracing::info!(
                ancestor = %anc,
                "no baseline at base; bringing an ancestor forward"
            );
            // One replay from the nearest ancestor that has an index, not one
            // per commit: the dirty set over the whole range is the union, and
            // re-querying a package twice would buy nothing.
            let inner = AffectedRequest {
                base: anc.clone(),
                head: base.to_string(),
                // The replay's whole purpose is to produce the base index.
                materialize: true,
                ..req.clone()
            };
            let changes = self.repo.diff(&anc, base)?;
            let out = self.compute(f.view(), &changes, querier, &inner)?;
            return match out.index {
                Some(idx) => Ok((
                    Baseline::Owned(Box::new(idx)),
                    BaselineOrigin::Replayed { from: anc },
                )),
                // The ancestor..base range invalidated everything, so the
                // ancestor's index cannot be brought forward.
                None => self.cold_baseline(base, req, querier),
            };
        }
        self.cold_baseline(base, req, querier)
    }

    fn cold_baseline(
        &self,
        base: &str,
        req: &AffectedRequest,
        querier: &dyn PackageQuerier,
    ) -> Result<(Baseline, BaselineOrigin)> {
        if req.no_full_build {
            return Err(EngineError::BaselineUnavailable {
                base: base.to_string(),
            });
        }
        Ok((
            Baseline::Owned(Box::new(self.full_build_with(base, querier)?)),
            BaselineOrigin::ColdBuild,
        ))
    }

    /// Publishes a base index this run had to compute, under the base commit.
    ///
    /// Only ever the base. The head-side index of an `affected` run is not
    /// published: a pull-request head is not on the main line, publishing it
    /// would fill the store with indexes nothing will ever ask for again, and
    /// an `affected` run does not even materialise one. `baseline update` is
    /// what publishes head indexes.
    pub fn publish_base_index(&self, index: &IndexData) -> Result<Vec<String>> {
        self.publish_base_index_allowing_partial(index, false)
    }

    fn publish_base_index_allowing_partial(
        &self,
        index: &IndexData,
        allow_partial: bool,
    ) -> Result<Vec<String>> {
        let commit = index.provenance.commit.clone();
        let dir = self.cache_dir.join("publish");
        std::fs::create_dir_all(&dir).map_err(EngineError::io_at(&dir))?;
        let path = dir.join(format!("{commit}.bin"));
        cambium_index::format::write_index(&path, index)?;
        let names = self.publish_index_allowing_partial(
            &path,
            &index.header.lineage,
            &commit,
            allow_partial,
        );
        let _ = std::fs::remove_file(&path);
        names
    }

    /// Publishes a base index that is already a file on disk (a mapped
    /// baseline), or one held in memory.
    pub fn publish_baseline(&self, base: &Baseline, allow_partial: bool) -> Result<Vec<String>> {
        match base {
            Baseline::Owned(d) => self.publish_base_index_allowing_partial(d, allow_partial),
            Baseline::Mapped(f) => {
                let v = f.view();
                self.publish_index_allowing_partial(f.path(), &v.lineage, v.commit, allow_partial)
            }
        }
    }

    /// An index may only be spliced onto if the config that produced it still
    /// holds (spec 5.5).
    fn check_compatible(&self, v: IndexRef<'_>, path: &Path) -> Result<()> {
        if v.config_hash != self.cfg.hash() {
            return Err(EngineError::ConfigHashMismatch {
                path: path.to_path_buf(),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------- run

    /// Answers "what is affected between base and head".
    pub fn affected(&self, req: &AffectedRequest) -> Result<RunOutcome> {
        let mut sw = Stopwatch::start();
        let total = Stopwatch::start();
        let base = self
            .repo
            .rev_parse(&req.base)
            .unwrap_or_else(|_| req.base.clone());
        let head = self
            .repo
            .rev_parse(&req.head)
            .unwrap_or_else(|_| req.head.clone());
        let req = AffectedRequest {
            base: base.clone(),
            head: head.clone(),
            ..req.clone()
        };

        let (base_index, origin) = self.resolve_baseline(&base, &req)?;
        if req.publish_base {
            match &origin {
                BaselineOrigin::Fetched => {
                    tracing::debug!(%base, "base index came from the store; nothing to publish")
                }
                origin => match self.publish_baseline(&base_index, req.allow_partial) {
                    Ok(names) => tracing::info!(
                        %base, ?origin, published = ?names,
                        "published the computed base index"
                    ),
                    // A run that produced the right answer should not fail
                    // because the store was unwritable; the next run will
                    // simply have to compute the base again.
                    Err(e) => tracing::warn!(%base, error = %e, "could not publish the base index"),
                },
            }
        }
        let load_ms = sw.lap();

        let mut out = self.run_incremental(base_index.view(), &req)?;
        out.report.timings_ms.load += load_ms;
        out.report.timings_ms.total = total.elapsed_ms();
        Ok(out)
    }

    /// The incremental pipeline, given a base index already in hand. Asks git
    /// what changed and queries Bazel for the dirty packages.
    pub fn run_incremental(
        &self,
        base_view: IndexRef<'_>,
        req: &AffectedRequest,
    ) -> Result<RunOutcome> {
        let changes = self.repo.diff(&req.base, &req.head)?;
        self.compute(base_view, &changes, &self.bazel_querier(&req.head)?, req)
    }

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
    fn screen_bazelrc<'a>(&self, changes: &'a [Change], req: &AffectedRequest) -> ScreenedDiff<'a> {
        let mut kept: Vec<&Change> = Vec::with_capacity(changes.len());
        let mut safe = 0usize;
        for c in changes {
            if !bazelrc::is_bazelrc_path(&c.path) {
                kept.push(c);
                continue;
            }
            match self.bazelrc_verdict(c, req) {
                Ok(n) => {
                    tracing::debug!(
                        path = %c.path, changed_flags = n,
                        "the bazelrc edit only touched flags that cannot affect loading"
                    );
                    safe += 1;
                }
                Err(reason) => {
                    tracing::info!(path = %c.path, %reason, "bazelrc edit forces a global run");
                    return ScreenedDiff {
                        changes: changes.iter().collect(),
                        safe,
                        global_reason: Some(reason),
                    };
                }
            }
        }
        ScreenedDiff {
            changes: kept,
            safe,
            global_reason: None,
        }
    }

    /// `Ok(changed flags)` if this one bazelrc change is irrelevant to loading,
    /// `Err(reason)` if it is not or cannot be read.
    fn bazelrc_verdict(
        &self,
        c: &Change,
        req: &AffectedRequest,
    ) -> std::result::Result<usize, String> {
        use cambium_diff::Status;
        let path = &c.path;
        match c.status {
            Status::Modified => {}
            // A bazelrc that appears or vanishes changes which files Bazel
            // reads at all, and the diff of a file against nothing is not a
            // flag diff.
            _ => return Err(format!("{path}: the file was added or removed")),
        }
        let text = |rev: &str| -> std::result::Result<String, String> {
            let bytes = self
                .repo
                .read_at(rev, path)
                .ok_or_else(|| format!("{path}: cannot be read at {rev}"))?;
            String::from_utf8(bytes).map_err(|_| format!("{path}: is not UTF-8 at {rev}"))
        };
        let base = text(&req.base)?;
        let head = text(&req.head)?;
        match bazelrc::classify(&base, &head, &self.cfg.bazelrc.allowlist_extra) {
            bazelrc::Verdict::Safe { changed_flags } => Ok(changed_flags),
            bazelrc::Verdict::Unknown(why) => Err(format!("{path}: {why}")),
        }
    }

    /// The pipeline proper: dirty set, query, splice, direct set, walk.
    ///
    /// Takes the diff and the row source as parameters so the whole engine can
    /// be exercised without git or Bazel (spec 12.4).
    pub fn compute(
        &self,
        base_view: IndexRef<'_>,
        changes: &[Change],
        querier: &dyn PackageQuerier,
        req: &AffectedRequest,
    ) -> Result<RunOutcome> {
        self.compute_with_texts(
            base_view,
            changes,
            querier,
            req,
            &crate::externals::GitText(&self.repo),
        )
    }

    /// As [`Engine::compute`], with the workspace-file reader supplied.
    ///
    /// The external-repository stage needs `MODULE.bazel` at *both* ends of
    /// the diff, and the base end is not on disk. The real run reads it with
    /// `git show`; the tests hand over two strings.
    pub fn compute_with_texts(
        &self,
        base_view: IndexRef<'_>,
        changes: &[Change],
        querier: &dyn PackageQuerier,
        req: &AffectedRequest,
        texts: &dyn crate::externals::WorkspaceText,
    ) -> Result<RunOutcome> {
        let mut sw = Stopwatch::start();
        let mut timings = Timings::default();
        let mut stats = Stats {
            base_nodes: base_view.num_nodes(),
            changed_files: changes.len(),
            ..Default::default()
        };

        // ---- external repositories ----
        //
        // Runs first because its answer decides whether `MODULE.bazel` still
        // counts as an invalidator: a diff this stage mapped onto a set of
        // repositories has been accounted for precisely, and letting the same
        // file also fire the invalidator rule would throw that away.
        let mut ext = if self.cfg.externals.enabled {
            crate::externals::analyse(base_view, changes, &req.base, &req.head, texts)
        } else {
            Externals::default()
        };
        stats.repos_dirty_by_input = ext.by_input;
        stats.repos_dirty_by_module = ext.by_module;
        stats.repos_label_only = ext.label_only;

        let mut matchers = Matchers::compile(&self.cfg.rules())?;
        matchers.exempt(ext.handled.clone());
        let mut packages = PackageSet::from_index(base_view);

        // ---- dirty set ----
        //
        // A changed bazelrc is read before the generic invalidator check, and
        // dropped from the diff when every flag it changed is known not to
        // reach loading or analysis. Anything else about it — an unknown flag,
        // an import, the file appearing or vanishing — invalidates the run, so
        // this only ever turns a global into a smaller answer.
        let screened = self.screen_bazelrc(changes, req);
        stats.bazelrc_safe_diffs = screened.safe;
        if let Some(reason) = screened.global_reason {
            timings.dirty = sw.lap();
            timings.total = timings.dirty;
            let dirty = DirtySet {
                global: true,
                global_reason: Some(reason),
                ..Default::default()
            };
            return Ok(RunOutcome {
                report: global_report(req, dirty, timings, stats),
                index: None,
            });
        }

        let mut dirty = cambium_classify::dirty_set(
            &screened.changes,
            &mut packages,
            &matchers,
            &cambium_classify::IndexConsumers(base_view),
        );

        let bzl_dirty = self.resolve_changed_bzl(base_view, &mut dirty, &mut ext, &mut stats);
        keep_surviving_packages(&mut dirty, querier, self.workspace());
        stats.ignored_files = dirty.ignored;

        // ---- from dirty repositories to graph work ----
        //
        // The stage's own escalation replaces an invalidator's wording: both
        // say "everything", but only one of them says why in terms of the
        // module system.
        if let Some(reason) = ext.global.take() {
            dirty.global = true;
            dirty.global_reason = Some(reason);
        }
        stats.repos_dirty_by_bzl = ext.by_bzl;
        let mut external_nodes: Vec<(u32, String)> = Vec::new();
        if !dirty.global && !ext.repos.is_empty() {
            let r = crate::externals::reach(base_view, &ext.repos, &ext.load_repos);
            tracing::debug!(
                repos = ?ext.repos,
                load_repos = ?ext.load_repos,
                nodes = r.nodes.len(),
                packages = r.packages.len(),
                "external repositories moved"
            );
            dirty.dirty.extend(r.packages);
            external_nodes = r
                .nodes
                .into_iter()
                .map(|id| (id, base_view.label(id).to_string()))
                .collect();
        }
        stats.external_direct_targets = external_nodes.len();
        timings.dirty = sw.lap();

        if dirty.global {
            timings.total = timings.dirty;
            return Ok(RunOutcome {
                report: global_report(req, dirty, timings, stats),
                index: None,
            });
        }

        // ---- query ----
        //
        // Normally the dirty packages, and only those. Past
        // `limits.max_dirty_packages` that stops being the cheap way round: a
        // per-package `bazel query` costs tens of milliseconds a package, and
        // once enough of them are dirty, one `//...` query answers for the
        // whole repository for less. So the run re-queries everything and
        // splices all of it onto the base.
        //
        // That is a cost decision, not an accuracy one, and the distinction is
        // the whole point of the change: the limit it replaces
        // (`max_changed_files`) made a large diff *global*, discarding the
        // answer. A cold rebuild keeps it. Every package the dirty set would
        // have named is in the universe, so the splice sees at least the rows
        // the incremental path would have seen; the packages it did not name
        // come back byte-identical and contribute nothing to the direct set.
        // The soundness sweep asserts exactly that, by running each synthetic
        // revision both ways.
        stats.dirty_packages = dirty.dirty.len();
        let limit = self.cfg.limits.max_dirty_packages;
        let cold = limit > 0 && dirty.dirty.len() > limit;
        let mut cold_reason = None;
        let (to_query, rows) = if cold {
            let reason = cold_rebuild_reason(dirty.dirty.len(), limit);
            tracing::info!(%reason, "re-indexing the whole repository at head");
            cold_reason = Some(reason);
            let u = querier.universe()?;
            let names: Vec<String> = u.updates.iter().map(|p| p.package.clone()).collect();
            (names, u)
        } else {
            let to_query: Vec<String> = dirty
                .dirty
                .iter()
                .filter(|p| !dirty.deleted_packages.contains(*p))
                .cloned()
                .collect();
            let rows = querier.query(&to_query)?;
            (to_query, rows)
        };
        stats.cold_rebuild = cold;
        stats.queried_targets = rows.updates.iter().map(|u| u.targets.len()).sum();
        timings.query = sw.lap();

        // ---- splice ----
        let mut builder = IndexBuilder::from_view(base_view);
        builder.set_config_hash(self.cfg.hash());
        // A query that exited 3 under `--keep_going` loaded some of what it
        // was asked for, and the index has to say so — a package that failed
        // to load was tombstoned as if deleted, and only this flag tells a
        // reader that its targets are missing rather than gone.
        if let Some(e) = &rows.partial {
            builder.set_partial(e.count, e.packages.clone());
        }
        // Only the materialised path: an overlay is never published, and the
        // run that produced it read the closure it needed out of the base.
        if req.materialize {
            if let Some(c) = rescan_closure(
                querier,
                builder.take_bzl_closure(),
                &to_query,
                &dirty.changed_bzl,
                &dirty.deleted_packages,
            ) {
                builder.set_bzl_closure(c);
            }
        }
        if !base_view.bazel_version.is_empty() {
            builder.set_bazel_version(base_view.bazel_version.to_string());
        }
        let mut deleted_by_package: Vec<(String, Vec<u32>)> = Vec::new();
        for p in &dirty.deleted_packages {
            let ids = builder.delete_package(p);
            deleted_by_package.push((p.clone(), ids));
        }
        let splice = builder.apply_packages(&rows.updates);

        // Resolve changed source files to node ids while the builder still has
        // its interner: rebuilding a label map over a finished index costs
        // O(labels), and at a few million labels that is the single most
        // expensive thing in an otherwise change-proportional run.
        let mut changed_file_ids: Vec<(u32, String)> = Vec::with_capacity(dirty.direct_files.len());
        for path in &dirty.direct_files {
            let Some(pkg) = packages.owner_of(path) else {
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

        let direct_inputs = DirectInputs {
            splice: &splice,
            deleted_by_package: &deleted_by_package,
            changed_file_ids: &changed_file_ids,
            bzl_dirty: &bzl_dirty,
            external_nodes: &external_nodes,
        };
        let mut run = RunState {
            req,
            dirty: &dirty,
            timings,
            stats,
            cold_reason,
            sw,
        };

        // Materialise a whole new index, or take only the delta.
        //
        // Both answer the same question over the same graph — one implements it
        // as a complete index, the other as the base plus corrections — and the
        // property tests compute every synthetic revision both ways and require
        // the answers to be identical.
        if req.materialize {
            let head_index = builder.finish(&req.head);
            let hv = head_index.view();
            run.stats.head_nodes = hv.num_nodes();
            run.stats.head_edges = hv.num_edges() as u64;
            let report = run.answer(&hv, &direct_inputs);
            Ok(RunOutcome {
                report,
                index: Some(head_index),
            })
        } else {
            let overlay = builder.finish_overlay();
            let delta = overlay.delta_size();
            run.stats.head_nodes = overlay.num_nodes();
            // The overlay knows the base's edge count and every edge it added
            // or removed, which is the same number the materialised index
            // would report.
            run.stats.head_edges = base_view.num_edges() as u64 + delta.edges_added as u64
                - delta.edges_removed as u64;
            tracing::debug!(delta = ?delta, "walking a delta overlay");
            let report = run.answer(&overlay, &direct_inputs);
            Ok(RunOutcome {
                report,
                index: None,
            })
        }
    }

    /// Turns the changed `.bzl` files into dirty packages, and returns the
    /// packages dirtied that way (they need the `BzlClosure` direct reason).
    ///
    /// Answered from the index's static load closure (see
    /// `cambium_index::loads`), not from Bazel. The `rbuildfiles` call this
    /// replaces needed Sky Query, which loads every external repository and
    /// therefore fails with exit 3 on any machine that cannot fetch one of
    /// them — and a partial answer there is unusable, so the run had to
    /// invalidate everything. The closure is computed at index time from the
    /// text of the BUILD and `.bzl` files, so this path has no Bazel process
    /// in it and no failure mode: one binary search and one CSR row per
    /// changed file.
    fn resolve_changed_bzl(
        &self,
        base_view: IndexRef<'_>,
        dirty: &mut DirtySet,
        ext: &mut Externals,
        stats: &mut Stats,
    ) -> BTreeSet<String> {
        let mut bzl_dirty: BTreeSet<String> = BTreeSet::new();
        if dirty.global || dirty.changed_bzl.is_empty() {
            return bzl_dirty;
        }
        let paths: Vec<String> = dirty.changed_bzl.iter().cloned().collect();
        if !base_view.bzl_closure {
            // An index built by a row source that could not read the tree.
            // "No consumers" and "we never looked" are the same empty
            // table, and only one of them is safe to act on.
            tracing::warn!(
                count = paths.len(),
                "the base index carries no static .bzl load closure; treating the change as global"
            );
            dirty.global = true;
            dirty.global_reason = Some(no_closure_reason(&paths));
            return bzl_dirty;
        }
        let mut workspace_level: Vec<String> = Vec::new();
        for path in &paths {
            // A `.bzl` the closure does not know is loaded by nothing: a dead
            // file, or one added by this very commit (in which case whatever
            // BUILD file started loading it changed too, and that dirties the
            // package on its own).
            let Some(id) = base_view.bzl_id(path) else {
                tracing::debug!(%path, "no package or workspace file loads this .bzl");
                continue;
            };
            // Reachable from MODULE.bazel/WORKSPACE: it can define repository
            // rules, whose output is not in the index at all. The index
            // records which repositories each such `.bzl` defines (format 5),
            // so the usual answer is "these repositories moved"; only a file
            // no extension and no repo rule reaches still escalates, as the
            // `rbuildfiles` path always did.
            if base_view.bzl_in_workspace(id) {
                match self
                    .cfg
                    .externals
                    .enabled
                    .then(|| crate::externals::repos_of_workspace_bzl(base_view, id))
                    .flatten()
                {
                    Some(repos) => {
                        tracing::debug!(%path, ?repos, "a workspace-level .bzl names its repositories");
                        ext.by_bzl += repos.len();
                        // The rule's implementation moved, so what it writes
                        // into the repository can be anything.
                        ext.load_repos.extend(repos.iter().cloned());
                        ext.repos.extend(repos);
                    }
                    None => workspace_level.push(path.clone()),
                }
            }
            for &p in base_view.bzl_consumers(id) {
                if base_view.package_tombstoned(p) {
                    continue;
                }
                let pkg = base_view.package_name(p).to_string();
                bzl_dirty.insert(pkg.clone());
                dirty.dirty.insert(pkg);
            }
        }
        stats.bzl_closure_hits = bzl_dirty.len();
        tracing::debug!(
            changed = paths.len(),
            packages = bzl_dirty.len(),
            "resolved changed .bzl files from the static load closure"
        );
        if !workspace_level.is_empty() {
            tracing::warn!(
                files = ?workspace_level,
                "a changed .bzl is loaded outside any package; treating the change as global"
            );
            dirty.global = true;
            dirty.global_reason = Some(workspace_level_reason(&workspace_level));
        }
        bzl_dirty
    }
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

/// Everything the direct set is derived from, once the splice has happened.
struct DirectInputs<'a> {
    splice: &'a SpliceReport,
    deleted_by_package: &'a [(String, Vec<u32>)],
    changed_file_ids: &'a [(u32, String)],
    bzl_dirty: &'a BTreeSet<String>,
    external_nodes: &'a [(u32, String)],
}

/// What the tail of a run carries from the splice to the report.
struct RunState<'a> {
    req: &'a AffectedRequest,
    dirty: &'a DirtySet,
    timings: Timings,
    stats: Stats,
    cold_reason: Option<String>,
    sw: Stopwatch,
}

impl RunState<'_> {
    /// Direct set, walk, report — over a materialised head index or an
    /// overlay. One implementation for both, so the two paths cannot disagree
    /// about anything after the splice.
    fn answer<V: GraphView + ?Sized>(
        mut self,
        view: &V,
        inputs: &DirectInputs<'_>,
    ) -> AffectedReport {
        self.timings.splice = self.sw.lap();

        let (direct, reasons) = derive_direct(view, inputs);
        self.stats.direct_targets = direct.len();
        self.timings.direct = self.sw.lap();

        let wopts = WalkOptions {
            include_manual: self.req.include_manual,
            explain: self.req.explain,
            max_depth: self.req.max_depth,
        };
        let result = walk::walk_view(view, &direct, &wopts);
        self.stats.affected_targets = result.affected.len();
        self.timings.walk = self.sw.lap();

        AffectedReport {
            schema: crate::report::SCHEMA_VERSION,
            base: self.req.base.clone(),
            head: self.req.head.clone(),
            global: false,
            global_reason: None,
            cold_rebuild_reason: self.cold_reason,
            dirty_packages: self.dirty.dirty.iter().cloned().collect(),
            direct: cambium_index::view::labels_sorted(view, &direct),
            affected: affected_targets(view, &result),
            tests: cambium_index::view::labels_sorted(view, &result.tests),
            timings_ms: self.timings,
            stats: self.stats,
            explanations: self.req.explain.then(|| explain(view, &result, &reasons)),
        }
    }
}

/// The direct set (spec 6.7), over either a materialised index or an overlay.
fn derive_direct<V: GraphView + ?Sized>(
    view: &V,
    inputs: &DirectInputs<'_>,
) -> (Vec<u32>, FxHashMap<u32, DirectReason>) {
    /// First reason wins: a target is direct once, for the first cause seen.
    fn add(
        direct: &mut FxHashSet<u32>,
        reasons: &mut FxHashMap<u32, DirectReason>,
        id: u32,
        reason: DirectReason,
    ) {
        if direct.insert(id) {
            reasons.insert(id, reason);
        }
    }

    let mut reasons: FxHashMap<u32, DirectReason> = FxHashMap::default();
    let mut direct: FxHashSet<u32> = FxHashSet::default();
    let mut scratch: Vec<u32> = Vec::new();

    for pd in &inputs.splice.packages {
        if inputs.bzl_dirty.contains(&pd.package) {
            // Spec 6.7's "bzl closure hash changed" branch, keyed on
            // membership of the stored load closure instead of a stored hash,
            // and load-bearing rather than an optimisation.
            //
            // A `.bzl` edit that changes a rule implementation without changing
            // any BUILD attribute leaves every rule proto in the loading
            // packages byte-identical — Bazel folds a rule's definition
            // environment into the proto, not the loading BUILD file's load
            // closure. So every rule hash comes back unchanged and `pd.changed`
            // is empty. Marking every rule in the package direct regardless of
            // its hash is the only thing that catches it, and
            // `Packages::bzl_closure_hash` is written as zero for exactly that
            // reason: the closure names the same set the hash comparison would
            // have, and names *which* packages, which a hash cannot.
            // (Covered by the `edit bzl` end-to-end scenario, which
            // asserts this is *why* those targets are reported.)
            for &id in &pd.current {
                if view.is_rule(id) {
                    let reason = DirectReason::BzlClosure {
                        package: pd.package.clone(),
                    };
                    add(&mut direct, &mut reasons, id, reason);
                }
            }
        } else {
            for &id in &pd.changed {
                add(&mut direct, &mut reasons, id, DirectReason::RuleHash);
            }
        }
        for &id in &pd.deleted {
            let label = view.label(id).to_string();
            for &consumer in view.reverse_row(id, &mut scratch) {
                let reason = DirectReason::DeletedDep {
                    label: label.clone(),
                };
                add(&mut direct, &mut reasons, consumer, reason);
            }
        }
    }

    for (pkg, ids) in inputs.deleted_by_package {
        for &id in ids {
            let label = view.label(id).to_string();
            tracing::debug!(package = %pkg, %label, "package deleted");
            for &consumer in view.reverse_row(id, &mut scratch) {
                let reason = DirectReason::DeletedDep {
                    label: label.clone(),
                };
                add(&mut direct, &mut reasons, consumer, reason);
            }
        }
    }

    // A file node's in-edges all come from rules, so its reverse row is its
    // consumer list.
    for (id, path) in inputs.changed_file_ids {
        for &consumer in view.reverse_row(*id, &mut scratch) {
            let reason = DirectReason::File { path: path.clone() };
            add(&mut direct, &mut reasons, consumer, reason);
        }
    }

    // An external repository's contents moved. The repository's targets are
    // leaves here — nothing ever queried them — so the change cannot be seen
    // as a rule-hash movement; what can be seen is every main-repo rule that
    // names one of its labels, which is exactly the leaf node's reverse row.
    for (id, label) in inputs.external_nodes {
        for &consumer in view.reverse_row(*id, &mut scratch) {
            let reason = DirectReason::ExternalRepo {
                label: label.clone(),
            };
            add(&mut direct, &mut reasons, consumer, reason);
        }
    }

    let mut direct: Vec<u32> = direct.into_iter().collect();
    direct.sort_unstable();
    (direct, reasons)
}

/// The affected list with each target's BFS depth, sorted by label.
fn affected_targets<V: GraphView + ?Sized>(
    view: &V,
    result: &walk::WalkResult,
) -> Vec<crate::report::AffectedTarget> {
    let mut out: Vec<crate::report::AffectedTarget> = result
        .affected
        .iter()
        .map(|&id| crate::report::AffectedTarget {
            label: view.label(id).to_string(),
            depth: result.depth_of(id).unwrap_or(0),
            test: view.is_test(id),
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out.dedup_by(|a, b| a.label == b.label);
    out
}

/// `--explain`: for each affected target, the shortest path back to the direct
/// member that reached it, and why that member was direct.
fn explain<V: GraphView + ?Sized>(
    view: &V,
    result: &walk::WalkResult,
    reasons: &FxHashMap<u32, DirectReason>,
) -> Vec<Explanation> {
    let mut out = Vec::with_capacity(result.affected.len());
    for &id in &result.affected {
        let Some(path) = result.path_to_seed(id) else {
            continue;
        };
        let seed = *path.last().unwrap_or(&id);
        let reason = reasons
            .get(&seed)
            .cloned()
            .unwrap_or(DirectReason::RuleHash);
        out.push(Explanation {
            target: view.label(id).to_string(),
            path: path.iter().map(|&i| view.label(i).to_string()).collect(),
            direct: view.label(seed).to_string(),
            reason,
        });
    }
    out.sort_by(|a, b| a.target.cmp(&b.target));
    out
}

fn global_report(
    req: &AffectedRequest,
    dirty: DirtySet,
    timings: Timings,
    stats: Stats,
) -> AffectedReport {
    AffectedReport {
        schema: crate::report::SCHEMA_VERSION,
        base: req.base.clone(),
        head: req.head.clone(),
        global: true,
        global_reason: dirty.global_reason,
        cold_rebuild_reason: None,
        dirty_packages: Vec::new(),
        direct: Vec::new(),
        affected: Vec::new(),
        tests: Vec::new(),
        timings_ms: timings,
        stats,
        explanations: None,
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
fn rescan_closure(
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
    let mapping = querier.repo_mapping();
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
    // external repositories may have moved with it.
    let facts = cambium_index::read_module_facts(src);
    c.inputs = cambium_index::build_repo_inputs(src, &mapping, &c.graph, &c.workspace, &facts);
    Some(c)
}

/// Why a run gave up on splicing the dirty set and re-indexed instead.
fn cold_rebuild_reason(dirty: usize, limit: usize) -> String {
    format!("dirty packages {dirty} exceed limits.max_dirty_packages = {limit}")
}

/// The `global_reason` for a `.bzl` whose loader is not a main-repo package.
///
/// The wording is deliberately the one the `rbuildfiles` path used, so that the
/// benchmark's reason histogram is comparable across the change.
fn workspace_level_reason(paths: &[String]) -> String {
    let what = match paths {
        [one] => format!("`{one}`"),
        many => format!("{} changed `.bzl` files", many.len()),
    };
    format!(
        "{what} is loaded by MODULE.bazel/WORKSPACE (repository rules), not by any \
         package BUILD file; it is in the workspace load closure"
    )
}

/// The `global_reason` for a base index with no load closure to consult.
fn no_closure_reason(paths: &[String]) -> String {
    let what = match paths {
        [one] => format!("`{one}`"),
        many => format!("{} changed `.bzl` files", many.len()),
    };
    format!("{what} changed and the base index carries no static .bzl load closure")
}
