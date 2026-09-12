//! The one renderer. `live` decides whether there is a bottom line and
//! whether a finished phase is rewritten in place; everything else is
//! identical, so a terminal and a pipe see the same rows byte for byte.
//!
//! The grammar is the same for every command:
//!
//! ```text
//! cambium profile  envoy  e6b1317..ea3b186  30 first-parent commits
//!   worktree ............... ready                             210 ms
//!   cold index ............. 68,412 targets                     2.5 s
//!   commits ................ 30 walked, 1 global               12.4 s
//!     <rows>
//! ```

use std::io::Write;
use std::time::Instant;

use crate::report;
use crate::sim::{self, Ev, Row};
use crate::term::{colour, stderr_tty, Paint};

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Where the leader dots after a phase name stop.
const DOT_COL: usize = 24;
/// Where a phase result starts, and where its time ends.
const RESULT_COL: usize = 26;
const TIME_END: usize = 74;

/// Fixed row columns. The commit cell takes whatever is left inside the cap.
const SHA_W: usize = 10;
const DATE_W: usize = 11;
const TIME_W: usize = 8;
const AFFECTED_W: usize = 8;
/// Everything to the right of the commit cell, plus the row's indent.
const TAIL: usize = 4 + 1 + TIME_W + 1 + AFFECTED_W + 2 + report::INDEX_COL;
/// A row never grows past this, so the numbers stay near the text on a wide
/// terminal instead of being flung at the right edge.
const ROW_CAP: usize = 96;

/// Terminal width, or the 80 columns a pipe is assumed to have.
fn width() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w as usize)
        .filter(|w| *w >= 40)
        .unwrap_or(80)
}

/// Width of the sha, date and subject taken together.
fn cell_width() -> usize {
    width().min(ROW_CAP).saturating_sub(TAIL).max(24)
}

fn subject_width(style: report::CommitStyle) -> usize {
    let used = match style {
        report::CommitStyle::Columns => SHA_W + 1 + DATE_W + 1,
        report::CommitStyle::Compact => 7 + 1 + DATE_W + 1,
    };
    cell_width().saturating_sub(used).max(12)
}

