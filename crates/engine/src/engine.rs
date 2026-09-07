//! The run: load a baseline, work out what is dirty, re-query it, splice, walk.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use cambium_bazel::Bazel;
use cambium_classify::{DirtySet, Matchers, PackageSet};
use cambium_diff::Repo;
use cambium_index::{
    format, walk, GraphView, IndexBuilder, IndexData, IndexFile, IndexRef, NodeKind, WalkOptions,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::Config;
use crate::error::{EngineError, Result};
use crate::query::{BazelQuerier, FileSource, PackageQuerier};
use crate::report::{AffectedReport, DirectReason, Explanation, Stats, Stopwatch, Timings};
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
            max_depth: None,
        }
    }
}

/// A base index, however it was obtained.
///
/// A fetched index stays mmapped rather than being deserialised into owned
/// memory: the `affected` path only ever reads it, and copying a few hundred
/// megabytes to read it would undo what the delta overlay bought.
impl std::fmt::Debug for Baseline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.view();
        f.debug_struct("Baseline")
            .field("commit", &v.commit)
            .field("nodes", &v.num_nodes())
            .finish()
    }
}

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

impl Engine {
    /// Opens an engine rooted at the Bazel workspace containing `start`.
    pub fn open(
        start: &Path,
        config_path: Option<&Path>,
        bazel_binary: Option<&str>,
    ) -> Result<Self> {
        let workspace = cambium_bazel::find_workspace_root(start)
            .ok_or_else(|| EngineError::NoWorkspace(start.to_path_buf()))?;
        let cfg = match config_path {
            Some(p) => Config::load_from(p)?,
            None => Config::load(&workspace)?,
        };
        let binary = bazel_binary
            .map(str::to_string)
            .or_else(|| (!cfg.bazel.binary.is_empty()).then(|| cfg.bazel.binary.clone()));
        let bazel = Bazel::at(workspace.clone(), binary.as_deref())
            .with_extra_query_flags(cfg.bazel.extra_query_flags.clone())
            .with_startup_flags(cfg.bazel.startup_flags.clone());
        let repo = Repo::discover(&workspace).unwrap_or_else(|_| Repo::new(workspace.clone()));
        let store = store::from_config(&cfg.baseline.store);
        let cache_dir = std::env::temp_dir().join("cambium-cache");
        Ok(Self {
            cfg,
            repo,
            bazel,
            store,
            cache_dir,
        })
    }

    /// Engine over an already-known workspace, for tests.
    pub fn with_parts(cfg: Config, repo: Repo, bazel: Bazel) -> Self {
        let store = store::from_config(&cfg.baseline.store);
        Self {
            cfg,
            repo,
            bazel,
            store,
            cache_dir: std::env::temp_dir().join("cambium-cache"),
        }
    }

    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    pub fn workspace(&self) -> &Path {
        self.bazel.workspace()
    }

    fn file_source(&self, rev: &str) -> FileSource {
        FileSource::worktree(self.workspace()).with_fallback_rev(Some(rev.to_string()))
    }

    // ------------------------------------------------------------- cold build

