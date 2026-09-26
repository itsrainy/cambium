//! `cambi profile`: what cambium costs and saves on this repository, in the
//! numbers someone would put in a proposal.
//!
//! A cold build at the start of the last N first-parent commits, one
//! incremental step per commit forward to the head, and a cold rebuild
//! wherever a commit went global: the benchmark's history workload, run over
//! one worktree in one process. It runs Bazel for minutes, so the walk
//! narrates itself on stderr through [`cambium_ui`] (a phase line per stage,
//! a row per commit, and on a terminal one live line underneath); the
//! document at the end is [`cambium_engine::Profile`], on stdout.

use std::io::IsTerminal as _;

use anyhow::{Context as _, Result};
use cambium_engine::profile::{self, CommitStep, Progress, WorktreeRevisions};
use cambium_engine::report::Stopwatch;
use cambium_engine::Diagnostic;
use cambium_engine::Profile;
use cambium_ui::report::{group, human_bytes, human_ms, kv, Pair};
use cambium_ui::{row, Cell, Event, Paint, Sink, Style};
use clap::Args as ClapArgs;

use crate::cmd::narrate::{commit_columns, index_cell};
use crate::cmd::{emit, short, Context};

/// What lands on stdout above the table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Report {
    /// An aligned key/value block: the same numbers, read down the left edge.
    Kv,
    /// The paragraphs someone pastes into a proposal. Kept one release.
    Prose,
}

#[derive(ClapArgs)]
pub struct Args {
    /// The revision the walk ends at (HEAD by default), or FROM..TO to walk
    /// from FROM to TO along TO's first-parent line. The oldest commit is
    /// indexed cold first, and an old commit is the likeliest not to build:
    /// FROM.. (or a smaller --commits) moves that build to a newer commit.
    #[arg(value_name = "REV")]
    rev: Option<String>,
    /// How many first-parent commits back to walk (30 by default). The
    /// oldest of them is indexed cold first; a smaller number, or a FROM..
    /// range, moves that build to a newer commit.
    #[arg(long)]
    commits: Option<usize>,
    /// Keep the index the walk ends with here, so `cambi index publish` can
    /// upload it. By default it is written to a scratch file and deleted.
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// How the report above the table reads.
    #[arg(long, value_enum, default_value_t = Report::Kv)]
    report: Report,
}

/// A row's cells. A global step answered `//...`, so it reads `all` rather
/// than the zero the document keeps; a failed step, and one indexed cold
/// after it, answered nothing, and read `-`.
fn row_cells(step: &CommitStep, subject: &str) -> Vec<Cell> {
    let count = |n: usize| {
        if step.global {
            Cell::text("all")
        } else if step.failed.is_some() || step.cold_after_failure {
            Cell::styled("-", Style::Dim)
        } else {
            Cell::text(group(n as u64))
        }
    };
    vec![
        Cell::Commit {
            sha: step.commit.clone(),
            date: step.date.clone(),
            subject: subject.to_string(),
        },
        Cell::text(human_ms(step.wall_ms)),
        count(step.affected),
        count(step.tests),
        index_cell(&step.index),
    ]
}

/// The first line of a diagnostic's message: a Bazel failure carries the
/// whole of its stderr after it, which belongs in the document, not in a
/// line of narration.
fn first_line(d: &Diagnostic) -> &str {
    d.message.lines().next().unwrap_or_default()
}

/// How the walk will read in the header: how many commits, and that the
/// oldest is indexed cold before any of them.
fn header_note(commits: usize) -> String {
    if commits == 0 {
        "no commit before it; a cold index only".to_string()
    } else {
        format!(
            "{commits} first-parent commit{}; the oldest is indexed cold first",
            plural(commits)
        )
    }
}

