//! `cambi affected`.

use anyhow::Result;
use cambium_engine::AffectedRequest;
use clap::Args as ClapArgs;

use crate::cmd::result_event;
use crate::cmd::Context;
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
    let req = request(&engine, &args, args.explain, args.out.is_some())?;
    let outcome = engine.affected(&req)?;
    let report = &outcome.report;
    // Under --format json the diagnostics are already in the document; every
    // other form puts them on stderr, where they cannot be mistaken for the
    // answer. `jsonl` streams them first, as events, so a watcher sees them as
    // the run raises them rather than only at the end.
    if !matches!(ctx.format, Format::Json) {
        for d in &report.diagnostics {
            match ctx.format {
                Format::Jsonl => {
                    if !ctx.render.quiet {
                        println!("{}", diagnostic_event(d)?);
                    }
                }
                _ => ctx.render.warn(d),
            }
        }
    }

    if let (Some(path), Some(index)) = (&args.out, &outcome.index) {
        cambium_index::format::write_index(path, index)?;
    }

    match ctx.format {
        Format::Null => {}
        Format::Json | Format::Jsonl => {
            if args.tests_only && !report.global {
                // `--tests-only` restricts `affected` to test targets; every
                // other field (including `direct`, which can name non-test
                // labels the change touched directly) is unchanged.
                let mut projected = report.clone();
                projected.affected.retain(|t| t.test);
                print_document(ctx, &projected)?;
            } else {
                print_document(ctx, report)?;
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
    let branch = &engine.cfg.git.default_branch;
    let base = engine.repo.merge_base(branch, &head).map_err(|e| {
        crate::diag::UsageError::raised(format!(
            "no merge base between {branch} and {head} ({e}); pass a range, or set \
             [git].default_branch in cambium.toml"
        ))
    })?;
    Ok((base, head))
}

/// The request both `affected` and `explain` run.
///
/// One construction for the two verbs: `explain` is the same computation with
/// the explanations on, so anything that changes what a run means — a new
/// flag, a different default — has to change it for both or for neither.
pub fn request(
    engine: &cambium_engine::Engine,
    args: &Args,
    explain: bool,
    materialize: bool,
) -> Result<AffectedRequest> {
    let (base, head) = resolve_range(engine, args)?;
    Ok(AffectedRequest {
        base,
        head,
        include_manual: args.include_manual,
        explain,
        // Only materialise a head index when someone needs one: the caller
        // asked for the file, or `explain` has a label to look up. Otherwise
        // the run walks a delta overlay and never builds one.
        materialize,
        no_full_build: args.no_full_build,
        publish_base: args.publish_base,
        // `--publish-base` follows the config's `baseline.publish_partial`.
        allow_partial: false,
        max_depth: args.max_depth,
    })
}

/// The report, as the one document or as the single `result` event.
fn print_document(ctx: &Context, report: &cambium_engine::AffectedReport) -> Result<()> {
    match ctx.format {
        Format::Jsonl => println!("{}", result_event(report)?),
        _ => println!("{}", serde_json::to_string_pretty(report)?),
    }
    Ok(())
}

/// One streamed diagnostic line.
fn diagnostic_event(d: &cambium_engine::Diagnostic) -> Result<String> {
    let mut doc = serde_json::json!({"event": "diagnostic"});
    let map = doc.as_object_mut().expect("an object");
    if let serde_json::Value::Object(fields) = serde_json::to_value(d)? {
        map.extend(fields);
    }
    Ok(serde_json::to_string(&doc)?)
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
