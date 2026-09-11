//! `cambium-bench` with no arguments: the screen that composes a run.
//!
//! Rainy's words on issue #1: "a TUI that walks us through running various
//! benchmarks against different repos and tools". So: pick the subjects (the
//! manifest's, and the synthetic presets beside them), pick the tools, pick
//! the workloads, pick a size, see the exact command lines, and confirm. On
//! confirm it spawns those command lines as child processes of this binary,
//! one after another, and switches to the watch screen with the running
//! child's stderr tail in the footer.
//!
//! It reimplements no workload. The plan is [`cambium_bench::plan::Plan`] —
//! the same one `cambium-bench run` executes and `tools/bench/run-published.sh`
//! calls — and the thing that runs its steps is `cambium-bench` itself.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cambium_bench::manifest::Manifest;
use cambium_bench::plan::{Invocation, Plan, Queue, Size, StepKind};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::watch::{screen, Screen, Supervisor};
use crate::Context;

// ------------------------------------------------------------------ the screen

/// Which list the cursor is in.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Pane {
    Subjects,
    Tools,
    Workloads,
    Size,
}

impl Pane {
    const ALL: [Pane; 4] = [Pane::Subjects, Pane::Tools, Pane::Workloads, Pane::Size];

    fn title(self) -> &'static str {
        match self {
            Pane::Subjects => "subjects",
            Pane::Tools => "tools",
            Pane::Workloads => "workloads",
            Pane::Size => "size",
        }
    }
}

/// The state of the composition screen.
struct Compose {
    subjects: Vec<(String, bool)>,
    tools: Vec<(String, bool)>,
    workloads: Vec<(StepKind, bool)>,
    size: Size,
    pane: Pane,
    row: [usize; 4],
    /// The plan is shown for confirmation before anything runs.
    confirming: bool,
}

impl Compose {
    fn new(manifest: &Manifest) -> Self {
        let mut subjects: Vec<(String, bool)> = manifest
            .repos
            .iter()
            .filter(|r| !r.skip)
            .map(|r| (r.id.clone(), false))
            .collect();
        // The synthetic presets are subjects too, and the only ones that run
        // on a machine with no Bazel and no fetch.
        subjects.extend(cambium_synth::GraphSpec::PRESETS.iter().map(|p| {
            (
                format!("{}{p}", cambium_bench::subject::SYNTH_PREFIX),
                false,
            )
        }));
        Self {
            subjects,
            tools: ["bazel-diff", "tango"]
                .iter()
                .map(|t| (t.to_string(), true))
                .collect(),
            workloads: StepKind::WORKLOADS.iter().map(|w| (*w, true)).collect(),
            size: Size::Laptop,
            pane: Pane::Subjects,
            row: [0; 4],
            confirming: false,
        }
    }

    /// The plan: the preparation steps and the reports always, the picked
    /// workloads in between.
    fn plan(&self) -> Plan {
        let picked_workloads: Vec<StepKind> = self
            .workloads
            .iter()
            .filter(|(_, on)| *on)
            .map(|(w, _)| *w)
            .collect();
        let mut plan = Plan::published(picked(&self.subjects));
        plan.tools = picked(&self.tools);
        plan.steps = StepKind::ALL
            .iter()
            .copied()
            .filter(|s| !StepKind::WORKLOADS.contains(s) || picked_workloads.contains(s))
            .collect();
        plan.size = self.size;
        plan
    }

    fn pane_index(&self) -> usize {
        Pane::ALL.iter().position(|p| *p == self.pane).unwrap_or(0)
    }

    fn len(&self) -> usize {
        match self.pane {
            Pane::Subjects => self.subjects.len(),
            Pane::Tools => self.tools.len(),
            Pane::Workloads => self.workloads.len(),
            Pane::Size => Size::ALL.len(),
        }
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.len();
        if n == 0 {
            return;
        }
        let i = self.pane_index();
        let row = self.row[i] as isize + delta;
        self.row[i] = row.clamp(0, n as isize - 1) as usize;
    }

