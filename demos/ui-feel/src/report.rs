//! The final report, in the shape `cambi profile`'s `print_human` prints:
//! two paragraphs someone pastes into a proposal, then the table behind it.

use crate::sim::{Sim, Splice, Step, REPO};

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Thousands separated, which is how a target count is read aloud.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Milliseconds in the unit someone would say: `180 ms`, `4.2 s`, `2 min 05 s`.
pub fn human_ms(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1000..=59_999 => format!("{:.1} s", ms as f64 / 1000.0),
        _ => format!("{} min {:02} s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

pub fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let b = b as f64;
    if b < K {
        format!("{b:.0} B")
    } else if b < K * K {
        format!("{:.0} KiB", b / K)
    } else if b < K * K * K {
        format!("{:.0} MiB", b / (K * K))
    } else {
        format!("{:.1} GiB", b / (K * K * K))
    }
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}

/// Statistics both the report and the chart demo quote.
pub struct Stats {
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub min_ms: u64,
}

pub fn stats(values: &[u64]) -> Stats {
    let mut v = values.to_vec();
    v.sort_unstable();
    let sum: u64 = v.iter().sum();
    Stats {
        mean_ms: if v.is_empty() { 0 } else { sum / v.len() as u64 },
        p50_ms: percentile(&v, 0.50),
        p90_ms: percentile(&v, 0.90),
        p95_ms: percentile(&v, 0.95),
        max_ms: v.last().copied().unwrap_or(0),
        min_ms: v.first().copied().unwrap_or(0),
    }
}


/// How the final report reads. Progress goes to stderr either way; this is
/// only about what lands on stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Style {
    /// The paragraphs first, then the table. What `print_human` prints today.
    Prose,
    /// The table first, then the paragraphs, with every number bold.
    ProseLast,
    /// A compact key/value block instead of the paragraphs, then the table.
    Kv,
}

/// How the sha, date and subject of a commit are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CommitStyle {
    /// One left-aligned cell: `e6b1317 09-02 09:14 tcp_proxy: idle …`.
    Compact,
    /// Three columns, each with its own header.
    Columns,
}

/// The `index` column: what the run did to the stored index for this
/// commit. Dim when nothing happened, so it adds no noise.
pub fn splice_cell(sp: &Splice) -> (String, bool) {
    match sp {
        Splice::Identical => ("= unchanged".to_string(), true),
        Splice::Spliced { targets } => (format!("~ {targets:>5} rewritten"), false),
        Splice::Rebuilt => ("\u{21bb} rebuilt".to_string(), false),
    }
}

/// Width of the `index` column, so the numbers line up in every renderer.
pub const INDEX_COL: usize = 17;

/// The same cell padded to the column width.
pub fn splice_col(sp: &Splice) -> (String, bool) {
    let (text, dim) = splice_cell(sp);
    let pad = INDEX_COL.saturating_sub(text.chars().count());
    (format!("{text}{:pad$}", "", pad = pad), dim)
}

