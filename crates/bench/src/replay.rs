//! Offline replay: the engine's static stages, driven from a stored index and
//! a list of changed paths, with no Bazel anywhere.
//!
//! # Why
//!
//! A published run leaves behind, per subject, the index at the pin
//! (`bench-results/<subject>/index-<pin>.bin`) and what cambium answered on
//! every recorded change set and history commit. Between the diff and the
//! `bazel query` the engine does a substantial amount of work that needs
//! neither: which external repositories moved and by which rule, which
//! packages that dirties, whether the dirty set exceeds
//! `limits.max_dirty_packages` and the run therefore falls back to a cold
//! rebuild. Every regression the last two runs found lived in exactly that
//! stretch. Replaying it against a stored index costs milliseconds, so an
//! engine change can be checked against the real corpus before the box is
//! started rather than seven hours after.
//!
//! # What a replay cannot know
//!
//! Being honest about this is the point; a replay that quietly guessed would
//! be worse than no replay.
//!
//! - **The answer.** `direct` and `affected` come from comparing rule hashes
//!   after the head packages are re-queried. There is no query here, so
//!   [`ReplayOutcome::affected_lower_bound`] is what the reverse walk reaches
//!   from the changed files and external nodes the *base* index already knows
//!   — a lower bound, never the run's number, and never gated on.
//! - **A cold rebuild's answer.** Past the limit the engine re-queries
//!   `//...`. The replay reports *that the fallback would fire*, which is the
//!   decision under test; what it would then answer needs Bazel.
//! - **The base index.** The only stored index is the one at the pin, and a
//!   change set's base is an ancestor of it. Package and repository sets
//!   therefore reflect the graph at the pin, not the graph the run walked.
//!   That is why a divergence from a recorded answer is a *lead*, not a
//!   verdict, and why the tight loop compares one replay against another
//!   ([`ReplayRun::divergences_against`]) rather than against the run.
//!
//!   Two shapes of that drift are large enough to be worth naming rather
//!   than leaving for a reader to rediscover, and both are recorded as
//!   indeterminate when `--repo` makes them visible: the stage naming a
//!   repository the base's `MODULE.bazel` does not declare, and the diff
//!   modifying a `.bzl` the stored load closure has never heard of. envoy's
//!   three hundred commits straddle a Bzlmod migration and produce both.
//! - **`MODULE.bazel` at either revision**, without a git repository to read
//!   it from. A change set that touches it is marked
//!   [`ReplayOutcome::indeterminate`] and excluded from gating unless
//!   `--repo` is given.
//! - **A change to how the index derives its externals side tables.** The
//!   stage does not derive which main-repo paths feed which repositories; it
//!   reads tables `cambium_index::build_repo_inputs` wrote at index time, so a
//!   replay of a build that changed that derivation still replays the old
//!   one's output. [`rederive_externals`] — `--rederive` — closes that by
//!   rebuilding those tables from the repository text at each base before
//!   replaying. What stays the pin's is everything `bazel query` established:
//!   the packages, the targets, the edges, the package closures.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use cambium_diff::{Change, Status};
use cambium_engine::externals::{self, WorkspaceText};
use cambium_engine::Config;
use cambium_index::{walk, IndexRef};
use serde::{Deserialize, Serialize};

/// The schema of the JSON this module reads and writes.
pub const REPLAY_SCHEMA: u32 = 1;

// ------------------------------------------------------------- changed paths
//
// The recorded change sets carry `(base, head)` and nothing else: the diff was
// taken from the subject's git mirror at run time and never written down. A
// replay away from the box therefore needs the paths cached alongside the
// results, which is what `tools/bench/precheck.sh` fills in.

/// One `(base, head)` pair's diff, as `git diff --name-status` gave it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathEntry {
    pub base: String,
    pub head: String,
    /// `A`, `M`, `D` or `T`, then the path — the `--name-status` spelling, so
    /// the cache can be produced by a shell loop and read back exactly.
    pub changes: Vec<String>,
}

/// `bench-precheck/<subject>/paths-<pin>.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathsCache {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    pub entries: Vec<PathEntry>,
}

impl PathsCache {
    pub fn read(path: &std::path::Path) -> Result<Self> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let c: PathsCache =
            serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            c.schema == REPLAY_SCHEMA,
            "{} was written by schema {} and this build reads {REPLAY_SCHEMA}",
            path.display(),
            c.schema
        );
        Ok(c)
    }

    /// The diff for one pair, by either revision spelling the results use.
    pub fn lookup(&self, base: &str, head: &str) -> Option<Vec<Change>> {
        self.entries
            .iter()
            .find(|e| revs_match(&e.base, base) && revs_match(&e.head, head))
            .map(|e| parse_name_status(&e.changes))
    }
}

/// Two revisions name the same commit when one is a prefix of the other.
///
/// The results file and the paths cache are both written from full shas, but a
/// person passing `--change-set` types a short one.
fn revs_match(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// `["M\tsrc/a.cc", "D\tsrc/b.h"]` -> changes. A line the parser does not
/// recognise is read as a modification, which is the conservative reading and
/// matches [`cambium_diff::Status::TypeChanged`]'s own reasoning.
pub fn parse_name_status(lines: &[String]) -> Vec<Change> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let (status, path) = match line.split_once(['\t', ' ']) {
            Some((s, p)) => (s.trim(), p.trim()),
            // A bare path, which is what `--name-only` gives.
            None => ("M", line),
        };
        if path.is_empty() {
            continue;
        }
        let status = match status.chars().next().unwrap_or('M') {
            'A' => Status::Added,
            'D' => Status::Deleted,
            'T' => Status::TypeChanged,
            _ => Status::Modified,
        };
        out.push(Change::new(status, path));
    }
    out
}

// ---------------------------------------------------------------- the replay

