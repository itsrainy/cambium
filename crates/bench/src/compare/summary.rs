//! Rolling the per-change-set results up, and scoring one of them.
//!
//! Pure arithmetic over [`super::model`], so every number the report prints
//! can be tested without a Bazel, a tool or a checkout -- which is the point:
//! a distribution taken over the wrong population is not a bug that shows up
//! as a crash.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::{classify, Class, Facts, Side};
use crate::util::percentile;

use super::model::{ChangeSetResult, ChangesSummary, Disagreement, GlobalStats, WallStats};

/// `|tool ∩ cambium| / |cambium|`: how much of what cambium reported the
/// tool also reported.
///
/// An empty cambium answer is defined as precision 1.0 — the two tools agreed
/// that nothing is affected, which is the common case on a source-only commit,
/// and calling it 0.0 would drag every mean down for the least interesting
/// change sets in the run.
pub fn precision(tool: &BTreeSet<String>, cambium: &BTreeSet<String>) -> f64 {
    if cambium.is_empty() {
        return 1.0;
    }
    tool.intersection(cambium).count() as f64 / cambium.len() as f64
}

/// p50/p90 over a set of wall times.
pub fn wall_stats(mut ms: Vec<u64>) -> WallStats {
    ms.sort_unstable();
    WallStats {
        samples: ms.len(),
        p50_ms: percentile(&ms, 0.5),
        p90_ms: percentile(&ms, 0.9),
    }
}

/// Rolls the per-change set results up. Pure, so the arithmetic is testable without
/// a repository.
///
/// Two wall-clock series come out of this, and the difference between them is
/// the point:
///
/// - the **incremental** series (`cambium_cold`, `tool_cold`, ...) covers
///   the change sets cambium answered with a walk. It is the series that belongs
///   next to precision and the class counts, because those are computed over
///   exactly those change sets.
/// - the **all-change sets** series (`all_*`) covers every change set compared, including
///   the ones where cambium fell back to `//...`. Nothing is excluded from it,
///   which is why it is the number the report quotes first.
///
/// Soundness is unchanged: a global change set enters no precision mean, no class
/// total and no recall, because there is nothing there to be right or wrong
/// about — `//...` contains everything by construction.
pub fn summarise(change_sets: &[ChangeSetResult]) -> ChangesSummary {
    let mut s = ChangesSummary {
        change_sets: change_sets.len(),
        min_recall: 1.0,
        ..Default::default()
    };
    for c in Class::ALL {
        s.by_class.insert(c, 0);
    }
    let mut precisions: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut all_cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut all_warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut g_cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut g_warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut g_labels: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut g_cambium: Vec<usize> = Vec::new();
    for p in change_sets {
        if p.pass {
            s.passed += 1;
        }
        if p.cambium.cold_rebuild {
            s.cold_rebuild_change_sets += 1;
        }
        // Every change set's walls, whatever cambium did with it.
        for pr in p.tools.iter().filter(|pr| pr.error.is_none()) {
            *s.all_tool_change_sets.entry(pr.name.clone()).or_default() += 1;
            all_cold
                .entry(pr.name.clone())
                .or_default()
                .push(pr.cold_wall_ms);
            if let Some(w) = pr.warm_wall_ms {
                all_warm.entry(pr.name.clone()).or_default().push(w);
            }
        }
        if p.global {
            s.globals += 1;
            g_cambium.push(p.cambium.answer_size());
            for pr in p.tools.iter().filter(|pr| pr.error.is_none()) {
                g_labels.entry(pr.name.clone()).or_default().push(pr.labels);
                g_cold
                    .entry(pr.name.clone())
                    .or_default()
                    .push(pr.cold_wall_ms);
                if let Some(w) = pr.warm_wall_ms {
                    g_warm.entry(pr.name.clone()).or_default().push(w);
                }
            }
            continue;
        }
        s.min_recall = s.min_recall.min(p.recall);
        if let Some(o) = &p.load_oracle {
            s.load_oracle.add(o);
            if !o.ok() {
                *s.by_class
                    .entry(Class::CambiumMissingBzlClosure)
                    .or_default() += o.missing_packages.len();
            }
        }
        for pr in &p.tools {
            if pr.error.is_some() {
                continue;
            }
            for (class, n) in &pr.counts {
                *s.by_class.entry(*class).or_default() += n;
            }
            *s.tool_change_sets.entry(pr.name.clone()).or_default() += 1;
            precisions
                .entry(pr.name.clone())
                .or_default()
                .push(pr.precision);
            cold.entry(pr.name.clone())
                .or_default()
                .push(pr.cold_wall_ms);
            if let Some(w) = pr.warm_wall_ms {
                warm.entry(pr.name.clone()).or_default().push(w);
            }
        }
    }
    s.mean_precision = precisions
        .into_iter()
        .map(|(k, v)| {
            let mean = if v.is_empty() {
                1.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            };
            (k, mean)
        })
        .collect();
    s.tool_cold = cold.into_iter().map(|(k, v)| (k, wall_stats(v))).collect();
    s.tool_warm = warm.into_iter().map(|(k, v)| (k, wall_stats(v))).collect();
    let timed = || change_sets.iter().filter(|p| !p.global);
    s.cambium_cold = wall_stats(timed().map(|p| p.cambium.cold_wall_ms).collect());
    s.cambium_warm = wall_stats(timed().map(|p| p.cambium.warm_wall_ms).collect());
    s.all_tool_cold = all_cold
        .into_iter()
        .map(|(k, v)| (k, wall_stats(v)))
        .collect();
    s.all_tool_warm = all_warm
        .into_iter()
        .map(|(k, v)| (k, wall_stats(v)))
        .collect();
    s.all_cambium_cold = wall_stats(change_sets.iter().map(|p| p.cambium.cold_wall_ms).collect());
    s.all_cambium_warm = wall_stats(change_sets.iter().map(|p| p.cambium.warm_wall_ms).collect());
    // 0 is "not recorded", not a zero-millisecond run.
    let engine = |it: &mut dyn Iterator<Item = u64>| wall_stats(it.filter(|ms| *ms > 0).collect());
    s.cambium_cold_engine = engine(&mut timed().map(|p| p.cambium.cold_engine_ms));
    s.cambium_warm_engine = engine(&mut timed().map(|p| p.cambium.warm_engine_ms));
    s.all_cambium_cold_engine = engine(&mut change_sets.iter().map(|p| p.cambium.cold_engine_ms));
    s.all_cambium_warm_engine = engine(&mut change_sets.iter().map(|p| p.cambium.warm_engine_ms));
    s.global_stats = GlobalStats {
        change_sets: s.globals,
        mean_cambium_labels: mean(g_cambium.iter().map(|n| *n as f64)),
        mean_tool_labels: g_labels
            .iter()
            .map(|(k, v)| (k.clone(), mean(v.iter().map(|n| *n as f64))))
            .collect(),
        cambium_cold: wall_stats(
            change_sets
                .iter()
                .filter(|p| p.global)
                .map(|p| p.cambium.cold_wall_ms)
                .collect(),
        ),
        cambium_warm: wall_stats(
            change_sets
                .iter()
                .filter(|p| p.global)
                .map(|p| p.cambium.warm_wall_ms)
                .collect(),
        ),
        tool_cold: g_cold
            .into_iter()
            .map(|(k, v)| (k, wall_stats(v)))
            .collect(),
        tool_warm: g_warm
            .into_iter()
            .map(|(k, v)| (k, wall_stats(v)))
            .collect(),
    };
    s.pass = s.passed == s.change_sets && s.min_recall >= 1.0;
    s
}

