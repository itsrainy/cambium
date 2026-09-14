//! `cambium-bench` — the benchmark (docs/METHODOLOGY.md).
//!
//! The command tree is the vocabulary: `subjects` for what a run points at,
//! `walk` for the one workload, `replay` for the offline engine check,
//! `report` to regenerate `docs/RESULTS.md`, and `run` for the whole
//! sequence.
//!
//! Every subcommand takes a `<subject>` — a name from `subject_repos.toml`, or
//! `synth:<preset>` — and every one of them is idempotent and resumable: what
//! finished is recorded in `bench-results/<subject>/state.json`, and rerunning
//! picks up from there rather than redoing an hour of `bazel query`. Progress
//! is logged at `info`, so a long run is followable with `--log info`.
//!
//! This file marshals arguments and prints answers. What a verb *does* is
//! [`cambium_bench::run`], against a [`cambium_bench::session::Session`].

use std::path::PathBuf;

use anyhow::Result;
use cambium_bench::plan::{Plan, Size, StepKind};
use cambium_bench::run as verbs;
use cambium_bench::run::{Labels, LabelsQuery};
use clap::{Args as ClapArgs, Subcommand};

use crate::Context;

/// Flags every `bench` subcommand shares.
#[derive(ClapArgs, Clone, Default)]
pub struct Common {
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Where results go: <results-dir>/<subject>/<workload>/.
    #[arg(long, default_value = "bench-results")]
    pub results_dir: PathBuf,
    /// Mirrors, worktrees, emitted synthetic subjects and caches (default:
    /// ~/.cache/cambium/subjects).
    #[arg(long, env = cambium_bench::checkout::SUBJECTS_DIR_ENV)]
    pub subjects_dir: Option<PathBuf>,
    /// Bazel version for bazelisk (USE_BAZEL_VERSION); also suffixes the
    /// result file names. Default: whatever the subject's .bazelversion says.
    #[arg(long)]
    pub bazel_version: Option<String>,
    /// Seed for the deterministic change-set sampling.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Laptop-sized: 50 commits, 50 change sets, Tango on the first 10.
    ///
    /// Sugar for those three flags and nothing else. It is not a mode: the
    /// results land in the same tree as any other run, and the sizes that
    /// produced them are inside the result file, so a short run and a
    /// published one differ only in size.
    #[arg(long)]
    pub quick: bool,
}