fn truncate(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        format!("{s}{:pad$}", "", pad = n - count)
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Everything that writes to stderr goes through here, so the live line is
/// always erased before anything else is printed.
struct Printer {
    live: bool,
    commits: report::CommitStyle,
    p: Paint,
    shown: bool,
    /// True while the last line printed is a phase line with no result yet,
    /// so the result can still be written over it.
    rewritable: bool,
    current: String,
    start: Instant,
    frame: usize,
}

impl Printer {
    fn new(live: bool, commits: report::CommitStyle) -> Self {
        let tty = stderr_tty();
        Self {
            live: live && tty,
            commits,
            p: Paint::new(colour(tty)),
            shown: false,
            rewritable: false,
            current: String::new(),
            start: Instant::now(),
            frame: 0,
        }
    }

    fn erase(&mut self) {
        if self.shown {
            eprint!("\r\x1b[2K");
            self.shown = false;
        }
    }

    fn line(&mut self, text: &str) {
        self.erase();
        self.rewritable = false;
        eprintln!("{text}");
    }

    fn phase_start(&mut self, name: &'static str, detail: Option<String>) {
        self.erase();
        let dots = DOT_COL.saturating_sub(name.chars().count() + 3);
        let head = format!("  {} {}", name, ".".repeat(dots));
        let text = match detail {
            Some(d) => format!("{head} {}", self.p.dim(&d)),
            None => head.clone(),
        };
        eprintln!("{text}");
        // In live mode the result is written back over this same line, as
        // long as nothing else is printed in between.
        self.rewritable = self.live;
    }

    fn phase_end(&mut self, name: &'static str, result: &str, ms: u64) {
        self.erase();
        let rewrite = self.rewritable;
        self.rewritable = false;
        let dots = DOT_COL.saturating_sub(name.chars().count() + 3);
        let head = format!("  {} {}", name, ".".repeat(dots));
        let pad = RESULT_COL.saturating_sub(head.chars().count() + 1);
        let time = report::human_ms(ms);
        let plain_len = head.chars().count() + 1 + pad + result.chars().count();
        let gap = TIME_END
            .saturating_sub(plain_len + time.chars().count())
            .max(2);
        let body = format!(
            "{head} {:pad$}{}{:gap$}{}",
            "",
            self.p.green(result),
            "",
            self.p.dim(&time),
            pad = pad,
            gap = gap
        );
        if rewrite {
            // One line up, wipe it, put the finished line in its place.
            eprint!("\x1b[1A\r\x1b[2K{body}\n");
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{body}");
        }
    }

    fn phase_skipped(&mut self, name: &'static str, reason: &str) {
        self.erase();
        self.rewritable = false;
        let dots = DOT_COL.saturating_sub(name.chars().count() + 3);
        let head = format!("  {} {}", name, ".".repeat(dots));
        let pad = RESULT_COL.saturating_sub(head.chars().count() + 1);
        eprintln!(
            "{head} {:pad$}{}",
            "",
            self.p.dim(&format!("skipped ({reason})")),
            pad = pad
        );
    }

    fn row_header(&mut self) {
        let cell = match self.commits {
            report::CommitStyle::Columns => format!(
                "{:<w0$} {:<w1$} {:<w2$}",
                "commit",
                "date",
                "subject",
                w0 = SHA_W,
                w1 = DATE_W,
                w2 = subject_width(self.commits),
            ),
            report::CommitStyle::Compact => {
                format!("{:<w$}", "commit", w = cell_width())
            }
        };
        let text = format!(
            "    {cell} {:>w3$} {:>w4$}  {}",
            "time",
            "affected",
            "index",
            w3 = TIME_W,
            w4 = AFFECTED_W,
        );
        let painted = self.p.dim(text.trim_end());
        self.line(&painted);
    }

    /// The sha, date and subject, laid out the way `--commit` asks.
    fn commit_cell(&self, r: &Row) -> String {
        let subject = truncate(&r.subject, subject_width(self.commits));
        match self.commits {
            report::CommitStyle::Columns => format!(
                "{} {:<w1$} {}",
                self.p
                    .bold(&format!("{:<w0$}", report::short(&r.sha), w0 = SHA_W)),
                r.date,
                subject,
                w1 = DATE_W,
            ),
            report::CommitStyle::Compact => format!(
                "{} {} {}",
                self.p.bold(&r.sha[..7]),
                self.p.dim(&format!("{:<w$}", r.date, w = DATE_W)),
                subject,
            ),
        }
    }

    fn row(&mut self, r: &Row) {
        let (cell, dim) = report::splice_col(&r.splice);
        let cell = if dim {
            self.p.dim(&cell)
        } else {
            self.p.cyan(&cell)
        };
        let text = format!(
            "    {} {:>w3$} {:>w4$}  {}{}",
            self.commit_cell(r),
            report::human_ms(r.wall_ms),
            r.affected,
            cell,
            match r.global {
                Some(g) => format!("  global: {g}"),
                None => String::new(),
            },
            w3 = TIME_W,
            w4 = AFFECTED_W,
        );
        self.line(text.trim_end());
    }

    /// The one live line, redrawn on a tick.
    fn tick(&mut self) {
        if !self.live || self.current.is_empty() {
            return;
        }
        self.frame += 1;
        let secs = self.start.elapsed().as_secs();
        let text = truncate(&self.current, width().saturating_sub(16));
        eprint!(
            "\r\x1b[2K{} {}   {}",
            self.p.cyan(&SPINNER[self.frame % SPINNER.len()].to_string()),
            text.trim_end(),
            self.p.dim(&format!("{}:{:02}", secs / 60, secs % 60)),
        );
        let _ = std::io::stderr().flush();
        self.shown = true;
    }

    fn finish(&mut self) {
        self.erase();
        self.rewritable = false;
    }
}

/// Drains one event stream through the printer. Returns when `Done` lands.
fn drive(rx: std::sync::mpsc::Receiver<Ev>, pr: &mut Printer) {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(80)) {
            Ok(Ev::Header { verb, range, note }) => {
                let text = format!(
                    "{} {}  {}  {}  {}",
                    pr.p.bold("cambium"),
                    pr.p.bold(verb),
                    sim::REPO,
                    range,
                    pr.p.dim(&note)
                );
                pr.line(&text);
            }
            Ok(Ev::PhaseStart { name, detail }) => pr.phase_start(name, detail),
            Ok(Ev::PhaseEnd { name, result, ms }) => pr.phase_end(name, &result, ms),
            Ok(Ev::PhaseSkipped { name, reason }) => pr.phase_skipped(name, &reason),
            Ok(Ev::RowHeader) => pr.row_header(),
            Ok(Ev::Row(r)) => pr.row(&r),
            Ok(Ev::Log(line)) => {
                let painted = if line.starts_with("warning") {
                    pr.p.yellow(&format!("    {line}"))
                } else {
                    pr.p.dim(&format!("    {line}"))
                };
                pr.line(&painted);
            }
            Ok(Ev::Current(what)) => pr.current = what,
            Ok(Ev::Milestone(text)) => {
                // Live mode is already showing this on the live line, with a
                // count that ticks; a pipe gets it once, as a line.
                if !pr.live {
                    let painted = pr.p.dim(&format!("    {text}"));
                    pr.line(&painted);
                }
            }
            Ok(Ev::Done) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        pr.tick();
    }
    pr.finish();
}

