//! The three-way comparison section as numbers, before it is prose.
//!
//! [`CompareSection::compute`] folds a subject's `changes-*.json` files into
//! one typed model — the soundness rows, the timing rows, the class totals,
//! the reason histogram, the two oracle roll-ups — and the renderer in the
//! parent module turns that into Markdown and nothing else. A test asserts on
//! the model; the rendering is pinned by a snapshot.

use std::collections::BTreeMap;

use crate::classify::Class;
use crate::compare::{ChangesResult, WallStats};
use crate::oracles::load::LoadOracleSummary;
use crate::oracles::rdeps::RdepsOracleSummary;

use super::{compare_bazel, tool_names, SubjectResults};

/// Which change sets a timing distribution is taken over.
///
/// `all change sets` is the published one: nothing is excluded from it.
/// `incremental` drops the change sets where cambium answered `//...`, and
/// exists because the precision and class numbers are computed over that set
/// and their walls have to be readable next to them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every compared change set, the globals included: the headline.
    All,
    /// The change sets cambium answered with a walk: the rows that belong
    /// with the precision and class numbers.
    Incremental,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::All => "all change sets",
            Scope::Incremental => "incremental",
        }
    }
}

/// One row of the soundness table: a tool under one Bazel, or `tool: None`
/// for a comparison no tool ran in.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundnessRow {
    pub bazel: String,
    pub tool: Option<String>,
    pub change_sets: usize,
    pub passed: usize,
    /// `None` when the summary has no precision for the tool.
    pub precision: Option<f64>,
    pub min_recall: f64,
}

/// One row of the timings table: a tool, or `tool: None` for cambium.
#[derive(Clone, Debug, PartialEq)]
pub struct TimingRow {
    pub bazel: String,
    pub tool: Option<String>,
    pub scope: Scope,
    /// How many change sets the distributions are over.
    pub change_sets: usize,
    /// `None` when the run recorded no such distribution for the tool.
    pub cold: Option<WallStats>,
    pub warm: Option<WallStats>,
    /// cambium's own engine time under the same scope; `None` for a tool, and
    /// for a run that recorded none.
    pub cold_engine: Option<WallStats>,
    pub warm_engine: Option<WallStats>,
}

/// One row of the globals table: what cambium's `//...` cost against the
/// precise set one tool found, or `tool: None` when no tool ran.
#[derive(Clone, Debug, PartialEq)]
pub struct GlobalRow {
    pub bazel: String,
    pub change_sets: usize,
    /// Mean size of cambium's `//...` answer.
    pub cambium_labels: u64,
    pub tool: Option<String>,
    /// Mean size of the tool's precise answer.
    pub tool_labels: Option<u64>,
    pub tool_cold_p50_ms: Option<u64>,
    pub cambium_cold_p50_ms: u64,
    pub cambium_warm_p50_ms: u64,
}

/// One class's total over every change set and tool. Every class is present,
/// at zero if it never fired: a failing class missing from the table reads as
/// "not measured".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassTotal {
    pub class: Class,
    pub labels: usize,
}

/// One of cambium's own `--explain` reasons for the labels a tool did not
/// report, summed over the change sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasonRow {
    pub tool: String,
    pub reason: String,
    pub labels: usize,
}

/// How far a tool's depths agree with cambium's, over the change sets that
/// reported any.
#[derive(Clone, Debug, PartialEq)]
pub struct DepthAgreement {
    pub mean: f64,
    pub change_sets: usize,
}

/// The two numbers that belong to a tool rather than to a class.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolNote {
    pub tool: String,
    /// `None` when the tool reported no distances.
    pub depth: Option<DepthAgreement>,
    pub dropped_file_targets: usize,
}