/// Why a repository is dirty. The names are the rules
/// [`cambium_engine::externals`] documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoRule {
    /// A main-repo file a repository rule reads by label moved.
    InputPath,
    /// The only thing naming the changed path is a target the repository rule
    /// *writes*, so nothing it reads — no `.bzl` — can have moved.
    TargetOnly,
    /// The `MODULE.bazel` diff named it: a version bump, a changed tag.
    ModuleDiff,
    /// A changed workspace-closure `.bzl` defines it.
    Load,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoDirty {
    pub repo: String,
    pub rule: RepoRule,
    /// Whether rule (2) applies: the repository's `.bzl` files may have moved,
    /// so every package loading from it is dirty. False is the precision win
    /// issue #3 is about.
    pub loads_too: bool,
    /// The changed paths that named it, when a path did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// How a package came to be dirty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageRule {
    /// A changed file belongs to it.
    ChangedFile,
    /// A changed `.bzl` is in its load closure.
    BzlClosure,
    /// Rule (a)/(2) through a `.bzl` row: some `.bzl` the package loads
    /// reaches a dirty repository.
    ExternalBzlClosure,
    /// Rule (a)/(2) through `pkg_ext`: the package's own BUILD file `load()`s
    /// from a dirty repository directly, with no `.bzl` row in between. This
    /// is the route issue #4 says `LoadGraph::package_closure` drops.
    ExternalPkgExt,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageDirty {
    pub package: String,
    pub rule: PackageRule,
}

/// What replaying the static stages concluded.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReplayOutcome {
    pub base: String,
    pub head: String,
    pub changed_files: usize,
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    pub repos: Vec<RepoDirty>,
    /// Capped by `--max-packages` in the text rendering; the count below is
    /// always the whole set.
    pub packages: Vec<PackageDirty>,
    pub dirty_packages: usize,
    pub max_dirty_packages: usize,
    pub cold_rebuild: bool,
    /// External leaf nodes rule (1) seeds the direct set with.
    pub external_direct_targets: usize,
    /// What the reverse walk reaches from the changed files and external nodes
    /// the base index knows. A lower bound on `affected`; see the module docs.
    pub affected_lower_bound: usize,
    /// The replay could not decide something the run decided, so its verdict
    /// must not be compared. The strings say what and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indeterminate: Vec<String>,
    /// The externals side tables were rebuilt from the repository text at
    /// [`ReplayOutcome::base`] by this build, rather than read back out of the
    /// stored index as the run's cambium derived them. See
    /// [`rederive_externals`]. A re-derived replay and a plain one are not
    /// comparable as engine evidence — the derivation under them differs — so
    /// this travels in the JSON and in every divergence report.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rederived: bool,
    /// What the re-derivation cost, in milliseconds, when one happened. Zero
    /// for a change set that reused a cached rebuild of the same base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rederive_ms: Option<u64>,
}

impl ReplayOutcome {
    pub fn key(&self) -> String {
        format!("{}..{}", self.base, self.head)
    }

    /// Package counts by rule, for the text rendering.
    pub fn packages_by_rule(&self) -> BTreeMap<PackageRule, usize> {
        let mut m = BTreeMap::new();
        for p in &self.packages {
            *m.entry(p.rule).or_insert(0) += 1;
        }
        m
    }
}

/// A [`WorkspaceText`] that has nothing to read.
///
/// Handing this to the stage is not the same as handing it an empty
/// `MODULE.bazel`: [`cambium_engine::externals`] treats an unreadable file as
/// unparseable and escalates, which is exactly the "we cannot know" the caller
/// then records as indeterminate.
pub struct NoText;

impl WorkspaceText for NoText {
    fn text_at(&self, _rev: &str, _path: &str) -> Option<String> {
        None
    }
}

/// Whether the stored index's repository set is the one the base revision
/// actually had.
///
/// The index is at the pin and the change set's base is an ancestor of it, so
/// the graph the replay walks is always a little ahead of the graph the run
/// walked. Usually that is drift of a package or two and the answers still
/// line up. `MODULE.bazel` is different in kind: the externals stage's whole
/// input — which repositories exist, which main-repo paths feed them, which
/// of those have a derivable path mapping — is derived from it, so a base
/// whose module file declares a different set of repositories is not a
/// smaller version of the pin's graph but a different one, and comparing the
/// replay's answer against the run's says nothing about the engine.
///
/// envoy is the case that motivated this. Its three hundred history commits
/// straddle a Bzlmod migration: at the pin `MODULE.bazel` declares fifty-three
/// repositories, and two hundred and eighty-eight commits earlier it declares
/// three, all `local_path_override`s. Nine change sets replayed to a tenth of
/// the dirty set the run recorded, and every one of them is on the far side
/// of that migration. Replaying each against the base index the run actually
/// used reproduces the run's number exactly.
///
/// It is deliberately narrow. envoy's `MODULE.bazel` moves on most of those
/// three hundred commits, and declaring every change set indeterminate for
/// that would throw the gate away to catch nine cases. What is asked instead
/// is whether *this* change set's answer rests on the drift: a repository the
/// replay dirtied that the base never declared is one the run cannot have
/// dirtied, whatever the engine does.
///
/// Needs `--repo`; without a git repository to read `MODULE.bazel` at the
/// base there is nothing to compare and the replay says nothing.
fn repos_declared_at(base_rev: &str, texts: &dyn WorkspaceText) -> Option<BTreeSet<String>> {
    let text = texts.text_at(base_rev, externals::MODULE_FILE)?;
    let facts = cambium_index::scan_module(&text);
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for e in facts.extensions.values() {
        declared.extend(e.repos.iter().cloned());
    }
    for r in facts.repo_rules.values() {
        declared.extend(r.repos.iter().cloned());
    }
    for module in facts.local_paths.keys() {
        let name = facts
            .deps
            .values()
            .find(|d| &d.module == module)
            .map(|d| d.repo_name.clone())
            .unwrap_or_else(|| module.clone());
        declared.insert(name);
    }
    Some(declared)
}

// ------------------------------------------------------- re-derived externals
//
// The externals side tables in a stored index — the repository input patterns,
// `bzl_repos`, the path maps, `target_only`, the repository set — were derived
// at index time by `cambium_index::build_repo_inputs` from the text of
// `MODULE.bazel` / `WORKSPACE` and the workspace `.bzl` closure. A replay that
// reads them back is therefore blind to a change in that derivation: it
// replays the *old* derivation's output whatever the build under test does.
// `--rederive` closes that by rebuilding those tables from the repository text
// at the change set's base, with the code in this build, before replaying.

/// One revision of a [`WorkspaceText`], as the index's [`FileText`].
///
/// The git layer stays behind [`WorkspaceText`] so a test can re-derive from
/// strings with no git anywhere; the CLI passes the same `git show`-backed
/// implementation the rest of the replay uses.
pub struct RevisionText<'a> {
    pub texts: &'a dyn WorkspaceText,
    pub rev: String,
}

impl cambium_index::FileText for RevisionText<'_> {
    fn text(&self, path: &str) -> Option<String> {
        self.texts.text_at(&self.rev, path)
    }
}

