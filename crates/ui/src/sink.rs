//! The four sinks. Every verb writes events; only this file writes bytes.

use std::io::Write;
use std::time::Instant;

use crate::event::{Cell, Column, Event, Level};
use crate::mode::{Kind, OutputMode};
use crate::paint::Paint;
use crate::row;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Where the leader dots after a phase name stop.
const DOT_COL: usize = 24;
/// Where a phase result starts, and where its time ends.
const RESULT_COL: usize = 26;
const TIME_END: usize = 74;

/// Where a run says what it is doing.
pub trait Sink {
    fn emit(&mut self, event: Event);
    /// Redraws the live line without a new event, for a phase that is slow
    /// and silent. A sink without a live line does nothing.
    fn tick(&mut self) {}
}

/// The sink `mode` asks for, writing to stderr.
pub fn boxed(mode: OutputMode) -> Box<dyn Sink> {
    match mode.kind {
        Kind::Stream => Box::new(Stream::to_stderr(mode)),
        Kind::Plain => Box::new(Plain::to_stderr(mode)),
        Kind::Jsonl => Box::new(Jsonl::to_stderr()),
        Kind::Null => Box::new(Null),
    }
}

/// Nothing at all: `--quiet`.
pub struct Null;

impl Sink for Null {
    fn emit(&mut self, _event: Event) {}
}

/// The lines both text sinks print. `live` is the only difference between
/// them, and it changes no row.
struct Text<W: Write> {
    w: W,
    live: bool,
    p: Paint,
    width: usize,
    columns: Vec<Column>,
    /// The live line is on screen and must be erased before anything else.
    shown: bool,
    /// The last line printed is a phase line with no result yet, so the
    /// result can still be written over it.
    rewritable: bool,
    current: String,
    start: Instant,
    frame: usize,
}

impl<W: Write> Text<W> {
    fn new(w: W, mode: OutputMode, live: bool) -> Self {
        Self {
            w,
            live,
            p: Paint::new(mode.colour),
            width: mode.width,
            columns: Vec::new(),
            shown: false,
            rewritable: false,
            current: String::new(),
            start: Instant::now(),
            frame: 0,
        }
    }

    fn erase(&mut self) {
        if self.shown {
            let _ = write!(self.w, "\r\x1b[2K");
            self.shown = false;
        }
    }

    fn line(&mut self, text: &str) {
        self.erase();
        self.rewritable = false;
        let _ = writeln!(self.w, "{text}");
    }

    fn head(&self, name: &str) -> String {
        let dots = DOT_COL.saturating_sub(name.chars().count() + 3);
        format!("  {} {}", name, ".".repeat(dots))
    }

    fn phase_start(&mut self, name: &str, detail: Option<String>) {
        self.erase();
        let head = self.head(name);
        let text = match detail {
            Some(d) => format!("{head} {}", self.p.dim(&d)),
            None => head,
        };
        let _ = writeln!(self.w, "{text}");
        self.rewritable = self.live;
    }

