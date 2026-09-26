//! Row layout: fixed columns, right-aligned numbers, one flexible cell.
//!
//! A row never grows past [`ROW_CAP`] columns, so on a wide terminal the
//! numbers stay beside the text instead of being flung at the right edge.
//! The flexible column takes whatever is left inside that cap, and its text
//! is truncated with an ellipsis to fit.

use crate::event::{Align, Cell, Column, Width};
use crate::paint::Paint;

/// A row never grows past this many columns.
pub const ROW_CAP: usize = 96;
/// Rows sit inside their phase.
pub(crate) const INDENT: usize = 4;
/// The narrowest a flexible column is ever squeezed to.
const FLEX_MIN: usize = 24;
/// The width assumed when there is no terminal to measure.
const PIPE_WIDTH: usize = 80;

/// Terminal width, or the 80 columns a pipe is assumed to have.
pub(crate) fn terminal_width() -> usize {
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w as usize)
        .filter(|w| *w >= 40)
        .unwrap_or(PIPE_WIDTH)
}

/// Pads `s` to `n` columns, or truncates it to `n` with a trailing ellipsis.
pub(crate) fn fit(s: &str, n: usize, align: Align) -> String {
    let count = s.chars().count();
    if count > n {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        return out;
    }
    let pad = " ".repeat(n - count);
    match align {
        Align::Left => format!("{s}{pad}"),
        Align::Right => format!("{pad}{s}"),
    }
}

/// The width of every column, with the flexible one sized to the cap.
fn widths(cols: &[Column], term: usize) -> Vec<usize> {
    let cap = term.min(ROW_CAP);
    let fixed: usize = cols
        .iter()
        .map(|c| match c.width {
            Width::Fixed(w) => w,
            Width::Flex => 0,
        })
        .sum();
    let gaps = cols.len().saturating_sub(1);
    let flex = cap.saturating_sub(INDENT + fixed + gaps).max(FLEX_MIN);
    cols.iter()
        .map(|c| match c.width {
            Width::Fixed(w) => w,
            Width::Flex => flex,
        })
        .collect()
}

/// The commit cell: `sha7 MM-DD HH:MM subject…`, date dimmed on a terminal.
fn commit_cell(sha: &str, date: &str, subject: &str, w: usize, p: &Paint) -> String {
    let sha7: String = sha.chars().take(7).collect();
    let used = sha7.chars().count() + 1 + date.chars().count() + 1;
    let subject = fit(subject, w.saturating_sub(used).max(1), Align::Left);
    format!("{} {} {}", p.bold(&sha7), p.dim(date), subject)
}

/// One row, painted, with the trailing blanks cut off.
pub fn render(cols: &[Column], cells: &[Cell], term: usize, p: &Paint) -> String {
    let ws = widths(cols, term);
    let mut out = " ".repeat(INDENT);
    for (i, cell) in cells.iter().enumerate() {
        let w = ws.get(i).copied().unwrap_or(0);
        let align = cols.get(i).map(|c| c.align).unwrap_or(Align::Left);
        if i > 0 {
            out.push(' ');
        }
        match cell {
            Cell::Text(s) => out.push_str(&fit(s, w, align)),
            Cell::Styled(s, style) => {
                let padded = fit(s, w, align);
                // Painted after padding, so the blanks are not inside the
                // escape and the column still lines up.
                let trimmed = padded.trim_end().to_string();
                let tail = padded.len() - trimmed.len();
                out.push_str(&p.style(&trimmed, *style));
                out.push_str(&" ".repeat(tail));
            }
            Cell::Commit { sha, date, subject } => {
                out.push_str(&commit_cell(sha, date, subject, w, p))
            }
        }
    }
    out.trim_end().to_string()
}

/// The column header line, dim, laid out exactly as the rows under it.
pub fn render_header(cols: &[Column], term: usize, p: &Paint) -> String {
    let ws = widths(cols, term);
    let mut out = " ".repeat(INDENT);
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&fit(&c.title, ws[i], c.align));
    }
    p.dim(out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Style;

    fn cols() -> Vec<Column> {
        vec![
            Column::flex("commit"),
            Column::fixed("time", 8, Align::Right),
            Column::fixed("affected", 8, Align::Right),
            Column::fixed("index", 17, Align::Left),
        ]
    }

    #[test]
    fn numbers_are_right_aligned_and_the_commit_cell_takes_what_is_left() {
        let cells = vec![
            Cell::Commit {
                sha: "e6b1317b7742".into(),
                date: "09-02 09:14".into(),
                subject: "tcp_proxy: idle timeout".into(),
            },
            Cell::text("116 ms"),
            Cell::text("375"),
            Cell::styled("~  2625 rewritten", Style::Cyan),
        ];
        let line = render(&cols(), &cells, 200, &Paint::new(false));
        assert!(
            line.starts_with("    e6b1317 09-02 09:14 tcp_proxy: idle timeout "),
            "{line}"
        );
        assert!(
            line.ends_with("  116 ms      375 ~  2625 rewritten"),
            "{line}"
        );
        // Never past the cap, whatever the terminal is.
        assert!(line.chars().count() <= ROW_CAP, "{line}");
    }

    #[test]
    fn a_narrow_terminal_truncates_the_subject_not_the_numbers() {
        let cells = vec![
            Cell::Commit {
                sha: "abcdef1234".into(),
                date: "09-02 09:14".into(),
                subject: "a subject far too long to fit in here at all".into(),
            },
            Cell::text("4.2 s"),
            Cell::text("1,024"),
            Cell::text("= unchanged"),
        ];
        let line = render(&cols(), &cells, 60, &Paint::new(false));
        assert!(line.contains('…'), "{line}");
        assert!(line.ends_with("= unchanged"), "{line}");
        assert!(line.contains("4.2 s    1,024"), "{line}");
    }

    #[test]
    fn the_header_is_laid_out_exactly_as_a_row() {
        let head = render_header(&cols(), 96, &Paint::new(false));
        let cells = vec![
            Cell::Commit {
                sha: "0123456789".into(),
                date: "09-02 09:14".into(),
                subject: "s".into(),
            },
            Cell::text("1 ms"),
            Cell::text("42"),
            Cell::text("x"),
        ];
        let row = render(&cols(), &cells, 96, &Paint::new(false));
        let end_of =
            |line: &str, needle: &str| line.find(needle).expect(needle) + needle.chars().count();
        assert_eq!(end_of(&head, "time"), end_of(&row, "1 ms"));
        assert_eq!(end_of(&head, "affected"), end_of(&row, "42"));
        assert_eq!(head.find("index"), row.find("x"));
    }

    #[test]
    fn colour_never_changes_the_columns() {
        let cells = vec![
            Cell::Commit {
                sha: "0123456789".into(),
                date: "09-02 09:14".into(),
                subject: "s".into(),
            },
            Cell::text("1 ms"),
            Cell::text("2"),
            Cell::styled("= unchanged", Style::Dim),
        ];
        let plain = render(&cols(), &cells, 96, &Paint::new(false));
        let painted = render(&cols(), &cells, 96, &Paint::new(true));
        let stripped: String = strip_ansi(&painted);
        assert_eq!(plain, stripped);
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