/// Where the walk ends and how many commits it takes: REV and `--commits`,
/// or a `FROM..TO` range, whose FROM must be on TO's first-parent line
/// because that is the only history the walk follows.
fn resolve_walk(
    engine: &cambium_engine::Engine,
    rev: Option<&str>,
    commits: Option<usize>,
) -> Result<(String, usize)> {
    let rev = rev.unwrap_or("HEAD");
    if rev.contains("...") {
        return Err(crate::diag::UsageError::raised(format!(
            "`{rev}`: cambi takes a two-dot range (FROM..TO); a bare revision is where the walk \
             ends"
        )));
    }
    let Some((from, to)) = rev.split_once("..") else {
        let head = engine
            .repo()
            .rev_parse(rev)
            .with_context(|| format!("{rev} is not a revision in this repository"))?;
        return Ok((head, commits.unwrap_or(DEFAULT_COMMITS)));
    };
    if commits.is_some() {
        return Err(crate::diag::UsageError::raised(format!(
            "`cambi profile {rev}` already says how far back to walk; drop --commits"
        )));
    }
    let to = if to.is_empty() { "HEAD" } else { to };
    let from_sha = engine
        .repo()
        .rev_parse(from)
        .with_context(|| format!("{from} is not a revision in this repository"))?;
    let head = engine
        .repo()
        .rev_parse(to)
        .with_context(|| format!("{to} is not a revision in this repository"))?;
    let count: usize = engine
        .repo()
        .git(&[
            "rev-list",
            "--first-parent",
            "--count",
            &format!("{from_sha}..{head}"),
        ])?
        .trim()
        .parse()
        .context("git rev-list --count printed something other than a number")?;
    let log = engine.repo().first_parent_log(&head, count + 1)?;
    if log.first() != Some(&from_sha) {
        return Err(crate::diag::UsageError::raised(format!(
            "{from} is not on {to}'s first-parent line, which is the history the walk follows; \
             pass a commit on it, or --commits"
        )));
    }
    Ok((head, count))
}

/// `--commits` when neither it nor a range is given.
const DEFAULT_COMMITS: usize = 30;

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let engine = ctx.engine()?;
    let (head, commits) = resolve_walk(&engine, args.rev.as_deref(), args.commits)?;
    let (out, _scratch) = match &args.out {
        Some(p) => (p.clone(), None),
        None => {
            std::fs::create_dir_all(engine.cache_dir())?;
            let scratch = tempfile::NamedTempFile::new_in(engine.cache_dir())?;
            (scratch.path().to_path_buf(), Some(scratch))
        }
    };

    let mut sink = cambium_ui::boxed(ctx.ui);
    let repo_name = engine
        .workspace()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // The range the header names is one git call, before any Bazel runs, so
    // the first line is on screen while the cold build starts.
    let log = engine.repo().first_parent_log(&head, commits + 1)?;
    let from = log.first().cloned().unwrap_or_else(|| head.clone());
    sink.emit(Event::Header {
        verb: "profile".into(),
        repo: repo_name,
        range: format!("{}..{}", short(&from), short(&head)),
        note: header_note(log.len().saturating_sub(1)),
    });

    // The worktree outlives one `profile` call, so the CLI creates it and
    // reports it in the same vocabulary the walk uses for its own phases.
    let mut render = Renderer {
        sink: sink.as_mut(),
        opened: false,
    };
    render.on(Progress::Worktree {
        commit: head.clone(),
    });
    let sw = Stopwatch::start();
    let revs = WorktreeRevisions::create(&engine, &head)?;
    render.on(Progress::WorktreeReady {
        elapsed_ms: sw.elapsed_ms(),
    });

    let doc = profile::profile(&engine, &revs, &head, commits, &out, &mut |p| render.on(p));
    sink.emit(Event::Done);
    drop(revs);
    let doc = doc?;

    // Each global raised its diagnostic, so a repository whose release
    // commits all go global for the same reason would print it once per
    // release; once is enough, and the report says how many.
    let mut printed = std::collections::HashSet::new();
    for d in &doc.diagnostics {
        if printed.insert((d.code, d.message.as_str())) {
            ctx.render.warn(d);
        }
    }
    emit(&doc, ctx.format.data("cambi profile")?, |p| {
        print_human(
            p,
            args.report,
            engine.workspace(),
            args.out.as_deref(),
            ctx.ui.width,
            // The table is stdout's, so it is painted only when stdout is a
            // terminal, and never when the invocation asked for no colour at
            // all.
            Paint::new(ctx.ui.colour && std::io::stdout().is_terminal()),
        )
    })?;
    Ok(crate::EXIT_OK)
}