    fn phase_end(&mut self, name: &str, result: &str, elapsed_ms: u64) {
        self.erase();
        let rewrite = self.rewritable;
        self.rewritable = false;
        let head = self.head(name);
        let pad = RESULT_COL.saturating_sub(head.chars().count() + 1);
        let time = crate::report::human_ms(elapsed_ms);
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
        );
        if rewrite {
            // One line up, wipe it, put the finished line in its place.
            let _ = write!(self.w, "\x1b[1A\r\x1b[2K{body}\n");
            let _ = self.w.flush();
        } else {
            let _ = writeln!(self.w, "{body}");
        }
    }

    fn phase_skipped(&mut self, name: &str, reason: &str) {
        self.erase();
        self.rewritable = false;
        let head = self.head(name);
        let pad = RESULT_COL.saturating_sub(head.chars().count() + 1);
        let body = format!(
            "{head} {:pad$}{}",
            "",
            self.p.dim(&format!("skipped ({reason})")),
        );
        let _ = writeln!(self.w, "{body}");
    }

    fn tick_line(&mut self) {
        if !self.live || self.current.is_empty() {
            return;
        }
        self.frame += 1;
        let secs = self.start.elapsed().as_secs();
        let text = row::fit(
            &self.current,
            self.width.saturating_sub(16),
            crate::event::Align::Left,
        );
        let _ = write!(
            self.w,
            "\r\x1b[2K{} {}   {}",
            self.p
                .cyan(&SPINNER[self.frame % SPINNER.len()].to_string()),
            text.trim_end(),
            self.p.dim(&format!("{}:{:02}", secs / 60, secs % 60)),
        );
        let _ = self.w.flush();
        self.shown = true;
    }

    fn emit(&mut self, event: Event) {
        match event {
            Event::Header {
                verb,
                repo,
                range,
                note,
            } => {
                let mut text = format!("{} {}", self.p.bold("cambium"), self.p.bold(&verb));
                for part in [repo, range] {
                    if !part.is_empty() {
                        text.push_str(&format!("  {part}"));
                    }
                }
                if !note.is_empty() {
                    text.push_str(&format!("  {}", self.p.dim(&note)));
                }
                self.line(&text);
            }
            Event::PhaseStart { name, detail } => self.phase_start(&name, detail),
            Event::PhaseEnd {
                name,
                result,
                elapsed_ms,
            } => self.phase_end(&name, &result, elapsed_ms),
            Event::PhaseSkipped { name, reason } => self.phase_skipped(&name, &reason),
            Event::RowHeader(columns) => {
                let text = row::render_header(&columns, self.width, &self.p);
                self.columns = columns;
                self.line(&text);
            }
            Event::Row(cells) => {
                let text = row::render(&self.columns, &cells, self.width, &self.p);
                self.line(&text);
            }
            Event::Milestone(text) => {
                // The live line is already showing this, with a count that
                // ticks; a pipe gets it once, as a line.
                if !self.live {
                    let painted = self.p.dim(&format!("    {text}"));
                    self.line(&painted);
                }
            }
            Event::Current(what) => {
                self.current = what;
                self.tick_line();
            }
            Event::Log(level, text) => {
                let painted = match level {
                    Level::Warning => self.p.yellow(&format!("    {text}")),
                    Level::Info => self.p.dim(&format!("    {text}")),
                };
                self.line(&painted);
            }
            Event::Done => {
                self.erase();
                self.rewritable = false;
                let _ = self.w.flush();
            }
        }
    }
}

/// A terminal: one live bottom line, and a finished phase written back over
/// its own start line as long as it has printed no rows.
pub struct Stream<W: Write = std::io::Stderr>(Text<W>);

impl Stream<std::io::Stderr> {
    pub fn to_stderr(mode: OutputMode) -> Self {
        Self(Text::new(std::io::stderr(), mode, true))
    }
}

impl<W: Write> Stream<W> {
    pub fn to_writer(w: W, mode: OutputMode) -> Self {
        Self(Text::new(w, mode, true))
    }
}

impl<W: Write> Sink for Stream<W> {
    fn emit(&mut self, event: Event) {
        self.0.emit(event)
    }
    fn tick(&mut self) {
        self.0.tick_line()
    }
}

/// A pipe or a CI log: the same bytes, minus the live line. Every phase
/// prints both its lines, so the log is never silent, and a milestone the
/// live line would have carried is printed once, dim.
pub struct Plain<W: Write = std::io::Stderr>(Text<W>);

impl Plain<std::io::Stderr> {
    pub fn to_stderr(mode: OutputMode) -> Self {
        Self(Text::new(std::io::stderr(), mode, false))
    }
}

impl<W: Write> Plain<W> {
    pub fn to_writer(w: W, mode: OutputMode) -> Self {
        Self(Text::new(w, mode, false))
    }
}

impl<W: Write> Sink for Plain<W> {
    fn emit(&mut self, event: Event) {
        self.0.emit(event)
    }
}

/// One JSON object per event, for `--format jsonl`. On stderr, because it is
/// progress: the answer still goes to stdout on its own.
pub struct Jsonl<W: Write = std::io::Stderr> {
    w: W,
    columns: Vec<Column>,
}

