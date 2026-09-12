//! End of run only. The stream is consumed behind a one-line spinner, then
//! the same wall times are drawn twice, once by rasciigraph and once by
//! textplots, so the two crates can be compared side by side. Both are shape
//! and range only: no axes, no ticks, one caption line underneath.

use std::io::Write;

use rasciigraph::{plot, Config};
use textplots::{Chart, Plot, Shape};

use crate::report;
use crate::sim::{self, Ev};
use crate::term::stderr_tty;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn run(speed: f32, _style: crate::report::Style) {
    let s = sim::build();
    let rx = sim::spawn(s.clone(), speed);
    let tty = stderr_tty();
    let mut walls: Vec<u64> = Vec::new();
    let mut frame = 0usize;
    let total = s.commits.len();

    for ev in rx {
        match ev {
            Ev::Row(r) => walls.push(r.wall_ms),
            Ev::Done => break,
            _ => {}
        }
        if tty {
            frame += 1;
            let _ = write!(
                std::io::stderr(),
                "\r{} cambium profile  {}  [{}/{}]",
                SPINNER[frame % SPINNER.len()],
                sim::REPO,
                walls.len(),
                total
            );
            let _ = std::io::stderr().flush();
        }
    }
    if tty {
        let _ = write!(std::io::stderr(), "\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }

    let st = report::stats(&walls);
    let caption = format!(
        "range {} - {}  p50 {}  p95 {}",
        report::human_ms(st.min_ms),
        report::human_ms(st.max_ms),
        report::human_ms(st.p50_ms),
        report::human_ms(st.p95_ms),
    );

    println!();
    println!("rasciigraph: wall time per commit, in walk order");
    println!();
    let series: Vec<f64> = walls.iter().map(|&w| w as f64).collect();
    let raw = plot(
        series,
        Config::default().with_height(8).with_width(60).with_offset(0),
    );
    for line in strip_axis(&raw) {
        println!("  {line}");
    }
    println!("  {caption}");

    println!();
    println!("textplots: distribution of the same wall times, braille");
    println!();
    let bins = histogram(&walls, 24);
    let xmax = bins.len() as f32;
    let mut chart = Chart::new(120, 40, 0.0, xmax);
    let shape = Shape::Bars(&bins);
    // `frame()` returns the braille canvas alone: no axis lines, no ticks,
    // no labels, which is exactly the "shape and range only" wanted here.
    let plotted = chart.lineplot(&shape);
    plotted.figures();
    let frame_text = plotted.frame();
    for line in frame_text.lines() {
        println!("  {line}");
    }
    println!("  {caption}");
    println!();
}

/// rasciigraph always prints a value gutter and an axis column. Shape and
/// range only means dropping everything left of the plotting area.
pub fn strip_axis(rendered: &str) -> Vec<String> {
    let cut = rendered
        .lines()
        .filter_map(|l| l.char_indices().find(|(_, c)| *c == '┤' || *c == '┼'))
        .map(|(i, _)| i)
        .max();
    rendered
        .lines()
        .map(|l| match cut {
            Some(c) => l.chars().skip(c + 1).collect::<String>(),
            None => l.to_string(),
        })
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn histogram(walls: &[u64], bins: usize) -> Vec<(f32, f32)> {
    let max = walls.iter().copied().max().unwrap_or(1) as f32;
    let min = walls.iter().copied().min().unwrap_or(0) as f32;
    let width = ((max - min) / bins as f32).max(1.0);
    let mut counts = vec![0f32; bins];
    for &w in walls {
        let b = (((w as f32 - min) / width) as usize).min(bins - 1);
        counts[b] += 1.0;
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(i, c)| (i as f32, c))
        .collect()
}
