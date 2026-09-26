//! Two ways to draw a distribution, both plain text with no ANSI in them, so
//! a pipe and a CI log see exactly what a terminal does.
//!
//! [`columns`] is a vertical column chart, eight rows tall, with each value
//! printed directly above its own column and the labels centred under an axis
//! line. [`bars`] is one horizontal block bar per bin with the count after
//! it, its bars as wide as the caller asks.
//!
//! The column chart fits itself inside [`crate::row::ROW_CAP`] as bins are
//! added: first its bars narrow from three columns to two, then it prints
//! every other value, then every other label, and last it narrows the bars
//! to one. Past that last step (47 bins, when no value or label is longer
//! than three characters) it runs wider than the cap.

use crate::row::ROW_CAP;

/// Rows of bar in a column chart.
const COL_HEIGHT: usize = 8;
/// Left indent, shared by the chart and everything around it.
const COL_INDENT: usize = 2;
/// Partial column tops, one eighth to seven eighths.
const EIGHTHS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];

/// How the column chart spends its width.
#[derive(Copy, Clone)]
struct Layout {
    /// Columns per bin, including the one blank between neighbours.
    cell: usize,
    /// Columns of block in each bar.
    bar_w: usize,
    /// A value is printed above every `val_every`th bin.
    val_every: usize,
    /// A label is printed under every `lab_every`th bin.
    lab_every: usize,
}

/// The steps the column chart narrows through as bins are added, as
/// `(val_every, lab_every, bar_w)`. The bars narrow to two before anything is
/// thinned, and to one only once values and labels are both thinned.
const STEPS: [(usize, usize, usize); 5] = [(1, 1, 3), (1, 1, 2), (2, 1, 2), (2, 2, 2), (2, 2, 1)];

impl Layout {
    /// The first step whose `n` bins fit inside the cap, or the last step
    /// when none does.
    fn fit(n: usize, w_val: usize, w_lab: usize) -> Self {
        let budget = ROW_CAP.saturating_sub(COL_INDENT);
        let steps = STEPS.map(|(val_every, lab_every, bar_w)| Layout {
            // A label printed every k columns may borrow the k columns it
            // spans.
            cell: (bar_w + 1)
                .max((w_val + 1).div_ceil(val_every))
                .max((w_lab + 1).div_ceil(lab_every)),
            bar_w,
            val_every,
            lab_every,
        });
        let last = steps[steps.len() - 1];
        // The last bin's blank neighbour is never drawn, hence the one spare.
        steps
            .into_iter()
            .find(|l| n * l.cell <= budget + 1)
            .unwrap_or(last)
    }
}

/// A vertical column chart: one column per bin, the value directly above each
/// column, labels under an axis line. A bin with one item in it is still
/// visible, because a fractional top is drawn from the eighth blocks.
pub fn columns(counts: &[u64], labels: &[String]) -> Vec<String> {
    if counts.is_empty() {
        return Vec::new();
    }
    let n = counts.len();
    let values: Vec<String> = counts.iter().map(|c| c.to_string()).collect();
    let w_val = values.iter().map(|v| v.chars().count()).max().unwrap_or(1);
    let w_lab = labels
        .iter()
        .map(|d| d.chars().count())
        .max()
        .unwrap_or(1)
        .max(1);
    let Layout {
        cell,
        bar_w,
        val_every,
        lab_every,
    } = Layout::fit(n, w_val, w_lab);
    let content = cell - 1;
    let bar_w = bar_w.min(content);
    let total = COL_INDENT + n * cell - 1;

    // One row above the tallest column, so its value has somewhere to sit.
    let mut grid = vec![vec![' '; total]; COL_HEIGHT + 1];
    let max = counts.iter().copied().max().unwrap_or(1).max(1);

    for (i, &count) in counts.iter().enumerate() {
        let x0 = COL_INDENT + i * cell;
        let bar_x = x0 + (content - bar_w) / 2;
        let mut eighths = (count as f64 / max as f64 * (COL_HEIGHT * 8) as f64).round() as usize;
        if count > 0 && eighths == 0 {
            eighths = 1;
        }
        let full = eighths / 8;
        let frac = eighths % 8;
        let top = 1 + COL_HEIGHT - full - usize::from(frac > 0);
        for row in grid
            .iter_mut()
            .take(COL_HEIGHT + 1)
            .skip(top + usize::from(frac > 0))
        {
            for dx in 0..bar_w {
                row[bar_x + dx] = '█';
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
    // Drop the blank rows above the tallest value.
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }

    let mut axis = " ".repeat(COL_INDENT);
    axis.push_str(&"─".repeat(n * cell - 1));
    out.push(axis);

    let mut line = vec![' '; total];
    for (i, d) in labels.iter().enumerate().take(n) {
        if i % lab_every != 0 {
            continue;
        }
        let x0 = COL_INDENT + i * cell;
        let bar_x = x0 + (content - bar_w) / 2;
        let w = d.chars().count();
        let start = (bar_x + bar_w / 2)
            .saturating_sub(w / 2)
            .min(total.saturating_sub(w));
        for (k, ch) in d.chars().enumerate() {
            if start + k < total {
                line[start + k] = ch;
            }
        }
    }
    out.push(line.into_iter().collect::<String>().trim_end().to_string());
    out
}

/// One horizontal block bar per bin, with its label before it and its count
/// after it. An eighth-width tail keeps two short bars from looking alike.
pub fn bars(counts: &[u64], labels: &[String], width: usize) -> Vec<String> {
    let max = counts.iter().copied().max().unwrap_or(1).max(1);
    let lw = labels
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(1);
    counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let label = labels.get(i).cloned().unwrap_or_default();
            format!("  {label:>lw$}  {} {count}", bar(count, max, width))
        })
        .collect()
}