    fn toggle(&mut self) {
        let row = self.row[self.pane_index()];
        match self.pane {
            Pane::Subjects => flip(&mut self.subjects, row),
            Pane::Tools => flip(&mut self.tools, row),
            Pane::Workloads => flip(&mut self.workloads, row),
            Pane::Size => self.size = Size::ALL[row.min(Size::ALL.len() - 1)],
        }
    }

    fn next_pane(&mut self, delta: isize) {
        let i = (self.pane_index() as isize + delta).rem_euclid(Pane::ALL.len() as isize);
        self.pane = Pane::ALL[i as usize];
    }

    /// The screen, as lines. The selected row of the active pane is marked
    /// with `>` rather than only by colour, so the screen reads the same under
    /// `NO_COLOR`.
    fn lines(&self) -> Vec<Line<'static>> {
        let mut out = vec![Line::from("cambium-bench — compose a run"), Line::from("")];
        for (i, pane) in Pane::ALL.iter().enumerate() {
            let active = *pane == self.pane;
            out.push(Line::from(Span::styled(
                format!("{} {}", if active { ">" } else { " " }, pane.title()),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            let rows: Vec<(String, bool)> = match pane {
                Pane::Subjects => self.subjects.clone(),
                Pane::Tools => self.tools.clone(),
                Pane::Workloads => self
                    .workloads
                    .iter()
                    .map(|(w, on)| (w.name().to_string(), *on))
                    .collect(),
                Pane::Size => Size::ALL
                    .iter()
                    .map(|s| {
                        (
                            format!("{:<10} {}", s.name(), s.describe()),
                            *s == self.size,
                        )
                    })
                    .collect(),
            };
            for (r, (name, on)) in rows.iter().enumerate() {
                let cursor = active && self.row[i] == r;
                out.push(Line::from(format!(
                    "   {} [{}] {name}",
                    if cursor { ">" } else { " " },
                    if *on { "x" } else { " " }
                )));
            }
            out.push(Line::from(""));
        }
        let plan = self.plan();
        out.push(Line::from(Span::styled(
            "it will run",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if plan.is_runnable() {
            for step in plan.steps() {
                out.push(Line::from(format!("   {}", step.command_line())));
            }
        } else {
            out.push(Line::from(
                "   nothing: pick at least one subject and one workload",
            ));
        }
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            match self.confirming {
                true => "run these? y to start, n to go back, q to quit",
                false => "space toggle   tab/up/down move   enter review   q quit",
            },
            Style::default().add_modifier(Modifier::DIM),
        )));
        out
    }
}

fn picked(rows: &[(String, bool)]) -> Vec<String> {
    rows.iter()
        .filter(|(_, on)| *on)
        .map(|(n, _)| n.clone())
        .collect()
}

fn flip<T>(rows: &mut [(T, bool)], row: usize) {
    if let Some(r) = rows.get_mut(row) {
        r.1 = !r.1;
    }
}

/// `cambium-bench` with no verb, on a terminal.
pub fn run(ctx: &Context) -> Result<u8> {
    let (_, manifest) = Manifest::discover(&ctx.root)?;
    let Some(plan) = compose(&manifest)? else {
        return Ok(crate::EXIT_OK);
    };
    let results = plan.results();
    std::fs::create_dir_all(&results)?;
    let results = cambium_bench::compare::absolute(&results);
    let subjects: Vec<String> = plan
        .subjects
        .iter()
        .map(|s| cambium_bench::session::subject_dir_name(s))
        .collect();
    let mut runner = Runner::start(&plan)?;
    screen(
        &Screen {
            results_dir: results,
            subjects,
            interval: Duration::from_secs(1),
        },
        &mut runner,
    )?;
    let failures = runner.finish();
    Ok(match failures {
        0 => crate::EXIT_OK,
        n => {
            eprintln!("cambium-bench: {n} step(s) failed; see the result files and the log");
            crate::EXIT_ERROR
        }
    })
}

/// Draws the composition screen until the plan is confirmed (`Some`) or
/// abandoned (`None`).
fn compose(manifest: &Manifest) -> Result<Option<Plan>> {
    let _guard = crate::watch::TerminalGuard::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut c = Compose::new(manifest);
    loop {
        term.draw(|f| {
            f.render_widget(Paragraph::new(Text::from(c.lines())), f.area());
        })?;
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char(' ') if !c.confirming => c.toggle(),
            KeyCode::Tab => c.next_pane(1),
            KeyCode::BackTab => c.next_pane(-1),
            KeyCode::Down | KeyCode::Char('j') => c.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => c.move_by(-1),
            KeyCode::Enter if !c.confirming => c.confirming = c.plan().is_runnable(),
            KeyCode::Char('y') | KeyCode::Enter if c.confirming => return Ok(Some(c.plan())),
            KeyCode::Char('n') if c.confirming => c.confirming = false,
            _ => {}
        }
    }
}

/// Runs the plan's steps in order, as children of this binary, under the
/// watch screen.
///
/// It drives nothing itself: every step is a `cambium-bench` invocation, the
/// same one a person would type and the same one `cambium-bench run` spawns.
pub struct Runner {
    queue: Queue,
    exe: PathBuf,
    current: Option<(Invocation, Child)>,
    tail: Arc<Mutex<VecDeque<String>>>,
    failures: usize,
    done: usize,
    /// `q` with a child running asks first.
    asking: bool,
    /// The answer was yes: the screen closes on its next poll.
    quitting: bool,
}

/// How many lines of the running child's stderr the footer keeps.
const TAIL_LINES: usize = 4;

impl Runner {
    fn start(plan: &Plan) -> Result<Self> {
        let mut r = Self {
            queue: Queue::new(plan),
            exe: std::env::current_exe()?,
            current: None,
            tail: Arc::new(Mutex::new(VecDeque::new())),
            failures: 0,
            done: 0,
            asking: false,
            quitting: false,
        };
        r.spawn_next();
        Ok(r)
    }

