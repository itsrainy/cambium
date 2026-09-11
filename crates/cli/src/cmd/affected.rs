//! `cambi affected`.

use anyhow::Result;
use cambium_engine::AffectedRequest;
use clap::Args as ClapArgs;

use crate::cmd::{emit_json, Context};
use crate::Format;

#[derive(ClapArgs)]
pub struct Args {
    /// The range, spelled as `git diff` spells it: `BASE..HEAD`, or a single
    /// revision for `<rev>..HEAD`. With no range, the merge base of
    /// `[git].default_branch` with HEAD.
    #[arg(value_name = "BASE..HEAD")]
    pub range: Option<String>,
    /// Revision to compare from (typically the merge base). The old spelling
    /// of the range's left half; it stays as an alias.
    #[arg(long)]
    pub base: Option<String>,
    /// Revision to compare to. Defaults to HEAD.
    #[arg(long)]
    pub head: Option<String>,
    /// Report only test targets.
    #[arg(long)]
    pub tests_only: bool,
    /// Include targets tagged `manual`.
    #[arg(long)]
    pub include_manual: bool,
    /// Deprecated: `cambi explain` is the verb. Kept for one release, hidden
    /// from help, and warned about by `compat`.
    #[arg(long, hide = true)]
    pub explain: bool,
    /// Print the plan — which index would be loaded, what the diff classifies
    /// as, what the run would do — and exit before any Bazel query.
    #[arg(long)]
    pub dry_run: bool,
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
    /// Write the head index produced by this run to this file. (The report
    /// itself goes to stdout; `--write-index` is the old spelling.)
    #[arg(long = "out", alias = "write-index", value_name = "FILE")]
    pub out: Option<std::path::PathBuf>,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    // A contradictory command line is the caller's error wherever it is run,
    // so it is answered before the workspace is opened: `cambi affected
    // a..b --base c` outside a Bazel tree must say what is wrong with it
    // rather than that there is no workspace.
    check_usage(&args)?;
    let engine = ctx.engine()?;
    let req = request(&engine, &args, Shape::affected(&args))?;
    if args.dry_run {
        return crate::cmd::plan::run(ctx, &engine, &req);
    }
    let outcome = engine.affected(&req)?;
    let report = &outcome.report;
    ctx.render.report(&report.diagnostics)?;

    if let (Some(path), Some(index)) = (&args.out, &outcome.index) {
        cambium_index::format::write_index(path, index)?;
    }

