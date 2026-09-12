//! The other case: `cambium-bench watch`, a whole matrix running at once.
//! Fullscreen, alternate screen, one table of subjects and three live
//! plots. The same event stream drives it: subject 1 / cambi is the real
//! run, the other cells are that run rescaled, so the shapes stay related.

use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyCode};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Bar, BarChart, Block, BorderType, Cell, Chart, Dataset, GraphType, Row, Table,
};
use ratatui::Frame;

use crate::report;
use crate::sim::{self, Ev};

const SUBJECTS: [&str; 3] = ["envoy", "abseil-cpp", "rules_go"];
const TOOLS: [&str; 3] = ["cambi", "bazel-diff", "tango"];
/// How each cell's wall time relates to the measured one.
const FACTOR: [[f64; 3]; 3] = [
    [1.0, 6.4, 3.1],
    [0.45, 3.0, 1.6],
    [0.28, 1.9, 0.95],
];

struct Cellst {
    step: usize,
    total_ms: f64,
    series: Vec<(f64, f64)>,
}

pub fn run(speed: f32, style: crate::report::Style) {
    let s = sim::build();
    if !crate::term::require_tty("full") {
        crate::stream::profile(speed, style, false, crate::report::CommitStyle::Compact);
        return;
    }
    let rx = sim::spawn(s.clone(), speed);

    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(err) => {
            ratatui::restore();
            eprintln!(
                "ui-feel full: this terminal would not start a fullscreen session ({err}). \
                 Falling back to the plain renderer."
            );
            crate::stream::profile(speed, style, false, crate::report::CommitStyle::Compact);
            return;
        }
    };
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        ratatui::restore();
        prev(info);
    }));

    let mut cells: Vec<Cellst> = (0..9)
        .map(|_| Cellst {
            step: 0,
            total_ms: 0.0,
            series: Vec::new(),
        })
        .collect();
    let total = s.commits.len();
    let start = Instant::now();
    let mut phase = "starting";
    let mut running = true;
    let mut quit = false;

    while running {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ev::PhaseStart { name, .. }) => phase = name,
            Ok(Ev::Row(r)) => {
                let idx = cells[0].step;
                let wall_ms = r.wall_ms;
                for (n, cell) in cells.iter_mut().enumerate() {
                    let f = FACTOR[n / 3][n % 3];
                    let ms = wall_ms as f64 * f;
                    cell.step = idx + 1;
                    cell.total_ms += ms;
                    cell.series.push((idx as f64, ms));
                }
            }
            Ok(Ev::Done) => running = false,
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => running = false,
        }

        if event::poll(Duration::from_millis(0)).unwrap_or(false) {
            if let Ok(CtEvent::Key(k)) = event::read() {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    quit = true;
                    running = false;
                }
            }
        }
        let _ = terminal.draw(|f| draw(f, &cells, total, phase, start));
    }

    // With `--speed 0` there is nothing to watch, so do not wait for a key.
    if !quit && speed > 0.0 {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let _ = terminal.draw(|f| draw(f, &cells, total, "done, press q", start));
            if event::poll(Duration::from_millis(200)).unwrap_or(false) {
                if let Ok(CtEvent::Key(k)) = event::read() {
                    if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                        break;
                    }
                }
            }
        }
    }

    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    ratatui::restore();
    println!("cambium-bench: {} subjects x {} tools", SUBJECTS.len(), TOOLS.len());
}

fn draw(f: &mut Frame, cells: &[Cellst], total: usize, phase: &str, start: Instant) {
    let [head, table_area, chart_area, bars_area, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3 + 2),
        Constraint::Min(8),
        Constraint::Length(9),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let secs = start.elapsed().as_secs();
    f.render_widget(
        Line::from(vec![
            Span::styled(
                " cambium-bench watch ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {phase}   elapsed {}:{:02}",
                secs / 60,
                secs % 60
            )),
        ]),
        head,
    );

    // Per-subject progress, with the ETA the slowest tool implies.
    let rows: Vec<Row> = SUBJECTS
        .iter()
        .enumerate()
        .map(|(si, name)| {
            let done = cells[si * 3].step;
            let mean = if done > 0 {
                cells[si * 3].total_ms / done as f64
            } else {
                0.0
            };
            let eta = mean * (total - done) as f64;
            Row::new(vec![
                Cell::from(Span::styled(
                    (*name).to_string(),
                    Style::default().fg(Color::Cyan),
                )),
                Cell::from(format!("{done}/{total}")),
                Cell::from(gauge(done, total, 24)),
                Cell::from(report::human_ms(eta as u64)),
                Cell::from(
                    TOOLS
                        .iter()
                        .enumerate()
                        .map(|(ti, t)| {
                            format!("{t} {}", report::human_ms(cells[si * 3 + ti].total_ms as u64))
                        })
                        .collect::<Vec<_>>()
                        .join("   "),
                ),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Length(26),
                Constraint::Length(10),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(vec!["subject", "step", "progress", "eta", "total by tool"]).style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Gray),
            ),
        )
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" matrix "),
        ),
        table_area,
    );

    // Wall time per tool over the walk, summed across subjects.
    let colours = [Color::Cyan, Color::Magenta, Color::Yellow];
    let per_tool: Vec<Vec<(f64, f64)>> = (0..3)
        .map(|ti| {
            let n = cells[ti].series.len();
            (0..n)
                .map(|i| {
                    let y: f64 = (0..3).map(|si| cells[si * 3 + ti].series[i].1).sum();
                    (i as f64, y)
                })
                .collect()
        })
        .collect();
    let ymax = per_tool
        .iter()
        .flatten()
        .map(|(_, y)| *y)
        .fold(1.0f64, f64::max);
    let datasets: Vec<Dataset> = per_tool
        .iter()
        .enumerate()
        .map(|(ti, d)| {
            Dataset::default()
                .name(TOOLS[ti])
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(colours[ti]))
                .data(d)
        })
        .collect();
    f.render_widget(
        Chart::new(datasets)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" wall time per commit "),
            )
            .x_axis(
                Axis::default()
                    .bounds([0.0, total as f64])
                    .style(Style::default().fg(Color::DarkGray)),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, ymax * 1.1])
                    .labels(vec!["0".into(), report::human_ms(ymax as u64)])
                    .style(Style::default().fg(Color::DarkGray)),
            ),
        chart_area,
    );

    let bars: Vec<Bar> = (0..9)
        .map(|n| {
            let (si, ti) = (n / 3, n % 3);
            Bar::default()
                .value(cells[n].total_ms as u64 / 100)
                .text_value(report::human_ms(cells[n].total_ms as u64))
                .label(Line::from(format!("{}/{}", &SUBJECTS[si][..3], TOOLS[ti])))
                .style(Style::default().fg(colours[ti]))
        })
        .collect();
    f.render_widget(
        BarChart::vertical(bars).bar_width(11).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" total wall time "),
        ),
        bars_area,
    );

    f.render_widget(
        Line::from(Span::styled(
            " q quits ",
            Style::default().add_modifier(Modifier::DIM),
        )),
        foot,
    );
}

fn gauge(done: usize, total: usize, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        done * width / total
    };
    let mut s = String::new();
    for i in 0..width {
        s.push(if i < filled { '█' } else { '░' });
    }
    s
}
