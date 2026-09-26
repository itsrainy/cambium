//! The run: load a baseline, work out what is dirty, re-query it, splice, walk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cambium_bazel::Bazel;
use cambium_diff::{Change, Repo};
use cambium_index::{IndexBuilder, IndexData, IndexFile, IndexRef};

use crate::baseline::BaselineCache;
use crate::config::Config;
use crate::error::{EngineError, Result};
use crate::progress::Progress;
use crate::query::{BazelQuerier, FileSource, PackageQuerier};
use crate::report::{AffectedReport, Stopwatch};
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

    /// What this baseline is, as the thing a caller can archive: a path when
    /// the store served the bytes, the index itself when the run computed it.
    ///
    /// Consumes the baseline, because an `Owned` one is a few hundred
    /// megabytes and the alternative is copying it.
    pub fn into_base_index(self) -> BaseIndex {
        match self {
            Baseline::Mapped(f) => BaseIndex::Mapped(f.path().to_path_buf()),
            Baseline::Owned(d) => BaseIndex::Owned(d),
        }
    }
}

impl Baseline {
    /// The index's lineage as hex, for a line that says which family of
    /// indexes this run is splicing onto.
    pub fn lineage_hex(&self) -> String {
        crate::inspect::hex(&self.view().lineage)
    }

    /// How long ago the index was built, in seconds, from the provenance it
    /// carries. `None` when the clock disagrees with it (a machine whose time
    /// moved, an index published from the future).
    pub fn age_secs(&self) -> Option<u64> {
        let created = self.view().created_unix;
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
            .and_then(|now| now.checked_sub(created))
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

/// Which base index a run actually spliced onto.
///
/// A replay away from the machine that ran it has only the index at the pin to
/// work with, and that is not what the run read: the store serves whatever the
/// walk left behind at the change set's own base, and a bug can be visible
/// against one and invisible against the other. An inert `MODULE.bazel` read in
/// WORKSPACE mode was exactly that. So the run says which base it used, and the
/// benchmark driver archives it beside the results.
pub enum BaseIndex {
    /// The store served this file and the run mmapped it; the bytes are still
    /// there to be copied.
    Mapped(PathBuf),
    /// The run computed the base itself — an ancestor brought forward, or a
    /// cold build — so the only copy is this one, in memory.
    Owned(Box<IndexData>),
}

impl BaseIndex {
    /// Whether the base index has a node for `label`. `None` when the mapped
    /// file can no longer be read: the cache may have been pruned since.
    pub fn has_label(&self, label: &str) -> Option<bool> {
        match self {
            BaseIndex::Owned(d) => Some(d.view().id_of(label).is_some()),
            BaseIndex::Mapped(path) => IndexFile::open(path)
                .ok()
                .map(|f| f.view().id_of(label).is_some()),
        }
    }
}

impl std::fmt::Debug for BaseIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BaseIndex::Mapped(p) => f.debug_tuple("Mapped").field(&p.display()).finish(),
            BaseIndex::Owned(d) => f
                .debug_struct("Owned")
                .field("commit", &d.view().commit)
                .finish(),
        }
    }
}

/// What a publish may do with an index Bazel could not load all of.
///
/// The config's `baseline.publish_partial` is the standing answer; this is
/// the per-call one (`--allow-partial`), and either being set is enough.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishPolicy {
    pub allow_partial: bool,
}

/// An index to publish: one the store can take from disk as it is, or one
/// still in memory that has to be written out first.
pub enum PublishSource<'a> {
    File {
        path: &'a Path,
        lineage: &'a [u8; 16],
        commit: &'a str,
    },
    Memory(&'a IndexData),
}

