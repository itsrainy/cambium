//! `cambi profile`: what cambium costs and saves on *this* repository.
//!
//! The question a team asks before adopting the tool is not "is it correct"
//! (the benchmark answers that on its subjects) but "what does it do on our
//! graph": how many targets, how long a cold index takes and how big it is,
//! how long an incremental answer takes on a typical commit, and how often a
//! commit invalidates everything. This module answers it the way the
//! benchmark's history workload does — a cold build at the start of a
//! first-parent range, then one incremental step per commit forward to the
//! head, cold-rebuilding wherever a commit goes global — but in one process,
//! over one checkout, with the index kept in memory between steps.
//!
//! The walk is generic over [`Revisions`], the thing that produces a row
//! source for a commit. The real one, [`WorktreeRevisions`], is a single
//! detached worktree moved from commit to commit with one Bazel server, so a
//! thirty-commit profile pays one server start rather than thirty. The tests
//! drive the same walk over a synthetic git history with no Bazel anywhere.

use std::collections::BTreeMap;
use std::path::Path;

use cambium_bazel::{Bazel, ConvertOptions};
use cambium_index::format::Compression;
use cambium_index::{IndexData, IndexFile};
use serde::{Deserialize, Serialize};

use cambium_diff::CommitMeta;

use crate::diag::{Code, Diagnostic, Diagnostics};
use crate::engine::{AffectedRequest, Engine};
use crate::error::{EngineError, Result};
use crate::query::{BazelQuerier, FileSource, PackageQuerier};
use crate::report::{Stopwatch, SCHEMA_VERSION};
use crate::revision::{worktrees_dir, RevisionWorkspace};

/// Where the walk gets its package rows for a commit.
pub trait Revisions {
    /// A row source that sees the repository at `sha`.
    fn querier(&self, sha: &str) -> Result<Box<dyn PackageQuerier + '_>>;
}

/// One detached worktree, moved from commit to commit, and the Bazel that
/// reads it.
///
/// The worktree is the same kind [`crate::revision`] makes for a run whose
/// base is not on disk, so it gets the same treatment: its own output base
/// (when the run has one), the untracked `cambium.toml` mirrored in, and its
/// server shut down when this drops. Using it for the head commit too — even
/// though the working tree may already be there — is deliberate: the profile
/// then describes committed history and never a half-edited checkout.
///
/// The walk crosses commits that pin different Bazels, and bazelisk starts
/// whichever one the checkout names on the worktree's one output base. A
/// `bazel shutdown` reaches only a server of the client's own version, so
/// the server is stopped before the checkout moves to a commit that names
/// another version, while the checkout still names the one that started it;
/// otherwise the last server of an older version outlives the walk, on a
/// worktree that is then deleted and a directory no later `bazel shutdown`
/// can be run in.
pub struct WorktreeRevisions {
    workspace: RevisionWorkspace,
    bazel: Bazel,
    opts: ConvertOptions,
    diags: Diagnostics,
    /// A querier has been handed out since the last shutdown, so a server
    /// may be running.
    served: std::sync::atomic::AtomicBool,
}

/// The files that decide which Bazel bazelisk runs in a checkout.
const VERSION_FILES: [&str; 2] = [".bazelversion", ".bazeliskrc"];