/// Wraps every run of digits, and the unit that follows it, in ANSI bold.
fn bold_numbers(line: &str, on: bool) -> String {
    if !on {
        return line.to_string();
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == ',' || chars[i] == '.')
            {
                i += 1;
            }
            // Take the unit written just after the number, if there is one.
            let mut end = i;
            if end < chars.len() && chars[end] == ' ' {
                let mut j = end + 1;
                let word_start = j;
                while j < chars.len() && chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let word: String = chars[word_start..j].iter().collect();
                if matches!(word.as_str(), "ms" | "s" | "min" | "B" | "KiB" | "MiB" | "GiB") {
                    end = j;
                }
            }
            out.push_str("\x1b[1m");
            out.extend(chars[start..end].iter());
            out.push_str("\x1b[0m");
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The prose paragraphs, without the table.
pub fn prose_lines(sim: &Sim) -> Vec<String> {
    let mut out = Vec::new();
    let head = sim.commits.last().map(|c| c.sha.clone()).unwrap_or_default();
    let from = sim.commits.first().map(|c| c.sha.clone()).unwrap_or_default();
    let n = sim.commits.len();
    out.push(format!(
        "Profile of /src/{} at {} (the last {} first-parent commit{}, from {})",
        REPO,
        short(&head),
        n,
        plural(n),
        short(&from),
    ));
    out.push(String::new());
    out.push(format!(
        "This build graph has {} targets ({} tests) in {} packages, with {} external \
         repositories.",
        group(sim.targets),
        group(sim.tests_total),
        group(sim.packages),
        group(sim.external_repos),
    ));
    out.push(format!(
        "A full cambium index takes {} to build and {} on disk (zstd-19); loading it takes {}.",
        human_ms(sim.cold_ms),
        human_bytes(sim.bytes_on_disk),
        human_ms(sim.load_ms),
    ));
    out.push(String::new());

    let inc = incremental(sim);
    let st = stats(&inc);
    let mean_affected: f64 = sim.steps.iter().map(|s| s.affected as f64).sum::<f64>() / n as f64;
    out.push(format!(
        "Based on the last {} commit{}, cambium takes {} on average to calculate the changed \
         targets for a commit (p50 {}, p90 {}, max {} over the {} commit{} answered \
         incrementally), answering with {:.0} targets on average.",
        n,
        plural(n),
        human_ms(st.mean_ms),
        human_ms(st.p50_ms),
        human_ms(st.p90_ms),
        human_ms(st.max_ms),
        inc.len(),
        plural(inc.len()),
        mean_affected,
    ));

    let globals = globals(sim);
    if globals.is_empty() {
        out.push(format!(
            "The entire graph never had to be rebuilt in those {} commit{}.",
            n,
            plural(n)
        ));
    } else {
        let rebuild_mean: u64 =
            globals.iter().map(|s| s.rebuild_ms).sum::<u64>() / globals.len() as u64;
        let reason = globals[0].global.unwrap_or("");
        out.push(format!(
            "The entire graph had to be rebuilt {} time{}, once every {:.1} commits on average, \
             at {} per rebuild ({} x {}).",
            globals.len(),
            plural(globals.len()),
            n as f64 / globals.len() as f64,
            human_ms(rebuild_mean),
            globals.len(),
            reason,
        ));
    }
    let identical = sim
        .steps
        .iter()
        .filter(|s| matches!(s.splice, crate::sim::Splice::Identical))
        .count();
    if identical > 0 {
        out.push(format!(
            "{} commit{} changed nothing the graph models, and were answered from the index \
             without splicing it.",
            identical,
            plural(identical),
        ));
    }
    out
}

fn incremental(sim: &Sim) -> Vec<u64> {
    sim.steps
        .iter()
        .filter(|s| s.global.is_none())
        .map(|s| s.wall_ms)
        .collect()
}

fn globals(sim: &Sim) -> Vec<&Step> {
    sim.steps.iter().filter(|s| s.global.is_some()).collect()
}

/// The same numbers as the paragraphs, for someone who only wants to read
/// down the left edge.
pub fn kv_lines(sim: &Sim) -> Vec<String> {
    let n = sim.commits.len();
    let st = stats(&incremental(sim));
    let g = globals(sim);
    let spliced = sim
        .steps
        .iter()
        .filter(|s| matches!(s.splice, crate::sim::Splice::Spliced { .. }))
        .count();
    let identical = sim
        .steps
        .iter()
        .filter(|s| matches!(s.splice, crate::sim::Splice::Identical))
        .count();
    let head = sim.commits.last().map(|c| c.sha.clone()).unwrap_or_default();
    vec![
        format!(
            "{:<12} {:<8} ({})",
            "repository",
            REPO,
            format!("at {}, last {} first-parent commits", short(&head), n)
        ),
        format!(
            "{:<12} {:<8} ({} tests, {} packages, {} externals)",
            "targets",
            group(sim.targets),
            group(sim.tests_total),
            group(sim.packages),
            group(sim.external_repos),
        ),
        format!(
            "{:<12} {:<8} ({} on disk, loads in {})",
            "full index",
            human_ms(sim.cold_ms),
            human_bytes(sim.bytes_on_disk),
            human_ms(sim.load_ms),
        ),
        format!(
            "{:<12} {:<8} p50 \u{b7} {} p95 \u{b7} {} max",
            "per commit",
            human_ms(st.p50_ms),
            human_ms(st.p95_ms),
            human_ms(st.max_ms),
        ),
        format!(
            "{:<12} {:<8} ({} spliced, {} unchanged)",
            "index work",
            format!("{spliced}/{n}"),
            spliced,
            identical,
        ),
        format!(
            "{:<12} {:<8} ({})",
            "rebuilds",
            format!("{}/{}", g.len(), n),
            match g.first() {
                Some(s) => s.global.unwrap_or("unknown"),
                None => "none",
            },
        ),
    ]
}

/// The per-commit table, splice column included.
pub fn table_lines(sim: &Sim, colour: bool, commits: CommitStyle) -> Vec<String> {
    let mut out = Vec::new();
    // Built the same way as a row, so the headers cannot drift out of line.
    let header_cell = match commits {
        CommitStyle::Columns => format!("{:<10} {:<11}", "commit", "date"),
        CommitStyle::Compact => format!("{:<19}", "commit"),
    };
    out.push(format!(
        "  {header_cell} {:>6} {:>9} {:>6} {:>9}  {}",
        "files", "affected", "tests", "time", "index"
    ));
    for (c, s) in sim.commits.iter().zip(sim.steps.iter()) {
        let (index, dim) = splice_col(&s.splice);
        let index = if dim && colour {
            format!("\x1b[2m{index}\x1b[0m")
        } else {
            index
        };
        let commit = match commits {
            CommitStyle::Columns => format!("{:<10} {:<11}", short(&c.sha), c.date),
            CommitStyle::Compact => {
                let date = if colour {
                    format!("\x1b[2m{:<11}\x1b[0m", c.date)
                } else {
                    format!("{:<11}", c.date)
                };
                format!("{:<7} {date}", &c.sha[..7])
            }
        };
        out.push(format!(
            "  {commit} {:>6} {:>9} {:>6} {:>9}  {}{}",
            s.files,
            s.affected,
            s.tests,
            human_ms(s.wall_ms + s.rebuild_ms),
            index,
            match s.global {
                Some(r) => format!("global: {r}"),
                None => String::new(),
            }
        )
        .trim_end()
        .to_string());
    }
    out
}

fn footer(sim: &Sim) -> String {
    let head = sim.commits.last().map(|c| c.sha.clone()).unwrap_or_default();
    format!(
        "The index for {} is at /tmp/{}-{}.cbi; `cambi index publish` uploads it.",
        short(&head),
        REPO,
        short(&head)
    )
}

/// The whole report, in the order `style` asks for.
pub fn lines(sim: &Sim, style: Style, colour: bool, commits: CommitStyle) -> Vec<String> {
    let mut out = Vec::new();
    match style {
        Style::Prose => {
            out.extend(prose_lines(sim));
            out.push(String::new());
            out.extend(table_lines(sim, colour, commits));
        }
        Style::ProseLast => {
            out.extend(table_lines(sim, colour, commits));
            out.push(String::new());
            out.extend(
                prose_lines(sim)
                    .into_iter()
                    .map(|l| bold_numbers(&l, colour)),
            );
        }
        Style::Kv => {
            out.extend(kv_lines(sim));
            out.push(String::new());
            out.extend(table_lines(sim, colour, commits));
        }
    }
    out.push(String::new());
    out.push(footer(sim));
    out
}

/// Results go to stdout; progress went to stderr while the run was live.
pub fn print(sim: &Sim, style: Style, colour: bool, commits: CommitStyle) {
    for line in lines(sim, style, colour, commits) {
        println!("{line}");
    }
}