/// Turns the walk's progress into the grammar the sink draws.
struct Renderer<'a> {
    sink: &'a mut dyn Sink,
    /// The `commits` phase has printed its header.
    opened: bool,
}

impl Renderer<'_> {
    fn on(&mut self, p: Progress) {
        match p {
            Progress::Range { .. } => {}
            Progress::Worktree { commit } => self.sink.emit(Event::PhaseStart {
                name: "worktree".into(),
                detail: Some(format!("checking out {}", short(&commit))),
            }),
            Progress::WorktreeReady { elapsed_ms } => self.sink.emit(Event::PhaseEnd {
                name: "worktree".into(),
                result: "ready".into(),
                elapsed_ms,
            }),
            Progress::ColdBuild { commit } => {
                self.sink.emit(Event::PhaseStart {
                    name: "cold index".into(),
                    detail: Some(format!("bazel query //... at {}", short(&commit))),
                });
                self.sink
                    .emit(Event::Current(format!("indexing {}", short(&commit))));
            }
            Progress::ColdBuilt {
                targets,
                elapsed_ms,
            } => self.sink.emit(Event::PhaseEnd {
                name: "cold index".into(),
                result: format!(
                    "{} target{}",
                    group(targets as u64),
                    plural(targets as usize)
                ),
                elapsed_ms,
            }),
            Progress::ColdFailed {
                commit,
                error,
                elapsed_ms,
            } => {
                self.sink.emit(Event::PhaseEnd {
                    name: "cold index".into(),
                    result: format!("failed ({})", error.code),
                    elapsed_ms,
                });
                self.sink.emit(Event::Log(
                    cambium_ui::Level::Warning,
                    format!(
                        "{} did not build: {}; the next commit is indexed cold in its place",
                        short(&commit),
                        first_line(&error)
                    ),
                ));
            }
            Progress::Step { index, of, commit } => {
                if !self.opened {
                    self.opened = true;
                    self.sink.emit(Event::PhaseStart {
                        name: "commits".into(),
                        detail: Some(format!("{of} step{}", plural(of))),
                    });
                    self.sink.emit(Event::RowHeader(commit_columns("commit")));
                }
                self.sink
                    .emit(Event::Current(format!("[{index}/{of}] {}", short(&commit))));
            }
            Progress::Rebuild {
                index,
                commit,
                reason,
                code,
            } => {
                // The reason is in the report below, with its help line; the
                // narration names the code, which is what someone looks up.
                let why = code.map(|c| c.to_string()).unwrap_or(reason);
                self.sink.emit(Event::Log(
                    cambium_ui::Level::Warning,
                    format!("{} went global ({why}); rebuilding", short(&commit)),
                ));
                self.sink.emit(Event::Current(format!(
                    "[{index}] rebuilding at {}",
                    short(&commit)
                )));
            }
            Progress::Recover { index, commit } => self.sink.emit(Event::Current(format!(
                "[{index}] indexing {} cold",
                short(&commit)
            ))),
            Progress::Stepped { step, subject, .. } => {
                if let Some(error) = &step.failed {
                    self.sink.emit(Event::Log(
                        cambium_ui::Level::Warning,
                        format!(
                            "{} failed ({}): {}; the next commit is indexed cold",
                            short(&step.commit),
                            error.code,
                            first_line(error)
                        ),
                    ));
                }
                let cells = row_cells(&step, &subject);
                self.sink.emit(match serde_json::to_value(&step) {
                    Ok(data) => Event::DataRow {
                        cells,
                        key: "step".into(),
                        data,
                    },
                    Err(_) => Event::Row(cells),
                });
            }
            Progress::Stopped { reason } => self.sink.emit(Event::Log(
                cambium_ui::Level::Warning,
                format!("stopped: {reason}"),
            )),
            Progress::Walked {
                commits,
                globals,
                elapsed_ms,
            } => {
                if self.opened {
                    self.sink.emit(Event::PhaseEnd {
                        name: "commits".into(),
                        result: if globals == 0 {
                            format!("{commits} walked")
                        } else {
                            format!("{commits} walked, {globals} global")
                        },
                        elapsed_ms,
                    });
                }
            }
        }
    }
}

