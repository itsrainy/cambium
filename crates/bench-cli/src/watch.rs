//! `cambium-bench watch`: the live matrix (issue #1).
//!
//! The screen is one row per subject, polled off the files the harness writes
//! — there is no IPC, no socket and no log scraping, which is what lets the
//! same view serve a run in progress, a finished run, and a `bench-results/`
//! copied off the benchmark box with `rsync`.
//!
//! What to show is [`cambium_bench::watch`], which is pure and unit-tested
//! against fixture result files. This module is only the terminal: the poll
//! loop, the keys, and `--once`, which prints the very same lines.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use cambium_bench::watch::{render, render_change_sets, snapshot_with, Snapshot};
use clap::Args as ClapArgs;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::{Context, DataFormat};

/// How many header lines [`render`] puts above the first subject row. The TUI
/// needs it to know which drawn line a selected subject is.
const HEADER_LINES: usize = 3;

#[derive(ClapArgs, Clone, Debug)]
pub struct Args {
    /// Where the results are: the directory holding one sub-directory per
    /// subject.
    #[arg(long, default_value = "bench-results")]
    pub results_dir: PathBuf,
    /// Watch only these subjects, by id. Repeatable; the default is every
    /// subject directory under --results-dir.
    #[arg(long = "subject", value_name = "S")]
    pub subjects: Vec<String>,
    /// Print the table once and exit, instead of opening the screen.
    #[arg(long)]
    pub once: bool,
    /// Seconds between polls.
    #[arg(long, default_value_t = 1.0, value_name = "SECS")]
    pub interval: f64,
}

/// Extra behaviour the screen needs when it is driving child processes: the
/// launcher's footer, and its say in what `q` means. Phase A watches a run it
/// did not start and uses the default of every method.
pub trait Supervisor {
    /// Lines under the table: the running child's stderr tail, a prompt.
    fn footer(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// Called once per poll, so a supervisor can reap a finished child and
    /// start the next one.
    fn tick(&mut self) {}

    /// What `q` does. `false` keeps the screen up — the launcher asks before
    /// killing a running child, and the answer arrives as a later key.
    fn quit(&mut self) -> bool {
        true
    }

    /// A key the screen did not use itself (`y`, `n`, anything else).
    fn key(&mut self, _key: KeyEvent) {}

    /// Asked every poll: `true` closes the screen. It is how a supervisor that
    /// refused a `q` — the launcher, asking before it kills a child — leaves
    /// once the answer arrives, instead of making someone press `q` twice.
    fn exit(&mut self) -> bool {
        false
    }
}

/// The supervisor of a run nobody here started.
pub struct Observer;

impl Supervisor for Observer {}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    anyhow::ensure!(args.interval > 0.0, "--interval must be positive");
    let results_dir = cambium_bench::compare::absolute(&args.results_dir);
    anyhow::ensure!(
        results_dir.is_dir(),
        "no results directory at {}; pass --results-dir",
        results_dir.display()
    );
    // Not a terminal, and no `--once`: print the snapshot rather than drawing
    // a screen nobody can see. Watching a run from a pipe is exactly what
    // `--once` is, so this is a note on stderr and not an error.
    let tty = std::io::stdout().is_terminal();
    if args.once || !tty {
        if !args.once && !ctx.quiet {
            eprintln!("cambium-bench watch: stdout is not a terminal; printing one snapshot");
        }
        let snap = snapshot_with(&results_dir, &args.subjects, &engine_ids(ctx));
        print_once(ctx, &snap)?;
        return Ok(crate::EXIT_OK);
    }
    let mut observer = Observer;
    screen(
        &Screen {
            results_dir,
            subjects: args.subjects,
            interval: Duration::from_secs_f64(args.interval),
            engines: engine_ids(ctx),
        },
        &mut observer,
    )?;
    Ok(crate::EXIT_OK)
}

/// `--once`, in whichever format was asked for. The human form is the same
/// lines the screen draws; `json` is the snapshot document, so another tool
/// reads the matrix without learning the result files.
pub fn print_once(ctx: &Context, snap: &Snapshot) -> Result<()> {
    match ctx.format.data() {
        None => {}
        Some(DataFormat::Text) => {
            for line in render(snap) {
                println!("{line}");
            }
        }
        Some(DataFormat::Json) => println!("{}", serde_json::to_string_pretty(snap)?),
        Some(DataFormat::Jsonl) => println!("{}", crate::result_event(snap)?),
    }
    Ok(())
}

