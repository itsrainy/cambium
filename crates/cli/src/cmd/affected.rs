//! `cambi affected`.
//!
//! The answer is on stdout and everything about getting it is on stderr,
//! rendered through [`cambium_ui`] like every other verb: an `index` phase
//! that says where it looked for an index and what it found, a `splice` phase
//! for the replay up to the base (or the reason there was none), and a
//! `query` phase for the diff and the reverse walk. Stdout is the count line,
//! the labels, and the targets-by-depth histogram underneath them.

use std::io::IsTerminal as _;

use anyhow::Result;
use cambium_engine::progress::Progress;
use cambium_engine::{AffectedRequest, Code, Diagnostic};
use cambium_ui::report::{group, human_ms};
use cambium_ui::{hist, Align, Cell, Column, Event, Level, Paint, Sink, Style};
use clap::Args as ClapArgs;

use crate::cmd::{emit_json, Context};
use crate::Format;

/// How the targets-by-depth distribution is drawn under the answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Hist {
    /// A vertical column chart, each count above its own column.
    Columns,
    /// One horizontal block bar per depth, with the count after it.
    Bars,
    /// No histogram at all.
    None,
}

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
    /// How the targets-by-depth distribution is drawn under the answer.
    #[arg(long, value_enum, default_value_t = Hist::Columns)]
    pub hist: Hist,
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
    let resolved = request(&engine, &args, Shape::affected(&args))?;
    if args.dry_run {
        say_note(ctx, &resolved);
        return crate::cmd::plan::run(ctx, &engine, &resolved.req);
    }
    let outcome = narrate(ctx, &engine, &resolved, "affected")?;
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
        Format::Labels => {
            for l in labels(report, args.tests_only) {
                println!("{l}");
            }
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
                // No list and no histogram: nothing was walked, so there is
                // no distribution to draw.
            } else {
                print_answer(report, args.tests_only, args.hist);
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

/// The two revisions a run compares, and anything the resolution did that
/// the caller did not type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub base: String,
    pub head: String,
    /// Said once in the narration: `[git].default_branch` names a branch this
    /// clone has no local copy of, and its remote-tracking branch stood in.
    pub note: Option<String>,
}

/// The two revisions this run compares: the range, the old flags, or (with
/// neither) the merge base of the default branch with HEAD.
///
/// Both are checked here, before the engine sees them, so that a revision
/// this clone does not have is `CAMBI-E003` naming the spelling the caller
/// typed and the fetch that brings it in, not git's own text from a
/// `rev-list` three calls into the run. A CI checkout is where this happens:
/// `actions/checkout` makes no local branch, and a shallow fetch of a branch
/// by bare name can leave it only in `FETCH_HEAD`.
pub fn resolve_range(engine: &cambium_engine::Engine, args: &Args) -> Result<Range> {
    let repo = engine.repo();
    let (base, head) = if let Some(range) = &args.range {
        match range.split_once("..") {
            Some((base, "")) => (base.to_string(), "HEAD".to_string()),
            Some((base, head)) => (base.to_string(), head.to_string()),
            None => (range.clone(), "HEAD".to_string()),
        }
    } else {
        let head = args.head.clone().unwrap_or_else(|| "HEAD".to_string());
        match &args.base {
            Some(base) => (base.clone(), head),
            None => {
                // No range at all: what someone on a feature branch means by
                // "what did I change": the merge base of the default branch
                // with where they are.
                require_commit(repo, &head)?;
                let (branch, note) = default_base(repo, &engine.config().git.default_branch)?;
                let base = repo.merge_base(&branch, &head).map_err(|e| {
                    let why = git_detail(&e)
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    crate::diag::UsageError::raised_with_help(
                        format!("no merge base between {branch} and {head} in this clone{why}"),
                        format!(
                            "a shallow clone can stop short of it: fetch more history \
                             (`git fetch --deepen=N`, or `--unshallow`), or pass a range \
                             such as `cambi affected {branch}..{head}`"
                        ),
                    )
                })?;
                return Ok(Range { base, head, note });
            }
        }
    };
    require_commit(repo, &base)?;
    require_commit(repo, &head)?;
    Ok(Range {
        base,
        head,
        note: None,
    })
}

