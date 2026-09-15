//! Running one strategy over one subject: the steps, the two timed calls, the
//! oracle, the result file.
//!
//! Everything a strategy sees is decided here, so two strategies on a subject
//! see the same steps in the same order under the same Bazel. The step choice
//! is the walk's own ([`crate::walk::steps`] tagged, [`crate::walk::sample`]
//! drawn at a fixed seed with at least one step per tag), which also means a
//! step measured here is a step the walk measures, and the two numbers are
//! about the same commit.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use cambium_index::IndexFile;

use crate::oracles::rdeps::{OracleInputs, RdepsOracle};
use crate::session::SubjectRun;

use super::{
    changed_paths, score, CambiStrategy, RunResult, StepContext, StepMeasurement, Strategy,
    DEFAULT_STEPS, PER_TAG,
};

/// What `skyframe` was asked to do.
#[derive(Clone, Debug)]
pub struct SkyframeRequest {
    pub strategy: String,
    /// How many steps to measure.
    pub steps: usize,
    /// The sampling seed. Two strategies must share it or they are not
    /// measured on the same steps.
    pub seed: u64,
}

impl Default for SkyframeRequest {
    fn default() -> Self {
        Self {
            strategy: "cambi".to_string(),
            steps: DEFAULT_STEPS,
            seed: 0,
        }
    }
}

/// What a run answered with.
pub struct Measured {
    pub result: RunResult,
    pub path: PathBuf,
}

/// Runs one strategy over one subject and writes
/// `<results>/skyframe/<strategy>-<subject>.json`.
pub fn run_skyframe(run: &SubjectRun<'_>, req: &SkyframeRequest) -> Result<Measured> {
    let checkout = run.checkout()?;
    let engine = checkout.engine(&run.entry, run.version())?;
    let scratch_root = run.results.join("skyframe-scratch");
    std::fs::create_dir_all(&scratch_root)?;
    let _servers = crate::servers::StopOnDrop::subject(
        &checkout.dir,
        &scratch_root,
        engine.bazel().startup_flags(),
    );
    let cambi = run.cambi(&checkout)?;
    let env = crate::checkout::bazelisk_env(&checkout.dir, run.version());

    let (paths, globs) = run.entry.invalidator_rules();
    let probes = crate::walk::Probes {
        // Best effort, as in the walk: without an index at the pin there are
        // no hub packages, which costs the `hub` tag its steps and nothing
        // else.
        hub_packages: run.hub_packages().unwrap_or_default(),
        invalidator_paths: paths,
        invalidator_globs: globs,
    };
    let (_, all) = crate::walk::steps(
        &checkout.repo,
        &run.entry.pin,
        run.entry.history_window,
        &probes,
    )?;
    anyhow::ensure!(!all.is_empty(), "the window holds no steps");
    let sample = crate::walk::sample(&all, req.seed, PER_TAG, Some(req.steps));
    let chosen: Vec<&crate::walk::Step> = sample.chosen.iter().map(|&i| &all[i]).collect();

    println!(
        "{} on {}: {} step(s), seed {}",
        req.strategy,
        run.entry.id,
        chosen.len(),
        req.seed
    );
    for s in &chosen {
        let tags: Vec<&str> = s.tags.iter().map(|t| t.name()).collect();
        println!("  {}..{}  [{}]", short(&s.parent), short(&s.head), tags.join(","));
    }

    let mut strategy = open(&req.strategy, &cambi, &run.results)?;
    let mut result = RunResult {
        strategy: strategy.name().to_string(),
        subject: run.entry.id.clone(),
        bazel_version: engine
            .bazel()
            .version()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        started: crate::util::now_unix(),
        steps: Vec::new(),
    };
    let path = RunResult::path(&run.session.results_root, strategy.name(), &run.entry.id);

    for (n, step) in chosen.iter().enumerate() {
        let scratch = scratch_root.join(format!("{}-{}", strategy.name(), short(&step.head)));
        std::fs::create_dir_all(&scratch)?;
        let changed = changed_paths(&checkout.dir, &step.parent, &step.head)?;
        let cx = StepContext {
            subject: &run.entry.id,
            worktree: &checkout.dir,
            step,
            changed: &changed,
            bazel: engine.bazel(),
            env: &env,
            scratch: &scratch,
        };
        tracing::info!(
            step = n + 1,
            of = chosen.len(),
            parent = %short(&step.parent),
            head = %short(&step.head),
            changed = changed.len(),
            "preparing"
        );
        checkout.checkout(&step.parent)?;
        let prepared = strategy
            .prepare(&cx)
            .with_context(|| format!("{} prepare at {}", strategy.name(), step.parent))?;
        checkout.checkout(&step.head)?;
        let answer = strategy
            .answer(&cx)
            .with_context(|| format!("{} answer {}..{}", strategy.name(), step.parent, step.head))?;

        let (oracle_size, missed, extra) =
            oracle(run, &engine, &cambi, &checkout, step, &changed, &answer.targets)?;
        let m = StepMeasurement {
            subject: run.entry.id.clone(),
            parent: step.parent.clone(),
            head: step.head.clone(),
            tags: step.tags.clone(),
            prepare_wall_ms: prepared.wall.as_millis() as u64,
            answer_wall_ms: answer.wall.as_millis() as u64,
            answer_size: answer.targets.len(),
            oracle_size,
            missed,
            extra,
            state_bytes: prepared.state_bytes,
            notes: prepared
                .notes
                .into_iter()
                .map(|(k, v)| (format!("prepare.{k}"), v))
                .chain(
                    answer
                        .notes
                        .into_iter()
                        .map(|(k, v)| (format!("answer.{k}"), v)),
                )
                .collect(),
        };
        println!(
            "  {}..{}  prepare {} ms, answer {} ms, {} targets, oracle {}, missed {}, extra {}",
            short(&step.parent),
            short(&step.head),
            m.prepare_wall_ms,
            m.answer_wall_ms,
            m.answer_size,
            m.oracle_size,
            m.missed.len(),
            m.extra
        );
        result.steps.push(m);
        // Written after every step: a killed run leaves what it measured.
        result.write(&path)?;
    }
    result.write(&path)?;
    Ok(Measured { result, path })
}