/// A completed run: the answer, plus the head index it produced.
pub struct RunOutcome {
    pub report: AffectedReport,
    /// Index at head. Present unless the run was globally invalidated: before
    /// a splice happened, or by what the splice found (a toolchain declaration
    /// moving), in which case the spliced index goes with the answer too.
    pub index: Option<IndexData>,
    /// The base this run spliced onto. Only [`Engine::affected`] knows it —
    /// the pipeline below is handed a view and never learns where it came
    /// from — so every other entry point leaves it `None`. It is deliberately
    /// not in [`AffectedReport`]: that JSON is a contract, and this is a local
    /// fact about one machine's cache.
    pub base_index: Option<BaseIndex>,
}

/// Whether a label exists at either end of a run, for `cambi explain` to
/// tell "not affected" from "no such target".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelPresence {
    /// `None` when the run did not materialise a head index.
    pub at_head: Option<bool>,
    /// `None` when the run kept no base, or its file can no longer be read.
    pub at_base: Option<bool>,
}

impl RunOutcome {
    /// Looks `label` up in the two graphs this run had.
    pub fn label_presence(&self, label: &str) -> LabelPresence {
        LabelPresence {
            at_head: self.index.as_ref().map(|i| i.view().id_of(label).is_some()),
            at_base: self.base_index.as_ref().and_then(|b| b.has_label(label)),
        }
    }
}

/// What `index update` does when the range invalidates everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnGlobal {
    /// Mint a fresh lineage with a cold build at the target revision.
    Rebuild,
    /// Fail with [`EngineError::RangeInvalidated`].
    Fail,
}

/// The index `index update` produced.
pub struct IndexUpdate {
    pub index: IndexData,
    /// Set when the range invalidated everything and the index is a cold
    /// build rather than the base brought forward; the reason the run gave.
    pub rebuilt_because: Option<String>,
    /// What the run that brought it forward said (a refused index, a store
    /// read as empty), which the report it came from would otherwise take
    /// with it. A rebuild's own findings are still in the engine's sink.
    pub diagnostics: Vec<crate::diag::Diagnostic>,
}

/// The parts are the crate's to reach into — the stages in `run`, `plan` and
/// `probe` are written as `impl Engine` blocks of their own — and a caller's
/// to read through the accessors below and set through the builders.
pub struct Engine {
    pub(crate) cfg: Config,
    pub(crate) repo: Repo,
    pub(crate) bazel: Bazel,
    pub(crate) store: Option<Box<dyn ArtifactStore>>,
    /// Where every actionable warning this run raises lands. Drained into the
    /// report at the end of [`Engine::affected`]; a caller that drives
    /// `compute` itself drains it itself.
    pub(crate) diagnostics: crate::diag::Diagnostics,
    cache_dir: PathBuf,
}

/// Where fetched indexes, revision worktrees and publish scratch files go
/// when neither the config nor the caller says.
pub fn default_cache_dir() -> PathBuf {
    std::env::temp_dir().join("cambium-cache")
}

impl Engine {
    /// Opens an engine rooted at the Bazel workspace containing `start`, with
    /// `bazel_binary` as the one command-line override.
    pub fn open(
        start: &Path,
        config_path: Option<&Path>,
        bazel_binary: Option<&str>,
    ) -> Result<Self> {
        let over = crate::config::Overrides {
            bazel_binary: bazel_binary.map(|b| {
                (
                    b.to_string(),
                    crate::config::Origin::Flag {
                        name: "--bazel-binary".into(),
                    },
                )
            }),
            cache_dir: None,
        };
        Self::open_with(start, config_path, &over)
    }

