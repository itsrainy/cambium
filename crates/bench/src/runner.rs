//! Driving the stages: single runs, scaling sweeps, regression gating, and the
//! Bazel-backed end-to-end comparison.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cambium_synth::{ChangeSpec, GraphSpec};

use crate::harness::{compare, peak_rss_mb, time, BenchReport, StageResult};
use crate::stages::{run_stage, StageContext, IN_MEMORY_STAGES};

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub presets: Vec<String>,
    /// Empty means every in-memory stage.
    pub stages: Vec<String>,
    pub runs: usize,
    pub out: Option<PathBuf>,
    pub markdown: Option<PathBuf>,
}

/// Runs the requested stages against the requested presets.
pub fn run(opts: &RunOptions) -> Result<BenchReport> {
    let stages: Vec<String> = if opts.stages.is_empty() {
        IN_MEMORY_STAGES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.stages.clone()
    };
    let mut results = Vec::new();
    for preset in &opts.presets {
        let ctx = StageContext::new(preset, opts.runs)?;
        for stage in &stages {
            eprintln!("cambium bench: {stage} on {preset}");
            results.push(run_stage(&ctx, stage)?);
        }
    }
    let report = BenchReport::new(results);
    emit(&report, opts.out.as_deref(), opts.markdown.as_deref())?;
    Ok(report)
}

#[derive(Clone, Debug)]
pub struct SweepOptions {
    pub stage: String,
    pub param: String,
    pub values: Vec<u64>,
    pub runs: usize,
    pub out: Option<PathBuf>,
    pub plot: Option<PathBuf>,
    /// Preset whose shape is held constant while `param` varies.
    pub base_preset: String,
}

/// Varies one parameter and reports the stage's cost at each size, so linearity
/// is visible at a glance.
pub fn sweep(opts: &SweepOptions) -> Result<BenchReport> {
    let base = GraphSpec::preset(&opts.base_preset)
        .with_context(|| format!("unknown preset {}", opts.base_preset))?;
    let mut results = Vec::new();
    let mut points = Vec::new();
    for &v in &opts.values {
        let mut spec = base.clone();
        match opts.param.as_str() {
            "targets" => spec.targets = v,
            "layers" => spec.layers = v as u32,
            "deps_per_target" => {
                spec.deps_per_target = cambium_synth::Dist::Const(v);
            }
            "packages" => spec.packages = v,
            other => anyhow::bail!(
                "unknown sweep parameter {other}; try targets, packages, layers, deps_per_target"
            ),
        }
        let ctx = StageContext {
            preset: format!("{}[{}={}]", opts.base_preset, opts.param, v),
            spec,
            runs: opts.runs,
        };
        eprintln!("cambium bench: {} at {}={}", opts.stage, opts.param, v);
        let mut r = run_stage(&ctx, &opts.stage)?;
        r.preset = opts.base_preset.clone();
        r.param = Some(v);
        points.push((v as f64, r.median_ms));
        results.push(r);
    }
    let report = BenchReport::new(results);
    emit(&report, opts.out.as_deref(), None)?;
    if let Some(p) = &opts.plot {
        let svg = crate::harness::plot_svg(&format!("{} vs {}", opts.stage, opts.param), &points);
        std::fs::write(p, svg).with_context(|| format!("writing {}", p.display()))?;
        eprintln!("cambium bench: wrote {}", p.display());
    }
    Ok(report)
}

/// Compares two saved reports; returns the number of regressions found.
pub fn compare_files(baseline: &Path, current: &Path, threshold_pct: f64) -> Result<usize> {
    let b: BenchReport = serde_json::from_str(&std::fs::read_to_string(baseline)?)
        .with_context(|| format!("parsing {}", baseline.display()))?;
    let c: BenchReport = serde_json::from_str(&std::fs::read_to_string(current)?)
        .with_context(|| format!("parsing {}", current.display()))?;
    let diffs = compare(&b, &c, threshold_pct)?;
    let mut regressions = 0;
    for d in &diffs {
        let arrow = if d.change_pct > 0.0 {
            "SLOWER"
        } else {
            "faster"
        };
        println!(
            "{arrow:>7} {:+6.1}%  {}  {:.2} ms -> {:.2} ms",
            d.change_pct, d.key, d.baseline_ms, d.current_ms
        );
        if d.change_pct > 0.0 {
            regressions += 1;
        }
    }
    if diffs.is_empty() {
        println!("no stage moved by more than {threshold_pct}%");
    }
    Ok(regressions)
}