/// The arithmetic mean, and 0.0 over nothing.
fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

/// Classifies the symmetric difference of the two label sets.
///
/// Both directions in one pass, so a caller cannot accidentally classify one
/// side with the other side's rules.
pub fn classify_difference(
    cambium: &BTreeSet<String>,
    tool: &BTreeSet<String>,
    facts: &dyn Facts,
    examples: usize,
) -> (BTreeMap<Class, usize>, Vec<Disagreement>) {
    let (counts, found) = classify_all(cambium, tool, facts);
    (counts, examples_of(found, examples))
}

/// Every classification, uncapped.
///
/// Split out of [`classify_difference`] because the cap belongs to the result
/// JSON and not to the classification: the driver writes the whole list beside
/// it ([`crate::labels`]) so that the change set can be studied offline, and
/// classifying twice to get both would double the Bazel queries the classifier
/// makes.
pub fn classify_all(
    cambium: &BTreeSet<String>,
    tool: &BTreeSet<String>,
    facts: &dyn Facts,
) -> (BTreeMap<Class, usize>, Vec<Disagreement>) {
    let mut counts: BTreeMap<Class, usize> = BTreeMap::new();
    let mut found: Vec<Disagreement> = Vec::new();
    let mut push = |label: &str, side: Side| {
        let class = classify(label, side, facts);
        *counts.entry(class).or_default() += 1;
        found.push(Disagreement {
            label: label.to_string(),
            side,
            class,
        });
    };
    for label in cambium.difference(tool) {
        push(label, Side::Cambium);
    }
    for label in tool.difference(cambium) {
        push(label, Side::Tool);
    }
    // Failures first: the examples that survive the cap are the ones a human
    // has to look at.
    found.sort_by_key(|d| (!d.class.is_failure(), d.class, d.label.clone()));
    (counts, found)
}

/// The first `examples` of an already-ordered classification.
pub fn examples_of(mut found: Vec<Disagreement>, examples: usize) -> Vec<Disagreement> {
    found.truncate(examples);
    found
}

// ----------------------------------------------------------------- the driver
