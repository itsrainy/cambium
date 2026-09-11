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
use ratatui::layout::{Constraint, Layout};
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

/// One pickable subject and the line that says what it is.
struct Subject {
    name: String,
    about: String,
    on: bool,
}

/// The state of the composition screen.
struct Compose {
    /// The manifest's repositories. The subjects pane lists these and then
    /// one more row that opens `synth`.
    subjects: Vec<Subject>,
    /// The synthetic presets: thirty of them, so a list of their own.
    synth: Vec<Subject>,
    /// The synthetic presets list is open in place of the subjects pane.
    in_synth: bool,
    synth_row: usize,
    tools: Vec<(String, bool)>,
    workloads: Vec<(StepKind, bool)>,
    size: Size,
    pane: Pane,
    row: [usize; 4],
    /// The plan is shown for confirmation before anything runs.
    confirming: bool,
    /// The first line of the screen drawn, so the cursor stays in view on a
    /// terminal shorter than the screen.
    scroll: usize,
    /// Where the plan writes, when not its size's default (the tests).
    results_dir: Option<PathBuf>,
    /// On review, the results the plan would run over: `envoy: history`.
    existing: Vec<String>,
}

/// How a confirmed run starts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Start {
    /// Nothing was recorded, or the plan resumes it.
    AsPlanned,
    /// Move the results directory aside first.
    Fresh,
}