    /// Opens an engine rooted at the Bazel workspace containing `start`, over
    /// the whole configuration stack: defaults, `cambium.toml`,
    /// `cambium.local.toml`, then `over` — the flags and environment
    /// variables the caller carries, with the origin of each.
    ///
    /// The overrides are folded in as the highest layer, which is why the
    /// binary and the cache directory are taken off the effective config
    /// below rather than from the parameters: that is how a local file
    /// reaches a real run and not only `cambi config explain`.
    pub fn open_with(
        start: &Path,
        config_path: Option<&Path>,
        over: &crate::config::Overrides,
    ) -> Result<Self> {
        let mut bazel =
            Bazel::discover(start, over.bazel_binary.as_ref().map(|(b, _)| b.as_str()))?;
        let workspace = bazel.workspace().to_path_buf();
        let cfg = Config::load_layered(&workspace, config_path, over)?.config;
        if !cfg.bazel.binary.is_empty() {
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
        let diagnostics = crate::diag::Diagnostics::new();
        for d in cfg.removed_key_diagnostics() {
            diagnostics.emit(d);
        }
        let store = store::from_config(&cfg.baseline.store, bazel.workspace(), &diagnostics);
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
            diagnostics,
            cache_dir,
        }
    }

    /// Overrides the cache directory (`--cache-dir` on the command line).
    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    /// Replaces the Bazel every query runs through: the benchmark's checkout
    /// adds the bazelisk environment that pins a subject to one version.
    pub fn with_bazel(mut self, bazel: Bazel) -> Self {
        self.bazel = bazel;
        self
    }

    /// The same engine under another configuration.
    ///
    /// The store is what the new configuration says, as it would be on
    /// [`Engine::open`], and the diagnostics start over; the repository,
    /// the Bazel and the cache directory — an explicit override included —
    /// are kept. For a test that flips one knob between two runs.
    pub fn with_config(self, cfg: Config) -> Self {
        Self {
            cache_dir: self.cache_dir,
            ..Self::with_parts(cfg, self.repo, self.bazel)
        }
    }

    /// The effective configuration: defaults, files and overrides folded.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The git repository the workspace is in.
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// The Bazel every query runs through.
    pub fn bazel(&self) -> &Bazel {
        &self.bazel
    }

    /// The index store `[baseline].store` configured, if any.
    pub fn store(&self) -> Option<&dyn ArtifactStore> {
        self.store.as_deref()
    }