/// Rebuilds a stored index's externals side tables from the repository text at
/// `rev`, with this build's derivation.
///
/// What is re-derived is exactly what [`cambium_index::build_repo_inputs`]
/// produces, plus which `.bzl` files the workspace closure contains. What is
/// kept is everything the run's `bazel query` established: the packages, the
/// targets, the edges, and the package -> `.bzl` closures. Those are the pin's
/// and no amount of reading text can recover them; see the module docs.
///
/// The `.bzl` ids are the stored index's, because the package closures are
/// expressed in them. The fresh walk gets its own [`LoadGraph`] and its
/// workspace closure is mapped back onto the stored ids by path, so a `.bzl`
/// in the base's workspace closure that the pin's index has never heard of is
/// dropped here — which is the drift the `indeterminate` guard already names.
pub fn rederive_externals(
    base: IndexRef<'_>,
    rev: &str,
    texts: &dyn WorkspaceText,
) -> Result<cambium_index::IndexData> {
    let src = RevisionText {
        texts,
        rev: rev.to_string(),
    };
    anyhow::ensure!(
        cambium_index::FileText::text(&src, externals::MODULE_FILE).is_some()
            || externals::LEGACY_WORKSPACE_FILES
                .iter()
                .any(|f| cambium_index::FileText::text(&src, f).is_some()),
        "no MODULE.bazel and no WORKSPACE at {rev}: nothing to re-derive the externals tables from"
    );

    let facts = cambium_index::read_module_facts(&src);
    let mapping = cambium_index::RepoMapping::from_canonical(
        (0..base.num_repos()).map(|r| (base.repo_name(r), base.repo_canonical.get(r))),
    )
    .with_main_alias(&facts.module_name);

    let mut graph = cambium_index::LoadGraph::new();
    let workspace = graph.workspace_closure(&src, &mapping);
    let inputs = cambium_index::build_repo_inputs(&src, &mapping, &graph, &workspace, &facts);

    let mut builder = cambium_index::IndexBuilder::from_view(base);
    let mut closure = builder
        .take_bzl_closure()
        .context("the stored index carries no .bzl load closure to re-derive over")?;
    // The stored graph and the stored package closures stay as they are: they
    // are the ids the packages table speaks. Only the workspace membership and
    // the input tables come from the fresh walk.
    closure.workspace = workspace
        .iter()
        .filter_map(|&id| base.bzl_id(graph.path(id)))
        .collect();
    closure.workspace.sort_unstable();
    closure.workspace.dedup();
    closure.inputs = inputs;
    builder.set_bzl_closure(closure);
    Ok(builder.finish(rev))
}

/// The re-derived indexes of a `--rederive` run, a few base revisions at a
/// time.
///
/// Re-deriving costs a full [`cambium_index::IndexBuilder::finish`] — every
/// label and both CSRs materialised — which at envoy's size is the expensive
/// part of the replay by an order of magnitude, while consecutive change sets
/// and history commits frequently share a base. Building once per revision is
/// what keeps `--rederive` in the same minutes-long budget as the plain
/// replay.
#[derive(Default)]
pub struct RederiveCache {
    built: BTreeMap<String, cambium_index::IndexData>,
    /// Insertion order, for eviction.
    order: Vec<String>,
    cap: usize,
}

/// How many re-derived indexes to hold at once.
///
/// A re-derived index is the whole index in memory, a few hundred megabytes at
/// envoy's size, and `--all --history` walks three hundred bases; holding them
/// all would be tens of gigabytes for no gain. Replays are driven in recorded
/// order, where a repeated base is a *consecutive* repeat — a change set and
/// the history commit that shares its parent — so a handful of slots catch
/// every reuse there is. A base that recurs later is simply rebuilt.
const REDERIVE_CACHE_SLOTS: usize = 4;

impl RederiveCache {
    pub fn new() -> Self {
        Self::with_slots(REDERIVE_CACHE_SLOTS)
    }

    pub fn with_slots(cap: usize) -> Self {
        Self {
            built: BTreeMap::new(),
            order: Vec::new(),
            cap: cap.max(1),
        }
    }

    /// Re-derives at `rev` unless it is already built. `Some(ms)` is what this
    /// call cost; `None` means the cache answered.
    pub fn ensure(
        &mut self,
        base: IndexRef<'_>,
        rev: &str,
        texts: &dyn WorkspaceText,
    ) -> Result<Option<u64>> {
        if self.built.contains_key(rev) {
            return Ok(None);
        }
        let t = std::time::Instant::now();
        let data = rederive_externals(base, rev, texts)?;
        let ms = t.elapsed().as_millis() as u64;
        self.built.insert(rev.to_string(), data);
        self.order.push(rev.to_string());
        while self.order.len() > self.cap {
            let evicted = self.order.remove(0);
            self.built.remove(&evicted);
        }
        Ok(Some(ms))
    }

    /// The re-derived index at `rev`, which [`RederiveCache::ensure`] must
    /// have built.
    pub fn get(&self, rev: &str) -> Option<IndexRef<'_>> {
        self.built.get(rev).map(|d| d.view())
    }

    pub fn len(&self) -> usize {
        self.built.len()
    }

    pub fn is_empty(&self) -> bool {
        self.built.is_empty()
    }
}