impl Jsonl<std::io::Stderr> {
    pub fn to_stderr() -> Self {
        Self {
            w: std::io::stderr(),
            columns: Vec::new(),
        }
    }
}

impl<W: Write> Jsonl<W> {
    pub fn to_writer(w: W) -> Self {
        Self {
            w,
            columns: Vec::new(),
        }
    }
}

impl<W: Write> Sink for Jsonl<W> {
    fn emit(&mut self, event: Event) {
        let value = match &event {
            Event::Row(cells) => json_row(&self.columns, cells),
            _ => json(&event),
        };
        if let Event::RowHeader(columns) = event {
            self.columns = columns;
        }
        if let Ok(line) = serde_json::to_string(&value) {
            let _ = writeln!(self.w, "{line}");
        }
    }
}

fn json(event: &Event) -> serde_json::Value {
    use serde_json::json;
    match event {
        Event::Header {
            verb,
            repo,
            range,
            note,
        } => json!({"event": "header", "verb": verb, "repo": repo, "range": range, "note": note}),
        Event::PhaseStart { name, detail } => {
            json!({"event": "phase_start", "name": name, "detail": detail})
        }
        Event::PhaseEnd {
            name,
            result,
            elapsed_ms,
        } => json!({
            "event": "phase_end", "name": name, "result": result, "elapsed_ms": elapsed_ms
        }),
        Event::PhaseSkipped { name, reason } => {
            json!({"event": "phase_skipped", "name": name, "reason": reason})
        }
        Event::RowHeader(columns) => json!({
            "event": "row_header",
            "columns": columns.iter().map(|c| c.title.clone()).collect::<Vec<_>>(),
        }),
        Event::Row(_) => json!({"event": "row"}),
        Event::Milestone(text) => json!({"event": "milestone", "text": text}),
        Event::Current(text) => json!({"event": "current", "text": text}),
        Event::Log(level, text) => {
            json!({"event": "log", "level": level.as_str(), "text": text})
        }
        Event::Done => json!({"event": "done"}),
    }
}