/// The comparison section, computed.
#[derive(Clone, Debug, PartialEq)]
pub struct CompareSection {
    pub change_sets: usize,
    pub passed: usize,
    pub failed: usize,
    pub globals: usize,
    pub min_recall: Option<f64>,
    pub soundness: Vec<SoundnessRow>,
    pub timings: Vec<TimingRow>,
    /// Incremental change sets the engine re-indexed whole.
    pub cold_rebuild_change_sets: usize,
    /// Empty when no compared change set was global.
    pub globals_table: Vec<GlobalRow>,
    pub classes: Vec<ClassTotal>,
    /// Per tool in name order; within a tool, most labels first.
    pub reasons: Vec<ReasonRow>,
    /// One per tool that ran anywhere, in name order.
    pub tool_notes: Vec<ToolNote>,
    pub rdeps: RdepsOracleSummary,
    pub load: LoadOracleSummary,
}

impl CompareSection {
    pub fn compute(r: &SubjectResults) -> Self {
        let compares = &r.compares;
        let change_sets = r.change_sets_total();
        let failed = r.failures();
        Self {
            change_sets,
            passed: change_sets - failed,
            failed,
            globals: compares.iter().map(|c| c.summary.globals).sum(),
            min_recall: r.min_recall(),
            soundness: compares.iter().flat_map(soundness_rows).collect(),
            timings: compares.iter().flat_map(timing_rows).collect(),
            cold_rebuild_change_sets: compares
                .iter()
                .map(|c| c.summary.cold_rebuild_change_sets)
                .sum(),
            globals_table: compares.iter().flat_map(global_rows).collect(),
            classes: class_totals(compares),
            reasons: reason_rows(compares),
            tool_notes: tool_notes(compares),
            rdeps: rdeps_summary(compares),
            load: load_summary(compares),
        }
    }

    /// Whether any tool reported a reason for a cambium-only label.
    pub fn has_reasons(&self) -> bool {
        !self.reasons.is_empty()
    }
}

fn soundness_rows(c: &ChangesResult) -> Vec<SoundnessRow> {
    let bazel = compare_bazel(c);
    let names = tool_names(c);
    let row = |tool: Option<String>| SoundnessRow {
        bazel: bazel.clone(),
        precision: tool
            .as_deref()
            .and_then(|n| c.summary.mean_precision.get(n).copied()),
        tool,
        change_sets: c.summary.change_sets,
        passed: c.summary.passed,
        min_recall: c.summary.min_recall,
    };
    if names.is_empty() {
        return vec![row(None)];
    }
    names.into_iter().map(|n| row(Some(n))).collect()
}

/// How many change sets a tool's distribution covers. The explicit count when
/// the run recorded one; otherwise the sample count of its cold distribution,
/// for results written before the field existed.
fn tool_ran(
    counts: &BTreeMap<String, usize>,
    cold: &BTreeMap<String, WallStats>,
    name: &str,
) -> usize {
    counts
        .get(name)
        .copied()
        .or_else(|| cold.get(name).map(|w| w.samples))
        .unwrap_or(0)
}

fn timing_rows(c: &ChangesResult) -> Vec<TimingRow> {
    let bazel = compare_bazel(c);
    let s = &c.summary;
    let mut out = Vec::new();
    // Per tool, because Tango is often capped: a p50 over 10 change sets and
    // one over 50 are different claims and the table has to say which is
    // which.
    for name in tool_names(c) {
        for (scope, counts, cold, warm) in [
            (
                Scope::All,
                &s.all_tool_change_sets,
                &s.all_tool_cold,
                &s.all_tool_warm,
            ),
            (
                Scope::Incremental,
                &s.tool_change_sets,
                &s.tool_cold,
                &s.tool_warm,
            ),
        ] {
            out.push(TimingRow {
                bazel: bazel.clone(),
                tool: Some(name.clone()),
                scope,
                change_sets: tool_ran(counts, cold, &name),
                cold: cold.get(&name).cloned(),
                warm: warm.get(&name).cloned(),
                cold_engine: None,
                warm_engine: None,
            });
        }
    }
    let recorded = |w: &WallStats| (w.samples > 0).then(|| w.clone());
    for (scope, cold, warm, cold_engine, warm_engine) in [
        (
            Scope::All,
            &s.all_cambium_cold,
            &s.all_cambium_warm,
            &s.all_cambium_cold_engine,
            &s.all_cambium_warm_engine,
        ),
        (
            Scope::Incremental,
            &s.cambium_cold,
            &s.cambium_warm,
            &s.cambium_cold_engine,
            &s.cambium_warm_engine,
        ),
    ] {
        out.push(TimingRow {
            bazel: bazel.clone(),
            tool: None,
            scope,
            change_sets: cold.samples,
            cold: Some(cold.clone()),
            warm: Some(warm.clone()),
            cold_engine: recorded(cold_engine),
            warm_engine: recorded(warm_engine),
        });
    }
    out
}