/// Runs the static stages over one diff.
pub fn replay(
    base: IndexRef<'_>,
    cfg: &Config,
    base_rev: &str,
    head_rev: &str,
    changes: &[Change],
    texts: &dyn WorkspaceText,
) -> ReplayOutcome {
    let mut out = ReplayOutcome {
        base: base_rev.to_string(),
        head: head_rev.to_string(),
        changed_files: changes.len(),
        max_dirty_packages: cfg.limits.max_dirty_packages,
        ..Default::default()
    };

    // A `MODULE.bazel` this replay cannot read is the one case where the
    // stage's own answer ("unparseable, so global") is an artefact of the
    // replay rather than a statement about the engine.
    let touches_module = changes.iter().any(|c| c.path == externals::MODULE_FILE);
    if touches_module && texts.text_at(head_rev, externals::MODULE_FILE).is_none() {
        out.indeterminate.push(format!(
            "the change set edits {} and no git repository was given to read it at either \
             revision; pass --repo to decide this one",
            externals::MODULE_FILE
        ));
    }

    // The bazelrc screen reads the file's *text* at both revisions and decides
    // whether every flag it changed is one that cannot reach loading. There is
    // no text here, and the outcomes are opposite extremes — a global run, or
    // the file dropped from the diff entirely — so the replay declines.
    if let Some(c) = changes
        .iter()
        .find(|c| cambium_engine::bazelrc::is_bazelrc_path(&c.path))
    {
        out.indeterminate.push(format!(
            "the change set edits {}, and whether a bazelrc edit goes global depends on which \
             flags it changed, which needs the file at both revisions",
            c.path
        ));
    }

    // A change set that adds or removes a package cannot be replayed against
    // the index at the pin: there the package already exists, or is long gone,
    // so the owner of every path under it differs from what the run saw. The
    // engine also asks the head worktree whether a deleted `BUILD` really
    // deleted its package, and there is no worktree here.
    if let Some(c) = changes.iter().find(|c| {
        c.status != Status::Modified
            && matches!(
                c.path.rsplit('/').next(),
                Some("BUILD") | Some("BUILD.bazel")
            )
    }) {
        out.indeterminate.push(format!(
            "the change set adds or removes {}, so the package set at the base differs from the \
             one in the stored index at the pin",
            c.path
        ));
    }

    let mut ext = if cfg.externals.enabled {
        externals::analyse(base, changes, base_rev, head_rev, texts)
    } else {
        externals::Externals::default()
    };

    // The same drift, one level down. A `.bzl` the diff *modified* existed at
    // the base by definition; one the stored index's load graph has never
    // heard of is one whose loaders and repositories the replay cannot see, so
    // it silently contributes nothing where the run may have gone global on
    // it. envoy's `bazel/repositories.bzl` is the case: the whole WORKSPACE
    // dependency file, in the closure at every change set's base and absent
    // from the pin's fifty-five `.bzl` files, because by then envoy is on
    // Bzlmod.
    if base.bzl_closure {
        let unknown: Vec<&str> = changes
            .iter()
            .filter(|c| {
                c.status == Status::Modified
                    && c.path.ends_with(".bzl")
                    && base.bzl_id(&c.path).is_none()
            })
            .map(|c| c.path.as_str())
            .collect();
        if !unknown.is_empty() {
            out.indeterminate.push(format!(
                "the change set modifies {}, and the stored index's load closure does not \
                 contain {} at all; whatever they loaded at the base is not in this graph",
                unknown.join(", "),
                if unknown.len() == 1 { "it" } else { "them" },
            ));
        }
    }

    if let Some(declared) = repos_declared_at(base_rev, texts) {
        let undeclared: Vec<&str> = ext
            .repos
            .iter()
            .filter(|r| !declared.contains(*r))
            .map(String::as_str)
            .collect();
        if !undeclared.is_empty() {
            out.indeterminate.push(format!(
                "the stage dirtied {} that {} at the base does not declare ({}); those \
                 repositories arrived between the base and the pin, so this answer is the \
                 stored index's graph rather than the run's",
                if undeclared.len() == 1 {
                    "a repository".to_string()
                } else {
                    format!("{} repositories", undeclared.len())
                },
                externals::MODULE_FILE,
                undeclared.join(", "),
            ));
        }
    }

    let mut matchers = match cambium_classify::Matchers::compile(&cfg.rules()) {
        Ok(m) => m,
        Err(e) => {
            out.global = true;
            out.global_reason = Some(format!("invalidator rules do not compile: {e}"));
            return out;
        }
    };
    matchers.exempt(ext.handled.clone());
    let mut packages = cambium_classify::PackageSet::from_index(base);
    let dirty = cambium_classify::dirty_set(
        changes,
        &mut packages,
        &matchers,
        &cambium_classify::IndexConsumers(base),
    );

    let mut by_package: BTreeMap<String, PackageRule> = BTreeMap::new();
    for p in &dirty.dirty {
        by_package.insert(p.clone(), PackageRule::ChangedFile);
    }

    let mut global = dirty.global;
    let mut reason = dirty.global_reason.clone();

    // Changed `.bzl` files, from the static closure. The engine escalates when
    // the index carries no closure or when a changed `.bzl` is loaded outside
    // any package and no repository owns it; both are reproduced here.
    if !global && !dirty.changed_bzl.is_empty() {
        if !base.bzl_closure {
            global = true;
            reason = Some("the base index carries no static .bzl load closure".into());
        } else {
            for path in &dirty.changed_bzl {
                let Some(id) = base.bzl_id(path) else {
                    continue;
                };
                // A workspace-level `.bzl` can define repository rules, whose
                // output is not in the index at all. The index says which
                // repositories each one defines, and the rule's implementation
                // moving means anything it writes into them can have moved —
                // so they join `load_repos`, not just `repos`. Missing this
                // was worth about eleven hundred packages a change set on
                // envoy, where every dirty repository arrives this way.
                if base.bzl_in_workspace(id) && cfg.externals.enabled {
                    match externals::repos_of_workspace_bzl(base, id) {
                        Some(repos) => {
                            ext.by_bzl += repos.len();
                            ext.load_repos.extend(repos.iter().cloned());
                            ext.repos.extend(repos);
                        }
                        None => {
                            global = true;
                            reason = Some(format!(
                                "a changed .bzl is loaded outside any package and no repository \
                                 owns it: {path}"
                            ));
                        }
                    }
                }
                for &p in base.bzl_consumers(id) {
                    if base.package_tombstoned(p) {
                        continue;
                    }
                    by_package
                        .entry(base.package_name(p).to_string())
                        .or_insert(PackageRule::BzlClosure);
                }
            }
        }
    }

    if let Some(r) = &ext.global {
        global = true;
        reason = Some(r.clone());
    }

    out.repos = attribute_repos(base, changes, &ext);
    let mut external_nodes: Vec<u32> = Vec::new();
    if !global && !ext.repos.is_empty() {
        let r = externals::reach(base, &ext.repos, &ext.load_repos);
        external_nodes = r.nodes;
        let routes = package_routes(base, &ext.load_repos);
        for p in r.packages {
            let rule = routes
                .get(p.as_str())
                .copied()
                .unwrap_or(PackageRule::ExternalBzlClosure);
            by_package.entry(p).or_insert(rule);
        }
    }

    out.global = global;
    out.global_reason = reason;
    out.external_direct_targets = external_nodes.len();
    out.dirty_packages = by_package.len();
    out.cold_rebuild = !global
        && cfg.limits.max_dirty_packages > 0
        && by_package.len() > cfg.limits.max_dirty_packages;
    out.packages = by_package
        .into_iter()
        .map(|(package, rule)| PackageDirty { package, rule })
        .collect();

    if !out.global {
        out.affected_lower_bound = lower_bound(base, changes, &external_nodes);
    }
    out
}