    /// Where the actionable warnings land; see the field.
    pub fn diagnostics(&self) -> &crate::diag::Diagnostics {
        &self.diagnostics
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
    /// clone does not have) is left to the working tree — not because that is
    /// certainly right, but because it is the only checkout there is: `git
    /// worktree add` on a revision git cannot name would fail, and failing the
    /// whole run over it would break every synthetic test, which uses ids like
    /// `"base"` that were never commits. On a real repository an unresolvable
    /// revision means the caller passed a sha this clone does not have, and
    /// the run goes on to fail at `git diff` a moment later with a message
    /// that actually says so.
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
                    diags: self.diagnostics.clone(),
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
                diags: self.diagnostics.clone(),
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
        // Where `//...` stops, and which of those directories a repository
        // rule of this tree claims to be. Both are read from the checkout,
        // and the probe that checks the claims runs here, *before* the
        // universe query: it is a Bazel command with its own option set, and
        // one of those between the universe query and the layer dump below
        // costs that dump packages. See `crate::repos`.
        let mut boundaries = q
            .workspace()
            .map(cambium_index::repo_boundaries)
            .unwrap_or_default();
        let confirmed = crate::repos::confirm_dir_claims(q, &self.cfg.externals, &boundaries);
        // Phase 5 stage 2 widens the universe here, to `//...:*` plus one
        // `@name//...:*` per confirmed in-tree repository, so that their
        // targets are nodes of this index under their real labels and the
        // reverse walk crosses back into `//...` through edges that are
        // leaves today.
        //
        // Conversion keeps those rows now: an external package's key is
        // `@repo//pkg`, which is Bazel's own spelling for it, and
        // `cambium_index::label::package_key` mints one for exactly the
        // repositories named here and drops every other repository's targets
        // as before. Each key travels back out through
        // `label::key_pattern` when the package is re-queried, so a refresh
        // asks for the same universe the cold build wrote.
        let indexed: Vec<String> = confirmed.iter().map(|(r, _)| r.clone()).collect();
        let u = q.universe_with(&indexed)?;
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.set_bazel_version(q.version());
        b.set_config_hash(self.cfg.hash());
        if let Some(e) = &u.partial {
            b.set_partial(e.count, e.packages.clone());
        }
        let packages: Vec<String> = u.updates.iter().map(|p| p.package.clone()).collect();
        // Which of those directories `//...` walked into after all. Read from
        // the universe answer rather than from the marker the directory
        // carries: Bazel 8.7 loads main-repo packages under xla's
        // `third_party/tsl/REPO.bazel`, and a boundary the main repository
        // owns packages under is not a hole in the graph.
        cambium_index::mark_traversed(&mut boundaries, packages.iter().map(String::as_str));
        b.apply_packages(&u.updates);
        // Recorded whatever the externals stage is configured to do: a
        // phantom package is a wrong answer, not an optimisation.
        b.set_repo_boundaries(boundaries.clone());
        // The loading layer, on the server the universe query has just loaded
        // the whole repository into. It has to be this server: a fresh one has
        // an empty Skyframe graph and would dump nothing.
        //
        // And it has to be *now*, before anything else is asked of that
        // server. A Bazel command whose option set differs from the last one's
        // re-evaluates, and a command that runs its own Skyframe evaluation
        // drops what it no longer needs; the two that used to sit between the
        // universe query and this dump -- `mod dump_repo_mapping` and
        // `query //external:* --output=build`, both from the closure rescan
        // below -- each cost this dump 19 of apollo's 644 packages, measured
        // one at a time on a fresh output base. Neither needs package values,
        // so the dump goes first and they see the server it leaves behind.
        if let Some(l) =
            crate::sky::build_layer(q, None, cambium_index::sky::SKYKEY_FILTER, None, &indexed)?
        {
            b.set_sky_layer(l);
        }
        if let Some(mut c) = crate::bzl::rescan_closure(
            q,
            None,
            &packages,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &crate::bzl::dirs(&boundaries),
            crate::bzl::InTreeRepos {
                confirmed: &confirmed,
                indexed: &indexed,
                consumers: &BTreeMap::new(),
            },
        ) {
            // Who consumes each indexed repository, asked of Bazel last: two
            // queries per repository, after the dump and the closure's own
            // commands, on the server they leave behind; then a third for
            // which of each consumer's leaves reach what.
            let table = q.consumer_repos(&indexed);
            for (r, cs) in &table {
                tracing::debug!(
                    repo = %r,
                    consumers = ?cs.keys().collect::<Vec<_>>(),
                    "external repositories consume an indexed repository"
                );
            }
            c.inputs.consumers = crate::consumers::with_leaves(
                q,
                table,
                &crate::consumers::external_labels(&b),
                &c.inputs.canonical,
                &Default::default(),
            );
            b.set_bzl_closure(c);
        }
        // Where the leaves lead back into the universe, asked last of all:
        // the closure of every leaf, which on a server that has loaded the
        // repository costs the external packages it reaches and nothing else.
        // Only with the externals stage on, which is what asks again at head.
        if self.config().externals.enabled {
            crate::round_trip::refresh(q, &mut b, &indexed, None, crate::round_trip::Want::All);
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
        self.store.as_deref().map(|s| {
            BaselineCache::new(s, &self.cache_dir, self.cfg.hash())
                .with_diagnostics(self.diagnostics.clone())
                .with_lineage_less_copies(self.cfg.baseline.lineage_less_copies)
        })
    }

    /// Finds an index for `commit` in the artifact store and caches it
    /// locally. See [`BaselineCache::fetch`] for how a commit becomes a key.
    pub fn fetch_baseline(&self, commit: &str) -> Result<Option<PathBuf>> {
        match self.baselines() {
            Some(c) => c.fetch(commit, &self.tree_of(commit)),
            None => Ok(None),
        }
    }

    /// `cambi index gc`: drops the store's stale lineages (unless
    /// `keep_incompatible`) and everything older than
    /// `baseline.retention_days`, sparing `keep`. See
    /// [`BaselineCache::collect`].
    pub fn collect_garbage(
        &self,
        keep: &str,
        keep_incompatible: bool,
    ) -> Result<crate::baseline::Collected> {
        let Some(cache) = self.baselines() else {
            return Err(EngineError::NoStore);
        };
        cache.collect(self.cfg.baseline.retention_days, keep, keep_incompatible)
    }

    /// Publishes an index under every key the store layout wants for its
    /// commit (see [`BaselineCache::publish`]), and returns them.
    ///
    /// The one implementation behind every publish. A partial index — one
    /// whose query finished with load errors — is refused unless the policy
    /// or `baseline.publish_partial` allows it: publishing it would hand
    /// every later run a base that silently lacks packages. The config's
    /// answer is a policy, not a boolean — it can name the packages whose
    /// absence is expected, and a failure it does not name is refused even
    /// though some partial index would have been allowed. An index still in
    /// memory is written to the cache directory first and the scratch file
    /// removed afterwards, whatever the store said.
    pub fn publish(&self, source: PublishSource<'_>, policy: PublishPolicy) -> Result<Vec<String>> {
        let Some(cache) = self.baselines() else {
            return Err(EngineError::NoStore);
        };
        match source {
            PublishSource::Memory(index) => {
                let commit = index.provenance.commit.clone();
                let dir = self.cache_dir.join("publish");
                std::fs::create_dir_all(&dir).map_err(EngineError::io_at(&dir))?;
                let path = dir.join(format!("{commit}.bin"));
                cambium_index::format::write_index_with(&path, index, self.index_compression())?;
                let names = self.publish(
                    PublishSource::File {
                        path: &path,
                        lineage: &index.header.lineage,
                        commit: &commit,
                    },
                    policy,
                );
                let _ = std::fs::remove_file(&path);
                names
            }
            PublishSource::File {
                path,
                lineage,
                commit,
            } => {
                if !policy.allow_partial {
                    let f = IndexFile::open(path)?;
                    let load = crate::inspect::PartialLoad::from(f.view());
                    if load.partial {
                        use crate::config::PartialVerdict;
                        match self
                            .cfg
                            .baseline
                            .publish_partial
                            .verdict(load.packages_in_error, &load.packages)
                        {
                            PartialVerdict::Allow => {}
                            PartialVerdict::Refuse => {
                                return Err(EngineError::PartialIndex {
                                    commit: commit.to_string(),
                                    packages_in_error: load.packages_in_error,
                                })
                            }
                            PartialVerdict::Unexpected(packages) => {
                                return Err(EngineError::PartialIndexUnexpected {
                                    commit: commit.to_string(),
                                    packages,
                                })
                            }
                            PartialVerdict::Unverifiable { count, named } => {
                                return Err(EngineError::PartialIndexUnverifiable {
                                    commit: commit.to_string(),
                                    count,
                                    named,
                                })
                            }
                        }
                    }
                }
                cache.publish(path, lineage, commit, &self.tree_of(commit))
            }
        }
    }

    /// Publishes the index file at `path` under the default policy: a partial
    /// index is refused unless `baseline.publish_partial` says otherwise.
    pub fn publish_index(
        &self,
        path: &Path,
        lineage: &[u8; 16],
        commit: &str,
    ) -> Result<Vec<String>> {
        self.publish(
            PublishSource::File {
                path,
                lineage,
                commit,
            },
            PublishPolicy::default(),
        )
    }

    /// Locates a usable baseline for `base` (spec 6.5): the store, then
    /// first-parent ancestors brought forward, then a cold build.
    fn resolve_baseline(
        &self,
        base: &str,
        req: &AffectedRequest,
        on: &mut dyn FnMut(Progress),
    ) -> Result<(Baseline, BaselineOrigin)> {
        let sw = Stopwatch::start();
        on(Progress::IndexLookup {
            base: base.to_string(),
        });
        if let Some(found) = self.fetch_mapped(base)? {
            narrate_found_at_base(&found.0, base, sw.elapsed_ms(), on);
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
        self.replay_or_build(base, req, &self.bazel_querier(base)?, on, sw)
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
        self.resolve_baseline_with_progress(base, req, querier, &mut crate::progress::quiet)
    }

    /// As [`Engine::resolve_baseline_with`], narrating the search: where it
    /// looked, every ancestor it checked and came up empty on, and what it
    /// finally found. This is the half of a run that takes the time on a
    /// repository with no index at the base.
    pub fn resolve_baseline_with_progress(
        &self,
        base: &str,
        req: &AffectedRequest,
        querier: &dyn PackageQuerier,
        on: &mut dyn FnMut(Progress),
    ) -> Result<(Baseline, BaselineOrigin)> {
        let sw = Stopwatch::start();
        on(Progress::IndexLookup {
            base: base.to_string(),
        });
        if let Some(found) = self.fetch_mapped(base)? {
            narrate_found_at_base(&found.0, base, sw.elapsed_ms(), on);
            return Ok(found);
        }
        self.replay_or_build(base, req, querier, on, sw)
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
        on: &mut dyn FnMut(Progress),
        search: Stopwatch,
    ) -> Result<(Baseline, BaselineOrigin)> {
        let max = self.cfg.baseline.max_ancestor_walk;
        on(Progress::SearchStart {
            base: base.to_string(),
            ancestors: max,
        });
        let mut checked = 0usize;
        for anc in self.repo.first_parent_ancestors(base, max)? {
            checked += 1;
            let Some(path) = self.fetch_baseline(&anc)? else {
                on(Progress::AncestorChecked {
                    checked,
                    of: max,
                    sha: anc.clone(),
                });
                continue;
            };
            let f = IndexFile::open(&path)?;
            self.check_compatible(f.view(), &path)?;
            tracing::info!(
                ancestor = %anc,
                "no baseline at base; bringing an ancestor forward"
            );
            let found = Baseline::Mapped(f);
            on(Progress::IndexFound {
                sha: anc.clone(),
                // The ancestors are walked nearest first, so the one that
                // answered is `base~checked`.
                behind: checked as u32,
                lineage: found.lineage_hex(),
                age_secs: found.age_secs(),
                elapsed_ms: search.elapsed_ms(),
            });
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
            on(Progress::SpliceStart {
                from: anc.clone(),
                to: base.to_string(),
                commits: checked,
            });
            let splice = Stopwatch::start();
            let out = self.compute(found.view(), &changes, querier, &inner)?;
            return match out.index {
                Some(idx) => {
                    let elapsed_ms = splice.elapsed_ms();
                    let r = &out.report;
                    on(Progress::SpliceStep {
                        step: Box::new(crate::profile::CommitStep {
                            commit: base.to_string(),
                            parent: anc.clone(),
                            date: self.commit_date(base),
                            subject: String::new(),
                            changed_files: r.stats.changed_files,
                            dirty_packages: r.stats.dirty_packages,
                            affected: r.affected.len(),
                            tests: r.tests.len(),
                            wall_ms: elapsed_ms,
                            global: false,
                            global_reason: None,
                            global_cause: None,
                            engine_cold_rebuild: r.stats.cold_rebuild,
                            cold_after_failure: false,
                            failed: None,
                            index: crate::profile::IndexWork::Spliced {
                                rows_rewritten: r.stats.spliced_rows,
                            },
                        }),
                    });
                    on(Progress::SpliceDone {
                        at: base.to_string(),
                        rows_rewritten: r.stats.spliced_rows,
                        elapsed_ms,
                    });
                    Ok((
                        Baseline::Owned(Box::new(idx)),
                        BaselineOrigin::Replayed { from: anc },
                    ))
                }
                // The ancestor..base range invalidated everything, so the
                // ancestor's index cannot be brought forward.
                None => {
                    on(Progress::SpliceSkipped {
                        reason: out
                            .report
                            .global_reason
                            .clone()
                            .unwrap_or_else(|| "the range invalidated everything".into()),
                    });
                    self.cold_baseline(base, req, querier, on)
                }
            };
        }
        on(Progress::IndexAbsent {
            searched: checked,
            elapsed_ms: search.elapsed_ms(),
        });
        self.cold_baseline(base, req, querier, on)
    }

    /// The commit date as a row prints it, or an empty cell when git cannot
    /// say (a synthetic revision in the tests).
    fn commit_date(&self, rev: &str) -> String {
        self.repo
            .first_parent_log_meta(rev, 1)
            .ok()
            .and_then(|m| m.into_iter().next())
            .map(|m| m.date)
            .unwrap_or_default()
    }

    fn cold_baseline(
        &self,
        base: &str,
        req: &AffectedRequest,
        querier: &dyn PackageQuerier,
        on: &mut dyn FnMut(Progress),
    ) -> Result<(Baseline, BaselineOrigin)> {
        if req.no_full_build {
            return Err(EngineError::BaselineUnavailable {
                base: base.to_string(),
                store_configured: self.store.is_some(),
            });
        }
        on(Progress::ColdBuild {
            commit: base.to_string(),
        });
        let sw = Stopwatch::start();
        let index = self.full_build_with(base, querier)?;
        on(Progress::ColdBuilt {
            targets: index.view().num_nodes(),
            elapsed_ms: sw.elapsed_ms(),
        });
        on(Progress::SpliceSkipped {
            reason: "cold index is at base".into(),
        });
        Ok((Baseline::Owned(Box::new(index)), BaselineOrigin::ColdBuild))
    }

    /// How this run writes indexes it publishes or caches: a zstd frame
    /// unless `baseline.compress_indexes` is off. The key and the header are
    /// the same either way, and a reader takes both forms.
    pub fn index_compression(&self) -> cambium_index::format::Compression {
        if self.cfg.baseline.compress_indexes {
            cambium_index::format::Compression::default()
        } else {
            cambium_index::format::Compression::None
        }
    }

    /// Publishes a base index this run had to compute, under the base commit.
    ///
    /// Only ever the base. The head-side index of an `affected` run is not
    /// published: a pull-request head is not on the main line, publishing it
    /// would fill the store with indexes nothing will ever ask for again, and
    /// an `affected` run does not even materialise one. `baseline update` is
    /// what publishes head indexes.
    pub fn publish_base_index(&self, index: &IndexData) -> Result<Vec<String>> {
        self.publish(PublishSource::Memory(index), PublishPolicy::default())
    }

    /// Publishes a base index that is already a file on disk (a mapped
    /// baseline), or one held in memory.
    pub fn publish_baseline(&self, base: &Baseline, allow_partial: bool) -> Result<Vec<String>> {
        let policy = PublishPolicy { allow_partial };
        match base {
            Baseline::Owned(d) => self.publish(PublishSource::Memory(d), policy),
            Baseline::Mapped(f) => {
                let v = f.view();
                self.publish(
                    PublishSource::File {
                        path: f.path(),
                        lineage: &v.lineage,
                        commit: v.commit,
                    },
                    policy,
                )
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
        self.affected_narrated(req, &mut crate::progress::quiet)
    }

    /// As [`Engine::affected`], telling `on` what it is doing while it does
    /// it. Nothing here prints: the CLI renders the events through
    /// `cambium_ui`, and `affected` itself passes a callback that drops them.
    pub fn affected_narrated(
        &self,
        req: &AffectedRequest,
        on: &mut dyn FnMut(Progress),
    ) -> Result<RunOutcome> {
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

        let (base_index, origin) = self.resolve_baseline(&base, &req, on)?;
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
                    Err(e) => {
                        tracing::warn!(%base, error = %e, "could not publish the base index");
                        self.diagnostics.emit(
                            crate::diag::Diagnostic::warning(
                                crate::diag::Code::W011PublishFailed,
                                format!(
                                    "the base index for {base} was computed but could not be \
                                     published: {e}"
                                ),
                            )
                            .with_help(
                                "the answer is unaffected; check that [baseline].store is \
                                 writable, or the next run recomputes the base",
                            ),
                        );
                    }
                },
            }
        }
        let load_ms = sw.lap();

        on(Progress::QueryStart);
        let query = Stopwatch::start();
        let mut out = self.run_incremental(base_index.view(), &req)?;
        let r = &out.report;
        match (&r.global, &r.global_reason) {
            (true, reason) => on(Progress::Global {
                reason: reason.clone().unwrap_or_else(|| "global".into()),
            }),
            _ => on(Progress::QueryDone {
                changed_files: r.stats.changed_files,
                packages: r.stats.dirty_packages,
                affected: r.affected.len(),
                tests: r.tests.len(),
                elapsed_ms: query.elapsed_ms(),
            }),
        }
        out.report.timings_ms.load += load_ms;
        out.report.timings_ms.total = total.elapsed_ms();
        // The view is dead by here, so the baseline can be given away whole:
        // the caller gets the path the store served or the index this run
        // computed, and nothing is copied either way.
        out.base_index = Some(base_index.into_base_index());
        // The sink is the run's, so this is everything every stage raised,
        // including the ones that happened before the report existed.
        out.report.diagnostics = self.diagnostics.drain();
        Ok(out)
    }

    /// Brings an index forward from `req.base` to `req.head`: the run with
    /// `materialize` on, and — when the range invalidates everything and
    /// `on_global` allows — a cold build at head in its place.
    ///
    /// `cambi index update` is this; the choice between rebuilding and
    /// failing is the caller's, the rebuild itself is not.
    pub fn update_index(&self, req: &AffectedRequest, on_global: OnGlobal) -> Result<IndexUpdate> {
        let req = AffectedRequest {
            materialize: true,
            ..req.clone()
        };
        let outcome = self.affected(&req)?;
        let diagnostics = outcome.report.diagnostics;
        if let Some(index) = outcome.index {
            return Ok(IndexUpdate {
                index,
                rebuilt_because: None,
                diagnostics,
            });
        }
        let reason = outcome
            .report
            .global_reason
            .unwrap_or_else(|| "global".into());
        match on_global {
            OnGlobal::Fail => Err(EngineError::RangeInvalidated { reason }),
            OnGlobal::Rebuild => {
                tracing::info!(%reason, "the range invalidates everything; rebuilding a fresh lineage");
                let head = self
                    .repo
                    .rev_parse(&req.head)
                    .unwrap_or_else(|_| req.head.clone());
                Ok(IndexUpdate {
                    index: self.full_build(&head)?,
                    rebuilt_because: Some(reason),
                    diagnostics,
                })
            }
        }
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
    ///
    /// The stages themselves are in [`crate::run`].
    pub fn compute_with_texts(
        &self,
        base_view: IndexRef<'_>,
        changes: &[Change],
        querier: &dyn PackageQuerier,
        req: &AffectedRequest,
        texts: &dyn crate::externals::WorkspaceText,
    ) -> Result<RunOutcome> {
        crate::run::compute(self, base_view, changes, querier, req, texts)
    }
}

/// The index was in the store under the base itself: there is nothing to
/// replay, so the splice phase says so rather than going quiet.
fn narrate_found_at_base(
    found: &Baseline,
    base: &str,
    elapsed_ms: u64,
    on: &mut dyn FnMut(Progress),
) {
    on(Progress::IndexFound {
        sha: base.to_string(),
        behind: 0,
        lineage: found.lineage_hex(),
        age_secs: found.age_secs(),
        elapsed_ms,
    });
    on(Progress::SpliceSkipped {
        reason: "index already at base".into(),
    });
}

pub use crate::bzl::main_alias_mapping;
