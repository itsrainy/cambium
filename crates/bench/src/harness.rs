//! Result records, machine fingerprinting and regression comparison.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Enough about the machine to know whether two runs are comparable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    pub cpu: String,
    pub cores: usize,
    pub ram_gb: f64,
    pub os: String,
}

impl Machine {
    pub fn detect() -> Self {
        Self {
            cpu: cpu_model(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            ram_gb: total_ram_gb(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }

    /// Stable identity used to refuse cross-machine comparisons.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}c|{:.0}g|{}",
            self.cpu, self.cores, self.ram_gb, self.os
        )
    }
}

fn cpu_model() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("model name") {
                    if let Some(v) = v.split(':').nth(1) {
                        return v.trim().to_string();
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

fn total_ram_gb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("MemTotal:") {
                    let kb: f64 = v.trim().trim_end_matches(" kB").parse().unwrap_or(0.0);
                    return kb / 1024.0 / 1024.0;
                }
            }
        }
    }
    0.0
}

/// Peak resident set size of this process, in MiB. Linux only; 0 elsewhere.
pub fn peak_rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("VmHWM:") {
                    let kb: f64 = v.trim().trim_end_matches(" kB").parse().unwrap_or(0.0);
                    return kb / 1024.0;
                }
            }
        }
    }
    0.0
}

/// One stage measured at one size.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageResult {
    pub stage: String,
    pub preset: String,
    pub spec_hash: String,
    /// Sweep parameter value, when this result came from a sweep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<u64>,
    pub runs: usize,
    pub median_ms: f64,
    pub p90_ms: f64,
    pub peak_rss_mb: f64,
    /// Stage-specific numbers: targets/s, bytes, ratios.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, f64>,
    /// Stage-specific facts that are not numbers, such as the version of an
    /// external tool a comparison stage ran against.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<String, String>,
}

impl StageResult {
    pub fn key(&self) -> String {
        match self.param {
            Some(p) => format!("{}@{}#{}", self.stage, self.preset, p),
            None => format!("{}@{}", self.stage, self.preset),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchReport {
    /// JSON contract version; see `crate::RESULT_SCHEMA`.
    #[serde(default = "schema_version")]
    pub schema: u32,
    pub tool_version: String,
    pub tool_sha: String,
    pub timestamp_unix: u64,
    pub machine: Machine,
    pub results: Vec<StageResult>,
}

const fn schema_version() -> u32 {
    crate::RESULT_SCHEMA
}

impl BenchReport {
    pub fn new(results: Vec<StageResult>) -> Self {
        Self {
            schema: schema_version(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            tool_sha: git_sha(),
            timestamp_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            machine: Machine::detect(),
            results,
        }
    }

    /// Markdown summary table.
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "# cambium benchmarks\n\n{} ({} cores, {:.0} GiB) — tool {}\n\n",
            self.machine.cpu, self.machine.cores, self.machine.ram_gb, self.tool_sha
        ));
        s.push_str("| stage | preset | param | median ms | p90 ms | peak RSS MiB | notes |\n");
        s.push_str("|---|---|---:|---:|---:|---:|---|\n");
        for r in &self.results {
            let notes: Vec<String> = r
                .extra
                .iter()
                .map(|(k, v)| format!("{k}={}", fmt_num(*v)))
                .chain(r.notes.iter().map(|(k, v)| format!("{k}={v}")))
                .collect();
            s.push_str(&format!(
                "| {} | {} | {} | {:.2} | {:.2} | {:.0} | {} |\n",
                r.stage,
                r.preset,
                r.param.map(|p| p.to_string()).unwrap_or_default(),
                r.median_ms,
                r.p90_ms,
                r.peak_rss_mb,
                notes.join(", ")
            ));
        }
        s
    }
}

fn fmt_num(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.2}")
    }
}

fn git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// One stage's change between two reports.
#[derive(Debug)]
pub struct Regression {
    pub key: String,
    pub baseline_ms: f64,
    pub current_ms: f64,
    pub change_pct: f64,
}