/// The `rdeps` oracle over one step, scored against `answer`.
///
/// It needs an index at head for the package list that turns a changed path
/// into a source label, so the harness builds one itself rather than reading
/// whatever the strategy happened to leave behind: every strategy is scored
/// against the same oracle, including the ones that build no index at all.
#[allow(clippy::too_many_arguments)]
fn oracle(
    run: &SubjectRun<'_>,
    engine: &cambium_engine::Engine,
    cambi: &crate::tools::cambi::Cambi,
    checkout: &crate::checkout::Checkout,
    step: &crate::walk::Step,
    changed: &[super::ChangedPath],
    answer: &BTreeSet<String>,
) -> Result<(usize, Vec<String>, usize)> {
    let head_index = run.index_path(&step.head);
    if !head_index.exists() {
        cambi
            .index_build(&checkout.dir, &step.head, &head_index, false)
            .with_context(|| format!("the oracle's index at {}", step.head))?;
    }
    let index = IndexFile::open(&head_index)?;
    // The deleted half needs an index at the parent. One is there when some
    // strategy published it; without it the oracle checks the queried half
    // only, which it records.
    let base = engine
        .fetch_baseline(&step.parent)
        .ok()
        .flatten()
        .and_then(|p| IndexFile::open(&p).ok());
    let changed_set: BTreeSet<String> = changed.iter().map(|c| c.path.clone()).collect();
    let dirty: BTreeSet<String> = BTreeSet::new();
    let out = RdepsOracle::new(0, run.session.seed).run(
        engine.bazel(),
        &OracleInputs {
            head: index.view(),
            base: base.as_ref().map(|f| f.view()),
            changed: &changed_set,
            cambium: answer,
            dirty_packages: &dirty,
        },
    )?;
    // `score` over the oracle's own set, so `missed` and `extra` are two
    // halves of one comparison rather than two different ones.
    let (missed, extra) = score(answer, &out.oracle_set);
    Ok((out.oracle_set.len(), missed, extra))
}

/// The strategy a name asks for. A prototype branch adds one arm.
fn open<'a>(
    name: &str,
    cambi: &'a crate::tools::cambi::Cambi,
    results: &std::path::Path,
) -> Result<Box<dyn Strategy + 'a>> {
    match name {
        "cambi" => Ok(Box::new(CambiStrategy {
            cambi,
            index_dir: results.join("skyframe-index"),
        })),
        "skyframe-index" => Ok(Box::new(super::skyindex::SkyIndexStrategy::plain())),
        "skyframe-index-requery" => Ok(Box::new(super::skyindex::SkyIndexStrategy::requery())),
        other => anyhow::bail!(
            "unknown strategy {other:?}; this cambium has {}",
            super::strategies().join(", ")
        ),
    }
}

fn short(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

/// Reads result files and renders the comparison table.
pub fn compare_files(paths: &[PathBuf]) -> Result<String> {
    let mut results = Vec::new();
    for p in paths {
        results.push(RunResult::read(p)?);
    }
    results.sort_by(|a, b| (&a.strategy, &a.subject).cmp(&(&b.strategy, &b.subject)));
    Ok(super::compare(&results))
}