/// The revision `[git].default_branch` stands for in this clone: the branch
/// itself, or, when there is no local copy (what `actions/checkout` and most
/// CI checkouts leave), its remote-tracking branch, with the note that says
/// so.
fn default_base(repo: &cambium_diff::Repo, branch: &str) -> Result<(String, Option<String>)> {
    if has_commit(repo, branch) {
        return Ok((branch.to_string(), None));
    }
    let twin = remote_twin(branch, &remotes(repo));
    if let Some(t) = twin.as_deref().filter(|t| has_commit(repo, t)) {
        return Ok((
            t.to_string(),
            Some(format!(
                "no local branch {branch}; comparing from {t}, its remote-tracking branch"
            )),
        ));
    }
    Err(match twin.as_deref().and_then(|t| t.split_once('/')) {
        Some((remote, name)) => crate::diag::UsageError::raised_with_help(
            format!(
                "[git].default_branch is {branch}, and neither {branch} nor {remote}/{name} \
                 names a commit in this clone"
            ),
            format!(
                "fetch it by name, `git fetch {remote} {name}:refs/remotes/{remote}/{name} \
                 --depth=N`, or pass a range such as `cambi affected <base>..HEAD`"
            ),
        ),
        None => crate::diag::UsageError::raised_with_help(
            format!("[git].default_branch is {branch}, which names no commit in this clone"),
            "pass a range such as `cambi affected <base>..HEAD`, or set \
             [git].default_branch in cambium.toml",
        ),
    })
}

/// `Ok` when `rev` names a commit here; otherwise `CAMBI-E003` with the
/// spelling the caller typed and what brings it in.
fn require_commit(repo: &cambium_diff::Repo, rev: &str) -> Result<()> {
    if has_commit(repo, rev) {
        return Ok(());
    }
    let remotes = remotes(repo);
    let twin = remote_twin(rev, &remotes).filter(|t| has_commit(repo, t));
    Err(anyhow::Error::new(crate::diag::Raised(missing_revision(
        rev,
        &remotes,
        twin.as_deref(),
    ))))
}

fn has_commit(repo: &cambium_diff::Repo, rev: &str) -> bool {
    repo.rev_parse(rev).is_ok()
}

/// This clone's remotes, `origin` first when there is one.
fn remotes(repo: &cambium_diff::Repo) -> Vec<String> {
    let mut out: Vec<String> = repo
        .git(&["remote"])
        .map(|o| o.lines().map(str::to_string).collect())
        .unwrap_or_default();
    out.sort_by_key(|r| r != "origin");
    out
}

/// The remote-tracking branch a bare branch name would be in a clone that
/// has no local copy: `origin/<branch>`, or `<remote>/<branch>` when there is
/// one remote and it is not `origin`. Nothing for a spelling that is not a
/// bare branch name: a sha, `HEAD`, `main~2`, `origin/main` itself.
fn remote_twin(rev: &str, remotes: &[String]) -> Option<String> {
    let bare = !rev.is_empty()
        && !rev.ends_with("HEAD")
        && !looks_like_sha(rev)
        && !rev.contains(['/', '~', '^', '@', ':', '{']);
    if !bare {
        return None;
    }
    let remote = match remotes {
        [] => return None,
        [only] => only,
        many => many.iter().find(|r| *r == "origin")?,
    };
    Some(format!("{remote}/{rev}"))
}