/// Which of the two rule (2) routes reaches each package.
///
/// [`externals::reach`] returns one package set; the diagnostic value is in
/// knowing whether a package is there because a `.bzl` it loads reaches the
/// repository or because its own BUILD file `load()`s from it, since only the
/// second is issue #4's route. Recomputed rather than returned, so the engine's
/// own answer stays the authority on *what* is dirty.
fn package_routes(
    base: IndexRef<'_>,
    load_repos: &BTreeSet<String>,
) -> BTreeMap<String, PackageRule> {
    let mut out = BTreeMap::new();
    if !base.bzl_closure || load_repos.is_empty() {
        return out;
    }
    let mut canon: BTreeMap<&str, &str> = BTreeMap::new();
    for r in 0..base.num_repos() {
        canon.insert(base.repo_name(r), base.repo_canonical.get(r));
    }
    let hot: BTreeSet<u32> = (0..base.bzl_ext_labels.len() as u32)
        .filter(|&e| {
            let label = base.bzl_ext_labels.get(e);
            let Some(token) = label_repo(label) else {
                return false;
            };
            load_repos.iter().any(|a| {
                externals::repo_token_matches(
                    token,
                    a,
                    canon.get(a.as_str()).copied().unwrap_or(""),
                )
            })
        })
        .collect();
    if hot.is_empty() {
        return out;
    }
    // `pkg_ext` first, then let the `.bzl` route overwrite nothing: a package
    // on both routes is reported as the `.bzl` one only if `pkg_ext` did not
    // claim it, because the direct load is the more specific fact.
    for p in 0..base.pkg_ext.len() as u32 {
        if base.package_tombstoned(p) {
            continue;
        }
        if base.pkg_ext.row(p).iter().any(|e| hot.contains(e)) {
            out.insert(
                base.package_name(p).to_string(),
                PackageRule::ExternalPkgExt,
            );
        }
    }
    for id in 0..base.num_bzl() {
        if !base.bzl_ext.row(id).iter().any(|e| hot.contains(e)) {
            continue;
        }
        for &p in base.bzl_consumers(id) {
            if base.package_tombstoned(p) {
                continue;
            }
            out.entry(base.package_name(p).to_string())
                .or_insert(PackageRule::ExternalBzlClosure);
        }
    }
    out
}

fn label_repo(label: &str) -> Option<&str> {
    let rest = label
        .strip_prefix("@@")
        .or_else(|| label.strip_prefix('@'))?;
    let (repo, _) = rest.split_once("//")?;
    (!repo.is_empty()).then_some(repo)
}

/// Per-repository attribution for the human report.
///
/// [`externals::Externals`] counts the rules but does not say which repository
/// each count belongs to; the counts are what the engine acts on and are left
/// alone. This recovers the attribution from the same index rows.
fn attribute_repos(
    base: IndexRef<'_>,
    changes: &[Change],
    ext: &externals::Externals,
) -> Vec<RepoDirty> {
    let mut by_input: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut by_target: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in changes {
        for (r, feeds_rule) in base.repos_for_path_kinds(&c.path) {
            let name = base.repo_name(r).to_string();
            let m = if feeds_rule {
                &mut by_input
            } else {
                &mut by_target
            };
            m.entry(name).or_default().push(c.path.clone());
        }
    }
    ext.repos
        .iter()
        .map(|repo| {
            let (rule, paths) = if let Some(p) = by_input.get(repo) {
                (RepoRule::InputPath, p.clone())
            } else if let Some(p) = by_target.get(repo) {
                (RepoRule::TargetOnly, p.clone())
            // Neither the module diff nor a workspace `.bzl` records which
            // repository it named, so with both in play the module is
            // reported: it is the coarser and more alarming of the two.
            } else if ext.by_module > 0 {
                (RepoRule::ModuleDiff, Vec::new())
            } else {
                (RepoRule::Load, Vec::new())
            };
            RepoDirty {
                repo: repo.clone(),
                rule,
                loads_too: ext.load_repos.contains(repo),
                paths,
            }
        })
        .collect()
}

/// The reverse walk from what the base index already knows changed.
fn lower_bound(base: IndexRef<'_>, changes: &[Change], external_nodes: &[u32]) -> usize {
    let packages = cambium_classify::PackageSet::from_index(base);
    let labels = base.labels.build_map();
    let mut seeds: Vec<u32> = external_nodes.to_vec();
    for c in changes {
        let Some(pkg) = packages.owner_of(&c.path) else {
            continue;
        };
        let label = cambium_index::label::source_label(pkg, &c.path);
        if let Some(&id) = labels.get(label.as_str()) {
            seeds.push(id);
        }
    }
    seeds.sort_unstable();
    seeds.dedup();
    if seeds.is_empty() {
        return 0;
    }
    let r = walk::walk(
        base,
        &seeds,
        &cambium_index::WalkOptions {
            include_manual: false,
            explain: false,
            max_depth: None,
        },
    );
    r.affected.len()
}

// ------------------------------------------------------------- what a run said

/// The part of a recorded result a replay can be held against.
///
/// Both `changes-<pin>.json` and `history-<pin>.json` collapse onto this, so
/// the comparison is written once and tested without either file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordedAnswer {
    pub base: String,
    pub head: String,
    pub global: bool,
    pub dirty_packages: usize,
    pub cold_rebuild: bool,
    /// `None` on a history record, which does not separate them out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected: Option<usize>,
}

impl RecordedAnswer {
    pub fn key(&self) -> String {
        format!("{}..{}", self.base, self.head)
    }
}

/// Every change set of a `changes-<pin>.json`.
pub fn recorded_from_changes(r: &crate::compare::ChangesResult) -> Vec<RecordedAnswer> {
    r.change_sets
        .iter()
        .map(|c| RecordedAnswer {
            base: c.base.clone(),
            head: c.head.clone(),
            global: c.cambium.global,
            dirty_packages: c.cambium.dirty_packages,
            cold_rebuild: c.cambium.cold_rebuild,
            affected: (!c.cambium.global).then_some(c.cambium.affected),
        })
        .collect()
}

