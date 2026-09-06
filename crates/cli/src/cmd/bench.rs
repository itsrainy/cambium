//! `cambi bench` and `cambi synth` (spec 13).

use std::path::PathBuf;

use anyhow::Result;
use cambium_bench::runner::{self, BazelE2eOptions, RunOptions, SweepOptions};
use clap::Subcommand;

use crate::cmd::Context;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
pub enum Args {
    /// Run stage benchmarks against synthetic graphs.
    Run {
        /// Graph presets: tiny, small, medium, large, xl, xxl, plus shape
        /// suffixes (-hubby, -deep, -flat, -wide-packages).
        #[arg(long, value_delimiter = ',', default_values_t = [String::from("small"), String::from("medium")])]
        preset: Vec<String>,
        /// Stages to run; defaults to every in-memory stage.
        #[arg(long, value_delimiter = ',')]
        stage: Vec<String>,
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Where to write the JSON results. Defaults to bench-results/<ts>.json.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also write a markdown summary table here.
        #[arg(long)]
        markdown: Option<PathBuf>,
        /// Print the results instead of writing a file.
        #[arg(long)]
        stdout: bool,
    },
    /// Vary one parameter and report how a stage scales.
    Sweep {
        #[arg(long)]
        stage: String,
        #[arg(long, default_value = "targets")]
        param: String,
        #[arg(long, value_delimiter = ',')]
        values: Vec<String>,
        #[arg(long, default_value = "medium")]
        base_preset: String,
        #[arg(long, default_value_t = 3)]
        runs: usize,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write an SVG of ms against the parameter, on log-log axes.
        #[arg(long)]
        plot: Option<PathBuf>,
    },
    /// Compare two result files and fail on regressions.
    Compare {
        #[arg(long)]
        baseline: PathBuf,
        #[arg(long)]
        current: PathBuf,
        /// Percentage a stage may slow down before it counts as a regression.
        #[arg(long, default_value_t = 15.0)]
        threshold: f64,
    },
    /// Compare against bazel-diff on the same synthetic change (spec 7).
    ///
    /// Bazel is replaced by a shim that replays the graph dump cambium's
    /// generator already produces, so the measured time is each tool's own
    /// work rather than a Bazel server's.
    Legacy {
        #[arg(long, default_value = "small")]
        preset: String,
        /// Change preset applied to reach the head revision.
        #[arg(long, default_value = "typical-pr")]
        changes: String,
        /// bazel-diff binary; defaults to $CAMBIUM_BAZEL_DIFF, then the path
        /// tools/bazel-diff/fetch.sh writes.
        #[arg(long = "bazel-diff")]
        bazel_diff: Option<PathBuf>,
        #[arg(long, default_value_t = 3)]
        runs: usize,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// Keep the emitted workspaces here instead of a temporary directory.
        #[arg(long)]
        workdir: Option<PathBuf>,
        #[arg(long, default_value = "text")]
        output: OutputFormat,
    },
    /// End-to-end timing against a real Bazel, on an emitted workspace.
    Bazel {
        /// Repository to run in (typically one from `cambi synth emit-bazel`).
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        base: String,
        #[arg(long, default_value = "HEAD")]
        head: String,
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// Also time the "dump the whole graph at both commits" approach.
        #[arg(long)]
        legacy: bool,
    },
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    match args {
        Args::Run {
            preset,
            stage,
            runs,
            out,
            markdown,
            stdout,
        } => {
            let opts = RunOptions {
                presets: preset,
                stages: stage,
                runs,
                out: if stdout {
                    None
                } else {
                    Some(out.unwrap_or_else(runner::default_results_path))
                },
                markdown,
            };
            runner::run(&opts)?;
            Ok(crate::EXIT_OK)
        }
        Args::Sweep {
            stage,
            param,
            values,
            base_preset,
            runs,
            out,
            plot,
        } => {
            let values = values
                .iter()
                .map(|v| parse_size(v))
                .collect::<Result<Vec<u64>>>()?;
            anyhow::ensure!(!values.is_empty(), "--values is required");
            let plot = plot.or_else(|| {
                (stage == cambium_bench::legacy::STAGE).then(|| PathBuf::from("bench-legacy.svg"))
            });
            runner::sweep(&SweepOptions {
                stage,
                param,
                values,
                runs,
                out,
                plot,
                base_preset,
            })?;
            Ok(crate::EXIT_OK)
        }
        Args::Compare {
            baseline,
            current,
            threshold,
        } => {
            let regressions = runner::compare_files(&baseline, &current, threshold)?;
            Ok(if regressions == 0 {
                crate::EXIT_OK
            } else {
                eprintln!(
                    "cambi bench: {regressions} stage(s) regressed by more than {threshold}%"
                );
                crate::EXIT_ERROR
            })
        }
        Args::Legacy {
            preset,
            changes,
            bazel_diff,
            runs,
            seed,
            workdir,
            output,
        } => {
            let ctx = cambium_bench::StageContext::new(&preset, runs)?;
            let run = cambium_bench::legacy::run(
                &ctx,
                &cambium_bench::legacy::LegacyOptions {
                    change_preset: changes,
                    seed,
                    binary: bazel_diff,
                    workdir,
                },
            )?;
            match output {
                OutputFormat::Json => {
                    let report = cambium_bench::BenchReport::new(vec![run.result]);
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
                OutputFormat::Text => print_legacy(&run),
            }
            Ok(crate::EXIT_OK)
        }
        Args::Bazel {
            repo,
            base,
            head,
            runs,
            legacy,
        } => {
            let results = runner::e2e_bazel(&BazelE2eOptions {
                repo,
                base,
                head,
                runs,
                legacy,
                bazel_binary: ctx.bazel_binary.clone(),
            })?;
            let report = cambium_bench::BenchReport::new(results);
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(crate::EXIT_OK)
        }
    }
}

fn print_legacy(run: &cambium_bench::legacy::LegacyRun) {
    let e = &run.result.extra;
    let get = |k: &str| e.get(k).copied().unwrap_or(0.0);
    println!(
        "preset:          {} ({} targets)",
        run.result.preset,
        get("targets") as u64
    );
    println!("bazel-diff:      {}", run.bazel_diff_version);
    println!("changed files:   {}", get("changed_files") as u64);
    println!();
    println!("bazel-diff cold: {:>10.1} ms", get("bazel_diff_cold_ms"));
    println!("bazel-diff warm: {:>10.1} ms", get("bazel_diff_warm_ms"));
    println!("cambium:         {:>10.1} ms", get("cambium_ms"));
    println!(
        "speedup:         {:>10.1}x cold, {:.1}x warm",
        get("speedup_vs_cold"),
        get("speedup_vs_warm")
    );
    println!();
    println!("agreed:          {}", run.agreed.len());
    println!(
        "cambium only:    {} (select-reachable over-approximation)",
        run.cambium_only.len()
    );
    println!("bazel-diff only: {}", run.bazel_diff_only.len());
}

/// Accepts `1000`, `1e6`, `2M`, `500k`.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1_000.0),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1_000_000.0),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1_000_000_000.0),
        _ => (s, 1.0),
    };
    let v: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("cannot parse size {s:?}"))?;
    Ok((v * mult) as u64)
}