fn looks_like_sha(rev: &str) -> bool {
    (7..=64).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `CAMBI-E003` for a revision this clone does not have. `twin` is its
/// remote-tracking branch when that one is here.
fn missing_revision(rev: &str, remotes: &[String], twin: Option<&str>) -> Diagnostic {
    let message = format!("`{rev}` does not name a commit in this clone");
    let tracking = rev
        .split_once('/')
        .filter(|(remote, name)| !name.is_empty() && remotes.iter().any(|r| r == remote));
    let help = if let Some(t) = twin {
        format!(
            "there is no local `{rev}` (a CI checkout usually makes none); its \
             remote-tracking branch `{t}` is here, so pass that"
        )
    } else if let Some((remote, name)) = tracking {
        format!(
            "fetch it by name: `git fetch {remote} {name}:refs/remotes/{remote}/{name} \
             --depth=N` (in a shallow clone, `git fetch {remote} {name}` can leave it only in \
             FETCH_HEAD)"
        )
    } else if let (true, Some(remote)) = (looks_like_sha(rev), remotes.first()) {
        format!(
            "fetch it: `git fetch {remote} {rev} --depth=N`, or deepen a shallow clone with \
             `git fetch --deepen=N`"
        )
    } else {
        format!("check the spelling: `git rev-parse --verify {rev}` names it when it is here")
    };
    Diagnostic::error(Code::E003Git, message).with_help(help)
}

/// The first line of what git said, when it said anything.
fn git_detail(e: &cambium_diff::GitError) -> Option<String> {
    match e {
        cambium_diff::GitError::Failed { stderr, .. } => stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string),
        _ => None,
    }
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
pub fn request(engine: &cambium_engine::Engine, args: &Args, shape: Shape) -> Result<Resolved> {
    let Range { base, head, note } = resolve_range(engine, args)?;
    let req = AffectedRequest {
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
    };
    Ok(Resolved { req, note })
}

/// A request, and the note [`resolve_range`] left for the narration.
pub struct Resolved {
    pub req: AffectedRequest,
    pub note: Option<String>,
}

/// The resolution's note on its own, for `--dry-run`, which has no narration
/// to carry it.
pub fn say_note(ctx: &Context, resolved: &Resolved) {
    if let Some(note) = &resolved.note {
        let mut sink = cambium_ui::boxed(ctx.ui);
        sink.emit(Event::Log(Level::Info, note.clone()));
        sink.emit(Event::Done);
    }
}

/// Runs `affected`, narrating it on stderr through the shared renderer.
///
/// `explain` calls this too: it is the same computation, so it is the same
/// three phases, and the two verbs cannot drift apart about what a run is
/// doing while it does it.
pub fn narrate(
    ctx: &Context,
    engine: &cambium_engine::Engine,
    resolved: &Resolved,
    verb: &str,
) -> Result<cambium_engine::RunOutcome> {
    let req = &resolved.req;
    let mut sink = cambium_ui::boxed(ctx.ui);
    let repo = engine
        .workspace()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    sink.emit(Event::Header {
        verb: verb.to_string(),
        repo,
        range: format!("{}..{}", short(&req.base), short(&req.head)),
        note: commits_in_range(engine, req),
    });
    if let Some(note) = &resolved.note {
        sink.emit(Event::Log(Level::Info, note.clone()));
    }
    let mut render = Narrator {
        sink: sink.as_mut(),
        rows_open: false,
    };
    let out = engine.affected_narrated(req, &mut |p| render.on(p));
    sink.emit(Event::Done);
    Ok(out?)
}

/// `N commits`, or nothing to say when git will not count them (a synthetic
/// revision, a shallow clone).
fn commits_in_range(engine: &cambium_engine::Engine, req: &AffectedRequest) -> String {
    let range = format!("{}..{}", req.base, req.head);
    let Ok(out) = engine.repo().git(&["rev-list", "--count", &range]) else {
        return String::new();
    };
    match out.trim().parse::<u64>() {
        Ok(n) => format!("{} commit{}", group(n), if n == 1 { "" } else { "s" }),
        Err(_) => String::new(),
    }
}

/// The columns of the splice row, the same five a profile row uses so the two
/// verbs lay a commit out alike.
fn splice_columns() -> Vec<Column> {
    vec![
        Column::flex("range"),
        Column::fixed("time", 9, Align::Right),
        Column::fixed("affected", 8, Align::Right),
        Column::fixed("tests", 6, Align::Right),
        Column::fixed("index", 17, Align::Left),
    ]
}

/// Turns the run's progress into the grammar the sink draws.
struct Narrator<'a> {
    sink: &'a mut dyn Sink,
    /// The splice phase has printed its row header.
    rows_open: bool,
}