fn print_human(
    p: &Profile,
    report: Report,
    workspace: &std::path::Path,
    kept: Option<&std::path::Path>,
    width: usize,
    paint: Paint,
) {
    match report {
        Report::Kv => {
            for line in kv(&kv_pairs(p, workspace)) {
                println!("{line}");
            }
        }
        Report::Prose => print_prose(p, workspace),
    }
    if p.summary.commits == 0 {
        println!();
        println!(
            "No commit precedes {} on its first-parent line, so nothing was walked.",
            short(&p.head)
        );
        return;
    }
    println!();
    println!(
        "{}",
        row::render_header(&commit_columns("commit"), width, &paint)
    );
    for c in &p.commits {
        println!(
            "{}",
            row::render(
                &commit_columns("commit"),
                &row_cells(c, &c.subject),
                width,
                &paint
            )
        );
    }
    if let Some(kept) = kept {
        println!();
        println!(
            "The index for {} is at {}; `cambi index publish {}` uploads it.",
            short(&p.index.commit),
            kept.display(),
            kept.display()
        );
    }
}

/// What every per-commit number here leaves out, said once at the top of the
/// report and once in the paragraphs, so nobody reads a warm figure as a CI
/// runner's.
const MEASURED: &str = "one Bazel server kept warm across steps and the index in memory; a cold \
                        CI runner also pays the store fetch, a server start and external fetches";

fn percent(share: f64) -> String {
    let pct = share * 100.0;
    if pct > 0.0 && pct < 1.0 {
        "<1%".to_string()
    } else {
        format!("{pct:.0}%")
    }
}