#[derive(Subcommand)]
pub enum Args {
    /// List the subjects, or fetch and cold-build one.
    Subjects {
        #[command(subcommand)]
        what: SubjectArgs,
    },
    /// The walk: one first-parent traversal to the pin, the index carried
    /// step to step, every tool scored against the rdeps oracle for recall
    /// and tightness. Two tiers share it: `correctness` (no peers, the oracle
    /// on a sample) is what every engine change pays for; `full` (the oracle
    /// on every step, the peers on a stratified sample) is the box run.
    Walk {
        /// A name from subject_repos.toml, or synth:<preset>.
        subject: String,
        /// correctness or full.
        #[arg(long, default_value = "full")]
        tier: String,
        /// First-parent steps before the pin. Defaults to the subject's
        /// history window.
        #[arg(long)]
        steps: Option<usize>,
        /// The peers to run alongside cambium: bazel-diff, tango,
        /// target-determinator, or `none`. Comma-separated; default: the
        /// tier's own (peers on `full`, none on `correctness`).
        #[arg(long = "peers", value_delimiter = ',')]
        peers: Vec<String>,
        /// Run the rdeps oracle on this many sampled steps; 0 is every step.
        #[arg(long)]
        oracle_sample: Option<usize>,
        /// Run the peers on this many sampled steps.
        #[arg(long)]
        peer_sample: Option<usize>,
        /// The sampler's minimum per tag.
        #[arg(long)]
        per_tag: Option<usize>,
        /// Cold-build and compare every N steps, on top of the pin; 0 is only
        /// at the pin.
        #[arg(long)]
        rebuild_every: Option<usize>,
        /// Continue the walk already recorded for this pin.
        #[arg(long)]
        resume: bool,
        /// Discard the walk already recorded for this pin and walk again.
        #[arg(long)]
        restart: bool,
        #[command(flatten)]
        common: Common,
    },
    /// The whole run, as `tools/bench/run-published.sh` runs it: per subject
    /// the fetch, the cold build, the walk and the report, then one report
    /// over every subject. Each step is a child `cambium-bench`.
    ///
    /// A failed step never stops the run; a failed fetch or build skips the
    /// rest of that subject. The launcher (no verb, on a terminal) composes
    /// the same plan on a screen.
    Run(RunArgs),
    /// Replay the engine's static stages offline, against a stored index and
    /// a published run's own answers.
    ///
    /// No Bazel, no checkout, milliseconds a change set: the check to run
    /// before the benchmark machine is started. See
    /// `docs/METHODOLOGY.md`, "Validating an engine change before a run".
    Replay(Box<crate::replay::Args>),
    /// Pull a subject's results off the benchmark box for an offline replay:
    /// the index at the pin, the recorded results, the archived base and head
    /// indexes, and the diff of every recorded change set.
    ///
    /// Read-only on the box. A subject already under --cache is pulled again
    /// only when the box's result files differ from the cached ones.
    PullResults(PullArgs),
    /// Print the label sets a change-set comparison stored.
    ///
    /// `changes-<pin>.json` keeps per-class counts and at most a handful of
    /// example labels; the sets themselves are written whole beside it. This
    /// reads them back — offline, no Bazel, no checkout.
    Labels {
        /// A name from subject_repos.toml, or synth:<preset>.
        subject: String,
        /// Which change set, as base..head. Abbreviated revisions are fine.
        #[arg(long = "change-set")]
        change_set: String,
        /// One stored set (a tool's name, `cambium` or `oracle`), or the tool
        /// a disagreement was with when --class or --side narrows one.
        #[arg(long)]
        tool: Option<String>,
        /// Only disagreements filed in this class.
        #[arg(long = "class")]
        class: Option<String>,
        /// Only disagreements on this side: tool or cambium.
        #[arg(long)]
        side: Option<String>,
        /// Print how each tool's set stands against cambium's instead of the
        /// labels: only-tool, only-cambium, both.
        #[arg(long)]
        diff: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Follow a run: one row per subject, polled off bench-results/.
    ///
    /// A pure reader. It holds nothing open on the run, so it works on a run
    /// in progress, on a finished one, and on results copied off the
    /// benchmark box. `--once` prints the same table and exits.
    Watch(crate::watch::Args),
    /// Regenerate docs/RESULTS.md from bench-results/.
    Report {
        /// Rewrite only this subject's section. Every subject that has results
        /// is rendered either way — the top table is built from all of them —
        /// but naming one makes a per-subject nightly job's intent explicit.
        subject: Option<String>,
        /// The document to update.
        #[arg(long)]
        doc: Option<PathBuf>,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Subcommand)]
pub enum SubjectArgs {
    /// Print every subject in the manifest, plus the synthetic presets.
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Mirror the subject and check out its pin.
    Fetch {
        /// A name from subject_repos.toml, or synth:<preset>. Omit for every
        /// subject the manifest does not skip.
        subject: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Cold-build an index at the pin and publish it to the subject's store.
    Build {
        /// A name from subject_repos.toml, or synth:<preset>. Omit for every
        /// subject the manifest does not skip.
        subject: Option<String>,
        /// Revision to build. Defaults to the manifest pin.
        #[arg(long)]
        at: Option<String>,
        /// Rebuild even if this revision was already built.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// `cambium-bench run`: the published sequence, from flags.
#[derive(ClapArgs, Clone, Debug)]
pub struct RunArgs {
    /// Subjects to run, cheapest first. Default: every subject the manifest
    /// does not skip.
    #[arg(long = "subject", value_name = "S", value_delimiter = ',')]
    pub subjects: Vec<String>,
    /// Which steps to take: fetch, build, history, changes, report.
    /// Comma-separated; default: all of them.
    #[arg(long = "steps", value_delimiter = ',')]
    pub steps: Vec<String>,
    /// How big: small, medium or large (the published run). The old names,
    /// smoke, laptop and published, are still accepted.
    #[arg(long, default_value = "large")]
    pub size: String,
    /// The fifteen-minute pass before the eight-hour one: `--size small`.
    #[arg(long, conflicts_with = "size")]
    pub smoke: bool,
    /// The foils `changes` compares against: bazel-diff, tango,
    /// target-determinator. Default: bazel-diff and tango.
    #[arg(long = "tools", value_delimiter = ',')]
    pub tools: Vec<String>,
    /// Continue the results already in the results directory. Without this
    /// or --fresh, a results directory that holds results is refused.
    #[arg(long, conflicts_with = "fresh")]
    pub resume: bool,
    /// Move the results directory aside to `<dir>-<UTC timestamp>` and start
    /// with nothing in it.
    #[arg(long)]
    pub fresh: bool,
    /// Where results go. Default: bench-results/, or bench-results-smoke/
    /// for a smoke run.
    #[arg(long)]
    pub results_dir: Option<PathBuf>,
    /// Refuse to start unless both `cambi` and this binary were built from
    /// this git sha: a run on a stale binary is hours lost.
    #[arg(long, value_name = "SHA")]
    pub require_sha: Option<String>,
    /// Print the steps and run nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

/// `cambium-bench pull-results`.
#[derive(ClapArgs, Clone, Debug)]
pub struct PullArgs {
    /// Subjects to pull. Default: every subject the manifest does not skip.
    pub subjects: Vec<String>,
    /// Where the pulled trees go: <cache>/<subject>/.
    #[arg(long, default_value = "bench-precheck")]
    pub cache: PathBuf,
    /// The box's address, instead of asking EC2 for the running instance.
    #[arg(long)]
    pub host: Option<String>,
    /// The ssh user on the box.
    #[arg(long, default_value = "ubuntu")]
    pub user: String,
    /// The ssh key.
    #[arg(long, value_name = "PEM")]
    pub key: Option<PathBuf>,
    /// The EC2 region the box is in.
    #[arg(long, default_value = "us-east-2", env = "AWS_REGION")]
    pub region: String,
    /// The cambium checkout on the box, relative to the user's home.
    #[arg(long, default_value = "cambium")]
    pub box_repo: String,
    /// The mirrors directory on the box, relative to the user's home.
    #[arg(long, default_value = ".cache/cambium/corpus")]
    pub box_subjects: String,
    /// Do not derive the change-set diffs on the box.
    #[arg(long)]
    pub no_paths: bool,
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    match args {
        Args::Subjects { what } => do_subjects(ctx, what),
        Args::Replay(args) => crate::replay::run(ctx, *args),
        Args::Watch(args) => crate::watch::run(ctx, args),
        Args::Run(args) => do_run(ctx, &args),
        Args::PullResults(args) => do_pull(ctx, &args),
        Args::Walk {
            subject,
            tier,
            steps,
            peers,
            oracle_sample,
            peer_sample,
            per_tag,
            rebuild_every,
            resume,
            restart,
            common,
        } => {
            let session = ctx.session(&common)?;
            let run = session.run_subject(&subject)?;
            let walked = verbs::run_walk(
                &run,
                &verbs::WalkRequest {
                    tier,
                    steps,
                    peers,
                    overrides: cambium_bench::walk::Overrides {
                        oracle_sample,
                        peer_sample,
                        per_tag,
                        rebuild_every,
                        peers: None,
                    },
                    on_existing: verbs::OnExisting::from_flags(resume, restart),
                },
                &ctx.tools,
            )?;
            println!("{}", verbs::walk_summary(&walked.result));
            println!("{}", walked.path.display());
            Ok(if walked.result.summary.pass {
                crate::EXIT_OK
            } else {
                crate::EXIT_ERROR
            })
        }
        Args::Labels {
            subject,
            change_set,
            tool,
            class,
            side,
            diff,
            common,
        } => {
            let session = ctx.session(&common)?;
            let run = session.subject(&subject)?;
            let answer = verbs::read_labels(
                &run,
                &LabelsQuery {
                    change_set,
                    tool,
                    class,
                    side,
                    diff,
                },
            )?;
            print_labels(&answer);
            Ok(crate::EXIT_OK)
        }
        Args::Report {
            subject,
            doc,
            common,
        } => {
            let session = ctx.session(&common)?;
            let (doc, _) = verbs::regenerate_report(&session, subject.as_deref(), doc.as_deref())?;
            println!("{}", doc.display());
            Ok(crate::EXIT_OK)
        }
    }
}

fn do_subjects(ctx: &Context, what: SubjectArgs) -> Result<u8> {
    match what {
        SubjectArgs::List { common: c } => {
            let session = ctx.session(&c)?;
            println!("real subjects ({}):", session.manifest_path.display());
            for e in &session.manifest.repos {
                println!(
                    "  {:<14} {:<12} window {:<4}{}  {}",
                    e.id,
                    &e.pin[..12],
                    e.history_window,
                    if e.skip { "  (skip)" } else { "" },
                    e.notes
                );
            }
            println!("\nsynthetic subjects (synth:<preset>):");
            for p in cambium_synth::GraphSpec::PRESETS {
                println!("  synth:{p}");
            }
            Ok(crate::EXIT_OK)
        }
        SubjectArgs::Fetch { subject, common: c } => {
            let session = ctx.session(&c)?;
            for name in session.subject_names(subject) {
                let run = session.run_subject(&name)?;
                println!("{}", verbs::fetch(&run)?.display());
            }
            Ok(crate::EXIT_OK)
        }
        SubjectArgs::Build {
            subject,
            at,
            force,
            common: c,
        } => {
            let session = ctx.session(&c)?;
            for name in session.subject_names(subject) {
                let run = session.run_subject(&name)?;
                let built = verbs::build(&run, at.as_deref(), force)?;
                if let Some(w) = &built.warning {
                    eprintln!("cambium-bench: WARNING: {w}");
                }
                println!("{}", built.index.display());
            }
            Ok(crate::EXIT_OK)
        }
    }
}

fn print_labels(answer: &Labels) {
    match answer {
        Labels::Diff(diffs) => {
            for d in diffs {
                println!(
                    "{}\tonly-tool {}\tonly-cambium {}\tboth {}",
                    d.tool, d.only_tool, d.only_cambium, d.both
                );
            }
        }
        Labels::Classes(rows) => {
            for row in rows {
                println!(
                    "{}\t{}\t{}\t{}",
                    row.tool,
                    cambium_bench::labels::side_name(row.side),
                    row.class.name(),
                    row.label
                );
            }
        }
        Labels::Sets(sets) => {
            // Named only when there is more than one of them: a `--tool`
            // answer is meant to be pipeable into `comm` or `grep -f`.
            let header = sets.len() > 1;
            for (name, labels) in sets {
                if header {
                    println!("# {name}");
                }
                for label in labels {
                    println!("{label}");
                }
            }
        }
    }
}

/// The plan `run` was asked for.
pub fn plan_of(ctx: &Context, args: &RunArgs) -> Result<Plan> {
    let session = ctx.session(&Common {
        manifest: args.manifest.clone(),
        ..Default::default()
    })?;
    let subjects = match args.subjects.is_empty() {
        true => session.subject_names(None),
        false => args.subjects.clone(),
    };
    let size = if args.smoke {
        Size::Small
    } else {
        Size::parse(&args.size)
            .ok_or_else(|| anyhow::anyhow!("--size is small, medium or large"))?
    };
    let steps = match args.steps.is_empty() {
        true => StepKind::ALL.to_vec(),
        false => args
            .steps
            .iter()
            .map(|s| {
                StepKind::parse(s.trim()).ok_or_else(|| {
                    anyhow::anyhow!("unknown step {s:?}: fetch, build, history, changes, report")
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let mut plan = Plan::published(subjects);
    plan.size = size;
    plan.steps = steps;
    plan.resume = args.resume;
    plan.results_dir = args.results_dir.clone();
    if !args.tools.is_empty() {
        plan.tools = args.tools.clone();
    }
    Ok(plan)
}

fn do_run(ctx: &Context, args: &RunArgs) -> Result<u8> {
    let plan = plan_of(ctx, args)?;
    if args.dry_run {
        for step in plan.steps() {
            println!("{}", step.command_line());
        }
        return Ok(crate::EXIT_OK);
    }
    // The preflight the driver used to do by hand: a run on a stale binary
    // is hours lost, so both binaries have to be built from the tree.
    if let Some(sha) = &args.require_sha {
        let session = ctx.session(&Common::default())?;
        cambium_bench::plan::require_sha(session.cambi()?.version(), sha)?;
        println!(
            "=== {} tree {sha} ===\n=== harness {} ===",
            session.cambi()?.version(),
            crate::VERSION
        );
    }
    if args.fresh {
        if let Some(to) = plan.move_results_aside()? {
            println!("=== moved the previous results to {} ===", to.display());
        }
    } else {
        plan.refuse_existing()?;
    }
    if plan.size == Size::Small {
        println!(
            "=== SMOKE: results in {}/, 3 history commits, 1 change set per subject ===",
            plan.results().display()
        );
    }
    let failures = cambium_bench::plan::run_plan(&plan, &std::env::current_exe()?)?;
    println!(
        "{}",
        match (plan.size, failures) {
            (Size::Small, 0) => "SMOKE RUN DONE".to_string(),
            (_, 0) => "PUBLISHED RUN DONE".to_string(),
            (_, n) => format!("RUN DONE WITH {n} FAILED STEP(S)"),
        }
    );
    Ok(if failures == 0 {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

fn do_pull(ctx: &Context, args: &PullArgs) -> Result<u8> {
    use cambium_bench::remote::BenchBox;
    let session = ctx.session(&Common {
        manifest: args.manifest.clone(),
        ..Default::default()
    })?;
    let subjects = match args.subjects.is_empty() {
        true => session.subject_names(None),
        false => args.subjects.clone(),
    };
    let bench_box = BenchBox {
        host: args.host.clone(),
        user: args.user.clone(),
        key: args.key.clone().unwrap_or_else(|| BenchBox::default().key),
        region: args.region.clone(),
        repo: args.box_repo.clone(),
        subjects: args.box_subjects.clone(),
        ..BenchBox::default()
    };
    let mut failed = 0u8;
    for s in &subjects {
        match bench_box.pull(s, &args.cache, !args.no_paths) {
            Ok(p) if p.cached => println!("{s}: cached: {}", args.cache.join(s).display()),
            Ok(p) => {
                println!("{s}: pulled into {}", args.cache.join(s).display());
                if p.refreshed {
                    println!("  the box's results changed since the cached pull; pulled again");
                }
                if !p.bases {
                    println!("  no archived base indexes for {s}; replaying against the pin");
                }
                if !p.heads {
                    println!("  no archived head index for {s}; the head check will be skipped");
                }
                if !args.no_paths && !p.paths {
                    println!("  no paths cache for {s}; pass --repo to replay");
                }
            }
            Err(e) => {
                eprintln!("cambium-bench: pull-results: {s}: {e:#}");
                failed = crate::EXIT_ERROR;
            }
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &std::path::Path) -> Context {
        std::fs::write(
            dir.join("subject_repos.toml"),
            format!(
                "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{0}\"\n\
                 [[repo]]\nid = \"envoy\"\nurl = \"u\"\npin = \"{0}\"\n\
                 [[repo]]\nid = \"gone\"\nurl = \"u\"\npin = \"{0}\"\nskip = true\n",
                "a".repeat(40)
            ),
        )
        .unwrap();
        Context {
            root: dir.to_path_buf(),
            bazel_binary: None,
            format: crate::Format::Human,
            quiet: false,
            cambi: None,
            tools: Default::default(),
        }
    }

    fn args() -> RunArgs {
        RunArgs {
            subjects: vec![],
            steps: vec![],
            size: "large".into(),
            smoke: false,
            tools: vec![],
            resume: false,
            fresh: false,
            results_dir: None,
            require_sha: None,
            dry_run: true,
            manifest: None,
        }
    }

    /// `run` with no flags is the published run over the manifest, and the
    /// script's environment knobs are its flags.
    #[test]
    fn the_run_verb_composes_the_published_plan_from_flags() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let plan = plan_of(&ctx, &args()).unwrap();
        assert_eq!(
            plan.subjects,
            ["bazel", "envoy"],
            "skipped subjects are left out"
        );
        assert_eq!(plan.size, Size::Large);
        assert_eq!(plan.steps, StepKind::ALL);
        assert_eq!(plan.tools, ["bazel-diff", "tango"]);

        let plan = plan_of(
            &ctx,
            &RunArgs {
                subjects: vec!["envoy".into()],
                steps: vec!["walk".into(), "report".into()],
                smoke: true,
                resume: true,
                ..args()
            },
        )
        .unwrap();
        assert_eq!(plan.subjects, ["envoy"]);
        assert_eq!(plan.size, Size::Small);
        assert_eq!(plan.steps, [StepKind::Walk, StepKind::Report]);
        assert!(plan.resume);
        let lines: Vec<String> = plan.steps().iter().map(|s| s.command_line()).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("--results-dir bench-results-smoke"),
            "{}",
            lines[0]
        );

        assert!(plan_of(
            &ctx,
            &RunArgs {
                steps: vec!["lunch".into()],
                ..args()
            }
        )
        .is_err());
        assert!(plan_of(
            &ctx,
            &RunArgs {
                size: "huge".into(),
                ..args()
            }
        )
        .is_err());
    }
}
