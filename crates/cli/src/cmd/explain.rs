//! `cambi explain [LABEL] [BASE..HEAD]`: why is this target affected, or why
//! did the run go the size it did.
//!
//! It is the same computation `affected` runs (the same range, the same
//! flags, the same `AffectedRequest`, built by `affected::request`) with the
//! explanations turned on. It is a verb rather than a flag because its output
//! has its own shape and its own audience: someone who already has an answer
//! and wants to know where it came from.
//!
//! Two scopes share one document. With a label it is about that target: the
//! shortest path back to the change and the direct reason at the end of it.
//! With none it is about the run: the cause that made it global or cold, with
//! the diagnostic that named the fix, or the four numbers that say why the
//! answer was the size it was.

use anyhow::Result;
use cambium_engine::{AffectedReport, Diagnostic, Explanation};
use clap::Args as ClapArgs;

use crate::cmd::affected::Shape;
use crate::cmd::{emit, git_short, plural_s, Context};

#[derive(ClapArgs)]
// Both this and `affected::Args` are `Args`, and clap names an argument group
// after the struct; `explain` adds no group of its own, so it skips one.
#[group(skip)]
pub struct Args {
    /// The target to explain. With none, the run explains itself.
    #[arg(value_name = "LABEL")]
    pub label: Option<String>,
    /// The range and the flags `affected` takes.
    #[command(flatten)]
    pub affected: crate::cmd::affected::Args,
}

pub fn run(ctx: &Context, mut args: Args) -> Result<u8> {
    // `cambi explain a..b` is a range, not a target: a label always starts
    // with `//`, `@` or `:`, so the one positional can be told apart without
    // asking the caller to remember an order.
    if let Some(first) = &args.label {
        if !is_label(first) && args.affected.range.is_none() {
            args.affected.range = args.label.take();
        }
    }
    crate::cmd::affected::check_usage(&args.affected)?;
    // Before the workspace is opened, like every other usage error: asking
    // `explain` for `--format query` is wrong wherever it is run, and
    // answering it with "no workspace here" would hide that.
    let fmt = ctx.format().data("cambi explain")?;

    let engine = ctx.engine()?;
    // `--dry-run` is one of the flags `explain` shares with `affected`, and it
    // means the same thing here: say what a run would do and stop.
    if args.affected.dry_run {
        let resolved = crate::cmd::affected::request(&engine, &args.affected, Shape::PLAN)?;
        crate::cmd::affected::say_note(ctx, &resolved);
        return crate::cmd::plan::run(ctx, &engine, &resolved.req);
    }
    let resolved = crate::cmd::affected::request(
        &engine,
        &args.affected,
        Shape::explain(args.label.as_deref()),
    )?;
    // The same computation `affected` runs is the same three phases on
    // stderr, drawn by the same renderer.
    let outcome = crate::cmd::affected::narrate(ctx, &engine, &resolved, "explain", None)?;
    let report = &outcome.report;

    let about = match &args.label {
        None => About::Run,
        Some(label) => about_label(&outcome, label)?,
    };

    let doc = document(report, args.label.as_deref(), &about)?;
    emit(&doc, fmt, |_| {
        print!(
            "{}",
            render(report, args.label.as_deref(), &about, ctx.render.color)
        )
    })?;
    // An unaffected label is an answer, not an error, and a global run is
    // still the answer to the question asked: `affected` is the verb whose
    // exit code carries the verdict.
    Ok(crate::EXIT_OK)
}

/// What the answer is about: the run itself, or one label and what reached it.
enum About {
    Run,
    Affected(Box<Explanation>),
    /// Affected only because the whole repository is.
    GloballyAffected,
    NotAffected {
        /// Why not, in one line, when there is something to say beyond "no
        /// changed input reached it".
        note: String,
    },
}