/// Compares two reports, returning every stage that moved by more than
/// `threshold_pct`, slower first. Refuses to compare across machines.
pub fn compare(
    baseline: &BenchReport,
    current: &BenchReport,
    threshold_pct: f64,
) -> anyhow::Result<Vec<Regression>> {
    if baseline.machine.fingerprint() != current.machine.fingerprint() {
        anyhow::bail!(
            "refusing to compare runs from different machines:\n  baseline: {}\n  current:  {}",
            baseline.machine.fingerprint(),
            current.machine.fingerprint()
        );
    }
    let base: BTreeMap<String, &StageResult> =
        baseline.results.iter().map(|r| (r.key(), r)).collect();
    let mut out = Vec::new();
    for c in &current.results {
        let Some(b) = base.get(&c.key()) else {
            continue;
        };
        if b.median_ms <= 0.0 {
            continue;
        }
        let change = (c.median_ms - b.median_ms) / b.median_ms * 100.0;
        if change.abs() > threshold_pct {
            out.push(Regression {
                key: c.key(),
                baseline_ms: b.median_ms,
                current_ms: c.median_ms,
                change_pct: change,
            });
        }
    }
    out.sort_by(|a, b| b.change_pct.total_cmp(&a.change_pct));
    Ok(out)
}

/// Times `f` `runs` times, returning (median ms, p90 ms).
pub fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, f64) {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs.max(1) {
        let t = std::time::Instant::now();
        let out = f();
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
        drop(out);
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    let p90 = samples[((samples.len() as f64 * 0.9) as usize).min(samples.len() - 1)];
    (median, p90)
}

/// Minimal SVG line chart of `ms` against a size parameter, on log-log axes.
///
/// Hand-rolled rather than pulled from a plotting crate: the whole point is to
/// see at a glance whether a stage is linear, and a straight line on log-log
/// axes says exactly that.
pub fn plot_svg(title: &str, points: &[(f64, f64)]) -> String {
    plot_svg_series(title, &[("", points)])
}

/// The same chart with several labelled series on shared axes, so two
/// approaches to the same problem can be read against each other.
pub fn plot_svg_series(title: &str, series: &[(&str, &[(f64, f64)])]) -> String {
    const W: f64 = 720.0;
    const H: f64 = 420.0;
    const PAD: f64 = 60.0;
    const COLORS: [&str; 4] = ["#2b6cb0", "#c05621", "#2f855a", "#6b46c1"];
    let usable: Vec<(&str, &[(f64, f64)])> = series
        .iter()
        .copied()
        .filter(|(_, p)| p.len() >= 2)
        .collect();
    if usable.is_empty() {
        return format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\"><text x=\"20\" y=\"30\">{title}: not enough points</text></svg>");
    }
    let all_x: Vec<f64> = usable
        .iter()
        .flat_map(|(_, p)| p.iter().map(|q| q.0.max(1.0).log10()))
        .collect();
    let all_y: Vec<f64> = usable
        .iter()
        .flat_map(|(_, p)| p.iter().map(|q| q.1.max(1e-3).log10()))
        .collect();
    let (x0, x1) = (min(&all_x), max(&all_x));
    let (y0, y1) = (min(&all_y), max(&all_y));
    let sx = |v: f64| PAD + (v - x0) / (x1 - x0).max(1e-9) * (W - 2.0 * PAD);
    let sy = |v: f64| H - PAD - (v - y0) / (y1 - y0).max(1e-9) * (H - 2.0 * PAD);

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" \
         font-family=\"system-ui, sans-serif\" font-size=\"12\">\n\
         <rect width=\"{W}\" height=\"{H}\" fill=\"#fff\"/>\n\
         <text x=\"{PAD}\" y=\"28\" font-size=\"15\" font-weight=\"600\">{title}</text>\n\
         <line x1=\"{PAD}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#999\"/>\n\
         <line x1=\"{PAD}\" y1=\"{PAD}\" x2=\"{PAD}\" y2=\"{}\" stroke=\"#999\"/>\n",
        H - PAD,
        W - PAD,
        H - PAD,
        H - PAD
    );
    // A reference line of exact linear scaling through the first series' first
    // point, so a reader can see super-linear behaviour immediately.
    let first = usable[0].1;
    let (fx, fy) = (first[0].0.max(1.0).log10(), first[0].1.max(1e-3).log10());
    let ideal_end = fy + (x1 - fx);
    s.push_str(&format!(
        "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#bbb\" \
         stroke-dasharray=\"4 4\"/>\n<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#999\">linear</text>\n",
        sx(fx),
        sy(fy),
        sx(x1),
        sy(ideal_end),
        sx(x1) - 40.0,
        sy(ideal_end) - 6.0
    ));
    for (i, (name, points)) in usable.iter().enumerate() {
        let color = COLORS[i % COLORS.len()];
        let lx: Vec<f64> = points.iter().map(|p| p.0.max(1.0).log10()).collect();
        let ly: Vec<f64> = points.iter().map(|p| p.1.max(1e-3).log10()).collect();
        let path: Vec<String> = lx
            .iter()
            .zip(&ly)
            .map(|(x, y)| format!("{:.1},{:.1}", sx(*x), sy(*y)))
            .collect();
        s.push_str(&format!(
            "<polyline fill=\"none\" stroke=\"{color}\" stroke-width=\"2\" points=\"{}\"/>\n",
            path.join(" ")
        ));
        for (j, (x, y)) in lx.iter().zip(&ly).enumerate() {
            s.push_str(&format!(
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3.5\" fill=\"{color}\"/>\n\
                 <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#444\">{}</text>\n",
                sx(*x),
                sy(*y),
                sx(*x),
                sy(*y) - 10.0,
                fmt_num(points[j].1)
            ));
            if i == 0 {
                s.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#666\">{}</text>\n",
                    sx(*x),
                    H - PAD + 18.0,
                    fmt_num(points[j].0)
                ));
            }
        }
        if !name.is_empty() {
            let ly_legend = PAD + 12.0 + i as f64 * 16.0;
            s.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{ly_legend:.1}\" x2=\"{:.1}\" y2=\"{ly_legend:.1}\" \
                 stroke=\"{color}\" stroke-width=\"2\"/>\n\
                 <text x=\"{:.1}\" y=\"{:.1}\" fill=\"#444\">{name}</text>\n",
                W - PAD - 150.0,
                W - PAD - 128.0,
                W - PAD - 122.0,
                ly_legend + 4.0
            ));
        }
    }
    s.push_str(&format!(
        "<text x=\"14\" y=\"{:.1}\" fill=\"#666\" \
         transform=\"rotate(-90 14 {:.1})\">median ms (log)</text>\n\
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#666\">size (log)</text>\n",
        H / 2.0,
        H / 2.0,
        W / 2.0,
        H - 14.0
    ));
    s.push_str("</svg>\n");
    s
}