    fn spawn_next(&mut self) {
        let Some(step) = self.queue.pop() else {
            return;
        };
        let mut cmd = Command::new(&self.exe);
        cmd.args(&step.args)
            .stdin(Stdio::null())
            // The child's stdout is the result path it printed, which the
            // screen has no room for and the result files record anyway; its
            // stderr is the progress and the diagnostics, which the footer
            // shows. Neither may be inherited: this is an alternate screen.
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                self.push_tail(format!("$ {}", step.command_line()));
                if let Some(err) = child.stderr.take() {
                    let tail = Arc::clone(&self.tail);
                    std::thread::spawn(move || {
                        for line in BufReader::new(err).lines().map_while(Result::ok) {
                            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                            t.push_back(line);
                            while t.len() > TAIL_LINES {
                                t.pop_front();
                            }
                        }
                    });
                }
                self.current = Some((step, child));
            }
            Err(e) => {
                self.failures += 1;
                self.done += 1;
                self.push_tail(format!("could not start {}: {e}", step.command_line()));
                self.queue.finished(&step, false);
            }
        }
    }

    fn push_tail(&self, line: String) {
        let mut t = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        t.push_back(line);
        while t.len() > TAIL_LINES {
            t.pop_front();
        }
    }

    /// Kills whatever is still running and answers with the failure count.
    pub fn finish(&mut self) -> usize {
        if let Some((_, child)) = self.current.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.failures
    }
}

impl Supervisor for Runner {
    /// Reaps a finished child and starts the next step. A step that failed
    /// does not stop the rest; a failed fetch or build skips its subject.
    fn tick(&mut self) {
        let mut wait_error = None;
        let finished = match self.current.as_mut() {
            Some((step, child)) => match child.try_wait() {
                Ok(Some(status)) => Some((step.clone(), status.success())),
                Ok(None) => None,
                Err(e) => {
                    wait_error = Some(format!("waiting on the child failed: {e}"));
                    Some((step.clone(), false))
                }
            },
            None => None,
        };
        if let Some(e) = wait_error {
            self.push_tail(e);
        }
        if let Some((step, ok)) = finished {
            self.current = None;
            self.done += 1;
            if !ok {
                self.failures += 1;
                self.push_tail(format!("FAILED {}", step.command_line()));
            }
            self.queue.finished(&step, ok);
            self.spawn_next();
        }
    }

