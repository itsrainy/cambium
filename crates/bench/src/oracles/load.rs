//! The `.bzl` load-closure oracle.
//!
//! cambium works out which packages a changed `.bzl` reaches by scanning
//! `load()` statements at index time (`cambium_index::loads`). That is a
//! reimplementation of something Bazel already knows, so on every change set whose
//! change set contains a `.bzl` the suite asks Bazel the same question —
//! `rbuildfiles(<paths>)`, the call the engine used to make — and requires
//! cambium's dirty set to be a **superset** of the packages Bazel names.
//!
//! One-sided on purpose, for the same reason the `rdeps` oracle is: extra
//! packages on cambium's side are over-approximation, which costs a query;
//! a package Bazel names that cambium did not dirty is a missed rule
//! implementation change, which is the bug class the design does not tolerate.
//!
//! Exit 3 (Sky Query finished with errors under `--keep_going`) is accepted as
//! a partial answer and recorded, exactly as the `rdeps` oracle accepts a
//! partial batch: a shorter Bazel answer can only weaken the containment
//! check, never invert it. That is also the failure mode this whole change
//! exists to remove — on the real subjects, `rbuildfiles` exited 3 on 5 of 150
//! `bazel/bazel` commits, and the engine had to answer `//...` each time.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use serde::{Deserialize, Serialize};

/// What Bazel said about the `.bzl` files loaded in one change set.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadOracleResult {
    /// The changed `.bzl` paths the query asked about.
    pub checked: Vec<String>,
    /// Sky Query finished with errors, so Bazel's answer is a lower bound.
    #[serde(default)]
    pub partial: bool,
    /// Main-repository packages `rbuildfiles` named.
    pub bazel_packages: BTreeSet<String>,
    /// Labels naming a loader outside any main-repo package — a module
    /// extension's `.bzl`, a `WORKSPACE` load. cambium answers those with a
    /// global invalidation, and a global change set never reaches this oracle, so
    /// they are recorded rather than compared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_level: Vec<String>,
    /// `bazel_packages - cambium's dirty set`: unsoundness. Empty means pass.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub missing_packages: BTreeSet<String>,
}

/// The class a non-empty [`LoadOracleResult::missing_packages`] is counted
/// under, and reported in the class table.
pub const MISSING_CLASS: &str = "cambium_missing_bzl_closure";

impl LoadOracleResult {
    pub fn ok(&self) -> bool {
        self.missing_packages.is_empty()
    }
}

/// The `.bzl` paths in a change set, in the form `rbuildfiles` wants
/// (workspace-relative).
pub fn changed_bzl<'a>(changed: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut v: Vec<String> = changed
        .into_iter()
        .filter(|p| cambium_classify::is_bzl_file(p))
        .cloned()
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Runs the oracle for one change set. `None` when the change set has no `.bzl` in
/// it, which is most change sets.
///
/// `bazel` must be the handle for the **head** workspace: `rbuildfiles` is a
/// question about the tree the changed file now lives in.
pub fn run<'a>(
    bazel: &Bazel,
    changed: impl IntoIterator<Item = &'a String>,
    cambium_dirty: &BTreeSet<String>,
) -> Result<Option<LoadOracleResult>> {
    let paths = changed_bzl(changed);
    if paths.is_empty() {
        return Ok(None);
    }
    let (r, partial) = bazel
        .rbuildfiles(&paths)
        .with_context(|| format!("rbuildfiles oracle over {} .bzl file(s)", paths.len()))?;
    let bazel_packages: BTreeSet<String> = r.packages.into_iter().collect();
    let missing_packages = bazel_packages.difference(cambium_dirty).cloned().collect();
    Ok(Some(LoadOracleResult {
        checked: paths,
        partial,
        bazel_packages,
        workspace_level: r.workspace_level,
        missing_packages,
    }))
}

/// Roll-up over a run's change sets, for the summary document and the report table.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadOracleSummary {
    /// ChangeSets whose change set contained a `.bzl`.
    pub change_sets: usize,
    /// `.bzl` files asked about, over those change sets.
    pub checked: usize,
    /// ChangeSets where Bazel's answer was partial (exit 3).
    pub partial: usize,
    /// Packages Bazel named that cambium did not dirty. Must be zero.
    pub missing_packages: usize,
}

impl LoadOracleSummary {
    pub fn add(&mut self, r: &LoadOracleResult) {
        self.change_sets += 1;
        self.checked += r.checked.len();
        self.partial += usize::from(r.partial);
        self.missing_packages += r.missing_packages.len();
    }

    pub fn is_empty(&self) -> bool {
        self.change_sets == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_bzl_paths_are_asked_about() {
        let changed = vec![
            "a/BUILD".to_string(),
            "bazel/defs.bzl".into(),
            "src/x.cc".into(),
            "bazel/defs.bzl".into(),
        ];
        assert_eq!(changed_bzl(&changed), vec!["bazel/defs.bzl"]);
        assert!(changed_bzl(&["a/BUILD".to_string()]).is_empty());
    }

    #[test]
    fn a_superset_passes_and_a_gap_fails() {
        let mut r = LoadOracleResult {
            checked: vec!["bazel/defs.bzl".into()],
            bazel_packages: set(&["app", "lib"]),
            ..Default::default()
        };
        r.missing_packages = r
            .bazel_packages
            .difference(&set(&["app", "lib", "extra"]))
            .cloned()
            .collect();
        assert!(r.ok(), "over-approximation is not a failure");

        r.missing_packages = r
            .bazel_packages
            .difference(&set(&["app"]))
            .cloned()
            .collect();
        assert!(!r.ok());
        assert_eq!(r.missing_packages, set(&["lib"]));
    }

    #[test]
    fn the_summary_counts_change_sets_files_and_gaps() {
        let mut s = LoadOracleSummary::default();
        assert!(s.is_empty());
        s.add(&LoadOracleResult {
            checked: vec!["a.bzl".into(), "b.bzl".into()],
            partial: true,
            ..Default::default()
        });
        s.add(&LoadOracleResult {
            checked: vec!["c.bzl".into()],
            missing_packages: set(&["lib"]),
            ..Default::default()
        });
        assert_eq!(s.change_sets, 2);
        assert_eq!(s.checked, 3);
        assert_eq!(s.partial, 1);
        assert_eq!(s.missing_packages, 1);
    }
}