/// Emits an entire synthetic repository with git history, ready for
/// `bazel build //...` and for end-to-end runs.
pub fn emit_bazel(
    preset: &str,
    change_preset: &str,
    revisions: u64,
    seed: u64,
    out: &Path,
) -> Result<cambium_synth::EmittedRepo> {
    let spec = GraphSpec::preset(preset)
        .with_context(|| format!("unknown preset {preset}"))?
        .with_seed(seed);
    let g = cambium_synth::generate(&spec);
    let change = ChangeSpec {
        seed,
        ..ChangeSpec::preset(change_preset)
            .with_context(|| format!("unknown change preset {change_preset}"))?
    };
    let repo = cambium_synth::emit_git_history(g, &change, revisions, out, false)?;
    Ok(repo)
}

/// End-to-end against a real Bazel, including the legacy "dump the whole graph
/// at both commits and diff" baseline this design replaces.
pub struct BazelE2eOptions {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    pub runs: usize,
    pub legacy: bool,
    pub bazel_binary: Option<String>,
}

pub fn e2e_bazel(opts: &BazelE2eOptions) -> Result<Vec<StageResult>> {
    use cambium_engine::{AffectedRequest, Engine};

    let engine = Engine::open(&opts.repo, None, opts.bazel_binary.as_deref())?;
    let mut results = Vec::new();

    // The cold build is also the baseline the incremental run splices onto.
    let base_sha = engine.repo.rev_parse(&opts.base)?;
    let (build_ms, build_p90) = time(1, || engine.full_build(&base_sha));
    let index = engine.full_build(&base_sha)?;
    results.push(StageResult {
        stage: "e2e_bazel_full_build".into(),
        preset: "emitted".into(),
        spec_hash: String::new(),
        param: None,
        runs: 1,
        median_ms: build_ms,
        p90_ms: build_p90,
        peak_rss_mb: peak_rss_mb(),
        extra: BTreeMap::from([
            ("nodes".to_string(), index.header.counts.nodes as f64),
            ("edges".to_string(), index.header.counts.edges as f64),
        ]),
    });

    let req = AffectedRequest::new(&base_sha, &opts.head);
    let (median, p90) = time(opts.runs, || {
        engine.run_incremental(index.view(), &req).expect("run")
    });
    let out = engine.run_incremental(index.view(), &req)?;
    let mut extra = BTreeMap::from([
        (
            "dirty_packages".to_string(),
            out.report.stats.dirty_packages as f64,
        ),
        (
            "affected".to_string(),
            out.report.stats.affected_targets as f64,
        ),
        ("query_ms".to_string(), out.report.timings_ms.query as f64),
        ("splice_ms".to_string(), out.report.timings_ms.splice as f64),
        ("walk_ms".to_string(), out.report.timings_ms.walk as f64),
    ]);

    if opts.legacy {
        // What the previous approach costs: dump the entire graph twice.
        let (legacy_ms, _) = time(1, || legacy_double_dump(&engine, &opts.base, &opts.head));
        extra.insert("legacy_double_dump_ms".to_string(), legacy_ms);
        extra.insert(
            "speedup_vs_legacy".to_string(),
            legacy_ms / median.max(1e-9),
        );
    }

    results.push(StageResult {
        stage: "e2e_bazel".into(),
        preset: "emitted".into(),
        spec_hash: String::new(),
        param: None,
        runs: opts.runs,
        median_ms: median,
        p90_ms: p90,
        peak_rss_mb: peak_rss_mb(),
        extra,
    });
    Ok(results)
}

/// The approach cambium replaces: full graph dump at both commits, then diff.
fn legacy_double_dump(engine: &cambium_engine::Engine, base: &str, head: &str) -> Result<usize> {
    let mut changed = 0;
    let mut hashes: Vec<BTreeMap<String, [u8; 32]>> = Vec::new();
    for rev in [base, head] {
        // Check the revision out is the caller's job; we measure the query and
        // hashing cost, which is what dominates.
        let idx = engine.full_build(rev)?;
        let v = idx.view();
        let mut m = BTreeMap::new();
        for id in 0..v.num_nodes() {
            if v.is_rule(id) {
                m.insert(v.label(id).to_string(), v.hash[id as usize]);
            }
        }
        hashes.push(m);
    }
    for (label, h) in &hashes[1] {
        if hashes[0].get(label) != Some(h) {
            changed += 1;
        }
    }
    Ok(changed)
}

fn emit(report: &BenchReport, out: Option<&Path>, markdown: Option<&Path>) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    match out {
        Some(p) => {
            if let Some(dir) = p.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            std::fs::write(p, &json).with_context(|| format!("writing {}", p.display()))?;
            eprintln!("cambium bench: wrote {}", p.display());
        }
        None => println!("{json}"),
    }
    if let Some(p) = markdown {
        std::fs::write(p, report.to_markdown())?;
        eprintln!("cambium bench: wrote {}", p.display());
    }
    Ok(())
}

/// Default results path: `bench-results/<unix seconds>.json`.
pub fn default_results_path() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from("bench-results").join(format!("{ts}.json"))
}
