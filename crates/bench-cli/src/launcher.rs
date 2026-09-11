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
//! It reimplements no workload. Everything here builds argument vectors —
//! [`Plan::steps`] is pure and unit-tested — and the thing that runs them is
//! `cambium-bench` itself, which is also what `tools/bench/run-published.sh`
//! runs. The script stays the scripted way to do the same thing, and a test
//! reads it to check that the launcher's "published" size produces the
//! command lines it does.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use cambium_bench::manifest::Manifest;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::watch::{screen, Screen, Supervisor};
use crate::Context;

/// How big a run is. The three sizes the repository already has: the fifteen
/// minutes before an eight-hour pass, the laptop-sized slice `--quick` is
/// sugar for, and the published run itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Size {
    Smoke,
    Laptop,
    Published,
}

impl Size {
    pub const ALL: [Size; 3] = [Size::Smoke, Size::Laptop, Size::Published];

    pub fn name(self) -> &'static str {
        match self {
            Size::Smoke => "smoke",
            Size::Laptop => "laptop",
            Size::Published => "published",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Size::Smoke => "3 commits, 1 change set, into bench-results-smoke/ (~15 min)",
            Size::Laptop => "--quick: 50 commits, 50 change sets, Tango on the first 10",
            Size::Published => "the subject's own window and 40 change sets: the published run",
        }
    }

    /// Where a run of this size writes. The smoke run has its own tree so the
    /// real one neither resumes nor overwrites it, exactly as `SMOKE=1` in
    /// `tools/bench/run-published.sh`.
    fn results_dir(self) -> &'static str {
        match self {
            Size::Smoke => "bench-results-smoke",
            _ => "bench-results",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Workload {
    History,
    Changes,
}

impl Workload {
    pub const ALL: [Workload; 2] = [Workload::History, Workload::Changes];

    pub fn name(self) -> &'static str {
        match self {
            Workload::History => "history",
            Workload::Changes => "changes",
        }
    }
}

/// What a composed run is.
#[derive(Clone, Debug)]
pub struct Plan {
    pub subjects: Vec<String>,
    /// Tool names as `--tools` takes them. `cambium` is always in the list it
    /// passes, because it is what the others are compared against.
    pub tools: Vec<String>,
    pub workloads: Vec<Workload>,
    pub size: Size,
    /// Overrides [`Size::results_dir`] when a caller has one.
    pub results_dir: Option<PathBuf>,
}

/// One child process: the argument vector, without the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub args: Vec<String>,
}

impl Step {
    fn new<const N: usize>(args: [&str; N]) -> Self {
        Self {
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn with(mut self, more: &[&str]) -> Self {
        self.args.extend(more.iter().map(|s| s.to_string()));
        self
    }

    /// What the screen shows and what a person could paste.
    pub fn command_line(&self) -> String {
        format!("cambium-bench {}", self.args.join(" "))
    }
}

impl Plan {
    /// The command lines, in the order they run: per subject the fetch, the
    /// cold build, the workloads and that subject's report, then one report
    /// over every subject at the end.
    ///
    /// The same order and the same flags as `tools/bench/run-published.sh`,
    /// which a test checks by reading the script.
    pub fn steps(&self) -> Vec<Step> {
        let results = self
            .results_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(self.size.results_dir()));
        let results = results.display().to_string();
        let dir: Vec<&str> = vec!["--results-dir", &results];
        let mut out = Vec::new();
        for s in &self.subjects {
            out.push(Step::new(["--log", "info", "subjects", "fetch", s]).with(&dir));
            out.push(Step::new(["--log", "info", "subjects", "build", s]).with(&dir));
            if self.workloads.contains(&Workload::History) {
                let mut step =
                    Step::new(["--log", "info", "history", s, "--oracle", "rebuild"]).with(&dir);
                step = match self.size {
                    Size::Smoke => step.with(&["--commits", "3", "--restart"]),
                    Size::Laptop => step.with(&["--quick"]),
                    Size::Published => step,
                };
                out.push(step);
            }
            if self.workloads.contains(&Workload::Changes) {
                let tools = self.tool_flag();
                let mut step = Step::new([
                    "--log",
                    "info",
                    "changes",
                    s,
                    "--tools",
                    &tools,
                    "--oracle",
                    "rdeps,load",
                ])
                .with(&dir);
                step = match self.size {
                    Size::Smoke => step.with(&["--change-sets", "1", "--restart"]),
                    Size::Laptop => step.with(&["--quick"]),
                    Size::Published => step,
                };
                out.push(step);
            }
            out.push(Step::new(["report", s]).with(&dir));
        }
        out.push(Step::new(["report"]).with(&dir));
        out
    }

    /// The `--tools` value: cambium first and always, since naming it alone is
    /// how a run measures cambium with no foil.
    fn tool_flag(&self) -> String {
        let mut names: Vec<&str> = vec![cambium_bench::tools::cambi::NAME];
        names.extend(
            self.tools
                .iter()
                .map(String::as_str)
                .filter(|t| *t != cambium_bench::tools::cambi::NAME),
        );
        names.join(",")
    }

    fn is_runnable(&self) -> bool {
        !self.subjects.is_empty() && !self.workloads.is_empty()
    }
}

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
    workloads: Vec<(Workload, bool)>,
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
            workloads: Workload::ALL.iter().map(|w| (*w, true)).collect(),
            size: Size::Laptop,
            pane: Pane::Subjects,
            row: [0; 4],
            confirming: false,
        }
    }

    fn plan(&self) -> Plan {
        Plan {
            subjects: picked(&self.subjects),
            tools: picked(&self.tools),
            workloads: self
                .workloads
                .iter()
                .filter(|(_, on)| *on)
                .map(|(w, _)| *w)
                .collect(),
            size: self.size,
            results_dir: None,
        }
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
    let results = plan
        .results_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(plan.size.results_dir()));
    std::fs::create_dir_all(&results)?;
    let results = cambium_bench::compare::absolute(&results);
    let subjects: Vec<String> = plan.subjects.iter().map(|s| s.replace(':', "-")).collect();
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