/// Every commit of a `history-<pin>.json`.
///
/// A history commit's base is the index the previous commit left behind, not
/// the pin, so these diverge for reasons that are not the engine's; the CLI
/// says so and keeps them off the default gate.
pub fn recorded_from_history(r: &crate::history::HistoryResult) -> Vec<RecordedAnswer> {
    r.commits
        .iter()
        .map(|c| RecordedAnswer {
            base: c.parent.clone(),
            head: c.commit.clone(),
            global: c.global,
            dirty_packages: c.dirty_packages,
            cold_rebuild: c.engine_cold_rebuild,
            affected: (!c.global).then_some(c.affected),
        })
        .collect()
}

/// How a side of a comparison derived its externals tables, for the
/// `rederived` divergence: a re-derived replay is *expected* to differ from a
/// plain one, and a report that only said `true`/`false` would leave a reader
/// guessing which half of the pair that was.
fn side(rederived: bool) -> String {
    if rederived {
        "re-derived from the repository text at the base".into()
    } else {
        "the stored index's tables".into()
    }
}

/// One field on which the replay and something recorded disagree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Divergence {
    pub key: String,
    pub field: String,
    pub recorded: String,
    pub replayed: String,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} recorded {} replayed {}",
            self.key, self.field, self.recorded, self.replayed
        )
    }
}

/// Compares a replay against a recorded answer.
///
/// Only the three static verdicts are compared, because they are the only ones
/// the replay computes: whether the run went global, how many packages it
/// dirtied, and whether it would cold-rebuild. An indeterminate replay yields
/// nothing — a question the replay declined to answer is not a disagreement.
pub fn diverge(recorded: &RecordedAnswer, replayed: &ReplayOutcome) -> Vec<Divergence> {
    if !replayed.indeterminate.is_empty() {
        return Vec::new();
    }
    let key = recorded.key();
    let mut out = Vec::new();
    let mut cmp = |field: &str, a: String, b: String| {
        if a != b {
            out.push(Divergence {
                key: key.clone(),
                field: field.into(),
                recorded: a,
                replayed: b,
            });
        }
    };
    cmp(
        "global",
        recorded.global.to_string(),
        replayed.global.to_string(),
    );
    // Past a global there is nothing else to compare: the engine returns
    // before it counts packages, and the recorded zeroes are absence, not
    // agreement.
    if recorded.global || replayed.global {
        return out;
    }
    cmp(
        "dirty_packages",
        recorded.dirty_packages.to_string(),
        replayed.dirty_packages.to_string(),
    );
    cmp(
        "cold_rebuild",
        recorded.cold_rebuild.to_string(),
        replayed.cold_rebuild.to_string(),
    );
    out
}

// ----------------------------------------------------------------- a whole run

/// Every replay of one subject, which is both the `--all` report and the
/// baseline a later `--all` is compared against.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRun {
    pub schema: u32,
    pub subject: String,
    pub pin: String,
    pub outcomes: Vec<ReplayOutcome>,
}

