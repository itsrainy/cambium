//! The blocks a verb prints on stdout once the run is over, and the units
//! every verb says a number in.

/// Milliseconds in the unit someone would say: `180 ms`, `4.2 s`, `2 min 05 s`.
pub fn human_ms(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1000..=59_999 => format!("{:.1} s", ms as f64 / 1000.0),
        _ => format!("{} min {:02} s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

/// Thousands separated, which is how a target count is read aloud.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A byte count in the unit someone would say it in: `512 B`, `3.5 MiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// One line of the key/value block: a key, the number it stands for, and an
/// aside that qualifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub key: String,
    pub value: String,
    pub note: String,
}

impl Pair {
    pub fn new(key: &str, value: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            value: value.into(),
            note: note.into(),
        }
    }
}

/// The aligned key/value block: the same numbers as the paragraphs, for
/// someone who only wants to read down the left edge.
///
/// Every column is as wide as its widest entry, so the block is as narrow as
/// the numbers in it allow and nothing is padded out to a guess.
pub fn kv(pairs: &[Pair]) -> Vec<String> {
    let kw = pairs
        .iter()
        .map(|p| p.key.chars().count())
        .max()
        .unwrap_or(0);
    let vw = pairs
        .iter()
        .map(|p| p.value.chars().count())
        .max()
        .unwrap_or(0);
    pairs
        .iter()
        .map(|p| {
            let line = format!(
                "{:<kw$}  {:<vw$}  {}",
                p.key,
                p.value,
                p.note,
                kw = kw,
                vw = vw
            );
            line.trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_in_the_unit_a_person_would_use() {
        assert_eq!(human_ms(180), "180 ms");
        assert_eq!(human_ms(4_230), "4.2 s");
        assert_eq!(human_ms(125_000), "2 min 05 s");
    }

    #[test]
    fn counts_are_grouped_by_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(1_234_567), "1,234,567");
    }

    #[test]
    fn bytes_are_rounded_the_way_a_reader_scans_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MiB");
    }

    #[test]
    fn the_key_value_block_aligns_on_its_widest_entry() {
        let lines = kv(&[
            Pair::new("repository", "envoy", "at cc1b5cb, last 30 commits"),
            Pair::new("targets", "68,412", "(1,204 tests)"),
            Pair::new("per commit", "116 ms", ""),
        ]);
        assert_eq!(
            lines,
            vec![
                "repository  envoy   at cc1b5cb, last 30 commits",
                "targets     68,412  (1,204 tests)",
                "per commit  116 ms",
            ]
        );
    }

    #[test]
    fn an_empty_block_is_no_lines_at_all() {
        assert!(kv(&[]).is_empty());
    }
}