impl WorktreeRevisions {
    /// A worktree at `sha`, named for it under the engine's cache directory.
    pub fn create(engine: &Engine, sha: &str) -> Result<Self> {
        let workspace =
            RevisionWorkspace::create(engine.repo(), sha, &worktrees_dir(engine.cache_dir()))?;
        let bazel = workspace.bazel(engine.bazel(), sha);
        Ok(Self {
            workspace,
            bazel,
            opts: engine.config().convert_options(),
            diags: engine.diagnostics().clone(),
            served: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Whether `sha` names a different Bazel than the checkout does now: any
    /// of [`VERSION_FILES`] differs, or is there on one side only.
    fn moves_bazel(&self, sha: &str) -> bool {
        let repo = cambium_diff::Repo::new(self.workspace.path());
        VERSION_FILES.iter().any(|f| {
            let now = std::fs::read_to_string(self.workspace.path().join(f)).ok();
            let next = repo.git(&["show", &format!("{sha}:{f}")]).ok();
            now.as_deref().map(str::trim) != next.as_deref().map(str::trim)
        })
    }
}

impl Drop for WorktreeRevisions {
    fn drop(&mut self) {
        self.bazel.shutdown();
    }
}

impl Revisions for WorktreeRevisions {
    fn querier(&self, sha: &str) -> Result<Box<dyn PackageQuerier + '_>> {
        use std::sync::atomic::Ordering;
        // Stopped under the version that started it, before the checkout
        // moves to one that names another (see the type's docs).
        if self.served.load(Ordering::Relaxed) && self.moves_bazel(sha) {
            self.bazel.shutdown();
        }
        self.served.store(true, Ordering::Relaxed);
        self.workspace.checkout(sha)?;
        Ok(Box::new(BazelQuerier {
            bazel: self.bazel.clone(),
            opts: self.opts.clone(),
            files: FileSource::worktree(self.workspace.path())
                .with_fallback_rev(Some(sha.to_string())),
            diags: self.diags.clone(),
        }))
    }
}

/// How many commits in a row may fail before the walk stops and reports what
/// it has. One failure is an old commit that no longer builds (a fetch that
/// moved, a toolchain that is gone); three in a row is a stretch of history
/// that will not build here, and walking it would only pay for more failures.
pub const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// What a caller wants told while the walk runs, since a profile over a real
/// repository takes minutes and the terminal should not go quiet.
///
/// This is the whole of what the engine says about a run in flight: it opens
/// and closes a phase, and reports one row per commit. Nothing here prints;
/// the CLI renders it through `cambium_ui`, and a caller that wants nothing
/// told passes a callback that drops it.
#[derive(Clone, Debug)]
pub enum Progress {
    /// The range, known before any work starts: where the walk begins, where
    /// it ends, and how many commits lie between.
    Range {
        from: String,
        head: String,
        commits: usize,
    },
    /// The detached worktree is being created at `commit`.
    Worktree { commit: String },
    /// It is ready. Emitted by whoever created it, which for the real walk is
    /// the caller: the worktree outlives one `profile` call.
    WorktreeReady { elapsed_ms: u64 },
    /// The cold build at the range's start is running.
    ColdBuild { commit: String },
    /// It finished, with the size of the graph it found.
    ColdBuilt { targets: u32, elapsed_ms: u64 },
    /// It failed. The walk goes on: the first commit after it is indexed
    /// cold in its place.
    ColdFailed {
        commit: String,
        error: Box<Diagnostic>,
        elapsed_ms: u64,
    },
    /// Step `index` of `of` is running.
    Step {
        index: usize,
        of: usize,
        commit: String,
    },
    /// Step `index` went global for `reason`; a cold build at its commit is
    /// running in its place. `code` is the diagnostic that explains it.
    Rebuild {
        index: usize,
        commit: String,
        reason: String,
        code: Option<Code>,
    },
    /// Step `index` has no index at its parent, because building one failed,
    /// so its own commit is being indexed cold.
    Recover { index: usize, commit: String },
    /// Step `index` is over: one row, ready to print. A step that failed
    /// carries its error in `step.failed`.
    Stepped {
        index: usize,
        of: usize,
        step: Box<CommitStep>,
        subject: String,
    },
    /// The walk stopped before the head, after
    /// [`MAX_CONSECUTIVE_FAILURES`] commits in a row failed.
    Stopped { reason: String },
    /// Every step is over.
    Walked {
        commits: usize,
        globals: usize,
        elapsed_ms: u64,
    },
}

/// The index at the head of the range, as a reader would describe it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexSummary {
    pub commit: String,
    /// Rules, which is what someone means by "targets".
    pub targets: u32,
    pub tests: u32,
    pub packages: u32,
    pub nodes: u32,
    pub edges: u64,
    pub external_repos: u32,
    /// The file as written, in the form `[baseline].compress_indexes` chooses.
    pub bytes_on_disk: u64,
    /// `none`, or `zstd-<level>`.
    pub compression: String,
    /// What opening that file costs, which every `cambi affected` pays
    /// before the numbers in `commits` begin.
    pub load_ms: u64,
    pub bazel_version: String,
}

/// The cold build the walk started from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColdBuild {
    /// Where it happened: the range's first commit, or, when that one failed
    /// to build, the first later commit that did.
    pub commit: String,
    pub wall_ms: u64,
    /// Rule targets in the index it built, manual ones included. The shares
    /// in `summary` are not of this but of `index.targets`, the repository
    /// at the walk's end: a window that starts where the repository was
    /// small would otherwise read "200% of 1".
    #[serde(default)]
    pub targets: u32,
    /// Test rules in the same index.
    #[serde(default)]
    pub tests: u32,
}

