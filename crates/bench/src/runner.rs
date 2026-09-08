//! Driving the stages: single runs, scaling sweeps, and regression gating.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cambium_synth::{ChangeSpec, GraphSpec};

use crate::harness::{compare, BenchReport};
use crate::stages::{run_stage, StageContext, IN_MEMORY_STAGES};

#[derive(Clone, Debug)]
pub struct RunOptions {
    /// The synthetic preset the stages run over. One, always: `bench stages`
    /// takes one subject, and a run over two presets is two runs.
    pub preset: String,
    /// Empty means every in-memory stage.
    pub stages: Vec<String>,
    pub runs: usize,
    pub out: Option<PathBuf>,
    pub markdown: Option<PathBuf>,
}

/// Runs the requested stages over one generated graph.
pub fn run(opts: &RunOptions) -> Result<BenchReport> {
    let stages: Vec<String> = if opts.stages.is_empty() {
        IN_MEMORY_STAGES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.stages.clone()
    };
    let ctx = StageContext::new(&opts.preset, opts.runs)?;
    let mut results = Vec::new();
    for stage in &stages {
        eprintln!("cambi bench: {stage} on {}", opts.preset);
        results.push(run_stage(&ctx, stage)?);
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
    let mut rival_cold: Vec<(f64, f64)> = Vec::new();
    let mut rival_warm: Vec<(f64, f64)> = Vec::new();
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
        eprintln!("cambi bench: {} at {}={}", opts.stage, opts.param, v);
        let mut r = run_stage(&ctx, &opts.stage)?;
        r.preset = opts.base_preset.clone();
        r.param = Some(v);
        points.push((v as f64, r.median_ms));
        // Comparison stages carry a second curve: the tool being compared
        // against, measured on the very same graph.
        for (key, series) in [
            ("bazel_diff_cold_ms", &mut rival_cold),
            ("bazel_diff_warm_ms", &mut rival_warm),
        ] {
            if let Some(ms) = r.extra.get(key) {
                series.push((v as f64, *ms));
            }
        }
        results.push(r);
    }
    let report = BenchReport::new(results);
    emit(&report, opts.out.as_deref(), None)?;
    if let Some(p) = &opts.plot {
        let title = format!("{} vs {}", opts.stage, opts.param);
        let svg = if rival_cold.is_empty() {
            crate::harness::plot_svg(&title, &points)
        } else {
            crate::harness::plot_svg_series(
                &title,
                &[
                    ("bazel-diff (cold)", &rival_cold),
                    ("bazel-diff (warm)", &rival_warm),
                    ("cambium", &points),
                ],
            )
        };
        std::fs::write(p, svg).with_context(|| format!("writing {}", p.display()))?;
        eprintln!("cambi bench: wrote {}", p.display());
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
            eprintln!("cambi bench: wrote {}", p.display());
        }
        None => println!("{json}"),
    }
    if let Some(p) = markdown {
        std::fs::write(p, report.to_markdown())?;
        eprintln!("cambi bench: wrote {}", p.display());
    }
    Ok(())
}
