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

use std::collections::HashMap;
use std::io::IsTerminal as _;

use anyhow::{Context as _, Result};
use cambium_engine::profile::{self, CommitStep, IndexWork, Progress, WorktreeRevisions};
use cambium_engine::report::Stopwatch;
use cambium_engine::Profile;
use cambium_ui::report::{group, human_bytes, human_ms, kv, Pair};
use cambium_ui::{row, Align, Cell, Column, Event, Paint, Sink, Style};
use clap::Args as ClapArgs;

use crate::cmd::{emit, Context};

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
    /// The revision the walk ends at. Defaults to HEAD.
    rev: Option<String>,
    /// How many first-parent commits back to walk.
    #[arg(long, default_value_t = 30)]
    commits: usize,
    /// Keep the index the walk ends with here, so `cambi index publish` can
    /// upload it. By default it is written to a scratch file and deleted.
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// How the report above the table reads.
    #[arg(long, value_enum, default_value_t = Report::Kv)]
    report: Report,
}

/// The columns of a commit row, live and in the final table alike, so the two
/// cannot drift.
fn columns() -> Vec<Column> {
    vec![
        Column::flex("commit"),
        Column::fixed("time", 9, Align::Right),
        Column::fixed("affected", 8, Align::Right),
        Column::fixed("tests", 6, Align::Right),
        Column::fixed("index", 17, Align::Left),
    ]
}

/// The `index` cell: what the run did to the stored index for this commit.
/// Dim when nothing happened, so it adds no noise.
fn index_cell(work: &IndexWork) -> Cell {
    match work {
        IndexWork::Identical => Cell::styled("= unchanged", Style::Dim),
        IndexWork::Spliced { rows_rewritten } => {
            Cell::styled(format!("~ {rows_rewritten:>5} rewritten"), Style::Cyan)
        }
        IndexWork::Rebuilt { .. } => Cell::styled("\u{21bb} rebuilt", Style::Cyan),
    }
}