impl Compose {
    fn new(manifest: &Manifest) -> Self {
        let subjects = manifest
            .repos
            .iter()
            .filter(|r| !r.skip)
            .map(|r| Subject {
                name: r.id.clone(),
                about: about_repo(r),
                on: false,
            })
            .collect();
        // The synthetic presets are subjects too, and the only ones that run
        // on a machine with no Bazel and no fetch.
        let synth = cambium_synth::GraphSpec::PRESETS
            .iter()
            .map(|p| Subject {
                name: format!("{}{p}", cambium_bench::subject::SYNTH_PREFIX),
                about: about_preset(p),
                on: false,
            })
            .collect();
        Self {
            subjects,
            synth,
            in_synth: false,
            synth_row: 0,
            tools: ["bazel-diff", "tango"]
                .iter()
                .map(|t| (t.to_string(), true))
                .collect(),
            workloads: StepKind::WORKLOADS.iter().map(|w| (*w, true)).collect(),
            size: Size::Medium,
            pane: Pane::Subjects,
            row: [0; 4],
            confirming: false,
            scroll: 0,
            results_dir: None,
            existing: Vec::new(),
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
        let subjects = self
            .subjects
            .iter()
            .chain(&self.synth)
            .filter(|s| s.on)
            .map(|s| s.name.clone())
            .collect();
        let mut plan = Plan::published(subjects);
        plan.tools = picked(&self.tools);
        plan.steps = StepKind::ALL
            .iter()
            .copied()
            .filter(|s| !StepKind::WORKLOADS.contains(s) || picked_workloads.contains(s))
            .collect();
        plan.size = self.size;
        plan.results_dir = self.results_dir.clone();
        plan
    }

    /// `enter`: the plan, for confirmation, with whatever it would run over.
    fn review(&mut self) {
        let plan = self.plan();
        self.confirming = plan.is_runnable();
        self.existing = match plan.size {
            Size::Small => Vec::new(),
            _ => plan.existing_results(),
        };
    }

    fn pane_index(&self) -> usize {
        Pane::ALL.iter().position(|p| *p == self.pane).unwrap_or(0)
    }

    /// The subjects pane's last row, the one that opens the presets.
    fn synth_entry(&self) -> usize {
        self.subjects.len()
    }

    fn on_synth_entry(&self) -> bool {
        self.pane == Pane::Subjects && self.row[0] == self.synth_entry()
    }

    fn len(&self) -> usize {
        match self.pane {
            Pane::Subjects => self.subjects.len() + 1,
            Pane::Tools => self.tools.len(),
            Pane::Workloads => self.workloads.len(),
            Pane::Size => Size::ALL.len(),
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.in_synth {
            let row = self.synth_row as isize + delta;
            self.synth_row = row.clamp(0, self.synth.len() as isize - 1) as usize;
            return;
        }
        let n = self.len();
        if n == 0 {
            return;
        }
        let i = self.pane_index();
        let row = self.row[i] as isize + delta;
        self.row[i] = row.clamp(0, n as isize - 1) as usize;
    }

    fn toggle(&mut self) {
        if self.in_synth {
            if let Some(s) = self.synth.get_mut(self.synth_row) {
                s.on = !s.on;
            }
            return;
        }
        let row = self.row[self.pane_index()];
        match self.pane {
            Pane::Subjects if row == self.synth_entry() => self.in_synth = true,
            Pane::Subjects => {
                if let Some(s) = self.subjects.get_mut(row) {
                    s.on = !s.on;
                }
            }
            Pane::Tools => flip(&mut self.tools, row),
            Pane::Workloads => flip(&mut self.workloads, row),
            Pane::Size => self.size = Size::ALL[row.min(Size::ALL.len() - 1)],
        }
    }

    fn next_pane(&mut self, delta: isize) {
        self.in_synth = false;
        let i = (self.pane_index() as isize + delta).rem_euclid(Pane::ALL.len() as isize);
        self.pane = Pane::ALL[i as usize];
    }

    /// The screen, as lines, and the line the cursor is on. The selected row
    /// is marked with `>` rather than only by colour, so the screen reads the
    /// same under `NO_COLOR`.
    fn lines(&self) -> (Vec<Line<'static>>, usize) {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut out = vec![Line::from("cambium-bench — compose a run"), Line::from("")];
        let mut cursor_line = 0;
        let row_line = |cursor: bool, on: bool, name: &str| {
            Line::from(format!(
                "   {} [{}] {name}",
                if cursor { ">" } else { " " },
                if on { "x" } else { " " }
            ))
        };
        for (i, pane) in Pane::ALL.iter().enumerate() {
            let active = *pane == self.pane;
            out.push(Line::from(Span::styled(
                format!("{} {}", if active { ">" } else { " " }, pane.title()),
                bold,
            )));
            match pane {
                Pane::Subjects if self.in_synth => {
                    for l in [
                        "     Generated graphs: a size (tiny 200 rules .. xxl 10M), optionally",
                        "     with a shape. No fetch and no real repository; esc goes back.",
                    ] {
                        out.push(Line::from(Span::styled(l, dim)));
                    }
                    for (r, s) in self.synth.iter().enumerate() {
                        let cursor = self.synth_row == r;
                        if cursor {
                            cursor_line = out.len();
                        }
                        out.push(row_line(cursor, s.on, &s.name));
                    }
                    if let Some(s) = self.synth.get(self.synth_row) {
                        out.push(Line::from(Span::styled(format!("     {}", s.about), dim)));
                    }
                }
                Pane::Subjects => {
                    for l in [
                        "     What to benchmark. The repositories in subject_repos.toml are",
                        "     fetched and indexed with Bazel on this machine (the box has them).",
                    ] {
                        out.push(Line::from(Span::styled(l, dim)));
                    }
                    for (r, s) in self.subjects.iter().enumerate() {
                        let cursor = active && self.row[i] == r;
                        if cursor {
                            cursor_line = out.len();
                        }
                        out.push(row_line(cursor, s.on, &s.name));
                    }
                    let picked: Vec<&str> = self
                        .synth
                        .iter()
                        .filter(|s| s.on)
                        .map(|s| s.name.as_str())
                        .collect();
                    let cursor = self.on_synth_entry();
                    if cursor {
                        cursor_line = out.len();
                    }
                    out.push(Line::from(format!(
                        "   {}     synthetic presets ›  {}",
                        if cursor { ">" } else { " " },
                        match picked.len() {
                            0 => "(none picked)".to_string(),
                            _ => picked.join(", "),
                        }
                    )));
                    if active {
                        let about = match self.subjects.get(self.row[i]) {
                            Some(s) => s.about.clone(),
                            None => format!(
                                "{} generated graphs; space or enter opens the list",
                                self.synth.len()
                            ),
                        };
                        out.push(Line::from(Span::styled(format!("     {about}"), dim)));
                    }
                }
                _ => {
                    let rows: Vec<(String, bool)> = match pane {
                        Pane::Tools => self.tools.clone(),
                        Pane::Workloads => self
                            .workloads
                            .iter()
                            .map(|(w, on)| (w.name().to_string(), *on))
                            .collect(),
                        _ => Size::ALL
                            .iter()
                            .map(|s| (format!("{:<7} {}", s.name(), s.describe()), *s == self.size))
                            .collect(),
                    };
                    for (r, (name, on)) in rows.iter().enumerate() {
                        let cursor = active && self.row[i] == r;
                        if cursor {
                            cursor_line = out.len();
                        }
                        out.push(row_line(cursor, *on, name));
                    }
                }
            }
            out.push(Line::from(""));
        }
        let plan = self.plan();
        out.push(Line::from(Span::styled("it will run", bold)));
        if plan.is_runnable() {
            for step in plan.steps() {
                out.push(Line::from(format!("   {}", step.command_line())));
            }
        } else {
            out.push(Line::from(
                "   nothing: pick at least one subject and one workload",
            ));
        }
        if self.confirming && !self.existing.is_empty() {
            let dir = plan.results().display().to_string();
            out.push(Line::from(""));
            out.push(Line::from(Span::styled(
                format!("{dir} already holds results"),
                bold,
            )));
            for e in &self.existing {
                out.push(Line::from(format!("   {e}")));
            }
            out.push(Line::from(Span::styled(
                format!("   r continues them; f moves {dir} aside and starts fresh"),
                dim,
            )));
        }
        (out, cursor_line)
    }

    /// The key help, pinned below the scrolling part.
    fn help(&self) -> &'static str {
        match (self.confirming, self.in_synth) {
            (true, _) if !self.existing.is_empty() => {
                "r resume those results   f move them aside and start fresh   n back   q quit"
            }
            (true, _) => "run these? y to start, n to go back, q to quit",
            (false, true) => "space toggle   up/down move   esc back   q quit",
            (false, false) => "space toggle   tab/up/down move   enter review   q quit",
        }
    }
}

/// Where to start drawing `total` lines in `height` rows so `cursor` stays in
/// view with a little context around it, moving no further than it must;
/// `to_end` shows the last screenful, which is where the plan is.
fn scroll_for(current: usize, cursor: usize, total: usize, height: usize, to_end: bool) -> usize {
    const MARGIN: usize = 2;
    let last = total.saturating_sub(height);
    if to_end || height == 0 {
        return last;
    }
    let mut top = current;
    if cursor < top + MARGIN {
        top = cursor.saturating_sub(MARGIN);
    }
    if cursor + MARGIN >= top + height {
        top = (cursor + MARGIN + 1).saturating_sub(height);
    }
    top.min(last)
}

/// A repository subject's one line: its notes, and what a run of it walks.
fn about_repo(r: &cambium_bench::manifest::RepoEntry) -> String {
    let mut parts = Vec::new();
    if !r.notes.is_empty() {
        parts.push(r.notes.clone());
    }
    let pin: String = r.pin.chars().take(12).collect();
    parts.push(format!("pin {pin}, history window {}", r.history_window));
    if let Some(v) = &r.bazel_version {
        parts.push(format!("Bazel {v}"));
    }
    parts.join(" · ")
}

/// A synthetic preset's one line: how big, and what its shape changes.
fn about_preset(name: &str) -> String {
    let (size, shape) = match name.split_once('-') {
        Some((s, sh)) => (s, Some(sh)),
        None => (name, None),
    };
    let Some(spec) = cambium_synth::GraphSpec::preset(name) else {
        return String::new();
    };
    let shape = match shape {
        None => "the default shape",
        Some("hubby") => "hubby: most rules depend on a few hub targets",
        Some("deep") => "deep: four times the layers, more deps per rule",
        Some("flat") => "flat: three layers, little preferential attachment",
        Some("wide-packages") => "wide packages: many rules per package, shallow paths",
        Some(other) => other,
    };
    format!(
        "{size}: ~{} rules in {} layers, {} .bzl files; {shape}",
        spec.targets, spec.layers, spec.bzl_files
    )
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
    let Some((plan, start)) = compose(&manifest)? else {
        return Ok(crate::EXIT_OK);
    };
    let moved = match start {
        Start::Fresh => plan.move_results_aside()?,
        Start::AsPlanned => None,
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
    if let Some(to) = moved {
        runner.push_tail(format!("moved the previous results to {}", to.display()));
    }
    screen(
        &Screen {
            results_dir: results,
            subjects,
            interval: Duration::from_secs(1),
            engines: crate::watch::engine_ids(ctx),
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
fn compose(manifest: &Manifest) -> Result<Option<(Plan, Start)>> {
    let _guard = crate::watch::TerminalGuard::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut c = Compose::new(manifest);
    loop {
        term.draw(|f| {
            let [body, foot] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
            let (lines, cursor) = c.lines();
            c.scroll = scroll_for(
                c.scroll,
                cursor,
                lines.len(),
                body.height as usize,
                c.confirming,
            );
            let top = u16::try_from(c.scroll).unwrap_or(u16::MAX);
            f.render_widget(Paragraph::new(Text::from(lines)).scroll((top, 0)), body);
            f.render_widget(
                Paragraph::new(Span::styled(
                    c.help(),
                    Style::default().add_modifier(Modifier::DIM),
                )),
                foot,
            );
        })?;
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if c.in_synth {
            match key.code {
                KeyCode::Char('q') => return Ok(None),
                KeyCode::Esc
                | KeyCode::Left
                | KeyCode::Backspace
                | KeyCode::Enter
                | KeyCode::Char('h') => c.in_synth = false,
                KeyCode::Char(' ') => c.toggle(),
                KeyCode::Tab => c.next_pane(1),
                KeyCode::BackTab => c.next_pane(-1),
                KeyCode::Down | KeyCode::Char('j') => c.move_by(1),
                KeyCode::Up | KeyCode::Char('k') => c.move_by(-1),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char(' ') if !c.confirming => c.toggle(),
            KeyCode::Enter | KeyCode::Right if !c.confirming && c.on_synth_entry() => c.toggle(),
            KeyCode::Tab => c.next_pane(1),
            KeyCode::BackTab => c.next_pane(-1),
            KeyCode::Down | KeyCode::Char('j') => c.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => c.move_by(-1),
            KeyCode::Enter if !c.confirming => c.review(),
            KeyCode::Char('y') | KeyCode::Enter if c.confirming && c.existing.is_empty() => {
                return Ok(Some((c.plan(), Start::AsPlanned)))
            }
            KeyCode::Char('r') if c.confirming && !c.existing.is_empty() => {
                let mut plan = c.plan();
                plan.resume = true;
                return Ok(Some((plan, Start::AsPlanned)));
            }
            KeyCode::Char('f') if c.confirming && !c.existing.is_empty() => {
                return Ok(Some((c.plan(), Start::Fresh)))
            }
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
             history_window = 300\nnotes = \"small, clean Java\"\n\
             [[repo]]\nid = \"gone\"\nurl = \"u\"\npin = \"{0}\"\nskip = true\n",
            "a".repeat(40)
        ))
        .unwrap()
    }

    fn text(c: &Compose) -> Vec<String> {
        c.lines()
            .0
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    #[test]
    fn the_screen_offers_the_manifest_and_the_synthetic_presets() {
        let c = Compose::new(&manifest());
        let names: Vec<&str> = c.subjects.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["bazel"], "a skipped subject is not offered");
        assert!(c.synth.iter().all(|s| s.name.starts_with("synth:")));
        assert_eq!(c.synth.len(), cambium_synth::GraphSpec::PRESETS.len());
        // The presets are one row on the main screen, not thirty.
        let lines = text(&c);
        assert!(
            lines.iter().any(|l| l.contains("synthetic presets ›")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("synth:tiny-deep")),
            "{lines:?}"
        );
        // Nothing is selected for you, but the tools and workloads are: the
        // question the screen asks is which repositories.
        assert!(c.plan().subjects.is_empty());
        assert_eq!(c.plan().tools, ["bazel-diff", "tango"]);
        assert_eq!(c.plan().steps, StepKind::ALL);
        assert_eq!(c.size, Size::Medium);
    }

    /// The highlighted subject says what it is, under the explanation of
    /// what a subject is.
    #[test]
    fn the_subjects_pane_explains_itself_and_the_highlighted_subject() {
        let c = Compose::new(&manifest());
        let lines = text(&c);
        assert!(
            lines.iter().any(|l| l.contains("What to benchmark")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("small, clean Java") && l.contains("history window 300")),
            "{lines:?}"
        );
    }

    #[test]
    fn the_presets_open_in_their_own_list_and_join_the_plan() {
        let mut c = Compose::new(&manifest());
        c.move_by(1);
        assert!(c.on_synth_entry());
        c.toggle();
        assert!(c.in_synth, "the presets row opens the list");
        c.move_by(1);
        c.toggle();
        assert_eq!(c.plan().subjects, ["synth:small"]);
        let lines = text(&c);
        assert!(
            lines.iter().any(|l| l.contains("small: ~2000 rules")),
            "{lines:?}"
        );
        c.in_synth = false;
        let lines = text(&c);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("synthetic presets ›  synth:small")),
            "{lines:?}"
        );
    }

    /// A terminal shorter than the screen still shows the cursor, and review
    /// shows the plan at the bottom.
    #[test]
    fn the_screen_scrolls_to_keep_the_cursor_in_view() {
        assert_eq!(scroll_for(0, 5, 100, 20, false), 0, "already visible");
        assert_eq!(scroll_for(0, 40, 100, 20, false), 23);
        assert_eq!(
            scroll_for(23, 30, 100, 20, false),
            23,
            "no further than it must"
        );
        assert_eq!(scroll_for(23, 10, 100, 20, false), 8);
        assert_eq!(scroll_for(0, 99, 100, 20, false), 80, "never past the end");
        assert_eq!(scroll_for(0, 3, 100, 20, true), 80, "review shows the plan");
        assert_eq!(
            scroll_for(7, 3, 10, 20, false),
            0,
            "a screen that fits never scrolls"
        );

        let mut c = Compose::new(&manifest());
        c.next_pane(3);
        c.move_by(2);
        let (lines, cursor) = c.lines();
        let line: String = lines[cursor]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(line.contains("large"), "{line}");
    }

    /// Review names the results a plan would run over, and only then offers
    /// resume and move-aside instead of a plain start.
    #[test]
    fn review_offers_resume_or_fresh_over_recorded_results() {
        let dir = tempfile::tempdir().unwrap();
        let results = dir.path().join("bench-results");
        let mut c = Compose::new(&manifest());
        c.results_dir = Some(results.clone());
        c.toggle();
        c.review();
        assert!(c.confirming && c.existing.is_empty());
        assert!(c.help().contains("y to start"));

        std::fs::create_dir_all(results.join("bazel/history")).unwrap();
        std::fs::write(results.join("bazel/history/history-abc.json"), "{}").unwrap();
        c.review();
        assert_eq!(c.existing, ["bazel: history"]);
        assert!(c.help().contains("r resume") && c.help().contains("f move"));
        assert!(text(&c).iter().any(|l| l.contains("already holds results")));
    }

    /// The screen composes the library's plan, so a confirmed large run is
    /// the published run.
    #[test]
    fn the_confirmed_plan_is_the_library_plan() {
        let mut c = Compose::new(&manifest());
        c.toggle();
        c.next_pane(3);
        c.move_by(5);
        c.toggle();
        let plan = c.plan();
        assert_eq!(plan.size, Size::Large);
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
        assert_eq!(c.size, Size::Small, "the cursor clamps at the top");
        // The plan is on the screen before anything runs.
        let lines = text(&c);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("cambium-bench --log info history bazel")),
            "{lines:?}"
        );
    }
}