/// What a step did to the stored index, which is the part of a walk that is
/// not visible from the times alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IndexWork {
    /// The commit changed nothing the index models: the index at head is the
    /// base, byte for byte.
    Identical,
    /// The base was spliced, rewriting this many target rows. The real
    /// number, counted by the splice itself
    /// ([`crate::report::Stats::spliced_rows`]), not an estimate from the
    /// diff.
    Spliced { rows_rewritten: usize },
    /// The index was rebuilt from `//...`, because the commit went global,
    /// because the dirty set exceeded `limits.max_dirty_packages`, or because
    /// the parent had no index (building it failed).
    Rebuilt { reason: String },
    /// The step failed, so there is no index at this commit; its error is
    /// the step's `failed`.
    Failed,
}

/// One commit forward.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitStep {
    pub commit: String,
    pub parent: String,
    /// The commit date, `MM-DD HH:MM` in the local zone, so a row can be read
    /// against the calendar without a second git call.
    #[serde(default)]
    pub date: String,
    /// The commit's subject line, as the table prints it.
    #[serde(default)]
    pub subject: String,
    pub changed_files: usize,
    pub dirty_packages: usize,
    /// Affected rule targets, manual ones included. Zero on a global step
    /// (whose answer is `//...`, see `global`), a failed one, and one indexed
    /// cold after a failure (which answered nothing).
    pub affected: usize,
    pub tests: usize,
    /// The engine's time for the step. When the step went global this is the
    /// cold build that replaced it, since that is what the commit cost.
    pub wall_ms: u64,
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    /// The diagnostic that explains `global`: its `CAMBI-W` code, the reason
    /// and the `help:` line naming what would change it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_cause: Option<Diagnostic>,
    /// The engine re-indexed `//...` because the dirty set exceeded
    /// `limits.max_dirty_packages`. The answer is exact and the lineage
    /// survives; only the cost is a cold build's.
    pub engine_cold_rebuild: bool,
    /// The parent had no index, because building it failed, so this commit
    /// was indexed cold and answered nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cold_after_failure: bool,
    /// Why the step failed: Bazel could not query the commit, a fetch
    /// failed, git could not diff it. The walk went on without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<Diagnostic>,
    /// What the step did to the index.
    #[serde(default = "IndexWork::unknown")]
    pub index: IndexWork,
}

impl IndexWork {
    /// A step read back from a document written before this field existed.
    fn unknown() -> Self {
        IndexWork::Identical
    }
}

impl CommitStep {
    /// A step before any of it has run.
    fn start(parent: &str, commit: &str, meta: &CommitMeta) -> Self {
        Self {
            commit: commit.to_string(),
            parent: parent.to_string(),
            date: meta.date.clone(),
            subject: meta.subject.clone(),
            changed_files: 0,
            dirty_packages: 0,
            affected: 0,
            tests: 0,
            wall_ms: 0,
            global: false,
            global_reason: None,
            global_cause: None,
            engine_cold_rebuild: false,
            cold_after_failure: false,
            failed: None,
            index: IndexWork::Failed,
        }
    }

    /// A cold build answered it: a global, an engine re-query of `//...`, or
    /// a commit indexed cold after a failure.
    pub fn rebuilt(&self) -> bool {
        self.global || self.engine_cold_rebuild || self.cold_after_failure
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Steps walked, failed ones included. Every other figure but `failed`
    /// is over the steps that did not fail.
    pub commits: usize,
    /// Over every step, rebuilds included: what the walk cost per commit.
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub max_ms: u64,
    /// Over the steps that were answered incrementally — no global, no
    /// engine re-query of `//...` — which is the number a commit usually
    /// costs. A handful of rebuilds otherwise put the mean minutes away from
    /// the median.
    pub incremental: Latency,
    /// Over the steps a cold build answered: globals, engine re-queries, and
    /// commits indexed cold after a failure.
    pub rebuild: Latency,
    /// Commits that invalidated everything and cost a cold build.
    pub globals: usize,
    /// `commits / globals`: how many commits, on average, between one whole
    /// rebuild and the next. Absent when nothing went global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild_every_commits: Option<f64>,
    /// Why the globals went global, by reason, most frequent first.
    pub global_reasons: Vec<ReasonCount>,
    pub engine_cold_rebuilds: usize,
    /// The mean of `commits[].affected`, a global step counting as 0.
    /// `incremental_affected` and `affected_share` are what a typical commit
    /// affects; `global_share` is how often a commit affects everything.
    pub mean_affected: f64,
    /// Fraction of commits whose diff dirtied no package.
    pub zero_dirty_fraction: f64,
    /// `rebuild` for the globals alone.
    #[serde(default)]
    pub global_rebuild: Latency,
    /// `rebuild` for the engine re-queries alone: the dirty set exceeded
    /// `limits.max_dirty_packages`, so the answer is exact but the commit
    /// cost a cold build.
    #[serde(default)]
    pub requery: Latency,
    /// Affected rule targets (manual ones included) over the steps answered
    /// incrementally.
    #[serde(default)]
    pub incremental_affected: Spread,
    /// Affected tests over the same steps.
    #[serde(default)]
    pub incremental_tests: Spread,
    /// `incremental_affected.p50` over `index.targets`: the share of the
    /// repository's rule targets, at the commit the walk ends on, that a
    /// typical commit affects. Of `cold.targets` before schema 21.
    #[serde(default)]
    pub affected_share: f64,
    /// `incremental_tests.p50` over `index.tests` (of `cold.tests` before
    /// schema 21).
    #[serde(default)]
    pub tests_share: f64,
    /// `globals / commits`: the share of commits whose answer is `//...`.
    #[serde(default)]
    pub global_share: f64,
    /// Steps that spliced rows into the index.
    #[serde(default)]
    pub index_spliced: usize,
    /// Steps that changed nothing the index models.
    #[serde(default)]
    pub index_unchanged: usize,
    /// Steps that failed, left out of every figure above but `commits`.
    #[serde(default)]
    pub failed: usize,
    /// Steps indexed cold because their parent failed.
    #[serde(default)]
    pub rebuilt_after_failure: usize,
}

/// Latency over one class of steps.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Latency {
    pub commits: usize,
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub max_ms: u64,
}