fn global_rows(c: &ChangesResult) -> Vec<GlobalRow> {
    let g = &c.summary.global_stats;
    if g.is_empty() {
        return Vec::new();
    }
    let bazel = compare_bazel(c);
    let row = |tool: Option<&String>| GlobalRow {
        bazel: bazel.clone(),
        change_sets: g.change_sets,
        cambium_labels: g.mean_cambium_labels.round() as u64,
        tool: tool.cloned(),
        tool_labels: tool.map(|n| g.mean_tool_labels[n].round() as u64),
        tool_cold_p50_ms: tool.and_then(|n| g.tool_cold.get(n).map(|w| w.p50_ms)),
        cambium_cold_p50_ms: g.cambium_cold.p50_ms,
        cambium_warm_p50_ms: g.cambium_warm.p50_ms,
    };
    if g.mean_tool_labels.is_empty() {
        return vec![row(None)];
    }
    g.mean_tool_labels.keys().map(|n| row(Some(n))).collect()
}

fn class_totals(compares: &[ChangesResult]) -> Vec<ClassTotal> {
    let mut totals: BTreeMap<Class, usize> = BTreeMap::new();
    for c in compares {
        for (k, v) in &c.summary.by_class {
            *totals.entry(*k).or_default() += v;
        }
    }
    Class::ALL
        .iter()
        .map(|class| ClassTotal {
            class: *class,
            labels: totals.get(class).copied().unwrap_or(0),
        })
        .collect()
}

fn reason_rows(compares: &[ChangesResult]) -> Vec<ReasonRow> {
    let mut reasons: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for c in compares {
        for change_set in &c.change_sets {
            for pr in &change_set.tools {
                let e = reasons.entry(pr.name.clone()).or_default();
                for (k, v) in &pr.cambium_only_reasons {
                    *e.entry(k.clone()).or_default() += v;
                }
            }
        }
    }
    let mut out = Vec::new();
    for (tool, m) in reasons {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out.extend(v.into_iter().map(|(reason, labels)| ReasonRow {
            tool: tool.clone(),
            reason,
            labels,
        }));
    }
    out
}

fn tool_notes(compares: &[ChangesResult]) -> Vec<ToolNote> {
    let mut depth: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    let mut dropped: BTreeMap<String, usize> = BTreeMap::new();
    for c in compares {
        for change_set in &c.change_sets {
            for pr in &change_set.tools {
                *dropped.entry(pr.name.clone()).or_default() += pr.dropped_file_targets;
                if let Some(d) = pr.depth_agreement {
                    let slot = depth.entry(pr.name.clone()).or_default();
                    slot.0 += d;
                    slot.1 += 1;
                }
            }
        }
    }
    dropped
        .into_iter()
        .map(|(tool, dropped_file_targets)| ToolNote {
            depth: depth
                .get(&tool)
                .filter(|(_, n)| *n > 0)
                .map(|(sum, n)| DepthAgreement {
                    mean: sum / *n as f64,
                    change_sets: *n,
                }),
            tool,
            dropped_file_targets,
        })
        .collect()
}

fn rdeps_summary(compares: &[ChangesResult]) -> RdepsOracleSummary {
    let mut s = RdepsOracleSummary::default();
    for c in compares {
        for p in &c.change_sets {
            if let Some(o) = &p.oracle {
                s.add(o);
            }
        }
    }
    s
}

fn load_summary(compares: &[ChangesResult]) -> LoadOracleSummary {
    let mut s = LoadOracleSummary::default();
    for c in compares {
        for p in &c.change_sets {
            if let Some(o) = &p.load_oracle {
                s.add(o);
            }
        }
    }
    s
}