    match ctx.format {
        Format::Null => {}
        Format::Json | Format::Jsonl => {
            let fmt = ctx.format.data("cambi affected")?.expect("json or jsonl");
            if args.tests_only && !report.global {
                // `--tests-only` restricts `affected` to test targets; every
                // other field (including `direct`, which can name non-test
                // labels the change touched directly) is unchanged.
                let mut projected = report.clone();
                projected.affected.retain(|t| t.test);
                emit_json(&projected, fmt)?;
            } else {
                emit_json(report, fmt)?;
            }
        }
        Format::Query => {
            println!("{}", report.as_query_expression(args.tests_only));
        }
        Format::Human => {
            if report.global {
                // Exit code 2 says "global"; stdout still carries a usable
                // pattern so a pipeline can just run it.
                println!("//...");
                if let Some(r) = &report.global_reason {
                    if !ctx.render.quiet {
                        eprintln!("cambium: everything is affected: {r}");
                    }
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

/// What is wrong with the command line itself.
///
/// `--base` and `--head` are the old spelling of the range's two halves and
/// stay for the scripts that use them, but giving both a range and a flag is
/// not a deprecation to warn about: there is no reading of `cambi affected
/// a..b --base c` that is what someone meant.
pub fn check_usage(args: &Args) -> Result<()> {
    let Some(range) = &args.range else {
        return Ok(());
    };
    if let Some(flag) = args
        .base
        .as_ref()
        .map(|_| "--base")
        .or(args.head.as_ref().map(|_| "--head"))
    {
        return Err(crate::diag::UsageError::raised(format!(
            "`cambi affected {range}` already says which revisions to compare; drop {flag}"
        )));
    }
    if range.contains("...") {
        return Err(crate::diag::UsageError::raised(format!(
            "`{range}`: cambi takes a two-dot range (BASE..HEAD); a bare revision already \
             means <rev>..HEAD"
        )));
    }
    Ok(())
}

/// The two revisions this run compares: the range, the old flags, or — with
/// neither — the merge base of the default branch with HEAD.
pub fn resolve_range(engine: &cambium_engine::Engine, args: &Args) -> Result<(String, String)> {
    if let Some(range) = &args.range {
        return Ok(match range.split_once("..") {
            Some((base, "")) => (base.to_string(), "HEAD".to_string()),
            Some((base, head)) => (base.to_string(), head.to_string()),
            None => (range.clone(), "HEAD".to_string()),
        });
    }
    let head = args.head.clone().unwrap_or_else(|| "HEAD".to_string());
    if let Some(base) = &args.base {
        return Ok((base.clone(), head));
    }
    // No range at all: what someone on a feature branch means by "what did I
    // change" — the merge base of the default branch with where they are.
    let branch = &engine.config().git.default_branch;
    let base = engine.repo().merge_base(branch, &head).map_err(|e| {
        crate::diag::UsageError::raised(format!(
            "no merge base between {branch} and {head} ({e}); pass a range, or set \
             [git].default_branch in cambium.toml"
        ))
    })?;
    Ok((base, head))
}

/// What a run is for, beyond what its flags say: whether it explains itself,
/// and whether it builds a head index. Only a run that someone will read a
/// graph out of materialises one — it is the most expensive step there is —
/// and only a run that will print explanations computes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub explain: bool,
    pub materialize: bool,
}

impl Shape {
    /// `--dry-run`: the plan reads neither.
    pub const PLAN: Self = Self {
        explain: false,
        materialize: false,
    };

    /// `cambi affected`: explanations only under the deprecated `--explain`;
    /// an index only when `--out` asked for the file.
    pub fn affected(args: &Args) -> Self {
        Self {
            explain: args.explain,
            materialize: args.out.is_some(),
        }
    }

    /// `cambi explain`: explanations always; an index when there is a label
    /// to look up in the head graph, to answer "not affected" rather than
    /// "no such target".
    pub fn explain(label: Option<&str>) -> Self {
        Self {
            explain: true,
            materialize: label.is_some(),
        }
    }
}

/// The request both `affected` and `explain` run.
///
/// One construction for the two verbs: `explain` is the same computation with
/// the explanations on, so anything that changes what a run means — a new
/// flag, a different default — has to change it for both or for neither.
pub fn request(
    engine: &cambium_engine::Engine,
    args: &Args,
    shape: Shape,
) -> Result<AffectedRequest> {
    let (base, head) = resolve_range(engine, args)?;
    Ok(AffectedRequest {
        base,
        head,
        include_manual: args.include_manual,
        explain: shape.explain,
        materialize: shape.materialize,
        no_full_build: args.no_full_build,
        publish_base: args.publish_base,
        // `--publish-base` follows the config's `baseline.publish_partial`.
        allow_partial: false,
        max_depth: args.max_depth,
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    /// Parses `cambi affected <args>` the way `main` would.
    fn parse(args: &[&str]) -> super::Args {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: super::Args,
        }
        Wrapper::parse_from(std::iter::once("affected").chain(args.iter().copied())).args
    }

    #[test]
    fn a_range_and_the_old_flags_together_are_a_usage_error() {
        let err = super::check_usage(&parse(&["a..b", "--base", "c"])).expect_err("a usage error");
        assert!(err.to_string().contains("drop --base"), "{err}");
        let err = super::check_usage(&parse(&["a..b", "--head", "c"])).expect_err("a usage error");
        assert!(err.to_string().contains("drop --head"), "{err}");
        assert!(super::check_usage(&parse(&["a..b"])).is_ok());
        assert!(super::check_usage(&parse(&["--base", "a"])).is_ok());
    }

    #[test]
    fn a_three_dot_range_says_what_to_type_instead() {
        let err = super::check_usage(&parse(&["a...b"])).expect_err("a usage error");
        assert!(err.to_string().contains("two-dot range"), "{err}");
    }
}