fn bar(count: u64, max: u64, cols: usize) -> String {
    let eighths = (count as f64 / max as f64 * cols as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rest = eighths % 8;
    let mut s = "█".repeat(full);
    if rest >= 4 {
        s.push('▌');
    } else if rest > 0 {
        s.push('▏');
    }
    format!("{s:<cols$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(n: usize) -> Vec<String> {
        (0..n).map(|d| d.to_string()).collect()
    }

    fn width_of(lines: &[String]) -> usize {
        lines.iter().map(|l| l.chars().count()).max().unwrap_or(0)
    }

    #[test]
    fn seven_bins_draw_every_value_and_every_label() {
        let counts = vec![12, 340, 1, 0, 76, 9, 250];
        let lines = columns(&counts, &labels(7));
        assert!(width_of(&lines) <= ROW_CAP, "{lines:#?}");
        let text = lines.join("\n");
        for v in ["12", "340", "76", "250"] {
            assert!(text.contains(v), "{v} is missing from\n{text}");
        }
        assert!(lines[lines.len() - 2].starts_with("  ─"), "an axis line");
        assert_eq!(
            lines.last().unwrap().split_whitespace().collect::<Vec<_>>(),
            ["0", "1", "2", "3", "4", "5", "6"]
        );
        // A bin with one item in it is still visible, and an empty one is not.
        assert!(text.contains('▁'));
    }

    #[test]
    fn twenty_five_bins_narrow_the_bars_and_keep_every_label() {
        let counts: Vec<u64> = (0..25).map(|i| (i * 37 % 91) as u64).collect();
        let lines = columns(&counts, &labels(25));
        assert!(width_of(&lines) <= ROW_CAP, "{lines:#?}");
        let drawn: Vec<&str> = lines.last().unwrap().split_whitespace().collect();
        assert_eq!(
            drawn.len(),
            25,
            "25 bins still fit inside the cap: {drawn:?}"
        );
        assert_eq!(drawn.last(), Some(&"24"));
    }

    #[test]
    fn forty_bins_thin_out_the_labels_and_still_draw_the_bars() {
        let counts: Vec<u64> = (0..40).map(|i| (i * 37 % 91) as u64).collect();
        let lines = columns(&counts, &labels(40));
        assert!(width_of(&lines) <= ROW_CAP, "{lines:#?}");
        let drawn: Vec<&str> = lines.last().unwrap().split_whitespace().collect();
        assert!(drawn.len() < 40, "labels thin out: {drawn:?}");
        assert!(
            lines.iter().any(|l| l.contains('█')),
            "the bars are still drawn"
        );
    }

    #[test]
    fn bars_are_one_line_a_bin_with_the_count_after_them() {
        let lines = bars(&[10, 0, 5], &labels(3), 10);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("  0  ██████████ 10"), "{:?}", lines[0]);
        assert!(lines[1].ends_with(" 0"));
        assert!(lines[2].contains('█'));
        assert!(lines.iter().all(|l| !l.contains('\x1b')));
    }

    #[test]
    fn an_empty_distribution_draws_nothing() {
        assert!(columns(&[], &[]).is_empty());
        assert!(bars(&[], &[], 10).is_empty());
    }
}