/// The aligned block: every number the paragraphs quote, in the order someone
/// reads them. Every figure in it is a field of the JSON document.
fn kv_pairs(p: &Profile, workspace: &std::path::Path) -> Vec<Pair> {
    let i = &p.index;
    let s = &p.summary;
    let inc = &s.incremental;
    let name = if p.repository.is_empty() {
        workspace
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| workspace.display().to_string())
    } else {
        p.repository.clone()
    };
    let mut pairs = vec![
        Pair::new(
            "repository",
            name,
            format!(
                "at {}, last {} first-parent commit{}",
                short(&p.head),
                s.commits,
                plural(s.commits)
            ),
        ),
        Pair::new("measured", "", MEASURED),
        Pair::new(
            "targets",
            group(i.targets as u64),
            format!(
                "{} tests, {} package{}, {} external repositor{}",
                group(i.tests as u64),
                group(i.packages as u64),
                plural(i.packages as usize),
                group(i.external_repos as u64),
                if i.external_repos == 1 { "y" } else { "ies" },
            ),
        ),
        Pair::new(
            "full index",
            human_ms(p.cold.wall_ms),
            format!(
                "at {} ({} targets, {} tests); {} on disk ({}), loads in {}",
                short(&p.cold.commit),
                group(p.cold.targets as u64),
                group(p.cold.tests as u64),
                human_bytes(i.bytes_on_disk),
                i.compression,
                human_ms(i.load_ms)
            ),
        ),
    ];
    if let Some(error) = &p.from_failed {
        pairs.push(Pair::new(
            "",
            "",
            format!(
                "the range's first commit, {}, did not build ({}): {}",
                short(&p.from),
                error.code,
                first_line(error)
            ),
        ));
    }
    if s.commits == 0 {
        return pairs;
    }
    pairs.push(Pair::new(
        "per commit",
        human_ms(inc.mean_ms),
        if inc.commits == 0 {
            "no commit was answered incrementally".to_string()
        } else {
            format!(
                "mean of {} incremental, p50 {}, p90 {}, max {}; {} mean over all {} with \
                 rebuilds",
                inc.commits,
                human_ms(inc.p50_ms),
                human_ms(inc.p90_ms),
                human_ms(inc.max_ms),
                human_ms(s.mean_ms),
                s.commits - s.failed,
            )
        },
    ));
    if inc.commits > 0 {
        let a = &s.incremental_affected;
        let t = &s.incremental_tests;
        pairs.push(Pair::new(
            "affected",
            group(a.p50),
            format!(
                "targets on a typical commit (p50), {} of {} (manual included); mean {}, \
                 p90 {}",
                percent(s.affected_share),
                group(p.cold.targets as u64),
                group(a.mean.round() as u64),
                group(a.p90),
            ),
        ));
        pairs.push(Pair::new(
            "tests",
            group(t.p50),
            format!(
                "tests on a typical commit (p50), {} of {}; mean {}, p90 {}",
                percent(s.tests_share),
                group(p.cold.tests as u64),
                group(t.mean.round() as u64),
                group(t.p90),
            ),
        ));
    }
    let answered = s.commits - s.failed;
    pairs.push(Pair::new(
        "global",
        format!("{}/{}", s.globals, answered),
        match s.rebuild_every_commits {
            None => "no commit invalidated the whole graph".to_string(),
            Some(every) => format!(
                "{} of commits answered //..., one every {every:.1} commits",
                percent(s.global_share)
            ),
        },
    ));
    for r in &s.global_reasons {
        let code = r.code.map(|c| format!("{c}: ")).unwrap_or_default();
        pairs.push(Pair::new(
            "",
            "",
            format!("{} x {code}{}", r.count, r.reason),
        ));
        if let Some(help) = &r.help {
            pairs.push(Pair::new("", "", format!("    help: {help}")));
        }
    }
    let mut rebuilt = Vec::new();
    if s.global_rebuild.commits > 0 {
        rebuilt.push(format!(
            "{} global at {} each",
            s.global_rebuild.commits,
            human_ms(s.global_rebuild.mean_ms)
        ));
    }
    if s.requery.commits > 0 {
        rebuilt.push(format!(
            "{} re-queried over limits.max_dirty_packages at {} each (exact, not global)",
            s.requery.commits,
            human_ms(s.requery.mean_ms)
        ));
    }
    if s.rebuilt_after_failure > 0 {
        rebuilt.push(format!(
            "{} indexed cold after a failed commit",
            s.rebuilt_after_failure
        ));
    }
    pairs.push(Pair::new(
        "rebuilds",
        format!("{}/{}", s.rebuild.commits, answered),
        if rebuilt.is_empty() {
            "the whole graph was never rebuilt".to_string()
        } else {
            rebuilt.join("; ")
        },
    ));
    pairs.push(Pair::new(
        "index work",
        format!("{}/{}", s.index_spliced, answered),
        format!(
            "spliced, {} changed nothing the graph models",
            s.index_unchanged
        ),
    ));
    if s.failed > 0 {
        pairs.push(Pair::new(
            "failed",
            format!("{}/{}", s.failed, s.commits),
            "left out of every figure above; the commit after each was indexed cold",
        ));
        for c in p.commits.iter() {
            if let Some(error) = &c.failed {
                pairs.push(Pair::new(
                    "",
                    "",
                    format!("{} {}: {}", short(&c.commit), error.code, first_line(error)),
                ));
            }
        }
    }
    if let Some(reason) = &p.stopped {
        pairs.push(Pair::new(
            "stopped",
            "early",
            format!("{reason}; a FROM.. range or a smaller --commits starts past them"),
        ));
    }
    pairs
}