/// A row as an object keyed by its column titles, so a watcher reads fields
/// rather than columns.
fn json_row(columns: &[Column], cells: &[Cell]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("event".into(), "row".into());
    for (i, cell) in cells.iter().enumerate() {
        let key = columns
            .get(i)
            .map(|c| c.title.clone())
            .unwrap_or_else(|| format!("column_{i}"));
        match cell {
            Cell::Text(s) | Cell::Styled(s, _) => {
                map.insert(key, serde_json::Value::String(s.clone()));
            }
            Cell::Commit { sha, date, subject } => {
                map.insert(key, serde_json::Value::String(sha.clone()));
                map.insert("date".into(), serde_json::Value::String(date.clone()));
                map.insert("subject".into(), serde_json::Value::String(subject.clone()));
            }
        }
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Align, Style};
    use crate::mode::Kind;

    fn mode(colour: bool) -> OutputMode {
        OutputMode {
            kind: Kind::Plain,
            live: false,
            colour,
            width: 96,
        }
    }

    fn script() -> Vec<Event> {
        let columns = vec![
            Column::flex("commit"),
            Column::fixed("time", 8, Align::Right),
            Column::fixed("affected", 8, Align::Right),
            Column::fixed("index", 17, Align::Left),
        ];
        vec![
            Event::Header {
                verb: "profile".into(),
                repo: "envoy".into(),
                range: "e6b1317..cc1b5cb".into(),
                note: "30 first-parent commits".into(),
            },
            Event::PhaseStart {
                name: "worktree".into(),
                detail: Some("checking out".into()),
            },
            Event::PhaseEnd {
                name: "worktree".into(),
                result: "ready".into(),
                elapsed_ms: 210,
            },
            Event::PhaseStart {
                name: "commits".into(),
                detail: Some("30 steps".into()),
            },
            Event::RowHeader(columns),
            Event::Current("e6b1317".into()),
            Event::Row(vec![
                Cell::Commit {
                    sha: "e6b1317b77".into(),
                    date: "09-02 09:14".into(),
                    subject: "tcp_proxy: idle timeout".into(),
                },
                Cell::text("116 ms"),
                Cell::text("375"),
                Cell::styled("~  2625 rewritten", Style::Cyan),
            ]),
            Event::Milestone("none at 3f2a9c1, searching 50 ancestors".into()),
            Event::PhaseEnd {
                name: "commits".into(),
                result: "30 walked, 1 global".into(),
                elapsed_ms: 9_900,
            },
            Event::Done,
        ]
    }

    fn drain(mut sink: impl Sink, out: std::rc::Rc<std::cell::RefCell<Vec<u8>>>) -> String {
        for e in script() {
            sink.emit(e);
        }
        drop(sink);
        String::from_utf8(out.borrow().clone()).expect("utf-8")
    }

    /// A writer the test can read back after the sink has dropped it.
    #[derive(Clone)]
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The rows, with the live line's erase sequences cut off the front of
    /// whichever line they were written into.
    fn rows_of(text: &str) -> Vec<String> {
        text.lines()
            .map(|l| match l.rfind("\x1b[2K") {
                Some(i) => l[i + 4..].to_string(),
                None => l.to_string(),
            })
            .filter(|l| l.starts_with("    "))
            .collect()
    }

    #[test]
    fn a_pipe_and_a_terminal_print_the_same_rows_byte_for_byte() {
        let a = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stream = drain(Stream::to_writer(Shared(a.clone()), mode(false)), a);
        let b = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let plain = drain(Plain::to_writer(Shared(b.clone()), mode(false)), b);

        let stream_rows = rows_of(&stream);
        assert_eq!(stream_rows.len(), 2, "a header and a row: {stream_rows:?}");
        assert!(stream_rows[0].trim_start().starts_with("commit"));
        assert!(
            stream_rows[1].starts_with("    e6b1317 09-02 09:14 tcp_proxy: idle timeout"),
            "{stream_rows:?}"
        );
        assert!(stream_rows[1].ends_with("116 ms      375 ~  2625 rewritten"));
        // The milestone is the one line only the pipe prints.
        let plain_rows = rows_of(&plain);
        assert_eq!(&plain_rows[..2], &stream_rows[..]);
        assert_eq!(plain_rows.len(), 3);
        assert!(plain_rows[2].contains("searching 50 ancestors"));
    }

    #[test]
    fn a_finished_phase_is_rewritten_in_place_only_when_it_printed_no_rows() {
        let a = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let stream = drain(Stream::to_writer(Shared(a.clone()), mode(false)), a);
        // `worktree` printed nothing in between, so its result goes back over
        // its own line; `commits` printed rows, so its result is a new line.
        assert_eq!(stream.matches("\x1b[1A").count(), 1);
        assert!(stream.contains("worktree ..........."));
        assert!(stream.contains("30 walked, 1 global"));

        let b = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let plain = drain(Plain::to_writer(Shared(b.clone()), mode(false)), b);
        assert!(
            !plain.contains('\x1b'),
            "a pipe takes no escapes: {plain:?}"
        );
    }

    #[test]
    fn jsonl_is_one_object_an_event_and_a_row_is_keyed_by_its_columns() {
        let b = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let text = drain(Jsonl::to_writer(Shared(b.clone())), b);
        let docs: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("one object per line"))
            .collect();
        assert_eq!(docs.len(), script().len());
        assert_eq!(docs[0]["event"], "header");
        let row = docs.iter().find(|d| d["event"] == "row").expect("a row");
        assert_eq!(row["commit"], "e6b1317b77");
        assert_eq!(row["date"], "09-02 09:14");
        assert_eq!(row["affected"], "375");
        assert_eq!(row["index"], "~  2625 rewritten");
    }

    #[test]
    fn colour_reaches_the_row_but_never_the_layout() {
        let a = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let painted = drain(Plain::to_writer(Shared(a.clone()), mode(true)), a);
        assert!(painted.contains("\x1b[36m"), "the index cell is cyan");
        assert!(painted.contains("\x1b[1mcambium\x1b[0m"));
    }
}