/// Runs the plan's steps in order, as children of this binary.
///
/// It drives nothing itself: every step is a `cambium-bench` invocation, the
/// same one a person would type and the same one the shell driver runs.
pub struct Runner {
    queue: VecDeque<Step>,
    exe: PathBuf,
    current: Option<(Step, Child)>,
    tail: Arc<Mutex<VecDeque<String>>>,
    failures: usize,
    done: usize,
    total: usize,
    /// `q` with a child running asks first.
    asking: bool,
    /// The answer was yes: the screen closes on its next poll.
    quitting: bool,
}

/// How many lines of the running child's stderr the footer keeps.
const TAIL_LINES: usize = 4;

impl Runner {
    fn start(plan: &Plan) -> Result<Self> {
        let steps = plan.steps();
        let mut r = Self {
            total: steps.len(),
            queue: steps.into(),
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
        let Some(step) = self.queue.pop_front() else {
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
    /// does not stop the rest, exactly as in the shell driver: a subject that
    /// cannot fetch must not cost the other four their run.
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
            self.spawn_next();
        }
    }

    fn footer(&mut self) -> Vec<String> {
        let mut out = vec![String::new()];
        out.push(match &self.current {
            Some((step, _)) => format!(
                "step {}/{}  {}",
                self.done + 1,
                self.total,
                step.command_line()
            ),
            None => format!(
                "{} of {} steps done, {} failed",
                self.done, self.total, self.failures
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

    fn plan(size: Size) -> Plan {
        Plan {
            subjects: vec!["bazel".into()],
            tools: vec!["bazel-diff".into(), "tango".into()],
            workloads: Workload::ALL.to_vec(),
            size,
            results_dir: None,
        }
    }

    fn lines(p: &Plan) -> Vec<String> {
        p.steps().iter().map(|s| s.command_line()).collect()
    }

    #[test]
    fn the_published_size_is_what_the_shell_driver_runs() {
        // The script is the scripted way to do the same thing, and the two
        // must not drift: a run composed here has to be the run that produced
        // the published numbers.
        let script = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/bench/run-published.sh"),
        )
        .expect("the driver script");
        let flags = |needle: &str| -> Vec<String> {
            let line = script
                .lines()
                .find(|l| l.trim_start().starts_with(needle))
                .unwrap_or_else(|| panic!("no {needle} line in run-published.sh"));
            let inner = line
                .split_once('(')
                .and_then(|(_, r)| r.rsplit_once(')'))
                .expect("an array literal")
                .0;
            inner
                .split_whitespace()
                .filter(|t| !t.starts_with("\"${"))
                .map(str::to_string)
                .collect()
        };
        // The verbs the script runs per subject, in its own order.
        let verbs: Vec<&str> = script
            .lines()
            .filter_map(|l| l.trim().strip_prefix("if wants "))
            .filter_map(|l| l.split(';').next())
            .collect();
        assert_eq!(verbs, ["fetch", "build", "history", "changes", "report"]);

        let got = lines(&plan(Size::Published));
        assert_eq!(
            got,
            [
                "cambium-bench --log info subjects fetch bazel --results-dir bench-results",
                "cambium-bench --log info subjects build bazel --results-dir bench-results",
                "cambium-bench --log info history bazel --oracle rebuild --results-dir bench-results",
                "cambium-bench --log info changes bazel --tools cambium,bazel-diff,tango --oracle rdeps,load --results-dir bench-results",
                "cambium-bench report bazel --results-dir bench-results",
                "cambium-bench report --results-dir bench-results",
            ]
        );
        // And every flag the script passes for the two workloads is in them.
        for f in flags("history_args=") {
            assert!(got[2].contains(&f), "history is missing {f}: {}", got[2]);
        }
        for f in flags("changes_args=") {
            assert!(got[3].contains(&f), "changes is missing {f}: {}", got[3]);
        }
        // The script's default subject list is still the manifest's, which is
        // what the launcher offers.
        assert!(script.contains("SUBJECTS=${SUBJECTS:-\"bazel sourcegraph apollo envoy xla\"}"));
    }

    #[test]
    fn the_smoke_size_is_the_scripts_smoke_run() {
        let got = lines(&plan(Size::Smoke));
        assert!(
            got.iter()
                .all(|l| l.contains("--results-dir bench-results-smoke")),
            "{got:?}"
        );
        assert!(got[2].ends_with("--commits 3 --restart"), "{}", got[2]);
        assert!(got[3].ends_with("--change-sets 1 --restart"), "{}", got[3]);
    }

    #[test]
    fn the_laptop_size_is_the_quick_sugar_and_nothing_else() {
        let got = lines(&plan(Size::Laptop));
        assert!(got[2].ends_with("--quick"), "{}", got[2]);
        assert!(got[3].ends_with("--quick"), "{}", got[3]);
        assert!(got[0].contains("bench-results"), "{}", got[0]);
    }

    #[test]
    fn one_workload_runs_one_workload_and_cambium_is_always_a_tool() {
        let p = Plan {
            workloads: vec![Workload::Changes],
            tools: vec![],
            ..plan(Size::Laptop)
        };
        let got = lines(&p);
        assert!(!got.iter().any(|l| l.contains(" history ")), "{got:?}");
        // Naming no foil measures cambium alone, which is a run, not an error.
        assert!(got[2].contains("--tools cambium "), "{}", got[2]);
    }

    #[test]
    fn a_plan_with_no_subject_or_no_workload_runs_nothing() {
        assert!(!Plan {
            subjects: vec![],
            ..plan(Size::Laptop)
        }
        .is_runnable());
        assert!(!Plan {
            workloads: vec![],
            ..plan(Size::Laptop)
        }
        .is_runnable());
        assert!(plan(Size::Laptop).is_runnable());
    }

    #[test]
    fn the_screen_offers_the_manifest_and_the_synthetic_presets() {
        let manifest: Manifest = toml::from_str(&format!(
            "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{0}\"\n\
             [[repo]]\nid = \"gone\"\nurl = \"u\"\npin = \"{0}\"\nskip = true\n",
            "a".repeat(40)
        ))
        .unwrap();
        let c = Compose::new(&manifest);
        let names: Vec<&str> = c.subjects.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"bazel"));
        assert!(!names.contains(&"gone"), "a skipped subject is not offered");
        assert!(names.iter().any(|n| n.starts_with("synth:")), "{names:?}");
        // Nothing is selected for you, but the tools and workloads are: the
        // question the screen asks is which repositories.
        assert!(c.plan().subjects.is_empty());
        assert_eq!(c.plan().tools, ["bazel-diff", "tango"]);
        assert_eq!(c.plan().workloads, Workload::ALL);
        assert_eq!(c.size, Size::Laptop);
    }

    #[test]
    fn the_panes_toggle_what_the_cursor_is_on() {
        let manifest: Manifest = toml::from_str(&format!(
            "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{}\"\n",
            "a".repeat(40)
        ))
        .unwrap();
        let mut c = Compose::new(&manifest);
        c.toggle();
        assert_eq!(c.plan().subjects, ["bazel"]);
        c.next_pane(1);
        c.toggle();
        assert_eq!(c.plan().tools, ["tango"], "bazel-diff was turned off");
        c.next_pane(1);
        c.move_by(1);
        c.toggle();
        assert_eq!(c.plan().workloads, [Workload::History]);
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