    fn footer(&mut self) -> Vec<String> {
        let mut out = vec![String::new()];
        out.push(match &self.current {
            Some((step, _)) => format!(
                "step {}/{}  {}",
                self.done + 1,
                self.queue.total,
                step.command_line()
            ),
            None => format!(
                "{} of {} steps done, {} failed",
                self.done, self.queue.total, self.failures
            ),
        });
        let tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        out.extend(tail.iter().map(|l| format!("  {l}")));
        if self.asking {
            out.push(String::new());
            out.push("a step is still running: kill it and quit? y / n".into());
        }
        out
    }

    /// `q` with a child running asks first; with nothing running it quits.
    fn quit(&mut self) -> bool {
        if self.current.is_none() {
            return true;
        }
        self.asking = true;
        false
    }

    fn exit(&mut self) -> bool {
        self.quitting
    }

    fn key(&mut self, key: KeyEvent) {
        if !self.asking {
            return;
        }
        match key.code {
            KeyCode::Char('y') => {
                self.queue.clear();
                self.finish();
                self.current = None;
                self.asking = false;
                self.quitting = true;
            }
            KeyCode::Char('n') => self.asking = false,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        toml::from_str(&format!(
            "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{0}\"\n\
             [[repo]]\nid = \"gone\"\nurl = \"u\"\npin = \"{0}\"\nskip = true\n",
            "a".repeat(40)
        ))
        .unwrap()
    }

    #[test]
    fn the_screen_offers_the_manifest_and_the_synthetic_presets() {
        let c = Compose::new(&manifest());
        let names: Vec<&str> = c.subjects.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"bazel"));
        assert!(!names.contains(&"gone"), "a skipped subject is not offered");
        assert!(names.iter().any(|n| n.starts_with("synth:")), "{names:?}");
        // Nothing is selected for you, but the tools and workloads are: the
        // question the screen asks is which repositories.
        assert!(c.plan().subjects.is_empty());
        assert_eq!(c.plan().tools, ["bazel-diff", "tango"]);
        assert_eq!(c.plan().steps, StepKind::ALL);
        assert_eq!(c.size, Size::Laptop);
    }

    /// The screen composes the library's plan, so a confirmed published run
    /// is the published run.
    #[test]
    fn the_confirmed_plan_is_the_library_plan() {
        let mut c = Compose::new(&manifest());
        c.toggle();
        c.next_pane(3);
        c.move_by(5);
        c.toggle();
        let plan = c.plan();
        assert_eq!(plan.size, Size::Published);
        assert_eq!(
            plan.steps()
                .iter()
                .map(|s| s.command_line())
                .collect::<Vec<_>>(),
            Plan::published(vec!["bazel".into()])
                .steps()
                .iter()
                .map(|s| s.command_line())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_panes_toggle_what_the_cursor_is_on() {
        let mut c = Compose::new(&manifest());
        c.toggle();
        assert_eq!(c.plan().subjects, ["bazel"]);
        c.next_pane(1);
        c.toggle();
        assert_eq!(c.plan().tools, ["tango"], "bazel-diff was turned off");
        c.next_pane(1);
        c.move_by(1);
        c.toggle();
        assert_eq!(
            c.plan().steps,
            [
                StepKind::Fetch,
                StepKind::Build,
                StepKind::History,
                StepKind::Report
            ],
            "changes was turned off; the preparation and the report stay"
        );
        c.next_pane(1);
        c.move_by(-5);
        c.toggle();
        assert_eq!(c.size, Size::Smoke, "the cursor clamps at the top");
        // The plan is on the screen before anything runs.
        let text: Vec<String> = c
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            text.iter()
                .any(|l| l.contains("cambium-bench --log info history bazel")),
            "{text:?}"
        );
    }
}