/// The paragraph someone pastes into a proposal. Kept one release.
fn print_prose(p: &Profile, workspace: &std::path::Path) {
    let i = &p.index;
    let s = &p.summary;
    println!(
        "Profile of {} at {} (the last {} first-parent commit{}, from {})",
        workspace.display(),
        short(&p.head),
        s.commits,
        plural(s.commits),
        short(&p.from),
    );
    println!();
    println!(
        "This build graph has {} targets ({} tests) in {} packages, with {} external \
         repositor{}.",
        group(i.targets as u64),
        group(i.tests as u64),
        group(i.packages as u64),
        group(i.external_repos as u64),
        if i.external_repos == 1 { "y" } else { "ies" },
    );
    println!(
        "A full cambium index takes {} to build and {} on disk ({}); loading it takes {}.",
        human_ms(p.cold.wall_ms),
        human_bytes(i.bytes_on_disk),
        i.compression,
        human_ms(i.load_ms),
    );
    if s.commits == 0 {
        return;
    }
    println!();
    let inc = &s.incremental;
    if inc.commits == 0 {
        println!(
            "Based on the last {} commit{}, none was answered incrementally, so there is no \
             incremental time to report.",
            s.commits,
            plural(s.commits),
        );
    } else {
        println!(
            "Based on the last {} commit{}, cambium takes {} on average to calculate the changed \
             targets for a commit (p50 {}, p90 {}, max {} over the {} commit{} answered \
             incrementally). A typical commit (the median) affects {} of {} targets ({}, \
             manual included) and {} of {} tests ({}).",
            s.commits,
            plural(s.commits),
            human_ms(inc.mean_ms),
            human_ms(inc.p50_ms),
            human_ms(inc.p90_ms),
            human_ms(inc.max_ms),
            inc.commits,
            plural(inc.commits),
            group(s.incremental_affected.p50),
            group(p.cold.targets as u64),
            percent(s.affected_share),
            group(s.incremental_tests.p50),
            group(p.cold.tests as u64),
            percent(s.tests_share),
        );
    }
    println!("Measured with {MEASURED}.");
    match s.rebuild_every_commits {
        None => println!(
            "The entire graph never had to be rebuilt in those {} commit{}.",
            s.commits,
            plural(s.commits)
        ),
        Some(every) => {
            let reasons: Vec<String> = s
                .global_reasons
                .iter()
                .map(|r| match r.code {
                    Some(code) => format!("{} x {code}: {}", r.count, r.reason),
                    None => format!("{} x {}", r.count, r.reason),
                })
                .collect();
            println!(
                "The entire graph had to be rebuilt {} time{} ({} of commits), once every {:.1} \
                 commits on average, at {} per rebuild ({}).",
                s.globals,
                plural(s.globals),
                percent(s.global_share),
                every,
                human_ms(s.global_rebuild.mean_ms),
                reasons.join("; "),
            );
        }
    }
    if s.engine_cold_rebuilds > 0 {
        println!(
            "{} commit{} exceeded limits.max_dirty_packages and were answered by re-querying \
             the whole repository: exact, at {} each.",
            s.engine_cold_rebuilds,
            plural(s.engine_cold_rebuilds),
            human_ms(s.requery.mean_ms),
        );
    }
    if s.failed > 0 {
        println!(
            "{} commit{} failed to build and {} left out of every figure; the commit after \
             each was indexed cold.",
            s.failed,
            plural(s.failed),
            if s.failed == 1 { "is" } else { "are" },
        );
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_engine::profile::IndexWork;

    fn step(index: IndexWork) -> CommitStep {
        CommitStep {
            commit: "e6b1317b7742".into(),
            parent: "p".into(),
            date: "09-02 09:14".into(),
            subject: "tcp_proxy: idle timeout".into(),
            changed_files: 3,
            dirty_packages: 2,
            affected: 375,
            tests: 12,
            wall_ms: 116,
            global: matches!(index, IndexWork::Rebuilt { .. }),
            global_reason: None,
            global_cause: None,
            engine_cold_rebuild: false,
            cold_after_failure: false,
            failed: None,
            index,
        }
    }

    /// A profile of four commits: two incremental, one global under
    /// `CAMBI-W014`, one that failed.
    fn profile_doc() -> Profile {
        use cambium_engine::{Code, Diagnostic};
        let mut global = step(IndexWork::Rebuilt {
            reason: "the root `module()` declaration changed".into(),
        });
        global.affected = 0;
        global.tests = 0;
        global.wall_ms = 10_300;
        global.global_reason = Some("the root `module()` declaration changed".into());
        global.global_cause = Some(Diagnostic {
            help: Some("expected on a release commit".into()),
            ..Diagnostic::warning(
                Code::W014ExternalsGlobal,
                "the root `module()` declaration changed",
            )
        });
        let mut failed = step(IndexWork::Failed);
        failed.commit = "a6f3b2c9d1".into();
        failed.affected = 0;
        failed.tests = 0;
        failed.failed = Some(Diagnostic::error(
            Code::E005BazelQuery,
            "bazel query for //... failed (exit 7)\n--- last stderr ---\nERROR: fetch failed",
        ));
        let mut small = step(IndexWork::Spliced { rows_rewritten: 3 });
        small.affected = 25;
        small.tests = 4;
        let steps = vec![step(IndexWork::Identical), small, global, failed];
        let cold = profile::ColdBuild {
            commit: "53f6d0dfe3aa".into(),
            wall_ms: 70_000,
            targets: 500,
            tests: 40,
        };
        let summary = profile::summarise(&steps, &cold);
        Profile {
            schema: cambium_engine::report::SCHEMA_VERSION,
            repository: "skylib".into(),
            head: "b34514a91abc".into(),
            from: "53f6d0dfe3aa".into(),
            from_failed: None,
            index: profile::IndexSummary {
                commit: "b34514a91abc".into(),
                targets: 456,
                tests: 180,
                packages: 24,
                nodes: 900,
                edges: 2000,
                external_repos: 12,
                bytes_on_disk: 46_000,
                compression: "zstd-3".into(),
                load_ms: 0,
                bazel_version: "8.4.2".into(),
            },
            cold,
            commits: steps,
            summary,
            stopped: None,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn the_report_names_the_share_the_globals_with_their_code_and_what_it_leaves_out() {
        let lines = kv(&kv_pairs(&profile_doc(), std::path::Path::new("/x/skylib")));
        let text = lines.join("\n");
        let line = |key: &str| {
            lines
                .iter()
                .find(|l| l.starts_with(key))
                .unwrap_or_else(|| panic!("no {key} line in\n{text}"))
                .clone()
        };
        assert!(
            line("measured").contains("one Bazel server kept warm"),
            "{text}"
        );
        assert!(
            line("measured").contains("a cold CI runner also pays"),
            "{text}"
        );
        // Two incremental steps, 375 and 25 affected: the p50 is 25, 5% of
        // the cold index's 500.
        assert!(line("affected").contains("25 "), "{text}");
        assert!(line("affected").contains("5% of 500"), "{text}");
        assert!(line("tests").contains("10% of 40"), "{text}");
        // The failed step is out of every figure: one global in three.
        assert!(line("global").contains("1/3"), "{text}");
        assert!(line("global").contains("33% of commits"), "{text}");
        assert!(text.contains("1 x CAMBI-W014: the root"), "{text}");
        assert!(
            text.contains("help: expected on a release commit"),
            "{text}"
        );
        assert!(
            line("rebuilds").contains("1 global at 10.3 s each"),
            "{text}"
        );
        assert!(line("failed").contains("1/4"), "{text}");
        assert!(text.contains("a6f3b2c9d1 CAMBI-E005: bazel query for //... failed (exit 7)"));
        assert!(!text.contains("last stderr"), "{text}");
        assert!(!text.contains('\u{2014}'), "no em-dashes: {text}");
    }

    #[test]
    fn a_global_row_reads_all_and_a_failed_one_reads_a_dash() {
        let doc = profile_doc();
        let render = |c: &CommitStep| {
            row::render(
                &commit_columns("commit"),
                &row_cells(c, &c.subject),
                96,
                &Paint::new(false),
            )
        };
        let global = render(&doc.commits[2]);
        assert!(global.contains("all    all"), "{global}");
        let failed = render(&doc.commits[3]);
        assert!(failed.ends_with("-      - x failed"), "{failed}");
    }

    #[test]
    fn the_header_says_how_many_commits_and_that_the_oldest_is_built_cold_first() {
        assert_eq!(
            header_note(10),
            "10 first-parent commits; the oldest is indexed cold first"
        );
        assert_eq!(header_note(0), "no commit before it; a cold index only");
    }

    #[test]
    fn the_json_document_carries_what_the_report_shows() {
        let doc = profile_doc();
        let json = serde_json::to_value(&doc).unwrap();
        let s = &json["summary"];
        for key in [
            "affected_share",
            "tests_share",
            "global_share",
            "incremental_affected",
            "incremental_tests",
            "global_rebuild",
            "requery",
            "index_spliced",
            "index_unchanged",
            "failed",
            "rebuilt_after_failure",
        ] {
            assert!(!s[key].is_null(), "summary.{key} missing");
        }
        assert_eq!(s["global_reasons"][0]["code"], "CAMBI-W014");
        assert_eq!(
            s["global_reasons"][0]["help"],
            "expected on a release commit"
        );
        assert_eq!(json["repository"], "skylib");
        assert_eq!(json["cold"]["targets"], 500);
        assert_eq!(json["commits"][2]["global_cause"]["code"], "CAMBI-W014");
        assert_eq!(
            json["commits"][2]["affected"], 0,
            "a global keeps affected 0"
        );
        assert_eq!(json["commits"][3]["failed"]["code"], "CAMBI-E005");
        assert_eq!(json["commits"][3]["index"]["kind"], "failed");
        assert_eq!(json["commits"][0]["subject"], "tcp_proxy: idle timeout");
    }

    #[test]
    fn a_row_is_the_commit_then_what_it_cost_and_what_it_did_to_the_index() {
        let cells = row_cells(
            &step(IndexWork::Spliced {
                rows_rewritten: 2625,
            }),
            "tcp_proxy: idle timeout",
        );
        let line = row::render(&commit_columns("commit"), &cells, 96, &Paint::new(false));
        assert!(
            line.starts_with("    e6b1317 09-02 09:14 tcp_proxy: idle timeout"),
            "{line}"
        );
        assert!(
            line.ends_with("116 ms      375     12 ~  2625 rewritten"),
            "{line}"
        );
    }

    #[test]
    fn a_commit_that_changed_nothing_the_graph_models_says_so_quietly() {
        let cells = row_cells(&step(IndexWork::Identical), "docs: a typo");
        let line = row::render(&commit_columns("commit"), &cells, 96, &Paint::new(false));
        assert!(line.ends_with("= unchanged"), "{line}");
        let painted = row::render(&commit_columns("commit"), &cells, 96, &Paint::new(true));
        assert!(painted.contains("\x1b[2m= unchanged\x1b[0m"), "{painted}");
    }
}