impl Latency {
    fn over(walls: impl Iterator<Item = u64>) -> Self {
        let mut walls: Vec<u64> = walls.collect();
        walls.sort_unstable();
        let n = walls.len();
        Self {
            commits: n,
            mean_ms: if n == 0 {
                0
            } else {
                walls.iter().sum::<u64>() / n as u64
            },
            p50_ms: percentile(&walls, 0.5),
            p90_ms: percentile(&walls, 0.9),
            max_ms: walls.last().copied().unwrap_or(0),
        }
    }
}

/// A count over one class of steps: its mean and nearest-rank percentiles.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    pub mean: f64,
    pub p50: u64,
    pub p90: u64,
}

impl Spread {
    fn over(values: impl Iterator<Item = u64>) -> Self {
        let mut values: Vec<u64> = values.collect();
        values.sort_unstable();
        let n = values.len();
        Self {
            mean: if n == 0 {
                0.0
            } else {
                values.iter().sum::<u64>() as f64 / n as f64
            },
            p50: percentile(&values, 0.5),
            p90: percentile(&values, 0.9),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasonCount {
    pub reason: String,
    pub count: usize,
    /// The `CAMBI-W` code of the diagnostic that explains this reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    /// Its `help:` line: what would change it, or that nothing needs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// The document `cambi profile` prints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub schema: u32,
    /// The workspace directory's name, which the report is headed with.
    #[serde(default)]
    pub repository: String,
    pub head: String,
    /// The oldest commit of the range, where the cold build was meant to
    /// happen.
    pub from: String,
    /// Why the cold build at `from` failed, when it did; `cold.commit` is
    /// then the first later commit that built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_failed: Option<Diagnostic>,
    pub index: IndexSummary,
    pub cold: ColdBuild,
    pub commits: Vec<CommitStep>,
    pub summary: Summary,
    /// Why the walk stopped before the head, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<crate::diag::Diagnostic>,
}

/// Walks the last `commits` first-parent commits up to `head` and writes the
/// index the walk ends with to `out`.
///
/// A range shorter than asked for — a young repository — is walked as far as
/// it goes; one commit means the cold build alone and an empty `commits`.
///
/// A commit that fails (Bazel cannot query it, a fetch fails) is a step with
/// `failed` set, not the end of the profile: the next commit is indexed cold
/// and the walk goes on, until [`MAX_CONSECUTIVE_FAILURES`] in a row stop it.
/// The index written to `out` is then the last one that built, which is
/// `index.commit`. Only a walk in which nothing built at all is an error:
/// the first failure's.
pub fn profile(
    engine: &Engine,
    revs: &dyn Revisions,
    head: &str,
    commits: usize,
    out: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<Profile> {
    let head = engine.repo().rev_parse(head)?;
    let meta = engine.repo().first_parent_log_meta(&head, commits + 1)?;
    let log: Vec<String> = meta.iter().map(|c| c.sha.clone()).collect();
    let from = log.first().cloned().unwrap_or_else(|| head.clone());
    let steps = log.len().saturating_sub(1);
    progress(Progress::Range {
        from: from.clone(),
        head: head.clone(),
        commits: steps,
    });

    let mut diagnostics = Vec::new();
    let mut first_error: Option<EngineError> = None;
    // The index the walk holds, and the commit it is at. A step is answered
    // incrementally only when this is its parent.
    let mut index: Option<(String, IndexData)> = None;
    let mut cold: Option<ColdBuild> = None;
    let mut from_failed = None;
    let mut consecutive = 0;

    progress(Progress::ColdBuild {
        commit: from.clone(),
    });
    let sw = Stopwatch::start();
    match revs
        .querier(&from)
        .and_then(|q| engine.full_build_with(&from, &*q))
    {
        Ok(built) => {
            let c = built.view().counts();
            let build = ColdBuild {
                commit: from.clone(),
                wall_ms: sw.elapsed_ms(),
                targets: c.rules,
                tests: c.tests,
            };
            progress(Progress::ColdBuilt {
                targets: c.rules,
                elapsed_ms: build.wall_ms,
            });
            cold = Some(build);
            index = Some((from.clone(), built));
        }
        Err(e) => {
            let d = e.diagnostic();
            progress(Progress::ColdFailed {
                commit: from.clone(),
                error: Box::new(d.clone()),
                elapsed_ms: sw.elapsed_ms(),
            });
            from_failed = Some(d);
            first_error = Some(e);
            consecutive = 1;
        }
    }
    diagnostics.extend(engine.diagnostics().drain());

    let walk_sw = Stopwatch::start();
    let mut walked = Vec::with_capacity(steps);
    let mut stopped = None;
    for (i, pair) in log.windows(2).enumerate() {
        let (parent, commit) = (&pair[0], &pair[1]);
        if consecutive >= MAX_CONSECUTIVE_FAILURES {
            let reason = format!(
                "{consecutive} commits in a row failed; {} of {steps} walked",
                walked.len()
            );
            progress(Progress::Stopped {
                reason: reason.clone(),
            });
            stopped = Some(reason);
            break;
        }
        progress(Progress::Step {
            index: i + 1,
            of: steps,
            commit: commit.clone(),
        });
        let mut step = CommitStep::start(parent, commit, &meta[i + 1]);
        let sw = Stopwatch::start();
        let result = match &index {
            Some((at, base)) if at == parent => {
                incremental(engine, revs, base, &mut step, i + 1, progress)
            }
            _ => {
                progress(Progress::Recover {
                    index: i + 1,
                    commit: commit.clone(),
                });
                step.cold_after_failure = true;
                revs.querier(commit).and_then(|q| {
                    let built = engine.full_build_with(commit, &*q)?;
                    step.wall_ms = sw.elapsed_ms();
                    step.index = IndexWork::Rebuilt {
                        reason: "the parent has no index (it failed), so this commit was \
                                 indexed cold"
                            .to_string(),
                    };
                    Ok(built)
                })
            }
        };
        diagnostics.extend(engine.diagnostics().drain());
        match result {
            Ok(next) => {
                if cold.is_none() {
                    let c = next.view().counts();
                    cold = Some(ColdBuild {
                        commit: commit.clone(),
                        wall_ms: step.wall_ms,
                        targets: c.rules,
                        tests: c.tests,
                    });
                }
                index = Some((commit.clone(), next));
                consecutive = 0;
            }
            Err(e) => {
                step.wall_ms = sw.elapsed_ms();
                step.index = IndexWork::Failed;
                step.failed = Some(e.diagnostic());
                first_error.get_or_insert(e);
                consecutive += 1;
            }
        }
        progress(Progress::Stepped {
            index: i + 1,
            of: steps,
            step: Box::new(step.clone()),
            subject: meta[i + 1].subject.clone(),
        });
        walked.push(step);
    }
    progress(Progress::Walked {
        commits: walked.len(),
        globals: walked.iter().filter(|s| s.global).count(),
        elapsed_ms: walk_sw.elapsed_ms(),
    });

    // Nothing built anywhere: there is no index to describe or keep, so the
    // walk ends the way a single failing build always has.
    let (Some((_, index)), Some(cold)) = (index, cold) else {
        return Err(first_error.expect("a walk with no index failed somewhere"));
    };

    let compression = engine.index_compression();
    cambium_index::format::write_index_with(out, &index, compression)?;
    let bytes_on_disk = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    let load_ms = {
        let sw = Stopwatch::start();
        let f = IndexFile::open(out)?;
        let _ = f.view().counts();
        sw.elapsed_ms()
    };
    let described = describe(&index, bytes_on_disk, compression, load_ms);
    let summary = summarise(&walked, described.targets, described.tests);
    diagnostics.extend(engine.diagnostics().drain());
    Ok(Profile {
        schema: SCHEMA_VERSION,
        repository: engine
            .workspace()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        head: head.clone(),
        from,
        from_failed,
        index: described,
        cold,
        commits: walked,
        summary,
        stopped,
        diagnostics,
    })
}

/// One step answered from the index at its parent: the engine's run, and the
/// cold build that replaces it when the commit went global. Fills `step` as
/// it goes, so a step that fails half way still says what it got to (a
/// global whose rebuild failed is still a global).
fn incremental(
    engine: &Engine,
    revs: &dyn Revisions,
    base: &IndexData,
    step: &mut CommitStep,
    index: usize,
    progress: &mut dyn FnMut(Progress),
) -> Result<IndexData> {
    let (parent, commit) = (step.parent.clone(), step.commit.clone());
    let changes = engine.repo().diff(&parent, &commit)?;
    let q = revs.querier(&commit)?;
    let req = AffectedRequest {
        include_manual: true,
        no_full_build: true,
        materialize: true,
        ..AffectedRequest::new(parent.clone(), commit.clone())
    };
    let sw = Stopwatch::start();
    let run = engine.compute(base.view(), &changes, &*q, &req)?;
    step.wall_ms = sw.elapsed_ms();
    step.changed_files = run.report.stats.changed_files;
    step.dirty_packages = run.report.stats.dirty_packages;
    step.affected = run.report.affected.len();
    step.tests = run.report.tests.len();
    step.global = run.report.global;
    step.global_reason = run.report.global_reason.clone();
    step.engine_cold_rebuild = run.report.stats.cold_rebuild;
    step.index = if run.report.stats.cold_rebuild {
        IndexWork::Rebuilt {
            reason: run
                .report
                .cold_rebuild_reason
                .clone()
                .unwrap_or_else(|| "the dirty set exceeded the limit".to_string()),
        }
    } else if run.report.stats.spliced_rows == 0 {
        IndexWork::Identical
    } else {
        IndexWork::Spliced {
            rows_rewritten: run.report.stats.spliced_rows,
        }
    };
    if step.global {
        // `compute` leaves its diagnostics in the engine's sink rather than
        // on the report; the one that explains the global is read from there
        // and put back, so the document's `diagnostics` still holds it.
        let raised = engine.diagnostics().drain();
        step.global_cause = raised
            .iter()
            .find(|d| crate::reason::GlobalReason::is_cause(d.code))
            .cloned();
        for d in raised {
            engine.diagnostics().emit(d);
        }
    }
    match run.index {
        Some(next) => Ok(next),
        None => {
            // The range invalidated everything: what the commit costs is
            // the cold build that follows, exactly as `cambi index update`
            // would pay it.
            progress(Progress::Rebuild {
                index,
                commit: commit.clone(),
                reason: step.global_reason.clone().unwrap_or_default(),
                code: step.global_cause.as_ref().map(|d| d.code),
            });
            let sw = Stopwatch::start();
            let built = engine.full_build_with(&commit, &*q)?;
            step.wall_ms = sw.elapsed_ms();
            step.index = IndexWork::Rebuilt {
                reason: step
                    .global_reason
                    .clone()
                    .unwrap_or_else(|| "the commit invalidated everything".to_string()),
            };
            Ok(built)
        }
    }
}

fn describe(
    index: &IndexData,
    bytes_on_disk: u64,
    compression: Compression,
    load_ms: u64,
) -> IndexSummary {
    let v = index.view();
    let c = v.counts();
    IndexSummary {
        commit: v.commit.to_string(),
        targets: c.rules,
        tests: c.tests,
        packages: c.packages,
        nodes: c.nodes,
        edges: c.edges,
        external_repos: v.num_repos(),
        bytes_on_disk,
        compression: match compression {
            Compression::None => "none".to_string(),
            Compression::Zstd(level) => format!("zstd-{level}"),
        },
        load_ms,
        bazel_version: v.bazel_version.to_string(),
    }
}

/// The summary of a walk. `targets` and `tests` are the counts the shares are
/// shares of: the index the walk ends with, which is the repository the
/// report's `targets` line describes.
///
/// A failed step is counted in `commits` and `failed` and nowhere else: it
/// answered nothing and its time is not what a commit costs.
pub fn summarise(steps: &[CommitStep], targets: u32, tests: u32) -> Summary {
    let failed = steps.iter().filter(|s| s.failed.is_some()).count();
    let walked = steps.len();
    let steps: Vec<&CommitStep> = steps.iter().filter(|s| s.failed.is_none()).collect();
    let all = Latency::over(steps.iter().map(|s| s.wall_ms));
    let n = steps.len();
    let globals = steps.iter().filter(|s| s.global).count();
    let mut by_reason: BTreeMap<String, ReasonCount> = BTreeMap::new();
    for s in steps.iter().filter(|s| s.global) {
        let reason = s.global_reason.clone().unwrap_or_else(|| "global".into());
        let entry = by_reason
            .entry(reason.clone())
            .or_insert_with(|| ReasonCount {
                reason,
                count: 0,
                code: None,
                help: None,
            });
        entry.count += 1;
        if entry.code.is_none() {
            if let Some(cause) = &s.global_cause {
                entry.code = Some(cause.code);
                entry.help = cause.help.clone();
            }
        }
    }
    let mut global_reasons: Vec<ReasonCount> = by_reason.into_values().collect();
    global_reasons.sort_by(|a, b| b.count.cmp(&a.count).then(a.reason.cmp(&b.reason)));
    let incremental: Vec<&&CommitStep> = steps.iter().filter(|s| !s.rebuilt()).collect();
    let incremental_affected = Spread::over(incremental.iter().map(|s| s.affected as u64));
    let incremental_tests = Spread::over(incremental.iter().map(|s| s.tests as u64));
    let share = |part: u64, whole: u32| {
        if whole == 0 {
            0.0
        } else {
            part as f64 / whole as f64
        }
    };
    let fraction = |count: usize| {
        if n == 0 {
            0.0
        } else {
            count as f64 / n as f64
        }
    };
    Summary {
        commits: walked,
        mean_ms: all.mean_ms,
        p50_ms: all.p50_ms,
        p90_ms: all.p90_ms,
        max_ms: all.max_ms,
        incremental: Latency::over(incremental.iter().map(|s| s.wall_ms)),
        rebuild: Latency::over(steps.iter().filter(|s| s.rebuilt()).map(|s| s.wall_ms)),
        globals,
        rebuild_every_commits: (globals > 0).then(|| n as f64 / globals as f64),
        global_reasons,
        engine_cold_rebuilds: steps.iter().filter(|s| s.engine_cold_rebuild).count(),
        mean_affected: if n == 0 {
            0.0
        } else {
            steps.iter().map(|s| s.affected as f64).sum::<f64>() / n as f64
        },
        zero_dirty_fraction: fraction(
            steps
                .iter()
                .filter(|s| s.dirty_packages == 0 && !s.global)
                .count(),
        ),
        global_rebuild: Latency::over(steps.iter().filter(|s| s.global).map(|s| s.wall_ms)),
        requery: Latency::over(
            steps
                .iter()
                .filter(|s| s.engine_cold_rebuild)
                .map(|s| s.wall_ms),
        ),
        affected_share: share(incremental_affected.p50, targets),
        tests_share: share(incremental_tests.p50, tests),
        incremental_affected,
        incremental_tests,
        global_share: fraction(globals),
        index_spliced: steps
            .iter()
            .filter(|s| matches!(s.index, IndexWork::Spliced { .. }))
            .count(),
        index_unchanged: steps
            .iter()
            .filter(|s| matches!(s.index, IndexWork::Identical))
            .count(),
        failed,
        rebuilt_after_failure: steps.iter().filter(|s| s.cold_after_failure).count(),
    }
}

/// Nearest-rank percentile of a sorted slice; 0 for an empty one.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(wall_ms: u64, global: Option<&str>, dirty: usize) -> CommitStep {
        CommitStep {
            commit: "c".into(),
            parent: "p".into(),
            date: "09-02 09:14".into(),
            subject: "a commit".into(),
            changed_files: 1,
            dirty_packages: dirty,
            affected: if global.is_some() { 0 } else { 4 },
            tests: if global.is_some() { 0 } else { 1 },
            wall_ms,
            global: global.is_some(),
            global_reason: global.map(String::from),
            global_cause: global.map(|reason| Diagnostic {
                help: Some(format!("help for {reason}")),
                ..Diagnostic::warning(Code::W013InvalidatorMatched, reason)
            }),
            engine_cold_rebuild: false,
            cold_after_failure: false,
            failed: None,
            index: match global {
                Some(reason) => IndexWork::Rebuilt {
                    reason: reason.to_string(),
                },
                None => IndexWork::Spliced { rows_rewritten: 12 },
            },
        }
    }

    #[test]
    fn rebuild_cadence_is_commits_per_global_and_absent_without_one() {
        let s = summarise(
            &[
                step(10, None, 1),
                step(20, Some(".bazelrc changed"), 0),
                step(30, None, 0),
                step(4000, Some(".bazelrc changed"), 0),
                step(40, Some("MODULE.bazel changed"), 0),
                step(50, None, 2),
            ],
            40,
            10,
        );
        assert_eq!(s.commits, 6);
        assert_eq!(s.globals, 3);
        assert_eq!(s.rebuild_every_commits, Some(2.0));
        assert_eq!(
            s.global_reasons,
            vec![
                ReasonCount {
                    reason: ".bazelrc changed".into(),
                    count: 2,
                    code: Some(Code::W013InvalidatorMatched),
                    help: Some("help for .bazelrc changed".into()),
                },
                ReasonCount {
                    reason: "MODULE.bazel changed".into(),
                    count: 1,
                    code: Some(Code::W013InvalidatorMatched),
                    help: Some("help for MODULE.bazel changed".into()),
                }
            ]
        );
        assert_eq!(s.p50_ms, 30);
        assert_eq!(s.max_ms, 4000);
        assert_eq!(s.mean_ms, (10 + 20 + 30 + 4000 + 40 + 50) / 6);
        assert_eq!(s.incremental.commits, 3);
        assert_eq!(s.incremental.mean_ms, 30);
        assert_eq!(s.incremental.max_ms, 50);
        assert_eq!(s.rebuild.commits, 3);
        assert_eq!(s.rebuild.max_ms, 4000);
        assert_eq!(s.global_rebuild, s.rebuild);
        assert_eq!(s.requery.commits, 0);
        // A global with no dirty packages is not a source-only commit.
        assert!((s.zero_dirty_fraction - 1.0 / 6.0).abs() < 1e-9);
        assert!((s.global_share - 0.5).abs() < 1e-9);

        let quiet = summarise(&[step(10, None, 1)], 40, 10);
        assert_eq!(quiet.rebuild_every_commits, None);
        assert!(quiet.global_reasons.is_empty());
    }

    #[test]
    fn a_typical_commit_is_the_incremental_median_and_globals_do_not_drag_it_to_zero() {
        let mut steps = vec![step(5000, Some(".bazelrc changed"), 0)];
        for (affected, tests) in [(10, 2), (20, 4), (30, 6), (40, 8)] {
            let mut s = step(100, None, 1);
            s.affected = affected;
            s.tests = tests;
            steps.push(s);
        }
        let s = summarise(&steps, 200, 20);
        // The existing mean keeps its meaning: the global counts as 0.
        assert_eq!(s.mean_affected, (10 + 20 + 30 + 40) as f64 / 5.0);
        assert_eq!(s.incremental_affected.p50, 20);
        assert_eq!(s.incremental_affected.p90, 40);
        assert_eq!(s.incremental_affected.mean, 25.0);
        assert_eq!(s.incremental_tests.p50, 4);
        assert!((s.affected_share - 0.1).abs() < 1e-9);
        assert!((s.tests_share - 0.2).abs() < 1e-9);
        assert!((s.global_share - 0.2).abs() < 1e-9);
        assert_eq!(s.index_spliced, 4);
    }

    #[test]
    fn a_requery_and_a_global_are_counted_apart() {
        let mut requery = step(700, None, 90);
        requery.engine_cold_rebuild = true;
        requery.index = IndexWork::Rebuilt {
            reason: "over the limit".into(),
        };
        let s = summarise(
            &[step(10, None, 1), requery, step(3000, Some("x"), 0)],
            10,
            1,
        );
        assert_eq!(s.rebuild.commits, 2);
        assert_eq!(s.requery.commits, 1);
        assert_eq!(s.requery.mean_ms, 700);
        assert_eq!(s.global_rebuild.commits, 1);
        assert_eq!(s.global_rebuild.mean_ms, 3000);
        assert_eq!(s.incremental.commits, 1);
    }

    #[test]
    fn a_failed_step_is_counted_and_left_out_of_every_figure() {
        let mut failed = step(90_000, None, 0);
        failed.failed = Some(Diagnostic::error(Code::E005BazelQuery, "no such package"));
        failed.index = IndexWork::Failed;
        let mut recovered = step(8000, None, 0);
        recovered.cold_after_failure = true;
        recovered.index = IndexWork::Rebuilt {
            reason: "the parent has no index".into(),
        };
        let s = summarise(
            &[step(10, None, 1), failed, recovered, step(30, None, 1)],
            10,
            1,
        );
        assert_eq!(s.commits, 4);
        assert_eq!(s.failed, 1);
        assert_eq!(s.rebuilt_after_failure, 1);
        assert_eq!(s.max_ms, 8000);
        assert_eq!(s.incremental.commits, 2);
        assert_eq!(s.rebuild.commits, 1);
        assert_eq!(s.globals, 0);
    }

    #[test]
    fn an_empty_walk_summarises_to_zeros() {
        let s = summarise(&[], 0, 0);
        assert_eq!(s.commits, 0);
        assert_eq!(s.p90_ms, 0);
        assert_eq!(s.mean_affected, 0.0);
        assert_eq!(s.affected_share, 0.0);
    }
}