fn row_cells(step: &CommitStep, subject: &str) -> Vec<Cell> {
    vec![
        Cell::Commit {
            sha: step.commit.clone(),
            date: step.date.clone(),
            subject: subject.to_string(),
        },
        Cell::text(human_ms(step.wall_ms)),
        Cell::text(group(step.affected as u64)),
        Cell::text(group(step.tests as u64)),
        index_cell(&step.index),
    ]
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let engine = ctx.engine()?;
    let rev = args.rev.as_deref().unwrap_or("HEAD");
    let head = engine
        .repo()
        .rev_parse(rev)
        .with_context(|| format!("{rev} is not a revision in this repository"))?;
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
    let log = engine.repo().first_parent_log(&head, args.commits + 1)?;
    let from = log.first().cloned().unwrap_or_else(|| head.clone());
    sink.emit(Event::Header {
        verb: "profile".into(),
        repo: repo_name,
        range: format!("{}..{}", short(&from), short(&head)),
        note: format!(
            "{} first-parent commit{}",
            log.len().saturating_sub(1),
            plural(log.len().saturating_sub(1))
        ),
    });

    // The worktree outlives one `profile` call, so the CLI creates it and
    // reports it in the same vocabulary the walk uses for its own phases.
    let mut render = Renderer {
        sink: sink.as_mut(),
        subjects: HashMap::new(),
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

    let doc = profile::profile(&engine, &revs, &head, args.commits, &out, &mut |p| {
        render.on(p)
    });
    let subjects = std::mem::take(&mut render.subjects);
    sink.emit(Event::Done);
    drop(revs);
    let doc = doc?;

    for d in &doc.diagnostics {
        ctx.render.warn(d);
    }
    emit(&doc, ctx.format.data("cambi profile")?, |p| {
        print_human(
            p,
            &subjects,
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
    /// Commit subjects, kept for the final table: only the progress stream
    /// carries them, and the document does not.
    subjects: HashMap<String, String>,
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
                result: format!("{} targets", group(targets as u64)),
                elapsed_ms,
            }),
            Progress::Step { index, of, commit } => {
                if !self.opened {
                    self.opened = true;
                    self.sink.emit(Event::PhaseStart {
                        name: "commits".into(),
                        detail: Some(format!("{of} step{}", plural(of))),
                    });
                    self.sink.emit(Event::RowHeader(columns()));
                }
                self.sink
                    .emit(Event::Current(format!("[{index}/{of}] {}", short(&commit))));
            }
            Progress::Rebuild {
                index,
                commit,
                reason,
            } => {
                self.sink.emit(Event::Log(
                    cambium_ui::Level::Warning,
                    format!(
                        "{} invalidated everything ({reason}); rebuilding",
                        short(&commit)
                    ),
                ));
                self.sink.emit(Event::Current(format!(
                    "[{index}] rebuilding at {}",
                    short(&commit)
                )));
            }
            Progress::Stepped { step, subject, .. } => {
                self.sink.emit(Event::Row(row_cells(&step, &subject)));
                self.subjects.insert(step.commit.clone(), subject);
            }
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

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn print_human(
    p: &Profile,
    subjects: &HashMap<String, String>,
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
    println!("{}", row::render_header(&columns(), width, &paint));
    for c in &p.commits {
        let subject = subjects.get(&c.commit).map(String::as_str).unwrap_or("");
        println!(
            "{}",
            row::render(&columns(), &row_cells(c, subject), width, &paint)
        );
    }
    if let Some(kept) = kept {
        println!();
        println!(
            "The index for {} is at {}; `cambi index publish {}` uploads it.",
            short(&p.head),
            kept.display(),
            kept.display()
        );
    }
}

/// The aligned block: every number the paragraphs quote, in the order someone
/// reads them.
fn kv_pairs(p: &Profile, workspace: &std::path::Path) -> Vec<Pair> {
    let i = &p.index;
    let s = &p.summary;
    let inc = &s.incremental;
    let spliced = p
        .commits
        .iter()
        .filter(|c| matches!(c.index, IndexWork::Spliced { .. }))
        .count();
    let identical = p
        .commits
        .iter()
        .filter(|c| matches!(c.index, IndexWork::Identical))
        .count();
    let mut pairs = vec![
        Pair::new(
            "repository",
            workspace
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| workspace.display().to_string()),
            format!(
                "at {}, last {} first-parent commit{}",
                short(&p.head),
                s.commits,
                plural(s.commits)
            ),
        ),
        Pair::new(
            "targets",
            group(i.targets as u64),
            format!(
                "{} tests, {} packages, {} external repositor{}",
                group(i.tests as u64),
                group(i.packages as u64),
                group(i.external_repos as u64),
                if i.external_repos == 1 { "y" } else { "ies" },
            ),
        ),
        Pair::new(
            "full index",
            human_ms(p.cold.wall_ms),
            format!(
                "{} on disk ({}), loads in {}",
                human_bytes(i.bytes_on_disk),
                i.compression,
                human_ms(i.load_ms)
            ),
        ),
    ];
    if s.commits == 0 {
        return pairs;
    }
    pairs.push(Pair::new(
        "per commit",
        human_ms(inc.mean_ms),
        if inc.commits == 0 {
            "every commit invalidated the whole graph".to_string()
        } else {
            format!(
                "mean of {} incremental, p50 {}, p90 {}, max {}",
                inc.commits,
                human_ms(inc.p50_ms),
                human_ms(inc.p90_ms),
                human_ms(inc.max_ms),
            )
        },
    ));
    pairs.push(Pair::new(
        "affected",
        format!("{:.0}", s.mean_affected),
        "targets on average".to_string(),
    ));
    pairs.push(Pair::new(
        "index work",
        format!("{spliced}/{}", s.commits),
        format!("spliced, {identical} changed nothing the graph models"),
    ));
    pairs.push(Pair::new(
        "rebuilds",
        format!("{}/{}", s.globals, s.commits),
        match s.rebuild_every_commits {
            None => "the whole graph was never rebuilt".to_string(),
            Some(every) => format!(
                "one every {every:.1} commits, at {} each ({})",
                human_ms(s.rebuild.mean_ms),
                s.global_reasons
                    .iter()
                    .map(|r| format!("{} x {}", r.count, r.reason))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        },
    ));
    if s.engine_cold_rebuilds > 0 {
        pairs.push(Pair::new(
            "re-queried",
            format!("{}/{}", s.engine_cold_rebuilds, s.commits),
            "over limits.max_dirty_packages: exact, at a cold build's cost".to_string(),
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
            "Based on the last {} commit{}, every one invalidated the whole graph, so there is \
             no incremental time to report.",
            s.commits,
            plural(s.commits),
        );
    } else {
        println!(
            "Based on the last {} commit{}, cambium takes {} on average to calculate the changed \
             targets for a commit (p50 {}, p90 {}, max {} over the {} commit{} answered \
             incrementally), answering with {:.0} targets on average.",
            s.commits,
            plural(s.commits),
            human_ms(inc.mean_ms),
            human_ms(inc.p50_ms),
            human_ms(inc.p90_ms),
            human_ms(inc.max_ms),
            inc.commits,
            plural(inc.commits),
            s.mean_affected,
        );
    }
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
                .map(|r| format!("{} x {}", r.count, r.reason))
                .collect();
            println!(
                "The entire graph had to be rebuilt {} time{}, once every {:.1} commits on \
                 average, at {} per rebuild ({}).",
                s.globals,
                plural(s.globals),
                every,
                human_ms(s.rebuild.mean_ms),
                reasons.join("; "),
            );
        }
    }
    if s.engine_cold_rebuilds > 0 {
        println!(
            "{} commit{} exceeded limits.max_dirty_packages and were answered by re-querying \
             the whole repository: exact, at a cold build's cost.",
            s.engine_cold_rebuilds,
            plural(s.engine_cold_rebuilds)
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

    fn step(index: IndexWork) -> CommitStep {
        CommitStep {
            commit: "e6b1317b7742".into(),
            parent: "p".into(),
            date: "09-02 09:14".into(),
            changed_files: 3,
            dirty_packages: 2,
            affected: 375,
            tests: 12,
            wall_ms: 116,
            global: matches!(index, IndexWork::Rebuilt { .. }),
            global_reason: None,
            engine_cold_rebuild: false,
            index,
        }
    }

    #[test]
    fn a_row_is_the_commit_then_what_it_cost_and_what_it_did_to_the_index() {
        let cells = row_cells(
            &step(IndexWork::Spliced {
                rows_rewritten: 2625,
            }),
            "tcp_proxy: idle timeout",
        );
        let line = row::render(&columns(), &cells, 96, &Paint::new(false));
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
        let line = row::render(&columns(), &cells, 96, &Paint::new(false));
        assert!(line.ends_with("= unchanged"), "{line}");
        let painted = row::render(&columns(), &cells, 96, &Paint::new(true));
        assert!(painted.contains("\x1b[2m= unchanged\x1b[0m"), "{painted}");
    }
}