// ------------------------------------------------------------------- synth

#[derive(Subcommand)]
pub enum SynthArgs {
    /// Write a real Bazel workspace with git history from a synthetic graph.
    EmitBazel {
        #[arg(long, default_value = "small")]
        spec: String,
        #[arg(long)]
        out: PathBuf,
        /// Change preset driving the generated commits.
        #[arg(long, default_value = "typical-pr")]
        changes: String,
        /// How many commits to generate after the base commit.
        #[arg(long, default_value_t = 5)]
        revisions: u64,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
    /// Report the shape of a generated graph without writing anything.
    Describe {
        #[arg(long, default_value = "small")]
        spec: String,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
    /// Build an index from a generated graph and write it to a file.
    ///
    /// The graph never goes near Bazel, so this is the cheap way to get an
    /// index of a given size to point `index stats` at.
    BuildIndex {
        #[arg(long, default_value = "small")]
        spec: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
    /// List the available graph and change presets.
    Presets,
}

pub fn run_synth(_ctx: &Context, args: SynthArgs) -> Result<u8> {
    match args {
        SynthArgs::EmitBazel {
            spec,
            out,
            changes,
            revisions,
            seed,
        } => {
            let repo = runner::emit_bazel(&spec, &changes, revisions, seed, &out)?;
            println!("workspace: {}", repo.dir.display());
            println!("targets:   {}", repo.graph.num_alive_targets());
            println!("packages:  {}", repo.graph.packages.len());
            println!("commits:");
            for (i, c) in repo.commits.iter().enumerate() {
                println!("  {i:>3} {c}");
            }
            println!(
                "\ntry: cambium -C {} affected --base {} --head {}",
                repo.dir.display(),
                repo.commits.first().cloned().unwrap_or_default(),
                repo.commits.last().cloned().unwrap_or_default()
            );
            Ok(crate::EXIT_OK)
        }
        SynthArgs::Describe { spec, seed } => {
            let s = cambium_synth::GraphSpec::preset(&spec)
                .ok_or_else(|| anyhow::anyhow!("unknown preset {spec}"))?
                .with_seed(seed);
            let g = cambium_synth::generate(&s);
            let index = g.build_index("describe");
            let c = &index.header.counts;
            println!("preset:     {spec} (spec hash {})", s.hash());
            println!("targets:    {}", g.num_alive_targets());
            println!("packages:   {}", g.packages.len());
            println!("bzl files:  {}", g.bzl.len());
            println!("graph edges:{}", g.num_edges());
            println!("index nodes:{}", c.nodes);
            println!("index edges:{}", c.edges);
            println!("rules:      {}", c.rules);
            println!("tests:      {}", c.tests);
            Ok(crate::EXIT_OK)
        }
        SynthArgs::BuildIndex { spec, out, seed } => {
            let s = cambium_synth::GraphSpec::preset(&spec)
                .ok_or_else(|| anyhow::anyhow!("unknown preset {spec}"))?
                .with_seed(seed);
            let g = cambium_synth::generate(&s);
            let index = g.build_index("synthetic");
            cambium_index::format::write_index(&out, &index)?;
            let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("wrote {} ({} bytes)", out.display(), bytes);
            println!(
                "nodes: {} edges: {}",
                index.header.counts.nodes, index.header.counts.edges
            );
            Ok(crate::EXIT_OK)
        }
        SynthArgs::Presets => {
            println!("graph presets:");
            for p in cambium_synth::GraphSpec::PRESETS {
                println!("  {p}");
            }
            println!("change presets:");
            for p in cambium_synth::ChangeSpec::PRESETS {
                println!("  {p}");
            }
            Ok(crate::EXIT_OK)
        }
    }
}