/// Looks the label up in the run's own graph.
///
/// A label that exists in neither index is a mistyped command line rather than
/// an answer: `explain` cannot say anything about a target that is not there,
/// and saying "not affected" about a typo is the least useful thing it could
/// do.
fn about_label(outcome: &cambium_engine::RunOutcome, label: &str) -> Result<About> {
    let report = &outcome.report;
    if report.global {
        return Ok(About::GloballyAffected);
    }
    if let Some(e) = report
        .explanations
        .iter()
        .flatten()
        .find(|e| e.target == label)
    {
        return Ok(About::Affected(Box::new(e.clone())));
    }

    let cambium_engine::LabelPresence { at_head, at_base } = outcome.label_presence(label);
    match (at_head, at_base) {
        (Some(false), Some(true)) => Ok(About::NotAffected {
            note: format!("it no longer exists at {}", git_short(&report.head)),
        }),
        (Some(false), Some(false)) => Err(crate::diag::UsageError::raised(format!(
            "{label} is in neither index: it exists at neither {} nor {}",
            git_short(&report.base),
            git_short(&report.head)
        ))),
        // Either the head index was not materialised or the base could not be
        // read back; the answer about the change still holds.
        _ => Ok(About::NotAffected {
            note: format!(
                "no changed input reached it: {} changed file{} dirtied {} package{}, and \
                 nothing in their reverse closure names it",
                report.stats.changed_files,
                plural_s(report.stats.changed_files),
                report.stats.dirty_packages,
                plural_s(report.stats.dirty_packages),
            ),
        }),
    }
}

fn is_label(s: &str) -> bool {
    s.starts_with("//") || s.starts_with('@') || s.starts_with(':')
}

// --------------------------------------------------------------------- human

/// The whole human answer as text, so the shapes above can be tested without
/// a run: everything here is a function of the report and the label.
fn render(report: &AffectedReport, label: Option<&str>, about: &About, color: bool) -> String {
    let mut out = String::new();
    match (label, about) {
        (Some(l), About::Affected(e)) => {
            out.push_str(&format!("{l} is affected\n"));
            out.push_str(&format!("  {}\n", super::affected::path_line(&e.path)));
            out.push_str(&format!("  because {}\n", e.reason.describe()));
        }
        (Some(l), About::NotAffected { note }) => {
            out.push_str(&format!("{l} is not affected\n"));
            out.push_str(&format!("  {note}\n"));
        }
        (Some(l), About::GloballyAffected) => {
            out.push_str(&format!("{l} is affected because everything is\n"));
            out.push_str(&run_story(report, color));
        }
        _ => out.push_str(&run_story(report, color)),
    }
    out
}

/// The run's own story: the cause it went global or cold for, with the
/// diagnostic that names the fix, or the numbers that say why it was this
/// size.
fn run_story(report: &AffectedReport, color: bool) -> String {
    if report.global {
        return format!(
            "everything is affected\n{}",
            cause(
                report.global_cause(),
                report.global_reason.as_deref(),
                color
            )
        );
    }
    if report.stats.cold_rebuild {
        return format!(
            "{}..{} re-indexed the whole repository at head; the answer is still exact\n{}",
            git_short(&report.base),
            git_short(&report.head),
            cause(
                report.cold_rebuild_cause(),
                report.cold_rebuild_reason.as_deref(),
                color
            )
        );
    }
    format!(
        "{}..{} was answered incrementally\n  \
         changed files      {}\n  \
         dirty packages     {}\n  \
         direct targets     {}{}\n  \
         affected targets   {}\n  \
         {}\n",
        git_short(&report.base),
        git_short(&report.head),
        report.stats.changed_files,
        report.stats.dirty_packages,
        report.stats.direct_targets,
        excluded_by_tag(report.stats.direct_excluded_by_tag),
        report.stats.affected_targets,
        base_index(report),
    )
}

/// The diagnostic the engine already emitted for this cause, rendered with its
/// help line. Its message carries the reason verbatim (the engine's own
/// tests hold that), so the reason is printed on its own only when there is
/// no diagnostic: a run replayed from a document, or a cause older than the
/// codes.
fn cause(found: Option<&Diagnostic>, reason: Option<&str>, color: bool) -> String {
    match (found, reason) {
        (Some(d), _) => format!("{}\n", crate::diag::render_human(d, color)),
        (None, Some(r)) => format!("  because {r}\n"),
        (None, None) => String::new(),
    }
}

/// Why `direct` can hold more targets than `affected`: the ones a tag in
/// `output.exclude_tags` dropped, said beside the count they are part of.
fn excluded_by_tag(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(", {n} of them dropped by output.exclude_tags")
    }
}

/// The index this run answered from, in the terms the report carries: the
/// revision it was built for and how big the graph it brought was.
fn base_index(report: &AffectedReport) -> String {
    format!(
        "base index         {} ({} nodes)",
        git_short(&report.base),
        report.stats.base_nodes
    )
}