    /// The querier a normal run uses.
    pub fn bazel_querier(&self, rev: &str) -> BazelQuerier<'_> {
        BazelQuerier {
            bazel: &self.bazel,
            opts: self.cfg.convert_options(),
            files: self.file_source(rev),
        }
    }

    /// Full build from `bazel query //...` (spec 6.9). Mints a new lineage.
    pub fn full_build(&self, commit: &str) -> Result<IndexData> {
        tracing::info!("running full bazel query //... (cold build)");
        self.full_build_with(commit, &self.bazel_querier(commit))
    }

    /// Full build from an arbitrary row source.
    pub fn full_build_with(&self, commit: &str, q: &dyn PackageQuerier) -> Result<IndexData> {
        let updates = q.universe()?;
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.set_bazel_version(q.version());
        b.set_config_hash(self.cfg.hash());
        b.apply_packages(&updates);
        Ok(b.finish(commit))
    }

    // ------------------------------------------------------------- baselines

    /// The git tree a revision points at, or the revision string itself when
    /// git cannot resolve it (synthetic revision ids in tests, a sha the local
    /// clone does not have).
    pub fn tree_of(&self, rev: &str) -> String {
        self.repo.tree_of(rev).unwrap_or_else(|_| rev.to_string())
    }

    /// Finds an index for `commit` in the artifact store.
    ///
    /// Indexes are keyed by tree hash — `indexes/<lineage>/<tree>.bin` — so the
    /// commit is first resolved to its tree, and any commit with the same
    /// content (a rebase, a squash-merge, a cherry-pick) hits the same blob. A
    /// run does not know the lineage before it has an index, so a store that
    /// can enumerate is asked for the key ending in this tree; failing that,
    /// for the `commits/<lineage>/<sha>` pointer, which names the tree when
    /// git could not. A store that cannot enumerate (plain HTTP) is asked for
    /// the lineage-less fallback keys the publisher also writes. Keys from the
    /// sha-addressed layout this replaces are still recognised.
    pub fn fetch_baseline(&self, commit: &str) -> Result<Option<PathBuf>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        std::fs::create_dir_all(&self.cache_dir)?;
        let tree = self.tree_of(commit);
        let mut suffixes = vec![format!("/{tree}.bin")];
        if tree != commit {
            suffixes.push(format!("/{commit}.bin"));
        }
        suffixes.push(format!("-{commit}.bin"));
        if let Ok(names) = store.list() {
            // Several lineages can hold this tree: every `baseline build`
            // mints one, and a config change (or a rule-hash version bump)
            // leaves the old lineage's blobs behind. Take the first one built
            // with this configuration; an incompatible blob is not a baseline,
            // it is a leftover.
            let candidates = names
                .iter()
                .filter(|n| suffixes.iter().any(|s| n.ends_with(s)));
            for name in candidates {
                if let Some(dest) = self.fetch_if_compatible(store.as_ref(), name)? {
                    return Ok(Some(dest));
                }
            }
            // Tooling that only has a sha: follow the pointer to the tree.
            let pointer_suffix = format!("/{commit}");
            let pointers = names
                .iter()
                .filter(|n| n.starts_with("commits/") && n.ends_with(&pointer_suffix));
            for name in pointers {
                let dest = self.cache_dir.join(cache_name(name));
                if !store.fetch(name, &dest)? {
                    continue;
                }
                let pointed = std::fs::read_to_string(&dest)?.trim().to_string();
                let suffix = format!("/{pointed}.bin");
                for name in names.iter().filter(|n| n.ends_with(&suffix)) {
                    if let Some(dest) = self.fetch_if_compatible(store.as_ref(), name)? {
                        return Ok(Some(dest));
                    }
                }
            }
            return Ok(None);
        }
        for name in [tree_addressed_key(&tree), commit_addressed_key(commit)] {
            if let Some(dest) = self.fetch_if_compatible(store.as_ref(), &name)? {
                return Ok(Some(dest));
            }
        }
        let pointer = commit_addressed_pointer(commit);
        let dest = self.cache_dir.join(cache_name(&pointer));
        if store.fetch(&pointer, &dest)? {
            let pointed = std::fs::read_to_string(&dest)?.trim().to_string();
            let name = tree_addressed_key(&pointed);
            if let Some(dest) = self.fetch_if_compatible(store.as_ref(), &name)? {
                return Ok(Some(dest));
            }
        }
        Ok(None)
    }

    /// Fetches one index blob into the cache and keeps it only if it was
    /// built with this configuration. An incompatible blob is logged and
    /// skipped, so that a store carrying an older lineage neither wins over a
    /// compatible one nor stops the ancestor walk.
    fn fetch_if_compatible(
        &self,
        store: &dyn ArtifactStore,
        name: &str,
    ) -> Result<Option<PathBuf>> {
        let dest = self.cache_dir.join(cache_name(name));
        if !store.fetch(name, &dest)? {
            return Ok(None);
        }
        let f = IndexFile::open(&dest)?;
        if f.view().config_hash != self.cfg.hash() {
            tracing::warn!(
                key = name,
                "skipping an index built with a different cambium.toml (or an older rule-hash version)"
            );
            return Ok(None);
        }
        Ok(Some(dest))
    }

    /// Publishes an index under `indexes/<lineage>/<tree>.bin`, with a
    /// `commits/<lineage>/<sha>` pointer naming the tree, plus lineage-less
    /// copies of both when the store cannot be listed.
    ///
    /// Idempotent: the payload is a function of the graph, so republishing the
    /// same tree rewrites identical bytes and concurrent publishers cannot
    /// disagree. Two commits with the same tree publish to the same key and
    /// differ only in the provenance trailer, which records whichever commit
    /// wrote last.
    pub fn publish_index(
        &self,
        path: &Path,
        lineage: &[u8; 16],
        commit: &str,
    ) -> Result<Vec<String>> {
        let Some(store) = &self.store else {
            return Err(EngineError::NoStore);
        };
        let tree = self.tree_of(commit);
        let listable = store.list().is_ok();
        let mut names = vec![format::index_key(lineage, &tree)];
        if !listable {
            names.push(tree_addressed_key(&tree));
        }
        for n in &names {
            store.publish(n, path)?;
        }
        if tree != commit {
            let pointer_file = self
                .cache_dir
                .join(format!("pointer.{}", format::temp_suffix()));
            std::fs::create_dir_all(&self.cache_dir)?;
            std::fs::write(&pointer_file, format!("{tree}\n"))?;
            let mut pointers = vec![format::commit_pointer_key(lineage, commit)];
            if !listable {
                pointers.push(commit_addressed_pointer(commit));
            }
            for p in &pointers {
                store.publish(p, &pointer_file)?;
            }
            let _ = std::fs::remove_file(&pointer_file);
            names.extend(pointers);
        }
        Ok(names)
    }

    /// Locates a usable baseline for `base` (spec 6.5): the store, then
    /// first-parent ancestors brought forward, then a cold build.
    fn resolve_baseline(
        &self,
        base: &str,
        req: &AffectedRequest,
    ) -> Result<(Baseline, BaselineOrigin)> {
        // The replay's head is `base`, so its querier is the one built for
        // `base` — not the one the outer run uses for its own head.
        self.resolve_baseline_with(base, req, &self.bazel_querier(base))
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
        if let Some(path) = self.fetch_baseline(base)? {
            let f = IndexFile::open(&path)?;
            self.check_compatible(f.view(), &path)?;
            tracing::info!(%base, path = %path.display(), "loaded baseline index");
            return Ok((Baseline::Mapped(f), BaselineOrigin::Fetched));
        }

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
                None => {
                    if req.no_full_build {
                        Err(EngineError::BaselineUnavailable {
                            base: base.to_string(),
                        })
                    } else {
                        Ok((
                            Baseline::Owned(Box::new(self.full_build_with(base, querier)?)),
                            BaselineOrigin::ColdBuild,
                        ))
                    }
                }
            };
        }

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
    /// published: a pull-request head is not on the main line, and publishing
    /// it would fill the store with indexes nothing will ever ask for again —
    /// and, once the delta overlay lands, an `affected` run will not even
    /// materialise one. `baseline update` is what publishes head indexes.
    pub fn publish_base_index(&self, index: &IndexData) -> Result<Vec<String>> {
        let commit = index.provenance.commit.clone();
        let dir = self.cache_dir.join("publish");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{commit}.bin"));
        cambium_index::format::write_index(&path, index)?;
        let names = self.publish_index(&path, &index.header.lineage, &commit)?;
        let _ = std::fs::remove_file(&path);
        Ok(names)
    }

    /// Publishes a base index that is already a file on disk (a mapped
    /// baseline), or one held in memory.
    pub fn publish_baseline_for_test(&self, base: &Baseline) -> Result<Vec<String>> {
        self.publish_baseline(base)
    }

    fn publish_baseline(&self, base: &Baseline) -> Result<Vec<String>> {
        match base {
            Baseline::Owned(d) => self.publish_base_index(d),
            Baseline::Mapped(f) => {
                let v = f.view();
                self.publish_index(f.path(), &v.lineage, v.commit)
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
                origin => match self.publish_baseline(&base_index) {
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
        self.compute(base_view, &changes, &self.bazel_querier(&req.head), req)
    }

    /// The pipeline proper: dirty set, query, splice, direct set, walk.
    ///
    /// Takes the diff and the row source as parameters so the whole engine can
    /// be exercised without git or Bazel (spec 12.4).
    pub fn compute(
        &self,
        base_view: IndexRef<'_>,
        changes: &[cambium_diff::Change],
        querier: &dyn PackageQuerier,
        req: &AffectedRequest,
    ) -> Result<RunOutcome> {
        let mut sw = Stopwatch::start();
        let mut timings = Timings::default();
        let mut stats = Stats {
            base_nodes: base_view.num_nodes(),
            ..Default::default()
        };

        // ---- dirty set ----
        stats.changed_files = changes.len();
        let matchers = Matchers::compile(&self.cfg.rules())?;
        let mut packages = PackageSet::from_index(base_view);
        let mut dirty = cambium_classify::dirty_set(
            changes,
            &mut packages,
            &matchers,
            self.cfg.limits.max_changed_files,
            &cambium_classify::IndexConsumers(base_view),
        );

        // `.bzl` files are not package inputs by path; Bazel resolves them.
        let mut bzl_dirty: BTreeSet<String> = BTreeSet::new();
        if !dirty.global && !dirty.changed_bzl.is_empty() {
            let paths: Vec<String> = dirty.changed_bzl.iter().cloned().collect();
            tracing::debug!(
                count = paths.len(),
                "resolving changed .bzl files with rbuildfiles"
            );
            stats.rbuildfiles_called = true;
            match querier.rbuildfiles(&paths) {
                Ok(pkgs) => {
                    for p in pkgs {
                        bzl_dirty.insert(p.clone());
                        dirty.dirty.insert(p);
                    }
                }
                // Sky Query loads the whole universe, external repositories
                // included, so on a machine where some of those cannot be
                // fetched it finishes with errors (exit 3) even though the
                // package query itself is fine. The rbuildfiles answer is
                // then incomplete, and the only sound move is to treat the
                // commit as global: over-approximate rather than miss a
                // package that loads the changed .bzl.
                Err(EngineError::Bazel(cambium_bazel::BazelError::Partial { stderr, .. })) => {
                    tracing::warn!(
                        stderr = %stderr,
                        "rbuildfiles finished with errors; treating the change as global"
                    );
                    dirty.global = true;
                    dirty.global_reason = Some(
                        "rbuildfiles finished with errors under --keep_going (bazel exit 3)"
                            .to_string(),
                    );
                }
                Err(e) => return Err(e),
            }
        }
        // A deleted `BUILD` does not always delete the package. `bazel/bazel`
        // spent several commits carrying both `docs/BUILD` and
        // `docs/BUILD.bazel`, and the commit that removed the old one reads,
        // in the diff alone, exactly like a package deletion — after which
        // the package is dropped from the index and every target in it
        // becomes invisible to every later revision. (The corpus comparison
        // found this: `//docs:docs` and `//docs:srcs` were reported by both
        // bazel-diff and Tango, on pairs where cambium's index no longer had
        // a `docs` package at all.) The diff cannot tell the two cases apart,
        // but the head worktree can: if either BUILD file is still on disk,
        // the package lives and is merely dirty.
        if !dirty.global && !dirty.deleted_packages.is_empty() {
            let ws = self.bazel.workspace().to_path_buf();
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

        stats.ignored_files = dirty.ignored;
        timings.dirty = sw.lap();

        if dirty.global {
            timings.total = timings.dirty;
            return Ok(RunOutcome {
                report: global_report(req, dirty, timings, stats),
                index: None,
            });
        }

        // ---- query ----
        let to_query: Vec<String> = dirty
            .dirty
            .iter()
            .filter(|p| !dirty.deleted_packages.contains(*p))
            .cloned()
            .collect();
        stats.dirty_packages = dirty.dirty.len();
        let updates = querier.query(&to_query)?;
        stats.queried_targets = updates.iter().map(|u| u.targets.len()).sum();
        timings.query = sw.lap();

        // ---- splice ----
        let mut builder = IndexBuilder::from_view(base_view);
        builder.set_config_hash(self.cfg.hash());
        if !base_view.bazel_version.is_empty() {
            builder.set_bazel_version(base_view.bazel_version.to_string());
        }
        let mut deleted_by_package: Vec<(String, Vec<u32>)> = Vec::new();
        for p in &dirty.deleted_packages {
            let ids = builder.delete_package(p);
            deleted_by_package.push((p.clone(), ids));
        }
        let splice = builder.apply_packages(&updates);

        // Resolve changed source files to node ids while the builder still has
        // its interner: rebuilding a label map over a finished index costs
        // O(labels), and at a few million labels that is the single most
        // expensive thing in an otherwise change-proportional run.
        let wopts = WalkOptions {
            include_manual: req.include_manual,
            explain: req.explain,
            max_depth: req.max_depth,
        };
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

        // Materialise a whole new index, or take only the delta.
        //
        // Both answer the same question over the same graph — one implements it
        // as a complete index, the other as the base plus corrections — and the
        // property tests compute every synthetic revision both ways and require
        // the answers to be identical.
        let (report, index) = if req.materialize {
            let head_index = builder.finish(&req.head);
            let hv = head_index.view();
            stats.head_nodes = hv.num_nodes();
            stats.head_edges = hv.num_edges() as u64;
            timings.splice = sw.lap();

            let (direct, reasons) = derive_direct(
                &hv,
                &splice,
                &deleted_by_package,
                &changed_file_ids,
                &bzl_dirty,
            );
            stats.direct_targets = direct.len();
            timings.direct = sw.lap();

            let result = walk::walk_view(&hv, &direct, &wopts);
            stats.affected_targets = result.affected.len();
            timings.walk = sw.lap();

            (
                finish_report(&hv, req, &dirty, &direct, &result, &reasons, timings, stats),
                Some(head_index),
            )
        } else {
            let overlay = builder.finish_overlay();
            stats.head_nodes = overlay.num_nodes();
            // The overlay never counts the graph's edges; it only knows the
            // ones it changed.
            stats.head_edges = base_view.num_edges() as u64;
            timings.splice = sw.lap();
            tracing::debug!(delta = ?overlay.delta_size(), "walking a delta overlay");

            let (direct, reasons) = derive_direct(
                &overlay,
                &splice,
                &deleted_by_package,
                &changed_file_ids,
                &bzl_dirty,
            );
            stats.direct_targets = direct.len();
            timings.direct = sw.lap();

            let result = walk::walk_view(&overlay, &direct, &wopts);
            stats.affected_targets = result.affected.len();
            timings.walk = sw.lap();

            (
                finish_report(
                    &overlay, req, &dirty, &direct, &result, &reasons, timings, stats,
                ),
                None,
            )
        };

        Ok(RunOutcome { report, index })
    }
}

/// The direct set (spec 6.7), over either a materialised index or an overlay.
///
/// One implementation for both, so the two paths cannot disagree about what a
/// change made direct.
fn derive_direct<V: GraphView + ?Sized>(
    view: &V,
    splice: &cambium_index::SpliceReport,
    deleted_by_package: &[(String, Vec<u32>)],
    changed_file_ids: &[(u32, String)],
    bzl_dirty: &BTreeSet<String>,
) -> (Vec<u32>, FxHashMap<u32, DirectReason>) {
    let mut reasons: FxHashMap<u32, DirectReason> = FxHashMap::default();
    let mut direct: FxHashSet<u32> = FxHashSet::default();
    let mut scratch: Vec<u32> = Vec::new();
    let add = |id: u32,
               reason: DirectReason,
               direct: &mut FxHashSet<u32>,
               reasons: &mut FxHashMap<u32, DirectReason>| {
        if direct.insert(id) {
            reasons.insert(id, reason);
        }
    };

    for pd in &splice.packages {
        if bzl_dirty.contains(&pd.package) {
            // Spec 6.7's "bzl closure hash changed" branch, keyed on
            // `rbuildfiles` membership instead of a stored hash, and
            // load-bearing rather than an optimisation.
            //
            // A `.bzl` edit that changes a rule implementation without changing
            // any BUILD attribute leaves every rule proto in the loading
            // packages byte-identical — Bazel folds a rule's definition
            // environment into the proto, not the loading BUILD file's load
            // closure. So every rule hash comes back unchanged and `pd.changed`
            // is empty. Marking every rule in the package direct regardless of
            // its hash is the only thing that catches it, and
            // `Packages::bzl_closure_hash` is written as zero for exactly that
            // reason: `rbuildfiles` names the same set the hash comparison
            // would have. (Covered by the `edit bzl` end-to-end scenario, which
            // asserts this is *why* those targets are reported.)
            for &id in &pd.current {
                if view.is_rule(id) {
                    add(
                        id,
                        DirectReason::BzlClosure {
                            package: pd.package.clone(),
                        },
                        &mut direct,
                        &mut reasons,
                    );
                }
            }
        } else {
            for &id in &pd.changed {
                add(id, DirectReason::RuleHash, &mut direct, &mut reasons);
            }
        }
        for &id in &pd.deleted {
            let label = view.label(id).to_string();
            let consumers = view.reverse_row(id, &mut scratch).to_vec();
            for consumer in consumers {
                add(
                    consumer,
                    DirectReason::DeletedDep {
                        label: label.clone(),
                    },
                    &mut direct,
                    &mut reasons,
                );
            }
        }
    }

    for (pkg, ids) in deleted_by_package {
        for &id in ids {
            let label = view.label(id).to_string();
            tracing::debug!(package = %pkg, %label, "package deleted");
            let consumers = view.reverse_row(id, &mut scratch).to_vec();
            for consumer in consumers {
                add(
                    consumer,
                    DirectReason::DeletedDep {
                        label: label.clone(),
                    },
                    &mut direct,
                    &mut reasons,
                );
            }
        }
    }

    // A file node's in-edges all come from rules, so its reverse row is its
    // consumer list.
    for (id, path) in changed_file_ids {
        let consumers = view.reverse_row(*id, &mut scratch).to_vec();
        for consumer in consumers {
            add(
                consumer,
                DirectReason::File { path: path.clone() },
                &mut direct,
                &mut reasons,
            );
        }
    }

    let mut direct: Vec<u32> = direct.into_iter().collect();
    direct.sort_unstable();
    (direct, reasons)
}

#[allow(clippy::too_many_arguments)]
fn finish_report<V: GraphView + ?Sized>(
    view: &V,
    req: &AffectedRequest,
    dirty: &DirtySet,
    direct: &[u32],
    result: &walk::WalkResult,
    reasons: &FxHashMap<u32, DirectReason>,
    timings: Timings,
    stats: Stats,
) -> AffectedReport {
    AffectedReport {
        schema: crate::report::SCHEMA_VERSION,
        base: req.base.clone(),
        head: req.head.clone(),
        global: false,
        global_reason: None,
        dirty_packages: dirty.dirty.iter().cloned().collect(),
        direct: cambium_index::view::labels_sorted(view, direct),
        affected: affected_targets(view, result),
        tests: cambium_index::view::labels_sorted(view, &result.tests),
        timings_ms: timings,
        stats,
        explanations: req.explain.then(|| explain(view, result, reasons)),
    }
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
        dirty_packages: Vec::new(),
        direct: Vec::new(),
        affected: Vec::new(),
        tests: Vec::new(),
        timings_ms: timings,
        stats,
        explanations: None,
    }
}

/// Fallback key for stores that cannot be listed: no lineage in the path,
/// because a fetcher has no way to learn it first.
fn tree_addressed_key(tree: &str) -> String {
    format!("indexes/by-tree/{tree}.bin")
}

/// The sha-addressed pointer for unlistable stores.
fn commit_addressed_pointer(commit: &str) -> String {
    format!("commits/by-commit/{commit}")
}

/// The key the previous, sha-addressed layout wrote for unlistable stores.
/// Still read, never written.
fn commit_addressed_key(commit: &str) -> String {
    format!("indexes/by-commit/{commit}.bin")
}

/// Local cache file name for a store key.
fn cache_name(key: &str) -> String {
    key.replace('/', "_")
}

/// Counts by node kind, for `index inspect`.
pub fn kind_histogram(v: IndexRef<'_>) -> HashMap<&'static str, u32> {
    let mut h = HashMap::new();
    for id in 0..v.num_nodes() {
        let k = match v.kind_of(id) {
            NodeKind::Rule => "rule",
            NodeKind::SourceFile => "source_file",
            NodeKind::GeneratedFile => "generated_file",
            NodeKind::PackageGroup => "package_group",
            NodeKind::EnvironmentGroup => "environment_group",
            NodeKind::Unknown => "unknown",
            NodeKind::Tombstone => "tombstone",
        };
        *h.entry(k).or_insert(0) += 1;
    }
    h
}
