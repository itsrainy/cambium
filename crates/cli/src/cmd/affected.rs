//! `cambi affected`.

use anyhow::Result;
use cambium_engine::AffectedRequest;
use clap::Args as ClapArgs;

use crate::cmd::Context;
use crate::OutputFormat;

#[derive(ClapArgs)]
pub struct Args {
    /// Revision to compare from (typically the merge base).
    #[arg(long)]
    pub base: String,
    /// Revision to compare to. Defaults to HEAD.
    #[arg(long, default_value = "HEAD")]
    pub head: String,
    /// Report only test targets.
    #[arg(long)]
    pub tests_only: bool,
    /// Include targets tagged `manual`.
    #[arg(long)]
    pub include_manual: bool,
    /// For each affected target, show the shortest path back to the change.
    #[arg(long)]
    pub explain: bool,
    /// Fail instead of falling back to a cold full build.
    #[arg(long)]
    pub no_full_build: bool,
    /// If this run had to compute the base index — because the store had none
    /// for --base and one was replayed forward from an ancestor, or built cold
    /// — publish it under the base revision, so the next run finds it. The
    /// head-side index is never published from `affected`.
    #[arg(long)]
    pub publish_base: bool,
    /// Report only targets within this many reverse-dependency hops of the
    /// change (0 = the direct set).
    #[arg(long)]
    pub max_depth: Option<u32>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,
    /// Write the head index produced by this run to this file. (The report
    /// itself goes to stdout; `--write-index` is the old spelling.)
    #[arg(long = "out", alias = "write-index", value_name = "FILE")]
    pub out: Option<std::path::PathBuf>,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let engine = ctx.engine()?;
    let req = AffectedRequest {
        base: args.base.clone(),
        head: args.head.clone(),
        include_manual: args.include_manual,
        explain: args.explain,
        // Only materialise a head index when the caller asked for the file;
        // otherwise the run walks a delta overlay and never builds one.
        materialize: args.out.is_some(),
        no_full_build: args.no_full_build,
        publish_base: args.publish_base,
        // `--publish-base` follows the config's `baseline.publish_partial`.
        allow_partial: false,
        max_depth: args.max_depth,
    };
    let outcome = engine.affected(&req)?;
    let report = &outcome.report;

    if let (Some(path), Some(index)) = (&args.out, &outcome.index) {
        cambium_index::format::write_index(path, index)?;
    }

    match args.output {
        OutputFormat::Json => {
            if args.tests_only && !report.global {
                // `--tests-only` restricts `affected` to test targets; every
                // other field (including `direct`, which can name non-test
                // labels the change touched directly) is unchanged.
                let mut projected = report.clone();
                projected.affected.retain(|t| t.test);
                println!("{}", serde_json::to_string_pretty(&projected)?);
            } else {
                println!("{}", serde_json::to_string_pretty(report)?);
            }
        }
        OutputFormat::Query => {
            println!("{}", report.as_query_expression(args.tests_only));
        }
        OutputFormat::Text => {
            if report.global {
                // Exit code 2 says "global"; stdout still carries a usable
                // pattern so a pipeline can just run it.
                println!("//...");
                if let Some(r) = &report.global_reason {
                    eprintln!("cambium: everything is affected: {r}");
                }
            } else {
                let items: Vec<&str> = if args.tests_only {
                    report.tests.iter().map(String::as_str).collect()
                } else {
                    report.affected.iter().map(|t| t.label.as_str()).collect()
                };
                for l in items {
                    println!("{l}");
                }
                if args.explain {
                    print_explanations(report, args.tests_only);
                }
            }
        }
    }

    Ok(if report.global {
        crate::EXIT_GLOBAL
    } else {
        crate::EXIT_OK
    })
}

fn print_explanations(report: &cambium_engine::AffectedReport, tests_only: bool) {
    let Some(explanations) = &report.explanations else {
        return;
    };
    let wanted: std::collections::HashSet<&str> = if tests_only {
        report.tests.iter().map(|s| s.as_str()).collect()
    } else {
        report.affected.iter().map(|t| t.label.as_str()).collect()
    };
    eprintln!("\n--- why ---");
    for e in explanations {
        if !wanted.contains(e.target.as_str()) {
            continue;
        }
        eprintln!(
            "{}\n  {}\n  because {}",
            e.target,
            e.path.join(" <- "),
            e.reason.describe()
        );
    }
}