/// The engine id this build records for each manifest subject, by id: what
/// tells a finished row this build wrote from a stale one. Empty when there
/// is no manifest to work them out from, and then no row is called stale.
pub fn engine_ids(ctx: &Context) -> BTreeMap<String, String> {
    let Ok(session) = ctx.session(&crate::bench::Common::default()) else {
        return BTreeMap::new();
    };
    session
        .manifest
        .repos
        .iter()
        .filter_map(|e| {
            let run = session.subject(&e.id).ok()?;
            Some((
                e.id.clone(),
                cambium_bench::state::State::engine_id(&run.config().hash()),
            ))
        })
        .collect()
}

/// What the screen watches.
pub struct Screen {
    pub results_dir: PathBuf,
    pub subjects: Vec<String>,
    pub interval: Duration,
    /// [`engine_ids`], worked out once.
    pub engines: BTreeMap<String, String>,
}

/// Which view is up.
#[derive(Copy, Clone, PartialEq, Eq)]
enum View {
    Matrix,
    /// The per-change-set table of the selected subject.
    ChangeSets,
}

/// Opens the screen and polls until `q`.
///
/// The terminal is restored on every exit, including a panic in the draw: the
/// alternate screen and raw mode are left by the guard's `Drop`, because a
/// benchmark watcher that leaves a terminal in raw mode on an error is worse
/// than no watcher.
pub fn screen(what: &Screen, sup: &mut dyn Supervisor) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let colour = std::env::var_os("NO_COLOR").is_none();
    let mut view = View::Matrix;
    let mut selected = 0usize;
    let mut scroll = 0u16;
    let mut snap = snapshot_with(&what.results_dir, &what.subjects, &what.engines);
    let mut last = Instant::now();
    loop {
        sup.tick();
        if sup.exit() {
            break;
        }
        let footer = sup.footer();
        term.draw(|f| draw(f, &snap, view, selected, scroll, &footer, colour))?;
        let wait = what.interval.saturating_sub(last.elapsed());
        if event::poll(wait)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Release {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc if sup.quit() => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if sup.quit() {
                                break;
                            }
                        }
                        KeyCode::Enter => {
                            view = match view {
                                View::Matrix => View::ChangeSets,
                                View::ChangeSets => View::Matrix,
                            };
                            scroll = 0;
                        }
                        KeyCode::Down | KeyCode::Char('j') => match view {
                            View::Matrix => {
                                selected = (selected + 1).min(snap.subjects.len().saturating_sub(1))
                            }
                            View::ChangeSets => scroll = scroll.saturating_add(1),
                        },
                        KeyCode::Up | KeyCode::Char('k') => match view {
                            View::Matrix => selected = selected.saturating_sub(1),
                            View::ChangeSets => scroll = scroll.saturating_sub(1),
                        },
                        _ => sup.key(key),
                    }
                }
            }
        }
        if last.elapsed() >= what.interval {
            snap = snapshot_with(&what.results_dir, &what.subjects, &what.engines);
            selected = selected.min(snap.subjects.len().saturating_sub(1));
            last = Instant::now();
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut ratatui::Frame,
    snap: &Snapshot,
    view: View,
    selected: usize,
    scroll: u16,
    footer: &[String],
    colour: bool,
) {
    let lines: Vec<String> = match view {
        View::Matrix => render(snap),
        View::ChangeSets => match snap.subjects.get(selected) {
            Some(r) => render_change_sets(r),
            None => vec!["no subject".to_string()],
        },
    };
    let highlight = (view == View::Matrix).then(|| HEADER_LINES + selected);
    let body: Vec<Line> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let style = match colour && Some(i) == highlight {
                true => Style::default().add_modifier(Modifier::REVERSED),
                false => Style::default(),
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .chain(footer.iter().map(|l| Line::from(l.clone())))
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            match view {
                View::Matrix => {
                    "q quit   enter the change sets of the selected subject   up/down select"
                }
                View::ChangeSets => "q quit   enter back to the matrix   up/down scroll",
            },
            match colour {
                true => Style::default().add_modifier(Modifier::DIM),
                false => Style::default(),
            },
        ))))
        .collect();
    f.render_widget(
        Paragraph::new(Text::from(body))
            .scroll((scroll, 0))
            .block(Block::default().borders(Borders::NONE)),
        f.area(),
    );
}

/// Raw mode and the alternate screen, left on drop.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        std::io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = std::io::stdout().flush();
    }
}
