//! `cambi profile`: what cambium costs and saves on *this* repository.
//!
//! The question a team asks before adopting the tool is not "is it correct"
//! (the benchmark answers that on the corpus) but "what does it do on our
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

use crate::diag::Diagnostics;
use crate::engine::{AffectedRequest, Engine};
use crate::error::Result;
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
pub struct WorktreeRevisions {
    workspace: RevisionWorkspace,
    bazel: Bazel,
    opts: ConvertOptions,
    diags: Diagnostics,
}

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

/// What a caller wants told while the walk runs, since a profile over a real
/// repository takes minutes and the terminal should not go quiet.
#[derive(Clone, Debug)]
pub enum Progress {
    /// The cold build at the range's start is running.
    ColdBuild { commit: String },
    /// Step `index` of `of` is running.
    Step {
        index: usize,
        of: usize,
        commit: String,
    },
    /// Step `index` went global for `reason`; a cold build at its commit is
    /// running in its place.
    Rebuild {
        index: usize,
        commit: String,
        reason: String,
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
    pub commit: String,
    pub wall_ms: u64,
}

/// One commit forward.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitStep {
    pub commit: String,
    pub parent: String,
    pub changed_files: usize,
    pub dirty_packages: usize,
    pub affected: usize,
    pub tests: usize,
    /// The engine's time for the step. When the step went global this is the
    /// cold build that replaced it, since that is what the commit cost.
    pub wall_ms: u64,
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    /// The engine re-indexed `//...` because the dirty set exceeded
    /// `limits.max_dirty_packages`. The answer is exact and the lineage
    /// survives; only the cost is a cold build's.
    pub engine_cold_rebuild: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub commits: usize,
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub max_ms: u64,
    /// Commits that invalidated everything and cost a cold build.
    pub globals: usize,
    /// `commits / globals`: how many commits, on average, between one whole
    /// rebuild and the next. Absent when nothing went global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild_every_commits: Option<f64>,
    /// Why the globals went global, by reason, most frequent first.
    pub global_reasons: Vec<ReasonCount>,
    pub engine_cold_rebuilds: usize,
    pub mean_affected: f64,
    /// Fraction of commits whose diff dirtied no package.
    pub zero_dirty_fraction: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasonCount {
    pub reason: String,
    pub count: usize,
}

/// The document `cambi profile` prints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub schema: u32,
    pub head: String,
    /// The oldest commit of the range, where the cold build happened.
    pub from: String,
    pub index: IndexSummary,
    pub cold: ColdBuild,
    pub commits: Vec<CommitStep>,
    pub summary: Summary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<crate::diag::Diagnostic>,
}