/// `cambi profile`: the long walk, then the report on stdout.
pub fn profile(speed: f32, style: report::Style, live: bool, commits: report::CommitStyle) {
    let s = sim::build();
    let rx = sim::spawn(s.clone(), speed);
    let mut pr = Printer::new(live, commits);
    drive(rx, &mut pr);
    println!();
    report::print(&s, style, colour(crate::term::stdout_tty()), commits);
}

/// How the targets-by-depth distribution is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Hist {
    /// One horizontal block bar per depth, with the count after it.
    Bars,
    /// A vertical column chart, values above each column.
    Columns,
}

/// `cambi affected`: three phases, then the answer on stdout.
pub fn affected(
    speed: f32,
    live: bool,
    all: bool,
    global: bool,
    commits: report::CommitStyle,
    hist: Hist,
    found: sim::IndexFound,
    depths: usize,
) {
    let rx = sim::spawn_affected(found, global, speed);
    let mut pr = Printer::new(live, commits);
    drive(rx, &mut pr);

    let a = sim::affected_answer(global, depths);
    let p = Paint::new(colour(crate::term::stdout_tty()));
    println!();
    if let Some(reason) = a.global {
        println!("everything is affected: {reason}");
        return;
    }
    println!(
        "{}  ({} tests)  max depth {}",
        p.bold(&format!("{} affected targets", a.targets)),
        a.tests,
        a.max_depth
    );
    println!();
    for label in &a.labels {
        println!("{label}");
    }
    let rest = a.targets as usize - a.labels.len();
    if !all && rest > 0 {
        println!("{}", p.dim(&format!("… {rest} more")));
    }
    println!();
    match hist {
        Hist::Bars => {
            println!("  {:>5}  {}", "depth", "targets");
            let max = a.by_depth.iter().copied().max().unwrap_or(1).max(1);
            for (depth, count) in a.by_depth.iter().enumerate() {
                println!(
                    "  {:>5}  {} {}",
                    depth,
                    bar(*count, max, 30),
                    p.dim(&count.to_string())
                );
            }
        }
        Hist::Columns => {
            for line in columns(&a.by_depth) {
                println!("{line}");
            }
            println!("  {}", p.dim(&format!("targets by depth 0..{}", a.max_depth)));
        }
    }
}

/// Rows of bar in a column chart.
const COL_HEIGHT: usize = 8;
/// Left indent, shared by the chart and everything around it.
const COL_INDENT: usize = 2;
/// Partial column tops, one eighth to seven eighths.
const EIGHTHS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];