impl ReplayRun {
    pub fn read(path: &std::path::Path) -> Result<Self> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let r: ReplayRun =
            serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            r.schema == REPLAY_SCHEMA,
            "{} was written by schema {} and this build reads {REPLAY_SCHEMA}",
            path.display(),
            r.schema
        );
        Ok(r)
    }

    pub fn write(&self, path: &std::path::Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Replay against replay: the tight loop.
    ///
    /// Both sides ran on the same stored index, so every difference is the
    /// engine's — no base-index drift, no missing `MODULE.bazel`. A key
    /// present on one side only is reported as its own divergence rather than
    /// skipped, because a change set the new build declined to replay is
    /// itself a change.
    pub fn divergences_against(&self, baseline: &ReplayRun) -> Vec<Divergence> {
        let mine: BTreeMap<String, &ReplayOutcome> =
            self.outcomes.iter().map(|o| (o.key(), o)).collect();
        let theirs: BTreeMap<String, &ReplayOutcome> =
            baseline.outcomes.iter().map(|o| (o.key(), o)).collect();
        let mut out = Vec::new();
        for (key, old) in &theirs {
            let Some(new) = mine.get(key) else {
                out.push(Divergence {
                    key: key.clone(),
                    field: "present".into(),
                    recorded: "yes".into(),
                    replayed: "no".into(),
                });
                continue;
            };
            out.extend(diverge(
                &RecordedAnswer {
                    base: old.base.clone(),
                    head: old.head.clone(),
                    global: old.global,
                    dirty_packages: old.dirty_packages,
                    cold_rebuild: old.cold_rebuild,
                    affected: None,
                },
                new,
            ));
            if old.affected_lower_bound != new.affected_lower_bound {
                out.push(Divergence {
                    key: key.clone(),
                    field: "affected_lower_bound".into(),
                    recorded: old.affected_lower_bound.to_string(),
                    replayed: new.affected_lower_bound.to_string(),
                });
            }
        }
        for key in mine.keys() {
            if !theirs.contains_key(key) {
                out.push(Divergence {
                    key: key.clone(),
                    field: "present".into(),
                    recorded: "no".into(),
                    replayed: "yes".into(),
                });
            }
        }
        // How each side derived its externals tables is a fact about the two
        // runs, not about any one change set, so it is reported once and at
        // the front: everything below it is being read across a derivation
        // change and is expected to differ.
        let sides = |rs: &[ReplayOutcome]| rs.iter().any(|o| o.rederived);
        let (old_side, new_side) = (sides(&baseline.outcomes), sides(&self.outcomes));
        if old_side != new_side {
            out.insert(
                0,
                Divergence {
                    key: "(whole run)".into(),
                    field: "rederived".into(),
                    recorded: side(old_side),
                    replayed: side(new_side),
                },
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(base: &str, head: &str, global: bool, dirty: usize, cold: bool) -> ReplayOutcome {
        ReplayOutcome {
            base: base.into(),
            head: head.into(),
            global,
            dirty_packages: dirty,
            cold_rebuild: cold,
            ..Default::default()
        }
    }

    /// A stored index with one package, one workspace `.bzl` and one
    /// repository, standing in for the pin.
    fn stored_index() -> cambium_index::IndexData {
        let mut b = cambium_index::IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[cambium_index::PackageUpdate {
            package: "lib".into(),
            targets: vec![cambium_index::TargetRow::rule(
                "//lib:core",
                "cc_library",
                vec![],
            )],
            ..Default::default()
        }]);
        struct Src;
        impl cambium_index::FileText for Src {
            fn text(&self, path: &str) -> Option<String> {
                match path {
                    "WORKSPACE" => Some("load(\"//bazel:known.bzl\", \"x\")\n".into()),
                    "bazel/known.bzl" => Some(String::new()),
                    _ => None,
                }
            }
        }
        let mut c = cambium_index::BzlClosure::default();
        c.workspace = c
            .graph
            .workspace_closure(&Src, &cambium_index::RepoMapping::identity());
        assert!(!c.workspace.is_empty());
        c.inputs.repos.insert("late_arrival".into());
        b.set_bzl_closure(c);
        b.finish("pin")
    }

    /// The repository text at one revision, with no git anywhere: the
    /// re-derivation reads its files through [`WorkspaceText`] exactly as the
    /// CLI's `git show` implementation serves them.
    fn files_at(rev: &str, files: &[(&str, &str)]) -> cambium_engine::externals::FixedText {
        let mut m = BTreeMap::new();
        for (path, text) in files {
            m.insert((rev.to_string(), path.to_string()), text.to_string());
        }
        cambium_engine::externals::FixedText(m)
    }

    /// A stored index whose externals tables say one thing while the
    /// repository text at the base says another.
    ///
    /// This is the whole point of `--rederive`: the tables in the index are
    /// the output of the derivation that ran at index time, so a replay that
    /// reads them back cannot see a change to the derivation itself. Here the
    /// stored table feeds `stale_repo` from `stale/**` and knows nothing about
    /// `.github/config.yml`; the text at the base derives `envoy_repo` from
    /// it.
    fn index_with_stale_externals() -> cambium_index::IndexData {
        let mut b = cambium_index::IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[cambium_index::PackageUpdate {
            package: "lib".into(),
            targets: vec![cambium_index::TargetRow::rule(
                "//lib:core",
                "cc_library",
                vec![],
            )],
            ..Default::default()
        }]);
        let mut c = cambium_index::BzlClosure::default();
        c.inputs.repos.insert("stale_repo".into());
        c.inputs.patterns.insert(
            cambium_index::Pattern::Prefix("stale/".into()),
            ["stale_repo".to_string()].into_iter().collect(),
        );
        b.set_bzl_closure(c);
        b.finish("pin")
    }

    const REDERIVE_MODULE: &str = "\
module(name = \"envoy\", version = \"1\")\n\
envoy_repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
use_repo(envoy_repo_ext, \"envoy_repo\")\n";

    /// The re-derived replay dirties the repository the *text at the base*
    /// feeds, not the one the stored table named.
    #[test]
    fn rederiving_replaces_the_stored_externals_tables_with_the_bases_own() {
        let stored = index_with_stale_externals();
        let cfg = Config::default();
        let texts = files_at(
            "base",
            &[
                (externals::MODULE_FILE, REDERIVE_MODULE),
                (
                    "bazel/extensions.bzl",
                    "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
                ),
                (
                    "bazel/repo.bzl",
                    "X = attr.label(default = \"@envoy//:.github/config.yml\")\n",
                ),
            ],
        );
        let changes = [Change::new(Status::Modified, ".github/config.yml")];

        // As stored: nothing knows that path feeds anything.
        let plain = replay(stored.view(), &cfg, "base", "head", &changes, &texts);
        assert!(plain.repos.is_empty(), "{:?}", plain.repos);

        let rebuilt = rederive_externals(stored.view(), "base", &texts).unwrap();
        let rederived = replay(rebuilt.view(), &cfg, "base", "head", &changes, &texts);
        let repos: Vec<&str> = rederived.repos.iter().map(|r| r.repo.as_str()).collect();
        assert_eq!(repos, vec!["envoy_repo"], "{:?}", rederived.repos);
        assert_eq!(rederived.repos[0].rule, RepoRule::InputPath);
        assert_eq!(rederived.repos[0].paths, vec![".github/config.yml"]);

        // The stale table is gone, and what the run's query established — the
        // packages — is not.
        let view = rebuilt.view();
        assert!(!(0..view.num_repos()).any(|r| view.repo_name(r) == "stale_repo"));
        assert_eq!(view.num_packages(), stored.view().num_packages());
        assert_eq!(view.package_name(0), "lib");
    }

    /// The cache answers a repeated base and holds only its slots, so a run
    /// over three hundred history commits does not accumulate three hundred
    /// whole indexes.
    #[test]
    fn the_rederive_cache_reuses_a_repeated_base_and_evicts_the_oldest() {
        let stored = index_with_stale_externals();
        let files = [
            (externals::MODULE_FILE, REDERIVE_MODULE),
            (
                "bazel/extensions.bzl",
                "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
            ),
        ];
        let mut cache = RederiveCache::with_slots(2);
        // The second visit to "a" is the cache answering; the rest are builds.
        for (rev, cached) in [("a", false), ("a", true), ("b", false), ("c", false)] {
            let texts = files_at(rev, &files);
            let cost = cache.ensure(stored.view(), rev, &texts).unwrap();
            assert_eq!(cost.is_none(), cached, "at {rev}");
        }
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_none(), "the oldest slot was not evicted");
        assert!(cache.get("b").is_some() && cache.get("c").is_some());
    }

    /// A re-derivation with nothing to read is an error rather than a silently
    /// empty set of tables, which would read as "no repository moved".
    #[test]
    fn rederiving_without_repository_text_fails_rather_than_emptying_the_tables() {
        let stored = index_with_stale_externals();
        let e = rederive_externals(stored.view(), "base", &NoText).unwrap_err();
        assert!(e.to_string().contains("nothing to re-derive"), "{e}");
    }

    fn module_at(rev: &str, text: &str) -> cambium_engine::externals::FixedText {
        let mut m = BTreeMap::new();
        m.insert(
            (rev.to_string(), externals::MODULE_FILE.to_string()),
            text.to_string(),
        );
        cambium_engine::externals::FixedText(m)
    }

    /// Both drift guards, on the shapes envoy produced: the stage names a
    /// repository the base's `MODULE.bazel` never declared, and the diff
    /// modifies a `.bzl` the stored closure does not contain. Neither is a
    /// statement about the engine, so neither is gated on.
    #[test]
    fn a_graph_the_base_never_had_is_indeterminate_rather_than_a_divergence() {
        let index = stored_index();
        let cfg = Config::default();
        let texts = module_at("base", "module(name = \"demo\", version = \"1\")\n");

        let o = replay(
            index.view(),
            &cfg,
            "base",
            "head",
            &[Change::new(Status::Modified, "bazel/gone.bzl")],
            &texts,
        );
        assert!(
            o.indeterminate.iter().any(|s| s.contains("bazel/gone.bzl")),
            "{:?}",
            o.indeterminate
        );

        // A `.bzl` the closure does know says nothing, so the guard is silent.
        let o = replay(
            index.view(),
            &cfg,
            "base",
            "head",
            &[Change::new(Status::Modified, "bazel/known.bzl")],
            &texts,
        );
        assert!(
            !o.indeterminate.iter().any(|s| s.contains("load closure")),
            "{:?}",
            o.indeterminate
        );
    }

    #[test]
    fn name_status_lines_become_changes() {
        let got = parse_name_status(&[
            "M\tsrc/a.cc".into(),
            "A\tsrc/b.h".into(),
            "D\tsrc/c.h".into(),
            "T\tsrc/d".into(),
            "R100\tsrc/e\tsrc/f".into(),
            "".into(),
        ]);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0], Change::new(Status::Modified, "src/a.cc"));
        assert_eq!(got[1], Change::new(Status::Added, "src/b.h"));
        assert_eq!(got[2], Change::new(Status::Deleted, "src/c.h"));
        assert_eq!(got[3], Change::new(Status::TypeChanged, "src/d"));
        // A rename's `--name-status` line carries two paths; the unrecognised
        // status reads as a modification and the rest of the line as the path,
        // which is wider than the truth and never narrower.
        assert_eq!(got[4].status, Status::Modified);
    }

    #[test]
    fn a_bare_path_is_read_as_a_modification() {
        let got = parse_name_status(&["src/a.cc".into()]);
        assert_eq!(got, vec![Change::new(Status::Modified, "src/a.cc")]);
    }

    #[test]
    fn a_short_revision_finds_the_full_one() {
        let cache = PathsCache {
            schema: REPLAY_SCHEMA,
            repo: "envoy".into(),
            pin: "abc".into(),
            entries: vec![PathEntry {
                base: "aaaaaaaabbbb".into(),
                head: "ccccccccdddd".into(),
                changes: vec!["M\tx".into()],
            }],
        };
        assert!(cache.lookup("aaaaaaaa", "cccccccc").is_some());
        assert!(cache.lookup("aaaaaaaabbbb", "ccccccccdddd").is_some());
        assert!(cache.lookup("aaaaaaaa", "eeeeeeee").is_none());
    }

    #[test]
    fn the_three_static_verdicts_are_what_diverges() {
        let rec = RecordedAnswer {
            base: "a".into(),
            head: "b".into(),
            global: false,
            dirty_packages: 7,
            cold_rebuild: false,
            affected: Some(100),
        };
        assert!(diverge(&rec, &outcome("a", "b", false, 7, false)).is_empty());

        let d = diverge(&rec, &outcome("a", "b", false, 9, true));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].field, "dirty_packages");
        assert_eq!(d[0].recorded, "7");
        assert_eq!(d[0].replayed, "9");
        assert_eq!(d[1].field, "cold_rebuild");
    }

    #[test]
    fn a_global_on_either_side_is_reported_once_and_not_elaborated() {
        let rec = RecordedAnswer {
            base: "a".into(),
            head: "b".into(),
            global: true,
            dirty_packages: 0,
            cold_rebuild: false,
            affected: None,
        };
        // The recorded zero package count is absence, not agreement, so a
        // replay that localised the change reports the global and stops.
        let d = diverge(&rec, &outcome("a", "b", false, 12, false));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].field, "global");
    }

    #[test]
    fn an_indeterminate_replay_disagrees_with_nothing() {
        let rec = RecordedAnswer {
            base: "a".into(),
            head: "b".into(),
            global: false,
            dirty_packages: 7,
            cold_rebuild: false,
            affected: None,
        };
        let mut o = outcome("a", "b", true, 0, false);
        o.indeterminate.push("no MODULE.bazel".into());
        assert!(diverge(&rec, &o).is_empty());
    }

    #[test]
    fn replay_against_replay_notices_an_appearance_and_a_disappearance() {
        let run = |outcomes: Vec<ReplayOutcome>| ReplayRun {
            schema: REPLAY_SCHEMA,
            subject: "envoy".into(),
            pin: "pin".into(),
            outcomes,
        };
        let old = run(vec![
            outcome("a", "b", false, 3, false),
            outcome("c", "d", false, 3, false),
        ]);
        let new = run(vec![
            outcome("a", "b", false, 4, false),
            outcome("e", "f", false, 3, false),
        ]);
        let d = new.divergences_against(&old);
        let fields: Vec<&str> = d.iter().map(|x| x.field.as_str()).collect();
        assert!(fields.contains(&"dirty_packages"));
        assert_eq!(d.iter().filter(|x| x.field == "present").count(), 2);
    }

    /// A re-derived run compared against a plain one is expected to differ,
    /// so the report says which side was which — once, at the front, rather
    /// than once per change set.
    #[test]
    fn a_rederived_run_says_so_once_when_compared_against_a_plain_one() {
        let run = |rederived: bool| ReplayRun {
            schema: REPLAY_SCHEMA,
            subject: "envoy".into(),
            pin: "pin".into(),
            outcomes: vec!["ab", "cd"]
                .into_iter()
                .map(|k| ReplayOutcome {
                    rederived,
                    ..outcome(k, k, false, 3, false)
                })
                .collect(),
        };
        let d = run(true).divergences_against(&run(false));
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].field, "rederived");
        assert!(d[0].replayed.contains("re-derived"), "{:?}", d[0]);
        assert!(d[0].recorded.contains("stored index"), "{:?}", d[0]);
        // Two runs derived the same way say nothing.
        assert!(run(true).divergences_against(&run(true)).is_empty());
    }

    #[test]
    fn the_lower_bound_is_part_of_a_replay_to_replay_comparison() {
        let mut a = outcome("a", "b", false, 3, false);
        let mut b = outcome("a", "b", false, 3, false);
        a.affected_lower_bound = 10;
        b.affected_lower_bound = 11;
        let run = |o| ReplayRun {
            schema: REPLAY_SCHEMA,
            subject: "s".into(),
            pin: "p".into(),
            outcomes: vec![o],
        };
        let d = run(b).divergences_against(&run(a));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].field, "affected_lower_bound");
    }
}