impl Narrator<'_> {
    fn on(&mut self, p: Progress) {
        match p {
            Progress::IndexLookup { base } => {
                self.sink.emit(Event::PhaseStart {
                    name: "index".into(),
                    detail: Some(format!("looking for {}", short(&base))),
                });
                self.sink
                    .emit(Event::Current(format!("looking for {}", short(&base))));
            }
            Progress::SearchStart { base, ancestors } => {
                // The live line is about to count these off; a pipe gets the
                // one line that says the search happened at all.
                self.sink.emit(Event::Milestone(format!(
                    "none at {}, searching {ancestors} ancestors",
                    short(&base)
                )));
                self.sink.emit(Event::Current(format!(
                    "none at {}, searching {ancestors} ancestors",
                    short(&base)
                )));
            }
            Progress::AncestorChecked { checked, of, .. } => {
                // Live only: `Current` is dropped by every sink without a
                // live line, which is exactly what a CI log wants.
                self.sink.emit(Event::Current(format!(
                    "searching {of} ancestors … {checked} checked"
                )));
            }
            Progress::IndexFound {
                sha,
                behind,
                lineage,
                age_secs,
                elapsed_ms,
            } => {
                let mut about = vec![format!("lineage {}", short(&lineage))];
                if behind > 0 {
                    about.insert(0, format!("{} behind", plural_hops(behind)));
                }
                if let Some(age) = age_secs {
                    about.push(format!("{} old", human_age(age)));
                }
                self.sink.emit(Event::PhaseEnd {
                    name: "index".into(),
                    result: format!("found at {}  ({})", short(&sha), about.join(", ")),
                    elapsed_ms,
                });
            }
            Progress::IndexAbsent {
                searched,
                elapsed_ms,
            } => self.sink.emit(Event::PhaseEnd {
                name: "index".into(),
                result: format!("none within {searched} ancestors, building cold"),
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
            Progress::SpliceStart { from, to, commits } => {
                self.sink.emit(Event::PhaseStart {
                    name: "splice".into(),
                    detail: Some(format!(
                        "{commits} commit{}  {}..{}",
                        if commits == 1 { "" } else { "s" },
                        short(&from),
                        short(&to)
                    )),
                });
                self.sink.emit(Event::Current(format!(
                    "replaying {}..{}",
                    short(&from),
                    short(&to)
                )));
            }
            Progress::SpliceStep { step } => {
                if !self.rows_open {
                    self.rows_open = true;
                    self.sink.emit(Event::RowHeader(splice_columns()));
                }
                self.sink.emit(Event::Row(vec![
                    Cell::Commit {
                        sha: step.commit.clone(),
                        date: step.date.clone(),
                        subject: format!(
                            "{} file{}, {} package{}",
                            step.changed_files,
                            if step.changed_files == 1 { "" } else { "s" },
                            step.dirty_packages,
                            if step.dirty_packages == 1 { "" } else { "s" },
                        ),
                    },
                    Cell::text(human_ms(step.wall_ms)),
                    Cell::text(group(step.affected as u64)),
                    Cell::text(group(step.tests as u64)),
                    index_cell(&step.index),
                ]));
            }
            Progress::SpliceDone {
                at,
                rows_rewritten,
                elapsed_ms,
            } => self.sink.emit(Event::PhaseEnd {
                name: "splice".into(),
                result: format!(
                    "{} row{} rewritten, index now at {}",
                    group(rows_rewritten as u64),
                    if rows_rewritten == 1 { "" } else { "s" },
                    short(&at)
                ),
                elapsed_ms,
            }),
            Progress::SpliceSkipped { reason } => self.sink.emit(Event::PhaseSkipped {
                name: "splice".into(),
                reason,
            }),
            Progress::QueryStart => {
                self.sink.emit(Event::PhaseStart {
                    name: "query".into(),
                    detail: Some("reading the diff".into()),
                });
                self.sink
                    .emit(Event::Current("walking reverse dependencies".into()));
            }
            Progress::QueryDone {
                changed_files,
                packages,
                affected,
                tests,
                elapsed_ms,
            } => self.sink.emit(Event::PhaseEnd {
                name: "query".into(),
                result: format!(
                    "{} changed file{}, {} package{} \u{2192} {} affected ({} tests)",
                    changed_files,
                    if changed_files == 1 { "" } else { "s" },
                    packages,
                    if packages == 1 { "" } else { "s" },
                    group(affected as u64),
                    group(tests as u64),
                ),
                elapsed_ms,
            }),
            Progress::Global { reason } => {
                self.sink.emit(Event::Log(
                    Level::Warning,
                    format!("everything is affected: {reason}"),
                ));
                self.sink.emit(Event::PhaseEnd {
                    name: "query".into(),
                    result: "the whole repository".into(),
                    elapsed_ms: 0,
                });
            }
        }
    }
}

/// What the splice did to the index, in the same cell `cambi profile` prints.
fn index_cell(work: &cambium_engine::profile::IndexWork) -> Cell {
    use cambium_engine::profile::IndexWork;
    match work {
        IndexWork::Identical => Cell::styled("= unchanged", Style::Dim),
        IndexWork::Spliced { rows_rewritten } => {
            Cell::styled(format!("~ {rows_rewritten:>5} rewritten"), Style::Cyan)
        }
        IndexWork::Rebuilt { .. } => Cell::styled("\u{21bb} rebuilt", Style::Cyan),
    }
}

/// An age in the coarsest unit that still says something: `40 min`, `4 h`,
/// `2 d`. An index is minutes to weeks old and nobody reads the seconds.
fn human_age(secs: u64) -> String {
    match secs {
        0..=119 => format!("{secs} s"),
        120..=7_199 => format!("{} min", secs / 60),
        7_200..=172_799 => format!("{} h", secs / 3_600),
        _ => format!("{} d", secs / 86_400),
    }
}

fn plural_hops(n: u32) -> String {
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// The answer as `--format labels` prints it, one per line: what `bazel test
/// --target_pattern_file` and `bazel build --target_pattern_file` read.
///
/// The same labels `--format query` wraps in `set(...)` and the human form
/// lists under its count line, in the same order, and nothing else: an empty
/// answer is no lines at all, so a caller tests for an empty file rather than
/// parsing a header. A global answer is the one pattern `//...`, as it is on
/// every other stdout, with exit 2.
fn labels(report: &cambium_engine::AffectedReport, tests_only: bool) -> Vec<&str> {
    if report.global {
        return vec!["//..."];
    }
    if tests_only {
        report.tests.iter().map(String::as_str).collect()
    } else {
        report.affected.iter().map(|t| t.label.as_str()).collect()
    }
}

/// Stdout: the count line, the labels, and the distribution underneath.
fn print_answer(report: &cambium_engine::AffectedReport, tests_only: bool, hist_kind: Hist) {
    let items = labels(report, tests_only);
    let paint = Paint::new(std::io::stdout().is_terminal());
    let depth = match (report.max_depth(), report.max_package_depth()) {
        (Some(d), Some(p)) => format!("  max depth {d}  ({p} package hops)"),
        (Some(d), None) => format!("  max depth {d}"),
        _ => String::new(),
    };
    println!(
        "{}  ({} tests){}",
        paint.bold(&format!(
            "{} affected target{}",
            group(items.len() as u64),
            if items.len() == 1 { "" } else { "s" }
        )),
        group(report.tests.len() as u64),
        depth,
    );
    println!();
    for l in &items {
        println!("{l}");
    }
    if hist_kind == Hist::None {
        return;
    }
    // `--tests-only` prints a list of tests, so its histograms are the tests'
    // distances and not the whole answer's: a distribution that did not match
    // the list above it would be worse than none. Two charts, because the
    // two distances have different axes: a target nine hops down one package
    // is one package hop away.
    let by_depth = depth_counts(report, tests_only);
    draw_hist(&paint, hist_kind, &by_depth, "targets by depth");
    let by_package = package_depth_counts(report, tests_only);
    draw_hist(&paint, hist_kind, &by_package, "targets by package hops");
}

/// One chart with its caption under it, or nothing when there is nothing in
/// it.
fn draw_hist(paint: &Paint, kind: Hist, counts: &[u64], what: &str) {
    if counts.is_empty() {
        return;
    }
    let labels: Vec<String> = (0..counts.len()).map(|d| d.to_string()).collect();
    println!();
    match kind {
        Hist::Bars => {
            for line in hist::bars(counts, &labels, 30) {
                println!("{line}");
            }
        }
        Hist::Columns => {
            for line in hist::columns(counts, &labels) {
                println!("{line}");
            }
        }
        Hist::None => unreachable!(),
    }
    println!(
        "  {}",
        paint.dim(&format!("{what} 0..{}", counts.len().saturating_sub(1)))
    );
}

/// The distribution drawn under the list: the report's own histogram, or the
/// tests' depths counted the same way when the list is tests alone.
fn depth_counts(report: &cambium_engine::AffectedReport, tests_only: bool) -> Vec<u64> {
    counts(report, tests_only, &report.depth_histogram, |t| t.depth)
}

/// The same for package hops.
fn package_depth_counts(report: &cambium_engine::AffectedReport, tests_only: bool) -> Vec<u64> {
    counts(report, tests_only, &report.package_depth_histogram, |t| {
        t.package_depth
    })
}

fn counts(
    report: &cambium_engine::AffectedReport,
    tests_only: bool,
    whole: &[u32],
    of: impl Fn(&cambium_engine::report::AffectedTarget) -> u32,
) -> Vec<u64> {
    if !tests_only {
        return whole.iter().map(|&c| c as u64).collect();
    }
    let tests = || report.affected.iter().filter(|t| t.test);
    let Some(max) = tests().map(&of).max() else {
        return Vec::new();
    };
    let mut out = vec![0u64; max as usize + 1];
    for t in tests() {
        out[of(t) as usize] += 1;
    }
    out
}

/// The path back to the change with its two lengths after it: hops, and how
/// many of those hops left a package. `//a:x <- //a:y <- //b:z  (2 hops, 1
/// package boundary)`. A direct target is its own path and gets no lengths.
pub fn path_line(path: &[String]) -> String {
    let hops = path.len().saturating_sub(1);
    if hops == 0 {
        return path.join(" <- ");
    }
    let boundaries = path
        .windows(2)
        .filter(|w| package_of(&w[0]) != package_of(&w[1]))
        .count();
    format!(
        "{}  ({} {}, {} package {})",
        path.join(" <- "),
        hops,
        if hops == 1 { "hop" } else { "hops" },
        boundaries,
        if boundaries == 1 {
            "boundary"
        } else {
            "boundaries"
        }
    )
}

fn package_of(label: &str) -> &str {
    match label.rfind(':') {
        Some(i) => &label[..i],
        None => label,
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
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
            path_line(&e.path),
            e.reason.describe()
        );
    }
}

#[cfg(test)]
mod tests {
    use cambium_engine::profile::{CommitStep, IndexWork};
    use cambium_engine::report::{AffectedReport, AffectedTarget};
    use cambium_ui::{ColorChoice, Kind, OutputMode};
    use clap::Parser;

    /// A sink writing into a buffer this test can read back.
    #[derive(Clone)]
    struct Buf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn plain_mode() -> OutputMode {
        let _ = ColorChoice::Never;
        OutputMode {
            kind: Kind::Plain,
            live: false,
            colour: false,
            width: 96,
        }
    }

    /// Drives the narrator over a scripted run and returns what stderr saw.
    fn narrate_to_text(events: Vec<super::Progress>) -> String {
        let buf = Buf(Default::default());
        let mut sink = cambium_ui::Plain::to_writer(buf.clone(), plain_mode());
        {
            let mut n = super::Narrator {
                sink: &mut sink,
                rows_open: false,
            };
            for e in events {
                n.on(e);
            }
        }
        let bytes = buf.0.borrow().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn the_index_phase_says_how_far_back_it_found_one_and_how_old_it_is() {
        let text = narrate_to_text(vec![
            super::Progress::IndexLookup {
                base: "3f2a9c1abc".into(),
            },
            super::Progress::SearchStart {
                base: "3f2a9c1abc".into(),
                ancestors: 50,
            },
            super::Progress::AncestorChecked {
                checked: 1,
                of: 50,
                sha: "aaaa".into(),
            },
            super::Progress::IndexFound {
                sha: "dbaed12def".into(),
                behind: 24,
                lineage: "0123456789abcdef".into(),
                age_secs: Some(2 * 86_400),
                elapsed_ms: 31,
            },
        ]);
        assert!(text.contains("found at dbaed12def"), "{text}");
        assert!(text.contains("24 commits behind"), "{text}");
        assert!(text.contains("2 d old"), "{text}");
        // The per-ancestor ticking is the live line's; a pipe gets the one
        // milestone instead.
        assert!(
            text.contains("none at 3f2a9c1abc, searching 50 ancestors"),
            "{text}"
        );
        assert!(!text.contains("1 checked"), "{text}");
    }

    #[test]
    fn a_skipped_splice_says_why_and_the_query_says_what_it_read() {
        let text = narrate_to_text(vec![
            super::Progress::SpliceSkipped {
                reason: "index already at base".into(),
            },
            super::Progress::QueryStart,
            super::Progress::QueryDone {
                changed_files: 12,
                packages: 4,
                affected: 1_284,
                tests: 96,
                elapsed_ms: 40,
            },
        ]);
        assert!(text.contains("skipped (index already at base)"), "{text}");
        assert!(
            text.contains("12 changed files, 4 packages")
                && text.contains("1,284 affected (96 tests)"),
            "{text}"
        );
    }

    #[test]
    fn the_splice_row_is_the_replay_and_what_it_rewrote() {
        let text = narrate_to_text(vec![
            super::Progress::SpliceStart {
                from: "dbaed12def".into(),
                to: "3f2a9c1abc".into(),
                commits: 24,
            },
            super::Progress::SpliceStep {
                step: Box::new(CommitStep {
                    commit: "3f2a9c1abc".into(),
                    parent: "dbaed12def".into(),
                    date: "09-02 09:14".into(),
                    changed_files: 312,
                    dirty_packages: 41,
                    affected: 2_100,
                    tests: 180,
                    wall_ms: 1_800,
                    global: false,
                    global_reason: None,
                    engine_cold_rebuild: false,
                    index: IndexWork::Spliced {
                        rows_rewritten: 2_625,
                    },
                }),
            },
            super::Progress::SpliceDone {
                at: "3f2a9c1abc".into(),
                rows_rewritten: 2_625,
                elapsed_ms: 1_800,
            },
        ]);
        assert!(text.contains("312 files, 41 packages"), "{text}");
        assert!(text.contains("~  2625 rewritten"), "{text}");
        assert!(
            text.contains("2,625 rows rewritten, index now at 3f2a9c1abc"),
            "{text}"
        );
    }

    /// Every target sits at `depth` hops and half as many package hops,
    /// rounded down, as if each package were two targets deep.
    fn report_with_depths(depths: &[(u32, bool)]) -> AffectedReport {
        let mut r = AffectedReport {
            affected: depths
                .iter()
                .enumerate()
                .map(|(i, &(depth, test))| AffectedTarget {
                    label: format!("//pkg{i}:t"),
                    depth,
                    package_depth: depth / 2,
                    test,
                })
                .collect(),
            ..AffectedReport::default()
        };
        let hist = |of: fn(&AffectedTarget) -> u32| {
            let max = r.affected.iter().map(of).max().unwrap();
            let mut h = vec![0u32; max as usize + 1];
            for t in &r.affected {
                h[of(t) as usize] += 1;
            }
            h
        };
        r.depth_histogram = hist(|t| t.depth);
        r.package_depth_histogram = hist(|t| t.package_depth);
        r.tests = r
            .affected
            .iter()
            .filter(|t| t.test)
            .map(|t| t.label.clone())
            .collect();
        r
    }

    #[test]
    fn the_max_depth_line_reads_off_the_histogram() {
        let r = report_with_depths(&[(0, false), (1, true), (3, false)]);
        assert_eq!(r.max_depth(), Some(3), "the deepest bin is the max depth");
        assert_eq!(super::depth_counts(&r, false), vec![1, 1, 0, 1]);
        assert_eq!(r.max_package_depth(), Some(1));
        assert_eq!(super::package_depth_counts(&r, false), vec![2, 1]);
    }

    #[test]
    fn the_path_line_counts_hops_and_the_package_boundaries_among_them() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::path_line(&p(&["//a:x"])), "//a:x");
        assert_eq!(
            super::path_line(&p(&["//a:x", "//a:y", "//b:z"])),
            "//a:x <- //a:y <- //b:z  (2 hops, 1 package boundary)"
        );
        assert_eq!(
            super::path_line(&p(&["//a:x", "//b:y"])),
            "//a:x <- //b:y  (1 hop, 1 package boundary)"
        );
        assert_eq!(
            super::path_line(&p(&["//a:x", "//a:y"])),
            "//a:x <- //a:y  (1 hop, 0 package boundaries)"
        );
    }

    /// `--tests-only` lists tests, so the distribution under the list has to
    /// be the tests' depths and not the whole answer's.
    #[test]
    fn tests_only_draws_the_tests_own_depths() {
        let r = report_with_depths(&[(0, false), (1, true), (1, true), (3, false)]);
        assert_eq!(super::depth_counts(&r, false), vec![1, 2, 0, 1]);
        assert_eq!(super::depth_counts(&r, true), vec![0, 2]);
        assert_eq!(super::package_depth_counts(&r, false), vec![3, 1]);
        assert_eq!(super::package_depth_counts(&r, true), vec![2]);
    }

    /// `--format labels`: the labels and nothing else, the tests alone under
    /// `--tests-only`, no lines for an empty answer, `//...` for a global one.
    #[test]
    fn labels_are_the_answer_and_nothing_else() {
        let r = report_with_depths(&[(0, false), (1, true), (3, true)]);
        assert_eq!(
            super::labels(&r, false),
            vec!["//pkg0:t", "//pkg1:t", "//pkg2:t"]
        );
        assert_eq!(super::labels(&r, true), vec!["//pkg1:t", "//pkg2:t"]);
        assert!(super::labels(&AffectedReport::default(), false).is_empty());
        let global = AffectedReport {
            global: true,
            ..AffectedReport::default()
        };
        assert_eq!(super::labels(&global, true), vec!["//..."]);
    }

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

    fn remotes(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bare_branch_name_has_a_remote_tracking_twin_and_nothing_else_does() {
        let origin = remotes(&["origin"]);
        assert_eq!(
            super::remote_twin("main", &origin).as_deref(),
            Some("origin/main")
        );
        for not_bare in [
            "origin/main",
            "HEAD",
            "FETCH_HEAD",
            "main~2",
            "main^",
            "0a4a001",
        ] {
            assert_eq!(super::remote_twin(not_bare, &origin), None, "{not_bare}");
        }
        assert_eq!(super::remote_twin("main", &[]), None, "no remotes, no twin");
        assert_eq!(
            super::remote_twin("main", &remotes(&["upstream"])).as_deref(),
            Some("upstream/main"),
            "a single remote is the one"
        );
        assert_eq!(
            super::remote_twin("main", &remotes(&["upstream", "fork"])),
            None,
            "two remotes and no origin: nothing to guess"
        );
    }

    /// The shallow CI clone: `origin/main` was typed and never fetched into
    /// its ref. The help names that spelling and the refspec that makes it,
    /// and never the `--base`/`--head` flags nobody passed.
    #[test]
    fn a_missing_remote_tracking_branch_is_fetched_by_name() {
        let d = super::missing_revision("origin/main", &remotes(&["origin"]), None);
        assert_eq!(d.code, cambium_engine::Code::E003Git);
        assert!(d.message.contains("`origin/main`"), "{}", d.message);
        let help = d.help.expect("a help line");
        assert!(
            help.contains("git fetch origin main:refs/remotes/origin/main --depth=N"),
            "{help}"
        );
        assert!(
            !help.contains("--base") && !help.contains("--head"),
            "{help}"
        );
    }

    #[test]
    fn a_missing_local_branch_points_at_its_remote_tracking_twin() {
        let d = super::missing_revision("main", &remotes(&["origin"]), Some("origin/main"));
        let help = d.help.expect("a help line");
        assert!(help.contains("`origin/main`"), "{help}");
    }

    #[test]
    fn a_missing_sha_is_fetched_or_deepened() {
        let d = super::missing_revision("0a4a001", &remotes(&["origin"]), None);
        let help = d.help.expect("a help line");
        assert!(
            help.contains("git fetch origin 0a4a001 --depth=N"),
            "{help}"
        );
        assert!(help.contains("--deepen"), "{help}");
        let d = super::missing_revision("0a4a001", &[], None);
        assert!(d.help.expect("a help line").contains("check the spelling"));
    }

    #[test]
    fn a_three_dot_range_says_what_to_type_instead() {
        let err = super::check_usage(&parse(&["a...b"])).expect_err("a usage error");
        assert!(err.to_string().contains("two-dot range"), "{err}");
    }
}