/// Column width, bar width, and how often a value and a depth label are
/// printed. Labels thin out before the bars do, and the bars only shrink
/// once thinning is not enough.
fn column_layout(n: usize, w_val: usize, w_dep: usize) -> (usize, usize, usize, usize) {
    let budget = ROW_CAP.saturating_sub(COL_INDENT);
    let mut last = (3usize, 3usize, 1usize, 1usize);
    for (val_every, dep_every, bar_w) in [
        (1usize, 1usize, 3usize),
        (1, 1, 2),
        (2, 1, 2),
        (2, 2, 2),
        (2, 2, 1),
    ] {
        // A label printed every k columns may borrow the k columns it spans.
        let cell = (bar_w + 1)
            .max((w_val + 1).div_ceil(val_every))
            .max((w_dep + 1).div_ceil(dep_every));
        last = (cell, bar_w, val_every, dep_every);
        if n * cell <= budget + 1 {
            return last;
        }
    }
    last
}

/// A vertical column chart: one column per depth, the value directly above
/// each column, depth labels under an axis line. Plain text throughout, so
/// it looks the same down a pipe as it does on a terminal.
fn columns(counts: &[u32]) -> Vec<String> {
    if counts.is_empty() {
        return Vec::new();
    }
    let n = counts.len();
    let values: Vec<String> = counts.iter().map(|c| c.to_string()).collect();
    let depths: Vec<String> = (0..n).map(|d| d.to_string()).collect();
    let w_val = values.iter().map(|v| v.chars().count()).max().unwrap_or(1);
    let w_dep = depths.iter().map(|d| d.chars().count()).max().unwrap_or(1);
    let (cell, bar_w, val_every, dep_every) = column_layout(n, w_val, w_dep);
    let content = cell - 1;
    let bar_w = bar_w.min(content);
    let total = COL_INDENT + n * cell - 1;

    // One row above the tallest column, so its value has somewhere to sit.
    let mut grid = vec![vec![' '; total]; COL_HEIGHT + 1];
    let max = counts.iter().copied().max().unwrap_or(1).max(1);

    for (i, &count) in counts.iter().enumerate() {
        let x0 = COL_INDENT + i * cell;
        let bar_x = x0 + (content - bar_w) / 2;
        let mut eighths =
            (count as f64 / max as f64 * (COL_HEIGHT * 8) as f64).round() as usize;
        if count > 0 && eighths == 0 {
            eighths = 1;
        }
        let full = eighths / 8;
        let frac = eighths % 8;
        let top = 1 + COL_HEIGHT - full - usize::from(frac > 0);
        for row in (top + usize::from(frac > 0))..=COL_HEIGHT {
            for dx in 0..bar_w {
                grid[row][bar_x + dx] = '█';
            }
        }
        if frac > 0 {
            for dx in 0..bar_w {
                grid[top][bar_x + dx] = EIGHTHS[frac - 1];
            }
        }
        if i % val_every == 0 {
            let v = &values[i];
            let end = bar_x + bar_w;
            let start = end.saturating_sub(v.chars().count());
            let row = top.saturating_sub(1);
            for (k, ch) in v.chars().enumerate() {
                if start + k < total {
                    grid[row][start + k] = ch;
                }
            }
        }
    }

    let mut out: Vec<String> = grid
        .into_iter()
        .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
        .collect();
    // Drop blank rows above the tallest value.
    while out.first().map(|l| l.is_empty()).unwrap_or(false) {
        out.remove(0);
    }

    let mut axis = " ".repeat(COL_INDENT);
    axis.push_str(&"─".repeat(n * cell - 1));
    out.push(axis);

    let mut labels = vec![' '; total];
    for (i, d) in depths.iter().enumerate() {
        if i % dep_every != 0 {
            continue;
        }
        let x0 = COL_INDENT + i * cell;
        let bar_x = x0 + (content - bar_w) / 2;
        let w = d.chars().count();
        let start = (bar_x + bar_w / 2).saturating_sub(w / 2).min(total - w);
        for (k, ch) in d.chars().enumerate() {
            labels[start + k] = ch;
        }
    }
    out.push(labels.into_iter().collect::<String>().trim_end().to_string());
    out
}

/// A block bar, with an eighth-width tail so short bars still differ.
fn bar(count: u32, max: u32, cols: usize) -> String {
    let eighths = (count as f64 / max as f64 * cols as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rest = eighths % 8;
    let mut s = "█".repeat(full);
    if rest >= 4 {
        s.push('▌');
    } else if rest > 0 {
        s.push('▏');
    }
    format!("{s:<cols$}", cols = cols)
}
