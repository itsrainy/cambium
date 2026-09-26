//! Running one strategy over one subject: the steps, the two timed calls, the
//! oracle, the result file.
//!
//! Everything a strategy sees is decided here, so two strategies on a subject
//! see the same steps in the same order under the same Bazel. The step choice
//! is the walk's own ([`crate::walk::steps()`] tagged, [`crate::walk::sample`]
//! drawn at a fixed seed with at least one step per tag), which also means a
//! step measured here is a step the walk measures, and the two numbers are
//! about the same commit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use cambium_index::IndexFile;

use crate::oracles::rdeps::{OracleInputs, RdepsOracle};
use crate::session::SubjectRun;
use crate::util::short;

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

    let (_, all) = crate::walk::steps(
        &checkout.repo,
        &run.entry.pin,
        run.entry.history_window,
        &run.probes(),
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
        println!(
            "  {}..{}  [{}]",
            short(&s.parent, 12),
            short(&s.head, 12),
            tags.join(",")
        );
    }

    let mut strategy = open(&req.strategy, &cambi, &run.results)?;
    let mut result = RunResult {
        strategy: strategy.name().to_string(),
        subject: run.entry.id.clone(),
        bazel_version: crate::env::bazel_version_used(engine.bazel()),
        started: crate::util::now_unix(),
        steps: Vec::new(),
    };
    let path = RunResult::path(&run.session.results_root, strategy.name(), &run.entry.id);

    let harness = Harness {
        run,
        engine: &engine,
        cambi: &cambi,
        checkout: &checkout,
        env: &env,
        scratch_root: &scratch_root,
    };
    for (n, step) in chosen.iter().enumerate() {
        let m = harness.measure(strategy.as_mut(), step, (n + 1, chosen.len()))?;
        println!(
            "  {}..{}  prepare {} ms, answer {} ms, {} targets, oracle {}, missed {}, extra {}",
            short(&step.parent, 12),
            short(&step.head, 12),
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

/// What every step of one run shares: the subject, its checkout (moved to
/// whichever revision a call needs), its Bazel, and the `cambi` that builds
/// the oracle's index.
struct Harness<'a> {
    run: &'a SubjectRun<'a>,
    engine: &'a cambium_engine::Engine,
    cambi: &'a crate::tools::cambi::Cambi,
    checkout: &'a crate::checkout::Checkout,
    env: &'a [(String, String)],
    scratch_root: &'a Path,
}

impl Harness<'_> {
    /// One step: `prepare` at the parent, `answer` at the head, both timed,
    /// and the answer scored against the oracle. `at` is the step's place in
    /// the run, for the log.
    fn measure(
        &self,
        strategy: &mut dyn Strategy,
        step: &crate::walk::Step,
        at: (usize, usize),
    ) -> Result<StepMeasurement> {
        let scratch =
            self.scratch_root
                .join(format!("{}-{}", strategy.name(), short(&step.head, 12)));
        std::fs::create_dir_all(&scratch)?;
        let changed = changed_paths(&self.checkout.dir, &step.parent, &step.head)?;
        let cx = StepContext {
            subject: &self.run.entry.id,
            worktree: &self.checkout.dir,
            step,
            changed: &changed,
            bazel: self.engine.bazel(),
            env: self.env,
            scratch: &scratch,
        };
        tracing::info!(
            step = at.0,
            of = at.1,
            parent = %short(&step.parent, 12),
            head = %short(&step.head, 12),
            changed = changed.len(),
            "preparing"
        );
        self.checkout.checkout(&step.parent)?;
        let prepared = strategy
            .prepare(&cx)
            .with_context(|| format!("{} prepare at {}", strategy.name(), step.parent))?;
        self.checkout.checkout(&step.head)?;
        let answer = strategy.answer(&cx).with_context(|| {
            format!("{} answer {}..{}", strategy.name(), step.parent, step.head)
        })?;

        let (oracle_size, missed, extra) =
            self.oracle(&cx, &answer.targets, &answer.dirty_packages)?;
        Ok(StepMeasurement {
            subject: self.run.entry.id.clone(),
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
        })
    }

    /// The `rdeps` oracle over one step, scored against `answer`.
    ///
    /// It needs an index at head for the package list that turns a changed
    /// path into a source label, so the harness builds one itself rather than
    /// reading whatever the strategy happened to leave behind: every strategy
    /// is scored against the same oracle, including the ones that build no
    /// index at all.
    fn oracle(
        &self,
        cx: &StepContext<'_>,
        answer: &BTreeSet<String>,
        dirty: &BTreeSet<String>,
    ) -> Result<(usize, Vec<String>, usize)> {
        let step = cx.step;
        let head_index = self.run.index_path(&step.head);
        if !head_index.exists() {
            self.cambi
                .index_build(&self.checkout.dir, &step.head, &head_index, false)
                .with_context(|| format!("the oracle's index at {}", step.head))?;
        }
        let index = IndexFile::open(&head_index)?;
        // The deleted half needs an index at the parent. One is there when
        // some strategy published it; without it the oracle checks the
        // queried half only, which it records.
        let base = self
            .engine
            .fetch_baseline(&step.parent)
            .ok()
            .flatten()
            .and_then(|p| IndexFile::open(&p).ok());
        let changed_set = cx.changed_set();
        let added_or_deleted: BTreeSet<String> = cx
            .changed
            .iter()
            .filter(|c| c.status.starts_with(['A', 'D', 'R']))
            .map(|c| c.path.clone())
            .collect();
        let output_base = crate::oracles::repo_inputs::output_base(self.engine.bazel());
        let out = RdepsOracle::new(0, self.run.session.seed).run(
            self.engine.bazel(),
            &OracleInputs {
                head: index.view(),
                base: base.as_ref().map(|f| f.view()),
                changed: &changed_set,
                cambium: answer,
                dirty_packages: dirty,
                output_base: output_base.as_deref(),
                added_or_deleted: Some(&added_or_deleted),
            },
        )?;
        // `score` over the oracle's own set, so `missed` and `extra` are two
        // halves of one comparison rather than two different ones.
        let (missed, extra) = score(answer, &out.oracle_set);
        Ok((out.oracle_set.len(), missed, extra))
    }
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
        other => anyhow::bail!(
            "unknown strategy {other:?}; this cambium has {}",
            super::strategies().join(", ")
        ),
    }
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
