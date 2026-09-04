//! The run: load a baseline, work out what is dirty, re-query it, splice, walk.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use cambium_bazel::Bazel;
use cambium_classify::{DirtySet, Matchers, PackageSet};
use cambium_diff::Repo;
use cambium_index::{
    format, walk, IndexBuilder, IndexData, IndexFile, IndexRef, NodeKind, WalkOptions,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::Config;
use crate::error::{EngineError, Result};
use crate::query::{BazelQuerier, FileSource, PackageQuerier};
use crate::report::{
    AffectedReport, DirectReason, Explanation, Stats, Stopwatch, Timings,
};
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
}

impl AffectedRequest {
    pub fn new(base: impl Into<String>, head: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            head: head.into(),
            include_manual: false,
            explain: false,
            no_full_build: false,
        }
    }
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
    pub fn open(start: &Path, config_path: Option<&Path>, bazel_binary: Option<&str>) -> Result<Self> {
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
            .with_extra_query_flags(cfg.bazel.extra_query_flags.clone());
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

    /// Finds an index for `commit` in the artifact store.
    pub fn fetch_baseline(&self, commit: &str) -> Result<Option<PathBuf>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        std::fs::create_dir_all(&self.cache_dir)?;
        let suffix = format!("-{commit}.bin");
        // Stores that can enumerate let us find the artifact whatever lineage
        // produced it. Those that cannot (plain HTTP) use a commit-addressed
        // name instead; the lineage is still recorded in the header.
        if let Ok(names) = store.list() {
            if let Some(name) = names.iter().find(|n| n.ends_with(&suffix)) {
                let dest = self.cache_dir.join(name);
                if store.fetch(name, &dest)? {
                    return Ok(Some(dest));
                }
            }
        }
        let name = commit_addressed_name(commit);
        let dest = self.cache_dir.join(&name);
        if store.fetch(&name, &dest)? {
            return Ok(Some(dest));
        }
        Ok(None)
    }

    /// Publishes an index under its canonical name, plus a commit-addressed
    /// name when the store cannot be listed.
    pub fn publish_index(&self, path: &Path, lineage: &[u8; 16], commit: &str) -> Result<Vec<String>> {
        let Some(store) = &self.store else {
            return Err(EngineError::NoStore);
        };
        let mut names = vec![format::index_file_name(lineage, commit)];
        if store.list().is_err() {
            names.push(commit_addressed_name(commit));
        }
        for n in &names {
            store.publish(n, path)?;
        }
        Ok(names)
    }

    /// Locates a usable baseline for `base` (spec 6.5): the store, then
    /// first-parent ancestors brought forward, then a cold build.
    fn resolve_baseline(&self, base: &str, req: &AffectedRequest) -> Result<(IndexData, String)> {
        if let Some(path) = self.fetch_baseline(base)? {
            let f = IndexFile::open(&path)?;
            self.check_compatible(f.view(), &path)?;
            tracing::info!(%base, path = %path.display(), "loaded baseline index");
            return Ok((f.deserialize()?, base.to_string()));
        }

        let max = self.cfg.baseline.max_ancestor_walk;
        for anc in self.repo.first_parent_ancestors(base, max)? {
            let Some(path) = self.fetch_baseline(&anc)? else {
                continue;
            };
            let f = IndexFile::open(&path)?;
            self.check_compatible(f.view(), &path)?;
            tracing::info!(ancestor = %anc, "no baseline at base; bringing an ancestor forward");
            // Same code path as a normal run: update the ancestor to `base`.
            let inner = AffectedRequest {
                base: anc.clone(),
                head: base.to_string(),
                ..req.clone()
            };
            let out = self.run_incremental(f.deserialize()?, &inner)?;
            return match out.index {
                Some(idx) => Ok((idx, base.to_string())),
                // The ancestor..base range invalidated everything, so the
                // ancestor's index cannot be brought forward.
                None => {
                    if req.no_full_build {
                        Err(EngineError::BaselineUnavailable {
                            base: base.to_string(),
                        })
                    } else {
                        Ok((self.full_build(base)?, base.to_string()))
                    }
                }
            };
        }

        if req.no_full_build {
            return Err(EngineError::BaselineUnavailable {
                base: base.to_string(),
            });
        }
        Ok((self.full_build(base)?, base.to_string()))
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
        let base = self.repo.rev_parse(&req.base).unwrap_or_else(|_| req.base.clone());
        let head = self.repo.rev_parse(&req.head).unwrap_or_else(|_| req.head.clone());
        let req = AffectedRequest {
            base: base.clone(),
            head: head.clone(),
            ..req.clone()
        };

        let (base_index, _) = self.resolve_baseline(&base, &req)?;
        let load_ms = sw.lap();

        let mut out = self.run_incremental(base_index, &req)?;
        out.report.timings_ms.load += load_ms;
        out.report.timings_ms.total = total.elapsed_ms();
        Ok(out)
    }

    /// The incremental pipeline, given a base index already in hand. Asks git
    /// what changed and queries Bazel for the dirty packages.
    pub fn run_incremental(&self, base_index: IndexData, req: &AffectedRequest) -> Result<RunOutcome> {
        let changes = self.repo.diff(&req.base, &req.head)?;
        self.compute(base_index, &changes, &self.bazel_querier(&req.head), req)
    }

    /// The pipeline proper: dirty set, query, splice, direct set, walk.
    ///
    /// Takes the diff and the row source as parameters so the whole engine can
    /// be exercised without git or Bazel (spec 12.4).
    pub fn compute(
        &self,
        base_index: IndexData,
        changes: &[cambium_diff::Change],
        querier: &dyn PackageQuerier,
        req: &AffectedRequest,
    ) -> Result<RunOutcome> {
        let mut sw = Stopwatch::start();
        let mut timings = Timings::default();
        let mut stats = Stats::default();

        let base_view = base_index.view();
        stats.base_nodes = base_view.num_nodes();

        // ---- dirty set ----
        stats.changed_files = changes.len();
        let matchers = Matchers::compile(&self.cfg.rules())?;
        let mut packages = PackageSet::from_index(base_view);
        let mut dirty = cambium_classify::dirty_set(
            changes,
            &mut packages,
            &matchers,
            self.cfg.limits.max_changed_files,
        );

        // `.bzl` files are not package inputs by path; Bazel resolves them.
        let mut bzl_dirty: BTreeSet<String> = BTreeSet::new();
        if !dirty.global && !dirty.changed_bzl.is_empty() {
            let paths: Vec<String> = dirty.changed_bzl.iter().cloned().collect();
            tracing::debug!(count = paths.len(), "resolving changed .bzl files with rbuildfiles");
            let pkgs = querier.rbuildfiles(&paths)?;
            for p in pkgs {
                bzl_dirty.insert(p.clone());
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
        let report = builder.apply_packages(&updates);
        let head_index = builder.finish(&req.head);
        let hv = head_index.view();
        stats.head_nodes = hv.num_nodes();
        stats.head_edges = hv.num_edges() as u64;
        timings.splice = sw.lap();

        // ---- direct set (spec 6.7) ----
        let mut reasons: FxHashMap<u32, DirectReason> = FxHashMap::default();
        let mut direct: FxHashSet<u32> = FxHashSet::default();
        let add = |id: u32, reason: DirectReason, direct: &mut FxHashSet<u32>, reasons: &mut FxHashMap<u32, DirectReason>| {
            if direct.insert(id) {
                reasons.insert(id, reason);
            }
        };

        for pd in &report.packages {
            if bzl_dirty.contains(&pd.package) {
                // The rule implementations themselves may have changed, so
                // every rule in the package is direct regardless of its hash.
                for &id in &pd.current {
                    if hv.is_rule(id) {
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
                let label = hv.label(id).to_string();
                for &consumer in hv.reverse.row(id) {
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
        for (pkg, ids) in &deleted_by_package {
            for &id in ids {
                let label = hv.label(id).to_string();
                tracing::debug!(package = %pkg, %label, "package deleted");
                for &consumer in hv.reverse.row(id) {
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

        if !dirty.direct_files.is_empty() {
            let labels = hv.labels.build_map();
            for path in &dirty.direct_files {
                let Some(pkg) = packages.owner_of(path) else {
                    continue;
                };
                let label = cambium_index::label::source_label(pkg, path);
                let Some(&id) = labels.get(label.as_str()) else {
                    // The most common "why wasn't my test run?" question: a file
                    // no rule references cannot affect anything.
                    tracing::debug!(%label, "changed file is not referenced by any rule");
                    continue;
                };
                for &consumer in hv.file_consumers(id) {
                    add(
                        consumer,
                        DirectReason::File {
                            path: path.clone(),
                        },
                        &mut direct,
                        &mut reasons,
                    );
                }
            }
        }

        let mut direct: Vec<u32> = direct.into_iter().collect();
        direct.sort_unstable();
        stats.direct_targets = direct.len();
        timings.direct = sw.lap();

        // ---- walk ----
        let wopts = WalkOptions {
            include_manual: req.include_manual,
            explain: req.explain,
        };
        let result = walk::walk(hv, &direct, &wopts);
        stats.affected_targets = result.affected.len();
        timings.walk = sw.lap();

        let explanations = req.explain.then(|| explain(hv, &result, &reasons));

        let report = AffectedReport {
            base: req.base.clone(),
            head: req.head.clone(),
            global: false,
            global_reason: None,
            dirty_packages: dirty.dirty.iter().cloned().collect(),
            direct: walk::labels_sorted(hv, &direct),
            affected: walk::labels_sorted(hv, &result.affected),
            tests: walk::labels_sorted(hv, &result.tests),
            timings_ms: timings,
            stats,
            explanations,
        };
        Ok(RunOutcome {
            report,
            index: Some(head_index),
        })
    }
}

/// `--explain`: for each affected target, the shortest path back to the direct
/// member that reached it, and why that member was direct.
fn explain(
    index: IndexRef<'_>,
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
            target: index.label(id).to_string(),
            path: path.iter().map(|&i| index.label(i).to_string()).collect(),
            direct: index.label(seed).to_string(),
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

fn commit_addressed_name(commit: &str) -> String {
    format!("cambium-index-{commit}.bin")
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