/// Walks the last `commits` first-parent commits up to `head` and writes the
/// head index to `out`.
///
/// A range shorter than asked for — a young repository — is walked as far as
/// it goes; one commit means the cold build alone and an empty `commits`.
pub fn profile(
    engine: &Engine,
    revs: &dyn Revisions,
    head: &str,
    commits: usize,
    out: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<Profile> {
    let head = engine.repo().rev_parse(head)?;
    let log = engine.repo().first_parent_log(&head, commits + 1)?;
    let from = log.first().cloned().unwrap_or_else(|| head.clone());

    progress(Progress::ColdBuild {
        commit: from.clone(),
    });
    let mut index;
    let cold = {
        let q = revs.querier(&from)?;
        let sw = Stopwatch::start();
        index = engine.full_build_with(&from, &*q)?;
        ColdBuild {
            commit: from.clone(),
            wall_ms: sw.elapsed_ms(),
        }
    };

    let steps = log.len().saturating_sub(1);
    let mut walked = Vec::with_capacity(steps);
    for (i, pair) in log.windows(2).enumerate() {
        let (parent, commit) = (&pair[0], &pair[1]);
        progress(Progress::Step {
            index: i + 1,
            of: steps,
            commit: commit.clone(),
        });
        let changes = engine.repo().diff(parent, commit)?;
        let q = revs.querier(commit)?;
        let req = AffectedRequest {
            include_manual: true,
            no_full_build: true,
            materialize: true,
            ..AffectedRequest::new(parent.clone(), commit.clone())
        };
        let sw = Stopwatch::start();
        let run = engine.compute(index.view(), &changes, &*q, &req)?;
        let mut step = CommitStep {
            commit: commit.clone(),
            parent: parent.clone(),
            changed_files: run.report.stats.changed_files,
            dirty_packages: run.report.stats.dirty_packages,
            affected: run.report.affected.len(),
            tests: run.report.tests.len(),
            wall_ms: sw.elapsed_ms(),
            global: run.report.global,
            global_reason: run.report.global_reason.clone(),
            engine_cold_rebuild: run.report.stats.cold_rebuild,
        };
        match run.index {
            Some(next) => index = next,
            None => {
                // The range invalidated everything: what the commit costs is
                // the cold build that follows, exactly as `cambi index
                // update` would pay it.
                progress(Progress::Rebuild {
                    index: i + 1,
                    commit: commit.clone(),
                    reason: step.global_reason.clone().unwrap_or_default(),
                });
                let sw = Stopwatch::start();
                index = engine.full_build_with(commit, &*q)?;
                step.wall_ms = sw.elapsed_ms();
            }
        }
        walked.push(step);
    }

    let compression = engine.index_compression();
    cambium_index::format::write_index_with(out, &index, compression)?;
    let bytes_on_disk = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    let load_ms = {
        let sw = Stopwatch::start();
        let f = IndexFile::open(out)?;
        let _ = f.view().counts();
        sw.elapsed_ms()
    };
    let summary = summarise(&walked);
    Ok(Profile {
        schema: SCHEMA_VERSION,
        head: head.clone(),
        from,
        index: describe(&index, bytes_on_disk, compression, load_ms),
        cold,
        commits: walked,
        summary,
        diagnostics: engine.diagnostics().drain(),
    })
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

pub fn summarise(steps: &[CommitStep]) -> Summary {
    let mut walls: Vec<u64> = steps.iter().map(|s| s.wall_ms).collect();
    walls.sort_unstable();
    let n = steps.len();
    let globals = steps.iter().filter(|s| s.global).count();
    let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
    for s in steps.iter().filter(|s| s.global) {
        *by_reason
            .entry(s.global_reason.clone().unwrap_or_else(|| "global".into()))
            .or_default() += 1;
    }
    let mut global_reasons: Vec<ReasonCount> = by_reason
        .into_iter()
        .map(|(reason, count)| ReasonCount { reason, count })
        .collect();
    global_reasons.sort_by(|a, b| b.count.cmp(&a.count).then(a.reason.cmp(&b.reason)));
    Summary {
        commits: n,
        mean_ms: if n == 0 {
            0
        } else {
            walls.iter().sum::<u64>() / n as u64
        },
        p50_ms: percentile(&walls, 0.5),
        p90_ms: percentile(&walls, 0.9),
        max_ms: walls.last().copied().unwrap_or(0),
        globals,
        rebuild_every_commits: (globals > 0).then(|| n as f64 / globals as f64),
        global_reasons,
        engine_cold_rebuilds: steps.iter().filter(|s| s.engine_cold_rebuild).count(),
        mean_affected: if n == 0 {
            0.0
        } else {
            steps.iter().map(|s| s.affected as f64).sum::<f64>() / n as f64
        },
        zero_dirty_fraction: if n == 0 {
            0.0
        } else {
            steps
                .iter()
                .filter(|s| s.dirty_packages == 0 && !s.global)
                .count() as f64
                / n as f64
        },
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
            changed_files: 1,
            dirty_packages: dirty,
            affected: 4,
            tests: 1,
            wall_ms,
            global: global.is_some(),
            global_reason: global.map(String::from),
            engine_cold_rebuild: false,
        }
    }

    #[test]
    fn rebuild_cadence_is_commits_per_global_and_absent_without_one() {
        let s = summarise(&[
            step(10, None, 1),
            step(20, Some(".bazelrc changed"), 0),
            step(30, None, 0),
            step(4000, Some(".bazelrc changed"), 0),
            step(40, Some("MODULE.bazel changed"), 0),
            step(50, None, 2),
        ]);
        assert_eq!(s.commits, 6);
        assert_eq!(s.globals, 3);
        assert_eq!(s.rebuild_every_commits, Some(2.0));
        assert_eq!(
            s.global_reasons,
            vec![
                ReasonCount {
                    reason: ".bazelrc changed".into(),
                    count: 2
                },
                ReasonCount {
                    reason: "MODULE.bazel changed".into(),
                    count: 1
                }
            ]
        );
        assert_eq!(s.p50_ms, 30);
        assert_eq!(s.max_ms, 4000);
        assert_eq!(s.mean_ms, (10 + 20 + 30 + 4000 + 40 + 50) / 6);
        // A global with no dirty packages is not a source-only commit.
        assert!((s.zero_dirty_fraction - 1.0 / 6.0).abs() < 1e-9);

        let quiet = summarise(&[step(10, None, 1)]);
        assert_eq!(quiet.rebuild_every_commits, None);
        assert!(quiet.global_reasons.is_empty());
    }

    #[test]
    fn an_empty_walk_summarises_to_zeros() {
        let s = summarise(&[]);
        assert_eq!(s.commits, 0);
        assert_eq!(s.p90_ms, 0);
        assert_eq!(s.mean_affected, 0.0);
    }
}