fn min(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

fn max(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(ms: f64) -> BenchReport {
        let mut r = BenchReport::new(vec![StageResult {
            stage: "walk".into(),
            preset: "small".into(),
            spec_hash: "abc".into(),
            param: None,
            runs: 3,
            median_ms: ms,
            p90_ms: ms,
            peak_rss_mb: 10.0,
            extra: BTreeMap::new(),
            notes: BTreeMap::new(),
        }]);
        r.machine = Machine {
            cpu: "test".into(),
            cores: 8,
            ram_gb: 16.0,
            os: "linux".into(),
        };
        r
    }

    #[test]
    fn compare_flags_regressions_only_past_the_threshold() {
        let base = report(10.0);
        assert!(compare(&base, &report(11.0), 15.0).unwrap().is_empty());
        let regressions = compare(&base, &report(12.0), 15.0).unwrap();
        assert_eq!(regressions.len(), 1);
        assert!((regressions[0].change_pct - 20.0).abs() < 1e-9);
        // Improvements are reported too, so a suspicious speedup is visible.
        let improved = compare(&base, &report(5.0), 15.0).unwrap();
        assert!(improved[0].change_pct < 0.0);
    }

    #[test]
    fn compare_refuses_across_machines() {
        let base = report(10.0);
        let mut other = report(10.0);
        other.machine.cores = 4;
        assert!(compare(&base, &other, 15.0).is_err());
    }

    #[test]
    fn plot_emits_svg() {
        let svg = plot_svg("walk", &[(1e4, 1.0), (1e5, 10.0), (1e6, 100.0)]);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("polyline"));
    }
}