// ---------------------------------------------------------------------- json

/// The one `explain` document, in both scopes (README, "The JSON contract").
fn document(
    report: &AffectedReport,
    label: Option<&str>,
    about: &About,
) -> Result<serde_json::Value> {
    let mut doc = serde_json::json!({
        "schema": cambium_engine::report::SCHEMA_VERSION,
        "base": report.base,
        "head": report.head,
        "global": report.global,
        "diagnostics": report.diagnostics,
        "summary": {
            "changed_files": report.stats.changed_files,
            "dirty_packages": report.stats.dirty_packages,
            "direct_targets": report.stats.direct_targets,
            "affected_targets": report.stats.affected_targets,
            "base_nodes": report.stats.base_nodes,
            "cold_rebuild": report.stats.cold_rebuild,
        },
    });
    let map = doc.as_object_mut().expect("an object");
    if let Some(r) = &report.global_reason {
        map.insert("global_reason".into(), r.clone().into());
    }
    if let Some(r) = &report.cold_rebuild_reason {
        map.insert("cold_rebuild_reason".into(), r.clone().into());
    }
    if let Some(l) = label {
        map.insert("label".into(), l.into());
        match about {
            About::Affected(e) => {
                map.insert("affected".into(), true.into());
                map.insert("path".into(), serde_json::json!(e.path));
                map.insert("reason".into(), serde_json::to_value(&e.reason)?);
            }
            About::GloballyAffected => {
                map.insert("affected".into(), true.into());
            }
            About::NotAffected { note } => {
                map.insert("affected".into(), false.into());
                map.insert("why_not".into(), note.clone().into());
            }
            About::Run => {}
        }
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_engine::{Code, DirectReason, Stats};

    fn report() -> AffectedReport {
        AffectedReport {
            base: "e5ac5431111".into(),
            head: "ca7fa292222".into(),
            stats: Stats {
                changed_files: 3,
                dirty_packages: 2,
                direct_targets: 4,
                affected_targets: 9,
                base_nodes: 120,
                ..Stats::default()
            },
            ..AffectedReport::default()
        }
    }

    /// The block `docs/CLI.md` asks for: the label, the path back to the
    /// change, and the reason at the end of it.
    #[test]
    fn a_label_that_is_affected_is_one_block_of_three_lines() {
        let about = About::Affected(Box::new(Explanation {
            target: "//foo:bar".into(),
            path: vec!["//foo:bar".into(), "//lib:core".into(), "//lib:base".into()],
            direct: "//lib:base".into(),
            reason: DirectReason::File {
                path: "lib/base.cc".into(),
            },
        }));
        assert_eq!(
            render(&report(), Some("//foo:bar"), &about, false),
            "//foo:bar is affected\n  \
             //foo:bar <- //lib:core <- //lib:base  (2 hops, 1 package boundary)\n  \
             because source file lib/base.cc changed\n"
        );
    }

    #[test]
    fn a_label_that_is_not_affected_says_so_and_why_not() {
        let about = About::NotAffected {
            note: "no changed input reached it".into(),
        };
        assert_eq!(
            render(&report(), Some("//foo:bar"), &about, false),
            "//foo:bar is not affected\n  no changed input reached it\n"
        );
    }

    /// A global run is not a per-target answer, so the label's line says that
    /// and the run's own story follows, with the diagnostic the engine
    /// already raised for the cause, its code, and its help line.
    #[test]
    fn a_global_run_names_the_causes_diagnostic_and_its_help_line() {
        let mut r = report();
        r.global = true;
        r.global_reason = Some("invalidator path .bazelrc matched".into());
        r.diagnostics = vec![Diagnostic::warning(
            Code::W013InvalidatorMatched,
            "a changed path matches an invalidator, so everything is affected: \
             invalidator path .bazelrc matched",
        )
        .with_help("take it out of [invalidators].paths")];
        let out = render(&r, Some("//foo:bar"), &About::GloballyAffected, false);
        assert_eq!(
            out,
            "//foo:bar is affected because everything is\n\
             everything is affected\n\
             warning[CAMBI-W013]: a changed path matches an invalidator, so everything is \
             affected: invalidator path .bazelrc matched\n  \
             = help: take it out of [invalidators].paths\n",
            "the cause, its code and the fix, and no second copy of the reason the \
             message already carries"
        );
        // With no label it is the same story without the first line.
        assert!(render(&r, None, &About::Run, false).starts_with("everything is affected\n"));
    }

    #[test]
    fn a_cold_rebuild_is_its_own_story() {
        let mut r = report();
        r.stats.cold_rebuild = true;
        let reason = cambium_engine::reason::cold_rebuild_reason(9001, 2000);
        r.cold_rebuild_reason = Some(reason.clone());
        r.diagnostics = vec![cambium_engine::reason::cold_rebuild_diagnostic(&reason)];
        let out = render(&r, None, &About::Run, false);
        assert!(out.contains("re-indexed the whole repository"), "{out}");
        assert!(out.contains("CAMBI-W012"), "{out}");
        assert_eq!(
            out.matches("9001").count(),
            1,
            "the reason is in the diagnostic and printed once:\n{out}"
        );
    }

    /// A document from before the codes existed has the reason and no
    /// diagnostic; the reason is still the answer.
    #[test]
    fn a_cause_without_a_diagnostic_still_prints_the_reason() {
        let mut r = report();
        r.global = true;
        r.global_reason = Some("MODULE.bazel matches an invalidator".into());
        let out = render(&r, None, &About::Run, false);
        assert_eq!(
            out,
            "everything is affected\n  because MODULE.bazel matches an invalidator\n"
        );
    }

    /// Neither global nor cold: the four numbers and the base, so someone can
    /// see why the answer was the size it was. Not "by splicing": the
    /// narration calls the replay of a stored index forward to the base a
    /// splice, and says `skipped` when the index was already at the base.
    #[test]
    fn an_ordinary_run_accounts_for_its_own_size() {
        let out = render(&report(), None, &About::Run, false);
        assert_eq!(
            out,
            "e5ac543..ca7fa29 was answered incrementally\n  \
             changed files      3\n  \
             dirty packages     2\n  \
             direct targets     4\n  \
             affected targets   9\n  \
             base index         e5ac543 (120 nodes)\n"
        );
        assert!(!out.contains("splic"), "{out}");
    }

    /// `affected` is `direct` plus its reverse closure less what
    /// `output.exclude_tags` drops, so a run where direct targets were
    /// dropped says how many, beside the number they are missing from.
    #[test]
    fn direct_targets_dropped_by_tag_are_said() {
        let mut r = report();
        r.stats.direct_targets = 172;
        r.stats.affected_targets = 167;
        r.stats.direct_excluded_by_tag = 8;
        let out = render(&r, None, &About::Run, false);
        assert!(
            out.contains("  direct targets     172, 8 of them dropped by output.exclude_tags\n"),
            "{out}"
        );
    }

    #[test]
    fn the_one_positional_is_a_range_when_it_is_not_a_label() {
        assert!(is_label("//foo:bar") && is_label("@repo//a:b") && is_label(":b"));
        assert!(!is_label("origin/main..HEAD") && !is_label("HEAD~3"));
    }

    /// The document is one shape in both scopes: the run's fields always, the
    /// label's only when a label was asked about.
    #[test]
    fn the_json_document_carries_the_label_fields_only_with_a_label() {
        let run = document(&report(), None, &About::Run).unwrap();
        assert_eq!(run["schema"], cambium_engine::report::SCHEMA_VERSION);
        assert_eq!(run["summary"]["affected_targets"], 9);
        assert!(run.get("label").is_none() && run.get("affected").is_none());

        let about = About::Affected(Box::new(Explanation {
            target: "//foo:bar".into(),
            path: vec!["//foo:bar".into(), "//lib:base".into()],
            direct: "//lib:base".into(),
            reason: DirectReason::File {
                path: "lib/base.cc".into(),
            },
        }));
        let one = document(&report(), Some("//foo:bar"), &about).unwrap();
        assert_eq!(one["label"], "//foo:bar");
        assert_eq!(one["affected"], true);
        assert_eq!(one["path"][1], "//lib:base");
        assert_eq!(one["reason"]["kind"], "file");
        assert_eq!(one["reason"]["path"], "lib/base.cc");

        let none = document(
            &report(),
            Some("//foo:bar"),
            &About::NotAffected {
                note: "no changed input reached it".into(),
            },
        )
        .unwrap();
        assert_eq!(none["affected"], false);
        assert_eq!(none["why_not"], "no changed input reached it");
    }
}
